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
//! for **4h55m** for exactly this reason, auto-merge refusing it 146 times, until a human marked it
//! ready by hand.
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
//! the process exiting. That is enforced structurally rather than by a flag: the poke only applies
//! to a pull request with an ORIGIN TICKET, so a draft is summonable only when [`crate::reviewintro`]
//! recorded a run handing its pull request over or [`crate::reviewadopt`] adopted a parked one. A run
//! that merely exits without a handoff introduces no row, and a `console:` row a `reviewconsole`
//! merge introduced carries no ticket to reopen, so it is never poked either.
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
//! An author who deliberately keeps a pull request in draft needs an out, so the poking is bounded
//! on TWO axes, because the incident this feature was filed for was a STATIC head:
//!
//! * [`MAX_DRAFT_POKES`] pokes — the bound for an author who keeps pushing without ever publishing.
//!   The ledger remembers only the head poked LAST, so this counts attempts, not distinct heads: a
//!   force-push back to an earlier head is a fresh poke, and `A → B → A` spends the whole budget.
//! * [`MAX_DRAFT_POKE_SWEEPS`] consecutive sweeps at the SAME head — the bound for the shape that
//!   actually happens (booch#537 never moved its head). A done-nothing author would otherwise be
//!   poked once and then heard from never again, which is the parking this ticket's title names.
//!
//! Either bound the daemon stops poking and ESCALATES to a human — a room post and a tokenless
//! comment on the pull request naming how many times it was poked. The escalation carries no summon
//! token: it is not asking the author again, it is telling a human the author will not.
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
use crate::triage::MANAGER_IDENTITY;

/// How many times one pull request may be poked before the daemon stops poking and asks a human.
///
/// In ATTEMPTS and not distinct heads: a poke is once per head CONSECUTIVELY ([`DraftPokeState`]),
/// so the ledger suppresses a repeat only of the head poked last. A force-push back to an earlier
/// head is a fresh poke, and `A → B → A` reaches this bound just as `A → B → C` does. Three is
/// enough for an author who simply forgot once or twice, and far below the point where a machine
/// repeating itself at a human is noise.
pub const MAX_DRAFT_POKES: usize = 3;

/// How many CONSECUTIVE watcher sweeps the SAME poked head may stay a draft before the daemon stops
/// poking and asks a human.
///
/// This is the second bound, and it is the one that makes the human backstop reachable in the shape
/// the ticket was filed for. [`MAX_DRAFT_POKES`] is only ever reached by a head that MOVES (the
/// consecutive rule suppresses a repeat of the head last poked), so an author who does nothing —
/// the head never moves — would be poked once and then heard from never again, and the pull request
/// would park exactly as makewhatis/booch#537 did. A draft that stays at one head across this many
/// sweeps is not an author mid-push; it is an author who is not coming.
///
/// Thirty sweeps was about an hour when the watcher's cadence was the pinned 120s
/// ([`crate::prstate::PR_STATE_POLL_INTERVAL`]) and EVERY WATCHED PULL REQUEST ANSWERED EVERY TICK.
/// STUDIO-974 made that cadence a hot-reloadable config key defaulting to 15s, so the same thirty
/// sweeps is now about eight minutes at the default — the wall-clock grace this bound was sized for
/// shrank with the tick, and re-scaling the sweep count is a decision for the maintainer rather than
/// a silent one here. The count still advances only on a sweep that actually observed this pull
/// request, and the watcher asks about a bounded number of coordinates per tick on a rotating cursor,
/// so a larger watch set or a flaky `gh` makes the grace a floor rather than a promise.
pub const MAX_DRAFT_POKE_SWEEPS: usize = 30;

/// One pull request the daemon must poke: a run has finished, and its pull request is still a draft.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftPokePlan {
    pub pr: PrCoord,
    /// The head being poked — a head is never poked twice consecutively ([`DraftPokeState`]).
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
    /// The ORIGIN TICKET — the run the summons would have reopened. Carried onto the room post's
    /// `refs` beside the pull request coordinate so the escalation re-grounds against the same
    /// candidate map every other manager room post does (`render_ref` re-grounds ticket-shaped refs
    /// only), and a teammate reading the room back gets the ticket to reopen rather than only a
    /// pull request to publish.
    pub identifier: String,
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
/// it is a churn floor, not an audit record. A restart forgets the WHOLE ledger — `escalated`
/// included — so a still-draft pull request already handed to a human is poked afresh, and can earn
/// a second human escalation, once per restart for as long as the draft stands. That is the cost of
/// not persisting a churn floor; persisting the ladder is a larger decision than this feature.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DraftPokeState {
    /// The head last poked. A DIFFERENT head is a new poke; the same head as the LAST poke is the
    /// same poke. Only one head is remembered, so a force-push back to an earlier head is a fresh
    /// poke.
    pub poked_head: String,
    /// How many times this pull request has been poked — ATTEMPTS, not distinct heads. The ledger
    /// remembers only the head poked last, so `A → B → A` reaches three here with two distinct heads.
    pub pokes: usize,
    /// How many CONSECUTIVE sweeps the [`Self::poked_head`] has been observed still a draft since it
    /// was poked. Reset when the head moves (a new head is a fresh poke); never advanced while the
    /// author's run is live; and never advanced on a tick GitHub could not answer, since an unstated
    /// `isDraft` is not an observation. The escalation fires at [`MAX_DRAFT_POKE_SWEEPS`].
    pub unanswered_sweeps: usize,
    /// Whether the human escalation has already been made. Once true, this pull request is silent
    /// until it stops being a draft.
    ///
    /// Latched when the escalation is PLANNED, before either of its writes is attempted, and
    /// [`crate::draftpoke::perform_nudge`] cannot report a delivery failure back to the control
    /// task. So the latch stands even when both writes failed: the terminal WARN says the
    /// escalation "reached no surface — no human was told", and the pull request stays silent at
    /// every head. The only things that clear it are the draft resolving (published, which drops
    /// this entry, or retired) or a daemon restart — so on a draft that never resolves, a restart
    /// is what re-arms the poke. An operator who greps that WARN should read it as "restart me or
    /// handle this by hand", not as a note about a write that failed.
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
         refuses a draft, so nothing progresses while it stays one. This is poke {n} of at most \
         {MAX_DRAFT_POKES}.\n\
         \n\
         The daemon does not poke forever: if the draft stands, it stops poking and asks a human \
         instead.\n"
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
        "`{pr}` is still a **draft** after the daemon made {n} attempt{} to have {who} publish it \
         since the run that opened it finished. A human must mark it ready for review or close it — \
         the daemon has stopped poking.\n\
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
            // Whether ANY surface actually accepted the escalation. The terminal log line below
            // must not claim a human now owns this pull request when neither write landed — the
            // one line an operator greps for would otherwise be false exactly when it matters.
            // Note the asymmetry with the ledger: `DraftPokeState::escalated` was already latched
            // when this nudge was planned and is not cleared by a failed write, so this line is the
            // ONLY signal that the escalation reached nobody — "reached no surface" means the pull
            // request stays silent until something clears the latch: a daemon restart, a human
            // publishing it by hand, or the pull request leaving the watch set (see
            // `DraftPokeState::escalated`, which names all three).
            let mut told = false;
            if let Some(room) = deps.room.as_ref() {
                let mut msg = Message::room(MANAGER_IDENTITY, at, body.clone());
                // The ticket leads, the pull request follows: the ticket is what re-grounds against
                // the candidate map (`teamscompose::render_ref`), and the pull request is the
                // coordinate to act on. `mergeconsole`'s manager room line carries its ticket the
                // same way.
                msg.refs = vec![esc.identifier.clone(), esc.pr.to_string()];
                match room.append(&msg) {
                    Ok(_) => told = true,
                    Err(e) => tracing::warn!(
                        pr = %esc.pr,
                        err = %e,
                        "draft poke: the escalation could not be posted to the room; the pull \
                         request comment is unaffected"
                    ),
                }
            }
            if let Some(comments) = deps.comments.as_ref() {
                match comments
                    .post_pr_comment(&esc.pr.owner, &esc.pr.repo, esc.pr.number, &body)
                    .await
                {
                    Ok(()) => told = true,
                    Err(e) => tracing::warn!(
                        pr = %esc.pr,
                        err = %e,
                        "draft poke: the escalation could not be recorded on the pull request; the \
                         room post is unaffected"
                    ),
                }
            }
            if told {
                tracing::warn!(
                    pr = %esc.pr,
                    author = %esc.author,
                    pokes = esc.pokes,
                    "draft poke: a draft pull request was ignored across every poke; handing it to a \
                     human"
                );
            } else {
                tracing::warn!(
                    pr = %esc.pr,
                    author = %esc.author,
                    pokes = esc.pokes,
                    "draft poke: a draft pull request was ignored across every poke, but the \
                     escalation reached no surface — no human was told"
                );
            }
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
        // The count the poke reports, pinned as a PHRASE: a bare digit would be satisfied by the
        // pull request number (537 contains a 3), so it could not see the sentence being deleted.
        assert!(
            body.contains(&format!("poke 1 of at most {MAX_DRAFT_POKES}")),
            "the poke says which poke it is: {body}"
        );
    }

    /// The escalation is NOT a summons — it must not reopen the author's run a fourth time.
    #[test]
    fn the_escalation_body_is_tokenless_and_names_the_poke_count() {
        let body = escalation_body(&DraftEscalation {
            pr: coord(),
            identifier: "STUDIO-721".to_string(),
            author: "alice".to_string(),
            pokes: 3,
        });
        assert!(
            !crate::reviewnotify::summons_author(&body, "@symphony"),
            "the escalation must not summon: {body}"
        );
        // A PHRASE, not a bare digit: `537` in the pull request name would satisfy `contains('3')`,
        // leaving the assertion vacuous exactly when the count sentence is deleted.
        assert!(
            body.contains("made 3 attempts"),
            "the count is named: {body}"
        );
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

    /// A comment sink that refuses every write — the failing sink `RecordingComments` never had, so
    /// the escalation's error paths are finally observable.
    struct FailingComments;

    #[async_trait]
    impl PrCommentSink for FailingComments {
        async fn post_pr_comment(
            &self,
            _owner: &str,
            _repo: &str,
            _number: i64,
            _body: &str,
        ) -> PrCommentResult {
            Err("the comment sink is down".into())
        }
    }

    /// A room that refuses every append — the failure `RecordingRoom` never had, so the room arm's
    /// `Err` branch is finally observable. Mirrors [`FailingComments`] on the other surface.
    struct FailingRoom;

    impl RoomLog for FailingRoom {
        fn append(&self, _msg: &Message) -> Result<String, RoomError> {
            Err(RoomError::Invalid("the room is unwritable".to_string()))
        }
        fn read_since(&self, _: &str, _: &Cursor, _: usize) -> Result<CaughtUp, RoomError> {
            Err(RoomError::Invalid("unused in this test".to_string()))
        }
        fn read_forward(&self, _: &str, _: &Cursor, _: usize) -> Result<CaughtUp, RoomError> {
            Err(RoomError::Invalid("unused in this test".to_string()))
        }
    }

    fn escalation() -> DraftEscalation {
        DraftEscalation {
            pr: coord(),
            identifier: "STUDIO-721".to_string(),
            author: "alice".to_string(),
            pokes: 3,
        }
    }

    /// Runs `f` twice under a fresh recording subscriber (the second run is the measured one, the
    /// first forces every callsite to register — TRA-243), serialized against the other
    /// subscriber-installing tests.
    async fn captured<F, Fut>(f: F) -> Vec<crate::testsupport::CapturedEvent>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let _serial = crate::testsupport::TRACING_TEST_LOCK.lock().await;
        let (events, subscriber) = crate::testsupport::recording_subscriber();
        let guard = tracing::subscriber::set_default(subscriber);
        f().await;
        tracing::callsite::rebuild_interest_cache();
        events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        f().await;
        drop(guard);
        events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The one write a poke performs is a comment — the daemon never marks a pull request ready
    /// itself, and this is the test that says so: the only seam a poke touches is `PrCommentSink`,
    /// and the body it posts is an INSTRUCTION to the author rather than a statement that the
    /// daemon published it. `DraftPokeDeps` exposes no readiness seam to call; adding one would be
    /// a new field here and a visible diff, and this assertion is what would notice the body
    /// changed from asking to telling.
    #[tokio::test]
    async fn a_poke_posts_exactly_one_comment_and_touches_no_other_surface() {
        let comments = Arc::new(RecordingComments::default());
        let room = Arc::new(RecordingRoom::default());
        let deps = DraftPokeDeps {
            comments: Some(Arc::clone(&comments) as Arc<dyn PrCommentSink>),
            room: Some(Arc::clone(&room) as Arc<dyn RoomLog>),
        };
        perform_nudge(&DraftNudge::Poke(plan("abc", 0)), &deps, Utc::now()).await;
        let posted = comments.0.lock().expect("lock").clone();
        assert_eq!(posted.len(), 1);
        assert!(
            posted[0].contains("mark it ready for review"),
            "the author is asked to publish it, not told the daemon did: {}",
            posted[0]
        );
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
        perform_nudge(&DraftNudge::Escalate(escalation()), &deps, Utc::now()).await;
        let posted = comments.0.lock().expect("lock").clone();
        assert_eq!(posted.len(), 1);
        assert!(!crate::reviewnotify::summons_author(
            &posted[0],
            "@symphony"
        ));
        let msgs = room.0.lock().expect("lock").clone();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].from, MANAGER_IDENTITY);
        // The ticket LEADS (it is what `render_ref` re-grounds against the candidate map), and the
        // pull request coordinate follows.
        assert_eq!(
            msgs[0].refs,
            vec![
                "STUDIO-721".to_string(),
                "makewhatis/rhapsody#537".to_string()
            ]
        );
    }

    /// ⚠️ The terminal log line is honest: when NEITHER surface accepted the escalation, it must not
    /// claim a human now owns the pull request. This is the failure `RecordingComments` never had —
    /// it always returned `Ok`, so the one line an operator would grep for was never tested against
    /// a write that refused.
    #[tokio::test]
    async fn an_escalation_that_reaches_no_surface_does_not_claim_a_human_was_told() {
        // Two ways to reach no surface, and only the SECOND is a shape production can have: an
        // installation that pokes at all has a room (see `an_escalation_that_reaches_only_the_
        // _pull_request_still_hands_it_to_a_human`), so the reachable no-surface shape is a room
        // that REFUSED alongside a comment that refused. The `room: None` stanza drives the same
        // line with the room arm skipped entirely rather than entered and failed.
        for (what, room) in [
            ("no room at all", None),
            (
                "a room that refused",
                Some(Arc::new(FailingRoom) as Arc<dyn RoomLog>),
            ),
        ] {
            let events = captured(|| {
                let room = room.clone();
                async move {
                    let deps = DraftPokeDeps {
                        comments: Some(Arc::new(FailingComments) as Arc<dyn PrCommentSink>),
                        room,
                    };
                    perform_nudge(&DraftNudge::Escalate(escalation()), &deps, Utc::now()).await;
                }
            })
            .await;
            assert!(
                events
                    .iter()
                    .any(|e| e.level == "WARN" && e.message.contains("reached no surface")),
                "({what}) expected the honest no-surface line, got {events:?}"
            );
            assert!(
                !events
                    .iter()
                    .any(|e| e.message.contains("handing it to a human")),
                "({what}) nothing accepted the escalation, so the log must not claim a human was \
                 told: {events:?}"
            );
        }
    }

    /// ⚠️ The room arm's `Err` branch, which every other escalation test misses by construction:
    /// `RecordingRoom` always returns `Ok` and the two `room: None` cases never enter the block at
    /// all. A room append that FAILED must not count as having told anybody — with the arm taking
    /// `told = true` on its error, a daemon whose room append and comment both failed would still
    /// log "handing it to a human" when nobody was.
    ///
    /// Driven as room-Fail + comment-Ok rather than as a second no-surface case, so the assertion
    /// discriminates the arm itself: the surviving surface is the comment, and the panic message
    /// names which surface actually accepted.
    #[tokio::test]
    async fn a_room_that_refuses_the_escalation_is_not_counted_as_telling_a_human() {
        let comments = Arc::new(RecordingComments::default());
        let events = captured(|| {
            let comments = Arc::clone(&comments);
            async move {
                let deps = DraftPokeDeps {
                    comments: Some(comments as Arc<dyn PrCommentSink>),
                    room: Some(Arc::new(FailingRoom) as Arc<dyn RoomLog>),
                };
                perform_nudge(&DraftNudge::Escalate(escalation()), &deps, Utc::now()).await;
            }
        })
        .await;
        // The refusal is reported where it happened, and the comment is explicitly unaffected.
        assert!(
            events.iter().any(|e| e.level == "WARN"
                && e.message
                    .contains("the escalation could not be posted to the room")),
            "the room's refusal must be logged where it happens: {events:?}"
        );
        // The comment carried it, so a human WAS told — the room's failure must not suppress that.
        assert!(
            events
                .iter()
                .any(|e| e.message.contains("handing it to a human")),
            "the comment accepted the escalation, so a human was told: {events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|e| e.message.contains("reached no surface")),
            "the comment is a surface and it accepted: {events:?}"
        );
        assert_eq!(
            comments.0.lock().expect("lock").len(),
            2,
            "the comment is posted once per `captured` pass, both times"
        );
    }

    /// The OTHER half of the `told` OR, which the two tests above cannot see by construction: a
    /// comment that POSTED, with the room arm skipped entirely. `room: None` is not a reachable
    /// production shape — the same runtime home gates both `resolve_teams_path` and
    /// `resolve_room_dir`, and no watcher spawns without Teams (`run.rs:1332-1335` says the same) —
    /// so it is used here to drive the comment arm IN ISOLATION. With that arm neutered, a daemon
    /// whose escalation comment did post would emit "reached no surface — no human was told":
    /// jimmy's false line in the opposite direction.
    #[tokio::test]
    async fn an_escalation_that_reaches_only_the_pull_request_still_hands_it_to_a_human() {
        let events = captured(|| async {
            let deps = DraftPokeDeps {
                comments: Some(Arc::new(RecordingComments::default()) as Arc<dyn PrCommentSink>),
                room: None,
            };
            perform_nudge(&DraftNudge::Escalate(escalation()), &deps, Utc::now()).await;
        })
        .await;
        assert!(
            events
                .iter()
                .any(|e| e.message.contains("handing it to a human")),
            "the comment accepted the escalation, so a human was told: {events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|e| e.message.contains("reached no surface")),
            "a surface accepted; the no-surface line must not fire: {events:?}"
        );
    }

    /// The complement of the test above: ONE surface accepting is enough. A refusing comment sink
    /// must not suppress the handoff line when the room took the escalation, or an operator would be
    /// told nobody was informed of a pull request a human does in fact own.
    #[tokio::test]
    async fn an_escalation_that_reaches_only_the_room_still_hands_it_to_a_human() {
        let events = captured(|| async {
            let room = Arc::new(RecordingRoom::default());
            let deps = DraftPokeDeps {
                comments: Some(Arc::new(FailingComments) as Arc<dyn PrCommentSink>),
                room: Some(Arc::clone(&room) as Arc<dyn RoomLog>),
            };
            perform_nudge(&DraftNudge::Escalate(escalation()), &deps, Utc::now()).await;
        })
        .await;
        assert!(
            events
                .iter()
                .any(|e| e.message.contains("handing it to a human")),
            "the room accepted the escalation, so a human was told: {events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|e| e.message.contains("reached no surface")),
            "a surface accepted; the no-surface line must not fire: {events:?}"
        );
    }
}
