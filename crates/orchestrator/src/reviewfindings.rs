//! reviewfindings — the REVIEWER OUTPUT CONTRACT and the durable finding revisions it produces
//! (STUDIO-1008, the manager-agent program's first ticket; design record
//! `~/.rhapsody/docs/manager-agent-design.md` §5.3 and §6.3).
//!
//! **No Go counterpart.** The frozen Symphony reference has no review feature at all, so nothing
//! here is a port; it is the additive Rhapsody surface the design record specifies, and every
//! production caller sits behind the ticketless-review gate (a review run only exists when Teams is
//! on and `review.mode: ticketless`).
//!
//! # What this module owns
//!
//! Today a review's verdict is a status per (reviewer, sha) plus free text: nothing downstream can
//! tell which objection is which, whether it was addressed, or whether a later review is raising the
//! same thing again. This module is the structured half of the fix. It owns exactly four pure pieces:
//!
//! 1. [`parse_verdict_block`] — reads the one machine-readable verdict block out of a review run's
//!    final message. A run with no block (or an unparseable one) is not an error: it becomes the
//!    UNSTRUCTURED fallback in [`plan_review_findings`].
//! 2. [`plan_review_findings`] — turns a parsed block (or the fallback) into the finding REVISIONS
//!    to persist, and decides whether a later approving review resolves the reviewer's open ones.
//!    This is the whole of §5.3's write side; [`crate::review`] applies the plan to the store.
//! 3. [`summary_hash`] / [`normalize_summary`] — the digest the reopen rule compares. The row stores
//!    a digest and never the reviewer's prose.
//! 4. [`reopen_decision`] — §6.3's rule for whether a re-raised DISMISSED finding is `settled` or
//!    `open`. It is a pure function over declared flags, the summary digest and paths: a MECHANICAL
//!    decision, never a semantic judgment.
//!
//! Nothing here writes `dismissed` and nothing here implements dismissal itself — the manager's
//! decisions do, in a later ticket (M3/M9). [`reopen_decision`] is defined and tested now, with
//! synthetic dismissals, because §5.3's row shape and §6.3's rule are what the later tickets build
//! on.
//!
//! # Two things deliberately NOT trusted
//!
//! * The parsed block's `approve` flag never overrides a recorded `changes` verdict. [`crate::review`]
//!   derives the effective verdict from BOTH and takes the conservative reading on disagreement, the
//!   direction STUDIO-894 established for a recorded verdict that contradicted the review text.
//! * A finding id is SCOPED per reviewer (`sol:B8`), and the unstructured fallback's id embeds the
//!   review's run id (`sol:unstructured:<run_id>`). A stable unstructured id would let one dismissal
//!   silence every later, unrelated objection that reviewer made.

use rhapsody_store::{
    REVIEW_FINDING_DISMISSED, REVIEW_FINDING_OPEN, REVIEW_FINDING_SETTLED, ReviewFindingRow,
};
use sha2::{Digest, Sha256};

/// The info string of the fenced block every review run's final message must end with. The manager's
/// own decision block is tagged `rhapsody-manager-decision` (§6.1); this is its reviewer-side
/// sibling.
pub const REVIEW_VERDICT_TAG: &str = "rhapsody-review-verdict";

/// The generation M1 wrote onto every finding revision, before M2 introduced the real value
/// (STUDIO-1009; §5.1). It is retained as the value the migration's backfill replaces and as a
/// documented constant, not as a value any production path writes any more: the completion path
/// records the pull request's real generation, read from `rhapsody_review_bound`.
pub const FINDING_GENERATION_PLACEHOLDER: i64 = 0;

/// The synthetic summary an unstructured review carries into [`summary_hash`]. It is a fixed literal
/// so two unstructured reviews hash the same — but their ids differ (see
/// [`unstructured_finding_id`]), so the equal digest never merges them.
const UNSTRUCTURED_SUMMARY: &str = "unstructured review";

/// One finding as the reviewer declared it in the verdict block.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BlockFinding {
    /// The reviewer's own id for this finding (`B8`), scoped per reviewer downstream.
    pub id: String,
    /// Whether the reviewer says it blocks. Required in the block; see [`parse_verdict_block`].
    pub blocking: bool,
    /// The reviewer's one-line summary. Only its digest is stored.
    pub summary: String,
    /// The files the finding is about. Empty means UNSCOPED (§6.3).
    pub paths: Vec<String>,
    /// The reviewer declares materially new evidence.
    pub new_evidence: bool,
    /// The reviewer declares a regression.
    pub regression: bool,
}

/// The parsed `rhapsody-review-verdict` block.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReviewVerdictBlock {
    /// The reviewer's own verdict. Never given authority over a contradicting recorded verdict.
    pub approve: bool,
    /// The findings the reviewer raised. May be empty (a `changes` verdict with no findings is
    /// recorded as the unstructured fallback).
    pub findings: Vec<BlockFinding>,
}

/// What a re-raised dismissed finding is recorded as (§6.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindingReopen {
    /// The same objection, decided already: recorded, non-blocking.
    Settled,
    /// Materially new — a flag, a changed summary, or changed code on the finding's paths.
    Open,
}

/// The plan [`crate::review`] applies to the store after a completed review.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FindingsPlan {
    /// The finding revisions to record, in block order (or the single fallback).
    pub rows: Vec<ReviewFindingRow>,
    /// Whether this review APPROVED: a later approving review by the same reviewer resolves that
    /// reviewer's open revisions (§5.3). Never true together with a non-empty `rows` — an approving
    /// review resolves, it does not raise.
    pub resolve_open: bool,
}

/// Reads the ONE `rhapsody-review-verdict` fenced block out of a review run's final message.
///
/// Returns `None` — the unstructured fallback — for every shape that is not exactly one well-formed
/// block: no block, more than one, a body that is not valid JSON, an unknown field, a missing
/// required field, or a finding with an empty id or summary. Strictness is the conservative
/// direction here: an ambiguous block is treated as "this reviewer's objection is not machine-
/// readable", which blocks, rather than being guessed at.
///
/// `result_text` is the TAIL of the agent's final message (the `TurnResult::result_text` contract),
/// so the block must end the message for it to be readable at all — which is what the review prompt
/// asks for.
pub fn parse_verdict_block(result_text: &str) -> Option<ReviewVerdictBlock> {
    let blocks = fenced_blocks(result_text, REVIEW_VERDICT_TAG);
    let [body] = blocks.as_slice() else {
        return None; // zero or more than one: ambiguous
    };
    let json: VerdictBlockJson = serde_json::from_str(body).ok()?;
    if json
        .findings
        .iter()
        .any(|f| f.id.trim().is_empty() || f.summary.trim().is_empty())
    {
        return None;
    }
    // DUPLICATE IDS WITHIN ONE VERDICT BLOCK are malformed, not two findings (STUDIO-1009).
    // [`plan_review_findings`] plans each declared finding against the SAME `prior` snapshot, so two
    // entries sharing an id would both compute the same `revision` and the second would collide with
    // the first on the finding table's primary key — one silently dropped, or a nondeterministic
    // winner. Rejecting the whole block is the conservative reading [`parse_verdict_block`] already
    // takes for an ambiguous block: the reviewer's output is not machine-readable, which becomes the
    // blocking unstructured fallback rather than a guessed split.
    let mut ids = std::collections::HashSet::with_capacity(json.findings.len());
    if json
        .findings
        .iter()
        .any(|f| !ids.insert(f.id.trim().to_string()))
    {
        return None;
    }
    Some(ReviewVerdictBlock {
        approve: json.approve,
        findings: json
            .findings
            .into_iter()
            .map(|f| BlockFinding {
                id: f.id,
                blocking: f.blocking,
                summary: f.summary,
                paths: f.paths,
                new_evidence: f.new_evidence,
                regression: f.regression,
            })
            .collect(),
    })
}

/// The `serde` shape of the block's JSON body. `deny_unknown_fields` and required `id`/`blocking`/
/// `summary` are the strictness [`parse_verdict_block`] documents; `paths`, `new_evidence` and
/// `regression` default to empty/false (an unscoped, flagless finding), which is the conservative
/// reading — an unscoped finding can never be settled by a diff that touches none of its paths.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct VerdictBlockJson {
    approve: bool,
    #[serde(default)]
    findings: Vec<FindingJson>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct FindingJson {
    id: String,
    blocking: bool,
    summary: String,
    #[serde(default)]
    paths: Vec<String>,
    #[serde(default)]
    new_evidence: bool,
    #[serde(default)]
    regression: bool,
}

/// Every fenced block in `text` whose info string is exactly `tag`, body included (without the
/// fences). Handles both the backtick and tilde fence styles, because a reviewer's message may use
/// either. A non-matching opening fence is ignored; its closing fence then looks like a bare fence
/// and is likewise ignored, so a stray fence cannot swallow a real block.
///
/// `pub(crate)` because the manager's decision block (STUDIO-1010, `managerdecision`) is the same
/// fence shape under a different tag; two scanners would be free to drift.
pub(crate) fn fenced_blocks(text: &str, tag: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        let trimmed = line.trim();
        let info = trimmed
            .strip_prefix("```")
            .or_else(|| trimmed.strip_prefix("~~~"))
            .map(str::trim);
        if info != Some(tag) {
            continue;
        }
        let mut body = String::new();
        for l in lines.by_ref() {
            let t = l.trim();
            if t == "```" || t == "~~~" {
                break;
            }
            body.push_str(l);
            body.push('\n');
        }
        out.push(body);
    }
    out
}

/// A summary reduced to the form the digest is taken over: leading/trailing whitespace removed and
/// every run of whitespace folded to one space.
///
/// Deliberately NOT lowercased. Case-folding would make two different summaries hash the same, and a
/// false "same summary" SETTLES a finding — the direction §6.3 warns against (reopening costs a
/// round; wrongly suppressing a regression costs a bug).
pub fn normalize_summary(summary: &str) -> String {
    summary.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The lowercase-hex SHA-256 of a summary's normalized form — the column the reopen rule compares.
/// The prose itself is never stored.
pub fn summary_hash(summary: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(normalize_summary(summary).as_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// The reviewer-scoped id of a declared finding (`sol:B8`).
pub fn scoped_finding_id(reviewer: &str, id: &str) -> String {
    format!("{reviewer}:{id}")
}

/// The id of an unstructured completed review. Unique per review RUN on purpose: a stable id
/// (`sol:unstructured`) would let one dismissal silence every later, unrelated objection that
/// reviewer made (§5.3).
pub fn unstructured_finding_id(reviewer: &str, run_id: i64) -> String {
    format!("{reviewer}:unstructured:{run_id}")
}

/// The inputs to [`reopen_decision`], bundled because §6.3's rule reads eight values and a positional
/// call would be unreadable (and over clippy's argument ceiling). `Default` is the "nothing known"
/// input, which callers and tests fill selectively.
#[derive(Debug, Default)]
pub struct ReopenInputs<'a> {
    /// The patch-id the finding was DISMISSED at.
    pub dismissed_at_patch_id: &'a str,
    /// The summary digest of the dismissed revision.
    pub dismissed_summary_hash: &'a str,
    /// The dismissed finding's paths (empty means unscoped).
    pub paths: &'a [String],
    /// The re-raise's patch-id (empty when unknown).
    pub new_patch_id: &'a str,
    /// The re-raise's summary digest.
    pub new_summary_hash: &'a str,
    /// The re-raise declares materially new evidence.
    pub new_evidence: bool,
    /// The re-raise declares a regression.
    pub regression: bool,
    /// Whether the diff between the two patch-ids touches any of `paths`. Computed by the caller:
    /// this crate has no part of a diff at M1.
    pub diff_since_dismissal_touches_paths: bool,
}

/// §6.3's rule for whether a re-raised DISMISSED finding is `settled` or `open`.
///
/// The decision is mechanical — declared flags, the summary digest and paths — never a semantic
/// judgment. `inputs.diff_since_dismissal_touches_paths` is supplied by the caller because computing
/// it needs the diff between two patch-ids, which this crate has no part of at M1; the pure rule is
/// what the later tickets consume.
pub fn reopen_decision(inputs: &ReopenInputs<'_>) -> FindingReopen {
    // New evidence or a regression reopens even at the SAME head (§6.3 row 2).
    if inputs.new_evidence
        || inputs.regression
        || inputs.new_summary_hash != inputs.dismissed_summary_hash
    {
        return FindingReopen::Open;
    }
    // Same patch-id, same digest, no flags: a repeat of an objection already decided.
    if inputs.new_patch_id == inputs.dismissed_at_patch_id {
        return FindingReopen::Settled;
    }
    // A different patch-id: the code the finding is about may have changed. Unscoped findings (no
    // paths) are always relevant; a scoped one reopens only when the diff touches one of its paths.
    if inputs.paths.is_empty() || inputs.diff_since_dismissal_touches_paths {
        return FindingReopen::Open;
    }
    FindingReopen::Settled
}

/// The inputs to [`plan_review_findings`]: one completed review, as the exit path observed it.
/// `Default` is the "unstructured, changes" input, which callers and tests fill selectively.
#[derive(Debug, Default)]
pub struct CompletionInputs<'a> {
    /// `owner/repo#number`.
    pub pr: &'a str,
    /// The review generation, read from the pull request's bound (STUDIO-1009; the M1 placeholder
    /// was `0`).
    pub generation: i64,
    /// The reviewing teammate's identity.
    pub reviewer: &'a str,
    /// The review run's `runs.id` (its scoped id for the unstructured fallback).
    pub run_id: i64,
    /// The head SHA the round was pinned to.
    pub head_sha: &'a str,
    /// The patch-id of `head_sha` against the pull request's base (STUDIO-977's stable patch-id,
    /// STUDIO-1009). Empty when the change could not be fingerprinted, which is the same value M1
    /// wrote and is the safe reading: an empty patch-id never matches a recorded one.
    pub head_patch_id: &'a str,
    /// The EFFECTIVE verdict (the reconciler has already reconciled the hand-off with the block).
    pub approved: bool,
    /// The parsed structured block, or `None` for an unstructured review.
    pub block: Option<&'a ReviewVerdictBlock>,
    /// Every finding row already recorded for this pull request.
    pub prior: &'a [ReviewFindingRow],
}

/// Plans the finding revisions a completed review produces, and whether it resolves the reviewer's
/// open revisions.
///
/// * `approved` is the EFFECTIVE verdict ([`crate::review`] reconciles the recorded verdict with the
///   block's `approve` flag first). An approving review resolves every open revision of `reviewer`'s
///   on `pr`/`generation` and raises none.
/// * A `changes` review records one revision per declared finding, incrementing `revision` for a
///   finding id the reviewer has raised before. A re-raise of a DISMISSED revision is recorded
///   `settled` or `open` per [`reopen_decision`]; a re-raise of anything else is `open`.
/// * A `changes` review with NO declared findings — no block at all, or a parsed block that lists
///   none — records the single unstructured fallback, blocking, scoped to this review run. A
///   `changes` verdict must always block something, or the review would resolve nothing and the
///   loop would read a rejection as a no-op.
/// * `prior` is every finding row already recorded for `pr` (any generation/reviewer); this function
///   filters it.
pub fn plan_review_findings(inputs: &CompletionInputs<'_>) -> FindingsPlan {
    let CompletionInputs {
        pr,
        generation,
        reviewer,
        run_id,
        head_sha,
        head_patch_id,
        approved,
        block,
        prior,
    } = *inputs;
    if approved {
        return FindingsPlan {
            rows: Vec::new(),
            resolve_open: true,
        };
    }
    let declared: &[BlockFinding] = block.map(|b| b.findings.as_slice()).unwrap_or(&[]);
    if declared.is_empty() {
        return FindingsPlan {
            rows: vec![ReviewFindingRow {
                pr: pr.to_string(),
                generation,
                reviewer: reviewer.to_string(),
                finding_id: unstructured_finding_id(reviewer, run_id),
                revision: 1,
                review_run_id: run_id,
                raised_at_sha: head_sha.to_string(),
                raised_at_patch_id: head_patch_id.to_string(),
                paths: Vec::new(),
                summary_hash: summary_hash(UNSTRUCTURED_SUMMARY),
                blocking: true,
                new_evidence: false,
                regression: false,
                status: REVIEW_FINDING_OPEN.to_string(),
                resolved_by: String::new(),
                dismissed_by: String::new(),
            }],
            resolve_open: false,
        };
    }
    let rows = declared
        .iter()
        .map(|f| {
            let finding_id = scoped_finding_id(reviewer, &f.id);
            let new_summary_hash = summary_hash(&f.summary);
            let latest = prior
                .iter()
                .filter(|r| {
                    r.pr == pr
                        && r.generation == generation
                        && r.reviewer == reviewer
                        && r.finding_id == finding_id
                })
                .max_by_key(|r| r.revision);
            let revision = latest.map(|r| r.revision + 1).unwrap_or(1);
            let status = match latest {
                None => REVIEW_FINDING_OPEN,
                Some(prev) if prev.status == REVIEW_FINDING_DISMISSED => {
                    // The dismissed revision's patch-id/digest stand in for the dismissal's own
                    // (`dismissed_by` carries the real `dismissed_at_patch_id` once a manager writes
                    // it); at M1 nothing dismisses, so this is exercised by the rule's own tests.
                    match reopen_decision(&ReopenInputs {
                        dismissed_at_patch_id: &prev.raised_at_patch_id,
                        dismissed_summary_hash: &prev.summary_hash,
                        paths: &prev.paths,
                        new_patch_id: head_patch_id,
                        new_summary_hash: &new_summary_hash,
                        new_evidence: f.new_evidence,
                        regression: f.regression,
                        diff_since_dismissal_touches_paths: false,
                    }) {
                        FindingReopen::Open => REVIEW_FINDING_OPEN,
                        FindingReopen::Settled => REVIEW_FINDING_SETTLED,
                    }
                }
                Some(_) => REVIEW_FINDING_OPEN,
            };
            ReviewFindingRow {
                pr: pr.to_string(),
                generation,
                reviewer: reviewer.to_string(),
                finding_id,
                revision,
                review_run_id: run_id,
                raised_at_sha: head_sha.to_string(),
                raised_at_patch_id: head_patch_id.to_string(),
                paths: f.paths.clone(),
                summary_hash: new_summary_hash,
                blocking: f.blocking,
                new_evidence: f.new_evidence,
                regression: f.regression,
                status: status.to_string(),
                resolved_by: String::new(),
                dismissed_by: String::new(),
            }
        })
        .collect();
    FindingsPlan {
        rows,
        resolve_open: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block_json(approve: bool, findings: &str) -> String {
        format!(
            "some review prose\n\n```{REVIEW_VERDICT_TAG}\n{{\"approve\": {approve}, \"findings\": [{findings}]}}\n```\n\nHANDOFF: findings\n"
        )
    }

    #[test]
    fn parses_a_structured_block() {
        let text = block_json(
            false,
            "{\"id\":\"B8\",\"blocking\":true,\"summary\":\"abort bypasses cancellation\",\
             \"paths\":[\"desktop/src/a.rs\"],\"new_evidence\":false,\"regression\":false}",
        );
        let block = parse_verdict_block(&text).expect("block parses");
        assert!(!block.approve);
        assert_eq!(block.findings.len(), 1);
        assert_eq!(block.findings[0].id, "B8");
        assert_eq!(block.findings[0].paths, vec!["desktop/src/a.rs"]);
    }

    #[test]
    fn optional_fields_default_conservatively() {
        let text = block_json(false, "{\"id\":\"B8\",\"blocking\":true,\"summary\":\"x\"}");
        let block = parse_verdict_block(&text).expect("block parses");
        assert_eq!(block.findings[0].paths, Vec::<String>::new(), "unscoped");
        assert!(!block.findings[0].new_evidence);
        assert!(!block.findings[0].regression);
    }

    #[test]
    fn rejects_unknown_and_missing_fields() {
        let unknown = block_json(
            false,
            "{\"id\":\"B8\",\"blocking\":true,\"summary\":\"x\",\"z\":1}",
        );
        assert!(parse_verdict_block(&unknown).is_none());
        let missing = block_json(false, "{\"id\":\"B8\",\"summary\":\"x\"}");
        assert!(parse_verdict_block(&missing).is_none());
        let empty_id = block_json(false, "{\"id\":\"\",\"blocking\":true,\"summary\":\"x\"}");
        assert!(parse_verdict_block(&empty_id).is_none());
    }

    #[test]
    fn rejects_zero_or_multiple_blocks() {
        assert!(parse_verdict_block("no block here\nHANDOFF: findings").is_none());
        let two = format!("{}\n{}", block_json(false, ""), block_json(false, ""));
        assert!(
            parse_verdict_block(&two).is_none(),
            "two blocks are ambiguous"
        );
    }

    /// **Duplicate ids within one verdict block (STUDIO-1009).** Two entries sharing an `id` would be
    /// planned against the same `prior` and collide on the finding table's key, so the block is
    /// rejected as malformed rather than split or silently deduplicated.
    #[test]
    fn rejects_a_block_listing_the_same_id_twice() {
        let dup = block_json(
            false,
            "{\"id\":\"B8\",\"blocking\":true,\"summary\":\"first\"},\
             {\"id\":\"B8\",\"blocking\":true,\"summary\":\"second\"}",
        );
        assert!(
            parse_verdict_block(&dup).is_none(),
            "a duplicated id is a malformed block, not two findings"
        );
        // Control: two DISTINCT ids parse.
        let distinct = block_json(
            false,
            "{\"id\":\"B8\",\"blocking\":true,\"summary\":\"first\"},\
             {\"id\":\"B9\",\"blocking\":true,\"summary\":\"second\"}",
        );
        let block = parse_verdict_block(&distinct).expect("distinct ids parse");
        assert_eq!(block.findings.len(), 2);
    }

    #[test]
    fn an_approving_block_with_no_findings_parses() {
        let text = block_json(true, "");
        let block = parse_verdict_block(&text).expect("block parses");
        assert!(block.approve);
        assert!(block.findings.is_empty());
    }

    // --- the reopen rule (§6.3's four rows) ---------------------------------------------------

    #[test]
    fn reopen_settles_an_unchanged_repeat_at_the_same_patch() {
        let d = reopen_decision(&ReopenInputs {
            dismissed_at_patch_id: "p1",
            dismissed_summary_hash: "h",
            new_patch_id: "p1",
            new_summary_hash: "h",
            ..Default::default()
        });
        assert_eq!(d, FindingReopen::Settled);
    }

    #[test]
    fn reopen_opens_on_new_evidence_at_the_same_head() {
        let row = |new_evidence, regression| ReopenInputs {
            dismissed_at_patch_id: "p1",
            dismissed_summary_hash: "h",
            new_patch_id: "p1",
            new_summary_hash: "h",
            new_evidence,
            regression,
            ..Default::default()
        };
        assert_eq!(reopen_decision(&row(true, false)), FindingReopen::Open);
        assert_eq!(reopen_decision(&row(false, true)), FindingReopen::Open);
        assert_eq!(
            reopen_decision(&row(false, false)),
            FindingReopen::Settled,
            "control: no flag, same patch, same digest"
        );
    }

    #[test]
    fn reopen_opens_on_a_different_summary() {
        assert_eq!(
            reopen_decision(&ReopenInputs {
                dismissed_at_patch_id: "p1",
                dismissed_summary_hash: "h",
                new_patch_id: "p1",
                new_summary_hash: "h2",
                ..Default::default()
            }),
            FindingReopen::Open
        );
    }

    #[test]
    fn reopen_on_paths_changed() {
        let paths = vec!["src/a.rs".to_string()];
        // Different patch-id, diff touches the finding's path → open.
        assert_eq!(
            reopen_decision(&ReopenInputs {
                dismissed_at_patch_id: "p1",
                dismissed_summary_hash: "h",
                paths: &paths,
                new_patch_id: "p2",
                new_summary_hash: "h",
                diff_since_dismissal_touches_paths: true,
                ..Default::default()
            }),
            FindingReopen::Open
        );
        // Different patch-id, diff touches none of them → settled.
        assert_eq!(
            reopen_decision(&ReopenInputs {
                dismissed_at_patch_id: "p1",
                dismissed_summary_hash: "h",
                paths: &paths,
                new_patch_id: "p2",
                new_summary_hash: "h",
                ..Default::default()
            }),
            FindingReopen::Settled
        );
        // Unscoped is always relevant, whatever the diff covers.
        assert_eq!(
            reopen_decision(&ReopenInputs {
                dismissed_at_patch_id: "p1",
                dismissed_summary_hash: "h",
                new_patch_id: "p2",
                new_summary_hash: "h",
                ..Default::default()
            }),
            FindingReopen::Open
        );
    }

    // --- planning -----------------------------------------------------------------------------

    fn finding(id: &str, summary: &str) -> BlockFinding {
        BlockFinding {
            id: id.to_string(),
            blocking: true,
            summary: summary.to_string(),
            ..Default::default()
        }
    }

    /// The base inputs for one completed review of `o/r#1` by `sol` at run `run_id`, with the two
    /// pieces each test varies.
    fn inputs<'a>(
        run_id: i64,
        block: Option<&'a ReviewVerdictBlock>,
        prior: &'a [ReviewFindingRow],
    ) -> CompletionInputs<'a> {
        CompletionInputs {
            pr: "o/r#1",
            generation: 0,
            reviewer: "sol",
            run_id,
            head_sha: "sha",
            block,
            prior,
            ..Default::default()
        }
    }

    #[test]
    fn a_repeat_raise_increments_revision() {
        let prior = vec![ReviewFindingRow {
            pr: "o/r#1".into(),
            generation: 0,
            reviewer: "sol".into(),
            finding_id: "sol:B8".into(),
            revision: 1,
            status: REVIEW_FINDING_OPEN.into(),
            ..Default::default()
        }];
        let block = ReviewVerdictBlock {
            approve: false,
            findings: vec![finding("B8", "same")],
        };
        let plan = plan_review_findings(&inputs(7, Some(&block), &prior));
        assert_eq!(plan.rows.len(), 1);
        assert_eq!(
            plan.rows[0].revision, 2,
            "the repeat raise increments revision"
        );
        assert_eq!(plan.rows[0].finding_id, "sol:B8");
        assert!(!plan.resolve_open);
    }

    #[test]
    fn a_re_raise_of_a_dismissed_finding_with_new_evidence_is_open() {
        let prior = vec![ReviewFindingRow {
            pr: "o/r#1".into(),
            generation: 0,
            reviewer: "sol".into(),
            finding_id: "sol:B8".into(),
            revision: 1,
            status: REVIEW_FINDING_DISMISSED.into(),
            summary_hash: summary_hash("same"),
            ..Default::default()
        }];
        let mut f = finding("B8", "same");
        f.new_evidence = true;
        let block = ReviewVerdictBlock {
            approve: false,
            findings: vec![f],
        };
        let plan = plan_review_findings(&inputs(8, Some(&block), &prior));
        assert_eq!(plan.rows[0].status, REVIEW_FINDING_OPEN);
    }

    #[test]
    fn a_re_raise_of_a_dismissed_finding_unchanged_is_settled() {
        let prior = vec![ReviewFindingRow {
            pr: "o/r#1".into(),
            generation: 0,
            reviewer: "sol".into(),
            finding_id: "sol:B8".into(),
            revision: 1,
            status: REVIEW_FINDING_DISMISSED.into(),
            summary_hash: summary_hash("same"),
            ..Default::default()
        }];
        let block = ReviewVerdictBlock {
            approve: false,
            findings: vec![finding("B8", "same")],
        };
        let plan = plan_review_findings(&inputs(8, Some(&block), &prior));
        assert_eq!(plan.rows[0].status, REVIEW_FINDING_SETTLED);
    }

    /// The reviewed head's patch-id is what the reopen rule compares (STUDIO-1009, carrying STUDIO-977
    /// into the finding revisions). An UNCHANGED patch-id settles a dismissed re-raise; a CHANGED one
    /// reopens it.
    #[test]
    fn a_re_raise_reopens_on_a_changed_patch_and_settles_on_the_same_one() {
        let dismissed = |patch: &str| {
            vec![ReviewFindingRow {
                pr: "o/r#1".into(),
                generation: 0,
                reviewer: "sol".into(),
                finding_id: "sol:B8".into(),
                revision: 1,
                status: REVIEW_FINDING_DISMISSED.into(),
                summary_hash: summary_hash("same"),
                raised_at_patch_id: patch.into(),
                ..Default::default()
            }]
        };
        let block = ReviewVerdictBlock {
            approve: false,
            findings: vec![finding("B8", "same")],
        };
        let base = CompletionInputs {
            pr: "o/r#1",
            generation: 0,
            reviewer: "sol",
            run_id: 8,
            head_sha: "sha",
            block: Some(&block),
            ..Default::default()
        };
        let same = dismissed("p1");
        let plan = plan_review_findings(&CompletionInputs {
            head_patch_id: "p1",
            prior: &same,
            ..base
        });
        assert_eq!(
            plan.rows[0].status, REVIEW_FINDING_SETTLED,
            "the same patch-id is a repeat of an objection already decided"
        );
        let changed = dismissed("p1");
        let plan = plan_review_findings(&CompletionInputs {
            head_patch_id: "p2",
            prior: &changed,
            ..base
        });
        assert_eq!(
            plan.rows[0].status, REVIEW_FINDING_OPEN,
            "a changed patch-id reopens an unscoped dismissed finding"
        );
        assert_eq!(
            plan.rows[0].raised_at_patch_id, "p2",
            "the revision records the patch it was raised at"
        );
    }

    #[test]
    fn an_approving_review_resolves_and_raises_nothing() {
        let plan = plan_review_findings(&CompletionInputs {
            pr: "o/r#1",
            reviewer: "sol",
            run_id: 9,
            approved: true,
            ..Default::default()
        });
        assert!(plan.rows.is_empty());
        assert!(plan.resolve_open);
    }

    #[test]
    fn two_unstructured_reviews_get_distinct_ids() {
        let a = plan_review_findings(&inputs(10, None, &[]));
        let b = plan_review_findings(&inputs(11, None, &[]));
        assert_eq!(a.rows.len(), 1);
        assert_eq!(b.rows.len(), 1);
        assert_ne!(
            a.rows[0].finding_id, b.rows[0].finding_id,
            "each unstructured review is scoped to its own run"
        );
        assert!(a.rows[0].blocking, "unstructured findings block");
        assert!(
            a.rows[0].paths.is_empty(),
            "unstructured findings are unscoped"
        );
    }

    #[test]
    fn an_empty_changes_block_falls_back_to_unstructured() {
        let block = ReviewVerdictBlock {
            approve: false,
            findings: vec![],
        };
        let plan = plan_review_findings(&inputs(12, Some(&block), &[]));
        assert_eq!(plan.rows.len(), 1);
        assert!(plan.rows[0].finding_id.contains("unstructured"));
    }

    #[test]
    fn summary_normalization_collapses_whitespace_only() {
        assert_eq!(normalize_summary("  a  b \n c "), "a b c");
        assert_ne!(
            summary_hash("Abort"),
            summary_hash("abort"),
            "case is significant"
        );
        assert_eq!(summary_hash("a  b"), summary_hash("a b"));
    }
}
