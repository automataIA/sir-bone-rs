# Sir Bone

<div class="sirbone-demo-embed">
  <iframe src="/sir-bone-rs/demo/" title="Sir Bone TUI — live demo" loading="lazy"></iframe>
  <p class="sirbone-demo-cap">▲ Live TUI demo (WebAssembly, no API key). <strong>Click the terminal</strong> to give it keyboard focus, then try <kbd>Alt</kbd>+<kbd>P</kbd> (palette), <kbd>Alt</kbd>+<kbd>A</kbd> (about), or just type. <a href="/sir-bone-rs/demo/" target="_blank" rel="noopener">Open full-screen ↗</a></p>
</div>

**Sir Bone** is a Rust CLI coding agent: it streams responses from an LLM, executes
tools (bash, file ops, web fetch, …), and renders the conversation in a REPL or a
full-screen TUI. Think "Claude Code clone", written from scratch in ~10k lines of Rust.

It supports prompt caching, extended thinking, multimodal image input, per-project
configuration, workspace snapshots with rollback, and MCP servers.

## Why it exists

- **Single static binary.** No runtime, no node_modules — `cargo build` and run.
- **Provider-agnostic.** Anthropic out of the box; any OpenAI-compatible endpoint
  (OpenAI, Ollama, Groq, …) via two env vars.
- **Safe by default.** A permission pipeline classifies tool calls (allow / ask /
  deny) before anything touches your machine; mutations are snapshotted for rollback.

## See it without installing

There's an interactive **[live demo](./demo.md)** of the TUI running entirely in your
browser (compiled to WebAssembly, no API key, scripted responses). It's the fastest way
to get a feel for the interface before building anything.

## How to read these docs

- [Getting Started](./getting-started.md) — build, configure, first run.
- [Providers](./providers.md) — Anthropic vs OpenAI-compatible, model selection.
- [Configuration](./configuration.md) — layered JSON config, `.env`.
- [Tools](./tools.md) — the built-in toolset.
- [Permissions](./permissions.md) — how tool calls are gated.
- [The TUI](./tui.md) — keys, panels, slash commands.
- [Sessions & Snapshots](./sessions.md) — persistence and rollback.
- [MCP](./mcp.md) — connecting external tool servers.
