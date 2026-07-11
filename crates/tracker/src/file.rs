//! File-backed tracker — port of Go `internal/tracker/file`.
//!
//! T1 ships only the skeleton (construction [`Config`] + the [`Tracker`] shell) so the factory can
//! select `kind: "file"`. The read/write bodies (the hermetic e2e path) are ported by P3 Task T2;
//! until then every method reports a not-yet-implemented [`TrackerError`].

use crate::TrackerError;
use async_trait::async_trait;
use rhapsody_core::{Comment, Issue, Project, Viewer};

/// Construction inputs for the file tracker — the file-adapter subset of the factory
/// [`Spec`](crate::Spec) (mirrors Go's `file.Config`). The file adapter ignores the linear-only
/// `endpoint`/`api_key`/`claim_mode` fields.
#[derive(Debug, Clone, Default)]
pub struct Config {
    /// Path to the JSON issue file.
    pub source: String,
    pub project_slug: String,
    pub active_states: Vec<String>,
    pub review_states: Vec<String>,
    pub summon_token: String,
    pub milestone: String,
}

/// The file-backed tracker (skeleton). T2 fills in the read/write bodies over [`Config`].
pub struct Tracker {
    config: Config,
}

/// Builds a file [`Tracker`] from its [`Config`] (mirrors Go's `file.New`).
pub fn new(config: Config) -> Tracker {
    Tracker { config }
}

impl Tracker {
    fn not_implemented(&self) -> TrackerError {
        TrackerError::Other(format!(
            "file tracker (source {:?}) not yet implemented — ported by P3 Task T2",
            self.config.source
        ))
    }
}

#[async_trait]
impl crate::Tracker for Tracker {
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
