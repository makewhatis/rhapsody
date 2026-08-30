//! handlers_teams — the Rhapsody Teams memory endpoints (STUDIO-645, slice T4;
//! design record `~/.rhapsody/docs/STUDIO-572-rhapsody-teams.md`, §5, §0.11.7).
//!
//! **No Go v0.4.0 counterpart, and no capture fixture** — this is the same
//! additive shape `/api/v1/capabilities` established. Every route here is NEW:
//! nothing is added to a parity-checked view, no golden moves, and a daemon
//! with Teams off answers each of them `teams_disabled` rather than growing a
//! key anywhere an existing fixture can see (§2.4 row 3).
//!
//! | Route | Backs |
//! |---|---|
//! | `GET /api/v1/teams/roster` | `teams_roster` |
//! | `GET /api/v1/teams/recall` | `teams_recall {identity, query}` |
//! | `POST /api/v1/teams/invalidate` | `teams_invalidate {identity, fact_id, reason}` |
//! | `POST /api/v1/runs/{id}/retain` | `teams_retain {content}` |
//!
//! Retain is deliberately **run-scoped in its path**, following
//! `/api/v1/runs/{id}/handoff`: the run id is what the host resolves the
//! identity, ticket and commit from, and the body carries `content` and nothing
//! else. There is no route by which an agent can supply its own provenance
//! (§5.1).

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{Method, StatusCode};
use axum::response::Response;
use rhapsody_orchestrator::teamsmemory::TeamsMemoryError;
use serde::Deserialize;

use crate::handlers::{require_get, require_post};
use crate::handlers_runaction::parse_run_id;
use crate::responses::{write_error, write_json};
use crate::server::StateProvider;

/// Bounds a retained record on the wire. The bank truncates to its own
/// `MAX_RETAIN_CONTENT_BYTES` as well; this rejects an obvious paste of a whole
/// transcript at the door, with a message that says what a record is for
/// (§5.1: a *constructed record, never a transcript*).
const MAX_RETAIN_BODY: usize = 1 << 16;

/// `GET /api/v1/teams/recall?identity=&query=`.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct RecallParams {
    #[serde(default)]
    identity: String,
    #[serde(default)]
    query: String,
}

/// `POST /api/v1/teams/invalidate` body.
#[derive(Debug, Default, Deserialize)]
struct InvalidateReq {
    #[serde(default)]
    identity: String,
    #[serde(default)]
    fact_id: String,
    #[serde(default)]
    reason: String,
}

/// `POST /api/v1/runs/{id}/retain` body — `content` and nothing else. Any other
/// key is ignored, which is the point: there is no field an agent could add to
/// influence the provenance the host stamps.
#[derive(Debug, Default, Deserialize)]
struct RetainReq {
    #[serde(default)]
    content: String,
}

/// Maps a [`TeamsMemoryError`] onto the response envelope. The split matters:
/// `teams_disabled` and `not_running` are *business* outcomes an agent should
/// read and stop on, while a backend failure is a 500 it may retry.
fn teams_error(err: &TeamsMemoryError) -> Response {
    match err {
        TeamsMemoryError::Disabled => write_error(
            StatusCode::CONFLICT,
            "teams_disabled",
            err.to_string(),
            None,
        ),
        TeamsMemoryError::NotRunning => {
            write_error(StatusCode::CONFLICT, "not_running", err.to_string(), None)
        }
        TeamsMemoryError::NotFound(_) => {
            write_error(StatusCode::NOT_FOUND, "not_found", err.to_string(), None)
        }
        TeamsMemoryError::Invalid(_) => write_error(
            StatusCode::BAD_REQUEST,
            "bad_request",
            err.to_string(),
            None,
        ),
        TeamsMemoryError::Backend(_) => write_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "memory_backend_error",
            err.to_string(),
            None,
        ),
    }
}

/// `GET /api/v1/teams/roster` — who is on the roster, the profile each wears,
/// and the runs live as each right now (§6.7's "derived status").
pub(crate) async fn handle_teams_roster(
    method: Method,
    State(provider): State<Arc<dyn StateProvider>>,
) -> Response {
    if let Some(resp) = require_get(&method) {
        return resp;
    }
    match provider.teams_roster().await {
        Ok(view) => write_json(StatusCode::OK, &view),
        Err(e) => teams_error(&e),
    }
}

/// `GET /api/v1/teams/recall` — an identity's memory for a free-text query, the
/// memory-first path that costs no model turn (§6.1, §6.7).
pub(crate) async fn handle_teams_recall(
    method: Method,
    Query(params): Query<RecallParams>,
    State(provider): State<Arc<dyn StateProvider>>,
) -> Response {
    if let Some(resp) = require_get(&method) {
        return resp;
    }
    match provider.teams_recall(&params.identity, &params.query).await {
        Ok(view) => write_json(StatusCode::OK, &view),
        Err(e) => teams_error(&e),
    }
}

/// `POST /api/v1/teams/invalidate` — §5.3's per-record correction, with the
/// reason stored and nothing deleted.
pub(crate) async fn handle_teams_invalidate(
    method: Method,
    State(provider): State<Arc<dyn StateProvider>>,
    body: Bytes,
) -> Response {
    if let Some(resp) = require_post(&method, "use POST to invalidate a memory") {
        return resp;
    }
    let req: InvalidateReq = match serde_json::from_slice(&body) {
        Ok(req) => req,
        Err(err) => return write_error(StatusCode::BAD_REQUEST, "bad_json", err.to_string(), None),
    };
    match provider
        .teams_invalidate(&req.identity, &req.fact_id, &req.reason)
        .await
    {
        Ok(view) => write_json(StatusCode::OK, &view),
        Err(e) => teams_error(&e),
    }
}

/// `POST /api/v1/runs/{id}/retain` — record what THIS run learned, with every
/// provenance field stamped by the host (§5.1).
pub(crate) async fn handle_run_retain(
    method: Method,
    Path(id): Path<String>,
    State(provider): State<Arc<dyn StateProvider>>,
    body: Bytes,
) -> Response {
    if let Some(resp) = require_post(&method, "use POST to retain a memory") {
        return resp;
    }
    let run_id = match parse_run_id(&id) {
        Ok(run_id) => run_id,
        Err(resp) => return *resp,
    };
    if body.len() > MAX_RETAIN_BODY {
        return write_error(
            StatusCode::BAD_REQUEST,
            "content_too_long",
            "a retained record is a short constructed observation, not a transcript",
            None,
        );
    }
    let req: RetainReq = match serde_json::from_slice(&body) {
        Ok(req) => req,
        Err(err) => return write_error(StatusCode::BAD_REQUEST, "bad_json", err.to_string(), None),
    };
    match provider.teams_retain(run_id, &req.content).await {
        Ok(view) => write_json(StatusCode::OK, &view),
        Err(e) => teams_error(&e),
    }
}

#[cfg(test)]
mod tests {
    //! The Teams memory endpoints, driven end to end against a REAL
    //! `TeamsMemory` over a temp bank — a canned provider result would prove
    //! only that the handler forwards, and the properties worth pinning here
    //! (host-stamped provenance, a Teams-off daemon answering `teams_disabled`)
    //! live in the interaction between the two.

    use std::sync::Arc;

    use rhapsody_config::memory::{DEFAULT_BANKS_SUBDIR, LocalBank};
    use rhapsody_config::teams::{Identity, Teams};
    use rhapsody_orchestrator::teamsmemory::{RunProvenance, TeamsMemory};
    use serde_json::Value;

    use crate::new_handler;
    use crate::testutil::{FakeProvider, empty_snapshot, spawn_router};

    /// A temp directory that cleans itself up; the crate takes no `tempfile`
    /// dependency, matching the sibling crates' hand-rolled helper.
    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new() -> Self {
            static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let p = std::env::temp_dir()
                .join(format!("rhapsody-httpapi-teams-{}-{n}", std::process::id()));
            std::fs::create_dir_all(&p).expect("create temp dir");
            Self(p)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn teams_memory(dir: &TempDir) -> Arc<TeamsMemory> {
        let teams = Teams {
            enabled: true,
            roster: vec![Identity {
                name: "alice".to_string(),
                profile: "swe".to_string(),
                labels: vec!["rust".to_string()],
                ..Identity::default()
            }],
            ..Teams::disabled()
        };
        let bank = LocalBank::new(dir.0.join(DEFAULT_BANKS_SUBDIR), "agent-");
        Arc::new(TeamsMemory::new(Arc::new(teams), Arc::new(bank)))
    }

    async fn spawn(provider: Arc<FakeProvider>) -> String {
        spawn_router(new_handler(provider, None)).await
    }

    async fn spawn_with(mem: Arc<TeamsMemory>) -> String {
        spawn(Arc::new(
            FakeProvider::ok(empty_snapshot()).with_teams_memory(mem),
        ))
        .await
    }

    async fn post(url: &str, body: &str) -> reqwest::Response {
        reqwest::Client::new()
            .post(url)
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .expect("POST")
    }

    async fn body_json(resp: reqwest::Response) -> Value {
        let text = resp.text().await.expect("body text");
        serde_json::from_str(&text).expect("json body")
    }

    async fn err_code(resp: reqwest::Response) -> String {
        body_json(resp).await["error"]["code"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    /// **Retain → recall → invalidate, end to end through HTTP.** The retain
    /// body carries `content` and nothing else; every provenance field on the
    /// response was stamped by the host from the run id in the PATH (§5.1).
    #[tokio::test]
    async fn retain_recall_invalidate_round_trip() {
        let dir = TempDir::new();
        let mem = teams_memory(&dir);
        mem.bind_run(
            7,
            RunProvenance {
                identity: "alice".to_string(),
                ticket: "MT-9".to_string(),
                workspace_dir: String::new(),
            },
        );
        let url = spawn_with(Arc::clone(&mem)).await;

        let retained = body_json(
            post(
                &format!("{url}/api/v1/runs/7/retain"),
                r#"{"content":"the mirror lock is per-repo","identity":"bob","ticket":"XX-1"}"#,
            )
            .await,
        )
        .await;
        assert_eq!(
            retained["identity"], "alice",
            "the identity is the RUN's, not the body's: {retained}"
        );
        assert_eq!(
            retained["ticket"], "MT-9",
            "the ticket is the RUN's, not the body's: {retained}"
        );
        assert_eq!(retained["document_id"], "run-7");
        let fact_id = retained["id"].as_str().expect("a record id").to_string();

        let recalled = body_json(
            reqwest::get(&format!(
                "{url}/api/v1/teams/recall?identity=alice&query=mirror%20lock"
            ))
            .await
            .expect("GET recall"),
        )
        .await;
        assert_eq!(recalled["facts"].as_array().expect("facts").len(), 1);
        assert_eq!(
            recalled["facts"][0]["content"],
            "the mirror lock is per-repo"
        );

        let invalidated = body_json(
            post(
                &format!("{url}/api/v1/teams/invalidate"),
                &format!(
                    r#"{{"identity":"alice","fact_id":"{fact_id}","reason":"the lock moved in MT-10"}}"#
                ),
            )
            .await,
        )
        .await;
        assert_eq!(invalidated["invalidated"], true);
        assert_eq!(invalidated["reason"], "the lock moved in MT-10");

        let after = body_json(
            reqwest::get(&format!(
                "{url}/api/v1/teams/recall?identity=alice&query=mirror%20lock"
            ))
            .await
            .expect("GET recall"),
        )
        .await;
        assert!(
            after["facts"].as_array().expect("facts").is_empty(),
            "an invalidated fact leaves recall: {after}"
        );
    }

    /// A retain naming a run the host has no binding for is `not_running`, not
    /// a record attributed to a guess.
    #[tokio::test]
    async fn retain_from_an_unbound_run_is_not_running() {
        let dir = TempDir::new();
        let url = spawn_with(teams_memory(&dir)).await;
        let resp = post(
            &format!("{url}/api/v1/runs/99/retain"),
            r#"{"content":"anything"}"#,
        )
        .await;
        assert_eq!(resp.status(), 409);
        assert_eq!(err_code(resp).await, "not_running");
    }

    /// The roster reports who exists and what each is doing (§6.7).
    #[tokio::test]
    async fn the_roster_endpoint_reports_derived_status() {
        let dir = TempDir::new();
        let mem = teams_memory(&dir);
        mem.bind_run(
            7,
            RunProvenance {
                identity: "alice".to_string(),
                ticket: "MT-9".to_string(),
                workspace_dir: String::new(),
            },
        );
        let url = spawn_with(mem).await;
        let view = body_json(
            reqwest::get(&format!("{url}/api/v1/teams/roster"))
                .await
                .expect("GET roster"),
        )
        .await;
        assert_eq!(view["backend"], "local");
        assert_eq!(view["roster"][0]["name"], "alice");
        assert_eq!(view["roster"][0]["profile"], "swe");
        assert_eq!(view["roster"][0]["live_runs"], 1);
        assert_eq!(view["roster"][0]["tickets"][0], "MT-9");
    }

    /// **A daemon with no Teams runtime answers `teams_disabled` on every
    /// route** — the same answer `enabled: false` gives. The routes exist (they
    /// are static paths on the router), but they contribute nothing, and the MCP
    /// facade removes the tools entirely so an agent never reaches them.
    #[tokio::test]
    async fn every_teams_route_is_disabled_without_a_teams_runtime() {
        let url = spawn(Arc::new(FakeProvider::ok(empty_snapshot()))).await;

        for path in ["/api/v1/teams/roster", "/api/v1/teams/recall?identity=a"] {
            let resp = reqwest::get(&format!("{url}{path}")).await.expect("GET");
            assert_eq!(resp.status(), 409, "{path}");
            assert_eq!(err_code(resp).await, "teams_disabled", "{path}");
        }
        for (path, body) in [
            (
                "/api/v1/teams/invalidate",
                r#"{"identity":"a","fact_id":"b","reason":"c"}"#,
            ),
            ("/api/v1/runs/7/retain", r#"{"content":"x"}"#),
        ] {
            let resp = post(&format!("{url}{path}"), body).await;
            assert_eq!(resp.status(), 409, "{path}");
            assert_eq!(err_code(resp).await, "teams_disabled", "{path}");
        }
    }

    /// An invalidation with no reason is refused: §5.3 stores the reason, and a
    /// reasonless correction is unreadable to whoever finds it later.
    #[tokio::test]
    async fn invalidate_without_a_reason_is_rejected() {
        let dir = TempDir::new();
        let url = spawn_with(teams_memory(&dir)).await;
        let resp = post(
            &format!("{url}/api/v1/teams/invalidate"),
            r#"{"identity":"alice","fact_id":"20260101T000000Z-run-1"}"#,
        )
        .await;
        assert_eq!(resp.status(), 400);
        assert_eq!(err_code(resp).await, "bad_request");
    }

    /// A record that does not exist is a 404, distinguishable from a backend
    /// failure so a caller knows retrying will not help.
    #[tokio::test]
    async fn invalidating_an_unknown_record_is_404() {
        let dir = TempDir::new();
        let url = spawn_with(teams_memory(&dir)).await;
        let resp = post(
            &format!("{url}/api/v1/teams/invalidate"),
            r#"{"identity":"alice","fact_id":"20260101T000000Z-run-1","reason":"why"}"#,
        )
        .await;
        assert_eq!(resp.status(), 404);
        assert_eq!(err_code(resp).await, "not_found");
    }

    /// Every route rejects the wrong method with a 405 envelope rather than
    /// falling through to the SPA fallback — the convention every other route
    /// in this crate follows.
    #[tokio::test]
    async fn wrong_methods_are_405_not_the_spa_fallback() {
        let dir = TempDir::new();
        let url = spawn_with(teams_memory(&dir)).await;
        for path in ["/api/v1/teams/roster", "/api/v1/teams/recall"] {
            let resp = post(&format!("{url}{path}"), "{}").await;
            assert_eq!(resp.status(), 405, "{path}");
        }
        for path in ["/api/v1/teams/invalidate", "/api/v1/runs/7/retain"] {
            let resp = reqwest::get(&format!("{url}{path}")).await.expect("GET");
            assert_eq!(resp.status(), 405, "{path}");
        }
    }
}
