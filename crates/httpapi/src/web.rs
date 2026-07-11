//! web — the embedded React dashboard + SPA fallback. Parity port of
//! `$REF/internal/httpapi/web.go` (`WebHandler`/`spaHandler`/`serveIndex`) and the build-output
//! contract of `$REF/internal/httpapi/web_dist_placeholder.go`.
//!
//! Go embeds `web/dist` via `//go:embed all:web/dist`; the Rust port embeds
//! `crates/httpapi/web-dist/` via `rust-embed`. The built vite bundle (index.html + hashed
//! `assets/`) is BUILD OUTPUT and is NOT committed (see the repo `.gitignore` + this crate's
//! `web-dist/.gitkeep`): the anchor is enough for `#[derive(RustEmbed)]` to compile on a clean /
//! Node-less checkout, and until the bundle is built the server serves "dashboard not built"
//! (placeholder parity with `web_dist_placeholder.go`). CI's `web` job runs `npm run build` (vite
//! `outDir` → `web-dist/`) so a real bundle exists there; `make`/F1 build it before a release embed.

use axum::body::Body;
use axum::http::{HeaderValue, StatusCode, Uri, header};
use axum::response::Response;
use rust_embed::RustEmbed;

/// The embedded production dashboard: the vite build output under `crates/httpapi/web-dist/`. Empty
/// (just the `.gitkeep` anchor) on a clean checkout — see the module docs. Mirrors Go's `distFS`.
#[derive(RustEmbed)]
#[folder = "web-dist/"]
pub(crate) struct WebDist;

/// The SPA catch-all handler, serving the embedded asset set `E` with SPA fallback. Registered as the
/// router fallback (mounted LAST), so it never shadows an `/api` route — and it 404s any `/api` path
/// defensively regardless, so a misconfigured mount cannot leak the dashboard to API clients. Generic
/// over `E` so the production embed (`WebDist`) and the tests' committed dist share one code path.
/// Mirrors Go `WebHandler` + `spaHandler`.
pub(crate) async fn serve_web<E: RustEmbed>(uri: Uri) -> Response {
    // Never handle API paths here (defensive; the API routes match before this fallback runs).
    let req_path = uri.path();
    if req_path == "/api" || req_path.starts_with("/api/") {
        return not_found();
    }
    // Clean the path and test for an existing embedded file. `clean_name` strips the leading slash
    // and collapses `.`/`..` segments (Go's `path.Clean("/"+p)` then trim `/`), so a request can
    // never traverse outside the embedded tree.
    let name = clean_name(req_path);
    // Serve the SPA entrypoint directly for "/" and an explicit "/index.html": routing the latter
    // through a file server would 301-redirect the canonical index name to "./"; serving the body
    // here returns 200 so a direct GET /index.html works without a hop (Go's `serveIndex` note).
    if name.is_empty() || name == "index.html" {
        return serve_index::<E>();
    }
    if let Some(file) = E::get(&name) {
        let mut resp = Response::new(Body::from(file.data.into_owned()));
        let mime = HeaderValue::from_str(file.metadata.mimetype())
            .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream"));
        resp.headers_mut().insert(header::CONTENT_TYPE, mime);
        return resp;
    }
    // Non-file, non-/api path: serve the SPA entrypoint (client-side route / deep link).
    serve_index::<E>()
}

/// Write `index.html` with a 200 + `text/html` (and `Cache-Control: no-cache`) so client-side routing
/// / deep-links work; when the bundle is not built (no embedded `index.html`) serve "dashboard not
/// built" (500). Mirrors Go `serveIndex` + the `web_dist_placeholder.go` contract.
fn serve_index<E: RustEmbed>() -> Response {
    match E::get("index.html") {
        Some(file) => {
            let mut resp = Response::new(Body::from(file.data.into_owned()));
            let headers = resp.headers_mut();
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/html; charset=utf-8"),
            );
            headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
            resp
        }
        None => {
            let mut resp = Response::new(Body::from("dashboard not built"));
            *resp.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
            resp.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/plain; charset=utf-8"),
            );
            resp
        }
    }
}

/// A 404 for an unknown `/api` path (never the SPA body). Mirrors Go's `http.NotFound` on the API
/// prefix guard.
fn not_found() -> Response {
    let mut resp = Response::new(Body::from("404 page not found"));
    *resp.status_mut() = StatusCode::NOT_FOUND;
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    resp
}

/// Strip the leading slash and normalize `.`/`..` segments — the Rust analog of Go's
/// `strings.TrimPrefix(path.Clean("/"+p), "/")`. The result is a forward-slash key with no traversal,
/// matching how `rust-embed` keys its files.
fn clean_name(req_path: &str) -> String {
    let mut segments: Vec<&str> = Vec::new();
    for seg in req_path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            other => segments.push(other),
        }
    }
    segments.join("/")
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rust_embed::RustEmbed;

    use super::serve_web;
    use crate::server::build_router;
    use crate::testutil::{FakeProvider, empty_snapshot, spawn_router};

    /// A committed test dist (`testdata/webdist/`) standing in for the vite bundle: `index.html` (with
    /// the `#root` anchor `web_test.go` checks) + a static `assets/app.js`. Lets the SPA tests assert
    /// the served dashboard deterministically without a Node build.
    #[derive(RustEmbed)]
    #[folder = "testdata/webdist/"]
    struct TestDist;

    /// An intentionally empty dist (`testdata/emptydist/`, just a `.gitkeep`) — exercises the
    /// placeholder path (no embedded `index.html` ⇒ "dashboard not built"), deterministically
    /// regardless of whether the real `web-dist/` happens to be built locally.
    #[derive(RustEmbed)]
    #[folder = "testdata/emptydist/"]
    struct EmptyDist;

    async fn spawn_with_dist<E: RustEmbed + 'static>() -> String {
        let router = build_router(Arc::new(FakeProvider::ok(empty_snapshot())), serve_web::<E>);
        spawn_router(router).await
    }

    // Mirrors Go `TestSPAServesIndexAtRoot`: GET / serves the embedded index.html with 200 + HTML.
    #[tokio::test]
    async fn spa_serves_index_at_root() {
        let base = spawn_with_dist::<TestDist>().await;
        let resp = reqwest::get(format!("{base}/")).await.expect("GET /");
        assert_eq!(resp.status(), 200);
        let ct = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert!(ct.starts_with("text/html"), "content-type = {ct:?}");
        let body = resp.text().await.expect("body");
        assert!(
            body.contains("<div id=\"root\">"),
            "index missing #root: {body}"
        );
    }

    // Mirrors Go `TestSPAServesIndexHTMLExplicitly`: GET /index.html returns the page body with 200
    // (NOT the 301 redirect to "./" a file server would emit for the canonical index name).
    #[tokio::test]
    async fn spa_serves_index_html_explicitly() {
        let base = spawn_with_dist::<TestDist>().await;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("client");
        let resp = client
            .get(format!("{base}/index.html"))
            .send()
            .await
            .expect("GET /index.html");
        assert_eq!(resp.status(), 200, "want 200 (no redirect for /index.html)");
        let body = resp.text().await.expect("body");
        assert!(body.contains("<div id=\"root\">"), "no page body: {body}");
    }

    // Mirrors Go `TestSPAFallbackDeepLink`: an unknown non-/api path falls back to index.html (200).
    #[tokio::test]
    async fn spa_fallback_deep_link() {
        let base = spawn_with_dist::<TestDist>().await;
        let resp = reqwest::get(format!("{base}/some/client/route"))
            .await
            .expect("GET deep link");
        assert_eq!(resp.status(), 200, "want 200 (index fallback)");
        let body = resp.text().await.expect("body");
        assert!(
            body.contains("<div id=\"root\">"),
            "no index fallback: {body}"
        );
    }

    // Mirrors Go `TestSPANeverShadowsAPI`: an unknown /api path returns 404 (NOT index.html).
    #[tokio::test]
    async fn spa_never_shadows_api() {
        let base = spawn_with_dist::<TestDist>().await;
        let resp = reqwest::get(format!("{base}/api/unknown"))
            .await
            .expect("GET /api/unknown");
        assert_eq!(resp.status(), 404);
        let body = resp.text().await.expect("body");
        assert!(
            !body.contains("<div id=\"root\">"),
            "/api leaked index.html: {body}"
        );
    }

    // The "serve an existing embedded file directly" branch (not the index fallback): a real static
    // asset is served with its own (non-HTML) MIME type.
    #[tokio::test]
    async fn spa_serves_static_asset() {
        let base = spawn_with_dist::<TestDist>().await;
        let resp = reqwest::get(format!("{base}/assets/app.js"))
            .await
            .expect("GET asset");
        assert_eq!(resp.status(), 200);
        let ct = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert!(
            ct.contains("javascript"),
            "asset content-type = {ct:?}, want a javascript type"
        );
        let body = resp.text().await.expect("body");
        assert!(body.contains("rhapsody-test-asset"), "asset body: {body}");
    }

    // Placeholder parity (web_dist_placeholder.go): an empty dist compiles and serves "dashboard not
    // built" with 500 until the bundle is built.
    #[tokio::test]
    async fn empty_dist_serves_not_built() {
        let base = spawn_with_dist::<EmptyDist>().await;
        let resp = reqwest::get(format!("{base}/")).await.expect("GET /");
        assert_eq!(resp.status(), 500);
        let body = resp.text().await.expect("body");
        assert!(body.contains("dashboard not built"), "body = {body}");
    }
}
