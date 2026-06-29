use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};

use super::{
    markdown::highlight_lines,
    theme::{blend, styled, Palette},
};

// ── side-by-side edit diff ──────────────────────────────────────────────────

fn lang_from_path(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "rs" => "rust",
        "py" => "python",
        "js" | "mjs" | "cjs" => "javascript",
        "ts" => "typescript",
        "tsx" => "tsx",
        "jsx" => "jsx",
        "go" => "go",
        "c" | "h" => "c",
        "cpp" | "cc" | "hpp" | "cxx" => "cpp",
        "java" => "java",
        "rb" => "ruby",
        "json" => "json",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "html" => "html",
        "css" => "css",
        "sh" | "bash" => "bash",
        "md" => "markdown",
        _ => "",
    }
}

#[derive(Clone)]
enum Cell {
    Empty, // no line on this side → hatch
    Line {
        n: usize,
        spans: Vec<Span<'static>>,
        changed: bool,
    },
}

struct DiffRow {
    left: Cell,
    right: Cell,
    gap: bool,
}

fn parse_hunk_header(h: &str) -> (usize, usize) {
    let (mut old_n, mut new_n) = (1usize, 1usize);
    for tok in h.split_whitespace() {
        if let Some(r) = tok.strip_prefix('-') {
            old_n = r
                .split(',')
                .next()
                .and_then(|s| s.parse().ok())
                .unwrap_or(1);
        } else if let Some(r) = tok.strip_prefix('+') {
            new_n = r
                .split(',')
                .next()
                .and_then(|s| s.parse().ok())
                .unwrap_or(1);
        }
    }
    (old_n, new_n)
}

fn build_diff_rows(diff: &str, lang: &str, p: &Palette) -> Vec<DiffRow> {
    let lines: Vec<&str> = diff.lines().collect();
    let mut rows = Vec::new();
    let mut first_hunk = true;
    let mut i = 0;
    while i < lines.len() {
        if !lines[i].starts_with("@@") {
            i += 1;
            continue;
        }
        let (mut old_n, mut new_n) = parse_hunk_header(lines[i]);
        i += 1;
        if !first_hunk {
            rows.push(DiffRow {
                left: Cell::Empty,
                right: Cell::Empty,
                gap: true,
            });
        }
        first_hunk = false;

        // hunk body
        let mut ops: Vec<(char, &str)> = Vec::new();
        while i < lines.len() && !lines[i].starts_with("@@") {
            let l = lines[i];
            let tag = l.chars().next().unwrap_or(' ');
            if matches!(tag, '+' | '-' | ' ') {
                ops.push((tag, &l[1..]));
            }
            i += 1;
        }

        // reconstruct old/new fragments for context-aware highlighting
        let old_code = ops
            .iter()
            .filter(|(t, _)| *t == ' ' || *t == '-')
            .map(|(_, s)| *s)
            .collect::<Vec<_>>()
            .join("\n");
        let new_code = ops
            .iter()
            .filter(|(t, _)| *t == ' ' || *t == '+')
            .map(|(_, s)| *s)
            .collect::<Vec<_>>()
            .join("\n");
        let old_hl = highlight_lines(lang, &old_code, p);
        let new_hl = highlight_lines(lang, &new_code, p);

        let (mut oi, mut ni, mut j) = (0usize, 0usize, 0usize);
        while j < ops.len() {
            if ops[j].0 == ' ' {
                let l = Cell::Line {
                    n: old_n,
                    spans: old_hl.get(oi).cloned().unwrap_or_default(),
                    changed: false,
                };
                let r = Cell::Line {
                    n: new_n,
                    spans: new_hl.get(ni).cloned().unwrap_or_default(),
                    changed: false,
                };
                rows.push(DiffRow {
                    left: l,
                    right: r,
                    gap: false,
                });
                oi += 1;
                ni += 1;
                old_n += 1;
                new_n += 1;
                j += 1;
            } else {
                let dels = {
                    let s = j;
                    while j < ops.len() && ops[j].0 == '-' {
                        j += 1;
                    }
                    j - s
                };
                let inss = {
                    let s = j;
                    while j < ops.len() && ops[j].0 == '+' {
                        j += 1;
                    }
                    j - s
                };
                for k in 0..dels.max(inss) {
                    let left = if k < dels {
                        let c = Cell::Line {
                            n: old_n,
                            spans: old_hl.get(oi).cloned().unwrap_or_default(),
                            changed: true,
                        };
                        oi += 1;
                        old_n += 1;
                        c
                    } else {
                        Cell::Empty
                    };
                    let right = if k < inss {
                        let c = Cell::Line {
                            n: new_n,
                            spans: new_hl.get(ni).cloned().unwrap_or_default(),
                            changed: true,
                        };
                        ni += 1;
                        new_n += 1;
                        c
                    } else {
                        Cell::Empty
                    };
                    rows.push(DiffRow {
                        left,
                        right,
                        gap: false,
                    });
                }
            }
        }
    }
    rows
}

/// Render one diff cell to exactly `col_w` visible columns.
fn render_cell(cell: &Cell, col_w: usize, change_bg: Color, p: &Palette) -> Vec<Span<'static>> {
    const GW: usize = 4; // gutter: 3-digit line number + space
    match cell {
        Cell::Empty => {
            let hatch = blend(p.muted, p.bg, 0.45);
            vec![Span::styled(
                "╱".repeat(col_w),
                Style::default().fg(hatch).bg(p.bg),
            )]
        }
        Cell::Line { n, spans, changed } => {
            let bg = if *changed { Some(change_bg) } else { None };
            let with_bg = |s: Style| if let Some(c) = bg { s.bg(c) } else { s };
            let code_w = col_w.saturating_sub(GW);
            let mut out = vec![Span::styled(
                format!("{:>w$} ", n, w = GW - 1),
                with_bg(Style::default().fg(p.muted)),
            )];
            let mut used = 0usize;
            for s in spans {
                if used >= code_w {
                    break;
                }
                let cnt = s.content.chars().count();
                let take = cnt.min(code_w - used);
                let txt: String = s.content.chars().take(take).collect();
                out.push(Span::styled(txt, with_bg(s.style)));
                used += take;
            }
            if used < code_w {
                out.push(Span::styled(
                    " ".repeat(code_w - used),
                    with_bg(Style::default()),
                ));
            }
            out
        }
    }
}

/// Render an Edit tool diff as a side-by-side (old | new) box, themed dynamically.
pub fn edit_diff_block(path: &str, diff: &str, w: usize, p: &Palette) -> Vec<Line<'static>> {
    let lang = lang_from_path(path);
    let rows = build_diff_rows(diff, lang, p);
    let col_w = w.saturating_sub(9) / 2;
    let fname = path.rsplit('/').next().unwrap_or(path);
    if col_w < 6 {
        // Too narrow for two columns — unified single-column fallback instead
        // of silently rendering nothing.
        let mut out = vec![Line::from(Span::styled(
            format!("▸ edit · {fname}"),
            styled(p.info, true),
        ))];
        for l in diff.lines() {
            let color = if l.starts_with("@@") {
                p.purple
            } else if l.starts_with('+') {
                p.success
            } else if l.starts_with('-') {
                p.err
            } else {
                p.fg
            };
            out.push(Line::from(Span::styled(
                l.chars().take(w).collect::<String>(),
                styled(color, false),
            )));
        }
        return out;
    }
    let mid = col_w + 2;
    let red_bg = blend(p.err, p.bg, 0.70);
    let green_bg = blend(p.success, p.bg, 0.70);
    let border = styled(p.muted, false);
    let raw_label = format!(" edit · {fname} ");
    let label: String = if raw_label.chars().count() > mid {
        raw_label.chars().take(mid).collect()
    } else {
        raw_label
    };
    let left_dashes = mid - label.chars().count();

    let mut out = vec![Line::from(vec![
        Span::styled("  ╭", border),
        Span::styled(label, styled(p.info, true)),
        Span::styled(
            format!("{}┬{}╮", "─".repeat(left_dashes), "─".repeat(mid)),
            border,
        ),
    ])];
    for row in &rows {
        if row.gap {
            out.push(Line::from(Span::styled(
                format!("  ├{}┼{}┤", "┄".repeat(mid), "┄".repeat(mid)),
                styled(blend(p.muted, p.bg, 0.3), false),
            )));
            continue;
        }
        let mut spans = vec![Span::styled("  │ ", border)];
        spans.extend(render_cell(&row.left, col_w, red_bg, p));
        spans.push(Span::styled(" │ ", border));
        spans.extend(render_cell(&row.right, col_w, green_bg, p));
        spans.push(Span::styled(" │", border));
        out.push(Line::from(spans));
    }
    out.push(Line::from(Span::styled(
        format!("  ╰{}┴{}╯", "─".repeat(mid), "─".repeat(mid)),
        border,
    )));
    out
}
