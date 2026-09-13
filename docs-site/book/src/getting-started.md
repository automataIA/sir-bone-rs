# Getting Started

## Try it without an API key

```bash
sirbone demo                    # the bundled recording
sirbone demo path/to/session.jsonl   # any session file you have
```

`demo` replays a real recorded run — a failing test suite, two bugs found and fixed, tests green —
inside the actual TUI: same rendering, same diffs, same tool boxes. No provider is contacted, and
typing a prompt says so instead of calling one. It is a recording, not a live agent.

The bundled recording is an artifact, not prose: `scripts/record-demo.sh` resets `playground/` to
its committed broken fixtures, runs one real turn, and scrubs the recording machine's paths out of
the session file. The prompt lives in `assets/demo-prompt.txt`, and a test asserts the recording
still starts from it — recipe and artifact cannot drift.

## Build

Requires a Rust toolchain.

```bash
cargo build              # debug build
cargo build --release    # optimized
```

The binary auto-detects its provider from environment variables, so the minimum to
run is an API token.

## Credentials

`sirbone login` is an interactive wizard: pick a provider from a preset menu (Anthropic, z.ai/GLM,
OpenRouter, Groq, Google AI Studio, OpenAI, or a custom OpenAI-compatible endpoint), paste your key,
and it writes **one global** `~/.sirbone/.env` (chmod 600), then runs a live connection test. If a
key is already exported in your shell it offers to persist it — no re-pasting. Configure **once**
and run from any directory; a per-project `.env` (or a real environment variable) still wins where
present. Non-interactive shells (CI, pipes) fall back to seeding a template.

`login` and `doctor` both run without a key, so setup never requires entering the TUI first.

To write `~/.sirbone/.env` by hand instead — the provider is auto-detected
(`ANTHROPIC_AUTH_TOKEN` → Anthropic, else OpenAI-compatible):

```env
# Anthropic
ANTHROPIC_AUTH_TOKEN=sk-ant-...
SIRBONE_MODEL=claude-opus-4-7
```

```env
# OpenAI / compatible (Ollama, Groq, LiteLLM, …)
OPENAI_API_KEY=sk-...
OPENAI_BASE_URL=https://api.openai.com/v1   # omit for OpenAI default
SIRBONE_MODEL=gpt-4o
```

`sirbone env` lists every variable the agent reads, with its current value.

## First run

```bash
# TUI (the default front-end)
ANTHROPIC_AUTH_TOKEN="…" SIRBONE_MODEL="claude-opus-4-7" cargo run

# One-shot (non-interactive)
ANTHROPIC_AUTH_TOKEN="…" SIRBONE_MODEL="claude-opus-4-7" cargo run -- "summarize src/agent.rs"

# REPL / readline mode
cargo run -- --repl

# Headless (like `claude -p`): one turn, print, exit. Prompt from arg or stdin.
cargo run -- -p "summarize src/agent.rs"
echo "summarize src/agent.rs" | cargo run -- -p
cargo run -- -p --output-format json "…" | jq   # {result,status,usage,session}
```

## Useful flags

| Flag | Effect |
|------|--------|
| `--repl` | readline REPL instead of the default TUI |
| `--review-only` | read-only run: no writes, no MCP, bash limited to the read-only whitelist |
| `--session <path>` | resume a saved session (`~/.sirbone/sessions/<uuid>.jsonl`) |
| `--thinking-budget <n>` | enable extended thinking with an `n`-token budget |
| `--temperature <t>` | pin the sampling temperature (unset = provider default; Claude models after Opus 4.6 accept only 1.0) |
| `--image <path>` | attach an image to the prompt (multimodal) |
| `-p` / `--print` | headless: run one turn, print, exit (prompt from arg or stdin) |
| `--output-format <text\|json>` | one-shot output; `json` emits `{result,status,usage,session}` |
| `--oracle` | explicitly enable the configured post-`Done` verification gate in headless/REPL runs |

```bash
cargo run -- --session ~/.sirbone/sessions/<uuid>.jsonl "follow-up question"
cargo run -- --thinking-budget 10000 "design a retry strategy for the HTTP client"
cargo run -- --image screenshot.png "what's wrong with this UI?"
```

## Configure project verification

Run the deterministic wizard from the project root:

```bash
cargo run -- setup-verification
cargo run -- setup-verification --json  # discovery only; never writes
```

It reads structured Rust, Python, and Node/TypeScript manifests, then shows any
authoritative and post-edit commands and offers the opt-in `high_risk`
confirmation preset before asking whether to run or save the exact patch.
Configuration alone does not enable the oracle in headless/REPL runs; add
`--oracle` or set `SIRBONE_ORACLE=1`. See [Configuration](./configuration.md)
for hooks, ablations, and telemetry.

## Try the UI first

If you just want to see the interface, open the in-browser
**[live demo](./demo.md)** — no install, no key.
