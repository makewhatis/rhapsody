//! GraphQL operations — parity port of `internal/tracker/linear/query.go` (upstream §11.2).
//!
//! The query TEXT is an observable contract: the Go daemon posts these exact documents, the R3
//! `linear-stub` matches on them, and Linear itself validates the variable typing (e.g. an id
//! filter fed a `String!` variable is rejected with HTTP 400). Every operation here is therefore
//! kept **byte-identical** to `query.go` — including the leading newline, two-space indentation,
//! and clause ordering — and the tests below assert that byte-for-byte.
//!
//! The parameterised documents (`query_candidates`, `query_backlog_candidates`) are assembled from
//! literal chunks exactly as Go's `fmt.Sprintf` templates are, so the four/two variants differ
//! only in the assignee/milestone variable declarations and filter clauses.
//!
//! Visibility: this module is `pub` because the query strings are the adapter's observable
//! contract (asserted here, consumed by the read/write paths in P3 T4/T5). `rhapsody-tracker` is
//! an internal workspace crate, so this exposes no external API-stability surface.

// ─── candidate builders (fmt.Sprintf templates in Go) ──────────────────────────────────────────

/// Head of `queryCandidates`, up to (but excluding) the `$assigneeID` / `$milestoneID` variable
/// declarations.
const CANDIDATES_HEAD: &str = r#"
query Candidates($projectSlug: String!, $states: [String!], $first: Int!, $after: String"#;

/// Middle of `queryCandidates`: the query body up to (but excluding) the assignee / milestone
/// filter clauses. Ends with the `, ` that precedes them.
const CANDIDATES_MID: &str = r#") {
  issues(
    first: $first
    after: $after
    filter: { project: { slugId: { eq: $projectSlug } }, state: { name: { in: $states } }, "#;

/// Head of `queryBacklogCandidates`, up to (but excluding) the `$milestoneID` variable
/// declaration. The `$assigneeID: ID!` variable is always present (Backlog is always
/// assignee-scoped), unlike candidates where it varies with claim mode.
const BACKLOG_HEAD: &str = r#"
query BacklogCandidates($projectSlug: String!, $first: Int!, $after: String, $assigneeID: ID!"#;

/// Middle of `queryBacklogCandidates`: the query body up to (but excluding) the milestone filter
/// clause. Filters by the Backlog state TYPE (config-free) + assignee.
const BACKLOG_MID: &str = r#") {
  issues(
    first: $first
    after: $after
    filter: { project: { slugId: { eq: $projectSlug } }, state: { type: { eq: "backlog" } }, assignee: { id: { eq: $assigneeID } }"#;

/// Shared tail of `queryCandidates` and `queryBacklogCandidates` — the full issue node selection.
/// Both queries select the identical fields (the PR-aware dispatch guard + blocker edges + branch
/// + team), so the tail is one constant. Begins with the ` }` that closes the filter object.
const FULL_NODE_TAIL: &str = r#" }
  ) {
    nodes {
      id
      identifier
      title
      description
      priority
      url
      branchName
      createdAt
      updatedAt
      state { name }
      team { id }
      assignee { id displayName }
      projectMilestone { id name }
      labels { nodes { name } }
      inverseRelations { nodes { type issue { id identifier state { name } } } }
      attachments(first: 100) { nodes { sourceType metadata } }
      comments(first: 50) { nodes { createdAt body } }
    }
    pageInfo { hasNextPage endCursor }
  }
}"#;

/// Builds the `Candidates` query (mirrors Go `queryCandidates`). Full issue nodes for a project
/// filtered by state name, paginated.
///
/// `with_milestone` adds the `$milestoneID: ID!` variable + a `projectMilestone` filter clause.
/// `pool` (INF-477) switches the assignee clause from `assignee: { id: { eq: $assigneeID } }`
/// (assignee mode — narrows to the API key owner, declaring `$assigneeID: ID!`) to
/// `assignee: { null: true }` (pool mode — the shared unassigned pool, declaring no `$assigneeID`).
///
/// Both `$assigneeID` and `$milestoneID` are typed `ID!` (not `String!`): the id comparator
/// expects `ID`, and a `String!` variable at an id position fails GraphQL validation (HTTP 400).
pub fn query_candidates(with_milestone: bool, pool: bool) -> String {
    // assignee clause + its variable declaration vary by claim mode. Pool mode filters unassigned
    // issues and declares no $assigneeID (an undeclared-but-referenced variable would 400).
    let (assignee_var, assignee_filter): (&str, &str) = if pool {
        ("", "assignee: { null: true }")
    } else {
        (
            ", $assigneeID: ID!",
            "assignee: { id: { eq: $assigneeID } }",
        )
    };
    let (milestone_var, milestone_filter): (&str, &str) = if with_milestone {
        (
            ", $milestoneID: ID!",
            ", projectMilestone: { id: { eq: $milestoneID } }",
        )
    } else {
        ("", "")
    };
    let mut q = String::with_capacity(
        CANDIDATES_HEAD.len() + CANDIDATES_MID.len() + FULL_NODE_TAIL.len() + 96,
    );
    q.push_str(CANDIDATES_HEAD);
    q.push_str(assignee_var);
    q.push_str(milestone_var);
    q.push_str(CANDIDATES_MID);
    q.push_str(assignee_filter);
    q.push_str(milestone_filter);
    q.push_str(FULL_NODE_TAIL);
    q
}

/// Builds the `BacklogCandidates` query (mirrors Go `queryBacklogCandidates`). Full issue nodes
/// (same selection as `query_candidates`) filtered to the Backlog state TYPE and the API key
/// owner's assigned issues, paginated. `with_milestone` adds the `$milestoneID: ID!` variable + a
/// `projectMilestone` filter clause, which MUST match `query_candidates`' so the DAG auto-promote
/// pass and the candidate poll see the same issue set. INF-318.
pub fn query_backlog_candidates(with_milestone: bool) -> String {
    let (milestone_var, milestone_filter): (&str, &str) = if with_milestone {
        (
            ", $milestoneID: ID!",
            ", projectMilestone: { id: { eq: $milestoneID } }",
        )
    } else {
        ("", "")
    };
    let mut q =
        String::with_capacity(BACKLOG_HEAD.len() + BACKLOG_MID.len() + FULL_NODE_TAIL.len() + 64);
    q.push_str(BACKLOG_HEAD);
    q.push_str(milestone_var);
    q.push_str(BACKLOG_MID);
    q.push_str(milestone_filter);
    q.push_str(FULL_NODE_TAIL);
    q
}

// ─── static operations ─────────────────────────────────────────────────────────────────────────

/// `queryViewer` — resolves the owner of the configured API key. No variables.
pub const QUERY_VIEWER: &str = r#"
query Viewer {
  viewer { id name displayName email organization { urlKey } }
}"#;

/// `queryProjects` — lists the workspace's projects for the add-agent picker (INF-224). Paginated.
pub const QUERY_PROJECTS: &str = r#"
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

/// `queryProjectMilestones` — lists a project's milestones (id + name) for resolving a configured
/// milestone NAME to its ID. Paginated.
pub const QUERY_PROJECT_MILESTONES: &str = r#"
query ProjectMilestones($projectSlug: String!, $first: Int!, $after: String) {
  projectMilestones(
    first: $first
    after: $after
    filter: { project: { slugId: { eq: $projectSlug } } }
  ) {
    nodes { id name }
    pageInfo { hasNextPage endCursor }
  }
}"#;

/// `queryBranchByID` — one issue's branchName + linked-PR attachments by tracker ID, for the
/// graphite stacking-context hint. `$ids` is typed `[ID!]` like `queryByIDs`. INF-318.
pub const QUERY_BRANCH_BY_ID: &str = r#"
query BranchByID($ids: [ID!], $first: Int!) {
  issues(first: $first, filter: { id: { in: $ids } }) {
    nodes {
      id
      branchName
      attachments(first: 100) { nodes { sourceType metadata } }
    }
  }
}"#;

/// `queryByStates` — minimal issues for a project in given states, paginated.
pub const QUERY_BY_STATES: &str = r#"
query ByStates($projectSlug: String!, $states: [String!], $first: Int!, $after: String) {
  issues(
    first: $first
    after: $after
    filter: { project: { slugId: { eq: $projectSlug } }, state: { name: { in: $states } } }
  ) {
    nodes { id identifier title state { name } }
    pageInfo { hasNextPage endCursor }
  }
}"#;

/// `queryByIDs` — minimal issues by tracker ID. `$ids` is typed `[ID!]` per upstream §11.2.
pub const QUERY_BY_IDS: &str = r#"
query ByIDs($ids: [ID!], $first: Int!) {
  issues(first: $first, filter: { id: { in: $ids } }) {
    nodes { id identifier title state { name } }
  }
}"#;

/// `queryTeamWorkflowStates` — all workflow states (id + name + type + position) for a team; the
/// caller matches the target NAME case-insensitively client-side. `$teamID` is typed `ID!`.
pub const QUERY_TEAM_WORKFLOW_STATES: &str = r#"
query TeamWorkflowStates($teamID: ID!) {
  workflowStates(filter: { team: { id: { eq: $teamID } } }) {
    nodes { id name type position }
  }
}"#;

/// `mutationIssueUpdateState` — moves an issue to a workflow state by its UUID.
pub const MUTATION_ISSUE_UPDATE_STATE: &str = r#"
mutation MoveIssueState($id: String!, $stateId: String!) {
  issueUpdate(id: $id, input: { stateId: $stateId }) { success }
}"#;

/// `mutationIssueAssign` — sets an issue's assignee (the durable lock in pool-mode claiming). Last
/// write wins; there is no conditional form. INF-477.
pub const MUTATION_ISSUE_ASSIGN: &str = r#"
mutation AssignIssue($id: String!, $assigneeId: String!) {
  issueUpdate(id: $id, input: { assigneeId: $assigneeId }) { success }
}"#;

/// `mutationCommentCreate` — posts a comment and returns the server-assigned id + createdAt. Used
/// to cast a pool-mode claim. INF-477.
pub const MUTATION_COMMENT_CREATE: &str = r#"
mutation CreateComment($issueId: String!, $body: String!) {
  commentCreate(input: { issueId: $issueId, body: $body }) {
    success
    comment { id createdAt }
  }
}"#;

/// `mutationCommentDelete` — removes a comment by id (claim-comment cleanup). INF-477.
pub const MUTATION_COMMENT_DELETE: &str = r#"
mutation DeleteComment($id: String!) {
  commentDelete(id: $id) { success }
}"#;

/// `queryIssueComments` — an issue's comments (id, body, createdAt) for the pool-mode claim
/// election, paginated so the election sees every claim marker. INF-477.
pub const QUERY_ISSUE_COMMENTS: &str = r#"
query IssueComments($id: String!, $first: Int!, $after: String) {
  issue(id: $id) {
    comments(first: $first, after: $after) {
      nodes { id body createdAt }
      pageInfo { hasNextPage endCursor }
    }
  }
}"#;

/// `queryIssueAssignee` — an issue's current assignee id ("" when unassigned) — the pool-mode
/// read-back gate after an assign. INF-477.
pub const QUERY_ISSUE_ASSIGNEE: &str = r#"
query IssueAssignee($id: String!) {
  issue(id: $id) { assignee { id } }
}"#;

#[cfg(test)]
mod tests {
    use super::*;

    // ─── candidate builder: full byte-identity for every variant ────────────────────────────────
    //
    // The four variants below are byte-for-byte transcriptions of `queryCandidates`'
    // `fmt.Sprintf` output in query.go — a flat golden independent of this module's chunk split,
    // so a wrong chunk boundary (dropped/duplicated byte) is caught. Assembly logic is the bug
    // surface, so every variant is locked.

    const GOLDEN_CANDIDATES_ASSIGNEE: &str = r#"
query Candidates($projectSlug: String!, $states: [String!], $first: Int!, $after: String, $assigneeID: ID!) {
  issues(
    first: $first
    after: $after
    filter: { project: { slugId: { eq: $projectSlug } }, state: { name: { in: $states } }, assignee: { id: { eq: $assigneeID } } }
  ) {
    nodes {
      id
      identifier
      title
      description
      priority
      url
      branchName
      createdAt
      updatedAt
      state { name }
      team { id }
      assignee { id displayName }
      projectMilestone { id name }
      labels { nodes { name } }
      inverseRelations { nodes { type issue { id identifier state { name } } } }
      attachments(first: 100) { nodes { sourceType metadata } }
      comments(first: 50) { nodes { createdAt body } }
    }
    pageInfo { hasNextPage endCursor }
  }
}"#;

    const GOLDEN_CANDIDATES_ASSIGNEE_MILESTONE: &str = r#"
query Candidates($projectSlug: String!, $states: [String!], $first: Int!, $after: String, $assigneeID: ID!, $milestoneID: ID!) {
  issues(
    first: $first
    after: $after
    filter: { project: { slugId: { eq: $projectSlug } }, state: { name: { in: $states } }, assignee: { id: { eq: $assigneeID } }, projectMilestone: { id: { eq: $milestoneID } } }
  ) {
    nodes {
      id
      identifier
      title
      description
      priority
      url
      branchName
      createdAt
      updatedAt
      state { name }
      team { id }
      assignee { id displayName }
      projectMilestone { id name }
      labels { nodes { name } }
      inverseRelations { nodes { type issue { id identifier state { name } } } }
      attachments(first: 100) { nodes { sourceType metadata } }
      comments(first: 50) { nodes { createdAt body } }
    }
    pageInfo { hasNextPage endCursor }
  }
}"#;

    const GOLDEN_CANDIDATES_POOL: &str = r#"
query Candidates($projectSlug: String!, $states: [String!], $first: Int!, $after: String) {
  issues(
    first: $first
    after: $after
    filter: { project: { slugId: { eq: $projectSlug } }, state: { name: { in: $states } }, assignee: { null: true } }
  ) {
    nodes {
      id
      identifier
      title
      description
      priority
      url
      branchName
      createdAt
      updatedAt
      state { name }
      team { id }
      assignee { id displayName }
      projectMilestone { id name }
      labels { nodes { name } }
      inverseRelations { nodes { type issue { id identifier state { name } } } }
      attachments(first: 100) { nodes { sourceType metadata } }
      comments(first: 50) { nodes { createdAt body } }
    }
    pageInfo { hasNextPage endCursor }
  }
}"#;

    const GOLDEN_CANDIDATES_POOL_MILESTONE: &str = r#"
query Candidates($projectSlug: String!, $states: [String!], $first: Int!, $after: String, $milestoneID: ID!) {
  issues(
    first: $first
    after: $after
    filter: { project: { slugId: { eq: $projectSlug } }, state: { name: { in: $states } }, assignee: { null: true }, projectMilestone: { id: { eq: $milestoneID } } }
  ) {
    nodes {
      id
      identifier
      title
      description
      priority
      url
      branchName
      createdAt
      updatedAt
      state { name }
      team { id }
      assignee { id displayName }
      projectMilestone { id name }
      labels { nodes { name } }
      inverseRelations { nodes { type issue { id identifier state { name } } } }
      attachments(first: 100) { nodes { sourceType metadata } }
      comments(first: 50) { nodes { createdAt body } }
    }
    pageInfo { hasNextPage endCursor }
  }
}"#;

    const GOLDEN_BACKLOG: &str = r#"
query BacklogCandidates($projectSlug: String!, $first: Int!, $after: String, $assigneeID: ID!) {
  issues(
    first: $first
    after: $after
    filter: { project: { slugId: { eq: $projectSlug } }, state: { type: { eq: "backlog" } }, assignee: { id: { eq: $assigneeID } } }
  ) {
    nodes {
      id
      identifier
      title
      description
      priority
      url
      branchName
      createdAt
      updatedAt
      state { name }
      team { id }
      assignee { id displayName }
      projectMilestone { id name }
      labels { nodes { name } }
      inverseRelations { nodes { type issue { id identifier state { name } } } }
      attachments(first: 100) { nodes { sourceType metadata } }
      comments(first: 50) { nodes { createdAt body } }
    }
    pageInfo { hasNextPage endCursor }
  }
}"#;

    const GOLDEN_BACKLOG_MILESTONE: &str = r#"
query BacklogCandidates($projectSlug: String!, $first: Int!, $after: String, $assigneeID: ID!, $milestoneID: ID!) {
  issues(
    first: $first
    after: $after
    filter: { project: { slugId: { eq: $projectSlug } }, state: { type: { eq: "backlog" } }, assignee: { id: { eq: $assigneeID } }, projectMilestone: { id: { eq: $milestoneID } } }
  ) {
    nodes {
      id
      identifier
      title
      description
      priority
      url
      branchName
      createdAt
      updatedAt
      state { name }
      team { id }
      assignee { id displayName }
      projectMilestone { id name }
      labels { nodes { name } }
      inverseRelations { nodes { type issue { id identifier state { name } } } }
      attachments(first: 100) { nodes { sourceType metadata } }
      comments(first: 50) { nodes { createdAt body } }
    }
    pageInfo { hasNextPage endCursor }
  }
}"#;

    #[test]
    fn candidates_query_byte_identical_all_variants() {
        assert_eq!(query_candidates(false, false), GOLDEN_CANDIDATES_ASSIGNEE);
        assert_eq!(
            query_candidates(true, false),
            GOLDEN_CANDIDATES_ASSIGNEE_MILESTONE
        );
        assert_eq!(query_candidates(false, true), GOLDEN_CANDIDATES_POOL);
        assert_eq!(
            query_candidates(true, true),
            GOLDEN_CANDIDATES_POOL_MILESTONE
        );
    }

    #[test]
    fn backlog_query_byte_identical_all_variants() {
        assert_eq!(query_backlog_candidates(false), GOLDEN_BACKLOG);
        assert_eq!(query_backlog_candidates(true), GOLDEN_BACKLOG_MILESTONE);
    }

    // ─── static operations: byte-identity goldens (lock against future drift) ────────────────────

    #[test]
    fn static_queries_byte_identical() {
        assert_eq!(
            QUERY_VIEWER,
            "\nquery Viewer {\n  viewer { id name displayName email organization { urlKey } }\n}"
        );
        assert_eq!(
            QUERY_BY_IDS,
            "\nquery ByIDs($ids: [ID!], $first: Int!) {\n  issues(first: $first, filter: { id: { in: $ids } }) {\n    nodes { id identifier title state { name } }\n  }\n}"
        );
        assert_eq!(
            QUERY_ISSUE_ASSIGNEE,
            "\nquery IssueAssignee($id: String!) {\n  issue(id: $id) { assignee { id } }\n}"
        );
        assert_eq!(
            MUTATION_ISSUE_UPDATE_STATE,
            "\nmutation MoveIssueState($id: String!, $stateId: String!) {\n  issueUpdate(id: $id, input: { stateId: $stateId }) { success }\n}"
        );
        assert_eq!(
            MUTATION_COMMENT_DELETE,
            "\nmutation DeleteComment($id: String!) {\n  commentDelete(id: $id) { success }\n}"
        );
    }

    // ─── mirror of query_milestone_test.go ───────────────────────────────────────────────────────

    // TestCandidateQueryMilestoneVarIsID: the milestone filter's variable MUST be `ID!`; a
    // `String!` variable at an id position is a GraphQL validation error (HTTP 400).
    #[test]
    fn candidate_query_milestone_var_is_id() {
        let q = query_candidates(true, false);
        assert!(
            q.contains("$milestoneID: ID!"),
            "milestone candidate query must declare $milestoneID as ID!; got:\n{q}"
        );
        assert!(
            !q.contains("$milestoneID: String!"),
            "milestone candidate query must NOT declare $milestoneID as String!"
        );
        assert!(
            q.contains("projectMilestone: { id: { eq: $milestoneID } }"),
            "milestone candidate query must keep the projectMilestone filter clause; got:\n{q}"
        );
    }

    // TestCandidateQueryNoMilestoneOmitsVar: the non-milestone variant declares neither the
    // variable nor the filter clause.
    #[test]
    fn candidate_query_no_milestone_omits_var() {
        let q = query_candidates(false, false);
        assert!(
            !q.contains("$milestoneID"),
            "non-milestone candidate query must NOT declare $milestoneID; got:\n{q}"
        );
        assert!(
            !q.contains("projectMilestone: { id:"),
            "non-milestone candidate query must NOT add the projectMilestone filter clause; got:\n{q}"
        );
    }
}
