//! handlers_drain — `GET`/`POST /api/v1/drain`: ask the daemon to settle before a restart
//! (STUDIO-880).
//!
//! **No Go v0.4.0 counterpart, and no capture fixture** — the same additive shape
//! `/api/v1/capabilities`, `/api/v1/reviews` and `/api/v1/teams/*` established. Nothing is added to
//! a parity-checked view and no golden moves. (`/api/v1/state` does gain a `drain` key, but only
//! while a drain is armed — see [`rhapsody_orchestrator::snapshot_json`].)
//!
//! # What a drain is, at this door
//!
//! `POST {"active": true}` stops new dispatch immediately and lets in-flight runs finish their
//! current turn. It **interrupts nothing and kills nothing**, so it is safe to ask for at any time;
//! the worst case of a mistaken drain is that the daemon takes no new work until it is cancelled
//! with `POST {"active": false}`.
//!
//! # This endpoint owns no budget, and that is deliberate
//!
//! It does not wait for `counts.running` to reach zero and it has no timeout. Whoever asked for the
//! drain does the waiting, because the only thing a budget could do from in here — on expiry —
//! is interrupt the work the drain exists to protect. The caller polls `/api/v1/state`'s
//! `counts.running` (with the "a probe that failed is not idle" bias STUDIO-551 established) and
//! decides for itself what an expired wait means.
//!
//! # Authentication
//!
//! The loopback listener itself, exactly as for `POST /api/v1/config` and the stop/resume actions:
//! the server binds loopback only, so reaching this route already means being on the operator's
//! machine.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{Method, StatusCode};
use axum::response::Response;
use rhapsody_orchestrator::drain::{DrainReason, DrainStatus};
use serde::Deserialize;

use crate::handlers::require_get;
use crate::responses::{write_error, write_json};
use crate::server::StateProvider;

/// A drain request carries two small scalars; anything larger is a malformed client, not a request.
/// Mirrors [`crate::handlers_reviews`]'s `MAX_CONTROL_BODY`.
const MAX_CONTROL_BODY: usize = 8 << 10;

/// The `POST` body. `active` is required — there is no sensible default for "arm or cancel?", and
/// guessing one would make a malformed request silently do the opposite of what was meant.
#[derive(Debug, Deserialize)]
struct DrainReq {
    active: bool,
    /// Who is asking (`operator` / `update`); annotation only, and it defaults to `operator`.
    /// Ignored when `active` is false, and ignored when a drain is already armed — re-arming never
    /// rewrites the drain that is running.
    #[serde(default)]
    reason: String,
}

/// `GET /api/v1/drain` — the drain's current state, or `POST /api/v1/drain` — arm or cancel it.
///
/// One route for both because they are one resource: the `POST` answers exactly what the `GET`
/// would, so a caller that arms a drain never has to follow up with a read to learn what it got.
pub(crate) async fn handle_drain(
    method: Method,
    State(provider): State<Arc<dyn StateProvider>>,
    body: Bytes,
) -> Response {
    if method == Method::POST {
        return post(provider.as_ref(), &body);
    }
    // GET/HEAD read the state; anything else is a 405 naming both verbs.
    if let Some(resp) = require_get(&method) {
        return resp;
    }
    write_json(StatusCode::OK, &status_json(&provider.drain_status()))
}

/// The write half. Every refusal here is a malformed REQUEST (400) — there is no daemon state in
/// which a drain cannot be asked for, which is why nothing answers 409.
fn post(provider: &dyn StateProvider, body: &Bytes) -> Response {
    if body.len() > MAX_CONTROL_BODY {
        return write_error(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "request body is too large for a drain request",
            None,
        );
    }
    let req: DrainReq = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(e) => {
            return write_error(
                StatusCode::BAD_REQUEST,
                "bad_request",
                format!("invalid JSON body: {e}"),
                None,
            );
        }
    };
    let status = provider.set_drain(req.active, DrainReason::parse(req.reason.trim()));
    write_json(StatusCode::OK, &status_json(&status))
}

/// The wire shape, shared by both verbs and matching the `drain` key `/api/v1/state` renders while a
/// drain is armed — so a client reads the same three fields wherever it finds them.
fn status_json(s: &DrainStatus) -> serde_json::Value {
    serde_json::json!({
        "active": s.active,
        "reason": s.reason.as_str(),
        // RFC3339 or `""` when there is no drain, the timestamp convention every other field on this
        // API uses (never null).
        "requested_at": s
            .requested_at
            .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
            .unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    //! The route end to end over a real loopback listener, against a provider holding a REAL drain
    //! signal — so what is asserted is that a `POST` actually changes what the next `GET` reports,
    //! not that a canned field is echoed back. The drain's own semantics (idempotent arming, the
    //! dispatch gate, the turn boundary) belong to `rhapsody_orchestrator::drain` and are tested
    //! there against a real control loop.

    use std::sync::Arc;

    use serde_json::Value;

    use super::*;
    use crate::server::new_handler;
    use crate::testutil::{FakeProvider, empty_snapshot, spawn_router};

    async fn spawn() -> String {
        spawn_router(new_handler(
            Arc::new(FakeProvider::ok(empty_snapshot())),
            None,
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

    // The whole round trip: a daemon starts un-drained, a POST arms it, the GET agrees, and a second
    // POST cancels it. The GET reading back what the POST did is the property a waiter depends on.
    #[tokio::test]
    async fn arming_and_cancelling_a_drain_round_trips() {
        let base = spawn().await;
        let url = format!("{base}/api/v1/drain");

        let before = body_json(reqwest::get(&url).await.expect("GET")).await;
        assert_eq!(before["active"], false);
        assert_eq!(
            before["requested_at"], "",
            "an un-drained daemon reports no timestamp, as the empty string (never null)"
        );

        let armed = post(&url, r#"{"active":true,"reason":"update"}"#).await;
        assert_eq!(armed.status(), 200);
        let armed = body_json(armed).await;
        assert_eq!(armed["active"], true);
        assert_eq!(armed["reason"], "update");
        assert!(
            !armed["requested_at"]
                .as_str()
                .unwrap_or_default()
                .is_empty(),
            "an armed drain reports when it was asked for"
        );

        let read_back = body_json(reqwest::get(&url).await.expect("GET")).await;
        assert_eq!(
            read_back, armed,
            "the GET must report exactly what the POST answered"
        );

        let cancelled = body_json(post(&url, r#"{"active":false}"#).await).await;
        assert_eq!(cancelled["active"], false);
        assert_eq!(
            body_json(reqwest::get(&url).await.expect("GET")).await["active"],
            false
        );
    }

    // `reason` is an optional annotation, and an unrecognized one must never refuse a drain that was
    // genuinely asked for — the request's POINT is `active`, and the label is decoration on it.
    #[tokio::test]
    async fn an_absent_or_unknown_reason_still_arms_the_drain() {
        let base = spawn().await;
        let url = format!("{base}/api/v1/drain");
        let armed = body_json(post(&url, r#"{"active":true}"#).await).await;
        assert_eq!(armed["active"], true);
        assert_eq!(armed["reason"], "operator", "the default annotation");

        post(&url, r#"{"active":false}"#).await;
        let armed = body_json(post(&url, r#"{"active":true,"reason":"nonsense"}"#).await).await;
        assert_eq!(armed["active"], true, "a typo must not refuse a drain");
        assert_eq!(armed["reason"], "operator");
    }

    // `active` is required: there is no sensible default for "arm or cancel?", and guessing one
    // would let a malformed request silently do the opposite of what was meant.
    #[tokio::test]
    async fn a_body_without_active_is_rejected_and_changes_nothing() {
        let base = spawn().await;
        let url = format!("{base}/api/v1/drain");
        let resp = post(&url, r#"{"reason":"update"}"#).await;
        assert_eq!(resp.status(), 400);
        assert_eq!(body_json(resp).await["error"]["code"], "bad_request");
        assert_eq!(
            body_json(reqwest::get(&url).await.expect("GET")).await["active"],
            false,
            "a rejected request must not have armed anything"
        );
    }

    // Malformed JSON and an oversized body are both 400s, and neither changes the drain.
    #[tokio::test]
    async fn a_malformed_or_oversized_body_is_rejected() {
        let base = spawn().await;
        let url = format!("{base}/api/v1/drain");
        assert_eq!(post(&url, "not json").await.status(), 400);

        let huge = format!(
            r#"{{"active":true,"reason":"{}"}}"#,
            "r".repeat(MAX_CONTROL_BODY)
        );
        let resp = post(&url, &huge).await;
        assert_eq!(resp.status(), 400);
        assert_eq!(body_json(resp).await["error"]["code"], "bad_request");
        assert_eq!(
            body_json(reqwest::get(&url).await.expect("GET")).await["active"],
            false
        );
    }

    // The route is registered method-agnostically so a mismatch yields a real 405 rather than the
    // SPA fallback swallowing it into a 200 HTML page (see the crate's routing convention).
    #[tokio::test]
    async fn an_unsupported_method_gets_a_405_not_the_spa_fallback() {
        let base = spawn().await;
        let resp = reqwest::Client::new()
            .delete(format!("{base}/api/v1/drain"))
            .send()
            .await
            .expect("DELETE");
        assert_eq!(resp.status(), 405);
    }
}
