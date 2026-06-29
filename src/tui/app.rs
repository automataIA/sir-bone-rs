use std::{
    collections::VecDeque,
    sync::Arc,
    time::{Duration, Instant},
};

use ratatui::{
    layout::Rect,
    text::{Line, Span},
};

use crate::{
    skills::SkillMeta,
    types::{AgentEvent, Message},
};

use super::{
    boar::{load_boar, BoarAnim},
    diff::edit_diff_block,
    markdown::md_to_lines,
    theme::{styled, Palette, PALETTES},
    widgets::{
        fmt_elapsed, out_preview_rows, thread_blank, thread_wrap, tool_box_row, tool_box_top,
        user_box, THREAD_GUTTER, TIMELINE_W,
    },
};

pub(super) fn format_tool_input(input: &serde_json::Value) -> String {
    for key in &["command", "path", "file_path", "pattern", "query"] {
        if let Some(s) = input.get(key).and_then(|v| v.as_str()) {
            return s.to_string();
        }
    }
    input.to_string()
}

// ── app ───────────────────────────────────────────────────────────────────────

#[derive(PartialEq)]
pub(super) enum Focus {
    Input,
    Output,
}

pub(super) enum AgentStatus {
    Idle,
    LlmThinking,
    ToolRunning(String),
}

// ── completion (@ files, / commands) ─────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
pub(super) enum CompletionMode {
    File,
    Slash,
}

pub(super) struct Completion {
    pub(super) active: bool,
    pub(super) mode: CompletionMode,
    pub(super) trigger_pos: usize,
    pub(super) query: String,
    pub(super) items: Vec<String>,     // filtered display strings
    pub(super) all_items: Vec<String>, // cached candidates
    pub(super) selected: usize,
}

impl Completion {
    fn new() -> Self {
        Self {
            active: false,
            mode: CompletionMode::File,
            trigger_pos: 0,
            query: String::new(),
            items: Vec::new(),
            all_items: Vec::new(),
            selected: 0,
        }
    }

    pub(super) fn activate_files(&mut self, trigger_pos: usize) {
        self.active = true;
        self.mode = CompletionMode::File;
        self.trigger_pos = trigger_pos;
        self.query.clear();
        self.selected = 0;
        self.all_items = scan_project_files();
        self.items = self.all_items.clone();
    }

    pub(super) fn activate_slash(&mut self, commands: &[String]) {
        self.active = true;
        self.mode = CompletionMode::Slash;
        self.trigger_pos = 0; // always at start
        self.query.clear();
        self.selected = 0;
        self.all_items = commands.to_vec();
        self.items = self.all_items.clone();
    }

    pub(super) fn deactivate(&mut self) {
        self.active = false;
        self.query.clear();
        self.items.clear();
        self.all_items.clear();
    }

    pub(super) fn filter(&mut self) {
        let q = self.query.to_lowercase();
        self.items = if q.is_empty() {
            self.all_items.clone()
        } else {
            self.all_items
                .iter()
                .filter(|p| p.to_lowercase().contains(&q))
                .cloned()
                .collect()
        };
        if self.selected >= self.items.len() {
            self.selected = self.items.len().saturating_sub(1);
        }
    }

    pub(super) fn accept(&mut self) -> Option<String> {
        let item = self.items.get(self.selected).cloned();
        self.deactivate();
        item
    }

    pub(super) fn popup_title(&self) -> &'static str {
        match self.mode {
            CompletionMode::File => " @ files ",
            CompletionMode::Slash => " / commands ",
        }
    }
}

fn scan_project_files() -> Vec<String> {
    // Cached per process: the walk is a blocking recursive `read_dir` that used
    // to re-run on every `@` trigger (blocking the render loop). Scanning once
    // and cloning is far cheaper; a stale list is acceptable for completion
    // (the model can still read/touch new files via tools).
    static CACHE: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    fn walk(dir: &std::path::Path, prefix: &str, out: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') || name == "target" || name == "node_modules" {
                continue;
            }
            let path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                walk(&entry.path(), &path, out);
            } else {
                out.push(path);
            }
        }
    }
    CACHE
        .get_or_init(|| {
            let mut files = Vec::new();
            walk(std::path::Path::new("."), "", &mut files);
            files
        })
        .clone()
}

// ── tool box popup ────────────────────────────────────────────────────────────

pub(super) struct ToolBox {
    pub(super) line_start: usize, // index of ╭─ line in self.lines
    pub(super) line_end: usize,   // index of ╰─ line in self.lines
    pub(super) name: String,
    pub(super) full_input: String,  // untruncated command/path
    pub(super) full_output: String, // untruncated result
}

pub(super) struct Popup {
    pub(super) content: PopupContent,
    pub(super) scroll: u16,
}

pub(super) enum PopupContent {
    Tool { tool_idx: usize },
    Text { title: String, lines: Vec<String> },
}

/// What a Settings picker is choosing: skills (`k`) or MCP servers (`m`). Decides
/// which per-project allowlist a toggle writes (`skills.enabled` / `mcp.enabled`).
#[derive(Clone, Copy, PartialEq)]
pub(super) enum PickerKind {
    Skills,
    Mcp,
}

/// One selectable line in the Settings screen, in render order. Arrow keys move a
/// cursor over these; ←/→/space/enter act on the focused row (toggle, cycle, or
/// open a sub-picker). Letter shortcuts still work too.
#[derive(Clone, Copy, PartialEq)]
pub(super) enum SettingsRow {
    Localize,
    Plan,
    Oracle,
    QuotaBar,
    Architect,
    Thinking,
    Skills,
    Mcp,
}

pub(super) const SETTINGS_ROWS: &[SettingsRow] = &[
    SettingsRow::Localize,
    SettingsRow::Plan,
    SettingsRow::Oracle,
    SettingsRow::QuotaBar,
    SettingsRow::Architect,
    SettingsRow::Thinking,
    SettingsRow::Skills,
    SettingsRow::Mcp,
];

/// One row in a Settings picker: an available item and whether the current
/// project has opted it in. `tag` is a precomputed marker (e.g. `(local)`,
/// `(always)`, `(trust)`) shown after the name.
pub(super) struct PickRow {
    pub(super) name: String,
    pub(super) description: String,
    pub(super) tag: String,
    pub(super) enabled: bool,
}

/// A Settings picker overlay: the full item list with a cursor. Open while
/// `Some`. Toggling a row rewrites the project's allowlist and takes effect on
/// the next launch (the catalog/tools are built once at startup).
pub(super) struct Picker {
    pub(super) kind: PickerKind,
    pub(super) rows: Vec<PickRow>,
    pub(super) cursor: usize,
}

/// One row in the side timeline panel: a user message or a tool call, with a
/// `target` index into `self.lines` so a click scrolls the chat to that step.
pub(super) struct TimelineEntry {
    pub(super) label: String, // tool name, or the user message snippet
    pub(super) clock: String, // "09:24" (empty for session-resumed history)
    pub(super) target: usize, // index into self.lines to scroll to on click
    pub(super) is_user: bool,
}

// ── replay events ─────────────────────────────────────────────────────────────

#[derive(Clone)]
pub(super) enum ReplayEvent {
    User(String),
    Agent(AgentEvent),
}

/// Rebuild the renderable event stream from a loaded conversation so the TUI can
/// repaint prior history (used at startup with `--session` and by `/resume`).
/// `ToolResult` carries only `tool_use_id`, so names are mapped back through the
/// preceding `ToolUse`. `Thinking`/`Image` blocks are dropped.
pub(super) fn messages_to_replay(messages: &[Message]) -> Vec<ReplayEvent> {
    use crate::types::{ContentBlock, Role};
    let mut out = Vec::new();
    let mut tool_names: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for msg in messages {
        match msg.role {
            Role::User => {
                let mut user_text = String::new();
                for b in &msg.content {
                    match b {
                        ContentBlock::Text { text } => {
                            if !user_text.is_empty() {
                                user_text.push('\n');
                            }
                            user_text.push_str(text);
                        }
                        // Tool results ride on user-role messages in the API format —
                        // render them as the tool's output box, not a user bubble.
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                        } => {
                            let name = tool_names.get(tool_use_id).cloned().unwrap_or_default();
                            out.push(ReplayEvent::Agent(AgentEvent::ToolCallEnd {
                                id: tool_use_id.clone(),
                                name,
                                result: content.clone(),
                                is_error: *is_error,
                            }));
                        }
                        _ => {}
                    }
                }
                if !user_text.trim().is_empty() {
                    out.push(ReplayEvent::User(user_text));
                }
            }
            Role::Assistant => {
                for b in &msg.content {
                    match b {
                        ContentBlock::Text { text } => {
                            out.push(ReplayEvent::Agent(AgentEvent::TextChunk(text.clone())));
                        }
                        ContentBlock::ToolUse { id, name, input } => {
                            tool_names.insert(id.clone(), name.clone());
                            out.push(ReplayEvent::Agent(AgentEvent::ToolCallStart {
                                id: id.clone(),
                                name: name.clone(),
                                input: input.clone(),
                            }));
                        }
                        _ => {}
                    }
                }
                out.push(ReplayEvent::Agent(AgentEvent::TurnEnd)); // flush buffered text
            }
            _ => {}
        }
    }
    out
}

pub(super) struct App {
    pub(super) lines: Vec<Line<'static>>,
    pub(super) pending: String,
    // Typewriter reveal: how many chars of `pending` are shown. Advances each
    // frame via tick_reveal() so network bursts surface smoothly.
    pub(super) revealed: usize,
    // Rate-paced reveal state: when the current block's text started arriving,
    // the last tick instant, and a fractional-char accumulator. Together they
    // pace `revealed` at the provider's average char/sec instead of dumping
    // each burst (see tick_reveal).
    pub(super) reveal_anchor: Option<Instant>,
    pub(super) reveal_clock: Option<Instant>,
    pub(super) reveal_carry: f64,
    // Low-passed reveal rate (chars/sec) for a near-constant on-screen velocity.
    pub(super) reveal_rate: f64,
    // Set once the turn's text stream has ended so tick_reveal releases the
    // jitter-buffer reserve and drains the tail at the established rate (instead
    // of flush_pending dumping it in one go).
    pub(super) reveal_drain: bool,
    // Segment-boundary events (ToolCallStart, TurnEnd, …) that arrived while the
    // typewriter still trailed the text. Parked here instead of dumping the tail
    // via flush_pending, and replayed in FIFO order by drain_deferred() once
    // `revealing()` catches up — keeps the cursor finishing the sentence before a
    // tool box drops in. Replay (process_replay_event) bypasses this entirely.
    pub(super) deferred: VecDeque<AgentEvent>,
    // Cache of the parsed in-flight tail, keyed by (revealed, width); skips the
    // per-frame md re-parse when neither changed (waiting on the network).
    pub(super) tail_cache: Option<(usize, usize, Vec<Line<'static>>)>,
    pub(super) input: String,
    pub(super) cursor_pos: usize,
    pub(super) queued_input: Option<String>,
    pub(super) scroll: u16,
    pub(super) auto_scroll: bool,
    pub(super) max_scroll: u16,
    pub(super) busy: bool,
    pub(super) focus: Focus,
    pub(super) status: AgentStatus,
    pub(super) spinner_tick: u64,
    pub(super) boar: BoarAnim,      // about-screen animation only
    pub(super) panel_visible: bool, // side timeline panel (⌥B)
    pub(super) about_mode: bool,
    pub(super) settings_mode: bool,
    pub(super) settings_cursor: usize, // focused row in the Settings screen (arrow-key nav)
    pub(super) picker: Option<Picker>, // Settings `k`/`m`: per-project skill/MCP allowlist
    pub(super) model: String,
    pub(super) provider: String,
    pub(super) ctx_pct: u8,
    pub(super) ctx_warned: bool, // one-shot 70% context-rot warning; rearms below threshold
    pub(super) cached_pct: u8,   // share of the prompt served from cache last request
    // Token spend cap (Feature C): cumulative spend + cap, shown as `tok N/M`
    // in the info bar. `None` cap = the cap is disabled, so nothing is drawn.
    pub(super) spend_tokens: u64,
    pub(super) spend_cap: Option<u64>,
    // (tool_use_id, name, input, monotonic start, wall-clock start "HH:MM").
    // Paired Start→End by id so parallel same-name calls don't cross-couple.
    pub(super) pending_tool_inputs: Vec<(String, String, serde_json::Value, Instant, String)>,
    // Wall time of each completed tool call, in ToolCallEnd order. Survives
    // replay() (palette/boar re-wrap); None for session-resumed history.
    pub(super) tool_durations: Vec<Option<Duration>>,
    // Wall-clock start ("HH:MM") of each completed call, parallel to
    // `tool_durations`; None for session-resumed history (no timing).
    pub(super) tool_start_clocks: Vec<Option<String>>,
    pub(super) replaying: bool,
    pub(super) replay_tool_idx: usize,
    pub(super) busy_since: Option<Instant>,
    pub(super) thinking_tail: String, // last ~200 chars of streamed thinking, for the status bar
    pub(super) history: Vec<String>,  // submitted inputs, ↑/↓ recall
    pub(super) history_idx: Option<usize>, // None = editing a fresh line
    pub(super) history_stash: String, // draft saved while browsing history
    pub(super) palette: &'static Palette,
    pub(super) palette_idx: usize,
    pub(super) last_models: Vec<String>, // most recent /model listing, for index selection
    pub(super) completion: Completion,
    pub(super) skills: Vec<SkillMeta>,
    pub(super) slash_names: Vec<String>, // built-in + skill names for autocomplete
    pub(super) replay_events: Vec<ReplayEvent>,
    pub(super) tool_boxes: Vec<ToolBox>,
    pub(super) timeline: Vec<TimelineEntry>, // side-panel rows: user msgs + tool calls
    pub(super) timeline_area: Rect,          // panel rect, for click hit-testing
    pub(super) timeline_rows: Vec<usize>,    // entry index per rendered panel row (hit-testing)
    pub(super) panel_w: u16,                 // side panel width, drag-resizable
    pub(super) resizing_panel: bool,         // a divider drag is in progress
    pub(super) trail_scroll: u16,            // lines scrolled up from the bottom of the trail
    pub(super) trail_max_scroll: u16,        // clamp for trail_scroll, set each render
    pub(super) hover_divider: bool,          // pointer is over the resize divider
    pub(super) scroll_to: Option<u16>,       // trail-click jump: scroll the chat toward this line
    pub(super) selected_entry: Option<usize>, // trail entry highlighted by a click (cleared on manual scroll)
    pub(super) popup: Option<Popup>,
    pub(super) chat_area: Rect,
    pub(super) thread_active: bool, // rail open: agent output nests under current user msg
    pub(super) confirm_request: Option<String>, // pending approval (y/n dialog) for a destructive command
    pub(super) jobs_running: Vec<(u32, String, Duration, Option<f32>)>, // background jobs, polled each frame
    pub(super) localize: bool, // run the localization pre-pass before each turn (Settings, `l`)
    pub(super) plan: bool, // `/plan`: prefix a directive so the model records a SPEC via the `plan` tool
    pub(super) oracle: bool, // `/oracle`: post-Done test gate (loop + rollback)
    pub(super) architect: Option<Arc<std::sync::atomic::AtomicBool>>, // Some if configured; Settings `a`
    pub(super) thinking_budget: Option<u32>, // mirrors the client's budget for display; Settings `t`
    // Active account-wide 5-hour quota window (persisted under ~/.sirbone),
    // shown in the info bar. None until the first prompt or once it has lapsed.
    pub(super) quota_window: Option<crate::quota::Window>,
    pub(super) quota_bar: bool, // show the quota window in the info bar (Settings)
    pub(super) show_logs: bool, // F12: overlay the tui-logger debug panel
}

const BUILTIN_SLASH: &[(&str, &str)] = &[
    ("help", "Show available commands"),
    ("login", "Seed the global ~/.sirbone/.env (configure credentials once)"),
    ("clear", "Clear conversation (start a new one)"),
    (
        "resume",
        "Resume a saved conversation (/resume, /resume <n>)",
    ),
    ("compact", "Summarize conversation to free tokens"),
    ("init", "Build the code map and write AGENTS.md"),
    (
        "tokens",
        "Count context tokens (system + tools + conversation)",
    ),
    ("jobs", "List background jobs (state, elapsed, log tail)"),
    (
        "rollback",
        "Restore a workspace snapshot (/rollback lists, /rollback <n|id>)",
    ),
    ("snapshots", "List workspace snapshots with changed files"),
    (
        "model",
        "Switch model (/model lists, /model <n|name> selects)",
    ),
    (
        "plan",
        "Toggle plan mode (model records a SPEC via the plan tool before editing)",
    ),
    ("oracle", "Toggle the post-Done test gate (loop + rollback)"),
    (
        "verify",
        "Run the project's test command now and show the result",
    ),
    ("quit", "Exit sirbone"),
];

/// Process cwd, falling back to `.` if it can't be determined.
pub(super) fn cwd() -> std::path::PathBuf {
    std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
}

impl App {
    /// Byte offset in `input` of the char cursor (`input.len()` when past end).
    pub(super) fn byte_at_cursor(&self) -> usize {
        self.input
            .char_indices()
            .nth(self.cursor_pos)
            .map(|(i, _)| i)
            .unwrap_or(self.input.len())
    }

    pub(super) fn new(model: String, provider: String) -> Self {
        let boar = load_boar().unwrap_or_else(|| BoarAnim::new_mock(60, 20));
        let skills = crate::skills::scan_skills();
        // Restore the palette chosen for this project, if any (default: index 0).
        let cwd = cwd();
        let meta = crate::project_store::load_meta(&cwd);
        let palette_idx = meta
            .palette
            .as_deref()
            .and_then(|name| PALETTES.iter().position(|(n, _)| *n == name))
            .unwrap_or(0);
        let palette = &PALETTES[palette_idx].1;
        let mut slash_names: Vec<String> = BUILTIN_SLASH
            .iter()
            .map(|(name, desc)| format!("{name}  — {desc}"))
            .collect();
        for s in &skills {
            let desc = if s.description.is_empty() {
                ""
            } else {
                &s.description
            };
            slash_names.push(format!("{}  — {desc}", s.name));
        }
        Self {
            lines: Vec::new(),
            pending: String::new(),
            revealed: 0,
            tail_cache: None,
            reveal_anchor: None,
            reveal_clock: None,
            reveal_carry: 0.0,
            reveal_rate: 0.0,
            reveal_drain: false,
            deferred: VecDeque::new(),
            input: String::new(),
            cursor_pos: 0,
            queued_input: None,
            scroll: 0,
            auto_scroll: true,
            max_scroll: 0,
            busy: false,
            focus: Focus::Input,
            status: AgentStatus::Idle,
            spinner_tick: 0,
            boar,
            panel_visible: true,
            about_mode: false,
            settings_mode: false,
            settings_cursor: 0,
            picker: None,
            model,
            provider,
            ctx_pct: 0,
            ctx_warned: false,
            cached_pct: 0,
            spend_tokens: 0,
            spend_cap: crate::config::spend_cap(),
            pending_tool_inputs: Vec::new(),
            tool_durations: Vec::new(),
            tool_start_clocks: Vec::new(),
            replaying: false,
            replay_tool_idx: 0,
            busy_since: None,
            thinking_tail: String::new(),
            history: Vec::new(),
            history_idx: None,
            history_stash: String::new(),
            palette,
            palette_idx,
            quota_window: crate::quota::current(),
            quota_bar: meta.quota_bar.unwrap_or(true),
            last_models: Vec::new(),
            completion: Completion::new(),
            skills,
            slash_names,
            replay_events: Vec::new(),
            tool_boxes: Vec::new(),
            timeline: Vec::new(),
            timeline_area: Rect::default(),
            timeline_rows: Vec::new(),
            panel_w: TIMELINE_W,
            resizing_panel: false,
            trail_scroll: 0,
            trail_max_scroll: 0,
            hover_divider: false,
            scroll_to: None,
            selected_entry: None,
            popup: None,
            chat_area: Rect::default(),
            thread_active: false,
            confirm_request: None,
            jobs_running: Vec::new(),
            localize: std::env::var_os("SIRBONE_NO_LOCALIZE").is_none(),
            plan: meta.plan.unwrap_or(false),
            oracle: meta
                .oracle
                .unwrap_or_else(|| crate::oracle::Oracle::load().is_some()),
            architect: None,
            thinking_budget: None,
            show_logs: false,
        }
    }

    /// Persist the current palette choice for this project (best-effort).
    pub(super) fn save_palette(&self) {
        let cwd = cwd();
        let mut meta = crate::project_store::load_meta(&cwd);
        meta.palette = Some(PALETTES[self.palette_idx].0.to_string());
        let _ = crate::project_store::save_meta(&cwd, &mut meta);
    }

    /// Persist the project-level Settings toggles (oracle/plan/quota bar) so they
    /// survive a TUI restart, mirroring [`save_palette`]. Best-effort.
    pub(super) fn save_settings(&self) {
        let cwd = cwd();
        let mut meta = crate::project_store::load_meta(&cwd);
        meta.oracle = Some(self.oracle);
        meta.plan = Some(self.plan);
        meta.quota_bar = Some(self.quota_bar);
        let _ = crate::project_store::save_meta(&cwd, &mut meta);
    }

    /// Open a Settings picker for `kind`, marking each item enabled if the current
    /// project's allowlist (`skills.enabled` / `mcp.enabled`) names it. Default-off
    /// — a fresh project shows everything unchecked. Rows sorted by name.
    pub(super) fn open_picker(&mut self, kind: PickerKind) {
        let rows: Vec<PickRow> = match kind {
            PickerKind::Skills => {
                let enabled = crate::config::skills_enabled();
                let mut rows: Vec<PickRow> = crate::skills::scan_all_skills()
                    .into_iter()
                    .map(|s| {
                        let local = s.scope == crate::skills::Scope::Local;
                        let tag = match (local, s.always) {
                            (true, true) => "  (local · always)",
                            (true, false) => "  (local)",
                            (false, true) => "  (always)",
                            (false, false) => "",
                        }
                        .to_string();
                        PickRow {
                            enabled: enabled.iter().any(|e| e == &s.name),
                            name: s.name,
                            description: s.description,
                            tag,
                        }
                    })
                    .collect();
                rows.sort_by(|a, b| a.name.cmp(&b.name));
                rows
            }
            PickerKind::Mcp => {
                let enabled = crate::config::mcp_enabled();
                let mut cat: Vec<_> = crate::mcp::read_catalog().into_iter().collect();
                cat.sort_by(|a, b| a.0.cmp(&b.0));
                cat.into_iter()
                    .map(|(name, cfg)| PickRow {
                        description: format!("{} {}", cfg.command, cfg.args.join(" "))
                            .trim_end()
                            .to_string(),
                        tag: if cfg.trust {
                            "  (trust)".to_string()
                        } else {
                            String::new()
                        },
                        enabled: enabled.iter().any(|e| e == &name),
                        name,
                    })
                    .collect()
            }
        };
        self.picker = Some(Picker {
            kind,
            rows,
            cursor: 0,
        });
    }

    /// Flip the highlighted item's enabled state and persist the resulting
    /// allowlist to the per-project config. Effect next launch.
    pub(super) fn toggle_picked(&mut self) {
        let Some(picker) = &mut self.picker else {
            return;
        };
        let Some(row) = picker.rows.get_mut(picker.cursor) else {
            return;
        };
        row.enabled = !row.enabled;
        let enabled: Vec<String> = picker
            .rows
            .iter()
            .filter(|r| r.enabled)
            .map(|r| r.name.clone())
            .collect();
        let _ = match picker.kind {
            PickerKind::Skills => crate::config::set_skills_enabled(&enabled),
            PickerKind::Mcp => crate::config::set_mcp_enabled(&enabled),
        };
    }

    /// Clear all conversation state from the UI (chat lines, streaming buffer,
    /// replay history, tool boxes, popup). The conversation history (`messages`)
    /// and session file live in `run_loop` and are reset there.
    pub(super) fn reset(&mut self) {
        self.lines.clear();
        self.pending.clear();
        self.replay_events.clear();
        self.tool_boxes.clear();
        self.timeline.clear();
        self.pending_tool_inputs.clear();
        self.tool_durations.clear();
        self.tool_start_clocks.clear();
        self.replay_tool_idx = 0;
        self.popup = None;
        self.scroll = 0;
        self.auto_scroll = true;
        self.thread_active = false;
    }

    /// Register a real prompt send against the persisted account-wide quota
    /// counter, opening a fresh 5-hour window when the previous one has lapsed.
    pub(super) fn touch_quota_window(&mut self) {
        self.quota_window = Some(crate::quota::touch());
    }

    /// `(start, end)` clock labels "HH:MM" for the active quota window, or None
    /// when none is open (never used, or the 5 hours have already elapsed).
    pub(super) fn quota_window_clocks(&self) -> Option<(String, String)> {
        if !self.quota_bar {
            return None;
        }
        let w = self.quota_window?;
        if chrono::Local::now() >= w.end {
            return None; // window elapsed → nothing active to show
        }
        Some((
            w.start.format("%H:%M").to_string(),
            w.end.format("%H:%M").to_string(),
        ))
    }

    pub(super) fn push_history(&mut self, text: &str) {
        if self.history.last().map(String::as_str) != Some(text) {
            self.history.push(text.to_string());
        }
        self.history_idx = None;
    }

    /// ↑ — recall the previous submitted input, stashing the in-progress draft.
    pub(super) fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        match self.history_idx {
            None => {
                self.history_stash = std::mem::take(&mut self.input);
                self.history_idx = Some(self.history.len() - 1);
            }
            Some(0) => {}
            Some(i) => self.history_idx = Some(i - 1),
        }
        if let Some(i) = self.history_idx {
            self.input = self.history[i].clone();
            self.cursor_pos = self.input.chars().count();
        }
    }

    /// ↓ — move toward the present; past the newest entry restores the draft.
    pub(super) fn history_next(&mut self) {
        let Some(i) = self.history_idx else { return };
        if i + 1 < self.history.len() {
            self.history_idx = Some(i + 1);
            self.input = self.history[i + 1].clone();
        } else {
            self.history_idx = None;
            self.input = std::mem::take(&mut self.history_stash);
        }
        self.cursor_pos = self.input.chars().count();
    }

    /// Reset duration bookkeeping after `replay_events` was rebuilt from a
    /// session file: history has no timing, so every completed call gets None.
    pub(super) fn reset_durations_for_replay(&mut self) {
        self.tool_durations = self
            .replay_events
            .iter()
            .filter(|e| matches!(e, ReplayEvent::Agent(AgentEvent::ToolCallEnd { .. })))
            .map(|_| None)
            .collect();
        self.tool_start_clocks = vec![None; self.tool_durations.len()];
    }

    /// Execute a built-in slash command. Returns true if handled.
    /// Push an informational line (or multi-line block) to the scrollback.
    pub(super) fn info_line(&mut self, msg: impl Into<String>) {
        let p = self.palette;
        self.lines.push(Line::default());
        for l in msg.into().lines() {
            self.lines.push(Line::from(Span::styled(
                format!("  {l}"),
                styled(p.accent, false),
            )));
        }
    }

    pub(super) fn exec_slash(&mut self, cmd: &str) -> bool {
        let (name, _args) = cmd.split_once(' ').unwrap_or((cmd, ""));
        let p = self.palette;
        match name {
            "help" => {
                self.lines.push(Line::default());
                self.lines.push(Line::from(Span::styled(
                    "  Commands:",
                    styled(p.accent, true),
                )));
                for (n, d) in BUILTIN_SLASH {
                    self.lines.push(Line::from(vec![
                        Span::styled(format!("    /{n}"), styled(p.fg, false)),
                        Span::styled(format!("  — {d}"), styled(p.muted, false)),
                    ]));
                }
                if !self.skills.is_empty() {
                    self.lines.push(Line::from(Span::styled(
                        "  Skills:",
                        styled(p.accent, true),
                    )));
                    for s in &self.skills {
                        let scope = match s.scope {
                            crate::skills::Scope::Global => "global",
                            crate::skills::Scope::Local => "local",
                        };
                        self.lines.push(Line::from(vec![
                            Span::styled(format!("    /{}", s.name), styled(p.fg, false)),
                            Span::styled(
                                format!("  — {} [{scope}]", s.description),
                                styled(p.muted, false),
                            ),
                        ]));
                    }
                }
                self.lines.push(Line::default());
                true
            }
            "login" => {
                match crate::config::ensure_global_env() {
                    Ok(info) => {
                        let head = if info.created {
                            format!("  created {} (chmod 600)", info.path.display())
                        } else {
                            format!("  {} already exists", info.path.display())
                        };
                        self.lines.push(Line::from(Span::styled(head, styled(p.accent, true))));
                        self.lines.push(Line::from(Span::styled(
                            "  fill ONE provider, then restart sirbone:",
                            styled(p.muted, false),
                        )));
                        for l in crate::config::ENV_TEMPLATE.lines() {
                            self.lines.push(Line::from(Span::styled(
                                format!("    {l}"),
                                styled(p.muted, false),
                            )));
                        }
                    }
                    Err(e) => self.lines.push(Line::from(Span::styled(
                        format!("  login failed: {e}"),
                        styled(p.err, false),
                    ))),
                }
                true
            }
            "clear" => {
                // UI-only fallback; run_loop intercepts /clear to also reset
                // `messages` and rotate the session file.
                self.reset();
                true
            }
            "model" => {
                self.lines.push(Line::from(Span::styled(
                    format!("  {} / {}", self.provider, self.model),
                    styled(p.accent, false),
                )));
                true
            }
            "init" => {
                let cwd = cwd();
                let idx = crate::structure::update(&cwd, crate::structure::Index::load(&cwd));
                let _ = idx.save(&cwd);
                let edges = crate::structure::graph_cached(&cwd, &idx);
                self.lines.push(Line::from(Span::styled(
                    format!(
                        "  built code map: {} files, {} edges",
                        idx.files.len(),
                        edges.len()
                    ),
                    styled(p.accent, false),
                )));
                if let Ok(crate::project_store::LinkOutcome::Created(link)) =
                    crate::project_store::link_config_into_repo(&cwd)
                {
                    self.lines.push(Line::from(Span::styled(
                        format!(
                            "  linked {} → per-project config/state (add to .gitignore)",
                            link.display()
                        ),
                        styled(p.accent, false),
                    )));
                }
                let dest = cwd.join("AGENTS.md");
                if dest.exists() {
                    self.lines.push(Line::from(Span::styled(
                        "  AGENTS.md already exists — not overwriting",
                        styled(p.muted, false),
                    )));
                } else {
                    let line =
                        match std::fs::write(&dest, crate::structure::init_doc(&cwd, &idx, &edges))
                        {
                            Ok(()) => Span::styled(
                                format!("  created {}", dest.display()),
                                styled(p.accent, true),
                            ),
                            Err(e) => {
                                Span::styled(format!("  init failed: {e}"), styled(p.err, false))
                            }
                        };
                    self.lines.push(Line::from(line));
                }
                true
            }
            "plan" => {
                self.plan = !self.plan;
                self.lines.push(Line::from(Span::styled(
                    format!("  plan mode {}", if self.plan { "on" } else { "off" }),
                    styled(p.accent, false),
                )));
                true
            }
            "quit" => false, // caller handles this
            _ => {
                // check skills
                if let Some(skill) = self.skills.iter().find(|s| s.name == name) {
                    match crate::skills::load_skill_body(&skill.path) {
                        Some(body) => {
                            self.lines.push(Line::default());
                            self.lines.push(Line::from(Span::styled(
                                format!("  ▌ skill: {name}"),
                                styled(p.accent, true),
                            )));
                            // inject body as pending markdown for rendering
                            self.pending.push_str(&body);
                            self.flush_pending();
                        }
                        None => {
                            self.lines.push(Line::from(Span::styled(
                                format!("  ✗ empty skill: {name}"),
                                styled(p.err, false),
                            )));
                        }
                    }
                    return true;
                }
                self.lines.push(Line::from(Span::styled(
                    format!("  ✗ unknown command: /{name}"),
                    styled(p.err, false),
                )));
                true
            }
        }
    }

    pub(super) fn replay(&mut self) {
        let events = std::mem::take(&mut self.replay_events);
        self.lines.clear();
        self.pending.clear();
        self.deferred.clear(); // rebuild is synchronous (push_agent direct), no parking
        self.reveal_drain = false;
        self.pending_tool_inputs.clear();
        self.tool_boxes.clear();
        self.timeline.clear();
        self.popup = None;
        self.thread_active = false;
        self.replaying = true;
        self.replay_tool_idx = 0;
        for ev in &events {
            self.process_replay_event(ev);
        }
        self.replaying = false;
        self.replay_events = events;
    }

    fn process_replay_event(&mut self, ev: &ReplayEvent) {
        match ev {
            ReplayEvent::User(text) => {
                let p = self.palette;
                let w = self.chat_w();
                self.thread_active = false; // new turn: close the previous rail
                let target = self.lines.len();
                self.lines.push(Line::default());
                self.lines.extend(user_box(text, w, p));
                self.record_user_timeline(text, target);
            }
            ReplayEvent::Agent(aev) => self.push_agent(aev.clone()),
        }
    }

    pub(super) fn chat_w(&self) -> usize {
        let tw = ratatui::crossterm::terminal::size()
            .map(|(w, _)| w as usize)
            .unwrap_or(80);
        let panel_w = if self.panel_visible {
            self.panel_w as usize
        } else {
            0
        };
        tw.saturating_sub(panel_w + 2)
    }

    /// Record a user message in the side timeline. Snippet only — the full text
    /// lives in the chat at `target`.
    pub(super) fn record_user_timeline(&mut self, text: &str, target: usize) {
        let label: String = text.lines().next().unwrap_or("").chars().take(60).collect();
        self.timeline.push(TimelineEntry {
            label,
            clock: String::new(),
            target,
            is_user: true,
        });
    }

    pub(super) fn push(&mut self, ev: AgentEvent) {
        self.replay_events.push(ReplayEvent::Agent(ev.clone()));
        // Reveal-aware gate (see `deferred`): a segment boundary arriving mid-reveal
        // is parked so the typewriter finishes the current text first, instead of
        // flush_pending snapping it to full. Everything after a parked event also
        // waits, to keep order. drain_deferred() replays them once the tail drains.
        if !self.deferred.is_empty() {
            self.deferred.push_back(ev);
            return;
        }
        if self.revealing() && Self::is_segment_boundary(&ev) {
            self.reveal_drain = true; // release the jitter reserve so the tail drains
            self.deferred.push_back(ev);
            return;
        }
        self.push_agent(ev);
    }

    /// Events whose `push_agent` arm calls `flush_pending` (and would therefore
    /// dump the unrevealed tail). Only these are worth deferring until the
    /// typewriter catches up.
    fn is_segment_boundary(ev: &AgentEvent) -> bool {
        matches!(
            ev,
            AgentEvent::ToolCallStart { .. }
                | AgentEvent::TurnEnd
                | AgentEvent::Error(_)
                | AgentEvent::Notice { .. }
                | AgentEvent::Cancelled
        )
    }

    /// Replay parked boundaries in FIFO order once the reveal has caught up. A
    /// parked `TextChunk` (text after a tool) restarts the reveal, so we stop and
    /// let it play. If events remain queued, keep `reveal_drain` set: a boundary's
    /// flush_pending resets it, and the `busy && !reveal_drain` reserve in
    /// tick_reveal would otherwise freeze the reveal below total and deadlock the
    /// queue. Returns true if it applied anything (caller repaints).
    pub(super) fn drain_deferred(&mut self) -> bool {
        let mut applied = false;
        while !self.deferred.is_empty() && !self.revealing() {
            let ev = self.deferred.pop_front().expect("non-empty checked above");
            self.push_agent(ev);
            applied = true;
            if self.revealing() {
                break;
            }
        }
        if !self.deferred.is_empty() {
            self.reveal_drain = true;
        }
        applied
    }

    /// Append an agent block onto the timeline rail, returning the index of its
    /// first line (for tool-box hit-testing). The first block of a turn is preceded
    /// by a plain blank; later blocks by a rail blank, keeping the rail continuous.
    fn push_block(&mut self, lines: Vec<Line<'static>>) -> usize {
        if self.thread_active {
            self.lines.push(thread_blank(self.palette));
        } else {
            self.lines.push(Line::default());
            self.thread_active = true;
        }
        let start = self.lines.len();
        self.lines.extend(thread_wrap(lines, true, self.palette));
        start
    }

    fn push_agent(&mut self, ev: AgentEvent) {
        let p = self.palette;
        match ev {
            AgentEvent::TurnStart => {
                self.status = AgentStatus::LlmThinking;
                self.thinking_tail.clear();
            }
            AgentEvent::TextChunk(s) => {
                self.pending.push_str(&s);
            }
            AgentEvent::TurnEnd => {
                self.flush_pending();
                self.status = AgentStatus::Idle;
            }
            AgentEvent::ToolCallStart { id, name, input } => {
                self.flush_pending();
                self.status = AgentStatus::ToolRunning(name.clone());
                // Buffer input — render complete box only on ToolCallEnd
                let clock = chrono::Local::now().format("%H:%M").to_string();
                self.pending_tool_inputs
                    .push((id, name, input, Instant::now(), clock));
            }
            AgentEvent::ToolCallEnd {
                id,
                name,
                result,
                is_error,
            } => {
                self.status = AgentStatus::LlmThinking;
                let cw = self.chat_w().saturating_sub(THREAD_GUTTER);
                let content_max = cw.saturating_sub(11);

                // Pair by id (the tool_use_id) so parallel same-name calls match
                // their own Start; fall back to name for replay events lacking one.
                let tool_input = self
                    .pending_tool_inputs
                    .iter()
                    .position(|(i, n, ..)| if id.is_empty() { n == &name } else { i == &id })
                    .map(|pos| self.pending_tool_inputs.remove(pos));
                let full_input = tool_input
                    .as_ref()
                    .map(|(_, _, inp, _, _)| format_tool_input(inp))
                    .unwrap_or_default();
                // Wall time + start clock: measured live, recalled by index during
                // replay (session-resumed history has no timing → None).
                let (elapsed, start_clock) = if self.replaying {
                    let idx = self.replay_tool_idx;
                    self.replay_tool_idx += 1;
                    (
                        self.tool_durations.get(idx).copied().flatten(),
                        self.tool_start_clocks.get(idx).cloned().flatten(),
                    )
                } else {
                    let d = tool_input.as_ref().map(|(_, _, _, t0, _)| t0.elapsed());
                    let clk = tool_input.as_ref().map(|(_, _, _, _, c)| c.clone());
                    self.tool_durations.push(d);
                    self.tool_start_clocks.push(clk.clone());
                    (d, clk)
                };

                // Build the tool box block, then drop it onto the timeline rail.
                let mut blk: Vec<Line<'static>> = Vec::new();
                if let Some(rest) = result.strip_prefix("Edited ") {
                    // Side-by-side (old | new) diff with its own framed box.
                    let (head, diff) = rest.split_once("\n\n").unwrap_or((rest, ""));
                    let path = head.trim_end_matches('.').trim();
                    blk.extend(edit_diff_block(path, diff, cw, p));
                } else {
                    blk.push(tool_box_top(&name, start_clock.as_deref(), elapsed, cw, p));

                    if !full_input.is_empty() && full_input != "null" {
                        let in_text: String = if full_input.chars().count() > content_max {
                            format!(
                                "{}…",
                                full_input
                                    .chars()
                                    .take(content_max.saturating_sub(1))
                                    .collect::<String>()
                            )
                        } else {
                            full_input.clone()
                        };
                        blk.push(tool_box_row(
                            "IN",
                            vec![Span::styled(in_text, styled(p.muted, false))],
                            cw,
                            p,
                        ));
                    }

                    if result == "blocked" {
                        blk.push(tool_box_row(
                            "OUT",
                            vec![Span::styled("✗ blocked", styled(p.err, false))],
                            cw,
                            p,
                        ));
                    } else {
                        blk.extend(out_preview_rows(&name, &result, is_error, cw, p));
                    }
                }
                let box_start = self.push_block(blk);
                self.timeline.push(TimelineEntry {
                    label: name.clone(),
                    clock: start_clock.unwrap_or_default(),
                    target: box_start,
                    is_user: false,
                });
                self.tool_boxes.push(ToolBox {
                    line_start: box_start,
                    line_end: self.lines.len() - 1,
                    name: name.clone(),
                    full_input,
                    full_output: result.clone(),
                });
            }
            AgentEvent::Error(e) => {
                self.flush_pending();
                self.pending_tool_inputs.clear(); // no End will come — drop "running" boxes
                self.status = AgentStatus::Idle;
                self.push_block(vec![Line::from(Span::styled(
                    format!("✗ {e}"),
                    styled(p.err, false),
                ))]);
            }
            AgentEvent::Notice { text, level } => {
                // Non-failure status (oracle green/retry): keep it out of the red
                // reserved for Error, and don't reset run status mid-loop.
                self.flush_pending();
                let (glyph, color) = match level {
                    crate::types::NoticeLevel::Success => ("✓", p.success),
                    crate::types::NoticeLevel::Info => ("•", p.info),
                };
                self.push_block(vec![Line::from(Span::styled(
                    format!("{glyph} {text}"),
                    styled(color, false),
                ))]);
            }
            AgentEvent::Cancelled => {
                self.flush_pending();
                self.pending_tool_inputs.clear();
                self.status = AgentStatus::Idle;
                self.push_block(vec![Line::from(Span::styled(
                    "↩ cancelled",
                    styled(p.muted, false),
                ))]);
            }
            AgentEvent::ContextUsage {
                used_tokens,
                context_window,
                cached_tokens,
            } => {
                self.ctx_pct =
                    ((used_tokens as u64 * 100) / context_window.max(1) as u64).min(100) as u8;
                self.cached_pct =
                    ((cached_tokens as u64 * 100) / used_tokens.max(1) as u64).min(100) as u8;
                // One-shot warning at the amber threshold: answer quality
                // degrades well before the window is full (context rot).
                // Rearms when usage drops back (compaction, /clear).
                if self.ctx_pct >= 70 && !self.ctx_warned {
                    self.ctx_warned = true;
                    self.push_block(vec![Line::from(Span::styled(
                        format!("⚠ context at {}% of {}k tokens — quality may degrade, /compact to summarize",
                                self.ctx_pct, context_window / 1000),
                        styled(p.accent, false),
                    ))]);
                } else if self.ctx_pct < 70 {
                    self.ctx_warned = false;
                }
            }
            AgentEvent::ThinkingStart => {
                self.status = AgentStatus::LlmThinking;
                self.thinking_tail.clear();
            }
            AgentEvent::ThinkingChunk(s) => {
                // Not part of the chat: only the tail feeds the status bar,
                // so "thinking…" shows live progress instead of a black box.
                self.thinking_tail.push_str(&s);
                let overflow = self.thinking_tail.chars().count().saturating_sub(200);
                if overflow > 0 {
                    self.thinking_tail = self.thinking_tail.chars().skip(overflow).collect();
                }
            }
            AgentEvent::Compacted { messages } => {
                self.push_block(vec![Line::from(Span::styled(
                    format!("· context compacted ({} messages)", messages.len()),
                    styled(p.muted, false),
                ))]);
            }
            AgentEvent::WorkspaceSnapshot { .. } => {}
            AgentEvent::SpendUsage { spent, cap } => {
                self.spend_tokens = spent;
                self.spend_cap = Some(cap);
            }
            AgentEvent::JobDone {
                id,
                command,
                exit,
                secs,
            } => {
                let ok = exit == Some(0);
                let (mark, color) = if ok {
                    ("✓ ", p.success)
                } else {
                    ("✗ ", p.err)
                };
                let code = exit.map_or_else(|| "?".into(), |c| c.to_string());
                self.push_block(vec![Line::from(vec![
                    Span::styled(mark, styled(color, false)),
                    Span::styled(
                        format!(
                            "job #{id} finished · {} · exit {code}",
                            fmt_elapsed(Duration::from_secs(secs))
                        ),
                        styled(p.fg, true),
                    ),
                    Span::styled(format!("  — {command}"), styled(p.muted, false)),
                ])]);
            }
        }
    }

    pub(super) fn flush_pending(&mut self) {
        self.revealed = 0;
        self.reveal_anchor = None;
        self.reveal_clock = None;
        self.reveal_carry = 0.0;
        self.reveal_rate = 0.0;
        self.reveal_drain = false;
        self.tail_cache = None;
        let md = std::mem::take(&mut self.pending);
        if md.is_empty() {
            return;
        }
        let cw = self.chat_w().saturating_sub(THREAD_GUTTER);
        let lines = md_to_lines(&md, cw, self.palette);
        self.push_block(lines);
    }

    /// Advance the typewriter cursor at a near-constant on-screen velocity. The
    /// target is the provider's average production rate scaled by `PACE` (a
    /// touch under production for a calmer feel), low-passed over `TAU` so bursty
    /// arrivals don't make the cursor speed jump. While the stream is live a
    /// reserve (`RESERVE_SECS` worth of chars, capped at a quarter of the buffer)
    /// is held back as a jitter buffer; once the stream ends (`reveal_drain`) the
    /// reserve is released and the tail drains at the same constant rate rather
    /// than being dumped by `flush_pending`. Returns true if it advanced.
    pub(super) fn tick_reveal(&mut self) -> bool {
        let total = self.pending.chars().count();
        if total == 0 || self.revealed >= total {
            self.reveal_clock = None;
            self.reveal_carry = 0.0;
            self.reveal_rate = 0.0;
            return false;
        }
        const MIN_RATE: f64 = 25.0; // chars/sec floor — short replies still move
        const MAX_RATE: f64 = 220.0; // ceiling — keeps fast dumps readable/slow
        const PACE: f64 = 0.85; // run slightly under production rate
        const TAU: f64 = 1.5; // rate low-pass time constant (s) → steady velocity
        const RESERVE_SECS: f64 = 0.6; // jitter buffer to absorb provider pauses
        let now = Instant::now();
        let anchor = *self.reveal_anchor.get_or_insert(now);
        let dt = self
            .reveal_clock
            .map_or(0.016, |c| now.duration_since(c).as_secs_f64())
            .min(0.1);
        self.reveal_clock = Some(now);
        // Track the rate only while text is still arriving; once draining, hold
        // the last rate so the tail keeps the same constant velocity.
        if !self.reveal_drain {
            let produced = now.duration_since(anchor).as_secs_f64();
            let measured = if produced > 0.25 {
                total as f64 / produced
            } else {
                MIN_RATE
            };
            let target = (measured * PACE).clamp(MIN_RATE, MAX_RATE);
            if self.reveal_rate <= 0.0 {
                self.reveal_rate = target;
            } else {
                let a = 1.0 - (-dt / TAU).exp();
                self.reveal_rate += (target - self.reveal_rate) * a;
            }
        }
        let rate = if self.reveal_rate > 0.0 {
            self.reveal_rate
        } else {
            MIN_RATE
        };
        self.reveal_carry += rate * dt;
        let cap = if self.busy && !self.reveal_drain {
            total - ((rate * RESERVE_SECS) as usize).min(total / 4)
        } else {
            total
        };
        let target = (self.revealed + self.reveal_carry as usize).min(cap);
        let advanced = target.saturating_sub(self.revealed);
        self.reveal_carry -= advanced as f64;
        if target >= cap {
            self.reveal_carry = self.reveal_carry.min(1.0); // don't bank a burst while capped
        }
        self.revealed = target;
        advanced > 0
    }

    /// True while the reveal cursor trails the received text — keeps the event
    /// loop on the fast frame cadence until the tail has caught up.
    pub(super) fn revealing(&self) -> bool {
        self.revealed < self.pending.chars().count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ContentBlock, Message, Role};

    #[test]
    fn replay_maps_tool_result_to_tool_name() {
        let messages = vec![
            Message::user("ciao"),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Text { text: "ok".into() },
                    ContentBlock::ToolUse {
                        id: "t1".into(),
                        name: "bash".into(),
                        input: serde_json::json!({"command": "ls"}),
                    },
                ],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "file.txt".into(),
                    is_error: true,
                }],
            },
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "done".into(),
                }],
            },
        ];

        let events = messages_to_replay(&messages);

        // First a user bubble, last assistant text — and no stray user bubble for the tool result.
        let user_count = events
            .iter()
            .filter(|e| matches!(e, ReplayEvent::User(_)))
            .count();
        assert_eq!(
            user_count, 1,
            "tool-result user message must not render as a user bubble"
        );

        // The tool result must carry the name resolved from the preceding ToolUse
        // and the is_error flag from the ToolResult block.
        let end = events.iter().find_map(|e| match e {
            ReplayEvent::Agent(AgentEvent::ToolCallEnd {
                name,
                result,
                is_error,
                ..
            }) => Some((name.clone(), result.clone(), *is_error)),
            _ => None,
        });
        assert_eq!(
            end,
            Some(("bash".to_string(), "file.txt".to_string(), true))
        );
    }

    #[test]
    fn replay_handles_parallel_tool_calls_in_order() {
        // pi runs tools in parallel: one assistant message with several ToolUse,
        // then one user message with the matching ToolResults.
        let messages = vec![
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Text {
                        text: "reading the two files".into(),
                    },
                    ContentBlock::ToolUse {
                        id: "a".into(),
                        name: "read".into(),
                        input: serde_json::json!({"path": "x"}),
                    },
                    ContentBlock::ToolUse {
                        id: "b".into(),
                        name: "read".into(),
                        input: serde_json::json!({"path": "y"}),
                    },
                ],
            },
            Message {
                role: Role::User,
                content: vec![
                    ContentBlock::ToolResult {
                        tool_use_id: "a".into(),
                        content: "X".into(),
                        is_error: false,
                    },
                    ContentBlock::ToolResult {
                        tool_use_id: "b".into(),
                        content: "Y".into(),
                        is_error: false,
                    },
                ],
            },
        ];

        let kinds: Vec<&str> = messages_to_replay(&messages)
            .iter()
            .map(|e| match e {
                ReplayEvent::User(_) => "User",
                ReplayEvent::Agent(AgentEvent::TextChunk(_)) => "TextChunk",
                ReplayEvent::Agent(AgentEvent::ToolCallStart { .. }) => "Start",
                ReplayEvent::Agent(AgentEvent::ToolCallEnd { .. }) => "End",
                ReplayEvent::Agent(AgentEvent::TurnEnd) => "TurnEnd",
                _ => "other",
            })
            .collect();

        assert_eq!(
            kinds,
            ["TextChunk", "Start", "Start", "TurnEnd", "End", "End"]
        );

        // Both results keep their name and pair to the right output.
        let ends: Vec<(String, String)> = messages_to_replay(&messages)
            .iter()
            .filter_map(|e| match e {
                ReplayEvent::Agent(AgentEvent::ToolCallEnd { name, result, .. }) => {
                    Some((name.clone(), result.clone()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            ends,
            [("read".into(), "X".into()), ("read".into(), "Y".into())]
        );

        // Each event carries the tool_use_id, so two same-name calls pair by id
        // (not by the colliding name) regardless of arrival order.
        let ids: Vec<String> = messages_to_replay(&messages)
            .iter()
            .filter_map(|e| match e {
                ReplayEvent::Agent(AgentEvent::ToolCallStart { id, .. })
                | ReplayEvent::Agent(AgentEvent::ToolCallEnd { id, .. }) => Some(id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(ids, ["a", "b", "a", "b"]);
    }

    fn tool_start(id: &str, name: &str) -> AgentEvent {
        AgentEvent::ToolCallStart {
            id: id.into(),
            name: name.into(),
            input: serde_json::Value::Null,
        }
    }

    #[test]
    fn tool_start_defers_during_reveal_applies_when_idle() {
        let mut app = App::new("m".into(), "p".into());

        // Nothing revealing → a tool start renders its running box immediately.
        app.push(tool_start("1", "ls"));
        assert_eq!(app.pending_tool_inputs.len(), 1);
        assert!(app.deferred.is_empty());

        // Mid-reveal → the next tool start is parked, not shown, and the reserve
        // is released so the tail can drain.
        app.busy = true;
        app.push(AgentEvent::TextChunk("hello world".into()));
        assert!(app.revealing());
        app.push(tool_start("2", "grep"));
        assert_eq!(app.deferred.len(), 1);
        assert_eq!(
            app.pending_tool_inputs.len(),
            1,
            "second tool stays parked until text drains"
        );
        assert!(app.reveal_drain);

        // Cursor catches up → drain reveals the parked box.
        app.revealed = app.pending.chars().count();
        app.drain_deferred();
        assert!(app.deferred.is_empty());
        assert_eq!(app.pending_tool_inputs.len(), 2);
    }

    #[test]
    fn deferred_queue_drains_without_deadlock() {
        // Regression: tool-end + trailing text + TurnEnd all arrive before the
        // first sentence finishes revealing, so they pile up behind the parked
        // ToolCallStart. A boundary's flush_pending resets reveal_drain; without
        // drain_deferred re-arming it, the busy-reserve cap would freeze the
        // reveal below total and the queue would never drain (busy forever).
        let mut app = App::new("m".into(), "p".into());
        app.busy = true;

        app.push(AgentEvent::TextChunk(
            "the classifier is the last stage. ".into(),
        ));
        assert!(app.revealing());
        app.push(tool_start("g", "grep"));
        app.push(AgentEvent::ToolCallEnd {
            id: "g".into(),
            name: "grep".into(),
            result: "hit".into(),
            is_error: false,
        });
        app.push(AgentEvent::TextChunk("found it in agent.rs.".into()));
        app.push(AgentEvent::TurnEnd);

        assert_eq!(
            app.deferred.len(),
            4,
            "all four park behind the mid-reveal boundary"
        );
        assert!(
            app.pending_tool_inputs.is_empty(),
            "tool box must not appear yet"
        );

        // Emulate the run loop: tick_reveal advances (capped by the reserve while
        // busy && !reveal_drain), then drain_deferred replays ready events. busy
        // stays set here — the real loop clears it on finalize; the cap is exactly
        // what would freeze the reveal without the drain_deferred re-arm fix.
        for _ in 0..64 {
            let total = app.pending.chars().count();
            let target = if app.busy && !app.reveal_drain {
                total.saturating_sub((total / 4).max(1)) // reserve held → never hits total
            } else {
                total
            };
            if target > app.revealed {
                app.revealed = target;
            }
            app.drain_deferred();
            if app.deferred.is_empty() && !app.revealing() {
                break;
            }
        }

        assert!(
            app.deferred.is_empty(),
            "queue must fully drain (no deadlock)"
        );
        assert!(
            !app.revealing(),
            "the tail must finish revealing, not freeze under the reserve"
        );
        assert!(app.pending.is_empty(), "TurnEnd flushed the trailing text");
        assert!(
            app.tool_boxes.iter().any(|b| b.name == "grep"),
            "grep box reached the rail"
        );
    }
}
