//! The config-gated write tools — parity port of `$REF/internal/mcpfacade/writes.go`.
//!
//! [`Facade::write_router`] registers the opt-in write tools; [`Facade::new`] (server.rs) then
//! REMOVES any tool disabled in `cfg.mcp`, so a disabled write tool is not registered at all — the
//! gate is the enabled-tool set, so a disabled tool is invisible to the agent (absent from
//! `list_tools`, rejected on call) rather than a runtime "permission denied" (design: Tool surface
//! §Write).
//!
//!   - `symphony_send_message`: ON by default (`cfg.mcp.allow_send_message`) — the INF-250 mailbox.
//!   - `symphony_stop` / `symphony_resume`: OFF by default (`cfg.mcp.allow_stop` / `allow_resume`).

use crate::client::FacadeError;
use crate::server::{Facade, RunArgs, err_result, or_default, text_result};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;

/// `symphony_send_message` args (writes.go's `sendMessageArgs`). `run_id` mirrors Go's `,omitempty`
/// (optional; defaults to SYMPHONY_RUN_ID), while `text` carries NO `omitempty` in Go, so it is a
/// REQUIRED field in the emitted JSON schema — the tool contract an agent reads. A present-but-empty
/// text is still caught at runtime as `empty_text`, exactly as Go's handler does.
#[derive(Debug, Default, Deserialize, JsonSchema)]
pub(crate) struct SendMessageArgs {
    /// the target run id (numeric). Defaults to SYMPHONY_RUN_ID (the worker's own run) when omitted.
    #[serde(default)]
    run_id: String,
    /// the message text to deliver to the running agent (required, max 4000 chars).
    text: String,
}

#[tool_router(router = write_router, vis = "pub(crate)")]
impl Facade {
    #[tool(
        name = "symphony_send_message",
        description = "Deliver a mid-run message (a \"btw\") to a live run's agent (INF-250 mailbox). Proxies POST /api/v1/runs/{id}/message. Defaults run_id to SYMPHONY_RUN_ID. 202 on accept; a not-running or backlog-full run returns an error result."
    )]
    async fn symphony_send_message(
        &self,
        Parameters(args): Parameters<SendMessageArgs>,
    ) -> CallToolResult {
        let id = or_default(&args.run_id, &self.opts.default_run_id);
        if id.is_empty() {
            return err_result(&FacadeError::new(
                "bad_request",
                "no run_id given and SYMPHONY_RUN_ID is not set",
            ));
        }
        if args.text.is_empty() {
            return err_result(&FacadeError::new("empty_text", "message text is required"));
        }
        match self.client.post_message(&id, &args.text).await {
            Ok(body) => text_result(&body),
            Err(e) => err_result(&e),
        }
    }

    #[tool(
        name = "symphony_stop",
        description = "Kill a running agent and move its ticket to Backlog (INF-223). Proxies POST /api/v1/runs/{id}/stop. Defaults run_id to SYMPHONY_RUN_ID."
    )]
    async fn symphony_stop(&self, Parameters(args): Parameters<RunArgs>) -> CallToolResult {
        self.run_action("stop", or_default(&args.run_id, &self.opts.default_run_id))
            .await
    }

    #[tool(
        name = "symphony_resume",
        description = "Resume a stopped run and move its ticket back to Todo (INF-223). Proxies POST /api/v1/runs/{id}/resume. Defaults run_id to SYMPHONY_RUN_ID."
    )]
    async fn symphony_resume(&self, Parameters(args): Parameters<RunArgs>) -> CallToolResult {
        self.run_action(
            "resume",
            or_default(&args.run_id, &self.opts.default_run_id),
        )
        .await
    }
}

impl Facade {
    /// The result-shaping half of writes.go's `runAction`, shared by `symphony_stop` /
    /// `symphony_resume`: an empty id ⇒ `bad_request`, else POST the action ([`crate::client::Client::post_action`])
    /// and surface the daemon body (or its error) as a tool result. Go folds the HTTP call and this
    /// shaping into one `*Client` method; the split keeps the client rmcp-free (M1's layering).
    async fn run_action(&self, action: &str, run_id: String) -> CallToolResult {
        if run_id.is_empty() {
            return err_result(&FacadeError::new(
                "bad_request",
                "no run_id given and SYMPHONY_RUN_ID is not set",
            ));
        }
        match self.client.post_action(action, &run_id).await {
            Ok(body) => text_result(&body),
            Err(e) => err_result(&e),
        }
    }
}

#[cfg(test)]
mod tests {
    //! Mirror of `$REF/internal/mcpfacade/writes_test.go`, driven through an in-memory MCP client
    //! over a tokio duplex (Go's `connectInMemory`) against a small axum stub (Go's `httptest`).
    use super::*;
    use crate::client::Client;
    use crate::server::Options;
    use crate::testutil::{spawn_router, test_config};
    use axum::Router;
    use axum::routing::any;
    use rhapsody_config::Config;
    use rmcp::ServiceExt;
    use rmcp::model::CallToolRequestParams;
    use rmcp::service::RunningService;
    use std::sync::{Arc, Mutex};

    /// Go's `cfgWith`: a config with the three MCP write toggles set explicitly (the rest carry
    /// `decode`'s defaults — irrelevant to gating).
    fn cfg_with(send_message: bool, stop: bool, resume: bool) -> Config {
        let mut c = test_config();
        c.mcp.allow_send_message = send_message;
        c.mcp.allow_stop = stop;
        c.mcp.allow_resume = resume;
        c
    }

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

    /// The visible tool names for a facade (Go's `toolNames`).
    async fn tool_names(facade: Facade) -> Vec<String> {
        let client = connect(facade).await;
        let names = client
            .list_all_tools()
            .await
            .expect("list tools")
            .iter()
            .map(|t| t.name.to_string())
            .collect();
        let _ = client.cancel().await;
        names
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

    fn args(v: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        v.as_object().cloned().unwrap()
    }

    // A disabled write tool is NOT registered (invisible), and an enabled one IS
    // (TestWriteToolsGatedByConfig).
    #[tokio::test]
    async fn write_tools_gated_by_config() {
        let write_tools = ["symphony_send_message", "symphony_stop", "symphony_resume"];

        // All off: no write tools at all.
        let names = tool_names(Facade::new(
            &cfg_with(false, false, false),
            Client::for_port(0),
            Options::default(),
        ))
        .await;
        for n in write_tools {
            assert!(
                !names.contains(&n.to_string()),
                "{n:?} must be absent when disabled: {names:?}"
            );
        }

        // Defaults (send-message on, stop/resume off).
        let names = tool_names(Facade::new(
            &cfg_with(true, false, false),
            Client::for_port(0),
            Options::default(),
        ))
        .await;
        assert!(
            names.contains(&"symphony_send_message".to_string()),
            "symphony_send_message must be present when allow_send_message: {names:?}"
        );
        assert!(
            !names.contains(&"symphony_stop".to_string())
                && !names.contains(&"symphony_resume".to_string()),
            "stop/resume must be absent by default: {names:?}"
        );

        // All on.
        let names = tool_names(Facade::new(
            &cfg_with(true, true, true),
            Client::for_port(0),
            Options::default(),
        ))
        .await;
        for n in write_tools {
            assert!(
                names.contains(&n.to_string()),
                "{n:?} must be present when enabled: {names:?}"
            );
        }
    }

    // symphony_send_message proxies POST /runs/{id}/message with the text body and defaults the run
    // id from SYMPHONY_RUN_ID (TestSendMessageProxies).
    #[tokio::test]
    async fn send_message_proxies() {
        let captured: Arc<Mutex<(String, String)>> =
            Arc::new(Mutex::new((String::new(), String::new())));
        let sink = captured.clone();
        let router = Router::new().fallback(any(move |uri: axum::http::Uri, body: String| {
            let sink = sink.clone();
            async move {
                *sink.lock().unwrap() = (uri.path().to_string(), body);
                (
                    axum::http::StatusCode::ACCEPTED,
                    [("Content-Type", "application/json")],
                    r#"{"id":7,"identifier":"INF-1","status":"sent"}"#,
                )
            }
        }));
        let port = spawn_router(router).await;

        // DefaultRunID stands in for the worker's SYMPHONY_RUN_ID; no explicit run_id is passed.
        let facade = Facade::new(
            &cfg_with(true, false, false),
            Client::for_port(port as i64),
            Options {
                default_run_id: "7".into(),
                ..Default::default()
            },
        );
        let client = connect(facade).await;

        let res = client
            .call_tool(
                call("symphony_send_message").with_arguments(args(serde_json::json!({
                    "text": "hello there"
                }))),
            )
            .await
            .expect("call");
        assert_ne!(
            res.is_error,
            Some(true),
            "unexpected error result: {}",
            result_text(&res)
        );

        let (path, body) = captured.lock().unwrap().clone();
        assert_eq!(path, "/api/v1/runs/7/message");
        let parsed: serde_json::Value = serde_json::from_str(&body).expect("request body JSON");
        assert_eq!(
            parsed["text"], "hello there",
            "request body = {body:?}, want {{text:hello there}}"
        );
        let _ = client.cancel().await;
    }

    // A 409 (not_running) from the daemon surfaces as an error result carrying the envelope code
    // (TestSendMessageConflictSurfacesError).
    #[tokio::test]
    async fn send_message_conflict_surfaces_error() {
        let router = Router::new().fallback(any(|| async {
            (
                axum::http::StatusCode::CONFLICT,
                [("Content-Type", "application/json")],
                r#"{"error":{"code":"not_running","message":"run is not currently running"}}"#,
            )
        }));
        let port = spawn_router(router).await;

        let facade = Facade::new(
            &cfg_with(true, false, false),
            Client::for_port(port as i64),
            Options {
                default_run_id: "7".into(),
                ..Default::default()
            },
        );
        let client = connect(facade).await;

        let res = client
            .call_tool(
                call("symphony_send_message").with_arguments(args(serde_json::json!({
                    "text": "hi"
                }))),
            )
            .await
            .expect("call");
        assert_eq!(res.is_error, Some(true), "want IsError result");
        assert!(
            result_text(&res).contains("not_running"),
            "want not_running in text = {:?}",
            result_text(&res)
        );
        let _ = client.cancel().await;
    }

    // symphony_stop proxies POST /runs/{id}/stop when allow_stop is on (TestStopProxies).
    #[tokio::test]
    async fn stop_proxies() {
        let captured: Arc<Mutex<(String, String)>> =
            Arc::new(Mutex::new((String::new(), String::new())));
        let sink = captured.clone();
        let router = Router::new().fallback(any(
            move |method: axum::http::Method, uri: axum::http::Uri| {
                let sink = sink.clone();
                async move {
                    *sink.lock().unwrap() = (uri.path().to_string(), method.to_string());
                    (
                        [("Content-Type", "application/json")],
                        r#"{"identifier":"INF-1","moved_to":"Backlog"}"#,
                    )
                }
            },
        ));
        let port = spawn_router(router).await;

        let facade = Facade::new(
            &cfg_with(false, true, false),
            Client::for_port(port as i64),
            Options::default(),
        );
        let client = connect(facade).await;

        let res = client
            .call_tool(
                call("symphony_stop").with_arguments(args(serde_json::json!({
                    "run_id": "42"
                }))),
            )
            .await
            .expect("call");
        assert_ne!(
            res.is_error,
            Some(true),
            "unexpected error: {}",
            result_text(&res)
        );

        let (path, method) = captured.lock().unwrap().clone();
        assert_eq!(path, "/api/v1/runs/42/stop");
        assert_eq!(method, "POST");
        let _ = client.cancel().await;
    }
}
