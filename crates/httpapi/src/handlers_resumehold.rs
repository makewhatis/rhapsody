//! handlers_resumehold — the operator's "Human step done → resume" action (STUDIO-1053).
//!
//! * `GET  /api/v1/runs/{id}/hold` (read-only): whether the run's ticket is held with
//!   `rhapsody:human`, and — when the durable breaker crossing row names one — the crossed limit,
//!   so the job page can show the action and its confirmation without guessing.
//! * `POST /api/v1/runs/{id}/resume-hold` (operator-guarded): remove the hold, record the operator's
//!   note for the next run, and requeue the ticket.
//!
//! Both are Rhapsody-only (no Go v0.4.0 counterpart). The POST follows the crate's "business
//! outcomes are not errors" convention: `not_found` → 404, `not_held` → 409, a failed requeue MOVE
//! → 200 with `queued:false` + `move_error`; only a failed label REMOVAL is an error status (502
//! `label_removal_failed`), because that is the one step whose failure means nothing was committed.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{Method, StatusCode};
use axum::response::Response;
use rhapsody_orchestrator::{BreakerHold, ResumeHoldError, ResumeHoldResult, RunHoldView};
use serde::Deserialize;
use serde::Serialize;

use crate::handlers::{require_get, require_post};
use crate::handlers_runaction::parse_run_id;
use crate::responses::{write_error, write_json};
use crate::server::StateProvider;

/// Bounds the operator's note; anything longer is a paste accident or abuse. Matches the room/message
/// composer bound, which is the same "a human typed this into a box" input.
const MAX_NOTE_LEN: usize = 4000;

/// The POST request body. `note` defaults to empty, so an absent field falls through to the same
/// `empty_note` rejection as `{"note":""}`.
#[derive(Deserialize)]
struct ResumeHoldReq {
    #[serde(default)]
    note: String,
}

/// The breaker limit on the wire. `rounds` is `0` and `providers` empty on a hold the breaker did
/// not apply; the whole key is omitted then.
#[derive(Serialize)]
struct BreakerHoldJson {
    rounds: i64,
    providers: Vec<String>,
}

impl From<&BreakerHold> for BreakerHoldJson {
    fn from(b: &BreakerHold) -> Self {
        BreakerHoldJson {
            rounds: b.rounds,
            providers: b.providers.clone(),
        }
    }
}

/// The 200 body of `POST /api/v1/runs/{id}/resume-hold`.
#[derive(Serialize)]
struct ResumeHoldJson {
    identifier: String,
    note_recorded: bool,
    label_removed: bool,
    queued: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    moved_to: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    move_error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    breaker: Option<BreakerHoldJson>,
}

impl From<ResumeHoldResult> for ResumeHoldJson {
    fn from(res: ResumeHoldResult) -> Self {
        ResumeHoldJson {
            identifier: res.identifier,
            note_recorded: res.note_recorded,
            label_removed: res.label_removed,
            queued: res.queued,
            moved_to: res.moved_to,
            move_error: res.move_err,
            breaker: res.breaker.as_ref().map(BreakerHoldJson::from),
        }
    }
}

/// The 200 body of `GET /api/v1/runs/{id}/hold`.
#[derive(Serialize)]
struct RunHoldJson {
    identifier: String,
    held: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    breaker: Option<BreakerHoldJson>,
}

impl From<RunHoldView> for RunHoldJson {
    fn from(view: RunHoldView) -> Self {
        RunHoldJson {
            identifier: view.identifier,
            held: view.held,
            breaker: view.breaker.as_ref().map(BreakerHoldJson::from),
        }
    }
}

/// `GET /api/v1/runs/{id}/hold` — read-only. 404 `run_not_found` for a bad/unknown run id; 500
/// `hold_read_failed` when the tracker read fails. Mirrors the run-action handlers' envelope shape.
pub(crate) async fn handle_run_hold(
    method: Method,
    Path(id): Path<String>,
    State(provider): State<Arc<dyn StateProvider>>,
) -> Response {
    if let Some(resp) = require_get(&method) {
        return resp;
    }
    let run_id = match parse_run_id(&id) {
        Ok(run_id) => run_id,
        Err(resp) => return *resp,
    };
    match provider.run_hold(run_id).await {
        Err(err) => write_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "hold_read_failed",
            err.to_string(),
            None,
        ),
        Ok(view) if view.not_found => write_error(
            StatusCode::NOT_FOUND,
            "run_not_found",
            "no run with that id",
            None,
        ),
        Ok(view) => write_json(StatusCode::OK, &RunHoldJson::from(view)),
    }
}

/// `POST /api/v1/runs/{id}/resume-hold` — the "Human step done → resume" action. 404 `run_not_found`;
/// 409 `not_held`; 400 `empty_note` / `note_too_long` / `bad_json`; 502 `label_removal_failed`;
/// a failed requeue move is a 200 with `queued:false` and `move_error` (the note and the hold removal
/// DID land, and the body says exactly that rather than claiming success).
pub(crate) async fn handle_resume_hold(
    method: Method,
    Path(id): Path<String>,
    State(provider): State<Arc<dyn StateProvider>>,
    body: Bytes,
) -> Response {
    if let Some(resp) = require_post(&method, "use POST to resume a held ticket") {
        return resp;
    }
    let run_id = match parse_run_id(&id) {
        Ok(run_id) => run_id,
        Err(resp) => return *resp,
    };
    let req: ResumeHoldReq = match serde_json::from_slice(&body) {
        Ok(req) => req,
        Err(err) => {
            return write_error(StatusCode::BAD_REQUEST, "bad_json", err.to_string(), None);
        }
    };
    let note = req.note.trim();
    if note.is_empty() {
        return write_error(
            StatusCode::BAD_REQUEST,
            "empty_note",
            "a note describing what you did is required",
            None,
        );
    }
    if note.chars().count() > MAX_NOTE_LEN {
        return write_error(
            StatusCode::BAD_REQUEST,
            "note_too_long",
            "the note exceeds 4000 characters",
            None,
        );
    }
    match provider.resume_hold(run_id, note).await {
        Ok(res) if res.not_found => write_error(
            StatusCode::NOT_FOUND,
            "run_not_found",
            "no run with that id",
            None,
        ),
        Ok(res) if res.not_held => write_error(
            StatusCode::CONFLICT,
            "not_held",
            "this ticket is not held for a human; there is nothing to resume",
            None,
        ),
        Ok(res) => write_json(StatusCode::OK, &ResumeHoldJson::from(res)),
        // The one failure that commits NOTHING: the hold label could not be removed, so the note was
        // not recorded and the ticket was not requeued. It must never read as a partial success.
        Err(ResumeHoldError::LabelRemovalFailed(err)) => {
            write_error(StatusCode::BAD_GATEWAY, "label_removal_failed", err, None)
        }
        Err(err) => write_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "resume_hold_failed",
            err.to_string(),
            None,
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rhapsody_orchestrator::{BreakerHold, ResumeHoldResult, RunHoldView};
    use serde_json::Value;

    use crate::new_handler;
    use crate::testutil::{FakeProvider, FakeResumeHoldError, empty_snapshot, spawn_router};

    async fn spawn(provider: Arc<FakeProvider>) -> String {
        spawn_router(new_handler(provider, None)).await
    }

    async fn post(url: &str, body: &str) -> reqwest::Response {
        crate::testutil::operator_client()
            .post(url)
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .expect("POST resume-hold")
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

    // The happy path: 200 with the three facts the operator needs, the note forwarded trimmed, and
    // the breaker limit named.
    #[tokio::test]
    async fn resume_hold_post_reports_what_happened() {
        let provider = Arc::new(FakeProvider::ok(empty_snapshot()).with_resume_hold_result(
            ResumeHoldResult {
                identifier: "STUDIO-1050".into(),
                note_recorded: true,
                label_removed: true,
                queued: true,
                moved_to: "Todo".into(),
                breaker: Some(BreakerHold {
                    rounds: 5,
                    providers: vec!["anthropic".into()],
                }),
                ..Default::default()
            },
        ));
        let base = spawn(provider.clone()).await;
        let resp = post(
            &format!("{base}/api/v1/runs/7/resume-hold"),
            r#"{"note":"  added the secrets  "}"#,
        )
        .await;
        assert_eq!(resp.status(), 200);
        let body = body_json(resp).await;
        assert_eq!(body["identifier"], "STUDIO-1050");
        assert_eq!(body["note_recorded"], true);
        assert_eq!(body["label_removed"], true);
        assert_eq!(body["queued"], true);
        assert_eq!(body["moved_to"], "Todo");
        assert!(body.get("move_error").is_none(), "no move error on success");
        assert_eq!(body["breaker"]["rounds"], 5);
        assert_eq!(body["breaker"]["providers"][0], "anthropic");
        assert_eq!(provider.resume_hold_run_id(), 7);
        assert_eq!(
            provider.resume_hold_note(),
            "added the secrets",
            "the note is trimmed before it reaches the provider"
        );
    }

    // A ticket that is not held ⇒ 409 not_held, and no success fields.
    #[tokio::test]
    async fn resume_hold_post_refuses_an_unheld_ticket() {
        let provider = Arc::new(FakeProvider::ok(empty_snapshot()).with_resume_hold_result(
            ResumeHoldResult {
                not_held: true,
                identifier: "STUDIO-1051".into(),
                ..Default::default()
            },
        ));
        let base = spawn(provider).await;
        let resp = post(
            &format!("{base}/api/v1/runs/7/resume-hold"),
            r#"{"note":"done"}"#,
        )
        .await;
        assert_eq!(resp.status(), 409);
        assert_eq!(err_code(resp).await, "not_held");
    }

    // An unknown run id ⇒ 404.
    #[tokio::test]
    async fn resume_hold_post_unknown_run_is_404() {
        let provider = Arc::new(FakeProvider::ok(empty_snapshot()).with_resume_hold_result(
            ResumeHoldResult {
                not_found: true,
                ..Default::default()
            },
        ));
        let base = spawn(provider).await;
        let resp = post(
            &format!("{base}/api/v1/runs/7/resume-hold"),
            r#"{"note":"done"}"#,
        )
        .await;
        assert_eq!(resp.status(), 404);
        assert_eq!(err_code(resp).await, "run_not_found");
    }

    // The note is required: an empty (or whitespace) one is 400 empty_note and never reaches the
    // provider.
    #[tokio::test]
    async fn resume_hold_post_requires_a_note() {
        let provider = Arc::new(FakeProvider::ok(empty_snapshot()));
        let base = spawn(provider.clone()).await;
        for body in [r#"{"note":""}"#, r#"{"note":"   "}"#, "{}"] {
            let resp = post(&format!("{base}/api/v1/runs/7/resume-hold"), body).await;
            assert_eq!(resp.status(), 400, "body {body}");
            assert_eq!(err_code(resp).await, "empty_note", "body {body}");
        }
        assert_eq!(
            provider.resume_hold_run_id(),
            0,
            "an empty note must not reach the provider"
        );
    }

    // A rejected label removal is a 502 and the body carries no success claim.
    #[tokio::test]
    async fn resume_hold_post_label_removal_failure_is_502() {
        let provider = Arc::new(FakeProvider::ok(empty_snapshot()).with_resume_hold_error(
            FakeResumeHoldError::LabelRemoval("linear_move_rejected".into()),
        ));
        let base = spawn(provider).await;
        let resp = post(
            &format!("{base}/api/v1/runs/7/resume-hold"),
            r#"{"note":"done"}"#,
        )
        .await;
        assert_eq!(resp.status(), 502);
        let body = body_json(resp).await;
        assert_eq!(body["error"]["code"], "label_removal_failed");
        assert!(
            body.get("note_recorded").is_none(),
            "no success claim: {body}"
        );
    }

    // A tracker/store read failure on the action is a 500 (not a business outcome).
    #[tokio::test]
    async fn resume_hold_post_read_failure_is_500() {
        let provider = Arc::new(
            FakeProvider::ok(empty_snapshot())
                .with_resume_hold_error(FakeResumeHoldError::Other("tracker down".into())),
        );
        let base = spawn(provider).await;
        let resp = post(
            &format!("{base}/api/v1/runs/7/resume-hold"),
            r#"{"note":"done"}"#,
        )
        .await;
        assert_eq!(resp.status(), 500);
        assert_eq!(err_code(resp).await, "resume_hold_failed");
    }

    // GET is 405 with Allow: POST.
    #[tokio::test]
    async fn resume_hold_get_is_405() {
        let base = spawn(Arc::new(FakeProvider::ok(empty_snapshot()))).await;
        let resp = crate::testutil::operator_client()
            .get(format!("{base}/api/v1/runs/7/resume-hold"))
            .send()
            .await
            .expect("GET");
        assert_eq!(resp.status(), 405);
        assert_eq!(
            resp.headers().get("allow").and_then(|v| v.to_str().ok()),
            Some("POST")
        );
    }

    // The read: held + the crossed limit.
    #[tokio::test]
    async fn run_hold_get_reports_the_hold_and_breaker() {
        let provider = Arc::new(FakeProvider::ok(empty_snapshot()).with_run_hold_view(
            RunHoldView {
                not_found: false,
                held: true,
                identifier: "STUDIO-988".into(),
                breaker: Some(BreakerHold {
                    rounds: 10,
                    providers: Vec::new(),
                }),
            },
        ));
        let base = spawn(provider.clone()).await;
        let resp = reqwest::get(format!("{base}/api/v1/runs/7/hold"))
            .await
            .expect("GET hold");
        assert_eq!(resp.status(), 200);
        let body = body_json(resp).await;
        assert_eq!(body["identifier"], "STUDIO-988");
        assert_eq!(body["held"], true);
        assert_eq!(body["breaker"]["rounds"], 10);
        assert_eq!(provider.run_hold_run_id(), 7);
    }

    // An unknown run id on the read ⇒ 404.
    #[tokio::test]
    async fn run_hold_get_unknown_is_404() {
        let provider = Arc::new(FakeProvider::ok(empty_snapshot()).with_run_hold_view(
            RunHoldView {
                not_found: true,
                ..Default::default()
            },
        ));
        let base = spawn(provider).await;
        let resp = reqwest::get(format!("{base}/api/v1/runs/7/hold"))
            .await
            .expect("GET hold");
        assert_eq!(resp.status(), 404);
        assert_eq!(err_code(resp).await, "run_not_found");
    }

    // A failed read on the GET ⇒ 500.
    #[tokio::test]
    async fn run_hold_get_read_failure_is_500() {
        let provider = Arc::new(
            FakeProvider::ok(empty_snapshot())
                .with_run_hold_error(FakeResumeHoldError::Other("tracker down".into())),
        );
        let base = spawn(provider).await;
        let resp = reqwest::get(format!("{base}/api/v1/runs/7/hold"))
            .await
            .expect("GET hold");
        assert_eq!(resp.status(), 500);
        assert_eq!(err_code(resp).await, "hold_read_failed");
    }
}
