# CLAUDE.md — crates/orchestrator

Parity port of Go `internal/orchestrator` — one Go package split across 26 source files, all
mutating a single `Orchestrator` struct. The Rust port mirrors that file split one-to-one (35
files under `src/`, one per Go source file plus a couple of Rust-only internal ports — see below);
resist the urge to further decompose or merge modules; the file boundary IS the port boundary and
every file's own top-of-file doc comment names its exact Go source. Read that comment first when
touching a file you don't know — it also records that file's specific Go→Rust deviations, which
are not repeated here.

## Concurrency model (read this before touching state)

The Go orchestrator is loop-confined: only the one control goroutine mutates scheduling state.
The Rust port keeps that discipline as a single owning tokio task (`control_loop::run_loaded`,
`loop.rs`) selecting on an mpsc `Event` channel — channels in, channels out, no `Mutex` webs over
the `Orchestrator` struct itself. Concretely:

- Modules whose functions take `&mut self` / `&Orchestrator` and are called from the loop
  (`orchestrator`, `dispatch`, `select`, `claim`, `retry`, `reconcile`/`reconcile_run`, `promote`,
  `agentupdate`, `persist`, `recovery`, `reload`, `workspace_gc`, `snapshot`) are loop-confined —
  they never lock anything and must never be called from another task.
- Four exceptions exist today, each `RwLock`/cloneable-handle guarded on purpose — these are the
  only sanctioned seams, not an exhaustive ceiling; if you add a new one, document it here too:
  - `reads.rs` — the Settings "connected as" identity + projects picker, served off-loop by the
    future HTTP layer.
  - `stop.rs`'s `ControlHandle` — the off-loop surface for Stop/Resume/handoff/operator-messages
    (`message.rs`, `handoff.rs` build on it too).
  - `warnings.rs`'s `WarningsState` (`Orchestrator::warnings: Arc<WarningsState>`, wrapping an
    `RwLock<WarningMaps>`) — mutated by spawned resolver tasks running off the control task, with a
    generation-counter guard so a slow/older pass can't clobber a newer reload's warnings (see
    "API-facing views" below).
  - `teamsmemory.rs`'s `TeamsMemory` (`Orchestrator::teams_memory: Option<Arc<TeamsMemory>>`,
    STUDIO-645) — the `/api/v1/teams/*` handlers drive it entirely on the HTTP task, with **no
    control round-trip at all**, because the design requires a `teams_retain` never to block the
    control task; an event-channel round-trip would queue it behind the current tick. The control
    task's whole involvement is two `HashMap` writes (`bind_run` at dispatch, `release_run` at run
    exit) with no I/O, and the `RwLock` is never held across an `.await`.

  If you need to touch orchestrator state from outside the loop task, route through one of these
  four seams; if none fits, that's a real design decision — don't reach for a fifth ad hoc
  `Arc<Mutex<..>>` without updating this list.
- `worker.rs` runs as its own spawned task per attempt and touches NO orchestrator state directly —
  it only emits events outward via an `on_event` callback. Don't reach into `Orchestrator` from
  worker code; add an event variant instead.
- A recurring pattern in the on-loop modules (`retry.rs::on_retry`, `reconcile_run.rs::reconcile`):
  before any `.await` or `&mut self` mutation, snapshot the bits of `Effective`/`ResolvedProject`
  you need into owned locals. Go aliases these via raw pointers across the await; Rust's borrow
  checker forbids that, so every async decision path re-derives this "snapshot first" shape. Don't
  try to hold a borrow of `self.eff` across an `.await` — follow the existing owned-locals pattern
  instead of fighting the borrow checker with `Arc<Mutex<..>>`.

## Module groups (35 files)

- **Core state**: `orchestrator.rs` (the `Orchestrator` struct + `RunningEntry`/`EventRecord`),
  `effective.rs` (`Config` → `Effective`/`ResolvedProject`, rebuilt+swapped on reload).
- **Scheduling pipeline**, in the order a tick runs them: `dispatch.rs` (ordering + eligibility
  predicates) → `select.rs` (the per-tick greedy slot-budgeted pass) → `concurrency.rs` (pure slot
  math, no state) → `claim.rs` (pool-mode multi-daemon claim election) → `promote.rs` (DAG
  auto-promote, feeds back into next tick's select rather than dispatching inline — deliberate,
  reuses the same slot/label gates).
- **Run lifecycle**: `retry.rs` (retry queue + worker-exit classification), `worker.rs` (one agent
  attempt, off-loop task), `agentupdate.rs` (folds a worker event into `RunningEntry` + totals),
  `message.rs` (operator→live-run mailbox delivery, mid-run summons routing, and STUDIO-649's
  reopen seed — the Rhapsody-only half that hands a reopening summons to the run it triggered),
  `handoff.rs` (daemon-mediated review handoff, TRA-242 — an addition beyond Go Symphony, see the
  pitfall note below), `stop.rs` (Stop/Resume).
- **Reconciliation**: `reconcile.rs` (pure decision: refreshed state → `ReconcileAction`) vs.
  `reconcile_run.rs` (the apply side: grouping, per-project refresh, stall detection, workspace
  cleanup). Keep that split when editing — decision logic stays testable without a tracker.
- **Persistence/recovery**: `persist.rs` (write-through to the store; low-volume calls run sync
  on the control task, high-volume history events batch async via a writer thread),
  `recovery.rs` (boot-time state rebuild — see the identifier-vs-id pitfall below), `reload.rs`
  (WORKFLOW.md hot-reload; polls mtime instead of Go's fsnotify — no new dep, small latency
  tradeoff, see its module doc).
- **API-facing views**: `snapshot.rs` (the `Orchestrator` → `Snapshot` assembly) and
  `snapshot_json.rs` (the `/api/v1/state` wire shape — owns the golden-fixture parity gate, see
  Testing below). `issuelog.rs` is the `/log` transcript humanizer. `warnings.rs` is the two
  advisory producers (`GET /api/v1/projects`), resolved off the control task with a
  generation-counter guard against a slow pass clobbering a newer reload.
- **GitHub summons integration**: `ghsummons.rs` (repo parsing + the `SummonSource` trait + the
  real `gh`-exec impl) and `ghenrich.rs` (fetch/apply the enrichment onto a candidate). Both are
  Go `internal/orchestrator/*.go` ports, not to be confused with the next group.
- **Orchestrator-internal ports of dependency-free Go packages** (`internal/liveness`,
  `internal/obslog`; `internal/ghsummons` above is a third): `liveness.rs` and `obslog.rs`. These
  exist because the Go packages have no dedicated Rust crate and the orchestrator is their sole
  consumer — don't extract them into new crates without checking nothing else needs them first.
- **Rhapsody Teams** (STUDIO-639…645; no Go counterpart — design record
  `~/.rhapsody/docs/STUDIO-572-rhapsody-teams.md`): `teams.rs` is the T3a dispatch router — a pure,
  sync, zero-I/O `route()` called from `dispatch_issue` — plus T4's memory recall, which renders into
  the same turn-1 section. Recall reads **local files only**, and the orchestrator holds the CONCRETE
  `rhapsody_config::memory::LocalBank` for it (`teams_bank`), never a `dyn MemoryBackend`: the trait
  is async because T8's `hindsight` does HTTP, and `dispatch_issue` is `fn`, so a remote backend is
  unrepresentable on that path rather than merely discouraged. Don't "tidy" `teams_bank` into the
  trait object — that type choice IS the no-network-on-dispatch proof. `teamsmemory.rs` is the
  off-loop half (see the fourth seam above). `triage.rs` is the T3b **off-loop** triage
  task: it holds no `Orchestrator`, sends no control event and takes no lock the control task takes,
  which is exactly why the design puts the feature's one model turn there (§0.11.2 — a model call on
  the dispatch path was the STUDIO-551 head-of-line class). Spawned at the composition root
  (`rhapsodyd/run.rs`) beside the prune scheduler, and only for `manager.mode: labels+model`. It is
  NOT a state seam at all: unlike `teamsmemory.rs` it never touches orchestrator state.
- **Cross-cutting constants**: `backoff.rs` (retry-cadence math), `telemetry_attrs.rs` (the
  bounded metric-label cardinality contract — project/model/outcome/reason only; never add an
  issue/run/session id here, that's a correctness bug, not a style nit).
- `lib.rs` documents the historical O1–O8 porting-ticket chain and a "compiling-stub protocol":
  a not-yet-ported call is a typed `OrchestratorError::Unimplemented` stub tagged with its owning
  ticket, never `todo!()`/`panic!()`. That chain is complete (O8's gate: no stub markers remain),
  but if you ever see one, it's a real gap, not a placeholder to leave alone.

## Testing

- `testsupport.rs` (`#[cfg(test)]`) is shared scaffolding across the on-loop test modules — issue/
  blocker/set builders, a hand-built baseline `Effective` (Rust's `Arc<dyn Tracker>` etc. can't be
  a partial zero-value struct literal the way Go's is), and a `tracing`-capturing helper standing
  in for Go's slog-to-buffer tests. Add new cross-module test helpers here, not per-file.
- `filetracker_e2e.rs` is the INF-303 no-Linear end-to-end gate: a real control pass against a
  temp file tracker + the committed `harness/stubs/fake-claude` stub, zero network/spend. It drives
  `on_tick`/`on_retry` directly rather than starting the poll loop, so it stays deterministic.
  Gotcha: its `FAKE_CLAUDE_*` env knobs are injected via an `env VAR=val <cmd>` prefix per test, not
  `std::env::set_var` / `t.Setenv` — `#[tokio::test]`s in this file run in parallel (unlike Go's
  serial package tests) and a process-global env var would race across them.
- `snapshot_json.rs::render` is checked against the committed golden `harness/fixtures/api/state.json`
  via `harness-fixtures` (dev-dep) — same golden-parity pattern as the config crate. If you change
  the wire shape intentionally, recapture per root CLAUDE.md's fixtures instructions.

## Pitfalls specific to this crate

- **Two key spaces.** In-memory maps (`running`, `retry_attempts`, …) key by the tracker's opaque
  issue ID. The store's `claims`/`retry_queue` tables key by the human IDENTIFIER (`"MT-12"`)
  because that's the only thing known at boot before the first tracker fetch. `recovery.rs`'s boot
  path is the seam where this mismatch is bridged — read its module doc before changing either
  table's key.
- `RunningEntry::cancel` defaults to an **unarmed** `CancelSignal` (Go leaves `cancel` nil for
  test/legacy entries); don't assume every `RunningEntry` in a test fixture can be cancelled.
- The control channel is `tokio::sync::mpsc::unbounded`, not Go's buffered-256 channel — a
  deliberate deviation (the worker's per-event forwarding closure is sync and can't await a
  bounded send). Don't "fix" this back to a bounded channel without re-reading `loop.rs`'s module
  doc.
- `handoff.rs` (`symphony_handoff` MCP tool → `POST /api/v1/runs/{id}/handoff`) is a capability
  beyond the frozen Go reference — don't treat its absence from the Go source as a porting gap, it's
  intentional. But as of this writing **no README.md Divergences entry documents it**: root
  CLAUDE.md requires every intentional deviation to get a Divergences entry, and grepping
  `README.md` for TRA-242/handoff/review-handoff turns up nothing. Don't assume this is already
  covered elsewhere — if you touch `handoff.rs`, add the missing entry (or confirm one now exists
  and update this note).
