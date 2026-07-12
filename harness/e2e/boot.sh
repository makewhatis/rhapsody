#!/usr/bin/env bash
# boot e2e — F1's boot gate. The ASSEMBLED `rhapsodyd` daemon (real orchestrator + store + httpapi,
# built from this tree) completes a scripted issue end-to-end against the R3 harness (linear-stub +
# fake-claude), serves the embedded React dashboard non-empty, publishes a reachable runtime.json
# port, its live `/api/v1/config` byte-matches the committed golden, and `rhapsodyd mcp` discovers +
# reaches the daemon through that runtime.json. Runs in CI (needs node, cargo, jq, curl). Never
# touches the reference tree; everything lives under a private $HOME so the daemon's DB + runtime.json
# stay isolated from any real daemon on the runner.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
FIX="$ROOT/harness/fixtures"
NORMALIZE="$ROOT/harness/capture/normalize.sh"
WORK="$(mktemp -d)"

# Build the dashboard bundle into crates/httpapi/web-dist/ (the rust-embed source), THEN the daemon —
# the embed is compile-time, so the daemon MUST build after the dist exists for the dashboard to be
# non-empty. Then linear-stub (the scripted Linear GraphQL double).
echo "boot-e2e: building web dashboard bundle (rust-embed source)" >&2
( cd "$ROOT/web" && npm ci && npm run build )
echo "boot-e2e: building rhapsodyd + linear-stub" >&2
( cd "$ROOT" && cargo build -p rhapsodyd -p linear-stub )
RHAPSODYD="$ROOT/target/debug/rhapsodyd"
STUB="$ROOT/target/debug/linear-stub"

STUB_PID=""
DAEMON_PID=""
cleanup() {
  [ -n "$DAEMON_PID" ] && kill "$DAEMON_PID" 2>/dev/null || true
  [ -n "$STUB_PID" ] && kill "$STUB_PID" 2>/dev/null || true
  wait 2>/dev/null || true
  rm -rf "$WORK"
}
trap cleanup EXIT

# --- start linear-stub (success scenario, ephemeral port) ---
"$STUB" --scenario "$ROOT/harness/capture/scenarios/success.json" --port 0 >"$WORK/stub.log" 2>&1 &
STUB_PID=$!
for _ in $(seq 1 100); do grep -q LISTENING "$WORK/stub.log" && break; sleep 0.1; done
STUB_PORT="$(sed -n 's/^LISTENING //p' "$WORK/stub.log" | head -1)"
[ -n "$STUB_PORT" ] || { echo "boot-e2e: linear-stub did not announce a port" >&2; cat "$WORK/stub.log" >&2; exit 1; }

# --- assemble the WORKFLOW.md (minimal.md) against a private $HOME ---
# normalize.sh rewrites this $HOME prefix to <HOME>, so the live /api/v1/config matches the golden
# regardless of the actual temp path.
export CAPTURE_HOME="$WORK/home"
mkdir -p "$CAPTURE_HOME/bin"
cp "$ROOT/harness/stubs/fake-claude" "$CAPTURE_HOME/bin/"
sed -e "s|__STUB_PORT__|$STUB_PORT|g" \
    -e "s|__CLAUDE_CMD__|$CAPTURE_HOME/bin/fake-claude|g" \
    -e "s|__STORE_PATH__|$CAPTURE_HOME/symphony.db|g" \
    "$ROOT/harness/capture/workflows/minimal.md" >"$CAPTURE_HOME/WORKFLOW.md"

# --- boot the daemon (server.port: 0 → ephemeral; runtime.json + DB under $CAPTURE_HOME) ---
HOME="$CAPTURE_HOME" FAKE_CLAUDE_SLEEP_S=0 "$RHAPSODYD" "$CAPTURE_HOME/WORKFLOW.md" >"$WORK/daemon.log" 2>&1 &
DAEMON_PID=$!
for _ in $(seq 1 100); do [ -f "$CAPTURE_HOME/.symphony/runtime.json" ] && break; sleep 0.1; done
[ -f "$CAPTURE_HOME/.symphony/runtime.json" ] || {
  echo "boot-e2e: daemon did not publish runtime.json" >&2; cat "$WORK/daemon.log" >&2; exit 1; }
PORT="$(jq -r .port "$CAPTURE_HOME/.symphony/runtime.json")"
API="http://127.0.0.1:$PORT"

# 1. the published runtime port is live + reachable (the target `rhapsodyd mcp` dials).
curl -fsS "$API/healthz" >/dev/null
echo "boot-e2e: /healthz ok on the published runtime port $PORT" >&2

# 2. api golden against the LIVE binary: /api/v1/config is deterministic from the workflow, so the
# assembled daemon must serve bytes identical to the committed golden after normalize.
curl -fsS "$API/api/v1/config" | jq -S . | "$NORMALIZE" >"$WORK/config.got.json"
if ! diff -u "$FIX/api/config.json" "$WORK/config.got.json"; then
  echo "boot-e2e: live /api/v1/config does not match the committed golden (parity drift)" >&2
  exit 1
fi
echo "boot-e2e: /api/v1/config byte-matches the golden against the live binary" >&2

# 3. the scripted issue completes end-to-end: a run reaches ended_at (fake-claude exits `continued`).
ok=0
for _ in $(seq 1 200); do
  if curl -fsS "$API/api/v1/history" | jq -e '.runs[] | select(.ended_at != "")' >/dev/null 2>&1; then ok=1; break; fi
  sleep 0.05
done
[ "$ok" = "1" ] || {
  echo "boot-e2e: scripted issue did not complete" >&2
  curl -fsS "$API/api/v1/history" >&2 || true; cat "$WORK/daemon.log" >&2; exit 1; }
echo "boot-e2e: scripted issue completed end-to-end (a run reached ended_at)" >&2

# 4. /api/v1/state is live + well-formed (carries the status field the SPA needs).
curl -fsS "$API/api/v1/state" | jq -e '.status' >/dev/null || {
  echo "boot-e2e: /api/v1/state did not return live state" >&2; exit 1; }

# 5. the embedded React dashboard is served non-empty (rust-embed → index.html at /).
body="$(curl -fsS "$API/")"
[ -n "$body" ] || { echo "boot-e2e: dashboard root is empty" >&2; exit 1; }
printf '%s' "$body" | grep -qiE '<!doctype html|<html|id="root"|<title' || {
  echo "boot-e2e: dashboard root is not the embedded HTML app" >&2; printf '%s' "$body" | head >&2; exit 1; }
echo "boot-e2e: embedded dashboard served non-empty" >&2

# 6. `rhapsodyd mcp` discovers the daemon via runtime.json + reaches it (symphony_state → live state,
# NOT daemon_unreachable). The trailing `sleep` keeps stdin open long enough for the async tool-call
# response to flush before EOF closes the stdio facade (closing immediately after the frames races the
# loopback round-trip); EOF then exits it cleanly. The `symphony_state` payload rides in the result's
# `text` field as an ESCAPED JSON string, so we key on the top-level (unescaped) tool-call outcome:
# `daemon_unreachable` → the facade couldn't reach the daemon; `"isError":false` → symphony_state
# returned live daemon state.
mcp_out="$({ printf '%s\n%s\n%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"boot-e2e","version":"0"}}}' \
  '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"symphony_state","arguments":{}}}'; \
  sleep 3; } | HOME="$CAPTURE_HOME" "$RHAPSODYD" mcp "$CAPTURE_HOME/WORKFLOW.md" 2>"$WORK/mcp.err")" || true
if printf '%s' "$mcp_out" | grep -q daemon_unreachable; then
  echo "boot-e2e: rhapsodyd mcp could not reach the daemon via runtime.json" >&2
  printf '%s\n' "$mcp_out" >&2; cat "$WORK/mcp.err" >&2; exit 1
fi
printf '%s' "$mcp_out" | grep -q '"isError":false' || {
  echo "boot-e2e: rhapsodyd mcp symphony_state did not return live daemon state" >&2
  printf '%s\n' "$mcp_out" >&2; cat "$WORK/mcp.err" >&2; exit 1; }
echo "boot-e2e: rhapsodyd mcp reached the daemon via runtime.json discovery" >&2

echo "boot-e2e: PASS" >&2
