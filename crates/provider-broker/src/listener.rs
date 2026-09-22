//! The private loopback listener and the one exact Chat Completions route (design §5).
//!
//! The server binds an IPv4 `TcpListener` to exactly `127.0.0.1:0`, serves HTTP/1.1 only, and reports
//! its actual address only through in-process handles. Loopback binding is **not** authentication:
//! every request must present the bearer capability, and an auth failure closes the connection so an
//! unread body cannot be reused.
//!
//! The handler authenticates from the bounded header block *before* polling the potentially large
//! JSON body, then acquires the grant's concurrency permit and the broker-wide request-memory budget,
//! validates the closed schema, and only then constructs the one fixed outbound request. The response
//! body is streamed back through the exact-secret redactor, both byte ceilings and the bounded SSE
//! observer — never collected whole, and never forwarded by a detached task.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::response::Response;
use axum::routing::any;
use bytes::Bytes;
use futures_util::StreamExt;
use futures_util::stream::{Stream, unfold};
use http::header::{
    ACCESS_CONTROL_REQUEST_HEADERS, ACCESS_CONTROL_REQUEST_METHOD, AUTHORIZATION, CONNECTION,
    CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE, HOST, ORIGIN, TE, UPGRADE,
};
use http::{HeaderMap, HeaderName, HeaderValue, Method};
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::net::TcpListener;
use tower::Service;

use crate::broker::Broker;
use crate::budget::{
    BUFFERED_RESPONSE_BUDGET, REQUEST_MEMORY_BUDGET, WeightedBudget, buffered_response_weight,
    request_weight,
};
use crate::redact::StreamingRedactor;
use crate::refusal::{PolicyRefusal, refusal_response};
use crate::schema::{ChatRequestPolicy, RequestRejection, validate_chat_request};
use crate::secret::ZeroizingBytes;
use crate::sse::SseUsageObserver;
use crate::turn::CapabilityGrant;
use crate::upstream::{
    NormalizedEndpoint, UpstreamClient, canonical_content_type, contains_secret, parse_retry_after,
    parse_retry_after_ms,
};

/// Broker-wide ceiling on accepted connections / active handlers (design §5.1).
pub const MAX_CONNECTIONS: usize = 256;
/// Maximum requests served on one HTTP/1.1 connection (design §5.1).
pub const MAX_REQUESTS_PER_CONNECTION: u32 = 64;
/// Header-read timeout for one request (design §5.1).
pub const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(5);
/// Maximum request header block size (design §5.1).
pub const MAX_HEADER_BLOCK_BYTES: usize = 64 * 1024;
/// Maximum count of headers hyper will parse on one request.
pub const MAX_HEADER_COUNT: usize = 64;

/// Diagnostic response headers the broker may forward, each bounded and never reflecting the key
/// (design §6.3).
const FORWARDED_DIAGNOSTIC_HEADERS: &[&str] = &[
    "retry-after",
    "retry-after-ms",
    "x-ratelimit-limit-requests",
    "x-ratelimit-remaining-requests",
    "x-ratelimit-limit-tokens",
    "x-ratelimit-remaining-tokens",
    "x-ratelimit-reset-requests",
    "x-ratelimit-reset-tokens",
];
/// Per-value bound for a forwarded diagnostic header (design §6.3).
const MAX_DIAGNOSTIC_HEADER_BYTES: usize = 256;

/// The shared handler state.
#[derive(Clone)]
struct BrokerState {
    broker: Broker,
    client: Arc<UpstreamClient>,
    request_budget: Arc<WeightedBudget>,
    response_budget: Arc<WeightedBudget>,
    expected_host: Arc<str>,
}

/// The private loopback listener. Binding is the only public construction; the actual address is
/// reported only through [`BrokerListener::local_addr`].
pub struct BrokerListener {
    listener: TcpListener,
    router: Router,
    expected_host: Arc<str>,
    connections: Arc<tokio::sync::Semaphore>,
    addr: SocketAddr,
}

impl BrokerListener {
    /// Bind an ephemeral IPv4 loopback listener and build the broker whose capability base URL names
    /// exactly that address. Returns the listener and the broker so the daemon can register sessions
    /// against the live address (design §5.1: the actual address is reported only in-process).
    pub fn bind_with(
        clock: Arc<dyn crate::clock::Clock>,
        rng: Arc<dyn crate::random::RandomSource>,
    ) -> std::io::Result<(Self, Broker)> {
        let std_listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
        let addr = std_listener.local_addr()?;
        let base_url = format!("http://127.0.0.1:{}/v1", addr.port());
        let broker = Broker::new(base_url, clock, rng).map_err(std::io::Error::other)?;
        let listener = Self::serve(std_listener, broker.clone())?;
        Ok((listener, broker))
    }

    /// Adopt an already-bound loopback listener for `broker`, verifying that the broker's capability
    /// base URL names exactly the bound address. Refuses a non-loopback or mismatched listener.
    pub fn serve(listener: std::net::TcpListener, broker: Broker) -> std::io::Result<Self> {
        let addr = listener.local_addr()?;
        if !addr.ip().is_loopback() || !addr.is_ipv4() {
            return Err(std::io::Error::other(
                "the provider broker listener must bind 127.0.0.1",
            ));
        }
        let expected_host = expected_host_for(broker.base_url(), addr.port())?;
        listener.set_nonblocking(true)?;
        let listener = TcpListener::from_std(listener)?;
        let client = Arc::new(
            UpstreamClient::new().map_err(|error| std::io::Error::other(error.to_string()))?,
        );
        let state = BrokerState {
            broker,
            client,
            request_budget: WeightedBudget::new(REQUEST_MEMORY_BUDGET),
            response_budget: WeightedBudget::new(BUFFERED_RESPONSE_BUDGET),
            expected_host: Arc::clone(&expected_host),
        };
        let router = Router::new()
            .route("/v1/chat/completions", any(handle_chat))
            .fallback(handle_not_found)
            .with_state(state);
        Ok(Self {
            listener,
            router,
            expected_host,
            connections: Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS)),
            addr,
        })
    }

    /// The actual bound address (the only publication of the ephemeral port).
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// The exact expected `Host` header (`127.0.0.1:<port>`).
    pub fn expected_host(&self) -> &str {
        &self.expected_host
    }

    /// Serve until the listener is dropped.
    pub async fn run(self) -> std::io::Result<()> {
        self.run_with_shutdown(std::future::pending::<()>()).await
    }

    /// Serve until `shutdown` resolves. Each accepted connection holds one broker-wide permit and may
    /// serve at most [`MAX_REQUESTS_PER_CONNECTION`] requests.
    pub async fn run_with_shutdown(
        self,
        shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> std::io::Result<()> {
        let listener = self.listener;
        let router = self.router;
        let connections = self.connections;
        tokio::pin!(shutdown);
        loop {
            let permit = tokio::select! {
                _ = &mut shutdown => break,
                permit = Arc::clone(&connections).acquire_owned() => {
                    permit.map_err(|_| std::io::Error::other("connection ceiling closed"))?
                }
            };
            let (stream, _peer) = tokio::select! {
                _ = &mut shutdown => break,
                accepted = listener.accept() => accepted?,
            };
            let router = router.clone();
            tokio::spawn(async move {
                let _permit = permit;
                serve_connection(stream, router).await;
            });
        }
        Ok(())
    }
}

impl std::fmt::Debug for BrokerListener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrokerListener")
            .field("addr", &self.addr)
            .finish_non_exhaustive()
    }
}

/// Verify the broker's capability base URL names exactly `127.0.0.1:<bound-port>/v1` and return the
/// exact expected `Host` value.
fn expected_host_for(base_url: &str, port: u16) -> std::io::Result<Arc<str>> {
    let parsed = url::Url::parse(base_url)
        .map_err(|_| std::io::Error::other("broker base url is not a valid url"))?;
    let host_matches = parsed.host_str() == Some("127.0.0.1");
    let port_matches = parsed.port_or_known_default() == Some(port);
    let path_matches = parsed.path().trim_end_matches('/') == "/v1";
    if !(host_matches && port_matches && path_matches) {
        return Err(std::io::Error::other(
            "broker base url must be http://127.0.0.1:<bound port>/v1",
        ));
    }
    Ok(Arc::from(format!("127.0.0.1:{port}").as_str()))
}

/// Serve one HTTP/1.1 connection: bounded header block/timeout, keep-alive, and a hard per-connection
/// request ceiling.
async fn serve_connection(stream: tokio::net::TcpStream, router: Router) {
    let io = TokioIo::new(stream);
    let requests = Arc::new(AtomicU32::new(0));
    let service = hyper::service::service_fn(move |req: Request<Incoming>| {
        let mut router = router.clone();
        let requests = Arc::clone(&requests);
        async move {
            let count = requests.fetch_add(1, Ordering::AcqRel) + 1;
            let mut response = match router.call(req).await {
                Ok(response) => response,
                Err(never) => match never {},
            };
            if count >= MAX_REQUESTS_PER_CONNECTION {
                response
                    .headers_mut()
                    .insert(CONNECTION, HeaderValue::from_static("close"));
            }
            Ok::<Response, std::convert::Infallible>(response)
        }
    });
    let connection = http1::Builder::new()
        .keep_alive(true)
        .max_headers(MAX_HEADER_COUNT)
        .max_buf_size(MAX_HEADER_BLOCK_BYTES)
        .header_read_timeout(HEADER_READ_TIMEOUT)
        .timer(TokioTimer::new())
        .serve_connection(io, service);
    let _ = connection.await;
}

/// Every path but the one exact route.
async fn handle_not_found() -> Response {
    refusal_response(PolicyRefusal::NotFound)
}

/// The one exact Chat Completions route (design §5.3, §5.4, §6).
async fn handle_chat(State(state): State<BrokerState>, req: Request) -> Response {
    let (parts, body) = req.into_parts();

    if parts.method != Method::POST {
        return refusal_response(PolicyRefusal::MethodNotAllowed);
    }
    // Absolute-form targets and any authority/scheme are proxies, not this route.
    if parts.uri.scheme().is_some() || parts.uri.authority().is_some() {
        return refusal_response(PolicyRefusal::InvalidRequest);
    }
    if parts.uri.query().is_some() {
        return refusal_response(PolicyRefusal::InvalidRequest);
    }
    if parts.headers.contains_key(UPGRADE) || parts.headers.contains_key(TE) {
        return refusal_response(PolicyRefusal::InvalidRequest);
    }
    // The exact expected Host emitted by the generated 127.0.0.1 base URL.
    match parts.headers.get(HOST) {
        Some(host) if host.as_bytes() == state.expected_host.as_bytes() => {}
        _ => return refusal_response(PolicyRefusal::NotFound),
    }
    // Browser origin / CORS preflight.
    if parts.headers.contains_key(ORIGIN)
        || parts.headers.contains_key(ACCESS_CONTROL_REQUEST_METHOD)
        || parts.headers.contains_key(ACCESS_CONTROL_REQUEST_HEADERS)
    {
        return refusal_response(PolicyRefusal::OriginRefused);
    }
    // Content-Encoding: absent or identity only.
    if let Some(encoding) = parts.headers.get(CONTENT_ENCODING)
        && !encoding.as_bytes().eq_ignore_ascii_case(b"identity")
    {
        return refusal_response(PolicyRefusal::ContentEncoding);
    }

    // Authentication from the bounded header block, before any body allocation.
    let token = match extract_bearer(&parts.headers) {
        Some(token) => token,
        None => return refusal_response(PolicyRefusal::Unauthorized),
    };
    let grant = match state.broker.lookup_capability(&token) {
        Ok(grant) => grant,
        Err(_) => return refusal_response(PolicyRefusal::Unauthorized),
    };

    // The grant concurrency permit is acquired before the body is read or allocated.
    let _permit = match grant.acquire_concurrency() {
        Ok(permit) => permit,
        Err(_) => return deny(&grant, PolicyRefusal::BudgetExhausted),
    };

    // Weighted request-memory budget: charged before the body is allocated.
    let max_request_bytes = grant.limits().max_request_bytes;
    let declared = declared_content_length(&parts.headers);
    if declared.is_some_and(|length| length > max_request_bytes) {
        return deny(&grant, PolicyRefusal::RequestTooLarge);
    }
    let Some(weight) = request_weight(declared, max_request_bytes) else {
        return deny(&grant, PolicyRefusal::RequestTooLarge);
    };
    let Some(_memory) = state.request_budget.try_acquire(weight) else {
        return deny(&grant, PolicyRefusal::BudgetExhausted);
    };

    // Read the body under the request byte ceiling.
    let buffer = match read_bounded_body(body, max_request_bytes).await {
        Ok(buffer) => buffer,
        Err(refusal) => return deny(&grant, refusal),
    };

    // Closed-schema validation.
    let request = match validate_chat_request(
        &buffer,
        ChatRequestPolicy {
            model: grant.model_id(),
            max_output_tokens: grant.limits().max_output_tokens_request,
        },
    ) {
        Ok(request) => request,
        Err(rejection) => return deny(&grant, map_rejection(rejection)),
    };

    // The fixed, normalized upstream endpoint (parsed once for this turn).
    let endpoint =
        match NormalizedEndpoint::parse(grant.normalized_endpoint(), grant.allow_insecure_http()) {
            Ok(endpoint) => endpoint,
            Err(_) => return deny(&grant, PolicyRefusal::ProviderMisconfigured),
        };

    // Re-serialize the validated object and apply the outbound body-size limit again.
    let outbound = request.to_json_bytes();
    if outbound.len() as u64 > max_request_bytes {
        return deny(&grant, PolicyRefusal::RequestTooLarge);
    }

    // Outbound admission: consume the forwarded-request slot immediately before construction.
    if grant
        .reserve_request(
            outbound.len() as u64,
            grant.limits().max_response_bytes,
            request.max_tokens,
        )
        .is_err()
    {
        return deny(&grant, PolicyRefusal::BudgetExhausted);
    }

    // The credential is borrowed for exactly this request and its redactor; the capability never
    // leaves the loopback. The `Authorization` header (and its transient buffer) is built inside the
    // borrow, and the buffer is zeroized on drop, so only the one request header and the redactor's
    // zeroizing copy outlive the closure.
    let Some(captured) = grant.with_credential(|key| {
        let mut buffer = zeroize::Zeroizing::new(Vec::with_capacity(7 + key.len()));
        buffer.extend_from_slice(b"Bearer ");
        buffer.extend_from_slice(key);
        HeaderValue::from_bytes(&buffer)
            .ok()
            .map(|authorization| (authorization, ZeroizingBytes::new(key.to_vec())))
    }) else {
        return refusal_response(PolicyRefusal::Unauthorized);
    };
    let Some((authorization, secret)) = captured else {
        return refusal_response(PolicyRefusal::Unauthorized);
    };

    let upstream = match state
        .client
        .forward_chat_completions(&endpoint, authorization, &outbound)
        .await
    {
        Ok(upstream) => upstream,
        // A transport failure after admission is conservatively charged (the reservation above).
        Err(_) => return refusal_response(PolicyRefusal::UpstreamUnavailable),
    };

    forward_response(&state, grant, request.stream, upstream, secret).await
}

/// Build the downstream response from a received upstream response: fresh headers, media-type
/// validation, and a bounded/redacted body (streamed for a successful SSE turn, buffered otherwise).
async fn forward_response(
    state: &BrokerState,
    grant: CapabilityGrant,
    streaming: bool,
    upstream: crate::upstream::UpstreamResponse,
    secret: ZeroizingBytes,
) -> Response {
    if upstream.has_non_identity_encoding() {
        return refusal_response(PolicyRefusal::UpstreamProtocol);
    }
    let status = upstream.status();
    let success_streaming = streaming && status.is_success();
    if status.is_success() {
        let media = upstream.media_type();
        let expected = if streaming {
            "text/event-stream"
        } else {
            "application/json"
        };
        if media.as_deref() != Some(expected) {
            return refusal_response(PolicyRefusal::UpstreamProtocol);
        }
    }

    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, canonical_content_type(success_streaming));
    forward_diagnostic_headers(
        upstream.headers(),
        &mut headers,
        &secret,
        grant.remaining_lifetime(),
    );

    let max_response_bytes = grant.limits().max_response_bytes;
    let body = if success_streaming {
        let stream = upstream.into_byte_stream();
        Body::from_stream(streaming_body(grant, stream, secret, max_response_bytes))
    } else {
        // Buffered non-streaming response: reserve the broker-wide weighted budget.
        let Some(weight) = buffered_response_weight(max_response_bytes) else {
            return refusal_response(PolicyRefusal::BudgetExhausted);
        };
        let Some(_reserved) = state.response_budget.try_acquire(weight) else {
            return refusal_response(PolicyRefusal::BudgetExhausted);
        };
        let stream = upstream.into_byte_stream();
        match buffer_body(stream, StreamingRedactor::new(secret), max_response_bytes).await {
            Ok(bytes) => Body::from(bytes),
            Err(refusal) => return refusal_response(refusal),
        }
    };

    let mut response = Response::new(body);
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}

/// The backpressured streaming pipeline (design §7.1):
/// upstream byte limit -> exact-secret redactor -> emitted byte limit -> bounded SSE observer.
fn streaming_body<S>(
    grant: CapabilityGrant,
    stream: S,
    secret: ZeroizingBytes,
    max_bytes: u64,
) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static
where
    S: Stream<Item = reqwest::Result<Bytes>> + Send + 'static,
{
    struct StreamState<S> {
        stream: std::pin::Pin<Box<S>>,
        redactor: StreamingRedactor,
        observer: SseUsageObserver,
        grant: CapabilityGrant,
        upstream_seen: u64,
        emitted: u64,
        max_bytes: u64,
        finished: bool,
    }

    let state = StreamState {
        stream: Box::pin(stream),
        redactor: StreamingRedactor::new(secret),
        observer: SseUsageObserver::new(),
        grant,
        upstream_seen: 0,
        emitted: 0,
        max_bytes,
        finished: false,
    };
    unfold(state, |mut state| async move {
        loop {
            // Cancellation: turn/session revocation and absolute expiry drop the upstream stream.
            if state.finished || !state.grant.is_live() {
                return None;
            }
            match state.stream.next().await {
                None => {
                    // Re-check after the await: a revocation during the final upstream read must not
                    // let a last byte through.
                    if !state.grant.is_live() {
                        return None;
                    }
                    state.observer.finish();
                    let tail = state.redactor.finish();
                    state.finished = true;
                    if tail.is_empty() {
                        return None;
                    }
                    state.emitted = state.emitted.saturating_add(tail.len() as u64);
                    if state.emitted > state.max_bytes {
                        return Some((Err(limit_error()), state));
                    }
                    return Some((Ok(Bytes::from(tail)), state));
                }
                Some(Err(_)) => {
                    state.finished = true;
                    return Some((Err(upstream_error()), state));
                }
                Some(Ok(chunk)) => {
                    // Re-check after the await: revocation (or expiry) during the upstream read must
                    // cancel before any byte reaches the child.
                    if !state.grant.is_live() {
                        return None;
                    }
                    state.upstream_seen = state.upstream_seen.saturating_add(chunk.len() as u64);
                    if state.upstream_seen > state.max_bytes {
                        state.finished = true;
                        return Some((Err(limit_error()), state));
                    }
                    let emitted = state.redactor.push(&chunk).unwrap_or_default();
                    if emitted.is_empty() {
                        continue;
                    }
                    state.observer.observe(&emitted);
                    state.emitted = state.emitted.saturating_add(emitted.len() as u64);
                    if state.emitted > state.max_bytes {
                        state.finished = true;
                        return Some((Err(limit_error()), state));
                    }
                    return Some((Ok(Bytes::from(emitted)), state));
                }
            }
        }
    })
}

/// Buffer a non-streaming body under the response byte ceiling, redacting and observing it.
async fn buffer_body<S>(
    stream: S,
    mut redactor: StreamingRedactor,
    max_bytes: u64,
) -> Result<Bytes, PolicyRefusal>
where
    S: Stream<Item = reqwest::Result<Bytes>> + Send + 'static,
{
    let mut stream = Box::pin(stream);
    let mut observer = SseUsageObserver::new();
    let mut out: Vec<u8> = Vec::new();
    let mut seen = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| PolicyRefusal::UpstreamProtocol)?;
        seen = seen.saturating_add(chunk.len() as u64);
        if seen > max_bytes {
            return Err(PolicyRefusal::UpstreamProtocol);
        }
        let emitted = redactor.push(&chunk).unwrap_or_default();
        if !emitted.is_empty() {
            observer.observe(&emitted);
            out.extend_from_slice(&emitted);
        }
        if out.len() as u64 > max_bytes {
            return Err(PolicyRefusal::UpstreamProtocol);
        }
    }
    let tail = redactor.finish();
    if !tail.is_empty() {
        observer.observe(&tail);
        out.extend_from_slice(&tail);
    }
    observer.finish();
    if out.len() as u64 > max_bytes {
        return Err(PolicyRefusal::UpstreamProtocol);
    }
    Ok(Bytes::from(out))
}

fn limit_error() -> std::io::Error {
    std::io::Error::other("response byte limit exceeded")
}

fn upstream_error() -> std::io::Error {
    std::io::Error::other("upstream stream failed")
}

/// Forward only the allow-listed diagnostic headers, each bounded and never reflecting the exact
/// credential (design §6.3). `Retry-After`/`Retry-After-Ms` are parsed and clamped to the remaining
/// turn lifetime.
fn forward_diagnostic_headers(
    upstream: &HeaderMap,
    out: &mut HeaderMap,
    secret: &ZeroizingBytes,
    remaining: Duration,
) {
    let secret = secret.as_slice();
    for name in FORWARDED_DIAGNOSTIC_HEADERS {
        let Ok(header_name) = HeaderName::from_bytes(name.as_bytes()) else {
            continue;
        };
        let Some(value) = upstream.get(&header_name) else {
            continue;
        };
        let raw = value.as_bytes();
        if raw.len() > MAX_DIAGNOSTIC_HEADER_BYTES || contains_secret(raw, secret) {
            continue;
        }
        // Retry delays are parsed and re-serialized in their own unit, clamped to the remaining turn
        // lifetime so the child cannot sleep past a grant that can no longer succeed (design §6.3).
        let clamped = match *name {
            "retry-after" => parse_retry_after(raw, remaining),
            "retry-after-ms" => parse_retry_after_ms(raw, remaining),
            _ => None,
        };
        if name.starts_with("retry-after") {
            if let Some(value) =
                clamped.and_then(|value| HeaderValue::from_str(&value.to_string()).ok())
            {
                out.insert(header_name, value);
            }
            continue;
        }
        if let Ok(value) = HeaderValue::from_bytes(raw) {
            out.insert(header_name, value);
        }
    }
}

/// Extract exactly one `Authorization: Bearer <token>` value (design §5.2). Missing, repeated, or
/// malformed headers all return `None` and collapse to the same 401.
fn extract_bearer(headers: &HeaderMap) -> Option<String> {
    let mut values = headers.get_all(AUTHORIZATION).iter();
    let value = values.next()?;
    if values.next().is_some() {
        return None;
    }
    let text = value.to_str().ok()?;
    let (scheme, token) = text.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") || token.is_empty() || token.contains(' ') {
        return None;
    }
    Some(token.to_owned())
}

/// A valid `Content-Length`, if present and parseable.
fn declared_content_length(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
}

/// Read the request body under `max_bytes`, never allocating past the ceiling.
async fn read_bounded_body(body: Body, max_bytes: u64) -> Result<Vec<u8>, PolicyRefusal> {
    let mut body = body;
    let mut buffer = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|_| PolicyRefusal::InvalidRequest)?;
        if let Some(data) = frame.data_ref() {
            if buffer.len() as u64 + data.len() as u64 > max_bytes {
                return Err(PolicyRefusal::RequestTooLarge);
            }
            buffer.extend_from_slice(data);
        }
    }
    Ok(buffer)
}

/// Map a schema rejection to its pinned policy refusal.
/// Refuse an authenticated request and count the local denial against the turn's bounded abuse
/// counter (design §5.2, §8.2). Reaching the configured threshold is counted and refused here; the
/// turn's owner (the worker retaining the receipt) performs the revocation.
fn deny(grant: &CapabilityGrant, refusal: PolicyRefusal) -> Response {
    let _ = grant.record_denied();
    refusal_response(refusal)
}

/// Map a schema rejection to its pinned policy refusal.
fn map_rejection(rejection: RequestRejection) -> PolicyRefusal {
    match rejection {
        RequestRejection::UnknownField => PolicyRefusal::UnknownField,
        RequestRejection::ModelMismatch => PolicyRefusal::ModelMismatch,
        RequestRejection::RemoteContentRefused => PolicyRefusal::UnsupportedContent,
        RequestRejection::InvalidTool => PolicyRefusal::InvalidTool,
        RequestRejection::TooManyItems => PolicyRefusal::TooManyItems,
        RequestRejection::Malformed(_)
        | RequestRejection::NotAnObject
        | RequestRejection::MissingField
        | RequestRejection::InvalidMaxTokens
        | RequestRejection::InvalidStream
        | RequestRejection::InvalidStreamOptions
        | RequestRejection::InvalidMessage => PolicyRefusal::InvalidRequest,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_extraction_is_exact() {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, HeaderValue::from_static("Bearer abc"));
        assert_eq!(extract_bearer(&headers), Some("abc".to_owned()));

        headers.insert(AUTHORIZATION, HeaderValue::from_static("bearer abc"));
        assert_eq!(extract_bearer(&headers), Some("abc".to_owned()));

        // Two Authorization headers collapse to Unauthorized.
        let mut double = HeaderMap::new();
        double.append(AUTHORIZATION, HeaderValue::from_static("Bearer a"));
        double.append(AUTHORIZATION, HeaderValue::from_static("Bearer b"));
        assert_eq!(extract_bearer(&double), None);

        for bad in ["Basic abc", "Bearer", "Bearer  a", "abc"] {
            let mut headers = HeaderMap::new();
            headers.insert(AUTHORIZATION, HeaderValue::from_static(bad));
            assert_eq!(extract_bearer(&headers), None, "{bad} must be refused");
        }
    }

    #[test]
    fn schema_rejections_map_to_pinned_refusals() {
        assert_eq!(
            map_rejection(RequestRejection::UnknownField),
            PolicyRefusal::UnknownField
        );
        assert_eq!(
            map_rejection(RequestRejection::RemoteContentRefused),
            PolicyRefusal::UnsupportedContent
        );
        assert_eq!(
            map_rejection(RequestRejection::Malformed(
                crate::schema::SchemaError::DuplicateKey
            )),
            PolicyRefusal::InvalidRequest
        );
    }
}
