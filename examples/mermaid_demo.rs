use std::io;

use ratatui::{
    backend::CrosstermBackend,
    crossterm::{
        event::{self, Event, KeyCode},
        execute,
        terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    },
    layout::Constraint,
    widgets::{Block, Borders},
    Frame, Terminal,
};
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use ratatui_markdown::{
    markdown::{MarkdownRenderer, RenderHooks},
    theme::{Generation, RichTextTheme},
};

/// Custom theme matching sirbone's style
struct SirboneTheme;

use ratatui::style::Color;

const ACCENT: Color = Color::Rgb(218, 165, 32);
const FG: Color = Color::Rgb(204, 204, 204);
const MUTED: Color = Color::Rgb(90, 90, 90);
const SUCCESS: Color = Color::Rgb(87, 171, 90);
const BORDER: Color = Color::Rgb(45, 45, 45);

impl RichTextTheme for SirboneTheme {
    fn generation(&self) -> Generation {
        Generation(1)
    }
    fn get_text_color(&self) -> Color {
        FG
    }
    fn get_muted_text_color(&self) -> Color {
        MUTED
    }
    fn get_primary_color(&self) -> Color {
        ACCENT
    }
    fn get_secondary_color(&self) -> Color {
        Color::Rgb(100, 149, 237)
    }
    fn get_info_color(&self) -> Color {
        Color::Rgb(100, 149, 237)
    }
    fn get_background_color(&self) -> Color {
        Color::Rgb(13, 13, 13)
    }
    fn get_border_color(&self) -> Color {
        BORDER
    }
    fn get_focused_border_color(&self) -> Color {
        MUTED
    }
    fn get_popup_selected_background(&self) -> Color {
        BORDER
    }
    fn get_popup_selected_text_color(&self) -> Color {
        FG
    }
    fn get_json_key_color(&self) -> Color {
        Color::Rgb(100, 149, 237)
    }
    fn get_json_string_color(&self) -> Color {
        SUCCESS
    }
    fn get_json_number_color(&self) -> Color {
        ACCENT
    }
    fn get_json_bool_color(&self) -> Color {
        Color::Rgb(180, 130, 210)
    }
    fn get_json_null_color(&self) -> Color {
        MUTED
    }
    fn get_accent_yellow(&self) -> Color {
        ACCENT
    }
}

// ── heading hooks ─────────────────────────────────────────────────────────────
// H1: amber bar prefix — most prominent
// H2: muted dash prefix — section level
// H3: indented muted — subsection
struct PiHooks;

impl RenderHooks for PiHooks {
    fn heading1(&self, text: &str) -> Option<Line<'static>> {
        Some(Line::from(vec![
            Span::styled("  ▌ ", Style::default().fg(ACCENT)),
            Span::styled(
                text.to_owned(),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
        ]))
    }
    fn heading2(&self, text: &str) -> Option<Line<'static>> {
        Some(Line::from(vec![
            Span::styled("  ╌ ", Style::default().fg(MUTED)),
            Span::styled(
                text.to_owned(),
                Style::default().fg(FG).add_modifier(Modifier::BOLD),
            ),
        ]))
    }
    fn heading3(&self, text: &str) -> Option<Line<'static>> {
        Some(Line::from(vec![
            Span::styled("    ", Style::default()),
            Span::styled(
                text.to_owned(),
                Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
            ),
        ]))
    }
}

/// Markdown content with embedded Mermaid diagrams describing the sirbone project
const MERMAID_MARKDOWN: &str = r#"
# 🏗️ Architettura di sirbone — AI Coding Agent

## Dipendenze tra crate

```mermaid
graph TD
    CLI["pi-cli  ·  binary"]
    TUI["pi-tui  ·  ratatui"]
    AI["pi-ai  ·  LLM providers"]
    AGENT["pi-agent  ·  core & tools"]

    CLI --> AGENT
    CLI --> AI
    CLI --> TUI
    TUI --> AGENT
    AI --> AGENT
```

## State Machine dell'agente

```mermaid
stateDiagram-v2
    [*] --> Idle : Utente invia prompt
    Idle --> ToolCalling : LLM → tool_calls
    Idle --> Done : LLM → solo testo
    ToolCalling --> Idle : risultati tool pronti
    Done --> [*]
```

## Tool Registry

```mermaid
pie title "7 Tool disponibili"
    "Bash" : 1
    "Read" : 1
    "Write" : 1
    "Edit" : 1
    "Grep" : 1
    "Find" : 1
    "Ls" : 1
```

---

*Premi **q** o **Esc** per uscire. j/k o ↑/↓ per scrollare.*
"#;

fn main() -> anyhow::Result<()> {
    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;

    let result = run(&mut terminal);

    disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen)?;
    result
}

fn run(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> anyhow::Result<()> {
    let mut scroll: u16 = 0;

    loop {
        terminal.draw(|f| render(f, scroll))?;

        if event::poll(std::time::Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Down | KeyCode::Char('j') => scroll = scroll.saturating_add(1),
                    KeyCode::Up | KeyCode::Char('k') => scroll = scroll.saturating_sub(1),
                    KeyCode::Char('G') => scroll = 200,
                    KeyCode::Char('g') => scroll = 0,
                    _ => {}
                }
            }
        }
    }

    Ok(())
}

fn render(f: &mut Frame, scroll: u16) {
    let area = f.area();

    // Parse & render markdown with embedded mermaid blocks
    let width = area.width.saturating_sub(2) as usize; // minus borders
    let renderer = MarkdownRenderer::new(width).with_render_hooks(Box::new(PiHooks));
    let blocks = renderer.parse(MERMAID_MARKDOWN);
    let lines = renderer.render(&blocks, &SirboneTheme);

    let paragraph = ratatui::widgets::Paragraph::new(lines)
        .scroll((scroll, 0))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" pi — Mermaid Diagram Demo "),
        );

    let [main_area, hint_area] = ratatui::layout::Layout::default()
        .constraints([Constraint::Min(3), Constraint::Length(1)])
        .areas(area);

    let hint = ratatui::widgets::Paragraph::new(format!(
        " scroll: {scroll}  |  j/k/↑/↓: scroll  |  g/G: top/bottom  |  q: quit "
    ))
    .style(ratatui::style::Style::default().fg(ratatui::style::Color::DarkGray));

    f.render_widget(paragraph, main_area);
    f.render_widget(hint, hint_area);
}
