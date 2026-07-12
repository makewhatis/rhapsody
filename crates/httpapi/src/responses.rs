//! responses — HTTP response envelope plumbing. Parity port of the response-writing helpers in
//! `$REF/internal/httpapi/handlers.go` (`writeJSON`/`writeError`) and the envelope DTOs in
//! `$REF/internal/httpapi/responses.go` (`errorEnvelope`/`errorBody`/`healthzJSON`).
//!
//! The `/state` wire DTOs are O4's `orchestrator::snapshot_json` (which the state handler reuses), so
//! this file carries the healthz body, the error envelope, and — added with the H3 config POST — the
//! typed per-field validation breakdown (`write_error_fields` + [`FieldError`], Go `writeErrorFields`/
//! `fieldError`).

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

/// The error body: a machine `code` + human `message`, plus an optional per-field `fields` breakdown
/// (the typed config POST). Mirrors Go `errorBody`. `fields` is `skip_serializing_if` empty, so it is
/// absent from every non-config-POST error — byte-identical to Go's `omitempty` output.
#[derive(Serialize)]
struct ErrorBody {
    code: &'static str,
    message: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    fields: Vec<FieldError>,
}

/// Attaches a validation message to a specific config field path so the Settings UI can surface it
/// inline (Go `fieldError`). Only populated on the typed config POST validation path. Mirrors the Go
/// struct's `path`/`message` json shape.
#[derive(Serialize)]
pub(crate) struct FieldError {
    pub(crate) path: String,
    pub(crate) message: String,
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
                fields: Vec::new(),
            },
        },
    );
    if let Some(allow) = allow {
        resp.headers_mut()
            .insert(header::ALLOW, HeaderValue::from_static(allow));
    }
    resp
}

/// Write an error envelope with a structured per-field `fields` breakdown (Go `writeErrorFields`),
/// used only by the typed config POST so the Settings UI can attach a validation message to the
/// offending input. With an empty `fields` this is byte-identical to [`write_error`].
pub(crate) fn write_error_fields(
    status: StatusCode,
    code: &'static str,
    message: impl Into<String>,
    fields: Vec<FieldError>,
) -> Response {
    write_json(
        status,
        &ErrorEnvelope {
            error: ErrorBody {
                code,
                message: message.into(),
                fields,
            },
        },
    )
}
