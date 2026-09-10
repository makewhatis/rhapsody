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
}
