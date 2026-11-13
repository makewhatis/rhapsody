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
use rhapsody_orchestrator::drain::{DrainReason, DrainStatus};
use rhapsody_orchestrator::managerread::ManagerReadOutcome;
use rhapsody_orchestrator::modelsview::{
    RoutingEntry, RoutingView, TeamsModelsApplyResult, TeamsModelsError, TeamsModelsRequest,
    TeamsModelsView,
};
use rhapsody_orchestrator::prstate::PrCoord;
use rhapsody_orchestrator::reviewconsole::{ReviewControlOutcome, ReviewsView};
use rhapsody_orchestrator::rundiff::DiffOutcome;
use rhapsody_orchestrator::runmerge::{MergeControlOutcome, MergeabilityOutcome};
use rhapsody_orchestrator::teamsmemory::{
    InvalidateView, PostView, RecallView, ReinstateView, RetainView, RoomView, RosterView,
    TeamsMemory, TeamsMemoryError, TeamsView,
};
use rhapsody_orchestrator::{
    CancelWait, ControlHandle, HandoffResult, Identity, IssueKey, IssueLifecycleRow, ReadsError,
    RefreshResult, ReloadError, ResumeHoldError, ResumeHoldResult, ResumeResult, RunHoldView,
    RunMessageResult, Snapshot, StopResult,
};
use rhapsody_store::{
    DayProviderRollup, DayRollup, DayTotals, EventHit, EventQuery, EventRow, IssueHistoryPage,
    ProviderTokens, ReviewWatchRow, RunCostBucket, RunFilter, RunMessage, RunProvenance,
    RunSummary, RunUsage, Store, StoreError,
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
    fn issue_history_page(
        &self,
        identifier: &str,
        project: &str,
        limit: i64,
    ) -> Result<IssueHistoryPage, StoreError> {
        self.0.issue_history_page(identifier, project, limit)
    }
    fn runs_for_issues(
        &self,
        identifiers: &[String],
        limit: i64,
    ) -> Result<Vec<RunSummary>, StoreError> {
        self.0.runs_for_issues(identifiers, limit)
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
    fn metrics_by_provider(
        &self,
        since_days: i64,
        project: &str,
    ) -> Result<Vec<DayProviderRollup>, StoreError> {
        self.0.metrics_by_provider(since_days, project)
    }
    fn list_run_messages(&self, run_id: i64) -> Result<Vec<RunMessage>, StoreError> {
        self.0.list_run_messages(run_id)
    }
    fn load_review_watch(&self) -> Result<Vec<ReviewWatchRow>, StoreError> {
        self.0.load_review_watch()
    }
    fn run_provenance(&self, run_id: i64) -> Result<Option<RunProvenance>, StoreError> {
        self.0.run_provenance(run_id)
    }
    fn run_usage(&self, run_id: i64) -> Result<Option<RunUsage>, StoreError> {
        self.0.run_usage(run_id)
    }
    fn load_run_provenances(
        &self,
        run_ids: &[i64],
    ) -> Result<std::collections::HashMap<i64, RunProvenance>, StoreError> {
        self.0.load_run_provenances(run_ids)
    }
    fn load_review_verdicts(
        &self,
        run_ids: &[i64],
    ) -> Result<std::collections::HashMap<i64, String>, StoreError> {
        self.0.load_review_verdicts(run_ids)
    }
    fn tokens_by_provider(&self, since: &str) -> Result<Vec<ProviderTokens>, StoreError> {
        self.0.tokens_by_provider(since)
    }
    fn run_costs(&self) -> Result<Vec<RunCostBucket>, StoreError> {
        self.0.run_costs()
    }
}

/// The daemon's [`StateProvider`]: the httpapi handlers' read/action surface, backed by the
/// orchestrator's off-loop [`ControlHandle`].
pub struct DaemonState {
    handle: ControlHandle,
    /// The read-only history view, narrowed once from the handle's shared store (stable for the
    /// daemon's lifetime — the store is injected before `Run`).
    history: Arc<dyn HistoryStore>,
    notifications: Option<Arc<dyn Store + Send + Sync>>,
    /// Where this daemon reads `teams.yaml`, for the enable flow's `GET`/`POST
    /// /api/v1/teams/config` (STUDIO-652). Empty ⇒ no on-disk runtime home, so there is no
    /// `teams.yaml` to read or write and the endpoint says so.
    ///
    /// Carried HERE rather than on `ControlHandle` because it is a boot-time path the control loop
    /// has no use for: `run::run` already resolves it (to decide whether to load Teams at all), and
    /// threading it through the orchestrator would give the control task a field only the HTTP task
    /// ever reads.
    teams_config_path: String,
    /// The provider status/catalog runtime (STUDIO-990, P9). `None` ⇒ the trait's empty/dormant
    /// answers, exactly what a daemon with no `providers:` block serves. The two `GET` routes read
    /// this cache only; the one credentialed operation is the operator-guarded refresh POST.
    provider: Option<Arc<crate::providers::ProviderRuntime>>,
    account_now_s: Option<i64>,
    lead_harnesses: Option<Arc<rhapsody_orchestrator::managerselftest::runtime::LeadHarnesses>>,
}

impl DaemonState {
    /// Wraps the orchestrator's control handle as the httpapi provider. Snapshot the handle (via
    /// `o.control()`) BEFORE the orchestrator moves into the control-loop task.
    pub fn new(handle: ControlHandle) -> Self {
        let history: Arc<dyn HistoryStore> = Arc::new(HistoryView(handle.store()));
        let store = handle.store();
        let notifications = if store.notices_enabled() {
            Some(store)
        } else {
            match rhapsody_store::Sqlite::open(rhapsody_store::StorePath::InMemory) {
                Ok(store) => Some(Arc::new(store) as Arc<dyn Store + Send + Sync>),
                Err(error) => {
                    tracing::warn!(%error, "notification state unavailable");
                    None
                }
            }
        };
        Self {
            handle,
            history,
            notifications,
            teams_config_path: String::new(),
            provider: None,
            account_now_s: None,
            lead_harnesses: None,
        }
    }

    /// Attach the provider status/catalog runtime (STUDIO-990, P9). Without it the provider routes
    /// answer exactly what a daemon with no `providers:` block serves.
    pub fn with_provider_runtime(
        mut self,
        runtime: Arc<crate::providers::ProviderRuntime>,
    ) -> Self {
        self.provider = Some(runtime);
        self
    }

    /// Boot test clock for recorded window epochs; production uses the current time.
    pub(crate) fn with_account_clock(mut self, now_s: Option<i64>) -> Self {
        self.account_now_s = now_s;
        self
    }

    /// Names the `teams.yaml` the enable flow reads and writes (STUDIO-652) — the same path
    /// `run::run` resolved to decide whether to load Teams at boot, so the endpoint and the daemon
    /// can never disagree about which file matters.
    pub fn with_teams_config_path(mut self, path: impl Into<String>) -> Self {
        self.teams_config_path = path.into();
        self
    }

    pub fn with_lead_harnesses(
        mut self,
        service: Option<Arc<rhapsody_orchestrator::managerselftest::runtime::LeadHarnesses>>,
    ) -> Self {
        self.lead_harnesses = service;
        self
    }
}

#[async_trait]
impl StateProvider for DaemonState {
    async fn landings(&self) -> Result<rhapsody_orchestrator::landing::LandingsView, String> {
        self.handle.landings().await
    }
    async fn landing_action(
        &self,
        id: i64,
        action: rhapsody_orchestrator::landing::LandingAction,
    ) -> rhapsody_orchestrator::landing::LandingActionOutcome {
        self.handle.landing_action(id, action).await
    }
    fn lead_harnesses(
        &self,
    ) -> Option<Arc<rhapsody_orchestrator::managerselftest::runtime::LeadHarnesses>> {
        self.lead_harnesses.clone()
    }
    fn notification_store(&self) -> Option<Arc<dyn Store + Send + Sync>> {
        self.notifications.clone()
    }
    fn manager_notices(&self) -> Vec<String> {
        self.handle.manager_health_notices()
    }
    fn lead_reports(&self) -> Option<Arc<rhapsody_orchestrator::leadreport::LeadReports>> {
        self.handle.lead_reports()
    }
    fn accounts(&self) -> Vec<rhapsody_orchestrator::accounts::AccountView> {
        self.handle
            .accounts(self.account_now_s.unwrap_or_else(|| Utc::now().timestamp()))
    }
    async fn clear_credential_hold(
        &self,
        harness: &str,
        account: &str,
    ) -> Result<bool, RunActionError> {
        self.handle
            .clear_credential_hold(harness, account)
            .await
            .map_err(RunActionError::new)
    }

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

    async fn review_tickets(&self, ids: &[String]) -> std::collections::HashSet<String> {
        // Off-loop and best-effort on the same terms (STUDIO-780): the handle resolves the review
        // marker from its own TTL memo and the reads cell's tracker, and answers nothing for the
        // rest — which the console reads exactly as "not a review ticket".
        self.handle.review_tickets(ids).await
    }

    fn run_transcript(&self, run_id: i64) -> Option<Vec<LogEntry>> {
        // Go's `([]agent.LogEntry, bool)` → `Option`: `found == false` (no such run row) is `None`.
        let (entries, found) = self.handle.run_transcript(run_id);
        found.then_some(entries)
    }

    fn run_transcript_summary(&self, run_id: i64) -> Option<(Vec<LogEntry>, usize)> {
        self.handle.run_transcript_summary(run_id)
    }

    fn transcript_log_dir(&self) -> std::path::PathBuf {
        self.handle.transcript_log_dir()
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

    async fn send_agent_run_message(&self, run_id: i64, text: &str) -> RunMessageResult {
        self.handle.send_agent_run_message(run_id, text).await
    }

    async fn resume_hold(
        &self,
        run_id: i64,
        note: &str,
    ) -> Result<ResumeHoldResult, ResumeHoldError> {
        // Like stop/resume, no HTTP request-cancel is threaded (the handle's lifetime ctx bounds the
        // reply wait); the tracker/store round-trips run on this request's own task (STUDIO-1053).
        self.handle
            .resume_hold(CancelWait::default(), run_id, note)
            .await
    }

    async fn run_hold(&self, run_id: i64) -> Result<RunHoldView, ResumeHoldError> {
        self.handle.run_hold(run_id).await
    }

    fn refresh(&self) -> RefreshResult {
        self.handle.refresh()
    }

    fn drain_status(&self) -> DrainStatus {
        self.handle.drain_status()
    }

    fn set_drain(&self, active: bool, reason: DrainReason) -> DrainStatus {
        // `Utc::now()` rather than the orchestrator's injectable clock: the handle is off-loop and
        // the loop-owned `now` is not reachable from here. The timestamp is an operator annotation
        // (how long has this drain been waiting), never an input to a decision.
        self.handle.set_drain(active, reason, Utc::now())
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

    // --- the provider status + model catalog surface (STUDIO-990, P9) ---
    //
    // The two reads serve the non-secret cache the runtime applied at boot; neither touches the
    // credential owner or a provider. The refresh is the ONE credentialed operation and is reached
    // only through the operator-guarded POST.

    fn provider_statuses(&self) -> Vec<rhapsody_provider_status::ProviderStatusView> {
        self.provider
            .as_ref()
            .map(|runtime| runtime.statuses())
            .unwrap_or_default()
    }

    fn provider_status(
        &self,
        provider_id: &str,
    ) -> Option<rhapsody_provider_status::ProviderStatusView> {
        self.provider
            .as_ref()
            .and_then(|runtime| runtime.status(provider_id))
    }

    fn provider_catalog(
        &self,
        provider_id: &str,
    ) -> Option<rhapsody_provider_status::CatalogSnapshot> {
        self.provider
            .as_ref()
            .and_then(|runtime| runtime.catalog(provider_id))
    }

    async fn refresh_provider_catalog(
        &self,
        provider_id: &str,
    ) -> Result<rhapsody_provider_status::CatalogSnapshot, rhapsody_provider_status::CatalogError>
    {
        match self.provider.as_ref() {
            Some(runtime) => runtime.refresh_catalog(provider_id).await,
            None => Err(rhapsody_provider_status::CatalogError::Unsupported),
        }
    }

    /// Every place `provider_id` is still selected (STUDIO-1048), computed from the on-disk workflow
    /// plus `teams.yaml` and the profile files beside it. Read-only and best-effort: an unreadable
    /// workflow or a malformed `teams.yaml` contributes no references rather than failing the check
    /// (a removal the operator plainly is not blocked from should not be refused by a parse hiccup),
    /// while the actual write path still validates the whole config against the daemon's load
    /// pipeline.
    fn provider_references(&self, provider_id: &str) -> Vec<rhapsody_config::ProviderReference> {
        let config =
            rhapsody_config::workflow::load(std::path::Path::new(self.handle.workflow_path()))
                .ok()
                .and_then(|def| rhapsody_config::decode::decode(&def).ok());
        let Some(config) = config else {
            return Vec::new();
        };
        let teams = if self.teams_config_path.is_empty() {
            None
        } else {
            rhapsody_config::teams::Teams::try_load(std::path::Path::new(&self.teams_config_path))
                .ok()
        };
        let profiles = if self.teams_config_path.is_empty() {
            Vec::new()
        } else {
            let teams_dir = std::path::Path::new(&self.teams_config_path)
                .parent()
                .map(|p| p.join("teams").join("profiles"))
                .unwrap_or_else(|| std::path::PathBuf::from("teams/profiles"));
            rhapsody_config::profile_providers(&teams_dir)
        };
        rhapsody_config::provider_references(provider_id, &config, teams.as_ref(), &profiles)
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

    async fn review_rerun(&self, pr: PrCoord, reviewer: Option<String>) -> ReviewControlOutcome {
        self.handle.rerun_review_named(pr, reviewer).await
    }

    async fn review_dismiss(&self, pr: PrCoord, reviewer: Option<String>) -> ReviewControlOutcome {
        self.handle.dismiss_review(pr, reviewer).await
    }

    async fn review_clear(&self, pr: PrCoord) -> ReviewControlOutcome {
        self.handle.clear_review(pr).await
    }

    /// `POST /api/v1/runs/{id}/merge` (STUDIO-767). Unlike the three above it does NOT simply
    /// round-trip the control task: the handle plans on the loop, performs the blocking `gh` half
    /// HERE — on this HTTP task, so a stalled merge parks this request and the daemon keeps
    /// ticking — and settles back on the loop.
    async fn merge_run(&self, run_id: i64, confirm: &str) -> MergeControlOutcome {
        self.handle.merge_run(run_id, confirm).await
    }

    /// `GET /api/v1/runs/{id}/mergeability` (STUDIO-790) — the same journey minus its last two
    /// steps: plan on the loop, resolve the pull request here, and stop. Nothing is merged, no
    /// single-flight claim is taken and nothing is recorded, so the console may refetch it.
    async fn run_mergeability(&self, run_id: i64) -> MergeabilityOutcome {
        self.handle.run_mergeability(run_id).await
    }

    /// `GET /api/v1/runs/{id}/diff` (STUDIO-749) — the diff a run produced on its branch.
    ///
    /// The shortest of the four: no control round trip at ALL. The handle reads the run row off
    /// its own store and does the blocking `gh` half here, on this HTTP task, because a diff read
    /// takes no claim, gates on no loop state and records nothing — so there is nothing for the
    /// control task to own (`rundiff`'s module doc).
    async fn run_diff(&self, run_id: i64) -> DiffOutcome {
        self.handle.run_diff(run_id).await
    }

    // --- the manager run's host-served reads (STUDIO-1014 §4.4) ---
    //
    // Each resolves the run's own coordinate on the handle and reads git objects from the live
    // workspace mirror; `diff`/`interdiff` also record the evidence-access log (§5.5). No control
    // round trip beyond the workspace manager lookup, and no claim taken — nothing here can stall
    // dispatch.
    async fn manager_file(&self, run_id: i64, sha: String, path: String) -> ManagerReadOutcome {
        self.handle.manager_file(run_id, &sha, &path).await
    }

    async fn docs_read(&self, run_id: i64, path: String) -> ManagerReadOutcome {
        self.handle.docs_read(run_id, &path).await
    }
    async fn docs_list(&self, run_id: i64, glob: String) -> ManagerReadOutcome {
        self.handle.docs_list(run_id, &glob).await
    }
    async fn tracker_documents(
        &self,
        run_id: i64,
        project: String,
        issue: String,
        query: String,
        excerpt: bool,
    ) -> ManagerReadOutcome {
        self.handle
            .tracker_documents(run_id, &project, &issue, &query, excerpt)
            .await
    }
    async fn manager_ticket(&self, run_id: i64, identifier: String) -> ManagerReadOutcome {
        self.handle.manager_ticket(run_id, &identifier).await
    }

    async fn operator_preferences(&self, run_id: i64, query: String) -> ManagerReadOutcome {
        match self.handle.teams_memory() {
            Some(memory) => memory.operator_preferences(run_id, &query).await,
            None => Err(
                rhapsody_orchestrator::managerread::ManagerReadError::Unavailable(
                    "operator memory unavailable",
                ),
            ),
        }
    }

    async fn investigate(
        &self,
        run_id: i64,
        head: String,
        cmd: String,
    ) -> Result<
        rhapsody_orchestrator::investigate::CommandOutput,
        rhapsody_orchestrator::investigate::InvestigateError,
    > {
        self.handle.investigate(run_id, &head, &cmd).await
    }

    async fn manager_ls(&self, run_id: i64, sha: String, path: String) -> ManagerReadOutcome {
        self.handle.manager_ls(run_id, &sha, &path).await
    }

    async fn manager_grep(
        &self,
        run_id: i64,
        sha: String,
        pattern: String,
        path: String,
    ) -> ManagerReadOutcome {
        self.handle
            .manager_grep(run_id, &sha, &pattern, &path)
            .await
    }

    async fn manager_diff(&self, run_id: i64, from: String, to: String) -> ManagerReadOutcome {
        self.handle.manager_diff(run_id, &from, &to).await
    }

    async fn manager_interdiff(&self, run_id: i64, from: String, to: String) -> ManagerReadOutcome {
        self.handle.manager_interdiff(run_id, &from, &to).await
    }

    async fn manager_patch_id(&self, run_id: i64, sha: String) -> ManagerReadOutcome {
        self.handle.manager_patch_id(run_id, &sha).await
    }

    async fn manager_findings(&self, run_id: i64) -> ManagerReadOutcome {
        self.handle.manager_findings(run_id).await
    }

    // The manager's pull-request reads run on the host's own off-loop `gh` (§4.4); the coordinate
    // is the run's own, resolved on the handle exactly as the git reads' is.
    async fn manager_pr(&self, run_id: i64) -> ManagerReadOutcome {
        self.handle.manager_pr(run_id).await
    }

    async fn manager_pr_activity(&self, run_id: i64, since: String) -> ManagerReadOutcome {
        self.handle.manager_pr_activity(run_id, &since).await
    }

    async fn manager_pr_commits(&self, run_id: i64, since: String) -> ManagerReadOutcome {
        self.handle.manager_pr_commits(run_id, &since).await
    }

    fn teams_config_path(&self) -> &str {
        &self.teams_config_path
    }

    // ── STUDIO-1976: Settings → Models ────────────────────────────────────────
    //
    // The view is built by the orchestrator's pure `modelsview` (the same resolver `teams show`
    // uses), and the writes go through the config crate's comment-preserving editors. The daemon
    // owns the file paths and the room line; the orchestrator owns the resolution.

    async fn teams_models(&self) -> Result<TeamsModelsView, TeamsModelsError> {
        let (teams, cfg, profiles_dir) = self.models_inputs()?;
        Ok(rhapsody_orchestrator::modelsview::build_view(
            &cfg,
            &teams,
            &profiles_dir,
        ))
    }

    async fn teams_models_apply(
        &self,
        req: TeamsModelsRequest,
    ) -> Result<TeamsModelsApplyResult, TeamsModelsError> {
        let (teams, cfg, profiles_dir) = self.models_inputs()?;
        let teams_path = std::path::PathBuf::from(&self.teams_config_path);
        let mut changed: Vec<String> = Vec::new();
        let mut previous: Vec<RoutingEntry> = Vec::new();

        match req {
            TeamsModelsRequest::Identity { name, routing } => {
                let id = find_identity(&teams, &name)?;
                validate_routing(&routing, &cfg)?;
                write_profile_routing(&profiles_dir, &id.profile, &routing)?;
                changed.push(models_line(&name, &routing));
            }
            TeamsModelsRequest::Review {
                harness,
                model,
                provider,
                effort,
            } => {
                if !rhapsody_orchestrator::modelsview::RUNNABLE_HARNESSES
                    .contains(&harness.as_str())
                {
                    return Err(TeamsModelsError::Invalid(format!(
                        "harness {harness:?} is not one this build can run"
                    )));
                }
                let routing = RoutingView {
                    harness: harness.clone(),
                    provider: provider.clone(),
                    model: model.clone(),
                    effort: effort.clone(),
                };
                validate_routing(&routing, &cfg)?;
                rhapsody_config::teams::save_review_routing(
                    &teams_path,
                    &harness,
                    &model,
                    &provider,
                    &effort,
                )
                .map_err(map_teams_write_err)?;
                changed.push(format!("models: review[{harness}] → {harness}/{model}"));
            }
            TeamsModelsRequest::Manager { routing } => {
                validate_routing(&routing, &cfg)?;
                rhapsody_config::teams::save_manager_routing(
                    &teams_path,
                    &routing.harness,
                    &routing.provider,
                    &routing.model,
                    &routing.effort,
                )
                .map_err(map_teams_write_err)?;
                changed.push(format!(
                    "models: manager → {}/{}",
                    routing.harness, routing.model
                ));
            }
            TeamsModelsRequest::MoveAll { routing } => {
                validate_routing(&routing, &cfg)?;
                for id in &teams.roster {
                    if teams.review.is_code_reviewer(id) {
                        continue;
                    }
                    previous.push(RoutingEntry {
                        name: id.name.clone(),
                        routing: read_profile_routing_view(&profiles_dir, &id.profile),
                    });
                    write_profile_routing(&profiles_dir, &id.profile, &routing)?;
                    changed.push(models_line(&id.name, &routing));
                }
            }
            TeamsModelsRequest::Undo { previous: entries } => {
                for entry in entries {
                    let id = find_identity(&teams, &entry.name)?;
                    validate_routing(&entry.routing, &cfg)?;
                    write_profile_routing(&profiles_dir, &id.profile, &entry.routing)?;
                    changed.push(models_line(&entry.name, &entry.routing));
                }
            }
        }

        // Reload the view from disk so it reflects the writes immediately (the live routing the
        // daemon itself applies follows via the `teamsreload` watcher within its poll interval).
        let (teams, cfg, profiles_dir) = self.models_inputs()?;
        let view = rhapsody_orchestrator::modelsview::build_view(&cfg, &teams, &profiles_dir);

        // One operator room line per change; best-effort, exactly like a teammate post.
        for line in &changed {
            let _ = self.teams_room_post(line, &[]).await;
        }

        Ok(TeamsModelsApplyResult {
            view,
            previous,
            changed,
        })
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

    /// The SHARED team bank's recall (STUDIO-1040) — same off-loop task, no
    /// identity: the shared bank belongs to the team, not to a teammate.
    async fn teams_recall_team(
        &self,
        query: &str,
        state: &str,
    ) -> Result<RecallView, TeamsMemoryError> {
        self.teams_memory()?.recall_team(query, state).await
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

    /// A shared-bank correction (STUDIO-1040).
    async fn teams_invalidate_team(
        &self,
        fact_id: &str,
        reason: &str,
    ) -> Result<InvalidateView, TeamsMemoryError> {
        self.teams_memory()?.invalidate_team(fact_id, reason).await
    }

    /// §5.3's reversal (STUDIO-689) — the same bank, the same off-loop task, no `reason`.
    async fn teams_reinstate(
        &self,
        identity: &str,
        fact_id: &str,
    ) -> Result<ReinstateView, TeamsMemoryError> {
        self.teams_memory()?.reinstate(identity, fact_id).await
    }

    /// A shared-bank reversal (STUDIO-1040).
    async fn teams_reinstate_team(&self, fact_id: &str) -> Result<ReinstateView, TeamsMemoryError> {
        self.teams_memory()?.reinstate_team(fact_id).await
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

    /// A SHARED retain (STUDIO-1040) — provenance stamped the same way, so the
    /// caller cannot forge the author; the record just lands in the team bank
    /// instead of its own.
    async fn teams_retain_shared(
        &self,
        run_id: i64,
        content: &str,
    ) -> Result<RetainView, TeamsMemoryError> {
        self.teams_memory()?
            .retain_for_run_scoped(run_id, content, true, Utc::now())
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

    /// The inputs the Models view and writes need: the loaded `teams.yaml`, the decoded workflow
    /// config, and the profiles directory beside `teams.yaml`. `Disabled` when there is no Teams
    /// runtime home or Teams is off.
    fn models_inputs(
        &self,
    ) -> Result<
        (
            rhapsody_config::teams::Teams,
            rhapsody_config::Config,
            std::path::PathBuf,
        ),
        TeamsModelsError,
    > {
        if self.teams_config_path.is_empty() {
            return Err(TeamsModelsError::Disabled);
        }
        let teams_path = std::path::Path::new(&self.teams_config_path);
        let teams = rhapsody_config::teams::Teams::try_load(teams_path)
            .map_err(|e| TeamsModelsError::Io(e.to_string()))?;
        if !teams.enabled {
            return Err(TeamsModelsError::Disabled);
        }
        let def =
            rhapsody_config::workflow::load(std::path::Path::new(self.handle.workflow_path()))
                .map_err(|e| TeamsModelsError::Io(e.to_string()))?;
        let cfg = rhapsody_config::decode::decode(&def)
            .map_err(|e| TeamsModelsError::Io(e.to_string()))?;
        let profiles_dir = teams_path
            .parent()
            .map(|p| p.join("teams").join("profiles"))
            .unwrap_or_else(|| std::path::PathBuf::from("teams/profiles"));
        Ok((teams, cfg, profiles_dir))
    }
}

// ── STUDIO-1976: Settings → Models helpers ────────────────────────────────────

/// The roster identity named `name`, or a 404.
fn find_identity<'a>(
    teams: &'a rhapsody_config::teams::Teams,
    name: &str,
) -> Result<&'a rhapsody_config::teams::Identity, TeamsModelsError> {
    teams
        .roster
        .iter()
        .find(|i| i.name == name)
        .ok_or_else(|| TeamsModelsError::Unknown(format!("no roster identity {name:?}")))
}

/// Refuses a tuple the daemon could not run, BEFORE any file is touched: an unknown harness, a
/// non-canonical or undefined provider, or a provider on `claude` (which uses native auth and names
/// no provider). This is the "invalid tuple is refused and the old value kept" rule.
fn validate_routing(
    routing: &RoutingView,
    cfg: &rhapsody_config::Config,
) -> Result<(), TeamsModelsError> {
    if !routing.harness.is_empty()
        && !rhapsody_orchestrator::modelsview::RUNNABLE_HARNESSES
            .contains(&routing.harness.as_str())
    {
        return Err(TeamsModelsError::Invalid(format!(
            "harness {:?} is not one this build can run",
            routing.harness
        )));
    }
    if !routing.provider.is_empty() {
        rhapsody_config::providers::canonical_provider_id(&routing.provider).map_err(|reason| {
            TeamsModelsError::Invalid(format!(
                "provider {:?} is not a canonical provider id: {reason}",
                routing.provider
            ))
        })?;
        if !cfg.providers.contains_key(&routing.provider) {
            return Err(TeamsModelsError::Invalid(format!(
                "provider {:?} is not defined in WORKFLOW.md",
                routing.provider
            )));
        }
        if routing.harness == "claude" {
            return Err(TeamsModelsError::Invalid(
                "the claude harness uses native auth and names no provider".into(),
            ));
        }
    }
    Ok(())
}

/// Writes one teammate's routing into its profile file's front matter, preserving the body and every
/// comment. A profile with no file of its own (a built-in-only profile) cannot be edited here and is
/// reported rather than silently seeded.
fn write_profile_routing(
    dir: &std::path::Path,
    profile: &str,
    routing: &RoutingView,
) -> Result<(), TeamsModelsError> {
    let path = rhapsody_config::profiles::profile_path(dir, profile);
    if !path.exists() {
        return Err(TeamsModelsError::Invalid(format!(
            "profile {profile:?} has no file at {} to edit; run `rhapsodyd teams fork {profile}` first",
            path.display()
        )));
    }
    let pr = rhapsody_config::profiles::ProfileRouting {
        harness: routing.harness.clone(),
        provider: routing.provider.clone(),
        model: routing.model.clone(),
        effort: routing.effort.clone(),
    };
    rhapsody_config::profiles::save_routing(&path, &pr)
        .map_err(|e| TeamsModelsError::Invalid(e.to_string()))?;
    Ok(())
}

/// The profile file's CURRENT routing front matter, best-effort (empty when the file is absent).
fn read_profile_routing_view(dir: &std::path::Path, profile: &str) -> RoutingView {
    let path = rhapsody_config::profiles::profile_path(dir, profile);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return RoutingView::default();
    };
    let r = rhapsody_config::profiles::read_routing(&text);
    RoutingView {
        harness: r.harness,
        provider: r.provider,
        model: r.model,
        effort: r.effort,
    }
}

/// Maps a `teams.yaml` write failure onto the Models error: a validation rejection is a 400, an I/O
/// failure a 500.
fn map_teams_write_err(err: rhapsody_config::teams::TeamsError) -> TeamsModelsError {
    match err {
        rhapsody_config::teams::TeamsError::Invalid(m)
        | rhapsody_config::teams::TeamsError::Parse(m) => TeamsModelsError::Invalid(m),
        rhapsody_config::teams::TeamsError::Io(m) => TeamsModelsError::Io(m),
    }
}

/// One room line for a routing change, e.g. `models: jerry → opencode/deepseek-v4p1-flash`.
fn models_line(name: &str, routing: &RoutingView) -> String {
    let harness = if routing.harness.is_empty() {
        "inherit"
    } else {
        routing.harness.as_str()
    };
    let model = if routing.model.is_empty() {
        "inherit"
    } else {
        routing.model.as_str()
    };
    format!("models: {name} → {harness}/{model}")
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

    // Real HTTP -> DaemonState -> control event -> durable queue -> harness prompt. In particular,
    // a wiring mistake that delegates agent messages to send_run_message must turn this red.
    #[tokio::test]
    async fn http_message_origin_survives_into_the_harness_prompt() {
        use rhapsody_agent::{Steering, fake};
        use rhapsody_orchestrator::{CancelSignal, effective::build_effective_with_runner};
        use std::time::Duration;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        for agent_origin in [true, false] {
            let dir = crate::testutil::TempDir::new();
            let release = dir.child("release-worker");
            let definition = rhapsody_config::workflow::parse(&format!(
                "---\ntracker:\n  kind: linear\n  endpoint: http://127.0.0.1:9\n  api_key: synthetic\n  project_slug: test\nworkspace:\n  root: {}\nlogging:\n  dir: {}\nstorage:\n  path: ':memory:'\nagent:\n  max_turns: 1\nhooks:\n  timeout_ms: 5000\n  before_run: 'while [ ! -f \"{}\" ]; do sleep 0.01; done'\n---\nWork the ticket.\n",
                dir.child("ws").display(), dir.child("logs").display(), release.display()
            )).unwrap();
            let cfg = rhapsody_config::decode(&definition).unwrap();
            let mut runner = fake::Fake::new();
            runner.capabilities.steering = Steering::BetweenTurns;
            let runner = Arc::new(runner);
            let mut effective = build_effective_with_runner(&cfg, &|_| runner.clone()).unwrap();
            let issue = rhapsody_core::Issue {
                id: "id".into(),
                identifier: "TEST-1".into(),
                state: "In Progress".into(),
                ..Default::default()
            };
            let mut tracker = rhapsody_tracker::fake::Fake::new();
            tracker.candidates = vec![issue.clone()];
            tracker.by_id.insert(issue.id.clone(), issue.clone());
            effective.tracker = Arc::new(tracker);
            effective.projects.clear();
            let store = Arc::new(Sqlite::open(StorePath::InMemory).unwrap());
            let mut o = Orchestrator::new(dir.child("WORKFLOW.md").to_string_lossy());
            o.set_store(store.clone());
            o.eff = Some(effective);
            assert!(
                o.dispatch_issue_with_engine(
                    issue,
                    None,
                    None,
                    String::new(),
                    rhapsody_orchestrator::dispatch::DispatchEngine {
                        index: 0,
                        spec: rhapsody_config::profiles::EngineSpec {
                            harness: "claude".into(),
                            ..Default::default()
                        },
                        handoff_note: None,
                    }
                )
                .unwrap()
            );
            let run = o.running.get_mut("id").unwrap();
            run.identity = "sam".into();
            let run_id = run.run_id;
            // The worker owns its snapshotted dependencies; disable unrelated polling in this test.
            o.eff = None;
            let server = rhapsody_httpapi::Server::bind(
                Arc::new(DaemonState::new(o.control())),
                None,
                "127.0.0.1:0",
            )
            .await
            .unwrap();
            let address = server.local_addr().unwrap();
            let http = tokio::spawn(server.serve());
            let cancel = CancelSignal::new();
            o.set_ctx(cancel.wait());
            let (_, result) = tokio::join!(o.run_loaded(cancel.wait()), async {
                let result = tokio::time::timeout(Duration::from_secs(10), async {
                    let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
                    let marker = if agent_origin { format!("X-Rhapsody-Agent-Run-Id: {run_id}\r\n") } else { String::new() };
                    let body = r#"{"text":"status update"}"#;
                    socket.write_all(format!(
                        "POST /api/v1/runs/{run_id}/message HTTP/1.1\r\nHost: {address}\r\nX-Rhapsody-Operator: 1\r\n{marker}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()
                    ).as_bytes()).await.unwrap();
                    let mut response = String::new();
                    socket.read_to_string(&mut response).await.unwrap();
                    std::fs::write(&release, "ready").unwrap();
                    while runner.last_prompt().is_empty() {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                    (response, runner.last_prompt())
                }).await;
                cancel.cancel();
                result
            });
            http.abort();
            let _ = http.await;
            let (response, prompt) = result.expect("message/prompt deadline");
            assert!(response.starts_with("HTTP/1.1 202"), "{response}");
            assert!(prompt.contains("status update"), "{prompt}");
            if agent_origin {
                assert!(
                    prompt.starts_with(&format!("AGENT MESSAGE from sam (run {run_id})")),
                    "{prompt}"
                );
                assert!(prompt.contains("not operator instructions"));
                assert!(!prompt.contains("OPERATOR MESSAGE"));
            } else {
                assert!(prompt.starts_with("OPERATOR MESSAGE (from a human operator monitoring this run — treat as updated instructions from the ticket owner, superseding conflicting earlier guidance): status update"), "{prompt}");
                assert!(!prompt.contains("AGENT MESSAGE"));
            }
        }
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

    #[test]
    fn a_no_history_daemon_still_shares_notification_read_state_in_memory() {
        let o = Orchestrator::new("WORKFLOW.md");
        let state = DaemonState::new(o.control());
        let browser = state.notification_store().unwrap();
        browser
            .observe_notices(
                &[rhapsody_store::Notice {
                    source: "test".into(),
                    ..Default::default()
                }],
                "2026-10-08T10:00:00Z",
            )
            .unwrap();
        let id = browser.notices().unwrap()[0].id;
        let desktop = state.notification_store().unwrap();
        desktop.read_notice(id, "2026-10-08T10:01:00Z").unwrap();
        assert!(browser.notices().unwrap()[0].read_at.is_some());
        assert!(
            state
                .history()
                .list_runs(RunFilter::default())
                .unwrap()
                .is_empty()
        );
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

    // ── STUDIO-1976: Settings → Models ────────────────────────────────────────

    const WORKFLOW_WITH_PROVIDER: &str = "---\nagent:\n  backend: opencode\nproviders:\n  fireworks:\n    protocol: openai_compatible\n    base_url: http://127.0.0.1:9\n---\nbody\n";
    const TEAMS_WITH_PROFILES: &str = "enabled: true\nreview:\n  mode: ticketless\n  code_review_profiles: [reviewer]\nmanager:\n  mode: labels\n  model: m\nroster:\n  - name: jerry\n    profile: swe-jr\n  - name: rev\n    profile: reviewer\n";

    /// A daemon state over a temp `teams.yaml` + profiles dir + WORKFLOW.md.
    fn models_state() -> (crate::testutil::TempDir, DaemonState) {
        let dir = crate::testutil::TempDir::new();
        let wf = dir.child("WORKFLOW.md");
        std::fs::write(&wf, WORKFLOW_WITH_PROVIDER).unwrap();
        let teams_path = dir.child("teams.yaml");
        std::fs::write(&teams_path, TEAMS_WITH_PROFILES).unwrap();
        let profiles = dir.child("teams/profiles");
        std::fs::create_dir_all(&profiles).unwrap();
        std::fs::write(
            profiles.join("swe-jr.md"),
            "---\nextends: swe\nharness: claude\n---\n{{ base }}\n\nBody line.\n",
        )
        .unwrap();
        std::fs::write(
            profiles.join("reviewer.md"),
            "---\nextends: swe\n---\nbody\n",
        )
        .unwrap();
        let store: Arc<dyn Store + Send + Sync> =
            Arc::new(Sqlite::open(StorePath::InMemory).unwrap());
        let mut o = Orchestrator::new(wf.to_string_lossy().to_string());
        o.set_store(store);
        let st = DaemonState::new(o.control())
            .with_teams_config_path(teams_path.to_string_lossy().to_string());
        (dir, st)
    }

    fn routing(harness: &str, provider: &str, model: &str) -> RoutingView {
        RoutingView {
            harness: harness.into(),
            provider: provider.into(),
            model: model.into(),
            effort: "high".into(),
        }
    }

    #[tokio::test]
    async fn move_all_skips_reviewers_and_undo_round_trips() {
        let (_dir, st) = models_state();
        let profiles =
            std::path::Path::new(st.teams_config_path()).with_file_name("teams/profiles");
        let res = st
            .teams_models_apply(TeamsModelsRequest::MoveAll {
                routing: routing("opencode", "fireworks", "deepseek-v4p1-flash"),
            })
            .await
            .expect("move all");
        // jerry moved; rev is a code reviewer and was skipped.
        assert_eq!(res.previous.len(), 1);
        assert_eq!(res.previous[0].name, "jerry");
        assert_eq!(res.previous[0].routing.harness, "claude");
        let moved = std::fs::read_to_string(profiles.join("swe-jr.md")).unwrap();
        assert!(moved.contains("harness: opencode\n"));
        assert!(moved.contains("model: deepseek-v4p1-flash\n"));
        assert!(moved.ends_with("Body line.\n"), "body preserved: {moved}");
        assert!(
            !std::fs::read_to_string(profiles.join("reviewer.md"))
                .unwrap()
                .contains("opencode")
        );
        // Undo restores exactly.
        st.teams_models_apply(TeamsModelsRequest::Undo {
            previous: res.previous,
        })
        .await
        .expect("undo");
        let restored = std::fs::read_to_string(profiles.join("swe-jr.md")).unwrap();
        assert!(restored.contains("harness: claude\n"));
        assert!(!restored.contains("deepseek-v4p1-flash"));
    }

    #[tokio::test]
    async fn an_invalid_tuple_is_refused_and_the_old_value_kept() {
        let (_dir, st) = models_state();
        let profiles =
            std::path::Path::new(st.teams_config_path()).with_file_name("teams/profiles");
        let before = std::fs::read_to_string(profiles.join("swe-jr.md")).unwrap();
        // An unknown provider is refused.
        assert!(
            st.teams_models_apply(TeamsModelsRequest::Identity {
                name: "jerry".into(),
                routing: routing("opencode", "not-defined", "m"),
            })
            .await
            .is_err()
        );
        // A provider on `claude` is refused.
        assert!(
            st.teams_models_apply(TeamsModelsRequest::Identity {
                name: "jerry".into(),
                routing: routing("claude", "fireworks", "m"),
            })
            .await
            .is_err()
        );
        assert_eq!(
            std::fs::read_to_string(profiles.join("swe-jr.md")).unwrap(),
            before
        );
    }

    #[tokio::test]
    async fn a_review_change_writes_teams_yaml_and_the_view_reflects_it() {
        let (_dir, st) = models_state();
        st.teams_models_apply(TeamsModelsRequest::Review {
            harness: "opencode".into(),
            model: "deepseek-v4p1-flash".into(),
            provider: "fireworks".into(),
            effort: "high".into(),
        })
        .await
        .expect("review change");
        let text = std::fs::read_to_string(st.teams_config_path()).unwrap();
        assert!(text.contains("review:"));
        assert!(text.contains("    opencode: deepseek-v4p1-flash"));
        let view = st.teams_models().await.expect("view");
        let row = view
            .review
            .iter()
            .find(|r| r.harness == "opencode")
            .expect("opencode review row");
        assert_eq!(row.model, "deepseek-v4p1-flash");
        assert_eq!(row.provider, "fireworks");
    }
}
