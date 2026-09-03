//! promote — parity port of Go `internal/orchestrator/promote.go` (the DAG auto-promote pass,
//! INF-318).
//!
//! For every ENABLED-mode (graphite/dag) project, [`Orchestrator::promote_unblocked`] flips Backlog
//! dependents whose blockers are all cleared Backlog→Todo (stashing a graphite stacking hint), so the
//! NEXT tick's standard select dispatches them through the normal slot/label/state accounting. It runs
//! ON the control task (from `on_tick`, O7), so every tracker write and every state-map mutation is
//! loop-confined.
//!
//! Promote-then-let-select-dispatch (rather than dispatching inline) is deliberate: it reuses the
//! existing concurrency caps + the label gate instead of re-implementing them, so auto-promote can
//! never over-dispatch past `max_concurrent_agents` or run a ticket that proactive dispatch would have
//! label-filtered.
//!
//! Disabled-is-noop (load-bearing): a project whose resolved `dependency_mode` is disabled (the
//! default) is skipped BEFORE any fetch — it issues ZERO tracker calls and ZERO moves, so a daemon
//! with all-default projects runs this pass as a pure no-op and is observably unchanged on upgrade.
//!
//! Deviations from the Go source, all behavior-preserving:
//!   * Go iterates `&o.eff.projects[i]` while calling `o.promoteUnblockedScope(...)` on the same
//!     receiver; Rust's borrow checker forbids holding that borrow across the `&mut self` scope call,
//!     so the per-project scope config (tracker + sets + slug) is snapshotted into an owned
//!     [`PromoteScope`] BEFORE the mutating pass. A disabled / disabled-mode project is filtered out of
//!     that snapshot, so it still issues zero tracker calls (the disabled-is-noop invariant holds).
//!   * The best-effort diagnostics log via `tracing` (as the sibling crates do) instead of a threaded
//!     `slog` logger.

use std::collections::HashSet;
use std::sync::Arc;

use rhapsody_config::{DEPENDENCY_MODE_GRAPHITE, WORKSPACE_MODE_CLONE};
use rhapsody_core::{BlockerRef, Issue, normalize_state};
use rhapsody_tracker::Tracker;

use crate::dispatch::{
    blocker_cleared, blocker_identifier, dependency_mode_enabled, has_any_label,
};
use crate::orchestrator::{Orchestrator, StackHint};

/// One project's owned auto-promote scope, snapshotted from a [`ResolvedProject`](crate::effective::ResolvedProject)
/// (or the top-level [`Effective`](crate::effective::Effective)) so the mutating pass borrows no config.
/// `slug` scopes the durable never-run history read (matches the persisted project slug); the sets are
/// that scope's blocker-clearing config.
struct PromoteScope {
    tracker: Arc<dyn Tracker>,
    mode: String,
    review: HashSet<String>,
    terminal: HashSet<String>,
    canceled: HashSet<String>,
    labels: HashSet<String>,
    slug: String,
}

impl Orchestrator {
    /// The DAG auto-promote control-loop pass (INF-318). For every enabled-mode project it flips
    /// Backlog dependents whose blockers are all cleared Backlog→Todo (stashing a graphite stacking
    /// hint), so the next tick's standard select dispatches them. Disabled / disabled-mode projects
    /// issue ZERO tracker calls (the disabled-is-noop invariant). Mirrors Go `promoteUnblocked`.
    ///
    /// `pub` — an `on_tick` control-loop entry point (O7), like [`on_worker_exit`](Orchestrator::on_worker_exit).
    pub async fn promote_unblocked(&mut self) {
        for scope in self.promote_scopes() {
            self.promote_unblocked_scope(scope).await;
        }
    }

    /// Snapshots the enabled-mode scopes to promote this tick. Multi-project path: one scope per
    /// non-disabled, enabled-mode project. Legacy single-tracker path (no projects): one scope from the
    /// top-level effective config iff its mode is enabled; otherwise none. A disabled project
    /// (`enabled:false`, INF-224) or a disabled-mode project is filtered out BEFORE any fetch.
    fn promote_scopes(&self) -> Vec<PromoteScope> {
        let Some(eff) = self.eff.as_ref() else {
            return Vec::new();
        };
        if !eff.projects.is_empty() {
            return eff
                .projects
                .iter()
                .filter(|p| !p.disabled && dependency_mode_enabled(&p.dependency_mode))
                .map(|p| PromoteScope {
                    tracker: Arc::clone(&p.tracker),
                    mode: p.dependency_mode.clone(),
                    review: p.review_states.clone(),
                    terminal: p.terminal_states.clone(),
                    canceled: p.canceled_states.clone(),
                    labels: p.labels.clone(),
                    slug: p.slug.clone(),
                })
                .collect();
        }
        // Legacy / test-injected single-tracker path (no projects).
        if !dependency_mode_enabled(&eff.dependency_mode) {
            return Vec::new();
        }
        vec![PromoteScope {
            tracker: Arc::clone(&eff.tracker),
            mode: eff.dependency_mode.clone(),
            review: eff.review_states.clone(),
            terminal: eff.terminal_states.clone(),
            canceled: eff.canceled_states.clone(),
            labels: eff.labels.clone(),
            slug: String::new(),
        }]
    }

    /// Runs the auto-promote pass for one project scope. Mirrors Go `promoteUnblockedScope`.
    async fn promote_unblocked_scope(&mut self, scope: PromoteScope) {
        let backlog = match scope.tracker.fetch_blocked_backlog_issues().await {
            Ok(b) => b,
            Err(e) => {
                tracing::error!(project_slug = %scope.slug, err = %e, "auto-promote: backlog fetch failed; skipping project this tick");
                return;
            }
        };
        for iss in backlog {
            // SAFETY: only edge-bearing tickets are ever auto-promoted (a standalone Backlog ticket is
            // never touched).
            if iss.blocked_by.iter().flatten().next().is_none() {
                continue;
            }
            // Cheap within-tick idempotency (short-circuits before the store read).
            if self.running.contains_key(&iss.id) || self.claimed.contains(&iss.id) {
                continue;
            }
            // Label gate: a project with required labels only proactively works tickets carrying one.
            // Apply it here too, so auto-promote never moves a label-less dependent to Todo where the
            // standard select's eligibility would then reject it — stranding it in Todo (INF-318).
            if !scope.labels.is_empty() && !has_any_label(&iss, &scope.labels) {
                continue;
            }
            // Never-run guard (durable): a ticket that has EVER run — including one Stopped or
            // human-parked back into Backlog — is NEVER re-promoted, so Stop/park stays authoritative
            // under an active mode, across restarts (INF-318).
            if self.has_prior_run(&iss.identifier, &scope.slug) {
                continue;
            }
            let mut all_cleared = true;
            let mut cancelled_blocker = false;
            for b in iss.blocked_by.iter().flatten() {
                if is_canceled_blocker(b, &scope.canceled) {
                    cancelled_blocker = true;
                }
                if !blocker_cleared(
                    b,
                    &scope.mode,
                    &scope.review,
                    &scope.terminal,
                    &scope.canceled,
                ) {
                    all_cleared = false;
                }
            }
            if cancelled_blocker {
                // A cancelled blocker means the premise is gone: surface the dependent as orphaned and
                // do NOT promote it (a human decides what to do). graphite/dag only.
                tracing::info!(issue_identifier = %iss.identifier, "auto-promote: dependent orphaned (a blocker was cancelled); not promoting");
                continue;
            }
            if !all_cleared {
                continue; // still waiting on at least one blocker
            }
            self.promote_unblocked_move(iss, &scope.tracker, &scope.mode)
                .await;
        }
    }

    /// Reports whether the store holds at least one prior run row for `(identifier, project)`. The
    /// durable, loop-side never-run guard. On a store read error it returns `true` (conservative: do
    /// not re-promote a ticket we cannot verify); a Noop store returns no rows, so a never-run ticket
    /// is promotable (INF-318). Mirrors Go `hasPriorRun`.
    fn has_prior_run(&self, identifier: &str, project: &str) -> bool {
        match self.store().issue_history(identifier, project, 1) {
            Ok(runs) => !runs.is_empty(),
            Err(_) => true,
        }
    }

    /// Performs the Backlog→Todo Linear write and, in graphite mode, stashes the predecessor stacking
    /// hint for the next-tick dispatch (dag stashes nothing — fresh-from-main). It does NOT dispatch:
    /// the promoted Todo ticket is picked up by the next tick's standard select. On a move error it
    /// logs and SKIPS (the ticket stays in Backlog, retried next tick). Mirrors Go
    /// `promoteUnblockedMove`.
    async fn promote_unblocked_move(&mut self, iss: Issue, tracker: &Arc<dyn Tracker>, mode: &str) {
        if let Err(e) = tracker
            .move_issue_to_type(&iss.id, &iss.team_id, "unstarted")
            .await
        {
            tracing::error!(issue_id = %iss.id, issue_identifier = %iss.identifier, err = %e, "auto-promote: Backlog→Todo move failed; leaving in Backlog");
            return;
        }
        if let Some(h) = self.stack_context_for(&iss, tracker, mode).await {
            // Consumed (and rendered with the dispatch-time mode) by dispatch_issue.
            self.pending_stack.insert(iss.id.clone(), h);
        }
        tracing::info!(issue_id = %iss.id, issue_identifier = %iss.identifier, mode = %mode, "auto-promote: dependent unblocked; promoted Backlog→Todo (dispatch on next tick via select)");
    }

    /// Derives the graphite-mode predecessor stacking fact for a promoted dependent. dag/disabled modes
    /// return `None` (fresh-from-main, no injection). The hint is advisory, so a lookup failure or
    /// missing branch degrades to `None` rather than blocking the run. By construction a graphite
    /// dependent has ONE blocker; a misconfigured >1 logs a warning and stacks on the first blocker
    /// edge. Mode-agnostic by design — the recipe is rendered at dispatch (INF-418). Mirrors Go
    /// `stackContextFor`.
    async fn stack_context_for(
        &self,
        iss: &Issue,
        tracker: &Arc<dyn Tracker>,
        mode: &str,
    ) -> Option<StackHint> {
        if mode != DEPENDENCY_MODE_GRAPHITE {
            return None;
        }
        let blockers = iss.blocked_by.as_deref().unwrap_or_default();
        let pred = blockers.first()?;
        if blockers.len() > 1 {
            tracing::warn!(issue_identifier = %iss.identifier, blocker_count = blockers.len(), "auto-promote: graphite dependent has >1 blocker; stacking on the first by declaration");
        }
        let pred_id = pred.id.as_deref().filter(|s| !s.is_empty())?;
        let (branch, pr_number) = match tracker.fetch_issue_branch_by_id(pred_id).await {
            Ok(bp) => bp,
            Err(e) => {
                tracing::warn!(issue_identifier = %iss.identifier, predecessor = %blocker_identifier(pred), err = %e, "auto-promote: predecessor branch lookup failed; dispatching without a stacking hint");
                return None;
            }
        };
        if branch.is_empty() {
            return None;
        }
        Some(StackHint { branch, pr_number })
    }
}

/// Builds the first-turn STACK ON hint for a graphite dependent. The recipe is WORKSPACE_MODE-AWARE
/// because the locked-parent dance exists ONLY to work around the worktree checkout lock. Worktree
/// mode renders the full 5-step locked-parent recipe verbatim (branch off the predecessor's remote
/// tip), because the lock is real; clone mode has NO sibling lock (each dispatch is an independent
/// clone), so it checks the predecessor out directly (`gt get <PR#>` hydrates the whole stack when a
/// PR exists). The "STACK ON: <branch> (PR #N)" prefix is preserved in both so existing
/// consumers/tests that match it keep working. Selected by dispatch-time effective `workspace_mode`
/// (INF-418). Mirrors Go `stackContextHint`.
pub(crate) fn stack_context_hint(branch: &str, pr_number: i64, workspace_mode: &str) -> String {
    let pr = if pr_number > 0 {
        format!(" (PR #{pr_number})")
    } else {
        String::new()
    };
    if workspace_mode == WORKSPACE_MODE_CLONE {
        // This dispatch is an independent clone — no cross-ticket checkout lock — so check the
        // predecessor out directly. `gt get <PR#>` hydrates the whole stack when a PR exists.
        let checkout = if pr_number > 0 {
            format!("`gt get {pr_number}`")
        } else {
            format!("`gt checkout {branch}`")
        };
        return format!(
            "STACK ON: {branch}{pr} — create your branch stacked on this predecessor. \
             This workspace is an independent clone (no cross-ticket checkout lock), so: \
             (1) {checkout}; (2) `gt track --parent {branch}`; \
             (3) implement, committing with `gt modify --commit`/`gt modify --update`; \
             (4) `gt submit --draft --no-edit`."
        );
    }
    // Worktree mode: the predecessor branch is checkout-locked in its sibling worktree, so branch off
    // its remote tip rather than checking it out — the full 5-step locked-parent recipe.
    format!(
        "STACK ON: {branch}{pr} — create your branch stacked on this predecessor. \
         The predecessor branch is checkout-locked in a sibling worktree, so: \
         (1) `git fetch origin {branch}`; (2) `git switch -c <your-branch> origin/{branch}`; \
         (3) `gt track --parent {branch}`; (4) implement, committing with `gt modify --commit`/`gt modify --update`; \
         (5) `gt submit --no-stack --draft --no-edit`."
    )
}

/// Reports whether a blocker is in a cancel-type state. A `None`-state blocker is not cancelled (it is
/// simply unknown/not-cleared). Mirrors Go `isCanceledBlocker`.
fn is_canceled_blocker(b: &BlockerRef, canceled: &HashSet<String>) -> bool {
    match &b.state {
        None => false,
        Some(s) => canceled.contains(&normalize_state(s)),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use rhapsody_core::{BlockerRef, Issue};
    use rhapsody_store::{
        DayRollup, EventHit, EventQuery, EventRow, Noop, Recovery, RunEnd, RunFilter, RunMessage,
        RunProgress, RunStart, RunSummary, Sqlite, Store, StoreError, StorePath, Totals,
    };
    use rhapsody_tracker::TrackerError;
    use rhapsody_tracker::fake::{BranchInfo, Fake};

    use super::*;
    use crate::orchestrator::Orchestrator;
    use crate::testsupport::{DispatchedEntries, empty_effective, record_entries, set_of};

    /// A legacy-path (no projects) orchestrator with a fake tracker, a recording spawn (captures the
    /// running entry so tests can read `stack_context`), and a dependency mode. Mirrors Go
    /// `newPromoteOrch`.
    fn new_promote_orch(tr: Arc<Fake>, mode: &str) -> (Orchestrator, DispatchedEntries) {
        let mut eff = empty_effective(tr);
        eff.active_states = set_of(&["todo", "in progress"]);
        eff.terminal_states = set_of(&["done", "cancelled"]);
        eff.canceled_states = set_of(&["cancelled"]);
        eff.review_states = set_of(&["in review"]);
        eff.dependency_mode = mode.to_string();
        eff.max_concurrent = 10;
        eff.max_retry_backoff_ms = 300_000;
        eff.poll_interval = Duration::from_secs(3600);
        let mut o = Orchestrator::new("WORKFLOW.md");
        o.eff = Some(eff);
        let dispatched: DispatchedEntries = Arc::new(Mutex::new(Vec::new()));
        o.spawn = Some(record_entries(&dispatched));
        (o, dispatched)
    }

    /// A Backlog dependent MT-2 (`b2`) blocked by MT-1 (`a1`) in the given blocker state. Mirrors Go
    /// `backlogDep`.
    fn backlog_dep(blocker_state: &str) -> Issue {
        Issue {
            id: "b2".into(),
            identifier: "MT-2".into(),
            title: "dependent".into(),
            state: "Backlog".into(),
            team_id: "team-1".into(),
            blocked_by: Some(vec![BlockerRef {
                id: Some("a1".into()),
                identifier: Some("MT-1".into()),
                state: Some(blocker_state.into()),
            }]),
            ..Default::default()
        }
    }

    fn dispatched_len(d: &DispatchedEntries) -> usize {
        d.lock().expect("dispatched lock").len()
    }

    // disabled-is-noop: a disabled-mode (and unset-mode) project issues ZERO backlog fetches and ZERO
    // moves across ticks, even with a cleared-blocker dependent present. Mirrors Go
    // `TestPromoteUnblockedDisabledIsNoop`.
    #[tokio::test]
    async fn promote_unblocked_disabled_is_noop() {
        for mode in ["disabled", ""] {
            let mut f = Fake::new();
            f.blocked_backlog = vec![backlog_dep("Done")];
            let tr = Arc::new(f);
            let (mut o, dispatched) = new_promote_orch(Arc::clone(&tr), mode);
            o.promote_unblocked().await;
            o.promote_unblocked().await;
            assert_eq!(
                tr.blocked_backlog_calls(),
                0,
                "mode={mode:?}: no fetch in disabled"
            );
            assert_eq!(tr.move_to_type_calls().len(), 0, "mode={mode:?}");
            assert_eq!(dispatched_len(&dispatched), 0, "mode={mode:?}");
        }
    }

    // graphite: an In-Review blocker clears → Backlog→Todo move + a stashed STACK ON hint carrying the
    // predecessor branch + PR (consumed by the next-tick dispatch). Promote itself does NOT dispatch.
    // Mirrors Go `TestPromoteUnblockedGraphitePromotesAndStacks`.
    #[tokio::test]
    async fn promote_unblocked_graphite_promotes_and_stacks() {
        let mut f = Fake::new();
        f.blocked_backlog = vec![backlog_dep("In Review")];
        f.branch_by_id = HashMap::from([(
            "a1".to_string(),
            BranchInfo {
                branch: "feat/mt-1".into(),
                pr: 7,
            },
        )]);
        f.move_to_type_name = "Todo".into();
        let tr = Arc::new(f);
        let (mut o, dispatched) = new_promote_orch(Arc::clone(&tr), "graphite");

        o.promote_unblocked().await;

        let moves = tr.move_to_type_calls();
        assert_eq!(moves.len(), 1);
        assert_eq!(moves[0].state_type, "unstarted");
        assert_eq!(
            dispatched_len(&dispatched),
            0,
            "promote must NOT dispatch inline"
        );
        // The stash holds the mode-agnostic predecessor fact (branch + PR); the recipe is rendered at
        // dispatch with the dispatch-time workspace_mode (INF-418).
        let h = o.pending_stack.get("b2").expect("pending_stack[b2]");
        assert_eq!(h.branch, "feat/mt-1");
        assert_eq!(h.pr_number, 7);
    }

    // dag: only a terminal blocker clears (In Review does not); promotion stashes NO hint and makes no
    // branch lookup (fresh-from-main). Mirrors Go `TestPromoteUnblockedDagPromotesNoStack`.
    #[tokio::test]
    async fn promote_unblocked_dag_promotes_no_stack() {
        let mut f = Fake::new();
        f.blocked_backlog = vec![backlog_dep("Done")];
        f.move_to_type_name = "Todo".into();
        let tr = Arc::new(f);
        let (mut o, dispatched) = new_promote_orch(Arc::clone(&tr), "dag");

        o.promote_unblocked().await;

        assert_eq!(tr.move_to_type_calls().len(), 1, "dag should move once");
        assert_eq!(dispatched_len(&dispatched), 0);
        assert!(
            !o.pending_stack.contains_key("b2"),
            "dag must stash no stack hint"
        );
        assert_eq!(
            tr.branch_by_id_calls(),
            0,
            "dag must not look up a predecessor branch"
        );
    }

    // The next dispatch of a promoted issue consumes the stashed stack hint (and clears it). Mirrors Go
    // `TestDispatchConsumesPendingStack`.
    #[tokio::test]
    async fn dispatch_consumes_pending_stack() {
        let (mut o, dispatched) = new_promote_orch(Arc::new(Fake::new()), "graphite");
        o.pending_stack.insert(
            "x1".into(),
            StackHint {
                branch: "feat/mt-1".into(),
                pr_number: 7,
            },
        );

        o.dispatch_issue(
            Issue {
                id: "x1".into(),
                identifier: "MT-9".into(),
                title: "dep".into(),
                state: "Todo".into(),
                ..Default::default()
            },
            None,
            None,
            String::new(),
        );

        let entries = dispatched.lock().expect("dispatched lock");
        assert_eq!(entries.len(), 1);
        assert!(
            entries[0]
                .stack_context
                .contains("STACK ON: feat/mt-1 (PR #7)"),
            "dispatch did not consume the stash: stack_context = {:?}",
            entries[0].stack_context
        );
        assert!(
            !o.pending_stack.contains_key("x1"),
            "stash must be cleared after consumption"
        );
    }

    // A label-gated project does NOT auto-promote a dependent lacking the required label. Mirrors Go
    // `TestPromoteUnblockedLabelGate`.
    #[tokio::test]
    async fn promote_unblocked_label_gate() {
        let mut f = Fake::new();
        f.blocked_backlog = vec![backlog_dep("Done")]; // MT-2, no labels
        f.move_to_type_name = "Todo".into();
        let tr = Arc::new(f);
        let (mut o, _) = new_promote_orch(Arc::clone(&tr), "dag");
        o.eff.as_mut().expect("eff").labels = set_of(&["ready"]);
        o.promote_unblocked().await;
        assert_eq!(
            tr.move_to_type_calls().len(),
            0,
            "label-less dependent must not be promoted under a label filter"
        );
    }

    // dag: an In-Review blocker does NOT clear → no promotion. Mirrors Go
    // `TestPromoteUnblockedDagInReviewStillBlocked`.
    #[tokio::test]
    async fn promote_unblocked_dag_in_review_still_blocked() {
        let mut f = Fake::new();
        f.blocked_backlog = vec![backlog_dep("In Review")];
        let tr = Arc::new(f);
        let (mut o, dispatched) = new_promote_orch(Arc::clone(&tr), "dag");
        o.promote_unblocked().await;
        assert_eq!(tr.move_to_type_calls().len(), 0);
        assert_eq!(dispatched_len(&dispatched), 0);
    }

    // An edge-less Backlog ticket is never auto-promoted (SAFETY boundary). Mirrors Go
    // `TestPromoteUnblockedEdgelessIgnored`.
    #[tokio::test]
    async fn promote_unblocked_edgeless_ignored() {
        let mut f = Fake::new();
        f.blocked_backlog = vec![Issue {
            id: "b9".into(),
            identifier: "MT-9".into(),
            title: "standalone".into(),
            state: "Backlog".into(),
            ..Default::default()
        }];
        f.move_to_type_name = "Todo".into();
        let tr = Arc::new(f);
        let (mut o, dispatched) = new_promote_orch(Arc::clone(&tr), "graphite");
        o.promote_unblocked().await;
        assert_eq!(tr.move_to_type_calls().len(), 0);
        assert_eq!(dispatched_len(&dispatched), 0);
    }

    // A cancelled blocker orphans the dependent: no move, no dispatch (graphite/dag). Mirrors Go
    // `TestPromoteUnblockedCancelledOrphan`.
    #[tokio::test]
    async fn promote_unblocked_cancelled_orphan() {
        let mut f = Fake::new();
        f.blocked_backlog = vec![backlog_dep("Cancelled")];
        let tr = Arc::new(f);
        let (mut o, dispatched) = new_promote_orch(Arc::clone(&tr), "graphite");
        o.promote_unblocked().await;
        assert_eq!(tr.move_to_type_calls().len(), 0);
        assert_eq!(dispatched_len(&dispatched), 0);
    }

    // Idempotency: a ticket already running/claimed is not re-promoted. Mirrors Go
    // `TestPromoteUnblockedIdempotentRunningClaimed`.
    #[tokio::test]
    async fn promote_unblocked_idempotent_running_claimed() {
        let mut f = Fake::new();
        f.blocked_backlog = vec![backlog_dep("Done")];
        f.move_to_type_name = "Todo".into();
        let tr = Arc::new(f);
        let (mut o, dispatched) = new_promote_orch(Arc::clone(&tr), "dag");
        o.claimed.insert("b2".into()); // already in flight
        o.promote_unblocked().await;
        assert_eq!(tr.move_to_type_calls().len(), 0);
        assert_eq!(dispatched_len(&dispatched), 0);
    }

    // A Backlog→Todo move failure leaves the ticket un-dispatched (re-tried next tick). Mirrors Go
    // `TestPromoteUnblockedMoveFailureNoDispatch`.
    #[tokio::test]
    async fn promote_unblocked_move_failure_no_dispatch() {
        let mut f = Fake::new();
        f.blocked_backlog = vec![backlog_dep("Done")];
        f.move_to_type_err = Some(TrackerError::Other("linear 500".into()));
        let tr = Arc::new(f);
        let (mut o, dispatched) = new_promote_orch(Arc::clone(&tr), "dag");
        o.promote_unblocked().await;
        assert_eq!(
            tr.move_to_type_calls().len(),
            1,
            "move should be attempted once"
        );
        assert_eq!(
            dispatched_len(&dispatched),
            0,
            "a failed move must not dispatch"
        );
    }

    /// A store whose `issue_history` fails, delegating everything else to [`Noop`], to exercise the
    /// conservative [`Orchestrator::has_prior_run`] error path. Mirrors Go `errHistStore`.
    struct ErrHistStore(Noop);

    impl Store for ErrHistStore {
        fn issue_history(&self, _: &str, _: &str, _: i64) -> Result<Vec<RunSummary>, StoreError> {
            Err(StoreError::Disabled)
        }
        fn start_run(&self, r: RunStart) -> Result<i64, StoreError> {
            self.0.start_run(r)
        }
        fn end_run(&self, run_id: i64, e: RunEnd) -> Result<(), StoreError> {
            self.0.end_run(run_id, e)
        }
        fn update_run_progress(&self, run_id: i64, p: RunProgress) -> Result<(), StoreError> {
            self.0.update_run_progress(run_id, p)
        }
        fn append_events(&self, run_id: i64, ev: &[EventRow]) -> Result<(), StoreError> {
            self.0.append_events(run_id, ev)
        }
        fn save_retry(&self, r: rhapsody_store::RetryRow) -> Result<(), StoreError> {
            self.0.save_retry(r)
        }
        fn delete_retry(&self, issue_id: &str) -> Result<(), StoreError> {
            self.0.delete_retry(issue_id)
        }
        fn save_claim(&self, id: &str, state: &str, slug: &str) -> Result<(), StoreError> {
            self.0.save_claim(id, state, slug)
        }
        fn delete_claim(&self, issue_id: &str) -> Result<(), StoreError> {
            self.0.delete_claim(issue_id)
        }
        fn load_recovery(&self) -> Result<Recovery, StoreError> {
            self.0.load_recovery()
        }
        fn mark_running_interrupted(&self) -> Result<i64, StoreError> {
            self.0.mark_running_interrupted()
        }
        fn save_totals(&self, t: Totals) -> Result<(), StoreError> {
            self.0.save_totals(t)
        }
        fn load_totals(&self) -> Result<Totals, StoreError> {
            self.0.load_totals()
        }
        fn list_runs(&self, f: RunFilter) -> Result<Vec<RunSummary>, StoreError> {
            self.0.list_runs(f)
        }
        fn list_issue_runs(&self, f: RunFilter) -> Result<Vec<RunSummary>, StoreError> {
            self.0.list_issue_runs(f)
        }
        fn day_totals(
            &self,
            since: &str,
            now: &str,
        ) -> Result<rhapsody_store::DayTotals, StoreError> {
            self.0.day_totals(since, now)
        }
        fn get_run(&self, run_id: i64) -> Result<Option<RunSummary>, StoreError> {
            self.0.get_run(run_id)
        }
        fn run_events(&self, run_id: i64) -> Result<Vec<EventRow>, StoreError> {
            self.0.run_events(run_id)
        }
        fn search_events(&self, q: EventQuery) -> Result<Vec<EventHit>, StoreError> {
            self.0.search_events(q)
        }
        fn earliest_run_start(&self) -> Result<Option<String>, StoreError> {
            self.0.earliest_run_start()
        }
        fn metrics(&self, since_days: i64, project: &str) -> Result<Vec<DayRollup>, StoreError> {
            self.0.metrics(since_days, project)
        }
        fn insert_run_message(&self, id: i64, b: &str, ms: i64) -> Result<i64, StoreError> {
            self.0.insert_run_message(id, b, ms)
        }
        fn mark_oldest_run_message_delivered(&self, id: i64, turn: i64) -> Result<(), StoreError> {
            self.0.mark_oldest_run_message_delivered(id, turn)
        }
        fn expire_run_messages(&self, run_id: i64) -> Result<(), StoreError> {
            self.0.expire_run_messages(run_id)
        }
        fn list_run_messages(&self, run_id: i64) -> Result<Vec<RunMessage>, StoreError> {
            self.0.list_run_messages(run_id)
        }
        fn save_review_watch(&self, w: rhapsody_store::ReviewWatchRow) -> Result<(), StoreError> {
            self.0.save_review_watch(w)
        }
        fn mark_review_requested(
            &self,
            key: &rhapsody_store::ReviewWatchKey,
            sha: &str,
        ) -> Result<(), StoreError> {
            self.0.mark_review_requested(key, sha)
        }
        fn mark_review_completed(
            &self,
            key: &rhapsody_store::ReviewWatchKey,
            sha: &str,
            status: &str,
        ) -> Result<(), StoreError> {
            self.0.mark_review_completed(key, sha, status)
        }
        fn mark_review_truncated(
            &self,
            key: &rhapsody_store::ReviewWatchKey,
        ) -> Result<(), StoreError> {
            self.0.mark_review_truncated(key)
        }
        fn drop_review_watch(
            &self,
            key: &rhapsody_store::ReviewWatchKey,
        ) -> Result<(), StoreError> {
            self.0.drop_review_watch(key)
        }
        fn get_review_watch(
            &self,
            key: &rhapsody_store::ReviewWatchKey,
        ) -> Result<Option<rhapsody_store::ReviewWatchRow>, StoreError> {
            self.0.get_review_watch(key)
        }
        fn load_review_watch(&self) -> Result<Vec<rhapsody_store::ReviewWatchRow>, StoreError> {
            self.0.load_review_watch()
        }
        fn load_live_review_watch(
            &self,
        ) -> Result<Vec<rhapsody_store::ReviewWatchRow>, StoreError> {
            self.0.load_live_review_watch()
        }
        fn prune(&self, retention_days: i64) -> Result<(), StoreError> {
            self.0.prune(retention_days)
        }
        fn close(&self) -> Result<(), StoreError> {
            self.0.close()
        }
    }

    // hasPriorRun is conservative on a store read error: it returns true, so a ticket we cannot verify
    // is NOT promoted. Mirrors Go `TestPromoteUnblockedStoreErrorIsConservative`.
    #[tokio::test]
    async fn promote_unblocked_store_error_is_conservative() {
        let mut f = Fake::new();
        f.blocked_backlog = vec![backlog_dep("Done")];
        f.move_to_type_name = "Todo".into();
        let tr = Arc::new(f);
        let (mut o, dispatched) = new_promote_orch(Arc::clone(&tr), "dag");
        o.set_store(Arc::new(ErrHistStore(Noop)));
        o.promote_unblocked().await;
        assert_eq!(
            tr.move_to_type_calls().len(),
            0,
            "a store read error must NOT promote"
        );
        assert_eq!(dispatched_len(&dispatched), 0);
    }

    // Never-run guard (durable): a ticket with a prior run row is NEVER re-promoted (Stop/park stays
    // authoritative), even after clearing o.claimed (restart sim); a never-run sibling with the same
    // cleared blocker IS promoted. Mirrors Go `TestPromoteUnblockedNeverRunGuardDurable`.
    #[tokio::test]
    async fn promote_unblocked_never_run_guard_durable() {
        let st: Arc<dyn Store + Send + Sync> =
            Arc::new(Sqlite::open(StorePath::InMemory).expect("open store"));
        // MT-2 has already run (e.g. dispatched then Stopped → Backlog); MT-3 never has.
        st.start_run(RunStart {
            issue_id: "b2".into(),
            issue_identifier: "MT-2".into(),
            title: "dependent".into(),
            ..Default::default()
        })
        .expect("seed run");

        let ran_dep = backlog_dep("Done"); // MT-2 (has a prior run row)
        let mut fresh_dep = backlog_dep("Done"); // becomes MT-3 (never run)
        fresh_dep.id = "b3".into();
        fresh_dep.identifier = "MT-3".into();

        let mut f = Fake::new();
        f.blocked_backlog = vec![ran_dep, fresh_dep];
        f.move_to_type_name = "Todo".into();
        let tr = Arc::new(f);
        let (mut o, _) = new_promote_orch(Arc::clone(&tr), "dag");
        o.set_store(st);
        // Simulate a restart: o.claimed is empty (Stop deleted it; recovery hasn't re-added). The
        // durable store row is the only thing that must still hold MT-2.

        o.promote_unblocked().await;

        // Only the never-run MT-3 is promoted; the previously-run MT-2 is held.
        let moves = tr.move_to_type_calls();
        assert_eq!(moves.len(), 1, "only never-run MT-3 should be promoted");
        assert_eq!(moves[0].issue_id, "b3");
    }
}
