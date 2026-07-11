//! Pool-mode claim writes + reads — parity port of `internal/tracker/linear/claim.go` (INF-477).
//!
//! The shared-pool claim protocol: [`assign_issue`] sets the durable assignee lock (last-write-wins
//! `issueUpdate(assigneeId)`, no CAS), and [`fetch_issue_assignee`] is the read-back gate the
//! election winner uses to confirm it holds the claim uncontested. [`create_comment`] /
//! [`list_comments`] / [`delete_comment`] cast, tally, and clean up claim-marker comments;
//! `list_comments` follows the comment connection cursor to completion so a busy ticket's claim
//! markers are never truncated (electing the wrong winner). A `success: false` response is
//! [`MoveRejected`](LinearErrorKind::MoveRejected); a `hasNextPage` with an empty cursor is
//! [`MissingCursor`](LinearErrorKind::MissingCursor).

use super::candidates::{MAX_PAGES, PageInfo};
use super::client::traced;
use super::normalize::parse_time;
use super::{Client, LinearError, LinearErrorKind, query};
use crate::TrackerError;
use chrono::{DateTime, Utc};
use rhapsody_core::Comment;
use serde::Deserialize;
use serde_json::json;
use std::time::UNIX_EPOCH;

/// The `issueUpdate { success }` envelope (the assign mutation; move uses its own copy).
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct IssueUpdateResp {
    #[serde(rename = "issueUpdate")]
    issue_update: Success,
}

/// The `commentDelete { success }` envelope.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct CommentDeleteResp {
    #[serde(rename = "commentDelete")]
    comment_delete: Success,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Success {
    success: bool,
}

/// The `commentCreate { success comment { id } }` envelope. The mutation also selects `createdAt`,
/// which Go decodes but never uses; serde ignores it (only `id` is read here).
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct CommentCreateResp {
    #[serde(rename = "commentCreate")]
    comment_create: CommentCreateNode,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct CommentCreateNode {
    success: bool,
    comment: CommentIdNode,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct CommentIdNode {
    id: String,
}

/// The `issue { assignee { id } }` envelope. `issue` is `Option` (a missing issue is `null`), as is
/// `assignee` (unassigned) — either absence resolves to the "" assignee id.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct AssigneeResp {
    issue: Option<AssigneeIssue>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct AssigneeIssue {
    assignee: Option<AssigneeUser>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct AssigneeUser {
    id: String,
}

/// The `issue { comments { nodes { id body createdAt } pageInfo } }` envelope. `issue` is `Option`
/// (a missing issue yields no comments).
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct IssueCommentsResp {
    issue: Option<CommentsIssue>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct CommentsIssue {
    comments: CommentsConn,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct CommentsConn {
    nodes: Vec<CommentNode>,
    #[serde(rename = "pageInfo")]
    page_info: PageInfo,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct CommentNode {
    id: String,
    body: String,
    #[serde(rename = "createdAt")]
    created_at: String,
}

/// AssignIssue sets an issue's assignee via `issueUpdate(assigneeId)` — the durable pool-mode lock
/// (claim.go's `AssignIssue`). Last-write-wins, no conditional form, so the caller re-reads via
/// [`fetch_issue_assignee`]. A `success: false` response is
/// [`MoveRejected`](LinearErrorKind::MoveRejected).
pub(super) async fn assign_issue(
    c: &Client,
    issue_id: &str,
    assignee_id: &str,
) -> Result<(), TrackerError> {
    traced(crate::tracker_span!("assign_issue"), async move {
        if issue_id.is_empty() || assignee_id.is_empty() {
            return Err(LinearError::new(
                LinearErrorKind::ApiRequest,
                format!(
                    "assign requires issueID and assigneeID (got {issue_id:?},{assignee_id:?})"
                ),
            )
            .into());
        }
        let vars = json!({ "id": issue_id, "assigneeId": assignee_id });
        let resp: IssueUpdateResp = c
            .do_graphql(query::MUTATION_ISSUE_ASSIGN, Some(vars))
            .await?;
        if !resp.issue_update.success {
            return Err(LinearError::new(
                LinearErrorKind::MoveRejected,
                format!("assign issue {issue_id} -> {assignee_id}"),
            )
            .into());
        }
        Ok(())
    })
    .await
}

/// FetchIssueAssignee returns an issue's current assignee user ID ("" when unassigned or the issue
/// is missing) — the pool-mode read-back gate (claim.go's `FetchIssueAssignee`). Deliberately
/// separate from the by-ids state read, whose staleness contract other callers rely on.
pub(super) async fn fetch_issue_assignee(
    c: &Client,
    issue_id: &str,
) -> Result<String, TrackerError> {
    traced(crate::tracker_span!("fetch_issue_assignee"), async move {
        if issue_id.is_empty() {
            return Err(LinearError::new(
                LinearErrorKind::ApiRequest,
                "assignee read requires issueID",
            )
            .into());
        }
        let resp: AssigneeResp = c
            .do_graphql(query::QUERY_ISSUE_ASSIGNEE, Some(json!({ "id": issue_id })))
            .await?;
        Ok(resp
            .issue
            .and_then(|i| i.assignee)
            .map(|a| a.id)
            .unwrap_or_default())
    })
    .await
}

/// CreateComment posts a comment and returns the server-assigned id — used to cast a pool-mode
/// claim (claim.go's `CreateComment`). A `success: false` response OR an empty id is
/// [`MoveRejected`](LinearErrorKind::MoveRejected) (the caller treats a failed comment as a failed
/// claim and skips the tick).
pub(super) async fn create_comment(
    c: &Client,
    issue_id: &str,
    body: &str,
) -> Result<String, TrackerError> {
    traced(crate::tracker_span!("create_comment"), async move {
        if issue_id.is_empty() || body.is_empty() {
            return Err(LinearError::new(
                LinearErrorKind::ApiRequest,
                format!(
                    "comment requires issueID and body (got {issue_id:?}, len(body)={})",
                    body.len()
                ),
            )
            .into());
        }
        let vars = json!({ "issueId": issue_id, "body": body });
        let resp: CommentCreateResp = c
            .do_graphql(query::MUTATION_COMMENT_CREATE, Some(vars))
            .await?;
        if !resp.comment_create.success || resp.comment_create.comment.id.is_empty() {
            return Err(LinearError::new(
                LinearErrorKind::MoveRejected,
                format!(
                    "comment on issue {issue_id} (success={}, id={:?})",
                    resp.comment_create.success, resp.comment_create.comment.id
                ),
            )
            .into());
        }
        Ok(resp.comment_create.comment.id)
    })
    .await
}

/// ListComments returns all of an issue's comments (id, body, createdAt) for the pool-mode claim
/// election (claim.go's `ListComments`). It follows the comment connection cursor to completion so
/// the election sees every claim marker regardless of Linear's page ordering; a missing issue
/// yields no comments. A `hasNextPage` with an empty cursor is
/// [`MissingCursor`](LinearErrorKind::MissingCursor) — not a silent truncation that could drop a
/// claim marker.
pub(super) async fn list_comments(
    c: &Client,
    issue_id: &str,
) -> Result<Vec<Comment>, TrackerError> {
    traced(crate::tracker_span!("list_comments"), async move {
        if issue_id.is_empty() {
            return Err(
                LinearError::new(LinearErrorKind::ApiRequest, "list comments requires issueID")
                    .into(),
            );
        }
        let mut out: Vec<Comment> = Vec::new();
        let mut after: Option<String> = None;
        let mut pages: u32 = 0;
        loop {
            pages += 1;
            if pages > MAX_PAGES {
                return Err(LinearError::new(
                    LinearErrorKind::MissingCursor,
                    format!(
                        "exceeded {MAX_PAGES} comment pages for issue {issue_id} without completing pagination"
                    ),
                )
                .into());
            }
            let vars = json!({ "id": issue_id, "first": c.page_size, "after": after });
            let resp: IssueCommentsResp = c.do_graphql(query::QUERY_ISSUE_COMMENTS, Some(vars)).await?;
            let Some(issue) = resp.issue else {
                return Ok(out);
            };
            for n in issue.comments.nodes {
                out.push(Comment {
                    id: n.id,
                    body: n.body,
                    // Go leaves core.Comment.CreatedAt as the zero time when the timestamp is
                    // unparseable; the field is non-optional here, so fall back to the Unix epoch.
                    // No stub/test produces an unparseable comment timestamp — parse_time (the
                    // parseTime mirror) supplies every real value; this is purely defensive.
                    created_at: parse_time(Some(&n.created_at))
                        .unwrap_or_else(|| DateTime::<Utc>::from(UNIX_EPOCH)),
                });
            }
            if !issue.comments.page_info.has_next_page {
                return Ok(out);
            }
            if issue.comments.page_info.end_cursor.is_empty() {
                return Err(LinearError::bare(LinearErrorKind::MissingCursor).into());
            }
            after = Some(issue.comments.page_info.end_cursor);
        }
    })
    .await
}

/// DeleteComment removes a comment by id (claim-comment cleanup; claim.go's `DeleteComment`). A
/// `success: false` response is [`MoveRejected`](LinearErrorKind::MoveRejected).
pub(super) async fn delete_comment(c: &Client, comment_id: &str) -> Result<(), TrackerError> {
    traced(crate::tracker_span!("delete_comment"), async move {
        if comment_id.is_empty() {
            return Err(
                LinearError::new(LinearErrorKind::ApiRequest, "delete requires commentID").into(),
            );
        }
        let resp: CommentDeleteResp = c
            .do_graphql(
                query::MUTATION_COMMENT_DELETE,
                Some(json!({ "id": comment_id })),
            )
            .await?;
        if !resp.comment_delete.success {
            return Err(LinearError::new(
                LinearErrorKind::MoveRejected,
                format!("delete comment {comment_id}"),
            )
            .into());
        }
        Ok(())
    })
    .await
}

#[cfg(test)]
mod tests {
    use crate::Tracker;
    use crate::TrackerError;
    use crate::linear::testutil::{MockResp, MockServer};
    use crate::linear::{Config, LinearErrorKind, new};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    fn client_at(url: String) -> crate::linear::Client {
        new(Config {
            endpoint: url,
            api_key: "k".into(),
            project_slug: "proj".into(),
            ..Config::default()
        })
    }

    fn is_kind(err: &TrackerError, kind: LinearErrorKind) -> bool {
        matches!(err, TrackerError::Linear(e) if e.kind == kind)
    }

    // Mirrors Go TestAssignIssue: AssignIssue sends issueUpdate(assigneeId) and reports success.
    #[tokio::test]
    async fn assign_issue() {
        let vars = Arc::new(Mutex::new((String::new(), String::new())));
        let vars_h = Arc::clone(&vars);
        let saw_mutation = Arc::new(Mutex::new(false));
        let saw_h = Arc::clone(&saw_mutation);
        let server = MockServer::start(move |req| {
            *saw_h.lock().expect("saw") = req
                .query
                .contains("issueUpdate(id: $id, input: { assigneeId: $assigneeId })");
            *vars_h.lock().expect("vars") = (
                req.var_str("id").unwrap_or_default().to_owned(),
                req.var_str("assigneeId").unwrap_or_default().to_owned(),
            );
            MockResp::ok(r#"{"data":{"issueUpdate":{"success":true}}}"#)
        })
        .await;
        let c = client_at(server.url());

        c.assign_issue("iss-1", "user-9")
            .await
            .expect("AssignIssue");
        assert!(*saw_mutation.lock().expect("saw"), "assign mutation text");
        assert_eq!(
            *vars.lock().expect("vars"),
            ("iss-1".to_owned(), "user-9".to_owned()),
            "assign vars"
        );
    }

    // Mirrors Go TestAssignIssueRejected: a success:false response is a rejection; empty args are
    // rejected before any request.
    #[tokio::test]
    async fn assign_issue_rejected() {
        let server =
            MockServer::start(|_req| MockResp::ok(r#"{"data":{"issueUpdate":{"success":false}}}"#))
                .await;
        let c = client_at(server.url());
        c.assign_issue("iss-1", "user-9")
            .await
            .expect_err("success:false must error");

        // Empty args are rejected before any request (no server needed).
        let c2 = client_at("http://127.0.0.1:1".into());
        c2.assign_issue("", "u")
            .await
            .expect_err("empty issueID must error");
    }

    // Mirrors Go TestFetchIssueAssignee: returns the assignee id, and "" when unassigned/missing.
    #[tokio::test]
    async fn fetch_issue_assignee() {
        let server = MockServer::start(|_req| {
            MockResp::ok(r#"{"data":{"issue":{"assignee":{"id":"user-7"}}}}"#)
        })
        .await;
        let c = client_at(server.url());
        assert_eq!(
            c.fetch_issue_assignee("iss-1").await.expect("assignee"),
            "user-7"
        );

        let nil =
            MockServer::start(|_req| MockResp::ok(r#"{"data":{"issue":{"assignee":null}}}"#)).await;
        let c_nil = client_at(nil.url());
        assert_eq!(
            c_nil
                .fetch_issue_assignee("iss-1")
                .await
                .expect("unassigned"),
            "",
            "unassigned read should be empty"
        );
    }

    // Mirrors Go TestCreateComment: returns the server comment id; a success:false / empty id errors.
    #[tokio::test]
    async fn create_comment() {
        let vars = Arc::new(Mutex::new((String::new(), String::new())));
        let vars_h = Arc::clone(&vars);
        let server = MockServer::start(move |req| {
            *vars_h.lock().expect("vars") = (
                req.var_str("issueId").unwrap_or_default().to_owned(),
                req.var_str("body").unwrap_or_default().to_owned(),
            );
            MockResp::ok(
                r#"{"data":{"commentCreate":{"success":true,"comment":{"id":"c-42","createdAt":"2026-07-07T12:00:00.000Z"}}}}"#,
            )
        })
        .await;
        let c = client_at(server.url());
        assert_eq!(
            c.create_comment("iss-1", "claim body")
                .await
                .expect("CreateComment"),
            "c-42"
        );
        assert_eq!(
            *vars.lock().expect("vars"),
            ("iss-1".to_owned(), "claim body".to_owned()),
            "comment vars"
        );

        let fail = MockServer::start(|_req| {
            MockResp::ok(r#"{"data":{"commentCreate":{"success":false,"comment":{"id":""}}}}"#)
        })
        .await;
        let c_fail = client_at(fail.url());
        c_fail
            .create_comment("iss-1", "b")
            .await
            .expect_err("success:false comment must error");
    }

    // Mirrors Go TestListComments: normalizes nodes into core.Comment with parsed, ordered createdAt.
    #[tokio::test]
    async fn list_comments() {
        let id_var = Arc::new(Mutex::new(String::new()));
        let id_h = Arc::clone(&id_var);
        let server = MockServer::start(move |req| {
            *id_h.lock().expect("id") = req.var_str("id").unwrap_or_default().to_owned();
            MockResp::ok(
                r#"{"data":{"issue":{"comments":{"nodes":[
                    {"id":"c1","body":"first","createdAt":"2026-07-07T10:00:00.000Z"},
                    {"id":"c2","body":"second","createdAt":"2026-07-07T10:00:05.000Z"}
                ],"pageInfo":{"hasNextPage":false,"endCursor":""}}}}}"#,
            )
        })
        .await;
        let c = client_at(server.url());

        let cs = c.list_comments("iss-1").await.expect("ListComments");
        assert_eq!(*id_var.lock().expect("id"), "iss-1", "list vars");
        assert_eq!(cs.len(), 2);
        assert_eq!(cs[0].id, "c1");
        assert_eq!(cs[1].body, "second");
        assert!(
            cs[0].created_at < cs[1].created_at,
            "createdAt parsed and ordered: {:?} vs {:?}",
            cs[0].created_at,
            cs[1].created_at
        );
    }

    // Mirrors Go TestListCommentsPaginates: follows the comment connection cursor across pages.
    #[tokio::test]
    async fn list_comments_paginates() {
        let pages = Arc::new(AtomicUsize::new(0));
        let pages_h = Arc::clone(&pages);
        let server = MockServer::start(move |req| {
            pages_h.fetch_add(1, Ordering::SeqCst);
            match req.var_str("after") {
                None => MockResp::ok(
                    r#"{"data":{"issue":{"comments":{"nodes":[
                        {"id":"c1","body":"first","createdAt":"2026-07-07T10:00:00.000Z"}
                    ],"pageInfo":{"hasNextPage":true,"endCursor":"CUR1"}}}}}"#,
                ),
                Some("CUR1") => MockResp::ok(
                    r#"{"data":{"issue":{"comments":{"nodes":[
                        {"id":"c2","body":"second","createdAt":"2026-07-07T10:00:05.000Z"},
                        {"id":"c3","body":"third","createdAt":"2026-07-07T10:00:10.000Z"}
                    ],"pageInfo":{"hasNextPage":false,"endCursor":"CUR2"}}}}}"#,
                ),
                Some(other) => MockResp::ok(format!(
                    r#"{{"errors":[{{"message":"unexpected after cursor {other}"}}]}}"#
                )),
            }
        })
        .await;
        let c = client_at(server.url());

        let cs = c.list_comments("iss-1").await.expect("ListComments");
        assert_eq!(pages.load(Ordering::SeqCst), 2, "expected 2 pages fetched");
        let ids: Vec<&str> = cs.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(
            ids,
            ["c1", "c2", "c3"],
            "pagination did not accumulate all comments"
        );
    }

    // Mirrors Go TestListCommentsMissingCursor: hasNextPage:true with an empty endCursor is a
    // pagination-integrity error, not a silent truncation.
    #[tokio::test]
    async fn list_comments_missing_cursor() {
        let server = MockServer::start(|_req| {
            MockResp::ok(
                r#"{"data":{"issue":{"comments":{"nodes":[
                    {"id":"c1","body":"first","createdAt":"2026-07-07T10:00:00.000Z"}
                ],"pageInfo":{"hasNextPage":true,"endCursor":""}}}}}"#,
            )
        })
        .await;
        let c = client_at(server.url());
        let err = c
            .list_comments("iss-1")
            .await
            .expect_err("missing cursor must error");
        assert!(
            is_kind(&err, LinearErrorKind::MissingCursor),
            "got {err:?}, want MissingCursor"
        );
    }

    // Mirrors Go TestDeleteComment: reports success and rejects an empty id before any request.
    #[tokio::test]
    async fn delete_comment() {
        let id_var = Arc::new(Mutex::new(String::new()));
        let id_h = Arc::clone(&id_var);
        let server = MockServer::start(move |req| {
            *id_h.lock().expect("id") = req.var_str("id").unwrap_or_default().to_owned();
            MockResp::ok(r#"{"data":{"commentDelete":{"success":true}}}"#)
        })
        .await;
        let c = client_at(server.url());

        c.delete_comment("c-1").await.expect("DeleteComment");
        assert_eq!(*id_var.lock().expect("id"), "c-1", "delete vars");

        let c2 = client_at("http://127.0.0.1:1".into());
        c2.delete_comment("")
            .await
            .expect_err("empty commentID must error");
    }
}
