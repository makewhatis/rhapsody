//! STUDIO-990 (P9), B2: the REAL `/models` egress path, driven against fake loopback providers.
//!
//! Every other catalog test in this crate scripts a fake `ModelDiscovery`; the credentialed adapter
//! itself (`OpenAiCompatibleDiscovery` → the broker's fixed `fetch_models`) had no test that would
//! notice it following a redirect, honouring a proxy, contacting a mismatched endpoint, or skipping
//! the status check. These drive it end to end against a raw loopback HTTP server, including the
//! redirect-to-a-second-listener mutation: the second listener must never see a request.
//!
//! Rhapsody-only; no Go counterpart.

#![cfg(feature = "discovery")]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use rhapsody_credential_ipc::domain::{
    Binding, BoundCredentialLease, OPENAI_CHAT_COMPLETIONS_BEARER_V1,
};
use rhapsody_provider_status::{
    CatalogError, DiscoveryRequest, ModelDiscovery, OpenAiCompatibleDiscovery,
};

/// The fake credential the adapter is handed. It is also the canary a reflected body must not leak.
const SECRET: &str = "sk-live-CANARY-0001";

/// A minimal HTTP/1 server on an ephemeral loopback port. `respond` produces the raw response bytes
/// per request (it may close over another server's address for the redirect case). Every accepted
/// connection increments the returned counter, so a test can prove a listener was never contacted.
/// `keep_open` holds the connection after writing, so a short body against a larger `Content-Length`
/// stalls the read until the client's own bounded timeout fires.
fn spawn_server(
    keep_open: bool,
    respond: impl Fn() -> Vec<u8> + Send + 'static,
) -> (SocketAddr, Arc<AtomicUsize>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback server");
    let addr = listener.local_addr().expect("server addr");
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_thread = Arc::clone(&hits);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            hits_thread.fetch_add(1, Ordering::SeqCst);
            let _ = read_request_head(&mut stream);
            let bytes = respond();
            let _ = stream.write_all(&bytes);
            let _ = stream.flush();
            if keep_open {
                // Hold the connection without completing the body: the client must time out on its
                // own bounded read rather than reading an EOF as a (wrong) short success.
                std::thread::sleep(Duration::from_secs(120));
            }
        }
    });
    (addr, hits)
}

fn read_request_head(stream: &mut TcpStream) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte) {
            Ok(0) => break,
            Ok(_) => {
                buf.push(byte[0]);
                if buf.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    buf
}

fn endpoint(addr: SocketAddr) -> String {
    format!("http://127.0.0.1:{}/v1", addr.port())
}

fn lease(endpoint: &str) -> BoundCredentialLease {
    BoundCredentialLease::new(
        Binding {
            provider_id: "fireworks".to_string(),
            adapter: OPENAI_CHAT_COMPLETIONS_BEARER_V1.to_string(),
            base_url: endpoint.to_string(),
        },
        SECRET.to_string(),
    )
}

fn request(endpoint: &str) -> DiscoveryRequest {
    DiscoveryRequest {
        endpoint: endpoint.to_string(),
        allow_insecure_http: true,
        lease: lease(endpoint),
    }
}

fn json_response(status: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

async fn discover(request: DiscoveryRequest) -> Result<Vec<String>, CatalogError> {
    let discovered = OpenAiCompatibleDiscovery.list_models(request).await?;
    Ok(discovered.entries.into_iter().map(|m| m.id).collect())
}

// Happy path: the real adapter maps a well-formed OpenAI list into model entries.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_real_adapter_returns_the_provider_model_list() {
    let body = r#"{"object":"list","data":[{"id":"gpt-4o"},{"id":"m2","display_name":"M Two"}]}"#;
    let (addr, hits) = spawn_server(false, move || json_response("200 OK", body));
    let ids = discover(request(&endpoint(addr))).await.expect("discovery");
    assert_eq!(ids, ["gpt-4o", "m2"]);
    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "exactly one provider request"
    );
}

// MUTATION GUARD (non-2xx must fail): a 500 must surface as the status, never as an empty success.
// Deleting the `!response.status().is_success()` check in `catalog::fetch_models` turns this green.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_non_success_status_is_a_failure() {
    let (addr, _hits) = spawn_server(false, || json_response("500 Internal Server Error", "boom"));
    assert_eq!(
        discover(request(&endpoint(addr))).await,
        Err(CatalogError::Status(500))
    );
}

// MUTATION GUARD (follow a redirect): the adapter must not follow a 302. The second listener is a
// real server whose hit counter must stay at zero; swapping `self.client` for a redirect-following
// `reqwest::Client::new()` in `UpstreamClient::fetch_models` fails this.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_redirect_is_not_followed_and_the_target_is_never_contacted() {
    let (target, target_hits) = spawn_server(false, || json_response("200 OK", r#"{"data":[]}"#));
    let location = format!("{}/models", endpoint(target));
    let (redirector, _hits) = spawn_server(false, move || {
        format!(
            "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
        .into_bytes()
    });

    assert_eq!(
        discover(request(&endpoint(redirector))).await,
        Err(CatalogError::Status(302)),
        "a redirect is reported as the status it is, never followed"
    );
    assert_eq!(
        target_hits.load(Ordering::SeqCst),
        0,
        "the redirect target must never see a request"
    );
}

// MUTATION GUARD (exact credential reflection is removed before the fields cross the adapter):
// an entry whose id reflects the exact credential is dropped, the drop is VISIBLE as `truncated`,
// and the secret never reaches the caller. Disabling the broker's reflection filter keeps `sk-live-…`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reflected_canary_is_dropped_before_it_crosses_the_adapter() {
    let body = format!(
        r#"{{"data":[{{"id":"ok"}},{{"id":"{SECRET}"}},{{"id":"b","display_name":"{SECRET}"}}]}}"#
    );
    let (addr, _hits) = spawn_server(false, move || json_response("200 OK", &body));
    let discovered = OpenAiCompatibleDiscovery
        .list_models(request(&endpoint(addr)))
        .await
        .expect("discovery");
    let ids: Vec<&str> = discovered.entries.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["ok"], "only the clean entry survives");
    assert!(discovered.truncated, "the drop must be visible, not silent");
}

// The endpoint policy is enforced before any contact: a plaintext endpoint is refused unless the
// request carries the explicit opt-in.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plaintext_is_refused_without_the_explicit_optin() {
    let (addr, hits) = spawn_server(false, || json_response("200 OK", r#"{"data":[]}"#));
    let endpoint = endpoint(addr);
    let mut req = request(&endpoint);
    req.allow_insecure_http = false;
    assert_eq!(discover(req).await, Err(CatalogError::Endpoint));
    assert_eq!(
        hits.load(Ordering::SeqCst),
        0,
        "no contact on a refused endpoint"
    );
}

// The bounded deadline is real: a provider that sends response headers and then stalls the body must
// produce `Timeout`, not hang. The broker's `read_timeout` is 30s, so this test is deliberately slow;
// it is the one deterministic way to prove the timeout mapping without a test-only timeout override.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stalled_body_is_a_timeout() {
    let (addr, _hits) = spawn_server(true, || {
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 1000\r\nConnection: close\r\n\r\npartial"
            .to_vec()
    });
    assert_eq!(
        discover(request(&endpoint(addr))).await,
        Err(CatalogError::Timeout)
    );
}
