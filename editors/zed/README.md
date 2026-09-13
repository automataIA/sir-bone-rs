# Sir Bone in Zed (ACP external agent)

Sir Bone speaks the **[Agent Client Protocol](https://agentclientprotocol.com/)
(ACP)** — JSON-RPC 2.0 over stdio — so it plugs straight into Zed's **Agent
panel** as a custom external agent. No extension package: Zed launches the
`sirbone` binary as a subprocess and talks to it.

What you get: streaming replies, tool-call cards, permission prompts (allow
once / allow always / reject), and session resume — all in the native Agent
panel.

## Topology: Zed on Windows, code in WSL

The supported setup is **Zed connected to WSL over SSH (remote project)**. Zed's
remote server runs *inside* WSL and launches `sirbone` there, so the agent shares
the project's Linux filesystem and cwd — paths are native Linux, **no
Windows↔Linux translation**. (Running the Windows `sirbone.exe` against a
`\\wsl$` path is not the supported path and needs translation Sir Bone does not
do.)

### 1. Build / install sirbone in WSL

```bash
cargo install --path .          # → ~/.cargo/bin/sirbone
# or: cargo build --release     # → target/release/sirbone
sirbone login                   # writes ~/.sirbone/.env with your provider key
```

### 2. Open the WSL project from Zed (remote)

In Zed on Windows: open the project over SSH into your WSL distro (Zed's remote
development flow). All the agent config below goes in the **Zed settings used on
the WSL/remote side**.

### 3. Register Sir Bone as a custom agent

`agent: open settings` → **External Agents** → **Add Custom Agent**, or edit
settings directly:

```json
{
  "agent_servers": {
    "Sir Bone": {
      "type": "custom",
      "command": "/home/<you>/.cargo/bin/sirbone",
      "args": ["acp"],
      "env": {
        "ANTHROPIC_AUTH_TOKEN": "sk-...",
        "SIRBONE_MODEL": "claude-opus-4-7"
      }
    }
  }
}
```

`command` must be the **absolute** Linux path to the binary. If your key already
lives in `~/.sirbone/.env` (via `sirbone login`), you can drop the `env` block —
Sir Bone loads it on start.

### 4. Use it

Open the Agent panel, start a new thread, pick **Sir Bone**, and prompt. Tool
calls surface as cards; destructive ones ask for permission. Inspect the wire
with `dev: open acp logs`.

## What's implemented

- `initialize` — protocol v1, advertises `loadSession` + image prompt capability
  (image only on a vision-capable Anthropic endpoint).
- `session/new`, `session/load` (replays history into the panel), `session/prompt`,
  `session/cancel`.
- Streaming `session/update`: agent text, thinking, and tool-call start/update.
- Tool permission via `session/request_permission` (allow once / allow always /
  reject), backed by Sir Bone's own permission policy.

Sessions persist as JSONL under `~/.sirbone/projects/<slug>/sessions/` — the same
store the TUI and CLI use, so threads are shared across surfaces.

### Known gaps

- The `ask_user` tool's free-form questions map onto `session/request_permission`
  best-effort (ACP has no dedicated question method).
- File edits stream as text tool-call content; structured ACP diffs and
  `usage_update` are not emitted yet.
- Multi-cwd: the tool set and system prompt are built for the launch cwd; one
  Zed remote window = one project.

## Testing

- Mapping logic (unit): `cargo test --lib acp`
- Live handshake (needs a provider key): `bash playground/acp_smoke.sh`
