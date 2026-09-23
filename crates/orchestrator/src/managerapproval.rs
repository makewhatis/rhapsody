//! managerapproval — the manager APPROVAL RECORD's pure rules (STUDIO-1011, the manager-agent
//! program's fourth ticket; design record `~/.rhapsody/docs/manager-agent-design.md` §3.1, §6.5 and
//! §8.3).
//!
//! **No Go counterpart.** The frozen Symphony reference has no review feature and no manager, so
//! nothing here is a port; it is the additive Rhapsody surface the design record specifies.
//!
//! # What this module owns
//!
//! The pure half of the record — no store, no `gh`, no clock — so §6.5's rules are table tests:
//!
//! 1. [`membership_hash`] — the digest of the live reviewer-row set at decision time. The approval's
//!    `membership_hash` is compared against a fresh digest of the CURRENT set, so any reviewer row
//!    added or dropped expires it without the daemon diffing two lists.
//! 2. [`ApprovalScope`] — the approval as the merge gate sees it, and [`ApprovalScope::covers`]: a
//!    reviewer is satisfied when the approval is `effective`, in the SAME generation, at the SAME
//!    (non-empty) patch-id, and names that reviewer in `covered_reviewers`. `pending` never covers.
//! 3. [`approval_expires`] — §6.5's six expiry triggers.
//! 4. [`approval_still_effective`] — §8.3's pre-merge recheck: the approval is still `effective`, in
//!    the same generation, at the same evidence revision, with no hold and authority still `act`.
//!
//! # The manager is never a reviewer
//!
//! Nothing here writes a watch row. [`ApprovalScope::covers`] answers about a reviewer NAME inside
//! an approval's own `covered_reviewers`, and the merge gate only asks it about rows already in the
//! watch set — so the manager's approval can never place the manager INTO that set (§3.1).

use rhapsody_store::MANAGER_APPROVAL_EFFECTIVE;
use sha2::{Digest, Sha256};

/// A digest of the live reviewer-row set at decision time (§6.5).
///
/// The digest is order-INDEPENDENT (the reviewers are sorted first), so two observations of the same
/// membership produce the same value whatever order the store returned the rows in. A row added or
/// dropped changes the digest, which is the whole of the "membership changed" expiry trigger; the
/// reviewer names are NUL-separated so no two distinct sets can collide by concatenation.
pub fn membership_hash(reviewers: &[String]) -> String {
    let mut sorted: Vec<&str> = reviewers.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    let mut hasher = Sha256::new();
    for reviewer in sorted {
        hasher.update(reviewer.as_bytes());
        hasher.update([0u8]);
    }
    let digest = hasher.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        // Infallible on a `String`: the only error `write!` can return is from the writer, and
        // `String`'s never fails.
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// One manager approval as the merge gate sees it (§6.5): the stored `state`, what it was decided
/// against, the rows it stands in for, and whether `review_authority` is currently `act`.
///
/// `authority_act` is an explicit INPUT rather than a read of config because the authority key
/// (`manager.review_authority`) does not exist until M6; until it does, every caller passes `false`
/// and nothing is ever effective — the design's "treat it as always `off`".
#[derive(Debug, Clone, Copy)]
pub struct ApprovalScope<'a> {
    /// The stored `state` ([`rhapsody_store::MANAGER_APPROVAL_EFFECTIVE`] or otherwise).
    pub state: &'a str,
    /// The generation the approval was decided in.
    pub generation: i64,
    /// The patch-id the approval was decided against.
    pub patch_id: &'a str,
    /// The live reviewer rows it stands in for.
    pub covered_reviewers: &'a [String],
    /// Whether `manager.review_authority` is `act` (always `false` until M6).
    pub authority_act: bool,
}

impl ApprovalScope<'_> {
    /// Whether this approval is EFFECTIVE for `(generation, patch_id)` — the only state that lets it
    /// satisfy a covered row. A `pending` (or expired/cancelled) approval never does, and neither
    /// does one whose generation or patch-id has moved. An empty patch-id on either side fails
    /// closed, exactly as [`crate::reviewevidence::completion_approved_at_current_patch`] does.
    pub fn is_effective(&self, generation: i64, patch_id: &str) -> bool {
        self.authority_act
            && self.state == MANAGER_APPROVAL_EFFECTIVE
            && self.generation == generation
            && !self.patch_id.is_empty()
            && !patch_id.is_empty()
            && self.patch_id == patch_id
    }

    /// Whether this approval SATISFIES `reviewer` at `(generation, patch_id)` — it is effective there
    /// and names the reviewer in its covered set. This is the one question the merge gate asks.
    pub fn covers(&self, reviewer: &str, generation: i64, patch_id: &str) -> bool {
        self.is_effective(generation, patch_id)
            && self.covered_reviewers.iter().any(|r| r == reviewer)
    }
}

/// §6.5's expiry inputs: the stored approval's binding, and the current state's answers.
#[derive(Debug, Clone, Copy)]
pub struct ExpiryInputs<'a> {
    /// The generation the approval was decided in.
    pub approval_generation: i64,
    /// The patch-id the approval was decided against.
    pub approval_patch_id: &'a str,
    /// The membership digest recorded at decision time.
    pub approval_membership_hash: &'a str,
    /// The current generation.
    pub current_generation: i64,
    /// The current patch-id.
    pub current_patch_id: &'a str,
    /// A fresh digest of the current live reviewer-row set.
    pub current_membership_hash: &'a str,
    /// A completed review has opened a blocking finding revision since the approval.
    pub blocking_finding_opened: bool,
    /// A `rhapsody:human` hold is applied.
    pub hold_applied: bool,
    /// `manager.review_authority` is `act`.
    pub authority_act: bool,
}

/// Whether an approval must be EXPIRED now (§6.5): the generation changed, the patch-id changed, the
/// live reviewer-row membership changed, a completed review opened a blocking finding, a hold was
/// applied, or authority left `act`.
pub fn approval_expires(inputs: &ExpiryInputs<'_>) -> bool {
    inputs.current_generation != inputs.approval_generation
        || inputs.current_patch_id != inputs.approval_patch_id
        || inputs.current_membership_hash != inputs.approval_membership_hash
        || inputs.blocking_finding_opened
        || inputs.hold_applied
        || !inputs.authority_act
}

/// §8.3's pre-merge recheck inputs: what the stored record says and what the current state says.
#[derive(Debug, Clone, Copy)]
pub struct RecheckInputs<'a> {
    /// The stored `state`.
    pub state: &'a str,
    /// The generation the plan carries.
    pub plan_generation: i64,
    /// The current generation.
    pub current_generation: i64,
    /// The evidence revision the plan carries.
    pub plan_evidence_rev: i64,
    /// The current evidence revision.
    pub current_evidence_rev: i64,
    /// A hold is applied.
    pub hold_applied: bool,
    /// `manager.review_authority` is `act`.
    pub authority_act: bool,
}

/// §8.3's recheck: may the merge that relies on this approval still be requested?
///
/// True only when the record is still `effective`, in the same generation, at the same evidence
/// revision, with no hold applied and authority still `act`. Every other answer — including an
/// approval the store no longer holds at all — refuses the merge, which is the fail-closed
/// direction the design requires between planning and the merge command.
pub fn approval_still_effective(inputs: &RecheckInputs<'_>) -> bool {
    inputs.authority_act
        && inputs.state == MANAGER_APPROVAL_EFFECTIVE
        && inputs.plan_generation == inputs.current_generation
        && inputs.plan_evidence_rev == inputs.current_evidence_rev
        && !inputs.hold_applied
}

#[cfg(test)]
mod tests {
    use super::*;
    use rhapsody_store::{
        MANAGER_APPROVAL_CANCELLED, MANAGER_APPROVAL_EXPIRED, MANAGER_APPROVAL_PENDING,
    };

    const PATCH: &str = "patch-id-1";

    fn covered(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| (*n).to_string()).collect()
    }

    fn scope<'a>(state: &'a str, covered: &'a [String]) -> ApprovalScope<'a> {
        ApprovalScope {
            state,
            generation: 1,
            patch_id: PATCH,
            covered_reviewers: covered,
            authority_act: true,
        }
    }

    // --- coverage (§6.5) ------------------------------------------------------------------------

    /// **The ticket's first acceptance criterion.** `effective` satisfies its covered rows at the
    /// same (generation, patch-id); a row it does not name is not satisfied, and no other state is.
    ///
    /// MUTATION: count `pending` (drop the `state == effective` check) and the pending assertion
    /// reds.
    #[test]
    fn effective_covers_only_its_named_rows() {
        let cov = covered(&["alice"]);
        let effective = scope(MANAGER_APPROVAL_EFFECTIVE, &cov);
        assert!(effective.covers("alice", 1, PATCH));
        assert!(
            !effective.covers("jimmy", 1, PATCH),
            "a row the approval does not name is not satisfied"
        );
        for state in [
            MANAGER_APPROVAL_PENDING,
            MANAGER_APPROVAL_EXPIRED,
            MANAGER_APPROVAL_CANCELLED,
        ] {
            assert!(
                !scope(state, &cov).covers("alice", 1, PATCH),
                "{state} never satisfies a row"
            );
        }
    }

    /// A generation or patch-id that has moved leaves the approval inert — the same F6/F9 rules the
    /// reviewer predicate follows. An empty patch-id on either side fails closed.
    #[test]
    fn a_moved_generation_or_patch_is_not_effective() {
        let cov = covered(&["alice"]);
        let s = scope(MANAGER_APPROVAL_EFFECTIVE, &cov);
        assert!(!s.covers("alice", 2, PATCH), "a changed generation");
        assert!(!s.covers("alice", 1, "patch-id-2"), "a changed patch-id");
        assert!(
            !s.covers("alice", 1, ""),
            "an unknown current patch fails closed"
        );
        let empty = ApprovalScope {
            patch_id: "",
            ..scope(MANAGER_APPROVAL_EFFECTIVE, &cov)
        };
        assert!(
            !empty.covers("alice", 1, PATCH),
            "an unknown recorded patch fails closed"
        );
    }

    /// `review_authority` off makes even an `effective` row inert — until M6 the authority is always
    /// off, so nothing is ever effective.
    #[test]
    fn authority_off_makes_every_approval_inert() {
        let cov = covered(&["alice"]);
        let off = ApprovalScope {
            authority_act: false,
            ..scope(MANAGER_APPROVAL_EFFECTIVE, &cov)
        };
        assert!(!off.covers("alice", 1, PATCH));
    }

    // --- membership hash ------------------------------------------------------------------------

    /// The digest is order-independent and changes when a reviewer is added or dropped.
    ///
    /// MUTATION: drop the sort (hash in list order) and the order assertion reds.
    #[test]
    fn membership_hash_is_order_independent_and_membership_sensitive() {
        let a = membership_hash(&covered(&["alice", "bob"]));
        let b = membership_hash(&covered(&["bob", "alice"]));
        assert_eq!(a, b, "the same membership in any order is the same digest");
        assert_ne!(
            a,
            membership_hash(&covered(&["alice"])),
            "a dropped reviewer changes the digest"
        );
        assert_ne!(
            a,
            membership_hash(&covered(&["alice", "bob", "carol"])),
            "an added reviewer changes the digest"
        );
        assert_ne!(
            a,
            membership_hash(&covered(&[])),
            "an empty set is its own digest"
        );
    }

    // --- expiry (§6.5) --------------------------------------------------------------------------

    fn expiry<'a>(patch: &'a str, membership: &'a str) -> ExpiryInputs<'a> {
        ExpiryInputs {
            approval_generation: 1,
            approval_patch_id: PATCH,
            approval_membership_hash: "membership",
            current_generation: 1,
            current_patch_id: patch,
            current_membership_hash: membership,
            blocking_finding_opened: false,
            hold_applied: false,
            authority_act: true,
        }
    }

    /// **Each §6.5 trigger expires the approval.** A changed generation, a changed patch-id, a
    /// changed membership hash (which is what an added or dropped reviewer produces), a completed
    /// blocking finding, a hold, or authority leaving `act`.
    ///
    /// MUTATION: drop the membership-hash comparison and the `membership` case reds (the ticket's
    /// "added-reviewer test").
    #[test]
    fn every_expiry_trigger_expires_the_approval() {
        assert!(
            !approval_expires(&expiry(PATCH, "membership")),
            "nothing changed: the approval holds"
        );

        let cases: Vec<(&str, ExpiryInputs<'_>)> = vec![
            (
                "patch-id changed",
                ExpiryInputs {
                    current_patch_id: "patch-id-2",
                    ..expiry(PATCH, "membership")
                },
            ),
            (
                "generation changed",
                ExpiryInputs {
                    current_generation: 2,
                    ..expiry(PATCH, "membership")
                },
            ),
            (
                "membership changed",
                ExpiryInputs {
                    current_membership_hash: "membership-plus-carol",
                    ..expiry(PATCH, "membership")
                },
            ),
            (
                "a blocking finding opened",
                ExpiryInputs {
                    blocking_finding_opened: true,
                    ..expiry(PATCH, "membership")
                },
            ),
            (
                "a hold was applied",
                ExpiryInputs {
                    hold_applied: true,
                    ..expiry(PATCH, "membership")
                },
            ),
            (
                "authority left act",
                ExpiryInputs {
                    authority_act: false,
                    ..expiry(PATCH, "membership")
                },
            ),
        ];
        for (what, inputs) in cases {
            assert!(approval_expires(&inputs), "{what} must expire the approval");
        }
    }

    /// A head move that KEEPS the patch-id does NOT expire it (§6.5) — the approval is bound to the
    /// change, not the commit.
    #[test]
    fn a_patch_preserving_head_move_does_not_expire_the_approval() {
        // The current patch-id is the same string even though the head moved; `expiry` takes the
        // patch only, so this is the same case as nothing-changed. Stated explicitly as the design's
        // rule for the next person editing `approval_expires`.
        assert!(!approval_expires(&expiry(PATCH, "membership")));
    }

    // --- the pre-merge recheck (§8.3) -----------------------------------------------------------

    fn recheck(state: &str) -> RecheckInputs<'_> {
        RecheckInputs {
            state,
            plan_generation: 1,
            current_generation: 1,
            plan_evidence_rev: 3,
            current_evidence_rev: 3,
            hold_applied: false,
            authority_act: true,
        }
    }

    /// The recheck answers yes only for an `effective` record whose generation, evidence revision,
    /// hold and authority all still hold.
    ///
    /// MUTATION: accept a `pending` record and the pending case reds; drop the evidence-revision
    /// comparison and the moved-evidence case reds.
    #[test]
    fn the_recheck_refuses_anything_but_a_still_current_effective_approval() {
        assert!(approval_still_effective(&recheck(
            MANAGER_APPROVAL_EFFECTIVE
        )));

        let cases: Vec<(&str, RecheckInputs<'_>)> = vec![
            (
                "pending is never effective",
                recheck(MANAGER_APPROVAL_PENDING),
            ),
            (
                "expired is never effective",
                recheck(MANAGER_APPROVAL_EXPIRED),
            ),
            (
                "a changed generation",
                RecheckInputs {
                    current_generation: 2,
                    ..recheck(MANAGER_APPROVAL_EFFECTIVE)
                },
            ),
            (
                "moved evidence",
                RecheckInputs {
                    current_evidence_rev: 4,
                    ..recheck(MANAGER_APPROVAL_EFFECTIVE)
                },
            ),
            (
                "a hold applied after planning",
                RecheckInputs {
                    hold_applied: true,
                    ..recheck(MANAGER_APPROVAL_EFFECTIVE)
                },
            ),
            (
                "authority changed",
                RecheckInputs {
                    authority_act: false,
                    ..recheck(MANAGER_APPROVAL_EFFECTIVE)
                },
            ),
        ];
        for (what, inputs) in cases {
            assert!(
                !approval_still_effective(&inputs),
                "the recheck must refuse: {what}"
            );
        }
    }
}
