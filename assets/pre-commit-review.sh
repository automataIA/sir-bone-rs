#!/usr/bin/env bash
# sirbone pre-commit review — installed by `sirbone hook install`.
#
# Reviews the staged diff with a read-only agent run: no key in a CI secret, no
# runner minutes, the same gate as `--review-only` in a workflow. The agent
# cannot write, build, or reach an MCP server, so the worst case is a wrong
# opinion.
#
# Fails open on purpose. A missing binary, a missing `jq`, an unconfigured
# provider, a timeout or an empty answer all let the commit through: a review
# assistant that blocks work when the network is down is a review assistant
# people uninstall. Only an explicit BLOCK verdict stops a commit.
#
#   git commit --no-verify     skip once
#   SIRBONE_HOOK=off           skip always
#   SIRBONE_HOOK_TIMEOUT=180   seconds before giving up (default 180)
set -uo pipefail

[ "${SIRBONE_HOOK:-on}" = "off" ] && exit 0
command -v sirbone >/dev/null 2>&1 || exit 0
git diff --cached --quiet && exit 0

if ! command -v jq >/dev/null 2>&1; then
    echo "sirbone hook: jq not found — skipping the review" >&2
    exit 0
fi

read -r -d '' prompt <<'PROMPT'
Review the staged diff of this repository — run `git diff --cached` yourself — for
correctness bugs: wrong logic, unhandled error paths, missing edge cases, broken
invariants. Ignore style and naming. Cite file:line for anything you report.

End your answer with a single final line, exactly one of:
VERDICT: BLOCK <one-line reason>
VERDICT: OK
PROMPT

run=(sirbone --review-only -p --output-format json "$prompt")
if command -v timeout >/dev/null 2>&1; then
    run=(timeout "${SIRBONE_HOOK_TIMEOUT:-180}" "${run[@]}")
fi

report=$("${run[@]}" 2>/dev/null | jq -r '.result // empty')
if [ -z "$report" ]; then
    echo "sirbone hook: no review (provider unconfigured, timed out, or empty answer)" >&2
    exit 0
fi

printf '%s\n' "$report" >&2

# Only the last VERDICT line counts: a diff under review can quote the word
# BLOCK, and the model can restate the format before answering.
verdict=$(printf '%s\n' "$report" | grep -E '^VERDICT: ' | tail -1)
case "$verdict" in
"VERDICT: BLOCK"*)
    echo >&2
    echo "sirbone: commit blocked —${verdict#VERDICT: BLOCK}" >&2
    echo "         commit anyway with: git commit --no-verify" >&2
    exit 1
    ;;
esac
exit 0
