//! windowserver — serves the top-level Tauri window from the embedded `web/` dashboard bundle and
//! reverse-proxies the app's same-origin `/api/*` + `/healthz` fetches to the supervised `rhapsodyd`
//! sidecar. This is the Tauri wiring of the ported [`crate::apiproxy`] `handle` core — the analogue of
//! Go's Wails `AssetServer` + `apiProxyMiddleware` (`$REF/desktop/main.go` + `apiproxy.go`):
//!
//!   - `next` (non-API paths) is the embedded-asset handler [`serve_asset`], backed by Tauri's
//!     [`AssetResolver`](tauri::AssetResolver) over the `frontendDist` bundle (`web/` → the same
//!     rust-embed dist the daemon serves), which already does SPA fallback + index-at-root + MIME.
//!   - `base_url` is [`crate::app::App::daemon_base_url`], resolved per request over the live supervisor.
//!
//! [`build_response`] is the framework-agnostic core (unit-tested against fakes); [`register`] adapts
//! a wry custom-protocol request into it. The responder is fully buffered, so it cannot forward an
//! infinite `text/event-stream`; the Logs view's live tail is streamed over a Tauri IPC channel instead
//! (see [`crate::logbridge`]).

use bytes::Bytes;
use http::{HeaderMap, HeaderValue, StatusCode, header};
use tauri::{AppHandle, Builder, Manager, Runtime, UriSchemeContext, UriSchemeResponder};

use crate::apiproxy::{self, ProxyRequest, ProxyResponse};
use crate::app::App;

/// The custom URI scheme the top-level window loads (`rhapsody://localhost/`, per `tauri.conf.json`'s
/// window `url`). A registered scheme is a *local* app origin: Tauri injects the IPC bootstrap so
/// `invoke(...)` works, exactly as it does for the built-in `tauri://` scheme.
pub const SCHEME: &str = "rhapsody";

/// Registers the window-serving custom-protocol handler on the Tauri builder. Each request is handled
/// on the async runtime: non-API paths serve embedded assets; `/api/*` + `/healthz` reverse-proxy to
/// the live daemon. The `App` state (managed in `setup`) supplies the per-request daemon target; before
/// it exists, API calls resolve to `503` (daemon not running) and assets still serve.
pub fn register<R: Runtime>(builder: Builder<R>, client: reqwest::Client) -> Builder<R> {
    builder.register_asynchronous_uri_scheme_protocol(
        SCHEME,
        move |ctx: UriSchemeContext<'_, R>, request, responder: UriSchemeResponder| {
            let app_handle = ctx.app_handle().clone();
            let client = client.clone();
            tauri::async_runtime::spawn(async move {
                // `try_state` (not `state`) so a request that races app teardown / a not-yet-managed
                // App never panics — errors are values (no panic on the request path).
                let app = app_handle.try_state::<App>().map(|s| s.inner().clone());
                let response = build_response(
                    &client,
                    request,
                    |req| serve_asset(&app_handle, req),
                    || app.as_ref().and_then(App::daemon_base_url),
                )
                .await;
                responder.respond(response);
            });
        },
    )
}

/// Handle one window request end to end and return a fully-buffered HTTP response for the wry
/// responder. Generic over `next` (the embedded-asset handler) and `base_url` (the live daemon target)
/// so the wiring is unit-testable against fakes, exactly like [`apiproxy::handle`] itself.
pub async fn build_response<N, B>(
    client: &reqwest::Client,
    request: http::Request<Vec<u8>>,
    next: N,
    base_url: B,
) -> http::Response<Vec<u8>>
where
    N: FnOnce(ProxyRequest) -> ProxyResponse,
    B: Fn() -> Option<String>,
{
    let req = to_proxy_request(request);
    let resp = apiproxy::handle(req, client, next, base_url).await;
    to_http_response(resp)
}

/// The embedded-asset handler ([`apiproxy::handle`]'s `next`): serves a static file from the bundled
/// `web/` dist, with Tauri's built-in SPA fallback (unknown non-file path → `index.html`) and MIME
/// detection. An empty / unbuilt dist (no embedded `index.html`) yields `500 dashboard not built`,
/// placeholder parity with the daemon's `web.rs`.
pub fn serve_asset<R: Runtime>(app: &AppHandle<R>, req: ProxyRequest) -> ProxyResponse {
    match app.asset_resolver().get(req.path) {
        Some(asset) => {
            let mut headers = HeaderMap::new();
            if let Ok(ct) = HeaderValue::from_str(&asset.mime_type) {
                headers.insert(header::CONTENT_TYPE, ct);
            }
            ProxyResponse {
                status: StatusCode::OK,
                headers,
                body: Bytes::from(asset.bytes),
            }
        }
        None => text_response(StatusCode::INTERNAL_SERVER_ERROR, "dashboard not built"),
    }
}

/// A plain-text [`ProxyResponse`] with the given status.
fn text_response(status: StatusCode, body: &'static str) -> ProxyResponse {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    ProxyResponse {
        status,
        headers,
        body: Bytes::from_static(body.as_bytes()),
    }
}

/// Adapt a wry custom-protocol request into the framework-agnostic [`ProxyRequest`]: method, the path
/// (without query) matched by the proxy, the raw query, headers, and the buffered body.
fn to_proxy_request(request: http::Request<Vec<u8>>) -> ProxyRequest {
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let query = request.uri().query().map(str::to_string);
    let headers = request.headers().clone();
    let body = Bytes::from(request.into_body());
    ProxyRequest {
        method,
        path,
        query,
        headers,
        body,
    }
}

/// Convert a [`ProxyResponse`] into the `http::Response<Vec<u8>>` the wry responder wants — status +
/// headers + fully-buffered body, no fallible builder (so no `unwrap`/`expect` on the request path).
fn to_http_response(resp: ProxyResponse) -> http::Response<Vec<u8>> {
    let mut out = http::Response::new(resp.body.to_vec());
    *out.status_mut() = resp.status;
    *out.headers_mut() = resp.headers;
    out
}

/// Opens `url` in the user's default browser (the `open_external` command). Validates the scheme is
/// `http`/`https` — the embedded webview must never be asked to open `file://` or a custom scheme — then
/// hands it to macOS `open`. Errors are values (no panic); the caller surfaces the message.
pub fn open_external(url: &str) -> Result<(), String> {
    validate_web_url(url)?;
    std::process::Command::new("open")
        .arg(url)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("failed to open browser: {e}"))
}

/// Reject anything that is not an `http`/`https` URL before handing it to `open`.
fn validate_web_url(url: &str) -> Result<(), String> {
    let parsed = url::Url::parse(url).map_err(|e| format!("invalid url: {e}"))?;
    match parsed.scheme() {
        "http" | "https" => Ok(()),
        other => Err(format!("refusing to open non-web url scheme: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::Method;

    fn req(method: Method, uri: &str) -> http::Request<Vec<u8>> {
        http::Request::builder()
            .method(method)
            .uri(uri)
            .body(Vec::new())
            .expect("build request")
    }

    #[test]
    fn to_proxy_request_splits_path_and_query() {
        let r = req(
            Method::GET,
            "http://rhapsody.localhost/api/v1/runs/7?after=abc",
        );
        let p = to_proxy_request(r);
        assert_eq!(p.method, Method::GET);
        assert_eq!(p.path, "/api/v1/runs/7");
        assert_eq!(p.query.as_deref(), Some("after=abc"));
    }

    #[test]
    fn to_proxy_request_preserves_body_and_headers() {
        let r = http::Request::builder()
            .method(Method::POST)
            .uri("http://rhapsody.localhost/api/v1/runs")
            .header("content-type", "application/json")
            .body(b"{\"k\":1}".to_vec())
            .expect("build request");
        let p = to_proxy_request(r);
        assert_eq!(&p.body[..], b"{\"k\":1}");
        assert_eq!(
            p.headers.get("content-type").and_then(|v| v.to_str().ok()),
            Some("application/json")
        );
    }

    #[test]
    fn to_http_response_carries_status_headers_body() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        let resp = ProxyResponse {
            status: StatusCode::CREATED,
            headers,
            body: Bytes::from_static(b"{}"),
        };
        let out = to_http_response(resp);
        assert_eq!(out.status(), StatusCode::CREATED);
        assert_eq!(
            out.headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("application/json")
        );
        assert_eq!(out.body(), b"{}");
    }

    // A non-API path falls through to `next` (the embedded-asset handler) and never resolves a daemon
    // target — mirrors apiproxy's asset-fallthrough contract, exercised through the window wiring.
    #[tokio::test]
    async fn asset_path_falls_through_to_next() {
        let client = reqwest::Client::new();
        let out = build_response(
            &client,
            req(Method::GET, "http://rhapsody.localhost/assets/app.js"),
            |r| {
                assert_eq!(r.path, "/assets/app.js");
                text_response(StatusCode::OK, "asset-body")
            },
            || panic!("asset request must not resolve a daemon target"),
        )
        .await;
        assert_eq!(out.status(), StatusCode::OK);
        assert_eq!(out.body(), b"asset-body");
    }

    // An /api path proxies via base_url (unusable target here → 503), never touching the asset handler.
    #[tokio::test]
    async fn api_path_uses_base_url_not_assets() {
        let client = reqwest::Client::new();
        let out = build_response(
            &client,
            req(Method::GET, "http://rhapsody.localhost/api/v1/state"),
            |_| panic!("an /api request must not fall through to the asset handler"),
            || None, // daemon down
        )
        .await;
        assert_eq!(out.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn validate_web_url_accepts_http_and_https() {
        assert!(validate_web_url("https://linear.app/issue/TRA-251").is_ok());
        assert!(validate_web_url("http://127.0.0.1:8799/").is_ok());
    }

    #[test]
    fn validate_web_url_rejects_non_web_schemes() {
        assert!(validate_web_url("file:///etc/passwd").is_err());
        assert!(validate_web_url("javascript:alert(1)").is_err());
        assert!(validate_web_url("not a url").is_err());
    }
}
