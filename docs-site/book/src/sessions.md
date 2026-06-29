# Sessions & Snapshots

## Sessions (JSONL)

Every run is persisted as an append-only JSONL file under
`~/.sirbone/sessions/<uuid>.jsonl` — one record per message/event. Resume any session:

```bash
cargo run -- --session ~/.sirbone/sessions/<uuid>.jsonl "follow-up question"
```

This replays the prior transcript into context and continues from there.

## Context compaction

Long conversations are compacted automatically: when the context reaches ~87.5% of the
window, older messages are summarized by the LLM and the most recent few are kept
verbatim. The TUI shows a one-shot warning as you approach the threshold, and the info
bar tracks live context usage.

## Workspace snapshots & rollback

Before the first mutation of a run, Sir Bone takes a **shadow-git** snapshot of the
workspace. File-mutating tools build on it, so you can undo a whole run's changes:

- `/rollback` — list available snapshots
- `/rollback <n|id>` — restore a specific snapshot

The `undo` tool reverts the most recent single mutation, while `/rollback` restores an
entire snapshot — your safety net when an automated edit goes wrong.

## Project memory (`HISTORIA.md`)

Each project gets a persistent, human-readable memory log at
`~/.sirbone/projects/<slug>/HISTORIA.md` — kept **outside** your repository, next to the
other per-project state (`meta.json`, `sessions/`, snapshots). It's seeded with a header
the first time Sir Bone runs in the project.

After the agent changes project files it prepends a timestamped entry, newest first:

```markdown
## 21/06/2026 - 11:45 — Switch greeting to "hello"
- Changed the printed string in `main.rs`
```

The agent reads this file back when starting work that depends on earlier decisions, so
it carries context across sessions — and because it's plain markdown, you can read or
edit it by hand. The agent is told to maintain it through its system prompt, so how
consistently entries get written depends on the model driving the session.

