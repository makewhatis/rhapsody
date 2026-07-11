//! Loopback HTTP client of the daemon's `/api/v1` API — parity port of
//! `$REF/internal/mcpfacade/client.go`.
//!
//! [`Client`] carries the resolved loopback base URL (`127.0.0.1:<port>`) and a `reqwest` client
//! (15s timeout, the mirror of Go's `*http.Client`). It reads NOTHING from `~/.symphony` or the DB
//! — the daemon stays the single source of truth (INF-473). Every failure maps to a
//! [`FacadeError`]: the daemon's `errorEnvelope` code when the HTTP layer answered, or
//! `daemon_unreachable` when the daemon could not be contacted at all.

use rhapsody_config::Config;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::time::Duration;

/// The HTTP timeout Go applies in `NewClientForPort` (`$REF/internal/mcpfacade/client.go`: 15s).
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);

/// A tool-surfaced error — parity port of client.go's `FacadeError`. `code` mirrors the daemon's
/// `errorEnvelope` code (httpapi `handlers.go`) when the failure came from the HTTP layer, or
/// `daemon_unreachable` when the daemon could not be contacted at all. `status` is the HTTP status
/// (0 when the failure was not HTTP-sourced), mirroring Go's `int` `Status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FacadeError {
    pub code: String,
    pub message: String,
    pub status: u16,
}

impl FacadeError {
    /// A non-HTTP FacadeError (status 0), the mirror of `&FacadeError{Code, Message}`.
    pub(crate) fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            status: 0,
        }
    }
}

impl fmt::Display for FacadeError {
    /// Mirrors Go `(*FacadeError).Error`: `code` alone when the message is empty, else
    /// `code: message`. The leading code is what the tool surfaces to the model (daemon down ⇒ the
    /// message starts with `daemon_unreachable`).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.message.is_empty() {
            write!(f, "{}", self.code)
        } else {
            write!(f, "{}: {}", self.code, self.message)
        }
    }
}

impl std::error::Error for FacadeError {}

/// The `daemon_unreachable` FacadeError (client.go's `unreachable`): the daemon could not be
/// contacted at all — an unset port, or a refused/timed-out dial.
pub(crate) fn unreachable(msg: impl Into<String>) -> FacadeError {
    FacadeError {
        code: "daemon_unreachable".into(),
        message: msg.into(),
        status: 0,
    }
}

/// Mirrors httpapi `errorEnvelope` (`handlers.go`): `{"error":{"code","message"}}`.
#[derive(Deserialize)]
struct ErrorEnvelope {
    #[serde(default)]
    error: ErrorEnvelopeInner,
}

#[derive(Default, Deserialize)]
struct ErrorEnvelopeInner {
    #[serde(default)]
    code: String,
    #[serde(default)]
    message: String,
}

/// An HTTP client of the daemon's loopback API. `base` is `Some("http://127.0.0.1:<port>")` when
/// the resolved port is valid, else `None` (an unaddressable daemon — every request returns
/// `daemon_unreachable`, matching Go's empty-string `base`). `http` is `None` only if the reqwest
/// client failed to build (surfaced as `daemon_unreachable` at request time, so construction stays
/// infallible like Go's `NewClientForPort`).
pub struct Client {
    base: Option<String>,
    http: Option<reqwest::Client>,
}

impl Client {
    /// Builds a loopback client from a resolved config's `server.port` — the mirror of Go's
    /// `NewClient` (config-only). The runtime.json discovery ([`crate::resolve_daemon_port`]) is a
    /// separate, higher-precedence resolution; this constructor is the config fallback / an
    /// explicit-port pin.
    pub fn new(cfg: &Config) -> Self {
        Self::for_port(port_from_config(cfg))
    }

    /// Builds a loopback client for an explicit port — the mirror of Go's `NewClientForPort`. A
    /// port ≤ 0 yields an empty base, so requests return a clear `daemon_unreachable` error rather
    /// than the process refusing to start (the read tools are still registered).
    pub fn for_port(port: i64) -> Self {
        let base = if port > 0 {
            Some(format!("http://127.0.0.1:{port}"))
        } else {
            None
        };
        let http = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .ok();
        Self { base, http }
    }

    /// Issues an HTTP request to the loopback API and returns the raw body on 2xx, or a
    /// [`FacadeError`] otherwise (client.go's `do`). `body` is `None` for GET. A refused/timed-out
    /// dial is `daemon_unreachable`; a ≥400 response with a decodable envelope carries the daemon's
    /// own code, else `http_error`.
    pub(crate) async fn do_request(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> Result<Vec<u8>, FacadeError> {
        let base = self.base.as_deref().ok_or_else(|| {
            unreachable(
                "daemon HTTP API not reachable — set a fixed server.port in WORKFLOW.md so `symphony mcp` can address the daemon",
            )
        })?;
        // A build failure (impossible in practice with no TLS backend to init) is surfaced as
        // unreachable rather than a panic, keeping construction infallible like Go.
        let http = self
            .http
            .as_ref()
            .ok_or_else(|| unreachable("daemon HTTP client unavailable"))?;

        let url = format!("{base}{path}");
        let mut req = http.request(method.clone(), &url);
        if let Some(b) = body {
            req = req.header("Content-Type", "application/json").body(b);
        }
        let resp = match req.send().await {
            Ok(r) => r,
            // Connection refused / timeout / DNS — the daemon is down or not listening.
            Err(e) => {
                return Err(unreachable(format!(
                    "{method} {path}: {e} (is the daemon running?)"
                )));
            }
        };

        let status = resp.status();
        // Mirror Go's `raw, _ := io.ReadAll(resp.Body)`: a read error yields an empty body, not a
        // hard failure — the status code still drives the branch below.
        let raw = resp.bytes().await.unwrap_or_default();
        if status.as_u16() >= 400 {
            if let Ok(env) = serde_json::from_slice::<ErrorEnvelope>(&raw)
                && !env.error.code.is_empty()
            {
                return Err(FacadeError {
                    code: env.error.code,
                    message: env.error.message,
                    status: status.as_u16(),
                });
            }
            return Err(FacadeError {
                code: "http_error".into(),
                message: String::from_utf8_lossy(&raw).trim().to_string(),
                status: status.as_u16(),
            });
        }
        Ok(raw.to_vec())
    }

    /// GET `path`, returning the raw 2xx body (client.go's `get`).
    pub(crate) async fn get(&self, path: &str) -> Result<Vec<u8>, FacadeError> {
        self.do_request(reqwest::Method::GET, path, None).await
    }

    /// Fetches and decodes `GET /api/v1/state` (client.go's `getState`).
    pub(crate) async fn get_state(&self) -> Result<StateResp, FacadeError> {
        let raw = self.get("/api/v1/state").await?;
        serde_json::from_slice(&raw).map_err(|e| FacadeError::new("decode_error", e.to_string()))
    }

    /// Fetches and decodes `GET /api/v1/runs/{id}` (client.go's `getRun`). The id is path-escaped
    /// for consistency with the direct tool handlers.
    pub(crate) async fn get_run(&self, run_id: &str) -> Result<RunDetail, FacadeError> {
        let raw = self
            .get(&format!(
                "/api/v1/runs/{}",
                crate::server::path_escape(run_id)
            ))
            .await?;
        serde_json::from_slice(&raw).map_err(|e| FacadeError::new("decode_error", e.to_string()))
    }

    /// Fetches and decodes `GET /api/v1/issues/{identifier}/history` (client.go's
    /// `getIssueHistory`). The identifier is path-escaped for consistency with the direct tool
    /// handlers.
    pub(crate) async fn get_issue_history(
        &self,
        identifier: &str,
    ) -> Result<IssueHistoryResp, FacadeError> {
        let raw = self
            .get(&format!(
                "/api/v1/issues/{}/history",
                crate::server::path_escape(identifier)
            ))
            .await?;
        serde_json::from_slice(&raw).map_err(|e| FacadeError::new("decode_error", e.to_string()))
    }
}

/// Extracts the configured loopback port (0 when unset) — client.go's `portFromConfig`.
pub(crate) fn port_from_config(cfg: &Config) -> i64 {
    cfg.server.port.unwrap_or(0)
}

// --- wire structs -----------------------------------------------------------------------------
// A minimal mirror of the httpapi response shapes (responses.go / responses_history.go). Only the
// fields the facade reads are declared; unknown fields are ignored by serde (so daemon-side
// additions are non-breaking) — the Rust-idiomatic form of client.go's "minimal mirror". Go
// `int64`/`int` → `i64`.

/// Mirror of `GET /api/v1/state` (client.go's `stateResp`): only the live `running` set the facade
/// reads (for `symphony_runs`' merge and the run-status lookups).
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct StateResp {
    #[serde(default)]
    pub running: Vec<RunningSession>,
}

/// One live session in `/state.running` (client.go's `runningSession`). Serializable so
/// `symphony_runs` can re-emit exactly these fields (the projection Go's `json.Marshal(Running)`
/// produces), with no `omitempty` — every field is always present, matching Go's tag-less struct.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct RunningSession {
    #[serde(default)]
    pub issue_id: String,
    #[serde(default)]
    pub issue_identifier: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub run_id: i64,
    #[serde(default)]
    pub turn_count: i64,
    #[serde(default)]
    pub started_at: String,
    #[serde(default)]
    pub last_event_at: String,
}

/// Mirror of `GET /api/v1/runs/{id}` (client.go's `runDetail`): the fields the run-status verdict
/// reads (outcome/live/error/last_event + identity). Other response fields are ignored by serde.
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct RunDetail {
    #[serde(default)]
    pub run_id: i64,
    #[serde(default)]
    pub issue_identifier: String,
    #[serde(default)]
    pub outcome: String,
    #[serde(default)]
    pub live: bool,
    #[serde(default)]
    pub last_event_at: String,
    #[serde(default)]
    pub error: String,
}

/// Mirror of `GET /api/v1/issues/{id}/history` (client.go's `issueHistoryResp`).
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct IssueHistoryResp {
    #[serde(default)]
    pub runs: Vec<RunSummary>,
}

/// One run row in an issue's history (client.go's `runSummary`): the fields the issue-path
/// summary fallback carries into a synthesized [`RunDetail`].
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct RunSummary {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub issue_identifier: String,
    #[serde(default)]
    pub outcome: String,
    #[serde(default)]
    pub error: String,
}

#[cfg(test)]
mod tests {
    //! Mirror of `$REF/internal/mcpfacade/client_test.go`.
    use super::*;
    use crate::testutil::{client_for_port, spawn_router, test_config};
    use axum::Router;
    use axum::routing::any;
    use std::sync::{Arc, Mutex};

    #[test]
    fn client_base_url_from_port() {
        let mut cfg = test_config();
        cfg.server.port = Some(8799);
        let c = Client::new(&cfg);
        assert_eq!(c.base.as_deref(), Some("http://127.0.0.1:8799"));
    }

    #[test]
    fn new_client_for_port() {
        assert_eq!(
            Client::for_port(1234).base.as_deref(),
            Some("http://127.0.0.1:1234")
        );
        assert_eq!(Client::for_port(0).base, None, "port 0 ⇒ unaddressable");
        assert_eq!(
            Client::for_port(-1).base,
            None,
            "negative port ⇒ unaddressable"
        );
    }

    #[tokio::test]
    async fn client_path_escapes_ids() {
        // F1: ids / identifiers must be path-escaped so a value with reserved characters can't
        // alter the request path (consistency with the direct tool handlers).
        let paths: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = paths.clone();
        let router = Router::new().fallback(any(move |uri: axum::http::Uri| {
            let sink = sink.clone();
            async move {
                sink.lock().unwrap().push(uri.path().to_string());
                axum::Json(serde_json::json!({}))
            }
        }));
        let client = client_for_port(spawn_router(router).await);

        client.get_run("a b").await.expect("getRun");
        client
            .get_issue_history("INF 1/x")
            .await
            .expect("getIssueHistory");

        let got = paths.lock().unwrap().clone();
        assert_eq!(got.len(), 2, "got {got:?}");
        assert_eq!(got[0], "/api/v1/runs/a%20b");
        assert_eq!(got[1], "/api/v1/issues/INF%201%2Fx/history");
    }

    #[tokio::test]
    async fn client_unset_port_is_unreachable() {
        let c = Client::new(&test_config()); // server.port None
        let err = c.get_state().await.expect_err("want error");
        assert_eq!(err.code, "daemon_unreachable");
    }

    #[tokio::test]
    async fn client_decodes_state() {
        let router = Router::new().route(
            "/api/v1/state",
            any(|| async {
                axum::response::Response::builder()
                    .header("Content-Type", "application/json")
                    .body(axum::body::Body::from(
                        r#"{"status":"running","poll_interval_ms":30000,"running":[{"issue_identifier":"INF-1","run_id":7,"last_event_at":"2026-07-06T12:00:00Z"}]}"#,
                    ))
                    .unwrap()
            }),
        );
        let client = client_for_port(spawn_router(router).await);
        let st = client.get_state().await.expect("state");
        assert_eq!(st.running.len(), 1);
        assert_eq!(st.running[0].issue_identifier, "INF-1");
        assert_eq!(st.running[0].run_id, 7);
    }

    #[tokio::test]
    async fn client_maps_error_envelope() {
        let router = Router::new().fallback(any(|| async {
            (
                axum::http::StatusCode::NOT_FOUND,
                [("Content-Type", "application/json")],
                r#"{"error":{"code":"not_found","message":"no such run"}}"#,
            )
        }));
        let client = client_for_port(spawn_router(router).await);
        let err = client.get_run("999").await.expect_err("want error");
        assert_eq!(err.code, "not_found");
        assert_eq!(err.status, 404);
    }

    #[tokio::test]
    async fn client_conn_refused_is_unreachable() {
        // Point at a port nothing listens on: bind then drop the listener.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let c = Client::for_port(port as i64);
        let err = c.get_state().await.expect_err("want error");
        assert_eq!(err.code, "daemon_unreachable");
    }
}
