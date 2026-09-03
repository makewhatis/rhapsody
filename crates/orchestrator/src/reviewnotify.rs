//! reviewnotify — the review-completion comment that re-engages the pull request's AUTHOR
//! (STUDIO-723, slice 9 of the design record `~/.rhapsody/docs/STUDIO-703-ticketless-pr-review.md`,
//! §14.2 "author re-summon needs the brand token", §14.4).
//!
//! **No Go counterpart.** The frozen Symphony reference has no review feature; this is the additive
//! Rhapsody surface the design record specifies, gated end to end on Teams (§16).
//!
//! # The loop this closes, and the one thing that closes it
//!
//! Slices 3–8 get a review to run and a pushed head re-reviewed. What none of them does is tell the
//! AUTHOR that findings are waiting — and without that the loop has no second lap, because the
//! re-review is edge-triggered on the head advancing and only the author advances it.
//!
//! Re-engagement already exists, and it is narrow. A GitHub comment reopens an author's run through
//! [`crate::ghsummons::SummonSource`] → [`crate::ghenrich::apply_github_summons`], which advances
//! the ticket's `latest_summon_at` only when BOTH hold:
//!
//! 1. the comment body carries the configured summon token as a STANDALONE mention
//!    ([`rhapsody_core::compile_summon_matcher`] — the token inside a URL or an identifier does not
//!    count), and
//! 2. the pull request is among the author ticket's `linked_prs` in the poller's own snapshot.
//!
//! The second is the tracker's business and outside this slice. The first is not, and it is exactly
//! the kind of requirement that fails silently: a review that posts perfect findings with no token
//! leaves the author's ticket un-reopened, and nothing anywhere reports a problem — the review ran,
//! the comments are there, and the author is simply never told. That is the failure this module
//! removes, by making the token the DAEMON's to guarantee rather than the review agent's to
//! remember. [`crate::quorum::review_description`] does instruct a reviewer to lead every finding
//! with the token, and that instruction stays; it is an instruction to an agent, which is a weaker
//! thing than a comment the host composes and posts itself.
//!
//! # The tokenless completion is a contract, not an accident
//!
//! A comment with no token re-engages nobody. This module states that twice, because it is relied
//! on:
//!
//! * An **approved** review posts a deliberately TOKENLESS note. Approval means there is nothing to
//!   fix, so reopening the author's run would spend a dispatch on an empty instruction — and §15-c
//!   already makes approval the pause in the re-review loop. Not re-engaging is the same decision
//!   on the author's side of it.
//! * A review that left **findings** posts a token-bearing comment, which is the whole point.
//!
//! [`summons_author`] is the one predicate both are judged by, it runs the REAL matcher rather than
//! a substring test, and [`run_review_notify_task`] logs which way each comment went — so the no-op
//! is visible in the log instead of being indistinguishable from a broken template.
//!
//! # Off the loop
//!
//! Posting is a `gh` call and [`crate::ghsummons::GH`] shells out through a synchronous
//! `std::process::Command`, so it happens on this module's own task for
//! [`crate::reviewintro::run_review_intro_task`]'s reason: the containment is structural rather
//! than temporal (that future has no await point, so a `tokio::time::timeout` around it could never
//! fire), and a hung `gh` must park the task that owns this subsystem's network I/O and nothing
//! else. The control task decides ([`Orchestrator::plan_review_notify`], at the review's exit) and
//! hands the decision over; it never waits for the post.

use std::sync::Arc;

use rhapsody_core::{SUMMON_TOKEN_SYMPHONY, compile_summon_matcher};

use crate::control_loop::CancelWait;
use crate::ghsummons::PrCommentSink;
use crate::orchestrator::Orchestrator;
use crate::review::ReviewRun;

/// One finished review round, as the CONTROL TASK observed it, on its way to a comment.
///
/// Everything here is already in memory at the review's exit — the coordinates come off the run's
/// pinned [`ReviewRun`], the verdict off the agent's declared handoff — so the planner touches no
/// network, exactly as [`crate::reviewintro`]'s does not.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReviewCompletion {
    /// The pull request's repository owner.
    pub owner: String,
    /// The pull request's repository name.
    pub repo: String,
    /// The pull request number. The comment goes here, and this is the number
    /// [`crate::ghenrich::apply_github_summons`] will match against the author ticket's
    /// `linked_prs`.
    pub number: i64,
    /// The teammate who reviewed it.
    pub reviewer: String,
    /// The teammate who authored it — named in the comment so a human reading the pull request can
    /// see who is being asked, and never used to address anybody: GitHub `@`-mentions are a
    /// different namespace from Teams identities, and guessing across them would ping a stranger.
    pub author: String,
    /// The head SHA the round was pinned to at dispatch (design §14.1 F-SHA), so the comment says
    /// WHICH commit was read rather than "the latest".
    pub head_sha: String,
    /// Whether the reviewer declared `HANDOFF: approved`. It is the whole of the token decision:
    /// approved ⇒ tokenless, findings ⇒ token-bearing.
    pub approved: bool,
    /// The configured summon token, resolved once on the control task. Carried rather than re-read
    /// so the comment that is POSTED and the predicate that judged it cannot disagree across a
    /// config reload.
    pub summon_token: String,
}

impl std::fmt::Display for ReviewCompletion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}#{}", self.owner, self.repo, self.number)
    }
}

/// The first 7 characters of a SHA, for human-readable prose only. Mirrors `review::short_sha`;
/// duplicated rather than shared because widening that one to `pub(crate)` for a comment template
/// would invite it into decisions, and a truncated SHA must never reach one.
fn short_sha(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

/// Whether `body` would re-engage an author — the REAL matcher
/// ([`rhapsody_core::compile_summon_matcher`]), not a substring test.
///
/// Running the matcher is the point. `body.contains(token)` would answer yes for a token inside a
/// URL or an email address, which is precisely what the matcher's boundary rules refuse, so a
/// substring check would report re-engagement that never happens — the silent failure this module
/// exists to remove, reintroduced in the check meant to catch it.
///
/// A token that (impossibly) fails to compile answers `false`: an unusable matcher is exactly the
/// case where nothing will be re-engaged.
pub fn summons_author(body: &str, token: &str) -> bool {
    compile_summon_matcher(token).is_ok_and(|re| re.is_match(body))
}

/// The host-composed review-completion comment.
///
/// **Written by the host, never by an agent** — [`crate::quorum::review_description`]'s line, and
/// here it is what makes the token a guarantee rather than an instruction somebody may or may not
/// have followed.
///
/// The token LEADS the findings comment, at the very start of the body. That is not styling: the
/// matcher requires start-of-string or whitespace before the token, and leading with it means no
/// later edit to this template can accidentally bury the token inside a URL or a word and quietly
/// turn every review into the tokenless no-op.
///
/// The approved comment carries no token, deliberately (see the module docs). It still says what
/// happened, because the pull request is where a review's record belongs.
///
/// The findings body is written to be READ BY THE AUTHOR'S AGENT, not only by a human, and that is
/// not a nicety either: [`crate::ghenrich::apply_github_summons`] copies the summoning comment into
/// `latest_summon_body`, which [`crate::message`] hands to the reopened run as its operator
/// instruction. So this text IS the instruction the author acts on, and it says the two things the
/// re-review loop needs — read the comments, push to this branch — rather than merely announcing
/// that a review happened.
pub fn re_engage_comment(c: &ReviewCompletion) -> String {
    let ReviewCompletion {
        reviewer,
        author,
        head_sha,
        ..
    } = c;
    let head = short_sha(head_sha);
    if c.approved {
        return format!(
            "**{reviewer}** reviewed this pull request at `{head}` and found nothing to raise.\n\
             \n\
             No changes are requested, so {author} is not being asked for anything and this \
             comment deliberately does not summon them. Pushing to this branch arms one more \
             review of the new commits.\n\
             \n\
             Reviewed, not merged — {author} owns the merge.\n"
        );
    }
    let token = &c.summon_token;
    format!(
        "{token} **{reviewer}** reviewed this pull request at `{head}` and left findings on it.\n\
         \n\
         {author}: read the review comments on this pull request, then push your fixes to this \
         branch. Pushing is the whole of it — the daemon watches this pull request's head and arms \
         a fresh review of whatever lands, so there is no re-review to request and nobody to \
         notify.\n\
         \n\
         Reviewed, not merged — {author} owns the merge.\n"
    )
}

/// Everything [`run_review_notify_task`] runs against. No `Orchestrator`, no store and no control
/// channel — the off-loop guarantee, in the type, as [`crate::reviewintro::ReviewIntroDeps`] and
/// [`crate::quorum::QuorumDeps`] state it.
pub struct ReviewNotifyDeps {
    /// Where the comment is posted. `None` disables the whole notification: a daemon that cannot
    /// reach GitHub cannot re-engage anybody, and there is no second route worth inventing — the
    /// author's ticket is reopened by a PR comment or not at all.
    pub comments: Option<Arc<dyn PrCommentSink>>,
}

/// Consumes [`ReviewCompletion`]s until `ctx` is cancelled or every sender is dropped.
///
/// One at a time, serially and with no `spawn`, for [`crate::reviewintro::run_review_intro_task`]'s
/// reason: the `gh` call blocks, so concurrency here would occupy several runtime workers and
/// multiply the rate-limit pressure of a subsystem nobody is waiting on.
///
/// There is no back-off and no retry. A failed post costs the author one re-engagement — they still
/// have the reviewer's own comments on the pull request, and the review itself is already recorded
/// — whereas a retry loop would re-post a comment GitHub may well have accepted before failing, and
/// a duplicated summons is worse than a missed one: it reopens the author's run twice for one
/// round.
pub async fn run_review_notify_task(
    mut ctx: CancelWait,
    deps: ReviewNotifyDeps,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<ReviewCompletion>,
) {
    tracing::info!(
        "ticketless review notification task started (off-loop; a review's exit never waits on it)"
    );
    loop {
        let c = tokio::select! {
            _ = ctx.cancelled() => return,
            r = rx.recv() => match r {
                Some(r) => r,
                None => return,
            },
        };
        let Some(sink) = deps.comments.as_ref() else {
            tracing::debug!(
                pr = %c, "ticketless review: no GitHub comment sink, so the author is not re-engaged"
            );
            continue;
        };
        let body = re_engage_comment(&c);
        // The contract, in the log, on every comment: whether THIS body will reopen the author's
        // ticket. Recomputed from the body that is actually about to be posted rather than from
        // `c.approved`, so an edit to the template that lost the token reads as "will not
        // re-engage" here instead of failing silently three systems away (design §14.2).
        let summons = summons_author(&body, &c.summon_token);
        if summons != !c.approved {
            // The two disagree only if the template and the verdict have come apart — a findings
            // comment whose token no longer matches, or an approval that grew one. Neither is
            // recoverable here and both are worth saying out loud before posting.
            tracing::error!(
                pr = %c, approved = c.approved, summons,
                "ticketless review: the completion comment does not carry the token its verdict \
                 calls for; posting it as composed"
            );
        }
        match sink
            .post_pr_comment(&c.owner, &c.repo, c.number, &body)
            .await
        {
            Ok(()) => tracing::info!(
                pr = %c, reviewer = %c.reviewer, author = %c.author, approved = c.approved, summons,
                "ticketless review: review-completion comment posted"
            ),
            Err(e) => tracing::warn!(
                pr = %c, reviewer = %c.reviewer, err = %e,
                "ticketless review: the review-completion comment could not be posted; the author \
                 is not re-engaged for this round"
            ),
        }
    }
}

impl Orchestrator {
    /// Opens the notification task's channel, storing the sender and handing back the receiver for
    /// [`run_review_notify_task`].
    ///
    /// A method rather than a public field, mirroring
    /// [`open_review_intro_channel`](Orchestrator::open_review_intro_channel): a daemon that never
    /// calls it has `review_notify_tx: None`, so a review exit cannot even represent a comment.
    /// Call it BEFORE the control task takes the orchestrator, and only on the ticketless path.
    pub fn open_review_notify_channel(
        &mut self,
    ) -> tokio::sync::mpsc::UnboundedReceiver<ReviewCompletion> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.review_notify_tx = Some(tx);
        rx
    }

    /// Plans the completion comment for a finished review round, or `None` when there is nothing to
    /// say.
    ///
    /// Runs ON the control task, at [`Orchestrator::on_review_exit`] — the moment the daemon knows
    /// both that the round is over and what its verdict was. Every gate is a comparison over data
    /// already in memory.
    ///
    /// The gates:
    ///
    /// 1. Teams is on AND `review.mode: ticketless` (§16), so a Teams-off daemon and a daemon on
    ///    the ticket fan-out are untouched.
    /// 2. The coordinates can name a comment: an owner, a repo and a positive number.
    ///
    /// The verdict is NOT a gate. An approved round is notified too — with a deliberately tokenless
    /// comment (see the module docs), which is the documented no-op rather than a missing one.
    pub(crate) fn plan_review_notify(
        &self,
        run: &ReviewRun,
        approved: bool,
    ) -> Option<ReviewCompletion> {
        if !self.review_ticketless_enabled() {
            return None;
        }
        if run.owner.is_empty() || run.repo.is_empty() || run.number <= 0 {
            return None;
        }
        Some(ReviewCompletion {
            owner: run.owner.clone(),
            repo: run.repo.clone(),
            number: run.number,
            reviewer: run.reviewer.clone(),
            author: run.author.clone(),
            head_sha: run.head_sha.clone(),
            approved,
            summon_token: self.review_summon_token(),
        })
    }

    /// Hands a planned completion comment to the off-loop task. A no-op when the round says
    /// nothing, when no task is running, or when that task has already stopped — none of which is
    /// worth failing a review's exit over, exactly as a missed introduction is not: the round is
    /// recorded and the run is winding down either way.
    pub(crate) fn request_review_notify(&self, c: Option<ReviewCompletion>) {
        let (Some(c), Some(tx)) = (c, self.review_notify_tx.as_ref()) else {
            return;
        };
        let pr = c.to_string();
        if tx.send(c).is_err() {
            tracing::warn!(
                pr = %pr,
                "ticketless review: the notification task is gone; the author is not re-engaged"
            );
        }
    }

    /// The configured summon token the completion comment leads with. Empty config ⇒ the shipped
    /// default, for [`crate::quorum`]'s reason: a comment naming no token would silently produce
    /// reviews that never re-engage the author, which is the exact failure this slice removes.
    fn review_summon_token(&self) -> String {
        let token = self
            .eff
            .as_ref()
            .map(|e| e.cfg.tracker.summon_token.trim())
            .unwrap_or_default();
        if token.is_empty() {
            SUMMON_TOKEN_SYMPHONY.to_string()
        } else {
            token.to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use chrono::{DateTime, TimeZone, Utc};
    use rhapsody_config::teams::{Identity, Review, ReviewMode, Teams};
    use rhapsody_core::{Issue, LinkedPRRef};
    use rhapsody_store::{Sqlite, StorePath};
    use rhapsody_tracker::fake::Fake;

    use super::*;
    use crate::ghenrich::apply_github_summons;
    use crate::ghsummons::{GH, PrCommentResult, RunFn, SummonSource};
    use crate::testsupport::{empty_effective, empty_resolved_project};

    const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const REPO_URL: &str = "git@github.com:makewhatis/rhapsody.git";

    fn completion(approved: bool, token: &str) -> ReviewCompletion {
        ReviewCompletion {
            owner: "makewhatis".to_string(),
            repo: "rhapsody".to_string(),
            number: 12,
            reviewer: "bob".to_string(),
            author: "alice".to_string(),
            head_sha: HEAD.to_string(),
            approved,
            summon_token: token.to_string(),
        }
    }

    fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, 0)
            .single()
            .expect("valid timestamp")
    }

    /// The author's ticket as the poller holds it: one open pull request, never summoned.
    fn author_issue() -> Issue {
        Issue {
            identifier: "STUDIO-999".into(),
            linked_prs: Some(vec![LinkedPRRef {
                owner: "makewhatis".into(),
                repo: "rhapsody".into(),
                number: 12,
                merged: false,
            }]),
            ..Default::default()
        }
    }

    /// A `gh api` runner serving `body` as the ONE issue comment on PR 12, so the real
    /// [`SummonSource`] does the matching rather than the test.
    fn gh_serving_comment(body: &str, at: DateTime<Utc>) -> GH {
        let comment = serde_json::json!([[{
            "body": body,
            "issue_url": "https://api.github.com/repos/makewhatis/rhapsody/issues/12",
            "updated_at": at.to_rfc3339(),
        }]])
        .to_string();
        let empty = "[[]]".to_string();
        let run: RunFn = Box::new(move |args: &[&str]| {
            // Two endpoints per query (issues/comments, then pulls/comments); only the first has
            // the comment, exactly as GitHub answers a plain PR comment.
            let ep = args.last().copied().unwrap_or_default();
            Ok(if ep.contains("/issues/comments") {
                comment.clone().into_bytes()
            } else {
                empty.clone().into_bytes()
            })
        });
        GH::new("@symphony", Some(run))
    }

    /// Runs a composed comment through the WHOLE re-engagement path the daemon actually uses —
    /// `SummonSource::summons_since` (which applies the summon matcher) then
    /// `apply_github_summons` — and answers the author ticket's resulting `latest_summon_at`.
    async fn summon_at_after_posting(body: &str, at: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let gh = gh_serving_comment(body, at);
        let by_pr = gh
            .summons_since("makewhatis", "rhapsody", at - chrono::Duration::hours(1))
            .await
            .expect("the summons query succeeds");
        apply_github_summons(vec![author_issue()], &by_pr, "makewhatis", "rhapsody")
            .first()
            .and_then(|i| i.latest_summon_at)
    }

    // ── acceptance 1: a review-completion comment reopens the author's ticket ────────────────────

    /// THE acceptance criterion (design §14.2 "author re-summon needs the brand token"): the comment
    /// the daemon posts when a review left findings advances the author ticket's `latest_summon_at`
    /// through the EXISTING ghenrich/summons path — no new re-engagement mechanism, and no reliance
    /// on the review agent having remembered the token.
    #[tokio::test]
    async fn a_findings_completion_comment_reopens_the_author_ticket() {
        let at = utc(2026, 9, 2, 16, 55);
        let body = re_engage_comment(&completion(false, "@symphony"));
        assert_eq!(
            summon_at_after_posting(&body, at).await,
            Some(at),
            "the findings comment must advance latest_summon_at through the real summons path"
        );
    }

    /// The same guarantee for every token spelling an installation can be on, including a custom
    /// one that the brand aliases do NOT cover — the case where a hard-coded `@symphony` in the
    /// template would produce a comment that matches nothing.
    #[test]
    fn the_findings_comment_carries_whichever_token_is_configured() {
        for token in ["@symphony", "@rhapsody", "@Rhapsody", "@bot", "@a|b"] {
            let body = re_engage_comment(&completion(false, token));
            assert!(
                summons_author(&body, token),
                "a findings comment must summon under the configured token {token:?}"
            );
            assert!(
                body.starts_with(token),
                "the token must LEAD the body so no later edit can bury it in a word or a URL"
            );
        }
    }

    // ── acceptance 2: a tokenless completion does NOT reopen — the documented no-op ──────────────

    /// The contract's other half, pinned rather than assumed: a completion comment with no token
    /// re-engages nobody. An APPROVED review posts exactly such a comment on purpose (§15-c —
    /// approval pauses the loop, so there is nothing to ask the author for), which makes the no-op a
    /// tested production path rather than a hypothetical.
    #[tokio::test]
    async fn a_tokenless_approved_completion_does_not_reopen_the_author_ticket() {
        let at = utc(2026, 9, 2, 16, 55);
        let body = re_engage_comment(&completion(true, "@symphony"));
        assert!(
            summon_at_after_posting(&body, at).await.is_none(),
            "an approved completion must leave the author's ticket un-reopened"
        );
        // And under every token spelling, not just the one the fake `gh` was built with — an
        // approval must never accidentally summon.
        for token in ["@symphony", "@rhapsody", "@bot"] {
            assert!(
                !summons_author(&re_engage_comment(&completion(true, token)), token),
                "an approved completion must carry no summon token ({token:?})"
            );
        }
    }

    /// The predicate that judges both halves runs the REAL matcher, so a token buried in a URL or an
    /// identifier reads as "will not re-engage" — which is what it is. A substring test would report
    /// re-engagement that never happens, reintroducing the silent failure inside the check meant to
    /// catch it.
    #[test]
    fn summons_author_applies_the_matchers_boundary_rules() {
        assert!(summons_author("@symphony please look", "@symphony"));
        assert!(!summons_author(
            "see https://example.test/@symphony/x",
            "@symphony"
        ));
        assert!(!summons_author("mail jp@symphony.dev", "@symphony"));
        assert!(!summons_author("nothing here", "@symphony"));
        // The brand spellings are synonyms, exactly as the Linear path treats them.
        assert!(summons_author("@rhapsody please look", "@symphony"));
    }

    // ── the off-loop task ────────────────────────────────────────────────────────────────────────

    /// A recording [`PrCommentSink`]: every comment it was asked to post, or a failure.
    struct RecordingSink {
        posted: Mutex<Vec<(String, i64, String)>>,
        fail: bool,
    }

    impl RecordingSink {
        fn new(fail: bool) -> Arc<RecordingSink> {
            Arc::new(RecordingSink {
                posted: Mutex::new(Vec::new()),
                fail,
            })
        }
        fn taken(&self) -> Vec<(String, i64, String)> {
            self.posted.lock().expect("posted lock").clone()
        }
    }

    #[async_trait::async_trait]
    impl PrCommentSink for RecordingSink {
        async fn post_pr_comment(
            &self,
            owner: &str,
            repo: &str,
            number: i64,
            body: &str,
        ) -> PrCommentResult {
            self.posted.lock().expect("posted lock").push((
                format!("{owner}/{repo}"),
                number,
                body.to_string(),
            ));
            if self.fail {
                return Err("gh pr comment: boom".into());
            }
            Ok(())
        }
    }

    async fn drain(deps: ReviewNotifyDeps, items: Vec<ReviewCompletion>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        for c in items {
            tx.send(c).expect("send");
        }
        drop(tx); // the task returns when every sender is gone
        run_review_notify_task(crate::control_loop::CancelSignal::new().wait(), deps, rx).await;
    }

    /// The task posts the host-composed body at the completion's own coordinates.
    #[tokio::test]
    async fn the_task_posts_the_composed_comment_on_the_reviewed_pull_request() {
        let sink = RecordingSink::new(false);
        drain(
            ReviewNotifyDeps {
                comments: Some(Arc::clone(&sink) as Arc<dyn PrCommentSink>),
            },
            vec![completion(false, "@symphony")],
        )
        .await;
        let posted = sink.taken();
        assert_eq!(posted.len(), 1);
        assert_eq!(posted[0].0, "makewhatis/rhapsody");
        assert_eq!(posted[0].1, 12);
        assert_eq!(
            posted[0].2,
            re_engage_comment(&completion(false, "@symphony"))
        );
    }

    /// A failed post costs one re-engagement and nothing else: the task keeps serving the queue
    /// rather than dying on it, and it never retries (a duplicated summons would reopen the author's
    /// run twice for one round).
    #[tokio::test]
    async fn a_failed_post_is_not_retried_and_does_not_stop_the_task() {
        let sink = RecordingSink::new(true);
        drain(
            ReviewNotifyDeps {
                comments: Some(Arc::clone(&sink) as Arc<dyn PrCommentSink>),
            },
            vec![
                completion(false, "@symphony"),
                completion(true, "@symphony"),
            ],
        )
        .await;
        assert_eq!(
            sink.taken().len(),
            2,
            "each completion is attempted exactly once, and a failure does not end the task"
        );
    }

    /// No sink ⇒ no post, and no panic: a daemon that cannot reach GitHub simply re-engages nobody.
    #[tokio::test]
    async fn a_missing_comment_sink_posts_nothing() {
        drain(
            ReviewNotifyDeps { comments: None },
            vec![completion(false, "@symphony")],
        )
        .await;
    }

    // ── the planner's gates (§16) ────────────────────────────────────────────────────────────────

    fn orch(teams_enabled: bool, mode: ReviewMode, token: &str) -> Orchestrator {
        let tracker = Arc::new(Fake::new());
        let mut eff = empty_effective(tracker.clone());
        eff.cfg.tracker.summon_token = token.to_string();
        let mut proj = empty_resolved_project("rhapsody", tracker);
        proj.repo = REPO_URL.to_string();
        eff.projects = vec![proj];
        let mut o = Orchestrator::new("WORKFLOW.md");
        o.eff = Some(eff);
        o.teams = Some(Teams {
            enabled: teams_enabled,
            review: Review {
                mode,
                ..Review::default()
            },
            roster: vec![Identity {
                name: "alice".to_string(),
                ..Default::default()
            }],
            ..Teams::disabled()
        });
        o.set_store(Arc::new(
            Sqlite::open(StorePath::InMemory).expect("open in-memory store"),
        ));
        o
    }

    fn run() -> ReviewRun {
        ReviewRun {
            owner: "makewhatis".to_string(),
            repo: "rhapsody".to_string(),
            number: 12,
            reviewer: "bob".to_string(),
            author: "alice".to_string(),
            team_id: "team-1".to_string(),
            repo_url: REPO_URL.to_string(),
            head_sha: HEAD.to_string(),
            introduced_by: "handoff".to_string(),
        }
    }

    /// §16's gate: the completion comment exists ONLY with Teams on AND `review.mode: ticketless`.
    /// Teams off and the ticket fan-out are untouched — neither can post as the daemon.
    #[test]
    fn only_a_teams_on_ticketless_daemon_plans_a_completion_comment() {
        let cases = [
            (true, ReviewMode::Ticketless, true),
            (true, ReviewMode::Tickets, false),
            (true, ReviewMode::Off, false),
            (false, ReviewMode::Ticketless, false),
        ];
        for (enabled, mode, want) in cases {
            let o = orch(enabled, mode, "@symphony");
            assert_eq!(
                o.plan_review_notify(&run(), false).is_some(),
                want,
                "teams_enabled={enabled} mode={mode:?}"
            );
        }
    }

    /// The planned comment carries the CONFIGURED token, and an installation that configured none
    /// falls back to the shipped default rather than composing a comment that summons nobody.
    #[test]
    fn the_plan_resolves_the_configured_summon_token_and_defaults_when_empty() {
        let o = orch(true, ReviewMode::Ticketless, "@bot");
        let c = o
            .plan_review_notify(&run(), false)
            .expect("the ticketless daemon plans a comment");
        assert_eq!(c.summon_token, "@bot");
        assert!(summons_author(&re_engage_comment(&c), "@bot"));

        let o = orch(true, ReviewMode::Ticketless, "   ");
        let c = o
            .plan_review_notify(&run(), false)
            .expect("the ticketless daemon plans a comment");
        assert_eq!(c.summon_token, rhapsody_core::SUMMON_TOKEN_SYMPHONY);
    }

    /// Coordinates that can never name a comment plan nothing, rather than handing the task a post
    /// that must fail.
    #[test]
    fn incomplete_coordinates_plan_nothing() {
        let o = orch(true, ReviewMode::Ticketless, "@symphony");
        for mutate in [
            (|r: &mut ReviewRun| r.owner.clear()) as fn(&mut ReviewRun),
            |r: &mut ReviewRun| r.repo.clear(),
            |r: &mut ReviewRun| r.number = 0,
        ] {
            let mut r = run();
            mutate(&mut r);
            assert!(o.plan_review_notify(&r, false).is_none(), "{r:?}");
        }
    }

    /// The plan copies the head PINNED at dispatch (design §14.1 F-SHA), so the comment names the
    /// commit that was actually read rather than wherever the branch is by the time it is posted.
    #[test]
    fn the_comment_names_the_pinned_head_not_a_later_one() {
        let o = orch(true, ReviewMode::Ticketless, "@symphony");
        let c = o
            .plan_review_notify(&run(), false)
            .expect("the ticketless daemon plans a comment");
        assert_eq!(c.head_sha, HEAD);
        assert!(re_engage_comment(&c).contains(&HEAD[..7]));
    }

    // ── the `gh` seam ────────────────────────────────────────────────────────────────────────────

    /// The exact `gh` invocation, pinned: a wrong flag here is a comment that never reaches the pull
    /// request, and the only place it can be caught is the argv.
    #[tokio::test]
    async fn the_gh_seam_posts_a_pr_comment_with_the_composed_body() {
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let run_fn: RunFn = Box::new(move |args: &[&str]| {
            sink.lock().expect("argv lock").push(args.join(" "));
            Ok(Vec::new())
        });
        let gh = GH::new("@symphony", Some(run_fn));
        gh.post_pr_comment("makewhatis", "rhapsody", 12, "hello")
            .await
            .expect("the post succeeds");
        assert_eq!(
            seen.lock().expect("argv lock").as_slice(),
            ["pr comment 12 --repo makewhatis/rhapsody --body hello".to_string()]
        );
    }

    /// Incomplete coordinates and an empty body are refused BEFORE `gh` is spawned. A read at a
    /// coordinate that cannot exist has a true answer; being asked to post a comment nobody can see
    /// is a caller bug, and an empty body additionally cannot carry the token — it would look like
    /// re-engagement while being the documented no-op.
    #[tokio::test]
    async fn the_gh_seam_refuses_a_comment_that_could_never_be_read() {
        let calls = Arc::new(Mutex::new(0usize));
        let counter = Arc::clone(&calls);
        let run_fn: RunFn = Box::new(move |_args: &[&str]| {
            *counter.lock().expect("call lock") += 1;
            Ok(Vec::new())
        });
        let gh = GH::new("@symphony", Some(run_fn));
        for (owner, repo, number, body) in [
            ("", "rhapsody", 12, "hi"),
            ("makewhatis", "", 12, "hi"),
            ("makewhatis", "rhapsody", 0, "hi"),
            ("makewhatis", "rhapsody", 12, ""),
        ] {
            assert!(
                gh.post_pr_comment(owner, repo, number, body).await.is_err(),
                "{owner}/{repo}#{number} body={body:?}"
            );
        }
        assert_eq!(
            *calls.lock().expect("call lock"),
            0,
            "no `gh` process may be spawned for a comment that cannot be posted"
        );
    }

    // ── the map an author actually needs (documentation of the second condition) ─────────────────

    /// The half of re-engagement this slice does NOT control, pinned so it is a known limit rather
    /// than a surprise: the comment reopens an author ticket only if the poller's snapshot links
    /// that pull request to it. An installation whose Linear carries no GitHub attachments has empty
    /// `linked_prs`, and a perfectly token-bearing comment then re-engages nobody.
    #[tokio::test]
    async fn a_token_bearing_comment_reopens_nothing_when_the_pr_is_not_linked() {
        let at = utc(2026, 9, 2, 16, 55);
        let body = re_engage_comment(&completion(false, "@symphony"));
        let gh = gh_serving_comment(&body, at);
        let by_pr = gh
            .summons_since("makewhatis", "rhapsody", at - chrono::Duration::hours(1))
            .await
            .expect("the summons query succeeds");
        assert_eq!(by_pr.len(), 1, "the comment IS a summons");
        let unlinked = Issue {
            identifier: "STUDIO-999".into(),
            linked_prs: None,
            ..Default::default()
        };
        let got: HashMap<_, _> =
            apply_github_summons(vec![unlinked], &by_pr, "makewhatis", "rhapsody")
                .into_iter()
                .map(|i| (i.identifier, i.latest_summon_at))
                .collect();
        assert_eq!(got.get("STUDIO-999"), Some(&None));
    }
}
