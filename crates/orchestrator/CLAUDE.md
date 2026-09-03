# CLAUDE.md — crates/orchestrator

Parity port of Go `internal/orchestrator` — one Go package split across 26 source files, all
mutating a single `Orchestrator` struct. The Rust port mirrors that file split one-to-one (one file
under `src/` per Go source file, plus the Rust-only additions listed below);
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
- Five exceptions exist today, each `RwLock`/cloneable-handle guarded on purpose — these are the
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
    exit) with no I/O, and the `RwLock` is never held across an `.await`. STUDIO-653's `teams_post`
    is the one Teams surface that uses **both** seams, in this order: the room append happens here,
    off-loop, and only then does `ControlHandle::record_teams_post` round-trip `Event::TeamsPost`
    for the two mirrors that genuinely need loop-owned state (`running`, `mailboxes`,
    `RunningEntry::event_seq`). That round-trip is best-effort by construction — the post is
    already in the log — so a gone loop costs nothing.
  - `lifecycle.rs`'s `LifecycleCache` (`Orchestrator::lifecycle: Arc<LifecycleCache>`,
    STUDIO-702) — two `Mutex`-guarded TTL memos, of each ticket's CURRENT tracker state and of its
    DURABLE assignee (STUDIO-735), read and written ENTIRELY on the HTTP task (they decorate
    `GET /api/v1/history/issues`). The control task never touches them, so it is a seam only in the
    sense that the handle carries it; neither lock is held across the tracker `.await`, and the
    state sets it classifies with are read through the `reads.rs` cell above rather than from
    loop-owned `Effective`. The assignee half also READS the store (the routing event a dispatch
    wrote into the DISPLAYED RUN's ledger — always scoped by `run_id`, never searched ticket-wide,
    because a ticket's runs can disagree about who ran them) — a read-only use of the same
    `Arc<dyn Store>` the handle already carries, never a write.

  If you need to touch orchestrator state from outside the loop task, route through one of these
  five seams; if none fits, that's a real design decision — don't reach for a sixth ad hoc
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

## Module groups

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
- **Rhapsody Teams** (STUDIO-639…653; no Go counterpart — design record
  `~/.rhapsody/docs/STUDIO-572-rhapsody-teams.md`): `teams.rs` is the T3a dispatch router — a pure,
  sync, zero-I/O `route()` called from `dispatch_issue` — plus T4's memory recall, which renders into
  the same turn-1 section. Recall reads **local files only**, and the orchestrator holds the CONCRETE
  `rhapsody_config::memory::LocalBank` for it (`teams_bank`), never a `dyn MemoryBackend`: the trait
  is async because T8's `hindsight` does HTTP, and `dispatch_issue` is `fn`, so a remote backend is
  unrepresentable on that path rather than merely discouraged. Don't "tidy" `teams_bank` into the
  trait object — that type choice IS the no-network-on-dispatch proof. `teamsmemory.rs` is the
  off-loop half (see the fourth seam above), and `teamspost.rs` is T6's write side — the teammate
  wrap (deliberately NOT `message.rs`'s `operator_wrap`), the `teams.message` timeline row, and the
  direct-to-live delivery, all of which reuse the INF-250 mailbox admission via
  `Orchestrator::admit_to_mailbox` rather than a second delivery path. A room post has no dispatch
  power at all (design §0.2) — that is pinned by a test, not just documented. `triage.rs` is the
  T3b **off-loop** triage task: it holds no `Orchestrator`, sends no control event and takes no lock the control task takes,
  which is exactly why the design puts the feature's one model turn there (§0.11.2 — a model call on
  the dispatch path was the STUDIO-551 head-of-line class). Spawned at the composition root
  (`rhapsodyd/run.rs`) beside the prune scheduler, and only for `manager.mode: labels+model`. It is
  NOT a state seam at all: unlike `teamsmemory.rs` it never touches orchestrator state. `quorum.rs`
  is T7's review quorum and is triage's structural sibling: an off-loop task that holds no
  `Orchestrator` and takes no lock the control task takes, fed `QuorumRequest`s — plain owned data —
  over a channel whose sender rides on `ControlHandle` (so it is inside the `stop.rs` seam, not a
  fifth one). The DECISION runs on the loop (`plan_quorum`, from the per-tick `record_quorum_state`
  snapshot: no tracker read, no await); the WRITES run on the task. The send is deliberately gated on
  the review-state move having succeeded, which is why the sender is on the handle rather than
  reached through `Event` — the move happens off-loop, after the control round-trip returned.
  `quorum.enabled` defaults false and spawns no task at all, so a default installation has no delta
  to have. `teamsears.rs` is §0.13's **manager room reader**: it runs INSIDE `triage.rs`'s cycle
  rather than in a task of its own (it needs exactly what a cycle already fetched — the candidate
  set is its validation set, and the manager's model budget is the one it spends), reads only
  `operator` posts, and can take only actions the manager was already authorized to take. Three
  boundaries there are security properties rather than style, and each is enforced by construction:
  ticket keys are extracted VERBATIM from the post and validated against the fetched issues (a model
  may choose among them and may never introduce one — `validate_targets`); the intent map is a
  CLOSED enum, so widening what a room post can cause requires editing it and shows in a diff; and
  the one path that moves post text into a running agent uses `room_operator_wrap`, deliberately NOT
  `message.rs`'s `operator_wrap`, because `from: operator` on a room line is forgeable by any local
  process. Don't add a variant to `Intent`, don't let a key reach an action without going through
  `find_issue`, and don't "unify" the two wraps. **One reviewed exception exists to the first of
  those** (STUDIO-731, design record §3.1/§4): `Intent::Answer` is a fifth variant, and it was
  admissible only because it adds no write power — it resolves to a room reply and returns *before*
  the `find_issue` gate, so it shares no state-mutating path with the four action intents. That
  early return is also why it is not a hole in the second rule: `Answer` is a READ, and its scope
  guard is `TeamScope` inside `teamsknow` applied to every row the gather returned, not the cycle's
  issue set (which a terminal ticket has already fallen out of — the bug the slice fixes). A sixth
  variant needs the same argument made afresh; "there is already an exception" is not one.
  Its live-run relay rides the `stop.rs` seam
  (`Event::TeamsRelay` → `handle_teams_relay`, which reuses `admit_to_mailbox`), so it is not a
  fifth state seam.
  `teamsanswer.rs` is that fifth outcome's own module — the gather, the DATA-fenced facts block the
  room prompt carries, and the vet that refuses model prose naming a ticket the team's own records
  never resolved. Everything it renders is attacker-influenceable (design §9.2: agent memory, room
  JSONL, GitHub comments), so nothing there is "the daemon's own trusted context" — treat any change
  that widens what reaches the block, or that trusts a fact inside it, as a security change. **Be
  exact about what the fencing buys**, because the vet is key-scoped and nothing inspects what a
  sentence MEANS: a plant can never mint an action and can never make the manager name an unresolved
  ticket, but a keyless planted sentence ("the deploy is safe") CAN reach a reply if the turn obeys
  it. The containment for that is `answer_for` rendering `Facts::grounded` under every accepted
  prose, so the host's own records are always beside the sentence — don't remove it, and don't
  restate the old claim that a planted line "is never obeyed". That containment is a claim about
  LAYOUT, so two guards keep the layout honest and both are load-bearing: `quote` marks EVERY line
  of the model's half with `QUOTE_PREFIX` — written by the daemon after the fact, so a forged
  `GROUNDING_LEAD` renders inside the quoted region instead of above the real one (the same rule
  `one_line` applies to a fence-closing backtick run: untrusted text never mints host structure).
  Do not replace that prefix with a check that REFUSES prose containing the lead — that shape was
  tried and is a blocklist, refusing the honest phrasing the prompt's own heading invites while
  admitting a singular *record*, a dropped *From* or a homoglyph. `quote` splits on `['\n', '\r']`
  and not `str::lines` on purpose: a BARE `\r` is not a line break to Rust but is one on every
  surface a reply reaches (`web/src/lib/markdown.ts` rewrites `\r\n?` to `\n` before splitting; a
  terminal returns the carriage over the `> ` already printed), so `lines` left a `\r`-separated
  forgery unquoted at column 0. Assert on renderer lines, never on `str::lines`, or the test cannot
  see the hazard it is named for. And `Answerable::offered` refuses
  an `answer` about a key whose records the facts block never rendered — a SET of keys, not a bool,
  because `Facts::render` fills front-to-back and drops per key: on a multi-key post a prompt-wide
  "the block rendered" is true for every key it dropped, and so is `Facts::resolved`. It is fed by `teamsknow.rs`'s
  accessor through `TriageDeps::knowledge`, which is `None` for a daemon
  with no durable store; that `None` is what keeps every teams-off and `labels`-only prompt
  byte-identical, so don't make it a `Noop` store instead.
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
  intentional. Its README.md Divergences entry ("Daemon-mediated review handoff", TRA-242) was
  finally written by STUDIO-659, which had to touch this file; keep it in step if the handoff's
  behaviour changes. Note that the handoff is now also the review quorum's trigger (`quorum.rs`), so
  a change to when it fires changes when reviews are requested.
