# Permissions

Every tool call is classified into a `Decision` — **Allow**, **Ask**, or **Deny** —
before it executes.

## The pipeline

The checks run in order:

1. **`allow` globs** — matching calls are allowed outright.
2. **`soft_deny` globs / destructive heuristics** — matching or obviously-destructive
   calls are blocked (or routed to a prompt).
3. **LLM classifier** — only for *undecided, non-safe* bash commands, and only when an
   `environment` is configured. The classifier may also **rewrite** the command
   (`updatedInput`) to a safer equivalent before it runs.

Anything that resolves to **Ask** is routed through a confirmation bridge — in the TUI
this is the `y/n` dialog.

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
