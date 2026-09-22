//! The fixed upstream egress adapter (design §6): one normalized base URL, one exact join to
//! `chat/completions`, and one outbound client whose redirect/proxy/TLS/decompression policy is
//! fixed at construction.
//!
//! This module is the *only* code that talks to a provider. The forwarding entry point is
//! `pub(crate)` and takes an already-validated [`ChatRequest`] plus a resolved
//! [`NormalizedEndpoint`] — there is no public generic `forward(url, headers, body)` primitive, so a
//! future caller cannot smuggle an arbitrary destination or header map through it.

use std::time::Duration;

use bytes::Bytes;
use futures_util::Stream;
use futures_util::stream;
use http::header::{ACCEPT, ACCEPT_ENCODING, AUTHORIZATION, CONTENT_TYPE};
use http::{HeaderValue, StatusCode};

/// The fixed user agent Rhapsody sends upstream. Non-secret.
pub const RHAPSODY_USER_AGENT: &str = concat!("rhapsody-broker/", env!("CARGO_PKG_VERSION"));

/// Bounded connect timeout for the outbound client (design §6.2).
pub const UPSTREAM_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Bounded per-read (response progress) timeout for the outbound client (design §6.2).
pub const UPSTREAM_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// Why an operator-configured base URL was refused during normalization (design §6.1). Every
/// variant names a closed reason; none carries a credential or the raw URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointError {
    /// The base URL was empty.
    Empty,
    /// No `scheme://` was present.
    MissingScheme,
    /// The scheme was not `http`/`https`.
    UnsupportedScheme,
    /// The authority carried userinfo (`user:pass@`).
    UserinfoRefused,
    /// A query string was present.
    QueryRefused,
    /// A fragment was present.
    FragmentRefused,
    /// The host was missing or empty.
    MissingHost,
    /// The port was not a valid `u16`.
    InvalidPort,
    /// The path carried a percent-encoded form.
    EncodedPathRefused,
    /// The path carried a `.` or `..` segment.
    DotSegmentRefused,
    /// The base already ended in `chat/completions`.
    AlreadyChatCompletions,
    /// Plaintext `http` was requested without the explicit opt-in.
    PlaintextRequiresOptIn,
    /// The insecure opt-in was set on an `https` base (a contradiction).
    InsecureFlagOnHttps,
    /// The final URL could not be parsed by the URL library.
    InvalidUrl,
    /// The hand-rolled authority/path split disagreed with the URL library's parse, so an encoded
    /// form could silently change the authority or path.
    RoundTripRefused,
}

impl std::fmt::Display for EndpointError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            EndpointError::Empty => "base url is empty",
            EndpointError::MissingScheme => "base url has no scheme",
            EndpointError::UnsupportedScheme => "base url scheme must be http or https",
            EndpointError::UserinfoRefused => "base url must not carry userinfo",
            EndpointError::QueryRefused => "base url must not carry a query string",
            EndpointError::FragmentRefused => "base url must not carry a fragment",
            EndpointError::MissingHost => "base url must name a host",
            EndpointError::InvalidPort => "base url port is invalid",
            EndpointError::EncodedPathRefused => "base url path must not be percent-encoded",
            EndpointError::DotSegmentRefused => "base url path must not contain dot segments",
            EndpointError::AlreadyChatCompletions => {
                "base url must not already end in chat/completions"
            }
            EndpointError::PlaintextRequiresOptIn => "plaintext http requires allow_insecure_http",
            EndpointError::InsecureFlagOnHttps => {
                "allow_insecure_http is invalid on an https base url"
            }
            EndpointError::InvalidUrl => "base url could not be parsed",
            EndpointError::RoundTripRefused => "base url changes authority or path when parsed",
        };
        f.write_str(text)
    }
}

/// A parsed and normalized upstream base URL plus the single exact `chat/completions` endpoint
/// derived from it. Both are non-secret workflow metadata (design §6.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedEndpoint {
    canonical: String,
    chat_completions: String,
    insecure_http: bool,
}

impl NormalizedEndpoint {
    /// Parse and normalize an operator-configured base URL once (design §6.1).
    ///
    /// `allow_insecure_http` defaults false: an `https` base accepts an omitted/false flag and
    /// refuses `true`; an `http` base refuses everything but `true`.
    pub fn parse(base_url: &str, allow_insecure_http: bool) -> Result<Self, EndpointError> {
        if base_url.trim().is_empty() {
            return Err(EndpointError::Empty);
        }
        let (scheme, rest) = base_url
            .split_once("://")
            .ok_or(EndpointError::MissingScheme)?;
        let scheme = scheme.to_ascii_lowercase();
        match scheme.as_str() {
            "https" => {
                if allow_insecure_http {
                    return Err(EndpointError::InsecureFlagOnHttps);
                }
            }
            "http" => {
                if !allow_insecure_http {
                    return Err(EndpointError::PlaintextRequiresOptIn);
                }
            }
            _ => return Err(EndpointError::UnsupportedScheme),
        }

        let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let authority = &rest[..authority_end];
        let remainder = &rest[authority_end..];

        if remainder.contains('?') {
            return Err(EndpointError::QueryRefused);
        }
        if remainder.contains('#') {
            return Err(EndpointError::FragmentRefused);
        }
        if authority.contains('@') {
            return Err(EndpointError::UserinfoRefused);
        }
        if authority.is_empty() {
            return Err(EndpointError::MissingHost);
        }
        let (host, port) = parse_authority(authority)?;

        let path = remainder;
        if path.contains('%') {
            return Err(EndpointError::EncodedPathRefused);
        }
        if path
            .split('/')
            .any(|segment| segment == "." || segment == "..")
        {
            return Err(EndpointError::DotSegmentRefused);
        }
        // Canonical storage removes redundant trailing slashes and preserves the operator's prefix.
        let trimmed = path.trim_end_matches('/');
        // The already-`chat/completions` check runs on the canonical (trailing-slash-trimmed) path,
        // so `.../chat/completions/` is refused too (design §6.1).
        let lowered = trimmed.to_ascii_lowercase();
        if lowered == "chat/completions" || lowered.ends_with("/chat/completions") {
            return Err(EndpointError::AlreadyChatCompletions);
        }
        let canonical = if trimmed.is_empty() {
            format!("{scheme}://{authority}")
        } else {
            format!("{scheme}://{authority}{trimmed}")
        };

        // Round-trip: the hand-rolled authority/path split must describe the same destination the
        // URL library will actually use, or an encoded form (a backslash or control byte in the
        // authority) would silently move the request (design §6.1). Compare the *normalized*
        // destination — `Host::parse` normalizes case/IPv6/IDNA, and the path is compared through
        // the URL library's own encoding — so normalization-equivalent forms are accepted while a
        // form whose authority/path actually changes is refused.
        let parsed = url::Url::parse(&canonical).map_err(|_| EndpointError::InvalidUrl)?;
        let parsed_host = parsed.host().ok_or(EndpointError::RoundTripRefused)?;
        // `Host::parse` expects an IPv6 literal to be bracketed.
        let host_for_parse = if host.contains(':') {
            format!("[{host}]")
        } else {
            host.clone()
        };
        let our_host =
            url::Host::parse(&host_for_parse).map_err(|_| EndpointError::RoundTripRefused)?;
        let host_matches = our_host == parsed_host;
        let default_port = if scheme == "https" { 443 } else { 80 };
        let effective_port = port.unwrap_or(default_port);
        let port_matches = parsed.port_or_known_default() == Some(effective_port);
        let expected_path = if trimmed.is_empty() { "/" } else { trimmed };
        let path_probe = url::Url::parse(&format!("https://example.invalid{expected_path}"))
            .map_err(|_| EndpointError::InvalidUrl)?;
        let path_matches = path_probe.path() == parsed.path();
        if !(host_matches && port_matches && path_matches) {
            return Err(EndpointError::RoundTripRefused);
        }

        // One internal trailing slash then the fixed relative path; never another `/v1`.
        let chat_completions = format!("{canonical}/chat/completions");
        if url::Url::parse(&chat_completions).is_err() {
            return Err(EndpointError::InvalidUrl);
        }
        Ok(Self {
            canonical,
            chat_completions,
            insecure_http: scheme == "http",
        })
    }

    /// The canonical base URL (redundant trailing slashes removed; operator prefix preserved).
    pub fn canonical(&self) -> &str {
        &self.canonical
    }

    /// The one exact Chat Completions endpoint, joined exactly once.
    pub fn chat_completions_url(&self) -> &str {
        &self.chat_completions
    }

    /// Whether this endpoint is plaintext HTTP (the operator explicitly opted in).
    pub fn is_insecure_http(&self) -> bool {
        self.insecure_http
    }
}

/// Split an authority into its host (IPv6 brackets removed) and an optional `u16` port.
fn parse_authority(authority: &str) -> Result<(String, Option<u16>), EndpointError> {
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        // IPv6 literal.
        let close = rest.find(']').ok_or(EndpointError::MissingHost)?;
        let host = &rest[..close];
        let after = &rest[close + 1..];
        let port = if after.is_empty() {
            None
        } else {
            Some(after.strip_prefix(':').ok_or(EndpointError::InvalidPort)?)
        };
        (host, port)
    } else if let Some((host, port)) = authority.rsplit_once(':') {
        (host, Some(port))
    } else {
        (authority, None)
    };
    if host.is_empty() {
        return Err(EndpointError::MissingHost);
    }
    let port = match port {
        Some(port) => Some(
            port.parse::<u16>()
                .map_err(|_| EndpointError::InvalidPort)?,
        ),
        None => None,
    };
    Ok((host.to_owned(), port))
}

/// Why an outbound request failed. A transport failure after admission is conservatively charged by
/// the caller (design §5.4).
#[derive(Debug)]
pub enum UpstreamError {
    /// The outbound client could not be constructed (configuration, not per-request).
    ClientBuild(reqwest::Error),
    /// The transport failed (connect/reset/timeout) after the request was admitted.
    Transport(reqwest::Error),
    /// The upstream credential could not form an `Authorization` header (never for a validated key).
    InvalidCredentialHeader,
    /// The upstream response used a content encoding other than `identity`.
    ContentEncoding,
    /// The upstream response media type did not match the requested response mode.
    MediaType,
}

impl std::fmt::Display for UpstreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UpstreamError::ClientBuild(_) => f.write_str("upstream client construction failed"),
            UpstreamError::Transport(_) => f.write_str("upstream transport failed"),
            UpstreamError::InvalidCredentialHeader => {
                f.write_str("upstream credential could not form a header")
            }
            UpstreamError::ContentEncoding => {
                f.write_str("upstream content encoding is not identity")
            }
            UpstreamError::MediaType => f.write_str("upstream media type is not acceptable"),
        }
    }
}

/// The one outbound client. Its policy is fixed at construction: redirects disabled, ambient
/// proxies disabled, HTTP/1 only, platform trust roots, bounded timeouts, no decompression.
pub struct UpstreamClient {
    client: reqwest::Client,
}

impl UpstreamClient {
    /// Build the fixed outbound client (design §6.2).
    pub fn new() -> Result<Self, UpstreamError> {
        let client = reqwest::Client::builder()
            // Redirects are disabled completely; a redirect cannot carry the credential elsewhere.
            .redirect(reqwest::redirect::Policy::none())
            // Never inherit HTTP_PROXY/HTTPS_PROXY/system proxies: an implicit proxy would receive
            // the provider credential.
            .no_proxy()
            // HTTP/1 only for v1; keeps the response parsing surface closed.
            .http1_only()
            // Decompression is explicitly disabled rather than left to feature unification: if any
            // crate in the daemon build later enables reqwest's `gzip`/`brotli`/`deflate`/`zstd`,
            // transparent decoding would hide the encoding from `has_non_identity_encoding` and
            // silently break §6.2's identity-only rule. These builders exist regardless of features.
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .no_zstd()
            // Platform trust roots with hostname validation; never disable verification.
            .https_only(false)
            .tls_built_in_root_certs(true)
            .danger_accept_invalid_certs(false)
            // Bounded connect and response-progress timeouts.
            .connect_timeout(UPSTREAM_CONNECT_TIMEOUT)
            .read_timeout(UPSTREAM_READ_TIMEOUT)
            .pool_max_idle_per_host(4)
            .user_agent(RHAPSODY_USER_AGENT)
            .build()
            .map_err(UpstreamError::ClientBuild)?;
        Ok(Self { client })
    }

    /// Send one Chat Completions request to the fixed endpoint with only the protocol headers
    /// (design §5.4). `authorization` is the already-built `Bearer <upstream credential>` header for
    /// this one request; it is never a default header on the reusable client.
    pub(crate) async fn forward_chat_completions(
        &self,
        endpoint: &NormalizedEndpoint,
        authorization: HeaderValue,
        body: &[u8],
    ) -> Result<UpstreamResponse, UpstreamError> {
        let response = self
            .client
            .post(endpoint.chat_completions_url())
            .header(AUTHORIZATION, authorization)
            .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
            .header(
                ACCEPT,
                HeaderValue::from_static("text/event-stream, application/json"),
            )
            .header(ACCEPT_ENCODING, HeaderValue::from_static("identity"))
            .body(body.to_vec())
            .send()
            .await
            .map_err(UpstreamError::Transport)?;
        Ok(UpstreamResponse { response })
    }
}

impl std::fmt::Debug for UpstreamClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<upstream client>")
    }
}

/// A received upstream response. The caller validates the media type, selects headers and streams the
/// body through the redactor.
pub(crate) struct UpstreamResponse {
    response: reqwest::Response,
}

impl UpstreamResponse {
    /// The upstream status code (forwarded as-is).
    pub(crate) fn status(&self) -> StatusCode {
        self.response.status()
    }

    /// The upstream headers.
    pub(crate) fn headers(&self) -> &http::HeaderMap {
        self.response.headers()
    }

    /// Whether the upstream declared a content encoding other than identity.
    pub(crate) fn has_non_identity_encoding(&self) -> bool {
        match self.response.headers().get(http::header::CONTENT_ENCODING) {
            None => false,
            Some(value) => !value.as_bytes().eq_ignore_ascii_case(b"identity"),
        }
    }

    /// The upstream media type, lowercased and stripped of parameters.
    pub(crate) fn media_type(&self) -> Option<String> {
        self.response
            .headers()
            .get(http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(|value| {
                value
                    .split(';')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_ascii_lowercase()
            })
    }

    /// The backpressured body stream of raw (unredacted) upstream chunks. Built from
    /// [`reqwest::Response::chunk`] (no `stream` feature) so each chunk is pulled only as the
    /// downstream body is polled.
    pub(crate) fn into_byte_stream(self) -> impl Stream<Item = reqwest::Result<Bytes>> + Send {
        stream::unfold(self.response, |mut response| async move {
            match response.chunk().await {
                Ok(Some(chunk)) => Some((Ok(chunk), response)),
                Ok(None) => None,
                Err(error) => Some((Err(error), response)),
            }
        })
    }
}

/// A `chat/completions` response body's canonical media type the broker emits (design §6.3).
pub fn canonical_content_type(streaming: bool) -> HeaderValue {
    if streaming {
        HeaderValue::from_static("text/event-stream")
    } else {
        HeaderValue::from_static("application/json")
    }
}

/// Whether a value reflects the exact upstream credential and must therefore be dropped rather than
/// forwarded (design §6.3).
pub(crate) fn contains_secret(value: &[u8], secret: &[u8]) -> bool {
    if secret.is_empty() || value.len() < secret.len() {
        return false;
    }
    value.windows(secret.len()).any(|window| window == secret)
}

/// Parse/normalize a `Retry-After` header (seconds) into a bounded seconds value, clamped to the
/// remaining turn lifetime (design §6.3). Non-numeric/HTTP-date forms are dropped.
pub(crate) fn parse_retry_after(value: &[u8], remaining: Duration) -> Option<u64> {
    let text = std::str::from_utf8(value).ok()?.trim();
    let seconds: u64 = text.parse().ok()?;
    Some(seconds.min(remaining.as_secs()))
}

/// Parse/normalize a `Retry-After-Ms` header (milliseconds) into a bounded millisecond value,
/// clamped to the remaining turn lifetime. Kept in milliseconds so the unit stays correct.
pub(crate) fn parse_retry_after_ms(value: &[u8], remaining: Duration) -> Option<u64> {
    let text = std::str::from_utf8(value).ok()?.trim();
    let millis: u64 = text.parse().ok()?;
    Some(millis.min(remaining.as_millis().min(u128::from(u64::MAX)) as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_bases_with_no_path_v1_and_nested_v1_exactly_once() {
        let no_path = NormalizedEndpoint::parse("https://api.example.com", false).expect("base");
        assert_eq!(
            no_path.chat_completions_url(),
            "https://api.example.com/chat/completions"
        );

        let v1 = NormalizedEndpoint::parse("https://api.example.com/v1", false).expect("base");
        assert_eq!(
            v1.chat_completions_url(),
            "https://api.example.com/v1/chat/completions"
        );

        let nested = NormalizedEndpoint::parse("https://api.fireworks.ai/inference/v1", false)
            .expect("base");
        assert_eq!(
            nested.chat_completions_url(),
            "https://api.fireworks.ai/inference/v1/chat/completions"
        );
        assert_eq!(nested.canonical(), "https://api.fireworks.ai/inference/v1");
    }

    #[test]
    fn redundant_trailing_slashes_are_canonicalized_once() {
        let endpoint =
            NormalizedEndpoint::parse("https://api.example.com/v1///", false).expect("base");
        assert_eq!(endpoint.canonical(), "https://api.example.com/v1");
        assert_eq!(
            endpoint.chat_completions_url(),
            "https://api.example.com/v1/chat/completions"
        );
    }

    #[test]
    fn a_base_already_ending_in_chat_completions_is_refused() {
        assert_eq!(
            NormalizedEndpoint::parse("https://api.example.com/v1/chat/completions", false),
            Err(EndpointError::AlreadyChatCompletions)
        );
        // A trailing slash must not sneak the base past the check.
        assert_eq!(
            NormalizedEndpoint::parse("https://api.example.com/v1/chat/completions/", false),
            Err(EndpointError::AlreadyChatCompletions)
        );
    }

    #[test]
    fn an_encoded_authority_that_changes_on_parse_is_refused() {
        // A backslash in the authority moves the path under WHATWG parsing.
        assert_eq!(
            NormalizedEndpoint::parse("https://good.example\\evil.example/v1", false),
            Err(EndpointError::RoundTripRefused)
        );
        // A raw tab inside the host is stripped by WHATWG parsing.
        assert_eq!(
            NormalizedEndpoint::parse("https://api.exa\tmple.com/v1", false),
            Err(EndpointError::RoundTripRefused)
        );
    }

    #[test]
    fn round_trip_accepts_default_ports_and_case_insensitive_hosts() {
        let upper = NormalizedEndpoint::parse("https://API.Example.COM/v1", false).expect("upper");
        assert_eq!(
            upper.chat_completions_url(),
            "https://API.Example.COM/v1/chat/completions"
        );
        let default_port =
            NormalizedEndpoint::parse("https://api.example.com:443/v1", false).expect(":443");
        assert_eq!(
            default_port.chat_completions_url(),
            "https://api.example.com:443/v1/chat/completions"
        );
        let ipv6 = NormalizedEndpoint::parse("https://[::1]:9000/v1", false).expect("ipv6");
        assert_eq!(
            ipv6.chat_completions_url(),
            "https://[::1]:9000/v1/chat/completions"
        );
    }

    #[test]
    fn round_trip_accepts_normalization_equivalent_endpoints() {
        // An expanded IPv6 literal normalizes to the same address the URL library uses.
        let expanded = NormalizedEndpoint::parse("https://[0:0:0:0:0:0:0:1]/v1", false)
            .expect("expanded ipv6");
        assert_eq!(
            expanded.chat_completions_url(),
            "https://[0:0:0:0:0:0:0:1]/v1/chat/completions"
        );
        // An IDNA/Unicode host normalizes to the same punycode destination.
        let idna =
            NormalizedEndpoint::parse("https://bücher.example/v1", false).expect("idna host");
        assert_eq!(idna.canonical(), "https://bücher.example/v1");
        // A raw Unicode path is encoded identically on both sides of the comparison.
        let unicode_path =
            NormalizedEndpoint::parse("https://api.example.com/v1/ünïcode", false).expect("path");
        assert!(
            unicode_path
                .chat_completions_url()
                .contains("/v1/ünïcode/chat/completions")
        );
    }

    #[test]
    fn plaintext_needs_the_explicit_optin() {
        assert_eq!(
            NormalizedEndpoint::parse("http://127.0.0.1:9000/v1", false),
            Err(EndpointError::PlaintextRequiresOptIn)
        );
        let opted = NormalizedEndpoint::parse("http://127.0.0.1:9000/v1", true).expect("opted");
        assert!(opted.is_insecure_http());
        // The flag is a contradiction on https.
        assert_eq!(
            NormalizedEndpoint::parse("https://api.example.com/v1", true),
            Err(EndpointError::InsecureFlagOnHttps)
        );
        // ...and omitted is fine on https.
        assert!(NormalizedEndpoint::parse("https://api.example.com/v1", false).is_ok());
    }

    #[test]
    fn rejects_userinfo_query_fragment_and_bad_ports() {
        assert_eq!(
            NormalizedEndpoint::parse("https://user:pass@api.example.com/v1", false),
            Err(EndpointError::UserinfoRefused)
        );
        assert_eq!(
            NormalizedEndpoint::parse("https://api.example.com/v1?x=1", false),
            Err(EndpointError::QueryRefused)
        );
        assert_eq!(
            NormalizedEndpoint::parse("https://api.example.com/v1#frag", false),
            Err(EndpointError::FragmentRefused)
        );
        assert_eq!(
            NormalizedEndpoint::parse("https://api.example.com:notaport/v1", false),
            Err(EndpointError::InvalidPort)
        );
        assert_eq!(
            NormalizedEndpoint::parse("https:///v1", false),
            Err(EndpointError::MissingHost)
        );
        assert_eq!(
            NormalizedEndpoint::parse("ftp://api.example.com/v1", false),
            Err(EndpointError::UnsupportedScheme)
        );
    }

    #[test]
    fn rejects_encoded_separators_and_dot_segments() {
        assert_eq!(
            NormalizedEndpoint::parse("https://api.example.com/v1%2Fevil", false),
            Err(EndpointError::EncodedPathRefused)
        );
        assert_eq!(
            NormalizedEndpoint::parse("https://api.example.com/a/../b", false),
            Err(EndpointError::DotSegmentRefused)
        );
    }

    #[test]
    fn retry_after_is_parsed_and_clamped() {
        assert_eq!(parse_retry_after(b"30", Duration::from_secs(600)), Some(30));
        assert_eq!(
            parse_retry_after(b"9999", Duration::from_secs(600)),
            Some(600)
        );
        assert_eq!(
            parse_retry_after_ms(b"999999999", Duration::from_secs(600)),
            Some(600_000)
        );
        assert_eq!(
            parse_retry_after_ms(b"1500", Duration::from_secs(600)),
            Some(1_500)
        );
        assert_eq!(
            parse_retry_after(b"Wed, 21 Oct 2015 07:28:00 GMT", Duration::from_secs(600)),
            None
        );
        assert_eq!(parse_retry_after(b"", Duration::from_secs(600)), None);
    }

    #[test]
    fn secret_reflection_detection_is_exact() {
        assert!(contains_secret(b"x sk-live-key y", b"sk-live-key"));
        assert!(!contains_secret(b"x sk-live-ke y", b"sk-live-key"));
        assert!(!contains_secret(b"short", b"sk-live-key"));
    }
}
