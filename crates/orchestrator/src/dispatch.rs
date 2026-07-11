//! dispatch — parity port of Go `internal/orchestrator/dispatch.go`.
//!
//! The dispatch ORDERING (`sort_for_dispatch`), the intrinsic eligibility predicate (`eligible` /
//! `eligibility`, incl. the blockedBy dependency-mode gate and the required-label gate), the
//! blocker-clearing rule (`blocker_cleared`, INF-318), and the FRESH-pickup suppression guards
//! (`pr_suppressed` / `review_reopen_eligible`, INF-448) the selection pass (`select.rs`) and the
//! retry path (O5) schedule against (upstream §8.2). Slot accounting lives in [`crate::concurrency`].
//!
//! Deviations from the Go source, all behavior-preserving:
//!   * Go's `Eligible(iss, running, claimed, active, terminal, requiredLabels, mode, review,
//!     canceled)` (9 positional args) groups its six state-config sets into an [`EligibilityGate`]
//!     borrow, so the predicate takes `(iss, running, claimed, gate)` — idiomatic arity without a
//!     `clippy::too_many_arguments` allow. The gate fields map one-to-one onto the effective config
//!     (single-project path) or a resolved project (multi path), exactly as the Go call sites pass.
//!   * `running`/`claimed`/state sets are Go `map[string]bool` SETs → Rust [`HashSet`].
//!   * Go's zero-`time.Time` sentinel from `lastRunStartedAt` becomes `Option<DateTime<Utc>>`.
//!   * The otherwise-silent blocked-skip diagnostic (INF-249) logs via `tracing` (as the sibling
//!     crates do) instead of a threaded `slog` logger; the fields (`issue_identifier`, `blocker`,
//!     `blocker_state`) and per-blocker cadence are preserved.

use std::cmp::Ordering;
use std::collections::HashSet;

use chrono::{DateTime, Utc};
use rhapsody_config::{DEPENDENCY_MODE_DAG, DEPENDENCY_MODE_GRAPHITE};
use rhapsody_core::{BlockerRef, Issue, normalize_state};
use rhapsody_store::OUTCOME_INTERRUPTED;

use crate::orchestrator::Orchestrator;

/// Orders issues by priority asc (nil/`None` last), then created_at oldest first (nil/`None` last),
/// then identifier lexicographically (upstream §8.2). Stable, mirroring Go `SortForDispatch`
/// (`sort.SliceStable`).
pub fn sort_for_dispatch(issues: &mut [Issue]) {
    issues.sort_by(dispatch_cmp);
}

/// The shared global dispatch ordering (priority asc, created_at oldest, identifier lexicographic)
/// used by [`sort_for_dispatch`] and `select`'s `sort_tagged_stable`. Mirrors Go `dispatchLess`,
/// expressed as an [`Ordering`]-returning comparator (Rust's stable `sort_by` takes a comparator,
/// not a `less` predicate) — the induced total order is identical.
pub(crate) fn dispatch_cmp(a: &Issue, b: &Issue) -> Ordering {
    cmp_priority(a.priority, b.priority)
        .then_with(|| cmp_created_time(a.created_at, b.created_at))
        .then_with(|| a.identifier.cmp(&b.identifier))
}

/// Priority ordering with `None` (Go nil `*int`) sorting LAST. Mirrors Go `cmpPriority`.
fn cmp_priority(a: Option<i64>, b: Option<i64>) -> Ordering {
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(x), Some(y)) => x.cmp(&y),
    }
}

/// Created-time ordering with `None` (Go nil `*time.Time`) sorting LAST. Mirrors Go `cmpCreatedTime`.
fn cmp_created_time(a: Option<DateTime<Utc>>, b: Option<DateTime<Utc>>) -> Ordering {
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(x), Some(y)) => x.cmp(&y),
    }
}

/// The six state-config sets the eligibility predicate consults, grouped from Go's positional
/// `Eligible`/`eligibility` params (see the module docs). Every field is a borrow into the effective
/// config (single-project path) or a resolved project (multi path). `mode` is the dependency-mode
/// string (`""`/`"disabled"`/`"graphite"`/`"dag"`); `required_labels` empty ⇒ no label filter.
///
/// `pub` because it appears in the signature of the `pub` [`eligible`] predicate (which the retry
/// path, O5, will also call).
pub struct EligibilityGate<'a> {
    pub active: &'a HashSet<String>,
    pub terminal: &'a HashSet<String>,
    pub required_labels: &'a HashSet<String>,
    pub mode: &'a str,
    pub review: &'a HashSet<String>,
    pub canceled: &'a HashSet<String>,
}

/// The verdict of the dispatch-eligibility predicate plus, when the issue was rejected SOLELY
/// because of non-terminal blockers, the offending blockers in declaration order. `blocked_by` is
/// non-empty only when `ok` is false AND non-terminal blockers were the operative reason — so the
/// dispatch loop can surface exactly that (otherwise silent) drop and nothing else. Mirrors Go
/// `eligibilityResult`.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct EligibilityResult {
    pub ok: bool,
    pub blocked_by: Vec<BlockerRef>,
}

/// Reports whether an issue is intrinsically dispatch-eligible (upstream §8.2). Slot availability is
/// checked separately by the caller. It is a pure predicate (no logging) used from multiple call
/// sites (the poll selection + retry.go, O5). When the dispatch loop needs to explain WHY a
/// candidate was skipped it calls [`eligibility`] directly for the structured reason (see
/// [`Orchestrator::log_blocked_skip`], INF-249). Mirrors Go `Eligible`.
///
/// The `mode`/`review`/`canceled` fields on the gate add the DAG dependency-mode dimension
/// (INF-318): in disabled mode (`""`/`"disabled"`) the blocker check collapses to the pre-feature
/// terminal-only rule, so every call with `mode == ""` is byte-identical; graphite/dag widen WHEN a
/// blocker clears.
pub fn eligible(
    iss: &Issue,
    running: &HashSet<String>,
    claimed: &HashSet<String>,
    gate: &EligibilityGate<'_>,
) -> bool {
    eligibility(iss, running, claimed, gate).ok
}

/// The shared core of [`eligible`]: computes the verdict and, for the blocker case, collects every
/// non-terminal blocker. The non-blocker gates are evaluated first and short-circuit with an empty
/// `blocked_by`, so a candidate dropped for some other reason (invalid fields, non-active state,
/// already running/claimed, label miss) is never mislabeled as blocked. The `ok` value is identical
/// to [`eligible`]. Mirrors Go `eligibility`.
pub(crate) fn eligibility(
    iss: &Issue,
    running: &HashSet<String>,
    claimed: &HashSet<String>,
    gate: &EligibilityGate<'_>,
) -> EligibilityResult {
    if iss.id.is_empty()
        || iss.identifier.is_empty()
        || iss.title.is_empty()
        || iss.state.is_empty()
    {
        return EligibilityResult::default();
    }
    let st = normalize_state(&iss.state);
    if !gate.active.contains(&st) || gate.terminal.contains(&st) {
        return EligibilityResult::default();
    }
    if running.contains(&iss.id) || claimed.contains(&iss.id) {
        return EligibilityResult::default();
    }
    // Label gate: when required labels are configured, the issue must carry AT LEAST ONE of them
    // (match-ANY, case-insensitive). A miss short-circuits with empty `blocked_by` so it is never
    // mislabeled as blocker-held. Empty set ⇒ no filter, so the verdict is byte-identical to the
    // pre-label logic.
    if !gate.required_labels.is_empty() && !has_any_label(iss, gate.required_labels) {
        return EligibilityResult::default();
    }
    if st == "todo" {
        let blocked: Vec<BlockerRef> = iss
            .blocked_by
            .iter()
            .flatten()
            .filter(|b| !blocker_cleared(b, gate.mode, gate.review, gate.terminal, gate.canceled))
            .cloned()
            .collect();
        if !blocked.is_empty() {
            return EligibilityResult {
                ok: false,
                blocked_by: blocked,
            };
        }
    }
    EligibilityResult {
        ok: true,
        blocked_by: Vec::new(),
    }
}

/// Reports whether `iss` carries at least one of the wanted labels (match-ANY). Each issue label is
/// normalized at compare time so the gate holds even if a tracker adapter fails to lowercase; `want`
/// is assumed already normalized. Mirrors Go `hasAnyLabel`.
pub(crate) fn has_any_label(iss: &Issue, want: &HashSet<String>) -> bool {
    iss.labels
        .iter()
        .flatten()
        .any(|l| want.contains(&normalize_state(l)))
}

/// Reports whether a resolved `dependency_mode` turns the DAG orchestration ON (graphite or dag).
/// disabled (`""`/`"disabled"`) is OFF — the single source of truth for "is the feature active?"
/// shared by [`blocker_cleared`] and (later) the auto-promote pass (INF-318). Mirrors Go
/// `dependencyModeEnabled`.
pub(crate) fn dependency_mode_enabled(mode: &str) -> bool {
    mode == DEPENDENCY_MODE_GRAPHITE || mode == DEPENDENCY_MODE_DAG
}

/// Reports whether a blocker no longer holds its dependent, per the dependency mode (INF-318). A
/// blocker with unknown (`None`) state is NEVER cleared (conservative). Mirrors Go `blockerCleared`:
///
///   * disabled (default; `""`/`"disabled"`): cleared ONLY when terminal — byte-identical to the
///     pre-feature terminal-only rule (the disabled-is-noop invariant).
///   * graphite: cleared when the blocker is in a review state OR terminal (In Review is enough).
///   * dag: cleared ONLY when terminal/merged.
///
/// A cancelled blocker (in `canceled`) is NEVER cleared in graphite/dag — the premise is gone, so
/// the dependent is surfaced as orphaned by the auto-promote pass, not promoted.
pub(crate) fn blocker_cleared(
    b: &BlockerRef,
    mode: &str,
    review: &HashSet<String>,
    terminal: &HashSet<String>,
    canceled: &HashSet<String>,
) -> bool {
    let state = match &b.state {
        Some(s) => s,
        None => return false,
    };
    let st = normalize_state(state);
    if !dependency_mode_enabled(mode) {
        // disabled (default): terminal-only, byte-identical to today.
        return terminal.contains(&st);
    }
    if canceled.contains(&st) {
        // cancelled never promotes (orphan) — graphite/dag only.
        return false;
    }
    if terminal.contains(&st) {
        // merged/done clears in both enabled modes.
        return true;
    }
    // In Review clears in graphite only.
    mode == DEPENDENCY_MODE_GRAPHITE && review.contains(&st)
}

/// Returns a human label for a blocker in log output, preferring the tracker identifier (e.g.
/// `"INF-243"`), then the opaque id, then `"unknown"`. Mirrors Go `blockerIdentifier`.
pub(crate) fn blocker_identifier(b: &BlockerRef) -> String {
    if let Some(id) = b.identifier.as_deref().filter(|s| !s.is_empty()) {
        return id.to_string();
    }
    if let Some(id) = b.id.as_deref().filter(|s| !s.is_empty()) {
        return id.to_string();
    }
    "unknown".to_string()
}

/// Returns the blocker's state name for log output, with original casing preserved (e.g.
/// `"In Review"`). A `None`/empty state logs as `"unknown"` — the same value eligibility treats as
/// non-terminal (conservative; INF-249). Mirrors Go `blockerStateName`.
pub(crate) fn blocker_state_name(b: &BlockerRef) -> String {
    match &b.state {
        Some(s) if !s.is_empty() => s.clone(),
        _ => "unknown".to_string(),
    }
}

impl Orchestrator {
    /// Surfaces the otherwise-silent drop of a Todo candidate held back by non-terminal blockers
    /// (INF-249): one `tracing::info!` line per non-terminal blocker, each naming the blocked issue,
    /// the blocker, and the blocker's state. `blockers` is the [`EligibilityResult::blocked_by`]
    /// slice; an empty slice (any non-blocker drop, or an eligible issue) logs nothing. Mirrors Go
    /// `logBlockedSkip` (emitted via `tracing` rather than `slog`; same fields, same per-blocker
    /// cadence).
    pub(crate) fn log_blocked_skip(&self, iss: &Issue, blockers: &[BlockerRef]) {
        for b in blockers {
            tracing::info!(
                issue_identifier = %iss.identifier,
                blocker = %blocker_identifier(b),
                blocker_state = %blocker_state_name(b),
                "skipping dispatch: blocked by non-terminal blocker"
            );
        }
    }

    /// Reports whether a FRESH dispatch of `iss` should be suppressed because a prior run already
    /// materialized work as a linked GitHub PR (open OR merged) and no newer summons has arrived. It
    /// is the dispatch-side guard against re-picking an issue whose work is already done/in-review
    /// when its Linear state briefly flaps back to active. Mirrors Go `prSuppressed`.
    ///
    /// Re-open rule (INF-448): a summons strictly newer than the ticket's LAST RUN START lifts the
    /// suppression. Comparing to run START (not PR activity) honors a summons posted while a run was
    /// in flight. Store-off fallback: with no run-start watermark the pre-INF-448 PR-activity
    /// comparison applies; a PR with no comparable activity time stays lenient so a legitimately-
    /// summoned issue is never wedged by missing metadata. Applied to FRESH pickups only.
    pub(crate) fn pr_suppressed(&self, iss: &Issue) -> bool {
        if !iss.linked_pr {
            return false;
        }
        let summon = match iss.latest_summon_at {
            Some(s) => s,
            // a linked PR but no summons → already-done work, nothing new → suppress.
            None => return true,
        };
        match self.last_run_started_at(&iss.identifier) {
            None => match iss.latest_pr_activity_at {
                // no watermark of any kind but a summons exists → be lenient.
                None => false,
                // pre-INF-448 fallback: suppress unless the summons is after the PR's last activity.
                Some(pr) => summon <= pr,
            },
            // suppress unless the summons arrived after the last run began (feedback the last round
            // could not have consumed from its start).
            Some(start) => summon <= start,
        }
    }

    /// Reports whether a review-state issue should be re-engaged this tick by a fresh summons
    /// (symphony-29). The review-branch counterpart to [`eligible`] (which intentionally rejects
    /// non-active states); a review issue is handled ONLY here. Eligible iff it is neither running
    /// nor claimed, carries a `team_id` (required to promote it), carries a summons, AND that
    /// summons is strictly newer than the START of Symphony's last run on it. No run / store
    /// disabled / unparseable start ⇒ NOT eligible (Symphony never grabs a human-managed review
    /// ticket it has never worked; the check converges). Mirrors Go `reviewReopenEligible`.
    pub(crate) fn review_reopen_eligible(&self, iss: &Issue, running: &HashSet<String>) -> bool {
        if iss.id.is_empty() || iss.identifier.is_empty() || iss.team_id.is_empty() {
            return false;
        }
        if running.contains(&iss.id) || self.claimed.contains(&iss.id) {
            return false;
        }
        let summon = match iss.latest_summon_at {
            Some(s) => s,
            None => return false,
        };
        match self.last_run_started_at(&iss.identifier) {
            None => false, // never worked it (or store off / no start time) → don't grab it.
            Some(last) => summon > last,
        }
    }

    /// Returns the `started_at` of the most recent non-interrupted run Symphony recorded for
    /// `identifier`, parsed as RFC3339; `None` when there is no such run, the store is disabled, or
    /// no recent run has a parseable start time. It deliberately counts a still-running newest row
    /// (its start IS the boundary a mid-run summons must beat, INF-448); INTERRUPTED rows are skipped
    /// (boot recovery re-dispatches them, so counting their start would bury the triggering summons).
    /// Runs come back newest-first, so the first qualifying start is the newest. Mirrors Go
    /// `lastRunStartedAt` (its zero-`time.Time` sentinel becomes `None`).
    pub(crate) fn last_run_started_at(&self, identifier: &str) -> Option<DateTime<Utc>> {
        let runs = self.store().issue_history(identifier, "", 10).ok()?;
        for r in runs {
            if r.started_at.is_empty() || r.outcome == OUTCOME_INTERRUPTED {
                continue;
            }
            if let Ok(t) = DateTime::parse_from_rfc3339(&r.started_at) {
                return Some(t.with_timezone(&Utc));
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use chrono::{Duration as ChronoDuration, TimeZone, Utc};
    use rhapsody_config::DEPENDENCY_MODE_DISABLED;
    use rhapsody_core::{BlockerRef, Issue, normalize_state};

    use super::*;
    use crate::orchestrator::Orchestrator;
    use crate::testsupport::*;

    // Mirrors Go `TestSortForDispatchPriorityThenCreatedThenIdentifier`.
    #[test]
    fn sort_for_dispatch_priority_then_created_then_identifier() {
        let t0 = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let t1 = t0 + ChronoDuration::hours(1);
        let mut input = vec![
            Issue {
                identifier: "C-3".into(),
                priority: Some(2),
                created_at: Some(t1),
                ..Default::default()
            },
            Issue {
                identifier: "C-1".into(),
                priority: Some(1),
                created_at: Some(t1),
                ..Default::default()
            },
            Issue {
                identifier: "C-2".into(),
                priority: Some(2),
                created_at: Some(t0),
                ..Default::default()
            },
            // nil priority sorts last.
            Issue {
                identifier: "C-4".into(),
                priority: None,
                created_at: Some(t0),
                ..Default::default()
            },
            // tie with C-3 → identifier.
            Issue {
                identifier: "C-5".into(),
                priority: Some(2),
                created_at: Some(t1),
                ..Default::default()
            },
        ];
        sort_for_dispatch(&mut input);
        assert_eq!(ids(&input), vec!["C-1", "C-2", "C-3", "C-5", "C-4"]);
    }

    // Mirrors Go `TestEligibleHappyPath`.
    #[test]
    fn eligible_happy_path() {
        let g = GateData::standard();
        assert!(eligible(&base_issue(), &no_ids(), &no_ids(), &g.gate()));
    }

    // Mirrors Go `TestEligibleMissingFields`.
    #[test]
    fn eligible_missing_fields() {
        let g = GateData::standard();
        let mut bad = base_issue();
        bad.title = String::new();
        assert!(
            !eligible(&bad, &no_ids(), &no_ids(), &g.gate()),
            "issue missing title must be ineligible"
        );
    }

    // Mirrors Go `TestEligibleNonActiveOrTerminalState`.
    #[test]
    fn eligible_non_active_or_terminal_state() {
        let g = GateData::standard();
        let mut backlog = base_issue();
        backlog.state = "Backlog".into();
        assert!(
            !eligible(&backlog, &no_ids(), &no_ids(), &g.gate()),
            "non-active"
        );
        let mut done = base_issue();
        done.state = "Done".into();
        assert!(
            !eligible(&done, &no_ids(), &no_ids(), &g.gate()),
            "terminal"
        );
    }

    // Mirrors Go `TestEligibleRunningOrClaimed`.
    #[test]
    fn eligible_running_or_claimed() {
        let g = GateData::standard();
        let i = base_issue();
        assert!(
            !eligible(&i, &id_set(&["1"]), &no_ids(), &g.gate()),
            "running"
        );
        assert!(
            !eligible(&i, &no_ids(), &id_set(&["1"]), &g.gate()),
            "claimed"
        );
    }

    // Mirrors Go `TestEligibleTodoBlockerRule`.
    #[test]
    fn eligible_todo_blocker_rule() {
        let g = GateData::standard();
        let mut blocked = issue("2", "MT-2", "Todo");
        blocked.blocked_by = Some(vec![blocker(Some("MT-9"), Some("In Progress"))]);
        assert!(
            !eligible(&blocked, &no_ids(), &no_ids(), &g.gate()),
            "Todo with non-terminal blocker must be ineligible"
        );

        let mut ok = blocked.clone();
        ok.blocked_by = Some(vec![blocker(Some("MT-9"), Some("Done"))]);
        assert!(
            eligible(&ok, &no_ids(), &no_ids(), &g.gate()),
            "Todo with terminal blocker should be eligible"
        );

        let mut unk = blocked.clone();
        unk.blocked_by = Some(vec![blocker(Some("MT-9"), None)]);
        assert!(
            !eligible(&unk, &no_ids(), &no_ids(), &g.gate()),
            "unknown-state blocker must be ineligible"
        );

        let mut ip = issue("3", "MT-3", "In Progress");
        ip.blocked_by = Some(vec![blocker(None, Some("In Progress"))]);
        assert!(
            eligible(&ip, &no_ids(), &no_ids(), &g.gate()),
            "blocker rule applies only to Todo"
        );

        let mut empty = blocked.clone();
        empty.blocked_by = Some(vec![blocker(Some("MT-9"), Some(""))]);
        assert!(
            !eligible(&empty, &no_ids(), &no_ids(), &g.gate()),
            "empty-string blocker state must be ineligible"
        );
    }

    // Mirrors Go `TestEligibleLabelGate`.
    #[test]
    fn eligible_label_gate() {
        assert!(
            eligible(
                &base_issue(),
                &no_ids(),
                &no_ids(),
                &GateData::standard().gate()
            ),
            "empty required-label set must not filter"
        );
        let g = GateData::standard().with_labels(&["symphony-do"]);

        let mut hit = base_issue();
        hit.labels = Some(vec!["symphony-do".into(), "infra".into()]);
        assert!(
            eligible(&hit, &no_ids(), &no_ids(), &g.gate()),
            "carrying the label → eligible"
        );

        let mut miss = base_issue();
        miss.labels = Some(vec!["infra".into()]);
        assert!(
            !eligible(&miss, &no_ids(), &no_ids(), &g.gate()),
            "lacking the label → ineligible"
        );

        assert!(
            !eligible(&base_issue(), &no_ids(), &no_ids(), &g.gate()),
            "no labels + a required set → ineligible"
        );

        let mut mixed = base_issue();
        mixed.labels = Some(vec!["Symphony-Do".into()]);
        assert!(
            eligible(&mixed, &no_ids(), &no_ids(), &g.gate()),
            "label match is case-insensitive"
        );

        // A label miss must NOT be reported as a blocker drop.
        let res = eligibility(&miss, &no_ids(), &no_ids(), &g.gate());
        assert!(
            !res.ok && res.blocked_by.is_empty(),
            "label-miss must not be a blocker drop"
        );
    }

    // Mirrors Go `TestEligibilityReportsAllNonTerminalBlockers`.
    #[test]
    fn eligibility_reports_all_non_terminal_blockers() {
        let g = GateData::standard();
        let mut iss = issue("2", "MT-2", "Todo");
        iss.blocked_by = Some(vec![
            blocker(Some("MT-9"), Some("In Review")), // non-terminal → included
            blocker(Some("MT-8"), Some("Done")),      // terminal → excluded
            blocker(Some("MT-7"), None),              // unknown → included (conservative)
        ]);
        let res = eligibility(&iss, &no_ids(), &no_ids(), &g.gate());
        assert!(!res.ok, "blocked Todo must be ineligible");
        assert_eq!(res.blocked_by.len(), 2, "want 2 non-terminal blockers");
        assert_eq!(
            blocker_identifier(&res.blocked_by[0]),
            "MT-9",
            "declaration order preserved"
        );
        assert_eq!(blocker_identifier(&res.blocked_by[1]), "MT-7");
    }

    // Mirrors Go `TestEligibilityOKReportsNoBlockers`.
    #[test]
    fn eligibility_ok_reports_no_blockers() {
        let g = GateData::standard();
        let res = eligibility(&base_issue(), &no_ids(), &no_ids(), &g.gate());
        assert!(res.ok && res.blocked_by.is_empty(), "eligible issue");

        let mut tb = issue("2", "MT-2", "Todo");
        tb.blocked_by = Some(vec![blocker(Some("MT-9"), Some("Done"))]);
        let res = eligibility(&tb, &no_ids(), &no_ids(), &g.gate());
        assert!(
            res.ok && res.blocked_by.is_empty(),
            "terminal-blocker issue"
        );
    }

    // Mirrors Go `TestEligibilityNonBlockerReasonsReportNoBlockers`.
    #[test]
    fn eligibility_non_blocker_reasons_report_no_blockers() {
        let g = GateData::standard();
        let with_blocker = |mut base: Issue| {
            base.blocked_by = Some(vec![blocker(Some("MT-9"), Some("In Review"))]);
            base
        };
        struct Case {
            name: &'static str,
            iss: Issue,
            running: std::collections::HashSet<String>,
            claimed: std::collections::HashSet<String>,
            want_ok: bool,
        }
        let cases = vec![
            Case {
                name: "running",
                iss: with_blocker(issue("1", "MT-1", "Todo")),
                running: id_set(&["1"]),
                claimed: no_ids(),
                want_ok: false,
            },
            Case {
                name: "claimed",
                iss: with_blocker(issue("1", "MT-1", "Todo")),
                running: no_ids(),
                claimed: id_set(&["1"]),
                want_ok: false,
            },
            Case {
                name: "non-active",
                iss: with_blocker(issue("1", "MT-1", "Backlog")),
                running: no_ids(),
                claimed: no_ids(),
                want_ok: false,
            },
            Case {
                name: "missing-fields",
                iss: with_blocker({
                    let mut i = issue("1", "MT-1", "Todo");
                    i.title = String::new();
                    i
                }),
                running: no_ids(),
                claimed: no_ids(),
                want_ok: false,
            },
            Case {
                name: "in-progress-eligible",
                iss: with_blocker(issue("1", "MT-1", "In Progress")),
                running: no_ids(),
                claimed: no_ids(),
                want_ok: true,
            },
        ];
        for tc in cases {
            let res = eligibility(&tc.iss, &tc.running, &tc.claimed, &g.gate());
            assert_eq!(res.ok, tc.want_ok, "{}: ok", tc.name);
            assert!(
                res.blocked_by.is_empty(),
                "{}: must not be a blocker drop",
                tc.name
            );
        }
    }

    // Mirrors Go `TestBlockerStateNameAndIdentifier`.
    #[test]
    fn blocker_state_name_and_identifier() {
        assert_eq!(
            blocker_state_name(&blocker(None, Some("In Review"))),
            "In Review"
        );
        assert_eq!(blocker_state_name(&blocker(None, None)), "unknown");
        assert_eq!(blocker_state_name(&blocker(None, Some(""))), "unknown");
        assert_eq!(blocker_identifier(&blocker(Some("MT-9"), None)), "MT-9");
        assert_eq!(
            blocker_identifier(&BlockerRef {
                id: Some("uuid-123".into()),
                identifier: None,
                state: None
            }),
            "uuid-123"
        );
        assert_eq!(
            blocker_identifier(&BlockerRef {
                id: None,
                identifier: None,
                state: None
            }),
            "unknown"
        );
    }

    // Mirrors Go `TestPRSuppressed`.
    #[test]
    fn pr_suppressed() {
        let (o, st) = orch_with_store();

        assert!(
            !o.pr_suppressed(&base_issue()),
            "no linked PR → not suppressed"
        );

        let mut pr_no_comment = base_issue();
        pr_no_comment.linked_pr = true;
        assert!(
            o.pr_suppressed(&pr_no_comment),
            "linked PR, no summons → suppressed"
        );

        // Linked PR + a summons but no run-start watermark and no PR-activity time → lenient.
        let mut lenient = pr_no_comment.clone();
        lenient.latest_summon_at = Some(Utc.with_ymd_and_hms(2026, 6, 3, 12, 0, 0).unwrap());
        assert!(
            !o.pr_suppressed(&lenient),
            "linked PR + summons but no watermark → lenient"
        );

        let run_start = Utc.with_ymd_and_hms(2026, 6, 3, 12, 0, 0).unwrap();
        seed_run(
            st.as_ref(),
            "u-1",
            "MT-1",
            run_start + ChronoDuration::minutes(1),
        );

        let mut base = base_issue();
        base.id = "u-1".into();
        base.identifier = "MT-1".into();
        base.linked_pr = true;

        // Summons AFTER run start → NOT suppressed (re-dispatch); PR activity newer than the summons
        // must no longer matter (the INF-448 dead zone).
        let mut newer = base.clone();
        newer.latest_summon_at = Some(run_start + ChronoDuration::hours(1));
        newer.latest_pr_activity_at = Some(run_start + ChronoDuration::hours(2));
        assert!(
            !o.pr_suppressed(&newer),
            "summons newer than last run START must lift suppression"
        );

        // Summons BEFORE run start → suppressed (stale).
        let mut older = base.clone();
        older.latest_summon_at = Some(run_start - ChronoDuration::hours(1));
        assert!(
            o.pr_suppressed(&older),
            "summons older than last run START must stay suppressed"
        );
    }

    // Mirrors Go `TestPRSuppressedStoreDisabled` (Noop store → PR-activity fallback).
    #[test]
    fn pr_suppressed_store_disabled() {
        let o = Orchestrator::new("WORKFLOW.md"); // Noop store (never set_store'd)
        let pr = Utc.with_ymd_and_hms(2026, 6, 3, 12, 0, 0).unwrap();
        let mut iss = base_issue();
        iss.linked_pr = true;
        assert!(
            o.pr_suppressed(&iss),
            "linked PR, no summons → suppressed even with the store off"
        );

        iss.latest_pr_activity_at = Some(pr);
        iss.latest_summon_at = Some(pr - ChronoDuration::hours(1));
        assert!(
            o.pr_suppressed(&iss),
            "store off: summons older than PR activity → suppressed"
        );

        iss.latest_summon_at = Some(pr + ChronoDuration::hours(1));
        assert!(
            !o.pr_suppressed(&iss),
            "store off: summons newer than PR activity → not suppressed"
        );

        iss.latest_pr_activity_at = None;
        assert!(
            !o.pr_suppressed(&iss),
            "store off, no PR-activity time, a summons → lenient"
        );
    }

    // Mirrors Go `TestDependencyModeEnabled` (dispatch_depmode_test.go).
    #[test]
    fn dependency_mode_enabled_table() {
        for (mode, want) in [
            ("", false),
            ("disabled", false),
            ("graphite", true),
            ("dag", true),
        ] {
            assert_eq!(dependency_mode_enabled(mode), want, "mode {mode:?}");
        }
    }

    // Mirrors Go `TestBlockerClearedTable`.
    #[test]
    fn blocker_cleared_table() {
        let (review, terminal, canceled) = (dep_review(), dep_terminal(), dep_canceled());
        let cases: &[(&str, &str, bool)] = &[
            // disabled: terminal-only; review/active NOT cleared; cancelled-but-terminal cleared; nil not.
            ("disabled", "Done", true),
            ("disabled", "In Review", false),
            ("disabled", "Todo", false),
            ("disabled", "Cancelled", true),
            ("disabled", "", false),
            ("", "Done", true), // unset == disabled
            ("", "In Review", false),
            // graphite: review OR terminal clears; active not; cancelled never; nil not.
            ("graphite", "In Review", true),
            ("graphite", "Done", true),
            ("graphite", "Todo", false),
            ("graphite", "Cancelled", false),
            ("graphite", "", false),
            // dag: terminal-only; review NOT cleared; cancelled never; nil not.
            ("dag", "In Review", false),
            ("dag", "Done", true),
            ("dag", "Cancelled", false),
            ("dag", "", false),
        ];
        for (mode, state, want) in cases {
            let got = blocker_cleared(&blocker_state(state), mode, &review, &terminal, &canceled);
            assert_eq!(got, *want, "blocker_cleared({state:?}, mode={mode:?})");
        }
    }

    // Mirrors Go `TestBlockerClearedDisabledEqualsPreFeature`.
    #[test]
    fn blocker_cleared_disabled_equals_pre_feature() {
        let (review, terminal, canceled) = (dep_review(), dep_terminal(), dep_canceled());
        let old_blocker_terminal = |b: &BlockerRef| -> bool {
            match &b.state {
                None => false,
                Some(s) => terminal.contains(&normalize_state(s)),
            }
        };
        for state in ["Todo", "In Review", "Done", "Cancelled", ""] {
            let b = blocker_state(state);
            let got = blocker_cleared(&b, DEPENDENCY_MODE_DISABLED, &review, &terminal, &canceled);
            assert_eq!(
                got,
                old_blocker_terminal(&b),
                "disabled blocker_cleared({state:?})"
            );
        }
    }

    // Mirrors Go `TestEligibilityModeAwareInReviewBlocker`.
    #[test]
    fn eligibility_mode_aware_in_review_blocker() {
        let mut iss = issue("1", "MT-2", "Todo");
        iss.blocked_by = Some(vec![blocker_state("In Review")]);
        let cases: &[(&str, bool, usize)] = &[
            ("graphite", true, 0),
            ("dag", false, 1),
            ("disabled", false, 1),
            ("", false, 1),
        ];
        for (mode, want_ok, want_blk) in cases {
            let g = GateData::dep().with_mode(mode);
            let res = eligibility(&iss, &no_ids(), &no_ids(), &g.gate());
            assert_eq!(res.ok, *want_ok, "mode={mode:?}");
            assert_eq!(
                res.blocked_by.len(),
                *want_blk,
                "mode={mode:?}: must surface for logging"
            );
        }
    }
}
