use std::time::Duration;

use ratatui::{
    layout::{Margin, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap,
    },
    Frame,
};

use super::theme::{styled, surface_tint, Palette};
use crate::agent::{Prompt, PromptAnswer, PromptKind, PromptQuestion, PromptReply};

pub const KB_PANEL_W: u16 = 50;

/// Default / min / max width of the side timeline panel (drag-resizable).
pub const TIMELINE_W: u16 = 30;
pub const TIMELINE_W_MIN: u16 = 16;
pub const TIMELINE_W_MAX: u16 = 70;

/// Tree decoration for a trail entry: `(first-line prefix, continuation prefix, body)`.
/// User messages sit at the top level (`▌`); tools nest under them with `├`/`└`
/// branches and a `●` bullet. Body keeps the clock but drops the duration.
pub fn timeline_tree_parts(
    is_user: bool,
    label: &str,
    clock: &str,
    last_child: bool,
) -> (&'static str, &'static str, String) {
    if is_user {
        ("▌ ", "  ", label.to_string())
    } else {
        let body = if clock.is_empty() {
            label.to_string()
        } else {
            format!("{label} · {clock}")
        };
        let (prefix, cont) = if last_child {
            (" └ ● ", "     ")
        } else {
            (" ├ ● ", " │   ")
        };
        (prefix, cont, body)
    }
}

/// One timeline entry as one-or-more wrapped lines. `prefix` leads the first line
/// (e.g. `▌ ` for a user message, ` ├ ● ` / ` └ ● ` for a nested tool); `cont`
/// leads wrapped continuation lines so they align under the body (keeping the
/// `│` rail for non-last children). `selected` highlights the current step.
pub fn timeline_entry_lines(
    prefix: &str,
    cont: &str,
    body: &str,
    selected: bool,
    w: usize,
    p: &Palette,
) -> Vec<Line<'static>> {
    let color = if selected { p.fg } else { p.muted };
    let avail = w.saturating_sub(prefix.chars().count()).max(4);
    let mut wrapped = wrap_text(body, avail).into_iter();
    let Some(first) = wrapped.next() else {
        return Vec::new();
    };
    let mut lines = vec![Line::from(Span::styled(
        format!("{prefix}{first}"),
        styled(color, selected),
    ))];
    for c in wrapped {
        lines.push(Line::from(Span::styled(
            format!("{cont}{c}"),
            styled(color, false),
        )));
    }
    lines
}

// ── about screen helpers ──────────────────────────────────────────────────────

pub fn kb_single(key: &'static str, desc: &'static str, p: &Palette) -> Vec<Line<'static>> {
    vec![
        Line::from(vec![
            Span::styled("    ", styled(p.muted, false)),
            Span::styled(
                key.to_string(),
                Style::default().fg(p.fg).add_modifier(Modifier::UNDERLINED),
            ),
            Span::styled(format!("  {desc}"), styled(p.muted, false)),
        ]),
        Line::default(),
    ]
}

pub fn kb_combo(
    modifier: &'static str,
    key: &'static str,
    desc: &'static str,
    p: &Palette,
) -> Vec<Line<'static>> {
    vec![
        Line::from(vec![
            Span::styled("    ", styled(p.muted, false)),
            Span::styled(
                modifier.to_string(),
                Style::default().fg(p.fg).add_modifier(Modifier::UNDERLINED),
            ),
            Span::styled("+", styled(p.muted, false)),
            Span::styled(
                key.to_string(),
                Style::default().fg(p.fg).add_modifier(Modifier::UNDERLINED),
            ),
            Span::styled(format!("  {desc}"), styled(p.muted, false)),
        ]),
        Line::default(),
    ]
}

pub fn build_kb_lines(p: &Palette) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for chunk in [
        kb_combo("Ctrl", "C", "Cancel agent · twice to quit", p),
        kb_single("Esc", "Cancel agent", p),
        kb_single("Tab", "Focus chat ↔ input", p),
        kb_single("↑ ↓", "Scroll output", p),
        kb_combo("Pg", "Up/Dn", "Fast scroll", p),
        kb_single("G", "Go to bottom (auto-scroll)", p),
        kb_combo("Ctrl", "V", "Attach image from clipboard", p),
        kb_combo("Alt", "B", "Toggle timeline panel", p),
        kb_combo("Alt", "P", "Cycle palette", p),
        kb_combo("Alt", "A", "About", p),
        kb_combo("Alt", "S", "Settings", p),
    ] {
        lines.extend(chunk);
    }
    lines
}

// ── tool call boxes ───────────────────────────────────────────────────────────

/// Compact human duration: `0.4s`, `12s`, `2m05s`.
pub fn fmt_elapsed(d: Duration) -> String {
    let s = d.as_secs_f32();
    if s < 10.0 {
        format!("{s:.1}s")
    } else if s < 60.0 {
        format!("{s:.0}s")
    } else {
        format!("{}m{:02}s", d.as_secs() / 60, d.as_secs() % 60)
    }
}

/// Compact token count: `0`, `999`, `1k`, `142k`, `1.2M`.
pub fn fmt_tok(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => format!("{}k", n / 1_000),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}

/// Gauge text for one running job: elapsed only, or — when the job emits
/// progress markers — a bar with percent and a linear ETA (`~`: estimate).
pub fn job_gauge(elapsed: Duration, progress: Option<f32>) -> String {
    match progress {
        Some(p) if p >= 0.05 => {
            let filled = ((p * 8.0).round() as usize).min(8);
            let bar = format!("{}{}", "█".repeat(filled), "░".repeat(8 - filled));
            let eta = elapsed.as_secs_f32() * (1.0 - p) / p;
            format!(
                "[{bar}] {:.0}% ~{} left",
                p * 100.0,
                fmt_elapsed(Duration::from_secs_f32(eta.max(0.0)))
            )
        }
        _ => fmt_elapsed(elapsed),
    }
}

/// Tool header: `▸ name · 14:32 · 1.2s` — start time (24h `HH:MM`) then run
/// duration. No box — the timeline rail already supplies the left edge, so a
/// full border would double-frame the step. `start`/`elapsed` are absent on
/// session-resumed history (no timing).
pub fn tool_box_top(
    name: &str,
    start: Option<&str>,
    elapsed: Option<Duration>,
    _w: usize,
    p: &Palette,
) -> Line<'static> {
    let mut spans = vec![Span::styled(format!("▸ {name}"), styled(p.info, true))];
    if let Some(hm) = start {
        spans.push(Span::styled(format!(" · {hm}"), styled(p.muted, false)));
    }
    if let Some(d) = elapsed {
        spans.push(Span::styled(
            format!(" · {}", fmt_elapsed(d)),
            styled(p.muted, false),
        ));
    }
    Line::from(spans)
}

/// A tool I/O row: a dim metadata label (`IN`/`OUT`) + content, no borders.
pub fn tool_box_row(
    label: &'static str,
    spans: Vec<Span<'static>>,
    _w: usize,
    p: &Palette,
) -> Line<'static> {
    let mut row = vec![Span::styled(format!("{label:<3} "), styled(p.muted, false))];
    row.extend(spans);
    Line::from(row)
}

/// Rendered `todo` tool call: the model's live step list as a checklist.
/// Completed steps are struck-through and dim, the in-progress one highlighted,
/// pending ones plain — the "Update Todos" view Claude Code/Codex users expect.
pub fn todo_block(
    items: &[crate::tools::TodoItem],
    start: Option<&str>,
    elapsed: Option<Duration>,
    w: usize,
    p: &Palette,
) -> Vec<Line<'static>> {
    use crate::tools::TodoStatus;
    let glyph_max = w.saturating_sub(7); // label(4) + mark(2) + margin
    let mut rows = Vec::with_capacity(items.len() + 1);
    rows.push(tool_box_top("todo", start, elapsed, w, p));
    for t in items {
        let content: String = if t.content.chars().count() > glyph_max {
            format!(
                "{}…",
                t.content
                    .chars()
                    .take(glyph_max.saturating_sub(1))
                    .collect::<String>()
            )
        } else {
            t.content.clone()
        };
        let (mark, mark_style, text_style) = match t.status {
            TodoStatus::Completed => (
                "✔ ",
                styled(p.success, false),
                styled(p.muted, false).add_modifier(Modifier::CROSSED_OUT),
            ),
            TodoStatus::InProgress => ("❯ ", styled(p.info, true), styled(p.fg, true)),
            TodoStatus::Pending => ("☐ ", styled(p.muted, false), styled(p.fg, false)),
        };
        rows.push(tool_box_row(
            "",
            vec![
                Span::styled(mark, mark_style),
                Span::styled(content, text_style),
            ],
            w,
            p,
        ));
    }
    rows
}

/// OUT preview: up to 3 result lines with a ✓/✗ outcome mark. Bash shows the
/// *tail* (test/exit summaries sit at the end); other tools show the head.
/// A dim `+N more lines` row points at the click-to-expand popup.
pub fn out_preview_rows(
    name: &str,
    result: &str,
    is_error: bool,
    w: usize,
    p: &Palette,
) -> Vec<Line<'static>> {
    const OUT_PREVIEW_LINES: usize = 3;
    let glyph_max = w.saturating_sub(13); // label(4) + mark(2) + margin
    let (mark, mark_color, text_color) = if is_error {
        ("✗ ", p.err, p.err)
    } else {
        ("✓ ", p.success, p.fg)
    };

    let all: Vec<&str> = result.trim_end().lines().collect();
    if all.is_empty() {
        return vec![tool_box_row(
            "OUT",
            vec![
                Span::styled(mark, styled(mark_color, false)),
                Span::styled("(no output)", styled(p.muted, false)),
            ],
            w,
            p,
        )];
    }
    let n_shown = all.len().min(OUT_PREVIEW_LINES);
    let shown = if name == "bash" {
        &all[all.len() - n_shown..]
    } else {
        &all[..n_shown]
    };
    let hidden = all.len() - n_shown;

    let mut rows = Vec::with_capacity(n_shown + 1);
    for (i, raw) in shown.iter().enumerate() {
        let t: String = if raw.chars().count() > glyph_max {
            format!(
                "{}…",
                raw.chars()
                    .take(glyph_max.saturating_sub(1))
                    .collect::<String>()
            )
        } else {
            (*raw).to_string()
        };
        let lead = if i == 0 {
            Span::styled(mark, styled(mark_color, false))
        } else {
            Span::raw("  ")
        };
        rows.push(tool_box_row(
            if i == 0 { "OUT" } else { "" },
            vec![lead, Span::styled(t, styled(text_color, false))],
            w,
            p,
        ));
    }
    if hidden > 0 {
        rows.push(tool_box_row(
            "",
            vec![Span::styled(
                format!("  … +{hidden} more lines (click to expand)"),
                styled(p.muted, false),
            )],
            w,
            p,
        ));
    }
    rows
}

/// Transient box for a tool that is still executing: name + IN + animated
/// "running… 12s" row. Rendered each frame while the call is in flight and
/// replaced by the final box on `ToolCallEnd`.
pub fn running_tool_block(
    name: &str,
    input_text: &str,
    spinner: &str,
    start: Option<&str>,
    elapsed: Duration,
    w: usize,
    p: &Palette,
) -> Vec<Line<'static>> {
    let content_max = w.saturating_sub(11);
    let mut blk = vec![tool_box_top(name, start, None, w, p)];
    if !input_text.is_empty() && input_text != "null" {
        let in_text: String = if input_text.chars().count() > content_max {
            format!(
                "{}…",
                input_text
                    .chars()
                    .take(content_max.saturating_sub(1))
                    .collect::<String>()
            )
        } else {
            input_text.to_string()
        };
        blk.push(tool_box_row(
            "IN",
            vec![Span::styled(in_text, styled(p.muted, false))],
            w,
            p,
        ));
    }
    blk.push(tool_box_row(
        "OUT",
        vec![Span::styled(
            format!("{spinner} running… {}", fmt_elapsed(elapsed)),
            styled(p.accent, false),
        )],
        w,
        p,
    ));
    blk
}

// ── conversation thread (timeline rail) ───────────────────────────────────────
//
// Agent output (tool boxes, LLM text) is nested under the user message it answers
// by prepending a left rail: `●` opens each step, `│` continues the turn. Content
// must be built at width `w - THREAD_GUTTER` so the prefixed line still fits in `w`.

pub const THREAD_GUTTER: usize = 4; // width of "  ● " / "  │ "

/// Prepend the timeline rail to a block of lines: `●` on the first line when
/// `starts_node`, `│` on every other line. Preserves line count.
pub fn thread_wrap(
    lines: Vec<Line<'static>>,
    starts_node: bool,
    p: &Palette,
) -> Vec<Line<'static>> {
    lines
        .into_iter()
        .enumerate()
        .map(|(i, line)| {
            let prefix = if i == 0 && starts_node {
                Span::styled("  ● ", styled(p.accent, false))
            } else {
                Span::styled("  │ ", styled(p.muted, false))
            };
            let mut spans = Vec::with_capacity(line.spans.len() + 1);
            spans.push(prefix);
            spans.extend(line.spans);
            Line::from(spans)
        })
        .collect()
}

/// A rail-only line used to keep the timeline continuous between blocks.
pub fn thread_blank(p: &Palette) -> Line<'static> {
    Line::from(Span::styled("  │", styled(p.muted, false)))
}

// ── user message box ─────────────────────────────────────────────────────

pub fn wrap_text(text: &str, max_w: usize) -> Vec<String> {
    if max_w == 0 {
        return vec![text.to_string()];
    }
    let mut result = Vec::new();
    for line in text.lines() {
        if line.chars().count() <= max_w {
            result.push(line.to_string());
        } else {
            let mut current = String::new();
            for word in line.split_whitespace() {
                if current.is_empty() {
                    current = word.to_string();
                } else if current.chars().count() + 1 + word.chars().count() <= max_w {
                    current.push(' ');
                    current.push_str(word);
                } else {
                    result.push(current);
                    current = word.to_string();
                }
                // Hard-split anything wider than the box (paths, URLs) so it
                // can't overflow the tinted zone.
                while current.chars().count() > max_w {
                    result.push(current.chars().take(max_w).collect());
                    current = current.chars().skip(max_w).collect();
                }
            }
            if !current.is_empty() {
                result.push(current);
            }
        }
    }
    if result.is_empty() {
        result.push(String::new());
    }
    result
}

/// User message: a left accent bar + bold text on a subtly lighter background,
/// so it reads as its own zone — distinct from bordered tool boxes and plain
/// agent text. The bar + bold carry the distinction in 16-color terminals; the
/// background tint is a true-color enhancement on top.
pub fn user_box(text: &str, w: usize, p: &Palette) -> Vec<Line<'static>> {
    let surface = surface_tint(p.bg);
    let bar_s = Style::default().fg(p.accent).bg(surface);
    let text_s = Style::default()
        .fg(p.fg)
        .bg(surface)
        .add_modifier(Modifier::BOLD);
    let fill_s = Style::default().bg(surface);

    let zone_w = w.saturating_sub(2); // 2-col left gutter (no background)
    let inner_w = zone_w.saturating_sub(3); // "▌ " prefix + 1 trailing space

    let mut lines = Vec::new();
    for wrapped in wrap_text(text, inner_w) {
        let len = wrapped.chars().count();
        let pad = zone_w.saturating_sub(2 + len); // remaining cells after "▌ " + text
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled("▌ ", bar_s),
            Span::styled(wrapped, text_s),
            Span::styled(" ".repeat(pad), fill_s),
        ]));
    }
    lines
}

// ── scroll indicators ─────────────────────────────────────────────────────────

/// Scrollbar on the chat block's right border plus a `↓ N` badge on the bottom
/// border when the view is pinned away from the live tail. Shared with the mock.
pub fn render_scroll_indicators(
    f: &mut Frame,
    chat_area: Rect,
    scroll: usize,
    max_scroll: usize,
    auto_scroll: bool,
    p: &Palette,
) {
    if max_scroll == 0 {
        return;
    }
    let mut state = ScrollbarState::new(max_scroll).position(scroll);
    f.render_stateful_widget(
        Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .track_style(Style::default().fg(p.border))
            .thumb_style(Style::default().fg(p.muted)),
        chat_area.inner(Margin {
            vertical: 1,
            horizontal: 0,
        }),
        &mut state,
    );
    if !auto_scroll && scroll < max_scroll {
        let label = format!(" ↓ {} ", max_scroll - scroll);
        let w = label.chars().count() as u16;
        let badge = Rect::new(
            chat_area.x + chat_area.width.saturating_sub(w + 2),
            chat_area.y + chat_area.height.saturating_sub(1),
            w.min(chat_area.width),
            1,
        );
        f.render_widget(
            Paragraph::new(Span::styled(label, styled(p.accent, true))),
            badge,
        );
    }
}

// ── confirm dialog ────────────────────────────────────────────────────────────

/// Interactive state for a pending [`Prompt`] dialog: which row is focused and,
/// when editing, the free-text/glob buffer. Shared by the real TUI and the mock
/// (both hold `Option<PromptUi>`) so navigation logic can't drift between them.
pub struct PromptUi {
    pub prompt: Prompt,
    /// Focused row. `0..options.len()` are options; `options.len()` is the
    /// "Other…" free-text row (present only when `allow_free_text`).
    pub selected: usize,
    /// True while the user is typing into `input`.
    pub editing: bool,
    /// Edit buffer: the "Other" value or an edited allow-glob.
    pub input: String,
    /// Active question and locally collected answers for an aggregated round.
    pub round_index: usize,
    pub round_answers: Vec<Option<PromptAnswer>>,
}

impl PromptUi {
    pub fn new(prompt: Prompt) -> Self {
        let round_len = match &prompt.kind {
            PromptKind::QuestionRound { questions } => questions.len(),
            _ => 0,
        };
        Self {
            prompt,
            selected: 0,
            editing: false,
            input: String::new(),
            round_index: 0,
            round_answers: vec![None; round_len],
        }
    }

    fn active_question(&self) -> Option<&PromptQuestion> {
        match &self.prompt.kind {
            PromptKind::QuestionRound { questions } => questions.get(self.round_index),
            _ => None,
        }
    }

    fn options(&self) -> &[String] {
        self.active_question()
            .map_or(&self.prompt.options, |question| &question.options)
    }

    fn allow_free_text(&self) -> bool {
        self.active_question()
            .map_or(self.prompt.allow_free_text, |question| {
                question.allow_free_text
            })
    }

    /// Total rows including the optional free-text row.
    fn row_count(&self) -> usize {
        self.options().len() + usize::from(self.allow_free_text())
    }

    /// Whether `selected` is the "Other…" free-text row.
    fn on_free_text(&self) -> bool {
        self.allow_free_text() && self.selected == self.options().len()
    }

    /// Whether the focused row accepts inline editing: the "Other…" row always,
    /// and the permission "Allow always" row (index 1) to tune its glob.
    fn editable_row(&self) -> bool {
        self.on_free_text()
            || (matches!(self.prompt.kind, PromptKind::Permission { .. }) && self.selected == 1)
    }

    /// The glob shown on the "Allow always" row: the user's edit, else the
    /// suggested rule.
    fn allow_glob(&self) -> &str {
        if !self.input.is_empty() {
            return &self.input;
        }
        match &self.prompt.kind {
            PromptKind::Permission { suggested_glob } => suggested_glob,
            PromptKind::Question | PromptKind::QuestionRound { .. } => "",
        }
    }

    pub fn up(&mut self) {
        if self.editing {
            return;
        }
        let n = self.row_count().max(1);
        self.selected = (self.selected + n - 1) % n;
        self.input.clear();
    }

    pub fn down(&mut self) {
        if self.editing {
            return;
        }
        let n = self.row_count().max(1);
        self.selected = (self.selected + 1) % n;
        self.input.clear();
    }

    /// Enter edit mode on the focused editable row, seeding the buffer (the
    /// suggested glob for the Allow-always row). No-op on non-editable rows.
    pub fn begin_edit(&mut self) -> bool {
        if !self.editable_row() {
            return false;
        }
        self.editing = true;
        if self.input.is_empty() {
            if let (1, PromptKind::Permission { suggested_glob }) =
                (self.selected, &self.prompt.kind)
            {
                self.input = suggested_glob.clone();
            }
        }
        true
    }

    pub fn push_char(&mut self, c: char) {
        if self.editing {
            self.input.push(c);
        }
    }

    pub fn backspace(&mut self) {
        if self.editing {
            self.input.pop();
        }
    }

    /// Build the reply for the current selection (Enter). The free-text row and a
    /// non-empty Allow-always edit carry `text`; plain option rows carry only the
    /// index.
    pub fn reply(&self) -> PromptReply {
        if self.on_free_text() {
            return PromptReply {
                index: None,
                text: Some(self.input.clone()),
                ..PromptReply::default()
            };
        }
        let text = match (&self.prompt.kind, self.selected) {
            (PromptKind::Permission { .. }, 1) if !self.input.is_empty() => {
                Some(self.input.clone())
            }
            _ => None,
        };
        PromptReply {
            index: Some(self.selected),
            text,
            ..PromptReply::default()
        }
    }

    /// Record the active choice. A question round returns a bridge reply only
    /// after its final question, so the agent receives one aggregated response.
    pub fn confirm(&mut self) -> Option<PromptReply> {
        if !matches!(self.prompt.kind, PromptKind::QuestionRound { .. }) {
            return Some(self.reply());
        }
        let answer = if self.on_free_text() {
            PromptAnswer {
                index: None,
                text: Some(self.input.clone()),
            }
        } else {
            PromptAnswer {
                index: Some(self.selected),
                text: None,
            }
        };
        self.round_answers[self.round_index] = Some(answer);
        if self.round_index + 1 < self.round_answers.len() {
            self.round_index += 1;
            self.selected = 0;
            self.editing = false;
            self.input.clear();
            None
        } else {
            Some(PromptReply {
                answers: self
                    .round_answers
                    .iter()
                    .map(|answer| answer.clone().unwrap_or_default())
                    .collect(),
                ..PromptReply::default()
            })
        }
    }
}

/// Centered multi-choice dialog for a pending [`Prompt`] (permission gate or an
/// `ask_user` question). Shared by the real TUI and the mock so the two can't
/// drift. Navigate with ↑/↓, Enter confirms, `e` edits the focused editable row,
/// Esc cancels.
pub fn render_prompt_dialog(f: &mut Frame, ui: &PromptUi, p: &Palette) {
    let area = f.area();
    let pw = (area.width * 3 / 5).clamp(34, 100);
    let inner_w = pw.saturating_sub(4) as usize;
    let is_perm = matches!(ui.prompt.kind, PromptKind::Permission { .. });
    let border = if is_perm { p.err } else { p.accent };

    let title = ui
        .active_question()
        .map_or(ui.prompt.title.as_str(), |question| question.title.as_str());
    let detail = ui
        .active_question()
        .and_then(|question| question.detail.as_ref())
        .or(ui.prompt.detail.as_ref());
    let options = ui.options();
    let allow_free_text = ui.allow_free_text();
    let mut lines = vec![Line::from(Span::styled(
        format!(" {title}"),
        styled(p.muted, false),
    ))];
    if let Some(detail) = detail {
        for l in wrap_text(detail, inner_w) {
            lines.push(Line::from(Span::styled(
                format!("   {l}"),
                styled(p.fg, true),
            )));
        }
    }
    lines.push(Line::default());

    // Option rows, then the optional "Other…" free-text row.
    let n_opts = options.len();
    let labels = options
        .iter()
        .cloned()
        .chain(allow_free_text.then(|| "Other…".to_string()));
    for (row, label) in labels.enumerate() {
        let focused = row == ui.selected;
        let marker = if focused { " ❯ " } else { "   " };
        let is_free = allow_free_text && row == n_opts;
        let allow_always = is_perm && row == 1;

        let mut spans = vec![
            Span::styled(marker, styled(p.accent, true)),
            Span::styled(label, styled(if focused { p.fg } else { p.muted }, focused)),
        ];
        // Show the (editable) glob on the Allow-always row.
        if allow_always {
            spans.push(Span::styled(
                format!("  {}", ui.allow_glob()),
                styled(p.success, false),
            ));
        }
        // Show the live edit buffer under focus.
        if focused && ui.editing && is_free {
            spans.push(Span::styled(format!("  {}▏", ui.input), styled(p.fg, true)));
        } else if focused && ui.editing && allow_always {
            // Replace the static glob with the live buffer + cursor.
            spans.truncate(2);
            spans.push(Span::styled(
                format!("  {}▏", ui.input),
                styled(p.success, true),
            ));
        }
        lines.push(Line::from(spans));
    }

    lines.push(Line::default());
    let hint = if ui.editing {
        "enter save · esc cancel edit"
    } else if let PromptKind::QuestionRound { questions } = &ui.prompt.kind {
        if ui.round_index + 1 == questions.len() {
            "↑↓ move · enter submit round · e edit · esc cancel"
        } else {
            "↑↓ move · enter next question · e edit · esc cancel"
        }
    } else {
        "↑↓ move · enter confirm · e edit · esc cancel"
    };
    lines.push(Line::from(Span::styled(
        format!(" {hint}"),
        styled(p.muted, false),
    )));

    let ph = (lines.len() as u16 + 2).min(area.height);
    let popup_area = Rect::new(
        area.x + area.width.saturating_sub(pw) / 2,
        area.y + area.height.saturating_sub(ph) / 2,
        pw,
        ph,
    );
    let dialog_title = if is_perm {
        "⚠ permission".to_string()
    } else if let PromptKind::QuestionRound { questions } = &ui.prompt.kind {
        format!("question {}/{}", ui.round_index + 1, questions.len())
    } else {
        "question".to_string()
    };
    f.render_widget(Clear, popup_area);
    f.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(styled(border, false))
                .title(Span::styled(
                    format!(" {dialog_title} "),
                    styled(border, true),
                )),
        ),
        popup_area,
    );
}

#[cfg(test)]
mod tests {
    use super::{fmt_tok, todo_block, PromptUi};
    use crate::agent::{Prompt, PromptKind, PromptQuestion};
    use crate::tools::{TodoItem, TodoStatus};
    use crate::tui::theme::PALETTES;

    #[test]
    fn todo_block_marks_and_strikethrough() {
        let items = vec![
            TodoItem {
                content: "done step".into(),
                status: TodoStatus::Completed,
            },
            TodoItem {
                content: "current step".into(),
                status: TodoStatus::InProgress,
            },
            TodoItem {
                content: "next step".into(),
                status: TodoStatus::Pending,
            },
        ];
        let p = PALETTES[0].1;
        let lines = todo_block(&items, Some("14:32"), None, 80, &p);
        // header + one row per item
        assert_eq!(lines.len(), 4);
        let row_text = |i: usize| {
            lines[i]
                .spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        };
        assert!(row_text(0).contains("todo"));
        assert!(row_text(1).contains("✔") && row_text(1).contains("done step"));
        assert!(row_text(2).contains("❯") && row_text(2).contains("current step"));
        assert!(row_text(3).contains("☐") && row_text(3).contains("next step"));
        // Completed content is struck through; pending is not.
        use ratatui::style::Modifier;
        let has_strike = |i: usize| {
            lines[i]
                .spans
                .iter()
                .any(|s| s.style.add_modifier.contains(Modifier::CROSSED_OUT))
        };
        assert!(has_strike(1), "completed row must be struck through");
        assert!(!has_strike(3), "pending row must not be struck through");
    }

    #[test]
    fn fmt_tok_compacts_by_magnitude() {
        assert_eq!(fmt_tok(0), "0");
        assert_eq!(fmt_tok(999), "999");
        assert_eq!(fmt_tok(1_000), "1k");
        assert_eq!(fmt_tok(142_000), "142k");
        assert_eq!(fmt_tok(1_200_000), "1.2M");
    }

    fn permission_prompt() -> Prompt {
        Prompt {
            title: "permission required".into(),
            detail: Some("rm -rf build/".into()),
            options: vec!["Allow once".into(), "Allow always".into(), "Deny".into()],
            allow_free_text: true,
            kind: PromptKind::Permission {
                suggested_glob: "Bash(rm -rf build/)".into(),
            },
        }
    }

    #[test]
    fn permission_allow_once_carries_only_index() {
        let ui = PromptUi::new(permission_prompt()); // selected == 0
        let r = ui.reply();
        assert_eq!(r.index, Some(0));
        assert_eq!(r.text, None);
    }

    #[test]
    fn allow_always_edit_seeds_and_returns_glob() {
        let mut ui = PromptUi::new(permission_prompt());
        ui.down(); // focus "Allow always" (index 1)
        assert!(ui.begin_edit(), "allow-always row must be editable");
        assert_eq!(
            ui.input, "Bash(rm -rf build/)",
            "buffer seeded with suggested glob"
        );
        // User widens the rule.
        ui.input.push('*');
        let r = ui.reply();
        assert_eq!(r.index, Some(1));
        assert_eq!(r.text.as_deref(), Some("Bash(rm -rf build/)*"));
    }

    #[test]
    fn deny_row_is_not_editable_and_denies() {
        let mut ui = PromptUi::new(permission_prompt());
        ui.down();
        ui.down(); // focus "Deny" (index 2)
        assert!(!ui.begin_edit(), "deny row is not editable");
        let r = ui.reply();
        assert_eq!(r.index, Some(2));
        assert_eq!(r.text, None);
    }

    #[test]
    fn free_text_row_returns_text_without_index() {
        let mut ui = PromptUi::new(permission_prompt());
        // Wrap around to the "Other…" row (3 options → row index 3).
        ui.up(); // from 0 wraps to last row (free-text)
        assert!(ui.begin_edit());
        ui.input.push_str("use a different dir");
        let r = ui.reply();
        assert_eq!(r.index, None);
        assert_eq!(r.text.as_deref(), Some("use a different dir"));
    }

    #[test]
    fn question_option_carries_index_no_text() {
        let mut ui = PromptUi::new(Prompt {
            title: "Which library?".into(),
            detail: None,
            options: vec!["pandas".into(), "polars".into()],
            allow_free_text: true,
            kind: PromptKind::Question,
        });
        ui.down(); // focus "polars"
        assert!(!ui.begin_edit(), "plain question option is not editable");
        let r = ui.reply();
        assert_eq!(r.index, Some(1));
        assert_eq!(r.text, None);
    }

    #[test]
    fn question_round_emits_only_one_final_reply() {
        let mut ui = PromptUi::new(Prompt {
            title: "2 questions".into(),
            detail: None,
            options: Vec::new(),
            allow_free_text: false,
            kind: PromptKind::QuestionRound {
                questions: vec![
                    PromptQuestion {
                        id: "db".into(),
                        title: "Database?".into(),
                        detail: None,
                        options: vec!["SQLite".into(), "Postgres".into()],
                        allow_free_text: true,
                    },
                    PromptQuestion {
                        id: "format".into(),
                        title: "Format?".into(),
                        detail: None,
                        options: vec!["JSON".into(), "CSV".into()],
                        allow_free_text: true,
                    },
                ],
            },
        });
        ui.down();
        assert!(ui.confirm().is_none(), "first choice stays local");
        assert_eq!(ui.round_index, 1);
        let reply = ui.confirm().expect("last choice submits the whole round");
        assert_eq!(reply.answers.len(), 2);
        assert_eq!(reply.answers[0].index, Some(1));
        assert_eq!(reply.answers[1].index, Some(0));
    }
}
