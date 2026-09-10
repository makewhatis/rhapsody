//! quorum — Rhapsody Teams' **notified review**: a teammate's handoff fans review tickets out to
//! the least-loaded other teammates, once, opt-in (STUDIO-659, slice T7; design record
//! `~/.rhapsody/docs/STUDIO-572-rhapsody-teams.md`, §0.6 and the trigger/cap decision recorded as
//! §0.12 on 2026-08-30). **No Go v0.4.0 counterpart** — Teams is a Rhapsody addition end to end.
//!
//! §0.6 asks for one thing: when a teammate hands off a PR, **at least two other teammates review
//! it, independently**. §0.12 decided how, on one principle — *use only machinery that already
//! exists and already dispatches* — and every structural choice here follows from it:
//!
//! * **Reviewers are ordinary tickets.** One Todo issue per reviewer, labelled
//!   `rhapsody:@<reviewer>`, is the whole fan-out. Separate tickets sidestep the
//!   one-live-run-per-issue invariant, need **no new dispatch machinery at all**, and give §0.6
//!   rule 2's context isolation for free: separate worktrees, separate prompts, no shared findings.
//! * **The room notifies; Linear dispatches** (§0.6 rule 1, §0.2's "the room has no dispatch power,
//!   ever"). The manager posts *"requested review of <PR> from bob, carol"*; the TICKETS are what
//!   wake anybody. A room post that failed costs a paragraph of history and nothing else.
//! * **The findings go back on the PR, not into an aggregator.** Each reviewer posts summon
//!   comments on the pull request, which re-engages the author through the machinery INF-448 and
//!   STUDIO-649 already built. There is no synthesis step and deliberately so — the author's
//!   re-engagement IS the loop.
//!
//! # The shape is triage's, deliberately
//!
//! This module is [`crate::triage`]'s sibling in every structural respect, because the constraint
//! is the same one: **nothing here may stall the control task.** [`run_quorum_task`] takes no
//! `Orchestrator`, sends no control event, and holds no lock the control task takes. It is fed
//! [`QuorumRequest`]s — plain owned data, decided on the loop — over a channel, backs off
//! exponentially on tracker failure ([`MAX_QUORUM_BACKOFF_MS`], triage's ceiling), and posts to the
//! room through the same best-effort seam. A Linear that never answers parks *this* task and
//! nothing else.
//!
//! The one difference from triage is the clock: triage has a cadence, the quorum has an event. It
//! wakes on a handoff rather than on a timer, which is why it is a channel consumer rather than a
//! schedule.
//!
//! # The PR is resolved from GitHub when Linear does not know it (STUDIO-674)
//!
//! §0.12's trigger is "a teammate's handoff **with a linked PR**", and the only thing that ever
//! knew about that PR was a Linear GitHub **attachment** on the poller's candidate snapshot. On an
//! installation whose Linear↔GitHub integration never materializes, every issue holds
//! `attachments: []` — including long-shipped ones with merged PRs — so that gate refused every
//! ticket and the quorum was structurally dead rather than merely quiet.
//!
//! So the attachment is a fallback, not the source of truth. The request is built carrying the
//! run's repo and the `symphony/<identifier>` branch its worktree pushed, and **the off-loop task**
//! asks GitHub for the open PR on that branch ([`crate::ghsummons::OpenPrSource`]) — falling back
//! to the attachment's URL when GitHub answers nothing, and dropping the request, having written
//! nothing, when there is no attachment either.
//!
//! **The lookup is made even when the attachment already carried a URL** (STUDIO-822). It used to
//! be skipped there, and skipping it cost nothing while a URL was all the fan-out needed; the
//! repeat guard below needs the pull request's HEAD, and no attachment carries one.
//!
//! The split is the point: [`Orchestrator::plan_quorum`] still touches no network, because building
//! the request needs only config it already holds (the branch name is a frozen contract and the
//! repo is the one the run was dispatched against). A handoff therefore costs one `gh` call, and a
//! ticket that never gets a PR costs that and no writes — its parent recording no head, so a PR
//! opened afterwards is still reviewable on the next handoff.
//!
//! # Off costs exactly nothing
//!
//! `quorum.enabled` defaults **false** (§0.12's cost control: §0.6 calls notified review the most
//! expensive item in the whole revision, at ≥2 extra agent runs per handoff). With it off — or with
//! Teams off — [`Orchestrator::quorum_enabled`] is false, so: no task is spawned, the per-tick
//! candidate sweep in [`Orchestrator::record_quorum_state`] returns immediately, no
//! [`QuorumRequest`] is ever built, and no tracker method is called. That is the acceptance
//! criterion, and it is enforced at four independent points rather than one.
//!
//! # "Already reviewed" is a property of a HEAD, not of a ticket (STUDIO-822)
//!
//! It used to be a property of a ticket, guarded twice — the `rhapsody:quorum-requested` label read
//! on the control task, and a process-lifetime `HashSet` of settled parents in the task. Both said
//! the same wrong thing: *this ticket has been reviewed once, so never again.* Review rounds 2, 3
//! and 4 of a ticket therefore got no reviewer at all, and those are exactly the rounds that exist
//! **because** a reviewer found something. Nothing an operator could edit in Linear reached the
//! in-memory half, so there was no workaround short of restarting the daemon.
//!
//! There is now ONE condition, and it is the shape [`crate::reviewwatch::review_round_due`] has
//! always used on the ticketless path: **fan out unless a quorum was already requested at THIS
//! pull-request head.** The head is recorded when the fan-out is requested (reviewwatch's
//! `requested_sha`, not its `last_reviewed_sha`), which is what makes a repeat handoff while a
//! review of the same head is still in flight fan out nothing — the record is already there.
//!
//! The two paths cannot share the *function*, because they do not share a subject:
//! [`review_round_due`](crate::reviewwatch::review_round_due) matches over a store-backed
//! [`ReviewWatchRow`](rhapsody_store::ReviewWatchRow)'s `status`, and the ticket path has no watch
//! set, no row and no completion signal to write a `last_reviewed_sha` from — a review ticket is an
//! ordinary Rhapsody run whose exit this module never sees. What is shared is the rule.
//!
//! **The record is in memory, and a restart forgets it.** The marker label is still written, but as
//! the operator-visible record it always was and as [`crate::teamsears`]'s own once-only guard —
//! never again as this module's refusal, because a label cannot carry a commit and a per-head label
//! would mint one Linear label per head forever. The cost of forgetting is at most one duplicate
//! review of an unchanged head after a restart; the cost of the durable version was every round
//! after the first, on every ticket, permanently. [`crate::reviewwatch::REVIEW_ROUNDS_PER_PR_CAP`]
//! makes the same trade for the same reason.
//!
//! # Every write is best-effort, and a partial fan-out is REPORTED, not retried
//!
//! The tradeoff, stated because it is a decision and not an accident: when 1 of 2 review tickets is
//! created, the quorum **records the head anyway** and names the shortfall in the room post. The
//! alternative — leaving the head unrecorded so a later handoff retries — would re-create the
//! ticket that already succeeded, and duplicate review tickets are worse than a stated gap: a human
//! reading the room can create the missing one in seconds, while a duplicate wakes a real agent
//! against a real PR for no reason. A fan-out that creates **nothing** records no head, so a later
//! handoff of the same ticket may still try.
//!
//! # A fan-out that FAILS is retried, and giving up is surfaced (STUDIO-822)
//!
//! A total failure — no tracker, no resolvable viewer, or every create refused — is retried up to
//! [`QUORUM_FANOUT_ATTEMPTS`] times behind the same exponential back-off, because the failure that
//! lost a real review round was a single connection error to Linear that nothing ever re-attempted.
//! Only a TOTAL failure retries: a partial fan-out already created tickets, and re-running it would
//! duplicate them.
//!
//! When the attempts run out the round is gone for good, so it is recorded on
//! [`crate::warnings::WarningsState`] and surfaced on `GET /api/v1/projects` beside the other
//! per-project advisories — a LOCAL surface, deliberately, because the fan-out fails when the
//! tracker is unreachable and a comment on the ticket is the one write guaranteed to fail with it.
//!
//! # What this slice deliberately does not do
//!
//! * **No human-label trigger.** Requesting a quorum on any ticket by hand is the natural
//!   follow-up; §0.12 explicitly calls it an override path, not the v1 default.
//! * **No aggregation of the reviews.** See above.
//! * **No reviewer-run catch-up suppression.** §0.12 names the leak and accepts it: a reviewer's
//!   turn-1 room catch-up could in principle show them a post the other reviewer made mid-review.
//!   In practice both dispatch before either posts, findings belong on the PR rather than the room,
//!   and suppressing catch-up for review runs is machinery §0.6's value does not justify.
//! * **Only the daemon-mediated handoff triggers it.** An agent that moves its own ticket to the
//!   review state through the Linear-MCP fallback path is not observed here. §0.12 chose "the
//!   moment the daemon already observes, because it executes the handoff", and
//!   `symphony_handoff` is that moment.

use std::borrow::Cow;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use rhapsody_config::room::{Message, RoomLog};
use rhapsody_config::teams::Teams;
use rhapsody_core::Issue;
use rhapsody_tracker::{NewIssue, Tracker};
use rhapsody_workspace::sanitize_key;

use crate::backoff::failure_backoff_ms;
use crate::control_loop::CancelWait;
use crate::ghsummons::OpenPrSource;
use crate::orchestrator::Orchestrator;
use crate::reads::ProjectTracker;
use crate::teams::IDENTITY_LABEL_PREFIX;
use crate::triage::MANAGER_IDENTITY;

/// The parent ticket gains this label when its quorum fires: the operator-visible record that this
/// daemon has asked for a review of it at least once.
///
/// **It is no longer this module's refusal** (STUDIO-822). It was §0.12's "once per ticket"
/// idempotency record, and once-per-ticket is the wrong granularity — see the module docs. A label
/// cannot carry a commit, so it cannot express the head-aware rule that replaced it; making it
/// per-head would mint a new Linear label per pushed head, forever. What still reads it is
/// [`crate::teamsears`]'s manual "@manager review <ticket>" lever, whose own rule *is* once per
/// ticket ("I asked once and I do not ask twice"), and which this ticket does not change.
///
/// A LABEL rather than a database row for §0.11.1's reason: Linear is the ledger, labels are
/// additive (adding one can never remove a ticket from candidacy). It sits in the same `rhapsody:`
/// namespace as capabilities and identity labels; `quorum-` cannot collide with an identity (those
/// carry the `@`) and an unknown `rhapsody:*` label is a documented silent no-op in the
/// capabilities registry.
pub const QUORUM_REQUESTED_LABEL: &str = "rhapsody:quorum-requested";

/// The marker every review ticket the quorum MINTS carries, beside the `rhapsody:@<reviewer>` label
/// that routes it (STUDIO-780).
///
/// It records a fact nothing else in the system records: that this ticket's whole job is to review
/// somebody else's work. The console needs it to say "reviewing" where it would otherwise say
/// "in review" — two different claims that read identically today — and the only alternative on
/// offer was sniffing the `Review: ` title prefix, which is a CONVENTION this module happens to
/// follow ([`review_title`]) and not a fact: a hand-written ticket whose title starts with that
/// word would be mislabelled, silently and forever.
///
/// A LABEL for exactly [`QUORUM_REQUESTED_LABEL`]'s reasons — Linear is the ledger, labels are
/// additive, and the record outlives the daemon that wrote it. It sits in the same `rhapsody:`
/// namespace; `review-ticket` cannot collide with an identity (those carry the `@`) and an unknown
/// `rhapsody:*` label is a documented silent no-op in the capabilities registry, so a daemon that
/// predates this simply ignores it.
///
/// **Forward-only, deliberately.** It is written at creation, so review tickets minted before this
/// existed carry nothing and the console reads them exactly as it did before — the same degradation
/// every other best-effort decoration on that listing takes. Backfilling is not possible without the
/// very title heuristic this exists to avoid; an operator who wants an old review ticket relabelled
/// can add the label by hand.
pub const REVIEW_TICKET_LABEL: &str = "rhapsody:review-ticket";

/// The ceiling on the failure back-off, [`crate::triage::MAX_TRIAGE_BACKOFF_MS`]'s value and its
/// reason: a tracker outage settles at one attempt per 15 minutes rather than a hot retry loop.
pub const MAX_QUORUM_BACKOFF_MS: i64 = 15 * 60 * 1000;

/// How many times ONE fan-out is attempted before the round is given up on and surfaced
/// (STUDIO-822).
///
/// Three, because the failure this exists for was a single `error sending request` to Linear — a
/// connection blip that the next attempt would have ridden out — and because each attempt after the
/// first is separated by the exponential back-off, so three attempts already spread across minutes
/// rather than seconds. More would not rescue a real outage; it would only park this task, and with
/// it every queued handoff behind it, for longer.
///
/// Only a TOTAL failure ([`FanOutcome::TrackerFailure`]) is retried. A partial fan-out created
/// review tickets, and a retry would duplicate them.
pub const QUORUM_FANOUT_ATTEMPTS: u32 = 3;

/// Bounds the open-PR lookup (STUDIO-674), [`crate::ghenrich`]'s `GH_SUMMONS_TIMEOUT` and its
/// reason: a network-stalled lookup must not park the fan-out indefinitely.
///
/// Honest reach, because a bound that reads stronger than it is, is worse than none: against the
/// PRODUCTION source it is currently INERT. [`crate::ghsummons::GH`] shells out through a
/// synchronous `std::process::Command`, so its future has no await point and runs to completion in
/// its first poll — `tokio::time::timeout` never gets to cancel it. The bound is real for any
/// source that actually yields, which today means the tests. What makes it real everywhere is the
/// non-blocking runner (`spawn_blocking` / `tokio::process`) already noted as a follow-up on
/// `ghsummons::default_run`, and until then the containment is structural rather than temporal: a
/// hung `gh` parks THIS task, which owns all of the quorum's network I/O and no lock the control
/// task takes, and the daemon keeps ticking.
const PR_LOOKUP_TIMEOUT: Duration = Duration::from_secs(15);

/// What the per-tick candidate sweep learned about ONE ticket, so the handoff moment can decide
/// without asking the tracker anything (§0.12: the check reads "the candidate's already-fetched
/// labels, not a fresh read").
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct QuorumFacts {
    /// The ticket's first UNMERGED linked PR as a browser URL, or empty when it has none.
    ///
    /// Unmerged, because a merged PR needs no review. Derived from the same Linear GitHub-attachment
    /// data `linked_pr` comes from, which is why it can be read off a candidate rather than fetched.
    pub pr_url: String,
}

/// One fan-out, decided on the control task and handed to the off-loop task as plain owned data.
///
/// Everything expensive is already resolved by the time this exists: the reviewers are chosen, the
/// PR is known, the state name is resolved. The task's whole job is the writes. That split is what
/// makes "no network on the dispatch path" checkable — this struct contains no handle, no `Arc`,
/// and nothing borrowed from the orchestrator.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QuorumRequest {
    /// The handed-off ticket: the tracker id (for the marker-label write), its team (review tickets
    /// are created beside the work they review) and its human identifier + title (for the fan-out's
    /// titles, descriptions and room post).
    pub parent_issue_id: String,
    pub parent_team_id: String,
    pub parent_identifier: String,
    pub parent_title: String,
    /// The owning project's slug, so the off-loop task can create the review ticket through THAT
    /// project's tracker rather than the account-level one (STUDIO-677).
    ///
    /// **May be empty**, and legitimately so: the legacy single-project config form stamps no slug
    /// on a run, and there the account tracker is itself bound to `tracker.project_slug`. An empty
    /// slug therefore means "use the account tracker", not "the project is unknown" — see
    /// [`tracker_for`].
    pub parent_project_slug: String,
    /// The project GROUP the parent belongs to, so a fan-out this daemon gives up on can be
    /// surfaced against that project on `GET /api/v1/projects` (STUDIO-822). Resolved on the
    /// control task, where `eff` lives; empty when no project could be resolved, in which case the
    /// advisory is recorded under the empty group and only the `WARN` line remains.
    pub parent_project_group: String,
    /// The pull request under review, as read off the ticket's Linear GitHub attachment.
    ///
    /// **May be empty** (STUDIO-674). An installation whose Linear↔GitHub integration never
    /// materializes holds `attachments: []` on every issue, so the poller's candidate snapshot has
    /// no URL to carry and this gate could never pass for any ticket. When it is empty the
    /// off-loop task resolves the PR from [`pr_owner`](Self::pr_owner)/[`pr_repo`](Self::pr_repo)
    /// and [`pr_head_branch`](Self::pr_head_branch), and drops the request if GitHub has no open
    /// PR either. When it is set, the attachment wins and no lookup is made.
    pub pr_url: String,
    /// The run's repository and the branch its worktree pushed, so the off-loop task can ask GitHub
    /// for the open PR when [`pr_url`](Self::pr_url) is empty (STUDIO-674). Derived on the control
    /// task from config alone — the repo URL the run was dispatched against and
    /// `symphony/<identifier>`, the frozen branch-naming contract — so the dispatch path makes no
    /// network call. Any of the three may be empty (a project with no GitHub remote); the task then
    /// has nothing to ask and drops the request.
    pub pr_owner: String,
    pub pr_repo: String,
    pub pr_head_branch: String,
    /// The roster identity the handed-off run wore. Excluded from its own review (§0.6: "at least
    /// two OTHER teammates").
    pub author: String,
    /// The chosen reviewers, least-loaded first. **May be empty** — a roster of one produces a
    /// request whose whole effect is the loud room post §0.12 asks for, which is information a team
    /// needs and an error nobody can act on.
    pub reviewers: Vec<String>,
    /// The workflow state review tickets open in — the run's own project's first configured active
    /// state. See [`Orchestrator::quorum_create_state`] for why that and not the literal "Todo".
    pub state_name: String,
    /// The configured summon token (e.g. `@symphony`), so the reviewer's instructions name the
    /// token THIS installation actually re-engages on rather than a hard-coded guess.
    pub summon_token: String,
}

/// The live tracker one fan-out runs against, read fresh per request so a hot-reloaded tracker is
/// honoured — [`crate::triage::TriageTarget`]'s shape and its reason.
pub struct QuorumTarget {
    /// The account-level/top-level client. In the `projects:` config form its `project_slug` is
    /// EMPTY by design, which is why it can no longer be the thing the fan-out creates through
    /// (STUDIO-677); it remains the legacy single-project form's slug-bound client, and so the
    /// fallback when no project tracker matches.
    pub tracker: Arc<dyn Tracker>,
    /// Every enabled project's slug-bound client, as the last reload published them (STUDIO-677).
    /// The fan-out creates through the one whose slug is the parent's.
    pub project_trackers: Vec<ProjectTracker>,
}

/// Everything [`run_quorum_task`] runs against. The absence of an `Orchestrator`, a control channel
/// and a store here is the off-loop guarantee, in the type.
pub struct QuorumDeps<TF> {
    /// The boot-loaded `teams.yaml`, captured once at the composition root (Teams config is not
    /// hot-reloaded in this slice, matching triage).
    pub teams: Arc<Teams>,
    /// Yields the live tracker, or `None` when no config has loaded yet.
    pub target: TF,
    /// The room the manager's post goes to; `None` when there is no room to write to (Teams without
    /// an on-disk runtime home), in which case the fan-out still happens and only the history is
    /// lost. A `dyn RoomLog` for triage's reason: nothing here runs on the control task, and the
    /// seam is what lets a test substitute a failing room to prove the fan-out survives one.
    pub room: Option<Arc<dyn RoomLog>>,
    /// Resolves the open PR by head branch when the ticket carries no GitHub attachment
    /// (STUDIO-674). `None` disables the fallback entirely, which is the pre-STUDIO-674 behaviour:
    /// a request with no `pr_url` is then simply dropped. It lives HERE, on the off-loop task's
    /// deps, and deliberately nowhere the control task can reach — that is what keeps the dispatch
    /// path network-free.
    pub pr_source: Option<Arc<dyn OpenPrSource>>,
    /// The back-off ceiling; [`MAX_QUORUM_BACKOFF_MS`] in production, milliseconds in tests.
    pub max_backoff_ms: i64,
    /// Where a fan-out this task gives up on is recorded so an operator can see it (STUDIO-822).
    /// `None` writes nothing and leaves the `WARN` line as the only trace, which is the pre-
    /// STUDIO-822 behaviour and what a test that does not care about the surface gets.
    pub warnings: Option<Arc<crate::warnings::WarningsState>>,
}

/// What one fan-out did — the input to the back-off decision, to the retry decision and to the
/// per-head bookkeeping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FanOutcome {
    /// `created` review tickets were made out of `wanted`. `created > 0`; `created < wanted` is the
    /// partial case, which still records the head and reports the shortfall.
    Fanned { created: usize, wanted: usize },
    /// The roster had nobody to ask. Nothing was written; a loud room post was attempted.
    NoReviewers,
    /// The tracker refused everything (no tracker at all, no viewer to assign to, or every create
    /// failed). Retry, then back off, and record NO head so a later handoff may still try.
    TrackerFailure,
    /// Neither the Linear attachment nor GitHub itself yielded an open PR for the ticket's branch,
    /// so there is nothing to review (STUDIO-674). Nothing was written; no head is recorded,
    /// because a PR opened after this handoff should still be reviewable on the next one. Not a
    /// failure either — a ticket without a PR is a normal state, not an outage.
    NoPullRequest,
    /// A quorum was already requested for this parent at THIS pull-request head, so this handoff is
    /// the repeat one (STUDIO-822). Nothing was written and nothing is wrong: either a review of
    /// this head is in flight, or one already finished and the author has pushed nothing since.
    AlreadyRequestedAtHead,
}

impl FanOutcome {
    /// Whether this outcome should extend the back-off — and, equivalently, whether it is worth
    /// re-attempting. Only a total tracker failure is either.
    fn is_failure(self) -> bool {
        matches!(self, FanOutcome::TrackerFailure)
    }

    /// Whether this outcome may clear a back-off earned by earlier failures. Only the outcomes that
    /// actually REACHED the tracker may: [`FanOutcome::NoPullRequest`] and
    /// [`FanOutcome::AlreadyRequestedAtHead`] both return from [`fan_out`] above the `deps.target`
    /// lookup, having neither reached the tracker nor learned anything about it, so reading either
    /// as a success would let one such handoff during a Linear outage erase the back-off that
    /// outage earned. Neither EXTENDS the back-off either — a ticket without a PR, and a repeat
    /// handoff at a head already under review, are both normal states rather than outages — so they
    /// simply leave the counter where they found it.
    fn clears_the_backoff(self) -> bool {
        !matches!(
            self,
            FanOutcome::NoPullRequest | FanOutcome::AlreadyRequestedAtHead
        )
    }

    /// Whether this parent's head should be recorded as requested. Neither a total failure nor a
    /// missing pull request records anything: both wrote nothing, and a later handoff is the only
    /// thing that should ever ask again. [`FanOutcome::AlreadyRequestedAtHead`] does not either —
    /// the head is already recorded, which is how it got here.
    fn records_the_head(self) -> bool {
        matches!(self, FanOutcome::Fanned { .. } | FanOutcome::NoReviewers)
    }
}

/// One fan-out attempt's full answer: what happened, at which head, and the loud room post a
/// failure wants made once its retries are exhausted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FanResult {
    pub outcome: FanOutcome,
    /// The pull-request head this attempt ran against — what gets recorded when
    /// [`FanOutcome::records_the_head`] says so. Empty when no pull request was resolved, and
    /// empty-but-recorded when GitHub answered a pull request without a `headRefOid`: an unknown
    /// head compares equal to the next unknown head, which degrades the guard to the per-ticket one
    /// it replaced rather than fanning out on every handoff forever.
    pub head: String,
    /// Why a [`FanOutcome::TrackerFailure`] failed, as one operator-facing sentence fragment.
    /// Carried out rather than posted inline so one outage costs ONE room post at exhaustion
    /// instead of one per attempt (STUDIO-822), and so the same sentence backs both the room post
    /// and the project advisory instead of two texts drifting apart.
    pub failure_why: String,
}

/// Consumes [`QuorumRequest`]s until `ctx` is cancelled or the sender is dropped (§0.12).
///
/// One request at a time, serially — there is no `spawn` in this module, so at most one fan-out is
/// ever in flight and a slow Linear cannot multiply into a burst of concurrent writes. A failed
/// fan-out is re-attempted up to [`QUORUM_FANOUT_ATTEMPTS`] times (STUDIO-822) and every attempt,
/// including the first of the NEXT request, waits out the exponential back-off the failures earned
/// — which is what keeps a retry from becoming a hot loop against a down API. Giving up is not
/// silent: the round is recorded on [`crate::warnings::WarningsState`] and posted to the room.
pub async fn run_quorum_task<TF>(
    mut ctx: CancelWait,
    deps: QuorumDeps<TF>,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<QuorumRequest>,
) where
    TF: Fn() -> Option<QuorumTarget>,
{
    tracing::info!(
        roster = deps.teams.roster.len(),
        reviewers = deps.teams.quorum.effective_reviewers(),
        "teams review quorum task started (off-loop; a handoff is never blocked on it)"
    );
    let mut failures: i64 = 0;
    // The head each parent's quorum was last REQUESTED at, in this process (STUDIO-822) — the one
    // condition that replaced the marker label's per-ticket refusal and the per-ticket `settled`
    // set that used to sit here. `requested_sha` in [`crate::reviewwatch`]'s vocabulary: written
    // when the fan-out is asked for rather than when a review finishes, which is what makes a
    // repeat handoff while a review of the same head is in flight fan out nothing.
    //
    // It grows by one entry per PARENT (not per head — a new head replaces the old), bounded by how
    // many tickets a daemon hands off in its lifetime; a few thousand short strings.
    let mut requested: HashMap<String, String> = HashMap::new();
    loop {
        let req = tokio::select! {
            _ = ctx.cancelled() => return,
            r = rx.recv() => match r {
                Some(r) => r,
                // Every sender dropped: the control handle is gone, so no further handoff can
                // arrive. Nothing left to do.
                None => return,
            },
        };
        let prior = requested.get(&req.parent_issue_id).cloned();
        // One request, up to `QUORUM_FANOUT_ATTEMPTS` attempts. The back-off is waited out BEFORE
        // every attempt, so a retry of this request and the first attempt of the next one are paced
        // identically — there is no faster path back to a tracker that just refused.
        let mut attempt: u32 = 0;
        let res = loop {
            attempt += 1;
            if failures > 0 {
                let delay = Duration::from_millis(
                    failure_backoff_ms(failures, deps.max_backoff_ms).max(0) as u64,
                );
                tokio::select! {
                    _ = ctx.cancelled() => return,
                    _ = tokio::time::sleep(delay) => {}
                }
            }
            if ctx.is_cancelled() {
                return;
            }
            let res = fan_out(&deps, &req, prior.as_deref()).await;
            if !res.outcome.is_failure() {
                break res;
            }
            failures += 1;
            if attempt >= QUORUM_FANOUT_ATTEMPTS {
                break res;
            }
            tracing::warn!(
                issue = %req.parent_identifier,
                attempt,
                attempts = QUORUM_FANOUT_ATTEMPTS,
                consecutive_failures = failures,
                "teams quorum fan-out failed; retrying after a back-off (the handoff itself already succeeded)"
            );
        };
        if res.outcome.records_the_head() {
            requested.insert(req.parent_issue_id.clone(), res.head.clone());
        }
        if res.outcome.is_failure() {
            give_up(&deps, &req, &res, attempt);
        } else if res.outcome.clears_the_backoff() {
            failures = 0;
        }
    }
}

/// Reports a fan-out this task attempted `attempts` times and is abandoning (STUDIO-822).
///
/// Three surfaces, because the failure that motivated this was visible on none of them: the `WARN`
/// line (which is what it already had), the room post the fan-out prepared, and — the one an
/// operator actually looks at — the project advisory on `GET /api/v1/projects`, which is LOCAL and
/// therefore still works when the reason for the failure is that the tracker does not answer.
fn give_up<TF>(deps: &QuorumDeps<TF>, req: &QuorumRequest, res: &FanResult, attempts: u32)
where
    TF: Fn() -> Option<QuorumTarget>,
{
    tracing::warn!(
        issue = %req.parent_identifier,
        project = %req.parent_project_group,
        attempts,
        "teams quorum gave up on the fan-out; this round has no reviewer and nothing will ask again"
    );
    if let Some(w) = deps.warnings.as_ref() {
        w.record_lost_review(
            &req.parent_project_group,
            &req.parent_identifier,
            &res.failure_why,
        );
    }
    post(
        deps,
        Message::room(
            MANAGER_IDENTITY,
            Utc::now(),
            format!(
                "REVIEW QUORUM ABANDONED for {}: {} after {attempts} attempts. No review of {} was \
                 requested and nothing will ask again — {} is unreviewed however the merge gate \
                 reads, so request the review by hand.",
                req.parent_identifier, res.failure_why, req.pr_url, req.parent_identifier,
            ),
        )
        .with_refs([req.parent_identifier.clone()]),
    );
}

/// One fan-out: resolve the pull request, refuse a head already asked about, create a review ticket
/// per reviewer, mark the parent, tell the room.
///
/// `requested_head` is the head this parent's quorum was last requested at, or `None` when it has
/// never been (STUDIO-822). It is the WHOLE repeat-guard — see the module docs — and it is compared
/// here rather than on the control task for the reason the pull request itself is: the head comes
/// from GitHub, and the control task makes no network call.
///
/// A create that fails does NOT abort the rest — that is what produces the partial fan-out the
/// module docs describe, and it is bounded by `reviewers` (2 by default), so a down tracker costs
/// two failed calls rather than a retry storm.
pub(crate) async fn fan_out<TF>(
    deps: &QuorumDeps<TF>,
    req: &QuorumRequest,
    requested_head: Option<&str>,
) -> FanResult
where
    TF: Fn() -> Option<QuorumTarget>,
{
    // The PR, first, because every write below names it AND because its head is what the repeat
    // guard compares (STUDIO-674, STUDIO-822). Only here — the control task that built this request
    // made no network call.
    let Some(pr) = resolve_open_pr(deps, req).await else {
        return FanResult {
            outcome: FanOutcome::NoPullRequest,
            head: String::new(),
            failure_why: String::new(),
        };
    };
    let head = pr.head_sha;
    // The one condition that replaced the marker label's refusal and the per-ticket `settled` set:
    // a quorum was already asked for at exactly this head, so either a review of it is in flight or
    // one finished and nothing has been pushed since. Both mean the same thing — there is no new
    // work to review — which is precisely what a per-TICKET guard could not tell apart from "the
    // author pushed the fixes a reviewer asked for".
    if requested_head == Some(head.as_str()) {
        tracing::debug!(
            issue = %req.parent_identifier,
            head = %head,
            "teams quorum already requested a review at this head; the repeat handoff fans out nothing"
        );
        return FanResult {
            outcome: FanOutcome::AlreadyRequestedAtHead,
            head,
            failure_why: String::new(),
        };
    }
    let req: Cow<'_, QuorumRequest> = if req.pr_url == pr.url {
        Cow::Borrowed(req)
    } else {
        let mut resolved = req.clone();
        resolved.pr_url = pr.url;
        Cow::Owned(resolved)
    };
    let req = req.as_ref();

    let Some(target) = (deps.target)() else {
        tracing::warn!(
            issue = %req.parent_identifier,
            "teams quorum has no tracker yet; the fan-out is skipped"
        );
        return FanResult {
            outcome: FanOutcome::TrackerFailure,
            head,
            failure_why: "no tracker has loaded yet, so no review ticket could be created"
                .to_string(),
        };
    };
    let tracker = tracker_for(&target, req);

    if req.reviewers.is_empty() {
        // §0.12: "zero ⇒ skip with a loud room post", never an error. A team of one is a valid
        // configuration; it just cannot hold a quorum, and the operator should be able to see that
        // said out loud rather than infer it from tickets that never appeared.
        tracing::warn!(
            issue = %req.parent_identifier,
            author = %req.author,
            "teams quorum has nobody to ask: the roster holds no teammate other than the author"
        );
        post(
            deps,
            Message::room(
                MANAGER_IDENTITY,
                Utc::now(),
                format!(
                    "NO REVIEW QUORUM for {}: {} handed off {} but the roster holds no other \
                     teammate to review it. Nothing was requested. Add a teammate to \
                     `teams.yaml`, or set `quorum.enabled: false` if a one-person team is what \
                     you meant.",
                    req.parent_identifier, req.author, req.pr_url
                ),
            )
            .with_refs([req.parent_identifier.clone()]),
        );
        return FanResult {
            outcome: FanOutcome::NoReviewers,
            head,
            failure_why: String::new(),
        };
    }

    // §0.12's claim rule: the review ticket must be ASSIGNED, because the default candidate query
    // is keyed on `assignee == viewer` — an unassigned ticket is simply never picked up, so a
    // fan-out that could not resolve the viewer would create work nobody ever does. Cached by the
    // Linear client for its lifetime, so this is one call per daemon, not one per fan-out.
    let assignee = match tracker.resolve_viewer().await {
        Ok(v) if !v.id.is_empty() => v.id,
        other => {
            let why = match other {
                Ok(_) => "the tracker returned a viewer with no id".to_string(),
                Err(e) => e.to_string(),
            };
            tracing::warn!(
                issue = %req.parent_identifier,
                err = %why,
                "teams quorum could not resolve the viewer to assign review tickets to; nothing created"
            );
            return FanResult {
                outcome: FanOutcome::TrackerFailure,
                head,
                // Not posted here: the room hears about it once, at exhaustion (STUDIO-822).
                failure_why: format!(
                    "the tracker viewer to assign review tickets to could not be resolved \
                     ({why}), and an unassigned ticket is never picked up, so creating one \
                     would have been worse than creating none"
                ),
            };
        }
    };

    let mut created: Vec<(String, String)> = Vec::with_capacity(req.reviewers.len());
    let mut failed: Vec<String> = Vec::new();
    // The last create error, kept so a total failure can SAY what went wrong rather than only that
    // something did — the one line an operator reading the abandoned-fan-out advisory needs.
    let mut create_err = String::from("no error was recorded");
    for reviewer in &req.reviewers {
        let spec = NewIssue {
            team_id: req.parent_team_id.clone(),
            title: review_title(&req.parent_identifier, &req.parent_title),
            description: review_description(req, reviewer, &head),
            state_name: req.state_name.clone(),
            assignee_id: assignee.clone(),
            // The identity label IS the assignment (§0.11.1); the marker records what KIND of
            // ticket this is (STUDIO-780). Both go through `create_issue`'s all-or-nothing
            // resolution, so a workspace that will not mint the marker refuses the whole ticket
            // rather than producing one the console cannot classify.
            labels: vec![
                format!("{IDENTITY_LABEL_PREFIX}{reviewer}"),
                REVIEW_TICKET_LABEL.to_string(),
            ],
        };
        match tracker.create_issue(&spec).await {
            Ok(identifier) => {
                tracing::info!(
                    parent = %req.parent_identifier,
                    %reviewer,
                    review_ticket = %identifier,
                    "teams quorum created a review ticket"
                );
                created.push((reviewer.clone(), identifier));
            }
            Err(e) => {
                tracing::warn!(
                    parent = %req.parent_identifier,
                    %reviewer,
                    err = %e,
                    "teams quorum could not create a review ticket"
                );
                create_err = e.to_string();
                failed.push(reviewer.clone());
            }
        }
    }

    if created.is_empty() {
        return FanResult {
            outcome: FanOutcome::TrackerFailure,
            head,
            failure_why: format!(
                "no review ticket could be created (asked: {}); the last create failed with {}",
                req.reviewers.join(", "),
                create_err,
            ),
        };
    }

    // The marker is written even for a partial fan-out (see the module docs), and re-written on
    // every later round — `issueAddLabel` is additive, so a label already present is a no-op. It no
    // longer guards anything here (STUDIO-822); what it costs when it fails is the operator-visible
    // record and [`crate::teamsears`]'s manual lever, which is worth saying out loud in the post.
    let mut marker_err = String::new();
    if let Err(e) = tracker
        .add_issue_label(
            &req.parent_issue_id,
            &req.parent_team_id,
            QUORUM_REQUESTED_LABEL,
        )
        .await
    {
        marker_err = e.to_string();
        tracing::warn!(
            issue = %req.parent_identifier,
            err = %marker_err,
            "teams quorum could not mark the parent; a later handoff could fan out a second time"
        );
    }

    post(
        deps,
        Message::room(
            MANAGER_IDENTITY,
            Utc::now(),
            fan_out_post(req, &created, &failed, &marker_err),
        )
        .with_refs(
            std::iter::once(req.parent_identifier.clone())
                .chain(created.iter().map(|(_, id)| id.clone())),
        ),
    );
    FanResult {
        outcome: FanOutcome::Fanned {
            created: created.len(),
            wanted: req.reviewers.len(),
        },
        head,
        failure_why: String::new(),
    }
}

/// Picks the client the fan-out writes through: the parent's OWN project tracker when the request
/// names a project the last reload published one for, and the account-level tracker otherwise
/// (STUDIO-677).
///
/// The account tracker cannot be the default. In the `projects:` config form it is bound to a
/// `tracker.project_slug` validation deliberately allows to be empty — STUDIO-671's read-side wedge,
/// here on the write side — and [`Tracker::create_issue`] REFUSES a slug-less client rather than
/// mint an issue outside the candidate query that would ever find it. So every fan-out on such an
/// installation failed with `create issue requires a configured tracker.project_slug`, which is the
/// bug this resolves. The parent's project client has the slug, and it is the project the
/// reviewer's own candidate query sweeps, so the review ticket is both creatable and findable.
///
/// The fallback is not a silent one. An EMPTY slug is the legacy single-project path, where the
/// account tracker is the slug-bound client and this is simply correct. A slug that matches nothing
/// is a project paused or removed between the dispatch and the handoff: the account tracker is then
/// the only client left to try, and if it too is slug-less the create fails into the task's retry
/// and its loud "REVIEW QUORUM ABANDONED" room post rather than dropping the request quietly.
fn tracker_for(target: &QuorumTarget, req: &QuorumRequest) -> Arc<dyn Tracker> {
    if req.parent_project_slug.is_empty() {
        tracing::debug!(
            issue = %req.parent_identifier,
            "teams quorum: the handed-off run names no project, so the review ticket is created \
             through the top-level tracker (the legacy single-project form's slug-bound client)"
        );
        return Arc::clone(&target.tracker);
    }
    match target
        .project_trackers
        .iter()
        .find(|p| p.slug == req.parent_project_slug)
    {
        Some(p) => Arc::clone(&p.tracker),
        None => {
            tracing::warn!(
                issue = %req.parent_identifier,
                project = %req.parent_project_slug,
                projects = target.project_trackers.len(),
                "teams quorum has no tracker for the handed-off ticket's project (paused or \
                 removed since dispatch?); falling back to the top-level tracker, which cannot \
                 create an issue unless it is itself bound to a project slug"
            );
            Arc::clone(&target.tracker)
        }
    }
}

/// Asks GitHub for the open PR on the request's head branch (STUDIO-674), returning `None` when
/// there is nothing to review — no source and no attachment, no repo/branch to ask about, no open
/// PR, and no attachment to fall back on.
///
/// **The lookup is no longer skipped when the Linear attachment already carried a URL**
/// (STUDIO-822). It used to be, and it cost nothing then, because a URL was all the fan-out needed.
/// It needs a HEAD now, the attachment carries none, and there is no way to learn one without
/// asking. GitHub is in any case the source of truth the attachment was only ever a cache of. What
/// this costs is one `gh pr list` per handoff on an installation whose Linear↔GitHub integration
/// works — and nothing at all on one whose does not, which is where the attachment path was already
/// dead.
///
/// The attachment remains the FALLBACK, with an empty head: when GitHub answers nothing (or cannot
/// be asked) and Linear says there is a pull request, the review still happens and only the
/// per-head granularity is lost. Refusing there would turn a lookup blip into a missing review,
/// which is the failure this whole ticket is about.
///
/// Every `None` is a DEBUG line except a lookup that FAILED, which is a warning: "GitHub says there
/// is no PR" is a normal state of a ticket, while "we could not ask GitHub" is an operator problem
/// that would otherwise look identical from the outside. Neither retries here — the task's own
/// retry covers a fan-out that reached the tracker and failed, and a lookup that answers "no pull
/// request" is not a failure to retry.
async fn resolve_open_pr<TF>(
    deps: &QuorumDeps<TF>,
    req: &QuorumRequest,
) -> Option<crate::ghsummons::OpenPr>
where
    TF: Fn() -> Option<QuorumTarget>,
{
    /// The Linear attachment, as a head-less [`OpenPr`](crate::ghsummons::OpenPr).
    fn attached(req: &QuorumRequest) -> Option<crate::ghsummons::OpenPr> {
        (!req.pr_url.is_empty()).then(|| crate::ghsummons::OpenPr {
            url: req.pr_url.clone(),
            head_sha: String::new(),
        })
    }
    let Some(src) = deps.pr_source.as_ref() else {
        if req.pr_url.is_empty() {
            tracing::debug!(
                issue = %req.parent_identifier,
                "teams quorum: the handed-off ticket has no linked PR and no PR source is \
                 configured; nothing to review"
            );
        }
        return attached(req);
    };
    // Bounded exactly as the poll path's summons fetch is, and for the same reason — but see
    // `PR_LOOKUP_TIMEOUT`: the real `gh` runner blocks rather than yields, so this cannot interrupt
    // it today. What holds either way is that the stall is confined to THIS task, which owns all of
    // the quorum's network I/O and no lock the control task takes.
    let looked_up = tokio::time::timeout(
        PR_LOOKUP_TIMEOUT,
        src.open_pr_for_branch(&req.pr_owner, &req.pr_repo, &req.pr_head_branch),
    )
    .await;
    match looked_up {
        Ok(Ok(Some(pr))) => {
            tracing::info!(
                issue = %req.parent_identifier,
                branch = %req.pr_head_branch,
                pr = %pr.url,
                head = %pr.head_sha,
                "teams quorum resolved the ticket's open PR and its head by head branch"
            );
            Some(pr)
        }
        Ok(Ok(None)) => {
            tracing::debug!(
                issue = %req.parent_identifier,
                repo = %format!("{}/{}", req.pr_owner, req.pr_repo),
                branch = %req.pr_head_branch,
                attached = !req.pr_url.is_empty(),
                "teams quorum: GitHub reports no open PR on the ticket's branch"
            );
            attached(req)
        }
        Ok(Err(e)) => {
            tracing::warn!(
                issue = %req.parent_identifier,
                branch = %req.pr_head_branch,
                err = %e,
                attached = !req.pr_url.is_empty(),
                "teams quorum could not ask GitHub for the ticket's open PR (the handoff itself \
                 already succeeded)"
            );
            attached(req)
        }
        Err(_) => {
            tracing::warn!(
                issue = %req.parent_identifier,
                branch = %req.pr_head_branch,
                timeout_ms = PR_LOOKUP_TIMEOUT.as_millis(),
                attached = !req.pr_url.is_empty(),
                "teams quorum's open-PR lookup timed out"
            );
            attached(req)
        }
    }
}

/// §0.6 rule 1's post, verbatim in substance: *"requested review of &lt;PR&gt; from bob, carol"*,
/// plus whatever went wrong. The post INFORMS; the tickets dispatch.
fn fan_out_post(
    req: &QuorumRequest,
    created: &[(String, String)],
    failed: &[String],
    marker_err: &str,
) -> String {
    let names: Vec<&str> = created.iter().map(|(n, _)| n.as_str()).collect();
    let tickets: Vec<&str> = created.iter().map(|(_, t)| t.as_str()).collect();
    let mut out = format!(
        "Requested review of {} from {} ({}), for {} handed off by {}. Findings go on the PR as \
         {} comments, which pulls {} back in; reviewers never merge.",
        req.pr_url,
        names.join(", "),
        tickets.join(", "),
        req.parent_identifier,
        req.author,
        req.summon_token,
        req.author,
    );
    if !failed.is_empty() {
        out.push_str(&format!(
            " SHORTFALL: no review ticket could be created for {} — {} of {} reviewers were asked. \
             This head is recorded as requested anyway, so {} will NOT be retried automatically; \
             create the missing ticket by hand if you want the full quorum.",
            failed.join(", "),
            created.len(),
            req.reviewers.len(),
            req.parent_identifier,
        ));
    }
    if !marker_err.is_empty() {
        out.push_str(&format!(
            " WARNING: the `{QUORUM_REQUESTED_LABEL}` marker could not be written to {} \
             ({marker_err}); it records nothing this daemon reads back, but the manager's \
             `review <ticket>` room lever will not know a review was already asked for.",
            req.parent_identifier,
        ));
    }
    out
}

/// The host-templated review-ticket title (§0.12: "host-templated title `Review: <parent-title>`").
/// The parent's identifier leads so a reviewer scanning a backlog can see what is being reviewed
/// without opening anything.
pub(crate) fn review_title(parent_identifier: &str, parent_title: &str) -> String {
    format!("Review: {parent_identifier} {parent_title}")
}

/// The host-templated review-ticket description (§0.12). It names the PR, the parent, and the job.
///
/// **Written by the host, never by an agent.** The reviewer's instructions are the one place where
/// "never merge" has to hold, and a description an agent could author is a description an agent
/// could rewrite. §0.11.5 already treats agent-authored text as untrusted; this keeps the
/// instruction on the trusted side of that line.
pub(crate) fn review_description(req: &QuorumRequest, reviewer: &str, head: &str) -> String {
    let QuorumRequest {
        pr_url,
        parent_identifier,
        parent_title,
        author,
        summon_token,
        ..
    } = req;
    // Named when it is known (STUDIO-822) so the reviewer, and anyone reading the ticket later, can
    // tell WHICH round this is: rounds 2+ exist because a reviewer found something, and two review
    // tickets for one pull request are otherwise indistinguishable. Omitted rather than rendered
    // empty when GitHub answered no `headRefOid`, because a blank commit reads as a bug.
    let at_head = if head.is_empty() {
        String::new()
    } else {
        format!("**Head:** {head}\n")
    };
    format!(
        "You are **{reviewer}**, reviewing a teammate's work.\n\
         \n\
         **Pull request:** {pr_url}\n\
         {at_head}\
         **Reviewing:** {parent_identifier} — {parent_title}\n\
         **Author:** {author}\n\
         \n\
         Review that pull request independently. Another teammate is reviewing it at the same \
         time, from their own workspace, and you are not meant to agree with them — two \
         independent passes beat one agreed pass, which is the entire reason this ticket exists. \
         Do not go looking for their findings.\n\
         \n\
         What to do:\n\
         \n\
         1. Read the pull request: `gh pr view {pr_url}` and `gh pr diff {pr_url}`.\n\
         2. Judge it on correctness first, then on whether it does what {parent_identifier} \
            actually asked for, then on hygiene. Say what you would say to a colleague.\n\
         3. Post your findings as comments **on the pull request**, each one starting with \
            `{summon_token}` so {author} is re-engaged on them. The pull request is where a review \
            belongs — not this ticket, and not the team room.\n\
         4. **Approve or request changes explicitly.** \"Looks fine\" is not a review; say which \
            one it is and why.\n\
         5. **Never merge, and never push to the author's branch.** {author} owns the work; you own \
            the opinion.\n\
         \n\
         When you have posted your findings, you are done — hand this ticket off.\n"
    )
}

/// Appends one manager post to the room, best-effort and never fatal to the fan-out — triage's
/// [`post`](crate::triage) helper and its reason (§0.11.4: the room is advisory, Linear is the
/// ledger). The tickets have already been created or already refused; this can neither undo nor
/// block them, which is why it returns nothing to check.
fn post<TF>(deps: &QuorumDeps<TF>, msg: Message) {
    let Some(room) = deps.room.as_ref() else {
        return;
    };
    if let Err(e) = room.append(&msg) {
        tracing::warn!(
            err = %e,
            "teams quorum could not post to the room; the review tickets and the Linear history \
             are unaffected"
        );
    }
}

/// Chooses who reviews: the roster **minus the author**, least-loaded first, capped at
/// `quorum.reviewers` (§0.12). Pure — the whole selection is a comparison over data already in
/// hand, which is what lets it run on the control task.
///
/// Ties break on roster order, exactly as [`crate::teams::route`]'s label-overlap fallback does, so
/// the choice is deterministic and a test can pin it. `load` is §0.11.1's load: open tickets
/// carrying `rhapsody:@x`, counted from the daemon's own in-memory candidate snapshot.
///
/// Too few candidates degrades to however many exist — never an error, and never a wait.
pub(crate) fn select_reviewers(
    teams: &Teams,
    author: &str,
    load: &HashMap<String, i64>,
) -> Vec<String> {
    let mut picked = rank_reviewers(teams, author, load);
    picked.truncate(teams.quorum.effective_reviewers());
    picked
}

/// The whole roster minus `author`, least-loaded first with roster order as the tie-break — the
/// ranking [`select_reviewers`] truncates to the quorum's count and the ticketless path truncates
/// to its own (STUDIO-721).
///
/// Split out because the two review paths ask for different numbers of reviewers from the same
/// order, and a second ranking function would be a second place for "never the author" and "ties
/// break on roster order" to drift.
pub(crate) fn rank_reviewers(
    teams: &Teams,
    author: &str,
    load: &HashMap<String, i64>,
) -> Vec<String> {
    let mut ranked: Vec<(i64, usize, &str)> = teams
        .roster
        .iter()
        .enumerate()
        .filter(|(_, i)| i.name != author)
        .map(|(idx, i)| {
            (
                load.get(&i.name).copied().unwrap_or(0),
                idx,
                i.name.as_str(),
            )
        })
        .collect();
    ranked.sort_unstable();
    ranked
        .into_iter()
        .map(|(_, _, name)| name.to_string())
        .collect()
}

/// The first UNMERGED linked pull request as a browser URL, or empty when the issue has none.
///
/// Unmerged because a merged PR needs no review. The refs come from the same Linear
/// GitHub-integration attachments `linked_pr` is derived from, so this reads off a candidate the
/// poller already fetched rather than costing a call.
pub(crate) fn open_pr_url(iss: &Issue) -> String {
    iss.linked_prs
        .iter()
        .flatten()
        .find(|p| !p.merged && !p.owner.is_empty() && !p.repo.is_empty() && p.number > 0)
        .map(|p| {
            format!(
                "https://github.com/{}/{}/pull/{}",
                p.owner, p.repo, p.number
            )
        })
        .unwrap_or_default()
}

impl Orchestrator {
    /// Opens the review quorum's channel: stores the sender (which every
    /// [`ControlHandle`](crate::stop::ControlHandle) built afterwards clones) and hands the caller
    /// the receiver to give [`run_quorum_task`].
    ///
    /// A method rather than a public field so the sender cannot be reached from outside this crate:
    /// injecting a [`QuorumRequest`] is injecting Linear writes, and the only thing entitled to do
    /// that is a handoff the daemon itself executed. Call it BEFORE
    /// [`control`](Orchestrator::control), and only when the quorum is enabled — a daemon that
    /// never calls it has `quorum_tx: None`, which makes the fan-out unrepresentable rather than
    /// merely skipped.
    pub fn open_quorum_channel(&mut self) -> tokio::sync::mpsc::UnboundedReceiver<QuorumRequest> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.quorum_tx = Some(tx);
        rx
    }

    /// The shared per-project advisory state, so the off-loop quorum task can record a fan-out it
    /// gave up on where an operator will see it (STUDIO-822).
    ///
    /// Handed out as the [`Arc`] the warning resolver tasks already share rather than through a
    /// control event: the recording is a lock-guarded write to a map the control task only ever
    /// READS (in `project_statuses`), so it needs no round-trip, and a round-trip would be one more
    /// thing that cannot happen while the control task is busy — which is the state the daemon is
    /// in when the tracker is timing out.
    pub fn warnings_state(&self) -> Arc<crate::warnings::WarningsState> {
        Arc::clone(&self.warnings)
    }

    /// Whether the review quorum is on: Teams enabled AND `quorum.enabled`. Both, always — the
    /// quorum is opt-in ON TOP of an already opt-in feature (§0.12's cost control).
    ///
    /// **Minus the ticketless cutover** (STUDIO-719; ticketless-review design §14.2 "config
    /// cutover double-fire", §15-d): the ticket fan-out and the ticketless review read the SAME
    /// handoff, so an installation on the ticketless path must not also fan review tickets out —
    /// that is two agent runs per pushed head and twice the bill.
    /// [`Teams::validate`](rhapsody_config::teams::Teams::validate) already rejects the pairing
    /// when the file loads, so this subtraction is unreachable from a boot-loaded config; it is
    /// here because "exactly one review path fires" is a property of the daemon, not of the
    /// loader, and a config assembled any other way (a test, a future API write) must obey it too.
    pub(crate) fn quorum_enabled(&self) -> bool {
        self.teams
            .as_ref()
            .is_some_and(|t| t.enabled && t.quorum.enabled)
            && !self.review_ticketless_enabled()
    }

    /// Whether review runs on the TICKETLESS path: Teams enabled AND `review.mode: ticketless`
    /// (STUDIO-719; design §16). The gate every later slice of the subsystem checks, and
    /// deliberately the same shape as [`quorum_enabled`](Orchestrator::quorum_enabled) — Teams off
    /// or absent is dormant, with no second way to spell it.
    pub(crate) fn review_ticketless_enabled(&self) -> bool {
        self.teams.as_ref().is_some_and(|t| t.review_ticketless())
    }

    /// Snapshots what the poller just observed, so a handoff arriving between ticks can choose
    /// reviewers and check the marker without asking the tracker anything (§0.12: "from the
    /// daemon's in-memory candidate state — no extra tracker read").
    ///
    /// Called from the same place — and for the same reason — as
    /// [`record_issue_states`](Orchestrator::record_issue_states), and a hard no-op when the quorum
    /// is off, which is what makes "zero behaviour change with the quorum disabled" true of the
    /// control loop as well as of the task.
    ///
    /// **The load count is the candidate set's, and that is narrower than §0.11.1's definition.**
    /// §0.11.1 counts open tickets in any non-terminal state; the candidate fetch is active ∪ review
    /// (and, in the default claim mode, assignee-scoped), so a Backlog ticket wearing
    /// `rhapsody:@alice` does not count here. Deliberate: §0.12 chose the free in-memory count over
    /// a tracker read on purpose, and the direction of the error is benign — under-counting a
    /// teammate's backlog can only make the quorum spread reviews a little wider. Review tickets
    /// themselves ARE counted, because the fan-out creates them assigned and active, so a teammate
    /// holding two open reviews is correctly seen as busier than one holding none.
    ///
    /// Replaces rather than merges, [`record_issue_states`](Orchestrator::record_issue_states)'s
    /// rule and its reason: being absent is meaningful, so a map that only ever grew would keep
    /// asserting a PR link or a marker that was true a week ago.
    pub(crate) fn record_quorum_state<'a>(&mut self, issues: impl Iterator<Item = &'a Issue>) {
        if !self.quorum_enabled() {
            return;
        }
        let roster: HashSet<&str> = self
            .teams
            .as_ref()
            .map(|t| t.roster.iter().map(|i| i.name.as_str()).collect())
            .unwrap_or_default();
        let mut load: HashMap<String, i64> = HashMap::new();
        let mut facts: HashMap<String, QuorumFacts> = HashMap::new();
        for iss in issues {
            for label in iss.labels.iter().flatten() {
                let Some(name) = label.strip_prefix(IDENTITY_LABEL_PREFIX) else {
                    continue;
                };
                if roster.contains(name) {
                    *load.entry(name.to_string()).or_default() += 1;
                }
            }
            if iss.id.is_empty() {
                continue;
            }
            facts.insert(
                iss.id.clone(),
                QuorumFacts {
                    pr_url: open_pr_url(iss),
                },
            );
        }
        self.quorum_load = load;
        self.quorum_facts = facts;
    }

    /// Builds the fan-out plan for a handoff, or `None` when this handoff is not one the quorum
    /// fires on. Runs ON the control task, and every gate here is a comparison over data already in
    /// memory — no tracker call, no `.await`, nothing that could stall a tick.
    ///
    /// The gates, in the order §0.12 states them:
    ///
    /// 1. Teams and `quorum.enabled` are both on.
    /// 2. The run was dispatched AS a roster identity. A run with no identity is an ordinary
    ///    Rhapsody run and the quorum has no author to exclude and no team to ask.
    /// 3. The ticket names a team to create the review tickets in.
    ///
    /// §0.12's remaining two gates are deliberately NOT here, and both for the same reason: they
    /// are questions only GitHub can answer, and the control task makes no network call.
    ///
    /// * "the ticket has an open linked PR" moved to [`fan_out`] in STUDIO-674 — the URL is filled
    ///   from the candidate's Linear attachment when there is one and carried EMPTY when there is
    ///   not, and the quorum task resolves it by head branch. Keeping it here would mean a quorum
    ///   that is structurally dead wherever Linear holds no attachments.
    /// * "the ticket is not already marked" is GONE, not moved (STUDIO-822). The marker label said
    ///   *this ticket has been reviewed once*, and once-per-ticket refused exactly the rounds that
    ///   exist because a reviewer found something. What replaced it is per-HEAD and therefore also
    ///   [`fan_out`]'s, since the head is GitHub's answer and not this task's. The label is still
    ///   WRITTEN — see [`QUORUM_REQUESTED_LABEL`] — and a re-handoff now reaches the off-loop task,
    ///   which costs one `gh pr list` and, at an unchanged head, still fans out nothing.
    pub(crate) fn plan_quorum(
        &self,
        re: &crate::orchestrator::RunningEntry,
    ) -> Option<QuorumRequest> {
        if !self.quorum_enabled() || re.identity.is_empty() {
            return None;
        }
        // A ticket with no team id cannot be reviewed: `create_issue` needs a team to create in and
        // `add_issue_label` needs one to find-or-create the marker in, so EVERY write would fail —
        // and, because the parent would then stay unmarked, fail again on the next handoff, and the
        // next. Refusing here turns a permanent, recurring "REVIEW QUORUM FAILED" room post into
        // one debug line. Triage drops team-less tickets for the same reason, before spending a
        // model turn on them.
        if re.issue.team_id.is_empty() {
            tracing::debug!(
                issue = %re.issue.identifier,
                "teams quorum: the handed-off ticket has no team id, so no review ticket could be \
                 created in it; nothing is requested"
            );
            return None;
        }
        let teams = self.teams.as_ref()?;
        let facts = self.quorum_facts.get(&re.issue.id);
        // The Linear GitHub attachment when there is one, and NOT a gate (STUDIO-674): an
        // installation whose Linear↔GitHub link never materializes holds `attachments: []` on every
        // issue, so requiring a URL here made the quorum structurally dead for every ticket. An
        // empty URL is carried through to the off-loop task, which asks GitHub by head branch — the
        // source of truth the attachment was only ever a cache of — and drops the request there if
        // GitHub has no open PR either. Nothing on this path touches the network.
        let pr_url = facts.map(|f| f.pr_url.clone()).unwrap_or_default();
        // The remote the run pushed its branch to, with [`Orchestrator::bind_teams_run`]'s fallback
        // and for its reason: a legacy single-project config never populates `project_repo` (only
        // the resolved-project dispatch path sets it) and carries the repo top-level instead.
        // Without the fallback the head-branch lookup would silently resolve nothing there.
        let repo_url = if re.project_repo.is_empty() {
            self.eff
                .as_ref()
                .map(|eff| eff.cfg.repo.clone())
                .unwrap_or_default()
        } else {
            re.project_repo.clone()
        };
        let (pr_owner, pr_repo) = crate::ghsummons::parse_repo(&repo_url).unwrap_or_default();
        Some(QuorumRequest {
            parent_issue_id: re.issue.id.clone(),
            parent_team_id: re.issue.team_id.clone(),
            parent_identifier: re.issue.identifier.clone(),
            parent_title: re.issue.title.clone(),
            // The owning project, so the off-loop task creates the review ticket through THAT
            // project's slug-bound tracker (STUDIO-677). Empty on the legacy single-project path,
            // which is the account tracker's own slug and handled as such.
            parent_project_slug: re.project_slug.clone(),
            parent_project_group: self.quorum_warning_group(re),
            pr_url,
            // Derived from config alone, and carried even when the attachment already won so the
            // request's shape never depends on which path filled the URL. `symphony/<key>` is the
            // branch the run's worktree was created on (`rhapsody_workspace::Manager::ensure_*`), a
            // frozen cross-process contract; `project_repo` is the remote it was pushed to.
            pr_owner,
            pr_repo,
            pr_head_branch: format!("symphony/{}", sanitize_key(&re.issue.identifier)),
            author: re.identity.clone(),
            reviewers: select_reviewers(teams, &re.identity, &self.quorum_load),
            state_name: self.quorum_create_state(&re.project_slug),
            summon_token: self.quorum_summon_token(),
        })
    }

    /// The project GROUP a fan-out this daemon gives up on is reported against (STUDIO-822), so the
    /// advisory lands on the same `GET /api/v1/projects` row an operator already reads for that
    /// project.
    ///
    /// The run's own group when it has one. It may not: `project_group` is unset on the legacy
    /// single-project and test dispatch paths — `project_statuses` falls back to the slug for the
    /// same reason — so the slug's resolved project answers next, and a single-project
    /// installation (which resolves to exactly one project) answers from that one. An empty result
    /// records the advisory under the empty group, where nothing renders it and the `WARN` line
    /// remains the only trace; that is a degradation, not a silent one, and it takes a config with
    /// no resolvable project at all to reach.
    fn quorum_warning_group(&self, re: &crate::orchestrator::RunningEntry) -> String {
        if !re.project_group.is_empty() {
            return re.project_group.clone();
        }
        let Some(eff) = self.eff.as_ref() else {
            return re.project_slug.clone();
        };
        eff.projects
            .iter()
            .find(|p| p.slug == re.project_slug)
            .or_else(|| (eff.projects.len() == 1).then(|| &eff.projects[0]))
            .map(|p| p.group.clone())
            .unwrap_or_else(|| re.project_slug.clone())
    }

    /// The workflow state a review ticket is created in: the run's owning project's FIRST
    /// configured active state, resolved per-project ⊕ top-level exactly as
    /// `review_handoff_state` resolves `review_states[0]`.
    ///
    /// §0.12 says "Todo", and this is what "Todo" means in a workspace-agnostic port. Hard-coding
    /// the literal would be worse than wrong: state names vary per workspace, and a review ticket
    /// created in a state this daemon's `active_states` does not list is not a candidate — the
    /// fan-out would appear to work and no reviewer would ever wake. The first active state is by
    /// construction a state the daemon dispatches from, which is the property that actually matters.
    fn quorum_create_state(&self, project_slug: &str) -> String {
        let Some(eff) = self.eff.as_ref() else {
            return String::new();
        };
        let project = if project_slug.is_empty() {
            None
        } else {
            eff.cfg
                .projects
                .iter()
                .find(|p| p.slugs.iter().any(|s| s == project_slug))
        };
        rhapsody_config::effective_for(&eff.cfg, project)
            .active_states
            .into_iter()
            .next()
            .unwrap_or_default()
    }

    /// The configured summon token the reviewer's instructions tell them to lead their PR comments
    /// with. Empty config ⇒ the shipped default, because an instruction that names no token would
    /// silently produce reviews that never re-engage the author.
    fn quorum_summon_token(&self) -> String {
        let token = self
            .eff
            .as_ref()
            .map(|e| e.cfg.tracker.summon_token.trim())
            .unwrap_or_default();
        if token.is_empty() {
            rhapsody_core::SUMMON_TOKEN_SYMPHONY.to_string()
        } else {
            token.to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::ghsummons::OpenPrResult;
    use crate::testsupport::{TempDir, issue};
    use rhapsody_config::room::{Cursor, LocalRoom, RoomError};
    use rhapsody_config::teams::{Identity, Quorum, Review, ReviewMode};
    use rhapsody_core::LinkedPRRef;
    use rhapsody_tracker::TrackerError;
    use rhapsody_tracker::fake::Fake;

    fn ident(name: &str) -> Identity {
        Identity {
            name: name.to_string(),
            profile: "swe".to_string(),
            labels: Vec::new(),
            bank: String::new(),
            max_concurrent: 0,
        }
    }

    /// Teams ON with the quorum ON — the only configuration the fan-out runs under.
    fn teams_quorum(names: &[&str], reviewers: i64) -> Teams {
        Teams {
            enabled: true,
            quorum: Quorum {
                enabled: true,
                reviewers,
            },
            roster: names.iter().map(|n| ident(n)).collect(),
            ..Teams::disabled()
        }
    }

    fn request(reviewers: &[&str]) -> QuorumRequest {
        QuorumRequest {
            parent_issue_id: "iss-1".into(),
            parent_team_id: "team-1".into(),
            parent_identifier: "MT-1".into(),
            parent_title: "do the thing".into(),
            pr_url: "https://github.com/o/r/pull/7".into(),
            author: "alice".into(),
            reviewers: reviewers.iter().map(|r| (*r).to_string()).collect(),
            state_name: "Todo".into(),
            summon_token: "@symphony".into(),
            // The attachment path: `pr_url` is already set, so the branch fields go unread.
            ..Default::default()
        }
    }

    /// The STUDIO-674 shape: a ticket whose Linear carries NO GitHub attachment, so the request
    /// arrives with an empty `pr_url` and the branch the off-loop task must ask GitHub about.
    fn request_without_attachment(reviewers: &[&str]) -> QuorumRequest {
        QuorumRequest {
            pr_url: String::new(),
            pr_owner: "o".into(),
            pr_repo: "r".into(),
            pr_head_branch: "symphony/MT-1".into(),
            ..request(reviewers)
        }
    }

    /// The pull request a [`FakePrSource`] answers with: a URL and a head.
    fn open_pr(url: &str, head: &str) -> crate::ghsummons::OpenPr {
        crate::ghsummons::OpenPr {
            url: url.to_string(),
            head_sha: head.to_string(),
        }
    }

    /// An [`OpenPrSource`] that answers every lookup with `answer`, recording what it was asked.
    struct FakePrSource {
        answer: Box<dyn Fn() -> OpenPrResult + Send + Sync>,
        seen: Mutex<Vec<(String, String, String)>>,
    }

    impl FakePrSource {
        fn new(answer: impl Fn() -> OpenPrResult + Send + Sync + 'static) -> Arc<FakePrSource> {
            Arc::new(FakePrSource {
                answer: Box::new(answer),
                seen: Mutex::new(Vec::new()),
            })
        }

        fn calls(&self) -> Vec<(String, String, String)> {
            self.seen.lock().unwrap_or_else(|e| e.into_inner()).clone()
        }
    }

    #[async_trait::async_trait]
    impl OpenPrSource for FakePrSource {
        async fn open_pr_for_branch(&self, owner: &str, repo: &str, branch: &str) -> OpenPrResult {
            self.seen.lock().unwrap_or_else(|e| e.into_inner()).push((
                owner.to_string(),
                repo.to_string(),
                branch.to_string(),
            ));
            (self.answer)()
        }
    }

    /// The legacy single-project shape: one tracker, which is BOTH the account client and the
    /// slug-bound one, and no published project trackers. Requests built by [`request`] name no
    /// project, so the fan-out writes through it.
    fn deps(teams: Teams, tr: Arc<Fake>) -> QuorumDeps<impl Fn() -> Option<QuorumTarget>> {
        QuorumDeps {
            teams: Arc::new(teams),
            target: move || {
                Some(QuorumTarget {
                    tracker: Arc::clone(&tr) as Arc<dyn Tracker>,
                    project_trackers: Vec::new(),
                })
            },
            room: None,
            pr_source: None,
            max_backoff_ms: 20,
            warnings: None,
        }
    }

    /// The `projects:` shape (STUDIO-677): a slug-less ACCOUNT tracker beside the per-project
    /// clients the reload publishes. `account` stands in for the real thing by refusing every
    /// create with Linear's own message, so a fan-out that reached for it fails the test loudly
    /// rather than passing on a client the production one would have refused.
    fn deps_multi_project(
        teams: Teams,
        account: Arc<Fake>,
        projects: Vec<(&str, Arc<Fake>)>,
    ) -> QuorumDeps<impl Fn() -> Option<QuorumTarget>> {
        let published: Vec<ProjectTracker> = projects
            .into_iter()
            .map(|(slug, tr)| ProjectTracker {
                slug: slug.to_string(),
                tracker: tr as Arc<dyn Tracker>,
            })
            .collect();
        QuorumDeps {
            teams: Arc::new(teams),
            target: move || {
                Some(QuorumTarget {
                    tracker: Arc::clone(&account) as Arc<dyn Tracker>,
                    project_trackers: published.clone(),
                })
            },
            room: None,
            pr_source: None,
            max_backoff_ms: 20,
            warnings: None,
        }
    }

    /// A tracker with a viewer that refuses every create exactly as the real slug-less Linear
    /// client does — `resolve_project_id`'s pre-flight error, verbatim.
    fn slug_less_account_tracker() -> Fake {
        let mut tr = tracker_with_viewer();
        tr.create_issue_err = Some(TrackerError::Other(
            "linear_api_request: create issue requires a configured tracker.project_slug"
                .to_string(),
        ));
        tr
    }

    /// [`deps`] with the STUDIO-674 fallback lookup wired in.
    fn deps_with_pr_source(
        teams: Teams,
        tr: Arc<Fake>,
        src: Arc<FakePrSource>,
    ) -> QuorumDeps<impl Fn() -> Option<QuorumTarget>> {
        QuorumDeps {
            pr_source: Some(src as Arc<dyn OpenPrSource>),
            ..deps(teams, tr)
        }
    }

    fn deps_with_room(
        teams: Teams,
        tr: Arc<Fake>,
        room: Arc<dyn RoomLog>,
    ) -> QuorumDeps<impl Fn() -> Option<QuorumTarget>> {
        QuorumDeps {
            room: Some(room),
            ..deps(teams, tr)
        }
    }

    /// A tracker with a viewer to assign to — the fan-out refuses to create anything without one.
    fn tracker_with_viewer() -> Fake {
        let mut tr = Fake::new();
        tr.viewer = rhapsody_core::Viewer {
            id: "viewer-1".into(),
            ..Default::default()
        };
        tr
    }

    /// One fan-out attempt against a parent nothing has been requested for yet — what every test
    /// below wants unless it is specifically about the repeat handoff.
    async fn fan(
        d: &QuorumDeps<impl Fn() -> Option<QuorumTarget>>,
        req: &QuorumRequest,
    ) -> FanOutcome {
        fan_out(d, req, None).await.outcome
    }

    /// Every message in the room, oldest first.
    fn room_posts(room: &LocalRoom) -> Vec<rhapsody_config::room::Message> {
        room.read_since("reader", &Cursor::default(), 0)
            .expect("catch up")
            .messages
    }

    // ── reviewer selection (pure) ───────────────────────────────────────────────────────────────

    // The acceptance criterion, pinned with a KNOWN load state: the author is excluded, the roster
    // is ordered least-loaded first, and exactly `reviewers` are taken. Roster order here is
    // deliberately the OPPOSITE of load order, so a test that merely returned the roster would fail.
    #[test]
    fn reviewers_are_least_loaded_first_and_never_the_author() {
        let teams = teams_quorum(&["alice", "bob", "carol", "dave"], 2);
        let load = HashMap::from([
            ("alice".to_string(), 0), // the author: excluded regardless of load
            ("bob".to_string(), 5),
            ("carol".to_string(), 1),
            ("dave".to_string(), 3),
        ]);
        assert_eq!(
            select_reviewers(&teams, "alice", &load),
            vec!["carol".to_string(), "dave".to_string()],
            "least-loaded first, author excluded, capped at reviewers"
        );
    }

    // An identity with no open tickets counts as zero rather than being skipped — the idlest
    // teammate is the best reviewer, and `HashMap::get` returning `None` must not read as "unknown".
    #[test]
    fn an_absent_load_entry_counts_as_idle() {
        let teams = teams_quorum(&["alice", "bob", "carol"], 1);
        let load = HashMap::from([("bob".to_string(), 4)]);
        assert_eq!(select_reviewers(&teams, "alice", &load), vec!["carol"]);
    }

    // Ties break on ROSTER ORDER, exactly as `teams::route`'s label-overlap fallback does, so the
    // choice is deterministic across runs and a fresh team does not fan out at random.
    #[test]
    fn a_tie_breaks_on_roster_order() {
        let teams = teams_quorum(&["alice", "bob", "carol", "dave"], 2);
        let load = HashMap::new();
        assert_eq!(
            select_reviewers(&teams, "alice", &load),
            vec!["bob".to_string(), "carol".to_string()]
        );
    }

    // Too few candidates degrades to however many exist — never an error, never a wait (§0.12).
    #[test]
    fn a_short_roster_degrades_to_as_many_as_exist() {
        let teams = teams_quorum(&["alice", "bob"], 2);
        assert_eq!(
            select_reviewers(&teams, "alice", &HashMap::new()),
            vec!["bob"]
        );

        let solo = teams_quorum(&["alice"], 2);
        assert!(
            select_reviewers(&solo, "alice", &HashMap::new()).is_empty(),
            "a roster of one has nobody to ask"
        );
    }

    // ── the PR read ─────────────────────────────────────────────────────────────────────────────

    // A merged PR needs no review, so the URL is the first UNMERGED ref; an issue with none has no
    // URL at all, which is what makes "a handoff with no PR fires nothing" decidable in memory.
    #[test]
    fn open_pr_url_picks_the_first_unmerged_ref() {
        let pr = |number: i64, merged: bool| LinkedPRRef {
            owner: "o".into(),
            repo: "r".into(),
            number,
            merged,
        };
        let mut iss = issue("i1", "MT-1", "In Progress");
        assert_eq!(open_pr_url(&iss), "", "no refs ⇒ no url");

        iss.linked_prs = Some(vec![pr(1, true)]);
        assert_eq!(open_pr_url(&iss), "", "a merged PR needs no review");

        iss.linked_prs = Some(vec![pr(1, true), pr(7, false)]);
        assert_eq!(open_pr_url(&iss), "https://github.com/o/r/pull/7");
    }

    // ── the fan-out ─────────────────────────────────────────────────────────────────────────────

    // The headline acceptance: exactly `reviewers` review tickets, each Todo, viewer-assigned and
    // labelled `rhapsody:@<reviewer>`; the parent marked; ONE manager post naming the PR and the
    // reviewers.
    #[tokio::test]
    async fn a_fan_out_creates_a_ticket_per_reviewer_marks_the_parent_and_tells_the_room() {
        let dir = TempDir::new();
        let room = Arc::new(LocalRoom::new(dir.child("room")));
        let tr = Arc::new(tracker_with_viewer());
        let d = deps_with_room(
            teams_quorum(&["alice", "bob", "carol"], 2),
            Arc::clone(&tr),
            Arc::clone(&room) as Arc<dyn RoomLog>,
        );

        assert_eq!(
            fan(&d, &request(&["bob", "carol"])).await,
            FanOutcome::Fanned {
                created: 2,
                wanted: 2
            }
        );

        let calls = tr.create_issue_calls();
        assert_eq!(calls.len(), 2, "one review ticket per reviewer: {calls:?}");
        for (call, reviewer) in calls.iter().zip(["bob", "carol"]) {
            let s = &call.spec;
            assert_eq!(s.team_id, "team-1", "created beside the work it reviews");
            assert_eq!(s.state_name, "Todo");
            assert_eq!(
                s.assignee_id, "viewer-1",
                "unassigned tickets are never picked up"
            );
            assert_eq!(
                s.labels,
                vec![
                    format!("rhapsody:@{reviewer}"),
                    REVIEW_TICKET_LABEL.to_string()
                ],
                "the reviewer's identity routes it; the marker says what kind of ticket it is",
            );
            assert_eq!(s.title, "Review: MT-1 do the thing");
            assert!(
                s.description.contains("https://github.com/o/r/pull/7"),
                "the description names the PR: {}",
                s.description
            );
            assert!(
                s.description.contains("MT-1"),
                "and the parent: {}",
                s.description
            );
            assert!(
                s.description.contains("@symphony"),
                "and the summon token findings must carry: {}",
                s.description
            );
            assert!(
                s.description.contains("Never merge"),
                "and the one instruction that must always hold: {}",
                s.description
            );
        }

        let labels = tr.add_label_calls();
        assert_eq!(labels.len(), 1, "the parent is marked once: {labels:?}");
        assert_eq!(labels[0].issue_id, "iss-1");
        assert_eq!(labels[0].label_name, QUORUM_REQUESTED_LABEL);

        let posts = room_posts(&room);
        assert_eq!(posts.len(), 1, "exactly one manager post: {posts:?}");
        let m = &posts[0];
        assert_eq!(m.from, MANAGER_IDENTITY, "`from` is host-stamped");
        assert_eq!(m.to, rhapsody_config::room::Audience::Room);
        assert!(
            m.body
                .contains("Requested review of https://github.com/o/r/pull/7"),
            "{}",
            m.body
        );
        assert!(m.body.contains("bob, carol"), "{}", m.body);
        assert!(
            m.refs.contains(&"MT-1".to_string()),
            "the post refs the parent: {:?}",
            m.refs
        );
    }

    // §0.12: "zero ⇒ skip with a loud room post". A one-person team is a valid configuration; it
    // just cannot hold a quorum, and nothing is written to the tracker at all.
    #[tokio::test]
    async fn a_roster_of_one_writes_nothing_and_posts_loudly() {
        let dir = TempDir::new();
        let room = Arc::new(LocalRoom::new(dir.child("room")));
        let tr = Arc::new(tracker_with_viewer());
        let d = deps_with_room(
            teams_quorum(&["alice"], 2),
            Arc::clone(&tr),
            Arc::clone(&room) as Arc<dyn RoomLog>,
        );

        assert_eq!(fan(&d, &request(&[])).await, FanOutcome::NoReviewers);
        assert!(tr.create_issue_calls().is_empty(), "nothing is created");
        assert!(tr.add_label_calls().is_empty(), "and nothing is marked");

        let posts = room_posts(&room);
        assert_eq!(posts.len(), 1);
        assert!(
            posts[0].body.contains("NO REVIEW QUORUM"),
            "{}",
            posts[0].body
        );
        assert!(posts[0].body.contains("MT-1"), "{}", posts[0].body);
    }

    // Scope 7's stated tradeoff: 1 of 2 created still marks the parent and REPORTS the shortfall,
    // rather than leaving the parent unmarked so a later handoff re-creates the ticket that already
    // succeeded. A duplicate review ticket wakes a real agent for no reason; a stated gap does not.
    #[tokio::test]
    async fn a_partial_fan_out_marks_the_parent_and_names_the_shortfall() {
        let dir = TempDir::new();
        let room = Arc::new(LocalRoom::new(dir.child("room")));
        let mut fake = tracker_with_viewer();
        fake.create_issue_fail_first = 1; // bob's create fails, carol's succeeds
        let tr = Arc::new(fake);
        let d = deps_with_room(
            teams_quorum(&["alice", "bob", "carol"], 2),
            Arc::clone(&tr),
            Arc::clone(&room) as Arc<dyn RoomLog>,
        );

        assert_eq!(
            fan(&d, &request(&["bob", "carol"])).await,
            FanOutcome::Fanned {
                created: 1,
                wanted: 2
            }
        );
        assert_eq!(
            tr.create_issue_calls().len(),
            2,
            "one create failing does not abort the other"
        );
        assert_eq!(
            tr.add_label_calls().len(),
            1,
            "the parent is marked anyway, so this is never retried"
        );

        let body = &room_posts(&room)[0].body;
        assert!(body.contains("SHORTFALL"), "{body}");
        assert!(body.contains("bob"), "the failed reviewer is named: {body}");
        assert!(body.contains("1 of 2"), "{body}");
    }

    // Every create failing is a TrackerFailure: the parent is NOT marked (so a later handoff may
    // still try), no head is recorded, and the reason comes back for the task to report. NOTHING is
    // posted here — the room hears about it once, at exhaustion (STUDIO-822), because a per-attempt
    // post would say "abandoned" three times for one outage.
    #[tokio::test]
    async fn a_total_failure_leaves_the_parent_unmarked_and_carries_the_reason_out() {
        let dir = TempDir::new();
        let room = Arc::new(LocalRoom::new(dir.child("room")));
        let mut fake = tracker_with_viewer();
        fake.create_issue_err = Some(TrackerError::Other("linear_api_status: 503".into()));
        let tr = Arc::new(fake);
        let d = deps_with_room(
            teams_quorum(&["alice", "bob", "carol"], 2),
            Arc::clone(&tr),
            Arc::clone(&room) as Arc<dyn RoomLog>,
        );

        let res = fan_out(&d, &request(&["bob", "carol"]), None).await;
        assert_eq!(res.outcome, FanOutcome::TrackerFailure);
        assert!(
            tr.add_label_calls().is_empty(),
            "nothing was requested, so nothing is marked"
        );
        assert!(
            !res.outcome.records_the_head(),
            "a total failure wrote nothing, so a later handoff must still be able to ask"
        );
        assert!(
            res.failure_why.contains("linear_api_status: 503"),
            "the tracker's own error is carried out, not just the fact of failure: {}",
            res.failure_why
        );
        assert!(
            room_posts(&room).is_empty(),
            "an attempt that will be retried says nothing to the room"
        );
    }

    // Without a viewer to assign to, creating the tickets would be worse than creating none: an
    // unassigned ticket is never picked up, so the fan-out would look like it worked.
    #[tokio::test]
    async fn no_resolvable_viewer_creates_nothing() {
        let dir = TempDir::new();
        let room = Arc::new(LocalRoom::new(dir.child("room")));
        let mut fake = Fake::new();
        fake.viewer_err = Some(TrackerError::Other("linear_api_request: boom".into()));
        let tr = Arc::new(fake);
        let d = deps_with_room(
            teams_quorum(&["alice", "bob", "carol"], 2),
            Arc::clone(&tr),
            Arc::clone(&room) as Arc<dyn RoomLog>,
        );

        let res = fan_out(&d, &request(&["bob", "carol"]), None).await;
        assert_eq!(res.outcome, FanOutcome::TrackerFailure);
        assert!(tr.create_issue_calls().is_empty());
        assert!(
            res.failure_why.contains("never picked up"),
            "the viewer case keeps its own explanation: {}",
            res.failure_why
        );
    }

    // The room is advisory and Linear is the ledger (§0.11.4): a room that cannot be written costs
    // the team a paragraph of history and costs the fan-out nothing.
    #[tokio::test]
    async fn a_failing_room_does_not_cost_the_fan_out() {
        struct BrokenRoom;
        impl RoomLog for BrokenRoom {
            fn append(&self, _msg: &Message) -> Result<String, RoomError> {
                Err(RoomError::Io("disk on fire".into()))
            }
            fn read_since(
                &self,
                _reader: &str,
                _cursor: &Cursor,
                _limit: usize,
            ) -> Result<rhapsody_config::room::CaughtUp, RoomError> {
                Err(RoomError::Io("disk on fire".into()))
            }
            fn read_forward(
                &self,
                _reader: &str,
                _cursor: &Cursor,
                _limit: usize,
            ) -> Result<rhapsody_config::room::CaughtUp, RoomError> {
                Err(RoomError::Io("disk on fire".into()))
            }
        }
        let tr = Arc::new(tracker_with_viewer());
        let d = deps_with_room(
            teams_quorum(&["alice", "bob", "carol"], 2),
            Arc::clone(&tr),
            Arc::new(BrokenRoom) as Arc<dyn RoomLog>,
        );

        assert_eq!(
            fan(&d, &request(&["bob", "carol"])).await,
            FanOutcome::Fanned {
                created: 2,
                wanted: 2
            }
        );
        assert_eq!(tr.create_issue_calls().len(), 2);
        assert_eq!(tr.add_label_calls().len(), 1);
    }

    // A marker write that fails does not undo a fan-out that succeeded — but it IS said out loud,
    // because it means a restarted daemon could fan out a second time.
    #[tokio::test]
    async fn a_failed_marker_still_reports_the_created_tickets() {
        let dir = TempDir::new();
        let room = Arc::new(LocalRoom::new(dir.child("room")));
        let mut fake = tracker_with_viewer();
        fake.add_label_err = Some(TrackerError::Other("linear_move_rejected: nope".into()));
        let tr = Arc::new(fake);
        let d = deps_with_room(
            teams_quorum(&["alice", "bob", "carol"], 2),
            Arc::clone(&tr),
            Arc::clone(&room) as Arc<dyn RoomLog>,
        );

        assert_eq!(
            fan(&d, &request(&["bob", "carol"])).await,
            FanOutcome::Fanned {
                created: 2,
                wanted: 2
            }
        );
        let body = &room_posts(&room)[0].body;
        assert!(body.contains("WARNING"), "{body}");
        assert!(body.contains(QUORUM_REQUESTED_LABEL), "{body}");
    }

    // No tracker yet (the daemon has not loaded a config) is a failure the caller backs off on, not
    // a silent success.
    #[tokio::test]
    async fn no_tracker_is_a_backed_off_failure() {
        let d: QuorumDeps<fn() -> Option<QuorumTarget>> = QuorumDeps {
            teams: Arc::new(teams_quorum(&["alice", "bob"], 2)),
            target: || None,
            room: None,
            pr_source: None,
            max_backoff_ms: 20,
            warnings: None,
        };
        assert_eq!(
            fan(&d, &request(&["bob"])).await,
            FanOutcome::TrackerFailure
        );
    }

    // ── the writing tracker (STUDIO-677) ────────────────────────────────────────────────────────

    // The bug, pinned: on a `projects:` config the ACCOUNT tracker's `project_slug` is empty by
    // design, and `create_issue` refuses a slug-less client rather than mint a lost issue — so the
    // fan-out must write through the parent project's OWN client. The account fake here refuses
    // every create with Linear's real message, so a regression to `target.tracker` fails loudly.
    #[tokio::test]
    async fn a_slug_less_account_tracker_fans_out_through_the_parents_project_tracker() {
        let dir = TempDir::new();
        let room = Arc::new(LocalRoom::new(dir.child("room")));
        let account = Arc::new(slug_less_account_tracker());
        let mine = Arc::new(tracker_with_viewer());
        let other = Arc::new(tracker_with_viewer());
        let d = QuorumDeps {
            room: Some(Arc::clone(&room) as Arc<dyn RoomLog>),
            ..deps_multi_project(
                teams_quorum(&["alice", "bob", "carol"], 2),
                Arc::clone(&account),
                vec![
                    ("other-proj", Arc::clone(&other)),
                    ("my-proj", Arc::clone(&mine)),
                ],
            )
        };
        let req = QuorumRequest {
            parent_project_slug: "my-proj".into(),
            ..request(&["bob", "carol"])
        };

        assert_eq!(
            fan(&d, &req).await,
            FanOutcome::Fanned {
                created: 2,
                wanted: 2
            },
            "the parent's project tracker has the slug `create_issue` requires"
        );

        assert_eq!(
            mine.create_issue_calls().len(),
            2,
            "both review tickets are created in the parent's own project"
        );
        assert!(
            account.create_issue_calls().is_empty(),
            "the slug-less account tracker is never asked to create anything"
        );
        assert!(
            other.create_issue_calls().is_empty(),
            "and neither is another project's client"
        );
        assert_eq!(
            mine.add_label_calls().len(),
            1,
            "the parent is marked through the same client"
        );
        let posts = room_posts(&room);
        assert_eq!(posts.len(), 1, "one ordinary fan-out post: {posts:?}");
        assert!(
            posts[0].body.starts_with("Requested review of"),
            "{}",
            posts[0].body
        );
    }

    // The legacy single-project form stamps no project slug on a run, and THERE the account tracker
    // is the slug-bound client. An empty slug must therefore keep writing through it — this is the
    // path every pre-STUDIO-677 test exercises, asserted directly so the fallback is not accidental.
    #[tokio::test]
    async fn an_unnamed_project_still_writes_through_the_top_level_tracker() {
        let account = Arc::new(tracker_with_viewer());
        let proj = Arc::new(tracker_with_viewer());
        let d = deps_multi_project(
            teams_quorum(&["alice", "bob"], 1),
            Arc::clone(&account),
            vec![("some-proj", Arc::clone(&proj))],
        );

        assert_eq!(
            fan(&d, &request(&["bob"])).await,
            FanOutcome::Fanned {
                created: 1,
                wanted: 1
            }
        );
        assert_eq!(account.create_issue_calls().len(), 1);
        assert!(
            proj.create_issue_calls().is_empty(),
            "a run with no project must not be guessed into one"
        );
    }

    // A project paused or removed between the dispatch and the handoff publishes no tracker. The
    // fall-back is the account client, and when THAT is the slug-less one every create fails — which
    // must degrade into the fan-out's existing loud room post and an unmarked parent, never a
    // silent drop.
    #[tokio::test]
    async fn an_unresolvable_project_fails_loudly_and_leaves_the_parent_unmarked() {
        let dir = TempDir::new();
        let room = Arc::new(LocalRoom::new(dir.child("room")));
        let account = Arc::new(slug_less_account_tracker());
        let other = Arc::new(tracker_with_viewer());
        let d = QuorumDeps {
            room: Some(Arc::clone(&room) as Arc<dyn RoomLog>),
            ..deps_multi_project(
                teams_quorum(&["alice", "bob", "carol"], 2),
                Arc::clone(&account),
                vec![("other-proj", Arc::clone(&other))],
            )
        };
        let req = QuorumRequest {
            parent_project_slug: "gone-proj".into(),
            ..request(&["bob", "carol"])
        };

        let res = fan_out(&d, &req, None).await;
        assert_eq!(
            res.outcome,
            FanOutcome::TrackerFailure,
            "nothing was created, so the parent must stay unmarked for a later handoff"
        );
        assert!(
            other.create_issue_calls().is_empty(),
            "an unmatched slug never falls through to some OTHER project's client"
        );
        assert_eq!(
            account.create_issue_calls().len(),
            2,
            "the only client left IS tried — the failure is Linear's, not a refusal to ask"
        );
        assert!(
            account.add_label_calls().is_empty(),
            "a fan-out that created nothing does not mark the parent"
        );
        assert!(
            res.failure_why.contains("bob, carol"),
            "the reason names who was asked: {}",
            res.failure_why
        );
        assert!(
            res.failure_why.contains("tracker.project_slug"),
            "…and what Linear actually said: {}",
            res.failure_why
        );
        assert!(
            room_posts(&room).is_empty(),
            "the room hears about it once, at exhaustion, not once per attempt"
        );
    }

    // ── the head-branch PR fallback (STUDIO-674) ────────────────────────────────────────────────

    // The whole point of the ticket: an installation whose Linear holds no GitHub attachment still
    // fans out, because the off-loop task asks GitHub for the open PR on `symphony/<identifier>`
    // and the reviewers are handed THAT url.
    #[tokio::test]
    async fn a_request_without_an_attachment_resolves_the_pr_by_head_branch() {
        let tr = Arc::new(tracker_with_viewer());
        let src =
            FakePrSource::new(|| Ok(Some(open_pr("https://github.com/o/r/pull/64", "head-a"))));
        let d = deps_with_pr_source(
            teams_quorum(&["alice", "bob", "carol"], 2),
            Arc::clone(&tr),
            Arc::clone(&src),
        );

        assert_eq!(
            fan(&d, &request_without_attachment(&["bob", "carol"])).await,
            FanOutcome::Fanned {
                created: 2,
                wanted: 2
            }
        );

        assert_eq!(
            src.calls(),
            vec![(
                "o".to_string(),
                "r".to_string(),
                "symphony/MT-1".to_string()
            )],
            "exactly one lookup, for the run's repo and its branch"
        );
        let created = tr.create_issue_calls();
        assert_eq!(created.len(), 2, "one review ticket per reviewer");
        for call in &created {
            assert!(
                call.spec
                    .description
                    .contains("https://github.com/o/r/pull/64"),
                "the reviewer is pointed at the RESOLVED pr, not an empty url: {}",
                call.spec.description
            );
        }
        let labels = tr.add_label_calls();
        assert_eq!(
            labels.len(),
            1,
            "a resolved fan-out marks the parent exactly as an attachment-driven one does"
        );
        assert_eq!(labels[0].label_name, QUORUM_REQUESTED_LABEL);
    }

    // The attachment no longer skips the lookup (STUDIO-822): the head is what the repeat guard
    // compares, the attachment carries none, and GitHub is the only place one can be learned. When
    // GitHub answers, its answer — url AND head — is what reviewers are handed.
    #[tokio::test]
    async fn an_attachment_still_costs_a_lookup_because_it_carries_no_head() {
        let tr = Arc::new(tracker_with_viewer());
        let src =
            FakePrSource::new(|| Ok(Some(open_pr("https://github.com/o/r/pull/7", "head-a"))));
        let d = deps_with_pr_source(
            teams_quorum(&["alice", "bob"], 2),
            Arc::clone(&tr),
            Arc::clone(&src),
        );

        assert_eq!(
            fan(&d, &request(&["bob"])).await,
            FanOutcome::Fanned {
                created: 1,
                wanted: 1
            }
        );
        assert_eq!(src.calls().len(), 1, "the head is asked for even so");
        let body = &tr.create_issue_calls()[0].spec.description;
        assert!(body.contains("https://github.com/o/r/pull/7"), "{body}");
        assert!(
            body.contains("head-a"),
            "the head is named on the ticket: {body}"
        );
    }

    // …and when GitHub answers NOTHING, the attachment is still honoured: a lookup blip must not
    // turn into a missing review, which is the failure this whole ticket is about. The price is the
    // per-head granularity, which degrades to the per-pull-request guard it replaced.
    #[tokio::test]
    async fn an_attachment_is_the_fallback_when_the_lookup_answers_nothing() {
        for answer in [
            || -> OpenPrResult { Ok(None) },
            || -> OpenPrResult { Err("gh: command not found".into()) },
        ] {
            let tr = Arc::new(tracker_with_viewer());
            let src = FakePrSource::new(answer);
            let d = deps_with_pr_source(
                teams_quorum(&["alice", "bob"], 2),
                Arc::clone(&tr),
                Arc::clone(&src),
            );

            let res = fan_out(&d, &request(&["bob"]), None).await;
            assert_eq!(
                res.outcome,
                FanOutcome::Fanned {
                    created: 1,
                    wanted: 1
                }
            );
            assert!(res.head.is_empty(), "no head could be learned");
            let body = &tr.create_issue_calls()[0].spec.description;
            assert!(
                body.contains("https://github.com/o/r/pull/7"),
                "the attachment's url is what reviewers get: {body}"
            );
            assert!(
                !body.contains("**Head:**"),
                "an unknown head is omitted rather than rendered blank: {body}"
            );
        }
    }

    // No open PR on the branch either: the request is dropped where the ticket says it should be —
    // in the off-loop task — writing NOTHING. The parent stays unmarked, so a PR opened later is
    // still reviewable on the next handoff.
    #[tokio::test]
    async fn no_open_pr_on_the_branch_writes_nothing_and_does_not_settle_the_parent() {
        let tr = Arc::new(tracker_with_viewer());
        let src = FakePrSource::new(|| Ok(None));
        let d = deps_with_pr_source(
            teams_quorum(&["alice", "bob", "carol"], 2),
            Arc::clone(&tr),
            Arc::clone(&src),
        );

        assert_eq!(
            fan(&d, &request_without_attachment(&["bob", "carol"])).await,
            FanOutcome::NoPullRequest
        );
        assert!(tr.create_issue_calls().is_empty(), "no review ticket");
        assert!(tr.add_label_calls().is_empty(), "the parent is NOT marked");
        assert!(
            !FanOutcome::NoPullRequest.records_the_head(),
            "a later handoff, once the PR exists, must still be able to fan out"
        );
        assert!(
            !FanOutcome::NoPullRequest.is_failure(),
            "a ticket without a PR is a normal state, not an outage to back off from"
        );
    }

    // A lookup that could not be MADE is treated the same way — nothing written, nothing marked —
    // because a review request naming no PR is worse than none. It is warned about rather than
    // debugged (see `resolve_open_pr`), and it is not a back-off failure: only a handoff asks.
    #[tokio::test]
    async fn a_failing_lookup_writes_nothing() {
        let tr = Arc::new(tracker_with_viewer());
        let src = FakePrSource::new(|| Err("gh: command not found".into()));
        let d = deps_with_pr_source(
            teams_quorum(&["alice", "bob"], 2),
            Arc::clone(&tr),
            Arc::clone(&src),
        );

        assert_eq!(
            fan(&d, &request_without_attachment(&["bob"])).await,
            FanOutcome::NoPullRequest
        );
        assert!(tr.create_issue_calls().is_empty() && tr.add_label_calls().is_empty());
    }

    // With no source configured at all, the fallback is simply off and the pre-STUDIO-674 outcome
    // stands: an attachment-less request is dropped, silently and without writing.
    #[tokio::test]
    async fn without_a_pr_source_an_attachment_less_request_is_dropped() {
        let tr = Arc::new(tracker_with_viewer());
        let d = deps(teams_quorum(&["alice", "bob"], 2), Arc::clone(&tr));

        assert_eq!(
            fan(&d, &request_without_attachment(&["bob"])).await,
            FanOutcome::NoPullRequest
        );
        assert!(tr.create_issue_calls().is_empty() && tr.add_label_calls().is_empty());
    }

    // A request with no repo to ask about (a project with no GitHub remote) never reaches `gh`:
    // the source itself refuses an empty owner/repo/branch, so the outcome is the same drop.
    #[tokio::test]
    async fn a_request_with_no_repo_resolves_nothing() {
        let tr = Arc::new(tracker_with_viewer());
        let src = FakePrSource::new(|| Ok(None));
        let d = deps_with_pr_source(
            teams_quorum(&["alice", "bob"], 2),
            Arc::clone(&tr),
            Arc::clone(&src),
        );
        let req = QuorumRequest {
            pr_owner: String::new(),
            pr_repo: String::new(),
            pr_head_branch: String::new(),
            ..request_without_attachment(&["bob"])
        };

        assert_eq!(fan(&d, &req).await, FanOutcome::NoPullRequest);
        assert!(tr.create_issue_calls().is_empty() && tr.add_label_calls().is_empty());
    }

    // The task-level consequence of recording no head: two handoffs of the same ticket, the first
    // before the PR exists and the second after, fan out on the second. The repeat guard must not
    // have swallowed it.
    #[tokio::test]
    async fn a_second_handoff_after_the_pr_appears_fans_out() {
        let tr = Arc::new(tracker_with_viewer());
        let answers = Arc::new(Mutex::new(vec![
            Ok(None),
            Ok(Some(open_pr("https://github.com/o/r/pull/64", "head-a"))),
        ]));
        let queue = Arc::clone(&answers);
        let src = FakePrSource::new(move || {
            let mut q = queue.lock().unwrap_or_else(|e| e.into_inner());
            if q.is_empty() { Ok(None) } else { q.remove(0) }
        });
        let deps = deps_with_pr_source(
            teams_quorum(&["alice", "bob"], 2),
            Arc::clone(&tr),
            Arc::clone(&src),
        );
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let signal = crate::control_loop::CancelSignal::default();
        let task = tokio::spawn(run_quorum_task(signal.wait(), deps, rx));

        tx.send(request_without_attachment(&["bob"])).expect("send");
        tx.send(request_without_attachment(&["bob"])).expect("send");
        drop(tx);
        task.await.expect("the task ends when the sender is gone");

        assert_eq!(
            tr.create_issue_calls().len(),
            1,
            "the first handoff found no PR and settled nothing; the second fanned out"
        );
        assert_eq!(src.calls().len(), 2, "both handoffs asked GitHub");
    }

    // ── the shipped prompt prose ────────────────────────────────────────────────────────────────

    // The reviewer's description is agent-facing prose assembled from a backslash-continued Rust
    // literal, and prose has no compiler: STUDIO-599 shipped a prompt whose source indentation
    // leaked into the rendered text, invisible to every other kind of test. So this asserts on the
    // RENDERED whitespace — no line may start with the literal's own indentation — as well as on
    // the four things the description exists to say.
    #[test]
    fn the_reviewer_description_renders_without_leaking_source_indentation() {
        let req = request(&["bob"]);
        let body = review_description(&req, "bob", "head-a");

        for (n, line) in body.lines().enumerate() {
            assert!(
                !line.starts_with("  ") || line.trim_start().starts_with('-'),
                "line {n} leaks source indentation: {line:?}\n---\n{body}"
            );
            assert_eq!(
                line.trim_end(),
                line,
                "line {n} has trailing whitespace: {line:?}"
            );
        }
        // The substance, in the order §0.12 lists it: the PR, the parent, the author, and the job.
        assert!(body.contains(&req.pr_url), "{body}");
        assert!(body.contains(&req.parent_identifier), "{body}");
        assert!(body.contains(&req.parent_title), "{body}");
        assert!(body.contains(&req.author), "{body}");
        assert!(body.contains("independently"), "{body}");
        assert!(
            body.contains(&format!("starting with `{}`", req.summon_token)),
            "findings must name the CONFIGURED summon token, not a hard-coded one: {body}"
        );
        assert!(body.contains("Approve or request changes"), "{body}");
        assert!(body.contains("Never merge"), "{body}");
        // The reviewer is named, so the run knows which identity it is wearing.
        assert!(body.contains("You are **bob**"), "{body}");
    }

    // The title is the host template §0.12 names, and it leads with the parent's identifier so a
    // reviewer scanning a backlog can see what is under review without opening anything.
    #[test]
    fn the_review_title_names_the_parent() {
        assert_eq!(
            review_title("MT-1", "do the thing"),
            "Review: MT-1 do the thing"
        );
    }

    // ── the per-tick candidate snapshot ─────────────────────────────────────────────────────────

    fn orch_with(teams: Teams) -> Orchestrator {
        let mut o = Orchestrator::new("WORKFLOW.md");
        o.teams = Some(teams);
        o
    }

    fn with_labels(id: &str, labels: &[&str]) -> Issue {
        let mut iss = issue(id, id, "Todo");
        iss.labels = Some(labels.iter().map(|s| (*s).to_string()).collect());
        iss
    }

    // The sweep tallies §0.11.1's load per identity and reads each ticket's PR + marker, so a
    // handoff arriving between ticks costs no tracker read at all.
    #[test]
    fn the_candidate_sweep_records_load_and_facts() {
        let mut o = orch_with(teams_quorum(&["alice", "bob"], 2));
        let mut parent = with_labels("iss-1", &["rhapsody:@alice"]);
        parent.linked_prs = Some(vec![LinkedPRRef {
            owner: "o".into(),
            repo: "r".into(),
            number: 7,
            merged: false,
        }]);
        let marked = with_labels("iss-2", &["rhapsody:@bob", QUORUM_REQUESTED_LABEL]);
        let other = with_labels("iss-3", &["rhapsody:@bob"]);
        // A label naming nobody on the roster is not load: §0.11.1 makes a present label
        // authoritative for ROUTING, but a departed teammate is not somebody to hand a review to.
        let stray = with_labels("iss-4", &["rhapsody:@mallory"]);

        o.record_quorum_state([parent, marked, other, stray].iter());

        assert_eq!(o.quorum_load.get("alice"), Some(&1));
        assert_eq!(o.quorum_load.get("bob"), Some(&2));
        assert_eq!(o.quorum_load.get("mallory"), None, "off-roster is not load");
        assert_eq!(
            o.quorum_facts["iss-1"].pr_url,
            "https://github.com/o/r/pull/7"
        );
        assert!(
            o.quorum_facts.contains_key("iss-2"),
            "a marked parent is still a candidate: the marker label stopped being a refusal in \
             STUDIO-822, so the sweep records it like any other"
        );
    }

    // Replaces rather than merges: a ticket that has left the candidate set stops asserting a PR
    // link or a marker that was true a week ago.
    #[test]
    fn the_candidate_sweep_replaces_rather_than_merges() {
        let mut o = orch_with(teams_quorum(&["alice", "bob"], 2));
        o.record_quorum_state([with_labels("iss-1", &["rhapsody:@alice"])].iter());
        assert!(o.quorum_facts.contains_key("iss-1"));

        o.record_quorum_state([with_labels("iss-2", &["rhapsody:@bob"])].iter());
        assert!(
            !o.quorum_facts.contains_key("iss-1"),
            "a departed ticket must not linger"
        );
        assert_eq!(o.quorum_load.get("alice"), None);
        assert_eq!(o.quorum_load.get("bob"), Some(&1));
    }

    // The acceptance criterion, at the sweep: with the quorum off — or with Teams off entirely —
    // the per-tick pass is a hard no-op, not a pass whose result is merely unread.
    #[test]
    fn the_candidate_sweep_is_a_hard_no_op_when_the_quorum_is_off() {
        let mut teams_on_quorum_off = teams_quorum(&["alice", "bob"], 2);
        teams_on_quorum_off.quorum.enabled = false;
        let mut teams_off = teams_quorum(&["alice", "bob"], 2);
        teams_off.enabled = false;

        for teams in [Some(teams_on_quorum_off), Some(teams_off), None] {
            let mut o = Orchestrator::new("WORKFLOW.md");
            o.teams = teams.clone();
            assert!(!o.quorum_enabled(), "{teams:?}");
            o.record_quorum_state([with_labels("iss-1", &["rhapsody:@alice"])].iter());
            assert!(o.quorum_load.is_empty(), "{teams:?}");
            assert!(o.quorum_facts.is_empty(), "{teams:?}");
        }
    }

    // ── the ticketless cutover (STUDIO-719; design §14.2, §15-d, §16) ───────────────────────────

    // §16's gate, at the daemon: ticketless review is reachable ONLY with Teams enabled AND
    // `review.mode: ticketless`. Every other shape — Teams off, no Teams at all, another mode — is
    // dormant, which is the invariant the rest of the subsystem inherits rather than re-checks.
    #[test]
    fn review_ticketless_enabled_needs_teams_on_and_the_ticketless_mode() {
        let with = |enabled: bool, mode: ReviewMode| Teams {
            enabled,
            review: Review {
                mode,
                ..Review::default()
            },
            roster: vec![ident("alice")],
            ..Teams::disabled()
        };
        for (teams, want) in [
            (Some(with(true, ReviewMode::Ticketless)), true),
            (Some(with(true, ReviewMode::Tickets)), false),
            (Some(with(true, ReviewMode::Off)), false),
            (Some(with(false, ReviewMode::Ticketless)), false),
            (Some(Teams::disabled()), false),
            (None, false),
        ] {
            let mut o = Orchestrator::new("WORKFLOW.md");
            o.teams = teams.clone();
            assert_eq!(o.review_ticketless_enabled(), want, "{teams:?}");
        }
    }

    // §14.2's "config cutover double-fire", at the hook site: with the ticketless path on, the
    // handoff plans NO ticket fan-out even when `quorum.enabled` is somehow also true — one handoff
    // fires exactly one review path. `off` and `tickets` are today's behaviour, unchanged.
    #[test]
    fn the_ticketless_mode_guards_the_ticket_fan_out_off() {
        for (mode, want_plan) in [
            (ReviewMode::Off, true),
            (ReviewMode::Tickets, true),
            (ReviewMode::Ticketless, false),
        ] {
            let mut teams = teams_quorum(&["alice", "bob"], 2);
            teams.review = Review {
                mode,
                ..Review::default()
            };
            let mut o = orch_with(teams);
            let mut re = crate::orchestrator::RunningEntry::empty(Issue {
                id: "iss-1".to_string(),
                identifier: "MT-1".to_string(),
                team_id: "team-1".to_string(),
                ..Default::default()
            });
            re.identity = "alice".to_string();

            assert_eq!(o.quorum_enabled(), want_plan, "mode={mode:?}");
            assert_eq!(
                o.plan_quorum(&re).is_some(),
                want_plan,
                "the ticket fan-out under mode={mode:?}"
            );
            // And the per-tick candidate sweep goes with it: under `ticketless` there is no
            // fan-out to feed, so the pass is the same hard no-op it is with the quorum off.
            o.record_quorum_state([with_labels("iss-1", &["rhapsody:@alice"])].iter());
            assert_eq!(o.quorum_load.is_empty(), !want_plan, "mode={mode:?}");
        }
    }

    // ── the task ────────────────────────────────────────────────────────────────────────────────

    // A second handoff of the same parent at an unchanged head fans out nothing. This is the case
    // §0.12 reached for "once per ticket" to cover, and the per-head record still covers it — what
    // it no longer does is refuse the handoffs that arrive at a NEW head (STUDIO-822).
    #[tokio::test(flavor = "multi_thread")]
    async fn a_second_handoff_of_the_same_parent_fans_out_nothing() {
        let tr = Arc::new(tracker_with_viewer());
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let signal = crate::control_loop::CancelSignal::new();
        let d = deps(teams_quorum(&["alice", "bob", "carol"], 2), Arc::clone(&tr));
        let task = tokio::spawn(run_quorum_task(signal.wait(), d, rx));

        tx.send(request(&["bob", "carol"])).expect("first handoff");
        tx.send(request(&["bob", "carol"])).expect("second handoff");
        // Drop the sender so the task drains and returns rather than being killed mid-write.
        drop(tx);
        task.await.expect("task joins");

        assert_eq!(
            tr.create_issue_calls().len(),
            2,
            "the repeat handoff must create nothing: {:?}",
            tr.create_issue_calls()
        );
        assert_eq!(tr.add_label_calls().len(), 1);
        signal.cancel();
    }

    // Cancellation returns promptly rather than waiting out a queued request.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_task_stops_on_cancel() {
        let tr = Arc::new(tracker_with_viewer());
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<QuorumRequest>();
        let signal = crate::control_loop::CancelSignal::new();
        let d = deps(teams_quorum(&["alice", "bob"], 2), tr);
        let task = tokio::spawn(run_quorum_task(signal.wait(), d, rx));
        signal.cancel();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("the task stops promptly on cancel")
            .expect("task joins");
    }

    // A dropped sender ends the task: no handoff can arrive once the control handle is gone.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_task_ends_when_every_sender_is_dropped() {
        let tr = Arc::new(tracker_with_viewer());
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<QuorumRequest>();
        let signal = crate::control_loop::CancelSignal::new();
        let d = deps(teams_quorum(&["alice", "bob"], 2), tr);
        let task = tokio::spawn(run_quorum_task(signal.wait(), d, rx));
        drop(tx);
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("the task ends")
            .expect("task joins");
        signal.cancel();
    }

    // A tracker that refuses everything is retried a BOUNDED number of times and then abandoned —
    // never a hot loop, and never (as before STUDIO-822) a single attempt whose failure silently
    // lost the round.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_fan_out_is_retried_a_bounded_number_of_times() {
        let mut fake = tracker_with_viewer();
        fake.create_issue_err = Some(TrackerError::Other("linear_api_status: 503".into()));
        let tr = Arc::new(fake);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let signal = crate::control_loop::CancelSignal::new();
        let d = deps(teams_quorum(&["alice", "bob"], 1), Arc::clone(&tr));
        let task = tokio::spawn(run_quorum_task(signal.wait(), d, rx));

        tx.send(request(&["bob"])).expect("send");
        drop(tx);
        task.await.expect("task joins");

        assert_eq!(
            tr.create_issue_calls().len(),
            QUORUM_FANOUT_ATTEMPTS as usize,
            "one create per reviewer per attempt, and exactly QUORUM_FANOUT_ATTEMPTS attempts"
        );
        signal.cancel();
    }

    // A no-PR outcome returns before the tracker is even reached, so it is not evidence the tracker
    // recovered: mid-Linear-outage one attachment-less handoff would otherwise clear the back-off
    // the outage earned. It does not extend the back-off either (a ticket without a PR is a normal
    // state), so it leaves the counter exactly where it found it. The loop reads `is_failure`
    // first, which is why `TrackerFailure` answering both is not a contradiction. That
    // `NoReviewers` clears the back-off having made no tracker CALL is pre-existing behaviour,
    // pinned here as-is rather than changed by this ticket.
    #[test]
    fn a_no_pr_outcome_neither_extends_nor_clears_the_back_off() {
        assert!(!FanOutcome::NoPullRequest.is_failure());
        assert!(!FanOutcome::NoPullRequest.clears_the_backoff());
        for got_to_the_tracker in [
            FanOutcome::Fanned {
                created: 1,
                wanted: 2,
            },
            FanOutcome::NoReviewers,
        ] {
            assert!(
                got_to_the_tracker.clears_the_backoff(),
                "{got_to_the_tracker:?} got as far as the tracker"
            );
        }
        assert!(FanOutcome::TrackerFailure.is_failure());
    }

    // ── the per-head repeat guard (STUDIO-822) ──────────────────────────────────────────────────

    const PR: &str = "https://github.com/o/r/pull/64";

    /// The parent as it looks on round 2 and after: wearing the marker label round 1 wrote, which
    /// used to be the whole reason no reviewer was ever assigned again.
    fn marked_parent() -> Issue {
        let mut i = issue("iss-1", "MT-1", "In Review");
        i.team_id = "team-1".to_string();
        i.title = "do the thing".to_string();
        i.labels = Some(vec![
            QUORUM_REQUESTED_LABEL.to_string(),
            "rhapsody:@alice".to_string(),
        ]);
        i
    }

    /// The run `plan_quorum` decides on: `iss`, handed off by `identity`.
    fn running_entry(iss: Issue, identity: &str) -> crate::orchestrator::RunningEntry {
        crate::orchestrator::RunningEntry {
            identity: identity.to_string(),
            project_repo: "git@github.com:o/r.git".to_string(),
            ..crate::orchestrator::RunningEntry::empty(iss)
        }
    }

    /// Runs `reqs` through the task, front to back, against a source answering `heads` in order
    /// (and its last answer forever after). Returns the tracker the fan-outs wrote through.
    async fn run_handoffs(
        reqs: Vec<QuorumRequest>,
        heads: &[&str],
    ) -> (Arc<Fake>, Arc<FakePrSource>) {
        let tr = Arc::new(tracker_with_viewer());
        let queue: Vec<String> = heads.iter().map(|h| (*h).to_string()).collect();
        let queue = Arc::new(Mutex::new(queue));
        let src = FakePrSource::new(move || {
            let mut q = queue.lock().unwrap_or_else(|e| e.into_inner());
            let head = if q.len() > 1 {
                q.remove(0)
            } else {
                q[0].clone()
            };
            Ok(Some(open_pr(PR, &head)))
        });
        let d = deps_with_pr_source(
            teams_quorum(&["alice", "bob"], 1),
            Arc::clone(&tr),
            Arc::clone(&src),
        );
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let signal = crate::control_loop::CancelSignal::new();
        let task = tokio::spawn(run_quorum_task(signal.wait(), d, rx));
        for r in reqs {
            tx.send(r).expect("send");
        }
        drop(tx);
        task.await.expect("task joins");
        signal.cancel();
        (tr, src)
    }

    // THE acceptance, and it crosses BOTH of the guards this ticket removed — which is the point:
    // a test that drove only one would have passed while the other still refused every repeat
    // handoff, which is exactly how the operator's "clear the label" workaround failed.
    //
    //   * GUARD A, the marker label: the parent already wears it, and the CONTROL task must still
    //     build a request. A label cannot carry a commit, so it cannot tell round 2 of a pull
    //     request from the same round asked twice.
    //   * GUARD B, the in-memory record: the same request arrives twice with the author's fixes
    //     pushed in between, so GitHub answers a new head — and the second one fans out.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_re_handoff_at_a_new_head_fans_out_a_second_review() {
        let mut o = orch_with(teams_quorum(&["alice", "bob"], 1));
        o.record_quorum_state(std::iter::once(&marked_parent()));
        let req = o
            .plan_quorum(&running_entry(marked_parent(), "alice"))
            .expect("a parent already marked must still plan a fan-out");

        let (tr, src) = run_handoffs(vec![req.clone(), req], &["head-a", "head-b"]).await;

        let created = tr.create_issue_calls();
        assert_eq!(
            created.len(),
            2,
            "round 2 exists BECAUSE a reviewer found something; it needs its own reviewer"
        );
        assert!(
            created[0].spec.description.contains("head-a"),
            "{}",
            created[0].spec.description
        );
        assert!(
            created[1].spec.description.contains("head-b"),
            "the second review ticket names the NEW head, so the two rounds are tellable apart: {}",
            created[1].spec.description
        );
        assert_eq!(
            src.calls().len(),
            2,
            "each handoff asked GitHub where the PR is"
        );
    }

    // The inverse, and the reason the guards could not simply be deleted: a repeat handoff at the
    // SAME head — the shape a live review of that head is in — must fan out nothing, or a second
    // agent wakes onto a diff somebody is already reading.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_re_handoff_at_the_same_head_fans_out_nothing() {
        let mut o = orch_with(teams_quorum(&["alice", "bob"], 1));
        o.record_quorum_state(std::iter::once(&marked_parent()));
        let req = o
            .plan_quorum(&running_entry(marked_parent(), "alice"))
            .expect("planned");

        let (tr, src) = run_handoffs(vec![req.clone(), req], &["head-a"]).await;

        assert_eq!(
            tr.create_issue_calls().len(),
            1,
            "the head has not moved, so there is nothing new to review: {:?}",
            tr.create_issue_calls()
        );
        assert_eq!(
            tr.add_label_calls().len(),
            1,
            "and the parent is marked once, not once per handoff"
        );
        assert_eq!(
            src.calls().len(),
            2,
            "the repeat handoff still ASKS — the head is the only thing that can answer it"
        );
    }

    // A pull request GitHub answers without a `headRefOid` degrades to the per-pull-request guard
    // rather than fanning out on every handoff forever: an unknown head compares equal to the next
    // unknown head. Pinned because the alternative — treating an empty head as "new" — turns a
    // GitHub quirk into a duplicate-reviewer loop.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_unknown_head_degrades_to_one_fan_out_rather_than_repeating() {
        let req = request(&["bob"]);
        let (tr, _) = run_handoffs(vec![req.clone(), req], &[""]).await;
        assert_eq!(tr.create_issue_calls().len(), 1);
    }

    // ── the retry and its exhaustion (STUDIO-822, defect 2) ─────────────────────────────────────

    // The observed failure: ONE connection error to Linear, and that round's review simply did not
    // exist. It is a retry now, and the retry recovers on its own.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_transient_fan_out_failure_is_retried_and_recovers() {
        let mut fake = tracker_with_viewer();
        // The real thing, verbatim from `rhapsodyd.2026-09-09.log`: a request that never left.
        fake.create_issue_fail_first = 1;
        let tr = Arc::new(fake);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let signal = crate::control_loop::CancelSignal::new();
        let d = deps(teams_quorum(&["alice", "bob"], 1), Arc::clone(&tr));
        let task = tokio::spawn(run_quorum_task(signal.wait(), d, rx));

        tx.send(request(&["bob"])).expect("send");
        drop(tx);
        task.await.expect("task joins");

        assert_eq!(
            tr.create_issue_calls().len(),
            2,
            "the first attempt failed and the second one made the ticket"
        );
        assert_eq!(
            tr.add_label_calls().len(),
            1,
            "the recovered fan-out finishes normally"
        );
        signal.cancel();
    }

    // …and when the retries run out, the round is gone for good — so it must be visible somewhere
    // an operator looks. A `WARN` in a log running 23 MB/day was where it used to live.
    #[tokio::test(flavor = "multi_thread")]
    async fn exhausting_the_retries_surfaces_on_the_project_and_in_the_room() {
        let dir = TempDir::new();
        let room = Arc::new(LocalRoom::new(dir.child("room")));
        let warnings = Arc::new(crate::warnings::WarningsState::default());
        let mut fake = tracker_with_viewer();
        fake.create_issue_err = Some(TrackerError::Other(
            "linear_api_request: error sending request for url".into(),
        ));
        let tr = Arc::new(fake);
        let d = QuorumDeps {
            room: Some(Arc::clone(&room) as Arc<dyn RoomLog>),
            warnings: Some(Arc::clone(&warnings)),
            ..deps(teams_quorum(&["alice", "bob"], 1), Arc::clone(&tr))
        };
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let signal = crate::control_loop::CancelSignal::new();
        let task = tokio::spawn(run_quorum_task(signal.wait(), d, rx));

        tx.send(QuorumRequest {
            parent_project_group: "proj-a".into(),
            ..request(&["bob"])
        })
        .expect("send");
        drop(tx);
        task.await.expect("task joins");

        let advisories = warnings.merged_for("proj-a");
        assert_eq!(
            advisories.len(),
            1,
            "the abandoned round is on the project an operator reads: {advisories:?}"
        );
        assert!(advisories[0].contains("MT-1"), "{}", advisories[0]);
        assert!(
            advisories[0].contains("error sending request for url"),
            "naming what actually went wrong: {}",
            advisories[0]
        );

        let posts = room_posts(&room);
        assert_eq!(
            posts.len(),
            1,
            "ONE post for the whole outage, not one per attempt: {posts:?}"
        );
        assert!(
            posts[0].body.contains("REVIEW QUORUM ABANDONED for MT-1"),
            "{}",
            posts[0].body
        );
        signal.cancel();
    }

    // The advisory outlives the next success, alone among the warning producers. Every other one
    // describes a live condition that self-heals; this one describes a round that will never be
    // reviewed, and clearing it on an unrelated later handoff is how it stayed invisible.
    #[test]
    fn an_abandoned_fan_out_is_not_cleared_by_a_later_one() {
        let w = crate::warnings::WarningsState::default();
        for n in 1..=crate::warnings::LOST_REVIEW_WARN_CAP + 2 {
            w.record_lost_review("proj-a", &format!("MT-{n}"), "boom");
        }
        let got = w.merged_for("proj-a");
        assert_eq!(
            got.len(),
            crate::warnings::LOST_REVIEW_WARN_CAP,
            "capped, because nothing clears it: {got:?}"
        );
        assert!(
            got[0].contains("MT-3") && got.last().is_some_and(|l| l.contains("MT-7")),
            "the OLDEST are dropped, so the advisory always names the most recent: {got:?}"
        );
        assert!(
            w.merged_for("proj-b").is_empty(),
            "and it is scoped to its own project"
        );
    }
}
