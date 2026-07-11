//! linear-stub — a scripted Linear GraphQL double for the Rhapsody parity harness.
//!
//! It answers exactly the operations Go Symphony's Linear adapter issues (enumerated from
//! `$REF/internal/tracker/linear/query.go` — read the source, do not guess), driven by a
//! scenario JSON file, so the reference Go daemon (and later `rhapsody-tracker`'s tests) can
//! poll → claim → dispatch → run against it with no network and no Linear account. Serves
//! `POST /graphql`; routes by the document's operation name.
//!
//! # Enumerated GraphQL operations (query.go)
//!
//! Reads (queries), answered from scenario data:
//! - **`Viewer`** — `viewer { id name displayName email organization { urlKey } }`; no vars.
//!   Answered from `scenario.viewer` (displayName = name; email/urlKey synthesized empty). The
//!   daemon binds its candidate filter to this id, so it must be non-empty.
//! - **`Candidates`** — full issue nodes for a project in `$states` (active ∪ review), paginated.
//!   Vars: `projectSlug`, `states`, `first`, `after`, `assigneeID` (assignee mode) / none (pool);
//!   optionally `milestoneID`. Answered from `scenario.issues` filtered by state name.
//! - **`BacklogCandidates`** — full issue nodes in the Backlog state TYPE (DAG auto-promote).
//!   Vars: `projectSlug`, `first`, `after`, `assigneeID`, optional `milestoneID`. Answered from
//!   issues whose state is the stub's Backlog state.
//! - **`ByStates`** — minimal issue nodes (`id identifier title state`) for a project in `$states`.
//!   Vars: `projectSlug`, `states`, `first`, `after`.
//! - **`ByIDs`** — minimal issue nodes for the given tracker `$ids` (reconcile). Vars: `ids`, `first`.
//! - **`BranchByID`** — `id branchName attachments` for `$ids` (graphite stacking hint). Vars: `ids`, `first`.
//! - **`Projects`** — workspace projects (`id name slugId color teams`). Vars: `first`, `after`.
//!   Answered from `scenario.project` (a single project) + a synthesized team.
//! - **`ProjectMilestones`** — a project's milestones (`id name`). Vars: `projectSlug`, `first`, `after`.
//!   The v1 scenario has no milestones → empty.
//! - **`TeamWorkflowStates`** — a team's workflow states (`id name type position`). Vars: `teamID`.
//!   Answered from the stub's fixed [`WORKFLOW_STATES`] table (the ids `MoveIssueState` round-trips).
//! - **`IssueComments`** — an issue's comments (`id body createdAt`), paginated. Vars: `id`, `first`, `after`.
//! - **`IssueAssignee`** — an issue's current assignee id (pool read-back). Vars: `id`.
//!
//! Writes (mutations), mutating in-memory state so multi-step runs behave:
//! - **`MoveIssueState`** — `issueUpdate(id, input:{stateId}) { success }`; sets the issue's state
//!   (`stateId` is resolved to a name via [`WORKFLOW_STATES`]). Vars: `id`, `stateId`.
//! - **`AssignIssue`** — `issueUpdate(id, input:{assigneeId}) { success }`; sets the assignee.
//!   Vars: `id`, `assigneeId`.
//! - **`CreateComment`** — `commentCreate { success comment { id createdAt } }`; appends a comment.
//!   Vars: `issueId`, `body`.
//! - **`DeleteComment`** — `commentDelete { success }`; removes a comment by id. Vars: `id`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use axum::{Json, Router, extract::State, routing::post};
use serde::Deserialize;
use serde_json::{Value, json};

pub mod scenario;

use scenario::Scenario;

/// The stub's single team id: every issue node reports `team { id }` = this, so
/// `TeamWorkflowStates(teamID:)` resolves to [`WORKFLOW_STATES`] regardless of the scenario.
const TEAM_ID: &str = "team_stub";

/// One workflow state in the stub's fixed table. `id` is the token `MoveIssueState`'s
/// `issueUpdate(stateId:)` carries, so it must round-trip back to `name` here; `kind` is
/// Linear's state `type` (backlog|unstarted|started|completed|canceled), used by the daemon's
/// type-based moves.
struct WorkflowState {
    id: &'static str,
    name: &'static str,
    kind: &'static str,
    position: u32,
}

/// Fixed workflow-state table (stable ids so `MoveIssueState` round-trips id → name). Covers the
/// state names the harness scenarios use and one state per Linear state type.
const WORKFLOW_STATES: &[WorkflowState] = &[
    WorkflowState {
        id: "state_backlog",
        name: "Backlog",
        kind: "backlog",
        position: 0,
    },
    WorkflowState {
        id: "state_todo",
        name: "Todo",
        kind: "unstarted",
        position: 1,
    },
    WorkflowState {
        id: "state_in_progress",
        name: "In Progress",
        kind: "started",
        position: 2,
    },
    WorkflowState {
        id: "state_in_review",
        name: "In Review",
        kind: "started",
        position: 3,
    },
    WorkflowState {
        id: "state_done",
        name: "Done",
        kind: "completed",
        position: 4,
    },
    WorkflowState {
        id: "state_canceled",
        name: "Canceled",
        kind: "canceled",
        position: 5,
    },
];

/// A comment created during a run (mutations persist within a stub process).
struct StoredComment {
    id: String,
    body: String,
    created_at: String,
}

/// The mutable stub world: the loaded scenario plus per-run mutations. Issue-state changes apply
/// directly to `scenario.issues[i].state`; comments and assignee overrides live alongside.
pub struct StubState {
    scenario: Scenario,
    comments: HashMap<String, Vec<StoredComment>>,
    assignees: HashMap<String, String>,
    comment_seq: u64,
}

impl StubState {
    fn new(scenario: Scenario) -> Self {
        Self {
            scenario,
            comments: HashMap::new(),
            assignees: HashMap::new(),
            comment_seq: 0,
        }
    }
}

type Shared = Arc<Mutex<StubState>>;

/// Build the stub's axum app for a scenario. State is `Arc<Mutex<StubState>>` so mutations
/// (issue-state updates, comments, assignee) persist across requests within a run.
pub fn router(scenario: Scenario) -> Router {
    let state: Shared = Arc::new(Mutex::new(StubState::new(scenario)));
    Router::new()
        .route("/graphql", post(graphql))
        .with_state(state)
}

/// The `{ "query": "...", "variables": {...} }` envelope the Go client posts (`doGraphQL`).
#[derive(Deserialize)]
struct GraphQlRequest {
    query: String,
    #[serde(default)]
    variables: Value,
}

/// `POST /graphql`: route by operation name and answer from (mutable) scenario state. Always
/// replies 200 with a GraphQL envelope — `{ "data": ... }` or `{ "errors": [...] }` — matching
/// Linear's transport (the Go client distinguishes the two).
async fn graphql(State(state): State<Shared>, Json(req): Json<GraphQlRequest>) -> Json<Value> {
    let op = operation_name(&req.query).unwrap_or("");
    let result = {
        let mut guard = state.lock().unwrap_or_else(PoisonError::into_inner);
        dispatch(&mut guard, op, &req.variables)
    };
    Json(match result {
        Ok(data) => json!({ "data": data }),
        Err(message) => json!({ "errors": [ { "message": message } ] }),
    })
}

/// Route an operation to its response `data`, or an error message for an unknown operation.
fn dispatch(st: &mut StubState, op: &str, vars: &Value) -> Result<Value, String> {
    match op {
        "Viewer" => Ok(viewer(st)),
        "Candidates" => Ok(issues_page(nodes_matching_states(st, vars, full_node))),
        "BacklogCandidates" => Ok(issues_page(backlog_nodes(st))),
        "ByStates" => Ok(issues_page(nodes_matching_states(st, vars, minimal_node))),
        "ByIDs" => Ok(issues_connection(nodes_matching_ids(
            st,
            vars,
            minimal_node,
        ))),
        "BranchByID" => Ok(issues_connection(nodes_matching_ids(st, vars, branch_node))),
        "Projects" => Ok(projects_page(st)),
        "ProjectMilestones" => Ok(milestones_page()),
        "TeamWorkflowStates" => Ok(team_workflow_states()),
        "IssueComments" => Ok(issue_comments(st, vars)),
        "IssueAssignee" => Ok(issue_assignee(st, vars)),
        "MoveIssueState" => Ok(move_issue_state(st, vars)),
        "AssignIssue" => Ok(assign_issue(st, vars)),
        "CreateComment" => Ok(create_comment(st, vars)),
        "DeleteComment" => Ok(delete_comment(st, vars)),
        other => Err(format!("linear-stub: unknown operation {other:?}")),
    }
}

/// Extract the GraphQL operation name: the identifier following the leading `query`/`mutation`
/// keyword (Linear's adapter always names its operations). Returns `None` for an anonymous or
/// keyword-less document.
fn operation_name(query: &str) -> Option<&str> {
    let mut tokens = query.split_whitespace();
    loop {
        let token = tokens.next()?;
        if token == "query" || token == "mutation" {
            let next = tokens.next()?;
            // The name may abut its arguments/selection: `Candidates($x:...)` or `Viewer{`.
            let name = match next.find(['(', '{']) {
                Some(end) => &next[..end],
                None => next,
            };
            if !name.is_empty() {
                return Some(name);
            }
        }
    }
}

// --- response builders -------------------------------------------------------------------

/// `{ "issues": { "nodes": [...], "pageInfo": {...} } }` — the paginated connection shape
/// (`Candidates`/`BacklogCandidates`/`ByStates`). One page, never a next page.
fn issues_page(nodes: Vec<Value>) -> Value {
    json!({ "issues": { "nodes": nodes, "pageInfo": page_info() } })
}

/// `{ "issues": { "nodes": [...] } }` — the un-paginated connection shape (`ByIDs`/`BranchByID`).
fn issues_connection(nodes: Vec<Value>) -> Value {
    json!({ "issues": { "nodes": nodes } })
}

fn page_info() -> Value {
    json!({ "hasNextPage": false, "endCursor": Value::Null })
}

fn viewer(st: &StubState) -> Value {
    let v = &st.scenario.viewer;
    json!({ "viewer": {
        "id": v.id,
        "name": v.name,
        "displayName": v.name,
        "email": "",
        "organization": { "urlKey": "" },
    } })
}

fn projects_page(st: &StubState) -> Value {
    let p = &st.scenario.project;
    json!({ "projects": {
        "nodes": [ {
            "id": p.id,
            "name": p.name,
            "slugId": p.slug_id,
            "color": "#5e6ad2",
            "teams": { "nodes": [ { "key": "STUB", "name": "Stub Team" } ] },
        } ],
        "pageInfo": page_info(),
    } })
}

fn milestones_page() -> Value {
    json!({ "projectMilestones": { "nodes": [], "pageInfo": page_info() } })
}

fn team_workflow_states() -> Value {
    let nodes: Vec<Value> = WORKFLOW_STATES
        .iter()
        .map(|w| json!({ "id": w.id, "name": w.name, "type": w.kind, "position": w.position }))
        .collect();
    json!({ "workflowStates": { "nodes": nodes } })
}

fn issue_comments(st: &StubState, vars: &Value) -> Value {
    let id = str_var(vars, "id");
    json!({ "issue": { "comments": {
        "nodes": comment_nodes(st, id, true),
        "pageInfo": page_info(),
    } } })
}

fn issue_assignee(st: &StubState, vars: &Value) -> Value {
    let id = str_var(vars, "id");
    let assignee = match st.assignees.get(id) {
        Some(a) if !a.is_empty() => json!({ "id": a }),
        _ => Value::Null,
    };
    json!({ "issue": { "assignee": assignee } })
}

// --- mutations ---------------------------------------------------------------------------

fn move_issue_state(st: &mut StubState, vars: &Value) -> Value {
    let id = str_var(vars, "id").to_owned();
    let state_id = str_var(vars, "stateId");
    let resolved = WORKFLOW_STATES
        .iter()
        .find(|w| w.id == state_id)
        .map(|w| w.name.to_owned());
    if let Some(name) = resolved
        && let Some(issue) = st.scenario.issues.iter_mut().find(|i| i.id == id)
    {
        issue.state = name;
    }
    json!({ "issueUpdate": { "success": true } })
}

fn assign_issue(st: &mut StubState, vars: &Value) -> Value {
    let id = str_var(vars, "id").to_owned();
    let assignee = str_var(vars, "assigneeId").to_owned();
    st.assignees.insert(id, assignee);
    json!({ "issueUpdate": { "success": true } })
}

fn create_comment(st: &mut StubState, vars: &Value) -> Value {
    let issue_id = str_var(vars, "issueId").to_owned();
    let body = str_var(vars, "body").to_owned();
    let seq = st.comment_seq;
    st.comment_seq += 1;
    let id = format!("comment_{seq}");
    let created_at = synth_timestamp(seq);
    st.comments
        .entry(issue_id)
        .or_default()
        .push(StoredComment {
            id: id.clone(),
            body,
            created_at: created_at.clone(),
        });
    json!({ "commentCreate": { "success": true, "comment": { "id": id, "createdAt": created_at } } })
}

fn delete_comment(st: &mut StubState, vars: &Value) -> Value {
    let id = str_var(vars, "id");
    for comments in st.comments.values_mut() {
        comments.retain(|c| c.id != id);
    }
    json!({ "commentDelete": { "success": true } })
}

// --- node builders -----------------------------------------------------------------------

/// Nodes for the scenario issues whose state name is in the request's `states` filter (absent
/// filter ⇒ all), mapped through `build` (full vs minimal node shape).
fn nodes_matching_states(
    st: &StubState,
    vars: &Value,
    build: fn(&StubState, &scenario::Issue) -> Value,
) -> Vec<Value> {
    let states = str_array(vars, "states");
    st.scenario
        .issues
        .iter()
        .filter(|i| match &states {
            Some(wanted) => wanted.iter().any(|s| s == &i.state),
            None => true,
        })
        .map(|i| build(st, i))
        .collect()
}

/// Nodes for the scenario issues whose `id` is in the request's `ids` filter, mapped through `build`.
fn nodes_matching_ids(
    st: &StubState,
    vars: &Value,
    build: fn(&StubState, &scenario::Issue) -> Value,
) -> Vec<Value> {
    let ids = str_array(vars, "ids").unwrap_or_default();
    st.scenario
        .issues
        .iter()
        .filter(|i| ids.iter().any(|id| id == &i.id))
        .map(|i| build(st, i))
        .collect()
}

/// Full backlog-state issue nodes (`BacklogCandidates`): issues whose state is the stub's
/// Backlog-typed state.
fn backlog_nodes(st: &StubState) -> Vec<Value> {
    let backlog: Vec<&str> = WORKFLOW_STATES
        .iter()
        .filter(|w| w.kind == "backlog")
        .map(|w| w.name)
        .collect();
    st.scenario
        .issues
        .iter()
        .filter(|i| backlog.contains(&i.state.as_str()))
        .map(|i| full_node(st, i))
        .collect()
}

/// The full candidate node selection (`queryCandidates`/`queryBacklogCandidates`). Fields the
/// scenario does not carry are synthesized to normalize-safe zero values.
fn full_node(st: &StubState, i: &scenario::Issue) -> Value {
    json!({
        "id": i.id,
        "identifier": i.identifier,
        "title": i.title,
        "description": i.description,
        "priority": 0,
        "url": Value::Null,
        "branchName": Value::Null,
        "createdAt": Value::Null,
        "updatedAt": Value::Null,
        "state": { "name": i.state },
        "team": { "id": TEAM_ID },
        "assignee": assignee_node(st, &i.id),
        "projectMilestone": Value::Null,
        "labels": { "nodes": i.labels.iter().map(|l| json!({ "name": l })).collect::<Vec<_>>() },
        "inverseRelations": { "nodes": blocked_by_nodes(st, i) },
        "attachments": { "nodes": [] },
        "comments": { "nodes": comment_nodes(st, &i.id, false) },
    })
}

/// The minimal node selection (`queryByStates`/`queryByIDs`): `id identifier title state{name}`.
fn minimal_node(_st: &StubState, i: &scenario::Issue) -> Value {
    json!({ "id": i.id, "identifier": i.identifier, "title": i.title, "state": { "name": i.state } })
}

/// The `queryBranchByID` node: `id branchName attachments`.
fn branch_node(_st: &StubState, i: &scenario::Issue) -> Value {
    json!({ "id": i.id, "branchName": Value::Null, "attachments": { "nodes": [] } })
}

fn assignee_node(st: &StubState, issue_id: &str) -> Value {
    match st.assignees.get(issue_id) {
        Some(a) if !a.is_empty() => json!({ "id": a, "displayName": "" }),
        _ => Value::Null,
    }
}

/// `inverseRelations` "blocks" edges from an issue's `blockedBy` list. Each entry is resolved to a
/// scenario issue by id or identifier when present (so its live state is reported), else echoed.
fn blocked_by_nodes(st: &StubState, i: &scenario::Issue) -> Vec<Value> {
    i.blocked_by
        .iter()
        .map(|b| {
            let (id, identifier, state) = match st.scenario.issues.iter().find(|x| &x.id == b || &x.identifier == b) {
                Some(x) => (x.id.clone(), x.identifier.clone(), x.state.clone()),
                None => (b.clone(), b.clone(), String::new()),
            };
            json!({ "type": "blocks", "issue": { "id": id, "identifier": identifier, "state": { "name": state } } })
        })
        .collect()
}

/// An issue's comments, newest-first (Linear's documented order). `with_id` selects the
/// `IssueComments` shape (`id body createdAt`) vs the candidate-node shape (`createdAt body`).
fn comment_nodes(st: &StubState, issue_id: &str, with_id: bool) -> Vec<Value> {
    let Some(comments) = st.comments.get(issue_id) else {
        return Vec::new();
    };
    comments
        .iter()
        .rev()
        .map(|c| {
            if with_id {
                json!({ "id": c.id, "body": c.body, "createdAt": c.created_at })
            } else {
                json!({ "createdAt": c.created_at, "body": c.body })
            }
        })
        .collect()
}

// --- variable helpers --------------------------------------------------------------------

/// A required string variable, or `""` when absent (the daemon always supplies its declared vars).
fn str_var<'a>(vars: &'a Value, key: &str) -> &'a str {
    vars.get(key).and_then(Value::as_str).unwrap_or("")
}

/// A `[String!]` variable as owned strings, or `None` when the key is absent/not an array.
fn str_array(vars: &Value, key: &str) -> Option<Vec<String>> {
    vars.get(key).and_then(Value::as_array).map(|items| {
        items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect()
    })
}

/// A deterministic, strictly-increasing, valid RFC3339 timestamp for the nth created comment, so
/// comment ordering (and fixture capture) is reproducible. `seq` is tiny in practice.
fn synth_timestamp(seq: u64) -> String {
    let seconds = seq % 60;
    let minutes = (seq / 60) % 60;
    let hours = (seq / 3600) % 24;
    format!("2020-01-01T{hours:02}:{minutes:02}:{seconds:02}Z")
}

#[cfg(test)]
mod fake_claude_tests {
    use std::process::{Command, Stdio};

    use serde_json::Value;

    // Step 2/3: the copied fake-claude speaks the Claude Code stream-json protocol the Go runner
    // (and the Rhapsody port) parses — first line `system`/init, terminal `result` is_error=false.
    #[test]
    fn fake_claude_emits_valid_protocol() {
        let out = Command::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../fake-claude"))
            .env("FAKE_CLAUDE_SLEEP_S", "0")
            .stdin(Stdio::null())
            .output()
            .expect("fake-claude runs");
        assert!(out.status.success(), "fake-claude exited non-zero");
        let lines: Vec<Value> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| serde_json::from_str(l).expect("every line is JSON"))
            .collect();
        assert_eq!(lines.first().expect("at least one line")["type"], "system");
        let last = lines.last().expect("a terminal line");
        assert_eq!(last["type"], "result");
        assert_eq!(last["is_error"], false);
    }

    #[test]
    fn fake_claude_error_wrapper_reports_failure() {
        let out = Command::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../fake-claude-error"))
            .env("FAKE_CLAUDE_SLEEP_S", "0")
            .stdin(Stdio::null())
            .output()
            .expect("fake-claude-error runs");
        assert!(out.status.success());
        let last: Value =
            serde_json::from_str(String::from_utf8_lossy(&out.stdout).lines().last().unwrap())
                .expect("terminal line is JSON");
        assert_eq!(last["type"], "result");
        assert_eq!(last["is_error"], true);
    }
}

#[cfg(test)]
mod stub_tests {
    use super::*;
    use tokio::net::TcpListener;

    // Real Linear operation documents lifted from $REF/internal/tracker/linear/query.go, so the
    // tests exercise the stub through the exact request bodies the Go adapter sends.
    const Q_VIEWER: &str =
        "query Viewer { viewer { id name displayName email organization { urlKey } } }";
    const Q_CANDIDATES: &str = "query Candidates($projectSlug: String!, $states: [String!], $first: Int!, $after: String, $assigneeID: ID!) { issues(first: $first, after: $after, filter: { project: { slugId: { eq: $projectSlug } }, state: { name: { in: $states } }, assignee: { id: { eq: $assigneeID } } }) { nodes { id identifier title description priority url branchName createdAt updatedAt state { name } team { id } assignee { id displayName } projectMilestone { id name } labels { nodes { name } } inverseRelations { nodes { type issue { id identifier state { name } } } } attachments(first: 100) { nodes { sourceType metadata } } comments(first: 50) { nodes { createdAt body } } } pageInfo { hasNextPage endCursor } } }";
    const Q_BACKLOG: &str = "query BacklogCandidates($projectSlug: String!, $first: Int!, $after: String, $assigneeID: ID!) { issues(first: $first, after: $after, filter: { project: { slugId: { eq: $projectSlug } }, state: { type: { eq: \"backlog\" } }, assignee: { id: { eq: $assigneeID } } }) { nodes { id identifier state { name } } pageInfo { hasNextPage endCursor } } }";
    const Q_BY_STATES: &str = "query ByStates($projectSlug: String!, $states: [String!], $first: Int!, $after: String) { issues(first: $first, after: $after, filter: { project: { slugId: { eq: $projectSlug } }, state: { name: { in: $states } } }) { nodes { id identifier title state { name } } pageInfo { hasNextPage endCursor } } }";
    const Q_BY_IDS: &str = "query ByIDs($ids: [ID!], $first: Int!) { issues(first: $first, filter: { id: { in: $ids } }) { nodes { id identifier title state { name } } } }";
    const Q_BRANCH_BY_ID: &str = "query BranchByID($ids: [ID!], $first: Int!) { issues(first: $first, filter: { id: { in: $ids } }) { nodes { id branchName attachments(first: 100) { nodes { sourceType metadata } } } } }";
    const Q_PROJECTS: &str = "query Projects($first: Int!, $after: String) { projects(first: $first, after: $after) { nodes { id name slugId color teams(first: 1) { nodes { key name } } } pageInfo { hasNextPage endCursor } } }";
    const Q_MILESTONES: &str = "query ProjectMilestones($projectSlug: String!, $first: Int!, $after: String) { projectMilestones(first: $first, after: $after, filter: { project: { slugId: { eq: $projectSlug } } }) { nodes { id name } pageInfo { hasNextPage endCursor } } }";
    const Q_WORKFLOW_STATES: &str = "query TeamWorkflowStates($teamID: ID!) { workflowStates(filter: { team: { id: { eq: $teamID } } }) { nodes { id name type position } } }";
    const Q_ISSUE_COMMENTS: &str = "query IssueComments($id: String!, $first: Int!, $after: String) { issue(id: $id) { comments(first: $first, after: $after) { nodes { id body createdAt } pageInfo { hasNextPage endCursor } } } }";
    const Q_ISSUE_ASSIGNEE: &str =
        "query IssueAssignee($id: String!) { issue(id: $id) { assignee { id } } }";
    const M_MOVE_STATE: &str = "mutation MoveIssueState($id: String!, $stateId: String!) { issueUpdate(id: $id, input: { stateId: $stateId }) { success } }";
    const M_ASSIGN: &str = "mutation AssignIssue($id: String!, $assigneeId: String!) { issueUpdate(id: $id, input: { assigneeId: $assigneeId }) { success } }";
    const M_CREATE_COMMENT: &str = "mutation CreateComment($issueId: String!, $body: String!) { commentCreate(input: { issueId: $issueId, body: $body }) { success comment { id createdAt } } }";
    const M_DELETE_COMMENT: &str =
        "mutation DeleteComment($id: String!) { commentDelete(id: $id) { success } }";

    /// Bind an ephemeral loopback port, serve the basic scenario, and return the `/graphql` URL.
    /// The listener is bound before serving, so a request issued immediately never races startup.
    async fn spawn_basic() -> String {
        let scn = Scenario::from_path(concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/basic.json"))
            .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router(scn)).await.unwrap() });
        format!("http://{addr}/graphql")
    }

    /// POST a GraphQL operation to the stub via curl (zero extra deps; present on the macOS CI
    /// target) and return the decoded response envelope.
    async fn gql(url: &str, query: &str, variables: Value) -> Value {
        let body =
            serde_json::to_string(&json!({ "query": query, "variables": variables })).unwrap();
        let out = tokio::process::Command::new("curl")
            .args([
                "-fsS",
                "-X",
                "POST",
                "-H",
                "Content-Type: application/json",
                "--data-binary",
                &body,
                url,
            ])
            .output()
            .await
            .unwrap();
        assert!(
            out.status.success(),
            "curl failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn answers_viewer() {
        let url = spawn_basic().await;
        let r = gql(&url, Q_VIEWER, json!({})).await;
        assert_eq!(r["data"]["viewer"]["id"], "usr_stub");
        assert_eq!(r["data"]["viewer"]["name"], "symphony-stub");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn answers_candidates_for_project() {
        let url = spawn_basic().await;
        let vars = json!({ "projectSlug": "558008ab185c", "states": ["Todo", "In Progress"], "first": 50, "after": null, "assigneeID": "usr_stub" });
        let r = gql(&url, Q_CANDIDATES, vars).await;
        let node = &r["data"]["issues"]["nodes"][0];
        assert_eq!(node["identifier"], "RHA-1");
        assert_eq!(node["state"]["name"], "Todo");
        assert_eq!(node["team"]["id"], "team_stub");
        assert_eq!(r["data"]["issues"]["pageInfo"]["hasNextPage"], false);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn candidates_filters_by_state() {
        let url = spawn_basic().await;
        // RHA-1 is Todo; asking only for Done returns no candidates.
        let vars = json!({ "projectSlug": "558008ab185c", "states": ["Done"], "first": 50, "after": null, "assigneeID": "usr_stub" });
        let r = gql(&url, Q_CANDIDATES, vars).await;
        assert_eq!(r["data"]["issues"]["nodes"].as_array().unwrap().len(), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn answers_backlog_candidates() {
        let url = spawn_basic().await;
        let vars = json!({ "projectSlug": "558008ab185c", "first": 50, "after": null, "assigneeID": "usr_stub" });
        let r = gql(&url, Q_BACKLOG, vars).await;
        // No Backlog-state issue in the basic scenario.
        assert_eq!(r["data"]["issues"]["nodes"].as_array().unwrap().len(), 0);
        assert!(r["data"]["issues"]["pageInfo"].is_object());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn answers_by_states() {
        let url = spawn_basic().await;
        let vars = json!({ "projectSlug": "558008ab185c", "states": ["Todo"], "first": 50, "after": null });
        let r = gql(&url, Q_BY_STATES, vars).await;
        assert_eq!(r["data"]["issues"]["nodes"][0]["identifier"], "RHA-1");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn answers_by_ids() {
        let url = spawn_basic().await;
        let r = gql(&url, Q_BY_IDS, json!({ "ids": ["iss_1"], "first": 1 })).await;
        assert_eq!(r["data"]["issues"]["nodes"][0]["identifier"], "RHA-1");
        // ByIDs has no pageInfo in the query selection.
        assert!(r["data"]["issues"]["pageInfo"].is_null());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn answers_branch_by_id() {
        let url = spawn_basic().await;
        let r = gql(
            &url,
            Q_BRANCH_BY_ID,
            json!({ "ids": ["iss_1"], "first": 1 }),
        )
        .await;
        let node = &r["data"]["issues"]["nodes"][0];
        assert_eq!(node["id"], "iss_1");
        assert!(node.get("branchName").is_some());
        assert!(node["attachments"]["nodes"].is_array());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn answers_projects() {
        let url = spawn_basic().await;
        let r = gql(&url, Q_PROJECTS, json!({ "first": 50, "after": null })).await;
        let node = &r["data"]["projects"]["nodes"][0];
        assert_eq!(node["slugId"], "558008ab185c");
        assert_eq!(node["teams"]["nodes"][0]["key"], "STUB");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn answers_project_milestones() {
        let url = spawn_basic().await;
        let vars = json!({ "projectSlug": "558008ab185c", "first": 50, "after": null });
        let r = gql(&url, Q_MILESTONES, vars).await;
        assert_eq!(
            r["data"]["projectMilestones"]["nodes"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn answers_team_workflow_states() {
        let url = spawn_basic().await;
        let r = gql(&url, Q_WORKFLOW_STATES, json!({ "teamID": "team_stub" })).await;
        let nodes = r["data"]["workflowStates"]["nodes"].as_array().unwrap();
        assert!(
            nodes
                .iter()
                .any(|n| n["name"] == "Todo" && n["type"] == "unstarted")
        );
        assert!(nodes.iter().any(|n| n["name"] == "In Progress"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn answers_issue_comments_initially_empty() {
        let url = spawn_basic().await;
        let r = gql(
            &url,
            Q_ISSUE_COMMENTS,
            json!({ "id": "iss_1", "first": 50, "after": null }),
        )
        .await;
        assert_eq!(
            r["data"]["issue"]["comments"]["nodes"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn answers_issue_assignee_unassigned() {
        let url = spawn_basic().await;
        let r = gql(&url, Q_ISSUE_ASSIGNEE, json!({ "id": "iss_1" })).await;
        assert!(r["data"]["issue"]["assignee"].is_null());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn move_issue_state_mutates_and_persists() {
        let url = spawn_basic().await;
        // Resolve "In Progress" to its stub state id, move RHA-1 there, and read the new state back.
        let states = gql(&url, Q_WORKFLOW_STATES, json!({ "teamID": "team_stub" })).await;
        let state_id = states["data"]["workflowStates"]["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["name"] == "In Progress")
            .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let moved = gql(
            &url,
            M_MOVE_STATE,
            json!({ "id": "iss_1", "stateId": state_id }),
        )
        .await;
        assert_eq!(moved["data"]["issueUpdate"]["success"], true);
        let after = gql(&url, Q_BY_IDS, json!({ "ids": ["iss_1"], "first": 1 })).await;
        assert_eq!(
            after["data"]["issues"]["nodes"][0]["state"]["name"],
            "In Progress"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn assign_issue_then_read_back() {
        let url = spawn_basic().await;
        let assigned = gql(
            &url,
            M_ASSIGN,
            json!({ "id": "iss_1", "assigneeId": "usr_stub" }),
        )
        .await;
        assert_eq!(assigned["data"]["issueUpdate"]["success"], true);
        let r = gql(&url, Q_ISSUE_ASSIGNEE, json!({ "id": "iss_1" })).await;
        assert_eq!(r["data"]["issue"]["assignee"]["id"], "usr_stub");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_comment_then_list_and_delete() {
        let url = spawn_basic().await;
        let created = gql(
            &url,
            M_CREATE_COMMENT,
            json!({ "issueId": "iss_1", "body": "@symphony claim" }),
        )
        .await;
        assert_eq!(created["data"]["commentCreate"]["success"], true);
        let comment_id = created["data"]["commentCreate"]["comment"]["id"]
            .as_str()
            .unwrap()
            .to_owned();

        let listed = gql(
            &url,
            Q_ISSUE_COMMENTS,
            json!({ "id": "iss_1", "first": 50, "after": null }),
        )
        .await;
        let nodes = listed["data"]["issue"]["comments"]["nodes"]
            .as_array()
            .unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0]["body"], "@symphony claim");

        let deleted = gql(&url, M_DELETE_COMMENT, json!({ "id": comment_id })).await;
        assert_eq!(deleted["data"]["commentDelete"]["success"], true);
        let empty = gql(
            &url,
            Q_ISSUE_COMMENTS,
            json!({ "id": "iss_1", "first": 50, "after": null }),
        )
        .await;
        assert_eq!(
            empty["data"]["issue"]["comments"]["nodes"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unknown_operation_returns_graphql_error() {
        let url = spawn_basic().await;
        let r = gql(&url, "query Nope { viewer { id } }", json!({})).await;
        assert!(r["errors"].is_array());
        assert!(r.get("data").is_none());
    }
}
