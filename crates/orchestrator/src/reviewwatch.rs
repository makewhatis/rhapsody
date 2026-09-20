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
//! **STUDIO-960 sits inside that window, deliberately.** When a head move might be carryable the
//! diff comparison ([`unchanged_reviewed_shas`]) runs AFTER the re-read and before the hand-back,
//! so a head's dispatch now waits behind up to `1 + 1 + N` bounded `gh` execs — all on this task,
//! none on the control task. That does not reopen the #189 window it looks like: the proof is
//! pinned to the SHA the re-read just returned, so an author pushing DURING the comparison lands on
//! a head whose diff was never compared; the next tick sees that head, comparison fails to prove it,
//! and a normal round is armed. The saving can be lost to a race; a review cannot.
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
use rhapsody_store::{
    REVIEW_STATUS_APPROVED, REVIEW_STATUS_DROPPED, REVIEW_STATUS_REVIEWED, ReviewWatchRow,
};

use crate::control_loop::{CancelWait, Event};
use crate::ghsummons::{HeadAllowlist, PrLookup, PrStateSource, PrStatus, ReviewDiffSource};
use crate::orchestrator::Orchestrator;
use crate::prstate::{PrCoord, PrObservation, sweep_pr_states};
use crate::review::{ReviewDispatchOutcome, ReviewRun, review_key};
use crate::stop::ControlHandle;
use crate::teams::LoadSnapshot;

/// How many review ROUNDS one pull request may be given, ever, in one daemon lifetime — the floor
/// against force-push churn (§14.2, "no approval terminal → unbounded re-review").
///
/// A ROUND, not a dispatch. `review_rounds` counts dispatches, and one round costs one dispatch per
/// required reviewer, so the check multiplies this by `teams.review.effective_reviewers()` before
/// comparing (STUDIO-727). Comparing the raw counter would silently divide the budget by the
/// reviewer count — at `reviewers: 8` a pull request would get its first round and never be
/// re-reviewed again, with nothing above `debug!` to say so.
///
/// The edge trigger already bounds the RATE: a round cannot start while one is in flight, so a
/// pull request costs at most one review per review's duration however fast its author pushes. What
/// it does not bound is the TOTAL, and an author amending in a loop — a rebase chain, a CI-driven
/// force-push, a `--fixup` habit — would otherwise buy a full agent run per amendment forever.
/// Eight rounds is far above any honest review conversation (a review, fixes, a re-review, more
/// fixes) and far below a runaway.
///
/// Deliberately in memory rather than a column: it is a churn floor, not an audit record, and the
/// churn it guards against happens over minutes inside one daemon lifetime. A restart resets it,
/// which is the correct outcome for an operator who restarted the daemon to unstick something.
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

/// One live watch row's head state for a polled pull request (STUDIO-960).
///
/// Carried per ROW rather than as a union of the two SHA columns, because whether the diff
/// comparison is worth its `gh` calls depends on the PAIR: a row already dispatched at the new head
/// cannot consume a proof, however far behind another row's `last_reviewed_sha` sits. A union loses
/// that pairing and lets one reviewer's in-flight round suppress the proof a PEER still needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchedRow {
    /// The head this row's verdict was recorded at (`last_reviewed_sha`), or empty when it never
    /// completed a round.
    pub reviewed_sha: String,
    /// The head a round was DISPATCHED against (`requested_sha`), or empty.
    pub requested_sha: String,
    /// The row's status. Only a `reviewed`/`approved` row can carry a verdict, so the comparison is
    /// spent only where one of those exists — a `requested`/`in_flight`/`truncated` row cannot
    /// consume any proof and would otherwise buy two `gh` reads per tick for as long as it sits
    /// there.
    pub status: String,
}

/// One pull request the watcher should ask GitHub about this tick, with the head state of each of
/// its live rows (STUDIO-960).
///
/// The row states travel beside the coordinate rather than being re-read by the watcher, which
/// holds no store: the control task is what reads the watch set, and this is the one fact the
/// off-loop diff comparison needs from it. The watcher keeps no row state of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchedPr {
    /// The pull request's repository and number.
    pub pr: PrCoord,
    /// One entry per live watch row, in the store's stable order.
    pub rows: Vec<WatchedRow>,
}

impl WatchedPr {
    /// A watched pull request with no completed review yet — the shape a freshly-introduced row has.
    pub fn new(pr: PrCoord) -> WatchedPr {
        WatchedPr {
            pr,
            rows: Vec::new(),
        }
    }

    /// Distinct non-empty `last_reviewed_sha` values across this pull request's rows, de-duplicated
    /// — the heads the diff comparison must be able to prove unchanged.
    ///
    /// The comparison is asked to fingerprint the union, not one row's head at a time: it filters
    /// out any SHA equal to the new head itself, so the caller may hand it every reviewed head it
    /// has and spend one `gh` read per distinct one.
    pub fn reviewed_shas(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for row in &self.rows {
            let sha = row.reviewed_sha.trim();
            if !sha.is_empty() && !out.iter().any(|s| s == sha) {
                out.push(sha.to_string());
            }
        }
        out
    }

    /// Whether any row holds a verdict at a head other than `head` with no round in flight at
    /// `head` — the exact rows
    /// [`handle_review_head_advanced`](crate::orchestrator::Orchestrator::handle_review_head_advanced)
    /// could carry across, and therefore the only case worth spending the comparison on.
    ///
    /// Per ROW, not per union: a peer already at `head` (reviewed there or dispatched there) does
    /// not excuse the comparison for a row still behind it, and a non-terminal row cannot consume a
    /// proof at all.
    pub fn has_carry_candidate(&self, head: &str) -> bool {
        let head = head.trim();
        !head.is_empty()
            && self.rows.iter().any(|row| {
                matches!(
                    row.status.as_str(),
                    REVIEW_STATUS_REVIEWED | REVIEW_STATUS_APPROVED
                ) && is_carry_candidate(&row.reviewed_sha, &row.requested_sha, head)
            })
    }
}

/// Whether one row's verdict could be carried from its recorded head to `head`: it read at a
/// different, non-empty head, and no round of it is already in flight at `head`.
fn is_carry_candidate(reviewed_sha: &str, requested_sha: &str, head: &str) -> bool {
    let reviewed = reviewed_sha.trim();
    !reviewed.is_empty() && reviewed != head && requested_sha.trim() != head
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
}

impl ControlWatchSink {
    pub fn new(control: ControlHandle) -> ControlWatchSink {
        ControlWatchSink {
            control,
            automerge: None,
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
}

/// Re-reads one OPEN observation's head once, off-loop, immediately before that observation is
/// handed to the control task — the re-verification that closes the window between the batched `gh`
/// lookup and the dispatch it feeds (STUDIO-953).
///
/// One observation at a time, not a second batch. A batch would re-create the very window it exists
/// to close: the first pull request's re-read would still wait behind every later pull request's
/// blocking `gh` call before its dispatch, which is the defect a reviewer reproduced on #189. Each
/// caller therefore re-reads, compares if a carry is possible, and hands over in the same step, so
/// no LATER pull request's `gh` call sits between a head and its dispatch.
///
/// The caller's STUDIO-960 diff comparison runs after this re-read on purpose, and that is the one
/// `gh` work that does sit between a head and its dispatch (module doc: the proof is pinned to the
/// freshly re-read SHA, so a push racing it loses the saving and never the review). Putting the
/// comparison BEFORE the re-read would spend up to `1 + 1 + N` reads proving a SHA the re-read then
/// replaces, and would need the proof dropped whenever the two disagree — strictly more work for a
/// strictly worse answer.
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
        .pr_state(&obs.pr.owner, &obs.pr.repo, obs.pr.number, allow)
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

/// Polls the watch set on [`PR_STATE_POLL_INTERVAL`](crate::prstate::PR_STATE_POLL_INTERVAL) until
/// `ctx` is cancelled.
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
        interval_secs = crate::prstate::PR_STATE_POLL_INTERVAL.as_secs(),
        "ticketless review watcher started (off-loop; the control task is never blocked on gh)"
    );
    // Where this tick starts in the watch list. The list comes back in a STABLE order (owner, repo,
    // number, reviewer) and `sweep_pr_states` asks about at most `MAX_PR_STATE_CALLS_PER_TICK` of
    // it, so polling it from the front every tick would ask about the same first 20 pull requests
    // forever and never once look at the 21st — the budget's "picked up next tick" promise is the
    // CALLER's to keep, and this is where it is kept.
    let mut cursor = 0usize;
    loop {
        tokio::select! {
            _ = ctx.cancelled() => return,
            () = tokio::time::sleep(crate::prstate::PR_STATE_POLL_INTERVAL) => {}
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
        if sweep.deferred > 0 || sweep.failed > 0 {
            tracing::debug!(
                observed = sweep.observed.len(),
                budget_deferred = sweep.deferred,
                failed = sweep.failed,
                "ticketless review watcher: not every watched pull request answered this tick"
            );
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
            // here, and only when a carry is even possible. The test is per ROW
            // (`WatchedPr::has_carry_candidate`): a union of the reviewed/requested SHAs would let
            // one reviewer already at the new head suppress the proof for a peer still behind it,
            // billing that peer the full round this feature exists to avoid.
            if let Some(diff) = deps.diff_source.as_ref()
                && let PrLookup::Found(snap) = &fresh.lookup
                && snap.status == PrStatus::Open
                && let Some(known) = recorded.get(&fresh.pr)
                && known.has_carry_candidate(&snap.head_sha)
            {
                fresh.unchanged_from = unchanged_reviewed_shas(
                    &ctx,
                    diff.as_ref(),
                    &fresh.pr,
                    &snap.head_sha,
                    &known.reviewed_shas(),
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
            } = one;
            report.dispatched += dispatched;
            report.retired += retired;
            report.deferred += deferred;
            report.armed += armed;
            report.skipped += skipped;
            report.stalled += stalled;
            report.done.extend(done);
            report.merge.extend(merge);
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

impl Orchestrator {
    /// The pull requests the watcher asks GitHub about this tick: every distinct coordinate the
    /// watch set still considers live, each beside the head state of its rows (STUDIO-960).
    ///
    /// Distinct by coordinate rather than by row: N reviewers of one pull request share one head,
    /// and asking GitHub N times for it would spend the per-tick call budget on an answer already
    /// in hand. Each row's own head state travels so the comparison can tell a row already at the
    /// head from a peer still behind it — the pairing a union of the two SHA columns cannot keep.
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
            out[idx].rows.push(WatchedRow {
                reviewed_sha: row.last_reviewed_sha.trim().to_string(),
                requested_sha: row.requested_sha.trim().to_string(),
                status: row.status,
            });
        }
        out
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
                PrLookup::Found(snap) => self.service_review_pr(
                    &rows,
                    &obs.pr,
                    &snap.head_sha,
                    &obs.unchanged_from,
                    &mut slots,
                    &mut report,
                ),
            }
        }
        (report, slots)
    }

    /// The daemon-wide dispatch budget available to one watcher tick: `max_concurrent` less what is
    /// already running. No config loaded ⇒ no budget: a dispatch could not resolve a project to
    /// route with in any case.
    fn review_dispatch_budget(&self) -> i64 {
        self.eff
            .as_ref()
            .map(|eff| {
                crate::concurrency::global_slots(
                    eff.max_concurrent,
                    i64::try_from(self.running.len()).unwrap_or(i64::MAX),
                )
            })
            .unwrap_or(0)
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
        // And what was announced about its auto-merge plan, for the first two of those reasons.
        self.auto_merge_announced.remove(&churn_key(pr));
        for id in retired_ids {
            self.review_unassignable.remove(&id);
        }
        dropped
    }

    /// Services one OPEN pull request at `head`: re-arms whatever the advance re-armed, then
    /// dispatches a review round for every row that still owes one.
    fn service_review_pr(
        &mut self,
        rows: &[ReviewWatchRow],
        pr: &PrCoord,
        head: &str,
        unchanged_from: &[String],
        slots: &mut i64,
        report: &mut ReviewSweepReport,
    ) {
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
        let reviewers_per_round = self
            .teams
            .as_ref()
            .map_or(1, |t| t.review.effective_reviewers().max(1));

        let mine: Vec<&ReviewWatchRow> = rows.iter().filter(|r| row_is(r, pr)).collect();
        // Who currently holds each of this pull request's required reviews, updated AS the loop
        // reassigns. `mine` is this hand-back's opening snapshot, so reading peers off it directly
        // would go stale the moment one row is reassigned: the next row would still see the retired
        // reviewer as a peer and not see the substitute, and could hand that substitute a second
        // required review of the same pull request.
        let mut assigned: Vec<String> = mine.iter().map(|r| r.key.reviewer.clone()).collect();
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
            if *slots <= 0 {
                tracing::debug!(
                    pr = %pr,
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
                    }
                    let counter = self.review_rounds.entry(churn_key(pr)).or_default();
                    *counter += 1;
                    if *counter == REVIEW_ROUNDS_PER_PR_CAP.saturating_mul(reviewers_per_round) {
                        tracing::warn!(
                            pr = %pr, rounds = *counter,
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
                ReviewDispatchOutcome::Refused(why) => {
                    report.deferred += 1;
                    tracing::warn!(pr = %pr, reason = why, "ticketless review: the dispatch was refused");
                }
            }
        }

        self.propose_auto_merge(&mine, pr, head, report);
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
        report: &mut ReviewSweepReport,
    ) {
        if !self.review_auto_merge_for_repo(&pr.owner, &pr.repo) {
            return; // opt-in, and off by default (the D5 invariant)
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
        let incumbent = row.key.reviewer.as_str();
        if row.author.trim().is_empty() {
            // Roster membership, and deliberately NOT capacity (D2): a reviewer who has left the
            // roster since the row was written has no identity left to dispatch under, which is a
            // reason to defer that survives. Being at their implementation cap is not.
            let on_roster = teams.roster.iter().any(|i| i.name == incumbent);
            return on_roster.then(|| incumbent.to_string());
        }
        // `rank_reviewers` only ever names roster members, so `peers` is the whole filter — a
        // teammate at their `max_concurrent` is a candidate like any other (D2).
        let exclusions = self.reviewer_exclusions(teams);
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
        // ranking never promoted — off the roster, the author, `unpinnable` — is not a reviewer this
        // round yields to, so it must not evict the incumbent either. Reading the raw list made an
        // unpinnable pin break continuity for a teammate the ranking never selected, handing the
        // round to whoever merely led on load (round 3).
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

/// The per-pull-request re-review budget, keyed by `owner/repo#number`.
pub type ReviewRounds = HashMap<String, usize>;

/// The auto-merge plan each watched pull request has already been ANNOUNCED for: its head and the
/// approvals that cleared the gate at that head, keyed by [`churn_key`] as [`ReviewRounds`] is.
/// See [`Orchestrator::auto_merge_announced`]. STUDIO-881.
pub type AnnouncedPlans = HashMap<String, (String, Vec<String>)>;

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
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use rhapsody_config::teams::{Identity, Review, ReviewMode};
    use rhapsody_store::{
        REVIEW_STATUS_IN_FLIGHT, REVIEW_STATUS_REQUESTED, REVIEW_STATUS_TRUNCATED, ReviewWatchKey,
        Sqlite, StorePath,
    };
    use rhapsody_tracker::fake::Fake;

    use super::*;
    use crate::control_loop::CancelSignal;
    use crate::ghsummons::{PrSnapshot, PrStateResult, ReviewDiffResult};
    use crate::orchestrator::RunningEntry;
    use crate::testsupport::{DispatchedEntries, empty_effective, empty_resolved_project, set_of};

    const REPO_URL: &str = "git@github.com:makewhatis/rhapsody.git";
    const OWNER: &str = "makewhatis";
    const REPO: &str = "rhapsody";
    const HEAD_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const HEAD_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const HEAD_C: &str = "cccccccccccccccccccccccccccccccccccccccc";

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
    fn orch(teams: Teams) -> (Orchestrator, DispatchedEntries) {
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
        o.set_store(Arc::new(
            Sqlite::open(StorePath::InMemory).expect("open in-memory store"),
        ));
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
                is_draft: false,
                head_sha: head.to_string(),
                status: PrStatus::Open,
                merged_at: None,
                head_repo: format!("{OWNER}/{REPO}"),
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
            is_draft: false,
            head_sha: head.to_string(),
            status: PrStatus::Merged,
            merged_at: chrono::DateTime::parse_from_rfc3339("2026-09-10T00:00:00Z")
                .ok()
                .map(|t| t.with_timezone(&chrono::Utc)),
            head_repo: format!("{OWNER}/{REPO}"),
        })
    }

    /// One observation of a pull request that was CLOSED without merging.
    fn closed_at(head: &str) -> PrLookup {
        PrLookup::Found(PrSnapshot {
            is_draft: false,
            head_sha: head.to_string(),
            status: PrStatus::Closed,
            merged_at: None,
            head_repo: format!("{OWNER}/{REPO}"),
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

    /// The round-1 review's blocker, at the orchestrator level (STUDIO-960): the proof is spent for
    /// a row even when a PEER already sits at the new head, and the still-behind peer is CARRIED
    /// rather than billed. The state falls out of an ordinary rebase: bob reached `HEAD_B` while
    /// carol's round was in flight, so carol completed at her pinned `HEAD_A` — and a union of the
    /// reviewed SHAs then hid `HEAD_A` behind bob's `HEAD_B`, suppressing the comparison and billing
    /// carol a full round.
    #[test]
    fn a_staggered_peer_is_carried_across_an_unchanged_head_move() {
        let (mut o, dispatched) = orch(ticketless(&["alice", "bob", "carol"]));
        introduce(&o, row(12, "bob"));
        introduce(&o, row(12, "carol"));
        o.handle_review_sweep(&[open_at(12, HEAD_A)]);
        approve(&mut o, 12, "bob", HEAD_B);
        approve(&mut o, 12, "carol", HEAD_A);
        let before = dispatched.lock().expect("lock").len();

        let report = o.handle_review_sweep(&[open_at_proven(12, HEAD_B, &[HEAD_A.to_string()])]);

        assert_eq!(report.skipped, 1, "carol's verdict is carried");
        assert_eq!(report.dispatched, 0, "nobody is billed a round");
        assert_eq!(dispatched.lock().expect("lock").len(), before);
        assert_eq!(
            watch_row(&o, 12, "carol").last_reviewed_sha,
            HEAD_B,
            "carol's approval now stands at the new head"
        );
        assert_eq!(
            watch_row(&o, 12, "bob").last_reviewed_sha,
            HEAD_B,
            "bob's row was already there and is untouched"
        );
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

    // --- the drop terminal ------------------------------------------------------------------

    /// Acceptance: a merged, closed or gone pull request is dropped from the watch set — and so is
    /// one whose head this daemon is not entitled to read.
    #[test]
    fn a_retired_pull_request_leaves_the_watch_set() {
        let merged = PrLookup::Found(PrSnapshot {
            is_draft: false,
            head_sha: HEAD_A.to_string(),
            status: PrStatus::Merged,
            merged_at: None,
            head_repo: format!("{OWNER}/{REPO}"),
        });
        let closed = PrLookup::Found(PrSnapshot {
            is_draft: false,
            head_sha: HEAD_A.to_string(),
            status: PrStatus::Closed,
            merged_at: None,
            head_repo: format!("{OWNER}/{REPO}"),
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
    /// `rank_reviewers` drops it from the pinned prefix and keeps it a plain ranked candidate — a
    /// name the guard must not treat as a pin. Reading the raw config list instead evicts the
    /// incumbent for a teammate the ranking never promoted, and the round goes to whoever merely
    /// leads on load: neither the incumbent nor the pin.
    ///
    /// `bob` is loaded and `carol` is idle, so with continuity broken the load leader `carol` wins;
    /// the assertion is that the incumbent keeps the round.
    ///
    /// Mutation check: read `teams.review_required()` instead of the effective pinned set and this
    /// goes red with `carol`.
    #[test]
    fn an_unpinnable_required_reviewer_does_not_break_continuity() {
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
            "an unpinnable required reviewer must not evict the incumbent"
        );
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

    /// Each polled coordinate carries each of its rows' OWN head state, not a union of the SHA
    /// columns (STUDIO-960): two reviewers can sit at two different reviewed heads, a dispatched
    /// row's head must be kept distinct from a reviewed one, and the comparison's per-row gate
    /// needs the pairing. A never-reviewed row contributes nothing.
    #[test]
    fn the_poll_list_carries_each_rows_own_head_state() {
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
        let twelve = got
            .iter()
            .find(|w| w.pr == coord(12))
            .expect("the pull request is polled");
        let mut reviewed = twelve.reviewed_shas();
        reviewed.sort();
        assert_eq!(
            reviewed,
            vec![HEAD_A.to_string(), HEAD_B.to_string()],
            "the union across rows, de-duplicated, is what the comparison fingerprints"
        );
        assert_eq!(twelve.rows.len(), 2, "one entry per live row");
        assert!(
            twelve.rows.iter().any(|r| r.reviewed_sha == HEAD_A
                && r.requested_sha.is_empty()
                && r.status == REVIEW_STATUS_APPROVED),
            "bob's row keeps its own head pairing"
        );
        assert!(
            twelve.rows.iter().any(|r| r.reviewed_sha == HEAD_B
                && r.requested_sha.is_empty()
                && r.status == REVIEW_STATUS_REVIEWED),
            "carol's row keeps its own head pairing"
        );

        let thirteen = got
            .iter()
            .find(|w| w.pr == coord(13))
            .expect("the pull request is polled");
        assert!(
            thirteen.reviewed_shas().is_empty(),
            "a never-reviewed row contributes no reviewed SHA"
        );
        assert_eq!(thirteen.rows.len(), 1);
        assert_eq!(
            thirteen.rows[0].requested_sha, HEAD_C,
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
        async fn merge(&self, plan: crate::automerge::AutoMergePlan) {
            self.merged.lock().expect("merged lock").push(plan);
        }
        async fn finish(&self, plan: crate::reviewdone::ReviewDonePlan) {
            self.finished.lock().expect("finished lock").push(plan);
        }
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
                is_draft: false,
                head_sha: self.0.to_string(),
                status: PrStatus::Open,
                merged_at: None,
                head_repo: format!("{OWNER}/{REPO}"),
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
                is_draft: false,
                head_sha: format!("{number:040}"),
                status: PrStatus::Open,
                merged_at: None,
                head_repo: format!("{OWNER}/{REPO}"),
            }))
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
            run_one_watch_tick(watched_pr(HEAD_A, ""), Arc::new(FakeDiffSource::same())).await;
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
        let handed =
            run_one_watch_tick(watched_pr(HEAD_A, ""), Arc::new(FakeDiffSource::changed())).await;
        assert_eq!(handed.len(), 1);
        assert!(
            handed[0].unchanged_from.is_empty(),
            "a changed diff is not proof of anything"
        );
    }

    /// The comparison costs `gh` reads, so it is spent only when a head move is even possible
    /// (STUDIO-960): a head already read, a head already dispatched, and a pull request with no
    /// reviewed head at all all cost ZERO calls. The last two cases are the ones that make the GATE
    /// observable rather than the helper's own short-circuit — each has a non-empty reviewed head,
    /// so only the gate keeps the `gh` reads from being spent.
    #[tokio::test(start_paused = true)]
    async fn the_watcher_compares_only_when_a_head_move_is_possible() {
        for watched in [
            // Already read at this head: no move.
            watched_pr(HEAD_B, ""),
            // A round is already dispatched at this head, on a row that cannot consume a proof.
            watched_pr_rows(&[(HEAD_A, HEAD_B, REVIEW_STATUS_REQUESTED)]),
            // A non-terminal row behind the head: it still OWES a review of the new head, so no
            // verdict could be carried and the comparison is not worth its reads.
            watched_pr_rows(&[(HEAD_A, "", REVIEW_STATUS_IN_FLIGHT)]),
            // A terminal row whose head is the new one — reviewed there, not moved to it.
            watched_pr_rows(&[(HEAD_A, HEAD_B, REVIEW_STATUS_REVIEWED)]),
            // Nothing has ever been reviewed, so there is nothing to compare against.
            watched_pr("", ""),
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

    /// The gate is per ROW, not per union of the SHA columns (STUDIO-960, round-1 review blocker):
    /// one reviewer already at the new head must not suppress the proof for a PEER still behind it.
    /// A union of `reviewed_shas` let the peer's old head be hidden behind the other's new one, and
    /// the peer was then billed the full round this feature exists to avoid.
    #[tokio::test(start_paused = true)]
    async fn a_peer_already_at_the_new_head_does_not_suppress_the_proof() {
        let watched = watched_pr_rows(&[
            (HEAD_B, "", REVIEW_STATUS_REVIEWED),
            (HEAD_A, "", REVIEW_STATUS_APPROVED),
        ]);
        let handed = run_one_watch_tick(watched, Arc::new(FakeDiffSource::same())).await;
        assert_eq!(handed.len(), 1);
        assert_eq!(
            handed[0].unchanged_from,
            vec![HEAD_A.to_string()],
            "the peer's old head is still compared against the new one"
        );
    }

    /// The same blocker from the other side: a peer whose round is DISPATCHED at the new head has
    /// `requested_sha == head`, but that is the PEER's row — it must not excuse the still-behind
    /// row. The union gate conflated the two and suppressed the proof for the behind row.
    #[tokio::test(start_paused = true)]
    async fn a_peer_in_flight_at_the_new_head_does_not_suppress_the_proof() {
        let watched = watched_pr_rows(&[
            ("", HEAD_B, REVIEW_STATUS_REQUESTED),
            (HEAD_A, "", REVIEW_STATUS_APPROVED),
        ]);
        let handed = run_one_watch_tick(watched, Arc::new(FakeDiffSource::same())).await;
        assert_eq!(handed.len(), 1);
        assert_eq!(
            handed[0].unchanged_from,
            vec![HEAD_A.to_string()],
            "a peer's in-flight round does not hide the behind row's old head"
        );
    }

    /// A watched pull request at the fixed head [`HEAD_B`] whose SINGLE terminal row records
    /// `reviewed`/`requested` — the shape most gate tests need.
    fn watched_pr(reviewed: &str, requested: &str) -> WatchedPr {
        watched_pr_rows(&[(reviewed, requested, REVIEW_STATUS_REVIEWED)])
    }

    /// A watched pull request at the fixed head [`HEAD_B`] with one row per `(reviewed, requested,
    /// status)` triple, in the given order.
    fn watched_pr_rows(rows: &[(&str, &str, &str)]) -> WatchedPr {
        WatchedPr {
            pr: coord(12),
            rows: rows
                .iter()
                .map(|(reviewed, requested, status)| WatchedRow {
                    reviewed_sha: (*reviewed).to_string(),
                    requested_sha: (*requested).to_string(),
                    status: (*status).to_string(),
                })
                .collect(),
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
                is_draft: false,
                head_sha: head.clone(),
                status: PrStatus::Open,
                merged_at: None,
                head_repo: format!("{OWNER}/{REPO}"),
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
                is_draft: false,
                head_sha: format!("{n:040}"),
                status: PrStatus::Open,
                merged_at: None,
                head_repo: format!("{OWNER}/{REPO}"),
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
        async fn merge(&self, _plan: crate::automerge::AutoMergePlan) {}
        async fn finish(&self, _plan: crate::reviewdone::ReviewDonePlan) {}
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
                is_draft: false,
                head_sha: head,
                status: PrStatus::Open,
                merged_at: None,
                head_repo: format!("{OWNER}/{REPO}"),
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
        async fn merge(&self, _plan: crate::automerge::AutoMergePlan) {}
        async fn finish(&self, _plan: crate::reviewdone::ReviewDonePlan) {}
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
        async fn merge(&self, _plan: crate::automerge::AutoMergePlan) {}
        async fn finish(&self, _plan: crate::reviewdone::ReviewDonePlan) {}
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
        async fn merge(&self, _plan: crate::automerge::AutoMergePlan) {}
        async fn finish(&self, _plan: crate::reviewdone::ReviewDonePlan) {}
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
