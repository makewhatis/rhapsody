//! orchestrator — parity port of the core state + constructor from Go
//! `internal/orchestrator/orchestrator.go`.
//!
//! The [`Orchestrator`] owns all scheduling state; only the control task mutates it (Go: "only the
//! Run goroutine mutates it"). The Go orchestrator is loop-confined — every tracker write and
//! state-map mutation happens on the one control goroutine; the Rust design keeps that discipline
//! as a single owning task with channels in / channels out (no `Mutex` webs). O1 delivers the owned
//! state + constructor; the control loop, workers, persistence, and telemetry that read and mutate
//! this state land in later P5 tickets (O2–O7), each extending this struct and the entry types with
//! the fields its behavior needs.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicI64};
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::JoinHandle;

use chrono::{DateTime, Utc};
use rhapsody_core::{Issue, normalize_state};
use rhapsody_store::{self as store, Store};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::watch;

use crate::control_loop::{CancelSignal, CancelWait, DEFAULT_RETENTION_DAYS, Event, WaitGroup};
use crate::effective::Effective;
use crate::warnings::WarningsState;

/// The worker-spawn seam signature (Go `spawn func(wctx, iss, attempt *int, re *runningEntry)`). The
/// Go `context` is dropped — the Rust worker is a task whose abort handle O7 owns, so cancellation is
/// a dropped future (mirroring `worker.rs`); [`Orchestrator::dispatch_issue`] passes the freshly-built
/// running entry so the spawn can stamp the worker's project deps / run id. See [`Orchestrator::spawn`].
pub(crate) type SpawnFn = Box<dyn Fn(&Issue, Option<i64>, &RunningEntry) + Send + Sync>;

/// One observed agent event, for the per-issue `recent_events` ring surfaced in the API. Mirrors Go
/// `EventRecord`.
#[derive(Debug, Clone, PartialEq)]
pub struct EventRecord {
    pub at: DateTime<Utc>,
    pub event: String,
    pub message: String,
}

/// Tracks a live worker (upstream §4.1.6, §4.1.8). Mirrors Go `runningEntry`.
///
/// The worker-machinery fields Go carries here — the cancellation handle (`cancel`), the per-run
/// operator-message mailbox (`mailbox`, INF-250), and the dispatch span context
/// (`dispatchSpanContext`) — need types that arrive with the worker (O3) and telemetry (P6); they
/// are added by those tickets. Every field below is control-task-owned.
#[derive(Debug, Clone, PartialEq)]
pub struct RunningEntry {
    pub issue: rhapsody_core::Issue,
    pub started_at: DateTime<Utc>,
    pub retry_attempt: i64,

    /// The worker's cancellation trigger (Go `runningEntry.cancel context.CancelFunc`), armed by
    /// [`dispatch_issue`](Orchestrator::dispatch_issue) before the spawn and fired by `terminate` /
    /// `shutdown` to kill the run (the SIGKILL path in production). The [`empty`](RunningEntry::empty)
    /// default is UNARMED (Go leaves `cancel` nil for test / legacy entries that never spawned a
    /// cancelable worker); its trivial `PartialEq`/`Debug` keep [`RunningEntry`]'s derives.
    pub(crate) cancel: CancelSignal,

    /// Owning project (Phase 2). Empty in the legacy single-project / test-injected path; stamped at
    /// dispatch from the resolved project. `project_repo` is carried for Phase 3 (worktrees).
    /// `project_group` is the stable per-project key shared by every slug of the same multi-slug
    /// project, so the per-project concurrency cap is counted across the whole group (falls back to
    /// `project_slug` for the legacy single-project path).
    pub project_slug: String,
    pub project_group: String,
    pub project_repo: String,
    /// The effective claude model for this dispatch, stamped from the owning project (or top-level).
    /// Carried so exit/terminate can label the run + token metrics by model without re-resolving the
    /// project. Empty when unset.
    pub model: String,

    /// The graphite-mode predecessor stacking hint passed to the worker on its first turn. Empty for
    /// every dispatch except a graphite-mode auto-promote (INF-318).
    pub stack_context: String,

    /// The rendered capability-instruction text prepended to the worker's first-turn prompt (BO-12).
    /// Computed at dispatch from the owning project's default capabilities ∪ the ticket's `rhapsody:*`
    /// labels, rendered through the capabilities registry. Empty when nothing resolves.
    pub capabilities_section: String,

    /// The Rhapsody Teams identity this run was routed to (STUDIO-643, T3a; design record
    /// `~/.rhapsody/docs/STUDIO-572-rhapsody-teams.md`, §3.1). Empty when Teams is off or when
    /// nobody was routed — in which case the run is byte-identical to a Teams-off dispatch.
    ///
    /// **In memory only.** The `runs` table is frozen: this is never persisted as a column, and the
    /// decision's per-run record is the `teams.route` / `teams.unrouted` *events* row instead
    /// (§3.4). Its one live consumer is
    /// [`LoadSnapshot::from_running`](crate::teams::LoadSnapshot::from_running), which counts live
    /// runs per identity so the router's tie-break can prefer the least busy teammate.
    pub identity: String,

    /// The rendered teammate section prepended to the worker's first-turn prompt (STUDIO-643) — the
    /// identity header plus the identity's resolved profile text. Empty when Teams is off, when
    /// nobody was routed, or when the profile failed to resolve; the `if !x.is_empty()` guard in
    /// `build_turn_prompt` then skips it and the prompt is byte-identical (§2.4 row 5), exactly as
    /// `capabilities_section` behaves.
    pub teammate_section: String,

    /// The `latest_summon_at` of the most recent mid-run summons already delivered to this run's
    /// mailbox (INF-448). The poll-side router delivers a summons only when it is strictly after BOTH
    /// `started_at` and this watermark, then advances it — so a stable summons is injected at most
    /// once. Zero value = nothing delivered yet.
    pub last_delivered_summon_at: DateTime<Utc>,

    pub thread_id: String,
    pub session_id: String,
    pub turn_count: i64,
    /// The last observed agent event type (one of `rhapsody_agent`'s `EVENT_*` values; Go's
    /// `agent.EventType`).
    pub last_event: String,
    pub last_message: String,
    pub last_event_at: DateTime<Utc>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub total_tokens: i64,

    /// Live in-flight (current-turn) token estimate, set LAST-WINS from per-message assistant usage
    /// events while a turn is running (assistant `message.usage` is cumulative-within-the-turn, so
    /// the latest snapshot IS the current-turn total, NOT a delta to sum). RESET to 0 when the
    /// authoritative result event commits the per-turn total into `input_tokens`/etc., so the
    /// committed totals stay authoritative (no double-count) while `cur_*` gives a live mid-turn
    /// estimate for the dashboard. `cur_total_tokens` is billed-inclusive (upstream §13.5).
    pub cur_input_tokens: i64,
    pub cur_output_tokens: i64,
    pub cur_total_tokens: i64,

    /// Phase 4 durable-history bookkeeping. `run_id` is the `StartRun`-assigned run row id (0 ⇒
    /// store disabled / `StartRun` failed; every persist helper no-ops on 0). `event_seq` is the
    /// monotonic per-run event sequence for the history events table.
    pub run_id: i64,
    pub event_seq: i64,

    /// The CONCRETE per-run transcript file (the timestamped `*.jsonl`, NOT the `latest.jsonl`
    /// alias) reported by the worker once it opens the transcript. Threaded onto
    /// `runs.transcript_path` so a past run row resolves to its OWN transcript. Empty until the
    /// worker opens a transcript (or when local logging is disabled).
    pub transcript_path: String,

    /// Liveness probe state (CPU-based stall detection). `pgid` is the process-group id (== agent
    /// pid; 0 until the first PID event). `last_cpu_ticks` is the last observed cumulative group CPU;
    /// `cpu_sampled` whether it holds a prior sample; `last_cpu_active_at` the last time group CPU
    /// changed (or last assume-alive).
    pub pgid: i32,
    pub last_cpu_ticks: u64,
    pub cpu_sampled: bool,
    pub last_cpu_active_at: DateTime<Utc>,

    pub recent_events: Vec<EventRecord>,
}

impl RunningEntry {
    /// A running entry for `issue` with every scheduling/telemetry field at its zero value (times at
    /// the [`zero_time`] epoch, per the port's "never observed" convention). [`Orchestrator::dispatch_issue`]
    /// builds one and then stamps the dispatch fields (`started_at`, `retry_attempt`, the owning-project
    /// slug/group/repo, `model`, `stack_context`); `run_id` is stamped afterwards by `persist_start_run`.
    /// The worker-machinery fields Go sets in `dispatchIssue` (`cancel`, `mailbox`) are O7's.
    pub(crate) fn empty(issue: Issue) -> RunningEntry {
        RunningEntry {
            issue,
            started_at: zero_time(),
            retry_attempt: 0,
            cancel: CancelSignal::default(),
            project_slug: String::new(),
            project_group: String::new(),
            project_repo: String::new(),
            model: String::new(),
            stack_context: String::new(),
            capabilities_section: String::new(),
            identity: String::new(),
            teammate_section: String::new(),
            last_delivered_summon_at: zero_time(),
            thread_id: String::new(),
            session_id: String::new(),
            turn_count: 0,
            last_event: String::new(),
            last_message: String::new(),
            last_event_at: zero_time(),
            input_tokens: 0,
            output_tokens: 0,
            total_tokens: 0,
            cur_input_tokens: 0,
            cur_output_tokens: 0,
            cur_total_tokens: 0,
            run_id: 0,
            event_seq: 0,
            transcript_path: String::new(),
            pgid: 0,
            last_cpu_ticks: 0,
            cpu_sampled: false,
            last_cpu_active_at: zero_time(),
            recent_events: Vec::new(),
        }
    }
}

/// Tracks a scheduled retry (upstream §4.1.7). Mirrors Go `retryEntry`.
///
/// Go also carries the live `*time.Timer` here; the Rust retry timer arrives with the retry queue
/// (O5) and is added then.
#[derive(Debug, Clone, PartialEq)]
pub struct RetryEntry {
    pub issue_id: String,
    pub identifier: String,
    pub attempt: i64,
    pub due_at: DateTime<Utc>,
    pub err: String,

    /// Owning project (Phase 2), so the retry re-fetches from the right slug and re-checks the right
    /// per-project caps. Empty in the legacy single-project path.
    pub project_slug: String,
    pub project_repo: String,

    /// The last-known full issue for a LIVE retry (carried from the running entry at schedule time).
    /// It lets `on_retry` continue already-in-flight work that has dropped OUT of the candidate set
    /// without being terminal — filter narrowing gates fresh dispatch only, not the lifecycle of
    /// work in flight. Zero-value (`id == ""`) for boot-recovered entries and legacy/test paths.
    pub issue: rhapsody_core::Issue,

    /// Phase 4 recovery bookkeeping. `due_at_ms` is the wall-clock unix-ms persisted due time (for
    /// `SaveRetry` + boot re-arm). `recovered` marks a boot-recovered entry keyed by IDENTIFIER
    /// (`issue_id == ""`); `on_retry` matches its candidate by `.identifier` and re-keys to the real
    /// opaque id before dispatch (Phase 4 §3.7).
    pub due_at_ms: i64,
    pub recovered: bool,
}

/// Aggregates token + runtime accounting (upstream §4.1.8, §13.5). Mirrors Go `Totals`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Totals {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub total_tokens: i64,
    pub seconds_running: f64,
}

/// A graphite-mode predecessor stacking FACT (issue id → {branch, PR}), carried from the
/// auto-promote pass to the next tick's standard dispatch. Storing the raw fact (not a rendered
/// string) means the "STACK ON: …" hint is built with the DISPATCH-time `workspace_mode`, so a
/// config reload between promote and dispatch can't hand the agent a wrong-mode recipe. Mirrors Go
/// `stackHint`. The promote pass that produces it and the dispatch that consumes it land in O6/O2.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StackHint {
    pub branch: String,
    pub pr_number: i64,
}

/// Owns all scheduling state; only the control task mutates it. Mirrors Go `Orchestrator`.
///
/// O1 roots the state the loop schedules against — the running / claimed / retrying / completed
/// maps, the pending-stack map, token totals, the injectable clock, and the resolved [`Effective`]
/// config. Later P5 tickets add their state to this struct: the control-event channel + drain
/// bookkeeping (O7), the durable store + async event writer (O4), telemetry handles (P6), the
/// per-project warning maps + read-only tracker surfaces (O6/O4), the worker-spawn seam (O3), and
/// the GitHub-summons source (O6).
pub struct Orchestrator {
    /// The path of the WORKFLOW.md this orchestrator loads and watches (the HTTP config endpoint
    /// reads and rewrites it; the watcher then hot-reloads).
    pub workflow_path: String,
    /// Injectable clock (Go `now func() time.Time`), defaulted to [`Utc::now`]. The whole control
    /// loop reads time through this so tests can pin it. Boxed (not a bare `fn` pointer) to match
    /// Go's closure-capable `func() time.Time`: later tickets' tests inject a *fixed captured*
    /// instant (`o.now = Box::new(move || fixed)`), which a non-capturing `fn` pointer could not
    /// represent. `Send + Sync` so the owning control task (O7) stays `Send`.
    pub now: Box<dyn Fn() -> DateTime<Utc> + Send + Sync>,

    /// The worker-spawn TEST seam (Go's injectable `spawn func(...)`). `None` = production: dispatch
    /// launches the real worker via [`spawn_worker`](Orchestrator::spawn_worker) (a tokio task driving
    /// `run_agent_attempt`, forwarding its per-turn events + exit back onto the control channel). O5's
    /// retry/recovery tests + O7's loop/stop tests inject a recorder here (`Some(..)`), exactly as the
    /// Go tests set `o.spawn` — the seam can't be a closure over `&self` (the real spawn needs
    /// `o.eff`/`o.events`/`o.wg`), so the production default is `None` rather than a self-capturing
    /// closure. [`dispatch_issue`](Orchestrator::dispatch_issue) hands the just-built running entry to
    /// the injected seam, or to the real spawn when none is set.
    pub(crate) spawn: Option<SpawnFn>,

    /// The live config + built deps the loop schedules against; `None` until `Run`/reload builds it
    /// (O7). Mirrors Go `eff *effective` (nil until the first `reloadFromDisk`). Rebuilt and swapped
    /// atomically on reload — the swap is the control task's, so no lock is needed.
    pub eff: Option<Effective>,

    /// The agent-capabilities registry loaded from `~/.rhapsody/capabilities.yaml` at daemon startup
    /// (BO-12). `None` when the daemon runs without a durable on-disk home (tests / storage disabled) or
    /// when the file fails to load — in which case capability rendering is a no-op, never a hard failure.
    /// [`dispatch_issue`](Orchestrator::dispatch_issue) renders the per-run capability section through it.
    pub capabilities_registry: Option<Vec<rhapsody_config::capabilities::CapabilityDef>>,

    /// The Rhapsody Teams config loaded from `~/.rhapsody/teams.yaml` at daemon startup (STUDIO-639,
    /// design record `~/.rhapsody/docs/STUDIO-572-rhapsody-teams.md`). `None` when the daemon runs
    /// without a durable on-disk home to anchor the file to (tests / storage disabled); otherwise the
    /// loaded config, which is [`Teams::disabled`](rhapsody_config::teams::Teams::disabled) when the
    /// file is absent (the shipped state, §2.1), malformed, or invalid.
    ///
    /// **Nothing reads this yet.** T1 carries the toggle and the roster inert; routing (T3a), triage
    /// (T3b) and memory (T4) are the slices that consume it. Any future consumer must gate on
    /// `.enabled` — `Some(..)` only means a file location existed, not that Teams is on.
    pub teams: Option<rhapsody_config::teams::Teams>,

    /// The Rhapsody Teams PROFILES directory (`<runtime home>/teams/profiles/`, STUDIO-642),
    /// resolved at daemon startup beside [`teams`](Orchestrator::teams). `None` when the daemon runs
    /// without a durable on-disk home — the same condition that leaves `teams` `None`, so in
    /// production the two are set together.
    ///
    /// Read-only and never created: an absent directory simply means every profile resolves to its
    /// built-in (§4). `dispatch_issue` resolves the routed identity's profile through it (T3a); a
    /// failure logs and drops the section rather than failing the run.
    pub teams_profiles_dir: Option<std::path::PathBuf>,

    /// The Rhapsody Teams **local** memory bank the dispatch path recalls from (STUDIO-645, T4;
    /// design record §5.2). `None` whenever there is nothing to recall: Teams off, no on-disk
    /// runtime home, or `memory.backend` set to anything other than `local`.
    ///
    /// **The concrete [`LocalBank`](rhapsody_config::memory::LocalBank), deliberately never a
    /// `dyn MemoryBackend`.** `dispatch_issue` runs inline on the single control task and is `fn`,
    /// not `async fn`; the trait is async precisely because T8's `hindsight` backend does network
    /// I/O. Holding the concrete local type here makes "zero network I/O on the dispatch path" a
    /// property a reviewer can clear by reading one type name — a remote backend is not merely
    /// forbidden here, it is unrepresentable. When T8 lands, its recall is prefetched off the
    /// dispatch path and hands the SAME plain-data `Fact` slice to the same renderer.
    pub teams_bank: Option<Arc<rhapsody_config::memory::LocalBank>>,

    /// The Rhapsody Teams **prefetched** memory the dispatch path reads when the bank is remote
    /// (STUDIO-660, T8; design record §5, §5.4). `Some` only for `memory.backend: hindsight` with
    /// a roster; `None` for `local`, `none`, and Teams off — so those configurations reach exactly
    /// the code they reached before this field existed.
    ///
    /// It is [`teams_bank`](Orchestrator::teams_bank)'s counterpart, not its replacement, and the
    /// two are mutually exclusive by construction at the composition root: `local` gets a bank and
    /// no cache, `hindsight` gets a cache and no bank. The rule that forces the split is the same
    /// one `teams_bank`'s doc states — the control task may not await — and the cache is how a
    /// remote bank obeys it. All of its network I/O happened earlier, on
    /// [`crate::teamsprefetch`]'s own task; what dispatch does here is
    /// [`PrefetchCache::try_get`](crate::teamsprefetch::PrefetchCache::try_get), a **non-blocking**
    /// map read that cannot wait, cannot fail and does no I/O. A miss, a stale entry or a
    /// contended lock all render no memory section, which is byte-for-byte `backend: none`.
    pub teams_prefetch: Option<Arc<crate::teamsprefetch::PrefetchCache>>,

    /// The seam between this task and the off-loop **triage** task (STUDIO-669; design record
    /// `~/.rhapsody/docs/STUDIO-668-multi-team.md` §A.3.2, §A.3.4): the selection gate kicks a
    /// triage cycle through it, and the router reads its pending-assignment map when a label write
    /// has failed.
    ///
    /// **`Some` exactly when a triage task was spawned**, which is what makes §A.3.1's hold safe:
    /// [`teams_awaiting_assignment`](Orchestrator::teams_awaiting_assignment) answers `false`
    /// without a handle, so no boot can hold work for a manager that does not exist.
    pub teams_triage: Option<Arc<crate::triage::TriageHandle>>,

    /// The Rhapsody Teams **room** the dispatch path catches up from (STUDIO-650, T5; design
    /// record §0.5, §0.11.4). `None` whenever there is no room to read: Teams off, or no on-disk
    /// runtime home to anchor `~/.rhapsody/teams/room/` to.
    ///
    /// **The concrete [`LocalRoom`](rhapsody_config::room::LocalRoom)**, for
    /// [`teams_bank`](Orchestrator::teams_bank)'s reason and one more: `RoomLog` is already sync,
    /// so it could not `.await` even as a trait object, and holding the local type additionally
    /// says which backend this is. A relay backend (§0.10's deferred Nostr leg) is unrepresentable
    /// here, which is how "zero network I/O on the dispatch path" stays checkable from a type name.
    pub teams_room: Option<Arc<rhapsody_config::room::LocalRoom>>,

    /// Where each identity's room watermark lives (STUDIO-650, T5; §0.11.4: the identity's own
    /// state, `<banks>/<bank-id>/cursor`, never the parity store). `None` on exactly the same
    /// conditions as [`teams_room`](Orchestrator::teams_room), and set with it.
    ///
    /// Read at turn-1 composition and written back ONLY after a catch-up that actually returned
    /// messages — so a room that is absent or quiet writes nothing and creates nothing.
    pub teams_cursors: Option<Arc<rhapsody_config::room::Cursors>>,

    /// Ticket identifier → its state, as last seen by the poller **in memory** (STUDIO-645, T4).
    /// Refreshed each tick from the candidate fetch, before dispatch, so a recall rendered during
    /// that tick re-grounds against what the daemon already knows.
    ///
    /// This map is the WHOLE of §5.2's re-grounding, by design: §0.11.3 corrected the original
    /// "free map lookup" pricing — a ticket now Done is never in the candidate fetch (active ∪
    /// review only), so it simply is not here and the fact renders flagged `(state not
    /// re-verified)`. The network fallback that would resolve it is deferred to an off-loop
    /// improvement; a tracker call at dispatch is exactly the head-of-line stall the design review
    /// forbade.
    pub issue_states: HashMap<String, String>,

    /// Per-identity load, as last seen by the poller **in memory** (STUDIO-659, T7; §0.11.1's
    /// load = open tickets carrying `rhapsody:@x`). Refreshed each tick from the candidate fetch by
    /// [`record_quorum_state`](Orchestrator::record_quorum_state), and read by the review quorum's
    /// reviewer selection so a handoff never costs a tracker round-trip on the control task.
    ///
    /// Empty and never written when the quorum is off, which is the whole of "zero behaviour change
    /// with `quorum.enabled: false`" on the control-loop side.
    pub(crate) quorum_load: HashMap<String, i64>,

    /// Tracker issue id → what the last candidate sweep learned about it for quorum purposes: its
    /// open linked PR and whether it already carries the `rhapsody:quorum-requested` marker
    /// (STUDIO-659, T7). Refreshed and gated exactly like
    /// [`quorum_load`](Orchestrator::quorum_load).
    pub(crate) quorum_facts: HashMap<String, crate::quorum::QuorumFacts>,

    /// The off-loop review-quorum task's inbox (STUDIO-659, T7). `None` whenever the quorum is off
    /// or no task was spawned, in which case a handoff sends nothing — the fan-out is
    /// unrepresentable rather than merely skipped.
    ///
    /// Cloned into every [`ControlHandle`](crate::stop::ControlHandle), which is what actually
    /// sends: the request is built ON the loop (`plan_quorum`) but only dispatched AFTER the
    /// off-loop review-state move succeeds, so a handoff whose Linear write failed never fans out.
    pub(crate) quorum_tx: Option<tokio::sync::mpsc::UnboundedSender<crate::quorum::QuorumRequest>>,

    /// The **off-loop** Teams memory runtime (STUDIO-645, T4): the loaded config, the constructed
    /// backend, and the live run → identity binding a `teams_retain` is stamped from. Shared by
    /// `Arc` with the daemon's HTTP layer.
    ///
    /// This is the fourth sanctioned off-loop seam (see `crates/orchestrator/CLAUDE.md`), and it
    /// exists because §5.1 requires a retain to be "best-effort, never fatal, **never blocking the
    /// control task**". The control task's only interaction with it is two `HashMap` operations —
    /// [`bind_run`](crate::teamsmemory::TeamsMemory::bind_run) at dispatch and
    /// [`release_run`](crate::teamsmemory::TeamsMemory::release_run) at run exit — with no I/O and
    /// no lock held across an `.await`. `None` when the daemon has no Teams runtime at all.
    pub teams_memory: Option<Arc<crate::teamsmemory::TeamsMemory>>,

    /// Live workers, keyed by opaque issue id.
    pub running: HashMap<String, RunningEntry>,
    /// Issue ids currently claimed (dispatched or in a claim election), a set.
    pub claimed: HashSet<String>,
    /// Scheduled retries, keyed by opaque issue id (or identifier for boot-recovered entries).
    pub retry_attempts: HashMap<String, RetryEntry>,
    /// Issue ids whose work has completed this process lifetime, a set.
    pub completed: HashSet<String>,
    /// Graphite-mode stacking facts carried from the auto-promote pass to the next tick's dispatch
    /// (written by `promote_unblocked`, consumed by `dispatch_issue`, both on the control task).
    /// INF-318 / INF-418.
    pub pending_stack: HashMap<String, StackHint>,
    /// Aggregate token + runtime accounting.
    pub totals: Totals,

    /// Per-process daemon identity embedded in pool-mode claim markers (`daemon=<viewerID>/<uuid>`,
    /// alongside the API-key viewer id). It is the audit/identity token in a claim comment; the
    /// claim election ([`crate::claim`]) decides the winner by the immutable comment `createdAt` and
    /// tie-breaks on comment id, so `daemon_id` is NOT load-bearing for election correctness. O1's
    /// constructor deferred it (the claim election is its sole consumer); O2 introduces it. Mirrors
    /// Go `daemonID`. INF-477.
    pub daemon_id: String,

    /// The durable history + recovery store (NEVER an absent handle: defaulted to [`store::Noop`] in
    /// [`Orchestrator::new`] and replaced with a real store by [`Orchestrator::set_store`] / the Run
    /// bootstrap), so every store call site is guard-free. O2 is its first consumer — the
    /// summons-suppression read (`last_run_started_at`) queries `issue_history`; the write-side run
    /// lifecycle lands in O4. Held behind [`Arc`] so the future off-loop HTTP read path (P6) can
    /// share it. Mirrors Go `pstore` (Go additionally guards it with `storeMu` for those off-loop
    /// reads, which arrive in P6; O2's access is control-task-confined, so it needs no lock).
    pub store: Arc<dyn Store + Send + Sync>,

    /// Account-level tracker plus resolved key backing the read-only Linear surfaces (the Settings
    /// identity endpoint and the add-agent projects picker, INF-224), which the P6 HTTP path serves
    /// OFF the control loop. Unlike the loop-confined scheduling state, these are read concurrently by
    /// HTTP tasks while the reload path ([`Orchestrator::set_reads_target`], O7) swaps them, so they
    /// sit behind an [`RwLock`] (Go `readsMu` guarding `readsTracker`/`readsAPIKey`). Empty until the
    /// first config load — the reads helpers surface [`crate::reads::ReadsError::ConfigNotLoaded`]
    /// until then. Held behind [`Arc`] (F1) so the daemon's off-loop [`crate::ControlHandle`] shares
    /// the SAME live target after the orchestrator moves into the control-loop task — the reload path
    /// still updates the one shared cell, so the HTTP reads reflect hot-reloads.
    pub(crate) reads: Arc<RwLock<crate::reads::ReadsTarget>>,

    /// The async event-writer feed (Phase 4 §3.1): coarse per-event history rows are handed to the
    /// batched writer thread through this bounded channel. A full buffer SHEDS the event (counted in
    /// [`Orchestrator::dropped`]) rather than block the control task — the raw `.jsonl` transcript on
    /// disk stays the lossless record. `None` once [`stop_event_writer`](Orchestrator::stop_event_writer)
    /// drops the sender to close the channel. Mirrors Go `storeEvents` (a buffered `chan storeEventWrite`).
    pub(crate) store_events_tx: Option<SyncSender<crate::persist::StoreEventWrite>>,
    /// The receive end, held until [`start_event_writer`](Orchestrator::start_event_writer) hands it
    /// to the writer thread. Wrapped in a `Mutex` solely so the `Orchestrator` stays `Sync` for the
    /// off-loop HTTP path (a bare [`Receiver`] is `!Sync`); it is taken exactly once, on the control
    /// task. Go keeps no separate handle — its writer goroutine ranges the channel directly.
    pub(crate) store_events_rx: Mutex<Option<Receiver<crate::persist::StoreEventWrite>>>,
    /// The event-writer thread handle, set by [`start_event_writer`](Orchestrator::start_event_writer)
    /// and joined by [`stop_event_writer`](Orchestrator::stop_event_writer). Go coordinates the same
    /// lifecycle with `writerWG` + `writerOnce`.
    pub(crate) writer_handle: Option<JoinHandle<()>>,
    /// Count of history events shed because [`Orchestrator::store_events_tx`] was full when the control
    /// task tried to enqueue them (the enqueue never blocks the loop). Mirrors Go `dropped`.
    pub(crate) dropped: AtomicI64,

    /// Per-run operator-message mailboxes (INF-250), keyed by opaque issue id (one live run per
    /// issue). Created at dispatch ([`dispatch_issue`](Orchestrator::dispatch_issue)) and dropped at
    /// run end ([`persist_end_run`](Orchestrator::persist_end_run)). Go carries the channel as a
    /// `runningEntry` field; a Rust [`mpsc`](tokio::sync::mpsc) split cannot be a `Clone` entry field,
    /// so the mailbox lives here as a side map (see [`crate::message`]). The [`Mutex`] keeps the
    /// `Orchestrator` `Sync` for the off-loop HTTP path (a bare `mpsc::Receiver` is `!Sync`); it is
    /// only touched on the control task. O7's real spawn takes each mailbox's receiver.
    pub(crate) mailboxes: Mutex<HashMap<String, crate::message::Mailbox>>,

    // --- O7: the control loop + its runtime handles (Go `orchestrator.go`'s `events` / `tickTimer` /
    //     `ctx` / `wg`, plus the reload-owned ghSource / retention / warning state). ---
    /// The control-event feed. Workers, timers, the config watcher, and the off-loop
    /// [`ControlHandle`](crate::stop::ControlHandle) SEND here; the single owning control task
    /// ([`run_loaded`](Orchestrator::run_loaded)) receives + dispatches. Unbounded (Go: buffered 256)
    /// so the worker's synchronous per-event forwarding closure can enqueue without blocking/awaiting
    /// and no control event is ever shed. Mirrors Go `events chan event`.
    pub(crate) events: UnboundedSender<Event>,
    /// The receive end, taken by [`run_loaded`](Orchestrator::run_loaded) exactly once. Behind a
    /// `Mutex` solely so the `Orchestrator` stays `Sync` (a bare receiver is `!Sync`); the loop takes
    /// it on the control task. Go's loop ranges the channel directly with no separate handle.
    pub(crate) events_rx: Mutex<Option<UnboundedReceiver<Event>>>,
    /// The always-readable published runtime snapshot (STUDIO-551) — the `GET /api/v1/state` source.
    /// The control task republishes it after every event it handles (and mid-tick, right after
    /// `reconcile`); the off-loop [`ControlHandle`](crate::stop::ControlHandle) READS the latest value
    /// without touching the control channel, so `/state` can never be starved behind a network-bound
    /// tick. `None` until the loop publishes its first snapshot, which keeps the pre-loop
    /// "snapshot unavailable" 503 that the `evSnapshot` round-trip used to produce via its timeout.
    pub(crate) snapshot_pub: watch::Sender<Option<Arc<crate::snapshot::Snapshot>>>,
    /// The orchestrator lifetime cancellation (Go `ctx context.Context`), set at the start of the
    /// control loop before the HTTP server accepts requests. `None` until the loop starts (some unit
    /// tests round-trip without it); read off-loop by Stop/Resume reply waits + the warning resolver.
    pub(crate) ctx: Option<CancelWait>,
    /// The armed poll-tick timer task (Go `tickTimer *time.Timer`); aborted + replaced by
    /// [`schedule_tick`](Orchestrator::schedule_tick), stopped on shutdown.
    pub(crate) tick_timer: Option<tokio::task::JoinHandle<()>>,
    /// The armed retry timer tasks keyed by issue id (Go `retryEntry.timer`, held here so
    /// [`RetryEntry`] keeps its derives). `schedule_retry_for` arms one; `clear_retry` / shutdown abort
    /// them. Go's `time.AfterFunc` fires `evRetry`; the Rust task sleeps then sends [`Event::Retry`].
    pub(crate) retry_timers: HashMap<String, tokio::task::JoinHandle<()>>,
    /// The workers-and-resolvers barrier `shutdown` waits on (Go `sync.WaitGroup`). Each spawned worker
    /// + off-loop warning resolver holds a [`WgGuard`](crate::control_loop::WgGuard) for its lifetime.
    pub(crate) wg: WaitGroup,
    /// The GitHub-summons source: `Some` iff the feature is enabled for the legacy config or a resolved
    /// project, else `None` (the poll path stays byte-identical when off — every enrichment site gates
    /// on `o.gh_source.is_some()`). Built once in `Run` and rebuilt on reload from the freshly-swapped
    /// `eff` via [`new_github_summon_source`](Orchestrator::new_github_summon_source). Mirrors Go
    /// `ghSource` (O6's `ghsummons::GH` polling source). Control-task-owned.
    pub(crate) gh_source: Option<Box<dyn crate::ghsummons::SummonSource>>,
    /// The effective `storage.retention_days` mirrored as an atomic so the daemon's prune scheduler
    /// (P6) reads it without racing the control task's reload (default 30 until the first reload).
    /// Mirrors Go `retentionDays`.
    pub(crate) retention_days: Arc<AtomicI64>,
    /// Flips true the first time [`reload_from_disk`](Orchestrator::reload_from_disk) stores the
    /// effective retention_days, so the prune scheduler can skip the startup worktree GC while
    /// `current_retention_days` still returns the `New` default. Mirrors Go `retentionLoaded`.
    pub(crate) retention_loaded: Arc<AtomicBool>,
    /// Per-project-group warning strings surfaced on the project status (INF-277 / INF-279), resolved
    /// OFF the control task by the reload/worker-exit resolver. `Arc` so the off-loop resolver tasks
    /// share it while the control task reads it in `project_statuses`. Mirrors Go's `warningsMu` +
    /// `projectWarnings` / `projectFileWarnings` + the two generation counters.
    pub(crate) warnings: Arc<WarningsState>,
    /// The TTL cache of ticket lifecycle states behind `GET /api/v1/history/issues`'s lifecycle
    /// fields (STUDIO-702). Purely a read-path memo — the control task never reads or writes it —
    /// held here rather than on the handle alone so every clone of [`crate::ControlHandle`] shares
    /// ONE freshness window. See `lifecycle.rs`.
    pub(crate) lifecycle: Arc<crate::lifecycle::LifecycleCache>,
    /// Whether a store was injected via [`set_store`](Orchestrator::set_store) (Go `storeInjected`),
    /// short-circuiting `Run`'s disk-open path so tests / callers own the store lifecycle.
    pub(crate) store_injected: bool,

    // --- BO-59: dispatch credential-liveness preflight (Rhapsody-only; see `preflight.rs`). ---
    /// The injectable credential-liveness probe seam. `None` disables the credential preflight →
    /// dispatch is byte-identical to the pre-feature behavior (the default for tests and any build that
    /// does not install one). Production installs a [`ClaudeCredentialProbe`](crate::preflight::ClaudeCredentialProbe)
    /// via [`set_credential_probe`](Orchestrator::set_credential_probe).
    pub(crate) cred_probe: Option<Arc<dyn crate::preflight::CredentialProbe>>,
    /// The cached credential-probe verdict for the dispatch preflight. Mutated only by on_tick's
    /// `credential_preflight` on the single control task, so it needs no lock. `None` until the first
    /// probe.
    pub(crate) probe_cache: Option<crate::preflight::ProbeCache>,
    /// The per-probe timeout bound: a probe that does not answer within this is treated as "cannot
    /// verify → skip dispatch" (fail closed), well under the poll interval so a hang never wedges the
    /// tick. A field (not a const) so tests can shrink it; defaulted to
    /// [`PROBE_TIMEOUT`](crate::preflight::PROBE_TIMEOUT).
    pub(crate) probe_timeout: std::time::Duration,
}

/// Returns an OS-seeded random 64-bit value without a `rand`/`getrandom`/`uuid` dependency: each
/// [`RandomState::new`](std::collections::hash_map::RandomState) draws fresh keys from the standard
/// library's OS-seeded thread-local RNG, so finishing a hasher over the empty input yields an
/// independent value per call. Used for the daemon id and the claim settle jitter — neither is
/// load-bearing for correctness (see [`new_daemon_id`] / `claim::jittered_settle`), so
/// non-cryptographic entropy is sufficient.
pub(crate) fn random_u64() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish()
}

/// Generates a per-process daemon identity for pool-mode claim markers. Mirrors Go
/// `uuid.NewString()`: the value is opaque and NOT load-bearing for election correctness (the claim
/// election orders by the immutable comment `createdAt` and tie-breaks on comment id), so it needs
/// only per-process uniqueness. Rendered as a 128-bit hex id from two [`random_u64`] draws, so two
/// `Orchestrator::new()` calls get distinct ids (the contention test relies on it) and two daemon
/// processes get distinct ids (the per-process RNG seed is OS-random).
fn new_daemon_id() -> String {
    format!("{:016x}{:016x}", random_u64(), random_u64())
}

impl Orchestrator {
    /// Builds an [`Orchestrator`] for the given workflow path. The deps are loaded at `Run` time (so
    /// startup validation failures surface there), leaving [`Orchestrator::eff`] `None` here.
    /// Mirrors Go `New`.
    ///
    /// Deviation from Go: Go's `New(workflowPath, logger *slog.Logger)` threads a `slog` logger; the
    /// Rust crate emits diagnostics via `tracing` (as the sibling crates do), so the logger
    /// parameter is dropped. The pool-mode daemon identity (Go `daemonID = uuid.NewString()`) IS set
    /// here now (O2, its introducing ticket); the durable store defaults to [`store::Noop`] until a
    /// real store is injected (Go defaults `pstore` to `store.Noop()`), keeping every store call site
    /// guard-free.
    pub fn new(workflow_path: impl Into<String>) -> Orchestrator {
        // The event feed is created up front (Go makes `storeEvents` in `New`) so `enqueue_event`
        // works before the writer starts — a full/unwatched buffer just sheds events; the writer
        // thread is spawned later by `start_event_writer`.
        let (store_events_tx, store_events_rx) =
            std::sync::mpsc::sync_channel(crate::persist::EVENT_BUF_CAP);
        // The control-event channel is created up front (Go makes `events` in `New`) so the off-loop
        // Stop/Resume handle + timers can send before the loop takes the receiver.
        let (events, events_rx) = tokio::sync::mpsc::unbounded_channel();
        Orchestrator {
            workflow_path: workflow_path.into(),
            now: Box::new(Utc::now),
            // No test seam by default → dispatch launches the real `spawn_worker` (Go `New` sets
            // `o.spawn = o.spawnWorker`). O5's handler tests + O7's loop tests inject a recorder.
            spawn: None,
            eff: None,
            capabilities_registry: None,
            teams: None,
            teams_profiles_dir: None,
            teams_bank: None,
            teams_prefetch: None,
            teams_triage: None,
            teams_room: None,
            teams_cursors: None,
            issue_states: HashMap::new(),
            quorum_load: HashMap::new(),
            quorum_facts: HashMap::new(),
            quorum_tx: None,
            teams_memory: None,
            running: HashMap::new(),
            claimed: HashSet::new(),
            retry_attempts: HashMap::new(),
            completed: HashSet::new(),
            pending_stack: HashMap::new(),
            totals: Totals::default(),
            daemon_id: new_daemon_id(),
            store: Arc::new(store::Noop),
            reads: Arc::new(RwLock::new(crate::reads::ReadsTarget::default())),
            store_events_tx: Some(store_events_tx),
            store_events_rx: Mutex::new(Some(store_events_rx)),
            writer_handle: None,
            dropped: AtomicI64::new(0),
            mailboxes: Mutex::new(HashMap::new()),
            events,
            events_rx: Mutex::new(Some(events_rx)),
            // Nothing is published until the control loop starts (see the field docs).
            snapshot_pub: watch::channel(None).0,
            ctx: None,
            tick_timer: None,
            retry_timers: HashMap::new(),
            wg: WaitGroup::new(),
            gh_source: None,
            retention_days: Arc::new(AtomicI64::new(DEFAULT_RETENTION_DAYS)),
            retention_loaded: Arc::new(AtomicBool::new(false)),
            warnings: Arc::new(WarningsState::default()),
            lifecycle: Arc::new(crate::lifecycle::LifecycleCache::default()),
            store_injected: false,
            // BO-59: no credential probe by default → the preflight is a no-op and dispatch is
            // byte-identical to the pre-feature behavior. The daemon installs the real probe at startup.
            cred_probe: None,
            probe_cache: None,
            probe_timeout: crate::preflight::PROBE_TIMEOUT,
        }
    }

    /// Returns the path of the WORKFLOW.md this orchestrator loads and watches. Mirrors Go
    /// `WorkflowPath`.
    pub fn workflow_path(&self) -> &str {
        &self.workflow_path
    }

    /// Returns the durable history + recovery store (never absent; [`store::Noop`] when disabled).
    /// Mirrors Go `Store`. O2's summons-suppression read (`last_run_started_at`) goes through it, as
    /// will O4's run-lifecycle writes.
    pub fn store(&self) -> &dyn Store {
        self.store.as_ref()
    }

    /// Injects a store handle, bypassing the Run-time open path — intended for tests (an in-memory
    /// store) and any caller that owns the store lifecycle. Marks the store as injected so `Run` skips
    /// the disk-open path. Mirrors Go `SetStore` (whose `nil → Noop` guard is unrepresentable here,
    /// since an [`Arc`] is never null).
    pub fn set_store(&mut self, st: Arc<dyn Store + Send + Sync>) {
        self.store = st;
        self.store_injected = true;
    }

    /// Installs the orchestrator's lifetime cancellation BEFORE [`Run`](Orchestrator::run) sets it, so
    /// a [`ControlHandle`](crate::ControlHandle) snapshotted via [`control`](Orchestrator::control)
    /// beforehand carries the REAL ctx (not the never-cancelling default). The daemon calls this with
    /// the same ctx it later passes to `Run`, so its off-loop stop/resume/message reply-waits are
    /// bounded by the lifetime ctx (Go's `o.ctx.Done()`), not only the control-channel teardown. `Run`
    /// re-sets the same ctx idempotently.
    pub fn set_ctx(&mut self, ctx: crate::control_loop::CancelWait) {
        self.ctx = Some(ctx);
    }

    /// The effective `storage.retention_days` (default 30 until the first reload). Read by the daemon's
    /// prune scheduler (P6) each cycle without racing the control task's reload. Mirrors Go
    /// `CurrentRetentionDays`.
    pub fn current_retention_days(&self) -> i64 {
        self.retention_days
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Whether [`reload_from_disk`](Orchestrator::reload_from_disk) has stored the effective
    /// retention_days at least once. The prune scheduler reads it to skip the startup worktree GC while
    /// `current_retention_days` would still return the `New` default. Mirrors Go `RetentionLoaded`.
    pub fn retention_loaded(&self) -> bool {
        self.retention_loaded
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Returns the set of currently-running issue ids (Go `runningIDSet`). The selection pass seeds
    /// its per-tick reservation set from this so one tick cannot re-dispatch an in-flight issue.
    pub(crate) fn running_id_set(&self) -> HashSet<String> {
        self.running.keys().cloned().collect()
    }

    /// Counts running issues per NORMALIZED state (Go `runningStateCounts`) — the base the selection
    /// pass measures the per-state cap against.
    pub(crate) fn running_state_counts(&self) -> HashMap<String, i64> {
        let mut counts: HashMap<String, i64> = HashMap::new();
        for re in self.running.values() {
            *counts.entry(normalize_state(&re.issue.state)).or_insert(0) += 1;
        }
        counts
    }

    /// Returns the set of issue IDENTIFIERs a still-pending boot-recovered retry owns (entries in
    /// [`Orchestrator::retry_attempts`] with `recovered == true`, keyed by identifier with an
    /// unresolved opaque id). Mirrors Go `recoveredClaimIdentifiers`: the boot-race guard the
    /// selection pass consults so a poll tick firing in the recovery window never dispatches an issue
    /// out from under a recovered claim (the recovered on-retry would then delete the freshly-started
    /// live run's persisted claim row; Phase 4 §3.7). Empty in the common case, so steady-state /
    /// storage-off dispatch is unaffected.
    pub(crate) fn recovered_claim_identifiers(&self) -> HashSet<String> {
        self.retry_attempts
            .values()
            .filter(|re| re.recovered && !re.identifier.is_empty())
            .map(|re| re.identifier.clone())
            .collect()
    }
}

/// The zero / "never observed" [`DateTime`] sentinel — the Unix epoch. Go uses `time.Time{}`
/// (`IsZero`); the Rust port initializes unset entry times to this epoch, so a field still equal to
/// it means "not yet observed" (the reconcile stall detector reads it that way). Infallible via
/// `from_timestamp_nanos(0)`.
pub(crate) fn zero_time() -> DateTime<Utc> {
    DateTime::from_timestamp_nanos(0)
}

/// Returns the attempt count, treating `None` (Go nil `*int`) as 0. Mirrors Go `normalizeAttempt`.
/// (Go places this in `orchestrator.go`; O5 is its first consumer — [`crate::retry`] /
/// [`Orchestrator::dispatch_issue`].)
pub(crate) fn normalize_attempt(attempt: Option<i64>) -> i64 {
    attempt.unwrap_or(0)
}

/// Returns the candidate matching `id` by opaque tracker ID. Mirrors Go `findByID`.
pub(crate) fn find_by_id<'a>(issues: &'a [Issue], id: &str) -> Option<&'a Issue> {
    issues.iter().find(|i| i.id == id)
}

/// Returns the candidate matching `identifier` by human identifier (e.g. "MT-12"). Boot-recovered
/// retry entries key by identifier (the opaque tracker ID is unknown at restart), so `on_retry`
/// resolves their candidate by identifier (Phase 4 §3.7). Mirrors Go `findByIdentifier`.
pub(crate) fn find_by_identifier<'a>(issues: &'a [Issue], identifier: &str) -> Option<&'a Issue> {
    issues.iter().find(|i| i.identifier == identifier)
}

#[cfg(test)]
mod tests {
    use super::*;

    // O1-authored construction coverage: `New` starts with empty scheduling state, no effective
    // config yet, and a wired clock. Go tests `New` indirectly through every behavioral test (there
    // is no dedicated Go `TestNew`); this pins the initial-state contract the later tickets build on.
    #[test]
    fn new_initializes_empty_state() {
        let o = Orchestrator::new("WORKFLOW.md");
        assert_eq!(o.workflow_path(), "WORKFLOW.md");
        assert!(o.running.is_empty());
        assert!(o.claimed.is_empty());
        assert!(o.retry_attempts.is_empty());
        assert!(o.completed.is_empty());
        assert!(o.pending_stack.is_empty());
        assert!(o.eff.is_none());
        assert_eq!(o.totals, Totals::default());
        // The clock is wired (Go defaults `now` to `time.Now`).
        let _ = (o.now)();
    }
}
