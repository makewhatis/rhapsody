//! Blocked-backlog + branch reads — parity port of `internal/tracker/linear/backlog.go`
//! (INF-318).
//!
//! [`fetch_blocked_backlog_issues`] returns fully-normalized Backlog-state issues (BlockedBy
//! populated) for the configured project, assigned to the API key owner — the DAG auto-promote
//! read. Backlog is selected by Linear state TYPE ("backlog"), which is config-free. It applies the
//! SAME milestone filter as [`fetch_candidate_issues`](super::candidates::fetch_candidate_issues)
//! so the two fetches see the same set (no stranding). [`fetch_issue_branch_by_id`] returns an
//! issue's `gitBranchName` + best-effort linked-PR number for the graphite stacking hint (advisory:
//! a missing issue returns `("", 0)`, never an error).

use super::candidates::{IssuesPage, MAX_PAGES, resolve_milestone_id};
use super::client::{resolve_viewer, traced};
use super::{Client, LinearError, LinearErrorKind, query};
use crate::TrackerError;
use rhapsody_core::Issue;
use serde_json::Value;

/// FetchBlockedBacklogIssues returns fully-normalized Backlog-state issues (BlockedBy populated)
/// for the configured project, assigned to the API key owner, following pagination
/// (backlog.go's `FetchBlockedBacklogIssues`).
pub(super) async fn fetch_blocked_backlog_issues(c: &Client) -> Result<Vec<Issue>, TrackerError> {
    traced(crate::tracker_span!("fetch_blocked_backlog"), async move {
        // Like the candidate fetch, narrow to the API key owner's assigned issues. Resolve (and
        // cache) the viewer first; a resolution failure fails the whole fetch.
        let viewer = resolve_viewer(c).await?;
        // Apply the SAME milestone filter as FetchCandidateIssues so both fetches see the same set.
        let milestone_id = if c.config.milestone.is_empty() {
            None
        } else {
            Some(resolve_milestone_id(c).await?)
        };
        let query = query::query_backlog_candidates(milestone_id.is_some());
        let mut out: Vec<Issue> = Vec::new();
        let mut after: Option<String> = None;
        let mut pages: u32 = 0;
        loop {
            pages += 1;
            if pages > MAX_PAGES {
                return Err(LinearError::new(
                    LinearErrorKind::MissingCursor,
                    format!("exceeded {MAX_PAGES} backlog pages without completing pagination"),
                )
                .into());
            }
            let mut vars = serde_json::Map::new();
            vars.insert(
                "projectSlug".into(),
                Value::from(c.config.project_slug.clone()),
            );
            vars.insert("assigneeID".into(), Value::from(viewer.id.clone()));
            vars.insert("first".into(), Value::from(c.page_size));
            vars.insert("after".into(), Value::from(after.clone()));
            if let Some(id) = &milestone_id {
                vars.insert("milestoneID".into(), Value::from(id.clone()));
            }
            let page: IssuesPage = c.do_graphql(&query, Some(Value::Object(vars))).await?;
            for n in page.issues.nodes {
                out.push(c.normalize_issue(n));
            }
            if !page.issues.page_info.has_next_page {
                return Ok(out);
            }
            if page.issues.page_info.end_cursor.is_empty() {
                return Err(LinearError::bare(LinearErrorKind::MissingCursor).into());
            }
            after = Some(page.issues.page_info.end_cursor);
        }
    })
    .await
}

/// FetchIssueBranchByID returns the issue's Linear `gitBranchName` and, best-effort, its linked
/// GitHub PR number. A missing issue (or empty id) returns `("", 0)` — the stacking hint is
/// advisory, so a not-found predecessor must never fail the dependent's run (backlog.go's
/// `FetchIssueBranchByID`).
pub(super) async fn fetch_issue_branch_by_id(
    c: &Client,
    id: &str,
) -> Result<(String, i64), TrackerError> {
    if id.is_empty() {
        return Ok((String::new(), 0));
    }
    traced(crate::tracker_span!("fetch_issue_branch"), async move {
        let vars = serde_json::json!({ "ids": [id], "first": 1 });
        let page: super::by_ids::IdsPage =
            c.do_graphql(query::QUERY_BRANCH_BY_ID, Some(vars)).await?;
        let Some(n) = page.issues.nodes.into_iter().next() else {
            return Ok((String::new(), 0));
        };
        let branch = n.git_branch_name().unwrap_or_default().to_string();
        let pr = super::normalize::pr_number_from_attachments(&n);
        Ok((branch, pr))
    })
    .await
}

#[cfg(test)]
mod tests {
    use crate::Tracker;
    use crate::linear::testutil::{MockResp, MockServer, new_test_client};
    use crate::linear::{Config, new};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    // Mirrors Go TestFetchBlockedBacklogIssues.
    #[tokio::test]
    async fn fetch_blocked_backlog_issues() {
        let flags = Arc::new(Mutex::new((false, false, false))); // type, assignee, inverse
        let flags_h = Arc::clone(&flags);
        let server = MockServer::start_with_viewer(move |req| {
            let mut f = flags_h.lock().expect("flags");
            if req.query.contains(r#"state: { type: { eq: "backlog" } }"#) {
                f.0 = true;
            }
            if req.query.contains("assignee: { id: { eq: $assigneeID } }") {
                f.1 = true;
            }
            if req.query.contains("inverseRelations") {
                f.2 = true;
            }
            drop(f);
            MockResp::ok(
                r#"{"data":{"issues":{"nodes":[
                    {"id":"b1","identifier":"MT-2","title":"dependent","state":{"name":"Backlog"},
                     "branchName":"feat/mt-2","team":{"id":"team-1"},
                     "inverseRelations":{"nodes":[{"type":"blocks","issue":{"id":"a1","identifier":"MT-1","state":{"name":"In Review"}}}]}}
                ],"pageInfo":{"hasNextPage":false,"endCursor":"x"}}}}"#,
            )
        })
        .await;
        let c = new(Config {
            endpoint: server.url(),
            api_key: "k".into(),
            project_slug: "proj".into(),
            ..Config::default()
        });
        let got = c.fetch_blocked_backlog_issues().await.expect("fetch");
        let (saw_type, saw_assignee, saw_inverse) = *flags.lock().expect("flags");
        assert!(saw_type, "must filter by state TYPE backlog");
        assert!(saw_assignee, "must filter by the viewer assignee");
        assert!(saw_inverse, "must select inverseRelations (blocker edges)");
        assert_eq!(got.len(), 1);
        let iss = &got[0];
        assert_eq!(iss.id, "b1");
        assert_eq!(iss.state, "Backlog");
        let blockers = iss.blocked_by.as_deref().unwrap_or_default();
        assert_eq!(blockers.len(), 1, "BlockedBy populated");
        assert_eq!(blockers[0].id.as_deref(), Some("a1"));
        assert_eq!(blockers[0].state.as_deref(), Some("In Review"));
    }

    // Mirrors Go TestFetchBlockedBacklogIssuesAppliesMilestone.
    #[tokio::test]
    async fn fetch_blocked_backlog_issues_applies_milestone() {
        let saw_ms = Arc::new(AtomicBool::new(false));
        let saw_ms_h = Arc::clone(&saw_ms);
        let saw_var = Arc::new(AtomicBool::new(false));
        let saw_var_h = Arc::clone(&saw_var);
        let server = MockServer::start_with_viewer(move |req| {
            if req.query.contains("projectMilestones(") {
                saw_ms_h.store(true, Ordering::SeqCst);
                return MockResp::ok(
                    r#"{"data":{"projectMilestones":{"nodes":[{"id":"ms-2","name":"v2.0"}],"pageInfo":{"hasNextPage":false,"endCursor":"x"}}}}"#,
                );
            }
            if req.var_str("milestoneID") == Some("ms-2")
                && req
                    .query
                    .contains("projectMilestone: { id: { eq: $milestoneID } }")
                && req.query.contains(r#"state: { type: { eq: "backlog" } }"#)
            {
                saw_var_h.store(true, Ordering::SeqCst);
            }
            MockResp::ok(r#"{"data":{"issues":{"nodes":[],"pageInfo":{"hasNextPage":false,"endCursor":"x"}}}}"#)
        })
        .await;
        let c = new(Config {
            endpoint: server.url(),
            api_key: "k".into(),
            project_slug: "proj".into(),
            milestone: "v2.0".into(),
            ..Config::default()
        });
        c.fetch_blocked_backlog_issues().await.expect("fetch");
        assert!(
            saw_ms.load(Ordering::SeqCst),
            "expected a projectMilestones resolution query"
        );
        assert!(
            saw_var.load(Ordering::SeqCst),
            "backlog query must carry milestoneID + the projectMilestone filter (parity with candidates)"
        );
    }

    // Mirrors Go TestFetchIssueBranchByID.
    #[tokio::test]
    async fn fetch_issue_branch_by_id() {
        let ok = Arc::new(AtomicBool::new(false));
        let ok_h = Arc::clone(&ok);
        let (c, _server) = new_test_client(move |req| {
            if req.query.contains("[ID!]") && req.query.contains("branchName") {
                ok_h.store(true, Ordering::SeqCst);
            }
            MockResp::ok(
                r#"{"data":{"issues":{"nodes":[
                    {"id":"a1","branchName":"feat/mt-1","attachments":{"nodes":[
                        {"sourceType":"github","metadata":{"url":"https://github.com/o/r/pull/42"}}
                    ]}}
                ]}}}"#,
            )
        })
        .await;
        let (branch, pr) = c.fetch_issue_branch_by_id("a1").await.expect("branch");
        assert!(
            ok.load(Ordering::SeqCst),
            "branch query must declare ids [ID!] and select branchName"
        );
        assert_eq!(branch, "feat/mt-1");
        assert_eq!(pr, 42);
    }

    // Mirrors Go TestFetchIssueBranchByIDEdgeCases.
    #[tokio::test]
    async fn fetch_issue_branch_by_id_edge_cases() {
        let called = Arc::new(AtomicBool::new(false));
        let called_h = Arc::clone(&called);
        let (c, _server) = new_test_client(move |_req| {
            called_h.store(true, Ordering::SeqCst);
            MockResp::ok(r#"{"data":{"issues":{"nodes":[]}}}"#)
        })
        .await;
        // Empty id short-circuits with no HTTP call.
        let (branch, pr) = c.fetch_issue_branch_by_id("").await.expect("empty id");
        assert_eq!((branch.as_str(), pr), ("", 0));
        assert!(
            !called.load(Ordering::SeqCst),
            "empty id should make no call"
        );
        // A not-found id returns ("", 0).
        let (branch, pr) = c
            .fetch_issue_branch_by_id("missing")
            .await
            .expect("missing");
        assert_eq!((branch.as_str(), pr), ("", 0));
    }
}
