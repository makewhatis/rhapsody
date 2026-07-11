//! handlers — the H1 request handlers: `/healthz` and `GET /api/v1/state`, plus the shared method
//! guard. Parity port of `$REF/internal/httpapi/handlers.go` (`handleHealthz`/`handleState`/
//! `requireGET`). The `/state` wire view is O4's `orchestrator::snapshot_json::render`, REUSED here
//! (no reimplementation — the plan's byte-parity rule), matching how the config handler reuses
//! `effective_json`.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::{Method, StatusCode};
use axum::response::Response;
use rhapsody_orchestrator::snapshot_json;

use crate::responses::{HealthzJson, write_error, write_json};
use crate::server::StateProvider;

/// Bounds how long a `/state` request waits on the orchestrator control task. Mirrors Go
/// `snapshotTimeout` (2s): the HTTP layer owns the deadline (Go wraps the request ctx), so even a
/// wedged provider yields a prompt 503 rather than hanging the desktop supervisor's readiness poll.
/// `pub(crate)` so the run-detail handler (H2), which also consults the snapshot, shares the deadline.
pub(crate) const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(2);

/// `GET /healthz` — a cheap, state-free liveness/readiness probe for the desktop supervisor. Never
/// touches orchestrator state, so it answers even before the first poll. Mirrors Go `handleHealthz`.
pub(crate) async fn handle_healthz(method: Method) -> Response {
    if let Some(resp) = require_get(&method) {
        return resp;
    }
    write_json(StatusCode::OK, &HealthzJson { status: "ok" })
}

/// `GET /api/v1/state` — the synchronous runtime view. Renders the O4 `Snapshot` via the REUSED
/// `snapshot_json::render`; a snapshot failure or timeout is a 503 `snapshot_unavailable` envelope
/// (Go maps both `ErrSnapshotUnavailable` and `ErrSnapshotTimeout` to that one body). Mirrors Go
/// `handleState`.
pub(crate) async fn handle_state(
    method: Method,
    State(provider): State<Arc<dyn StateProvider>>,
) -> Response {
    if let Some(resp) = require_get(&method) {
        return resp;
    }
    match tokio::time::timeout(SNAPSHOT_TIMEOUT, provider.snapshot()).await {
        Ok(Ok(snap)) => write_json(StatusCode::OK, &snapshot_json::render(&snap)),
        Ok(Err(err)) => write_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "snapshot_unavailable",
            err.to_string(),
            None,
        ),
        Err(_elapsed) => write_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "snapshot_unavailable",
            "snapshot timed out",
            None,
        ),
    }
}

/// Enforce GET/HEAD on a read-only route: `Some(405 envelope)` (with `Allow: GET, HEAD`) on any other
/// method, `None` when allowed. The routes are registered method-agnostically (`any`) so a mismatch
/// reaches the handler and yields an explicit 405 rather than the SPA fallback swallowing it into a
/// 404. HEAD is allowed — the server elides the response body for HEAD, so no handler change is
/// needed. Mirrors Go `requireGET`. `pub(crate)` so every read handler (H2) shares the one guard.
pub(crate) fn require_get(method: &Method) -> Option<Response> {
    if *method == Method::GET || *method == Method::HEAD {
        return None;
    }
    Some(write_error(
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
        "use GET",
        Some("GET, HEAD"),
    ))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use reqwest::header::CONTENT_TYPE;
    use rhapsody_orchestrator::{Snapshot, Totals};
    use serde_json::Value;

    use crate::new_handler;
    use crate::testutil::{
        FakeProvider, empty_snapshot, fixed_instant, retry_row, running_row, spawn_router,
    };

    /// Recursively sort object keys, mirroring the capture pipeline's `jq -S .` (which stabilizes key
    /// order before a fixture is committed). Same helper the config crate's golden test uses.
    fn sort_keys(v: Value) -> Value {
        match v {
            Value::Object(m) => {
                let sorted: BTreeMap<String, Value> =
                    m.into_iter().map(|(k, v)| (k, sort_keys(v))).collect();
                Value::Object(sorted.into_iter().collect())
            }
            Value::Array(a) => Value::Array(a.into_iter().map(sort_keys).collect()),
            other => other,
        }
    }

    async fn spawn(provider: FakeProvider) -> String {
        spawn_router(new_handler(Arc::new(provider), None)).await
    }

    async fn get_json(url: &str) -> (reqwest::StatusCode, Value) {
        let resp = reqwest::get(url).await.expect("GET");
        let status = resp.status();
        let text = resp.text().await.expect("body text");
        let body: Value = serde_json::from_str(&text).expect("json body");
        (status, body)
    }

    // ------- healthz (mirrors healthz_test.go) -------

    // Mirrors Go `TestHealthzEndpoint`.
    #[tokio::test]
    async fn healthz_endpoint() {
        let base = spawn(FakeProvider::ok(empty_snapshot())).await;
        let resp = reqwest::get(format!("{base}/healthz"))
            .await
            .expect("GET /healthz");
        assert_eq!(resp.status(), 200);
        let ct = resp
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert_eq!(ct, "application/json", "content-type");
        let text = resp.text().await.expect("body");
        let body: Value = serde_json::from_str(&text).expect("json");
        assert_eq!(body["status"], "ok");
    }

    // Mirrors Go `TestHealthzMethodNotAllowed`.
    #[tokio::test]
    async fn healthz_method_not_allowed() {
        let base = spawn(FakeProvider::ok(empty_snapshot())).await;
        let resp = reqwest::Client::new()
            .post(format!("{base}/healthz"))
            .send()
            .await
            .expect("POST /healthz");
        assert_eq!(resp.status(), 405);
    }

    // Mirrors Go `TestHealthzHEADAllowed`.
    #[tokio::test]
    async fn healthz_head_allowed() {
        let base = spawn(FakeProvider::ok(empty_snapshot())).await;
        let resp = reqwest::Client::new()
            .head(format!("{base}/healthz"))
            .send()
            .await
            .expect("HEAD /healthz");
        assert_eq!(resp.status(), 200);
    }

    // ------- state endpoint (mirrors server_test.go) -------

    // Mirrors Go `TestStateEndpoint`.
    #[tokio::test]
    async fn state_endpoint() {
        let mut snap = empty_snapshot();
        snap.generated_at = fixed_instant();
        let mut row = running_row("MT-1");
        row.issue_id = "abc".to_string();
        row.title = "fix the bug".to_string();
        row.state = "In Progress".to_string();
        row.last_event = "turn_completed".to_string();
        row.turn_count = 2;
        row.run_id = 101;
        row.tokens.input_tokens = 100;
        row.tokens.output_tokens = 50;
        row.tokens.total_tokens = 150;
        snap.running.push(row);
        let mut retry = retry_row("MT-2");
        retry.attempt = 3;
        retry.error = "no available orchestrator slots".to_string();
        snap.retrying.push(retry);
        snap.totals = Totals {
            input_tokens: 100,
            output_tokens: 50,
            total_tokens: 150,
            seconds_running: 12.5,
        };

        let base = spawn(FakeProvider::ok(snap)).await;
        let (status, body) = get_json(&format!("{base}/api/v1/state")).await;
        assert_eq!(status, 200);
        assert_eq!(body["status"], "ok");
        assert_eq!(body["poll_interval_ms"], 2000);
        assert_eq!(body["counts"]["running"], 1);
        assert_eq!(body["counts"]["retrying"], 1);
        let run = &body["running"][0];
        assert_eq!(run["issue_identifier"], "MT-1");
        assert_eq!(run["last_codex_event"], "turn_completed");
        // Tokens are FLAT now (no nested tokens{}).
        assert!(
            run.get("tokens").is_none(),
            "running row still nests tokens"
        );
        assert_eq!(run["total_tokens"], 150);
        // rate_limits is always [] (never null).
        assert_eq!(body["rate_limits"], serde_json::json!([]));
        assert_eq!(body["codex_totals"]["seconds_running"], 12.5);
    }

    // Mirrors Go `TestStateMethodNotAllowed`.
    #[tokio::test]
    async fn state_method_not_allowed() {
        let base = spawn(FakeProvider::ok(empty_snapshot())).await;
        let resp = reqwest::Client::new()
            .post(format!("{base}/api/v1/state"))
            .send()
            .await
            .expect("POST /state");
        assert_eq!(resp.status(), 405);
    }

    // Mirrors Go `TestStateHEADAllowed`: HEAD must get 200, not 405 (routes are method-agnostic).
    #[tokio::test]
    async fn state_head_allowed() {
        let base = spawn(FakeProvider::ok(empty_snapshot())).await;
        let resp = reqwest::Client::new()
            .head(format!("{base}/api/v1/state"))
            .send()
            .await
            .expect("HEAD /state");
        assert_eq!(resp.status(), 200);
    }

    // Mirrors Go `TestStateSnapshotErrorReturns503`.
    #[tokio::test]
    async fn state_snapshot_error_returns_503() {
        let base = spawn(FakeProvider::failing("snapshot_timeout")).await;
        let (status, body) = get_json(&format!("{base}/api/v1/state")).await;
        assert_eq!(status, 503);
        assert!(!body["error"].is_null(), "expected error envelope");
        assert_eq!(body["error"]["code"], "snapshot_unavailable");
    }

    // ------- state wire shape (mirrors state_test.go) -------

    // Mirrors Go `TestStateGoldenWireShape`: the exact reshaped /state wire shape — flat running
    // tokens, last_codex_event, title/project/repo, RFC3339-string times, rate_limits:[].
    #[tokio::test]
    async fn state_golden_wire_shape() {
        let started = fixed_instant();
        let last_ev = started + chrono::Duration::minutes(30);
        let due = started + chrono::Duration::minutes(5);
        let mut snap = empty_snapshot();
        snap.generated_at = started;
        let mut row = running_row("MT-1");
        row.issue_id = "abc".to_string();
        row.title = "fix the bug".to_string();
        row.state = "In Progress".to_string();
        row.project = "core".to_string();
        row.repo = "git@example.com:core.git".to_string();
        row.run_id = 42;
        row.turn_count = 2;
        row.last_event = "turn_completed".to_string();
        row.started_at = started;
        row.last_event_at = last_ev;
        row.tokens.input_tokens = 100;
        row.tokens.output_tokens = 50;
        row.tokens.total_tokens = 150;
        snap.running.push(row);
        let mut retry = retry_row("MT-2");
        retry.attempt = 3;
        retry.due_at = due;
        retry.error = "no slots".to_string();
        snap.retrying.push(retry);
        snap.totals = Totals {
            input_tokens: 100,
            output_tokens: 50,
            total_tokens: 150,
            seconds_running: 12.5,
        };

        let base = spawn(FakeProvider::ok(snap)).await;
        let (status, body) = get_json(&format!("{base}/api/v1/state")).await;
        assert_eq!(status, 200);
        assert_eq!(body["status"], "ok");
        assert_eq!(body["poll_interval_ms"], 2000);
        assert_eq!(body["generated_at"], "2026-05-28T12:00:00Z");

        let run = body["running"][0].as_object().expect("running row");
        let want: BTreeMap<&str, Value> = BTreeMap::from([
            ("issue_id", Value::from("abc")),
            ("issue_identifier", Value::from("MT-1")),
            ("title", Value::from("fix the bug")),
            ("state", Value::from("In Progress")),
            ("project", Value::from("core")),
            ("repo", Value::from("git@example.com:core.git")),
            ("run_id", Value::from(42)),
            ("turn_count", Value::from(2)),
            ("last_codex_event", Value::from("turn_completed")),
            ("started_at", Value::from("2026-05-28T12:00:00Z")),
            ("last_event_at", Value::from("2026-05-28T12:30:00Z")),
            ("input_tokens", Value::from(100)),
            ("output_tokens", Value::from(50)),
            ("total_tokens", Value::from(150)),
        ]);
        assert_eq!(run.len(), want.len(), "running row key set: {run:?}");
        for (k, v) in &want {
            assert_eq!(run.get(*k), Some(v), "running[{k}]");
        }
        // Dropped fields must be gone.
        for k in ["tokens", "session_id", "last_message"] {
            assert!(run.get(k).is_none(), "running row still carries {k}");
        }

        let ret = &body["retrying"][0];
        assert_eq!(ret["issue_identifier"], "MT-2");
        assert_eq!(ret["attempt"], 3);
        assert_eq!(ret["due_at"], "2026-05-28T12:05:00Z");
        assert_eq!(ret["error"], "no slots");
        assert!(
            ret.get("issue_id").is_none(),
            "retry row still carries issue_id"
        );
        assert_eq!(body["rate_limits"], serde_json::json!([]));
    }

    // Mirrors Go `TestStateZeroTimesEmptyString`: zero times serialize as "" (not null / sentinel).
    #[tokio::test]
    async fn state_zero_times_empty_string() {
        let mut snap = empty_snapshot();
        snap.running.push(running_row("MT-1")); // zero started_at / last_event_at
        snap.retrying.push(retry_row("MT-2")); // zero due_at
        let base = spawn(FakeProvider::ok(snap)).await;
        let (_status, body) = get_json(&format!("{base}/api/v1/state")).await;
        let run = &body["running"][0];
        assert_eq!(run["started_at"], "");
        assert_eq!(run["last_event_at"], "");
        assert_eq!(body["retrying"][0]["due_at"], "");
    }

    // Mirrors Go `TestStateEmptySnapshotEmptyLists`: an empty snapshot serves [] lists (never null).
    #[tokio::test]
    async fn state_empty_snapshot_empty_lists() {
        let base = spawn(FakeProvider::ok(empty_snapshot())).await;
        let (_status, body) = get_json(&format!("{base}/api/v1/state")).await;
        for k in ["running", "retrying", "rate_limits"] {
            assert_eq!(body[k], serde_json::json!([]), "{k} must be []");
        }
    }

    // ------- H1 acceptance: served /state byte-matches the committed golden -------

    // The H1 gate: the served `GET /api/v1/state` body, normalized, is byte-identical to the committed
    // `api/state.json` fixture. Reproduces the captured scenario (0 running, 1 retrying RHA-1,
    // codex_totals 20/20/40) — the same fixture O4's `snapshot_json` test proves at the render level,
    // asserted here end-to-end over the HTTP server.
    #[tokio::test]
    async fn state_endpoint_matches_state_golden() {
        let mut snap: Snapshot = empty_snapshot();
        snap.generated_at = fixed_instant();
        let mut retry = retry_row("RHA-1");
        retry.attempt = 1;
        retry.due_at = fixed_instant();
        snap.retrying.push(retry);
        snap.totals = Totals {
            input_tokens: 20,
            output_tokens: 20,
            total_tokens: 40,
            seconds_running: 0.0,
        };

        let base = spawn(FakeProvider::ok(snap)).await;
        let (status, body) = get_json(&format!("{base}/api/v1/state")).await;
        assert_eq!(status, 200);

        let pretty = format!(
            "{}\n",
            serde_json::to_string_pretty(&sort_keys(body)).expect("serialize")
        );
        let got = harness_fixtures::normalize(&pretty);
        let want = harness_fixtures::normalize(&harness_fixtures::load("api/state.json"));
        assert_eq!(got, want, "served /state drifts from api/state.json golden");
    }
}
