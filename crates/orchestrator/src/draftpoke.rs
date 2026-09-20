//! draftpoke — poking a finished run's author when its pull request is still a draft (STUDIO-962).
//!
//! **No Go v0.4.0 counterpart.** Ticketless review is a Rhapsody addition end to end, and this is
//! the backstop the first run of it left out.
//!
//! # Why a draft is never right here, and why the daemon still cannot fix it
//!
//! A draft exists to withhold a pull request from reviewers until it is worth their attention.
//! Rhapsody's reviewers are dispatched by the daemon, not by a notification, so a draft buys
//! nothing — and it costs everything: [`crate::runautomerge`] refuses a draft outright, and nothing
//! in the pipeline ever marks one ready. On 2026-09-17 makewhatis/booch#537 sat approved and green
//! for **4h55m** for exactly this reason, refused 361 times, until a human marked it ready by hand.
//!
//! The obvious fix — have the daemon un-draft it — is the one this module must not take.
//! **Un-drafting is the author's declaration that the work is ready for review.** Having the daemon
//! do it silently converts a deliberate signal into a no-op and removes the only way an author can
//! hold their own work back. So the daemon does not mark a pull request ready; it POKES the author,
//! and the poke is a summons: a comment leading with the configured summon token, which reopens the
//! author's run with this text as its instruction ([`crate::reviewnotify`]'s mechanism, the same one
//! a findings verdict uses). The only write in this module is that comment.
//!
//! # The run has finished by construction
//!
//! The trigger is *run finished **and** still draft*, and "finished" means the HANDOFF, not merely
//! the process exiting. That is enforced structurally rather than by a flag: the only pull requests
//! the daemon ever observes are the ones in [`crate::reviewintro`]'s watch set, and a row exists
//! there only because a run handed its pull request over ([`crate::reviewintro`]) or the adoption
//! sweep found a parked one ([`crate::reviewadopt`]). A run that merely exits without a handoff
//! introduces no row and is never seen here.
//!
//! The one guard that remains is [`Orchestrator::ticket_run_live`](crate::orchestrator::Orchestrator):
//! a draft is entirely normal mid-run, so a pull request whose author is running RIGHT NOW is never
//! poked. That is what covers the re-engaged author — the run a review's findings reopened — who is
//! mid-fix with the pull request still a draft.
//!
//! # One poke per head, and a human at the end
//!
//! The draft state persists until the author acts, so a per-tick summons is a re-dispatch loop —
//! the churn STUDIO-956 exists to bound. The watcher's own `plan_draft_poke` therefore remembers
//! the head it last poked and says nothing again while the head is unchanged. When the author
//! pushes but leaves it a draft, the new head is poked once too.
//!
//! An author who deliberately keeps a pull request in draft needs an out, so the poking is bounded:
//! after [`MAX_DRAFT_POKES`] distinct heads the daemon stops poking and ESCALATES to a human — a
//! room post and a tokenless comment on the pull request naming how many times it was poked. The
//! escalation carries no summon token: it is not asking the author again, it is telling a human the
//! author will not.
//!
//! # Off the loop
//!
//! The decision is made on the control task, where the watch set and `running` are single-writer;
//! the two writes — a `gh` comment and a room append — happen off it, on the review watcher's own
//! task, for [`crate::runautomerge`]'s and [`crate::reviewnotify`]'s reason: a slow `gh` or a
//! slow disk must park the task that owns this subsystem's I/O and nothing else.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use rhapsody_config::room::{Message, RoomLog};

use crate::ghsummons::PrCommentSink;
use crate::prstate::PrCoord;

/// The `from` every escalation post is host-stamped with. Same value as
/// `crate::triage::MANAGER_IDENTITY` and the adjudicator's, restated rather than imported because
/// the manager is one function however many of its halves exist.
pub const MANAGER_IDENTITY: &str = "@manager";

/// How many distinct HEADS one pull request may be poked at before the daemon stops poking and asks
/// a human.
///
/// In HEADS and not attempts: a poke is once per head ([`DraftPokeState`]), so this bounds how many
/// times an author who keeps it a draft but keeps pushing may be asked. Three is enough for an
/// author who simply forgot once, and far below the point where a machine repeating itself at a
/// human is noise.
pub const MAX_DRAFT_POKES: usize = 3;

/// One pull request the daemon must poke: a run has finished, and its pull request is still a draft.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftPokePlan {
    pub pr: PrCoord,
    /// The head being poked — the same head is never poked twice ([`DraftPokeState`]).
    pub head: String,
    /// The teammate who authored it. Named in the comment so the author knows who is being asked;
    /// empty means unknown, and the prose degrades to a role.
    pub author: String,
    /// The configured summon token, resolved once on the control task and carried so the comment
    /// that is POSTED and the predicate that judges it cannot disagree across a config reload.
    pub summon_token: String,
    /// How many times this pull request has ALREADY been poked, at earlier heads. `0` on the first.
    pub pokes: usize,
}

/// One pull request the daemon stops poking and hands to a human.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftEscalation {
    pub pr: PrCoord,
    /// The teammate who authored it, for the record. Empty when unknown.
    pub author: String,
    /// How many times it was poked before the daemon gave up — the count the escalation reports.
    pub pokes: usize,
}

/// What one still-draft pull request earned this tick: a poke, or (once) the human escalation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DraftNudge {
    Poke(DraftPokePlan),
    Escalate(DraftEscalation),
}

/// Per-pull-request poke bookkeeping, keyed by
/// `churn_key` on the control task.
///
/// In memory rather than a column, for [`crate::reviewwatch::REVIEW_ROUNDS_PER_PR_CAP`]'s reason:
/// it is a churn floor, not an audit record. A restart forgets it, which for an operator who
/// restarted the daemon to unstick something is the correct outcome — the worst it costs is one
/// more poke at a head already poked.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DraftPokeState {
    /// The head last poked. A DIFFERENT head is a new poke; the same head is the same poke.
    pub poked_head: String,
    /// How many distinct heads this pull request has been poked at.
    pub pokes: usize,
    /// Whether the human escalation has already been made. Once true, this pull request is silent
    /// until it stops being a draft.
    pub escalated: bool,
}

/// The host-composed summons a poke posts.
///
/// **Written by the host, never by an agent**, and it LEADS with the token — [`crate::reviewnotify`]'s
/// rule and for its reason: the matcher requires start-of-string or whitespace before the token, and
/// leading with it means no later edit to this template can bury the token inside a URL or a word
/// and quietly turn the poke into a no-op.
///
/// The body is written to be read by the AUTHOR'S AGENT, not only by a human, because
/// [`crate::ghenrich::apply_github_summons`] copies the summoning comment into `latest_summon_body`,
/// which [`crate::message`] hands to the reopened run as its instruction. So it says the one thing
/// the run must do — mark the pull request ready — and why the daemon cannot.
pub fn poke_comment(plan: &DraftPokePlan) -> String {
    let token = &plan.summon_token;
    let who = if plan.author.is_empty() {
        "The author"
    } else {
        plan.author.as_str()
    };
    let n = plan.pokes + 1;
    let pr = &plan.pr;
    format!(
        "{token} `{pr}` is still a **draft** and the run that opened it has finished.\n\
         \n\
         {who}: publish it — mark it ready for review (`gh pr ready`). The daemon will not do it \
         for you: un-drafting is the author's own declaration that the work is ready, and auto-merge \
         refuses a draft, so nothing progresses while it stays one. This is poke {n} of \
         {MAX_DRAFT_POKES}.\n\
         \n\
         If you mean to keep it a draft, say so on this pull request — the daemon stops poking after \
         {MAX_DRAFT_POKES} and asks a human instead.\n"
    )
}

/// The host-composed escalation, posted to the room and the pull request.
///
/// Deliberately carries NO summon token: it is not asking the author a fourth time, it is telling a
/// human that the author will not. It names the count so the human knows how long this has gone on.
pub fn escalation_body(esc: &DraftEscalation) -> String {
    let who = if esc.author.is_empty() {
        "its author"
    } else {
        esc.author.as_str()
    };
    let n = esc.pokes;
    let pr = &esc.pr;
    format!(
        "`{pr}` has been a **draft** across {n} poke{}, and {who} has not published it. A human \
         must mark it ready for review or close it — the daemon has stopped poking.\n\
         \n\
         The daemon never marks a pull request ready itself: un-drafting is the author's declaration \
         that the work is ready for review, and auto-merge refuses a draft, so this pull request \
         will not progress on its own.\n",
        if n == 1 { "" } else { "s" }
    )
}

/// Everything an off-loop poke or escalation needs. No `Orchestrator`, no store, no control channel
/// — the off-loop guarantee, in the type, as [`crate::reviewnotify::ReviewNotifyDeps`] states it.
pub struct DraftPokeDeps {
    /// Where the poke (and the escalation's half) is posted. `None` disables the poke: a daemon that
    /// cannot reach GitHub cannot re-engage anybody, and there is no second route worth inventing —
    /// the author's ticket is reopened by a PR comment or not at all.
    pub comments: Option<Arc<dyn PrCommentSink>>,
    /// The room the escalation is audited in. `None` when there is no on-disk runtime home — the
    /// escalation still lands on the pull request, and the log line still says what happened.
    pub room: Option<Arc<dyn RoomLog>>,
}

/// Performs one nudge, off the control task.
///
/// Infallible by contract, like [`crate::runautomerge::perform_auto_merge`] and the adjudicator:
/// there is no caller with anything to do about a failure, and every branch is logged where it
/// happens. A failed poke is NOT retried at the same head — the head did not move, and a retry loop
/// against a GitHub that just refused is the duplicate summons [`crate::reviewnotify`] already
/// refuses to risk.
pub async fn perform_nudge(nudge: &DraftNudge, deps: &DraftPokeDeps, at: DateTime<Utc>) {
    match nudge {
        DraftNudge::Poke(plan) => {
            let Some(comments) = deps.comments.as_ref() else {
                tracing::warn!(
                    pr = %plan.pr,
                    "draft poke: no GitHub comment sink, so the author is not poked"
                );
                return;
            };
            let body = poke_comment(plan);
            match comments
                .post_pr_comment(&plan.pr.owner, &plan.pr.repo, plan.pr.number, &body)
                .await
            {
                Ok(()) => tracing::info!(
                    pr = %plan.pr,
                    head = %plan.head,
                    author = %plan.author,
                    poke = plan.pokes + 1,
                    "draft poke: summoned the author to publish a draft pull request"
                ),
                Err(e) => tracing::warn!(
                    pr = %plan.pr,
                    head = %plan.head,
                    err = %e,
                    "draft poke: the summons could not be posted; the daemon will not poke again \
                     until the head moves"
                ),
            }
        }
        DraftNudge::Escalate(esc) => {
            let body = escalation_body(esc);
            if let Some(room) = deps.room.as_ref() {
                let mut msg = Message::room(MANAGER_IDENTITY, at, body.clone());
                msg.refs = vec![esc.pr.to_string()];
                if let Err(e) = room.append(&msg) {
                    tracing::warn!(
                        pr = %esc.pr,
                        err = %e,
                        "draft poke: the escalation could not be posted to the room; the pull \
                         request comment is unaffected"
                    );
                }
            }
            if let Some(comments) = deps.comments.as_ref()
                && let Err(e) = comments
                    .post_pr_comment(&esc.pr.owner, &esc.pr.repo, esc.pr.number, &body)
                    .await
            {
                tracing::warn!(
                    pr = %esc.pr,
                    err = %e,
                    "draft poke: the escalation could not be recorded on the pull request; the \
                     room post is unaffected"
                );
            }
            tracing::warn!(
                pr = %esc.pr,
                author = %esc.author,
                pokes = esc.pokes,
                "draft poke: a draft pull request was ignored across every poke; handing it to a \
                 human"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;
    use rhapsody_config::room::{CaughtUp, Cursor, RoomError};

    use super::*;
    use crate::ghsummons::PrCommentResult;

    fn coord() -> PrCoord {
        PrCoord::new("makewhatis", "rhapsody", 537)
    }

    fn plan(head: &str, pokes: usize) -> DraftPokePlan {
        DraftPokePlan {
            pr: coord(),
            head: head.to_string(),
            author: "alice".to_string(),
            summon_token: "@symphony".to_string(),
            pokes,
        }
    }

    /// The poke is a summons: the token LEADS, so the matcher that reopens the author's run can
    /// never be defeated by a later edit to where the token sits.
    #[test]
    fn the_poke_comment_leads_with_the_summon_token_and_names_the_action() {
        let body = poke_comment(&plan("abc123", 0));
        assert!(
            crate::reviewnotify::summons_author(&body, "@symphony"),
            "the poke must re-engage the author: {body}"
        );
        assert!(body.starts_with("@symphony"), "the token leads: {body}");
        assert!(
            body.contains("makewhatis/rhapsody#537"),
            "the pull request is named: {body}"
        );
        assert!(body.contains("draft"), "{body}");
        assert!(body.contains("mark it ready"), "{body}");
    }

    /// The escalation is NOT a summons — it must not reopen the author's run a fourth time.
    #[test]
    fn the_escalation_body_is_tokenless_and_names_the_poke_count() {
        let body = escalation_body(&DraftEscalation {
            pr: coord(),
            author: "alice".to_string(),
            pokes: 3,
        });
        assert!(
            !crate::reviewnotify::summons_author(&body, "@symphony"),
            "the escalation must not summon: {body}"
        );
        assert!(body.contains('3'), "the count is named: {body}");
        assert!(
            body.contains("makewhatis/rhapsody#537"),
            "the pull request is named: {body}"
        );
        assert!(body.contains("alice"), "{body}");
    }

    #[derive(Default)]
    struct RecordingComments(Mutex<Vec<String>>);

    #[async_trait]
    impl PrCommentSink for RecordingComments {
        async fn post_pr_comment(
            &self,
            _owner: &str,
            _repo: &str,
            _number: i64,
            body: &str,
        ) -> PrCommentResult {
            self.0.lock().expect("lock").push(body.to_string());
            Ok(())
        }
    }

    #[derive(Default)]
    struct RecordingRoom(Mutex<Vec<Message>>);

    impl RoomLog for RecordingRoom {
        fn append(&self, msg: &Message) -> Result<String, RoomError> {
            self.0.lock().expect("lock").push(msg.clone());
            Ok("room:1".to_string())
        }
        fn read_since(&self, _: &str, _: &Cursor, _: usize) -> Result<CaughtUp, RoomError> {
            Err(RoomError::Invalid("unused in this test".to_string()))
        }
        fn read_forward(&self, _: &str, _: &Cursor, _: usize) -> Result<CaughtUp, RoomError> {
            Err(RoomError::Invalid("unused in this test".to_string()))
        }
    }

    /// The one write a poke performs is a comment — the daemon never marks a pull request ready
    /// itself, and this is the test that says so: the only seam a poke touches is `PrCommentSink`.
    #[tokio::test]
    async fn a_poke_posts_exactly_one_comment_and_touches_no_other_surface() {
        let comments = Arc::new(RecordingComments::default());
        let room = Arc::new(RecordingRoom::default());
        let deps = DraftPokeDeps {
            comments: Some(Arc::clone(&comments) as Arc<dyn PrCommentSink>),
            room: Some(Arc::clone(&room) as Arc<dyn RoomLog>),
        };
        perform_nudge(&DraftNudge::Poke(plan("abc", 0)), &deps, Utc::now()).await;
        assert_eq!(comments.0.lock().expect("lock").len(), 1);
        assert!(
            room.0.lock().expect("lock").is_empty(),
            "a poke is a message to the author, not a room announcement"
        );
    }

    /// The escalation is recorded on BOTH surfaces, and no summon token reaches either — the author
    /// is not being asked again.
    #[tokio::test]
    async fn an_escalation_lands_in_the_room_and_on_the_pull_request() {
        let comments = Arc::new(RecordingComments::default());
        let room = Arc::new(RecordingRoom::default());
        let deps = DraftPokeDeps {
            comments: Some(Arc::clone(&comments) as Arc<dyn PrCommentSink>),
            room: Some(Arc::clone(&room) as Arc<dyn RoomLog>),
        };
        perform_nudge(
            &DraftNudge::Escalate(DraftEscalation {
                pr: coord(),
                author: "alice".to_string(),
                pokes: 3,
            }),
            &deps,
            Utc::now(),
        )
        .await;
        let posted = comments.0.lock().expect("lock").clone();
        assert_eq!(posted.len(), 1);
        assert!(!crate::reviewnotify::summons_author(
            &posted[0],
            "@symphony"
        ));
        let msgs = room.0.lock().expect("lock").clone();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].from, MANAGER_IDENTITY);
        assert_eq!(msgs[0].refs, vec!["makewhatis/rhapsody#537".to_string()]);
    }
}
