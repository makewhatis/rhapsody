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
//! Intentional liveness divergences (STUDIO-1177 / STUDIO-1208 / STUDIO-1262): thresholded CPU activity,
//! portable tool lifecycle, worker emission time and widened sampling failure grace; see README.
//!
//! Other deviations from the Go source, all behavior-preserving:
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
use crate::effective::stall_timeout_for_harness;
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

fn event_liveness_at(re: &RunningEntry) -> DateTime<Utc> {
    // Entries with no worker emission yet retain the startup / legacy event fallback.
    let last = re.worker_emit_time.get().unwrap_or(re.last_event_at);
    if last == zero_time() {
        re.started_at
    } else {
        last
    }
}

/// Measures CPU work over at least one second (STUDIO-1177). Failed sampling keeps
/// the bounded event fallback; tool liveness comes from the harness, not children.
fn sample_cpu(
    re: &mut RunningEntry,
    now: DateTime<Utc>,
    sampler: Option<&dyn Sampler>,
    threshold: f64,
) {
    if re.pgid == 0 {
        return;
    }
    let Some(sampler) = sampler else {
        return;
    };
    // A descendant is not proof of a tool wait: MCP servers are long-lived even
    // when the model stream is dead. Only actual tool lifecycle grants that credit.
    match sampler
        .group_cpu(re.pgid)
        .zip(sampler.ticks_per_second())
        .filter(|(_, hz)| *hz > 0)
    {
        None => {
            re.cpu_failure_since.get_or_insert(now);
            re.cpu_sampled = false;
            if !re.cpu_failure_logged {
                tracing::warn!(run_id = re.run_id, issue_identifier = %re.issue.identifier,
                    pgid = re.pgid, "CPU sampler unavailable; using events with twice the stall window (CPU sampling requires Linux /proc)");
                re.cpu_failure_logged = true;
            }
        }
        Some((ticks, hz)) => {
            let elapsed = now - re.last_cpu_sample_at;
            if re.cpu_sampled
                && ticks >= re.last_cpu_ticks
                && elapsed < chrono::Duration::seconds(1)
            {
                return; // accumulate ticks and wall time until a stable interval is available
            }
            if re.cpu_sampled
                && ticks >= re.last_cpu_ticks
                && elapsed >= chrono::Duration::seconds(1)
            {
                // A recovery baseline cannot measure work: retain the original bounded grace
                // until a full interval is available, without crediting unmeasured CPU activity.
                re.cpu_failure_since = None;
                if ticks > re.last_cpu_ticks
                    && (ticks - re.last_cpu_ticks) as f64 / hz as f64
                        > elapsed.num_milliseconds() as f64 / 1000.0 * threshold
                {
                    re.last_cpu_active_at = now;
                }
            }
            re.last_cpu_ticks = ticks;
            re.last_cpu_sample_at = now;
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
            if crate::review::is_non_tracker_key(id) {
                continue;
            }
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
                        // AFTER the worktree removal: the terminated worker's future is dropped
                        // during that await, and its session `Drop` would otherwise re-record the
                        // retained session after this discard removed it (STUDIO-1043 review B1).
                        self.discard_retained_opencode_session(&re.issue.identifier);
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
    /// process-group CPU activity or an open tool call — and retries them (upstream §8.5 Part A). The stall timeout is read
    /// per running entry from its owning project (falling back to the top-level); an entry whose
    /// effective stall timeout is `<= 0` is skipped. Mirrors Go `reconcileStalled`.
    pub(crate) fn reconcile_stalled(&mut self) {
        let now = (self.now)();
        // Snapshot the sampler so CPU sampling doesn't hold a borrow of `self.eff` while mutating
        // `self.running`; resolve each entry's stall timeout up front (also an `self.eff` read).
        let sampler = self.eff.as_ref().map(|e| Arc::clone(&e.cpu_sampler));
        let threshold = self
            .eff
            .as_ref()
            .map_or(0.05, |e| e.cfg.agent.stall_cpu_threshold);
        let timeouts: HashMap<String, std::time::Duration> = self
            .running
            .iter()
            .map(|(id, re)| (id.clone(), self.stall_timeout_for(re)))
            .collect();

        let mut wedged = Vec::new();
        for (id, re) in self.running.iter_mut() {
            if re.terminal_result_seen {
                continue; // runner owns bounded post-result cleanup; never retry a captured result
            }
            if self.limit_policy.deadlines.contains_key(id) {
                continue;
            }
            let Some(stall) = timeouts.get(id).copied() else {
                continue;
            };
            if stall.is_zero() {
                continue; // stall detection disabled for this entry
            }
            sample_cpu(re, now, sampler.as_deref(), threshold);
            // Portable lifecycle evidence, independent of /proc and CPU. The
            // turn deadline still bounds tools that never return.
            if !re.open_tool_calls.is_empty() {
                continue;
            }

            let mut last = event_liveness_at(re);
            let stall = chrono::Duration::from_std(stall).unwrap_or(chrono::Duration::MAX);
            let stall = if re.cpu_failure_since.is_some() {
                stall.checked_mul(2).unwrap_or(chrono::Duration::MAX)
            } else {
                stall
            };
            if re.cpu_failure_since.is_none() && re.last_cpu_active_at > last {
                last = re.last_cpu_active_at;
            }
            if let Some(since) = re.cpu_failure_since
                && now - since <= stall
            {
                continue;
            }
            if now - last > stall {
                wedged.push((id.clone(), stall));
            }
        }

        for (id, stall) in wedged {
            // Other groups' CPU scans can delay this action after the first verdict. Re-read
            // the shared watermark at the cancellation boundary so intervening work wins.
            if self
                .running
                .get(&id)
                .is_some_and(|re| now - event_liveness_at(re) <= stall)
            {
                continue;
            }
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
            if re.landing.is_some() {
                self.claimed.remove(&id);
                self.persist_complete(&re.issue.identifier);
                continue; // the durable landing, rather than tracker retry, consumes this failure
            }
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
                re.identity.clone(),
            );
        }
    }

    /// Returns the stall timeout for a running entry: its owning project's timeout when stamped and
    /// still resolvable, else the top-level stall timeout. Mirrors Go `stallTimeoutFor`.
    ///
    /// Plus the harness the run was DISPATCHED on, which Go has no notion of: when `re.harness`
    /// names a backend the resolved config's own `agent.backend` is not (STUDIO-902's headline
    /// arrangement — `agent.backend: claude` with one teammate's profile naming `harness:
    /// opencode`), that backend's own `stall_timeout_ms` wins over the config's precomputed one.
    /// Every run that names no harness, or names the configured one, is unchanged — see
    /// [`crate::effective::stall_timeout_for_harness`].
    pub(crate) fn stall_timeout_for(&self, re: &RunningEntry) -> std::time::Duration {
        if !re.project_slug.is_empty()
            && let Some(p) = self
                .eff
                .as_ref()
                .and_then(|e| e.project_by_slug(&re.project_slug))
        {
            return stall_timeout_for_harness(&p.mcfg, &re.harness).unwrap_or(p.stall_timeout);
        }
        self.eff.as_ref().map_or(std::time::Duration::ZERO, |e| {
            stall_timeout_for_harness(&e.cfg, &re.harness).unwrap_or(e.stall_timeout)
        })
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
        self.limit_policy.deadlines.remove(id);
        if let Some(timer) = self.retry_timers.remove(&format!("limit:{id}")) {
            timer.abort();
        }
        self.release_teams_run(&re);
        re.cancel.cancel();
        // A review run's detached worktree is reclaimed at its EXIT, and a termination is the one
        // way a review ends without reaching that exit: the entry is already gone, so the worker's
        // later exit event returns at the stale/absent guard, before `on_worker_exit`'s teardown
        // (STUDIO-716). Nothing downstream could name the tree afterwards — a `pr:` id reaches no
        // terminal tracker state, so `TerminateCleanup` never fires for it either — so an operator
        // Stop used to leak it permanently. Review runs only: a stopped or stalled TICKET keeps its
        // workspace, which `reconcile_stalled` retries straight back into.
        //
        // AFTER the cancellation, never before: the removal is `git worktree remove --force` with an
        // `rm -rf` fallback, so it must not be handed to the runtime while the agent's process group
        // is still alive inside the tree it deletes.
        if let Some(run) = re.review.as_ref() {
            self.teardown_review_worktree(run, &re.project_slug);
        }
        if let Some(run) = re.landing.as_ref() {
            self.teardown_landing_worktree(run);
        }
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

    // STUDIO-1043: a ticket that goes terminal while it is running has its retained opencode session
    // discarded when reconcile terminates and cleans it (review B1).
    #[tokio::test]
    async fn reconcile_terminal_discards_a_retained_opencode_session() {
        let mut f = Fake::new();
        f.states_by_ids_func = Some(states_ok(vec![issue("1", "MT-1", "Done")]));
        let tr = Arc::new(f);
        let (mut o, _dir) = orch_for_reconcile(Arc::clone(&tr), std::time::Duration::ZERO);
        let state_root = TempDir::new();
        o.eff.as_mut().expect("eff").cfg.opencode.state_root = state_root.path.clone();
        let (kept_dir, rec) =
            seed_opencode_session(std::path::Path::new(&state_root.path), "MT-1", 1_000_000);
        add_running(&mut o, "1", "MT-1", "In Progress", Utc::now());

        o.reconcile().await;

        assert!(!o.running.contains_key("1"), "terminal issue terminated");
        assert!(
            !kept_dir.exists(),
            "a terminal issue's retained opencode session directory must be discarded"
        );
        assert!(!rec.exists(), "and its record removed");
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

    /// STUDIO-801, and the premise the retry-side capacity count rests on. `reconcile_stalled` has
    /// no review guard — it terminates and retries ANY wedged worker — so a TICKETLESS review run
    /// really does land in `retry_attempts`, which is the door `on_review_exit` does not close
    /// (STUDIO-716). Pinned end to end rather than assumed, because assuming the opposite is what
    /// let the parked review be miscounted as implementation work: the reviewer's identity is
    /// carried onto the parked entry, and the router's snapshot must still not charge them an
    /// implementation slot for it (design §4.3, §4.4 fix 2 / D2).
    #[tokio::test]
    async fn a_wedged_ticketless_review_parks_without_spending_implementation_capacity() {
        let mut f = Fake::new();
        f.states_by_ids_func = Some(states_ok(vec![]));
        let tr = Arc::new(f);
        let (mut o, _dir) =
            orch_for_reconcile(Arc::clone(&tr), std::time::Duration::from_millis(100));

        let run = crate::review::ReviewRun {
            owner: "makewhatis".to_string(),
            repo: "rhapsody".to_string(),
            number: 128,
            reviewer: "alice".to_string(),
            ..crate::review::ReviewRun::default()
        };
        let key = run.key();
        let mut re = running_entry(run.synthetic_issue(), "", "");
        re.started_at = Utc::now() - chrono::Duration::hours(1); // long-stale ⇒ wedged
        re.identity = "alice".to_string();
        re.review = Some(run);
        o.running.insert(key.clone(), re);
        o.claimed.insert(key.clone());

        o.reconcile_stalled();

        let parked = o
            .retry_attempts
            .get(&key)
            .expect("a wedged review run IS parked in the retry queue");
        assert_eq!(
            parked.identity, "alice",
            "the parked review is still alice's work"
        );

        let load =
            crate::teams::LoadSnapshot::from_running_and_retries(&o.running, &o.retry_attempts);
        assert_eq!(
            load.impl_live("alice"),
            0,
            "D2: a stalled review must not spend alice's implementation slot"
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

    /// ⚠️ The stall timeout follows the harness a run was DISPATCHED on, not only the backend its
    /// config names. STUDIO-902's headline arrangement is `agent.backend: claude` with ONE
    /// teammate's profile naming `harness: opencode`; resolving per config alone governed that run
    /// by `claude.stall_timeout_ms` and left `opencode.stall_timeout_ms` consulted by nothing.
    ///
    /// Every number here is DIFFERENT so no arm can pass by coincidence, and the per-project arm is
    /// asserted beside the top-level one because the two resolve through different branches.
    #[test]
    fn stall_timeout_follows_the_dispatched_harness_not_only_the_config() {
        let (mut o, _dir) = liveness_orch(std::time::Duration::from_millis(111_000));
        {
            let eff = o.eff.as_mut().expect("eff");
            eff.cfg.agent.backend = "claude".to_string();
            eff.cfg.claude.stall_timeout_ms = 111_000;
            eff.cfg.opencode.stall_timeout_ms = 222_000;

            let mut p = empty_resolved_project("proj", Arc::new(Fake::new()));
            p.mcfg.agent.backend = "claude".to_string();
            p.mcfg.claude.stall_timeout_ms = 333_000;
            p.mcfg.opencode.stall_timeout_ms = 444_000;
            p.stall_timeout = std::time::Duration::from_millis(333_000);
            eff.projects.push(p);
        }

        let resolved = |o: &Orchestrator, slug: &str, harness: &str| {
            let mut re = running_entry(issue("1", "MT-1", "In Progress"), slug, slug);
            re.harness = harness.to_string();
            o.stall_timeout_for(&re).as_millis()
        };

        // Top level: no harness, and the configured one, both keep the config's own number.
        assert_eq!(resolved(&o, "", ""), 111_000);
        assert_eq!(resolved(&o, "", "claude"), 111_000);
        // The routed teammate: opencode's own knob, which nothing consulted before.
        assert_eq!(
            resolved(&o, "", "opencode"),
            222_000,
            "an opencode-dispatched run must be governed by opencode's stall timeout"
        );
        // A harness this build has no runner for never reaches a turn — `spawn_worker` REFUSES it
        // (STUDIO-978) — so it keeps the default's liveness window here rather than losing stall
        // detection to a typo.
        assert_eq!(resolved(&o, "", "goose"), 111_000);

        // And the per-project branch, which reads the project's own resolved config.
        assert_eq!(resolved(&o, "proj", ""), 333_000);
        assert_eq!(resolved(&o, "proj", "opencode"), 444_000);
    }

    fn liveness_orch(stall: std::time::Duration) -> (Orchestrator, TempDir) {
        let mut f = Fake::new();
        f.states_by_ids_func = Some(states_ok(vec![issue("1", "MT-1", "In Progress")]));
        orch_for_reconcile(Arc::new(f), stall)
    }

    #[tokio::test]
    async fn stall_detector_does_not_retry_a_run_with_a_terminal_result() {
        let _serial = TRACING_TEST_LOCK.lock().await;
        let (logs, subscriber) = recording_subscriber();
        let _guard = tracing::subscriber::set_default(subscriber);
        tracing::callsite::rebuild_interest_cache();
        for is_error in [false, true] {
            let (mut o, _dir) = liveness_orch(std::time::Duration::from_secs(300));
            o.eff.as_mut().unwrap().cpu_sampler = sampler(true, &[(42, &[0])]);
            let base = utc(2026, 5, 29, 12, 0, 0);
            let clock = install_clock(&mut o, base);
            add_running(&mut o, "1", "MT-1", "In Progress", base);
            let send = |o: &mut Orchestrator, mut ev: agent::Event| {
                ev.pid = 42;
                o.on_agent_update(AgentUpdate {
                    issue_id: "1".into(),
                    ev,
                });
            };
            send(
                &mut o,
                agent::claude::classify(br#"{"type":"system","subtype":"init","session_id":"s"}"#)
                    .event,
            );
            let line = format!(r#"{{"type":"result","subtype":"success","is_error":{is_error}}}"#);
            send(&mut o, agent::claude::classify(line.as_bytes()).event);
            // Late nonterminal chatter must not erase receipt of the result.
            send(
                &mut o,
                agent::Event {
                    event_type: agent::EVENT_NOTIFICATION.into(),
                    ..Default::default()
                },
            );
            advance(&clock, chrono::Duration::seconds(301));
            logs.lock().unwrap().clear();
            o.reconcile_stalled();
            assert!(
                o.running.contains_key("1"),
                "captured result was killed as wedged"
            );
            assert!(
                !o.retry_attempts.contains_key("1"),
                "captured result was retried"
            );
            assert!(
                !logs
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|e| e.message.contains("worker wedged"))
            );

            // A continuation on the same session is new work, not a sticky run exemption.
            send(
                &mut o,
                agent::claude::classify(br#"{"type":"system","subtype":"init","session_id":"s"}"#)
                    .event,
            );
            advance(&clock, chrono::Duration::seconds(301));
            o.reconcile_stalled();
            assert!(
                !o.running.contains_key("1"),
                "new resultless turn must still stall"
            );
            assert_eq!(o.retry_attempts["1"].err, "stalled");
        }
    }

    #[tokio::test]
    async fn terminal_result_stall_exemption_resets_on_pid_and_excludes_synthetic_failure() {
        for replace_pid in [false, true] {
            let (mut o, _dir) = liveness_orch(std::time::Duration::from_secs(300));
            o.eff.as_mut().unwrap().cpu_sampler = sampler(true, &[(42, &[0]), (99, &[0])]);
            let base = utc(2026, 5, 29, 12, 0, 0);
            let clock = install_clock(&mut o, base);
            add_running(&mut o, "1", "MT-1", "In Progress", base);
            if replace_pid {
                let mut result =
                    agent::claude::classify(br#"{"type":"result","subtype":"success"}"#).event;
                result.pid = 42;
                o.on_agent_update(AgentUpdate {
                    issue_id: "1".into(),
                    ev: result,
                });
            }
            o.on_agent_update(AgentUpdate {
                issue_id: "1".into(),
                ev: agent::Event {
                    event_type: if replace_pid {
                        agent::EVENT_NOTIFICATION
                    } else {
                        agent::EVENT_TURN_FAILED
                    }
                    .into(),
                    pid: 99,
                    ..Default::default()
                },
            });
            advance(&clock, chrono::Duration::seconds(301));
            o.reconcile_stalled();
            assert!(
                !o.running.contains_key("1"),
                "resultless work must retain ordinary stall rules"
            );
            assert_eq!(o.retry_attempts["1"].err, "stalled");
        }
    }

    fn sampler(ok: bool, seq: &[(i32, &[u64])]) -> std::sync::Arc<dyn Sampler> {
        let map: std::collections::HashMap<i32, Vec<u64>> =
            seq.iter().map(|(k, v)| (*k, v.to_vec())).collect();
        std::sync::Arc::new(FakeSampler::new(ok, map))
    }

    fn claude_liveness_case(body: &str) -> (Orchestrator, TempDir) {
        let (mut o, dir) = liveness_orch(std::time::Duration::from_millis(750));
        let script = dir.child("claude.sh");
        std::fs::write(&script, body).unwrap();
        let eff = o.eff.as_mut().unwrap();
        eff.cfg.claude.stall_timeout_ms = 750;
        eff.agent = Arc::new(agent::claude::Runner::new(agent::claude::Config {
            command: format!("bash {script}"),
            workspace_root: dir.path.clone(),
            turn_timeout: std::time::Duration::from_secs(20),
            ..Default::default()
        }));
        eff.max_turns = 1;
        eff.cpu_sampler = sampler(false, &[]); // macOS fallback, regardless of the test host
        eff.log_dir = dir.child("logs");
        eff.transcripts = Arc::new(crate::obslog::Store::new(&eff.log_dir));
        o.spawn = None;
        o.dispatch_issue(issue("1", "MT-1", "In Progress"), None, None, String::new());
        // A completely silent CLI has no init from which to learn its PID. Still exercise
        // the same unavailable-sampler grace as the partial-message case.
        o.running.get_mut("1").unwrap().pgid = 42;
        (o, dir)
    }

    const COMPLETE_CLAUDE_MESSAGES: &str = r#"
printf '%s\n' '{"type":"assistant","session_id":"s","message":{"content":[{"type":"text","text":"finished message"}],"usage":{"input_tokens":10,"output_tokens":20}}}'
printf '%s\n' '{"type":"result","subtype":"success","session_id":"s","result":"finished message","usage":{"input_tokens":10,"output_tokens":20}}'
"#;

    async fn drive_claude_liveness(o: &mut Orchestrator) {
        let mut rx = o.events_rx.lock().unwrap().take().unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            loop {
                while let Ok(event) = rx.try_recv() {
                    match event {
                        crate::control_loop::Event::AgentUpdate(update) => {
                            o.on_agent_update(update)
                        }
                        crate::control_loop::Event::WorkerExit(exit) => {
                            assert!(!exit.failed, "fake CLI failed: {}", exit.err_msg);
                        }
                        _ => {}
                    }
                }
                o.reconcile_stalled();
                if !o.running.contains_key("1") || o.running["1"].terminal_result_seen {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("fake Claude did not finish or wedge");
        tokio::time::timeout(std::time::Duration::from_secs(5), o.wg.wait())
            .await
            .expect("worker did not exit after completion/cancellation");
    }

    #[tokio::test]
    async fn long_generation_with_partial_events_is_not_wedged() {
        let body = format!(
            r#"
read -r prompt
printf '%s\n' '{{"type":"system","subtype":"init","session_id":"s","apiKeySource":"none"}}'
for i in {{1..40}}; do
  case " $* " in *' --include-partial-messages '*)
    printf '%s\n' '{{"type":"stream_event","session_id":"s","event":{{"type":"content_block_delta","delta":{{"type":"text_delta","text":"partial"}}}}}}'
  esac
  sleep 0.1
done
{COMPLETE_CLAUDE_MESSAGES}
"#
        );
        let (mut o, _dir) = claude_liveness_case(&body);
        let started = std::time::Instant::now();
        drive_claude_liveness(&mut o).await;
        assert!(o.running.contains_key("1"), "generation was wedge-killed");
        assert!(o.running["1"].terminal_result_seen);
        assert!(o.retry_attempts.is_empty());
        assert!(started.elapsed() > std::time::Duration::from_secs(3));
        assert_eq!(o.running["1"].total_tokens, 30);
    }

    #[tokio::test]
    async fn silent_cli_is_still_wedged() {
        let (mut o, _dir) = claude_liveness_case("read -r prompt\nexec sleep 30\n");
        drive_claude_liveness(&mut o).await;
        assert!(!o.running.contains_key("1"));
        assert_eq!(o.retry_attempts["1"].err, "stalled");
    }

    #[tokio::test]
    async fn partial_events_are_not_persisted() {
        let body = format!(
            r#"
read -r prompt
printf '%s\n' '{{"type":"system","subtype":"init","session_id":"s","apiKeySource":"none"}}'
for i in {{1..1000}}; do
  printf '%s\n' '{{"type":"stream_event","session_id":"s","event":{{"type":"message_delta","usage":{{"output_tokens":999}}}}}}'
done
{COMPLETE_CLAUDE_MESSAGES}
"#
        );
        let (mut o, _dir) = claude_liveness_case(&body);
        drive_claude_liveness(&mut o).await;
        let re = &o.running["1"];
        let types: Vec<_> = re.recent_events.iter().map(|e| e.event.as_str()).collect();
        assert_eq!(types, ["session_started", "notification", "turn_completed"]);
        assert_eq!(re.total_tokens, 30, "partials must not affect accounting");
        let raw = std::fs::read_to_string(o.eff.as_ref().unwrap().transcripts.latest_path("MT-1"))
            .unwrap();
        let types: Vec<_> = raw
            .lines()
            .map(|line| {
                serde_json::from_str::<serde_json::Value>(line).unwrap()["type"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(types, ["system", "assistant", "result"]);
    }

    // Exercise the production worker callback while control deliberately leaves its channel
    // unapplied. MUTATION: read re.last_event_at in event_liveness_at and the first verdict reds.
    #[tokio::test]
    async fn stall_check_uses_worker_emit_time_not_control_loop_apply_time() {
        let (mut o, _dir) = liveness_orch(std::time::Duration::from_secs(300));
        let mut ag = agent::fake::Fake::new();
        ag.turns = vec![agent::fake::TurnScript {
            events: vec![agent::Event {
                event_type: agent::EVENT_NOTIFICATION.into(),
                message: "queued work".into(),
                ..Default::default()
            }],
            result: agent::TurnResult {
                status: agent::TURN_SUCCEEDED.into(),
                ..Default::default()
            },
            ..Default::default()
        }];
        let eff = o.eff.as_mut().unwrap();
        eff.agent = Arc::new(ag);
        eff.max_turns = 1;
        eff.cpu_sampler = sampler(true, &[(42, &[100])]);
        o.spawn = None; // real worker wiring, with an in-process harness
        let base = Utc::now() - chrono::Duration::hours(1);
        let clock = install_clock(&mut o, base);
        o.dispatch_issue(issue("1", "MT-1", "In Progress"), None, None, String::new());
        o.running.get_mut("1").unwrap().pgid = 42;
        o.reconcile_stalled(); // establish idle CPU baseline
        tokio::time::timeout(std::time::Duration::from_secs(10), o.wg.wait())
            .await
            .expect("worker did not finish");
        let emitted_by = Utc::now();
        assert_eq!(o.running["1"].last_event_at, zero_time());
        let mut rx = o.events_rx.lock().unwrap().take().unwrap();
        assert!(!rx.is_empty());

        *clock.lock().unwrap() = emitted_by + chrono::Duration::seconds(2);
        o.reconcile_stalled();
        assert!(
            o.running.contains_key("1"),
            "a queued event emitted two seconds ago must prevent a wedge verdict"
        );
        assert!(!o.retry_attempts.contains_key("1"));

        // Applying the backlog later must not buy another full silence window.
        *clock.lock().unwrap() = emitted_by + chrono::Duration::seconds(100);
        let mut applied = 0;
        while let Ok(event) = rx.try_recv() {
            if let crate::control_loop::Event::AgentUpdate(update) = event {
                o.on_agent_update(update);
                applied += 1;
            }
        }
        assert_eq!(applied, 1, "the worker emitted the expected notification");
        assert_eq!(o.running["1"].last_message, "queued work");
        *clock.lock().unwrap() = emitted_by + chrono::Duration::seconds(301);
        o.reconcile_stalled();
        assert!(
            !o.running.contains_key("1"),
            "applying an old event must not renew worker liveness"
        );
        assert_eq!(o.retry_attempts["1"].err, "stalled");
    }

    #[tokio::test]
    async fn sampler_unavailable_widens_the_stall_window() {
        let (mut o, _dir) = liveness_orch(std::time::Duration::from_secs(300));
        o.eff.as_mut().unwrap().cpu_sampler = sampler(false, &[]);
        let base = utc(2026, 5, 29, 12, 0, 0);
        let clock = install_clock(&mut o, base);
        add_running(&mut o, "1", "MT-1", "In Progress", base);
        o.running.get_mut("1").unwrap().pgid = 42;
        o.reconcile_stalled(); // first failure starts bounded grace
        advance(&clock, chrono::Duration::seconds(601));
        o.on_agent_update(AgentUpdate {
            issue_id: "1".into(),
            ev: agent::Event {
                event_type: agent::EVENT_NOTIFICATION.into(),
                ..Default::default()
            },
        });
        advance(&clock, chrono::Duration::seconds(301));
        o.reconcile_stalled();
        assert!(
            o.running.contains_key("1"),
            "after initial grace expires, an event still gets twice the stall window"
        );
        advance(&clock, chrono::Duration::seconds(299));
        o.reconcile_stalled();
        assert!(o.running.contains_key("1"), "strict 2x boundary");
        advance(&clock, chrono::Duration::seconds(1));
        o.reconcile_stalled();
        assert!(!o.running.contains_key("1"));
        assert_eq!(o.retry_attempts["1"].err, "stalled");
    }

    #[tokio::test]
    async fn stall_check_rechecks_emission_before_terminating_candidates() {
        // Sampling the second group can block long enough for the first worker to emit. Each
        // sample stamps the OTHER run, so this pins the race independent of HashMap iteration.
        struct EmittingSampler {
            times: Vec<(i32, crate::liveness::WorkerEmitTime)>,
            now: DateTime<Utc>,
        }
        impl Sampler for EmittingSampler {
            fn group_cpu(&self, pgid: i32) -> Option<u64> {
                for (id, time) in &self.times {
                    if *id != pgid {
                        time.record(self.now);
                    }
                }
                Some(0)
            }
            fn ticks_per_second(&self) -> Option<u64> {
                Some(100)
            }
        }
        let (mut o, _dir) = liveness_orch(std::time::Duration::from_secs(300));
        let now = utc(2026, 5, 29, 12, 0, 0);
        install_clock(&mut o, now);
        let mut times = Vec::new();
        for (id, pgid) in [("1", 42), ("2", 43)] {
            add_running(
                &mut o,
                id,
                &format!("MT-{id}"),
                "In Progress",
                now - chrono::Duration::hours(1),
            );
            let re = o.running.get_mut(id).unwrap();
            re.pgid = pgid;
            re.worker_emit_time.record(re.started_at);
            times.push((pgid, re.worker_emit_time.clone()));
        }
        o.eff.as_mut().unwrap().cpu_sampler = Arc::new(EmittingSampler { times, now });
        o.reconcile_stalled();
        assert_eq!(
            o.running.len(),
            2,
            "both workers emitted before termination"
        );
        assert!(o.retry_attempts.is_empty());
    }

    // Both real harness shapes, with /proc explicitly unavailable. The tool has no
    // event or CPU heartbeat while waiting; its matching result ends the exemption.
    async fn portable_open_tool_case(harness: &str) {
        let claude_start = br#"{"type":"assistant","session_id":"ses_x","message":{"content":[{"type":"tool_use","id":"call_x","name":"Bash","input":{"command":"gh pr checks --watch"}}]}}"#;
        let claude_end = br#"{"type":"user","session_id":"ses_x","message":{"content":[{"type":"tool_result","tool_use_id":"call_x","content":"green"}]}}"#;
        let opencode_start = br#"{"type":"tool_use","sessionID":"ses_x","part":{"id":"prt_x","callID":"call_x","type":"tool","state":{"status":"running"}}}"#;
        let opencode_end = br#"{"type":"tool_use","sessionID":"ses_x","part":{"id":"prt_x","callID":"call_x","type":"tool","state":{"status":"completed"}}}"#;
        for (name, start, end) in [
            (
                "claude",
                agent::claude::classify(claude_start).event,
                agent::claude::classify(claude_end).event,
            ),
            (
                "opencode",
                agent::opencode::classify(opencode_start).event,
                agent::opencode::classify(opencode_end).event,
            ),
        ] {
            if name != harness {
                continue;
            }
            let (mut o, _dir) = liveness_orch(std::time::Duration::from_secs(300));
            o.eff.as_mut().unwrap().cpu_sampler = sampler(false, &[]);
            let base = utc(2026, 5, 29, 12, 0, 0);
            let clock = install_clock(&mut o, base);
            add_running(&mut o, "1", "MT-1", "In Progress", base);
            o.on_agent_update(AgentUpdate {
                issue_id: "1".into(),
                ev: agent::Event {
                    event_type: agent::EVENT_SESSION_STARTED.into(),
                    pid: 42,
                    message: "ses_x".into(),
                    ..Default::default()
                },
            });
            o.on_agent_update(AgentUpdate {
                issue_id: "1".into(),
                ev: start,
            });
            o.reconcile_stalled();
            // No optional tool timeout configured: only the runner's turn deadline bounds it.
            advance(&clock, chrono::Duration::hours(2));
            o.reconcile_stalled();
            assert!(
                o.running.contains_key("1"),
                "{name}: open tool must survive silence without /proc"
            );
            o.on_agent_update(AgentUpdate {
                issue_id: "1".into(),
                ev: end,
            });
            advance(&clock, chrono::Duration::seconds(601));
            o.reconcile_stalled();
            assert!(
                !o.running.contains_key("1"),
                "{name}: closed tool must not mask a dead stream"
            );
            assert_eq!(o.retry_attempts["1"].err, "stalled");
        }
    }

    #[tokio::test]
    async fn portable_claude_open_tool_survives_then_closed_tool_stalls() {
        portable_open_tool_case("claude").await;
    }

    #[tokio::test]
    async fn portable_opencode_open_tool_survives_then_closed_tool_stalls() {
        portable_open_tool_case("opencode").await;
    }

    #[tokio::test]
    async fn reconcile_liveness_honors_configured_cpu_threshold() {
        // 10% CPU survives the default 5%, but not a configured 20% or the exact 10% boundary.
        for (threshold, survives) in [(0.05, true), (0.2, false), (0.1, false)] {
            let (mut o, _dir) = liveness_orch(std::time::Duration::from_secs(2));
            let eff = o.eff.as_mut().unwrap();
            eff.cfg.agent.stall_cpu_threshold = threshold;
            eff.cpu_sampler = sampler(true, &[(42, &[100, 130])]);
            let base = utc(2026, 5, 29, 12, 0, 0);
            let clock = install_clock(&mut o, base);
            add_running(&mut o, "1", "MT-1", "In Progress", base);
            o.running.get_mut("1").unwrap().pgid = 42;
            o.reconcile_stalled();
            advance(&clock, chrono::Duration::seconds(3));
            o.reconcile_stalled();
            assert_eq!(
                o.running.contains_key("1"),
                survives,
                "threshold {threshold}"
            );
        }
    }

    #[test]
    fn cpu_samples_accumulate_for_a_minimum_interval_and_reset_after_failure() {
        let base = utc(2026, 5, 29, 12, 0, 0);
        let mut re = running_entry(issue("1", "MT-1", "In Progress"), "", "");
        re.pgid = 42;
        let cpu = sampler(true, &[(42, &[100, 101, 106, 500, 506])]);
        sample_cpu(&mut re, base, Some(cpu.as_ref()), 0.05);
        assert_eq!(re.last_cpu_active_at, zero_time(), "baseline is not work");
        sample_cpu(
            &mut re,
            base + chrono::Duration::milliseconds(10),
            Some(cpu.as_ref()),
            0.05,
        );
        assert_eq!(
            re.last_cpu_active_at,
            zero_time(),
            "a short burst grants no liveness"
        );
        let one_second = base + chrono::Duration::seconds(1);
        sample_cpu(&mut re, one_second, Some(cpu.as_ref()), 0.05);
        assert_eq!(
            re.last_cpu_active_at, one_second,
            "6 ticks over accumulated 1s is 6%"
        );
        let failed = sampler(false, &[]);
        let (_, logs) = capture_events(|| {
            sample_cpu(
                &mut re,
                base + chrono::Duration::seconds(2),
                Some(failed.as_ref()),
                0.05,
            );
            sample_cpu(
                &mut re,
                base + chrono::Duration::seconds(3),
                Some(failed.as_ref()),
                0.05,
            );
        });
        assert_eq!(
            logs.iter()
                .filter(|e| e.message.contains("CPU sampler unavailable"))
                .count(),
            1
        );
        assert_eq!(
            re.cpu_failure_since,
            Some(base + chrono::Duration::seconds(2))
        );
        sample_cpu(
            &mut re,
            base + chrono::Duration::seconds(4),
            Some(cpu.as_ref()),
            0.05,
        );
        assert_eq!(
            re.cpu_failure_since,
            Some(base + chrono::Duration::seconds(2)),
            "baseline-only recovery preserves the existing failure grace"
        );
        assert_eq!(
            re.last_cpu_active_at, one_second,
            "recovery must not credit unmeasured CPU"
        );
        sample_cpu(
            &mut re,
            base + chrono::Duration::seconds(5),
            Some(cpu.as_ref()),
            0.05,
        );
        assert_eq!(re.last_cpu_active_at, base + chrono::Duration::seconds(5));
        assert_eq!(re.cpu_failure_since, None);
    }

    // Lifecycle ownership is per turn, not one sticky boolean per run.
    #[tokio::test]
    async fn portable_tool_lifecycle_handles_parallel_duplicates_errors_and_turn_resets() {
        let (mut o, _dir) = liveness_orch(std::time::Duration::from_secs(300));
        o.eff.as_mut().unwrap().cpu_sampler = sampler(false, &[]);
        let base = utc(2026, 5, 29, 12, 0, 0);
        let clock = install_clock(&mut o, base);
        add_running(&mut o, "1", "MT-1", "In Progress", base);
        let send = |o: &mut Orchestrator, ev: agent::Event| {
            o.on_agent_update(AgentUpdate {
                issue_id: "1".into(),
                ev,
            })
        };
        let init = || agent::Event {
            event_type: agent::EVENT_SESSION_STARTED.into(),
            message: "ses_x".into(),
            pid: 42,
            ..Default::default()
        };
        send(&mut o, init());
        let call = |id: &str, open| agent::Event {
            event_type: agent::EVENT_NOTIFICATION.into(),
            tool_calls: vec![agent::ToolCall {
                session_id: "ses_x".into(),
                id: id.into(),
                open,
            }],
            ..Default::default()
        };
        send(&mut o, call("a", true));
        send(&mut o, call("a", true));
        send(&mut o, call("b", true));
        assert_eq!(o.running["1"].open_tool_calls.len(), 2);
        send(&mut o, call("unknown", false));
        send(&mut o, call("a", false));
        advance(&clock, chrono::Duration::hours(2));
        o.reconcile_stalled();
        assert!(
            o.running.contains_key("1"),
            "one of two tools still running"
        );
        let error = agent::opencode::classify(br#"{"type":"tool_use","sessionID":"ses_x","part":{"id":"b","state":{"status":"error"}}}"#).event;
        send(&mut o, error);
        assert!(o.running["1"].open_tool_calls.is_empty());
        send(&mut o, call("a", true));
        send(
            &mut o,
            agent::Event {
                event_type: agent::EVENT_TURN_FAILED.into(),
                ..Default::default()
            },
        );
        send(&mut o, call("late", true));
        assert!(
            o.running["1"].open_tool_calls.is_empty(),
            "terminal failure must reject late updates"
        );
        send(&mut o, init());
        send(&mut o, call("a", true));
        send(&mut o, init());
        assert!(
            o.running["1"].open_tool_calls.is_empty(),
            "continuation is a new tool generation"
        );
        let mut foreign = call("foreign", true);
        foreign.tool_calls[0].session_id = "another-session".into();
        send(&mut o, foreign);
        assert!(o.running["1"].open_tool_calls.is_empty());
        send(&mut o, call("a", true));
        send(
            &mut o,
            agent::Event {
                event_type: agent::EVENT_NOTIFICATION.into(),
                pid: 99,
                ..Default::default()
            },
        );
        assert!(
            o.running["1"].open_tool_calls.is_empty(),
            "PID replacement clears tool state"
        );
        advance(&clock, chrono::Duration::seconds(301));
        o.reconcile_stalled();
        advance(&clock, chrono::Duration::seconds(601));
        o.reconcile_stalled();
        assert!(!o.running.contains_key("1"));
    }

    // STUDIO-1177: idle event-loop ticks without a tool are not evidence of useful work.
    #[tokio::test]
    async fn reconcile_liveness_tiny_cpu_deltas_stall() {
        let (mut o, _dir) = liveness_orch(std::time::Duration::from_secs(300));
        o.eff.as_mut().unwrap().cpu_sampler =
            sampler(true, &[(42, &[100, 125, 150, 175, 200, 225, 250, 275])]);
        let base = utc(2026, 5, 29, 12, 0, 0);
        let clock = install_clock(&mut o, base);
        add_running(&mut o, "1", "MT-1", "In Progress", base);
        o.running.get_mut("1").unwrap().pgid = 42;
        o.reconcile_stalled();
        for _ in 0..6 {
            advance(&clock, chrono::Duration::seconds(50)); // 0.5% of one 100-Hz core
            o.reconcile_stalled();
            assert!(
                o.running.contains_key("1"),
                "strict stall boundary is preserved"
            );
        }
        advance(&clock, chrono::Duration::seconds(50));
        o.reconcile_stalled();
        assert!(
            !o.running.contains_key("1"),
            "idle ticking must not keep a silent run alive"
        );
        assert_eq!(o.retry_attempts["1"].err, "stalled");
        assert_eq!(o.retry_attempts["1"].attempt, 1);
    }

    #[tokio::test]
    async fn reconcile_liveness_persistent_sampler_failure_stalls() {
        let (mut o, _dir) = liveness_orch(std::time::Duration::from_secs(300));
        o.eff.as_mut().unwrap().cpu_sampler = sampler(false, &[]);
        let base = utc(2026, 5, 29, 12, 0, 0);
        let clock = install_clock(&mut o, base);
        add_running(
            &mut o,
            "1",
            "MT-1",
            "In Progress",
            base - chrono::Duration::hours(1),
        );
        o.running.get_mut("1").unwrap().pgid = 42;
        o.reconcile_stalled();
        assert!(
            o.running.contains_key("1"),
            "first failure grants two bounded stall windows"
        );
        advance(&clock, chrono::Duration::seconds(600));
        o.reconcile_stalled();
        assert!(o.running.contains_key("1"));
        advance(&clock, chrono::Duration::seconds(1));
        o.reconcile_stalled();
        assert!(
            !o.running.contains_key("1"),
            "unreadable CPU cannot grant perpetual liveness"
        );
        assert_eq!(o.retry_attempts["1"].err, "stalled");
    }

    #[tokio::test]
    async fn recovery_baseline_keeps_failure_grace_until_cpu_can_be_measured() {
        let (mut o, _dir) = liveness_orch(std::time::Duration::from_secs(300));
        o.eff.as_mut().unwrap().cpu_sampler = sampler(false, &[]);
        let base = utc(2026, 5, 29, 12, 0, 0);
        let clock = install_clock(&mut o, base);
        add_running(
            &mut o,
            "1",
            "MT-1",
            "In Progress",
            base - chrono::Duration::hours(1),
        );
        o.running.get_mut("1").unwrap().pgid = 42;
        o.reconcile_stalled(); // failed sample starts the bounded grace
        assert!(o.running.contains_key("1"));

        o.eff.as_mut().unwrap().cpu_sampler =
            sampler(true, &[(42, &[100, 150, 200, 30_200, 60_200])]);
        advance(&clock, chrono::Duration::seconds(1));
        o.reconcile_stalled(); // recovery baseline cannot measure CPU activity yet
        assert!(
            o.running.contains_key("1"),
            "a recovery baseline must not discard the still-active failure grace"
        );
        advance(&clock, chrono::Duration::milliseconds(500));
        o.reconcile_stalled(); // still accumulating the minimum measurement interval
        assert!(
            o.running.contains_key("1"),
            "subsecond recovery samples must preserve the failure grace"
        );
        advance(&clock, chrono::Duration::milliseconds(500));
        o.reconcile_stalled(); // now measures 100% of a core
        assert!(o.running.contains_key("1"));
        for _ in 0..2 {
            advance(&clock, chrono::Duration::seconds(300));
            o.reconcile_stalled();
            assert!(
                o.running.contains_key("1"),
                "measured CPU work must survive beyond the original failure grace"
            );
        }
    }

    #[tokio::test]
    async fn recovery_baselines_cannot_renew_failure_grace() {
        let (mut o, _dir) = liveness_orch(std::time::Duration::from_secs(300));
        let base = utc(2026, 5, 29, 12, 0, 0);
        let clock = install_clock(&mut o, base);
        add_running(
            &mut o,
            "1",
            "MT-1",
            "In Progress",
            base - chrono::Duration::hours(1),
        );
        o.running.get_mut("1").unwrap().pgid = 42;
        // Alternate failures and baseline-only samples, never measuring any work.
        for ok in [false, true, false, true] {
            o.eff.as_mut().unwrap().cpu_sampler = sampler(ok, &[(42, &[100])]);
            o.reconcile_stalled();
            assert!(
                o.running.contains_key("1"),
                "recovery must retain the original grace window"
            );
            advance(&clock, chrono::Duration::seconds(200));
        }
        o.eff.as_mut().unwrap().cpu_sampler = sampler(false, &[]);
        o.reconcile_stalled();
        assert!(
            !o.running.contains_key("1"),
            "baseline-only recovery must not restart the failure grace"
        );
        assert_eq!(o.retry_attempts["1"].err, "stalled");
    }

    #[tokio::test]
    async fn recovery_with_idle_cpu_does_not_credit_liveness() {
        let (mut o, _dir) = liveness_orch(std::time::Duration::from_secs(300));
        o.eff.as_mut().unwrap().cpu_sampler = sampler(false, &[]);
        let base = utc(2026, 5, 29, 12, 0, 0);
        let clock = install_clock(&mut o, base);
        add_running(
            &mut o,
            "1",
            "MT-1",
            "In Progress",
            base - chrono::Duration::hours(1),
        );
        o.running.get_mut("1").unwrap().pgid = 42;
        o.reconcile_stalled();
        o.eff.as_mut().unwrap().cpu_sampler = sampler(true, &[(42, &[100, 101])]);
        advance(&clock, chrono::Duration::seconds(1));
        o.reconcile_stalled();
        assert!(o.running.contains_key("1"), "baseline keeps the grace");
        advance(&clock, chrono::Duration::seconds(2));
        o.reconcile_stalled(); // measured 0.5% CPU is idle, despite successful sampling
        assert!(
            !o.running.contains_key("1"),
            "a measured idle recovery must not count as CPU liveness"
        );
        assert_eq!(o.retry_attempts["1"].err, "stalled");
    }

    #[tokio::test]
    async fn reconcile_liveness_new_event_resets_idle_clock() {
        let (mut o, _dir) = liveness_orch(std::time::Duration::from_secs(300));
        o.eff.as_mut().unwrap().cpu_sampler = sampler(true, &[(42, &[100])]);
        let base = utc(2026, 5, 29, 12, 0, 0);
        let clock = install_clock(&mut o, base);
        add_running(&mut o, "1", "MT-1", "In Progress", base);
        o.running.get_mut("1").unwrap().pgid = 42;
        o.reconcile_stalled();
        advance(&clock, chrono::Duration::seconds(299));
        o.on_agent_update(AgentUpdate {
            issue_id: "1".into(),
            ev: agent::Event {
                event_type: agent::EVENT_SESSION_STARTED.into(),
                ..Default::default()
            },
        });
        advance(&clock, chrono::Duration::seconds(300));
        o.reconcile_stalled();
        assert!(
            o.running.contains_key("1"),
            "event gets the full stall window"
        );
        advance(&clock, chrono::Duration::seconds(1));
        o.reconcile_stalled();
        assert!(!o.running.contains_key("1"));
    }

    // Go's active-work case, now requiring a measured interval rather than a baseline sample.
    #[tokio::test]
    async fn reconcile_liveness_cpu_active_survives() {
        let (mut o, _dir) = liveness_orch(std::time::Duration::from_millis(100));
        o.eff.as_mut().unwrap().cpu_sampler = sampler(true, &[(42, &[100, 200, 300, 400, 500])]);
        let base = utc(2026, 5, 29, 12, 0, 0);
        let clock = install_clock(&mut o, base);
        add_running(&mut o, "1", "MT-1", "In Progress", base);
        o.running.get_mut("1").unwrap().pgid = 42;
        o.running.get_mut("1").unwrap().last_event_at = base; // event stream goes silent
        o.reconcile_stalled(); // baseline, no liveness credit
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

    // STUDIO-1177: child exits may decrease the total, but are not positive CPU work.
    #[tokio::test]
    async fn reconcile_liveness_decreasing_sum_stalls() {
        let (mut o, _dir) = liveness_orch(std::time::Duration::from_millis(100));
        o.eff.as_mut().unwrap().cpu_sampler = sampler(true, &[(42, &[500, 300, 200, 100])]);
        let base = utc(2026, 5, 29, 12, 0, 0);
        let clock = install_clock(&mut o, base);
        add_running(&mut o, "1", "MT-1", "In Progress", base);
        o.running.get_mut("1").unwrap().pgid = 42;
        o.running.get_mut("1").unwrap().last_event_at = base;
        o.reconcile_stalled();
        advance(&clock, chrono::Duration::seconds(1));
        o.reconcile_stalled();
        assert!(
            !o.running.contains_key("1"),
            "a falling total must not renew liveness"
        );
    }

    // Go's first-failure grace is retained, but no longer renewable indefinitely.
    #[tokio::test]
    async fn reconcile_liveness_first_sampler_failure_grants_grace() {
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
        o.reconcile_stalled(); // fresh baseline for group 20
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
