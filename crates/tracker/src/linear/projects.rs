//! Projects list — parity port of `internal/tracker/linear/projects.go` (INF-224).
//!
//! [`list_projects`] lists the workspace's Linear projects (id, name, slug, team, color) for the
//! Settings add-agent picker, following pagination like the issue queries. Team is the first
//! associated team's name (falling back to its key); a project with no team has `team == ""`. It is
//! account-scoped (not project-filtered).

use super::candidates::{MAX_PAGES, PageInfo};
use super::{Client, LinearError, LinearErrorKind, query};
use crate::TrackerError;
use rhapsody_core::Project;
use serde::Deserialize;

/// The `projects` connection shape returned by [`query::QUERY_PROJECTS`].
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ProjectsPage {
    projects: ProjectsConnection,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ProjectsConnection {
    nodes: Vec<ProjectNode>,
    #[serde(rename = "pageInfo")]
    page_info: PageInfo,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ProjectNode {
    id: String,
    name: String,
    #[serde(rename = "slugId")]
    slug_id: String,
    color: String,
    teams: TeamsConnection,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct TeamsConnection {
    nodes: Vec<TeamNode>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct TeamNode {
    key: String,
    name: String,
}

/// ListProjects lists the workspace's Linear projects for the add-agent picker, following
/// pagination (projects.go's `ListProjects`). Team is the first team's name, falling back to its
/// key; no team => empty string.
pub(super) async fn list_projects(c: &Client) -> Result<Vec<Project>, TrackerError> {
    let mut out: Vec<Project> = Vec::new();
    let mut after: Option<String> = None;
    let mut pages: u32 = 0;
    loop {
        pages += 1;
        if pages > MAX_PAGES {
            return Err(LinearError::new(
                LinearErrorKind::MissingCursor,
                format!("exceeded {MAX_PAGES} project pages without completing pagination"),
            )
            .into());
        }
        let vars = serde_json::json!({ "first": c.page_size, "after": after });
        let page: ProjectsPage = c.do_graphql(query::QUERY_PROJECTS, Some(vars)).await?;
        for n in page.projects.nodes {
            let team = match n.teams.nodes.first() {
                Some(t) if !t.name.is_empty() => t.name.clone(),
                Some(t) => t.key.clone(),
                None => String::new(),
            };
            out.push(Project {
                id: n.id,
                name: n.name,
                slug: n.slug_id,
                team,
                color: n.color,
            });
        }
        if !page.projects.page_info.has_next_page {
            return Ok(out);
        }
        if page.projects.page_info.end_cursor.is_empty() {
            return Err(LinearError::bare(LinearErrorKind::MissingCursor).into());
        }
        after = Some(page.projects.page_info.end_cursor);
    }
}

#[cfg(test)]
mod tests {
    use crate::Tracker;
    use crate::linear::testutil::{MockResp, MockServer};
    use crate::linear::{Config, new};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    const TEST_PROJECTS_RESP: &str = r##"{"data":{"projects":{"nodes":[
      {"id":"p1","name":"Infra Bot","slugId":"symphony","color":"#10b981","teams":{"nodes":[{"key":"INF","name":"Foundation Engineering"}]}},
      {"id":"p2","name":"Core","slugId":"core-proj","color":"#6366f1","teams":{"nodes":[]}}
    ],"pageInfo":{"hasNextPage":false,"endCursor":"x"}}}}"##;

    // Mirrors Go TestListProjectsParsesFields.
    #[tokio::test]
    async fn list_projects_parses_fields() {
        let q = Arc::new(Mutex::new(String::new()));
        let q_h = Arc::clone(&q);
        let server = MockServer::start(move |req| {
            *q_h.lock().expect("q") = req.query.clone();
            MockResp::ok(TEST_PROJECTS_RESP)
        })
        .await;
        let c = new(Config {
            endpoint: server.url(),
            api_key: "k".into(),
            ..Config::default()
        });
        let got = c.list_projects().await.expect("list");
        let q = q.lock().expect("q");
        assert!(q.contains("projects("), "expected a projects query");
        for want in ["slugId", "color", "teams"] {
            assert!(
                q.contains(want),
                "projects query missing {want:?} selection"
            );
        }
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].id, "p1");
        assert_eq!(got[0].name, "Infra Bot");
        assert_eq!(got[0].slug, "symphony");
        assert_eq!(got[0].color, "#10b981");
        assert_eq!(got[0].team, "Foundation Engineering");
        assert_eq!(got[1].team, "", "team empty when no teams");
    }

    // Mirrors Go TestListProjectsPaginates.
    #[tokio::test]
    async fn list_projects_paginates() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_h = Arc::clone(&calls);
        let server = MockServer::start(move |_req| {
            if calls_h.fetch_add(1, Ordering::SeqCst) == 0 {
                MockResp::ok(
                    r#"{"data":{"projects":{"nodes":[{"id":"p1","name":"A","slugId":"a","color":"","teams":{"nodes":[]}}],"pageInfo":{"hasNextPage":true,"endCursor":"c1"}}}}"#,
                )
            } else {
                MockResp::ok(
                    r#"{"data":{"projects":{"nodes":[{"id":"p2","name":"B","slugId":"b","color":"","teams":{"nodes":[]}}],"pageInfo":{"hasNextPage":false,"endCursor":"c2"}}}}"#,
                )
            }
        })
        .await;
        let c = new(Config {
            endpoint: server.url(),
            api_key: "k".into(),
            ..Config::default()
        });
        let got = c.list_projects().await.expect("list");
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].id, "p1");
        assert_eq!(got[1].id, "p2");
        assert_eq!(calls.load(Ordering::SeqCst), 2, "followed hasNextPage");
    }
}
