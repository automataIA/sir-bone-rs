use std::{
    io,
    io::Write as _,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::Result;
use ratatui::{
    backend::CrosstermBackend,
    crossterm::{
        event::{
            self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyModifiers,
            MouseButton, MouseEvent, MouseEventKind,
        },
        execute,
        terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    },
    style::Color,
    text::{Line, Span},
    Terminal,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{
    agent::{AgentContext, ConfirmBridge, LlmClient, Prompt, PromptReply},
    tools::ToolRegistry,
    types::{AgentEvent, Message},
};

use super::{
    app::{
        messages_to_replay, AgentStatus, App, CompletionMode, Focus, PickerKind, Popup,
        PopupContent, ReplayEvent, SettingsRow, SETTINGS_ROWS,
    },
    theme::{color_to_hex, styled, PALETTES},
    widgets::{user_box, KB_PANEL_W, TIMELINE_W_MAX, TIMELINE_W_MIN},
};

// ── guards ────────────────────────────────────────────────────────────────────

struct RawModeGuard;
impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}

struct AlternateScreenGuard;
impl Drop for AlternateScreenGuard {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

struct MouseCaptureGuard;
impl Drop for MouseCaptureGuard {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), DisableMouseCapture);
    }
}

/// Restore the terminal *before* the default panic handler prints, so the panic
/// message lands on a clean screen instead of being swallowed by the alternate
/// screen teardown. The RAII guards already prevent corruption on unwind; this
/// hook is purely about making the crash message visible.
fn install_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
        prev(info);
    }));
}

/// True when running inside WSL (env var set by wslservice, or the kernel
/// banner mentions Microsoft).
fn is_wsl() -> bool {
    static WSL: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *WSL.get_or_init(|| {
        std::env::var_os("WSL_DISTRO_NAME").is_some()
            || std::fs::read_to_string("/proc/version")
                .is_ok_and(|v| v.to_lowercase().contains("microsoft"))
    })
}

/// Completion chime: terminal BEL always; on WSL also the Windows notification
/// sound via the PowerShell interop (the terminal bell is often inaudible
/// there). Fire-and-forget — failures are ignored.
fn notify_bell() {
    let _ = write!(io::stdout(), "\x07");
    let _ = io::stdout().flush();
    if is_wsl() {
        if let Ok(mut child) = tokio::process::Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-c",
                "[System.Media.SystemSounds]::Exclamation.Play()",
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            // Reap in the background so the child doesn't linger as a zombie.
            tokio::spawn(async move {
                let _ = child.wait().await;
            });
        }
    }
}

fn set_cursor_color(input_focused: bool, accent: Color) {
    let esc = if input_focused {
        format!("\x1b]12;{}\x1b\\", color_to_hex(accent))
    } else {
        "\x1b]112\x1b\\".to_string()
    };
    let _ = write!(io::stdout(), "{esc}");
    let _ = io::stdout().flush();
}

async fn session_append(path: &PathBuf, msg: &Message) {
    use tokio::io::AsyncWriteExt;
    #[derive(serde::Serialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    enum E<'a> {
        Message(&'a Message),
    }
    if let Some(p) = path.parent() {
        if let Err(e) = tokio::fs::create_dir_all(p).await {
            tracing::warn!("session dir: {e}");
            return;
        }
    }
    match tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await
    {
        Err(e) => tracing::warn!("session open: {e}"),
        Ok(mut f) => match serde_json::to_string(&E::Message(msg)) {
            Err(e) => tracing::warn!("session serialize: {e}"),
            Ok(s) => {
                if let Err(e) = f.write_all(format!("{s}\n").as_bytes()).await {
                    tracing::warn!("session write: {e}");
                }
            }
        },
    }
}

/// Drain pending agent events into the app. A `Compacted` event is also
/// persisted as a session `Compaction` checkpoint, and its transcript length
/// recorded so the post-run append knows where the un-persisted tail starts.
/// Drain queued agent events into the app. Returns `true` if any were applied,
/// so the caller knows the screen changed and needs a repaint.
async fn drain_events(
    rx: &mut mpsc::Receiver<AgentEvent>,
    app: &mut App,
    session_path: &std::path::Path,
    compaction_base: &mut Option<usize>,
    usage: &mut (u64, u64, u32),
) -> bool {
    let mut applied = false;
    while let Ok(ev) = rx.try_recv() {
        match &ev {
            AgentEvent::Compacted { messages } => {
                *compaction_base = Some(messages.len());
                let entry = crate::session::SessionEntry::Compaction {
                    messages: messages.clone(),
                };
                if let Err(e) = crate::session::append(session_path, &entry).await {
                    tracing::warn!("session compaction append: {e}");
                }
            }
            AgentEvent::WorkspaceSnapshot { id, label } => {
                let entry = crate::session::SessionEntry::WorkspaceSnapshot {
                    id: id.clone(),
                    label: label.clone(),
                };
                if let Err(e) = crate::session::append(session_path, &entry).await {
                    tracing::warn!("session snapshot append: {e}");
                }
            }
            AgentEvent::ContextUsage {
                used_tokens,
                cached_tokens,
                ..
            } if *used_tokens > 0 => {
                usage.0 += *used_tokens as u64;
                usage.1 += *cached_tokens as u64;
                usage.2 = usage.2.max(*used_tokens);
            }
            _ => {}
        }
        app.push(ev);
        applied = true;
    }
    applied
}

/// Owned-argument wrapper so the poll can be `tokio::spawn`ed.
async fn refresh_glm_quota(model: String) {
    crate::quota::refresh_glm(&model).await;
}

// ── public entry point ────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
pub async fn run_tui(
    model: String,
    provider: String,
    client: Arc<dyn LlmClient>,
    registry: ToolRegistry,
    system_prompt: String,
    messages: Vec<Message>,
    session_path: PathBuf,
    mcp_task: tokio::task::JoinHandle<crate::mcp::McpLoad>,
    plan_requested: bool,
) -> Result<()> {
    install_panic_hook();
    enable_raw_mode()?;
    let _raw = RawModeGuard;
    run_inner(
        model,
        provider,
        client,
        registry,
        system_prompt,
        messages,
        session_path,
        mcp_task,
        plan_requested,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn run_inner(
    model: String,
    provider: String,
    client: Arc<dyn LlmClient>,
    registry: ToolRegistry,
    system_prompt: String,
    mut messages: Vec<Message>,
    mut session_path: PathBuf,
    mcp_task: tokio::task::JoinHandle<crate::mcp::McpLoad>,
    plan_requested: bool,
) -> Result<()> {
    execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let _screen = AlternateScreenGuard;
    let _mouse = MouseCaptureGuard;
    run_loop(
        &mut terminal,
        model,
        provider,
        client,
        registry,
        system_prompt,
        &mut messages,
        &mut session_path,
        mcp_task,
        plan_requested,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    model: String,
    provider: String,
    client: Arc<dyn LlmClient>,
    mut registry: ToolRegistry,
    system_prompt: String,
    messages: &mut Vec<Message>,
    session_path: &mut PathBuf,
    mcp_task: tokio::task::JoinHandle<crate::mcp::McpLoad>,
    plan_requested: bool,
) -> Result<()> {
    let mut app = App::new(model.clone(), provider);
    app.attach_dir = crate::attachments::dir_for(session_path);
    if plan_requested {
        app.plan = true;
    }
    app.thinking_budget = client.thinking_budget();
    let (ev_tx, mut ev_rx) = mpsc::channel::<AgentEvent>(64);
    let mut agent_task: Option<(tokio::task::JoinHandle<Result<Vec<Message>>>, usize)> = None;
    let mut cancel: Option<CancellationToken> = None;
    // UI side of the user-prompt bridge: the agent task sends a `Prompt` on
    // `ask`; we answer with a `PromptReply` after the multi-choice dialog.
    let mut confirm_ui: Option<(mpsc::Receiver<Prompt>, mpsc::Sender<PromptReply>)> = None;
    // While busy the first ^C cancels the agent; a second within 2s quits.
    let mut last_ctrl_c: Option<Instant> = None;
    // Job-log progress scan is file IO — throttle it to ~1/s.
    let mut last_progress_poll = Instant::now();
    // Live GLM quota: refresh in the background (no-op off z.ai/GLM) and let the
    // 1/s reload below pick the result up. Same 5-minute cadence as the ZCode UI.
    let mut last_quota_poll = Instant::now();
    tokio::spawn(refresh_glm_quota(model.clone()));
    // Length of the transcript snapshot at the last mid-run compaction, if any.
    // The post-run session append starts here instead of the pre-run length,
    // since the Compaction entry already persisted everything before it.
    let mut compaction_base: Option<usize> = None;
    let mut run_usage: (u64, u64, u32) = (0, 0, 0);

    // Repaint prior history when resuming a session at startup.
    if !messages.is_empty() {
        app.replay_events = messages_to_replay(messages);
        app.reset_durations_for_replay();
        app.replay();
    }

    terminal.draw(|f| app.render(f))?;

    // The first frame is up — now join the background MCP load (npx servers cost
    // seconds) and register their tools before any turn can start. Overlaps with
    // the user reading the freshly drawn screen, so the launch feels instant.
    // `_mcp_handles` lives to the end of run_loop: dropping a handle kills its
    // child process, so they must outlive every turn.
    let _mcp_handles = match mcp_task.await {
        Ok((tools, handles)) => {
            for tool in tools {
                registry.register_dyn(tool);
            }
            handles
        }
        Err(e) => {
            app.info_line(format!("MCP load failed: {e}"));
            Vec::new()
        }
    };

    loop {
        // Repaint only when something changed (`dirty`). When idle the loop just
        // wakes to poll for input and goes straight back to sleep — no redraw, no
        // CPU. Animated states (busy spinner, About boar) keep dirty set.
        let mut dirty = false;

        // Boar animation now lives only in the About screen — advance it there.
        if app.about_mode {
            let term_w = ratatui::crossterm::terminal::size()
                .map(|(w, _)| w)
                .unwrap_or(200);
            app.boar
                .advance_about(term_w.saturating_sub(KB_PANEL_W + 2));
            dirty = true;
        }

        // Poll briefly while animating for smooth frames; sleep longer when idle
        // (a keypress still wakes `poll` immediately regardless of the timeout).
        let poll_ms = if app.busy || app.about_mode || app.revealing() {
            16
        } else {
            250
        };
        if event::poll(Duration::from_millis(poll_ms))? {
            dirty = true;
            match event::read()? {
                Event::Resize(_, _) => app.replay(), // re-wrap chat to the new width
                Event::Mouse(mouse) => handle_mouse(&mut app, mouse),
                Event::Key(key) => match handle_key_event(&mut app, key) {
                    KeyOutcome::Continue => {}
                    KeyOutcome::Redraw => {
                        terminal.draw(|f| app.render(f))?;
                        continue;
                    }
                    KeyOutcome::Quit => {
                        if let Some(ct) = &cancel {
                            ct.cancel();
                        }
                        break;
                    }
                    KeyOutcome::CtrlC => {
                        let again =
                            last_ctrl_c.is_some_and(|t| t.elapsed() < Duration::from_secs(2));
                        if let Some(ct) = &cancel {
                            ct.cancel();
                        }
                        if !app.busy || again {
                            break;
                        }
                        last_ctrl_c = Some(Instant::now());
                    }
                    KeyOutcome::Cancel => {
                        if let Some(ct) = &cancel {
                            ct.cancel();
                        }
                    }
                    KeyOutcome::SetThinking(next) => {
                        client.set_thinking_budget(next);
                        app.thinking_budget = next;
                    }
                    KeyOutcome::SendReply(reply) => {
                        if let Some((_, tx)) = &confirm_ui {
                            tx.try_send(reply).ok();
                        }
                        app.prompt = None;
                        terminal.draw(|f| app.render(f))?;
                        continue;
                    }
                    KeyOutcome::Submit(text) => {
                        // Fresh run: any recorded base belongs to a previous
                        // (already persisted) manual compaction.
                        compaction_base = None;
                        dispatch_message(
                            text,
                            &mut app,
                            messages,
                            session_path,
                            &model,
                            &client,
                            &registry,
                            &system_prompt,
                            &ev_tx,
                            &mut agent_task,
                            &mut cancel,
                            &mut confirm_ui,
                        )
                        .await;
                    }
                    KeyOutcome::Slash(cmd) => {
                        let (name, query) = cmd
                            .split_once(' ')
                            .map_or((cmd.as_str(), ""), |(name, query)| (name, query.trim()));
                        if name == "historia" {
                            let limit = if query.is_empty() { 6 } else { 20 };
                            match registry
                                .execute(
                                    "historia",
                                    serde_json::json!({
                                        "query": query,
                                        "max_sessions": limit,
                                        "focus": "all"
                                    }),
                                )
                                .await
                            {
                                Ok(history) => {
                                    compaction_base = None;
                                    dispatch_message(
                                        crate::tools::historia::continuation_prompt(
                                            query, &history,
                                        ),
                                        &mut app,
                                        messages,
                                        session_path,
                                        &model,
                                        &client,
                                        &registry,
                                        &system_prompt,
                                        &ev_tx,
                                        &mut agent_task,
                                        &mut cancel,
                                        &mut confirm_ui,
                                    )
                                    .await;
                                }
                                Err(e) => app.info_line(format!("historia failed: {e}")),
                            }
                        } else if run_slash_command(
                            cmd,
                            &mut app,
                            messages,
                            session_path,
                            &model,
                            &client,
                            &registry,
                            &system_prompt,
                            &ev_tx,
                        )
                        .await
                        {
                            break; // `/quit`
                        }
                    }
                },
                _ => {}
            } // match event
        } // event::poll

        dirty |= drain_events(
            &mut ev_rx,
            &mut app,
            session_path,
            &mut compaction_base,
            &mut run_usage,
        )
        .await;

        // Background jobs: refresh the info-bar line and surface completions
        // exactly once (rail block + bell/Windows chime).
        if last_progress_poll.elapsed() >= Duration::from_secs(1) {
            last_progress_poll = Instant::now();
            registry.jobs.poll_progress();
            // Pick up background quota refreshes, redrawing only on a change.
            let quota = crate::quota::current();
            if quota.map(|w| (w.end, w.used_pct)) != app.quota_window.map(|w| (w.end, w.used_pct)) {
                app.quota_window = quota;
                dirty = true;
            }
            if last_quota_poll.elapsed() >= Duration::from_secs(300) {
                last_quota_poll = Instant::now();
                tokio::spawn(refresh_glm_quota(model.clone()));
            }
            if !app.jobs_running.is_empty() {
                dirty = true;
            } // live elapsed/gauge
        }
        app.jobs_running = registry.jobs.running();
        for (id, command, exit, dur) in registry.jobs.take_finished() {
            app.push(AgentEvent::JobDone {
                id,
                command,
                exit,
                secs: dur.as_secs(),
            });
            notify_bell();
            dirty = true;
        }

        // Surface a pending user prompt (permission gate or question) as the
        // multi-choice dialog.
        if let Some((ask_rx, _)) = &mut confirm_ui {
            if let Ok(prompt) = ask_rx.try_recv() {
                app.prompt = Some(super::widgets::PromptUi::new(prompt));
                dirty = true;
            }
        }

        if app.busy {
            app.spinner_tick = app.spinner_tick.wrapping_add(1);
            dirty = true;
        }
        dirty |= app.tick_reveal();
        // Replay any boundaries parked mid-reveal (tool boxes drop in once the
        // typewriter reaches them) — see App::deferred / drain_deferred.
        dirty |= app.drain_deferred();

        if agent_task.as_ref().is_some_and(|(t, _)| t.is_finished()) {
            // Pull any last chunks, then release the jitter-buffer reserve and let
            // the typewriter drain the tail at its constant rate before finalizing.
            drain_events(
                &mut ev_rx,
                &mut app,
                session_path,
                &mut compaction_base,
                &mut run_usage,
            )
            .await;
            app.reveal_drain = true;
            dirty = true;
            // Keep ticking while the tail is still revealing OR boundaries remain
            // parked, so drain_deferred (above) can flush the whole queue first.
            if app.revealing() || !app.deferred.is_empty() {
                terminal.draw(|f| app.render(f))?;
                continue;
            }
            finalize_run(
                &mut app,
                &mut agent_task,
                &mut cancel,
                &mut confirm_ui,
                &mut compaction_base,
                &mut run_usage,
                messages,
                session_path,
                &model,
                &client,
                &registry,
                &system_prompt,
                &ev_tx,
            )
            .await;
        }

        if dirty {
            terminal.draw(|f| app.render(f))?;
            if !app.about_mode {
                set_cursor_color(app.focus == Focus::Input, app.palette.accent);
            }
        }
    }

    Ok(())
}

/// Run a `/command` (payload is the text after the leading slash). Returns `true`
/// only for `/quit`, signalling the event loop to break. Extracted from the
/// `KeyOutcome::Slash` arm so the loop's dispatch stays thin.
#[allow(clippy::too_many_arguments)]
async fn run_slash_command(
    cmd: String,
    app: &mut App,
    messages: &mut Vec<Message>,
    session_path: &mut PathBuf,
    model: &str,
    client: &Arc<dyn LlmClient>,
    registry: &ToolRegistry,
    system_prompt: &str,
    ev_tx: &mpsc::Sender<AgentEvent>,
) -> bool {
    let (name, arg) = cmd
        .split_once(' ')
        .map_or((cmd.as_str(), ""), |(n, a)| (n, a.trim()));
    match name {
        "quit" => return true,
        "clear" => {
            // New conversation: drop history, start a fresh session file (the old
            // one stays on disk).
            messages.clear();
            *session_path = crate::session::new_session_path();
            app.attach_dir = crate::attachments::dir_for(session_path);
            app.reset();
        }
        "resume" => {
            resume_command(arg, app, messages, session_path).await;
            app.attach_dir = crate::attachments::dir_for(session_path);
        }
        "model" => model_command(arg, app, client).await,
        "tokens" => tokens_command(app, client, system_prompt, messages, registry).await,
        "jobs" => {
            let p = app.palette;
            app.lines.push(Line::default());
            for l in registry.jobs.report(None, 5).lines() {
                let style = if l.starts_with("job #") {
                    styled(p.accent, false)
                } else {
                    styled(p.muted, false)
                };
                app.lines
                    .push(Line::from(Span::styled(format!("  {l}"), style)));
            }
        }
        "compact" => {
            compact_command(app, model, client, registry, system_prompt, messages, ev_tx).await
        }
        "rollback" => rollback_command(arg, app).await,
        "snapshots" => snapshots_command(app).await,
        "plan" => {
            app.plan = !app.plan;
            let s = if app.plan {
                "on — tasks use a compact persistent contract"
            } else {
                registry.notes.stop_plan();
                "off"
            };
            app.info_line(format!("plan mode {s}"));
        }
        "oracle" => {
            app.oracle = !app.oracle;
            app.save_settings();
            app.info_line(format!(
                "oracle gate {}",
                if app.oracle { "on" } else { "off" }
            ));
        }
        "setup-verification" => {
            // Hand the terminal to the same offline CLI wizard, then restore the
            // alternate-screen TUI. No model or network is involved.
            let _ = disable_raw_mode();
            let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
            let outcome = match std::env::current_exe() {
                Ok(exe) => tokio::process::Command::new(exe)
                    .arg("setup-verification")
                    .status()
                    .await
                    .map_err(anyhow::Error::from),
                Err(e) => Err(anyhow::Error::from(e)),
            };
            let _ = execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture);
            let _ = enable_raw_mode();
            match outcome {
                Ok(status) if status.success() => app.info_line("verification setup completed"),
                Ok(status) => app.info_line(format!("setup-verification exited with {status}")),
                Err(e) => app.info_line(format!("setup-verification failed: {e}")),
            }
        }
        "verify" => app.info_line(crate::oracle::verify_once().await),
        _ => {
            app.exec_slash(&cmd);
        }
    }
    false
}

/// Finalize a completed agent run: await the task, persist the new tail + run
/// status/usage to the session file, reset the busy/cancel/confirm state, and
/// dispatch any queued message. Called once `agent_task` has finished *and* the
/// typewriter tail has fully revealed (the reveal-drain guard stays in the loop).
#[allow(clippy::too_many_arguments)]
async fn finalize_run(
    app: &mut App,
    agent_task: &mut Option<(tokio::task::JoinHandle<Result<Vec<Message>>>, usize)>,
    cancel: &mut Option<CancellationToken>,
    confirm_ui: &mut Option<(mpsc::Receiver<Prompt>, mpsc::Sender<PromptReply>)>,
    compaction_base: &mut Option<usize>,
    run_usage: &mut (u64, u64, u32),
    messages: &mut Vec<Message>,
    session_path: &PathBuf,
    model: &str,
    client: &Arc<dyn LlmClient>,
    registry: &ToolRegistry,
    system_prompt: &str,
    ev_tx: &mpsc::Sender<AgentEvent>,
) {
    app.flush_pending();
    app.pending_tool_inputs.clear();
    app.status = AgentStatus::Idle;
    let Some((task, n_before)) = agent_task.take() else {
        return;
    };
    // After a mid-run compaction the snapshot entry already covers everything up
    // to `compaction_base`; only the tail is new.
    let persist_from = compaction_base.take().unwrap_or(n_before);
    let was_cancelled = cancel.as_ref().is_some_and(CancellationToken::is_cancelled);
    let outcome = task.await;
    let (run_status, status_reason) = if was_cancelled {
        ("cancelled", Some("cancelled by user".to_string()))
    } else {
        match &outcome {
            Ok(Ok(_)) => ("done", None),
            Ok(Err(e)) => ("error", Some(e.to_string())),
            Err(e) if e.is_cancelled() => ("cancelled", Some("task cancelled".to_string())),
            Err(e) => ("error", Some(format!("join error: {e}"))),
        }
    };
    match outcome {
        Ok(Ok(new_msgs)) => {
            for msg in new_msgs.get(persist_from..).unwrap_or(&[]) {
                session_append(session_path, msg).await;
            }
            if run_usage.0 > 0 {
                let entry = crate::session::SessionEntry::RunUsage {
                    input_tokens: run_usage.0,
                    cached_tokens: run_usage.1,
                    peak_context_tokens: run_usage.2,
                };
                if let Err(e) = crate::session::append(session_path, &entry).await {
                    tracing::warn!("session usage append: {e}");
                }
                if let Err(e) = crate::session::append_run_telemetry(session_path).await {
                    tracing::warn!("session telemetry append: {e}");
                }
                *run_usage = (0, 0, 0);
            }
            *messages = new_msgs;
        }
        Ok(Err(e)) => {
            messages.truncate(n_before.saturating_sub(1));
            app.lines.push(Line::from(Span::styled(
                format!("  ✗ {e}"),
                styled(app.palette.err, false),
            )));
        }
        Err(e) => {
            messages.truncate(n_before.saturating_sub(1));
            app.lines.push(Line::from(Span::styled(
                format!("  ✗ join error: {e}"),
                styled(app.palette.err, false),
            )));
        }
    }
    let entry = crate::session::SessionEntry::RunStatus {
        status: run_status.to_string(),
        reason: status_reason,
    };
    if let Err(e) = crate::session::append(session_path, &entry).await {
        tracing::warn!("session status append: {e}");
    }
    *cancel = None;
    *confirm_ui = None;
    app.prompt = None;
    app.busy = false;
    app.busy_since = None;
    app.auto_scroll = true;

    if let Some(queued) = app.queued_input.take() {
        dispatch_message(
            queued,
            app,
            messages,
            session_path,
            model,
            client,
            registry,
            system_prompt,
            ev_tx,
            agent_task,
            cancel,
            confirm_ui,
        )
        .await;
    }
}

// ── key handling ────────────────────────────────────────────────────────────
//
// The event loop is a thin router: it polls crossterm, hands each `KeyEvent` to
// `handle_key_event`, and executes the returned `KeyOutcome`. The handlers below
// are pure functions of `(&mut App, KeyEvent)` — no terminal, client, channels,
// or async — so they can be unit-tested by feeding synthetic keys and asserting
// on `App` fields. Anything with a side effect (draw, dispatch, cancel, quit,
// thinking-budget change) is deferred to `run_loop` via a `KeyOutcome`.

/// What a key press asks the event loop to do after the pure handler has already
/// applied every `App`-only mutation.
enum KeyOutcome {
    /// Handled; fall through to the agent-lifecycle checks + end-of-loop redraw.
    Continue,
    /// A sub-mode key (settings picker, prompt dialog, popup, completion): redraw
    /// now and skip the rest of this iteration (old `terminal.draw()?; continue;`).
    Redraw,
    /// Cancel any in-flight run and exit the loop.
    Quit,
    /// Ctrl-C in normal mode: two-stage cancel-then-quit (the loop owns the timer).
    CtrlC,
    /// Esc with a run in flight: cancel it but stay in the loop.
    Cancel,
    /// Settings "Thinking" row cycled: apply this budget to the client + `App`.
    SetThinking(Option<u32>),
    /// Prompt dialog answered: send this reply on the confirm bridge, then close.
    SendReply(PromptReply),
    /// Enter on a plain message: dispatch a new turn with this text.
    Submit(String),
    /// Enter on a `/command`: the payload is the text after the leading slash.
    Slash(String),
}

/// Route one key press to the handler for the current mode. F12 (log overlay) and
/// the About screen are handled up front; otherwise settings, prompt, popup, and
/// completion intercept in priority order before normal input handling.
fn handle_key_event(app: &mut App, key: KeyEvent) -> KeyOutcome {
    // F12 toggles the debug log overlay from any mode.
    if key.code == KeyCode::F(12) {
        app.show_logs = !app.show_logs;
        return KeyOutcome::Continue;
    }
    if app.about_mode {
        // Ctrl-C still quits; any other key closes about.
        if key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL {
            return KeyOutcome::Quit;
        }
        app.about_mode = false;
        return KeyOutcome::Continue;
    }
    if app.settings_mode {
        return handle_settings_key(app, key);
    }
    if app.prompt.is_some() {
        return handle_prompt_key(app, key);
    }
    if app.popup.is_some() {
        return handle_popup_key(app, key);
    }
    if app.completion.active && app.focus == Focus::Input {
        if let Some(outcome) = handle_completion_key(app, key) {
            return outcome;
        }
        // completion deactivated on this key → fall through to normal handling
    }
    handle_input_key(app, key)
}

/// Settings screen: arrow-key row navigation, ←/→/space/enter act on the focused
/// row, esc backs out, Ctrl-C quits. An open skills/MCP picker intercepts first.
fn handle_settings_key(app: &mut App, key: KeyEvent) -> KeyOutcome {
    if key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL {
        return KeyOutcome::Quit;
    }
    // Picker sub-screen (skills/mcp) intercepts keys while open.
    if let Some(picker) = &mut app.picker {
        match key.code {
            KeyCode::Up => picker.cursor = picker.cursor.saturating_sub(1),
            KeyCode::Down if picker.cursor + 1 < picker.rows.len() => picker.cursor += 1,
            KeyCode::Char(' ') | KeyCode::Enter | KeyCode::Right => app.toggle_picked(),
            KeyCode::Esc | KeyCode::Left => app.picker = None,
            _ => {}
        }
        return KeyOutcome::Redraw;
    }
    match key.code {
        KeyCode::Esc => app.settings_mode = false,
        KeyCode::Up => app.settings_cursor = app.settings_cursor.saturating_sub(1),
        KeyCode::Down if app.settings_cursor + 1 < SETTINGS_ROWS.len() => app.settings_cursor += 1,
        KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') | KeyCode::Enter => {
            let fwd = key.code != KeyCode::Left;
            let row = SETTINGS_ROWS[app.settings_cursor];
            let outcome = act_settings_row(app, row, fwd);
            if matches!(
                row,
                SettingsRow::Oracle | SettingsRow::Plan | SettingsRow::QuotaBar
            ) {
                app.save_settings();
            }
            return outcome;
        }
        _ => {}
    }
    KeyOutcome::Continue
}

/// Act on the focused settings row. `fwd` = forward (→/space/enter); ← reverses
/// (toggles flip either way; Thinking cycles the other direction). Toggles apply
/// in place; Thinking returns its new budget for the loop to push to the client.
fn act_settings_row(app: &mut App, row: SettingsRow, fwd: bool) -> KeyOutcome {
    match row {
        SettingsRow::Localize => app.localize = !app.localize,
        SettingsRow::Plan => app.plan = !app.plan,
        SettingsRow::Oracle => app.oracle = !app.oracle,
        SettingsRow::QuotaBar => app.quota_bar = !app.quota_bar,
        SettingsRow::Thinking => {
            let next = if fwd {
                match app.thinking_budget {
                    None => Some(8000),
                    Some(b) if b < 16000 => Some(16000),
                    Some(b) if b < 32000 => Some(32000),
                    _ => None,
                }
            } else {
                match app.thinking_budget {
                    None => Some(32000),
                    Some(b) if b > 16000 => Some(16000),
                    Some(b) if b > 8000 => Some(8000),
                    _ => None,
                }
            };
            return KeyOutcome::SetThinking(next);
        }
        SettingsRow::Skills => {
            if fwd {
                app.open_picker(PickerKind::Skills);
            }
        }
        SettingsRow::Mcp => {
            if fwd {
                app.open_picker(PickerKind::Mcp);
            }
        }
    }
    KeyOutcome::Continue
}

/// Prompt dialog (permission gate or `ask_user` question): option navigation,
/// inline glob/free-text editing, and reply construction. Ctrl-C quits; Esc (not
/// editing) denies/cancels. Returns `SendReply` once answered, else `Redraw`.
fn handle_prompt_key(app: &mut App, key: KeyEvent) -> KeyOutcome {
    let Some(ui) = &mut app.prompt else {
        return KeyOutcome::Continue;
    };
    let mut answered: Option<PromptReply> = None;
    match (key.code, key.modifiers) {
        (KeyCode::Char('c'), KeyModifiers::CONTROL) => return KeyOutcome::Quit,
        // Editing the focused row (glob or free text).
        (KeyCode::Char(c), _) if ui.editing => ui.push_char(c),
        (KeyCode::Backspace, _) if ui.editing => ui.backspace(),
        (KeyCode::Esc, _) if ui.editing => ui.editing = false,
        (KeyCode::Enter, _) if ui.editing => {
            ui.editing = false;
            answered = ui.confirm();
        }
        // Navigation / confirm.
        (KeyCode::Up, _) => ui.up(),
        (KeyCode::Down, _) => ui.down(),
        (KeyCode::Char('e'), _) => {
            ui.begin_edit();
        }
        (KeyCode::Enter, _) => answered = ui.confirm(),
        (KeyCode::Esc, _) => answered = Some(PromptReply::default()), // no index = deny/cancel
        _ => {}
    }
    match answered {
        Some(reply) => KeyOutcome::SendReply(reply),
        None => KeyOutcome::Redraw,
    }
}

/// Text/tool-box popup: Esc closes, ↑/↓ scroll. Always redraws.
fn handle_popup_key(app: &mut App, key: KeyEvent) -> KeyOutcome {
    match key.code {
        KeyCode::Esc => app.popup = None,
        KeyCode::Up => {
            if let Some(p) = &mut app.popup {
                p.scroll = p.scroll.saturating_sub(1);
            }
        }
        KeyCode::Down => {
            if let Some(p) = &mut app.popup {
                p.scroll += 1;
            }
        }
        _ => {}
    }
    KeyOutcome::Redraw
}

/// `@`-file / `/`-command completion overlay. Returns `Some(Redraw)` for keys it
/// consumes; `None` when the key deactivates completion and should fall through
/// to normal input handling (empty-query Backspace, or any unhandled key).
fn handle_completion_key(app: &mut App, key: KeyEvent) -> Option<KeyOutcome> {
    match key.code {
        KeyCode::Up => {
            app.completion.selected = app.completion.selected.saturating_sub(1);
            Some(KeyOutcome::Redraw)
        }
        KeyCode::Down => {
            if app.completion.selected + 1 < app.completion.items.len() {
                app.completion.selected += 1;
            }
            Some(KeyOutcome::Redraw)
        }
        KeyCode::Tab | KeyCode::Enter => {
            let mode = app.completion.mode;
            if let Some(picked) = app.completion.accept() {
                match mode {
                    CompletionMode::Slash => {
                        // extract name before "  —"
                        let name = picked.split("  —").next().unwrap_or(&picked).trim();
                        app.input = format!("/{name}");
                        app.cursor_pos = app.input.chars().count();
                    }
                    CompletionMode::File => {
                        let start = app
                            .input
                            .char_indices()
                            .nth(app.completion.trigger_pos)
                            .map(|(i, _)| i)
                            .unwrap_or(app.input.len());
                        let end = app
                            .input
                            .char_indices()
                            .nth(app.cursor_pos)
                            .map(|(i, _)| i)
                            .unwrap_or(app.input.len());
                        app.input.replace_range(start..end, &picked);
                        app.cursor_pos = app.completion.trigger_pos + picked.chars().count();
                    }
                }
            }
            Some(KeyOutcome::Redraw)
        }
        KeyCode::Esc => {
            app.completion.deactivate();
            Some(KeyOutcome::Redraw)
        }
        KeyCode::Backspace => {
            if app.completion.query.is_empty() {
                app.completion.deactivate();
                None // fall through to normal backspace
            } else {
                app.completion.query.pop();
                app.completion.filter();
                // also delete char from input
                app.cursor_pos -= 1;
                let byte_pos = app.byte_at_cursor();
                app.input.remove(byte_pos);
                Some(KeyOutcome::Redraw)
            }
        }
        KeyCode::Char(c) => {
            // type char into input + update query
            let byte_pos = app.byte_at_cursor();
            app.input.insert(byte_pos, c);
            app.cursor_pos += 1;
            app.completion.query.push(c);
            app.completion.filter();
            if app.completion.items.is_empty() {
                app.completion.deactivate();
            }
            Some(KeyOutcome::Redraw)
        }
        _ => {
            app.completion.deactivate();
            None
        }
    }
}

/// Normal (chat) input handling: Ctrl-C/Esc lifecycle, Alt shortcuts, Enter
/// submit/queue, focus/scroll/history, and cursor-aware text editing. Side
/// effects (submit, slash, cancel, quit) are returned as a `KeyOutcome`.
fn handle_input_key(app: &mut App, key: KeyEvent) -> KeyOutcome {
    match (key.code, key.modifiers) {
        (KeyCode::Char('c'), KeyModifiers::CONTROL) => return KeyOutcome::CtrlC,
        // First Esc drops a queued message; otherwise cancel the agent.
        (KeyCode::Esc, _) if app.queued_input.is_some() => app.queued_input = None,
        (KeyCode::Esc, _) => return KeyOutcome::Cancel,
        // `/quit` must work mid-turn too: quit immediately instead of queueing.
        (KeyCode::Enter, _) if app.input.trim() == "/quit" => return KeyOutcome::Quit,
        (KeyCode::Char('b'), KeyModifiers::ALT) => {
            app.panel_visible = !app.panel_visible;
            app.replay(); // re-wrap chat to the new width
        }
        (KeyCode::Char('s'), KeyModifiers::ALT) => {
            app.settings_mode = true;
            app.picker = None;
        }
        (KeyCode::Char('a'), KeyModifiers::ALT) => app.about_mode = true,
        // Image paste. The terminal keeps Ctrl-Shift-V for text, so this binding
        // only ever competes with a clipboard that has no image in it.
        (KeyCode::Char('v'), KeyModifiers::CONTROL) => app.attach_from_clipboard(),
        (KeyCode::Char('p'), KeyModifiers::ALT) => {
            app.palette_idx = (app.palette_idx + 1) % PALETTES.len();
            app.palette = &PALETTES[app.palette_idx].1;
            app.save_palette();
            app.replay();
        }
        (KeyCode::Enter, _) if !app.input.is_empty() && !app.busy => {
            let text = std::mem::take(&mut app.input);
            app.cursor_pos = 0;
            app.auto_scroll = true;
            app.push_history(&text);
            return match text.strip_prefix('/') {
                Some(cmd) => KeyOutcome::Slash(cmd.to_string()),
                None => KeyOutcome::Submit(text),
            };
        }
        (KeyCode::Enter, _) if !app.input.is_empty() && app.busy && app.queued_input.is_none() => {
            let text = std::mem::take(&mut app.input);
            app.push_history(&text);
            app.queued_input = Some(text);
            app.cursor_pos = 0;
        }
        (KeyCode::Tab, _) | (KeyCode::BackTab, _) => {
            app.focus = if app.focus == Focus::Input {
                Focus::Output
            } else {
                Focus::Input
            };
        }
        (KeyCode::Up, _) if app.focus == Focus::Output => {
            app.auto_scroll = false;
            app.scroll = app.scroll.saturating_sub(1);
            app.selected_entry = None;
        }
        (KeyCode::Down, _) if app.focus == Focus::Output => {
            app.scroll = app.scroll.saturating_add(1);
            if app.scroll >= app.max_scroll {
                app.auto_scroll = true;
            }
            app.selected_entry = None;
        }
        (KeyCode::Up, _) if app.focus == Focus::Input => app.history_prev(),
        (KeyCode::Down, _) if app.focus == Focus::Input => app.history_next(),
        (KeyCode::PageUp, _) if app.focus == Focus::Output => {
            app.auto_scroll = false;
            app.scroll = app.scroll.saturating_sub(10);
            app.selected_entry = None;
        }
        (KeyCode::PageDown, _) if app.focus == Focus::Output => {
            app.auto_scroll = false;
            app.scroll = app.scroll.saturating_add(10);
            app.selected_entry = None;
        }
        (KeyCode::Char('G'), _) if app.focus == Focus::Output => {
            app.auto_scroll = true;
            app.selected_entry = None;
        }
        (KeyCode::Left, _) if app.focus == Focus::Input => {
            app.cursor_pos = app.cursor_pos.saturating_sub(1);
        }
        (KeyCode::Right, _) if app.focus == Focus::Input => {
            let n = app.input.chars().count();
            if app.cursor_pos < n {
                app.cursor_pos += 1;
            }
        }
        (KeyCode::Home, _) if app.focus == Focus::Input => app.cursor_pos = 0,
        (KeyCode::End, _) if app.focus == Focus::Input => {
            app.cursor_pos = app.input.chars().count();
        }
        (KeyCode::Char(c), _) if app.focus == Focus::Input => {
            app.history_idx = None; // editing detaches from history
            let byte_pos = app.byte_at_cursor();
            app.input.insert(byte_pos, c);
            app.cursor_pos += 1;
            if c == '@' {
                app.completion.activate_files(app.cursor_pos - 1);
            } else if c == '/' && app.cursor_pos == 1 && app.input == "/" {
                app.completion.activate_slash(&app.slash_names);
            }
        }
        (KeyCode::Backspace, _) if app.focus == Focus::Input && app.cursor_pos > 0 => {
            app.history_idx = None;
            app.cursor_pos -= 1;
            let byte_pos = app.byte_at_cursor();
            app.input.remove(byte_pos);
        }
        (KeyCode::Delete, _) if app.focus == Focus::Input => {
            let n = app.input.chars().count();
            if app.cursor_pos < n {
                let byte_pos = app.byte_at_cursor();
                app.input.remove(byte_pos);
            }
        }
        _ => {}
    }
    KeyOutcome::Continue
}

fn handle_mouse(app: &mut App, mouse: MouseEvent) {
    match mouse.kind {
        MouseEventKind::ScrollUp => {
            if let Some(p) = &mut app.popup {
                p.scroll = p.scroll.saturating_sub(3);
            } else if over_trail(app, mouse.column) {
                app.trail_scroll = app.trail_scroll.saturating_add(3).min(app.trail_max_scroll);
            } else {
                app.auto_scroll = false;
                app.scroll = app.scroll.saturating_sub(3);
                app.selected_entry = None; // manual scroll → resume auto-follow
            }
        }
        MouseEventKind::ScrollDown => {
            if let Some(p) = &mut app.popup {
                p.scroll = p.scroll.saturating_add(3);
            } else if over_trail(app, mouse.column) {
                app.trail_scroll = app.trail_scroll.saturating_sub(3);
            } else {
                app.scroll = app.scroll.saturating_add(3);
                if app.scroll >= app.max_scroll {
                    app.auto_scroll = true;
                }
                app.selected_entry = None; // manual scroll → resume auto-follow
            }
        }
        // Light up the divider when the pointer is over its grab column.
        MouseEventKind::Moved => app.hover_divider = on_divider(app, mouse.column),
        // Drag the chat/panel divider to resize the side panel on the fly.
        MouseEventKind::Drag(MouseButton::Left) if app.resizing_panel => {
            resize_panel(app, mouse.column);
        }
        MouseEventKind::Up(MouseButton::Left) => app.resizing_panel = false,
        MouseEventKind::Down(MouseButton::Left) => {
            // Close any open popup first, then try to open from click
            app.popup = None;
            // Grab the divider (panel's left border column) to start a resize.
            if on_divider(app, mouse.column) {
                app.resizing_panel = true;
                return;
            }
            // Click in the timeline panel: scroll the chat to that step.
            let ta = app.timeline_area;
            if ta.width > 0
                && mouse.row >= ta.y
                && mouse.row < ta.y + ta.height
                && mouse.column >= ta.x
                && mouse.column < ta.x + ta.width
            {
                if let Some(&idx) = app.timeline_rows.get((mouse.row - ta.y) as usize) {
                    if let Some(entry) = app.timeline.get(idx) {
                        app.scroll_to = Some(entry.target);
                        app.selected_entry = Some(idx);
                    }
                }
                return;
            }
            let ca = app.chat_area;
            if ca.height >= 2
                && mouse.row > ca.y
                && mouse.row < ca.y + ca.height - 1
                && mouse.column > ca.x
                && mouse.column < ca.x + ca.width - 1
            {
                let content_row = (mouse.row - ca.y - 1) as usize + app.scroll;
                if let Some(idx) = app
                    .tool_boxes
                    .iter()
                    .position(|tb| content_row >= tb.line_start && content_row <= tb.line_end)
                {
                    app.popup = Some(Popup {
                        content: PopupContent::Tool { tool_idx: idx },
                        scroll: 0,
                    });
                }
            }
        }
        _ => {}
    }
}

/// The resize grab column: the divider between chat and the side panel (±1).
fn on_divider(app: &App, col: u16) -> bool {
    if !app.panel_visible {
        return false;
    }
    let divider = app.chat_area.x + app.chat_area.width;
    col + 1 >= divider && col <= divider + 1
}

/// Whether the pointer column is over the side trail panel (for scroll routing).
fn over_trail(app: &App, col: u16) -> bool {
    app.panel_visible && col >= app.chat_area.x + app.chat_area.width
}

/// Resize the side panel so its left edge tracks the cursor column, clamped to
/// `[TIMELINE_W_MIN, TIMELINE_W_MAX]` and leaving the chat at least 20 columns.
/// Re-wraps the chat to the new width.
fn resize_panel(app: &mut App, cursor_col: u16) {
    let term_w = ratatui::crossterm::terminal::size()
        .map(|(w, _)| w)
        .unwrap_or(80);
    let new_w = term_w.saturating_sub(cursor_col).clamp(
        TIMELINE_W_MIN,
        TIMELINE_W_MAX.min(term_w.saturating_sub(22)),
    );
    if new_w != app.panel_w {
        app.panel_w = new_w;
        app.replay(); // re-wrap chat to the new width
    }
}

/// `/resume` — with no argument, list saved conversations; with `<n>`, load that
/// one: swap in its history, repaint the scrollback, and continue writing to its file.
async fn resume_command(
    arg: &str,
    app: &mut App,
    messages: &mut Vec<Message>,
    session_path: &mut PathBuf,
) {
    let p = app.palette;
    let sessions = crate::session::list_sessions().await;
    if sessions.is_empty() {
        app.lines.push(Line::default());
        app.lines.push(Line::from(Span::styled(
            "  no saved conversations",
            styled(p.muted, false),
        )));
        return;
    }
    if arg.is_empty() {
        app.lines.push(Line::default());
        app.lines.push(Line::from(Span::styled(
            "  Recent conversations — /resume <n>:",
            styled(p.accent, true),
        )));
        for (i, s) in sessions.iter().enumerate().take(20) {
            let dt = chrono::DateTime::<chrono::Local>::from(s.modified);
            app.lines.push(Line::from(vec![
                Span::styled(format!("  {:>2}  ", i + 1), styled(p.info, true)),
                Span::styled(
                    format!("{}  ", dt.format("%d %b %H:%M")),
                    styled(p.muted, false),
                ),
                Span::styled(s.preview.clone(), styled(p.fg, false)),
            ]));
        }
        app.lines.push(Line::default());
        return;
    }
    let Ok(n) = arg.parse::<usize>() else {
        app.lines.push(Line::from(Span::styled(
            format!("  /resume: invalid index '{arg}'"),
            styled(p.err, false),
        )));
        return;
    };
    let Some(info) = sessions.get(n.wrapping_sub(1)) else {
        app.lines.push(Line::from(Span::styled(
            format!("  /resume: no session {n}"),
            styled(p.err, false),
        )));
        return;
    };
    *messages =
        crate::session::collapse(crate::session::load(&info.path).await.unwrap_or_default());
    *session_path = info.path.clone();
    app.reset();
    app.replay_events = messages_to_replay(messages);
    app.reset_durations_for_replay();
    app.replay();
}

/// `/model` — no argument lists the provider's models (caching them for index
/// selection); `<n>` picks from that listing, `<name>` switches directly.
async fn model_command(arg: &str, app: &mut App, client: &Arc<dyn LlmClient>) {
    let p = app.palette;
    if arg.is_empty() {
        match client.list_models().await {
            Ok(models) if !models.is_empty() => {
                app.last_models = models;
                app.lines.push(Line::default());
                app.lines.push(Line::from(Span::styled(
                    format!("  Models — /model <n> (current: {}):", app.model),
                    styled(p.accent, true),
                )));
                for (i, m) in app.last_models.iter().enumerate() {
                    let marker = if *m == app.model { "●" } else { " " };
                    app.lines.push(Line::from(vec![
                        Span::styled(format!("  {:>2}  ", i + 1), styled(p.info, true)),
                        Span::styled(format!("{marker} "), styled(p.accent, false)),
                        Span::styled(m.clone(), styled(p.fg, false)),
                    ]));
                }
                app.lines.push(Line::default());
            }
            Ok(_) => app.lines.push(Line::from(Span::styled(
                "  /model: no models listed — use /model <name>",
                styled(p.muted, false),
            ))),
            Err(e) => app.lines.push(Line::from(Span::styled(
                format!("  /model: listing unavailable ({e}) — use /model <name>"),
                styled(p.muted, false),
            ))),
        }
        return;
    }
    let name = match arg.parse::<usize>() {
        Ok(n) => match app.last_models.get(n.wrapping_sub(1)) {
            Some(m) => m.clone(),
            None => {
                app.lines.push(Line::from(Span::styled(
                    format!("  /model: invalid index '{arg}'"),
                    styled(p.err, false),
                )));
                return;
            }
        },
        Err(_) => arg.to_string(),
    };
    let cwd = super::app::cwd();
    crate::agent::switch_model(client.as_ref(), &cwd, name.clone());
    app.model = name.clone();
    app.lines.push(Line::from(Span::styled(
        format!("  ▌ model → {name}"),
        styled(p.accent, true),
    )));
}

/// `/tokens` — input-token count of the full next payload (system prompt + tool
/// schemas + conversation), via the provider when supported, else a local estimate.
async fn tokens_command(
    app: &mut App,
    client: &Arc<dyn LlmClient>,
    system_prompt: &str,
    messages: &[Message],
    registry: &ToolRegistry,
) {
    let p = app.palette;
    let mut msgs = vec![Message {
        role: crate::types::Role::System,
        injected: false,
        content: vec![crate::types::ContentBlock::Text {
            text: system_prompt.to_string(),
        }],
    }];
    msgs.extend(messages.iter().cloned());
    let line = match client
        .count_tokens(&msgs.iter().collect::<Vec<_>>(), registry)
        .await
    {
        Ok(n) => format!("  {n} tokens  (system + tools + conversation, provider count)"),
        Err(_) => format!(
            "  ~{} tokens  (local estimate — provider count unavailable)",
            crate::agent::estimate_context_tokens(&msgs)
        ),
    };
    app.lines
        .push(Line::from(Span::styled(line, styled(p.accent, false))));
}

/// Summarize the conversation to free context, in place. Builds a transient
/// `AgentContext` (the TUI keeps `messages`, not a persistent context) and
/// reuses `agent::compact`. The real `ev_tx` is passed so the context-usage
/// bar refreshes.
/// `/rollback` — no arg lists workspace snapshots, `<n|id>` restores one.
async fn rollback_command(arg: &str, app: &mut App) {
    let p = app.palette;
    let Some(snaps) = crate::snapshot::workspace_snapshots() else {
        app.lines.push(Line::from(Span::styled(
            "  snapshots disabled (SIRBONE_NO_SNAPSHOT)".to_string(),
            styled(p.muted, false),
        )));
        return;
    };
    if arg.is_empty() {
        let entries = snaps.list_detailed(10).await;
        if entries.is_empty() {
            app.lines.push(Line::from(Span::styled(
                "  no snapshots yet (one is taken before each run that edits files)".to_string(),
                styled(p.muted, false),
            )));
            return;
        }
        app.lines.push(Line::default());
        for (i, entry) in entries.iter().enumerate() {
            app.lines.push(Line::from(vec![
                Span::styled(
                    format!("  {:>2}. {}", i + 1, entry.short_id),
                    styled(p.accent, false),
                ),
                Span::styled(
                    format!("  {}  — {}", entry.age, entry.label),
                    styled(p.muted, false),
                ),
            ]));
            for file in &entry.changed_files {
                app.lines.push(Line::from(Span::styled(
                    format!("      {file}"),
                    styled(p.muted, false),
                )));
            }
        }
        app.lines.push(Line::from(Span::styled(
            "  restore with /rollback <n|id>".to_string(),
            styled(p.muted, false),
        )));
        return;
    }
    let line = match snaps.rollback(arg).await {
        Ok(msg) => format!("  {msg}"),
        Err(e) => format!("  rollback failed: {e}"),
    };
    app.lines
        .push(Line::from(Span::styled(line, styled(p.accent, false))));
}

async fn snapshots_command(app: &mut App) {
    let p = app.palette;
    let Some(snaps) = crate::snapshot::workspace_snapshots() else {
        app.lines.push(Line::from(Span::styled(
            "  snapshots disabled (SIRBONE_NO_SNAPSHOT)".to_string(),
            styled(p.muted, false),
        )));
        return;
    };
    let entries = snaps.list_detailed(20).await;
    if entries.is_empty() {
        app.lines.push(Line::from(Span::styled(
            "  no snapshots yet (one is taken before each run that edits files)".to_string(),
            styled(p.muted, false),
        )));
        return;
    }

    let mut lines = Vec::new();
    for (i, entry) in entries.iter().enumerate() {
        lines.push(format!(
            "{:>2}. {}  {}  - {}",
            i + 1,
            entry.short_id,
            entry.age,
            entry.label
        ));
        for file in &entry.changed_files {
            lines.push(format!("    {file}"));
        }
        lines.push(String::new());
    }
    lines.push("restore remains /rollback <n|id>".to_string());
    app.popup = Some(Popup {
        content: PopupContent::Text {
            title: " snapshots ".to_string(),
            lines,
        },
        scroll: 0,
    });
}

#[allow(clippy::too_many_arguments)]
async fn compact_command(
    app: &mut App,
    model: &str,
    client: &Arc<dyn LlmClient>,
    registry: &ToolRegistry,
    system_prompt: &str,
    messages: &mut Vec<Message>,
    ev_tx: &mpsc::Sender<AgentEvent>,
) {
    let p = app.palette;
    let n_before = messages.len();
    let context_window = client.context_window().await.map(|n| n as usize);
    let mut ctx = AgentContext {
        model: model.to_owned(),
        system_prompt: Some(system_prompt.to_owned()),
        messages: std::mem::take(messages),
        tools: registry.clone(),
        client: Arc::clone(client),
        events: ev_tx.clone(),
        cancel: CancellationToken::new(),
        context_window,
        confirm: None,
        compaction_keep_recent: None,
        permissions: crate::PermissionConfig::load(),
        snapshots: None,
        hooks: Default::default(),
        oracle: None,
        max_steps: None,
        spend_cap: None,
        tokens_spent: 0,
        stream_rules: Default::default(),
        compacted_files: Vec::new(),
        last_request: None,
    };
    let line = match crate::agent::compact(&mut ctx).await {
        Ok(()) => format!(
            "  context compacted: {n_before} → {} messages",
            ctx.messages.len()
        ),
        Err(e) => format!("  compaction skipped: {e}"),
    };
    *messages = std::mem::take(&mut ctx.messages);
    app.lines
        .push(Line::from(Span::styled(line, styled(p.accent, false))));
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_message(
    text: String,
    app: &mut App,
    messages: &mut Vec<Message>,
    session_path: &PathBuf,
    model: &str,
    client: &Arc<dyn LlmClient>,
    registry: &ToolRegistry,
    system_prompt: &str,
    ev_tx: &mpsc::Sender<AgentEvent>,
    agent_task: &mut Option<(tokio::task::JoinHandle<Result<Vec<Message>>>, usize)>,
    cancel: &mut Option<CancellationToken>,
    confirm_ui: &mut Option<(mpsc::Receiver<Prompt>, mpsc::Sender<PromptReply>)>,
) {
    // A real prompt is going out — seed/roll the estimated quota window.
    app.touch_quota_window();
    // Pasted images ride along with this turn and are cleared from the composer.
    let attached = std::mem::take(&mut app.attachments);
    let display_text = format!("{}{text}", crate::attachments::pills(&attached));
    if app.plan {
        registry.start_plan(&text);
    } else {
        registry.notes.stop_plan();
    }
    let user_msg = Message {
        role: crate::types::Role::User,
        injected: false,
        content: crate::attachments::user_content(&attached, text),
    };
    app.replay_events
        .push(ReplayEvent::User(display_text.clone()));
    let p = app.palette;
    let w = app.chat_w();
    let target = app.lines.len();
    app.lines.push(Line::default());
    app.lines.extend(user_box(&display_text, w, p));
    app.lines.push(Line::default());
    app.record_user_timeline(&display_text, target);
    session_append(session_path, &user_msg).await;
    // Prefix fingerprint for the run about to start (see `SessionEntry::RequestHeader`).
    if let Err(e) =
        crate::session::append_request_header(session_path, model, Some(system_prompt), registry)
            .await
    {
        tracing::warn!("session request header append: {e}");
    }
    messages.push(user_msg);
    app.busy = true;
    app.busy_since = Some(Instant::now());
    app.status = AgentStatus::LlmThinking;

    let n_before = messages.len();
    let msgs_snap = messages.clone();
    let ct = CancellationToken::new();
    *cancel = Some(ct.clone());
    // User-prompt bridge: agent side moves into the task, UI side stays here.
    let (ask_tx, ask_rx) = mpsc::channel::<Prompt>(1);
    let (reply_tx, reply_rx) = mpsc::channel::<PromptReply>(1);
    *confirm_ui = Some((ask_rx, reply_tx));
    let localize_on = app.localize;
    let oracle_on = app.oracle;
    let task_text = display_text;
    let (m, c, r, sp, etx) = (
        model.to_owned(),
        Arc::clone(client),
        registry.clone(),
        system_prompt.to_owned(),
        ev_tx.clone(),
    );
    *agent_task = Some((
        tokio::spawn(async move {
            if localize_on {
                // Localization pre-pass (Settings `l`): seed working notes with where to
                // change before the main run. Runs inside the task so the UI stays live.
                if let Some(report) = crate::agent::localize(
                    Arc::clone(&c),
                    &m,
                    &task_text,
                    crate::tools::read_only_registry(),
                    6,
                    &ct,
                )
                .await
                {
                    r.notes.seed(format!(
                        "LOCALIZATION (where the change likely belongs):\n{report}"
                    ));
                }
            }
            let context_window = c.context_window().await.map(|n| n as usize);
            let stream_rules = crate::stream_rules::install(c.as_ref());
            let mut ctx = AgentContext {
                model: m,
                system_prompt: Some(sp),
                messages: msgs_snap,
                tools: r,
                client: c,
                events: etx,
                cancel: ct,
                context_window,
                confirm: Some(ConfirmBridge {
                    ask: ask_tx,
                    reply: reply_rx,
                }),
                compaction_keep_recent: None,
                permissions: crate::PermissionConfig::load(),
                snapshots: crate::snapshot::workspace_snapshots(),
                hooks: crate::checks::Hooks::load(),
                oracle: oracle_on.then(crate::oracle::Oracle::load).flatten(),
                max_steps: crate::agent::env_max_steps(),
                spend_cap: crate::config::spend_cap(),
                tokens_spent: 0,
                stream_rules,
                compacted_files: Vec::new(),
                last_request: None,
            };
            crate::run(&mut ctx).await?;
            Ok(std::mem::take(&mut ctx.messages))
        }),
        n_before,
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{Prompt, PromptKind};
    use crate::tui::app::{PickRow, Picker};
    use crate::tui::widgets::PromptUi;

    fn app() -> App {
        App::new("model".into(), "prov".into())
    }
    fn k(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    fn km(code: KeyCode, m: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, m)
    }
    fn ch(c: char) -> KeyEvent {
        k(KeyCode::Char(c))
    }

    // ── text input: insert, cursor, edit ────────────────────────────────────

    #[test]
    fn typing_inserts_and_advances_cursor() {
        let mut a = app();
        for c in "abc".chars() {
            assert!(matches!(
                handle_key_event(&mut a, ch(c)),
                KeyOutcome::Continue
            ));
        }
        assert_eq!(a.input, "abc");
        assert_eq!(a.cursor_pos, 3);
    }

    #[test]
    fn cursor_left_right_home_end_and_midword_insert() {
        let mut a = app();
        for c in "abc".chars() {
            handle_key_event(&mut a, ch(c));
        }
        handle_key_event(&mut a, k(KeyCode::Left)); // cursor 2
        handle_key_event(&mut a, k(KeyCode::Left)); // cursor 1
        assert_eq!(a.cursor_pos, 1);
        handle_key_event(&mut a, ch('Z')); // insert at 1 -> aZbc
        assert_eq!(a.input, "aZbc");
        assert_eq!(a.cursor_pos, 2);
        handle_key_event(&mut a, k(KeyCode::Home));
        assert_eq!(a.cursor_pos, 0);
        handle_key_event(&mut a, k(KeyCode::End));
        assert_eq!(a.cursor_pos, 4);
        // Right at end is clamped.
        handle_key_event(&mut a, k(KeyCode::Right));
        assert_eq!(a.cursor_pos, 4);
    }

    #[test]
    fn backspace_and_delete_remove_around_cursor() {
        let mut a = app();
        for c in "hello".chars() {
            handle_key_event(&mut a, ch(c));
        }
        handle_key_event(&mut a, k(KeyCode::Backspace)); // "hell"
        assert_eq!(a.input, "hell");
        assert_eq!(a.cursor_pos, 4);
        handle_key_event(&mut a, k(KeyCode::Home));
        handle_key_event(&mut a, k(KeyCode::Delete)); // remove 'h' -> "ell"
        assert_eq!(a.input, "ell");
        assert_eq!(a.cursor_pos, 0);
    }

    // ── completion (@ files, / commands) ────────────────────────────────────

    #[test]
    fn slash_opens_completion_then_esc_closes() {
        let mut a = app();
        handle_key_event(&mut a, ch('/'));
        assert!(a.completion.active);
        assert!(matches!(a.completion.mode, CompletionMode::Slash));
        // Esc while completion active deactivates it (input unchanged).
        assert!(matches!(
            handle_key_event(&mut a, k(KeyCode::Esc)),
            KeyOutcome::Redraw
        ));
        assert!(!a.completion.active);
        assert_eq!(a.input, "/");
    }

    #[test]
    fn slash_completion_tab_accepts_into_input() {
        let mut a = app();
        handle_key_event(&mut a, ch('/'));
        handle_key_event(&mut a, k(KeyCode::Down)); // select 2nd builtin
        handle_key_event(&mut a, k(KeyCode::Tab));
        assert!(!a.completion.active);
        assert!(a.input.starts_with('/'));
        assert_eq!(a.cursor_pos, a.input.chars().count());
    }

    #[test]
    fn at_opens_file_completion() {
        let mut a = app();
        handle_key_event(&mut a, ch('@'));
        assert!(a.completion.active);
        assert!(matches!(a.completion.mode, CompletionMode::File));
    }

    #[test]
    fn completion_backspace_on_empty_query_falls_through_to_delete() {
        let mut a = app();
        handle_key_event(&mut a, ch('/')); // input "/", query empty, active
                                           // Backspace with empty query: deactivates AND the fall-through deletes '/'.
        let out = handle_key_event(&mut a, k(KeyCode::Backspace));
        assert!(matches!(out, KeyOutcome::Continue));
        assert!(!a.completion.active);
        assert_eq!(a.input, "");
    }

    // ── focus + scroll ──────────────────────────────────────────────────────

    #[test]
    fn tab_toggles_focus() {
        let mut a = app();
        assert!(a.focus == Focus::Input);
        handle_key_event(&mut a, k(KeyCode::Tab));
        assert!(a.focus == Focus::Output);
        handle_key_event(&mut a, k(KeyCode::BackTab));
        assert!(a.focus == Focus::Input);
    }

    #[test]
    fn scroll_keys_move_scroll_and_g_reenables_autoscroll() {
        let mut a = app();
        a.focus = Focus::Output;
        a.max_scroll = 100;
        a.scroll = 50;
        handle_key_event(&mut a, k(KeyCode::Up));
        assert_eq!(a.scroll, 49);
        assert!(!a.auto_scroll);
        handle_key_event(&mut a, k(KeyCode::PageUp));
        assert_eq!(a.scroll, 39);
        handle_key_event(&mut a, k(KeyCode::PageDown));
        assert_eq!(a.scroll, 49);
        handle_key_event(&mut a, ch('G'));
        assert!(a.auto_scroll);
        // Down past max_scroll re-enables auto_scroll.
        a.scroll = a.max_scroll;
        a.auto_scroll = false;
        handle_key_event(&mut a, k(KeyCode::Down));
        assert!(a.auto_scroll);
    }

    #[test]
    fn up_down_recall_history_in_input_focus() {
        let mut a = app();
        a.push_history("first");
        a.push_history("second");
        handle_key_event(&mut a, k(KeyCode::Up)); // most recent
        assert_eq!(a.input, "second");
        handle_key_event(&mut a, k(KeyCode::Up));
        assert_eq!(a.input, "first");
        handle_key_event(&mut a, k(KeyCode::Down));
        assert_eq!(a.input, "second");
    }

    // ── enter: submit / slash / queue ───────────────────────────────────────

    #[test]
    fn enter_plain_text_submits() {
        let mut a = app();
        for c in "do it".chars() {
            handle_key_event(&mut a, ch(c));
        }
        match handle_key_event(&mut a, k(KeyCode::Enter)) {
            KeyOutcome::Submit(t) => assert_eq!(t, "do it"),
            o => panic!("expected Submit, got {}", outcome_name(&o)),
        }
        assert_eq!(a.input, "");
        assert_eq!(a.cursor_pos, 0);
        assert!(a.auto_scroll);
    }

    #[test]
    fn enter_slash_command_routes_to_slash() {
        let mut a = app();
        for c in "help".chars() {
            // seed input directly to skip the '/'-completion path
            a.input.push(c);
        }
        a.input.insert(0, '/');
        a.cursor_pos = a.input.chars().count();
        match handle_key_event(&mut a, k(KeyCode::Enter)) {
            KeyOutcome::Slash(cmd) => assert_eq!(cmd, "help"),
            o => panic!("expected Slash, got {}", outcome_name(&o)),
        }
    }

    #[test]
    fn enter_while_busy_queues_input() {
        let mut a = app();
        a.busy = true;
        for c in "later".chars() {
            handle_key_event(&mut a, ch(c));
        }
        assert!(matches!(
            handle_key_event(&mut a, k(KeyCode::Enter)),
            KeyOutcome::Continue
        ));
        assert_eq!(a.queued_input.as_deref(), Some("later"));
        assert_eq!(a.input, "");
    }

    #[test]
    fn quit_slash_short_circuits_even_when_busy() {
        let mut a = app();
        a.busy = true;
        for c in "/quit".chars() {
            a.input.push(c);
        }
        a.cursor_pos = a.input.chars().count();
        assert!(matches!(
            handle_key_event(&mut a, k(KeyCode::Enter)),
            KeyOutcome::Quit
        ));
    }

    // ── esc precedence + cancel ─────────────────────────────────────────────

    #[test]
    fn esc_first_drops_queued_then_cancels() {
        let mut a = app();
        a.queued_input = Some("q".into());
        assert!(matches!(
            handle_key_event(&mut a, k(KeyCode::Esc)),
            KeyOutcome::Continue
        ));
        assert!(a.queued_input.is_none());
        // Second Esc (nothing queued) cancels the in-flight run.
        assert!(matches!(
            handle_key_event(&mut a, k(KeyCode::Esc)),
            KeyOutcome::Cancel
        ));
    }

    #[test]
    fn esc_closes_popup_before_normal_cancel() {
        let mut a = app();
        a.popup = Some(Popup {
            content: PopupContent::Text {
                title: "t".into(),
                lines: vec![],
            },
            scroll: 0,
        });
        assert!(matches!(
            handle_key_event(&mut a, k(KeyCode::Esc)),
            KeyOutcome::Redraw
        ));
        assert!(a.popup.is_none());
    }

    #[test]
    fn esc_closes_settings_picker_before_settings_itself() {
        let mut a = app();
        a.settings_mode = true;
        a.picker = Some(Picker {
            kind: PickerKind::Skills,
            rows: vec![],
            cursor: 0,
        });
        // First Esc closes the picker, staying in settings.
        assert!(matches!(
            handle_key_event(&mut a, k(KeyCode::Esc)),
            KeyOutcome::Redraw
        ));
        assert!(a.picker.is_none());
        assert!(a.settings_mode);
        // Second Esc leaves settings.
        assert!(matches!(
            handle_key_event(&mut a, k(KeyCode::Esc)),
            KeyOutcome::Continue
        ));
        assert!(!a.settings_mode);
    }

    // ── ctrl-c per mode ─────────────────────────────────────────────────────

    #[test]
    fn ctrl_c_is_ctrlc_in_normal_and_quit_in_overlays() {
        let mut a = app();
        let cc = km(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(matches!(handle_key_event(&mut a, cc), KeyOutcome::CtrlC));

        let mut a = app();
        a.about_mode = true;
        assert!(matches!(handle_key_event(&mut a, cc), KeyOutcome::Quit));

        let mut a = app();
        a.settings_mode = true;
        assert!(matches!(handle_key_event(&mut a, cc), KeyOutcome::Quit));

        let mut a = app();
        a.prompt = Some(PromptUi::new(question_prompt()));
        assert!(matches!(handle_key_event(&mut a, cc), KeyOutcome::Quit));
    }

    #[test]
    fn about_mode_any_key_closes_it() {
        let mut a = app();
        a.about_mode = true;
        assert!(matches!(
            handle_key_event(&mut a, ch('x')),
            KeyOutcome::Continue
        ));
        assert!(!a.about_mode);
    }

    // ── settings ────────────────────────────────────────────────────────────

    #[test]
    fn settings_arrows_navigate_rows() {
        let mut a = app();
        a.settings_mode = true;
        assert_eq!(a.settings_cursor, 0);
        handle_key_event(&mut a, k(KeyCode::Down));
        assert_eq!(a.settings_cursor, 1);
        handle_key_event(&mut a, k(KeyCode::Up));
        assert_eq!(a.settings_cursor, 0);
        // Up at the top is clamped.
        handle_key_event(&mut a, k(KeyCode::Up));
        assert_eq!(a.settings_cursor, 0);
    }

    #[test]
    fn settings_localize_toggle_is_in_place() {
        let mut a = app();
        a.settings_mode = true;
        a.settings_cursor = 0; // Localize
        let before = a.localize;
        handle_key_event(&mut a, k(KeyCode::Right));
        assert_eq!(a.localize, !before);
    }

    #[test]
    fn settings_thinking_cycles_forward_and_back() {
        let mut a = app();
        a.thinking_budget = None;
        assert!(matches!(
            act_settings_row(&mut a, SettingsRow::Thinking, true),
            KeyOutcome::SetThinking(Some(8000))
        ));
        a.thinking_budget = Some(8000);
        assert!(matches!(
            act_settings_row(&mut a, SettingsRow::Thinking, true),
            KeyOutcome::SetThinking(Some(16000))
        ));
        a.thinking_budget = Some(32000);
        assert!(matches!(
            act_settings_row(&mut a, SettingsRow::Thinking, true),
            KeyOutcome::SetThinking(None)
        ));
        a.thinking_budget = None;
        assert!(matches!(
            act_settings_row(&mut a, SettingsRow::Thinking, false),
            KeyOutcome::SetThinking(Some(32000))
        ));
    }

    #[test]
    fn settings_picker_selection_round_trips() {
        let mut a = app();
        a.settings_mode = true;
        a.picker = Some(Picker {
            kind: PickerKind::Skills,
            rows: vec![
                PickRow {
                    name: "one".into(),
                    description: String::new(),
                    tag: String::new(),
                    enabled: false,
                },
                PickRow {
                    name: "two".into(),
                    description: String::new(),
                    tag: String::new(),
                    enabled: false,
                },
            ],
            cursor: 0,
        });
        handle_key_event(&mut a, k(KeyCode::Down));
        assert_eq!(a.picker.as_ref().unwrap().cursor, 1);
        // Down past the end is clamped.
        handle_key_event(&mut a, k(KeyCode::Down));
        assert_eq!(a.picker.as_ref().unwrap().cursor, 1);
        handle_key_event(&mut a, k(KeyCode::Up));
        assert_eq!(a.picker.as_ref().unwrap().cursor, 0);
    }

    // ── prompt dialog ───────────────────────────────────────────────────────

    fn question_prompt() -> Prompt {
        Prompt {
            title: "pick".into(),
            detail: None,
            options: vec!["a".into(), "b".into()],
            allow_free_text: false,
            kind: PromptKind::Question,
        }
    }
    fn permission_prompt() -> Prompt {
        Prompt {
            title: "allow?".into(),
            detail: Some("rm -rf x".into()),
            options: vec!["Allow once".into(), "Allow always".into(), "Deny".into()],
            allow_free_text: false,
            kind: PromptKind::Permission {
                suggested_glob: "rm *".into(),
            },
        }
    }

    #[test]
    fn prompt_enter_confirms_focused_option() {
        let mut a = app();
        a.prompt = Some(PromptUi::new(question_prompt()));
        handle_key_event(&mut a, k(KeyCode::Down)); // select option 1
        match handle_key_event(&mut a, k(KeyCode::Enter)) {
            KeyOutcome::SendReply(r) => {
                assert_eq!(r.index, Some(1));
                assert!(r.text.is_none());
            }
            o => panic!("expected SendReply, got {}", outcome_name(&o)),
        }
    }

    #[test]
    fn prompt_esc_denies_with_empty_reply() {
        let mut a = app();
        a.prompt = Some(PromptUi::new(question_prompt()));
        match handle_key_event(&mut a, k(KeyCode::Esc)) {
            KeyOutcome::SendReply(r) => assert!(r.index.is_none() && r.text.is_none()),
            o => panic!("expected SendReply, got {}", outcome_name(&o)),
        }
    }

    #[test]
    fn prompt_inline_glob_edit_carries_text() {
        let mut a = app();
        a.prompt = Some(PromptUi::new(permission_prompt()));
        handle_key_event(&mut a, k(KeyCode::Down)); // row 1 = Allow always (editable)
        handle_key_event(&mut a, ch('e')); // begin edit, seeds "rm *"
        assert!(a.prompt.as_ref().unwrap().editing);
        handle_key_event(&mut a, ch('!')); // "rm *!"
        match handle_key_event(&mut a, k(KeyCode::Enter)) {
            KeyOutcome::SendReply(r) => {
                assert_eq!(r.index, Some(1));
                assert_eq!(r.text.as_deref(), Some("rm *!"));
            }
            o => panic!("expected SendReply, got {}", outcome_name(&o)),
        }
    }

    // Small helper so panic messages name the unexpected outcome.
    fn outcome_name(o: &KeyOutcome) -> &'static str {
        match o {
            KeyOutcome::Continue => "Continue",
            KeyOutcome::Redraw => "Redraw",
            KeyOutcome::Quit => "Quit",
            KeyOutcome::CtrlC => "CtrlC",
            KeyOutcome::Cancel => "Cancel",
            KeyOutcome::SetThinking(_) => "SetThinking",
            KeyOutcome::SendReply(_) => "SendReply",
            KeyOutcome::Submit(_) => "Submit",
            KeyOutcome::Slash(_) => "Slash",
        }
    }
}

#[cfg(test)]
mod snapshots {
    //! Buffer snapshots of `App::render` at a fixed 80x24 `TestBackend`. Scenarios
    //! are built through the replay path (durations/clocks `None`) so no wall-clock
    //! time leaks into the golden buffers. See ratatui's insta+TestBackend recipe.
    use crate::tui::app::{App, ReplayEvent};
    use crate::tui::theme::PALETTES;
    use crate::tui::widgets::PromptUi;
    use crate::types::AgentEvent;
    use insta::assert_snapshot;
    use ratatui::{backend::TestBackend, Terminal};

    // A deterministic App: `App::new` otherwise seeds palette/oracle/plan/quota/
    // localize from the running machine's ~/.sirbone project config and env, which
    // would make these golden buffers machine-dependent. Pin every such field.
    fn base() -> App {
        let mut app = App::new("claude-opus".into(), "anthropic".into());
        app.quota_window = None; // window clocks are time-based
        app.palette_idx = 0;
        app.palette = &PALETTES[0].1;
        app.oracle = false;
        app.plan = false;
        app.quota_bar = true;
        app.localize = true;
        app.thinking_budget = None;
        app
    }

    fn render(app: &mut App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("test terminal");
        terminal.draw(|f| app.render(f)).expect("draw");
        format!("{}", terminal.backend())
    }

    #[test]
    fn idle_screen() {
        let mut app = base();
        assert_snapshot!(render(&mut app));
    }

    #[test]
    fn tool_box_and_todo_list() {
        let mut app = base();
        let todo_input = serde_json::json!({
            "todos": [
                {"content": "read the failing test", "status": "completed"},
                {"content": "patch the parser", "status": "in_progress"},
                {"content": "run the suite", "status": "pending"}
            ]
        });
        app.replay_events = vec![
            ReplayEvent::User("fix the parser bug".into()),
            ReplayEvent::Agent(AgentEvent::TextChunk("On it — planning first.".into())),
            ReplayEvent::Agent(AgentEvent::TurnEnd),
            ReplayEvent::Agent(AgentEvent::ToolCallStart {
                id: "t1".into(),
                name: "todo".into(),
                input: todo_input,
            }),
            ReplayEvent::Agent(AgentEvent::ToolCallEnd {
                id: "t1".into(),
                name: "todo".into(),
                result: "ok".into(),
                is_error: false,
            }),
            ReplayEvent::Agent(AgentEvent::ToolCallStart {
                id: "t2".into(),
                name: "bash".into(),
                input: serde_json::json!({"command": "cargo test"}),
            }),
            ReplayEvent::Agent(AgentEvent::ToolCallEnd {
                id: "t2".into(),
                name: "bash".into(),
                result: "test result: ok. 12 passed".into(),
                is_error: false,
            }),
        ];
        app.reset_durations_for_replay();
        app.replay();
        assert_snapshot!(render(&mut app));
    }

    #[test]
    fn prompt_dialog_open() {
        let mut app = base();
        app.prompt = Some(PromptUi::new(crate::agent::Prompt {
            title: "permission required".into(),
            detail: Some("rm -rf build/".into()),
            options: vec!["Allow once".into(), "Allow always".into(), "Deny".into()],
            allow_free_text: false,
            kind: crate::agent::PromptKind::Permission {
                suggested_glob: "rm *".into(),
            },
        }));
        assert_snapshot!(render(&mut app));
    }

    #[test]
    fn settings_screen() {
        let mut app = base();
        app.settings_mode = true;
        assert_snapshot!(render(&mut app));
    }
}
