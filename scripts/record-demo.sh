#!/usr/bin/env bash
# Regenerate assets/demo-session.jsonl — the recording `sirbone demo` replays.
#
# The asset is generated, never hand-edited: it is a real run of this binary
# against playground/, whose starting state is fixed (playground/fixtures).
# Anyone with a provider key can reproduce it, and a doctored transcript would
# be a claim about the agent that nothing backs.
#
#   scripts/record-demo.sh            # record, scrub, overwrite the asset
#   scripts/record-demo.sh --keep     # also leave playground/ as the agent left it
#
# Costs one real API call. Uses whatever provider the environment selects
# (ANTHROPIC_AUTH_TOKEN, else OPENAI_API_KEY); .env at the repo root is loaded
# if present.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PLAY="$ROOT/playground"
PROMPT_FILE="$ROOT/assets/demo-prompt.txt"
OUT="$ROOT/assets/demo-session.jsonl"
KEEP=0
[[ "${1:-}" == "--keep" ]] && KEEP=1

if [[ -f "$ROOT/.env" ]]; then
    set -a
    # shellcheck disable=SC1090
    source <(grep -v '^\s*#' "$ROOT/.env" | grep -v '^\s*$')
    set +a
fi
if [[ -z "${ANTHROPIC_AUTH_TOKEN:-}${ANTHROPIC_API_KEY:-}${OPENAI_API_KEY:-}" ]]; then
    echo "ERROR: no provider key in the environment (ANTHROPIC_AUTH_TOKEN or OPENAI_API_KEY)" >&2
    exit 1
fi

reset_playground() {
    cp "$PLAY/fixtures/calculator.rs" "$PLAY/src/calculator.rs"
    cp "$PLAY/fixtures/utils.rs" "$PLAY/src/utils.rs"
    cp "$PLAY/fixtures/lib.rs" "$PLAY/src/lib.rs"
    rm -f "$PLAY/src/formatter.rs" "$PLAY/TASKS.md"
}

echo "Building sirbone…"
cargo build --manifest-path "$ROOT/Cargo.toml" --bin sirbone --quiet

echo "Resetting playground to its broken fixture state…"
reset_playground

# The new session is whichever session file the run touches, so mark the clock
# first and take the newest match afterwards.
marker="$(mktemp)"
trap 'rm -f "$marker"' EXIT

prompt="$(cat "$PROMPT_FILE")"
echo "Recording: $prompt"
(cd "$PLAY" && "$ROOT/target/debug/sirbone" -p "$prompt")

session="$(find "$HOME/.sirbone" -name '*.jsonl' -newer "$marker" -print0 2>/dev/null \
    | xargs -0 ls -t 2>/dev/null | head -1)"
[[ -n "$session" ]] || { echo "ERROR: the run wrote no session file" >&2; exit 1; }
echo "Recorded $(wc -l < "$session") entries in $session"

# Scrub the recording machine out of the asset: it ships in the crates.io
# tarball, and `sirbone demo` shows every path on screen.
sed -e "s#${PLAY}#/home/you/playground#g" -e "s#${HOME}#/home/you#g" "$session" > "$OUT"
if grep -q "$HOME" "$OUT"; then
    echo "ERROR: $OUT still contains $HOME" >&2
    exit 1
fi

[[ "$KEEP" -eq 1 ]] || reset_playground

echo "Wrote $OUT ($(wc -l < "$OUT") entries)."
echo "Now: cargo test --bins bundled_recording && cargo run -- demo"
