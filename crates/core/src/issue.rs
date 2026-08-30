//! Shared issue domain types ported from Go `internal/core/issue.go`.
//!
//! Field-for-field parity with the Go structs, including pointer→`Option` optionality: a Go
//! `*T` field distinguishes unset from zero, so it maps to `Option<T>`; a Go nil slice maps to
//! `Option<Vec<T>>` so a zero-value issue's slice fields are `None`, mirroring Go's `nil`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// `LinkedPRRef` is one GitHub pull request linked to the issue (via a Linear github
/// attachment). `merged` is read from the attachment metadata (status:"merged" / mergedAt);
/// the GitHub-summons enrichment pass polls comments only for UNMERGED refs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LinkedPRRef {
    pub owner: String,
    pub repo: String,
    pub number: i64,
    pub merged: bool,
}

/// `Comment` is a normalized issue comment. It carries only the fields the shared-pool claim
/// election needs: the server-assigned `id` (identity + tie-break), the raw `body` (the claim
/// marker is parsed from it), and the immutable server `created_at` (the election's ordering
/// key). INF-477.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Comment {
    pub id: String,
    pub body: String,
    pub created_at: DateTime<Utc>,
}

/// `BlockerRef` is a normalized reference to a blocking issue (upstream §4.1.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BlockerRef {
    pub id: Option<String>,
    pub identifier: Option<String>,
    pub state: Option<String>,
}

/// `Issue` is the normalized issue record used across orchestration, prompt rendering, and
/// observability (upstream §4.1.1).
///
/// `Default` is derived (beyond the plan's base derive set) so the ported zero-value test can
/// construct `Issue::default()` — the parity mirror of Go's `var i Issue` — and so trackers can
/// build issues incrementally; every field's type is itself `Default`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Issue {
    pub id: String,
    pub identifier: String,
    pub title: String,
    pub description: Option<String>,
    pub priority: Option<i64>,
    pub state: String,
    pub branch_name: Option<String>,
    pub url: Option<String>,
    /// `team_id` is the Linear team UUID the issue belongs to, used to resolve a
    /// workflow-state UUID by name when moving the issue's state (the review-summon reopen
    /// write). Empty for trackers/queries that do not supply it.
    pub team_id: String,
    /// Normalized to lowercase by the tracker adapter.
    pub labels: Option<Vec<String>>,
    pub blocked_by: Option<Vec<BlockerRef>>,
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,

    /// `linked_pr` reports whether the issue has at least one linked GitHub pull request
    /// (open OR merged), set by the tracker from Linear's GitHub-integration attachments. A
    /// linked PR means a prior run already materialized work for this issue, so a FRESH
    /// dispatch is suppressed (see orchestrator.prSuppressed) unless newer feedback arrived.
    pub linked_pr: bool,
    /// `latest_pr_activity_at` is the most recent update time across the issue's linked PRs
    /// (`None` when there are none). Populated by the trackers for observability; no longer the
    /// summon-reopen watermark (the gate compares to the last run's start instead — INF-448: a
    /// round's own end-of-round commits bump PR activity past a mid-run summons).
    pub latest_pr_activity_at: Option<DateTime<Utc>>,
    /// `linked_prs` lists the issue's linked GitHub PRs with per-PR merged state. Populated
    /// from the same attachments as `linked_pr`/`latest_pr_activity_at`.
    pub linked_prs: Option<Vec<LinkedPRRef>>,
    /// `latest_summon_at` is the newest timestamp of a comment whose body contains the
    /// configured summon token (e.g. "@symphony"), case-insensitive (`None` when none). It is
    /// the single re-engagement signal across the system: a summons newer than the START of
    /// the daemon's last run on the ticket re-opens a PR-suppressed issue for dispatch
    /// (prSuppressed) and re-engages a review-state ticket (reviewReopenEligible) — so a
    /// summons posted mid-run still counts after that run ends (INF-448). Plain comments
    /// without the token do NOT set it.
    pub latest_summon_at: Option<DateTime<Utc>>,
    /// `latest_summon_body` is the comment body of the newest summons (the SAME comment whose
    /// time is `latest_summon_at`), empty when there is no summons or the source cannot surface
    /// a body. Carried so a mid-run summons delivered to the live run's operator mailbox
    /// conveys the actual instruction (INF-448). Never used for eligibility — only for the
    /// delivered message text.
    pub latest_summon_body: String,

    /// `milestone_id` / `milestone_name` are the Linear project milestone the issue belongs to
    /// (both empty when the issue has no milestone). Populated by the tracker for
    /// log/observability visibility; milestone-based candidate filtering itself is server-side.
    pub milestone_id: String,
    pub milestone_name: String,

    /// `assignee_id` / `assignee_name` are the Linear user the issue is assigned to (both empty
    /// when unassigned). Populated by the tracker for log/observability visibility;
    /// assignee-based candidate filtering itself is server-side — the daemon only fetches issues
    /// assigned to the owner of its API key (the resolved `viewer`).
    pub assignee_id: String,
    pub assignee_name: String,
}

/// `normalize_state` lowercases and trims a tracker state for comparison (upstream §4.2).
/// Mirrors Go's `NormalizeState`: `strings.ToLower(strings.TrimSpace(s))`.
pub fn normalize_state(s: &str) -> String {
    s.trim().to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Mirrors Go `core.TestNormalizeState` (issue_test.go).
    #[test]
    fn normalize_state_trims_and_lowercases() {
        let cases = [
            ("Todo", "todo"),
            ("In Progress", "in progress"),
            ("  DONE  ", "done"),
            ("", ""),
        ];
        for (input, want) in cases {
            assert_eq!(normalize_state(input), want, "normalize_state({input:?})");
        }
    }

    // Mirrors Go `core.TestIssueZeroValueOptionalFields` (issue_test.go): a zero-value issue
    // has `None` for its pointer-derived optional fields and `None` (not an empty `Vec`) for
    // its nil-slice-derived fields.
    #[test]
    fn issue_zero_value_optional_fields() {
        let i = Issue::default();
        assert!(
            i.description.is_none() && i.priority.is_none() && i.created_at.is_none(),
            "optional fields should be None on zero value"
        );
        assert!(
            i.labels.is_none() && i.blocked_by.is_none(),
            "slice fields should be None on zero value"
        );
    }
}
