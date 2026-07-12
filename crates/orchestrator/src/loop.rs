//! control loop — parity port of Go `internal/orchestrator/loop.go` (the daemon's heart).
//!
//! Go runs the orchestrator as one control goroutine that owns all scheduling state and selects on a
//! buffered `chan event`; workers, timers, and the config watcher are separate goroutines that only
//! *send* events. The Rust port keeps that discipline as a single owning tokio task ([`Orchestrator::run_loaded`])
//! selecting on an mpsc receiver — channels in, channels out, no `Mutex` webs over the loop state.
//!
//! Because Go leans on language/stdlib features that have no dedicated source file (`context.Context`,
//! `sync.WaitGroup`, the `time.AfterFunc` timer, the `event` interface), the Rust stand-ins live here,
//! next to the loop they serve:
//!
//! * [`CancelSignal`] / [`CancelWait`] — a set-once cancellation pair (Go's cancelable `context.Context`
//!   and its `CancelFunc`). A `CancelSignal` embeds in [`RunningEntry`](crate::RunningEntry) as the
//!   worker's kill handle; a `CancelWait` is the orchestrator lifetime ctx (`o.ctx`).
//! * [`WaitGroup`] — the workers-and-resolvers barrier `shutdown` waits on (Go `sync.WaitGroup`).
//! * [`Event`] — the control-loop message set (Go's `event` interface + its `ev*` implementors),
//!   wrapping the payload structs the earlier tickets already own ([`EvWorkerExit`](crate::EvWorkerExit),
//!   [`EvRetry`](crate::EvRetry), [`AgentUpdate`](crate::AgentUpdate)).
//!
//! Deviations from Go, all behavior-preserving (matching the OBSERVABLE behavior the tests assert, per
//! the P5 plan's "semantics over structure" note):
//!   * The control channel is `tokio::sync::mpsc::unbounded` (Go: buffered 256). The worker's per-event
//!     forwarding closure is synchronous and cannot block/await a bounded send, and an unbounded feed
//!     never SHEDS a control event — strictly safer than Go's block-until-space. See the PR body.
//!   * Telemetry is P6: this loop emits its OWN short control-loop spans (`symphony.poll`,
//!     `symphony.fetch_candidates`) as `tracing` spans, but the `symphony.reconcile` / `symphony.dispatch`
//!     / `symphony.run` spans + the OTel export live in the reconcile/dispatch/worker tickets, which
//!     deferred them to P6 (see `worker.rs` / `retry.rs` module docs). The full `loop_spans_test` mirror
//!     is `#[ignore]`d for P6.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use rhapsody_agent as agent;
use rhapsody_config::CLAIM_MODE_POOL;
use rhapsody_core::Issue;
use rhapsody_tracker::Tracker;
use rhapsody_workspace::Manager;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::{oneshot, watch};
use tracing::Instrument;

use crate::agentupdate::AgentUpdate;
use crate::dispatch::dependency_mode_enabled;
use crate::effective::{Effective, ResolvedProject};
use crate::ghenrich::{apply_github_summons, enrich_with_github_summons, fetch_github_summons};
use crate::ghsummons::{self, GH, SummonHit};
use crate::handoff::HandoffPlan;
use crate::message::RunMessageResult;
use crate::orchestrator::Orchestrator;
use crate::reload::ReloadError;
use crate::retry::{DispatchRoute, EvRetry, EvWorkerExit};
use crate::select::TaggedIssue;
use crate::snapshot::{RefreshResult, Snapshot};
use crate::stop::{ControlHandle, ResumePlan, StopPlan};
use crate::worker::{WorkerDeps, run_agent_attempt};
use crate::workspace_gc::WorkspaceGcPlan;

/// A set-once cancellation trigger — the Rust stand-in for a cancelable `context.Context`'s
/// `CancelFunc`. Cloneable; `wait()` hands out awaitable [`CancelWait`] receivers. The trivial
/// `PartialEq`/`Debug` (a signal is never part of a value's identity) + a `Default` "unarmed" variant
/// let it embed inside `#[derive]`d structs like [`RunningEntry`](crate::RunningEntry).
#[derive(Clone)]
pub struct CancelSignal {
    tx: Option<watch::Sender<bool>>,
}

impl CancelSignal {
    /// An armed signal that can be cancelled (Go `context.WithCancel`).
    pub fn new() -> CancelSignal {
        CancelSignal {
            tx: Some(watch::channel(false).0),
        }
    }

    /// Fires the cancellation (Go the `CancelFunc`). Idempotent; a no-op on the unarmed default.
    pub fn cancel(&self) {
        if let Some(tx) = &self.tx {
            tx.send_replace(true);
        }
    }

    /// A fresh awaitable receiver of this signal (Go deriving a ctx off the same cancel).
    pub fn wait(&self) -> CancelWait {
        CancelWait {
            rx: self.tx.as_ref().map(watch::Sender::subscribe),
        }
    }
}

impl Default for CancelSignal {
    /// The UNARMED signal: `cancel()` is a no-op and `wait()` yields a never-cancelling [`CancelWait`].
    /// Used for test / legacy [`RunningEntry`](crate::RunningEntry)s that never spawned a real
    /// cancelable worker (Go leaves `runningEntry.cancel` nil there).
    fn default() -> CancelSignal {
        CancelSignal { tx: None }
    }
}

impl PartialEq for CancelSignal {
    fn eq(&self, _: &CancelSignal) -> bool {
        true
    }
}

impl std::fmt::Debug for CancelSignal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CancelSignal")
    }
}

/// An awaitable view of a [`CancelSignal`] — the Rust stand-in for a `context.Context`'s `Done()`.
/// The unarmed default (`None`) never cancels (the `context.Background()` analogue): `cancelled()`
/// blocks forever and `is_cancelled()` is always false.
#[derive(Clone, Default)]
pub struct CancelWait {
    rx: Option<watch::Receiver<bool>>,
}

impl CancelWait {
    /// Resolves once the underlying signal is cancelled (or its sender is dropped). Blocks forever on
    /// the background default. `&mut self` because the `watch` wait borrows the receiver mutably.
    pub async fn cancelled(&mut self) {
        match &mut self.rx {
            Some(rx) => {
                let _ = rx.wait_for(|c| *c).await;
            }
            None => std::future::pending::<()>().await,
        }
    }

    /// Whether the signal has already fired (Go a non-blocking `select { <-ctx.Done() }`).
    pub fn is_cancelled(&self) -> bool {
        self.rx.as_ref().is_some_and(|rx| *rx.borrow())
    }
}

/// The workers-and-resolvers barrier — the Rust stand-in for Go's `sync.WaitGroup`. [`add`](WaitGroup::add)
/// returns an RAII [`WgGuard`] whose drop decrements the count, so a spawned task registers by holding
/// the guard for its lifetime; [`wait`](WaitGroup::wait) resolves when the count returns to zero.
/// Cloneable (shared count) so off-loop tasks can register while the control task waits.
#[derive(Clone)]
pub struct WaitGroup {
    count: std::sync::Arc<watch::Sender<usize>>,
}

impl WaitGroup {
    pub fn new() -> WaitGroup {
        WaitGroup {
            count: std::sync::Arc::new(watch::channel(0usize).0),
        }
    }

    /// Registers one in-flight task; the returned guard decrements on drop (Go `wg.Add(1)` + `defer
    /// wg.Done()`).
    pub fn add(&self) -> WgGuard {
        self.count.send_modify(|c| *c += 1);
        WgGuard {
            count: std::sync::Arc::clone(&self.count),
        }
    }

    /// Resolves when every registered task has dropped its guard (Go `wg.Wait()`). Returns immediately
    /// when nothing is in flight.
    pub async fn wait(&self) {
        let mut rx = self.count.subscribe();
        let _ = rx.wait_for(|c| *c == 0).await;
    }
}

impl Default for WaitGroup {
    fn default() -> WaitGroup {
        WaitGroup::new()
    }
}

/// The in-flight registration handle for a [`WaitGroup`]; dropping it marks the task done.
pub struct WgGuard {
    count: std::sync::Arc<watch::Sender<usize>>,
}

impl Drop for WgGuard {
    fn drop(&mut self) {
        self.count.send_modify(|c| *c = c.saturating_sub(1));
    }
}

/// The GitHub-summons `since` lookback window (Go `New` sets `ghLookback = 5 * time.Minute`): a
/// generous overlap over the poll interval so a comment is never missed between adjacent ticks. A
/// fixed default in the Go daemon; the enrichment passes `now - DEFAULT_GH_LOOKBACK` as the `since`.
pub(crate) const DEFAULT_GH_LOOKBACK: Duration = Duration::from_secs(5 * 60);

/// A control-loop message (Go's `event` interface + its `ev*` implementors). The single owning task
/// selects these off the mpsc receiver and dispatches them in [`Orchestrator::handle`]; workers,
/// timers, the config watcher, and the off-loop [`ControlHandle`](crate::stop::ControlHandle) only
/// SEND them. Reply-carrying variants use a `oneshot` back-channel (Go a buffered reply `chan`).
///
/// The [`RunMessage`](Event::RunMessage) variant (operator mid-run messages, INF-250) carries an
/// admission request for a live run onto the control channel: O6 (`message.rs`) owns its handler
/// [`handle_run_message`](Orchestrator::handle_run_message) + the mailbox plumbing, and O7 routes it
/// through this channel so admission stays loop-confined (Go's `evRunMessage` / `SendRunMessage`).
pub enum Event {
    /// A poll-timer fire (Go `evTick`).
    Tick,
    /// A worker task's terminal report (Go `evWorkerExit`).
    WorkerExit(EvWorkerExit),
    /// One agent event folded into the running entry (Go `evAgentUpdate`).
    AgentUpdate(AgentUpdate),
    /// The concrete per-run transcript path the worker opened (Go `evTranscriptOpened`).
    TranscriptOpened { issue_id: String, path: String },
    /// A fired retry timer (Go `evRetry`).
    Retry(EvRetry),
    /// A WORKFLOW.md change observed by the watcher (Go `evReload`).
    Reload,
    /// A request for the API state snapshot, built on the loop (Go `evSnapshot`).
    Snapshot { reply: oneshot::Sender<Snapshot> },
    /// A request for the race-free workspace-GC plan (Go `evWorkspaceGC`).
    WorkspaceGc {
        reply: oneshot::Sender<WorkspaceGcPlan>,
    },
    /// The TOCTOU guard: is this worktree path a live running issue RIGHT NOW (Go `evWorkspaceInUse`)?
    WorkspaceInUse {
        mgr: Option<Arc<Manager>>,
        path: String,
        reply: oneshot::Sender<bool>,
    },
    /// Kill + record-canceled + suppress on the loop, replying with the issue/team (Go `evStopRun`).
    StopRun {
        run_id: i64,
        reply: oneshot::Sender<StopPlan>,
    },
    /// Clear (moved) or keep (not moved) the in-memory suppression after the Backlog move (Go `evStopFinalize`).
    StopFinalize {
        issue_id: String,
        moved: bool,
        reply: oneshot::Sender<()>,
    },
    /// The resume admission check on the loop (Go `evResume`).
    Resume {
        issue_id: String,
        identifier: String,
        project: String,
        run_id: i64,
        reply: oneshot::Sender<ResumePlan>,
    },
    /// Clear (moved) or keep (not moved) the suppression after the move-to-Todo (Go `evResumeFinalize`).
    ResumeFinalize {
        issue_id: String,
        moved: bool,
        reply: oneshot::Sender<()>,
    },
    /// Admit an operator message for a live run ON the loop (Go `evRunMessage`, INF-250). O6 owns the
    /// handler ([`handle_run_message`](Orchestrator::handle_run_message)); O7 routes it through the
    /// control channel so admission stays loop-confined.
    RunMessage {
        run_id: i64,
        text: String,
        reply: oneshot::Sender<RunMessageResult>,
    },
    /// Resolve a live run's issue/team + configured review state on the loop, replying with the handoff
    /// plan (TRA-242; NEW beyond Go v0.4.0). Read-only — no kill, no suppression change; the off-loop
    /// [`handoff_run`](ControlHandle::handoff_run) does the review-state move that winds the run down.
    HandoffRun {
        run_id: i64,
        reply: oneshot::Sender<HandoffPlan>,
    },
}

/// The default `storage.retention_days` until a reload stores the effective value (Go `New`).
pub(crate) const DEFAULT_RETENTION_DAYS: i64 = 30;

impl Orchestrator {
    /// Builds the shared github-summons source iff the feature is enabled for the legacy single-project
    /// config OR any resolved project, else `None` (feature off ⇒ the poll path stays byte-identical to
    /// the pre-feature behavior — every enrichment site is gated on `o.gh_source.is_some()`). A single
    /// [`GH`] serves every project (owner/repo are passed per call; the summon substring is the only
    /// construction input). Called once from `Run` at startup and rebuilt by `on_reload` from the
    /// freshly-swapped `o.eff`. Mirrors Go `newGitHubSummonSource`.
    pub(crate) fn new_github_summon_source(&self) -> Option<Box<dyn ghsummons::SummonSource>> {
        let eff = self.eff.as_ref()?;
        if eff.cfg.tracker.github_summons {
            return Some(Box::new(GH::new(&eff.cfg.tracker.summon_token, None)));
        }
        for p in &eff.projects {
            if p.github_summons {
                return Some(Box::new(GH::new(&p.mcfg.tracker.summon_token, None)));
            }
        }
        None
    }
}

/// Snapshots the effective deps for a worker (captured at dispatch; reload does not affect in-flight
/// workers). When `rp` is `Some` the worker runs with that project's prompt / active-states / tracker
/// (and shared agent/workspace); when `None` it falls back to the top-level effective fields (legacy
/// single-project + the test-injected effective path). `max_turns` / transcripts / `pr_label` stay
/// top-level. Mirrors Go `workerDepsFor` (dropping the telemetry fields — Tracer/Metrics/Model/
/// DispatchSpanContext/RunID — per the P6 deferral; see `worker.rs`).
fn worker_deps_for(eff: &Effective, rp: Option<&ResolvedProject>) -> WorkerDeps {
    // Local raw logging is enabled only when a log dir is configured (Go passes `o.eff.transcripts`,
    // which is nil when logging is off).
    let transcripts = if eff.log_dir.is_empty() {
        None
    } else {
        Some(Arc::clone(&eff.transcripts))
    };
    let mut deps = WorkerDeps {
        workspace: Arc::clone(&eff.workspace),
        agent: Arc::clone(&eff.agent),
        tracker: Arc::clone(&eff.tracker),
        prompt_tmpl: eff.prompt_tmpl.clone(),
        prompt_file: eff.prompt_file.clone(),
        max_turns: eff.max_turns,
        active_states: eff.active_states.clone(),
        transcripts,
        repo_url: String::new(),
        project_slug: String::new(),
        git_flow: eff.git_flow.clone(),
        workspace_mode: eff.workspace_mode.clone(),
        stack_context: String::new(),
        pr_label: eff.pr_label.clone(),
        // The review state a declared HANDOFF parks the ticket in (TRA-240). review_states is a
        // normalized set; MoveIssueState resolves case-insensitively, so the normalized name is fine.
        // `None` when the feature is off ⇒ Go-identical ticket-state-only loop termination.
        review_handoff_state: eff.review_states.iter().next().cloned(),
    };
    if let Some(rp) = rp {
        deps.workspace = Arc::clone(&rp.workspace);
        deps.agent = Arc::clone(&rp.agent);
        deps.tracker = Arc::clone(&rp.tracker);
        deps.prompt_tmpl = rp.prompt_tmpl.clone();
        deps.prompt_file = rp.prompt_file.clone();
        deps.git_flow = rp.git_flow.clone();
        deps.workspace_mode = rp.workspace_mode.clone();
        deps.active_states = rp.active_states.clone();
        deps.review_handoff_state = rp.review_states.iter().next().cloned(); // per-project park state (TRA-240)
        deps.repo_url = rp.repo.clone(); // Phase 3: per-issue repo URL for the worktree workspace
        deps.project_slug = rp.slug.clone(); // surfaced to hooks as SYMPHONY_PROJECT
    }
    // Prompt selection by dependency_mode (INF-318): graphite/dag mode renders the mode-on prompt
    // (`.rhapsody/PROMPT.dep_mod.md` by default, TRA-238); disabled mode leaves `prompt_file`
    // untouched (byte-identical).
    let (mode, dep_prompt) = match rp {
        Some(rp) => (
            rp.dependency_mode.as_str(),
            rp.dep_mode_prompt_file.as_str(),
        ),
        None => (
            eff.dependency_mode.as_str(),
            eff.dep_mode_prompt_file.as_str(),
        ),
    };
    if dependency_mode_enabled(mode) && !dep_prompt.is_empty() {
        deps.prompt_file = dep_prompt.to_string();
    }
    deps
}

impl Orchestrator {
    /// Loads + validates the workflow (failing startup on error), builds the github-summons source,
    /// starts the async event writer, runs boot recovery, performs startup terminal-workspace cleanup,
    /// watches the workflow for changes, and runs the control loop until `ctx` is cancelled (upstream
    /// §16.1). Mirrors Go `Run`.
    pub async fn run(&mut self, ctx: CancelWait) -> Result<(), ReloadError> {
        self.ctx = Some(ctx.clone());
        if let Err(e) = self.reload_from_disk() {
            tracing::error!(err = %e, "startup validation failed");
            return Err(e);
        }
        // github-summons enrichment (AIE-299): built once at startup, only when some project (or the
        // legacy config) enables the flag; `on_reload` rebuilds it from the freshly-swapped `o.eff`.
        self.gh_source = self.new_github_summon_source();
        if !self.store_injected {
            // Go opens the durable disk store here (`openStore`) from storage config + the --db /
            // --no-store overrides; that disk-open wiring lands with the `rhapsodyd` binary (P6). O7
            // keeps the injected / Noop store, so this is intentionally a no-op path.
            tracing::debug!(
                "no store injected; using the default store (disk store-open is P6/daemon)"
            );
        }
        self.start_event_writer();
        self.boot_recovery();
        self.startup_cleanup().await;
        let watch = self.start_watch(ctx.clone());
        self.run_loaded(ctx).await;
        watch.abort();
        Ok(())
    }

    /// The control loop proper: schedules the first tick, then selects on `ctx` cancellation + the
    /// control channel, dispatching each event via [`handle`](Orchestrator::handle) until cancelled or
    /// the channel closes, then shuts down. Assumes `o.ctx` is already set by the caller. Mirrors Go
    /// `runLoaded`.
    pub async fn run_loaded(&mut self, mut ctx: CancelWait) {
        let Some(mut rx) = self
            .events_rx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        else {
            return; // the receiver was already taken (loop already running) — defensive
        };
        self.schedule_tick(Duration::ZERO);
        loop {
            tokio::select! {
                _ = ctx.cancelled() => {
                    self.shutdown(&mut rx).await;
                    return;
                }
                ev = rx.recv() => match ev {
                    Some(ev) => self.handle(ev).await,
                    None => {
                        self.shutdown(&mut rx).await;
                        return;
                    }
                }
            }
        }
    }

    /// Dispatches one control-loop event to its handler. Mirrors Go `handle`.
    async fn handle(&mut self, ev: Event) {
        match ev {
            Event::Tick => self.on_tick().await,
            Event::WorkerExit(e) => self.on_worker_exit(e),
            Event::AgentUpdate(e) => self.on_agent_update(e),
            Event::TranscriptOpened { issue_id, path } => {
                self.on_transcript_opened(&issue_id, &path)
            }
            Event::Retry(e) => self.on_retry(e).await,
            Event::Reload => self.on_reload(),
            Event::Snapshot { reply } => {
                let _ = reply.send(self.build_snapshot());
            }
            Event::WorkspaceGc { reply } => {
                let _ = reply.send(self.build_workspace_gc_plan());
            }
            Event::WorkspaceInUse { mgr, path, reply } => {
                let _ = reply.send(self.worktree_in_use(mgr.as_deref(), &path));
            }
            Event::StopRun { run_id, reply } => {
                let _ = reply.send(self.handle_stop_run(run_id));
            }
            Event::StopFinalize {
                issue_id,
                moved,
                reply,
            } => {
                self.handle_stop_finalize(&issue_id, moved);
                let _ = reply.send(());
            }
            Event::Resume {
                issue_id,
                identifier,
                project,
                run_id,
                reply,
            } => {
                let _ = reply.send(self.handle_resume(&issue_id, &identifier, &project, run_id));
            }
            Event::ResumeFinalize {
                issue_id,
                moved,
                reply,
            } => {
                self.handle_resume_finalize(&issue_id, moved);
                let _ = reply.send(());
            }
            Event::RunMessage {
                run_id,
                text,
                reply,
            } => {
                let _ = reply.send(self.handle_run_message(run_id, &text));
            }
            Event::HandoffRun { run_id, reply } => {
                let _ = reply.send(self.handle_handoff_run(run_id));
            }
        }
    }

    /// (Re)arms the poll-tick timer: aborts the previous, spawns a task that sleeps `delay` then sends
    /// [`Event::Tick`] (unless the lifetime ctx is cancelled first). Mirrors Go `scheduleTick`
    /// (`time.AfterFunc`).
    pub(crate) fn schedule_tick(&mut self, delay: Duration) {
        if let Some(t) = self.tick_timer.take() {
            t.abort();
        }
        let events = self.events.clone();
        let mut ctx = self.ctx.clone().unwrap_or_default();
        self.tick_timer = Some(tokio::spawn(async move {
            tokio::select! {
                _ = tokio::time::sleep(delay) => { let _ = events.send(Event::Tick); }
                _ = ctx.cancelled() => {}
            }
        }));
    }

    /// The poll interval from the effective config (Go `o.eff.pollInterval`), or zero when no config is
    /// loaded (unreachable on the live loop).
    fn poll_interval(&self) -> Duration {
        self.eff
            .as_ref()
            .map(|e| e.poll_interval)
            .unwrap_or(Duration::ZERO)
    }

    /// The github-summons `since` watermark for this tick: now minus the sliding lookback window (Go
    /// `o.now().Add(-o.ghLookback)`).
    fn gh_since(&self) -> DateTime<Utc> {
        (self.now)() - chrono::Duration::seconds(DEFAULT_GH_LOOKBACK.as_secs() as i64)
    }

    /// onTick: reconcile → preflight validate → fetch candidates → dispatch (upstream §8.1, §16.2),
    /// re-arming the poll timer at the end (Go's `defer scheduleTick`). Mirrors Go `onTick`.
    pub(crate) async fn on_tick(&mut self) {
        let poll = self.poll_interval();
        self.reconcile().await;
        if let Err(e) = self.validate() {
            tracing::error!(err = %e, "dispatch preflight validation failed; skipping dispatch");
            self.schedule_tick(poll);
            return;
        }
        // symphony.poll wraps this tick's candidate fetch + dispatch decisions (a short control-loop
        // span; reconcile is its own root). fetch_candidates + dispatch nest under it. O7 owns these
        // control-loop spans; the reconcile/dispatch/run spans + OTel export are P6 (see the module docs).
        async {
            self.dispatch_decisions().await;
            // DAG auto-promote (INF-318): AFTER the normal dispatch decisions, flip any enabled-mode
            // Backlog dependent whose blockers are cleared Backlog→Todo (O6 `promote.rs`). A no-op for
            // disabled-mode projects; runs for BOTH the multi-project and legacy paths.
            self.promote_unblocked().await;
        }
        .instrument(tracing::info_span!("symphony.poll"))
        .await;
        self.schedule_tick(poll);
    }

    /// The per-tick candidate fetch + dispatch + review-reopen for both the multi-project and the
    /// legacy single-tracker path. Extracted from `on_tick` so the auto-promote pass always runs after
    /// it (INF-318). Mirrors Go `dispatchDecisions`.
    async fn dispatch_decisions(&mut self) {
        let has_projects = self.eff.as_ref().is_some_and(|e| !e.projects.is_empty());
        if has_projects {
            let tagged = self.poll_all_projects().await;
            // Route mid-run summons into live runs BEFORE select drops the running issues (INF-448,
            // O6 `message.rs`).
            self.deliver_mid_run_summons_tagged(&tagged);
            let (picked, reopen) = self.select_dispatch_multi_with_reopens(tagged);
            // Pool-mode picks (INF-477) win the single-claimant claim BEFORE dispatch; assignee-mode
            // picks dispatch immediately. Build owned routes before the `&mut self` dispatch.
            let mut pool_picks: Vec<TaggedIssue> = Vec::new();
            let mut direct: Vec<(Issue, Option<DispatchRoute>)> = Vec::new();
            for ti in picked {
                if self.is_pool_project(ti.proj) {
                    pool_picks.push(ti);
                } else {
                    let route = self.route_for(ti.proj);
                    direct.push((ti.iss, route));
                }
            }
            for (iss, route) in direct {
                self.dispatch_issue(iss, None, route, String::new());
            }
            for ti in self.claim_winners(pool_picks).await {
                let route = self.route_for(ti.proj);
                self.dispatch_issue(ti.iss, None, route, String::new());
            }
            // Review-reopens: promote (Linear WRITE) THEN dispatch.
            for ti in reopen {
                let route = self.route_for(ti.proj);
                self.promote_and_dispatch(ti.iss, route).await;
            }
            return;
        }

        // Legacy single-tracker path (test-injected effectives).
        let Some(tracker) = self.eff.as_ref().map(|e| Arc::clone(&e.tracker)) else {
            return;
        };
        let issues = match tracker
            .fetch_candidate_issues()
            .instrument(tracing::info_span!("symphony.fetch_candidates"))
            .await
        {
            Ok(i) => i,
            Err(e) => {
                tracing::error!(err = %e, "candidate fetch failed; skipping dispatch this tick");
                return;
            }
        };
        // github-summons enrichment (AIE-299): advance latest_summon_at from unmerged linked-PR summon
        // comments before eligibility selection. Gated on the legacy flag AND a non-nil source, so the
        // feature being off is byte-identical. Owner/repo derive from the legacy top-level repo (O6).
        let mut issues = issues;
        let enrich_repo = self
            .eff
            .as_ref()
            .filter(|e| e.cfg.tracker.github_summons)
            .map(|e| e.cfg.repo.clone());
        if let (Some(repo_url), Some(src)) = (enrich_repo, self.gh_source.as_deref()) {
            let (owner, repo) = ghsummons::parse_repo(&repo_url).unwrap_or_default();
            let since = self.gh_since();
            issues = enrich_with_github_summons(issues, Some(src), &owner, &repo, since).await;
        }
        // Route mid-run summons into live runs BEFORE select drops the running issues (INF-448, O6).
        self.deliver_mid_run_summons(&issues);
        let (active, reopen) = self.select_dispatch_with_reopens(issues);
        if self
            .eff
            .as_ref()
            .is_some_and(|e| e.claim_mode == CLAIM_MODE_POOL)
        {
            let pool_picks: Vec<TaggedIssue> = active
                .into_iter()
                .map(|iss| TaggedIssue { iss, proj: None })
                .collect();
            for ti in self.claim_winners(pool_picks).await {
                self.dispatch_issue(ti.iss, None, None, String::new());
            }
        } else {
            for iss in active {
                self.dispatch_issue(iss, None, None, String::new());
            }
        }
        for iss in reopen {
            self.promote_and_dispatch(iss, None).await;
        }
    }

    /// Whether the resolved project at index `proj` claims in pool mode (INF-477). `None` proj (legacy)
    /// is never pool here.
    fn is_pool_project(&self, proj: Option<usize>) -> bool {
        proj.and_then(|idx| {
            self.eff
                .as_ref()
                .and_then(|e| e.projects.get(idx))
                .map(|p| p.claim_mode == CLAIM_MODE_POOL)
        })
        .unwrap_or(false)
    }

    /// Builds the dispatch routing snapshot for a tagged pick's owning project (`None` proj ⇒ legacy).
    fn route_for(&self, proj: Option<usize>) -> Option<DispatchRoute> {
        let idx = proj?;
        let p = self.eff.as_ref()?.projects.get(idx)?;
        Some(DispatchRoute {
            slug: p.slug.clone(),
            group: p.group.clone(),
            repo: p.repo.clone(),
            model: p.model.clone(),
            workspace_mode: p.workspace_mode.clone(),
        })
    }

    /// Fetches candidates from EVERY resolved project's slug-bound tracker, tags each issue with its
    /// owning project index, and de-dups by issue ID (first project wins). A per-project fetch error is
    /// logged and that project is skipped this tick. github-summons enrichment (AIE-299) advances a
    /// kept issue's `latest_summon_at` against its project's repo, fetching each distinct repo at most
    /// once per tick (a per-repo cache keeps GitHub usage flat per repo, O6 `ghenrich`). Mirrors Go
    /// `pollAllProjects`.
    async fn poll_all_projects(&self) -> Vec<TaggedIssue> {
        struct ProjPoll {
            idx: usize,
            tracker: Arc<dyn Tracker>,
            slug: String,
            gh_summons: bool,
            gh_owner: String,
            gh_repo: String,
        }
        let Some(eff) = self.eff.as_ref() else {
            return Vec::new();
        };
        // Snapshot each enabled project's poll inputs before the awaits (no `self.eff` borrow held
        // across the async fetches).
        let projs: Vec<ProjPoll> = eff
            .projects
            .iter()
            .enumerate()
            .filter(|(_, p)| !p.disabled) // paused project (enabled:false): never polled (INF-224)
            .map(|(idx, p)| ProjPoll {
                idx,
                tracker: Arc::clone(&p.tracker),
                slug: p.slug.clone(),
                gh_summons: p.github_summons,
                gh_owner: p.gh_owner.clone(),
                gh_repo: p.gh_repo.clone(),
            })
            .collect();
        let src = self.gh_source.as_deref();
        let since = self.gh_since();
        async {
            let mut tagged: Vec<TaggedIssue> = Vec::new();
            let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
            // Per-repo summon cache, scoped to THIS tick (owner/repo lowercased — GitHub is
            // case-insensitive), so multiple projects on one repo fetch it at most once.
            let mut gh_cache: std::collections::HashMap<String, Option<std::collections::HashMap<i64, SummonHit>>> =
                std::collections::HashMap::new();
            for p in projs {
                let issues = match p.tracker.fetch_candidate_issues().await {
                    Ok(i) => i,
                    Err(e) => {
                        tracing::error!(project_slug = %p.slug, err = %e, "candidate fetch failed for project; skipping it this tick");
                        continue;
                    }
                };
                for mut iss in issues {
                    if !seen.insert(iss.id.clone()) {
                        continue; // first project wins on duplicate issue IDs
                    }
                    // Enrich the KEPT copy (after dedup) against its owning project's repo (AIE-299).
                    if let Some(src) = src
                        && p.gh_summons
                        && !p.gh_owner.is_empty()
                        && !p.gh_repo.is_empty()
                    {
                        let key = format!("{}/{}", p.gh_owner, p.gh_repo).to_lowercase();
                        if !gh_cache.contains_key(&key) {
                            let hits =
                                fetch_github_summons(Some(src), &p.gh_owner, &p.gh_repo, since).await;
                            gh_cache.insert(key.clone(), hits);
                        }
                        if let Some(Some(by_pr)) = gh_cache.get(&key) {
                            iss = apply_github_summons(vec![iss], by_pr, &p.gh_owner, &p.gh_repo)
                                .into_iter()
                                .next()
                                .unwrap_or_default();
                        }
                    }
                    tagged.push(TaggedIssue {
                        iss,
                        proj: Some(p.idx),
                    });
                }
            }
            tagged
        }
        .instrument(tracing::info_span!("symphony.fetch_candidates"))
        .await
    }

    /// Re-engages a summoned review-state issue: moves the ticket to the configured active promote
    /// state via a Linear WRITE and ONLY on success dispatches it. On a write error it logs and SKIPS
    /// (reconcile would terminate an un-promoted review dispatch within a tick). Mirrors Go
    /// `promoteAndDispatch`.
    async fn promote_and_dispatch(&mut self, mut iss: Issue, route: Option<DispatchRoute>) {
        let (tracker, promote_state) = {
            let Some(eff) = self.eff.as_ref() else {
                return;
            };
            let tracker = match &route {
                Some(r) => eff
                    .project_by_slug(&r.slug)
                    .map_or_else(|| Arc::clone(&eff.tracker), |p| Arc::clone(&p.tracker)),
                None => Arc::clone(&eff.tracker),
            };
            (tracker, eff.review_promote_state.clone())
        };
        if let Err(e) = tracker
            .move_issue_state(&iss.id, &iss.team_id, &promote_state)
            .await
        {
            tracing::error!(issue_id = %iss.id, issue_identifier = %iss.identifier, promote_state = %promote_state, err = %e, "review-reopen promote failed; skipping (not dispatching un-promoted review issue)");
            return;
        }
        tracing::info!(issue_id = %iss.id, issue_identifier = %iss.identifier, from_state = %iss.state, promote_state = %promote_state, "review-reopen: summoned ticket promoted and dispatched");
        iss.state = promote_state;
        self.dispatch_issue(iss, None, route, String::new());
    }

    /// Removes workspaces for issues already in terminal states at startup (§8.6). Per-project when
    /// projects are resolved (each project's slug-bound tracker + its own terminal-state list);
    /// single-project degenerates to today's behavior. Mirrors Go `startupCleanup`.
    async fn startup_cleanup(&self) {
        struct Group {
            tracker: Arc<dyn Tracker>,
            terminal: Vec<String>,
            workspace: Arc<Manager>,
            repo: String,
            slug: String,
        }
        let groups: Vec<Group> = {
            let Some(eff) = self.eff.as_ref() else {
                return;
            };
            if !eff.projects.is_empty() {
                eff.projects
                    .iter()
                    .map(|p| Group {
                        tracker: Arc::clone(&p.tracker),
                        terminal: p.mcfg.tracker.terminal_states.clone(),
                        workspace: Arc::clone(&p.workspace),
                        repo: p.repo.clone(),
                        slug: p.slug.clone(),
                    })
                    .collect()
            } else {
                vec![Group {
                    tracker: Arc::clone(&eff.tracker),
                    terminal: eff.cfg.tracker.terminal_states.clone(),
                    workspace: Arc::clone(&eff.workspace),
                    repo: String::new(),
                    slug: String::new(),
                }]
            }
        };
        for g in groups {
            let issues = match g.tracker.fetch_issues_by_states(&g.terminal).await {
                Ok(i) => i,
                Err(e) => {
                    tracing::warn!(project_slug = %g.slug, err = %e, "startup terminal cleanup fetch failed; continuing");
                    continue;
                }
            };
            for iss in issues {
                if let Err(e) = g
                    .workspace
                    .remove_worktree(&g.repo, &g.slug, &iss.identifier)
                    .await
                {
                    tracing::warn!(issue_identifier = %iss.identifier, project_slug = %g.slug, err = %e, "startup workspace cleanup failed");
                }
            }
        }
    }

    /// Cancels all workers + timers, then drains events until workers (and off-loop resolvers) exit,
    /// finally stopping the event writer. Mirrors Go `shutdown`.
    async fn shutdown(&mut self, rx: &mut UnboundedReceiver<Event>) {
        for re in self.running.values() {
            re.cancel.cancel();
        }
        if let Some(t) = self.tick_timer.take() {
            t.abort();
        }
        for (_, t) in self.retry_timers.drain() {
            t.abort();
        }
        // Wait for workers + resolvers to finish, draining the channel so any final worker-exit sends
        // are consumed promptly (the unbounded channel never blocks the senders).
        {
            let wg = self.wg.clone();
            let mut wait = Box::pin(wg.wait());
            loop {
                tokio::select! {
                    _ = &mut wait => break,
                    _ = rx.recv() => {}
                }
            }
        }
        self.stop_event_writer();
    }

    /// The production worker spawn: builds the owning project's deps, takes the run's operator mailbox
    /// receiver (INF-250), launches a tokio task driving `run_agent_attempt`, and forwards its per-turn
    /// agent events + transcript-open + terminal exit back onto the control channel. Cancellation (from
    /// `terminate` / `shutdown`) races the run and drops it. Mirrors Go `spawnWorker` (telemetry span
    /// links are P6; full agent-subprocess kill on cancel is validated e2e in O8).
    pub(crate) fn spawn_worker(
        &self,
        mut cancel: CancelWait,
        iss: Issue,
        attempt: Option<i64>,
        project_slug: String,
        stack_context: String,
        started_at: DateTime<Utc>,
    ) {
        let Some(eff) = self.eff.as_ref() else {
            return; // no effective config → nothing to run (defensive; production always has one)
        };
        let mut deps = worker_deps_for(eff, eff.project_by_slug(&project_slug));
        deps.stack_context = stack_context;
        // Take this run's operator-message mailbox receiver (INF-250, O6): the worker drains it onto the
        // agent's held-open stdin. `None` for legacy / test-injected entries with no mailbox.
        let mut mailbox = self
            .mailboxes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_mut(&iss.id)
            .and_then(|mb| mb.rx.take());
        let events_exit = self.events.clone();
        let events_ev = self.events.clone();
        let events_tr = self.events.clone();
        let issue_id = iss.id.clone();
        let issue_id_ev = iss.id.clone();
        let issue_id_tr = iss.id.clone();
        let attempt32 = attempt.map(|a| a as i32);
        let guard = self.wg.add();
        tokio::spawn(async move {
            let _guard = guard; // held for the worker's lifetime (Go `o.wg.Add(1)` + `defer Done`)
            let on_event = move |e: agent::Event| {
                let _ = events_ev.send(Event::AgentUpdate(AgentUpdate {
                    issue_id: issue_id_ev.clone(),
                    ev: e,
                }));
            };
            let on_transcript = move |path: &str| {
                let _ = events_tr.send(Event::TranscriptOpened {
                    issue_id: issue_id_tr.clone(),
                    path: path.to_string(),
                });
            };
            let run = run_agent_attempt(
                &deps,
                iss.clone(),
                attempt32,
                mailbox.as_mut(),
                &on_event,
                Some(&on_transcript),
            );
            let (final_state, declared, err) = tokio::select! {
                res = run => res,
                _ = cancel.cancelled() => (iss.state.clone(), false, None),
            };
            let exit = EvWorkerExit {
                issue_id,
                failed: err.is_some(),
                started_at,
                err_msg: err.map(|e| e.to_string()).unwrap_or_default(),
                last_state: final_state,
                declared_handoff: declared,
            };
            let _ = events_exit.send(Event::WorkerExit(exit));
        });
    }
}

impl ControlHandle {
    /// Requests the API state snapshot, built on the control task (Go's HTTP layer sends `evSnapshot`).
    /// The P6 `/api/v1/state` surface; provided here so O7 owns the round-trip. Returns `None` if the
    /// loop is gone.
    pub async fn snapshot(&self) -> Option<Snapshot> {
        let (tx, rx) = oneshot::channel();
        self.events.send(Event::Snapshot { reply: tx }).ok()?;
        rx.await.ok()
    }

    /// Requests the race-free workspace-GC plan, built on the control task (Go `evWorkspaceGC`). Backs
    /// the P6 prune scheduler.
    pub async fn workspace_gc_plan(&self) -> Option<WorkspaceGcPlan> {
        let (tx, rx) = oneshot::channel();
        self.events.send(Event::WorkspaceGc { reply: tx }).ok()?;
        rx.await.ok()
    }

    /// The TOCTOU guard: is `path` a live running issue's worktree RIGHT NOW (Go `evWorkspaceInUse`)?
    /// The prune scheduler sends one immediately before each candidate removal.
    pub async fn worktree_in_use(&self, mgr: Option<Arc<Manager>>, path: String) -> bool {
        let (tx, rx) = oneshot::channel();
        if self
            .events
            .send(Event::WorkspaceInUse {
                mgr,
                path,
                reply: tx,
            })
            .is_err()
        {
            return false;
        }
        rx.await.unwrap_or(false)
    }

    /// Queues an operator message for a live run's agent (INF-250), round-tripping the control channel
    /// so admission stays loop-confined (Go `SendRunMessage`). Backs the P6 `POST /api/v1/runs/:id/message`
    /// surface. Returns `not_running` if the loop is gone or the reply is dropped.
    pub async fn send_run_message(&self, run_id: i64, text: &str) -> RunMessageResult {
        let (tx, rx) = oneshot::channel();
        let ev = Event::RunMessage {
            run_id,
            text: text.to_string(),
            reply: tx,
        };
        if self.events.send(ev).is_err() {
            return RunMessageResult {
                not_running: true,
                ..Default::default()
            };
        }
        let mut lifetime = self.ctx.clone();
        tokio::select! {
            r = rx => r.unwrap_or(RunMessageResult { not_running: true, ..Default::default() }),
            _ = lifetime.cancelled() => RunMessageResult { not_running: true, ..Default::default() },
        }
    }

    /// Requests a coalesced poll+reconcile tick (Go's non-blocking `evTick` send), backing the P6
    /// `POST /api/v1/refresh` surface. The control channel is unbounded, so a tick is always enqueued
    /// (`queued: true`) and never folded into a pending one — `coalesced` is unreachable here, the
    /// deliberate deviation from Go's buffered-channel coalescing (a burst of refreshes just enqueues a
    /// burst of ticks the loop drains). Synchronous + infallible, so the handler always answers 202.
    /// Mirrors Go `Refresh` (`snapshot.go`), whose live `Orchestrator::refresh` the assembly (F1) owns.
    pub fn refresh(&self) -> RefreshResult {
        // Best-effort: a send failure means the loop is already gone (the daemon is shutting down), in
        // which case the tick is moot; still report `queued` to match Go's unconditional result shape.
        let _ = self.events.send(Event::Tick);
        RefreshResult {
            queued: true,
            coalesced: false,
            requested_at: Utc::now(),
            operations: vec!["poll".to_string(), "reconcile".to_string()],
        }
    }
}

#[cfg(test)]
impl Orchestrator {
    /// Test seam: dispatch one control event exactly as the running loop's [`handle`](Orchestrator::handle)
    /// would (Go's e2e tests call `o.handle(o.ctx, e)` directly). O8's file-tracker e2e drives
    /// `on_tick` / `on_retry` itself and pumps the control channel through this, so the whole
    /// poll → claim → dispatch → worker → store cycle runs deterministically on the one test task
    /// without ever starting [`run_loaded`](Orchestrator::run_loaded).
    pub(crate) async fn drive_event(&mut self, ev: Event) {
        self.handle(ev).await;
    }

    /// Test seam: take the control-event receiver so the e2e pump can drain it (Go's e2e tests read
    /// `o.events` directly). Taken once; `run_loaded` is never started in these tests, so nothing else
    /// contends for it.
    pub(crate) fn take_events_rx(&self) -> Option<UnboundedReceiver<Event>> {
        self.events_rx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestrator::Orchestrator;
    use crate::testsupport::{
        DispatchedEntries, TempDir, empty_effective, issue, orch_for_retry_multi,
        proj_with_tracker, record_entries, set_of,
    };
    use rhapsody_tracker::TrackerError;
    use rhapsody_tracker::fake::Fake;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicI32, Ordering};

    /// Wires an orchestrator with fakes and a recording spawn that increments a counter and simulates
    /// an immediate clean worker exit (Go `newLoopOrch`), bypassing the disk load (sets `eff` directly).
    fn new_loop_orch(tr: Fake, poll: Duration) -> (Orchestrator, Arc<AtomicI32>) {
        let mut eff = empty_effective(Arc::new(tr));
        eff.active_states = set_of(&["todo", "in progress"]);
        eff.terminal_states = set_of(&["done"]);
        eff.max_concurrent = 10;
        eff.poll_interval = poll;
        eff.max_retry_backoff_ms = 300_000;
        let mut o = Orchestrator::new("WORKFLOW.md");
        o.eff = Some(eff);
        let spawned = Arc::new(AtomicI32::new(0));
        let spawned2 = Arc::clone(&spawned);
        let tx = o.events.clone();
        o.spawn = Some(Box::new(move |iss, _attempt, re| {
            spawned2.fetch_add(1, Ordering::SeqCst);
            // Simulate an immediate clean worker exit (Go posts evWorkerExit from a goroutine).
            let _ = tx.send(Event::WorkerExit(EvWorkerExit {
                issue_id: iss.id.clone(),
                failed: false,
                started_at: re.started_at,
                err_msg: String::new(),
                last_state: String::new(),
                declared_handoff: false,
            }));
        }));
        (o, spawned)
    }

    // Mirrors Go `TestRunDispatchesCandidatesThenStops`.
    #[tokio::test(flavor = "multi_thread")]
    async fn run_dispatches_candidates_then_stops() {
        let mut tr = Fake::new();
        tr.candidates = vec![issue("1", "MT-1", "Todo")];
        let (mut o, spawned) = new_loop_orch(tr, Duration::from_millis(20));
        let signal = CancelSignal::new();
        o.ctx = Some(signal.wait());
        let loop_ctx = signal.wait();
        let task = tokio::spawn(async move {
            o.run_loaded(loop_ctx).await;
            o
        });
        // Wait until at least one dispatch happened.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while spawned.load(Ordering::SeqCst) == 0 {
            if tokio::time::Instant::now() > deadline {
                signal.cancel();
                panic!("no dispatch happened");
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        signal.cancel();
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("Run did not stop on context cancel")
            .expect("loop task panicked");
    }

    // Mirrors Go `TestRunStartupValidationFailureReturnsError`: a real disk load with a missing file
    // must fail startup.
    #[tokio::test]
    async fn run_startup_validation_failure_returns_error() {
        let dir = TempDir::new();
        let mut o = Orchestrator::new(dir.child("does-not-exist.md"));
        assert!(
            o.run(CancelWait::default()).await.is_err(),
            "expected startup error for a missing workflow file"
        );
    }

    /// A `tracing` [`Layer`](tracing_subscriber::layer::Layer) recording every created span's name —
    /// the analogue of the Go `loop_spans_test`'s OTel `SpanRecorder`.
    struct SpanNameLayer {
        names: Arc<Mutex<Vec<String>>>,
    }

    impl<S> tracing_subscriber::layer::Layer<S> for SpanNameLayer
    where
        S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    {
        fn on_new_span(
            &self,
            attrs: &tracing::span::Attributes<'_>,
            _id: &tracing::span::Id,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            self.names
                .lock()
                .expect("span names lock")
                .push(attrs.metadata().name().to_string());
        }
    }

    // The O7 slice of Go `loop_spans_test.go`'s `TestControlLoopSpans`: one `on_tick` records the
    // control-loop spans O7 OWNS — `symphony.poll` + `symphony.fetch_candidates` — as `tracing` spans.
    // The sibling `symphony.reconcile` / `symphony.dispatch` / `symphony.run` spans + the OTel export
    // are P6 (O2/O3/O5 deferred them; see the module docs) and are asserted by the ignored full mirror.
    #[tokio::test]
    async fn control_loop_emits_poll_and_fetch_spans() {
        use tracing_subscriber::layer::SubscriberExt;
        let mut tr = Fake::new();
        tr.candidates = vec![issue("1", "MT-1", "Todo")];
        let (mut o, _spawned) = new_loop_orch(tr, Duration::from_secs(3600)); // don't re-arm during the test
        o.ctx = Some(CancelWait::default());
        o.spawn = Some(Box::new(|_iss, _attempt, _re| {})); // no worker send — just observe the spans

        let _serial = crate::testsupport::TRACING_TEST_LOCK.lock().await; // TRA-243
        let names = Arc::new(Mutex::new(Vec::<String>::new()));
        let guard =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(SpanNameLayer {
                names: Arc::clone(&names),
            }));
        // TRA-243: the control-loop span callsites (`symphony.poll` / `symphony.fetch_candidates`) are
        // shared with other tests that drive `on_tick` WITHOUT installing a subscriber. In tracing-core
        // a callsite's Interest is cached ONCE, computed from whichever thread first *registers* it
        // (`callsite::register` → `rebuild_callsite_interest`; with no global default set, that uses the
        // registering thread's default subscriber). If a parallel no-subscriber test registers the
        // callsite first it caches `Interest::never`, and this recording subscriber is then never
        // consulted → the capture comes back empty (the flake). `rebuild_interest_cache` only recomputes
        // callsites that are ALREADY registered, so a lone pre-run rebuild can't rescue a not-yet-hit
        // callsite. Fix: run one throwaway warm-up tick to force these callsites to register, THEN
        // rebuild against THIS thread's subscriber (→ enabled), THEN capture a clean tick. The held lock
        // keeps any other subscriber test from rebuilding concurrently, so the pin is stable.
        o.on_tick().await;
        if let Some(t) = o.tick_timer.take() {
            t.abort(); // warm-up re-armed the poll timer; stop it
        }
        tracing::callsite::rebuild_interest_cache();
        names.lock().expect("span names lock").clear();

        o.on_tick().await;
        if let Some(t) = o.tick_timer.take() {
            t.abort(); // captured tick re-armed the poll timer; stop it
        }
        drop(guard);

        let recorded = names.lock().expect("span names lock");
        assert!(
            recorded.iter().any(|n| n == "symphony.poll"),
            "expected the symphony.poll control-loop span, got {recorded:?}"
        );
        assert!(
            recorded.iter().any(|n| n == "symphony.fetch_candidates"),
            "expected the symphony.fetch_candidates control-loop span, got {recorded:?}"
        );
    }

    // The full Go `TestControlLoopSpans` also asserts `symphony.reconcile` (reconcile's own root) +
    // `symphony.dispatch`, and `TestRunLinksToDispatch` asserts the `symphony.run` span LINKS to its
    // dispatch span. Those spans + OTel span-links live in the reconcile / dispatch / worker tickets,
    // which deferred all telemetry to P6 (see `worker.rs` / `retry.rs` docs). Un-ignored when P6 wires
    // the OTel bridge + those spans.
    #[tokio::test]
    #[ignore = "telemetry P6: symphony.reconcile/dispatch/run spans + OTel span-links (O2/O3/O5 deferred them)"]
    async fn control_loop_spans_full() {
        use tracing_subscriber::layer::SubscriberExt;
        let mut tr = Fake::new();
        tr.candidates = vec![issue("1", "MT-1", "Todo")];
        let (mut o, _s) = new_loop_orch(tr, Duration::from_secs(3600));
        o.ctx = Some(CancelWait::default());
        o.spawn = Some(Box::new(|_iss, _attempt, _re| {}));
        let _serial = crate::testsupport::TRACING_TEST_LOCK.lock().await; // TRA-243
        let names = Arc::new(Mutex::new(Vec::<String>::new()));
        let guard =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(SpanNameLayer {
                names: Arc::clone(&names),
            }));
        // TRA-243: warm up to force callsite registration, then rebuild against this thread's
        // subscriber, then capture — see `control_loop_emits_poll_and_fetch_spans` for the full rationale.
        o.on_tick().await;
        if let Some(t) = o.tick_timer.take() {
            t.abort();
        }
        tracing::callsite::rebuild_interest_cache();
        names.lock().expect("span names lock").clear();

        o.on_tick().await;
        if let Some(t) = o.tick_timer.take() {
            t.abort();
        }
        drop(guard);
        let recorded = names.lock().expect("span names lock");
        for want in [
            "symphony.reconcile",
            "symphony.poll",
            "symphony.fetch_candidates",
            "symphony.dispatch",
        ] {
            assert!(
                recorded.iter().any(|n| n == want),
                "expected control-loop span {want:?}"
            );
        }
    }

    // Mirrors Go `reconcile_multi_test.go`'s `TestStartupCleanupMultiPerProject` (which exercises
    // `startupCleanup`, living in O7's `loop.go`): each project's slug-bound tracker + its own
    // terminal-state list drive a per-project terminal-workspace cleanup.
    #[tokio::test(flavor = "multi_thread")]
    async fn startup_cleanup_multi_per_project() {
        use crate::testsupport::{TempDir, empty_resolved_project, mk_workspace};
        let mut fa = Fake::new();
        fa.by_state = std::collections::HashMap::from([(
            "done".to_string(),
            vec![issue("a1", "A-1", "Done")],
        )]);
        let mut fb = Fake::new();
        fb.by_state = std::collections::HashMap::from([(
            "shipped".to_string(),
            vec![issue("b1", "B-1", "Shipped")],
        )]);
        let tr_a = Arc::new(fa);
        let tr_b = Arc::new(fb);
        let dir_a = TempDir::new();
        let dir_b = TempDir::new();
        let ws_a = mk_workspace(&dir_a.path);
        let ws_b = mk_workspace(&dir_b.path);

        let mut o = Orchestrator::new("WORKFLOW.md");
        o.ctx = Some(CancelWait::default());
        // mcfg carries each project's terminal-state list (startup_cleanup reads it).
        let mut pa = empty_resolved_project("a", Arc::clone(&tr_a) as Arc<dyn Tracker>);
        pa.workspace = Arc::clone(&ws_a);
        pa.mcfg.tracker.terminal_states = vec!["Done".to_string()];
        let mut pb = empty_resolved_project("b", Arc::clone(&tr_b) as Arc<dyn Tracker>);
        pb.workspace = Arc::clone(&ws_b);
        pb.mcfg.tracker.terminal_states = vec!["Shipped".to_string()];
        let mut eff = empty_effective(Arc::new(Fake::new()));
        eff.max_concurrent = 10;
        eff.projects = vec![pa, pb];
        o.eff = Some(eff);

        let wsa = ws_a.create_for_issue("", "A-1").await.expect("create A");
        let wsb = ws_b.create_for_issue("", "B-1").await.expect("create B");

        o.startup_cleanup().await;

        assert!(
            std::fs::metadata(&wsa.path).is_err(),
            "project A terminal workspace should be removed"
        );
        assert!(
            std::fs::metadata(&wsb.path).is_err(),
            "project B terminal workspace should be removed"
        );
        assert_eq!(
            tr_a.by_state_calls(),
            1,
            "project A tracker fetch_issues_by_states should be called once"
        );
        assert_eq!(
            tr_b.by_state_calls(),
            1,
            "project B tracker fetch_issues_by_states should be called once"
        );
    }

    // --- loop_multi_test.go: onTick's multi-project poll → dedup → dispatch fan-out (O8 e2e) ------
    //
    // These drive the assembled `on_tick` control pass against N slug-bound fake trackers and a
    // recording spawn that captures the dispatched running entries (Go `newMultiOrch`'s
    // `*[]*runningEntry`), asserting the project stamping / disabled-skip / per-project-error /
    // dedup / legacy-path semantics `dispatch_decisions` + `poll_all_projects` encode.

    // Mirrors Go `TestOnTickPollsAllProjectsAndDispatches`: onTick polls EVERY resolved project's
    // slug-bound tracker exactly once and dispatches each candidate stamped with its owning project.
    #[tokio::test(flavor = "multi_thread")]
    async fn on_tick_polls_all_projects_and_dispatches() {
        let mut ta = Fake::new();
        ta.candidates = vec![issue("a1", "A-1", "Todo")];
        let mut tb = Fake::new();
        tb.candidates = vec![issue("b1", "B-1", "Todo")];
        let ta = Arc::new(ta);
        let tb = Arc::new(tb);
        let (mut o, spawned) = orch_for_retry_multi(
            vec![
                proj_with_tracker("a", Arc::clone(&ta), "promptA"),
                proj_with_tracker("b", Arc::clone(&tb), "promptB"),
            ],
            10,
        );
        o.on_tick().await;
        if let Some(t) = o.tick_timer.take() {
            t.abort();
        }
        let entries = spawned.lock().expect("dispatched lock");
        assert_eq!(entries.len(), 2, "both projects' issues should dispatch");
        let a1 = entries.iter().find(|re| re.issue.id == "a1");
        let b1 = entries.iter().find(|re| re.issue.id == "b1");
        assert_eq!(
            a1.map(|re| re.project_slug.as_str()),
            Some("a"),
            "a1 should be stamped with project a"
        );
        assert_eq!(
            b1.map(|re| re.project_slug.as_str()),
            Some("b"),
            "b1 should be stamped with project b"
        );
        assert_eq!(ta.candidate_calls(), 1, "project a polled once");
        assert_eq!(tb.candidate_calls(), 1, "project b polled once");
    }

    // Mirrors Go `TestOnTickSkipsDisabledProject`: a paused project (enabled:false) is never polled.
    #[tokio::test(flavor = "multi_thread")]
    async fn on_tick_skips_disabled_project() {
        let mut ta = Fake::new();
        ta.candidates = vec![issue("a1", "A-1", "Todo")];
        let mut tb = Fake::new();
        tb.candidates = vec![issue("b1", "B-1", "Todo")];
        let ta = Arc::new(ta);
        let tb = Arc::new(tb);
        let pa = proj_with_tracker("a", Arc::clone(&ta), "promptA");
        let mut pb = proj_with_tracker("b", Arc::clone(&tb), "promptB");
        pb.disabled = true; // project B is paused (enabled:false in config)
        let (mut o, spawned) = orch_for_retry_multi(vec![pa, pb], 10);
        o.on_tick().await;
        if let Some(t) = o.tick_timer.take() {
            t.abort();
        }
        let entries = spawned.lock().expect("dispatched lock");
        assert_eq!(entries.len(), 1, "only enabled project A should dispatch");
        assert_eq!(entries[0].issue.id, "a1");
        assert_eq!(
            tb.candidate_calls(),
            0,
            "disabled project B must NOT be polled"
        );
        assert_eq!(ta.candidate_calls(), 1, "enabled project A polled once");
    }

    // Mirrors Go `TestOnTickProjectFetchErrorSkipsOnlyThatProject`: a per-project fetch error logs
    // and skips only that project; the sibling still dispatches.
    #[tokio::test(flavor = "multi_thread")]
    async fn on_tick_project_fetch_error_skips_only_that_project() {
        let mut ta = Fake::new();
        ta.candidates_err = Some(TrackerError::Other("boom".to_string()));
        let mut tb = Fake::new();
        tb.candidates = vec![issue("b1", "B-1", "Todo")];
        let (mut o, spawned) = orch_for_retry_multi(
            vec![
                proj_with_tracker("a", Arc::new(ta), "promptA"),
                proj_with_tracker("b", Arc::new(tb), "promptB"),
            ],
            10,
        );
        o.on_tick().await;
        if let Some(t) = o.tick_timer.take() {
            t.abort();
        }
        let entries = spawned.lock().expect("dispatched lock");
        assert_eq!(
            entries.len(),
            1,
            "only project B's issue should dispatch when A errors"
        );
        assert_eq!(entries[0].issue.id, "b1");
    }

    // Mirrors Go `TestOnTickDedupByIDFirstWins`: a duplicate issue id across projects dispatches once
    // and the FIRST project wins the dedup.
    #[tokio::test(flavor = "multi_thread")]
    async fn on_tick_dedup_by_id_first_wins() {
        let shared = issue("x1", "X-1", "Todo");
        let mut ta = Fake::new();
        ta.candidates = vec![shared.clone()];
        let mut tb = Fake::new();
        tb.candidates = vec![shared];
        let (mut o, spawned) = orch_for_retry_multi(
            vec![
                proj_with_tracker("a", Arc::new(ta), "promptA"),
                proj_with_tracker("b", Arc::new(tb), "promptB"),
            ],
            10,
        );
        o.on_tick().await;
        if let Some(t) = o.tick_timer.take() {
            t.abort();
        }
        let entries = spawned.lock().expect("dispatched lock");
        assert_eq!(
            entries.len(),
            1,
            "a duplicate issue id should dispatch once"
        );
        assert_eq!(
            entries[0].project_slug, "a",
            "first project (a) should win the dedup"
        );
    }

    // Mirrors Go `TestOnTickSingleProjectLegacyPathWhenProjectsNil`: projects == nil + eff.tracker set
    // ⇒ the legacy single-tracker path dispatches with an empty project slug.
    #[tokio::test(flavor = "multi_thread")]
    async fn on_tick_single_project_legacy_path_when_projects_nil() {
        let mut tr = Fake::new();
        tr.candidates = vec![issue("1", "MT-1", "Todo")];
        let tr = Arc::new(tr);
        let tr_dyn: Arc<dyn Tracker> = tr.clone();
        let mut eff = empty_effective(tr_dyn);
        eff.active_states = set_of(&["todo"]);
        eff.terminal_states = set_of(&["done"]);
        eff.max_concurrent = 10;
        eff.poll_interval = Duration::from_secs(3600);
        eff.max_retry_backoff_ms = 300_000;
        let mut o = Orchestrator::new("WORKFLOW.md");
        o.eff = Some(eff);
        let sink: DispatchedEntries = Arc::new(Mutex::new(Vec::new()));
        o.spawn = Some(record_entries(&sink));
        o.on_tick().await;
        if let Some(t) = o.tick_timer.take() {
            t.abort();
        }
        let entries = sink.lock().expect("dispatched lock");
        assert_eq!(
            entries.len(),
            1,
            "legacy single-tracker path should dispatch the candidate"
        );
        assert_eq!(entries[0].issue.id, "1");
        assert_eq!(
            entries[0].project_slug, "",
            "legacy path should leave projectSlug empty"
        );
        assert_eq!(tr.candidate_calls(), 1, "legacy tracker polled once");
    }
}
