//! operator_guard — the loopback operator-write guard every mutating route shares (STUDIO-982,
//! provider-auth design §P0d). Rhapsody-only: Go v0.4.0 guards no route, so this is a recorded
//! README divergence (a tightening of every existing local write endpoint).
//!
//! Binding to `127.0.0.1` keeps other machines out, but it does not keep out a web page open in the
//! operator's own browser. That page can send a "simple" cross-site `POST` (a form, or a `fetch`
//! with a safelisted content type) without a CORS preflight. After a DNS rebind it can also send a
//! same-origin request under a hostile name. So every mutating request has to prove it came from a
//! client that is not a web page on another origin:
//!
//! * `Host` is exactly `127.0.0.1:<port>`. The port comes from the accepted socket's own local
//!   address ([`BoundAddr`]), which is server state. It never comes from a client-supplied header:
//!   `X-Forwarded-Host` and friends are not read at all. A rebound hostname fails here.
//! * Exactly one `X-Rhapsody-Operator: 1`. A cross-site page cannot set a custom header without a
//!   preflight, and the preflight is refused.
//! * No `Cookie`, no CORS preflight (`OPTIONS`), and no form-shaped content type.
//! * An `Origin`, when present, is exactly `http://127.0.0.1:<port>`, so `null` and any other
//!   origin are refused. A `Sec-Fetch-Site`, when present, is `same-origin` or `none`.
//!
//! This is a browser-origin gate. It does not authenticate one local process against another
//! running as the same operator: such a process can set every header above.
//!
//! The guard is a per-route layer ([`operator_write`] in `server`). It sees the request before the
//! handler's body extractor reads anything and before any [`crate::StateProvider`] call. It acts
//! only on `POST`, the one mutating method, and on `OPTIONS`, a preflight for that `POST`. Every
//! other method reaches the handler unchanged: reads keep their wire contract, and a wrong method
//! still gets the handler's own 405. Every denial is the same bounded 403 envelope. The specific
//! reason is logged with the method and route path, but no header value is echoed back or logged.

use std::net::SocketAddr;

use axum::extract::Request;
use axum::extract::connect_info::{ConnectInfo, Connected};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::Response;
use axum::serve::IncomingStream;
use tokio::net::TcpListener;

use crate::responses::write_error;

/// The custom header every mutating request must carry exactly once, with exactly
/// [`OPERATOR_HEADER_VALUE`].
pub(crate) const OPERATOR_HEADER: &str = "x-rhapsody-operator";

/// The one accepted value of [`OPERATOR_HEADER`].
pub(crate) const OPERATOR_HEADER_VALUE: &str = "1";

/// The code of the single denial envelope.
pub(crate) const DENIED_CODE: &str = "operator_write_forbidden";

/// The message of the single denial envelope. It is fixed, so no client-supplied value is ever
/// reflected back.
pub(crate) const DENIED_MESSAGE: &str = "mutating requests need Host 127.0.0.1:<port>, exactly one X-Rhapsody-Operator: 1 header, a same-origin Origin, and no cookies";

/// The local address of the socket a request arrived on, captured once per accepted connection.
/// This is the "server state" the guard compares `Host` against. `None` means the address could
/// not be read, and the guard then denies every mutation.
///
/// Serve the router with `into_make_service_with_connect_info::<BoundAddr>()`. A router served
/// without it carries no [`BoundAddr`], so every guarded write fails closed.
#[derive(Clone, Copy, Debug)]
pub struct BoundAddr(Option<SocketAddr>);

impl Connected<IncomingStream<'_, TcpListener>> for BoundAddr {
    fn connect_info(stream: IncomingStream<'_, TcpListener>) -> Self {
        BoundAddr(stream.io().local_addr().ok())
    }
}

/// Why a mutation was refused. It is logged, never rendered: the client always gets
/// [`DENIED_MESSAGE`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Denial {
    Preflight,
    NoBoundPort,
    Host,
    OperatorHeader,
    Cookie,
    Origin,
    FetchSite,
    FormContentType,
}

impl Denial {
    fn as_str(self) -> &'static str {
        match self {
            Denial::Preflight => "cors_preflight",
            Denial::NoBoundPort => "no_bound_port",
            Denial::Host => "host",
            Denial::OperatorHeader => "operator_header",
            Denial::Cookie => "cookie",
            Denial::Origin => "origin",
            Denial::FetchSite => "sec_fetch_site",
            Denial::FormContentType => "form_content_type",
        }
    }
}

/// The guard as a middleware. `server::operator_write` layers it onto every mutating route.
pub(crate) async fn require_operator_write(req: Request, next: Next) -> Response {
    let method = req.method().clone();
    if method != Method::POST && method != Method::OPTIONS {
        return next.run(req).await;
    }
    let port = req
        .extensions()
        .get::<ConnectInfo<BoundAddr>>()
        .and_then(|ConnectInfo(BoundAddr(addr))| addr.map(|a| a.port()));
    match check(&method, req.headers(), port) {
        Ok(()) => next.run(req).await,
        Err(denial) => {
            tracing::warn!(
                reason = denial.as_str(),
                method = %method,
                path = req.uri().path(),
                "refused a loopback write that did not come from an operator client"
            );
            denied()
        }
    }
}

/// The single bounded denial envelope.
pub(crate) fn denied() -> Response {
    write_error(StatusCode::FORBIDDEN, DENIED_CODE, DENIED_MESSAGE, None)
}

/// The pure check behind [`require_operator_write`]. `port` is the bound port from [`BoundAddr`].
pub(crate) fn check(method: &Method, headers: &HeaderMap, port: Option<u16>) -> Result<(), Denial> {
    if *method == Method::OPTIONS {
        return Err(Denial::Preflight);
    }
    let port = port.ok_or(Denial::NoBoundPort)?;
    let host = format!("127.0.0.1:{port}");
    if single(headers, header::HOST) != Some(host.as_bytes()) {
        return Err(Denial::Host);
    }
    if single(headers, OPERATOR_HEADER) != Some(OPERATOR_HEADER_VALUE.as_bytes()) {
        return Err(Denial::OperatorHeader);
    }
    if headers.contains_key(header::COOKIE) {
        return Err(Denial::Cookie);
    }
    if headers.contains_key(header::ORIGIN) {
        let origin = format!("http://{host}");
        if single(headers, header::ORIGIN) != Some(origin.as_bytes()) {
            return Err(Denial::Origin);
        }
    }
    if headers.contains_key("sec-fetch-site") {
        match single(headers, "sec-fetch-site") {
            Some(b"same-origin") | Some(b"none") => {}
            _ => return Err(Denial::FetchSite),
        }
    }
    if headers.contains_key(header::CONTENT_TYPE) {
        match single(headers, header::CONTENT_TYPE) {
            Some(value) if !is_form_content_type(value) => {}
            _ => return Err(Denial::FormContentType),
        }
    }
    Ok(())
}

/// The value of `name` when the request carries it exactly once. `None` when it is absent or
/// repeated: a repeated header is never treated as equivalent to one.
fn single(headers: &HeaderMap, name: impl header::AsHeaderName) -> Option<&[u8]> {
    let mut values = headers.get_all(name).iter();
    let first = values.next()?;
    if values.next().is_some() {
        return None;
    }
    Some(first.as_bytes())
}

/// Whether a `Content-Type` is one of the three a browser may send cross-site without a preflight
/// (`application/x-www-form-urlencoded`, `multipart/form-data`, `text/plain`), i.e. a form post.
/// Only the media type is compared, case-insensitively, ignoring parameters.
fn is_form_content_type(value: &[u8]) -> bool {
    let essence = value.split(|b| *b == b';').next().unwrap_or_default();
    let essence = essence.trim_ascii().to_ascii_lowercase();
    matches!(
        essence.as_slice(),
        b"application/x-www-form-urlencoded" | b"multipart/form-data" | b"text/plain"
    )
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    const PORT: u16 = 4312;

    fn good() -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::HOST, HeaderValue::from_static("127.0.0.1:4312"));
        h.insert(OPERATOR_HEADER, HeaderValue::from_static("1"));
        h
    }

    fn with(mut h: HeaderMap, name: &'static str, value: &'static str) -> HeaderMap {
        h.append(name, HeaderValue::from_static(value));
        h
    }

    fn post(h: &HeaderMap) -> Result<(), Denial> {
        check(&Method::POST, h, Some(PORT))
    }

    // The minimal operator request passes, and so do the browser-shaped same-origin additions a
    // dashboard fetch carries.
    #[test]
    fn accepts_the_operator_shape() {
        assert_eq!(post(&good()), Ok(()));
        let h = with(good(), "origin", "http://127.0.0.1:4312");
        let h = with(h, "sec-fetch-site", "same-origin");
        let h = with(h, "content-type", "application/json");
        assert_eq!(post(&h), Ok(()));
        assert_eq!(post(&with(good(), "sec-fetch-site", "none")), Ok(()));
    }

    // Exact-once, exact-value: absent, wrong, repeated, and comma-joined operator headers all fail.
    #[test]
    fn operator_header_must_be_exactly_one_1() {
        let mut none = good();
        none.remove(OPERATOR_HEADER);
        assert_eq!(post(&none), Err(Denial::OperatorHeader));
        let mut zero = good();
        zero.insert(OPERATOR_HEADER, HeaderValue::from_static("0"));
        assert_eq!(post(&zero), Err(Denial::OperatorHeader));
        assert_eq!(
            post(&with(good(), OPERATOR_HEADER, "1")),
            Err(Denial::OperatorHeader),
            "a repeated header is not one header"
        );
        let mut joined = good();
        joined.insert(OPERATOR_HEADER, HeaderValue::from_static("1, 1"));
        assert_eq!(post(&joined), Err(Denial::OperatorHeader));
    }

    // Host is compared against the bound port. A forwarding header naming the right host changes
    // nothing, a rebound name fails, and a repeated Host fails.
    #[test]
    fn host_is_the_bound_loopback_address_only() {
        for bad in [
            "localhost:4312",
            "127.0.0.1:4313",
            "127.0.0.1",
            "evil.example:4312",
            "[::1]:4312",
        ] {
            let mut h = good();
            h.insert(header::HOST, HeaderValue::from_static(bad));
            let h = with(h, "x-forwarded-host", "127.0.0.1:4312");
            assert_eq!(post(&h), Err(Denial::Host), "Host {bad}");
        }
        assert_eq!(
            post(&with(good(), "host", "127.0.0.1:4312")),
            Err(Denial::Host)
        );
        let mut missing = good();
        missing.remove(header::HOST);
        assert_eq!(post(&missing), Err(Denial::Host));
        assert_eq!(
            check(&Method::POST, &good(), None),
            Err(Denial::NoBoundPort),
            "no bound port ⇒ fail closed"
        );
    }

    #[test]
    fn origin_must_be_the_bound_origin_when_present() {
        for bad in [
            "null",
            "http://evil.example",
            "http://localhost:4312",
            "https://127.0.0.1:4312",
            "http://127.0.0.1:4313",
            "rhapsody://localhost",
        ] {
            assert_eq!(
                post(&with(good(), "origin", bad)),
                Err(Denial::Origin),
                "{bad}"
            );
        }
        let twice = with(
            with(good(), "origin", "http://127.0.0.1:4312"),
            "origin",
            "http://127.0.0.1:4312",
        );
        assert_eq!(post(&twice), Err(Denial::Origin));
    }

    #[test]
    fn cross_site_fetch_metadata_cookies_and_preflight_fail() {
        for bad in ["cross-site", "same-site", ""] {
            assert_eq!(
                post(&with(good(), "sec-fetch-site", bad)),
                Err(Denial::FetchSite),
                "{bad}"
            );
        }
        assert_eq!(post(&with(good(), "cookie", "a=b")), Err(Denial::Cookie));
        assert_eq!(
            check(&Method::OPTIONS, &good(), Some(PORT)),
            Err(Denial::Preflight)
        );
    }

    #[test]
    fn form_content_types_fail() {
        for bad in [
            "application/x-www-form-urlencoded",
            "multipart/form-data; boundary=x",
            "text/plain;charset=UTF-8",
            " Text/Plain ",
        ] {
            assert_eq!(
                post(&with(good(), "content-type", bad)),
                Err(Denial::FormContentType),
                "{bad}"
            );
        }
        let twice = with(
            with(good(), "content-type", "application/json"),
            "content-type",
            "text/plain",
        );
        assert_eq!(post(&twice), Err(Denial::FormContentType));
    }
}

/// The guard as served: a real loopback server over every registered route.
#[cfg(test)]
mod router_tests {
    use std::sync::Arc;

    use reqwest::header::{CONTENT_TYPE, HOST};
    use serde_json::Value;

    use super::{DENIED_CODE, DENIED_MESSAGE, OPERATOR_HEADER};
    use crate::new_handler;
    use crate::testutil::{FakeProvider, empty_snapshot, spawn_router};

    /// Every mutating route. A route added to `build_router` with a POST side must be added here
    /// and wrapped in `operator_write`, or `every_route_refuses_an_unguarded_unsafe_request` fails.
    const MUTATING: &[&str] = &[
        "/api/v1/refresh",
        "/api/v1/drain",
        "/api/v1/config",
        "/api/v1/teams/config",
        "/api/v1/teams/invalidate",
        "/api/v1/teams/reinstate",
        "/api/v1/teams/room",
        "/api/v1/reviews/rerun",
        "/api/v1/reviews/dismiss",
        "/api/v1/reviews/clear",
        "/api/v1/runs/7/stop",
        "/api/v1/runs/7/resume",
        "/api/v1/runs/7/merge",
        "/api/v1/runs/7/handoff",
        "/api/v1/runs/7/retain",
        "/api/v1/runs/7/post",
        "/api/v1/runs/7/message",
        // STUDIO-1053: the held-ticket "Human step done → resume" action.
        "/api/v1/runs/7/resume-hold",
        // STUDIO-990: the ONE credentialed provider operation — the catalog refresh POST.
        "/api/v1/providers/7/models/refresh",
        // STUDIO-1048: authoring a provider DEFINITION (non-secret config, but still a local write).
        "/api/v1/providers/config",
    ];

    /// Every path `build_router` registers, read from its source so a new route cannot be missed
    /// by forgetting to list it. `{id}` becomes `7`.
    fn registered_paths() -> Vec<String> {
        let src = include_str!("server.rs");
        let body = src
            .split("fn build_router")
            .nth(1)
            .and_then(|s| s.split(".fallback(").next())
            .expect("build_router body");
        let paths: Vec<String> = body
            .split(".route(")
            .skip(1)
            .map(|chunk| {
                let start = chunk.find('"').expect("route path literal") + 1;
                let end = start + chunk[start..].find('"').expect("closing quote");
                chunk[start..end].replace("{id}", "7")
            })
            .collect();
        assert!(paths.len() > 40, "route scan found only {paths:?}");
        paths
    }

    struct Fixture {
        provider: Arc<FakeProvider>,
        base: String,
        port: u16,
    }

    async fn serve() -> Fixture {
        let provider = Arc::new(FakeProvider::ok(empty_snapshot()));
        let base = spawn_router(new_handler(provider.clone(), None)).await;
        let port = base
            .rsplit(':')
            .next()
            .and_then(|p| p.parse().ok())
            .expect("port");
        Fixture {
            provider,
            base,
            port,
        }
    }

    async fn assert_denied(resp: reqwest::Response, what: &str) {
        assert_eq!(resp.status(), 403, "{what}: status");
        let body: Value = resp.json().await.expect("json envelope");
        assert_eq!(
            body,
            serde_json::json!({"error": {"code": DENIED_CODE, "message": DENIED_MESSAGE}}),
            "{what}: envelope"
        );
    }

    /// The route inventory: an unsafe request without the operator header, to any registered
    /// route, is either refused by the guard or refused by the handler's own method check, and
    /// reaches no provider call either way. The routes that refuse by guard are exactly
    /// [`MUTATING`].
    #[tokio::test]
    async fn every_route_refuses_an_unguarded_unsafe_request() {
        let f = serve().await;
        let client = reqwest::Client::new();
        let mut guarded = Vec::new();
        for path in registered_paths() {
            for method in ["POST", "PUT", "PATCH", "DELETE", "OPTIONS"] {
                let method = reqwest::Method::from_bytes(method.as_bytes()).expect("method");
                let resp = client
                    .request(method.clone(), format!("{}{path}", f.base))
                    .header(CONTENT_TYPE, "application/json")
                    .body("{}")
                    .send()
                    .await
                    .expect("send");
                let status = resp.status().as_u16();
                match status {
                    405 => {}
                    403 => {
                        assert_denied(resp, &format!("{method} {path}")).await;
                        if method == reqwest::Method::POST {
                            guarded.push(path.clone());
                        }
                    }
                    other => panic!("{method} {path} without the operator header answered {other}"),
                }
            }
        }
        assert_eq!(
            f.provider.calls(),
            0,
            "a refused request reached the provider"
        );
        guarded.sort();
        let mut want: Vec<String> = MUTATING.iter().map(|p| p.to_string()).collect();
        want.sort();
        assert_eq!(guarded, want, "the guarded route set");
    }

    /// Each denial case against every mutating route: one 403 envelope, no provider call. The body
    /// is invalid JSON, so a guard that ran after body parsing would answer 400 instead.
    #[tokio::test]
    async fn every_mutation_denies_the_browser_origin_matrix() {
        let f = serve().await;
        let port = f.port;
        let good_host = format!("127.0.0.1:{port}");
        let good_origin = format!("http://127.0.0.1:{port}");
        type Case = (&'static str, Vec<(&'static str, String)>);
        let cases: Vec<Case> = vec![
            ("missing operator header", vec![]),
            ("operator header 0", vec![(OPERATOR_HEADER, "0".into())]),
            (
                "repeated operator header",
                vec![(OPERATOR_HEADER, "1".into()), (OPERATOR_HEADER, "1".into())],
            ),
            (
                "comma-joined operator header",
                vec![(OPERATOR_HEADER, "1, 1".into())],
            ),
            (
                "localhost Host",
                vec![
                    (OPERATOR_HEADER, "1".into()),
                    ("host", format!("localhost:{port}")),
                ],
            ),
            (
                "wrong port, forwarded host names the bound one",
                vec![
                    (OPERATOR_HEADER, "1".into()),
                    ("host", format!("127.0.0.1:{}", port.wrapping_add(1))),
                    ("x-forwarded-host", good_host.clone()),
                    ("x-forwarded-port", port.to_string()),
                ],
            ),
            (
                "DNS rebinding",
                vec![
                    (OPERATOR_HEADER, "1".into()),
                    ("host", format!("rebind.example:{port}")),
                    ("origin", format!("http://rebind.example:{port}")),
                    ("sec-fetch-site", "same-origin".into()),
                    ("x-forwarded-host", good_host.clone()),
                ],
            ),
            (
                "null origin",
                vec![(OPERATOR_HEADER, "1".into()), ("origin", "null".into())],
            ),
            (
                "cross-site origin",
                vec![
                    (OPERATOR_HEADER, "1".into()),
                    ("origin", "https://evil.example".into()),
                ],
            ),
            (
                "repeated origin",
                vec![
                    (OPERATOR_HEADER, "1".into()),
                    ("origin", good_origin.clone()),
                    ("origin", good_origin.clone()),
                ],
            ),
            (
                "cross-site fetch metadata",
                vec![
                    (OPERATOR_HEADER, "1".into()),
                    ("sec-fetch-site", "cross-site".into()),
                ],
            ),
            (
                "same-site fetch metadata",
                vec![
                    (OPERATOR_HEADER, "1".into()),
                    ("sec-fetch-site", "same-site".into()),
                ],
            ),
            (
                "cookie",
                vec![
                    (OPERATOR_HEADER, "1".into()),
                    ("cookie", "session=x".into()),
                ],
            ),
            (
                "urlencoded form post",
                vec![
                    (OPERATOR_HEADER, "1".into()),
                    ("content-type", "application/x-www-form-urlencoded".into()),
                ],
            ),
            (
                "multipart form post",
                vec![
                    (OPERATOR_HEADER, "1".into()),
                    ("content-type", "multipart/form-data; boundary=x".into()),
                ],
            ),
            (
                "text/plain post",
                vec![
                    (OPERATOR_HEADER, "1".into()),
                    ("content-type", "text/plain;charset=UTF-8".into()),
                ],
            ),
            (
                "bare form post, no custom header",
                vec![("content-type", "application/x-www-form-urlencoded".into())],
            ),
        ];
        let client = reqwest::Client::new();
        for path in MUTATING {
            for (name, headers) in &cases {
                let mut req = client.post(format!("{}{path}", f.base)).body("{\"");
                if !headers.iter().any(|(k, _)| *k == "content-type") {
                    req = req.header(CONTENT_TYPE, "application/json");
                }
                for (k, v) in headers {
                    req = req.header(*k, v.as_str());
                }
                let resp = req.send().await.expect("send");
                assert_denied(resp, &format!("POST {path}: {name}")).await;
            }
            let preflight = client
                .request(reqwest::Method::OPTIONS, format!("{}{path}", f.base))
                .header("origin", "https://evil.example")
                .header("access-control-request-method", "POST")
                .header("access-control-request-headers", OPERATOR_HEADER)
                .send()
                .await
                .expect("send");
            assert_eq!(
                preflight.headers().get("access-control-allow-origin"),
                None,
                "OPTIONS {path}: no CORS grant"
            );
            assert_denied(preflight, &format!("OPTIONS {path}: preflight")).await;
        }
        assert_eq!(
            f.provider.calls(),
            0,
            "a refused request reached the provider"
        );
        // The request reqwest builds with a Host override really did carry it: the same client,
        // correct Host, reaches the handler.
        let ok = client
            .post(format!("{}/api/v1/refresh", f.base))
            .header(HOST, good_host)
            .header(OPERATOR_HEADER, "1")
            .header(CONTENT_TYPE, "application/json")
            .body("{}")
            .send()
            .await
            .expect("send");
        assert_eq!(ok.status(), 202);
    }

    /// A body each route's own schema accepts, so the request gets past the handler's validation
    /// to a provider call.
    fn valid_body(path: &str) -> &'static str {
        match path {
            "/api/v1/drain" => r#"{"active":false}"#,
            "/api/v1/runs/7/message" => r#"{"text":"hi"}"#,
            // A non-empty note, so the request reaches the provider rather than the handler's
            // `empty_note` rejection.
            "/api/v1/runs/7/resume-hold" => r#"{"note":"done"}"#,
            // A remove reaches the provider's reference check (the definition never exists here,
            // so it answers 404 — past the guard and past body validation).
            "/api/v1/providers/config" => r#"{"op":"remove","provider_id":"unknown"}"#,
            _ => "{}",
        }
    }

    /// The operator's own request passes the guard on every mutating route and reaches its
    /// handler, with and without a dashboard's same-origin fetch metadata.
    #[tokio::test]
    async fn every_mutation_admits_the_operator_request() {
        let f = serve().await;
        let client = reqwest::Client::new();
        for path in MUTATING {
            for browser in [false, true] {
                let before = f.provider.calls();
                let body = valid_body(path);
                let mut req = client
                    .post(format!("{}{path}", f.base))
                    .header(OPERATOR_HEADER, "1")
                    .header(CONTENT_TYPE, "application/json")
                    .body(body);
                if browser {
                    req = req
                        .header("origin", format!("http://127.0.0.1:{}", f.port))
                        .header("sec-fetch-site", "same-origin");
                }
                let resp = req.send().await.expect("send");
                let status = resp.status().as_u16();
                let text = resp.text().await.unwrap_or_default();
                assert!(
                    !text.contains(DENIED_CODE),
                    "POST {path} (browser={browser}) was refused: {status} {text}"
                );
                assert!(
                    f.provider.calls() > before,
                    "POST {path} (browser={browser}) never reached the provider: {status} {text}"
                );
            }
        }
    }

    /// Reads keep their wire contract: a GET is never refused by the guard, even with a hostile
    /// origin and no operator header.
    #[tokio::test]
    async fn reads_are_not_guarded() {
        let f = serve().await;
        let client = reqwest::Client::new();
        for path in registered_paths() {
            if path == "/api/v1/logs/stream" {
                continue; // an infinite SSE stream; its GET contract is pinned in handlers_logs
            }
            let resp = client
                .get(format!("{}{path}", f.base))
                .header("origin", "https://evil.example")
                .header("sec-fetch-site", "cross-site")
                .send()
                .await
                .expect("send");
            let status = resp.status().as_u16();
            let text = resp.text().await.unwrap_or_default();
            assert!(
                !text.contains(DENIED_CODE),
                "GET {path} was refused by the write guard: {status}"
            );
        }
    }
}
