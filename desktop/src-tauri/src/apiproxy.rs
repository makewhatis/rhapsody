//! The same-origin reverse proxy that forwards the app's `/api/*` + `/healthz` requests to the
//! supervised `symphonyd` sidecar. Parity port of `$REF/desktop/apiproxy.go`.
//!
//! The packaged app serves its UI from its own origin, so the UI's relative fetches — GET/POST
//! `/api/*` and `/healthz` — must be forwarded to symphonyd's loopback server. That server binds a
//! dynamically-chosen free port which is reassigned across start/stop/restart, so the target is
//! resolved from the supervisor PER REQUEST (via the `base_url` closure) rather than captured once.
//! Non-API paths fall through to the static asset handler (`next`).
//!
//! [`handle`] is the ported, unit-testable core of Go's `apiProxyHandler`; the D3 window-serving
//! task wires it (with `next` = the embedded-asset handler and `base_url` = [`usable_base_url`] over
//! the live supervisor), exactly as Go's Wails `AssetServer.Middleware` does.

use bytes::Bytes;
use http::{HeaderMap, Method, StatusCode, header};

use crate::supervisor::State;

/// A request as seen by the proxy. Framework-agnostic (the D3 wiring adapts the webview request into
/// this) so [`handle`] is testable against an httptest-style backend without a real webview.
pub struct ProxyRequest {
    pub method: Method,
    /// The request path, without the query (e.g. `/api/v1/config`). Matched by [`is_daemon_api_path`].
    pub path: String,
    /// The raw query string, if any (e.g. `after=abc`).
    pub query: Option<String>,
    pub headers: HeaderMap,
    pub body: Bytes,
}

/// A response produced by the proxy — either forwarded from the daemon, delegated to `next`, or a
/// synthesized 503/502.
pub struct ProxyResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
}

impl ProxyResponse {
    fn text(status: StatusCode, body: &'static str) -> Self {
        ProxyResponse {
            status,
            headers: HeaderMap::new(),
            body: Bytes::from_static(body.as_bytes()),
        }
    }
}

/// Reports whether a request path must be proxied to symphonyd instead of being served from the
/// embedded UI bundle. Mirrors Go `isDaemonAPIPath`.
pub fn is_daemon_api_path(path: &str) -> bool {
    path == "/healthz" || path.starts_with("/api/")
}

/// Resolves the live daemon target for the API proxy from the supervisor's state + URL, returning the
/// base URL only when it is usable. Mirrors the usability core of Go `App.daemonBaseURL`:
///
///   - Only a RUNNING daemon is proxied. After stop (or a crash) the supervisor retains the last
///     bound port, so a port check alone would forward to a now-dead port (connection refused → 502);
///     gating on `Running` yields the intended 503 "daemon not running", and also avoids proxying
///     during the not-yet-ready `Starting` window.
///   - The URL must parse with a non-empty host and a real (non-zero) port.
pub fn usable_base_url(state: State, url: &str) -> Option<String> {
    if state != State::Running {
        return None;
    }
    let parsed = url::Url::parse(url).ok()?;
    if parsed.host_str().is_none_or(str::is_empty) {
        return None;
    }
    match parsed.port() {
        Some(0) | None => None,
        Some(_) => Some(url.to_string()),
    }
}

/// The proxying handler. Mirrors Go `apiProxyHandler(next, baseURL)`:
///
///   - Non-API paths fall through to `next` (the static asset handler) — `base_url` is NOT consulted.
///   - For `/api/*` + `/healthz`, the live daemon target is resolved ONCE here via `base_url`; an
///     unusable (stopped / not-yet-running) or unparseable target yields 503 rather than proxying to
///     a stale host. Resolving exactly once (vs. Go's `ReverseProxy.Director` re-resolving) is
///     guaranteed structurally: there is a single `base_url()` call site.
///   - Otherwise the request is forwarded to `<target><path>[?query]` and the response returned
///     verbatim; a forwarding failure yields 502 "daemon unavailable" (Go's `ErrorHandler`).
pub async fn handle<N, B>(
    req: ProxyRequest,
    client: &reqwest::Client,
    next: N,
    base_url: B,
) -> ProxyResponse
where
    N: FnOnce(ProxyRequest) -> ProxyResponse,
    B: Fn() -> Option<String>,
{
    if !is_daemon_api_path(&req.path) {
        return next(req);
    }
    // Resolve the live daemon target ONCE, here. If it's unusable (stopped / not yet running) or
    // unparseable, return 503 rather than proxying to a stale or empty host.
    let raw = match base_url() {
        Some(u) => u,
        None => return ProxyResponse::text(StatusCode::SERVICE_UNAVAILABLE, "daemon not running"),
    };
    let target = match url::Url::parse(&raw) {
        Ok(u) if u.host_str().is_some_and(|h| !h.is_empty()) => u,
        _ => return ProxyResponse::text(StatusCode::SERVICE_UNAVAILABLE, "daemon not running"),
    };
    forward(req, client, &target).await
}

/// Forwards `req` to the daemon `target`, preserving method / path / query / headers / body, and
/// returns the daemon's response verbatim.
async fn forward(req: ProxyRequest, client: &reqwest::Client, target: &url::Url) -> ProxyResponse {
    // `target` carries only the origin (scheme://host:port); the request path + query come from the
    // incoming request. `reqwest`'s http types ARE the `http` crate's, so no conversion is needed.
    let mut dst = target.clone();
    dst.set_path(&req.path);
    dst.set_query(req.query.as_deref());

    let mut builder = client.request(req.method.clone(), dst);
    for (name, value) in &req.headers {
        // Drop Host (the client sets it from the target; the app-origin Host would misroute) and the
        // hop-by-hop / framing headers — reqwest re-derives Content-Length from the buffered body.
        if name == header::HOST || is_hop_by_hop(name) {
            continue;
        }
        builder = builder.header(name.clone(), value.clone());
    }
    if !req.body.is_empty() {
        builder = builder.body(req.body.clone());
    }

    match builder.send().await {
        Ok(resp) => {
            let status = resp.status();
            // Copy the daemon's headers EXCEPT hop-by-hop / framing ones — the body is fully buffered
            // below, so the serializer (D3) sets Content-Length itself. Mirrors the header set Go's
            // `httputil.ReverseProxy` strips. `append` preserves multi-valued headers (e.g. Set-Cookie).
            let mut headers = HeaderMap::new();
            for (name, value) in resp.headers() {
                if !is_hop_by_hop(name) {
                    headers.append(name.clone(), value.clone());
                }
            }
            let body = resp.bytes().await.unwrap_or_default();
            ProxyResponse {
                status,
                headers,
                body,
            }
        }
        // Go's ReverseProxy.ErrorHandler -> 502 "daemon unavailable".
        Err(_) => ProxyResponse::text(StatusCode::BAD_GATEWAY, "daemon unavailable"),
    }
}

/// Reports whether `name` is a hop-by-hop / framing header that must not be forwarded across the
/// proxy. The request/response body is re-buffered, so `Content-Length` / `Transfer-Encoding` are
/// re-derived by the client and the serializer. Mirrors the set Go's `httputil.ReverseProxy` strips.
fn is_hop_by_hop(name: &http::HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-connection"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "content-length"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::convert::Infallible;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use http_body_util::Full;
    use hyper::body::Incoming;
    use hyper::service::service_fn;
    use hyper::{Request, Response};
    use hyper_util::rt::TokioIo;
    use tokio::net::TcpListener;

    fn get(path: &str) -> ProxyRequest {
        ProxyRequest {
            method: Method::GET,
            path: path.to_string(),
            query: None,
            headers: HeaderMap::new(),
            body: Bytes::new(),
        }
    }

    // Mirrors TestIsDaemonAPIPath.
    #[test]
    fn is_daemon_api_path_matches_only_api_and_healthz() {
        let cases = [
            ("/healthz", true),
            ("/api/v1/config", true),
            ("/api/v1/state", true),
            ("/api/v1/runs/abc", true),
            ("/", false),
            ("/index.html", false),
            ("/assets/app-abc123.js", false),
            ("/apidocs", false), // not under /api/
            ("/health", false),
        ];
        for (path, want) in cases {
            assert_eq!(is_daemon_api_path(path), want, "path {path}");
        }
    }

    /// A minimal backend HTTP server that records the last path it saw and returns `{"ok":true}` —
    /// the Rust equivalent of Go's `httptest.NewServer`.
    struct Backend {
        url: String,
        last_path: Arc<Mutex<Option<String>>>,
        _handle: tokio::task::JoinHandle<()>,
    }

    async fn start_backend() -> Backend {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind backend");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        let last_path = Arc::new(Mutex::new(None));
        let recorder = last_path.clone();
        let handle = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    continue;
                };
                let io = TokioIo::new(stream);
                let recorder = recorder.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |req: Request<Incoming>| {
                        let recorder = recorder.clone();
                        async move {
                            *recorder.lock().expect("lock") = Some(req.uri().path().to_string());
                            Ok::<_, Infallible>(Response::new(Full::new(Bytes::from_static(
                                b"{\"ok\":true}",
                            ))))
                        }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(io, service)
                        .await;
                });
            }
        });
        Backend {
            url,
            last_path,
            _handle: handle,
        }
    }

    // Mirrors TestAPIProxyForwardsToDaemon: /api/* and /healthz are reverse-proxied to the live
    // daemon target (preserving the path) and the response is returned verbatim.
    #[tokio::test]
    async fn api_proxy_forwards_to_daemon() {
        let backend = start_backend().await;
        let client = reqwest::Client::new();
        for path in ["/api/v1/config", "/healthz"] {
            let resp = handle(
                get(path),
                &client,
                |_| panic!("API request must not fall through to the asset handler"),
                || Some(backend.url.clone()),
            )
            .await;
            assert_eq!(resp.status, StatusCode::OK, "{path}: status");
            assert_eq!(&resp.body[..], b"{\"ok\":true}", "{path}: body");
            assert_eq!(
                backend.last_path.lock().expect("lock").as_deref(),
                Some(path),
                "{path}: backend saw path"
            );
        }
    }

    // Mirrors TestAPIProxyFallsThroughForAssets: non-API paths reach the asset handler and never
    // resolve a daemon target.
    #[tokio::test]
    async fn api_proxy_falls_through_for_assets() {
        let client = reqwest::Client::new();
        let resolved = AtomicUsize::new(0);
        let resp = handle(
            get("/index.html"),
            &client,
            |_| ProxyResponse::text(StatusCode::IM_A_TEAPOT, "asset"),
            || {
                resolved.fetch_add(1, Ordering::SeqCst);
                None
            },
        )
        .await;
        assert_eq!(
            resp.status,
            StatusCode::IM_A_TEAPOT,
            "asset path fell through"
        );
        assert_eq!(
            resolved.load(Ordering::SeqCst),
            0,
            "asset request must not resolve a daemon target"
        );
    }

    // Mirrors TestAPIProxyResolvesTargetOnce: the daemon target is resolved EXACTLY ONCE per request.
    #[tokio::test]
    async fn api_proxy_resolves_target_once() {
        let backend = start_backend().await;
        let client = reqwest::Client::new();
        let calls = AtomicUsize::new(0);
        let base_url = || {
            // Good target on the first call, unusable on any second call — a re-resolve would break.
            if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                Some(backend.url.clone())
            } else {
                None
            }
        };
        let resp = handle(
            get("/api/v1/state"),
            &client,
            |_| panic!("API request must not fall through to the asset handler"),
            base_url,
        )
        .await;
        assert_eq!(resp.status, StatusCode::OK);
        assert_eq!(&resp.body[..], b"{\"ok\":true}");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "base_url called more than once (Director must not re-resolve)"
        );
    }

    // Mirrors TestAPIProxyUnavailableWhenDaemonDown: API calls get a clean 503 (not an asset
    // fallthrough) while the daemon is stopped / its port is unbound.
    #[tokio::test]
    async fn api_proxy_unavailable_when_daemon_down() {
        let client = reqwest::Client::new();
        let resp = handle(
            get("/api/v1/state"),
            &client,
            |_| panic!("API path must not fall through when the daemon is down"),
            || None,
        )
        .await;
        assert_eq!(resp.status, StatusCode::SERVICE_UNAVAILABLE);
    }

    // usable_base_url gates on Running + a real port (the daemonBaseURL usability core).
    #[test]
    fn usable_base_url_gates_on_running_and_port() {
        assert_eq!(
            usable_base_url(State::Running, "http://127.0.0.1:53211"),
            Some("http://127.0.0.1:53211".to_string())
        );
        assert_eq!(
            usable_base_url(State::Stopped, "http://127.0.0.1:53211"),
            None
        );
        assert_eq!(
            usable_base_url(State::Starting, "http://127.0.0.1:53211"),
            None
        );
        assert_eq!(usable_base_url(State::Running, "http://127.0.0.1:0"), None);
        assert_eq!(usable_base_url(State::Running, "not a url"), None);
    }
}
