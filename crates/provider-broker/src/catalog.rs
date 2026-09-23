//! The fixed-endpoint `/models` catalog fetch (STUDIO-990, P9). PB2 owns the one HTTP stack, so the
//! credentialed catalog leg shares the broker's redirect/proxy/TLS/header policy by construction:
//! the same [`UpstreamClient`] (redirects off, ambient proxies off, HTTP/1 only, platform roots,
//! bounded timeouts, no decompression), the same Bearer-only Authorization built inside a
//! credential borrow, the same identity-only encoding.
//!
//! It is deliberately a *purpose-specific* primitive, not a generic forwarder: the destination is
//! exactly [`NormalizedEndpoint::models_url`] and the only caller-supplied header is the leased
//! Bearer credential. It never mints or returns an agent capability, never returns the credential,
//! and drops any entry reflecting the exact credential before it can leave this boundary.

use http::HeaderValue;
use serde::Deserialize;

use crate::binding::BoundCredentialLease;
use crate::secret::ZeroizingBytes;
use crate::upstream::{
    NormalizedEndpoint, ReadError, UpstreamClient, UpstreamError, contains_secret,
};

/// The v1 catalog body cap (design §2.6): 8 MiB.
pub const MAX_CATALOG_BODY_BYTES: usize = 8 * 1024 * 1024;
/// The v1 cap on unique model entries per provider (design §2.6).
pub const MAX_CATALOG_ENTRIES: usize = 10_000;
/// The model id byte cap, mirroring config/status.
pub const MAX_MODEL_ID_BYTES: usize = 512;
/// Per-entry capability-hint caps, so one entry cannot amplify the bounded body into unbounded memory.
pub const MAX_CAPABILITY_HINTS: usize = 64;
pub const MAX_CAPABILITY_BYTES: usize = 512;

/// One sanitized discovered model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogModel {
    pub id: String,
    pub display_name: Option<String>,
    pub capabilities: Vec<String>,
}

/// The sanitized result of one catalog fetch. `truncated` is `true` when the provider list exceeded
/// the cap, contained duplicate ids, contained invalid ids, or reflected the credential — so an
/// incomplete list is always visible rather than silently short.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedModels {
    pub models: Vec<CatalogModel>,
    pub truncated: bool,
}

/// Why a catalog fetch failed. Closed and non-secret.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchError {
    /// The configured endpoint was refused by the fixed-endpoint normalizer.
    Endpoint,
    /// The provider did not answer within the bounded deadline.
    Timeout,
    /// The transport failed after the request was admitted.
    Transport,
    /// The provider answered a non-success status. Only the numeric status crosses.
    Status(u16),
    /// The body was not a well-formed `{ "data": [ { "id": ... } ] }` list.
    Malformed,
    /// The body exceeded [`MAX_CATALOG_BODY_BYTES`].
    BodyTooLarge,
}

/// Fetch and sanitize the model list from `endpoint` using `lease`, a credential already bound to
/// that exact endpoint. The lease is consumed; the credential bytes are borrowed only inside the
/// Authorization construction and a zeroizing copy used for exact-reflection filtering.
pub async fn fetch_models(
    endpoint: &str,
    allow_insecure_http: bool,
    lease: BoundCredentialLease,
) -> Result<FetchedModels, FetchError> {
    let normalized = NormalizedEndpoint::parse(endpoint, allow_insecure_http)
        .map_err(|_| FetchError::Endpoint)?;
    let client = UpstreamClient::new().map_err(|error| match error {
        UpstreamError::ClientBuild(_) => FetchError::Transport,
        _ => FetchError::Transport,
    })?;

    // Build the one Authorization header and a zeroizing copy of the secret for filtering, both
    // inside a single borrow of the lease. Nothing else outlives it.
    let captured = lease.expose_for_upstream(|key| {
        let mut buffer = zeroize::Zeroizing::new(Vec::with_capacity(7 + key.len()));
        buffer.extend_from_slice(b"Bearer ");
        buffer.extend_from_slice(key);
        HeaderValue::from_bytes(&buffer)
            .ok()
            .map(|authorization| (authorization, ZeroizingBytes::new(key.to_vec())))
    });
    let Some((authorization, secret)) = captured else {
        return Err(FetchError::Malformed);
    };

    let response = client
        .fetch_models(&normalized, authorization)
        .await
        .map_err(map_upstream_error)?;

    if !response.status().is_success() {
        return Err(FetchError::Status(response.status().as_u16()));
    }
    if response.has_non_identity_encoding() {
        return Err(FetchError::Malformed);
    }
    let body = response
        .read_bounded(MAX_CATALOG_BODY_BYTES)
        .await
        .map_err(|error| match error {
            ReadError::TooLarge => FetchError::BodyTooLarge,
            ReadError::Transport(error) => map_transport(error),
        })?;

    parse_models(&body, secret.as_slice())
}

fn map_upstream_error(error: UpstreamError) -> FetchError {
    match error {
        UpstreamError::Transport(error) => map_transport(error),
        UpstreamError::ClientBuild(_)
        | UpstreamError::InvalidCredentialHeader
        | UpstreamError::ContentEncoding
        | UpstreamError::MediaType => FetchError::Transport,
    }
}

fn map_transport(error: reqwest::Error) -> FetchError {
    if error.is_timeout() {
        FetchError::Timeout
    } else {
        FetchError::Transport
    }
}

#[derive(Deserialize)]
struct RawList {
    #[serde(default)]
    data: Vec<RawModel>,
}

#[derive(Deserialize)]
struct RawModel {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    capabilities: Vec<String>,
}

/// Parse the closed list shape, drop invalid/reflected/duplicate ids (first valid occurrence wins),
/// and cap at [`MAX_CATALOG_ENTRIES`]. `secret` is the leased credential; any entry reflecting it in
/// its id, display name, or a capability hint is dropped.
fn parse_models(body: &[u8], secret: &[u8]) -> Result<FetchedModels, FetchError> {
    let raw: RawList = serde_json::from_slice(body).map_err(|_| FetchError::Malformed)?;
    let mut models: Vec<CatalogModel> = Vec::new();
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut truncated = false;
    for model in raw.data {
        let Some(id) = model.id else {
            truncated = true;
            continue;
        };
        if !valid_model_id(&id) || !seen.insert(id.clone()) {
            truncated = true;
            continue;
        }
        let display_name = model.display_name.or(model.name);
        let capabilities: Vec<String> = model
            .capabilities
            .into_iter()
            .filter(|hint| hint.len() <= MAX_CAPABILITY_BYTES)
            .take(MAX_CAPABILITY_HINTS)
            .collect();
        if reflected(&id, display_name.as_deref(), &capabilities, secret) {
            truncated = true;
            continue;
        }
        if models.len() >= MAX_CATALOG_ENTRIES {
            truncated = true;
            break;
        }
        models.push(CatalogModel {
            id,
            display_name,
            capabilities,
        });
    }
    Ok(FetchedModels { models, truncated })
}

/// Whether any displayed field reflects the exact credential.
fn reflected(id: &str, display_name: Option<&str>, capabilities: &[String], secret: &[u8]) -> bool {
    if secret.is_empty() {
        return false;
    }
    contains_secret(id.as_bytes(), secret)
        || display_name.is_some_and(|value| contains_secret(value.as_bytes(), secret))
        || capabilities
            .iter()
            .any(|hint| contains_secret(hint.as_bytes(), secret))
}

/// The model-id transport bounds mirrored from config (non-empty, ≤ 512 bytes, no surrounding
/// whitespace, no control/NUL bytes). Membership is never required.
pub fn valid_model_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_MODEL_ID_BYTES
        && id.trim() == id
        && !id.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(body: &str, secret: &str) -> FetchedModels {
        parse_models(body.as_bytes(), secret.as_bytes()).expect("parse")
    }

    #[test]
    fn parses_ids_names_and_capabilities_ignoring_unknown_fields() {
        let fetched = parse(
            r#"{"object":"list","data":[
                {"id":"gpt-4o","object":"model","created":1,"owned_by":"x"},
                {"id":"m2","display_name":"M Two","capabilities":["tools","vision"],"extra":7}
            ]}"#,
            "sk-secret",
        );
        assert!(!fetched.truncated);
        assert_eq!(fetched.models.len(), 2);
        assert_eq!(fetched.models[0].id, "gpt-4o");
        assert_eq!(fetched.models[0].display_name, None);
        assert_eq!(fetched.models[1].display_name.as_deref(), Some("M Two"));
        assert_eq!(fetched.models[1].capabilities, ["tools", "vision"]);
    }

    #[test]
    fn duplicates_and_missing_ids_are_dropped_and_flagged() {
        let fetched = parse(r#"{"data":[{"id":"a"},{"id":"a"},{"name":"no id"}]}"#, "s");
        assert!(fetched.truncated);
        assert_eq!(fetched.models.len(), 1);
        assert_eq!(fetched.models[0].id, "a");
    }

    // MUTATION GUARD (exact credential reflection is removed): an entry reflecting the credential in
    // ANY displayed field is dropped before it can cross the boundary.
    #[test]
    fn reflected_entries_are_dropped() {
        let fetched = parse(
            r#"{"data":[{"id":"ok"},{"id":"sk-live-CANARY"},{"id":"b","display_name":"sk-live-CANARY"},{"id":"c","capabilities":["sk-live-CANARY"]}]}"#,
            "sk-live-CANARY",
        );
        assert!(fetched.truncated);
        assert_eq!(
            fetched
                .models
                .iter()
                .map(|m| m.id.as_str())
                .collect::<Vec<_>>(),
            ["ok"]
        );
    }

    #[test]
    fn a_non_list_body_is_malformed() {
        assert!(parse_models(b"not json", b"s").is_err());
        assert!(parse_models(br#"{"data":"nope"}"#, b"s").is_err());
    }

    #[test]
    fn model_id_bounds_are_enforced() {
        assert!(valid_model_id("gpt-4o"));
        assert!(valid_model_id("accounts/fireworks/models/x"));
        assert!(!valid_model_id(""));
        assert!(!valid_model_id(" padded "));
        assert!(!valid_model_id("has\nnewline"));
    }

    // The endpoint is normalized once and the models URL is exactly the canonical base plus
    // `/models` — never a second `/v1`.
    #[test]
    fn the_models_url_is_joined_exactly_once() {
        let endpoint = NormalizedEndpoint::parse("https://api.openai.com/v1", false).expect("base");
        assert_eq!(endpoint.models_url(), "https://api.openai.com/v1/models");
    }
}
