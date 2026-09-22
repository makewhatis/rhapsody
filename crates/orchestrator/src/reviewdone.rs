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

use chrono::{DateTime, Utc};
use rhapsody_store::{ReviewDoneRow, RunFilter};

use crate::orchestrator::Orchestrator;
use crate::prstate::PrCoord;
use crate::reviewintro::{REVIEW_ORIGIN_ADOPT, REVIEW_ORIGIN_HANDOFF};
use crate::stop::ControlHandle;

/// How many times ONE merged pull request's terminal move is attempted before it is given up on
/// and left to the reconciliation sweep to report (STUDIO-1007).
///
/// Three, because the failure it exists for is a single tracker blip the next attempt rides out,
/// and because each attempt after the first is separated by seconds-to-minutes of backoff rather
/// than a hot loop — the whole budget is about ten minutes (see
/// [`REVIEW_DONE_RETRY_DELAYS_SECS`]). A tracker that is down for longer than that is a fact an
/// operator needs to see, not a reason to retry forever; that is what the report is for.
pub const REVIEW_DONE_ATTEMPTS: u32 = 3;

/// The delay before each attempt, in seconds, indexed by the number of attempts ALREADY made: a
/// failure of attempt 1 waits `[1]`, a failure of attempt 2 waits `[2]`, and attempt 3 is the last.
///
/// So the three attempts land at roughly +0s, +150s and +600s — about ten minutes end to end,
/// which is the bound the ticket names. A fixed table rather than [`failure_backoff_ms`](crate::backoff::failure_backoff_ms)'s
/// doubling: that helper is sized for the retry queue's much longer horizon and would put the
/// third attempt hours out, well past the point where the operator's feed should already have the
/// divergence.
const REVIEW_DONE_RETRY_DELAYS_SECS: [i64; REVIEW_DONE_ATTEMPTS as usize] = [0, 150, 600];

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

/// The ticket named by a `handoff:<identifier>` or `adopt:<identifier>` origin, or `None` for any
/// other origin.
///
/// The one reader of those spellings besides the writers in [`crate::reviewintro`] and
/// [`crate::reviewadopt`], and the whole of this module's scope guard: a `console:` row (or a
/// future origin nobody has written yet) yields `None` and moves no ticket.
///
/// `pub` because the read side wants the same answer the write side acts on: the issue listing
/// joins it onto each `pr:` row so a review job says which ticket it is reviewing (STUDIO-834).
/// It is CALLED there rather than copied — a second implementation is how the `handoff:`-only
/// reading STUDIO-839 spent a round removing gets reintroduced, and an adopted review showing no
/// ticket is precisely the row STUDIO-834 exists to fix.
///
/// **Both ticket-bearing origins, not just the handoff** (STUDIO-838). The guard is "a ticket THIS
/// DAEMON parked in a review state", and an adoption is that — resolved from the daemon's own run
/// ledger and its own configured repository, through the same gates. A `console:` origin still
/// yields `None` for the reason it always did, which is not that it is untrusted: it names an
/// OPERATOR, so there is no ticket in it to move.
pub fn origin_ticket(introduced_by: &str) -> Option<&str> {
    let identifier = introduced_by
        .strip_prefix(REVIEW_ORIGIN_HANDOFF)
        .or_else(|| introduced_by.strip_prefix(REVIEW_ORIGIN_ADOPT))?
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
            .find_map(|row| origin_ticket(&row.introduced_by))?
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
    /// The merge is recorded DURABLY first (STUDIO-1007), before the move is attempted, because the
    /// row is the "this pull request merged" fact three other readers stand on: the handoff's
    /// terminal/merged guard, the bounded retry, and the reconciliation sweep. Writing it after a
    /// failed attempt would leave exactly the window this ticket exists to close — a merged pull
    /// request whose ticket a late handoff can move back into review.
    ///
    /// A failure no longer drops the transition (STUDIO-1004): the row keeps the attempts made and
    /// the next due time, and the watcher's [`ControlHandle::retry_pending_review_done`] carries it
    /// on until it lands or the attempt budget is spent.
    pub(crate) async fn finish_review_ticket(&self, plan: ReviewDonePlan) {
        self.record_review_done(&plan, 0, String::new(), false);
        self.advance_review_done(&plan, 1).await;
    }

    /// Retries every owed terminal move that is due (STUDIO-1007), off the control loop, on the
    /// review watcher's own tick.
    ///
    /// `now` is a parameter rather than read here so the schedule is a fact a test can drive in
    /// seconds instead of waiting out [`REVIEW_DONE_RETRY_DELAYS_SECS`]. A row that is `gave_up` is
    /// skipped deliberately — the budget is spent and the reconciliation sweep owns surfacing it —
    /// and a store that cannot be read warns and decides nothing, exactly as the sweep does.
    pub(crate) async fn retry_pending_review_done(&self, now: DateTime<Utc>) {
        let rows = match self.store.load_review_done() {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(
                    err = %e,
                    "auto-done: the owed terminal moves could not be read; this tick retries nothing"
                );
                return;
            }
        };
        for row in rows {
            if row.gave_up || !review_done_due(&row.next_at, now) {
                continue;
            }
            // `row.attempts` counts moves already made, so the next one is that plus one.
            let attempt = u32::try_from(row.attempts)
                .unwrap_or(u32::MAX)
                .saturating_add(1);
            let plan = ReviewDonePlan {
                pr: row.pr,
                issue_id: row.issue_id,
                team_id: row.team_id,
                identifier: row.identifier,
                state: row.state,
            };
            self.advance_review_done(&plan, attempt).await;
        }
    }

    /// Makes ONE terminal-move attempt for `plan` and records its outcome durably: the row is
    /// CLEARED when the move lands, and otherwise carries the attempt count and the next due time —
    /// or `gave_up` once [`REVIEW_DONE_ATTEMPTS`] is spent.
    ///
    /// The success and failure log lines are the incident's own (STUDIO-712, STUDIO-1004): the
    /// success names the pull request, the ticket and the state, and the failure names how many
    /// attempts have been made and whether this was the last, so the operator can tell a blip from
    /// an exhausted budget without reading the store.
    async fn advance_review_done(&self, plan: &ReviewDonePlan, attempt: u32) {
        match self
            .move_issue_state(&plan.issue_id, &plan.team_id, &plan.state)
            .await
        {
            Ok(()) => {
                tracing::info!(
                    pr = %plan.pr,
                    issue_identifier = %plan.identifier,
                    state = %plan.state,
                    "auto-done: the merged pull request's ticket was moved to its terminal state"
                );
                if let Err(e) = self.store.clear_review_done(&plan.identifier) {
                    // The ticket DID move; only the ledger write failed. Warn and keep going — a
                    // leftover row re-attempts a move the ticket is already in, which is a
                    // redundant write rather than a wrong one, and the sweep reports it as still
                    // owed only until the next attempt clears it.
                    tracing::warn!(
                        pr = %plan.pr,
                        issue_identifier = %plan.identifier,
                        err = %e,
                        "auto-done: the move landed but the owed-move row could not be cleared"
                    );
                }
            }
            Err(e) => {
                let exhausted = attempt >= REVIEW_DONE_ATTEMPTS;
                let next_at = if exhausted {
                    String::new()
                } else {
                    review_done_next_at(attempt, Utc::now())
                };
                tracing::warn!(
                    pr = %plan.pr,
                    issue_identifier = %plan.identifier,
                    state = %plan.state,
                    attempt,
                    attempts = REVIEW_DONE_ATTEMPTS,
                    exhausted,
                    err = %e,
                    "auto-done: the terminal-state move failed; the ticket stays in review and the \
                     move is retried until its budget is spent, after which the reconciliation \
                     sweep reports it"
                );
                self.record_review_done(plan, i64::from(attempt), next_at, exhausted);
            }
        }
    }

    /// Writes (or replaces) the durable owed-move row for `plan`. A store error is warned and
    /// otherwise ignored: the move itself must still be attempted, and persistence being off or
    /// broken is not a reason to skip a transition the ticket needs.
    fn record_review_done(
        &self,
        plan: &ReviewDonePlan,
        attempts: i64,
        next_at: String,
        gave_up: bool,
    ) {
        let row = ReviewDoneRow {
            identifier: plan.identifier.clone(),
            pr: plan.pr.clone(),
            issue_id: plan.issue_id.clone(),
            team_id: plan.team_id.clone(),
            state: plan.state.clone(),
            attempts,
            next_at,
            gave_up,
        };
        if let Err(e) = self.store.save_review_done(row) {
            tracing::warn!(
                pr = %plan.pr,
                issue_identifier = %plan.identifier,
                err = %e,
                "auto-done: the owed-move row could not be recorded; a restart would forget it"
            );
        }
    }
}

/// Whether an owed move's `next_at` is due at `now` — an empty stamp (the state the first attempt
/// leaves it in) is due immediately, and an unparseable one is due immediately too, because the
/// fail-safe direction for a retry is to TRY rather than to strand the move behind a malformed
/// timestamp.
fn review_done_due(next_at: &str, now: DateTime<Utc>) -> bool {
    if next_at.is_empty() {
        return true;
    }
    match DateTime::parse_from_rfc3339(next_at) {
        Ok(at) => at.with_timezone(&Utc) <= now,
        Err(_) => true,
    }
}

/// When attempt `attempt`'s failure schedules the next attempt: `now` plus the delay that attempt
/// number earned from [`REVIEW_DONE_RETRY_DELAYS_SECS`]. Indexed rather than read directly so a
/// future edit to either constant degrades to the shortest wait instead of panicking.
fn review_done_next_at(attempt: u32, now: DateTime<Utc>) -> String {
    let delay = REVIEW_DONE_RETRY_DELAYS_SECS
        .get(attempt as usize)
        .copied()
        .unwrap_or(0);
    rhapsody_store::format_summon_at(now + chrono::Duration::seconds(delay))
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
    use crate::reviewintro::{REVIEW_ORIGIN_ADOPT, REVIEW_ORIGIN_CONSOLE};
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

    /// An ADOPTED row names its ticket exactly as a handoff row does, so a merged pull request the
    /// repair sweep introduced still moves its implementation ticket to the terminal state
    /// (STUDIO-838).
    ///
    /// The alternative would punish the unlucky twice: a ticket whose handoff lost its introduction
    /// would get its review back and then never leave the review column, for no reason an operator
    /// could see. The scope guard this module is built on is "a ticket THIS DAEMON parked in a
    /// review state" — an adoption is that, from the daemon's own ledger and its own config.
    #[test]
    fn an_adopted_row_names_its_ticket_exactly_as_a_handoff_row_does() {
        let o = orch(ticketless_done("Done"));
        run_of(&o, "STUDIO-836", "ID-836", "TEAM-1");

        let plan = o
            .plan_review_done(
                &[row(144, &format!("{REVIEW_ORIGIN_ADOPT}:STUDIO-836"))],
                &coord(144),
            )
            .expect("an adopted row names a ticket");
        assert_eq!(plan.identifier, "STUDIO-836");
        assert_eq!(plan.issue_id, "ID-836");
        assert_eq!(plan.state, "Done");
    }

    /// The scope guard: only a ticket the daemon's own handoff or adoption parked is in scope. An
    /// operator-introduced pull request names no ticket, so its merge moves nothing.
    #[test]
    fn only_a_handoff_introduced_row_names_a_ticket() {
        for origin in [
            format!("{REVIEW_ORIGIN_CONSOLE}:operator"),
            "handoff:".to_string(),
            "handoff".to_string(),
            "handoffs:STUDIO-712".to_string(),
            format!("{REVIEW_ORIGIN_ADOPT}:"),
            REVIEW_ORIGIN_ADOPT.to_string(),
            "adopts:STUDIO-712".to_string(),
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
                .filter_map(|r| origin_ticket(&r.introduced_by))
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

    /// The failure contract STUDIO-1007 replaces "logged and dropped" with: a refused move is
    /// recorded durably (the merge fact the handoff guard and the sweep read) and its attempt is
    /// counted, so the retry knows where it was. The first attempt is all `finish` makes — the
    /// retries run on the watcher's tick.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_refused_move_is_recorded_durably() {
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
        let owed = o.store().load_review_done().expect("read owed");
        assert_eq!(owed.len(), 1, "the merge fact outlives the failed move");
        assert_eq!(owed[0].identifier, "STUDIO-712");
        assert_eq!(owed[0].pr, "makewhatis/rhapsody#64");
        assert_eq!(owed[0].attempts, 1, "the attempt is counted for the retry");
        assert!(
            !owed[0].gave_up,
            "the budget is not spent after one attempt"
        );
    }

    /// **STUDIO-1007 acceptance: a single failed move is retried and succeeds.** The first attempt
    /// fails, the row keeps the merge fact, the due retry lands and the row is forgotten.
    ///
    /// MUTATION (the ticket's ⚠️): drop the retry from `retry_pending_review_done` and the second
    /// `move_calls` assertion reds.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_terminal_move_is_retried_and_lands() {
        let mut fake = Fake::new();
        fake.move_err = Some(TrackerError::Other("linear_api_status: 503".into()));
        fake.move_err_calls = 1; // the first attempt only
        let tr = Arc::new(fake);
        let o = orch_tracked(ticketless_done("Done"), Arc::clone(&tr));
        run_of(&o, "STUDIO-712", "ID-712", "TEAM-1");
        let plan = o
            .plan_review_done(&[row(64, "handoff:STUDIO-712")], &coord(64))
            .expect("a plan");
        let handle = o.control();

        handle.finish_review_ticket(plan).await;
        assert_eq!(tr.move_calls().len(), 1, "the first attempt was refused");
        assert_eq!(
            o.store().load_review_done().expect("read owed").len(),
            1,
            "the merge is durable while the move is owed"
        );

        // Far enough past the backoff that the retry is due.
        handle
            .retry_pending_review_done(chrono::Utc::now() + chrono::Duration::hours(1))
            .await;
        assert_eq!(tr.move_calls().len(), 2, "the retry landed");
        assert!(
            o.store().load_review_done().expect("read owed").is_empty(),
            "a landed move forgets the row"
        );
    }

    /// **STUDIO-1007 acceptance: a failure that persists ends in a reported divergence.** Three
    /// refused attempts spend the bound, the row is marked `gave_up` and never retried again — it
    /// is REPORTED, not dropped (the sweep reads it; see `reviewreconcile`'s own test).
    ///
    /// MUTATION: drop the exhaustion branch from `advance_review_done` and either the attempt
    /// counts reds or the row is retried a fourth time.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_persistently_refused_terminal_move_is_reported_not_dropped() {
        let mut fake = Fake::new();
        fake.move_err = Some(TrackerError::Other("linear_move_rejected: nope".into()));
        let tr = Arc::new(fake);
        let o = orch_tracked(ticketless_done("Done"), Arc::clone(&tr));
        run_of(&o, "STUDIO-712", "ID-712", "TEAM-1");
        let plan = o
            .plan_review_done(&[row(64, "handoff:STUDIO-712")], &coord(64))
            .expect("a plan");
        let handle = o.control();
        let far = chrono::Utc::now() + chrono::Duration::hours(1);

        handle.finish_review_ticket(plan).await; // attempt 1
        handle.retry_pending_review_done(far).await; // attempt 2
        handle.retry_pending_review_done(far).await; // attempt 3 — the last

        assert_eq!(
            tr.move_calls().len(),
            3,
            "the retry is bounded at three attempts"
        );
        let owed = o.store().load_review_done().expect("read owed");
        assert_eq!(
            owed.len(),
            1,
            "an exhausted move is reported, never dropped"
        );
        assert!(owed[0].gave_up);
        assert_eq!(owed[0].attempts, 3);

        handle.retry_pending_review_done(far).await;
        assert_eq!(tr.move_calls().len(), 3, "a given-up row is never retried");
    }

    /// The origin parser, over the spellings the three writers produce and the near-misses.
    #[test]
    fn origin_ticket_reads_the_ticket_bearing_origins_only() {
        assert_eq!(origin_ticket("handoff:STUDIO-712"), Some("STUDIO-712"));
        assert_eq!(origin_ticket("handoff: STUDIO-712 "), Some("STUDIO-712"));
        assert_eq!(origin_ticket("adopt:STUDIO-836"), Some("STUDIO-836"));
        assert_eq!(origin_ticket("adopt: STUDIO-836 "), Some("STUDIO-836"));
        for other in [
            // Names an operator, not a ticket.
            "console:operator",
            "handoff",
            "handoff:",
            "handoff:   ",
            "HANDOFF:STUDIO-712",
            "adopt",
            "adopt:",
            "adopt:   ",
            "ADOPT:STUDIO-836",
            "adopted:STUDIO-836",
            "",
        ] {
            assert_eq!(origin_ticket(other), None, "{other:?}");
        }
    }
}
