# Getting Started

## Build

Requires a Rust toolchain.

```bash
cargo build              # debug build
cargo build --release    # optimized
```

The binary auto-detects its provider from environment variables, so the minimum to
run is an API token.

## First run

Provider priority: `ANTHROPIC_AUTH_TOKEN` → Anthropic, else `OPENAI_API_KEY` → OpenAI.

```bash
# REPL (interactive)
ANTHROPIC_AUTH_TOKEN="…" SIRBONE_MODEL="claude-opus-4-7" cargo run

# One-shot (non-interactive)
ANTHROPIC_AUTH_TOKEN="…" SIRBONE_MODEL="claude-opus-4-7" cargo run -- "summarize src/agent.rs"

# Full-screen TUI
cargo run -- --tui
```

## Useful flags

| Flag | Effect |
|------|--------|
| `--tui` | full-screen terminal UI |
| `--session <path>` | resume a saved session (`~/.sirbone/sessions/<uuid>.jsonl`) |
| `--thinking-budget <n>` | enable extended thinking with an `n`-token budget |
| `--image <path>` | attach an image to the prompt (multimodal) |

```bash
cargo run -- --session ~/.sirbone/sessions/<uuid>.jsonl "follow-up question"
cargo run -- --thinking-budget 10000 "design a retry strategy for the HTTP client"
cargo run -- --image screenshot.png "what's wrong with this UI?"
```

## Try the UI first

If you just want to see the interface, open the in-browser
**[live demo](./demo.md)** — no install, no key.
