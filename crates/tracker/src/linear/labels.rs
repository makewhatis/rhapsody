//! Rhapsody Teams label surface (STUDIO-644; Rhapsody-only, no Go v0.4.0 counterpart).
//!
//! Two additive [`Tracker`](crate::Tracker) methods, and both exist because §0.11.1 of the design
//! record (`~/.rhapsody/docs/STUDIO-572-rhapsody-teams.md`) made the `rhapsody:@<identity>` LABEL
//! the assignment — replacing the assignee-field write the adversarial review found unbuildable:
//!
//! * [`add_issue_label`] — find-or-create the label in the issue's team, then ADD it. Additive at
//!   every layer: `issueAddLabel` (not `issueUpdate(labelIds:)`, which replaces the set), and an
//!   issue that already carries the label is a successful no-op. Nothing here can remove a label a
//!   human wrote, which is the mechanical form of §0.11.1's human-conflict rule.
//! * [`fetch_open_issues_by_labels`] — the per-identity load read: open (non-terminal) issues
//!   carrying any of the roster's labels, id + identifier + labels only, paginated.
//!
//! "Open" is expressed as a Linear state TYPE exclusion (`completed`/`canceled`), config-free for
//! the same reason `fetch_blocked_backlog_issues` selects Backlog by type: state NAMES vary per
//! workspace, types do not.

use super::candidates::{MAX_PAGES, PageInfo};
use super::client::traced;
use super::{Client, LinearError, LinearErrorKind, query};
use crate::TrackerError;
use rhapsody_core::{Issue, normalize_state};
use serde::Deserialize;
use serde_json::{Value, json};

/// The `issueLabels { nodes { id name team { id } } }` envelope.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct LabelsResp {
    #[serde(rename = "issueLabels")]
    issue_labels: LabelsConn,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct LabelsConn {
    nodes: Vec<LabelNode>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct LabelNode {
    #[serde(deserialize_with = "super::decode::null_to_empty")]
    id: String,
    #[serde(deserialize_with = "super::decode::null_to_empty")]
    name: String,
    /// `null` for a workspace-level label (one that belongs to no team).
    team: Option<LabelTeam>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct LabelTeam {
    #[serde(deserialize_with = "super::decode::null_to_empty")]
    id: String,
}

/// The `issueLabelCreate { success issueLabel { id } }` envelope.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct LabelCreateResp {
    #[serde(rename = "issueLabelCreate")]
    issue_label_create: LabelCreateNode,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct LabelCreateNode {
    success: bool,
    #[serde(rename = "issueLabel")]
    issue_label: LabelIdNode,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct LabelIdNode {
    #[serde(deserialize_with = "super::decode::null_to_empty")]
    id: String,
}

/// The `issueAddLabel { success }` envelope.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct AddLabelResp {
    #[serde(rename = "issueAddLabel")]
    issue_add_label: SuccessNode,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SuccessNode {
    success: bool,
}

/// The lean `issues { nodes { id identifier labels } pageInfo }` envelope the load read uses.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct LabelledIssuesResp {
    issues: LabelledIssuesConn,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct LabelledIssuesConn {
    nodes: Vec<LabelledIssueNode>,
    #[serde(rename = "pageInfo")]
    page_info: PageInfo,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct LabelledIssueNode {
    #[serde(deserialize_with = "super::decode::null_to_empty")]
    id: String,
    #[serde(deserialize_with = "super::decode::null_to_empty")]
    identifier: String,
    labels: LabelNamesConn,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct LabelNamesConn {
    nodes: Vec<LabelNameNode>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct LabelNameNode {
    #[serde(deserialize_with = "super::decode::null_to_empty")]
    name: String,
}

/// How many same-named labels one lookup page reads. A name is expected to resolve to one label
/// (occasionally two: a team-scoped and a workspace-level one), so this is generous headroom, not
/// a pagination story.
const LABEL_LOOKUP_PAGE: i64 = 50;

/// AddIssueLabel find-or-creates `label_name` in `team_id` and ADDS it to the issue (STUDIO-644,
/// design §0.11.1). Empty arguments are [`ApiRequest`](LinearErrorKind::ApiRequest); a
/// `success: false` from either mutation is [`MoveRejected`](LinearErrorKind::MoveRejected).
///
/// The add is `issueAddLabel`, which is additive server-side — it can never drop a label the issue
/// already carries, so a human editing labels concurrently cannot lose their edit to this write.
/// Re-adding a label the issue already has is a successful no-op in Linear, which makes the whole
/// operation idempotent without a read-back.
pub(super) async fn add_issue_label(
    c: &Client,
    issue_id: &str,
    team_id: &str,
    label_name: &str,
) -> Result<(), TrackerError> {
    traced(crate::tracker_span!("add_issue_label"), async move {
        if issue_id.is_empty() || team_id.is_empty() || label_name.is_empty() {
            return Err(LinearError::new(
                LinearErrorKind::ApiRequest,
                format!(
                    "add label requires issueID, teamID and labelName (got {issue_id:?},{team_id:?},{label_name:?})"
                ),
            )
            .into());
        }
        let label_id = resolve_or_create_label(c, team_id, label_name).await?;
        let resp: AddLabelResp = c
            .do_graphql(
                query::MUTATION_ISSUE_ADD_LABEL,
                Some(json!({ "id": issue_id, "labelId": label_id })),
            )
            .await?;
        if !resp.issue_add_label.success {
            return Err(LinearError::new(
                LinearErrorKind::MoveRejected,
                format!("add label {label_name:?} to issue {issue_id}"),
            )
            .into());
        }
        Ok(())
    })
    .await
}

/// The find-or-create half: look the name up, preferring a label already scoped to `team_id`, then
/// a workspace-level label of the same name (an operator may well have created `rhapsody:@alice`
/// there, and creating a second team-scoped copy would split the ledger this feature depends on).
/// Only when neither exists is one created in the team.
async fn resolve_or_create_label(
    c: &Client,
    team_id: &str,
    label_name: &str,
) -> Result<String, TrackerError> {
    let resp: LabelsResp = c
        .do_graphql(
            query::QUERY_LABELS_BY_NAME,
            Some(json!({ "name": label_name, "first": LABEL_LOOKUP_PAGE })),
        )
        .await?;
    if let Some(id) = pick_label(&resp.issue_labels.nodes, team_id, label_name) {
        return Ok(id);
    }
    let created: LabelCreateResp = c
        .do_graphql(
            query::MUTATION_ISSUE_LABEL_CREATE,
            Some(json!({ "teamId": team_id, "name": label_name })),
        )
        .await?;
    if !created.issue_label_create.success || created.issue_label_create.issue_label.id.is_empty() {
        return Err(LinearError::new(
            LinearErrorKind::MoveRejected,
            format!(
                "create label {label_name:?} in team {team_id} (success={}, id={:?})",
                created.issue_label_create.success, created.issue_label_create.issue_label.id
            ),
        )
        .into());
    }
    Ok(created.issue_label_create.issue_label.id)
}

/// Picks the label to reuse from a by-name lookup: the team-scoped one first, then a
/// workspace-level one, and nothing otherwise. Names are compared case-insensitively because
/// Linear treats label names case-insensitively for uniqueness; a node with an empty id is skipped
/// (it could not be added anyway). Pure, so the preference order is unit-testable without a server.
fn pick_label(nodes: &[LabelNode], team_id: &str, label_name: &str) -> Option<String> {
    let matches = |n: &&LabelNode| !n.id.is_empty() && n.name.eq_ignore_ascii_case(label_name);
    nodes
        .iter()
        .find(|n| matches(n) && n.team.as_ref().is_some_and(|t| t.id == team_id))
        .or_else(|| {
            nodes
                .iter()
                .find(|n| matches(n) && n.team.as_ref().is_none_or(|t| t.id.is_empty()))
        })
        .map(|n| n.id.clone())
}

/// FetchOpenIssuesByLabels returns open (non-terminal) project issues carrying any of
/// `label_names`, with `id`, `identifier` and `labels` populated (STUDIO-644, design §0.11.1) —
/// the per-identity load read. An empty slice returns an empty result with NO API call, mirroring
/// `fetch_issues_by_states`. Follows the connection cursor to completion; a `hasNextPage` with an
/// empty cursor is [`MissingCursor`](LinearErrorKind::MissingCursor) rather than a silent
/// truncation, which would under-count load.
///
/// Labels are lowercased exactly as [`normalize_issue`](Client::normalize_issue) lowercases them
/// on the candidate path, so a caller can compare against one canonical spelling.
pub(super) async fn fetch_open_issues_by_labels(
    c: &Client,
    label_names: &[String],
) -> Result<Vec<Issue>, TrackerError> {
    if label_names.is_empty() {
        return Ok(Vec::new());
    }
    traced(
        crate::tracker_span!("fetch_open_issues_by_labels"),
        async move {
            let mut out: Vec<Issue> = Vec::new();
            let mut after: Option<String> = None;
            let mut pages: u32 = 0;
            loop {
                pages += 1;
                if pages > MAX_PAGES {
                    return Err(LinearError::new(
                        LinearErrorKind::MissingCursor,
                        format!("exceeded {MAX_PAGES} pages without completing pagination"),
                    )
                    .into());
                }
                let vars = json!({
                    "projectSlug": c.config.project_slug.clone(),
                    "names": label_names.to_vec(),
                    "first": c.page_size,
                    "after": Value::from(after.clone()),
                });
                let page: LabelledIssuesResp = c
                    .do_graphql(query::QUERY_OPEN_ISSUES_BY_LABELS, Some(vars))
                    .await?;
                for n in page.issues.nodes {
                    out.push(to_issue(n));
                }
                if !page.issues.page_info.has_next_page {
                    return Ok(out);
                }
                if page.issues.page_info.end_cursor.is_empty() {
                    return Err(LinearError::bare(LinearErrorKind::MissingCursor).into());
                }
                after = Some(page.issues.page_info.end_cursor);
            }
        },
    )
    .await
}

/// Maps one lean node to a core [`Issue`] carrying only the three fields this read promises.
/// Labels are lowercased (`normalize_state` is the adapter's canonical lowercaser) and an empty
/// label set stays `None`, matching the full normalizer.
fn to_issue(n: LabelledIssueNode) -> Issue {
    let mut iss = Issue {
        id: n.id,
        identifier: n.identifier,
        ..Issue::default()
    };
    for l in n.labels.nodes {
        iss.labels
            .get_or_insert_with(Vec::new)
            .push(normalize_state(&l.name));
    }
    iss
}

#[cfg(test)]
mod server_tests {
    use crate::Tracker;
    use crate::TrackerError;
    use crate::linear::testutil::{MockResp, MockServer};
    use crate::linear::{Config, LinearErrorKind, new};
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

    /// Records the query text of every GraphQL document a test's client posted, in order — the
    /// assertion surface for "which documents did the adapter actually send, and in what order".
    type Seen = Arc<Mutex<Vec<String>>>;

    /// A recorder and the handle to move into the mock's handler closure.
    fn recorder() -> (Seen, Seen) {
        let seen: Seen = Arc::new(Mutex::new(Vec::new()));
        (Arc::clone(&seen), seen)
    }

    fn push(seen: &Seen, q: &str) {
        seen.lock().expect("seen").push(q.to_string());
    }

    // The find half: an EXISTING label is reused and nothing is created. Creating a duplicate
    // would split the `rhapsody:@<identity>` ledger the whole feature reads as Tier 0.
    #[tokio::test]
    async fn add_issue_label_reuses_an_existing_label() {
        let (seen, h) = recorder();
        let server = MockServer::start(move |req| {
            push(&h, &req.query);
            if req.query.contains("issueLabels(") {
                return MockResp::ok(
                    r#"{"data":{"issueLabels":{"nodes":[{"id":"lbl-1","name":"rhapsody:@alice","team":{"id":"team-1"}}]}}}"#,
                );
            }
            MockResp::ok(r#"{"data":{"issueAddLabel":{"success":true}}}"#)
        })
        .await;

        client_at(server.url())
            .add_issue_label("iss-1", "team-1", "rhapsody:@alice")
            .await
            .expect("add_issue_label");

        let seen = seen.lock().expect("seen");
        assert_eq!(seen.len(), 2, "expected lookup + add only, got {seen:?}");
        assert!(
            !seen.iter().any(|q| q.contains("issueLabelCreate")),
            "an existing label must not be re-created: {seen:?}"
        );
        assert!(
            seen[1].contains("issueAddLabel(id: $id, labelId: $labelId)"),
            "the add must be issueAddLabel (additive), not issueUpdate(labelIds:) which replaces \
             the whole set: {seen:?}"
        );
    }

    // The create half: an absent label is created in the issue's team, then added.
    #[tokio::test]
    async fn add_issue_label_creates_the_label_when_absent() {
        let (seen, h) = recorder();
        let server = MockServer::start(move |req| {
            push(&h, &req.query);
            if req.query.contains("issueLabels(") {
                return MockResp::ok(r#"{"data":{"issueLabels":{"nodes":[]}}}"#);
            }
            if req.query.contains("issueLabelCreate") {
                assert_eq!(req.var_str("teamId"), Some("team-1"), "create is team-scoped");
                assert_eq!(req.var_str("name"), Some("rhapsody:@alice"));
                return MockResp::ok(
                    r#"{"data":{"issueLabelCreate":{"success":true,"issueLabel":{"id":"lbl-new"}}}}"#,
                );
            }
            assert_eq!(
                req.var_str("labelId"),
                Some("lbl-new"),
                "the add must use the just-created label id"
            );
            MockResp::ok(r#"{"data":{"issueAddLabel":{"success":true}}}"#)
        })
        .await;

        client_at(server.url())
            .add_issue_label("iss-1", "team-1", "rhapsody:@alice")
            .await
            .expect("add_issue_label");

        let seen = seen.lock().expect("seen");
        assert_eq!(
            seen.len(),
            3,
            "expected lookup + create + add, got {seen:?}"
        );
    }

    // A rejected add is an error, not a silent success: the triage task must be able to tell that
    // the assignment did NOT land, so the ticket stays a candidate for the next pass.
    #[tokio::test]
    async fn add_issue_label_rejected_add_is_an_error() {
        let server = MockServer::start(move |req| {
            if req.query.contains("issueLabels(") {
                return MockResp::ok(
                    r#"{"data":{"issueLabels":{"nodes":[{"id":"lbl-1","name":"rhapsody:@alice","team":{"id":"team-1"}}]}}}"#,
                );
            }
            MockResp::ok(r#"{"data":{"issueAddLabel":{"success":false}}}"#)
        })
        .await;

        let err = client_at(server.url())
            .add_issue_label("iss-1", "team-1", "rhapsody:@alice")
            .await
            .expect_err("a rejected add must surface");
        assert!(is_kind(&err, LinearErrorKind::MoveRejected), "err = {err}");
    }

    // Empty arguments never reach the network — the same guard `assign_issue` / `move_issue_state`
    // apply, so a caller bug is an immediate ApiRequest rather than a malformed mutation.
    #[tokio::test]
    async fn add_issue_label_rejects_empty_arguments() {
        let (seen, h) = recorder();
        let server = MockServer::start(move |req| {
            push(&h, &req.query);
            MockResp::ok(r#"{"data":{}}"#)
        })
        .await;
        let c = client_at(server.url());

        for (issue, team, label) in [
            ("", "team-1", "rhapsody:@alice"),
            ("iss-1", "", "rhapsody:@alice"),
            ("iss-1", "team-1", ""),
        ] {
            let err = c
                .add_issue_label(issue, team, label)
                .await
                .expect_err("empty argument must be rejected");
            assert!(is_kind(&err, LinearErrorKind::ApiRequest), "err = {err}");
        }
        assert!(
            seen.lock().expect("seen").is_empty(),
            "no request may be sent for empty arguments"
        );
    }

    // The load read: the project + label + non-terminal filter is in the document, and the result
    // carries id/identifier/lowercased labels so the caller can tally per identity.
    #[tokio::test]
    async fn fetch_open_issues_by_labels_filters_and_normalizes() {
        let (seen, h) = recorder();
        let server = MockServer::start(move |req| {
            push(&h, &req.query);
            assert_eq!(req.var_str("projectSlug"), Some("proj"));
            assert_eq!(
                req.var("names").and_then(|v| v.as_array()).map(Vec::len),
                Some(2),
                "both roster labels are sent in one call"
            );
            MockResp::ok(
                r#"{"data":{"issues":{"nodes":[
                    {"id":"u1","identifier":"STU-1","labels":{"nodes":[{"name":"Rhapsody:@Alice"}]}},
                    {"id":"u2","identifier":"STU-2","labels":{"nodes":[{"name":"rhapsody:@bob"}]}}
                ],"pageInfo":{"hasNextPage":false,"endCursor":null}}}}"#,
            )
        })
        .await;

        let out = client_at(server.url())
            .fetch_open_issues_by_labels(&[
                "rhapsody:@alice".to_string(),
                "rhapsody:@bob".to_string(),
            ])
            .await
            .expect("fetch_open_issues_by_labels");

        assert_eq!(out.len(), 2);
        assert_eq!(out[0].id, "u1");
        assert_eq!(out[0].identifier, "STU-1");
        assert_eq!(out[0].labels, Some(vec!["rhapsody:@alice".to_string()]));
        let q = &seen.lock().expect("seen")[0];
        assert!(
            q.contains(r#"state: { type: { nin: ["completed", "canceled"] } }"#),
            "the read must exclude terminal states by TYPE: {q}"
        );
        assert!(
            q.contains("labels: { name: { in: $names } }"),
            "the read must filter by label name: {q}"
        );
    }

    // An empty roster costs NO API call — the same contract `fetch_issues_by_states` has, and what
    // keeps a Teams-off / empty-roster daemon from talking to Linear at all.
    #[tokio::test]
    async fn fetch_open_issues_by_labels_empty_makes_no_call() {
        let (seen, h) = recorder();
        let server = MockServer::start(move |req| {
            push(&h, &req.query);
            MockResp::ok(r#"{"data":{}}"#)
        })
        .await;

        let out = client_at(server.url())
            .fetch_open_issues_by_labels(&[])
            .await
            .expect("empty is Ok");
        assert!(out.is_empty());
        assert!(
            seen.lock().expect("seen").is_empty(),
            "an empty label list must not reach the network"
        );
    }

    // Pagination is followed to completion: an under-counted load would pile work on a busy
    // teammate, so a truncated page is never silently accepted.
    #[tokio::test]
    async fn fetch_open_issues_by_labels_paginates() {
        let server = MockServer::start(move |req| {
            if req.var_str("after").is_none() || req.var("after") == Some(&serde_json::Value::Null) {
                return MockResp::ok(
                    r#"{"data":{"issues":{"nodes":[
                        {"id":"u1","identifier":"STU-1","labels":{"nodes":[{"name":"rhapsody:@alice"}]}}
                    ],"pageInfo":{"hasNextPage":true,"endCursor":"c1"}}}}"#,
                );
            }
            MockResp::ok(
                r#"{"data":{"issues":{"nodes":[
                    {"id":"u2","identifier":"STU-2","labels":{"nodes":[{"name":"rhapsody:@alice"}]}}
                ],"pageInfo":{"hasNextPage":false,"endCursor":null}}}}"#,
            )
        })
        .await;

        let out = client_at(server.url())
            .fetch_open_issues_by_labels(&["rhapsody:@alice".to_string()])
            .await
            .expect("fetch_open_issues_by_labels");
        assert_eq!(
            out.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
            vec!["u1", "u2"],
            "both pages, in order"
        );
    }

    // `hasNextPage` with no cursor is an error, not a truncation — same stance as the candidate and
    // comment paginators.
    #[tokio::test]
    async fn fetch_open_issues_by_labels_missing_cursor_errors() {
        let server = MockServer::start(move |_req| {
            MockResp::ok(
                r#"{"data":{"issues":{"nodes":[],"pageInfo":{"hasNextPage":true,"endCursor":null}}}}"#,
            )
        })
        .await;

        let err = client_at(server.url())
            .fetch_open_issues_by_labels(&["rhapsody:@alice".to_string()])
            .await
            .expect_err("a missing cursor must surface");
        assert!(is_kind(&err, LinearErrorKind::MissingCursor), "err = {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, name: &str, team: Option<&str>) -> LabelNode {
        LabelNode {
            id: id.to_string(),
            name: name.to_string(),
            team: team.map(|t| LabelTeam { id: t.to_string() }),
        }
    }

    // The team-scoped label wins over a same-named workspace-level one: the ticket being labelled
    // lives in that team, and Linear shows the team label on it.
    #[test]
    fn pick_label_prefers_the_team_scoped_label() {
        let nodes = vec![
            node("ws", "rhapsody:@alice", None),
            node("team", "rhapsody:@alice", Some("t1")),
        ];
        assert_eq!(
            pick_label(&nodes, "t1", "rhapsody:@alice"),
            Some("team".to_string())
        );
    }

    // A workspace-level label (team: null) is REUSED rather than duplicated into the team — the
    // whole point of not filtering the lookup server-side by team.
    #[test]
    fn pick_label_falls_back_to_a_workspace_level_label() {
        let nodes = vec![node("ws", "rhapsody:@alice", None)];
        assert_eq!(
            pick_label(&nodes, "t1", "rhapsody:@alice"),
            Some("ws".to_string())
        );
    }

    // A same-named label owned by ANOTHER team is not ours to add: it would not be addable to this
    // team's issue, so the caller must create one instead.
    #[test]
    fn pick_label_ignores_another_teams_label() {
        let nodes = vec![node("other", "rhapsody:@alice", Some("t2"))];
        assert_eq!(pick_label(&nodes, "t1", "rhapsody:@alice"), None);
    }

    // Linear treats label names case-insensitively for uniqueness, so a stored `Rhapsody:@Alice`
    // IS the label we are asking for — creating a second one would split the assignment ledger.
    #[test]
    fn pick_label_matches_case_insensitively() {
        let nodes = vec![node("ws", "Rhapsody:@Alice", None)];
        assert_eq!(
            pick_label(&nodes, "t1", "rhapsody:@alice"),
            Some("ws".to_string())
        );
    }

    // An id-less node cannot be added to anything; it must not shadow a usable match.
    #[test]
    fn pick_label_skips_id_less_nodes() {
        let nodes = vec![
            node("", "rhapsody:@alice", Some("t1")),
            node("ws", "rhapsody:@alice", None),
        ];
        assert_eq!(
            pick_label(&nodes, "t1", "rhapsody:@alice"),
            Some("ws".to_string())
        );
    }

    // The load read returns the three promised fields and lowercases labels, exactly as the
    // candidate path's normalizer does, so callers compare one canonical spelling.
    #[test]
    fn to_issue_keeps_id_identifier_and_lowercased_labels() {
        let iss = to_issue(LabelledIssueNode {
            id: "u1".to_string(),
            identifier: "STU-1".to_string(),
            labels: LabelNamesConn {
                nodes: vec![
                    LabelNameNode {
                        name: "Rhapsody:@Alice".to_string(),
                    },
                    LabelNameNode {
                        name: "Rust".to_string(),
                    },
                ],
            },
        });
        assert_eq!(iss.id, "u1");
        assert_eq!(iss.identifier, "STU-1");
        assert_eq!(
            iss.labels,
            Some(vec!["rhapsody:@alice".to_string(), "rust".to_string()])
        );
    }

    // No labels at all stays `None` (not `Some(vec![])`), matching the full normalizer's shape.
    #[test]
    fn to_issue_leaves_an_empty_label_set_none() {
        let iss = to_issue(LabelledIssueNode {
            id: "u1".to_string(),
            identifier: "STU-1".to_string(),
            labels: LabelNamesConn::default(),
        });
        assert!(iss.labels.is_none());
    }
}
