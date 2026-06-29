# Tools

Tools are how the agent acts on your machine. Each is a typed unit (input validated
via serde/schemars) registered in a `ToolRegistry`; the agent can call several in
parallel within a turn.

## Built-in tools

| Tool | What it does |
|------|--------------|
| `bash` | run a shell command (supports background jobs) |
| `read` | read a file (with offset/limit) |
| `write` | create or overwrite a file |
| `edit` | exact-string replacement in a file |
| `sed` | stream-edit a file |
| `grep` | search file contents |
| `glob` | match files by glob |
| `find` | find files by name/pattern |
| `ls` | list a directory |
| `web_fetch` | fetch a URL |
| `web_search` | search the web |
| `load_skill` | pull a skill's instructions into context |
| `wait` | poll/sleep until a background job advances |
| `note` | record a working note that survives context compaction |
| `code_map` | build/query a structural map of the codebase |
| `architect` | request a second-opinion design pass (opt-in) |
| `job_status` | inspect background `bash` jobs |
| `undo` | revert the last file mutation |

## Background jobs

`bash` with a background flag returns immediately and keeps running; the TUI info bar
shows a live gauge, and a result block lands when the job exits. The `wait` and
`job_status` tools let the agent track progress without busy-waiting.

## Skills

A **skill** is a reusable instruction block. It can be invoked two ways:

- by you, typing `/skill-name` in the TUI, or
- by the model, calling the `load_skill` tool.

Both inject the same skill body into the conversation.

Every tool call passes through the [permission pipeline](./permissions.md) before it
runs, and file mutations are [snapshotted](./sessions.md) so they can be rolled back.
