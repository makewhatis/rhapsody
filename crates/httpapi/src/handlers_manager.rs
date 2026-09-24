//! handlers_manager — the manager run's host-served reads (STUDIO-1014; design record
//! `~/.rhapsody/docs/manager-agent-design.md` §4.4, §5.5).
//!
//! **No Go v0.4.0 counterpart.** These are the daemon endpoints the `--role manager` MCP facade
//! proxies: `/api/v1/manager/{file,ls,grep,diff,interdiff,patch-id,findings,pr,pr/activity,pr/commits}`.
//! The manager run has no checkout, no `gh` and no `git`, so the host answers every read.
//!
//! # The coordinate is the RUN, never a client value
//!
//! Each handler takes `run_id` (the manager's own `SYMPHONY_RUN_ID`) and nothing that names a
//! repository or pull request: the coordinate is parsed from the run row's `issue_identifier`
//! (a `pr:owner/repo#n@manager` key). Every route is GET-only and registers method-agnostically
//! (via `any`) so a non-GET gets an explicit 405 rather than the SPA fallback.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::{Method, StatusCode};
use axum::response::Response;
use rhapsody_orchestrator::managerread::ManagerReadError;

use crate::handlers::require_get;
use crate::responses::{write_error, write_json};
use crate::server::StateProvider;

/// Extracts a positive `run_id` query parameter, or a 400 envelope.
fn run_id_param(q: &HashMap<String, String>) -> Result<i64, Box<Response>> {
    match q
        .get("run_id")
        .map(String::as_str)
        .and_then(|s| s.parse::<i64>().ok())
    {
        Some(id) if id > 0 => Ok(id),
        _ => Err(Box::new(write_error(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "run_id is required and must be a positive integer",
            None,
        ))),
    }
}

/// A required non-empty query parameter, or a 400 envelope.
fn required_param(
    q: &HashMap<String, String>,
    name: &'static str,
) -> Result<String, Box<Response>> {
    match q.get(name) {
        Some(v) if !v.is_empty() => Ok(v.clone()),
        _ => Err(Box::new(write_error(
            StatusCode::BAD_REQUEST,
            "bad_request",
            format!("{name} is required"),
            None,
        ))),
    }
}

/// An optional query parameter (empty string when absent).
fn optional_param(q: &HashMap<String, String>, name: &str) -> String {
    q.get(name).cloned().unwrap_or_default()
}

/// Maps a manager read refusal to the wire: a 200 body for a served read, else the refusal's own
/// code and status.
fn render(outcome: Result<serde_json::Value, ManagerReadError>) -> Response {
    match outcome {
        Ok(body) => write_json(StatusCode::OK, &body),
        Err(ManagerReadError::NoSuchRun) => {
            write_error(StatusCode::NOT_FOUND, "not_found", "no such run", None)
        }
        Err(ManagerReadError::NotAManagerRun) => write_error(
            StatusCode::NOT_FOUND,
            "not_a_manager_run",
            "this run is not a manager run",
            None,
        ),
        Err(ManagerReadError::Unavailable(why)) => {
            write_error(StatusCode::SERVICE_UNAVAILABLE, "unavailable", why, None)
        }
        // The host's own `gh` could not answer: a gateway failure, not a not-found. A refusal to
        // answer must never read as "the read found nothing".
        Err(e @ ManagerReadError::Gh(_)) => {
            write_error(StatusCode::BAD_GATEWAY, e.code(), e.message(), None)
        }
        // A host git read (or a store read): its own code carries the meaning, so the status is
        // derived from the SAME code the tool sees rather than from a second mapping that can drift.
        Err(e @ ManagerReadError::Read(_)) | Err(e @ ManagerReadError::Store(_)) => {
            let code = e.code();
            let status = match code {
                "not_found" => StatusCode::NOT_FOUND,
                "invalid_revision" | "is_a_directory" => StatusCode::BAD_REQUEST,
                "too_large" => StatusCode::PAYLOAD_TOO_LARGE,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            };
            write_error(status, code, e.message(), None)
        }
    }
}

/// `GET /api/v1/manager/file?run_id&sha&path`.
pub(crate) async fn handle_manager_file(
    State(provider): State<Arc<dyn StateProvider>>,
    method: Method,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if let Some(r) = require_get(&method) {
        return r;
    }
    let (run_id, sha, path) = match (
        run_id_param(&q),
        required_param(&q, "sha"),
        required_param(&q, "path"),
    ) {
        (Ok(a), Ok(b), Ok(c)) => (a, b, c),
        (Err(r), _, _) | (_, Err(r), _) | (_, _, Err(r)) => return *r,
    };
    render(provider.manager_file(run_id, sha, path).await)
}

/// `GET /api/v1/manager/ls?run_id&sha&path`.
pub(crate) async fn handle_manager_ls(
    State(provider): State<Arc<dyn StateProvider>>,
    method: Method,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if let Some(r) = require_get(&method) {
        return r;
    }
    let run_id = match run_id_param(&q) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    let sha = match required_param(&q, "sha") {
        Ok(v) => v,
        Err(r) => return *r,
    };
    let path = optional_param(&q, "path");
    render(provider.manager_ls(run_id, sha, path).await)
}

/// `GET /api/v1/manager/grep?run_id&sha&pattern&path`.
pub(crate) async fn handle_manager_grep(
    State(provider): State<Arc<dyn StateProvider>>,
    method: Method,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if let Some(r) = require_get(&method) {
        return r;
    }
    let run_id = match run_id_param(&q) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    let sha = match required_param(&q, "sha") {
        Ok(v) => v,
        Err(r) => return *r,
    };
    let pattern = match required_param(&q, "pattern") {
        Ok(v) => v,
        Err(r) => return *r,
    };
    let path = optional_param(&q, "path");
    render(provider.manager_grep(run_id, sha, pattern, path).await)
}

/// `GET /api/v1/manager/diff?run_id&from&to`.
pub(crate) async fn handle_manager_diff(
    State(provider): State<Arc<dyn StateProvider>>,
    method: Method,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if let Some(r) = require_get(&method) {
        return r;
    }
    let run_id = match run_id_param(&q) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    let (from, to) = match (required_param(&q, "from"), required_param(&q, "to")) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(r), _) | (_, Err(r)) => return *r,
    };
    render(provider.manager_diff(run_id, from, to).await)
}

/// `GET /api/v1/manager/interdiff?run_id&from&to`.
pub(crate) async fn handle_manager_interdiff(
    State(provider): State<Arc<dyn StateProvider>>,
    method: Method,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if let Some(r) = require_get(&method) {
        return r;
    }
    let run_id = match run_id_param(&q) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    let (from, to) = match (required_param(&q, "from"), required_param(&q, "to")) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(r), _) | (_, Err(r)) => return *r,
    };
    render(provider.manager_interdiff(run_id, from, to).await)
}

/// `GET /api/v1/manager/patch-id?run_id&sha`.
pub(crate) async fn handle_manager_patch_id(
    State(provider): State<Arc<dyn StateProvider>>,
    method: Method,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if let Some(r) = require_get(&method) {
        return r;
    }
    let run_id = match run_id_param(&q) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    let sha = match required_param(&q, "sha") {
        Ok(v) => v,
        Err(r) => return *r,
    };
    render(provider.manager_patch_id(run_id, sha).await)
}

/// `GET /api/v1/manager/findings?run_id`.
pub(crate) async fn handle_manager_findings(
    State(provider): State<Arc<dyn StateProvider>>,
    method: Method,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if let Some(r) = require_get(&method) {
        return r;
    }
    let run_id = match run_id_param(&q) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    render(provider.manager_findings(run_id).await)
}

/// `GET /api/v1/manager/pr?run_id`.
pub(crate) async fn handle_manager_pr(
    State(provider): State<Arc<dyn StateProvider>>,
    method: Method,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if let Some(r) = require_get(&method) {
        return r;
    }
    let run_id = match run_id_param(&q) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    render(provider.manager_pr(run_id).await)
}

/// `GET /api/v1/manager/pr/activity?run_id&since`.
pub(crate) async fn handle_manager_pr_activity(
    State(provider): State<Arc<dyn StateProvider>>,
    method: Method,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if let Some(r) = require_get(&method) {
        return r;
    }
    let run_id = match run_id_param(&q) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    render(
        provider
            .manager_pr_activity(run_id, optional_param(&q, "since"))
            .await,
    )
}

/// `GET /api/v1/manager/pr/commits?run_id&since`.
pub(crate) async fn handle_manager_pr_commits(
    State(provider): State<Arc<dyn StateProvider>>,
    method: Method,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if let Some(r) = require_get(&method) {
        return r;
    }
    let run_id = match run_id_param(&q) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    render(
        provider
            .manager_pr_commits(run_id, optional_param(&q, "since"))
            .await,
    )
}

#[cfg(test)]
mod tests {
    //! The route end to end over a real loopback listener, against a canned provider. What is
    //! asserted is the WIRE contract: the run id reaches the provider, the args pass through, a
    //! served read is a 200 JSON body, and a refusal keeps its code. The git behaviour itself lives
    //! in `rhapsody_workspace::read` and the coordinate/refusal rules in
    //! `rhapsody_orchestrator::managerread`.

    use std::sync::Arc;

    use serde_json::Value;

    use super::*;
    use crate::server::new_handler;
    use crate::testutil::{FakeProvider, empty_snapshot, spawn_router};

    async fn spawn(provider: Arc<FakeProvider>) -> String {
        spawn_router(new_handler(provider, None)).await
    }

    async fn body_json(resp: reqwest::Response) -> Value {
        let text = resp.text().await.expect("body text");
        serde_json::from_str(&text).expect("json body")
    }

    #[tokio::test]
    async fn manager_file_serves_the_body_and_forwards_args() {
        let provider = Arc::new(FakeProvider::ok(empty_snapshot()).with_manager_outcome(Ok(
            serde_json::json!({"sha": "abc", "path": "src/lib.rs", "content": "fn main() {}",
             "symlink": false}),
        )));
        let url = spawn(Arc::clone(&provider)).await;
        let resp = reqwest::get(format!(
            "{url}/api/v1/manager/file?run_id=7&sha=abc&path=src/lib.rs"
        ))
        .await
        .expect("GET");
        assert_eq!(resp.status(), 200);
        let body = body_json(resp).await;
        assert_eq!(body["content"], "fn main() {}");
        assert_eq!(body["symlink"], false);
        assert_eq!(
            provider.manager_asked().as_deref(),
            Some("file:7:abc:src/lib.rs")
        );
    }

    #[tokio::test]
    async fn manager_diff_forwards_the_range() {
        let provider = Arc::new(FakeProvider::ok(empty_snapshot()).with_manager_outcome(Ok(
            serde_json::json!({"from": "a", "to": "b", "patch": "PATCH"}),
        )));
        let url = spawn(Arc::clone(&provider)).await;
        let resp = reqwest::get(format!("{url}/api/v1/manager/diff?run_id=7&from=a&to=b"))
            .await
            .expect("GET");
        assert_eq!(resp.status(), 200);
        let body = body_json(resp).await;
        assert_eq!(body["patch"], "PATCH");
        assert_eq!(provider.manager_asked().as_deref(), Some("diff:7:a:b"));
    }

    // A run that is not a manager run keeps its own code and never becomes a 200.
    #[tokio::test]
    async fn a_non_manager_run_is_refused_with_its_code() {
        let provider = Arc::new(
            FakeProvider::ok(empty_snapshot())
                .with_manager_outcome(Err(ManagerReadError::NotAManagerRun)),
        );
        let url = spawn(provider).await;
        let resp = reqwest::get(format!("{url}/api/v1/manager/findings?run_id=7"))
            .await
            .expect("GET");
        assert_eq!(resp.status(), 404);
        let body = body_json(resp).await;
        assert_eq!(body["error"]["code"], "not_a_manager_run");
    }

    // A missing run_id is a 400 and never reaches the provider.
    #[tokio::test]
    async fn a_missing_run_id_is_a_bad_request() {
        let provider = Arc::new(FakeProvider::ok(empty_snapshot()));
        let url = spawn(Arc::clone(&provider)).await;
        let resp = reqwest::get(format!("{url}/api/v1/manager/file?sha=a&path=p"))
            .await
            .expect("GET");
        assert_eq!(resp.status(), 400);
        assert_eq!(
            provider.manager_asked(),
            None,
            "provider must not be called"
        );
    }

    // A daemon that cannot serve manager reads answers 503 with a stated reason, not a 500 fault.
    #[tokio::test]
    async fn an_unavailable_read_is_a_503_with_a_reason() {
        let provider = Arc::new(FakeProvider::ok(empty_snapshot()).with_manager_outcome(Err(
            ManagerReadError::Unavailable("this daemon cannot serve manager reads"),
        )));
        let url = spawn(provider).await;
        let resp = reqwest::get(format!("{url}/api/v1/manager/file?run_id=7&sha=abc&path=p"))
            .await
            .expect("GET");
        assert_eq!(resp.status(), 503);
        let body = body_json(resp).await;
        assert_eq!(body["error"]["code"], "unavailable");
    }

    // The host's own `gh` could not answer: a 502 gateway failure with its own code, never a 200
    // that reads as "the read found nothing".
    #[tokio::test]
    async fn a_gh_failure_is_a_502_with_its_own_code() {
        let provider = Arc::new(FakeProvider::ok(empty_snapshot()).with_manager_outcome(Err(
            ManagerReadError::Gh("gh pr view 7 --repo o/r: HTTP 502".to_string()),
        )));
        let url = spawn(provider).await;
        let resp = reqwest::get(format!("{url}/api/v1/manager/pr?run_id=7"))
            .await
            .expect("GET");
        assert_eq!(resp.status(), 502);
        let body = body_json(resp).await;
        assert_eq!(body["error"]["code"], "gh_failed");
    }

    // The routes are GET-only: a POST is a 405, not the SPA fallback.
    #[tokio::test]
    async fn a_post_is_method_not_allowed() {
        let provider = Arc::new(FakeProvider::ok(empty_snapshot()));
        let url = spawn(provider).await;
        let client = reqwest::Client::new();
        let resp = client
            .post(format!("{url}/api/v1/manager/diff?run_id=7&from=a&to=b"))
            .send()
            .await
            .expect("POST");
        assert_eq!(resp.status(), 405);
    }
}
