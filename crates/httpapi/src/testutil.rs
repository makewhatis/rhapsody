//! Shared test scaffolding for the httpapi handler/web tests: a fake [`StateProvider`], a loopback
//! server spawner, and `Snapshot` builders. The Rust analog of `server_test.go`'s `fakeProvider` +
//! `testServer` + `sampleSnapshot` helpers, narrowed to the H1 surface.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use rhapsody_agent::LogEntry;
use rhapsody_config::workflow::Definition;
use rhapsody_config::{decode, resolve, validate};
use rhapsody_core::Project;
use rhapsody_orchestrator::managerread::{ManagerReadError, ManagerReadOutcome};
use rhapsody_orchestrator::prstate::PrCoord;
use rhapsody_orchestrator::reviewconsole::{ReviewControlOutcome, ReviewsView};
use rhapsody_orchestrator::rundiff::DiffOutcome;
use rhapsody_orchestrator::runmerge::{MergeControlOutcome, MergeabilityOutcome};
use rhapsody_orchestrator::{
    HandoffResult, Identity, IssueKey, IssueLifecycleRow, ReadsError, RefreshResult, ResumeResult,
    RetryRow, RunMessageResult, RunningRow, Snapshot, StopResult, TokenCounts, Totals,
};
use rhapsody_provider_status::{CatalogError, CatalogSnapshot, ProviderStatusView};
use rhapsody_store::Noop;

use crate::{ConfigValidateError, HistoryStore, RunActionError, SnapshotError, StateProvider};

/// A canned [`StateProvider`]: a fixed snapshot (or snapshot error) plus a read-only history store.
/// Mirrors Go `fakeProvider` (`server_test.go`), grown across the H-lane exactly as Go grows its one
/// fake. The history store defaults to a [`Noop`] (Go's `fakeProvider.Store()` returns `store.Noop()`
/// when `hist == nil`), so an endpoint that reads history without a seeded store still degrades to `[]`.
pub(crate) struct FakeProvider {
    snap: Snapshot,
    snap_err: Option<String>,
    history: Arc<dyn HistoryStore>,
    /// The canned transcript: `None` ⇒ no such run (→ 404); `Some(entries)` ⇒ found (empty entries is
    /// a found-but-pruned run → 200 `entries:[]`). Mirrors Go's `runEntries`/`runFound` pair.
    transcript: Option<Vec<LogEntry>>,
    /// The canned Linear project list (Go's `fakeProvider.projects`).
    linear_projects: Vec<Project>,
    /// When set, `list_linear_projects` fails with [`ReadsError::ConfigNotLoaded`] (the pre-first-load
    /// 503 path). The tracker-error 502 path needs a real `TrackerError` and, like Go's fake, is left
    /// to F1 integration.
    projects_config_not_loaded: bool,
    /// The canned connected-as identity (Go's `fakeProvider.identity`).
    identity: Identity,
    /// H3 run-action surfaces: canned results, an optional control-round-trip error (the 500 path),
    /// and the recorded run id (interior-mutable so a test holding an `Arc<FakeProvider>` can assert
    /// the handler parsed + forwarded the `{id}`, mirroring Go reading `p.stopRunID`).
    stop_result: StopResult,
    stop_err: Option<String>,
    stop_run_id: AtomicI64,
    resume_result: ResumeResult,
    resume_err: Option<String>,
    resume_run_id: AtomicI64,
    /// H-lane handoff surface (TRA-242): canned result, an optional control-round-trip error (the 500
    /// path), and the recorded run id (so a test can assert the handler parsed + forwarded the `{id}`).
    handoff_result: HandoffResult,
    handoff_err: Option<String>,
    handoff_run_id: AtomicI64,
    /// H3 operator-message surface: canned result + recorded args (Go's `messageResult`/`messageRunID`
    /// /`messageText`). `message_text` records the TRIMMED text the handler forwarded.
    message_result: RunMessageResult,
    message_run_id: AtomicI64,
    message_text: Mutex<String>,
    /// The canned `refresh` result (Go's `fakeProvider.refresh`).
    refresh_result: RefreshResult,
    /// A REAL drain signal rather than a canned answer (STUDIO-880): arming and cancelling are the
    /// behaviour the route tests are about, and a canned status could not show that a `POST` is what
    /// changed the subsequent `GET`.
    drain: rhapsody_orchestrator::drain::DrainSignal,
    /// The WORKFLOW.md path the config endpoints read/write (Go's `fakeProvider.workflowPath`); its
    /// parent dir is the `resolve` base in [`validate_config`].
    workflow_path: String,
    /// The canned agent-capabilities registry `GET /api/v1/capabilities` serves. `None` ⇒ the
    /// handler's empty-registry (`[]`) path.
    capabilities_registry: Option<Vec<rhapsody_config::capabilities::CapabilityDef>>,
    /// The Teams memory runtime the `/api/v1/teams/*` handlers drive (STUDIO-645). Unset ⇒ the
    /// trait's default `teams_disabled`, which is exactly what a Teams-off daemon answers.
    teams_memory: Option<Arc<rhapsody_orchestrator::teamsmemory::TeamsMemory>>,
    /// The `teams.yaml` path `/api/v1/teams/config` reads and writes (STUDIO-652). Empty ⇒ the
    /// no-runtime-home answer a `--no-store` daemon gives.
    teams_config_path: String,
    /// The canned ticket lifecycles the issue listing is decorated with (STUDIO-702), keyed by
    /// tracker issue id. Empty ⇒ the trait default: no answer for anything, which is what a daemon
    /// with no tracker yet reports. `issue_lifecycles_asked` records the ids the handler forwarded,
    /// so a test can assert it asked about exactly the page it served.
    issue_lifecycles: HashMap<String, IssueLifecycleRow>,
    issue_lifecycles_asked: Mutex<Vec<String>>,
    /// The canned durable assignees the issue listing is decorated with (STUDIO-735), keyed by
    /// tracker issue id. `issue_assignees_asked` records the KEYS the handler forwarded, which is
    /// how a test sees that it passed the identifier along as well as the id.
    issue_assignees: HashMap<String, String>,
    issue_assignees_asked: Mutex<Vec<IssueKey>>,
    /// The canned review-ticket markers the issue listing is decorated with (STUDIO-780), as a set
    /// of tracker issue ids. Empty ⇒ the trait default: nothing is a review ticket, which is what a
    /// daemon with no tracker — or one that has only ever seen ordinary tickets — reports.
    review_tickets: HashSet<String>,
    review_tickets_asked: Mutex<Vec<String>>,
    /// The canned `GET /api/v1/reviews` view (STUDIO-722). Unset ⇒ the trait's default, which is a
    /// DORMANT surface — exactly what a daemon with Teams off or the mode not `ticketless` serves.
    reviews: Option<ReviewsView>,
    /// Make `reviews()` fail with a store error — the read's only `Err` path (a broken watch set),
    /// which is a 500 rather than a dormant surface.
    reviews_err: Option<String>,
    /// The canned outcome both review controls return, and the coordinates the last one was called
    /// with, so a test can assert the handler forwarded what the body said (Go's `p.stopRunID`
    /// pattern, for a struct rather than an id).
    review_outcome: Option<ReviewControlOutcome>,
    review_rerun_pr: Mutex<Option<PrCoord>>,
    review_dismiss_pr: Mutex<Option<PrCoord>>,
    /// The `reviewer` the last dismiss carried, so a test can assert the optional per-reviewer
    /// lever reached the provider (STUDIO-1022).
    review_dismiss_reviewer: Mutex<Option<Option<String>>>,
    review_clear_pr: Mutex<Option<PrCoord>>,
    /// The canned outcome `merge_run` returns, and what the last call was asked — how a test
    /// asserts the handler forwarded only the run id and the confirmation (STUDIO-767).
    merge_outcome: Option<MergeControlOutcome>,
    merge_asked: Mutex<Option<(i64, String)>>,
    /// The canned outcome `run_mergeability` returns, and the run id the last read asked about
    /// (STUDIO-790).
    mergeability_outcome: Option<MergeabilityOutcome>,
    mergeability_asked: Mutex<Option<i64>>,
    /// The canned outcome `run_diff` returns, and the run id the last read asked about
    /// (STUDIO-749).
    diff_outcome: Option<DiffOutcome>,
    diff_asked: Mutex<Option<i64>>,
    /// The canned outcome the manager read methods return, taken on first use, and the last manager
    /// call's signature (`"file:7:sha:path"`) (STUDIO-1014).
    manager_outcome: Mutex<Option<ManagerReadOutcome>>,
    manager_asked: Mutex<Option<String>>,
    /// Every [`StateProvider`] call made on this fake, of any kind. The operator-write guard's tests
    /// (STUDIO-982) assert a refused request leaves this at zero, i.e. the guard ran before any
    /// read or side effect.
    calls: AtomicUsize,
    /// The canned non-secret provider status views `GET /api/v1/providers` serves (STUDIO-990).
    provider_statuses: Vec<ProviderStatusView>,
    /// The canned per-provider catalog snapshot `GET /api/v1/providers/{id}/models` serves, when the
    /// id matches. `None` ⇒ the handler's 404.
    provider_catalog: Option<CatalogSnapshot>,
    /// What the explicit refresh POST returns once reached. `Some` ⇒ a `200` snapshot (which is how
    /// a test drives the success path); `None` ⇒ the unknown-provider `404`. The provider id the
    /// handler forwarded is recorded in `provider_refresh_asked`.
    provider_refresh_result: Option<CatalogSnapshot>,
    provider_refresh_asked: Mutex<Option<String>>,
    /// Canned provider references the removal check sees (STUDIO-1048), keyed by provider id.
    provider_references: HashMap<String, Vec<rhapsody_config::ProviderReference>>,
}

impl FakeProvider {
    /// A provider that returns `snap` from every `snapshot()` call, with an empty (Noop) history store.
    pub(crate) fn ok(snap: Snapshot) -> Self {
        Self {
            snap,
            snap_err: None,
            drain: rhapsody_orchestrator::drain::DrainSignal::new(),
            history: Arc::new(Noop),
            transcript: None,
            linear_projects: Vec::new(),
            projects_config_not_loaded: false,
            identity: Identity::default(),
            stop_result: StopResult::default(),
            stop_err: None,
            stop_run_id: AtomicI64::new(0),
            resume_result: ResumeResult::default(),
            resume_err: None,
            resume_run_id: AtomicI64::new(0),
            handoff_result: HandoffResult::default(),
            handoff_err: None,
            handoff_run_id: AtomicI64::new(0),
            message_result: RunMessageResult::default(),
            message_run_id: AtomicI64::new(0),
            message_text: Mutex::new(String::new()),
            // RefreshResult has no `Default` (its `DateTime` field), so build a zero value explicitly.
            refresh_result: RefreshResult {
                queued: false,
                coalesced: false,
                requested_at: epoch(),
                operations: Vec::new(),
            },
            workflow_path: String::new(),
            capabilities_registry: None,
            teams_memory: None,
            teams_config_path: String::new(),
            issue_lifecycles: HashMap::new(),
            issue_lifecycles_asked: Mutex::new(Vec::new()),
            review_tickets: HashSet::new(),
            review_tickets_asked: Mutex::new(Vec::new()),
            issue_assignees: HashMap::new(),
            issue_assignees_asked: Mutex::new(Vec::new()),
            reviews: None,
            reviews_err: None,
            review_outcome: None,
            review_rerun_pr: Mutex::new(None),
            review_dismiss_pr: Mutex::new(None),
            review_dismiss_reviewer: Mutex::new(None),
            review_clear_pr: Mutex::new(None),
            merge_outcome: None,
            merge_asked: Mutex::new(None),
            mergeability_outcome: None,
            mergeability_asked: Mutex::new(None),
            diff_outcome: None,
            diff_asked: Mutex::new(None),
            manager_outcome: Mutex::new(None),
            manager_asked: Mutex::new(None),
            calls: AtomicUsize::new(0),
            provider_statuses: Vec::new(),
            provider_catalog: None,
            provider_refresh_result: None,
            provider_refresh_asked: Mutex::new(None),
            provider_references: HashMap::new(),
        }
    }

    /// A provider whose `snapshot()` fails with `message` (drives the 503 path).
    pub(crate) fn failing(message: &str) -> Self {
        Self {
            snap_err: Some(message.to_string()),
            ..Self::ok(empty_snapshot())
        }
    }

    /// Back the history endpoints with `store` (a real seeded [`rhapsody_store::Sqlite`] or a
    /// [`Noop`]). The Rust analog of Go's `&fakeProvider{hist: st}`.
    /// Canned ticket lifecycles for the issue listing's `lifecycle`/`tracker_state` fields
    /// (STUDIO-702), keyed by tracker issue id. Ids absent from `rows` get no answer.
    pub(crate) fn with_issue_lifecycles(
        mut self,
        rows: HashMap<String, IssueLifecycleRow>,
    ) -> Self {
        self.issue_lifecycles = rows;
        self
    }

    /// Canned durable assignees for the issue listing's `assignee` field (STUDIO-735), keyed by
    /// tracker issue id.
    pub(crate) fn with_issue_assignees(mut self, rows: HashMap<String, String>) -> Self {
        self.issue_assignees = rows;
        self
    }

    /// The issue keys the last `issue_assignees` call forwarded, in order.
    pub(crate) fn issue_assignees_asked(&self) -> Vec<IssueKey> {
        self.issue_assignees_asked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Canned review-ticket markers for the issue listing's `review_ticket` field (STUDIO-780), as
    /// a set of tracker issue ids. Ids absent from `ids` are not review tickets.
    pub(crate) fn with_review_tickets(mut self, ids: HashSet<String>) -> Self {
        self.review_tickets = ids;
        self
    }

    /// The issue ids the last `review_tickets` call forwarded, in order.
    pub(crate) fn review_tickets_asked(&self) -> Vec<String> {
        self.review_tickets_asked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The issue ids the last `issue_lifecycles` call forwarded, in order.
    pub(crate) fn issue_lifecycles_asked(&self) -> Vec<String> {
        self.issue_lifecycles_asked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn with_history(mut self, store: Arc<dyn HistoryStore>) -> Self {
        self.history = store;
        self
    }

    /// Set the canned `run_transcript` result (`None` = not found → 404). The Rust analog of Go's
    /// `&fakeProvider{runFound: …, runEntries: …}`.
    pub(crate) fn with_transcript(mut self, transcript: Option<Vec<LogEntry>>) -> Self {
        self.transcript = transcript;
        self
    }

    /// Set the canned Linear project list (Go's `&fakeProvider{projects: …}`).
    pub(crate) fn with_linear_projects(mut self, projects: Vec<Project>) -> Self {
        self.linear_projects = projects;
        self
    }

    /// Make `list_linear_projects` fail with [`ReadsError::ConfigNotLoaded`] (the 503 path).
    pub(crate) fn with_projects_config_not_loaded(mut self) -> Self {
        self.projects_config_not_loaded = true;
        self
    }

    /// Set the canned connected-as identity (Go's `&fakeProvider{identity: …}`).
    pub(crate) fn with_identity(mut self, identity: Identity) -> Self {
        self.identity = identity;
        self
    }

    /// Set the canned `stop_run` result (Go's `&fakeProvider{stopResult: …}`).
    pub(crate) fn with_stop_result(mut self, result: StopResult) -> Self {
        self.stop_result = result;
        self
    }

    /// Set the canned `resume_run` result (Go's `&fakeProvider{resumeResult: …}`).
    pub(crate) fn with_resume_result(mut self, result: ResumeResult) -> Self {
        self.resume_result = result;
        self
    }

    /// The run id the last `stop_run` was called with (Go's `p.stopRunID`).
    pub(crate) fn stop_run_id(&self) -> i64 {
        self.stop_run_id.load(Ordering::SeqCst)
    }

    /// The run id the last `resume_run` was called with (Go's `p.resumeRunID`).
    pub(crate) fn resume_run_id(&self) -> i64 {
        self.resume_run_id.load(Ordering::SeqCst)
    }

    /// Set the canned `handoff_run` result (TRA-242).
    pub(crate) fn with_handoff_result(mut self, result: HandoffResult) -> Self {
        self.handoff_result = result;
        self
    }

    /// The run id the last `handoff_run` was called with (TRA-242).
    pub(crate) fn handoff_run_id(&self) -> i64 {
        self.handoff_run_id.load(Ordering::SeqCst)
    }

    /// Set the canned `send_run_message` result (Go's `&fakeProvider{messageResult: …}`).
    pub(crate) fn with_message_result(mut self, result: RunMessageResult) -> Self {
        self.message_result = result;
        self
    }

    /// The run id the last `send_run_message` was called with (Go's `p.messageRunID`).
    pub(crate) fn message_run_id(&self) -> i64 {
        self.message_run_id.load(Ordering::SeqCst)
    }

    /// The (trimmed) text the last `send_run_message` was called with (Go's `p.messageText`).
    pub(crate) fn message_text(&self) -> String {
        self.message_text.lock().expect("message_text lock").clone()
    }

    /// Set the canned `refresh` result (Go's `&fakeProvider{refresh: …}`).
    pub(crate) fn with_refresh_result(mut self, result: RefreshResult) -> Self {
        self.refresh_result = result;
        self
    }

    /// Set the WORKFLOW.md path the config endpoints read/write (Go's `&fakeProvider{workflowPath:…}`).
    pub(crate) fn with_workflow_path(mut self, path: impl Into<String>) -> Self {
        self.workflow_path = path.into();
        self
    }

    /// Set the canned capabilities registry `GET /api/v1/capabilities` serves (unset ⇒ the `[]` path).
    pub(crate) fn with_capabilities_registry(
        mut self,
        registry: Vec<rhapsody_config::capabilities::CapabilityDef>,
    ) -> Self {
        self.capabilities_registry = Some(registry);
        self
    }

    /// Give the provider a REAL Teams memory runtime, so the `/api/v1/teams/*` handler tests drive
    /// the actual backend over a temp bank rather than a canned result (STUDIO-645).
    pub(crate) fn with_teams_memory(
        mut self,
        mem: Arc<rhapsody_orchestrator::teamsmemory::TeamsMemory>,
    ) -> Self {
        self.teams_memory = Some(mem);
        self
    }

    /// Set the `teams.yaml` path `GET`/`POST /api/v1/teams/config` reads and writes (STUDIO-652).
    /// Unset ⇒ the no-runtime-home path, which is what a `--no-store` daemon serves.
    pub(crate) fn with_teams_config_path(mut self, path: impl Into<String>) -> Self {
        self.teams_config_path = path.into();
        self
    }

    /// Set the canned `GET /api/v1/reviews` view (STUDIO-722). Unset ⇒ the dormant surface.
    pub(crate) fn with_reviews(mut self, view: ReviewsView) -> Self {
        self.reviews = Some(view);
        self
    }

    /// Make `GET /api/v1/reviews` fail with a store error (the 500 path).
    pub(crate) fn with_reviews_error(mut self, message: &str) -> Self {
        self.reviews_err = Some(message.to_string());
        self
    }

    /// Set the canned outcome EVERY review control returns. Unset ⇒ the trait's `Dormant`.
    pub(crate) fn with_review_outcome(mut self, outcome: ReviewControlOutcome) -> Self {
        self.review_outcome = Some(outcome);
        self
    }

    /// The coordinates the last `review_rerun` / `review_dismiss` / `review_clear` was called with —
    /// how a test asserts the handler forwarded the body's own owner/repo/number and nothing else.
    pub(crate) fn review_rerun_pr(&self) -> Option<PrCoord> {
        self.review_rerun_pr
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn review_dismiss_pr(&self) -> Option<PrCoord> {
        self.review_dismiss_pr
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The `reviewer` the last dismiss carried: `None` when dismiss was never called,
    /// `Some(None)` for a whole-pull-request dismissal, `Some(Some(name))` for a named one.
    pub(crate) fn review_dismiss_reviewer(&self) -> Option<Option<String>> {
        self.review_dismiss_reviewer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn review_clear_pr(&self) -> Option<PrCoord> {
        self.review_clear_pr
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Set the canned outcome `merge_run` returns. Unset ⇒ the trait's `Dormant`.
    pub(crate) fn with_merge_outcome(mut self, outcome: MergeControlOutcome) -> Self {
        self.merge_outcome = Some(outcome);
        self
    }

    /// The `(run id, confirmation)` the last `merge_run` was called with.
    pub(crate) fn merge_asked(&self) -> Option<(i64, String)> {
        self.merge_asked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Set the canned outcome `run_mergeability` returns. Unset ⇒ the trait's `Dormant`.
    pub(crate) fn with_mergeability(mut self, outcome: MergeabilityOutcome) -> Self {
        self.mergeability_outcome = Some(outcome);
        self
    }

    /// The run id the last `run_mergeability` read asked about.
    pub(crate) fn mergeability_asked(&self) -> Option<i64> {
        *self
            .mergeability_asked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Set the canned outcome `run_diff` returns. Unset ⇒ the trait's `Unavailable`.
    pub(crate) fn with_diff(mut self, outcome: DiffOutcome) -> Self {
        self.diff_outcome = Some(outcome);
        self
    }

    /// Canned non-secret provider status views `GET /api/v1/providers` serves (STUDIO-990).
    pub(crate) fn with_provider_statuses(mut self, views: Vec<ProviderStatusView>) -> Self {
        self.provider_statuses = views;
        self
    }

    /// Canned catalog snapshot `GET /api/v1/providers/{id}/models` serves for that id (STUDIO-990).
    pub(crate) fn with_provider_catalog(mut self, snapshot: CatalogSnapshot) -> Self {
        self.provider_catalog = Some(snapshot);
        self
    }

    /// Canned success snapshot the explicit refresh POST returns (STUDIO-990). Unset ⇒ the
    /// unknown-provider `404`.
    pub(crate) fn with_provider_refresh_result(mut self, snapshot: CatalogSnapshot) -> Self {
        self.provider_refresh_result = Some(snapshot);
        self
    }

    /// Canned references the provider-removal check sees for `id` (STUDIO-1048).
    pub(crate) fn with_provider_references(
        mut self,
        id: &str,
        references: Vec<rhapsody_config::ProviderReference>,
    ) -> Self {
        self.provider_references.insert(id.to_string(), references);
        self
    }

    /// The provider id the last refresh POST forwarded, or `None` if it was never called — the
    /// proof that a GET never triggers a refresh.
    pub(crate) fn provider_refresh_asked(&self) -> Option<String> {
        self.provider_refresh_asked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// How many [`StateProvider`] calls this fake has served.
    pub(crate) fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn touch(&self) {
        self.calls.fetch_add(1, Ordering::SeqCst);
    }

    /// The run id the last `run_diff` read asked about — `None` proves the daemon was never asked.
    pub(crate) fn diff_asked(&self) -> Option<i64> {
        *self
            .diff_asked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Set the canned outcome every manager read method returns (taken on first use; unset ⇒ the
    /// trait's `Unavailable`). STUDIO-1014.
    pub(crate) fn with_manager_outcome(mut self, outcome: ManagerReadOutcome) -> Self {
        self.manager_outcome = Mutex::new(Some(outcome));
        self
    }

    /// The last manager call's signature, e.g. `"diff:7:aaa:bbb"` — `None` proves no manager read
    /// reached the provider. STUDIO-1014.
    pub(crate) fn manager_asked(&self) -> Option<String> {
        self.manager_asked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn take_manager(&self, sig: String) -> ManagerReadOutcome {
        *self
            .manager_asked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(sig);
        self.manager_outcome
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .unwrap_or(Err(ManagerReadError::Unavailable(
                "this daemon cannot serve manager reads",
            )))
    }
}

#[async_trait]
impl StateProvider for FakeProvider {
    async fn snapshot(&self) -> Result<Snapshot, SnapshotError> {
        self.touch();
        match &self.snap_err {
            Some(message) => Err(SnapshotError::new(message.clone())),
            None => Ok(self.snap.clone()),
        }
    }

    async fn issue_lifecycles(&self, ids: &[String]) -> HashMap<String, IssueLifecycleRow> {
        self.touch();
        *self
            .issue_lifecycles_asked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = ids.to_vec();
        ids.iter()
            .filter_map(|id| {
                self.issue_lifecycles
                    .get(id)
                    .map(|r| (id.clone(), r.clone()))
            })
            .collect()
    }

    async fn review_tickets(&self, ids: &[String]) -> HashSet<String> {
        self.touch();
        *self
            .review_tickets_asked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = ids.to_vec();
        ids.iter()
            .filter(|id| self.review_tickets.contains(*id))
            .cloned()
            .collect()
    }

    async fn issue_assignees(&self, keys: &[IssueKey]) -> HashMap<String, String> {
        self.touch();
        *self
            .issue_assignees_asked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = keys.to_vec();
        keys.iter()
            .filter_map(|k| {
                self.issue_assignees
                    .get(&k.id)
                    .map(|name| (k.id.clone(), name.clone()))
            })
            .collect()
    }

    fn history(&self) -> Arc<dyn HistoryStore> {
        self.touch();
        self.history.clone()
    }

    fn run_transcript(&self, _run_id: i64) -> Option<Vec<LogEntry>> {
        self.touch();
        self.transcript.clone()
    }

    async fn list_linear_projects(&self) -> Result<Vec<Project>, ReadsError> {
        self.touch();
        if self.projects_config_not_loaded {
            return Err(ReadsError::ConfigNotLoaded);
        }
        Ok(self.linear_projects.clone())
    }

    async fn connected_viewer(&self) -> (Identity, Option<String>) {
        self.touch();
        // The resolution-error (Option) is only logged by the handler; no mirrored test exercises it
        // (Go's linear_test.go leaves `identityErr` unset), so the fake never surfaces one.
        (self.identity.clone(), None)
    }

    async fn stop_run(&self, run_id: i64) -> Result<StopResult, RunActionError> {
        self.touch();
        self.stop_run_id.store(run_id, Ordering::SeqCst);
        match &self.stop_err {
            Some(message) => Err(RunActionError::new(message.clone())),
            None => Ok(self.stop_result.clone()),
        }
    }

    async fn resume_run(&self, run_id: i64) -> Result<ResumeResult, RunActionError> {
        self.touch();
        self.resume_run_id.store(run_id, Ordering::SeqCst);
        match &self.resume_err {
            Some(message) => Err(RunActionError::new(message.clone())),
            None => Ok(self.resume_result.clone()),
        }
    }

    async fn handoff_run(&self, run_id: i64) -> Result<HandoffResult, RunActionError> {
        self.touch();
        self.handoff_run_id.store(run_id, Ordering::SeqCst);
        match &self.handoff_err {
            Some(message) => Err(RunActionError::new(message.clone())),
            None => Ok(self.handoff_result.clone()),
        }
    }

    async fn send_run_message(&self, run_id: i64, text: &str) -> RunMessageResult {
        self.touch();
        self.message_run_id.store(run_id, Ordering::SeqCst);
        *self.message_text.lock().expect("message_text lock") = text.to_string();
        self.message_result.clone()
    }

    fn refresh(&self) -> RefreshResult {
        self.touch();
        self.refresh_result.clone()
    }

    fn drain_status(&self) -> rhapsody_orchestrator::drain::DrainStatus {
        self.touch();
        self.drain.status()
    }

    fn set_drain(
        &self,
        active: bool,
        reason: rhapsody_orchestrator::drain::DrainReason,
    ) -> rhapsody_orchestrator::drain::DrainStatus {
        self.touch();
        if active {
            self.drain.arm(chrono::Utc::now(), reason);
        } else {
            self.drain.disarm();
        }
        self.drain.status()
    }

    fn workflow_path(&self) -> &str {
        self.touch();
        &self.workflow_path
    }

    fn validate_config(&self, def: &Definition) -> Result<(), ConfigValidateError> {
        self.touch();
        // Mirror the Go fake's ValidateConfig: Decode → Resolve → ValidateDispatch (the real
        // orchestrator additionally runs buildEffective; that extra gate is covered by the
        // orchestrator crate's own validate_config test). `resolve` bases relative paths on the
        // WORKFLOW.md's dir, exactly like Go's `filepath.Dir(f.workflowPath)`.
        let cfg = decode(def).map_err(|e| ConfigValidateError::Other(e.to_string()))?;
        let dir = Path::new(&self.workflow_path)
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut cfg = resolve(cfg, &dir).map_err(|e| ConfigValidateError::Other(e.to_string()))?;
        validate(&mut cfg).map_err(ConfigValidateError::Validation)?;
        Ok(())
    }

    async fn teams_room(
        &self,
        limit: usize,
    ) -> Result<
        rhapsody_orchestrator::teamsmemory::RoomView,
        rhapsody_orchestrator::teamsmemory::TeamsMemoryError,
    > {
        self.touch();
        self.teams()?.room(limit)
    }

    async fn teams_roster(
        &self,
    ) -> Result<
        rhapsody_orchestrator::teamsmemory::RosterView,
        rhapsody_orchestrator::teamsmemory::TeamsMemoryError,
    > {
        self.touch();
        self.teams()?.roster()
    }

    async fn teams_overview(
        &self,
    ) -> Result<
        rhapsody_orchestrator::teamsmemory::TeamsView,
        rhapsody_orchestrator::teamsmemory::TeamsMemoryError,
    > {
        self.touch();
        self.teams()?.overview()
    }

    fn teams_enabled(&self) -> bool {
        self.touch();
        self.teams_memory.as_ref().is_some_and(|m| m.enabled())
    }

    fn teams_config_path(&self) -> &str {
        self.touch();
        &self.teams_config_path
    }

    async fn reviews(&self) -> Result<ReviewsView, rhapsody_store::StoreError> {
        self.touch();
        match &self.reviews_err {
            Some(message) => Err(rhapsody_store::StoreError::Io(std::io::Error::other(
                message.clone(),
            ))),
            None => Ok(self.reviews.clone().unwrap_or_default()),
        }
    }

    async fn review_rerun(&self, pr: PrCoord) -> ReviewControlOutcome {
        self.touch();
        *self
            .review_rerun_pr
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(pr);
        self.review_outcome
            .clone()
            .unwrap_or(ReviewControlOutcome::Dormant)
    }

    async fn review_dismiss(&self, pr: PrCoord, reviewer: Option<String>) -> ReviewControlOutcome {
        self.touch();
        *self
            .review_dismiss_pr
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(pr);
        *self
            .review_dismiss_reviewer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(reviewer);
        self.review_outcome
            .clone()
            .unwrap_or(ReviewControlOutcome::Dormant)
    }

    async fn review_clear(&self, pr: PrCoord) -> ReviewControlOutcome {
        self.touch();
        *self
            .review_clear_pr
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(pr);
        self.review_outcome
            .clone()
            .unwrap_or(ReviewControlOutcome::Dormant)
    }

    async fn merge_run(&self, run_id: i64, confirm: &str) -> MergeControlOutcome {
        self.touch();
        *self
            .merge_asked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some((run_id, confirm.to_string()));
        self.merge_outcome
            .clone()
            .unwrap_or(MergeControlOutcome::Dormant)
    }

    async fn run_mergeability(&self, run_id: i64) -> MergeabilityOutcome {
        self.touch();
        *self
            .mergeability_asked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(run_id);
        self.mergeability_outcome
            .clone()
            .unwrap_or(MergeabilityOutcome::Dormant)
    }

    async fn run_diff(&self, run_id: i64) -> DiffOutcome {
        self.touch();
        *self
            .diff_asked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(run_id);
        self.diff_outcome
            .clone()
            .unwrap_or(DiffOutcome::Unavailable("this daemon cannot read a diff"))
    }

    // The manager read methods (STUDIO-1014): record the call signature and return the canned
    // outcome — one shape for every `manager_*` route.
    async fn manager_file(&self, run_id: i64, sha: String, path: String) -> ManagerReadOutcome {
        self.touch();
        self.take_manager(format!("file:{run_id}:{sha}:{path}"))
    }

    async fn manager_ls(&self, run_id: i64, sha: String, path: String) -> ManagerReadOutcome {
        self.touch();
        self.take_manager(format!("ls:{run_id}:{sha}:{path}"))
    }

    async fn manager_grep(
        &self,
        run_id: i64,
        sha: String,
        pattern: String,
        path: String,
    ) -> ManagerReadOutcome {
        self.touch();
        self.take_manager(format!("grep:{run_id}:{sha}:{pattern}:{path}"))
    }

    async fn manager_diff(&self, run_id: i64, from: String, to: String) -> ManagerReadOutcome {
        self.touch();
        self.take_manager(format!("diff:{run_id}:{from}:{to}"))
    }

    async fn manager_interdiff(&self, run_id: i64, from: String, to: String) -> ManagerReadOutcome {
        self.touch();
        self.take_manager(format!("interdiff:{run_id}:{from}:{to}"))
    }

    async fn manager_patch_id(&self, run_id: i64, sha: String) -> ManagerReadOutcome {
        self.touch();
        self.take_manager(format!("patch-id:{run_id}:{sha}"))
    }

    async fn manager_findings(&self, run_id: i64) -> ManagerReadOutcome {
        self.touch();
        self.take_manager(format!("findings:{run_id}"))
    }

    async fn manager_pr(&self, run_id: i64) -> ManagerReadOutcome {
        self.touch();
        self.take_manager(format!("pr:{run_id}"))
    }

    async fn manager_pr_activity(&self, run_id: i64, since: String) -> ManagerReadOutcome {
        self.touch();
        self.take_manager(format!("pr-activity:{run_id}:{since}"))
    }

    async fn manager_pr_commits(&self, run_id: i64, since: String) -> ManagerReadOutcome {
        self.touch();
        self.take_manager(format!("pr-commits:{run_id}:{since}"))
    }

    async fn teams_recall(
        &self,
        identity: &str,
        query: &str,
        state: &str,
    ) -> Result<
        rhapsody_orchestrator::teamsmemory::RecallView,
        rhapsody_orchestrator::teamsmemory::TeamsMemoryError,
    > {
        self.touch();
        self.teams()?.recall(identity, query, state).await
    }

    async fn teams_recall_team(
        &self,
        query: &str,
        state: &str,
    ) -> Result<
        rhapsody_orchestrator::teamsmemory::RecallView,
        rhapsody_orchestrator::teamsmemory::TeamsMemoryError,
    > {
        self.touch();
        self.teams()?.recall_team(query, state).await
    }

    async fn teams_invalidate(
        &self,
        identity: &str,
        fact_id: &str,
        reason: &str,
    ) -> Result<
        rhapsody_orchestrator::teamsmemory::InvalidateView,
        rhapsody_orchestrator::teamsmemory::TeamsMemoryError,
    > {
        self.touch();
        self.teams()?.invalidate(identity, fact_id, reason).await
    }

    async fn teams_invalidate_team(
        &self,
        fact_id: &str,
        reason: &str,
    ) -> Result<
        rhapsody_orchestrator::teamsmemory::InvalidateView,
        rhapsody_orchestrator::teamsmemory::TeamsMemoryError,
    > {
        self.touch();
        self.teams()?.invalidate_team(fact_id, reason).await
    }

    async fn teams_reinstate(
        &self,
        identity: &str,
        fact_id: &str,
    ) -> Result<
        rhapsody_orchestrator::teamsmemory::ReinstateView,
        rhapsody_orchestrator::teamsmemory::TeamsMemoryError,
    > {
        self.touch();
        self.teams()?.reinstate(identity, fact_id).await
    }

    async fn teams_reinstate_team(
        &self,
        fact_id: &str,
    ) -> Result<
        rhapsody_orchestrator::teamsmemory::ReinstateView,
        rhapsody_orchestrator::teamsmemory::TeamsMemoryError,
    > {
        self.touch();
        self.teams()?.reinstate_team(fact_id).await
    }

    async fn teams_retain(
        &self,
        run_id: i64,
        content: &str,
    ) -> Result<
        rhapsody_orchestrator::teamsmemory::RetainView,
        rhapsody_orchestrator::teamsmemory::TeamsMemoryError,
    > {
        self.touch();
        self.teams()?
            .retain_for_run(run_id, content, fixed_instant())
            .await
    }

    async fn teams_retain_shared(
        &self,
        run_id: i64,
        content: &str,
    ) -> Result<
        rhapsody_orchestrator::teamsmemory::RetainView,
        rhapsody_orchestrator::teamsmemory::TeamsMemoryError,
    > {
        self.touch();
        self.teams()?
            .retain_for_run_scoped(run_id, content, true, fixed_instant())
            .await
    }

    /// The room's write side (STUDIO-653, T6). The ROOM half only: the timeline row and the
    /// direct-to-live delivery need the control task's `running` / `mailboxes`, which no fake
    /// provider has — those are exercised in `rhapsody-orchestrator`'s `teamspost` tests against a
    /// real orchestrator. What this proves is what the HTTP boundary owns: the host-stamped `from`,
    /// roster validation, and the wire envelope.
    async fn teams_post(
        &self,
        run_id: i64,
        body: &str,
        to: &str,
        refs: &[String],
    ) -> Result<
        rhapsody_orchestrator::teamsmemory::PostView,
        rhapsody_orchestrator::teamsmemory::TeamsMemoryError,
    > {
        self.touch();
        self.teams()?
            .post_for_run(run_id, body, to, refs, fixed_instant())
    }

    /// The room's human door (STUDIO-661). Nothing here needs the control task at all — there is
    /// no run, so no timeline row and no delivery — which is why the fake provider can exercise
    /// the whole operation rather than only its room half.
    async fn teams_room_post(
        &self,
        body: &str,
        refs: &[String],
    ) -> Result<
        rhapsody_orchestrator::teamsmemory::PostView,
        rhapsody_orchestrator::teamsmemory::TeamsMemoryError,
    > {
        self.touch();
        self.teams()?.post_as_operator(body, refs, fixed_instant())
    }

    fn capabilities_registry(&self) -> Option<Vec<rhapsody_config::capabilities::CapabilityDef>> {
        self.touch();
        self.capabilities_registry.clone()
    }

    fn provider_statuses(&self) -> Vec<ProviderStatusView> {
        self.touch();
        self.provider_statuses.clone()
    }

    fn provider_status(&self, provider_id: &str) -> Option<ProviderStatusView> {
        self.touch();
        self.provider_statuses
            .iter()
            .find(|view| view.provider_id == provider_id)
            .cloned()
    }

    fn provider_catalog(&self, provider_id: &str) -> Option<CatalogSnapshot> {
        self.touch();
        self.provider_catalog
            .as_ref()
            .filter(|snapshot| snapshot.provider_id == provider_id)
            .cloned()
    }

    async fn refresh_provider_catalog(
        &self,
        provider_id: &str,
    ) -> Result<CatalogSnapshot, CatalogError> {
        self.touch();
        *self
            .provider_refresh_asked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(provider_id.to_string());
        match &self.provider_refresh_result {
            Some(snapshot) => Ok(snapshot.clone()),
            None => Err(CatalogError::Unsupported),
        }
    }

    fn provider_references(&self, provider_id: &str) -> Vec<rhapsody_config::ProviderReference> {
        self.touch();
        self.provider_references
            .get(provider_id)
            .cloned()
            .unwrap_or_default()
    }
}

/// Bind a loopback listener on an ephemeral port, serve `router` on a background task, and return the
/// base URL. Mirrors Go's `httptest.NewServer(NewHandler(...))`; the listener is bound before serving
/// so a request issued immediately never races startup.
impl FakeProvider {
    /// The injected Teams runtime, or the Teams-off answer.
    fn teams(
        &self,
    ) -> Result<
        &Arc<rhapsody_orchestrator::teamsmemory::TeamsMemory>,
        rhapsody_orchestrator::teamsmemory::TeamsMemoryError,
    > {
        self.teams_memory
            .as_ref()
            .ok_or(rhapsody_orchestrator::teamsmemory::TeamsMemoryError::Disabled)
    }
}

/// A client whose every request carries `X-Rhapsody-Operator: 1` — the operator's own client, as
/// the dashboard, `rhapsodyd mcp` and the desktop proxy are (STUDIO-982). Handler tests that exercise
/// a write's behaviour use it so the request gets past the operator-write guard; the guard's own
/// tests in `operator_guard` cover the requests that must not.
pub(crate) fn operator_client() -> reqwest::Client {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        crate::operator_guard::OPERATOR_HEADER,
        reqwest::header::HeaderValue::from_static(crate::operator_guard::OPERATOR_HEADER_VALUE),
    );
    reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .expect("build operator client")
}

pub(crate) async fn spawn_router(router: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let addr = listener.local_addr().expect("resolve bound address");
    tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<crate::operator_guard::BoundAddr>(),
        )
        .await
        .expect("serve");
    });
    format!("http://{addr}")
}

/// The Unix epoch — the zero `DateTime<Utc>` the render treats as "unset" (renders `""`), the analog
/// of Go's `time.Time{}`.
pub(crate) fn epoch() -> DateTime<Utc> {
    DateTime::from_timestamp(0, 0).expect("unix epoch")
}

/// A fixed non-zero instant for deterministic timestamp rendering (any wall time works; the fixtures
/// normalize timestamps to `<TIMESTAMP>`). Matches the orchestrator snapshot tests' fixed instant.
pub(crate) fn fixed_instant() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 5, 28, 12, 0, 0)
        .single()
        .expect("valid fixed instant")
}

/// The empty (never-published) snapshot — zero time, empty lists. Mirrors Go `orchestrator.Snapshot{}`.
pub(crate) fn empty_snapshot() -> Snapshot {
    Snapshot {
        generated_at: epoch(),
        running: Vec::new(),
        retrying: Vec::new(),
        totals: Totals::default(),
        rate_limits: Vec::new(),
        drain: None,
        projects: Vec::new(),
        review_divergence: Vec::new(),
        held_for_human: Vec::new(),
        budget_held: Vec::new(),
        notifications: Vec::new(),
    }
}

/// A `RunningRow` with every field defaulted (zero times, empty strings) but the identifier set —
/// tests override just the fields they assert.
pub(crate) fn running_row(issue_identifier: &str) -> RunningRow {
    RunningRow {
        issue_id: String::new(),
        issue_identifier: issue_identifier.to_string(),
        title: String::new(),
        state: String::new(),
        session_id: String::new(),
        turn_count: 0,
        last_event: String::new(),
        last_message: String::new(),
        started_at: epoch(),
        last_event_at: epoch(),
        workspace_path: String::new(),
        tokens: TokenCounts::default(),
        usage_estimated: false,
        recent_events: Vec::new(),
        transcript_path: String::new(),
        run_id: 0,
        attempt: 0,
        project: String::new(),
        repo: String::new(),
    }
}

/// A `RetryRow` with every field defaulted but the identifier set.
pub(crate) fn retry_row(issue_identifier: &str) -> RetryRow {
    RetryRow {
        issue_id: String::new(),
        issue_identifier: issue_identifier.to_string(),
        attempt: 0,
        due_at: epoch(),
        error: String::new(),
        workspace_path: String::new(),
        transcript_path: String::new(),
        project: String::new(),
        repo: String::new(),
    }
}
