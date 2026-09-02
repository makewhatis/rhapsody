//! reconcile_run — parity port of Go `internal/orchestrator/reconcile_run.go` (the reconcile APPLY
//! side, upstream §8.5).
//!
//! [`Orchestrator::reconcile`] runs stall detection then refreshes running-issue states and applies
//! the resulting [`crate::reconcile::reconcile_actions`], grouping running issues by their owning
//! project and refreshing each group via that project's slug-bound tracker (single-project / test
//! mode degenerates to one group on the top-level tracker). A terminal move cleans the workspace +
//! releases the claim; a non-terminal move only refreshes the in-memory snapshot (INF-266 — the
//! worker keeps running and its exit is classified by [`Orchestrator::on_worker_exit`]).
//!
//! Deviations from the Go source, all behavior-preserving:
//!   * `reconcile` snapshots each group's reconcile inputs (slug tracker, canceled/terminal sets,
//!     workspace manager) into owned values before the tracker/workspace `await`s and the state
//!     mutations — Go aliases the `resolvedProject`/`effective` via pointers, which Rust's borrow
//!     checker forbids across `&mut self`. Resolved projects are still visited in declaration order.
//!   * `terminate` drops Go's `outcome` parameter: it labeled only the `run.duration` metric (P6,
//!     dropped); the caller still passes the outcome to `persist_end_run`. The worker task abort
//!     (Go `re.cancel()`) is O7's (dropped-future cancellation — see `worker.rs`).
//!   * `sample_cpu` is a free function taking the sampler explicitly (extracted from `eff`) so the
//!     per-entry CPU sampling doesn't hold a borrow of `self.eff` while mutating `self.running`.
//!
//! Startup terminal cleanup (Go `loop.go` `startupCleanup`, exercised by
//! `reconcile_multi_test.go`'s `TestStartupCleanupMultiPerProject`) lives in `loop.go` = O7; it is
//! mirrored with the control loop there, not in O5.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use rhapsody_core::normalize_state;
use rhapsody_store as store;
use rhapsody_tracker::Tracker;
use rhapsody_workspace::Manager;

use crate::backoff::failure_backoff_ms;
use crate::liveness::Sampler;
use crate::orchestrator::{Orchestrator, RunningEntry, zero_time};
use crate::reconcile::{ActionKind, reconcile_actions};
use crate::retry::RetryTarget;

/// One project group's reconcile inputs, snapshotted into owned values so [`Orchestrator::reconcile`]
/// holds no borrow of `self.eff` across the per-group tracker/workspace `await`s and state mutations.
struct GroupInputs {
    ids: Vec<String>,
    tracker: Arc<dyn Tracker>,
    canceled: HashSet<String>,
    terminal: HashSet<String>,
    workspace: Arc<Manager>,
}

/// Updates `re.last_cpu_active_at` from the process-group CPU sampler. Any change in the group's
/// cumulative tick sum (up OR down — a finished child drops out of the sum) counts as activity; an
/// unreadable sampler assumes the run is alive rather than falling back to event-silence. Mirrors Go
/// `sampleCPU` (a free function here, taking the sampler explicitly so it needs no `&self`).
fn sample_cpu(re: &mut RunningEntry, now: DateTime<Utc>, sampler: Option<&dyn Sampler>) {
    if re.pgid == 0 {
        return;
    }
    let Some(sampler) = sampler else {
        return;
    };
    match sampler.group_cpu(re.pgid) {
        None => re.last_cpu_active_at = now, // degrade: assume alive
        Some(ticks) => {
            if !re.cpu_sampled || ticks != re.last_cpu_ticks {
                re.last_cpu_active_at = now;
            }
            re.last_cpu_ticks = ticks;
            re.cpu_sampled = true;
        }
    }
}

impl Orchestrator {
    /// Runs stall detection then refreshes running-issue states and applies the resulting actions
    /// (upstream §8.5), grouping running issues by their owning project and refreshing each group via
    /// that project's slug-bound tracker. Mirrors Go `reconcile`. A control-loop (O7) entry point.
    pub async fn reconcile(&mut self) {
        self.reconcile_stalled();

        if self.running.is_empty() {
            return;
        }

        // Group running IDs by project slug. Issues with no stamped slug (legacy / test-injected) land
        // in the "" group, handled by the top-level tracker/sets.
        let mut groups: HashMap<String, Vec<String>> = HashMap::new();
        for (id, re) in &self.running {
            groups
                .entry(re.project_slug.clone())
                .or_default()
                .push(id.clone());
        }

        // Snapshot each group's reconcile inputs, visiting resolved projects in declaration order (so
        // reconcile actions / tracker calls are reproducible across ticks) then the remaining groups
        // (legacy "" group, or a slug whose project was removed on reload) on the top-level tracker.
        let ordered: Vec<GroupInputs> = {
            let Some(eff) = self.eff.as_ref() else {
                return;
            };
            let mut ordered = Vec::new();
            for p in &eff.projects {
                if let Some(ids) = groups.remove(&p.slug) {
                    ordered.push(GroupInputs {
                        ids,
                        tracker: Arc::clone(&p.tracker),
                        canceled: p.canceled_states.clone(),
                        terminal: p.terminal_states.clone(),
                        workspace: Arc::clone(&p.workspace),
                    });
                }
            }
            for (_slug, ids) in groups.drain() {
                ordered.push(GroupInputs {
                    ids,
                    tracker: Arc::clone(&eff.tracker),
                    canceled: eff.canceled_states.clone(),
                    terminal: eff.terminal_states.clone(),
                    workspace: Arc::clone(&eff.workspace),
                });
            }
            ordered
        };

        for grp in ordered {
            self.reconcile_group(
                &grp.ids,
                &grp.tracker,
                &grp.canceled,
                &grp.terminal,
                &grp.workspace,
            )
            .await;
        }
    }

    /// Refreshes one project group's running issues and applies the resulting actions, classifying
    /// against the group's canceled/terminal sets and cleaning workspaces via the group's workspace
    /// manager. Non-terminal states only refresh the in-memory snapshot (INF-266 / INF-272). Mirrors
    /// Go `reconcileGroup`.
    async fn reconcile_group(
        &mut self,
        ids: &[String],
        tr: &Arc<dyn Tracker>,
        canceled: &HashSet<String>,
        terminal: &HashSet<String>,
        ws: &Arc<Manager>,
    ) {
        let refreshed = match tr.fetch_issue_states_by_ids(ids).await {
            Ok(r) => r,
            Err(_) => {
                tracing::debug!("running-state refresh failed; keeping workers");
                return;
            }
        };
        for act in reconcile_actions(ids, &refreshed, terminal) {
            match act.kind {
                ActionKind::TerminateCleanup => {
                    // A Done-type terminal records `completed`; a cancel-type terminal (in
                    // canceled_states AND terminal) records `stopped`. Drop the retry row + claim
                    // either way (taxonomy v2, INF-272).
                    let st = normalize_state(&act.new_state);
                    let (outcome, reason) = if canceled.contains(&st) && terminal.contains(&st) {
                        (store::OUTCOME_STOPPED, "ticket cancelled")
                    } else {
                        (store::OUTCOME_COMPLETED, "")
                    };
                    if let Some(re) = self.terminate(&act.issue_id) {
                        tracing::info!(issue_id = %act.issue_id, issue_identifier = %re.issue.identifier, "issue terminal; terminating and cleaning workspace");
                        // remove_worktree prunes the repo-backed worktree+mirror admin when a repo is
                        // configured; with no repo (project_repo == "") it delegates to the legacy remove.
                        let _ = ws
                            .remove_worktree(
                                &re.project_repo,
                                &re.project_slug,
                                &re.issue.identifier,
                            )
                            .await;
                        self.claimed.remove(&act.issue_id);
                        self.completed.remove(&act.issue_id);
                        self.persist_end_run(&re, outcome, reason);
                        self.persist_complete(&re.issue.identifier);
                        self.persist_totals();
                    }
                }
                ActionKind::UpdateState => {
                    if let Some(re) = self.running.get_mut(&act.issue_id) {
                        re.issue.state = act.new_state;
                    }
                }
            }
        }
    }

    /// Terminates workers that show no liveness — neither a recent stream event nor recent
    /// process-group CPU activity — and retries them (upstream §8.5 Part A). The stall timeout is read
    /// per running entry from its owning project (falling back to the top-level); an entry whose
    /// effective stall timeout is `<= 0` is skipped. Mirrors Go `reconcileStalled`.
    pub(crate) fn reconcile_stalled(&mut self) {
        let now = (self.now)();
        // Snapshot the sampler so CPU sampling doesn't hold a borrow of `self.eff` while mutating
        // `self.running`; resolve each entry's stall timeout up front (also an `self.eff` read).
        let sampler = self.eff.as_ref().map(|e| Arc::clone(&e.cpu_sampler));
        let timeouts: HashMap<String, std::time::Duration> = self
            .running
            .iter()
            .map(|(id, re)| (id.clone(), self.stall_timeout_for(re)))
            .collect();

        let mut wedged: Vec<String> = Vec::new();
        for (id, re) in self.running.iter_mut() {
            let Some(stall) = timeouts.get(id).copied() else {
                continue;
            };
            if stall.is_zero() {
                continue; // stall detection disabled for this entry
            }
            sample_cpu(re, now, sampler.as_deref());

            let mut last = re.last_event_at;
            if last == zero_time() {
                last = re.started_at;
            }
            if re.last_cpu_active_at > last {
                last = re.last_cpu_active_at;
            }
            let stall = chrono::Duration::from_std(stall).unwrap_or(chrono::Duration::MAX);
            if now - last > stall {
                wedged.push(id.clone());
            }
        }

        for id in wedged {
            let Some(re) = self.terminate(&id) else {
                continue;
            };
            let attempt = re.retry_attempt + 1;
            tracing::warn!(
                issue_id = %id, issue_identifier = %re.issue.identifier, run_id = re.run_id,
                session_id = %re.session_id, reason = "liveness_flatline", pgid = re.pgid,
                "worker wedged (no events and no CPU); terminating and retrying"
            );
            // A stall is a genuine failure for backoff purposes: drop any stale continuation marker so
            // the follow-up retry uses exponential failure backoff, not the short continuation cadence.
            self.completed.remove(&id);
            // The `stalled` OUTCOME folds into `failed` (taxonomy v2, INF-272); the "stalled" reason
            // string survives so the UI can show `failed · stalled`.
            self.persist_end_run(&re, store::OUTCOME_FAILED, "stalled");
            self.persist_totals();
            let max_backoff = self.eff.as_ref().map_or(0, |e| e.max_retry_backoff_ms);
            self.schedule_retry_for(
                RetryTarget {
                    id: &id,
                    identifier: &re.issue.identifier,
                    project_slug: &re.project_slug,
                    project_repo: &re.project_repo,
                },
                attempt,
                failure_backoff_ms(attempt, max_backoff),
                "stalled",
                re.issue.clone(),
            );
        }
    }

    /// Returns the stall timeout for a running entry: its owning project's timeout when stamped and
    /// still resolvable, else the top-level stall timeout. Mirrors Go `stallTimeoutFor`.
    pub(crate) fn stall_timeout_for(&self, re: &RunningEntry) -> std::time::Duration {
        if !re.project_slug.is_empty()
            && let Some(p) = self
                .eff
                .as_ref()
                .and_then(|e| e.project_by_slug(&re.project_slug))
        {
            return p.stall_timeout;
        }
        self.eff
            .as_ref()
            .map_or(std::time::Duration::ZERO, |e| e.stall_timeout)
    }

    /// Returns the active / canceled / terminal / review state sets for a running entry: its owning
    /// project's when stamped and resolvable, else the top-level sets (mirrors [`stall_timeout_for`]).
    /// Used by [`Orchestrator::on_worker_exit`] to classify a clean exit (INF-266 / INF-272 /
    /// TRA-279). Returns owned clones so the caller holds no borrow of `self.eff`. Mirrors Go
    /// `statesFor`, plus the `review` set Go's classifier never received (TRA-279) — it is read from
    /// the SAME resolved/effective source as the other three, so a per-project `review_states`
    /// override is honored rather than the global tracker config.
    pub(crate) fn states_for(
        &self,
        re: &RunningEntry,
    ) -> (
        HashSet<String>,
        HashSet<String>,
        HashSet<String>,
        HashSet<String>,
    ) {
        if !re.project_slug.is_empty()
            && let Some(p) = self
                .eff
                .as_ref()
                .and_then(|e| e.project_by_slug(&re.project_slug))
        {
            return (
                p.active_states.clone(),
                p.canceled_states.clone(),
                p.terminal_states.clone(),
                p.review_states.clone(),
            );
        }
        match self.eff.as_ref() {
            Some(e) => (
                e.active_states.clone(),
                e.canceled_states.clone(),
                e.terminal_states.clone(),
                e.review_states.clone(),
            ),
            None => (
                HashSet::new(),
                HashSet::new(),
                HashSet::new(),
                HashSet::new(),
            ),
        }
    }

    /// Removes a running worker's entry, FIRES its cancellation (Go `re.cancel()` → SIGKILL the claude
    /// process group), accumulating its runtime, and returns it (or `None` if not running). The later
    /// worker-exit event becomes a no-op (entry already gone). Mirrors Go `terminate` (minus the
    /// `outcome` metric label, which is dropped telemetry).
    pub(crate) fn terminate(&mut self, id: &str) -> Option<RunningEntry> {
        let re = self.running.remove(id)?;
        self.release_teams_run(&re);
        // A review run's detached worktree is reclaimed at its EXIT, and a termination is the one
        // way a review ends without reaching that exit: the entry is already gone, so the worker's
        // later exit event returns at the stale/absent guard, before `on_worker_exit`'s teardown
        // (STUDIO-716). Nothing downstream could name the tree afterwards — a `pr:` id reaches no
        // terminal tracker state, so `TerminateCleanup` never fires for it either — so an operator
        // Stop used to leak it permanently. Review runs only: a stopped or stalled TICKET keeps its
        // workspace, which `reconcile_stalled` retries straight back into.
        if let Some(run) = re.review.as_ref() {
            self.teardown_review_worktree(run, &re.project_slug);
        }
        re.cancel.cancel();
        let dur = ((self.now)() - re.started_at)
            .num_nanoseconds()
            .unwrap_or(0) as f64
            / 1e9;
        self.totals.seconds_running += dur;
        Some(re)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agentupdate::AgentUpdate;
    use crate::retry::EvRetry;
    use crate::testsupport::*;
    use rhapsody_agent as agent;
    use rhapsody_core::Issue;
    use rhapsody_tracker::TrackerError;
    use rhapsody_tracker::fake::Fake;

    // NOTE (O7): reconcile_multi_test.go's TestStartupCleanupMultiPerProject exercises
    // `startupCleanup`, which lives in Go `loop.go` (O7's file); it is mirrored with the control loop
    // in O7, not here.

    /// A programmable `fetch_issue_states_by_ids` override (the `Fake`'s `states_by_ids_func` shape).
    type StatesByIds = Box<dyn Fn(&[String]) -> Result<Vec<Issue>, TrackerError> + Send + Sync>;

    /// A `states_by_ids_func` override that always returns `issues`.
    fn states_ok(issues: Vec<Issue>) -> StatesByIds {
        Box::new(move |_ids| Ok(issues.clone()))
    }

    // Mirrors Go `TestReconcileTerminalCleansAndReleases`.
    #[tokio::test]
    async fn reconcile_terminal_cleans_and_releases() {
        let mut f = Fake::new();
        f.states_by_ids_func = Some(states_ok(vec![issue("1", "MT-1", "Done")]));
        let tr = Arc::new(f);
        let (mut o, _dir) = orch_for_reconcile(Arc::clone(&tr), std::time::Duration::ZERO);
        let mgr = Arc::clone(&o.eff.as_ref().unwrap().workspace);
        let ws = mgr.create_for_issue("", "MT-1").await.expect("create ws");
        add_running(&mut o, "1", "MT-1", "In Progress", Utc::now());
        o.reconcile().await;
        assert!(
            !o.running.contains_key("1") && !o.claimed.contains("1"),
            "terminal issue should be terminated and released"
        );
        assert!(
            std::fs::metadata(&ws.path).is_err(),
            "workspace should be removed"
        );
    }

    // Mirrors Go `TestReconcileActiveUpdatesState`.
    #[tokio::test]
    async fn reconcile_active_updates_state() {
        let mut f = Fake::new();
        f.states_by_ids_func = Some(states_ok(vec![issue("1", "MT-1", "In Progress")]));
        let tr = Arc::new(f);
        let (mut o, _dir) = orch_for_reconcile(Arc::clone(&tr), std::time::Duration::ZERO);
        add_running(&mut o, "1", "MT-1", "Todo", Utc::now());
        o.reconcile().await;
        assert_eq!(
            o.running.get("1").expect("running").issue.state,
            "In Progress",
            "active issue snapshot should be updated"
        );
    }

    // Mirrors Go `TestReconcileNonTerminalKeepsWorkerRunning` (INF-266).
    #[tokio::test]
    async fn reconcile_non_terminal_keeps_worker_running() {
        let mut f = Fake::new();
        f.states_by_ids_func = Some(states_ok(vec![issue("1", "MT-1", "In Review")]));
        let tr = Arc::new(f);
        let (mut o, _dir) = orch_for_reconcile(Arc::clone(&tr), std::time::Duration::ZERO);
        let mgr = Arc::clone(&o.eff.as_ref().unwrap().workspace);
        let ws = mgr.create_for_issue("", "MT-1").await.expect("create ws");
        add_running(&mut o, "1", "MT-1", "In Progress", Utc::now());
        o.reconcile().await;
        let re = o.running.get("1").expect("worker kept running");
        assert_eq!(
            re.issue.state, "In Review",
            "snapshot should refresh to the new state"
        );
        assert!(
            o.claimed.contains("1"),
            "claim must be kept while the worker runs"
        );
        assert!(
            std::fs::metadata(&ws.path).is_ok(),
            "workspace must NOT be removed"
        );
    }

    // Mirrors Go `TestReconcileStallTerminatesAndRetries`.
    #[tokio::test]
    async fn reconcile_stall_terminates_and_retries() {
        let mut f = Fake::new();
        f.states_by_ids_func = Some(states_ok(vec![]));
        let tr = Arc::new(f);
        let (mut o, _dir) =
            orch_for_reconcile(Arc::clone(&tr), std::time::Duration::from_millis(100));
        add_running(
            &mut o,
            "1",
            "MT-1",
            "In Progress",
            Utc::now() - chrono::Duration::hours(1),
        ); // long-stale
        o.reconcile().await;
        assert!(
            !o.running.contains_key("1"),
            "stalled worker should be terminated"
        );
        assert!(
            o.retry_attempts.contains_key("1"),
            "stalled worker should be retried"
        );
    }

    // Mirrors Go `TestReconcileStallClearsContinuationMarker`.
    #[tokio::test]
    async fn reconcile_stall_clears_continuation_marker() {
        let mut f = Fake::new();
        f.states_by_ids_func = Some(states_ok(vec![]));
        f.candidates = vec![issue("1", "MT-1", "In Progress")]; // for the follow-up on_retry
        let tr = Arc::new(f);
        let (mut o, _dir) =
            orch_for_reconcile(Arc::clone(&tr), std::time::Duration::from_millis(100));
        o.eff.as_mut().unwrap().max_concurrent = 0; // force slot exhaustion in the follow-up on_retry
        add_running(
            &mut o,
            "1",
            "MT-1",
            "In Progress",
            Utc::now() - chrono::Duration::hours(1),
        );
        o.completed.insert("1".into()); // this worker had cleanly exited and been re-dispatched

        o.reconcile_stalled();

        assert!(
            !o.completed.contains("1"),
            "reconcile_stalled must clear the continuation marker"
        );
        assert!(
            o.retry_attempts.contains_key("1"),
            "stalled worker should have a pending retry"
        );

        // Drive the follow-up retry under slot exhaustion: it must take the failure backoff path.
        o.on_retry(EvRetry {
            issue_id: "1".into(),
        })
        .await;
        let got = o
            .retry_attempts
            .get("1")
            .expect("rescheduled retry under slot exhaustion");
        assert_eq!(
            got.err, "no available orchestrator slots",
            "stall retry must use the failure backoff path"
        );
    }

    // Mirrors Go `TestReconcileRefreshErrorKeepsWorkers`.
    #[tokio::test]
    async fn reconcile_refresh_error_keeps_workers() {
        let mut f = Fake::new();
        f.states_by_ids_func = Some(Box::new(|_ids| Err(TrackerError::Other("boom".into()))));
        let tr = Arc::new(f);
        let (mut o, _dir) = orch_for_reconcile(Arc::clone(&tr), std::time::Duration::ZERO);
        add_running(&mut o, "1", "MT-1", "In Progress", Utc::now());
        o.reconcile().await;
        assert!(
            o.running.contains_key("1"),
            "refresh failure must keep workers running"
        );
    }

    // --- liveness (reconcile_liveness_test.go) -------------------------------------------------

    /// Installs a pinned, mutable clock on `o` (reading returns the current value) and returns its
    /// handle so a test can [`advance`] it — the Rust analogue of Go's `now := base; o.now = func() {
    /// return now }` with `now = now.Add(...)`.
    fn install_clock(
        o: &mut Orchestrator,
        base: DateTime<Utc>,
    ) -> std::sync::Arc<std::sync::Mutex<DateTime<Utc>>> {
        let clock = std::sync::Arc::new(std::sync::Mutex::new(base));
        let c = std::sync::Arc::clone(&clock);
        o.now = Box::new(move || *c.lock().expect("clock lock"));
        clock
    }

    /// Moves a pinned clock forward by `by`.
    fn advance(clock: &std::sync::Arc<std::sync::Mutex<DateTime<Utc>>>, by: chrono::Duration) {
        let mut g = clock.lock().expect("clock lock");
        *g += by;
    }

    fn liveness_orch(stall: std::time::Duration) -> (Orchestrator, TempDir) {
        let mut f = Fake::new();
        f.states_by_ids_func = Some(states_ok(vec![issue("1", "MT-1", "In Progress")]));
        orch_for_reconcile(Arc::new(f), stall)
    }

    fn sampler(ok: bool, seq: &[(i32, &[u64])]) -> std::sync::Arc<dyn Sampler> {
        let map: std::collections::HashMap<i32, Vec<u64>> =
            seq.iter().map(|(k, v)| (*k, v.to_vec())).collect();
        std::sync::Arc::new(FakeSampler::new(ok, map))
    }

    // Mirrors Go `TestReconcileLivenessCPUActiveSurvives`.
    #[tokio::test]
    async fn reconcile_liveness_cpu_active_survives() {
        let (mut o, _dir) = liveness_orch(std::time::Duration::from_millis(100));
        o.eff.as_mut().unwrap().cpu_sampler = sampler(true, &[(42, &[100, 200, 300, 400, 500])]);
        let base = utc(2026, 5, 29, 12, 0, 0);
        let clock = install_clock(&mut o, base);
        add_running(&mut o, "1", "MT-1", "In Progress", base);
        o.running.get_mut("1").unwrap().pgid = 42;
        o.running.get_mut("1").unwrap().last_event_at = base; // event stream goes silent
        for i in 0..4 {
            advance(&clock, chrono::Duration::seconds(1)); // far beyond the 100ms stall window
            o.reconcile().await;
            assert!(
                o.running.contains_key("1"),
                "CPU-active run wedged on tick {i}"
            );
        }
    }

    // Mirrors Go `TestReconcileLivenessFlatWedges`.
    #[tokio::test]
    async fn reconcile_liveness_flat_wedges() {
        let (mut o, _dir) = liveness_orch(std::time::Duration::from_millis(100));
        o.eff.as_mut().unwrap().cpu_sampler = sampler(true, &[(42, &[500])]); // constant
        let base = utc(2026, 5, 29, 12, 0, 0);
        let clock = install_clock(&mut o, base);
        add_running(&mut o, "1", "MT-1", "In Progress", base);
        o.running.get_mut("1").unwrap().pgid = 42;
        o.running.get_mut("1").unwrap().last_event_at = base;

        // First tick establishes the CPU baseline (active at base).
        o.reconcile().await;
        assert!(
            o.running.contains_key("1"),
            "run should survive the baseline tick"
        );
        // Advance past the stall window with no CPU change and no events.
        advance(&clock, chrono::Duration::milliseconds(200));
        o.reconcile().await;
        assert!(
            !o.running.contains_key("1"),
            "run flat on events and CPU should be wedged"
        );
        assert!(
            o.retry_attempts.contains_key("1"),
            "wedged run should be retried"
        );
    }

    // Mirrors Go `TestReconcileLivenessDecreasingSumStillActive`.
    #[tokio::test]
    async fn reconcile_liveness_decreasing_sum_still_active() {
        let (mut o, _dir) = liveness_orch(std::time::Duration::from_millis(100));
        o.eff.as_mut().unwrap().cpu_sampler = sampler(true, &[(42, &[500, 300, 200, 100])]);
        let base = utc(2026, 5, 29, 12, 0, 0);
        let clock = install_clock(&mut o, base);
        add_running(&mut o, "1", "MT-1", "In Progress", base);
        o.running.get_mut("1").unwrap().pgid = 42;
        o.running.get_mut("1").unwrap().last_event_at = base;
        for i in 0..3 {
            advance(&clock, chrono::Duration::seconds(1));
            o.reconcile().await;
            assert!(
                o.running.contains_key("1"),
                "decreasing-but-changing CPU must count as active (tick {i})"
            );
        }
    }

    // Mirrors Go `TestReconcileLivenessDegradesAssumeAlive`.
    #[tokio::test]
    async fn reconcile_liveness_degrades_assume_alive() {
        let (mut o, _dir) = liveness_orch(std::time::Duration::from_millis(100));
        o.eff.as_mut().unwrap().cpu_sampler = sampler(false, &[]); // unreadable /proc
        let base = utc(2026, 5, 29, 12, 0, 0);
        install_clock(&mut o, base);
        add_running(
            &mut o,
            "1",
            "MT-1",
            "In Progress",
            base - chrono::Duration::hours(1),
        );
        o.running.get_mut("1").unwrap().pgid = 42;
        o.running.get_mut("1").unwrap().last_event_at = base - chrono::Duration::hours(1);
        o.reconcile().await;
        assert!(
            o.running.contains_key("1"),
            "degraded sampler must keep the run alive"
        );
    }

    // Mirrors Go `TestReconcileLivenessFollowsPgidAcrossTurns`.
    #[tokio::test]
    async fn reconcile_liveness_follows_pgid_across_turns() {
        let (mut o, _dir) = liveness_orch(std::time::Duration::from_millis(100));
        o.eff.as_mut().unwrap().cpu_sampler = sampler(
            true,
            &[
                (10, &[100]),                // turn-1 group: constant (would look flat)
                (20, &[500, 600, 700, 800]), // turn-2 group: actively changing
            ],
        );
        let base = utc(2026, 5, 29, 12, 0, 0);
        let clock = install_clock(&mut o, base);
        add_running(&mut o, "1", "MT-1", "In Progress", base);
        // Turn 1 process announces itself.
        o.on_agent_update(AgentUpdate {
            issue_id: "1".into(),
            ev: agent::Event {
                event_type: agent::EVENT_SESSION_STARTED.to_string(),
                pid: 10,
                ..Default::default()
            },
        });
        // Turn boundary: a new process (group 20) announces itself one second later.
        advance(&clock, chrono::Duration::seconds(1));
        o.on_agent_update(AgentUpdate {
            issue_id: "1".into(),
            ev: agent::Event {
                event_type: agent::EVENT_SESSION_STARTED.to_string(),
                pid: 20,
                ..Default::default()
            },
        });
        assert_eq!(
            o.running["1"].pgid, 20,
            "pgid should follow the new turn's process"
        );
        // Event-silent, but turn-2's group keeps using CPU → must stay alive.
        for i in 0..3 {
            advance(&clock, chrono::Duration::seconds(1));
            o.reconcile().await;
            assert!(
                o.running.contains_key("1"),
                "run on active turn-2 group wedged (tick {i})"
            );
        }
    }

    // --- multi-project (reconcile_multi_test.go) ------------------------------------------------

    /// Builds a resolved project with a states-by-ids override, active/terminal sets, and a fresh
    /// workspace manager (returning the backing TempDir to keep alive). Mirrors the inline
    /// `resolvedProject{…}` literals in reconcile_multi_test.
    fn recon_project(
        slug: &str,
        tr: Arc<Fake>,
        active: &[&str],
        terminal: &[&str],
    ) -> (crate::effective::ResolvedProject, TempDir) {
        let dir = TempDir::new();
        let mut p = empty_resolved_project(slug, tr);
        p.active_states = set_of(active);
        p.terminal_states = set_of(terminal);
        p.max_concurrent = 10;
        p.workspace = mk_workspace(&dir.path);
        (p, dir)
    }

    // Mirrors Go `TestReconcileGroupsByProject`.
    #[tokio::test]
    async fn reconcile_groups_by_project() {
        let mut fa = Fake::new();
        fa.states_by_ids_func = Some(states_ok(vec![issue("a1", "A-1", "Done")])); // A: terminal
        let tr_a = Arc::new(fa);
        let mut fb = Fake::new();
        fb.states_by_ids_func = Some(states_ok(vec![issue("b1", "B-1", "In Progress")])); // B: active
        let tr_b = Arc::new(fb);
        let (pa, _da) = recon_project("a", Arc::clone(&tr_a), &["in progress"], &["done"]);
        let (pb, _db) = recon_project("b", Arc::clone(&tr_b), &["in progress"], &["done"]);
        let mut eff = empty_effective(Arc::new(Fake::new()));
        eff.max_concurrent = 10;
        eff.max_retry_backoff_ms = 300_000;
        eff.projects = vec![pa, pb];
        let mut o = Orchestrator::new("WORKFLOW.md");
        o.eff = Some(eff);
        o.spawn = Some(Box::new(|_, _, _| {}));
        o.running.insert(
            "a1".into(),
            running_entry(issue("a1", "A-1", "In Progress"), "a", "a"),
        );
        o.running.insert(
            "b1".into(),
            running_entry(issue("b1", "B-1", "In Progress"), "b", "b"),
        );
        o.claimed.insert("a1".into());
        o.claimed.insert("b1".into());

        o.reconcile().await;

        assert!(
            !o.running.contains_key("a1") && !o.claimed.contains("a1"),
            "A's terminal issue terminated + released"
        );
        assert!(
            o.running.contains_key("b1"),
            "B's active issue should keep running"
        );
        assert_eq!(tr_a.by_id_calls(), 1, "project A tracker refreshed once");
        assert_eq!(tr_b.by_id_calls(), 1, "project B tracker refreshed once");
    }

    // Mirrors Go `TestReconcileHandoffUsesProjectEffectiveStates`.
    #[tokio::test]
    async fn reconcile_handoff_uses_project_effective_states() {
        let mut fa = Fake::new();
        fa.states_by_ids_func = Some(states_ok(vec![issue("a1", "A-1", "Shipped")]));
        let tr_a = Arc::new(fa);
        // "Shipped" is terminal for project A.
        let (pa, _da) = recon_project("a", Arc::clone(&tr_a), &["started"], &["shipped"]);
        let mgr = Arc::clone(&pa.workspace);
        let mut eff = empty_effective(Arc::new(Fake::new()));
        eff.max_concurrent = 10;
        eff.max_retry_backoff_ms = 300_000;
        eff.projects = vec![pa];
        let mut o = Orchestrator::new("WORKFLOW.md");
        o.eff = Some(eff);
        o.spawn = Some(Box::new(|_, _, _| {}));
        let ws = mgr.create_for_issue("", "A-1").await.expect("create ws");
        o.running.insert(
            "a1".into(),
            running_entry(issue("a1", "A-1", "Started"), "a", "a"),
        );
        o.claimed.insert("a1".into());

        o.reconcile().await;

        assert!(
            !o.running.contains_key("a1"),
            "terminal-for-A issue should be terminated"
        );
        assert!(
            std::fs::metadata(&ws.path).is_err(),
            "workspace should be removed (terminal cleanup)"
        );
    }
}
