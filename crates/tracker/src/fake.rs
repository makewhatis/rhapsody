//! Deterministic in-memory [`Tracker`](crate::Tracker) for tests — port of Go
//! `internal/tracker/fake`.
//!
//! Programmable inputs (candidates, by-state/by-id maps, injected errors, viewer/projects) are
//! plain fields set directly by tests, exactly as Go's `fake.Tracker` exposes them. State the
//! methods *mutate* (call counters, recorded calls, the claim comment/assignee stores) lives
//! behind a single [`Mutex`] because the trait's methods take `&self` (the orchestrator shares one
//! tracker across tasks, and the pool-mode contention test drives the claim methods concurrently —
//! Go guards the same state with its `cmu sync.Mutex`). That recorded state is read back through
//! the accessor methods (`candidate_calls()`, `move_calls()`, …) rather than Go's public fields.

use crate::{Tracker, TrackerError};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rhapsody_core::{Comment, Issue, Project, Viewer, normalize_state};
use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

/// One [`Tracker::move_issue_state`] invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoveCall {
    pub issue_id: String,
    pub team_id: String,
    pub state_name: String,
}

/// One [`Tracker::move_issue_to_type`] invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoveToTypeCall {
    pub issue_id: String,
    pub team_id: String,
    pub state_type: String,
}

/// A programmable [`Tracker::fetch_issue_branch_by_id`] result (branch + best-effort PR number).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BranchInfo {
    pub branch: String,
    pub pr: i64,
}

/// One [`Tracker::create_comment`] invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommentCall {
    pub issue_id: String,
    pub body: String,
}

/// One [`Tracker::assign_issue`] invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssignCall {
    pub issue_id: String,
    pub assignee_id: String,
}

/// A programmable [`Tracker::fetch_issue_states_by_ids`] override (lets tests vary results per
/// call, e.g. active-then-inactive). Takes precedence over `by_id`/`by_id_err` when set.
type StatesByIdsFn = Box<dyn Fn(&[String]) -> Result<Vec<Issue>, TrackerError> + Send + Sync>;

/// A hook run INSIDE [`Tracker::move_issue_to_type`] after the call is recorded and before the
/// return value is produced.
type Hook = Box<dyn Fn() + Send + Sync>;

/// The clock that stamps created claim comments; defaults to `Utc::now`.
type Clock = Box<dyn Fn() -> DateTime<Utc> + Send + Sync>;

/// A programmable in-memory tracker. Programmable inputs are set directly by tests.
#[derive(Default)]
pub struct Fake {
    pub candidates: Vec<Issue>,
    /// Normalized-lowercase state -> issues.
    pub by_state: HashMap<String, Vec<Issue>>,
    /// Issue id -> issue.
    pub by_id: HashMap<String, Issue>,

    /// The programmable `fetch_blocked_backlog_issues` result (Backlog-state issues with
    /// `blocked_by` populated); `blocked_backlog_err`, when set, is returned instead. INF-318.
    pub blocked_backlog: Vec<Issue>,
    pub blocked_backlog_err: Option<TrackerError>,
    /// Maps an issue id to its branch + PR for `fetch_issue_branch_by_id`; a miss returns
    /// `("", 0)`. `branch_by_id_err`, when set, is returned instead. INF-318.
    pub branch_by_id: HashMap<String, BranchInfo>,
    pub branch_by_id_err: Option<TrackerError>,

    pub candidates_err: Option<TrackerError>,
    pub by_state_err: Option<TrackerError>,
    pub by_id_err: Option<TrackerError>,

    /// When set, returned by `move_issue_state` (the move is still recorded).
    pub move_err: Option<TrackerError>,

    /// The state display name returned by `move_issue_to_type`.
    pub move_to_type_name: String,
    /// When set, returned by `move_issue_to_type` (the call is still recorded).
    pub move_to_type_err: Option<TrackerError>,
    /// When set, runs INSIDE `move_issue_to_type` after the call is recorded and before the return
    /// value is produced. It lets a test simulate a request-context cancellation that lands after
    /// the (successful) Linear move but before suppression finalize (INF-223 finding A/B).
    pub move_to_type_hook: Option<Hook>,

    /// When set, overrides `fetch_issue_states_by_ids`. Takes precedence over `by_id`/`by_id_err`.
    pub states_by_ids_func: Option<StatesByIdsFn>,
    /// When set, `fetch_issue_states_by_ids` AWAITS this gate (until it carries `true`) before it
    /// produces a result — an async stand-in for a slow Linear round-trip. It suspends the CALLING
    /// FUTURE rather than blocking its thread, which is what an in-flight network call really does,
    /// so a test can park the orchestrator's control task inside `reconcile` and observe what the
    /// rest of the daemon can still answer while it is stalled (STUDIO-551).
    pub states_by_ids_gate: Option<tokio::sync::watch::Receiver<bool>>,

    /// `viewer` / `projects` back the INF-224 read surfaces (`resolve_viewer` / `list_projects`);
    /// the `*_err` fields, when set, are returned instead.
    pub viewer: Viewer,
    pub viewer_err: Option<TrackerError>,
    pub projects: Vec<Project>,
    pub projects_err: Option<TrackerError>,

    // Error injectors for the claim methods (the call is still recorded before the error returns,
    // except `create_comment` which cannot record a comment it failed to create).
    pub create_comment_err: Option<TrackerError>,
    pub list_comments_err: Option<TrackerError>,
    pub assign_err: Option<TrackerError>,
    pub assignee_err: Option<TrackerError>,
    pub delete_comment_err: Option<TrackerError>,

    /// When it has an entry for an issue, forces `fetch_issue_assignee` to return that value
    /// regardless of `assign_issue` — used to simulate a concurrent daemon that won the assign
    /// race (lose-on-read-back).
    pub assignee_read_override: HashMap<String, String>,

    /// Stamps created claim comments; defaults to `Utc::now`. Tests can pin it (e.g. to exercise
    /// claim_ttl freshness) via [`Fake::set_clock`].
    clock: Option<Clock>,

    /// Recorded state mutated by the `&self` methods (see the module docs).
    inner: Mutex<Inner>,
}

/// Recorded state guarded by [`Fake::inner`].
#[derive(Default)]
struct Inner {
    move_calls: Vec<MoveCall>,
    move_to_type_calls: Vec<MoveToTypeCall>,
    create_comment_calls: Vec<CommentCall>,
    assign_calls: Vec<AssignCall>,
    delete_comment_calls: Vec<String>,
    list_comments_calls: usize,
    assignee_calls: usize,
    candidate_calls: usize,
    by_state_calls: usize,
    by_id_calls: usize,
    blocked_backlog_calls: usize,
    branch_by_id_calls: usize,

    /// `comment_seq` generates unique, monotonically-ordered comment IDs so a tie on `created_at`
    /// resolves deterministically (smaller id = earlier caller).
    comment_seq: u32,
    /// The per-issue comment store `create_comment` appends to and `list_comments` returns. Tests
    /// pre-seed competitor claims here via [`Fake::seed_comment`].
    comments: HashMap<String, Vec<Comment>>,
    /// The per-issue assignee set by `assign_issue` (read back by `fetch_issue_assignee`).
    assignees: HashMap<String, String>,
}

impl Fake {
    /// Returns an empty fake.
    pub fn new() -> Self {
        Self::default()
    }

    /// Pins the clock used to stamp created claim comments (default `Utc::now`).
    pub fn set_clock(&mut self, clock: impl Fn() -> DateTime<Utc> + Send + Sync + 'static) {
        self.clock = Some(Box::new(clock));
    }

    /// Pre-seeds a comment on an issue (e.g. a competitor's claim in an election test).
    pub fn seed_comment(&self, issue_id: &str, c: Comment) {
        self.lock()
            .comments
            .entry(issue_id.to_string())
            .or_default()
            .push(c);
    }

    /// Number of `fetch_candidate_issues` calls.
    pub fn candidate_calls(&self) -> usize {
        self.lock().candidate_calls
    }
    /// Number of `fetch_issues_by_states` calls.
    pub fn by_state_calls(&self) -> usize {
        self.lock().by_state_calls
    }
    /// Number of `fetch_issue_states_by_ids` calls.
    pub fn by_id_calls(&self) -> usize {
        self.lock().by_id_calls
    }
    /// Number of `fetch_blocked_backlog_issues` calls.
    pub fn blocked_backlog_calls(&self) -> usize {
        self.lock().blocked_backlog_calls
    }
    /// Number of `fetch_issue_branch_by_id` calls.
    pub fn branch_by_id_calls(&self) -> usize {
        self.lock().branch_by_id_calls
    }
    /// Number of `list_comments` calls.
    pub fn list_comments_calls(&self) -> usize {
        self.lock().list_comments_calls
    }
    /// Number of `fetch_issue_assignee` calls.
    pub fn assignee_calls(&self) -> usize {
        self.lock().assignee_calls
    }
    /// Every `move_issue_state` invocation, in order.
    pub fn move_calls(&self) -> Vec<MoveCall> {
        self.lock().move_calls.clone()
    }
    /// Every `move_issue_to_type` invocation, in order.
    pub fn move_to_type_calls(&self) -> Vec<MoveToTypeCall> {
        self.lock().move_to_type_calls.clone()
    }
    /// Every `create_comment` invocation, in order.
    pub fn create_comment_calls(&self) -> Vec<CommentCall> {
        self.lock().create_comment_calls.clone()
    }
    /// Every `assign_issue` invocation, in order.
    pub fn assign_calls(&self) -> Vec<AssignCall> {
        self.lock().assign_calls.clone()
    }
    /// Every `delete_comment` id, in order.
    pub fn delete_comment_calls(&self) -> Vec<String> {
        self.lock().delete_comment_calls.clone()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // Recover a poisoned lock rather than panic: the fake is library (non-test-cfg) code, and
        // a panic in one claim goroutine must not poison-crash the others.
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[async_trait]
impl Tracker for Fake {
    async fn fetch_candidate_issues(&self) -> Result<Vec<Issue>, TrackerError> {
        self.lock().candidate_calls += 1;
        if let Some(e) = &self.candidates_err {
            return Err(e.clone());
        }
        Ok(self.candidates.clone())
    }

    async fn fetch_issues_by_states(&self, states: &[String]) -> Result<Vec<Issue>, TrackerError> {
        self.lock().by_state_calls += 1;
        if states.is_empty() {
            return Ok(Vec::new());
        }
        if let Some(e) = &self.by_state_err {
            return Err(e.clone());
        }
        let mut out = Vec::new();
        for s in states {
            if let Some(issues) = self.by_state.get(&normalize_state(s)) {
                out.extend(issues.iter().cloned());
            }
        }
        Ok(out)
    }

    async fn fetch_issue_states_by_ids(&self, ids: &[String]) -> Result<Vec<Issue>, TrackerError> {
        self.lock().by_id_calls += 1;
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        if let Some(gate) = &self.states_by_ids_gate {
            let mut gate = gate.clone();
            // `borrow()`'s guard is dropped before each `await`; a dropped sender (`Err`) opens the
            // gate rather than parking forever, so a test that forgets to release cannot wedge.
            while !*gate.borrow() {
                if gate.changed().await.is_err() {
                    break;
                }
            }
        }
        if let Some(func) = &self.states_by_ids_func {
            return func(ids);
        }
        if let Some(e) = &self.by_id_err {
            return Err(e.clone());
        }
        let mut out = Vec::new();
        for id in ids {
            if let Some(iss) = self.by_id.get(id) {
                out.push(iss.clone());
            }
        }
        Ok(out)
    }

    async fn fetch_blocked_backlog_issues(&self) -> Result<Vec<Issue>, TrackerError> {
        self.lock().blocked_backlog_calls += 1;
        if let Some(e) = &self.blocked_backlog_err {
            return Err(e.clone());
        }
        Ok(self.blocked_backlog.clone())
    }

    async fn fetch_issue_branch_by_id(&self, id: &str) -> Result<(String, i64), TrackerError> {
        self.lock().branch_by_id_calls += 1;
        if let Some(e) = &self.branch_by_id_err {
            return Err(e.clone());
        }
        if let Some(bi) = self.branch_by_id.get(id) {
            return Ok((bi.branch.clone(), bi.pr));
        }
        Ok((String::new(), 0))
    }

    async fn move_issue_state(
        &self,
        issue_id: &str,
        team_id: &str,
        state_name: &str,
    ) -> Result<(), TrackerError> {
        self.lock().move_calls.push(MoveCall {
            issue_id: issue_id.to_string(),
            team_id: team_id.to_string(),
            state_name: state_name.to_string(),
        });
        match &self.move_err {
            Some(e) => Err(e.clone()),
            None => Ok(()),
        }
    }

    async fn move_issue_to_type(
        &self,
        issue_id: &str,
        team_id: &str,
        state_type: &str,
    ) -> Result<String, TrackerError> {
        self.lock().move_to_type_calls.push(MoveToTypeCall {
            issue_id: issue_id.to_string(),
            team_id: team_id.to_string(),
            state_type: state_type.to_string(),
        });
        if let Some(hook) = &self.move_to_type_hook {
            hook();
        }
        match &self.move_to_type_err {
            Some(e) => Err(e.clone()),
            None => Ok(self.move_to_type_name.clone()),
        }
    }

    async fn resolve_viewer(&self) -> Result<Viewer, TrackerError> {
        if let Some(e) = &self.viewer_err {
            return Err(e.clone());
        }
        Ok(self.viewer.clone())
    }

    async fn list_projects(&self) -> Result<Vec<Project>, TrackerError> {
        if let Some(e) = &self.projects_err {
            return Err(e.clone());
        }
        Ok(self.projects.clone())
    }

    async fn assign_issue(&self, issue_id: &str, assignee_id: &str) -> Result<(), TrackerError> {
        let mut inner = self.lock();
        inner.assign_calls.push(AssignCall {
            issue_id: issue_id.to_string(),
            assignee_id: assignee_id.to_string(),
        });
        if let Some(e) = &self.assign_err {
            return Err(e.clone());
        }
        inner
            .assignees
            .insert(issue_id.to_string(), assignee_id.to_string());
        Ok(())
    }

    async fn fetch_issue_assignee(&self, issue_id: &str) -> Result<String, TrackerError> {
        let mut inner = self.lock();
        inner.assignee_calls += 1;
        if let Some(e) = &self.assignee_err {
            return Err(e.clone());
        }
        if let Some(v) = self.assignee_read_override.get(issue_id) {
            return Ok(v.clone());
        }
        Ok(inner.assignees.get(issue_id).cloned().unwrap_or_default())
    }

    async fn create_comment(&self, issue_id: &str, body: &str) -> Result<String, TrackerError> {
        let mut inner = self.lock();
        inner.create_comment_calls.push(CommentCall {
            issue_id: issue_id.to_string(),
            body: body.to_string(),
        });
        if let Some(e) = &self.create_comment_err {
            return Err(e.clone());
        }
        inner.comment_seq += 1;
        let id = format!("cmt-{:04}", inner.comment_seq);
        // Stamp with the pinned clock (default `Utc::now`), the mirror of Go's `f.now()` — read
        // here, after the error check, so it borrows only `self.clock` (disjoint from `inner`).
        let created_at = match &self.clock {
            Some(clock) => clock(),
            None => Utc::now(),
        };
        inner
            .comments
            .entry(issue_id.to_string())
            .or_default()
            .push(Comment {
                id: id.clone(),
                body: body.to_string(),
                created_at,
            });
        Ok(id)
    }

    async fn list_comments(&self, issue_id: &str) -> Result<Vec<Comment>, TrackerError> {
        let mut inner = self.lock();
        inner.list_comments_calls += 1;
        if let Some(e) = &self.list_comments_err {
            return Err(e.clone());
        }
        Ok(inner.comments.get(issue_id).cloned().unwrap_or_default())
    }

    async fn delete_comment(&self, comment_id: &str) -> Result<(), TrackerError> {
        let mut inner = self.lock();
        inner.delete_comment_calls.push(comment_id.to_string());
        if let Some(e) = &self.delete_comment_err {
            return Err(e.clone());
        }
        for cs in inner.comments.values_mut() {
            if let Some(pos) = cs.iter().position(|c| c.id == comment_id) {
                cs.remove(pos);
                return Ok(());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Tracker, TrackerError};
    use rhapsody_core::Issue;
    use std::collections::HashMap;

    // Mirrors Go `fake.TestFakeImplementsTracker`: compile-time interface check.
    #[test]
    fn fake_implements_tracker() {
        let _: &dyn Tracker = &Fake::new();
    }

    // Mirrors Go `fake.TestFakeReturnsCandidatesAndRecordsCalls`.
    #[tokio::test]
    async fn returns_candidates_and_records_calls() {
        let mut f = Fake::new();
        f.candidates = vec![Issue {
            id: "1".into(),
            identifier: "MT-1".into(),
            state: "Todo".into(),
            ..Default::default()
        }];
        let got = f.fetch_candidate_issues().await.expect("no error");
        assert_eq!(got.len(), 1, "candidates = {got:?}");
        assert_eq!(got[0].identifier, "MT-1", "candidates = {got:?}");
        assert_eq!(f.candidate_calls(), 1, "CandidateCalls");
    }

    // Mirrors Go `fake.TestFakeByStatesEmptyShortCircuits`.
    #[tokio::test]
    async fn by_states_empty_short_circuits() {
        let f = Fake::new();
        let got = f.fetch_issues_by_states(&[]).await.expect("no error");
        assert!(got.is_empty(), "expected empty, got {got:?}");
    }

    // Mirrors Go `fake.TestFakeStatesByIDsFiltersByID`.
    #[tokio::test]
    async fn states_by_ids_filters_by_id() {
        let mut f = Fake::new();
        f.by_id = HashMap::from([
            (
                "1".into(),
                Issue {
                    id: "1".into(),
                    identifier: "MT-1".into(),
                    state: "Done".into(),
                    ..Default::default()
                },
            ),
            (
                "2".into(),
                Issue {
                    id: "2".into(),
                    identifier: "MT-2".into(),
                    state: "In Progress".into(),
                    ..Default::default()
                },
            ),
        ]);
        let got = f
            .fetch_issue_states_by_ids(&["2".into(), "missing".into()])
            .await
            .expect("no error");
        assert_eq!(got.len(), 1, "by ids = {got:?}");
        assert_eq!(got[0].id, "2", "by ids = {got:?}");
    }

    // Mirrors Go `fake.TestFakeErrorInjection`.
    #[tokio::test]
    async fn error_injection() {
        let mut f = Fake::new();
        let sentinel = TrackerError::Other("boom".into());
        f.candidates_err = Some(sentinel.clone());
        let err = f
            .fetch_candidate_issues()
            .await
            .expect_err("want injected error");
        assert_eq!(err, sentinel, "got {err:?}, want injected error");
    }
}
