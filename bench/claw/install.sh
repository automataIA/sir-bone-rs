#!/usr/bin/env bash
# Clone Claw-SWE-Bench + the SWE-bench evaluator at pinned commits and register
# the sirbone adapter.
#
#   bash bench/claw/install.sh [DEST]        # default DEST: /tmp/claw-swe-bench
#
# Idempotent: re-running refreshes the adapter from this repo, so edits to
# bench/claw/sirbone.py land with one command.
set -euo pipefail

REPO="$(git rev-parse --show-toplevel)"
DEST="${1:-/tmp/claw-swe-bench}"
SWEBENCH_DIR="$DEST-swebench"

# Pin both harnesses: an unpinned benchmark is not a benchmark. Set the SHAs
# once from the first clone, then never move them inside a campaign.
CLAW_REF="${CLAW_REF:-main}"
SWEBENCH_REF="${SWEBENCH_REF:-main}"

clone() {  # clone <url> <dir> <ref>
    if [ ! -d "$3/.git" ]; then
        git clone "$1" "$3"
    fi
    git -C "$3" fetch --quiet origin
    git -C "$3" checkout --quiet "$2"
    echo "$3 @ $(git -C "$3" rev-parse HEAD)"
}

clone https://github.com/opensquilla/claw-swe-bench.git "$CLAW_REF" "$DEST"
clone https://github.com/SWE-bench/SWE-bench.git "$SWEBENCH_REF" "$SWEBENCH_DIR"

# Adapter + registration.
cp "$REPO/bench/claw/sirbone.py" "$DEST/claw_swebench/claws/sirbone.py"

python3 - "$DEST" <<'PY'
import re, sys
from pathlib import Path

root = Path(sys.argv[1])

init = root / "claw_swebench" / "claws" / "__init__.py"
src = init.read_text()
if "SirBoneAdapter" not in src:
    src = src.replace(
        "CLAWS = {",
        "from claw_swebench.claws.sirbone import SirBoneAdapter\n\nCLAWS = {",
        1,
    )
    src = re.sub(r"(CLAWS = \{)", r'\1\n    "sirbone": SirBoneAdapter,', src, count=1)
    init.write_text(src)
    print(f"registered SirBoneAdapter in {init}")

cfg = root / "claw_swebench" / "config.py"
src = cfg.read_text()
if '"sirbone"' not in src:
    src = re.sub(
        r"(CLAW_DEFAULTS[^=]*= \{)",
        r'\1\n    "sirbone": {"model": "glm-5.2", "timeout": 3600, "max_turns": 300},',
        src,
        count=1,
    )
    cfg.write_text(src)
    print(f"added sirbone defaults in {cfg}")
PY

# Evaluator venv, separate from the runner's (upstream requirement).
if [ ! -d "$SWEBENCH_DIR/.venv" ]; then
    (cd "$SWEBENCH_DIR" && uv venv && uv pip install -e .)
fi

cat <<EOF

installed. Export before running:

  export CLAW_DIR="$DEST"
  export SWEBENCH_VENV="$SWEBENCH_DIR/.venv"
  export SIRBONE_BIN="$REPO/bench/claw/arms/sirbone-candidate"

Pin the SHAs above into docs/BENCH_DECISIONS.md before the campaign starts.
EOF
