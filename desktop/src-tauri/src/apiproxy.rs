//! The same-origin reverse proxy that forwards the app's `/api/*` + `/healthz` requests to the
//! supervised `rhapsodyd` sidecar. Parity port of `$REF/desktop/apiproxy.go`.
//!
//! The packaged app serves its UI from its own origin, so the UI's relative fetches — GET/POST
//! `/api/*` and `/healthz` — must be forwarded to rhapsodyd's loopback server. That server binds a
//! dynamically-chosen free port which is reassigned across start/stop/restart, so the target is
//! resolved from the supervisor PER REQUEST (via the `base_url` closure) rather than captured once.
//! Non-API paths fall through to the static asset handler (`next`).
//!
//! [`handle`] is the ported, unit-testable core of Go's `apiProxyHandler`; the D3 window-serving
//! task wires it (with `next` = the embedded-asset handler and `base_url` = [`usable_base_url`] over
//! the live supervisor), exactly as Go's Wails `AssetServer.Middleware` does.
//!
//! The daemon's operator-write guard (STUDIO-982, Rhapsody-only) refuses a mutation unless its
//! `Host` is the daemon's own `127.0.0.1:<port>`, it carries exactly one `X-Rhapsody-Operator: 1`,
//! and it has no foreign `Origin` and no `Cookie`. The window's requests arrive from the bundled
//! origin ([`BUNDLED_ORIGIN`]), which the daemon would refuse. So the proxy never forwards what the
//! webview sent for those headers. It drops any `Host`, `Origin`, `Cookie`, `Sec-Fetch-*` or
//! operator header, sets `Host` to the daemon target itself, and injects exactly one operator
//! header when the request came through the app's own custom-protocol handler ([`may_vouch_for`]).
//! Any other request is forwarded without the header, so the daemon refuses its writes.
//!
//! STUDIO-1044: the original rule vouched only for a literal `Origin: rhapsody://localhost`, and
//! that vouched for NOTHING in the real app — observed on macOS, WebKit sends no `Origin` for a
//! same-origin `fetch` from the `rhapsody://localhost/` document, so every console write was
//! refused as `operator_header`. [`may_vouch_for`] carries the observed evidence and its rationale.

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

/// The origin the app's own window has: the `rhapsody` custom scheme it is served from
/// (`crate::windowserver::SCHEME`, window `url` `rhapsody://localhost/` in `tauri.conf.json`).
pub const BUNDLED_ORIGIN: &str = "rhapsody://localhost";

/// The daemon's operator-write header (httpapi `operator_guard`) and its one accepted value.
pub const OPERATOR_HEADER: &str = "x-rhapsody-operator";
pub const OPERATOR_HEADER_VALUE: &str = "1";

/// Whether the proxy should vouch for a request that arrived through the app's own custom-protocol
/// handler by injecting the operator header.
///
/// The scheme handler is registered on the app's own `WKWebView` (wry's `setURLSchemeHandler`), and
/// no page in any other origin can address it — so the handler itself is the origin evidence. The
/// only thing that could still be judged is what the webview claims about its own origin:
///
///   - **No `Origin`** is vouched for. This is the real app's shape: observed on macOS (STUDIO-1044),
///     WebKit sends NO `Origin` for a same-origin `fetch` from the `rhapsody://localhost/` document
///     (it sends `Referer: rhapsody://localhost/` instead), so the old exact-`Origin` rule vouched
///     for nothing and every console write was refused.
///   - **Exactly one `Origin: `[`BUNDLED_ORIGIN`]** is vouched for, should WebKit ever send it.
///   - **A foreign or repeated `Origin`** is not: a page somewhere else in this webview, or a forged
///     duplicate, is refused by the daemon's guard.
///   - **A `Cookie`** is not: the app's own writes are cookie-free (`credentials: "omit"`), and a
///     cookie is exactly what the guard refuses (cookies are scoped by host, not by port).
pub fn may_vouch_for(headers: &HeaderMap) -> bool {
    if headers.contains_key(header::COOKIE) {
        return false;
    }
    let mut origins = headers.get_all(header::ORIGIN).iter();
    match (origins.next(), origins.next()) {
        (None, _) => true,
        (Some(origin), None) => origin.as_bytes() == BUNDLED_ORIGIN.as_bytes(),
        (Some(_), Some(_)) => false,
    }
}

/// Reports whether a request path must be proxied to rhapsodyd instead of being served from the
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
        // Drop the hop-by-hop / framing headers (reqwest re-derives Content-Length from the buffered
        // body) and every header the daemon's operator-write guard judges: the proxy sets those
        // itself below, so nothing the webview sent for them reaches the daemon.
        if is_guard_header(name) || is_hop_by_hop(name) {
            continue;
        }
        builder = builder.header(name.clone(), value.clone());
    }
    // The daemon's own `host:port`, not the app origin's Host (which would also misroute).
    if let Some(host) = target.host_str() {
        let authority = match target.port() {
            Some(port) => format!("{host}:{port}"),
            None => host.to_string(),
        };
        builder = builder.header(header::HOST, authority);
    }
    if may_vouch_for(&req.headers) {
        builder = builder.header(OPERATOR_HEADER, OPERATOR_HEADER_VALUE);
    } else {
        // STUDIO-1044: say WHY the proxy declined to vouch. `Origin` is the only evidence a caller
        // could be judged on, so record it (or that none was present) — the marker that showed the
        // real webview sends none. It is logged, never trusted, and never forwarded.
        let origins: Vec<String> = req
            .headers
            .get_all(header::ORIGIN)
            .iter()
            .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
            .collect();
        let origin = if origins.is_empty() {
            "absent".to_string()
        } else {
            origins.join(", ")
        };
        eprintln!(
            "rhapsody-desktop: apiproxy: declined to vouch for {} {} (Origin: {origin})",
            req.method, req.path
        );
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

/// Reports whether `name` is one of the headers the daemon's operator-write guard judges, which the
/// proxy never forwards from the webview: `Host`, `Origin`, `Cookie`, the operator header, and the
/// `Sec-Fetch-*` metadata. That metadata describes the webview's own fetch, not the proxy's request
/// to the daemon, which the proxy vouches for itself.
fn is_guard_header(name: &http::HeaderName) -> bool {
    name == header::HOST
        || name == header::ORIGIN
        || name == header::COOKIE
        || name.as_str() == OPERATOR_HEADER
        || name.as_str().starts_with("sec-fetch-")
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

    use http_body_util::{BodyExt, Full};
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
        /// The headers and body of the last request, as the daemon would see them.
        last_request: Arc<Mutex<Option<(HeaderMap, Bytes)>>>,
        _handle: tokio::task::JoinHandle<()>,
    }

    async fn start_backend() -> Backend {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind backend");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        let last_path = Arc::new(Mutex::new(None));
        let recorder = last_path.clone();
        let last_request = Arc::new(Mutex::new(None));
        let request_recorder = last_request.clone();
        let handle = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    continue;
                };
                let io = TokioIo::new(stream);
                let recorder = recorder.clone();
                let request_recorder = request_recorder.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |req: Request<Incoming>| {
                        let recorder = recorder.clone();
                        let request_recorder = request_recorder.clone();
                        async move {
                            *recorder.lock().expect("lock") = Some(req.uri().path().to_string());
                            let headers = req.headers().clone();
                            let body = req
                                .into_body()
                                .collect()
                                .await
                                .map(|b| b.to_bytes())
                                .unwrap_or_default();
                            *request_recorder.lock().expect("lock") = Some((headers, body));
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
            last_request,
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

    /// The exact headers the real macOS webview was observed to send for a same-origin console
    /// write (STUDIO-1044): NO `Origin` at all, a `Referer` of the bundled document, and the
    /// console's own single operator header. The old exact-`Origin` rule vouched for nothing here.
    fn observed_webview_post() -> ProxyRequest {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::ACCEPT,
            http::HeaderValue::from_static("application/json"),
        );
        headers.insert(
            header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/json"),
        );
        headers.insert(
            header::REFERER,
            http::HeaderValue::from_static("rhapsody://localhost/"),
        );
        headers.insert(
            header::USER_AGENT,
            http::HeaderValue::from_static("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7)"),
        );
        headers.insert(OPERATOR_HEADER, http::HeaderValue::from_static("1"));
        ProxyRequest {
            method: Method::POST,
            path: "/api/v1/runs/2508/stop".to_string(),
            query: None,
            headers,
            body: Bytes::from_static(b"{}"),
        }
    }

    /// A window POST carrying every header the daemon's operator-write guard judges, forged or
    /// duplicated, plus a body and a content type that must survive. `origin` is what the webview
    /// claimed. No cookie: the tests that exercise the cookie rule add their own.
    fn hostile_post(origin: Option<&'static str>) -> ProxyRequest {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::HOST,
            http::HeaderValue::from_static("evil.example:1"),
        );
        if let Some(origin) = origin {
            headers.insert(header::ORIGIN, http::HeaderValue::from_static(origin));
        }
        headers.insert(
            "sec-fetch-site",
            http::HeaderValue::from_static("cross-site"),
        );
        headers.append(OPERATOR_HEADER, http::HeaderValue::from_static("1"));
        headers.append(OPERATOR_HEADER, http::HeaderValue::from_static("1"));
        headers.insert(
            header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/json"),
        );
        ProxyRequest {
            method: Method::POST,
            path: "/api/v1/refresh".to_string(),
            query: None,
            headers,
            body: Bytes::from_static(b"{}"),
        }
    }

    async fn forwarded(backend: &Backend, req: ProxyRequest) -> (HeaderMap, Bytes) {
        let resp = handle(
            req,
            &reqwest::Client::new(),
            |_| panic!("API request must not fall through to the asset handler"),
            || Some(backend.url.clone()),
        )
        .await;
        assert_eq!(resp.status, StatusCode::OK);
        backend
            .last_request
            .lock()
            .expect("lock")
            .clone()
            .expect("backend saw a request")
    }

    fn values(headers: &HeaderMap, name: &str) -> Vec<String> {
        headers
            .get_all(name)
            .iter()
            .map(|v| v.to_str().unwrap_or_default().to_string())
            .collect()
    }

    // STUDIO-1044: the request the REAL macOS webview sends (no `Origin`; see observed_webview_post)
    // reaches the daemon with its own Host and exactly one operator header, so the daemon accepts
    // the write. Restoring the old exact-`Origin` rule — which vouched only for a literal
    // `Origin: rhapsody://localhost` — turns this red.
    #[tokio::test]
    async fn the_observed_webview_write_is_vouched_for_with_exactly_one_operator_header() {
        let backend = start_backend().await;
        let authority = backend.url.trim_start_matches("http://").to_string();
        let (headers, body) = forwarded(&backend, observed_webview_post()).await;
        assert_eq!(values(&headers, "host"), std::slice::from_ref(&authority));
        assert_eq!(values(&headers, OPERATOR_HEADER), ["1"]);
        assert!(values(&headers, "origin").is_empty());
        assert!(values(&headers, "cookie").is_empty());
        assert!(values(&headers, "sec-fetch-site").is_empty());
        assert_eq!(values(&headers, "content-type"), ["application/json"]);
        assert_eq!(&body[..], b"{}");
    }

    // STUDIO-982: a window write from the bundled origin reaches the daemon with the daemon's own
    // Host, exactly one operator header, and no Origin. Every forged or duplicated copy the webview
    // sent is dropped. The body and content type pass through. (A cookie would stop the proxy
    // vouching at all — see `a_cookie_bearing_write_is_never_vouched_for`.)
    #[tokio::test]
    async fn a_bundled_origin_write_gets_exactly_one_operator_header_and_the_daemon_host() {
        let backend = start_backend().await;
        let authority = backend.url.trim_start_matches("http://").to_string();
        let (headers, body) = forwarded(&backend, hostile_post(Some(BUNDLED_ORIGIN))).await;
        assert_eq!(values(&headers, "host"), std::slice::from_ref(&authority));
        assert_eq!(values(&headers, OPERATOR_HEADER), ["1"]);
        assert!(values(&headers, "origin").is_empty());
        assert!(values(&headers, "sec-fetch-site").is_empty());
        assert_eq!(values(&headers, "content-type"), ["application/json"]);
        assert_eq!(&body[..], b"{}");
    }

    // A foreign origin — `null`, another scheme, a look-alike host — is never vouched for, even a
    // forged one of its own, so the daemon's guard refuses its write. The dropped copies are gone.
    #[tokio::test]
    async fn a_foreign_origin_is_never_vouched_for() {
        let backend = start_backend().await;
        for origin in [
            "null",
            "https://evil.example",
            "http://127.0.0.1:8799",
            "rhapsody://localhost.evil",
        ] {
            let (headers, _) = forwarded(&backend, hostile_post(Some(origin))).await;
            assert!(values(&headers, OPERATOR_HEADER).is_empty(), "{origin}");
            assert!(values(&headers, "origin").is_empty(), "{origin}");
        }
    }

    // A repeated `Origin` is not one `Origin`: never vouched for.
    #[tokio::test]
    async fn a_repeated_origin_is_never_vouched_for() {
        let backend = start_backend().await;
        let mut req = hostile_post(Some(BUNDLED_ORIGIN));
        req.headers.append(
            header::ORIGIN,
            http::HeaderValue::from_static(BUNDLED_ORIGIN),
        );
        let (headers, _) = forwarded(&backend, req).await;
        assert!(
            values(&headers, OPERATOR_HEADER).is_empty(),
            "a repeated Origin is not the bundled origin"
        );
    }

    // A request carrying a `Cookie` is never vouched for, even with no `Origin`: the app's own
    // writes are cookie-free (`credentials: "omit"`), and a cookie is exactly what the guard
    // refuses. The cookie is dropped in any case.
    #[tokio::test]
    async fn a_cookie_bearing_write_is_never_vouched_for() {
        let backend = start_backend().await;
        let mut req = observed_webview_post();
        req.headers
            .insert(header::COOKIE, http::HeaderValue::from_static("session=x"));
        let (headers, _) = forwarded(&backend, req).await;
        assert!(values(&headers, OPERATOR_HEADER).is_empty());
        assert!(values(&headers, "cookie").is_empty());
    }
}
