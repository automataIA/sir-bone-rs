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

## Algorithmic project memory (`historia`)

The `historia` tool reconstructs project memory on demand from every JSONL session under
`~/.sirbone/projects/<slug>/sessions/`. The JSONL schema is interpreted structurally: human
requests, assistant answers, plans, mutated paths, tool failures, and run status are distinct
search fields. Thinking, images, successful tool output, injected control messages, duplicate
compaction tails, and compaction boilerplate are removed deterministically.

An empty query returns recent project state. A query can be restricted to requests, assistant
answers, plans, files, problems, or status; an unrestricted search weights requests and plans
above incidental path/error matches. Results are bounded and newest-first after relevance, so
recalling context does not inject every raw transcript into the model context.

This memory is read-only: the chat sessions remain the single source of truth, and no LLM-authored
`HISTORIA.md` entry is required. Ask the agent what happened, why a solution was chosen, or to
continue an earlier plan and it will call `historia` explicitly.

Use `/historia` to continue from the latest relevant state, or add a date/topic such as
`/historia 2026-08-10 compaction`. The command runs the structured lookup before the model turn and
supplies its result with an explicit continuation contract: inspect the current workspace, resume
unfinished plans, reuse solutions that worked, and avoid repeating documented failed attempts.
