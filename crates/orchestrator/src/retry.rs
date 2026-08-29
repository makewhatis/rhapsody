//! retry — parity port of Go `internal/orchestrator/retry.go` (the retry queue + worker-exit
//! classification, upstream §8.4 / §16.6).
//!
//! [`Orchestrator::dispatch_issue`] claims an issue, records a running entry, and hands it to the
//! worker-spawn seam; [`Orchestrator::on_worker_exit`] classifies a clean exit (continuation vs
//! release, taxonomy v2 / INF-266 / INF-272) or schedules exponential failure backoff; and
//! [`Orchestrator::on_retry`] re-resolves a fired retry against the current effective set — re-fetching
//! from the right project tracker, relocating still-in-flight work that dropped out of the narrowed
//! candidate set, and re-checking per-project / per-state / global slots — then re-dispatches,
//! requeues, or releases the claim.
//!
//! Deviations from the Go source, all behavior-preserving:
//!   * The live retry TIMER (Go `time.AfterFunc` → `o.events <- evRetry`) and the worker-spawn body
//!     (Go `spawnWorker`) are O7's — they need the control-event channel + task abort handle the loop
//!     owns (see the `worker.rs` module docs). O5 records the retry DECISION + due time
//!     ([`RetryEntry`], persisted) and dispatches through the injectable [`spawn`](Orchestrator::spawn)
//!     seam; O7 arms the live timer against `due_at_ms` and wires the real worker.
//!   * `on_retry` snapshots the effective config it schedules against into owned locals ([`RetryConfig`])
//!     before the tracker `await` and the state mutations — Go aliases the `resolvedProject` via a
//!     pointer, which Rust's borrow checker forbids across `&mut self`. The observable decisions are
//!     identical.
//!   * `schedule_retry_for` derives the issue id + identifier from its `iss` argument: every Go call
//!     site passes `id == iss.ID` and `identifier == iss.Identifier`, so the two redundant parameters
//!     are dropped (keeping the arity idiomatic).
//!   * Telemetry (Go `o.tracer`/`o.metrics`) is P6 and dropped; diagnostics go through `tracing`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use chrono::Duration;
use rhapsody_core::{Issue, normalize_state};
use rhapsody_store as store;
use rhapsody_tracker::{Tracker, TrackerError};

use crate::backoff::{CONTINUATION_DELAY_MS, failure_backoff_ms};
use crate::concurrency::{global_slots, state_limit};
use crate::control_loop::{CancelSignal, Event};
use crate::dispatch::{EligibilityGate, eligible};
use crate::effective::{Effective, ResolvedProject};
use crate::orchestrator::{
    Orchestrator, RetryEntry, RunningEntry, find_by_id, find_by_identifier, normalize_attempt,
};

/// A fired retry timer for one issue (Go `evRetry`). The control loop (O7) delivers it to
/// [`Orchestrator::on_retry`]; O5's tests construct it directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvRetry {
    pub issue_id: String,
}

/// A worker task's terminal report (Go `evWorkerExit`). The control loop (O7) delivers it to
/// [`Orchestrator::on_worker_exit`]; O5's tests construct it directly.
#[derive(Debug, Clone, PartialEq)]
pub struct EvWorkerExit {
    pub issue_id: String,
    pub failed: bool,
    pub started_at: chrono::DateTime<chrono::Utc>,
    /// The real (truncated) failure reason from the worker — the git/clone/SSH error, claude startup
    /// failure, or turn error — persisted onto the run. Empty on success.
    pub err_msg: String,
    /// The worker's last-known issue state (refreshed after every turn); `on_worker_exit` classifies
    /// the clean-exit outcome from it (INF-266). May be the dispatch-time snapshot if the run failed
    /// before any turn completed.
    pub last_state: String,
    /// True when the agent's final result text ended with a `HANDOFF:` line. A clean exit into a
    /// non-terminal, non-active state records `completed` only when declared, else `stopped`. (INF-272)
    pub declared_handoff: bool,
}

/// The ceiling on an armed retry delay. A real retry is minutes-scale by construction (the 1s
/// continuation cadence, or `max_retry_backoff_ms`), so a longer delay can only come from a corrupt
/// `due_at_ms` read back from the store — an unbounded `INTEGER` column. Handing such a value to
/// `tokio::time::sleep` would overflow the `Instant` it is added to and panic the control task, so it
/// is clamped here instead: firing a nonsense row EARLY is safe, because
/// [`Orchestrator::on_retry`] re-validates it against live Linear and releases the claim if the issue
/// is no longer eligible, whereas never firing is the TRA-316 deadlock itself.
const MAX_RETRY_DELAY_MS: i64 = 7 * 24 * 60 * 60 * 1000; // 7 days

/// The stable identity + project routing for a scheduled retry. Go passes these as four positional
/// args to `scheduleRetryFor` (`id`, `identifier`, `slug`, `repo`); they are bundled here so the
/// method keeps an idiomatic arity. `id` is the opaque map key (== the running/event id), `identifier`
/// is the store PK; they DIFFER from the retry's last-known `iss` (which may be the zero issue on a
/// poll-failure reschedule), so they cannot be derived from it.
pub(crate) struct RetryTarget<'a> {
    pub id: &'a str,
    pub identifier: &'a str,
    pub project_slug: &'a str,
    pub project_repo: &'a str,
}

/// The owning resolved project's routing snapshot, passed by value to [`Orchestrator::dispatch_issue`]
/// (Go passes a `*resolvedProject`; the Rust borrow checker forbids holding that borrow across the
/// `&mut self` dispatch, so the fields the dispatch stamps are cloned out).
pub(crate) struct DispatchRoute {
    pub slug: String,
    pub group: String,
    pub repo: String,
    pub model: String,
    /// The owning project's `workspace_mode`, so a graphite auto-promote's stashed stacking hint is
    /// rendered at dispatch with THIS project's provisioning shape (Go `rp.workspaceMode`; INF-318 /
    /// INF-418). Empty for the legacy single-project path (dispatch falls back to the top-level mode).
    pub workspace_mode: String,
}

/// The effective config a fired retry schedules against, snapshotted into owned values so no borrow
/// of `self.eff` is held across the tracker `await` / the state mutations in [`Orchestrator::on_retry`]
/// (Go aliases these via the `resolvedProject`/`effective` pointers).
struct RetryConfig {
    tracker: Arc<dyn Tracker>,
    active: HashSet<String>,
    terminal: HashSet<String>,
    mode: String,
    review: HashSet<String>,
    canceled: HashSet<String>,
    per_state: HashMap<String, i64>,
    /// The GLOBAL concurrency cap (always the top-level `max_concurrent`, even for a routed retry —
    /// mirrors Go's `globalCap := o.eff.maxConcurrent`).
    global_cap: i64,
    /// The per-project cap (the project's `max_concurrent`, else the global cap for a legacy retry).
    project_cap: i64,
    max_retry_backoff_ms: i64,
    /// Routing for the eventual dispatch; `None` for a legacy single-project retry.
    route: Option<DispatchRoute>,
    /// The owning project's group key for the per-project slot check; `None` for a legacy retry.
    rp_group: Option<String>,
}

impl RetryConfig {
    /// The legacy single-project / test-injected snapshot: the top-level effective sets, no routing.
    fn from_top_level(eff: &Effective) -> RetryConfig {
        RetryConfig {
            tracker: Arc::clone(&eff.tracker),
            active: eff.active_states.clone(),
            terminal: eff.terminal_states.clone(),
            mode: eff.dependency_mode.clone(),
            review: eff.review_states.clone(),
            canceled: eff.canceled_states.clone(),
            per_state: eff.per_state_limits.clone(),
            global_cap: eff.max_concurrent,
            project_cap: eff.max_concurrent,
            max_retry_backoff_ms: eff.max_retry_backoff_ms,
            route: None,
            rp_group: None,
        }
    }

    /// The routed snapshot: the project's slug-bound tracker + its active/terminal/… sets and
    /// per-project cap. The global cap + retry backoff stay top-level (Go: `globalCap :=
    /// o.eff.maxConcurrent`, `maxRetryBackoffMS` is not per-project).
    fn from_project(eff: &Effective, rp: &ResolvedProject) -> RetryConfig {
        RetryConfig {
            tracker: Arc::clone(&rp.tracker),
            active: rp.active_states.clone(),
            terminal: rp.terminal_states.clone(),
            mode: rp.dependency_mode.clone(),
            review: rp.review_states.clone(),
            canceled: rp.canceled_states.clone(),
            per_state: rp.per_state_limits.clone(),
            global_cap: eff.max_concurrent,
            project_cap: rp.max_concurrent,
            max_retry_backoff_ms: eff.max_retry_backoff_ms,
            route: Some(DispatchRoute {
                slug: rp.slug.clone(),
                group: rp.group.clone(),
                repo: rp.repo.clone(),
                model: rp.model.clone(),
                workspace_mode: rp.workspace_mode.clone(),
            }),
            rp_group: Some(rp.group.clone()),
        }
    }
}

/// Routing verdict for a fired retry: schedule against a config, or release the claim (project gone /
/// paused).
enum RetryRouting {
    /// Release the claim; the payload is the log reason.
    Release(&'static str),
    Config(Box<RetryConfig>),
}

/// The outcome of relocating an in-flight issue absent from the candidate set (Go `recheckInFlight`'s
/// `(iss, gone, err)` return; the error case is the `Err` arm of the `Result`).
enum Recheck {
    /// Still present and non-terminal → continue with the last-known issue at the refreshed state.
    /// Boxed to keep the variant size down (the [`Issue`] is large; `Gone` is empty).
    Relocated(Box<Issue>),
    /// Terminal or genuinely absent → release.
    Gone,
}

/// The configured review state a clean exit left the ticket parked in — but only when that is what
/// [`classify_clean_exit`]'s review branch actually keys on. A state that is ALSO terminal is decided
/// by the earlier terminal branch (and a cancel-type state is terminal by definition), so a terminal
/// sample yields `None`. The sibling branches test both samples with a plain `||` and need no
/// tie-break because they return constants; this one returns a value, so when BOTH samples are review
/// states it reports the worker's own per-turn refresh — the sample the agent itself last observed.
/// Both state arguments must already be [`normalize_state`]d.
///
/// Shared by the classifier and [`Orchestrator::on_worker_exit`]'s undeclared-hand-off warning so the
/// two can never disagree about what counts as "parked in review" (TRA-279).
fn parked_review_state<'a>(
    review: &HashSet<String>,
    terminal: &HashSet<String>,
    w_st: &'a str,
    s_st: &'a str,
) -> Option<&'a str> {
    if terminal.contains(w_st) || terminal.contains(s_st) {
        return None;
    }
    if review.contains(w_st) {
        return Some(w_st);
    }
    if review.contains(s_st) {
        return Some(s_st);
    }
    None
}

/// Maps a clean worker exit to its stored outcome (taxonomy v2, INF-272) from the two freshest
/// ticket-state samples (the worker's per-turn refresh + reconcile's snapshot; either may be newer —
/// INF-266) plus whether the agent declared hand-off. The ticket is treated as having LEFT the active
/// set if EITHER source reports a non-active state. Cancel-type requires the state to ALSO be terminal
/// (a `canceled_states` entry that isn't terminal must not hijack classification); a cancel-type
/// sample wins over a Done-type sample. `release == false` means the segment is `continued` and the
/// claim is kept for the continuation. Mirrors Go `classifyCleanExit`.
///
/// DIVERGENCE from Go v0.4.0 (TRA-279): Go's classifier never receives `review_states`, so a run whose
/// agent followed its prompt — open the PR, move the ticket to review — and then ended a turn without
/// emitting a `HANDOFF:` marker fell through to the catch-all and was recorded
/// `stopped` / "ticket moved externally", blaming an external actor for the agent's own move. The
/// `review` branch below sits AFTER `declared` and BEFORE the catch-all, so behavior changes only when
/// the state is genuinely in the configured review set; with `review_states` unset (the Go default)
/// classification is byte-identical to the reference.
pub(crate) fn classify_clean_exit(
    active: &HashSet<String>,
    canceled: &HashSet<String>,
    terminal: &HashSet<String>,
    review: &HashSet<String>,
    declared: bool,
    worker_state: &str,
    snap_state: &str,
) -> (String, bool, String) {
    let w_st = normalize_state(worker_state);
    let s_st = normalize_state(snap_state);
    let worker_left = !worker_state.is_empty() && !active.contains(&w_st);
    let snap_left = !snap_state.is_empty() && !active.contains(&s_st);
    if !worker_left && !snap_left {
        return (store::OUTCOME_CONTINUED.to_string(), false, String::new());
    }
    if (canceled.contains(&w_st) && terminal.contains(&w_st))
        || (canceled.contains(&s_st) && terminal.contains(&s_st))
    {
        return (
            store::OUTCOME_STOPPED.to_string(),
            true,
            "ticket cancelled".to_string(),
        );
    }
    if terminal.contains(&w_st) || terminal.contains(&s_st) {
        return (store::OUTCOME_COMPLETED.to_string(), true, String::new());
    }
    if declared {
        return (store::OUTCOME_COMPLETED.to_string(), true, String::new());
    }
    if parked_review_state(review, terminal, &w_st, &s_st).is_some() {
        // The ticket is parked in a configured review state — the expected end state for a
        // review-gated run, whether the agent declared hand-off or moved it itself. Not external.
        return (store::OUTCOME_COMPLETED.to_string(), true, String::new());
    }
    (
        store::OUTCOME_STOPPED.to_string(),
        true,
        "ticket moved externally".to_string(),
    )
}

impl Orchestrator {
    /// Claims the issue, records a running entry, and hands it to the worker-spawn seam (upstream
    /// §16.4). `route` is the owning resolved project's snapshot (`None` => legacy single-project /
    /// test-injected path); `stack_context` is the graphite-mode predecessor stacking hint (INF-318),
    /// `""` for every dispatch except a graphite auto-promote. Mirrors Go `dispatchIssue`.
    pub(crate) fn dispatch_issue(
        &mut self,
        iss: Issue,
        attempt: Option<i64>,
        route: Option<DispatchRoute>,
        stack_context: String,
    ) {
        // A graphite auto-promote stashed a predecessor stacking hint for this issue's first dispatch
        // (it moved the ticket Backlog→Todo and left the slot-accounted dispatch to the select path).
        // Consume it when the caller didn't pass one explicitly, rendering the workspace_mode-aware
        // recipe HERE with the DISPATCH-time effective mode (this project's when routed, else the
        // top-level/legacy default), so a workspace_mode flip between promote and dispatch can never
        // produce a recipe for the wrong provisioning shape (INF-318 / INF-418). A retry never hits
        // this (its issue's hint was consumed at first dispatch, so `pending_stack` is empty for it).
        let mut stack_context = stack_context;
        if stack_context.is_empty()
            && let Some(h) = self.pending_stack.remove(&iss.id)
        {
            let ws_mode = match &route {
                Some(r) => r.workspace_mode.clone(),
                None => self
                    .eff
                    .as_ref()
                    .map_or_else(String::new, |e| e.workspace_mode.clone()),
            };
            stack_context = crate::promote::stack_context_hint(&h.branch, h.pr_number, &ws_mode);
        }
        // Additive: project defaults ∪ this ticket's rhapsody:* labels.
        // A capability the registry doesn't recognize is a no-op, never a failure.
        let capability_names: Vec<String> = {
            let mut names: Vec<String> = match &route {
                Some(r) => self
                    .eff
                    .as_ref()
                    .and_then(|e| e.project_by_slug(&r.slug))
                    .map(|p| p.capabilities.clone())
                    .unwrap_or_default(),
                None => self
                    .eff
                    .as_ref()
                    .map(|e| e.capabilities.clone())
                    .unwrap_or_default(),
            };
            for l in iss.labels.iter().flatten() {
                if let Some(name) = l.strip_prefix("rhapsody:")
                    && !names.iter().any(|n| n == name)
                {
                    names.push(name.to_string());
                }
            }
            names
        };
        let capabilities_section = self
            .capabilities_registry
            .as_ref()
            .map(|reg| rhapsody_config::capabilities::render_section(&capability_names, reg))
            .unwrap_or_default();
        // Rhapsody Teams routing (STUDIO-643, T3a; design record
        // `~/.rhapsody/docs/STUDIO-572-rhapsody-teams.md`, §3.1). Called ONCE, HERE — after the
        // issue was selected and its slot taken, so routing can only ever DECORATE a run that is
        // already going to happen. `None` (Teams off, or §3.5's `mode: off` with no default) leaves
        // every field below untouched and this dispatch byte-identical to today. Sync and pure: the
        // decision is `crate::teams::route`, plain data in, no I/O and above all no network call on
        // the single control task (§0.11.2).
        // NB: `route` (the parameter) is the owning PROJECT's snapshot — an unrelated sense of the
        // word. `teams_dispatch` is named in full so the two cannot be conflated at a glance.
        let teams_dispatch = self.route_teams(&iss);
        let attempt_norm = normalize_attempt(attempt);
        let mut re = RunningEntry::empty(iss.clone());
        re.started_at = (self.now)();
        re.retry_attempt = attempt_norm;
        re.stack_context = stack_context;
        re.capabilities_section = capabilities_section;
        if let Some(td) = &teams_dispatch {
            re.identity = td.identity.clone();
            re.teammate_section = td.section.clone();
        }
        // Bounded telemetry label, stamped at dispatch (Go `re.model = o.modelFor(rp)`): the routed
        // project's model, else the top-level effective claude model.
        re.model = match &route {
            Some(r) => r.model.clone(),
            None => self
                .eff
                .as_ref()
                .map_or_else(String::new, |e| e.cfg.claude.model.clone()),
        };
        if let Some(r) = &route {
            re.project_slug = r.slug.clone();
            // The per-project cap is counted across the whole project group; fall back to the slug when
            // a project carries no group (legacy single-project synthesis already sets group == slug).
            re.project_group = if r.group.is_empty() {
                r.slug.clone()
            } else {
                r.group.clone()
            };
            re.project_repo = r.repo.clone();
        }
        // Arm the worker's cancellation before the spawn observes it (Go `wctx, cancel :=
        // context.WithCancel(o.ctx)` + `re.cancel = cancel`); `terminate` / `shutdown` fire it.
        re.cancel = CancelSignal::new();
        let id = iss.id.clone();
        self.claimed.insert(id.clone());
        self.clear_retry(&id);
        self.persist_start_run(&mut re, attempt_norm);
        // AFTER the run row exists: the decision is a row ON a run, so it cannot be written before
        // one (§3.1 — the router has no store and can persist no intention). A zero `run_id` (store
        // disabled / `start_run` failed) makes this a no-op.
        if let Some(td) = &teams_dispatch {
            self.record_route_event(&mut re, td);
        }
        // Per-run operator-message mailbox (INF-250): buffered so a brief delivery lag doesn't reject
        // an operator's message; a full mailbox (`OPERATOR_MAILBOX_CAP`) rejects backlog_full. Go
        // carries this channel on the running entry; the Rust `mpsc` split makes it a side map keyed by
        // issue id (`message.rs`). O7's real spawn takes its receiver; `persist_end_run` drops it.
        self.mailboxes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id.clone(), crate::message::Mailbox::new());
        let cancel = re.cancel.wait();
        // Snapshot the routing + started_at the real spawn needs before `re` moves into `o.running`.
        let project_slug = re.project_slug.clone();
        let stack_context = re.stack_context.clone();
        let capabilities_section = re.capabilities_section.clone();
        let teammate_section = re.teammate_section.clone();
        let started_at = re.started_at;
        // Hand the entry to the injected TEST seam (Go `o.spawn(...)`) BEFORE moving it into
        // `o.running` — the borrow of `self.spawn` must not alias `self.running`. In production
        // (`self.spawn` is `None`) the real worker is launched AFTER the insert via `spawn_worker`,
        // which reads `o.eff` / `o.events` / `o.wg` off `&self`.
        if let Some(spawn) = &self.spawn {
            spawn(&iss, attempt, &re);
        }
        let production = self.spawn.is_none();
        self.running.insert(id, re);
        if production {
            self.spawn_worker(
                cancel,
                iss,
                attempt,
                project_slug,
                stack_context,
                capabilities_section,
                teammate_section,
                started_at,
            );
        }
    }

    /// Removes any pending retry entry for an issue AND aborts its live timer (Go `clearRetry` stops
    /// `retryEntry.timer`; the Rust timer task is held in [`retry_timers`](Orchestrator::retry_timers)).
    pub(crate) fn clear_retry(&mut self, id: &str) {
        self.retry_attempts.remove(id);
        if let Some(timer) = self.retry_timers.remove(id) {
            timer.abort();
        }
    }

    /// Arms the live retry timer for `key`, firing [`Event::Retry`] with `issue_id: key` after
    /// `delay_ms` clamped to `[0, MAX_RETRY_DELAY_MS]` (Go `time.AfterFunc(delay, () => o.events <-
    /// evRetry{key})`).
    ///
    /// THE ONLY place a retry timer is armed — every arming path (runtime [`schedule_retry_for`], boot-recovery
    /// [`re_arm_retry`](Orchestrator::re_arm_retry) / [`arm_immediate_retry`](Orchestrator::arm_immediate_retry),
    /// and [`requeue_recovered`](Orchestrator::requeue_recovered)) goes through here, so a new path
    /// cannot silently ship an inert retry (TRA-316: the recovery paths recorded the entry but armed
    /// nothing, so the retry never fired while its claim kept `select_dispatch` skipping the issue —
    /// a permanent deadlock a restart could not clear).
    ///
    /// `key` MUST be the string the [`RetryEntry`] is keyed by in
    /// [`retry_attempts`](Orchestrator::retry_attempts): the opaque issue id for a live entry, the
    /// IDENTIFIER for a boot-recovered one (whose `issue_id` is empty until the first fire resolves
    /// it). The timer key, the entry key, and the fired [`EvRetry::issue_id`] must all agree or
    /// [`on_retry`](Orchestrator::on_retry) looks up an entry that isn't there and drops the retry.
    ///
    /// Arms ONLY when the daemon is live — `o.ctx` is set exactly when a control loop is running to
    /// receive the fire. The off-loop unit tests drive `on_retry` / `on_worker_exit` directly with a
    /// nil `o.ctx` and no runtime to spawn onto; they assert on `retry_attempts`, not the timer.
    pub(crate) fn arm_retry_timer(&mut self, key: &str, delay_ms: i64) {
        // Never strand a timer already armed for this key (a re-arm of the same entry): the replaced
        // `JoinHandle` would otherwise be dropped without aborting, leaving a detached task alive.
        if let Some(prev) = self.retry_timers.remove(key) {
            prev.abort();
        }
        let Some(mut ctx) = self.ctx.clone() else {
            return; // off-loop: nothing is receiving, and there is no runtime to spawn onto
        };
        let events = self.events.clone();
        let fire_id = key.to_string();
        // A past-due entry (every boot-recovered `arm_immediate_retry`, and any row whose `due_at`
        // elapsed while the daemon was down) clamps to zero and fires immediately, never underflows.
        // The upper clamp bounds a `due_at_ms` read back from the store — see `MAX_RETRY_DELAY_MS`.
        let delay = std::time::Duration::from_millis(delay_ms.clamp(0, MAX_RETRY_DELAY_MS) as u64);
        let timer = tokio::spawn(async move {
            tokio::select! {
                _ = tokio::time::sleep(delay) => {
                    let _ = events.send(Event::Retry(EvRetry { issue_id: fire_id }));
                }
                _ = ctx.cancelled() => {}
            }
        });
        self.retry_timers.insert(key.to_string(), timer);
    }

    /// (Re)schedules a retry for `t.id` after `delay_ms` (upstream §8.4) with explicit project routing,
    /// so the retry re-fetches from the right slug and re-checks the right per-project caps. Records the
    /// [`RetryEntry`] (with the wall-clock due time + the last-known `iss`), keeps the claim, and
    /// persists the retry row + `claim=retry_queued` so a restart re-arms it. Mirrors Go
    /// `scheduleRetryFor` (the live timer is O7's, armed against `due_at_ms`).
    pub(crate) fn schedule_retry_for(
        &mut self,
        t: RetryTarget<'_>,
        attempt: i64,
        delay_ms: i64,
        err_str: &str,
        iss: Issue,
    ) {
        self.clear_retry(t.id);
        self.claimed.insert(t.id.to_string()); // remain claimed while a retry is pending
        let due_at = (self.now)() + Duration::milliseconds(delay_ms);
        let due_at_ms = due_at.timestamp_millis();
        self.retry_attempts.insert(
            t.id.to_string(),
            RetryEntry {
                issue_id: t.id.to_string(),
                identifier: t.identifier.to_string(),
                attempt,
                due_at,
                due_at_ms,
                err: err_str.to_string(),
                project_slug: t.project_slug.to_string(),
                project_repo: t.project_repo.to_string(),
                issue: iss, // last-known full issue, so a later fire can re-locate in-flight work
                recovered: false,
            },
        );
        // Arm the live retry timer, keyed by the opaque id this entry is keyed by.
        self.arm_retry_timer(t.id, delay_ms);
        // Persist the retry row (keyed by identifier) + claim=retry_queued so a restart re-arms this
        // timer and keeps the claim (Phase 4 §3.8).
        self.persist_retry(t.identifier, attempt, due_at_ms, err_str, t.project_slug);
    }

    /// Converts a worker exit into a continuation or a backoff retry (upstream §16.6). A no-op if the
    /// entry was already terminated by reconcile, or if the exit is stale (its `started_at` doesn't
    /// match the live entry — an exit from a prior dispatch of a re-dispatched issue). Mirrors Go
    /// `onWorkerExit`. A control-loop (O7) entry point.
    pub fn on_worker_exit(&mut self, e: EvWorkerExit) {
        // Stale/absent guard: only the exit whose started_at matches the live entry counts.
        let live = self
            .running
            .get(&e.issue_id)
            .is_some_and(|re| re.started_at == e.started_at);
        if !live {
            return; // already terminated/cleaned by reconcile, or a stale exit from a prior dispatch
        }
        let Some(re) = self.running.remove(&e.issue_id) else {
            return;
        };
        let dur = ((self.now)() - re.started_at)
            .num_nanoseconds()
            .unwrap_or(0) as f64
            / 1e9;
        self.totals.seconds_running += dur;
        // Re-resolve prompt-file warnings (Go `refreshPromptFileWarnings`, INF-279) so the missing-file
        // flag surfaces after the first mirror sync. Off-loop + gated on a live daemon (a no-op when
        // `o.ctx` is nil — the direct handler tests). `warnings.go` is O7's, so it is wired here.
        if let Some(eff) = self.eff.as_ref() {
            let inputs = crate::warnings::project_warn_inputs(eff);
            let checker = self.prompt_file_checker_for(eff);
            self.refresh_prompt_file_warnings(inputs, checker);
        }

        if !e.failed {
            // The two freshest state samples: the worker's per-turn refresh (e.last_state) and
            // reconcile's snapshot (re.issue.state). classify_clean_exit treats the ticket as having
            // LEFT the active set if EITHER reports a non-active state (INF-266).
            let (active, canceled, terminal, review) = self.states_for(&re);
            let (outcome, release, reason) = classify_clean_exit(
                &active,
                &canceled,
                &terminal,
                &review,
                e.declared_handoff,
                &e.last_state,
                &re.issue.state,
            );
            // The run parked the ticket in review but never confirmed it was finished. The outcome
            // stays `completed` (the review state IS the intended end state — TRA-279), but the
            // missing declaration is the diagnostic signal the old "ticket moved externally" reason
            // used to carry, so surface it here rather than inventing a new outcome. `release &&
            // !declared` with a non-terminal review sample can only be the classifier's review branch.
            if release && !e.declared_handoff {
                let (w_st, s_st) = (
                    normalize_state(&e.last_state),
                    normalize_state(&re.issue.state),
                );
                if let Some(st) = parked_review_state(&review, &terminal, &w_st, &s_st) {
                    tracing::warn!(
                        run_id = re.run_id,
                        issue_id = %e.issue_id,
                        issue_identifier = %re.issue.identifier,
                        state = %st,
                        project_slug = %re.project_slug,
                        "run ended in review state without a HANDOFF declaration; recording completed"
                    );
                }
            }
            if release {
                // Left the active set: declared hand-off (completed), external non-terminal move
                // (stopped), a Done-type terminal (completed), or a cancel-type terminal (stopped).
                // Drop the claim + continuation marker and DON'T schedule a retry.
                self.completed.remove(&e.issue_id);
                self.claimed.remove(&e.issue_id);
                self.persist_end_run(&re, &outcome, &reason);
                self.persist_complete(&re.issue.identifier);
                self.persist_totals();
                return;
            }
            // Still active (per both samples) or unknown state: a continuation segment. The EndRun
            // happens before the continuation reschedule (independent rows); the segment is `continued`.
            self.completed.insert(e.issue_id.clone());
            self.persist_end_run(&re, &outcome, &reason);
            self.persist_totals();
            self.schedule_retry_for(
                RetryTarget {
                    id: &e.issue_id,
                    identifier: &re.issue.identifier,
                    project_slug: &re.project_slug,
                    project_repo: &re.project_repo,
                },
                1,
                CONTINUATION_DELAY_MS,
                "",
                re.issue.clone(),
            );
            return;
        }
        // A non-zero err means the turn genuinely failed (git/SSH/clone, claude startup/turn error):
        // record `failed` and retry with exponential backoff regardless of the ticket's state. Drop
        // any stale continuation marker from a prior clean exit so backoff (not the 1s continuation
        // cadence) applies.
        self.completed.remove(&e.issue_id);
        let next = re.retry_attempt + 1;
        let reason = if e.err_msg.is_empty() {
            "worker failed".to_string()
        } else {
            e.err_msg.clone()
        };
        self.persist_end_run(&re, store::OUTCOME_FAILED, &reason);
        self.persist_totals();
        let max_backoff = self.eff.as_ref().map_or(0, |eff| eff.max_retry_backoff_ms);
        self.schedule_retry_for(
            RetryTarget {
                id: &e.issue_id,
                identifier: &re.issue.identifier,
                project_slug: &re.project_slug,
                project_repo: &re.project_repo,
            },
            next,
            failure_backoff_ms(next, max_backoff),
            &reason,
            re.issue.clone(),
        );
    }

    /// Handles a fired retry timer (upstream §16.6). Re-resolves the retry's project against the
    /// CURRENT effective set (a hot-reload may have removed/paused it), re-fetches candidates from that
    /// project's slug-bound tracker, relocates still-in-flight work that dropped out of the narrowed
    /// candidate set, and re-checks per-project + per-state + global slots — then re-dispatches,
    /// requeues, or releases the claim. Mirrors Go `onRetry`. A control-loop (O7) entry point.
    pub async fn on_retry(&mut self, e: EvRetry) {
        let Some(re) = self.retry_attempts.remove(&e.issue_id) else {
            return;
        };
        // A continuation retry (clean exit → ContinuationDelayMS) must not escalate to failure backoff
        // on a transient reschedule; `completed` is set only on the clean-exit continuation path.
        let is_continuation = self.completed.contains(&e.issue_id);

        let cfg = match self.resolve_retry_config(&re.project_slug) {
            RetryRouting::Release(reason) => {
                tracing::info!(issue_id = %e.issue_id, issue_identifier = %re.identifier, project_slug = %re.project_slug, "releasing claim; {reason}");
                self.claimed.remove(&e.issue_id);
                self.completed.remove(&e.issue_id); // drop continuation marker so the map can't grow unbounded
                self.persist_release(&re.identifier);
                return;
            }
            RetryRouting::Config(cfg) => *cfg,
        };

        let candidates = match cfg.tracker.fetch_candidate_issues().await {
            Ok(c) => c,
            Err(_) => {
                // Keep the claim + requeue across a transient poll failure (§3.7) rather than abandon
                // in-flight work; a recovered entry stays IDENTIFIER-keyed + recovered.
                let next = re.attempt + 1;
                let backoff = failure_backoff_ms(next, cfg.max_retry_backoff_ms);
                let target = RetryTarget {
                    id: &e.issue_id,
                    identifier: &re.identifier,
                    project_slug: &re.project_slug,
                    project_repo: &re.project_repo,
                };
                if re.recovered {
                    self.requeue_recovered(&re, next, backoff, "retry poll failed");
                } else if is_continuation {
                    // A continuation under a sustained tracker outage must not busy-loop at 1s: escalate
                    // with failure backoff, never below the continuation cadence, persisting the attempt.
                    let delay = CONTINUATION_DELAY_MS.max(backoff);
                    self.schedule_retry_for(
                        target,
                        next,
                        delay,
                        "continuation retry poll failed",
                        re.issue.clone(),
                    );
                } else {
                    self.schedule_retry_for(
                        target,
                        next,
                        backoff,
                        "retry poll failed",
                        re.issue.clone(),
                    );
                }
                return;
            }
        };

        // A boot-recovered entry is keyed by IDENTIFIER (the opaque ID is unknown at restart), so match
        // its candidate by identifier; live entries match by ID (§3.7).
        let mut iss: Option<Issue> = if re.recovered {
            find_by_identifier(&candidates, &re.identifier).cloned()
        } else {
            find_by_id(&candidates, &e.issue_id).cloned()
        };

        // In-flight relocation fallback (live + recovered). The candidate set is narrowed (active
        // states + key-owner assignee + optional milestone); that narrowing gates FRESH dispatch only,
        // not the lifecycle of work in flight, so re-resolve the current state via the UNFILTERED
        // by-ids path. Gated on a known opaque ID (re.issue.id): a freshly-booted recovered entry whose
        // ID was never learned has nothing to look up and keeps release-on-miss.
        if iss.is_none() && !re.issue.id.is_empty() {
            match self
                .recheck_in_flight(&cfg.tracker, &re.issue.id, re.issue.clone(), &cfg.terminal)
                .await
            {
                Err(_) => {
                    // State undeterminable this tick — keep the claim + requeue rather than abandon.
                    let next = re.attempt + 1;
                    let mut delay = failure_backoff_ms(next, cfg.max_retry_backoff_ms);
                    if is_continuation {
                        delay = CONTINUATION_DELAY_MS.max(delay);
                    }
                    tracing::info!(issue_id = %e.issue_id, issue_identifier = %re.identifier, "requeue: in-flight issue absent from candidates; state recheck failed");
                    if re.recovered {
                        self.requeue_recovered(&re, next, delay, "in-flight state recheck failed");
                    } else {
                        self.schedule_retry_for(
                            RetryTarget {
                                id: &e.issue_id,
                                identifier: &re.identifier,
                                project_slug: &re.project_slug,
                                project_repo: &re.project_repo,
                            },
                            next,
                            delay,
                            "in-flight state recheck failed",
                            re.issue.clone(),
                        );
                    }
                    return;
                }
                Ok(Recheck::Gone) => { /* genuinely gone / terminal → fall through to release */ }
                Ok(Recheck::Relocated(relocated)) => {
                    tracing::info!(issue_id = %e.issue_id, issue_identifier = %re.identifier, state = %relocated.state, "continuing in-flight issue filtered out of candidates (still active)");
                    iss = Some(*relocated);
                }
            }
        }

        let Some(iss) = iss else {
            tracing::info!(issue_id = %e.issue_id, issue_identifier = %re.identifier, "releasing claim");
            self.claimed.remove(&e.issue_id); // gone / no longer a candidate → release
            self.completed.remove(&e.issue_id);
            self.persist_release(&re.identifier);
            return;
        };

        // Boot-recovery re-dispatch only: a recovered issue with a linked PR (open or merged) and no
        // newer summons has already shipped — release instead of re-dispatching (mirrors the
        // selectDispatch guard). Restricted to recovered entries so continuations / in-session failure
        // retries are never suppressed by a PR they themselves opened.
        if re.recovered && self.pr_suppressed(&iss) {
            tracing::info!(issue_id = %e.issue_id, issue_identifier = %re.identifier, "releasing recovered claim: issue has a linked PR and no newer summons");
            self.claimed.remove(&e.issue_id);
            self.completed.remove(&e.issue_id);
            self.persist_release(&re.identifier);
            return;
        }

        // Eligibility excluding this issue's own pending claim. The label gate is proactive-pickup-only
        // (nil labels here), so a required label stripped mid-run does not abandon in-flight work.
        let claimed_except: HashSet<String> = self
            .claimed
            .iter()
            .filter(|k| k.as_str() != e.issue_id)
            .cloned()
            .collect();
        let no_labels: HashSet<String> = HashSet::new();
        let running = self.running_id_set();
        let ok = eligible(
            &iss,
            &running,
            &claimed_except,
            &EligibilityGate {
                active: &cfg.active,
                terminal: &cfg.terminal,
                required_labels: &no_labels,
                mode: &cfg.mode,
                review: &cfg.review,
                canceled: &cfg.canceled,
            },
        );
        if !ok {
            tracing::info!(issue_id = %e.issue_id, issue_identifier = %re.identifier, "releasing claim");
            self.claimed.remove(&e.issue_id); // no longer eligible (e.g. blocker appeared) → release
            self.completed.remove(&e.issue_id);
            self.persist_release(&re.identifier);
            return;
        }

        let st = normalize_state(&iss.state);
        let no_global = global_slots(cfg.global_cap, self.running.len() as i64) <= 0;
        let no_project = cfg
            .rp_group
            .as_ref()
            .is_some_and(|g| self.running_in_project_group(g) >= cfg.project_cap);
        // The per-state cap is a shared GLOBAL ceiling (runningStateCounts spans all projects), so its
        // fallback is the GLOBAL cap, not the per-project cap (consistent with selectDispatchMulti).
        let no_state = self.running_state_counts().get(&st).copied().unwrap_or(0)
            >= state_limit(&iss.state, &cfg.per_state, cfg.global_cap);
        if no_global || no_project || no_state {
            // Recovered guard FIRST (matching the poll-failure ordering): a recovered entry that can't
            // dispatch yet must stay IDENTIFIER-keyed + recovered so the next fire re-matches by
            // identifier (a recovered entry is never a continuation, so this reorder is behavior-identical).
            if re.recovered {
                tracing::warn!(issue_id = %e.issue_id, issue_identifier = %iss.identifier, attempt = re.attempt + 1, project_slug = %re.project_slug, "requeue: no available orchestrator slots");
                self.requeue_recovered(
                    &re,
                    re.attempt + 1,
                    failure_backoff_ms(re.attempt + 1, cfg.max_retry_backoff_ms),
                    "no available orchestrator slots",
                );
                return;
            }
            // `iss` is moved into the reschedule below, so capture its identifier first (Go passes
            // `iss.Identifier` as the retry's store PK on this path).
            let slot_identifier = iss.identifier.clone();
            let target = RetryTarget {
                id: &e.issue_id,
                identifier: &slot_identifier,
                project_slug: &re.project_slug,
                project_repo: &re.project_repo,
            };
            if is_continuation {
                // Slots full and staying full → a continuation requeued at the fixed 1s cadence would
                // busy-loop; escalate with failure backoff, never below the continuation cadence.
                let next = re.attempt + 1;
                let delay =
                    CONTINUATION_DELAY_MS.max(failure_backoff_ms(next, cfg.max_retry_backoff_ms));
                tracing::info!(issue_id = %e.issue_id, issue_identifier = %iss.identifier, attempt = next, project_slug = %re.project_slug, "requeue continuation: no available orchestrator slots");
                self.schedule_retry_for(
                    target,
                    next,
                    delay,
                    "no available orchestrator slots",
                    iss,
                );
                return;
            }
            tracing::warn!(issue_id = %e.issue_id, issue_identifier = %iss.identifier, attempt = re.attempt + 1, project_slug = %re.project_slug, "requeue: no available orchestrator slots");
            let attempt = re.attempt + 1;
            self.schedule_retry_for(
                target,
                attempt,
                failure_backoff_ms(attempt, cfg.max_retry_backoff_ms),
                "no available orchestrator slots",
                iss,
            );
            return;
        }

        tracing::info!(issue_id = %iss.id, issue_identifier = %iss.identifier, attempt = re.attempt, project_slug = %re.project_slug, "retrying issue");
        // Re-key a boot-recovered entry to the real opaque ID before dispatch: drop the stale
        // identifier-keyed claim + clear the stale retry_queue row (dispatch → persist_start_run
        // re-writes the claim to running) (§3.7).
        if re.recovered {
            self.claimed.remove(&e.issue_id);
            if let Err(err) = self.store.delete_retry(&re.identifier) {
                tracing::error!(issue_identifier = %re.identifier, error = %err, "persist delete retry failed");
            }
        }
        let attempt = re.attempt;
        self.dispatch_issue(iss, Some(attempt), cfg.route, String::new());
    }

    /// Resolves a fired retry's routing against the current effective set. Snapshots the config into
    /// owned values so no borrow of `self.eff` outlives this call. An empty slug is a legacy retry; a
    /// non-empty slug that no longer resolves, or a paused (disabled) project, releases the claim.
    fn resolve_retry_config(&self, slug: &str) -> RetryRouting {
        let Some(eff) = self.eff.as_ref() else {
            // Unreachable after `Run` (eff is always built first); release rather than panic.
            return RetryRouting::Release("no effective config");
        };
        if slug.is_empty() {
            return RetryRouting::Config(Box::new(RetryConfig::from_top_level(eff)));
        }
        match eff.project_by_slug(slug) {
            None => RetryRouting::Release("project no longer configured"),
            // A paused project (enabled:false) must not re-fetch or re-dispatch (INF-224).
            Some(rp) if rp.disabled => RetryRouting::Release("project paused"),
            Some(rp) => RetryRouting::Config(Box::new(RetryConfig::from_project(eff, rp))),
        }
    }

    /// Re-resolves an already-claimed in-flight issue that is ABSENT from the candidate set. The by-ids
    /// read is STATE-ONLY (the query selects id/identifier/title/state), so only `State` can be
    /// refreshed honestly; the dispatch-time `blocked_by` is stale and cannot be re-verified, so it is
    /// DROPPED (the relocated path must not re-block work in flight on data it can no longer trust — the
    /// next full candidate poll re-applies the real blocker gate). Mirrors Go `recheckInFlight`.
    async fn recheck_in_flight(
        &self,
        tr: &Arc<dyn Tracker>,
        id: &str,
        last: Issue,
        terminal: &HashSet<String>,
    ) -> Result<Recheck, TrackerError> {
        let ids = [id.to_string()];
        let states = tr.fetch_issue_states_by_ids(&ids).await?;
        let Some(cur) = find_by_id(&states, id) else {
            return Ok(Recheck::Gone); // genuinely gone from the tracker
        };
        if terminal.contains(&normalize_state(&cur.state)) {
            return Ok(Recheck::Gone); // reached a terminal state → release
        }
        let mut out = last; // last-known full issue (description / PR signals)
        out.state = cur.state.clone(); // refresh to the current state (the only field by-ids can refresh)
        out.blocked_by = None; // drop dispatch-time blockers we cannot re-verify (staleness contract)
        Ok(Recheck::Relocated(Box::new(out)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testsupport::*;
    use chrono::SecondsFormat;
    use rhapsody_core::BlockerRef;
    use rhapsody_store::Store;
    use rhapsody_tracker::fake::Fake;

    /// One [`classify_clean_exit`] table case. A named struct (rather than a wider tuple) keeps the
    /// review-set column readable and mirrors Go `TestClassifyCleanExit`'s anonymous struct table.
    struct ClassifyCase {
        name: &'static str,
        worker_state: &'static str,
        snap_state: &'static str,
        declared: bool,
        /// This case's `review_states`, already normalized — production feeds the classifier the
        /// `normalize_set`-lowered effective set, so the table's entries are lowercase too while the
        /// `worker_state` / `snap_state` inputs stay mixed-case to exercise the normalization.
        review: &'static [&'static str],
        want_outcome: &'static str,
        want_release: bool,
        want_reason: &'static str,
    }

    // Mirrors Go `TestClassifyCleanExit` (taxonomy v2, INF-272 / INF-266), extended with the
    // review-state branch (TRA-279) that Go v0.4.0 lacks — see the divergence note on
    // `classify_clean_exit`.
    #[test]
    fn classify_clean_exit_taxonomy_v2() {
        let active = set_of(&["todo", "in progress"]);
        let canceled = set_of(&["cancelled", "duplicate"]);
        let terminal = set_of(&["done", "cancelled", "duplicate"]);
        const REVIEW: &[&str] = &["in review"];
        let cases: &[ClassifyCase] = &[
            ClassifyCase {
                name: "both active, declared",
                worker_state: "In Progress",
                snap_state: "Todo",
                declared: true,
                review: REVIEW,
                want_outcome: store::OUTCOME_CONTINUED,
                want_release: false,
                want_reason: "",
            },
            ClassifyCase {
                name: "both empty",
                worker_state: "",
                snap_state: "",
                declared: false,
                review: REVIEW,
                want_outcome: store::OUTCOME_CONTINUED,
                want_release: false,
                want_reason: "",
            },
            ClassifyCase {
                name: "left to In Review, declared",
                worker_state: "In Review",
                snap_state: "Todo",
                declared: true,
                review: REVIEW,
                want_outcome: store::OUTCOME_COMPLETED,
                want_release: true,
                want_reason: "",
            },
            // THE BUG (TRA-279): the agent moved the ticket to review itself and ended its turn
            // without a HANDOFF marker. That is the intended end state of a review-gated run, not an
            // external actor.
            ClassifyCase {
                name: "left to In Review, undeclared, review configured",
                worker_state: "In Review",
                snap_state: "Todo",
                declared: false,
                review: REVIEW,
                want_outcome: store::OUTCOME_COMPLETED,
                want_release: true,
                want_reason: "",
            },
            // Back-compat: with the feature unconfigured, classification is byte-identical to Go.
            ClassifyCase {
                name: "left to In Review, review set EMPTY",
                worker_state: "In Review",
                snap_state: "Todo",
                declared: false,
                review: &[],
                want_outcome: store::OUTCOME_STOPPED,
                want_release: true,
                want_reason: "ticket moved externally",
            },
            // The reconcile snapshot is the sample that left the active set, and it is uppercased —
            // the review test must run on the NORMALIZED state, like every sibling branch.
            ClassifyCase {
                name: "snapshot sample in review, mixed case, undeclared",
                worker_state: "",
                snap_state: "IN REVIEW",
                declared: false,
                review: REVIEW,
                want_outcome: store::OUTCOME_COMPLETED,
                want_release: true,
                want_reason: "",
            },
            ClassifyCase {
                name: "left to Done (no cancel), undeclared",
                worker_state: "Done",
                snap_state: "In Progress",
                declared: false,
                review: REVIEW,
                want_outcome: store::OUTCOME_COMPLETED,
                want_release: true,
                want_reason: "",
            },
            ClassifyCase {
                name: "left to Done, declared",
                worker_state: "Done",
                snap_state: "In Progress",
                declared: true,
                review: REVIEW,
                want_outcome: store::OUTCOME_COMPLETED,
                want_release: true,
                want_reason: "",
            },
            ClassifyCase {
                name: "left to Cancelled",
                worker_state: "Cancelled",
                snap_state: "In Progress",
                declared: false,
                review: REVIEW,
                want_outcome: store::OUTCOME_STOPPED,
                want_release: true,
                want_reason: "ticket cancelled",
            },
            ClassifyCase {
                name: "left to Duplicate",
                worker_state: "In Progress",
                snap_state: "Duplicate",
                declared: true,
                review: REVIEW,
                want_outcome: store::OUTCOME_STOPPED,
                want_release: true,
                want_reason: "ticket cancelled",
            },
            ClassifyCase {
                name: "conflicting Done/Cancelled, cancel wins",
                worker_state: "Done",
                snap_state: "Cancelled",
                declared: true,
                review: REVIEW,
                want_outcome: store::OUTCOME_STOPPED,
                want_release: true,
                want_reason: "ticket cancelled",
            },
            ClassifyCase {
                name: "canceled-but-not-terminal misconfig, undeclared",
                worker_state: "Parked",
                snap_state: "Todo",
                declared: false,
                review: REVIEW,
                want_outcome: store::OUTCOME_STOPPED,
                want_release: true,
                want_reason: "ticket moved externally",
            },
            ClassifyCase {
                name: "left to a non-review, non-terminal state, undeclared",
                worker_state: "Blocked",
                snap_state: "Todo",
                declared: false,
                review: REVIEW,
                want_outcome: store::OUTCOME_STOPPED,
                want_release: true,
                want_reason: "ticket moved externally",
            },
            // Branch ordering: a review state that is ALSO terminal is decided by the earlier
            // terminal branch (same outcome, but it must not be the review branch that fires — the
            // undeclared-hand-off warning keys on that distinction).
            ClassifyCase {
                name: "review state that is also terminal follows terminal semantics",
                worker_state: "Done",
                snap_state: "In Progress",
                declared: false,
                review: &["done"],
                want_outcome: store::OUTCOME_COMPLETED,
                want_release: true,
                want_reason: "",
            },
            // Branch ordering: cancel-type still wins over the new review branch.
            ClassifyCase {
                name: "review state that is also cancel-type + terminal stays cancelled",
                worker_state: "Cancelled",
                snap_state: "In Progress",
                declared: false,
                review: &["cancelled"],
                want_outcome: store::OUTCOME_STOPPED,
                want_release: true,
                want_reason: "ticket cancelled",
            },
        ];
        // A canceled_states entry that is NOT terminal must not hijack classification.
        let canceled_misconfig = set_of(&["cancelled", "duplicate", "parked"]);
        for c in cases {
            let cset = if c.name == "canceled-but-not-terminal misconfig, undeclared" {
                &canceled_misconfig
            } else {
                &canceled
            };
            let review = set_of(c.review);
            let (got, release, reason) = classify_clean_exit(
                &active,
                cset,
                &terminal,
                &review,
                c.declared,
                c.worker_state,
                c.snap_state,
            );
            assert_eq!(
                (got.as_str(), release, reason.as_str()),
                (c.want_outcome, c.want_release, c.want_reason),
                "{}",
                c.name
            );
        }
    }

    /// TRA-279 end-to-end at the `on_worker_exit` seam: the exact shape of run 35 / TRA-278 — the
    /// agent opened its PR, moved the ticket to the configured review state itself, then ended a turn
    /// without a `HANDOFF:` marker. The run must be stored `completed` with no error, and the claim
    /// must be released rather than a continuation scheduled.
    #[test]
    fn on_worker_exit_review_state_undeclared_records_completed() {
        let (mut o, _) = orch_for_retry(Arc::new(Fake::new()), 10);
        let store_handle: Arc<dyn Store + Send + Sync> = Arc::new(
            rhapsody_store::Sqlite::open(rhapsody_store::StorePath::InMemory).expect("open"),
        );
        o.set_store(Arc::clone(&store_handle));
        if let Some(eff) = o.eff.as_mut() {
            eff.review_states = set_of(&["in review"]);
        }
        o.dispatch_issue(issue("1", "MT-1", "Todo"), None, None, String::new());
        let st = o.running["1"].started_at;

        o.on_worker_exit(EvWorkerExit {
            issue_id: "1".into(),
            failed: false,
            started_at: st,
            err_msg: String::new(),
            last_state: "In Review".into(),
            declared_handoff: false,
        });

        assert!(
            !o.retry_attempts.contains_key("1"),
            "a review-state exit must release, not schedule a continuation"
        );
        assert!(!o.claimed.contains("1"), "claim must be released");
        let runs = store_handle
            .list_runs(rhapsody_store::RunFilter::default())
            .expect("list runs");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].outcome, store::OUTCOME_COMPLETED);
        assert_eq!(runs[0].error, "", "no 'ticket moved externally' diagnosis");
    }

    // BO-12: dispatch computes the ADDITIVE capability set (project defaults ∪ the ticket's
    // `rhapsody:*` labels), rendered (in REGISTRY order) through the registry into the running entry's
    // first-turn capability section. Unknown `rhapsody:*` names and non-`rhapsody:` labels contribute
    // nothing; a `None` registry makes the whole thing a no-op.
    #[test]
    fn dispatch_computes_additive_capability_section() {
        let (mut o, _) = orch_for_retry(Arc::new(Fake::new()), 10);
        // Project default carries `simplify`; the registry orders `code-review` BEFORE `simplify`.
        if let Some(eff) = o.eff.as_mut() {
            eff.capabilities = vec!["simplify".to_string()];
        }
        o.capabilities_registry = Some(rhapsody_config::capabilities::default_capabilities());
        // The ticket adds `rhapsody:code-review` (unioned), a non-`rhapsody:` label (ignored), and an
        // unknown `rhapsody:bogus` (rendered to nothing).
        let iss = Issue {
            labels: Some(vec![
                "rhapsody:code-review".to_string(),
                "backend".to_string(),
                "rhapsody:bogus".to_string(),
            ]),
            ..issue("1", "MT-1", "Todo")
        };
        o.dispatch_issue(iss, None, None, String::new());

        let section = o.running["1"].capabilities_section.clone();
        assert!(
            section.starts_with("## Required practices for this ticket"),
            "section = {section:?}"
        );
        let cr = section
            .find("review your own diff")
            .expect("code-review (from the ticket label) must render");
        let simp = section
            .find("unnecessary abstraction")
            .expect("simplify (from the project default) must render");
        assert!(
            cr < simp,
            "capabilities render in REGISTRY order (code-review before simplify), not selection order"
        );
        assert!(
            !section.contains("bogus"),
            "an unknown rhapsody:* capability renders nothing"
        );

        // With no registry the section is empty (capabilities become a no-op).
        let (mut o2, _) = orch_for_retry(Arc::new(Fake::new()), 10);
        if let Some(eff) = o2.eff.as_mut() {
            eff.capabilities = vec!["simplify".to_string()];
        }
        o2.dispatch_issue(issue("1", "MT-1", "Todo"), None, None, String::new());
        assert_eq!(
            o2.running["1"].capabilities_section, "",
            "no registry ⇒ empty section"
        );
    }

    /// The review set must come from the OWNING PROJECT's effective config, not the global tracker
    /// config: a project whose `review_states` override is `{qa}` completes on QA, while a sibling
    /// project that overrides review off keeps the legacy "ticket moved externally" classification.
    #[test]
    fn on_worker_exit_review_state_honors_per_project_override() {
        let mut qa = proj_with_tracker("qa-proj", Arc::new(Fake::new()), "p");
        qa.review_states = set_of(&["qa"]);
        let mut none = proj_with_tracker("no-review-proj", Arc::new(Fake::new()), "p");
        none.review_states = HashSet::new();
        let (mut o, _) = orch_for_retry_multi(vec![qa, none], 10);
        let store_handle: Arc<dyn Store + Send + Sync> = Arc::new(
            rhapsody_store::Sqlite::open(rhapsody_store::StorePath::InMemory).expect("open"),
        );
        o.set_store(Arc::clone(&store_handle));
        // The top-level set is deliberately EMPTY, so a global-config read would misclassify the
        // QA exit and an inherited-from-global read would misclassify the override-off exit.
        if let Some(eff) = o.eff.as_mut() {
            eff.review_states = HashSet::new();
        }

        for (id, ident, slug, state, want_outcome, want_err) in [
            ("1", "QA-1", "qa-proj", "QA", store::OUTCOME_COMPLETED, ""),
            (
                "2",
                "NR-1",
                "no-review-proj",
                "QA",
                store::OUTCOME_STOPPED,
                "ticket moved externally",
            ),
        ] {
            o.dispatch_issue(
                issue(id, ident, "Todo"),
                None,
                Some(DispatchRoute {
                    slug: slug.to_string(),
                    group: slug.to_string(),
                    repo: String::new(),
                    model: String::new(),
                    workspace_mode: String::new(),
                }),
                String::new(),
            );
            let st = o.running[id].started_at;
            o.on_worker_exit(EvWorkerExit {
                issue_id: id.into(),
                failed: false,
                started_at: st,
                err_msg: String::new(),
                last_state: state.into(),
                declared_handoff: false,
            });
            let runs = store_handle
                .list_runs(rhapsody_store::RunFilter {
                    issue: ident.to_string(),
                    ..Default::default()
                })
                .expect("list runs");
            assert_eq!(runs.len(), 1, "{ident}");
            assert_eq!(runs[0].outcome, want_outcome, "{ident}");
            assert_eq!(runs[0].error, want_err, "{ident}");
        }
    }

    // Mirrors Go `TestHasHandoffMarker` (defined in retry_test.go; the fn lives in worker.rs).
    #[test]
    fn has_handoff_marker_grammar() {
        let yes = [
            "HANDOFF: in-review",
            "All done.\nHANDOFF: in-review",
            "work summary\n  HANDOFF: in-review  \ntrailing note",
        ];
        let no = [
            "",
            "all done, moved to In Review",
            "the HANDOFF: is mid-line not at line start",
        ];
        for s in yes {
            assert!(crate::worker::has_handoff_marker(s), "{s:?} should be true");
        }
        for s in no {
            assert!(
                !crate::worker::has_handoff_marker(s),
                "{s:?} should be false"
            );
        }
    }

    // Mirrors Go `TestDispatchIssueClaimsAndRecords`.
    #[test]
    fn dispatch_issue_claims_and_records() {
        let (mut o, dispatched) = orch_for_retry(Arc::new(Fake::new()), 10);
        o.dispatch_issue(issue("1", "MT-1", "Todo"), None, None, String::new());
        assert!(
            o.claimed.contains("1") && o.running.contains_key("1"),
            "issue should be claimed and running"
        );
        assert_eq!(*dispatched.lock().unwrap(), vec!["1".to_string()]);
    }

    // Mirrors Go `TestOnWorkerExitNormalSchedulesContinuation`.
    #[test]
    fn on_worker_exit_normal_schedules_continuation() {
        let (mut o, _) = orch_for_retry(Arc::new(Fake::new()), 10);
        o.dispatch_issue(issue("1", "MT-1", "Todo"), None, None, String::new());
        let st = o.running["1"].started_at;
        // Clean exit with the ticket still active (turn-budget exhaustion) → continued + continuation.
        o.on_worker_exit(EvWorkerExit {
            issue_id: "1".into(),
            failed: false,
            started_at: st,
            err_msg: String::new(),
            last_state: "In Progress".into(),
            declared_handoff: false,
        });
        assert!(
            !o.running.contains_key("1"),
            "running entry should be removed"
        );
        let re = o.retry_attempts.get("1").expect("continuation retry");
        assert_eq!(re.attempt, 1);
        assert!(
            o.completed.contains("1"),
            "normal exit should mark completed"
        );
        assert!(
            o.totals.seconds_running > 0.0,
            "runtime should be accumulated"
        );
    }

    // Mirrors Go `TestOnWorkerExitFailureSchedulesBackoff`.
    #[test]
    fn on_worker_exit_failure_schedules_backoff() {
        let (mut o, _) = orch_for_retry(Arc::new(Fake::new()), 10);
        o.dispatch_issue(issue("1", "MT-1", "Todo"), Some(2), None, String::new()); // retry_attempt=2
        let st = o.running["1"].started_at;
        o.on_worker_exit(EvWorkerExit {
            issue_id: "1".into(),
            failed: true,
            started_at: st,
            err_msg: String::new(),
            last_state: String::new(),
            declared_handoff: false,
        });
        let re = o.retry_attempts.get("1").expect("backoff retry");
        assert_eq!(re.attempt, 3);
    }

    // Mirrors Go `TestOnWorkerExitUnknownIsNoop`.
    #[test]
    fn on_worker_exit_unknown_is_noop() {
        let (mut o, _) = orch_for_retry(Arc::new(Fake::new()), 10);
        o.on_worker_exit(EvWorkerExit {
            issue_id: "ghost".into(),
            failed: true,
            started_at: (o.now)(),
            err_msg: String::new(),
            last_state: String::new(),
            declared_handoff: false,
        });
        assert!(
            o.retry_attempts.is_empty(),
            "exit for a non-running issue (already terminated) must be a no-op"
        );
    }

    // Mirrors Go `TestOnRetryDispatchesWhenEligible`.
    #[tokio::test]
    async fn on_retry_dispatches_when_eligible() {
        let mut f = Fake::new();
        f.candidates = vec![issue("1", "MT-1", "In Progress")];
        let tr = Arc::new(f);
        let (mut o, dispatched) = orch_for_retry(Arc::clone(&tr), 10);
        o.claimed.insert("1".into());
        o.retry_attempts
            .insert("1".into(), retry_entry("1", "MT-1", 1));
        o.on_retry(EvRetry {
            issue_id: "1".into(),
        })
        .await;
        assert_eq!(*dispatched.lock().unwrap(), vec!["1".to_string()]);
        assert!(
            !o.retry_attempts.contains_key("1"),
            "retry entry should be cleared on dispatch"
        );
    }

    // Mirrors Go `TestOnRetryReleasesRecoveredWhenPRLinked` (INF-191).
    #[tokio::test]
    async fn on_retry_releases_recovered_when_pr_linked() {
        let pr = utc(2026, 6, 3, 12, 0, 0);
        let mut f = Fake::new();
        f.candidates = vec![Issue {
            linked_pr: true,
            latest_pr_activity_at: Some(pr),
            ..issue("u-191", "INF-191", "In Progress")
        }];
        let tr = Arc::new(f);
        let (mut o, dispatched) = orch_for_retry(Arc::clone(&tr), 10);
        o.claimed.insert("INF-191".into()); // recovered claims are keyed by IDENTIFIER
        let mut re = retry_entry("", "INF-191", 1);
        re.recovered = true;
        o.retry_attempts.insert("INF-191".into(), re);
        o.on_retry(EvRetry {
            issue_id: "INF-191".into(),
        })
        .await;
        assert!(
            !o.claimed.contains("INF-191"),
            "PR-linked recovered issue must be released"
        );
        assert!(
            !o.retry_attempts.contains_key("INF-191"),
            "recovered retry entry must be cleared"
        );
        assert!(
            dispatched.lock().unwrap().is_empty(),
            "must NOT be re-dispatched"
        );
    }

    // Mirrors Go `TestOnRetryDispatchesRecoveredWhenSummonReopens`.
    #[tokio::test]
    async fn on_retry_dispatches_recovered_when_summon_reopens() {
        let pr = utc(2026, 6, 3, 12, 0, 0);
        let summon = pr + chrono::Duration::hours(1);
        let mut f = Fake::new();
        f.candidates = vec![Issue {
            linked_pr: true,
            latest_pr_activity_at: Some(pr),
            latest_summon_at: Some(summon),
            ..issue("u-191", "INF-191", "In Progress")
        }];
        let tr = Arc::new(f);
        let (mut o, dispatched) = orch_for_retry(Arc::clone(&tr), 10);
        o.claimed.insert("INF-191".into());
        let mut re = retry_entry("", "INF-191", 1);
        re.recovered = true;
        o.retry_attempts.insert("INF-191".into(), re);
        o.on_retry(EvRetry {
            issue_id: "INF-191".into(),
        })
        .await;
        assert_eq!(
            *dispatched.lock().unwrap(),
            vec!["u-191".to_string()],
            "a recovered issue reopened by a newer summons must be re-dispatched"
        );
    }

    // Mirrors Go `TestOnRetryReleasesWhenGone`.
    #[tokio::test]
    async fn on_retry_releases_when_gone() {
        let (mut o, dispatched) = orch_for_retry(Arc::new(Fake::new()), 10); // no candidates
        o.claimed.insert("1".into());
        o.retry_attempts
            .insert("1".into(), retry_entry("1", "MT-1", 1));
        o.on_retry(EvRetry {
            issue_id: "1".into(),
        })
        .await;
        assert!(
            !o.claimed.contains("1"),
            "claim should be released when no longer a candidate"
        );
        assert!(
            dispatched.lock().unwrap().is_empty(),
            "nothing should be dispatched"
        );
    }

    // Mirrors Go `TestOnRetryContinuesInFlightWhenFilteredOut`.
    #[tokio::test]
    async fn on_retry_continues_in_flight_when_filtered_out() {
        let mut f = Fake::new(); // issue NOT in the filtered candidate set...
        f.by_id
            .insert("1".into(), issue("1", "MT-1", "In Progress")); // ...but by-ids reports it active
        let tr = Arc::new(f);
        let (mut o, dispatched) = orch_for_retry(Arc::clone(&tr), 10);
        o.completed.insert("1".into()); // continuation (clean exit wanting more turns)
        o.claimed.insert("1".into());
        let mut re = retry_entry("1", "MT-1", 1);
        re.issue = issue("1", "MT-1", "In Progress");
        o.retry_attempts.insert("1".into(), re);
        o.on_retry(EvRetry {
            issue_id: "1".into(),
        })
        .await;
        assert!(
            o.claimed.contains("1"),
            "filtered-out-but-active in-flight work must NOT be released"
        );
        assert_eq!(
            *dispatched.lock().unwrap(),
            vec!["1".to_string()],
            "must continue (re-dispatch)"
        );
    }

    // Mirrors Go `TestOnRetryReleasesInFlightWhenTerminal`.
    #[tokio::test]
    async fn on_retry_releases_in_flight_when_terminal() {
        let mut f = Fake::new();
        f.by_id.insert("1".into(), issue("1", "MT-1", "Done")); // terminal
        let tr = Arc::new(f);
        let (mut o, dispatched) = orch_for_retry(Arc::clone(&tr), 10);
        o.claimed.insert("1".into());
        let mut re = retry_entry("1", "MT-1", 1);
        re.issue = issue("1", "MT-1", "In Progress");
        o.retry_attempts.insert("1".into(), re);
        o.on_retry(EvRetry {
            issue_id: "1".into(),
        })
        .await;
        assert!(
            !o.claimed.contains("1"),
            "an in-flight issue that reached terminal must be released"
        );
        assert!(
            dispatched.lock().unwrap().is_empty(),
            "a terminal issue must not be re-dispatched"
        );
    }

    // Mirrors Go `TestOnRetryContinuesRecoveredInFlightWhenFilteredOut` (Finding A).
    #[tokio::test]
    async fn on_retry_continues_recovered_in_flight_when_filtered_out() {
        let mut f = Fake::new(); // NOT in the filtered candidate set...
        f.by_id
            .insert("u-1".into(), issue("u-1", "MT-1", "In Progress")); // ...still active
        let tr = Arc::new(f);
        let (mut o, dispatched) = orch_for_retry(Arc::clone(&tr), 10);
        o.claimed.insert("MT-1".into()); // recovered claims keyed by IDENTIFIER
        let mut re = retry_entry("", "MT-1", 1);
        re.recovered = true;
        re.issue = issue("u-1", "MT-1", "In Progress"); // opaque ID known
        o.retry_attempts.insert("MT-1".into(), re);
        o.on_retry(EvRetry {
            issue_id: "MT-1".into(),
        })
        .await;
        assert_eq!(
            *dispatched.lock().unwrap(),
            vec!["u-1".to_string()],
            "a recovered, filtered-out-but-active claim must relocate and continue"
        );
        assert!(
            !o.claimed.contains("MT-1"),
            "the identifier-keyed recovered claim must be re-keyed away on dispatch"
        );
    }

    // Mirrors Go `TestOnRetryReleasesRecoveredInFlightWithoutKnownID` (Finding A, guard).
    #[tokio::test]
    async fn on_retry_releases_recovered_in_flight_without_known_id() {
        let mut f = Fake::new(); // not a candidate; the by-id entry is never looked up
        f.by_id
            .insert("u-1".into(), issue("u-1", "MT-1", "In Progress"));
        let tr = Arc::new(f);
        let (mut o, dispatched) = orch_for_retry(Arc::clone(&tr), 10);
        o.claimed.insert("MT-1".into());
        let mut re = retry_entry("", "MT-1", 1); // no issue snapshot
        re.recovered = true;
        o.retry_attempts.insert("MT-1".into(), re);
        o.on_retry(EvRetry {
            issue_id: "MT-1".into(),
        })
        .await;
        assert!(
            !o.claimed.contains("MT-1"),
            "must release when filtered out (cannot relocate)"
        );
        assert!(
            dispatched.lock().unwrap().is_empty(),
            "nothing dispatchable without an opaque ID"
        );
        assert_eq!(
            tr.by_id_calls(),
            0,
            "recheck must not be attempted without a known opaque ID"
        );
    }

    // Mirrors Go `TestOnRetryRelocatedDropsStaleBlocker` (Finding B).
    #[tokio::test]
    async fn on_retry_relocated_drops_stale_blocker() {
        let mut f = Fake::new(); // not in the filtered candidate set...
        f.by_id.insert("1".into(), issue("1", "MT-1", "Todo")); // ...still active, Todo
        let tr = Arc::new(f);
        let (mut o, dispatched) = orch_for_retry(Arc::clone(&tr), 10);
        o.claimed.insert("1".into());
        let mut re = retry_entry("1", "MT-1", 1);
        // dispatch-time snapshot carried a non-terminal blocker on a Todo ticket.
        re.issue = Issue {
            blocked_by: Some(vec![BlockerRef {
                id: Some("u-blk".into()),
                identifier: Some("MT-9".into()),
                state: Some("todo".into()),
            }]),
            ..issue("1", "MT-1", "Todo")
        };
        o.retry_attempts.insert("1".into(), re);
        o.on_retry(EvRetry {
            issue_id: "1".into(),
        })
        .await;
        assert_eq!(
            *dispatched.lock().unwrap(),
            vec!["1".to_string()],
            "relocated in-flight work must continue (stale blocker dropped)"
        );
    }

    // Mirrors Go `TestOnRetryRetainsClaimWhenLabelRemoved`.
    #[tokio::test]
    async fn on_retry_retains_claim_when_label_removed() {
        let mut f = Fake::new();
        f.candidates = vec![issue("1", "MT-1", "In Progress")]; // no "ready" label
        let tr = Arc::new(f);
        let (mut o, dispatched) = orch_for_retry(Arc::clone(&tr), 10);
        o.eff.as_mut().expect("eff").labels = label_set(&["ready"]); // require "ready" for FRESH pickup
        o.claimed.insert("1".into());
        let mut re = retry_entry("1", "MT-1", 1);
        re.issue = issue("1", "MT-1", "In Progress");
        o.retry_attempts.insert("1".into(), re);
        o.on_retry(EvRetry {
            issue_id: "1".into(),
        })
        .await;
        assert!(
            o.claimed.contains("1"),
            "in-flight work must NOT be released when a label is stripped"
        );
        assert_eq!(
            *dispatched.lock().unwrap(),
            vec!["1".to_string()],
            "must be re-dispatched"
        );
    }

    // Mirrors Go `TestOnRetryContinuationPollFailureEscalatesDelay`.
    #[tokio::test]
    async fn on_retry_continuation_poll_failure_escalates_delay() {
        let mut f = Fake::new();
        f.candidates_err = Some(TrackerError::Other("boom".into())); // FetchCandidateIssues always fails
        let tr = Arc::new(f);
        let (mut o, _) = orch_for_retry(Arc::clone(&tr), 10);
        let base = utc(2023, 11, 14, 22, 13, 20); // Unix 1_700_000_000
        o.now = Box::new(move || base);

        o.completed.insert("1".into()); // continuation
        o.retry_attempts
            .insert("1".into(), retry_entry("1", "MT-1", 1));

        let cont_delay = chrono::Duration::milliseconds(CONTINUATION_DELAY_MS);
        let (mut last_attempt, mut last_delay_ms) = (0i64, 0i64);
        for i in 0..5 {
            o.on_retry(EvRetry {
                issue_id: "1".into(),
            })
            .await;
            let re = o
                .retry_attempts
                .get("1")
                .unwrap_or_else(|| panic!("iter {i}: expected reschedule"));
            assert_eq!(
                re.err, "continuation retry poll failed",
                "iter {i}: poll-failure reason"
            );
            let delay_ms = (re.due_at - base).num_milliseconds();
            assert!(
                re.attempt > last_attempt,
                "iter {i}: attempt must increment ({} <= {last_attempt})",
                re.attempt
            );
            assert!(
                delay_ms >= last_delay_ms,
                "iter {i}: delay must not shrink ({delay_ms} < {last_delay_ms})"
            );
            last_attempt = re.attempt;
            last_delay_ms = delay_ms;
        }
        assert!(
            chrono::Duration::milliseconds(last_delay_ms) > cont_delay,
            "delay never escalated past continuation cadence: {last_delay_ms}ms"
        );
    }

    // Mirrors Go `TestOnRetryReleasesRecoveredWhenHandedOff`.
    #[tokio::test]
    async fn on_retry_releases_recovered_when_handed_off() {
        let (mut o, dispatched) = orch_for_retry(Arc::new(Fake::new()), 10); // INF-178 not a candidate
        o.claimed.insert("INF-178".into());
        let mut re = retry_entry("", "INF-178", 1);
        re.recovered = true;
        o.retry_attempts.insert("INF-178".into(), re);
        o.on_retry(EvRetry {
            issue_id: "INF-178".into(),
        })
        .await;
        assert!(
            !o.claimed.contains("INF-178"),
            "handed-off recovered issue must be released"
        );
        assert!(
            !o.retry_attempts.contains_key("INF-178"),
            "recovered retry entry must be cleared"
        );
        assert!(
            dispatched.lock().unwrap().is_empty(),
            "must NOT be re-dispatched"
        );
    }

    // Mirrors Go `TestOnRetryRequeuesWhenNoSlots`.
    #[tokio::test]
    async fn on_retry_requeues_when_no_slots() {
        let mut f = Fake::new();
        f.candidates = vec![issue("1", "MT-1", "In Progress")];
        let tr = Arc::new(f);
        let (mut o, dispatched) = orch_for_retry(Arc::clone(&tr), 0); // zero slots
        o.claimed.insert("1".into());
        o.retry_attempts
            .insert("1".into(), retry_entry("1", "MT-1", 1));
        o.on_retry(EvRetry {
            issue_id: "1".into(),
        })
        .await;
        assert!(
            dispatched.lock().unwrap().is_empty(),
            "should not dispatch with no slots"
        );
        let re = o.retry_attempts.get("1").expect("requeued retry");
        assert_eq!(re.err, "no available orchestrator slots");
    }

    // Mirrors Go `TestOnRetryRecoveredSummonSurvivesRestart` (store-backed, INF-448).
    #[tokio::test]
    async fn on_retry_recovered_summon_survives_restart() {
        let completed_start = utc(2026, 6, 3, 10, 0, 0);
        let summon = completed_start + chrono::Duration::hours(1); // triggered the follow-up round
        let interrupted_start = summon + chrono::Duration::minutes(30); // the follow-up, killed by restart

        let mut f = Fake::new();
        f.candidates = vec![Issue {
            linked_pr: true,
            latest_summon_at: Some(summon),
            ..issue("u-1", "MT-1", "In Progress")
        }];
        let tr = Arc::new(f);
        let st: Arc<dyn Store + Send + Sync> =
            Arc::new(store::Sqlite::open(store::StorePath::InMemory).expect("open"));
        let (mut o, dispatched) = orch_for_retry(Arc::clone(&tr), 10);
        o.set_store(Arc::clone(&st));
        // prior completed round (started before the summons)...
        seed_run(
            st.as_ref(),
            "u-1",
            "MT-1",
            completed_start + chrono::Duration::minutes(1),
        );
        // ...then the summons-triggered round, interrupted by the restart.
        let id = st
            .start_run(store::RunStart {
                issue_id: "u-1".into(),
                issue_identifier: "MT-1".into(),
                title: "t".into(),
                started_at: interrupted_start.to_rfc3339_opts(SecondsFormat::Secs, true),
                ..Default::default()
            })
            .expect("start_run");
        st.end_run(
            id,
            store::RunEnd {
                outcome: store::OUTCOME_INTERRUPTED.into(),
                ended_at: (interrupted_start + chrono::Duration::minutes(1))
                    .to_rfc3339_opts(SecondsFormat::Secs, true),
                ..Default::default()
            },
        )
        .expect("end_run");

        o.claimed.insert("MT-1".into()); // recovered claims keyed by IDENTIFIER
        let mut re = retry_entry("", "MT-1", 1);
        re.recovered = true;
        o.retry_attempts.insert("MT-1".into(), re);
        o.on_retry(EvRetry {
            issue_id: "MT-1".into(),
        })
        .await;
        assert_eq!(
            *dispatched.lock().unwrap(),
            vec!["u-1".to_string()],
            "a summons-triggered round interrupted by a restart must be re-dispatched"
        );
    }

    // --- retry_multi_test.go mirrors -----------------------------------------------------------

    // Mirrors Go `TestRetryReFetchesFromProjectTracker`.
    #[tokio::test]
    async fn retry_refetches_from_project_tracker() {
        let tr_a = Arc::new(Fake::new()); // no candidate for the retried issue
        let mut fb = Fake::new();
        fb.candidates = vec![issue("b1", "B-1", "In Progress")];
        let tr_b = Arc::new(fb);
        let pa = proj_with_tracker("a", Arc::clone(&tr_a), "pa");
        let pb = proj_with_tracker("b", Arc::clone(&tr_b), "pb");
        let (mut o, dispatched) = orch_for_retry_multi(vec![pa, pb], 10);
        o.claimed.insert("b1".into());
        let mut re = retry_entry("b1", "B-1", 1);
        re.project_slug = "b".into();
        o.retry_attempts.insert("b1".into(), re);
        o.on_retry(EvRetry {
            issue_id: "b1".into(),
        })
        .await;
        assert_eq!(
            tr_a.candidate_calls(),
            0,
            "project A must NOT be polled for a B-routed retry"
        );
        assert_eq!(tr_b.candidate_calls(), 1, "project B should be polled once");
        let d = dispatched.lock().unwrap();
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].issue.id, "b1");
        assert_eq!(d[0].project_slug, "b");
    }

    // Mirrors Go `TestRetryDisabledProjectReleases`.
    #[tokio::test]
    async fn retry_disabled_project_releases() {
        let mut fa = Fake::new();
        fa.candidates = vec![issue("a1", "A-1", "In Progress")];
        let tr_a = Arc::new(fa);
        let mut pa = proj_with_tracker("a", Arc::clone(&tr_a), "pa");
        pa.disabled = true;
        let (mut o, dispatched) = orch_for_retry_multi(vec![pa], 10);
        o.claimed.insert("a1".into());
        let mut re = retry_entry("a1", "A-1", 1);
        re.project_slug = "a".into();
        o.retry_attempts.insert("a1".into(), re);
        o.on_retry(EvRetry {
            issue_id: "a1".into(),
        })
        .await;
        assert!(
            !o.claimed.contains("a1"),
            "claim should be released when the project is paused"
        );
        assert_eq!(
            tr_a.candidate_calls(),
            0,
            "paused project must NOT be polled on retry"
        );
        assert!(
            dispatched.lock().unwrap().is_empty(),
            "nothing should dispatch for a paused project"
        );
    }

    // Mirrors Go `TestRetryProjectRemovedReleases`.
    #[tokio::test]
    async fn retry_project_removed_releases() {
        let pa = proj_with_tracker("a", Arc::new(Fake::new()), "pa");
        let (mut o, dispatched) = orch_for_retry_multi(vec![pa], 10);
        o.claimed.insert("x1".into());
        let mut re = retry_entry("x1", "X-1", 1);
        re.project_slug = "gone".into(); // slug not in the resolved set
        o.retry_attempts.insert("x1".into(), re);
        o.on_retry(EvRetry {
            issue_id: "x1".into(),
        })
        .await;
        assert!(
            !o.claimed.contains("x1"),
            "claim should be released when the slug is not configured"
        );
        assert!(
            dispatched.lock().unwrap().is_empty(),
            "nothing should be dispatched for a removed project"
        );
    }

    // Mirrors Go `TestRetryRespectsPerProjectSlot`.
    #[tokio::test]
    async fn retry_respects_per_project_slot() {
        let mut fa = Fake::new();
        fa.candidates = vec![issue("a2", "A-2", "In Progress")];
        let tr_a = Arc::new(fa);
        let pa = proj_with_tracker("a", Arc::clone(&tr_a), "pa"); // cap 10... overridden below
        let mut pa = pa;
        pa.max_concurrent = 1; // cap=1, one A-issue already running => requeue
        let (mut o, dispatched) = orch_for_retry_multi(vec![pa], 10);
        // one A-issue already running
        o.running.insert(
            "a1".into(),
            running_entry(issue("a1", "A-1", "In Progress"), "a", "a"),
        );
        o.claimed.insert("a1".into());
        o.claimed.insert("a2".into());
        let mut re = retry_entry("a2", "A-2", 1);
        re.project_slug = "a".into();
        o.retry_attempts.insert("a2".into(), re);
        o.on_retry(EvRetry {
            issue_id: "a2".into(),
        })
        .await;
        assert!(
            dispatched.lock().unwrap().is_empty(),
            "should not dispatch when per-project cap is exhausted"
        );
        let re = o.retry_attempts.get("a2").expect("requeued");
        assert_eq!(re.err, "no available orchestrator slots");
        assert_eq!(
            re.project_slug, "a",
            "requeued retry should keep project slug a"
        );
    }

    // Mirrors Go `TestRetryPerStateCapIsGlobalAcrossProjects`.
    #[tokio::test]
    async fn retry_per_state_cap_is_global_across_projects() {
        let mut fa = Fake::new();
        fa.candidates = vec![issue("a1", "A-1", "In Progress")];
        let tr_a = Arc::new(fa);
        let mut pa = proj_with_tracker("a", Arc::clone(&tr_a), "pa");
        pa.per_state_limits = [("in progress".to_string(), 1i64)].into_iter().collect();
        let mut pb = proj_with_tracker("b", Arc::new(Fake::new()), "pb");
        pb.per_state_limits = [("in progress".to_string(), 1i64)].into_iter().collect();
        let (mut o, dispatched) = orch_for_retry_multi(vec![pa, pb], 10);
        // one B-issue already running in the same normalized state (global in-state = 1)
        o.running.insert(
            "b1".into(),
            running_entry(issue("b1", "B-1", "In Progress"), "b", "b"),
        );
        o.claimed.insert("b1".into());
        o.claimed.insert("a1".into());
        let mut re = retry_entry("a1", "A-1", 1);
        re.project_slug = "a".into();
        o.retry_attempts.insert("a1".into(), re);
        o.on_retry(EvRetry {
            issue_id: "a1".into(),
        })
        .await;
        assert!(
            dispatched.lock().unwrap().is_empty(),
            "shared per-state cap exhausted by B should requeue A"
        );
        let re = o.retry_attempts.get("a1").expect("requeued");
        assert_eq!(re.err, "no available orchestrator slots");
    }

    // Mirrors Go `TestRetryPerStateFallbackUsesGlobalCap`.
    #[tokio::test]
    async fn retry_per_state_fallback_uses_global_cap() {
        let mut fa = Fake::new();
        fa.candidates = vec![issue("a1", "A-1", "In Progress")];
        let tr_a = Arc::new(fa);
        let mut pa = proj_with_tracker("a", Arc::clone(&tr_a), "pa");
        pa.max_concurrent = 1; // cap 1, NO per-state override
        let pb = proj_with_tracker("b", Arc::new(Fake::new()), "pb");
        let (mut o, dispatched) = orch_for_retry_multi(vec![pa, pb], 5); // global cap 5
        // one B-issue running in the same state (does not touch A's project cap)
        o.running.insert(
            "b1".into(),
            running_entry(issue("b1", "B-1", "In Progress"), "b", "b"),
        );
        o.claimed.insert("b1".into());
        o.claimed.insert("a1".into());
        let mut re = retry_entry("a1", "A-1", 1);
        re.project_slug = "a".into();
        o.retry_attempts.insert("a1".into(), re);
        o.on_retry(EvRetry {
            issue_id: "a1".into(),
        })
        .await;
        let d = dispatched.lock().unwrap();
        assert_eq!(
            d.len(),
            1,
            "per-state fallback must use GLOBAL cap (5), so A's retry dispatches"
        );
        assert_eq!(d[0].issue.id, "a1");
    }

    // Mirrors Go `TestRetryProjectRouteStampsRunningEntry`.
    #[tokio::test]
    async fn retry_project_route_stamps_running_entry() {
        let mut fa = Fake::new();
        fa.candidates = vec![issue("a1", "A-1", "In Progress")];
        let tr_a = Arc::new(fa);
        let mut pa = proj_with_tracker("a", Arc::clone(&tr_a), "pa");
        pa.repo = "git@github.com:o/r.git".into();
        let (mut o, dispatched) = orch_for_retry_multi(vec![pa], 10);
        o.claimed.insert("a1".into());
        let mut re = retry_entry("a1", "A-1", 1);
        re.project_slug = "a".into();
        re.project_repo = "git@github.com:o/r.git".into();
        o.retry_attempts.insert("a1".into(), re);
        o.on_retry(EvRetry {
            issue_id: "a1".into(),
        })
        .await;
        let d = dispatched.lock().unwrap();
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].project_slug, "a");
        assert_eq!(d[0].project_repo, "git@github.com:o/r.git");
    }
}
