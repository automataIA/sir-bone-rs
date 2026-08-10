# Benchmark Decisions

This file is the decision table. `CRONOLOGIA.md` keeps the narrative; this file
keeps the product verdicts short enough to audit before changing defaults.

Last updated: 2026-08-10.

> **Telemetry scope correction (2026-07-11):** ACB results produced before
> `eval-v2` aggregate correctness across all seeds but retain detailed `records` and
> usage only from seed 0. Correctness/pass verdicts below remain valid. Multi-seed cost
> figures are historical `seed-0-only` estimates and must not be used as paired cost
> evidence for a new promotion. New runs store every attempt and use
> `stable_pass_k` plus `compare_eval.py` paired intervals.
>
> **Cost-accounting correction (2026-07-24):** paired efficiency is now the
> arm-wide ratio `sum(cost of every attempt) / sum(stably accepted tasks)`.
> Failed tasks therefore keep their cost in the numerator. Missing provider
> usage in either arm makes token/call evidence incomplete and blocks promotion
> instead of silently dropping the affected task or treating its cost as zero.

## Default / Flag Decisions

| Feature | Default | Evidence | Result | Decision |
|---|---:|---|---|---|
| High-risk hook preset | off; `hooks.presets: ["high_risk"]` | Offline paired pilot 2026-08-09, three counterbalanced repetitions: 12/12 risky dependency/migration/schema/public-API operations and 6 safe controls. The first combined agent smoke then exposed a Bash-heredoc public-API bypass; its failure packet replayed exactly with zero provider calls, the recognizer was extended, and `end_to_end_plan_high_risk_20260809_v4` closed 8/8 cumulative checkpoints | Risk recall 100%; safe prompts 0; maximum one prompt per operation; no cumulative-verdict regression. Both pilot arms made zero model calls/tokens. The combined rerun verifies the repaired route and a narrow explicit permission override, but its single repetition is integration evidence rather than promotion evidence | **Keep explicit/default-off in the existing hook engine.** Use the standard Ask and Allow once/always/Deny path; require supervised natural-task evidence before considering a default. |
| Post-edit hook (`hook:post`) | on when configured | SpecBench removal A/B 2026-08-09, one mechanism task ×3 paired repetitions. Active: 6 checks, 3 failures caught, 3/3 final pass. Ablated: 0 checks, 3/3 final pass | Removing it raised calls 21→25 (+19.0%) and tokens 92,903→106,869 (+15.0%); churn 9→8. The one-scenario paired intervals exclude zero, but generality remains narrow | **Keep configured-only.** It caught every reproduced intermediate error and shortened the trajectory without global prompt/schema cost. Do not infer value for untested commands/ecosystems. |
| Stop hook (`hook:stop`) | on when configured | SpecBench removal A/B 2026-08-09, non-test completion invariant ×3: active **3/3**, ablated **0/3**; ablation `stable_pass` Δ −1.0, CI95 [−1.0,−1.0]. Active: 8 runs/5 retries/0 exhausted | Eliminates the reproduced critical failure in every pair. The failed ablated arm is cheaper only because it stops with the invariant violated, so cost is not quality-gated/comparable | **Keep configured-only**, bounded. This is evidence for an invariant not duplicable by the ordinary test command, not permission to configure stop automatically. |
| Headless oracle gate (`oracle:gate`) | off; explicit `--oracle`/env | SpecBench removal A/B `oracle_gate_v2_20260809T074635Z`, prospectively frozen holdout ×3 paired repetitions. Active: **3/3 pass**, 6 oracle runs, 3 failures caught, 3 retries, 0 exhausted. Ablated: **1/3 pass**, 0 oracle runs; ablation `stable_pass` Δ −1.0, CI95 [−1.0,−1.0] | The gate eliminated all three reproduced final failures. Each active run first declared `Done`, received the authoritative failure and passed after one retry. The sole ablated pass came from an autonomous early fix. Cost is not quality-comparable because 2/3 cheaper ablated runs ended defective; mechanism check is discriminating | **Keep explicit and default-off.** This validates the configured headless gate for the reproduced failure class, not global activation. Require the monthly human pilot and broader natural-task evidence before reconsidering the default. The contradictory v1 campaign remains invalid and is not pooled with v2. |
| `ask_user` question rounds | off (`SIRBONE_ASK_ROUNDS=1`) | Scripted protocol A/B 2026-08-09: same 3/3 structured decisions and model-facing calls 3→1. The first two live attempts were invalidated by, respectively, a missing prompt bridge and an underspecified fixture; neither is pooled. Valid schema-v2 run `ask-live-20260809T082824Z-c092f2ed`, participant `dev-01`, legacy-first: identical decisions and zero ambiguity/errors; tool calls 3→1, agent calls 4→2, input tokens 37,020→18,592, elapsed 61→39 s, but human submissions stayed 3→3, so its gate failed. Two valid schema-v3 runs, `ask-live-20260809T085753Z-1ff7fd44` (`dev-01`) and `ask-live-20260809T090348Z-2b176975` (`dev-02`), both round-first: identical 3/3 decisions and zero ambiguity/errors/protocol errors in every arm; aggregate human submissions **6→2**, tool calls 6→2, agent calls 8→4 and input tokens 74,258→36,520 (−50.8%). Telemetry is exact in both runs and both four-part promotion gates pass. | Aggregate replies consistently remove two human submissions and halve provider work, so the bridge mechanism is validated. Wall time remains unstable: schema-v3 aggregate is 57→83 s (+45.6%), because the first run regressed 30→63 s while the second improved 27→20 s. Both schema-v3 runs used the same fixture and round-first order; participant identifiers differ, but logs cannot establish human independence. There is no counterbalanced or interval evidence, and ACP v1 remains sequential at the UI boundary. | **Keep experimental and default-off; campaign closed 2026-08-09.** Keep the aggregate implementation; do not claim a latency benefit or promote the model-facing schema from this sample. No further round benchmark is planned unless promotion to default is reconsidered; that future decision would require counterbalanced, independent evidence and the cost/non-regression gate. Remains ablatable with `ask:rounds`. |
| Model-facing `verify` tool | only when `oracle.test_command` exists | SpecBench removal A/B 2026-08-09, visible-failure task ×3: active **3/3**, ablated **0/3**; ablation `stable_pass` Δ −1.0, CI95 [−1.0,−1.0]. Active 5 invocations, ablated 0 | Eliminates the reproduced critical failure in every pair. Failed ablated runs do not pass the quality gate for cost comparison | **Keep conditionally registered.** No broader/default registration: unconfigured projects continue to pay zero schema cost. |
| Compact persistent Plan contract | off; explicit `--plan`, `SIRBONE_PLAN=1` or `/plan` | Redesigned paired SpecBench campaign `plan_contract_20260809_v3`, `hist`+`long`, 3 repetitions, 54 checkpoints. Mechanism: initialized 27, updated 12, blocked 0 in Plan vs 0/0/0 baseline. Cumulative `stable_pass_k` Δ **+0.222**, CI95 **[0.000,0.556]**. The final combined smoke `end_to_end_plan_high_risk_20260809_v4` passed 8/8 checkpoints and 12/12 final requirements in both arms | Promotion gate passes (lower bound ≥ −0.05); Plan reduces tokens **26.7%**, calls **26.2%** and churn **35.2%** in the three-repetition campaign. The combined one-repetition smoke is quality-neutral, token −8.29%, calls −6.45%, churn +13.21%; it proves integration only. The first model-authored seven-section design is excluded because it regressed quality and cost | **Keep and distribute as explicit/default-off Plan mode.** The short deterministic contract removes the authoring turn and repeated reminders. Default-on promotion still requires the monthly human-supervision pilot. |
| System prompt, all sirbone-authored blocks (`SIRBONE_DISABLE=prompt:*,skill:*`) | on | SpecBench session A/B, stage 1 of the removal protocol, 2026-08-07: `long`+`hist` ×3 reps ×2 arms, 54 agent runs, glm-5.2. Prompt weight in-container 1235 → **49 tok/turn** (−96%) | **Quality exactly tied**: `stable_pass` Δ 0.000, CI [0.000, 0.000], n=9 — identical pass count on all 9 tasks. **Cost regresses out of band**: calls +14.1% (Δ +4.83/task, CI [0.44, 13.5], excludes zero) and churn **+85.0%** (111 → 206 lines, CI [62.9, 158.8], excludes zero; naked writes more on **9/9** tasks). Tokens −7.3%, CI crosses zero → neutral | **Keep the blocks.** The naked arm's lower bound is ≥ 0, so by the letter of the stage-1 rule the cut list is "free" — but it is free only on correctness. The blocks buy **shorter trajectories and restraint**, not accuracy, and both cost intervals exclude zero. Stage 2 changes purpose: not "add back to recover quality" but "find which block buys the conciseness" — start with `minimal` (240 chars, the churn suspect). Model-specific to glm-5.2. |
| HISTORIA completion-requirement clause (770 chars) | on | Same run, mechanism counters: `historia_writes` **24 with the clause, 24 without it**; hits 1 vs 0 | The clause's stated job — mandatory logging — happens identically when the clause is deleted. The tool schema alone drives the writes | **Cut, done 2026-08-07** (`src/main.rs`): the clause is gone, the tool and its one-line pointer stay. System prompt 19220 → 18450 chars. First free removal the protocol has produced: 770 chars for zero measured behavior change. |
| `verify` block (583 chars / 146 tok) | on | SpecBench session A/B 2026-08-07, 54/54 runs, prompt 1069 → 923 tok. Read first, measured second: the block is close to a literal subset of `grounding` | **Every metric neutral**: `stable_pass` +0.111 CI [0.0, 0.333] (gate passes), tokens +3.7%, calls +5.2%, churn +13.9% — all CIs cross zero; per-scenario the same. Failures: baseline 1 (genuine), ablated 0 | **Remove, done 2026-08-07**: block deleted, its one non-duplicated idea (AI summaries and "deep research" are not a source) merged into `grounding`'s external-facts bullet. System prompt 18450 → 17988 chars. Second free removal of the protocol. |
| `grounding` block (1324 chars / 331 tok) | on | SpecBench session A/B 2026-08-07, 54/54 runs, prompt 1069 → 738 tok. The largest sirbone-authored block and the only always-on prompt addition, never A/B'd alone | Quality tied (`stable_pass` 0.000, CI [−0.333, +0.333], 1 failure per arm). **Removing it costs tokens**: +27405 input tokens/task, paired CI95 [+7563, +52115] — excludes zero; on `long` **+50277/task**, CI95 [+17400, +89673]. On `hist` (short sessions) the effect vanishes: −1185 tok, CI [−7452, +4876] | **Keep.** First block with a *measured cost benefit*, not merely a non-negative one: 331 tokens of instruction save far more than they cost, and the saving grows with session length. Caveat: SpecBench cannot see the block's stated purpose — its tasks are code-grounded, so hallucination is not observable here. |
| `minimal` block, stage 2 (`SIRBONE_DISABLE=prompt:minimal`) | on | SpecBench session A/B 2026-08-07: 54/54 runs, 0 timeouts, 0 duplicate cells; prompt 1069 → 1009 tok | Headline `stable_pass` **+0.333 for the ablated arm is an artifact** — 3 of the 5 baseline failures are one poisoned `long` r1 session; excluding it, baseline 1 failure vs ablated 2, no quality signal. Real effect is on the clean `hist` scenario: dropping the block raises churn on **11/12 pairs**, **+7.33 lines/task, CI95 [+2.33, +13.25]** (paired bootstrap, excludes zero) | **Keep**, and stage 2 produced **no removal candidate**. The block does restrain output, but explains ~7 of the ~95 lines/task stage 1 attributed to the whole prompt: the restraint effect is **diffuse across blocks**, not concentrated in one. |
| UX tool group `ask_user`+`todo`+`note`+`job_status`+`undo` | on | SpecBench session A/B 2026-08-07: 54/54 runs; `native_tools` 17 → 12, schema 4390 → 3113. Call histogram over 27 baseline runs: `note` **14**, the other four **zero**. Per-tool schema cost: `ask_user` 369, `todo` 357, `job_status` 231, `undo` 120, `note` 200 tok | On the clean `hist` scenario the ablated arm spends **8406 fewer input tokens/task, CI95 [−15389, −672]** (excludes zero), matching the schema weight paid per call; calls and churn neutral. The overall quality delta is contaminated again — all 3 baseline failures are the compaction/context-window defect (peak 23.9k–35.0k against a 24k window) | **Change, done 2026-08-07**: `make_tools` takes an `interactive` flag; `ask_user` and `todo` are registered only for TUI/REPL/ACP/bidirectional stream-json. Headless now 15 tools / 3664 tok, **−726/turn**. Only two of the four uncalled tools are cut: `job_status` and `undo` work headless (background jobs, edit recovery), so removing them would be a capability cut, not a saving. |
| Compaction kept-window sizing | count-based | Three campaigns in a row lost baseline sessions to `Context window full after compaction` (peak 23.9k–35.0k against a 24k window, `compaction_fired` 1–2). Code read: `keep` is a fixed 6-message count against a token budget | Six recent messages carrying large tool results exceed the window on their own, so compaction succeeds and the next check still overflows. It also killed the paired quality channel on `long` in stage 1 and stage 2 | **Fixed 2026-08-07** (`src/agent/compact.rs`): the kept window is walked back from the newest message until it fits **half** the context window, floor of 2 so there is always a last exchange. Separately, the split boundary now walks back off a `tool_result` so a kept result never loses its `tool_use`. |
| `edit` nearest-match hint | on | Unit tests; ACB edit-fail base rate about 0.03/task | Not statistically measurable on ACB, but deterministic and only on failed edit path | Keep default-on; opt-out with `SIRBONE_NO_EDIT_HINT`. |
| `web_fetch` HTML to markdown | on | Unit tests for HTML/script/style/JSON/plain text; source compression pattern from headroom analysis | Removes markup noise before history/cache; no model cost | Keep default-on. |
| Agent Skills `.agents/skills` compatibility | on, read-only | `cargo test skills::tests`; loader precedence tests | Native `.sirbone` roots win; compat roots visible | Keep. `.sirbone` remains canonical. |
| TUI/LLM cancellation | on | Code path fix plus test suite/clippy; dogfood finding | `.send()`/stream open and localize pre-pass share cancellation | Keep. Add tmux regression coverage. |
| Headless JSON output | on | Sandbox/Harbor smoke in changelog; local test suite | Enables CI/Harbor wiring and machine-readable usage | Keep. |
| Grounding instruction block | on | Product rationale; no clean current A/B for the exact block | Prompt weight is about 1.4 KB/run | Keep temporarily, but run A/B against `SIRBONE_NO_GROUNDING`; shorten or demote if neutral. |
| `sirbone ground` deterministic claim check | on | `grounding_bench` 8/8; cold-plan detection caught real stale path claims with zero false facts in recorded bench | Robust no-LLM verification | Keep as primary claim-verification surface. |
| `SIRBONE_GROUND` post-run report | off | Same engine as `sirbone ground`; previous model reconciliation was unreliable | Advisory output is useful; feeding back to model was not robust | Keep opt-in. |
| ACE-lite playbook | removed | **SpecBench session A/B (2026-07-14)**: ace scenario 6 tasks ×3 reps, fresh session per task, persisted HOME. Quality ceiling-flat (stable_pass 1.0 both arms); tokens Δ n.s.; **mechanism dead: 0 lessons recorded/injected across all 18 ACE-arm sessions** (playbook.jsonl never created). Prior: ACB hardfail8 neutral. Removal gate 2026-08-09: full Rust suite and feature audit green; legacy session fields still load and are dropped on new serialization. | Inert as shipped on glm-5.2: with only the tool description, the model never records lessons, so there is nothing to transfer. Position curve flat. | **Remove (completed 2026-08-09).** Tool, storage, prompt injection, telemetry and env surface are out of core. Existing files under `~/.sirbone` are deliberately left untouched. |
| History hygiene | off | **SpecBench session A/B (2026-07-14)**: long scenario 5 tasks ×3 reps, one resumed session, 24k window. stable_pass Δ **−0.2, CI [−0.6, 0.0] → quality gate FAIL**; the decision-consistency task (t5: header must match the §1.4 rounding mode chosen at t1) fails 2/3 with pruning active (~6.7k tokens pruned each) vs 3/3 pass baseline; tokens −10.1% CI [−106k,−7k]. Prior: ACB hyg10 neutral-negative. | Net-negative exactly on the long-session bench it was designed for: superseded-read stubbing removes decision context; the token saving is bought with quality loss. | **Removed, done 2026-08-08**: `src/agent/hygiene.rs` deleted along with its call site, the `SIRBONE_HYGIENE` toggle, the `hygiene_pruned_tokens` counter and its `[usage]`/session-telemetry fields. Compaction is the only history rewriter left. Do not reintroduce unless the pruning rules protect decision-bearing content — the failure was semantic, not a tuning problem. |
| Context compaction | on (last-resort) | **SpecBench session A/B (2026-07-14)**: long ×3 reps at `SIRBONE_CONTEXT_WINDOW=24000`, treatment = `SIRBONE_NO_COMPACT` (full history; real window 128k absorbs it). Full-history arm stable_pass **+0.6, CI [0.2, 1.0]** (15/15 vs 12/15 task-reps); compaction-on failures: 2 tasks broken outright by **mid-task** compaction (rep0 t1/t2, 0/3 reqs) + t5 consistency R2 fails 2/3. Compaction saves ~63% input tokens (full history +169%). | The summary loses task-critical decisions and mid-task firing is destructive; but the feature is the only guard at a true window limit, so removal is not an option. | **Change**: keep as overflow guard; fix before trusting — compact only at turn boundaries (never mid-task), and/or carry forward decision/constraint lines verbatim; re-run the long scenario after the fix. |
| HISTORIA project memory | on | **SpecBench session A/B (2026-07-14)**: hist scenario 4 tasks ×3 reps, fresh session per task, treatment = `SIRBONE_NO_HISTORIA`. Quality equal (all 24 task-reps pass both arms — decisions were code-recoverable by design); **disabling saves 15.4% tokens** (CI [−39k, −2075], excludes zero); mechanism: 12 writes vs only 3 hits. | The mandatory-logging prompt clause ("completion requirement") buys writes, not retrieval value: the model logs every task but rarely reads the log back, and re-derives decisions from code at similar quality. | **Change**: demote the prompt clause from completion requirement to on-demand memory (keep the tool); re-measure the hist scenario after softening. |
| Anchor prompt directive | off | hardfail8 x3 seeds: +1 flip in band; calls +26% | Net-negative on mission cost | Keep off. |
| Oracle/self-review plan variants | off/removed | Multiple ACB rounds neutral or negative | Same self-deception ceiling, extra cost | Keep removed/default-off. |
| Architect | removed | SWE-bench mini ablation found net-negative; reviewer rewrote correct fixes into failing ones. Removal gate 2026-08-09: full Rust suite and feature audit green; old env values cannot restore the tool or prompt steering. | Harmful when enabled; its second client, tool schema and provider/configuration surface were unjustified. | **Remove (completed 2026-08-09).** Reintroduction requires a new design outside the core and new paired evidence. |
| Prompt-cache discipline | on | Code audit (`ai/anthropic.rs`, `agent/state.rs`) + arXiv 2601.06007 "Don't Break the Cache" | Already correct: static prefix (system+tools) cached, working-notes injected *after* system, `HISTORIA`/`CRONOLOGIA` never in prompt | Keep as-is; no change. Any new memory surface must keep dynamic content out of the cached prefix. |
| Multi-choice prompt (permessi 3-vie + `ask_user`) | on | UX/safety feature, not a quality lever — no A/B. Verified by unit tests (`PromptUi`, `suggested_glob`, `ask_user` e2e with/without bridge, `parse_repl_choice`/`parse_stdin_reply`), clippy `-D warnings` on `--all-targets`, and `tsc --noEmit`. Pattern adopted from Claude Code (3-option permission prompt; AskUserQuestion "Other" free-text). | Replaces the binary y/n confirm with Allow once / Allow always (editable glob → per-project `permissions.allow`) / Deny+feedback; adds `ask_user` for model-driven choices; VS Code made interactive via `--input-format stream-json`. | Keep default-on: strictly widens user control over the pre-existing Ask path; headless with no control channel still auto-denies. |
| Separate `MEMORY.md` active-memory file | off/rejected | `docs/STATUS.md`+`docs/BENCH_DECISIONS.md` already ~1.8k tokens of curated active memory | A third "curated memory" home = the dispersion it aimed to fix | Do not add. The distillate *is* MEMORY. |
| Embeddings / dense retrieval inside Sir Bone | off/rejected | Breaks the single-binary/dependency-light identity; native doc-search benchmarks favored grep | Bundling models in core adds cost to every install | Keep native core lean; use the separate `rag-bone` CLI/skill only for conceptual or cross-file retrieval. |
| Native `rag.rs` / `doc_search` | removed | Deterministic benchmark favored grep on recall and context bytes | Duplicate retrieval surface had negative ROI | Do not reintroduce; route exact lookup to grep and optional semantic work to external `rag-bone`. |
| `CRONOLOGIA.md` rotation (Claude Code dev-log) | on | Hit ~1954 lines / ~72k tokens before rotation | Whole-file re-read on every update was wasteful | Rotate tail >400 lines to `CRONOLOGIA-archive-*`; recall decisions from this distillate, not the log. |
| Extended thinking (`--thinking-budget`) as one-shot quality lever | on-demand (flag), **not on glm/z.ai** | ACB-V2 hard, 40×3 seeds, budget=10000 vs baseline: resolved_rate **0.725 vs 0.808** (−10/120 pairs), **pass^k 0.575 vs 0.725**, tokens/call +21%, peak_context +52%. Mechanism: **17 empty-solution runs vs 2** — the agent burns budget thinking and fails to emit `solution.<ext>` in one shot | **Net-negative as measured on glm/z.ai one-shot.** The theory (internalize the critique loop in one round-trip) did not survive contact with this model+harness: bloated context → no output. **Refined via the discriminative subset (10 hard ids, think_on not used in selection → non-circular): on hard tasks thinking is only −2/30 = within noise, and wins some (csharp/01 0/3→3/3).** So the damage is **regression on *easy* tasks (empty output), not degradation on hard ones** | **Do NOT recommend `--thinking-budget` on glm/z.ai coding.** Was a theory-led "higher-confidence lever"; measured net-negative, driven by breaking easy tasks not by hurting hard ones. An empty-solution nudge could in principle rescue it, but thinking buys nothing net on hard tasks (+csharp, −cpp/−rust) so not worth the complexity. Untested on Claude/Anthropic (may differ). Still: no external best-of-n/critic loop as default. |
| Planner/worker model routing (frontier decomposes, cheap executes) | **candidate, unmeasured** | External only: Cursor agent-swarm post (2026-07, `cursor.com/blog/agent-swarm-model-economics`) — same test-pass quality at $1,339 (Opus 4.8 planner + Composer 2.5 worker) vs $10,565 (GPT-5.5 throughout); worker-side alone $411 vs $9,373. Their claim: only decomposition and design decisions need frontier intelligence. **No sirbone evidence yet.** | Not the same lever as the removed `architect` advisor nor as plan/oracle: the strong model would run once up front and never re-enter. The former second-client infrastructure was removed on 2026-08-09, so this candidate would require an independently justified implementation. | **Do not build yet.** Third attempt at "a second model improves the run" after architect (net-negative) and plan/oracle (net-neutral) — prior is against it. Gate on a cost-first metric: promote only on **$ per stably-resolved task**, not `resolved_rate`. Design + kill-criteria in Next Measurements #6. |
| `quality-code` skill (one-shot quality rubric, no persona) | opt-in | ACB-V2 hard, 40×3 seeds, **force-injected** (progressive-disclosure fired only ~1/5 when enabled, too rare to measure — so the arm force-injects the body to test the rubric itself). resolved_rate 0.833 vs 0.808 control (+3/120 pairs); pass@k 0.900 vs 0.850; **pass^k 0.725 = 0.725 (no movement)**; tokens/call 11423 vs 12181 (−6%) | Direction favors treatment on 3 metrics with no cost penalty, but the swing (+3–4 tasks) sits at the noise band the runbook itself calls noise, and pass^k is flat — no task moved to stable-resolve. Even at 100%-inject ceiling it does not decisively move hard tasks | **Keep opt-in, do NOT promote to default** (within-noise gain). Net-neutral / weakly-positive, consistent with repo prior. Harness note: A/B needs the skill in the project `skills.enabled` allowlist inside the container — mounting the dir alone leaves it dormant. |

## Bench Inventory

Run `bench/eval_harness/feature_audit.py audit` before starting a new A/B. It
combines this decision table with automatically discovered native tools/toggles,
checks that ACB flags are actually forwarded, and assigns exactly one current
classification: `keep`, `change`, `remove`, or `insufficient evidence`.

| Bench | What it is good for | Caveat |
|---|---|---|
| Unit tests / proptests / httpmock | Pin deterministic behavior and parser/error paths | Does not prove agentic value. |
| `cargo run --example grounding_bench` | No-LLM correctness of claim-fact extraction | Narrow to path/symbol/count facts. |
| `sirbone stats [--stats-project=SLUG] [--json]` | Real-project tool usage folded across every local session: calls, sessions using, errors, **result tokens**, bucketed by peak context. Free, offline, and the only surface that sees a real repository. | Usage, not outcome — it cannot say whether a call helped. Corpus mixes binary versions (removed tools like `ls`/`sed` still appear in old sessions). Sessions predating a tool make it look unused. |
| `cargo run --example code_map_bench -- OUT.jsonl` | No-LLM capability of `code_map` vs `rg` on two repos (find_ref hit-rate, definer identification, output bytes), n=48 | Measures capability, not usage: it cannot tell whether the model *reaches for* the tool. Use it as the regression guard when editing the tool's schema/description. |
| ACB-V2 hard subset | Agentic correctness and cost sentinel across languages | Short sessions under-measure memory/hygiene value. |
| ACB discriminative subset (`acb_discriminative.json`, 10 ids) | **Fast, high-signal A/Bs**: drops the 26 saturated + 3 floor tasks that never flip, keeping only needle-deciders. ~4× shorter runs. Built by `build_discriminative.py` from output-normal arms (qc_off+qc_on, 6 obs/id). | **Model-specific** (tuned to glm — rebuild per model). N=6 is small, 5/6 tasks are semi-saturated. **Do not re-report a lever's delta on the same runs used to select the subset (circular).** Use for *future* A/Bs only. |
| ACB hardfail8/hyg10 | Focused feature A/B under hard/flaky tasks | Small N; use as directional unless paired over seeds. |
| Harbor / Terminal-Bench | End-to-end CLI/adapter validation | Provider overload can dominate score; do not fabricate leaderboard numbers from 529-heavy runs. |
| Dogfood REPL/TUI in Docker/tmux | Interactive path bugs, raw mode, confirm, resume | More manual today; should become a small scripted smoke. |
| SpecBench (`bench/specbench`) | Primary mission bench: spec adherence, repository changes, supervision, churn and cost after quality | Initial 16 historical tasks are calibration-only; fresh tasks alone may enter frozen holdout. |
| SpecBench session A/B (`run_session_ab.py` + `tasks/frozen_holdout.jsonl`) | The multi-task persistent-project bench ACB cannot be: ordered task sequences in one workspace+HOME (fresh or resumed sessions) for memory/long-session features; every checkpoint rechecks all prior requirements and uses collision-free `task_id::requirement_id` keys. Paired `compare_eval` now treats `cumulative_pass` as resolved while retaining isolated verdicts, mechanism counters and the quality/cost position curve. | Small n (4–6 paired ids ×3 reps) → wide CIs on quality; ace tasks saturated at 1.0; `long` runs at an artificial 24k window. Because these tasks already informed decisions, use them as a promotion set—not as the final sealed holdout for open-ended search. Checker-infrastructure failures resume from a saved attempt without a second provider call; legacy isolated-only `eval-v2` rows remain readable. Implemented and locally gated 2026-08-09 (23/23 unit tests, including failure packets and removal-counter audit). |
| SpecBench `failure-packet-v1` + replay | Failed cumulative checkpoints are bound to task/schedule/model/binary/checker hashes, workspace commits, bounded redacted tool trace, churn, isolated/cumulative verdicts and telemetry. Replay checks references, clones the staged repository at the recorded commit and invokes only the hidden checkers. | The repository and checker artifacts must still exist locally; missing or hash-mismatched references fail closed. Outputs are truncated but retain original size/hash. Real synthetic replay on 2026-08-09 reproduced the failed verdict exactly with `provider_calls=0`; 4 focused tests cover schema round-trip, redaction/bounds/integrity, missing references and provider isolation. |
| Claw-SWE-Bench (`bench/claw`) | **The only bench here that runs on real repositories at scale**: 350 GitHub issue-resolution tasks / 8 languages / 43 repos (80-task Lite), harness held constant, patch collected by the runner and scored by the official SWE-bench evaluator. Built to answer the Mission's first question — does a whole-agent change raise *stable* task resolution — which SpecBench (2–8 file workspaces) and ACB (single-file) structurally cannot. Paired scoring in `score.py`: pass^k, McNemar, cost per stably resolved task, `INVALID_*` excluded from the denominator. | **Harness ready, campaign not run** (2026-08-10). Expensive by construction: 2100 valid attempts at `--workers 1` is ~420 serial hours at a 12-min median, and a quota-limited plan cannot hold it. Lite is underpowered for a promotion claim (see Next Measurements 7) — it detects breakage, not improvement. Container network is on, so sirbone's `web_search`/`web_fetch` must stay ablated or the run is contaminated. |
| Prompt ablation arm (`SIRBONE_DISABLE=prompt:…`) | Prices the system prompt: one block or all of them (`prompt:*`), with the assembled size in `sirbone doctor` and `system_prompt_tokens` on `[usage]` | Measures *weight* for free (offline); measuring *value* still needs a paired SpecBench run. Verdict is model-specific — a block a weak model needs may be dead weight on a stronger one. |
| `sirbone-lab` offline control plane | Seals evaluator/config hashes, restricts candidate diffs and archives records append-only. Bubblewrap covers offline shadow runs; static black-box keeps file oracles outside. The dynamic path accepts rootless Docker or an explicitly isolated remote Podman Machine only when cgroup v2, required controllers, gVisor and a digest-pinned image pass the probe. Remote candidates are streamed into disposable read-only volumes; bounded JSONL challenges are scored in the controller. The dedicated Hyper-V endpoint passes probe and synthetic end-to-end evaluation, with rootful Podman confined to the VM and candidates non-root under `runsc`. The standalone broker holds provider credentials and enforces expiring tokens plus model/call/token/byte budgets. | No mutator or auto-promotion. Docker Desktop stays unchanged and native WSL engines remain rejected on cgroup v1. Real external holdouts, broker-only generation networking and a clean committed baseline remain mandatory. Non-externalizable evaluators still require a remote VM/microVM. |

## How To Promote A Feature

Promote a feature to default only if at least one is true:

- It removes a real, reproduced failure class with deterministic behavior.
- It improves a gated metric (`resolved_rate`, `mean_tool_calls`, or
  `mean_tokens_per_call`) outside the noise band.
- It has zero happy-path model/schema/prompt cost and only adds information on an
  error path.
- It replaces broader prompt instructions with a mechanical check or tool.

For model-facing changes measured with eval-v2, apply the stricter lexicographic
screen: no critical failures; paired 95% lower bound for `stable_pass_k` ≥ −0.05;
no spec regression; and either a deterministic failure-class removal or ≥10% paired
efficiency gain whose interval excludes zero, with no other primary cost regression
over 10%. Workflow changes additionally require the monthly 5–10 task human pilot.

Demote or keep opt-in if:

- The result is neutral but costs prompt/tool-schema/context budget.
- The win is inside the noise band and adds turns/tool calls.
- The feature depends on the model self-correcting from advice rather than a
  deterministic gate.
- The evidence comes from a bench that structurally cannot measure the intended
  value.

## Next Measurements

1. A/B the exact grounding instruction block:

   ```bash
   # control: current default
   # treatment: SIRBONE_NO_GROUNDING=1
   # compare resolved_rate, mean_tool_calls, mean_tokens_per_call, and bad claims
   ```

2. **DONE (2026-07-14).** Built the multi-task same-project bench
   (`bench/specbench/run_session_ab.py` + `tasks/frozen_holdout.jsonl`, design in
   `bench/specbench/session_design.md`) and ran all four session A/Bs (120 agent
   runs, glm-5.2). Historical verdicts in the table above led to ACE **remove**
   (inert — 0 lessons recorded in 18 sessions; removed from core 2026-08-09),
   hygiene **remove** (quality gate fail on its own home
   turf), compaction **change** (mid-task firing destructive, summary loses
   decisions; −63% tokens), historia **change** (mandatory-logging clause costs
   15% tokens with 12 writes / 3 hits). New Rust telemetry backs the mechanism
   claims. The surviving active counters are `compaction_fired` and
   `historia_writes/hits`; removed-feature counters are accepted only while
   reading old reports and are absent from new audits. Active ablation toggles
   include `SIRBONE_NO_COMPACT` / `SIRBONE_NO_HISTORIA`.

3. Follow-ups from #2 (each is change-then-re-measure, not a new bench):

   - Compaction: compact only at turn boundaries; carry decision/constraint lines
     verbatim into the summary; re-run the `long` scenario.
   - HISTORIA: soften the completion-requirement clause to on-demand; re-run `hist`.
   - ACE: **closed** — do not add a nudge or re-run the old arm. The inert feature
     was removed from core; reconsideration requires a new design and new evidence.

4. **DONE (2026-07-09).** A/B'd `quality-code` on ACB-V2 hard (40×3 seeds).
   Verdict: net-neutral / weakly-positive, **kept opt-in, not promoted** — resolved_rate
   0.833 vs 0.808 control (+3/120 pairs), pass@k 0.900 vs 0.850, **pass^k flat 0.725**,
   tokens/call −6%. See the `quality-code` row above. Two findings from the run:
   - **Enable bug:** mounting the skills dir is *not* enough — `scan_skills()` gates the
     model catalog on the project `skills.enabled` allowlist, empty in a fresh container,
     so the skill was dormant (0/1 fired). Fix: also mount an enabling
     `config.json` at the `/work` project slug (`-work`), single-file `:ro` so sirbone can
     still create its sessions dir. `sirbone doctor` in-container then shows `1 enabled`.
   - **Fire rate ~1/5** even once enabled — too rare to move the metric. So the arm
     **force-injects** the body via `SIRBONE_ACB_FORCE_SKILL` (added to
     `run_inference_acb.py`), prepending the frontmatter-stripped rubric to every prompt.
     This measures the rubric ceiling (100% use), not opt-in willingness.

   Original A/B design (superseded by the force-inject arm above; kept for context — the
   mount-only toggle measured non-use):

   ```bash
   cd bench/eval_harness
   # treatment (skill present): mount an isolated dir holding ONLY the skill
   SIRBONE_ACB_SKILLS_ROOT="$PWD/ab_skills" \
     uv run python run_acb.py --ids acb_subset.json --seeds 3 --out eval_qc_on_$(date +%s).json --concurrency 4
   # control (skill absent): leave SIRBONE_ACB_SKILLS_ROOT unset
   uv run python run_acb.py --ids acb_subset.json --seeds 3 --out eval_qc_off_$(date +%s).json --concurrency 4
   # compare resolved_rate (hard fail→pass?), mean_tokens_per_task, mean_tool_calls
   ```

   Quality shows up as `resolved_rate` on **hard** tasks specifically (easy tasks
   saturate). Two live caveats: (a) the skill is progressive-disclosure, so confirm
   the model actually calls `load_skill` on the problem — if it never fires, the A/B
   measures non-use, not the rubric; (b) `SIRBONE_THINKING_BUDGET` is already in
   `FORWARD_ENV`, so the #1 thinking-budget arm needs no wiring — A/B it independently
   by setting that var. Promote to default only on an out-of-band resolved_rate gain
   without a token blow-up.

5. **Prompt ablation — naked baseline, then add back on measured failure.**

   **Stage 1 DONE (2026-08-07).** 54 agent runs, verdict in the table above:
   quality exactly tied (Δ 0.000, CI [0.000, 0.000]), calls +14.1% and churn
   +85.0% against the naked arm, both intervals excluding zero. The blocks are
   kept, and stage 2 is re-aimed from "recover quality" to "attribute the
   conciseness". Two findings from the same run:

   - **The HISTORIA clause is redundant** — 24 writes with it, 24 without.
     Cut it (row above).
   - **`long/t1-parse` at position 1 fails identically on both arms**
     (0/3 requirements, churn 0, 4 calls, ~35s, `compaction_fired=1` at peak
     ~23.1–23.2k against the scenario's artificial 24k window). It hit the
     naked arm in rep1 and the baseline in rep2. **The bench's own control is
     unstable on that task**, which is also *why* the paired delta is exactly
     zero: each arm lost one `long` session. Fix the compaction trigger before
     reading any small effect off this scenario.

   Harness landed 2026-08-02: `SIRBONE_DISABLE=prompt:<block>` drops one named
   system-prompt block, `prompt:*` drops all sirbone-authored blocks (the naked
   arm; identity + platform context + the user's CLAUDE.md survive — `claude_md`
   is exempt from the wildcard and must be named to go). `sirbone doctor` prints
   the assembled prompt size, and `[usage]` carries `system_prompt_tokens`, so an
   arm's prompt weight is separable from the trajectory it caused.

   Why the protocol is inverted relative to every other row in this table: our
   A/Bs add a clause and test it against the current default, which cannot detect
   *accumulated* weight — five clauses each inside the noise band still cost real
   tokens together. Anthropic runs the opposite loop on Claude Code (delete ~80%
   of the prompt on each model generation, add a line back only on a repeated
   observed failure; `CLAUDE_CODE_SIMPLE=1` is their naked arm). The repo already
   has one confirmed instance of the failure mode this catches: the HISTORIA
   clause cost 15.4% of tokens for 12 writes / 3 hits.

   Measured weight on this repo (`sirbone doctor`, full = 19049 chars / ~4762 tok):

   | Block | chars | note |
   |---|---:|---|
   | `claude_md` | 10701 | the user's own file — 56% of the prompt, not our dead weight |
   | `git` | 1655 | **dynamic content inside the cached prefix** (`git status`) |
   | `grounding` | 1324 | already has its own `SIRBONE_NO_GROUNDING`, still un-A/B'd (#1) |
   | `debug_toolkit` | 880 | gated to languages present |
   | `historia` | 770 | the clause measured at −15.4% tokens when removed |
   | `verify` | 583 | overlaps `grounding` in content — merge candidate |
   | `bugfix` / `output_filter` / `minimal` / `ask` / `truthful` / `style` | 433/353/240/221/188/141 | |

   Sirbone-authored total ≈ 6788 chars (~1700 tok, 36% of prompt).

   **The tool schemas are the bigger half.** Measured the same way
   (`sirbone doctor`, 2026-08-02): 17 native tools ≈ **4315 tokens** of
   `{name, description, input_schema}` riding the cached prefix every turn —
   about **2.5× the whole authored prompt**. Ranking, dearest first:

   | tok | tool | | tok | tool |
   |---:|---|---|---:|---|
   | 492 | `bash` | | 232 | `edit` / `read` |
   | 369 | `ask_user` | | 231 | `job_status` |
   | 357 | `todo` | | 203 | `glob` |
   | 332 | `code_map` | | 200 | `note` |
   | 287 | `historia` | | 183 | `write` |
   | 273 | `grep` / `verify` | | 163 | `web_fetch` |
   | 242 | `web_search` | | 126 | `load_skill` |
   | | | | 120 | `undo` |

   This reorders the ablation queue: prompt blocks are the cheap half. It does
   **not** license cutting tools — ablating a core tool measures nothing because
   the agent simply breaks. Two different levers apply to a dear tool:

   - **Trim the schema** (deterministic, no A/B needed when capability is
     untouched). `code_map` was 762 tok on 2026-08-02, the dearest of the 17.
     Its `description()` re-stated in prose what the `Op` doc comments already
     shipped as `$defs` variant descriptions, so schemars sent the same op
     semantics to the model twice per turn. Deduplicated to one place and cut
     the non-actionable implementation notes (.gitignore, mtime cache):
     **762 → 332 tok, −430/turn, −9% of the whole native schema surface**;
     `examples/code_map_bench.rs` byte-identical before/after (n=48). It was
     the outlier, not a systemic pattern: `todo` (357 tok), the next-dearest
     tool with an enum input, carries no such duplication.
   - **A/B the tool itself**, for tools both dear and rarely used
     (`todo` 357, `ask_user` 369, `note` 200, `job_status` 231, `undo` 120 — a
     ~1277-token group). That is a *group* screen, not 17 single A/Bs: at n=9
     paired tasks ×3 reps the CI is wider than any single tool's effect.

   `code_map` itself is KEEP on **capability** (n=48: definer_id 48/48 vs rg
   0/48) and its **agentic value is still unmeasured after two A/Bs**, both of
   which failed for the same reason: no repository big enough to navigate.

   | | 2026-07-11 | 2026-08-02 |
   |---|---|---|
   | bench | ACB-V2 | SpecBench session (long+hist ×3, 54 runs) |
   | why it can't see the lever | single-file algorithm tasks; baseline arm timed out at 900s, empty output | workspaces are 2 files/184 LOC (`long`) and 8 files (`hist`) |
   | `code_map` invocations, baseline arm | — | **0 in 27 runs** |

   Zero invocations is the whole result. The arms differed only by 332 tokens of
   schema the model never read, so nothing measured can be attributed to the
   tool's function:

   - `resolved_rate` identical, 0.852 both arms.
   - `stable_pass` +0.111 CI [0.0, 0.333] *for the ablated arm* — but that is one
     task moving (`long/t5-export` 1/3 → 3/3) against two regressing
     (`t1-parse`, `t2-totals`, 2/3 → 1/3). Three tasks drifting in two directions
     at n=3 each is noise, not a signal, and the CI's lower bound is exactly 0.
   - `calls` −21.9% CI [−36.3, −1.0] is the only interval excluding zero. Do not
     read it as an effect: 332 unread prefix tokens cannot plausibly cut a fifth
     of the API round trips. `tokens` (−15.6%) and `churn` (−13.3%) both cross
     zero.

   **Do not promote this into a removal.** The negative here is about the bench,
   not the tool. What is now solid: on small workspaces `code_map` is pure cost —
   332 tok/turn for zero calls, because `read`+`grep` are strictly cheaper than a
   repo map when the repo fits in two files. The open question is whether that
   inverts on a real codebase, and no existing task set can answer it. The bench
   that could is a dogfood set on sir-bone-rs itself (~26k LOC), which does not
   exist yet. Until then `code_map` stays default-on on capability evidence.

   **Follow-up (same day): the real-session telemetry answers what the bench
   could not.** `sirbone stats` folds every local session into one per-tool
   picture; over 33 sessions on real repositories (293 tool calls, 7 projects):

   | tool | calls | share | sessions | result tokens | ≈ per call |
   |---|---:|---:|---:|---:|---:|
   | `read` | 106 | 36.2% | 30/33 | 129 305 | 1 220 |
   | `bash` | 59 | 20.1% | 16/33 | 13 994 | 237 |
   | `grep` | 42 | 14.3% | 17/33 | 48 737 | 1 160 |
   | **`code_map`** | **15** | **5.1%** | **14/33** | **154 552** | **10 300** |

   Two conclusions, both invisible to the A/B:

   - **The model does reach for `code_map` on real work** (14 of 33 sessions),
     and the rate climbs with context: 5/20 sessions under 10k peak context,
     6/10 at 10–30k, 3/3 at 30–60k. Small n, but the direction is the tool's
     whole premise. The SpecBench null was a bench artifact, now independently
     confirmed.
   - **Its schema was never the problem.** 12 of the 14 calls land within
     rounding of the 16k-token truncation cap (`truncate::DEFAULT_MAX_RESULT_TOKENS`;
     the audit's ÷4 estimate reads it as ~12k). On a ~26k-LOC repo `op=list`
     reliably returns a *truncated* symbol map — maximum cost for an incomplete
     answer. The 2026-08-02 trim saved 430 tokens per **turn**; this is 12k per
     **call**.

   **Fixed the same day — degrade, don't truncate.** `op=list` renders the full
   signature map only while it fits `LIST_BUDGET_CHARS` (half the global result
   cap); past that it returns one line per file (path + symbol count), complete,
   with an explicit pointer to `path="<dir>"` and to the single-symbol ops. A new
   optional `path` scopes the listing back to full signatures for a subtree.
   Measured on this repo (228 files, 2174 symbols):

   | call | before | after |
   |---|---:|---:|
   | `list` (whole repo) | 121 412 chars rendered, **cut to 48 000**, incomplete | **9 423** chars, complete |
   | `list path="src/tools"` | n/a | 10 125 chars, full signatures |
   | `list path="src/agent"` | n/a | 3 376 chars, full signatures |

   Cost of the fix: schema 332 → 407 tok/turn (the `path` field plus one line of
   description) against ~9.6k tokens returned per call. Capability untouched —
   `examples/code_map_bench.rs` byte-identical (n=48), since it exercises
   `structure::` directly and never went through the renderer. Two tests pin the
   new behaviour: an oversized repo must degrade rather than truncate (asserting
   the *last* file alphabetically survives, which is exactly what truncation
   dropped), and a scoped path must come back with signatures.

   Note the ordering lesson: the schema trim was measured first because `doctor`
   already priced schemas, and it found a real 430 tok/turn. The 25× larger cost
   was invisible until `sirbone stats` priced tool *output* — build the
   measurement for the axis you have not measured, not another one for the axis
   you have.

   Also from the same scan: `ask_user`, `todo`, `verify` and `undo` have never
   been called on a real project, and **compaction has fired zero times in 181
   sessions** (highest peak context ever observed: 42.5k).

   Harness bug found by this run: `compare_eval.py` divided by a zero control
   value (`self_tests` was 0 in both arms), crashing after all 54 runs had
   completed and before the comparison was written. Fixed — a zero-vs-zero metric
   is now 0% change, and only a genuinely undefined ratio stays `None`. The saved
   per-arm files let the comparison be recomputed with no extra quota.

   Two findings that fall out of the breakdown, before any A/B is run:

   - `git` is the second-heaviest block we author *and* it is dynamic. The
     prompt-cache rule in the row above ("keep dynamic content out of the cached
     prefix") is violated here: a dirty tree changes `git status`, so the cached
     prefix differs between sessions in the same repo. Verify the real cache-hit
     impact before changing anything — it may be cheap in practice, since the
     prefix is stable *within* a session.
   - `grounding` + `verify` = 1907 chars saying overlapping things. Merging them
     is the "replace broader prompt instructions with one rule" promotion criterion
     already in this file.

   Protocol (run on SpecBench, not ACB — ACB's single-file tasks under-measure
   prompt-driven discipline). Wired and validated 2026-08-02; step-by-step
   runbook in [`bench/specbench/README.md`](../bench/specbench/README.md)
   §Prompt-ablation campaign:

   ```bash
   cargo build --release
   cd bench/specbench
   uv run --no-project python run_session_ab.py prompt_naked \
     --schedule schedules/prompt_naked.json --out-dir runs
   ```

   Stage 1 is the naked arm (`prompt_naked` in `feature_catalog.json`, toggle
   `SIRBONE_DISABLE=prompt:*,skill:*`), 12 sessions / 54 agent runs over `long` +
   `hist` ×3 reps. `ace` is excluded — saturated at 1.0, it can only burn quota.
   Stage 2 adds one block back per campaign, heaviest-first, stopping when the
   gap to baseline closes (3–4 campaigns, not 13). `prompt:*` cannot be
   subtracted from, so an add-back arm enumerates the blocks that stay off.

   Gate — this is a *removal* protocol, so the screen runs in reverse: keep a
   block only if putting it back moves `stable_pass_k` with the paired lower bound
   above zero. A block whose removal is quality-neutral is dead weight and goes,
   even if it "seems obviously useful". Model-specificity is the live caveat:
   Cherny's result is about a frontier model having internalized the patches, and
   glm-5.2 is not that model — expect the naked arm to lose here, and treat the
   output as a *ranked cut list*, not as a candidate default.

6. **Planner/worker model routing** (candidate row above) — spec before code.

   Shape: one call to a separately configured frontier model that reads the
   prompt plus `structure.rs` `prompt_context` and emits N closed sub-tasks (goal,
   files, acceptance check). Each sub-task then runs as an ordinary sirbone turn on
   the cheap executor with **fresh context** — not a resumed session. The planner
   never re-enters; there is no advisor call mid-task (the former `architect`,
   measured net-negative and removed from core on 2026-08-09).

   Why it might behave differently from the three prior negatives: the cost is paid
   **once** and bounded (one planner call), and the executor's context stays small
   because each sub-task carries only its own scope. The swarm result that motivates
   it is a *cost* result at fixed quality, so measure cost first.

   Gate — promote only if all hold:

   - `stable_pass_k` paired lower bound ≥ −0.05 (standard eval-v2 screen), and
   - **$ per stably-resolved task** drops ≥25% with the interval excluding zero, and
   - no increase in empty/no-output runs (the failure mode that sank
     `--thinking-budget`: bloated context → no `solution.<ext>`).

   Kill early if the planner's sub-tasks are just a restatement of the prompt (log
   planner output and diff against the prompt on the first 5 tasks) — that was the
   plan-variant failure, and it is cheap to detect before spending a full A/B.

   Bench: SpecBench multi-task scenarios, not ACB — ACB tasks are single-file and
   too small to have a decomposition worth paying a frontier call for, so ACB would
   measure overhead only. Needs a new `$`-aware aggregate in `compare_eval.py`
   (per-arm price table, since the two arms use different models).

7. **Wide-benchmark proof that the removals and reinvestments raise stable task
   resolution** — the Mission's first objective, and the one claim nothing in
   this file currently supports.

   Every verdict above is measured on workspaces of 2–8 files (SpecBench) or
   single files (ACB-V2). They establish that individual features are neutral or
   positive *there*. They cannot establish the aggregate claim, because the
   removals were justified partly by context economics that only bite on real
   repositories. The `code_map` null is the sharpest evidence of the gap: 27
   baseline runs, zero calls, because no bench workspace is large enough to need
   a repo map.

   **Harness landed 2026-08-10** in `bench/claw/` (adapter, arm freezer, pinned
   installer, paired scorer; protocol in `bench/claw/README.md`). Verified
   offline: static musl arms build and are checked for static linking, the
   scorer reproduces a seeded fixture with a known regression and a known
   invalid repetition. **Not yet run against a provider.**

   Staging, and why it is not "just run Lite ×3":

   - *Phase 0* — 1-task smoke. Validates the adapter. Check `git.patch` carries
     source files only (sirbone's `~/.sirbone` state must stay out of the work
     tree) and that `web_search`/`web_fetch` never fired.
   - *Phase 1* — Lite 80 × 1 rep × 2 arms. Infrastructure and no-regression
     gate, and the measurement of median wall time/tokens that decides whether
     Phase 2 is affordable. **Promotes nothing.**
   - *Phase 2* — Full 350 × 3 reps × 2 arms = 2100 valid attempts. The only
     phase that can support a promotion claim.

   Power, stated before the campaign so the result cannot be reinterpreted
   after it: the paired test is McNemar on discordant tasks. At n=80 with a
   ~20% discordant rate, a true +10pp gain yields about 12 gains vs 4
   regressions — exact p ≈ 0.077, not significant. Lite can only detect deltas
   above roughly 15pp; n=350 bottoms out near 5pp. Reading a Lite delta as a
   result is exactly how `localize_prepass` produced CI [−0.15, +0.15] — a null
   dressed as a tie. `score.py` warns below 5 discordant pairs.

   Cost gate before Phase 2: 2100 attempts at `--workers 1` and a 3600 s cap is
   up to 2100 wall-clock hours, ~420 at a 12-minute median. It needs metered API
   capacity (the z.ai 5-hour window forces ≤25-task chunks) and enough
   `--workers` to fit a schedule. Decide this from Phase 1's measured median, not
   from an estimate.

   Confound to record, not to discover afterwards: **compaction is default-on
   and documented as harmful on long sessions**, and a 3600 s budget on a real
   repository is the first setting where it can actually fire. Check
   `peak_context` per arm before attributing any delta to the change under test.

## Deferred (memory / retrieval / packaging)

Ordered by ROI; each is a *do-when-triggered*, not a now-task. Do not build ahead
of the trigger — the current default (distillate + read-free `historia` write +
opt-in BM25) already covers the common case.

1. **Large `HISTORIA.md` retrieval**. Trigger: measured context waste in a real project.
   Use the external `rag-bone` skill rather than restoring deleted native indexing.
2. **Persistent rag-bone process/MCP**. Trigger: model cold-start is at least 20% of
   multi-query session latency. Keep one-shot CLI otherwise.
3. **Publish `search2md` standalone** (separate repo `/home/dio/search2md`) and keep
   the binary name. Sir Bone now consumes its stable JSON CLI contract directly.
