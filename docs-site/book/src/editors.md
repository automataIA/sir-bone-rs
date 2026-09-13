# Editor Integrations

Sir Bone runs inside editors two ways: as an **ACP external agent** (Zed) and as
a **VS Code extension**. Both drive the same core agent — same tools, sessions,
permissions, and the live `todo` plan view.

## Zed (ACP)

Sir Bone speaks the [Agent Client Protocol](https://agentclientprotocol.com/)
(JSON-RPC 2.0 over stdio): Zed launches `sirbone acp` as a subprocess and talks
to it from the native Agent panel. No extension package needed.

What you get: streaming replies, tool-call cards, permission prompts (allow
once / allow always / reject), session resume, and the agent's live step list
rendered through ACP's native `plan` updates whenever the model uses the `todo`
tool.

Setup (Zed `settings.json`):

```json
{
  "agent_servers": {
    "Sir Bone": { "command": "sirbone", "args": ["acp"] }
  }
}
```

The agent must run in the same environment as the project files (native paths,
no Windows↔WSL translation). Full walkthrough per OS — including the
Windows + WSL remote topology — in the repo under
[`editors/zed/`](https://github.com/automataIA/sir-bone-rs/tree/main/editors/zed).

## VS Code

The extension in [`editors/vscode/`](https://github.com/automataIA/sir-bone-rs/tree/main/editors/vscode)
(`sirbone-vscode-<version>.vsix`) embeds a chat webview that drives
`sirbone -p --output-format stream-json --input-format stream-json`:

- streaming markdown replies (mermaid + syntax highlighting),
- tool boxes with IN/OUT rows and timing — the `todo` tool renders as a live
  checklist (completed struck-through, current step highlighted),
- permission and `ask_user` prompts answered from the UI,
- session history replay, slash commands, `@file` mentions.

Install: `code --install-extension sirbone-vscode-<version>.vsix`, then set
credentials once with `sirbone login` (the extension reuses `~/.sirbone/.env`).

Plan mode can be enabled from VS Code Settings with **Sir Bone: Plan Mode**
(`sirbone.planMode`). The extension then passes `SIRBONE_PLAN=1` automatically
on every new turn; the setting is off by default.

## Headless / other front-ends

Any front-end can integrate the same way the VS Code extension does: spawn
`sirbone -p --output-format stream-json` and read one NDJSON event per line
(`text`, `thinking`, `tool_start`, `tool_end`, `ctx`, `ask`, final `result`).
With `--input-format stream-json`, permission and multi-choice prompts arrive
as `{type:"ask"}` events and are answered on stdin.
