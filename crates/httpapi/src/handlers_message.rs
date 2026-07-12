//! handlers_message — the operator-message handlers: `POST /api/v1/runs/{id}/message` (queue a
//! "btw" for a live run's agent) and `GET /api/v1/runs/{id}/messages` (list a run's messages with
//! their delivery status). Parity port of `$REF/internal/httpapi/handlers_message.go`
//! (`handleRunMessage`/`handleRunMessages`/`runMessageReqJSON`/`runMessageJSON`), INF-250.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{Method, StatusCode};
use axum::response::Response;
use rhapsody_store::{RUN_MESSAGE_SENT, RunMessage};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::handlers::{require_get, require_post};
use crate::handlers_runaction::parse_run_id;
use crate::responses::{write_error, write_json};
use crate::server::StateProvider;

/// Bounds an operator message; anything longer is a likely paste accident or abuse and is rejected
/// with 400 (Go `maxOperatorMessageLen`, INF-250).
const MAX_OPERATOR_MESSAGE_LEN: usize = 4000;

/// The POST request body (Go `runMessageReqJSON`). `text` defaults to empty when absent, so a body
/// with no `text` falls through to the same `empty_text` rejection as `{"text":""}` (Go decodes into
/// a zero-value struct). Unknown fields are ignored (Go does not `DisallowUnknownFields` here).
#[derive(Deserialize)]
struct RunMessageReq {
    #[serde(default)]
    text: String,
}

/// `POST /api/v1/runs/{id}/message` — queue an operator "btw" for a live run's agent. 202 on accept;
/// 404 bad/unknown run id; 409 `not_running` / `backlog_full`; 400 `bad_json` / `empty_text` /
/// `text_too_long`. Mirrors Go `handleRunMessage`.
pub(crate) async fn handle_run_message(
    method: Method,
    Path(id): Path<String>,
    State(provider): State<Arc<dyn StateProvider>>,
    body: Bytes,
) -> Response {
    if let Some(resp) = require_post(&method, "use POST to message a run") {
        return resp;
    }
    let run_id = match parse_run_id(&id) {
        Ok(run_id) => run_id,
        Err(resp) => return *resp,
    };
    let req: RunMessageReq = match serde_json::from_slice(&body) {
        Ok(req) => req,
        Err(err) => {
            return write_error(StatusCode::BAD_REQUEST, "bad_json", err.to_string(), None);
        }
    };
    let text = req.text.trim();
    if text.is_empty() {
        return write_error(
            StatusCode::BAD_REQUEST,
            "empty_text",
            "message text is required",
            None,
        );
    }
    // Count CHARACTERS (runes), not bytes, so the limit matches the composer's character-based
    // maxLength rather than rejecting a CJK/emoji message the UI accepted (Go `utf8.RuneCountInString`).
    if text.chars().count() > MAX_OPERATOR_MESSAGE_LEN {
        return write_error(
            StatusCode::BAD_REQUEST,
            "text_too_long",
            "message exceeds 4000 characters",
            None,
        );
    }
    let res = provider.send_run_message(run_id, text).await;
    if res.not_running {
        return write_error(
            StatusCode::CONFLICT,
            "not_running",
            "run is not currently running",
            None,
        );
    }
    if res.full {
        return write_error(
            StatusCode::CONFLICT,
            "backlog_full",
            "too many pending operator messages for this run",
            None,
        );
    }
    write_json(
        StatusCode::ACCEPTED,
        &json!({ "id": res.id, "identifier": res.identifier, "status": RUN_MESSAGE_SENT }),
    )
}

/// `GET /api/v1/runs/{id}/messages` — the run's operator messages with their delivery status, read
/// straight from the store (like the history handlers). Always a JSON array (never null) so the UI
/// renders an empty timeline rather than choking on null. Mirrors Go `handleRunMessages`.
pub(crate) async fn handle_run_messages(
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
    match provider.history().list_run_messages(run_id) {
        Ok(msgs) => write_json(
            StatusCode::OK,
            &Value::Array(msgs.iter().map(run_message_json).collect()),
        ),
        Err(err) => write_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "list_messages_failed",
            err.to_string(),
            None,
        ),
    }
}

/// One store `RunMessage` on the wire (Go `store.RunMessage` json tags): `delivered_turn` is omitted
/// when unset (`omitempty`); everything else is always present.
fn run_message_json(m: &RunMessage) -> Value {
    let mut out = Map::new();
    out.insert("id".into(), Value::from(m.id));
    out.insert("run_id".into(), Value::from(m.run_id));
    out.insert("body".into(), Value::from(m.body.clone()));
    out.insert("created_at_ms".into(), Value::from(m.created_at_ms));
    out.insert("status".into(), Value::from(m.status.clone()));
    if let Some(turn) = m.delivered_turn {
        out.insert("delivered_turn".into(), Value::from(turn));
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rhapsody_store::{Sqlite, Store, StorePath};
    use serde_json::Value;

    use crate::new_handler;
    use crate::testutil::{FakeProvider, empty_snapshot, spawn_router};

    async fn spawn(provider: Arc<FakeProvider>) -> String {
        spawn_router(new_handler(provider, None)).await
    }

    async fn post_message(url: &str, body: &str) -> reqwest::Response {
        reqwest::Client::new()
            .post(url)
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .expect("POST message")
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

    // Mirrors Go `TestHandleRunMessage_PostAccepted`: 202, the body echoes id/identifier/status:sent,
    // and the handler forwards the parsed run id + the TRIMMED text.
    #[tokio::test]
    async fn message_post_accepted() {
        let provider = Arc::new(FakeProvider::ok(empty_snapshot()).with_message_result(
            rhapsody_orchestrator::RunMessageResult {
                id: 11,
                identifier: "INF-250".into(),
                ..Default::default()
            },
        ));
        let base = spawn(provider.clone()).await;
        let resp = post_message(
            &format!("{base}/api/v1/runs/7/message"),
            r#"{"text":"  watch the branch  "}"#,
        )
        .await;
        assert_eq!(resp.status(), 202);
        let body = body_json(resp).await;
        assert_eq!(body["id"], 11);
        assert_eq!(body["identifier"], "INF-250");
        assert_eq!(body["status"], "sent");
        assert_eq!(provider.message_run_id(), 7);
        assert_eq!(
            provider.message_text(),
            "watch the branch",
            "text trimmed before dispatch"
        );
    }

    // Mirrors Go `TestHandleRunMessage_GetIs405`.
    #[tokio::test]
    async fn message_get_is_405() {
        let base = spawn(Arc::new(FakeProvider::ok(empty_snapshot()))).await;
        let resp = reqwest::Client::new()
            .get(format!("{base}/api/v1/runs/7/message"))
            .send()
            .await
            .expect("GET message");
        assert_eq!(resp.status(), 405);
        assert_eq!(
            resp.headers().get("allow").and_then(|v| v.to_str().ok()),
            Some("POST")
        );
    }

    // Mirrors Go `TestHandleRunMessage_StatusTable`: the full 404/400/409 matrix.
    #[tokio::test]
    async fn message_status_table() {
        let long = format!(r#"{{"text":"{}"}}"#, "x".repeat(4001));
        let cases: Vec<(
            &str,
            &str,
            rhapsody_orchestrator::RunMessageResult,
            u16,
            &str,
        )> = vec![
            (
                "/api/v1/runs/0/message",
                r#"{"text":"hi"}"#,
                Default::default(),
                404,
                "not_found",
            ),
            (
                "/api/v1/runs/7/message",
                r#"{"text":"   "}"#,
                Default::default(),
                400,
                "empty_text",
            ),
            (
                "/api/v1/runs/7/message",
                &long,
                Default::default(),
                400,
                "text_too_long",
            ),
            (
                "/api/v1/runs/7/message",
                "{not json",
                Default::default(),
                400,
                "bad_json",
            ),
            (
                "/api/v1/runs/7/message",
                r#"{"text":"hi"}"#,
                rhapsody_orchestrator::RunMessageResult {
                    not_running: true,
                    ..Default::default()
                },
                409,
                "not_running",
            ),
            (
                "/api/v1/runs/7/message",
                r#"{"text":"hi"}"#,
                rhapsody_orchestrator::RunMessageResult {
                    full: true,
                    ..Default::default()
                },
                409,
                "backlog_full",
            ),
        ];
        for (path, body, result, want_status, want_code) in cases {
            let base = spawn(Arc::new(
                FakeProvider::ok(empty_snapshot()).with_message_result(result),
            ))
            .await;
            let resp = post_message(&format!("{base}{path}"), body).await;
            assert_eq!(resp.status(), want_status, "case {want_code}: status");
            assert_eq!(err_code(resp).await, want_code, "case {want_code}: code");
        }
    }

    // Mirrors Go `TestHandleRunMessages_GetListsFromStore`: the GET reads messages + delivery status
    // straight from a seeded store.
    #[tokio::test]
    async fn messages_get_lists_from_store() {
        let store = Sqlite::open(StorePath::InMemory).expect("open in-memory store");
        store
            .insert_run_message(7, "first", 1000)
            .expect("insert first");
        store
            .insert_run_message(7, "second", 2000)
            .expect("insert second");
        store
            .mark_oldest_run_message_delivered(7, 3)
            .expect("mark delivered");

        let base = spawn(Arc::new(
            FakeProvider::ok(empty_snapshot()).with_history(Arc::new(store)),
        ))
        .await;
        let resp = reqwest::get(format!("{base}/api/v1/runs/7/messages"))
            .await
            .expect("GET messages");
        assert_eq!(resp.status(), 200);
        let msgs = body_json(resp).await;
        let msgs = msgs.as_array().expect("array");
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["body"], "first");
        assert_eq!(msgs[0]["status"], "delivered");
        assert_eq!(msgs[0]["delivered_turn"], 3);
        assert_eq!(msgs[1]["body"], "second");
        assert_eq!(msgs[1]["status"], "sent");
        assert!(
            msgs[1].get("delivered_turn").is_none(),
            "an undelivered message omits delivered_turn"
        );
    }

    // Mirrors Go `TestHandleRunMessages_EmptyIsArray`: a run with no messages returns [] (never null).
    #[tokio::test]
    async fn messages_empty_is_array() {
        let base = spawn(Arc::new(FakeProvider::ok(empty_snapshot()))).await; // Noop store
        let resp = reqwest::get(format!("{base}/api/v1/runs/7/messages"))
            .await
            .expect("GET messages");
        assert_eq!(resp.status(), 200);
        let text = resp.text().await.expect("body");
        assert_eq!(text.trim(), "[]");
    }
}
