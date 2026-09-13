# Permissions

Every tool call is classified into a `Decision` — **Allow**, **Ask**, or **Deny** —
before it executes.

## The pipeline

The checks run in order:

1. **`--review-only`** — ahead of everything, including your own `allow` globs.
2. **`allow` globs** — matching calls are allowed outright.
3. **`soft_deny` globs / destructive heuristics** — matching or obviously-destructive
   calls are blocked (or routed to a prompt).
4. **MCP server trust**, then the **trust-root guard**: writing into
   `~/.sirbone/{system,prompts,skills}`, or to any `config.json` — the global one
   or a per-project `~/.sirbone/projects/<slug>/config.json` — asks first. Those
   files are how a policy or a hook command is defined, so writing one is a
   request to widen sirbone's own trust, not an ordinary edit. The guard reads
   each tool's `mutation_target`, so it covers every writer rather than a
   hardcoded list. Caches and session state in the same directories are not
   guarded; the match is on the filename.
5. **`pre_tool_use` hooks** — your own deterministic gate, decided by exit code.
   It can allow, deny, ask, **rewrite** the call, or **answer it without running
   the tool**; every verdict except "no opinion" skips the classifier below and
   the turn it would have cost. See
   [configuration](./configuration.md#pre_tool_use-exit-codes).
6. **LLM classifier** — only for *undecided, non-safe* bash commands, and only when an
   `environment` is configured. The classifier may also **rewrite** the command
   (`updatedInput`) to a safer equivalent before it runs.

A decision here governs whether the call *runs*. What it produced is a separate
question: [`tusk` filters](./configuration.md#tusk-filtering-tool-results) can
rewrite or withhold the **result** before the model, the session file, or the UI
sees it.

Anything that resolves to **Ask** is routed through a prompt bridge and surfaced as a
multi-choice dialog:

- **Allow once** — run this call, remember nothing.
- **Allow always** — run it and persist an editable glob rule to the per-project
  `permissions.allow` list, so the same call is auto-approved from now on.
- **Deny** (with optional free-text feedback forwarded to the model).

The dialog renders in the TUI (arrow keys / `Enter` / `e` to edit the glob / `Esc`),
as a numbered menu in the REPL, and in the VS Code extension when it drives sirbone with
`--input-format stream-json`. In a non-interactive headless run with no control channel,
**Ask** auto-denies. The same dialog also backs the [`ask_user`](./tools.md) tool, which
lets the model put a multiple-choice question to you mid-task.

## Configuring

The `permissions` key (in global or per-project [config](./configuration.md)) holds the
`allow` and `soft_deny` glob lists. The per-project file replaces the global
`permissions` section wholesale; an empty config keeps the legacy behavior.

```json
{
  "permissions": {
    "allow": ["bash:cargo *", "read:**", "grep:**"],
    "soft_deny": ["bash:rm -rf *", "bash:git push *"]
  },
  "environment": "local-dev"
}
```

> The classifier only engages when `environment` is set; without it, decisions come
> purely from the glob lists and the destructive-command heuristics.

## Safety net

Even an allowed mutation is reversible: file changes are captured in a per-run
workspace snapshot. See [Sessions & Snapshots](./sessions.md) for `/rollback`.
