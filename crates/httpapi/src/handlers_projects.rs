//! handlers_projects — the per-project live-status read handler. Parity port of Go
//! `$REF/internal/httpapi/handlers_projects.go` (`handleProjects`) + the `projectsStatusJSON`/
//! `projectStatusJSON` DTOs of `responses.go`.

use std::sync::Arc;

use axum::extract::State;
use axum::http::{Method, StatusCode};
use axum::response::Response;
use rhapsody_orchestrator::ProjectStatus;
use serde_json::{Map, Value, json};

use crate::handlers::{SNAPSHOT_TIMEOUT, require_get};
use crate::responses::{write_error, write_json};
use crate::server::StateProvider;

/// `GET /api/v1/projects`: the per-project live status (enum + running count, + optional warnings) for
/// the Settings agents list. Derived from the same runtime snapshot as `/api/v1/state`; a snapshot
/// failure/timeout is a 503 `snapshot_unavailable`. Method-agnostic route, so it guards GET/HEAD here.
/// Mirrors Go `handleProjects`.
pub(crate) async fn handle_projects(
    method: Method,
    State(provider): State<Arc<dyn StateProvider>>,
) -> Response {
    if let Some(resp) = require_get(&method) {
        return resp;
    }
    let snap = match tokio::time::timeout(SNAPSHOT_TIMEOUT, provider.snapshot()).await {
        Ok(Ok(snap)) => snap,
        Ok(Err(err)) => {
            return write_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "snapshot_unavailable",
                err.to_string(),
                None,
            );
        }
        Err(_elapsed) => {
            return write_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "snapshot_unavailable",
                "snapshot timed out",
                None,
            );
        }
    };
    let projects: Vec<Value> = snap.projects.iter().map(project_status_json).collect();
    write_json(StatusCode::OK, &json!({ "projects": projects }))
}

/// One project's live status `{slug, name, status, running, warnings?}`. `warnings` is omitted when the
/// project is healthy (Go's `omitempty`), so the golden's healthy row carries no `warnings` key.
/// Mirrors Go `projectStatusJSON`.
fn project_status_json(p: &ProjectStatus) -> Value {
    let mut obj = Map::new();
    obj.insert("slug".into(), json!(p.slug));
    obj.insert("name".into(), json!(p.name));
    obj.insert("status".into(), json!(p.status));
    obj.insert("running".into(), json!(p.running));
    if !p.warnings.is_empty() {
        obj.insert("warnings".into(), json!(p.warnings));
    }
    Value::Object(obj)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rhapsody_orchestrator::ProjectStatus;
    use serde_json::Value;

    use crate::new_handler;
    use crate::testutil::{FakeProvider, empty_snapshot, spawn_router};

    async fn spawn(provider: FakeProvider) -> String {
        spawn_router(new_handler(Arc::new(provider), None)).await
    }

    fn project(slug: &str, name: &str, status: &str, running: i64) -> ProjectStatus {
        ProjectStatus {
            slug: slug.into(),
            name: name.into(),
            status: status.into(),
            running,
            warnings: Vec::new(),
        }
    }

    async fn get_json(url: &str) -> (reqwest::StatusCode, Value) {
        let resp = reqwest::get(url).await.expect("GET");
        let status = resp.status();
        let body: Value = serde_json::from_str(&resp.text().await.expect("body")).expect("json");
        (status, body)
    }

    // Mirrors Go `TestProjectsStatusEndpoint`.
    #[tokio::test]
    async fn projects_status_endpoint() {
        let mut snap = empty_snapshot();
        snap.projects = vec![
            project("alpha", "Alpha", "running", 2),
            project("gamma", "Gamma", "paused", 0),
        ];
        let base = spawn(FakeProvider::ok(snap)).await;
        let (status, body) = get_json(&format!("{base}/api/v1/projects")).await;
        assert_eq!(status, 200);
        let projects = body["projects"].as_array().expect("projects");
        assert_eq!(projects.len(), 2);
        assert_eq!(projects[0]["name"], "Alpha");
        assert_eq!(projects[0]["status"], "running");
        assert_eq!(projects[0]["running"], 2);
        assert_eq!(projects[1]["status"], "paused");
        assert_eq!(projects[1]["running"], 0);
    }

    // Mirrors Go `TestProjectsStatusWarnings`: unhealthy projects carry warnings; healthy ones omit it.
    #[tokio::test]
    async fn projects_status_warnings() {
        let mut snap = empty_snapshot();
        let mut bad = project("bad", "Bad", "idle", 0);
        bad.warnings = vec![
            r#"project slug "bad" matches no Linear project — its agent will never dispatch"#
                .into(),
        ];
        snap.projects = vec![project("alpha", "Alpha", "idle", 0), bad];
        let base = spawn(FakeProvider::ok(snap)).await;
        let (_s, body) = get_json(&format!("{base}/api/v1/projects")).await;
        let projects = body["projects"].as_array().expect("projects");
        assert_eq!(projects.len(), 2);
        assert!(
            projects[0].get("warnings").is_none(),
            "healthy project must omit warnings"
        );
        assert_eq!(
            projects[1]["warnings"].as_array().expect("warnings").len(),
            1
        );
    }

    // Mirrors Go `TestProjectsStatusMethodNotAllowed`.
    #[tokio::test]
    async fn projects_status_method_not_allowed() {
        let base = spawn(FakeProvider::ok(empty_snapshot())).await;
        let status = reqwest::Client::new()
            .post(format!("{base}/api/v1/projects"))
            .send()
            .await
            .expect("POST")
            .status();
        assert_eq!(status, 405);
    }
}
