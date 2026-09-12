//! automerge — deciding whether a watched pull request has cleared every gate and may be MERGED
//! by the daemon (STUDIO-874).
//!
//! **No Go v0.4.0 counterpart.** Ticketless review is a Rhapsody addition end to end and this is
//! the edge that finishes it: [`crate::reviewdone`] already moves a ticket to Done when its pull
//! request merges, but until now the merge itself waited on a human noticing. Two pull requests in
//! one batch sat approved, green and `CLEAN` for roughly eleven hours for no other reason.
//!
//! # The verdict is DATA, and it already exists
//!
//! The obvious wrong turn here is to grep review comments for "approve", and it is wrong in a way
//! that cannot be patched: real reviewer prose from this repository includes *"I would happily
//! approve on the next push"* and *"**Request changes**, narrowly and only on finding 1"* — one of
//! which is not an approval and one of which contains its own contradiction. A merge gate that
//! guesses is worse than no merge gate.
//!
//! It never has to guess, because the ticketless path already records the verdict structurally.
//! [`review_exit_state`](crate::review::review_exit_state) reads an EXACT `HANDOFF: approved` line
//! off the review agent's final result — the payload must equal `approved`, so `HANDOFF: not
//! approved` is what it says it is — and that becomes the row's
//! [`REVIEW_STATUS_APPROVED`]/[`REVIEW_STATUS_REVIEWED`] status, written by `mark_review_completed`
//! together with the SHA the reviewer actually read. So the fact this module needs is a stored
//! enum keyed to a head commit, not English.
//!
//! That is also why the whole feature is gated on
//! [`review_auto_merge`](rhapsody_config::teams::Teams::review_auto_merge), which requires
//! `review.mode: ticketless`. On a `tickets` (quorum) installation no such verdict is written
//! anywhere — that gap is STUDIO-797's — and the watch set is empty, so auto-merging there would
//! not be conservative, it would be merging with no reviewer verdict to read at all.
//!
//! # Every gate that is a REFUSAL, and why each one is not the others
//!
//! The decision splits in two, and the split is the containment: [`auto_merge_verdict`] is a pure
//! function of the watch rows and the observed head, decided on the control task where the watch
//! set is single-writer; everything that needs GitHub happens off-loop in
//! [`crate::runautomerge`]. This half refuses on:
//!
//! * **No verdict at all** — a pull request with no live watch row has been reviewed by nobody.
//!   Fails closed rather than reading an empty required-reviewer set as "everybody approved".
//! * **A round still owing** — `requested`, `in_flight` or `truncated`. Merging here discards a
//!   review that was about to land, and a `truncated` round read the head only partially.
//! * **Changes requested** — `reviewed` means the round posted findings. STUDIO-784's second gap
//!   was exactly this state going unread, and it is the most dangerous one there is: the reviewer
//!   said no and the author has not answered yet.
//! * **A verdict older than the head** — an approval of `a324d2d` is not an approval of `c366a61`.
//!   Every round in the batch that motivated this ticket pushed new commits after a verdict, so
//!   this is the common case and not the corner one.
//! * **A status this daemon does not recognise** — fails closed, for the reason
//!   [`closed_review_status`](crate::review) states: a value the watcher cannot read is worse than
//!   no value.
//!
//! The remaining gates — draft, BEHIND, a conflict, a check that is failing or still running —
//! need GitHub and live in [`crate::runautomerge`].

use rhapsody_store::{
    REVIEW_STATUS_APPROVED, REVIEW_STATUS_IN_FLIGHT, REVIEW_STATUS_REQUESTED,
    REVIEW_STATUS_REVIEWED, REVIEW_STATUS_TRUNCATED, ReviewWatchRow,
};

use crate::prstate::PrCoord;

/// One pull request this tick's gates cleared on the control task, handed to the off-loop half.
///
/// It carries the HEAD the verdicts were read against, not just the coordinate, because the
/// off-loop half re-resolves the pull request before merging and must be able to tell that the
/// author pushed in between. Merging `head` after a push would land a commit no reviewer saw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoMergePlan {
    pub pr: PrCoord,
    /// The commit every [`AutoMergePlan::approved_by`] verdict was recorded against.
    pub head: String,
    /// The reviewers whose approval cleared it, in row order — the audit trail for the log line
    /// and the room post, so a merge can always be traced to the verdicts that allowed it.
    pub approved_by: Vec<String>,
}

/// Why a pull request was NOT proposed for an auto-merge this tick.
///
/// An enum and not a `bool`, because "a gate nobody has watched refuse is not a gate": each
/// variant is asserted on by name in this module's tests, so removing any one of the branches
/// below reds a test that says which refusal went missing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoMergeRefusal {
    /// The observation carried no head, so there is no commit to gate against.
    NoHead,
    /// No live watch row: this pull request has been reviewed by nobody.
    NoVerdict,
    /// A reviewer still owes a round at this head (`requested`, `in_flight`, `truncated`).
    RoundInFlight,
    /// The newest completed round posted findings and nothing has answered them.
    ChangesRequested,
    /// Every reviewer approved, but at least one did so at a DIFFERENT head.
    StaleVerdict,
    /// A watch status this daemon does not recognise. Fails closed.
    UnknownStatus,
}

impl AutoMergeRefusal {
    /// The refusal as the one short phrase the logs and the room use, so an operator reads the
    /// same words wherever the decision surfaces.
    pub fn why(self) -> &'static str {
        match self {
            AutoMergeRefusal::NoHead => "the observation carried no head commit",
            AutoMergeRefusal::NoVerdict => "no reviewer is watching this pull request",
            AutoMergeRefusal::RoundInFlight => "a review round is still owed at this head",
            AutoMergeRefusal::ChangesRequested => "the newest review round asked for changes",
            AutoMergeRefusal::StaleVerdict => "the approval predates the current head",
            AutoMergeRefusal::UnknownStatus => "a review row is in a state this daemon cannot read",
        }
    }
}

/// Whether every required reviewer of one pull request has recorded a non-blocking verdict AT
/// `head` — the control-task half of the gate, pure over the tick's rows.
///
/// `rows` is every LIVE watch row of this one pull request. The answer is the approving reviewers
/// on success, so the caller can name them in the audit record.
///
/// The SHA comparison is case-insensitive because hexadecimal case is not semantic; it is an
/// equality and never a prefix match, so an abbreviated SHA can never satisfy a full one.
pub(crate) fn auto_merge_verdict(
    rows: &[&ReviewWatchRow],
    head: &str,
) -> Result<Vec<String>, AutoMergeRefusal> {
    if head.trim().is_empty() {
        return Err(AutoMergeRefusal::NoHead);
    }
    if rows.is_empty() {
        return Err(AutoMergeRefusal::NoVerdict);
    }
    let mut approved_by = Vec::with_capacity(rows.len());
    for row in rows {
        match row.status.as_str() {
            REVIEW_STATUS_APPROVED => {
                // The head-keying, and the reason this is not merely `status == approved`: the
                // verdict is a statement about the commit the reviewer READ, which is the SHA
                // `mark_review_completed` stamped alongside it.
                if !row.last_reviewed_sha.eq_ignore_ascii_case(head.trim()) {
                    return Err(AutoMergeRefusal::StaleVerdict);
                }
                approved_by.push(row.key.reviewer.clone());
            }
            REVIEW_STATUS_REVIEWED => return Err(AutoMergeRefusal::ChangesRequested),
            REVIEW_STATUS_REQUESTED | REVIEW_STATUS_IN_FLIGHT | REVIEW_STATUS_TRUNCATED => {
                return Err(AutoMergeRefusal::RoundInFlight);
            }
            _ => return Err(AutoMergeRefusal::UnknownStatus),
        }
    }
    Ok(approved_by)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rhapsody_store::{REVIEW_STATUS_DROPPED, ReviewWatchKey};

    const HEAD: &str = "c366a61c366a61c366a61c366a61c366a61c366a";
    const OLD: &str = "a324d2da324d2da324d2da324d2da324d2da324d";

    fn row(reviewer: &str, status: &str, reviewed_sha: &str) -> ReviewWatchRow {
        ReviewWatchRow {
            key: ReviewWatchKey {
                owner: "makewhatis".to_string(),
                repo: "tally".to_string(),
                number: 151,
                reviewer: reviewer.to_string(),
            },
            author: "alice".to_string(),
            introduced_by: "handoff:STUDIO-867".to_string(),
            requested_sha: reviewed_sha.to_string(),
            last_reviewed_sha: reviewed_sha.to_string(),
            status: status.to_string(),
            open: true,
        }
    }

    /// The happy path the ticket's first acceptance criterion names: every reviewer approved AT the
    /// current head, so the gate clears and names them.
    #[test]
    fn every_reviewer_approved_at_the_head_clears_the_gate() {
        let rows = [
            row("alice", REVIEW_STATUS_APPROVED, HEAD),
            row("jimmy", REVIEW_STATUS_APPROVED, HEAD),
        ];
        let refs: Vec<&ReviewWatchRow> = rows.iter().collect();
        assert_eq!(
            auto_merge_verdict(&refs, HEAD),
            Ok(vec!["alice".to_string(), "jimmy".to_string()])
        );
    }

    /// ⚠️ The ticket's headline trap, and the batch's common case: every round pushed new commits
    /// after a verdict. An approval of `a324d2d` is not an approval of `c366a61`.
    #[test]
    fn an_approval_that_predates_the_head_is_refused() {
        let rows = [row("alice", REVIEW_STATUS_APPROVED, OLD)];
        let refs: Vec<&ReviewWatchRow> = rows.iter().collect();
        assert_eq!(
            auto_merge_verdict(&refs, HEAD),
            Err(AutoMergeRefusal::StaleVerdict)
        );
    }

    /// One approval at the head does not carry a peer's stale one — the gate is over EVERY row,
    /// and a partially-re-reviewed pull request is not an approved one.
    #[test]
    fn one_fresh_approval_does_not_carry_a_stale_peer() {
        let rows = [
            row("alice", REVIEW_STATUS_APPROVED, HEAD),
            row("jimmy", REVIEW_STATUS_APPROVED, OLD),
        ];
        let refs: Vec<&ReviewWatchRow> = rows.iter().collect();
        assert_eq!(
            auto_merge_verdict(&refs, HEAD),
            Err(AutoMergeRefusal::StaleVerdict)
        );
    }

    /// An `approved` row that never recorded a reviewed SHA is stale, not clear: the empty string
    /// must never compare equal to a head.
    #[test]
    fn an_approval_with_no_recorded_sha_is_refused() {
        let rows = [row("alice", REVIEW_STATUS_APPROVED, "")];
        let refs: Vec<&ReviewWatchRow> = rows.iter().collect();
        assert_eq!(
            auto_merge_verdict(&refs, HEAD),
            Err(AutoMergeRefusal::StaleVerdict)
        );
    }

    /// STUDIO-784's second gap: a round that FINISHED by posting findings. The reviewer said no.
    #[test]
    fn a_round_that_asked_for_changes_is_refused() {
        let rows = [
            row("alice", REVIEW_STATUS_APPROVED, HEAD),
            row("jimmy", REVIEW_STATUS_REVIEWED, HEAD),
        ];
        let refs: Vec<&ReviewWatchRow> = rows.iter().collect();
        assert_eq!(
            auto_merge_verdict(&refs, HEAD),
            Err(AutoMergeRefusal::ChangesRequested)
        );
    }

    /// A reviewer mid-round. Merging here discards a review that was about to land — and a
    /// `truncated` round read the head only partially, which is why it is in this set and not a
    /// completion.
    #[test]
    fn a_round_still_owed_is_refused() {
        for status in [
            REVIEW_STATUS_REQUESTED,
            REVIEW_STATUS_IN_FLIGHT,
            REVIEW_STATUS_TRUNCATED,
        ] {
            let rows = [
                row("alice", REVIEW_STATUS_APPROVED, HEAD),
                row("jimmy", status, HEAD),
            ];
            let refs: Vec<&ReviewWatchRow> = rows.iter().collect();
            assert_eq!(
                auto_merge_verdict(&refs, HEAD),
                Err(AutoMergeRefusal::RoundInFlight),
                "({status})"
            );
        }
    }

    /// No reviewer is watching, so nobody has approved. An empty required set must not read as a
    /// cleared one — the `for` loop over an empty `rows` would otherwise fall straight through to
    /// `Ok`, which is precisely the guard-that-does-not-guard this batch kept producing.
    #[test]
    fn a_pull_request_nobody_reviewed_is_refused() {
        assert_eq!(
            auto_merge_verdict(&[], HEAD),
            Err(AutoMergeRefusal::NoVerdict)
        );
    }

    /// Fails closed on a status this daemon cannot read — including `dropped`, which is a retired
    /// row and never an approval.
    #[test]
    fn an_unrecognised_status_is_refused() {
        for status in [REVIEW_STATUS_DROPPED, "", "APPROVED", "rubber-stamped"] {
            let rows = [row("alice", status, HEAD)];
            let refs: Vec<&ReviewWatchRow> = rows.iter().collect();
            assert_eq!(
                auto_merge_verdict(&refs, HEAD),
                Err(AutoMergeRefusal::UnknownStatus),
                "({status})"
            );
        }
    }

    /// An observation with no head is not an observation about a head, so nothing is gated on it.
    #[test]
    fn an_empty_head_is_refused() {
        for head in ["", "   "] {
            let rows = [row("alice", REVIEW_STATUS_APPROVED, HEAD)];
            let refs: Vec<&ReviewWatchRow> = rows.iter().collect();
            assert_eq!(
                auto_merge_verdict(&refs, head),
                Err(AutoMergeRefusal::NoHead),
                "({head:?})"
            );
        }
    }

    /// Hexadecimal case is not semantic, so a differently-cased SHA is the same commit.
    #[test]
    fn the_head_comparison_ignores_hex_case() {
        let rows = [row("alice", REVIEW_STATUS_APPROVED, HEAD)];
        let refs: Vec<&ReviewWatchRow> = rows.iter().collect();
        assert_eq!(
            auto_merge_verdict(&refs, &HEAD.to_ascii_uppercase()),
            Ok(vec!["alice".to_string()])
        );
    }

    /// An abbreviated SHA is never an approval of the full one: the comparison is equality, so a
    /// prefix cannot satisfy it.
    #[test]
    fn an_abbreviated_sha_does_not_satisfy_the_head() {
        let rows = [row("alice", REVIEW_STATUS_APPROVED, &HEAD[..7])];
        let refs: Vec<&ReviewWatchRow> = rows.iter().collect();
        assert_eq!(
            auto_merge_verdict(&refs, HEAD),
            Err(AutoMergeRefusal::StaleVerdict)
        );
    }
}
