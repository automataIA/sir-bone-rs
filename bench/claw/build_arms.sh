#!/usr/bin/env bash
# Freeze the two campaign arms as static musl binaries.
#
#   control    = a committed SHA (default: HEAD) built in a detached worktree
#   candidate  = the current working tree (uncommitted work included)
#
# Built inside rust:1-alpine so the host needs no musl toolchain, and verified
# static: a dynamically linked binary dies in most SWE-bench images.
#
#   bash bench/claw/build_arms.sh [CONTROL_SHA]
#
# Writes bench/claw/arms/{sirbone-control,sirbone-candidate,manifest.json}.
set -euo pipefail

REPO="$(git rev-parse --show-toplevel)"
OUT="$REPO/bench/claw/arms"
CONTROL_SHA="$(git -C "$REPO" rev-parse "${1:-HEAD}")"
TARGET_DIR="/tmp/sirbone-claw-target"   # shared cargo target, outside the repo
REGISTRY_DIR="/tmp/sirbone-claw-registry"  # not ~/.cargo: the container is root
WORKTREE="/tmp/sirbone-claw-control"

mkdir -p "$OUT" "$TARGET_DIR" "$REGISTRY_DIR"

build() {  # build <source-dir> <output-name>
    local src="$1" name="$2"
    echo "==> building $name from $src"
    docker run --rm \
        -v "$src:/src:ro" \
        -v "$TARGET_DIR:/target" \
        -v "$REGISTRY_DIR:/usr/local/cargo/registry" \
        -w /src \
        -e CARGO_TARGET_DIR=/target \
        rust:1-alpine \
        sh -c "apk add --no-cache musl-dev build-base perl make >/dev/null &&
               cargo build --release --target x86_64-unknown-linux-musl"
    cp "$TARGET_DIR/x86_64-unknown-linux-musl/release/sirbone" "$OUT/$name"
    chmod +x "$OUT/$name"
    # musl builds land as "static-pie linked"; plain "statically linked" is the
    # non-PIE form. Anything else carries an interpreter and dies on GLIBC.
    if ! file "$OUT/$name" | grep -Eq "statically linked|static-pie linked"; then
        echo "FATAL: $name is not statically linked — it will not run in SWE-bench images" >&2
        file "$OUT/$name" >&2
        exit 1
    fi
}

# control: clean checkout of the frozen SHA, no uncommitted candidate work.
rm -rf "$WORKTREE"
git -C "$REPO" worktree prune
git -C "$REPO" worktree add --detach "$WORKTREE" "$CONTROL_SHA" >/dev/null
trap 'git -C "$REPO" worktree remove --force "$WORKTREE" 2>/dev/null || true' EXIT
build "$WORKTREE" sirbone-control

build "$REPO" sirbone-candidate

cat > "$OUT/manifest.json" <<JSON
{
  "built_at": "$(date -Iseconds)",
  "control": {
    "sha": "$CONTROL_SHA",
    "sha256": "$(sha256sum "$OUT/sirbone-control" | cut -d' ' -f1)",
    "version": "$("$OUT/sirbone-control" --version | tr -d '\n')"
  },
  "candidate": {
    "sha": "$(git -C "$REPO" rev-parse HEAD)",
    "dirty": $(git -C "$REPO" diff --quiet && echo false || echo true),
    "diff_sha256": "$(git -C "$REPO" diff HEAD | sha256sum | cut -d' ' -f1)",
    "sha256": "$(sha256sum "$OUT/sirbone-candidate" | cut -d' ' -f1)",
    "version": "$("$OUT/sirbone-candidate" --version | tr -d '\n')"
  }
}
JSON

echo
cat "$OUT/manifest.json"
echo
echo "A dirty candidate is only reproducible through diff_sha256 — commit before a publishable campaign."
