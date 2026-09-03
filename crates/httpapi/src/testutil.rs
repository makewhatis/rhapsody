//! Shared test scaffolding for the httpapi handler/web tests: a fake [`StateProvider`], a loopback
//! server spawner, and `Snapshot` builders. The Rust analog of `server_test.go`'s `fakeProvider` +
//! `testServer` + `sampleSnapshot` helpers, narrowed to the H1 surface.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use rhapsody_agent::LogEntry;
use rhapsody_config::workflow::Definition;
use rhapsody_config::{decode, resolve, validate};
use rhapsody_core::Project;
use rhapsody_orchestrator::prstate::PrCoord;
use rhapsody_orchestrator::reviewconsole::{ReviewControlOutcome, ReviewsView};
use rhapsody_orchestrator::{
    HandoffResult, Identity, IssueKey, IssueLifecycleRow, ReadsError, RefreshResult, ResumeResult,
    RetryRow, RunMessageResult, RunningRow, Snapshot, StopResult, TokenCounts, Totals,
};
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
}

impl FakeProvider {
    /// A provider that returns `snap` from every `snapshot()` call, with an empty (Noop) history store.
    pub(crate) fn ok(snap: Snapshot) -> Self {
        Self {
            snap,
            snap_err: None,
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
            issue_assignees: HashMap::new(),
            issue_assignees_asked: Mutex::new(Vec::new()),
            reviews: None,
            reviews_err: None,
            review_outcome: None,
            review_rerun_pr: Mutex::new(None),
            review_dismiss_pr: Mutex::new(None),
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

    /// Set the canned outcome BOTH review controls return. Unset ⇒ the trait's `Dormant`.
    pub(crate) fn with_review_outcome(mut self, outcome: ReviewControlOutcome) -> Self {
        self.review_outcome = Some(outcome);
        self
    }

    /// The coordinates the last `review_rerun` / `review_dismiss` was called with — how a test
    /// asserts the handler forwarded the body's own owner/repo/number and nothing else.
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
}

#[async_trait]
impl StateProvider for FakeProvider {
    async fn snapshot(&self) -> Result<Snapshot, SnapshotError> {
        match &self.snap_err {
            Some(message) => Err(SnapshotError::new(message.clone())),
            None => Ok(self.snap.clone()),
        }
    }

    async fn issue_lifecycles(&self, ids: &[String]) -> HashMap<String, IssueLifecycleRow> {
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

    async fn issue_assignees(&self, keys: &[IssueKey]) -> HashMap<String, String> {
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
        self.history.clone()
    }

    fn run_transcript(&self, _run_id: i64) -> Option<Vec<LogEntry>> {
        self.transcript.clone()
    }

    async fn list_linear_projects(&self) -> Result<Vec<Project>, ReadsError> {
        if self.projects_config_not_loaded {
            return Err(ReadsError::ConfigNotLoaded);
        }
        Ok(self.linear_projects.clone())
    }

    async fn connected_viewer(&self) -> (Identity, Option<String>) {
        // The resolution-error (Option) is only logged by the handler; no mirrored test exercises it
        // (Go's linear_test.go leaves `identityErr` unset), so the fake never surfaces one.
        (self.identity.clone(), None)
    }

    async fn stop_run(&self, run_id: i64) -> Result<StopResult, RunActionError> {
        self.stop_run_id.store(run_id, Ordering::SeqCst);
        match &self.stop_err {
            Some(message) => Err(RunActionError::new(message.clone())),
            None => Ok(self.stop_result.clone()),
        }
    }

    async fn resume_run(&self, run_id: i64) -> Result<ResumeResult, RunActionError> {
        self.resume_run_id.store(run_id, Ordering::SeqCst);
        match &self.resume_err {
            Some(message) => Err(RunActionError::new(message.clone())),
            None => Ok(self.resume_result.clone()),
        }
    }

    async fn handoff_run(&self, run_id: i64) -> Result<HandoffResult, RunActionError> {
        self.handoff_run_id.store(run_id, Ordering::SeqCst);
        match &self.handoff_err {
            Some(message) => Err(RunActionError::new(message.clone())),
            None => Ok(self.handoff_result.clone()),
        }
    }

    async fn send_run_message(&self, run_id: i64, text: &str) -> RunMessageResult {
        self.message_run_id.store(run_id, Ordering::SeqCst);
        *self.message_text.lock().expect("message_text lock") = text.to_string();
        self.message_result.clone()
    }

    fn refresh(&self) -> RefreshResult {
        self.refresh_result.clone()
    }

    fn workflow_path(&self) -> &str {
        &self.workflow_path
    }

    fn validate_config(&self, def: &Definition) -> Result<(), ConfigValidateError> {
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
        self.teams()?.room(limit)
    }

    async fn teams_roster(
        &self,
    ) -> Result<
        rhapsody_orchestrator::teamsmemory::RosterView,
        rhapsody_orchestrator::teamsmemory::TeamsMemoryError,
    > {
        self.teams()?.roster()
    }

    async fn teams_overview(
        &self,
    ) -> Result<
        rhapsody_orchestrator::teamsmemory::TeamsView,
        rhapsody_orchestrator::teamsmemory::TeamsMemoryError,
    > {
        self.teams()?.overview()
    }

    fn teams_enabled(&self) -> bool {
        self.teams_memory.as_ref().is_some_and(|m| m.enabled())
    }

    fn teams_config_path(&self) -> &str {
        &self.teams_config_path
    }

    async fn reviews(&self) -> Result<ReviewsView, rhapsody_store::StoreError> {
        match &self.reviews_err {
            Some(message) => Err(rhapsody_store::StoreError::Io(std::io::Error::other(
                message.clone(),
            ))),
            None => Ok(self.reviews.clone().unwrap_or_default()),
        }
    }

    async fn review_rerun(&self, pr: PrCoord) -> ReviewControlOutcome {
        *self
            .review_rerun_pr
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(pr);
        self.review_outcome
            .clone()
            .unwrap_or(ReviewControlOutcome::Dormant)
    }

    async fn review_dismiss(&self, pr: PrCoord) -> ReviewControlOutcome {
        *self
            .review_dismiss_pr
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(pr);
        self.review_outcome
            .clone()
            .unwrap_or(ReviewControlOutcome::Dormant)
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
        self.teams()?.recall(identity, query, state).await
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
        self.teams()?.invalidate(identity, fact_id, reason).await
    }

    async fn teams_reinstate(
        &self,
        identity: &str,
        fact_id: &str,
    ) -> Result<
        rhapsody_orchestrator::teamsmemory::ReinstateView,
        rhapsody_orchestrator::teamsmemory::TeamsMemoryError,
    > {
        self.teams()?.reinstate(identity, fact_id).await
    }

    async fn teams_retain(
        &self,
        run_id: i64,
        content: &str,
    ) -> Result<
        rhapsody_orchestrator::teamsmemory::RetainView,
        rhapsody_orchestrator::teamsmemory::TeamsMemoryError,
    > {
        self.teams()?
            .retain_for_run(run_id, content, fixed_instant())
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
        self.teams()?.post_as_operator(body, refs, fixed_instant())
    }

    fn capabilities_registry(&self) -> Option<Vec<rhapsody_config::capabilities::CapabilityDef>> {
        self.capabilities_registry.clone()
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

pub(crate) async fn spawn_router(router: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let addr = listener.local_addr().expect("resolve bound address");
    tokio::spawn(async move {
        axum::serve(listener, router).await.expect("serve");
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
        projects: Vec::new(),
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
