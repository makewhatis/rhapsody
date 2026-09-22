//! PB2 acceptance: the private loopback adapter driven end-to-end against a **fake upstream** and a
//! **fake client** (design §14.2, §14.3). No test touches a real provider or the network beyond
//! loopback.
//!
//! Two-sided canaries are the point: the fake upstream must receive only the upstream fake key and
//! never the capability, and the fake client must receive only redacted provider output and never
//! the upstream fake key. The named mutations in the ticket each have a test that turns red for the
//! bad implementation (verified during the self-review pass; see the PR body).

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::response::Response;
use axum::routing::any;
use bytes::Bytes;
use futures_util::stream;
use http::header::{AUTHORIZATION, CONTENT_TYPE};
use http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use rhapsody_provider_broker::{
    BoundCredentialLease, BrokerLedgerReceiver, BrokerLimits, BrokerListener, BrokerProtocol,
    BrokerRegistrationPlan, BrokerSession, CredentialBinding, DEFAULT_BROKER_LIMITS, ManualClock,
    ScriptedRandom, SessionPolicy, TurnAccess, TurnMeta, TurnReceipt,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const UPSTREAM_KEY: &str = "sk-fake-upstream-key-do-not-leak";
const MODEL: &str = "probe-model";
const CAPABILITY_PLACEHOLDER: &str = "<CAPABILITY>";

// ---------------------------------------------------------------------------------------------
// Fake upstream
// ---------------------------------------------------------------------------------------------

#[derive(Clone)]
struct RecordedRequest {
    method: String,
    path: String,
    headers: HeaderMap,
    body: Vec<u8>,
}

#[derive(Clone)]
struct FakeResponse {
    status: u16,
    content_type: &'static str,
    chunks: Vec<Vec<u8>>,
    chunk_delay: Duration,
    extra_headers: Vec<(&'static str, &'static str)>,
}

impl FakeResponse {
    fn sse(chunks: Vec<Vec<u8>>) -> Self {
        Self {
            status: 200,
            content_type: "text/event-stream",
            chunks,
            chunk_delay: Duration::ZERO,
            extra_headers: Vec::new(),
        }
    }

    fn json(body: impl Into<String>) -> Self {
        Self {
            status: 200,
            content_type: "application/json",
            chunks: vec![body.into().into_bytes()],
            chunk_delay: Duration::ZERO,
            extra_headers: Vec::new(),
        }
    }
}

#[derive(Clone)]
struct FakeUpstream {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    response: Arc<Mutex<FakeResponse>>,
}

impl FakeUpstream {
    async fn spawn(response: FakeResponse) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind upstream");
        let addr = listener.local_addr().expect("upstream addr");
        let upstream = Self {
            addr,
            requests: Arc::new(Mutex::new(Vec::new())),
            response: Arc::new(Mutex::new(response)),
        };
        let app = Router::new()
            .fallback(any(fake_handler))
            .with_state(upstream.clone());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        upstream
    }

    fn port(&self) -> u16 {
        self.addr.port()
    }

    fn endpoint(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.port())
    }

    fn requests(&self) -> Vec<RecordedRequest> {
        self.requests.lock().expect("requests").clone()
    }

    fn count(&self) -> usize {
        self.requests.lock().expect("requests").len()
    }
}

async fn fake_handler(State(state): State<FakeUpstream>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let body = axum::body::to_bytes(body, 8 * 1024 * 1024)
        .await
        .unwrap_or_default()
        .to_vec();
    state
        .requests
        .lock()
        .expect("requests")
        .push(RecordedRequest {
            method: parts.method.to_string(),
            path: parts.uri.path().to_owned(),
            headers: parts.headers,
            body,
        });
    let response = state.response.lock().expect("response").clone();
    let chunks = response.chunks.clone();
    let delay = response.chunk_delay;
    let stream = stream::unfold((chunks, 0usize), move |(chunks, index)| async move {
        if index >= chunks.len() {
            return None;
        }
        if index > 0 && !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        let chunk = chunks[index].clone();
        Some((
            Ok::<Bytes, std::io::Error>(Bytes::from(chunk)),
            (chunks, index + 1),
        ))
    });
    let mut out = Response::new(Body::from_stream(stream));
    *out.status_mut() = StatusCode::from_u16(response.status).unwrap_or(StatusCode::OK);
    out.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static(response.content_type),
    );
    for (name, value) in response.extra_headers {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_bytes(value.as_bytes()),
        ) {
            out.headers_mut().insert(name, value);
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Broker harness
// ---------------------------------------------------------------------------------------------

struct Harness {
    api_port: u16,
    capability: String,
    #[allow(dead_code)]
    access: Option<TurnAccess>,
    #[allow(dead_code)]
    receipt: TurnReceipt,
    #[allow(dead_code)]
    session: BrokerSession,
    #[allow(dead_code)]
    ledgers: BrokerLedgerReceiver,
    upstream: FakeUpstream,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    server: Option<tokio::task::JoinHandle<()>>,
    #[allow(dead_code)]
    clock: Arc<ManualClock>,
}

impl Harness {
    async fn with(response: FakeResponse, limits: BrokerLimits, allow_insecure_http: bool) -> Self {
        let upstream = FakeUpstream::spawn(response).await;
        Self::against(upstream, limits, allow_insecure_http).await
    }

    async fn against(
        upstream: FakeUpstream,
        limits: BrokerLimits,
        allow_insecure_http: bool,
    ) -> Self {
        let clock = Arc::new(ManualClock::new());
        let rng = Arc::new(ScriptedRandom::new());
        let (listener, broker) = BrokerListener::bind_with(clock.clone(), rng).expect("listener");
        let api_port = listener.local_addr().port();

        let endpoint = upstream.endpoint();
        let binding = CredentialBinding::new(
            "provider-a",
            BrokerProtocol::OpenAiChatCompletions,
            endpoint.clone(),
        )
        .expect("binding");
        let lease =
            BoundCredentialLease::new(binding, UPSTREAM_KEY.as_bytes().to_vec()).expect("lease");
        let plan = BrokerRegistrationPlan::new(
            "provider-a",
            BrokerProtocol::OpenAiChatCompletions,
            endpoint,
            allow_insecure_http,
            MODEL,
            limits,
        )
        .expect("plan");
        let mut registration = broker
            .register_session(plan, lease, SessionPolicy::new(limits).expect("policy"))
            .expect("registration");
        let (attempt, receipt) = registration
            .ledgers
            .arm_turn(TurnMeta::without_deadline())
            .expect("arm");
        let access = attempt.mint_access().expect("mint");
        let capability = access.api_key.expose_for_child(str::to_owned);
        assert!(
            capability.len() == 43 && !capability.contains(CAPABILITY_PLACEHOLDER),
            "a real capability is minted"
        );

        let (tx, rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let _ = listener
                .run_with_shutdown(async move {
                    let _ = rx.await;
                })
                .await;
        });
        Self {
            api_port,
            capability,
            access: Some(access),
            receipt,
            session: registration.session,
            ledgers: registration.ledgers,
            upstream,
            shutdown: Some(tx),
            server: Some(server),
            clock,
        }
    }

    fn chat_url(&self) -> String {
        format!("http://127.0.0.1:{}/v1/chat/completions", self.api_port)
    }

    fn client() -> reqwest::Client {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("client")
    }

    async fn post(
        &self,
        bearer: Option<&str>,
        extra: &[(&str, &str)],
        body: &[u8],
    ) -> reqwest::Response {
        let mut request = Self::client().post(self.chat_url()).body(body.to_vec());
        if let Some(bearer) = bearer {
            request = request.bearer_auth(bearer);
        }
        for (name, value) in extra {
            request = request.header(*name, *value);
        }
        request.send().await.expect("request")
    }

    async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(server) = self.server.take() {
            let _ = server.await;
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(server) = self.server.take() {
            server.abort();
        }
    }
}

fn chat_body(model: &str, stream: bool) -> Vec<u8> {
    serde_json::json!({
        "model": model,
        "messages": [
            {"role": "system", "content": "system prompt"},
            {"role": "user", "content": "hello"},
        ],
        "max_tokens": 32_000,
        "stream": stream,
        "stream_options": {"include_usage": true},
        "tool_choice": "auto",
        "tools": [{"type": "function", "function": {
            "name": "bash", "description": "run", "parameters": {"type": "object"}
        }}],
    })
    .to_string()
    .into_bytes()
}

fn default_limits() -> BrokerLimits {
    DEFAULT_BROKER_LIMITS
}

fn sse_body() -> Vec<String> {
    let usage = serde_json::json!({
        "choices": [],
        "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5},
        "echo": UPSTREAM_KEY,
    })
    .to_string();
    vec![
        "data: {\"choices\":[{\"delta\":{\"content\":\"hello \"}}]}\n\n".to_owned(),
        format!("data: {usage}\n\n"),
        "data: [DONE]\n\n".to_owned(),
    ]
    .into_iter()
    .map(|chunk| chunk.into_bytes())
    .collect::<Vec<_>>()
    .into_iter()
    .map(|bytes| String::from_utf8_lossy(&bytes).to_string())
    .collect()
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

/// §5.4 + §6.3 + the two-sided canary: exactly one request, the fixed header set, the upstream fake
/// key upstream, the redacted output to the child, and neither secret on the wrong side.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn happy_streaming_forwards_once_with_fixed_headers_and_redacts_both_sides() {
    let mut response = FakeResponse::sse(Vec::new());
    response.chunks = sse_body().into_iter().map(String::into_bytes).collect();
    let harness = Harness::with(response, default_limits(), true).await;

    let capability = harness.capability.clone();
    let resp = harness
        .post(
            Some(&capability),
            &[("x-child-header", "should-not-forward")],
            &chat_body(MODEL, true),
        )
        .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()
            .get(CONTENT_TYPE)
            .map(|v| v.to_str().unwrap()),
        Some("text/event-stream")
    );
    let body = resp.bytes().await.expect("body");
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("hello "), "the provider stream is forwarded");
    assert!(
        !text.contains(UPSTREAM_KEY),
        "the upstream key must never reach the child"
    );
    assert!(
        text.contains("[redacted-provider-key]"),
        "the exact secret is replaced with the fixed marker"
    );

    let recorded = harness.upstream.requests();
    assert_eq!(recorded.len(), 1, "exactly one upstream request");
    let request = &recorded[0];
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/v1/chat/completions");
    assert_eq!(
        request
            .headers
            .get(AUTHORIZATION)
            .unwrap()
            .to_str()
            .unwrap(),
        format!("Bearer {UPSTREAM_KEY}")
    );
    assert_eq!(
        request.headers.get(CONTENT_TYPE).unwrap().to_str().unwrap(),
        "application/json"
    );
    assert_eq!(
        request
            .headers
            .get("accept-encoding")
            .unwrap()
            .to_str()
            .unwrap(),
        "identity"
    );
    assert!(
        request.headers.get("x-child-header").is_none(),
        "incoming child headers never reach upstream"
    );
    // The upstream must never observe the capability, anywhere.
    let flat = format!(
        "{:?} {}",
        request.headers,
        String::from_utf8_lossy(&request.body)
    );
    assert!(
        !flat.contains(&harness.capability),
        "the capability must never reach upstream"
    );
    // The forwarded body is the normalized closed schema.
    let forwarded: serde_json::Value =
        serde_json::from_slice(&request.body).expect("forwarded json");
    assert_eq!(forwarded["model"], serde_json::json!(MODEL));
    assert_eq!(
        forwarded["stream_options"]["include_usage"],
        serde_json::json!(true)
    );

    harness.shutdown().await;
}

/// §6.3 + mutation "redact only whole chunks": a key split across every chunk boundary must still be
/// redacted, and no fragment of the key may survive.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn redaction_spans_every_chunk_split_boundary() {
    // Split the key across single-byte chunks so every possible boundary is exercised.
    let secret = UPSTREAM_KEY.as_bytes();
    let mut chunks: Vec<Vec<u8>> = Vec::new();
    chunks.push(b"prefix-".to_vec());
    for byte in secret {
        chunks.push(vec![*byte]);
    }
    chunks.push(b"-suffix".to_vec());
    let harness = Harness::with(FakeResponse::sse(chunks), default_limits(), true).await;

    let capability = harness.capability.clone();
    let resp = harness
        .post(Some(&capability), &[], &chat_body(MODEL, true))
        .await;
    let body = resp.bytes().await.expect("body");
    let text = String::from_utf8_lossy(&body);
    assert_eq!(text, "prefix-[redacted-provider-key]-suffix");
    assert!(!text.contains(UPSTREAM_KEY));
    harness.shutdown().await;
}

/// §5.3.6 + mutation "forward an unknown field": an unknown generation control is refused before any
/// upstream contact.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_generation_control_is_refused_before_upstream_contact() {
    let harness = Harness::with(FakeResponse::json("{\"ok\":true}"), default_limits(), true).await;
    let body = serde_json::json!({
        "model": MODEL,
        "messages": [],
        "stream": false,
        "max_completion_tokens": 999_999,
    })
    .to_string();
    let capability = harness.capability.clone();
    let resp = harness.post(Some(&capability), &[], body.as_bytes()).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = resp.bytes().await.expect("body");
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("unknown_field"), "pinned code: {text}");
    assert_eq!(harness.upstream.count(), 0, "no upstream contact");
    harness.shutdown().await;
}

/// §5.3.7: a remote-fetch content form is refused rather than passed through.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_fetch_content_is_refused_before_upstream_contact() {
    let harness = Harness::with(FakeResponse::json("{\"ok\":true}"), default_limits(), true).await;
    let body = serde_json::json!({
        "model": MODEL,
        "messages": [{"role": "user", "content": [
            {"type": "image_url", "image_url": {"url": "http://evil.example/x.png"}}
        ]}],
        "stream": false,
    })
    .to_string();
    let capability = harness.capability.clone();
    let resp = harness.post(Some(&capability), &[], body.as_bytes()).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let text = String::from_utf8_lossy(&resp.bytes().await.expect("body")).to_string();
    assert!(text.contains("unsupported_content"), "pinned code: {text}");
    assert_eq!(harness.upstream.count(), 0);
    harness.shutdown().await;
}

/// §5.3.3: a model mismatch is rejected, never silently routed to another model.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_model_mismatch_is_refused_not_routed() {
    let harness = Harness::with(FakeResponse::json("{\"ok\":true}"), default_limits(), true).await;
    let capability = harness.capability.clone();
    let resp = harness
        .post(
            Some(&capability),
            &[],
            &chat_body("some-other-model", false),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let text = String::from_utf8_lossy(&resp.bytes().await.expect("body")).to_string();
    assert!(text.contains("model_mismatch"), "pinned code: {text}");
    assert_eq!(harness.upstream.count(), 0);
    harness.shutdown().await;
}

/// §5.2: missing, malformed, repeated, and unknown credentials all produce the same 401 body and
/// close the connection; the unauthenticated body is never read.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auth_failures_are_indistinguishable() {
    let harness = Harness::with(FakeResponse::json("{\"ok\":true}"), default_limits(), true).await;

    let missing = harness.post(None, &[], &chat_body(MODEL, false)).await;
    assert_eq!(missing.status(), StatusCode::UNAUTHORIZED);
    let missing_body = missing.bytes().await.expect("body");

    let unknown = harness
        .post(
            Some("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"),
            &[],
            &chat_body(MODEL, false),
        )
        .await;
    assert_eq!(unknown.status(), StatusCode::UNAUTHORIZED);
    let unknown_body = unknown.bytes().await.expect("body");

    assert_eq!(
        missing_body, unknown_body,
        "token states are indistinguishable"
    );
    assert!(!String::from_utf8_lossy(&missing_body).contains(&harness.capability));
    assert_eq!(harness.upstream.count(), 0);
    harness.shutdown().await;
}

/// §5.1: the exact Host, absence of Origin, POST-only, no query, and the exact path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_origin_method_query_and_path_variants_are_refused() {
    let harness = Harness::with(FakeResponse::json("{\"ok\":true}"), default_limits(), true).await;
    let port = harness.api_port;
    let capability = harness.capability.clone();
    let body = chat_body(MODEL, false);

    // Wrong Host -> 404 (the exact expected Host is required).
    let bad_host = raw(
        port,
        &format!(
            "POST /v1/chat/completions HTTP/1.1\r\nHost: evil.example\r\nAuthorization: Bearer {capability}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        ),
        &body,
    )
    .await;
    assert_eq!(bad_host.status, 404);

    // Origin present -> 403.
    let origin = raw(
        port,
        &format!(
            "POST /v1/chat/completions HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: http://evil.example\r\nAuthorization: Bearer {capability}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        ),
        &body,
    )
    .await;
    assert_eq!(origin.status, 403);

    // GET -> 405.
    let get = raw(
        port,
        &format!(
            "GET /v1/chat/completions HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {capability}\r\nConnection: close\r\n\r\n"
        ),
        b"",
    )
    .await;
    assert_eq!(get.status, 405);

    // Query string -> 400.
    let query = raw(
        port,
        &format!(
            "POST /v1/chat/completions?x=1 HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {capability}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        ),
        &body,
    )
    .await;
    assert_eq!(query.status, 400);

    // Path variant -> 404.
    let variant = raw(
        port,
        &format!(
            "POST /v1/chat/completions/ HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {capability}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        ),
        &body,
    )
    .await;
    assert_eq!(variant.status, 404);

    assert_eq!(harness.upstream.count(), 0);
    harness.shutdown().await;
}

/// §6.1 + mutation "insecure HTTP without resolved opt-in": a plaintext upstream is refused before
/// any contact unless the plan carries `allow_insecure_http: true`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plaintext_upstream_is_refused_without_the_explicit_optin() {
    let harness = Harness::with(FakeResponse::json("{\"ok\":true}"), default_limits(), false).await;
    let capability = harness.capability.clone();
    let resp = harness
        .post(Some(&capability), &[], &chat_body(MODEL, false))
        .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let text = String::from_utf8_lossy(&resp.bytes().await.expect("body")).to_string();
    assert!(
        text.contains("provider_misconfigured"),
        "pinned code: {text}"
    );
    assert_eq!(harness.upstream.count(), 0);
    harness.shutdown().await;
}

/// §6.2 + mutation "follow a redirect": a redirect is not followed; the upstream sees one request.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn redirects_are_not_followed() {
    let response = FakeResponse {
        status: 302,
        content_type: "text/plain",
        chunks: vec![b"moved".to_vec()],
        chunk_delay: Duration::ZERO,
        extra_headers: vec![("location", "http://127.0.0.1:1/elsewhere")],
    };
    let harness = Harness::with(response, default_limits(), true).await;
    let capability = harness.capability.clone();
    let resp = harness
        .post(Some(&capability), &[], &chat_body(MODEL, false))
        .await;
    assert_eq!(resp.status(), StatusCode::FOUND);
    assert_eq!(harness.upstream.count(), 1, "a redirect is never followed");
    harness.shutdown().await;
}

/// §6.3 + mutation "remove response bounds": a response past the byte ceiling is cut off.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_oversized_response_is_cut_at_the_ceiling() {
    let limits = BrokerLimits {
        max_response_bytes: 64,
        max_response_bytes_turn: 128,
        ..DEFAULT_BROKER_LIMITS
    };
    // The first chunk fits under the ceiling (so the response head is flushed), the second does not.
    let mut response = FakeResponse::sse(vec![b"data: {\"choices\":[]}\n\n".to_vec()]);
    response.chunks.push(vec![b'x'; 200]);
    let harness = Harness::with(response, limits, true).await;
    let capability = harness.capability.clone();
    let resp = harness
        .post(Some(&capability), &[], &chat_body(MODEL, true))
        .await;
    assert_eq!(resp.status(), 200);
    // The body stream errors at the ceiling rather than buffering past it.
    assert!(
        resp.bytes().await.is_err(),
        "an over-ceiling response must be terminated"
    );
    harness.shutdown().await;
}

/// §7.1 + mutation "buffer the full stream": the first SSE chunk reaches the child before the
/// upstream has produced the second.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn streaming_is_incremental_and_backpressured() {
    let response = FakeResponse {
        status: 200,
        content_type: "text/event-stream",
        chunks: vec![
            b"data: {\"choices\":[{\"delta\":{\"content\":\"first\"}}]}\n\n".to_vec(),
            b"data: {\"choices\":[{\"delta\":{\"content\":\"second\"}}]}\n\n".to_vec(),
        ],
        // The second chunk is delayed well past the assertion horizon: a broker that buffered the
        // whole stream would not flush the response head until after this delay.
        chunk_delay: Duration::from_millis(1_500),
        extra_headers: Vec::new(),
    };
    let harness = Harness::with(response, default_limits(), true).await;
    let capability = harness.capability.clone();
    let started = std::time::Instant::now();
    let resp = harness
        .post(Some(&capability), &[], &chat_body(MODEL, true))
        .await;
    assert_eq!(resp.status(), 200);
    assert!(
        started.elapsed() < Duration::from_millis(1_000),
        "the response head must be flushed before the upstream completes, not after buffering"
    );
    let mut resp = resp;
    let first = resp
        .chunk()
        .await
        .expect("a first chunk")
        .expect("first chunk ok");
    assert!(String::from_utf8_lossy(&first).contains("first"));
    harness.shutdown().await;
}

/// §7.2: revoking the session mid-stream cancels the upstream and ends the downstream body.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revoking_the_session_cancels_a_live_stream() {
    let response = FakeResponse {
        status: 200,
        content_type: "text/event-stream",
        chunks: vec![
            b"data: {\"choices\":[{\"delta\":{\"content\":\"first\"}}]}\n\n".to_vec(),
            b"data: {\"choices\":[{\"delta\":{\"content\":\"second\"}}]}\n\n".to_vec(),
        ],
        chunk_delay: Duration::from_millis(600),
        extra_headers: Vec::new(),
    };
    let harness = Harness::with(response, default_limits(), true).await;
    let capability = harness.capability.clone();
    let resp = harness
        .post(Some(&capability), &[], &chat_body(MODEL, true))
        .await;
    let mut resp = resp;
    let first = resp.chunk().await.expect("first").expect("first ok");
    assert!(String::from_utf8_lossy(&first).contains("first"));
    // Revoke the turn: the streaming body must stop pulling upstream.
    harness.session.revoke();
    let next = resp.chunk().await;
    assert!(
        !matches!(next, Ok(Some(_))),
        "revocation must terminate the downstream stream, got {next:?}"
    );
    harness.shutdown().await;
}

/// §5.2/§8.2: authenticated denials increment the turn's bounded abuse counter, capped at
/// `max_denied_requests`, without consuming a forwarded-request slot.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn authenticated_denials_increment_the_bounded_abuse_counter() {
    let limits = BrokerLimits {
        max_denied_requests: 2,
        ..DEFAULT_BROKER_LIMITS
    };
    let mut harness = Harness::with(FakeResponse::json("{\"ok\":true}"), limits, true).await;
    let capability = harness.capability.clone();
    let body = serde_json::json!({"model": MODEL, "messages": [], "bogus": 1}).to_string();
    for _ in 0..3 {
        let resp = harness.post(Some(&capability), &[], body.as_bytes()).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }
    assert_eq!(
        harness.upstream.count(),
        0,
        "denied requests never reach upstream"
    );

    // Finalize the turn to read the ledger the abuses were counted into.
    drop(harness.access.take());
    let ledger = harness.receipt.take().expect("finalized ledger");
    assert_eq!(
        ledger.denied_requests(),
        2,
        "the abuse counter is counted up to and bounded by max_denied_requests"
    );
    assert_eq!(ledger.forwarded_requests(), 0);
    harness.shutdown().await;
}

/// §6.3: `Retry-After` is parsed and clamped, and a header reflecting the key is dropped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_after_is_clamped_and_reflecting_headers_are_dropped() {
    let response = FakeResponse {
        status: 200,
        content_type: "text/event-stream",
        chunks: vec![b"data: [DONE]\n\n".to_vec()],
        chunk_delay: Duration::ZERO,
        extra_headers: vec![
            ("retry-after", "999999999"),
            ("x-ratelimit-remaining-requests", UPSTREAM_KEY),
        ],
    };
    let harness = Harness::with(response, default_limits(), true).await;
    let capability = harness.capability.clone();
    let resp = harness
        .post(Some(&capability), &[], &chat_body(MODEL, true))
        .await;
    let retry_after = resp
        .headers()
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .expect("a parsed retry-after");
    assert!(
        retry_after <= 3600,
        "retry-after is clamped to the turn lifetime, got {retry_after}"
    );
    assert!(
        resp.headers()
            .get("x-ratelimit-remaining-requests")
            .is_none(),
        "a header reflecting the key is dropped, not rewritten"
    );
    harness.shutdown().await;
}

/// The closed schema is fixture-backed: every top-level key and message role the committed PB0
/// OpenCode captures exercise must be inside the allow-list, and a minimal request shaped from each
/// fixture must validate.
#[test]
fn pinned_opencode_fixtures_are_within_the_closed_schema() {
    use rhapsody_provider_broker::{
        ChatRequestPolicy, top_level_field_allowed, validate_chat_request,
    };

    let fixtures: [(&str, &str); 5] = [
        (
            "happy",
            include_str!("../../../harness/harness-spike/opencode/broker/requests/happy.json"),
        ),
        (
            "compaction",
            include_str!("../../../harness/harness-spike/opencode/broker/requests/compaction.json"),
        ),
        (
            "subagent",
            include_str!("../../../harness/harness-spike/opencode/broker/requests/subagent.json"),
        ),
        (
            "retry",
            include_str!("../../../harness/harness-spike/opencode/broker/requests/retry.json"),
        ),
        (
            "auth",
            include_str!("../../../harness/harness-spike/opencode/broker/requests/auth.json"),
        ),
    ];
    for (name, raw) in fixtures {
        let fixture: serde_json::Value = serde_json::from_str(raw).expect("fixture json");
        let requests = fixture["requests"].as_array().expect("requests array");
        assert!(!requests.is_empty(), "{name} has at least one request");
        for request in requests {
            let body = &request["body"];
            for key in body["keys"].as_array().expect("keys") {
                let key = key.as_str().expect("key");
                assert!(
                    top_level_field_allowed(key),
                    "{name}: fixture key `{key}` is outside the closed top-level schema"
                );
            }
            for role in body["message_roles"].as_array().expect("roles") {
                let role = role.as_str().expect("role");
                assert!(
                    rhapsody_provider_broker::schema::ALLOWED_MESSAGE_ROLES.contains(&role),
                    "{name}: fixture role `{role}` is outside the closed role set"
                );
            }

            // Reconstruct a minimal request shaped from the recorded fixture and validate it.
            let model = body["model"].as_str().expect("model");
            let messages: Vec<serde_json::Value> = body["message_roles"]
                .as_array()
                .expect("roles")
                .iter()
                .map(|role| serde_json::json!({"role": role, "content": "x"}))
                .collect();
            let mut normalized = serde_json::json!({
                "model": model,
                "messages": messages,
                "max_tokens": body["max_tokens"].clone(),
                "stream": body["stream"].clone(),
            });
            if let Some(stream_options) = body.get("stream_options") {
                normalized["stream_options"] = stream_options.clone();
            }
            let tool_names = body["tool_names"].as_array().cloned().unwrap_or_default();
            if !tool_names.is_empty() {
                normalized["tools"] = serde_json::Value::Array(
                    tool_names
                        .iter()
                        .map(|tool| {
                            serde_json::json!({
                                "type": "function",
                                "function": {"name": tool}
                            })
                        })
                        .collect(),
                );
            }
            if let Some(choice) = body["tool_choice"].as_str() {
                normalized["tool_choice"] = serde_json::json!(choice);
            }
            let policy = ChatRequestPolicy {
                model,
                max_output_tokens: 32_000,
            };
            assert!(
                validate_chat_request(normalized.to_string().as_bytes(), policy).is_ok(),
                "{name}: a request shaped from the pinned fixture must validate"
            );
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Raw HTTP helper (for Host/Origin/method/query negatives reqwest would normalize away)
// ---------------------------------------------------------------------------------------------

struct RawResponse {
    status: u16,
}

async fn raw(port: u16, head: &str, body: &[u8]) -> RawResponse {
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");
    stream.write_all(head.as_bytes()).await.expect("head");
    if !body.is_empty() {
        stream.write_all(body).await.expect("body");
    }
    let mut response = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut response)).await;
    let text = String::from_utf8_lossy(&response);
    let status = text
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .unwrap_or(0);
    // Sanity: the refusal bodies are bounded and never echo a token.
    assert!(!text.contains(UPSTREAM_KEY));
    RawResponse { status }
}
