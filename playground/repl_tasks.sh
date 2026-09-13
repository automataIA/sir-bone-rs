#!/usr/bin/env bash
set -euo pipefail

# Small regression harness for CLI/REPL paths that unit tests do not exercise
# well. Provider-backed tmux cases are opt-in so this script is safe to run as a
# local smoke without spending tokens.

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${SIRBONE_BIN:-$ROOT/target/debug/sirbone}"

if [[ ! -x "$BIN" ]]; then
  cargo build --manifest-path "$ROOT/Cargo.toml"
fi

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

pass() {
  echo "PASS: $*"
}

tmpdir="$(mktemp -d)"
cleanup() {
  rm -rf "$tmpdir"
}
trap cleanup EXIT

# 1. `--help` must not leak env values rendered by clap.
help_out="$tmpdir/help.txt"
OPENAI_API_KEY="sk-this-value-must-not-appear" "$BIN" --help >"$help_out" 2>&1 || true
if grep -q "sk-this-value-must-not-appear" "$help_out"; then
  fail "--help leaked OPENAI_API_KEY value"
fi
pass "help hides provider env values"

# 2. `doctor` must remain offline and runnable as a readiness smoke.
"$BIN" doctor >/dev/null
pass "doctor runs offline"

# Provider-backed / PTY tests are intentionally opt-in. They can spend tokens and
# require tmux plus a configured provider.
if [[ "${SIRBONE_REPL_E2E:-0}" != "1" ]]; then
  echo "SKIP: tmux REPL e2e (set SIRBONE_REPL_E2E=1 to run provider-backed cases)"
  exit 0
fi

command -v tmux >/dev/null || fail "tmux not found"

session="sirbone-repl-$RANDOM"
tmux new-session -d -s "$session" -c "$ROOT" "$BIN --repl"
sleep 1
tmux send-keys -t "$session" "/quit" Enter

for _ in {1..20}; do
  if ! tmux has-session -t "$session" 2>/dev/null; then
    pass "/quit exits a live REPL session"
    exit 0
  fi
  sleep 0.25
done

tmux capture-pane -t "$session" -p >"$tmpdir/repl-pane.txt" || true
tmux kill-session -t "$session" 2>/dev/null || true
cat "$tmpdir/repl-pane.txt" >&2
fail "/quit did not exit the REPL session"
