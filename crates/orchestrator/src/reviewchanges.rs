//! reviewchanges — routing a ticket BACK out of the review state when its review round files
//! findings (STUDIO-839).
//!
//! **No Go v0.4.0 counterpart, and a deliberate DIVERGENCE rather than new surface**, exactly as
//! [`crate::reviewdone`] is: the frozen reference never moves a ticket into a state at all except
//! through the handoff, and `README.md`'s Divergences section carries the entry.
//!
//! # The gap this closes
//!
//! The ticketless review loop closed itself on the RUN side and not on the TRACKER side.
//! [`crate::reviewnotify`] posts a token-bearing completion comment when a review leaves findings,
//! and that comment reopens the author's run through
//! [`crate::ghsummons::SummonSource`] → [`crate::ghenrich::apply_github_summons`]. Nothing moved
//! the ticket. So a ticket whose review came back with findings sat in the review state while its
//! author was being re-engaged and was actively pushing commits, and the review state stopped
//! distinguishing three situations: waiting for a reviewer, being reviewed, and
//! reviewed-with-findings. A state that means three things means none of them.
//!
//! # The pairing, which is the whole of the decision
//!
//! Findings ⇒ the ticket moves. **Approved ⇒ it does not.** Approval is the pause in the re-review
//! loop (design §15-c) and the tokenless completion is already the same decision on the author's
//! side of it, so moving an approved ticket would contradict a decision the daemon has already
//! taken one branch earlier. [`Orchestrator::plan_review_changes`] refuses the approved arm, and
//! [`crate::reviewnotify::run_review_notify_task`] refuses it AGAIN at the point of action — a
//! guard rather than an assertion, because a refactor that carried an approved plan this far must
//! not be able to perform it.
//!
//! # What it may move, and what it may not
//!
//! The scope guard is [`crate::reviewdone::origin_ticket`]'s, shared rather than re-derived: only
//! a ticket THIS DAEMON parked, named by the `handoff:<identifier>` or `adopt:<identifier>` origin
//! recorded on the review run. Sharing is the point, so the widening STUDIO-838 gave that guard is
//! inherited here rather than re-litigated: an adopted pull request's ticket was parked by this
//! daemon too, and a findings verdict on it routes back exactly as a handoff's does. An
//! operator-introduced pull request (`console:…`) names an operator rather than a ticket, so it
//! moves none.
//!
//! **And only while the pull request is still open.** That is the guard against
//! [`crate::reviewdone`], the other writer of ticket state off a review outcome: a findings round
//! that exits after its pull request has already MERGED would otherwise pull a finished ticket back
//! out of its terminal state, which is the one way the two transitions can fight. The watch row's
//! `open` flag is the daemon's own answer to "is this still live", the same flag the watcher
//! retires a merged row on, so the two read one fact rather than two.
//!
//! A residual, stated rather than hidden: both decisions are made on the control task but both
//! WRITES happen on their own off-loop tasks, so a merge observed in the window between this
//! module's plan and its write can still land the two moves in either order. The window is one
//! review exit wide and needs the merge to fall inside it; the cost when it loses is a ticket in
//! the changes state after a merge, which the next merge sweep does not re-correct. Narrowing it
//! further would mean serializing two tracker writes that are deliberately off the loop.
//!
//! # Off the loop
//!
//! The decision is made on the control task, where the store and the watch set are single-writer;
//! the tracker write is a Linear round-trip and happens on [`crate::reviewnotify`]'s task, through
//! the [`ReviewChangesSink`] seam. It rides the notification task rather than a task of its own
//! because it is the SAME event — one review exit produces one comment and, when findings, one move
//! — and because the comment must be posted FIRST: the comment is what re-engages the author, and a
//! ticket moved out of review before the summons exists is the very disagreement this module has to
//! report rather than create.

use rhapsody_store::RunFilter;

use crate::orchestrator::Orchestrator;
use crate::review::ReviewRun;
use crate::reviewdone::origin_ticket;
use crate::stop::ControlHandle;

/// One findings verdict's implementation ticket, and the state it is going back to.
///
/// Resolved BEFORE it leaves the control task — the identifier off the run's recorded origin, the
/// opaque ids off the run row — so the off-loop half makes one call and has no decision left to get
/// wrong. [`crate::reviewdone::ReviewDonePlan`]'s shape, deliberately: the two transitions differ in
/// their edge and their state, in nothing else, and a reviewer should be able to read one against
/// the other.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReviewChangesPlan {
    /// `owner/repo#number` — the pull request whose review filed the findings, for the log line.
    pub pr: String,
    /// The tracker's opaque issue id.
    pub issue_id: String,
    /// The tracker team the state name is resolved within.
    pub team_id: String,
    /// The human ticket id, e.g. `STUDIO-839`.
    pub identifier: String,
    /// The state NAME, from `teams.review.changes_state`.
    pub state: String,
}

/// Performs one findings route-back, off the control task.
///
/// A trait for [`crate::reviewwatch::ReviewWatchSink`]'s reason: the notification task must be
/// testable without a control loop, and the seam is what lets a test assert on the move that was
/// asked for rather than on a side effect two hops away.
#[async_trait::async_trait]
pub trait ReviewChangesSink: Send + Sync {
    /// Moves ONE ticket out of the review state.
    ///
    /// `re_engaged` is what the notification task observed of the OTHER consequence of this same
    /// verdict: the completion comment was posted and carries the summon token. It is not a gate —
    /// the move happens either way, because a ticket whose review filed findings is not waiting for
    /// a reviewer whether or not the author's run reopened — it is what makes the disagreement
    /// visible instead of silent.
    ///
    /// Infallible by contract, like [`crate::reviewwatch::ReviewWatchSink::finish`]: a failed move
    /// is logged where it happens and the ticket stays in review, which is exactly where it was
    /// before this feature existed.
    async fn route_back(&self, plan: ReviewChangesPlan, re_engaged: bool);
}

/// The production [`ReviewChangesSink`]: the control handle's own tracker, through the same seam
/// every other off-loop→tracker write uses.
pub struct ControlChangesSink {
    control: ControlHandle,
}

impl ControlChangesSink {
    pub fn new(control: ControlHandle) -> ControlChangesSink {
        ControlChangesSink { control }
    }
}

#[async_trait::async_trait]
impl ReviewChangesSink for ControlChangesSink {
    async fn route_back(&self, plan: ReviewChangesPlan, re_engaged: bool) {
        self.control
            .route_back_review_ticket(plan, re_engaged)
            .await
    }
}

impl Orchestrator {
    /// Resolves the ticket a findings verdict should route back out of the review state, and the
    /// state it is going to — or `None` when this daemon must not, or cannot, move one.
    ///
    /// Runs ON the control task at [`Orchestrator::on_review_exit`], the moment the daemon knows
    /// both that the round is over and what its verdict was, beside
    /// [`Orchestrator::plan_review_notify`] and off the same two facts.
    ///
    /// `None` covers six situations, all quiet by design except the three that point at a real
    /// problem:
    ///
    /// * the round was APPROVED — the pairing's other arm, and the common case;
    /// * the transition is not configured (or Teams / ticketless review is off) — the default;
    /// * the review was introduced by neither a handoff nor an adoption, so no ticket is in scope;
    /// * the pull request is no longer open (or its row is gone) — warned, because a findings
    ///   verdict arriving after a merge is the one way this can fight [`crate::reviewdone`];
    /// * no run row survives for that identifier — warned: the ticket is real but the daemon has
    ///   no opaque id to address the tracker with, and history retention is the usual reason;
    /// * the run row carries no issue or team id — warned, for the same reason.
    pub(crate) fn plan_review_changes(
        &self,
        run: &ReviewRun,
        approved: bool,
    ) -> Option<ReviewChangesPlan> {
        if approved {
            return None;
        }
        let state = self.teams.as_ref()?.review_changes_state()?.to_string();
        let identifier = origin_ticket(&run.introduced_by)?.to_string();
        let pr = format!("{}/{}#{}", run.owner, run.repo, run.number);
        // The anti-race guard against `reviewdone`: a merged pull request's rows are retired, so an
        // absent-or-closed row means the ticket has either already been finished or is about to be,
        // and pulling it back out of a terminal state is the one wrong move available here.
        match self.store().get_review_watch(&run.watch_key()) {
            Ok(Some(row)) if row.open => {}
            Ok(_) => {
                tracing::warn!(pr = %pr, issue_identifier = %identifier, "review route-back: the pull request is no longer open, so its ticket was not moved out of review");
                return None;
            }
            Err(e) => {
                tracing::warn!(pr = %pr, issue_identifier = %identifier, err = %e, "review route-back: the watch row could not be read; the ticket was not moved out of review");
                return None;
            }
        }
        // The opaque tracker ids the move needs live on the run that produced the pull request. The
        // LATEST run of that ticket, because a retried ticket has several and they all carry the
        // same issue and team — `list_issue_runs` returns one row per identifier, newest first.
        let runs = match self.store().list_issue_runs(RunFilter {
            issue: identifier.clone(),
            limit: 1,
            ..RunFilter::default()
        }) {
            Ok(runs) => runs,
            Err(e) => {
                tracing::warn!(pr = %pr, issue_identifier = %identifier, err = %e, "review route-back: the run history could not be read; the ticket was not moved out of review");
                return None;
            }
        };
        let Some(r) = runs.into_iter().next() else {
            tracing::warn!(pr = %pr, issue_identifier = %identifier, "review route-back: no run of this ticket is left in history, so the tracker cannot be addressed; the ticket was not moved out of review");
            return None;
        };
        if r.issue_id.is_empty() || r.team_id.is_empty() {
            tracing::warn!(pr = %pr, issue_identifier = %identifier, "review route-back: the run row carries no tracker issue/team id; the ticket was not moved out of review");
            return None;
        }
        Some(ReviewChangesPlan {
            pr,
            issue_id: r.issue_id,
            team_id: r.team_id,
            identifier,
            state,
        })
    }
}

impl ControlHandle {
    /// Moves one findings verdict's ticket out of the review state, off the control loop — the same
    /// by-NAME `MoveIssueState` [`ControlHandle::handoff_run`] and
    /// [`ControlHandle::finish_review_ticket`] use, resolving the tracker the same way.
    ///
    /// A failure is logged and dropped rather than returned to a caller who has nothing to do with
    /// it: the round is over, there is no second edge to retry against, and the ticket stays in
    /// review — exactly where it was before this feature existed.
    ///
    /// **The disagreement is the log line's job.** A findings verdict has two consequences and they
    /// can come apart: the state move is this daemon's own write and always happens, while the run
    /// re-engagement additionally needs the completion comment to have been posted with its token
    /// AND the pull request to be among the ticket's `linked_prs` in the poller's snapshot — the
    /// tracker's business, which on an installation whose Linear carries no GitHub attachments is
    /// empty (STUDIO-674). So the daemon says which half it knows: `re_engaged` false is a WARNING
    /// that names a ticket nothing will reopen, and `re_engaged` true still names the remaining
    /// condition rather than promising a run. A ticket sitting in the changes state with no run is
    /// then traceable to one line naming both the ticket and the pull request.
    pub(crate) async fn route_back_review_ticket(&self, plan: ReviewChangesPlan, re_engaged: bool) {
        if let Err(e) = self
            .move_issue_state(&plan.issue_id, &plan.team_id, &plan.state)
            .await
        {
            tracing::warn!(
                pr = %plan.pr,
                issue_identifier = %plan.identifier,
                state = %plan.state,
                err = %e,
                "review route-back: the state move failed; the ticket stays in review"
            );
            return;
        }
        if re_engaged {
            tracing::info!(
                pr = %plan.pr,
                issue_identifier = %plan.identifier,
                state = %plan.state,
                "review route-back: the review filed findings, so its ticket was moved out of review; the author's run reopens when the poller sees this pull request among the ticket's linked pull requests"
            );
        } else {
            tracing::warn!(
                pr = %plan.pr,
                issue_identifier = %plan.identifier,
                state = %plan.state,
                "review route-back: the ticket was moved out of review but its completion comment carries no summons, so no run is being re-engaged for it"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rhapsody_config::teams::{Review, ReviewMode, Teams};
    use rhapsody_store::{REVIEW_STATUS_IN_FLIGHT, ReviewWatchRow, RunStart, Sqlite, StorePath};
    use rhapsody_tracker::TrackerError;
    use rhapsody_tracker::fake::Fake;

    use super::*;
    use crate::reviewintro::{REVIEW_ORIGIN_ADOPT, REVIEW_ORIGIN_CONSOLE};
    use crate::testsupport::{empty_effective, set_of};

    const OWNER: &str = "makewhatis";
    const REPO: &str = "rhapsody";
    const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    /// Teams on, ticketless review, `changes_state` named — everything the transition gates on.
    fn ticketless_changes(state: &str) -> Teams {
        Teams {
            enabled: true,
            review: Review {
                mode: ReviewMode::Ticketless,
                changes_state: state.to_string(),
                ..Review::default()
            },
            ..Teams::disabled()
        }
    }

    fn orch(teams: Teams) -> Orchestrator {
        orch_tracked(teams, Arc::new(Fake::new()))
    }

    fn orch_tracked(teams: Teams, tracker: Arc<Fake>) -> Orchestrator {
        let mut eff = empty_effective(tracker);
        eff.review_states = set_of(&["in review"]);
        let mut o = Orchestrator::new("WORKFLOW.md");
        o.eff = Some(eff);
        o.teams = Some(teams);
        o.set_store(Arc::new(
            Sqlite::open(StorePath::InMemory).expect("open in-memory store"),
        ));
        o
    }

    fn review(introduced_by: &str) -> ReviewRun {
        ReviewRun {
            owner: OWNER.to_string(),
            repo: REPO.to_string(),
            number: 64,
            reviewer: "bob".to_string(),
            author: "alice".to_string(),
            team_id: "TEAM-1".to_string(),
            repo_url: "git@github.com:makewhatis/rhapsody.git".to_string(),
            head_sha: HEAD.to_string(),
            introduced_by: introduced_by.to_string(),
        }
    }

    /// The watch row the review run is the dispatch of — the `open` flag the anti-race guard reads.
    fn watched(o: &Orchestrator, run: &ReviewRun, open: bool) {
        o.store()
            .save_review_watch(ReviewWatchRow {
                key: run.watch_key(),
                author: run.author.clone(),
                introduced_by: run.introduced_by.clone(),
                requested_sha: run.head_sha.clone(),
                last_reviewed_sha: String::new(),
                status: REVIEW_STATUS_IN_FLIGHT.to_string(),
                open,
            })
            .expect("save review watch");
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

    /// An orchestrator with everything the happy path needs already in place.
    fn ready(state: &str) -> (Orchestrator, ReviewRun) {
        let o = orch(ticketless_changes(state));
        let run = review("handoff:STUDIO-839");
        watched(&o, &run, true);
        run_of(&o, "STUDIO-839", "ID-839", "TEAM-1");
        (o, run)
    }

    // ── the pairing: findings ⇒ moved, approved ⇒ not moved ──────────────────────────────────────

    /// Arm one of THE acceptance criterion. A review that files findings resolves a move out of the
    /// review state, carrying the opaque ids the tracker needs and the configured state name.
    #[test]
    fn a_findings_round_plans_its_tickets_route_back() {
        let (o, run) = ready("In Progress");
        assert_eq!(
            o.plan_review_changes(&run, false).expect("a plan"),
            ReviewChangesPlan {
                pr: "makewhatis/rhapsody#64".to_string(),
                issue_id: "ID-839".to_string(),
                team_id: "TEAM-1".to_string(),
                identifier: "STUDIO-839".to_string(),
                state: "In Progress".to_string(),
            }
        );
    }

    /// Arm two, pinned SEPARATELY so that inverting the branch reds both rather than one: approval
    /// is the pause in the re-review loop (design §15-c) and moves nothing. Everything else about
    /// this daemon is identical to the arm above — same config, same row, same run history — so the
    /// verdict is the only thing that can account for the difference.
    #[test]
    fn an_approved_round_plans_no_route_back() {
        let (o, run) = ready("In Progress");
        assert_eq!(o.plan_review_changes(&run, true), None);
    }

    /// The two arms, side by side, over one orchestrator — the pairing as a single property rather
    /// than as two tests that could drift apart.
    #[test]
    fn the_verdict_is_the_whole_of_the_route_back_decision() {
        let (o, run) = ready("In Progress");
        assert_eq!(
            [false, true].map(|approved| o.plan_review_changes(&run, approved).is_some()),
            [true, false],
            "findings move the ticket; approval leaves it exactly where it is"
        );
    }

    // ── empty configuration is off ───────────────────────────────────────────────────────────────

    /// The byte-identical-when-unconfigured property, which is what makes this safe to ship: an
    /// installation that has not named a state behaves exactly as it did before — and so does one
    /// on any other review path, or with Teams off, or with no Teams runtime at all.
    #[test]
    fn an_unconfigured_transition_plans_nothing() {
        for teams in [
            ticketless_changes(""),
            ticketless_changes("   "),
            Teams {
                enabled: true,
                review: Review {
                    mode: ReviewMode::Tickets,
                    changes_state: "In Progress".to_string(),
                    ..Review::default()
                },
                ..Teams::disabled()
            },
            Teams {
                enabled: true,
                review: Review {
                    mode: ReviewMode::Off,
                    changes_state: "In Progress".to_string(),
                    ..Review::default()
                },
                ..Teams::disabled()
            },
            Teams {
                enabled: false,
                review: Review {
                    mode: ReviewMode::Ticketless,
                    changes_state: "In Progress".to_string(),
                    ..Review::default()
                },
                ..Teams::disabled()
            },
        ] {
            let o = orch(teams.clone());
            let run = review("handoff:STUDIO-839");
            watched(&o, &run, true);
            run_of(&o, "STUDIO-839", "ID-839", "TEAM-1");
            assert_eq!(o.plan_review_changes(&run, false), None, "{teams:?}");
        }

        // …and a daemon with no Teams runtime at all.
        let (mut o, run) = ready("In Progress");
        o.teams = None;
        assert_eq!(o.plan_review_changes(&run, false), None);
    }

    // ── scope ────────────────────────────────────────────────────────────────────────────────────

    /// An ADOPTED review names its ticket exactly as a handoff-introduced one does, so a findings
    /// verdict on a pull request the repair sweep introduced routes its ticket back out of the
    /// review state (STUDIO-838's widening of [`origin_ticket`], inherited here rather than
    /// re-derived).
    ///
    /// Pinned in THIS module and not only in [`crate::reviewdone`]: the guard is shared, and a
    /// sibling's tests protect "the helper is wide", never "this module shares it". Narrowing the
    /// guard back to handoff-only at [`Orchestrator::plan_review_changes`]'s call site leaves the
    /// rest of the workspace green, so without this test an adopted pull request's findings verdict
    /// could silently move no ticket at all.
    ///
    /// The scope guard this module is built on is "a ticket THIS DAEMON parked in a review state",
    /// and an adoption is that — resolved from the daemon's own run ledger and its own configured
    /// repository, through the same gates and recorded on the same ledger.
    #[test]
    fn an_adopted_review_names_its_ticket_exactly_as_a_handoff_does() {
        let o = orch(ticketless_changes("In Progress"));
        let run = review(&format!("{REVIEW_ORIGIN_ADOPT}:STUDIO-839"));
        watched(&o, &run, true);
        run_of(&o, "STUDIO-839", "ID-839", "TEAM-1");

        assert_eq!(
            o.plan_review_changes(&run, false).expect("a plan"),
            ReviewChangesPlan {
                pr: "makewhatis/rhapsody#64".to_string(),
                issue_id: "ID-839".to_string(),
                team_id: "TEAM-1".to_string(),
                identifier: "STUDIO-839".to_string(),
                state: "In Progress".to_string(),
            }
        );
    }

    /// The scope guard: only a ticket this daemon's own handoff or adoption parked is in scope,
    /// read off the recorded origin rather than inferred. An operator-introduced pull request names
    /// an OPERATOR rather than a ticket, so it moves none — and neither does an origin that merely
    /// looks like one of the two prefixes.
    #[test]
    fn only_a_handoff_or_adoption_introduced_review_names_a_ticket() {
        for origin in [
            format!("{REVIEW_ORIGIN_CONSOLE}:operator"),
            "handoff:".to_string(),
            "handoff".to_string(),
            "handoffs:STUDIO-839".to_string(),
            format!("{REVIEW_ORIGIN_ADOPT}:"),
            REVIEW_ORIGIN_ADOPT.to_string(),
            "adopts:STUDIO-839".to_string(),
            String::new(),
        ] {
            let o = orch(ticketless_changes("In Progress"));
            let run = review(&origin);
            watched(&o, &run, true);
            run_of(&o, "STUDIO-839", "ID-839", "TEAM-1");
            assert_eq!(
                o.plan_review_changes(&run, false),
                None,
                "origin {origin:?}"
            );
        }
    }

    /// The guard against [`crate::reviewdone`], the other writer of ticket state off a review
    /// outcome. A merged pull request's rows are retired, so a findings round that exits after the
    /// merge must not pull a finished ticket back out of its terminal state. Both shapes of "no
    /// longer live" are refused: a closed row, and a row that is simply gone.
    #[test]
    fn a_pull_request_that_is_no_longer_open_moves_nothing() {
        let o = orch(ticketless_changes("In Progress"));
        let run = review("handoff:STUDIO-839");
        run_of(&o, "STUDIO-839", "ID-839", "TEAM-1");
        assert_eq!(
            o.plan_review_changes(&run, false),
            None,
            "a pull request with no watch row at all is not live"
        );

        watched(&o, &run, false);
        assert_eq!(
            o.plan_review_changes(&run, false),
            None,
            "a retired (merged or closed) pull request's ticket is not this transition's to move"
        );

        watched(&o, &run, true);
        assert!(
            o.plan_review_changes(&run, false).is_some(),
            "and the same daemon DOES move it while the pull request is open"
        );
    }

    /// A ticket whose runs have aged out of history cannot be addressed — `MoveIssueState` needs
    /// the opaque ids — so the daemon declines and warns rather than firing a call it knows will
    /// fail. Same for a run row that carries no ids.
    #[test]
    fn a_ticket_the_tracker_cannot_be_addressed_for_moves_nothing() {
        let o = orch(ticketless_changes("In Progress"));
        let run = review("handoff:STUDIO-839");
        watched(&o, &run, true);
        assert_eq!(
            o.plan_review_changes(&run, false),
            None,
            "no run of this ticket is left in history"
        );

        run_of(&o, "STUDIO-839", "", "TEAM-1");
        assert_eq!(
            o.plan_review_changes(&run, false),
            None,
            "a run row with no opaque issue id addresses nothing"
        );
    }

    // ── the off-loop write ───────────────────────────────────────────────────────────────────────

    /// The argument order is the point, exactly as it is for auto-Done: a plan carries both a human
    /// identifier and an opaque issue id, and passing the identifier where the id belongs would send
    /// Linear `MoveIssueState("STUDIO-839", …)`, which it rejects — so every findings round would
    /// log a failed move and every ticket would stay in review, the feature dead in exactly the way
    /// it exists to fix.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_findings_verdicts_ticket_is_moved_by_its_opaque_ids() {
        let tr = Arc::new(Fake::new());
        let o = orch_tracked(ticketless_changes("In Progress"), Arc::clone(&tr));
        let run = review("handoff:STUDIO-839");
        watched(&o, &run, true);
        run_of(&o, "STUDIO-839", "ID-839", "TEAM-1");
        let plan = o.plan_review_changes(&run, false).expect("a plan");

        ControlChangesSink::new(o.control())
            .route_back(plan, true)
            .await;

        let calls = tr.move_calls();
        assert_eq!(calls.len(), 1, "move_calls = {calls:?}");
        assert_eq!(
            (
                calls[0].issue_id.as_str(),
                calls[0].team_id.as_str(),
                calls[0].state_name.as_str()
            ),
            ("ID-839", "TEAM-1", "In Progress"),
            "the opaque ids off the run row and the configured state, in that order"
        );
        assert!(
            tr.move_to_type_calls().is_empty(),
            "the route-back moves by NAME to the configured state, not by type"
        );
    }

    /// The failure contract the doc comment promises: a tracker that REFUSES the move is logged and
    /// dropped, the ticket stays in review, and there is no error to swallow and nothing to retry
    /// against.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_refused_move_is_logged_and_dropped() {
        let mut fake = Fake::new();
        fake.move_err = Some(TrackerError::Other("linear_move_rejected: nope".into()));
        let tr = Arc::new(fake);
        let o = orch_tracked(ticketless_changes("In Progress"), Arc::clone(&tr));
        let run = review("handoff:STUDIO-839");
        watched(&o, &run, true);
        run_of(&o, "STUDIO-839", "ID-839", "TEAM-1");
        let plan = o.plan_review_changes(&run, false).expect("a plan");

        // Returns rather than panicking or propagating — the whole of the contract.
        ControlChangesSink::new(o.control())
            .route_back(plan, true)
            .await;

        assert_eq!(
            tr.move_calls().len(),
            1,
            "the move was attempted; the tracker is what refused it"
        );
    }

    // ── the two consequences can disagree, and that is visible ───────────────────────────────────

    /// Runs one route-back under a recording subscriber and returns the captured events, warming
    /// the callsites with a throwaway pass first so a sibling test cannot pin them
    /// `Interest::never` (TRA-243) — `ghenrich`'s `captured` helper, for its reason.
    async fn captured(re_engaged: bool) -> Vec<crate::testsupport::CapturedEvent> {
        let _serial = crate::testsupport::TRACING_TEST_LOCK.lock().await;
        let (events, subscriber) = crate::testsupport::recording_subscriber();
        let guard = tracing::subscriber::set_default(subscriber);
        let once = || async {
            let o = orch(ticketless_changes("In Progress"));
            let run = review("handoff:STUDIO-839");
            watched(&o, &run, true);
            run_of(&o, "STUDIO-839", "ID-839", "TEAM-1");
            let plan = o.plan_review_changes(&run, false).expect("a plan");
            ControlChangesSink::new(o.control())
                .route_back(plan, re_engaged)
                .await;
        };
        once().await; // warm-up: force every callsite to register
        tracing::callsite::rebuild_interest_cache();
        events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        once().await;
        drop(guard);
        events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// A state move whose re-engagement will NOT happen is a WARNING that names the ticket and the
    /// pull request, never a silent success. The daemon knows one half for certain — whether the
    /// completion comment was posted carrying its token — so that half is reported as fact.
    #[tokio::test]
    async fn a_move_with_no_summons_warns_and_names_both_halves() {
        let events = captured(false).await;
        let warned = events
            .iter()
            .find(|e| e.message.contains("carries no summons"))
            .unwrap_or_else(|| panic!("no disagreement warning in {events:?}"));
        assert_eq!(warned.level, "WARN");
        assert_eq!(
            warned.fields.get("issue_identifier").map(String::as_str),
            Some("STUDIO-839")
        );
        assert_eq!(
            warned.fields.get("pr").map(String::as_str),
            Some("makewhatis/rhapsody#64")
        );
    }

    /// And when the comment DID summon, the daemon still names the condition it cannot see — the
    /// pull request being among the ticket's `linked_prs` in the poller's snapshot — rather than
    /// reporting a run it has not observed.
    #[tokio::test]
    async fn a_move_with_a_summons_still_names_the_condition_it_cannot_see() {
        let events = captured(true).await;
        let said = events
            .iter()
            .find(|e| e.message.contains("moved out of review"))
            .unwrap_or_else(|| panic!("no route-back line in {events:?}"));
        assert_eq!(said.level, "INFO");
        assert!(
            said.message.contains("linked pull requests"),
            "the remaining condition must be named rather than a run promised: {}",
            said.message
        );
    }
}
