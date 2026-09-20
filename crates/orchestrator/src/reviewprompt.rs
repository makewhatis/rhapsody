//! The base prompt a REVIEW run renders, and the predicate that selects it (STUDIO-798).
//!
//! **No Go counterpart.** The frozen Symphony reference has no review runs at all; this is the
//! additive Rhapsody surface, and it is inert for every implementation run.
//!
//! # Why the host owns this text
//!
//! [`quorum::review_description`](crate::quorum::review_description) writes a review ticket's
//! description on the host precisely so the one instruction that has to hold — *never merge* —
//! cannot be authored, or rewritten, by an agent. But a description is only ever a `{{
//! issue.description }}` INSIDE a base template, and the base template is whatever
//! `prompt_file`/`prompt` names — on a real installation, the reviewed repository's own implementer
//! prompt, read out of the agent's own worktree at run time. Rhapsody's says "You DO merge your own
//! PR". So the host's prohibition was being embedded in a document that contradicts it, one edit
//! away from the exact hazard the quorum design was built to avoid.
//!
//! A review run therefore does not render the configured template at all. It renders
//! [`REVIEW_BASE_PROMPT`], which is `include_str!`d into the daemon binary: changing it needs a
//! merged pull request and a rebuilt daemon, not a write into a worktree the agent already owns.
//! The ticket's host-written description still lands inside it as `{{ issue.description }}`, so the
//! quorum keeps saying everything it said before — inside a document that now agrees with it.
//!
//! Implementation runs are untouched: [`is_review_run`] is false for them, and the worker takes the
//! existing [`resolve_prompt_template`](crate::worker::resolve_prompt_template) path byte-for-byte.

use rhapsody_core::Issue;

use crate::review::ReviewCheckout;

/// The host-written base prompt every review run renders (STUDIO-798).
///
/// Compiled in rather than read from disk: this is the one prompt whose contents a reviewed
/// repository must not be able to change, and `include_str!` is what makes "the repository cannot
/// change it" a build-time fact rather than a convention. It is the same mechanism the built-in
/// teammate profiles use (`rhapsody_config::profiles`).
///
/// It is a Liquid template rendered by the same strict-variables
/// [`prompt::render`](rhapsody_config::prompt::render) every other prompt goes through, so it may
/// only touch bound keys — and it must render cleanly for the ticketless review path's synthetic
/// issue too, which carries a `pr:`-shaped identifier, no url and no description at all.
pub const REVIEW_BASE_PROMPT: &str = include_str!("reviewprompt/review-base.md");

/// Reports whether this attempt is a REVIEW run — one whose whole job is to read a teammate's pull
/// request and say what it thinks of it — rather than an implementation run.
///
/// Two independent signals, either of which is sufficient, because Rhapsody has two review shapes
/// and they are distinguishable in different places:
///
/// * `review` is `Some` for a TICKETLESS review (STUDIO-715): the run carries the pull request's
///   coordinates and its issue is synthetic, so it has no label to read.
/// * the issue carries [`REVIEW_TICKET_LABEL`](crate::quorum::REVIEW_TICKET_LABEL) for a QUORUM
///   review ticket (STUDIO-780): a real tracker ticket the quorum minted, dispatched through the
///   ordinary path with no review coordinates at all.
///
/// The label read is [`crate::lifecycle::is_review_ticket`] — the same one the console decorates
/// with, so the two can never disagree about what a review ticket is.
pub(crate) fn is_review_run(review: Option<&ReviewCheckout>, issue: &Issue) -> bool {
    review.is_some() || crate::lifecycle::is_review_ticket(issue)
}

/// How one review round reads the pull request (STUDIO-959).
///
/// Round 1 for a reviewer is a full read of the whole change. A later round, where the daemon knows
/// which commit that reviewer last read AND that commit is an ancestor of the current head, is a
/// DELTA: start from the difference plus the findings already filed, confirm each is addressed, and
/// review the delta for anything new. The instructions for conducting either round live in
/// [`REVIEW_BASE_PROMPT`] (compiled in, so an agent cannot rewrite them); the FACTS for this round —
/// which commit, which findings, which mode — are host-written into the description
/// [`review_round_description`] composes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewRoundMode {
    /// Read the whole change, with the reason it is not a delta.
    Full(FullReviewReason),
    /// Read the delta from `prior_sha` to the head, confirming `findings` and re-checking the change
    /// itself.
    Delta {
        prior_sha: String,
        findings: Vec<String>,
    },
}

/// Why a round is FULL rather than a delta. Recorded rather than collapsed into one variant so the
/// round can say which mode it took AND why — a rebase falling back to a full read is a different
/// event, to the operator and to the reviewer, from a first-ever round.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FullReviewReason {
    /// No prior round this reviewer read, or no record of one: the ordinary first review.
    NoPriorRound,
    /// A prior attempt at this SAME head did not record a verdict (a truncated round), so the whole
    /// change is read again rather than diffed against itself.
    SameHead,
    /// The prior commit is not an ancestor of the head — a rebase or force-push — so a delta between
    /// them would be meaningless.
    Rebase,
    /// The ancestry or findings could not be read. The daemon degrades to a full review rather than
    /// asking the reviewer to verify a list it does not have.
    Unavailable,
}

impl ReviewRoundMode {
    /// Whether this round is a delta — the predicate the tests and the description branch on.
    pub fn is_delta(&self) -> bool {
        matches!(self, ReviewRoundMode::Delta { .. })
    }
}

/// The pure mode decision, from the facts a delta round is built out of (STUDIO-959).
///
/// `is_ancestor`/`findings` are `Option` because each is a `gh` read that can fail: `None` means the
/// read did not answer, and every `None` falls back to a FULL review. `prior_sha == head` is a
/// round at a head already read — a truncated round re-armed at the same commit — and is full too,
/// because there is no delta to review.
///
/// Pure and side-effect-free on purpose: the five acceptance cases (first round, rebase, fetch
/// failure, delta, same head) are all driveable through this one function, which is what makes the
/// mutation discipline cheap to keep.
pub fn review_round_mode(
    prior_sha: &str,
    head: &str,
    is_ancestor: Option<bool>,
    findings: Option<Vec<String>>,
) -> ReviewRoundMode {
    if prior_sha.is_empty() || head.is_empty() {
        return ReviewRoundMode::Full(FullReviewReason::NoPriorRound);
    }
    if prior_sha == head {
        return ReviewRoundMode::Full(FullReviewReason::SameHead);
    }
    match is_ancestor {
        Some(false) => ReviewRoundMode::Full(FullReviewReason::Rebase),
        None => ReviewRoundMode::Full(FullReviewReason::Unavailable),
        Some(true) => match findings {
            None => ReviewRoundMode::Full(FullReviewReason::Unavailable),
            Some(findings) => ReviewRoundMode::Delta {
                prior_sha: prior_sha.to_string(),
                findings,
            },
        },
    }
}

/// The host-written per-round description for a ticketless review (STUDIO-959).
///
/// **Written by the host, never by an agent.** It lands inside [`REVIEW_BASE_PROMPT`] as
/// `{{ issue.description }}`, so it is the only per-round channel into a prompt that is otherwise
/// compiled in. The MODE and its reason are stated in the round's own words, because "which review
/// was this" is the first thing a human reading the transcript afterwards needs, and because an
/// acceptance criterion is that the round says which mode it used.
///
/// The delta branch embeds the findings text the host retrieved. A delta with no findings is still
/// a delta — the reviewer may genuinely have filed none — and says so rather than leaving the
/// section blank.
pub fn review_round_description(mode: &ReviewRoundMode, head: &str) -> String {
    let at = short_sha(head);
    match mode {
        ReviewRoundMode::Full(reason) => {
            let why = match reason {
                FullReviewReason::NoPriorRound => {
                    "you have not reviewed this pull request before, or the daemon has no record of \
                     a round you read at an earlier commit"
                }
                FullReviewReason::SameHead => {
                    "an earlier attempt at this same commit did not record a verdict"
                }
                FullReviewReason::Rebase => {
                    "the commit you last read is not an ancestor of the head (the branch was rebased \
                     or force-pushed), so a diff between them would be meaningless"
                }
                FullReviewReason::Unavailable => {
                    "the daemon could not read the earlier round's commit relationship or its \
                     findings, and will not ask you to confirm a list it does not have"
                }
            };
            format!("**Round mode:** full review — {why}. Read the whole change at `{at}`.")
        }
        ReviewRoundMode::Delta {
            prior_sha,
            findings,
        } => {
            let prior = short_sha(prior_sha);
            let mut out = format!(
                "**Round mode:** delta review — start from the difference between the commit you \
                 last read and the head.\n\
                 \n\
                 **Last reviewed:** `{prior}`\n\
                 **Head:** `{at}`\n\
                 Diff them with `git diff {prior}..{at}`.\n"
            );
            if findings.is_empty() {
                out.push_str(
                    "\n**Your prior findings:** the daemon found no findings comments on this pull \
                     request from that round.\n",
                );
            } else {
                out.push_str("\n**Your prior findings from that round:**\n");
                for f in findings {
                    out.push_str("\n---\n");
                    out.push_str(f);
                    out.push('\n');
                }
            }
            out.push_str(
                "\nConfirm each of those is addressed, or say why it is not, and review the delta \
                 itself for anything new.\n",
            );
            out
        }
    }
}

/// Resolves the round's mode from the off-loop `gh` reads (STUDIO-959).
///
/// Every read is best-effort and every failure degrades to a full review: a missing source, an
/// ancestry that could not be asked, or findings that could not be fetched each produce a FULL
/// round rather than a delta taken on faith. This is the one place the mode's inputs are gathered,
/// so a test can drive the whole matrix with a fake [`ReviewDeltaSource`].
pub async fn resolve_review_round(
    source: Option<&dyn crate::ghsummons::ReviewDeltaSource>,
    request: &crate::review::ReviewDeltaRequest,
) -> ReviewRoundMode {
    let Some(src) = source else {
        return review_round_mode(&request.prior_sha, &request.head_sha, None, None);
    };
    // Nothing to ask when there is no prior commit, or the prior commit IS the head: both are full
    // rounds by construction, and spending two `gh` calls to learn that would be waste on the one
    // path (a truncated round re-armed at its own head) that reaches it.
    if request.prior_sha.is_empty()
        || request.head_sha.is_empty()
        || request.prior_sha == request.head_sha
    {
        return review_round_mode(&request.prior_sha, &request.head_sha, None, None);
    }
    let is_ancestor = src
        .is_ancestor(
            &request.owner,
            &request.repo,
            &request.prior_sha,
            &request.head_sha,
        )
        .await
        .ok();
    // Only ask for findings once the ancestry is known good: a delta across a rebase is refused
    // regardless, and the extra read would be spent for nothing.
    let findings = if is_ancestor == Some(true) {
        src.prior_findings(&request.owner, &request.repo, request.number)
            .await
            .ok()
    } else {
        None
    };
    review_round_mode(&request.prior_sha, &request.head_sha, is_ancestor, findings)
}

/// The first 7 characters of a SHA, for the description's human-readable prose. A `review::short_sha`
/// clone: that one is private to `review.rs`, and widening it for a description template would put
/// a truncated SHA one call away from a decision.
fn short_sha(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

#[cfg(test)]
mod tests {
    use rhapsody_config::prompt;

    use super::*;
    use crate::quorum::{QuorumRequest, REVIEW_TICKET_LABEL, review_description};

    /// The description a real quorum fan-out writes onto the review ticket it mints.
    fn review_ticket() -> Issue {
        let req = QuorumRequest {
            pr_url: "https://github.com/makewhatis/rhapsody/pull/124".into(),
            parent_identifier: "STUDIO-792".into(),
            parent_title: "page the Jobs list".into(),
            author: "jimmy".into(),
            summon_token: "@symphony".into(),
            ..QuorumRequest::default()
        };
        Issue {
            id: "iss-1".into(),
            identifier: "STUDIO-801".into(),
            title: "Review: STUDIO-792 page the Jobs list".into(),
            description: Some(review_description(&req, "alice", "")),
            url: Some("https://linear.app/studio49/issue/STUDIO-801".into()),
            labels: Some(vec!["rhapsody:@alice".into(), REVIEW_TICKET_LABEL.into()]),
            ..Issue::default()
        }
    }

    fn checkout() -> ReviewCheckout {
        ReviewCheckout {
            pr_number: 124,
            head_sha: "387a5f12aadc75d563be20650207135af371b009".into(),
            delta: None,
        }
    }

    #[test]
    fn a_quorum_review_ticket_is_a_review_run() {
        assert!(is_review_run(None, &review_ticket()));
    }

    #[test]
    fn a_ticketless_review_is_a_review_run_without_any_label() {
        let iss = Issue {
            id: "pr:makewhatis/rhapsody#124@alice".into(),
            identifier: "pr:makewhatis/rhapsody#124@alice".into(),
            labels: Some(vec!["rhapsody:@alice".into()]),
            ..Issue::default()
        };
        assert!(is_review_run(Some(&checkout()), &iss));
    }

    #[test]
    fn an_implementation_ticket_is_not_a_review_run() {
        let iss = Issue {
            id: "iss-2".into(),
            identifier: "STUDIO-798".into(),
            labels: Some(vec!["rhapsody:@jimmy".into()]),
            ..Issue::default()
        };
        assert!(!is_review_run(None, &iss));
    }

    /// The base prompt must survive strict-variables rendering for BOTH review shapes: an unbound
    /// key fails the RUN, not just the assertion. The ticketless shape is the demanding one — its
    /// synthetic issue has no url and no description — so the template's emptiness guards have to
    /// leave a readable document rather than dangling structure.
    #[test]
    fn the_base_prompt_renders_for_both_review_shapes() {
        let rendered = prompt::render(REVIEW_BASE_PROMPT, &review_ticket(), None).expect("render");
        assert!(
            rendered.contains("STUDIO-801 — Review: STUDIO-792 page the Jobs list"),
            "the ticket's own heading is missing:\n{rendered}"
        );
        assert!(
            rendered.contains("Never merge, and never push to the author's branch."),
            "the quorum's host-written description must still land inside it:\n{rendered}"
        );

        let synthetic = Issue {
            id: "pr:makewhatis/rhapsody#124@alice".into(),
            identifier: "pr:makewhatis/rhapsody#124@alice".into(),
            title: "Review makewhatis/rhapsody#124 at 387a5f1".into(),
            ..Issue::default()
        };
        let rendered = prompt::render(REVIEW_BASE_PROMPT, &synthetic, None).expect("render");
        assert!(
            rendered.contains("Review makewhatis/rhapsody#124 at 387a5f1"),
            "a synthetic review issue must still name what is being reviewed:\n{rendered}"
        );
        assert!(
            rendered.contains("Review makewhatis/rhapsody#124 at 387a5f1\n\n# Standing rules"),
            "an absent url and description must leave no dangling structure behind the \
             heading:\n{rendered}"
        );
    }

    /// The prohibition itself, as an invariant over the whole file rather than a spot check: EVERY
    /// line that mentions merging must be the one forbidding it.
    ///
    /// A future edit that adds any other sentence about merging — however well meant — fails here,
    /// which is the property this file exists to hold. (It is a canary on text this repository
    /// owns, not a filter on untrusted input; the safety of the prompt rests on it being compiled
    /// in, not on this scan.)
    #[test]
    fn every_mention_of_merging_in_the_base_prompt_forbids_it() {
        let mentions: Vec<&str> = REVIEW_BASE_PROMPT
            .lines()
            .filter(|ln| ln.to_lowercase().contains("merg"))
            .collect();
        assert_eq!(
            mentions,
            vec![
                "1. **Never merge.** Not this pull request, not any other, not \"once CI is green\"."
            ],
            "the reviewer's base prompt must say nothing about merging except that it must not"
        );
    }

    // ── STUDIO-959: delta rounds ─────────────────────────────────────────────────────────────────

    const HEAD: &str = "def5678def5678def5678def5678def5678def5";
    const PRIOR: &str = "abc1234abc1234abc1234abc1234abc1234abc1";

    /// The compiled instructions for a delta round must be present and must say the three things
    /// the acceptance calls for: confirm your findings, review the delta, and do NOT stop at the
    /// delta's edges (a change inside it can invalidate a round-1 conclusion). Mutation: deleting
    /// the section from `review-base.md` reds here.
    #[test]
    fn the_base_prompt_instructs_a_delta_round_without_fencing_it_in() {
        let lower = REVIEW_BASE_PROMPT.to_lowercase();
        assert!(
            lower.contains("delta review"),
            "the compiled base prompt must name the delta round"
        );
        assert!(
            lower.contains("confirm each of those findings"),
            "it must ask the reviewer to confirm its prior findings"
        );
        assert!(
            lower.contains("the delta is where you start, not the limit of what you may read"),
            "it must forbid fencing the reviewer inside the delta"
        );
        assert!(
            lower.contains("break what the test claims to protect"),
            "mutation discipline must survive the cost cut"
        );
    }

    /// Acceptance case 1: a reviewer with a prior commit that IS an ancestor of the head gets a
    /// delta, and the description hands over the prior commit AND the findings. Mutation: dropping
    /// the findings from the description while keeping the delta reds the findings assertion below.
    #[test]
    fn an_ancestor_prior_commit_is_a_delta_given_its_findings() {
        let findings = vec!["fix the off-by-one in the parser".to_string()];
        let mode = review_round_mode(PRIOR, HEAD, Some(true), Some(findings.clone()));
        assert!(mode.is_delta(), "an ancestor prior commit must be a delta");
        let text = review_round_description(&mode, HEAD);
        assert!(text.contains("delta review"), "{text}");
        assert!(
            text.contains("abc1234"),
            "the prior commit must be named:\n{text}"
        );
        assert!(text.contains("def5678"), "the head must be named:\n{text}");
        assert!(
            text.contains("fix the off-by-one in the parser"),
            "the prior findings must actually be in the description:\n{text}"
        );
        assert!(
            text.contains("git diff abc1234..def5678"),
            "the delta round must be told how to read the delta:\n{text}"
        );
    }

    /// Acceptance case 2a: no prior commit is a FULL review, and the round says so.
    #[test]
    fn no_prior_commit_is_a_full_review() {
        let mode = review_round_mode("", HEAD, Some(true), Some(Vec::new()));
        assert_eq!(mode, ReviewRoundMode::Full(FullReviewReason::NoPriorRound));
        let text = review_round_description(&mode, HEAD);
        assert!(text.contains("full review"), "{text}");
        assert!(!text.contains("delta review"), "{text}");
    }

    /// Acceptance case 2b: a prior commit that is NOT an ancestor (a rebase or force-push) is a
    /// FULL review, and the round says why. Mutation: forcing the delta path for a non-ancestor
    /// reds here — the ancestry check is what gates the mode.
    #[test]
    fn a_rebased_prior_commit_is_a_full_review_that_says_why() {
        let mode = review_round_mode(PRIOR, HEAD, Some(false), Some(vec!["stale".to_string()]));
        assert_eq!(mode, ReviewRoundMode::Full(FullReviewReason::Rebase));
        let text = review_round_description(&mode, HEAD);
        assert!(text.contains("full review"), "{text}");
        assert!(
            text.contains("rebased") || text.contains("force-pushed"),
            "the round must say why it fell back to a full read:\n{text}"
        );
    }

    /// Acceptance case 2c: a prior commit the daemon cannot ask about, or findings it cannot read,
    /// is a FULL review — the "do not ask a reviewer to verify a list it does not have" rule.
    #[test]
    fn an_unreadable_prior_round_is_a_full_review() {
        let unasked = review_round_mode(PRIOR, HEAD, None, None);
        assert_eq!(
            unasked,
            ReviewRoundMode::Full(FullReviewReason::Unavailable)
        );
        let unreadable = review_round_mode(PRIOR, HEAD, Some(true), None);
        assert_eq!(
            unreadable,
            ReviewRoundMode::Full(FullReviewReason::Unavailable),
            "an ancestry that answered but findings that did not must still be full"
        );
        assert!(review_round_description(&unreadable, HEAD).contains("full review"));
    }

    /// A round at the SAME commit is not a delta against itself: a truncated round re-armed at its
    /// own head is a full re-read.
    #[test]
    fn a_prior_commit_equal_to_the_head_is_a_full_review() {
        assert_eq!(
            review_round_mode(HEAD, HEAD, Some(true), Some(vec![])),
            ReviewRoundMode::Full(FullReviewReason::SameHead)
        );
    }

    /// A delta round that filed no findings is still a delta, and says so rather than rendering an
    /// empty section.
    #[test]
    fn a_delta_with_no_findings_says_so() {
        let mode = review_round_mode(PRIOR, HEAD, Some(true), Some(Vec::new()));
        let text = review_round_description(&mode, HEAD);
        assert!(text.contains("delta review"), "{text}");
        assert!(
            text.contains("no findings comments"),
            "an empty findings list must be stated, not left blank:\n{text}"
        );
    }

    /// A fake `gh` seam so the resolve matrix is driveable without a process: each read is either
    /// an answer or a failure.
    struct FakeDelta {
        ancestor: Result<bool, String>,
        findings: Result<Vec<String>, String>,
    }

    #[async_trait::async_trait]
    impl crate::ghsummons::ReviewDeltaSource for FakeDelta {
        async fn is_ancestor(
            &self,
            _owner: &str,
            _repo: &str,
            _base: &str,
            _head: &str,
        ) -> crate::ghsummons::DeltaResult<bool> {
            match &self.ancestor {
                Ok(v) => Ok(*v),
                Err(e) => Err(e.clone().into()),
            }
        }
        async fn prior_findings(
            &self,
            _owner: &str,
            _repo: &str,
            _number: i64,
        ) -> crate::ghsummons::DeltaResult<Vec<String>> {
            match &self.findings {
                Ok(v) => Ok(v.clone()),
                Err(e) => Err(e.clone().into()),
            }
        }
    }

    fn request() -> crate::review::ReviewDeltaRequest {
        crate::review::ReviewDeltaRequest {
            owner: "makewhatis".into(),
            repo: "rhapsody".into(),
            number: 12,
            prior_sha: PRIOR.into(),
            head_sha: HEAD.into(),
        }
    }

    #[tokio::test]
    async fn resolve_reads_the_delta_from_the_off_loop_seam() {
        let src = FakeDelta {
            ancestor: Ok(true),
            findings: Ok(vec!["finding one".into()]),
        };
        let mode = resolve_review_round(Some(&src), &request()).await;
        assert!(mode.is_delta(), "an answering seam must produce a delta");
        assert!(review_round_description(&mode, HEAD).contains("finding one"));
    }

    /// Mutation: turning the ancestry answer false must red this — it is the gate, not a log line.
    #[tokio::test]
    async fn resolve_degrades_to_full_when_the_seam_answers_not_an_ancestor() {
        let src = FakeDelta {
            ancestor: Ok(false),
            findings: Ok(vec!["finding one".into()]),
        };
        let mode = resolve_review_round(Some(&src), &request()).await;
        assert_eq!(mode, ReviewRoundMode::Full(FullReviewReason::Rebase));
    }

    #[tokio::test]
    async fn resolve_degrades_to_full_when_a_read_fails_or_is_absent() {
        let failed = FakeDelta {
            ancestor: Err("gh compare: boom".into()),
            findings: Ok(vec![]),
        };
        assert_eq!(
            resolve_review_round(Some(&failed), &request()).await,
            ReviewRoundMode::Full(FullReviewReason::Unavailable)
        );
        let no_findings = FakeDelta {
            ancestor: Ok(true),
            findings: Err("gh comments: boom".into()),
        };
        assert_eq!(
            resolve_review_round(Some(&no_findings), &request()).await,
            ReviewRoundMode::Full(FullReviewReason::Unavailable)
        );
        assert_eq!(
            resolve_review_round(None, &request()).await,
            ReviewRoundMode::Full(FullReviewReason::Unavailable)
        );
    }
}
