//! Linear GraphQL tracker adapter — port of Go `internal/tracker/linear`.
//!
//! T1 ships only the skeleton (construction [`Config`] + the [`Client`] shell) so the factory can
//! select `kind: "linear"` (the historical default). The GraphQL transport, query builder,
//! normalize rules, and read/write paths are ported by P3 Tasks T3–T5 into sibling modules under
//! `linear/`; until then every method reports a not-yet-implemented [`TrackerError`].

use crate::TrackerError;
use async_trait::async_trait;
use rhapsody_core::{Comment, Issue, Project, Viewer};

/// Construction inputs for the linear adapter — the linear subset of the factory
/// [`Spec`](crate::Spec) (mirrors Go's `linear.Config`).
#[derive(Debug, Clone, Default)]
pub struct Config {
    pub endpoint: String,
    pub api_key: String,
    pub project_slug: String,
    pub active_states: Vec<String>,
    pub review_states: Vec<String>,
    pub summon_token: String,
    pub milestone: String,
    /// The resolved ticket-claim policy ("assignee" | "pool"). In "pool" the candidate query
    /// filters UNASSIGNED issues instead of assignee == viewer; every other query is unchanged.
    /// Empty is treated as "assignee". INF-477.
    pub claim_mode: String,
}

/// The Linear GraphQL client (skeleton). T3–T5 fill in the transport + read/write bodies over
/// [`Config`].
pub struct Client {
    config: Config,
}

/// Builds a linear [`Client`] from its [`Config`] (mirrors Go's `linear.New`).
pub fn new(config: Config) -> Client {
    Client { config }
}

impl Client {
    fn not_implemented(&self) -> TrackerError {
        TrackerError::Other(format!(
            "linear adapter (endpoint {:?}) not yet implemented — ported by P3 Tasks T3–T5",
            self.config.endpoint
        ))
    }
}

#[async_trait]
impl crate::Tracker for Client {
    async fn fetch_candidate_issues(&self) -> Result<Vec<Issue>, TrackerError> {
        Err(self.not_implemented())
    }
    async fn fetch_issues_by_states(&self, _states: &[String]) -> Result<Vec<Issue>, TrackerError> {
        Err(self.not_implemented())
    }
    async fn fetch_issue_states_by_ids(&self, _ids: &[String]) -> Result<Vec<Issue>, TrackerError> {
        Err(self.not_implemented())
    }
    async fn fetch_blocked_backlog_issues(&self) -> Result<Vec<Issue>, TrackerError> {
        Err(self.not_implemented())
    }
    async fn fetch_issue_branch_by_id(&self, _id: &str) -> Result<(String, i64), TrackerError> {
        Err(self.not_implemented())
    }
    async fn move_issue_state(
        &self,
        _issue_id: &str,
        _team_id: &str,
        _state_name: &str,
    ) -> Result<(), TrackerError> {
        Err(self.not_implemented())
    }
    async fn move_issue_to_type(
        &self,
        _issue_id: &str,
        _team_id: &str,
        _state_type: &str,
    ) -> Result<String, TrackerError> {
        Err(self.not_implemented())
    }
    async fn resolve_viewer(&self) -> Result<Viewer, TrackerError> {
        Err(self.not_implemented())
    }
    async fn list_projects(&self) -> Result<Vec<Project>, TrackerError> {
        Err(self.not_implemented())
    }
    async fn assign_issue(&self, _issue_id: &str, _assignee_id: &str) -> Result<(), TrackerError> {
        Err(self.not_implemented())
    }
    async fn fetch_issue_assignee(&self, _issue_id: &str) -> Result<String, TrackerError> {
        Err(self.not_implemented())
    }
    async fn create_comment(&self, _issue_id: &str, _body: &str) -> Result<String, TrackerError> {
        Err(self.not_implemented())
    }
    async fn list_comments(&self, _issue_id: &str) -> Result<Vec<Comment>, TrackerError> {
        Err(self.not_implemented())
    }
    async fn delete_comment(&self, _comment_id: &str) -> Result<(), TrackerError> {
        Err(self.not_implemented())
    }
}
