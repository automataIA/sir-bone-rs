# Configuration

Sir Bone reads layered JSON configuration plus environment variables (`.env` is
loaded automatically).

## Layers

1. **Global** — `~/.sirbone/config.json`
2. **Per-project** — `~/.sirbone/projects/<slug>/config.json`

Merge rules:

- Most sections in the per-project file **replace** the global section wholesale.
- `mcpServers` is merged **per key** (see [MCP](./mcp.md)).
- An empty config falls back to legacy default behavior.

## Common keys

| Key | Purpose |
|-----|---------|
| `permissions` | allow / soft-deny globs + classifier settings — see [Permissions](./permissions.md) |
| `post_edit_check` | map of file glob → lint/format command run after edits |
| `hooks` | deterministic presets plus `pre_tool_use`, `post_tool_use`, `tusk` result filters, and bounded `stop` checks |
| `oracle` | authoritative command run after `Done` when explicitly enabled |
| `mcpServers` | external MCP servers to spawn at startup |
| `environment` | enables the LLM permission classifier for undecided bash commands |

## `.env`

Credentials and model selection are usually kept in a `.env` at the project root:

```dotenv
ANTHROPIC_AUTH_TOKEN=sk-…
SIRBONE_MODEL=claude-opus-4-7
# or an OpenAI-compatible endpoint:
# OPENAI_API_KEY=…
# OPENAI_BASE_URL=https://api.groq.com/openai/v1
```

## Guided verification setup

Run `sirbone setup-verification` from the project root, or
`/setup-verification` in the TUI. The wizard reads only structured manifests:
Cargo metadata, Python `pyproject.toml` tool sections, and declared
`package.json` scripts with a detected Node lockfile. It performs no LLM or
network call. It shows the command, directory, source, and exact JSON patch
before offering **Run and save**, **Save without running**, **Edit**, or
**Cancel**. A failed command needs a second confirmation before it can be saved.

With no TTY it writes nothing. `sirbone setup-verification --json` prints the
candidate commands and their sources without modifying configuration. Manual
commands and glob/command pairs cover ecosystems without a built-in detector.
The atomic project write preserves unrelated keys and never creates `pre` or
`stop` hooks automatically. The wizard separately offers the opt-in
`high_risk` confirmation preset and includes that choice in the exact patch
shown before saving.

```json
{
  "oracle": {
    "test_command": "cargo test -q",
    "max_attempts": 3
  },
  "hooks": {
    "post_tool_use": {
      "*.rs": "cargo check -q --message-format=short"
    }
  }
}
```

## Hooks and oracle

`post_tool_use` is advisory: matching checks run once per distinct command and
failures are appended to tool feedback. `pre_tool_use` is an advanced policy
gate; `stop` is a bounded completion-invariant gate (exit 2 requests another
turn). Output is bounded and all hooks time out. The built-in `high_risk`
preset returns the normal interactive **Ask** verdict before dependency or
migration operations, manifest/schema changes, and recognizable public-API
edits. It never approves an operation automatically.

```json
{
  "hooks": {
    "presets": ["high_risk"],
    "pre_tool_use": [{"match": "bash", "command": "./policy-check"}],
    "post_tool_use": {"*.py": "python -m ruff check ."},
    "tusk": [{"match": "*", "command": "./redact-secrets"}],
    "stop": ["./completion-invariant"]
  }
}
```

### `pre_tool_use` exit codes

The hook receives `{"tool": …, "input": …}` on stdin and decides with its exit
code. Exits `0`, `4` and `5` skip the LLM command classifier.

| Exit | Meaning |
|------|---------|
| `0` | allow |
| `2` | deny — the output is the reason the model reads |
| `3` | ask — route through the normal interactive confirmation |
| `4` | allow a **rewritten** call — stdout is a JSON object merged into the tool input |
| `5` | **short-circuit** — stdout *is* the tool result and the tool never runs |
| anything else | no verdict: fall through to the normal permission path |

A hook that exits `4` without printing a JSON object is a configuration error,
and the call is denied rather than run unchanged. Use `4` to normalize a command
(`npm` → `pnpm`), `5` to answer from a cache or to replace a built-in tool with
your own implementation.

### `tusk`: filtering tool results

`tusk` filters see a tool result **before anything else does** — the model's
context, the session transcript and the UI are all written from the single point
where the filter runs. This is where a secret a command printed can be removed,
which no `post_tool_use` check can do (it only appends).

The result arrives raw on stdin, so an ordinary text filter is a valid hook and
a chain of them composes like a shell pipeline (each filter sees what the
previous one produced). Metadata comes through the environment: `SIRBONE_TOOL`,
`SIRBONE_TOOL_INPUT` (the tool input as JSON), and `SIRBONE_IS_ERROR` (`0`/`1`).
Filters run on failed calls too — an error quoting the command that produced it
is exactly as likely to carry a secret.

| Exit | Meaning |
|------|---------|
| `0`, no output | pass the result through unchanged |
| `0`, with output | stdout replaces the result |
| `2` | withhold the result; the output becomes the reason |
| anything else, spawn failure, timeout | **withhold** — a filter that fails open is not a filter |

Because any non-zero exit withholds, a filter must exit `0` on the happy path:
write `grep -v SECRET || true`, since `grep` exits `1` when it selects no lines.
Configuring any `tusk` filter also disables spill-to-file for the run — spilling
happens inside the tool, before a filter can see the content, and would leave
the unfiltered original on disk.

#### A filter you can actually use

Redact anything shaped like a key, so a stray `env`, a `cat .env`, or a stack
trace quoting a token never becomes part of the transcript:

```bash
# ~/.sirbone/redact-secrets — chmod +x
#!/bin/sh
sed -E \
  -e 's/(sk-[A-Za-z0-9_-]{8})[A-Za-z0-9_-]+/\1…[redacted]/g' \
  -e 's/(gh[pousr]_[A-Za-z0-9]{4})[A-Za-z0-9]+/\1…[redacted]/g' \
  -e 's/((API_KEY|TOKEN|SECRET|PASSWORD)[[:space:]]*=[[:space:]]*).+/\1[redacted]/g'
```

```json
{"hooks": {"tusk": [{"match": "*", "command": "~/.sirbone/redact-secrets"}]}}
```

`sed` exits `0` whether or not it substituted anything, which is exactly the
behaviour a filter needs. The token rules keep the first few characters on
purpose (`sk-ant-api0…[redacted]`): the model can still tell two different keys
apart, and still see that a value *was* there, without learning it. The
`KEY=`/`TOKEN=` rule is deliberately blunter and drops the value entirely.

To refuse a whole result instead of editing it, exit `2` — here, rather than let
the model read a production dump at all:

```sh
#!/bin/sh
if grep -qi 'BEGIN .*PRIVATE KEY'; then
  echo 'result withheld: it contained a private key' >&2
  exit 2
fi
# nothing to say: pass the result through untouched
exit 0
```

That script consumes stdin in `grep`, so it prints nothing on the happy path —
and exit `0` with no output means "unchanged", not "empty result".

Verify a filter before trusting it, without spending a model call:

```bash
printf 'export API_KEY=sk-live-abcdefghijklmnop\n' | ~/.sirbone/redact-secrets; echo "exit=$?"
```

Once it is configured, `tusk_runs`, `tusk_edits` and `tusk_withheld` in
`sirbone stats` tell you whether it ever actually fired. **A filter that never
ran is not protection** — zero edits on a run that printed a secret means the
glob or the pattern is wrong, not that you were safe.

#### Asking sirbone to write the hook for you

Nothing here has to be typed by hand: "add a tusk filter that redacts API keys"
is a normal request, and the agent writes both the script and the config entry.
Two things are worth knowing before you accept the edit.

**It takes effect on the next turn, not the next launch.** Configuration is read
from disk on every agent run, so a hook written during one turn is live for the
following one. The exception is spill-to-file, which is switched off once per
run: the turn that *creates* the first filter still has spilling on.

**Writing the config asks first, and that prompt is the point.** Both
`~/.sirbone/config.json` and the per-project
`~/.sirbone/projects/<slug>/config.json` are inside the trust root, so a write to
either routes through confirmation even under a permissive policy — see
[trust](./trust.md). A `hooks` entry is a command that will run in a shell on
every matching call, and a `permissions` entry rewrites the gate itself, so read
the diff rather than approving it reflexively. The script the entry points at is
an ordinary file at an ordinary path and is *not* guarded; the confirmation you
get is for the config, not for the code it names.

#### Trimming noise you would otherwise pay for

Redaction is the motivating case, not the only one. A filter sees the **raw**
result, so whatever it removes is never paid for: a build log's thousands of
lines, an `npm` progress bar, Docker timestamps — all of it is input tokens
charged on *every* turn for as long as it stays in context.

```json
{"hooks": {"tusk": [
  {"match": "bash", "command": "grep -v 'warning: unused' || true"},
  {"match": "bash", "command": "tail -200"}
]}}
```

Filters chain in order, each seeing what the previous one produced.

This is not what truncation and spill-to-file do, and the difference is the
reason configuring a filter switches spilling off. Truncation fires only on an
oversized result, keeps its head and tail, and writes the *full* original to
`~/.sirbone/projects/<slug>/spill/` so the elided middle is still recoverable —
the noise is preserved, on disk, by design. A `tusk` filter runs on every result
and the text it removes never exists anywhere.

The oracle is authoritative only after the agent declares `Done`. Configuration
alone does not activate it in headless or REPL runs: pass `--oracle` or set
`SIRBONE_ORACLE=1`. The TUI keeps its per-project `/oracle` toggle. The
model-facing `verify` tool is registered only when `oracle.test_command` exists;
the human `/verify` command remains available.

Tool profiles remain cost-aware: `ask_user` and `todo` are registered only for
interactive frontends, while `undo` and `job_status` remain available headless
because prior experiments established their recoverability value. Verification
adds no system-prompt instruction; deterministic output reaches the model only
when a hook, oracle, or explicit `verify` run actually executes.

For controlled ablations, set comma-separated `SIRBONE_DISABLE` entries:
`hook:pre`, `hook:post`, `hook:tusk`, `hook:stop`, `oracle:gate`, or `ask:rounds`. Ablation is
applied after loading configuration, so paired arms receive identical commands.
The experimental multi-question `ask_user` schema is enabled with
`SIRBONE_ASK_ROUNDS=1`; it remains disabled by default. Interactive trials
validated aggregate replies (fewer submissions and provider calls with the same
decisions), but did not establish a stable latency benefit or enough evidence to
change the model-facing default. The campaign is closed unless default promotion
is reconsidered. Both the single-question and round schemas require decision
context and an explanation of the consequence or trade-off for each option.
Old string-only payloads remain readable for session replay.

Verification counters are emitted in `[usage]`, persisted in session telemetry,
and aggregated by `sirbone audit` and `sirbone stats`: hook runs/failures/retries,
tusk runs/edits/withholdings, oracle runs/failures/retries/rollbacks/exhaustion,
and ask-user rounds/questions.
`verify_tool_runs` distinguishes explicit one-shot verification from the
post-`Done` oracle gate. A zero run count means the mechanism was not exercised,
not that it had no value; smoke results with zero invocations are non-discriminating.
The files under `info/` may motivate hypotheses, but they are neither runtime
dependencies nor stronger evidence than the paired benchmarks required by
`MISSION.md`.

## Legacy post-edit checks

`post_edit_check` runs a command after a file matching the glob is written — handy for
auto-formatting or linting. It remains supported as an alias for
`hooks.post_tool_use`:

```json
{
  "post_edit_check": {
    "**/*.rs": "cargo clippy --quiet -- -D warnings"
  }
}
```

A check re-runs after every batch of edits, so its output would otherwise be
re-appended in full each time — on a file with pre-existing warnings, the model
pays for all of them on every edit. Sirbone keeps a per-command record of the
diagnostics it has already shown (matching them by message, not by line number,
so they survive the lines around them moving) and reports only what is new.
Anything already reported collapses into a counted reminder that the check is
still failing; a red check never goes quiet. Turn the deduplication off with
`SIRBONE_DISABLE=hook:ledger`.

## Stream rules

A project constraint written into the system prompt is paid for on every single
turn, whether or not the model was about to break it. A stream rule costs nothing
until it fires: its pattern watches the response as it is being generated, and the
first match stops the response mid-sentence, hands the rule back to the model as a
reminder, and restarts the turn — so the model reads the rule exactly when it is
about to violate it.

```json
{
  "stream_rules": [
    {
      "name": "box-leak",
      "pattern": "Box::leak",
      "message": "Never Box::leak in production paths; use Arc<str> instead."
    }
  ]
}
```

`pattern` is a regular expression matched against the tail of what has been
generated so far. An invalid pattern is skipped with a warning rather than
failing the run.

The mechanism is bounded: a turn can be restarted at most twice, and a rule that
has fired is disarmed for the rest of that turn, so a model that ignores the
reminder still finishes. The aborted text is discarded and never enters the
conversation. With no `stream_rules` configured nothing changes; ablate a
configured set with `SIRBONE_DISABLE=stream:rules`.
