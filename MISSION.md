## Mission

Support spec-driven development through LLM-assisted coding under strict developer supervision.

Goal: produce less but fully functional code, reduce repeated iterations, minimize token usage, and maintain high code quality by combining rigorous human oversight with maximum token efficiency.

## Principle

No authority to the model.

Not over facts: context is extracted deterministically from the repo rather than retrieved from an
index, claims are checked algorithmically against filesystem and symbols, and outcomes are judged by
compiler, tests and linter — never by the model itself. The operating rule is the
**generator–verifier asymmetry**: where output cannot be constrained, it is verified.

Not over actions: every shell, file and MCP call passes a permission gate, and snapshots make runs
reversible.

The model keeps the one irreducible competence — proposing.

## Metrics

Each goal above is a computable quantity, not an aspiration. What is measured, and by what:

| Goal | Metric | Computed by |
|---|---|---|
| Fully functional code | **stable resolution** = `pass^k`: a task counts only if resolved in *every* repetition | `bench/claw/score.py` |
| — same, comparatively | paired control/candidate contingency + **exact McNemar** p-value | `bench/claw/score.py` |
| Fewer repeated iterations | **calls / stably resolved task**, plus raw `calls` and `tool_calls` | `score.py`, from per-run artifacts |
| Minimal token usage | **weighted cost / stably resolved task**, priced with `bench/eval_harness/rates.json`; raw `input + output` and `cached_tokens` kept as diagnostics | `score.py`; per-run `[usage]` line from `src/telemetry.rs` |
| High code quality (repo) | `cargo test`, `cargo clippy -- -D warnings`, mutation gate on changed lines | CI |
| Claim honesty (per run) | deterministic grounding check on paths, symbols and counts | `sirbone ground` |

Four rules in the scorer, all deliberate:

* a repetition where the agent never reached the model (0 calls, quota/429, adapter or container
  failure) is **INVALID**, not a failure — it is listed for rerun and kept out of every denominator;
* a task is scored only when *both* arms have all repetitions valid, so the paired comparison stays
  on identical ground;
* cost is money, not token count. With ~97% of input served from cache, summing `input + output`
  rates a change that lengthens a cached prefix the same as one that adds output tokens, which are
  ~17x more expensive — so the raw sums stay in the report as diagnostics and the decision is taken
  on the weighted figure;
* the report says what it can resolve **before** it says what it found: MDE at this N, the N*
  required for the target difference declared ahead of the campaign, and the ratio N/N*. Below 1,
  a null result reads "not measured", never "no effect".

Single-run `Pass@1` is not the target number. Stability across repetitions is.

## Method

Every additional cognitive layer goes through an A/B ablation, and most did not survive. Minimality
here is an experimental result, not a design preference. Per-feature verdicts:
`docs/BENCH_DECISIONS.md`. Current surface: `docs/STATUS.md`.

> The model proposes, the toolchain disposes.
