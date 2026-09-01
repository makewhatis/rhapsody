//! teams — the Rhapsody Teams MCP tools (STUDIO-645, slice T4; design record
//! `~/.rhapsody/docs/STUDIO-572-rhapsody-teams.md`, §0.11.7, §6.7).
//!
//! **No Go v0.4.0 counterpart** — the Go facade has no analog, exactly as
//! `symphony_handoff` has none. Seven tools, each a thin proxy of a NEW additive
//! daemon endpoint:
//!
//! | Tool | Reads/Writes | Endpoint |
//! |---|---|---|
//! | `teams_roster` | read | `GET /api/v1/teams/roster` |
//! | `teams_recall {identity, query}` | read | `GET /api/v1/teams/recall` |
//! | `teams_invalidate {identity, fact_id, reason}` | write | `POST /api/v1/teams/invalidate` |
//! | `teams_reinstate {identity, fact_id}` | write | `POST /api/v1/teams/reinstate` (STUDIO-689) |
//! | `teams_retain {content}` | write | `POST /api/v1/runs/{id}/retain` |
//! | `teams_room_read {limit?}` | read | `GET /api/v1/teams/room` (STUDIO-650, T5) |
//! | `teams_post {body, to?, refs?}` | write | `POST /api/v1/runs/{id}/post` (STUDIO-653, T6) |
//!
//! `teams_retain` is **this slice's addition to §6.7's table** — §6.7 listed
//! `teams_roster` / `teams_recall` / `teams_invalidate` but not the retain half
//! of §5.1, which has no other home: the design's retain is "authored by the
//! agent at end of run", so the agent needs a tool to author it with.
//!
//! # Off ⇒ invisible, not merely inert (§6.7, §2.4 row 7)
//!
//! When Teams is off, [`Facade::new`] REMOVES all seven routes, so `list_tools`
//! is byte-identical to a daemon built before Teams existed. That is the
//! `allow_handoff` mechanism, reused unchanged — the gate is the enabled-tool
//! set, so a disabled tool is absent rather than surfacing a runtime
//! permission-denied.
//!
//! # `teams_room_read` never advances a cursor (STUDIO-650, T5)
//!
//! The room's watermarks belong to **hydration**: the composer earns one from what it actually
//! rendered into a turn-1 prompt. A tool read that advanced a cursor would let a mid-run peek eat
//! another run's catch-up and silently hide a hand-off from the teammate it was addressed to — so
//! this tool reads the newest bounded window every time and moves nothing. That is also why it
//! takes no `identity`: a peek is not any identity, so it sees room-audience posts and no direct
//! ones. §6.7's table gains it with a T5 home (§0.11.7).
//!
//! # `teams_post` cannot forge its author either (STUDIO-653, T6)
//!
//! **There is no `from` argument, and there is no way to add one** (§0.11.4: "`from` is stamped by
//! the host … a run cannot supply it"). The run id comes from `SYMPHONY_RUN_ID` exactly as
//! `teams_retain`'s does, the daemon resolves that run to the identity it was dispatched as, and a
//! run wearing no identity cannot post at all. `to` names a teammate — validated against the roster
//! on the far side, where an unknown name is a loud `bad_request` rather than a silent downgrade to
//! a room post, because a message its author believed was private must never quietly become public.
//!
//! The tool is a **proxy and nothing more**: it never opens the room log. The daemon stays the
//! single writer (§0.11.4), which is what makes the concurrent-append problem dissolve rather than
//! needing a lock. And a post has no dispatch power whatever (§0.2) — it starts no run, writes no
//! label and touches no tracker, however it is addressed.
//!
//! # `teams_retain` cannot forge provenance
//!
//! The tool takes `content` and nothing else. The run id comes from
//! `SYMPHONY_RUN_ID` — the env the daemon itself injected into this worker — and
//! the daemon resolves that run to its identity, ticket and commit on the far
//! side (§5.1). There is deliberately **no `identity` argument**: a tool that
//! accepted one would let a run dispatched as `bob` write into `alice`'s bank,
//! which is the forgery §0.11.4 rules out for the room's `from` and which
//! applies with equal force here.

use crate::client::FacadeError;
use crate::server::{Facade, err_result, or_default, path_escape, text_result};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// `teams_recall` args. Both carry no `omitempty` analog: `identity` is required
/// by the daemon and an empty `query` is a legitimate "everything you remember,
/// bounded by `recall_top_k`".
#[derive(Debug, Default, Deserialize, JsonSchema)]
pub(crate) struct RecallArgs {
    /// the teammate whose memory to read (a name from `teams_roster`).
    identity: String,
    /// what to look for — a ticket identifier, a subject, or a few keywords.
    #[serde(default)]
    query: String,
}

/// `teams_invalidate` args (§5.3). `reason` is REQUIRED, and deliberately so:
/// the reason is the thing a correction is worth, and the Go
/// `studiomemory.Invalidate` client's measured 400 came from omitting it.
#[derive(Debug, Default, Deserialize, JsonSchema)]
pub(crate) struct InvalidateArgs {
    /// the teammate whose bank holds the record.
    identity: String,
    /// the record id, as `teams_recall` reports it in each fact's `id`.
    fact_id: String,
    /// why this fact is no longer true. Stored with the record and reversible.
    reason: String,
}

/// `teams_reinstate` args (§5.3's reversal, STUDIO-689). There is deliberately no `reason`: a
/// correction has to be justified, undoing one restores the original and justifies nothing new.
#[derive(Debug, Default, Deserialize, JsonSchema)]
pub(crate) struct ReinstateArgs {
    /// the teammate whose bank holds the record.
    identity: String,
    /// the record id, as `teams_recall` reports it in each fact's `id`.
    fact_id: String,
}

/// `teams_room_read` args. `limit` is optional and can only ever NARROW: the daemon clamps it to
/// the room's own ceiling, so no caller can widen the window §0.5 calls non-negotiable.
#[derive(Debug, Default, Deserialize, JsonSchema)]
pub(crate) struct RoomReadArgs {
    /// how many of the newest posts to return; omit for the default window.
    #[serde(default)]
    limit: u32,
}

/// `teams_post` args (STUDIO-653, T6). Note what is NOT here: no `from`, no
/// `identity`, no `run_id`. The author is resolved by the daemon from
/// `SYMPHONY_RUN_ID`, and `retain_declares_only_content`'s sibling test pins
/// this schema so a provenance argument cannot be added by accident.
#[derive(Debug, Default, Deserialize, JsonSchema)]
pub(crate) struct PostArgs {
    /// what you want the team to know, in your own words.
    body: String,
    /// a teammate's name (from `teams_roster`) to address it to; omit, or use
    /// `*`, for the whole room.
    #[serde(default)]
    to: String,
    /// ticket ids, PR urls or commit SHAs that back it up.
    #[serde(default)]
    refs: Vec<String>,
}

/// `teams_retain` args — `content` and nothing else, by design (module docs).
#[derive(Debug, Default, Deserialize, JsonSchema)]
pub(crate) struct RetainArgs {
    /// what you learned, in your own words: observations and outcomes only.
    content: String,
}

/// The `POST /api/v1/teams/invalidate` body.
#[derive(Serialize)]
struct InvalidateBody<'a> {
    identity: &'a str,
    fact_id: &'a str,
    reason: &'a str,
}

/// The `POST /api/v1/teams/reinstate` body.
#[derive(Serialize)]
struct ReinstateBody<'a> {
    identity: &'a str,
    fact_id: &'a str,
}

/// The `POST /api/v1/runs/{id}/retain` body.
#[derive(Serialize)]
struct RetainBody<'a> {
    content: &'a str,
}

/// The `POST /api/v1/runs/{id}/post` body. Carries exactly the three declared
/// arguments — the run id travels in the PATH, which is what makes `from`
/// unforgeable.
#[derive(Serialize)]
struct PostBody<'a> {
    body: &'a str,
    to: &'a str,
    refs: &'a [String],
}

#[tool_router(router = teams_router, vis = "pub(crate)")]
impl Facade {
    #[tool(
        name = "teams_roster",
        description = "Who is on this Rhapsody team: each identity's name, the profile it wears, its matching labels, its memory bank id, and the runs live as it right now. Proxies GET /api/v1/teams/roster. Only present when Teams is enabled."
    )]
    async fn teams_roster(&self) -> CallToolResult {
        match self.client.get("/api/v1/teams/roster").await {
            Ok(body) => text_result(&body),
            Err(e) => err_result(&e),
        }
    }

    #[tool(
        name = "teams_recall",
        description = "Read a teammate's retained memory: past observations and outcomes matching your query, bounded by memory.recall_top_k. Each fact carries the ticket, run and commit it came from, so you can re-ground it yourself. Costs no model turn. Proxies GET /api/v1/teams/recall."
    )]
    async fn teams_recall(&self, Parameters(args): Parameters<RecallArgs>) -> CallToolResult {
        if args.identity.is_empty() {
            return err_result(&FacadeError::new(
                "bad_request",
                "identity is required (see teams_roster for the names)",
            ));
        }
        let path = format!(
            "/api/v1/teams/recall?identity={}&query={}",
            query_escape(&args.identity),
            query_escape(&args.query)
        );
        match self.client.get(&path).await {
            Ok(body) => text_result(&body),
            Err(e) => err_result(&e),
        }
    }

    #[tool(
        name = "teams_invalidate",
        description = "Mark one retained fact as no longer true, with the reason why. The record is NOT deleted — its content and your reason stay on disk and the change is reversible — but it stops being recalled into anyone's prompt. Use this the moment you find a remembered fact contradicted, rather than retaining a correction on top of it. Proxies POST /api/v1/teams/invalidate."
    )]
    async fn teams_invalidate(
        &self,
        Parameters(args): Parameters<InvalidateArgs>,
    ) -> CallToolResult {
        if args.identity.is_empty() || args.fact_id.is_empty() {
            return err_result(&FacadeError::new(
                "bad_request",
                "identity and fact_id are required (see teams_recall for a fact's id)",
            ));
        }
        if args.reason.trim().is_empty() {
            return err_result(&FacadeError::new(
                "empty_reason",
                "reason is required: an invalidation with no reason cannot be judged later",
            ));
        }
        let payload = match serde_json::to_vec(&InvalidateBody {
            identity: &args.identity,
            fact_id: &args.fact_id,
            reason: &args.reason,
        }) {
            Ok(p) => p,
            Err(e) => return err_result(&FacadeError::new("encode_error", e.to_string())),
        };
        match self
            .client
            .post_json("/api/v1/teams/invalidate", payload)
            .await
        {
            Ok(body) => text_result(&body),
            Err(e) => err_result(&e),
        }
    }

    #[tool(
        name = "teams_reinstate",
        description = "Undo an invalidation: put one corrected fact back into recall, exactly as it was. Nothing was ever deleted, so this restores the record and drops the reason it was invalidated for. Use it when the correction itself turns out to be the wrong call. Proxies POST /api/v1/teams/reinstate."
    )]
    async fn teams_reinstate(&self, Parameters(args): Parameters<ReinstateArgs>) -> CallToolResult {
        if args.identity.is_empty() || args.fact_id.is_empty() {
            return err_result(&FacadeError::new(
                "bad_request",
                "identity and fact_id are required (see teams_recall for a fact's id)",
            ));
        }
        let payload = match serde_json::to_vec(&ReinstateBody {
            identity: &args.identity,
            fact_id: &args.fact_id,
        }) {
            Ok(p) => p,
            Err(e) => return err_result(&FacadeError::new("encode_error", e.to_string())),
        };
        match self
            .client
            .post_json("/api/v1/teams/reinstate", payload)
            .await
        {
            Ok(body) => text_result(&body),
            Err(e) => err_result(&e),
        }
    }

    #[tool(
        name = "teams_room_read",
        description = "Read the newest posts in the team room: the manager's routing decisions and teammates' hand-offs, oldest first, bounded. This is the same log your turn-1 prompt caught you up on, so use it to look FURTHER back or to re-check something mid-run. Read-only — it never advances your catch-up watermark, so nothing you read here is hidden from your next run. Proxies GET /api/v1/teams/room."
    )]
    async fn teams_room_read(&self, Parameters(args): Parameters<RoomReadArgs>) -> CallToolResult {
        let path = format!("/api/v1/teams/room?limit={}", args.limit);
        match self.client.get(&path).await {
            Ok(body) => text_result(&body),
            Err(e) => err_result(&e),
        }
    }

    #[tool(
        name = "teams_retain",
        description = "Record what THIS run learned, in your own words, into your teammate memory — observations and outcomes only, never a transcript and never a conclusion you did not verify. Rhapsody stamps the identity, ticket, run and commit itself from your run, so you supply only the prose. Best-effort: a failure never fails the run. Proxies POST /api/v1/runs/{id}/retain for SYMPHONY_RUN_ID."
    )]
    async fn teams_retain(&self, Parameters(args): Parameters<RetainArgs>) -> CallToolResult {
        // There is no `run_id` argument: the run is the one the daemon injected
        // into this worker's env, which is what makes the provenance the daemon
        // stamps unforgeable (module docs, §5.1).
        let id = or_default("", &self.opts.default_run_id);
        if id.is_empty() {
            return err_result(&FacadeError::new(
                "bad_request",
                "SYMPHONY_RUN_ID is not set: only a dispatched run can retain a memory",
            ));
        }
        if args.content.trim().is_empty() {
            return err_result(&FacadeError::new(
                "empty_content",
                "content is required: say what you learned",
            ));
        }
        let payload = match serde_json::to_vec(&RetainBody {
            content: &args.content,
        }) {
            Ok(p) => p,
            Err(e) => return err_result(&FacadeError::new("encode_error", e.to_string())),
        };
        match self
            .client
            .post_json(
                &format!("/api/v1/runs/{}/retain", path_escape(&id)),
                payload,
            )
            .await
        {
            Ok(body) => text_result(&body),
            Err(e) => err_result(&e),
        }
    }

    #[tool(
        name = "teams_post",
        description = "Say something to your team: ask a question, hand off a decision, or flag what you found. Omit `to` (or use `*`) to post to the whole room; set `to` to a teammate's name from teams_roster to address them directly. Rhapsody stamps WHO you are from your run — you cannot post as anyone else. A teammate who is running right now also gets it in-turn, clearly marked as coming from you rather than from the operator; one who is not running reads it when they next start. Posting never starts a run, never assigns a ticket and never changes anything in Linear — the room is for talking, not for dispatching work. Proxies POST /api/v1/runs/{id}/post for SYMPHONY_RUN_ID."
    )]
    async fn teams_post(&self, Parameters(args): Parameters<PostArgs>) -> CallToolResult {
        // As with `teams_retain`: no `run_id` argument, so the identity the daemon stamps is the
        // one it dispatched this worker with (module docs, §0.11.4).
        let id = or_default("", &self.opts.default_run_id);
        if id.is_empty() {
            return err_result(&FacadeError::new(
                "bad_request",
                "SYMPHONY_RUN_ID is not set: only a dispatched run can post to the team room",
            ));
        }
        if args.body.trim().is_empty() {
            return err_result(&FacadeError::new(
                "empty_body",
                "body is required: say what you want the team to know",
            ));
        }
        let payload = match serde_json::to_vec(&PostBody {
            body: &args.body,
            to: &args.to,
            refs: &args.refs,
        }) {
            Ok(p) => p,
            Err(e) => return err_result(&FacadeError::new("encode_error", e.to_string())),
        };
        match self
            .client
            .post_json(&format!("/api/v1/runs/{}/post", path_escape(&id)), payload)
            .await
        {
            Ok(body) => text_result(&body),
            Err(e) => err_result(&e),
        }
    }
}

/// Query-escapes one component, the same rule [`crate::server`]'s `encode_query`
/// applies. Kept here rather than exported because these two tools build their
/// query strings directly (both parameters are always present, so the
/// drop-empty/sort behaviour of `encode_query` would be wrong for `query=`).
fn query_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else if b == b' ' {
            out.push('+');
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    //! Driven through an in-memory MCP client over a tokio duplex (Go's
    //! `connectInMemory`) against a small axum stub, exactly as `writes.rs`
    //! drives the `mcp:`-gated write tools.
    use super::*;
    use crate::client::Client;
    use crate::server::Options;
    use crate::testutil::{spawn_router, test_config};
    use axum::Router;
    use axum::routing::any;
    use rmcp::ServiceExt;
    use rmcp::model::CallToolRequestParams;
    use rmcp::service::RunningService;
    use std::sync::{Arc, Mutex};

    type SeenLog = Arc<Mutex<Vec<(String, String)>>>;

    fn teams_on() -> Options {
        Options {
            teams_enabled: true,
            ..Options::default()
        }
    }

    fn facade(opts: Options, port: u16) -> Facade {
        Facade::new(&test_config(), Client::for_port(port as i64), opts)
    }

    async fn connect(facade: Facade) -> RunningService<rmcp::RoleClient, ()> {
        let (client_t, server_t) = tokio::io::duplex(1 << 16);
        tokio::spawn(async move {
            if let Ok(server) = facade.serve(server_t).await {
                let _ = server.waiting().await;
            }
        });
        ().serve(client_t).await.expect("client connect")
    }

    async fn tool_names(facade: Facade) -> Vec<String> {
        let client = connect(facade).await;
        let mut names: Vec<String> = client
            .list_all_tools()
            .await
            .expect("list tools")
            .iter()
            .map(|t| t.name.to_string())
            .collect();
        let _ = client.cancel().await;
        names.sort();
        names
    }

    /// A stub daemon that records every `(method, path-and-query)` it is asked
    /// for and answers `{"ok":true}`.
    async fn stub_daemon() -> (u16, SeenLog) {
        let seen: SeenLog = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let router = Router::new().fallback(any(
            move |method: axum::http::Method, uri: axum::http::Uri| {
                let sink = Arc::clone(&sink);
                async move {
                    sink.lock()
                        .expect("seen lock")
                        .push((method.to_string(), uri.to_string()));
                    (
                        axum::http::StatusCode::OK,
                        [("Content-Type", "application/json")],
                        r#"{"ok":true}"#,
                    )
                }
            },
        ));
        (spawn_router(router).await, seen)
    }

    async fn call_text(
        client: &RunningService<rmcp::RoleClient, ()>,
        name: &str,
        args: serde_json::Value,
    ) -> String {
        let res = client
            .call_tool(
                CallToolRequestParams::new(name.to_string())
                    .with_arguments(args.as_object().cloned().unwrap_or_default()),
            )
            .await
            .expect("call tool");
        res.content
            .iter()
            .filter_map(|c| c.as_text())
            .map(|t| t.text.as_str())
            .collect()
    }

    /// **§6.7 / §2.4 row 7: off ⇒ invisible.** With Teams disabled not one
    /// `teams_*` tool is registered, so `list_tools` is byte-identical to a
    /// daemon built before Teams existed. The gate is the enabled-tool set —
    /// `allow_handoff`'s mechanism, reused unchanged.
    #[tokio::test]
    async fn teams_off_registers_no_teams_tools() {
        let names = tool_names(facade(Options::default(), 0)).await;
        assert!(
            !names.iter().any(|n| n.starts_with("teams_")),
            "teams off must register NO teams_* tool: {names:?}"
        );
    }

    /// Turning Teams on ADDS exactly the §0.11.7 + §6.7 tools and changes nothing
    /// else — the precise statement of "byte-identical when off", checked from
    /// both directions so a future tool cannot slip in unnoticed.
    ///
    /// STUDIO-650 (T5) extended this list with `teams_room_read`, STUDIO-653
    /// (T6) with `teams_post` and STUDIO-689 with `teams_reinstate`; the
    /// assertion is unchanged in kind, only in the set it names.
    #[tokio::test]
    async fn enabling_teams_only_adds_the_teams_tools() {
        let off = tool_names(facade(Options::default(), 0)).await;
        let on = tool_names(facade(teams_on(), 0)).await;

        let added: Vec<String> = on.iter().filter(|n| !off.contains(n)).cloned().collect();
        let removed: Vec<String> = off.iter().filter(|n| !on.contains(n)).cloned().collect();
        assert_eq!(
            added,
            vec![
                "teams_invalidate",
                "teams_post",
                "teams_recall",
                "teams_reinstate",
                "teams_retain",
                "teams_room_read",
                "teams_roster"
            ]
        );
        assert!(removed.is_empty(), "enabling teams removed {removed:?}");
        // The pre-Teams surface is still all there.
        for expected in ["symphony_state", "symphony_runs", "symphony_handoff"] {
            assert!(on.contains(&expected.to_string()), "{on:?}");
        }
    }

    /// `teams_retain` declares `content` and NOTHING else. The input schema is
    /// the contract an agent reads, so this is where "the agent cannot forge
    /// provenance" is checkable rather than merely intended (§5.1, §0.11.4).
    #[tokio::test]
    async fn retain_declares_only_content() {
        let client = connect(facade(teams_on(), 0)).await;
        let tools = client.list_all_tools().await.expect("list tools");
        let retain = tools
            .iter()
            .find(|t| t.name == "teams_retain")
            .expect("teams_retain is registered");
        let props = retain
            .input_schema
            .get("properties")
            .and_then(|p| p.as_object())
            .expect("an object schema");
        let mut keys: Vec<&String> = props.keys().collect();
        keys.sort();
        assert_eq!(
            keys,
            vec!["content"],
            "teams_retain must expose no provenance argument"
        );
        let _ = client.cancel().await;
    }

    /// The retain path end to end: the tool posts to the run named by
    /// SYMPHONY_RUN_ID — never a run id the caller supplied.
    #[tokio::test]
    async fn retain_posts_to_the_run_from_the_env() {
        let (port, seen) = stub_daemon().await;
        let opts = Options {
            default_run_id: "412".to_string(),
            ..teams_on()
        };
        let client = connect(facade(opts, port)).await;

        let out = call_text(
            &client,
            "teams_retain",
            serde_json::json!({"content": "learned"}),
        )
        .await;
        assert!(out.contains("\"ok\""), "out = {out}");
        assert_eq!(
            seen.lock().expect("seen lock").clone(),
            vec![("POST".to_string(), "/api/v1/runs/412/retain".to_string())]
        );
        let _ = client.cancel().await;
    }

    /// A coordinator session (no SYMPHONY_RUN_ID) cannot retain: there is no run
    /// to attribute the record to, and the tool refuses rather than guessing.
    #[tokio::test]
    async fn retain_without_a_run_id_is_refused() {
        let (port, seen) = stub_daemon().await;
        let client = connect(facade(teams_on(), port)).await;
        let out = call_text(&client, "teams_retain", serde_json::json!({"content": "x"})).await;
        assert!(out.contains("SYMPHONY_RUN_ID"), "out = {out}");
        assert!(
            seen.lock().expect("seen lock").is_empty(),
            "an unattributable retain must not reach the daemon"
        );
        let _ = client.cancel().await;
    }

    /// An invalidation with no reason never leaves the facade: §5.3 stores the
    /// reason, and the Go `studiomemory.Invalidate` client's measured 400 came
    /// from omitting exactly this.
    #[tokio::test]
    async fn invalidate_without_a_reason_never_leaves_the_facade() {
        let (port, seen) = stub_daemon().await;
        let client = connect(facade(teams_on(), port)).await;
        let out = call_text(
            &client,
            "teams_invalidate",
            serde_json::json!({"identity": "alice", "fact_id": "f1", "reason": "  "}),
        )
        .await;
        assert!(out.contains("empty_reason"), "out = {out}");
        assert!(
            seen.lock().expect("seen lock").is_empty(),
            "a reasonless invalidate must not reach the daemon"
        );
        let _ = client.cancel().await;
    }

    /// `teams_reinstate` proxies §5.3's reversal, and refuses an incomplete one
    /// before it reaches the daemon — the same door `teams_invalidate` keeps.
    #[tokio::test]
    async fn reinstate_proxies_its_endpoint_and_refuses_an_incomplete_one() {
        let (port, seen) = stub_daemon().await;
        let client = connect(facade(teams_on(), port)).await;

        let out = call_text(
            &client,
            "teams_reinstate",
            serde_json::json!({"identity": "alice", "fact_id": ""}),
        )
        .await;
        assert!(out.contains("bad_request"), "out = {out}");
        assert!(
            seen.lock().expect("seen lock").is_empty(),
            "an incomplete reinstate must not reach the daemon"
        );

        let out = call_text(
            &client,
            "teams_reinstate",
            serde_json::json!({"identity": "alice", "fact_id": "20260101T000000Z-run-1"}),
        )
        .await;
        assert!(out.contains("\"ok\""), "out = {out}");
        assert_eq!(
            seen.lock().expect("seen lock").clone(),
            vec![("POST".to_string(), "/api/v1/teams/reinstate".to_string())]
        );
        let _ = client.cancel().await;
    }

    /// `teams_recall` escapes both parameters, so an identity or query carrying
    /// reserved characters cannot alter the request it builds.
    #[tokio::test]
    async fn recall_escapes_both_parameters() {
        let (port, seen) = stub_daemon().await;
        let client = connect(facade(teams_on(), port)).await;
        call_text(
            &client,
            "teams_recall",
            serde_json::json!({"identity": "alice", "query": "a&b c"}),
        )
        .await;
        assert_eq!(
            seen.lock().expect("seen lock").clone(),
            vec![(
                "GET".to_string(),
                "/api/v1/teams/recall?identity=alice&query=a%26b+c".to_string()
            )]
        );
        let _ = client.cancel().await;
    }

    /// `teams_room_read` proxies its endpoint and passes the limit straight through — the daemon
    /// owns the clamp, so the tool cannot widen the window and cannot disagree with the ceiling.
    #[tokio::test]
    async fn room_read_proxies_its_endpoint_with_the_limit() {
        let (port, seen) = stub_daemon().await;
        let client = connect(facade(teams_on(), port)).await;

        let out = call_text(&client, "teams_room_read", serde_json::json!({"limit": 5})).await;
        assert!(out.contains("\"ok\""), "out = {out}");
        // An omitted limit is `0`, which the daemon reads as "the default window" — the same
        // non-positive-means-fallback rule `recall_top_k` follows.
        call_text(&client, "teams_room_read", serde_json::json!({})).await;
        assert_eq!(
            seen.lock().expect("seen lock").clone(),
            vec![
                ("GET".to_string(), "/api/v1/teams/room?limit=5".to_string()),
                ("GET".to_string(), "/api/v1/teams/room?limit=0".to_string()),
            ]
        );
        let _ = client.cancel().await;
    }

    /// `teams_room_read` declares `limit` and NOTHING else. In particular no `identity`: a peek is
    /// not any identity, and a tool that accepted one would be a read-as-somebody-else surface of
    /// exactly the kind §0.11.4 rules out for `from`.
    #[tokio::test]
    async fn room_read_declares_only_limit() {
        let client = connect(facade(teams_on(), 0)).await;
        let tools = client.list_all_tools().await.expect("list tools");
        let tool = tools
            .iter()
            .find(|t| t.name == "teams_room_read")
            .expect("teams_room_read is registered");
        let props = tool
            .input_schema
            .get("properties")
            .and_then(|p| p.as_object())
            .expect("an object schema");
        let mut keys: Vec<&String> = props.keys().collect();
        keys.sort();
        assert_eq!(keys, vec!["limit"]);
        let _ = client.cancel().await;
    }

    // ── the room's write side (STUDIO-653, T6) ────────────────────────────

    /// `teams_post` declares `body`, `to` and `refs` — and, crucially, NOTHING else. No `from`, no
    /// `identity`, no `run_id`. The input schema is the contract an agent reads, so this is where
    /// §0.11.4's "a run cannot supply it" is checkable rather than merely intended.
    #[tokio::test]
    async fn post_declares_no_provenance_argument() {
        let client = connect(facade(teams_on(), 0)).await;
        let tools = client.list_all_tools().await.expect("list tools");
        let tool = tools
            .iter()
            .find(|t| t.name == "teams_post")
            .expect("teams_post is registered");
        let props = tool
            .input_schema
            .get("properties")
            .and_then(|p| p.as_object())
            .expect("an object schema");
        let mut keys: Vec<&String> = props.keys().collect();
        keys.sort();
        assert_eq!(
            keys,
            vec!["body", "refs", "to"],
            "teams_post must expose no provenance argument"
        );
        let _ = client.cancel().await;
    }

    /// The post path end to end: the tool posts to the run named by SYMPHONY_RUN_ID — never a run
    /// id, and never an author, the caller supplied.
    #[tokio::test]
    async fn post_goes_to_the_run_from_the_env() {
        let (port, seen) = stub_daemon().await;
        let opts = Options {
            default_run_id: "412".to_string(),
            ..teams_on()
        };
        let client = connect(facade(opts, port)).await;

        let out = call_text(
            &client,
            "teams_post",
            serde_json::json!({"body": "the mirror lock is per-repo", "to": "alice"}),
        )
        .await;
        assert!(out.contains("\"ok\""), "out = {out}");
        assert_eq!(
            seen.lock().expect("seen lock").clone(),
            vec![("POST".to_string(), "/api/v1/runs/412/post".to_string())]
        );
        let _ = client.cancel().await;
    }

    /// A coordinator session (no SYMPHONY_RUN_ID) cannot post: there is no run to resolve an author
    /// from, and the tool refuses rather than guessing — the same rule `teams_retain` follows.
    #[tokio::test]
    async fn post_without_a_run_id_is_refused() {
        let (port, seen) = stub_daemon().await;
        let client = connect(facade(teams_on(), port)).await;
        let out = call_text(&client, "teams_post", serde_json::json!({"body": "hi"})).await;
        assert!(out.contains("SYMPHONY_RUN_ID"), "out = {out}");
        assert!(
            seen.lock().expect("seen lock").is_empty(),
            "an unattributable post must not reach the daemon"
        );
        let _ = client.cancel().await;
    }

    /// An empty body never leaves the facade: a blank line in the room is noise in every teammate's
    /// turn-1 prompt, forever.
    #[tokio::test]
    async fn an_empty_post_never_leaves_the_facade() {
        let (port, seen) = stub_daemon().await;
        let opts = Options {
            default_run_id: "412".to_string(),
            ..teams_on()
        };
        let client = connect(facade(opts, port)).await;
        let out = call_text(&client, "teams_post", serde_json::json!({"body": "  \n "})).await;
        assert!(out.contains("empty_body"), "out = {out}");
        assert!(
            seen.lock().expect("seen lock").is_empty(),
            "an empty post must not reach the daemon"
        );
        let _ = client.cancel().await;
    }

    /// The roster proxies its endpoint verbatim.
    #[tokio::test]
    async fn roster_proxies_its_endpoint() {
        let (port, seen) = stub_daemon().await;
        let client = connect(facade(teams_on(), port)).await;
        let out = call_text(&client, "teams_roster", serde_json::json!({})).await;
        assert!(out.contains("\"ok\""), "out = {out}");
        assert_eq!(
            seen.lock().expect("seen lock").clone(),
            vec![("GET".to_string(), "/api/v1/teams/roster".to_string())]
        );
        let _ = client.cancel().await;
    }
}
