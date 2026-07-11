# Fixture capture

`make fixtures` (→ `harness/capture/capture.sh`) rebuilds the committed golden fixtures in
`harness/fixtures/` from the **reference Go daemon** (Symphony v0.4.0, read-only). It runs
**on the operator's machine only** — CI never builds Go; the fixtures are committed and every
Rust crate asserts against them via `harness-fixtures` (Task R5).

Requires: **Go ≥ 1.25**, **cargo** (builds `linear-stub`), **jq**, **sqlite3**, **curl**.

## What it captures

| Fixture | Source | Notes |
| -- | -- | -- |
| `fixtures/schema.sql` | `sqlite3 <db> .schema` on a Go-initialized DB | the 6 authored `CREATE TABLE`s |
| `fixtures/config/{minimal,full,graphite}.json` | `GET /api/v1/config` per workflow | effective config across the surface |
| `fixtures/api/*.json` | scripted `GET /api/v1/*` (state, config, projects, history, metrics, events, logs, run_detail + error/stalled variants) | response envelopes |
| `fixtures/runs/{success,success_transcript,error,stalled}.jsonl` | `GET /api/v1/runs/{id}/{events,transcript}` | per-run lifecycle streams |
| `fixtures/db/go-daemon.db` + `fixtures/db/go-daemon-rows.json` | the success-run SQLite database (`VACUUM INTO`) + a `sqlite3 -json` per-table dump | round-trip fixture for `rhapsody-store`; **the rows-JSON is the determinism artifact, the `.db` binary is exempt** (see below) |

Each scenario boots `linear-stub` (R3) + the daemon against a private `$CAPTURE_HOME` (the
daemon's `$HOME`), drives one run with `fake-claude*` (R3), and records the API. The workflow
inputs live in `workflows/` (config-fixture inputs `minimal`/`full`/`graphite`, plus the `hang`
stall variant) and `scenarios/` (`success`/`error`/`hang` Linear issue data).

## Determinism contract

Running capture twice **must** produce an empty `diff -r` — this is the R4 acceptance gate. The one
documented exception is the binary `fixtures/db/go-daemon.db` (see "Go-written database fixture"
below): its bytes vary between captures, so it is excluded and determinism is asserted on its
normalized `go-daemon-rows.json` dump instead, which **must** diff empty like every other fixture:

```sh
make fixtures && cp -r harness/fixtures /tmp/fix1 && make fixtures \
  && diff -r -x go-daemon.db /tmp/fix1 harness/fixtures
```

Nondeterministic values are rewritten to placeholders by `normalize.sh` (the single source of
truth; `harness_fixtures::normalize` mirrors it **exactly** — change them in lockstep):
`<TIMESTAMP>`, `<UUID>`, `<HOME>`, `<PORT>`, `<NUM>`. There must be **no** timestamps, UUIDs,
absolute paths, or live ports anywhere in a committed fixture.

## Recapture

Re-run `make fixtures` (the parity target is frozen at v0.4.0, so this is only needed to fill a
fixture gap). State the reason in the PR body. **Never hand-edit a fixture** — that is drift
laundering; the loader's canary (R5) exists to catch it.

## Capture-fidelity notes (why the script does what it does)

- **Reference path / macOS TCC.** `REF` defaults to the frozen
  `~/Downloads/symphony-v0.4.0/golang/symphony`. macOS TCC blocks `~/Downloads`
  for daemon-spawned processes on some machines; when the primary `REF` is unreadable the script
  falls back to the spec-documented copy at `~/workspace/symphony-go-reference/golang/symphony`
  (design §2/§6). Override with `REF=/path/to/symphony make fixtures`. The Go build output and
  work dir live under `harness/capture/{target,work}` — **never** inside `$REF`.
- **`schema.sql` = the 6 authored tables.** `run_messages` uses `AUTOINCREMENT`, so SQLite also
  materializes its internal `sqlite_sequence` bookkeeping table. That single engine-internal line
  is filtered out (`grep -v '^CREATE TABLE sqlite_sequence'`) so the golden holds exactly the six
  tables Symphony's migrations author (spec §3.1) — every authored table and index is kept
  byte-for-byte. Both a Go- and a rusqlite-written DB produce `sqlite_sequence` identically, so
  excluding it loses no parity signal.
- **Stalled run uses `turn_timeout_ms`, not `stall_timeout_ms`.** The daemon's CPU-liveness stall
  detector needs a readable `/proc` and is disabled on macOS (the capture host): *"CPU-based
  liveness unavailable (no readable /proc); stall detection will not fire."* `hang.md` therefore
  trips the platform-independent per-turn `turn_timeout_ms` (3s); the hung agent still yields
  `outcome:"failed"`, exactly the run the stalled fixtures need.
- **Single-run success snapshot.** `fake-claude` "success" exits `continued` (the no-op agent
  never drives the ticket to a terminal state), so the daemon re-dispatches a continuation ~1s
  later. The success/api fixtures are snapshotted inside that one-run window and the capture
  re-runs the scenario if a continuation raced in (the run list must show exactly one run).
- **Post-flush event capture.** Run events reach the store on the writer's async flush
  (`flushInterval` = 1s). The capture waits for that flush before snapshotting `runs/{id}/events`
  and the `/events` search, so those fixtures carry the settled lifecycle stream rather than a
  pre-flush empty table.
- **Go-written database fixture (`db/`).** After the success run, the daemon's SQLite database is
  committed so `rhapsody-store` (P2) can round-trip a real Go-written file in CI without ever
  opening `$REF`. Two subtleties drive how it is captured:
  - **`VACUUM INTO`, not `cp`.** The daemon opens SQLite in **WAL mode** (`$REF/internal/store/sqlite.go`)
    and the capture stops it by killing the process, so freshly committed rows can still live in the
    un-checkpointed `symphony.db-wal` sidecar. A plain `cp` of `symphony.db` alone would miss those
    rows *and* leave the fixture WAL-marked (reading it would then spawn `-wal`/`-shm` sidecars in
    the tree). `sqlite3 VACUUM INTO` reads the committed WAL data through a fresh connection and
    writes a complete, self-contained, rollback-mode (`journal_mode=delete`) snapshot with no
    sidecars — the file opens cleanly read-only on CI.
  - **The `.db` binary is a determinism exception; the rows-JSON is not.** SQLite page layout and
    embedded rowids/timestamps make the `.db` bytes differ between captures, so `go-daemon.db` is
    excluded from the double-capture `diff` (commit the copy from the **last** capture). Determinism
    is instead asserted on `go-daemon-rows.json` — a `sqlite3 -json` dump of every authored table
    (empty tables → `[]`), built from the committed `.db` and piped through `normalize.sh` so it
    holds no timestamps/UUIDs/paths/ports. That dump **must** diff empty across captures.
