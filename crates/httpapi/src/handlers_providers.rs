//! handlers_providers — the provider status and model-catalog routes (STUDIO-990, P9; design §P9/§6).
//! Rhapsody-only: the frozen Go daemon has no provider concept, so these routes are an ADDITIVE
//! divergence (README "Divergences").
//!
//! ```text
//! GET  /api/v1/providers                        → { providers: [ status view, … ] }  cache-only
//! GET  /api/v1/providers/{id}/models            → CatalogSnapshot                     cache-only
//! POST /api/v1/providers/{id}/models/refresh    → CatalogSnapshot                     operator-only
//! ```
//!
//! The two `GET` reads touch no owner and no provider — they render the daemon's non-secret cache.
//! The POST is the ONE credentialed catalog operation, and it is deliberately hard to reach: it is
//! registered behind the shared operator-write guard (Host/Origin/custom-header/cookie/form rules,
//! STUDIO-982) and its body must be a closed empty JSON object of at most
//! [`MAX_REFRESH_BODY_BYTES`] bytes. A response never contains a credential, a stored binding, a
//! credential revision, or a raw provider body.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{Method, StatusCode};
use axum::response::Response;
use serde::Serialize;

use crate::handlers::{require_get, require_post};
use crate::responses::{write_error, write_json};
use crate::server::StateProvider;

/// The closed body ceiling for the credentialed refresh POST (design §2.6): an explicit operator
/// operation carries `{}` and nothing else. A larger body is refused before anything is parsed.
pub(crate) const MAX_REFRESH_BODY_BYTES: usize = 1024;

/// The `GET /api/v1/providers` body. `providers` is the non-secret status list, in provider-id order.
#[derive(Serialize)]
struct ProviderListBody {
    providers: Vec<rhapsody_provider_status::ProviderStatusView>,
}

/// `GET /api/v1/providers` — the cache-only provider status list. Never opens Keychain, performs IPC,
/// shells `opencode models`, or contacts a provider.
pub(crate) async fn handle_providers(
    method: Method,
    State(provider): State<Arc<dyn StateProvider>>,
) -> Response {
    if let Some(resp) = require_get(&method) {
        return resp;
    }
    write_json(
        StatusCode::OK,
        &ProviderListBody {
            providers: provider.provider_statuses(),
        },
    )
}

/// `GET /api/v1/providers/{id}/models` — the cache-only model catalog for one provider. An unknown
/// provider id is a 404; a known provider with no cache yet answers an empty, aged-unknown body.
pub(crate) async fn handle_provider_models(
    method: Method,
    Path(id): Path<String>,
    State(provider): State<Arc<dyn StateProvider>>,
) -> Response {
    if let Some(resp) = require_get(&method) {
        return resp;
    }
    match provider.provider_catalog(&id) {
        Some(snapshot) => write_json(StatusCode::OK, &snapshot),
        None => write_error(
            StatusCode::NOT_FOUND,
            "provider_not_found",
            format!("no configured provider with id {id:?}"),
            None,
        ),
    }
}

/// `POST /api/v1/providers/{id}/models/refresh` — the explicit, bounded, credentialed catalog
/// refresh. Guarded by `operator_write` at the route layer; this handler additionally enforces the
/// closed empty-JSON body and the 1 KiB ceiling. A catalog failure is visible in the returned
/// snapshot's `error`, never fatal — manual model entry stays available.
pub(crate) async fn handle_provider_models_refresh(
    method: Method,
    Path(id): Path<String>,
    State(provider): State<Arc<dyn StateProvider>>,
    body: Bytes,
) -> Response {
    if let Some(resp) = require_post(&method, "use POST to refresh a provider model catalog") {
        return resp;
    }
    if body.len() > MAX_REFRESH_BODY_BYTES {
        return write_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "payload_too_large",
            format!("refresh body must be at most {MAX_REFRESH_BODY_BYTES} bytes"),
            None,
        );
    }
    if !is_empty_json_object(&body) {
        return write_error(
            StatusCode::BAD_REQUEST,
            "invalid_body",
            "refresh body must be an empty JSON object",
            None,
        );
    }
    match provider.refresh_provider_catalog(&id).await {
        Ok(snapshot) => write_json(StatusCode::OK, &snapshot),
        // The provider id is unknown: the same 404 the cache-only GET would give.
        Err(rhapsody_provider_status::CatalogError::Unsupported) => write_error(
            StatusCode::NOT_FOUND,
            "provider_not_found",
            format!("no configured provider with id {id:?}"),
            None,
        ),
        // A control-round-trip failure (never a business catalog failure, which rides in the
        // snapshot's `error` field that the Ok arm serves). Redacted and actionable.
        Err(error) => write_error(
            StatusCode::BAD_GATEWAY,
            "catalog_refresh_failed",
            error.message(),
            None,
        ),
    }
}

/// Whether `body` is exactly the closed empty JSON object `{}` (whitespace tolerated). Anything else
/// — a non-object, a key, a scalar, or invalid JSON — is refused.
fn is_empty_json_object(body: &[u8]) -> bool {
    matches!(
        serde_json::from_slice::<serde_json::Value>(body),
        Ok(serde_json::Value::Object(map)) if map.is_empty()
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rhapsody_provider_status::{
        CatalogErrorCode, CatalogSnapshot, ModelEntry, ProviderStatusView,
    };

    use super::*;
    use crate::new_handler;
    use crate::operator_guard::{DENIED_CODE, OPERATOR_HEADER, OPERATOR_HEADER_VALUE};
    use crate::testutil::{FakeProvider, operator_client, spawn_router};

    fn status_view(id: &str, status: &'static str) -> ProviderStatusView {
        ProviderStatusView {
            provider_id: id.to_string(),
            status,
            cache_age_ms: Some(42),
            refreshing: false,
            broker_available: true,
            broker_reason: None,
            recovery: None,
        }
    }

    fn snapshot(id: &str) -> CatalogSnapshot {
        CatalogSnapshot {
            provider_id: id.to_string(),
            models: vec![ModelEntry {
                id: "gpt-4o".into(),
                display_name: None,
                capabilities: Vec::new(),
            }],
            truncated: false,
            cache_age_ms: Some(7),
            error: None,
            error_message: None,
            manual_entry_allowed: true,
        }
    }

    async fn serve(fake: FakeProvider) -> (String, Arc<FakeProvider>) {
        let fake = Arc::new(fake);
        let base = spawn_router(new_handler(fake.clone(), None)).await;
        (base, fake)
    }

    // MUTATION GUARD (GET is cache-only): the status list and the catalog GET touch no credentialed
    // operation. A handler that triggered a refresh would record `provider_refresh_asked`.
    #[tokio::test]
    async fn the_two_gets_are_cache_only() {
        let fake = FakeProvider::ok(crate::testutil::empty_snapshot())
            .with_provider_statuses(vec![status_view("fireworks", "configured")])
            .with_provider_catalog(snapshot("fireworks"));
        let (base, provider) = serve(fake).await;
        let client = reqwest::Client::new();
        let list = client
            .get(format!("{base}/api/v1/providers"))
            .send()
            .await
            .expect("list");
        assert_eq!(list.status(), 200);
        let body: serde_json::Value = list.json().await.expect("json");
        assert_eq!(body["providers"][0]["provider_id"], "fireworks");
        assert_eq!(body["providers"][0]["status"], "configured");

        let models = client
            .get(format!("{base}/api/v1/providers/fireworks/models"))
            .send()
            .await
            .expect("models");
        assert_eq!(models.status(), 200);
        let body: serde_json::Value = models.json().await.expect("json");
        assert_eq!(body["models"][0]["id"], "gpt-4o");
        assert_eq!(body["manual_entry_allowed"], true);

        assert_eq!(provider.provider_refresh_asked(), None, "a GET refreshed");
    }

    #[tokio::test]
    async fn an_unknown_catalog_id_is_404() {
        let (base, _provider) = serve(FakeProvider::ok(crate::testutil::empty_snapshot())).await;
        let resp = reqwest::Client::new()
            .get(format!("{base}/api/v1/providers/nope/models"))
            .send()
            .await
            .expect("models");
        assert_eq!(resp.status(), 404);
        let body: serde_json::Value = resp.json().await.expect("json");
        assert_eq!(body["error"]["code"], "provider_not_found");
    }

    // The refresh POST is the one credentialed operation, and every part of its contract holds: the
    // operator guard first, then the 1 KiB ceiling, then the closed empty-object body, then the
    // forwarded id.
    #[tokio::test]
    async fn refresh_requires_operator_guard_and_a_closed_empty_body() {
        let fake = FakeProvider::ok(crate::testutil::empty_snapshot())
            .with_provider_refresh_result(snapshot("fireworks"));
        let (base, provider) = serve(fake).await;
        let url = format!("{base}/api/v1/providers/fireworks/models/refresh");

        // No operator header ⇒ the shared guard's 403, before any provider call.
        let denied = reqwest::Client::new()
            .post(&url)
            .header("content-type", "application/json")
            .body("{}")
            .send()
            .await
            .expect("denied");
        assert_eq!(denied.status(), 403);
        let body: serde_json::Value = denied.json().await.expect("json");
        assert_eq!(body["error"]["code"], DENIED_CODE);
        assert_eq!(provider.provider_refresh_asked(), None);

        let client = operator_client();
        // A non-empty body is refused.
        let invalid = client
            .post(&url)
            .header("content-type", "application/json")
            .body("{\"x\":1}")
            .send()
            .await
            .expect("invalid");
        assert_eq!(invalid.status(), 400);
        let body: serde_json::Value = invalid.json().await.expect("json");
        assert_eq!(body["error"]["code"], "invalid_body");

        // Over the 1 KiB ceiling is refused before parsing.
        let huge = client
            .post(&url)
            .header("content-type", "application/json")
            .body(" ".repeat(MAX_REFRESH_BODY_BYTES + 1))
            .send()
            .await
            .expect("huge");
        assert_eq!(huge.status(), 413);

        // The operator's own closed `{}` reaches the provider and forwards the path id.
        let ok = client
            .post(&url)
            .header("content-type", "application/json")
            .body("{}")
            .send()
            .await
            .expect("ok");
        assert_eq!(ok.status(), 200);
        assert_eq!(
            provider.provider_refresh_asked().as_deref(),
            Some("fireworks")
        );
    }

    #[tokio::test]
    async fn a_refresh_for_an_unknown_provider_is_404() {
        let (base, _provider) = serve(FakeProvider::ok(crate::testutil::empty_snapshot())).await;
        let resp = operator_client()
            .post(format!("{base}/api/v1/providers/nope/models/refresh"))
            .header("content-type", "application/json")
            .body("{}")
            .send()
            .await
            .expect("post");
        assert_eq!(resp.status(), 404);
    }

    // The response scan: no credential, stored binding, credential revision, or raw provider body
    // may cross this boundary. A real credential can never appear because no DTO field can hold one;
    // this pins that the forbidden keys/values are absent from the wire.
    #[tokio::test]
    async fn no_provider_response_exposes_forbidden_keys() {
        let fake = FakeProvider::ok(crate::testutil::empty_snapshot())
            .with_provider_statuses(vec![status_view("fireworks", "binding_mismatch")])
            .with_provider_catalog(snapshot("fireworks"))
            .with_provider_refresh_result(snapshot("fireworks"));
        let (base, _provider) = serve(fake).await;
        let client = operator_client();
        for (method, url) in [
            ("GET", format!("{base}/api/v1/providers")),
            ("GET", format!("{base}/api/v1/providers/fireworks/models")),
        ] {
            let resp = client
                .request(reqwest::Method::from_bytes(method.as_bytes()).unwrap(), url)
                .send()
                .await
                .expect("send");
            scan(resp).await;
        }
        let refreshed = client
            .post(format!("{base}/api/v1/providers/fireworks/models/refresh"))
            .header("content-type", "application/json")
            .body("{}")
            .send()
            .await
            .expect("refresh");
        scan(refreshed).await;
    }

    // The status wire carries the non-secret broker availability and its CLOSED reason code, and
    // nothing else about the broker (no listener address, capability, or credential). An
    // unavailable broker is `broker_available: false` + `provider_broker_unavailable`.
    #[tokio::test]
    async fn the_status_body_carries_broker_availability_and_a_closed_reason() {
        let down = ProviderStatusView {
            broker_available: false,
            broker_reason: Some(rhapsody_provider_status::BROKER_UNAVAILABLE),
            ..status_view("fireworks", "configured")
        };
        let fake =
            FakeProvider::ok(crate::testutil::empty_snapshot()).with_provider_statuses(vec![down]);
        let (base, _provider) = serve(fake).await;
        let body: serde_json::Value = reqwest::Client::new()
            .get(format!("{base}/api/v1/providers"))
            .send()
            .await
            .expect("list")
            .json()
            .await
            .expect("json");
        assert_eq!(body["providers"][0]["broker_available"], false);
        assert_eq!(
            body["providers"][0]["broker_reason"],
            "provider_broker_unavailable"
        );
    }

    async fn scan(resp: reqwest::Response) {
        let text = resp.text().await.expect("body");
        for forbidden in [
            "\"credential\"",
            "\"binding\"",
            "\"fingerprint\"",
            "\"revision\"",
            "\"value\"",
            "\"key\"",
            "\"secret\"",
            "sk-",
            "Bearer ",
        ] {
            assert!(
                !text.contains(forbidden),
                "provider response leaked {forbidden:?}: {text}"
            );
        }
    }

    // A catalog failure is visible and actionable but never fatal: the refresh still answers 200 with
    // the error code in the snapshot, and manual entry stays available.
    #[tokio::test]
    async fn a_catalog_failure_is_visible_and_keeps_manual_entry() {
        let failing = CatalogSnapshot {
            provider_id: "fireworks".into(),
            models: Vec::new(),
            truncated: false,
            cache_age_ms: Some(1),
            error: Some(CatalogErrorCode::Timeout),
            error_message: Some("deadline".into()),
            manual_entry_allowed: true,
        };
        let fake = FakeProvider::ok(crate::testutil::empty_snapshot())
            .with_provider_refresh_result(failing);
        let (base, _provider) = serve(fake).await;
        let resp = operator_client()
            .post(format!("{base}/api/v1/providers/fireworks/models/refresh"))
            .header("content-type", "application/json")
            .body("{}")
            .send()
            .await
            .expect("refresh");
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.expect("json");
        assert_eq!(body["error"], "timeout");
        assert_eq!(body["manual_entry_allowed"], true);
    }

    // A stale/unknown OPERATOR_HEADER value is denied (the guard is exact-once), cited so the import
    // is exercised and the denial is pinned at the provider route specifically.
    #[tokio::test]
    async fn a_wrong_operator_header_is_denied() {
        let (base, _provider) = serve(FakeProvider::ok(crate::testutil::empty_snapshot())).await;
        let resp = reqwest::Client::new()
            .post(format!("{base}/api/v1/providers/fireworks/models/refresh"))
            .header(OPERATOR_HEADER, "0")
            .header("content-type", "application/json")
            .body("{}")
            .send()
            .await
            .expect("send");
        assert_eq!(resp.status(), 403);
        // The correct value is accepted by the guard (a 404 here, since the fake has no catalog).
        let accepted = reqwest::Client::new()
            .post(format!("{base}/api/v1/providers/fireworks/models/refresh"))
            .header(OPERATOR_HEADER, OPERATOR_HEADER_VALUE)
            .header("content-type", "application/json")
            .body("{}")
            .send()
            .await
            .expect("send");
        assert_ne!(accepted.status(), 403);
    }
}
