#!/usr/bin/env bash
# ACP handshake smoke: drives `sirbone acp` through initialize -> session/new ->
# session/prompt -> session/load and asserts the JSON-RPC responses + streamed
# session/update notifications. Live (needs a provider key); skips cleanly if none.
#
#   bash playground/acp_smoke.sh [path/to/sirbone]
#
# Offline note: the ACP mapping logic itself is unit-tested (cargo test --lib acp);
# this script exercises the real stdio protocol end-to-end against a live model.
set -euo pipefail

BIN="${1:-target/debug/sirbone}"
[ -x "$BIN" ] || BIN="$(command -v sirbone || true)"
if [ -z "${BIN:-}" ] || [ ! -x "$BIN" ]; then
  echo "SKIP: no sirbone binary (build it, or pass a path)"; exit 0
fi
BIN="$(realpath "$BIN")"  # absolute: the driver spawns it from a temp cwd

# Provider key present? (env or the global ~/.sirbone/.env)
if [ -z "${ANTHROPIC_AUTH_TOKEN:-}${ANTHROPIC_API_KEY:-}${OPENAI_API_KEY:-}" ] \
   && ! grep -qE 'ANTHROPIC_AUTH_TOKEN|ANTHROPIC_API_KEY|OPENAI_API_KEY' "$HOME/.sirbone/.env" 2>/dev/null; then
  echo "SKIP: no provider key configured (env or ~/.sirbone/.env)"; exit 0
fi

WS="$(mktemp -d)"; trap 'rm -rf "$WS"' EXIT

python3 - "$BIN" "$WS" <<'PY'
import json, subprocess, sys, threading, queue, time, os
BIN, WS = sys.argv[1], sys.argv[2]
p = subprocess.Popen([BIN, "acp"], cwd=WS, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                     stderr=subprocess.DEVNULL, text=True, bufsize=1)
q = queue.Queue()
threading.Thread(target=lambda: [q.put(l.strip()) for l in p.stdout] or q.put(None), daemon=True).start()
def send(o): p.stdin.write(json.dumps(o) + "\n"); p.stdin.flush()
def wait(pred, t=120):
    end = time.time() + t
    while time.time() < end:
        try: line = q.get(timeout=end - time.time())
        except queue.Empty: break
        if line is None: sys.exit("stream closed early")
        try: m = json.loads(line)
        except Exception: continue
        if pred(m): return m
    sys.exit("timeout")

send({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}})
assert wait(lambda m: m.get("id")==1)["result"]["protocolVersion"] == 1
print("ok initialize")

send({"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":WS,"mcpServers":[]}})
sid = wait(lambda m: m.get("id")==2)["result"]["sessionId"]
print("ok session/new")

seen = {"chunk": False}
def done(m):
    if m.get("method")=="session/update" and m["params"]["update"].get("sessionUpdate")=="agent_message_chunk":
        seen["chunk"] = True
    return m.get("id")==3
send({"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":sid,
      "prompt":[{"type":"text","text":"Reply with exactly the word OK."}]}})
assert wait(done)["result"]["stopReason"] in ("end_turn","cancelled")
assert seen["chunk"], "no agent_message_chunk streamed"
print("ok session/prompt (streamed + stopReason)")

send({"jsonrpc":"2.0","id":4,"method":"session/load","params":{"sessionId":sid,"cwd":WS,"mcpServers":[]}})
ups = {"n": 0}
def loaded(m):
    if m.get("method")=="session/update": ups["n"] += 1
    return m.get("id")==4 and "result" in m
wait(loaded, t=30)
assert ups["n"] > 0, "session/load replayed nothing"
print(f"ok session/load (replayed {ups['n']} updates)")

p.stdin.close(); p.terminate()
print("\nACP SMOKE PASSED")
PY
