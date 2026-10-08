//! Read-only project/issue documents (STUDIO-1146). No Go counterpart.
//! Uses the public Linear SDK schema: DocumentFilter.project/issue/title, Document.content.

use super::Client;
use crate::{Document, Documents, TrackerError};
use serde::Deserialize;

const QUERY: &str = "query ManagerDocuments($filter: DocumentFilter!, $after: String, $excerpt: Boolean!) { documents(first: 50, after: $after, filter: $filter, orderBy: updatedAt) { nodes { id title url updatedAt content @include(if: $excerpt) project { slugId } issue { id } } pageInfo { hasNextPage endCursor } } }";

#[derive(Deserialize)]
struct Page {
    documents: Connection,
}
#[derive(Deserialize)]
struct Connection {
    nodes: Vec<Node>,
    #[serde(rename = "pageInfo")]
    page_info: PageInfo,
}
#[derive(Deserialize)]
struct PageInfo {
    #[serde(rename = "hasNextPage")]
    has_next: bool,
    #[serde(rename = "endCursor")]
    cursor: Option<String>,
}
#[derive(Deserialize)]
struct Node {
    #[serde(deserialize_with = "super::decode::null_to_empty")]
    id: String,
    #[serde(deserialize_with = "super::decode::null_to_empty")]
    title: String,
    #[serde(deserialize_with = "super::decode::null_to_empty")]
    url: String,
    #[serde(
        rename = "updatedAt",
        deserialize_with = "super::decode::null_to_empty"
    )]
    updated: String,
    content: Option<String>,
    project: Option<Project>,
    issue: Option<IssueId>,
}
#[derive(Deserialize)]
struct Project {
    #[serde(rename = "slugId", deserialize_with = "super::decode::null_to_empty")]
    slug: String,
}
#[derive(Deserialize)]
struct IssueId {
    #[serde(deserialize_with = "super::decode::null_to_empty")]
    id: String,
}

pub(crate) fn bounded(text: &str, cap: usize) -> String {
    let mut end = text.len().min(cap);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].into()
}

pub(super) async fn fetch_documents(
    c: &Client,
    issue: Option<&str>,
    query: &str,
    excerpt: bool,
) -> Result<Documents, TrackerError> {
    if c.config.project_slug.is_empty() || query.len() > 4000 {
        return Err(TrackerError::Other(
            "documents require a configured project and a bounded query".into(),
        ));
    }
    let issue_id = match issue {
        Some(identifier) => {
            let scoped = super::by_ids::fetch_issue_by_identifier(c, identifier)
                .await?
                .ok_or_else(|| {
                    TrackerError::Other(
                        "document issue is absent or outside configured project".into(),
                    )
                })?;
            Some(scoped.id)
        }
        None => None,
    };
    let mut filter = if let Some(id) = &issue_id {
        serde_json::json!({"issue":{"id":{"eq":id}}})
    } else {
        serde_json::json!({"project":{"slugId":{"eq":c.config.project_slug}}})
    };
    if !query.trim().is_empty() {
        filter["title"] = serde_json::json!({"containsIgnoreCase":query.trim()});
    }
    let mut after: Option<String> = None;
    let mut out = Documents::default();
    let mut cursors = std::collections::HashSet::new();
    for _ in 0..4 {
        let page: Page = c
            .do_graphql(
                QUERY,
                Some(serde_json::json!({"filter":filter,"after":after,"excerpt":excerpt})),
            )
            .await?;
        for node in page.documents.nodes {
            let in_scope = match &issue_id {
                Some(id) => node.issue.as_ref().is_some_and(|i| i.id == *id),
                None => node
                    .project
                    .as_ref()
                    .is_some_and(|p| p.slug == c.config.project_slug),
            };
            if !in_scope {
                return Err(TrackerError::Other(
                    "document lookup returned an out-of-scope result".into(),
                ));
            }
            if out.documents.len() == 200 {
                out.truncated = true;
                return Ok(out);
            }
            out.documents.push(Document {
                id: bounded(&node.id, 128),
                title: bounded(&node.title, 512),
                url: bounded(&node.url, 2048),
                updated_at: bounded(&node.updated, 64),
                excerpt: if excerpt {
                    node.content.as_deref().map(|s| bounded(s, 2048))
                } else {
                    None
                },
            });
        }
        if !page.documents.page_info.has_next {
            return Ok(out);
        }
        let cursor = page
            .documents
            .page_info
            .cursor
            .filter(|s| !s.is_empty())
            .ok_or_else(|| TrackerError::Other("documents pagination missing cursor".into()))?;
        if !cursors.insert(cursor.clone()) {
            return Err(TrackerError::Other(
                "documents pagination repeated cursor".into(),
            ));
        }
        after = Some(cursor);
    }
    out.truncated = true;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use crate::Tracker;
    use crate::linear::testutil::{MockResp, new_test_client};

    #[tokio::test]
    async fn tracker_documents_project_lookup_is_scoped_paginated_and_read_only() {
        let (client, _server) = new_test_client(|req| {
            assert!(req.query.starts_with("query ManagerDocuments"));
            assert!(!req.query.contains("mutation"));
            assert_eq!(req.var("filter").unwrap()["project"]["slugId"]["eq"], "proj");
            assert_eq!(req.var("filter").unwrap()["title"]["containsIgnoreCase"], "plugins");
            assert_eq!(req.var("excerpt").unwrap(), true);
            let first = req.var("after").unwrap().is_null();
            MockResp::ok(serde_json::json!({"data":{"documents":{"nodes":[{"id":if first {"one"} else {"two"},"title":if first {Some("Plugins")} else {None},"url":"https://linear.app/document/plugins","updatedAt":"2026-08-29T00:00:00Z","content":"x".repeat(3000),"project":{"slugId":"proj"}}],"pageInfo":{"hasNextPage":first,"endCursor":"next"}}}}).to_string())
        }).await;
        let got = client.fetch_documents(None, "plugins", true).await.unwrap();
        assert_eq!(got.documents.len(), 2);
        assert_eq!(got.documents[0].updated_at, "2026-08-29T00:00:00Z");
        assert_eq!(got.documents[0].excerpt.as_ref().unwrap().len(), 2048);
        assert!(got.documents[1].title.is_empty());
        assert!(!got.truncated);
    }

    #[tokio::test]
    async fn tracker_documents_issue_lookup_checks_project_and_attachment_membership() {
        for (project, attached, accepted) in [
            ("proj", "uuid-598", true),
            ("other", "uuid-598", false),
            ("proj", "other-issue", false),
        ] {
            let (client, _server) = new_test_client(move |req| {
                if req.query.contains("query LeadIssue") {
                    return MockResp::ok(serde_json::json!({"data":{"issue":{"id":"uuid-598","identifier":"TEST-598","project":{"slugId":project}}}}).to_string());
                }
                assert!(req.query.starts_with("query ManagerDocuments"));
                assert_eq!(req.var("filter").unwrap()["issue"]["id"]["eq"], "uuid-598");
                assert_eq!(req.var("excerpt").unwrap(), false);
                MockResp::ok(serde_json::json!({"data":{"documents":{"nodes":[{"id":"doc","title":"Design","url":"https://linear.app/document/design","updatedAt":null,"content":null,"issue":{"id":attached}}],"pageInfo":{"hasNextPage":false,"endCursor":null}}}}).to_string())
            }).await;
            let got = client.fetch_documents(Some("TEST-598"), "", false).await;
            assert_eq!(got.is_ok(), accepted, "{project}/{attached}");
            if let Ok(got) = got {
                assert!(got.documents[0].excerpt.is_none());
                assert!(got.documents[0].updated_at.is_empty());
            }
        }
    }

    #[tokio::test]
    async fn tracker_documents_pagination_and_gateway_failures_never_claim_absence() {
        for mode in ["missing", "repeated", "foreign", "gateway", "capped"] {
            let (client, _server) = new_test_client(move |req| {
                if mode == "gateway" { return MockResp::ok(r#"{"errors":[{"message":"unavailable"}]}"#); }
                let after = req.var("after").unwrap();
                let cursor = if mode == "missing" { None } else if mode == "capped" { Some(format!("next-{after}")) } else { Some("repeated".into()) };
                MockResp::ok(serde_json::json!({"data":{"documents":{"nodes":[{"id":"doc","title":"Design","url":"u","updatedAt":"d","project":{"slugId":if mode == "foreign" { "other" } else {"proj"}}}],"pageInfo":{"hasNextPage":true,"endCursor":cursor}}}}).to_string())
            }).await;
            let got = client.fetch_documents(None, "", false).await;
            if mode == "capped" {
                assert!(got.unwrap().truncated);
            } else {
                assert!(got.is_err(), "{mode}");
            }
        }
    }
}
