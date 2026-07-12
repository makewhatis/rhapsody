//! handlers_runaction — the run-action write handlers: `POST /api/v1/runs/{id}/stop` and
//! `POST /api/v1/runs/{id}/resume`. Parity port of `$REF/internal/httpapi/handlers_runaction.go`
//! (`handleRunStop`/`handleRunResume`/`parseRunID`/`runActionJSON`).
//!
//! Both are POST-only (405 on any other method, with `Allow: POST`), take `{id}` as a positive run
//! id (a bad/zero id is a 404), and split a control-round-trip failure (→ 500) from a *business*
//! outcome (not-running/not-found → 409/404, a partial move failure → 200 with `move_error`) exactly
//! as Go does: the failure detail rides in the `StopResult`/`ResumeResult` value, never on an
//! error status.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{Method, StatusCode};
use axum::response::Response;
use serde::Serialize;

use crate::handlers::require_post;
use crate::responses::{write_error, write_json};
use crate::server::StateProvider;

/// The 200 body for the stop/resume endpoints (Go `runActionJSON`). `moved_to`/`move_error` are
/// omitted when empty, so a clean success is just `{identifier, moved_to}` and a partial-success kill
/// is `{identifier, move_error}`. `move_error` only ever travels in a 200 body — a kill/resume that
/// was performed is a success even when the follow-on ticket move failed.
#[derive(Serialize)]
struct RunActionJson {
    identifier: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    moved_to: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    move_error: String,
}

/// Parse `{id}` as a positive run id, or return `Err(404 envelope)` on a bad/zero id (run id 0 is
/// reserved for the persistence-disabled state and is never a real run). Mirrors Go `parseRunID`.
/// `pub(crate)` so the run-message handler (also `{id}`-keyed) shares the one parser. The error is
/// `Box`ed so the common `Ok` path stays small (clippy `result_large_err`: a [`Response`] is large).
pub(crate) fn parse_run_id(id: &str) -> Result<i64, Box<Response>> {
    match id.parse::<i64>() {
        Ok(run_id) if run_id > 0 => Ok(run_id),
        _ => Err(Box::new(write_error(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("invalid run id: {id}"),
            None,
        ))),
    }
}

/// `POST /api/v1/runs/{id}/stop` — kill the agent + move its ticket to Backlog. `not_running` ⇒ 409;
/// a kill with a failed Backlog move is a PARTIAL SUCCESS (200 with `move_error` in the body, which
/// the UI surfaces so the operator moves the ticket by hand before a restart could re-dispatch it).
/// Mirrors Go `handleRunStop`.
pub(crate) async fn handle_run_stop(
    method: Method,
    Path(id): Path<String>,
    State(provider): State<Arc<dyn StateProvider>>,
) -> Response {
    if let Some(resp) = require_post(&method, "use POST to stop a run") {
        return resp;
    }
    let run_id = match parse_run_id(&id) {
        Ok(run_id) => run_id,
        Err(resp) => return *resp,
    };
    match provider.stop_run(run_id).await {
        Err(err) => write_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "stop_failed",
            err.to_string(),
            None,
        ),
        Ok(res) if res.not_running => write_error(
            StatusCode::CONFLICT,
            "not_running",
            "run is not currently running",
            None,
        ),
        // The run was found and the agent killed. A failed Backlog move is a partial success, not an
        // error: return 200 and let `move_error` carry the detail to the UI.
        Ok(res) => write_json(
            StatusCode::OK,
            &RunActionJson {
                identifier: res.identifier,
                moved_to: res.moved_to,
                move_error: res.move_err,
            },
        ),
    }
}

/// `POST /api/v1/runs/{id}/resume` — move a stopped run's ticket back to Todo. NotFound ⇒ 404;
/// NoTeam (pre-resume-support row) / NotStopped (only stopped runs resume) / LiveRun (a newer run for
/// the same issue is executing) / Superseded (a newer run already finished non-stopped) ⇒ 409. A
/// failed Todo move is a partial success — 200 with `move_error`. Mirrors Go `handleRunResume`.
pub(crate) async fn handle_run_resume(
    method: Method,
    Path(id): Path<String>,
    State(provider): State<Arc<dyn StateProvider>>,
) -> Response {
    if let Some(resp) = require_post(&method, "use POST to resume a run") {
        return resp;
    }
    let run_id = match parse_run_id(&id) {
        Ok(run_id) => run_id,
        Err(resp) => return *resp,
    };
    let res = match provider.resume_run(run_id).await {
        Ok(res) => res,
        Err(err) => {
            return write_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "resume_failed",
                err.to_string(),
                None,
            );
        }
    };
    if res.not_found {
        return write_error(
            StatusCode::NOT_FOUND,
            "run_not_found",
            "no run with that id",
            None,
        );
    }
    if res.not_stopped {
        return write_error(
            StatusCode::CONFLICT,
            "not_stopped",
            "only a stopped run can be resumed",
            None,
        );
    }
    if res.live_run {
        return write_error(
            StatusCode::CONFLICT,
            "live_run",
            "a newer run for this issue is currently executing — stop it before resuming an older attempt",
            None,
        );
    }
    if res.superseded {
        return write_error(
            StatusCode::CONFLICT,
            "superseded",
            "a newer run for this issue has already finished — resuming this older attempt would re-open a completed ticket",
            None,
        );
    }
    if res.no_team {
        return write_error(
            StatusCode::CONFLICT,
            "no_team",
            "this run predates resume support — move the ticket back to an active state in Linear",
            None,
        );
    }
    // The run was found and processed; a failed Todo move is a partial success carried via
    // `move_error` in a 200 body, not an error status.
    write_json(
        StatusCode::OK,
        &RunActionJson {
            identifier: res.identifier,
            moved_to: res.moved_to,
            move_error: res.move_err,
        },
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use reqwest::StatusCode;
    use rhapsody_orchestrator::{ResumeResult, StopResult};
    use serde_json::Value;

    use crate::new_handler;
    use crate::testutil::{FakeProvider, empty_snapshot, spawn_router};

    /// Spawn a loopback server over an `Arc<FakeProvider>` the test keeps a clone of, so it can read
    /// back the recorded run id after the request (mirrors Go holding the `*fakeProvider` pointer).
    async fn spawn(provider: Arc<FakeProvider>) -> String {
        spawn_router(new_handler(provider, None)).await
    }

    async fn do_action(url: &str, method: reqwest::Method) -> reqwest::Response {
        reqwest::Client::new()
            .request(method, url)
            .send()
            .await
            .expect("run-action request")
    }

    /// Decode a response body as JSON (the crate's `reqwest` is built without the `json` feature, so
    /// tests read text + `serde_json::from_str`, exactly like the H1/H2 handler tests).
    async fn body_json(resp: reqwest::Response) -> Value {
        let text = resp.text().await.expect("body text");
        serde_json::from_str(&text).expect("json body")
    }

    async fn err_code(resp: reqwest::Response) -> String {
        let body = body_json(resp).await;
        body["error"]["code"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    // Mirrors Go `TestHandleRunStop_PostMovesAndReturnsJSON`.
    #[tokio::test]
    async fn stop_post_moves_and_returns_json() {
        let provider = Arc::new(
            FakeProvider::ok(empty_snapshot()).with_stop_result(StopResult {
                identifier: "INF-9".into(),
                moved_to: "Backlog".into(),
                ..Default::default()
            }),
        );
        let base = spawn(provider.clone()).await;
        let resp = do_action(&format!("{base}/api/v1/runs/7/stop"), reqwest::Method::POST).await;
        assert_eq!(resp.status(), 200);
        let body = body_json(resp).await;
        assert_eq!(body["identifier"], "INF-9");
        assert_eq!(body["moved_to"], "Backlog");
        assert!(
            body.get("move_error").is_none(),
            "move_error must be omitted on success: {body}"
        );
        assert_eq!(
            provider.stop_run_id(),
            7,
            "StopRun called with the parsed run id"
        );
    }

    // Mirrors Go `TestHandleRunStop_GetIs405`.
    #[tokio::test]
    async fn stop_get_is_405() {
        let base = spawn(Arc::new(FakeProvider::ok(empty_snapshot()))).await;
        let resp = do_action(&format!("{base}/api/v1/runs/7/stop"), reqwest::Method::GET).await;
        assert_eq!(resp.status(), 405);
        assert_eq!(
            resp.headers().get("allow").and_then(|v| v.to_str().ok()),
            Some("POST")
        );
    }

    // Mirrors Go `TestHandleRunStop_NotRunningIs409`.
    #[tokio::test]
    async fn stop_not_running_is_409() {
        let provider = Arc::new(
            FakeProvider::ok(empty_snapshot()).with_stop_result(StopResult {
                not_running: true,
                ..Default::default()
            }),
        );
        let base = spawn(provider).await;
        let resp = do_action(&format!("{base}/api/v1/runs/7/stop"), reqwest::Method::POST).await;
        assert_eq!(resp.status(), 409);
        assert_eq!(err_code(resp).await, "not_running");
    }

    // Mirrors Go `TestHandleRunStop_MoveErrStill200WithBody`: a kill whose Backlog move failed is a
    // PARTIAL SUCCESS — 200 with identifier + move_error, never an error status.
    #[tokio::test]
    async fn stop_move_err_still_200_with_body() {
        let provider = Arc::new(
            FakeProvider::ok(empty_snapshot()).with_stop_result(StopResult {
                identifier: "INF-9".into(),
                move_err: "no backlog state for team".into(),
                ..Default::default()
            }),
        );
        let base = spawn(provider).await;
        let resp = do_action(&format!("{base}/api/v1/runs/7/stop"), reqwest::Method::POST).await;
        assert_eq!(
            resp.status(),
            200,
            "a kill with a failed move is a partial success"
        );
        let body = body_json(resp).await;
        assert_eq!(body["identifier"], "INF-9");
        assert_eq!(body["move_error"], "no backlog state for team");
    }

    // Mirrors Go `TestHandleRunResume_PostMoves`.
    #[tokio::test]
    async fn resume_post_moves() {
        let provider = Arc::new(FakeProvider::ok(empty_snapshot()).with_resume_result(
            ResumeResult {
                identifier: "INF-9".into(),
                moved_to: "Todo".into(),
                ..Default::default()
            },
        ));
        let base = spawn(provider.clone()).await;
        let resp = do_action(
            &format!("{base}/api/v1/runs/7/resume"),
            reqwest::Method::POST,
        )
        .await;
        assert_eq!(resp.status(), 200);
        let body = body_json(resp).await;
        assert_eq!(body["identifier"], "INF-9");
        assert_eq!(body["moved_to"], "Todo");
        assert_eq!(provider.resume_run_id(), 7);
    }

    // Mirrors Go `TestHandleRunResume_MoveErr200WithBody`.
    #[tokio::test]
    async fn resume_move_err_200_with_body() {
        let provider = Arc::new(FakeProvider::ok(empty_snapshot()).with_resume_result(
            ResumeResult {
                identifier: "INF-9".into(),
                move_err: "no unstarted state for team".into(),
                ..Default::default()
            },
        ));
        let base = spawn(provider).await;
        let resp = do_action(
            &format!("{base}/api/v1/runs/7/resume"),
            reqwest::Method::POST,
        )
        .await;
        assert_eq!(resp.status(), 200);
        let body = body_json(resp).await;
        assert_eq!(body["identifier"], "INF-9");
        assert_eq!(body["move_error"], "no unstarted state for team");
    }

    // The five 409 conflict outcomes + the 404, driven table-style (mirrors the individual Go
    // `TestHandleRunResume_*Is409`/`_NotFoundIs404` cases).
    #[tokio::test]
    async fn resume_conflict_table() {
        let cases: Vec<(ResumeResult, u16, &str)> = vec![
            (
                ResumeResult {
                    not_found: true,
                    ..Default::default()
                },
                404,
                "run_not_found",
            ),
            (
                ResumeResult {
                    not_stopped: true,
                    identifier: "INF-9".into(),
                    ..Default::default()
                },
                409,
                "not_stopped",
            ),
            (
                ResumeResult {
                    live_run: true,
                    identifier: "INF-9".into(),
                    ..Default::default()
                },
                409,
                "live_run",
            ),
            (
                ResumeResult {
                    superseded: true,
                    identifier: "INF-9".into(),
                    ..Default::default()
                },
                409,
                "superseded",
            ),
            (
                ResumeResult {
                    no_team: true,
                    identifier: "INF-9".into(),
                    ..Default::default()
                },
                409,
                "no_team",
            ),
        ];
        for (result, want_status, want_code) in cases {
            let base = spawn(Arc::new(
                FakeProvider::ok(empty_snapshot()).with_resume_result(result),
            ))
            .await;
            let resp = do_action(
                &format!("{base}/api/v1/runs/7/resume"),
                reqwest::Method::POST,
            )
            .await;
            assert_eq!(resp.status(), StatusCode::from_u16(want_status).unwrap());
            assert_eq!(err_code(resp).await, want_code);
        }
    }

    // A bad/zero run id is a 404 (mirrors `parseRunID`, shared by every `{id}` write handler).
    #[tokio::test]
    async fn stop_bad_id_is_404() {
        let base = spawn(Arc::new(FakeProvider::ok(empty_snapshot()))).await;
        for bad in ["0", "abc"] {
            let resp = do_action(
                &format!("{base}/api/v1/runs/{bad}/stop"),
                reqwest::Method::POST,
            )
            .await;
            assert_eq!(resp.status(), 404, "run id {bad} must 404");
            assert_eq!(err_code(resp).await, "not_found");
        }
    }
}
