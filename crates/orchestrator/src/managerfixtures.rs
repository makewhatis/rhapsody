//! managerfixtures — the M12 **release gate** (STUDIO-1019; design record
//! `~/.rhapsody/docs/manager-agent-design.md` §15).
//!
//! **No Go v0.4.0 counterpart.** The whole manager program is a Rhapsody addition, and this module
//! is its release gate: the nine incidents that justify the manager, encoded as **recorded evidence
//! paired with proposed decisions** (tier 2, §15.2), a **replay harness that scores** a proposer
//! against them (tier 3, §15.3), and the **§15.4 acceptance matrix** that maps every named
//! failure-injection case to the test that protects it.
//!
//! Three properties this module must keep:
//!
//! * **Tier 2 is deterministic.** Every fixture's acceptable answer is accepted by the same pure
//!   functions the daemon uses ([`crate::managerdecision`]), and every rejected answer is refused
//!   for the exact reason §15.2 names — no model, no clock, no store.
//! * **Tier 3 REPORTS and never blocks.** The harness' real proposer runs a live model and is
//!   `#[ignore]`d and operator-driven; CI exercises it only through deterministic scripted
//!   proposers. A wrong answer from a model can never red a build.
//! * **F8's original human action is known-WRONG, not an acceptance.** "Hold for the tests" was
//!   schema-valid but decided on an hour-old snapshot; the harness counts it as a judgment miss.
//!
//! The §15.4 matrix consumes the earlier manager tickets' tests rather than re-implementing them
//! (the ticket: "This ticket consumes earlier tickets' evidence. It doesn't re-implement their
//! tests."). Each entry names the test that fails when its protection is removed; the guard test
//! below proves no entry has rotted to a missing test. The mutations themselves are sampled
//! manually, one per §15.4 group, and recorded in the pull request.

use crate::managerdecision::{
    ApprovalInputs, DecisionKind, FindingRef, KnownFinding, ManagerReviewRow, PreconditionInputs,
    approval_eligibility, eligible_rows, head_is_current, parse_decision, preconditions,
    validate_final,
};
use crate::managerintervention::{CaseRow, ManagerCasePacket};
use rhapsody_store::{
    REVIEW_COMPLETION_APPROVE, REVIEW_COMPLETION_CHANGES, REVIEW_FINDING_OPEN, ReviewCompleted,
};

// ---------------------------------------------------------------------------------------------
// Tier 2 — the incident fixtures (§15.2)
// ---------------------------------------------------------------------------------------------

/// One watch row in a fixture's recorded evidence: the reviewer, the transient status, and the
/// last review that completed with a verdict (`last_completed_*`, §5.4).
struct IncidentRow {
    reviewer: &'static str,
    status: &'static str,
    verdict: &'static str,
    generation: i64,
    sha: &'static str,
    patch_id: &'static str,
    diff_covered: bool,
}

impl IncidentRow {
    fn to_row(&self) -> ManagerReviewRow {
        ManagerReviewRow {
            reviewer: self.reviewer.to_string(),
            is_manager: false,
            status: self.status.to_string(),
            completed: Some(ReviewCompleted {
                generation: self.generation,
                sha: self.sha.to_string(),
                patch_id: self.patch_id.to_string(),
                verdict: self.verdict.to_string(),
            }),
            diff_covered: self.diff_covered,
        }
    }
}

/// A finding revision the daemon knows, as `parse_decision` needs it (§5.3).
struct KnownFindingRaw {
    id: &'static str,
    revision: i64,
    status: &'static str,
    blocking: bool,
}

impl KnownFindingRaw {
    fn to_known(&self) -> KnownFinding {
        KnownFinding {
            finding_id: self.id.to_string(),
            revision: self.revision,
            status: self.status.to_string(),
            blocking: self.blocking,
        }
    }
}

/// How a fixture's case must resolve deterministically.
#[derive(Clone, Copy, Debug)]
enum Expect {
    /// Accepted by the full deterministic pipeline.
    Accept,
    /// Refused at parse; the `DecisionError`'s Debug must contain the needle.
    Parse(&'static str),
    /// Refused by [`preconditions`].
    Preconditions(&'static str),
    /// Refused by [`head_is_current`].
    Head(&'static str),
    /// Refused by [`approval_eligibility`]; the needle names the failed condition.
    Approval(&'static str),
}

/// One proposed decision block and how it must resolve against a fixture's evidence.
struct IncidentCase {
    label: &'static str,
    /// The decision block body (a compact JSON object, unfenced).
    block: &'static str,
    expect: Expect,
    /// An override of the finding ledger this case validates against (defaults to the fixture's).
    known: Option<&'static [KnownFindingRaw]>,
}

/// One incident: its authoritative evidence plus the acceptable, deterministically rejected, and
/// (for F8) known-wrong proposed decisions.
struct IncidentFixture {
    id: &'static str,
    pr: u64,
    head: &'static str,
    current_patch_id: &'static str,
    generation: i64,
    threshold_reached: bool,
    final_intervention: bool,
    effective_reviewers: usize,
    required: &'static [&'static str],
    rows: &'static [IncidentRow],
    findings: &'static [KnownFindingRaw],
    open_blocking: &'static [(&'static str, i64)],
    acceptable: &'static [IncidentCase],
    rejected: &'static [IncidentCase],
    /// F8 only: the answer the human gave that was WRONG on the evidence (§15.2).
    known_wrong: Option<IncidentCase>,
    /// Rows that must NOT be re-requested by the acceptable effect: F5's effect-level rejection.
    must_not_rerun: &'static [&'static str],
}

const F1_ROWS: &[IncidentRow] = &[IncidentRow {
    reviewer: "sol",
    status: "reviewed",
    verdict: REVIEW_COMPLETION_CHANGES,
    generation: 1,
    sha: "829a28f",
    patch_id: "old",
    diff_covered: true,
}];

const F2_ROWS: &[IncidentRow] = &[
    IncidentRow {
        reviewer: "sol",
        status: "reviewed",
        verdict: REVIEW_COMPLETION_CHANGES,
        generation: 1,
        sha: "ad09f06",
        patch_id: "p-old-1",
        diff_covered: true,
    },
    IncidentRow {
        reviewer: "jimmy",
        status: "reviewed",
        verdict: REVIEW_COMPLETION_CHANGES,
        generation: 1,
        sha: "60af0ff",
        patch_id: "p-old-2",
        diff_covered: true,
    },
];

const F3_ROWS: &[IncidentRow] = &[IncidentRow {
    reviewer: "sol",
    status: "reviewed",
    verdict: REVIEW_COMPLETION_CHANGES,
    generation: 1,
    sha: "375349d",
    patch_id: "p-old",
    diff_covered: true,
}];

const F4_ROWS: &[IncidentRow] = &[IncidentRow {
    reviewer: "sol",
    status: "reviewed",
    verdict: REVIEW_COMPLETION_CHANGES,
    generation: 1,
    sha: "d196719",
    patch_id: "p-old",
    diff_covered: true,
}];

const F5_ROWS: &[IncidentRow] = &[
    IncidentRow {
        reviewer: "sol",
        status: "reviewed",
        verdict: REVIEW_COMPLETION_CHANGES,
        generation: 1,
        sha: "older",
        patch_id: "OLD",
        diff_covered: true,
    },
    IncidentRow {
        reviewer: "jimmy",
        status: "approved",
        verdict: REVIEW_COMPLETION_APPROVE,
        generation: 1,
        sha: "cur",
        patch_id: "pid",
        diff_covered: true,
    },
];

const F6_ROWS: &[IncidentRow] = &[
    IncidentRow {
        reviewer: "sol",
        status: "approved",
        verdict: REVIEW_COMPLETION_APPROVE,
        generation: 1,
        sha: "e2c52c1",
        patch_id: "same-pid",
        diff_covered: true,
    },
    IncidentRow {
        reviewer: "jimmy",
        status: "approved",
        verdict: REVIEW_COMPLETION_APPROVE,
        generation: 1,
        sha: "e2c52c1",
        patch_id: "same-pid",
        diff_covered: true,
    },
];

const F7_ROWS: &[IncidentRow] = &[
    IncidentRow {
        reviewer: "alice",
        status: "reviewed",
        verdict: REVIEW_COMPLETION_CHANGES,
        generation: 1,
        sha: "h",
        patch_id: "pid",
        diff_covered: true,
    },
    IncidentRow {
        reviewer: "jimmy",
        status: "reviewed",
        verdict: REVIEW_COMPLETION_CHANGES,
        generation: 1,
        sha: "h",
        patch_id: "pid",
        diff_covered: true,
    },
];

const F7_FINDINGS: &[KnownFindingRaw] = &[
    KnownFindingRaw {
        id: "alice:F1",
        revision: 1,
        status: REVIEW_FINDING_OPEN,
        blocking: true,
    },
    KnownFindingRaw {
        id: "alice:F2",
        revision: 1,
        status: REVIEW_FINDING_OPEN,
        blocking: true,
    },
    KnownFindingRaw {
        id: "jimmy:B5",
        revision: 1,
        status: REVIEW_FINDING_OPEN,
        blocking: true,
    },
];

const F8_ROWS: &[IncidentRow] = &[IncidentRow {
    reviewer: "sol",
    status: "reviewed",
    verdict: REVIEW_COMPLETION_CHANGES,
    generation: 1,
    sha: "0052489",
    patch_id: "p-old",
    diff_covered: true,
}];

const F8_FINDINGS: &[KnownFindingRaw] = &[KnownFindingRaw {
    id: "sol:B8",
    revision: 1,
    status: REVIEW_FINDING_OPEN,
    blocking: true,
}];

const F9_ROWS: &[IncidentRow] = &[
    IncidentRow {
        reviewer: "alice",
        status: "requested",
        verdict: REVIEW_COMPLETION_APPROVE,
        generation: 1,
        sha: "e4f615e",
        patch_id: "pid",
        diff_covered: true,
    },
    IncidentRow {
        reviewer: "bob",
        status: "requested",
        verdict: REVIEW_COMPLETION_APPROVE,
        generation: 1,
        sha: "e4f615e",
        patch_id: "pid",
        diff_covered: true,
    },
    IncidentRow {
        reviewer: "carol",
        status: "requested",
        verdict: REVIEW_COMPLETION_APPROVE,
        generation: 1,
        sha: "e4f615e",
        patch_id: "pid",
        diff_covered: true,
    },
];

/// The nine incidents of §1, as the §15.2 table pins them.
const FIXTURES: &[IncidentFixture] = &[
    IncidentFixture {
        id: "F1",
        pr: 212,
        head: "87aa044",
        current_patch_id: "cur",
        generation: 1,
        threshold_reached: false,
        final_intervention: false,
        effective_reviewers: 1,
        required: &[],
        rows: F1_ROWS,
        findings: &[],
        open_blocking: &[],
        acceptable: &[IncidentCase {
            label: "another review round",
            block: r#"{"decision":"RERUN_REVIEW","head":"87aa044","evidence_rev":1,"rerun":{},"rationale":"no reviewer read head 87aa044"}"#,
            expect: Expect::Accept,
            known: None,
        }],
        rejected: &[IncidentCase {
            label: "APPROVE below the threshold",
            block: r#"{"decision":"APPROVE","head":"87aa044","evidence_rev":1,"rationale":"everyone read it"}"#,
            expect: Expect::Approval("Threshold"),
            known: None,
        }],
        known_wrong: None,
        must_not_rerun: &[],
    },
    IncidentFixture {
        id: "F2",
        pr: 213,
        head: "568f8fa",
        current_patch_id: "p-new",
        generation: 1,
        threshold_reached: true,
        final_intervention: false,
        effective_reviewers: 1,
        required: &[],
        rows: F2_ROWS,
        findings: &[],
        open_blocking: &[],
        acceptable: &[IncidentCase {
            label: "another review round",
            block: r#"{"decision":"RERUN_REVIEW","head":"568f8fa","evidence_rev":1,"rerun":{},"rationale":"no reviewer read head 568f8fa"}"#,
            expect: Expect::Accept,
            known: None,
        }],
        rejected: &[IncidentCase {
            label: "APPROVE before the rows read the current patch",
            block: r#"{"decision":"APPROVE","head":"568f8fa","evidence_rev":1,"rationale":"all approved"}"#,
            expect: Expect::Approval("RowsAtCurrentPatch"),
            known: None,
        }],
        known_wrong: None,
        must_not_rerun: &[],
    },
    IncidentFixture {
        id: "F3",
        pr: 216,
        head: "2b09f2a",
        current_patch_id: "p-new",
        generation: 1,
        threshold_reached: true,
        final_intervention: false,
        effective_reviewers: 1,
        required: &[],
        rows: F3_ROWS,
        findings: &[],
        open_blocking: &[],
        acceptable: &[IncidentCase {
            label: "another review round",
            block: r#"{"decision":"RERUN_REVIEW","head":"2b09f2a","evidence_rev":1,"rerun":{},"rationale":"the current patch is unread"}"#,
            expect: Expect::Accept,
            known: None,
        }],
        rejected: &[IncidentCase {
            label: "APPROVE before the rows read the current patch",
            block: r#"{"decision":"APPROVE","head":"2b09f2a","evidence_rev":1,"rationale":"all approved"}"#,
            expect: Expect::Approval("RowsAtCurrentPatch"),
            known: None,
        }],
        known_wrong: None,
        must_not_rerun: &[],
    },
    IncidentFixture {
        id: "F4",
        pr: 213,
        head: "c589e1f",
        current_patch_id: "p-new",
        generation: 1,
        threshold_reached: true,
        final_intervention: false,
        effective_reviewers: 1,
        required: &[],
        rows: F4_ROWS,
        findings: &[],
        open_blocking: &[],
        acceptable: &[IncidentCase {
            label: "another review round, with a note to reviewers",
            block: r#"{"decision":"RERUN_REVIEW","head":"c589e1f","evidence_rev":1,"rerun":{"note":"please check the gap specifically"},"rationale":"the head is unread"}"#,
            expect: Expect::Accept,
            known: None,
        }],
        rejected: &[IncidentCase {
            label: "ROUTE_TO_AUTHOR naming no open revision",
            block: r#"{"decision":"ROUTE_TO_AUTHOR","head":"c589e1f","evidence_rev":1,"route":{"fix":[{"finding":"sol:B8","revision":1}],"instructions":"fix it"},"rationale":"there is a finding"}"#,
            expect: Expect::Parse("UnknownFinding"),
            known: None,
        }],
        known_wrong: None,
        must_not_rerun: &[],
    },
    IncidentFixture {
        id: "F5",
        pr: 212,
        head: "f5head",
        current_patch_id: "pid",
        generation: 1,
        threshold_reached: true,
        final_intervention: false,
        effective_reviewers: 1,
        required: &[],
        rows: F5_ROWS,
        findings: &[],
        open_blocking: &[],
        acceptable: &[IncidentCase {
            label: "clear + rerun; the effect re-requests sol only",
            block: r#"{"decision":"RERUN_REVIEW","head":"f5head","evidence_rev":1,"rerun":{},"rationale":"sol has not read the current patch"}"#,
            expect: Expect::Accept,
            known: None,
        }],
        // F5's rejection is effect-level (§15.2): the acceptable effect must NOT reset jimmy's
        // already-approved row. Expressed by `must_not_rerun` below, asserted after the accept.
        rejected: &[],
        known_wrong: None,
        must_not_rerun: &["jimmy"],
    },
    IncidentFixture {
        id: "F6",
        pr: 213,
        head: "d17d0b7",
        current_patch_id: "same-pid",
        generation: 1,
        threshold_reached: true,
        final_intervention: false,
        effective_reviewers: 1,
        required: &[],
        rows: F6_ROWS,
        findings: &[],
        open_blocking: &[],
        acceptable: &[IncidentCase {
            label: "APPROVE — every row approved at the current patch",
            block: r#"{"decision":"APPROVE","head":"d17d0b7","evidence_rev":1,"rationale":"identical patch-id; everyone approved it"}"#,
            expect: Expect::Accept,
            known: None,
        }],
        rejected: &[IncidentCase {
            label: "RERUN_REVIEW with no eligible rows",
            block: r#"{"decision":"RERUN_REVIEW","head":"d17d0b7","evidence_rev":1,"rerun":{},"rationale":"rerun"}"#,
            expect: Expect::Preconditions("NoEligibleRows"),
            known: None,
        }],
        known_wrong: None,
        must_not_rerun: &["sol", "jimmy"],
    },
    IncidentFixture {
        id: "F7",
        pr: 214,
        head: "h",
        current_patch_id: "pid",
        generation: 1,
        threshold_reached: true,
        final_intervention: false,
        effective_reviewers: 1,
        required: &[],
        rows: F7_ROWS,
        findings: F7_FINDINGS,
        open_blocking: &[("alice:F1", 1), ("alice:F2", 1), ("jimmy:B5", 1)],
        acceptable: &[IncidentCase {
            label: "ROUTE_TO_AUTHOR naming every open blocking revision",
            block: r#"{"decision":"ROUTE_TO_AUTHOR","head":"h","evidence_rev":1,"route":{"fix":[{"finding":"alice:F1","revision":1},{"finding":"alice:F2","revision":1},{"finding":"jimmy:B5","revision":1}],"instructions":"address all three findings"},"rationale":"the author skipped them for a round"}"#,
            expect: Expect::Accept,
            known: None,
        }],
        rejected: &[
            IncidentCase {
                label: "ROUTE_TO_AUTHOR naming a resolved revision",
                block: r#"{"decision":"ROUTE_TO_AUTHOR","head":"h","evidence_rev":1,"route":{"fix":[{"finding":"alice:F1","revision":1}],"instructions":"fix it"},"rationale":"route"}"#,
                expect: Expect::Parse("WrongStatusFinding"),
                known: Some(&[KnownFindingRaw {
                    id: "alice:F1",
                    revision: 1,
                    status: rhapsody_store::REVIEW_FINDING_RESOLVED,
                    blocking: true,
                }]),
            },
            IncidentCase {
                label: "APPROVE without dismissing the open findings",
                block: r#"{"decision":"APPROVE","head":"h","evidence_rev":1,"rationale":"approve"}"#,
                expect: Expect::Approval("FindingsResolved"),
                known: None,
            },
        ],
        known_wrong: None,
        must_not_rerun: &[],
    },
    IncidentFixture {
        id: "F8",
        pr: 210,
        head: "b02fc72",
        current_patch_id: "p-new",
        generation: 1,
        threshold_reached: true,
        final_intervention: false,
        effective_reviewers: 1,
        required: &[],
        rows: F8_ROWS,
        findings: F8_FINDINGS,
        open_blocking: &[("sol:B8", 1)],
        acceptable: &[IncidentCase {
            label: "another review round at the current head",
            block: r#"{"decision":"RERUN_REVIEW","head":"b02fc72","evidence_rev":1,"rerun":{},"rationale":"the tests are already pushed; sol has not read b02fc72"}"#,
            expect: Expect::Accept,
            known: None,
        }],
        rejected: &[
            IncidentCase {
                label: "a decision naming the stale head 31ee051",
                block: r#"{"decision":"RERUN_REVIEW","head":"31ee051","evidence_rev":1,"rerun":{},"rationale":"hold"}"#,
                expect: Expect::Head("HeadNotCurrent"),
                known: None,
            },
            IncidentCase {
                label: "APPROVE before the rows read the current patch",
                block: r#"{"decision":"APPROVE","head":"b02fc72","evidence_rev":1,"rationale":"approve"}"#,
                expect: Expect::Approval("RowsAtCurrentPatch"),
                known: None,
            },
        ],
        // F8's original human action: schema-valid as a ROUTE_TO_AUTHOR, wrong on the evidence.
        // It is a KNOWN-WRONG answer for tier 3, never an acceptance.
        known_wrong: Some(IncidentCase {
            label: "the human's 'hold for the tests' (known-wrong)",
            block: r#"{"decision":"ROUTE_TO_AUTHOR","head":"b02fc72","evidence_rev":1,"route":{"fix":[{"finding":"sol:B8","revision":1}],"instructions":"hold for the tests"},"rationale":"the requested tests are missing"}"#,
            expect: Expect::Accept,
            known: None,
        }),
        must_not_rerun: &[],
    },
    IncidentFixture {
        id: "F9",
        pr: 216,
        head: "e4f615e",
        current_patch_id: "pid",
        generation: 1,
        threshold_reached: true,
        final_intervention: false,
        effective_reviewers: 3,
        required: &[],
        rows: F9_ROWS,
        findings: &[],
        open_blocking: &[],
        acceptable: &[IncidentCase {
            label: "APPROVE — the recorded approvals carry across the re-introduction",
            block: r#"{"decision":"APPROVE","head":"e4f615e","evidence_rev":1,"rationale":"every reviewer approved the current patch; the statuses are only a re-introduction"}"#,
            expect: Expect::Accept,
            known: None,
        }],
        rejected: &[IncidentCase {
            label: "RERUN_REVIEW with no eligible rows",
            block: r#"{"decision":"RERUN_REVIEW","head":"e4f615e","evidence_rev":1,"rerun":{},"rationale":"rerun"}"#,
            expect: Expect::Preconditions("NoEligibleRows"),
            known: None,
        }],
        known_wrong: None,
        must_not_rerun: &["alice", "bob", "carol"],
    },
];

// ---------------------------------------------------------------------------------------------
// Tier 2 — the deterministic evaluator
// ---------------------------------------------------------------------------------------------

/// The deterministic outcome of running one proposed decision against a fixture's evidence.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Accepted,
    Parse(String),
    Preconditions(String),
    Head(String),
    Approval(Vec<String>),
}

fn fence(block: &str) -> String {
    format!(
        "prose\n\n```{}\n{block}\n```\n\nHANDOFF: done\n",
        crate::managerdecision::MANAGER_DECISION_TAG
    )
}

fn open_from(o: &(&'static str, i64)) -> FindingRef {
    FindingRef {
        finding: o.0.to_string(),
        revision: o.1,
    }
}

/// Run the full deterministic pipeline (parse → head → preconditions → final → approval) exactly
/// as the daemon does, so a fixture's acceptable/rejected pair is judged by the production rules.
fn evaluate_text(
    f: &IncidentFixture,
    text: &str,
    known_override: Option<&[KnownFindingRaw]>,
) -> Outcome {
    let raw_known = known_override.unwrap_or(f.findings);
    let known: Vec<KnownFinding> = raw_known.iter().map(KnownFindingRaw::to_known).collect();
    let decision = match parse_decision(text, &known) {
        Ok(d) => d,
        Err(e) => return Outcome::Parse(format!("{e:?}")),
    };
    if let Err(e) = head_is_current(&decision, f.head) {
        return Outcome::Head(format!("{e:?}"));
    }
    let rows: Vec<ManagerReviewRow> = f.rows.iter().map(IncidentRow::to_row).collect();
    let eligible = eligible_rows(&rows, f.generation, f.current_patch_id).len();
    if let Err(e) = preconditions(
        &decision,
        &PreconditionInputs {
            eligible_rows: eligible,
        },
    ) {
        return Outcome::Preconditions(format!("{e:?}"));
    }
    if let Err(e) = validate_final(&decision, f.final_intervention) {
        return Outcome::Parse(format!("{e:?}"));
    }
    if matches!(decision.kind, DecisionKind::Approve) {
        let required: Vec<String> = f.required.iter().map(|s| (*s).to_string()).collect();
        let open: Vec<FindingRef> = f.open_blocking.iter().map(open_from).collect();
        let dismissed: Vec<FindingRef> =
            decision.dismiss.iter().map(|d| d.finding.clone()).collect();
        let inputs = ApprovalInputs {
            rows: &rows,
            required: &required,
            effective_reviewers: f.effective_reviewers,
            generation: f.generation,
            current_patch_id: f.current_patch_id,
            threshold_reached: f.threshold_reached,
            final_intervention: f.final_intervention,
            open_blocking: &open,
            dismissed: &dismissed,
        };
        if let Err(conds) = approval_eligibility(&inputs) {
            return Outcome::Approval(conds.iter().map(|c| format!("{c:?}")).collect());
        }
    }
    Outcome::Accepted
}

fn evaluate(f: &IncidentFixture, case: &IncidentCase) -> Outcome {
    evaluate_text(f, &fence(case.block), case.known)
}

fn expect_matches(expect: Expect, got: &Outcome) -> bool {
    match (expect, got) {
        (Expect::Accept, Outcome::Accepted) => true,
        (Expect::Parse(n), Outcome::Parse(m))
        | (Expect::Preconditions(n), Outcome::Preconditions(m))
        | (Expect::Head(n), Outcome::Head(m)) => m.contains(n),
        (Expect::Approval(n), Outcome::Approval(conds)) => conds.iter().any(|c| c.contains(n)),
        _ => false,
    }
}

/// Check one fixture end to end: acceptable answers accepted, rejected answers refused for the
/// named reason, the known-wrong answer schema-valid, and the effect-level `must_not_rerun` set
/// excluded from the eligible rows. Panics with the fixture id in the message on any failure.
fn check_fixture(f: &IncidentFixture) {
    for case in f.acceptable {
        assert_eq!(
            evaluate(f, case),
            Outcome::Accepted,
            "{}: acceptable case `{}` must be accepted",
            f.id,
            case.label
        );
    }
    for case in f.rejected {
        let got = evaluate(f, case);
        assert!(
            expect_matches(case.expect, &got),
            "{}: rejected case `{}` must be refused as {:?}, got {got:?}",
            f.id,
            case.label,
            case.expect
        );
    }
    if let Some(case) = &f.known_wrong {
        let got = evaluate(f, case);
        assert!(
            matches!(got, Outcome::Accepted),
            "{}: the known-wrong answer `{}` must be schema-valid (a tier-3 miss, not a parse \
             failure); got {got:?}",
            f.id,
            case.label
        );
    }
    let rows: Vec<ManagerReviewRow> = f.rows.iter().map(IncidentRow::to_row).collect();
    let eligible = eligible_rows(&rows, f.generation, f.current_patch_id);
    for name in f.must_not_rerun {
        assert!(
            !eligible.iter().any(|r| r.reviewer == *name),
            "{}: the acceptable effect must NOT re-request `{name}` (effect-level rejection)",
            f.id
        );
    }
}

macro_rules! tier2_test {
    ($name:ident, $id:literal) => {
        #[test]
        fn $name() {
            let f = FIXTURES.iter().find(|f| f.id == $id).expect("fixture");
            check_fixture(f);
        }
    };
}

tier2_test!(tier2_f1_unread_head_before_threshold, "F1");
tier2_test!(tier2_f2_unread_current_patch, "F2");
tier2_test!(tier2_f3_unread_current_patch, "F3");
tier2_test!(tier2_f4_unread_head_with_a_note, "F4");
tier2_test!(tier2_f5_only_sol_is_rerequested, "F5");
tier2_test!(tier2_f6_identical_patch_id_approves, "F6");
tier2_test!(tier2_f7_route_names_every_open_revision, "F7");
tier2_test!(tier2_f8_stale_head_is_refused, "F8");
tier2_test!(tier2_f9_reintroduction_approval_survives, "F9");

/// All nine fixtures pass tier 2, with unique ids and the §15.2 shape (every fixture has at least
/// one acceptable answer, and F8 alone carries a known-wrong).
#[test]
fn all_nine_incident_fixtures_pass_tier_2() {
    assert_eq!(FIXTURES.len(), 9, "F1–F9");
    for (i, f) in FIXTURES.iter().enumerate() {
        assert!(
            FIXTURES[..i].iter().all(|prev| prev.id != f.id),
            "fixture ids must be unique: {}",
            f.id
        );
        assert!(
            !f.acceptable.is_empty(),
            "{} must carry an acceptable answer",
            f.id
        );
        check_fixture(f);
    }
    assert_eq!(
        FIXTURES.iter().filter(|f| f.known_wrong.is_some()).count(),
        1,
        "only F8 carries a known-wrong answer"
    );
    assert!(
        FIXTURES
            .iter()
            .find(|f| f.id == "F8")
            .is_some_and(|f| f.known_wrong.is_some()),
        "F8's human action is the known-wrong answer"
    );
}

// ---------------------------------------------------------------------------------------------
// Tier 3 — the replay harness (§15.3): REPORT ONLY, never gating
// ---------------------------------------------------------------------------------------------

/// A source of manager proposals. The harness is report-only: no proposer's answer can fail a
/// build, and the only proposer CI runs is deterministic.
trait Proposer {
    /// The manager's final-message text for `fixture`, given the rendered case packet; `None` when
    /// the proposal could not be produced.
    fn propose(&self, fixture: &IncidentFixture, case_packet: &str) -> Option<String>;
}

/// Render the fixture's evidence as the case packet a manager run would receive (§7.2).
fn case_packet(f: &IncidentFixture) -> String {
    let rows: Vec<CaseRow> = f
        .rows
        .iter()
        .map(|r| CaseRow {
            reviewer: r.reviewer.to_string(),
            status: r.status.to_string(),
            requested_sha: String::new(),
            last_completed_generation: r.generation,
            last_completed_sha: r.sha.to_string(),
            last_completed_patch_id: r.patch_id.to_string(),
            last_completed_verdict: r.verdict.to_string(),
        })
        .collect();
    ManagerCasePacket {
        pr: format!("makewhatis/rhapsody#{}", f.pr),
        ticket: "STUDIO-1".to_string(),
        generation: f.generation,
        evidence_rev: 1,
        stall_kinds: vec![],
        is_final: f.final_intervention,
        interventions_used: 0,
        rounds: if f.threshold_reached { 1 } else { 0 },
        rows,
    }
    .render()
}

/// The variant an incident case names, so a proposal can be matched against it.
fn case_variant(f: &IncidentFixture, case: &IncidentCase) -> Option<&'static str> {
    let raw_known = case.known.unwrap_or(f.findings);
    let known: Vec<KnownFinding> = raw_known.iter().map(KnownFindingRaw::to_known).collect();
    parse_decision(&fence(case.block), &known)
        .ok()
        .map(|d| d.variant())
}

/// The score for one fixture's proposal.
#[derive(Debug, PartialEq, Eq)]
enum ProposalScore {
    /// The pinned acceptable answer, deterministically accepted.
    Accepted,
    /// The known-wrong answer (F8's human action): a judgment miss, never an acceptance.
    KnownWrong,
    /// Deterministically refused (schema-invalid, wrong head, or a failed precondition/condition).
    Refused(String),
    /// Schema-valid and accepted, but not the pinned answer.
    Unmapped,
    /// No proposal was produced.
    Missing,
}

fn describe(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Accepted => "accepted".to_string(),
        Outcome::Parse(m) => format!("parse: {m}"),
        Outcome::Preconditions(m) => format!("preconditions: {m}"),
        Outcome::Head(m) => format!("head: {m}"),
        Outcome::Approval(c) => format!("approval: {c:?}"),
    }
}

/// Score one proposal against a fixture: parse it, match its variant against the acceptable and
/// known-wrong answers, and confirm the acceptable answer is deterministically accepted.
fn score_proposal(f: &IncidentFixture, text: Option<&str>) -> ProposalScore {
    let Some(text) = text else {
        return ProposalScore::Missing;
    };
    let known: Vec<KnownFinding> = f.findings.iter().map(KnownFindingRaw::to_known).collect();
    let decision = match parse_decision(text, &known) {
        Ok(d) => d,
        Err(e) => return ProposalScore::Refused(format!("{e:?}")),
    };
    let variant = decision.variant();
    let acceptable = f
        .acceptable
        .iter()
        .filter_map(|c| case_variant(f, c))
        .collect::<Vec<_>>();
    if acceptable.contains(&variant) {
        return match evaluate_text(f, text, None) {
            Outcome::Accepted => ProposalScore::Accepted,
            other => ProposalScore::Refused(describe(&other)),
        };
    }
    if f.known_wrong
        .as_ref()
        .is_some_and(|kw| case_variant(f, kw) == Some(variant))
    {
        return ProposalScore::KnownWrong;
    }
    match evaluate_text(f, text, None) {
        Outcome::Accepted => ProposalScore::Unmapped,
        other => ProposalScore::Refused(describe(&other)),
    }
}

struct FixtureScore {
    id: &'static str,
    outcome: ProposalScore,
}

/// A tier-3 score report. It is rendered, never asserted on (except by the deterministic tests
/// that prove the scorer works).
struct ScoreReport {
    scores: Vec<FixtureScore>,
}

impl ScoreReport {
    fn accepted(&self) -> usize {
        self.scores
            .iter()
            .filter(|s| s.outcome == ProposalScore::Accepted)
            .count()
    }

    fn known_wrong(&self) -> usize {
        self.scores
            .iter()
            .filter(|s| s.outcome == ProposalScore::KnownWrong)
            .count()
    }

    fn render(&self) -> String {
        let mut out = String::new();
        out.push_str("tier-3 replay score report\n");
        out.push_str(&format!(
            "  acceptable: {} / {}\n",
            self.accepted(),
            self.scores.len()
        ));
        out.push_str(&format!("  known-wrong: {}\n", self.known_wrong()));
        for s in &self.scores {
            out.push_str(&format!("  {}: {:?}\n", s.id, s.outcome));
        }
        out
    }
}

fn run_harness<P: Proposer>(proposer: &P) -> ScoreReport {
    let scores = FIXTURES
        .iter()
        .map(|f| {
            let packet = case_packet(f);
            let proposal = proposer.propose(f, &packet);
            FixtureScore {
                id: f.id,
                outcome: score_proposal(f, proposal.as_deref()),
            }
        })
        .collect();
    ScoreReport { scores }
}

/// The perfect proposer: always the fixture's pinned acceptable answer. Proves the scorer without
/// a live model.
struct OracleProposer;

impl Proposer for OracleProposer {
    fn propose(&self, f: &IncidentFixture, _case_packet: &str) -> Option<String> {
        f.acceptable.first().map(|c| fence(c.block))
    }
}

/// The oracle scores every fixture acceptable and never counts a known-wrong.
#[test]
fn the_replay_harness_scores_the_oracle_perfectly() {
    let report = run_harness(&OracleProposer);
    assert_eq!(
        report.accepted(),
        FIXTURES.len(),
        "the oracle must score all nine: {}",
        report.render()
    );
    assert_eq!(report.known_wrong(), 0, "{}", report.render());
}

/// A proposer that gives F8's known-wrong human answer where the correct answer was
/// `RERUN_REVIEW`: the harness counts it a known-wrong and does NOT count it acceptable.
#[test]
fn the_replay_harness_counts_the_f8_human_answer_as_known_wrong() {
    struct HumanAnswer;
    impl Proposer for HumanAnswer {
        fn propose(&self, f: &IncidentFixture, _case_packet: &str) -> Option<String> {
            if f.id == "F8" {
                f.known_wrong.as_ref().map(|c| fence(c.block))
            } else {
                f.acceptable.first().map(|c| fence(c.block))
            }
        }
    }
    let report = run_harness(&HumanAnswer);
    assert_eq!(report.known_wrong(), 1, "{}", report.render());
    assert_eq!(
        report.accepted(),
        FIXTURES.len() - 1,
        "F8's human answer is not an acceptance: {}",
        report.render()
    );
}

/// The rendered report names every fixture and both counts.
#[test]
fn the_score_report_renders_every_fixture() {
    let rendered = run_harness(&OracleProposer).render();
    assert!(rendered.contains("acceptable: 9 / 9"), "{rendered}");
    assert!(rendered.contains("known-wrong: 0"), "{rendered}");
    for f in FIXTURES {
        assert!(
            rendered.contains(f.id),
            "report must name {}: {rendered}",
            f.id
        );
    }
}

/// The live proposer: one manager turn per fixture on the pinned model. **Operator-run only** —
/// CI must never depend on a live model choosing one exact answer, so this is `#[ignore]`d and
/// prints the report rather than asserting on it.
///
/// Set `STUDIO_1019_MANAGER_CLI` to a command that reads the case packet on stdin and writes the
/// manager's final message to stdout (default: `claude -p --model claude-opus-5-5`):
///
/// ```text
/// STUDIO_1019_MANAGER_CLI="claude -p --model claude-opus-5-5" \
///   cargo test -p rhapsody-orchestrator the_live_replay_harness_reports_a_score \
///   -- --ignored --nocapture
/// ```
struct LiveManagerProposer {
    argv: Vec<String>,
}

impl Proposer for LiveManagerProposer {
    fn propose(&self, _f: &IncidentFixture, case_packet: &str) -> Option<String> {
        use std::io::Write;
        let (program, args) = self.argv.split_first()?;
        let mut child = std::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok()?;
        child.stdin.take()?.write_all(case_packet.as_bytes()).ok()?;
        let out = child.wait_with_output().ok()?;
        if !out.status.success() {
            return None;
        }
        Some(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

#[test]
#[ignore = "tier 3 REPORTS; CI must not depend on a live model"]
fn the_live_replay_harness_reports_a_score() {
    let argv: Vec<String> = std::env::var("STUDIO_1019_MANAGER_CLI")
        .unwrap_or_else(|_| "claude -p --model claude-opus-5-5".to_string())
        .split_whitespace()
        .map(str::to_string)
        .collect();
    let report = run_harness(&LiveManagerProposer { argv });
    eprintln!("{}", report.render());
}

// ---------------------------------------------------------------------------------------------
// Tier 4 — the §15.4 acceptance / failure-injection matrix
// ---------------------------------------------------------------------------------------------

/// One §15.4 failure-injection case and the test that fails when its protection is removed.
///
/// This matrix CONSUMES the earlier manager tickets' tests (the ticket: "This ticket consumes
/// earlier tickets' evidence. It doesn't re-implement their tests."). `file` is relative to the
/// workspace root; the guard test below proves every named test still exists, so an entry cannot
/// silently rot to a missing or renamed test.
struct AcceptanceCase {
    group: &'static str,
    case: &'static str,
    test: &'static str,
    file: &'static str,
}

const GROUPS: &[&str] = &[
    "budgets_and_serialization",
    "approval_eligibility",
    "dismissal",
    "merge_freshness",
    "activation_and_delivery",
    "activation_boundary",
    "waking_the_author",
    "exchange_accounting",
    "mandatory_explanations",
    "one_contract",
    "startup_boundary",
    "modes_gates_memory_off",
];

const ACCEPTANCE_MATRIX: &[AcceptanceCase] = &[
    // --- budgets and serialization ---
    AcceptanceCase {
        group: "budgets_and_serialization",
        case: "the same stall recurs after exhausted: stopped, nothing created, stays on the feed",
        test: "a_repeated_stall_after_exhausted_creates_nothing_new",
        file: "crates/orchestrator/src/managerintervention.rs",
    },
    AcceptanceCase {
        group: "budgets_and_serialization",
        case: "repeated apply_failed: the first stops the generation",
        test: "a_repeated_apply_failed_creates_nothing_new",
        file: "crates/orchestrator/src/managerintervention.rs",
    },
    AcceptanceCase {
        group: "budgets_and_serialization",
        case: "two stall kinds arrive together: one intervention, both kinds merged",
        test: "two_stall_kinds_produce_one_intervention",
        file: "crates/orchestrator/src/managerintervention.rs",
    },
    AcceptanceCase {
        group: "budgets_and_serialization",
        case: "restart after a terminal failure: manager_stopped persists",
        test: "a_stopped_generation_survives_a_restart",
        file: "crates/store/src/sqlite.rs",
    },
    AcceptanceCase {
        group: "budgets_and_serialization",
        case: "two interventions compete for the final allocation: impossible by the unique index",
        test: "only_one_active_intervention_per_pr_across_stall_kinds",
        file: "crates/store/src/sqlite.rs",
    },
    AcceptanceCase {
        group: "budgets_and_serialization",
        case: "generation run budget: the 13th launch is refused",
        test: "the_thirteenth_launch_is_refused_and_stops_the_generation",
        file: "crates/store/src/sqlite.rs",
    },
    // --- approval eligibility ---
    AcceptanceCase {
        group: "approval_eligibility",
        case: "APPROVE before the threshold: rejected (condition 0)",
        test: "f1_before_threshold",
        file: "crates/orchestrator/src/managerdecision.rs",
    },
    AcceptanceCase {
        group: "approval_eligibility",
        case: "the manager approval is never counted in the reviewer set",
        test: "the_manager_row_never_satisfies_quorum",
        file: "crates/orchestrator/src/managerdecision.rs",
    },
    AcceptanceCase {
        group: "approval_eligibility",
        case: "below quorum or no reviewer rows: APPROVE rejected (condition 1)",
        test: "an_empty_set_is_never_approvable",
        file: "crates/orchestrator/src/managerdecision.rs",
    },
    AcceptanceCase {
        group: "approval_eligibility",
        case: "a reviewer added or dropped after approval: the approval expires",
        test: "every_expiry_trigger_expires_the_approval",
        file: "crates/orchestrator/src/managerapproval.rs",
    },
    // --- dismissal ---
    AcceptanceCase {
        group: "dismissal",
        case: "a dismissed finding raised again after a change to its paths: open",
        test: "reopen_on_paths_changed",
        file: "crates/orchestrator/src/reviewfindings.rs",
    },
    AcceptanceCase {
        group: "dismissal",
        case: "new_evidence true at the same head: open",
        test: "reopen_opens_on_new_evidence_at_the_same_head",
        file: "crates/orchestrator/src/reviewfindings.rs",
    },
    AcceptanceCase {
        group: "dismissal",
        case: "a later unstructured review with a different objection: new id, blocking",
        test: "two_unstructured_reviews_get_distinct_ids",
        file: "crates/orchestrator/src/reviewfindings.rs",
    },
    AcceptanceCase {
        group: "dismissal",
        case: "a dismissed objection repeated unchanged at the same patch: settled",
        test: "reopen_settles_an_unchanged_repeat_at_the_same_patch",
        file: "crates/orchestrator/src/reviewfindings.rs",
    },
    // --- merge freshness ---
    AcceptanceCase {
        group: "merge_freshness",
        case: "a same-head blocking review after approval but before the merge request: not requested",
        test: "a_manager_approval_that_lapsed_before_the_merge_command_is_not_requested",
        file: "crates/orchestrator/src/runautomerge.rs",
    },
    AcceptanceCase {
        group: "merge_freshness",
        case: "a hold after the merge was planned: the merge isn't requested",
        test: "the_recheck_refuses_anything_but_a_still_current_effective_approval",
        file: "crates/orchestrator/src/managerapproval.rs",
    },
    AcceptanceCase {
        group: "merge_freshness",
        case: "an authority change during multi-effect application: nothing further is applied",
        test: "a_revoked_decision_mid_effects_reports_done_and_cancelled",
        file: "crates/orchestrator/src/runmanagerapply.rs",
    },
    AcceptanceCase {
        group: "merge_freshness",
        case: "the head moves during the final merge request: --match-head-commit rejects it",
        test: "merge_pr_pins_the_head_commit_when_one_is_given",
        file: "crates/orchestrator/src/ghsummons.rs",
    },
    // --- activation and delivery ---
    AcceptanceCase {
        group: "activation_and_delivery",
        case: "the approval is recorded locally but the PR comment is rejected: never effective",
        test: "an_approve_whose_explanation_failed_never_becomes_effective",
        file: "crates/orchestrator/src/managerintervention.rs",
    },
    AcceptanceCase {
        group: "activation_and_delivery",
        case: "a crash after the comment succeeded but before the local acknowledgement: recovery revalidates",
        test: "a_restart_after_the_comment_revalidates_and_refuses",
        file: "crates/orchestrator/src/managerintervention.rs",
    },
    AcceptanceCase {
        group: "activation_and_delivery",
        case: "a timed-out comment that completes late: reconciled by its marker, harmless",
        test: "a_late_comment_is_reconciled_by_its_marker",
        file: "crates/orchestrator/src/runmanagerapply.rs",
    },
    AcceptanceCase {
        group: "activation_and_delivery",
        case: "authority revoked while mandatory effects are pending: pending records cancelled",
        test: "an_authority_change_between_request_and_ack_refuses_activation",
        file: "crates/orchestrator/src/managerintervention.rs",
    },
    // --- activation boundary ---
    AcceptanceCase {
        group: "activation_boundary",
        case: "a hold after the explanation request but before its acknowledgement: superseded",
        test: "a_hold_between_request_and_ack_refuses_activation",
        file: "crates/orchestrator/src/managerintervention.rs",
    },
    AcceptanceCase {
        group: "activation_boundary",
        case: "a same-head blocking review in that interval: stale",
        test: "an_approve_refuses_activation_after_a_same_head_blocking_review",
        file: "crates/orchestrator/src/managerintervention.rs",
    },
    AcceptanceCase {
        group: "activation_boundary",
        case: "authority changes in that interval: superseded",
        test: "an_authority_change_between_request_and_ack_refuses_activation",
        file: "crates/orchestrator/src/managerintervention.rs",
    },
    AcceptanceCase {
        group: "activation_boundary",
        case: "a restart after a successful comment, before activation: recovery refuses",
        test: "a_restart_after_the_comment_revalidates_and_refuses",
        file: "crates/orchestrator/src/managerintervention.rs",
    },
    AcceptanceCase {
        group: "activation_boundary",
        case: "a decision's own star effects never invalidate it",
        test: "a_marked_comment_or_ticket_move_is_not_evidence",
        file: "crates/orchestrator/src/reviewevidence.rs",
    },
    // --- waking the author ---
    AcceptanceCase {
        group: "waking_the_author",
        case: "the comment succeeds but the ticket move fails: no activation, no wake",
        test: "a_failed_ticket_move_writes_no_wake_obligation",
        file: "crates/orchestrator/src/managerintervention.rs",
    },
    AcceptanceCase {
        group: "waking_the_author",
        case: "a manager comment observed before activation: no effect",
        test: "no_wake_obligation_exists_before_activation",
        file: "crates/orchestrator/src/managerintervention.rs",
    },
    AcceptanceCase {
        group: "waking_the_author",
        case: "a hold before admission: the obligation is refused",
        test: "a_hold_before_admission_refuses_the_wake",
        file: "crates/orchestrator/src/managerwake.rs",
    },
    AcceptanceCase {
        group: "waking_the_author",
        case: "a generation change before admission: refused",
        test: "a_generation_change_before_admission_refuses_the_wake",
        file: "crates/orchestrator/src/managerwake.rs",
    },
    AcceptanceCase {
        group: "waking_the_author",
        case: "a forged rhapsody-manager marker: no manager-authorized dispatch",
        test: "a_manager_marker_grants_no_summons_of_its_own",
        file: "crates/orchestrator/src/ghsummons.rs",
    },
    AcceptanceCase {
        group: "waking_the_author",
        case: "a crash between admission and delivery: back to pending, woken once",
        test: "a_crash_between_admission_and_delivery_returns_the_wake_to_pending",
        file: "crates/orchestrator/src/managerwake.rs",
    },
    AcceptanceCase {
        group: "waking_the_author",
        case: "ordinary selection skips a ticket with a pending wake",
        test: "selection_skips_a_ticket_with_an_unspent_wake",
        file: "crates/orchestrator/src/managerwake.rs",
    },
    // --- exchange accounting ---
    AcceptanceCase {
        group: "exchange_accounting",
        case: "created before the threshold, activated after it: charged post-threshold; at N-1 stale and re-planned final",
        test: "a_threshold_crossing_at_the_last_slot_stales_a_non_final_runtime_decision",
        file: "crates/orchestrator/src/managerdecision.rs",
    },
    AcceptanceCase {
        group: "exchange_accounting",
        case: "the threshold is crossed while an exchange is in flight: it completes unauthorized",
        test: "an_exchange_in_flight_at_the_crossing_completes_without_an_authorization",
        file: "crates/orchestrator/src/reviewwatch.rs",
    },
    AcceptanceCase {
        group: "exchange_accounting",
        case: "after the threshold, the arming paths fail without an authorization; a retry consumes nothing",
        test: "a_retry_consumes_no_authorization",
        file: "crates/orchestrator/src/managerexchange.rs",
    },
    AcceptanceCase {
        group: "exchange_accounting",
        case: "a restart preserves both budgets, outstanding authorizations and wake obligations",
        test: "manager_exchanges_round_trip_and_invalidation_spares_terminal_rows",
        file: "crates/store/src/sqlite.rs",
    },
    AcceptanceCase {
        group: "exchange_accounting",
        case: "a final intervention, an APPROVE or an ESCALATE creates no authorization",
        test: "a_final_intervention_approve_and_escalate_write_no_exchange",
        file: "crates/orchestrator/src/managerintervention.rs",
    },
    AcceptanceCase {
        group: "exchange_accounting",
        case: "operator actions still work after the threshold (a person's summons is not gated)",
        test: "an_operator_dispatch_is_not_gated_after_the_threshold",
        file: "crates/orchestrator/src/managerexchange.rs",
    },
    // --- mandatory explanations ---
    AcceptanceCase {
        group: "mandatory_explanations",
        case: "RERUN_REVIEW with dismissals and no note: the explanation is still required",
        test: "a_rerun_with_dismissals_and_no_note_still_posts_the_explanation",
        file: "crates/orchestrator/src/managerintervention.rs",
    },
    AcceptanceCase {
        group: "mandatory_explanations",
        case: "the explanation fails to post: dismissals stay ineffective",
        test: "a_failed_explanation_leaves_the_dismissals_ineffective",
        file: "crates/orchestrator/src/managerintervention.rs",
    },
    AcceptanceCase {
        group: "mandatory_explanations",
        case: "the explanation succeeds but revalidation fails at activation: dismissals ineffective",
        test: "a_hold_between_request_and_ack_refuses_activation",
        file: "crates/orchestrator/src/managerintervention.rs",
    },
    // --- one contract ---
    AcceptanceCase {
        group: "one_contract",
        case: "the null field rule is tested directly against the table",
        test: "rejects_null_for_a_field_that_does_not_apply",
        file: "crates/orchestrator/src/managerdecision.rs",
    },
    AcceptanceCase {
        group: "one_contract",
        case: "the design's example block parses",
        test: "the_design_example_block_parses",
        file: "crates/orchestrator/src/managerdecision.rs",
    },
    AcceptanceCase {
        group: "one_contract",
        case: "the F6 rejection (RERUN with no eligible rows) is tested directly",
        test: "f6_identical_patch_id_is_approved",
        file: "crates/orchestrator/src/managerdecision.rs",
    },
    AcceptanceCase {
        group: "one_contract",
        case: "the F9 rejection (RERUN with no eligible rows) is tested directly",
        test: "f9_reintroduction_approval_survives",
        file: "crates/orchestrator/src/managerdecision.rs",
    },
    AcceptanceCase {
        group: "one_contract",
        case: "per-decision revalidation is tested directly against the table",
        test: "supersession_covers_every_section_8_1_cause",
        file: "crates/orchestrator/src/managerdecision.rs",
    },
    // --- startup boundary ---
    AcceptanceCase {
        group: "startup_boundary",
        case: "a repository startup hook is never executed: no repository in the run's cwd",
        test: "a_manager_attempt_provisions_an_empty_cwd_and_removes_it",
        file: "crates/orchestrator/src/worker.rs",
    },
    AcceptanceCase {
        group: "startup_boundary",
        case: "an inherited plugin or MCP server exposing a mutation: not loaded",
        test: "the_canary_init_posture_is_parsed_and_checked",
        file: "crates/orchestrator/src/managerselftest.rs",
    },
    AcceptanceCase {
        group: "startup_boundary",
        case: "a symlink targeting a credential file: manager_file returns the link text, never the target",
        test: "a_manager_file_returns_a_symlink_as_its_text",
        file: "crates/orchestrator/src/managerread.rs",
    },
    AcceptanceCase {
        group: "startup_boundary",
        case: "the installed CLI doesn't honour the flags: the self-test fails and the manager is disabled",
        test: "a_failed_self_test_disables_the_manager_and_a_pass_does_not",
        file: "crates/orchestrator/src/managerselftest.rs",
    },
    AcceptanceCase {
        group: "startup_boundary",
        case: "Bash, Read, WebFetch and every other built-in: refused",
        test: "all_attempts_refused_passes",
        file: "crates/orchestrator/src/managerselftest.rs",
    },
    AcceptanceCase {
        group: "startup_boundary",
        case: "unregistered MCP write tools: refused",
        test: "the_canary_report_is_parsed_strictly",
        file: "crates/orchestrator/src/managerselftest.rs",
    },
    AcceptanceCase {
        group: "startup_boundary",
        case: "the environment has no GH_TOKEN, GITHUB_TOKEN or tracker key",
        test: "scrub_child_env_manager_drops_gh_tokens",
        file: "crates/orchestrator/src/preflight.rs",
    },
    AcceptanceCase {
        group: "startup_boundary",
        case: "a secret-shaped string in the rationale: the block is rejected",
        test: "rejects_secret_shaped_posted_text",
        file: "crates/orchestrator/src/managerdecision.rs",
    },
    // --- modes, gates, memory, off ---
    AcceptanceCase {
        group: "modes_gates_memory_off",
        case: "advise causes no effect and consumes no live budget",
        test: "shadow_runs_do_not_consume_the_live_run_budget",
        file: "crates/store/src/sqlite.rs",
    },
    AcceptanceCase {
        group: "modes_gates_memory_off",
        case: "a red-CI, draft or conflicted PR with an effective manager approval doesn't merge (D7)",
        test: "a_d7_gate_blocks_a_manager_approved_merge",
        file: "crates/orchestrator/src/runautomerge.rs",
    },
    AcceptanceCase {
        group: "modes_gates_memory_off",
        case: "a hold, drain, exhausted budget or failed credentials prevent or defer a launch, at every retry",
        test: "gates_are_honoured_on_every_retry",
        file: "crates/orchestrator/src/managerintervention.rs",
    },
    AcceptanceCase {
        group: "modes_gates_memory_off",
        case: "a memory failure leaves the decision applied, doesn't repeat an effect, memory_state pending",
        test: "a_memory_failure_leaves_the_decision_applied_and_pending",
        file: "crates/orchestrator/src/managerintervention.rs",
    },
    AcceptanceCase {
        group: "modes_gates_memory_off",
        case: "review_authority: off routes nothing (the byte-identical path)",
        test: "off_routes_nothing",
        file: "crates/orchestrator/src/managerintervention.rs",
    },
];

/// Every §15.4 case's named test exists in its named file. This is what keeps the matrix from
/// silently rotting when a test is renamed or deleted: an entry whose test is gone reds here.
#[test]
fn every_acceptance_case_names_a_test_that_exists() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
    let mut missing: Vec<String> = Vec::new();
    for c in ACCEPTANCE_MATRIX {
        let path = format!("{root}/{}", c.file);
        let Ok(src) = std::fs::read_to_string(&path) else {
            missing.push(format!("{}: cannot read {}", c.test, c.file));
            continue;
        };
        if !src.contains(&format!("fn {}(", c.test)) {
            missing.push(format!("{}: not found in {}", c.test, c.file));
        }
    }
    assert!(
        missing.is_empty(),
        "§15.4 matrix references tests that do not exist:\n{}",
        missing.join("\n")
    );
}

/// Every §15.4 group is represented in the matrix.
#[test]
fn the_matrix_covers_every_acceptance_group() {
    for group in GROUPS {
        assert!(
            ACCEPTANCE_MATRIX.iter().any(|c| c.group == *group),
            "no §15.4 case maps to group `{group}`"
        );
    }
    // No stray group names.
    for c in ACCEPTANCE_MATRIX {
        assert!(
            GROUPS.contains(&c.group),
            "matrix entry `{}` names an unknown group `{}`",
            c.case,
            c.group
        );
    }
}

/// A guard on the guard: the existence check actually catches a nonsense test name.
#[test]
fn the_matrix_guard_detects_a_missing_test() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
    let path = format!("{root}/crates/orchestrator/src/managerdecision.rs");
    let src = std::fs::read_to_string(&path).expect("read managerdecision.rs");
    assert!(
        !src.contains("fn this_test_does_not_exist_anywhere("),
        "the guard's needle must not appear"
    );
}
