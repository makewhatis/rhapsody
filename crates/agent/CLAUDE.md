# CLAUDE.md — crates/agent

Parity port of Go `internal/agent` (+ its `claude` subpackage). Ports `agent.go`, `errors.go`,
`humanize.go`, and the `fake` backend at the crate root; `src/claude/` ports the `claude` subpackage
(`args.go`, `billing.go`, `mcpinject.go`, `parse.go`, `runner.go`) as an ordinary Rust module — it has
no `Cargo.toml` of its own and is just the crate's second backend, not a separate build unit.

## Layout

| File | Go source | Role |
|---|---|---|
| `src/lib.rs` | `agent.go`, `errors.go` | `Runner`/`Session` traits, `Event`/`TurnResult`/`Usage`, `AgentError` |
| `src/humanize.rs` | `humanize.go` | stream-json line → `LogEntry` for the `/log` API/dashboard |
| `src/fake.rs` | `internal/agent/fake` | scriptable in-process backend, the orchestrator's test double |
| `src/proctree.rs` | — | harness-agnostic process-TREE kill (`kill_tree`, `KillTreeOnDrop`); no Go counterpart |
| `src/harness.rs` | — | the pluggable-harnesses contract (`HarnessSpec`/`HarnessCapabilities`/`Harness`, STUDIO-900); no Go counterpart — the frozen reference runs one backend. `Harness: Runner`, so `Runner`/`Session` stay the only traits the orchestrator schedules against |
| `src/manager.rs` | — | the manager run's isolated startup posture (STUDIO-1014; design `manager-agent-design.md` §4.2–§4.4); no Go counterpart. Pure: `manager_config`/`manager_args` (permission-mode `default`, allowlist = manager MCP tools only, deny-list = every built-in, `--mcp-config` + `--strict-mcp-config`, `--setting-sources user`, operator `extra_args` dropped) and `manager_mcp_config` (one server, `--role manager`). Never inherit `bypassPermissions` |
| `src/claude/mod.rs` | `internal/agent/claude` | re-exports; module doc lists the five submodules' Go files 1:1 |
| `src/claude/args.rs` | `args.go` | `Config` + `build_args`/`split_command` |
| `src/claude/billing.rs` | `billing.go` | env-scrub name sets + billing-guard decisions |
| `src/claude/mcpinject.rs` | `mcpinject.go` | per-workspace `.symphony-mcp.json` merge + "me" identity env |
| `src/claude/parse.rs` | `parse.go` | one stream-json line → normalized `Event`/`TurnResult` |
| `src/claude/runner.rs` | `runner.go` | the subprocess `Runner`/`Session` impl; wires the four modules above; also implements `harness::Harness` for `Runner` (STUDIO-900) — its declared `HarnessCapabilities` live next to the behavior they describe |
| `src/opencode/*` | — | the SECOND backend (STUDIO-902); no Go counterpart. The same module split as `claude/` (`args`/`parse`/`mcpinject`/`runner`) plus `state` (the per-run `XDG_DATA_HOME`, now with explicit legacy/brokered modes), `probe` (the STUDIO-995 managed-version probe against the committed broker fixtures), and `brokered` (PB6/STUDIO-1001: the internal provider id, generated config/auth, managed env, pinned brokered argv, and the capability/broker-URL streaming redactor), built against the committed captures in `harness/harness-spike/opencode/`. Read its `mod.rs` first: it tabulates every spike finding against the module that implements it |
| `tests/fake_claude_gate.rs` | — | P4 phase gate: runs the real Claude `Runner` against the committed `harness/stubs/fake-claude*` and diffs the humanized output against `harness/fixtures/runs/*.jsonl` |
| `tests/opencode_broker_fixture.rs` | — | PB0 phase gate (STUDIO-995): pins the committed managed-OpenCode broker request fixtures under `harness/harness-spike/opencode/broker/requests/` — request counts, route/auth/model/`max_tokens`/SSE shape, the closed schema, and a secret/machine-path sanitization scan |

## Architecture — reading order

Read `lib.rs`'s module doc first: it fixes the porting conventions the whole crate follows (Go
`type X string` enums → `&'static str` consts + `String` fields; Go `ctx context.Context` dropped in
favor of `async fn` since callers are tokio-based; Go `int` → `i64`; Go pointers → `Option<T>`). Every
other file in the crate assumes these without restating them.

`runner.rs` is where the other four `claude/` modules compose into one turn — it's the file to read
to understand the crate's actual behavior, not any single module in isolation:

- **One process per turn.** `ClaudeSession::run_turn` spawns a fresh `claude` subprocess every turn
  (continuation turns pass `--resume <thread_id>`, captured from the first `session_id`-bearing
  stream line — even an unclassified one, so `--resume`/billing association is never lost). The child
  runs in its own Unix process group (`process_group(0)`), but the group is NOT the containment
  boundary: every harness `setpgid`s the shell it runs a tool command in, so a group kill takes the
  leader and leaves the model's `git push` running (STUDIO-871). A stall/deadline/Stop kill therefore
  goes through `proctree::kill_tree`, which walks the descendant tree and signals every group it
  spans — the leader's group unconditionally, so STUDIO-840's guarantee holds even when `ps` cannot
  be read. It lives outside `claude/` deliberately: a future opencode or codex backend arms the same
  `KillTreeOnDrop`. This crate is Unix-only as written (`libc::kill`, `process_group`); there's no
  Windows path. (Since STUDIO-902 that "future opencode backend" exists and arms exactly this
  guard — its own runner test asserts an escaped tool child dies with the turn.)
- **stdin is an operator mailbox (INF-250)** — for claude. stdin stays open after the initial
  prompt; a second `mpsc::Receiver<String>` (passed the SAME channel across continuation turns) is
  drained inside the same `tokio::select!` as the stdout scanner, folding queued operator messages
  into the live turn. The mailbox arm is gated `if mailbox_open && !terminal_seen` and stdin is
  dropped the instant a terminal result is classified — "no write after result" is structural, not
  just documented. ⚠️ It is NOT a crate-wide assumption: `harness::StdinPolicy` declares the
  requirement per backend, and `opencode` closes stdin at start (its prompt is an argv positional),
  so that backend has no mailbox, declares `Steering::BetweenTurns`, and logs any message it could
  not deliver rather than dropping it silently.
- **Billing guard is fail-closed and per-turn.** Every turn's first `system`/`init` line must report
  `apiKeySource == "none"` (checked once per turn via `billing_checked`, since `--resume` re-emits its
  own init). A non-`"none"` source kills the process tree immediately (`BillingGuard`); a result
  observed with `guard_on` but no init ever seen is *also* refused (`billing_guard_failed: no system/init
  observed`) — a result can't be trusted to be guard-compliant without positive confirmation.
- **Brokered OpenCode is a separate entry point (PB6, STUDIO-1001).** `opencode::start_brokered_session`
  probes the pinned version, provisions a credential-FREE `RunState::provision_brokered` (no
  `auth.json`), and mints a per-session `rhapsody-<22 base64url chars>` internal provider id. Its
  turns run through `Session::run_turn_brokered`, which re-probes the command BEFORE the worker's
  `BrokerTurnAttempt` is minted, generates the per-turn `OPENCODE_CONFIG_CONTENT`/`OPENCODE_AUTH_CONTENT`
  (only the internal provider), strips inherited `OPENCODE_*`/`XDG_DATA_HOME` before appending the
  managed allow-list, and redacts the capability plus broker base URL/authority from raw child chunks
  before parse/tee/event/error. The `Session` trait's defaulted `run_turn_brokered` drops any attempt
  and runs the ordinary turn, so every legacy backend is byte-identical.
- **Env scrub is re-applied every turn**, including resumes: `TRACKER_ENV_VARS` (`LINEAR_API_KEY`,
  by name *and* by the configured credential's value) are always stripped; `BILLING_ENV_VARS`
  (`ANTHROPIC_*`, `CLAUDE_CODE_USE_*`) are stripped only when the guard is on. `append_me_env` runs
  *after* the scrub so `SYMPHONY_ISSUE`/`SYMPHONY_RUN_ID` survive it.
- **argv order is a byte-compatible contract**, not incidental: `args.rs`'s `build_args` asserts the
  full vector in tests, and operator `extra_args` must always be appended last so an operator can
  override any managed flag (including a managed `--settings {"ultracode":...}`). Don't reorder flags
  without checking `args_test`-equivalent assertions and the Go reference.
- **Truncation direction is asymmetric and meaningful**: `parse.rs` head-truncates assistant
  notification text (`MAX_MESSAGE_LEN`, keeps the start) but *tail*-truncates the final result text
  (`MAX_RESULT_TEXT`, keeps the end) because the `HANDOFF:` marker the orchestrator scans for lives on
  the last line. Both truncators back off to a UTF-8 char boundary rather than the raw byte cut.

## Conventions specific to this crate

- **Interior mutability via `Mutex`/`Atomic*`, not `RefCell`.** `Session`/`Runner` trait methods take
  `&self` (so the orchestrator can hold `Box<dyn Session>` across turns without `&mut`), so any
  per-turn state (`thread_id`, `turn_n`, the transcript sink, warn-once flags) lives behind
  `Mutex`/`AtomicI64`/`AtomicBool`. Every lock site recovers a poisoned lock rather than panicking
  (`.lock().unwrap_or_else(|e| e.into_inner())`) — follow this pattern for any new locked state; a
  panicking accessor here would violate the workspace's `-D warnings` clippy posture on a code path
  that must never itself be the cause of a turn failure.
- **MCP injection and transcript teeing are best-effort.** A failed `inject_symphony_mcp` warns and
  falls back to the operator's original `mcp_config` unchanged (never blocks the run); a failed
  transcript write warns once (`transcript_warned`) and never aborts the turn — the capped in-memory
  stderr buffer, not the transcript sink, is what a failure error is built from.

## Testing this crate

- `runner.rs`'s own tests spawn `bash <script>` "fake claude" scripts written inline per test (not
  the committed `harness/stubs/fake-claude`) to control stream-json output precisely. Every test that
  invokes `run_turn` (which reads `std::env::vars_os()` for the scrub) must hold
  `crate::ENV_GUARD.read().await` first; the two tests that mutate process env with
  `set_var`/`remove_var` must take the write lock instead. `std::env` is not internally synchronized
  in Rust the way Go's `os` package is — add the read-lock line to any new `run_turn`-invoking test
  or it can race. The guard lives at the CRATE root (STUDIO-902), not in one backend's test module:
  both backends' runners read the environment, so a per-module guard would serialize each backend
  against itself and against nothing else.
- `src/opencode/runner.rs`'s tests follow the same pattern, with one addition worth copying: its
  fake CLI REPRODUCES the hazard (it refuses a state directory already in use, exactly as a real
  turn losing the `database is locked` race does — zero events, exit 1) rather than always
  succeeding, so deleting the per-run isolation turns the concurrency test red instead of leaving it
  green.
- `tests/fake_claude_gate.rs` is the crate's one integration test: it drives the *real*
  `claude::Runner` against the committed `harness/stubs/fake-claude*` scripts and diffs the humanized
  event stream against `harness/fixtures/runs/*.jsonl` (see `harness/CLAUDE.md`). It resolves stub
  paths via `CARGO_MANIFEST_DIR`-relative canonicalization, so it only works run from within the repo
  checkout (`cargo test -p rhapsody-agent` from anywhere inside the tree is fine; copying just this
  crate out is not).
- No fixture recapture needed for this crate specifically — `make fixtures` (operator-machine-only,
  root CLAUDE.md) regenerates the goldens `fake_claude_gate.rs` reads, but the gate itself runs in
  plain `cargo test`.
