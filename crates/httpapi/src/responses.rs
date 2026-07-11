//! responses — HTTP response envelope plumbing. Parity port of the response-writing helpers in
//! `$REF/internal/httpapi/handlers.go` (`writeJSON`/`writeError`) and the envelope DTOs in
//! `$REF/internal/httpapi/responses.go` (`errorEnvelope`/`errorBody`/`healthzJSON`).
//!
//! Only the H1 subset is ported here. The typed per-field validation breakdown
//! (`writeErrorFields` + `fieldError`, used only by the config POST) lands with H3, and the `/state`
//! wire DTOs are O4's `orchestrator::snapshot_json` (which the state handler reuses) — so this file
//! carries just the healthz body + the error envelope the H1 handlers write.

use axum::body::Body;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use serde::Serialize;

/// `GET /healthz` body — a minimal, state-free liveness signal for the desktop supervisor's
/// readiness poll. Mirrors Go `healthzJSON`.
#[derive(Serialize)]
pub(crate) struct HealthzJson {
    pub status: &'static str,
}

/// The error response wrapper (`{"error": {…}}`). Mirrors Go `errorEnvelope`.
#[derive(Serialize)]
struct ErrorEnvelope {
    error: ErrorBody,
}

/// The error body: a machine `code` + human `message`. Mirrors Go `errorBody`. Go's `omitempty`
/// per-field `fields` breakdown (typed config POST) is added with H3; omitting it here is
/// byte-identical to Go's output when it is empty, which is every H1 error response.
#[derive(Serialize)]
struct ErrorBody {
    code: &'static str,
    message: String,
}

/// Serialize `value` as a JSON response with `status` and `Content-Type: application/json` — exactly
/// that value, with no `charset` parameter (the SPA + `healthz_test.go` assert the literal string).
/// Mirrors Go `writeJSON`.
pub(crate) fn write_json<T: Serialize>(status: StatusCode, value: &T) -> Response {
    match serde_json::to_vec(value) {
        Ok(bytes) => {
            let mut resp = Response::new(Body::from(bytes));
            *resp.status_mut() = status;
            resp.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
            resp
        }
        // Serializing our own owned DTOs / a `serde_json::Value` cannot realistically fail; log it
        // like Go ("http: encode response failed") and fall back to the bare status so the caller's
        // status line is still honored. Never a panic (errors are values on production paths).
        Err(err) => {
            tracing::error!(error = %err, "http: encode response failed");
            let mut resp = Response::new(Body::empty());
            *resp.status_mut() = status;
            resp
        }
    }
}

/// Write an error envelope with `status`, `code`, and `message`, optionally advertising the allowed
/// methods via an `Allow` header (the 405 path). Mirrors Go `writeError` plus the handlers' explicit
/// `Allow` set.
pub(crate) fn write_error(
    status: StatusCode,
    code: &'static str,
    message: impl Into<String>,
    allow: Option<&'static str>,
) -> Response {
    let mut resp = write_json(
        status,
        &ErrorEnvelope {
            error: ErrorBody {
                code,
                message: message.into(),
            },
        },
    );
    if let Some(allow) = allow {
        resp.headers_mut()
            .insert(header::ALLOW, HeaderValue::from_static(allow));
    }
    resp
}
