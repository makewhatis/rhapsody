//! Issue creation — Rhapsody Teams' review-quorum fan-out (STUDIO-659, slice T7; design record
//! `~/.rhapsody/docs/STUDIO-572-rhapsody-teams.md`, §0.12). **No Go v0.4.0 counterpart**: Symphony
//! never created work, it only ever read and moved it.
//!
//! [`create_issue`] is the contract's one issue-MINTING surface, and §0.12 chose it deliberately
//! over every richer alternative: reviewers are *ordinary tickets*, so the quorum needs no new
//! dispatch path, cannot collide with the one-live-run-per-issue invariant, and gets §0.6's
//! reviewer context isolation (separate worktrees, separate prompts, no shared findings) for free.
//!
//! # Four resolutions, then one mutation
//!
//! `issueCreate` speaks in tracker UUIDs while the caller speaks in names, so the create resolves
//! — reusing the SAME helpers the existing writes use, never a parallel copy:
//!
//! * the **project** from the client's own configured `project_slug` ([`resolve_project_id`],
//!   cached for the client's lifetime like the milestone id). Placing the issue in the daemon's
//!   project is what makes a created ticket a *candidate*: `queryCandidates` filters on exactly
//!   that `slugId`, so an issue created outside it would be invisible to the daemon that made it.
//! * the **state** by name, via [`move_state::resolve_state_id`] — the same team-scoped,
//!   case-insensitive, memoized resolution `MoveIssueState` performs.
//! * each **label** by name, via [`labels::resolve_or_create_label`] — the same find-or-create,
//!   preferring a team-scoped label and falling back to a workspace-level one of the same name.
//! * the **assignee** not at all: it is already a user id by the time it reaches here.
//!
//! # Failures are loud, never partial
//!
//! A label that will not resolve fails the whole create rather than producing an unlabelled issue.
//! That is the important direction: the `rhapsody:@<reviewer>` label IS the assignment (§0.11.1),
//! so an unlabelled review ticket is a ticket routed to nobody — worse than no ticket at all,
//! because it looks like the fan-out worked. Same for `success: false`, and for a response that
//! reports success with no identifier: both are [`MoveRejected`](LinearErrorKind::MoveRejected)
//! rather than an `Ok("")` the caller would happily report to the room.
//!
//! **Not idempotent, and it cannot be.** There is no natural key to dedupe on — two review tickets
//! for the same PR are two legitimate issues. The once-per-parent guard is the caller's; the
//! quorum's is the `rhapsody:quorum-requested` marker label on the parent.

use super::client::traced;
use super::{Client, LinearError, LinearErrorKind, labels, move_state, query};
use crate::{NewIssue, TrackerError};
use serde::Deserialize;
use serde_json::{Value, json};

/// The `issueCreate { success issue { id identifier } }` envelope.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct IssueCreateResp {
    #[serde(rename = "issueCreate")]
    issue_create: IssueCreateNode,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct IssueCreateNode {
    success: bool,
    issue: CreatedIssueNode,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct CreatedIssueNode {
    #[serde(deserialize_with = "super::decode::null_to_empty")]
    id: String,
    #[serde(deserialize_with = "super::decode::null_to_empty")]
    identifier: String,
}

/// The `projects { nodes { id } }` envelope of the by-slug lookup.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ProjectsBySlugResp {
    projects: ProjectsBySlugConn,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ProjectsBySlugConn {
    nodes: Vec<ProjectIdNode>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ProjectIdNode {
    #[serde(deserialize_with = "super::decode::null_to_empty")]
    id: String,
}

/// How many by-slug project hits one lookup page reads. A `slugId` is unique, so this is headroom
/// against a filter that unexpectedly matches more than one, not a pagination story.
const PROJECT_LOOKUP_PAGE: i64 = 5;

/// CreateIssue creates one issue in the client's configured project and returns its human
/// identifier (STUDIO-659, design §0.12). See the module docs for the resolution order and why a
/// partial create is refused.
pub(super) async fn create_issue(c: &Client, spec: &NewIssue) -> Result<String, TrackerError> {
    traced(crate::tracker_span!("create_issue"), async move {
        if spec.team_id.is_empty() || spec.title.is_empty() {
            return Err(LinearError::new(
                LinearErrorKind::ApiRequest,
                format!(
                    "create issue requires teamID and title (got {:?},{:?})",
                    spec.team_id, spec.title
                ),
            )
            .into());
        }
        let project_id = resolve_project_id(c).await?;
        // Resolved BEFORE the mutation, so a state or label the workspace will not give us fails
        // without leaving a half-built issue behind.
        let state_id = if spec.state_name.is_empty() {
            String::new()
        } else {
            move_state::resolve_state_id(c, &spec.team_id, &spec.state_name).await?
        };
        let mut label_ids: Vec<String> = Vec::with_capacity(spec.labels.len());
        for name in &spec.labels {
            if name.is_empty() {
                continue;
            }
            label_ids.push(labels::resolve_or_create_label(c, &spec.team_id, name).await?);
        }
        let vars = json!({
            "teamId": spec.team_id,
            "projectId": opt(&project_id),
            "title": spec.title,
            "description": opt(&spec.description),
            "stateId": opt(&state_id),
            "assigneeId": opt(&spec.assignee_id),
            "labelIds": if label_ids.is_empty() { Value::Null } else { Value::from(label_ids) },
        });
        let resp: IssueCreateResp = c
            .do_graphql(query::MUTATION_ISSUE_CREATE, Some(vars))
            .await?;
        if !resp.issue_create.success || resp.issue_create.issue.identifier.is_empty() {
            return Err(LinearError::new(
                LinearErrorKind::MoveRejected,
                format!(
                    "create issue {:?} in team {} (success={}, id={:?}, identifier={:?})",
                    spec.title,
                    spec.team_id,
                    resp.issue_create.success,
                    resp.issue_create.issue.id,
                    resp.issue_create.issue.identifier
                ),
            )
            .into());
        }
        Ok(resp.issue_create.issue.identifier)
    })
    .await
}

/// An empty string becomes a JSON `null`, which Linear treats as "key omitted" — the one-document
/// trick the mutation's doc comment describes.
fn opt(s: &str) -> Value {
    if s.is_empty() {
        Value::Null
    } else {
        Value::from(s)
    }
}

/// Resolves the configured `project_slug` to the project UUID `issueCreate` needs, memoized under
/// [`Client::project_id`] for the client's lifetime (the project a daemon is pointed at does not
/// change without a reload, which builds a new client). A slug that matches no project is
/// [`ApiRequest`](LinearErrorKind::ApiRequest): creating the issue anyway would put it outside the
/// candidate query and lose it.
///
/// The lock is HELD across the query, mirroring the viewer/milestone single-flight caches rather
/// than `state_id_cache`'s lock/query/lock: a stampede here would be several identical project
/// lookups on the same fan-out.
async fn resolve_project_id(c: &Client) -> Result<String, TrackerError> {
    if c.config.project_slug.is_empty() {
        return Err(LinearError::new(
            LinearErrorKind::ApiRequest,
            "create issue requires a configured tracker.project_slug".to_string(),
        )
        .into());
    }
    let mut cached = c.project_id.lock().await;
    if !cached.is_empty() {
        return Ok(cached.clone());
    }
    let resp: ProjectsBySlugResp = c
        .do_graphql(
            query::QUERY_PROJECT_BY_SLUG,
            Some(json!({
                "projectSlug": c.config.project_slug,
                "first": PROJECT_LOOKUP_PAGE,
            })),
        )
        .await?;
    let id = resp
        .projects
        .nodes
        .into_iter()
        .map(|n| n.id)
        .find(|id| !id.is_empty())
        .ok_or_else(|| {
            TrackerError::from(LinearError::new(
                LinearErrorKind::ApiRequest,
                format!(
                    "no project matches tracker.project_slug {:?}",
                    c.config.project_slug
                ),
            ))
        })?;
    *cached = id.clone();
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Tracker as _;
    use crate::linear::testutil::{MockResp, MockServer};
    use crate::linear::{Config, new};
    use std::sync::{Arc, Mutex};

    fn client_at(url: String) -> Client {
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

    /// Records every GraphQL document a test's client posted, in order.
    type Seen = Arc<Mutex<Vec<String>>>;

    fn recorder() -> (Seen, Seen) {
        let seen: Seen = Arc::new(Mutex::new(Vec::new()));
        (Arc::clone(&seen), seen)
    }

    fn spec() -> NewIssue {
        NewIssue {
            team_id: "team-1".into(),
            title: "Review: STUDIO-1 do the thing".into(),
            description: "review https://github.com/o/r/pull/7".into(),
            state_name: "Todo".into(),
            assignee_id: "viewer-1".into(),
            labels: vec!["rhapsody:@bob".into()],
        }
    }

    /// A handler that answers each resolution in turn and finally the create. Shared by the tests
    /// below so each one only states what it is actually asserting.
    fn happy_path(seen: Seen) -> impl Fn(&crate::linear::testutil::GqlReq) -> MockResp {
        move |req| {
            seen.lock().expect("seen").push(req.query.to_string());
            if req.query.contains("projects(") {
                return MockResp::ok(r#"{"data":{"projects":{"nodes":[{"id":"proj-uuid"}]}}}"#);
            }
            if req.query.contains("workflowStates(") {
                return MockResp::ok(
                    r#"{"data":{"workflowStates":{"nodes":[{"id":"state-todo","name":"Todo","type":"unstarted","position":1}]}}}"#,
                );
            }
            if req.query.contains("issueLabels(") {
                return MockResp::ok(
                    r#"{"data":{"issueLabels":{"nodes":[{"id":"lbl-bob","name":"rhapsody:@bob","team":{"id":"team-1"}}]}}}"#,
                );
            }
            MockResp::ok(
                r#"{"data":{"issueCreate":{"success":true,"issue":{"id":"new-uuid","identifier":"STUDIO-700"}}}}"#,
            )
        }
    }

    // STUDIO-659, design §0.12: the create resolves project + state + labels to ids and sends ONE
    // issueCreate carrying all of them. Every variable here is an acceptance criterion of the
    // review-quorum fan-out — the ticket must be in the daemon's project (else it is not a
    // candidate), assigned (else the claim rule never picks it up) and labelled (else it is routed
    // to nobody), so the assertion is on the whole variable set, not just the title.
    #[tokio::test]
    async fn create_issue_resolves_everything_and_returns_the_identifier() {
        let (seen, h) = recorder();
        let vars: Arc<Mutex<serde_json::Value>> = Arc::new(Mutex::new(serde_json::Value::Null));
        let seen_vars = Arc::clone(&vars);
        let inner = happy_path(h);
        let server = MockServer::start(move |req| {
            if req.query.contains("issueCreate") {
                *seen_vars.lock().expect("vars") = req.variables.clone();
            }
            inner(req)
        })
        .await;

        let got = client_at(server.url())
            .create_issue(&spec())
            .await
            .expect("create_issue");
        assert_eq!(got, "STUDIO-700", "the human identifier is returned");

        let v = vars.lock().expect("vars").clone();
        assert_eq!(v["teamId"], "team-1");
        assert_eq!(
            v["projectId"], "proj-uuid",
            "created inside the daemon's project"
        );
        assert_eq!(
            v["stateId"], "state-todo",
            "the state name resolved to a uuid"
        );
        assert_eq!(
            v["assigneeId"], "viewer-1",
            "assigned, or nothing picks it up"
        );
        assert_eq!(
            v["labelIds"],
            serde_json::json!(["lbl-bob"]),
            "the rhapsody:@<reviewer> label IS the assignment"
        );
        assert_eq!(v["title"], "Review: STUDIO-1 do the thing");

        let seen = seen.lock().expect("seen");
        assert_eq!(
            seen.len(),
            4,
            "expected project + state + label + create, got {seen:?}"
        );
    }

    // The project lookup is cached for the client's lifetime: a fan-out of N review tickets must
    // not issue N identical project queries.
    #[tokio::test]
    async fn create_issue_caches_the_project_lookup() {
        let (seen, h) = recorder();
        let server = MockServer::start(happy_path(h)).await;
        let c = client_at(server.url());

        c.create_issue(&spec()).await.expect("first");
        c.create_issue(&spec()).await.expect("second");

        let seen = seen.lock().expect("seen");
        let projects = seen.iter().filter(|q| q.contains("projects(")).count();
        assert_eq!(projects, 1, "the project id is resolved once: {seen:?}");
    }

    // Empty optionals travel as JSON null (== omitted, per the mutation's doc comment), so one
    // document serves "no state / no assignee / no labels" rather than four forked ones.
    #[tokio::test]
    async fn create_issue_sends_null_for_absent_optionals() {
        let vars: Arc<Mutex<serde_json::Value>> = Arc::new(Mutex::new(serde_json::Value::Null));
        let seen_vars = Arc::clone(&vars);
        let (_seen, h) = recorder();
        let inner = happy_path(h);
        let server = MockServer::start(move |req| {
            if req.query.contains("issueCreate") {
                *seen_vars.lock().expect("vars") = req.variables.clone();
            }
            inner(req)
        })
        .await;

        client_at(server.url())
            .create_issue(&NewIssue {
                team_id: "team-1".into(),
                title: "bare".into(),
                ..NewIssue::default()
            })
            .await
            .expect("create_issue");

        let v = vars.lock().expect("vars").clone();
        assert!(v["description"].is_null());
        assert!(v["stateId"].is_null(), "no state name ⇒ the team default");
        assert!(v["assigneeId"].is_null());
        assert!(v["labelIds"].is_null());
    }

    // A label that will not resolve must fail the CREATE, not yield an unlabelled issue: the
    // `rhapsody:@<reviewer>` label is the assignment, so an unlabelled review ticket is routed to
    // nobody while looking like the fan-out worked. Nothing is created.
    #[tokio::test]
    async fn create_issue_refuses_to_create_when_a_label_cannot_resolve() {
        let (seen, h) = recorder();
        let server = MockServer::start(move |req| {
            h.lock().expect("seen").push(req.query.to_string());
            if req.query.contains("projects(") {
                return MockResp::ok(r#"{"data":{"projects":{"nodes":[{"id":"proj-uuid"}]}}}"#);
            }
            if req.query.contains("workflowStates(") {
                return MockResp::ok(
                    r#"{"data":{"workflowStates":{"nodes":[{"id":"state-todo","name":"Todo","type":"unstarted","position":1}]}}}"#,
                );
            }
            if req.query.contains("issueLabels(") {
                return MockResp::ok(r#"{"data":{"issueLabels":{"nodes":[]}}}"#);
            }
            if req.query.contains("issueLabelCreate") {
                return MockResp::ok(
                    r#"{"data":{"issueLabelCreate":{"success":false,"issueLabel":{"id":""}}}}"#,
                );
            }
            MockResp::ok(
                r#"{"data":{"issueCreate":{"success":true,"issue":{"id":"x","identifier":"STUDIO-700"}}}}"#,
            )
        })
        .await;

        let err = client_at(server.url())
            .create_issue(&spec())
            .await
            .expect_err("an unresolvable label must fail the create");
        assert!(is_kind(&err, LinearErrorKind::MoveRejected), "got {err}");
        let seen = seen.lock().expect("seen");
        assert!(
            !seen.iter().any(|q| q.contains("issueCreate")),
            "nothing may be created once a label failed: {seen:?}"
        );
    }

    // A slug that matches no project is an error, not a create into the void: an issue outside the
    // configured project fails `queryCandidates`' `slugId` filter, so the daemon could never pick
    // up the review ticket it just made.
    #[tokio::test]
    async fn create_issue_errors_when_the_project_slug_matches_nothing() {
        let server = MockServer::start(|req| {
            if req.query.contains("projects(") {
                return MockResp::ok(r#"{"data":{"projects":{"nodes":[]}}}"#);
            }
            MockResp::ok(r#"{"data":{}}"#)
        })
        .await;

        let err = client_at(server.url())
            .create_issue(&spec())
            .await
            .expect_err("an unmatched slug must error");
        assert!(is_kind(&err, LinearErrorKind::ApiRequest), "got {err}");
    }

    // `success: true` with no identifier is a rejection, never an `Ok("")` the caller would report
    // to the room as a created review ticket.
    #[tokio::test]
    async fn create_issue_rejects_a_success_without_an_identifier() {
        let (_seen, h) = recorder();
        let inner = happy_path(h);
        let server = MockServer::start(move |req| {
            if req.query.contains("issueCreate") {
                return MockResp::ok(
                    r#"{"data":{"issueCreate":{"success":true,"issue":{"id":"","identifier":""}}}}"#,
                );
            }
            inner(req)
        })
        .await;

        let err = client_at(server.url())
            .create_issue(&spec())
            .await
            .expect_err("no identifier must error");
        assert!(is_kind(&err, LinearErrorKind::MoveRejected), "got {err}");
    }

    // Missing required arguments never reach the network.
    #[tokio::test]
    async fn create_issue_requires_a_team_and_a_title() {
        let c = client_at("http://127.0.0.1:1".to_string());
        for spec in [
            NewIssue {
                title: "t".into(),
                ..NewIssue::default()
            },
            NewIssue {
                team_id: "team-1".into(),
                ..NewIssue::default()
            },
        ] {
            let err = c.create_issue(&spec).await.expect_err("must error");
            assert!(is_kind(&err, LinearErrorKind::ApiRequest), "got {err}");
        }
    }
}
