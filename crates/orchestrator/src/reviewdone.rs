//! reviewdone — moving an implementation ticket to its terminal state when the pull request the
//! daemon parked it for MERGES (STUDIO-712).
//!
//! **No Go v0.4.0 counterpart, and this one is a deliberate DIVERGENCE rather than new surface.**
//! The frozen reference has `TerminalStates`, but it only ever READS them — for claim-skip and for
//! startup worktree cleanup — and it has no pull-request merge watcher at all, so nothing in Go
//! ever moves a ticket INTO a terminal state. Rhapsody adds that one transition, and `README.md`'s
//! Divergences section carries the entry.
//!
//! # Why it exists
//!
//! [`crate::handoff`] moves a finished run's ticket into the configured review state and stops
//! there. Nothing has ever moved it out: an operator merging the pull request leaves the ticket
//! sitting in review forever, and the column fills with work that shipped. The maintainer's own
//! merge checklist carries the manual step this replaces.
//!
//! # What it is allowed to act on, and why that is exactly the watch set
//!
//! The scope is narrow on purpose: only a ticket THIS DAEMON parked in a review state, for a pull
//! request it can still resolve. [`crate::reviewintro`]'s watch set is precisely that population —
//! a row exists because [`Orchestrator::plan_review_intro`] ran at
//! [`handle_handoff_run`](Orchestrator::handle_handoff_run), on the run's own trusted repository
//! binding, in the same breath as the review-state move — and it records WHERE it came from in
//! [`ReviewWatchRow::introduced_by`], as `handoff:<identifier>`. So the ticket this transition may
//! move is read off the row rather than inferred, and a row an OPERATOR introduced through the
//! console (`console:…`) names no ticket and moves none.
//!
//! That is also why the merge edge is [`crate::reviewwatch`]'s and not a watcher of its own: the
//! ticketless watcher already sweeps exactly these pull requests off-loop through
//! [`crate::prstate`], already reads merge state, and already drops a merged one. A second timer
//! polling the same numbers would double this daemon's GitHub spend to learn a fact the first one
//! already has.
//!
//! # Merged, and nothing else
//!
//! A **closed-unmerged** pull request is out of scope and stays a human's call. It looks identical
//! to a merged one everywhere the watch set cares (`open: false`, retired), and the temptation is
//! to treat "not open any more" as "finished" — but a closed pull request is abandoned work, its
//! ticket is not done, and auto-Cancelling it would destroy the one signal a maintainer has that
//! something needs picking up. So this module keys on [`PrStatus::Merged`](crate::ghsummons::PrStatus)
//! alone; `Closed`, `Gone` and `Untrusted` retire the row and move nothing.
//!
//! The STATUS is the key, deliberately, and not [`PrSnapshot::merged_at`](crate::ghsummons::PrSnapshot):
//! `Merged` can only come from GitHub answering `state: MERGED`, and an unrecognised state is an
//! ERROR rather than a default, so the status cannot be arrived at by accident. `mergedAt` is
//! best-effort parsed and is `None` on any timestamp that will not parse — requiring both would
//! turn a formatting change at GitHub into a ticket that never closes and says nothing about why.
//!
//! # Off the loop
//!
//! The decision is made on the control task, where the watch set and the store are single-writer:
//! [`Orchestrator::plan_review_done`] resolves the ticket and returns a [`ReviewDonePlan`], which
//! rides back to the watcher task on [`ReviewSweepReport::done`](crate::reviewwatch::ReviewSweepReport::done).
//! The tracker write — a Linear round-trip — happens out there, exactly as [`crate::handoff`]
//! resolves its plan on the loop and moves the ticket off it.
//!
//! What that costs, stated plainly: the moves run before the watcher's next sleep, so a slow Linear
//! delays the next PR-state sweep by however long it takes. That is the same containment the
//! watcher already accepts for a slow `gh` — the delay lands on a task nobody is waiting on, and
//! never on the control task — and it is bounded by one move per merged pull request per tick,
//! which on any real installation is nought or one.

use rhapsody_store::RunFilter;

use crate::orchestrator::Orchestrator;
use crate::prstate::PrCoord;
use crate::reviewintro::REVIEW_ORIGIN_HANDOFF;
use crate::stop::ControlHandle;

/// One merged pull request's implementation ticket, and the terminal state it is going to.
///
/// Everything the tracker needs is resolved BEFORE this leaves the control task — the identifier
/// off the watch row, the opaque ids off the run row — so the off-loop half makes one call and has
/// no decision left to get wrong. A field is never empty: a plan that could not be completed is
/// `None`, because `move_issue_state` rejects an empty issue, team or state anyway and a plan that
/// is certain to fail is worse than no plan (it reads, in the logs, like a transition that was
/// attempted for a real reason).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReviewDonePlan {
    /// `owner/repo#number` — the pull request whose merge triggered this, for the log line.
    pub pr: String,
    /// The tracker's opaque issue id.
    pub issue_id: String,
    /// The tracker team the state name is resolved within.
    pub team_id: String,
    /// The human ticket id, e.g. `STUDIO-712`.
    pub identifier: String,
    /// The terminal state NAME, from `teams.review.done_state`.
    pub state: String,
}

/// The ticket named by a `handoff:<identifier>` origin, or `None` for any other origin.
///
/// The one reader of that spelling besides the writer in [`crate::reviewintro`], and the whole of
/// this module's scope guard: a `console:` row (or a future origin nobody has written yet) yields
/// `None` and moves no ticket.
pub(crate) fn handoff_ticket(introduced_by: &str) -> Option<&str> {
    let identifier = introduced_by
        .strip_prefix(REVIEW_ORIGIN_HANDOFF)?
        .strip_prefix(':')?
        .trim();
    (!identifier.is_empty()).then_some(identifier)
}

impl Orchestrator {
    /// Resolves the ticket a just-MERGED pull request's implementation work belongs to, and the
    /// terminal state it should move to — or `None` when this daemon must not, or cannot, move one.
    ///
    /// `rows` is the tick's watch-set snapshot, passed in rather than re-read because the caller
    /// retires these very rows in the same breath — reading the STORE here would race that
    /// retirement, and reading the snapshot cannot, whichever order the two run in.
    ///
    /// `None` covers five distinct situations, all of them quiet by design except the two that
    /// point at a real problem:
    ///
    /// * the transition is not configured (or Teams / ticketless review is off) — the default;
    /// * no row of this pull request came from a handoff, so no ticket is in scope;
    /// * the store could not be read — warned, and the row is retired anyway;
    /// * no run row survives for that identifier — warned: the ticket is real but the daemon has
    ///   no opaque id to address the tracker with, and history retention is the usual reason;
    /// * the run row carries no issue or team id — warned, for the same reason.
    ///
    /// Called once per merged pull request per tick, and normally exactly once ever: the caller
    /// retires the rows immediately afterwards and a retired pull request is never polled again. If
    /// that retirement FAILS (a store error, which warns) the next tick observes the same merge and
    /// plans it again — harmless, because the move is by NAME to a state the ticket is already in,
    /// so a repeat is a redundant write rather than a wrong one.
    pub(crate) fn plan_review_done(
        &self,
        rows: &[rhapsody_store::ReviewWatchRow],
        pr: &PrCoord,
    ) -> Option<ReviewDonePlan> {
        let state = self.teams.as_ref()?.review_done_state()?.to_string();
        let identifier = rows
            .iter()
            .filter(|row| crate::reviewwatch::row_is(row, pr))
            .find_map(|row| handoff_ticket(&row.introduced_by))?
            .to_string();
        // The opaque tracker ids the move needs live on the run that produced the pull request.
        // The LATEST run of that ticket, because a retried ticket has several and they all carry
        // the same issue and team — `list_issue_runs` returns one row per identifier, newest first.
        let runs = match self.store().list_issue_runs(RunFilter {
            issue: identifier.clone(),
            limit: 1,
            ..RunFilter::default()
        }) {
            Ok(runs) => runs,
            Err(e) => {
                tracing::warn!(pr = %pr, issue_identifier = %identifier, err = %e, "auto-done: the run history could not be read; the ticket was not moved");
                return None;
            }
        };
        let Some(run) = runs.into_iter().next() else {
            tracing::warn!(pr = %pr, issue_identifier = %identifier, "auto-done: no run of this ticket is left in history, so the tracker cannot be addressed; the ticket was not moved");
            return None;
        };
        if run.issue_id.is_empty() || run.team_id.is_empty() {
            tracing::warn!(pr = %pr, issue_identifier = %identifier, "auto-done: the run row carries no tracker issue/team id; the ticket was not moved");
            return None;
        }
        Some(ReviewDonePlan {
            pr: pr.to_string(),
            issue_id: run.issue_id,
            team_id: run.team_id,
            identifier,
            state,
        })
    }
}

impl ControlHandle {
    /// Moves one merged pull request's implementation ticket to its terminal state, off the control
    /// loop — the same by-NAME `MoveIssueState` [`ControlHandle::handoff_run`] uses, resolving the
    /// tracker the same way.
    ///
    /// A failure is logged and dropped rather than returned to a caller who has nothing to do with
    /// it: the watcher's next tick no longer holds the row (the pull request is merged and retired),
    /// so there is nothing to retry against and nobody waiting on an answer. The ticket stays in
    /// review, which is exactly where it was before this feature existed.
    pub(crate) async fn finish_review_ticket(&self, plan: ReviewDonePlan) {
        match self
            .move_issue_state(&plan.issue_id, &plan.team_id, &plan.state)
            .await
        {
            Ok(()) => tracing::info!(
                pr = %plan.pr,
                issue_identifier = %plan.identifier,
                state = %plan.state,
                "auto-done: the merged pull request's ticket was moved to its terminal state"
            ),
            Err(e) => tracing::warn!(
                pr = %plan.pr,
                issue_identifier = %plan.identifier,
                state = %plan.state,
                err = %e,
                "auto-done: the terminal-state move failed; the ticket stays in review"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rhapsody_config::teams::{Review, ReviewMode, Teams};
    use rhapsody_store::{
        REVIEW_STATUS_REQUESTED, ReviewWatchKey, ReviewWatchRow, RunStart, Sqlite, StorePath,
    };
    use rhapsody_tracker::TrackerError;
    use rhapsody_tracker::fake::Fake;

    use super::*;
    use crate::reviewintro::REVIEW_ORIGIN_CONSOLE;
    use crate::reviewwatch::{ControlWatchSink, ReviewWatchSink};
    use crate::testsupport::{empty_effective, set_of};

    const OWNER: &str = "makewhatis";
    const REPO: &str = "rhapsody";

    /// Teams on, ticketless review, `done_state` named — everything the transition gates on.
    fn ticketless_done(state: &str) -> Teams {
        Teams {
            enabled: true,
            review: Review {
                mode: ReviewMode::Ticketless,
                done_state: state.to_string(),
                ..Review::default()
            },
            ..Teams::disabled()
        }
    }

    fn orch(teams: Teams) -> Orchestrator {
        orch_tracked(teams, Arc::new(Fake::new()))
    }

    /// The same, over a tracker the CALLER keeps a handle on — what the two move tests read the
    /// recorded `move_issue_state` call off.
    fn orch_tracked(teams: Teams, tracker: Arc<Fake>) -> Orchestrator {
        let mut eff = empty_effective(tracker);
        eff.terminal_states = set_of(&["done"]);
        let mut o = Orchestrator::new("WORKFLOW.md");
        o.eff = Some(eff);
        o.teams = Some(teams);
        o.set_store(Arc::new(
            Sqlite::open(StorePath::InMemory).expect("open in-memory store"),
        ));
        o
    }

    fn coord(number: i64) -> PrCoord {
        PrCoord::new(OWNER, REPO, number)
    }

    fn row(number: i64, introduced_by: &str) -> ReviewWatchRow {
        ReviewWatchRow {
            key: ReviewWatchKey {
                owner: OWNER.to_string(),
                repo: REPO.to_string(),
                number,
                reviewer: "bob".to_string(),
            },
            author: "alice".to_string(),
            introduced_by: introduced_by.to_string(),
            requested_sha: String::new(),
            last_reviewed_sha: String::new(),
            status: REVIEW_STATUS_REQUESTED.to_string(),
            open: true,
        }
    }

    /// A finished run of `issue` — the row the opaque tracker ids are read off.
    fn run_of(o: &Orchestrator, issue: &str, issue_id: &str, team_id: &str) {
        o.store()
            .start_run(RunStart {
                issue_id: issue_id.to_string(),
                issue_identifier: issue.to_string(),
                team_id: team_id.to_string(),
                ..RunStart::default()
            })
            .expect("start run");
    }

    /// The happy path: a handoff-introduced row names its ticket, and the plan carries the opaque
    /// ids the tracker needs plus the configured state.
    #[test]
    fn a_handoff_row_plans_its_tickets_move() {
        let o = orch(ticketless_done("Done"));
        run_of(&o, "STUDIO-712", "ID-712", "TEAM-1");

        let plan = o
            .plan_review_done(&[row(64, "handoff:STUDIO-712")], &coord(64))
            .expect("a plan");

        assert_eq!(
            plan,
            ReviewDonePlan {
                pr: "makewhatis/rhapsody#64".to_string(),
                issue_id: "ID-712".to_string(),
                team_id: "TEAM-1".to_string(),
                identifier: "STUDIO-712".to_string(),
                state: "Done".to_string(),
            }
        );
    }

    /// The config gate: an unnamed `done_state` plans nothing, and neither does a named one on an
    /// installation whose review path is not ticketless.
    #[test]
    fn an_unconfigured_transition_plans_nothing() {
        for teams in [
            ticketless_done(""),
            Teams {
                review: Review {
                    mode: ReviewMode::Tickets,
                    done_state: "Done".to_string(),
                    ..Review::default()
                },
                enabled: true,
                ..Teams::disabled()
            },
            Teams {
                review: Review {
                    mode: ReviewMode::Ticketless,
                    done_state: "Done".to_string(),
                    ..Review::default()
                },
                enabled: false,
                ..Teams::disabled()
            },
        ] {
            let o = orch(teams.clone());
            run_of(&o, "STUDIO-712", "ID-712", "TEAM-1");
            assert_eq!(
                o.plan_review_done(&[row(64, "handoff:STUDIO-712")], &coord(64)),
                None,
                "{teams:?}"
            );
        }

        // …and a daemon with no Teams runtime at all.
        let mut o = orch(ticketless_done("Done"));
        o.teams = None;
        run_of(&o, "STUDIO-712", "ID-712", "TEAM-1");
        assert_eq!(
            o.plan_review_done(&[row(64, "handoff:STUDIO-712")], &coord(64)),
            None
        );
    }

    /// The scope guard: only a ticket the daemon's own handoff parked is in scope. An
    /// operator-introduced pull request names no ticket, so its merge moves nothing.
    #[test]
    fn only_a_handoff_introduced_row_names_a_ticket() {
        for origin in [
            format!("{REVIEW_ORIGIN_CONSOLE}:operator"),
            "handoff:".to_string(),
            "handoff".to_string(),
            "handoffs:STUDIO-712".to_string(),
            String::new(),
        ] {
            let o = orch(ticketless_done("Done"));
            run_of(&o, "STUDIO-712", "ID-712", "TEAM-1");
            assert_eq!(
                o.plan_review_done(&[row(64, &origin)], &coord(64)),
                None,
                "origin {origin:?}"
            );
        }
    }

    /// Rows of OTHER pull requests are not this pull request's ticket. The tick's snapshot holds
    /// every watched row, so the coordinate filter is what keeps one merge from moving another
    /// ticket.
    #[test]
    fn a_merge_reads_only_its_own_pull_requests_rows() {
        let o = orch(ticketless_done("Done"));
        run_of(&o, "STUDIO-712", "ID-712", "TEAM-1");
        run_of(&o, "STUDIO-999", "ID-999", "TEAM-1");

        let rows = [row(11, "handoff:STUDIO-999"), row(64, "handoff:STUDIO-712")];
        let plan = o.plan_review_done(&rows, &coord(64)).expect("a plan");
        assert_eq!(plan.identifier, "STUDIO-712");
        assert_eq!(o.plan_review_done(&rows, &coord(12)), None);
    }

    /// An N-reviewer pull request has N watch rows naming the SAME ticket, and its merge must
    /// finish that ticket ONCE. `retire_review_pr` has the sibling property (it drops every
    /// reviewer's row); the difference is that here N moves would be N tracker writes for one
    /// event, so the collapse is pinned rather than left to the shape of an iterator call.
    #[test]
    fn an_n_reviewer_pull_request_finishes_its_ticket_once() {
        let o = orch(ticketless_done("Done"));
        run_of(&o, "STUDIO-712", "ID-712", "TEAM-1");

        let mut carol = row(64, "handoff:STUDIO-712");
        carol.key.reviewer = "carol".to_string();
        let rows = [row(64, "handoff:STUDIO-712"), carol];

        // `plan_review_done` answers at most one plan by signature; the assertion that matters is
        // that it is the shared ticket's, not one plan per row.
        let plan = o.plan_review_done(&rows, &coord(64)).expect("a plan");
        assert_eq!(plan.identifier, "STUDIO-712");
        assert_eq!(
            rows.iter()
                .filter_map(|r| handoff_ticket(&r.introduced_by))
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            1,
            "both rows name one ticket, so one move is the whole of the work"
        );
    }

    /// A retried ticket has several runs; the LATEST one is read, which is what the daemon's own
    /// most recent view of the ticket's tracker identity is.
    #[test]
    fn a_retried_ticket_is_read_off_its_newest_run() {
        let o = orch(ticketless_done("Done"));
        for (issue_id, started) in [
            ("ID-old", "2026-09-01T00:00:00Z"),
            ("ID-new", "2026-09-09T00:00:00Z"),
        ] {
            o.store()
                .start_run(RunStart {
                    issue_id: issue_id.to_string(),
                    issue_identifier: "STUDIO-712".to_string(),
                    team_id: "TEAM-1".to_string(),
                    started_at: started.to_string(),
                    ..RunStart::default()
                })
                .expect("start run");
        }

        let plan = o
            .plan_review_done(&[row(64, "handoff:STUDIO-712")], &coord(64))
            .expect("a plan");
        assert_eq!(plan.issue_id, "ID-new");
    }

    /// A ticket whose runs have aged out of history cannot be addressed — `move_issue_state`
    /// requires the opaque ids — so the transition declines rather than firing a call it knows
    /// will fail.
    #[test]
    fn a_ticket_with_no_run_left_in_history_plans_nothing() {
        let o = orch(ticketless_done("Done"));
        assert_eq!(
            o.plan_review_done(&[row(64, "handoff:STUDIO-712")], &coord(64)),
            None
        );

        // Present, but without the ids the move needs.
        run_of(&o, "STUDIO-712", "", "TEAM-1");
        assert_eq!(
            o.plan_review_done(&[row(64, "handoff:STUDIO-712")], &coord(64)),
            None
        );
        let o = orch(ticketless_done("Done"));
        run_of(&o, "STUDIO-712", "ID-712", "");
        assert_eq!(
            o.plan_review_done(&[row(64, "handoff:STUDIO-712")], &coord(64)),
            None
        );
    }

    /// The move itself, over the production chain the watcher task runs: `ControlWatchSink::finish`
    /// → [`ControlHandle::finish_review_ticket`] → `Tracker::move_issue_state`. Through the SINK
    /// rather than the handle directly, because the sink is the whole of what the task holds.
    ///
    /// The argument order is the point. A plan carries both a human identifier and an opaque issue
    /// id, only one of them addresses the tracker, and a test that stops at a fake sink — proving
    /// the task called `finish` — cannot tell them apart. Passing `identifier` where `issue_id`
    /// belongs would send Linear `MoveIssueState("STUDIO-712", …)`, which it rejects, so every
    /// merge would log a failed move and every ticket would stay in review: the feature dead in
    /// exactly the way it exists to fix. This is the assertion that says so.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_merged_pull_requests_ticket_is_moved_by_its_opaque_ids() {
        let tr = Arc::new(Fake::new());
        let o = orch_tracked(ticketless_done("Done"), Arc::clone(&tr));
        run_of(&o, "STUDIO-712", "ID-712", "TEAM-1");
        let plan = o
            .plan_review_done(&[row(64, "handoff:STUDIO-712")], &coord(64))
            .expect("a plan");

        ControlWatchSink::new(o.control()).finish(plan).await;

        let calls = tr.move_calls();
        assert_eq!(calls.len(), 1, "move_calls = {calls:?}");
        assert_eq!(
            (
                calls[0].issue_id.as_str(),
                calls[0].team_id.as_str(),
                calls[0].state_name.as_str()
            ),
            ("ID-712", "TEAM-1", "Done"),
            "the opaque ids off the run row and the configured state, in that order"
        );
        assert!(
            tr.move_to_type_calls().is_empty(),
            "auto-done moves by NAME to the configured state, not by type"
        );
    }

    /// The failure contract the doc comment promises: a tracker that REFUSES the move is logged and
    /// dropped — [`ControlHandle::finish_review_ticket`] returns `()`, so there is no error to
    /// swallow and nothing to retry against, and the ticket stays in review, which is where it sat
    /// before this feature existed. The call is still recorded, so a ticket left in review is the
    /// tracker's answer rather than a move this daemon quietly skipped.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_refused_move_is_logged_and_dropped() {
        let mut fake = Fake::new();
        fake.move_err = Some(TrackerError::Other("linear_move_rejected: nope".into()));
        let tr = Arc::new(fake);
        let o = orch_tracked(ticketless_done("Done"), Arc::clone(&tr));
        run_of(&o, "STUDIO-712", "ID-712", "TEAM-1");
        let plan = o
            .plan_review_done(&[row(64, "handoff:STUDIO-712")], &coord(64))
            .expect("a plan");

        // Returns rather than panicking or propagating — the whole of the contract.
        ControlWatchSink::new(o.control()).finish(plan).await;

        assert_eq!(
            tr.move_calls().len(),
            1,
            "the move was attempted; the tracker is what refused it"
        );
    }

    /// The origin parser, over the spellings the two writers produce and the near-misses.
    #[test]
    fn handoff_ticket_reads_the_handoff_origin_only() {
        assert_eq!(handoff_ticket("handoff:STUDIO-712"), Some("STUDIO-712"));
        assert_eq!(handoff_ticket("handoff: STUDIO-712 "), Some("STUDIO-712"));
        for other in [
            "console:operator",
            "handoff",
            "handoff:",
            "handoff:   ",
            "HANDOFF:STUDIO-712",
            "",
        ] {
            assert_eq!(handoff_ticket(other), None, "{other:?}");
        }
    }
}
