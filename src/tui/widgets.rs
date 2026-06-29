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
    scroll: u16,
    max_scroll: u16,
    auto_scroll: bool,
    p: &Palette,
) {
    if max_scroll == 0 {
        return;
    }
    let mut state = ScrollbarState::new(max_scroll as usize).position(scroll as usize);
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

/// Centered y/n dialog for a destructive command awaiting approval.
/// Shared by the real TUI and the mock so the two can't drift.
pub fn render_confirm_dialog(f: &mut Frame, cmd: &str, p: &Palette) {
    let area = f.area();
    let pw = (area.width * 3 / 5).clamp(30, 100);
    let inner_w = pw.saturating_sub(4) as usize;
    let cmd_lines = wrap_text(cmd, inner_w);
    let ph = (cmd_lines.len() as u16 + 4).min(area.height);
    let popup_area = Rect::new(
        area.x + area.width.saturating_sub(pw) / 2,
        area.y + area.height.saturating_sub(ph) / 2,
        pw,
        ph,
    );
    f.render_widget(Clear, popup_area);

    let (header, title, border, verb) = (
        " the agent wants to run:",
        " ⚠ destructive command ",
        p.err,
        "allow",
    );

    let mut lines = vec![Line::from(Span::styled(header, styled(p.muted, false)))];
    for l in cmd_lines {
        lines.push(Line::from(Span::styled(
            format!("   {l}"),
            styled(p.fg, true),
        )));
    }
    lines.push(Line::default());
    lines.push(Line::from(vec![
        Span::styled(" y ", styled(p.success, true)),
        Span::styled(format!("{verb}   "), styled(p.muted, false)),
        Span::styled("n/Esc ", styled(p.err, true)),
        Span::styled("cancel", styled(p.muted, false)),
    ]));

    f.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(styled(border, false))
                .title(Span::styled(title, styled(border, true))),
        ),
        popup_area,
    );
}

#[cfg(test)]
mod tests {
    use super::fmt_tok;

    #[test]
    fn fmt_tok_compacts_by_magnitude() {
        assert_eq!(fmt_tok(0), "0");
        assert_eq!(fmt_tok(999), "999");
        assert_eq!(fmt_tok(1_000), "1k");
        assert_eq!(fmt_tok(142_000), "142k");
        assert_eq!(fmt_tok(1_200_000), "1.2M");
    }
}
