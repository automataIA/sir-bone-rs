# Providers

Sir Bone picks a provider from environment variables, in priority order:

1. `ANTHROPIC_AUTH_TOKEN` set → **Anthropic**
2. else `OPENAI_API_KEY` set → **OpenAI / OpenAI-compatible**

The model is chosen with `SIRBONE_MODEL`.

## Anthropic

```bash
export ANTHROPIC_AUTH_TOKEN="sk-…"
export SIRBONE_MODEL="claude-opus-4-7"
cargo run -- --tui
```

The Anthropic client streams via SSE and supports:

- **Prompt caching** — `cache_control` on the system prompt and tool definitions.
- **Extended thinking** — `--thinking-budget <tokens>`.
- **Multimodal** — images attached with `--image` are sent as base64 blocks.

## OpenAI-compatible (OpenAI, Ollama, Groq, …)

Any endpoint speaking the OpenAI chat-completions API works via `OPENAI_API_KEY` plus
an optional `OPENAI_BASE_URL`:

```bash
# OpenAI
export OPENAI_API_KEY="sk-…"
export SIRBONE_MODEL="gpt-4o"

# Ollama (local) — any non-empty key, point the base URL at the daemon
export OPENAI_API_KEY="ollama"
export OPENAI_BASE_URL="http://localhost:11434/v1"
export SIRBONE_MODEL="llama3.2"

# Groq
export OPENAI_API_KEY="gsk_…"
export OPENAI_BASE_URL="https://api.groq.com/openai/v1"
export SIRBONE_MODEL="llama-3.3-70b-versatile"
```

> Image input (`--image`) is only sent on the Anthropic path; the OpenAI path
> ignores image blocks.

Both clients retry with backoff on HTTP 429 and 5xx responses.
