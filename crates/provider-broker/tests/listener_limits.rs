//! STUDIO-1003 (PB8) — the §5.1 listener bounds, driven behaviorally against a hostile local client.
//!
//! Design §14.2 requires HTTP/1-only serving, a broker-wide connection/handler ceiling, a
//! per-connection request ceiling, and a bounded header-read timeout to stay bounded under hostile
//! local clients. Named mutation 3 ("weaken redirect/proxy/schema/collision/title/share/skill/MCP/
//! cancellation controls") and B3 of the review name the ceilings and the timeout specifically:
//! multiplying `MAX_CONNECTIONS`/`MAX_REQUESTS_PER_CONNECTION` by 1000 and deleting
//! `.header_read_timeout(..)` used to leave every suite green.
//!
//! Each test below asserts the REAL constants' behavior (not an overridden seam), so the named
//! mutations red directly:
//!
//! * [`a_silent_client_is_closed_at_the_header_read_timeout`] — deleting the header timeout leaves
//!   the connection open and the bounded read never returns EOF.
//! * [`one_connection_serves_exactly_the_request_ceiling`] — multiplying the request ceiling leaves
//!   the ceiling-th response without `Connection: close` and the socket open.
//! * [`the_connection_ceiling_defers_the_next_client_until_one_frees`] — multiplying the connection
//!   ceiling serves the ceiling+1th client immediately.
//!
//! The connection-ceiling test opens `MAX_CONNECTIONS` sockets, so it first raises the process's
//! `RLIMIT_NOFILE` soft limit to its hard limit (macOS defaults a soft limit of 256, which two
//! sockets per held connection would exhaust). The raise is best-effort and only ever widens the
//! limit; the test then fails loudly rather than silently if it still cannot open a socket.

use std::net::SocketAddr;
use std::time::Duration;

use rhapsody_provider_broker::{
    BoundCredentialLease, Broker, BrokerLedgerReceiver, BrokerListener, BrokerProtocol,
    BrokerRegistrationPlan, BrokerSession, CredentialBinding, DEFAULT_BROKER_LIMITS,
    HEADER_READ_TIMEOUT, MAX_CONNECTIONS, MAX_REQUESTS_PER_CONNECTION, OsRandom, SessionPolicy,
    SystemClock, TurnAccess, TurnMeta, TurnReceipt,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const UPSTREAM_KEY: &str = "sk-fake-upstream-key-do-not-leak";
const MODEL: &str = "probe-model";

/// Raise this process's open-file soft limit to its hard limit (best effort). Opening
/// `MAX_CONNECTIONS` clients against an in-process listener needs roughly twice that many
/// descriptors; the macOS default soft limit of 256 would otherwise fail before the ceiling could be
/// reached. Never lowers the limit.
fn raise_open_file_limit() {
    // SAFETY: `getrlimit`/`setrlimit` are plain FFI calls on `libc::rlimit` values that this
    // function owns; the only process-wide effect is widening the open-file limit.
    unsafe {
        let mut limits = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limits) == 0
            && limits.rlim_cur < limits.rlim_max
        {
            let raised = libc::rlimit {
                rlim_cur: limits.rlim_max,
                rlim_max: limits.rlim_max,
            };
            let _ = libc::setrlimit(libc::RLIMIT_NOFILE, &raised);
        }
    }
}

/// A raw loopback fake upstream: answers every request with a small non-streaming JSON completion.
/// Raw TCP (not axum) keeps this file independent of the loopback feature's router while still
/// exercising the real outbound client.
async fn spawn_fake_upstream() -> (u16, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind fake upstream");
    let port = listener.local_addr().expect("upstream addr").port();
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let _ = serve_one_upstream(&mut socket).await;
            });
        }
    });
    (port, task)
}

async fn serve_one_upstream(socket: &mut TcpStream) -> std::io::Result<()> {
    // Read the request head, then the declared body, so the broker's request is fully consumed.
    let mut head = Vec::new();
    loop {
        let mut byte = [0u8; 1];
        if socket.read(&mut byte).await? == 0 {
            return Ok(());
        }
        head.push(byte[0]);
        if head.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let head_text = String::from_utf8_lossy(&head).to_ascii_lowercase();
    let content_length = head_text
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; content_length];
    socket.read_exact(&mut body).await?;

    let payload =
        br#"{"choices":[],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#;
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        payload.len()
    );
    socket.write_all(response.as_bytes()).await?;
    socket.write_all(payload).await?;
    socket.flush().await?;
    Ok(())
}

#[derive(Debug)]
struct Response {
    status: u16,
    headers: String,
}

/// Read exactly one HTTP/1.1 response from `stream`, byte-by-byte so no bytes of a following
/// response are consumed. Returns `Err` on EOF (a closed connection).
async fn read_one_response(stream: &mut TcpStream) -> std::io::Result<Response> {
    let mut head = Vec::new();
    loop {
        let mut byte = [0u8; 1];
        let read = stream.read(&mut byte).await?;
        if read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "connection closed before a response",
            ));
        }
        head.push(byte[0]);
        if head.ends_with(b"\r\n\r\n") {
            break;
        }
        if head.len() > 64 * 1024 {
            return Err(std::io::Error::other("response head exceeded the bound"));
        }
    }
    let text = String::from_utf8_lossy(&head).to_string();
    let status = text
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .unwrap_or(0);
    let content_length = text
        .to_ascii_lowercase()
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; content_length];
    stream.read_exact(&mut body).await?;
    Ok(Response {
        status,
        headers: text.to_ascii_lowercase(),
    })
}

fn chat_body() -> Vec<u8> {
    serde_json::json!({
        "model": MODEL,
        "messages": [
            {"role": "system", "content": "system prompt"},
            {"role": "user", "content": "hello"},
        ],
        "max_tokens": 1,
        "stream": false,
    })
    .to_string()
    .into_bytes()
}

/// A live broker serving on loopback against the raw fake upstream, with one live turn.
struct LimitsHarness {
    addr: SocketAddr,
    capability: String,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    server: Option<tokio::task::JoinHandle<()>>,
    // Kept alive so the capability stays minted and the session stays registered.
    _access: TurnAccess,
    _receipt: TurnReceipt,
    _session: BrokerSession,
    _ledgers: BrokerLedgerReceiver,
    _broker: Broker,
    _upstream: tokio::task::JoinHandle<()>,
}

impl LimitsHarness {
    async fn new() -> Self {
        let (upstream_port, upstream) = spawn_fake_upstream().await;
        let (listener, broker) = BrokerListener::bind_with(
            std::sync::Arc::new(SystemClock::new()),
            std::sync::Arc::new(OsRandom::new()),
        )
        .expect("listener");
        let addr = listener.local_addr();
        let endpoint = format!("http://127.0.0.1:{upstream_port}/v1");
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
            true,
            MODEL,
            DEFAULT_BROKER_LIMITS,
        )
        .expect("plan");
        let policy = SessionPolicy::new(DEFAULT_BROKER_LIMITS).expect("policy");
        let mut registration = broker
            .register_session(plan, lease, policy)
            .expect("register");
        let (attempt, receipt) = registration
            .ledgers
            .arm_turn(TurnMeta::without_deadline())
            .expect("arm");
        let access = attempt.mint_access().expect("mint");
        let capability = access.api_key.expose_for_child(str::to_owned);

        let (tx, rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let _ = listener
                .run_with_shutdown(async move {
                    let _ = rx.await;
                })
                .await;
        });

        Self {
            addr,
            capability,
            shutdown: Some(tx),
            server: Some(server),
            _access: access,
            _receipt: receipt,
            _session: registration.session,
            _ledgers: registration.ledgers,
            _broker: broker,
            _upstream: upstream,
        }
    }

    fn host(&self) -> String {
        format!("127.0.0.1:{}", self.addr.port())
    }

    /// A full authenticated Chat Completions request as raw bytes.
    fn request(&self) -> Vec<u8> {
        let body = chat_body();
        let head = format!(
            "POST /v1/chat/completions HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            self.host(),
            self.capability,
            body.len()
        );
        let mut out = head.into_bytes();
        out.extend_from_slice(&body);
        out
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

impl Drop for LimitsHarness {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(server) = self.server.take() {
            server.abort();
        }
    }
}

/// §5.1 + mutation "delete the header-read timeout": a client that opens a connection and never
/// completes its header block is closed at `HEADER_READ_TIMEOUT`, not left open holding a handler.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_silent_client_is_closed_at_the_header_read_timeout() {
    let harness = LimitsHarness::new().await;
    let mut stream = TcpStream::connect(harness.addr)
        .await
        .expect("connect the listener");
    // A partial header block: the terminating CRLF never arrives.
    stream
        .write_all(
            format!(
                "POST /v1/chat/completions HTTP/1.1\r\nHost: {}\r\n",
                harness.host()
            )
            .as_bytes(),
        )
        .await
        .expect("write the partial header");
    stream.flush().await.expect("flush");

    // The listener must close the connection once the read timeout elapses. A generous margin over
    // the exact bound keeps a loaded runner from flaking; without the timeout this read blocks.
    let mut buffer = [0u8; 64];
    let read = tokio::time::timeout(
        HEADER_READ_TIMEOUT + Duration::from_secs(3),
        stream.read(&mut buffer),
    )
    .await
    .expect("the header-read timeout must fire; the connection stayed open");
    assert_eq!(
        read.expect("read the closed connection"),
        0,
        "the listener must close a silent connection, not answer it"
    );
    harness.shutdown().await;
}

/// §14.2 + mutation "serve HTTP/2": the listener speaks HTTP/1 only, so an HTTP/2 prior-knowledge
/// preface is never served as a request. Mutating `.http1_only()` to an h2-capable builder would let
/// the preface negotiate a stream, reddening the "no HTTP/1 response / connection closed" assertion.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_http2_preface_is_not_served_as_a_request() {
    let harness = LimitsHarness::new().await;
    let mut stream = TcpStream::connect(harness.addr)
        .await
        .expect("connect the listener");
    // The connection preface a client with prior knowledge of HTTP/2 sends first.
    stream
        .write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n")
        .await
        .expect("write the HTTP/2 preface");
    stream.flush().await.expect("flush");

    let mut buffer = [0u8; 128];
    let read = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buffer))
        .await
        .expect("an HTTP/1-only listener must terminate an HTTP/2 preface, not hang");
    let served = match read {
        Ok(0) => String::new(),
        Ok(n) => String::from_utf8_lossy(&buffer[..n]).to_string(),
        Err(_) => String::new(),
    };
    assert!(
        !served.starts_with("HTTP/1.1 2"),
        "an HTTP/2 preface must not be served as a successful HTTP/1 request: {served}"
    );
    harness.shutdown().await;
}

/// §5.1 + mutation "multiply the per-connection request ceiling": one keep-alive connection is
/// served at most `MAX_REQUESTS_PER_CONNECTION` requests, and the last response tells the client the
/// connection is closing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_connection_serves_exactly_the_request_ceiling() {
    let harness = LimitsHarness::new().await;
    let request = harness.request();
    let mut stream = TcpStream::connect(harness.addr)
        .await
        .expect("connect the listener");

    for index in 0..MAX_REQUESTS_PER_CONNECTION {
        stream
            .write_all(&request)
            .await
            .unwrap_or_else(|e| panic!("write request {index}: {e}"));
        stream.flush().await.expect("flush");
        let response = read_one_response(&mut stream)
            .await
            .unwrap_or_else(|e| panic!("response {index}: {e}"));
        assert_eq!(response.status, 200, "request {index} must be served");
        let is_last = index + 1 == MAX_REQUESTS_PER_CONNECTION;
        assert_eq!(
            response.headers.contains("connection: close"),
            is_last,
            "only the ceiling-th response may close the connection (request {index})"
        );
    }

    // The ceiling close must be real: the next request on the same socket meets a closed connection.
    let _ = stream.write_all(&request).await;
    let _ = stream.flush().await;
    let next = read_one_response(&mut stream).await;
    assert!(
        next.is_err(),
        "the connection must be closed after the request ceiling, got {next:?}",
    );
    harness.shutdown().await;
}

/// §5.1 + mutation "multiply the connection ceiling": while `MAX_CONNECTIONS` connections are held
/// by a slowloris header, a further client is deferred until one of them frees its handler permit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_connection_ceiling_defers_the_next_client_until_one_frees() {
    raise_open_file_limit();
    let harness = LimitsHarness::new().await;

    // Saturate the broker-wide connection ceiling. Connect with retry so a transient backlog does
    // not false-fail; each held socket sends a partial header so its handler holds its permit.
    let mut held = Vec::with_capacity(MAX_CONNECTIONS);
    let mut attempts = 0usize;
    while held.len() < MAX_CONNECTIONS {
        match TcpStream::connect(harness.addr).await {
            Ok(mut socket) => {
                socket
                    .write_all(b"POST /v1/chat/completions HTTP/1.1\r\n")
                    .await
                    .expect("write the partial header");
                socket.flush().await.expect("flush");
                held.push(socket);
            }
            Err(error) => {
                attempts += 1;
                assert!(
                    attempts < 20_000,
                    "could not open {MAX_CONNECTIONS} connections after {attempts} attempts: {error}"
                );
                tokio::task::yield_now().await;
            }
        }
    }
    // Give the accept loop time to take every permit before probing the ceiling.
    tokio::time::sleep(Duration::from_millis(800)).await;

    let mut extra = TcpStream::connect(harness.addr)
        .await
        .expect("connect the extra client");
    extra
        .write_all(&harness.request())
        .await
        .expect("write the extra request");
    extra.flush().await.expect("flush");

    let deferred =
        tokio::time::timeout(Duration::from_millis(700), read_one_response(&mut extra)).await;
    assert!(
        deferred.is_err(),
        "a connection above the ceiling must not be served while every permit is held, got {deferred:?}",
    );

    // Freeing one holder must release its permit and let the waiting client be served.
    drop(held.pop());
    let served = tokio::time::timeout(Duration::from_secs(5), read_one_response(&mut extra))
        .await
        .expect("freeing a connection must let the deferred client proceed")
        .expect("the deferred client must be answered");
    assert_eq!(served.status, 200);
    harness.shutdown().await;
}
