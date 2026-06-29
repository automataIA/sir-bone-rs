# The TUI

Launch with `cargo run -- --tui`. The interface has a chat pane, a side **trail**
panel listing each turn/tool as a clickable step, a status row (run state + extended
thinking tail), and an info bar (provider, model, context usage, cache-hit share,
background jobs).

> **Try it live:** the **[in-browser demo](./demo.md)** is this exact TUI compiled to
> WebAssembly — scripted, no API key. Great for learning the keys.

## Keys

| Key | Action |
|-----|--------|
| `Tab` | switch focus: chat ↔ input |
| `↑ / ↓` | scroll output (in chat focus) · history (in input focus) |
| `PgUp / PgDn` | fast scroll |
| `G` | jump to bottom (re-enable auto-scroll) |
| `Alt+B` | toggle the trail panel |
| `Alt+P` | cycle color palette |
| `Alt+A` | about screen (keybindings + boar) |
| `Alt+S` | settings (toggle localize, plan, oracle, architect, thinking budget) |
| `Esc` | cancel a running turn / drop a queued message |
| `Ctrl+C` | cancel current turn; at the prompt, quit |
| `Shift`+drag | select text in the chat pane (see below) |

## Selecting text with the mouse

The TUI captures the mouse so the scroll wheel, trail clicks, and panel resizing
work — which means the terminal's own text selection is turned off. To copy text,
hold a modifier while you drag:

- **Linux / WSL** (most terminals): **`Shift`**
- **macOS** (iTerm2, Terminal.app): **`Option/Alt`**
- **Windows Terminal**: **`Ctrl`** or **`Shift`**

Release the modifier and the scroll wheel goes back to scrolling the chat.

## Markdown & syntax highlighting

Agent responses are rendered as markdown — headings, tables, fenced code blocks with
syntax highlighting, and even **mermaid** diagrams drawn in the terminal.

## Slash commands

Type `/` in the input for built-in commands (e.g. `/help`, `/init`, `/model`,
`/plan`, `/rollback`, `/tokens`) and any installed skills (`/your-skill`). Skills typed
this way inject their instruction body into the conversation.

## Palettes

Six built-in palettes (sirbone, catppuccin, gruvbox, nord, tokyo-night, rose-pine).
Cycle with `Alt+P`; the choice is remembered per project.
