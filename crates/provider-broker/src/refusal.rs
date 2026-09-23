//! The broker-generated error body and its fixture-pinned statuses (design §5.5).
//!
//! Broker errors are bounded and OpenAI-shaped:
//!
//! ```json
//! {"error":{"type":"rhapsody_policy_error","code":"budget_exhausted",
//!  "message":"Rhapsody provider capability exhausted"}}
//! ```
//!
//! Policy failures use exact, non-retryable 4xx statuses and messages that do not match OpenCode's
//! transient retry patterns. The exact internal reason is recorded in bounded broker diagnostics,
//! never reflected to the child; upstream statuses are forwarded as upstream statuses.

use axum::body::Body;
use axum::response::Response;
use http::header::{CONNECTION, CONTENT_TYPE};
use http::{HeaderValue, StatusCode};

/// A broker-generated policy refusal. Each variant has one pinned status/code/message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyRefusal {
    /// Missing, repeated, malformed, unknown, expired, or revoked credential — all identical.
    Unauthorized,
    /// A request whose JSON could not be parsed or whose shape is not a Chat Completions request.
    InvalidRequest,
    /// An unknown top-level field, including a named generation-control alias.
    UnknownField,
    /// The requested model did not equal the grant's exact model.
    ModelMismatch,
    /// A content part outside the pinned text/tool shapes, or a refused remote-fetch form.
    UnsupportedContent,
    /// A tool or tool_choice outside the pinned function-tool shape.
    InvalidTool,
    /// More messages/tools than the structural limits allow.
    TooManyItems,
    /// The request body exceeded the admitted byte size.
    RequestTooLarge,
    /// A turn/session/concurrency budget would be exceeded.
    BudgetExhausted,
    /// The operator-configured upstream endpoint could not be used (e.g. plaintext without opt-in).
    ProviderMisconfigured,
    /// The path or method did not match the one exact route.
    NotFound,
    /// The method was not `POST`.
    MethodNotAllowed,
    /// A browser `Origin`/CORS preflight header was present.
    OriginRefused,
    /// A request `Content-Encoding` other than `identity`.
    ContentEncoding,
    /// The upstream transport failed after admission.
    UpstreamUnavailable,
    /// The upstream response was unusable (encoding/media type/size).
    UpstreamProtocol,
}

/// The pinned public view of a refusal, for callers and tests that must assert all three.
impl PolicyRefusal {
    /// The pinned HTTP status.
    pub fn status(self) -> StatusCode {
        pin(self).0
    }

    /// The pinned OpenAI error code.
    pub fn code(self) -> &'static str {
        pin(self).1
    }

    /// The pinned bounded message.
    pub fn message(self) -> &'static str {
        pin(self).2
    }
}

/// The pinned status, code and message for a refusal. Kept as one table so a test can pin all three
/// and no call site can invent a new message.
pub(crate) fn pin(refusal: PolicyRefusal) -> (StatusCode, &'static str, &'static str) {
    match refusal {
        PolicyRefusal::Unauthorized => (
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "Rhapsody provider capability is not valid",
        ),
        PolicyRefusal::InvalidRequest => (
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "Rhapsody provider request is not valid",
        ),
        PolicyRefusal::UnknownField => (
            StatusCode::BAD_REQUEST,
            "unknown_field",
            "Rhapsody provider request carries an unsupported field",
        ),
        PolicyRefusal::ModelMismatch => (
            StatusCode::BAD_REQUEST,
            "model_mismatch",
            "Rhapsody provider request model does not match the grant",
        ),
        PolicyRefusal::UnsupportedContent => (
            StatusCode::BAD_REQUEST,
            "unsupported_content",
            "Rhapsody provider request content is not supported",
        ),
        PolicyRefusal::InvalidTool => (
            StatusCode::BAD_REQUEST,
            "invalid_tool",
            "Rhapsody provider request tools are not supported",
        ),
        PolicyRefusal::TooManyItems => (
            StatusCode::BAD_REQUEST,
            "too_many_items",
            "Rhapsody provider request exceeds a structural limit",
        ),
        PolicyRefusal::RequestTooLarge => (
            StatusCode::PAYLOAD_TOO_LARGE,
            "request_too_large",
            "Rhapsody provider request exceeds the admitted size",
        ),
        PolicyRefusal::BudgetExhausted => (
            StatusCode::FORBIDDEN,
            "budget_exhausted",
            "Rhapsody provider capability exhausted",
        ),
        PolicyRefusal::ProviderMisconfigured => (
            StatusCode::FORBIDDEN,
            "provider_misconfigured",
            "Rhapsody provider endpoint configuration is invalid",
        ),
        PolicyRefusal::NotFound => (
            StatusCode::NOT_FOUND,
            "not_found",
            "Rhapsody provider route does not exist",
        ),
        PolicyRefusal::MethodNotAllowed => (
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
            "Rhapsody provider route accepts POST only",
        ),
        PolicyRefusal::OriginRefused => (
            StatusCode::FORBIDDEN,
            "origin_refused",
            "Rhapsody provider route refuses browser origins",
        ),
        PolicyRefusal::ContentEncoding => (
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "content_encoding",
            "Rhapsody provider request content encoding is not supported",
        ),
        PolicyRefusal::UpstreamUnavailable => (
            StatusCode::BAD_GATEWAY,
            "upstream_unavailable",
            "Rhapsody provider upstream is unavailable",
        ),
        PolicyRefusal::UpstreamProtocol => (
            StatusCode::BAD_GATEWAY,
            "upstream_protocol",
            "Rhapsody provider upstream response is not usable",
        ),
    }
}

/// Render a refusal as the bounded OpenAI-shaped response.
///
/// Every broker-generated refusal closes the HTTP/1.1 connection. An auth failure *must* close so an
/// unread request body cannot be reused or drained into another request (design §5.2); the same
/// framing closes the pre-body refusals (bad Host/Origin/method/encoding, an over-ceiling body) whose
/// request body was deliberately not consumed. Only successful forwarded responses keep the
/// connection alive.
pub(crate) fn refusal_response(refusal: PolicyRefusal) -> Response {
    let (status, code, message) = pin(refusal);
    let body = serde_json::json!({
        "error": {
            "type": "rhapsody_policy_error",
            "code": code,
            "message": message,
        }
    });
    let bytes = serde_json::to_vec(&body).unwrap_or_else(|_| b"{}".to_vec());
    let mut response = Response::new(Body::from(bytes));
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    response
        .headers_mut()
        .insert(CONNECTION, HeaderValue::from_static("close"));
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_policy_failure_is_a_non_retryable_4xx() {
        for refusal in [
            PolicyRefusal::Unauthorized,
            PolicyRefusal::InvalidRequest,
            PolicyRefusal::UnknownField,
            PolicyRefusal::ModelMismatch,
            PolicyRefusal::UnsupportedContent,
            PolicyRefusal::InvalidTool,
            PolicyRefusal::TooManyItems,
            PolicyRefusal::RequestTooLarge,
            PolicyRefusal::BudgetExhausted,
            PolicyRefusal::ProviderMisconfigured,
            PolicyRefusal::NotFound,
            PolicyRefusal::MethodNotAllowed,
            PolicyRefusal::OriginRefused,
            PolicyRefusal::ContentEncoding,
        ] {
            let (status, code, message) = pin(refusal);
            assert!(
                status.is_client_error(),
                "{code} must be a non-retryable 4xx, got {status}"
            );
            assert_ne!(status, StatusCode::TOO_MANY_REQUESTS);
            assert!(!message.is_empty());
        }
    }

    #[test]
    fn unauthorized_body_is_one_bounded_shape() {
        let response = refusal_response(PolicyRefusal::Unauthorized);
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            response.headers().get(CONNECTION),
            Some(&HeaderValue::from_static("close"))
        );
    }
}
