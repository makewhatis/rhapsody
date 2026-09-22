//! reviewwatch — the edge-triggered watcher that turns a watch-set row into a review run
//! (STUDIO-721, slice 5 of the design record `~/.rhapsody/docs/STUDIO-703-ticketless-pr-review.md`,
//! §14.1, §14.4).
//!
//! **No Go counterpart.** Ticketless review is a Rhapsody addition end to end; this is the slice
//! that makes it fire. Slice 6 fills the watch set, slice 3 knows how to dispatch one review, slice
//! 4 knows how to wind one down — nothing until now decided WHEN.
//!
//! # The edge trigger is the whole design (§14.1 F-DUP)
//!
//! The obvious rule — "review whenever `head != last_reviewed_sha` and the pull request is open" —
//! is level-triggered, and level-triggered is a duplicate-dispatch machine. `last_reviewed_sha` is
//! written only at COMPLETION, so from introduction until the first review finishes the condition
//! is true on every tick: the watcher would dispatch a second agent onto the first one's detached
//! worktree, overwrite the live `running` entry (losing the cancel handle, so the first run can
//! never be stopped) and have the first exit dropped by the stale-guard.
//!
//! So the trigger is an EDGE, and it has three parts:
//!
//! * `requested_sha`, written at DISPATCH by [`Orchestrator::dispatch_review`], is the record that
//!   this head has already been asked about. A head equal to it fires nothing.
//! * `last_reviewed_sha`, written at COMPLETION, is the record that this head was actually READ.
//! * the live `running`/`claimed` sets are the record that a review is happening RIGHT NOW.
//!
//! [`review_round_due`] is those three facts and nothing else, and it is deliberately written as a
//! match over the row's `status` rather than a SHA comparison alone, because the two states that
//! still owe a review of the SAME head are invisible to a SHA comparison:
//!
//! * a row `in_flight` at a head with **no live run** is a CRASHED round — the exit path leaves the
//!   marker exactly where the dispatch put it (§14.1, "clear on crash"), so this is where it gets
//!   cleared, which is what re-surfaces a crashed review without a daemon restart;
//! * a row `truncated` is a round that ended without a declared verdict — either the agent burned
//!   its whole turn budget without finishing (STUDIO-721's carried slice-4 nit) or it declared a
//!   hand-off whose payload was neither `approved` nor a recognised rejection (STUDIO-894) — the
//!   head was read partially at best (or not conclusively at all), and `last_reviewed_sha` was
//!   deliberately not advanced, so nothing but the status distinguishes it from a row nobody has
//!   looked at.
//!
//! # Reviewer selection is re-made at DISPATCH, not trusted from introduction (§14.2)
//!
//! The row names a reviewer, but that name was chosen when the pull request was introduced, from
//! whatever the roster looked like then — and under `review.mode: ticketless` it was chosen against
//! an empty load map, because `quorum_load` is filled by `record_quorum_state`, which returns early
//! when the ticket fan-out is off. Two pull requests introduced in one tick therefore name the same
//! teammate. [`Orchestrator::choose_review_reviewer`] re-decides from a LIVE
//! [`LoadSnapshot`](crate::teams::LoadSnapshot) over `running` — which counts review runs, since
//! they are dispatched wearing the reviewer's identity. That load RANKS the candidates; it does not
//! exclude any of them. An identity's `max_concurrent` is deliberately not consulted (design D2,
//! "reviews are free"): it caps the implementation work a teammate is dispatched, never their
//! availability to review, and gating reviews on it only bought review latency — a busy teammate
//! deferred a round somebody else was waiting on (STUDIO-800).
//!
//! Decision B ("prefer the same reviewer on re-review") is honoured where it means something: a row
//! that HAS a last round keeps its reviewer, because continuity is worth something only to somebody
//! who read the previous round. A row that has never been reviewed has no continuity to preserve,
//! so it is selected fresh.
//!
//! # The head is re-read immediately before its OWN dispatch (STUDIO-953)
//!
//! The observation a tick decides from is a BATCH: up to
//! [`MAX_PR_STATE_CALLS_PER_TICK`](crate::prstate::MAX_PR_STATE_CALLS_PER_TICK) serial `gh`
//! round-trips, so without a re-read the first pull request's head can be seconds older than the
//! dispatch it feeds. [`run_review_watch_task`] therefore re-reads each OPEN observation's head
//! once, off-loop, and hands THAT observation to the control task immediately — before re-reading
//! the next — so no later pull request's blocking `gh` call can sit between a head and its
//! dispatch. The fresh answer is ADOPTED: refusing on a move would let a short-cycle author starve
//! the review entirely, which is strictly worse than reviewing slightly-stale code.
//!
//! **Why two passes.** They do different jobs: the sweep's read CLASSIFIES (a merged, closed, gone
//! or untrusted answer dispatches nothing and leaves the watch set; only an OPEN one may be
//! re-read) while the re-read PINS the head the dispatch records. One interleaved pass would do
//! both from a single call — N `gh` requests per tick instead of 2N — at the cost of the re-read's
//! FAILURE rule, which only exists when there are two answers to disagree: a failed read would
//! simply be a failed read, counted and re-asked next tick. Doubling the requests buys that
//! fallback, and is a deliberate price rather than an oversight (STUDIO-953).
//!
//! **What this closes, and what it does not.** It closes the window between the batched
//! observation and the dispatch — on this deployment that is the ~2.4 s the tick body spends on
//! its serial lookups. It does NOT close the window that produced makewhatis/rhapsody#185: there
//! the summon named `59108fe`, the run began at 02:37:19, the author's `6a8ce86` landed at
//! 02:41:31, and the daemon's own log shows the watcher still reading `59108fe` 4 m 10 s after the
//! dispatch — the head was correct when the review fired, and the author pushed DURING the run.
//! Closing that needs a check on the dispatch→verdict seam (`reviewnotify`), not here, and is out
//! of this ticket's scope. The cost of this guard is honest and not free: one extra `gh` call per
//! OPEN observation per tick, up to doubling this subsystem's share of GitHub's hourly budget.
//!
//! # A head move that carried no work arms nobody (STUDIO-960)
//!
//! The edge trigger above fires on the head MOVING, and it cannot tell why it moved. A rebase onto
//! `main`, a `gh pr update-branch`, a squash or an amend all rewrite every SHA while often
//! introducing exactly the change a reviewer already read — so the whole round is billed again, and
//! with STUDIO-959 that round is the expensive full cold read, because the old reviewed SHA is no
//! longer an ancestor of the new head.
//!
//! The fix asks the sharper question: did the DIFF change? The watcher compares the pull request's
//! three-dot diff against its base at the previously-reviewed head and at the new head (one `gh`
//! compare per SHA, off-loop, bounded by [`GH_EXEC_TIMEOUT`](crate::ghsummons::GH_EXEC_TIMEOUT)).
//! When the two are byte-identical it hands the reviewed SHA back in
//! [`PrObservation::unchanged_from`], and the control task advances `last_reviewed_sha` to the new
//! head while keeping the terminal status — an approval stays an approval, a rejection stays a
//! rejection, and neither round is re-earned. When the diff changed (a resolved conflict is the
//! canonical case), the comparison proves nothing, `unchanged_from` is empty, and a normal round is
//! armed exactly as before. A comparison that failed, timed out or could not read a file in full
//! also proves nothing: the one direction this must never fail is toward silently skipping a review.
//!
//! Only a row that COMPLETED a round can be carried: a `requested`, `in_flight` or `truncated` row
//! still owes a review of this head, whatever the diff says.
//!
//! # Off the loop, then back onto it (§5, F3)
//!
//! Asking GitHub where a pull request stands is a `gh` call, and [`crate::ghsummons::GH`] shells out
//! through a synchronous `std::process::Command` — off-task and bounded since STUDIO-829, but still
//! a round trip per watched pull request. [`run_review_watch_task`] owns every one of them, holds
//! no `Orchestrator`, and the only lock it shares with the control task is
//! [`crate::runautomerge::AutoMergeLedger`]'s — taken read-only by the control task through
//! [`crate::runautomerge::AutoMergeLedger::peek`] (STUDIO-923) — the same structural containment
//! [`crate::prstate`] was built for and documents. What comes back crosses to the control task as
//! a stream of [`Event::ReviewSweep`]s, one per observation, each handed over as soon as its head
//! is re-read, where the watch set stays single-writer beside `dispatch_review` and
//! `handle_review_introduce`, and where `running`/`claimed` can be read without a race.
//!
//! # Teams-gating (§16)
//!
//! Both loop-side handlers short-circuit on `review_ticketless_enabled()`, the task is only spawned
//! on that same condition, and `sweep_pr_states` refuses to spawn a process with Teams off. A
//! Teams-off daemon asks GitHub nothing and writes nothing.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;

use rhapsody_config::teams::Teams;
use rhapsody_core::Issue;
use rhapsody_store::{
    REVIEW_STATUS_APPROVED, REVIEW_STATUS_DROPPED, REVIEW_STATUS_REVIEWED, REVIEW_STATUS_TRUNCATED,
    ReviewWatchRow,
};

use crate::control_loop::{CancelWait, Event};
use crate::ghsummons::{
    HeadAllowlist, MERGE_STATE_DIRTY, PrLookup, PrSnapshot, PrStateSource, PrStatus,
    ReviewDiffSource,
};
use crate::orchestrator::Orchestrator;
use crate::prstate::{PrCoord, PrObservation, sweep_pr_states};
use crate::review::{ReviewDispatchOutcome, ReviewRun, review_key};
use crate::stop::ControlHandle;
use crate::teams::LoadSnapshot;

/// How many ROUNDS one pull request's review↔author loop may run, ever, in one daemon lifetime —
/// the floor against force-push churn (§14.2, "no approval terminal → unbounded re-review").
///
/// A ROUND, not a dispatch. `review_rounds` counts dispatches, and one round costs one dispatch per
/// required reviewer, so the check multiplies this by `teams.review.effective_reviewers()` before
/// comparing (STUDIO-727). Comparing the raw counter would silently divide the budget by the
/// reviewer count — at `reviewers: 8` a pull request would get its first round and never be
/// re-reviewed again, with nothing above `debug!` to say so.
///
/// **This cap bounds REVIEW rounds only, and that is deliberate.** Since the STUDIO-956 rewrite the
/// author side is bounded by the opt-in manager adjudication
/// ([`Orchestrator::adjudication_threshold`], `review.adjudicate_after_rounds`), not by this
/// constant: an install that sets no threshold keeps exactly the behaviour it had before this
/// ticket — this cap and its current stop — while the adjudication is opt-in. Charging author runs
/// to this counter unconditionally would shrink the review cap and stop the author loop on a
/// default install, which is not the byte-identical behaviour the ticket requires.
///
/// Eight is far above any honest review conversation (a review, fixes, a re-review, more fixes) so
/// a converging loop does not reach it, and far below a runaway.
///
/// The edge trigger already bounds the RATE: a round cannot start while one is in flight, so a
/// pull request costs at most one review per review's duration however fast its author pushes. What
/// it does not bound is the TOTAL, and an author amending in a loop — a rebase chain, a CI-driven
/// force-push, a `--fixup` habit, or a reviewer who keeps summoning — would otherwise buy a full
/// agent run per amendment forever. The adjudication threshold is the bound for that half.
///
/// **The counter is DURABLE, and a restart does not refund it** (STUDIO-956). It is written to
/// `rhapsody_review_bound` at every charge and rehydrated at boot
/// ([`Orchestrator::rehydrate_review_bounds`]), keyed by the PULL REQUEST, so it means "rounds spent
/// on this pull request" rather than "rounds since this daemon booted".
///
/// This replaces an earlier claim that keeping it in memory was right because "a restart clears it
/// too, which is the correct outcome for an operator who restarted the daemon to unstick something".
/// That was measured wrong. On 2026-09-20 there were five restarts, every one of them to apply a
/// boot-only `teams.yaml` change — i.e. caused by tuning the review configuration — and each one
/// handed seven in-flight pull requests a fresh budget: 46 review runs on one pull request against a
/// nominal cap of 16, 264 review runs that day. The effective bound was 16 PER RESTART, which is no
/// bound at all, and a pull request the manager had already escalated forgot the decision and
/// resumed the loop from zero.
///
/// The deliberate clear is still there and is now the only thing that lifts a bound in place:
/// [`Orchestrator::handle_review_clear`], `POST /api/v1/reviews/clear`. A pull request that leaves
/// the watch set — merged, closed, dismissed — has its row deleted, so a re-introduced, reopened or
/// rebuilt pull request never inherits a spent budget.
pub const REVIEW_ROUNDS_PER_PR_CAP: usize = 8;

/// How many CONSECUTIVE sweeps a round may find nobody to take it before the daemon stops treating
/// that as ordinary back-pressure and calls it stalled (STUDIO-891).
///
/// Deferring is normal and usually momentary: the reviewer is mid-round, or this tick's dispatch
/// budget is spent. What is not normal is deferring for the same reason forever, which is what a
/// roster that shrank underneath a live watch row produces — the incumbent is gone, every remaining
/// teammate is either the author or already holds another of this pull request's required reviews,
/// and no tick will ever change that on its own. Boot validation cannot see it: the config was
/// satisfiable when it was written.
///
/// Three, not one, because the first deferral of a round is usually the concurrency budget and
/// saying "stalled" there would cry wolf on every busy tick; and not thirty, because the whole
/// point is that an operator finds out before they mistake it for an idle board.
pub const REVIEW_UNASSIGNABLE_SWEEPS: usize = 3;

/// While a round stays stalled, the steady-state line is logged once per this many sweeps rather
/// than on every one. The transitions — crossing [`REVIEW_UNASSIGNABLE_SWEEPS`], and recovering —
/// always log regardless, mirroring how [`crate::preflight`] and [`crate::drain`] rate-limit
/// theirs. In sweeps rather than in a `Duration` because this counter is already in sweeps and a
/// clock here would be a second unit to keep honest.
pub const REVIEW_UNASSIGNABLE_LOG_EVERY: usize = 20;

/// The operator advisory surfaced on each project's `/api/v1/projects` status while any watched
/// round has been unassignable for [`REVIEW_UNASSIGNABLE_SWEEPS`] sweeps — the state-visible half,
/// exactly as [`crate::preflight::CREDENTIAL_DEAD_WARNING`] and [`crate::drain::DRAINING_WARNING`]
/// are for a paused dispatch. Silence is this condition's failure mode: a pull request that has
/// quietly stopped being reviewed looks precisely like one nobody has pushed to.
pub const REVIEW_UNASSIGNABLE_WARNING: &str = "a pull request review cannot be assigned — every eligible teammate is the author or already \
     holds one of its reviews; add a teammate or lower `review.reviewers`";

/// What one watcher tick did, reported rather than logged-and-forgotten so a caller (and a test)
/// can tell the four outcomes apart — they look identical in a silent no-op.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReviewSweepReport {
    /// Review runs dispatched this tick.
    pub dispatched: usize,
    /// (PR, reviewer) rows dropped out of the watch set — merged, closed, gone, or a head this
    /// daemon is not entitled to read.
    pub retired: usize,
    /// Rows that WANT a review and did not get one: this pull request's other rows left no
    /// eligible reviewer, an author-less row's incumbent has left the roster, the pull request hit
    /// [`REVIEW_ROUNDS_PER_PR_CAP`], or the repository is no longer configured. Every one of them
    /// is re-considered next tick. A reviewer being at their `max_concurrent` is NOT among the
    /// reasons — see [`Orchestrator::choose_review_reviewer`].
    pub deferred: usize,
    /// Rows re-armed to `requested` by the head-advance signal (design §14.1's in-process Event,
    /// standing in for the room post it forbids).
    pub armed: usize,
    /// Rows whose head moved but whose diff against the base was proven byte-identical to the one
    /// the row's verdict was made against, so the head move cost NO review round (STUDIO-960). A
    /// SUBSET of the rows the advance would otherwise have re-armed; they are disjoint from
    /// [`ReviewSweepReport::armed`]. Reported rather than silently no-op'd, because "the author
    /// pushed" and "the author rebased onto main" look identical in every other line this tick logs.
    pub skipped: usize,
    /// The implementation tickets whose pull request MERGED this tick, and the terminal state each
    /// is going to (STUDIO-712). A work LIST rather than a count, because the move itself is a
    /// tracker round-trip and must not happen on the control task: the loop resolves it, the
    /// watcher task performs it. Empty on every installation that has not named
    /// `teams.review.done_state`, and on every tick where nothing merged.
    pub done: Vec<crate::reviewdone::ReviewDonePlan>,
    /// The pull requests whose reviewer verdicts cleared the control task's half of the auto-merge
    /// gate this tick (STUDIO-874), each with the head those verdicts were recorded against.
    ///
    /// A work LIST for [`ReviewSweepReport::done`]'s reason: what remains is several `gh` calls and
    /// an irreversible one, which must not happen on the control task. Empty on every installation
    /// that has not set `teams.review.auto_merge`, and on every tick where nothing cleared.
    pub merge: Vec<crate::automerge::AutoMergePlan>,
    /// Rows that have now found nobody eligible for [`REVIEW_UNASSIGNABLE_SWEEPS`] consecutive
    /// sweeps (STUDIO-891) — a SUBSET of [`ReviewSweepReport::deferred`], and the part of it that
    /// is not going to resolve itself. Always `<= deferred`.
    pub stalled: usize,
    /// Conflicts routed back to their author this tick (STUDIO-961). A COUNT rather than a work
    /// list, because the work — a PR comment and a tracker move — is already handed to the review
    /// notification task on the control task, through the same channel a verdict's completion
    /// uses; the watcher task performs nothing extra for it.
    pub routed: usize,
    /// The pokes (and, at most once, the human escalation) that a finished run's still-draft pull
    /// request earned this tick (STUDIO-962). A work LIST for [`ReviewSweepReport::done`]'s reason:
    /// posting the summons is a `gh` call, and the room post an escalation makes is disk I/O, so
    /// neither may happen on the control task. Empty on every healthy board and on every tick where
    /// nothing is both finished and still a draft.
    pub nudges: Vec<crate::draftpoke::DraftNudge>,
    /// The pull requests whose round threshold was reached this tick and which the MANAGER must
    /// adjudicate (STUDIO-956). A work LIST for [`ReviewSweepReport::done`]'s reason: the decision
    /// is a model turn and a pair of writes, which must not happen on the control task. Empty on
    /// every installation that has not set `review.adjudicate_after_rounds`, and on every tick
    /// where no pull request reached it.
    pub adjudicate: Vec<crate::reviewadjudicate::ReviewAdjudicationPlan>,
}

/// The carried-budget sentinel meaning "this hand-back reached no decision": it spent nothing and
/// counted nothing, so the next hand-back's fresh count governs unchallenged (STUDIO-953).
///
/// `i64::MAX` rather than `0` because a hand-back that fails before the control task decides — a
/// store read that errors, a dropped reply, a dead control channel — must not convert into a zero
/// budget that retires every remaining observation of the tick. It never reaches the per-dispatch
/// decrement: [`Orchestrator::handle_review_sweep_slots`] clamps any carry to that tick's fresh
/// [`Orchestrator::review_dispatch_budget`] before spending it, so `MAX` only ever means "count
/// afresh".
const UNCAPPED_SLOTS: i64 = i64::MAX;

/// One pull request the watcher should ask GitHub about this tick, with the head SHAs its watch
/// rows have already had READ (STUDIO-960).
///
/// The reviewed SHAs travel beside the coordinate rather than being re-read by the watcher, which
/// holds no store: the control task is what reads the watch set, and this is the one fact the
/// off-loop diff comparison needs from it. The watcher keeps no row state of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchedPr {
    /// The pull request's repository and number.
    pub pr: PrCoord,
    /// Distinct non-empty `last_reviewed_sha` values across this pull request's live rows. A head
    /// equal to one of these is already read; a head different from all of them may have MOVED, and
    /// only then is a diff comparison worth its `gh` calls.
    pub reviewed_shas: Vec<String>,
    /// Distinct non-empty `requested_sha` values across the same rows — the heads a round has been
    /// DISPATCHED against. A head equal to one of these already has a round in flight, so the edge
    /// trigger arms nothing and the comparison is not needed either; carrying these is what keeps a
    /// long review from re-spending two `gh` calls per tick for its whole duration.
    pub requested_shas: Vec<String>,
}

impl WatchedPr {
    /// A watched pull request with no completed review yet — the shape a freshly-introduced row has.
    pub fn new(pr: PrCoord) -> WatchedPr {
        WatchedPr {
            pr,
            reviewed_shas: Vec::new(),
            requested_shas: Vec::new(),
        }
    }
}

/// Delivers the watcher's two control-task round-trips. A trait for [`ReviewIntroSink`]'s reason:
/// the task must be testable without a control loop, and the seam is what lets a test assert on the
/// coordinates handed over rather than on a side effect two hops away.
///
/// [`ReviewIntroSink`]: crate::reviewintro::ReviewIntroSink
#[async_trait]
pub trait ReviewWatchSink: Send + Sync {
    /// The pull requests worth asking GitHub about this tick, each with the head SHAs already read.
    async fn watched(&self) -> Vec<WatchedPr>;
    /// Hands observations to the control task and reports what it decided.
    ///
    /// `slots` is the daemon-wide dispatch budget this call may spend: `None` on a tick's FIRST
    /// hand-back, so the control task counts the budget then; `Some(left)` on every later one, so a
    /// tick's per-observation hand-backs spend ONE budget rather than recomputing it each time
    /// (STUDIO-953). The remaining budget comes back beside the report, ready to hand to the next
    /// call. Recomputing per hand-back would let a review worker that exits mid-tick hand its slot
    /// to a fresh round in the same tick, defeating the operator's `max_concurrent` for that tick.
    async fn sweep(
        &self,
        observed: Vec<PrObservation>,
        slots: Option<i64>,
    ) -> (ReviewSweepReport, i64);
    /// Reports the coordinates whose `gh` lookup FAILED this tick (STUDIO-950), so the control task
    /// can stop trusting a capacity hold for a pull request it can no longer read.
    ///
    /// Separate from [`Self::sweep`] rather than a field on it because a failure is a TICK-level
    /// fact: a coordinate whose lookup failed yields no observation at all, so there is no
    /// hand-back of its own to carry it, and it must still be reported on a tick where EVERY lookup
    /// failed — the watcher hands no observation back at all then. The same §16 master gate as
    /// every other entry point: a Teams-off daemon runs no tick and reports nothing.
    async fn unreadable(&self, failed: Vec<PrCoord>);
    /// Merges ONE pull request whose gates the control task cleared (STUDIO-874).
    ///
    /// On the sink for [`Self::finish`]'s reason: the remaining gates are GitHub round trips and
    /// the merge is irreversible, so both happen out here on the watcher's own task. Infallible by
    /// contract — a refusal is logged where it happens and the pull request is re-considered next
    /// tick, which is also what makes a merge that races a fresh push safe to simply lose.
    async fn merge(&self, plan: crate::automerge::AutoMergePlan);

    /// Moves ONE merged pull request's implementation ticket to its terminal state (STUDIO-712).
    ///
    /// On the sink rather than inside [`Self::sweep`] because it is a TRACKER write, and the point
    /// of the seam is that the network calls happen out here on the watcher's own task while the
    /// control task only ever decides. Infallible by contract: a failed move is logged where it
    /// happens and the ticket stays in review — there is no caller with anything to do about it.
    async fn finish(&self, plan: crate::reviewdone::ReviewDonePlan);

    /// Pokes ONE finished run's author when its pull request is still a draft, or escalates to a
    /// human once the poking is exhausted (STUDIO-962).
    ///
    /// On the sink for [`Self::finish`]'s reason: a poke is a `gh` comment (and an escalation a
    /// room append), neither of which may happen on the control task. Infallible by contract: a
    /// failed post is logged where it happens and the head is not re-poked — there is no caller
    /// with anything to do about it.
    async fn nudge(&self, nudge: crate::draftpoke::DraftNudge);

    /// Asks the MANAGER to adjudicate ONE pull request that has reached its round threshold
    /// (STUDIO-956) — ship it, or escalate.
    ///
    /// On the sink for [`Self::merge`]'s reason: the decision is a model turn plus a room write and
    /// a GitHub comment, none of which may happen on the control task. Infallible by contract: a
    /// failed turn is logged and the decision is re-asked on a later sweep, and there is no caller
    /// to return to.
    async fn adjudicate(&self, plan: crate::reviewadjudicate::ReviewAdjudicationPlan);
}

/// The production [`ReviewWatchSink`]: the control channel, through the same [`ControlHandle`] seam
/// every other off-loop→loop hand-back uses.
pub struct ControlWatchSink {
    control: ControlHandle,
    /// The `gh` seams an auto-merge needs, or `None` when the feature is off or this daemon has no
    /// GitHub source (STUDIO-874). Held HERE rather than reached through the control task: the
    /// merge needs no loop-owned state at all, so routing it through the control channel would
    /// queue an irreversible network call behind the current tick for no benefit.
    automerge: Option<Arc<crate::runautomerge::AutoMergeDeps>>,
    /// The `gh` comment seam and the room a draft poke (or its escalation) writes through
    /// (STUDIO-962). Held here for [`Self::automerge`]'s reason: the poke is a `gh` comment and the
    /// escalation also appends to the room, neither of which the control task may block on.
    ///
    /// Wiring is UNCONDITIONAL, like the auto-merge's and the findings route-back's: the feature's
    /// gate is the run having handed over a pull request, which lives on the control task where the
    /// plan is made, so an installation with nothing to poke sends no plan and this is never called.
    draft_poke: Option<crate::draftpoke::DraftPokeDeps>,
    /// The manager's adjudication turn and its two audit writes (STUDIO-956), or `None` when
    /// `review.adjudicate_after_rounds` is unset. Held here for [`Self::automerge`]'s reason: the
    /// turn is a model call and the writes are a room append and a `gh` comment, none of which the
    /// control task may block on.
    adjudication: Option<crate::reviewadjudicate::AdjudicationDeps>,
}

impl ControlWatchSink {
    pub fn new(control: ControlHandle) -> ControlWatchSink {
        ControlWatchSink {
            control,
            automerge: None,
            draft_poke: None,
            adjudication: None,
        }
    }

    /// Gives the sink the seams [`crate::runautomerge::perform_auto_merge`] needs. Without this
    /// the sink is inert on the merge path and says so once per plan, which is also what a daemon
    /// with no GitHub source gets.
    pub fn with_auto_merge(
        mut self,
        deps: Arc<crate::runautomerge::AutoMergeDeps>,
    ) -> ControlWatchSink {
        self.automerge = Some(deps);
        self
    }

    /// Gives the sink the comment and room seams a draft poke needs (STUDIO-962). Without this a
    /// plan is still emitted by the control task and this side says so once per plan, which a
    /// daemon with no GitHub source or no room reaches only for the half it is missing.
    pub fn with_draft_poke(mut self, deps: crate::draftpoke::DraftPokeDeps) -> ControlWatchSink {
        self.draft_poke = Some(deps);
        self
    }

    /// Gives the sink the manager adjudication turn and its audit writes (STUDIO-956). Without this
    /// a plan is still emitted by the control task and this side says so once per plan — which is
    /// also what a daemon whose threshold is unset never reaches, because no plan is emitted.
    pub fn with_adjudication(
        mut self,
        deps: crate::reviewadjudicate::AdjudicationDeps,
    ) -> ControlWatchSink {
        self.adjudication = Some(deps);
        self
    }
}

#[async_trait]
impl ReviewWatchSink for ControlWatchSink {
    async fn watched(&self) -> Vec<WatchedPr> {
        self.control.review_watch_list().await
    }
    async fn sweep(
        &self,
        observed: Vec<PrObservation>,
        slots: Option<i64>,
    ) -> (ReviewSweepReport, i64) {
        self.control.review_sweep(observed, slots).await
    }
    async fn unreadable(&self, failed: Vec<PrCoord>) {
        self.control.review_unreadable(failed).await
    }
    async fn merge(&self, plan: crate::automerge::AutoMergePlan) {
        let Some(deps) = self.automerge.as_ref() else {
            tracing::warn!(pr = %plan.pr, "auto-merge: no GitHub source is configured; not merging");
            return;
        };
        // Infallible by contract: no outcome has a caller with anything to do about it, and every
        // one of them is logged — the two that CHANGED something where the change happens, in
        // `runautomerge`, which holds the fields describing it, and the three that changed nothing
        // here. A refusal is re-considered on the next tick.
        match crate::runautomerge::perform_auto_merge(&plan, deps).await {
            crate::runautomerge::AutoMergeOutcome::Merged(_) => {}
            crate::runautomerge::AutoMergeOutcome::Updated => {}
            crate::runautomerge::AutoMergeOutcome::Declined(why) => {
                tracing::info!(pr = %plan.pr, reason = why, "auto-merge: declined")
            }
            // The same refusal as last tick, at the same head. Said once, above, on the tick it
            // was decided; saying it again every minute is what STUDIO-881 measured 182 of.
            crate::runautomerge::AutoMergeOutcome::Held(why) => {
                tracing::debug!(pr = %plan.pr, reason = why, "auto-merge: still declined")
            }
            crate::runautomerge::AutoMergeOutcome::Failed(err) => {
                tracing::warn!(pr = %plan.pr, %err, "auto-merge: a gate could not be read; not merging")
            }
        }
    }
    async fn finish(&self, plan: crate::reviewdone::ReviewDonePlan) {
        self.control.finish_review_ticket(plan).await
    }
    async fn nudge(&self, nudge: crate::draftpoke::DraftNudge) {
        let Some(deps) = self.draft_poke.as_ref() else {
            // The control task emits a plan, so this is only reachable on a daemon whose sink was
            // built without the seams. Say so once per plan rather than silently dropping it.
            tracing::warn!(
                "draft poke: the watcher has no comment/room seams; the author is not poked"
            );
            return;
        };
        // Infallible by contract: `perform_nudge` logs every failure and retries nothing.
        crate::draftpoke::perform_nudge(&nudge, deps, chrono::Utc::now()).await;
    }
    async fn adjudicate(&self, plan: crate::reviewadjudicate::ReviewAdjudicationPlan) {
        let Some(deps) = self.adjudication.as_ref() else {
            tracing::warn!(
                pr = %plan.pr,
                "review adjudication: no manager turn is configured; the loop stays stopped"
            );
            return;
        };
        // Infallible by contract: `perform_adjudication` logs every failure and records what it
        // decided, so there is nothing here to propagate.
        crate::reviewadjudicate::perform_adjudication(&plan, deps, chrono::Utc::now()).await;
    }
}

/// Everything [`run_review_watch_task`] runs against. No `Orchestrator`, no store and no control
/// channel of its own — the off-loop guarantee, in the type.
pub struct ReviewWatchDeps {
    /// Resolves a pull request's head SHA and state by NUMBER. `None` disables the watcher
    /// entirely: a daemon that cannot ask GitHub where a pull request stands has no honest way to
    /// decide anything, and acting on a stale row is exactly the lost update F-SHA describes.
    pub pr_source: Option<Arc<dyn PrStateSource>>,
    /// The head repositories a watched pull request may come from besides the base's own owner.
    pub allow: HeadAllowlist,
    /// The Teams config the §16 gate reads. A snapshot, like every other off-loop task's.
    pub teams: Teams,
    /// Where a tick's observations are handed back to the control task.
    pub sink: Arc<dyn ReviewWatchSink>,
    /// The two reads that prove a head move carried no new work (STUDIO-960). `None` disables the
    /// comparison: every head move then arms a normal round, which is exactly the behaviour before
    /// this feature, so a daemon that cannot ask (or an installation that never wires it) loses the
    /// saving and never the review.
    pub diff_source: Option<Arc<dyn ReviewDiffSource>>,
    /// How often to sweep, in milliseconds, read fresh each tick (STUDIO-974). A shared atomic
    /// rather than a copied value so a hot reload of `polling.pr_state_interval_ms` applies on the
    /// next tick without respawning the task. Defaults to
    /// [`DEFAULT_PR_STATE_INTERVAL_MS`](rhapsody_config::model::DEFAULT_PR_STATE_INTERVAL_MS) (15s)
    /// at boot; `<= 0` (a direct construction that skipped the field) falls back to that same
    /// default rather than a busy loop.
    pub poll_interval_ms: std::sync::Arc<std::sync::atomic::AtomicI64>,
}

/// Re-reads one OPEN observation's head once, off-loop, immediately before that observation is
/// handed to the control task — the re-verification that closes the window between the batched `gh`
/// lookup and the dispatch it feeds (STUDIO-953).
///
/// One observation at a time, not a second batch. A batch would re-create the very window it exists
/// to close: the first pull request's re-read would still wait behind every later pull request's
/// blocking `gh` call before its dispatch, which is the defect a reviewer reproduced on #189. Each
/// caller therefore re-reads and hands over in the same step, so nothing blocking sits between a
/// head and its dispatch.
///
/// The re-read is a READ, not a refusal: up to
/// [`MAX_PR_STATE_CALLS_PER_TICK`](crate::prstate::MAX_PR_STATE_CALLS_PER_TICK) lookups precede it,
/// so by the time the control task acts on the first pull request's answer its author may already
/// have pushed past it.
///
/// Three deliberate choices, all about not replacing a stale review with no review:
///
/// * a moved head is ADOPTED, not refused. Refusing whenever the head moved would let an author
///   pushing on a short cycle starve the review forever, which is strictly worse than reviewing
///   slightly-stale code. Adopting also keeps the edge trigger intact: the review that fires is one
///   review OF the new head, and `requested_sha` records it, so the next tick reads that head as
///   asked about and arms nothing further.
/// * a re-read that FAILS keeps the observed answer and says so once. A failed re-read is not
///   evidence the head moved, and refusing on it would be the same livelock by another route.
/// * a non-OPEN observation is not re-read at all: a merged, closed, gone or untrusted answer
///   dispatches nothing, so its head does not matter.
///
/// §16 first: with Teams off the sweep has already observed nothing, and this refuses to spawn a
/// process even if a stale watch set hands it one.
async fn refresh_observed_head(
    ctx: &CancelWait,
    teams: &Teams,
    src: &dyn PrStateSource,
    allow: &HeadAllowlist,
    obs: PrObservation,
) -> PrObservation {
    if !crate::prstate::pr_state_polling_enabled(teams) || ctx.is_cancelled() {
        return obs;
    }
    let open = matches!(&obs.lookup, PrLookup::Found(snap) if snap.status == PrStatus::Open);
    if !open {
        return obs;
    }
    match src
        .pr_state_unconditional(&obs.pr.owner, &obs.pr.repo, obs.pr.number, allow)
        .await
    {
        Ok(lookup) => PrObservation {
            unchanged_from: obs.unchanged_from,
            pr: obs.pr,
            lookup,
        },
        Err(e) => {
            tracing::warn!(
                pr = %obs.pr,
                error = %e,
                "pr-state re-read before dispatch failed; the observed head stands and the \
                 review dispatches from it"
            );
            obs
        }
    }
}

/// Which of a pull request's previously-reviewed heads carry a diff against the base that is
/// byte-identical to `head`'s — the proof that a head move did no work (STUDIO-960).
///
/// One `gh` read per DISTINCT reviewed head plus one for `head`, and none at all when nothing could
/// have moved (the caller filters that case out). The comparison is on the three-dot diff's text,
/// not on the SHAs and not on the history's shape: a rebase, a squash, an amend and a
/// `gh pr update-branch` all rewrite the head and can all carry the same change, which is exactly
/// the case this exists to detect. A rebase that resolved a conflict changes the diff text and is
/// therefore NOT in the answer.
///
/// Every failure — an unreadable base, a compare that timed out, a diff with a file GitHub will not
/// render — returns the reviewed heads it could NOT prove, i.e. omits them, so the caller arms a
/// normal round. The function never reports "unchanged" from a comparison it did not complete:
/// that direction is the one that silently skips a review of real work.
async fn unchanged_reviewed_shas(
    ctx: &CancelWait,
    src: &dyn ReviewDiffSource,
    pr: &PrCoord,
    head: &str,
    reviewed: &[String],
) -> Vec<String> {
    let head = head.trim();
    if head.is_empty() {
        return Vec::new();
    }
    let olds: Vec<&str> = reviewed
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty() && *s != head)
        .collect();
    if olds.is_empty() {
        return Vec::new();
    }
    let base = match src.pr_base_ref(&pr.owner, &pr.repo, pr.number).await {
        Ok(base) => base,
        Err(e) => {
            tracing::warn!(
                pr = %pr, error = %e,
                "ticketless review: the pull request's base branch could not be read; a head move \
                 is not proven to have carried no work, so a normal round will be armed"
            );
            return Vec::new();
        }
    };
    let head_patch = match src.merge_base_patch(&pr.owner, &pr.repo, &base, head).await {
        Ok(patch) => patch,
        Err(e) => {
            tracing::warn!(
                pr = %pr, base, head, error = %e,
                "ticketless review: the head's diff against the base could not be read; a normal \
                 round will be armed"
            );
            return Vec::new();
        }
    };
    let mut unchanged = Vec::new();
    for old in olds {
        if ctx.is_cancelled() {
            break;
        }
        match src.merge_base_patch(&pr.owner, &pr.repo, &base, old).await {
            Ok(patch) if patch == head_patch => unchanged.push(old.to_string()),
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(
                    pr = %pr, base, reviewed_sha = old, error = %e,
                    "ticketless review: a previously-reviewed diff against the base could not be \
                     read; that head is not proven unchanged, so a normal round will be armed"
                );
            }
        }
    }
    unchanged
}

/// The effective watcher cadence in milliseconds, read fresh from the shared atomic (STUDIO-974).
/// `<= 0` (unset, or a direct test construction that did not bother with the field) falls back to
/// [`DEFAULT_PR_STATE_INTERVAL_MS`](rhapsody_config::model::DEFAULT_PR_STATE_INTERVAL_MS) rather
/// than a zero-millisecond busy loop.
fn poll_interval(deps: &ReviewWatchDeps) -> i64 {
    let ms = deps
        .poll_interval_ms
        .load(std::sync::atomic::Ordering::Relaxed);
    if ms > 0 {
        ms
    } else {
        rhapsody_config::model::DEFAULT_PR_STATE_INTERVAL_MS
    }
}

/// Polls the watch set on the configured `polling.pr_state_interval_ms` (default
/// [`DEFAULT_PR_STATE_INTERVAL_MS`](rhapsody_config::model::DEFAULT_PR_STATE_INTERVAL_MS)) until
/// `ctx` is cancelled.
///
/// The interval is read from [`ReviewWatchDeps::poll_interval_ms`] at the TOP of each tick, so a hot
/// reload of the key applies on the next sleep without respawning this task.
///
/// Sleeps BEFORE its first tick, deliberately: the daemon's own boot recovery has to load config
/// and rebuild `running` first, and a tick that arrived before either would refuse everything and
/// achieve nothing but a burst of `gh` calls at start-up.
pub async fn run_review_watch_task(mut ctx: CancelWait, deps: ReviewWatchDeps) {
    let Some(src) = deps.pr_source.as_ref() else {
        tracing::info!(
            "ticketless review watcher: no GitHub source, so no pull request can be watched"
        );
        return;
    };
    tracing::info!(
        interval_ms = poll_interval(&deps),
        "ticketless review watcher started (off-loop; the control task is never blocked on gh)"
    );
    // Where this tick starts in the watch list. The list comes back in a STABLE order (owner, repo,
    // number, reviewer) and `sweep_pr_states` asks about at most `MAX_PR_STATE_CALLS_PER_TICK` of
    // it, so polling it from the front every tick would ask about the same first 20 pull requests
    // forever and never once look at the 21st — the budget's "picked up next tick" promise is the
    // CALLER's to keep, and this is where it is kept.
    let mut cursor = 0usize;
    loop {
        // Re-read each iteration: the value is hot-reloadable, so a change applies on the NEXT
        // sleep. `<= 0` is the unset/invalid case and falls back to the configured default.
        let interval_ms = poll_interval(&deps);
        tokio::select! {
            _ = ctx.cancelled() => return,
            () = tokio::time::sleep(std::time::Duration::from_millis(interval_ms as u64)) => {}
        }
        let prs = deps.sink.watched().await;
        if prs.is_empty() {
            cursor = 0;
            continue;
        }
        let start = cursor % prs.len();
        let rotated: Vec<WatchedPr> = prs[start..].iter().chain(&prs[..start]).cloned().collect();
        // Advance by the budget, not by what actually answered: a failed lookup has had its turn,
        // and holding the cursor back for it would starve everything behind it instead.
        cursor = start.saturating_add(crate::prstate::MAX_PR_STATE_CALLS_PER_TICK);
        // The reviewed and requested SHAs, by coordinate, for the diff comparison below. Kept here
        // rather than re-read from the store: this task holds none (STUDIO-960).
        let recorded: HashMap<PrCoord, WatchedPr> =
            rotated.iter().map(|w| (w.pr.clone(), w.clone())).collect();
        let coords: Vec<PrCoord> = rotated.into_iter().map(|w| w.pr).collect();
        let sweep = sweep_pr_states(&ctx, &deps.teams, src.as_ref(), &deps.allow, &coords).await;
        // `failed` is the list of coordinates GitHub would not answer for, not a count
        // (STUDIO-950 round 14) — the control task needs to know WHICH, so this reads emptiness
        // where STUDIO-960's line read a number.
        if sweep.deferred > 0 || !sweep.failed.is_empty() {
            tracing::debug!(
                observed = sweep.observed.len(),
                budget_deferred = sweep.deferred,
                failed = sweep.failed.len(),
                "ticketless review watcher: not every watched pull request answered this tick"
            );
        }
        // Report the failures BEFORE the observations (STUDIO-950 round 14): the control task must
        // stop trusting a capacity hold for a pull request GitHub would not answer for, and a tick
        // on which EVERY lookup failed hands back no observation at all — so this is the only place
        // the failure reaches it. A success this tick is the other direction and clears the record
        // on the control task's side (`handle_review_sweep_slots`).
        if !sweep.failed.is_empty() {
            deps.sink.unreadable(sweep.failed).await;
        }
        if sweep.observed.is_empty() {
            continue;
        }
        // Re-read each head and hand that observation over BEFORE re-reading the next (STUDIO-953).
        // One at a time, not a second batch: the batch above is serial, so an author can push
        // between the first lookup and the dispatch it leads to, and a second batch would leave the
        // first pull request waiting behind every later re-read exactly as before. ADOPTS the fresh
        // answer. The per-observation reports are folded back into one tick report so the log line
        // and the merge/Done work lists keep their tick shape.
        let mut report = ReviewSweepReport::default();
        // One dispatch budget for the whole tick, carried across the per-observation hand-backs
        // (STUDIO-953): a review worker that exits mid-tick must not hand its slot back to a fresh
        // round in the SAME tick, which is what recomputing the budget per hand-back would do.
        // `None` on the first hand-back tells the control task to count the budget then.
        let mut slots: Option<i64> = None;
        for obs in sweep.observed {
            let mut fresh =
                refresh_observed_head(&ctx, &deps.teams, src.as_ref(), &deps.allow, obs).await;
            // STUDIO-960: prove the head move carried no new work before the control task decides
            // whether to arm a round. Off-loop, bounded by GH_EXEC_TIMEOUT like every other read
            // here, and only when a head move is even possible: a head equal to one of this pull
            // request's reviewed SHAs is already read and costs nothing.
            if let Some(diff) = deps.diff_source.as_ref()
                && let PrLookup::Found(snap) = &fresh.lookup
                && snap.status == PrStatus::Open
                && let Some(known) = recorded.get(&fresh.pr)
                && !snap.head_sha.is_empty()
                // Nothing to prove with no reviewed head to compare against, and nothing to prove
                // when the head is already read or already dispatched: in every one of those cases
                // the edge trigger arms nothing, so the `gh` calls would be pure waste.
                && !known.reviewed_shas.is_empty()
                && !known
                    .reviewed_shas
                    .iter()
                    .chain(known.requested_shas.iter())
                    .any(|s| !s.is_empty() && s == &snap.head_sha)
            {
                fresh.unchanged_from = unchanged_reviewed_shas(
                    &ctx,
                    diff.as_ref(),
                    &fresh.pr,
                    &snap.head_sha,
                    &known.reviewed_shas,
                )
                .await;
            }
            let (one, left) = deps.sink.sweep(vec![fresh], slots).await;
            slots = Some(left);
            // Destructured rather than field-by-field so a field added later cannot be silently
            // dropped from the tick fold: the compiler flags the missing binding here.
            let ReviewSweepReport {
                dispatched,
                retired,
                deferred,
                armed,
                skipped,
                stalled,
                done,
                merge,
                routed,
                nudges,
                adjudicate,
            } = one;
            report.dispatched += dispatched;
            report.retired += retired;
            report.deferred += deferred;
            report.armed += armed;
            report.skipped += skipped;
            report.stalled += stalled;
            report.routed += routed;
            report.done.extend(done);
            report.merge.extend(merge);
            report.nudges.extend(nudges);
            report.adjudicate.extend(adjudicate);
        }
        if report != ReviewSweepReport::default() {
            tracing::info!(
                dispatched = report.dispatched,
                retired = report.retired,
                deferred = report.deferred,
                // The subset of `deferred` that is not going to resolve itself (STUDIO-891). Its
                // own field rather than folded into the count above, so a tick line that reads
                // "deferred = 1" every 30 seconds can be told apart from one that is waiting.
                stalled = report.stalled,
                armed = report.armed,
                // Head moves proven to have carried no new work (STUDIO-960): a re-arm that did
                // NOT happen, counted apart from `armed` so an operator can tell a rebase from a
                // push in the one line that reports the tick.
                skipped = report.skipped,
                done = report.done.len(),
                merge = report.merge.len(),
                routed = report.routed,
                nudges = report.nudges.len(),
                "ticketless review watcher tick"
            );
        }
        // The auto-merges (STUDIO-874), out here because each is several `gh` calls ending in an
        // irreversible one. Before the auto-Done moves below and not after: a merge performed now
        // is observed as MERGED by the NEXT tick, which is what produces its ticket's Done plan
        // through the existing STUDIO-712 path rather than a second one written here.
        for plan in report.merge {
            if ctx.is_cancelled() {
                return;
            }
            deps.sink.merge(plan).await;
        }
        // The auto-Done moves (STUDIO-712), out here because each is a tracker round-trip. Serially
        // and with a cancellation check between them, for `sweep_pr_states`' reasons: a shutting-down
        // daemon stops after at most one more call, and a merged pull request that goes unmoved leaves
        // its ticket exactly where this feature found it.
        for plan in report.done {
            if ctx.is_cancelled() {
                return;
            }
            deps.sink.finish(plan).await;
        }
        // The draft pokes (STUDIO-962), out here because each is a `gh` comment and an escalation
        // is also a room append. After the merges and the auto-Done moves and before the
        // adjudications, because nothing else waits on them: a poke that goes unposted costs one
        // nudge, and the head not moving means the next tick does not repeat it.
        for nudge in report.nudges {
            if ctx.is_cancelled() {
                return;
            }
            deps.sink.nudge(nudge).await;
        }
        // The manager adjudications (STUDIO-956), out here because each is a bounded model turn plus
        // a room append and a GitHub comment. Serially and with a cancellation check between them,
        // for the same reasons as the two loops above: a shutdown stops after at most one more turn,
        // and a decision that goes unmade is re-asked on a later sweep rather than lost.
        for plan in report.adjudicate {
            if ctx.is_cancelled() {
                return;
            }
            deps.sink.adjudicate(plan).await;
        }
    }
}

/// Whether this (PR, reviewer) row still owes a review OF `head` — the edge trigger, and the one
/// place the three facts that decide it are combined.
///
/// `in_flight_now` is whether a run for this exact key is live (`running` or `claimed`), which only
/// the control task can answer; it is a parameter rather than a lookup so the rule itself stays a
/// pure function a test can drive through every state.
pub(crate) fn review_round_due(row: &ReviewWatchRow, head: &str, in_flight_now: bool) -> bool {
    if !row.open || row.status == REVIEW_STATUS_DROPPED || head.is_empty() {
        return false;
    }
    // A healthy live review of this pair. Dispatching a second one overwrites its `running` entry
    // and points a second agent at its detached worktree (§14.1 F-DUP) — the single most damaging
    // thing this module can get wrong.
    if in_flight_now {
        return false;
    }
    match row.status.as_str() {
        // A round FINISHED at a head. Both terminals pause re-review identically while the pull
        // request sits at that head — which is §15-c's "approved-pauses" — and both re-arm the
        // moment the author pushes something nobody has read.
        REVIEW_STATUS_REVIEWED | REVIEW_STATUS_APPROVED => row.last_reviewed_sha != head,
        // Everything else still owes a review of this head, INCLUDING at the same SHA: `requested`
        // was never dispatched, `in_flight` without a live run is a crashed round, and `truncated`
        // is a round that ended without a declared verdict. The last two are why this is a status
        // match and not a SHA comparison — a SHA comparison calls all three "already handled".
        _ => true,
    }
}

/// The per-PR churn key: `owner/repo#number`, case-folded so two spellings of one repository cannot
/// each get their own budget.
///
/// `pub(crate)` for [`crate::reviewconsole`]: the operator's re-run and dismiss both retire a pull
/// request's budget, and a second spelling of this key would give them a different one from the
/// watcher's.
pub(crate) fn churn_key(pr: &PrCoord) -> String {
    format!("{}/{}#{}", pr.owner, pr.repo, pr.number).to_ascii_lowercase()
}

/// The first seven characters of a SHA, for a human-readable finding line. Character-safe rather
/// than byte-sliced: a head is hex in practice, but a malformed value must not panic a production
/// path.
fn short_sha(head: &str) -> String {
    head.chars().take(7).collect()
}

impl Orchestrator {
    /// The pull requests the watcher asks GitHub about this tick: every distinct coordinate the
    /// watch set still considers live, each beside the head SHAs its rows have already had READ
    /// (STUDIO-960).
    ///
    /// Distinct by coordinate rather than by row: N reviewers of one pull request share one head,
    /// and asking GitHub N times for it would spend the per-tick call budget on an answer already
    /// in hand. The reviewed SHAs are UNIONED across those rows and de-duplicated, because two
    /// reviewers can be at two different reviewed heads and each is a head the diff comparison must
    /// be able to prove against.
    pub(crate) fn review_watch_coords(&self) -> Vec<WatchedPr> {
        if !self.review_ticketless_enabled() {
            return Vec::new(); // §16
        }
        let rows = match self.store().load_live_review_watch() {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(err = %e, "ticketless review: the watch set could not be read; nothing is polled this tick");
                return Vec::new();
            }
        };
        let mut seen: HashMap<(String, String, i64), usize> = HashMap::new();
        let mut out: Vec<WatchedPr> = Vec::new();
        for row in rows {
            if !row.open || row.status == REVIEW_STATUS_DROPPED {
                continue;
            }
            let k = (
                row.key.owner.to_ascii_lowercase(),
                row.key.repo.to_ascii_lowercase(),
                row.key.number,
            );
            let idx = match seen.get(&k) {
                Some(i) => *i,
                None => {
                    let i = out.len();
                    out.push(WatchedPr::new(PrCoord::new(
                        &row.key.owner,
                        &row.key.repo,
                        row.key.number,
                    )));
                    seen.insert(k, i);
                    i
                }
            };
            let reviewed = row.last_reviewed_sha.trim();
            if !reviewed.is_empty() && !out[idx].reviewed_shas.iter().any(|s| s == reviewed) {
                out[idx].reviewed_shas.push(reviewed.to_string());
            }
            let requested = row.requested_sha.trim();
            if !requested.is_empty() && !out[idx].requested_shas.iter().any(|s| s == requested) {
                out[idx].requested_shas.push(requested.to_string());
            }
        }
        out
    }

    /// The ticketless review runs currently in flight — the count the separate review budget
    /// ([`Effective::max_concurrent_reviews`](crate::effective::Effective::max_concurrent_reviews))
    /// is drawn against when it is set.
    ///
    /// Only the TICKETLESS shape counts. This budget governs exactly what
    /// [`service_review_pr`](Self::service_review_pr) dispatches; a quorum review is a real tracker
    /// ticket on the implementation ladder, drawing the implementation budget, and counting it here
    /// would let it silently consume ticketless review capacity it never drew from.
    ///
    /// `pub(crate)` because the implementation ladders
    /// ([`select_dispatch_with_reopens`](Self::select_dispatch_with_reopens) and
    /// [`select_dispatch_multi_with_reopens`](Self::select_dispatch_multi_with_reopens)) subtract
    /// it from their own draw when the key is set — see
    /// [`implementation_pool_holders`](Self::implementation_pool_holders).
    pub(crate) fn running_ticketless_reviews(&self) -> i64 {
        i64::try_from(
            self.running
                .values()
                .filter(|re| re.review.is_some())
                .count(),
        )
        .unwrap_or(i64::MAX)
    }

    /// How many running entries currently SPEND the global pool the review watcher draws against
    /// (STUDIO-950). When `agent.max_concurrent_reviews` gives reviews their own pool that is the
    /// ticketless review runs alone; unset, it is EVERY running run on the shared
    /// `max_concurrent_agents` budget the watcher shared before the key existed.
    ///
    /// The count the watcher both DRAWS from and names in its capacity-hold log, so the log reports
    /// what actually spent the pool instead of always the reviews — in shared mode the pool is held
    /// by implementations too, and `holding=0` while four implementations spend it is a lie the
    /// operator tuning the key cannot act on.
    fn review_pool_holders(&self) -> i64 {
        match self.eff.as_ref().and_then(|e| e.max_concurrent_reviews) {
            Some(_) => self.running_ticketless_reviews(),
            None => i64::try_from(self.running.len()).unwrap_or(i64::MAX),
        }
    }

    /// Turns one tick's observations into drops, re-arms and review dispatches, counting a FRESH
    /// dispatch budget for the call. **The watcher's whole decision**, on the control task, where
    /// the watch set is single-writer and `running`/`claimed` cannot race.
    ///
    /// The one-shot shape is a test convenience: production always goes through
    /// [`Self::handle_review_sweep_slots`] so a tick's per-observation hand-backs share one budget.
    /// Kept as the batched contract because it is what most of this module's tests drive.
    #[cfg(test)]
    pub(crate) fn handle_review_sweep(&mut self, observed: &[PrObservation]) -> ReviewSweepReport {
        self.handle_review_sweep_slots(observed, None).0
    }

    /// [`Self::handle_review_sweep`]'s production shape, with the tick's dispatch budget carried
    /// across the watcher's per-observation hand-backs (STUDIO-953).
    ///
    /// `slots` is `None` on a tick's FIRST hand-back — [`Self::review_dispatch_budget`] counts it —
    /// and `Some(left)` on every later one, so a tick spends ONE budget however many hand-backs it
    /// makes. Returning the leftover rather than recomputing is what keeps a review worker that
    /// exits mid-tick from handing its slot to another round in the SAME tick.
    pub(crate) fn handle_review_sweep_slots(
        &mut self,
        observed: &[PrObservation],
        slots: Option<i64>,
    ) -> (ReviewSweepReport, i64) {
        let mut report = ReviewSweepReport::default();
        if !self.review_ticketless_enabled() {
            return (report, 0); // §16
        }
        // The watcher is alive and deciding THIS tick (STUDIO-950 round 11). The reconciliation
        // sweep ages this stamp against `CAPACITY_HOLD_TTL` to tell a hold a healthy watcher is
        // still carrying from one left by a sweep that has stopped happening. It is stamped here —
        // before the store read, so a local read failure still counts as a live tick — rather than
        // per hold, because the cursor reaches only `MAX_PR_STATE_CALLS_PER_TICK` pull requests a
        // tick and a held round re-evaluated once per rotation would otherwise age out while the
        // watcher is sweeping every tick.
        //
        // A stamp that has ITSELF aged past `CAPACITY_HOLD_TTL` means the watcher STOPPED between
        // the two sweeps — a `gh` outage delivers no sweep event at all. The holds it left are
        // already stale (`fresh_capacity_hold` ages them against this same stamp), but freshness
        // only FILTERS on read and never removes, so they are still in the map and re-stamping
        // liveness here would re-date every one of them — RESURRECTING a round nothing has
        // re-observed since before the outage (STUDIO-950 round 12). A first sweep after a gap
        // therefore DROPS them rather than re-dating them.
        //
        // A HEALTHY watcher can never trip the gap condition, and the reason is structural rather
        // than a matter of the gaps happening to be small. The gap is stamp-to-stamp, and with
        // `I = PR_STATE_POLL_INTERVAL`, `N = MAX_PR_STATE_CALLS_PER_TICK` and
        // `T = GH_EXEC_TIMEOUT`, the worst healthy gap is the interval, one full lookup phase
        // (`N*T`) and the ONE bounded pre-dispatch re-read that opens the next phase (`T`) —
        // `I + N*T + T` — while the TTL budgets `I + 2*N*T`. So `gap <= TTL` reduces to `T <= N*T`,
        // true for every `N >= 1`: the TTL always reserves a whole second phase that a healthy gap
        // cannot spend. (The two hand-backs WITHIN a tick are separated by at most one such bounded
        // `gh` call, which is well inside the same bound.)
        let swept_now = (self.now)();
        if let Some(prev) = self.review_watch_swept {
            // A clock that went backwards is not continuity either: treat it as a gap, so the holds
            // are dropped rather than re-dated onto a stamp the wall clock can never outrun.
            let gap = swept_now
                .signed_duration_since(prev)
                .to_std()
                .unwrap_or(CAPACITY_HOLD_TTL);
            if gap >= CAPACITY_HOLD_TTL {
                self.review_capacity_held.clear();
            }
        }
        self.review_watch_swept = Some(swept_now);
        // A coordinate that ANSWERED is readable again (STUDIO-950 round 14): drop the failure
        // record its failed lookups left, so a round it still holds can be named again and a
        // failure run that ended does not leave a stale entry behind.
        //
        // This runs ABOVE the store read, unlike the per-pull-request hold refresh further down
        // (STUDIO-950 round 15). An observation is direct evidence that GitHub ANSWERED for that
        // coordinate, and that is true whether or not a local SQLite read then succeeded; the hold
        // refresh below is a DECISION about rounds, which a failed read genuinely leaves unknown.
        // Keeping the clear below the read left a recovering pull request's failure count standing
        // on the tick it answered, so a later single failure would deny a hold GitHub had just
        // confirmed.
        for obs in observed {
            self.review_watch_unreadable.remove(&obs.pr);
        }
        let rows = match self.store().load_live_review_watch() {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(err = %e, "ticketless review: the watch set could not be read; this tick decides nothing");
                // This hand-back decides nothing, so it must SPEND nothing either: a carried budget
                // stands, and with none carried the next hand-back counts its own. Returning `0`
                // would convert one failed WAL read into a zero budget for every remaining
                // observation of the tick (STUDIO-953).
                return (report, slots.unwrap_or(UNCAPPED_SLOTS));
            }
        };
        // The daemon-wide dispatch budget, honoured for the same reason `select` honours it: a
        // review is a full agent run on this machine, and twenty pull requests coming due in one
        // tick would otherwise spawn twenty agents past a cap the operator set. TWO bounds compose
        // here and both are load-bearing (STUDIO-953): the CARRIED leftover bounds REPLENISHMENT —
        // a review worker that exits mid-tick cannot hand its slot to another round in the same
        // tick — while a FRESH count bounds CONSUMPTION, because the control task can start an
        // ordinary ticket run (`Event::Tick` → `on_tick`) between two hand-backs while the watcher
        // sits in a blocking `gh` call, and a tick must not spend slots that no longer exist.
        // Taking the smaller of the two keeps `running` at or below `max_concurrent` whichever
        // direction moves; dropping either half is a regression.
        //
        // STUDIO-950: the FRESH count is the pool the active key selects — reviews' own
        // `max_concurrent_reviews` when it is set, the shared `max_concurrent_agents` otherwise.
        //
        // The capacity holds are refreshed PER PULL REQUEST, as each observation re-evaluates its
        // rounds (`service_review_pr` drops the holds for the pull request it is deciding, and the
        // capacity branch below re-records every round it still defers), NOT cleared wholesale once
        // per tick. A round deferred for a different reason, or one whose round is now dispatchable,
        // stops annotating because ITS pull request was re-evaluated and did not defer it — which is
        // the property the wholesale clear was reaching for.
        //
        // What the wholesale clear got WRONG is a pull request the cursor did NOT reach. The sweep
        // visits only `MAX_PR_STATE_CALLS_PER_TICK` of the watch set per tick, so with a larger watch
        // set a continuously-held round's entry vanished on every tick that did not rotate to it and
        // returned on the next with no hold having ended (STUDIO-950 round 10). The reconciliation
        // sweep reads an absent entry as "not held by the most recent sweep", not "not held" (see
        // `Orchestrator::review_capacity_held`), so each blink re-logged its capacity line and
        // alternating sweeps re-logged the false "nothing has reported it blocked" one — defeating
        // the log's rate limit and re-emitting the very page this ticket closes. An unreached round
        // therefore KEEPS its hold, bounded only by the watcher's own liveness: while the watcher
        // keeps sweeping, `review_watch_swept` keeps advancing and the hold stays fresh however many
        // rotations it takes to revisit the round; when the watcher stops, the stamp stops, and the
        // first sweep after it returns DROPS every hold the gap left behind (the check above) rather
        // than re-dating them — freshness alone would only have filtered them on read, and
        // re-stamping would have resurrected them (STUDIO-950 round 12).
        //
        // The per-pull-request refresh is deliberately NOT before the store read above: a first
        // hand-back whose WAL read failed decided nothing about its rounds, so the tick's holds
        // there are unknown and the deliberate choice is to KEEP the previous tick's (still
        // `CAPACITY_HOLD_TTL`-fresh) records rather than blank the map. The continuity clear above
        // is a different thing and rightly precedes the read: a gap past the TTL means the holds
        // were already stale whatever this hand-back decides, so dropping them cannot blank a live
        // hold — it only stops a dead one from being resurrected.
        let fresh_budget = self.review_dispatch_budget();
        let mut slots = slots
            .map(|left| left.min(fresh_budget))
            .unwrap_or(fresh_budget);
        for obs in observed {
            match &obs.lookup {
                // GitHub cannot resolve it any more: deleted, transferred, or never there. Nothing
                // is left to observe, so it leaves the watch set for good.
                PrLookup::Gone => report.retired += self.retire_review_pr(&obs.pr, "gone"),
                // The head repository is neither the base's nor allowlisted. A review of it would
                // check out and execute a stranger's code (§14.1 F-SEC), so it can never be
                // dispatched — and re-asking every tick forever is not a plan.
                PrLookup::Untrusted => {
                    report.retired += self.retire_review_pr(&obs.pr, "untrusted head repository")
                }
                PrLookup::Found(snap) if snap.status != PrStatus::Open => {
                    // One drop, two different facts about the WORK — and only one of them says
                    // the ticket is finished (STUDIO-712). A closed-unmerged pull request is
                    // ABANDONED work whose ticket still needs a human; it is deliberately left
                    // where it is rather than auto-Cancelled, because that would destroy the one
                    // signal a maintainer has that something needs picking up. The ticket is
                    // read off `rows` — this tick's in-memory snapshot — so the retirement below
                    // cannot race it, whichever order the two run in.
                    let why = if snap.status == PrStatus::Merged {
                        report.done.extend(self.plan_review_done(&rows, &obs.pr));
                        "merged"
                    } else {
                        "closed"
                    };
                    report.retired += self.retire_review_pr(&obs.pr, why);
                }
                PrLookup::Found(snap) => {
                    // STUDIO-962: a finished run's pull request left in draft gets its author
                    // poked, once per head, before the review dispatch below — the two are
                    // independent and a draft may still owe a round.
                    self.plan_draft_poke(&rows, &obs.pr, snap, swept_now, &mut report);
                    self.service_review_pr(
                        &rows,
                        &obs.pr,
                        ObservedHead {
                            head: &snap.head_sha,
                            merge_state: &snap.merge_state,
                            unchanged_from: &obs.unchanged_from,
                        },
                        &mut slots,
                        &mut report,
                    )
                }
            }
        }
        (report, slots)
    }

    /// Records that a tick's `gh` lookup FAILED for every coordinate in `failed` (STUDIO-950
    /// round 14). The reconciliation sweep reads this to stop trusting a capacity hold for a pull
    /// request GitHub would not answer for: the watcher's global liveness
    /// ([`Orchestrator::review_watch_swept`]) is advanced by any ANSWERING sibling, so on its own it
    /// cannot tell a healthy unreached round from one whose pull request has become unreadable.
    ///
    /// The entry is a COUNT of CONSECUTIVE failed attempts, not a timestamp (STUDIO-950 round 15).
    /// The quantity being bounded is how long until the rotating cursor next reaches this pull
    /// request — a rotation of `ceil(watch_set / MAX_PR_STATE_CALLS_PER_TICK)` ticks, which no
    /// constant sized against the one-tick `CAPACITY_HOLD_TTL` can bound. Counting attempts is
    /// rotation-independent by construction: a tick on which the coordinate was not ASKED cannot
    /// move the counter either way. A success clears the entry entirely
    /// ([`Self::handle_review_sweep_slots`]), so the next failure starts a fresh run, and
    /// [`Self::fresh_capacity_hold`](crate::reviewreconcile) denies a hold once the count reaches
    /// [`UNREADABLE_ATTEMPTS_TO_DROP_HOLD`].
    pub(crate) fn handle_review_unreadable(&mut self, failed: &[PrCoord]) {
        if !self.review_ticketless_enabled() {
            return; // §16
        }
        for pr in failed {
            let attempts = self.review_watch_unreadable.entry(pr.clone()).or_insert(0);
            *attempts = attempts.saturating_add(1);
        }
    }

    /// The daemon-wide dispatch budget available to one watcher tick. Unset, that is
    /// `max_concurrent` less what is already running — every run draws the one pool. When
    /// `agent.max_concurrent_reviews` is set (STUDIO-950), ticketless reviews draw their OWN pool:
    /// that key less the ticketless reviews already running, so a round dispatches while
    /// implementations hold every `max_concurrent_agents` slot. No config loaded ⇒ no budget: a
    /// dispatch could not resolve a project to route with in any case.
    fn review_dispatch_budget(&self) -> i64 {
        self.eff
            .as_ref()
            .map(|eff| {
                let holding = self.review_pool_holders();
                match eff.max_concurrent_reviews {
                    Some(max_reviews) => crate::concurrency::global_slots(max_reviews, holding),
                    None => crate::concurrency::global_slots(eff.max_concurrent, holding),
                }
            })
            .unwrap_or(0)
    }

    /// Whether the implementation ticket `identifier` has a run in flight right now — the guard
    /// that keeps a draft poke off a run that is still going (STUDIO-962).
    ///
    /// Reads `running`'s own issues, not a second index: a review run's synthetic issue carries
    /// `pr:owner/repo#n@reviewer` as its identifier ([`crate::review::ReviewRun::key`]), so an
    /// in-flight REVIEW of the pull request does not read as a live AUTHOR run, while the author's
    /// own re-engaged run under the origin ticket does. `claimed` is deliberately not consulted — it
    /// holds opaque issue IDs, which cannot be matched against an identifier without a second lookup
    /// this path does not need; a claim that becomes a run is seen here on the next tick.
    fn ticket_run_live(&self, identifier: &str) -> bool {
        self.running
            .values()
            .any(|entry| entry.issue.identifier == identifier)
    }

    /// Plans the one poke — or, once the poking is exhausted, the human escalation — that a
    /// finished run's still-draft pull request earns this tick (STUDIO-962).
    ///
    /// The trigger is the HANDOFF, not the process exiting: the rows this acts on exist because a
    /// run handed its pull request over ([`crate::reviewintro`]) or the adoption sweep found a
    /// parked one ([`crate::reviewadopt`]), so an observed draft is by construction one the author's
    /// run has stopped working on. A row can also be introduced from the CONSOLE
    /// ([`crate::reviewconsole`]) — a third source, with no run behind it — and those are excluded
    /// by the `origin_ticket` gate below rather than by this sentence: a `console:` row names an
    /// operator, not a ticket, so there is no run for a summons to reopen.
    ///
    /// The guards that remain are [`Self::ticket_run_live`] — a draft is normal mid-run, so a live
    /// author run is never poked — and the per-head bookkeeping in [`Orchestrator::draft_pokes`],
    /// which makes the poke once per head and the escalation once ever rather than once per tick.
    ///
    /// The poking is bounded on two axes (STUDIO-962, jimmy's round-1 finding): after
    /// [`crate::draftpoke::MAX_DRAFT_POKES`] pokes (ATTEMPTS — the ledger remembers only the head
    /// poked last, so `A → B → A` spends the budget), and after
    /// [`crate::draftpoke::MAX_DRAFT_POKE_UNANSWERED`] of WALL CLOCK at the SAME head. The second
    /// is the one that matters for the incident this was filed on — a head that never moves would
    /// otherwise get one poke and then silence, which is the parking the ticket names.
    ///
    /// The second bound is wall clock and not a sweep count (STUDIO-974, jimmy's review): the
    /// watcher's cadence is a hot-reloadable key now, so a sweep count would shrink the grace with
    /// the tick. `now` is the tick's single clock instant, threaded rather than re-read, so every
    /// decision this sweep makes uses the same instant.
    fn plan_draft_poke(
        &mut self,
        rows: &[ReviewWatchRow],
        pr: &PrCoord,
        snap: &PrSnapshot,
        now: chrono::DateTime<chrono::Utc>,
        report: &mut ReviewSweepReport,
    ) {
        if snap.draft_published() {
            // GitHub positively said it is NOT a draft — it was published. Forget the count so a
            // re-draft starts afresh and the map does not grow for the daemon's whole life.
            self.draft_pokes.remove(&churn_key(pr));
            return;
        }
        if !snap.draft_observed() {
            // GitHub did not say. An unstated answer is not a resolved draft: forgetting here would
            // drop a ledger that may already have escalated and restart the poke cycle at the same
            // head — acting on a guess, the direction the summon read exists to refuse. Change
            // nothing; the ledger stands.
            return;
        }
        let head = snap.head_sha.trim();
        if head.is_empty() {
            return; // an answer with no head is not an answer about a head
        }
        let mine: Vec<&ReviewWatchRow> = rows.iter().filter(|r| row_is(r, pr)).collect();
        // Only a pull request this daemon parked for a TICKET can be re-engaged by a summons: the
        // token reopens that ticket's run. A `console:` row names an operator and has none.
        let Some(identifier) = mine
            .iter()
            .find_map(|r| crate::reviewdone::origin_ticket(&r.introduced_by))
        else {
            return;
        };
        if self.ticket_run_live(identifier) {
            // The author is working on it; a draft is entirely normal there. Re-anchor the
            // unanswered window so a long re-engaged run does not spend the grace: the window is
            // for an author who has STOPPED, not one mid-fix. (The old sweep count simply did not
            // advance while the run was live; a wall clock has to be pushed forward explicitly.)
            if let Some(state) = self.draft_pokes.get_mut(&churn_key(pr)) {
                state.unanswered_since = Some(now);
            }
            return;
        }
        let author = mine
            .iter()
            .find(|r| !r.author.is_empty())
            .map(|r| r.author.clone())
            .unwrap_or_default();
        let token = self.review_summon_token();
        let state = self.draft_pokes.entry(churn_key(pr)).or_default();
        if state.escalated {
            return;
        }
        if state.pokes > 0 && state.poked_head == head {
            // Already poked at this head: the author has not moved it. Hand it to a human once the
            // poke has clearly gone unanswered for the wall-clock grace — the bound that makes the
            // escalation reachable in the STATIC-head shape booch#537 had, where a distinct-head
            // ceiling alone would poke once and then go silent forever. Wall clock, not sweeps
            // (STUDIO-974): the cadence hot-reloads, so a sweep count would shrink the grace.
            // A missing anchor opens the window now rather than reading as an already-expired one.
            // The poke above always sets it, so this only covers an entry built without one. A
            // clock that went backwards yields a negative elapsed, which is never >= the grace.
            let since = *state.unanswered_since.get_or_insert(now);
            if now.signed_duration_since(since) >= crate::draftpoke::MAX_DRAFT_POKE_UNANSWERED {
                state.escalated = true;
                report.nudges.push(crate::draftpoke::DraftNudge::Escalate(
                    crate::draftpoke::DraftEscalation {
                        pr: pr.clone(),
                        identifier: identifier.to_string(),
                        author,
                        pokes: state.pokes,
                    },
                ));
            }
            return;
        }
        if state.pokes >= crate::draftpoke::MAX_DRAFT_POKES {
            state.escalated = true;
            report.nudges.push(crate::draftpoke::DraftNudge::Escalate(
                crate::draftpoke::DraftEscalation {
                    pr: pr.clone(),
                    identifier: identifier.to_string(),
                    author,
                    pokes: state.pokes,
                },
            ));
            return;
        }
        let pokes = state.pokes;
        state.poked_head = head.to_string();
        state.pokes += 1;
        // A new head is a fresh poke: the unanswered window reopens, because the author has
        // demonstrably done something since the last poke.
        state.unanswered_since = Some(now);
        report.nudges.push(crate::draftpoke::DraftNudge::Poke(
            crate::draftpoke::DraftPokePlan {
                pr: pr.clone(),
                head: head.to_string(),
                author,
                summon_token: token,
                pokes,
            },
        ));
    }

    /// How many dispatches one review ROUND costs for this installation — the unit the shared
    /// review↔author budget ([`REVIEW_ROUNDS_PER_PR_CAP`]) is counted in.
    ///
    /// The floor of one matches `service_review_pr`'s own `.max(1)`: a misconfigured
    /// `review.reviewers: 0` must still cost a round rather than make the budget free.
    pub(crate) fn reviewers_per_round(&self) -> usize {
        self.teams
            .as_ref()
            .map_or(1, |t| t.review.effective_reviewers().max(1))
    }

    /// The shared review↔author round budget for one pull request, in dispatches.
    fn shared_round_budget(&self) -> usize {
        REVIEW_ROUNDS_PER_PR_CAP.saturating_mul(self.reviewers_per_round())
    }

    /// Whether `pr`'s legacy REVIEW round budget ([`REVIEW_ROUNDS_PER_PR_CAP`]) is spent.
    ///
    /// A pull request the watcher has never charged (no entry) is not spent: nothing about it is
    /// bounded, which is what makes a daemon with ticketless review off byte-identical to one built
    /// before this budget existed. The author side does NOT read this — see
    /// [`Orchestrator::author_round_budget_spent`].
    pub(crate) fn round_budget_spent(&self, pr: &PrCoord) -> bool {
        self.review_rounds.get(&churn_key(pr)).copied().unwrap_or(0) >= self.shared_round_budget()
    }

    /// The linked pull requests of `iss` that already carry a shared budget — ones a review round
    /// has charged. A ticket whose pull request was never reviewed has none, so an ordinary fresh
    /// dispatch can neither create a budget nor charge one.
    fn charged_linked_prs(&self, iss: &Issue) -> Vec<PrCoord> {
        iss.linked_prs
            .iter()
            .flatten()
            .map(|r| PrCoord::new(&r.owner, &r.repo, r.number))
            .filter(|pr| self.review_rounds.contains_key(&churn_key(pr)))
            .collect()
    }

    /// Whether a summons-driven AUTHOR re-dispatch of `iss` must be refused because the adjudication
    /// threshold of one of its pull requests is reached, or the manager has already decided
    /// (STUDIO-956).
    ///
    /// **Only armed under the opt-in threshold.** With `review.adjudicate_after_rounds` unset the
    /// answer is `false` unconditionally: the legacy cap bounds REVIEW rounds only, and the author
    /// side is exactly as unbounded as it was before this ticket. That is the byte-identical-when-
    /// unset property the revised ticket's last ⚠️ requires.
    pub(crate) fn author_round_budget_spent(&self, iss: &Issue) -> bool {
        let Some(threshold) = self.adjudication_threshold() else {
            return false;
        };
        let charged = self.charged_linked_prs(iss);
        if charged.is_empty() {
            return false;
        }
        // A manager decision — settled or still in flight — stops the author half on its own.
        if charged.iter().any(|pr| self.adjudication(pr).is_some()) {
            return true;
        }
        // A pull request that CONVERGED at the threshold is not bounded. The review half declines to
        // adjudicate it (`service_review_pr` lets every-live-row-approved fall to the ordinary
        // auto-merge path), so refusing the author here would freeze a healthy pull request with no
        // decision in the ledger and nothing reporting it. Read the same live rows the review half
        // reads to make that call — an author summoned after convergence is asking to move the head,
        // which re-arms the review half and re-opens the budget.
        let rows = self.store().load_live_review_watch().unwrap_or_default();
        charged
            .iter()
            .any(|pr| self.rounds_used(pr) >= threshold && !converged(&rows, pr))
    }

    /// The configured adjudication threshold, or `None` when adjudication is off (STUDIO-956).
    fn adjudication_threshold(&self) -> Option<usize> {
        self.teams
            .as_ref()
            .and_then(|t| t.review_adjudicate_after_rounds())
    }

    /// [`Self::adjudication_threshold`] for tests in sibling modules — the reconciliation sweep's
    /// budget-copy test asserts that its fixture really is an install with no threshold, which is
    /// the whole premise of the sentence it pins.
    #[cfg(test)]
    pub(crate) fn adjudication_threshold_for_test(&self) -> Option<usize> {
        self.adjudication_threshold()
    }

    /// What the manager has decided (or is deciding) about `pr`, if anything (STUDIO-956).
    pub(crate) fn adjudication(
        &self,
        pr: &PrCoord,
    ) -> Option<crate::reviewadjudicate::Adjudication> {
        self.adjudication_ledger.as_ref().and_then(|l| l.peek(pr))
    }

    /// How many review↔author ROUNDS `pr` has run — the dispatch counter in
    /// [`REVIEW_ROUNDS_PER_PR_CAP`]'s unit, so the configured threshold and the hard cap are the
    /// same number of rounds. Under the threshold both sides charge this counter; unset, only
    /// reviews do.
    fn rounds_used(&self, pr: &PrCoord) -> usize {
        self.review_rounds.get(&churn_key(pr)).copied().unwrap_or(0) / self.reviewers_per_round()
    }

    /// Writes `key`'s round counter through to `rhapsody_review_bound`, so the bound survives the
    /// restart that used to refund it (STUDIO-956). Call it after EVERY change to
    /// [`Orchestrator::review_rounds`] that is not a wholesale forget (which is
    /// [`Orchestrator::forget_review_bound`]).
    ///
    /// The in-memory figure is what is written, not an increment: the counter has exactly one
    /// writer — the control task — and it is rehydrated at boot, so memory is authoritative and a
    /// dropped write is repaired by the next charge rather than compounding.
    ///
    /// A store error is a WARN and nothing else. Persistence is best-effort everywhere in this
    /// daemon, and the alternative — refusing to charge a round the store could not record — would
    /// turn a disk problem into an unbounded review loop, which is the failure this ticket exists
    /// to end.
    pub(crate) fn persist_review_rounds(&self, key: &str) {
        let spent = self.review_rounds.get(key).copied().unwrap_or(0);
        if let Err(e) = self.store().set_review_rounds(key, spent as i64) {
            tracing::warn!(
                pr = %key, err = %e,
                "ticketless review: the round counter could not be persisted; this pull request's \
                 bound is per-boot until a later charge writes it"
            );
        }
    }

    /// Deletes everything durable about `pr` — the counter AND the manager's decision — for a pull
    /// request that has left the watch set or that an operator has deliberately cleared
    /// (STUDIO-956). The durability trap the ticket names: a bound that outlived its pull request
    /// would hand a rebuilt or reopened one a spent budget it never earned.
    pub(crate) fn forget_review_bound(&self, pr: &PrCoord) {
        if let Err(e) = self.store().clear_review_bound(&churn_key(pr)) {
            tracing::warn!(pr = %pr, err = %e, "ticketless review: the durable round bound could not be cleared");
        }
    }

    /// Rebuilds the per-pull-request review bounds from the store at boot — the round counters into
    /// [`Orchestrator::review_rounds`] and the manager's settled decisions into the adjudication
    /// ledger (STUDIO-956). Called by [`Orchestrator::boot_recovery`], before the first tick.
    ///
    /// Only SETTLED decisions come back: an in-flight marker is never persisted (see
    /// [`crate::reviewadjudicate::Adjudication`]), so an adjudication a restart interrupted is
    /// simply re-asked rather than left stopping the loop forever with no turn anywhere to land it.
    ///
    /// Best-effort, like every other step of boot recovery: a failed read is logged and the daemon
    /// starts with the per-boot behaviour it had before this ticket rather than refusing to boot.
    pub(crate) fn rehydrate_review_bounds(&mut self) {
        let rows = match self.store().load_review_bounds() {
            Ok(rows) => rows,
            Err(e) => {
                tracing::error!(error = %e, "recovery: the review round bounds could not be read; the bound is per-boot this lifetime");
                return;
            }
        };
        let (mut counters, mut decisions) = (0usize, 0usize);
        for row in rows {
            if row.dispatches > 0 {
                // `row.pr` IS `churn_key`'s spelling — it is what wrote the row — so no second
                // place derives the key and the two can never disagree about what one budget is.
                self.review_rounds
                    .insert(row.pr.clone(), row.dispatches as usize);
                counters += 1;
            }
            if let Some(stored) = row.adjudication.as_ref()
                && let Some(ledger) = self.adjudication_ledger.as_ref()
                && let Some(decision) = crate::reviewadjudicate::Adjudication::from_stored(stored)
            {
                ledger.seed(&row.pr, decision);
                decisions += 1;
            }
        }
        if counters > 0 || decisions > 0 {
            tracing::info!(
                counters,
                decisions,
                "recovery: rehydrated the review round bounds; a restart no longer refunds a spent \
                 budget or forgets a manager decision"
            );
        }
    }

    /// The decision-relevant open facts at `head`, one human-readable line per live row. Named on an
    /// escalation, which must carry the specific findings rather than "needs a human" (STUDIO-956).
    ///
    /// **Not "the verdicts at this exact head".** The row's status is transient: a head advance
    /// re-arms `reviewed` to `requested` (preserving `last_reviewed_sha`) and an unfinished round
    /// parks at `truncated`, so at the instant the loop reaches its threshold both halves of a
    /// `status == reviewed && last_reviewed_sha == head` filter can fail at once and the plan would
    /// carry no findings at all. That is the rule rather than the exception on an EVEN threshold,
    /// which the author's own summoned dispatch is what crosses. Three shapes are named instead:
    ///
    /// * a row that posted findings at the current head — `{reviewer} asked for changes at {head}`;
    /// * a row whose last read predates the head — the author has pushed since and nobody has read
    ///   the new head, which is the single most decision-relevant fact available here and is stated
    ///   verdict-neutrally because the re-arm preserved the SHA but not whether it was findings or
    ///   an approval;
    /// * a `truncated` row — the round was attempted and never finished, so it posted nothing.
    ///
    /// Skipped are the rows with nothing to say: one approved at the current head, and one that has
    /// never been reviewed at all and is not in the unfinished-round state.
    fn open_findings(&self, mine: &[&ReviewWatchRow], head: &str) -> Vec<String> {
        let mut findings = Vec::new();
        for r in mine
            .iter()
            .filter(|r| r.open && r.status != REVIEW_STATUS_DROPPED)
        {
            if r.last_reviewed_sha == head {
                if r.status == REVIEW_STATUS_REVIEWED {
                    findings.push(format!(
                        "{} asked for changes at {}",
                        r.key.reviewer,
                        short_sha(head)
                    ));
                }
                // An `approved` row at this head is the only genuinely closed one; every other
                // status here (`truncated` after a completed read of the same head, say) falls
                // through to the unfinished-round arm below.
            } else if !r.last_reviewed_sha.is_empty() {
                findings.push(format!(
                    "{} last reviewed {}; the author has pushed {} since and no reviewer has read it",
                    r.key.reviewer,
                    short_sha(&r.last_reviewed_sha),
                    short_sha(head)
                ));
                continue;
            }
            if r.status == REVIEW_STATUS_TRUNCATED {
                let attempted = if r.requested_sha.is_empty() {
                    head
                } else {
                    &r.requested_sha
                };
                findings.push(format!(
                    "{}'s review of {} did not finish; no findings were posted",
                    r.key.reviewer,
                    short_sha(attempted)
                ));
            }
        }
        findings
    }

    /// Whether any half of `pr`'s loop — a review round OR the author's summoned run — is live
    /// right now.
    ///
    /// A decision must not be made over a round mid-flight: new findings could still land, and a fix
    /// the author is actively writing is about to supersede the head the manager would decide
    /// against. The author half is easy to miss because the counter is charged at DISPATCH
    /// ([`crate::retry`]), so an author run is in flight from the very instant its charge lands —
    /// and with the loop alternating review→author, any EVEN threshold is crossed by the author's
    /// own dispatch. [`reconcile_pr`](crate::reviewreconcile::reconcile_pr) already treats an
    /// in-flight run as activity that silences the whole pull request; this is the same rule on the
    /// decision path.
    fn review_round_in_flight(&self, mine: &[&ReviewWatchRow]) -> bool {
        let review_live = mine.iter().any(|r| {
            let id = review_key(&r.key.owner, &r.key.repo, r.key.number, &r.key.reviewer);
            self.running.contains_key(&id) || self.claimed.contains(&id)
        });
        review_live
            || mine.iter().any(|r| {
                crate::reviewdone::origin_ticket(&r.introduced_by)
                    .is_some_and(|ticket| self.author_run_live(ticket))
            })
    }

    /// Whether `pr` still owes its ONE resumed round at `head` (STUDIO-971).
    ///
    /// Past the adjudication threshold a content-changing head move buys exactly one review round;
    /// this answers "is that round still owed?", so the fresh adjudication is held back until it is
    /// spent. It is decided from the ROWS — the round is owed while ANY live row still owes a review
    /// of `head`, the same [`review_round_due`] the dispatch loop itself uses.
    ///
    /// Deliberately NOT `rounds_used(pr) <= decision.rounds()`. A round costs one dispatch per live
    /// reviewer, and `rounds_used` is the floor of the dispatch counter divided by
    /// `review.reviewers`. When a pull request has fewer live rows than that configured count — a
    /// smaller eligible roster, a retired or unassignable row — the resumed round does not carry the
    /// counter across the next whole multiple, so the floor comparison keeps reporting the round
    /// owed forever, the threshold branch is never reached, the stale `ship` is never cleared, and
    /// the pull request stalls exactly as it did before this ticket, one round later. Reading the
    /// rows answers the question the counter was a proxy for.
    ///
    /// **A round the per-pull-request hard cap will refuse is not owed.** Once
    /// `dispatches >= REVIEW_ROUNDS_PER_PR_CAP * reviewers_per_round` the dispatch loop `continue`s
    /// past every row while leaving it `requested`, so `review_round_due` stays true for good and the
    /// rows would report the round owed forever — the same stall, at the cap. The cap is permanent
    /// until a restart, an operator Clear or the pull request closing, so such a round can never be
    /// dispatched and must fall through to a fresh adjudication instead. The threshold branch's
    /// `rounds_used(pr) >= threshold` is satisfied by definition at the cap.
    ///
    /// A row deferred for a reason that CAN change — no eligible reviewer this sweep, capacity, a
    /// human hold, a drain — still owes its round, and some later sweep can arm it. An unassignable
    /// row is the one such deferral that can persist; it is reported as `stalled` by
    /// [`Self::note_unassignable`] rather than silently, and the round genuinely has not happened.
    fn resumed_round_owed(&self, pr: &PrCoord, mine: &[&ReviewWatchRow], head: &str) -> bool {
        if self.round_budget_spent(pr) {
            return false;
        }
        mine.iter().any(|r| {
            let id = review_key(&r.key.owner, &r.key.repo, r.key.number, &r.key.reviewer);
            let live = self.running.contains_key(&id) || self.claimed.contains(&id);
            review_round_due(r, head, live)
        })
    }

    /// Whether the ticket `identifier` — a watched pull request's author — has a live run.
    ///
    /// The author's run is a normal ticket run, so it is keyed by the tracker's opaque ID rather
    /// than by the identifier this reads; `RunningEntry` carries its own `Issue` and is the one
    /// place the two are available together.
    ///
    /// A run parked in BACKOFF is live work too, and it is `claimed` without being `running` for
    /// the whole backoff delay. `schedule_retry_for` records its `RetryEntry` (which carries the
    /// identifier) at the same instant it claims the id, so reading `retry_attempts` covers that
    /// window — without it a flaky agent's mid-loop retry would be decided over, the same harm as
    /// deciding over a running fix. `LoadSnapshot::from_running_and_retries` counts the state
    /// against its owner for exactly this reason.
    fn author_run_live(&self, identifier: &str) -> bool {
        self.running
            .values()
            .any(|entry| entry.issue.identifier == identifier)
            || self
                .retry_attempts
                .values()
                .any(|entry| entry.identifier == identifier)
    }

    /// Charges one AUTHOR round to every linked pull request of `iss` that already carries a budget,
    /// so the author half of the loop counts toward the adjudication threshold (STUDIO-956).
    ///
    /// **A no-op unless the threshold is set.** Under an unset threshold the counter bounds REVIEW
    /// rounds only, and charging author runs to it would change what a default install does — the
    /// byte-identical property the revised ticket's last ⚠️ requires. Also a no-op for a ticket
    /// whose pull requests have never been reviewed, so an ordinary first dispatch is free.
    ///
    /// A ROUND rather than a dispatch, matching [`REVIEW_ROUNDS_PER_PR_CAP`]'s unit: the author's
    /// run its review's findings bought is the loop's other half, so it costs the same as the review
    /// round did at any reviewer count.
    pub(crate) fn note_author_round(&mut self, iss: &Issue) {
        if self.adjudication_threshold().is_none() {
            return;
        }
        let round = self.reviewers_per_round();
        for pr in self.charged_linked_prs(iss) {
            let key = churn_key(&pr);
            *self.review_rounds.entry(key.clone()).or_default() += round;
            self.persist_review_rounds(&key);
        }
    }

    /// Records that `id`'s round found nobody this sweep, and answers whether that has now been
    /// true for [`REVIEW_UNASSIGNABLE_SWEEPS`] consecutive sweeps.
    ///
    /// The log follows [`crate::drain`]'s discipline exactly: the sweep that CROSSES the threshold
    /// says so loudly and once, and the steady state repeats at [`REVIEW_UNASSIGNABLE_LOG_EVERY`]
    /// rather than at poll rate — a stalled round would otherwise emit a warning every 30 seconds
    /// for as long as the pull request stays open, which is how an operator learns to filter the
    /// one line that mattered.
    fn note_unassignable(&mut self, pr: &PrCoord, id: &str, reviewer: &str) -> bool {
        let sweeps = self.review_unassignable.entry(id.to_string()).or_insert(0);
        *sweeps += 1;
        let sweeps = *sweeps;
        if sweeps < REVIEW_UNASSIGNABLE_SWEEPS {
            return false;
        }
        // The crossing sweep and the rate-limited repeats in ONE condition: at the crossing the
        // difference is zero, and zero is a multiple of everything. Spelling the crossing out as a
        // separate disjunct would read as a second rule while deciding nothing.
        if (sweeps - REVIEW_UNASSIGNABLE_SWEEPS).is_multiple_of(REVIEW_UNASSIGNABLE_LOG_EVERY) {
            tracing::warn!(
                pr = %pr, reviewer, sweeps,
                "ticketless review: this round has had no eligible reviewer for {sweeps} \
                 consecutive sweeps and will not resolve on its own — every teammate left is \
                 either the pull request's author or already holds one of its reviews. Add a \
                 teammate to `teams.yaml`, or lower `review.reviewers`."
            );
        }
        true
    }

    /// Forgets `id`'s consecutive-deferral count, logging the RECOVERY when there was a reported
    /// stall to recover from — the edge [`note_unassignable`](Self::note_unassignable) is the other
    /// half of. A round that never reached the threshold clears silently: it was never news.
    fn clear_unassignable(&mut self, pr: &PrCoord, id: &str, reviewer: &str) {
        if let Some(sweeps) = self.review_unassignable.remove(id)
            && sweeps >= REVIEW_UNASSIGNABLE_SWEEPS
        {
            tracing::info!(
                pr = %pr, reviewer, sweeps,
                "ticketless review: a round that had been unassignable for {sweeps} sweeps has a \
                 reviewer again"
            );
        }
    }

    /// Whether any watched round is currently stalled — read by `project_statuses` to surface
    /// [`REVIEW_UNASSIGNABLE_WARNING`] (STUDIO-891).
    pub(crate) fn review_rounds_stalled(&self) -> bool {
        self.review_unassignable
            .values()
            .any(|n| *n >= REVIEW_UNASSIGNABLE_SWEEPS)
    }

    /// Drops every live row of one pull request out of the watch set and forgets its churn budget.
    /// Returns how many rows were dropped.
    fn retire_review_pr(&mut self, pr: &PrCoord, why: &str) -> usize {
        // The FULL set on purpose, unlike the paths above: this predicate also catches a row that
        // is closed but not yet dropped, which `load_live_review_watch` filters out.
        let rows = match self.store().load_review_watch() {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(pr = %pr, err = %e, "ticketless review: the watch set could not be read; nothing was dropped");
                return 0;
            }
        };
        let mut dropped = 0usize;
        // Collected as the rows go, rather than reconstructed by prefix-matching `review_key`'s
        // format from here: the key's spelling is that function's business, and a second place
        // that knows it is a second place for it to drift.
        let mut retired_ids: Vec<String> = Vec::new();
        for row in rows {
            if !row_is(&row, pr) || (!row.open && row.status == REVIEW_STATUS_DROPPED) {
                continue;
            }
            retired_ids.push(review_key(
                &row.key.owner,
                &row.key.repo,
                row.key.number,
                &row.key.reviewer,
            ));
            match self.store().drop_review_watch(&row.key) {
                Ok(()) => dropped += 1,
                Err(e) => {
                    tracing::warn!(review = %review_key(&row.key.owner, &row.key.repo, row.key.number, &row.key.reviewer), err = %e, "ticketless review: dropping the watch row failed")
                }
            }
        }
        if dropped > 0 {
            tracing::info!(pr = %pr, reason = why, rows = dropped, "ticketless review: pull request dropped from the watch set");
        }
        // A dropped pull request can be re-introduced later; its old churn budget should not follow
        // it, and leaving the entry would grow this map for the daemon's whole life. The
        // consecutive-deferral counts go with it for both of those reasons (STUDIO-891) — and for a
        // third: a retired pull request that kept a stall count would keep the operator advisory
        // lit for a review nobody is waiting on any more.
        self.review_rounds.remove(&churn_key(pr));
        // Durably too (STUDIO-956) — counter and decision in one delete, so a pull request that is
        // rebuilt or reopened under the same number starts from zero rather than inheriting a
        // budget the pull request it replaced had spent.
        self.forget_review_bound(pr);
        // And the manager's adjudication of it (STUDIO-956), for the same reasons: a re-introduced
        // pull request must be adjudicated afresh, and an entry for a gone pull request would keep
        // a divergence reported for a review nobody is waiting on any more.
        if let Some(ledger) = self.adjudication_ledger.as_ref() {
            ledger.clear(pr);
        }
        // And what was announced about its auto-merge plan, for the first two of those reasons.
        self.auto_merge_announced.remove(&churn_key(pr));
        // And its conflict route-back record (STUDIO-961): a retired pull request has no ticket to
        // route back, and leaving the entry would keep the reconciliation sweep silent about a
        // coordinate that is watched again later.
        self.conflict_routed.remove(pr);
        // And the draft-poke bookkeeping (STUDIO-962): a re-introduced pull request must be poked
        // afresh, and an entry for a gone one would be a map that only ever grows.
        self.draft_pokes.remove(&churn_key(pr));
        // The failure record goes too (STUDIO-950 round 14): keyed by coordinate, it would otherwise
        // outlive the pull request it names and sit in the map for the daemon's whole life. It is
        // part of this function's own contract to forget EVERYTHING about the coordinate, even
        // though the sweep's success-clear loop has usually removed it already (a `Gone` lookup is
        // still an ANSWER); relying on that caller's ordering would make this function silently
        // incomplete if the loop were ever reordered.
        self.review_watch_unreadable.remove(pr);
        for id in retired_ids {
            self.review_unassignable.remove(&id);
            // A round cannot be held for capacity once its pull request has left the watch set
            // (STUDIO-950): the hold's whole job is to annotate the reconciliation sweep's report of
            // an OWED round, and a retired pull request no longer owes one.
            self.review_capacity_held.remove(&id);
        }
        dropped
    }

    /// Services one OPEN pull request at `head`: re-arms whatever the advance re-armed, then
    /// dispatches a review round for every row that still owes one.
    fn service_review_pr(
        &mut self,
        rows: &[ReviewWatchRow],
        pr: &PrCoord,
        observed: ObservedHead<'_>,
        slots: &mut i64,
        report: &mut ReviewSweepReport,
    ) {
        let ObservedHead {
            head,
            merge_state,
            unchanged_from,
        } = observed;
        if head.is_empty() {
            return; // an answer with no head is not an answer about a head
        }
        // The design's in-process re-review signal (§14.1 F-SEC's fix for the room post §13.1 had):
        // rows whose head has moved past what they recorded are parked back at `requested`, so the
        // console and the room read the same fact the dispatch below acts on. It can only ever
        // touch rows that already exist, and it changes no field this function's decision reads —
        // `review_round_due` answers identically before and after it — which is why `rows` (loaded
        // fresh for this observation's hand-back) is still sound to decide from.
        let advance = self.handle_review_head_advanced(pr, head, unchanged_from);
        report.armed += advance.armed;
        report.skipped += advance.skipped.len();

        // How many dispatches one ROUND of this pull request costs — the unit the churn budget
        // below has to be expressed in. Read from config rather than from `mine.len()`, which is
        // the rows that happen to exist right now and would let a retired row shrink the budget.
        let reviewers_per_round = self.reviewers_per_round();

        let mine: Vec<&ReviewWatchRow> = rows.iter().filter(|r| row_is(r, pr)).collect();
        // STUDIO-950: this pull request is being re-evaluated NOW, so a hold recorded for it on a
        // PREVIOUS tick is refuted — a round that is dispatchable again, or deferred for another
        // reason, must stop annotating the reconciliation sweep. Any round this tick still defers
        // re-records its hold in the capacity branch below. Applied only to THIS pull request's
        // rows: a pull request this tick's cursor did not reach keeps its hold, because not being
        // re-evaluated is not evidence the hold ended (see the module-level comment on the sweep).
        for r in &mine {
            self.review_capacity_held.remove(&review_key(
                &r.key.owner,
                &r.key.repo,
                r.key.number,
                &r.key.reviewer,
            ));
        }
        // The CURRENT-LABEL set (STUDIO-949), lowercased for the case-insensitive comparison against
        // a row's origin ticket below. This is `labelled()`, not the console's `held()`: the reported
        // hold excludes a ticket the daemon is running, but this gate must also catch an origin
        // labelled while its run was still LIVE — the mid-run hold shape. Empty on any daemon with no
        // hold, which is what keeps the default path paying only a clone of an empty set.
        //
        // Read together with the priming latch, under one lock (STUDIO-949 round 13): read
        // separately, a selection pass landing between the two calls would let a gate hold an
        // un-primed empty set and then read `primed == true`, treating "nothing has looked" as "no
        // hold". The pair is now always the pair one pass produced.
        let (held, ledger_primed) = self.human_holds.labelled_and_primed();
        // Whether THIS pull request's origin ticket currently wears the `rhapsody:human` label
        // (STUDIO-949 round 5). Bound HERE, beside the label set it reads, because two gates need
        // the same answer: the conflict route-back immediately below, and the auto-merge gate at the
        // tail of this function. Both are decided before their plan is formed, so nothing is handed
        // across the seam.
        //
        // For auto-merge: the round gate below refuses to DISPATCH against a held ticket, but a pull
        // request whose reviewers had already approved the current head when the label landed would
        // still clear auto-merge — and the merge then runs `plan_review_done` on the next tick and
        // moves the ticket to `review.done_state`. Refusing the review round while MERGING the code
        // and closing the ticket is the daemon finishing work the label says only a person can do,
        // and the merge is irreversible. Read from the same current-LABEL set as the round gate
        // (`labelled()`, live runs included).
        //
        // MUTATION: delete this gate and `a_held_origin_ticket_holds_back_auto_merge` /
        // `a_held_origin_ticket_routes_no_conflict_back` red (a plan is proposed).
        let held_origin = mine.iter().any(|row| {
            crate::reviewdone::origin_ticket(&row.introduced_by)
                .is_some_and(|t| held.contains(&t.to_ascii_lowercase()))
        });
        // The conflict route-back (STUDIO-961), behind STUDIO-949's two gates and deliberately ABOVE
        // the adjudication block rather than at the tail beside `propose_auto_merge`.
        //
        // BEHIND THE GATES because a route-back moves tracker state AND reopens the author's agent
        // run through the summon token — precisely the class of action `rhapsody:human` exists to
        // refuse. A conflict on a human-held ticket is a human's to resolve; the daemon must not
        // summon an agent back onto it, and moving the ticket out of the review state would undo a
        // placement a person made. The fail-closed `ledger_primed` half is the auto-merge gate's own
        // reasoning verbatim: on a daemon held by a bad config, an armed drain or a dead credential
        // no selection pass ever runs, so an empty label set is "unknown", not "no hold" — and this
        // sweep runs from the watcher's own task, independent of those gates.
        //
        // ABOVE THE ADJUDICATION BLOCK because STUDIO-956's four `return`s precede the tail of this
        // function. At the tail, a pull request that has reached `review.adjudicate_after_rounds`
        // would never route back on a conflict again — and a `ship` adjudication of a conflicted
        // head is exactly the nine-hour stall STUDIO-961 was filed for: the manager decides the
        // FINDINGS question, `propose_auto_merge` hands out a plan, and GitHub declines the merge
        // every time with nobody handing the branch back. Mergeability is orthogonal to the review
        // verdict, so its trigger must be too. Placing it here costs nothing: `mine` is this
        // hand-back's opening snapshot and neither the adjudication block nor the dispatch loop
        // writes any state this decision reads, so its answer is the same at either end.
        //
        // MUTATION: move this call to the tail's `else` branch and
        // `a_conflict_past_the_adjudication_threshold_still_routes_back` reds (nothing routes).
        if ledger_primed && !held_origin {
            self.propose_conflict_route_back(&mine, pr, head, merge_state, report);
        } else if merge_state == MERGE_STATE_DIRTY {
            tracing::debug!(
                pr = %pr, ledger_primed, held_origin,
                "ticketless review: this pull request is conflicted, but its origin ticket is held \
                 for a human (or no selection pass has read the board yet); the conflict is a \
                 human's to resolve"
            );
        }
        // Who currently holds each of this pull request's required reviews, updated AS the loop
        // reassigns. `mine` is this hand-back's opening snapshot, so reading peers off it directly
        // would go stale the moment one row is reassigned: the next row would still see the retired
        // reviewer as a peer and not see the substitute, and could hand that substitute a second
        // required review of the same pull request.
        let mut assigned: Vec<String> = mine.iter().map(|r| r.key.reviewer.clone()).collect();

        // STUDIO-956: at the configured round threshold the loop stops ARMING and the MANAGER
        // decides — ship it, or escalate — instead of the loop silently stopping at the hard cap.
        // Checked before the dispatch loop so no row of this pull request is dispatched once the
        // threshold is reached — except the ONE resumed round STUDIO-971 grants a new head below.
        //
        // STUDIO-971: a decision applies to the HEAD it was made at, never to the pull request for
        // ever. A route-back after a `ship` adjudication is a NORMAL flow — the threshold fires on
        // exactly the pull requests that are churning — so a push since the decision used to leave
        // the loop stopped at a head that no longer existed, and the pull request could neither be
        // reviewed nor merged. The maintained policy: past the threshold each new head buys exactly
        // ONE round. The decision stops governing, the loop arms a single round at the new head,
        // and a round that returns findings buys a FRESH adjudication there. The durable count is
        // never reset, so it still shows the churn.
        if !mine.is_empty()
            && let Some(threshold) = self.adjudication_threshold()
        {
            // Whether this head still owes its one resumed round: set only by a settled `ship`
            // whose head a content-changing push has moved past and whose one round is not yet
            // spent. It suppresses the fresh adjudication below so the round is armed instead —
            // exactly once.
            let mut resumed_round = false;
            if let Some(decision) = self.adjudication(pr) {
                // A turn is out right now: arm nothing and ask nothing. This is also what keeps an
                // escalation from ever resuming — an in-flight marker never reaches the resume path.
                if !decision.settled() {
                    report.deferred += 1;
                    self.propose_auto_merge(&mine, pr, head, merge_state, report);
                    return;
                }
                // The gates keep their say either way: a `ship` verdict adjudicates the open
                // findings, never CI, approval-at-head, a draft, a conflict, or any other merge
                // gate. A decision that still describes this head — a `ship` at it, a no-op rebase
                // it survives, or any `escalate` — stops the loop here.
                if decision.governs(head, unchanged_from) {
                    self.propose_auto_merge(&mine, pr, head, merge_state, report);
                    return;
                }
                // A settled `ship` at a head this content-changing push has moved past. Past the
                // threshold the new head buys exactly one round. Whether that one round is still
                // owed is answered from the ROWS ([`Self::resumed_round_owed`]) and NOT from
                // `rounds_used`: a round dispatches one row per live reviewer, and when fewer rows
                // than `review.reviewers` exist the floor-divided counter never reaches the next
                // whole multiple, so a counter comparison would consider the round owed forever and
                // reproduce this ticket's stall one round later. Once the round is spent the fresh
                // adjudication below takes over — and a round that came back with findings is
                // exactly what re-adjudicates here.
                resumed_round = self.resumed_round_owed(pr, &mine, head);
            }
            if !resumed_round && self.rounds_used(pr) >= threshold {
                // A pull request that CONVERGED on its last allowed round is not a failure for the
                // manager to decide. `auto_merge_verdict` is the head-exact "every live row approved
                // at this head" predicate the merge gate already uses; `is_ok()` is the convergence
                // question. Sending a converged pull request to the manager would ask it to decide a
                // loop that already did — on a prompt that asserts it did NOT converge and names no
                // findings — and an `ESCALATE` answer would post a false alarm and freeze the author
                // half for a pull request every reviewer approved. Let it fall to the ordinary
                // auto-merge path below, which re-applies every gate.
                if crate::automerge::auto_merge_verdict(&mine, head).is_ok() {
                    self.propose_auto_merge(&mine, pr, head, merge_state, report);
                    return;
                }
                // Never decide over a round mid-flight: findings could still land, and the author's
                // own fix may be about to supersede the head this would decide against.
                if self.review_round_in_flight(&mine) {
                    report.deferred += 1;
                    self.propose_auto_merge(&mine, pr, head, merge_state, report);
                    return;
                }
                let rounds = self.rounds_used(pr);
                let findings = self.open_findings(&mine, head);
                // A turn that has failed its bounded attempts ESCALATES rather than being re-asked,
                // but that escalation is recorded where its two audit writes happen — off the
                // control task, in `reviewadjudicate::perform_adjudication`. The settled entry it
                // lands there is what this branch reads back on the next sweep (the
                // `self.adjudication(pr)` check above) to stop handing out plans, so the bound is
                // enforced without a second, comment-less escalation path here.
                let plan = crate::reviewadjudicate::ReviewAdjudicationPlan {
                    pr: pr.clone(),
                    head: head.to_string(),
                    rounds,
                    findings,
                };
                if let Some(ledger) = self.adjudication_ledger.as_ref() {
                    // A decision recorded at a head this one has moved past no longer governs: it
                    // was the reason the resume above was considered, and `mark_in_flight`'s
                    // `or_insert` would otherwise let it swallow the fresh plan. Forget it — the
                    // round COUNT beside it in the same durable row is deliberately left alone,
                    // because the count is "how much has been spent on this pull request" and the
                    // decision is "what the manager concluded about one specific head". This is the
                    // inverse of a `record`, and it clears exactly one half of the durable row; it
                    // also drops any in-memory failure tally for the pull request, which is the
                    // right lifetime for a tally that only bounds the re-asking of THIS decision.
                    if self.adjudication(pr).is_some() {
                        ledger.clear(pr);
                    }
                    // Marks it in flight so the next tick does not hand out a second plan while the
                    // manager is still deciding.
                    ledger.mark_in_flight(pr, rounds);
                }
                report.adjudicate.push(plan);
                report.deferred += 1;
                self.propose_auto_merge(&mine, pr, head, merge_state, report);
                return;
            }
        }

        for (idx, row) in mine.iter().enumerate() {
            // A row whose verdict was just carried across an unchanged head move (STUDIO-960). The
            // advance above wrote the NEW head into its `last_reviewed_sha`, but `rows` is this
            // hand-back's opening snapshot and still holds the old one, so `review_round_due` below
            // would report the round due again. Skipping it here is what keeps the dispatch loop
            // reading the state the store now holds; `mine` is left whole so the auto-merge gate
            // still sees this row's verdict.
            if advance.skipped.iter().any(|k| k == &row.key) {
                continue;
            }
            let id = review_key(
                &row.key.owner,
                &row.key.repo,
                row.key.number,
                &row.key.reviewer,
            );
            let live = self.running.contains_key(&id) || self.claimed.contains(&id);
            if !review_round_due(row, head, live) {
                continue;
            }
            // A `rhapsody:human` origin ticket is refused at dispatch on every path (STUDIO-949), and
            // this watcher is a dispatch path. The gate is deliberately on the ROW'S CURRENT HOLD
            // rather than on the row's creation: a ticket labelled after an agent already flailed on
            // it is the likeliest way the label is ever applied, and that ticket has a watch row from
            // the earlier round. The row is LEFT ARMED — the hold can come off, and the obligation it
            // records is still real when it does — so this defers rather than retires, and unlike the
            // `requested` back-pressure above it is a deliberate hold, not a budget.
            //
            // Fail CLOSED while the ledger has never been primed (STUDIO-949 round 13). The
            // current-label set has no writer above `on_tick`'s three early-return gates, so on a
            // daemon held by a bad config, an armed drain or a dead credential it is empty for the
            // WHOLE process lifetime — and this sweep runs from the watcher's own 120s task,
            // independent of those gates. `dispatch_review` gates on a drain but on neither of the
            // other two, so reading an unknown empty set as "no hold" here dispatches a REAL review
            // round, for the whole life of the gate, at a held ticket's pull request. An empty set
            // with no pass having looked is "unknown", not "no hold".
            //
            // MUTATION: drop this fail-closed branch and
            // `an_unprimed_hold_ledger_refuses_the_review_round` reds (a round is dispatched).
            if !ledger_primed {
                tracing::debug!(
                    pr = %pr, reviewer = %row.key.reviewer,
                    "ticketless review: no selection pass has run yet, so the human-hold label set \
                     is unknown; the round waits"
                );
                report.deferred += 1;
                continue;
            }
            let origin = crate::reviewdone::origin_ticket(&row.introduced_by)
                .map(|t| t.to_ascii_lowercase());
            if origin.as_deref().is_some_and(|t| held.contains(t)) {
                tracing::debug!(
                    pr = %pr, reviewer = %row.key.reviewer, origin = %row.introduced_by,
                    "ticketless review: the origin ticket is held for a human; the round waits"
                );
                report.deferred += 1;
                continue;
            }
            if *slots <= 0 {
                // STUDIO-950: remember WHY this round is deferred, so the reconciliation sweep can
                // report it as a deliberate capacity hold rather than the unexplained stall it
                // would otherwise re-derive. The count is the runs holding the ACTIVE pool — every
                // running run in shared mode, the ticketless reviews once the key gives them their
                // own — so the log a human reads when tuning the key names what actually spent it
                // rather than always the reviews. The record is timestamped so a hold from a sweep
                // that has since stopped happening (a `gh` outage) cannot outlive its freshness.
                let holding = self.review_pool_holders();
                let separate = self
                    .eff
                    .as_ref()
                    .is_some_and(|e| e.max_concurrent_reviews.is_some());
                let recorded = (self.now)();
                self.review_capacity_held.insert(
                    id.clone(),
                    CapacityHold {
                        holders: holding,
                        separate,
                        recorded,
                    },
                );
                tracing::debug!(
                    pr = %pr, holding,
                    "ticketless review: the daemon-wide concurrency budget is spent; this round is \
                     re-considered next tick"
                );
                report.deferred += 1;
                continue;
            }
            // The churn floor (§14.2). Keyed per PULL REQUEST rather than per row so N reviewers of
            // one pull request share one budget — the cost this bounds is agent runs, not rows.
            //
            // The counter is in dispatches and the cap is in rounds, so the budget is scaled by the
            // required-reviewer count to make the two comparable (STUDIO-727). Without that, a
            // two-reviewer config would get four rounds and an eight-reviewer config exactly one.
            let dispatched = self.review_rounds.get(&churn_key(pr)).copied().unwrap_or(0);
            let budget = REVIEW_ROUNDS_PER_PR_CAP.saturating_mul(reviewers_per_round);
            if dispatched >= budget {
                tracing::debug!(
                    pr = %pr, dispatched, budget,
                    "ticketless review: the per-pull-request re-review cap is reached; no further \
                     round is dispatched until the daemon restarts or the pull request closes"
                );
                report.deferred += 1;
                continue;
            }
            // Live, not `quorum_load` — which is always empty on this path (§14.2, the load
            // finding). Rebuilt per round on purpose: a review dispatched a moment ago is already
            // in `running`, so the SECOND round of this tick sees the first one's load and picks
            // somebody else.
            let load = LoadSnapshot::from_running(&self.running);
            let peers: HashSet<String> = assigned
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != idx)
                .map(|(_, name)| name.clone())
                .collect();
            let Some(chosen) = self.choose_review_reviewer(row, &peers, &load) else {
                tracing::debug!(
                    pr = %pr, reviewer = %row.key.reviewer,
                    "ticketless review: no teammate is eligible to review this pull request; \
                     re-considered next tick"
                );
                report.deferred += 1;
                // STUDIO-891: "re-considered next tick" is only reassuring while some tick can
                // change the answer. Count the consecutive ones so a round that no tick will ever
                // resolve stops presenting as ordinary back-pressure.
                report.stalled += usize::from(self.note_unassignable(pr, &id, &row.key.reviewer));
                continue;
            };
            // THE dispatch-side allowlist re-check (the slice-6 F-SEC review's item (a)). The row
            // is stored state, and a project can be disabled or repointed by a config reload
            // between introduction and now; trusting the row would let a review be dispatched
            // against a repository no configured project owns any more. Fails closed.
            let Some(repo_url) = self.review_repo_url(&row.key.owner, &row.key.repo) else {
                tracing::warn!(
                    pr = %pr,
                    "ticketless review: refusing to dispatch a review in a repository no enabled \
                     project owns; the watch row is left alone"
                );
                report.deferred += 1;
                continue;
            };
            // The recovery edge (STUDIO-891): this round found somebody, so whatever it had been
            // owed before is settled. Cleared BEFORE the dispatch, because a dispatch that fails
            // further down is a different problem with its own log line, and leaving the count
            // standing would let this row keep claiming a stall it no longer has.
            self.clear_unassignable(pr, &id, &row.key.reviewer);
            let reassigned = chosen != row.key.reviewer;
            let picked = chosen.clone();
            let run = ReviewRun {
                owner: row.key.owner.clone(),
                repo: row.key.repo.clone(),
                number: row.key.number,
                reviewer: chosen,
                author: row.author.clone(),
                // A `pr:` key resolves to no tracker ticket, so there is no team to move it in and
                // nothing to carry one for. Left empty deliberately: a non-empty `team_id` is what
                // would make the worker's hand-off auto-park call `move_issue_state("pr:…")`, a
                // guaranteed 404 on every review (design §14.2, "team_id is a red herring").
                team_id: String::new(),
                repo_url,
                head_sha: head.to_string(),
                introduced_by: row.introduced_by.clone(),
                // Empty at DISPATCH: `dispatch_review` fills it from the watch row's
                // `last_reviewed_sha` before the dispatch writes this head as requested
                // (STUDIO-959). The watcher has no prior-round record to offer here.
                prior_sha: String::new(),
            };
            match self.dispatch_review(run) {
                ReviewDispatchOutcome::Dispatched => {
                    report.dispatched += 1;
                    *slots -= 1;
                    assigned[idx] = picked;
                    if reassigned {
                        // The round moved to a substitute, so the incumbent's row leaves the watch
                        // set rather than staying beside theirs: it is the SAME required review,
                        // and two rows would make the pull request owe two of them forever —
                        // `review_round_due` would go on answering true for the incumbent at every
                        // head, for a reviewer nobody is waiting on.
                        //
                        // Retired only AFTER the dispatch succeeded. Doing it first would leave the
                        // pull request with no row at all for this required review on any refusal,
                        // and nothing would ever ask for it again.
                        tracing::info!(
                            pr = %pr, from = %row.key.reviewer,
                            "ticketless review: the round was reassigned — the incumbent was not \
                             eligible for it"
                        );
                        if let Err(e) = self.store().drop_review_watch(&row.key) {
                            tracing::warn!(review = %id, err = %e, "ticketless review: retiring the reassigned watch row failed");
                        }
                        // No capacity hold to drop here: this row's key was already removed at the
                        // top of THIS call, and the only site that inserts a hold `continue`s before
                        // reaching the reassignment, so the incumbent can never hold one by now
                        // (STUDIO-950 round 11). The top-of-call removal is the guard for it.
                    }
                    let key = churn_key(pr);
                    let counter = self.review_rounds.entry(key.clone()).or_default();
                    *counter += 1;
                    let spent = *counter;
                    // Durable from the instant it is charged (STUDIO-956): a round the daemon spent
                    // and then forgot across a restart is how one pull request ran 46 of them.
                    self.persist_review_rounds(&key);
                    if spent == REVIEW_ROUNDS_PER_PR_CAP.saturating_mul(reviewers_per_round) {
                        tracing::warn!(
                            pr = %pr, rounds = spent,
                            "ticketless review: this pull request has now had its whole re-review \
                             budget; further pushes will not be reviewed"
                        );
                    }
                }
                // Not a failure: something claimed the key between the check above and here, which
                // is precisely what the guard exists for. Next tick.
                ReviewDispatchOutcome::AlreadyInFlight => report.deferred += 1,
                ReviewDispatchOutcome::TeamsOff => report.deferred += 1,
                // A drain is armed: the row is untouched and this head is re-offered on the sweep
                // after the drain is cancelled, exactly as a deferred one is.
                ReviewDispatchOutcome::Draining => report.deferred += 1,
                // The reviewer's provider is out of daily budget (STUDIO-957): the row is untouched
                // and this head is re-offered once the budget resets, exactly as a deferred one is.
                // `dispatch_review` does not log here (it records the hold, which the sweep and
                // `/api/v1/state` carry); this is the watcher's own once-per-round line.
                ReviewDispatchOutcome::BudgetHeld => {
                    report.deferred += 1;
                    tracing::info!(
                        pr = %pr,
                        "ticketless review: deferred — the reviewer's provider is out of daily budget"
                    );
                }
                ReviewDispatchOutcome::Refused(why) => {
                    report.deferred += 1;
                    tracing::warn!(pr = %pr, reason = why, "ticketless review: the dispatch was refused");
                }
            }
        }

        // The current-label set has no writer above `on_tick`'s three early-return gates (STUDIO-949
        // round 11), so on a daemon held by a bad config, an armed drain or a dead credential — the
        // exact daemon whose dispatch has stopped — `held` is empty for the whole process lifetime
        // and `held_origin` is `false` for a ticket that genuinely wears the label. The gate then
        // opens and merges human-only work, irreversibly. Fail CLOSED instead: until a pass has
        // actually READ THE BOARD, an empty set is "unknown", not "no hold". Once a pass has run the
        // answer is real and the gate behaves exactly as before. `ledger_primed` is the same latch
        // the round gate above reads, taken in the same lock as `held`.
        //
        // MUTATION: drop the fail-closed branch and
        // `an_unprimed_hold_ledger_refuses_auto_merge` reds (a plan is proposed).
        if !ledger_primed {
            tracing::debug!(
                pr = %pr,
                "auto-merge: no selection pass has run yet, so the human-hold label set is unknown; \
                 refusing to merge"
            );
        } else if held_origin {
            tracing::debug!(
                pr = %pr,
                "auto-merge: the origin ticket is held for a human; not merging"
            );
        } else {
            self.propose_auto_merge(&mine, pr, head, merge_state, report);
        }
    }

    /// The `mergeStateStatus` values that are a SETTLED non-conflict — GitHub has finished
    /// computing mergeability and answered with something other than the conflict.
    ///
    /// Deliberately an ALLOW-list, and deliberately here rather than in [`crate::ghsummons`]: it is
    /// this one guard's question ("may I forget the head I routed for?"), not a second
    /// classification of GitHub's vocabulary. [`crate::ghsummons::MergeStateResult`] is explicit
    /// that the vocabulary is open and has grown before, so an unrecognised value is neither the
    /// conflict nor evidence that it resolved — it must forget nothing, exactly as the unsettled
    /// `UNKNOWN` (or, briefly, empty) read does. The cost of a rename is at most
    /// [`crate::reviewreconcile::RECONCILE_STALE_AFTER`] of sweep silence; the cost of the
    /// deny-list it replaces is re-summonsing an author on a vocabulary change.
    const SETTLED_NON_CONFLICT_MERGE_STATES: [&str; 6] = [
        "BEHIND",
        "BLOCKED",
        "CLEAN",
        "DRAFT",
        "HAS_HOOKS",
        "UNSTABLE",
    ];

    /// Routes a CONFLICTED watch row's ticket back to its author, once per conflicted head
    /// (STUDIO-961).
    ///
    /// The second trigger of the [`crate::reviewchanges`] route-back, and deliberately independent
    /// of the review verdict: a pull request that cannot merge is unfinished work, so an approved
    /// one still needs its author to publish a working diff. The route-back itself — comment first,
    /// then the tracker move — is performed by [`crate::reviewnotify`]'s off-loop task, through the
    /// same channel and the same plan/perform split a findings verdict uses; this half only decides.
    ///
    /// A FIFTH guard lives at the caller and not here: STUDIO-949's human hold. `service_review_pr`
    /// calls this only once `ledger_primed && !held_origin`, beside the same decision the auto-merge
    /// gate takes, because a route-back moves tracker state AND reopens the author's run — the class
    /// of action `rhapsody:human` exists to refuse. It is called from ABOVE the adjudication block
    /// (STUDIO-956) rather than from the tail, so a pull request past the round threshold still
    /// routes back: the manager decides the findings, never whether the branch merges.
    ///
    /// Four guards here, each of which the ticket names:
    ///
    /// * **Settled only.** GitHub computes mergeability lazily and answers `UNKNOWN` — or, briefly,
    ///   nothing — while it does, so only a positively-recognised [`MERGE_STATE_DIRTY`] acts. An
    ///   UNSETTLED read acts on nothing AND forgets nothing, because a mid-computation read is not
    ///   evidence that a conflict resolved; only a positively-recognised settled value that is not
    ///   the conflict clears the record (see [`Self::SETTLED_NON_CONFLICT_MERGE_STATES`]), so a
    ///   conflict that clears stops suppressing the reconciliation sweep.
    /// * **The ticket is in review.** A `reviewed` row is this daemon's own record that a findings
    ///   verdict already routed the ticket out; moving it again would be churn, and the author is
    ///   already engaged.
    /// * **Once per conflicted HEAD.** The conflict persists across every poll until a push lands,
    ///   so a naive trigger re-routes and re-summons on every sweep.
    /// * **A performer exists.** Without the notification task there is nobody to move the ticket,
    ///   so nothing is recorded and the next tick can still fire.
    fn propose_conflict_route_back(
        &mut self,
        mine: &[&ReviewWatchRow],
        pr: &PrCoord,
        head: &str,
        merge_state: &str,
        report: &mut ReviewSweepReport,
    ) {
        // The settled-state gate. `DIRTY` is the conflict and acts; a settled non-conflict is the
        // conflict GONE, so its record — which also keeps the reconciliation sweep silent — must go
        // with it. But an UNSETTLED read (`UNKNOWN`, or briefly nothing; GitHub recomputes
        // mergeability whenever the base advances) is neither: it decides nothing and forgets
        // nothing. Reading it as the conflict resolved would let `DIRTY → UNKNOWN → DIRTY` at one
        // unchanged head re-route and re-summons the author into the very loop the once-per-head
        // guard exists to prevent.
        //
        // An ALLOW-list, not "anything but `UNKNOWN`": this region's vocabulary is GitHub's own and
        // has grown before, so a value this daemon does not recognise is no more evidence the
        // conflict resolved than `UNKNOWN` is. The allow-list costs at most the sweep's own
        // staleness horizon in the case where GitHub renames a settled value, and buys immunity
        // from re-summonsing an author on a vocabulary change.
        if merge_state != MERGE_STATE_DIRTY {
            if Self::SETTLED_NON_CONFLICT_MERGE_STATES.contains(&merge_state) {
                self.conflict_routed.remove(pr);
            }
            return;
        }
        // DEFENCE IN DEPTH, not a live guard: `service_review_pr` — the one caller — returns on
        // this same condition before it reaches here, so no tick can arrive with an empty head.
        // Kept because every line below treats `head` as a real head SHA, and the cost of the
        // check is one comparison per conflicted poll.
        if head.is_empty() {
            return; // an answer with no head is not an answer about a head
        }
        // This one IS load-bearing, even though the caller's adjudication block also tests it: the
        // route-back reads `mine[0]` for the author and the origin below, so an empty slice would
        // PANIC the control task rather than skip a tick.
        if mine.is_empty() {
            return;
        }
        if mine.iter().any(|r| r.status == REVIEW_STATUS_REVIEWED) {
            return;
        }
        if self
            .conflict_routed
            .get(pr)
            .is_some_and(|routed| routed.head == head)
        {
            return;
        }
        let Some(plan) = self.plan_conflict_route_back(pr, &mine[0].introduced_by) else {
            return;
        };
        if self.review_notify_tx.is_none() {
            return;
        }
        // Recorded BEFORE the send, exactly as `auto_merge_announced` is recorded when the plan is
        // formed: the plan is what the tick acted on, and a notification task that dies between here
        // and the perform leaves the ticket where a findings route-back would have left it too. The
        // instant is carried so the sweep's silence can expire (see `ConflictRoute`).
        self.conflict_routed.insert(
            pr.clone(),
            ConflictRoute {
                head: head.to_string(),
                routed_at: (self.now)(),
            },
        );
        let completion = crate::reviewnotify::ReviewCompletion {
            reason: crate::reviewnotify::CompletionReason::Conflict,
            owner: pr.owner.clone(),
            repo: pr.repo.clone(),
            number: pr.number,
            reviewer: String::new(),
            author: mine[0].author.clone(),
            head_sha: head.to_string(),
            approved: false,
            summon_token: self.review_summon_token(),
            changes: Some(plan),
        };
        report.routed += 1;
        self.request_review_notify(Some(completion));
    }

    /// Whether this pull request's reviewer verdicts clear the auto-merge gate at `head`
    /// (STUDIO-874), appending the plan to `report` if they do.
    ///
    /// Decided from `mine` — this hand-back's OPENING snapshot of this pull request's rows — for
    /// the same reason the dispatch loop above is: it is the state the control task owns, and
    /// nothing this function reads is written by the loop it follows. A row re-armed by
    /// [`Self::handle_review_head_advanced`] moved to `requested`, which this gate refuses on
    /// EITHER reading; a row the loop dispatched is `in_flight` on either; and a row whose verdict
    /// is at an older head is stale on either. So the opening snapshot cannot clear a gate the
    /// closing one would refuse.
    fn propose_auto_merge(
        &mut self,
        mine: &[&ReviewWatchRow],
        pr: &PrCoord,
        head: &str,
        merge_state: &str,
        report: &mut ReviewSweepReport,
    ) {
        if !self.review_auto_merge_for_repo(&pr.owner, &pr.repo) {
            return; // opt-in, and off by default (the D5 invariant)
        }
        // This tick ALREADY has GitHub's answer (STUDIO-961): the `mergeStateStatus` the watcher
        // read on the poll it was making anyway says the branch conflicts, and
        // `perform_auto_merge` will re-read exactly that field through its own seam and decline on
        // anything but `CLEAN` (`runautomerge.rs`). Handing out a plan here spends two `gh` round
        // trips to be told what this string already says, and opens a narrow window in which the
        // merge is attempted while the conflict route-back's comment and tracker move are in
        // flight. Refused on the SETTLED conflict only — an empty or `UNKNOWN` read decides
        // nothing here either, and must not hold back a merge GitHub would allow.
        //
        // MUTATION: drop this branch and `a_conflicted_pull_request_is_not_offered_for_auto_merge`
        // reds (a merge plan is proposed beside the route-back).
        if merge_state == MERGE_STATE_DIRTY {
            tracing::debug!(
                pr = %pr, head,
                "auto-merge: GitHub reports this pull request conflicted, so no merge is asked for; \
                 the conflict route-back has it"
            );
            return;
        }
        match crate::automerge::auto_merge_verdict(mine, head) {
            Ok(approved_by) => {
                // At INFO when it is news, and at DEBUG for as long as it stays the same plan.
                // The gate is re-decided from the watch rows on EVERY tick and the plan is handed
                // out on every tick — a pull request held by a gate on the other side of the seam
                // must still merge the moment that gate clears. What is quieted is only the line:
                // STUDIO-881's log carried 97 of these for ONE draft pull request, at INFO, in
                // lockstep with the refusal they led to.
                if self.auto_merge_plan_is_news(pr, head, &approved_by) {
                    tracing::info!(
                        pr = %pr, head, ?approved_by,
                        "auto-merge: every reviewer approved this head; the remaining gates are \
                         asked of GitHub off-loop"
                    );
                } else {
                    tracing::debug!(
                        pr = %pr, head, ?approved_by,
                        "auto-merge: the same plan as the last tick; still asking GitHub"
                    );
                }
                report.merge.push(crate::automerge::AutoMergePlan {
                    pr: pr.clone(),
                    head: head.to_string(),
                    approved_by,
                });
            }
            // At DEBUG, not INFO: on a pull request awaiting review this is the answer on every
            // tick for as long as the review takes, and it is not news.
            Err(why) => {
                tracing::debug!(pr = %pr, head, reason = why.why(), "auto-merge: not merging")
            }
        }
    }

    /// Whether announcing this auto-merge plan says anything that has not been said, remembering
    /// it when it does (STUDIO-881).
    ///
    /// News is a plan whose HEAD or whose set of approvals differs from the one last announced for
    /// this pull request — the two things the line itself claims. Anything else is the same
    /// sentence about the same commit, and a pull request held by a gate downstream re-forms that
    /// plan once a minute for as long as the hold lasts.
    ///
    /// This governs the REPORT and nothing else: [`Self::propose_auto_merge`] hands the plan out
    /// either way, so a refusal that clears is merged on the very next tick whatever this answers.
    fn auto_merge_plan_is_news(
        &mut self,
        pr: &PrCoord,
        head: &str,
        approved_by: &[String],
    ) -> bool {
        let key = churn_key(pr);
        if self
            .auto_merge_announced
            .get(&key)
            .is_some_and(|(was_head, was_by)| was_head == head && was_by == approved_by)
        {
            return false;
        }
        self.auto_merge_announced
            .insert(key, (head.to_string(), approved_by.to_vec()));
        true
    }

    /// Who reviews this round: the incumbent where continuity means something, otherwise the
    /// least-loaded non-author. `None` when this pull request's remaining rows leave nobody
    /// eligible — which defers the round rather than handing one teammate two of its reviews — or
    /// when an author-less row's incumbent has left the roster, leaving no identity to dispatch
    /// under.
    ///
    /// **A teammate's `max_concurrent` is not consulted here** (design D2, "reviews are free"): it
    /// caps the IMPLEMENTATION work they are dispatched, never their availability to read somebody
    /// else's. `load` still ranks — the least-loaded eligible teammate goes first — it just no
    /// longer excludes.
    ///
    /// `peers` are the reviewers of this pull request's OTHER rows, excluded so a substitution
    /// cannot hand one teammate two of the same pull request's required reviews.
    ///
    /// **An empty `author` fails closed.** It means the row predates the column or came from a
    /// caller that did not supply one, and "nobody is the author" is the one reading that would
    /// hand a teammate their own pull request to review. So an author-less row may only ever be
    /// serviced by its incumbent — who introduction already excluded the author from being.
    fn choose_review_reviewer(
        &self,
        row: &ReviewWatchRow,
        peers: &HashSet<String>,
        load: &LoadSnapshot,
    ) -> Option<String> {
        let teams = self.teams.as_ref()?;
        // `rank_reviewers` only ever names roster members, so `peers` is the whole filter — a
        // teammate at their `max_concurrent` is a candidate like any other (D2).
        let exclusions = self.reviewer_exclusions(teams);
        let incumbent = row.key.reviewer.as_str();
        if row.author.trim().is_empty() {
            // Roster membership, and deliberately NOT capacity (D2): a reviewer who has left the
            // roster since the row was written has no identity left to dispatch under, which is a
            // reason to defer that survives. Being at their implementation cap is not. An identity
            // whose dispatch would be REFUSED is deferred for the same reason a refuse cannot
            // become a review (STUDIO-978): handing it the round would only dispatch a run that
            // records failed and never completes.
            let on_roster = teams.roster.iter().any(|i| i.name == incumbent);
            return (on_roster && !exclusions.unselectable.contains(incumbent))
                .then(|| incumbent.to_string());
        }
        let candidates: Vec<String> =
            crate::quorum::rank_reviewers(teams, row.author.trim(), load.counts(), &exclusions)
                .into_iter()
                .filter(|name| !peers.contains(name.as_str()))
                .collect();
        // Decision B, applied where it earns its keep: a reviewer who READ the previous round knows
        // the pull request and their own findings, so they keep it as long as they are still a
        // candidate — on the roster, not the author, not already holding another of this pull
        // request's required reviews. A row with no last round has no such continuity, so it takes
        // the ranking's answer — which is what makes two same-tick introductions land on two
        // different reviewers.
        //
        // The one thing continuity does NOT outrank is a required reviewer (STUDIO-951). A row
        // persisted before the operator added `review.required` still names its old reviewer, and
        // letting continuity keep them would mean the required identity never reviews that pull
        // request — the guarantee the config asks for, silently unmet, with no adoption or
        // reconciliation sweep to repair it. So continuity holds only while the incumbent is itself
        // required, or while the ranking offers no required reviewer at all (the unset case, which
        // is byte-identical to before the feature).
        //
        // "Required" here is the EFFECTIVE pin set, not the configured list: a required name the
        // ranking never promoted — off the roster, the author, `unselectable` — is not a reviewer
        // this round yields to, so it must not evict the incumbent either. Reading the raw list made
        // an unselectable pin break continuity for a teammate the ranking never selected, handing
        // the round to whoever merely led on load (round 3).
        if !row.last_reviewed_sha.is_empty() && candidates.iter().any(|name| name == incumbent) {
            let pinned =
                crate::quorum::pinned_required_reviewers(teams, row.author.trim(), &exclusions);
            let incumbent_required = pinned.iter().any(|name| name == incumbent);
            let required_among_candidates = candidates.iter().any(|name| pinned.contains(name));
            if incumbent_required || !required_among_candidates {
                return Some(incumbent.to_string());
            }
        }
        candidates.into_iter().next()
    }

    /// The clone URL of the ENABLED project that owns `owner/repo`, or `None` — the watched-repo
    /// allowlist, re-checked at dispatch time against the CURRENT configuration.
    ///
    /// Returns the project's own `repo` string rather than reconstructing a URL, because
    /// `dispatch_review`'s routing matches a project by that exact string: anything else would
    /// resolve the allowlist and then fail to route.
    ///
    /// Only `projects` are searched, and nothing is lost by that: `resolve_projects` synthesizes a
    /// project even for a config with no `projects:` block, carrying the top-level `repo` and always
    /// enabled, so a legacy single-project configuration resolves here like any other. An earlier
    /// version of this comment claimed the legacy form could introduce a pull request but never
    /// dispatch a review of it — it cannot happen, and no such gap exists (STUDIO-727).
    fn review_repo_url(&self, owner: &str, repo: &str) -> Option<String> {
        let eff = self.eff.as_ref()?;
        eff.projects
            .iter()
            .find(|p| {
                !p.disabled
                    && crate::ghsummons::parse_repo(&p.repo).is_some_and(|(o, r)| {
                        o.eq_ignore_ascii_case(owner) && r.eq_ignore_ascii_case(repo)
                    })
            })
            .map(|p| p.repo.clone())
    }

    /// Every slug of a resolved project that owns `owner/repo`, in resolution
    /// order; empty when no resolved project owns it. [`Self::review_repo_url`]'s
    /// lookup, without its `!disabled` filter and returning the slugs rather than
    /// the repo string.
    ///
    /// The missing `!disabled` is deliberate for the one caller,
    /// [`Self::review_auto_merge_for_repo`]: a project paused after a pull
    /// request entered the watch set must still have its override read, because
    /// the override is the fail-safe half — dropping it would fall back to a
    /// GLOBAL value the operator may have set this very repo aside from.
    ///
    /// It returns EVERY owning slug, not the first match, because a WORKFLOW
    /// project fans out to one resolved project per slug and they share one repo
    /// (STUDIO-927). A first-match scan would let an override naming any slug but
    /// the first be invisible — exactly the booch case, whose two hex slugs share
    /// `git@github.com:makewhatis/booch.git`.
    fn review_project_slugs_for_repo(&self, owner: &str, repo: &str) -> Vec<&str> {
        let Some(eff) = self.eff.as_ref() else {
            return Vec::new();
        };
        eff.projects
            .iter()
            .filter_map(|p| {
                crate::ghsummons::parse_repo(&p.repo)
                    .is_some_and(|(o, r)| {
                        o.eq_ignore_ascii_case(owner) && r.eq_ignore_ascii_case(repo)
                    })
                    .then_some(p.slug.as_str())
            })
            .collect()
    }

    /// The effective `teams.review.auto_merge` for the project set that owns
    /// `owner/repo` (STUDIO-927): the owning projects' own per-project overrides
    /// when they have any, else the installation-wide default. A repo that no
    /// resolved project owns — and an orchestrator carrying no resolved project set
    /// at all — falls back to the top-level value, so every existing installation
    /// behaves exactly as it did before the per-project block existed.
    ///
    /// When several resolved projects share the repo, their answers are ANDed:
    /// **any** owning project that resolves `false` holds the merge. That is the
    /// fail-safe direction — the choice this accessor exists to make — because the
    /// alternative (first match wins) fails OPEN, letting a repo an operator
    /// explicitly set aside self-merge simply because its override named the second
    /// of two slugs.
    ///
    /// The AND cuts both ways, though. Holding a merge back takes naming any ONE
    /// slug (its sibling inherits the global `true`, and the AND still yields
    /// `false`); opting a repo back IN under a global `false` takes naming EVERY
    /// slug of the project, because an unnamed sibling inherits the global `false`
    /// and holds the merge. That asymmetry is deliberate: the alternative — any
    /// explicit `false` wins, else any explicit `true`, else global — would let a
    /// single `true` on a sibling slug override another slug's explicit `false` and
    /// fail OPEN, which is the direction this accessor exists to prevent.
    pub(crate) fn review_auto_merge_for_repo(&self, owner: &str, repo: &str) -> bool {
        let Some(teams) = self.teams.as_ref() else {
            return false;
        };
        let slugs = self.review_project_slugs_for_repo(owner, repo);
        if slugs.is_empty() {
            return teams.review_auto_merge();
        }
        slugs.iter().all(|slug| teams.review_auto_merge_for(slug))
    }
}

/// Whether a watch row belongs to `pr`. Case-insensitive, because GitHub logins and repository
/// names are.
pub(crate) fn row_is(row: &ReviewWatchRow, pr: &PrCoord) -> bool {
    row.key.owner.eq_ignore_ascii_case(&pr.owner)
        && row.key.repo.eq_ignore_ascii_case(&pr.repo)
        && row.key.number == pr.number
}

/// Whether every LIVE row of `pr` is an APPROVAL — the convergence question
/// [`crate::automerge::auto_merge_verdict`] answers from a live head, reduced here to the rows' own
/// verdicts so the author half (which holds no GitHub observation) can ask it too.
///
/// A row that is `approved` has stated a verdict about the commit it read, and a head advance
/// re-arms it to `requested` on the next sweep — so at the moment an author is summoned after a
/// changes-requested round, "all approved" cannot be a stale pre-push verdict. A pull request with
/// no live row is NOT converged: nothing has reviewed it.
fn converged(rows: &[ReviewWatchRow], pr: &PrCoord) -> bool {
    let mut any = false;
    for r in rows
        .iter()
        .filter(|r| row_is(r, pr) && r.open && r.status != REVIEW_STATUS_DROPPED)
    {
        any = true;
        if r.status != REVIEW_STATUS_APPROVED {
            return false;
        }
    }
    any
}

/// The per-pull-request re-review budget, keyed by `owner/repo#number`.
pub type ReviewRounds = HashMap<String, usize>;

/// The auto-merge plan each watched pull request has already been ANNOUNCED for: its head and the
/// approvals that cleared the gate at that head, keyed by [`churn_key`] as [`ReviewRounds`] is.
/// See [`Orchestrator::auto_merge_announced`]. STUDIO-881.
pub type AnnouncedPlans = HashMap<String, (String, Vec<String>)>;

/// What one poll of ONE watched pull request told the watcher about its current head.
///
/// The three facts arrive from two different structs — `head`/`merge_state` off the
/// [`PrSnapshot`](crate::ghsummons::PrSnapshot) the lookup answered with, `unchanged_from` off the
/// [`PrObservation`](crate::prstate::PrObservation) that carried it — so no existing type holds
/// them together. Bundled rather than threaded flat because they are one observation and they
/// travel together, and because three more scalars on
/// [`service_review_pr`](Orchestrator::service_review_pr)'s already long argument list is where a
/// caller starts transposing two `&str`s.
struct ObservedHead<'a> {
    /// The head SHA GitHub reports for the pull request right now.
    head: &'a str,
    /// GitHub's `mergeStateStatus`, upper-cased, or empty when it answered none (STUDIO-961).
    merge_state: &'a str,
    /// The head SHAs this pull request's rows have already READ, for STUDIO-960's proof that a head
    /// move carried no new work.
    unchanged_from: &'a [String],
}

/// One pull request's record of the conflict route-back the watcher has already fired
/// (STUDIO-961), keyed by coordinate. See [`Orchestrator::conflict_routed`].
///
/// Two facts with two different lifetimes, which is why they are named rather than folded into one
/// value: the HEAD is the debounce and lives until the head moves (or the conflict resolves), while
/// the instant lets the reconciliation sweep's silence expire independently of it.
#[derive(Debug, Clone)]
pub(crate) struct ConflictRoute {
    /// The head the route-back was fired at — the once-per-conflicted-head guard.
    pub head: String,
    /// When it was fired. A conflict route-back keeps the reconciliation sweep silent, but only
    /// while it is FRESH: past [`crate::reviewreconcile::RECONCILE_STALE_AFTER`] the transition has
    /// stopped being progress — the author never answered it, or the tracker move never landed —
    /// and the pull request needs the human signal again.
    pub routed_at: chrono::DateTime<chrono::Utc>,
}

/// One round the watcher deferred for want of a global slot, recorded so the reconciliation sweep
/// can report it as a DELIBERATE capacity hold rather than an unexplained stall (STUDIO-950).
///
/// It annotates, it never suppresses: the sweep keeps reporting the pull request — the alarm that
/// STUDIO-898 exists to raise — and names this as the cause instead of claiming nothing has
/// reported it blocked, exactly as [`Divergence::auto_merge_reason`](crate::reviewreconcile::Divergence::auto_merge_reason)
/// does for auto-merge (STUDIO-923). It is a statement about the capacity THIS sweep found, not a
/// duration: what reaches the report is a ROW whose obligation has been stale past the sweep's own
/// 90-minute threshold — the incident — while the hold itself is only as old as the watcher's most
/// recent tick. A transient hold self-corrects the moment a slot frees.
///
/// The record is deliberately coarse: it is taken the moment the slot check fails, BEFORE the
/// permanent refusals below it (the per-PR churn cap, `choose_review_reviewer`, `review_repo_url`)
/// are evaluated, so a round that would have been turned away for one of those anyway is annotated
/// "held for capacity" while the budget happens to be spent. That is a true statement about the
/// present and self-corrects the moment a slot frees; it does not claim the round WOULD dispatch
/// next tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapacityHold {
    /// How many runs held the ACTIVE pool when the sweep deferred this round — every running run in
    /// shared mode, the ticketless reviews once `agent.max_concurrent_reviews` gives them their own.
    pub holders: i64,
    /// Whether those runs held reviews' OWN budget (`agent.max_concurrent_reviews`) rather than the
    /// shared `agent.max_concurrent_agents` one. Names which knob the operator would tune.
    pub separate: bool,
    /// When the holding sweep ran. Retained as the reconciliation sweep's FALLBACK freshness
    /// reference for a hold that predates any sweep; the live reference is the watcher's own
    /// [`Orchestrator::review_watch_swept`] liveness, because the cursor may not revisit this round
    /// for several ticks and ageing the hold itself expired rounds a healthy watcher still held
    /// (STUDIO-950 round 11). The fallback is a TEST AFFORDANCE, not a production state: the only
    /// production insert site (`reviewwatch`) stamps `review_watch_swept` at the top of the same
    /// call, and the stamp is never reset to `None`, so a hold in the map always implies a stamp.
    /// The `reviewreconcile` fixtures insert holds directly and set no stamp, which is the only path
    /// that reaches it.
    pub recorded: chrono::DateTime<chrono::Utc>,
}

impl CapacityHold {
    /// The config key an operator would turn to free a slot from the pool this hold names — reviews'
    /// own `agent.max_concurrent_reviews` when the round was held against it, the shared
    /// `agent.max_concurrent_agents` otherwise. One source for the reconciliation WARN and the
    /// `/api/v1/state` annotation, so the two cannot drift apart.
    pub fn budget_key(&self) -> &'static str {
        if self.separate {
            "agent.max_concurrent_reviews"
        } else {
            "agent.max_concurrent_agents"
        }
    }
}

/// The rounds the watcher held on its most recent sweep, keyed by the same
/// `review:<owner>/<repo>#<n>@<reviewer>` id `running` and `claimed` use. See
/// [`Orchestrator::review_capacity_held`]. STUDIO-950.
pub(crate) type CapacityHolds = HashMap<String, CapacityHold>;

/// How long the watcher's liveness ([`Orchestrator::review_watch_swept`]) may go un-refreshed
/// before a recorded [`CapacityHold`] stops being meaningful. The watcher stamps its liveness on
/// EVERY sweep it runs — not once per hold — so the thing this bounds is the sweep-to-sweep gap, and
/// a healthy gap is NOT one [`PR_STATE_POLL_INTERVAL`](crate::prstate::PR_STATE_POLL_INTERVAL): the
/// interval is the sleep BEFORE each sweep, and the tick then makes TWO serial phases of `gh`
/// lookups, each bounded by
/// [`MAX_PR_STATE_CALLS_PER_TICK`](crate::prstate::MAX_PR_STATE_CALLS_PER_TICK) calls at
/// [`GH_EXEC_TIMEOUT`](crate::ghsummons::GH_EXEC_TIMEOUT) apiece. The FIRST is the batched sweep
/// ([`sweep_pr_states`](crate::prstate::sweep_pr_states)); the SECOND is STUDIO-953's
/// per-observation pre-dispatch re-read ([`refresh_observed_head`]), which asks GitHub once more for
/// every OPEN observation the sweep returned — the same bound, spent again. Counting only the first
/// phase understates a healthy tick by half, so the watcher's liveness could be called stale while
/// it is still working through the very sweep that stamped it. Three slow lookups already put a
/// healthy watcher past two intervals, so the bound is the worst case — the sleep plus both lookup
/// phases — rather than the cadence alone. Liveness older than that is from a watcher that has since
/// stopped happening (a `gh` outage, a cancelled watcher), and the reconciliation sweep must not keep
/// naming a hold nothing is refreshing. The threshold keeps the two sweeps decoupled: the
/// reconciliation sweep asks only whether the watcher's liveness is FRESH, never whether the watcher
/// is running. Because liveness — not the individual hold — is what ages, a round the rotating cursor
/// has not revisited for several ticks stays fresh for exactly as long as the watcher keeps sweeping
/// (STUDIO-950 round 11).
pub(crate) const CAPACITY_HOLD_TTL: std::time::Duration = std::time::Duration::from_secs(
    crate::prstate::PR_STATE_POLL_INTERVAL.as_secs()
        + 2 * crate::prstate::MAX_PR_STATE_CALLS_PER_TICK as u64
            * crate::ghsummons::GH_EXEC_TIMEOUT.as_secs(),
);

/// How many CONSECUTIVE failed `gh` lookups of one pull request it takes before the reconciliation
/// sweep stops naming that pull request's [`CapacityHold`] (STUDIO-950 rounds 14–15).
///
/// This bounds WHOLE ATTEMPTS, not wall-clock, and that is the whole point. The quantity being
/// bounded is how long until the rotating cursor next REACHES this pull request —
/// `ceil(watch_set.len() / MAX_PR_STATE_CALLS_PER_TICK)` ticks, a ROTATION — while
/// [`CAPACITY_HOLD_TTL`] is a TICK-sized bound (`PR_STATE_POLL_INTERVAL + 2 * N * T`). No value of
/// those three constants makes `ceil(W/N) * (I + 2*N*T) <= CAPACITY_HOLD_TTL` hold once `W > N`,
/// so a wall-clock grace against the TTL dropped a hold the watcher was still carrying on any
/// watch set larger than `MAX_PR_STATE_CALLS_PER_TICK` — one transient rate-limit, one rotation,
/// and the false "nothing has reported it blocked" page came back. A count cannot make that
/// mistake: it only moves on a tick that actually asked this coordinate, so it is
/// rotation-independent by construction.
///
/// The value is 2, so ONE failed attempt still names the hold — a single transient lookup failure
/// must not blink a live annotation off for a sweep — while a second consecutive failure (two ticks
/// on which GitHub would not answer) drops it. The grace is deliberately small and is NOT sized to
/// survive a GitHub rate limit: the primary REST limit resets on the hour, far beyond any count of
/// attempts, and a wall-clock grace that tried to cover it is exactly what rounds 14–15 removed. It
/// need not cover it, because the denial is REPORTED — the reconciliation sweep names the unreadable
/// coordinate instead of falling through to the false plain page (round 18). A genuinely dead
/// coordinate takes one further rotation per attempt to reach that count, which is a bound; the
/// defect this closes was that it never aged out at all.
pub(crate) const UNREADABLE_ATTEMPTS_TO_DROP_HOLD: u32 = 2;

impl ControlHandle {
    /// The pull requests the watcher should ask GitHub about, each with the head SHAs its rows have
    /// already had read. Empty when the subsystem is off or the control task is gone — an empty
    /// poll list, never a guess.
    pub(crate) async fn review_watch_list(&self) -> Vec<WatchedPr> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        if self
            .events
            .send(Event::ReviewWatchList { reply: tx })
            .is_err()
        {
            return Vec::new();
        }
        let mut lifetime = self.ctx.clone();
        tokio::select! {
            r = rx => r.unwrap_or_default(),
            _ = lifetime.cancelled() => Vec::new(),
        }
    }

    /// Hands observations to the control task, which decides every drop, re-arm and dispatch, and
    /// returns the remaining daemon-wide dispatch budget for the tick beside the report.
    ///
    /// `slots` is `None` on the tick's first hand-back and `Some(left)` afterwards, so one tick
    /// spends one budget: see [`ReviewWatchSink::sweep`].
    ///
    /// The wait is bounded by the daemon lifetime rather than a timer, as every other
    /// off-loop hand-back here is: nothing is answering an agent's MCP call, so a busy tick should
    /// delay this tick's decisions rather than turn them into a false failure.
    pub(crate) async fn review_sweep(
        &self,
        observed: Vec<PrObservation>,
        slots: Option<i64>,
    ) -> (ReviewSweepReport, i64) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        if self
            .events
            .send(Event::ReviewSweep {
                observed,
                slots,
                reply: tx,
            })
            .is_err()
        {
            return (
                ReviewSweepReport::default(),
                slots.unwrap_or(UNCAPPED_SLOTS),
            );
        }
        let mut lifetime = self.ctx.clone();
        tokio::select! {
            // A dropped reply is a control task that never decided: like the failed send above, it
            // spent nothing, so the carry stands and the next hand-back counts its own rather than
            // inheriting a zero that would retire the rest of the tick (STUDIO-953).
            r = rx => r.unwrap_or_else(|_| (ReviewSweepReport::default(), slots.unwrap_or(UNCAPPED_SLOTS))),
            _ = lifetime.cancelled() => (ReviewSweepReport::default(), slots.unwrap_or(UNCAPPED_SLOTS)),
        }
    }

    /// Reports the coordinates whose `gh` lookup failed this tick to the control task (STUDIO-950
    /// round 14). Fire-and-forget: it records control-owned state, but nothing else in the same
    /// tick depends on the write having landed, and the unbounded event channel preserves its order
    /// ahead of the observations that tick hands back. A gone control task drops it silently, which
    /// is what a failed send already means for [`Self::review_sweep`].
    pub(crate) async fn review_unreadable(&self, failed: Vec<PrCoord>) {
        if failed.is_empty() {
            return;
        }
        let _ = self.events.send(Event::ReviewUnreadable { failed });
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use rhapsody_config::teams::{Identity, Review, ReviewMode};
    use rhapsody_core::LinkedPRRef;
    use rhapsody_store::{
        REVIEW_STATUS_IN_FLIGHT, REVIEW_STATUS_REQUESTED, REVIEW_STATUS_TRUNCATED, ReviewWatchKey,
        Sqlite, StorePath,
    };
    use rhapsody_tracker::fake::Fake;

    use super::*;
    use crate::control_loop::CancelSignal;
    use crate::ghsummons::{PrSnapshot, PrStateResult, ReviewDiffResult};
    use crate::orchestrator::RunningEntry;
    use crate::testsupport::{
        DispatchedEntries, capture_events, empty_effective, empty_resolved_project, retry_entry,
        set_of,
    };

    const REPO_URL: &str = "git@github.com:makewhatis/rhapsody.git";
    const OWNER: &str = "makewhatis";
    const REPO: &str = "rhapsody";
    const HEAD_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const HEAD_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const HEAD_C: &str = "cccccccccccccccccccccccccccccccccccccccc";

    /// The watcher cadence a test's [`ReviewWatchDeps`] starts with: the legacy pinned interval, so
    /// the paused-clock tests that advance
    /// [`PR_STATE_POLL_INTERVAL`](crate::prstate::PR_STATE_POLL_INTERVAL) still observe exactly one
    /// tick per interval (STUDIO-974). A positive value, so the `<= 0` fallback to the real default
    /// never applies in those tests.
    fn test_poll_interval() -> std::sync::Arc<std::sync::atomic::AtomicI64> {
        std::sync::Arc::new(std::sync::atomic::AtomicI64::new(
            crate::prstate::PR_STATE_POLL_INTERVAL.as_millis() as i64,
        ))
    }

    /// STUDIO-974: the watcher reads its cadence from the shared atomic each tick, and a
    /// non-positive stored value (unset, or a direct construction that skipped the field) falls back
    /// to the configured default rather than a zero-millisecond busy loop.
    #[test]
    fn poll_interval_reads_the_shared_atomic() {
        let deps = |ms: i64| ReviewWatchDeps {
            pr_source: None,
            allow: HeadAllowlist::none(),
            teams: ticketless(&["alice"]),
            sink: Arc::new(FakeSink::default()),
            diff_source: None,
            poll_interval_ms: std::sync::Arc::new(std::sync::atomic::AtomicI64::new(ms)),
        };
        assert_eq!(poll_interval(&deps(5_000)), 5_000);
        assert_eq!(
            poll_interval(&deps(0)),
            rhapsody_config::model::DEFAULT_PR_STATE_INTERVAL_MS
        );
    }

    /// STUDIO-974 review finding: the watcher must CONSUME a hot-reloaded cadence, not merely have
    /// a helper that reads the atomic. This drives the real task on a paused clock: it polls on a
    /// non-default 1s cadence, the atomic is changed to 7s WHILE THE TASK IS ALIVE, and the next
    /// sleep that begins after the change must use 7s.
    ///
    /// Mutation check: hardcode the sleep at the pinned `PR_STATE_POLL_INTERVAL` and this reds at
    /// the first assert (no tick arrives at 1s). Before this test, that mutation left every
    /// orchestrator test green (sol's review at STUDIO-974).
    #[tokio::test(start_paused = true)]
    async fn the_watcher_consumes_a_hot_reloaded_cadence() {
        const FIRST_MS: i64 = 1_000;
        const RELOADED_MS: i64 = 7_000;
        let interval = Arc::new(std::sync::atomic::AtomicI64::new(FIRST_MS));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let boundaries = Arc::new(Mutex::new(Vec::new()));
        let done = Arc::new(tokio::sync::Notify::new());
        let signal = CancelSignal::new();
        let deps = ReviewWatchDeps {
            pr_source: Some(Arc::new(FakeSource)),
            allow: HeadAllowlist::none(),
            poll_interval_ms: Arc::clone(&interval),
            teams: ticketless(&["alice", "bob"]),
            diff_source: None,
            sink: Arc::new(FakeSink {
                watched: vec![WatchedPr::new(coord(12))],
                seen: Arc::clone(&seen),
                boundaries: Arc::clone(&boundaries),
                done: Arc::clone(&done),
                ..FakeSink::default()
            }),
        };
        let task = tokio::spawn(run_review_watch_task(signal.wait(), deps));

        // The first tick must arrive on the configured 1s cadence, not the pinned 120s default.
        tokio::time::sleep(std::time::Duration::from_millis(1_050)).await;
        assert_eq!(
            boundaries.lock().expect("boundaries lock").len(),
            1,
            "the watcher must tick on the configured cadence"
        );

        // Hot reload the cadence while the task is alive. The sleep already in flight was
        // scheduled from the OLD value and still fires a second later.
        interval.store(RELOADED_MS, Ordering::Relaxed);
        tokio::time::sleep(std::time::Duration::from_millis(1_050)).await;
        assert_eq!(
            boundaries.lock().expect("boundaries lock").len(),
            2,
            "the sleep already in flight still used the old cadence"
        );

        // The sleep AFTER that one must use the reloaded 7s: nothing at ~8.1s, then a tick at ~9.1s.
        tokio::time::sleep(std::time::Duration::from_millis(6_000)).await;
        assert_eq!(
            boundaries.lock().expect("boundaries lock").len(),
            2,
            "the watcher must consume the hot-reloaded cadence, not the old one"
        );
        tokio::time::sleep(std::time::Duration::from_millis(1_000)).await;
        assert_eq!(
            boundaries.lock().expect("boundaries lock").len(),
            3,
            "the reloaded cadence's tick must arrive on the new interval"
        );

        signal.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), task).await;
    }

    fn ident(name: &str, max_concurrent: i64) -> Identity {
        Identity {
            name: name.to_string(),
            profile: "swe".to_string(),
            max_concurrent,
            ..Identity::default()
        }
    }

    fn teams_with(enabled: bool, mode: ReviewMode, roster: Vec<Identity>) -> Teams {
        Teams {
            enabled,
            review: Review {
                mode,
                ..Review::default()
            },
            roster,
            ..Teams::disabled()
        }
    }

    /// Teams on, `review.mode: ticketless`, an uncapped roster — everything the watcher gates on.
    /// The `max_concurrent: 0` is belt-and-braces, full stop: reviewer choice stopped consulting
    /// capacity in STUDIO-800, and the dispatch below it routes at Tier 0 on the synthetic issue's
    /// `rhapsody:@<reviewer>` label (`review.rs`), short-circuiting `best_by_label_overlap` — the
    /// one caller of `at_capacity` this module could otherwise reach.
    fn ticketless(names: &[&str]) -> Teams {
        teams_with(
            true,
            ReviewMode::Ticketless,
            names.iter().map(|n| ident(n, 0)).collect(),
        )
    }

    /// An orchestrator with one enabled project owning [`REPO_URL`], an in-memory store and a
    /// recording spawn seam — the shape `dispatch_review` needs to reach a worker.
    ///
    /// Primed by a selection pass, because every watcher test after this one simulates a daemon that
    /// is actually dispatching: on a real one the first tick runs before the watcher's 120s first
    /// sweep, so the human-hold label set is a real answer by the time the watcher reads it. The
    /// un-primed state is its own case — see [`orch_before_first_pass`].
    fn orch(teams: Teams) -> (Orchestrator, DispatchedEntries) {
        let (o, dispatched) = orch_before_first_pass(teams);
        o.human_holds.begin_pass(true);
        (o, dispatched)
    }

    /// [`orch`] against a store the CALLER owns — the restart shape (STUDIO-956).
    fn orch_on(
        teams: Teams,
        store: Arc<dyn rhapsody_store::Store + Send + Sync>,
    ) -> (Orchestrator, DispatchedEntries) {
        let (o, dispatched) = orch_on_store(teams, store);
        o.human_holds.begin_pass(true);
        (o, dispatched)
    }

    /// [`orch`] with the human-hold ledger left un-primed: no selection pass has run, so the ledger's
    /// current-label set is an absence of information rather than "no hold" (STUDIO-949 round 11).
    fn orch_before_first_pass(teams: Teams) -> (Orchestrator, DispatchedEntries) {
        orch_on_store(
            teams,
            Arc::new(Sqlite::open(StorePath::InMemory).expect("open in-memory store")),
        )
    }

    /// [`orch_before_first_pass`] against a store the CALLER owns — the restart shape. Two
    /// orchestrators built over one `Arc<Sqlite>` are two daemon lifetimes over one database file
    /// (STUDIO-956).
    fn orch_on_store(
        teams: Teams,
        store: Arc<dyn rhapsody_store::Store + Send + Sync>,
    ) -> (Orchestrator, DispatchedEntries) {
        let tracker = Arc::new(Fake::new());
        let mut eff = empty_effective(tracker.clone());
        eff.active_states = set_of(&["todo", "in progress"]);
        eff.terminal_states = set_of(&["done"]);
        eff.max_concurrent = 10;
        let mut proj = empty_resolved_project("rhapsody", tracker);
        proj.repo = REPO_URL.to_string();
        eff.projects = vec![proj];
        let mut o = Orchestrator::new("WORKFLOW.md");
        o.eff = Some(eff);
        o.teams = Some(teams);
        o.set_store(store);
        let dispatched: DispatchedEntries = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&dispatched);
        o.spawn = Some(Box::new(move |_iss, _attempt, re| {
            sink.lock().expect("dispatched lock").push(re.clone());
        }));
        (o, dispatched)
    }

    fn key(number: i64, reviewer: &str) -> ReviewWatchKey {
        ReviewWatchKey {
            owner: OWNER.to_string(),
            repo: REPO.to_string(),
            number,
            reviewer: reviewer.to_string(),
        }
    }

    /// A freshly-introduced row: `alice` authored it, `reviewer` was asked, nothing dispatched yet.
    fn row(number: i64, reviewer: &str) -> ReviewWatchRow {
        ReviewWatchRow {
            key: key(number, reviewer),
            author: "alice".to_string(),
            introduced_by: "handoff:STUDIO-721".to_string(),
            requested_sha: String::new(),
            last_reviewed_sha: String::new(),
            status: REVIEW_STATUS_REQUESTED.to_string(),
            open: true,
        }
    }

    /// [`ticketless`] with the STUDIO-712 auto-Done transition switched on.
    fn ticketless_done(names: &[&str], state: &str) -> Teams {
        let mut teams = ticketless(names);
        teams.review.done_state = state.to_string();
        teams
    }

    fn introduce(o: &Orchestrator, r: ReviewWatchRow) {
        o.store().save_review_watch(r).expect("introduce");
    }

    /// Writes a profile file under the orchestrator's profiles dir, creating the dir.
    fn write_profile(dir: &crate::testsupport::TempDir, name: &str, text: &str) {
        let p = std::path::PathBuf::from(dir.child("profiles"));
        std::fs::create_dir_all(&p).expect("create profiles dir");
        std::fs::write(p.join(format!("{name}.md")), text).expect("write profile");
    }

    /// A finished run of `issue` — the row the auto-Done transition reads the opaque tracker ids
    /// off, written by `persist_start_run` in production.
    fn run_of(o: &Orchestrator, issue: &str) {
        o.store()
            .start_run(rhapsody_store::RunStart {
                issue_id: format!("ID-{issue}"),
                issue_identifier: issue.to_string(),
                team_id: "TEAM-1".to_string(),
                ..rhapsody_store::RunStart::default()
            })
            .expect("start run");
    }

    fn coord(number: i64) -> PrCoord {
        PrCoord::new(OWNER, REPO, number)
    }

    /// One observation of an OPEN pull request at `head`.
    fn open_at(number: i64, head: &str) -> PrObservation {
        PrObservation {
            pr: coord(number),
            lookup: PrLookup::Found(PrSnapshot {
                is_draft: Some(false),
                head_sha: head.to_string(),
                status: PrStatus::Open,
                merged_at: None,
                head_repo: format!("{OWNER}/{REPO}"),
                merge_state: String::new(),
            }),
            unchanged_from: Vec::new(),
        }
    }

    /// One observation of an OPEN pull request at `head`, with `unchanged_from` set as the off-loop
    /// watcher would after proving a head move carried no new work (STUDIO-960).
    fn open_at_proven(number: i64, head: &str, unchanged_from: &[String]) -> PrObservation {
        PrObservation {
            unchanged_from: unchanged_from.to_vec(),
            ..open_at(number, head)
        }
    }

    /// One observation of a MERGED pull request at `head`. The timestamp is fixed rather than
    /// `now()` and is never asserted on: the transition keys on the STATUS, and a merged pull
    /// request whose `mergedAt` would not parse must still be a merge (see `reviewdone`).
    fn merged_at(head: &str) -> PrLookup {
        PrLookup::Found(PrSnapshot {
            is_draft: Some(false),
            head_sha: head.to_string(),
            status: PrStatus::Merged,
            merged_at: chrono::DateTime::parse_from_rfc3339("2026-09-10T00:00:00Z")
                .ok()
                .map(|t| t.with_timezone(&chrono::Utc)),
            head_repo: format!("{OWNER}/{REPO}"),
            merge_state: String::new(),
        })
    }

    /// One observation of a pull request that was CLOSED without merging.
    fn closed_at(head: &str) -> PrLookup {
        PrLookup::Found(PrSnapshot {
            is_draft: Some(false),
            head_sha: head.to_string(),
            status: PrStatus::Closed,
            merged_at: None,
            head_repo: format!("{OWNER}/{REPO}"),
            merge_state: String::new(),
        })
    }

    fn observed(number: i64, lookup: PrLookup) -> PrObservation {
        PrObservation {
            pr: coord(number),
            lookup,
            unchanged_from: Vec::new(),
        }
    }

    fn watch_row(o: &Orchestrator, number: i64, reviewer: &str) -> ReviewWatchRow {
        o.store()
            .get_review_watch(&key(number, reviewer))
            .expect("read watch row")
            .expect("row exists")
    }

    /// The reviewer each dispatched run was given, in dispatch order.
    fn reviewers_of(dispatched: &DispatchedEntries) -> Vec<String> {
        dispatched
            .lock()
            .expect("dispatched lock")
            .iter()
            .map(|re| re.identity.clone())
            .collect()
    }

    /// Ends the live review of `(number, reviewer)` as a clean, DECLARED completion at `head` —
    /// what `on_review_exit` does, without needing a worker.
    fn complete(o: &mut Orchestrator, number: i64, reviewer: &str, head: &str) {
        let id = review_key(OWNER, REPO, number, reviewer);
        o.running.remove(&id);
        o.claimed.remove(&id);
        o.store()
            .mark_review_completed(&key(number, reviewer), head, REVIEW_STATUS_REVIEWED)
            .expect("complete");
    }

    /// [`complete`] as a clean APPROVAL — the verdict a rebase must carry forward rather than make
    /// the reviewer earn again (STUDIO-960).
    fn approve(o: &mut Orchestrator, number: i64, reviewer: &str, head: &str) {
        let id = review_key(OWNER, REPO, number, reviewer);
        o.running.remove(&id);
        o.claimed.remove(&id);
        o.store()
            .mark_review_completed(&key(number, reviewer), head, REVIEW_STATUS_APPROVED)
            .expect("approve");
    }

    /// Ends the live review of `(number, reviewer)` the way a CRASH does: the run is gone from
    /// `running`, and the watch row is left exactly where the dispatch put it (`in_flight` at its
    /// requested SHA) — the exit path deliberately does not clear it.
    fn crash(o: &mut Orchestrator, number: i64, reviewer: &str) {
        let id = review_key(OWNER, REPO, number, reviewer);
        o.running.remove(&id);
        o.claimed.remove(&id);
    }

    // --- the edge trigger -------------------------------------------------------------------

    /// Acceptance: a healthy in-flight review does NOT re-fire each tick. Level-triggered, this is
    /// the F-DUP double dispatch — a second agent on the first one's detached worktree, and the
    /// first one's cancel handle lost.
    #[test]
    fn a_healthy_in_flight_review_does_not_re_fire() {
        let (mut o, dispatched) = orch(ticketless(&["alice", "bob"]));
        introduce(&o, row(12, "bob"));

        let first = o.handle_review_sweep(&[open_at(12, HEAD_A)]);
        assert_eq!(first.dispatched, 1);
        assert_eq!(watch_row(&o, 12, "bob").status, REVIEW_STATUS_IN_FLIGHT);

        // Three more ticks at the same head, with the review still running.
        for _ in 0..3 {
            let again = o.handle_review_sweep(&[open_at(12, HEAD_A)]);
            assert_eq!(again.dispatched, 0, "a live review was dispatched again");
        }
        assert_eq!(dispatched.lock().expect("lock").len(), 1);
    }

    /// Acceptance: a head advance fires EXACTLY ONE re-review — not one per tick.
    #[test]
    fn a_head_advance_fires_exactly_one_re_review() {
        let (mut o, dispatched) = orch(ticketless(&["alice", "bob"]));
        introduce(&o, row(12, "bob"));

        o.handle_review_sweep(&[open_at(12, HEAD_A)]);
        complete(&mut o, 12, "bob", HEAD_A);
        // Same head after the completion: nothing to do.
        assert_eq!(o.handle_review_sweep(&[open_at(12, HEAD_A)]).dispatched, 0);

        // The author pushes. One review of the new head, and only one however long it takes.
        assert_eq!(o.handle_review_sweep(&[open_at(12, HEAD_B)]).dispatched, 1);
        for _ in 0..3 {
            assert_eq!(o.handle_review_sweep(&[open_at(12, HEAD_B)]).dispatched, 0);
        }
        assert_eq!(dispatched.lock().expect("lock").len(), 2);
        assert_eq!(watch_row(&o, 12, "bob").requested_sha, HEAD_B);
    }

    // --- STUDIO-960: a head move that carried no new work -------------------------------------

    /// Acceptance, named after the case that produces most of these head moves: a
    /// `gh pr update-branch` rewrites every SHA while re-introducing the same change, so the diff
    /// the reviewer already approved is byte-identical. It must arm NOBODY and carry the approval
    /// forward to the new head — not discard an approval the diff still justifies and bill a whole
    /// round.
    #[tokio::test]
    async fn an_update_branch_arms_no_round_and_carries_the_approval() {
        let (mut o, dispatched) = orch(ticketless(&["alice", "bob"]));
        introduce(&o, row(12, "bob"));
        o.handle_review_sweep(&[open_at(12, HEAD_A)]);
        approve(&mut o, 12, "bob", HEAD_A);
        let before = dispatched.lock().expect("lock").len();
        assert_eq!(
            before, 1,
            "the round under test is the one already dispatched"
        );

        // The watcher compared the two diffs off-loop and proved them identical. Driven through the
        // REAL comparison, so a helper that reported "unchanged" (or "changed") unconditionally
        // turns this red rather than being bypassed by a hand-written proof.
        let signal = CancelSignal::new();
        let proven = unchanged_reviewed_shas(
            &signal.wait(),
            &FakeDiffSource::same(),
            &coord(12),
            HEAD_B,
            &[HEAD_A.to_string()],
        )
        .await;
        assert_eq!(proven, vec![HEAD_A.to_string()]);
        let report = o.handle_review_sweep(&[open_at_proven(12, HEAD_B, &proven)]);

        assert_eq!(report.dispatched, 0, "an unchanged diff must arm nobody");
        assert_eq!(report.armed, 0);
        assert_eq!(
            report.skipped, 1,
            "the round that did not happen is reported"
        );
        assert_eq!(
            dispatched.lock().expect("lock").len(),
            before,
            "no second agent was spawned"
        );
        let row = watch_row(&o, 12, "bob");
        assert_eq!(row.status, REVIEW_STATUS_APPROVED);
        assert_eq!(
            row.last_reviewed_sha, HEAD_B,
            "the verdict moved to the new head"
        );
        assert_eq!(
            crate::automerge::auto_merge_verdict(&[&row], HEAD_B),
            Ok(vec!["bob".to_string()]),
            "the approval is valid AT THE NEW HEAD, which is what carrying it forward means"
        );

        // And it stays carried: the next tick at the same head has nothing to do.
        assert_eq!(o.handle_review_sweep(&[open_at(12, HEAD_B)]).dispatched, 0);
    }

    /// Acceptance, the dangerous direction: a head move whose diff CHANGED is real work and a
    /// normal round applies. The watcher proves nothing here, so the re-arm happens exactly as
    /// before — and an implementation that carried approvals regardless of the diff would skip this
    /// and red.
    #[test]
    fn a_head_move_that_changed_the_diff_arms_a_normal_round() {
        let (mut o, dispatched) = orch(ticketless(&["alice", "bob"]));
        introduce(&o, row(12, "bob"));
        o.handle_review_sweep(&[open_at(12, HEAD_A)]);
        approve(&mut o, 12, "bob", HEAD_A);

        let report = o.handle_review_sweep(&[open_at(12, HEAD_B)]);

        assert_eq!(report.dispatched, 1, "a changed diff is a fresh round");
        assert_eq!(report.skipped, 0);
        assert_eq!(dispatched.lock().expect("lock").len(), 2);
        assert_eq!(watch_row(&o, 12, "bob").requested_sha, HEAD_B);
    }

    /// Acceptance: a rebase that RESOLVED A CONFLICT is new SHAs *and* a changed diff. It must arm
    /// a normal round, never a skip — the case the "always report unchanged" mutation is required
    /// to turn red.
    #[tokio::test]
    async fn a_conflict_resolving_rebase_arms_a_round_not_a_skip() {
        let (mut o, dispatched) = orch(ticketless(&["alice", "bob"]));
        introduce(&o, row(12, "bob"));
        o.handle_review_sweep(&[open_at(12, HEAD_A)]);
        approve(&mut o, 12, "bob", HEAD_A);

        // The watcher's comparison ran and found the conflict in the patch text, so it proved
        // nothing — `unchanged_from` is empty, which is the whole distinction from the rebase above.
        let signal = CancelSignal::new();
        let proven = unchanged_reviewed_shas(
            &signal.wait(),
            &FakeDiffSource::changed(),
            &coord(12),
            HEAD_B,
            &[HEAD_A.to_string()],
        )
        .await;
        assert!(
            proven.is_empty(),
            "a conflict resolution is not a content-preserving move"
        );

        let report = o.handle_review_sweep(&[open_at_proven(12, HEAD_B, &proven)]);
        assert_eq!(report.dispatched, 1, "a conflict resolution is real work");
        assert_eq!(report.skipped, 0);
        assert_eq!(dispatched.lock().expect("lock").len(), 2);
    }

    /// Acceptance: a failed or timed-out comparison degrades to arming a NORMAL round, never to
    /// silently skipping one.
    #[tokio::test]
    async fn a_failed_comparison_arms_a_round() {
        let (mut o, dispatched) = orch(ticketless(&["alice", "bob"]));
        introduce(&o, row(12, "bob"));
        o.handle_review_sweep(&[open_at(12, HEAD_A)]);
        approve(&mut o, 12, "bob", HEAD_A);

        let signal = CancelSignal::new();
        let proven = unchanged_reviewed_shas(
            &signal.wait(),
            &FakeDiffSource::base_fails(),
            &coord(12),
            HEAD_B,
            &[HEAD_A.to_string()],
        )
        .await;
        assert!(proven.is_empty(), "a read that failed proves nothing");

        let report = o.handle_review_sweep(&[open_at_proven(12, HEAD_B, &proven)]);
        assert_eq!(report.dispatched, 1);
        assert_eq!(report.skipped, 0);
        assert_eq!(dispatched.lock().expect("lock").len(), 2);
    }

    /// The skip is confined to a row that COMPLETED a round. A `truncated` row read the head only
    /// partially and owes a full round of the new head however identical the diff is — carrying it
    /// forward would ship a partial read as a verdict. The same holds for a crashed `in_flight`.
    #[test]
    fn only_a_completed_round_is_carried_across_an_unchanged_head_move() {
        for status in [REVIEW_STATUS_TRUNCATED, REVIEW_STATUS_IN_FLIGHT] {
            let (mut o, dispatched) = orch(ticketless(&["alice", "bob"]));
            introduce(&o, row(12, "bob"));
            o.handle_review_sweep(&[open_at(12, HEAD_A)]);
            o.store()
                .mark_review_completed(&key(12, "bob"), HEAD_A, status)
                .expect("mark");
            o.running.remove(&review_key(OWNER, REPO, 12, "bob"));
            o.claimed.remove(&review_key(OWNER, REPO, 12, "bob"));

            let report =
                o.handle_review_sweep(&[open_at_proven(12, HEAD_B, &[HEAD_A.to_string()])]);
            assert_eq!(
                report.dispatched, 1,
                "({status}) a partial round still owes one"
            );
            assert_eq!(report.skipped, 0, "({status})");
            assert_eq!(dispatched.lock().expect("lock").len(), 2, "({status})");
        }
    }

    /// Acceptance: a crashed review re-surfaces WITHOUT a daemon restart. The exit path leaves the
    /// `in_flight` marker in place on purpose; "no live run for an in-flight row" is what clears it.
    #[test]
    fn a_crashed_review_re_surfaces_without_a_restart() {
        let (mut o, _d) = orch(ticketless(&["alice", "bob"]));
        introduce(&o, row(12, "bob"));

        o.handle_review_sweep(&[open_at(12, HEAD_A)]);
        crash(&mut o, 12, "bob");
        assert_eq!(
            watch_row(&o, 12, "bob").status,
            REVIEW_STATUS_IN_FLIGHT,
            "the crashed round's marker is the input this test is about"
        );

        // Same head, same tick cadence, no restart.
        assert_eq!(o.handle_review_sweep(&[open_at(12, HEAD_A)]).dispatched, 1);
        assert!(o.running.contains_key(&review_key(OWNER, REPO, 12, "bob")));
    }

    /// A review an operator STOPPED is not resurrected two minutes later. `stop_run` leaves the
    /// key in `claimed` when the (impossible for a `pr:` key) tracker move fails, which is exactly
    /// the "dead this session" suppression the edge trigger already reads — worth pinning, because
    /// the watcher is the first thing in the daemon that would re-dispatch on its own initiative.
    #[test]
    fn an_operator_stopped_review_is_not_re_dispatched() {
        let (mut o, dispatched) = orch(ticketless(&["alice", "bob"]));
        introduce(&o, row(12, "bob"));
        o.handle_review_sweep(&[open_at(12, HEAD_A)]);

        let id = review_key(OWNER, REPO, 12, "bob");
        o.running.remove(&id);
        o.claimed.insert(id); // what `stop_run` + a failed finalize leave behind

        for _ in 0..3 {
            assert_eq!(o.handle_review_sweep(&[open_at(12, HEAD_A)]).dispatched, 0);
        }
        assert_eq!(dispatched.lock().expect("lock").len(), 1);
    }

    /// STUDIO-949: a watch row whose ORIGIN ticket is currently held for a human dispatches no
    /// review, even though the row exists from an earlier round — a ticket labelled after an agent
    /// already flailed on it is the likeliest way the label is ever applied. The row is left armed,
    /// so a later label removal still gets the review it is owed.
    ///
    /// The fixture seeds the CURRENT-LABEL-only state (`note_human_label`, the state the selection
    /// pass produces for a candidate labelled while its run is still live) rather than `hold`, which
    /// feeds the reported subset too. That pins this gate to `labelled()`: seeding `hold` passed
    /// against either reader.
    ///
    /// MUTATION: delete the origin-hold gate from `service_review_pr` and the first assertion reds;
    /// read the reported `held()` set instead of `labelled()` and it reds too.
    #[test]
    fn a_watch_row_whose_origin_ticket_is_held_for_a_human_is_not_dispatched() {
        let (mut o, dispatched) = orch(ticketless(&["alice", "bob"]));
        introduce(&o, row(12, "bob")); // origin: `handoff:STUDIO-721`
        o.human_holds.note_human_label("STUDIO-721");

        let report = o.handle_review_sweep(&[open_at(12, HEAD_A)]);
        assert_eq!(
            report.dispatched, 0,
            "a held ticket's review must not dispatch"
        );
        assert!(dispatched.lock().expect("lock").is_empty());
        assert_eq!(
            watch_row(&o, 12, "bob").status,
            REVIEW_STATUS_REQUESTED,
            "the row must stay armed for a later label removal"
        );

        // The label comes off — the next selection pass clears the current hold set — so the row is
        // still owed, and now dispatches.
        o.human_holds.begin_pass(true);
        assert_eq!(o.handle_review_sweep(&[open_at(12, HEAD_A)]).dispatched, 1);
    }

    /// ⚠️ STUDIO-949 round 13: the SAME un-primed latch fail-closes the ROUND gate ten lines above
    /// the auto-merge gate, on the same sweep. `dispatch_review` gates on a drain but on neither
    /// `validate()` nor `credential_preflight()`, both of which are on `on_tick`'s dispatch half
    /// only — so on a daemon whose config validation has failed since boot this sweep would
    /// otherwise dispatch a REAL review round at a held ticket's pull request for the whole life of
    /// the gate. Nothing is held here; the set is simply unknown.
    ///
    /// `a_watch_row_whose_origin_ticket_is_held_for_a_human_is_not_dispatched` is the live control:
    /// the same fixture through a primed ledger refuses for the held reason, and dispatches once the
    /// label is cleared.
    ///
    /// MUTATION: drop the `!ledger_primed` branch before the origin-hold check and this reds (a
    /// round is dispatched).
    #[test]
    fn an_unprimed_hold_ledger_refuses_the_review_round() {
        let (mut o, dispatched) = orch_before_first_pass(ticketless(&["alice", "bob"]));
        introduce(&o, row(12, "bob")); // nothing held — the label set is just unknown

        let report = o.handle_review_sweep(&[open_at(12, HEAD_A)]);
        assert_eq!(
            report.dispatched, 0,
            "with no pass having read the board, the hold set is unknown and no round may dispatch"
        );
        assert!(dispatched.lock().expect("lock").is_empty());
        assert_eq!(
            watch_row(&o, 12, "bob").status,
            REVIEW_STATUS_REQUESTED,
            "the row must stay armed for once the set is known"
        );

        // Once a pass has read the board the set is a real answer, and the row is dispatched.
        o.human_holds.begin_pass(true);
        assert_eq!(o.handle_review_sweep(&[open_at(12, HEAD_A)]).dispatched, 1);
    }

    /// Acceptance: a `max_turns`-truncated round is re-reviewed AT THE SAME HEAD. Nothing but the
    /// status distinguishes it from a completed one — `last_reviewed_sha` was deliberately not
    /// advanced — so a SHA-only trigger would call the partial review sufficient forever.
    #[test]
    fn a_truncated_round_is_re_reviewed_at_the_same_head() {
        let (mut o, _d) = orch(ticketless(&["alice", "bob"]));
        introduce(&o, row(12, "bob"));
        o.handle_review_sweep(&[open_at(12, HEAD_A)]);

        // The agent burned its budget: non-terminal, both SHAs untouched, run gone.
        o.store()
            .mark_review_truncated(&key(12, "bob"))
            .expect("truncate");
        crash(&mut o, 12, "bob");
        assert_eq!(watch_row(&o, 12, "bob").status, REVIEW_STATUS_TRUNCATED);

        assert_eq!(
            o.handle_review_sweep(&[open_at(12, HEAD_A)]).dispatched,
            1,
            "a partially-reviewed head must be reviewed again"
        );
    }

    /// The rule itself, driven through every state — the table a reader can check the prose against.
    #[test]
    fn the_edge_trigger_is_a_status_rule_not_a_sha_comparison() {
        let at = |status: &str, requested: &str, reviewed: &str| ReviewWatchRow {
            status: status.to_string(),
            requested_sha: requested.to_string(),
            last_reviewed_sha: reviewed.to_string(),
            ..row(12, "bob")
        };
        // Live review: never, whatever the row says.
        for status in [
            REVIEW_STATUS_REQUESTED,
            REVIEW_STATUS_IN_FLIGHT,
            REVIEW_STATUS_TRUNCATED,
            REVIEW_STATUS_REVIEWED,
        ] {
            assert!(
                !review_round_due(&at(status, HEAD_B, ""), HEAD_B, true),
                "{status}"
            );
        }
        // Not live: reviewed/approved pause at the head they read and re-arm past it…
        assert!(!review_round_due(
            &at(REVIEW_STATUS_REVIEWED, HEAD_A, HEAD_A),
            HEAD_A,
            false
        ));
        assert!(review_round_due(
            &at(REVIEW_STATUS_REVIEWED, HEAD_A, HEAD_A),
            HEAD_B,
            false
        ));
        assert!(!review_round_due(
            &at(REVIEW_STATUS_APPROVED, HEAD_A, HEAD_A),
            HEAD_A,
            false
        ));
        assert!(review_round_due(
            &at(REVIEW_STATUS_APPROVED, HEAD_A, HEAD_A),
            HEAD_B,
            false
        ));
        // …and the three unfinished states owe a review of the SAME head.
        assert!(review_round_due(
            &at(REVIEW_STATUS_REQUESTED, "", ""),
            HEAD_A,
            false
        ));
        assert!(review_round_due(
            &at(REVIEW_STATUS_IN_FLIGHT, HEAD_A, ""),
            HEAD_A,
            false
        ));
        assert!(review_round_due(
            &at(REVIEW_STATUS_TRUNCATED, HEAD_A, ""),
            HEAD_A,
            false
        ));
        // A row that left the watch set is never due, and neither is a headless observation.
        let dropped = ReviewWatchRow {
            open: false,
            ..at(REVIEW_STATUS_DROPPED, HEAD_A, HEAD_A)
        };
        assert!(!review_round_due(&dropped, HEAD_B, false));
        assert!(!review_round_due(
            &at(REVIEW_STATUS_REQUESTED, "", ""),
            "",
            false
        ));
    }

    // --- draft pokes (STUDIO-962) -----------------------------------------------------------

    /// One observation of an OPEN, DRAFT pull request at `head`.
    fn draft_at(number: i64, head: &str) -> PrObservation {
        observed(
            number,
            PrLookup::Found(PrSnapshot {
                is_draft: Some(true),
                head_sha: head.to_string(),
                status: PrStatus::Open,
                merged_at: None,
                head_repo: format!("{OWNER}/{REPO}"),
                // An UNSETTLED read (STUDIO-961): it decides no conflict and forgets none, which
                // is what these draft fixtures want from a field they do not exercise.
                merge_state: String::new(),
            }),
        )
    }

    /// One observation of an OPEN pull request at `head` whose payload carried NO boolean
    /// `isDraft` — GitHub did not say whether it is a draft ([`PrSnapshot::is_draft`]).
    fn unstated_at(number: i64, head: &str) -> PrObservation {
        observed(
            number,
            PrLookup::Found(PrSnapshot {
                is_draft: None,
                head_sha: head.to_string(),
                status: PrStatus::Open,
                merged_at: None,
                head_repo: format!("{OWNER}/{REPO}"),
                // An UNSETTLED read (STUDIO-961): it decides no conflict and forgets none, which
                // is what these draft fixtures want from a field they do not exercise.
                merge_state: String::new(),
            }),
        )
    }

    /// The origin ticket with a run in flight RIGHT NOW — the mid-run shape a draft is normal in.
    fn live_author_run(o: &mut Orchestrator, identifier: &str) {
        let iss = rhapsody_core::Issue {
            id: format!("ID-{identifier}"),
            identifier: identifier.to_string(),
            ..Default::default()
        };
        o.running.insert(iss.id.clone(), RunningEntry::empty(iss));
    }

    /// The heads a report's POKES named, in order. An escalation contributes nothing.
    fn poked_heads(report: &ReviewSweepReport) -> Vec<String> {
        report
            .nudges
            .iter()
            .filter_map(|n| match n {
                crate::draftpoke::DraftNudge::Poke(p) => Some(p.head.clone()),
                crate::draftpoke::DraftNudge::Escalate(_) => None,
            })
            .collect()
    }

    /// The poke state for pull request `number`, if any.
    fn poke_state(o: &Orchestrator, number: i64) -> Option<crate::draftpoke::DraftPokeState> {
        o.draft_pokes.get(&churn_key(&coord(number))).cloned()
    }

    /// The fixed instant the wall-clock draft tests anchor on.
    fn draft_clock() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339("2026-09-14T21:20:00Z")
            .expect("test instant")
            .with_timezone(&chrono::Utc)
    }

    /// One sweep with the control clock pinned at `at`. The unanswered-draft bound is WALL CLOCK
    /// now, so a test advances time by moving the clock between sweeps — nothing in a sweep knows
    /// the tick's cadence, which is the point.
    fn sweep_at(
        o: &mut Orchestrator,
        at: chrono::DateTime<chrono::Utc>,
        observed: &[PrObservation],
    ) -> ReviewSweepReport {
        o.now = Box::new(move || at);
        o.handle_review_sweep(observed)
    }

    /// Acceptance: a finished run's still-draft pull request produces ONE summons naming the pull
    /// request, its author and the head — the run is finished because the row exists (it was
    /// handoff-introduced), and the poke is what asks the author to publish it.
    #[test]
    fn a_finished_draft_pull_request_pokes_its_author_once() {
        let (mut o, _d) = orch(ticketless(&["alice", "bob"]));
        introduce(&o, row(12, "bob")); // introduced_by: handoff:STUDIO-721

        let report = o.handle_review_sweep(&[draft_at(12, HEAD_A)]);

        assert_eq!(report.nudges.len(), 1, "{:?}", report.nudges);
        match &report.nudges[0] {
            crate::draftpoke::DraftNudge::Poke(p) => {
                assert_eq!(p.pr, coord(12));
                assert_eq!(p.head, HEAD_A);
                assert_eq!(p.author, "alice");
                assert_eq!(p.summon_token, "@symphony");
                assert_eq!(p.pokes, 0, "the first poke");
            }
            other => panic!("expected a poke, got {other:?}"),
        }
        assert_eq!(
            poke_state(&o, 12).map(|s| (s.poked_head, s.pokes)),
            Some((HEAD_A.to_string(), 1))
        );
    }

    /// ⚠️ Acceptance: no poke while the author's run is still going. A draft is entirely normal
    /// mid-run; the trigger is run FINISHED and still draft, and this is the guard that says so.
    #[test]
    fn a_draft_is_not_poked_while_the_authors_run_is_live() {
        let (mut o, _d) = orch(ticketless(&["alice", "bob"]));
        introduce(&o, row(12, "bob"));
        live_author_run(&mut o, "STUDIO-721"); // `row`'s origin

        let report = o.handle_review_sweep(&[draft_at(12, HEAD_A)]);

        assert!(report.nudges.is_empty(), "{:?}", report.nudges);
        assert!(
            poke_state(&o, 12).is_none(),
            "a live run must not even record a poke"
        );
    }

    /// ⚠️ Acceptance: ONE poke per head, across many ticks with the state unchanged. The draft
    /// persists until the author acts, so a per-tick summons would be a re-dispatch loop.
    #[test]
    fn a_draft_is_poked_once_per_head_not_once_per_tick() {
        let (mut o, _d) = orch(ticketless(&["alice", "bob"]));
        introduce(&o, row(12, "bob"));

        let first = o.handle_review_sweep(&[draft_at(12, HEAD_A)]);
        assert_eq!(poked_heads(&first), vec![HEAD_A.to_string()]);
        for _ in 0..5 {
            let again = o.handle_review_sweep(&[draft_at(12, HEAD_A)]);
            assert!(
                again.nudges.is_empty(),
                "the same head must never be poked twice consecutively"
            );
        }

        // The author pushes but leaves it a draft: the new head is a new poke, exactly once.
        let moved = o.handle_review_sweep(&[draft_at(12, HEAD_B)]);
        assert_eq!(poked_heads(&moved), vec![HEAD_B.to_string()]);
        assert!(
            o.handle_review_sweep(&[draft_at(12, HEAD_B)])
                .nudges
                .is_empty()
        );
    }

    /// ⚠️ Acceptance (alice's round-3 blocker): the unanswered window is per-head, so a pushed head
    /// RESTARTS it. Without the restart, the window carried across a push would escalate an author
    /// who demonstrably just acted — at poke 2 of 3, not the `MAX_DRAFT_POKES` the distinct-head
    /// axis promises.
    #[test]
    fn a_new_head_restarts_the_unanswered_window() {
        let (mut o, _d) = orch(ticketless(&["alice", "bob"]));
        introduce(&o, row(12, "bob"));
        let base = draft_clock();
        let half = crate::draftpoke::MAX_DRAFT_POKE_UNANSWERED / 2;

        // Poke HEAD_A: the window opens at the poke.
        assert_eq!(
            poked_heads(&sweep_at(&mut o, base, &[draft_at(12, HEAD_A)])),
            vec![HEAD_A.to_string()]
        );
        assert_eq!(
            poke_state(&o, 12).and_then(|s| s.unanswered_since),
            Some(base),
            "the window opens at the poke"
        );
        // Sit at it for half the window — far enough that a carried window would cross the bound
        // soon after a push, but never crossing it at A.
        let half_way = sweep_at(&mut o, base + half, &[draft_at(12, HEAD_A)]);
        assert!(half_way.nudges.is_empty(), "{:?}", half_way.nudges);

        // The author pushes but leaves it a draft: the new head is a fresh poke AND a fresh window.
        let pushed_at = base + half;
        let moved = sweep_at(&mut o, pushed_at, &[draft_at(12, HEAD_B)]);
        assert_eq!(poked_heads(&moved), vec![HEAD_B.to_string()]);
        assert_eq!(
            poke_state(&o, 12).map(|s| (s.pokes, s.unanswered_since)),
            Some((2, Some(pushed_at))),
            "a pushed head reopens the unanswered window"
        );
        // And the new head gets the WHOLE window: a carried window would escalate on the next tick.
        let early = sweep_at(&mut o, pushed_at + half, &[draft_at(12, HEAD_B)]);
        assert!(
            early.nudges.is_empty(),
            "the window restarted at the new head: {:?}",
            early.nudges
        );
    }

    /// ⚠️ Acceptance (jimmy's round-1 blocker): a draft IGNORED at a static head — the shape
    /// booch#537 actually had — escalates to a human instead of parking in silence forever. The
    /// distinct-head ceiling alone poked once and then heard from nobody, so this pins the SECOND
    /// bound: the same head still draft after `MAX_DRAFT_POKE_UNANSWERED` of wall clock.
    #[test]
    fn a_static_draft_head_escalates_to_a_human_after_a_bounded_silence() {
        let (mut o, _d) = orch(ticketless(&["alice", "bob"]));
        introduce(&o, row(12, "bob"));
        let base = draft_clock();

        // The one poke, at the head the author never moves.
        assert_eq!(
            poked_heads(&sweep_at(&mut o, base, &[draft_at(12, HEAD_A)])),
            vec![HEAD_A.to_string()]
        );
        // The poke is unanswered and the head does not move: silence right up to the grace, then a
        // human. The tick one second short is the boundary that says the bound is the GRACE and not
        // the first sweep that happens to run after it.
        let just_short =
            base + crate::draftpoke::MAX_DRAFT_POKE_UNANSWERED - chrono::Duration::seconds(1);
        assert!(
            sweep_at(&mut o, just_short, &[draft_at(12, HEAD_A)])
                .nudges
                .is_empty(),
            "one second short of the grace is still silence"
        );
        let report = sweep_at(
            &mut o,
            base + crate::draftpoke::MAX_DRAFT_POKE_UNANSWERED,
            &[draft_at(12, HEAD_A)],
        );
        assert_eq!(report.nudges.len(), 1, "{:?}", report.nudges);
        match &report.nudges[0] {
            crate::draftpoke::DraftNudge::Escalate(e) => {
                assert_eq!(e.pr, coord(12));
                assert_eq!(e.identifier, "STUDIO-721", "the origin ticket is named");
                assert_eq!(e.author, "alice");
                assert_eq!(e.pokes, 1, "poked once; the head never moved");
                // `pokes == 1` is the PRIMARY shape of this axis, not an edge case — it is the only
                // count a static head can produce — so its singular render is pinned here as a
                // PHRASE. The trailing `to` discriminates: the plural form renders "made 1 attempts
                // to", which does not contain "made 1 attempt to".
                assert!(
                    crate::draftpoke::escalation_body(e).contains("made 1 attempt to"),
                    "the singular count is named: {}",
                    crate::draftpoke::escalation_body(e)
                );
            }
            other => panic!("expected an escalation, got {other:?}"),
        }
        // And once a human holds it, the static head stays quiet forever rather than re-poking —
        // however much further the wall clock runs.
        let much_later = base + chrono::Duration::hours(5);
        for _ in 0..5 {
            assert!(
                sweep_at(&mut o, much_later, &[draft_at(12, HEAD_A)])
                    .nudges
                    .is_empty()
            );
        }
    }

    /// ⚠️ Acceptance (jimmy's round-6 blocker): the unanswered bound is WALL CLOCK, so the grace an
    /// ignored draft gets does not depend on the watcher's cadence. The old sweep count gave a
    /// static head 30 sweeps, which was about an hour at the pinned 120s and about EIGHT MINUTES
    /// once STUDIO-974 made the cadence a hot-reloadable key defaulting to 15s. This drives the
    /// same wall clock at two cadences and asserts the escalation lands at the same grace either
    /// way — at 15s it takes ~240 sweeps, at 120s ~30, and both are one hour.
    #[test]
    fn the_unanswered_draft_bound_is_wall_clock_not_sweeps() {
        let grace = crate::draftpoke::MAX_DRAFT_POKE_UNANSWERED;
        for cadence_secs in [15i64, 120] {
            let (mut o, _d) = orch(ticketless(&["alice", "bob"]));
            introduce(&o, row(12, "bob"));
            let base = draft_clock();

            // The one poke at t0.
            assert_eq!(
                poked_heads(&sweep_at(&mut o, base, &[draft_at(12, HEAD_A)])),
                vec![HEAD_A.to_string()]
            );

            // Then observed, unanswered, one tick per `cadence_secs`. Escalation must land once the
            // WALL CLOCK crosses the grace — not after a fixed number of ticks.
            let mut elapsed = chrono::Duration::zero();
            let mut escalated_at = None;
            // Two ticks past the grace is plenty at either cadence.
            let ticks = grace.num_seconds() / cadence_secs + 2;
            for _ in 0..ticks {
                elapsed += chrono::Duration::seconds(cadence_secs);
                let report = sweep_at(&mut o, base + elapsed, &[draft_at(12, HEAD_A)]);
                if report
                    .nudges
                    .iter()
                    .any(|n| matches!(n, crate::draftpoke::DraftNudge::Escalate(_)))
                {
                    escalated_at = Some(elapsed);
                    break;
                }
            }
            let escalated_at =
                escalated_at.unwrap_or_else(|| panic!("cadence {cadence_secs}s: never escalated"));
            assert!(
                escalated_at >= grace,
                "cadence {cadence_secs}s: escalated before the wall-clock grace ({escalated_at:?})"
            );
            assert!(
                escalated_at < grace + chrono::Duration::seconds(cadence_secs),
                "cadence {cadence_secs}s: escalated more than one tick past the grace \
                 ({escalated_at:?})"
            );
        }
    }

    /// ⚠️ Acceptance (jimmy's round-6 blocker, the case named in the review): a live author run
    /// re-anchors the unanswered window, so a long re-engaged run does not ESCALATE the author it
    /// re-engaged. The window is for an author who has stopped, not one mid-fix. The old sweep
    /// count had this property by construction — it never advanced while a run was live — and a
    /// wall clock has to re-anchor explicitly.
    ///
    /// Mutation check: delete the re-anchor in `plan_draft_poke`'s live-run branch and this reds
    /// with an escalation when the run ends.
    #[test]
    fn a_live_author_run_does_not_spend_the_unanswered_grace() {
        let (mut o, _d) = orch(ticketless(&["alice", "bob"]));
        introduce(&o, row(12, "bob"));
        let base = draft_clock();

        // Poke, then an hour passes with the author's run live: no escalation, and the window is
        // pushed forward to the last tick the run was still working.
        assert_eq!(
            poked_heads(&sweep_at(&mut o, base, &[draft_at(12, HEAD_A)])),
            vec![HEAD_A.to_string()]
        );
        live_author_run(&mut o, "STUDIO-721");
        let during = base + crate::draftpoke::MAX_DRAFT_POKE_UNANSWERED;
        let live = sweep_at(&mut o, during, &[draft_at(12, HEAD_A)]);
        assert!(live.nudges.is_empty(), "{:?}", live.nudges);
        assert_eq!(
            poke_state(&o, 12).and_then(|s| s.unanswered_since),
            Some(during),
            "the live run re-anchors the window"
        );

        // The run ends and the draft is STILL there, but the author only just stopped: the full
        // grace is theirs, so the very next tick is not an escalation. Without the re-anchor the
        // window would already be an hour old here and this would escalate.
        o.running.clear();
        let just_after = sweep_at(
            &mut o,
            during + chrono::Duration::minutes(1),
            &[draft_at(12, HEAD_A)],
        );
        assert!(
            just_after.nudges.is_empty(),
            "a just-finished run must get its full grace: {:?}",
            just_after.nudges
        );
    }

    /// ⚠️ Acceptance (alice's round-2 blocker): an UNSTATED `isDraft` is not a published draft. A
    /// pull request already handed to a human must survive a tick GitHub could not answer — if the
    /// state-clearing branch took `None` as "resolved" it would drop the ledger, including
    /// `escalated`, and the next `Some(true)` tick would restart the whole poke cycle at the same
    /// head, re-summoning a run a human was just told to take over.
    #[test]
    fn an_unstated_draft_does_not_forget_the_poke_ledger() {
        let (mut o, _d) = orch(ticketless(&["alice", "bob"]));
        introduce(&o, row(12, "bob"));
        let base = draft_clock();

        // Poke the static head, then let the wall clock reach the human escalation.
        sweep_at(&mut o, base, &[draft_at(12, HEAD_A)]);
        assert!(matches!(
            sweep_at(
                &mut o,
                base + crate::draftpoke::MAX_DRAFT_POKE_UNANSWERED,
                &[draft_at(12, HEAD_A)],
            )
            .nudges
            .first(),
            Some(crate::draftpoke::DraftNudge::Escalate(_))
        ));

        // GitHub does not say: no answer is not an answer, and the ledger — and the escalation —
        // stands. Nothing is poked, and the state is not dropped.
        let unstated = sweep_at(
            &mut o,
            base + crate::draftpoke::MAX_DRAFT_POKE_UNANSWERED + chrono::Duration::hours(1),
            &[unstated_at(12, HEAD_A)],
        );
        assert!(unstated.nudges.is_empty(), "{:?}", unstated.nudges);
        assert_eq!(
            poke_state(&o, 12).map(|s| s.escalated),
            Some(true),
            "an unstated answer must not erase the escalation"
        );

        // And the next tick that positively says "still a draft" stays silent rather than reopening
        // as a fresh "poke 1 of at most 3".
        let again = o.handle_review_sweep(&[draft_at(12, HEAD_A)]);
        assert!(again.nudges.is_empty(), "{:?}", again.nudges);
    }

    /// ⚠️ Acceptance (sol's round-5/6 blocker): an UNSTATED `isDraft` is never ACTED ON either. The
    /// test above drives its `None` tick after the escalation has latched, so it returns on the
    /// `escalated` branch and never reaches the unstated guard; this one drives a FRESH row, where
    /// that guard is the only thing standing between "GitHub did not say" and a summons.
    ///
    /// Two halves, and both are load-bearing. The `None` tick must poke NOTHING and record nothing —
    /// a recorded poke on an unstated answer is the "act on a guess" failure the `Option<bool>`
    /// refactor exists to prevent, and it would also spend a poke of the bounded budget. The
    /// `Some(true)` tick that follows must then poke normally, which is what says the guard SKIPPED
    /// the tick rather than retiring the pull request from poking altogether.
    #[test]
    fn an_unstated_draft_on_a_fresh_pull_request_is_never_poked() {
        let (mut o, _d) = orch(ticketless(&["alice", "bob"]));
        introduce(&o, row(12, "bob"));

        // GitHub did not say, and nothing has been poked yet: no summons, and no ledger at all.
        let unstated = o.handle_review_sweep(&[unstated_at(12, HEAD_A)]);
        assert!(
            unstated.nudges.is_empty(),
            "an answer GitHub never gave must not summon anyone: {:?}",
            unstated.nudges
        );
        assert_eq!(
            poke_state(&o, 12),
            None,
            "an unstated answer must not record a poke either"
        );

        // And the tick that positively says "still a draft" pokes normally — the guard skipped a
        // tick, it did not retire the pull request from poking.
        let stated = o.handle_review_sweep(&[draft_at(12, HEAD_A)]);
        assert_eq!(
            poked_heads(&stated),
            vec![HEAD_A.to_string()],
            "the first STATED draft is poked exactly once: {:?}",
            stated.nudges
        );
        assert_eq!(
            poke_state(&o, 12).map(|s| (s.poked_head, s.pokes)),
            Some((HEAD_A.to_string(), 1)),
            "and it is the FIRST poke: the unstated tick spent none of the budget"
        );
    }

    /// Acceptance: a draft ignored across every head escalates to a human rather than poking
    /// forever, and names how many times it was poked.
    #[test]
    fn a_draft_ignored_across_every_head_escalates_to_a_human() {
        let (mut o, _d) = orch(ticketless(&["alice", "bob"]));
        introduce(&o, row(12, "bob"));
        const HEAD_D: &str = "dddddddddddddddddddddddddddddddddddddddd";

        for (n, head) in [HEAD_A, HEAD_B, HEAD_C].into_iter().enumerate() {
            let report = o.handle_review_sweep(&[draft_at(12, head)]);
            assert_eq!(
                poked_heads(&report),
                vec![head.to_string()],
                "poke {}",
                n + 1
            );
        }
        // The next distinct head is not poked: the ceiling is reached and a human is asked instead.
        let report = o.handle_review_sweep(&[draft_at(12, HEAD_D)]);
        assert_eq!(report.nudges.len(), 1, "{:?}", report.nudges);
        match &report.nudges[0] {
            crate::draftpoke::DraftNudge::Escalate(e) => {
                assert_eq!(e.pr, coord(12));
                assert_eq!(e.identifier, "STUDIO-721", "the origin ticket is named");
                assert_eq!(e.author, "alice");
                assert_eq!(e.pokes, crate::draftpoke::MAX_DRAFT_POKES);
            }
            other => panic!("expected an escalation, got {other:?}"),
        }
        // Once handed to a human it stays quiet — at any head, for as long as the draft persists.
        for _ in 0..3 {
            assert!(
                o.handle_review_sweep(&[draft_at(12, HEAD_D)])
                    .nudges
                    .is_empty()
            );
            assert!(
                o.handle_review_sweep(&[draft_at(12, HEAD_A)])
                    .nudges
                    .is_empty()
            );
        }
    }

    /// ⚠️ `MAX_DRAFT_POKES` counts ATTEMPTS, not distinct heads (alice's round-5 finding): the
    /// ledger remembers only the head poked LAST, so a force-push back to an earlier head is a fresh
    /// poke and `A → B → A` spends the whole budget on two distinct heads. This is the test the four
    /// doc sites that used to say "distinct heads" were corrected to describe; without it the
    /// "returning to an earlier head is a fresh poke" sentence is a claim nothing drives.
    #[test]
    fn a_force_push_back_to_an_earlier_head_is_a_fresh_poke() {
        let (mut o, _d) = orch(ticketless(&["alice", "bob"]));
        introduce(&o, row(12, "bob"));

        // A → B → A: each move is a new head and so a new poke, even though the last one returns to
        // a head already poked. Only TWO distinct heads are involved.
        assert_eq!(
            poked_heads(&o.handle_review_sweep(&[draft_at(12, HEAD_A)])),
            vec![HEAD_A.to_string()]
        );
        assert_eq!(
            poked_heads(&o.handle_review_sweep(&[draft_at(12, HEAD_B)])),
            vec![HEAD_B.to_string()]
        );
        let back = o.handle_review_sweep(&[draft_at(12, HEAD_A)]);
        assert_eq!(
            poked_heads(&back),
            vec![HEAD_A.to_string()],
            "returning to an earlier head is a fresh poke"
        );
        assert_eq!(
            poke_state(&o, 12).map(|s| (s.poked_head, s.pokes)),
            Some((HEAD_A.to_string(), 3)),
            "three ATTEMPTS across two distinct heads"
        );

        // The budget is now spent: the NEXT head escalates rather than being poked a fourth time.
        let report = o.handle_review_sweep(&[draft_at(12, HEAD_C)]);
        assert_eq!(report.nudges.len(), 1, "{:?}", report.nudges);
        match &report.nudges[0] {
            crate::draftpoke::DraftNudge::Escalate(e) => {
                assert_eq!(e.pokes, crate::draftpoke::MAX_DRAFT_POKES);
            }
            other => panic!("expected an escalation, got {other:?}"),
        }
    }

    /// A pull request the operator introduced names no ticket, so the summon token could re-engage
    /// nobody: it is never poked. The watch set can hold such rows (`console:…`).
    #[test]
    fn a_console_introduced_pull_request_is_never_poked() {
        let (mut o, _d) = orch(ticketless(&["alice", "bob"]));
        introduce(
            &o,
            ReviewWatchRow {
                introduced_by: "console:operator".to_string(),
                ..row(12, "bob")
            },
        );

        assert!(
            o.handle_review_sweep(&[draft_at(12, HEAD_A)])
                .nudges
                .is_empty()
        );
    }

    /// Marking it ready resolves the draft: the bookkeeping is dropped, so a later re-draft starts
    /// afresh rather than inheriting a spent poke count.
    #[test]
    fn publishing_a_draft_clears_its_poke_state() {
        let (mut o, _d) = orch(ticketless(&["alice", "bob"]));
        introduce(&o, row(12, "bob"));

        o.handle_review_sweep(&[draft_at(12, HEAD_A)]);
        assert!(poke_state(&o, 12).is_some());

        let ready = o.handle_review_sweep(&[open_at(12, HEAD_A)]);
        assert!(ready.nudges.is_empty());
        assert!(
            poke_state(&o, 12).is_none(),
            "a published draft forgets its pokes"
        );
    }

    /// A pull request that leaves the watch set takes its poke bookkeeping with it, for the same
    /// reasons `review_rounds` does — and so it cannot keep a stale count against a re-introduction.
    #[test]
    fn retiring_a_pull_request_forgets_its_poke_state() {
        let (mut o, _d) = orch(ticketless(&["alice", "bob"]));
        introduce(&o, row(12, "bob"));
        o.handle_review_sweep(&[draft_at(12, HEAD_A)]);
        assert!(poke_state(&o, 12).is_some());

        assert_eq!(
            o.handle_review_sweep(&[observed(12, PrLookup::Gone)])
                .retired,
            1
        );
        assert!(poke_state(&o, 12).is_none());
    }

    // --- the drop terminal ------------------------------------------------------------------

    /// Acceptance: a merged, closed or gone pull request is dropped from the watch set — and so is
    /// one whose head this daemon is not entitled to read.
    #[test]
    fn a_retired_pull_request_leaves_the_watch_set() {
        let merged = PrLookup::Found(PrSnapshot {
            is_draft: Some(false),
            head_sha: HEAD_A.to_string(),
            status: PrStatus::Merged,
            merged_at: None,
            head_repo: format!("{OWNER}/{REPO}"),
            merge_state: String::new(),
        });
        let closed = PrLookup::Found(PrSnapshot {
            is_draft: Some(false),
            head_sha: HEAD_A.to_string(),
            status: PrStatus::Closed,
            merged_at: None,
            head_repo: format!("{OWNER}/{REPO}"),
            merge_state: String::new(),
        });
        for (n, lookup) in [
            (12, merged),
            (13, closed),
            (14, PrLookup::Gone),
            (15, PrLookup::Untrusted),
        ] {
            let (mut o, dispatched) = orch(ticketless(&["alice", "bob"]));
            introduce(&o, row(n, "bob"));

            let report = o.handle_review_sweep(&[observed(n, lookup.clone())]);

            assert_eq!(report.retired, 1, "pr #{n}");
            assert_eq!(report.dispatched, 0, "pr #{n}");
            let r = watch_row(&o, n, "bob");
            assert_eq!(r.status, REVIEW_STATUS_DROPPED, "pr #{n}");
            assert!(!r.open, "pr #{n}");
            assert!(dispatched.lock().expect("lock").is_empty(), "pr #{n}");
            // …and it is not polled again.
            assert!(o.review_watch_coords().is_empty(), "pr #{n}");
        }
    }

    /// Every reviewer of an N-reviewer pull request is dropped, not just the first — and dropping
    /// is idempotent, so a second observation of a merged PR is not a second retirement.
    #[test]
    fn retiring_drops_every_reviewer_once() {
        let (mut o, _d) = orch(ticketless(&["alice", "bob", "carol"]));
        introduce(&o, row(12, "bob"));
        introduce(&o, row(12, "carol"));

        assert_eq!(
            o.handle_review_sweep(&[observed(12, PrLookup::Gone)])
                .retired,
            2
        );
        assert_eq!(
            o.handle_review_sweep(&[observed(12, PrLookup::Gone)])
                .retired,
            0
        );
    }

    // --- auto-Done on merge (STUDIO-712) ----------------------------------------------------

    /// Acceptance: a ticket in a review state whose pull request MERGES is moved to the configured
    /// terminal state within one watch tick. The plan rides back on the report because the move
    /// itself is a tracker round-trip the control task must not make.
    #[test]
    fn a_merged_pull_request_finishes_its_implementation_ticket() {
        let (mut o, _d) = orch(ticketless_done(&["alice", "bob"], "Done"));
        introduce(&o, row(64, "bob"));
        run_of(&o, "STUDIO-721");

        let report = o.handle_review_sweep(&[observed(64, merged_at(HEAD_A))]);

        assert_eq!(
            report.done,
            vec![crate::reviewdone::ReviewDonePlan {
                pr: format!("{OWNER}/{REPO}#64"),
                issue_id: "ID-STUDIO-721".to_string(),
                team_id: "TEAM-1".to_string(),
                identifier: "STUDIO-721".to_string(),
                state: "Done".to_string(),
            }],
        );
        assert_eq!(report.retired, 1, "and the row still leaves the watch set");
    }

    /// Acceptance, the destructive half: a **closed-unmerged** pull request moves NOTHING. It is
    /// abandoned work, its ticket still needs a human, and auto-Cancelling it would destroy the one
    /// signal that says so. Neither does a pull request that is gone or whose head is untrusted.
    #[test]
    fn a_closed_unmerged_pull_request_finishes_nothing() {
        for (n, lookup) in [
            (64, closed_at(HEAD_A)),
            (65, PrLookup::Gone),
            (66, PrLookup::Untrusted),
        ] {
            let (mut o, _d) = orch(ticketless_done(&["alice", "bob"], "Done"));
            introduce(&o, row(n, "bob"));
            run_of(&o, "STUDIO-721");

            let report = o.handle_review_sweep(&[observed(n, lookup.clone())]);

            assert!(
                report.done.is_empty(),
                "pr #{n} ({lookup:?}) must move no ticket, and never auto-Cancel one"
            );
            assert_eq!(report.retired, 1, "pr #{n} still leaves the watch set");
        }
    }

    /// One merge does not finish the OTHER watched pull requests' tickets — the plan is built from
    /// the merged coordinate's own rows, out of a snapshot that holds every watched row.
    #[test]
    fn a_merge_finishes_only_its_own_ticket() {
        let (mut o, _d) = orch(ticketless_done(&["alice", "bob"], "Done"));
        introduce(&o, row(64, "bob"));
        let mut other = row(65, "bob");
        other.introduced_by = "handoff:STUDIO-999".to_string();
        introduce(&o, other);
        run_of(&o, "STUDIO-721");
        run_of(&o, "STUDIO-999");

        let report = o.handle_review_sweep(&[observed(64, merged_at(HEAD_A))]);

        assert_eq!(
            report
                .done
                .iter()
                .map(|p| p.identifier.clone())
                .collect::<Vec<_>>(),
            vec!["STUDIO-721".to_string()]
        );
    }

    /// The transition is a divergence and therefore OFF by default: the same merge on an
    /// installation that never named a terminal state retires the row and moves nothing.
    #[test]
    fn an_unnamed_done_state_moves_nothing_on_a_merge() {
        let (mut o, _d) = orch(ticketless(&["alice", "bob"]));
        introduce(&o, row(64, "bob"));
        run_of(&o, "STUDIO-721");

        let report = o.handle_review_sweep(&[observed(64, merged_at(HEAD_A))]);

        assert!(report.done.is_empty(), "off by default");
        assert_eq!(report.retired, 1);
    }

    // --- auto-merge on a cleared gate (STUDIO-874) ------------------------------------------

    /// [`ticketless`] with the auto-merge switched on.
    fn ticketless_automerge(names: &[&str]) -> Teams {
        let mut teams = ticketless(names);
        teams.review.auto_merge = true;
        teams
    }

    /// A watch row whose reviewer APPROVED the head they were asked about.
    fn approved_row(number: i64, reviewer: &str, sha: &str) -> ReviewWatchRow {
        let mut r = row(number, reviewer);
        r.requested_sha = sha.to_string();
        r.last_reviewed_sha = sha.to_string();
        r.status = rhapsody_store::REVIEW_STATUS_APPROVED.to_string();
        r
    }

    /// Acceptance: every reviewer approved AT the observed head, so the tick proposes the merge —
    /// and names the head those verdicts were recorded against, not merely the pull request.
    #[test]
    fn an_approved_pull_request_at_its_reviewed_head_is_proposed_for_merge() {
        let (mut o, _d) = orch(ticketless_automerge(&["alice", "bob"]));
        introduce(&o, approved_row(64, "bob", HEAD_A));

        let report = o.handle_review_sweep(&[open_at(64, HEAD_A)]);

        assert_eq!(
            report.merge,
            vec![crate::automerge::AutoMergePlan {
                pr: coord(64),
                head: HEAD_A.to_string(),
                approved_by: vec!["bob".to_string()],
            }]
        );
        assert_eq!(report.dispatched, 0, "and no review round is dispatched");
    }

    /// STUDIO-949 round 5: an approved, at-head pull request whose ORIGIN ticket is held for a human
    /// is not proposed for merge. The round gate refuses to dispatch a review against a held ticket;
    /// without this one, a pull request approved before the label landed would still merge, and the
    /// merge then moves the ticket to Done — the daemon finishing what only a person may do.
    /// `an_approved_pull_request_at_its_reviewed_head_is_proposed_for_merge` is the live control: the
    /// identical fixture without the hold proposes the plan.
    ///
    /// MUTATION: delete the `held_origin` gate from `service_review_pr` and this reds; read the
    /// reported `held()` set instead of `labelled()` and it reds too (the fixture seeds the
    /// current-label-only state, the live-labelled hold shape).
    #[test]
    fn a_held_origin_ticket_holds_back_auto_merge() {
        let (mut o, _d) = orch(ticketless_automerge(&["alice", "bob"]));
        introduce(&o, approved_row(64, "bob", HEAD_A)); // origin: `handoff:STUDIO-721`
        o.human_holds.note_human_label("STUDIO-721");

        let report = o.handle_review_sweep(&[open_at(64, HEAD_A)]);

        assert!(
            report.merge.is_empty(),
            "a held ticket's pull request must not self-merge: {:?}",
            report.merge
        );

        // The label comes off — the next selection pass clears the current hold set — and the merge
        // the reviewers already approved is proposed on the next tick.
        o.human_holds.begin_pass(true);
        assert_eq!(
            o.handle_review_sweep(&[open_at(64, HEAD_A)]).merge.len(),
            1,
            "once the hold is gone the approved merge is proposed"
        );
    }

    /// ⚠️ STUDIO-949 round 11: an approved, at-head pull request is NOT merged while the human-hold
    /// ledger has never been primed by a selection pass — even with nothing labelled at all. This is
    /// the failing-open direction: the current-label set has no writer above `on_tick`'s three early
    /// gates (a bad config, an armed drain, a dead credential), so on a daemon held by one of them it
    /// is empty for the whole process lifetime and the ordinary hold check would see "no hold" for a
    /// ticket that wears the label. An unknown set fails closed.
    ///
    /// `an_approved_pull_request_at_its_reviewed_head_is_proposed_for_merge` is the live control: it
    /// runs the SAME fixture through a primed daemon and proposes the plan.
    ///
    /// MUTATION: drop the un-primed (`!ledger_primed`) fail-closed branch in `service_review_pr` and this reds (a
    /// plan is proposed).
    #[test]
    fn an_unprimed_hold_ledger_refuses_auto_merge() {
        let (mut o, _d) = orch_before_first_pass(ticketless_automerge(&["alice", "bob"]));
        introduce(&o, approved_row(64, "bob", HEAD_A)); // nothing held — the label set is just unknown

        let report = o.handle_review_sweep(&[open_at(64, HEAD_A)]);

        assert!(
            report.merge.is_empty(),
            "with no pass having looked, the hold set is unknown and the merge must not run: {:?}",
            report.merge
        );

        // Once a pass has run the set is a real answer, and the same pull request merges.
        o.human_holds.begin_pass(true);
        assert_eq!(
            o.handle_review_sweep(&[open_at(64, HEAD_A)]).merge.len(),
            1,
            "a primed ledger merges the approved head"
        );
    }

    /// ⚠️ The D5 invariant and the opt-in, at the one place it decides anything: the SAME approved,
    /// at-head pull request proposes nothing when `auto_merge` was never asked for.
    #[test]
    fn auto_merge_is_off_by_default() {
        let (mut o, _d) = orch(ticketless(&["alice", "bob"]));
        introduce(&o, approved_row(64, "bob", HEAD_A));

        let report = o.handle_review_sweep(&[open_at(64, HEAD_A)]);

        assert!(report.merge.is_empty(), "off unless the operator asked");
    }

    /// ⚠️ STUDIO-927: the gate is scoped to the project that owns the pull request's repo. With the
    /// top-level flag ON, a `projects:` entry for THIS project setting `auto_merge: false` holds the
    /// merge back. Mutation: make `propose_auto_merge` gate on `Teams::review_auto_merge` again —
    /// the global wins, the plan is handed out, and this goes red.
    #[test]
    fn a_per_project_override_holds_back_this_projects_auto_merge() {
        let mut teams = ticketless_automerge(&["alice", "bob"]);
        teams.projects = vec![rhapsody_config::teams::TeamsProject {
            slugs: vec!["rhapsody".to_string()],
            review: rhapsody_config::teams::ProjectReview {
                auto_merge: Some(false),
            },
        }];
        let (mut o, _d) = orch(teams);
        introduce(&o, approved_row(64, "bob", HEAD_A));

        let report = o.handle_review_sweep(&[open_at(64, HEAD_A)]);

        assert!(
            report.merge.is_empty(),
            "the project this repo belongs to asked not to self-merge"
        );
    }

    /// ⚠️ STUDIO-927, the case that must not be missed: a WORKFLOW project fans
    /// out to one resolved project per slug, ALL sharing one repo, so an operator
    /// may name ANY of its slugs. Here the override names the SECOND slug of a
    /// two-slug project; the first has no entry. A first-match scan resolves the
    /// first slug, finds no entry and falls back to the global `true`, so the repo
    /// self-merges anyway — the bug alice found on the live install, where booch's
    /// two hex slugs share one repo. The AND over every owning slug is what holds
    /// it back. Mutation: return `review_auto_merge_for_repo` to a first-match
    /// `find_map` and the plan is handed out again, so this goes red.
    #[test]
    fn an_override_naming_a_fanned_projects_second_slug_still_holds_the_merge() {
        let mut teams = ticketless_automerge(&["alice", "bob"]);
        teams.projects = vec![rhapsody_config::teams::TeamsProject {
            slugs: vec!["161ac721c8bc".to_string()],
            review: rhapsody_config::teams::ProjectReview {
                auto_merge: Some(false),
            },
        }];
        let (mut o, _d) = orch(teams);
        // Fan the one repo out over two slugs, as `resolve_projects` does for
        // `slugs: [4f4a2350682f, 161ac721c8bc]`. The override names the second.
        let tracker = Arc::clone(&o.eff.as_ref().expect("eff").projects[0].tracker);
        let mut second = empty_resolved_project("161ac721c8bc", tracker);
        second.repo = REPO_URL.to_string();
        {
            let eff = o.eff.as_mut().expect("eff");
            eff.projects[0].slug = "4f4a2350682f".to_string();
            eff.projects[0].group = "4f4a2350682f".to_string();
            eff.projects.push(second);
        }
        introduce(&o, approved_row(64, "bob", HEAD_A));

        let report = o.handle_review_sweep(&[open_at(64, HEAD_A)]);

        assert!(
            report.merge.is_empty(),
            "naming the second slug of a fanned project must still hold the merge"
        );
    }

    /// ⚠️ The stale-approval refusal, end to end through the sweep: the reviewer approved HEAD_A
    /// and the author has since pushed HEAD_B. Nothing is proposed — and the row is re-armed for a
    /// review of the new head instead, which is the behaviour that makes the refusal temporary
    /// rather than a dead end.
    #[test]
    fn an_approval_that_predates_the_observed_head_proposes_no_merge() {
        let (mut o, _d) = orch(ticketless_automerge(&["alice", "bob"]));
        introduce(&o, approved_row(64, "bob", HEAD_A));

        let report = o.handle_review_sweep(&[open_at(64, HEAD_B)]);

        assert!(
            report.merge.is_empty(),
            "an approval of HEAD_A is not one of HEAD_B"
        );
        assert_eq!(report.armed, 1, "the new head is re-reviewed instead");
    }

    /// A round that filed findings blocks the merge, and so does one still owed — the two refusals
    /// the watch set can distinguish and a bare "is anything running" cannot.
    #[test]
    fn a_findings_round_or_an_owed_one_proposes_no_merge() {
        for status in [
            rhapsody_store::REVIEW_STATUS_REVIEWED,
            rhapsody_store::REVIEW_STATUS_REQUESTED,
            rhapsody_store::REVIEW_STATUS_IN_FLIGHT,
            rhapsody_store::REVIEW_STATUS_TRUNCATED,
        ] {
            let (mut o, _d) = orch(ticketless_automerge(&["alice", "bob"]));
            let mut r = approved_row(64, "bob", HEAD_A);
            r.status = status.to_string();
            introduce(&o, r);

            let report = o.handle_review_sweep(&[open_at(64, HEAD_A)]);

            assert!(report.merge.is_empty(), "({status})");
        }
    }

    /// Every REQUIRED reviewer, not merely one of them: a second row that has not approved this
    /// head blocks the merge the first row's approval would otherwise clear.
    #[test]
    fn one_reviewers_approval_does_not_merge_a_two_reviewer_pull_request() {
        let (mut o, _d) = orch(ticketless_automerge(&["alice", "bob", "carol"]));
        introduce(&o, approved_row(64, "bob", HEAD_A));
        introduce(&o, row(64, "carol"));

        let report = o.handle_review_sweep(&[open_at(64, HEAD_A)]);

        assert!(report.merge.is_empty(), "carol has not reviewed this head");
    }

    /// ⚠️ The ticket bookkeeping, CONFIRMED rather than assumed (STUDIO-874 item 5): a pull request
    /// this feature merges must land its ticket in Done through STUDIO-712's existing path.
    ///
    /// Two ticks, because that is how it really happens: the first clears the gate and hands the
    /// merge out, and the merge itself writes nothing to the watch set — so the SECOND tick
    /// observes the pull request as MERGED exactly as it would have observed a human's merge, and
    /// the auto-Done transition fires on it unchanged. Nothing in the auto-merge path had to know
    /// about tickets at all, which is the property this pins.
    #[test]
    fn a_pull_request_this_feature_merges_lands_its_ticket_in_done() {
        let mut teams = ticketless_automerge(&["alice", "bob"]);
        teams.review.done_state = "Done".to_string();
        let (mut o, _d) = orch(teams);
        introduce(&o, approved_row(64, "bob", HEAD_A));
        run_of(&o, "STUDIO-721");

        // Tick one: the gate clears and the merge is handed to the off-loop half.
        let first = o.handle_review_sweep(&[open_at(64, HEAD_A)]);
        assert_eq!(first.merge.len(), 1, "the merge was proposed");
        assert!(first.done.is_empty(), "and nothing is Done yet");

        // Tick two, after that merge landed. This is the ONLY thing that changed.
        let second = o.handle_review_sweep(&[observed(64, merged_at(HEAD_A))]);

        assert_eq!(
            second.done,
            vec![crate::reviewdone::ReviewDonePlan {
                pr: format!("{OWNER}/{REPO}#64"),
                issue_id: "ID-STUDIO-721".to_string(),
                team_id: "TEAM-1".to_string(),
                identifier: "STUDIO-721".to_string(),
                state: "Done".to_string(),
            }],
            "the auto-merged pull request finishes its ticket like any other merge"
        );
        assert!(
            second.merge.is_empty(),
            "and a merged pull request is never proposed for merging again"
        );
    }

    /// Every `tracing` event a test emits, by level and message. Enough of a `Subscriber` to
    /// COUNT lines, which is the only question STUDIO-881 asks of the log: the ticket was filed
    /// off `grep | sort | uniq -c`, and "announced once" is a claim about that count.
    #[derive(Default, Clone)]
    struct CountedLog(Arc<Mutex<Vec<(tracing::Level, String)>>>);

    impl CountedLog {
        /// The messages logged at `level`, in order.
        fn at(&self, level: tracing::Level) -> Vec<String> {
            self.0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .filter(|(l, _)| *l == level)
                .map(|(_, m)| m.clone())
                .collect()
        }
    }

    /// Pulls the `message` field out of an event and ignores its structured fields.
    struct MessageOf<'a>(&'a mut String);
    impl tracing::field::Visit for MessageOf<'_> {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" {
                *self.0 = format!("{value:?}");
            }
        }
    }

    impl tracing::Subscriber for CountedLog {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            let mut message = String::new();
            event.record(&mut MessageOf(&mut message));
            self.0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((*event.metadata().level(), message));
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    /// ⚠️ The ticket's own measurement, run forward: twenty ticks of a pull request that is stuck
    /// downstream cost ONE line at INFO, not twenty. The log this ticket was filed from carried 97
    /// of this exact line for one draft pull request — the count is the defect, so the count is
    /// what this asserts, and it is counted rather than reasoned about.
    #[test]
    fn twenty_ticks_of_a_stuck_pull_request_cost_one_info_line() {
        let (mut o, _d) = orch(ticketless_automerge(&["alice", "bob"]));
        introduce(&o, approved_row(64, "bob", HEAD_A));
        let log = CountedLog::default();

        tracing::subscriber::with_default(log.clone(), || {
            for _ in 0..20 {
                let report = o.handle_review_sweep(&[open_at(64, HEAD_A)]);
                assert_eq!(report.merge.len(), 1, "and every tick still proposes it");
            }
        });

        assert_eq!(
            log.at(tracing::Level::INFO).len(),
            1,
            "at INFO: {:?}",
            log.at(tracing::Level::INFO)
        );
        assert!(
            log.at(tracing::Level::INFO)[0].starts_with("auto-merge: every reviewer approved"),
            "{:?}",
            log.at(tracing::Level::INFO)
        );
        assert!(
            log.at(tracing::Level::WARN).is_empty(),
            "nothing here is a warning: {:?}",
            log.at(tracing::Level::WARN)
        );
    }

    /// ⚠️ STUDIO-881's second half, at the site that actually dominated the log: the plan line is
    /// announced when it is NEWS and quiet afterwards, while the PLAN itself is re-proposed on
    /// every tick. Both halves matter — the log the ticket was filed from carried 97 identical
    /// INFO lines for one stuck draft, and a "fix" that stopped re-proposing would strand the pull
    /// request the tick its refusal cleared.
    #[test]
    fn an_unchanged_plan_is_announced_once_and_proposed_every_tick() {
        let (mut o, _d) = orch(ticketless_automerge(&["alice", "bob"]));
        introduce(&o, approved_row(64, "bob", HEAD_A));

        let first = o.handle_review_sweep(&[open_at(64, HEAD_A)]);
        let second = o.handle_review_sweep(&[open_at(64, HEAD_A)]);
        let third = o.handle_review_sweep(&[open_at(64, HEAD_A)]);

        assert_eq!(first.merge.len(), 1, "the gate cleared");
        assert_eq!(
            second.merge, first.merge,
            "and keeps clearing: the plan is re-proposed"
        );
        assert_eq!(third.merge, first.merge);
        assert!(
            !o.auto_merge_plan_is_news(&coord(64), HEAD_A, &["bob".to_string()]),
            "said on the first tick; saying it again every minute is what the ticket measured"
        );
    }

    /// What makes the announcement news again: a different head, or a different set of approvals
    /// at the same head. Either changes the claim the line makes, so either is worth saying.
    #[test]
    fn a_new_head_or_a_new_approver_is_news_again() {
        let (mut o, _d) = orch(ticketless_automerge(&["alice", "bob"]));
        let bob = vec!["bob".to_string()];

        assert!(o.auto_merge_plan_is_news(&coord(64), HEAD_A, &bob), "first");
        assert!(!o.auto_merge_plan_is_news(&coord(64), HEAD_A, &bob));
        assert!(
            o.auto_merge_plan_is_news(&coord(64), HEAD_B, &bob),
            "a head the operator has not been told cleared"
        );
        assert!(
            o.auto_merge_plan_is_news(
                &coord(64),
                HEAD_B,
                &["bob".to_string(), "carol".to_string()]
            ),
            "a second approval at that head is a different claim"
        );
        assert!(
            o.auto_merge_plan_is_news(&coord(65), HEAD_B, &bob),
            "and another pull request is its own subject"
        );
    }

    /// The announcement is dropped with the pull request, exactly as its churn budget is: a
    /// coordinate re-introduced later is announced again, and the map does not grow for the
    /// daemon's whole life.
    #[test]
    fn retiring_a_pull_request_forgets_what_was_announced_about_it() {
        let (mut o, _d) = orch(ticketless_automerge(&["alice", "bob"]));
        introduce(&o, approved_row(64, "bob", HEAD_A));

        o.handle_review_sweep(&[open_at(64, HEAD_A)]);
        o.handle_review_sweep(&[observed(64, merged_at(HEAD_A))]);

        assert!(
            o.auto_merge_plan_is_news(&coord(64), HEAD_A, &["bob".to_string()]),
            "the retirement forgot it"
        );
    }

    // --- the conflict route-back (STUDIO-961) ------------------------------------------------

    /// [`ticketless`] with the route-back's state configured — the gate the transition reads.
    fn ticketless_conflict(names: &[&str], state: &str) -> Teams {
        let mut teams = ticketless(names);
        teams.review.changes_state = state.to_string();
        teams
    }

    /// One observation of an OPEN pull request at `head` carrying GitHub's `mergeStateStatus`.
    fn open_conflicted(number: i64, head: &str, merge_state: &str) -> PrObservation {
        PrObservation {
            pr: coord(number),
            lookup: PrLookup::Found(PrSnapshot {
                // STUDIO-962's tri-state: GitHub said "not a draft", which is what a conflicted
                // pull request awaiting its author looks like.
                is_draft: Some(false),
                head_sha: head.to_string(),
                status: PrStatus::Open,
                merged_at: None,
                head_repo: format!("{OWNER}/{REPO}"),
                merge_state: merge_state.to_string(),
            }),
            // STUDIO-960's unchanged-head comparison is not what these fixtures exercise: an empty
            // list is "no head this row has already read", so the advance re-arms as it always did.
            unchanged_from: Vec::new(),
        }
    }

    /// Arms every stream the acceptance names at once: a watched row, the run whose opaque ids the
    /// move needs, and the notification channel the route-back is handed to.
    fn conflict_harness(
        teams: Teams,
    ) -> (
        Orchestrator,
        tokio::sync::mpsc::UnboundedReceiver<crate::reviewnotify::ReviewCompletion>,
    ) {
        let (mut o, _d) = orch(teams);
        introduce(&o, row(12, "bob"));
        run_of(&o, "STUDIO-721");
        let rx = o.open_review_notify_channel();
        (o, rx)
    }

    /// The headline acceptance: a pull request observed CONFLICTED routes its ticket back to its
    /// author once, with a message naming the conflict — and not once per tick.
    ///
    /// Mutation check: delete the once-per-head guard in `propose_conflict_route_back` and the
    /// many-ticks loop below routes six more times, so `routed.len() == 2` reds. Delete the settled
    /// guard and the unsettled loop reds first.
    #[test]
    fn a_conflicted_pull_request_routes_its_ticket_back_once_per_head() {
        let (mut o, mut rx) =
            conflict_harness(ticketless_conflict(&["alice", "bob"], "In Progress"));

        // ⚠️ An unsettled (or non-conflict) mergeability read acts on nothing: GitHub computes
        // mergeability lazily and answers UNKNOWN — or, briefly, nothing — while it does.
        for state in ["", "UNKNOWN", "CLEAN", "BLOCKED", "BEHIND", "DRAFT"] {
            assert_eq!(
                o.handle_review_sweep(&[open_conflicted(12, HEAD_A, state)])
                    .routed,
                0,
                "({state:?}) must not route a ticket"
            );
        }
        assert!(rx.try_recv().is_err(), "no completion was sent");

        // The settled conflict routes once…
        assert_eq!(
            o.handle_review_sweep(&[open_conflicted(12, HEAD_A, "DIRTY")])
                .routed,
            1
        );
        // ⚠️ …and not again at the same head, however many ticks pass with the state unchanged.
        for _ in 0..5 {
            assert_eq!(
                o.handle_review_sweep(&[open_conflicted(12, HEAD_A, "DIRTY")])
                    .routed,
                0,
                "a conflict that persists across a tick is not a new event"
            );
        }
        // A push moves the head, so a conflict at the NEW head is a new event.
        assert_eq!(
            o.handle_review_sweep(&[open_conflicted(12, HEAD_B, "DIRTY")])
                .routed,
            1
        );

        let mut routed = Vec::new();
        while let Ok(c) = rx.try_recv() {
            routed.push(c);
        }
        assert_eq!(
            routed.len(),
            2,
            "exactly one route-back per conflicted head, not one per tick"
        );
        assert!(
            routed
                .iter()
                .all(|c| c.reason == crate::reviewnotify::CompletionReason::Conflict),
            "both completions are conflict route-backs, not verdicts"
        );
        // ⚠️ `approved` is load-bearing TWICE at the consumer, and neither site is reached from
        // here: `reviewnotify::route_back` REFUSES the tracker move outright when it is set
        // (`reviewnotify.rs`, the approved-arm refusal), and the token-consistency check logs
        // `error!` on every completion whose summons disagrees with it — a conflict comment
        // deliberately carries the token, so an `approved: true` conflict would log an error on
        // every route-back AND never move the ticket. Asserted on the completion the PRODUCER
        // built, because that is the value the consumer reads.
        //
        // MUTATION: flip `approved` to `true` in `propose_conflict_route_back` and this reds.
        assert!(
            routed.iter().all(|c| !c.approved),
            "a conflict route-back is not an approval: `approved` gates the move and the token \
             consistency check at the consumer"
        );
        assert_eq!(routed[0].head_sha, HEAD_A);
        assert_eq!(routed[1].head_sha, HEAD_B);
        // The message names the conflict as the reason and what to fix, and it re-engages the
        // author through the real summon matcher.
        let body = crate::reviewnotify::re_engage_comment(&routed[0]);
        assert!(body.contains("CONFLICTED"), "{body}");
        assert!(body.contains("rebase"), "{body}");
        assert!(
            crate::reviewnotify::summons_author(&body, &routed[0].summon_token),
            "the conflict comment must carry the token that reopens the author's run"
        );
        let plan = routed[0].changes.as_ref().expect("a route-back plan");
        assert_eq!(plan.identifier, "STUDIO-721");
        assert_eq!(plan.state, "In Progress");
        assert_eq!(plan.pr, "makewhatis/rhapsody#12");
    }

    /// ⚠️ A conflict on a pull request whose ticket is ALREADY out of review — here, a `reviewed`
    /// row is this daemon's own record that a findings verdict moved it — routes it nowhere.
    #[test]
    fn a_conflict_does_not_re_route_a_ticket_a_findings_verdict_already_moved() {
        let (mut o, mut rx) =
            conflict_harness(ticketless_conflict(&["alice", "bob"], "In Progress"));
        // The row the finder left behind: findings at HEAD_A, ticket already routed back.
        o.store()
            .mark_review_completed(&key(12, "bob"), HEAD_A, REVIEW_STATUS_REVIEWED)
            .expect("a findings verdict");

        assert_eq!(
            o.handle_review_sweep(&[open_conflicted(12, HEAD_A, "DIRTY")])
                .routed,
            0,
            "a ticket already routed back must not be moved again"
        );
        assert!(rx.try_recv().is_err());
    }

    /// The origin scope is the shared one: an operator-introduced pull request names an operator,
    /// not a ticket this daemon parked, so a conflict routes nothing.
    #[test]
    fn a_conflict_on_an_operator_introduced_pull_request_routes_no_ticket() {
        let (mut o, mut rx) =
            conflict_harness(ticketless_conflict(&["alice", "bob"], "In Progress"));
        o.store()
            .save_review_watch(ReviewWatchRow {
                introduced_by: "console:operator".to_string(),
                ..row(12, "bob")
            })
            .expect("reintroduce");

        assert_eq!(
            o.handle_review_sweep(&[open_conflicted(12, HEAD_A, "DIRTY")])
                .routed,
            0
        );
        assert!(rx.try_recv().is_err());
    }

    /// An installation that has not named `teams.review.changes_state` has no transition to fire,
    /// so a conflict routes nothing — the byte-identical-when-unconfigured property.
    #[test]
    fn a_conflict_routes_nothing_when_the_transition_is_unconfigured() {
        let (mut o, mut rx) = conflict_harness(ticketless(&["alice", "bob"]));

        assert_eq!(
            o.handle_review_sweep(&[open_conflicted(12, HEAD_A, "DIRTY")])
                .routed,
            0
        );
        assert!(rx.try_recv().is_err());
    }

    /// Without the notification task there is nobody to post the comment or move the ticket, so the
    /// plan is not recorded as routed — the next tick, with a task, can still fire.
    #[test]
    fn a_conflict_routes_nothing_when_no_task_can_perform_it() {
        let (mut o, _d) = orch(ticketless_conflict(&["alice", "bob"], "In Progress"));
        introduce(&o, row(12, "bob"));
        run_of(&o, "STUDIO-721");
        // Deliberately no `open_review_notify_channel`.

        assert_eq!(
            o.handle_review_sweep(&[open_conflicted(12, HEAD_A, "DIRTY")])
                .routed,
            0
        );
        // And because nothing was recorded, a later tick with a task can still route it.
        let mut rx = o.open_review_notify_channel();
        assert_eq!(
            o.handle_review_sweep(&[open_conflicted(12, HEAD_A, "DIRTY")])
                .routed,
            1
        );
        assert!(rx.try_recv().is_ok());
    }

    /// A conflict that CLEARS forgets the head it routed for, so the record stops suppressing the
    /// reconciliation sweep and a fresh conflict (even at the same head) is news again.
    ///
    /// Every settled non-conflict the allow-list recognises is driven through the clear, because
    /// the allow-list IS the load-bearing decision and it is data: a value dropped or misspelled
    /// there must be caught here. Mutation check: shrink
    /// `SETTLED_NON_CONFLICT_MERGE_STATES` to `["CLEAN"]` and each of the other five loop
    /// iterations reds at the `contains_key` assertion.
    #[test]
    fn a_resolved_conflict_forgets_the_head_it_routed_for() {
        let (mut o, mut rx) =
            conflict_harness(ticketless_conflict(&["alice", "bob"], "In Progress"));

        for settled in [
            "BEHIND",
            "BLOCKED",
            "CLEAN",
            "DRAFT",
            "HAS_HOOKS",
            "UNSTABLE",
        ] {
            // The conflict is present and routes back at this head…
            assert_eq!(
                o.handle_review_sweep(&[open_conflicted(12, HEAD_A, "DIRTY")])
                    .routed,
                1,
                "({settled:?}) the conflict must route back first"
            );
            // …then GitHub settles on a non-conflict, which must NOT route and must forget the head.
            assert_eq!(
                o.handle_review_sweep(&[open_conflicted(12, HEAD_A, settled)])
                    .routed,
                0,
                "({settled:?}) is a settled non-conflict and is not a route-back"
            );
            assert!(
                !o.conflict_routed.contains_key(&coord(12)),
                "({settled:?}) is a settled non-conflict and must stop suppressing the sweep"
            );
        }

        let mut n = 0;
        while rx.try_recv().is_ok() {
            n += 1;
        }
        assert_eq!(
            n, 6,
            "one summons per conflict, and every settled value forgets it"
        );
    }

    /// ⚠️ An UNSETTLED read between two DIRTY reads at one head must not re-route. GitHub recomputes
    /// mergeability whenever the base moves and answers `UNKNOWN` until it has, so `DIRTY → UNKNOWN
    /// → DIRTY` is the live sequence on any branch whose base is moving; reading the `UNKNOWN` tick as
    /// "the conflict resolved" re-sends the summons and moves the ticket a second time.
    ///
    /// `RECOMPUTING` stands in for the value this daemon has never seen: the clear is an ALLOW-list,
    /// so an unrecognised value must forget nothing too. A deny-list ("anything but `UNKNOWN`")
    /// re-opens the same loop on a vocabulary change.
    ///
    /// Mutation check: fold the unsettled read back into the clear (drop the carve-out in
    /// `propose_conflict_route_back`) and the second `DIRTY` reds — routed 1, expected 0.
    #[test]
    fn an_unsettled_read_does_not_forget_the_head_it_routed_for() {
        let (mut o, mut rx) =
            conflict_harness(ticketless_conflict(&["alice", "bob"], "In Progress"));

        assert_eq!(
            o.handle_review_sweep(&[open_conflicted(12, HEAD_A, "DIRTY")])
                .routed,
            1
        );
        // The base moved: GitHub has not recomputed, and the head is unchanged.
        for unsettled in ["", "UNKNOWN", "RECOMPUTING"] {
            assert_eq!(
                o.handle_review_sweep(&[open_conflicted(12, HEAD_A, unsettled)])
                    .routed,
                0,
                "({unsettled:?}) must act on nothing"
            );
            assert!(
                o.conflict_routed.contains_key(&coord(12)),
                "({unsettled:?}) must forget nothing: it is not evidence the conflict resolved"
            );
        }
        assert_eq!(
            o.handle_review_sweep(&[open_conflicted(12, HEAD_A, "DIRTY")])
                .routed,
            0,
            "the same conflicted head must not be routed back twice"
        );

        let mut n = 0;
        while rx.try_recv().is_ok() {
            n += 1;
        }
        assert_eq!(n, 1, "exactly one summons for one conflicted head");
    }

    /// ⚠️ The instant the watcher stamps on the record is the whole of the reconciliation sweep's
    /// freshness bound, so it is pinned at the PRODUCER, not only at the sweep that reads it. If
    /// this wrote a constant — ancient or future — every real route-back would either stop
    /// suppressing the sweep or silence it forever, and the two hand-inserted reconcile tests would
    /// not notice.
    ///
    /// Mutation check: date the record `DateTime::<Utc>::MIN_UTC` (or `MAX_UTC`) in
    /// `propose_conflict_route_back` and this reds.
    #[test]
    fn a_conflict_route_back_is_stamped_with_the_clock_it_acted_on() {
        let (mut o, _rx) = conflict_harness(ticketless_conflict(&["alice", "bob"], "In Progress"));
        let instant = chrono::DateTime::parse_from_rfc3339("2026-09-14T21:20:00Z")
            .expect("test instant")
            .with_timezone(&chrono::Utc);
        o.now = Box::new(move || instant);

        assert_eq!(
            o.handle_review_sweep(&[open_conflicted(12, HEAD_A, "DIRTY")])
                .routed,
            1
        );
        let routed = o
            .conflict_routed
            .get(&coord(12))
            .expect("a recorded route-back");
        assert_eq!(
            routed.routed_at, instant,
            "the freshness anchor must be the instant the tick acted on, not a constant"
        );
        assert_eq!(routed.head, HEAD_A);
    }

    /// Retiring a pull request drops its conflict record — a coordinate watched again later must
    /// not inherit the old silence.
    #[test]
    fn retiring_a_pull_request_forgets_its_conflict_route_back() {
        let (mut o, _rx) = conflict_harness(ticketless_conflict(&["alice", "bob"], "In Progress"));
        assert_eq!(
            o.handle_review_sweep(&[open_conflicted(12, HEAD_A, "DIRTY")])
                .routed,
            1
        );

        o.handle_review_sweep(&[observed(12, merged_at(HEAD_A))]);

        assert!(
            !o.conflict_routed.contains_key(&coord(12)),
            "the retired pull request's record must not survive"
        );
    }

    /// ⚠️ STUDIO-949's hold, on the conflict trigger: a `rhapsody:human` origin ticket whose pull
    /// request is observed CONFLICTED routes nowhere and summons nobody.
    ///
    /// A route-back is not a read. It moves tracker state out of the review state AND reopens the
    /// author's agent run through the summon token, which is exactly the pair of actions the label
    /// exists to refuse — a conflict on a human-held ticket is a human's to resolve. Without the
    /// gate the merged tree answers `routed=1, summons=1` here.
    ///
    /// MUTATION: move `propose_conflict_route_back` back outside the `ledger_primed && !held_origin`
    /// gate in `service_review_pr` and this reds (a ticket is routed and a summons sent).
    #[test]
    fn a_held_origin_ticket_routes_no_conflict_back() {
        let (mut o, mut rx) =
            conflict_harness(ticketless_conflict(&["alice", "bob"], "In Progress"));
        o.human_holds.note_human_label("STUDIO-721"); // the row's origin is `handoff:STUDIO-721`

        assert_eq!(
            o.handle_review_sweep(&[open_conflicted(12, HEAD_A, "DIRTY")])
                .routed,
            0,
            "a held ticket must not be moved out of review by the daemon"
        );
        assert!(
            rx.try_recv().is_err(),
            "and no summons may reopen the author's run on it"
        );

        // The label comes off — the next selection pass clears the current hold set — and the same
        // conflict, at the same head, routes back on the next tick. The live control: nothing but
        // the hold was holding it.
        o.human_holds.begin_pass(true);
        assert_eq!(
            o.handle_review_sweep(&[open_conflicted(12, HEAD_A, "DIRTY")])
                .routed,
            1,
            "once the hold is gone the conflict routes back"
        );
        assert!(rx.try_recv().is_ok());
    }

    /// ⚠️ The fail-closed half of the same gate (STUDIO-949 round 11): no selection pass has run, so
    /// the current-label set is an ABSENCE OF INFORMATION rather than "no hold" — on a daemon held
    /// by a bad config, an armed drain or a dead credential it stays that way for the whole process
    /// lifetime, while this sweep runs from the watcher's own independent task. An unknown set must
    /// not route a ticket that may wear the label.
    ///
    /// MUTATION: drop the `ledger_primed` half of the gate and this reds (a ticket is routed).
    #[test]
    fn an_unprimed_hold_ledger_routes_no_conflict_back() {
        let (o, _d) = orch_before_first_pass(ticketless_conflict(&["alice", "bob"], "In Progress"));
        let mut o = o;
        introduce(&o, row(12, "bob")); // nothing held — the label set is just unknown
        run_of(&o, "STUDIO-721");
        let mut rx = o.open_review_notify_channel();

        assert_eq!(
            o.handle_review_sweep(&[open_conflicted(12, HEAD_A, "DIRTY")])
                .routed,
            0,
            "with no pass having looked, the hold set is unknown and the route-back must wait"
        );
        assert!(rx.try_recv().is_err());

        // Once a pass has run the answer is real, and the same conflict routes back.
        o.human_holds.begin_pass(true);
        assert_eq!(
            o.handle_review_sweep(&[open_conflicted(12, HEAD_A, "DIRTY")])
                .routed,
            1,
            "a primed ledger routes the conflicted head back"
        );
        assert!(rx.try_recv().is_ok());
    }

    /// ⚠️ A conflict routes back PAST the adjudication threshold too (STUDIO-956 × STUDIO-961).
    ///
    /// The manager's `ship`/`escalate` decision settles the FINDINGS question; it says nothing about
    /// whether the branch merges, and `perform_auto_merge` declines a conflicted head every time.
    /// STUDIO-956's four `return`s precede the tail of `service_review_pr`, so a route-back placed
    /// there would never fire again for any pull request that reached the threshold — the
    /// nine-hour stall STUDIO-961 was filed for, restored for exactly the pull requests that have
    /// already spent the most review rounds. Mergeability is orthogonal to the verdict, so the
    /// trigger is decided above the adjudication block.
    ///
    /// MUTATION: move `propose_conflict_route_back` to the tail's `else` branch and this reds
    /// (`routed == 0`), while every other conflict test stays green.
    #[test]
    fn a_conflict_past_the_adjudication_threshold_still_routes_back() {
        let mut teams = ticketless_conflict(&["alice", "bob"], "In Progress");
        teams.review.adjudicate_after_rounds = 3;
        let (mut o, _d) = orch(teams);
        let l = ledger(&mut o);
        introduce(&o, row(12, "bob"));
        run_of(&o, "STUDIO-721");
        let mut rx = o.open_review_notify_channel();
        // The manager has already decided: the loop is stopped, and every path through
        // `service_review_pr` now returns before its tail.
        l.record(
            &coord(12),
            Adjudication::Ship {
                head: HEAD_A.to_string(),
                rounds: 3,
            },
        );

        let report = o.handle_review_sweep(&[open_conflicted(12, HEAD_A, "DIRTY")]);

        assert!(
            report.adjudicate.is_empty(),
            "a settled decision must not be re-asked"
        );
        assert_eq!(
            report.dispatched, 0,
            "and the round loop stays stopped — this changes nothing about STUDIO-956"
        );
        assert_eq!(
            report.routed, 1,
            "a shipped pull request that cannot merge still needs its author"
        );
        let c = rx.try_recv().expect("the author is summoned");
        assert_eq!(c.reason, crate::reviewnotify::CompletionReason::Conflict);
        assert_eq!(c.head_sha, HEAD_A);
    }

    /// ⚠️ The tick already knows the answer: a pull request every reviewer approved at this head,
    /// observed CONFLICTED, is NOT handed to the auto-merge gate — `perform_auto_merge` re-reads
    /// `mergeStateStatus` through its own seam and declines on anything but `CLEAN`, so the plan
    /// costs two `gh` round trips to be told what the string this tick already read says, and opens
    /// a window in which the merge is attempted while the route-back's comment and move are in
    /// flight.
    ///
    /// The `CLEAN` half is the live control: the identical fixture, one `mergeStateStatus` apart,
    /// merges and routes nothing.
    ///
    /// MUTATION: drop the `merge_state == MERGE_STATE_DIRTY` branch in `propose_auto_merge` and this
    /// reds (`merge_plans = 1` beside `routed = 1`, from one sweep).
    #[test]
    fn a_conflicted_pull_request_is_not_offered_for_auto_merge() {
        let mut teams = ticketless_automerge(&["alice", "bob"]);
        teams.review.changes_state = "In Progress".to_string();
        let (mut o, _d) = orch(teams);
        introduce(&o, approved_row(12, "bob", HEAD_A)); // origin: `handoff:STUDIO-721`
        run_of(&o, "STUDIO-721");
        let _rx = o.open_review_notify_channel();

        // Settled and CLEAN: the merge is proposed and nothing routes.
        let clean = o.handle_review_sweep(&[open_conflicted(12, HEAD_A, "CLEAN")]);
        assert_eq!(
            clean.merge.len(),
            1,
            "the control: an approved clean head merges"
        );
        assert_eq!(clean.routed, 0);

        // Settled and CONFLICTED: no merge is asked for, and the author gets the branch back.
        let dirty = o.handle_review_sweep(&[open_conflicted(12, HEAD_A, "DIRTY")]);
        assert!(
            dirty.merge.is_empty(),
            "GitHub has already said this cannot merge: {:?}",
            dirty.merge
        );
        assert_eq!(dirty.routed, 1, "and the conflict goes back to its author");
    }

    // --- load-aware reviewer selection ------------------------------------------------------

    /// Acceptance: two review requests in ONE tick pick two different reviewers. This is the load
    /// finding: introduction ranks both rows against a load map that is always empty on the
    /// ticketless path, so both name the same teammate; the watcher re-decides from live runs, and
    /// the first dispatch of the tick is already in `running` when the second is decided.
    #[test]
    fn two_review_requests_in_one_tick_pick_two_different_reviewers() {
        let (mut o, dispatched) = orch(ticketless(&["alice", "bob", "carol"]));
        // Both introduced naming `bob` — exactly what the degenerate ranking produces.
        introduce(&o, row(12, "bob"));
        introduce(&o, row(13, "bob"));

        let report = o.handle_review_sweep(&[open_at(12, HEAD_A), open_at(13, HEAD_A)]);

        assert_eq!(report.dispatched, 2);
        let mut picked = reviewers_of(&dispatched);
        picked.sort();
        assert_eq!(
            picked,
            vec!["bob".to_string(), "carol".to_string()],
            "the second round must see the first one's load and pick somebody else"
        );
        assert!(
            !picked.contains(&"alice".to_string()),
            "the author must never be handed their own pull request"
        );
    }

    /// Acceptance: a capped / at-max reviewer KEEPS the round. This test used to assert the
    /// opposite — that `bob`, at his `max_concurrent` and busy, was skipped to `carol` — and the
    /// deliberate loosening in STUDIO-800 (design D2, "reviews are free") reverses it. Everything
    /// that made `bob` the right reviewer still holds: he read the previous round, so decision B
    /// prefers him, and his cap is a limit on the implementation work he is dispatched rather than
    /// on his availability to read `alice`'s pull request. The load ranking would have put the
    /// idle `carol` first; continuity outranks it, and capacity no longer overrides continuity.
    ///
    /// Because the round is NOT reassigned, `bob`'s row stays in the watch set — the retirement
    /// this test used to check happens only on a substitution.
    #[test]
    fn a_capped_reviewer_keeps_the_round_because_reviews_are_free() {
        let (mut o, dispatched) = orch(teams_with(
            true,
            ReviewMode::Ticketless,
            vec![ident("alice", 0), ident("bob", 1), ident("carol", 0)],
        ));
        introduce(&o, row(12, "bob"));
        // bob reviewed #12 once, and is now busy with something else.
        o.store()
            .mark_review_completed(&key(12, "bob"), HEAD_A, REVIEW_STATUS_REVIEWED)
            .expect("complete");
        let mut busy = RunningEntry::empty(rhapsody_core::Issue {
            id: "iss-9".to_string(),
            identifier: "STUDIO-999".to_string(),
            ..Default::default()
        });
        busy.identity = "bob".to_string();
        o.running.insert("iss-9".to_string(), busy);

        let report = o.handle_review_sweep(&[open_at(12, HEAD_B)]);

        assert_eq!(report.dispatched, 1);
        assert_eq!(
            reviewers_of(&dispatched),
            vec!["bob".to_string()],
            "an implementation cap must not move the round off the reviewer who read the last one"
        );
        assert_ne!(
            watch_row(&o, 12, "bob").status,
            REVIEW_STATUS_DROPPED,
            "nothing was reassigned, so the incumbent's row must stay in the watch set"
        );
        assert_eq!(watch_row(&o, 12, "bob").requested_sha, HEAD_B);
    }

    /// Acceptance (D2, "reviews are free"): a teammate who is at their `max_concurrent` running
    /// IMPLEMENTATION work is still chosen as a reviewer. `bob` is the only possible reviewer —
    /// the row's author `alice` is not on this roster — and he is at his cap of one, so under the
    /// old rule the round was deferred with nobody available. A cap is a limit on the work a
    /// teammate is DISPATCHED, never on their availability to read somebody else's.
    #[test]
    fn a_reviewer_at_their_implementation_cap_is_still_chosen() {
        let (mut o, dispatched) = orch(teams_with(
            true,
            ReviewMode::Ticketless,
            vec![ident("bob", 1)],
        ));
        introduce(&o, row(1, "bob"));
        let mut busy = RunningEntry::empty(rhapsody_core::Issue {
            id: "iss-9".to_string(),
            identifier: "STUDIO-999".to_string(),
            ..Default::default()
        });
        busy.identity = "bob".to_string();
        o.running.insert("iss-9".to_string(), busy);

        let report = o.handle_review_sweep(&[open_at(1, HEAD_A)]);

        assert_eq!(
            reviewers_of(&dispatched),
            vec!["bob".to_string()],
            "an implementation cap must not withhold a reviewer (D2)"
        );
        assert_eq!(report.dispatched, 1);
        assert_eq!(report.deferred, 0);
    }

    /// Decision B: a reviewer who READ the previous round keeps the pull request even when
    /// somebody else is idler — continuity outranks the load ranking.
    #[test]
    fn a_re_review_prefers_the_reviewer_who_read_the_last_round() {
        let (mut o, dispatched) = orch(ticketless(&["alice", "bob", "carol"]));
        introduce(&o, row(12, "bob"));
        o.store()
            .mark_review_completed(&key(12, "bob"), HEAD_A, REVIEW_STATUS_REVIEWED)
            .expect("complete");
        // `carol` is idle and ranks first on load; continuity must still win.
        let mut busy = RunningEntry::empty(rhapsody_core::Issue {
            id: "iss-9".to_string(),
            identifier: "STUDIO-999".to_string(),
            ..Default::default()
        });
        busy.identity = "bob".to_string();
        o.running.insert("iss-9".to_string(), busy);

        o.handle_review_sweep(&[open_at(12, HEAD_B)]);

        assert_eq!(reviewers_of(&dispatched), vec!["bob".to_string()]);
    }

    /// STUDIO-951: a required reviewer outranks a NON-required incumbent. The persisted row names
    /// `bob` — reviewing before the operator added `review.required: [carol]` — and continuity must
    /// not keep him, or the pinned identity never reviews this pull request and nothing repairs it.
    ///
    /// Mutation check: remove the `required_among_candidates` guard and this goes red with `bob`,
    /// which is exactly the defect the round-2 review found.
    #[test]
    fn a_required_reviewer_outranks_a_non_required_incumbent() {
        let mut teams = ticketless(&["alice", "bob", "carol"]);
        teams.review.required = vec!["carol".to_string()];
        let (mut o, dispatched) = orch(teams);
        introduce(&o, row(12, "bob"));
        o.store()
            .mark_review_completed(&key(12, "bob"), HEAD_A, REVIEW_STATUS_REVIEWED)
            .expect("complete");

        o.handle_review_sweep(&[open_at(12, HEAD_B)]);

        assert_eq!(
            reviewers_of(&dispatched),
            vec!["carol".to_string()],
            "a required reviewer must win over an incumbent who is no longer pinned"
        );
    }

    /// The counterpart, so the fix cannot simply disable continuity: with `review.required` unset a
    /// non-required incumbent is still preferred over an idler (Decision B is unchanged).
    #[test]
    fn an_optional_incumbent_still_keeps_the_round() {
        let (mut o, dispatched) = orch(ticketless(&["alice", "bob", "carol"]));
        introduce(&o, row(12, "bob"));
        o.store()
            .mark_review_completed(&key(12, "bob"), HEAD_A, REVIEW_STATUS_REVIEWED)
            .expect("complete");

        o.handle_review_sweep(&[open_at(12, HEAD_B)]);

        assert_eq!(reviewers_of(&dispatched), vec!["bob".to_string()]);
    }

    /// STUDIO-951 / round 3: continuity yields only to a required reviewer who is **actually
    /// pinned**. `sol` is required but its profile names a harness this build cannot run, so
    /// `rank_reviewers` drops it from the pinned prefix (and, since STUDIO-978, from the ranked fill
    /// too) — a name the guard must not treat as a pin. Reading the raw config list instead evicts
    /// the incumbent for a teammate the ranking never promoted, and the round goes to whoever merely
    /// leads on load: neither the incumbent nor the pin.
    ///
    /// `bob` is loaded and `carol` is idle, so with continuity broken the load leader `carol` wins;
    /// the assertion is that the incumbent keeps the round.
    ///
    /// Mutation check: read `teams.review_required()` instead of the effective pinned set and this
    /// goes red with `carol`.
    #[test]
    fn an_unselectable_required_reviewer_does_not_break_continuity() {
        let dir = crate::testsupport::TempDir::new();
        write_profile(
            &dir,
            "codexer",
            "---\nextends: swe\nharness: codex\n---\nCodex.\n",
        );
        let mut teams = ticketless(&["alice", "bob", "carol", "sol"]);
        teams.roster[3].profile = "codexer".to_string();
        teams.review.required = vec!["sol".to_string()];
        let (mut o, dispatched) = orch(teams);
        o.teams_profiles_dir = Some(std::path::PathBuf::from(dir.child("profiles")));
        introduce(&o, row(12, "bob"));
        o.store()
            .mark_review_completed(&key(12, "bob"), HEAD_A, REVIEW_STATUS_REVIEWED)
            .expect("complete");
        let mut busy = RunningEntry::empty(rhapsody_core::Issue {
            id: "iss-9".to_string(),
            identifier: "STUDIO-999".to_string(),
            ..Default::default()
        });
        busy.identity = "bob".to_string();
        o.running.insert("iss-9".to_string(), busy);

        o.handle_review_sweep(&[open_at(12, HEAD_B)]);

        assert_eq!(
            reviewers_of(&dispatched),
            vec!["bob".to_string()],
            "an unselectable required reviewer must not evict the incumbent"
        );
    }

    /// STUDIO-978 / alice's F2: an AUTHOR-LESS row can only ever be serviced by its INCUMBENT, so an
    /// incumbent whose dispatch `spawn_worker` would REFUSE must defer the round rather than being
    /// re-offered every tick. Without the unselectable check the row is dispatched, refused in the
    /// worker, left `in_flight`, and crash recovery re-offers the same impossible incumbent — the
    /// loop this branch is the only backstop for. The authored test above
    /// (`an_unselectable_required_reviewer_does_not_break_continuity`) exercises the other branch and
    /// cannot see this one.
    ///
    /// MUTATION GUARD: `(on_roster && !exclusions.unselectable.contains(incumbent))` →
    /// `(on_roster)` and the refused incumbent is dispatched.
    #[test]
    fn an_authorless_row_defers_when_its_incumbent_cannot_run() {
        let dir = crate::testsupport::TempDir::new();
        write_profile(
            &dir,
            "codexer",
            "---\nextends: swe\nharness: codex\n---\nCodex.\n",
        );
        let mut teams = ticketless(&["alice", "bob", "carol", "sol"]);
        teams.roster[3].profile = "codexer".to_string();
        let (mut o, dispatched) = orch(teams);
        o.teams_profiles_dir = Some(std::path::PathBuf::from(dir.child("profiles")));
        // `row(..)`'s author is alice; blanking it drives the author-less branch, which may only
        // ever name the incumbent `sol`.
        let mut r = row(12, "sol");
        r.author = String::new();
        introduce(&o, r);

        let report = o.handle_review_sweep(&[open_at(12, HEAD_A)]);

        assert_eq!(
            report.dispatched, 0,
            "an incumbent whose dispatch is refused must not be dispatched"
        );
        assert_eq!(report.deferred, 1, "the round is deferred, not lost");
        assert!(dispatched.lock().expect("lock").is_empty());
    }

    /// STUDIO-951 / round 4: a persisted incumbent that is a TAIL pin beyond `review.reviewers`
    /// must not displace the declaration-order pin that actually survives the clamp. With
    /// `reviewers: 1` and `required: [carol, bob]`, `carol` is the one pin selection keeps and the
    /// tail pin `bob` is dropped — so a row persisted with `bob` before this config existed must
    /// yield to `carol`, exactly as a non-required incumbent would. The continuity guard's
    /// "required" set is the EFFECTIVE pin prefix, capped at the ticketless reviewer count, not the
    /// untruncated configured list; otherwise the tail pin is treated as required and keeps the
    /// round, contradicting both the clamp and the boot warning that says the tail is dropped.
    ///
    /// Mutation check: read the untruncated `plan_required_pins(..).pinned` (drop the
    /// `effective_reviewers()` cap in `quorum::pinned_required_reviewers`) and this goes red with
    /// `bob` — the tail pin that no longer survives selection.
    #[test]
    fn a_tail_required_pin_beyond_the_clamp_does_not_displace_the_surviving_pin() {
        let mut teams = ticketless(&["alice", "bob", "carol"]);
        teams.review.reviewers = 1;
        teams.review.required = vec!["carol".to_string(), "bob".to_string()];
        assert_eq!(teams.review.effective_reviewers(), 1);
        let (mut o, dispatched) = orch(teams);
        introduce(&o, row(12, "bob"));
        o.store()
            .mark_review_completed(&key(12, "bob"), HEAD_A, REVIEW_STATUS_REVIEWED)
            .expect("complete");

        o.handle_review_sweep(&[open_at(12, HEAD_B)]);

        assert_eq!(
            reviewers_of(&dispatched),
            vec!["carol".to_string()],
            "the clamp keeps the first declaration-order pin; a persisted tail pin must not win"
        );
    }

    /// A round with no eligible reviewer is DEFERRED, not forced onto somebody and not silently
    /// lost — the next tick considers it again.
    ///
    /// The lever used to be capacity; STUDIO-800 removed that as a deferral reason (D2), so this
    /// pins the same behaviour on one that survives: the ranking has nobody to offer. `alice`
    /// authored the pull request and is the only teammate left on the roster, and an author never
    /// reviews their own work — so the round has no candidate at all. Deliberately ONE row and one
    /// roster member, so the deferral can only have come from reviewer selection: with two rows a
    /// downstream guard (an already-in-flight review key) reports the same counts, and the test
    /// would pass for a reason it is not about.
    #[test]
    fn a_round_nobody_can_take_is_deferred_and_reconsidered() {
        let (mut o, dispatched) = orch(teams_with(
            true,
            ReviewMode::Ticketless,
            vec![ident("alice", 0)],
        ));
        introduce(&o, row(12, "bob"));

        let report = o.handle_review_sweep(&[open_at(12, HEAD_A)]);
        assert_eq!((report.dispatched, report.deferred), (0, 1));
        assert!(dispatched.lock().expect("lock").is_empty());

        // `bob` joins the roster; the same row is picked up with no new introduction.
        o.teams = Some(ticketless(&["alice", "bob"]));
        assert_eq!(o.handle_review_sweep(&[open_at(12, HEAD_A)]).dispatched, 1);
        assert_eq!(reviewers_of(&dispatched), vec!["bob".to_string()]);
    }

    /// The operator advisory is PROSE that goes out on the wire, and a backslash-continued Rust
    /// literal is exactly where source indentation leaks into shipped text. A test that compares
    /// the constant to itself would never see it, so this reads the rendered value.
    #[test]
    fn the_stalled_round_advisory_renders_as_one_sentence() {
        assert!(
            !REVIEW_UNASSIGNABLE_WARNING.contains("  ")
                && !REVIEW_UNASSIGNABLE_WARNING.contains('\n'),
            "leaked source indentation: {REVIEW_UNASSIGNABLE_WARNING:?}"
        );
        assert!(
            REVIEW_UNASSIGNABLE_WARNING.contains("review.reviewers"),
            "the advisory must name the knob an operator turns"
        );
    }

    /// STUDIO-891: a round that keeps deferring stops being a quiet one.
    ///
    /// Boot validation cannot see this case — the config was satisfiable when it was written and
    /// the roster shrank underneath it — so the deferral has to report itself. The failure shape
    /// this whole batch keeps producing is an idle board: the round defers, `deferred` ticks up in
    /// a report nobody reads, and a `debug!` line is the only record. After
    /// [`REVIEW_UNASSIGNABLE_SWEEPS`] consecutive sweeps the row is called stalled, and that fact
    /// reaches `/api/v1/projects` — the same surface a paused dispatch uses.
    #[test]
    fn a_round_that_defers_for_several_sweeps_is_reported_as_stalled() {
        let (mut o, _dispatched) = orch(teams_with(
            true,
            ReviewMode::Ticketless,
            vec![ident("alice", 0)],
        ));
        introduce(&o, row(12, "bob"));

        // `alice` authored it and is the whole roster, so no sweep can ever assign this round.
        for sweep in 1..REVIEW_UNASSIGNABLE_SWEEPS {
            let report = o.handle_review_sweep(&[open_at(12, HEAD_A)]);
            assert_eq!((report.deferred, report.stalled), (1, 0), "sweep {sweep}");
            assert!(
                o.project_statuses()[0]
                    .warnings
                    .iter()
                    .all(|w| w != REVIEW_UNASSIGNABLE_WARNING),
                "a round that has only just started deferring is not yet news (sweep {sweep})"
            );
        }
        // The sweep that crosses the threshold.
        let report = o.handle_review_sweep(&[open_at(12, HEAD_A)]);
        assert_eq!((report.deferred, report.stalled), (1, 1));
        assert!(
            o.project_statuses()[0]
                .warnings
                .iter()
                .any(|w| w == REVIEW_UNASSIGNABLE_WARNING),
            "a stalled round must reach the operator's project status"
        );
        // It stays reported for as long as it stays stalled.
        assert_eq!(o.handle_review_sweep(&[open_at(12, HEAD_A)]).stalled, 1);

        // A roster that can service the round again clears BOTH the count and the advisory on the
        // very next sweep — the recovery edge, which is the half a warning that only ever latches
        // would get wrong.
        o.teams = Some(ticketless(&["alice", "bob"]));
        let report = o.handle_review_sweep(&[open_at(12, HEAD_A)]);
        assert_eq!((report.dispatched, report.stalled), (1, 0));
        assert!(
            o.project_statuses()[0]
                .warnings
                .iter()
                .all(|w| w != REVIEW_UNASSIGNABLE_WARNING),
            "a round that got a reviewer is no longer stalled"
        );
    }

    /// The counter is per ROW and self-cleaning: a pull request that leaves the watch set takes its
    /// stall count with it, so the map cannot grow for the daemon's whole life and a re-introduced
    /// pull request does not inherit an old grievance.
    #[test]
    fn a_retired_pull_request_forgets_its_stall_count() {
        let (mut o, _dispatched) = orch(teams_with(
            true,
            ReviewMode::Ticketless,
            vec![ident("alice", 0)],
        ));
        introduce(&o, row(12, "bob"));
        for _ in 0..REVIEW_UNASSIGNABLE_SWEEPS {
            o.handle_review_sweep(&[open_at(12, HEAD_A)]);
        }
        assert!(!o.review_unassignable.is_empty());
        assert!(o.review_rounds_stalled());

        o.handle_review_sweep(&[PrObservation {
            pr: coord(12),
            lookup: PrLookup::Gone,
            unchanged_from: Vec::new(),
        }]);
        assert!(
            o.review_unassignable.is_empty(),
            "a retired pull request must not leave a counter behind"
        );
        assert!(!o.review_rounds_stalled());
    }

    /// An author-less row (written before the column existed, or by a caller that supplied none)
    /// may only be serviced by its INCUMBENT: with no author to exclude, any substitution could
    /// hand a teammate their own pull request.
    ///
    /// The incumbent is unavailable here because `bob` has left the roster — after STUDIO-800 the
    /// only thing this path still requires of him. `carol` is idle and eligible and must STILL not
    /// be handed the round; the row waits for `bob` instead.
    #[test]
    fn an_author_less_row_never_substitutes() {
        let (mut o, dispatched) = orch(teams_with(
            true,
            ReviewMode::Ticketless,
            vec![ident("alice", 0), ident("carol", 0)],
        ));
        introduce(
            &o,
            ReviewWatchRow {
                author: String::new(),
                ..row(12, "bob")
            },
        );

        let report = o.handle_review_sweep(&[open_at(12, HEAD_A)]);
        assert_eq!((report.dispatched, report.deferred), (0, 1));
        assert!(dispatched.lock().expect("lock").is_empty());
    }

    // --- the approval terminal and the churn floor ------------------------------------------

    /// Acceptance (§15-c): an approved pull request stops re-reviewing while it stays open, and a
    /// subsequent push re-arms EXACTLY ONE review of the new changes.
    #[test]
    fn an_approved_pull_request_pauses_and_a_push_re_arms_exactly_one() {
        let (mut o, dispatched) = orch(ticketless(&["alice", "bob"]));
        introduce(&o, row(12, "bob"));
        o.handle_review_sweep(&[open_at(12, HEAD_A)]);
        o.running.remove(&review_key(OWNER, REPO, 12, "bob"));
        o.claimed.remove(&review_key(OWNER, REPO, 12, "bob"));
        o.store()
            .mark_review_completed(&key(12, "bob"), HEAD_A, REVIEW_STATUS_APPROVED)
            .expect("approve");

        for _ in 0..3 {
            assert_eq!(
                o.handle_review_sweep(&[open_at(12, HEAD_A)]).dispatched,
                0,
                "an approved pull request must stop re-reviewing while it stays at that head"
            );
        }
        assert_eq!(o.handle_review_sweep(&[open_at(12, HEAD_B)]).dispatched, 1);
        assert_eq!(o.handle_review_sweep(&[open_at(12, HEAD_B)]).dispatched, 0);
        assert_eq!(dispatched.lock().expect("lock").len(), 2);
    }

    /// Acceptance: a churning pull request hits the cap. Each round completes instantly and the
    /// author pushes again — the shape a force-push loop produces — and the budget is finite.
    #[test]
    fn a_churning_pull_request_hits_the_cap() {
        let (mut o, dispatched) = orch(ticketless(&["alice", "bob"]));
        introduce(&o, row(12, "bob"));

        for round in 0..(REVIEW_ROUNDS_PER_PR_CAP + 4) {
            let head = format!("{round:040}");
            o.handle_review_sweep(&[open_at(12, &head)]);
            complete(&mut o, 12, "bob", &head);
        }

        assert_eq!(
            dispatched.lock().expect("lock").len(),
            REVIEW_ROUNDS_PER_PR_CAP,
            "the per-pull-request re-review budget must be finite"
        );
        let over = o.handle_review_sweep(&[open_at(12, HEAD_B)]);
        assert_eq!((over.dispatched, over.deferred), (0, 1));
    }

    // --- the shared review↔author budget (STUDIO-956) ----------------------------------------

    /// A summons-driven author ticket whose work is on pull-request `number` — the shape
    /// `pr_suppressed` stops suppressing once a review's findings summon the author.
    fn author_issue(identifier: &str, number: i64) -> Issue {
        Issue {
            id: format!("ID-{identifier}"),
            identifier: identifier.to_string(),
            title: "t".to_string(),
            state: "In Progress".to_string(),
            linked_pr: true,
            linked_prs: Some(vec![LinkedPRRef {
                owner: OWNER.to_string(),
                repo: REPO.to_string(),
                number,
                merged: false,
            }]),
            ..Default::default()
        }
    }

    /// **Unset ⇒ the author side is untouched.** The legacy cap bounds review rounds only; with no
    /// threshold, an author re-dispatch is never refused and never charges the counter — exactly the
    /// behaviour a daemon built before STUDIO-956 had, which is what makes the whole feature opt-in.
    #[test]
    fn an_unset_threshold_leaves_the_author_side_unbounded() {
        let (mut o, _d) = orch(ticketless(&["alice", "bob"]));
        let iss = author_issue("STUDIO-170", 12);
        // The review half is already deep into — and past — the legacy cap.
        o.review_rounds
            .insert(churn_key(&coord(12)), REVIEW_ROUNDS_PER_PR_CAP);

        let mut author_rounds = 0;
        for _ in 0..11 {
            if o.author_round_budget_spent(&iss) {
                break;
            }
            o.note_author_round(&iss);
            author_rounds += 1;
        }

        assert_eq!(
            author_rounds, 11,
            "with no threshold the author half must be exactly as unbounded as it was before \
             STUDIO-956"
        );
        assert_eq!(
            o.review_rounds.get(&churn_key(&coord(12))),
            Some(&REVIEW_ROUNDS_PER_PR_CAP),
            "and an author round must not consume the legacy review-only cap"
        );
    }

    /// An author round costs one ROUND, not one dispatch: the budget is counted in rounds at every
    /// reviewer count (STUDIO-727), so a two-reviewer pull request charges two dispatches per author
    /// round exactly as it charges two per review round. Under the opt-in threshold, where author
    /// rounds count at all.
    #[test]
    fn an_author_round_charges_one_round_at_every_reviewer_count() {
        let mut teams = adjudicating(&["alice", "bob", "carol"], 8);
        teams.review.reviewers = 2;
        let (mut o, _d) = orch(teams);
        let iss = author_issue("STUDIO-1", 12);
        o.review_rounds.insert(churn_key(&coord(12)), 1);

        o.note_author_round(&iss);
        assert_eq!(o.review_rounds.get(&churn_key(&coord(12))), Some(&3));
    }

    /// **alice round 1 on PR #199, finding 1.** `note_author_round` is placed AFTER the provider
    /// budget gate, so a budget-REFUSED fresh dispatch charges nothing. A ticket held every tick
    /// would otherwise spend its pull request's shared review↔author budget on each poll and reach
    /// adjudication without anyone having run.
    ///
    /// Mutation: move `note_author_round` back above the gate in `dispatch_issue` and the counter
    /// reds to 2.
    #[test]
    fn a_budget_refused_dispatch_charges_no_author_round() {
        use chrono::{SecondsFormat, Utc};
        use rhapsody_store::{OUTCOME_COMPLETED, RunEnd, RunProvenance, RunStart};

        let (mut o, _d) = orch(adjudicating(&["alice"], 5));
        o.eff.as_mut().expect("eff").cfg.claude.model = "claude-opus-4-8".to_string();
        o.eff.as_mut().expect("eff").cfg.budgets.insert(
            "anthropic".to_string(),
            rhapsody_config::ProviderBudget { daily_tokens: 200 },
        );

        // Today's anthropic spend is over the ceiling.
        let started = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
        let id = o
            .store()
            .start_run(RunStart {
                issue_identifier: "MT-seed".to_string(),
                started_at: started.clone(),
                ..Default::default()
            })
            .expect("start");
        o.store()
            .end_run(
                id,
                RunEnd {
                    outcome: OUTCOME_COMPLETED.to_string(),
                    total_tokens: 300,
                    ended_at: started,
                    ..Default::default()
                },
            )
            .expect("end");
        o.store()
            .set_run_provenance(
                id,
                &RunProvenance {
                    provider: "anthropic".to_string(),
                    harness: "claude".to_string(),
                    model: "claude-opus-4-8".to_string(),
                    ..Default::default()
                },
            )
            .expect("provenance");

        // The pull request already carries a shared review budget entry.
        o.review_rounds.insert(churn_key(&coord(12)), 1);
        let iss = author_issue("STUDIO-170", 12);

        o.dispatch_issue(iss, None, None, String::new());

        assert!(
            o.budget_ledger
                .get("STUDIO-170", o.budget_hold_ttl())
                .is_some(),
            "sanity: the dispatch was refused for budget"
        );
        assert_eq!(
            o.review_rounds.get(&churn_key(&coord(12))),
            Some(&1),
            "a refused dispatch must charge no author round"
        );
    }

    /// **Acceptance.** Once the adjudication threshold is reached, BOTH sides stop: the review sweep
    /// refuses the round and the author re-dispatch is refused, on the same counter.
    #[test]
    fn a_reached_threshold_stops_the_review_side_and_the_author_side() {
        let (mut o, dispatched) = orch(adjudicating(&["alice", "bob"], 3));
        let _l = ledger(&mut o);
        introduce(&o, row(12, "bob"));
        let iss = author_issue("STUDIO-170", 12);
        // Three rounds reached.
        o.review_rounds
            .insert(churn_key(&coord(12)), 3 * o.reviewers_per_round());

        let report = o.handle_review_sweep(&[open_at(12, HEAD_A)]);

        assert_eq!(
            (report.dispatched, report.adjudicate.len()),
            (0, 1),
            "the review half stops arming and the manager is asked instead"
        );
        assert!(dispatched.lock().expect("lock").is_empty());
        assert!(
            o.author_round_budget_spent(&iss),
            "and the author half, on the same threshold"
        );
    }

    /// **A decided pull request refuses the author even below the threshold.** A settled decision is
    /// itself a reason to stop the loop, so `author_round_budget_spent` reads the ledger as well as
    /// the counter. The two agree today — a decision is only ever recorded at or above the threshold,
    /// which is exactly why dropping the ledger half of the predicate leaves every other test green.
    #[test]
    fn a_settled_decision_refuses_the_author_side_below_the_threshold() {
        let (mut o, _d) = orch(adjudicating(&["alice", "bob"], 3));
        let l = ledger(&mut o);
        introduce(&o, row(12, "bob"));
        let iss = author_issue("STUDIO-170", 12);
        // One round charged — below the threshold of three — but the manager has already escalated.
        o.review_rounds
            .insert(churn_key(&coord(12)), o.reviewers_per_round());
        l.record(
            &coord(12),
            Adjudication::Escalate {
                head: HEAD_A.to_string(),
                rounds: 1,
                findings: vec![],
                reason: "needs a human".to_string(),
            },
        );

        assert!(
            o.author_round_budget_spent(&iss),
            "a decided pull request stops the author half even below the threshold"
        );
    }

    /// **A decision is never made over an in-flight AUTHOR run.** The counter is charged at
    /// DISPATCH, so the summoned author's run is live from the instant its charge lands — and with
    /// the loop alternating review→author, every EVEN threshold is crossed by that dispatch. The
    /// guard used to look only at review rows, so the manager was handed findings somebody was
    /// actively fixing and a head about to be superseded.
    #[test]
    fn an_in_flight_author_run_defers_the_adjudication() {
        let (mut o, dispatched) = orch(adjudicating(&["alice", "bob"], 3));
        let l = ledger(&mut o);
        introduce(&o, row(12, "bob"));
        // `row(12, _)`'s origin ticket is `handoff:STUDIO-721`; its author is mid-fix.
        o.running.insert(
            "iss-author".to_string(),
            RunningEntry::empty(rhapsody_core::Issue {
                id: "iss-author".to_string(),
                identifier: "STUDIO-721".to_string(),
                ..Default::default()
            }),
        );
        o.review_rounds
            .insert(churn_key(&coord(12)), 3 * o.reviewers_per_round());

        let report = o.handle_review_sweep(&[open_at(12, HEAD_A)]);

        assert!(
            report.adjudicate.is_empty(),
            "the manager must not decide while the author's run is still fixing"
        );
        assert_eq!(
            report.deferred, 1,
            "the decision is deferred to a later sweep, not dropped"
        );
        assert_eq!(
            l.peek(&coord(12)),
            None,
            "no plan was handed out, so nothing was marked in flight"
        );
        assert!(dispatched.lock().expect("lock").is_empty());
    }

    /// **A decision is never made over a live REVIEW round either.** The guard's own doc says why:
    /// new findings could still land and the head is about to move. The author half was pinned by
    /// `an_in_flight_author_run_defers_the_adjudication`; the review half had no test, so setting
    /// `review_live = false` left the whole crate green. Pinned for both states it reads — a
    /// running round and one merely claimed.
    #[test]
    fn a_live_review_round_defers_the_adjudication() {
        for claimed_only in [false, true] {
            let (mut o, dispatched) = orch(adjudicating(&["alice", "bob"], 3));
            let l = ledger(&mut o);
            let r = row(12, "bob");
            let id = review_key(&r.key.owner, &r.key.repo, r.key.number, &r.key.reviewer);
            introduce(&o, r);
            o.claimed.insert(id.clone());
            if !claimed_only {
                o.running.insert(
                    id,
                    RunningEntry::empty(rhapsody_core::Issue {
                        id: "iss-review".to_string(),
                        identifier: "STUDIO-721".to_string(),
                        ..Default::default()
                    }),
                );
            }
            o.review_rounds
                .insert(churn_key(&coord(12)), 3 * o.reviewers_per_round());

            let report = o.handle_review_sweep(&[open_at(12, HEAD_A)]);

            assert!(
                report.adjudicate.is_empty(),
                "the manager must not decide while a review round is live (claimed_only={claimed_only})"
            );
            assert_eq!(
                report.deferred, 1,
                "the decision is deferred to a later sweep, not dropped (claimed_only={claimed_only})"
            );
            assert_eq!(
                l.peek(&coord(12)),
                None,
                "no plan was handed out, so nothing was marked in flight (claimed_only={claimed_only})"
            );
            assert!(dispatched.lock().expect("lock").is_empty());
        }
    }

    /// **A decision is never made over an author run parked in BACKOFF.** A run that failed and is
    /// waiting out its retry is `claimed` but not `running` for the whole backoff delay, and the
    /// retry then re-dispatches (`attempt` is `Some`, so nothing is charged) against a decision the
    /// manager already made — the same harm as deciding over a running fix, reached through the
    /// state the running-only guard did not cover. `LoadSnapshot::from_running_and_retries` counts
    /// this state as live work for the same reason.
    #[test]
    fn an_author_run_parked_in_backoff_defers_the_adjudication() {
        let (mut o, dispatched) = orch(adjudicating(&["alice", "bob"], 3));
        let l = ledger(&mut o);
        introduce(&o, row(12, "bob"));
        // `schedule_retry_for` leaves exactly this: the id claimed, the entry carrying the
        // identifier, and no `running` entry.
        o.claimed.insert("iss-author".to_string());
        o.retry_attempts.insert(
            "iss-author".to_string(),
            retry_entry("iss-author", "STUDIO-721", 1),
        );
        o.review_rounds
            .insert(churn_key(&coord(12)), 3 * o.reviewers_per_round());

        let report = o.handle_review_sweep(&[open_at(12, HEAD_A)]);

        assert!(
            report.adjudicate.is_empty(),
            "the manager must not decide while the author's run is parked in backoff"
        );
        assert_eq!(
            report.deferred, 1,
            "the decision is deferred to a later sweep, not dropped"
        );
        assert_eq!(
            l.peek(&coord(12)),
            None,
            "no plan was handed out, so nothing was marked in flight"
        );
        assert!(dispatched.lock().expect("lock").is_empty());
    }

    /// **A settled escalation stops the loop and is never re-asked.** The bound on a failing turn is
    /// enforced where the turn runs and its audit writes happen
    /// (`reviewadjudicate::perform_adjudication`, pinned in that module's tests); what the control
    /// task owes is to honour the settled ledger entry — arm nothing, hand out no further plan.
    #[test]
    fn a_settled_escalation_stops_the_loop_and_is_not_re_asked() {
        let (mut o, dispatched) = orch(adjudicating(&["alice", "bob"], 3));
        let l = ledger(&mut o);
        introduce(&o, row(12, "bob"));
        o.review_rounds
            .insert(churn_key(&coord(12)), 3 * o.reviewers_per_round());
        l.record(
            &coord(12),
            Adjudication::Escalate {
                head: HEAD_A.to_string(),
                rounds: 3,
                findings: vec![format!("bob asked for changes at {}", &HEAD_A[..7])],
                reason: "the manager turn failed 3 times; no decision could be made".to_string(),
            },
        );

        let report = o.handle_review_sweep(&[open_at(12, HEAD_A)]);

        assert!(
            report.adjudicate.is_empty(),
            "a settled escalation must not be re-asked"
        );
        assert_eq!(report.dispatched, 0, "and no round may arm");
        assert!(dispatched.lock().expect("lock").is_empty());
    }

    /// …and one short of the bound still re-asks: the retry is real, not a first-failure give-up.
    #[test]
    fn a_turn_short_of_its_attempts_re_asks() {
        let (mut o, _d) = orch(adjudicating(&["alice", "bob"], 3));
        let l = ledger(&mut o);
        introduce(&o, row(12, "bob"));
        o.review_rounds
            .insert(churn_key(&coord(12)), 3 * o.reviewers_per_round());
        for _ in 0..(crate::reviewadjudicate::MAX_ADJUDICATION_ATTEMPTS - 1) {
            l.note_failure(&coord(12));
        }

        let report = o.handle_review_sweep(&[open_at(12, HEAD_A)]);

        assert_eq!(
            report.adjudicate.len(),
            1,
            "a turn inside its attempt bound is re-asked"
        );
    }

    /// **A default daemon is byte-identical to today.** A ticket whose pull request no review has
    /// ever charged carries no budget, so a fresh dispatch is never refused and never charged —
    /// which is what keeps a Teams-off (or never-reviewed) installation exactly as it was.
    #[test]
    fn a_pull_request_no_review_has_charged_is_never_bounded() {
        let (mut o, _d) = orch(ticketless(&["alice", "bob"]));
        let iss = author_issue("STUDIO-1", 12);

        assert!(
            !o.author_round_budget_spent(&iss),
            "no budget entry ⇒ nothing is bounded"
        );
        o.note_author_round(&iss);
        assert_eq!(
            o.review_rounds.get(&churn_key(&coord(12))),
            None,
            "a ticket whose pull request was never reviewed must not create a budget"
        );
    }

    // --- the manager adjudication decider (STUDIO-956) ---------------------------------------

    use crate::reviewadjudicate::{Adjudication, AdjudicationLedger};

    /// [`ticketless`] with the opt-in adjudication threshold set.
    fn adjudicating(names: &[&str], threshold: i64) -> Teams {
        let mut teams = ticketless(names);
        teams.review.adjudicate_after_rounds = threshold;
        teams
    }

    fn ledger(o: &mut Orchestrator) -> Arc<AdjudicationLedger> {
        let l = Arc::new(AdjudicationLedger::default());
        o.adjudication_ledger = Some(Arc::clone(&l));
        l
    }

    /// [`ledger`] whose settled decisions are written through to the orchestrator's own store — the
    /// shape `rhapsodyd::run` builds (STUDIO-956).
    fn durable_ledger(
        o: &mut Orchestrator,
        store: Arc<dyn rhapsody_store::Store + Send + Sync>,
    ) -> Arc<AdjudicationLedger> {
        let l = Arc::new(AdjudicationLedger::with_store(store));
        o.adjudication_ledger = Some(Arc::clone(&l));
        l
    }

    /// **Acceptance, and the round-8 blocker.** *"The threshold and the recorded decision survive a
    /// daemon restart — assert it by writing rounds, dropping and rebuilding the Orchestrator from
    /// the same store, and reading the count back."*
    ///
    /// The rounds are charged through the REAL path (`handle_review_sweep`'s dispatch), not by
    /// poking the map, so what is pinned is that charging a round persists it — and the decision is
    /// recorded through the real ledger the off-loop turn writes.
    ///
    /// Why it mattered: `review_rounds` was a bare `HashMap` nothing ever rehydrated, so every
    /// restart refunded every pull request's whole budget. On 2026-09-20 five restarts (each one to
    /// apply a boot-only `teams.yaml` change) produced 46 review runs on one pull request against a
    /// nominal cap of 16, and a pull request the manager had already escalated forgot the decision
    /// and resumed the loop from zero.
    ///
    /// ⚠️ MUTATION (the ticket's): make the round counter in-memory again — drop the
    /// `persist_review_rounds` call from the dispatch site, or the `rehydrate_review_bounds` call
    /// from `boot_recovery` — and this reds. Dropping the ledger's durable write reds the decision
    /// half.
    #[test]
    fn the_round_counter_and_the_decision_survive_a_daemon_restart() {
        let store: Arc<dyn rhapsody_store::Store + Send + Sync> =
            Arc::new(Sqlite::open(StorePath::InMemory).expect("open in-memory store"));

        // --- daemon lifetime one ---
        let (mut o, dispatched) = orch_on(adjudicating(&["alice", "bob"], 3), Arc::clone(&store));
        let l = durable_ledger(&mut o, Arc::clone(&store));
        introduce(&o, row(12, "bob"));
        assert_eq!(
            o.handle_review_sweep(&[open_at(12, HEAD_A)]).dispatched,
            1,
            "one round is dispatched, and charged"
        );
        assert!(!dispatched.lock().expect("lock").is_empty());
        assert_eq!(o.review_rounds.get(&churn_key(&coord(12))), Some(&1));
        // And the manager decides, off-loop, exactly as `perform_adjudication` does.
        l.record(
            &coord(12),
            Adjudication::Escalate {
                head: HEAD_A.to_string(),
                rounds: 3,
                findings: vec!["bob asked for changes at aaa".to_string()],
                reason: "the reviewers disagree about the schema".to_string(),
            },
        );
        drop(o);

        // --- daemon lifetime two, same store ---
        let (mut o2, _d2) = orch_on(adjudicating(&["alice", "bob"], 3), Arc::clone(&store));
        let _l2 = durable_ledger(&mut o2, Arc::clone(&store));
        assert_eq!(
            o2.review_rounds.get(&churn_key(&coord(12))),
            None,
            "a fresh Orchestrator knows nothing until boot recovery runs"
        );

        o2.boot_recovery();

        assert_eq!(
            o2.review_rounds.get(&churn_key(&coord(12))),
            Some(&1),
            "the round this pull request spent must survive the restart that used to refund it"
        );
        assert_eq!(
            o2.adjudication(&coord(12)),
            Some(Adjudication::Escalate {
                head: HEAD_A.to_string(),
                rounds: 3,
                findings: vec!["bob asked for changes at aaa".to_string()],
                reason: "the reviewers disagree about the schema".to_string(),
            }),
            "and so must the manager's decision, with the findings and the reason it named"
        );
    }

    /// The other half of durability: an IN-FLIGHT marker must NOT survive, because the turn that
    /// was going to land it does not. Persisted, it would stop every further round for that pull
    /// request forever with no turn left anywhere to clear it — a permanent freeze in place of the
    /// temporary refund this ticket fixes. So the restarted daemon sees no decision and the next
    /// sweep re-asks.
    ///
    /// MUTATION: make `AdjudicationLedger::mark_in_flight` write through to the store the way
    /// `record` does, and this reds. (`Adjudication::to_stored` already refuses an `InFlight`, which
    /// is why `record` itself cannot be mutated into this defect.)
    #[test]
    fn an_in_flight_decision_does_not_survive_the_restart_that_killed_its_turn() {
        let store: Arc<dyn rhapsody_store::Store + Send + Sync> =
            Arc::new(Sqlite::open(StorePath::InMemory).expect("open in-memory store"));

        let (mut o, _d) = orch_on(adjudicating(&["alice", "bob"], 3), Arc::clone(&store));
        let l = durable_ledger(&mut o, Arc::clone(&store));
        l.mark_in_flight(&coord(12), 3);
        assert!(o.adjudication(&coord(12)).is_some());
        drop(o);

        let (mut o2, _d2) = orch_on(adjudicating(&["alice", "bob"], 3), Arc::clone(&store));
        let _l2 = durable_ledger(&mut o2, Arc::clone(&store));
        o2.boot_recovery();

        assert_eq!(
            o2.adjudication(&coord(12)),
            None,
            "an interrupted adjudication is re-asked, never left stopping the loop forever"
        );
    }

    /// The durability trap the ticket names: a pull request that LEAVES the watch set must not hand
    /// a spent budget to the one that replaces it. A merged, closed or dismissed pull request
    /// deletes its durable row, so a re-introduced, reopened or rebuilt one under the same number
    /// boots with nothing.
    ///
    /// MUTATION: drop the `forget_review_bound` call from `retire_review_pr` and this reds — the
    /// rebuilt pull request boots already at the threshold, its author half frozen, with no
    /// decision anywhere and nothing to clear.
    #[test]
    fn a_reopened_pull_request_does_not_inherit_the_spent_budget() {
        let store: Arc<dyn rhapsody_store::Store + Send + Sync> =
            Arc::new(Sqlite::open(StorePath::InMemory).expect("open in-memory store"));

        let (mut o, _d) = orch_on(adjudicating(&["alice", "bob"], 3), Arc::clone(&store));
        let l = durable_ledger(&mut o, Arc::clone(&store));
        introduce(&o, row(12, "bob"));
        o.handle_review_sweep(&[open_at(12, HEAD_A)]);
        l.record(
            &coord(12),
            Adjudication::Ship {
                head: HEAD_A.to_string(),
                rounds: 3,
            },
        );
        assert_eq!(o.review_rounds.get(&churn_key(&coord(12))), Some(&1));

        // The pull request is merged: the watcher retires it.
        o.handle_review_sweep(&[observed(12, merged_at(HEAD_A))]);
        drop(o);

        let (mut o2, _d2) = orch_on(adjudicating(&["alice", "bob"], 3), Arc::clone(&store));
        let _l2 = durable_ledger(&mut o2, Arc::clone(&store));
        o2.boot_recovery();

        assert_eq!(
            o2.review_rounds.get(&churn_key(&coord(12))),
            None,
            "the retired pull request's budget must not outlive it"
        );
        assert_eq!(
            o2.adjudication(&coord(12)),
            None,
            "nor the decision that was made about it"
        );
    }

    /// The operator's deliberate clear (`POST /api/v1/reviews/clear`) is the escape hatch now that a
    /// restart is not one. It must clear DURABLY: a clear the next boot undoes is worse than no
    /// clear, because the operator watched it succeed.
    ///
    /// MUTATION: drop the `forget_review_bound` call from `handle_review_clear` and this reds.
    #[test]
    fn an_operator_clear_survives_the_restart_too() {
        let store: Arc<dyn rhapsody_store::Store + Send + Sync> =
            Arc::new(Sqlite::open(StorePath::InMemory).expect("open in-memory store"));

        let (mut o, _d) = orch_on(adjudicating(&["alice", "bob"], 3), Arc::clone(&store));
        let l = durable_ledger(&mut o, Arc::clone(&store));
        introduce(&o, row(12, "bob"));
        o.handle_review_sweep(&[open_at(12, HEAD_A)]);
        l.record(
            &coord(12),
            Adjudication::Escalate {
                head: HEAD_A.to_string(),
                rounds: 3,
                findings: Vec::new(),
                reason: "a human is needed".to_string(),
            },
        );

        assert!(matches!(
            o.handle_review_clear(&coord(12)),
            crate::reviewconsole::ReviewControlOutcome::Applied(_)
        ));
        drop(o);

        let (mut o2, _d2) = orch_on(adjudicating(&["alice", "bob"], 3), Arc::clone(&store));
        let _l2 = durable_ledger(&mut o2, Arc::clone(&store));
        o2.boot_recovery();

        assert_eq!(o2.review_rounds.get(&churn_key(&coord(12))), None);
        assert_eq!(
            o2.adjudication(&coord(12)),
            None,
            "the operator cleared it; a restart must not bring the decision back"
        );
    }

    /// **Acceptance.** With the threshold set to 3, a pull request reaching round 3 dispatches NO
    /// further review or author round, and instead produces a manager decision naming the head and
    /// the round count.
    #[test]
    fn a_threshold_of_three_stops_the_loop_and_produces_a_manager_decision() {
        let (mut o, dispatched) = orch(adjudicating(&["alice", "bob"], 3));
        let l = ledger(&mut o);
        introduce(&o, row(12, "bob"));
        // Three rounds already run (one reviewer per round, so three dispatches).
        o.review_rounds
            .insert(churn_key(&coord(12)), 3 * o.reviewers_per_round());

        let report = o.handle_review_sweep(&[open_at(12, HEAD_A)]);

        assert_eq!(
            report.dispatched, 0,
            "no further review round may be dispatched"
        );
        assert_eq!(report.adjudicate.len(), 1, "exactly one manager decision");
        let plan = &report.adjudicate[0];
        assert_eq!(plan.pr, coord(12));
        assert_eq!(plan.rounds, 3);
        assert_eq!(plan.head, HEAD_A);
        assert!(
            dispatched.lock().expect("lock").is_empty(),
            "nothing reached a worker"
        );
        assert_eq!(
            l.peek(&coord(12)),
            Some(Adjudication::InFlight { rounds: 3 }),
            "the decision is marked in flight so the next tick does not re-ask"
        );

        // The author half is stopped too, on the same threshold.
        let iss = author_issue("STUDIO-12", 12);
        assert!(o.author_round_budget_spent(&iss));
    }

    /// **The threshold is not a failure when the loop CONVERGED.** A pull request whose last allowed
    /// round ended with every live row approved at the head has finished; handing it to the manager
    /// would spawn a turn whose prompt falsely asserts "reached its limit without converging" and
    /// lists no findings, and an `ESCALATE` answer would post a false alarm and freeze the author
    /// half for a pull request every reviewer approved. Pinned on the default (`auto_merge: false`)
    /// config, where the turn is the only effect of this branch.
    #[test]
    fn a_converged_pull_request_at_the_threshold_is_not_sent_to_the_manager() {
        let (mut o, dispatched) = orch(adjudicating(&["alice", "bob"], 3));
        let l = ledger(&mut o);
        introduce(&o, approved_row(12, "bob", HEAD_A));
        o.review_rounds
            .insert(churn_key(&coord(12)), 3 * o.reviewers_per_round());

        let report = o.handle_review_sweep(&[open_at(12, HEAD_A)]);

        assert!(
            report.adjudicate.is_empty(),
            "every reviewer approved at the head: the loop converged and the manager has nothing to \
             decide"
        );
        assert_eq!(
            l.peek(&coord(12)),
            None,
            "and no decision is even marked in flight"
        );
        assert_eq!(report.dispatched, 0, "an approved row owes no round");
        assert!(
            dispatched.lock().expect("lock").is_empty(),
            "nothing reached a worker"
        );
    }

    /// Once a decision has LANDED, the loop stays stopped and is not re-asked — and a `ship` verdict
    /// does NOT clear the findings gate: a pull request whose reviewer asked for changes still does
    /// not propose a merge (the merge gates are the merge gates; see the ticket's first ⚠️).
    #[test]
    fn a_settled_ship_verdict_stops_arming_and_leaves_the_merge_gate_alone() {
        let mut teams = adjudicating(&["alice", "bob"], 3);
        teams.review.auto_merge = true;
        let (mut o, dispatched) = orch(teams);
        let l = ledger(&mut o);
        introduce(&o, row(12, "bob"));
        // The reviewer's round at HEAD_A posted findings.
        o.store()
            .mark_review_requested(&key(12, "bob"), HEAD_A)
            .expect("requested");
        o.store()
            .mark_review_completed(&key(12, "bob"), HEAD_A, REVIEW_STATUS_REVIEWED)
            .expect("completed");
        l.record(
            &coord(12),
            Adjudication::Ship {
                head: HEAD_A.to_string(),
                rounds: 3,
            },
        );

        let report = o.handle_review_sweep(&[open_at(12, HEAD_A)]);

        assert_eq!(report.dispatched, 0);
        assert!(
            report.adjudicate.is_empty(),
            "a settled decision must not be re-asked"
        );
        assert!(
            report.merge.is_empty(),
            "a `ship` verdict adjudicates the FINDINGS, never the gates: a changes-requested row \
             still holds the merge back"
        );
        assert!(dispatched.lock().expect("lock").is_empty());
    }

    /// The other half of the README's claim: a settled `ship` whose rows ARE all approved at the head
    /// still reaches `report.merge`. The decision stops the loop; it does not stop a pull request the
    /// gates have cleared. Without this the `propose_auto_merge` call in the settled-decision branch
    /// can be deleted with the whole suite green — the sibling test above only pins the refusal.
    #[test]
    fn a_settled_ship_verdict_still_proposes_the_merge_once_every_row_approved() {
        let mut teams = adjudicating(&["alice", "bob"], 3);
        teams.review.auto_merge = true;
        let (mut o, _d) = orch(teams);
        let l = ledger(&mut o);
        introduce(&o, approved_row(12, "bob", HEAD_A));
        l.record(
            &coord(12),
            Adjudication::Ship {
                head: HEAD_A.to_string(),
                rounds: 3,
            },
        );

        let report = o.handle_review_sweep(&[open_at(12, HEAD_A)]);

        assert_eq!(report.dispatched, 0, "a settled decision arms nothing");
        assert!(
            report.adjudicate.is_empty(),
            "a settled decision must not be re-asked"
        );
        assert_eq!(
            report.merge,
            vec![crate::automerge::AutoMergePlan {
                pr: coord(12),
                head: HEAD_A.to_string(),
                approved_by: vec!["bob".to_string()],
            }],
            "a shipped pull request whose rows are all approved still reaches the merge gate"
        );
    }

    // --- STUDIO-971: a shipped pull request that gets another commit -----------------------------

    /// **Acceptance, named after the pull request that filed it.** Reproduce `makewhatis/rhapsody#202`
    /// exactly: the manager shipped at head `243a790`, a route-back pushed the branch to `daa65d3`,
    /// and three watch rows still sat at older SHAs while the durable count read seven dispatches.
    /// The loop armed nothing for three minutes and the pull request could neither be reviewed nor
    /// merged. It must now arm exactly ONE round at the new head, and a round that comes back with
    /// findings must buy a FRESH adjudication at that head rather than a silent stop.
    ///
    /// MUTATION (the ticket's): gate the decision on `settled()` alone — ignore the recorded head —
    /// and this reds with `dispatched == 0`. Treat every head move as content-changing and the
    /// sibling no-op test below reds instead.
    #[test]
    fn pr_202_a_stale_ship_decision_arms_one_round_at_the_new_head() {
        const STALE_HEAD: &str = "243a790";
        const NEW_HEAD: &str = "daa65d3";
        // Three reviewers, so one round is three dispatches (`reviewers_per_round`). The threshold
        // is 2: the decision was made at `dispatches=7`, which is two whole rounds of three, so the
        // count is already at the threshold when the push arrives — the shape that used to stall.
        let mut teams = adjudicating(&["alice", "bob", "carol", "dave"], 2);
        teams.review.reviewers = 3;
        let (mut o, dispatched) = orch(teams);
        let l = ledger(&mut o);
        for reviewer in ["bob", "carol", "dave"] {
            introduce(&o, row(202, reviewer));
        }
        // The three rows were re-armed by the author's push, each against a head nobody read.
        for (reviewer, sha) in [
            ("bob", "db9a13d"),
            ("carol", "ceb73dc"),
            ("dave", "ceb73dc"),
        ] {
            o.store()
                .mark_review_requested(&key(202, reviewer), sha)
                .expect("requested");
        }
        // `rhapsody_review_bound`: dispatches=7, decision=ship, head=243a790 two commits stale.
        o.review_rounds.insert(churn_key(&coord(202)), 7);
        l.record(
            &coord(202),
            Adjudication::Ship {
                head: STALE_HEAD.to_string(),
                rounds: 7 / o.reviewers_per_round(),
            },
        );

        let report = o.handle_review_sweep(&[open_at(202, NEW_HEAD)]);

        assert_eq!(
            report.dispatched, 3,
            "exactly one round — three reviewers, three dispatches — arms at the new head"
        );
        assert!(
            report.adjudicate.is_empty(),
            "the resumed round is armed BEFORE the manager is asked again"
        );
        assert_eq!(
            o.review_rounds.get(&churn_key(&coord(202))),
            Some(&10),
            "the durable count keeps climbing (7 + one round); it is never reset"
        );

        // The round comes back with findings: the manager must decide again AT THE NEW HEAD — not
        // leave the loop silently stopped as it was before this ticket.
        for reviewer in ["bob", "carol", "dave"] {
            complete(&mut o, 202, reviewer, NEW_HEAD);
        }
        let next = o.handle_review_sweep(&[open_at(202, NEW_HEAD)]);
        assert_eq!(
            next.dispatched, 0,
            "the one resumed round is spent; no second round is armed"
        );
        assert_eq!(
            next.adjudicate.len(),
            1,
            "a round with findings buys a fresh adjudication at the new head"
        );
        assert_eq!(next.adjudicate[0].head, NEW_HEAD);
        assert_eq!(next.adjudicate[0].rounds, 3);
        assert_eq!(
            o.review_rounds.get(&churn_key(&coord(202))),
            Some(&10),
            "clearing the superseded decision before the fresh adjudication leaves the count alone"
        );
        assert_eq!(
            dispatched.lock().expect("lock").len(),
            3,
            "exactly the one resumed round reached a worker"
        );

        // ⚠️ The fresh adjudication CLEARS the superseded decision before it marks the plan in
        // flight (B2). Without the `ledger.clear`, `mark_in_flight`'s `or_insert` keeps the stale
        // settled `Ship`, this third sweep sees `settled()` true, `governs(head_b)` false and a spent
        // round, and hands out a SECOND plan while the manager is still deciding — one per sweep.
        assert!(
            !l.peek(&coord(202)).expect("a plan is in flight").settled(),
            "the superseded decision is cleared and the fresh plan is marked in flight, not left \
             settled at the old head"
        );
        let third = o.handle_review_sweep(&[open_at(202, NEW_HEAD)]);
        assert!(
            third.adjudicate.is_empty(),
            "a turn is already out for this head; a second plan must not be handed out on the next \
             sweep"
        );
    }

    /// **B1: a resumed round with FEWER rows than `review.reviewers` still counts as spent.** The
    /// original resume check compared the floor-divided dispatch counter against the decision's
    /// round count, so a round that dispatched fewer rows than the configured reviewer count left
    /// the counter on the same whole multiple — the resumed round stayed "owed" forever, the
    /// threshold branch was never reached, and the pull request stalled exactly as before, one round
    /// later. The resume is now decided from the rows.
    ///
    /// Reproduced with alice's numbers: `reviewers=3`, two rows (bob, carol), `dispatches=6`,
    /// `Ship{head:"243a790", rounds:2}`, threshold 2, head `daa65d3`. The round dispatches 2, the
    /// count reaches 8, and `8 / 3 == 2` never crosses to 3 — the shape the old check stalled on.
    ///
    /// MUTATION: restore `self.rounds_used(pr) <= decision.rounds()` and this reds — the second
    /// sweep adjudicates nothing and dispatches nothing.
    #[test]
    fn a_resumed_round_that_dispatches_fewer_rows_than_reviewers_is_still_spent() {
        const STALE_HEAD: &str = "243a790";
        const NEW_HEAD: &str = "daa65d3";
        let mut teams = adjudicating(&["alice", "bob", "carol"], 2);
        teams.review.reviewers = 3;
        let (mut o, dispatched) = orch(teams);
        let l = ledger(&mut o);
        introduce(&o, row(202, "bob"));
        introduce(&o, row(202, "carol"));
        o.review_rounds.insert(churn_key(&coord(202)), 6);
        l.record(
            &coord(202),
            Adjudication::Ship {
                head: STALE_HEAD.to_string(),
                rounds: 6 / o.reviewers_per_round(),
            },
        );

        let first = o.handle_review_sweep(&[open_at(202, NEW_HEAD)]);
        assert_eq!(
            first.dispatched, 2,
            "the one resumed round arms both live rows"
        );
        assert_eq!(o.review_rounds.get(&churn_key(&coord(202))), Some(&8));
        assert_eq!(
            o.rounds_used(&coord(202)),
            2,
            "sanity: the floor-divided counter has NOT crossed to a new round"
        );

        for reviewer in ["bob", "carol"] {
            complete(&mut o, 202, reviewer, NEW_HEAD);
        }
        let second = o.handle_review_sweep(&[open_at(202, NEW_HEAD)]);
        assert_eq!(
            second.dispatched, 0,
            "the round is spent; nothing more is armed at the head"
        );
        assert_eq!(
            second.adjudicate.len(),
            1,
            "and with the round spent the manager is asked again at the new head"
        );
        assert_eq!(second.adjudicate[0].head, NEW_HEAD);
        assert_eq!(
            dispatched.lock().expect("lock").len(),
            2,
            "exactly one short round reached a worker"
        );
    }

    /// **STUDIO-960 survives (acceptance).** A settled `ship` whose head is rebased to a new SHA with
    /// NO content change must not resume: `unchanged_from` proved the diff is the one the manager
    /// adjudicated, and re-opening the loop on a no-op rebase would undo tonight's largest saving.
    ///
    /// MUTATION (the ticket's): treat every head move as content-changing — ignore `unchanged_from`
    /// — and this reds with a round armed.
    #[test]
    fn a_no_op_head_move_does_not_resume_a_shipped_decision() {
        let mut teams = adjudicating(&["alice", "bob", "carol"], 3);
        teams.review.reviewers = 2;
        let (mut o, dispatched) = orch(teams);
        let l = ledger(&mut o);
        // `bob` read HEAD_A and approved; `carol`'s row is still owed a review, so a resume would
        // actually dispatch (unlike a terminal-only fixture, which STUDIO-960 already carries).
        introduce(&o, approved_row(12, "bob", HEAD_A));
        introduce(&o, row(12, "carol"));
        o.review_rounds
            .insert(churn_key(&coord(12)), 3 * o.reviewers_per_round());
        l.record(
            &coord(12),
            Adjudication::Ship {
                head: HEAD_A.to_string(),
                rounds: 3,
            },
        );

        // The branch is rebased to HEAD_B with a byte-identical diff against the base.
        let proven = vec![HEAD_A.to_string()];
        let report = o.handle_review_sweep(&[open_at_proven(12, HEAD_B, &proven)]);

        assert_eq!(
            report.dispatched, 0,
            "a no-op rebase must not re-open an adjudicated pull request"
        );
        assert!(
            report.adjudicate.is_empty(),
            "and the manager is not asked about a diff it already decided"
        );
        assert!(dispatched.lock().expect("lock").is_empty());
    }

    /// **sol's #2 / alice's partial-proof finding.** `unchanged_from` proves the diff carried by
    /// individual HISTORICAL reviewed SHAs, and `handle_review_head_advanced` correctly carries only
    /// the rows whose own `last_reviewed_sha` appears in it. Reading a non-empty list as proof for
    /// the whole pull request lets one matching row suppress an unmatched row's owed review.
    ///
    /// bob approved at `old_bob`, carol approved at `old_carol`, the manager shipped at `shipped`,
    /// and the new head is proven equal only to `old_bob`. bob's verdict carries; carol's does not
    /// and she is re-armed, so her round MUST arm. A `Ship` at a head the proof does not cover does
    /// not govern: the loop resumes.
    ///
    /// MUTATION: restore `|| !unchanged_from.is_empty()` in `Adjudication::governs` and this reds —
    /// `dispatched` is 0 and carol's owed review is silently suppressed.
    #[test]
    fn a_partial_unchanged_from_proof_does_not_carry_a_ship_at_another_head() {
        const OLD_BOB: &str = "0ldb0b0000000000000000000000000000000000";
        const OLD_CAROL: &str = "ca40101010101010101010101010101010101010";
        const SHIPPED: &str = "5h1pped000000000000000000000000000000000";
        let (mut o, _d) = orch(adjudicating(&["alice", "bob", "carol"], 2));
        let l = ledger(&mut o);
        introduce(&o, approved_row(12, "bob", OLD_BOB));
        introduce(&o, approved_row(12, "carol", OLD_CAROL));
        o.review_rounds
            .insert(churn_key(&coord(12)), 2 * o.reviewers_per_round());
        l.record(
            &coord(12),
            Adjudication::Ship {
                head: SHIPPED.to_string(),
                rounds: 2,
            },
        );

        // The new head is proven byte-identical only to the head BOB reviewed.
        let proven = vec![OLD_BOB.to_string()];
        let report = o.handle_review_sweep(&[open_at_proven(12, HEAD_B, &proven)]);

        assert_eq!(
            report.dispatched, 1,
            "carol's verdict was not carried, so her owed review of the new head must arm — a \
             partial proof must not suppress it"
        );
        assert!(
            report.adjudicate.is_empty(),
            "the loop resumes at the new head rather than asking the manager about a head it never \
             adjudicated"
        );
        assert_eq!(
            watch_row(&o, 12, "bob").last_reviewed_sha,
            HEAD_B,
            "bob's no-op proof carries his verdict onto the new head"
        );
        assert_eq!(
            watch_row(&o, 12, "carol").requested_sha,
            HEAD_B,
            "carol is re-armed at the new head and her round is the one that arms"
        );
    }

    /// **alice's N1: the per-pull-request hard cap must not make the resumed round forever owed.**
    ///
    /// Once `dispatches >= REVIEW_ROUNDS_PER_PR_CAP * reviewers_per_round` the dispatch loop refuses
    /// every row while leaving it `requested`, so `review_round_due` stays true for good. Deciding
    /// the resumed round purely from the rows then reported it owed forever: the threshold branch
    /// never ran, the stale `ship` was never cleared and the author half stayed blocked — this
    /// ticket's stall again, this time at the cap. A round no dispatch can ever satisfy is not owed;
    /// the manager decides at the new head.
    ///
    /// MUTATION: drop the `round_budget_spent` guard from `resumed_round_owed` and this reds —
    /// `adjudicate` is empty and the loop stalls at the cap.
    #[test]
    fn a_resumed_round_that_the_hard_cap_refuses_is_not_owed() {
        const STALE_HEAD: &str = "243a790";
        const NEW_HEAD: &str = "daa65d3";
        let mut teams = adjudicating(&["alice", "bob"], 2);
        teams.review.reviewers = 1;
        let (mut o, dispatched) = orch(teams);
        let l = ledger(&mut o);
        introduce(&o, row(202, "bob"));
        o.review_rounds.insert(
            churn_key(&coord(202)),
            REVIEW_ROUNDS_PER_PR_CAP * o.reviewers_per_round(),
        );
        l.record(
            &coord(202),
            Adjudication::Ship {
                head: STALE_HEAD.to_string(),
                rounds: REVIEW_ROUNDS_PER_PR_CAP - 1,
            },
        );

        let report = o.handle_review_sweep(&[open_at(202, NEW_HEAD)]);

        assert_eq!(
            report.dispatched, 0,
            "the per-pull-request cap refuses every row, so no review can arm"
        );
        assert_eq!(
            report.adjudicate.len(),
            1,
            "a round that can never be dispatched is not owed: the manager is asked at the new head"
        );
        assert_eq!(report.adjudicate[0].head, NEW_HEAD);
        assert!(
            dispatched.lock().expect("lock").is_empty(),
            "and no worker was handed a review"
        );
    }

    /// **An escalation never resumes on the author's own push (acceptance, ⚠️).** The escalation
    /// named a HUMAN as the next actor; resuming it on a push the author made themselves would mean
    /// the escalation never reaches that human. Only `ship` resumes.
    ///
    /// MUTATION (the ticket's): let `Escalate` fall through the resume path too and this reds with a
    /// round armed.
    #[test]
    fn an_escalated_pull_request_does_not_resume_on_a_push() {
        let (mut o, dispatched) = orch(adjudicating(&["alice", "bob"], 3));
        let l = ledger(&mut o);
        introduce(&o, row(12, "bob"));
        o.review_rounds
            .insert(churn_key(&coord(12)), 3 * o.reviewers_per_round());
        l.record(
            &coord(12),
            Adjudication::Escalate {
                head: HEAD_A.to_string(),
                rounds: 3,
                findings: vec!["bob asked for changes at aaaaaaa".to_string()],
                reason: "a human is needed".to_string(),
            },
        );

        let report = o.handle_review_sweep(&[open_at(12, HEAD_B)]);

        assert_eq!(
            report.dispatched, 0,
            "an escalation is a human's; a push does not re-open the loop"
        );
        assert!(report.adjudicate.is_empty());
        assert!(dispatched.lock().expect("lock").is_empty());
        assert_eq!(
            l.peek(&coord(12)).map(|d| d.head().to_string()),
            Some(HEAD_A.to_string()),
            "and the escalation still stands, still naming its head"
        );
    }

    /// **Acceptance: the durable count is NOT reset when the superseded decision clears.** The
    /// decision and the count are separate facts sharing one row — the count is "how much has been
    /// spent on this pull request", the decision is "what the manager concluded about one specific
    /// head". Clearing the superseded decision must leave the count climbing, because the rising
    /// count is what lets the manager see churn.
    ///
    /// MUTATION (the ticket's): clear with `forget_review_bound` (both halves) instead of
    /// `AdjudicationLedger::clear` and this reds — the durable row is gone.
    #[test]
    fn superseding_a_ship_decision_does_not_reset_the_durable_round_count() {
        let store: Arc<dyn rhapsody_store::Store + Send + Sync> =
            Arc::new(Sqlite::open(StorePath::InMemory).expect("open in-memory store"));
        let (mut o, _d) = orch_on(adjudicating(&["alice", "bob"], 3), Arc::clone(&store));
        let l = durable_ledger(&mut o, Arc::clone(&store));
        introduce(&o, row(12, "bob"));
        o.review_rounds.insert(churn_key(&coord(12)), 3);
        l.record(
            &coord(12),
            Adjudication::Ship {
                head: HEAD_A.to_string(),
                rounds: 3,
            },
        );
        assert_eq!(
            store.load_review_bounds().expect("bounds").len(),
            1,
            "sanity: the decision is durable"
        );

        // A push to a new head: the resumed round is armed, spending the one round.
        assert_eq!(o.handle_review_sweep(&[open_at(12, HEAD_B)]).dispatched, 1);

        // It returns findings: the fresh adjudication clears the superseded decision without
        // touching the count beside it in the same row.
        complete(&mut o, 12, "bob", HEAD_B);
        let next = o.handle_review_sweep(&[open_at(12, HEAD_B)]);
        assert_eq!(next.adjudicate.len(), 1);
        assert_eq!(next.adjudicate[0].head, HEAD_B);

        let bounds = store.load_review_bounds().expect("bounds");
        assert_eq!(
            bounds.len(),
            1,
            "the durable row survives the superseded decision"
        );
        assert_eq!(
            bounds[0].dispatches, 4,
            "the count is 3 + the one resumed round; a clear must not reset it"
        );
        assert_eq!(
            o.review_rounds.get(&churn_key(&coord(12))),
            Some(&4),
            "and the in-memory count agrees"
        );
    }

    /// The escalation carries the open findings, so a human gets the specific findings rather than
    /// "needs a human". Pinned at the plan the control task hands over.
    #[test]
    fn the_plan_names_the_open_findings_at_the_head() {
        let (mut o, _d) = orch(adjudicating(&["alice", "bob"], 3));
        let _l = ledger(&mut o);
        introduce(&o, row(12, "bob"));
        o.store()
            .mark_review_requested(&key(12, "bob"), HEAD_A)
            .expect("requested");
        o.store()
            .mark_review_completed(&key(12, "bob"), HEAD_A, REVIEW_STATUS_REVIEWED)
            .expect("completed");
        o.review_rounds
            .insert(churn_key(&coord(12)), 3 * o.reviewers_per_round());

        let report = o.handle_review_sweep(&[open_at(12, HEAD_A)]);
        let plan = &report.adjudicate[0];
        assert_eq!(
            plan.findings,
            vec![format!("bob asked for changes at {}", &HEAD_A[..7])]
        );
    }

    /// **The EVEN-threshold shape, where a blank prompt was the rule rather than the exception.**
    ///
    /// With the loop alternating review→author, an even threshold is crossed by the AUTHOR's own
    /// summoned dispatch, and the deferral then holds the decision until their run ends — which is
    /// after they have pushed. By then every row has been re-armed to `requested`, so a finding
    /// filter keyed on `status == reviewed && last_reviewed_sha == head` names nothing at all and
    /// the manager is asked to decide on a blank prompt. The plan must instead name the head that
    /// was actually read and the unread head the author pushed.
    ///
    /// Mutation check: restoring the exact-head/`reviewed`-only filter reds this test.
    #[test]
    fn the_plan_names_the_unread_head_on_an_even_threshold() {
        let (mut o, _d) = orch(adjudicating(&["alice", "bob"], 4));
        let _l = ledger(&mut o);
        introduce(&o, row(12, "bob"));
        // bob read HEAD_A and asked for changes; three rounds are charged through the real site.
        o.store()
            .mark_review_requested(&key(12, "bob"), HEAD_A)
            .expect("requested");
        o.store()
            .mark_review_completed(&key(12, "bob"), HEAD_A, REVIEW_STATUS_REVIEWED)
            .expect("completed");
        o.review_rounds
            .insert(churn_key(&coord(12)), 3 * o.reviewers_per_round());

        // The author's summoned re-dispatch charges the fourth (even) round, and the author's run
        // pushes HEAD_B before it ends.
        let iss = author_issue("STUDIO-956", 12);
        assert!(
            !o.author_round_budget_spent(&iss),
            "at three of four the author's summon is still allowed"
        );
        o.note_author_round(&iss);

        let report = o.handle_review_sweep(&[open_at(12, HEAD_B)]);

        assert_eq!(report.adjudicate.len(), 1, "exactly one manager decision");
        let plan = &report.adjudicate[0];
        assert_eq!(plan.head, HEAD_B);
        assert!(
            !plan.findings.is_empty(),
            "the manager must not be handed a blank prompt when the head has moved: {:?}",
            plan.findings
        );
        assert!(
            plan.findings
                .iter()
                .any(|f| f.contains(&HEAD_A[..7]) && f.contains(&HEAD_B[..7])),
            "the finding names the head that was last read AND the unread head: {:?}",
            plan.findings
        );
    }

    /// A TRUNCATED round at the threshold is a review that never happened; the plan must say that
    /// rather than hand the manager "none recorded" over a round nobody completed.
    #[test]
    fn a_truncated_round_at_the_threshold_names_the_review_that_never_finished() {
        let (mut o, _d) = orch(adjudicating(&["alice", "bob"], 3));
        let _l = ledger(&mut o);
        introduce(&o, row(12, "bob"));
        o.store()
            .mark_review_requested(&key(12, "bob"), HEAD_A)
            .expect("requested");
        o.store()
            .mark_review_completed(&key(12, "bob"), HEAD_A, REVIEW_STATUS_TRUNCATED)
            .expect("completed");
        o.review_rounds
            .insert(churn_key(&coord(12)), 3 * o.reviewers_per_round());

        let report = o.handle_review_sweep(&[open_at(12, HEAD_A)]);
        let plan = &report.adjudicate[0];
        assert!(
            !plan.findings.is_empty(),
            "a truncated round is not 'nothing open': {:?}",
            plan.findings
        );
        assert!(
            plan.findings[0].contains(&HEAD_A[..7]),
            "the unfinished review names the head it was attempted at: {:?}",
            plan.findings
        );
    }

    /// **Unset ⇒ today's behaviour, byte-identical.** No threshold means no plan is ever emitted,
    /// whatever the counter says, and the review and author halves fall back to the legacy cap.
    #[test]
    fn an_unset_threshold_never_adjudicates() {
        let (mut o, _d) = orch(ticketless(&["alice", "bob"]));
        let _l = ledger(&mut o);
        introduce(&o, row(12, "bob"));
        o.review_rounds
            .insert(churn_key(&coord(12)), 3 * o.reviewers_per_round());

        let report = o.handle_review_sweep(&[open_at(12, HEAD_A)]);
        assert!(
            report.adjudicate.is_empty(),
            "an install that never set the threshold must see no adjudication"
        );
        assert_eq!(
            report.dispatched, 1,
            "and rounds still dispatch exactly as before, up to the legacy cap"
        );
        let iss = author_issue("STUDIO-12", 12);
        assert!(
            !o.author_round_budget_spent(&iss),
            "three rounds is far inside the legacy cap, so the author half is still open"
        );
    }

    /// **Acceptance, named for the incident:** the STUDIO-170 shape — eleven summons-driven author
    /// rounds — is bounded at the configured threshold. The author half in isolation, for the reason
    /// the sibling test gives: charging whole cycles would let the review cap stop the loop even if
    /// the author half were removed.
    ///
    /// Mutation check (the ticket's ⚠️): removing the author-side threshold count (letting
    /// [`Orchestrator::author_round_budget_spent`] consult only the legacy cap) makes this run all
    /// eleven rounds and reds it.
    #[test]
    fn the_studio_170_shape_stops_at_the_adjudication_threshold() {
        let (mut o, _d) = orch(adjudicating(&["alice", "bob"], 3));
        let _l = ledger(&mut o);
        let iss = author_issue("STUDIO-170", 12);
        // The review round that first armed the loop.
        o.review_rounds
            .insert(churn_key(&coord(12)), o.reviewers_per_round());

        let mut author_rounds = 0;
        for _ in 0..11 {
            if o.author_round_budget_spent(&iss) {
                break;
            }
            o.note_author_round(&iss);
            author_rounds += 1;
        }

        assert_eq!(
            author_rounds, 2,
            "one review round plus two author rounds reaches the threshold of three; eleven must \
             not all run"
        );
        assert!(o.author_round_budget_spent(&iss));
    }

    /// **A converged pull request is not bounded.** The review half declines to adjudicate a pull
    /// request whose every live row approved at the head — it falls to the ordinary auto-merge path
    /// — so the author half must not refuse a summons on the count alone. That would freeze a
    /// healthy pull request with no decision in the ledger and nothing reporting it.
    ///
    /// Mutation check: refusing on `rounds_used(pr) >= threshold` alone reds this test.
    #[test]
    fn a_converged_pull_request_at_the_threshold_does_not_freeze_the_author_half() {
        let (mut o, _d) = orch(adjudicating(&["alice", "bob"], 3));
        let _l = ledger(&mut o);
        let iss = author_issue("STUDIO-1", 12);
        introduce(&o, approved_row(12, "bob", HEAD_A));
        o.review_rounds
            .insert(churn_key(&coord(12)), 3 * o.reviewers_per_round());

        assert!(
            !o.author_round_budget_spent(&iss),
            "every live row approved at the head: the loop converged, so the author half stays open"
        );
        assert!(
            o.adjudication(&coord(12)).is_none(),
            "and nothing recorded a decision that could explain a refusal"
        );
    }

    /// …but the convergence exemption must not become an unbounded author half: a threshold reached
    /// with a row still holding changes-requested findings still refuses the re-dispatch.
    #[test]
    fn a_churning_pull_request_at_the_threshold_still_refuses_the_author_half() {
        let (mut o, _d) = orch(adjudicating(&["alice", "bob"], 3));
        let _l = ledger(&mut o);
        let iss = author_issue("STUDIO-1", 12);
        let mut r = row(12, "bob");
        r.status = REVIEW_STATUS_REVIEWED.to_string();
        r.last_reviewed_sha = HEAD_A.to_string();
        introduce(&o, r);
        o.review_rounds
            .insert(churn_key(&coord(12)), 3 * o.reviewers_per_round());

        assert!(
            o.author_round_budget_spent(&iss),
            "a live row still holds changes-requested findings, so the loop has not converged"
        );
    }

    /// The author guard is per PULL REQUEST: a ticket linked to a pull request that has reached the
    /// threshold is refused even while a sibling pull request of the same ticket still has budget,
    /// because one loop needing a decision is enough.
    #[test]
    fn a_reached_threshold_on_any_linked_pull_request_refuses_the_author_round() {
        let (mut o, _d) = orch(adjudicating(&["alice", "bob"], 3));
        let mut iss = author_issue("STUDIO-1", 12);
        iss.linked_prs = Some(vec![
            LinkedPRRef {
                owner: OWNER.to_string(),
                repo: REPO.to_string(),
                number: 12,
                merged: false,
            },
            LinkedPRRef {
                owner: OWNER.to_string(),
                repo: REPO.to_string(),
                number: 13,
                merged: false,
            },
        ]);
        // #12 has reached the threshold; #13 has barely started.
        o.review_rounds
            .insert(churn_key(&coord(12)), 3 * o.reviewers_per_round());
        o.review_rounds
            .insert(churn_key(&coord(13)), o.reviewers_per_round());

        assert!(o.author_round_budget_spent(&iss));
    }

    /// …and the budget is counted in ROUNDS at every reviewer count, not in dispatches (STUDIO-727).
    ///
    /// The counter increments once per dispatch, and one round of a two-reviewer pull request costs
    /// two. Comparing the raw counter against the cap would give this configuration four rounds
    /// instead of eight — and an eight-reviewer one exactly one round, for the daemon's whole
    /// lifetime, with nothing above `debug!` to say why re-reviews stopped.
    #[test]
    fn the_churn_budget_is_counted_in_rounds_at_every_reviewer_count() {
        let mut teams = ticketless(&["alice", "bob", "carol"]);
        teams.review.reviewers = 2;
        let (mut o, dispatched) = orch(teams);
        introduce(&o, row(12, "bob"));
        introduce(&o, row(12, "carol"));

        for round in 0..(REVIEW_ROUNDS_PER_PR_CAP + 4) {
            let head = format!("{round:040}");
            o.handle_review_sweep(&[open_at(12, &head)]);
            complete(&mut o, 12, "bob", &head);
            complete(&mut o, 12, "carol", &head);
        }

        assert_eq!(
            dispatched.lock().expect("lock").len(),
            REVIEW_ROUNDS_PER_PR_CAP * 2,
            "eight ROUNDS of two reviewers is sixteen dispatches, not eight"
        );
    }

    /// **The console's re-run reaches a real dispatch, through a SPENT churn budget** (STUDIO-722).
    ///
    /// This is the end-to-end half of slice 8's "re-run enqueues an in-process re-review": the
    /// operator's lever exists precisely for a pull request the daemon has stopped reviewing, and
    /// the two states that stop it are "approved at the current head" and "out of budget". A re-run
    /// that only re-armed the row would accept the click, defer on the cap, and never review — a
    /// button that silently does nothing, which is worse than one that is not there.
    #[test]
    fn an_operator_rerun_dispatches_a_pull_request_the_churn_cap_had_stopped() {
        let (mut o, dispatched) = orch(ticketless(&["alice", "bob"]));
        introduce(&o, row(12, "bob"));

        // Burn the whole budget: one round per head, each completed so the next is due.
        for round in 0..REVIEW_ROUNDS_PER_PR_CAP {
            let head = format!("{round:040}");
            o.handle_review_sweep(&[open_at(12, &head)]);
            complete(&mut o, 12, "bob", &head);
        }
        let spent = dispatched.lock().expect("lock").len();
        assert_eq!(spent, REVIEW_ROUNDS_PER_PR_CAP);

        // The cap now refuses even a genuine push — which is what it is for.
        assert_eq!(o.handle_review_sweep(&[open_at(12, HEAD_B)]).dispatched, 0);
        assert_eq!(dispatched.lock().expect("lock").len(), spent);

        // The operator overrides it from the authenticated console.
        assert_eq!(
            o.handle_review_rerun(&coord(12)),
            crate::reviewconsole::ReviewControlOutcome::Applied(1)
        );
        assert_eq!(
            o.handle_review_sweep(&[open_at(12, HEAD_B)]).dispatched,
            1,
            "the re-run must reach a dispatch, not merely a re-armed row"
        );
        assert_eq!(dispatched.lock().expect("lock").len(), spent + 1);
        assert_eq!(watch_row(&o, 12, "bob").requested_sha, HEAD_B);
    }

    /// **A re-run buys exactly the re-read it asked for, and not a fresh budget** (STUDIO-722).
    ///
    /// The cap (§14.2) is the floor under a force-push loop, so an operator clicking through it must
    /// not reset it: clearing the counter outright would hand an already-runaway pull request eight
    /// more UNATTENDED rounds, which is the very cost the cap exists to bound. Refunding one round
    /// gives the operator the read they asked for and leaves the cap holding the line behind them.
    #[test]
    fn an_operator_rerun_refunds_one_round_and_not_the_whole_budget() {
        let (mut o, dispatched) = orch(ticketless(&["alice", "bob"]));
        introduce(&o, row(12, "bob"));

        for round in 0..REVIEW_ROUNDS_PER_PR_CAP {
            let head = format!("{round:040}");
            o.handle_review_sweep(&[open_at(12, &head)]);
            complete(&mut o, 12, "bob", &head);
        }
        let spent = dispatched.lock().expect("lock").len();

        // One re-run, one round.
        assert_eq!(
            o.handle_review_rerun(&coord(12)),
            crate::reviewconsole::ReviewControlOutcome::Applied(1)
        );
        assert_eq!(o.handle_review_sweep(&[open_at(12, HEAD_B)]).dispatched, 1);
        complete(&mut o, 12, "bob", HEAD_B);
        assert_eq!(dispatched.lock().expect("lock").len(), spent + 1);

        // And then the cap is holding again: the NEXT push is deferred exactly as it was before the
        // operator intervened, rather than running free for another whole budget.
        assert_eq!(
            o.handle_review_sweep(&[open_at(12, HEAD_C)]).dispatched,
            0,
            "a re-run must not reset the churn budget, only refund the round it spent"
        );
        assert_eq!(dispatched.lock().expect("lock").len(), spent + 1);
    }

    /// The other half: a dismissal stops the watcher dead. The pull request is still open and its
    /// head still moves, and neither fact brings it back — the row left the watch set.
    #[test]
    fn an_operator_dismissal_stops_the_watcher_dispatching_that_pull_request() {
        let (mut o, dispatched) = orch(ticketless(&["alice", "bob"]));
        introduce(&o, row(12, "bob"));
        introduce(&o, row(13, "bob"));

        assert_eq!(
            o.handle_review_dismiss(&coord(12)),
            crate::reviewconsole::ReviewControlOutcome::Applied(1)
        );

        assert!(
            !o.review_watch_coords().iter().any(|w| w.pr == coord(12)),
            "a dismissed pull request is not even polled"
        );
        assert_eq!(o.handle_review_sweep(&[open_at(12, HEAD_A)]).dispatched, 0);
        assert!(dispatched.lock().expect("lock").is_empty());

        // Its neighbour is untouched: dismissal is per pull request, not a global stop.
        assert_eq!(o.handle_review_sweep(&[open_at(13, HEAD_A)]).dispatched, 1);
    }

    /// The budget belongs to a pull request, not to the daemon: a second pull request gets its own,
    /// and a retired one gives its entry back.
    #[test]
    fn the_churn_budget_is_per_pull_request_and_released_on_retirement() {
        let (mut o, _d) = orch(ticketless(&["alice", "bob"]));
        introduce(&o, row(12, "bob"));
        o.handle_review_sweep(&[open_at(12, HEAD_A)]);
        assert_eq!(o.review_rounds.get("makewhatis/rhapsody#12"), Some(&1));

        introduce(&o, row(13, "bob"));
        complete(&mut o, 12, "bob", HEAD_A);
        o.handle_review_sweep(&[open_at(13, HEAD_A)]);
        assert_eq!(o.review_rounds.get("makewhatis/rhapsody#13"), Some(&1));

        o.handle_review_sweep(&[observed(12, PrLookup::Gone)]);
        assert_eq!(o.review_rounds.get("makewhatis/rhapsody#12"), None);
    }

    // --- N > 1 --------------------------------------------------------------------------------

    /// Acceptance (N>1): a crashed SECOND reviewer's review is re-dispatched — the pull request is
    /// not "reviewed" until every required reviewer has recorded the SHA. Per-(PR, reviewer) rows
    /// are what make that true: the first completer cannot stamp the pull request done.
    #[test]
    fn a_crashed_second_reviewer_is_re_dispatched() {
        let (mut o, dispatched) = orch(ticketless(&["alice", "bob", "carol"]));
        introduce(&o, row(12, "bob"));
        introduce(&o, row(12, "carol"));

        assert_eq!(o.handle_review_sweep(&[open_at(12, HEAD_A)]).dispatched, 2);
        // bob finishes; carol crashes.
        complete(&mut o, 12, "bob", HEAD_A);
        crash(&mut o, 12, "carol");

        let report = o.handle_review_sweep(&[open_at(12, HEAD_A)]);
        assert_eq!(
            report.dispatched, 1,
            "the crashed second reviewer's round must be re-dispatched at the same head"
        );
        assert_eq!(
            reviewers_of(&dispatched),
            vec!["bob".to_string(), "carol".to_string(), "carol".to_string()],
            "and bob, who finished, must not be asked again"
        );
    }

    /// Two reviewers of one pull request are never the same teammate: a substitution excludes the
    /// other rows' reviewers, so a capped incumbent cannot be replaced by their own peer.
    #[test]
    fn a_substitution_never_doubles_up_one_reviewer() {
        let (mut o, dispatched) = orch(teams_with(
            true,
            ReviewMode::Ticketless,
            vec![
                ident("alice", 0),
                ident("bob", 1),
                ident("carol", 0),
                ident("dave", 0),
            ],
        ));
        introduce(&o, row(12, "bob"));
        introduce(&o, row(12, "carol"));
        let mut busy = RunningEntry::empty(rhapsody_core::Issue {
            id: "iss-9".to_string(),
            identifier: "STUDIO-999".to_string(),
            ..Default::default()
        });
        busy.identity = "bob".to_string();
        o.running.insert("iss-9".to_string(), busy);

        o.handle_review_sweep(&[open_at(12, HEAD_A)]);

        let mut picked = reviewers_of(&dispatched);
        picked.sort();
        assert_eq!(picked, vec!["carol".to_string(), "dave".to_string()]);
    }

    /// A reassigned incumbent's row is RETIRED, not left standing beside the substitute's. It is
    /// the SAME required review, and two rows would make the pull request owe two of them forever —
    /// `review_round_due` would go on answering true for the incumbent at every head, for a reviewer
    /// nobody is waiting on.
    ///
    /// This invariant used to ride as a second, unrelated assertion inside the capacity test that
    /// STUDIO-800 re-levered, which is how it came within one `assert_ne!` of being lost: reversing
    /// that test's SUBJECT reversed its passenger with it. It gets its own test here, named for the
    /// branch it guards, so that deleting the `drop_review_watch` call under `if reassigned` reds a
    /// test whose name says what broke.
    #[test]
    fn a_reassigned_incumbents_row_is_retired() {
        let (mut o, dispatched) = orch(ticketless(&["alice", "carol"]));
        // `bob` has left the roster, so the round cannot stay with him and must be reassigned.
        introduce(&o, row(12, "bob"));

        let report = o.handle_review_sweep(&[open_at(12, HEAD_A)]);

        assert_eq!(report.dispatched, 1);
        assert_eq!(reviewers_of(&dispatched), vec!["carol".to_string()]);
        assert_eq!(
            watch_row(&o, 12, "bob").status,
            REVIEW_STATUS_DROPPED,
            "a reassigned incumbent's row must leave the watch set, or the pull request owes \
             bob's required review forever"
        );
        assert_eq!(
            watch_row(&o, 12, "carol").requested_sha,
            HEAD_A,
            "and the substitute's row is the one now carrying the round"
        );
    }

    /// Two rows of one pull request BOTH reassigned in the same tick must not land on the same
    /// substitute. The tick's opening snapshot goes stale the moment the first row is reassigned —
    /// it still names the retired reviewer as a peer and does not name the substitute — so the
    /// second row would otherwise be handed a teammate who already holds one of this pull request's
    /// required reviews.
    #[test]
    fn two_reassignments_in_one_tick_do_not_land_on_the_same_substitute() {
        let (mut o, dispatched) = orch(teams_with(
            true,
            ReviewMode::Ticketless,
            vec![ident("alice", 0), ident("carol", 0), ident("erin", 0)],
        ));
        introduce(&o, row(12, "bob"));
        introduce(&o, row(12, "dave"));
        // Both incumbents have left the roster, so both rounds must be reassigned. (Until
        // STUDIO-800 the lever here was their `max_concurrent`; capacity no longer moves a round,
        // so this uses a reason that still does.) `erin` carries a standing load so that `carol`
        // STAYS the least-loaded candidate even after taking the first round — without which the
        // live load snapshot alone would separate the two, and this test would pass on the stale
        // peer set it exists to catch.
        for (n, who) in [("iss-3", "erin"), ("iss-4", "erin")] {
            let mut busy = RunningEntry::empty(rhapsody_core::Issue {
                id: n.to_string(),
                identifier: format!("STUDIO-{n}"),
                ..Default::default()
            });
            busy.identity = who.to_string();
            o.running.insert(n.to_string(), busy);
        }

        let report = o.handle_review_sweep(&[open_at(12, HEAD_A)]);

        assert_eq!(report.dispatched, 2);
        let mut picked = reviewers_of(&dispatched);
        picked.sort();
        assert_eq!(
            picked,
            vec!["carol".to_string(), "erin".to_string()],
            "one substitute must not take both of a pull request's required reviews"
        );
    }

    // --- gating and the dispatch-side allowlist ----------------------------------------------

    /// Acceptance: Teams off, or a mode that is not `ticketless`, and the watcher is dormant — it
    /// polls nothing and decides nothing, even with a watch set full of live rows.
    #[test]
    fn the_watcher_is_dormant_off_the_ticketless_path() {
        for teams in [
            teams_with(false, ReviewMode::Ticketless, vec![ident("bob", 0)]),
            teams_with(true, ReviewMode::Off, vec![ident("bob", 0)]),
            teams_with(true, ReviewMode::Tickets, vec![ident("bob", 0)]),
        ] {
            let (mut o, dispatched) = orch(teams.clone());
            introduce(&o, row(12, "bob"));

            assert!(o.review_watch_coords().is_empty(), "{teams:?}");
            assert_eq!(
                o.handle_review_sweep(&[open_at(12, HEAD_A)]),
                ReviewSweepReport::default(),
                "{teams:?}"
            );
            assert!(dispatched.lock().expect("lock").is_empty(), "{teams:?}");
            assert_eq!(watch_row(&o, 12, "bob").status, REVIEW_STATUS_REQUESTED);
        }
    }

    /// Added acceptance (a): the dispatch-side resolver re-checks the watched-repo allowlist. A row
    /// whose repository a config reload has since disabled is NOT dispatched — the stored row is
    /// never taken on trust.
    #[test]
    fn a_row_whose_repo_is_no_longer_configured_is_not_dispatched() {
        let (mut o, dispatched) = orch(ticketless(&["alice", "bob"]));
        introduce(&o, row(12, "bob"));
        // The operator disables the project (or repoints it) between introduction and this tick.
        o.eff.as_mut().expect("eff").projects[0].disabled = true;

        let report = o.handle_review_sweep(&[open_at(12, HEAD_A)]);

        assert_eq!((report.dispatched, report.deferred), (0, 1));
        assert!(dispatched.lock().expect("lock").is_empty());
        assert_eq!(
            watch_row(&o, 12, "bob").requested_sha,
            "",
            "nothing may be recorded as dispatched"
        );
        // …and the head-advance re-arm is refused for the same reason (item (a)'s other half).
        assert_eq!(report.armed, 0);
    }

    /// The same check, on its own: a coordinate no configured project owns resolves to no clone URL
    /// at all, so there is nothing for `dispatch_review` to route with.
    #[test]
    fn the_dispatch_side_allowlist_resolves_only_enabled_configured_projects() {
        let (mut o, _d) = orch(ticketless(&["alice", "bob"]));
        assert_eq!(
            o.review_repo_url(OWNER, REPO),
            Some(REPO_URL.to_string()),
            "case-insensitively, on the parsed owner/repo rather than the URL text"
        );
        assert_eq!(
            o.review_repo_url("MAKEWHATIS", "RHAPSODY"),
            Some(REPO_URL.to_string())
        );
        assert_eq!(o.review_repo_url("attacker", "evil"), None);
        o.eff.as_mut().expect("eff").projects[0].disabled = true;
        assert_eq!(o.review_repo_url(OWNER, REPO), None);
    }

    /// The poll list is the live watch set, distinct by COORDINATE: N reviewers of one pull request
    /// cost one `gh` call, not N.
    #[test]
    fn the_poll_list_is_distinct_by_coordinate() {
        let (o, _d) = orch(ticketless(&["alice", "bob", "carol"]));
        introduce(&o, row(12, "bob"));
        introduce(&o, row(12, "carol"));
        introduce(&o, row(13, "bob"));

        let prs: Vec<PrCoord> = o.review_watch_coords().into_iter().map(|w| w.pr).collect();
        assert_eq!(prs, vec![coord(12), coord(13)]);
    }

    /// Each polled coordinate carries the UNION of its rows' reviewed SHAs, de-duplicated
    /// (STUDIO-960): two reviewers can sit at two different reviewed heads and the comparison must
    /// be able to prove either. A row that has never completed contributes nothing.
    #[test]
    fn the_poll_list_carries_the_union_of_its_rows_reviewed_shas() {
        let (o, _d) = orch(ticketless(&["alice", "bob", "carol"]));
        introduce(&o, row(12, "bob"));
        introduce(&o, row(12, "carol"));
        introduce(&o, row(13, "bob"));
        o.store()
            .mark_review_completed(&key(12, "bob"), HEAD_A, REVIEW_STATUS_APPROVED)
            .expect("bob completed");
        o.store()
            .mark_review_completed(&key(12, "carol"), HEAD_B, REVIEW_STATUS_REVIEWED)
            .expect("carol completed");
        o.store()
            .mark_review_requested(&key(13, "bob"), HEAD_C)
            .expect("bob dispatched");

        let got = o.review_watch_coords();
        let mut twelve = got
            .iter()
            .find(|w| w.pr == coord(12))
            .expect("the pull request is polled")
            .reviewed_shas
            .clone();
        twelve.sort();
        assert_eq!(twelve, vec![HEAD_A.to_string(), HEAD_B.to_string()]);

        let thirteen = got
            .iter()
            .find(|w| w.pr == coord(13))
            .expect("the pull request is polled");
        assert!(
            thirteen.reviewed_shas.is_empty(),
            "a never-reviewed row contributes no reviewed SHA"
        );
        assert_eq!(
            thirteen.requested_shas,
            vec![HEAD_C.to_string()],
            "a dispatched head is carried so the comparison can tell it apart from a move"
        );
    }

    /// The daemon-wide dispatch budget is honoured, not just each identity's. Twenty pull requests
    /// coming due in one tick must not spawn twenty agents past a cap the operator set.
    #[test]
    fn the_daemon_wide_concurrency_cap_bounds_one_tick() {
        let (mut o, dispatched) = orch(ticketless(&["alice", "bob", "carol", "dave"]));
        o.eff.as_mut().expect("eff").max_concurrent = 2;
        for n in 12..16 {
            introduce(&o, row(n, "bob"));
        }

        let report = o.handle_review_sweep(&[
            open_at(12, HEAD_A),
            open_at(13, HEAD_A),
            open_at(14, HEAD_A),
            open_at(15, HEAD_A),
        ]);

        assert_eq!(report.dispatched, 2, "the global cap must bound one tick");
        assert_eq!(report.deferred, 2, "and the rest are deferred, not lost");
        assert_eq!(dispatched.lock().expect("lock").len(), 2);
    }

    /// One busy IMPLEMENTATION run for `identity`, keyed by `id`. It carries no `review`
    /// coordinates, so the separate review budget (which counts TICKETLESS review runs) leaves it
    /// out — exactly the shape of the four implementations that held all four slots in the incident.
    fn add_impl_run(o: &mut Orchestrator, id: &str, identity: &str) {
        let mut re = RunningEntry::empty(crate::testsupport::issue(id, id, "Todo"));
        re.identity = identity.to_string();
        o.running.insert(id.to_string(), re);
    }

    /// Records one FINISHED run of `identifier`, so the reconciliation sweep can date the row.
    fn finished_run(o: &Orchestrator, identifier: &str, started: &str, ended: &str) {
        let id = o
            .store()
            .start_run(rhapsody_store::RunStart {
                issue_identifier: identifier.to_string(),
                started_at: started.to_string(),
                ..Default::default()
            })
            .expect("start_run");
        o.store()
            .end_run(
                id,
                rhapsody_store::RunEnd {
                    outcome: "completed".to_string(),
                    ended_at: ended.to_string(),
                    ..Default::default()
                },
            )
            .expect("end_run");
    }

    /// STUDIO-950, named after makewhatis/strava#31 (2026-09-20): four implementation runs held all
    /// four global slots and a review round waited over an hour for one. With
    /// `agent.max_concurrent_reviews` set, reviews draw their OWN pool, so the round that would
    /// CLEAR a pull request — and thereby free an implementation slot — is no longer queued behind
    /// the work spending the slots.
    ///
    /// Mutation check: delete the separate budget and restore the shared draw, and this reds — it
    /// asserts the round ran *while implementations held the global cap*, which is the whole
    /// property, not merely that some review ran.
    #[test]
    fn strava_31_a_review_round_dispatches_while_implementations_hold_the_global_budget() {
        let (mut o, dispatched) = orch(ticketless(&["bob"]));
        {
            let eff = o.eff.as_mut().expect("eff");
            eff.max_concurrent = 4;
            eff.max_concurrent_reviews = Some(2);
        }
        for i in 0..4 {
            add_impl_run(&mut o, &format!("impl-{i}"), "alice");
        }
        introduce(&o, row(31, "bob"));

        let report = o.handle_review_sweep(&[open_at(31, HEAD_A)]);

        assert_eq!(
            report.dispatched, 1,
            "the review must not queue behind a full implementation budget"
        );
        assert_eq!(report.deferred, 0);
        assert_eq!(dispatched.lock().expect("lock").len(), 1);
    }

    /// The behaviour-preservation gate: the SAME fixture with `max_concurrent_reviews` left unset —
    /// every install that never writes the key — keeps the shared draw, so the round is deferred
    /// because implementations hold the one global budget. This test passes against the code before
    /// and after STUDIO-950, which is what "unset preserves today exactly" means.
    #[test]
    fn an_unset_review_budget_keeps_the_shared_global_draw() {
        let (mut o, dispatched) = orch(ticketless(&["bob"]));
        o.eff.as_mut().expect("eff").max_concurrent = 4;
        // `max_concurrent_reviews` deliberately left `None` — the pre-STUDIO-950 install.
        for i in 0..4 {
            add_impl_run(&mut o, &format!("impl-{i}"), "alice");
        }
        introduce(&o, row(31, "bob"));

        let report = o.handle_review_sweep(&[open_at(31, HEAD_A)]);

        assert_eq!(report.dispatched, 0);
        assert_eq!(
            report.deferred, 1,
            "an unset key must keep reviews on the shared global budget"
        );
        assert!(dispatched.lock().expect("lock").is_empty());
    }

    /// STUDIO-950: the capacity-hold log names the count for the ACTIVE pool. In shared mode (the
    /// key unset) that is every running run, not just the reviews — four implementations spending
    /// the budget must read `holding=4`, not the misleading `0` a reviews-only count produced.
    ///
    /// Mutation check: make `holding` unconditionally `running_ticketless_reviews()` and the
    /// assertion reds (`Some("0")` vs `Some("4")`).
    #[test]
    fn a_capacity_hold_in_shared_mode_names_the_shared_holders() {
        let (mut o, _d) = orch(ticketless(&["bob"]));
        o.eff.as_mut().expect("eff").max_concurrent = 4;
        // `max_concurrent_reviews` deliberately left `None` — the shared budget.
        for i in 0..4 {
            add_impl_run(&mut o, &format!("impl-{i}"), "alice");
        }
        introduce(&o, row(31, "bob"));

        let (report, events) = capture_events(|| o.handle_review_sweep(&[open_at(31, HEAD_A)]));

        assert_eq!(report.deferred, 1);
        let hold = events
            .iter()
            .find(|e| {
                e.message
                    .contains("daemon-wide concurrency budget is spent")
            })
            .expect("the capacity hold must be logged");
        assert_eq!(
            hold.fields.get("holding").map(String::as_str),
            Some("4"),
            "the hold log must name what actually spent the shared pool"
        );
    }

    /// With the key set, review runs never exceed their OWN budget — four rounds due in one tick and
    /// a budget of two dispatch two and defer two, whatever the implementation budget is doing.
    ///
    /// It also pins the SEPARATE-mode capacity hold, the only place `CapacityHold::separate` and its
    /// holder count are recorded — and therefore which budget knob the reconciliation WARN tells the
    /// operator to turn. Inverting the `separate` mapping or reporting the reviews-only count in the
    /// wrong arm reds here.
    #[test]
    fn review_runs_never_exceed_their_own_budget() {
        let (mut o, dispatched) = orch(ticketless(&["bob"]));
        {
            let eff = o.eff.as_mut().expect("eff");
            eff.max_concurrent = 10;
            eff.max_concurrent_reviews = Some(2);
        }
        for n in 31..35 {
            introduce(&o, row(n, "bob"));
        }

        let report = o.handle_review_sweep(&[
            open_at(31, HEAD_A),
            open_at(32, HEAD_A),
            open_at(33, HEAD_A),
            open_at(34, HEAD_A),
        ]);

        assert_eq!(
            report.dispatched, 2,
            "the review budget must bound the tick"
        );
        assert_eq!(report.deferred, 2);
        assert_eq!(dispatched.lock().expect("lock").len(), 2);

        // The two deferred rounds each record a hold in SEPARATE mode, naming the review pool's own
        // holders (the two reviews just dispatched, not the implementation count) and the knob that
        // would actually free a review slot.
        for n in [33, 34] {
            let hold = o
                .review_capacity_held
                .get(&review_key(OWNER, REPO, n, "bob"))
                .copied()
                .unwrap_or_else(|| panic!("round {n} must record a capacity hold"));
            assert_eq!(
                (hold.holders, hold.separate),
                (2, true),
                "a separate-mode hold names the review pool's holders and its own budget knob"
            );
        }

        // ...and the reconciliation WARN names `agent.max_concurrent_reviews`, not the implementation
        // knob — the whole point of the `separate` bit. It needs a dated origin run to reach the
        // report at all.
        finished_run(
            &o,
            "STUDIO-721",
            "2020-01-01T00:00:00Z",
            "2020-01-01T01:00:00Z",
        );
        let (_, events) = capture_events(|| o.reconcile_review_divergence());
        let warn = events
            .iter()
            .find(|e| {
                e.message.contains("review reconciliation")
                    && e.message.contains("held for capacity")
            })
            .expect("a capacity-held round must be reported");
        assert!(
            warn.message.contains("agent.max_concurrent_reviews"),
            "an operator with the key set must be told the review budget is the knob, got: {}",
            warn.message
        );
    }

    /// The other half of the same acceptance item, and the half the single-sweep test above cannot
    /// reach: the budget must subtract the reviews ALREADY RUNNING, not only decrement within the
    /// tick that dispatched them.
    ///
    /// `review_runs_never_exceed_their_own_budget` drives ONE sweep, so it pins the per-dispatch
    /// decrement of a budget that started full — with nothing running, `global_slots(max_reviews,
    /// holding)` and `global_slots(max_reviews, 0)` are the same number, and substituting the second
    /// for the first leaves that test (and the rest of the crate) green. This test drives TWO ticks
    /// with the pool already full on the second, where the two differ: a tick that starts with
    /// `max_concurrent_reviews` reviews in flight has NO budget at all.
    ///
    /// It is the subtraction the README's safety claim rests on — that total live agents exceed
    /// `max_concurrent_agents` by at most `max_concurrent_reviews`. Without it the review pool is
    /// only a per-tick rate limit and the daemon can run unboundedly many reviews.
    ///
    /// Mutation check: `global_slots(max_reviews, holding)` -> `global_slots(max_reviews, 0)` in
    /// `review_dispatch_budget` and this reds `left: 2, right: 0` on the second tick.
    #[test]
    fn a_full_review_pool_leaves_the_next_tick_no_budget() {
        let (mut o, dispatched) = orch(ticketless(&["bob"]));
        {
            let eff = o.eff.as_mut().expect("eff");
            // Deliberately generous, so nothing the implementation budget does can explain the
            // refusal below: the ONLY bound in play is the review pool.
            eff.max_concurrent = 10;
            eff.max_concurrent_reviews = Some(2);
        }
        for n in 31..35 {
            introduce(&o, row(n, "bob"));
        }

        // Tick one fills the review pool exactly.
        let first = o.handle_review_sweep(&[open_at(31, HEAD_A), open_at(32, HEAD_A)]);
        assert_eq!(first.dispatched, 2, "tick one must fill the pool");
        assert_eq!(first.deferred, 0);
        assert_eq!(
            o.running_ticketless_reviews(),
            2,
            "the two dispatched rounds must be holding the review pool"
        );

        // Tick two: a FRESH budget, counted against a pool that is already full.
        let second = o.handle_review_sweep(&[open_at(33, HEAD_A), open_at(34, HEAD_A)]);

        assert_eq!(
            second.dispatched, 0,
            "a tick that starts with the review pool full has no budget to dispatch from"
        );
        assert_eq!(second.deferred, 2);
        assert_eq!(
            dispatched.lock().expect("lock").len(),
            2,
            "only tick one's two rounds ever ran"
        );
        assert_eq!(
            o.running_ticketless_reviews(),
            2,
            "running review runs must never exceed agent.max_concurrent_reviews"
        );
    }

    /// STUDIO-950's second half: a round the watcher is HOLDING for capacity is not an unexplained
    /// stall — but it is still REPORTED. `reviewwatch` records the hold when it defers the round;
    /// the reconciliation sweep copies it onto the divergence and its WARN names the hold and its
    /// holder count instead of claiming nothing has reported the pull request blocked. That is the
    /// STUDIO-923 shape (annotate, never suppress): at a budget spent for longer than the sweep's
    /// own 90-minute threshold, this is the incident and a human should hear about it.
    ///
    /// The positive control (forget the hold, reconcile again) is deliberate: without it the test
    /// would pass if `capacity_held` came from anywhere, including an unconditional field.
    ///
    /// Mutation check: drop the annotation (`RowFacts.capacity_held` always `None`) or revert the
    /// WARN's capacity arm to the plain wording, and the assertions below red.
    #[test]
    fn a_review_held_for_capacity_names_the_hold_instead_of_nothing() {
        let (mut o, _d) = orch(ticketless(&["bob"]));
        o.eff.as_mut().expect("eff").max_concurrent = 4;
        for i in 0..4 {
            add_impl_run(&mut o, &format!("impl-{i}"), "alice");
        }
        introduce(&o, row(31, "bob"));
        // The row's origin ticket, so the `requested` rule has an author run to anchor on. Long
        // stale, so the rule would report with or without the hold.
        finished_run(
            &o,
            "STUDIO-721",
            "2020-01-01T00:00:00Z",
            "2020-01-01T01:00:00Z",
        );

        let report = o.handle_review_sweep(&[open_at(31, HEAD_A)]);
        assert_eq!(
            report.deferred, 1,
            "the fixture holds the round for capacity"
        );
        let id = review_key(OWNER, REPO, 31, "bob");
        assert_eq!(
            o.review_capacity_held
                .get(&id)
                .map(|h| (h.holders, h.separate)),
            Some((4, false)),
            "the hold must record the shared pool's four holders"
        );

        let (_, events) = capture_events(|| o.reconcile_review_divergence());
        assert_eq!(
            o.review_divergences().len(),
            1,
            "a held round is still reported, annotated"
        );
        assert_eq!(
            o.review_divergences()[0].capacity_held.map(|h| h.holders),
            Some(4),
            "the hold travels with the report"
        );
        let warn = events
            .iter()
            .find(|e| e.message.contains("review reconciliation"))
            .expect("the divergence must be logged");
        assert!(
            warn.message.contains("held for capacity"),
            "the report must name the hold, got: {}",
            warn.message
        );
        assert!(
            !warn.message.contains("nothing has reported it blocked"),
            "it must not claim nothing reported it, got: {}",
            warn.message
        );
        // ...and it names the knob the operator would actually turn. `max_concurrent_reviews` is
        // UNSET here, so naming it would point a default install at a key its WORKFLOW.md does not
        // contain; the shared draw's knob is `max_concurrent_agents`. Pinning both polarities is
        // what survives a constant fold of the `separate` mapping to either arm.
        assert!(
            warn.message.contains("agent.max_concurrent_agents"),
            "the shared hold must name the implementation knob, got: {}",
            warn.message
        );
        assert!(
            !warn.message.contains("agent.max_concurrent_reviews"),
            "an unset key must not be named as the knob, got: {}",
            warn.message
        );

        // Positive control: forget the hold and the SAME row reports under the plain wording, so
        // the naming above is the annotation and not an unconditional message.
        o.review_capacity_held.clear();
        o.reconcile_review_divergence();
        assert_eq!(
            o.review_divergences().len(),
            1,
            "the row still reports without a hold"
        );
        assert!(
            o.review_divergences()[0].capacity_held.is_none(),
            "no hold, no annotation"
        );
    }

    /// STUDIO-950: the capacity-hold map is cleared once per TICK, not once per hand-back.
    /// STUDIO-953 hands the watcher's observations to the control task ONE at a time (`slots` is
    /// `None` only on the tick's first hand-back), so an unguarded clear would wipe the hold recorded
    /// for a pull request seen earlier in the same tick — and the reconciliation sweep would then
    /// drop its annotation for that row.
    ///
    /// Mutation check: drop the `slots.is_none()` guard (clear on every hand-back) and the first
    /// round's hold is gone once the second hand-back lands.
    #[test]
    fn a_capacity_hold_survives_a_later_hand_back_in_the_same_tick() {
        // TRA-243: this test drives the same capacity-hold `tracing` callsites a capturing test
        // asserts on, so it serializes against them rather than poisoning their interest cache.
        let _serial = crate::testsupport::TRACING_TEST_LOCK.blocking_lock();
        let (mut o, _d) = orch(ticketless(&["bob"]));
        o.eff.as_mut().expect("eff").max_concurrent = 4;
        for i in 0..4 {
            add_impl_run(&mut o, &format!("impl-{i}"), "alice");
        }
        introduce(&o, row(31, "bob"));
        introduce(&o, row(32, "bob"));

        // First hand-back of the tick: the budget is spent, so round 31 is held.
        let (_, left) = o.handle_review_sweep_slots(&[open_at(31, HEAD_A)], None);
        assert_eq!(
            o.review_capacity_held
                .get(&review_key(OWNER, REPO, 31, "bob"))
                .map(|h| h.holders),
            Some(4),
            "the first hand-back records its hold"
        );

        // A later hand-back in the SAME tick must not clear it.
        let (_, left) = o.handle_review_sweep_slots(&[open_at(32, HEAD_A)], Some(left));
        assert_eq!(
            o.review_capacity_held
                .get(&review_key(OWNER, REPO, 31, "bob"))
                .map(|h| h.holders),
            Some(4),
            "a hold from an earlier hand-back must survive the rest of the tick"
        );
        assert_eq!(
            o.review_capacity_held
                .get(&review_key(OWNER, REPO, 32, "bob"))
                .map(|h| h.holders),
            Some(4),
            "the later hand-back records its own hold too"
        );

        // A FINAL observation that holds nothing — a retirement — must not erase the tick's holds
        // either: an unguarded clear leaves the map empty, which is the false page in its worst form
        // (no row annotated at all) once the tick's last observation is not a deferred round.
        let _ = o.handle_review_sweep_slots(&[observed(99, PrLookup::Gone)], Some(left));
        assert_eq!(
            o.review_capacity_held
                .get(&review_key(OWNER, REPO, 31, "bob"))
                .map(|h| h.holders),
            Some(4),
            "a final non-holding observation must not erase an earlier hold"
        );
        assert_eq!(
            o.review_capacity_held
                .get(&review_key(OWNER, REPO, 32, "bob"))
                .map(|h| h.holders),
            Some(4),
            "nor the one recorded by the previous hand-back"
        );
    }

    /// STUDIO-950 / the reconciliation sweep's decoupling: a hold recorded by a watcher sweep that
    /// has since STOPPED HAPPENING must not keep naming the capacity budget. The watcher delivers no
    /// sweep event at all when every `gh` lookup answers nothing — the outage case — so its hold map
    /// is never refreshed, while the reconciliation sweep (local, `gh`-free, and deliberately so)
    /// keeps running. It must stop trusting a record older than `CAPACITY_HOLD_TTL` and fall back to
    /// the plain wording rather than name a fact nothing is refreshing.
    ///
    /// Mutation check: drop the freshness test in `fresh_capacity_hold` and the stale hold still
    /// annotates, red.
    #[test]
    fn a_stale_capacity_hold_is_not_named_by_the_reconciliation_sweep() {
        let (mut o, _d) = orch(ticketless(&["bob"]));
        o.eff.as_mut().expect("eff").max_concurrent = 4;
        for i in 0..4 {
            add_impl_run(&mut o, &format!("impl-{i}"), "alice");
        }
        introduce(&o, row(31, "bob"));
        finished_run(
            &o,
            "STUDIO-721",
            "2020-01-01T00:00:00Z",
            "2020-01-01T01:00:00Z",
        );

        // One sweep records the hold; then the watcher goes quiet (a `gh` outage delivers no further
        // sweep), so nothing refreshes it.
        let report = o.handle_review_sweep(&[open_at(31, HEAD_A)]);
        assert_eq!(report.deferred, 1);

        let later = chrono::Utc::now()
            + chrono::Duration::seconds(
                i64::try_from(CAPACITY_HOLD_TTL.as_secs()).expect("ttl") + 1,
            );
        o.now = Box::new(move || later);

        o.reconcile_review_divergence();

        assert_eq!(
            o.review_divergences().len(),
            1,
            "the owed round still reports"
        );
        assert!(
            o.review_divergences()[0].capacity_held.is_none(),
            "a hold no live sweep is refreshing must not be named"
        );
    }

    /// STUDIO-950 (round 12): a hold that aged out during a watcher OUTAGE must not be RESURRECTED
    /// by the first sweep after the watcher returns.
    ///
    /// Freshness ages on the watcher's liveness ([`Orchestrator::review_watch_swept`]) and only
    /// FILTERS on read — it never removes. So when a `gh` outage stops the sweeps, the holds it left
    /// are already stale by `CAPACITY_HOLD_TTL`, but they are still IN `review_capacity_held`, and
    /// re-stamping liveness on the recovery sweep re-dates every one of them — including a round
    /// nothing has re-observed since before the outage. The row then names a capacity reading (the
    /// holders count and the key) taken before an outage during which those runs may all have
    /// finished, which is a WRONG named cause — the class this ticket exists to remove.
    ///
    /// The sibling stale test advances the clock and never sweeps again, so it cannot see this: only
    /// the watcher RETURNING and re-observing a DIFFERENT pull request exposes the resurrection.
    ///
    /// Mutation check: drop the continuity gap check in `handle_review_sweep_slots` and this reds —
    /// the pre-outage `Some(4)` comes back as the row's `capacity_held`.
    #[test]
    fn a_capacity_hold_is_not_resurrected_when_the_watcher_returns() {
        let (mut o, _d) = orch(ticketless(&["bob"]));
        o.eff.as_mut().expect("eff").max_concurrent = 4;
        for i in 0..4 {
            add_impl_run(&mut o, &format!("impl-{i}"), "alice");
        }
        introduce(&o, row(31, "bob"));
        finished_run(
            &o,
            "STUDIO-721",
            "2020-01-01T00:00:00Z",
            "2020-01-01T01:00:00Z",
        );

        let base = chrono::Utc::now();
        o.now = Box::new(move || base);

        // The last sweep before the outage records the hold.
        let report = o.handle_review_sweep(&[open_at(31, HEAD_A)]);
        assert_eq!(report.deferred, 1);
        let id31 = review_key(OWNER, REPO, 31, "bob");
        assert_eq!(
            o.review_capacity_held.get(&id31).map(|h| h.holders),
            Some(4),
            "precondition: the pre-outage sweep recorded the hold"
        );

        // The watcher goes quiet past `CAPACITY_HOLD_TTL` — its stamps stop advancing — then returns,
        // and its cursor reaches `#32`, NOT `#31`. Nothing re-observes the held round, so its record
        // is exactly the one the outage left behind.
        let later = base
            + chrono::Duration::seconds(
                i64::try_from(CAPACITY_HOLD_TTL.as_secs()).expect("ttl") + 1,
            );
        o.now = Box::new(move || later);
        o.handle_review_sweep_slots(&[open_at(32, HEAD_A)], None);

        o.reconcile_review_divergence();

        assert_eq!(
            o.review_divergences().len(),
            1,
            "the owed round still reports"
        );
        assert!(
            o.review_divergences()[0].capacity_held.is_none(),
            "a hold from before the outage must not be resurrected by the sweep that ends it"
        );
    }

    /// STUDIO-950 (round 14, sol's blocker; round 15 counter): a pull request whose `gh` lookup
    /// keeps FAILING must age out its capacity hold, even while an answering sibling keeps the
    /// watcher's global liveness fresh.
    ///
    /// The global stamp ([`Orchestrator::review_watch_swept`]) advances on ANY answering pull
    /// request, so a sibling that answers every tick kept a hold live for a pull request GitHub had
    /// stopped answering for — indefinitely, after its recorded holders had all exited. The
    /// per-coordinate failure count ([`Orchestrator::handle_review_unreadable`]) is what lets the
    /// sweep tell a healthy unreached round from an unreadable one. ONE failed attempt is the
    /// grace — a transient rate-limit must not blink a live annotation off — and a SECOND
    /// consecutive failure is when the hold stops being named. The count is ATTEMPTS, not a
    /// wall-clock TTL: the quantity is a rotation of the cursor, which no tick-sized constant can
    /// bound (STUDIO-950 round 15).
    ///
    /// Mutation check: drop the `review_watch_unreadable` test in `fresh_capacity_hold` and this reds
    /// — the hold stays named past the failure count on the answering sibling's liveness alone.
    #[test]
    fn an_unreadable_pull_request_ages_out_its_capacity_hold() {
        let (mut o, _d) = orch(ticketless(&["bob"]));
        o.eff.as_mut().expect("eff").max_concurrent = 4;
        for i in 0..4 {
            add_impl_run(&mut o, &format!("impl-{i}"), "alice");
        }
        introduce(&o, row(31, "bob"));
        finished_run(
            &o,
            "STUDIO-721",
            "2020-01-01T00:00:00Z",
            "2020-01-01T01:00:00Z",
        );

        // The last read that ANSWERED records the hold; from here `#31` stops answering.
        let report = o.handle_review_sweep(&[open_at(31, HEAD_A)]);
        assert_eq!(report.deferred, 1);
        let id31 = review_key(OWNER, REPO, 31, "bob");

        // ONE failed attempt is the grace: a transient rate-limit must not blink the annotation.
        // `#32` answers this tick, keeping the watcher's global liveness fresh — the mechanism that
        // used to keep the stale hold alive.
        o.handle_review_unreadable(&[coord(31)]);
        o.handle_review_sweep_slots(&[open_at(32, HEAD_A)], None);
        o.reconcile_review_divergence();
        assert_eq!(
            o.review_divergences()[0].capacity_held.map(|h| h.holders),
            Some(4),
            "one failed attempt is within the grace, so the hold is still named"
        );

        // A SECOND consecutive failed attempt is "GitHub is not answering for this pull request":
        // it stops being named, and stays gone while the failures continue and `#32` keeps answering.
        o.handle_review_unreadable(&[coord(31)]);
        o.handle_review_sweep_slots(&[open_at(32, HEAD_A)], None);
        o.reconcile_review_divergence();
        assert_eq!(
            o.review_divergences().len(),
            1,
            "the owed round still reports"
        );
        assert!(
            o.review_divergences()[0].capacity_held.is_none(),
            "a pull request GitHub has not answered for is not a live capacity hold"
        );
        assert!(
            o.review_capacity_held.contains_key(&id31),
            "the record itself is kept; only its freshness is denied"
        );

        // Recovery: `#31` answers again and is still deferred, so the hold is named again — a
        // success clears the failure count, not merely the freshness check.
        o.handle_review_sweep_slots(&[open_at(31, HEAD_A)], None);
        assert!(
            !o.review_watch_unreadable.contains_key(&coord(31)),
            "an answering pull request clears its failure record"
        );
        o.reconcile_review_divergence();
        assert_eq!(
            o.review_divergences()[0].capacity_held.map(|h| h.holders),
            Some(4),
            "an answering pull request's hold is named again"
        );
    }

    /// STUDIO-950 (round 14, alice's non-blocking 2): the backwards-clock branch is a deliberate
    /// behavioural choice with its own comment — a clock that went backwards is not continuity — and
    /// this pins it. The gap is computed from a negative duration, whose `to_std()` fails and takes
    /// the `unwrap_or(CAPACITY_HOLD_TTL)` arm, so the holds are dropped.
    ///
    /// Mutation check: change `.unwrap_or(CAPACITY_HOLD_TTL)` to `.unwrap_or(Duration::ZERO)` in
    /// `handle_review_sweep_slots` and this reds — the negative gap no longer reads as a gap and the
    /// holds are retained.
    #[test]
    fn a_backwards_clock_drops_a_capacity_hold() {
        let (mut o, _d) = orch(ticketless(&["bob"]));
        o.eff.as_mut().expect("eff").max_concurrent = 4;
        for i in 0..4 {
            add_impl_run(&mut o, &format!("impl-{i}"), "alice");
        }
        introduce(&o, row(31, "bob"));

        let base = chrono::Utc::now();
        o.now = Box::new(move || base);
        let report = o.handle_review_sweep(&[open_at(31, HEAD_A)]);
        assert_eq!(report.deferred, 1);
        assert!(
            !o.review_capacity_held.is_empty(),
            "precondition: the sweep recorded a hold"
        );

        let earlier = base - chrono::Duration::hours(1);
        o.now = Box::new(move || earlier);
        o.handle_review_sweep_slots(&[], None);

        assert!(
            o.review_capacity_held.is_empty(),
            "a clock that went backwards is not continuity; the holds must be dropped"
        );
    }

    /// STUDIO-950: [`CAPACITY_HOLD_TTL`]'s LOWER bound — the half the stale test above cannot see,
    /// because it only ever advances past the constant. A hold is recorded partway through a tick
    /// and the reconciliation sweep may read it only after the watcher has finished the rest of that
    /// tick, so the TTL has to outlast a healthy WORST-CASE tick — the sleep before a sweep plus BOTH
    /// serial `gh` phases it then makes: the batched sweep and STUDIO-953's per-observation re-read,
    /// each bounded by `MAX_PR_STATE_CALLS_PER_TICK` calls — or the sweep would call a live hold
    /// stale while the watcher is still working through the very tick that recorded it.
    ///
    /// Mutation check: shrink the TTL to count only ONE of the two phases (the pre-STUDIO-953
    /// bound sol flagged) and the age advanced here overtakes it, red. The advance is derived from
    /// the documented worst case, never from `CAPACITY_HOLD_TTL`, so it cannot follow the constant it
    /// is pinning.
    #[test]
    fn a_capacity_hold_survives_a_full_healthy_tick() {
        // TRA-243: see the sibling test above — same callsites, serialized against the capturers.
        let _serial = crate::testsupport::TRACING_TEST_LOCK.blocking_lock();
        let (mut o, _d) = orch(ticketless(&["bob"]));
        o.eff.as_mut().expect("eff").max_concurrent = 4;
        for i in 0..4 {
            add_impl_run(&mut o, &format!("impl-{i}"), "alice");
        }
        introduce(&o, row(31, "bob"));
        finished_run(
            &o,
            "STUDIO-721",
            "2020-01-01T00:00:00Z",
            "2020-01-01T01:00:00Z",
        );

        // Pin the clock before the sweep so `recorded` is exactly `base`. Pinning it only for the
        // advance below would leave `recorded` on the real clock, and the age at reconcile would be
        // `worst_case - 1` PLUS whatever wall-clock elapsed between the sweep and the advance — so
        // one real second of scheduling stall would red this test with the same
        // `left: None / right: Some(4)` the TTL mutation produces, sending a maintainer after the
        // constant. The pin costs the assertion nothing: the advance is still derived from the
        // documented worst case.
        let base = chrono::Utc::now();
        o.now = Box::new(move || base);

        let report = o.handle_review_sweep(&[open_at(31, HEAD_A)]);
        assert_eq!(report.deferred, 1);

        // One second short of the documented worst case: the interval slept before a sweep, then
        // both serial lookup phases, each a full budget of lookups bounded by `GH_EXEC_TIMEOUT`.
        let worst_case = crate::prstate::PR_STATE_POLL_INTERVAL.as_secs()
            + 2 * crate::prstate::MAX_PR_STATE_CALLS_PER_TICK as u64
                * crate::ghsummons::GH_EXEC_TIMEOUT.as_secs();
        let later =
            base + chrono::Duration::seconds(i64::try_from(worst_case).expect("worst case") - 1);
        o.now = Box::new(move || later);

        o.reconcile_review_divergence();

        assert_eq!(
            o.review_divergences().len(),
            1,
            "the owed round still reports"
        );
        assert_eq!(
            o.review_divergences()[0].capacity_held.map(|h| h.holders),
            Some(4),
            "a hold from the tick currently in flight must still be named"
        );
    }

    /// STUDIO-950 (round 10): a continuously-held round must not BLINK.
    ///
    /// The watcher's cursor rotates `MAX_PR_STATE_CALLS_PER_TICK` pull requests per tick, so on a
    /// larger watch set a held round is not re-evaluated every tick. Clearing the hold map wholesale
    /// dropped its entry on the ticks that did not reach it and restored it on the ones that did,
    /// with no hold having ended — and the reconciliation sweep reads an absent entry as a report
    /// transition, so every blink re-logged its capacity line and the alternating sweeps re-logged
    /// the false "nothing has reported it blocked" page, defeating the log's rate limit and
    /// re-emitting the very page this ticket closes.
    ///
    /// Here ONLY `#31` is a watched divergence; `#32` exists only as a pull request the cursor can
    /// reach INSTEAD of `#31` (it has no watch row, so it is never reported). The round is held
    /// throughout; the ticks alternate between reaching `#31` and reaching `#32`. The hold must
    /// survive every tick that did not reach it, and the sweep must log exactly ONE line — the
    /// capacity one.
    ///
    /// Mutation check: restore the wholesale `review_capacity_held.clear()` and this reds — the
    /// sweep logs again, with the plain wording, on every tick that missed `#31`, and the hold
    /// survival assertion reds on the first missed tick.
    #[test]
    fn a_continuously_held_round_does_not_blink_across_unreached_ticks() {
        let (mut o, _d) = orch(ticketless(&["bob"]));
        o.eff.as_mut().expect("eff").max_concurrent = 4;
        for i in 0..4 {
            add_impl_run(&mut o, &format!("impl-{i}"), "alice");
        }
        introduce(&o, row(31, "bob"));
        // The row's origin ticket, so the `requested` rule has an author run to anchor on.
        finished_run(
            &o,
            "STUDIO-721",
            "2020-01-01T00:00:00Z",
            "2020-01-01T01:00:00Z",
        );

        let id31 = review_key(OWNER, REPO, 31, "bob");

        // TRA-243: register both callsites against a capturing subscriber before the real run. The
        // reset afterwards starts the real run from `#31` newly reported with no hold — the crossing.
        let _ = capture_events(|| {
            o.reconcile_review_divergence(); // the plain callsite, no hold yet
            o.handle_review_sweep_slots(&[open_at(31, HEAD_A)], None);
            o.reconcile_review_divergence(); // the capacity callsite
        });
        o.review_divergent.clear();
        o.review_divergence.clear();
        o.review_capacity_held.clear();

        let (holds, events) = capture_events(|| {
            let mut holds = Vec::new();
            for i in 0..6 {
                // The cursor alternates: half of these ticks never look at `#31`.
                let obs = if i % 2 == 0 {
                    open_at(31, HEAD_A)
                } else {
                    open_at(32, HEAD_A)
                };
                o.handle_review_sweep_slots(&[obs], None);
                holds.push(o.review_capacity_held.get(&id31).map(|h| h.holders));
                o.reconcile_review_divergence();
            }
            holds
        });

        assert_eq!(
            holds,
            vec![Some(4); 6],
            "an unreached tick must not drop a held round's annotation"
        );
        let warns: Vec<&crate::testsupport::CapturedEvent> = events
            .iter()
            .filter(|e| e.message.contains("review reconciliation"))
            .collect();
        assert_eq!(
            warns.len(),
            1,
            "a continuously-held round reports once, not once per blink: {warns:?}"
        );
        assert!(
            warns[0].message.contains("held for capacity"),
            "and it names the hold rather than the false page, got: {}",
            warns[0].message
        );
    }

    /// STUDIO-950 (round 11): freshness is the WATCHER's liveness, not the age of an individual
    /// hold.
    ///
    /// The sibling test above freezes the clock and so pins only "does not blink within one
    /// `CAPACITY_HOLD_TTL`". The cursor rotates `MAX_PR_STATE_CALLS_PER_TICK` pull requests a tick,
    /// so a round the cursor reaches once is re-evaluated once per ROTATION — under the earlier
    /// per-hold freshness the hold aged out after one TTL while the watcher was healthy and
    /// sweeping every tick, blinking its annotation off and re-emitting the false "nothing has
    /// reported it blocked" page this ticket exists to stop. Here the clock advances one nominal
    /// `PR_STATE_POLL_INTERVAL` per tick for 22 ticks (past the 21 it takes to exceed
    /// `CAPACITY_HOLD_TTL`), and only tick 0 reaches `#31`.
    ///
    /// Mutation check: age `hold.recorded` instead of `review_watch_swept` in `fresh_capacity_hold`
    /// and the hold drops at tick 21 — a second WARN, the plain-wording one, reds both assertions.
    #[test]
    fn a_continuously_held_round_survives_a_full_rotation_on_a_large_watch_set() {
        let (mut o, _d) = orch(ticketless(&["bob"]));
        o.eff.as_mut().expect("eff").max_concurrent = 4;
        for i in 0..4 {
            add_impl_run(&mut o, &format!("impl-{i}"), "alice");
        }
        introduce(&o, row(31, "bob"));
        // The row's origin ticket, so the `requested` rule has an author run to anchor on.
        finished_run(
            &o,
            "STUDIO-721",
            "2020-01-01T00:00:00Z",
            "2020-01-01T01:00:00Z",
        );

        let id31 = review_key(OWNER, REPO, 31, "bob");
        let base = chrono::Utc::now();

        // TRA-243: register both callsites against a capturing subscriber before the real run, then
        // reset so the real run starts from `#31` newly reported with a hold.
        let _ = capture_events(|| {
            o.now = Box::new(move || base);
            o.reconcile_review_divergence(); // the plain callsite, no hold yet
            o.handle_review_sweep_slots(&[open_at(31, HEAD_A)], None);
            o.reconcile_review_divergence(); // the capacity callsite
        });
        o.review_divergent.clear();
        o.review_divergence.clear();
        o.review_capacity_held.clear();

        let interval = chrono::Duration::from_std(crate::prstate::PR_STATE_POLL_INTERVAL)
            .expect("the poll interval fits a chrono duration");
        let (holds, events) = capture_events(|| {
            let mut holds = Vec::new();
            for i in 0..22 {
                let at = base + interval * i;
                o.now = Box::new(move || at);
                // Only the first tick's cursor reaches `#31`; the rest reach `#32`, which has no
                // watch row and so re-evaluates nothing.
                let obs = if i == 0 {
                    open_at(31, HEAD_A)
                } else {
                    open_at(32, HEAD_A)
                };
                o.handle_review_sweep_slots(&[obs], None);
                holds.push(o.review_capacity_held.get(&id31).map(|h| h.holders));
                o.reconcile_review_divergence();
            }
            holds
        });

        assert_eq!(
            holds,
            vec![Some(4); 22],
            "a hold a healthy watcher keeps carrying must survive a full rotation"
        );
        let warns: Vec<&crate::testsupport::CapturedEvent> = events
            .iter()
            .filter(|e| e.message.contains("review reconciliation"))
            .collect();
        assert_eq!(
            warns.len(),
            1,
            "a continuously-held round reports once, not once per rotation: {warns:?}"
        );
        assert!(
            warns[0].message.contains("held for capacity"),
            "and it names the hold rather than the false page, got: {}",
            warns[0].message
        );
    }

    /// STUDIO-950 (round 15, alice's blocking finding): ONE transient `gh` failure must not blink a
    /// continuously-held round across a full ROTATION of a large watch set.
    ///
    /// This is the round-10 defect re-opened through the unreadability record. The round-14 grace
    /// denied a hold whose coordinate had been in `review_watch_unreadable` for `CAPACITY_HOLD_TTL`
    /// — but that constant is TICK-sized (`I + 2*N*T`), while the quantity being bounded is how long
    /// until the rotating cursor next REACHES this pull request, which is
    /// `ceil(watch_set / MAX_PR_STATE_CALLS_PER_TICK)` ticks. On any watch set larger than the
    /// tick's call budget a rotation is at least two ticks, so a single failed lookup aged the hold
    /// out mid-rotation and the sweep re-emitted the false "nothing has reported it blocked" page.
    /// The fix counts ATTEMPTS instead, which cannot move on a tick that never asked the coordinate.
    ///
    /// Here `#31` is held the whole time; `#32` is a pull request the cursor reaches instead (no
    /// watch row, so never reported). Tick 1 reports `#31` unreadable ONCE, then the watcher is
    /// healthy and answers `#32` every tick until `#31` is reached again on the final tick. The
    /// hold must stay named, and the sweep must log exactly ONE line — the capacity one.
    ///
    /// Mutation check: reinstate the wall-clock test in `fresh_capacity_hold` (deny once the
    /// failure is older than `CAPACITY_HOLD_TTL`) and this reds — under the one-interval-per-tick
    /// loop the hold's annotation blinks off mid-rotation and the plain-wording page re-emits.
    #[test]
    fn a_transient_failure_does_not_blink_a_round_across_a_rotation() {
        let (mut o, _d) = orch(ticketless(&["bob"]));
        o.eff.as_mut().expect("eff").max_concurrent = 4;
        for i in 0..4 {
            add_impl_run(&mut o, &format!("impl-{i}"), "alice");
        }
        introduce(&o, row(31, "bob"));
        finished_run(
            &o,
            "STUDIO-721",
            "2020-01-01T00:00:00Z",
            "2020-01-01T01:00:00Z",
        );

        let id31 = review_key(OWNER, REPO, 31, "bob");
        let base = chrono::Utc::now();

        // Register both callsites against a capturing subscriber before the real run, then reset so
        // the real run starts from `#31` newly reported with a hold.
        let _ = capture_events(|| {
            o.now = Box::new(move || base);
            o.reconcile_review_divergence(); // the plain callsite, no hold yet
            o.handle_review_sweep_slots(&[open_at(31, HEAD_A)], None);
            o.reconcile_review_divergence(); // the capacity callsite
        });
        o.review_divergent.clear();
        o.review_divergence.clear();
        o.review_capacity_held.clear();

        let interval = chrono::Duration::from_std(crate::prstate::PR_STATE_POLL_INTERVAL)
            .expect("the poll interval fits a chrono duration");
        // One interval per tick. The binding bound is not `MAX_PR_STATE_CALLS_PER_TICK` (a per-tick
        // CALL budget, not a tick count): the round-14 wall-clock rule this test guards would deny a
        // hold first at `ceil(CAPACITY_HOLD_TTL / PR_STATE_POLL_INTERVAL) + 1` = 22 ticks, and the
        // false page needs one more tick that is neither first nor last before `#31` answers again
        // and clears the record. 24 is the smallest `ticks` the mutation check still detects; 30
        // leaves slack.
        let ticks = 30i32;
        let (holds, events) = capture_events(|| {
            let mut holds = Vec::new();
            for i in 0..ticks {
                let at = base + interval * i;
                o.now = Box::new(move || at);
                // A single transient failure for `#31` on tick 1 — the rate-limited lookup.
                if i == 1 {
                    o.handle_review_unreadable(&[coord(31)]);
                }
                // `#31` is reached on the first and last ticks; every tick between reaches `#32`,
                // which has no watch row and re-evaluates nothing.
                let obs = if i == 0 || i == ticks - 1 {
                    open_at(31, HEAD_A)
                } else {
                    open_at(32, HEAD_A)
                };
                o.handle_review_sweep_slots(&[obs], None);
                holds.push(o.review_capacity_held.get(&id31).map(|h| h.holders));
                o.reconcile_review_divergence();
            }
            holds
        });

        assert_eq!(
            holds,
            vec![Some(4); ticks as usize],
            "one transient failure must not blink a held round across a full rotation"
        );
        let warns: Vec<&crate::testsupport::CapturedEvent> = events
            .iter()
            .filter(|e| e.message.contains("review reconciliation"))
            .collect();
        assert_eq!(
            warns.len(),
            1,
            "a continuously-held round reports once, not once per blink: {warns:?}"
        );
        assert!(
            warns[0].message.contains("held for capacity")
                && !warns[0].message.contains("nothing has reported it blocked"),
            "and it names the hold rather than the false page, got: {}",
            warns[0].message
        );
    }

    /// STUDIO-950 (round 11, non-blocking B): retirement drops the retired round's capacity hold.
    /// Under the per-pull-request refresh a leaked hold no longer lives one tick — it lives until the
    /// watcher stops sweeping — so a closed-and-reopened pull request could inherit a stale "held
    /// for capacity" annotation before any sweep re-evaluates it, a WRONG named cause (the class this
    /// ticket exists to remove). Pin the removal on the retirement path.
    ///
    /// Mutation check: drop the `review_capacity_held.remove(&id)` in `retire_review_pr` and this
    /// reds.
    #[test]
    fn a_retirement_drops_a_capacity_hold() {
        let (mut o, _d) = orch(ticketless(&["bob"]));
        o.eff.as_mut().expect("eff").max_concurrent = 4;
        for i in 0..4 {
            add_impl_run(&mut o, &format!("impl-{i}"), "alice");
        }
        introduce(&o, row(31, "bob"));

        let report = o.handle_review_sweep(&[open_at(31, HEAD_A)]);
        assert_eq!(report.deferred, 1, "the fixture holds the round");
        let id = review_key(OWNER, REPO, 31, "bob");
        assert!(
            o.review_capacity_held.contains_key(&id),
            "precondition: the round is held for capacity"
        );

        assert_eq!(
            o.handle_review_sweep(&[observed(31, PrLookup::Gone)])
                .retired,
            1
        );
        assert!(
            !o.review_capacity_held.contains_key(&id),
            "a retired pull request must not keep a capacity hold for its round"
        );
    }

    /// STUDIO-950 (round 15, alice's non-blocking 1): retirement forgets the retired pull request's
    /// unreadability record. Keyed by COORDINATE, it would otherwise outlive the pull request it
    /// names and sit in the map for the daemon's whole life; a re-introduced coordinate could also
    /// inherit a failure count it never earned and have its first fresh hold denied.
    ///
    /// Driven through the PRIVATE `retire_review_pr` rather than the sweep on purpose. In the sweep
    /// the success-clear loop above has already removed the coordinate (a `Gone` lookup still
    /// ANSWERED), so a sweep-driven test cannot see whether the function forgets it itself — and
    /// `retire_review_pr`'s job is to forget EVERYTHING about the coordinate, so the fact one caller
    /// happens to pre-clear must not make it silently incomplete if that loop is ever reordered.
    ///
    /// Mutation check: drop the `review_watch_unreadable.remove(pr)` in `retire_review_pr` and this
    /// reds.
    #[test]
    fn a_retirement_forgets_the_unreadable_record() {
        let (mut o, _d) = orch(ticketless(&["bob"]));
        introduce(&o, row(31, "bob"));
        o.handle_review_unreadable(&[coord(31)]);
        assert!(
            o.review_watch_unreadable.contains_key(&coord(31)),
            "precondition: a failed lookup is recorded"
        );

        assert_eq!(o.retire_review_pr(&coord(31), "gone"), 1);
        assert!(
            !o.review_watch_unreadable.contains_key(&coord(31)),
            "a retired pull request must not keep an unreadability record"
        );
    }

    // --- the off-loop task --------------------------------------------------------------------

    /// A sink recording what the task asked for and handed back.
    #[derive(Default)]
    struct FakeSink {
        watched: Vec<WatchedPr>,
        seen: Arc<Mutex<Vec<Vec<PrObservation>>>>,
        /// `seen.len()` at the start of each tick, recorded by [`ReviewWatchSink::watched`] — which
        /// the task calls exactly once per tick. A test can then slice `seen` into whole ticks
        /// rather than approximating a tick boundary by a count.
        boundaries: Arc<Mutex<Vec<usize>>>,
        done: Arc<tokio::sync::Notify>,
        /// What the control task pretends to have decided, handed back from every `sweep`.
        hand_back: ReviewSweepReport,
        /// The auto-Done moves the task asked for, in order (STUDIO-712).
        finished: Arc<Mutex<Vec<crate::reviewdone::ReviewDonePlan>>>,
        /// The auto-merges the task asked for, in order (STUDIO-874).
        merged: Arc<Mutex<Vec<crate::automerge::AutoMergePlan>>>,
        /// The coordinates the task reported as unreadable, across every tick (STUDIO-950 round 14).
        unreadable: Arc<Mutex<Vec<PrCoord>>>,
    }

    #[async_trait]
    impl ReviewWatchSink for FakeSink {
        async fn watched(&self) -> Vec<WatchedPr> {
            let start = self.seen.lock().expect("seen lock").len();
            self.boundaries.lock().expect("boundaries lock").push(start);
            self.watched.clone()
        }
        async fn sweep(
            &self,
            observed: Vec<PrObservation>,
            _slots: Option<i64>,
        ) -> (ReviewSweepReport, i64) {
            self.seen.lock().expect("seen lock").push(observed);
            self.done.notify_one();
            (self.hand_back.clone(), 0)
        }
        async fn unreadable(&self, failed: Vec<PrCoord>) {
            self.unreadable
                .lock()
                .expect("unreadable lock")
                .extend(failed);
        }
        async fn merge(&self, plan: crate::automerge::AutoMergePlan) {
            self.merged.lock().expect("merged lock").push(plan);
        }
        async fn finish(&self, plan: crate::reviewdone::ReviewDonePlan) {
            self.finished.lock().expect("finished lock").push(plan);
        }
        async fn nudge(&self, _plan: crate::draftpoke::DraftNudge) {}
        async fn adjudicate(&self, _plan: crate::reviewadjudicate::ReviewAdjudicationPlan) {}
    }

    /// A [`ReviewDiffSource`] whose two patches are fixed, so a test can drive the watcher's
    /// "did this head move carry no work" comparison without GitHub (STUDIO-960). `head_patch` is
    /// answered for [`HEAD_B`] and `old_patch` for anything else, matching how
    /// [`unchanged_reviewed_shas`] calls it (once for the head, once per reviewed SHA).
    struct FakeDiffSource {
        base: Result<String, String>,
        head_patch: Result<String, String>,
        old_patch: Result<String, String>,
    }

    impl FakeDiffSource {
        fn same() -> FakeDiffSource {
            FakeDiffSource {
                base: Ok("main".to_string()),
                head_patch: Ok("diff".to_string()),
                old_patch: Ok("diff".to_string()),
            }
        }
        fn changed() -> FakeDiffSource {
            FakeDiffSource {
                base: Ok("main".to_string()),
                head_patch: Ok("head diff".to_string()),
                old_patch: Ok("conflict-resolved diff".to_string()),
            }
        }
        fn base_fails() -> FakeDiffSource {
            FakeDiffSource {
                base: Err("gh: boom".to_string()),
                head_patch: Ok("diff".to_string()),
                old_patch: Ok("diff".to_string()),
            }
        }
        fn old_fails() -> FakeDiffSource {
            FakeDiffSource {
                base: Ok("main".to_string()),
                head_patch: Ok("diff".to_string()),
                old_patch: Err("gh: boom".to_string()),
            }
        }
    }

    fn diff_err(e: String) -> Box<dyn std::error::Error + Send + Sync> {
        e.into()
    }

    #[async_trait]
    impl ReviewDiffSource for FakeDiffSource {
        async fn pr_base_ref(&self, _owner: &str, _repo: &str, _number: i64) -> ReviewDiffResult {
            self.base.clone().map_err(diff_err)
        }
        async fn merge_base_patch(
            &self,
            _owner: &str,
            _repo: &str,
            _base: &str,
            sha: &str,
        ) -> ReviewDiffResult {
            let which = if sha == HEAD_B {
                &self.head_patch
            } else {
                &self.old_patch
            };
            which.clone().map_err(diff_err)
        }
    }

    /// A [`ReviewDiffSource`] that counts every call, so a test can prove the watcher spends NO
    /// `gh` read when nothing could have moved (STUDIO-960).
    struct CountingDiffSource(Arc<Mutex<usize>>);

    #[async_trait]
    impl ReviewDiffSource for CountingDiffSource {
        async fn pr_base_ref(&self, _owner: &str, _repo: &str, _number: i64) -> ReviewDiffResult {
            *self.0.lock().expect("count") += 1;
            Ok("main".to_string())
        }
        async fn merge_base_patch(
            &self,
            _owner: &str,
            _repo: &str,
            _base: &str,
            _sha: &str,
        ) -> ReviewDiffResult {
            *self.0.lock().expect("count") += 1;
            Ok("diff".to_string())
        }
    }

    /// A [`PrStateSource`] that always reports one fixed, OPEN head — the shape a rebase leaves
    /// behind for the watcher to compare (STUDIO-960).
    struct FixedHeadSource(&'static str);

    #[async_trait]
    impl PrStateSource for FixedHeadSource {
        async fn pr_state(
            &self,
            _owner: &str,
            _repo: &str,
            _number: i64,
            _allow: &HeadAllowlist,
        ) -> PrStateResult {
            Ok(PrLookup::Found(PrSnapshot {
                is_draft: Some(false),
                head_sha: self.0.to_string(),
                status: PrStatus::Open,
                merged_at: None,
                head_repo: format!("{OWNER}/{REPO}"),
                // Not what this source is for: an empty read is UNSETTLED, so it decides nothing
                // about a conflict and clears nothing (STUDIO-961).
                merge_state: String::new(),
            }))
        }
    }

    struct FakeSource;

    #[async_trait]
    impl PrStateSource for FakeSource {
        async fn pr_state(
            &self,
            _owner: &str,
            _repo: &str,
            number: i64,
            _allow: &HeadAllowlist,
        ) -> PrStateResult {
            Ok(PrLookup::Found(PrSnapshot {
                is_draft: Some(false),
                head_sha: format!("{number:040}"),
                status: PrStatus::Open,
                merged_at: None,
                head_repo: format!("{OWNER}/{REPO}"),
                merge_state: String::new(),
            }))
        }
    }

    /// A [`PrStateSource`] that never answers — the per-pull-request lookup failure STUDIO-950
    /// round 14 exists for.
    struct FailingSource;

    #[async_trait]
    impl PrStateSource for FailingSource {
        async fn pr_state(
            &self,
            _owner: &str,
            _repo: &str,
            _number: i64,
            _allow: &HeadAllowlist,
        ) -> PrStateResult {
            Err("gh: API rate limit exceeded".into())
        }
    }

    /// STUDIO-950 round 14: one tick's FAILED lookups reach the control task, so a capacity hold for
    /// a pull request GitHub would not answer for stops being trusted. A failure yields no
    /// observation, so this is the only channel that can carry it — and it must fire even on a tick
    /// where EVERY lookup failed and no observation is handed back.
    ///
    /// Mutation check: drop the `deps.sink.unreadable(..)` call in `run_review_watch_task` and this
    /// reds with an empty recorded list.
    #[tokio::test(start_paused = true)]
    async fn the_task_reports_the_coordinates_github_would_not_answer_for() {
        let unreadable = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let done = Arc::new(tokio::sync::Notify::new());
        let signal = CancelSignal::new();
        let deps = ReviewWatchDeps {
            pr_source: Some(Arc::new(FailingSource)),
            allow: HeadAllowlist::none(),
            poll_interval_ms: test_poll_interval(),
            teams: ticketless(&["alice", "bob"]),
            diff_source: None,
            sink: Arc::new(FakeSink {
                watched: vec![WatchedPr::new(coord(12)), WatchedPr::new(coord(13))],
                seen: Arc::clone(&seen),
                done: Arc::clone(&done),
                unreadable: Arc::clone(&unreadable),
                ..FakeSink::default()
            }),
        };
        let task = tokio::spawn(run_review_watch_task(signal.wait(), deps));

        tokio::time::sleep(crate::prstate::PR_STATE_POLL_INTERVAL * 2).await;
        signal.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), task).await;

        let reported = unreadable.lock().expect("unreadable lock").clone();
        assert!(
            reported.contains(&coord(12)) && reported.contains(&coord(13)),
            "every coordinate whose lookup failed must be reported, got {reported:?}"
        );
        assert!(
            seen.lock().expect("seen lock").is_empty(),
            "a failed lookup is not an observation"
        );
    }

    /// A [`PrStateSource`] that records which entry point the watcher used, so a test can prove the
    /// pre-dispatch re-read bypasses any conditional cache (STUDIO-974).
    struct MethodRecordingSource {
        saw_unconditional: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl PrStateSource for MethodRecordingSource {
        async fn pr_state(
            &self,
            _owner: &str,
            _repo: &str,
            _number: i64,
            _allow: &HeadAllowlist,
        ) -> PrStateResult {
            Ok(PrLookup::Found(PrSnapshot {
                is_draft: Some(false),
                head_sha: HEAD_B.to_string(),
                status: PrStatus::Open,
                merged_at: None,
                head_repo: format!("{OWNER}/{REPO}"),
                merge_state: String::new(),
            }))
        }

        async fn pr_state_unconditional(
            &self,
            owner: &str,
            repo: &str,
            number: i64,
            allow: &HeadAllowlist,
        ) -> PrStateResult {
            self.saw_unconditional.fetch_add(1, Ordering::SeqCst);
            self.pr_state(owner, repo, number, allow).await
        }
    }

    /// STUDIO-974 + STUDIO-953: the per-observation pre-dispatch re-read must go through
    /// `pr_state_unconditional`, because a conditional source would otherwise answer a cached
    /// "unchanged" and hide a head an author pushed after the sweep. Mutation check: point
    /// `refresh_observed_head` back at `src.pr_state(..)` and this reds.
    #[tokio::test]
    async fn the_pre_dispatch_re_read_bypasses_a_conditional_cache() {
        let saw = Arc::new(AtomicUsize::new(0));
        let src = MethodRecordingSource {
            saw_unconditional: Arc::clone(&saw),
        };
        let obs = PrObservation {
            pr: coord(12),
            lookup: PrLookup::Found(PrSnapshot {
                is_draft: Some(false),
                head_sha: HEAD_A.to_string(),
                status: PrStatus::Open,
                merged_at: None,
                head_repo: format!("{OWNER}/{REPO}"),
                merge_state: String::new(),
            }),
            unchanged_from: Vec::new(),
        };

        let out = refresh_observed_head(
            &crate::control_loop::CancelWait::default(),
            &ticketless(&["alice"]),
            &src,
            &HeadAllowlist::none(),
            obs,
        )
        .await;

        assert_eq!(
            saw.load(Ordering::SeqCst),
            1,
            "the re-read must use the unconditional entry point"
        );
        match out.lookup {
            PrLookup::Found(snap) => assert_eq!(snap.head_sha, HEAD_B),
            other => panic!("expected the fresh head, got {other:?}"),
        }
    }

    /// The task's whole shape: it asks the control task what to poll, asks GitHub about exactly
    /// that, and hands the answers back — never touching the store or the orchestrator itself.
    #[tokio::test(start_paused = true)]
    async fn the_task_polls_the_watch_set_and_hands_the_answers_back() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let boundaries = Arc::new(Mutex::new(Vec::new()));
        let done = Arc::new(tokio::sync::Notify::new());
        let signal = CancelSignal::new();
        let deps = ReviewWatchDeps {
            pr_source: Some(Arc::new(FakeSource)),
            allow: HeadAllowlist::none(),
            poll_interval_ms: test_poll_interval(),
            teams: ticketless(&["alice", "bob"]),
            diff_source: None,
            sink: Arc::new(FakeSink {
                watched: vec![WatchedPr::new(coord(12)), WatchedPr::new(coord(13))],
                seen: Arc::clone(&seen),
                boundaries: Arc::clone(&boundaries),
                done: Arc::clone(&done),
                ..FakeSink::default()
            }),
        };
        let task = tokio::spawn(run_review_watch_task(signal.wait(), deps));

        // Since STUDIO-953 the task hands each observation over on its own, immediately after its
        // head is re-read, so the first tick is TWO hand-backs. Waking here does not preempt the
        // tick, so the task's whole tick (both hand-backs) runs before it sleeps for the interval;
        // the sleep below parks this task so the paused clock can advance at all.
        done.notified().await;
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        signal.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), task).await;

        let seen = seen.lock().expect("seen lock").clone();
        let boundaries = boundaries.lock().expect("boundaries lock").clone();
        assert!(!seen.is_empty(), "the task never handed a tick back");
        // Slice to the tick's END, not a fixed count: `take(2)` would hide a third hand-back in
        // the same tick, which is exactly what this test claims cannot happen.
        let end = boundaries.get(1).copied().unwrap_or(seen.len());
        let handed: Vec<PrCoord> = seen[..end].iter().flatten().map(|o| o.pr.clone()).collect();
        assert_eq!(
            handed,
            vec![coord(12), coord(13)],
            "exactly the coordinates the control task named, and no others"
        );
    }

    // --- STUDIO-960: the off-loop diff comparison ---------------------------------------------

    /// The watcher compares the two diffs and hands the PROOF to the control task. The whole
    /// feature is off without this wiring, so it is asserted end to end: the observation the
    /// control task receives carries the reviewed head that was proven identical.
    #[tokio::test(start_paused = true)]
    async fn the_watcher_proves_an_unchanged_head_move_before_handing_it_over() {
        let handed =
            run_one_watch_tick(watched_pr(&[HEAD_A], &[]), Arc::new(FakeDiffSource::same())).await;
        assert_eq!(handed.len(), 1);
        assert_eq!(
            handed[0].unchanged_from,
            vec![HEAD_A.to_string()],
            "the reviewed head whose diff is identical must be handed over as proof"
        );
    }

    /// The dangerous direction, wired: when the comparison finds a changed diff it proves NOTHING,
    /// and the control task arms a normal round. An implementation that reported "unchanged" from a
    /// comparison it did not complete turns this red.
    #[tokio::test(start_paused = true)]
    async fn the_watcher_proves_nothing_when_the_diff_changed() {
        let handed = run_one_watch_tick(
            watched_pr(&[HEAD_A], &[]),
            Arc::new(FakeDiffSource::changed()),
        )
        .await;
        assert_eq!(handed.len(), 1);
        assert!(
            handed[0].unchanged_from.is_empty(),
            "a changed diff is not proof of anything"
        );
    }

    /// The comparison costs `gh` reads, so it is spent only when a head move is even possible
    /// (STUDIO-960): a head already read, a head already dispatched, and a pull request with no
    /// reviewed head at all all cost ZERO calls.
    #[tokio::test(start_paused = true)]
    async fn the_watcher_compares_only_when_a_head_move_is_possible() {
        for watched in [
            // Already read at this head: no move.
            watched_pr(&[HEAD_B], &[]),
            // A round is already dispatched at this head: the edge trigger arms nothing.
            watched_pr(&[], &[HEAD_B]),
            // Nothing has ever been reviewed, so there is nothing to compare against.
            watched_pr(&[], &[]),
        ] {
            let calls = Arc::new(Mutex::new(0usize));
            let handed =
                run_one_watch_tick(watched, Arc::new(CountingDiffSource(Arc::clone(&calls)))).await;
            assert_eq!(handed.len(), 1);
            assert!(
                handed[0].unchanged_from.is_empty(),
                "no comparison means no proof"
            );
            assert_eq!(
                *calls.lock().expect("count"),
                0,
                "no gh read may be spent when nothing could have moved"
            );
        }
    }

    /// A watched pull request at the fixed head [`HEAD_B`] with the given reviewed/requested SHAs.
    fn watched_pr(reviewed: &[&str], requested: &[&str]) -> WatchedPr {
        WatchedPr {
            pr: coord(12),
            reviewed_shas: reviewed.iter().map(|s| (*s).to_string()).collect(),
            requested_shas: requested.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    /// Runs exactly one watcher tick against a pull request at [`HEAD_B`], and returns the
    /// observations handed to the control task.
    async fn run_one_watch_tick(
        watched: WatchedPr,
        diff: Arc<dyn ReviewDiffSource>,
    ) -> Vec<PrObservation> {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let boundaries = Arc::new(Mutex::new(Vec::new()));
        let done = Arc::new(tokio::sync::Notify::new());
        let signal = CancelSignal::new();
        let deps = ReviewWatchDeps {
            pr_source: Some(Arc::new(FixedHeadSource(HEAD_B))),
            allow: HeadAllowlist::none(),
            poll_interval_ms: test_poll_interval(),
            teams: ticketless(&["alice", "bob"]),
            sink: Arc::new(FakeSink {
                watched: vec![watched],
                seen: Arc::clone(&seen),
                boundaries: Arc::clone(&boundaries),
                done: Arc::clone(&done),
                ..FakeSink::default()
            }),
            diff_source: Some(diff),
        };
        let task = tokio::spawn(run_review_watch_task(signal.wait(), deps));
        done.notified().await;
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        signal.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), task).await;
        let seen = seen.lock().expect("seen lock").clone();
        let boundaries = boundaries.lock().expect("boundaries lock").clone();
        let end = boundaries.get(1).copied().unwrap_or(seen.len());
        seen[..end].iter().flatten().cloned().collect()
    }

    /// The helper itself, in isolation: an identical patch on both heads is proof; a different one
    /// is not (the conflict case); and a read that failed proves nothing at all.
    #[tokio::test]
    async fn unchanged_reviewed_shas_proves_only_an_identical_patch() {
        let signal = CancelSignal::new();
        let ctx = signal.wait();
        let reviewed = [HEAD_A.to_string()];

        let same =
            unchanged_reviewed_shas(&ctx, &FakeDiffSource::same(), &coord(12), HEAD_B, &reviewed)
                .await;
        assert_eq!(same, vec![HEAD_A.to_string()]);

        for source in [
            FakeDiffSource::changed(),
            FakeDiffSource::base_fails(),
            FakeDiffSource::old_fails(),
        ] {
            let got = unchanged_reviewed_shas(&ctx, &source, &coord(12), HEAD_B, &reviewed).await;
            assert!(got.is_empty(), "only a fully-read, identical diff is proof");
        }
    }

    /// A head that is already one of the reviewed SHAs is not a move and costs no `gh` call; so does
    /// an empty reviewed set. Asserted on a source that would panic-free return either way by
    /// counting calls.
    #[tokio::test]
    async fn unchanged_reviewed_shas_costs_nothing_when_nothing_moved() {
        let signal = CancelSignal::new();
        let ctx = signal.wait();

        assert!(
            unchanged_reviewed_shas(&ctx, &FakeDiffSource::same(), &coord(12), HEAD_B, &[])
                .await
                .is_empty()
        );
        assert!(
            unchanged_reviewed_shas(
                &ctx,
                &FakeDiffSource::same(),
                &coord(12),
                HEAD_A,
                &[HEAD_A.to_string()],
            )
            .await
            .is_empty()
        );
    }

    /// A watch set larger than the per-tick `gh` budget must not starve its tail. The list comes
    /// back in a stable order and `sweep_pr_states` takes the first N of it, so polling from the
    /// front every tick would ask about the same 20 pull requests forever — the budget's
    /// "picked up next tick" promise is the caller's to keep.
    #[tokio::test(start_paused = true)]
    async fn the_poll_list_rotates_so_nothing_past_the_budget_starves() {
        let budget = crate::prstate::MAX_PR_STATE_CALLS_PER_TICK;
        let total = budget + 5;
        let watched: Vec<WatchedPr> = (0..total)
            .map(|n| WatchedPr::new(coord(n as i64 + 1)))
            .collect();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let boundaries = Arc::new(Mutex::new(Vec::new()));
        let done = Arc::new(tokio::sync::Notify::new());
        let signal = CancelSignal::new();
        let deps = ReviewWatchDeps {
            pr_source: Some(Arc::new(FakeSource)),
            allow: HeadAllowlist::none(),
            poll_interval_ms: test_poll_interval(),
            teams: ticketless(&["alice", "bob"]),
            diff_source: None,
            sink: Arc::new(FakeSink {
                watched: watched.clone(),
                seen: Arc::clone(&seen),
                boundaries: Arc::clone(&boundaries),
                done: Arc::clone(&done),
                ..FakeSink::default()
            }),
        };
        let task = tokio::spawn(run_review_watch_task(signal.wait(), deps));

        // Two ticks is enough to cover a list one budget-and-a-bit long.
        tokio::time::sleep(crate::prstate::PR_STATE_POLL_INTERVAL * 3).await;
        signal.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), task).await;

        let ticks = seen.lock().expect("seen lock").clone();
        let boundaries = boundaries.lock().expect("boundaries lock").clone();
        assert!(
            boundaries.len() >= 2,
            "expected at least two ticks, got {}",
            boundaries.len()
        );
        // Since STUDIO-953 a tick is a run of single-observation hand-backs, not one batch, so a
        // tick boundary is no longer visible by counting to `budget` — the FakeSink records one at
        // the start of each tick (`watched` is called exactly once per tick), and the first tick is
        // the slice between the first two boundaries. Halving the budget reds this.
        let first_tick: Vec<&PrObservation> = ticks[boundaries[0]..boundaries[1]]
            .iter()
            .flatten()
            .collect();
        assert_eq!(
            first_tick.len(),
            budget,
            "the first tick spends the whole budget"
        );
        let covered: HashSet<PrCoord> = ticks.iter().flatten().map(|o| o.pr.clone()).collect();
        for pr in &watched {
            assert!(covered.contains(&pr.pr), "{:?} was never polled", pr.pr);
        }
    }

    /// STUDIO-712: the auto-Done moves the control task decided are performed out HERE, on the
    /// watcher's own task, because each is a tracker round-trip. The loop decides, the task moves.
    #[tokio::test(start_paused = true)]
    async fn the_task_performs_the_moves_the_control_task_decided() {
        let plan = crate::reviewdone::ReviewDonePlan {
            pr: format!("{OWNER}/{REPO}#64"),
            issue_id: "ID-STUDIO-712".to_string(),
            team_id: "TEAM-1".to_string(),
            identifier: "STUDIO-712".to_string(),
            state: "Done".to_string(),
        };
        let seen = Arc::new(Mutex::new(Vec::new()));
        let done = Arc::new(tokio::sync::Notify::new());
        let finished = Arc::new(Mutex::new(Vec::new()));
        let signal = CancelSignal::new();
        let deps = ReviewWatchDeps {
            pr_source: Some(Arc::new(FakeSource)),
            allow: HeadAllowlist::none(),
            poll_interval_ms: test_poll_interval(),
            teams: ticketless(&["alice", "bob"]),
            diff_source: None,
            sink: Arc::new(FakeSink {
                watched: vec![WatchedPr::new(coord(64))],
                seen: Arc::clone(&seen),
                done: Arc::clone(&done),
                hand_back: ReviewSweepReport {
                    done: vec![plan.clone()],
                    ..ReviewSweepReport::default()
                },
                finished: Arc::clone(&finished),
                ..FakeSink::default()
            }),
        };
        let task = tokio::spawn(run_review_watch_task(signal.wait(), deps));

        done.notified().await;
        // One more scheduling pass, so the move that follows the hand-back can run.
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        signal.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), task).await;

        assert_eq!(
            finished.lock().expect("finished lock")[0],
            plan,
            "the plan the control task decided must reach the tracker unchanged"
        );
    }

    /// §16: with Teams off the task spawns no process at all — `sweep_pr_states` refuses — so it
    /// hands nothing back however many rows a stale watch set names.
    #[tokio::test(start_paused = true)]
    async fn the_task_asks_github_nothing_with_teams_off() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let done = Arc::new(tokio::sync::Notify::new());
        let signal = CancelSignal::new();
        let deps = ReviewWatchDeps {
            pr_source: Some(Arc::new(FakeSource)),
            allow: HeadAllowlist::none(),
            poll_interval_ms: test_poll_interval(),
            teams: teams_with(false, ReviewMode::Ticketless, vec![ident("bob", 0)]),
            diff_source: None,
            sink: Arc::new(FakeSink {
                watched: vec![WatchedPr::new(coord(12))],
                seen: Arc::clone(&seen),
                done: Arc::clone(&done),
                ..FakeSink::default()
            }),
        };
        let task = tokio::spawn(run_review_watch_task(signal.wait(), deps));

        tokio::time::sleep(crate::prstate::PR_STATE_POLL_INTERVAL * 3).await;
        signal.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), task).await;

        assert!(
            seen.lock().expect("seen lock").is_empty(),
            "a Teams-off daemon must observe nothing and decide nothing"
        );
    }

    // --- the pre-dispatch head re-read (STUDIO-953) ---------------------------------------------

    /// The head of a `Found` observation, or `None` for every other answer.
    fn head_of(obs: &PrObservation) -> Option<&str> {
        match &obs.lookup {
            PrLookup::Found(snap) => Some(snap.head_sha.as_str()),
            _ => None,
        }
    }

    /// A [`PrStateSource`] answering its FIRST call with `first` and every later call with `rest` —
    /// "the author pushed between the sweep's lookup and the pre-dispatch re-read".
    struct OnceThenSource {
        calls: AtomicUsize,
        first: String,
        rest: String,
    }

    #[async_trait]
    impl PrStateSource for OnceThenSource {
        async fn pr_state(
            &self,
            _owner: &str,
            _repo: &str,
            _number: i64,
            _allow: &HeadAllowlist,
        ) -> PrStateResult {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            let head = if call == 0 { &self.first } else { &self.rest };
            Ok(PrLookup::Found(PrSnapshot {
                is_draft: Some(false),
                head_sha: head.clone(),
                status: PrStatus::Open,
                merged_at: None,
                head_repo: format!("{OWNER}/{REPO}"),
                merge_state: String::new(),
            }))
        }
    }

    /// A [`PrStateSource`] whose head ADVANCES on every call — an author pushing continuously.
    struct AdvancingSource {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl PrStateSource for AdvancingSource {
        async fn pr_state(
            &self,
            _owner: &str,
            _repo: &str,
            _number: i64,
            _allow: &HeadAllowlist,
        ) -> PrStateResult {
            let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(PrLookup::Found(PrSnapshot {
                is_draft: Some(false),
                head_sha: format!("{n:040}"),
                status: PrStatus::Open,
                merged_at: None,
                head_repo: format!("{OWNER}/{REPO}"),
                merge_state: String::new(),
            }))
        }
    }

    /// A [`PrStateSource`] replaying a scripted list of answers and counting how often it was asked.
    struct ScriptedSource {
        answers: Mutex<Vec<PrStateResult>>,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl PrStateSource for ScriptedSource {
        async fn pr_state(
            &self,
            _owner: &str,
            _repo: &str,
            _number: i64,
            _allow: &HeadAllowlist,
        ) -> PrStateResult {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let mut answers = self.answers.lock().expect("answers lock");
            if answers.is_empty() {
                Ok(PrLookup::Gone)
            } else {
                answers.remove(0)
            }
        }
    }

    /// A sink that runs the REAL control decision ([`Orchestrator::handle_review_sweep`]) behind a
    /// `Mutex` standing in for the single control task, so a test can assert what the watcher's
    /// hand-back actually caused to be dispatched rather than merely what it handed over.
    struct ControlStubSink {
        watched: Vec<WatchedPr>,
        orch: Mutex<Orchestrator>,
        done: Arc<tokio::sync::Notify>,
    }

    #[async_trait]
    impl ReviewWatchSink for ControlStubSink {
        async fn watched(&self) -> Vec<WatchedPr> {
            self.watched.clone()
        }
        async fn sweep(
            &self,
            observed: Vec<PrObservation>,
            slots: Option<i64>,
        ) -> (ReviewSweepReport, i64) {
            let (report, left) = self
                .orch
                .lock()
                .expect("orchestrator lock")
                .handle_review_sweep_slots(&observed, slots);
            self.done.notify_one();
            (report, left)
        }
        async fn unreadable(&self, failed: Vec<PrCoord>) {
            self.orch
                .lock()
                .expect("orchestrator lock")
                .handle_review_unreadable(&failed);
        }
        async fn merge(&self, _plan: crate::automerge::AutoMergePlan) {}
        async fn finish(&self, _plan: crate::reviewdone::ReviewDonePlan) {}
        async fn nudge(&self, _plan: crate::draftpoke::DraftNudge) {}
        async fn adjudicate(&self, _plan: crate::reviewadjudicate::ReviewAdjudicationPlan) {}
    }

    /// The re-read itself, driven directly: a moved head is ADOPTED, a non-open observation is
    /// never re-asked about, and a FAILED re-read keeps the observed answer rather than dropping
    /// the review.
    #[tokio::test]
    async fn the_pre_dispatch_re_read_adopts_a_moved_head_and_keeps_a_failed_one() {
        let calls = Arc::new(AtomicUsize::new(0));
        let src = ScriptedSource {
            answers: Mutex::new(vec![
                Ok(open_at(12, HEAD_B).lookup), // the head moved since the sweep
                Err("gh: API rate limit exceeded".into()), // the re-read failed
            ]),
            calls: Arc::clone(&calls),
        };
        let teams = ticketless(&["alice", "bob"]);

        let moved = refresh_observed_head(
            &CancelWait::default(),
            &teams,
            &src,
            &HeadAllowlist::none(),
            open_at(12, HEAD_A),
        )
        .await;
        let failed = refresh_observed_head(
            &CancelWait::default(),
            &teams,
            &src,
            &HeadAllowlist::none(),
            open_at(13, HEAD_A),
        )
        .await;
        let gone = refresh_observed_head(
            &CancelWait::default(),
            &teams,
            &src,
            &HeadAllowlist::none(),
            observed(14, PrLookup::Gone),
        )
        .await;

        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "only the two OPEN observations may be re-read"
        );
        assert_eq!(
            head_of(&moved),
            Some(HEAD_B),
            "the moved head is adopted, not the swept one"
        );
        assert_eq!(
            head_of(&failed),
            Some(HEAD_A),
            "a failed re-read keeps the observed head; the review still dispatches"
        );
        assert_eq!(
            gone.lookup,
            PrLookup::Gone,
            "a non-open observation dispatches nothing and is not re-read"
        );
    }

    /// §16, the nit a reviewer flagged: the re-read carries the same master gate as every other
    /// entry point in this subsystem — with Teams off it asks GitHub nothing and adopts nothing.
    #[tokio::test]
    async fn the_pre_dispatch_re_read_is_dormant_with_teams_off() {
        let calls = Arc::new(AtomicUsize::new(0));
        let src = ScriptedSource {
            answers: Mutex::new(vec![Ok(open_at(12, HEAD_B).lookup)]),
            calls: Arc::clone(&calls),
        };
        let teams = teams_with(false, ReviewMode::Ticketless, vec![ident("bob", 0)]);

        let kept = refresh_observed_head(
            &CancelWait::default(),
            &teams,
            &src,
            &HeadAllowlist::none(),
            open_at(12, HEAD_A),
        )
        .await;

        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "a Teams-off re-read must ask GitHub nothing"
        );
        assert_eq!(head_of(&kept), Some(HEAD_A), "and must not adopt anything");
    }

    /// The ticket's acceptance case: the author pushes between the observation snapshot and the
    /// dispatch. The review must be pinned to the head live when it fires, never to the superseded
    /// snapshot. Removing [`refresh_observed_head`] reds this test.
    ///
    /// Deliberately NOT named after makewhatis/rhapsody#185: the log shows that incident's window
    /// was dispatch→verdict, not observation→dispatch, so this guard would not have prevented it
    /// (see the module doc). Naming the test after it would assert a causality the log refutes.
    #[tokio::test(start_paused = true)]
    async fn a_head_that_moves_between_the_observation_and_its_dispatch_is_reviewed_at_the_new_head()
     {
        let (o, dispatched) = orch(ticketless(&["alice", "bob"]));
        introduce(&o, row(12, "bob"));
        let done = Arc::new(tokio::sync::Notify::new());
        let sink = Arc::new(ControlStubSink {
            watched: vec![WatchedPr::new(coord(12))],
            orch: Mutex::new(o),
            done: Arc::clone(&done),
        });
        let deps = ReviewWatchDeps {
            pr_source: Some(Arc::new(OnceThenSource {
                calls: AtomicUsize::new(0),
                first: HEAD_A.to_string(),
                rest: HEAD_B.to_string(),
            })),
            allow: HeadAllowlist::none(),
            poll_interval_ms: test_poll_interval(),
            teams: ticketless(&["alice", "bob"]),
            diff_source: None,
            sink: sink.clone(),
        };
        let signal = CancelSignal::new();
        let task = tokio::spawn(run_review_watch_task(signal.wait(), deps));

        let ticked =
            tokio::time::timeout(crate::prstate::PR_STATE_POLL_INTERVAL * 3, done.notified()).await;
        assert!(
            ticked.is_ok(),
            "the watcher never handed a tick back at all"
        );
        signal.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), task).await;

        let guard = sink.orch.lock().expect("orchestrator lock");
        assert_eq!(
            watch_row(&guard, 12, "bob").requested_sha,
            HEAD_B,
            "the watch row was pinned to the superseded observation, not the live head"
        );
        let entries = dispatched.lock().expect("dispatched lock");
        assert_eq!(entries.len(), 1, "exactly one review round fired");
        assert_eq!(
            entries[0].review.as_ref().map(|r| r.head_sha.as_str()),
            Some(HEAD_B),
            "the worker was sent to the head the author had NOT superseded"
        );
    }

    /// A [`PrStateSource`] that records each pre-dispatch RE-READ (the first call per number is the
    /// sweep's own lookup, every later one the re-read) and, when asked about `later`, advances
    /// `earlier`'s head — "the first pull request's author pushes while the second is re-read".
    struct InterleavingSource {
        counts: Mutex<HashMap<i64, usize>>,
        head: Mutex<HashMap<i64, String>>,
        events: Arc<Mutex<Vec<String>>>,
        earlier: i64,
        later: i64,
    }

    #[async_trait]
    impl PrStateSource for InterleavingSource {
        async fn pr_state(
            &self,
            _owner: &str,
            _repo: &str,
            number: i64,
            _allow: &HeadAllowlist,
        ) -> PrStateResult {
            let call = {
                let mut counts = self.counts.lock().expect("counts lock");
                let call = counts.entry(number).or_insert(0);
                *call += 1;
                *call
            };
            if call > 1 {
                self.events
                    .lock()
                    .expect("events lock")
                    .push(format!("refresh:{number}"));
                if number == self.later {
                    self.head
                        .lock()
                        .expect("head lock")
                        .insert(self.earlier, HEAD_B.to_string());
                }
            }
            let head = self
                .head
                .lock()
                .expect("head lock")
                .get(&number)
                .cloned()
                .unwrap_or_else(|| HEAD_A.to_string());
            Ok(PrLookup::Found(PrSnapshot {
                is_draft: Some(false),
                head_sha: head,
                status: PrStatus::Open,
                merged_at: None,
                head_repo: format!("{OWNER}/{REPO}"),
                merge_state: String::new(),
            }))
        }
    }

    /// A sink running the real control decision AND recording the order in which each observation
    /// was handed over — so a test can assert the watcher interleaves re-read and hand-back rather
    /// than batching the re-reads.
    struct OrderRecordingSink {
        watched: Vec<WatchedPr>,
        orch: Mutex<Orchestrator>,
        done: Arc<tokio::sync::Notify>,
        events: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl ReviewWatchSink for OrderRecordingSink {
        async fn watched(&self) -> Vec<WatchedPr> {
            self.watched.clone()
        }
        async fn sweep(
            &self,
            observed: Vec<PrObservation>,
            slots: Option<i64>,
        ) -> (ReviewSweepReport, i64) {
            for obs in &observed {
                self.events
                    .lock()
                    .expect("events lock")
                    .push(format!("sweep:{}", obs.pr.number));
            }
            let (report, left) = self
                .orch
                .lock()
                .expect("orchestrator lock")
                .handle_review_sweep_slots(&observed, slots);
            self.done.notify_one();
            (report, left)
        }
        async fn unreadable(&self, failed: Vec<PrCoord>) {
            self.orch
                .lock()
                .expect("orchestrator lock")
                .handle_review_unreadable(&failed);
        }
        async fn merge(&self, _plan: crate::automerge::AutoMergePlan) {}
        async fn finish(&self, _plan: crate::reviewdone::ReviewDonePlan) {}
        async fn nudge(&self, _plan: crate::draftpoke::DraftNudge) {}
        async fn adjudicate(&self, _plan: crate::reviewadjudicate::ReviewAdjudicationPlan) {}
    }

    /// Sol's blocking finding on #189: a two-pull-request tick must hand each re-read head to the
    /// control task BEFORE re-reading the next, or the first pull request waits behind the second's
    /// blocking `gh` call exactly as it did when the re-read was a second batch. This asserts the
    /// ORDER — the first pull request's hand-back sits between its own re-read and the later
    /// re-read — which a batched implementation cannot satisfy (it would log
    /// `refresh:12, refresh:13, sweep:12, sweep:13`).
    #[tokio::test(start_paused = true)]
    async fn a_multi_pr_tick_hands_each_re_read_head_over_before_re_reading_the_next() {
        let (o, _dispatched) = orch(ticketless(&["alice", "bob"]));
        introduce(&o, row(12, "bob"));
        introduce(&o, row(13, "bob"));
        let done = Arc::new(tokio::sync::Notify::new());
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::new(OrderRecordingSink {
            watched: vec![WatchedPr::new(coord(12)), WatchedPr::new(coord(13))],
            orch: Mutex::new(o),
            done: Arc::clone(&done),
            events: Arc::clone(&events),
        });
        let deps = ReviewWatchDeps {
            pr_source: Some(Arc::new(InterleavingSource {
                counts: Mutex::new(HashMap::new()),
                head: Mutex::new(HashMap::from([
                    (12, HEAD_A.to_string()),
                    (13, HEAD_A.to_string()),
                ])),
                events: Arc::clone(&events),
                earlier: 12,
                later: 13,
            })),
            allow: HeadAllowlist::none(),
            poll_interval_ms: test_poll_interval(),
            teams: ticketless(&["alice", "bob"]),
            diff_source: None,
            sink: sink.clone(),
        };
        let signal = CancelSignal::new();
        let task = tokio::spawn(run_review_watch_task(signal.wait(), deps));

        let ticked =
            tokio::time::timeout(crate::prstate::PR_STATE_POLL_INTERVAL * 3, done.notified()).await;
        assert!(
            ticked.is_ok(),
            "the watcher never handed a tick back at all"
        );
        // Let the rest of the tick (the second observation) finish before reading the log.
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        signal.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), task).await;

        let events = events.lock().expect("events lock").clone();
        assert_eq!(
            events,
            vec!["refresh:12", "sweep:12", "refresh:13", "sweep:13"],
            "each re-read must be handed over before the next blocking re-read begins"
        );
    }

    /// The livelock guard, as a test rather than a comment: an author who pushes on EVERY
    /// observation must still get a review. A rule that refused to dispatch whenever the head moved
    /// would leave the row untouched forever — strictly worse than reviewing slightly-stale code,
    /// and the failure mode this subsystem has spent weeks eliminating. The re-read ADOPTS the new
    /// head, so the round fires on the very first tick; a defer-on-every-move implementation would
    /// leave `dispatched` empty here and red this assertion rather than hang.
    #[tokio::test(start_paused = true)]
    async fn an_author_pushing_on_every_observation_still_gets_a_review() {
        let (o, dispatched) = orch(ticketless(&["alice", "bob"]));
        introduce(&o, row(12, "bob"));
        let done = Arc::new(tokio::sync::Notify::new());
        let sink = Arc::new(ControlStubSink {
            watched: vec![WatchedPr::new(coord(12))],
            orch: Mutex::new(o),
            done: Arc::clone(&done),
        });
        let deps = ReviewWatchDeps {
            pr_source: Some(Arc::new(AdvancingSource {
                calls: AtomicUsize::new(0),
            })),
            allow: HeadAllowlist::none(),
            poll_interval_ms: test_poll_interval(),
            teams: ticketless(&["alice", "bob"]),
            diff_source: None,
            sink: sink.clone(),
        };
        let signal = CancelSignal::new();
        let task = tokio::spawn(run_review_watch_task(signal.wait(), deps));

        // Bounded rather than a bare `notified().await`: an implementation that never hands a tick
        // back must RED this test, not hang it (the ticket's mutation discipline).
        let ticked =
            tokio::time::timeout(crate::prstate::PR_STATE_POLL_INTERVAL * 3, done.notified()).await;
        assert!(
            ticked.is_ok(),
            "the watcher never handed a tick back at all"
        );
        signal.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), task).await;

        let guard = sink.orch.lock().expect("orchestrator lock");
        assert_eq!(
            dispatched.lock().expect("dispatched lock").len(),
            1,
            "a continuously-pushed head must still be reviewed within a bounded number of ticks"
        );
        assert!(
            !watch_row(&guard, 12, "bob").requested_sha.is_empty(),
            "the round must be recorded as requested, not silently dropped"
        );
    }

    /// A sink running the real control decision and then simulating the just-dispatched review
    /// worker exiting before the next observation is handed over — the seam sol reproduced on #189.
    /// The control task processes that exit between hand-backs, so a budget recomputed per hand-back
    /// would see the slot as free and let one tick exceed `max_concurrent`.
    struct WorkerExitSink {
        watched: Vec<WatchedPr>,
        orch: Mutex<Orchestrator>,
        /// One message per hand-back, so a test can await the tick without busy-waiting (which
        /// would stop tokio's paused clock from advancing to the watcher's poll interval).
        handed: tokio::sync::mpsc::UnboundedSender<()>,
        /// Reviews the control task dispatched across the tick.
        dispatched: Arc<Mutex<usize>>,
    }

    #[async_trait]
    impl ReviewWatchSink for WorkerExitSink {
        async fn watched(&self) -> Vec<WatchedPr> {
            self.watched.clone()
        }
        async fn sweep(
            &self,
            observed: Vec<PrObservation>,
            slots: Option<i64>,
        ) -> (ReviewSweepReport, i64) {
            let mut orch = self.orch.lock().expect("orchestrator lock");
            let (report, left) = orch.handle_review_sweep_slots(&observed, slots);
            if report.dispatched > 0 {
                // The worker just dispatched exits before the next hand-back.
                orch.running.clear();
            }
            drop(orch);
            *self.dispatched.lock().expect("dispatched lock") += report.dispatched;
            let _ = self.handed.send(());
            (report, left)
        }
        async fn unreadable(&self, failed: Vec<PrCoord>) {
            self.orch
                .lock()
                .expect("orchestrator lock")
                .handle_review_unreadable(&failed);
        }
        async fn merge(&self, _plan: crate::automerge::AutoMergePlan) {}
        async fn finish(&self, _plan: crate::reviewdone::ReviewDonePlan) {}
        async fn nudge(&self, _plan: crate::draftpoke::DraftNudge) {}
        async fn adjudicate(&self, _plan: crate::reviewadjudicate::ReviewAdjudicationPlan) {}
    }

    /// The daemon-wide dispatch budget is counted ONCE per watcher tick, not once per observation.
    /// Four due pull requests at `max_concurrent = 1`, with the dispatched worker exiting between
    /// hand-backs, must still dispatch exactly one review: the slot is spent for the tick. This is
    /// the shape production actually uses (`sink.sweep(vec![fresh])` per observation), which the
    /// batched [`the_daemon_wide_concurrency_cap_bounds_one_tick`] no longer exercises.
    ///
    /// Mutation: drop the `slots` carry in `run_review_watch_task` (pass `None` every time) and this
    /// reds with 4 against 1.
    #[tokio::test(start_paused = true)]
    async fn the_daemon_wide_cap_bounds_a_tick_of_single_observation_sweeps() {
        let (mut o, _dispatched) = orch(ticketless(&["alice", "bob", "carol", "dave"]));
        o.eff.as_mut().expect("eff").max_concurrent = 1;
        for n in 12..16 {
            introduce(&o, row(n, "bob"));
        }
        let (handed, mut hand_backs) = tokio::sync::mpsc::unbounded_channel();
        let total = Arc::new(Mutex::new(0usize));
        let sink = Arc::new(WorkerExitSink {
            watched: (12..16).map(|n| WatchedPr::new(coord(n))).collect(),
            orch: Mutex::new(o),
            handed,
            dispatched: Arc::clone(&total),
        });
        let deps = ReviewWatchDeps {
            pr_source: Some(Arc::new(FakeSource)),
            allow: HeadAllowlist::none(),
            poll_interval_ms: test_poll_interval(),
            teams: ticketless(&["alice", "bob", "carol", "dave"]),
            diff_source: None,
            sink: sink.clone(),
        };
        let signal = CancelSignal::new();
        let task = tokio::spawn(run_review_watch_task(signal.wait(), deps));

        // All four observations of the first tick are handed over one at a time; wait for the tick
        // to complete before cancelling, bounded so a never-firing implementation reds rather than
        // hangs.
        let whole_tick = async {
            for _ in 0..4 {
                if hand_backs.recv().await.is_none() {
                    break;
                }
            }
        };
        tokio::time::timeout(crate::prstate::PR_STATE_POLL_INTERVAL * 3, whole_tick)
            .await
            .expect("the watcher never handed the whole tick back");
        signal.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), task).await;

        assert_eq!(
            *total.lock().expect("dispatched lock"),
            1,
            "a worker exiting mid-tick must not replenish the tick's dispatch budget"
        );
        let guard = sink.orch.lock().expect("orchestrator lock");
        assert_eq!(
            watch_row(&guard, 12, "bob").requested_sha,
            format!("{:040}", 12),
            "the one dispatch the budget allowed is the first pull request"
        );
        assert!(
            watch_row(&guard, 13, "bob").requested_sha.is_empty(),
            "the round past the spent budget must stay un-dispatched, re-considered next tick"
        );
    }

    /// A sink running the real control decision and then simulating the control task starting an
    /// unrelated TICKET run while the watcher sits in its blocking pre-dispatch `gh` read — the
    /// opposite direction from [`WorkerExitSink`]. It starts exactly ONE such run, on the first
    /// hand-back: a real control task starts no more once `running` is at its cap.
    struct TicketStartSink {
        watched: Vec<WatchedPr>,
        orch: Mutex<Orchestrator>,
        /// One message per hand-back, so a test can await the tick without busy-waiting (which
        /// would stop tokio's paused clock from advancing to the watcher's poll interval).
        handed: tokio::sync::mpsc::UnboundedSender<()>,
        /// The most agents (reviews + the ticket run) ever live at once, read after each hand-back.
        peak: Arc<Mutex<usize>>,
        /// Reviews the control task dispatched across the tick.
        dispatched: Arc<Mutex<usize>>,
        started: AtomicUsize,
    }

    #[async_trait]
    impl ReviewWatchSink for TicketStartSink {
        async fn watched(&self) -> Vec<WatchedPr> {
            self.watched.clone()
        }
        async fn sweep(
            &self,
            observed: Vec<PrObservation>,
            slots: Option<i64>,
        ) -> (ReviewSweepReport, i64) {
            let mut orch = self.orch.lock().expect("orchestrator lock");
            let (report, left) = orch.handle_review_sweep_slots(&observed, slots);
            if self.started.fetch_add(1, Ordering::SeqCst) == 0 {
                // The control task started an ordinary ticket run while the watcher was blocked on
                // `gh`, filling the last free slot. A later hand-back that keeps the carried budget
                // would spend a slot this run already holds.
                let mut busy = RunningEntry::empty(rhapsody_core::Issue {
                    id: "iss-ticket".to_string(),
                    identifier: "STUDIO-999".to_string(),
                    ..Default::default()
                });
                busy.identity = "carol".to_string();
                orch.running.insert("iss-ticket".to_string(), busy);
            }
            let live = orch.running.len();
            drop(orch);
            let mut peak = self.peak.lock().expect("peak lock");
            *peak = (*peak).max(live);
            let _ = self.handed.send(());
            *self.dispatched.lock().expect("dispatched lock") += report.dispatched;
            (report, left)
        }
        async fn unreadable(&self, failed: Vec<PrCoord>) {
            self.orch
                .lock()
                .expect("orchestrator lock")
                .handle_review_unreadable(&failed);
        }
        async fn merge(&self, _plan: crate::automerge::AutoMergePlan) {}
        async fn finish(&self, _plan: crate::reviewdone::ReviewDonePlan) {}
        async fn nudge(&self, _plan: crate::draftpoke::DraftNudge) {}
        async fn adjudicate(&self, _plan: crate::reviewadjudicate::ReviewAdjudicationPlan) {}
    }

    /// The carried budget must compose with a FRESH count, not replace it (STUDIO-953, jimmy's
    /// round-4 blocker). `max_concurrent = 2`; the first hand-back dispatches one review and the
    /// control task then starts one ordinary ticket run, so `running` is at its cap. A later
    /// hand-back must dispatch nothing: a budget counted before that run existed would spend a slot
    /// that no longer exists and take `running` to 3. This is the START direction that
    /// [`the_daemon_wide_cap_bounds_a_tick_of_single_observation_sweeps`] does not exercise — it
    /// covers a worker EXITING mid-tick, which moves the budget the other way.
    ///
    /// Mutation: replace the `left.min(fresh_budget)` clamp in `handle_review_sweep_slots` with the
    /// bare carry and this reds on peak 3 > 2.
    #[tokio::test(start_paused = true)]
    async fn a_run_started_mid_tick_lowers_the_carried_review_budget() {
        let (mut o, _dispatched) = orch(ticketless(&["alice", "bob", "carol", "dave"]));
        o.eff.as_mut().expect("eff").max_concurrent = 2;
        for n in 12..16 {
            introduce(&o, row(n, "bob"));
        }
        let (handed, mut hand_backs) = tokio::sync::mpsc::unbounded_channel();
        let peak = Arc::new(Mutex::new(0usize));
        let total = Arc::new(Mutex::new(0usize));
        let sink = Arc::new(TicketStartSink {
            watched: (12..16).map(|n| WatchedPr::new(coord(n))).collect(),
            orch: Mutex::new(o),
            handed,
            peak: Arc::clone(&peak),
            dispatched: Arc::clone(&total),
            started: AtomicUsize::new(0),
        });
        let deps = ReviewWatchDeps {
            pr_source: Some(Arc::new(FakeSource)),
            allow: HeadAllowlist::none(),
            poll_interval_ms: test_poll_interval(),
            teams: ticketless(&["alice", "bob", "carol", "dave"]),
            diff_source: None,
            sink: sink.clone(),
        };
        let signal = CancelSignal::new();
        let task = tokio::spawn(run_review_watch_task(signal.wait(), deps));

        let whole_tick = async {
            for _ in 0..4 {
                if hand_backs.recv().await.is_none() {
                    break;
                }
            }
        };
        tokio::time::timeout(crate::prstate::PR_STATE_POLL_INTERVAL * 3, whole_tick)
            .await
            .expect("the watcher never handed the whole tick back");
        signal.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), task).await;

        assert!(
            *peak.lock().expect("peak lock") <= 2,
            "a ticket run started mid-tick must lower the tick's remaining review budget, not be \
             ignored: peak {} agents against max_concurrent 2",
            *peak.lock().expect("peak lock")
        );
        assert_eq!(
            *total.lock().expect("dispatched lock"),
            1,
            "only the first review fits before the ticket run fills the last slot"
        );
        let guard = sink.orch.lock().expect("orchestrator lock");
        assert_eq!(
            watch_row(&guard, 12, "bob").requested_sha,
            format!("{:040}", 12),
            "the one dispatch the budget allowed is the first pull request"
        );
        assert!(
            watch_row(&guard, 13, "bob").requested_sha.is_empty(),
            "a head the budget can no longer afford must stay un-dispatched, re-considered next tick"
        );
    }
}
