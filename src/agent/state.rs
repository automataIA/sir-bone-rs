//! The agent state machine: `run()` plus its turn/tool-execution helpers, the
//! chit-chat short-circuit, and the read-only localization pre-pass.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use futures::stream::{self, StreamExt};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::instrument;

use super::policy::{decide, nudge_if_stuck};
use super::tokens::should_compact;
use super::{compact, estimate_context_tokens, AgentContext, COMPACTION_KEEP_RECENT_DEFAULT};
use crate::permissions::{self, Decision, PermissionConfig};
use crate::tools::ToolRegistry;
use crate::types::{
    extract_text, AgentEvent, AgentState, ContentBlock, Message, NoticeLevel, ToolCall,
};

/// Max times a `stop` hook may force the loop to continue, so a mis-written hook
/// (one that never exits 0) can't spin forever.
const STOP_HOOK_MAX: usize = 5;

#[instrument(skip_all, fields(model = %ctx.model))]
pub async fn run(ctx: &mut AgentContext) -> Result<()> {
    // Greeting / meta-question short-circuit: a pure chit-chat message is
    // answered in a single minimal turn (no tools, no full system prompt)
    // instead of spinning up the task workflow. Saves the whole prompt + tool
    // schemas + any spurious init/tool calls.
    if chit_chat_gate(ctx).await? {
        return Ok(());
    }
    // Reset the per-task architect consult counter.
    *crate::types::lock_or_recover(&ctx.tools.architect_calls) = 0;
    let max_steps = ctx.max_steps;
    let mut steps = 0usize;
    let mut stop_retries = 0usize;
    let mut state = AgentState::Idle;
    loop {
        // Count LLM turns (the Idle→run_turn transition); cap them when opted in.
        if matches!(state, AgentState::Idle) {
            steps += 1;
            if let Some(cap) = max_steps {
                if steps > cap {
                    ctx.events
                        .send(AgentEvent::Notice {
                            text: format!("Max steps reached ({cap}) — stopping"),
                            level: NoticeLevel::Info,
                        })
                        .await
                        .ok();
                    break;
                }
            }
            // Token spend cap (Feature C): stop cleanly before spending more.
            // A Notice (not an Error) — hitting a budget is not a failure.
            if let Some(cap) = ctx.spend_cap {
                if ctx.tokens_spent >= cap {
                    ctx.events
                        .send(AgentEvent::Notice {
                            text: format!(
                                "spend cap reached ({}/{} tokens) — raise the limit or reset",
                                ctx.tokens_spent, cap
                            ),
                            level: NoticeLevel::Info,
                        })
                        .await
                        .ok();
                    break;
                }
            }
        }
        state = match state {
            // Verification oracle gate: when enabled, a Done turn runs the
            // project's tests; a red result injects feedback and resumes the
            // loop (Self-Debug / AgentCoder, see `oracle` module).
            AgentState::Done => {
                // Oracle gate first: a red test result resumes the loop.
                let oracle_retry = match ctx.oracle.as_mut() {
                    None => None,
                    Some(oracle) => {
                        let snaps = ctx.snapshots.clone();
                        match oracle.gate(snaps.as_deref(), &ctx.events).await {
                            crate::oracle::Outcome::Done => None,
                            crate::oracle::Outcome::Retry(msg) => Some(msg),
                        }
                    }
                };
                match oracle_retry {
                    Some(msg) => {
                        ctx.messages.push(Message::user(msg));
                        AgentState::Idle
                    }
                    // Stop hooks (Feature B): exit 2 = "not done", force another
                    // iteration with the hook output as feedback. Capped so a
                    // mis-written hook can't loop forever.
                    None => match ctx.hooks.stop().await {
                        Some(reason) if stop_retries < STOP_HOOK_MAX => {
                            stop_retries += 1;
                            ctx.events
                                .send(AgentEvent::Notice {
                                    text: "stop hook requested another iteration".into(),
                                    level: NoticeLevel::Info,
                                })
                                .await
                                .ok();
                            ctx.messages.push(Message::user(reason));
                            AgentState::Idle
                        }
                        _ => break,
                    },
                }
            }
            AgentState::Idle => run_turn(ctx).await?,
            AgentState::ToolCalling(tcs) => {
                let next = run_tools(tcs, ctx).await?;
                nudge_if_stuck(&mut ctx.messages);
                next
            }
        };

        let context_window = ctx.context_window.unwrap_or(128_000);
        let tokens = estimate_context_tokens(&ctx.messages);
        let keep = ctx
            .compaction_keep_recent
            .unwrap_or(COMPACTION_KEEP_RECENT_DEFAULT)
            .min(ctx.messages.len());
        if should_compact(tokens, context_window, ctx.messages.len(), keep) {
            match compact(ctx).await {
                Ok(()) => {
                    // Check if compaction freed enough space
                    let tokens = estimate_context_tokens(&ctx.messages);
                    let keep = ctx
                        .compaction_keep_recent
                        .unwrap_or(COMPACTION_KEEP_RECENT_DEFAULT)
                        .min(ctx.messages.len());
                    if should_compact(tokens, context_window, ctx.messages.len(), keep) {
                        ctx.events
                            .send(AgentEvent::Error(
                                "Context window full after compaction — start a new session".into(),
                            ))
                            .await
                            .ok();
                        break;
                    }
                }
                Err(e) => {
                    // Non-fatal compaction errors (e.g. "too few messages") should
                    // not kill the session — the agent can still make progress.
                    let msg = format!("{e}");
                    if msg.contains("too few messages") {
                        // Context is small but token-heavy (large tool output).
                        // Can't compact yet — continue the loop.
                        ctx.events
                            .send(AgentEvent::Error(
                                "Context high but too few messages to compact — continuing".into(),
                            ))
                            .await
                            .ok();
                    } else {
                        ctx.events
                            .send(AgentEvent::Error(format!(
                                "Compaction failed ({e}) — start a new session"
                            )))
                            .await
                            .ok();
                        break;
                    }
                }
            }
        }
    }
    Ok(())
}

/// Greetings / identity questions that `is_chit_chat` treats as non-tasks.
/// Matched exactly (after lowercasing and stripping punctuation), so a short
/// technical prompt like "crea il file X" or "fix the parser" never matches.
const CHIT_CHAT: &[&str] = &[
    "ciao",
    "salve",
    "hey",
    "hi",
    "hello",
    "hola",
    "yo",
    "ehi",
    "ola",
    "buongiorno",
    "buonasera",
    "buonanotte",
    "grazie",
    "thanks",
    "thank you",
    "ty",
    "chi sei",
    "chi sei tu",
    "who are you",
    "what are you",
    "cosa sai fare",
    "cosa puoi fare",
    "che cosa fai",
    "che fai",
    "what can you do",
    "come stai",
    "come va",
    "how are you",
    "come ti chiami",
    "what is your name",
    "whats your name",
    "aiuto",
    "help",
];

/// Conservative greeting/meta detector — pure heuristic, no LLM call, so normal
/// prompts pay nothing. True only on an exact whitelist match, which a real task
/// prompt cannot hit; ambiguous input falls through to the normal workflow.
pub(crate) fn is_chit_chat(text: &str) -> bool {
    let lower = text.trim().to_lowercase();
    if lower.is_empty() || lower.len() > 64 {
        return false;
    }
    let norm = lower
        .split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()))
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    CHIT_CHAT.contains(&norm.as_str())
}

/// True when the current workspace has no agent instruction file (CLAUDE.md or
/// AGENTS.md) — the chit-chat gate uses it to offer initialization.
fn project_uninitialized() -> bool {
    match std::env::current_dir() {
        Ok(cwd) => !cwd.join("CLAUDE.md").exists() && !cwd.join("AGENTS.md").exists(),
        Err(_) => false,
    }
}

/// When the user's latest message is pure chit-chat, answer it in one turn with
/// a minimal system prompt and an empty tool registry, then restore the real
/// ones. Returns true when it handled the turn so `run()` returns early.
async fn chit_chat_gate(ctx: &mut AgentContext) -> Result<bool> {
    let Some(last) = ctx.messages.last() else {
        return Ok(false);
    };
    if !matches!(last.role, crate::types::Role::User) {
        return Ok(false);
    }
    if !is_chit_chat(&extract_text(&last.content)) {
        return Ok(false);
    }

    const MINI_SYSTEM: &str = "You are Sir Bone (\"sirbone\"), an AI coding agent that runs \
        shell commands, reads and edits files, and helps with software tasks. The user sent a \
        greeting or a question about you, not a task. Reply briefly and conversationally in the \
        user's language. Do not start any task, make a plan, or use any tool. If they want to \
        work on something, invite them to describe it.";

    let mut system = MINI_SYSTEM.to_string();
    if project_uninitialized() {
        system.push_str(
            " This workspace has no CLAUDE.md/AGENTS.md yet, so it is not initialized; if the \
             user wants to start working here, briefly offer to create a CLAUDE.md documenting \
             the project (purpose, layout, build/test commands).",
        );
    }

    // Swap in the minimal prompt + an empty registry for this one turn, then put
    // the real ones back — a greeting must not pay for the full prompt or tools.
    let saved_system = ctx.system_prompt.replace(system);
    let saved_tools = std::mem::replace(&mut ctx.tools, ToolRegistry::new());
    let result = run_turn(ctx).await;
    ctx.system_prompt = saved_system;
    ctx.tools = saved_tools;
    result?;
    Ok(true)
}

/// Bounded, read-only pre-pass. Runs up to `max_turns` with `read_only_tools`
/// under the given `system` prompt and returns the last assistant message
/// carrying non-empty text. Never edits (the registry is read-only); events are
/// drained silently. Best-effort — any error or empty result yields `None`.
/// Shared by `localize` (find *where*) and `plan` (decide *what*).
async fn read_only_prepass(
    client: Arc<dyn super::LlmClient>,
    model: &str,
    task: &str,
    read_only_tools: ToolRegistry,
    max_turns: usize,
    system: &str,
) -> Option<String> {
    // Silent: drain events into a dummy channel (like compaction).
    let (tx, mut rx) = mpsc::channel(64);
    tokio::spawn(async move { while rx.recv().await.is_some() {} });

    let mut ctx = AgentContext {
        model: model.to_string(),
        system_prompt: Some(system.to_string()),
        messages: vec![Message::user(task)],
        tools: read_only_tools,
        client,
        events: tx,
        cancel: CancellationToken::new(),
        context_window: None,
        confirm: None,
        compaction_keep_recent: None,
        permissions: PermissionConfig::default(),
        snapshots: None,
        hooks: Default::default(),
        oracle: None,
        max_steps: None,
        spend_cap: None,
        tokens_spent: 0,
    };

    let mut state = AgentState::Idle;
    for _ in 0..max_turns {
        state = match state {
            AgentState::Done => break,
            AgentState::Idle => run_turn(&mut ctx).await.ok()?,
            AgentState::ToolCalling(tcs) => run_tools(tcs, &mut ctx).await.ok()?,
        };
    }

    // Result = the last assistant message carrying non-empty text.
    ctx.messages
        .iter()
        .rev()
        .filter(|m| matches!(m.role, crate::types::Role::Assistant))
        .map(|m| extract_text(&m.content))
        .find(|t| !t.trim().is_empty())
}

/// Bounded, read-only localization pre-pass (Agentless stage-1). Finds *where*
/// a change must happen and returns a concise report. The caller seeds the
/// result into the working notes.
pub async fn localize(
    client: Arc<dyn super::LlmClient>,
    model: &str,
    task: &str,
    read_only_tools: ToolRegistry,
    max_turns: usize,
) -> Option<String> {
    const SYSTEM: &str = "You are a code-localization assistant. Given a task or bug \
        report, find WHERE in the codebase the change must happen. Do NOT edit anything — \
        investigate with the read-only tools (grep, glob, find, ls, read, code_map; \
        code_map gives the symbol index and call graph), then output a concise report \
        listing the exact file path(s), the relevant function/class, the approximate line \
        range, and a one-line reason for each. If unsure, give your best candidates.";
    read_only_prepass(client, model, task, read_only_tools, max_turns, SYSTEM).await
}

/// The working-note text that carries an approved plan into the main run. It
/// is re-injected every turn and survives compaction, so the plan stays the
/// contract for the whole session.
pub fn approved_plan_note(spec: &str) -> String {
    format!("APPROVED PLAN — follow this. If you must deviate, say why in your reply before acting:\n{spec}")
}

#[instrument(skip_all)]
pub(crate) async fn run_turn(ctx: &mut AgentContext) -> Result<AgentState> {
    ctx.events.send(AgentEvent::TurnStart).await.ok();

    // Persistent working notes: re-injected every turn as a (non-system, so the
    // cached system+tools prefix stays warm) user+ack pair right after the system
    // prompt. Lives in the tool registry, so it survives context compaction.
    let notes = ctx.tools.notes.get();
    let note_msgs: Vec<Message> = if notes.trim().is_empty() {
        Vec::new()
    } else {
        vec![
            Message::user(format!(
                "<working_notes>\n{notes}\n</working_notes>\n\
                 These are my persistent notes; I keep them current via the `note` tool."
            )),
            Message::assistant("Acknowledged — I'll use and update these notes."),
        ]
    };

    // System + notes are small owned locals; the transcript (`ctx.messages`) is
    // borrowed by reference — no per-turn deep clone of the whole history (the
    // biggest allocation in the steady-state loop on long sessions).
    let sys_msgs: Vec<Message> = ctx
        .system_prompt
        .iter()
        .map(|s| Message::system(s.as_str()))
        .collect();
    let messages: Vec<&Message> = sys_msgs
        .iter()
        .chain(note_msgs.iter())
        .chain(ctx.messages.iter())
        .collect();

    let result = ctx
        .client
        .run_turn(&messages, &ctx.tools, &ctx.events, &ctx.cancel)
        .await?;

    // Accumulate session token spend (Feature C). Prefer the provider's real
    // usage; fall back to the estimate when it reports none (local/odd endpoints).
    if ctx.spend_cap.is_some() {
        let turn = match result.usage.total() {
            0 => {
                estimate_context_tokens(&messages.iter().map(|m| (*m).clone()).collect::<Vec<_>>())
                    + estimate_context_tokens(std::slice::from_ref(&result.assistant_message))
            }
            n => n as usize,
        };
        ctx.tokens_spent += turn as u64;
        if let Some(cap) = ctx.spend_cap {
            ctx.events
                .send(AgentEvent::SpendUsage {
                    spent: ctx.tokens_spent,
                    cap,
                })
                .await
                .ok();
        }
    }

    ctx.messages.push(result.assistant_message);
    Ok(result.state)
}

#[instrument(skip_all, fields(n_tools = tool_calls.len()))]
async fn run_tools(tool_calls: Vec<ToolCall>, ctx: &mut AgentContext) -> Result<AgentState> {
    // Snapshot the live transcript so the `architect` tool (if called this turn)
    // forwards the full history to its reviewer model.
    *crate::types::lock_or_recover(&ctx.tools.transcript) = ctx.messages.clone();

    // Sequential permission pass: decide each tool call before any parallel
    // execution begins.
    let mut approved: Vec<ToolCall> = Vec::new();
    for mut tc in tool_calls {
        let inner = permissions::tool_inner(&tc.name, &tc.arguments);
        let (decision, rewrite) = decide(ctx, &tc.name, &tc.arguments).await;

        // updatedInput: apply a safer rewrite suggested by the classifier.
        if let Some(new_cmd) = rewrite {
            if let Some(obj) = tc.arguments.as_object_mut() {
                obj.insert("command".into(), serde_json::Value::String(new_cmd));
            }
        }

        // Steers the model after a block: pursue the goal another legitimate
        // way, but never relitigate the denial through a side door.
        const DENY_GUIDANCE: &str = "You may pursue the goal another reasonable \
            way that respects the intent behind this denial — do not work around \
            it (e.g. by doing the same thing through a different tool). If this \
            capability is essential, stop, explain what you were trying to do \
            and why, and let the user decide.";
        let blocked_reason = match decision {
            Decision::Allow => None,
            Decision::Deny(reason) => Some(format!("Command blocked: {reason}. {DENY_GUIDANCE}")),
            Decision::Ask => {
                let allowed = if let Some(bridge) = &mut ctx.confirm {
                    bridge.ask.send(inner.clone()).await.ok();
                    bridge.reply.recv().await.unwrap_or(false)
                } else {
                    false
                };
                (!allowed).then(|| format!("Command blocked (denied): {inner}. {DENY_GUIDANCE}"))
            }
        };

        if let Some(reason) = blocked_reason {
            ctx.events
                .send(AgentEvent::ToolCallStart {
                    id: tc.id.clone(),
                    name: tc.name.clone(),
                    input: tc.arguments.clone(),
                })
                .await
                .ok();
            ctx.events
                .send(AgentEvent::ToolCallEnd {
                    id: tc.id.clone(),
                    name: tc.name.clone(),
                    result: "blocked".into(),
                    is_error: true,
                })
                .await
                .ok();
            ctx.messages
                .push(Message::tool_result(&tc.id, reason, true));
            continue;
        }
        approved.push(tc);
    }

    if approved.is_empty() {
        return Ok(AgentState::Idle);
    }

    // Workspace snapshot (shadow git, once per run) before anything mutates,
    // so the whole run is rollback-able — including bash side effects that the
    // per-file undo tool can't see.
    if let Some(snaps) = &ctx.snapshots {
        if approved.iter().any(|tc| mutates_workspace(tc, &ctx.tools)) {
            let label = first_user_prompt(&ctx.messages);
            if let Some(id) = snaps.take_once_id(&label).await {
                ctx.events
                    .send(AgentEvent::WorkspaceSnapshot { id, label })
                    .await
                    .ok();
            }
        }
    }

    // Paths edited by this batch, for the post-edit checks below.
    let edited: Vec<String> = approved
        .iter()
        .filter_map(|tc| mutation_path(tc, &ctx.tools))
        .map(|p| p.to_string_lossy().into_owned())
        .collect();

    // Group calls into lanes: mutating calls hitting the same file share a lane
    // and run in order; everything else gets its own lane. Lanes run in
    // parallel, calls within a lane sequentially — so two edits/writes to the
    // same path can't race and silently drop one update.
    let lanes = plan_lanes(approved, &ctx.tools);

    let parallelism = std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .min(16);
    let tools = ctx.tools.clone();
    let events = ctx.events.clone();

    let results: Vec<Vec<Message>> = stream::iter(lanes)
        .map(|lane| {
            let tools = tools.clone();
            let events = events.clone();
            async move {
                let mut out = Vec::with_capacity(lane.len());
                for tc in lane {
                    events
                        .send(AgentEvent::ToolCallStart {
                            id: tc.id.clone(),
                            name: tc.name.clone(),
                            input: tc.arguments.clone(),
                        })
                        .await
                        .ok();
                    let exec_result = tools.execute(&tc.name, tc.arguments.clone()).await;
                    let (content, is_error) = match exec_result {
                        Ok(s) => (s, false),
                        Err(e) => (format!("Error: {e}"), true),
                    };
                    events
                        .send(AgentEvent::ToolCallEnd {
                            id: tc.id.clone(),
                            name: tc.name.clone(),
                            result: content.clone(),
                            is_error,
                        })
                        .await
                        .ok();
                    out.push(Message::tool_result(&tc.id, content, is_error));
                }
                out
            }
        })
        .buffer_unordered(parallelism)
        .collect()
        .await;

    ctx.messages.extend(results.into_iter().flatten());

    // Post-edit checks: lint/typecheck the batch and ride failures into the
    // last tool result (same inline mechanism as the stuck-loop nudge), so the
    // model fixes breakage in this turn instead of discovering it edits later.
    if !edited.is_empty() && !ctx.hooks.post.is_empty() {
        if let Some(report) = ctx.hooks.post.run(&edited).await {
            if let Some(ContentBlock::ToolResult { content, .. }) =
                ctx.messages.last_mut().and_then(|m| m.content.last_mut())
            {
                content.push_str(&report);
            }
        }
    }
    Ok(AgentState::Idle)
}

/// True if the call can change the workspace: a file-mutating tool, or a bash
/// command that isn't known read-only (background jobs included).
fn mutates_workspace(tc: &ToolCall, tools: &ToolRegistry) -> bool {
    mutation_path(tc, tools).is_some()
        || (tc.name == "bash"
            && !permissions::is_safe_readonly(
                tc.arguments
                    .get("command")
                    .and_then(|v| v.as_str())
                    .unwrap_or(""),
            ))
}

/// First user message of the transcript — the snapshot label.
fn first_user_prompt(messages: &[Message]) -> String {
    messages
        .iter()
        .find(|m| matches!(m.role, crate::types::Role::User))
        .map(|m| extract_text(&m.content))
        .unwrap_or_default()
}

/// Resolved target path of a mutating call (canonicalized for lane grouping), or
/// `None` if the call doesn't mutate a file. Asks the tool via
/// [`ToolRegistry::mutation_target`]; falls back to the raw path when the file
/// doesn't exist yet.
fn mutation_path(tc: &ToolCall, tools: &ToolRegistry) -> Option<PathBuf> {
    let raw = tools.mutation_target(&tc.name, &tc.arguments)?;
    Some(std::fs::canonicalize(&raw).unwrap_or(raw))
}

/// Partition approved calls into execution lanes. Mutating calls on the same
/// resolved path land in one lane (serial); all others get a private lane.
pub(crate) fn plan_lanes(approved: Vec<ToolCall>, tools: &ToolRegistry) -> Vec<Vec<ToolCall>> {
    let mut lanes: Vec<Vec<ToolCall>> = Vec::new();
    let mut by_path: HashMap<PathBuf, usize> = HashMap::new();
    for tc in approved {
        match mutation_path(&tc, tools) {
            Some(path) => {
                let idx = *by_path.entry(path).or_insert_with(|| {
                    lanes.push(Vec::new());
                    lanes.len() - 1
                });
                lanes[idx].push(tc);
            }
            None => lanes.push(vec![tc]),
        }
    }
    lanes
}
