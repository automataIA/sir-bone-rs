use std::io::{self, IsTerminal as _, Write as _};

use crossterm::style::{Color, StyledContent, Stylize};
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use synoptic::{from_extension, TokOpt};

use crate::highlight::{lang_to_ext, syn_role, SynRole};
use crate::types::NoticeLevel;

/// Centralized REPL/CLI palette. Replaces the per-method hardcoded `Color::*`
/// so the spinner, diffs and `synoptic` code highlighting share one source of
/// truth and can be switched for light terminals or disabled entirely.
#[derive(Clone, Copy)]
pub struct ColorTheme {
    enabled: bool,
    tool: Color,
    success: Color,
    error: Color,
    info: Color,
    muted: Color,
    diff_add: Color,
    diff_del: Color,
    diff_hunk: Color,
    fg: Color,
    syn_comment: Color,
    syn_keyword: Color,
    syn_string: Color,
    syn_number: Color,
    syn_function: Color,
}

impl ColorTheme {
    /// Default — the colors the REPL used before theming existed.
    pub fn dark() -> Self {
        Self {
            enabled: true,
            tool: Color::Cyan,
            success: Color::Green,
            error: Color::Red,
            info: Color::Yellow,
            muted: Color::DarkGrey,
            diff_add: Color::Green,
            diff_del: Color::Red,
            diff_hunk: Color::Magenta,
            fg: Color::Reset,
            syn_comment: Color::DarkGrey,
            syn_keyword: Color::DarkYellow,
            syn_string: Color::Green,
            syn_number: Color::Magenta,
            syn_function: Color::Blue,
        }
    }

    /// Darker variants that stay legible on a light terminal background.
    pub fn light() -> Self {
        Self {
            enabled: true,
            tool: Color::Blue,
            success: Color::DarkGreen,
            error: Color::DarkRed,
            info: Color::DarkYellow,
            muted: Color::DarkGrey,
            diff_add: Color::DarkGreen,
            diff_del: Color::DarkRed,
            diff_hunk: Color::DarkMagenta,
            fg: Color::Reset,
            syn_comment: Color::DarkGrey,
            syn_keyword: Color::DarkBlue,
            syn_string: Color::DarkGreen,
            syn_number: Color::DarkMagenta,
            syn_function: Color::Blue,
        }
    }

    /// No-color: every `paint` returns plain text, emitting zero ANSI.
    pub fn none() -> Self {
        Self {
            enabled: false,
            ..Self::dark()
        }
    }

    /// `NO_COLOR` → none; else `SIRBONE_THEME` env; else `cli.theme` config; else dark.
    pub fn load() -> Self {
        if std::env::var_os("NO_COLOR").is_some() {
            return Self::none();
        }
        let name = std::env::var("SIRBONE_THEME")
            .ok()
            .or_else(crate::config::cli_theme)
            .map(|s| s.to_lowercase());
        match name.as_deref() {
            Some("light") => Self::light(),
            Some("none") | Some("off") => Self::none(),
            _ => Self::dark(),
        }
    }

    fn paint<'a>(&self, s: &'a str, c: Color) -> StyledContent<&'a str> {
        if self.enabled {
            s.with(c)
        } else {
            s.stylize()
        }
    }

    fn paint_bold<'a>(&self, s: &'a str, c: Color) -> StyledContent<&'a str> {
        if self.enabled {
            s.with(c).bold()
        } else {
            s.stylize()
        }
    }

    fn syn(&self, role: SynRole) -> Color {
        match role {
            SynRole::Comment => self.syn_comment,
            SynRole::Keyword | SynRole::Macro | SynRole::Heading => self.syn_keyword,
            SynRole::Str => self.syn_string,
            SynRole::Number | SynRole::Type => self.syn_number,
            SynRole::Function | SynRole::Reference | SynRole::Link => self.syn_function,
            SynRole::Plain => self.fg,
        }
    }
}

/// A fenced code block accumulated across streaming chunks until its closing
/// fence, then highlighted as a unit (synoptic needs the whole block).
struct CodeBlock {
    lang: String,
    lines: Vec<String>,
}

/// Braille spinner frames (same set as the TUI / claw-code).
const SPIN_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub struct Renderer {
    out: io::Stdout,
    theme: ColorTheme,
    /// Terminal is in raw mode (REPL holds it via rustyline) — bare `\n` then does
    /// not return the cursor to column 0, so we emit `\r\n`. False in one-shot
    /// (cooked) and pipes, where a stray `\r` would corrupt output.
    raw: bool,
    /// Carry of the current incomplete line — text arrives in arbitrary chunks
    /// but fenced code and plain lines are decided per whole line.
    line_buf: String,
    code: Option<CodeBlock>,
    /// Consecutive markdown table rows (pipe lines) accumulated until a non-row
    /// line closes them; rendered as an aligned box, or flushed verbatim when the
    /// block turns out not to be a real table (no separator row).
    table: Vec<String>,
    /// Whether the spinner may draw (interactive tty + colors on). Off in pipes
    /// and under `NO_COLOR`/none so `\r`+clear sequences never pollute output.
    spin_ok: bool,
    spin_active: bool,
    spin_drawn: bool,
    spin_frame: usize,
    spin_label: &'static str,
}

impl Default for Renderer {
    fn default() -> Self {
        Self::new(false)
    }
}

impl Renderer {
    /// `raw` = the terminal is in raw mode (REPL holds it via rustyline). rustyline
    /// puts the tty raw through its own termios, not crossterm's flag, so this is
    /// threaded from the REPL/one-shot dispatch rather than auto-detected.
    pub fn new(raw: bool) -> Self {
        let theme = ColorTheme::load();
        Self {
            out: io::stdout(),
            raw: raw && io::stdout().is_terminal(),
            spin_ok: theme.enabled && io::stdout().is_terminal(),
            theme,
            line_buf: String::new(),
            code: None,
            table: Vec::new(),
            spin_active: false,
            spin_drawn: false,
            spin_frame: 0,
            spin_label: "",
        }
    }

    /// Write `s` to stdout, translating `\n`→`\r\n` when the terminal is raw.
    fn put(&mut self, s: &str) {
        let _ = write!(self.out, "{}", nl_fix(s, self.raw));
        self.out.flush().ok();
    }

    /// [`Renderer::put`] followed by a raw-aware line break.
    fn putln(&mut self, s: &str) {
        self.put(s);
        let _ = self.out.write_all(if self.raw { b"\r\n" } else { b"\n" });
        self.out.flush().ok();
    }

    /// Raw-aware line write to stderr (errors/warnings).
    fn eputln(&self, s: &str) {
        let nl = if self.raw { "\r\n" } else { "\n" };
        let _ = write!(io::stderr(), "{}{nl}", nl_fix(s, self.raw));
    }

    /// Mark the spinner active with a label; the next [`Renderer::spin_tick`]
    /// draws it. No-op output until then.
    pub fn spin_start(&mut self, label: &'static str) {
        if self.spin_ok {
            self.spin_active = true;
            self.spin_label = label;
        }
    }

    /// Stop the spinner and erase its line if currently shown.
    pub fn spin_stop(&mut self) {
        self.clear_spin_line();
        self.spin_active = false;
    }

    /// Advance and redraw the spinner if active. Driven by an interval timer.
    pub fn spin_tick(&mut self) {
        if !self.spin_active {
            return;
        }
        self.clear_spin_line();
        let frame = SPIN_FRAMES[self.spin_frame % SPIN_FRAMES.len()];
        let _ = write!(
            self.out,
            "\r  {} {}",
            self.theme.paint(frame, self.theme.tool),
            self.theme.paint(self.spin_label, self.theme.muted)
        );
        self.out.flush().ok();
        self.spin_drawn = true;
        self.spin_frame = self.spin_frame.wrapping_add(1);
    }

    /// Erase the transient spinner line so a real output line can take its place.
    fn clear_spin_line(&mut self) {
        if self.spin_drawn {
            let _ = write!(self.out, "\r\x1b[K");
            self.out.flush().ok();
            self.spin_drawn = false;
        }
    }

    /// Streaming text chunk. Buffered by line so fenced code blocks can be
    /// highlighted; complete lines are emitted as they close.
    pub fn text(&mut self, s: &str) {
        self.clear_spin_line();
        self.line_buf.push_str(s);
        while let Some(nl) = self.line_buf.find('\n') {
            let after = self.line_buf.split_off(nl + 1);
            self.line_buf.truncate(nl); // drop the '\n'
            let line = std::mem::replace(&mut self.line_buf, after);
            self.feed_line(&line);
        }
    }

    fn feed_line(&mut self, line: &str) {
        if self.code.is_some() {
            if is_fence(line) {
                let block = self.code.take().unwrap();
                self.emit_code(&block);
            } else {
                self.code.as_mut().unwrap().lines.push(line.to_string());
            }
            return;
        }
        // A pipe row may start a table; buffer consecutive rows and decide at the
        // close (the separator row is only known once the next line arrives).
        if is_table_row(line) {
            self.table.push(line.to_string());
            return;
        }
        if !self.table.is_empty() {
            self.flush_table();
        }
        if let Some(lang) = fence_open(line) {
            self.code = Some(CodeBlock {
                lang,
                lines: Vec::new(),
            });
        } else {
            // Inline markdown (bold/italic/code/headings/lists) — rendered once,
            // here, as the line completes; the append-only REPL never repaints it.
            let rendered = md_inline(line, &self.theme);
            self.putln(&rendered);
        }
    }

    /// Render the buffered pipe rows as an aligned box, or — when they aren't a
    /// real table (no `|---|` separator) — flush them verbatim as prose.
    fn flush_table(&mut self) {
        let rows = std::mem::take(&mut self.table);
        match render_table(&rows, &self.theme) {
            Some(lines) => {
                for l in lines {
                    self.putln(&l);
                }
            }
            None => {
                for r in rows {
                    let rendered = md_inline(&r, &self.theme);
                    self.putln(&rendered);
                }
            }
        }
    }

    /// Flush any pending prose line / unterminated code block. Call before
    /// rendering a non-text event so buffered output is not stranded.
    pub fn flush_text(&mut self) {
        self.clear_spin_line();
        // The final table row of a message often arrives without a closing newline,
        // so it sits in `line_buf` rather than the table buffer — fold it back in
        // before rendering, or it renders as stray pipes under the box.
        if !self.table.is_empty() && is_table_row(&self.line_buf) {
            let row = std::mem::take(&mut self.line_buf);
            self.table.push(row);
        }
        if !self.table.is_empty() {
            self.flush_table();
        }
        if let Some(block) = self.code.take() {
            for l in &block.lines.clone() {
                self.putln(&format!("    {l}"));
            }
        }
        if !self.line_buf.is_empty() {
            // A trailing line with no newline (common: the final line of a message)
            // still gets inline markdown — same as a completed line, just no break.
            let line = std::mem::take(&mut self.line_buf);
            let rendered = md_inline(&line, &self.theme);
            self.put(&rendered);
        }
    }

    /// Highlight a code block into per-line styled strings, or `None` when the
    /// fence language has no synoptic ruleset (the caller prints it plain).
    fn highlight_block(&self, block: &CodeBlock) -> Option<Vec<String>> {
        let ext = lang_to_ext(&block.lang)?;
        let mut h = from_extension(ext, 4)?;
        h.run(&block.lines);
        Some(
            block
                .lines
                .iter()
                .enumerate()
                .map(|(y, raw)| {
                    h.line(y, raw)
                        .into_iter()
                        .map(|tok| match tok {
                            TokOpt::Some(t, name) => self
                                .theme
                                .paint(&t, self.theme.syn(syn_role(&name)))
                                .to_string(),
                            TokOpt::None(t) => t,
                        })
                        .collect::<String>()
                })
                .collect(),
        )
    }

    fn emit_code(&mut self, block: &CodeBlock) {
        let lines = self
            .highlight_block(block)
            .unwrap_or_else(|| block.lines.clone());
        for l in lines {
            self.putln(&format!("    {l}"));
        }
    }

    /// `▸ tool_name` in the tool color.
    pub fn tool_start(&mut self, name: &str) {
        self.clear_spin_line();
        let s = format!(
            "\n  {} {}",
            self.theme.paint("▸", self.theme.tool),
            self.theme.paint_bold(name, self.theme.tool)
        );
        self.putln(&s);
    }

    /// Result block. Edit results get diff coloring; everything else gets a preview line.
    pub fn tool_end(&mut self, name: &str, result: &str, is_error: bool) {
        self.clear_spin_line();
        if is_diff_result(result) {
            self.render_diff(result);
        } else if result == "blocked" {
            let s = format!(
                "    {} {name} blocked",
                self.theme.paint("✗", self.theme.error)
            );
            self.putln(&s);
        } else {
            let preview: String = result.chars().take(120).collect();
            let suffix = if result.len() > 120 { "…" } else { "" };
            let mark = if is_error {
                self.theme.paint("✗", self.theme.error)
            } else {
                self.theme.paint("✓", self.theme.success)
            };
            self.putln(&format!("    {mark} {name} → {preview}{suffix}"));
        }
    }

    pub fn error(&mut self, msg: &str) {
        self.eputln(&format!(
            "\n  {} {}",
            self.theme.paint("✗", self.theme.error),
            self.theme.paint(msg, self.theme.error)
        ));
    }

    /// Non-failure status line (e.g. the verification oracle): green ✓ for
    /// success, amber • for in-progress — never the red of [`Renderer::error`].
    pub fn notice(&mut self, msg: &str, level: NoticeLevel) {
        let color = match level {
            NoticeLevel::Success => self.theme.success,
            NoticeLevel::Info => self.theme.info,
        };
        let glyph = match level {
            NoticeLevel::Success => "✓",
            NoticeLevel::Info => "•",
        };
        let s = format!(
            "\n  {} {}",
            self.theme.paint(glyph, color),
            self.theme.paint(msg, color)
        );
        self.putln(&s);
    }

    pub fn cancelled(&mut self) {
        self.eputln(&format!(
            "\n  {}",
            self.theme.paint("cancelled", self.theme.info)
        ));
    }

    pub fn ctx_warning(&mut self, pct: u8, window: u32) {
        self.eputln(&format!(
            "\n  {} context at {pct}% of {}k tokens — quality may degrade",
            self.theme.paint("⚠", self.theme.info),
            window / 1000,
        ));
    }

    fn render_diff(&mut self, result: &str) {
        for (i, line) in result.lines().enumerate() {
            let s = if i == 0 {
                format!("    {}", self.theme.paint(line, self.theme.muted))
            } else if line.starts_with("@@") {
                format!("    {}", self.theme.paint(line, self.theme.diff_hunk))
            } else if line.starts_with('+') {
                format!("    {}", self.theme.paint(line, self.theme.diff_add))
            } else if line.starts_with('-') {
                format!("    {}", self.theme.paint(line, self.theme.diff_del))
            } else if line.starts_with(' ') {
                format!("    {}", self.theme.paint(line, self.theme.muted))
            } else if !line.is_empty() {
                format!("    {line}")
            } else {
                continue;
            };
            self.putln(&s);
        }
    }
}

/// EditTool output carries the `Edited <path>.\n\n<unified diff>` envelope. The
/// `@@` hunk marker is required too, so an unrelated result that happens to
/// start with "Edited " (a CHANGELOG line, a bash `git log`) isn't misrouted
/// into diff rendering.
fn is_diff_result(result: &str) -> bool {
    result.starts_with("Edited ") && result.contains("\n@@")
}

/// A line that opens (or closes) a fenced code block; the opener carries the
/// language tag.
fn fence_open(line: &str) -> Option<String> {
    let rest = line.trim_start().strip_prefix("```")?;
    // Ignore inline `` ```x``` `` on one line — only treat a bare opener fence.
    if rest.contains("```") {
        return None;
    }
    Some(rest.trim().to_string())
}

fn is_fence(line: &str) -> bool {
    line.trim_start().starts_with("```")
}

/// A candidate markdown table row: trimmed line starting with `|`.
fn is_table_row(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with('|') && t.len() > 1
}

/// Split a pipe row into trimmed cells, dropping the empty cells a leading or
/// trailing `|` produces.
fn split_cells(line: &str) -> Vec<String> {
    let mut cells: Vec<String> = line
        .trim()
        .split('|')
        .map(|c| c.trim().to_string())
        .collect();
    if cells.first().is_some_and(String::is_empty) {
        cells.remove(0);
    }
    if cells.last().is_some_and(String::is_empty) {
        cells.pop();
    }
    cells
}

/// A GFM separator row, e.g. `|---|:--:|`: every cell is dashes with optional
/// alignment colons.
fn is_table_separator(line: &str) -> bool {
    let cells = split_cells(line);
    !cells.is_empty()
        && cells.iter().all(|c| {
            let core = c.trim_start_matches(':').trim_end_matches(':');
            !core.is_empty() && core.chars().all(|ch| ch == '-')
        })
}

/// Render buffered pipe rows as an aligned box-drawing table, or `None` when the
/// rows aren't a real table (need a header + a `|---|` separator). Cells are
/// left-aligned and shown literally — inline markdown inside a cell stays as
/// source so column widths always match the visible text. Widths use char counts
/// (fine for ASCII/Latin; wide CJK or emoji may misalign — out of scope here).
fn render_table(rows: &[String], theme: &ColorTheme) -> Option<Vec<String>> {
    if rows.len() < 2 || !is_table_separator(&rows[1]) {
        return None;
    }
    let header = split_cells(&rows[0]);
    let body: Vec<Vec<String>> = rows[2..].iter().map(|r| split_cells(r)).collect();
    let ncols = header.len();
    if ncols == 0 {
        return None;
    }
    let mut widths = vec![0usize; ncols];
    for (i, w) in widths.iter_mut().enumerate() {
        *w = header[i].chars().count();
        for row in &body {
            if let Some(cell) = row.get(i) {
                *w = (*w).max(cell.chars().count());
            }
        }
    }
    let border = |left: &str, mid: &str, right: &str| {
        let seg: Vec<String> = widths.iter().map(|w| "─".repeat(w + 2)).collect();
        theme
            .paint(&format!("{left}{}{right}", seg.join(mid)), theme.muted)
            .to_string()
    };
    let bar = theme.paint("│", theme.muted).to_string();
    let row_line = |cells: &[String]| {
        let mut s = String::new();
        for (i, w) in widths.iter().enumerate() {
            let raw = cells.get(i).map(String::as_str).unwrap_or("");
            s.push_str(&bar);
            s.push(' ');
            s.push_str(raw);
            s.push_str(&" ".repeat(w - raw.chars().count() + 1));
        }
        s.push_str(&bar);
        s
    };
    let mut out = Vec::with_capacity(body.len() + 4);
    out.push(border("┌", "┬", "┐"));
    out.push(row_line(&header));
    out.push(border("├", "┼", "┤"));
    for row in &body {
        out.push(row_line(row));
    }
    out.push(border("└", "┴", "┘"));
    Some(out)
}

/// Style one inline text segment with the active emphasis flags + optional color.
/// Returns plain text when the theme is disabled (none / `NO_COLOR`).
fn styled_seg(
    theme: &ColorTheme,
    text: &str,
    bold: bool,
    italic: bool,
    strike: bool,
    color: Option<Color>,
) -> String {
    if !theme.enabled {
        return text.to_string();
    }
    let mut s = text.stylize();
    if let Some(c) = color {
        s = s.with(c);
    }
    if bold {
        s = s.bold();
    }
    if italic {
        s = s.italic();
    }
    if strike {
        s = s.crossed_out();
    }
    s.to_string()
}

/// Render a single line of CommonMark inline markup (bold/italic/strikethrough/
/// inline code/headings/list bullets/blockquotes) to an ANSI string. Operates per
/// line — multi-line constructs degrade to literal text, which is acceptable for
/// streaming agent prose and keeps every line rendered exactly once.
fn md_inline(line: &str, theme: &ColorTheme) -> String {
    if !theme.enabled || line.trim().is_empty() {
        return line.to_string();
    }
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    let mut out = String::new();
    let (mut bold, mut italic, mut strike) = (0u32, 0u32, 0u32);
    let mut heading = false;
    let mut lists: Vec<Option<u64>> = Vec::new();
    for ev in Parser::new_ext(line, opts) {
        match ev {
            Event::Start(Tag::Strong) => bold += 1,
            Event::End(TagEnd::Strong) => bold = bold.saturating_sub(1),
            Event::Start(Tag::Emphasis) => italic += 1,
            Event::End(TagEnd::Emphasis) => italic = italic.saturating_sub(1),
            Event::Start(Tag::Strikethrough) => strike += 1,
            Event::End(TagEnd::Strikethrough) => strike = strike.saturating_sub(1),
            Event::Start(Tag::Heading { .. }) => heading = true,
            Event::End(TagEnd::Heading(_)) => heading = false,
            Event::Start(Tag::List(start)) => lists.push(start),
            Event::End(TagEnd::List(_)) => {
                lists.pop();
            }
            Event::Start(Tag::Item) => {
                out.push_str(&"  ".repeat(lists.len().saturating_sub(1)));
                match lists.last_mut() {
                    Some(Some(n)) => {
                        let cur = *n;
                        *n += 1;
                        out.push_str(&styled_seg(
                            theme,
                            &format!("{cur}. "),
                            false,
                            false,
                            false,
                            Some(theme.muted),
                        ));
                    }
                    _ => out.push_str(&styled_seg(
                        theme,
                        "• ",
                        false,
                        false,
                        false,
                        Some(theme.tool),
                    )),
                }
            }
            Event::Start(Tag::BlockQuote(_)) => {
                out.push_str(&styled_seg(
                    theme,
                    "│ ",
                    false,
                    false,
                    false,
                    Some(theme.muted),
                ));
            }
            Event::Text(t) => {
                let color = heading.then_some(theme.tool);
                out.push_str(&styled_seg(
                    theme,
                    &t,
                    bold > 0 || heading,
                    italic > 0,
                    strike > 0,
                    color,
                ));
            }
            Event::Code(t) => out.push_str(&styled_seg(
                theme,
                &t,
                false,
                false,
                false,
                Some(theme.success),
            )),
            Event::SoftBreak | Event::HardBreak => out.push(' '),
            _ => {}
        }
    }
    if out.is_empty() {
        line.to_string()
    } else {
        out
    }
}

/// In a raw-mode terminal a bare `\n` does not return the cursor to column 0;
/// translate to `\r\n` so REPL output isn't staircased. No-op when not raw.
fn nl_fix(s: &str, raw: bool) -> std::borrow::Cow<'_, str> {
    if raw && s.contains('\n') {
        std::borrow::Cow::Owned(s.replace('\n', "\r\n"))
    } else {
        std::borrow::Cow::Borrowed(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_theme_emits_no_ansi() {
        let t = ColorTheme::none();
        assert_eq!(t.paint("hi", Color::Red).to_string(), "hi");
        assert_eq!(t.paint_bold("hi", Color::Red).to_string(), "hi");
    }

    #[test]
    fn dark_theme_emits_ansi() {
        let t = ColorTheme::dark();
        assert!(t.paint("hi", Color::Red).to_string().contains('\u{1b}'));
    }

    #[test]
    fn none_constructor_disabled() {
        assert!(!ColorTheme::none().enabled);
        assert!(ColorTheme::dark().enabled);
    }

    #[test]
    fn fence_open_parses_lang() {
        assert_eq!(fence_open("```rust").as_deref(), Some("rust"));
        assert_eq!(fence_open("   ```py ").as_deref(), Some("py"));
        assert_eq!(fence_open("not a fence"), None);
        assert_eq!(fence_open("```inline```"), None);
    }

    #[test]
    fn table_row_and_separator_detection() {
        assert!(is_table_row("| a | b |"));
        assert!(!is_table_row("plain text"));
        assert!(!is_table_row("|")); // a lone pipe is not a row
        assert!(is_table_separator("|---|---|"));
        assert!(is_table_separator("| :--- | ---: |"));
        assert!(!is_table_separator("| a | b |"));
    }

    #[test]
    fn render_table_aligns_and_boxes() {
        let t = ColorTheme::none(); // deterministic plain output
        let rows = vec![
            "| Tool | Categoria |".to_string(),
            "|------|-----------|".to_string(),
            "| bash | esecuzione |".to_string(),
            "| write | file |".to_string(),
        ];
        let out = render_table(&rows, &t).expect("valid table");
        assert!(out[0].starts_with('┌') && out[0].ends_with('┐'));
        assert!(out.last().unwrap().starts_with('└'));
        // Every rendered line is the same display width (aligned columns).
        let w0 = out[0].chars().count();
        assert!(
            out.iter().all(|l| l.chars().count() == w0),
            "rows misaligned: {out:?}"
        );
        assert!(out.iter().any(|l| l.contains("bash")));
    }

    #[test]
    fn trailing_table_row_without_newline_joins_table() {
        let mut r = renderer_with(ColorTheme::none());
        // Last row has no closing newline — the common end-of-message case.
        r.text("| A | B |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |");
        assert_eq!(r.table.len(), 3, "header+sep+first row buffered");
        assert_eq!(r.line_buf, "| 3 | 4 |", "trailing row stranded in line_buf");
        r.flush_text();
        assert!(
            r.table.is_empty() && r.line_buf.is_empty(),
            "trailing row absorbed and flushed"
        );
    }

    #[test]
    fn render_table_rejects_non_table() {
        let t = ColorTheme::none();
        // No separator row → not a table.
        let rows = vec![
            "| just | pipes |".to_string(),
            "| more | pipes |".to_string(),
        ];
        assert!(render_table(&rows, &t).is_none());
    }

    fn renderer_with(theme: ColorTheme) -> Renderer {
        let mut r = Renderer::new(false);
        r.theme = theme;
        r
    }

    #[test]
    fn highlight_block_colorizes_known_language() {
        let r = renderer_with(ColorTheme::dark());
        let block = CodeBlock {
            lang: "rust".into(),
            lines: vec!["fn main() {}".into()],
        };
        let out = r.highlight_block(&block).expect("rust has a ruleset");
        assert!(
            out[0].contains('\u{1b}'),
            "keyword `fn` should be colored: {:?}",
            out[0]
        );
    }

    #[test]
    fn md_inline_styles_bold_code_heading_and_passes_plain() {
        let t = ColorTheme::dark();
        assert!(
            md_inline("a **b** c", &t).contains('\u{1b}'),
            "bold emits ANSI"
        );
        assert!(
            md_inline("use `foo()`", &t).contains('\u{1b}'),
            "inline code emits ANSI"
        );
        assert!(
            md_inline("# Title", &t).contains('\u{1b}'),
            "heading emits ANSI"
        );
        // Plain prose with no markup round-trips unchanged.
        assert_eq!(md_inline("just plain text", &t), "just plain text");
        // Non-marker asterisks are not emphasis.
        assert_eq!(md_inline("2 * 3 = 6", &t), "2 * 3 = 6");
    }

    #[test]
    fn md_inline_none_theme_is_literal() {
        let t = ColorTheme::none();
        assert_eq!(md_inline("a **b** `c`", &t), "a **b** `c`");
    }

    #[test]
    fn nl_fix_translates_only_when_raw() {
        assert_eq!(nl_fix("a\nb", true), "a\r\nb");
        assert_eq!(nl_fix("a\nb", false), "a\nb"); // cooked / pipe: untouched
        assert_eq!(nl_fix("no newline", true), "no newline");
    }

    #[test]
    fn highlight_block_none_for_bare_or_unknown_fence() {
        let r = renderer_with(ColorTheme::dark());
        // Bare fence (no language) and unknown languages fall back to plain.
        assert!(r
            .highlight_block(&CodeBlock {
                lang: String::new(),
                lines: vec!["x".into()]
            })
            .is_none());
        assert!(r
            .highlight_block(&CodeBlock {
                lang: "brainfuck".into(),
                lines: vec!["+".into()]
            })
            .is_none());
    }
}
