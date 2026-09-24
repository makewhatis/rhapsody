//! manager — the manager run's MCP role and host-served read tools (STUDIO-1014;
//! design record `~/.rhapsody/docs/manager-agent-design.md` §4.4).
//!
//! **No Go v0.4.0 counterpart** — the manager role is a Rhapsody addition.
//!
//! A manager run is started with `rhapsodyd mcp --role manager` (see
//! [`crate::server::Options::role`]). Its tool set is **fixed at startup by the router**: a tool
//! that is not registered is absent from `list_tools` and rejected on call. The set is exactly
//! [`MANAGER_TOOL_NAMES`] — every existing read, the three Teams reads, `teams_retain` (the ONE
//! write), and the host-served `manager_*` reads. The mutation tools
//! (`symphony_send_message`/`_stop`/`_resume`/`_handoff`, `teams_post`/`_invalidate`/`_reinstate`)
//! are NOT registered, regardless of `mcp.allow_*`.
//!
//! # The host serves every read
//!
//! The manager has no checkout, no `gh` and no `git` (§4). The `manager_*` tools below are thin
//! proxies of NEW daemon endpoints under `/api/v1/manager/…`; the daemon — not the run — owns the
//! bare mirror and the off-loop `gh` execution. Each tool passes **only** the run's own id (from
//! `SYMPHONY_RUN_ID`) so the daemon resolves the pull-request coordinate from the RUN, never from a
//! caller-supplied owner/repo — and there is deliberately no `run_id` argument, so a manager can
//! never be pointed at another adjudication.

use crate::client::FacadeError;
use crate::server::{Facade, encode_query, err_result, text_result};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;

/// The exact tool names a manager role registers (§4.4). Any route outside this list is removed in
/// [`Facade::new`](crate::server::Facade::new) when the role is manager.
pub(crate) const MANAGER_TOOL_NAMES: &[&str] = &[
    // Existing always-on reads.
    "symphony_state",
    "symphony_runs",
    "symphony_run",
    "symphony_run_status",
    "symphony_events",
    "symphony_logs",
    "symphony_ticket",
    // Teams reads.
    "teams_recall",
    "teams_room_read",
    "teams_roster",
    // The one write: its own bank, observations only.
    "teams_retain",
    // Host-served reads (§4.4).
    "manager_pr",
    "manager_pr_activity",
    "manager_pr_commits",
    "manager_file",
    "manager_ls",
    "manager_grep",
    "manager_diff",
    "manager_interdiff",
    "manager_patch_id",
    "manager_findings",
];

/// `manager_pr_activity` / `manager_pr_commits` args.
#[derive(Debug, Default, Deserialize, JsonSchema)]
pub(crate) struct ManagerSinceArgs {
    /// only items since this timestamp (RFC3339 / ISO-8601), or a sha for commits.
    #[serde(default)]
    since: String,
}

/// `manager_file` / `manager_ls` args.
#[derive(Debug, Default, Deserialize, JsonSchema)]
pub(crate) struct ManagerPathArgs {
    /// the commit sha to read at.
    sha: String,
    /// the repository-relative path.
    #[serde(default)]
    path: String,
}

/// `manager_grep` args.
#[derive(Debug, Default, Deserialize, JsonSchema)]
pub(crate) struct ManagerGrepArgs {
    /// the commit sha to search at.
    sha: String,
    /// the pattern to search for.
    pattern: String,
    /// restrict the search to this subpath; omit for the whole tree.
    #[serde(default)]
    path: String,
}

/// `manager_diff` / `manager_interdiff` args.
#[derive(Debug, Default, Deserialize, JsonSchema)]
pub(crate) struct ManagerRangeArgs {
    /// the base revision (an ancestor of `to`, or the prior head for an interdiff).
    from: String,
    /// the target revision (the current head).
    to: String,
}

/// `manager_patch_id` args.
#[derive(Debug, Default, Deserialize, JsonSchema)]
pub(crate) struct ManagerShaArgs {
    /// the commit sha whose patch-id (against its merge-base with the PR base) to compute.
    sha: String,
}

/// Reads the manager run id for a call, refusing with the mcp crate's usual `bad_request` envelope
/// when `SYMPHONY_RUN_ID` is not available. Mirrors `teams_retain`'s rule: a manager read is only
/// meaningful for a dispatched run. There is deliberately **no argument** a caller could use to name
/// a different run: the daemon always resolves the coordinate from the manager's OWN run, so a
/// manager can never read another adjudication's pull request (§4.4's `manager_file {sha, path}`
/// has no run id for exactly this reason).
fn manager_run_id(default: &str) -> Result<String, FacadeError> {
    if default.is_empty() {
        return Err(FacadeError::new(
            "bad_request",
            "SYMPHONY_RUN_ID is not set: only a dispatched manager run can serve reads",
        ));
    }
    Ok(default.to_string())
}

#[tool_router(router = manager_router, vis = "pub(crate)")]
impl Facade {
    #[tool(
        name = "manager_pr",
        description = "The pull request a manager run is adjudicating: head, base, state, draft, mergeable, and the checks at head. Served by the host's own off-loop gh. Proxies GET /api/v1/manager/pr. Defaults to your own run via SYMPHONY_RUN_ID."
    )]
    async fn manager_pr(&self) -> CallToolResult {
        let run_id = match manager_run_id(&self.opts.default_run_id) {
            Ok(id) => id,
            Err(e) => return err_result(&e),
        };
        let path = format!(
            "/api/v1/manager/pr{}",
            encode_query(vec![("run_id", run_id)])
        );
        match self.client.get(&path).await {
            Ok(body) => text_result(&body),
            Err(e) => err_result(&e),
        }
    }

    #[tool(
        name = "manager_pr_activity",
        description = "Comments and reviews on the pull request since a timestamp. Served by the host's own off-loop gh. Proxies GET /api/v1/manager/pr/activity."
    )]
    async fn manager_pr_activity(
        &self,
        Parameters(args): Parameters<ManagerSinceArgs>,
    ) -> CallToolResult {
        let run_id = match manager_run_id(&self.opts.default_run_id) {
            Ok(id) => id,
            Err(e) => return err_result(&e),
        };
        let path = format!(
            "/api/v1/manager/pr/activity{}",
            encode_query(vec![("run_id", run_id), ("since", args.since)])
        );
        match self.client.get(&path).await {
            Ok(body) => text_result(&body),
            Err(e) => err_result(&e),
        }
    }

    #[tool(
        name = "manager_pr_commits",
        description = "Commits and their messages on the pull request since a sha. Served by the host's own off-loop gh. Proxies GET /api/v1/manager/pr/commits."
    )]
    async fn manager_pr_commits(
        &self,
        Parameters(args): Parameters<ManagerSinceArgs>,
    ) -> CallToolResult {
        let run_id = match manager_run_id(&self.opts.default_run_id) {
            Ok(id) => id,
            Err(e) => return err_result(&e),
        };
        let path = format!(
            "/api/v1/manager/pr/commits{}",
            encode_query(vec![("run_id", run_id), ("since", args.since)])
        );
        match self.client.get(&path).await {
            Ok(body) => text_result(&body),
            Err(e) => err_result(&e),
        }
    }

    #[tool(
        name = "manager_file",
        description = "Read one file's content at a commit sha, from the repository's git objects — no checkout runs. A symlink is returned as its blob text and is never followed. Proxies GET /api/v1/manager/file."
    )]
    async fn manager_file(&self, Parameters(args): Parameters<ManagerPathArgs>) -> CallToolResult {
        let run_id = match manager_run_id(&self.opts.default_run_id) {
            Ok(id) => id,
            Err(e) => return err_result(&e),
        };
        let path = format!(
            "/api/v1/manager/file{}",
            encode_query(vec![
                ("run_id", run_id),
                ("sha", args.sha),
                ("path", args.path),
            ])
        );
        match self.client.get(&path).await {
            Ok(body) => text_result(&body),
            Err(e) => err_result(&e),
        }
    }

    #[tool(
        name = "manager_ls",
        description = "List a directory tree at a commit sha, from the repository's git objects. Proxies GET /api/v1/manager/ls."
    )]
    async fn manager_ls(&self, Parameters(args): Parameters<ManagerPathArgs>) -> CallToolResult {
        let run_id = match manager_run_id(&self.opts.default_run_id) {
            Ok(id) => id,
            Err(e) => return err_result(&e),
        };
        let path = format!(
            "/api/v1/manager/ls{}",
            encode_query(vec![
                ("run_id", run_id),
                ("sha", args.sha),
                ("path", args.path),
            ])
        );
        match self.client.get(&path).await {
            Ok(body) => text_result(&body),
            Err(e) => err_result(&e),
        }
    }

    #[tool(
        name = "manager_grep",
        description = "Search the repository's git objects at a commit sha for a pattern. Proxies GET /api/v1/manager/grep."
    )]
    async fn manager_grep(&self, Parameters(args): Parameters<ManagerGrepArgs>) -> CallToolResult {
        let run_id = match manager_run_id(&self.opts.default_run_id) {
            Ok(id) => id,
            Err(e) => return err_result(&e),
        };
        let path = format!(
            "/api/v1/manager/grep{}",
            encode_query(vec![
                ("run_id", run_id),
                ("sha", args.sha),
                ("pattern", args.pattern),
                ("path", args.path),
            ])
        );
        match self.client.get(&path).await {
            Ok(body) => text_result(&body),
            Err(e) => err_result(&e),
        }
    }

    #[tool(
        name = "manager_diff",
        description = "The diff between two revisions (from..to), served from the host's git. Every diff served is recorded in the evidence-access log. Proxies GET /api/v1/manager/diff."
    )]
    async fn manager_diff(&self, Parameters(args): Parameters<ManagerRangeArgs>) -> CallToolResult {
        let run_id = match manager_run_id(&self.opts.default_run_id) {
            Ok(id) => id,
            Err(e) => return err_result(&e),
        };
        let path = format!(
            "/api/v1/manager/diff{}",
            encode_query(vec![
                ("run_id", run_id),
                ("from", args.from),
                ("to", args.to),
            ])
        );
        match self.client.get(&path).await {
            Ok(body) => text_result(&body),
            Err(e) => err_result(&e),
        }
    }

    #[tool(
        name = "manager_interdiff",
        description = "The difference between two pull-request patches (from's patch vs to's patch), like `git range-diff`. Used after a rebase or force-push, where `from` is not an ancestor of `to`. Proxies GET /api/v1/manager/interdiff."
    )]
    async fn manager_interdiff(
        &self,
        Parameters(args): Parameters<ManagerRangeArgs>,
    ) -> CallToolResult {
        let run_id = match manager_run_id(&self.opts.default_run_id) {
            Ok(id) => id,
            Err(e) => return err_result(&e),
        };
        let path = format!(
            "/api/v1/manager/interdiff{}",
            encode_query(vec![
                ("run_id", run_id),
                ("from", args.from),
                ("to", args.to),
            ])
        );
        match self.client.get(&path).await {
            Ok(body) => text_result(&body),
            Err(e) => err_result(&e),
        }
    }

    #[tool(
        name = "manager_patch_id",
        description = "A stable patch-id for a commit, computed over merge-base(base, sha)..sha — content, not commit identity. Proxies GET /api/v1/manager/patch-id."
    )]
    async fn manager_patch_id(
        &self,
        Parameters(args): Parameters<ManagerShaArgs>,
    ) -> CallToolResult {
        let run_id = match manager_run_id(&self.opts.default_run_id) {
            Ok(id) => id,
            Err(e) => return err_result(&e),
        };
        let path = format!(
            "/api/v1/manager/patch-id{}",
            encode_query(vec![("run_id", run_id), ("sha", args.sha)])
        );
        match self.client.get(&path).await {
            Ok(body) => text_result(&body),
            Err(e) => err_result(&e),
        }
    }

    #[tool(
        name = "manager_findings",
        description = "The structured findings recorded for this pull request, identified per revision. Proxies GET /api/v1/manager/findings."
    )]
    async fn manager_findings(&self) -> CallToolResult {
        let run_id = match manager_run_id(&self.opts.default_run_id) {
            Ok(id) => id,
            Err(e) => return err_result(&e),
        };
        let path = format!(
            "/api/v1/manager/findings{}",
            encode_query(vec![("run_id", run_id)])
        );
        match self.client.get(&path).await {
            Ok(body) => text_result(&body),
            Err(e) => err_result(&e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::{Options, Role};
    use crate::testutil::{client_for_port, spawn_router, test_config};
    use axum::Router;
    use axum::routing::get;
    use rmcp::ServiceExt;
    use rmcp::model::{CallToolRequestParams, CallToolResult};
    use rmcp::service::RunningService;

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

    fn manager_options() -> Options {
        Options {
            role: Role::Manager,
            default_run_id: "42".to_string(),
            ..Options::default()
        }
    }

    // The manager role registers EXACTLY the fixed tool set — no more, no less. This is the
    // "a tool that isn't registered is absent" contract (§4.4).
    #[tokio::test]
    async fn manager_role_registers_exactly_the_fixed_set() {
        let facade = Facade::new(&test_config(), client_for_port(0), manager_options());
        let client = connect(facade).await;
        let tools = client.list_all_tools().await.expect("list tools");
        let mut names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
        names.sort_unstable();
        let mut want: Vec<&str> = MANAGER_TOOL_NAMES.to_vec();
        want.sort_unstable();
        assert_eq!(names, want);
        let _ = client.cancel().await;
    }

    // The unregistered mutation tools are absent and rejected on call.
    #[tokio::test]
    async fn manager_role_rejects_unregistered_write_tools() {
        let facade = Facade::new(&test_config(), client_for_port(0), manager_options());
        let client = connect(facade).await;
        let tools = client.list_all_tools().await.expect("list tools");
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
        for forbidden in [
            "symphony_send_message",
            "symphony_stop",
            "symphony_resume",
            "symphony_handoff",
            "teams_post",
            "teams_invalidate",
            "teams_reinstate",
        ] {
            assert!(
                !names.contains(&forbidden),
                "{forbidden} must not be registered for the manager role: {names:?}"
            );
        }
        for forbidden in ["symphony_stop", "teams_post", "teams_invalidate"] {
            assert!(
                client
                    .call_tool(CallToolRequestParams::new(forbidden.to_string()))
                    .await
                    .is_err(),
                "calling unregistered {forbidden} must be rejected"
            );
        }
        let _ = client.cancel().await;
    }

    // A manager read proxies the host endpoint with the run id defaulted from SYMPHONY_RUN_ID.
    #[tokio::test]
    async fn manager_pr_proxies_host_endpoint() {
        let router = Router::new().route(
            "/api/v1/manager/pr",
            get(|uri: axum::http::Uri| async move { format!("got {}", uri.query().unwrap_or("")) }),
        );
        let port = spawn_router(router).await;
        let facade = Facade::new(&test_config(), client_for_port(port), manager_options());
        let client = connect(facade).await;
        let res = client
            .call_tool(CallToolRequestParams::new("manager_pr"))
            .await
            .expect("call manager_pr");
        assert_eq!(result_text(&res), "got run_id=42");
        let _ = client.cancel().await;
    }

    // The declared args are passed through; the run id is ALWAYS the manager's own (SYMPHONY_RUN_ID).
    #[tokio::test]
    async fn manager_file_passes_declared_args_and_its_own_run_id() {
        let router = Router::new().route(
            "/api/v1/manager/file",
            get(|uri: axum::http::Uri| async move { format!("got {}", uri.query().unwrap_or("")) }),
        );
        let port = spawn_router(router).await;
        let facade = Facade::new(&test_config(), client_for_port(port), manager_options());
        let client = connect(facade).await;
        let mut req = CallToolRequestParams::new("manager_file");
        req.arguments = serde_json::json!({"sha": "abc123", "path": "src/lib.rs"})
            .as_object()
            .cloned();
        let res = client.call_tool(req).await.expect("call manager_file");
        let text = result_text(&res);
        assert!(text.contains("sha=abc123"), "{text}");
        assert!(text.contains("path=src%2Flib.rs"), "{text}");
        assert!(text.contains("run_id=42"), "{text}");
        let _ = client.cancel().await;
    }

    // A manager read cannot be pointed at another run: a caller-supplied `run_id` is ignored and
    // the request still carries the manager's OWN run id.
    #[tokio::test]
    async fn manager_read_ignores_a_caller_supplied_run_id() {
        let router = Router::new().route(
            "/api/v1/manager/file",
            get(|uri: axum::http::Uri| async move { format!("got {}", uri.query().unwrap_or("")) }),
        );
        let port = spawn_router(router).await;
        let facade = Facade::new(&test_config(), client_for_port(port), manager_options());
        let client = connect(facade).await;
        let mut req = CallToolRequestParams::new("manager_file");
        req.arguments = serde_json::json!({"sha": "abc123", "run_id": "999"})
            .as_object()
            .cloned();
        let res = client.call_tool(req).await.expect("call manager_file");
        let text = result_text(&res);
        assert!(
            text.contains("run_id=42"),
            "the manager's own run id must win over a caller-supplied one: {text}"
        );
        assert!(!text.contains("run_id=999"), "{text}");
        let _ = client.cancel().await;
    }

    // Without SYMPHONY_RUN_ID a manager read refuses, like teams_retain.
    #[tokio::test]
    async fn manager_read_requires_a_run_id() {
        let facade = Facade::new(
            &test_config(),
            client_for_port(0),
            Options {
                role: Role::Manager,
                ..Options::default()
            },
        );
        let client = connect(facade).await;
        let res = client
            .call_tool(CallToolRequestParams::new("manager_pr"))
            .await
            .expect("call");
        assert!(
            result_text(&res).contains("bad_request"),
            "{}",
            result_text(&res)
        );
        let _ = client.cancel().await;
    }
}
