# Configuration

Sir Bone reads layered JSON configuration plus environment variables (`.env` is
loaded automatically).

## Layers

1. **Global** — `~/.sirbone/config.json`
2. **Per-project** — `~/.sirbone/projects/<slug>/config.json`

Merge rules:

- Most sections in the per-project file **replace** the global section wholesale.
- `mcpServers` is merged **per key** (see [MCP](./mcp.md)).
- An empty config falls back to legacy default behavior.

## Common keys

| Key | Purpose |
|-----|---------|
| `permissions` | allow / soft-deny globs + classifier settings — see [Permissions](./permissions.md) |
| `post_edit_check` | map of file glob → lint/format command run after edits |
| `mcpServers` | external MCP servers to spawn at startup |
| `environment` | enables the LLM permission classifier for undecided bash commands |

## `.env`

Credentials and model selection are usually kept in a `.env` at the project root:

```dotenv
ANTHROPIC_AUTH_TOKEN=sk-…
SIRBONE_MODEL=claude-opus-4-7
# or an OpenAI-compatible endpoint:
# OPENAI_API_KEY=…
# OPENAI_BASE_URL=https://api.groq.com/openai/v1
```

## Post-edit checks

`post_edit_check` runs a command after a file matching the glob is written — handy for
auto-formatting or linting:

```json
{
  "post_edit_check": {
    "**/*.rs": "cargo clippy --quiet -- -D warnings"
  }
}
```
