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
//! approved` is what it says it is — and a declared verdict becomes the row's
//! [`REVIEW_STATUS_APPROVED`]/[`REVIEW_STATUS_REVIEWED`] status, written by `mark_review_completed`
//! together with the SHA the reviewer actually read. A hand-off with no readable verdict at all
//! (STUDIO-894) is never guessed into either bucket — it parks the row `REVIEW_STATUS_TRUNCATED`,
//! which [`auto_merge_verdict_with_proof`] below already refuses as a round still owed. So the fact
//! this module needs is a stored enum keyed to a head commit, not English.
//!
//! That is also why the whole feature is gated on
//! [`review_auto_merge`](rhapsody_config::teams::Teams::review_auto_merge), which requires
//! `review.mode: ticketless`. On a `tickets` (quorum) installation no such verdict is written
//! anywhere — that gap is STUDIO-797's — and the watch set is empty, so auto-merging there would
//! not be conservative, it would be merging with no reviewer verdict to read at all.
//!
//! # Every gate that is a REFUSAL, and why each one is not the others
//!
//! The decision splits in two, and the split is the containment: [`auto_merge_verdict_with_proof`]
//! is a pure function of the watch rows and the observed head, decided on the control task where
//! the watch set is single-writer; everything that needs GitHub happens off-loop in
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
//! need GitHub and live in [`crate::runautomerge`]. `draft` was named in that set from the start
//! and went unchecked until STUDIO-881, which is why it now has a gate of its own there rather
//! than relying on `mergeStateStatus`: a draft reports `CLEAN`.

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

/// Whether every required reviewer of one pull request has recorded a non-blocking verdict against
/// the change `head` carries — the control-task half of the gate, pure over the tick's rows.
///
/// `rows` is every LIVE watch row of this one pull request. The answer is the approving reviewers
/// on success, so the caller can name them in the audit record.
///
/// `proven` is the patch-id proof from [`crate::reviewwatch`] (STUDIO-977, C): the set of heads whose
/// CHANGE is the same as `head`'s — `head` itself plus every previously-reviewed head the off-loop
/// watcher proved identical. A verdict recorded at one of those counts as a verdict about this
/// change, which is what lets a `ship` (or a carried approval) satisfy approval-at-head after a
/// merge from the base. With `proven == [head]` this is the exact-head gate, unchanged.
///
/// A `ship` adjudication may satisfy approval-at-head for a patch the reviewers actually approved,
/// but the rows it is read from may still name the OLD head: the carry-over advances
/// `last_reviewed_sha` on its own tick, and a same-tick read sees the pre-advance snapshot.
///
/// This is NOT a relaxation of what counts as an approval. A row that is `requested`, `in_flight` or
/// `truncated` still refuses — nobody finished reading the change — and a row approved at a head
/// OUTSIDE `proven` is still [`AutoMergeRefusal::StaleVerdict`], because the change there was never
/// proven the same.
///
/// The SHA comparison is EXACT — byte equality, never a prefix and never case-folded.
///
/// Case-folding would be defensible on its own (hexadecimal case is not semantic), and it is still
/// the wrong choice here, because this gate does not get to decide alone. `review_round_due` and
/// [`handle_review_head_advanced`](crate::orchestrator::Orchestrator::handle_review_head_advanced)
/// both compare the same two values with `==`, and they decide whether the row is re-armed for a
/// FRESH review. A comparison looser than theirs is the one direction that breaks the whole
/// arrangement: on a differently-cased head they would arm a re-review while this cleared the pull
/// request to merge, which is exactly the "merging while a reviewer is mid-round" hazard. Matching
/// them exactly makes the disagreement unrepresentable rather than merely unlikely.
pub(crate) fn auto_merge_verdict_with_proof(
    rows: &[&ReviewWatchRow],
    head: &str,
    proven: &[&str],
) -> Result<Vec<String>, AutoMergeRefusal> {
    let head = head.trim();
    if head.is_empty() {
        return Err(AutoMergeRefusal::NoHead);
    }
    if rows.is_empty() {
        return Err(AutoMergeRefusal::NoVerdict);
    }
    let proven: Vec<&str> = proven.iter().map(|p| p.trim()).collect();
    let mut approved_by = Vec::with_capacity(rows.len());
    for row in rows {
        match row.status.as_str() {
            REVIEW_STATUS_APPROVED => {
                // The head-keying, and the reason this is not merely `status == approved`: the
                // verdict is a statement about the CHANGE the reviewer READ, which is the SHA
                // `mark_review_completed` stamped alongside it — or a head proven to carry the same
                // change.
                if !proven.contains(&row.last_reviewed_sha.as_str()) {
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

/// Whether a manager's `ship` may be recorded for the change `head` carries — the structural half of
/// STUDIO-977's rule C.
///
/// The rule is "**an unread head cannot be shipped**", and nothing more. Every live row must carry a
/// verdict that was recorded against the change `head` carries — an `approved` verdict, or a
/// `reviewed` one (findings) — at `head` or at a head `proven` to carry the same change (see
/// [`auto_merge_verdict_with_proof`]). A row still owing a round (`requested`/`in_flight`/
/// `truncated`) means nobody finished reading this change, no row at all means nobody is watching,
/// a status this daemon cannot read fails closed, and a verdict whose head was never proven the same
/// is a verdict about a DIFFERENT change; in every one of those cases the manager must escalate.
///
/// **A `reviewed` row does NOT make `ship` unavailable.** That is the whole point of STUDIO-956, and
/// the distinction between a `ship` and an `escalate`: a reviewer's findings are exactly what the
/// manager exists to adjudicate, so "a reviewer said no" is a decision the manager may overrule,
/// while "nobody has read it" is not. This is deliberately NARROWER than the merge gate (which
/// refuses a `reviewed` row as [`AutoMergeRefusal::ChangesRequested`]): a recorded `ship` over open
/// findings does not merge, and that is correct — it stops the loop and leaves the pull request to
/// the normal merge gates and a human. Whether a `ship` actually MERGES is still decided by
/// [`auto_merge_verdict_with_proof`] at `head`, untouched here; CI, draft and conflict are not
/// consulted, let alone satisfied, by this function.
///
/// Criterion 5's deliberate reversal of STUDIO-956 lives in that merge gate, not here: 956 said a
/// ship must not satisfy approval-at-head at all; this says it may, but only for a patch the
/// required reviewers approved.
pub(crate) fn ship_available(
    rows: &[&ReviewWatchRow],
    head: &str,
    proven: &[&str],
) -> Result<(), AutoMergeRefusal> {
    let head = head.trim();
    if head.is_empty() {
        return Err(AutoMergeRefusal::NoHead);
    }
    if rows.is_empty() {
        return Err(AutoMergeRefusal::NoVerdict);
    }
    let proven: Vec<&str> = proven.iter().map(|p| p.trim()).collect();
    for row in rows {
        match row.status.as_str() {
            // A settled verdict about a change the reviewer READ. `approved` and `reviewed` are the
            // same fact here — someone finished reading this change; only the merge gate below
            // distinguishes them.
            REVIEW_STATUS_APPROVED => {
                if !proven.contains(&row.last_reviewed_sha.as_str()) {
                    return Err(AutoMergeRefusal::StaleVerdict);
                }
            }
            REVIEW_STATUS_REVIEWED => {
                // A findings verdict is read — shippable — but only for THIS change. Findings about
                // a head whose change was never proven the same are findings about other code, so a
                // round is genuinely owed at `head`.
                if !proven.contains(&row.last_reviewed_sha.as_str()) {
                    return Err(AutoMergeRefusal::RoundInFlight);
                }
            }
            REVIEW_STATUS_REQUESTED | REVIEW_STATUS_IN_FLIGHT | REVIEW_STATUS_TRUNCATED => {
                return Err(AutoMergeRefusal::RoundInFlight);
            }
            _ => return Err(AutoMergeRefusal::UnknownStatus),
        }
    }
    Ok(())
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
            auto_merge_verdict_with_proof(&refs, HEAD, &[HEAD]),
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
            auto_merge_verdict_with_proof(&refs, HEAD, &[HEAD]),
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
            auto_merge_verdict_with_proof(&refs, HEAD, &[HEAD]),
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
            auto_merge_verdict_with_proof(&refs, HEAD, &[HEAD]),
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
            auto_merge_verdict_with_proof(&refs, HEAD, &[HEAD]),
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
                auto_merge_verdict_with_proof(&refs, HEAD, &[HEAD]),
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
            auto_merge_verdict_with_proof(&[], HEAD, &[HEAD]),
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
                auto_merge_verdict_with_proof(&refs, HEAD, &[HEAD]),
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
                auto_merge_verdict_with_proof(&refs, head, &[head]),
                Err(AutoMergeRefusal::NoHead),
                "({head:?})"
            );
        }
    }

    /// The comparison matches the rest of the subsystem exactly, and refuses a differently-cased
    /// head rather than accepting it.
    ///
    /// Not because the case means anything — it does not, it is the same commit — but because
    /// `review_round_due` and `handle_review_head_advanced` compare these two values with `==`. If
    /// this gate were the looser of the three, a differently-cased head would have them arming a
    /// fresh review while this one cleared the merge. Refusing costs one more tick of a case that
    /// does not arise; agreeing to disagree costs a merge under a live reviewer.
    #[test]
    fn a_differently_cased_head_is_refused_rather_than_disagreeing_with_the_watcher() {
        let rows = [row("alice", REVIEW_STATUS_APPROVED, HEAD)];
        let refs: Vec<&ReviewWatchRow> = rows.iter().collect();
        assert_eq!(
            auto_merge_verdict_with_proof(
                &refs,
                &HEAD.to_ascii_uppercase(),
                &[&HEAD.to_ascii_uppercase()]
            ),
            Err(AutoMergeRefusal::StaleVerdict)
        );
    }

    /// An abbreviated SHA is never an approval of the full one: the comparison is equality, so a
    /// prefix cannot satisfy it.
    #[test]
    fn an_abbreviated_sha_does_not_satisfy_the_head() {
        let rows = [row("alice", REVIEW_STATUS_APPROVED, &HEAD[..7])];
        let refs: Vec<&ReviewWatchRow> = rows.iter().collect();
        assert_eq!(
            auto_merge_verdict_with_proof(&refs, HEAD, &[HEAD]),
            Err(AutoMergeRefusal::StaleVerdict)
        );
    }

    // ── STUDIO-977 C: a verdict at a head proven to carry the same change ────────────────────────

    /// **Acceptance.** A verdict recorded at `OLD` clears the gate at `HEAD` once `HEAD`'s change is
    /// PROVEN the same as `OLD`'s — the patch-id proof the ship path passes. Without the proof, the
    /// exact-head rule stands.
    ///
    /// MUTATION: drop the `proven` argument (compare `last_reviewed_sha` to `head` alone) and the
    /// first assertion reds.
    #[test]
    fn a_verdict_at_a_proven_head_clears_the_gate() {
        let rows = [row("alice", REVIEW_STATUS_APPROVED, OLD)];
        let refs: Vec<&ReviewWatchRow> = rows.iter().collect();
        assert_eq!(
            auto_merge_verdict_with_proof(&refs, HEAD, &[HEAD]),
            Err(AutoMergeRefusal::StaleVerdict)
        );
        assert_eq!(
            auto_merge_verdict_with_proof(&refs, HEAD, &[HEAD, OLD]),
            Ok(vec!["alice".to_string()]),
            "an approval on the proven-identical change is an approval of this head"
        );
    }

    /// The proof is not a wildcard: a verdict at a head OUTSIDE the proven set is still stale, so a
    /// real change to the branch re-opens the gate.
    #[test]
    fn a_verdict_outside_the_proof_is_still_stale() {
        let rows = [row(
            "alice",
            REVIEW_STATUS_APPROVED,
            "ffffffffffffffffffffffffffffffffffffffff",
        )];
        let refs: Vec<&ReviewWatchRow> = rows.iter().collect();
        assert_eq!(
            auto_merge_verdict_with_proof(&refs, HEAD, &[HEAD, OLD]),
            Err(AutoMergeRefusal::StaleVerdict)
        );
    }

    /// The proof never manufactures a verdict: a row that still OWES a round refuses under the
    /// proof exactly as without it.
    #[test]
    fn the_proof_does_not_carry_a_round_still_owed() {
        for status in [
            REVIEW_STATUS_REQUESTED,
            REVIEW_STATUS_IN_FLIGHT,
            REVIEW_STATUS_TRUNCATED,
        ] {
            let rows = [row("alice", status, OLD)];
            let refs: Vec<&ReviewWatchRow> = rows.iter().collect();
            assert_eq!(
                auto_merge_verdict_with_proof(&refs, HEAD, &[HEAD, OLD]),
                Err(AutoMergeRefusal::RoundInFlight),
                "({status})"
            );
        }
    }

    /// **`ship` availability (C).** A `ship` is available exactly when every required reviewer has
    /// READ the change `head` carries — an `approved` verdict or a `reviewed` one (findings), at
    /// `head` or at a head proven to carry the same change. It is deliberately NARROWER than the
    /// merge gate: a `reviewed` row does not block a ship (the manager adjudicates findings —
    /// STUDIO-956), while a row still owing a round, no rows at all, an unreadable status, or a
    /// verdict about a change nobody proved the same all make it unavailable, so the manager must
    /// escalate.
    ///
    /// MUTATION: hold `ship_available` to the merge gate (refuse a `REVIEW_STATUS_REVIEWED` row) and
    /// the `with_findings` assertion reds — STUDIO-956's ship is gone; treat an unreadable status as
    /// read and the unknown-status assertion reds; return `Ok` unconditionally and the
    /// requested/empty/stale assertions red.
    #[test]
    fn a_ship_is_available_once_the_change_has_been_read() {
        // Approved at a head proven to carry the same change.
        let approved = [row("alice", REVIEW_STATUS_APPROVED, OLD)];
        let a: Vec<&ReviewWatchRow> = approved.iter().collect();
        assert_eq!(ship_available(&a, HEAD, &[HEAD, OLD]), Ok(()));

        // Findings are a READ, and adjudicating them is the manager's job: a `reviewed` row must not
        // make a ship unavailable. (This is the one distinction from the merge gate.)
        let with_findings = [
            row("alice", REVIEW_STATUS_APPROVED, HEAD),
            row("bob", REVIEW_STATUS_REVIEWED, HEAD),
        ];
        let wf: Vec<&ReviewWatchRow> = with_findings.iter().collect();
        assert_eq!(
            ship_available(&wf, HEAD, &[HEAD]),
            Ok(()),
            "a reviewer who asked for changes means the manager adjudicates the findings, not that \
             the change is unread"
        );

        for status in [
            REVIEW_STATUS_REQUESTED,
            REVIEW_STATUS_IN_FLIGHT,
            REVIEW_STATUS_TRUNCATED,
        ] {
            let rows = [row("alice", status, HEAD)];
            let refs: Vec<&ReviewWatchRow> = rows.iter().collect();
            assert_eq!(
                ship_available(&refs, HEAD, &[HEAD]),
                Err(AutoMergeRefusal::RoundInFlight),
                "({status}) an unread change cannot be shipped"
            );
        }

        // A verdict at a head nobody proved the same is not a verdict about THIS change.
        assert_eq!(
            ship_available(&a, HEAD, &[HEAD]),
            Err(AutoMergeRefusal::StaleVerdict)
        );
        let reviewed = [row("bob", REVIEW_STATUS_REVIEWED, OLD)];
        let r: Vec<&ReviewWatchRow> = reviewed.iter().collect();
        assert_eq!(
            ship_available(&r, HEAD, &[HEAD]),
            Err(AutoMergeRefusal::RoundInFlight),
            "findings about another change leave this change unread"
        );
        assert_eq!(
            ship_available(&[], HEAD, &[HEAD]),
            Err(AutoMergeRefusal::NoVerdict)
        );
        let unknown = [row("alice", "rubber-stamped", HEAD)];
        let u: Vec<&ReviewWatchRow> = unknown.iter().collect();
        assert_eq!(
            ship_available(&u, HEAD, &[HEAD]),
            Err(AutoMergeRefusal::UnknownStatus),
            "silence is not a read"
        );
    }
}
