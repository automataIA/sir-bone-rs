"""Paired stable-resolution scoring for a Claw-SWE-Bench control/candidate campaign.

Answers the Mission question — *do the removals and reinvestments raise stable
task resolution?* — instead of reporting a single Pass@1 number.

  uv run python bench/claw/score.py \
      control:control-lite-r1:reports/control-r1.json \
      control:control-lite-r2:reports/control-r2.json \
      control:control-lite-r3:reports/control-r3.json \
      candidate:candidate-lite-r1:reports/candidate-r1.json \
      candidate:candidate-lite-r2:reports/candidate-r2.json \
      candidate:candidate-lite-r3:reports/candidate-r3.json \
      --artifacts "$CLAW_DIR/artifacts" [--json out.json]

Each argument is ``arm:run_id:swebench_report.json`` (the report ``run_eval.py``
writes, holding ``resolved_ids``).

Two rules, both deliberate:
  * a repetition where the agent never reached the model (0 calls, quota/429,
    adapter or container failure) is **INVALID**, not a failure — it is listed
    for rerun and kept out of every denominator;
  * a task is only scored when *both* arms have all repetitions valid, so the
    paired comparison stays on identical ground.
"""

import argparse
import json
import math
import sys
from collections import defaultdict
from pathlib import Path

# Substrings in stderr that mean "the infrastructure failed", not "the agent failed".
INVALID_STDERR = ("429", "quota", "rate limit", "overloaded", "503", "502")


def parse_args():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("runs", nargs="+", metavar="ARM:RUN_ID:REPORT")
    ap.add_argument("--artifacts", required=True, type=Path)
    ap.add_argument("--json", type=Path, help="also write the full result object")
    return ap.parse_args()


def load_resolved(report: Path) -> set[str]:
    data = json.loads(report.read_text())
    ids = data.get("resolved_ids")
    if ids is None:
        raise SystemExit(f"{report}: no 'resolved_ids' key — is this a SWE-bench report?")
    return set(ids)


def dig_usage(obj) -> dict:
    """metadata.json layout varies by upstream version; find the usage dict."""
    if isinstance(obj, dict):
        if "usage" in obj and isinstance(obj["usage"], dict):
            return obj["usage"]
        for value in obj.values():
            found = dig_usage(value)
            if found:
                return found
    return {}


def load_attempt(run_dir: Path, instance_id: str) -> dict:
    """One (task, repetition) observation: validity + measured cost."""
    inst_dir = run_dir / instance_id
    meta_file = inst_dir / "metadata.json"
    if not meta_file.exists():
        return {"valid": False, "why": "INVALID_ADAPTER (no metadata.json)"}
    meta = json.loads(meta_file.read_text())
    usage = dig_usage(meta)
    calls = int(usage.get("calls") or 0)

    stderr_file = inst_dir / "agent_stderr.log"
    stderr = stderr_file.read_text(errors="replace").lower() if stderr_file.exists() else ""
    provider_error = any(sig in stderr for sig in INVALID_STDERR)

    if calls == 0:
        why = "INVALID_PROVIDER_QUOTA" if provider_error else "INVALID_ADAPTER (0 model calls)"
        return {"valid": False, "why": why}
    return {
        "valid": True,
        "input_tokens": int(usage.get("input_tokens") or 0),
        "output_tokens": int(usage.get("output_tokens") or 0),
        "cached_tokens": int(usage.get("cached_tokens") or 0),
        "calls": calls,
        "tool_calls": int(usage.get("tool_calls") or 0),
    }


def mcnemar_exact(b: int, c: int) -> float:
    """Two-sided exact McNemar p-value on the discordant pairs (b vs c)."""
    n = b + c
    if n == 0:
        return 1.0
    k = min(b, c)
    tail = sum(math.comb(n, i) for i in range(k + 1)) / 2**n
    return min(1.0, 2 * tail)


def main() -> int:
    args = parse_args()

    reps = defaultdict(list)  # arm -> [(run_id, resolved_ids)]
    for spec in args.runs:
        try:
            arm, run_id, report = spec.split(":", 2)
        except ValueError:
            raise SystemExit(f"bad run spec {spec!r} — expected ARM:RUN_ID:REPORT")
        reps[arm].append((run_id, load_resolved(Path(report))))

    if set(reps) != {"control", "candidate"}:
        raise SystemExit(f"expected arms control+candidate, got {sorted(reps)}")
    k = len(reps["control"])
    if k != len(reps["candidate"]):
        raise SystemExit("both arms need the same number of repetitions")

    # Ground truth for "which tasks were attempted" is the artifact tree; a task
    # missing there in one run is exactly the INVALID case load_attempt reports.
    tasks = set()
    for arm_runs in reps.values():
        for run_id, resolved in arm_runs:
            run_dir = args.artifacts / run_id
            if not run_dir.is_dir():
                raise SystemExit(f"missing artifacts dir: {run_dir}")
            tasks |= {d.name for d in run_dir.iterdir() if d.is_dir()} | resolved
    tasks = sorted(tasks)

    stable, invalid, cost = {}, defaultdict(list), defaultdict(lambda: defaultdict(int))
    for arm, runs in reps.items():
        for task in tasks:
            passes = []
            for run_id, resolved in runs:
                att = load_attempt(args.artifacts / run_id, task)
                if not att["valid"]:
                    invalid[arm].append((task, run_id, att["why"]))
                    passes.append(None)
                    continue
                for key in ("input_tokens", "output_tokens", "calls", "tool_calls"):
                    cost[arm][key] += att[key]
                passes.append(task in resolved)
            stable[(arm, task)] = None if None in passes else all(passes)

    scored = [t for t in tasks if stable[("control", t)] is not None and stable[("candidate", t)] is not None]
    dropped = [t for t in tasks if t not in scored]

    both = [t for t in scored if stable[("control", t)] and stable[("candidate", t)]]
    cand_only = [t for t in scored if stable[("candidate", t)] and not stable[("control", t)]]
    ctrl_only = [t for t in scored if stable[("control", t)] and not stable[("candidate", t)]]
    neither = [t for t in scored if not stable[("control", t)] and not stable[("candidate", t)]]

    n = len(scored)

    def rate(arm: str) -> float:
        return (sum(1 for t in scored if stable[(arm, t)]) / n) if n else 0.0

    p = mcnemar_exact(len(cand_only), len(ctrl_only))

    out = {
        "repetitions": k,
        "tasks_scored": n,
        "tasks_dropped_invalid": dropped,
        "stable_resolution": {"control": rate("control"), "candidate": rate("candidate")},
        "paired": {
            "both_stable": len(both),
            "candidate_only": len(cand_only),
            "control_only": len(ctrl_only),
            "neither": len(neither),
            "mcnemar_p": p,
        },
        "regressions": ctrl_only,
        "gains": cand_only,
        "cost": {arm: dict(vals) for arm, vals in cost.items()},
        "invalid": {arm: [list(x) for x in rows] for arm, rows in invalid.items()},
    }

    per_stable = {}
    for arm in ("control", "candidate"):
        stable_n = sum(1 for t in scored if stable[(arm, t)])
        total_tok = cost[arm]["input_tokens"] + cost[arm]["output_tokens"]
        # Charged across *all* valid attempts, so a cheap-but-flaky arm cannot
        # look efficient by failing early.
        per_stable[arm] = {
            "tokens_per_stable_task": round(total_tok / stable_n) if stable_n else None,
            "calls_per_stable_task": round(cost[arm]["calls"] / stable_n, 1) if stable_n else None,
        }
    out["per_stably_resolved"] = per_stable

    print(f"\nClaw-SWE-Bench paired campaign — pass^{k}, {n} scored tasks\n")
    print(f"{'':<28}{'CONTROL':>12}{'CANDIDATE':>12}")
    print(f"{'stable resolution':<28}{rate('control'):>11.1%}{rate('candidate'):>12.1%}")
    for label, key in (("tokens/stable task", "tokens_per_stable_task"), ("calls/stable task", "calls_per_stable_task")):
        c, d = per_stable["control"][key], per_stable["candidate"][key]
        print(f"{label:<28}{str(c):>12}{str(d):>12}")
    print(
        f"\npaired: both {len(both)} | candidate-only {len(cand_only)} | "
        f"control-only {len(ctrl_only)} | neither {len(neither)}"
    )
    print(f"McNemar exact p = {p:.4f} on {len(cand_only) + len(ctrl_only)} discordant pairs")
    if ctrl_only:
        print(f"\nREGRESSIONS (control stable, candidate not):\n  " + "\n  ".join(ctrl_only))
    if dropped:
        print(f"\n{len(dropped)} task(s) dropped for invalid repetitions — rerun before concluding:")
        for arm, rows in invalid.items():
            for task, run_id, why in rows:
                print(f"  {arm:<10} {run_id:<28} {task:<32} {why}")
    if n and len(cand_only) + len(ctrl_only) < 5:
        print("\nWARNING: too few discordant pairs to conclude anything — this is a null, not a tie.")

    if args.json:
        args.json.write_text(json.dumps(out, indent=2))
        print(f"\nwrote {args.json}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
