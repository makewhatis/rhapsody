//! Linear GraphQL client + transport — parity port of `internal/tracker/linear/client.go`
//! (upstream §11.2).
//!
//! [`Client`] carries the configured endpoint/key and a `reqwest` client; [`Client::do_graphql`]
//! executes one GraphQL operation and decodes `response.data`, mapping every failure to the
//! matching [`LinearErrorKind`] sentinel (client.go's `doGraphQL`). The read/write paths (P3
//! T4/T5) and viewer resolution build on it.

use super::{LinearError, LinearErrorKind, query};
use crate::TrackerError;
use regex::Regex;
use rhapsody_core::Viewer;
use serde::de::DeserializeOwned;
use std::future::Future;
use std::time::Duration;
use tracing::{Instrument, Span};

/// The default HTTP timeout Go applies in `New` (upstream §11.2: 30s).
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// The default page size Go applies in `New` (upstream §11.2: 50), sent as the `first` variable of
/// every paginated query (client.go's `defaultPageSize`).
const DEFAULT_PAGE_SIZE: i64 = 50;

/// The default summon token when none is configured (client.go's `defaultSummonToken`): the
/// case-insensitive mention a comment must contain to count as a summons.
const DEFAULT_SUMMON_TOKEN: &str = "@symphony";

/// Construction inputs for the linear adapter — the linear subset of the factory
/// [`Spec`](crate::Spec) (mirrors Go's `linear.Config`).
#[derive(Debug, Clone, Default)]
pub struct Config {
    pub endpoint: String,
    pub api_key: String,
    pub project_slug: String,
    pub active_states: Vec<String>,
    pub review_states: Vec<String>,
    pub summon_token: String,
    pub milestone: String,
    /// The resolved ticket-claim policy ("assignee" | "pool"). In "pool" the candidate query
    /// filters UNASSIGNED issues instead of assignee == viewer; every other query is unchanged.
    /// Empty is treated as "assignee". INF-477.
    pub claim_mode: String,
}

/// The Linear GraphQL client. T3 fills in the construction + transport over [`Config`]; the read
/// (T4) and write (T5) paths add the per-operation methods.
pub struct Client {
    pub(in crate::linear) config: Config,
    /// The shared `reqwest` client (30s timeout), the mirror of Go's `*http.Client`. `None` only
    /// if the TLS backend failed to initialise at construction — surfaced as an
    /// [`ApiRequest`](LinearErrorKind::ApiRequest) error at request time rather than a panic, so
    /// `new` stays infallible like Go's `New`.
    http: Option<reqwest::Client>,
    /// The compiled summon matcher (client.go's `summonRe`), resolved once from the configured
    /// token (or `@symphony`). `None` only if the constant pattern failed to compile — impossible
    /// in practice, and treated as "no summon detected" (never a panic), mirroring `normalize.go`'s
    /// `c.summonRe == nil` guard. Read by `normalize` (sibling module) when computing summons.
    pub(in crate::linear) summon_re: Option<Regex>,
    /// The page size for paginated queries (client.go's `pageSize`, default 50), sent as the
    /// `first` variable. Nothing in Rhapsody's `Spec` overrides it, so it is always the default.
    pub(in crate::linear) page_size: i64,
    /// The owner of the API key (client.go's `viewer` guarded by `viewerMu`), resolved lazily and
    /// cached for the client's lifetime. A `tokio::sync::Mutex` (not `std`) so the guard can be held
    /// across the `do_graphql` await — the async mirror of Go serializing resolution under
    /// `viewerMu`, and object-safe with the `Send` async-trait future.
    viewer: tokio::sync::Mutex<Option<Viewer>>,
    /// The resolved/cached milestone UUID (candidates.go's `milestoneID` guarded by `milestoneMu`);
    /// empty means unresolved. Same async-mutex rationale as [`viewer`](Self::viewer). Read/written
    /// by `resolve_milestone_id` in the sibling `candidates` module.
    pub(in crate::linear) milestone_id: tokio::sync::Mutex<String>,
}

/// Builds a linear [`Client`] from its [`Config`], applying Go `New`'s defaults (30s timeout,
/// `@symphony` summon token).
pub fn new(config: Config) -> Client {
    let token = if config.summon_token.is_empty() {
        DEFAULT_SUMMON_TOKEN
    } else {
        config.summon_token.as_str()
    };
    let summon_re = rhapsody_core::compile_summon_re(token).ok();
    let http = reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
        .ok();
    Client {
        config,
        http,
        summon_re,
        page_size: DEFAULT_PAGE_SIZE,
        viewer: tokio::sync::Mutex::new(None),
        milestone_id: tokio::sync::Mutex::new(String::new()),
    }
}

/// The GraphQL envelope (client.go's `gqlResponse`). `data` is decoded into the caller's target.
#[derive(serde::Deserialize)]
struct GqlResponse {
    #[serde(default)]
    data: Option<serde_json::Value>,
    #[serde(default)]
    errors: Vec<serde_json::Value>,
}

impl Client {
    /// True when the resolved claim mode is "pool" (client.go's `poolMode`):
    /// [`fetch_candidate_issues`](super::candidates::fetch_candidate_issues) then fetches UNASSIGNED
    /// issues instead of assignee == viewer. Every other query stays assignee-keyed. INF-477.
    pub(in crate::linear) fn pool_mode(&self) -> bool {
        self.config.claim_mode == "pool"
    }

    /// Reports "not yet implemented" for the Tracker methods whose bodies land in P3 T5. Mirror
    /// of the T1 skeleton's placeholder; each method's real body replaces this call.
    pub(crate) fn not_implemented(&self) -> TrackerError {
        TrackerError::Other(format!(
            "linear adapter (endpoint {:?}) not yet implemented — ported by P3 Tasks T4–T5",
            self.config.endpoint
        ))
    }

    /// Executes one GraphQL operation and decodes `response.data` into `T` (client.go's
    /// `doGraphQL`). Sends the `Authorization` (raw API key) + `Content-Type: application/json`
    /// headers to the configured endpoint, and maps failures to the matching sentinel:
    /// transport/build/read → [`ApiRequest`](LinearErrorKind::ApiRequest); non-200 →
    /// [`ApiStatus`](LinearErrorKind::ApiStatus) (with a bounded body snippet); a top-level
    /// `errors` array → [`GraphqlErrors`](LinearErrorKind::GraphqlErrors); an undecodable body or
    /// empty `data` → [`UnknownPayload`](LinearErrorKind::UnknownPayload).
    ///
    /// Visibility: `pub` because it is the adapter's low-level transport, consumed by the read
    /// (T4) and write (T5) operation methods; `rhapsody-tracker` is an internal workspace crate,
    /// so this exposes no external API-stability surface.
    pub async fn do_graphql<T: DeserializeOwned>(
        &self,
        query: &str,
        variables: Option<serde_json::Value>,
    ) -> Result<T, TrackerError> {
        let http = self.http.as_ref().ok_or_else(|| {
            LinearError::new(LinearErrorKind::ApiRequest, "http client unavailable")
        })?;
        let payload = serde_json::json!({ "query": query, "variables": variables });
        let body = serde_json::to_vec(&payload)
            .map_err(|e| LinearError::new(LinearErrorKind::ApiRequest, format!("marshal: {e}")))?;

        let resp = http
            .post(&self.config.endpoint)
            .header("Authorization", &self.config.api_key)
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await
            .map_err(|e| LinearError::new(LinearErrorKind::ApiRequest, e.to_string()))?;

        let status = resp.status();
        let raw = resp.bytes().await.map_err(|e| {
            LinearError::new(LinearErrorKind::ApiRequest, format!("read body: {e}"))
        })?;

        if status != reqwest::StatusCode::OK {
            // Include a bounded snippet of the body: on a 400 Linear returns the actionable
            // GraphQL parse/validation error here, which the status code alone hides. Bounded so a
            // large/HTML error page can't flood logs.
            return Err(LinearError::new(
                LinearErrorKind::ApiStatus,
                format!("status {}: {}", status.as_u16(), body_snippet(&raw, 512)),
            )
            .into());
        }

        let env: GqlResponse = serde_json::from_slice(&raw)
            .map_err(|e| LinearError::new(LinearErrorKind::UnknownPayload, e.to_string()))?;
        if !env.errors.is_empty() {
            return Err(LinearError::new(
                LinearErrorKind::GraphqlErrors,
                format!("{:?}", env.errors),
            )
            .into());
        }
        let data = env
            .data
            .ok_or_else(|| LinearError::new(LinearErrorKind::UnknownPayload, "empty data"))?;
        serde_json::from_value(data).map_err(|e| {
            TrackerError::from(LinearError::new(
                LinearErrorKind::UnknownPayload,
                format!("decode data: {e}"),
            ))
        })
    }
}

/// The `viewer` query envelope (client.go's inline `resp` struct in `ResolveViewer`).
#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct ViewerResp {
    viewer: ViewerNode,
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct ViewerNode {
    id: String,
    name: String,
    #[serde(rename = "displayName")]
    display_name: String,
    email: String,
    organization: OrganizationNode,
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct OrganizationNode {
    #[serde(rename = "urlKey")]
    url_key: String,
}

/// ResolveViewer resolves and caches the owner of the configured API key (client.go's
/// `ResolveViewer`). Cached for the client's lifetime (viewer identity is stable), so it costs one
/// query per client; an empty id is [`ViewerUnresolved`](LinearErrorKind::ViewerUnresolved) — a bad
/// or non-user token must fail loudly rather than silently matching no tickets.
pub(super) async fn resolve_viewer(c: &Client) -> Result<Viewer, TrackerError> {
    // Hold the viewer-cache lock across resolution (single-flight), the async mirror of Go holding
    // `viewerMu` across the network call. On success the value is cached; a failure leaves the
    // cache empty so the next call re-resolves.
    let mut cached = c.viewer.lock().await;
    if let Some(v) = cached.as_ref() {
        return Ok(v.clone());
    }
    let resp: ViewerResp = c.do_graphql(query::QUERY_VIEWER, None).await?;
    if resp.viewer.id.is_empty() {
        return Err(LinearError::bare(LinearErrorKind::ViewerUnresolved).into());
    }
    let v = Viewer {
        id: resp.viewer.id,
        name: resp.viewer.name,
        display_name: resp.viewer.display_name,
        email: resp.viewer.email,
        url_key: resp.viewer.organization.url_key,
    };
    *cached = Some(v.clone());
    Ok(v)
}

/// Runs a tracker operation `fut` under `span` for the call's whole duration, recording the
/// operation's error onto the span before it ends — the async mirror of tracing.go's
/// `startTrackerSpan`/`endTrackerSpan` pairing (`tracker_span!` supplies the span).
pub(super) async fn traced<T, F>(span: Span, fut: F) -> Result<T, TrackerError>
where
    F: Future<Output = Result<T, TrackerError>>,
{
    let result = fut.instrument(span.clone()).await;
    if let Err(e) = &result {
        span.record("error", e.to_string().as_str());
    }
    result
}

/// A whitespace-trimmed, length-bounded view of `b` for an error message (client.go's
/// `bodySnippet`). Unlike Go's raw byte slice, truncation lands on a char boundary so a multi-byte
/// rune is never split (a Rust `&str[..n]` would panic mid-rune).
fn body_snippet(b: &[u8], max: usize) -> String {
    let lossy = String::from_utf8_lossy(b);
    let s = lossy.trim();
    if s.len() > max {
        let mut end = max;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…(truncated)", &s[..end])
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Tracker;
    use crate::linear::testutil::{MockResp, MockServer, TEST_VIEWER_RESP};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    /// The request headers a one-shot loopback server captured.
    struct Captured {
        authorization: Option<String>,
        content_type: Option<String>,
    }

    /// A one-shot loopback HTTP/1.1 server: binds an ephemeral port, accepts a single connection,
    /// captures the request headers, and replies with a canned status line + body. Stands in for
    /// Go's `net/http/httptest.NewServer`.
    async fn serve_once(
        status_line: &'static str,
        body: &'static str,
    ) -> (String, tokio::task::JoinHandle<Captured>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        let url = format!("http://{addr}");
        let handle = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let captured = read_request(&mut stream).await;
            let resp = format!(
                "{status_line}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream
                .write_all(resp.as_bytes())
                .await
                .expect("write response");
            stream.flush().await.expect("flush");
            captured
        });
        (url, handle)
    }

    /// Reads an HTTP/1.1 request off `stream`: the header block (until CRLFCRLF) then exactly
    /// `Content-Length` body bytes, returning the headers we assert on.
    async fn read_request(stream: &mut TcpStream) -> Captured {
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
        let mut body_read = buf.len() - header_end;
        while body_read < content_length {
            let n = stream.read(&mut tmp).await.expect("read body");
            if n == 0 {
                break;
            }
            body_read += n;
        }
        Captured {
            authorization: header_value(&head, "authorization"),
            content_type: header_value(&head, "content-type"),
        }
    }

    /// Case-insensitive lookup of a single HTTP header value from the raw header block.
    fn header_value(head: &str, name: &str) -> Option<String> {
        head.lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(k, _)| k.trim().eq_ignore_ascii_case(name))
            .map(|(_, v)| v.trim().to_string())
    }

    fn client_for(url: String) -> Client {
        new(Config {
            endpoint: url,
            api_key: "test-key".into(),
            project_slug: "proj".into(),
            ..Config::default()
        })
    }

    fn is_kind(err: &TrackerError, kind: LinearErrorKind) -> bool {
        matches!(err, TrackerError::Linear(e) if e.kind == kind)
    }

    // Mirrors Go TestDoGraphQLSendsAuthHeaderAndDecodes.
    #[tokio::test]
    async fn do_graphql_sends_auth_header_and_decodes() {
        let (url, handle) = serve_once("HTTP/1.1 200 OK", r#"{"data":{"ok":true}}"#).await;
        let c = client_for(url);

        #[derive(serde::Deserialize)]
        struct Out {
            ok: bool,
        }
        let out: Out = c
            .do_graphql("query{}", None)
            .await
            .expect("do_graphql should succeed");
        assert!(out.ok, "data not decoded");

        let cap = handle.await.expect("server task");
        assert_eq!(
            cap.authorization.as_deref(),
            Some("test-key"),
            "Authorization header"
        );
        assert_eq!(
            cap.content_type.as_deref(),
            Some("application/json"),
            "Content-Type header"
        );
    }

    // Mirrors Go TestDoGraphQLNon200.
    #[tokio::test]
    async fn do_graphql_non_200() {
        let (url, handle) = serve_once("HTTP/1.1 500 Internal Server Error", "").await;
        let c = client_for(url);
        let err = c
            .do_graphql::<serde_json::Value>("query{}", None)
            .await
            .expect_err("500 must be an error");
        assert!(
            is_kind(&err, LinearErrorKind::ApiStatus),
            "got {err:?}, want ApiStatus"
        );
        let _ = handle.await;
    }

    // Mirrors Go TestDoGraphQLGraphQLErrors.
    #[tokio::test]
    async fn do_graphql_graphql_errors() {
        let (url, handle) =
            serve_once("HTTP/1.1 200 OK", r#"{"errors":[{"message":"bad query"}]}"#).await;
        let c = client_for(url);
        let err = c
            .do_graphql::<serde_json::Value>("query{}", None)
            .await
            .expect_err("top-level errors must fail");
        assert!(
            is_kind(&err, LinearErrorKind::GraphqlErrors),
            "got {err:?}, want GraphqlErrors"
        );
        let _ = handle.await;
    }

    // Mirrors Go TestDoGraphQLMalformed.
    #[tokio::test]
    async fn do_graphql_malformed() {
        let (url, handle) = serve_once("HTTP/1.1 200 OK", "not json").await;
        let c = client_for(url);
        let err = c
            .do_graphql::<serde_json::Value>("query{}", None)
            .await
            .expect_err("undecodable body must fail");
        assert!(
            is_kind(&err, LinearErrorKind::UnknownPayload),
            "got {err:?}, want UnknownPayload"
        );
        let _ = handle.await;
    }

    // Mirrors Go TestDoGraphQLTransportError.
    #[tokio::test]
    async fn do_graphql_transport_error() {
        // Bind then drop to obtain a loopback port with nothing listening → connection refused
        // (Go points at 127.0.0.1:0, an unreachable endpoint).
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        drop(listener);
        let c = new(Config {
            endpoint: format!("http://{addr}"),
            api_key: "k".into(),
            project_slug: "p".into(),
            ..Config::default()
        });
        let err = c
            .do_graphql::<serde_json::Value>("query{}", None)
            .await
            .expect_err("connection refused must fail");
        assert!(
            is_kind(&err, LinearErrorKind::ApiRequest),
            "got {err:?}, want ApiRequest"
        );
    }

    // ─── viewer resolution (viewer_test.go / projects_test.go) ───────────────────────────────────

    // Mirrors Go TestResolveViewerCaches.
    #[tokio::test]
    async fn resolve_viewer_caches() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_h = Arc::clone(&calls);
        let saw_query = Arc::new(AtomicBool::new(true));
        let saw_query_h = Arc::clone(&saw_query);
        let server = MockServer::start(move |req| {
            if !req.query.contains("viewer {") {
                saw_query_h.store(false, Ordering::SeqCst);
            }
            calls_h.fetch_add(1, Ordering::SeqCst);
            MockResp::ok(TEST_VIEWER_RESP)
        })
        .await;
        let c = new(Config {
            endpoint: server.url(),
            api_key: "k".into(),
            project_slug: "proj".into(),
            ..Config::default()
        });
        let v = c.resolve_viewer().await.expect("resolve");
        assert_eq!(v.id, "viewer-1");
        assert_eq!(v.display_name, "Test Owner");
        assert_eq!(v.email, "owner@example.com");
        // Second call is cached: no extra HTTP request (viewer identity is stable).
        c.resolve_viewer().await.expect("cached");
        assert!(saw_query.load(Ordering::SeqCst), "expected a viewer query");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "expected viewer resolved once (cached)"
        );
    }

    // Mirrors Go TestResolveViewerEmptyIDErrors.
    #[tokio::test]
    async fn resolve_viewer_empty_id_errors() {
        // An empty viewer id (e.g. an over-scoped/non-user token) must fail loudly rather than
        // silently filtering candidates to no one.
        let server = MockServer::start(|_req| {
            MockResp::ok(r#"{"data":{"viewer":{"id":"","displayName":"","email":""}}}"#)
        })
        .await;
        let c = new(Config {
            endpoint: server.url(),
            api_key: "k".into(),
            project_slug: "proj".into(),
            ..Config::default()
        });
        let err = c.resolve_viewer().await.expect_err("empty id must error");
        assert!(
            is_kind(&err, LinearErrorKind::ViewerUnresolved),
            "got {err:?}, want ViewerUnresolved"
        );
    }

    // Mirrors Go TestResolveViewerParsesName.
    #[tokio::test]
    async fn resolve_viewer_parses_name() {
        let server = MockServer::start(|_req| {
            MockResp::ok(
                r#"{"data":{"viewer":{"id":"v1","name":"Jane Quentin","displayName":"jane","email":"jane@example.com"}}}"#,
            )
        })
        .await;
        let c = new(Config {
            endpoint: server.url(),
            api_key: "k".into(),
            ..Config::default()
        });
        let v = c.resolve_viewer().await.expect("resolve");
        assert_eq!(v.id, "v1");
        assert_eq!(v.name, "Jane Quentin");
        assert_eq!(v.display_name, "jane");
        assert_eq!(v.email, "jane@example.com");
    }
}
