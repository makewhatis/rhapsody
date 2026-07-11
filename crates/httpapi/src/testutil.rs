//! Shared test scaffolding for the httpapi handler/web tests: a fake [`StateProvider`], a loopback
//! server spawner, and `Snapshot` builders. The Rust analog of `server_test.go`'s `fakeProvider` +
//! `testServer` + `sampleSnapshot` helpers, narrowed to the H1 surface.

use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use rhapsody_orchestrator::{RetryRow, RunningRow, Snapshot, TokenCounts, Totals};

use crate::{SnapshotError, StateProvider};

/// A canned [`StateProvider`]: returns a fixed snapshot, or a snapshot error. Mirrors Go
/// `fakeProvider`, narrowed to H1's `snapshot`.
pub(crate) struct FakeProvider {
    snap: Snapshot,
    snap_err: Option<String>,
}

impl FakeProvider {
    /// A provider that returns `snap` from every `snapshot()` call.
    pub(crate) fn ok(snap: Snapshot) -> Self {
        Self {
            snap,
            snap_err: None,
        }
    }

    /// A provider whose `snapshot()` fails with `message` (drives the 503 path).
    pub(crate) fn failing(message: &str) -> Self {
        Self {
            snap: empty_snapshot(),
            snap_err: Some(message.to_string()),
        }
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
