#!/usr/bin/env bash
# capture — record golden parity fixtures from the reference Go daemon (Symphony v0.4.0).
# Operator-machine only (needs Go >= 1.25, cargo, sqlite3, jq, curl); CI never runs this — the
# fixtures under harness/fixtures/ are committed. Determinism contract (plan R4 Step 5): running
# `make fixtures` twice yields an empty `diff -r`. NEVER writes into $REF; the Go build output +
# work dir live under harness/capture/{target,work}.
#
# The daemon is driven against the R3 harness (linear-stub + fake-claude*) reusing smoke.md's boot
# recipe. Each scenario runs with $HOME set to a private $CAPTURE_HOME, so every machine-specific
# path (DB, workspaces, transcripts, the fake-claude command copied under $CAPTURE_HOME/bin) shares
# one prefix that normalize.sh rewrites to <HOME>.
set -euo pipefail

# --- reference tree (READ-ONLY) -----------------------------------------------------------------
# REF is REQUIRED: the operator provides the path to the frozen, read-only Symphony v0.4.0 tree —
# none is committed here (run as `REF=/path/to/symphony make fixtures`). macOS TCC blocks
# ~/Downloads for daemon-spawned processes on some machines; when the given REF is unreadable, fall
# back to the spec-documented copy at ~/workspace/symphony-go-reference (design §2/§6).
REF="${REF:?set REF to the read-only Symphony v0.4.0 reference tree}"
if ! cat "$REF/go.mod" >/dev/null 2>&1; then
  fallback="$HOME/workspace/symphony-go-reference/golang/symphony"
  if cat "$fallback/go.mod" >/dev/null 2>&1; then
    echo "capture: given REF unreadable ($REF); using documented fallback $fallback" >&2
    REF="$fallback"
  else
    echo "capture: reference tree unreadable at REF=$REF and fallback $fallback." >&2
    echo "capture: restore the Symphony v0.4.0 tree (or copy it to ~/workspace/symphony-go-reference) — see harness/capture/README.md." >&2
    exit 1
  fi
fi

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
FIX="$ROOT/harness/fixtures"
WORK="$HERE/work"
# Local build output of the frozen Go reference daemon (`$REF/cmd/symphony`, binary name `symphony`).
BIN="$HERE/target/symphony-go"

rm -rf "$WORK" "$FIX"
mkdir -p "$WORK" "$HERE/target" "$FIX/config" "$FIX/api" "$FIX/runs" "$FIX/db"

echo "capture: building the reference daemon from $REF" >&2
( cd "$REF" && GOFLAGS=-mod=readonly go build -o "$BIN" ./cmd/symphony )
echo "capture: building linear-stub" >&2
( cd "$ROOT" && cargo build -p linear-stub )
STUB="$ROOT/target/debug/linear-stub"

STUB_PID=""
DAEMON_PID=""
API=""

# start_stack SCENARIO WORKFLOW CLAUDE_CMD_BASENAME — boot linear-stub + the daemon against a fresh
# $CAPTURE_HOME, and set $API to the daemon's loopback base URL. CLAUDE_CMD_BASENAME selects which
# fake-claude* (copied under $CAPTURE_HOME/bin) the workflow's claude.command points at.
start_stack() {
  local scenario="$1" workflow="$2" claude_cmd="$3"
  "$STUB" --scenario "$scenario" --port 0 >"$WORK/stub.log" 2>&1 &
  STUB_PID=$!
  for _ in $(seq 1 100); do grep -q LISTENING "$WORK/stub.log" && break; sleep 0.1; done
  local stub_port
  stub_port="$(sed -n 's/^LISTENING //p' "$WORK/stub.log" | head -1)"
  [ -n "$stub_port" ] || { echo "capture: linear-stub did not announce a port" >&2; cat "$WORK/stub.log" >&2; exit 1; }

  export CAPTURE_HOME="$WORK/home"
  rm -rf "$CAPTURE_HOME"
  mkdir -p "$CAPTURE_HOME/bin"
  cp "$ROOT/harness/stubs/fake-claude" "$ROOT/harness/stubs/fake-claude-error" \
     "$ROOT/harness/stubs/fake-claude-hang" "$CAPTURE_HOME/bin/"
  sed -e "s|__STUB_PORT__|$stub_port|g" \
      -e "s|__CLAUDE_CMD__|$CAPTURE_HOME/bin/$claude_cmd|g" \
      -e "s|__STORE_PATH__|$CAPTURE_HOME/symphony.db|g" \
      "$workflow" >"$CAPTURE_HOME/WORKFLOW.md"

  HOME="$CAPTURE_HOME" FAKE_CLAUDE_SLEEP_S=0 "$BIN" "$CAPTURE_HOME/WORKFLOW.md" >"$WORK/daemon.log" 2>&1 &
  DAEMON_PID=$!
  for _ in $(seq 1 100); do [ -f "$CAPTURE_HOME/.symphony/runtime.json" ] && break; sleep 0.1; done
  [ -f "$CAPTURE_HOME/.symphony/runtime.json" ] || {
    echo "capture: daemon did not publish runtime.json" >&2; cat "$WORK/daemon.log" >&2; exit 1; }
  API="http://127.0.0.1:$(jq -r .port "$CAPTURE_HOME/.symphony/runtime.json")"
  curl -fsS "$API/healthz" >/dev/null
}

stop_stack() {
  [ -n "$DAEMON_PID" ] && kill "$DAEMON_PID" 2>/dev/null || true
  [ -n "$STUB_PID" ] && kill "$STUB_PID" 2>/dev/null || true
  wait 2>/dev/null || true
  DAEMON_PID=""
  STUB_PID=""
}
trap stop_stack EXIT

# grab PATH FIXTURE — GET an endpoint, canonicalize key order (jq -S), normalize placeholders.
grab() { curl -fsS "$API$1" | jq -S . | "$HERE/normalize.sh" >"$FIX/$2"; }

# ------------------------------------------------------------------------------------------------
# config fixtures: boot per workflow, snapshot the effective config (GET /api/v1/config).
# ------------------------------------------------------------------------------------------------
for wf in minimal full graphite; do
  echo "capture: config/$wf.json" >&2
  start_stack "$HERE/scenarios/success.json" "$HERE/workflows/$wf.md" fake-claude
  grab /api/v1/config "config/$wf.json"
  stop_stack
done

# ------------------------------------------------------------------------------------------------
# api + schema + success-run fixtures: one run against minimal.md. fake-claude "success" exits
# `continued` (the no-op agent never drives the ticket terminal), so the daemon re-dispatches a
# continuation ~ContinuationDelayMS(1s) later. We snapshot the whole API in that quiet window and
# verify exactly one run landed; a rare race that catches run 2 retries the scenario.
# ------------------------------------------------------------------------------------------------
capture_success() {
  start_stack "$HERE/scenarios/success.json" "$HERE/workflows/minimal.md" fake-claude
  for _ in $(seq 1 200); do
    curl -fsS "$API/api/v1/history" | jq -e '.runs[] | select(.ended_at != "")' >/dev/null 2>&1 && break
    sleep 0.05
  done
  # Run 1's events reach the store on the async writer's next flush (flushInterval=1s), which for
  # this <1s run lands its COMPLETE set. Wait for that flush so runs/1/events + the /events search
  # + recent_events are the settled stream, not a pre-flush empty snapshot. The continuation fires
  # ~1s after run 1 ends, so this stays inside the single-run window; the n==1 guard below catches
  # the rare race.
  for _ in $(seq 1 60); do
    [ "$(curl -fsS "$API/api/v1/runs/1/events" | jq '.events | length')" -gt 0 ] && break
    sleep 0.05
  done
  grab /api/v1/state              api/state.json
  grab /api/v1/config             api/config.json
  grab /api/v1/projects           api/projects.json
  grab /api/v1/history            api/history.json
  grab /api/v1/metrics            api/metrics.json
  grab /api/v1/events             api/events.json
  grab /api/v1/logs               api/logs.json
  grab /api/v1/runs/1             api/run_detail.json
  grab /api/v1/runs/1/events      runs/success.jsonl
  grab /api/v1/runs/1/transcript  runs/success_transcript.jsonl
  sqlite3 "$CAPTURE_HOME/symphony.db" '.schema' | grep -v '^CREATE TABLE sqlite_sequence' >"$FIX/schema.sql"
  # Guard the continuation race: the whole snapshot must describe exactly one run.
  local n
  n="$(curl -fsS "$API/api/v1/history" | jq '.runs | length')"
  stop_stack
  [ "$n" = "1" ]
}
echo "capture: api + schema + success run" >&2
success_ok=0
for attempt in 1 2 3 4 5; do
  if capture_success; then success_ok=1; break; fi
  echo "capture: success snapshot raced a continuation (attempt $attempt/5); retrying" >&2
done
[ "$success_ok" = "1" ] || { echo "capture: could not obtain a clean single-run success snapshot" >&2; exit 1; }

# ------------------------------------------------------------------------------------------------
# Go-written database fixture (Task S1): commit the success run's SQLite DB so rhapsody-store can
# round-trip a real daemon-written file in CI without ever opening $REF. The daemon runs SQLite in
# WAL mode (see $REF/internal/store/sqlite.go) and stop_stack kills it without a clean checkpoint,
# so committed rows can still live in symphony.db-wal. `VACUUM INTO` reads that committed data
# through a fresh connection and writes a complete, self-contained, rollback-mode snapshot with no
# -wal/-shm sidecars — a plain `cp` of symphony.db alone can miss un-checkpointed rows and would
# drag WAL sidecars into the fixtures tree. This runs after the success block, where $CAPTURE_HOME
# still holds the clean single-run database and the daemon is already stopped.
#
# Determinism is asserted on the normalized rows JSON, NOT the .db binary: SQLite page layout and
# embedded rowids vary between captures, so the committed .db is a documented double-capture
# exception (see README.md). go-daemon-rows.json is a per-table `sqlite3 -json` dump (empty tables
# -> []) piped through normalize.sh, and is built from the committed .db so it provably describes it.
echo "capture: db/go-daemon.db + db/go-daemon-rows.json" >&2
sqlite3 "$CAPTURE_HOME/symphony.db" "VACUUM INTO '$FIX/db/go-daemon.db'"
{
  printf '{'
  sep=''
  for t in $(sqlite3 "$FIX/db/go-daemon.db" \
      "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name"); do
    rows="$(sqlite3 -json "$FIX/db/go-daemon.db" "SELECT * FROM \"$t\" ORDER BY 1")"
    printf '%s"%s":%s' "$sep" "$t" "${rows:-[]}"
    sep=','
  done
  printf '}'
} | jq -S . | "$HERE/normalize.sh" >"$FIX/db/go-daemon-rows.json"

# ------------------------------------------------------------------------------------------------
# error-run fixtures: fake-claude-error exits is_error:true -> run recorded `failed`. A failure
# escalates on the 10s FailureBackoffMS, so run 1 sits finished-and-alone for a wide window.
# ------------------------------------------------------------------------------------------------
echo "capture: error run" >&2
start_stack "$HERE/scenarios/error.json" "$HERE/workflows/minimal.md" fake-claude-error
for _ in $(seq 1 300); do
  curl -fsS "$API/api/v1/runs/1" | jq -e '.outcome == "failed"' >/dev/null 2>&1 && break
  sleep 0.1
done
# Let the async event writer flush run 1's events (flushInterval=1s). A failure escalates on the
# 10s backoff, so the window is wide — settle for up to 3s so error.jsonl is the flushed stream.
for _ in $(seq 1 60); do
  [ "$(curl -fsS "$API/api/v1/runs/1/events" | jq '.events | length')" -gt 0 ] && break
  sleep 0.05
done
grab /api/v1/runs/1        api/run_detail_error.json
grab /api/v1/runs/1/events runs/error.jsonl
stop_stack

# ------------------------------------------------------------------------------------------------
# stalled-run fixtures: fake-claude-hang never emits a result; hang.md's turn_timeout_ms:3000 kills
# the never-ending turn so run 1 is recorded `failed` in ~3s. (turn_timeout, not stall_timeout: the
# /proc-based stall detector is disabled on the macOS capture host — see hang.md.)
# ------------------------------------------------------------------------------------------------
echo "capture: stalled run" >&2
start_stack "$HERE/scenarios/hang.json" "$HERE/workflows/hang.md" fake-claude-hang
for _ in $(seq 1 300); do
  curl -fsS "$API/api/v1/runs/1" | jq -e '.outcome == "failed"' >/dev/null 2>&1 && break
  sleep 0.1
done
# The ~3s hang already spans several flush ticks, so run 1's events are settled; poll once to be sure.
for _ in $(seq 1 60); do
  [ "$(curl -fsS "$API/api/v1/runs/1/events" | jq '.events | length')" -gt 0 ] && break
  sleep 0.05
done
grab /api/v1/runs/1        api/run_detail_stalled.json
grab /api/v1/runs/1/events runs/stalled.jsonl
stop_stack

echo "capture: fixtures written to $FIX" >&2
