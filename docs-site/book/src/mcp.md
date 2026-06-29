# MCP

Sir Bone is a generic [Model Context Protocol](https://modelcontextprotocol.io) client.
It spawns MCP servers over stdio at startup, in the background, and registers their
tools before the first turn.

## Where servers are declared

- **Global** — `mcpServers` in `~/.sirbone/config.json`
- **Per-project** — `./.mcp.json`

The two are merged per server name; on a name collision the **per-project** entry wins.

```json
{
  "mcpServers": {
    "filesystem": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "/path/to/project"]
    }
  }
}
```

## Naming

Each remote tool is registered as `mcp__<server>__<tool>` so it never collides with a
built-in tool. From the agent's perspective it's just another tool, subject to the same
[permission pipeline](./permissions.md).

## Lifecycle

Servers are launched asynchronously at startup so they don't block the first prompt;
their tools become available once the handshake completes (before the first turn runs).
