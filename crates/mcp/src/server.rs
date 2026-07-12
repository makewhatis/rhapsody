//! The `symphony mcp` server + always-on read tools — parity port of
//! `$REF/internal/mcpfacade/server.go`.
//!
//! [`Facade`] registers the seven always-on read tools (`symphony_state` / `_runs` / `_run` /
//! `_ticket` / `_logs` / `_events` + the derived `_run_status`) over the official `rmcp` SDK. Each
//! tool proxies the daemon's loopback API and surfaces a failure as an `IsError` tool result (not a
//! protocol error), so the model can see it and self-correct — daemon down ⇒ a clear
//! `daemon_unreachable`. The opt-in write tools (`symphony_send_message` / `_stop` / `_resume`) are
//! registered by a later phase (M2); this phase is read-only.

use crate::client::{Client, FacadeError};
use chrono::{DateTime, Utc};
use rhapsody_config::Config;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerInfo};
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;

/// Reported in the MCP server implementation handshake (server.go's `Version`).
pub const VERSION: &str = "0.1.0";

/// Per-process defaults for "me" resolution (server.go's `Options`). A dispatched worker's env
/// injection (SYMPHONY_RUN_ID / SYMPHONY_ISSUE, wired by the cmd layer) is threaded here so
/// `symphony_run` / `symphony_ticket` / `symphony_run_status` default to the worker's own run. A
/// coordinator session (no such env) leaves these empty and must pass an explicit id.
#[derive(Debug, Clone, Default)]
pub struct Options {
    pub default_run_id: String,
    pub default_issue: String,
    /// Overridable clock for tests; `None` ⇒ `Utc::now()` at call time (Go `Options.now`, nil ⇒
    /// `time.Now`).
    pub now: Option<DateTime<Utc>>,
}

impl Options {
    fn clock(&self) -> DateTime<Utc> {
        self.now.unwrap_or_else(Utc::now)
    }
}

/// The `symphony mcp` facade server: a thin client of the daemon's loopback API. Built by
/// [`Facade::new`]; served over stdio by [`Facade::run_stdio`]. Mirrors Go's `NewServer` — the
/// always-on read tools plus the derived `symphony_run_status`, and the opt-in write tools when
/// enabled in `cfg.mcp`.
pub struct Facade {
    pub(crate) client: Client,
    pub(crate) opts: Options,
    /// The resolved tool set — always-on reads + the `cfg.mcp`-gated writes — built once in
    /// [`Facade::new`] and consulted by the `#[tool_handler]` list/call/get plumbing.
    tool_router: ToolRouter<Facade>,
}

#[tool_router(router = read_router)]
impl Facade {
    /// Builds the `symphony mcp` server: always-on read tools plus the derived `symphony_run_status`,
    /// and — when enabled in `cfg.mcp` — the opt-in write tools (`registerWriteTools`). The mirror of
    /// Go's `NewServer(cfg, c, opts)`. A disabled write tool is not registered at all (invisible, per
    /// the design's "the gate is the enabled-tool set").
    pub fn new(cfg: &Config, client: Client, opts: Options) -> Self {
        // Reads are always present; the writes (registered by [`crate::writes`]) are gated per
        // `cfg.mcp` by REMOVING each disabled tool from the merged router — so a disabled tool is
        // absent from `list_tools` and rejected on call (writes.go: "not registered at all"), rather
        // than surfacing a runtime permission-denied.
        let mut tool_router = Self::read_router();
        tool_router.merge(Self::write_router());
        if !cfg.mcp.allow_send_message {
            tool_router.remove_route("symphony_send_message");
        }
        if !cfg.mcp.allow_stop {
            tool_router.remove_route("symphony_stop");
        }
        if !cfg.mcp.allow_resume {
            tool_router.remove_route("symphony_resume");
        }
        // symphony_handoff (TRA-242): ON by default, so removed only on an explicit opt-out — the
        // mirror of the send-message gate, not the stop/resume opt-in gate.
        if !cfg.mcp.allow_handoff {
            tool_router.remove_route("symphony_handoff");
        }
        Self {
            client,
            opts,
            tool_router,
        }
    }

    #[tool(
        name = "symphony_state",
        description = "Daemon overview: status, poll interval, counts, and the currently-running sessions (with last_event_at). Proxies GET /api/v1/state."
    )]
    async fn symphony_state(&self) -> CallToolResult {
        match self.client.get("/api/v1/state").await {
            Ok(body) => text_result(&body),
            Err(e) => err_result(&e),
        }
    }

    #[tool(
        name = "symphony_runs",
        description = "List recent + active runs. Merges GET /api/v1/history (filterable, paged) with the live GET /api/v1/state.running set."
    )]
    async fn symphony_runs(&self, Parameters(args): Parameters<RunsArgs>) -> CallToolResult {
        let mut pairs: Vec<(&str, String)> = vec![
            ("issue", args.issue),
            ("outcome", args.outcome),
            ("project", args.project),
        ];
        if args.limit > 0 {
            pairs.push(("limit", args.limit.to_string()));
        }
        if args.offset > 0 {
            pairs.push(("offset", args.offset.to_string()));
        }
        let hist = match self
            .client
            .get(&format!("/api/v1/history{}", encode_query(pairs)))
            .await
        {
            Ok(b) => b,
            Err(e) => return err_result(&e),
        };
        let state = match self.client.get_state().await {
            Ok(s) => s,
            Err(e) => return err_result(&e),
        };
        // Combine the live running set with the recent-history page into one object, embedding the
        // history bytes verbatim (Go's `json.RawMessage`) and re-marshaling the running projection.
        // Keys sort recent < running, matching Go's `json.Marshal` of a `map[string]…`.
        let running = serde_json::to_vec(&state.running).unwrap_or_else(|_| b"[]".to_vec());
        let mut merged = Vec::with_capacity(hist.len() + running.len() + 24);
        merged.extend_from_slice(b"{\"recent\":");
        merged.extend_from_slice(&hist);
        merged.extend_from_slice(b",\"running\":");
        merged.extend_from_slice(&running);
        merged.push(b'}');
        text_result(&merged)
    }

    #[tool(
        name = "symphony_run",
        description = "One run's detail: outcome, live flag, issue state, turn/token counts, last_event_at, recent events. Proxies GET /api/v1/runs/{id}. Defaults run_id to SYMPHONY_RUN_ID."
    )]
    async fn symphony_run(&self, Parameters(args): Parameters<RunArgs>) -> CallToolResult {
        let id = or_default(&args.run_id, &self.opts.default_run_id);
        if id.is_empty() {
            return err_result(&FacadeError::new(
                "bad_request",
                "no run_id given and SYMPHONY_RUN_ID is not set",
            ));
        }
        match self
            .client
            .get(&format!("/api/v1/runs/{}", path_escape(&id)))
            .await
        {
            Ok(body) => text_result(&body),
            Err(e) => err_result(&e),
        }
    }

    #[tool(
        name = "symphony_ticket",
        description = "A Linear issue's run history. Proxies GET /api/v1/issues/{identifier}/history. Defaults identifier to SYMPHONY_ISSUE."
    )]
    async fn symphony_ticket(&self, Parameters(args): Parameters<TicketArgs>) -> CallToolResult {
        let id = or_default(&args.identifier, &self.opts.default_issue);
        if id.is_empty() {
            return err_result(&FacadeError::new(
                "bad_request",
                "no identifier given and SYMPHONY_ISSUE is not set",
            ));
        }
        match self
            .client
            .get(&format!("/api/v1/issues/{}/history", path_escape(&id)))
            .await
        {
            Ok(body) => text_result(&body),
            Err(e) => err_result(&e),
        }
    }

    #[tool(
        name = "symphony_logs",
        description = "Daemon process-log ring snapshot (recent daemon-level log lines). Proxies GET /api/v1/logs."
    )]
    async fn symphony_logs(&self) -> CallToolResult {
        match self.client.get("/api/v1/logs").await {
            Ok(body) => text_result(&body),
            Err(e) => err_result(&e),
        }
    }

    #[tool(
        name = "symphony_events",
        description = "Cross-run event search. Proxies GET /api/v1/events?q=&issue=&kind=&limit=."
    )]
    async fn symphony_events(&self, Parameters(args): Parameters<EventsArgs>) -> CallToolResult {
        let mut pairs: Vec<(&str, String)> =
            vec![("q", args.text), ("issue", args.issue), ("kind", args.kind)];
        if args.limit > 0 {
            pairs.push(("limit", args.limit.to_string()));
        }
        match self
            .client
            .get(&format!("/api/v1/events{}", encode_query(pairs)))
            .await
        {
            Ok(body) => text_result(&body),
            Err(e) => err_result(&e),
        }
    }

    #[tool(
        name = "symphony_run_status",
        description = "The headline verdict for a run or issue: alive (last event <age>) | stalled | completed(outcome) | failed(reason) | interrupted (recovery pending) | not-dispatched(reason). Composes /state + /runs/{id} + /issues/{id}/history; a not-dispatched reason is never fabricated (unknown when unresolvable). Defaults to \"me\" via SYMPHONY_RUN_ID / SYMPHONY_ISSUE."
    )]
    async fn symphony_run_status(
        &self,
        Parameters(args): Parameters<StatusArgs>,
    ) -> CallToolResult {
        // Explicit args win over the "me" env defaults. Only when the caller passes NEITHER run_id
        // nor issue do we fall back to SYMPHONY_RUN_ID / SYMPHONY_ISSUE — otherwise an env-filled
        // run id would shadow an explicitly-passed issue and silently take the run-id path.
        let (mut run_id, mut issue) = (args.run_id, args.issue);
        if run_id.is_empty() && issue.is_empty() {
            run_id = self.opts.default_run_id.clone();
            issue = self.opts.default_issue.clone();
        }
        match self
            .client
            .run_status(self.opts.clock(), &run_id, &issue)
            .await
        {
            Ok(st) => match serde_json::to_vec(&st) {
                Ok(body) => text_result(&body),
                Err(e) => err_result(&FacadeError::new("encode_error", e.to_string())),
            },
            Err(e) => err_result(&e),
        }
    }
}

impl Facade {
    /// Serves the facade over stdio (newline-delimited JSON-RPC on stdin/stdout) until the peer
    /// disconnects — the `symphony mcp` server loop (mcp.go's `srv.Run(ctx, &mcp.StdioTransport{})`).
    pub async fn run_stdio(self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        use rmcp::ServiceExt;
        let running = self.serve(rmcp::transport::stdio()).await?;
        running.waiting().await?;
        Ok(())
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for Facade {
    /// The initialize handshake: implementation name/version + the tools capability (server.go's
    /// `mcp.Implementation{Name: "symphony", Version}`).
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("symphony", VERSION))
    }
}

/// Wraps a raw JSON body as a successful (unstructured text) tool result — the daemon's JSON is the
/// payload (server.go's `textResult`). `pub(crate)` so the write tools ([`crate::writes`]) share it,
/// mirroring Go's package-private `textResult`.
pub(crate) fn text_result(body: &[u8]) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(
        String::from_utf8_lossy(body).into_owned(),
    )])
}

/// Surfaces a tool failure as an `IsError` result (NOT a protocol error), so the agent can see it
/// and self-correct — the FacadeError code (e.g. `daemon_unreachable`) leads the message
/// (server.go's `errResult`). `pub(crate)` so the write tools ([`crate::writes`]) share it.
pub(crate) fn err_result(err: &FacadeError) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(err.to_string())])
}

/// `v` when non-empty, else `def` (server.go's `orDefault`). `pub(crate)` so the write tools' run-id
/// defaulting ([`crate::writes`]) shares it.
pub(crate) fn or_default(v: &str, def: &str) -> String {
    if v.is_empty() {
        def.to_string()
    } else {
        v.to_string()
    }
}

/// Percent-escapes one path segment exactly like Go's `url.PathEscape` (encodePathSegment): every
/// byte except the unreserved marks (A-Za-z0-9-_.~) and the segment-safe sub-delims (`$&+:=@`) is
/// %XX-encoded, so a value with reserved characters (space, `/`, …) can't alter the request path
/// (client_test.go `TestClientPathEscapesIDs`).
pub(crate) fn path_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        let keep = b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'-' | b'_' | b'.' | b'~' | b'$' | b'&' | b'+' | b':' | b'=' | b'@'
            );
        if keep {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(hex_upper(b >> 4));
            out.push(hex_upper(b & 0x0f));
        }
    }
    out
}

/// Query-escapes one component like Go's `url.QueryEscape`: unreserved (A-Za-z0-9-_.~) pass through,
/// a space becomes `+`, every other byte is %XX-encoded.
fn query_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else if b == b' ' {
            out.push('+');
        } else {
            out.push('%');
            out.push(hex_upper(b >> 4));
            out.push(hex_upper(b & 0x0f));
        }
    }
    out
}

/// Builds the query string for a set of `key=value` pairs like Go's `url.Values.Encode` +
/// `encodeQuery` (server.go): drops empty values (`setIf`), sorts by key, query-escapes each, and
/// prefixes `?` — or the empty string when nothing remains.
fn encode_query(pairs: Vec<(&str, String)>) -> String {
    let mut pairs: Vec<(&str, String)> = pairs.into_iter().filter(|(_, v)| !v.is_empty()).collect();
    if pairs.is_empty() {
        return String::new();
    }
    pairs.sort_by(|a, b| a.0.cmp(b.0));
    let mut q = String::from("?");
    for (i, (k, v)) in pairs.iter().enumerate() {
        if i > 0 {
            q.push('&');
        }
        q.push_str(&query_escape(k));
        q.push('=');
        q.push_str(&query_escape(v));
    }
    q
}

/// Upper-case hex digit for a nibble (0–15).
fn hex_upper(nibble: u8) -> char {
    char::from_digit(nibble as u32, 16)
        .unwrap_or('0')
        .to_ascii_uppercase()
}

// --- tool argument structs (server.go's `*Args`) ----------------------------------------------
// Every field is optional (Go `,omitempty`): `#[serde(default)]` so an empty `{}` deserializes.

/// `symphony_run` args — also reused by the `symphony_stop` / `symphony_resume` write tools
/// ([`crate::writes`]), exactly as Go's `runArgs` is shared across server.go and writes.go.
#[derive(Debug, Default, Deserialize, JsonSchema)]
pub(crate) struct RunArgs {
    /// the run id (numeric). Defaults to SYMPHONY_RUN_ID (the worker's own run) when omitted.
    #[serde(default)]
    pub(crate) run_id: String,
}

/// `symphony_ticket` args.
#[derive(Debug, Default, Deserialize, JsonSchema)]
struct TicketArgs {
    /// the Linear issue identifier (e.g. INF-473). Defaults to SYMPHONY_ISSUE when omitted.
    #[serde(default)]
    identifier: String,
}

/// `symphony_runs` args.
#[derive(Debug, Default, Deserialize, JsonSchema)]
struct RunsArgs {
    /// filter to one issue identifier (exact).
    #[serde(default)]
    issue: String,
    /// filter by run outcome (running|continued|completed|stopped|failed|interrupted).
    #[serde(default)]
    outcome: String,
    /// filter by project slug (exact).
    #[serde(default)]
    project: String,
    /// max runs to return (0 ⇒ store default page).
    #[serde(default)]
    limit: i64,
    /// pagination offset.
    #[serde(default)]
    offset: i64,
}

/// `symphony_events` args.
#[derive(Debug, Default, Deserialize, JsonSchema)]
struct EventsArgs {
    /// substring to search for across run events.
    #[serde(default)]
    text: String,
    /// limit the search to one issue identifier.
    #[serde(default)]
    issue: String,
    /// limit to one event kind.
    #[serde(default)]
    kind: String,
    /// max hits to return.
    #[serde(default)]
    limit: i64,
}

/// `symphony_run_status` args.
#[derive(Debug, Default, Deserialize, JsonSchema)]
struct StatusArgs {
    /// a run id to judge. Defaults to SYMPHONY_RUN_ID when omitted.
    #[serde(default)]
    run_id: String,
    /// an issue identifier to judge (used when run_id is absent). Defaults to SYMPHONY_ISSUE.
    #[serde(default)]
    issue: String,
}

#[cfg(test)]
mod tests {
    //! Mirror of `$REF/internal/mcpfacade/server_test.go` (+ the server-level
    //! `TestRunStatusExplicitIssueBeatsEnvRunID` from status_test.go), driven through an in-memory
    //! MCP client over a tokio duplex — the analogue of Go's `mcp.NewInMemoryTransports()`.
    use super::*;
    use crate::testutil::{spawn_router, test_config};
    use crate::verdict::{KIND_ALIVE, KIND_NOT_DISPATCHED, Status};
    use axum::Router;
    use axum::routing::any;
    use chrono::TimeZone;
    use rmcp::ServiceExt;
    use rmcp::model::CallToolRequestParams;
    use rmcp::service::RunningService;
    use std::sync::{Arc, Mutex};

    /// Connects an in-memory MCP client to `facade` over a duplex pipe (Go's `connectInMemory`).
    async fn connect(facade: Facade) -> RunningService<rmcp::RoleClient, ()> {
        let (client_t, server_t) = tokio::io::duplex(1 << 16);
        tokio::spawn(async move {
            if let Ok(server) = facade.serve(server_t).await {
                let _ = server.waiting().await;
            }
        });
        ().serve(client_t).await.expect("client connect")
    }

    fn result_text(res: &CallToolResult) -> String {
        res.content
            .iter()
            .filter_map(|c| c.as_text())
            .map(|t| t.text.as_str())
            .collect()
    }

    fn call(name: &str) -> CallToolRequestParams {
        CallToolRequestParams::new(name.to_string())
    }

    // The always-on read tools + the derived run_status are registered regardless of config.
    #[tokio::test]
    async fn read_tools_always_registered() {
        let facade = Facade::new(&test_config(), Client::for_port(0), Options::default());
        let client = connect(facade).await;
        let tools = client.list_all_tools().await.expect("list tools");
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
        for want in [
            "symphony_state",
            "symphony_runs",
            "symphony_run",
            "symphony_ticket",
            "symphony_logs",
            "symphony_events",
            "symphony_run_status",
        ] {
            assert!(
                names.contains(&want),
                "read tool {want:?} not registered: {names:?}"
            );
        }
        let _ = client.cancel().await;
    }

    // Daemon down (no server.port) ⇒ a read tool returns a clear daemon_unreachable error result
    // (IsError text), not a protocol error.
    #[tokio::test]
    async fn read_tool_daemon_unreachable() {
        let facade = Facade::new(&test_config(), Client::for_port(0), Options::default());
        let client = connect(facade).await;
        let res = client
            .call_tool(call("symphony_state"))
            .await
            .expect("call");
        assert_eq!(res.is_error, Some(true), "want IsError result");
        assert!(
            result_text(&res).contains("daemon_unreachable"),
            "text = {:?}",
            result_text(&res)
        );
        let _ = client.cancel().await;
    }

    // run_status end-to-end against a fake daemon: a fresh running row ⇒ alive.
    #[tokio::test]
    async fn run_status_alive_end_to_end() {
        let now = Utc.with_ymd_and_hms(2026, 7, 6, 12, 0, 0).unwrap();
        let fresh =
            (now - chrono::Duration::seconds(5)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let state = format!(
            r#"{{"status":"running","running":[{{"issue_identifier":"INF-1","run_id":7,"last_event_at":"{fresh}"}}]}}"#
        );
        let run = format!(
            r#"{{"run_id":7,"issue_identifier":"INF-1","outcome":"running","live":true,"last_event_at":"{fresh}"}}"#
        );
        let router = Router::new()
            .route("/api/v1/state", json_route(state))
            .route("/api/v1/runs/7", json_route(run));
        let port = spawn_router(router).await;

        let facade = Facade::new(
            &test_config(),
            Client::for_port(port as i64),
            Options {
                default_run_id: "7".into(),
                now: Some(now),
                ..Default::default()
            },
        );
        let client = connect(facade).await;
        let res = client
            .call_tool(call("symphony_run_status"))
            .await
            .expect("call");
        assert_ne!(
            res.is_error,
            Some(true),
            "unexpected error: {}",
            result_text(&res)
        );
        let st: Status = serde_json::from_str(&result_text(&res)).expect("decode status");
        assert_eq!(st.kind, KIND_ALIVE, "summary {}", st.summary);
        assert_eq!(st.run_id, 7);
        let _ = client.cancel().await;
    }

    // run_status for an issue with no run history and no running row ⇒ not-dispatched(unknown).
    #[tokio::test]
    async fn run_status_not_dispatched_unknown() {
        let router = Router::new()
            .route(
                "/api/v1/state",
                json_route(r#"{"status":"running","running":[]}"#.to_string()),
            )
            .route(
                "/api/v1/issues/INF-404/history",
                json_route(r#"{"issue_identifier":"INF-404","runs":[]}"#.to_string()),
            );
        let port = spawn_router(router).await;

        let facade = Facade::new(
            &test_config(),
            Client::for_port(port as i64),
            Options {
                default_issue: "INF-404".into(),
                ..Default::default()
            },
        );
        let client = connect(facade).await;
        let res = client
            .call_tool(call("symphony_run_status"))
            .await
            .expect("call");
        let st: Status = serde_json::from_str(&result_text(&res)).expect("decode status");
        assert_eq!(st.kind, KIND_NOT_DISPATCHED);
        assert!(st.reason.contains("unknown"), "reason {}", st.reason);
        let _ = client.cancel().await;
    }

    // An explicitly-passed issue must win over an env-default run id: the ISSUE path is taken
    // (/issues/INF-9/history requested, /runs/7 NOT requested).
    #[tokio::test]
    async fn run_status_explicit_issue_beats_env_run_id() {
        let paths: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = paths.clone();
        let router = Router::new().fallback(any(move |uri: axum::http::Uri| {
            let sink = sink.clone();
            async move {
                sink.lock().unwrap().push(uri.path().to_string());
                let body = match uri.path() {
                    "/api/v1/state" => r#"{"status":"running","running":[]}"#,
                    "/api/v1/issues/INF-9/history" => r#"{"issue_identifier":"INF-9","runs":[]}"#,
                    _ => "{}",
                };
                axum::response::Response::builder()
                    .header("Content-Type", "application/json")
                    .body(axum::body::Body::from(body))
                    .unwrap()
            }
        }));
        let port = spawn_router(router).await;

        // DefaultRunID is set (worker "me" env), but the caller explicitly asks about a different issue.
        let facade = Facade::new(
            &test_config(),
            Client::for_port(port as i64),
            Options {
                default_run_id: "7".into(),
                default_issue: "INF-1".into(),
                ..Default::default()
            },
        );
        let client = connect(facade).await;
        let params = call("symphony_run_status").with_arguments(
            serde_json::json!({"issue": "INF-9"})
                .as_object()
                .cloned()
                .unwrap(),
        );
        let res = client.call_tool(params).await.expect("call");
        assert_ne!(
            res.is_error,
            Some(true),
            "unexpected error: {}",
            result_text(&res)
        );

        let joined = paths.lock().unwrap().join(",");
        assert!(
            joined.contains("/api/v1/issues/INF-9/history"),
            "expected the issue path; paths={joined}"
        );
        assert!(
            !joined.contains("/api/v1/runs/7"),
            "run-id path was taken despite an explicit issue arg; paths={joined}"
        );
        let _ = client.cancel().await;
    }

    /// A stub route serving `body` as JSON (cloned per request so the handler stays `Fn`).
    fn json_route(body: String) -> axum::routing::MethodRouter {
        any(move || {
            let body = body.clone();
            async move {
                axum::response::Response::builder()
                    .header("Content-Type", "application/json")
                    .body(axum::body::Body::from(body))
                    .unwrap()
            }
        })
    }
}
