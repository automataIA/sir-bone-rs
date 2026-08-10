# Sir Bone Status

This file is the short, product-facing snapshot. It answers: what is default,
what is opt-in, what is experimental, and what must be true before release.

Last updated: 2026-08-10.

## Product Thesis

Sir Bone is a local Rust coding agent for supervised, spec-driven work. The
product bet is not maximum autonomy; it is mechanical reliability, reversible
actions, grounded claims, and token discipline in one native binary.

## Default Surface

These are considered product behavior, not lab-only experiments:

| Area | Default | Why it stays |
|---|---:|---|
| Permission pipeline | on | Core safety boundary for shell/file/MCP actions. **Ask** now surfaces as a 3-way multi-choice prompt (Allow once / Allow always / Deny+feedback); "Allow always" persists an editable glob to the per-project `permissions.allow`. Works in TUI, REPL, and the VS Code extension (via `--input-format stream-json`). |
| `ask_user` tool | on in interactive profiles | Lets the model put a contextual multiple-choice question to the user mid-task. It is omitted from ordinary headless profiles; the multi-question schema remains a separate opt-in experiment. |
| `todo` tool (live plan view) | on in interactive profiles | Claude Code/Codex-style step list the model keeps current on multi-step tasks. Rendered as a checklist in the TUI (`todo_block`), the VS Code webview, and Zed via ACP-native `sessionUpdate: "plan"`. `/plan` mode now routes through it (the old `plan` tool was removed in the 2026-06 trim). Untested on-bench; adopted for supervision value (user sees the plan), not measured quality. |
| Guided verification setup | on, offline | `sirbone setup-verification` and `/setup-verification` inspect structured Rust/Python/Node metadata, preview exact commands/config, and write only after explicit confirmation. The wizard also offers the opt-in `high_risk` preset; non-TTY mode is read-only (`--json`) or exits without writing. |
| Lifecycle hooks | on when configured | `pre_tool_use`, `post_tool_use`, and bounded `stop` checks provide deterministic policy/edit/completion feedback with no prompt tax. `hooks.presets: ["high_risk"]` adds the standard Ask flow for dependency, migration, schema/manifest, and recognizable public-API operations, including source writes through Bash redirection/heredoc; the paired pilot caught 12/12 risky controls, prompted on 0/6 safe controls, and made no model calls. |
| `verify` tool | conditional | Registered only when the project defines `oracle.test_command`; `/verify` remains available to the human. This avoids an always-on schema cost in unconfigured projects. |
| Workspace snapshots and rollback | on when available | Gives the user a reversible run boundary without touching project git. |
| Prompt cache and context tracking | on | Core token-efficiency path. Static prefix (system+tools) is cached; working-notes inject *after* the system prompt; `HISTORIA`/`CRONOLOGIA` never enter the prompt. Keep dynamic content out of the cached prefix. |
| Localize pre-pass | on | Long-standing navigation aid; cancellable in the current implementation. |
| Grounding instruction block | on, opt-out with `SIRBONE_NO_GROUNDING` | Product direction is right, but it still needs a focused A/B because it adds prompt weight. |
| `edit` nearest-match hint | on, opt-out with `SIRBONE_NO_EDIT_HINT` | Deterministic, only fires on an error path, and avoids a re-read turn. |
| `web_fetch` HTML to markdown | on | Compresses HTML at the source and drops script/style noise before context. |
| `web_search` via `search2md` | on when CLI is installed | Direct bounded subprocess; no daemon, endpoint env var, scraper fallback, or duplicate search implementation in core. Missing CLI fails loudly. |
| `sirbone stats` | on | Folds every local session (`~/.sirbone/projects/*/sessions/`) into one per-tool picture: calls, sessions using, errors, **result tokens**, and the same split bucketed by peak context. Offline, nothing leaves the machine. Answers the question no bench can — *does the model reach for this tool on real work, and does that change as context grows* — because bench workspaces are small by construction. `--stats-project=SLUG` filters, `--json` for machine output. |
| `doctor`, `audit`, `ground`, `env` | on | Offline/mechanical surfaces that do not depend on model obedience. `doctor` also reports the assembled system-prompt size and marks a non-default provider base URL (the API key is sent there — the redirected-endpoint bug class is silent by nature). `sirbone env` lists every env var the agent reads (+ current value, secrets masked; `--json`), with a test-gate that fails if a new `SIRBONE_*` read lands without a table row. |
| `login` credential wizard | on (interactive TTY) | Preset menu (Anthropic, z.ai/GLM, OpenRouter, Groq, Google AI Studio, OpenAI, custom) writes `~/.sirbone/.env` (0600) + a 15s-bounded connection test. Provider chosen by preset, not key prefix (opaque GLM tokens disambiguate by URL). Smart pickup of an already-exported key. Non-TTY falls back to seed-and-print. OAuth deliberately skipped: only 2 of the supported providers offer it. |
| Image attachments | on | One ingest path for every front-end: TUI `Ctrl-V`, REPL `/paste` and `/attach PATH`, VS Code composer paste, and `--image`. Stored content-addressed in `<session>.attachments/` (5 MB cap) so a resumed session still has the file. Vision capability is read from the resolved endpoint (`attachments::vision_supported`), not negotiated in the prompt — a non-Anthropic endpoint attaches and warns. |
| Headless `-p` and `--output-format json` | on | Needed for Harbor/CI/eval wiring and reproducible automation. |
| SpecBench cumulative scoring | on in the benchmark | Each task checkpoint reruns every prior checker. `cumulative_pass`, collision-free requirement identities and explicit regressions are the promotion signal; isolated task verdicts remain diagnostic and legacy `eval-v2` reports remain readable. Checker failures resume without repeating the provider attempt. Local gate: 23/23 SpecBench tests on 2026-08-09. |
| SpecBench failure packets | on in the benchmark | Every failed cumulative checkpoint emits a redacted, integrity-checked `failure-packet-v1` with identities, commits, bounded tool trace, churn and verdicts. `failure_packet.py --replay PACKET` reconstructs the recorded commit and reruns checkers only; a real synthetic replay reproduced the verdict with zero provider calls. |
| Persistent Plan contract | explicit/default-off | `--plan`, `SIRBONE_PLAN=1` and `/plan` share one short deterministic contract stored in the persistent note and projected to `todo`. Reads remain free; an incomplete contract blocks mutations. The redesigned paired campaign passed cumulative quality (Δ +0.222, CI95 [0.000,0.556]) while reducing tokens/calls about 26%; keep opt-in pending a monthly supervised pilot. |
| Zed ACP agent (`sirbone acp`) | on | JSON-RPC 2.0 / Agent Client Protocol server over stdio → native Zed Agent panel (streaming, tool cards, 3-way permission, session resume). Reuses `agent::run` + `ConfirmBridge`; no new deps. Supported topology: Zed remote-SSH into WSL (Linux-native paths). Setup: `editors/zed/README.md`. Gaps: `ask_user` maps onto `request_permission` best-effort; no structured diffs/`usage_update` yet. |

## Opt-In / Lab Surface

These are useful, but not default product behavior yet:

| Feature | Flag | Current decision |
|---|---|---|
| High-risk operation preset | `hooks.presets: ["high_risk"]` | **Keep explicit/default-off.** It uses the normal Allow once/always/Deny interaction, adds no model/schema/prompt cost, and remains silent on the frozen safe controls. A combined agent smoke found and then closed a Bash-heredoc bypass; exact project permission overrides still take precedence. |
| External `rag-bone` retrieval | project skill | Keep out of the native tool schema. Enable the skill per project: grep/find for exact lookup, BM25 for keywords, hybrid for conceptual/cross-file retrieval. |
| `SIRBONE_GROUND` post-run report | `SIRBONE_GROUND=1` | Useful advisory output; primary robust surface remains `sirbone ground FILE`. |
| Headless/REPL oracle gate | `--oracle` / `SIRBONE_ORACLE=1` | **Keep explicit and default-off.** The valid prospective v2 holdout caught and repaired all reproduced final failures (active 3/3 pass vs ablated 1/3), but this establishes the configured mechanism for that failure class, not a global default. TUI activation is a persisted project toggle. |
| Multi-question `ask_user` rounds | `SIRBONE_ASK_ROUNDS=1` | **Keep experimental and default-off; campaign closed 2026-08-09.** Valid live trials preserved 3/3 decisions, reduced aggregate submissions 6→2 and input tokens 74,258→36,520, but latency was unstable. Re-run only if reconsidering default promotion; remains ablatable with `SIRBONE_DISABLE=ask:rounds`. |
| Persistent Plan contract | `--plan` / `SIRBONE_PLAN=1` / `/plan` | **Keep explicit and default-off.** The deterministic redesign passed its paired cumulative gate and lowered provider work; a human-supervision pilot, not another model-only campaign, gates any default-on decision. |
| Prompt ablation | `SIRBONE_DISABLE=prompt:<block>` / `prompt:*` | Bench-only harness, zero effect when unset. Drops named system-prompt blocks (`grounding`, `git`, `historia`, `bugfix`, `minimal`, `style`, `ask`, `truthful`, `output_filter`, `debug_toolkit`, `user_appends`, `claude_md`); `prompt:*` is the naked arm and spares `claude_md`. Weight is readable offline via `sirbone doctor`; no default changes until a paired SpecBench run says which blocks earn their tokens (protocol: `docs/BENCH_DECISIONS.md` §Next Measurements 5). |
| `sirbone-lab` self-improvement control plane | offline shadow + sealed evaluators | Evaluator lock, one-file mutation gate, append-only archive and fail-closed Bubblewrap/static black-box paths remain. The opt-in dynamic path accepts rootless Docker or an explicitly isolated remote Podman Machine with enforced cgroup v2 limits and gVisor, streams JSONL challenges while scoring in the controller, and withholds raw/per-task results. The dedicated Hyper-V endpoint passes its probe and synthetic end-to-end evaluation; remote candidates enter disposable read-only volumes, never host mounts. A standalone broker keeps provider keys host-side behind expiring tokens, model/call/token/byte limits and append-only audit. Still inactive pending independently reviewed external tasks and broker-only generation networking; no mutator or auto-promotion. |

## Removed Surface

Architect and ACE-lite were removed from core on 2026-08-09. Their tools,
provider/configuration wiring, prompt steering, storage and active telemetry no
longer exist; old env variables cannot reactivate them. Historical benchmark
evidence remains in `docs/BENCH_DECISIONS.md` and old session JSON continues to
load by ignoring the former experimental fields. Existing files under
`~/.sirbone` are not deleted automatically.

## Final Integration Evidence

The combined Plan + high-risk SpecBench smoke
`bench/specbench/runs/end_to_end_plan_high_risk_20260809_v4` passed all 8/8
cumulative checkpoints and 12/12 final requirements in both arms, with no
failure packets. Plan was quality-neutral (CI95 `[0,0]`) and used 8.29% fewer
tokens and 6.45% fewer calls; churn increased 13.21%. Because this is a
one-repetition integration smoke, the three-repetition Plan campaign remains the
promotion evidence and both features remain default-off.

## Release Gate

Before a release candidate, run the cheap local gate:

```bash
cargo fmt --check
cargo check --all-targets
cargo clippy --all-targets -- -D warnings
cargo test
cargo run --example grounding_bench
cargo publish --dry-run --allow-dirty  # omit the flag on a clean release tree
```

Run broader checks when the environment supports them:

```bash
cargo deny check
bash playground/repl_tasks.sh
```

Provider-backed evals (`bench/eval_harness`, Harbor, full A/Bs) are not part of
every release candidate; run them when changing prompt/tool/agent behavior with
possible correctness or token-cost impact.

## Current Risks

- The separate `docs-site/sirbone-web` crate is currently out of sync with the
  shared TUI widgets (`render_confirm_dialog`, agent/tool types and scroll width)
  and does not compile. The main workspace and native mock TUI are green; repair
  the web adapter before the next docs-site deployment.
- The grounding instruction block is default-on but not yet isolated by a clean
  A/B in the current form.
- REPL/TUI raw-mode behavior needs a small tmux regression set because unit tests
  do not exercise terminal input well.
- **Context compaction (default-on) measurably hurts long sessions** (SpecBench
  2026-07-14: mid-task firing broke tasks; summaries lose decisions; −0.6
  stable_pass vs full history at a 24k window, while saving ~63% tokens). It
  stays as the only overflow guard, but needs turn-boundary firing and
  decision-preserving summaries.
- **The always-on context cost is dominated by tool schemas, not the prompt**:
  17 native tools ≈ 4315 tokens vs ~1700 for every sirbone-authored prompt block
  put together (`sirbone doctor` prints both plus a per-tool ranking). Unmeasured
  as *value*; see `docs/BENCH_DECISIONS.md` §Next Measurements 5. `code_map` was
  trimmed 762 → 332 tok on 2026-08-02 by removing the duplication between its
  `description()` and the schemars-generated `Op` variant descriptions —
  capability unchanged (`examples/code_map_bench.rs` identical, n=48). It was
  the outlier: `todo`, the next-dearest tool with an enum input, has no such
  duplication, so the remaining schema weight is genuine surface, not prose.
- **The aggregate Mission claim is still unproven.** Each removal and
  reinvestment has its own verdict in `docs/BENCH_DECISIONS.md`, but no run shows
  that together they raise *stable task resolution* on a wide benchmark — every
  bench we own uses 2–8 file (SpecBench) or single-file (ACB) workspaces. The
  harness for that proof landed 2026-08-10 in `bench/claw/` (Claw-SWE-Bench: 350
  real-repository tasks, control vs candidate at a fixed model, paired pass^k
  scoring with infrastructure failures excluded from the denominator). It is
  verified offline and **not yet run against a provider**; the Lite subset is
  underpowered for a promotion claim by design, so the decision needs the full
  350×3×2 campaign and the metered capacity to pay for it. Protocol and power
  note: `bench/claw/README.md`, `docs/BENCH_DECISIONS.md` §Next Measurements 7.
- **We cannot currently measure whether a tool earns its cost *on-bench*.** The
  `code_map` A/B (SpecBench session, 54 runs, 2026-08-02) returned a null because
  the model never called the tool once in 27 baseline runs — the holdout
  workspaces are 2–8 files, too small for a repo map to beat `grep`. ACB-V2 had
  already failed on the same question for the mirror-image reason (single-file
  tasks). No task set runs against a repository large enough to exercise
  navigation. `sirbone stats` now covers part of this gap from real sessions
  instead: it shows *usage*, though still not outcome.
- ~~**`code_map` output, not its schema, is the real cost.**~~ **Fixed
  2026-08-02.** `sirbone stats` over 33 real sessions found 15 `code_map` calls
  (5% of all tool calls) carrying **154k result tokens**, the largest of any tool
  — ~10k per call against `read`'s ~1.2k — with 12 of 14 calls landing on the
  16k-token truncation cap. On this repo `op=list` rendered 121k chars and was
  cut to 48k, so the model paid a context window for an alphabetically
  incomplete map. `op=list` now degrades instead of truncating: over a budget it
  returns one line per file (path + symbol count), complete, and names the
  drill-down. Measured here: **48 000 → 9 423 chars** for the whole-repo call,
  and `path="<dir>"` returns full signatures for a subtree (3–10k chars). Schema
  grew 332 → 407 tok/turn for the new field; that buys ~9.6k tokens back per
  call.
- Historical real-project telemetry predates the recent `ask_user`, `todo`, and
  verification work. Bench invocation proves the mechanisms fire, but does not
  replace broader natural-task evidence; use `sirbone stats` as the corpus grows.
- **Compaction has fired zero times in 181 local sessions** (peak context ever
  observed: 42.5k). The default-on feature documented above as harmful on long
  sessions has never actually engaged in real use here.
- **`git status` sits in the cached system-prompt prefix** (1655 chars, the
  second-heaviest block we author). It is dynamic by definition, so the prefix
  differs across sessions in the same dirty repo. Stable within a session, so the
  real cost may be small — measure the cache-hit rate before moving it.
- **HISTORIA's "completion requirement" clause costs ~15% tokens** with little
  measured retrieval value (12 writes vs 3 hits); demote to on-demand and
  re-measure.
- The existing SpecBench frozen holdout has already informed decisions. For
  open-ended candidate search it is a promotion set, not an unseen final
  holdout. Static and dynamic sealed protocols now exist, but real external task
  bundles still must be created and reviewed. Docker Desktop is intentionally
  unchanged; native WSL Docker/Podman expose cgroup v1 and are rejected. A
  dedicated Hyper-V Podman Machine, root-only `runsc` configuration and pinned
  image are active; sealed probe and synthetic end-to-end validation pass. A
  real independently reviewed bundle still gates confidential execution.
  Operational continuity is in
  [`SEALED_EVALUATOR_RUNBOOK.md`](SEALED_EVALUATOR_RUNBOOK.md).
- `CRONOLOGIA.md` is useful as a lab notebook, but product decisions should be
  mirrored here and in `docs/BENCH_DECISIONS.md` so the current state is readable.
  It is now rotated (>400-line tail moved to `CRONOLOGIA-archive-*`); recall
  decisions from this distillate, not the log.
