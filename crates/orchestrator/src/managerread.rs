//! managerread — the host's half of the manager run's reads (STUDIO-1014; design record
//! `~/.rhapsody/docs/manager-agent-design.md` §4.4, §5.5).
//!
//! **No Go v0.4.0 counterpart.** The manager run has no checkout, no `gh` and no `git`; the daemon
//! serves every repository read. This module is the daemon-side plumbing behind the
//! `/api/v1/manager/*` endpoints the manager MCP tools (the `mcp` crate's `manager_*`, role
//! `manager`) proxy. It is deliberately off-loop and holds no `Orchestrator`, for [`crate::rundiff`]'s
//! reason: everything here shells out (`git` through the workspace manager, `gh` through the read
//! seams) and no read takes a claim, so nothing can stall dispatch.
//!
//! # The coordinate comes from the RUN, never the caller
//!
//! A manager run's key is `pr:<owner>/<repo>#<n>@manager`. Every endpoint takes only `run_id`; the
//! coordinate is parsed from that run row's `issue_identifier` and the mirror URL from its `repo`.
//! A run whose key does not end `@manager` is refused, so a review run's id can never be used to
//! read through the manager's surface.
//!
//! # The evidence-access log (§5.5)
//!
//! Every `manager_diff`/`manager_interdiff` the host serves is recorded, per run id, in
//! `rhapsody_evidence_access`. That log is what §6.4 condition 3 reads to prove the run was GIVEN
//! the covering diffs — it cannot prove the model read them, and says so.

use std::sync::Arc;

use rhapsody_store::{EVIDENCE_ACCESS_DIFF, EVIDENCE_ACCESS_INTERDIFF, EvidenceAccess, RunSummary};
use rhapsody_workspace::{Manager, ReadError};

use crate::teamsknow::parse_pr_ref;

/// The manager role token in a manager run's key (`pr:<owner>/<repo>#<n>@manager`).
pub const MANAGER_ROLE_TOKEN: &str = "manager";

/// A resolved manager-run coordinate: where to read, and from which mirror.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagerCoordinate {
    pub owner: String,
    pub repo: String,
    pub number: i64,
    /// The clone URL whose bare mirror holds the objects (`runs.repo`).
    pub repo_url: String,
}

impl ManagerCoordinate {
    /// `owner/repo#number`, the findings table's key.
    pub fn pr_slug(&self) -> String {
        format!("{}/{}#{}", self.owner, self.repo, self.number)
    }
}

/// A refused manager read. Typed so the HTTP layer can map each to a stable code without guessing.
#[derive(Debug)]
pub enum ManagerReadError {
    /// No run has that id (404).
    NoSuchRun,
    /// The run exists but is not a manager run (`@manager` key) — a caller must never read another
    /// kind of run through this surface.
    NotAManagerRun,
    /// The read cannot be served on this daemon/for this run, with a true reason.
    Unavailable(&'static str),
    /// The host's git read failed (`not_found`, `too_large`, `invalid_revision`, `git_failed`).
    Read(ReadError),
    /// A store read failed.
    Store(String),
}

impl ManagerReadError {
    /// The stable error code the endpoint renders.
    pub fn code(&self) -> &'static str {
        match self {
            ManagerReadError::NoSuchRun => "not_found",
            ManagerReadError::NotAManagerRun => "not_a_manager_run",
            ManagerReadError::Unavailable(_) => "unavailable",
            ManagerReadError::Read(e) => read_error_code(e),
            ManagerReadError::Store(_) => "store_error",
        }
    }

    /// The human-facing message.
    pub fn message(&self) -> String {
        match self {
            ManagerReadError::NoSuchRun => "no such run".to_string(),
            ManagerReadError::NotAManagerRun => "this run is not a manager run".to_string(),
            ManagerReadError::Unavailable(why) => (*why).to_string(),
            ManagerReadError::Read(e) => e.to_string(),
            ManagerReadError::Store(e) => e.clone(),
        }
    }
}

/// The stable code for a host-served git read failure.
pub fn read_error_code(e: &ReadError) -> &'static str {
    match e {
        ReadError::InvalidRevision => "invalid_revision",
        ReadError::NotFound => "not_found",
        ReadError::IsDirectory => "is_a_directory",
        ReadError::TooLarge => "too_large",
        ReadError::Git(_) => "git_failed",
    }
}

/// The result of one manager read: the JSON body a tool returns, or a typed refusal. Defined here so
/// the HTTP layer and the daemon share one shape.
pub type ManagerReadOutcome = Result<serde_json::Value, ManagerReadError>;

/// Resolves a manager run's coordinate from its own run row. Pure, so the refusal rules are testable
/// without a daemon: the key must parse as a `pr:` coordinate carrying the `manager` role, and the
/// row must name a repository for the mirror.
pub fn manager_coordinate(run: &RunSummary) -> Result<ManagerCoordinate, ManagerReadError> {
    let Some(pr) = parse_pr_ref(&run.issue_identifier) else {
        return Err(ManagerReadError::NotAManagerRun);
    };
    if pr.reviewer != MANAGER_ROLE_TOKEN {
        return Err(ManagerReadError::NotAManagerRun);
    }
    if run.repo.is_empty() {
        return Err(ManagerReadError::Unavailable(
            "this manager run has no repository to read",
        ));
    }
    Ok(ManagerCoordinate {
        owner: pr.owner,
        repo: pr.repo,
        number: pr.number,
        repo_url: run.repo.clone(),
    })
}

impl crate::ControlHandle {
    /// Resolves a run's manager coordinate from its own row. Every manager route starts here, so a
    /// review run's id can never be read through the manager's surface.
    async fn manager_coordinate_for(
        &self,
        run_id: i64,
    ) -> Result<ManagerCoordinate, ManagerReadError> {
        let run = match self.store().get_run(run_id) {
            Ok(Some(run)) => run,
            Ok(None) => return Err(ManagerReadError::NoSuchRun),
            Err(e) => return Err(ManagerReadError::Store(e.to_string())),
        };
        manager_coordinate(&run)
    }

    /// The live workspace manager, obtained through the SAME control round-trip the prune scheduler
    /// uses ([`crate::ControlHandle::workspace_gc_plan`]), so a read always uses the live root
    /// rather than a boot-time snapshot. Only the REPOSITORY reads need this; the store-backed ones
    /// do not, so they never fail merely because the workspace is not built yet.
    async fn manager_workspace(&self) -> Result<Arc<Manager>, ManagerReadError> {
        self.workspace_gc_plan()
            .await
            .and_then(|plan| plan.mgr)
            .ok_or(ManagerReadError::Unavailable(
                "the workspace is not available yet",
            ))
    }

    /// `manager_file {sha, path}`: one blob at a commit sha. A symlink is returned as its blob
    /// text and is never followed.
    pub async fn manager_file(
        &self,
        run_id: i64,
        sha: &str,
        path: &str,
    ) -> Result<serde_json::Value, ManagerReadError> {
        let coord = self.manager_coordinate_for(run_id).await?;
        let mgr = self.manager_workspace().await?;
        let blob = mgr
            .read_blob(&coord.repo_url, sha, path)
            .await
            .map_err(ManagerReadError::Read)?;
        Ok(serde_json::json!({
            "sha": sha,
            "path": path,
            "content": blob.content,
            "symlink": blob.symlink,
        }))
    }

    /// `manager_ls {sha, path}`: a tree listing at a commit sha.
    pub async fn manager_ls(
        &self,
        run_id: i64,
        sha: &str,
        path: &str,
    ) -> Result<serde_json::Value, ManagerReadError> {
        let coord = self.manager_coordinate_for(run_id).await?;
        let mgr = self.manager_workspace().await?;
        let tree = mgr
            .ls_tree(&coord.repo_url, sha, path)
            .await
            .map_err(ManagerReadError::Read)?;
        let entries: Vec<serde_json::Value> = tree
            .entries
            .iter()
            .map(|e| {
                serde_json::json!({
                    "mode": e.mode,
                    "kind": e.kind,
                    "sha": e.sha,
                    "path": e.path,
                })
            })
            .collect();
        Ok(serde_json::json!({
            "sha": sha,
            "path": path,
            "entries": entries,
            "truncated": tree.truncated,
        }))
    }

    /// `manager_grep {sha, pattern, path}`: a `git grep` at a commit sha.
    pub async fn manager_grep(
        &self,
        run_id: i64,
        sha: &str,
        pattern: &str,
        path: &str,
    ) -> Result<serde_json::Value, ManagerReadError> {
        let coord = self.manager_coordinate_for(run_id).await?;
        let mgr = self.manager_workspace().await?;
        let got = mgr
            .grep(&coord.repo_url, sha, pattern, path)
            .await
            .map_err(ManagerReadError::Read)?;
        Ok(serde_json::json!({
            "sha": sha,
            "pattern": pattern,
            "path": path,
            "text": got.text,
            "truncated": got.truncated,
        }))
    }

    /// `manager_diff {from, to}`: the diff between two revisions, recorded in the evidence log.
    pub async fn manager_diff(
        &self,
        run_id: i64,
        from: &str,
        to: &str,
    ) -> Result<serde_json::Value, ManagerReadError> {
        let coord = self.manager_coordinate_for(run_id).await?;
        let mgr = self.manager_workspace().await?;
        let patch = mgr
            .diff(&coord.repo_url, from, to)
            .await
            .map_err(ManagerReadError::Read)?;
        self.record_evidence(run_id, EVIDENCE_ACCESS_DIFF, from, to);
        Ok(serde_json::json!({ "from": from, "to": to, "patch": patch }))
    }

    /// `manager_interdiff {from, to}`: the difference between the two pull-request patches (the
    /// `git range-diff` comparison), recorded in the evidence log.
    pub async fn manager_interdiff(
        &self,
        run_id: i64,
        from: &str,
        to: &str,
    ) -> Result<serde_json::Value, ManagerReadError> {
        let coord = self.manager_coordinate_for(run_id).await?;
        let mgr = self.manager_workspace().await?;
        let base = mgr
            .default_base_sha(&coord.repo_url)
            .await
            .map_err(ManagerReadError::Read)?;
        let patch = mgr
            .range_diff(&coord.repo_url, &base, from, to)
            .await
            .map_err(ManagerReadError::Read)?;
        self.record_evidence(run_id, EVIDENCE_ACCESS_INTERDIFF, from, to);
        Ok(serde_json::json!({
            "from": from,
            "to": to,
            "base": base,
            "patch": patch,
        }))
    }

    /// `manager_patch_id {sha}`: a stable patch-id over `merge-base(base, sha)..sha`, where `base`
    /// is the mirror's default branch — the design's content identity (§5.5).
    pub async fn manager_patch_id(
        &self,
        run_id: i64,
        sha: &str,
    ) -> Result<serde_json::Value, ManagerReadError> {
        let coord = self.manager_coordinate_for(run_id).await?;
        let mgr = self.manager_workspace().await?;
        let base = mgr
            .default_base_sha(&coord.repo_url)
            .await
            .map_err(ManagerReadError::Read)?;
        let mb = mgr
            .merge_base(&coord.repo_url, &base, sha)
            .await
            .map_err(ManagerReadError::Read)?;
        if mb.is_empty() {
            return Err(ManagerReadError::Read(ReadError::NotFound));
        }
        let id = mgr
            .patch_id(&coord.repo_url, &mb, sha)
            .await
            .map_err(ManagerReadError::Read)?;
        Ok(serde_json::json!({ "sha": sha, "base": base, "patch_id": id }))
    }

    /// `manager_findings`: the structured findings recorded for the run's pull request (§5.3). A
    /// pure store read — it needs the coordinate, not the workspace.
    pub async fn manager_findings(
        &self,
        run_id: i64,
    ) -> Result<serde_json::Value, ManagerReadError> {
        let coord = self.manager_coordinate_for(run_id).await?;
        let pr = coord.pr_slug();
        let rows = self
            .store()
            .load_review_findings(&pr)
            .map_err(|e| ManagerReadError::Store(e.to_string()))?;
        let findings: Vec<serde_json::Value> = rows
            .iter()
            .map(|f| {
                serde_json::json!({
                    "pr": f.pr,
                    "generation": f.generation,
                    "reviewer": f.reviewer,
                    "finding_id": f.finding_id,
                    "revision": f.revision,
                    "review_run_id": f.review_run_id,
                    "raised_at_sha": f.raised_at_sha,
                    "raised_at_patch_id": f.raised_at_patch_id,
                    "paths": f.paths,
                    "summary_hash": f.summary_hash,
                    "blocking": f.blocking,
                    "new_evidence": f.new_evidence,
                    "regression": f.regression,
                    "status": f.status,
                    "resolved_by": f.resolved_by,
                    "dismissed_by": f.dismissed_by,
                })
            })
            .collect();
        Ok(serde_json::json!({ "pr": pr, "findings": findings }))
    }

    /// Appends one served diff/interdiff to the evidence-access log (§5.5). Best-effort: a store
    /// failure is logged, never surfaced as a read failure — the diff was served regardless.
    fn record_evidence(&self, run_id: i64, kind: &str, from: &str, to: &str) {
        let access = EvidenceAccess {
            run_id,
            kind: kind.to_string(),
            from_sha: from.to_string(),
            to_sha: to.to_string(),
            recorded_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        };
        if let Err(e) = self.store().record_evidence_access(access) {
            tracing::warn!(run = run_id, kind, error = %e, "manager read: could not record evidence access");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_row(key: &str, repo: &str) -> RunSummary {
        RunSummary {
            id: 7,
            issue_identifier: key.to_string(),
            repo: repo.to_string(),
            ..RunSummary::default()
        }
    }

    #[test]
    fn a_manager_key_resolves_its_coordinate() {
        let got = manager_coordinate(&run_row(
            "pr:makewhatis/rhapsody#42@manager",
            "git@github.com:makewhatis/rhapsody.git",
        ))
        .expect("a manager coordinate");
        assert_eq!(got.owner, "makewhatis");
        assert_eq!(got.repo, "rhapsody");
        assert_eq!(got.number, 42);
        assert_eq!(got.pr_slug(), "makewhatis/rhapsody#42");
        assert_eq!(got.repo_url, "git@github.com:makewhatis/rhapsody.git");
    }

    // A review run's key (`@alice`) is NOT a manager run: the manager surface must refuse it.
    #[test]
    fn a_review_run_is_not_a_manager_run() {
        assert!(matches!(
            manager_coordinate(&run_row(
                "pr:makewhatis/rhapsody#42@alice",
                "git@github.com:makewhatis/rhapsody.git"
            )),
            Err(ManagerReadError::NotAManagerRun)
        ));
        assert!(matches!(
            manager_coordinate(&run_row("STUDIO-1014", "git@github.com:x/y.git")),
            Err(ManagerReadError::NotAManagerRun)
        ));
    }

    #[test]
    fn a_manager_run_without_a_repo_is_unavailable() {
        assert!(matches!(
            manager_coordinate(&run_row("pr:makewhatis/rhapsody#42@manager", "")),
            Err(ManagerReadError::Unavailable(_))
        ));
    }

    #[test]
    fn read_errors_map_to_stable_codes() {
        assert_eq!(
            read_error_code(&ReadError::InvalidRevision),
            "invalid_revision"
        );
        assert_eq!(read_error_code(&ReadError::NotFound), "not_found");
        assert_eq!(read_error_code(&ReadError::TooLarge), "too_large");
        assert_eq!(read_error_code(&ReadError::Git("x".into())), "git_failed");
    }
}
