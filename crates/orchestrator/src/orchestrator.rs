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

use chrono::{DateTime, Utc};

use crate::effective::Effective;

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

    /// The live config + built deps the loop schedules against; `None` until `Run`/reload builds it
    /// (O7). Mirrors Go `eff *effective` (nil until the first `reloadFromDisk`). Rebuilt and swapped
    /// atomically on reload — the swap is the control task's, so no lock is needed.
    pub eff: Option<Effective>,

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
}

impl Orchestrator {
    /// Builds an [`Orchestrator`] for the given workflow path. The deps are loaded at `Run` time (so
    /// startup validation failures surface there), leaving [`Orchestrator::eff`] `None` here.
    /// Mirrors Go `New`.
    ///
    /// Deviation from Go: Go's `New(workflowPath, logger *slog.Logger)` threads a `slog` logger; the
    /// Rust crate emits diagnostics via `tracing` (as the sibling crates do), so the logger
    /// parameter is dropped. The pool-mode daemon identity (Go `daemonID = uuid.NewString()`) is not
    /// set here — it is used only by the claim election (O2), which introduces it.
    pub fn new(workflow_path: impl Into<String>) -> Orchestrator {
        Orchestrator {
            workflow_path: workflow_path.into(),
            now: Box::new(Utc::now),
            eff: None,
            running: HashMap::new(),
            claimed: HashSet::new(),
            retry_attempts: HashMap::new(),
            completed: HashSet::new(),
            pending_stack: HashMap::new(),
            totals: Totals::default(),
        }
    }

    /// Returns the path of the WORKFLOW.md this orchestrator loads and watches. Mirrors Go
    /// `WorkflowPath`.
    pub fn workflow_path(&self) -> &str {
        &self.workflow_path
    }
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
