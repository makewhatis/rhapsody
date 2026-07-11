//! Issue state moves — parity port of `internal/tracker/linear/move.go` (Rust `move_state`
//! because `move` is a keyword).
//!
//! [`move_issue_state`] promotes an issue to a named workflow state (the single WRITE the tracker
//! contract exposes, symphony-29): it resolves the state NAME to its workflow-state UUID scoped by
//! `team_id` (state names are unique only within a team, matched case-insensitively) and issues an
//! `issueUpdate(stateId)` mutation. [`move_issue_to_type`] is the config-free variant — it resolves
//! by Linear state TYPE (stable across workspaces) and returns the resolved state's display name.
//! A team with no matching state is [`TrackerError::StateNotFound`] (the `ErrLinearStateNotFound`
//! mirror); a `success: false` response is [`MoveRejected`](LinearErrorKind::MoveRejected).

use super::client::traced;
use super::{Client, LinearError, LinearErrorKind, query};
use crate::TrackerError;
use rhapsody_core::normalize_state;
use serde::Deserialize;
use serde_json::json;

/// The `issueUpdate { success }` mutation envelope (shared by both moves; the assign mutation in
/// `claim` decodes the same shape into its own copy).
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct IssueUpdateResp {
    #[serde(rename = "issueUpdate")]
    issue_update: SuccessNode,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SuccessNode {
    success: bool,
}

/// The `workflowStates { nodes { id name type position } }` envelope. The name-based resolution
/// reads only `id`/`name`; the type-based one also reads `type`/`position` (absent fields default,
/// so one struct serves both queries).
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct WorkflowStatesResp {
    #[serde(rename = "workflowStates")]
    workflow_states: WorkflowStatesConn,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct WorkflowStatesConn {
    nodes: Vec<WorkflowStateNode>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct WorkflowStateNode {
    id: String,
    name: String,
    #[serde(rename = "type")]
    kind: String,
    position: f64,
}

/// MoveIssueState moves an issue to the named workflow state (move.go's `MoveIssueState`). Resolves
/// the state name → UUID (team-scoped, case-insensitive, cached), then issues `issueUpdate(stateId)`.
/// Empty args are [`ApiRequest`](LinearErrorKind::ApiRequest); a `success: false` response is
/// [`MoveRejected`](LinearErrorKind::MoveRejected).
pub(super) async fn move_issue_state(
    c: &Client,
    issue_id: &str,
    team_id: &str,
    state_name: &str,
) -> Result<(), TrackerError> {
    traced(crate::tracker_span!("move_issue_state"), async move {
        if issue_id.is_empty() || team_id.is_empty() || state_name.is_empty() {
            return Err(LinearError::new(
                LinearErrorKind::ApiRequest,
                format!(
                    "move requires issueID, teamID and stateName (got {issue_id:?},{team_id:?},{state_name:?})"
                ),
            )
            .into());
        }
        let state_id = resolve_state_id(c, team_id, state_name).await?;
        let vars = json!({ "id": issue_id, "stateId": state_id });
        let resp: IssueUpdateResp = c
            .do_graphql(query::MUTATION_ISSUE_UPDATE_STATE, Some(vars))
            .await?;
        if !resp.issue_update.success {
            return Err(LinearError::new(
                LinearErrorKind::MoveRejected,
                format!("issue {issue_id} -> {state_name:?}"),
            )
            .into());
        }
        Ok(())
    })
    .await
}

/// MoveIssueToType moves an issue to the team's workflow state of the given Linear state TYPE (e.g.
/// "backlog", "unstarted"), returning the resolved state's display name (move.go's
/// `MoveIssueToType`). Config-free: TYPES are stable across workspaces while names vary. Reuses the
/// `issueUpdate(stateId)` mutation. A team with no state of that type is
/// [`StateNotFound`](TrackerError::StateNotFound). Unlike [`move_issue_state`], this is not wrapped
/// in a tracker span — mirroring move.go, which spans only the name-based move.
pub(super) async fn move_issue_to_type(
    c: &Client,
    issue_id: &str,
    team_id: &str,
    state_type: &str,
) -> Result<String, TrackerError> {
    if issue_id.is_empty() || team_id.is_empty() || state_type.is_empty() {
        return Err(LinearError::new(
            LinearErrorKind::ApiRequest,
            format!(
                "move-to-type requires issueID, teamID and type (got {issue_id:?},{team_id:?},{state_type:?})"
            ),
        )
        .into());
    }
    let (state_id, state_name) = resolve_state_id_by_type(c, team_id, state_type).await?;
    let vars = json!({ "id": issue_id, "stateId": state_id });
    let resp: IssueUpdateResp = c
        .do_graphql(query::MUTATION_ISSUE_UPDATE_STATE, Some(vars))
        .await?;
    if !resp.issue_update.success {
        return Err(LinearError::new(
            LinearErrorKind::MoveRejected,
            format!("issue {issue_id} -> type {state_type:?}"),
        )
        .into());
    }
    Ok(state_name)
}

/// Resolves the workflow-state UUID for a (team, state name) pair, memoized under
/// [`state_id_cache`](Client::state_id_cache) (move.go's `resolveStateID`). The name is matched
/// case-insensitively ([`normalize_state`]) so a config value like "in progress" resolves to
/// Linear's "In Progress" (a server-side eq filter would be case-sensitive and silently miss). The
/// cache lock is dropped across the query (not single-flight, mirroring Go); a not-found result is
/// never cached, so a later-created state resolves on a subsequent move.
async fn resolve_state_id(
    c: &Client,
    team_id: &str,
    state_name: &str,
) -> Result<String, TrackerError> {
    let norm = normalize_state(state_name);
    let key = format!("{team_id}\0{norm}");
    // Lock / check / unlock — the guard is released before the network call (move.go's `stateIDMu`
    // is not held across `doGraphQL`).
    if let Some(id) = c.state_id_cache.lock().await.get(&key) {
        return Ok(id.clone());
    }
    let resp: WorkflowStatesResp = c
        .do_graphql(
            query::QUERY_TEAM_WORKFLOW_STATES,
            Some(json!({ "teamID": team_id })),
        )
        .await?;
    for n in &resp.workflow_states.nodes {
        if !n.id.is_empty() && normalize_state(&n.name) == norm {
            c.state_id_cache.lock().await.insert(key, n.id.clone());
            return Ok(n.id.clone());
        }
    }
    Err(TrackerError::StateNotFound(format!(
        "team {team_id} state {state_name:?}"
    )))
}

/// Resolves the (id, name) of the FIRST workflow state of the given TYPE for a team, ordered by
/// position (stable, lowest first) — move.go's `resolveStateIDByType`. Not cached (cheap, rare). A
/// team with no state of that type is [`StateNotFound`](TrackerError::StateNotFound).
async fn resolve_state_id_by_type(
    c: &Client,
    team_id: &str,
    state_type: &str,
) -> Result<(String, String), TrackerError> {
    let resp: WorkflowStatesResp = c
        .do_graphql(
            query::QUERY_TEAM_WORKFLOW_STATES,
            Some(json!({ "teamID": team_id })),
        )
        .await?;
    let mut best: Option<&WorkflowStateNode> = None;
    for n in &resp.workflow_states.nodes {
        if n.id.is_empty() || n.kind != state_type {
            continue;
        }
        if best.is_none_or(|b| n.position < b.position) {
            best = Some(n);
        }
    }
    match best {
        Some(n) => Ok((n.id.clone(), n.name.clone())),
        None => Err(TrackerError::StateNotFound(format!(
            "team {team_id} has no state of type {state_type:?}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use crate::Tracker;
    use crate::TrackerError;
    use crate::linear::testutil::{MockResp, MockServer};
    use crate::linear::{Config, new};
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

    // Mirrors Go TestMoveIssueStateResolvesAndMutates: a move first resolves the state name -> UUID
    // via workflowStates (team-scoped), then issues issueUpdate with that stateId.
    #[tokio::test]
    async fn move_issue_state_resolves_and_mutates() {
        let queries = Arc::new(Mutex::new(Vec::<String>::new()));
        let queries_h = Arc::clone(&queries);
        let update_vars = Arc::new(Mutex::new((String::new(), String::new())));
        let update_vars_h = Arc::clone(&update_vars);
        let team_ok = Arc::new(Mutex::new(false));
        let team_ok_h = Arc::clone(&team_ok);
        let server = MockServer::start(move |req| {
            queries_h.lock().expect("queries").push(req.query.clone());
            if req.query.contains("workflowStates") {
                *team_ok_h.lock().expect("team") = req.var_str("teamID") == Some("team-1");
                MockResp::ok(
                    r#"{"data":{"workflowStates":{"nodes":[{"id":"state-uuid-9","name":"In Progress"}]}}}"#,
                )
            } else if req.query.contains("issueUpdate") {
                *update_vars_h.lock().expect("vars") = (
                    req.var_str("id").unwrap_or_default().to_owned(),
                    req.var_str("stateId").unwrap_or_default().to_owned(),
                );
                MockResp::ok(r#"{"data":{"issueUpdate":{"success":true}}}"#)
            } else {
                MockResp::ok(r#"{"data":{}}"#)
            }
        })
        .await;
        let c = client_at(server.url());

        c.move_issue_state("iss-1", "team-1", "In Progress")
            .await
            .expect("MoveIssueState");

        let q = queries.lock().expect("queries");
        assert_eq!(
            q.len(),
            2,
            "expected 2 queries (resolve + mutate), got {q:?}"
        );
        assert!(
            q[0].contains("workflowStates") && q[1].contains("issueUpdate"),
            "query order wrong: {q:?}"
        );
        assert!(*team_ok.lock().expect("team"), "workflowStates teamID var");
        assert_eq!(
            *update_vars.lock().expect("vars"),
            ("iss-1".to_owned(), "state-uuid-9".to_owned()),
            "issueUpdate vars"
        );
    }

    // Mirrors Go TestMoveIssueStateCachesStateID: a second move to the same (team,state) reuses the
    // cached UUID and issues NO second workflowStates query.
    #[tokio::test]
    async fn move_issue_state_caches_state_id() {
        let resolve_calls = Arc::new(AtomicUsize::new(0));
        let resolve_h = Arc::clone(&resolve_calls);
        let mutate_calls = Arc::new(AtomicUsize::new(0));
        let mutate_h = Arc::clone(&mutate_calls);
        let server = MockServer::start(move |req| {
            if req.query.contains("workflowStates") {
                resolve_h.fetch_add(1, Ordering::SeqCst);
                MockResp::ok(
                    r#"{"data":{"workflowStates":{"nodes":[{"id":"sid","name":"In Progress"}]}}}"#,
                )
            } else {
                mutate_h.fetch_add(1, Ordering::SeqCst);
                MockResp::ok(r#"{"data":{"issueUpdate":{"success":true}}}"#)
            }
        })
        .await;
        let c = client_at(server.url());

        for _ in 0..3 {
            c.move_issue_state("iss", "t", "In Progress")
                .await
                .expect("move");
        }
        assert_eq!(
            resolve_calls.load(Ordering::SeqCst),
            1,
            "state resolution should be cached"
        );
        assert_eq!(
            mutate_calls.load(Ordering::SeqCst),
            3,
            "each move should mutate"
        );
    }

    // Mirrors Go TestMoveIssueStateNotFound: workflowStates returns no nodes -> StateNotFound and NO
    // mutation is issued.
    #[tokio::test]
    async fn move_issue_state_not_found() {
        let mutate_calls = Arc::new(AtomicUsize::new(0));
        let mutate_h = Arc::clone(&mutate_calls);
        let server = MockServer::start(move |req| {
            if req.query.contains("issueUpdate") {
                mutate_h.fetch_add(1, Ordering::SeqCst);
            }
            MockResp::ok(r#"{"data":{"workflowStates":{"nodes":[]}}}"#)
        })
        .await;
        let c = client_at(server.url());

        let err = c
            .move_issue_state("iss", "team", "Ghost State")
            .await
            .expect_err("no state -> error");
        assert!(
            matches!(err, TrackerError::StateNotFound(_)),
            "got {err:?}, want StateNotFound"
        );
        assert_eq!(
            mutate_calls.load(Ordering::SeqCst),
            0,
            "no mutation when the state cannot be resolved"
        );
    }

    // Mirrors Go TestMoveIssueStateRejected: issueUpdate returns success:false -> MoveRejected.
    #[tokio::test]
    async fn move_issue_state_rejected() {
        let server = MockServer::start(move |req| {
            if req.query.contains("workflowStates") {
                MockResp::ok(
                    r#"{"data":{"workflowStates":{"nodes":[{"id":"sid","name":"In Progress"}]}}}"#,
                )
            } else {
                MockResp::ok(r#"{"data":{"issueUpdate":{"success":false}}}"#)
            }
        })
        .await;
        let c = client_at(server.url());

        let err = c
            .move_issue_state("iss", "team", "In Progress")
            .await
            .expect_err("success:false -> error");
        assert!(
            matches!(&err, TrackerError::Linear(e) if e.kind == crate::linear::LinearErrorKind::MoveRejected),
            "got {err:?}, want MoveRejected"
        );
    }

    // Mirrors Go TestMoveIssueStateCaseInsensitive: a configured lowercase name ("in progress")
    // resolves to Linear's "In Progress" — the match is case-insensitive.
    #[tokio::test]
    async fn move_issue_state_case_insensitive() {
        let moved_state_id = Arc::new(Mutex::new(Option::<String>::None));
        let moved_h = Arc::clone(&moved_state_id);
        let server = MockServer::start(move |req| {
            if req.query.contains("workflowStates") {
                MockResp::ok(
                    r#"{"data":{"workflowStates":{"nodes":[{"id":"s1","name":"Backlog"},{"id":"s2","name":"In Progress"}]}}}"#,
                )
            } else {
                *moved_h.lock().expect("moved") =
                    Some(req.var_str("stateId").unwrap_or_default().to_owned());
                MockResp::ok(r#"{"data":{"issueUpdate":{"success":true}}}"#)
            }
        })
        .await;
        let c = client_at(server.url());

        c.move_issue_state("iss", "team", "in progress")
            .await
            .expect("MoveIssueState (lowercase config)");
        assert_eq!(
            moved_state_id.lock().expect("moved").as_deref(),
            Some("s2"),
            "expected the issue moved to the case-insensitively matched In Progress state (s2)"
        );
    }

    // Mirrors Go TestMoveIssueToType_ResolvesByTypeAndMoves: a type-based move resolves the team's
    // workflow state by Linear state TYPE (not name), returns its display name, and mutates with its
    // UUID.
    #[tokio::test]
    async fn move_issue_to_type_resolves_by_type_and_moves() {
        let queries = Arc::new(Mutex::new(Vec::<String>::new()));
        let queries_h = Arc::clone(&queries);
        let moved_state_id = Arc::new(Mutex::new(Option::<String>::None));
        let moved_h = Arc::clone(&moved_state_id);
        let server = MockServer::start(move |req| {
            queries_h.lock().expect("queries").push(req.query.clone());
            if req.query.contains("workflowStates") {
                MockResp::ok(
                    r#"{"data":{"workflowStates":{"nodes":[{"id":"triage-uuid","name":"Triage me","type":"triage","position":0},{"id":"backlog-uuid","name":"Backlog","type":"backlog","position":1}]}}}"#,
                )
            } else if req.query.contains("issueUpdate") {
                *moved_h.lock().expect("moved") =
                    Some(req.var_str("stateId").unwrap_or_default().to_owned());
                MockResp::ok(r#"{"data":{"issueUpdate":{"success":true}}}"#)
            } else {
                MockResp::ok(r#"{"data":{}}"#)
            }
        })
        .await;
        let c = client_at(server.url());

        let name = c
            .move_issue_to_type("ISSUE", "TEAM", "backlog")
            .await
            .expect("MoveIssueToType");
        assert_eq!(name, "Backlog", "resolved state name");
        assert_eq!(
            moved_state_id.lock().expect("moved").as_deref(),
            Some("backlog-uuid"),
            "issueUpdate must carry the backlog state id"
        );
        assert_eq!(
            queries.lock().expect("queries").len(),
            2,
            "expected 2 queries (resolve + mutate)"
        );
    }

    // Mirrors Go TestMoveIssueToType_NoStateOfType: a team with only a "Done" (completed) state has
    // no "backlog" state -> StateNotFound and NO mutation is issued.
    #[tokio::test]
    async fn move_issue_to_type_no_state_of_type() {
        let mutate_calls = Arc::new(AtomicUsize::new(0));
        let mutate_h = Arc::clone(&mutate_calls);
        let server = MockServer::start(move |req| {
            if req.query.contains("issueUpdate") {
                mutate_h.fetch_add(1, Ordering::SeqCst);
            }
            MockResp::ok(
                r#"{"data":{"workflowStates":{"nodes":[{"id":"done-uuid","name":"Done","type":"completed","position":0}]}}}"#,
            )
        })
        .await;
        let c = client_at(server.url());

        let err = c
            .move_issue_to_type("ISSUE", "TEAM", "backlog")
            .await
            .expect_err("no state of type -> error");
        assert!(
            matches!(err, TrackerError::StateNotFound(_)),
            "got {err:?}, want StateNotFound"
        );
        assert_eq!(
            mutate_calls.load(Ordering::SeqCst),
            0,
            "no mutation when no state of the type exists"
        );
    }
}
