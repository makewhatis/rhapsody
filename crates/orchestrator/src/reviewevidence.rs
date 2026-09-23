//! reviewevidence — the EVIDENCE LEDGER for one pull request's review loop: the loop generation,
//! the evidence revision, and the "approved at the current patch" predicate (STUDIO-1009, the
//! manager-agent program's second ticket; design record `~/.rhapsody/docs/manager-agent-design.md`
//! §5.1, §5.2 and §5.4).
//!
//! **No Go counterpart.** The frozen Symphony reference has no review feature at all, so nothing
//! here is a port; it is the additive Rhapsody surface the design record specifies, and every
//! production caller sits behind the ticketless-review gate (a review run only exists when Teams is
//! on and `review.mode: ticketless`).
//!
//! # What this module owns
//!
//! Three pure pieces, and nothing else — no store, no `gh`, no clock:
//!
//! 1. [`EvidenceInputs`] / [`evidence_fingerprint`] / [`next_evidence_rev`] — the §5.2 evidence
//!    revision. The fingerprint is a canonical rendering of exactly the listed inputs; anything the
//!    design calls NOT an input (a `rhapsody-manager`-marked comment, a tracker state move) is a
//!    field here that the fingerprint deliberately does not read, so a test can prove it is not
//!    counted.
//! 2. [`row_approved_at_current_patch`] — §5.4's predicate. Defined on the recorded COMPLETED
//!    review (its verdict, its patch-id, its generation) and never on the transient `status`
//!    column, which is exactly the F9 defect ([STUDIO-1006]): a re-introduction resets `status` to
//!    `requested`, and a predicate reading it would call an approval dead while the code it read is
//!    unchanged.
//! 3. [`completion_record`] — the one pure decision of WHAT a completed round records. A verdict of
//!    `approve`/`changes` produces a [`ReviewCompleted`]; every other exit (truncated, dropped,
//!    failed, undeclared) produces `None`, so the four `last_completed_*` columns are written only
//!    by a round that actually had its turn.
//!
//! [STUDIO-1006]: https://linear.app/studio49/issue/STUDIO-1006

use rhapsody_store::{
    REVIEW_COMPLETION_APPROVE, REVIEW_STATUS_APPROVED, ReviewCompleted, ReviewWatchRow,
};

/// One watch row as the evidence revision sees it: the row's identity, its status, the head it was
/// dispatched against, the head it last had read, and the last COMPLETED review recorded beside it.
/// Membership is the row's presence in the list that carries these, so an added or removed row
/// changes the fingerprint without a separate flag.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EvidenceWatchRow {
    /// `owner/repo#number@reviewer` — the row's identity.
    pub reviewer: String,
    /// The transient [`ReviewWatchRow::status`].
    pub status: String,
    /// The head a review was dispatched against (F-DUP's marker).
    pub requested_sha: String,
    /// The head the last completed review read (F-SHA's marker).
    pub last_reviewed_sha: String,
    /// The last review that completed with a verdict for this row, or `None` (STUDIO-1010, the M2
    /// review's fifth follow-up). §5.2 lists "any watch row's … last-completed review" as an evidence
    /// input, and these four values are the authoritative record of it (the four
    /// `last_completed_*` columns, read through [`rhapsody_store::ReviewCompleted`]); `status` is
    /// transient and `last_reviewed_sha` is written by a different path, so neither substitutes.
    pub last_completed: Option<ReviewCompleted>,
}

impl EvidenceWatchRow {
    /// The watch row as evidence, dropping everything the revision does not read. `completed` is the
    /// row's [`ReviewCompleted`] record, read by the caller from the store.
    pub fn of(row: &ReviewWatchRow, completed: Option<&ReviewCompleted>) -> EvidenceWatchRow {
        EvidenceWatchRow {
            reviewer: row.key.reviewer.clone(),
            status: row.status.clone(),
            requested_sha: row.requested_sha.clone(),
            last_reviewed_sha: row.last_reviewed_sha.clone(),
            last_completed: completed.cloned(),
        }
    }
}

/// Every input §5.2 lists, plus the two it explicitly excludes.
///
/// The excluded fields exist so a caller can pass the whole observed context in one value and a
/// test can vary ONLY an excluded field: if the fingerprint ever started reading one, the test that
/// asserts it is not an evidence input would fail. They are documentation with teeth, not dead
/// weight.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EvidenceInputs {
    /// The pull request's current head SHA.
    pub head_sha: String,
    /// Every live watch row of the pull request, in a deterministic order (see [`Self::ordered`]).
    pub watch_rows: Vec<EvidenceWatchRow>,
    /// The finding set: one canonical token per recorded finding revision (`pr|generation|reviewer|
    /// finding_id|revision|status`). The caller supplies these already canonicalized.
    pub findings: Vec<String>,
    /// The `rhapsody:human` hold, read through `labelled_and_primed`. `None` means the label set has
    /// NOT been read yet, which fails closed — an unprimed daemon must not read "no hold".
    pub human_hold: Option<bool>,
    /// Whether the pull request is a draft, as last observed. `None` is "not observed".
    pub draft: Option<bool>,
    /// Whether the pull request conflicts with its base, as last observed. `None` is "not observed".
    pub conflict: Option<bool>,
    /// The CI state as last observed (`None` is "not observed").
    pub ci: Option<String>,
    /// The loop generation (§5.1).
    pub generation: i64,
    /// **NOT an evidence input.** The bodies/ids of `rhapsody-manager`-marked comments observed on
    /// the pull request. A decision's own comment must never invalidate the decision it carries.
    pub marked_comments: Vec<String>,
    /// **NOT an evidence input.** The origin ticket's tracker state, as observed. A ticket move the
    /// decision's own effects produced must not invalidate it.
    pub ticket_state: String,
}

impl EvidenceInputs {
    /// The watch rows in a deterministic order, so two observations of the same membership produce
    /// the same fingerprint whatever order the store returned the rows in.
    fn ordered_watch_rows(&self) -> Vec<&EvidenceWatchRow> {
        let mut rows: Vec<&EvidenceWatchRow> = self.watch_rows.iter().collect();
        rows.sort_by(|a, b| a.reviewer.cmp(&b.reviewer));
        rows
    }
}

/// A canonical, order-independent rendering of every §5.2 input — and deliberately of nothing else.
///
/// Two calls with the same inputs (in any order) produce the same string; a change to any listed
/// input changes it. NUL separates fields and newline separates the row/finding lists, neither of
/// which any input may contain (a SHA, a status token, a reviewer name and a canonical finding token
/// are all single-line, NUL-free), so no two distinct inputs can collide by construction.
pub fn evidence_fingerprint(inputs: &EvidenceInputs) -> String {
    let mut out = String::new();
    out.push_str("head=");
    out.push_str(&inputs.head_sha);
    out.push('\0');
    out.push_str("generation=");
    out.push_str(&inputs.generation.to_string());
    out.push('\0');
    out.push_str("hold=");
    // A three-state rendering, because `None` (unread) is not `Some(false)` (read, no hold): folding
    // them together is precisely the fail-open the design warns against.
    out.push_str(match inputs.human_hold {
        None => "unread",
        Some(true) => "held",
        Some(false) => "clear",
    });
    out.push('\0');
    out.push_str("draft=");
    out.push_str(option_bool(inputs.draft));
    out.push('\0');
    out.push_str("conflict=");
    out.push_str(option_bool(inputs.conflict));
    out.push('\0');
    out.push_str("ci=");
    out.push_str(inputs.ci.as_deref().unwrap_or("unobserved"));
    out.push('\0');
    for row in inputs.ordered_watch_rows() {
        out.push_str("row=");
        out.push_str(&row.reviewer);
        out.push('\0');
        out.push_str(&row.status);
        out.push('\0');
        out.push_str(&row.requested_sha);
        out.push('\0');
        out.push_str(&row.last_reviewed_sha);
        out.push('\0');
        // The last completed review, in a fixed field order (STUDIO-1010; §5.2). `none` is the
        // no-completion case; every value below is single-line and NUL-free, like the row's own.
        match &row.last_completed {
            None => out.push_str("completed=none"),
            Some(c) => {
                out.push_str("completed=");
                out.push_str(&c.generation.to_string());
                out.push('\0');
                out.push_str(&c.sha);
                out.push('\0');
                out.push_str(&c.patch_id);
                out.push('\0');
                out.push_str(&c.verdict);
            }
        }
        out.push('\n');
    }
    for finding in &inputs.findings {
        out.push_str("finding=");
        out.push_str(finding);
        out.push('\n');
    }
    out
}

/// `unobserved` / `true` / `false` for an optional observation flag.
fn option_bool(v: Option<bool>) -> &'static str {
    match v {
        None => "unobserved",
        Some(true) => "true",
        Some(false) => "false",
    }
}

/// The evidence revision after observing `new_fingerprint` (STUDIO-1009; §5.2).
///
/// * The FIRST observation of a pull request (`previous` is `None`) establishes the baseline and
///   leaves the revision alone: nothing has been seen to change yet.
/// * A changed fingerprint increments by one.
/// * An unchanged fingerprint leaves it alone.
///
/// The revision is monotonic within a run; a caller persists the returned value and keeps the
/// fingerprint it was derived from.
pub fn next_evidence_rev(previous: Option<&str>, current_rev: i64, new_fingerprint: &str) -> i64 {
    match previous {
        None => current_rev,
        Some(prev) if prev == new_fingerprint => current_rev,
        Some(_) => current_rev.saturating_add(1),
    }
}

/// One watch row plus the completed-review record that belongs beside it — the input to
/// [`row_approved_at_current_patch`]. A view rather than a field on [`ReviewWatchRow`] because the
/// record is written by exactly one call and read by exactly this predicate, while the watch row is
/// touched by dozens of call sites that carry no completion.
#[derive(Debug, Clone, Copy)]
pub struct RowEvidence<'a> {
    /// The watch row. Its `status` is deliberately NOT the approval input (see the predicate).
    pub row: &'a ReviewWatchRow,
    /// The last review that completed with a verdict for this row, or `None`.
    pub completed: Option<&'a ReviewCompleted>,
}

/// §5.4's predicate: is this row APPROVED AT THE CURRENT PATCH?
///
/// True when, and only when:
/// * the recorded COMPLETED review's verdict is `approve` — never the transient `status` column, so
///   a row a re-introduction reset to `requested` still satisfies it (the F9 replay);
/// * its recorded patch-id is non-empty and equal to `current_patch_id` — so a head that moved to a
///   byte-different commit carrying the same change (the F6 replay: approved at `e2c52c1`, head
///   `d17d0b7`, identical patch-id) still satisfies it;
/// * its recorded generation equals `current_generation` — so an operator `/clear` invalidates it.
///
/// An empty `current_patch_id` (the change could not be fingerprinted) never matches: the predicate
/// fails closed whenever either side is unknown.
pub fn row_approved_at_current_patch(
    evidence: &RowEvidence<'_>,
    current_generation: i64,
    current_patch_id: &str,
) -> bool {
    completion_approved_at_current_patch(evidence.completed, current_generation, current_patch_id)
}

/// The predicate without the watch row: a completion record alone decides whether the row it
/// belongs to is approved at the current (generation, patch). [`row_approved_at_current_patch`]
/// delegates here, and the manager's approval eligibility (STUDIO-1010, design §6.4) calls this
/// directly over its own row view, so the two can never disagree about §5.4.
pub fn completion_approved_at_current_patch(
    completed: Option<&ReviewCompleted>,
    current_generation: i64,
    current_patch_id: &str,
) -> bool {
    let Some(completed) = completed else {
        return false;
    };
    completed.verdict == REVIEW_COMPLETION_APPROVE
        && !completed.patch_id.is_empty()
        && !current_patch_id.is_empty()
        && completed.patch_id == current_patch_id
        && completed.generation == current_generation
}

/// The completed-review record a round that finished with `status` leaves behind, or `None` when the
/// round did not complete with a verdict.
///
/// `status` is the EFFECTIVE verdict the completion path records
/// ([`crate::review::effective_review_status`]) — [`REVIEW_STATUS_APPROVED`] or
/// [`REVIEW_STATUS_REVIEWED`]. Every other value (the truncated/dropped/failed/undeclared exits)
/// yields `None`, which is the whole of the "a truncated review leaves `last_completed_*` untouched"
/// rule: the caller writes fields only for `Some`.
pub fn completion_record(
    status: &str,
    generation: i64,
    head_sha: &str,
    patch_id: &str,
) -> Option<ReviewCompleted> {
    let verdict = match status {
        REVIEW_STATUS_APPROVED => REVIEW_COMPLETION_APPROVE,
        rhapsody_store::REVIEW_STATUS_REVIEWED => rhapsody_store::REVIEW_COMPLETION_CHANGES,
        _ => return None,
    };
    Some(ReviewCompleted {
        generation,
        sha: head_sha.to_string(),
        patch_id: patch_id.to_string(),
        verdict: verdict.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rhapsody_store::{REVIEW_STATUS_REQUESTED, ReviewWatchKey};

    fn row(reviewer: &str, status: &str) -> ReviewWatchRow {
        ReviewWatchRow {
            key: ReviewWatchKey {
                owner: "o".into(),
                repo: "r".into(),
                number: 1,
                reviewer: reviewer.into(),
            },
            status: status.into(),
            ..Default::default()
        }
    }

    fn inputs() -> EvidenceInputs {
        EvidenceInputs {
            head_sha: "sha1".into(),
            watch_rows: vec![EvidenceWatchRow::of(&row("bob", "approved"), None)],
            findings: vec!["finding-a".into()],
            human_hold: Some(false),
            draft: Some(false),
            conflict: Some(false),
            ci: Some("success".into()),
            generation: 1,
            ..Default::default()
        }
    }

    // --- evidence revision (§5.2) -------------------------------------------------------------

    #[test]
    fn every_listed_input_moves_the_evidence_rev() {
        let base = inputs();
        let base_fp = evidence_fingerprint(&base);
        let cases: Vec<(&str, EvidenceInputs)> = vec![
            (
                "head",
                EvidenceInputs {
                    head_sha: "sha2".into(),
                    ..base.clone()
                },
            ),
            (
                "generation",
                EvidenceInputs {
                    generation: 2,
                    ..base.clone()
                },
            ),
            (
                "hold",
                EvidenceInputs {
                    human_hold: Some(true),
                    ..base.clone()
                },
            ),
            (
                "draft",
                EvidenceInputs {
                    draft: Some(true),
                    ..base.clone()
                },
            ),
            (
                "conflict",
                EvidenceInputs {
                    conflict: Some(true),
                    ..base.clone()
                },
            ),
            (
                "ci",
                EvidenceInputs {
                    ci: Some("failure".into()),
                    ..base.clone()
                },
            ),
            (
                "watch row membership",
                EvidenceInputs {
                    watch_rows: vec![],
                    ..base.clone()
                },
            ),
            (
                "watch row status",
                EvidenceInputs {
                    watch_rows: vec![EvidenceWatchRow::of(&row("bob", "requested"), None)],
                    ..base.clone()
                },
            ),
            (
                "watch row requested sha",
                EvidenceInputs {
                    watch_rows: vec![EvidenceWatchRow {
                        requested_sha: "req".into(),
                        ..EvidenceWatchRow::of(&row("bob", "approved"), None)
                    }],
                    ..base.clone()
                },
            ),
            (
                "watch row last reviewed sha",
                EvidenceInputs {
                    watch_rows: vec![EvidenceWatchRow {
                        last_reviewed_sha: "rev".into(),
                        ..EvidenceWatchRow::of(&row("bob", "approved"), None)
                    }],
                    ..base.clone()
                },
            ),
            (
                "watch row last completed review",
                EvidenceInputs {
                    watch_rows: vec![EvidenceWatchRow {
                        last_completed: Some(completed(REVIEW_COMPLETION_APPROVE, 1, "pid")),
                        ..EvidenceWatchRow::of(&row("bob", "approved"), None)
                    }],
                    ..base.clone()
                },
            ),
            (
                "finding set",
                EvidenceInputs {
                    findings: vec!["finding-b".into()],
                    ..base.clone()
                },
            ),
        ];
        for (what, changed) in cases {
            let fp = evidence_fingerprint(&changed);
            assert_ne!(fp, base_fp, "{what} is an evidence input");
            assert_eq!(
                next_evidence_rev(Some(&base_fp), 7, &fp),
                8,
                "{what} must increment the revision"
            );
        }
        assert_eq!(
            next_evidence_rev(Some(&base_fp), 7, &base_fp),
            7,
            "an unchanged observation leaves the revision alone"
        );
        assert_eq!(
            next_evidence_rev(None, 0, &base_fp),
            0,
            "the first observation is a baseline, not a change"
        );
    }

    #[test]
    fn an_unread_hold_is_not_no_hold() {
        let unread = EvidenceInputs {
            human_hold: None,
            ..inputs()
        };
        let clear = EvidenceInputs {
            human_hold: Some(false),
            ..inputs()
        };
        assert_ne!(
            evidence_fingerprint(&unread),
            evidence_fingerprint(&clear),
            "an unprimed label set must not fold into 'no hold'"
        );
    }

    /// **The self-invalidation guard.** A `rhapsody-manager`-marked comment and a tracker state move
    /// are NOT evidence inputs: varying only them must not move the revision. MUTATION: read either
    /// field in `evidence_fingerprint` and this test fails.
    #[test]
    fn a_marked_comment_or_ticket_move_is_not_evidence() {
        let base = inputs();
        let base_fp = evidence_fingerprint(&base);
        let commented = EvidenceInputs {
            marked_comments: vec!["rhapsody-manager: decision posted".into()],
            ..base.clone()
        };
        let moved = EvidenceInputs {
            ticket_state: "In Review".into(),
            ..base.clone()
        };
        assert_eq!(evidence_fingerprint(&commented), base_fp);
        assert_eq!(evidence_fingerprint(&moved), base_fp);
        assert_eq!(next_evidence_rev(Some(&base_fp), 3, &base_fp), 3);
    }

    #[test]
    fn the_fingerprint_is_row_order_independent() {
        let a = EvidenceInputs {
            watch_rows: vec![
                EvidenceWatchRow::of(&row("alice", "requested"), None),
                EvidenceWatchRow::of(&row("bob", "approved"), None),
            ],
            ..inputs()
        };
        let b = EvidenceInputs {
            watch_rows: vec![
                EvidenceWatchRow::of(&row("bob", "approved"), None),
                EvidenceWatchRow::of(&row("alice", "requested"), None),
            ],
            ..inputs()
        };
        assert_eq!(evidence_fingerprint(&a), evidence_fingerprint(&b));
    }

    // --- the approval predicate (§5.4) --------------------------------------------------------

    fn completed(verdict: &str, generation: i64, patch_id: &str) -> ReviewCompleted {
        ReviewCompleted {
            generation,
            sha: "e2c52c1".into(),
            patch_id: patch_id.into(),
            verdict: verdict.into(),
        }
    }

    #[test]
    fn approves_when_verdict_patch_and_generation_all_match() {
        let r = row("bob", "approved");
        let c = completed(REVIEW_COMPLETION_APPROVE, 1, "pid");
        let ev = RowEvidence {
            row: &r,
            completed: Some(&c),
        };
        assert!(row_approved_at_current_patch(&ev, 1, "pid"));
    }

    /// **F9 replay.** The re-introduction reset the row's `status` to `requested`, but the last
    /// COMPLETED review still approved the current patch in the current generation. MUTATION: read
    /// `row.status` instead of the completed verdict and this fails.
    #[test]
    fn f9_reintroduction_reset_row_still_approved_at_current_patch() {
        let r = row("bob", REVIEW_STATUS_REQUESTED);
        let c = completed(REVIEW_COMPLETION_APPROVE, 1, "pid");
        let ev = RowEvidence {
            row: &r,
            completed: Some(&c),
        };
        assert!(
            row_approved_at_current_patch(&ev, 1, "pid"),
            "an approval at the current patch survives a status reset"
        );
    }

    /// **F6 replay.** Approved at `e2c52c1`, head moved to `d17d0b7`, identical patch-id.
    #[test]
    fn f6_base_merge_with_identical_patch_id_is_still_approved() {
        let r = row("bob", "approved");
        // The recorded completed review is at the OLD head; the current head is a different commit
        // with the SAME patch-id.
        let c = ReviewCompleted {
            generation: 1,
            sha: "e2c52c1".into(),
            patch_id: "same-pid".into(),
            verdict: REVIEW_COMPLETION_APPROVE.into(),
        };
        let ev = RowEvidence {
            row: &r,
            completed: Some(&c),
        };
        assert!(row_approved_at_current_patch(&ev, 1, "same-pid"));
        assert!(
            !row_approved_at_current_patch(&ev, 1, "different-pid"),
            "a changed change is not approved"
        );
    }

    #[test]
    fn a_clear_generation_invalidates_a_prior_approval() {
        let r = row("bob", "approved");
        let c = completed(REVIEW_COMPLETION_APPROVE, 1, "pid");
        let ev = RowEvidence {
            row: &r,
            completed: Some(&c),
        };
        assert!(
            !row_approved_at_current_patch(&ev, 2, "pid"),
            "the generation must match (the operator's /clear)"
        );
    }

    #[test]
    fn a_changes_verdict_or_unknown_patch_is_never_approved() {
        let r = row("bob", "reviewed");
        let changes = completed(rhapsody_store::REVIEW_COMPLETION_CHANGES, 1, "pid");
        assert!(!row_approved_at_current_patch(
            &RowEvidence {
                row: &r,
                completed: Some(&changes)
            },
            1,
            "pid"
        ));
        let empty_patch = completed(REVIEW_COMPLETION_APPROVE, 1, "");
        assert!(
            !row_approved_at_current_patch(
                &RowEvidence {
                    row: &r,
                    completed: Some(&empty_patch)
                },
                1,
                "pid"
            ),
            "an unknown recorded patch-id fails closed"
        );
        assert!(
            !row_approved_at_current_patch(
                &RowEvidence {
                    row: &r,
                    completed: None
                },
                1,
                "pid"
            ),
            "no completed review is not an approval"
        );
    }

    // --- the completion record -----------------------------------------------------------------

    /// **A truncated review leaves `last_completed_*` untouched.** MUTATION: return `Some` for a
    /// truncated status and this fails.
    #[test]
    fn only_a_verdict_completion_records_a_completed_review() {
        assert!(completion_record(REVIEW_STATUS_APPROVED, 1, "sha", "pid").is_some());
        assert!(
            completion_record(rhapsody_store::REVIEW_STATUS_REVIEWED, 1, "sha", "pid").is_some()
        );
        for status in [
            rhapsody_store::REVIEW_STATUS_TRUNCATED,
            rhapsody_store::REVIEW_STATUS_DROPPED,
            rhapsody_store::REVIEW_STATUS_IN_FLIGHT,
            rhapsody_store::REVIEW_STATUS_REQUESTED,
            "review:undeclared",
            "",
        ] {
            assert!(
                completion_record(status, 1, "sha", "pid").is_none(),
                "{status} is not a completed review"
            );
        }
    }
}
