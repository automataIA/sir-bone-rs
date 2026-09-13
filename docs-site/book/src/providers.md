# Providers

Sir Bone picks a provider from environment variables, in priority order:

1. `SIRBONE_CODEX=1` → **ChatGPT Plus/Pro through the official Codex CLI**
2. `ANTHROPIC_AUTH_TOKEN` set → **Anthropic**
3. else `OPENAI_API_KEY` set → **OpenAI / OpenAI-compatible**

The model is chosen with `SIRBONE_MODEL`.

## ChatGPT Plus/Pro (Codex OAuth)

ChatGPT subscriptions and OpenAI API billing are separate. To use a Plus/Pro
subscription, install the official Codex CLI and authenticate it in the browser:

```bash
sirbone login --codex
SIRBONE_CODEX=1 sirbone
```

You can also use `sirbone login codex` or `sirbone --codex`. Sir Bone delegates
the turn to `codex exec --json`; it never reads or stores the Codex OAuth token.
In this mode Codex owns the model/tool execution, so Sir Bone's own API-key
client and tool-confirmation pipeline are not used.

Codex uses its `workspace-write` sandbox by default. If Sir Bone already runs
inside a trusted outer sandbox that prevents nested Linux namespaces, set
`SIRBONE_CODEX_SANDBOX=danger-full-access`. This disables only Codex's inner
sandbox, so use it only when the outer sandbox provides the intended boundary.

## Anthropic

```bash
export ANTHROPIC_AUTH_TOKEN="sk-…"
export SIRBONE_MODEL="claude-opus-4-7"
cargo run
```

The Anthropic client streams via SSE and supports:

- **Prompt caching** — `cache_control` on the system prompt and tool definitions.
- **Extended thinking** — `--thinking-budget <tokens>`.
- **Multimodal** — images attached with `--image` are sent as base64 blocks.

On **GLM via z.ai** (either the Anthropic-compatible or the OpenAI-compatible
endpoint) the thinking dial is translated into the reasoning-effort levels z.ai
actually honours — a token budget sent there only selects a heavy default:

| Dial (TUI `t` / `--thinking-budget`) | What GLM runs |
|---|---|
| off / light (no budget) | light thinking (`thinking: disabled` / `minimal`) |
| 8k | low |
| 16k | medium |
| 32k | max |

Full off does not exist on z.ai: every "disable" spelling is converted to a
lightweight-thinking low. Measured on `glm-5.2`/`glm-5.3`: light ≈ 30–80
reasoning tokens, low ≈ 130–670, medium ≈ 50–880, max ≈ 350–1160 — with wait
time scaling accordingly. On the OpenAI-compatible endpoint the same dial is
sent as `reasoning_effort`; leaving it unset there means z.ai's default, which
is max.

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

### Images on an OpenAI-compatible endpoint

No OpenAI-compatible endpoint advertises whether its model can see, so sirbone does not
guess: images are dropped unless you declare the capability with `--vision`
(`SIRBONE_VISION=1`). With the flag set, attachments travel as `image_url` parts carrying
a base64 data URI — the format llama.cpp, OpenAI and OpenRouter all read.

```bash
# llama-server with a vision projector
llama-server -m qwen3-vl-8b.gguf --mmproj mmproj-qwen3-vl-8b.gguf -c 32768

export OPENAI_API_KEY="local"
export OPENAI_BASE_URL="http://127.0.0.1:8080/v1"
sirbone --vision --image shot.png "what's wrong with this UI?"
```

The flag is a claim about the endpoint, not a probe: switching to a text-only model with
`/model` leaves it on, and the model will hallucinate the image instead of reading it. On
the Anthropic path it does nothing — vision is inferred from the host there.

Image bytes count against the server's context but not against sirbone's compaction
estimate, which weighs base64 as text. On a small local window, set `SIRBONE_CONTEXT_WINDOW`
conservatively (e.g. `30000`).

Both clients retry with backoff on HTTP 429 and 5xx responses.
