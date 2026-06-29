#!/usr/bin/env bash
set -euo pipefail

PLAYGROUND_DIR="$(cd "$(dirname "$0")" && pwd)"
RUST_DIR="$(cd "$PLAYGROUND_DIR/.." && pwd)"
PI="cargo run --manifest-path $RUST_DIR/Cargo.toml --bin sirbone --"

# Load .env if present (ignores comments and blank lines)
if [[ -f "$PLAYGROUND_DIR/.env" ]]; then
    set -a
    # shellcheck disable=SC1090
    source <(grep -v '^\s*#' "$PLAYGROUND_DIR/.env" | grep -v '^\s*$')
    set +a
fi

# ── helpers ──────────────────────────────────────────────────────────────────

reset_fixtures() {
    cp "$PLAYGROUND_DIR/fixtures/calculator.rs" "$PLAYGROUND_DIR/src/calculator.rs"
    cp "$PLAYGROUND_DIR/fixtures/utils.rs"      "$PLAYGROUND_DIR/src/utils.rs"
    cp "$PLAYGROUND_DIR/fixtures/lib.rs"        "$PLAYGROUND_DIR/src/lib.rs"
    rm -f "$PLAYGROUND_DIR/src/formatter.rs"
    rm -f "$PLAYGROUND_DIR/TASKS.md"
}

run_task() {
    local n="$1" prompt="$2"
    echo ""
    echo "━━━ Task $n ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
    echo "$prompt"
    echo "────────────────────────────────────────────────────────────"
    (cd "$PLAYGROUND_DIR" && $PI "$prompt")
}

check() {
    local desc="$1" result="$2"
    if [[ "$result" == "ok" ]]; then
        echo "  ✓ $desc"
    else
        echo "  ✗ $desc"
        FAILURES=$((FAILURES + 1))
    fi
}

# ── check env ────────────────────────────────────────────────────────────────

if [[ -z "${ANTHROPIC_AUTH_TOKEN:-}" && -z "${ANTHROPIC_API_KEY:-}" && -z "${OPENAI_API_KEY:-}" ]]; then
    echo "ERROR: set ANTHROPIC_AUTH_TOKEN (or OPENAI_API_KEY) before running"
    exit 1
fi

# ── reset ────────────────────────────────────────────────────────────────────

echo "Resetting playground to broken state..."
reset_fixtures

echo "Confirming tests are broken before agent runs:"
if cargo test --manifest-path "$PLAYGROUND_DIR/Cargo.toml" 2>/dev/null; then
    echo "WARNING: tests already pass — fixtures may not be broken"
else
    echo "Good — tests fail as expected"
fi

FAILURES=0

# ── tasks ────────────────────────────────────────────────────────────────────

run_task 1 "The add() function in src/calculator.rs returns the wrong result. Find the bug and fix it with a minimal change."

run_task 2 "The is_even() function in src/utils.rs is not implemented (it panics). Implement it."

run_task 3 "Create the file src/formatter.rs with a public function format_number(n: i32) -> String that returns the number with an explicit sign: \"+5\" for positive, \"-3\" for negative, \"0\" for zero. Add pub mod formatter; to src/lib.rs. Follow the project rules in AGENTS.md."

run_task 4 "List all .rs source files in src/. Grep each one for lines containing 'pub fn'. Write a TASKS.md in the project root listing all public functions found, grouped by file in markdown."

# ── verify ───────────────────────────────────────────────────────────────────

echo ""
echo "━━━ Verifying ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"

# cargo test (tasks 1 + 2)
if cargo test --manifest-path "$PLAYGROUND_DIR/Cargo.toml" 2>/dev/null; then
    check "cargo test (7 tests)" "ok"
else
    check "cargo test (7 tests)" "fail"
fi

# task 3 checks
[[ -f "$PLAYGROUND_DIR/src/formatter.rs" ]] \
    && check "formatter.rs created" "ok" || check "formatter.rs created" "fail"

grep -q "pub fn format_number" "$PLAYGROUND_DIR/src/formatter.rs" 2>/dev/null \
    && check "format_number function exists" "ok" || check "format_number function exists" "fail"

grep -q "pub mod formatter" "$PLAYGROUND_DIR/src/lib.rs" 2>/dev/null \
    && check "pub mod formatter in lib.rs" "ok" || check "pub mod formatter in lib.rs" "fail"

grep -q "///" "$PLAYGROUND_DIR/src/formatter.rs" 2>/dev/null \
    && check "doc comment present (AGENTS.md rule)" "ok" || check "doc comment present (AGENTS.md rule)" "fail"

cargo build --manifest-path "$PLAYGROUND_DIR/Cargo.toml" 2>/dev/null \
    && check "project compiles with formatter" "ok" || check "project compiles with formatter" "fail"

# task 4 checks
[[ -f "$PLAYGROUND_DIR/TASKS.md" ]] \
    && check "TASKS.md created" "ok" || check "TASKS.md created" "fail"

grep -qE "add|multiply|is_even|format_number|max|diff" "$PLAYGROUND_DIR/TASKS.md" 2>/dev/null \
    && check "TASKS.md lists function names" "ok" || check "TASKS.md lists function names" "fail"

# ── result ───────────────────────────────────────────────────────────────────

echo ""
if [[ $FAILURES -eq 0 ]]; then
    echo "✓ All checks passed"
else
    echo "✗ $FAILURES check(s) failed"
    exit 1
fi
