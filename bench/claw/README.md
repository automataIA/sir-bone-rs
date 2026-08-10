# Claw-SWE-Bench arm (`bench/claw/`)

Wires sirbone into [Claw-SWE-Bench](https://github.com/opensquilla/claw-swe-bench)
(350 GitHub issue-resolution tasks, 8 languages, 43 repos; 80-task Lite subset)
so the Mission's first objective can finally be tested on a wide benchmark:

> do the removals and reinvestments actually raise **stable task resolution**,
> without costing safety, reproducibility or token efficiency?

Every other bench we run answers a narrower question. SpecBench workspaces are
2–8 files, ACB-V2 tasks are single-file — neither can show whether a whole-agent
change helps on real repositories. This one can, at a real cost in time and
provider quota, which is why the protocol below is deliberately staged.

## What is here

| File | Role |
|---|---|
| `sirbone.py` | The adapter. Copied into the upstream checkout by `install.sh`. |
| `install.sh` | Clones + pins Claw-SWE-Bench and SWE-bench, installs and registers the adapter. |
| `build_arms.sh` | Freezes `control` (a committed SHA) and `candidate` (the working tree) as static musl binaries + `manifest.json` with SHA256s. |
| `score.py` | Paired stable-resolution scoring: pass^k, McNemar, cost per stably resolved task, `INVALID_*` excluded from the denominator. |

Nothing here vendors the upstream benchmark; `install.sh` fetches it into
`/tmp` (or `$1`) so the pinned SHAs, not a copy, are the record.

## Adapter contract

The benchmark core owns the container, the prompt, the patch (`git diff` from
the runner, never the agent) and the evaluator. The adapter only has to put
sirbone in front of the task honestly. Four things it gets right that a naive
port does not:

1. **Provider env is forwarded** into `docker exec` (`-e ANTHROPIC_AUTH_TOKEN`,
   `SIRBONE_MODEL`, …). Without it every task fails with zero model calls and
   the campaign silently measures nothing. A missing credential raises here.
2. **`HOME=/opt/sirbone-home`.** sirbone writes sessions, snapshots and the
   project store under `~/.sirbone`; pinning `HOME` outside `/testbed`
   guarantees none of it can reach `git.patch`.
3. **`web_search` and `web_fetch` are ablated off** (`SIRBONE_DISABLE`). The
   benchmark forbids network answers, and these tools can fetch the upstream
   issue and its fix. This is a sirbone-specific contamination path that the
   harness cannot see.
4. **`-w /testbed`, no shell.** The prompt travels as one argv element, so an
   issue body cannot be interpreted by `bash -c`.

`max_turns` maps to `SIRBONE_MAX_STEPS` (sirbone has no such CLI flag). Usage
comes from sirbone's own headless JSON — measured tokens, no static pricing
table; upstream's ZeroClaw table has no GLM-5.2 row and would be wrong by
construction.

## Run it

```bash
# 1. freeze both arms (docker rust:1-alpine, no host musl toolchain needed)
bash bench/claw/build_arms.sh              # control = HEAD; pass a SHA to override

# 2. install the harnesses + adapter
bash bench/claw/install.sh
export CLAW_DIR=/tmp/claw-swe-bench
export SWEBENCH_VENV=/tmp/claw-swe-bench-swebench/.venv

# 3. smoke: one task, candidate arm
export SIRBONE_BIN="$PWD/bench/claw/arms/sirbone-candidate"
cd "$CLAW_DIR"
echo "<one instance_id>" > config/smoke.txt
uv run python run_infer.py --claw sirbone --dataset multilingual \
    --run_id sirbone-smoke-001 --instance_file config/smoke.txt \
    --timeout 3600 --workers 1
```

Before trusting the smoke, check **all four**:

```bash
cat artifacts/sirbone-smoke-001/*/agent_stderr.log     # no auth/quota errors
cat artifacts/sirbone-smoke-001/*/git.patch            # source files ONLY
jq .usage artifacts/sirbone-smoke-001/*/metadata.json  # calls > 0, tokens > 0
grep -c web_search artifacts/sirbone-smoke-001/*/session.jsonl   # must be 0
```

A `git.patch` containing `.sirbone-symlink`, `HISTORIA.md` or session files
means the `HOME` isolation broke — stop, do not run a campaign on it.

Then evaluate (separate venv, separate process — the agent must never see the
evaluator or the reference patch):

```bash
uv run python run_eval.py \
    --predictions artifacts/sirbone-smoke-001/predictions.jsonl \
    --dataset_name SWE-bench/SWE-bench_Multilingual \
    --run_id sirbone-smoke-001
```

## Campaign protocol

Fixed across arms, always: model, task set, prompt (`prompts/default.txt`),
timeout, max turns, worker count, benchmark and evaluator SHAs. The only
variable is the sirbone binary.

**Phase 0 — smoke (1 task).** As above. Validates the adapter, not the agent.

**Phase 1 — Lite gate (80 tasks × 1 rep × 2 arms = 160 runs).** Purpose:
infrastructure and no-regression sanity, *plus* the measurement that decides
whether Phase 2 is affordable — record median wall time and tokens per task
from `metadata.json`. This phase does **not** promote anything.

**Phase 2 — decision campaign (350 × 3 × 2 = 2100 valid attempts).** Only this
phase can support a promotion claim. See the power note before committing.

### Why Lite cannot be the decision phase

The paired test is McNemar on discordant tasks. At n=80 with a realistic
discordant rate (~20% of tasks), a true +10pp improvement produces roughly 12
gains vs 4 regressions — exact p ≈ 0.077, not significant. Detecting anything
at n=80 needs a delta above ~15pp, which is not the kind of change these
removals produce. At n=350 the same reasoning bottoms out near ~5pp.

So: **Lite answers "did anything break", Full answers "did anything improve".**
Running Lite ×3 and reading the delta as a result is how `localize_prepass`
ended up with CI [-0.15, 0.15] — a null dressed as a tie. `score.py` prints an
explicit warning below 5 discordant pairs for this reason.

### Budget, honestly

2100 attempts at `--workers 1` with a 3600 s cap is up to 2100 hours of wall
clock; even at a 12-minute median it is ~420 serial hours. Phase 2 is therefore
gated on two things: the measured median from Phase 1, and enough parallelism
(`--workers N`, one container each) to fit the campaign into a schedule. On a
quota-limited plan it does not fit at all — the z.ai 5-hour window forces
≤25-task chunks. Budget metered API capacity before freezing the arms, and
price it from the versioned pricing constant recorded with the campaign.

### Validity rules

An attempt where the agent never reached the model is **not** an agent failure.
`score.py` classifies and excludes:

| Class | Meaning |
|---|---|
| `INVALID_PROVIDER_QUOTA` | 0 model calls + 429/quota/overloaded in stderr |
| `INVALID_ADAPTER` | 0 model calls, or no `metadata.json` |

Excluded attempts are listed for rerun and kept out of every denominator. A
task is scored only when **both** arms have all repetitions valid, so the
paired comparison stays on identical ground. Publish the invalid counts next to
the result; never fold them into `FAIL`.

### Scoring

```bash
uv run python bench/claw/score.py \
    control:control-lite-r1:reports/control-r1.json \
    control:control-lite-r2:reports/control-r2.json \
    control:control-lite-r3:reports/control-r3.json \
    candidate:candidate-lite-r1:reports/candidate-r1.json \
    candidate:candidate-lite-r2:reports/candidate-r2.json \
    candidate:candidate-lite-r3:reports/candidate-r3.json \
    --artifacts "$CLAW_DIR/artifacts" --json campaign.json
```

Primary metric: `stable_resolution` = pass^k (all repetitions resolved).
Secondary: tokens and calls **per stably resolved task, charged across all
valid attempts including failures** — so a cheap-but-flaky arm cannot look
efficient by giving up early. Per-task regressions are printed by name.

### Preregister before starting

Fill this in `docs/BENCH_DECISIONS.md` *before* the first Phase 2 run:

```
control SHA / sha256        candidate SHA / sha256
claw-swe-bench SHA          SWE-bench SHA
model + provider endpoint   prompt file SHA256
timeout / max_turns / workers
non-inferiority threshold   repetitions (k)
invalid-retry policy
```

## Known confounds

- **Context compaction.** Default-on and documented as harmful on long sessions
  (`docs/STATUS.md`). A 3600 s budget on a real repository is the first setting
  where it can actually fire, so it may differ between arms for reasons
  unrelated to the change under test. `metadata.json` carries `peak_context` —
  check it before attributing a delta.
- **Localize pre-pass.** The Harbor adapter disables it (`SIRBONE_NO_LOCALIZE=1`)
  because those tasks start from a non-code cwd. Here the cwd *is* a repository,
  so it stays on — deliberately different from Harbor, and the two benches are
  therefore not comparable to each other.
- **Dirty candidate.** `manifest.json` records `diff_sha256` when the working
  tree is uncommitted. That is enough to detect drift, not to reproduce the
  build. Commit the candidate before any publishable campaign.
