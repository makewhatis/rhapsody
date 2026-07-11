//! Integration test: drive the facade's read tools through an in-memory MCP client against a small
//! axum stub that serves the committed golden `harness/fixtures/api/*.json` — the ticket's explicit
//! acceptance ("Tests drive the tools against a small axum stub serving the committed fixtures").
//!
//! The tools proxy the daemon verbatim, so each tool's result must equal the fixture body the stub
//! served (a proxy-fidelity check); `symphony_run_status` additionally composes the verdict from the
//! `state` + `run_detail` fixtures.

use axum::Router;
use axum::routing::{any, get};
use rhapsody_mcp::{Client, Facade, Options};
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::service::RunningService;

/// A route serving `body` as JSON (cloned per request so the handler stays `Fn`).
fn fixture_route(body: String) -> axum::routing::MethodRouter {
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

/// Spawns the fixture-serving stub and returns its bound port. Routes mirror the daemon's `/api/v1`
/// surface the read tools proxy; each serves the committed golden body verbatim.
async fn spawn_stub() -> u16 {
    let f = |rel: &str| harness_fixtures::load(rel);
    let router = Router::new()
        .route("/api/v1/state", fixture_route(f("api/state.json")))
        .route("/api/v1/history", fixture_route(f("api/history.json")))
        .route("/api/v1/runs/1", fixture_route(f("api/run_detail.json")))
        .route("/api/v1/logs", fixture_route(f("api/logs.json")))
        .route("/api/v1/events", fixture_route(f("api/events.json")))
        // 404 with the daemon's error envelope for an unknown run (exercised indirectly).
        .fallback(get(|| async {
            (
                axum::http::StatusCode::NOT_FOUND,
                [("Content-Type", "application/json")],
                r#"{"error":{"code":"not_found","message":"no such route"}}"#,
            )
        }));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    port
}

/// Connects an in-memory MCP client to `facade` over a tokio duplex pipe.
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

async fn call_tool(
    client: &RunningService<rmcp::RoleClient, ()>,
    name: &str,
    args: serde_json::Value,
) -> CallToolResult {
    let mut params = CallToolRequestParams::new(name.to_string());
    if let Some(obj) = args.as_object()
        && !obj.is_empty()
    {
        params = params.with_arguments(obj.clone());
    }
    client.call_tool(params).await.expect("call_tool")
}

/// Each single-endpoint read tool proxies the daemon body verbatim: the result equals the fixture.
#[tokio::test]
async fn read_tools_proxy_fixture_bodies() {
    let port = spawn_stub().await;
    let facade = Facade::new(Client::for_port(port as i64), Options::default());
    let client = connect(facade).await;

    for (tool, fixture) in [
        ("symphony_state", "api/state.json"),
        ("symphony_logs", "api/logs.json"),
        ("symphony_events", "api/events.json"),
    ] {
        let res = call_tool(&client, tool, serde_json::json!({})).await;
        assert_ne!(
            res.is_error,
            Some(true),
            "{tool} errored: {}",
            result_text(&res)
        );
        assert_eq!(
            result_text(&res),
            harness_fixtures::load(fixture),
            "{tool} must proxy {fixture} verbatim"
        );
    }

    // symphony_run defaults its run_id and proxies /runs/{id}.
    let res = call_tool(&client, "symphony_run", serde_json::json!({"run_id": "1"})).await;
    assert_ne!(
        res.is_error,
        Some(true),
        "symphony_run errored: {}",
        result_text(&res)
    );
    assert_eq!(
        result_text(&res),
        harness_fixtures::load("api/run_detail.json")
    );

    let _ = client.cancel().await;
}

/// `symphony_runs` merges the recent-history page (verbatim) with the live running set.
#[tokio::test]
async fn runs_merges_history_and_running() {
    let port = spawn_stub().await;
    let facade = Facade::new(Client::for_port(port as i64), Options::default());
    let client = connect(facade).await;

    let res = call_tool(&client, "symphony_runs", serde_json::json!({})).await;
    assert_ne!(
        res.is_error,
        Some(true),
        "symphony_runs errored: {}",
        result_text(&res)
    );
    let merged: serde_json::Value = serde_json::from_str(&result_text(&res)).expect("merged JSON");

    let history: serde_json::Value =
        serde_json::from_str(&harness_fixtures::load("api/history.json")).unwrap();
    assert_eq!(
        merged["recent"], history,
        "recent must embed /history verbatim"
    );
    // state.json's running set is empty, so the merged running projection is [].
    assert_eq!(merged["running"], serde_json::json!([]));

    let _ = client.cancel().await;
}

/// `symphony_run_status` composes the verdict from `/state` + `/runs/{id}`: the golden run_detail
/// has a terminal outcome (`continued`), so the headline is `completed(continued)`.
#[tokio::test]
async fn run_status_composes_completed_from_fixtures() {
    let port = spawn_stub().await;
    // The "me" default run id resolves to run 1, exactly as a dispatched worker's env would.
    let facade = Facade::new(
        Client::for_port(port as i64),
        Options {
            default_run_id: "1".into(),
            ..Default::default()
        },
    );
    let client = connect(facade).await;

    // Both the explicit-arg and the "me"-default paths must agree.
    for args in [serde_json::json!({"run_id": "1"}), serde_json::json!({})] {
        let res = call_tool(&client, "symphony_run_status", args).await;
        assert_ne!(
            res.is_error,
            Some(true),
            "run_status errored: {}",
            result_text(&res)
        );
        let status: rhapsody_mcp::Status =
            serde_json::from_str(&result_text(&res)).expect("status JSON");
        assert_eq!(status.kind, "completed", "summary {}", status.summary);
        assert_eq!(status.outcome, "continued");
        assert_eq!(status.run_id, 1);
    }

    let _ = client.cancel().await;
}

/// Daemon unreachable (no port) ⇒ every read tool returns a `daemon_unreachable` IsError result.
#[tokio::test]
async fn daemon_unreachable_when_no_port() {
    let facade = Facade::new(Client::for_port(0), Options::default());
    let client = connect(facade).await;

    let res = call_tool(&client, "symphony_state", serde_json::json!({})).await;
    assert_eq!(res.is_error, Some(true));
    assert!(
        result_text(&res).contains("daemon_unreachable"),
        "text = {:?}",
        result_text(&res)
    );

    let _ = client.cancel().await;
}
