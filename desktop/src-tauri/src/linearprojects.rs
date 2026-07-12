//! Lists a Linear workspace's projects via a direct GraphQL call, for the first-launch onboarding
//! picker. Parity port of `$REF/desktop/internal/linearprojects/linearprojects.go`.
//!
//! The picker must show real projects BEFORE the daemon (and its config) exist, so it cannot use the
//! daemon's `GET /api/v1/linear/projects` endpoint — that needs a running daemon with a loaded config,
//! which onboarding has not produced yet (INF-277). This intentionally duplicates a minimal slice of
//! the daemon's canonical Linear client (`internal/tracker/linear`); keep `QUERY` and the
//! pagination/team-flattening below in sync with the canonical client if it changes.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// A boxed std error mirrors Go's opaque `error` return.
type Error = Box<dyn std::error::Error + Send + Sync>;

/// Mirrors config's default tracker endpoint.
const ENDPOINT: &str = "https://api.linear.app/graphql";

/// Bounds pagination so a malformed/looping cursor can't spin forever (mirrors the daemon).
const MAX_PAGES: usize = 50;

const PAGE_SIZE: u32 = 50;

/// Mirrors `internal/tracker/linear/query.go`'s `queryProjects`.
const QUERY: &str = r#"
query Projects($first: Int!, $after: String) {
  projects(first: $first, after: $after) {
    nodes {
      id
      name
      slugId
      color
      teams(first: 1) { nodes { key name } }
    }
    pageInfo { hasNextPage endCursor }
  }
}"#;

/// One Linear project as the onboarding picker needs it. The serde field names match the web
/// `LinearProject` type so the binding marshals straight through. `slug` is Linear's bare `slugId` —
/// the value the daemon's dispatch query filters on. Mirrors Go `linearprojects.Project`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub slug: String,
    pub team: String,
    pub color: String,
}

#[derive(Deserialize)]
struct ProjectsPage {
    projects: ProjectsConnection,
}

#[derive(Deserialize)]
struct ProjectsConnection {
    nodes: Vec<ProjectNode>,
    #[serde(rename = "pageInfo")]
    page_info: PageInfo,
}

#[derive(Deserialize)]
struct ProjectNode {
    id: String,
    name: String,
    #[serde(rename = "slugId")]
    slug_id: String,
    #[serde(default)]
    color: String,
    #[serde(default)]
    teams: TeamsConnection,
}

#[derive(Deserialize, Default)]
struct TeamsConnection {
    #[serde(default)]
    nodes: Vec<TeamNode>,
}

#[derive(Deserialize)]
struct TeamNode {
    #[serde(default)]
    key: String,
    #[serde(default)]
    name: String,
}

#[derive(Deserialize)]
struct PageInfo {
    #[serde(rename = "hasNextPage")]
    has_next_page: bool,
    #[serde(rename = "endCursor", default)]
    end_cursor: String,
}

#[derive(Deserialize)]
struct GqlResponse {
    #[serde(default)]
    data: Option<serde_json::Value>,
    #[serde(default)]
    errors: Vec<GqlError>,
}

#[derive(Deserialize)]
struct GqlError {
    message: String,
}

/// Returns the workspace's Linear projects using the given personal API token (the value the
/// onboarding wizard just saved). It paginates, mirroring the daemon's client. Mirrors Go `List`.
pub async fn list(token: &str) -> Result<Vec<Project>, Error> {
    list_from(ENDPOINT, token).await
}

/// [`list`] against an overridable endpoint (production uses [`ENDPOINT`]; tests point it at a local
/// mock). Mirrors Go `listFrom`.
async fn list_from(url: &str, token: &str) -> Result<Vec<Project>, Error> {
    if token.is_empty() {
        return Err("no Linear token saved".into());
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| Box::new(e) as Error)?;
    let mut out: Vec<Project> = Vec::new();
    let mut after: Option<String> = None;
    let mut pages = 0usize;
    loop {
        if pages >= MAX_PAGES {
            return Err(format!(
                "exceeded {MAX_PAGES} project pages without completing pagination"
            )
            .into());
        }
        pages += 1;
        let page = do_graphql(&client, url, token, after.as_deref()).await?;
        for node in page.projects.nodes {
            out.push(Project {
                id: node.id,
                name: node.name,
                slug: node.slug_id,
                team: first_team(&node.teams),
                color: node.color,
            });
        }
        if !page.projects.page_info.has_next_page {
            return Ok(out);
        }
        if page.projects.page_info.end_cursor.is_empty() {
            return Err("linear returned hasNextPage with no pagination cursor".into());
        }
        after = Some(page.projects.page_info.end_cursor);
    }
}

/// The first team's display name, falling back to its key, or "" when the project has no team. Mirrors
/// the Go team-flattening.
fn first_team(teams: &TeamsConnection) -> String {
    match teams.nodes.first() {
        Some(t) if !t.name.is_empty() => t.name.clone(),
        Some(t) => t.key.clone(),
        None => String::new(),
    }
}

async fn do_graphql(
    client: &reqwest::Client,
    url: &str,
    token: &str,
    after: Option<&str>,
) -> Result<ProjectsPage, Error> {
    let body = serde_json::json!({
        "query": QUERY,
        "variables": { "first": PAGE_SIZE, "after": after },
    });
    // Linear personal API keys go in the Authorization header VERBATIM (no "Bearer"), matching the
    // daemon's client.
    let resp = client
        .post(url)
        .header("Authorization", token)
        .header("Content-Type", "application/json")
        .body(serde_json::to_vec(&body).map_err(|e| Box::new(e) as Error)?)
        .send()
        .await
        .map_err(|e| Box::new(e) as Error)?;

    let status = resp.status();
    let raw = resp.bytes().await.map_err(|e| Box::new(e) as Error)?;
    if status != reqwest::StatusCode::OK {
        return Err(format!(
            "linear API returned status {}: {}",
            status.as_u16(),
            snippet(&raw)
        )
        .into());
    }
    let env: GqlResponse =
        serde_json::from_slice(&raw).map_err(|e| format!("decode Linear response: {e}"))?;
    if let Some(first) = env.errors.first() {
        return Err(format!("linear API error: {}", first.message).into());
    }
    let data = match env.data {
        Some(d) => d,
        None => return Err("linear API returned an empty response".into()),
    };
    serde_json::from_value(data).map_err(|e| format!("decode Linear data: {e}").into())
}

fn snippet(b: &[u8]) -> String {
    const MAX: usize = 256;
    if b.len() > MAX {
        format!("{}…", String::from_utf8_lossy(&b[..MAX]))
    } else {
        String::from_utf8_lossy(b).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::{BodyExt, Full};
    use hyper::body::{Bytes, Incoming};
    use hyper::service::service_fn;
    use hyper::{Request, Response};
    use hyper_util::rt::TokioIo;
    use std::sync::{Arc, Mutex};
    use tokio::net::TcpListener;

    /// A mock Linear GraphQL server (the Rust equivalent of Go's `httptest.NewServer`): for each POST it
    /// invokes `responder(auth_header, after_var)` and returns the produced `(status, body)`. Records
    /// the number of requests it served.
    struct MockServer {
        url: String,
        calls: Arc<Mutex<usize>>,
        _handle: tokio::task::JoinHandle<()>,
    }

    async fn start_mock<F>(responder: F) -> MockServer
    where
        F: Fn(Option<String>, Option<String>) -> (u16, String) + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
        let url = format!("http://{}/graphql", listener.local_addr().expect("addr"));
        let calls = Arc::new(Mutex::new(0usize));
        let responder = Arc::new(responder);
        let calls_for_loop = calls.clone();
        let handle = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    continue;
                };
                let io = TokioIo::new(stream);
                let responder = responder.clone();
                let calls = calls_for_loop.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |req: Request<Incoming>| {
                        let responder = responder.clone();
                        let calls = calls.clone();
                        async move {
                            let auth = req
                                .headers()
                                .get("Authorization")
                                .and_then(|v| v.to_str().ok())
                                .map(str::to_string);
                            let body = req
                                .into_body()
                                .collect()
                                .await
                                .map(|c| c.to_bytes())
                                .unwrap_or_default();
                            let after = extract_after(&body);
                            *calls.lock().expect("calls lock") += 1;
                            let (status, resp_body) = responder(auth, after);
                            let resp = Response::builder()
                                .status(status)
                                .body(Full::new(Bytes::from(resp_body)))
                                .expect("build response");
                            Ok::<_, std::convert::Infallible>(resp)
                        }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(io, service)
                        .await;
                });
            }
        });
        MockServer {
            url,
            calls,
            _handle: handle,
        }
    }

    /// Extracts `variables.after` from a GraphQL request body (the pagination cursor), if present.
    fn extract_after(body: &[u8]) -> Option<String> {
        let v: serde_json::Value = serde_json::from_slice(body).ok()?;
        v.get("variables")?
            .get("after")?
            .as_str()
            .map(str::to_string)
    }

    // Mirrors TestList_ParsesAndFlattensTeam: the token is sent verbatim in Authorization; projects
    // parse; the team name flattens (name, else key, else empty).
    #[tokio::test]
    async fn list_parses_and_flattens_team() {
        let auth_seen = Arc::new(Mutex::new(None));
        let auth_recorder = auth_seen.clone();
        let srv = start_mock(move |auth, _after| {
            *auth_recorder.lock().expect("auth lock") = auth;
            (
                200,
                r##"{"data":{"projects":{"nodes":[
                    {"id":"p1","name":"Symphony App","slugId":"872639248532","color":"#fff","teams":{"nodes":[{"key":"FND","name":"Foundation"}]}},
                    {"id":"p2","name":"NoTeamName","slugId":"abc12345","color":"#000","teams":{"nodes":[{"key":"OPS","name":""}]}},
                    {"id":"p3","name":"Teamless","slugId":"def67890","color":"","teams":{"nodes":[]}}
                ],"pageInfo":{"hasNextPage":false,"endCursor":""}}}}"##
                    .to_string(),
            )
        })
        .await;

        let got = list_from(&srv.url, "lin_test").await.expect("list");
        assert_eq!(got.len(), 3, "want 3 projects");
        assert_eq!(got[0].slug, "872639248532");
        assert_eq!(got[0].name, "Symphony App");
        assert_eq!(got[0].team, "Foundation");
        assert_eq!(
            got[1].team, "OPS",
            "falls back to the team key when the name is empty"
        );
        assert_eq!(got[2].team, "", "no team => empty string, never panics");
        assert_eq!(
            auth_seen.lock().expect("auth lock").as_deref(),
            Some("lin_test"),
            "Authorization must be the verbatim token"
        );
    }

    // Mirrors TestList_Paginates: two pages, the second requested with after=CUR; both nodes returned.
    #[tokio::test]
    async fn list_paginates() {
        let srv = start_mock(|_auth, after| match after.as_deref() {
            None => (
                200,
                r#"{"data":{"projects":{"nodes":[{"id":"p1","name":"A","slugId":"aaaa1111"}],"pageInfo":{"hasNextPage":true,"endCursor":"CUR"}}}}"#
                    .to_string(),
            ),
            Some("CUR") => (
                200,
                r#"{"data":{"projects":{"nodes":[{"id":"p2","name":"B","slugId":"bbbb2222"}],"pageInfo":{"hasNextPage":false,"endCursor":""}}}}"#
                    .to_string(),
            ),
            Some(other) => (200, format!(r#"{{"errors":[{{"message":"unexpected after {other}"}}]}}"#)),
        })
        .await;

        let got = list_from(&srv.url, "lin_test").await.expect("list");
        assert_eq!(
            *srv.calls.lock().expect("calls"),
            2,
            "want 2 calls (paginated)"
        );
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].slug, "aaaa1111");
        assert_eq!(got[1].slug, "bbbb2222");
    }

    // Mirrors TestList_EmptyTokenErrors: an empty token errors before any HTTP.
    #[tokio::test]
    async fn list_empty_token_errors() {
        assert!(list("").await.is_err(), "empty token should error");
    }

    // Mirrors TestList_GraphQLErrorSurfaces: a GraphQL error payload surfaces as an error.
    #[tokio::test]
    async fn list_graphql_error_surfaces() {
        let srv = start_mock(|_, _| {
            (
                200,
                r#"{"errors":[{"message":"Authentication required"}]}"#.to_string(),
            )
        })
        .await;
        assert!(
            list_from(&srv.url, "bad").await.is_err(),
            "a GraphQL error payload should surface"
        );
    }

    // Mirrors TestList_HTTPErrorStatusSurfaces: a non-200 status surfaces as an error.
    #[tokio::test]
    async fn list_http_error_status_surfaces() {
        let srv = start_mock(|_, _| (401, "unauthorized".to_string())).await;
        assert!(
            list_from(&srv.url, "bad").await.is_err(),
            "a non-200 status should surface"
        );
    }
}
