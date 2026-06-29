use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
    Frame,
};

use super::{
    app::{format_tool_input, AgentStatus, App, Focus, PopupContent},
    boar::BrailleWidget,
    markdown::md_to_lines,
    theme::{ctx_usage_color, styled, PALETTES},
    widgets::{
        build_kb_lines, fmt_elapsed, fmt_tok, job_gauge, render_confirm_dialog,
        render_scroll_indicators, running_tool_block, thread_blank, thread_wrap,
        timeline_entry_lines, timeline_tree_parts, KB_PANEL_W, THREAD_GUTTER,
    },
};

const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

impl App {
    pub(super) fn render(&mut self, f: &mut Frame) {
        if self.about_mode {
            self.render_about(f);
            return;
        }
        if self.settings_mode {
            self.render_settings(f);
            return;
        }

        let [output_area, status_area, input_area, info_area] = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(3),
                Constraint::Length(1),
                Constraint::Length(3),
                Constraint::Length(1),
            ])
            .areas(f.area());

        let out_focused = self.focus == Focus::Output;
        let p = self.palette;
        let focus_color = |focused: bool| if focused { p.accent } else { p.muted };

        let (chat_area, timeline_panel) = if self.panel_visible {
            let [chat, panel]: [Rect; 2] = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Min(20), Constraint::Length(self.panel_w)])
                .areas(output_area);
            (chat, Some(panel))
        } else {
            (output_area, None)
        };
        self.chat_area = chat_area;

        let sp = SPINNER[(self.spinner_tick / 6) as usize % SPINNER.len()];
        // Transient tail rebuilt each frame: streaming text + in-flight tool
        // boxes. Stored history (`self.lines`) is never cloned wholesale.
        let mut tail: Vec<Line<'static>> = Vec::new();
        let mut rail_open = self.thread_active;
        let cw = self.chat_w().saturating_sub(THREAD_GUTTER);
        if self.revealed > 0 {
            // Live markdown of the revealed slice. md_to_lines wraps to `cw`, so
            // visual rows == logical lines (scroll math + hit-testing stay exact).
            // Cache by (revealed, cw): skip the re-parse when the network stalls.
            let fresh = !matches!(&self.tail_cache,
                Some((r, w, _)) if *r == self.revealed && *w == cw);
            if fresh {
                let shown: String = self.pending.chars().take(self.revealed).collect();
                let lines = md_to_lines(&shown, cw, self.palette);
                self.tail_cache = Some((self.revealed, cw, lines));
            }
            let mut blk = self
                .tail_cache
                .as_ref()
                .map(|(_, _, l)| l.clone())
                .unwrap_or_default();
            match blk.last_mut() {
                Some(last) => last.spans.push(Span::raw("▌")),
                None => blk.push(Line::from(Span::raw("▌"))),
            }
            tail.push(if rail_open {
                thread_blank(p)
            } else {
                Line::default()
            });
            tail.extend(thread_wrap(blk, true, p));
            rail_open = true;
        }
        // In-flight tools: transient "running" boxes, replaced by the final
        // box when ToolCallEnd lands.
        for (_, name, input, t0, clock) in &self.pending_tool_inputs {
            let blk = running_tool_block(
                name,
                &format_tool_input(input),
                sp,
                Some(clock),
                t0.elapsed(),
                cw,
                p,
            );
            tail.push(if rail_open {
                thread_blank(p)
            } else {
                Line::default()
            });
            tail.extend(thread_wrap(blk, true, p));
            rail_open = true;
        }

        let total_n = self.lines.len() + tail.len();
        let total = total_n.min(u16::MAX as usize) as u16;
        let viewport = chat_area.height.saturating_sub(2);
        let max_scroll = total.saturating_sub(viewport);
        self.max_scroll = max_scroll;
        // Keep self.scroll in sync with what's on screen so mouse hit-testing
        // and the first ↑ after auto-scroll start from the visible position.
        // A trail-click jump (`scroll_to`) scrolls the chat toward the target,
        // clamped to max_scroll (the last steps are already at the bottom — the
        // trail highlight, not the scroll, signals which one was picked).
        self.scroll = if let Some(t) = self.scroll_to.take() {
            self.auto_scroll = false;
            t.min(max_scroll)
        } else if self.auto_scroll {
            max_scroll
        } else {
            self.scroll.min(max_scroll)
        };
        let scroll = self.scroll;
        // Clone only the visible window — scrolling a long chat stays cheap.
        let start = scroll as usize;
        let end = (start + viewport as usize).min(total_n);
        let visible: Vec<Line<'static>> = (start..end)
            .map(|i| {
                if i < self.lines.len() {
                    self.lines[i].clone()
                } else {
                    tail[i - self.lines.len()].clone()
                }
            })
            .collect();

        f.render_widget(
            Paragraph::new(visible).wrap(Wrap { trim: false }).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(Span::styled(" sirbone ", styled(p.muted, false)))
                    .border_style(Style::default().fg(if out_focused {
                        p.muted
                    } else {
                        p.border
                    })),
            ),
            chat_area,
        );
        render_scroll_indicators(f, chat_area, scroll, max_scroll, self.auto_scroll, p);

        if let Some(panel) = timeline_panel {
            self.render_timeline(f, panel);
        } else {
            self.timeline_area = Rect::default();
        }

        // Shortcuts: always visible, pinned to the far right of the status row.
        // The left side carries the run state (idle hint or busy spinner).
        let shortcuts =
            "Tab focus  ↑↓ scroll  ⌥B trail  ⌥P palette  ⌥A about  ⌥S settings  ^C quit";
        let busy_for = self
            .busy_since
            .map(|t0| format!(" {}", fmt_elapsed(t0.elapsed())))
            .unwrap_or_default();
        let busy_line = |label: &str| {
            Line::from(vec![
                Span::styled(format!(" {sp} "), styled(p.accent, false)),
                Span::styled(format!("{label}…{busy_for}"), styled(p.fg, false)),
                Span::styled("   Esc cancel", styled(p.muted, false)),
            ])
        };
        let left_line = match &self.status {
            AgentStatus::Idle => {
                let hint = if self.tool_boxes.is_empty() {
                    ""
                } else {
                    "  · click box to expand"
                };
                Line::from(Span::styled(format!("  {hint}"), styled(p.muted, false)))
            }
            AgentStatus::LlmThinking => {
                let mut line = busy_line("thinking");
                // Live tail of the thinking stream, dimmed.
                let tail = self
                    .thinking_tail
                    .lines()
                    .rev()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("")
                    .trim();
                if !tail.is_empty() {
                    let t: String = tail.chars().take(60).collect();
                    line.spans
                        .push(Span::styled(format!("  ·  {t}"), styled(p.muted, false)));
                }
                line
            }
            AgentStatus::ToolRunning(name) => busy_line(name),
        };
        // Split the row: state on the left, shortcuts right-aligned on the right.
        let sc_w = (shortcuts.chars().count() as u16 + 2).min(status_area.width);
        let [left_area, right_area] = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(0), Constraint::Length(sc_w)])
            .areas(status_area);
        f.render_widget(Paragraph::new(left_line), left_area);
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!("{shortcuts} "),
                styled(p.muted, false),
            )))
            .alignment(Alignment::Right),
            right_area,
        );

        let inp_title = if let Some(q) = &self.queued_input {
            // Show what's queued so it can be reviewed — Esc drops it.
            let prev: String = q.chars().take(40).collect();
            let ell = if q.chars().count() > 40 { "…" } else { "" };
            Span::styled(
                format!(" queued: {prev}{ell} · Esc drop "),
                styled(p.accent, false),
            )
        } else if self.busy {
            Span::styled(" Esc cancel ", styled(p.muted, false))
        } else {
            Span::styled(" › ", styled(p.accent, false))
        };
        // Horizontal window over the input so the cursor stays visible when the
        // text outgrows the box.
        let inner_w = input_area.width.saturating_sub(2) as usize;
        let input_skip = self.cursor_pos.saturating_sub(inner_w.saturating_sub(1));
        let input_visible: String = self.input.chars().skip(input_skip).take(inner_w).collect();
        f.render_widget(
            Paragraph::new(Span::styled(input_visible, Style::default().fg(p.fg))).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .title(inp_title)
                    .border_style(Style::default().fg(focus_color(!out_focused))),
            ),
            input_area,
        );

        // ── @ completion popup ────────────────────────────────
        if self.completion.active && !self.completion.items.is_empty() {
            let max_vis = 8_u16;
            let count = (self.completion.items.len() as u16).min(max_vis);
            let popup_h = count + 2; // +2 for border
            let popup_w = input_area.width.saturating_sub(2).min(60);
            let popup_y = input_area.y.saturating_sub(popup_h);
            let popup_area = Rect::new(input_area.x + 1, popup_y, popup_w, popup_h).clamp(f.area());
            f.render_widget(Clear, popup_area);
            let items: Vec<ListItem> = self
                .completion
                .items
                .iter()
                .take(max_vis as usize)
                .map(|s| ListItem::new(s.as_str()))
                .collect();
            let list = List::new(items)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded)
                        .title(Span::styled(
                            self.completion.popup_title(),
                            styled(p.muted, false),
                        ))
                        .border_style(Style::default().fg(p.border)),
                )
                .highlight_style(Style::default().bg(p.border).fg(p.accent));
            let mut list_state = ListState::default().with_selected(Some(self.completion.selected));
            f.render_stateful_widget(list, popup_area, &mut list_state);
        }

        if self.focus == Focus::Input {
            let max_x = input_area.x + input_area.width.saturating_sub(2);
            let cx = (input_area.x + 1 + (self.cursor_pos - input_skip) as u16).min(max_x);
            f.set_cursor_position((cx, input_area.y + 1));
        }

        self.render_info_bar(f, info_area);
        self.render_popup(f);
        if let Some(cmd) = &self.confirm_request {
            render_confirm_dialog(f, cmd, self.palette);
        }
        if self.show_logs {
            self.render_logs(f);
        }
    }

    /// F12 debug overlay: the tui-logger buffer (tracing events) in a centered
    /// box. Unfiltered — shows everything captured since startup.
    fn render_logs(&self, f: &mut Frame) {
        let p = self.palette;
        let full = f.area();
        let w = full.width.saturating_sub(8).min(120);
        let h = full.height.saturating_sub(4).max(6);
        let area = Rect::new(
            full.x + (full.width.saturating_sub(w)) / 2,
            full.y + (full.height.saturating_sub(h)) / 2,
            w,
            h,
        );
        f.render_widget(Clear, area);
        let widget = tui_logger::TuiLoggerWidget::default()
            .block(
                Block::bordered()
                    .border_type(BorderType::Rounded)
                    .title(Span::styled(
                        " logs — F12 to close ",
                        styled(p.muted, false),
                    ))
                    .border_style(Style::default().fg(p.border)),
            )
            .output_separator(' ')
            .output_timestamp(Some("%H:%M:%S".to_string()))
            .output_level(Some(tui_logger::TuiLoggerLevelOutput::Abbreviated))
            .output_target(true)
            .output_file(false)
            .output_line(false)
            .style_error(Style::default().fg(p.err))
            .style_warn(Style::default().fg(p.info))
            .style_info(Style::default().fg(p.success))
            .style_debug(Style::default().fg(p.muted))
            .style_trace(Style::default().fg(p.muted))
            .style(Style::default().fg(p.fg).bg(p.bg));
        f.render_widget(widget, area);
    }

    /// Side panel: a clickable summary of the run — user messages and tool
    /// calls in order. Entries wrap to multiple lines and the panel scrolls on
    /// its own (`trail_scroll`, mouse-wheel while hovering it). Per-row entry
    /// indices (`timeline_rows`) map a click back to an entry; the left border
    /// lights up (`hover_divider`/`resizing_panel`) to mark the resize grab zone.
    fn render_timeline(&mut self, f: &mut Frame, panel: Rect) {
        let p = self.palette;
        let grab = self.hover_divider || self.resizing_panel;
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(Span::styled(" Chronika ", styled(p.muted, false)))
            .border_style(Style::default().fg(if grab { p.accent } else { p.border }));
        let inner = block.inner(panel);
        f.render_widget(block, panel);
        self.timeline_area = inner;

        let rows = inner.height as usize;
        let w = inner.width as usize;
        // Highlight the clicked entry if any (it may sit below the reachable
        // scroll), else auto-follow the chat: greatest target at/above the top.
        let selected = self.selected_entry.or_else(|| {
            self.timeline
                .iter()
                .rposition(|e| e.target <= self.scroll as usize)
        });

        // Build every entry's wrapped lines (chronological) so the panel can
        // scroll back through history, not just show what fits vertically.
        let mut lines: Vec<Line<'static>> = Vec::new();
        let mut owners: Vec<usize> = Vec::new();
        for (idx, e) in self.timeline.iter().enumerate() {
            // A tool is the last child of its turn when the next entry is a user
            // message (or there is none) → closing `└` branch instead of `├`.
            let last_child = self.timeline.get(idx + 1).is_none_or(|n| n.is_user);
            let (prefix, cont, body) =
                timeline_tree_parts(e.is_user, &e.label, &e.clock, last_child);
            for line in timeline_entry_lines(prefix, cont, &body, selected == Some(idx), w, p) {
                lines.push(line);
                owners.push(idx);
            }
        }
        // Bottom-anchored window: trail_scroll counts lines up from the bottom.
        self.trail_max_scroll = (lines.len().saturating_sub(rows)) as u16;
        self.trail_scroll = self.trail_scroll.min(self.trail_max_scroll);
        let end = lines.len().saturating_sub(self.trail_scroll as usize);
        let start = end.saturating_sub(rows);
        self.timeline_rows = owners[start..end].to_vec();
        f.render_widget(Paragraph::new(lines[start..end].to_vec()), inner);
    }

    fn render_popup(&mut self, f: &mut Frame) {
        let Some(scroll_req) = self.popup.as_ref().map(|pop| pop.scroll) else {
            return;
        };
        let p = self.palette;
        let area = f.area();

        let pw = (area.width * 4 / 5).clamp(40, 120);
        let ph = (area.height * 7 / 10).clamp(8, 50);
        let popup_area = Rect::new(
            area.x + area.width.saturating_sub(pw) / 2,
            area.y + area.height.saturating_sub(ph) / 2,
            pw,
            ph,
        );

        let Some((lines, title)) = self.popup.as_ref().and_then(|pop| match &pop.content {
            PopupContent::Tool { tool_idx } => {
                let tb = self.tool_boxes.get(*tool_idx)?;
                let mut lines: Vec<Line<'static>> = Vec::new();
                if !tb.full_input.is_empty() && tb.full_input != "null" {
                    lines.push(Line::from(vec![
                        Span::styled("IN  ", styled(p.muted, true)),
                        Span::styled(tb.full_input.clone(), styled(p.fg, false)),
                    ]));
                    lines.push(Line::default());
                }
                for (i, raw_line) in tb.full_output.lines().enumerate() {
                    let lbl = if i == 0 { "OUT " } else { "    " };
                    let color = if raw_line.starts_with("@@") {
                        p.purple
                    } else if raw_line.starts_with('+') {
                        p.success
                    } else if raw_line.starts_with('-') {
                        p.err
                    } else {
                        p.fg
                    };
                    lines.push(Line::from(vec![
                        Span::styled(lbl, styled(p.muted, i == 0)),
                        Span::styled(raw_line.to_string(), styled(color, false)),
                    ]));
                }
                Some((lines, format!(" ▸ {} ", tb.name)))
            }
            PopupContent::Text { title, lines } => Some((
                lines
                    .iter()
                    .map(|line| Line::from(Span::styled(line.clone(), styled(p.fg, false))))
                    .collect(),
                format!(" {title} "),
            )),
        }) else {
            return;
        };

        // Clamp so scrolling stops at the last content line.
        let viewport = ph.saturating_sub(2);
        let scroll = scroll_req.min((lines.len() as u16).saturating_sub(viewport));
        if let Some(pop) = &mut self.popup {
            pop.scroll = scroll;
        }

        f.render_widget(Clear, popup_area);
        f.render_widget(
            Paragraph::new(lines)
                .scroll((scroll, 0))
                .wrap(Wrap { trim: false })
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(styled(p.accent, false))
                        .title(Span::styled(title, styled(p.fg, true)))
                        .title_bottom(Span::styled(
                            " ↑↓ scroll  Esc/click close ",
                            styled(p.muted, false),
                        )),
                ),
            popup_area,
        );
    }

    fn render_info_bar(&self, f: &mut Frame, area: Rect) {
        let p = self.palette;
        let filled = (self.ctx_pct as usize * 10 / 100).min(10);
        let bar = format!("{}{}", "█".repeat(filled), "░".repeat(10 - filled));
        // Pastel semantic colour: green ok → amber → red near the 87.5% compaction line.
        let ctx_color = ctx_usage_color(self.ctx_pct);
        let palette_name = PALETTES[self.palette_idx].0;
        let mut spans = vec![
            Span::raw("  "),
            Span::styled(self.provider.clone(), styled(p.muted, false)),
            Span::styled("  ·  ", styled(p.muted, false)),
            Span::styled(self.model.clone(), styled(p.accent, false)),
            Span::styled("  ·  ctx ", styled(p.muted, false)),
            Span::styled(bar, Style::default().fg(ctx_color)),
            Span::styled(
                format!(" {}%", self.ctx_pct),
                Style::default().fg(ctx_color),
            ),
        ];
        // Prompt-cache hit share of the last request — stays absent until the
        // provider reports a hit, so a silent cache invalidator is visible.
        if self.cached_pct > 0 {
            spans.push(Span::styled(
                format!(" ⚡{}%", self.cached_pct),
                styled(p.muted, false),
            ));
        }
        // Token spend cap (Feature C): `tok 142k/500k`, coloured by share of cap.
        // Only shown when the cap is enabled.
        if let Some(cap) = self.spend_cap {
            let pct = ((self.spend_tokens * 100) / cap.max(1)).min(100) as u8;
            spans.push(Span::styled(
                format!("  ·  tok {}/{}", fmt_tok(self.spend_tokens), fmt_tok(cap)),
                Style::default().fg(ctx_usage_color(pct)),
            ));
        }
        // Estimated 5-hour quota-window span (start→end), before the theme label.
        if let Some((from, to)) = self.quota_window_clocks() {
            spans.push(Span::styled(
                format!("  ·  win {from}→{to}"),
                styled(p.muted, false),
            ));
        }
        spans.push(Span::styled(
            format!("  ·  theme:{palette_name}"),
            styled(p.muted, false),
        ));
        // Live background jobs: one job shows its command + elapsed (and a
        // progress bar + ETA when the job emits markers), several show a count.
        match self.jobs_running.as_slice() {
            [] => {}
            [(id, cmd, dur, progress)] => {
                let c: String = cmd.chars().take(24).collect();
                let ell = if cmd.chars().count() > 24 { "…" } else { "" };
                spans.push(Span::styled(
                    format!("  ·  ⚙ #{id} {c}{ell} {}", job_gauge(*dur, *progress)),
                    styled(p.info, false),
                ));
            }
            many => {
                let oldest = many.iter().map(|(_, _, d, _)| *d).max().unwrap_or_default();
                spans.push(Span::styled(
                    format!("  ·  ⚙ {} jobs {}", many.len(), fmt_elapsed(oldest)),
                    styled(p.info, false),
                ));
            }
        }
        f.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn render_about(&self, f: &mut Frame) {
        let area = f.area();
        let p = self.palette;
        f.render_widget(Clear, area);

        let [kb_area, boar_area]: [Rect; 2] = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(KB_PANEL_W), Constraint::Min(0)])
            .areas(area);

        let kb_block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(p.accent))
            .title(Span::styled(" keybindings ", styled(p.muted, false)));
        let kb_inner = kb_block.inner(kb_area);
        f.render_widget(kb_block, kb_area);
        f.render_widget(
            Paragraph::new(build_kb_lines(p)).wrap(Wrap { trim: false }),
            kb_inner,
        );

        let boar_block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(p.accent))
            .title(Span::styled(" sirbone ", styled(p.muted, false)));
        let boar_inner = boar_block.inner(boar_area);
        f.render_widget(boar_block, boar_area);

        let boar_y = boar_inner.y + boar_inner.height.saturating_sub(self.boar.h) / 2;
        let fi = self.boar.frame_idx();
        let frames = self.boar.current_frames();
        if fi < frames.len() {
            f.render_widget(
                BrailleWidget {
                    frame: &frames[fi],
                    x_offset: self.boar.x,
                },
                Rect {
                    x: boar_inner.x,
                    y: boar_y,
                    width: boar_inner.width,
                    height: self.boar.h.min(boar_inner.height),
                },
            );
        }

        if boar_inner.height > 0 {
            f.render_widget(
                Paragraph::new(Span::styled("  any key → chat", styled(p.muted, false))),
                Rect {
                    x: boar_inner.x,
                    y: boar_inner.y + boar_inner.height.saturating_sub(1),
                    width: boar_inner.width,
                    height: 1,
                },
            );
        }
    }

    fn render_settings(&self, f: &mut Frame) {
        if self.picker.is_some() {
            self.render_picker(f);
            return;
        }
        let area = f.area();
        let p = self.palette;
        f.render_widget(Clear, area);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(p.accent))
            .title(Span::styled(" settings ", styled(p.muted, false)));
        let inner = block.inner(area);
        f.render_widget(block, area);

        let on = |b: bool| if b { "ON" } else { "OFF" };
        let cur = self.settings_cursor;
        let arch = match &self.architect {
            Some(g) => {
                if g.load(std::sync::atomic::Ordering::Relaxed) {
                    "ON"
                } else {
                    "OFF"
                }
            }
            None => "not configured",
        };
        let think = match self.thinking_budget {
            None => "off".to_string(),
            Some(b) => format!("{}k", b / 1000),
        };
        // One per SETTINGS_ROWS entry, in order: (label, help shown when focused).
        let rows: [(String, &str); 8] = [
            (
                format!("Localize pre-pass:  {}", on(self.localize)),
                "Run the localization pre-pass before each turn.",
            ),
            (
                format!("Plan mode:  {}", on(self.plan)),
                "Record a SPEC via the plan tool before editing code.",
            ),
            (
                format!("Oracle gate:  {}", on(self.oracle)),
                "After the model finishes, run the test gate (loop + rollback).",
            ),
            (
                format!("Quota window bar:  {}", on(self.quota_bar)),
                "Show the estimated 5-hour quota window (start→end) in the info bar.",
            ),
            (
                format!("Architect:  {arch}"),
                "Consult a second model for a design opinion each turn.",
            ),
            (
                format!("Thinking budget:  {think}"),
                "Extended-thinking token budget — cycle off / 8k / 16k / 32k.",
            ),
            (
                "Skills".to_string(),
                "Choose which skills load for this project.",
            ),
            (
                "MCP".to_string(),
                "Choose which MCP servers load for this project.",
            ),
        ];
        let mut lines = vec![Line::default()];
        for (i, (label, help)) in rows.iter().enumerate() {
            let here = i == cur;
            let mark = if here { "›" } else { " " };
            lines.push(Line::from(vec![
                Span::styled(format!("  {mark}  "), styled(p.accent, here)),
                Span::styled(label.clone(), styled(p.fg, here)),
            ]));
            if here {
                lines.push(Line::from(Span::styled(
                    format!("        {help}"),
                    styled(p.muted, false),
                )));
            }
        }
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            "  provider / model (read-only, set via env):",
            styled(p.muted, false),
        )));
        lines.push(Line::from(Span::styled(
            format!("    {} · {}", self.provider, self.model),
            styled(p.muted, false),
        )));
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            "  ↑↓ move · ←/→/space toggle · esc → chat",
            styled(p.muted, false),
        )));
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
    }

    fn render_picker(&self, f: &mut Frame) {
        use super::app::PickerKind;
        let Some(picker) = &self.picker else { return };
        let area = f.area();
        let p = self.palette;
        f.render_widget(Clear, area);

        let (title, empty_hint) = match picker.kind {
            PickerKind::Skills => (
                " settings · skills ",
                "  no skills found in ~/.sirbone/skills or .sirbone/skills",
            ),
            PickerKind::Mcp => (
                " settings · mcp ",
                "  no MCP servers defined in ~/.sirbone/mcp.json",
            ),
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(p.accent))
            .title(Span::styled(title, styled(p.muted, false)));
        let inner = block.inner(area);
        f.render_widget(block, area);

        let mut lines = vec![
            Line::from(Span::styled(
                "  ↑↓ move · space toggle · esc back · applies next launch",
                styled(p.muted, false),
            )),
            Line::default(),
        ];
        if picker.rows.is_empty() {
            lines.push(Line::from(Span::styled(empty_hint, styled(p.muted, false))));
        }
        for (i, r) in picker.rows.iter().enumerate() {
            let here = i == picker.cursor;
            let mark = if r.enabled { "[x]" } else { "[ ]" };
            let cur = if here { "›" } else { " " };
            let name_color = if r.enabled { p.fg } else { p.muted };
            lines.push(Line::from(vec![
                Span::styled(format!(" {cur} {mark} "), styled(p.accent, here)),
                Span::styled(r.name.clone(), styled(name_color, here)),
                Span::styled(r.tag.clone(), styled(p.muted, false)),
            ]));
            if here && !r.description.is_empty() {
                lines.push(Line::from(Span::styled(
                    format!("        {}", r.description),
                    styled(p.muted, false),
                )));
            }
        }
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
    }
}
