//! Shared test scaffolding for the httpapi handler/web tests: a fake [`StateProvider`], a loopback
//! server spawner, and `Snapshot` builders. The Rust analog of `server_test.go`'s `fakeProvider` +
//! `testServer` + `sampleSnapshot` helpers, narrowed to the H1 surface.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use rhapsody_agent::LogEntry;
use rhapsody_core::Project;
use rhapsody_orchestrator::{
    Identity, ReadsError, RetryRow, RunningRow, Snapshot, TokenCounts, Totals,
};
use rhapsody_store::Noop;

use crate::{HistoryStore, SnapshotError, StateProvider};

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
}

#[async_trait]
impl StateProvider for FakeProvider {
    async fn snapshot(&self) -> Result<Snapshot, SnapshotError> {
        match &self.snap_err {
            Some(message) => Err(SnapshotError::new(message.clone())),
            None => Ok(self.snap.clone()),
        }
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
}

/// Bind a loopback listener on an ephemeral port, serve `router` on a background task, and return the
/// base URL. Mirrors Go's `httptest.NewServer(NewHandler(...))`; the listener is bound before serving
/// so a request issued immediately never races startup.
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
