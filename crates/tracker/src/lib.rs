//! rhapsody-tracker — parity port of Go `internal/tracker` (Symphony v0.4.0).
//!
//! Defines the [`Tracker`] contract the orchestrator schedules against (the port of Go's
//! `tracker.Tracker` interface, upstream §11.1), plus the construction [`Spec`] + [`new`]
//! factory and the in-memory [`fake::Fake`] test double. The `linear` (GraphQL) and `file`
//! adapters land in the later P3 tasks; T1 ships their skeletons so the factory can select them.
//!
//! Go's `ctx context.Context` cancellation becomes async cancellation (the trait is async, driven
//! by the tokio orchestrator); Go's bare `error` returns become a typed [`TrackerError`]; Go
//! pointer/`nil`-slice fields on the returned `core` types stay `Option<…>` (see `rhapsody-core`).

mod factory;
pub mod fake;
pub mod file;
pub mod linear;

pub use factory::{Spec, new};

use async_trait::async_trait;
use rhapsody_core::{Comment, Issue, Project, Viewer};
use std::any::Any;

/// The error type for tracker operations. Go's tracker methods return bare `error` values
/// (adapters wrap transport/GraphQL failures with `fmt.Errorf` and expose sentinels like
/// `ErrLinearStateNotFound`); following `rhapsody-store`'s dependency-free style, Rhapsody makes
/// the failure an explicit value type instead of pulling in a derive crate.
///
/// T1 needs only the opaque [`TrackerError::Other`] carrier (used by the fake's error injection
/// and the adapter skeletons); the linear adapter's structured sentinels are added alongside it
/// by the later P3 tasks that introduce them — T2 adds the first, [`TrackerError::StateNotFound`],
/// which the file tracker's `move_issue_to_type` returns; T3–T5 add the remaining linear sentinels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackerError {
    /// A tracker operation failed with the given message. This is the parity mirror of Go's
    /// opaque `errors.New`/`fmt.Errorf` values: the [`fake::Fake`] surfaces a test-injected
    /// failure through it, and the T1 adapter skeletons report "not yet implemented" through it
    /// until their P3 task fills the body in.
    Other(String),

    /// No workflow state exists for the requested (team, name/type). The parity mirror of Go's
    /// `linear.ErrLinearStateNotFound` sentinel (`internal/tracker/linear/errors.go`): its Display
    /// is `linear_state_not_found`, wrapped with the caller's context after `: `. Both adapters
    /// return it from the by-type move — the file tracker (T2) when a `state_types` mapping is
    /// missing, the linear adapter (T5) when the team has no state of that type. Carries the
    /// context string; the sentinel token is composed by [`Display`](std::fmt::Display).
    StateNotFound(String),
}

impl std::fmt::Display for TrackerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TrackerError::Other(msg) => write!(f, "{msg}"),
            TrackerError::StateNotFound(msg) => write!(f, "linear_state_not_found: {msg}"),
        }
    }
}

impl std::error::Error for TrackerError {}

/// Tracker is the issue-tracker contract used by the orchestrator (upstream §11.1).
/// Implementations must return normalized [`Issue`](rhapsody_core::Issue) values.
///
/// Ported from Go's `tracker.Tracker` interface. Method names are snake_cased; Go's
/// `ctx context.Context` first argument maps to async cancellation (implicit); Go's multi-value
/// returns become tuples/[`Result`]. The trait is `Send + Sync` so the tokio orchestrator (P5)
/// can hold a `Box<dyn Tracker>` across `.await` points and share it between tasks; the [`Any`]
/// supertrait lets [`new`]'s adapter-selection test downcast the boxed tracker to its concrete
/// adapter, the parity mirror of Go `factory_test.go`'s `.(*file.Tracker)` type assertions (it
/// adds no method to the contract surface).
#[async_trait]
pub trait Tracker: Any + Send + Sync {
    /// FetchCandidateIssues returns issues in the configured active states for the configured
    /// project.
    async fn fetch_candidate_issues(&self) -> Result<Vec<Issue>, TrackerError>;

    /// FetchIssuesByStates returns issues in the given states for the configured project. An
    /// empty states slice returns an empty result with no API call.
    async fn fetch_issues_by_states(&self, states: &[String]) -> Result<Vec<Issue>, TrackerError>;

    /// FetchIssueStatesByIDs returns minimal normalized issues (id, identifier, title, state) for
    /// the given tracker IDs, used for reconciliation.
    async fn fetch_issue_states_by_ids(&self, ids: &[String]) -> Result<Vec<Issue>, TrackerError>;

    /// FetchBlockedBacklogIssues returns FULLY-normalized Backlog-state issues (BlockedBy
    /// populated) for the configured project, assigned to the API key owner. It backs the DAG
    /// auto-promote pass (orchestrator.promoteUnblocked), which needs Backlog tickets WITH their
    /// blocker edges — neither FetchCandidateIssues (active ∪ review only) nor FetchIssuesByStates
    /// (no blocker edges) supplies that. Read-only; Backlog is selected by Linear state TYPE
    /// ("backlog"), which is config-free (state names vary per workspace, types are stable).
    /// INF-318.
    async fn fetch_blocked_backlog_issues(&self) -> Result<Vec<Issue>, TrackerError>;

    /// FetchIssueBranchByID returns an issue's Linear gitBranchName and, best-effort, its linked
    /// GitHub PR number for graphite-mode stacking-context injection (the predecessor a dependent
    /// stacks on). A missing issue returns `("", 0)` — the hint is advisory, never fatal. INF-318.
    async fn fetch_issue_branch_by_id(&self, id: &str) -> Result<(String, i64), TrackerError>;

    /// MoveIssueState moves an issue to the named workflow state. `team_id` scopes the state-name
    /// -> state-UUID resolution (state names are unique only within a team). It is the only WRITE
    /// in the contract, used to promote a summoned review-state ticket to an active state before
    /// dispatch (symphony-29).
    async fn move_issue_state(
        &self,
        issue_id: &str,
        team_id: &str,
        state_name: &str,
    ) -> Result<(), TrackerError>;

    /// MoveIssueToType moves an issue to the team's workflow state of the given Linear state TYPE
    /// ("backlog" or "unstarted"), returning the resolved state's display name (for UI toasts).
    /// State names vary per workspace but the type is stable, so this is config-free. Errors with
    /// the linear adapter's `ErrLinearStateNotFound` sentinel if the team has no state of that
    /// type.
    async fn move_issue_to_type(
        &self,
        issue_id: &str,
        team_id: &str,
        state_type: &str,
    ) -> Result<String, TrackerError>;

    /// ResolveViewer returns the owner of the configured API key — the user whose assigned issues
    /// this tracker works — cached for the client's lifetime. It backs the "connected as" identity
    /// surface in the Settings API (INF-224).
    async fn resolve_viewer(&self) -> Result<Viewer, TrackerError>;

    /// ListProjects lists the tracker workspace's projects (id, name, slug, team, color) for the
    /// Settings add-agent picker (INF-224). It is account-scoped (not project-filtered).
    async fn list_projects(&self) -> Result<Vec<Project>, TrackerError>;

    /// AssignIssue sets an issue's assignee (the durable lock in pool-mode claiming). It is a
    /// last-write-wins issueUpdate(assigneeId); there is no conditional/CAS form, so the caller
    /// re-reads the assignee (`fetch_issue_assignee`) to verify it holds the claim uncontested.
    /// INF-477.
    async fn assign_issue(&self, issue_id: &str, assignee_id: &str) -> Result<(), TrackerError>;

    /// FetchIssueAssignee returns an issue's current assignee user ID ("" when unassigned). It is
    /// the pool-mode read-back gate: after `assign_issue`, the election winner re-reads the
    /// assignee and aborts the run if it is not itself. A dedicated single-purpose read (NOT the
    /// state-only by-ids query, whose staleness contract other callers depend on). INF-477.
    async fn fetch_issue_assignee(&self, issue_id: &str) -> Result<String, TrackerError>;

    /// CreateComment posts a comment on an issue and returns the server-assigned comment ID. Used
    /// to cast a pool-mode claim (a machine-parseable marker + daemon identity). INF-477.
    async fn create_comment(&self, issue_id: &str, body: &str) -> Result<String, TrackerError>;

    /// ListComments returns an issue's comments (id, body, createdAt) for the pool-mode claim
    /// election, which orders by the immutable server createdAt and tie-breaks on id. INF-477.
    async fn list_comments(&self, issue_id: &str) -> Result<Vec<Comment>, TrackerError>;

    /// DeleteComment removes a comment by ID, used to clean up a daemon's own claim comment
    /// (whether it won or lost the election) so stale claims don't accumulate. INF-477.
    async fn delete_comment(&self, comment_id: &str) -> Result<(), TrackerError>;
}
