//! state — the httpapi [`StateProvider`] adapter over the orchestrator's off-loop
//! [`rhapsody_orchestrator::ControlHandle`].
//!
//! Go hands the `*Orchestrator` straight to `httpapi.New` because `*Orchestrator` satisfies Go's
//! `StateProvider` interface. Rust cannot alias the control loop's `&mut self` (which owns the
//! scheduling state) with the HTTP tasks' `&self`, so the orchestrator moves into the loop task and
//! the daemon serves HTTP through the cloneable [`ControlHandle`] it snapshotted first. This adapter
//! (F1) implements [`StateProvider`] by delegating to that handle — the same off-loop surface Go's
//! interface methods reach — mapping the orchestrator's typed results into the httpapi trait's shapes.

use std::sync::Arc;

use async_trait::async_trait;

use chrono::Utc;
use rhapsody_agent::LogEntry;
use rhapsody_config::workflow::Definition;
use rhapsody_core::Project;
use rhapsody_httpapi::{
    ConfigValidateError, HistoryStore, RunActionError, SnapshotError, StateProvider,
};
use rhapsody_orchestrator::prstate::PrCoord;
use rhapsody_orchestrator::reviewconsole::{ReviewControlOutcome, ReviewsView};
use rhapsody_orchestrator::teamsmemory::{
    InvalidateView, PostView, RecallView, ReinstateView, RetainView, RoomView, RosterView,
    TeamsMemory, TeamsMemoryError, TeamsView,
};
use rhapsody_orchestrator::{
    CancelWait, ControlHandle, HandoffResult, Identity, IssueKey, IssueLifecycleRow, ReadsError,
    RefreshResult, ReloadError, ResumeResult, RunMessageResult, Snapshot, StopResult,
};
use rhapsody_store::{
    DayRollup, DayTotals, EventHit, EventQuery, EventRow, RunFilter, RunMessage, RunSummary, Store,
    StoreError,
};

/// Narrows the orchestrator's full [`Store`] handle to the httpapi read-only [`HistoryStore`]. The
/// blanket `impl<S: Store> HistoryStore for S` makes a *concrete* store usable directly, but the
/// daemon only holds an already-erased `Arc<dyn Store>` (there is no trait-object upcast from
/// `dyn Store` to `dyn HistoryStore`), so this thin wrapper re-erases it, forwarding each read to the
/// SAME shared store the orchestrator writes to. The Rust analog of Go's `api.History()` narrowing.
struct HistoryView(Arc<dyn Store + Send + Sync>);

impl HistoryStore for HistoryView {
    fn list_runs(&self, f: RunFilter) -> Result<Vec<RunSummary>, StoreError> {
        self.0.list_runs(f)
    }
    fn list_issue_runs(&self, f: RunFilter) -> Result<Vec<RunSummary>, StoreError> {
        self.0.list_issue_runs(f)
    }
    fn day_totals(&self, since: &str, now: &str) -> Result<DayTotals, StoreError> {
        self.0.day_totals(since, now)
    }
    fn issue_history(
        &self,
        identifier: &str,
        project: &str,
        limit: i64,
    ) -> Result<Vec<RunSummary>, StoreError> {
        self.0.issue_history(identifier, project, limit)
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
    fn metrics(&self, since_days: i64, project: &str) -> Result<Vec<DayRollup>, StoreError> {
        self.0.metrics(since_days, project)
    }
    fn list_run_messages(&self, run_id: i64) -> Result<Vec<RunMessage>, StoreError> {
        self.0.list_run_messages(run_id)
    }
}

/// The daemon's [`StateProvider`]: the httpapi handlers' read/action surface, backed by the
/// orchestrator's off-loop [`ControlHandle`].
pub struct DaemonState {
    handle: ControlHandle,
    /// The read-only history view, narrowed once from the handle's shared store (stable for the
    /// daemon's lifetime — the store is injected before `Run`).
    history: Arc<dyn HistoryStore>,
    /// Where this daemon reads `teams.yaml`, for the enable flow's `GET`/`POST
    /// /api/v1/teams/config` (STUDIO-652). Empty ⇒ no on-disk runtime home, so there is no
    /// `teams.yaml` to read or write and the endpoint says so.
    ///
    /// Carried HERE rather than on `ControlHandle` because it is a boot-time path the control loop
    /// has no use for: `run::run` already resolves it (to decide whether to load Teams at all), and
    /// threading it through the orchestrator would give the control task a field only the HTTP task
    /// ever reads.
    teams_config_path: String,
}

impl DaemonState {
    /// Wraps the orchestrator's control handle as the httpapi provider. Snapshot the handle (via
    /// `o.control()`) BEFORE the orchestrator moves into the control-loop task.
    pub fn new(handle: ControlHandle) -> Self {
        let history: Arc<dyn HistoryStore> = Arc::new(HistoryView(handle.store()));
        Self {
            handle,
            history,
            teams_config_path: String::new(),
        }
    }

    /// Names the `teams.yaml` the enable flow reads and writes (STUDIO-652) — the same path
    /// `run::run` resolved to decide whether to load Teams at boot, so the endpoint and the daemon
    /// can never disagree about which file matters.
    pub fn with_teams_config_path(mut self, path: impl Into<String>) -> Self {
        self.teams_config_path = path.into();
        self
    }
}

#[async_trait]
impl StateProvider for DaemonState {
    async fn snapshot(&self) -> Result<Snapshot, SnapshotError> {
        // `None` means the control loop is not serving — it is gone (the daemon is shutting down) or
        // has not published its first snapshot yet; Go's `Snapshot(ctx)` surfaces that as an error the
        // handler renders 503 `snapshot_unavailable`. The read itself is off-loop (STUDIO-551), so it
        // never waits on the control task's current tick.
        self.handle
            .snapshot()
            .ok_or_else(|| SnapshotError::new("daemon is not running"))
    }

    async fn issue_assignees(
        &self,
        keys: &[IssueKey],
    ) -> std::collections::HashMap<String, String> {
        // Off-loop and best-effort (STUDIO-735), exactly like the lifecycle decoration beside it:
        // the handle answers from its TTL cache, the store's own routing ledger and the reads
        // cell's tracker, and answers nothing for the rest.
        self.handle.issue_assignees(keys).await
    }

    fn history(&self) -> Arc<dyn HistoryStore> {
        Arc::clone(&self.history)
    }

    async fn issue_lifecycles(
        &self,
        ids: &[String],
    ) -> std::collections::HashMap<String, IssueLifecycleRow> {
        // Off-loop and best-effort (STUDIO-702): the handle resolves what it can from its TTL cache
        // and the reads cell's tracker, and answers nothing for the rest. Nothing here can fail the
        // listing this decorates.
        self.handle.issue_lifecycles(ids).await
    }

    fn run_transcript(&self, run_id: i64) -> Option<Vec<LogEntry>> {
        // Go's `([]agent.LogEntry, bool)` → `Option`: `found == false` (no such run row) is `None`.
        let (entries, found) = self.handle.run_transcript(run_id);
        found.then_some(entries)
    }

    async fn list_linear_projects(&self) -> Result<Vec<Project>, ReadsError> {
        self.handle.list_linear_projects().await
    }

    async fn connected_viewer(&self) -> (Identity, Option<String>) {
        // The tracker resolution error is surfaced only for the handler's log line (Go's
        // `(Identity, error)` → `(Identity, Option<String>)`).
        let (identity, err) = self.handle.connected_viewer().await;
        (identity, err.map(|e| e.to_string()))
    }

    async fn stop_run(&self, run_id: i64) -> Result<StopResult, RunActionError> {
        // No HTTP request-cancel is threaded from the handler (Go drops the `context.Context` here);
        // the handle's own lifetime ctx bounds the reply wait, so a never-cancelling request ctx is
        // correct. A failed control round-trip becomes the 500 `stop_failed` error.
        self.handle
            .stop_run(CancelWait::default(), run_id)
            .await
            .map_err(|e| RunActionError::new(e.to_string()))
    }

    async fn resume_run(&self, run_id: i64) -> Result<ResumeResult, RunActionError> {
        self.handle
            .resume_run(CancelWait::default(), run_id)
            .await
            .map_err(|e| RunActionError::new(e.to_string()))
    }

    async fn handoff_run(&self, run_id: i64) -> Result<HandoffResult, RunActionError> {
        // Like stop/resume, no HTTP request-cancel is threaded (the handle's lifetime ctx bounds the
        // reply wait); a failed control round-trip becomes the 500 `handoff_failed` error (TRA-242).
        self.handle
            .handoff_run(CancelWait::default(), run_id)
            .await
            .map_err(|e| RunActionError::new(e.to_string()))
    }

    async fn send_run_message(&self, run_id: i64, text: &str) -> RunMessageResult {
        self.handle.send_run_message(run_id, text).await
    }

    fn refresh(&self) -> RefreshResult {
        self.handle.refresh()
    }

    fn workflow_path(&self) -> &str {
        self.handle.workflow_path()
    }

    fn validate_config(&self, def: &Definition) -> Result<(), ConfigValidateError> {
        // Map the orchestrator's load-pipeline error onto the httpapi classification: a
        // ValidateDispatch rejection keeps its structured `ValidationError` (so the typed config POST
        // maps it to a field code); every other stage (decode / resolve / buildEffective) is opaque.
        match self.handle.validate_config(def) {
            Ok(()) => Ok(()),
            Err(ReloadError::Validation(v)) => Err(ConfigValidateError::Validation(v)),
            Err(other) => Err(ConfigValidateError::Other(other.to_string())),
        }
    }

    fn capabilities_registry(&self) -> Option<Vec<rhapsody_config::capabilities::CapabilityDef>> {
        // The daemon does not yet cache the loaded registry off the control loop: that plumbing
        // (an orchestrator/`ControlHandle` `capabilities_registry` field seeded from
        // `~/.rhapsody/capabilities.yaml`) lands with the capabilities-registry-into-daemon-state
        // task (BO-12). Until then the endpoint honestly serves `[]` rather than re-reading + seeding
        // the file from an HTTP read handler; wiring here becomes a one-line delegate to the handle.
        None
    }

    /// The four Rhapsody Teams memory surfaces (STUDIO-645, T4). Each delegates straight to the
    /// `Arc`-shared [`TeamsMemory`] the control handle carries, so the request is served **entirely
    /// on this HTTP task**: no control-channel round-trip, and therefore no way for a `teams_retain`
    /// to sit behind whatever the current tick is doing (§5.1 — "never blocking the control task").
    ///
    /// `None` — a daemon with no Teams runtime at all — is `teams_disabled`, the same answer a
    /// daemon with `enabled: false` gives.
    async fn teams_roster(&self) -> Result<RosterView, TeamsMemoryError> {
        self.teams_memory()?.roster()
    }

    /// The dashboard's one view (STUDIO-652) — the roster plus the settings that make it legible.
    async fn teams_overview(&self) -> Result<TeamsView, TeamsMemoryError> {
        self.teams_memory()?.overview()
    }

    /// The gate the dashboard reads off `GET /api/v1/version` before it fetches any Teams route
    /// (STUDIO-652). A field read on the shared config — no control-loop round-trip.
    fn teams_enabled(&self) -> bool {
        self.handle.teams_memory().is_some_and(|mem| mem.enabled())
    }

    // --- the ticketless review console (STUDIO-722, slice 8; design §7, §15-e) ---
    //
    // All three round-trip the control task rather than reading the store directly the way
    // `HistoryView` does, and the reason is the watch set's single-writer property: the two
    // controls read `running`/`claimed` alongside it to make the F-DUP in-flight decision, and only
    // the control task owns those. The read follows them so the console cannot observe a watch set
    // half-written by the tick it is racing.

    async fn reviews(&self) -> Result<ReviewsView, StoreError> {
        self.handle.list_reviews().await
    }

    async fn review_rerun(&self, pr: PrCoord) -> ReviewControlOutcome {
        self.handle.rerun_review(pr).await
    }

    async fn review_dismiss(&self, pr: PrCoord) -> ReviewControlOutcome {
        self.handle.dismiss_review(pr).await
    }

    fn teams_config_path(&self) -> &str {
        &self.teams_config_path
    }

    /// The room's read side (STUDIO-650, T5) — the fifth Teams surface, and read-only in the
    /// strongest sense: serving it advances no identity's cursor.
    async fn teams_room(&self, limit: usize) -> Result<RoomView, TeamsMemoryError> {
        self.teams_memory()?.room(limit)
    }

    async fn teams_recall(
        &self,
        identity: &str,
        query: &str,
        state: &str,
    ) -> Result<RecallView, TeamsMemoryError> {
        self.teams_memory()?.recall(identity, query, state).await
    }

    async fn teams_invalidate(
        &self,
        identity: &str,
        fact_id: &str,
        reason: &str,
    ) -> Result<InvalidateView, TeamsMemoryError> {
        self.teams_memory()?
            .invalidate(identity, fact_id, reason)
            .await
    }

    /// §5.3's reversal (STUDIO-689) — the same bank, the same off-loop task, no `reason`.
    async fn teams_reinstate(
        &self,
        identity: &str,
        fact_id: &str,
    ) -> Result<ReinstateView, TeamsMemoryError> {
        self.teams_memory()?.reinstate(identity, fact_id).await
    }

    async fn teams_retain(
        &self,
        run_id: i64,
        content: &str,
    ) -> Result<RetainView, TeamsMemoryError> {
        self.teams_memory()?
            .retain_for_run(run_id, content, Utc::now())
            .await
    }

    /// The room's write side (STUDIO-653, T6) — the ONE Teams surface that touches both seams, in
    /// this order and deliberately so:
    ///
    /// 1. **Off-loop, on this HTTP task:** resolve the run to its identity, validate `to` against
    ///    the roster, and append through the room's single writer. That append IS the post, and it
    ///    is complete before the control task is involved at all.
    /// 2. **On the loop, best-effort:** the poster's `teams.message` timeline row and the
    ///    teammate-wrapped delivery into any live recipient, both of which need `running` /
    ///    `mailboxes` / `event_seq`. A loop that is gone, a recipient that is not running and a
    ///    full mailbox all report `delivered: 0` and degrade to §0.5's catch-up — nothing is
    ///    queued, nothing is retried, and nothing is lost, because the post is already in the log.
    async fn teams_post(
        &self,
        run_id: i64,
        body: &str,
        to: &str,
        refs: &[String],
    ) -> Result<PostView, TeamsMemoryError> {
        let mut view = self
            .teams_memory()?
            .post_for_run(run_id, body, to, refs, Utc::now())?;
        view.delivered = self.handle.record_teams_post(run_id, &view, body).await;
        Ok(view)
    }

    /// The room's HUMAN door (STUDIO-661) — and, unlike [`teams_post`](Self::teams_post), it
    /// touches ONE seam rather than two. There is no run to resolve, so there is no timeline row
    /// to write and nobody to deliver to: the append on this HTTP task is the entire operation,
    /// and the control task is never involved at all.
    async fn teams_room_post(
        &self,
        body: &str,
        refs: &[String],
    ) -> Result<PostView, TeamsMemoryError> {
        self.teams_memory()?
            .post_as_operator(body, refs, Utc::now())
    }
}

impl DaemonState {
    /// The shared Teams memory runtime, or [`TeamsMemoryError::Disabled`] when the daemon has none.
    fn teams_memory(&self) -> Result<&Arc<TeamsMemory>, TeamsMemoryError> {
        self.handle.teams_memory().ok_or(TeamsMemoryError::Disabled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rhapsody_orchestrator::Orchestrator;
    use rhapsody_store::{RunFilter, Sqlite, StorePath};

    fn state_with_store() -> DaemonState {
        let store: Arc<dyn Store + Send + Sync> =
            Arc::new(Sqlite::open(StorePath::InMemory).expect("open in-memory store"));
        let mut o = Orchestrator::new("WORKFLOW.md");
        o.set_store(store);
        DaemonState::new(o.control())
    }

    // workflow_path threads through the handle.
    #[test]
    fn workflow_path_passthrough() {
        let st = state_with_store();
        assert_eq!(st.workflow_path(), "WORKFLOW.md");
    }

    // history() serves the injected store's read side (empty list on a fresh in-memory store).
    #[test]
    fn history_reads_the_shared_store() {
        let st = state_with_store();
        let runs = st
            .history()
            .list_runs(RunFilter::default())
            .expect("list runs");
        assert!(runs.is_empty(), "fresh store must have no runs");
    }

    // run_transcript for an absent run id → None (Go `found == false`).
    #[test]
    fn run_transcript_absent_run_is_none() {
        let st = state_with_store();
        assert!(st.run_transcript(4242).is_none());
    }

    // With no control loop running, snapshot() surfaces the 503-mapped error rather than hanging (the
    // events channel has no consumer, so the reply is dropped → `None`).
    #[tokio::test]
    async fn snapshot_without_loop_is_error() {
        let st = state_with_store();
        assert!(
            st.snapshot().await.is_err(),
            "no loop ⇒ snapshot unavailable"
        );
    }
}
