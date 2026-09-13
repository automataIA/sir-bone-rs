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
| `grep` | search file contents |
| `glob` | match files by glob |
| `web_fetch` | fetch a URL (HTML converted to markdown) |
| `web_search` | search via the required `search2md` CLI |
| `load_skill` | pull a skill's instructions into context |
| `note` | record a working note that survives context compaction |
| `todo` | maintain the live step list for multi-step tasks — rendered as a checklist in the TUI, VS Code, and Zed (ACP plan) |
| `ask_user` | ask the user a multiple-choice question when the decision is theirs (library/approach choice, ambiguous requirement) |
| `code_map` | build/query a structural map of the codebase |
| `job_status` | inspect background `bash` jobs |
| `undo` | revert the last file mutation |
| `verify` | run the configured authoritative project command once; registered only when `oracle.test_command` exists |

`ask_user` questions include a short decision context and 2–4 choices. Each
choice has a stable label plus a concrete consequence or trade-off; frontends
show both. Single-question calls return the selected label; round calls return
answers keyed by question ID with value, option index, and origin (`user`,
`free_text`, or `dismissed`). The first choice is the recommendation. Historical
payloads with string-only options are still accepted, but new tool schemas
publish the explanatory object form.
With `SIRBONE_ASK_ROUNDS=1`, one call can contain 1–3 independent questions.
The TUI collects the choices locally and sends one aggregate reply; REPL and
bidirectional stream JSON accept one comma-separated reply such as `1,2,1`.
ACP v1 cannot represent a multi-question form, so its adapter displays native
permission dialogs sequentially while retaining one model-facing tool call.
Rounds remain experimental and default-off: live trials preserved the decisions
and reduced aggregate submissions and provider work, but latency was not stable
enough to justify changing the default. Disable them explicitly during an
ablation with `SIRBONE_DISABLE=ask:rounds`.

`ask_user` and `todo` are available only in interactive profiles. `undo` and
`job_status` remain available headless; `verify` is conditional on project
configuration, so projects without an authoritative command pay no tool-schema
cost for it.

## Truncated output

A tool result that exceeds the size budget keeps its head and tail and elides the
middle. Before the cut, the full text is written to
`~/.sirbone/projects/<slug>/spill/<hash>.txt`, and the truncation marker names the
file:

```
... (8412 lines truncated — full output: /home/you/.sirbone/projects/myproj/spill/a3f1....txt; narrow with grep/offset/limit) ...
```

So the elided part is one `read` away rather than one re-run away — which matters
most for `bash`, where re-running also repeats the command's side effects. Only
over-budget results are written; the file name is the content hash, so the same
output spilled twice is one file; and each project keeps at most 32 spill files
(256 MB), oldest pruned first. Turn it off with `SIRBONE_DISABLE=tool:spill`.

## Structural read (opt-in)

With `SIRBONE_READ_OUTLINE=1`, reading a supported source file whole (no
`offset`/`limit`) that runs past 80 lines returns its declaration outline instead
of every line:

```text
src/agent/state.rs — outline (882 lines, 14 decls)
    30 | pub async fn run(ctx: &mut AgentContext) -> Result<()>
   398 | pub(crate) async fn run_turn(ctx: &mut AgentContext) -> Result<AgentState>
elided: 1-29, 31-397, 399-467 — read ONLY these ranges with offset+limit; never guess their contents.
```

Reads that already ask for a range behave exactly as before, and the freshness
stamp still records the full file, so the guard against writing to a file you
have not read stays as strict as it was. Ablate with
`SIRBONE_DISABLE=read:outline`.

## The `patch` tool (opt-in)

`SIRBONE_HASHLINE=1` swaps `edit` out for `patch` — never both, so the model is
never choosing between two ways to say the same thing. Under the flag, `read`
prefixes a file with its content tag and numbers the lines:

```text
[src/lib.rs#4F2A]
  1| pub mod ablate;
  2| pub mod acp;
```

A patch cites those numbers rather than re-copying the text to be replaced, and
the tag is checked before anything is written: if the file changed since the
read, the call is refused instead of misapplied.

```text
[src/lib.rs#4F2A]
PUT 2.=2:
+pub mod acp;
+pub mod agent;
CUT 10.=14 @moved
PUT >40 @moved
```

| Form | Meaning |
|------|---------|
| `PUT A.=B:` | replace lines A through B with the `+` body |
| `PUT <A:` / `PUT >A:` | insert before / after line A (`>$` = end of file) |
| `CUT A.=B [@name]` | delete lines A through B, optionally capturing them into a register |
| `PUT <A @name` / `PUT A.=B @name` | paste a register into a gap or over a range |
| `MV dest/path.rs` | rename the file |
| `REM` | delete the file |
| `+TEXT` | one literal body line (a bare `+` is an empty line) |

Every address refers to the file as it was read; the edits are applied bottom-up,
so one never shifts the addresses of another. One file section per call.

## Background jobs

`bash` with a background flag returns immediately and keeps running; the TUI info bar
shows a live gauge, and a result block lands when the job exits. The `job_status`
tool lets the agent track progress without busy-waiting.

## Skills

A **skill** is a reusable instruction block. It can be invoked two ways:

- by you, typing `/skill-name` in the TUI, or
- by the model, calling the `load_skill` tool.

Both inject the same skill body into the conversation.

Sir Bone's native skill roots are `~/.sirbone/skills/` and `.sirbone/skills/`.
It also reads `~/.agents/skills/` and `.agents/skills/` for Agent Skills
compatibility. Prefer the user-level `.sirbone` root for portable personal
skills you want to back up and carry across machines; project roots are useful
only when a repository intentionally ships its own shared agent workflow.

Every tool call passes through the [permission pipeline](./permissions.md) before it
runs, and file mutations are [snapshotted](./sessions.md) so they can be rolled back.

### Web search backend

`web_search` runs `search2md search --no-cache --json` from `PATH`. The child process has a
20-second timeout and bounded stdout/stderr capture. Missing binaries, non-zero exits, invalid JSON,
and timeouts are surfaced as explicit tool errors; Sir Bone does not fall back to another engine.
Search results and fetched pages are untrusted data and must not override user or system instructions.
