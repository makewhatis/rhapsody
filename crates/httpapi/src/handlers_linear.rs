//! handlers_linear — the read-only Linear proxy handlers for the Settings page. Parity port of Go
//! `$REF/internal/httpapi/handlers_linear.go` (`handleLinearProjects`/`handleLinearIdentity`) + the
//! `linearProjectsJSON`/`linearProjectJSON`/`identityJSON` DTOs of `responses.go`.

use std::sync::Arc;

use axum::extract::State;
use axum::http::{Method, StatusCode};
use axum::response::Response;
use rhapsody_core::Project;
use rhapsody_orchestrator::{Identity, ReadsError};
use serde_json::{Value, json};

use crate::handlers::require_get;
use crate::responses::{write_error, write_json};
use crate::server::StateProvider;

/// `GET /api/v1/linear/projects`: the workspace's Linear projects for the add-agent picker
/// (id/name/slug/team/color). 503 `config_not_loaded` before the daemon's first config load; 502
/// `linear_unavailable` on any other Linear failure. Method-agnostic route, so it guards GET/HEAD here.
/// Mirrors Go `handleLinearProjects`.
pub(crate) async fn handle_linear_projects(
    method: Method,
    State(provider): State<Arc<dyn StateProvider>>,
) -> Response {
    if let Some(resp) = require_get(&method) {
        return resp;
    }
    match provider.list_linear_projects().await {
        Ok(projects) => {
            let projects: Vec<Value> = projects.iter().map(linear_project_json).collect();
            write_json(StatusCode::OK, &json!({ "projects": projects }))
        }
        Err(err @ ReadsError::ConfigNotLoaded) => write_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "config_not_loaded",
            err.to_string(),
            None,
        ),
        Err(err) => write_error(
            StatusCode::BAD_GATEWAY,
            "linear_unavailable",
            err.to_string(),
            None,
        ),
    }
}

/// `GET /api/v1/linear/identity`: the connected-as account. ALWAYS 200 — a resolution failure is logged
/// and surfaced as `connected:false` (the masked token still indicates a key is configured), so the UI
/// renders "not connected" rather than erroring. The token is always masked. Mirrors Go
/// `handleLinearIdentity`.
pub(crate) async fn handle_linear_identity(
    method: Method,
    State(provider): State<Arc<dyn StateProvider>>,
) -> Response {
    if let Some(resp) = require_get(&method) {
        return resp;
    }
    let (id, err) = provider.connected_viewer().await;
    if let Some(err) = err {
        tracing::warn!(error = %err, "linear: connected-as identity resolution failed");
    }
    write_json(StatusCode::OK, &identity_json(&id))
}

/// One Linear project on the wire `{id, name, slug, team, color}`. Mirrors Go `linearProjectJSON`.
fn linear_project_json(p: &Project) -> Value {
    json!({
        "id": p.id,
        "name": p.name,
        "slug": p.slug,
        "team": p.team,
        "color": p.color,
    })
}

/// The connected-as identity on the wire `{connected, name, display_name, email, token,
/// workspace_url_key}`. `token` is the MASKED indicator, never the raw secret. Mirrors Go
/// `identityJSON`.
fn identity_json(id: &Identity) -> Value {
    json!({
        "connected": id.connected,
        "name": id.viewer.name,
        "display_name": id.viewer.display_name,
        "email": id.viewer.email,
        "token": id.masked_token,
        "workspace_url_key": id.viewer.url_key,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rhapsody_core::{Project, Viewer};
    use rhapsody_orchestrator::Identity;
    use serde_json::Value;

    use crate::new_handler;
    use crate::testutil::{FakeProvider, empty_snapshot, spawn_router};

    async fn spawn(provider: FakeProvider) -> String {
        spawn_router(new_handler(Arc::new(provider), None)).await
    }

    async fn get_json(url: &str) -> (reqwest::StatusCode, Value) {
        let resp = reqwest::get(url).await.expect("GET");
        let status = resp.status();
        let body: Value = serde_json::from_str(&resp.text().await.expect("body")).expect("json");
        (status, body)
    }

    async fn post_status(url: &str) -> reqwest::StatusCode {
        reqwest::Client::new()
            .post(url)
            .send()
            .await
            .expect("POST")
            .status()
    }

    // Mirrors Go `TestLinearProjectsEndpoint`: id/name/slug/team/color are returned.
    #[tokio::test]
    async fn linear_projects_endpoint() {
        let projects = vec![
            Project {
                id: "p1".into(),
                name: "Infra Bot".into(),
                slug: "infra-bot".into(),
                team: "Foundation Engineering".into(),
                color: "#10b981".into(),
            },
            Project {
                id: "p2".into(),
                name: "Core".into(),
                slug: "core-proj".into(),
                team: String::new(),
                color: "#6366f1".into(),
            },
        ];
        let base = spawn(FakeProvider::ok(empty_snapshot()).with_linear_projects(projects)).await;
        let (status, body) = get_json(&format!("{base}/api/v1/linear/projects")).await;
        assert_eq!(status, 200);
        let projects = body["projects"].as_array().expect("projects");
        assert_eq!(projects.len(), 2);
        let p0 = &projects[0];
        assert_eq!(p0["id"], "p1");
        assert_eq!(p0["name"], "Infra Bot");
        assert_eq!(p0["slug"], "infra-bot");
        assert_eq!(p0["team"], "Foundation Engineering");
        assert_eq!(p0["color"], "#10b981");
    }

    // Mirrors Go `TestLinearProjectsMethodNotAllowed`.
    #[tokio::test]
    async fn linear_projects_method_not_allowed() {
        let base = spawn(FakeProvider::ok(empty_snapshot())).await;
        assert_eq!(
            post_status(&format!("{base}/api/v1/linear/projects")).await,
            405
        );
    }

    // The config-not-loaded path (before the first config load) is a 503 `config_not_loaded`. Go's
    // linear_test.go leaves this untested; asserting it here covers the handler's error mapping.
    #[tokio::test]
    async fn linear_projects_config_not_loaded_503() {
        let base =
            spawn(FakeProvider::ok(empty_snapshot()).with_projects_config_not_loaded()).await;
        let (status, body) = get_json(&format!("{base}/api/v1/linear/projects")).await;
        assert_eq!(status, 503);
        assert_eq!(body["error"]["code"], "config_not_loaded");
    }

    // Mirrors Go `TestLinearIdentityConnected`: the token is MASKED, never the raw secret.
    #[tokio::test]
    async fn linear_identity_connected() {
        let identity = Identity {
            connected: true,
            viewer: Viewer {
                id: "v1".into(),
                name: "Jane Quentin".into(),
                display_name: "jane".into(),
                email: "jane@example.com".into(),
                url_key: String::new(),
            },
            masked_token: "lin_***…1234".into(),
        };
        let base = spawn(FakeProvider::ok(empty_snapshot()).with_identity(identity)).await;
        let (status, body) = get_json(&format!("{base}/api/v1/linear/identity")).await;
        assert_eq!(status, 200);
        assert_eq!(body["connected"], true);
        assert_eq!(body["name"], "Jane Quentin");
        assert_eq!(body["display_name"], "jane");
        assert_eq!(body["email"], "jane@example.com");
        assert_eq!(body["token"], "lin_***…1234");
    }

    // Mirrors Go `TestLinearIdentityNotConnected`: connected=false (no 5xx) when no key resolves.
    #[tokio::test]
    async fn linear_identity_not_connected() {
        let identity = Identity {
            connected: false,
            ..Default::default()
        };
        let base = spawn(FakeProvider::ok(empty_snapshot()).with_identity(identity)).await;
        let (status, body) = get_json(&format!("{base}/api/v1/linear/identity")).await;
        assert_eq!(status, 200);
        assert_eq!(body["connected"], false);
    }
}
