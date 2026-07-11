//! Test-only Linear GraphQL mock server + helpers — the Rust analogue of the linear package's
//! in-test `net/http/httptest` servers (`client_test.go`'s `newTestClient` / `answerViewer`).
//!
//! A [`MockServer`] binds an ephemeral loopback port and answers each GraphQL POST from a
//! caller-supplied handler; [`MockServer::start_with_viewer`] additionally auto-answers the
//! always-on `viewer` resolution query (mirroring Go's `answerViewer`) so per-test handlers can
//! focus on the issues/milestones/projects queries. Responses carry `Connection: close`, so the
//! sequential fetch/paginate calls each open a fresh connection the accept loop handles in turn.

use super::{Client, Config, new};
use serde::Deserialize;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// The canned `viewer` resolution payload used by the assignee-filter tests (`testViewerResp`).
pub(super) const TEST_VIEWER_RESP: &str = r#"{"data":{"viewer":{"id":"viewer-1","displayName":"Test Owner","email":"owner@example.com"}}}"#;

/// The decoded GraphQL request body, so handlers can assert on the query text + variables
/// (`gqlReq` in `candidates_test.go`).
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(super) struct GqlReq {
    pub query: String,
    pub variables: serde_json::Value,
}

impl GqlReq {
    /// The variable named `key`, or `None` when absent (`req.Variables[key]`).
    pub(super) fn var(&self, key: &str) -> Option<&serde_json::Value> {
        self.variables.get(key)
    }

    /// The variable named `key` as a string, or `None` when absent / not a string.
    pub(super) fn var_str(&self, key: &str) -> Option<&str> {
        self.variables.get(key).and_then(|v| v.as_str())
    }
}

/// A canned HTTP reply from a handler: a status code + JSON body.
pub(super) struct MockResp {
    status: u16,
    body: String,
}

impl MockResp {
    /// A 200 OK reply carrying `body`.
    pub(super) fn ok(body: impl Into<String>) -> Self {
        Self {
            status: 200,
            body: body.into(),
        }
    }

    /// A reply with an explicit status code (e.g. a 500 mid-pagination failure).
    pub(super) fn status(code: u16, body: impl Into<String>) -> Self {
        Self {
            status: code,
            body: body.into(),
        }
    }
}

type Handler = Arc<dyn Fn(&GqlReq) -> MockResp + Send + Sync>;

/// A loopback GraphQL server that lives until dropped (which aborts its accept loop).
pub(super) struct MockServer {
    url: String,
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

impl MockServer {
    /// The server's base URL (`http://127.0.0.1:<port>`).
    pub(super) fn url(&self) -> String {
        self.url.clone()
    }

    /// Starts a server that routes every request through `handler`.
    pub(super) async fn start<F>(handler: F) -> Self
    where
        F: Fn(&GqlReq) -> MockResp + Send + Sync + 'static,
    {
        Self::start_inner(Arc::new(handler), false).await
    }

    /// Starts a server that auto-answers the `viewer` query with [`TEST_VIEWER_RESP`] and routes
    /// every other request through `handler` (the Rust mirror of `answerViewer`).
    pub(super) async fn start_with_viewer<F>(handler: F) -> Self
    where
        F: Fn(&GqlReq) -> MockResp + Send + Sync + 'static,
    {
        Self::start_inner(Arc::new(handler), true).await
    }

    async fn start_inner(handler: Handler, answer_viewer: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        let url = format!("http://{addr}");
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let req = read_gql_request(&mut stream).await;
                let resp = if answer_viewer && req.query.contains("viewer {") {
                    MockResp::ok(TEST_VIEWER_RESP)
                } else {
                    handler(&req)
                };
                write_response(&mut stream, &resp).await;
            }
        });
        MockServer { url, handle }
    }
}

/// Builds a [`Client`] pointed at a viewer-auto-answering server, the Rust mirror of Go's
/// `newTestClient` (`ProjectSlug: "proj"`, `APIKey: "test-key"`). Returns the server too so the
/// caller keeps it alive for the test's duration.
pub(super) async fn new_test_client<F>(handler: F) -> (Client, MockServer)
where
    F: Fn(&GqlReq) -> MockResp + Send + Sync + 'static,
{
    let server = MockServer::start_with_viewer(handler).await;
    let c = new(Config {
        endpoint: server.url(),
        api_key: "test-key".into(),
        project_slug: "proj".into(),
        ..Config::default()
    });
    (c, server)
}

/// Reads one HTTP/1.1 request off `stream` (header block until CRLFCRLF, then `Content-Length`
/// body bytes) and decodes the JSON body into a [`GqlReq`].
async fn read_gql_request(stream: &mut TcpStream) -> GqlReq {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    let header_end = loop {
        let n = stream.read(&mut tmp).await.expect("read request");
        if n == 0 {
            break buf.len();
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let content_length = header_value(&head, "content-length")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);
    while buf.len() - header_end < content_length {
        let n = stream.read(&mut tmp).await.expect("read body");
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    let end = (header_end + content_length).min(buf.len());
    serde_json::from_slice(&buf[header_end..end]).unwrap_or_default()
}

/// Case-insensitive lookup of a single HTTP header value from the raw header block.
fn header_value(head: &str, name: &str) -> Option<String> {
    head.lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case(name))
        .map(|(_, v)| v.trim().to_string())
}

/// Writes an HTTP/1.1 response with the reply's status + body and `Connection: close`.
async fn write_response(stream: &mut TcpStream, resp: &MockResp) {
    let payload = format!(
        "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        resp.status,
        reason_phrase(resp.status),
        resp.body.len(),
        resp.body
    );
    let _ = stream.write_all(payload.as_bytes()).await;
    let _ = stream.flush().await;
}

/// A minimal HTTP reason phrase for the status codes the tests use (reqwest keys off the numeric
/// code, so the phrase is cosmetic).
fn reason_phrase(code: u16) -> &'static str {
    match code {
        200 => "OK",
        500 => "Internal Server Error",
        _ => "Status",
    }
}
