use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use ratatui_markdown::markdown::{MarkdownRenderer, RenderHooks};
use synoptic::{from_extension, TokOpt};

use crate::highlight::{lang_to_ext, syn_role, SynRole};

use super::theme::{Palette, SirboneTheme};

pub struct PiHooks {
    pub width: usize,
    pub palette: &'static Palette,
}

impl RenderHooks for PiHooks {
    fn heading1(&self, text: &str) -> Option<Line<'static>> {
        let p = self.palette;
        Some(Line::from(vec![
            Span::styled("  ▌ ", Style::default().fg(p.accent)),
            Span::styled(
                text.to_owned(),
                Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
            ),
        ]))
    }
    fn heading2(&self, text: &str) -> Option<Line<'static>> {
        let p = self.palette;
        Some(Line::from(vec![
            Span::styled("  ╌ ", Style::default().fg(p.muted)),
            Span::styled(
                text.to_owned(),
                Style::default().fg(p.fg).add_modifier(Modifier::BOLD),
            ),
        ]))
    }
    fn heading3(&self, text: &str) -> Option<Line<'static>> {
        Some(Line::from(vec![
            Span::styled("    ", Style::default()),
            Span::styled(
                text.to_owned(),
                Style::default()
                    .fg(self.palette.muted)
                    .add_modifier(Modifier::BOLD),
            ),
        ]))
    }

    fn render_code_block(&self, lang: &str, code: &str) -> Option<Vec<Line<'static>>> {
        let p = self.palette;
        let w = self.width;
        let border = Style::default().fg(p.muted);

        if lang == "mermaid" {
            let content_zone = w.saturating_sub(5);
            let top = format!("╭─ mermaid {}╮", "─".repeat(w.saturating_sub(13)));
            let bottom = format!("╰{}╯", "─".repeat(w.saturating_sub(3)));

            if let Some(ml) = ratatui_markdown::mermaid::render_mermaid(
                code,
                content_zone,
                None,
                &SirboneTheme { palette: p },
            ) {
                let diag_w: usize = ml
                    .iter()
                    .map(|l| {
                        l.spans
                            .iter()
                            .map(|s| s.content.chars().count())
                            .sum::<usize>()
                    })
                    .max()
                    .unwrap_or(0);
                let left_pad = content_zone.saturating_sub(diag_w) / 2;
                let mut out = vec![Line::from(Span::styled(top, border))];
                for m in ml {
                    let line_w: usize = m.spans.iter().map(|s| s.content.chars().count()).sum();
                    let right_pad = content_zone.saturating_sub(left_pad + line_w);
                    let mut spans: Vec<Span<'static>> = vec![Span::styled("│ ", border)];
                    if left_pad > 0 {
                        spans.push(Span::raw(" ".repeat(left_pad)));
                    }
                    spans.extend(m.spans);
                    if right_pad > 0 {
                        spans.push(Span::raw(" ".repeat(right_pad)));
                    }
                    spans.push(Span::styled(" │", border));
                    out.push(Line::from(spans));
                }
                out.push(Line::from(Span::styled(bottom, border)));
                return Some(out);
            } else {
                let mut out = vec![Line::from(Span::styled(top, border))];
                for line in code.lines() {
                    let text: String = line.chars().take(content_zone).collect();
                    let right_pad = content_zone.saturating_sub(text.chars().count());
                    let mut spans = vec![
                        Span::styled("│ ", border),
                        Span::styled(text, Style::default().fg(p.fg)),
                    ];
                    if right_pad > 0 {
                        spans.push(Span::raw(" ".repeat(right_pad)));
                    }
                    spans.push(Span::styled(" │", border));
                    out.push(Line::from(spans));
                }
                out.push(Line::from(Span::styled(bottom, border)));
                return Some(out);
            }
        }

        // Syntax highlighting via synoptic (regex-based, no tree-sitter).
        if lang.is_empty() {
            return None;
        }
        let line_spans = synoptic_spans(lang, code, p)?; // unknown lang → default renderer

        let label = format!(" {lang} ");
        let fill = w.saturating_sub(4 + label.chars().count());
        let top = format!("╭─{}{}╮", label, "─".repeat(fill));
        let bottom = format!("╰{}╯", "─".repeat(w.saturating_sub(3)));
        let content_width = w.saturating_sub(2);

        let mut out = vec![Line::from(Span::styled(top, border))];
        for spans in line_spans {
            push_code_line(&mut out, spans, border, content_width);
        }
        out.push(Line::from(Span::styled(bottom, border)));
        Some(out)
    }
}

/// Append one highlighted source line to `out` as boxed lines: a `"│ "` gutter
/// prefix, wrapping at `max_width` (char-count width; code is ~ASCII).
fn push_code_line(
    out: &mut Vec<Line<'static>>,
    spans: Vec<Span<'static>>,
    border: Style,
    max_width: usize,
) {
    const PREFIX_W: usize = 2;
    let prefix = || Span::styled("│ ".to_string(), border);
    let mut cur: Vec<Span<'static>> = vec![prefix()];
    let mut w = PREFIX_W;
    for sp in spans {
        let style = sp.style;
        for ch in sp.content.chars() {
            if w + 1 > max_width && w > PREFIX_W {
                out.push(Line::from(std::mem::take(&mut cur)));
                cur = vec![prefix()];
                w = PREFIX_W;
            }
            if let Some(last) = cur.last_mut() {
                if last.style == style {
                    last.content.to_mut().push(ch);
                    w += 1;
                    continue;
                }
            }
            cur.push(Span::styled(ch.to_string(), style));
            w += 1;
        }
    }
    out.push(Line::from(cur));
}

/// Map a synoptic token kind to a palette color.
fn synoptic_color(name: &str, p: &Palette) -> Color {
    match syn_role(name) {
        SynRole::Comment => p.muted,
        SynRole::Keyword | SynRole::Macro | SynRole::Heading => p.accent,
        SynRole::Str => p.success,
        SynRole::Number | SynRole::Type => p.purple,
        SynRole::Function | SynRole::Reference | SynRole::Link => p.info,
        SynRole::Plain => p.fg,
    }
}

/// Highlight `code` with synoptic into per-source-line styled spans (fg always
/// set). Returns `None` when the language has no synoptic ruleset.
fn synoptic_spans(lang: &str, code: &str, p: &Palette) -> Option<Vec<Vec<Span<'static>>>> {
    let ext = lang_to_ext(lang)?;
    let mut h = from_extension(ext, 4)?;
    let lines: Vec<String> = code.split('\n').map(str::to_string).collect();
    h.run(&lines);

    let out = lines
        .iter()
        .enumerate()
        .map(|(y, raw)| {
            let mut spans: Vec<Span<'static>> = Vec::new();
            for tok in h.line(y, raw) {
                let (text, color) = match tok {
                    TokOpt::Some(t, name) => (t, synoptic_color(&name, p)),
                    TokOpt::None(t) => (t, p.fg),
                };
                let style = Style::default().fg(color);
                if let Some(last) = spans.last_mut() {
                    if last.style == style {
                        last.content.to_mut().push_str(&text);
                        continue;
                    }
                }
                spans.push(Span::styled(text, style));
            }
            spans
        })
        .collect();
    Some(out)
}

/// Highlight `code` into per-line styled spans, falling back to plain text when
/// the language has no synoptic ruleset.
pub(super) fn highlight_lines(lang: &str, code: &str, p: &Palette) -> Vec<Vec<Span<'static>>> {
    synoptic_spans(lang, code, p).unwrap_or_else(|| {
        code.split('\n')
            .map(|l| vec![Span::styled(l.to_string(), Style::default().fg(p.fg))])
            .collect()
    })
}

pub fn md_to_lines(md: &str, width: usize, palette: &'static Palette) -> Vec<Line<'static>> {
    let renderer =
        MarkdownRenderer::new(width).with_render_hooks(Box::new(PiHooks { width, palette }));
    renderer.render(&renderer.parse(md), &SirboneTheme { palette })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::PALETTES;

    #[test]
    fn synoptic_highlights_known_language() {
        let p = &PALETTES[0].1;
        let spans = synoptic_spans("rust", "fn main() {\n    let x = 1;\n}", p).unwrap();
        assert_eq!(spans.len(), 3, "one span vec per source line");
        // Highlighting happened if more than one distinct fg color appears.
        let colors: std::collections::HashSet<_> =
            spans.iter().flatten().map(|s| s.style.fg).collect();
        assert!(colors.len() > 1, "expected multiple colors, got {colors:?}");
    }

    #[test]
    fn synoptic_unknown_language_falls_back() {
        let p = &PALETTES[0].1;
        assert!(synoptic_spans("brainfuck", "+++", p).is_none());
        // highlight_lines degrades to one plain span per line.
        let lines = highlight_lines("brainfuck", "+++\n---", p);
        assert_eq!(lines.len(), 2);
        assert!(lines.iter().all(|l| l.len() == 1));
    }
}
