//! reviewreconcile — the reconciliation sweep that REPORTS a ticket whose board state and activity
//! disagree (STUDIO-898). **No Go counterpart**: ticketless review is a Rhapsody addition end to
//! end, and so is this.
//!
//! # The class this closes, which fixing causes did not
//!
//! Six distinct defects between 2026-09-12 and 2026-09-14 presented identically — an idle board —
//! and each was fixed on its own terms: a missing tracker attachment (STUDIO-875), an attachment
//! that resolved to the wrong `sourceType` (STUDIO-882), a summons visible for only five minutes
//! (STUDIO-885), an unsatisfiable `review.reviewers` (STUDIO-891), an approving review recorded as
//! changes-requested (STUDIO-894), and a summons that was simply never applied (STUDIO-893). Fixing
//! six causes did not close the class: STUDIO-885 shipped and STUDIO-893 stalled anyway, because
//! every fix so far patches one path into the same silent state. There will be a seventh.
//!
//! The missing invariant is one sentence:
//!
//! > A ticket with an open pull request is either progressing or blocked, and the daemon can say
//! > which.
//!
//! Today those are indistinguishable. STUDIO-893 produced ZERO `skipping dispatch` lines all day —
//! it was not evaluated and rejected, it stopped being a candidate invisibly. The only reason anyone
//! noticed is that a human asked.
//!
//! # The rule is CAUSE-AGNOSTIC, and that is the entire point
//!
//! Every fix so far required knowing the mechanism in advance. This module deliberately does not
//! enumerate the six — an enumeration is exactly how the seventh stays invisible. It asks one
//! question of each watched pull request, in terms no cause appears in:
//!
//! **Each live watch row names a party who owes the next move. Has that party moved since the row
//! started owing it?**
//!
//! * `reviewed` — the reviewer posted findings, so the AUTHOR owes a run on the origin ticket.
//!   Activity is a run of that ticket started at or after the verdict landed. This is the ticket's
//!   divergence (a), and it is what catches STUDIO-875, -882, -885, -893 and -894 without knowing
//!   that any of them exist: all five end with a verdict on the pull request and no run on its
//!   ticket, however they got there.
//! * `requested` — a round is owed and the REVIEWER owes it. Activity is a reviewer run started at
//!   or after the AUTHOR's last run, which is the only arming event the ledger can date. This
//!   catches STUDIO-891, where every second review deferred permanently.
//! * `truncated` — a round ran and delivered no verdict, so it is owed again. Its own case, because
//!   here the reviewer's run is the failed attempt rather than activity; anchored on that attempt
//!   ending, since any genuine retry moves the status off `truncated`.
//! * `approved` on a still-open pull request — divergence (b). Nobody owes a RUN; the merge gate
//!   owes a merge. This is what found the STUDIO-881 draft loop and the `BEHIND` decline by hand.
//! * `in_flight` — a round is happening RIGHT NOW. Never divergence.
//!
//! # A human-gated ticket is deliberately NOT this sweep's business (STUDIO-949)
//!
//! `rhapsody:human` is the one hold the dispatcher applies on purpose: the ticket can only be done by
//! a person, so a held ticket sitting in Todo is working as intended, not stalled. The sweep must
//! never report it as a stall, and it cannot rely on such a ticket having no watch row: a ticket
//! labelled AFTER an agent already ran has one, which is the likeliest way the label is ever applied.
//! So a row whose origin ticket currently WEARS the label is dropped before the rules see it (see
//! [`Orchestrator::reconcile_review_divergence`]) — the exclusion is explicit, not by construction,
//! and it reads the live-inclusive current-label set so a label added while the origin run is still
//! live is honoured too. The dispatch refusal itself lives on the review watcher
//! ([`crate::reviewwatch`]), so a row that is held arms nothing new either; this filter is what keeps
//! the ALREADY-ARMED row from reporting the deliberate hold as a stalled obligation. A future change
//! that made this sweep range over tickets rather than watch rows would have to add the hold back
//! deliberately.
//!
//! # It reports and it does NOT act
//!
//! Nothing here re-dispatches, re-arms, merges or moves a ticket. That is a decision, not an
//! omission: re-dispatching on a rule nobody has watched fire is how a stall becomes a loop, and
//! the sweep's value does not depend on it — every one of the six cost hours only because nobody
//! could SEE it. Acting can be its own reviewed change once these lines have been watched.
//!
//! # Where it reports, and why the log alone is not enough
//!
//! `linked_prs_total=0` sat in the log for eleven hours on STUDIO-875 and cost eleven hours anyway.
//! So this follows [`crate::preflight::CREDENTIAL_DEAD_WARNING`] and
//! [`crate::reviewwatch::REVIEW_UNASSIGNABLE_WARNING`]: a per-project advisory on
//! `/api/v1/projects` ([`REVIEW_DIVERGENCE_WARNING`]) **and** — because a warning string cannot say
//! WHICH ticket — the divergences themselves on `/api/v1/state`, emitted only when there are any,
//! exactly as STUDIO-880's `drain` key is (see [`crate::snapshot_json::render`]). That conditional is
//! load-bearing: `/api/v1/state` is byte-pinned to the Go golden, so a key present on a healthy
//! daemon would be parity drift.
//!
//! # How it avoids becoming noise
//!
//! A permanent warning nobody reads is the failure mode this exists to prevent — the trap
//! STUDIO-894's review caught in STUDIO-881's first attempt, where a removed WARN came back as an
//! equally repetitive INFO. Three things keep it quiet:
//!
//! * [`RECONCILE_STALE_AFTER`] is a THRESHOLD, not a tick: a pull request mid-round is silent.
//! * transitions log loudly and once; the steady state repeats at [`RECONCILE_LOG_EVERY`] sweeps.
//! * an UNKNOWN is never a divergence. A row the `runs` ledger cannot date — no completed reviewer
//!   run, an origin naming no ticket, a store that was pruned out from under it — is reported as
//!   nothing at all. Under-reporting a case nobody can act on costs an operator nothing; crying wolf
//!   costs them the whole signal, and then the seventh variant is invisible again.

use std::collections::HashMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
use rhapsody_store::{
    REVIEW_STATUS_APPROVED, REVIEW_STATUS_DROPPED, REVIEW_STATUS_IN_FLIGHT,
    REVIEW_STATUS_REQUESTED, REVIEW_STATUS_REVIEWED, REVIEW_STATUS_TRUNCATED, RunFilter,
};

use crate::orchestrator::Orchestrator;
use crate::prstate::PrCoord;
use crate::review::review_key;
use crate::reviewdone::origin_ticket;
use crate::reviewwatch::{CAPACITY_HOLD_TTL, CapacityHold, UNREADABLE_ATTEMPTS_TO_DROP_HOLD};

/// How long a party may owe the next move before the sweep calls the pull request diverged.
///
/// **A threshold, not a tick** — the distinction the whole design rests on. A pull request mid-round
/// is not stalled, and a sweep that said so on the first quiet minute would be the permanent
/// unread warning this module exists to avoid.
///
/// Ninety minutes, chosen against what was actually measured on the operator's own store
/// (`~/.rhapsody/rhapsody.db`, n=197 completed runs on 2026-09-14): p50 7.3 min, p90 26.3 min,
/// longest ever 61.1 min. So the window is ~1.5x the longest run this daemon has EVER taken and
/// ~3.4x its p90 — a slow-but-healthy round cannot reach it, including one that waited behind the
/// 4-slot `max_concurrent_agents` for a dispatch turn. And it is far below what the incidents
/// actually cost: six hours on STUDIO-893, eleven on STUDIO-875. The window is the gap between "no
/// human would notice yet" and "a human noticed and asked", and there is more than an hour of room
/// between those two.
pub const RECONCILE_STALE_AFTER: Duration = Duration::from_secs(90 * 60);

/// While a pull request stays diverged, the steady-state line is logged once per this many sweeps.
/// The transitions — crossing the threshold, and recovering — always log regardless, mirroring
/// [`crate::preflight`], [`crate::drain`] and [`crate::reviewwatch::REVIEW_UNASSIGNABLE_LOG_EVERY`].
///
/// In sweeps rather than a `Duration` for that last one's reason: the counter is already in sweeps
/// and a clock here would be a second unit to keep honest. A sweep is one control-loop tick, so at
/// the default 30s `polling.interval_ms` sixty of them is ~30 minutes.
pub const RECONCILE_LOG_EVERY: usize = 60;

/// The operator advisory surfaced on each project's `/api/v1/projects` status while any watched pull
/// request is diverged — the state-visible half, exactly as
/// [`crate::preflight::CREDENTIAL_DEAD_WARNING`] and
/// [`crate::reviewwatch::REVIEW_UNASSIGNABLE_WARNING`] are. It names the surface that says WHICH,
/// because a fixed string cannot: a warning that an operator cannot act on is the noise this module
/// is careful about everywhere else.
pub const REVIEW_DIVERGENCE_WARNING: &str = "a pull request's board state and its activity disagree — nothing is progressing it and \
     nothing has reported it blocked; see `review_divergence` on /api/v1/state";

/// [`REVIEW_DIVERGENCE_WARNING`]'s sibling for a divergence the review watcher is HOLDING for want
/// of a global slot (STUDIO-950). The plain string's "nothing has reported it blocked" is false
/// there — the watcher reports the hold every tick — so the project advisory names the deliberate
/// wait instead, and still points at the surface that says WHICH pull request and with what holder
/// count. It is pushed alongside [`REVIEW_DIVERGENCE_WARNING`], never instead of it: a reported set
/// can hold both a deliberately-waited round and a genuinely unexplained one, and the two
/// conditions are independent (see `snapshot.rs`'s `project_statuses`).
pub const REVIEW_DIVERGENCE_CAPACITY_WARNING: &str = "a pull request's board state and its activity disagree because it is held \
     for capacity — no reviewer run can start yet; see `review_divergence` on /api/v1/state";

/// [`REVIEW_DIVERGENCE_WARNING`]'s sibling for a divergence whose pull request GitHub has stopped
/// answering for (STUDIO-950 round 18). The watcher reports the coordinate as unreadable every tick,
/// so "nothing has reported it blocked" is false here too; the advisory names the silence instead and
/// still points at the surface that says WHICH pull request. Pushed alongside the other two, never
/// instead of them: a reported set can hold an unreadable coordinate, a capacity-held round and a
/// genuinely unexplained stall at once, and each string is pushed on its own evidence.
pub const REVIEW_DIVERGENCE_UNREADABLE_WARNING: &str = "a pull request's board state and its activity disagree and its GitHub state \
     could not be read — the daemon cannot confirm it is progressing; see `review_divergence` on \
     /api/v1/state";

/// Which way a pull request's intent and its activity disagree. Three shapes, not six causes — see
/// the module docs on why an enumeration of causes would defeat the purpose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DivergenceKind {
    /// Divergence (a): the newest verdict asked for changes, and the origin ticket has had no run
    /// since it landed. STUDIO-875, -882, -885, -893 and -894 all end here.
    ChangesRequestedNoRun,
    /// Divergence (a), one step earlier: a round is owed and no reviewer run has started since the
    /// row was armed. STUDIO-891 ends here.
    ReviewRequestedNoRun,
    /// The author's newest run after the verdict stopped at its per-run token ceiling (STUDIO-967).
    ///
    /// Without this kind the row's own rule reads the ceiling-stopped run as the author having
    /// MOVED — it started after the verdict landed — and reports nothing, so a ticket that keeps
    /// hitting its ceiling looks like a healthy author quietly working. That is the silent stall the
    /// ticket forbids: the run made no progress, and the reason is a bound this daemon itself
    /// applied, so the sweep can and must name it. Reported at the ordinary staleness threshold
    /// because a later run may still resume the ticket.
    AuthorTokenCeilingStopped,
    /// A ticketless REVIEW round stopped at its per-run token ceiling (STUDIO-967): the run was
    /// killed mid-turn, so it delivered no verdict and the head is still owed a review.
    ///
    /// Its own kind rather than borrowing [`DivergenceKind::ReviewRequestedNoRun`], whose sentence
    /// ("no reviewer run has started") is false here — a run started, and this daemon stopped it.
    /// The `truncated` row alone would report the generic wording, and the run's `token_ceiling`
    /// outcome is the fact that names the cause. The review half is where much of the spend lives
    /// (the incident that motivated this bound was all reviews), so a resume that left it silent
    /// would close the author's hole and leave the review's open.
    ReviewTokenCeilingStopped,
    /// Divergence (b): every required reviewer approved the current head and the pull request is
    /// still open, with `review.auto_merge` on. STUDIO-881's draft loop and the `BEHIND` decline.
    ApprovedStillOpen,
    /// Not a divergence of intent and activity but of a BOUND: the pull request's REVIEW round
    /// budget ([`crate::reviewwatch::REVIEW_ROUNDS_PER_PR_CAP`] × reviewers) is spent, so no
    /// further review round will be dispatched and nothing will resume on its own (STUDIO-956).
    ///
    /// **What it does NOT claim, and why (round-8 finding 3).** It used to say "no further review
    /// or AUTHOR re-run will be dispatched". That is false in exactly the case where it prints
    /// most: this rule is reached only when there is no manager decision, and on an installation
    /// that sets no `review.adjudicate_after_rounds` the author half is deliberately unbounded —
    /// so the author can and does keep running. (It is also false with a threshold ABOVE the legacy
    /// cap, where the cap stops the review half before the threshold is anywhere near.) The fix is
    /// the wording, NOT a gate on the threshold being set: gating it would restore the silent stop
    /// on the default install, which is the incident that filed this ticket — three pull requests
    /// sat unreviewable on 2026-09-20 because the cap stopped the review half at DEBUG and no
    /// surface said so.
    ///
    /// It is a `DivergenceKind` and not a separate channel because it is exactly what this module
    /// exists to make visible: a pull request that has stopped progressing and is not blocked by
    /// anything a later tick will clear. Reported the moment the budget is spent — the staleness
    /// threshold [`RECONCILE_STALE_AFTER`] is for obligations that might yet resolve, and a spent
    /// budget never does — and cleared, with the recovery line, the moment an operator clears the
    /// budget or the pull request leaves the watch set.
    RoundBudgetExhausted,
    /// A BOUND of the opt-in kind: the manager reached its configured round threshold and decided
    /// that a human is needed (STUDIO-956). Not a stall — it is a decision, and the line names the
    /// specific open findings and the head the loop stopped at.
    ///
    /// A separate kind from [`DivergenceKind::RoundBudgetExhausted`] because the two are opposite
    /// outcomes of the same bound: that one is "the legacy cap stopped the loop and nothing
    /// decided", this one is "the manager decided, and it says a human is needed".
    ReviewEscalated,
    /// The manager SHIPPED the loop and the pull request still cannot merge on its own (STUDIO-956):
    /// no further review or author round will ever arm, and the merge gate is still holding it —
    /// typically because a row still records findings rather than an approval at the head.
    ///
    /// A `ship` is not a stall the manager owns; it is a decision the MERGE GATES now own. But a
    /// decision the daemon acts on must not be a decision the daemon hides: without this kind, a
    /// shipped pull request that cannot merge is reported nowhere at all — strictly worse than the
    /// silent stop the ticket replaced. Reported as soon as the gate is known to be stuck (an
    /// unapproved live row), because that will never clear by itself; a shipped pull request whose
    /// rows are all approved is not reported here, since auto-merge either merges it or
    /// [`DivergenceKind::ApprovedStillOpen`] reports the stuck gate after the staleness threshold.
    ReviewShipped,
}

impl DivergenceKind {
    /// The stable wire/log token. Stable because an operator greps it and the console switches on
    /// it; renaming one is a breaking change to both.
    pub fn as_str(self) -> &'static str {
        match self {
            DivergenceKind::ChangesRequestedNoRun => "changes_requested_no_run",
            DivergenceKind::ReviewRequestedNoRun => "review_requested_no_run",
            DivergenceKind::AuthorTokenCeilingStopped => "author_token_ceiling_stopped",
            DivergenceKind::ReviewTokenCeilingStopped => "review_token_ceiling_stopped",
            DivergenceKind::ApprovedStillOpen => "approved_still_open",
            DivergenceKind::RoundBudgetExhausted => "round_budget_exhausted",
            DivergenceKind::ReviewEscalated => "review_escalated",
            DivergenceKind::ReviewShipped => "review_shipped",
        }
    }
    /// The operator-facing sentence: what was expected to happen, and what did not. Phrased as an
    /// observation rather than a diagnosis — the sweep genuinely does not know the cause, and
    /// guessing one in the message is how an operator is sent down the wrong path.
    pub fn detail(self) -> &'static str {
        match self {
            DivergenceKind::ChangesRequestedNoRun => {
                "a reviewer asked for changes and the ticket has had no run since"
            }
            DivergenceKind::ReviewRequestedNoRun => {
                "a review round is owed and no reviewer run has started"
            }
            DivergenceKind::AuthorTokenCeilingStopped => {
                "the author's newest run stopped at its per-run token ceiling, so it made no \
                 progress and the ticket is still owed a run"
            }
            DivergenceKind::ReviewTokenCeilingStopped => {
                "the review round was stopped at its per-run token ceiling before it delivered a \
                 verdict, so the head is still owed a review"
            }
            DivergenceKind::ApprovedStillOpen => {
                "every required reviewer approved and the pull request is still open"
            }
            DivergenceKind::RoundBudgetExhausted => {
                "the per-pull-request review round budget is spent, so no further review round \
                 will be dispatched until it is cleared"
            }
            DivergenceKind::ReviewEscalated => {
                "the manager adjudicated the review loop and escalated it: the open findings need \
                 a human"
            }
            DivergenceKind::ReviewShipped => {
                "the manager shipped the review loop and no further review or author round will be \
                 dispatched; the merge gate still holds the pull request"
            }
        }
    }
}

/// One reported divergence: a pull request, the way its intent and activity disagree, and how long
/// they have. Carries no cause, because the sweep does not know one and must not invent one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Divergence {
    /// `owner/repo#number`, the form the logs, the room and the design record all use.
    pub pr: String,
    /// How they disagree.
    pub kind: DivergenceKind,
    /// The origin ticket, or `""` when the origin names none (a `console:` row). Empty is never a
    /// [`DivergenceKind::ChangesRequestedNoRun`] — that rule needs a ticket to have had no run.
    pub ticket: String,
    /// The row whose obligation is outstanding, or `""` for [`DivergenceKind::ApprovedStillOpen`],
    /// which is a property of EVERY row rather than of one.
    pub reviewer: String,
    /// Seconds since the party owing the next move started owing it. For every staleness-rule kind
    /// it is greater than [`RECONCILE_STALE_AFTER`] — it IS the staleness the threshold was crossed
    /// by. [`DivergenceKind::RoundBudgetExhausted`] is the one exception: it has no threshold and is
    /// reported as soon as the budget is spent, so its value is seconds since the newest activity
    /// the sweep can date (and `0` when it can date none) rather than a crossed bound.
    pub stale_secs: i64,
    /// What [`crate::runautomerge::AutoMergeLedger`] has most recently SAID about this pull request,
    /// `None` unless [`DivergenceKind::ApprovedStillOpen`] and the ledger holds an entry for it
    /// (STUDIO-923). Not a cause the sweep worked out — [`reconcile_pr`] never sets this, it is
    /// filled in afterward by [`Orchestrator::reconcile_review_divergence`] from a sibling module's
    /// own already-decided report — so it does not weaken the doc above: the sweep still invents
    /// nothing, it just repeats a fact this process already has. Read by
    /// [`Orchestrator::set_review_divergences`] to enrich the WARN line; deliberately NOT rendered
    /// onto `/api/v1/state` (`snapshot_json::render` enumerates fields explicitly and this is not
    /// among them) — the ticket's ask is the human-facing log report, not a wire-shape change.
    pub auto_merge_reason: Option<&'static str>,
    /// The capacity hold the review watcher recorded for THIS row's round, when one is fresh
    /// (STUDIO-950). Like `auto_merge_reason` it is a fact this process already has, copied from
    /// [`Orchestrator::review_capacity_held`] — the sweep invents nothing. It ANNOTATES: the row is
    /// still reported, and [`Orchestrator::set_review_divergences`] names the hold and its holder
    /// count instead of claiming nothing has reported it blocked. Unlike `auto_merge_reason` it IS
    /// rendered onto `/api/v1/state`, as a conditional `capacity_held` object on the (already
    /// Rhapsody-only, already conditional) divergence row — the console banner and the project
    /// advisory both need it to say the wait is deliberate, and it cannot be folded into the fixed
    /// advisory string. Absent when there is no hold, so the healthy payload is untouched.
    pub capacity_held: Option<CapacityHold>,
    /// How many CONSECUTIVE `gh` lookups of this pull request's coordinate had failed when the sweep
    /// ran, once that count has reached [`UNREADABLE_ATTEMPTS_TO_DROP_HOLD`] (STUDIO-950 round 18).
    /// `None` below the threshold — the annotation is absent for a coordinate the watcher is still
    /// answering for — and it is mutually exclusive with `capacity_held`, because the count reaching
    /// the threshold is exactly what makes [`Orchestrator::fresh_capacity_hold`] deny the hold.
    ///
    /// It exists so the sweep can say WHY a row is not being reported as a capacity hold — GitHub
    /// stopped answering for this coordinate, which is a fact the daemon knows and previously
    /// declined to state, falling through instead to the false "nothing has reported it blocked".
    /// The common case is a hold it still has in memory whose freshness is denied for that reason,
    /// but the annotation is filled from the per-coordinate count unconditionally (not gated on a
    /// hold existing), so a row that never held a slot — including an approved-and-open pull request
    /// awaiting a merge — reports the silence too (STUDIO-950 round 20). The sweep's own local
    /// determination is unaffected: it still reports the divergence under its ordinary kind, this
    /// only states a fact the daemon already has — GitHub was refusing the coordinate.
    pub capacity_unreadable: Option<u32>,
    /// The head the manager stopped at — only meaningful for [`DivergenceKind::ReviewEscalated`],
    /// `""` otherwise. Read by [`Orchestrator::set_review_divergences`] to name where the loop
    /// stopped. Rendered onto `/api/v1/state` ONLY when the escalation is SUPERSEDED, so a
    /// still-current escalation keeps the row shape it had before STUDIO-1005.
    pub adjudicated_head: String,
    /// The head the review watcher most recently OBSERVED for this pull request, or `""` when no
    /// observation is available (STUDIO-1005). Filled in from
    /// [`Orchestrator::review_observed_head`] for a [`DivergenceKind::ReviewEscalated`] row; `""`
    /// for every other kind. When it differs from [`Divergence::adjudicated_head`] the escalation is
    /// SUPERSEDED — its reason was computed against a head the branch no longer carries — and the
    /// operator must be told so. See [`Divergence::superseded`] and [`Divergence::supersession`].
    pub current_head: String,
    /// How many review↔author rounds the loop ran before the escalation — `0` for every kind but
    /// [`DivergenceKind::ReviewEscalated`]. Rendered onto the escalation log line, not the wire.
    pub rounds: usize,
    /// The open findings the manager escalated on — empty for every kind but
    /// [`DivergenceKind::ReviewEscalated`]. Named on the log line so the escalation is actionable.
    pub findings: Vec<String>,
    /// The manager's OWN words for an escalation — empty for every kind but
    /// [`DivergenceKind::ReviewEscalated`]. Read by [`Orchestrator::set_review_divergences`] so the
    /// WARN carries the reason rather than only the room post and the pull-request comment. Rendered
    /// onto `/api/v1/state` only when the escalation is SUPERSEDED (STUDIO-1005): a superseded row
    /// shows the manager's reason beside the supersession notice, so the operator can see the text
    /// they must not act on as current fact.
    pub reason: String,
}

impl Divergence {
    /// Whether this escalation's reason is no longer known to describe the current head
    /// (STUDIO-1005).
    ///
    /// True only for a [`DivergenceKind::ReviewEscalated`] whose recorded
    /// [`Divergence::adjudicated_head`] is non-empty and differs from an OBSERVED
    /// [`Divergence::current_head`]. A missing observation (`current_head` empty) is "unknown", not
    /// "stale": the ticket forbids claiming staleness the daemon cannot stand behind, and this is the
    /// direction that renders exactly as before the feature existed. A moved head is a signal that
    /// the text MAY be stale, never a verdict that the findings were addressed — a merge, a
    /// CHANGELOG bump or a force-push all move the head without touching a finding.
    pub fn superseded(&self) -> bool {
        self.kind == DivergenceKind::ReviewEscalated
            && !self.adjudicated_head.is_empty()
            && !self.current_head.is_empty()
            && self.current_head != self.adjudicated_head
    }

    /// The operator-facing supersession sentence, or `None` when the escalation is not superseded.
    ///
    /// Deliberately states only what the daemon knows: the head the reason was computed at and the
    /// head the branch now carries. It does NOT claim the findings were fixed — the signal is "this
    /// text may be stale", not "this text is wrong" — and it does not count commits, because a local
    /// string comparison cannot honestly answer that without a `git`/`gh` call the sweep may not make.
    pub fn supersession(&self) -> Option<String> {
        self.superseded().then(|| {
            format!(
                "This escalation was computed at head `{}`; the branch has since moved to `{}`, so \
                 these findings may already be addressed — treat the reason below as evidence, not a \
                 verdict.",
                self.adjudicated_head, self.current_head
            )
        })
    }
}

/// When one run started, and whether it has finished. The only two facts about a `runs` row the
/// rules need, so the rules can be driven by a test without a store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunMoment {
    pub started_at: DateTime<Utc>,
    /// `None` while the run is still in flight.
    pub ended_at: Option<DateTime<Utc>>,
    /// The run's stored `runs.outcome`. The rules use one value from it: a ticket run stopped at its
    /// per-run token ceiling ([`rhapsody_store::OUTCOME_TOKEN_CEILING`]) did NOT make progress, so a
    /// `reviewed` row must report the ceiling rather than counting that run as the author moving.
    pub outcome: String,
}

impl RunMoment {
    /// Whether this run is still going. A party with a live run has moved, whenever it started —
    /// "a ticket mid-round is not reported" is unconditional, and a conditional version of it would
    /// report a ticket an agent is working on right now.
    fn in_flight(&self) -> bool {
        self.ended_at.is_none()
    }

    /// The latest instant this run proves activity: its end, or its start while it is still live.
    fn last_at(&self) -> DateTime<Utc> {
        self.ended_at.unwrap_or(self.started_at)
    }
}

/// One live watch row, plus the newest `runs` row for each of the two identifiers that can discharge
/// its obligation. Built by [`Orchestrator::reconcile_review_divergence`]; a plain input struct so
/// every rule below is a pure function over data a test can write by hand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RowFacts {
    pub reviewer: String,
    /// One of the `REVIEW_STATUS_*` values.
    pub status: String,
    /// The row's `origin_ticket`, or `""` when its origin names none.
    pub ticket: String,
    pub open: bool,
    /// The capacity hold the review watcher recorded for THIS row's round when it deferred it for
    /// want of a global slot (STUDIO-950), `None` when there is none or the recorded hold has gone
    /// stale. It does not suppress the row — the divergence is still reported — it is copied onto
    /// it so [`Orchestrator::set_review_divergences`] can name the hold and its holder count rather
    /// than claim nothing has reported it blocked.
    pub capacity_held: Option<CapacityHold>,
    /// The consecutive failed `gh` lookups of this pull request's coordinate, once they have reached
    /// [`UNREADABLE_ATTEMPTS_TO_DROP_HOLD`]; `None` below the threshold. Copied onto the divergence
    /// so [`Orchestrator::set_review_divergences`] can name GitHub's silence rather than falling
    /// through to the plain wording when `capacity_held` is denied for it (STUDIO-950 round 18).
    pub capacity_unreadable: Option<u32>,
    /// The newest run of `review_key(pr, reviewer)` — this row's own review round.
    pub reviewer_run: Option<RunMoment>,
    /// The newest run of `ticket`.
    pub ticket_run: Option<RunMoment>,
}

/// Everything known about ONE pull request this sweep. Grouped per pull request rather than per row
/// because divergence (b) is a property of every row at once: one reviewer's approval is not the
/// gate clearing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PrFacts {
    /// `owner/repo#number`.
    pub pr: String,
    pub rows: Vec<RowFacts>,
    /// The effective `teams.review.auto_merge` FOR THIS PULL REQUEST'S PROJECT (STUDIO-927) — the
    /// per-project override when the owning project has one, else the installation-wide default.
    /// It gates divergence (b) and nothing else, and the
    /// gate is not a special case: with auto-merge OFF, an approved-and-open pull request is waiting
    /// for a HUMAN by design, so there is no intent for its activity to diverge from. Reporting it
    /// would light the warning on every healthy review on such a board — the exact crying-wolf
    /// failure the acceptance forbids.
    pub auto_merge: bool,
}

/// The whole rule, as one pure function over one pull request: at most one divergence, or `None`.
///
/// At most ONE because the report is read by an operator deciding whether to intervene, and three
/// lines about the same pull request is three times the noise for one decision. When several rows
/// diverge the STALEST wins — it is the one that has been waiting longest and the one whose cause is
/// furthest upstream.
///
/// `stale_after` is a parameter rather than [`RECONCILE_STALE_AFTER`] read directly so a test can
/// drive the threshold in seconds instead of sleeping for ninety minutes.
pub(crate) fn reconcile_pr(
    facts: &PrFacts,
    now: DateTime<Utc>,
    stale_after: Duration,
) -> Option<Divergence> {
    // A row that has left the watch set says nothing about a live obligation.
    let live: Vec<&RowFacts> = facts
        .rows
        .iter()
        .filter(|r| r.open && r.status != REVIEW_STATUS_DROPPED)
        .collect();
    if live.is_empty() {
        return None;
    }
    // A round in flight is a conversation mid-sentence, whatever the other rows say. Checked across
    // the whole pull request rather than per row because a second reviewer reading the same head is
    // activity ON THIS PULL REQUEST, and reporting it while an agent is inside it is precisely the
    // "mid-round is not stalled" case.
    if live.iter().any(|r| r.status == REVIEW_STATUS_IN_FLIGHT) {
        return None;
    }

    // Divergence (b), first because it is a property of every row and would otherwise be masked by
    // the per-row pass returning `None` for each approved row individually.
    if facts.auto_merge && live.iter().all(|r| r.status == REVIEW_STATUS_APPROVED) {
        // The instant the gate finally cleared: the LAST of the approving runs, not the first. An
        // earlier reviewer's approval was not the gate clearing, and anchoring on it would report a
        // pull request whose final approval landed a minute ago.
        let anchor = live
            .iter()
            .filter_map(|r| r.reviewer_run.as_ref().map(RunMoment::last_at))
            .max()?;
        if let Some(stale_secs) = stale_secs(now, anchor, stale_after) {
            return Some(Divergence {
                pr: facts.pr.clone(),
                kind: DivergenceKind::ApprovedStillOpen,
                // Any row's ticket: they are rows of ONE pull request, so they share an origin.
                ticket: live
                    .iter()
                    .find(|r| !r.ticket.is_empty())
                    .map(|r| r.ticket.clone())
                    .unwrap_or_default(),
                reviewer: String::new(),
                stale_secs,
                // Filled in by the caller ([`Orchestrator::reconcile_review_divergence`]), which
                // has the ledger this pure function deliberately does not.
                auto_merge_reason: None,
                // A capacity hold defers a ROUND; it has nothing to say about an approved-and-open
                // pull request, whose next move is a merge.
                capacity_held: None,
                // The unreadability annotation is NOT a hold and does not transfer that reasoning:
                // "GitHub stopped answering for this coordinate" is exactly as true, and more
                // alarming, for a pull request whose next move is a merge. Filled in by the caller
                // (which has the per-coordinate count [`reconcile_pr`] deliberately does not).
                capacity_unreadable: None,
                adjudicated_head: String::new(),
                current_head: String::new(),
                rounds: 0,
                findings: Vec::new(),
                reason: String::new(),
            });
        }
        return None;
    }

    live.iter()
        .filter_map(|row| row_divergence(&facts.pr, row, now, stale_after))
        .max_by_key(|d| d.stale_secs)
}

/// Divergence (a), for one row: the party this row's status puts the next move on, and whether they
/// have moved since it landed on them.
fn row_divergence(
    pr: &str,
    row: &RowFacts,
    now: DateTime<Utc>,
    stale_after: Duration,
) -> Option<Divergence> {
    let (kind, anchor) = row_owed(row)?;
    Some(Divergence {
        pr: pr.to_string(),
        kind,
        ticket: row.ticket.clone(),
        reviewer: row.reviewer.clone(),
        stale_secs: stale_secs(now, anchor, stale_after)?,
        auto_merge_reason: None,
        // Copied onto every row kind `row_owed` reports (STUDIO-950). The three per-arm
        // constructions this replaces each carried them; STUDIO-959 factored those into this one
        // site, and the fields are a property of the ROW, not of the kind, so one fill serves all
        // three arms.
        capacity_held: row.capacity_held,
        capacity_unreadable: row.capacity_unreadable,
        adjudicated_head: String::new(),
        current_head: String::new(),
        rounds: 0,
        findings: Vec::new(),
        reason: String::new(),
    })
}

/// The instant this row's obligation started, and which divergence it is — `None` when the row owes
/// nothing RIGHT NOW.
///
/// Factored out of [`row_divergence`] so [`round_budget_owed`] can ask the same question without
/// re-deriving the discharge rules (the two must never disagree about whether a row still owes a
/// round), and without a staleness threshold: a spent budget is reported the moment it is spent, not
/// after [`RECONCILE_STALE_AFTER`]. Every `None` here is answered by `row_divergence` as "no
/// divergence", so the table of discharge rules lives in exactly one place.
fn row_owed(row: &RowFacts) -> Option<(DivergenceKind, DateTime<Utc>)> {
    match row.status.as_str() {
        // The reviewer posted findings, so the AUTHOR owes a run on the origin ticket.
        REVIEW_STATUS_REVIEWED => {
            // The verdict's own run dates it. `ended_at` and not `started_at`: a verdict lands when
            // the reviewer FINISHES, and dating it from the start would spend the reviewer's own
            // runtime out of the author's window.
            //
            // `?` on both: a row with no completed reviewer run cannot be dated at all, and an
            // origin naming no ticket has no run that could discharge it. Neither is a divergence —
            // unknown is never reported (module docs).
            let anchor = row.reviewer_run.as_ref()?.ended_at?;
            if row.ticket.is_empty() {
                return None;
            }
            if let Some(t) = &row.ticket_run
                && (t.in_flight() || t.started_at >= anchor)
            {
                // The author moved — UNLESS the run that moved was stopped at its per-run token
                // ceiling (STUDIO-967). Such a run ended mid-turn without finishing anything, so
                // counting it as progress is exactly how a ceiling-stopped ticket reads as a healthy
                // author; name the ceiling instead. A LATER run (of any outcome) supersedes this one
                // as `ticket_run` and clears the report, which is correct: the ticket resumed.
                if t.outcome == rhapsody_store::OUTCOME_TOKEN_CEILING {
                    return Some((DivergenceKind::AuthorTokenCeilingStopped, anchor));
                }
                return None; // the author moved, or is moving
            }
            Some((DivergenceKind::ChangesRequestedNoRun, anchor))
        }
        // A round ENDED without the agent ever declaring it had finished, so the round is owed
        // AGAIN. Its own arm rather than sharing `requested`'s below, because the reviewer's run is
        // not activity here — it is the failed attempt, and treating it as activity is how a review
        // that never happened reads as one that did. Anchored on that attempt ending; any genuine
        // retry moves the status off `truncated`, so a row still here has not had one.
        REVIEW_STATUS_TRUNCATED => {
            let attempt = row.reviewer_run.as_ref()?;
            if attempt.in_flight() {
                return None; // the retry is running
            }
            // STUDIO-967: a round stopped at its per-run token ceiling ended WITHOUT reading the
            // head, and the daemon knows it did. The plain `ReviewRequestedNoRun` sentence ("no
            // reviewer run has started") is false about it — one did start, and this daemon killed
            // it — so name the ceiling. A genuine retry moves the status off `truncated`, which is
            // why a row still here is still owed.
            if attempt.outcome == rhapsody_store::OUTCOME_TOKEN_CEILING {
                return Some((DivergenceKind::ReviewTokenCeilingStopped, attempt.last_at()));
            }
            Some((DivergenceKind::ReviewRequestedNoRun, attempt.last_at()))
        }
        // A round is owed and the REVIEWER owes it, and no run for this head has been recorded yet.
        REVIEW_STATUS_REQUESTED => {
            if let Some(m) = &row.reviewer_run
                && m.in_flight()
            {
                return None; // a round is running under a status that has not caught up yet
            }
            // The row was armed by the AUTHOR: their handoff introduced it, or their pushed fixes
            // re-armed it, and a push follows their run. Deliberately NOT the reviewer's own
            // previous round ending, even though that is also "the latest thing that happened here":
            // anchoring on it makes the reviewer late by construction — their run always STARTS
            // before it ENDS — so every row that had ever been reviewed would be reported forever.
            //
            // The consequence is the conservative one. A row re-armed by a push AFTER the reviewer's
            // last round has no datable arming event of its own, so the reviewer having acted more
            // recently than any author activity we can see reads as silence. Under-reporting there
            // beats reporting every healthy re-review.
            let author = row.ticket_run.as_ref()?;
            if author.in_flight() {
                return None; // the head is still moving; a round would be re-armed anyway
            }
            let anchor = author.last_at();
            if let Some(m) = &row.reviewer_run
                && m.started_at >= anchor
            {
                return None; // the reviewer moved after the row was armed
            }
            Some((DivergenceKind::ReviewRequestedNoRun, anchor))
        }
        // `approved` is decided above (and, with auto-merge off, is a healthy wait for a human);
        // `in_flight` and `dropped` are filtered out by the caller. Spelled as a catch-all rather
        // than three arms returning `None`, because a status added later must default to SILENT:
        // reporting a state nobody has thought about yet is how this becomes noise.
        _ => None,
    }
}

/// How stale the obligation is, or `None` when it has not crossed `stale_after` yet.
///
/// A negative elapsed — an anchor in the future, which a clock adjustment or a hand-written
/// timestamp can produce — fails the comparison and reports nothing, which is the right answer for
/// a measurement that cannot be trusted.
fn stale_secs(now: DateTime<Utc>, anchor: DateTime<Utc>, stale_after: Duration) -> Option<i64> {
    let elapsed = now.signed_duration_since(anchor).num_seconds();
    let threshold = i64::try_from(stale_after.as_secs()).unwrap_or(i64::MAX);
    (elapsed > threshold).then_some(elapsed)
}

/// Whether two capacity annotations say the same thing: the held-or-not fact, and the holder count
/// and budget that name it. [`CapacityHold::recorded`] is deliberately EXCLUDED — the watcher
/// re-stamps it whenever the rotating cursor next evaluates the pull request, which is not a change
/// in the held capacity, so comparing it would log each rotation as a transition and
/// defeat the reconciliation log's rate limit. A change to the holder count or the budget IS a
/// transition worth logging: the operator tuning the key needs the new number.
fn same_capacity(a: Option<CapacityHold>, b: Option<CapacityHold>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => x.holders == y.holders && x.separate == y.separate,
        _ => false,
    }
}

/// Whether any LIVE row of a pull request still OWES a round — a review, or the author's run a
/// verdict bought (STUDIO-956).
///
/// Scoped to these three statuses on purpose. An `approved` pull request is waiting on the merge
/// gate rather than on this budget, and reporting it here would cry wolf on every healthy approval.
/// A row that has left the watch set says nothing at all.
///
/// A round still `in_flight` silences the whole pull request, matching [`reconcile_pr`]: a second
/// reviewer reading the same head is activity ON THIS PULL REQUEST, and a spent budget beside a
/// running agent is not a stall — the report would be premature and would flicker off when that
/// round finished. That is a mixed-row case (one reviewer mid-round, another's findings outstanding)
/// and it is exactly why this cannot be a per-row predicate.
///
/// Whether a row owes a round is [`row_owed`]'s question, asked UNCONDITIONALLY and through the very
/// same table `reconcile_pr` uses, so the two can never disagree about a row. That matters most for
/// the author's own run, which the first cut of this predicate ignored: the budget is charged at
/// DISPATCH, so the summoned author run that spent it is `in_flight` for its whole duration, and a
/// per-status predicate reported `round_budget_exhausted` over a running agent — premature by an
/// entire agent run. A `ticket_run` that started at or after the reviewer's verdict discharges the
/// row the same way `reconcile_pr`'s `reviewed` arm says it does, so an author who answered and
/// deliberately held without pushing is not a stall either.
fn round_budget_owed(facts: &PrFacts) -> bool {
    let live = || {
        facts
            .rows
            .iter()
            .filter(|r| r.open && r.status != REVIEW_STATUS_DROPPED)
    };
    if live().any(|r| r.status == REVIEW_STATUS_IN_FLIGHT) {
        return false;
    }
    live().any(|r| row_owed(r).is_some())
}

/// Seconds since the newest activity this sweep can see on the pull request — the latest of its
/// rows' reviewer and ticket runs, or `0` when it can date none.
///
/// The budget is a counter with no timestamp, so when it was SPENT cannot be read off it. The newest
/// run is the closest honest anchor for "how long has this been stuck"; a negative elapsed is
/// clamped to zero for [`stale_secs`]'s reason.
fn newest_activity_secs(facts: &PrFacts, now: DateTime<Utc>) -> i64 {
    let newest = facts
        .rows
        .iter()
        .flat_map(|r| {
            [
                r.reviewer_run.as_ref().map(RunMoment::last_at),
                r.ticket_run.as_ref().map(RunMoment::last_at),
            ]
        })
        .flatten()
        .max();
    newest.map_or(0, |n| now.signed_duration_since(n).num_seconds().max(0))
}

impl Orchestrator {
    /// Runs one reconciliation sweep: reads the live watch set, dates each row against the `runs`
    /// ledger, and records what diverged. **Reports only** — nothing here dispatches, arms, merges
    /// or moves anything.
    ///
    /// On the control task, and cheap enough to belong there: the reads are indexed SQLite point
    /// queries bounded by the number of OPEN watch rows (one per pull request per required
    /// reviewer), which is bounded by the roster. It asks GitHub nothing at all — the whole sweep is
    /// local — which is why it is here rather than in the review watcher, whose tick `continue`s
    /// past everything when a `gh` lookup answers nothing. A daemon that cannot reach GitHub is
    /// exactly a daemon whose board may have quietly stopped.
    pub(crate) fn reconcile_review_divergence(&mut self) {
        if !self.review_ticketless_enabled() {
            self.set_review_divergences(Vec::new());
            return;
        }
        // The current-label set has no writer above `on_tick`'s three early-return gates (STUDIO-949
        // round 11). This sweep deliberately runs ABOVE them, so it keeps executing on a daemon held
        // by a bad config, an armed drain or a dead credential — and on a daemon held since boot NO
        // selection pass has ever READ THE BOARD, leaving `labelled` empty for the whole process
        // lifetime. The held-row filter below would then match nothing, and this sweep would publish
        // a `review_divergence` WARN for the very ticket the operator deliberately took over — the
        // false alarm the filter exists to prevent, on every tick. With no pass having looked, an
        // empty set is "unknown", not "no hold", so report NOTHING rather than a verdict the sweep
        // cannot stand behind. (Once a pass has run the set is a real answer and the filter is
        // exact; a false stall for a held ticket is strictly worse than a deferred report, and this
        // sweep is a report, not a control.)
        //
        // The set and the latch are read together, under one lock (STUDIO-949 round 13): read
        // separately, a pass landing between the two calls would let this sweep hold an un-primed
        // empty set and then read `primed == true`, treating "nothing has looked" as "no hold".
        //
        // MUTATION: drop this branch and
        // `an_unprimed_hold_ledger_reports_no_divergence` reds (the held row is reported).
        let (labelled, ledger_primed) = self.human_holds.labelled_and_primed();
        if !ledger_primed {
            self.set_review_divergences(Vec::new());
            return;
        }
        let rows = match self.store().load_live_review_watch() {
            Ok(rows) => rows,
            Err(e) => {
                // The previous verdict is deliberately LEFT STANDING rather than cleared: a store
                // read that failed is not evidence that a divergence resolved, and clearing would
                // flicker the warning off and re-log the crossing when the next read succeeds.
                tracing::warn!(
                    err = %e,
                    "review reconciliation: the watch set could not be read; this sweep decides nothing"
                );
                return;
            }
        };
        // Read once, before the rows, because the capacity-hold freshness test below needs it.
        let now = (self.now)();
        // The current `rhapsody:human` LABEL set (STUDIO-949), already lowercased for the
        // case-insensitive comparison below. A row whose origin ticket wears the label is a
        // DELIBERATE hold, not a stalled obligation, so it is dropped before the rules can date it —
        // the module doc's "a held ticket never arms a watch row" is false (a label applied after a
        // run leaves one), so this is a filter, not a construction. Read with the priming latch,
        // above, under one lock.
        //
        // This reads the CURRENT-LABEL set, not the reported-hold subset the console reads. The
        // reported set deliberately excludes a ticket the daemon is RUNNING right now (a live run is
        // not yet a deliberate hold for an operator), but the sweep must honour a label added to a
        // run that is still live — the likeliest way the label is ever applied, and precisely the
        // shape the ticketless watcher and the auto-merge gate already read this same set for. Using
        // the reported subset instead let the sweep publish a `review_divergence` WARN for a ticket
        // this feature had deliberately blocked. Empty on a daemon with no hold, so the default path
        // is byte-identical.
        // Grouped by pull request, preserving `load_live_review_watch`'s stable order so the
        // reported list is stable across sweeps and a console diff is not noise.
        let mut order: Vec<PrCoord> = Vec::new();
        let mut by_pr: HashMap<PrCoord, PrFacts> = HashMap::new();
        for row in &rows {
            if let Some(ticket) = origin_ticket(&row.introduced_by)
                && labelled.contains(&ticket.to_ascii_lowercase())
            {
                continue; // a deliberate hold, not this sweep's business
            }
            let pr = PrCoord::new(&row.key.owner, &row.key.repo, row.key.number);
            // Per project (STUDIO-927): each pull request reads the override for the project
            // that owns its repo, so one repo can be held back while a sibling still merges.
            // A repo no resolved project owns falls back to the top-level value.
            let auto_merge = self.review_auto_merge_for_repo(&row.key.owner, &row.key.repo);
            let ticket = origin_ticket(&row.introduced_by)
                .unwrap_or_default()
                .to_string();
            let facts = RowFacts {
                reviewer: row.key.reviewer.clone(),
                status: row.status.clone(),
                ticket: ticket.clone(),
                open: row.open,
                capacity_held: self.fresh_capacity_hold(
                    &pr,
                    &review_key(
                        &row.key.owner,
                        &row.key.repo,
                        row.key.number,
                        &row.key.reviewer,
                    ),
                    now,
                ),
                capacity_unreadable: self.unreadable_attempts(&pr),
                reviewer_run: self.newest_run_moment(&review_key(
                    &row.key.owner,
                    &row.key.repo,
                    row.key.number,
                    &row.key.reviewer,
                )),
                ticket_run: (!ticket.is_empty())
                    .then(|| self.newest_run_moment(&ticket))
                    .flatten(),
            };
            by_pr
                .entry(pr.clone())
                .or_insert_with(|| {
                    order.push(pr.clone());
                    PrFacts {
                        pr: pr.to_string(),
                        rows: Vec::new(),
                        auto_merge,
                    }
                })
                .rows
                .push(facts);
        }
        let found: Vec<Divergence> = order
            .iter()
            .filter_map(|pr| by_pr.get(pr).map(|facts| (pr, facts)))
            .filter_map(|(pr, facts)| {
                // STUDIO-956, first because it is the cause and the staleness rules below would only
                // report the resulting silence an hour and a half later, without naming the bound. A
                // spent shared review↔author budget stops BOTH halves of the loop, so a round that is
                // still owed will never be dispatched; an in-flight round is progressing and an
                // approved pull request is the merge gate's business, so neither is reported here.
                //
                // A manager adjudication suppresses the row rules entirely: an ESCALATE is reported
                // as the decision it is (with its findings and rounds), a SHIP is reported only
                // while the merge gate is still stuck on an unapproved row, and a pull request the
                // manager is still deciding is not a stall at all.
                let mut d = match self.adjudication(pr) {
                    Some(crate::reviewadjudicate::Adjudication::Escalate {
                        head,
                        rounds,
                        findings,
                        reason,
                    }) => Some(Divergence {
                        pr: pr.to_string(),
                        kind: DivergenceKind::ReviewEscalated,
                        ticket: facts
                            .rows
                            .iter()
                            .find(|r| !r.ticket.is_empty())
                            .map(|r| r.ticket.clone())
                            .unwrap_or_default(),
                        reviewer: String::new(),
                        stale_secs: newest_activity_secs(facts, now),
                        auto_merge_reason: None,
                        // A manager decision, not a slot: the loop is stopped because the
                        // adjudication settled it, and this kind's WARN arm names that cause and
                        // returns before the capacity wording is ever reached (STUDIO-950).
                        capacity_held: None,
                        capacity_unreadable: None,
                        adjudicated_head: head,
                        // STUDIO-1005: the head the watcher last observed. `""` until it has
                        // observed this pull request, which renders exactly as before this ticket.
                        current_head: self
                            .review_observed_head
                            .get(pr)
                            .cloned()
                            .unwrap_or_default(),
                        rounds,
                        findings,
                        reason,
                    }),
                    // A `ship` adjudicates the OPEN FINDINGS, never the gates — so if a live row
                    // still records findings rather than an approval at the head, the merge gate
                    // will never clear on its own and the pull request would otherwise be silent
                    // forever. Report it as the decision it is; a shipped pull request whose rows
                    // are ALL approved falls through below, where auto-merge either merges it or
                    // `ApprovedStillOpen` reports the gate holding it after the staleness threshold.
                    Some(crate::reviewadjudicate::Adjudication::Ship { head, rounds })
                        if facts.rows.iter().any(|r| {
                            r.open
                                && r.status != REVIEW_STATUS_DROPPED
                                && r.status != REVIEW_STATUS_APPROVED
                        }) =>
                    {
                        Some(Divergence {
                            pr: pr.to_string(),
                            kind: DivergenceKind::ReviewShipped,
                            ticket: facts
                                .rows
                                .iter()
                                .find(|r| !r.ticket.is_empty())
                                .map(|r| r.ticket.clone())
                                .unwrap_or_default(),
                            reviewer: String::new(),
                            stale_secs: newest_activity_secs(facts, now),
                            auto_merge_reason: None,
                            // Same as the escalation above (STUDIO-950): a shipped loop is stopped
                            // by the manager's decision, which its own WARN arm names.
                            capacity_held: None,
                            capacity_unreadable: None,
                            adjudicated_head: head,
                            // A ship is not an escalation's reason, so it carries no observed head
                            // (STUDIO-1005): the supersession notice is the escalation surface's.
                            current_head: String::new(),
                            rounds,
                            findings: Vec::new(),
                            reason: String::new(),
                        })
                    }
                    // A shipped pull request whose rows are ALL approved is not this rule's: it is
                    // the merge gate's business, and the gate either merges it or
                    // `ApprovedStillOpen` reports it after the staleness threshold.
                    Some(crate::reviewadjudicate::Adjudication::Ship { .. }) => {
                        reconcile_pr(facts, now, RECONCILE_STALE_AFTER)
                    }
                    // Still deciding: the loop is deliberately stopped while the manager's turn
                    // runs, so the row and budget rules must not report it.
                    Some(crate::reviewadjudicate::Adjudication::InFlight { .. }) => None,
                    None if self.round_budget_spent(pr) && round_budget_owed(facts) => {
                        Some(Divergence {
                            pr: pr.to_string(),
                            kind: DivergenceKind::RoundBudgetExhausted,
                            ticket: facts
                                .rows
                                .iter()
                                .find(|r| !r.ticket.is_empty())
                                .map(|r| r.ticket.clone())
                                .unwrap_or_default(),
                            reviewer: String::new(),
                            stale_secs: newest_activity_secs(facts, now),
                            // Filled in below only for `ApprovedStillOpen`; a spent budget has
                            // nothing for the auto-merge ledger to say.
                            auto_merge_reason: None,
                            // Nor the capacity annotations (STUDIO-950): a spent round budget is
                            // not a slot wait, and its own WARN arm states the cause and the
                            // remedy before the capacity wording is reached.
                            capacity_held: None,
                            capacity_unreadable: None,
                            adjudicated_head: String::new(),
                            current_head: String::new(),
                            rounds: 0,
                            findings: Vec::new(),
                            reason: String::new(),
                        })
                    }
                    None => reconcile_pr(facts, now, RECONCILE_STALE_AFTER),
                }?;
                // The one place this sweep reads the auto-merge ledger (STUDIO-923): only for
                // `ApprovedStillOpen`, the one divergence auto-merge would itself be attempting a
                // merge against — the ledger has nothing meaningful to say about a pull request
                // still owed a review round. A `None` here is silent either way: no ledger handle
                // (the review watcher never spawned) and no entry for this coordinate (auto-merge
                // off, or this head never reached a gate) both fall back to the plain wording.
                if d.kind == DivergenceKind::ApprovedStillOpen {
                    // STUDIO-961: a conflict route-back the watcher has already fired IS the
                    // progress this pull request was waiting on — the author has been handed it and
                    // owns the next move. Reporting it as needing a human would be the false
                    // positive the divergence's own docs warn against, so it is dropped entirely
                    // (and logged as recovered, once, by `set_review_divergences`).
                    //
                    // While it is FRESH. The transition is the progress at the moment it fires, and
                    // only for so long: a route-back the author never answers — or a tracker move
                    // that never landed — stops being progress once it is itself older than this
                    // sweep's own staleness horizon, and the pull request needs the human signal
                    // again. Suppressing it forever would trade a false positive for a false
                    // negative, which is the worse of the two.
                    if let Some(routed) = self.conflict_routed.get(pr)
                        && stale_secs(now, routed.routed_at, RECONCILE_STALE_AFTER).is_none()
                    {
                        return None;
                    }
                    d.auto_merge_reason = self.automerge_ledger.as_ref().and_then(|l| l.peek(pr));
                    // The approved-and-open arm hardcodes `capacity_held` to `None` (a hold defers a
                    // ROUND, and this row's next move is a merge), but that reasoning does not extend
                    // to the unreadability annotation: GitHub refusing the coordinate is exactly as
                    // true for a row awaiting a merge, and during a `gh` outage the auto-merge ledger
                    // stays empty — the outage that sets `review_watch_unreadable` is the same one
                    // that keeps `peek` from answering — so `(None, None)` is the ordinary arm here,
                    // not the unlucky one. Without this the row reports the false "nothing has
                    // reported it blocked" page the watcher refutes every tick (STUDIO-950 round 20).
                    d.capacity_unreadable = self.unreadable_attempts(pr);
                }
                Some(d)
            })
            .collect();
        self.set_review_divergences(found);
    }

    /// The capacity hold recorded for one review key, or `None` when there is none or the record is
    /// no longer FRESH (STUDIO-950).
    ///
    /// The freshness test is what keeps this sweep decoupled from the watcher, which its own module
    /// docs treat as a design constraint (the sweep is local and must keep deciding when `gh` cannot
    /// be reached). A watcher that is still sweeping stamps `review_watch_swept` on every tick, so a
    /// hold it is still carrying stays fresh however many rotations pass before the cursor revisits
    /// that pull request; a watcher that has STOPPED — a `gh` outage delivers no sweep event at all,
    /// so the stamp stops advancing — leaves every hold to age out. The age is measured on the
    /// WATCHER's liveness, not on the hold's own `recorded`, because the cursor reaches only
    /// `MAX_PR_STATE_CALLS_PER_TICK` pull requests a tick and per-hold ageing expired rounds a
    /// healthy watcher was still holding (STUDIO-950 round 11). A hold older than
    /// [`CAPACITY_HOLD_TTL`] — or one whose watcher stamp is in the future, which no honest clock
    /// produces — is dropped and the row reports under its ordinary wording. A hold that predates
    /// any sweep (no liveness stamp yet) falls back to its own `recorded`; no PRODUCTION hold can be
    /// in that state (the watcher stamps before it ever inserts a hold), so the fallback exists for
    /// this module's own fixtures, which insert holds directly.
    ///
    /// The global stamp alone is not enough (STUDIO-950 round 14): it advances whenever ANY watched
    /// pull request answers, so a sibling keeps a hold fresh for a pull request GitHub has stopped
    /// answering for. `review_watch_unreadable` carries that per-coordinate fact — how many
    /// CONSECUTIVE `gh` lookups of it have FAILED — and a hold whose coordinate has reached
    /// [`UNREADABLE_ATTEMPTS_TO_DROP_HOLD`] is not a wait anything is still confirming. Counting
    /// ATTEMPTS rather than wall-clock is deliberate (STUDIO-950 round 15): the quantity is a
    /// ROTATION of the cursor (`ceil(watch_set / MAX_PR_STATE_CALLS_PER_TICK)` ticks), not the
    /// one-tick [`CAPACITY_HOLD_TTL`], so no constant sized against the tick could bound it without
    /// blinking a live hold. See [`UNREADABLE_ATTEMPTS_TO_DROP_HOLD`]. The one-attempt grace keeps
    /// a single transient rate-limit from blinking a live annotation off for a sweep.
    fn fresh_capacity_hold(
        &self,
        pr: &PrCoord,
        id: &str,
        now: DateTime<Utc>,
    ) -> Option<CapacityHold> {
        let hold = self.review_capacity_held.get(id)?;
        let swept = self.review_watch_swept.unwrap_or(hold.recorded);
        let age = now.signed_duration_since(swept).to_std().ok()?;
        if age >= CAPACITY_HOLD_TTL {
            return None;
        }
        // A pull request whose lookup has failed for enough CONSECUTIVE ATTEMPTS is not one the
        // watcher can be said to be holding. Counting attempts (not a deadline) is what makes this
        // rotation-independent: the counter only moves on a tick that actually asked this
        // coordinate, so a large watch set cannot age a still-carried hold out between rotations.
        // The denial is not silent: `unreadable_attempts` carries the same count onto the reported
        // row, so the sweep names GitHub's silence instead of the false plain page (round 18).
        if self.unreadable_attempts(pr).is_some() {
            return None;
        }
        Some(*hold)
    }

    /// The consecutive failed `gh` lookups recorded for one coordinate, once they have reached
    /// [`UNREADABLE_ATTEMPTS_TO_DROP_HOLD`] — the count at which [`Self::fresh_capacity_hold`] stops
    /// naming a hold. `None` below the threshold, so the annotation is absent for a coordinate the
    /// watcher is still answering for. STUDIO-950 round 18.
    fn unreadable_attempts(&self, pr: &PrCoord) -> Option<u32> {
        let attempts = self.review_watch_unreadable.get(pr).copied().unwrap_or(0);
        (attempts >= UNREADABLE_ATTEMPTS_TO_DROP_HOLD).then_some(attempts)
    }

    /// The newest `runs` row for one issue identifier, or `None` when the ledger has none (a store
    /// that is off, a history pruned past it, or work that genuinely never ran).
    ///
    /// A read failure answers `None` and warns rather than propagating: there is nothing a caller
    /// could do about it, and the rules already treat an undatable row as silent.
    fn newest_run_moment(&self, identifier: &str) -> Option<RunMoment> {
        let runs = match self.store().list_runs(RunFilter {
            issue: identifier.to_string(),
            limit: 1,
            ..Default::default()
        }) {
            Ok(runs) => runs,
            Err(e) => {
                tracing::warn!(
                    identifier, err = %e,
                    "review reconciliation: a run history could not be read; this row is not dated"
                );
                return None;
            }
        };
        let run = runs.first()?;
        Some(RunMoment {
            started_at: parse_run_time(&run.started_at)?,
            // An unparseable end on a row that HAS one reads as in-flight, which is the safe
            // direction: in-flight reports nothing.
            ended_at: parse_run_time(&run.ended_at),
            outcome: run.outcome.clone(),
        })
    }

    /// Replaces the reported set, logging the transitions and rate-limiting the steady state.
    fn set_review_divergences(&mut self, found: Vec<Divergence>) {
        // The capacity annotations each pull request carried on the PREVIOUS sweep, so an annotation
        // that APPEARS is a transition that logs on the sweep that learned it rather than waiting out
        // the steady-state rate limit. A pull request newly reported simply has none, which is why
        // the crossing sweep's `sweeps == 1` still logs. BOTH annotations are carried, not just the
        // hold (STUDIO-950 round 21): the unreadable denial is likewise filled by
        // [`Orchestrator::reconcile_review_divergence`] and compared by PRESENCE here, and a row that
        // first crossed WITHOUT it and only later had its coordinate go unreadable would otherwise
        // update the advisory while the log said nothing for a full `RECONCILE_LOG_EVERY` window —
        // the same false page on the second sweep. The count is deliberately NOT compared, only its
        // presence: it climbs on every failed lookup, so comparing the number would make every
        // outage sweep a transition and defeat the rate limit.
        let previous: HashMap<&str, (Option<CapacityHold>, bool, bool)> = self
            .review_divergence
            .iter()
            .map(|d| {
                (
                    d.pr.as_str(),
                    (
                        d.capacity_held,
                        d.capacity_unreadable.is_some(),
                        d.superseded(),
                    ),
                )
            })
            .collect();
        for d in &found {
            let sweeps = self.review_divergent.entry(d.pr.clone()).or_insert(0);
            *sweeps += 1;
            let sweeps = *sweeps;
            // A newly added or CHANGED capacity annotation is its own report transition — whether it
            // is a hold appearing/changing or the unreadable denial appearing. The generic repeat
            // clock alone would let a row that first crossed the threshold WITHOUT an annotation keep
            // the plain "nothing has reported it blocked" wording for a full `RECONCILE_LOG_EVERY`
            // window (~30 min at the default cadence) after the watcher learned the cause — the false
            // page this ticket exists to close, reintroduced on the second sweep instead of the
            // first. `recorded` is excluded from the hold comparison (see [`same_capacity`]): it is
            // re-stamped when the rotating cursor next evaluates the pull request, not every sweep,
            // so comparing it would log a rotation as a transition and defeat the rate limit; the
            // unreadable count is likewise reduced to presence for the same reason.
            let (prev_hold, prev_unreadable, prev_superseded) = previous
                .get(d.pr.as_str())
                .copied()
                .unwrap_or((None, false, false));
            // A supersession APPEARING is its own transition (STUDIO-1005), for the capacity
            // annotations' reason one paragraph up: the head move is the news, and waiting out the
            // steady-state rate limit would leave the log repeating "still unaddressed" for a full
            // `RECONCILE_LOG_EVERY` window after the branch moved. Presence only — `current_head`
            // can change again without the superseded FACT changing, and comparing the SHA would
            // make every push a logged transition.
            let annotation_changed = !same_capacity(prev_hold, d.capacity_held)
                || prev_unreadable != d.capacity_unreadable.is_some()
                || prev_superseded != d.superseded();
            // The crossing sweep, an annotation transition, and the rate-limited repeats in ONE
            // condition: at the crossing the count is 1, and `1 - 1` is a multiple of everything.
            if annotation_changed || (sweeps - 1).is_multiple_of(RECONCILE_LOG_EVERY) {
                // STUDIO-956: the spent budget HAS a cause and a remedy, so it must not wear the
                // "nothing has reported it blocked" wording the other rules use — that copy being
                // false about a capped pull request is the defect this ticket's first ⚠️ names. The
                // line says what stopped the loop and how an operator resumes it.
                if d.kind == DivergenceKind::RoundBudgetExhausted {
                    tracing::warn!(
                        pr = %d.pr,
                        kind = d.kind.as_str(),
                        ticket = %d.ticket,
                        reviewer = %d.reviewer,
                        stale_secs = d.stale_secs,
                        sweeps,
                        "review reconciliation: {} — {}. Nothing will resume it on its own; an \
                         operator can clear the budget from the console (`POST \
                         /api/v1/reviews/clear`) or close the pull request.",
                        d.pr,
                        d.kind.detail()
                    );
                    continue;
                }
                // STUDIO-956's decider: the manager reached the round threshold and escalated. The
                // line carries the decision, the head it stopped at, the round count and the
                // specific findings — an escalation an operator can act on, not "needs a human".
                if d.kind == DivergenceKind::ReviewEscalated {
                    tracing::warn!(
                        pr = %d.pr,
                        kind = d.kind.as_str(),
                        ticket = %d.ticket,
                        head = %d.adjudicated_head,
                        rounds = d.rounds,
                        findings = ?d.findings,
                        reason = %d.reason,
                        stale_secs = d.stale_secs,
                        sweeps,
                        "review reconciliation: {} — {}. Head {}, {} rounds. Reason: {}. Open \
                         findings: {}",
                        d.pr,
                        d.kind.detail(),
                        d.adjudicated_head,
                        d.rounds,
                        if d.reason.trim().is_empty() {
                            "not stated"
                        } else {
                            d.reason.trim()
                        },
                        if d.findings.is_empty() {
                            "none recorded".to_string()
                        } else {
                            d.findings.join("; ")
                        }
                    );
                    // STUDIO-1005: when the watcher has seen the branch move past the head the
                    // escalation was computed at, say so ON THE LINE THAT CARRIES THE REASON — the
                    // same defect one surface over. The operator reading "still unaddressed" must not
                    // have to infer that the text is a snapshot; the log is the third place this
                    // reason reaches a human after the room post and the pull-request comment.
                    if let Some(note) = d.supersession() {
                        tracing::warn!(
                            pr = %d.pr,
                            adjudicated_head = %d.adjudicated_head,
                            current_head = %d.current_head,
                            "review reconciliation: the escalation above is SUPERSEDED — {}",
                            note
                        );
                    }
                    continue;
                }
                // STUDIO-956's decider: the manager SHIPPED the loop and the merge gate still holds
                // it. The generic copy below would be false here — something HAS reported it
                // blocked — so name the decision, the head it stopped at and the round count. A
                // human merges it, approves the head, or clears the adjudication from the console.
                if d.kind == DivergenceKind::ReviewShipped {
                    tracing::warn!(
                        pr = %d.pr,
                        kind = d.kind.as_str(),
                        ticket = %d.ticket,
                        head = %d.adjudicated_head,
                        rounds = d.rounds,
                        stale_secs = d.stale_secs,
                        sweeps,
                        "review reconciliation: {} — {}. Head {}, {} rounds. A human must merge it \
                         or clear the adjudication from the console (`POST /api/v1/reviews/clear`).",
                        d.pr,
                        d.kind.detail(),
                        d.adjudicated_head,
                        d.rounds
                    );
                    continue;
                }
                // STUDIO-967: the author's newest run was stopped at its per-run token ceiling, so
                // the generic "nothing has reported it blocked" below would be false — this daemon
                // stopped it and said so on the run. Name the bound and the two ways out.
                if d.kind == DivergenceKind::AuthorTokenCeilingStopped {
                    tracing::warn!(
                        pr = %d.pr,
                        kind = d.kind.as_str(),
                        ticket = %d.ticket,
                        reviewer = %d.reviewer,
                        stale_secs = d.stale_secs,
                        sweeps,
                        "review reconciliation: {} — {}. The run was stopped by \
                         `agent.max_run_tokens`; raise the ceiling or split the ticket to make it \
                         fit, then re-run. This sweep only reports, so it needs a human.",
                        d.pr,
                        d.kind.detail()
                    );
                    continue;
                }
                // STUDIO-967's review half: a ticketless REVIEW round was stopped at the same
                // ceiling, so it read nothing and the head is owed a review. Same shape as the
                // author arm — name the bound instead of the false "no reviewer run has started" —
                // but the remedy differs: the stopped round's `pr:`
                // key is held for the rest of this session (a re-offer would re-burn a whole
                // ceiling on the same read), so the head is only re-offered after a restart.
                if d.kind == DivergenceKind::ReviewTokenCeilingStopped {
                    tracing::warn!(
                        pr = %d.pr,
                        kind = d.kind.as_str(),
                        ticket = %d.ticket,
                        reviewer = %d.reviewer,
                        stale_secs = d.stale_secs,
                        sweeps,
                        "review reconciliation: {} — {}. The round was stopped by \
                         `agent.max_run_tokens`; raise the ceiling, then restart the daemon to \
                         re-offer this head (the stopped round's key stays held this session so it \
                         cannot re-burn the ceiling). This sweep only reports, so it needs a human.",
                        d.pr,
                        d.kind.detail()
                    );
                    continue;
                }
                // STUDIO-923: when auto-merge has already said something about this exact pull
                // request, name it instead of claiming nothing has. The sentence states no count:
                // auto-merge's own attempts run on the review watcher's separate, configurable
                // cadence (`polling.pr_state_interval_ms`, default 15s; the legacy pinned
                // `PR_STATE_POLL_INTERVAL` was 120s), not this sweep's `polling.interval_ms`
                // (default 30s), so this sweep's own `sweeps` field would misstate auto-merge's
                // tally as its own — trading the ticket's false negative for a false positive. No
                // ledger entry (auto-merge off, or this head never reached a gate) falls back to the
                // original wording unchanged.
                //
                // STUDIO-950: a round the review watcher deferred for want of a global slot is the
                // same shape of fact and takes the same slot — name it instead of claiming nothing
                // has reported it blocked. It is checked first because it is the only annotation a
                // `requested`/`truncated` row can carry (auto-merge's belongs to `approved`), and it
                // states the holder COUNT, which the operator tuning the budget needs. The count is
                // the watcher's own, from its most recent sweep, not this sweep's `sweeps`.
                // STUDIO-957: a divergence whose subject is budget-held has a cause this process
                // already recorded (`Orchestrator::budget_hold_for`) — the provider's daily token
                // budget is spent, so no run on it will start. Name that instead of the false
                // "nothing has reported it blocked"; it sits between capacity (the immediate
                // resource blocker) and auto-merge (a decline reason).
                let budget_held = self.budget_hold_for(&d.pr, &d.ticket);
                match (d.capacity_held, budget_held, d.auto_merge_reason) {
                    (Some(hold), _, _) => {
                        let budget = hold.budget_key();
                        tracing::warn!(
                            pr = %d.pr,
                            kind = d.kind.as_str(),
                            ticket = %d.ticket,
                            reviewer = %d.reviewer,
                            stale_secs = d.stale_secs,
                            sweeps,
                            holding = hold.holders,
                            budget,
                            "review reconciliation: {} — {}. It is held for capacity: {} run(s) \
                             hold the {} budget, so no reviewer run can start yet. This sweep only \
                             reports, so it needs a human.",
                            d.pr,
                            d.kind.detail(),
                            hold.holders,
                            budget
                        );
                    }
                    (None, Some(hold), _) => {
                        tracing::warn!(
                            pr = %d.pr,
                            kind = d.kind.as_str(),
                            ticket = %d.ticket,
                            reviewer = %d.reviewer,
                            stale_secs = d.stale_secs,
                            sweeps,
                            provider = %hold.provider,
                            daily_tokens = hold.daily_tokens,
                            spent_tokens = hold.spent_tokens,
                            "review reconciliation: {} — {}. It is held for budget: the provider \
                             {}'s daily token budget is spent ({} of {}), so no run on it can start \
                             yet. Work on other providers continues; this sweep only reports, so it \
                             needs a human.",
                            d.pr,
                            d.kind.detail(),
                            hold.provider,
                            hold.spent_tokens,
                            hold.daily_tokens
                        );
                    }
                    (None, None, Some(reason)) => {
                        tracing::warn!(
                            pr = %d.pr,
                            kind = d.kind.as_str(),
                            ticket = %d.ticket,
                            reviewer = %d.reviewer,
                            stale_secs = d.stale_secs,
                            sweeps,
                            auto_merge_reason = reason,
                            "review reconciliation: {} — {}. Auto-merge has been declining it: {}. \
                             This sweep only reports, so it needs a human.",
                            d.pr,
                            d.kind.detail(),
                            reason
                        );
                    }
                    (None, None, None) if d.capacity_unreadable.is_some() => {
                        // STUDIO-950 round 18: the hold (if any) was DENIED because GitHub stopped
                        // answering for this coordinate, so the fallback's "nothing has reported it
                        // blocked" is false — the watcher reports the failure every tick. Name the
                        // silence instead. `capacity_unreadable` is `Some` by the guard; the default
                        // cannot panic and only affects a hand-written `Divergence` fixture.
                        let attempts = d.capacity_unreadable.unwrap_or_default();
                        tracing::warn!(
                            pr = %d.pr,
                            kind = d.kind.as_str(),
                            ticket = %d.ticket,
                            reviewer = %d.reviewer,
                            stale_secs = d.stale_secs,
                            sweeps,
                            unreadable_attempts = attempts,
                            "review reconciliation: {} — {}. Its GitHub state could not be read for \
                             {} consecutive attempt(s), so the daemon cannot confirm it is still \
                             progressing. This sweep only reports, so it needs a human.",
                            d.pr,
                            d.kind.detail(),
                            attempts
                        );
                    }
                    (None, None, None) => {
                        tracing::warn!(
                            pr = %d.pr,
                            kind = d.kind.as_str(),
                            ticket = %d.ticket,
                            reviewer = %d.reviewer,
                            stale_secs = d.stale_secs,
                            sweeps,
                            "review reconciliation: {} — {}. Nothing is progressing it and nothing \
                             has reported it blocked; this sweep only reports, so it needs a human.",
                            d.pr,
                            d.kind.detail()
                        );
                    }
                }
            }
        }
        // Recovery is news exactly once, and only for a pull request that was actually REPORTED.
        let still: Vec<&str> = found.iter().map(|d| d.pr.as_str()).collect();
        let recovered: Vec<String> = self
            .review_divergent
            .keys()
            .filter(|pr| !still.contains(&pr.as_str()))
            .cloned()
            .collect();
        for pr in recovered {
            self.review_divergent.remove(&pr);
            tracing::info!(
                pr = %pr,
                "review reconciliation: a pull request whose state and activity had diverged is \
                 progressing again"
            );
        }
        self.review_divergence = found;
    }

    /// The divergences this daemon is currently reporting — read by `project_statuses` to light
    /// [`REVIEW_DIVERGENCE_WARNING`] and by `build_snapshot` to serve the detail.
    pub(crate) fn review_divergences(&self) -> &[Divergence] {
        &self.review_divergence
    }
}

/// Parses a `runs` timestamp, or `None` for the empty string a live run carries (and for anything
/// unparseable — a malformed row must not decide anything).
fn parse_run_time(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s)
            .expect("test timestamp")
            .with_timezone(&Utc)
    }

    /// A finished run.
    fn ran(started: &str, ended: &str) -> Option<RunMoment> {
        Some(RunMoment {
            started_at: t(started),
            ended_at: Some(t(ended)),
            outcome: rhapsody_store::OUTCOME_COMPLETED.to_string(),
        })
    }

    /// A run still in flight.
    fn running(started: &str) -> Option<RunMoment> {
        Some(RunMoment {
            started_at: t(started),
            ended_at: None,
            outcome: rhapsody_store::OUTCOME_RUNNING.to_string(),
        })
    }

    /// A FINISHED run that was stopped at its per-run token ceiling (STUDIO-967).
    fn ran_at_ceiling(started: &str, ended: &str) -> Option<RunMoment> {
        Some(RunMoment {
            started_at: t(started),
            ended_at: Some(t(ended)),
            outcome: rhapsody_store::OUTCOME_TOKEN_CEILING.to_string(),
        })
    }

    fn row(
        reviewer: &str,
        status: &str,
        ticket: &str,
        reviewer_run: Option<RunMoment>,
        ticket_run: Option<RunMoment>,
    ) -> RowFacts {
        RowFacts {
            reviewer: reviewer.to_string(),
            status: status.to_string(),
            ticket: ticket.to_string(),
            open: true,
            capacity_held: None,
            capacity_unreadable: None,
            reviewer_run,
            ticket_run,
        }
    }

    fn pr(rows: Vec<RowFacts>, auto_merge: bool) -> PrFacts {
        PrFacts {
            pr: "makewhatis/rhapsody#100".to_string(),
            rows,
            auto_merge,
        }
    }

    /// The whole rule, driven at the REAL threshold rather than a shrunken test one: the incidents
    /// below are hours long and the constant is ninety minutes, so nothing here needs a fake window.
    fn verdict(facts: &PrFacts, now: &str) -> Option<Divergence> {
        reconcile_pr(facts, t(now), RECONCILE_STALE_AFTER)
    }

    // ── The six incidents, replayed ──────────────────────────────────────────────────────────
    //
    // These are the acceptance: every one is a worked example of the invariant breaking, and each
    // was diagnosed by hand between 2026-09-12 and 2026-09-14. What makes the sweep worth shipping
    // is visible in the fixtures themselves — 875, 882, 885, 893 and 894 have five DIFFERENT causes
    // and produce the SAME row state, so one cause-agnostic rule reports all five and would have
    // reported them before anyone knew which was which.

    /// STUDIO-875 — rhapsody's pull requests had no tracker attachment, so `apply_github_summons`
    /// dropped every hit and the author was never re-engaged. Eleven hours.
    #[test]
    fn reports_studio_875_findings_never_re_engaged_the_author() {
        let facts = pr(
            vec![row(
                "jimmy",
                REVIEW_STATUS_REVIEWED,
                "STUDIO-870",
                ran("2026-09-12T09:00:00Z", "2026-09-12T09:25:00Z"),
                // The authoring run, which ENDED before the review even started.
                ran("2026-09-12T08:00:00Z", "2026-09-12T08:50:00Z"),
            )],
            false,
        );
        let d = verdict(&facts, "2026-09-12T20:25:00Z").expect("reported");
        assert_eq!(d.kind, DivergenceKind::ChangesRequestedNoRun);
        assert_eq!(d.ticket, "STUDIO-870");
        assert_eq!(d.stale_secs, 11 * 3600, "eleven hours, as the incident ran");
    }

    /// STUDIO-967 — the author's newest run after the verdict exists, but it was STOPPED at its
    /// per-run token ceiling. The run started after the verdict, so the plain `ChangesRequestedNoRun`
    /// rule reads it as the author having moved and reports nothing; that is precisely the silent
    /// stall the ticket forbids, because the run made no progress and the cause is a bound this
    /// daemon applied. The sweep must name the ceiling.
    #[test]
    fn reports_studio_967_author_run_stopped_at_its_token_ceiling() {
        let facts = pr(
            vec![row(
                "alice",
                REVIEW_STATUS_REVIEWED,
                "STUDIO-967",
                ran("2026-09-21T09:00:00Z", "2026-09-21T09:25:00Z"),
                // The author DID run after the verdict — and was stopped at the ceiling.
                ran_at_ceiling("2026-09-21T10:00:00Z", "2026-09-21T10:05:00Z"),
            )],
            false,
        );
        let d = verdict(&facts, "2026-09-21T12:00:00Z").expect("reported");
        assert_eq!(d.kind, DivergenceKind::AuthorTokenCeilingStopped);
        assert_eq!(d.ticket, "STUDIO-967");
        assert!(
            d.kind.detail().contains("token ceiling"),
            "the operator must read the cause, got {:?}",
            d.kind.detail()
        );
    }

    /// The sibling guard: a ticket run after the verdict that merely COMPLETED is still the author
    /// having moved, and must NOT be reported. Without this, the ceiling check above could decay into
    /// "any post-verdict run that is not in flight", which would report every healthy review.
    #[test]
    fn a_completed_author_run_after_the_verdict_is_not_reported() {
        let facts = pr(
            vec![row(
                "alice",
                REVIEW_STATUS_REVIEWED,
                "STUDIO-967",
                ran("2026-09-21T09:00:00Z", "2026-09-21T09:25:00Z"),
                ran("2026-09-21T10:00:00Z", "2026-09-21T10:05:00Z"),
            )],
            false,
        );
        assert!(verdict(&facts, "2026-09-21T12:00:00Z").is_none());
    }

    /// A ceiling-stopped run that PREDATES the verdict is not this rule's business: it did not move
    /// the ticket after the findings landed, so the ordinary `changes_requested_no_run` applies.
    #[test]
    fn a_ceiling_stop_before_the_verdict_falls_through_to_the_plain_rule() {
        let facts = pr(
            vec![row(
                "alice",
                REVIEW_STATUS_REVIEWED,
                "STUDIO-967",
                ran("2026-09-21T10:00:00Z", "2026-09-21T10:25:00Z"),
                ran_at_ceiling("2026-09-21T09:00:00Z", "2026-09-21T09:05:00Z"),
            )],
            false,
        );
        let d = verdict(&facts, "2026-09-21T12:00:00Z").expect("reported");
        assert_eq!(d.kind, DivergenceKind::ChangesRequestedNoRun);
    }

    /// STUDIO-967's review half — alice's finding on PR #208. A ticketless REVIEW round stopped at
    /// its per-run token ceiling parks its row `truncated`, which the plain rule reports as
    /// `review_requested_no_run` ("no reviewer run has started"). That is false: a run started, and
    /// this daemon killed it. The sweep must name the ceiling instead.
    #[test]
    fn reports_studio_967_review_round_stopped_at_its_token_ceiling() {
        let facts = pr(
            vec![row(
                "alice",
                REVIEW_STATUS_TRUNCATED,
                "STUDIO-967",
                // The round ran and was stopped at the ceiling; no ticket run is involved.
                ran_at_ceiling("2026-09-21T09:00:00Z", "2026-09-21T09:25:00Z"),
                None,
            )],
            false,
        );
        let d = verdict(&facts, "2026-09-21T12:00:00Z").expect("reported");
        assert_eq!(d.kind, DivergenceKind::ReviewTokenCeilingStopped);
        assert_eq!(d.reviewer, "alice");
        assert!(
            d.kind.detail().contains("token ceiling"),
            "the operator must read the cause, got {:?}",
            d.kind.detail()
        );
    }

    /// The sibling guard: a `truncated` round whose attempt merely COMPLETED (the `max_turns`
    /// backstop) must still report the ordinary `review_requested_no_run`. Without this, the check
    /// above could decay into "report every truncated row as a ceiling stop".
    #[test]
    fn a_plain_truncated_round_is_not_reported_as_a_ceiling_stop() {
        let facts = pr(
            vec![row(
                "alice",
                REVIEW_STATUS_TRUNCATED,
                "STUDIO-967",
                ran("2026-09-21T09:00:00Z", "2026-09-21T09:25:00Z"),
                None,
            )],
            false,
        );
        let d = verdict(&facts, "2026-09-21T12:00:00Z").expect("reported");
        assert_eq!(d.kind, DivergenceKind::ReviewRequestedNoRun);
    }

    /// STUDIO-882 — 875's attachment WAS written, but resolved `sourceType: "api"`, so `is_github_pr`
    /// rejected it. A different cause with an identical presentation: the fix did not fix it.
    #[test]
    fn reports_studio_882_the_fix_that_did_not_fix_it() {
        let facts = pr(
            vec![row(
                "alice",
                REVIEW_STATUS_REVIEWED,
                "STUDIO-879",
                ran("2026-09-13T10:00:00Z", "2026-09-13T10:30:00Z"),
                ran("2026-09-13T09:00:00Z", "2026-09-13T09:55:00Z"),
            )],
            false,
        );
        let d = verdict(&facts, "2026-09-13T14:00:00Z").expect("reported");
        assert_eq!(d.kind, DivergenceKind::ChangesRequestedNoRun);
    }

    /// STUDIO-885 — the summons was visible for five minutes; a ticket that could not get a
    /// concurrency slot inside that window was suppressed forever.
    #[test]
    fn reports_studio_885_a_summons_that_aged_out_while_capped() {
        let facts = pr(
            vec![row(
                "jimmy",
                REVIEW_STATUS_REVIEWED,
                "STUDIO-884",
                ran("2026-09-13T15:00:00Z", "2026-09-13T15:20:00Z"),
                ran("2026-09-13T13:00:00Z", "2026-09-13T14:30:00Z"),
            )],
            false,
        );
        assert_eq!(
            verdict(&facts, "2026-09-13T19:00:00Z")
                .expect("reported")
                .kind,
            DivergenceKind::ChangesRequestedNoRun
        );
    }

    /// STUDIO-891 — `review.reviewers` above what the roster could satisfy deferred every SECOND
    /// review permanently. The row never reaches a verdict at all, so it is the other rule's case:
    /// a round is owed and no reviewer run has started.
    #[test]
    fn reports_studio_891_an_unsatisfiable_reviewer_count() {
        let facts = pr(
            vec![
                // Reviewer one did their round.
                row(
                    "alice",
                    REVIEW_STATUS_APPROVED,
                    "STUDIO-890",
                    ran("2026-09-13T11:00:00Z", "2026-09-13T11:40:00Z"),
                    ran("2026-09-13T09:00:00Z", "2026-09-13T10:50:00Z"),
                ),
                // Reviewer two was deferred on every sweep and never ran.
                row(
                    "jimmy",
                    REVIEW_STATUS_REQUESTED,
                    "STUDIO-890",
                    None,
                    ran("2026-09-13T09:00:00Z", "2026-09-13T10:50:00Z"),
                ),
            ],
            false,
        );
        let d = verdict(&facts, "2026-09-13T17:00:00Z").expect("reported");
        assert_eq!(d.kind, DivergenceKind::ReviewRequestedNoRun);
        assert_eq!(d.reviewer, "jimmy", "the row that owes the round");
    }

    /// STUDIO-894 — a reviewer wrote an APPROVING review whose verdict recorded as
    /// changes-requested. The pull request reads approved and the daemon reads blocked; the sweep
    /// sees only that a verdict asked for changes and nothing followed, which is enough.
    #[test]
    fn reports_studio_894_a_verdict_contradicting_the_review_written() {
        let facts = pr(
            vec![row(
                "alice",
                REVIEW_STATUS_REVIEWED,
                "STUDIO-892",
                ran("2026-09-14T08:00:00Z", "2026-09-14T08:35:00Z"),
                ran("2026-09-14T06:00:00Z", "2026-09-14T07:45:00Z"),
            )],
            false,
        );
        assert_eq!(
            verdict(&facts, "2026-09-14T12:00:00Z")
                .expect("reported")
                .kind,
            DivergenceKind::ChangesRequestedNoRun
        );
    }

    /// STUDIO-893 — a summons posted at 15:20 was never applied: six hours, no run, and ZERO
    /// `skipping dispatch` lines. The cause was never diagnosed from the logs at all, which is
    /// exactly the case an enumeration of the other five would have missed.
    #[test]
    fn reports_studio_893_a_summons_that_was_never_applied() {
        let facts = pr(
            vec![row(
                "jimmy",
                REVIEW_STATUS_REVIEWED,
                "STUDIO-893",
                ran("2026-09-14T14:50:00Z", "2026-09-14T15:20:00Z"),
                ran("2026-09-14T13:00:00Z", "2026-09-14T14:40:00Z"),
            )],
            false,
        );
        let d = verdict(&facts, "2026-09-14T21:20:00Z").expect("reported");
        assert_eq!(d.kind, DivergenceKind::ChangesRequestedNoRun);
        assert_eq!(d.stale_secs, 6 * 3600, "six hours, as the incident ran");
    }

    /// STUDIO-881 — auto-merge retried a DRAFT pull request forever: 178 failed merges in three
    /// hours. Divergence (b): every required reviewer approved the head and the pull request is
    /// still open. The sweep knows nothing about drafts, or about `BEHIND` — only that the gate
    /// cleared and the merge did not happen.
    #[test]
    fn reports_studio_881_auto_merge_declining_forever() {
        let facts = pr(
            vec![
                row(
                    "alice",
                    REVIEW_STATUS_APPROVED,
                    "STUDIO-877",
                    ran("2026-09-12T12:00:00Z", "2026-09-12T12:30:00Z"),
                    ran("2026-09-12T10:00:00Z", "2026-09-12T11:50:00Z"),
                ),
                row(
                    "jimmy",
                    REVIEW_STATUS_APPROVED,
                    "STUDIO-877",
                    ran("2026-09-12T12:00:00Z", "2026-09-12T12:45:00Z"),
                    ran("2026-09-12T10:00:00Z", "2026-09-12T11:50:00Z"),
                ),
            ],
            true,
        );
        let d = verdict(&facts, "2026-09-12T15:45:00Z").expect("reported");
        assert_eq!(d.kind, DivergenceKind::ApprovedStillOpen);
        assert_eq!(d.reviewer, "", "a property of every row, not of one");
        assert_eq!(
            d.stale_secs,
            3 * 3600,
            "dated from the LAST approval, not the first"
        );
    }

    // ── A healthy board reports nothing ──────────────────────────────────────────────────────
    //
    // The other half of the acceptance, and the half that decides whether anyone ever reads the
    // warning. Each case below is a REAL shape from the operator's own board on 2026-09-14.

    /// The live board at 21:46:15Z: PR #164's two reviewers both approved at head `c0a54eb` with
    /// `auto_merge: true` — and the last of those runs ended 27 seconds earlier. Auto-merge has not
    /// had its tick yet, and a sweep that called this diverged would fire on every healthy merge.
    #[test]
    fn a_just_approved_pull_request_is_not_reported() {
        let facts = pr(
            vec![
                row(
                    "alice",
                    REVIEW_STATUS_APPROVED,
                    "STUDIO-893",
                    ran("2026-09-14T21:41:08Z", "2026-09-14T21:45:48Z"),
                    ran("2026-09-14T21:37:55Z", "2026-09-14T21:40:38Z"),
                ),
                row(
                    "jimmy",
                    REVIEW_STATUS_APPROVED,
                    "STUDIO-893",
                    ran("2026-09-14T21:41:08Z", "2026-09-14T21:43:25Z"),
                    ran("2026-09-14T21:37:55Z", "2026-09-14T21:40:38Z"),
                ),
            ],
            true,
        );
        assert_eq!(verdict(&facts, "2026-09-14T21:46:15Z"), None);
    }

    /// The live board's other pull request at the same instant: PR #21's two reviewers both posted
    /// findings, and STUDIO-897 has a run IN FLIGHT that started after the newest verdict. The
    /// author is mid-round — the case the ticket names outright.
    #[test]
    fn a_ticket_mid_round_on_the_findings_is_not_reported() {
        let mk = |reviewer: &str, ended: &str| {
            row(
                reviewer,
                REVIEW_STATUS_REVIEWED,
                "STUDIO-897",
                ran("2026-09-14T21:26:58Z", ended),
                running("2026-09-14T21:35:38Z"),
            )
        };
        let facts = pr(
            vec![
                mk("alice", "2026-09-14T21:32:47Z"),
                mk("jimmy", "2026-09-14T21:35:08Z"),
            ],
            true,
        );
        // Not now, and not in a week: an in-flight run is activity however long it runs.
        assert_eq!(verdict(&facts, "2026-09-14T21:46:15Z"), None);
        assert_eq!(verdict(&facts, "2026-09-21T00:00:00Z"), None);
    }

    /// The ordinary loop, long after the fact: findings landed, the author ran, and that run started
    /// after the verdict. Nothing is owed by anyone.
    #[test]
    fn an_author_who_answered_the_findings_is_not_reported() {
        let facts = pr(
            vec![row(
                "alice",
                REVIEW_STATUS_REVIEWED,
                "STUDIO-870",
                ran("2026-09-12T09:00:00Z", "2026-09-12T09:25:00Z"),
                ran("2026-09-12T09:30:00Z", "2026-09-12T10:10:00Z"),
            )],
            false,
        );
        assert_eq!(verdict(&facts, "2026-09-13T00:00:00Z"), None);
    }

    /// A round IN FLIGHT is never divergence, whatever the other rows of the pull request say — and
    /// however long it has been running. A review that outlives the window is a slow review.
    #[test]
    fn a_review_in_flight_is_not_reported() {
        let facts = pr(
            vec![
                row(
                    "alice",
                    REVIEW_STATUS_IN_FLIGHT,
                    "STUDIO-870",
                    running("2026-09-12T09:00:00Z"),
                    ran("2026-09-12T07:00:00Z", "2026-09-12T08:50:00Z"),
                ),
                // A second row that would otherwise report on its own.
                row(
                    "jimmy",
                    REVIEW_STATUS_REVIEWED,
                    "STUDIO-870",
                    ran("2026-09-12T08:00:00Z", "2026-09-12T08:40:00Z"),
                    ran("2026-09-12T07:00:00Z", "2026-09-12T08:50:00Z"),
                ),
            ],
            false,
        );
        assert_eq!(verdict(&facts, "2026-09-13T00:00:00Z"), None);
    }

    /// With `auto_merge` OFF, approved-and-open is a healthy wait for a HUMAN — the state every
    /// review on such a board ends in. Reporting it would light the advisory permanently, which is
    /// the crying-wolf failure the whole module is careful about.
    #[test]
    fn approved_and_open_is_silent_when_auto_merge_is_off() {
        let rows = vec![row(
            "alice",
            REVIEW_STATUS_APPROVED,
            "STUDIO-877",
            ran("2026-09-12T12:00:00Z", "2026-09-12T12:30:00Z"),
            ran("2026-09-12T10:00:00Z", "2026-09-12T11:50:00Z"),
        )];
        assert_eq!(
            verdict(&pr(rows.clone(), false), "2026-09-14T00:00:00Z"),
            None
        );
        // The SAME rows with auto-merge on do report — so the silence above is the gate, not an
        // accident of the fixture.
        assert!(verdict(&pr(rows, true), "2026-09-14T00:00:00Z").is_some());
    }

    /// Inside the window, nothing is reported — the threshold is a threshold. Checked on both sides
    /// of the exact boundary so it cannot silently become "any staleness at all".
    #[test]
    fn the_threshold_is_a_threshold_not_a_tick() {
        let facts = pr(
            vec![row(
                "alice",
                REVIEW_STATUS_REVIEWED,
                "STUDIO-870",
                ran("2026-09-12T09:00:00Z", "2026-09-12T10:00:00Z"),
                ran("2026-09-12T07:00:00Z", "2026-09-12T08:00:00Z"),
            )],
            false,
        );
        // 89 minutes: silent. 90 exactly: still silent (the comparison is strict).
        assert_eq!(verdict(&facts, "2026-09-12T11:29:00Z"), None);
        assert_eq!(verdict(&facts, "2026-09-12T11:30:00Z"), None);
        // 90 minutes and one second: reported.
        assert_eq!(
            verdict(&facts, "2026-09-12T11:30:01Z")
                .expect("reported")
                .stale_secs,
            90 * 60 + 1
        );
    }

    /// An UNKNOWN is never a divergence. Three ways a row cannot be dated, each reported as nothing
    /// — under-reporting a case nobody can act on is free; crying wolf costs the whole signal.
    #[test]
    fn a_row_that_cannot_be_dated_is_not_reported() {
        // No completed reviewer run to date the verdict from.
        let no_verdict_run = pr(
            vec![row(
                "alice",
                REVIEW_STATUS_REVIEWED,
                "STUDIO-870",
                None,
                ran("2026-09-12T07:00:00Z", "2026-09-12T08:00:00Z"),
            )],
            false,
        );
        assert_eq!(verdict(&no_verdict_run, "2026-09-14T00:00:00Z"), None);

        // A `console:` origin names no ticket, so no run could ever discharge it.
        let no_ticket = pr(
            vec![row(
                "alice",
                REVIEW_STATUS_REVIEWED,
                "",
                ran("2026-09-12T09:00:00Z", "2026-09-12T10:00:00Z"),
                None,
            )],
            false,
        );
        assert_eq!(verdict(&no_ticket, "2026-09-14T00:00:00Z"), None);

        // A `requested` row with no run on either identifier — an empty or pruned ledger.
        let no_runs_at_all = pr(
            vec![row(
                "alice",
                REVIEW_STATUS_REQUESTED,
                "STUDIO-870",
                None,
                None,
            )],
            false,
        );
        assert_eq!(verdict(&no_runs_at_all, "2026-09-14T00:00:00Z"), None);
    }

    /// A row that has left the watch set is not an obligation. Both spellings of gone — the
    /// `dropped` status and `open: false` — because the store can show either.
    #[test]
    fn a_retired_row_is_not_reported() {
        let dropped = RowFacts {
            status: REVIEW_STATUS_DROPPED.to_string(),
            ..row(
                "alice",
                REVIEW_STATUS_DROPPED,
                "STUDIO-870",
                ran("2026-09-12T09:00:00Z", "2026-09-12T10:00:00Z"),
                ran("2026-09-12T07:00:00Z", "2026-09-12T08:00:00Z"),
            )
        };
        assert_eq!(
            verdict(&pr(vec![dropped], true), "2026-09-14T00:00:00Z"),
            None
        );

        let closed = RowFacts {
            open: false,
            ..row(
                "alice",
                REVIEW_STATUS_REVIEWED,
                "STUDIO-870",
                ran("2026-09-12T09:00:00Z", "2026-09-12T10:00:00Z"),
                ran("2026-09-12T07:00:00Z", "2026-09-12T08:00:00Z"),
            )
        };
        assert_eq!(
            verdict(&pr(vec![closed], true), "2026-09-14T00:00:00Z"),
            None
        );
    }

    /// A re-armed row whose reviewer DID run again is silent: the reviewer's run started after the
    /// author's push, which is the activity the rule asks for.
    #[test]
    fn a_reviewer_who_took_the_re_armed_round_is_not_reported() {
        let facts = pr(
            vec![row(
                "alice",
                REVIEW_STATUS_REQUESTED,
                "STUDIO-870",
                // The new round started after the author's fixes landed.
                ran("2026-09-12T12:00:00Z", "2026-09-12T12:40:00Z"),
                ran("2026-09-12T10:00:00Z", "2026-09-12T11:50:00Z"),
            )],
            false,
        );
        assert_eq!(verdict(&facts, "2026-09-14T00:00:00Z"), None);
    }

    /// A row re-armed by a push the reviewer never answered IS reported — the same shape as the
    /// test above with the reviewer's run moved before the arming event, so the two together pin
    /// that the anchor is the LATER of the two runs rather than whichever exists.
    #[test]
    fn a_re_armed_round_nobody_took_is_reported() {
        let facts = pr(
            vec![row(
                "alice",
                REVIEW_STATUS_REQUESTED,
                "STUDIO-870",
                ran("2026-09-12T08:00:00Z", "2026-09-12T08:40:00Z"),
                ran("2026-09-12T10:00:00Z", "2026-09-12T11:50:00Z"),
            )],
            false,
        );
        let d = verdict(&facts, "2026-09-12T14:00:00Z").expect("reported");
        assert_eq!(d.kind, DivergenceKind::ReviewRequestedNoRun);
        assert_eq!(
            d.stale_secs,
            2 * 3600 + 10 * 60,
            "dated from the author's run ending, the later of the two"
        );
    }

    /// One pull request yields at most ONE divergence, and it is the STALEST — three lines about
    /// one pull request is three times the noise for one decision.
    #[test]
    fn at_most_one_divergence_per_pull_request_and_the_stalest_wins() {
        let facts = pr(
            vec![
                row(
                    "alice",
                    REVIEW_STATUS_REVIEWED,
                    "STUDIO-870",
                    ran("2026-09-12T09:00:00Z", "2026-09-12T09:30:00Z"),
                    ran("2026-09-12T07:00:00Z", "2026-09-12T08:00:00Z"),
                ),
                row(
                    "jimmy",
                    REVIEW_STATUS_REVIEWED,
                    "STUDIO-870",
                    ran("2026-09-12T11:00:00Z", "2026-09-12T11:30:00Z"),
                    ran("2026-09-12T07:00:00Z", "2026-09-12T08:00:00Z"),
                ),
            ],
            false,
        );
        let d = verdict(&facts, "2026-09-12T20:00:00Z").expect("reported");
        assert_eq!(d.reviewer, "alice", "the row that has waited longest");
        assert_eq!(d.stale_secs, 10 * 3600 + 30 * 60);
    }

    /// A `truncated` row owes the same round a `requested` one does — the head was read partially at
    /// best, so the obligation is the reviewer's and still outstanding.
    #[test]
    fn a_truncated_round_is_reported_as_a_round_still_owed() {
        let facts = pr(
            vec![row(
                "alice",
                REVIEW_STATUS_TRUNCATED,
                "STUDIO-870",
                ran("2026-09-12T09:00:00Z", "2026-09-12T09:30:00Z"),
                ran("2026-09-12T07:00:00Z", "2026-09-12T08:00:00Z"),
            )],
            false,
        );
        assert_eq!(
            verdict(&facts, "2026-09-12T20:00:00Z")
                .expect("reported")
                .kind,
            DivergenceKind::ReviewRequestedNoRun
        );
    }

    /// An anchor in the FUTURE — a clock adjustment, or a hand-written row — reports nothing rather
    /// than a negative staleness.
    #[test]
    fn a_future_anchor_reports_nothing() {
        let facts = pr(
            vec![row(
                "alice",
                REVIEW_STATUS_REVIEWED,
                "STUDIO-870",
                ran("2026-09-20T09:00:00Z", "2026-09-20T10:00:00Z"),
                ran("2026-09-12T07:00:00Z", "2026-09-12T08:00:00Z"),
            )],
            false,
        );
        assert_eq!(verdict(&facts, "2026-09-12T20:00:00Z"), None);
    }

    /// An empty pull request decides nothing rather than panicking on the `max()` of no runs.
    #[test]
    fn a_pull_request_with_no_live_rows_reports_nothing() {
        assert_eq!(verdict(&pr(Vec::new(), true), "2026-09-14T00:00:00Z"), None);
    }

    /// STUDIO-950: a capacity hold ANNOTATES the report, it does not suppress it. A row the watcher
    /// is holding for want of a global slot is STILL reported — the alarm STUDIO-898 exists to
    /// raise, and at a budget spent for longer than the 90-minute threshold that is the incident —
    /// carrying the hold so [`Orchestrator::set_review_divergences`] can name it and its holder
    /// count. Pinned at the pure rule, which is where the field is copied onto the divergence.
    #[test]
    fn a_capacity_hold_annotates_the_divergence_instead_of_hiding_it() {
        let mut held = row(
            "alice",
            REVIEW_STATUS_REQUESTED,
            "STUDIO-950",
            None,
            // The authoring run, which armed the row; long stale against the real threshold.
            ran("2026-09-14T13:00:00Z", "2026-09-14T13:30:00Z"),
        );
        let hold = CapacityHold {
            holders: 4,
            separate: false,
            recorded: t("2026-09-14T21:00:00Z"),
        };
        held.capacity_held = Some(hold);
        let facts = pr(vec![held], false);

        let d = verdict(&facts, "2026-09-14T21:20:00Z").expect("still reported, not suppressed");
        assert_eq!(d.kind, DivergenceKind::ReviewRequestedNoRun);
        assert_eq!(
            d.capacity_held,
            Some(hold),
            "the hold travels with the report"
        );
    }
}

/// The sweep driven END TO END against a real store: the watch set, the `runs` ledger, the grouping
/// and the two report surfaces.
///
/// Separate from the rule tests above because it proves a different thing. Those drive
/// [`reconcile_pr`] on hand-written facts and would keep passing if nothing ever BUILT those facts
/// from a store — which is the defect shape this daemon has had twice (STUDIO-822, STUDIO-839): a
/// feature whose only production seam is one call site, with every behavioural test green while the
/// feature is dead.
#[cfg(test)]
mod store_tests {
    use std::sync::Arc;

    use rhapsody_config::teams::{Identity, Review, ReviewMode, Teams};
    use rhapsody_store::{
        REVIEW_STATUS_REVIEWED, ReviewWatchKey, ReviewWatchRow, RunEnd, RunStart, Sqlite, StorePath,
    };
    use rhapsody_tracker::fake::Fake;

    use super::*;
    use crate::testsupport::{
        empty_effective, empty_resolved_project, issue, running_entry, set_of,
    };

    const REPO_URL: &str = "git@github.com:makewhatis/rhapsody.git";
    const HEAD: &str = "c0a54eb0000000000000000000000000000000000";

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s)
            .expect("test timestamp")
            .with_timezone(&Utc)
    }

    /// An enabled ticketless daemon with one project. Primed by a selection pass, because every
    /// sweep test after this one simulates a running daemon whose dispatch half has executed; the
    /// un-primed state is its own case — see [`orch_before_first_pass`].
    fn orch(auto_merge: bool, now: &str) -> Orchestrator {
        let o = orch_before_first_pass(auto_merge, now);
        o.human_holds.begin_pass(true);
        o
    }

    /// [`orch`] with the human-hold ledger left un-primed: no selection pass has run, so its
    /// current-label set is an absence of information rather than "no hold" (STUDIO-949 round 11).
    fn orch_before_first_pass(auto_merge: bool, now: &str) -> Orchestrator {
        let tracker = Arc::new(Fake::new());
        let mut eff = empty_effective(tracker.clone());
        eff.active_states = set_of(&["todo"]);
        eff.max_concurrent = 10;
        let mut proj = empty_resolved_project("rhapsody", tracker);
        proj.repo = REPO_URL.to_string();
        eff.projects = vec![proj];
        let mut o = Orchestrator::new("WORKFLOW.md");
        o.eff = Some(eff);
        o.teams = Some(Teams {
            enabled: true,
            review: Review {
                mode: ReviewMode::Ticketless,
                auto_merge,
                ..Review::default()
            },
            roster: vec![Identity {
                name: "alice".to_string(),
                profile: "swe".to_string(),
                ..Identity::default()
            }],
            ..Teams::disabled()
        });
        o.set_store(Arc::new(
            Sqlite::open(StorePath::InMemory).expect("open in-memory store"),
        ));
        let at = t(now);
        o.now = Box::new(move || at);
        o
    }

    /// Seeds one watch row that has been REVIEWED (findings posted) at `HEAD`.
    fn reviewed_row(o: &Orchestrator, reviewer: &str, ticket: &str) {
        reviewed_row_at(o, 164, reviewer, ticket);
    }

    /// As [`reviewed_row`] for an explicit pull request number — the mixed-set advisory test drives
    /// two divergences at once and needs them on distinct pull requests.
    fn reviewed_row_at(o: &Orchestrator, number: i64, reviewer: &str, ticket: &str) {
        let key = ReviewWatchKey {
            owner: "makewhatis".to_string(),
            repo: "rhapsody".to_string(),
            number,
            reviewer: reviewer.to_string(),
        };
        o.store()
            .save_review_watch(ReviewWatchRow {
                key: key.clone(),
                author: "jimmy".to_string(),
                introduced_by: format!("handoff:{ticket}"),
                requested_sha: String::new(),
                last_reviewed_sha: String::new(),
                status: String::new(),
                open: true,
            })
            .expect("seed the row");
        o.store()
            .mark_review_requested(&key, HEAD)
            .expect("requested");
        o.store()
            .mark_review_completed(&key, HEAD, REVIEW_STATUS_REVIEWED)
            .expect("completed");
    }

    /// Seeds one watch row parked `truncated` at `HEAD`: a round that ran and delivered no verdict
    /// (the `max_turns` backstop, or a STUDIO-967 ceiling stop). Its `reviewer_run` is what tells
    /// the two apart.
    fn truncated_row(o: &Orchestrator, reviewer: &str, ticket: &str) {
        let key = ReviewWatchKey {
            owner: "makewhatis".to_string(),
            repo: "rhapsody".to_string(),
            number: 164,
            reviewer: reviewer.to_string(),
        };
        o.store()
            .save_review_watch(ReviewWatchRow {
                key: key.clone(),
                author: "jimmy".to_string(),
                introduced_by: format!("handoff:{ticket}"),
                requested_sha: String::new(),
                last_reviewed_sha: String::new(),
                status: String::new(),
                open: true,
            })
            .expect("seed the row");
        o.store()
            .mark_review_requested(&key, HEAD)
            .expect("requested");
        o.store().mark_review_truncated(&key).expect("truncated");
    }

    /// Seeds one watch row that has been APPROVED at `HEAD` — divergence (b)'s shape, the one
    /// auto-merge itself would be attempting to merge.
    fn approved_row(o: &Orchestrator, reviewer: &str, ticket: &str) {
        let key = ReviewWatchKey {
            owner: "makewhatis".to_string(),
            repo: "rhapsody".to_string(),
            number: 164,
            reviewer: reviewer.to_string(),
        };
        o.store()
            .save_review_watch(ReviewWatchRow {
                key: key.clone(),
                author: "jimmy".to_string(),
                introduced_by: format!("handoff:{ticket}"),
                requested_sha: String::new(),
                last_reviewed_sha: String::new(),
                status: String::new(),
                open: true,
            })
            .expect("seed the row");
        o.store()
            .mark_review_requested(&key, HEAD)
            .expect("requested");
        o.store()
            .mark_review_completed(&key, HEAD, REVIEW_STATUS_APPROVED)
            .expect("completed");
    }

    /// Records one run of `identifier` that has STARTED and not yet ended — the shape of the
    /// summoned author run the budget was just spent on.
    fn run_in_flight(o: &Orchestrator, identifier: &str, started: &str) -> i64 {
        o.store()
            .start_run(RunStart {
                issue_identifier: identifier.to_string(),
                started_at: started.to_string(),
                ..Default::default()
            })
            .expect("start_run")
    }

    /// Records one finished run of `identifier`.
    fn run(o: &Orchestrator, identifier: &str, started: &str, ended: &str) {
        run_outcome(o, identifier, started, ended, "completed");
    }

    /// As [`run`] with an explicit terminal outcome — for the STUDIO-967 ceiling stop.
    fn run_outcome(o: &Orchestrator, identifier: &str, started: &str, ended: &str, outcome: &str) {
        let id = o
            .store()
            .start_run(RunStart {
                issue_identifier: identifier.to_string(),
                started_at: started.to_string(),
                ..Default::default()
            })
            .expect("start_run");
        o.store()
            .end_run(
                id,
                RunEnd {
                    outcome: outcome.to_string(),
                    ended_at: ended.to_string(),
                    ..Default::default()
                },
            )
            .expect("end_run");
    }

    /// STUDIO-893's shape, through the real store: a verdict asking for changes, six hours, no run
    /// on the ticket since. Asserts on BOTH report surfaces, because the log alone is what cost
    /// eleven hours on STUDIO-875.
    #[test]
    fn a_diverged_pull_request_is_reported_on_both_surfaces() {
        let o = &mut orch(false, "2026-09-14T21:20:00Z");
        reviewed_row(o, "alice", "STUDIO-893");
        // The reviewer's run, which produced the verdict.
        run(
            o,
            &review_key("makewhatis", "rhapsody", 164, "alice"),
            "2026-09-14T14:50:00Z",
            "2026-09-14T15:20:00Z",
        );
        // The authoring run, which ended BEFORE the review — so nothing has answered the findings.
        run(
            o,
            "STUDIO-893",
            "2026-09-14T13:00:00Z",
            "2026-09-14T14:40:00Z",
        );

        o.reconcile_review_divergence();

        let found = o.review_divergences();
        assert_eq!(found.len(), 1, "one divergence, got {found:?}");
        assert_eq!(found[0].pr, "makewhatis/rhapsody#164");
        assert_eq!(found[0].kind, DivergenceKind::ChangesRequestedNoRun);
        assert_eq!(
            found[0].ticket, "STUDIO-893",
            "the origin ticket, resolved from `handoff:STUDIO-893`"
        );
        assert_eq!(found[0].stale_secs, 6 * 3600);

        // Surface one: the per-project advisory.
        let projects = o.project_statuses();
        assert!(
            projects
                .iter()
                .any(|p| p.warnings.iter().any(|w| w == REVIEW_DIVERGENCE_WARNING)),
            "the advisory must reach /api/v1/projects, got {projects:?}"
        );
        // Surface two: the detail on /api/v1/state.
        let rendered = crate::snapshot_json::render(&o.build_snapshot());
        assert_eq!(rendered["review_divergence"][0]["ticket"], "STUDIO-893");
    }

    /// STUDIO-950: a capacity annotation is a REPORT TRANSITION, not a repeat. A pull request that
    /// first crosses the sweep's threshold with no hold logs the plain "nothing has reported it
    /// blocked" line; when the NEXT sweep sees the watcher now holding that round, the enriched
    /// "held for capacity" line must be emitted on THAT sweep, not one `RECONCILE_LOG_EVERY` window
    /// (~30 minutes at the default cadence) later. Without this, the second sweep updates the
    /// in-memory divergence silently and the false page stands for half an hour — the very defect
    /// the ticket closes, reintroduced on the second sweep.
    ///
    /// Mutation check: drop `annotation_changed` from the log condition and the second reconciliation
    /// emits no WARN at all, because `sweeps` is 2 and `RECONCILE_LOG_EVERY` is 60.
    #[test]
    fn a_newly_held_round_is_logged_on_the_sweep_that_learned_it() {
        let o = &mut orch(false, "2026-09-14T21:20:00Z");
        reviewed_row(o, "alice", "STUDIO-893");
        run(
            o,
            &review_key("makewhatis", "rhapsody", 164, "alice"),
            "2026-09-14T14:50:00Z",
            "2026-09-14T15:20:00Z",
        );
        run(
            o,
            "STUDIO-893",
            "2026-09-14T13:00:00Z",
            "2026-09-14T14:40:00Z",
        );

        let id = review_key("makewhatis", "rhapsody", 164, "alice");
        let hold = CapacityHold {
            holders: 4,
            separate: false,
            recorded: t("2026-09-14T21:20:00Z"),
        };

        // TRA-243: this test asserts on TWO `tracing` callsites — the plain line and the capacity
        // line — and `tracing` caches per-callsite Interest GLOBALLY, so a callsite whose first hit
        // races a concurrently-running subscriber can be cached `never` and then drop its events.
        // A warm-up capture pass under the same lock registers BOTH callsites against a capturing
        // subscriber before the assertions below. Reset afterwards so the real run starts from a
        // clean crossing with no hold.
        let _ = crate::testsupport::capture_events(|| {
            o.reconcile_review_divergence(); // the plain callsite
            o.review_capacity_held.insert(id.clone(), hold);
            o.reconcile_review_divergence(); // the capacity callsite
        });
        o.review_divergent.clear();
        o.review_capacity_held.clear();

        let (_, events) = crate::testsupport::capture_events(|| {
            // Sweep 1: the obligation is stale, but the watcher is holding nothing — the plain line.
            o.reconcile_review_divergence();
            // The watcher's next tick defers the round for want of a slot and records the hold.
            o.review_capacity_held.insert(id.clone(), hold);
            // Sweep 2: the hold is newly known, so it must be reported NOW.
            o.reconcile_review_divergence();
            // Sweep 3: the holder count CHANGED. That is a transition too — the operator tuning the
            // budget needs the new number, not a line that still says four.
            o.review_capacity_held.insert(
                id.clone(),
                CapacityHold {
                    holders: 2,
                    separate: false,
                    recorded: t("2026-09-14T21:20:00Z"),
                },
            );
            o.reconcile_review_divergence();
        });

        let review_warns: Vec<&crate::testsupport::CapturedEvent> = events
            .iter()
            .filter(|e| e.message.contains("review reconciliation"))
            .collect();
        assert_eq!(
            review_warns.len(),
            3,
            "every transition must report, got: {review_warns:?}"
        );
        assert!(
            review_warns[0]
                .message
                .contains("nothing has reported it blocked"),
            "the first sweep knows of no hold, got: {}",
            review_warns[0].message
        );
        assert!(
            review_warns[1].message.contains("held for capacity"),
            "the sweep that LEARNED the hold must say so immediately, got: {}",
            review_warns[1].message
        );
        assert!(
            review_warns[2].message.contains("2 run(s) hold"),
            "a changed holder count must be reported on the sweep that saw it, got: {}",
            review_warns[2].message
        );
    }

    /// STUDIO-957: a divergence whose subject is BUDGET-held must be named by that cause, not fall
    /// through to the false "nothing has reported it blocked". The watcher recorded the refusal when
    /// it deferred the round for the spent provider budget, so the sweep repeats a fact this process
    /// already has rather than inventing a cause.
    ///
    /// Mutation check: drop the `(None, Some(hold), _)` arm in
    /// [`Orchestrator::set_review_divergences`] and this reds on the fallback wording.
    #[test]
    fn a_budget_held_round_is_reported_as_held_for_budget() {
        let o = &mut orch(false, "2026-09-14T21:20:00Z");
        reviewed_row(o, "alice", "STUDIO-893");
        run(
            o,
            &review_key("makewhatis", "rhapsody", 164, "alice"),
            "2026-09-14T14:50:00Z",
            "2026-09-14T15:20:00Z",
        );
        run(
            o,
            "STUDIO-893",
            "2026-09-14T13:00:00Z",
            "2026-09-14T14:40:00Z",
        );
        // The reviewer's provider is out of daily budget; the watcher recorded it against the
        // reviewer's identity, with the PR coordinate carried for this lookup.
        o.note_review_budget_hold(
            "pr:makewhatis/rhapsody#164@alice",
            "makewhatis/rhapsody#164",
            "core",
            "anthropic",
            400_000_000,
            410_000_000,
        );

        let (_, events) = crate::testsupport::capture_events(|| {
            o.reconcile_review_divergence();
        });
        let warns: Vec<&crate::testsupport::CapturedEvent> = events
            .iter()
            .filter(|e| e.message.contains("review reconciliation"))
            .collect();
        assert_eq!(warns.len(), 1, "one divergence, one line: {warns:?}");
        let msg = &warns[0].message;
        assert!(
            msg.contains("held for budget"),
            "the sweep must name the budget, not a stall: {msg}"
        );
        assert!(
            msg.contains("anthropic") && msg.contains("410000000"),
            "the line must carry the provider and the two figures: {msg}"
        );
        assert!(
            !msg.contains("nothing has reported it blocked"),
            "the false fallback wording must not be used: {msg}"
        );
    }

    /// STUDIO-950 (round 18, jimmy's blocking finding): a hold the watcher DENIED because GitHub
    /// stopped answering for its coordinate must not fall through to "nothing has reported it
    /// blocked" — false, and false BECAUSE of the branch that produced it. The watcher knows exactly
    /// what is wrong, so the sweep names it: the state could not be read for N consecutive attempts.
    ///
    /// Mutation check: delete the `capacity_unreadable` arm in [`Orchestrator::set_review_divergences`]
    /// and this reds on the fallback wording (and on the advisory assertion, which reverts to the
    /// plain "nothing has reported it blocked" string).
    #[test]
    fn a_round_github_stopped_answering_for_is_reported_as_unreadable() {
        let o = &mut orch(false, "2026-09-14T21:20:00Z");
        reviewed_row(o, "alice", "STUDIO-893");
        run(
            o,
            &review_key("makewhatis", "rhapsody", 164, "alice"),
            "2026-09-14T14:50:00Z",
            "2026-09-14T15:20:00Z",
        );
        run(
            o,
            "STUDIO-893",
            "2026-09-14T13:00:00Z",
            "2026-09-14T14:40:00Z",
        );
        // The watcher is still holding the round for capacity, but its coordinate has failed
        // `gh` lookups for enough consecutive attempts that the hold is no longer a wait anyone is
        // confirming.
        o.review_capacity_held.insert(
            review_key("makewhatis", "rhapsody", 164, "alice"),
            CapacityHold {
                holders: 4,
                separate: false,
                recorded: t("2026-09-14T21:20:00Z"),
            },
        );
        o.review_watch_unreadable.insert(
            PrCoord::new("makewhatis", "rhapsody", 164),
            UNREADABLE_ATTEMPTS_TO_DROP_HOLD,
        );

        // TRA-243: register the unreadable callsite (new here) against a capturing subscriber before
        // the real run, then reset the sweep counter so the assertion below is the FIRST crossing.
        let _ = crate::testsupport::capture_events(|| o.reconcile_review_divergence());
        o.review_divergent.clear();

        let (_, events) = crate::testsupport::capture_events(|| o.reconcile_review_divergence());
        let warn = events
            .iter()
            .find(|e| e.level == "WARN")
            .unwrap_or_else(|| panic!("no WARN fired: {events:?}"));
        assert!(
            warn.message.contains("could not be read"),
            "the sweep must name GitHub's silence, got: {}",
            warn.message
        );
        assert!(
            !warn.message.contains("nothing has reported it blocked"),
            "the unreadable line replaces the fallback wording, not sits beside it: {}",
            warn.message
        );

        // The project advisory makes the same claim and must stop making it too.
        let projects = o.project_statuses();
        assert!(
            projects.iter().any(|p| p
                .warnings
                .iter()
                .any(|w| w == REVIEW_DIVERGENCE_UNREADABLE_WARNING)),
            "the advisory must name the unreadable coordinate, got {projects:?}"
        );
        assert!(
            !projects
                .iter()
                .any(|p| p.warnings.iter().any(|w| w == REVIEW_DIVERGENCE_WARNING)),
            "it must not also claim nothing has reported it blocked, got {projects:?}"
        );

        // Alice's round-22 finding: the rendered row is where "the two annotations are never both"
        // is a real question, because this sweep sets a hold AND a denial. `fresh_capacity_hold`
        // refuses a coordinate whose lookups have been failing, so the row must carry the denial and
        // no `capacity_held` — pinned end-to-end (sweep -> render) on the production path, which the
        // hand-built fixture in `snapshot_json` could not exercise.
        let rendered = crate::snapshot_json::render(&o.build_snapshot());
        assert_eq!(
            rendered["review_divergence"][0]["capacity_unreadable"]["attempts"],
            UNREADABLE_ATTEMPTS_TO_DROP_HOLD,
            "the denial reaches the state row, got: {rendered}"
        );
        assert!(
            rendered["review_divergence"][0]
                .get("capacity_held")
                .is_none(),
            "a denied hold is not a live hold — the row must not carry both, got: {rendered}"
        );
    }

    /// STUDIO-950 (round 21, sol's blocking finding at `71c02b3`, re-derived from alice's round 20):
    /// the unreadable denial is a REPORT TRANSITION too. A row that first crosses the sweep's
    /// threshold with NO annotation logs the plain "nothing has reported it blocked" line; when the
    /// NEXT sweep learns its coordinate has gone unreadable, the advisory flips to the unreadable
    /// string but the log said nothing — the false page, reintroduced on the second sweep. `sweeps`
    /// is 2 there, so the repeat clock is no help; the transition test must see the annotation.
    ///
    /// Mutation check: compare only the hold in `annotation_changed` (as at `71c02b3`) and the
    /// second sweep emits no WARN at all, because `RECONCILE_LOG_EVERY` is 60 and `sweeps - 1` is 1.
    #[test]
    fn an_unreadable_denial_learned_on_a_later_sweep_is_logged() {
        let o = &mut orch(false, "2026-09-14T21:20:00Z");
        reviewed_row(o, "alice", "STUDIO-893");
        run(
            o,
            &review_key("makewhatis", "rhapsody", 164, "alice"),
            "2026-09-14T14:50:00Z",
            "2026-09-14T15:20:00Z",
        );
        run(
            o,
            "STUDIO-893",
            "2026-09-14T13:00:00Z",
            "2026-09-14T14:40:00Z",
        );

        // TRA-243: register BOTH the plain and the unreadable callsites against a capturing
        // subscriber before the real run, then start from a clean crossing with no annotation.
        let _ = crate::testsupport::capture_events(|| {
            o.reconcile_review_divergence(); // the plain callsite and the crossing
            o.review_watch_unreadable.insert(
                PrCoord::new("makewhatis", "rhapsody", 164),
                UNREADABLE_ATTEMPTS_TO_DROP_HOLD,
            );
            o.reconcile_review_divergence(); // the unreadable callsite
        });
        o.review_divergent.clear();
        o.review_watch_unreadable.clear();
        // Sol's round-22 finding: the warm-up's last call left the unreadable annotation on
        // `review_divergence`, so without this the real "sweep 1" would see `prev_unreadable == true`
        // and log because the annotation DISAPPEARS — passing for the wrong reason. Clear it so sweep
        // 1 crosses genuinely clean, and the test pins the fresh crossing (mutating
        // `annotation_changed` to the hold-only compare reds it) rather than an annotation's removal.
        o.review_divergence.clear();

        let (_, events) = crate::testsupport::capture_events(|| {
            // Sweep 1: stale, with no annotation of any kind — the plain line.
            o.reconcile_review_divergence();
            // GitHub stops answering for the coordinate before the next sweep.
            o.review_watch_unreadable.insert(
                PrCoord::new("makewhatis", "rhapsody", 164),
                UNREADABLE_ATTEMPTS_TO_DROP_HOLD,
            );
            // Sweep 2: the denial is newly known, so it must be reported NOW — not one
            // `RECONCILE_LOG_EVERY` window later.
            o.reconcile_review_divergence();
        });

        let review_warns: Vec<&crate::testsupport::CapturedEvent> = events
            .iter()
            .filter(|e| e.message.contains("review reconciliation"))
            .collect();
        assert_eq!(
            review_warns.len(),
            2,
            "every transition must report, got: {review_warns:?}"
        );
        assert!(
            review_warns[0]
                .message
                .contains("nothing has reported it blocked"),
            "the first sweep knows of nothing, got: {}",
            review_warns[0].message
        );
        assert!(
            review_warns[1].message.contains("could not be read"),
            "the sweep that LEARNED the denial must say so immediately, got: {}",
            review_warns[1].message
        );
    }

    /// STUDIO-950 (round 10): [`same_capacity`] deliberately EXCLUDES [`CapacityHold::recorded`],
    /// because the watcher re-stamps it on every sweep — comparing it would make every sweep look
    /// like a transition and defeat the reconciliation log's rate limit. That decision was
    /// documented but UNPINNED: adding `&& x.recorded == y.recorded` left the whole suite green.
    ///
    /// Mutation check: add `&& x.recorded == y.recorded` to `same_capacity` and this logs a third
    /// WARN, red.
    #[test]
    fn a_restamped_hold_with_an_unchanged_count_is_not_a_transition() {
        let o = &mut orch(false, "2026-09-14T21:20:00Z");
        reviewed_row(o, "alice", "STUDIO-893");
        run(
            o,
            &review_key("makewhatis", "rhapsody", 164, "alice"),
            "2026-09-14T14:50:00Z",
            "2026-09-14T15:20:00Z",
        );
        run(
            o,
            "STUDIO-893",
            "2026-09-14T13:00:00Z",
            "2026-09-14T14:40:00Z",
        );

        let id = review_key("makewhatis", "rhapsody", 164, "alice");
        let hold_at = |recorded: &str| CapacityHold {
            holders: 4,
            separate: false,
            recorded: t(recorded),
        };

        // Register both callsites before the real run (TRA-243), then start from a clean crossing.
        let _ = crate::testsupport::capture_events(|| {
            o.reconcile_review_divergence(); // the plain callsite
            o.review_capacity_held
                .insert(id.clone(), hold_at("2026-09-14T21:00:00Z"));
            o.reconcile_review_divergence(); // the capacity callsite
        });
        o.review_divergent.clear();
        o.review_capacity_held.clear();

        let (_, events) = crate::testsupport::capture_events(|| {
            // Sweep 1: no hold — the plain line.
            o.reconcile_review_divergence();
            // Sweep 2: the watcher records the hold — the capacity line.
            o.review_capacity_held
                .insert(id.clone(), hold_at("2026-09-14T21:00:00Z"));
            o.reconcile_review_divergence();
            // Sweep 3: the watcher RE-STAMPS the hold with the same holder count (its every-sweep
            // refresh). Not a transition — the number an operator reads is unchanged.
            o.review_capacity_held
                .insert(id.clone(), hold_at("2026-09-14T21:10:00Z"));
            o.reconcile_review_divergence();
        });

        let warns: Vec<&crate::testsupport::CapturedEvent> = events
            .iter()
            .filter(|e| e.message.contains("review reconciliation"))
            .collect();
        assert_eq!(
            warns.len(),
            2,
            "a re-stamped hold with an unchanged count is not a transition, got: {warns:?}"
        );
    }

    /// STUDIO-950: when the reported divergence is HELD for capacity, the project advisory must stop
    /// claiming nothing has reported it blocked. It names the deliberate wait instead, while the
    /// state row says which pull request and with what holder count — the two surfaces the ticket
    /// says must not keep making the false claim.
    ///
    /// Mutation check: revert the advisory selection to always push `REVIEW_DIVERGENCE_WARNING` and
    /// the first assertion reds (the false claim is back).
    #[test]
    fn a_capacity_held_divergence_changes_the_project_advisory() {
        let o = &mut orch(false, "2026-09-14T21:20:00Z");
        reviewed_row(o, "alice", "STUDIO-893");
        run(
            o,
            &review_key("makewhatis", "rhapsody", 164, "alice"),
            "2026-09-14T14:50:00Z",
            "2026-09-14T15:20:00Z",
        );
        run(
            o,
            "STUDIO-893",
            "2026-09-14T13:00:00Z",
            "2026-09-14T14:40:00Z",
        );
        o.review_capacity_held.insert(
            review_key("makewhatis", "rhapsody", 164, "alice"),
            CapacityHold {
                holders: 4,
                separate: false,
                recorded: t("2026-09-14T21:20:00Z"),
            },
        );

        o.reconcile_review_divergence();

        let projects = o.project_statuses();
        assert!(
            projects.iter().any(|p| p
                .warnings
                .iter()
                .any(|w| w == REVIEW_DIVERGENCE_CAPACITY_WARNING)),
            "the advisory must name the capacity hold, got {projects:?}"
        );
        assert!(
            !projects
                .iter()
                .any(|p| p.warnings.iter().any(|w| w == REVIEW_DIVERGENCE_WARNING)),
            "it must not also claim nothing has reported it blocked, got {projects:?}"
        );
        // The state row carries the detail the fixed advisory string cannot.
        let rendered = crate::snapshot_json::render(&o.build_snapshot());
        assert_eq!(
            rendered["review_divergence"][0]["capacity_held"]["holders"],
            4
        );
    }

    /// STUDIO-950 (round 10): the two divergence advisories are INDEPENDENT. A reported set can hold
    /// both a capacity-held round AND a genuinely unexplained stall at once, and selecting one
    /// string for the whole set would re-label the stall as a deliberate capacity wait — the mirror
    /// image of the false claim this ticket closes, and exactly what the STUDIO-898 surface exists
    /// to avoid. Each string is pushed on its own evidence, so a mixed set carries both.
    ///
    /// Mutation check: restore the either/or selection (`if review_held { capacity } else { plain }`)
    /// and the plain-warning assertion reds while the capacity one stays green.
    #[test]
    fn a_mixed_divergence_set_carries_both_advisories() {
        let o = &mut orch(false, "2026-09-14T21:20:00Z");
        // #164: held for capacity by the review watcher.
        reviewed_row(o, "alice", "STUDIO-893");
        run(
            o,
            &review_key("makewhatis", "rhapsody", 164, "alice"),
            "2026-09-14T14:50:00Z",
            "2026-09-14T15:20:00Z",
        );
        run(
            o,
            "STUDIO-893",
            "2026-09-14T13:00:00Z",
            "2026-09-14T14:40:00Z",
        );
        o.review_capacity_held.insert(
            review_key("makewhatis", "rhapsody", 164, "alice"),
            CapacityHold {
                holders: 4,
                separate: false,
                recorded: t("2026-09-14T21:20:00Z"),
            },
        );
        // #165: the same stale shape, holding nothing — the unexplained stall with no known cause.
        reviewed_row_at(o, 165, "jimmy", "STUDIO-899");
        run(
            o,
            &review_key("makewhatis", "rhapsody", 165, "jimmy"),
            "2026-09-14T14:50:00Z",
            "2026-09-14T15:20:00Z",
        );
        run(
            o,
            "STUDIO-899",
            "2026-09-14T13:00:00Z",
            "2026-09-14T14:40:00Z",
        );

        o.reconcile_review_divergence();
        assert_eq!(o.review_divergences().len(), 2, "both rows report");

        let projects = o.project_statuses();
        assert!(
            projects.iter().any(|p| p
                .warnings
                .iter()
                .any(|w| w == REVIEW_DIVERGENCE_CAPACITY_WARNING)),
            "the held round's advisory must reach /api/v1/projects, got {projects:?}"
        );
        assert!(
            projects
                .iter()
                .any(|p| p.warnings.iter().any(|w| w == REVIEW_DIVERGENCE_WARNING)),
            "the unexplained stall must keep its warning alongside the capacity one, got {projects:?}"
        );
    }

    /// STUDIO-949: the shape above, but with the origin ticket CURRENTLY held for a human. The row
    /// exists (it was armed by an earlier run) yet the obligation is a deliberate hold, not a stall,
    /// so the sweep must report nothing — on either surface.
    ///
    /// The fixture seeds the CURRENT-LABEL-only state (`note_human_label`, which is what the
    /// selection pass records for a candidate wearing the label while its run is still live) rather
    /// than `hold` (which feeds the reported subset too). That is the state the two sets disagree
    /// on, so only this fixture pins the sweep to the live-inclusive signal; seeding `hold` passes
    /// against either reader.
    ///
    /// MUTATION: delete the held-origin filter from `reconcile_review_divergence` and this reds;
    /// read the reported `held()` set instead of `labelled()` and this reds too.
    #[test]
    fn a_held_ticket_is_not_reported_as_a_stall() {
        let o = &mut orch(false, "2026-09-14T21:20:00Z");
        reviewed_row(o, "alice", "STUDIO-893");
        run(
            o,
            &review_key("makewhatis", "rhapsody", 164, "alice"),
            "2026-09-14T14:50:00Z",
            "2026-09-14T15:20:00Z",
        );
        // The authoring run ended BEFORE the review, which is the divergence the test above pins.
        run(
            o,
            "STUDIO-893",
            "2026-09-14T13:00:00Z",
            "2026-09-14T14:40:00Z",
        );
        o.human_holds.note_human_label("STUDIO-893");

        o.reconcile_review_divergence();

        assert!(
            o.review_divergences().is_empty(),
            "a held ticket is a deliberate hold, not a stalled obligation"
        );
        assert!(
            o.project_statuses()
                .iter()
                .all(|p| !p.warnings.iter().any(|w| w == REVIEW_DIVERGENCE_WARNING))
        );
    }

    /// ⚠️ STUDIO-949 round 11: while the human-hold ledger has never been primed by a selection
    /// pass, the sweep reports NOTHING — even a genuinely diverged row with no hold on it. The
    /// current-label set has no writer above `on_tick`'s three early gates, and this sweep runs
    /// ABOVE them on purpose, so on a daemon held by a bad config, an armed drain or a dead
    /// credential the set is empty for the whole process lifetime. With no pass having looked, "the
    /// row is not held" is not a fact the sweep can assert, and publishing a `review_divergence` WARN
    /// for a held ticket is the false alarm the filter exists to prevent.
    ///
    /// `a_diverged_pull_request_is_reported_on_both_surfaces` is the live control: the SAME fixture
    /// through a primed daemon reports on both surfaces.
    ///
    /// MUTATION: drop the un-primed (`!ledger_primed`) branch from `reconcile_review_divergence` and this reds (a
    /// divergence is published).
    #[test]
    fn an_unprimed_hold_ledger_reports_no_divergence() {
        let o = &mut orch_before_first_pass(false, "2026-09-14T21:20:00Z");
        reviewed_row(o, "alice", "STUDIO-893");
        run(
            o,
            &review_key("makewhatis", "rhapsody", 164, "alice"),
            "2026-09-14T14:50:00Z",
            "2026-09-14T15:20:00Z",
        );
        run(
            o,
            "STUDIO-893",
            "2026-09-14T13:00:00Z",
            "2026-09-14T14:40:00Z",
        );

        o.reconcile_review_divergence();

        assert!(
            o.review_divergences().is_empty(),
            "with no pass having looked, the sweep cannot tell a hold from a stall"
        );

        // Once a pass has run, the same row is reported again.
        o.human_holds.begin_pass(true);
        o.reconcile_review_divergence();
        assert_eq!(
            o.review_divergences().len(),
            1,
            "a primed sweep reports the divergence"
        );
    }

    /// **STUDIO-956.** A watched pull request whose shared review↔author budget is spent is
    /// reported, with its reason, immediately — not after [`RECONCILE_STALE_AFTER`], because a spent
    /// budget never resolves on its own, and not as the "nothing has reported it blocked" copy,
    /// which is false here.
    ///
    /// Mutation check (the ticket's ⚠️): demoting the exhausted-budget line to DEBUG leaves no WARN
    /// for this capture and reds it. That is deliberate — the ticket asks for a stop the sweep
    /// SURFACES, and a DEBUG line is the silent cap that already pages a human as an idle board.
    #[test]
    fn a_spent_round_budget_is_reported_on_both_surfaces() {
        let o = &mut orch(false, "2026-09-14T21:20:00Z");
        reviewed_row(o, "alice", "STUDIO-170");
        // The findings that would summon the author, landed a MINUTE ago: far inside the staleness
        // threshold, so the staleness rules report nothing and only the budget rule can.
        run(
            o,
            &review_key("makewhatis", "rhapsody", 164, "alice"),
            "2026-09-14T21:10:00Z",
            "2026-09-14T21:19:00Z",
        );
        // The loop has spent its whole shared budget.
        o.review_rounds.insert(
            crate::reviewwatch::churn_key(&PrCoord::new("makewhatis", "rhapsody", 164)),
            crate::reviewwatch::REVIEW_ROUNDS_PER_PR_CAP,
        );

        // See the sibling tests above for why the warm-up call exists: this WARN callsite is new,
        // and an uncaptured first hit can race a test running on another thread.
        o.reconcile_review_divergence();
        o.review_divergent.clear();

        let (_, events) = crate::testsupport::capture_events(|| o.reconcile_review_divergence());

        let warn = events
            .iter()
            .find(|e| e.level == "WARN")
            .unwrap_or_else(|| panic!("no WARN fired: {events:?}"));
        assert!(
            warn.message.contains("review round budget is spent"),
            "the line must name the reason, got: {}",
            warn.message
        );
        assert!(
            !warn.message.contains("nothing has reported it blocked"),
            "the copy that was false about a capped pull request must not be reused: {}",
            warn.message
        );
        // Round-8 finding 3: this fixture is an install with NO `review.adjudicate_after_rounds`,
        // where the author half is deliberately unbounded. The line must not claim it is stopped —
        // that sentence was false in exactly the incident it printed in.
        //
        // MUTATION: restore "no further review or author re-run will be dispatched" to
        // `DivergenceKind::detail` and this reds.
        assert!(
            o.adjudication_threshold_for_test().is_none(),
            "the fixture must be the unset install this assertion is about"
        );
        assert!(
            !warn.message.contains("author"),
            "an unset install's author half is unbounded; the line must not say otherwise: {}",
            warn.message
        );

        let found = o.review_divergences();
        assert_eq!(found.len(), 1, "one divergence, got {found:?}");
        assert_eq!(found[0].kind, DivergenceKind::RoundBudgetExhausted);
        assert_eq!(found[0].pr, "makewhatis/rhapsody#164");
        assert_eq!(found[0].ticket, "STUDIO-170");
        assert!(
            !found[0].kind.detail().contains("author"),
            "and neither must the console/`/api/v1/state` copy: {}",
            found[0].kind.detail()
        );

        // Surface one: the per-project advisory.
        let projects = o.project_statuses();
        assert!(
            projects
                .iter()
                .any(|p| p.warnings.iter().any(|w| w == REVIEW_DIVERGENCE_WARNING)),
            "the advisory must reach /api/v1/projects, got {projects:?}"
        );
        // Surface two: the detail on /api/v1/state.
        let rendered = crate::snapshot_json::render(&o.build_snapshot());
        assert_eq!(
            rendered["review_divergence"][0]["kind"],
            "round_budget_exhausted"
        );
    }

    /// STUDIO-967, through the real store: a verdict asked for changes, the author DID run after it,
    /// and that run was stopped at its per-run token ceiling. Without the ceiling kind the author run
    /// reads as progress and the pull request reports NOTHING — the silent stall the ticket forbids.
    /// Asserts on BOTH report surfaces, like the incidents above.
    #[test]
    fn a_ceiling_stopped_author_run_is_reported_on_both_surfaces() {
        // Well past the 90-minute threshold measured from the VERDICT, so the report is due rather
        // than early: verdict ended 21:00, now 23:30.
        let o = &mut orch(false, "2026-09-21T23:30:00Z");
        reviewed_row(o, "alice", "STUDIO-967");
        run(
            o,
            &review_key("makewhatis", "rhapsody", 164, "alice"),
            "2026-09-21T20:00:00Z",
            "2026-09-21T21:00:00Z",
        );
        // The author moved — and was stopped at the ceiling.
        run_outcome(
            o,
            "STUDIO-967",
            "2026-09-21T21:05:00Z",
            "2026-09-21T21:10:00Z",
            rhapsody_store::OUTCOME_TOKEN_CEILING,
        );

        // Warm-up so the first WARN is captured deterministically (see the sibling tests).
        o.reconcile_review_divergence();
        o.review_divergent.clear();

        let (_, events) = crate::testsupport::capture_events(|| o.reconcile_review_divergence());

        let warn = events
            .iter()
            .find(|e| e.level == "WARN")
            .unwrap_or_else(|| panic!("no WARN fired: {events:?}"));
        assert!(
            warn.message.contains("token ceiling"),
            "the line must name the ceiling, got: {}",
            warn.message
        );
        assert!(
            !warn.message.contains("nothing has reported it blocked"),
            "the copy that was false about a ceiling stop must not be reused: {}",
            warn.message
        );

        let found = o.review_divergences();
        assert_eq!(found.len(), 1, "one divergence, got {found:?}");
        assert_eq!(found[0].kind, DivergenceKind::AuthorTokenCeilingStopped);
        assert_eq!(found[0].pr, "makewhatis/rhapsody#164");
        assert_eq!(found[0].ticket, "STUDIO-967");

        // Surface one: the per-project advisory.
        let projects = o.project_statuses();
        assert!(
            projects
                .iter()
                .any(|p| p.warnings.iter().any(|w| w == REVIEW_DIVERGENCE_WARNING)),
            "the advisory must reach /api/v1/projects, got {projects:?}"
        );
        // Surface two: the detail on /api/v1/state.
        let rendered = crate::snapshot_json::render(&o.build_snapshot());
        assert_eq!(
            rendered["review_divergence"][0]["kind"],
            "author_token_ceiling_stopped"
        );
    }

    /// STUDIO-967, end to end through the two halves: the CEILING actually stops a live run (not a
    /// hand-seeded row), and the sweep then reports it. MUTATION: make the stop log-only (do not
    /// record `token_ceiling` on the run) and the sweep sees an in-flight roomy run and reports
    /// nothing, so this reds; delete the sweep rule and it reds too.
    #[test]
    fn the_ceiling_stop_and_the_sweep_agree_end_to_end() {
        let o = &mut orch(false, "2026-09-21T23:30:00Z");
        o.eff.as_mut().expect("eff").max_run_tokens = 1_000;
        reviewed_row(o, "alice", "STUDIO-967");
        // The reviewer's run, which produced the verdict that put the author on the clock.
        run(
            o,
            &review_key("makewhatis", "rhapsody", 164, "alice"),
            "2026-09-21T20:00:00Z",
            "2026-09-21T21:00:00Z",
        );

        // A LIVE author run for the ticket, started after the verdict, that blows the ceiling on a
        // single in-flight usage event.
        let mut re = running_entry(
            issue("ID-967", "STUDIO-967", "In Progress"),
            "rhapsody",
            "rhapsody",
        );
        // Arm the signal, as every real dispatch does: the ceiling refuses to record a stop it
        // cannot deliver (STUDIO-840), so an unarmed fixture would not stop at all.
        re.cancel = crate::CancelSignal::new();
        re.started_at = t("2026-09-21T21:05:00Z");
        o.persist_start_run(&mut re, 0);
        o.running.insert("ID-967".into(), re);
        o.on_agent_update(crate::agentupdate::AgentUpdate {
            issue_id: "ID-967".into(),
            ev: rhapsody_agent::Event {
                event_type: rhapsody_agent::EVENT_NOTIFICATION.to_string(),
                usage: Some(rhapsody_agent::Usage {
                    input_tokens: 44_743_645,
                    total_tokens: 44_743_645,
                    ..Default::default()
                }),
                ..Default::default()
            },
        });
        assert!(
            !o.running.contains_key("ID-967"),
            "the live runaway run must have been stopped"
        );

        // The sweep reads the run the stop WROTE and reports the cause.
        o.reconcile_review_divergence();
        let found = o.review_divergences();
        assert_eq!(found.len(), 1, "one divergence, got {found:?}");
        assert_eq!(found[0].kind, DivergenceKind::AuthorTokenCeilingStopped);
    }

    /// STUDIO-967's REVIEW half, through the real store — alice's finding on PR #208. A ticketless
    /// review stopped at the ceiling parks its row `truncated` and its run carries
    /// `token_ceiling`; the sweep must name the ceiling on BOTH surfaces, not the
    /// false `review_requested_no_run` ("no reviewer run has started"). MUTATION: drop the
    /// `ReviewTokenCeilingStopped` arm from `row_owed` and the row reports
    /// `ReviewRequestedNoRun` — with the generic "nothing has reported it blocked" copy — so this
    /// reds on both the kind and the log line.
    #[test]
    fn a_ceiling_stopped_review_round_is_reported_on_both_surfaces() {
        let o = &mut orch(false, "2026-09-21T23:30:00Z");
        truncated_row(o, "alice", "STUDIO-967");
        // The reviewer's own run: it ran head-first into the ceiling and was killed mid-turn.
        // Verdict ended 21:00, now 23:30 — well past the 90-minute threshold.
        run_outcome(
            o,
            &review_key("makewhatis", "rhapsody", 164, "alice"),
            "2026-09-21T20:00:00Z",
            "2026-09-21T21:00:00Z",
            rhapsody_store::OUTCOME_TOKEN_CEILING,
        );

        // Warm-up so the first WARN is captured deterministically (see the sibling tests).
        o.reconcile_review_divergence();
        o.review_divergent.clear();

        let (_, events) = crate::testsupport::capture_events(|| o.reconcile_review_divergence());

        let warn = events
            .iter()
            .find(|e| e.level == "WARN")
            .unwrap_or_else(|| panic!("no WARN fired: {events:?}"));
        assert!(
            warn.message.contains("token ceiling"),
            "the line must name the ceiling, got: {}",
            warn.message
        );
        assert!(
            !warn.message.contains("nothing has reported it blocked"),
            "the copy that was false about a ceiling stop must not be reused: {}",
            warn.message
        );

        let found = o.review_divergences();
        assert_eq!(found.len(), 1, "one divergence, got {found:?}");
        assert_eq!(found[0].kind, DivergenceKind::ReviewTokenCeilingStopped);
        assert_eq!(found[0].pr, "makewhatis/rhapsody#164");
        assert_eq!(found[0].reviewer, "alice");

        // Surface one: the per-project advisory.
        let projects = o.project_statuses();
        assert!(
            projects
                .iter()
                .any(|p| p.warnings.iter().any(|w| w == REVIEW_DIVERGENCE_WARNING)),
            "the advisory must reach /api/v1/projects, got {projects:?}"
        );
        // Surface two: the detail on /api/v1/state.
        let rendered = crate::snapshot_json::render(&o.build_snapshot());
        assert_eq!(
            rendered["review_divergence"][0]["kind"],
            "review_token_ceiling_stopped"
        );
    }

    /// **Acceptance.** An ESCALATE decision is reported by the sweep as an ESCALATION carrying the
    /// open findings, the round count and the head the loop stopped at — not as an unexplained
    /// stall, and not with the "nothing has reported it blocked" copy.
    ///
    /// Mutation check (the ticket's ⚠️): making the escalation log-only (so the divergence is not
    /// recorded) leaves no WARN here and reds it.
    #[test]
    fn an_escalated_review_is_reported_with_its_findings_and_rounds() {
        use crate::reviewadjudicate::{Adjudication, AdjudicationLedger};

        let o = &mut orch(false, "2026-09-14T21:20:00Z");
        reviewed_row(o, "alice", "STUDIO-170");
        o.review_rounds.insert(
            crate::reviewwatch::churn_key(&PrCoord::new("makewhatis", "rhapsody", 164)),
            3,
        );
        let ledger = Arc::new(AdjudicationLedger::default());
        ledger.record(
            &PrCoord::new("makewhatis", "rhapsody", 164),
            Adjudication::Escalate {
                head: HEAD.to_string(),
                rounds: 3,
                findings: vec!["alice asked for changes at aaaaaaa".to_string()],
                reason: "the migration needs a DBA".to_string(),
            },
        );
        o.adjudication_ledger = Some(ledger);

        // Warm-up for the new WARN callsite, then capture.
        o.reconcile_review_divergence();
        o.review_divergent.clear();
        let (_, events) = crate::testsupport::capture_events(|| o.reconcile_review_divergence());

        let warn = events
            .iter()
            .find(|e| e.level == "WARN")
            .unwrap_or_else(|| panic!("no WARN fired: {events:?}"));
        assert!(
            warn.message.contains("escalated"),
            "the line must name the decision, got: {}",
            warn.message
        );
        assert!(
            warn.message.contains("alice asked for changes at aaaaaaa"),
            "the specific open findings must be named, got: {}",
            warn.message
        );
        assert!(
            warn.message.contains("the migration needs a DBA"),
            "the manager's own reason must reach the WARN, not only the room and the pull request: \
             {}",
            warn.message
        );

        let found = o.review_divergences();
        assert_eq!(found.len(), 1, "one divergence, got {found:?}");
        assert_eq!(found[0].kind, DivergenceKind::ReviewEscalated);
        assert_eq!(found[0].pr, "makewhatis/rhapsody#164");
        assert_eq!(found[0].ticket, "STUDIO-170");
        assert_eq!(found[0].rounds, 3);
        assert_eq!(found[0].adjudicated_head, HEAD);
        assert_eq!(found[0].reason, "the migration needs a DBA");
        assert_eq!(
            found[0].findings,
            vec!["alice asked for changes at aaaaaaa".to_string()]
        );

        // Surface one: the per-project advisory.
        let projects = o.project_statuses();
        assert!(
            projects
                .iter()
                .any(|p| p.warnings.iter().any(|w| w == REVIEW_DIVERGENCE_WARNING)),
            "the advisory must reach /api/v1/projects, got {projects:?}"
        );
        // Surface two: the detail on /api/v1/state.
        let rendered = crate::snapshot_json::render(&o.build_snapshot());
        assert_eq!(rendered["review_divergence"][0]["kind"], "review_escalated");
    }

    /// Records an ESCALATE at `head` for PR #164 with the incident's own reason text, so a test can
    /// seed the watcher's observed-head memo beside it and watch the supersession follow.
    fn escalated_at(o: &mut Orchestrator, head: &str) {
        use crate::reviewadjudicate::{Adjudication, AdjudicationLedger};
        let pr = PrCoord::new("makewhatis", "rhapsody", 164);
        let ledger = Arc::new(AdjudicationLedger::default());
        ledger.record(
            &pr,
            Adjudication::Escalate {
                head: head.to_string(),
                rounds: 3,
                findings: vec!["alice asked for changes at 0052489".to_string()],
                reason: "sol's REQUEST CHANGES at 0052489 is still unaddressed".to_string(),
            },
        );
        o.adjudication_ledger = Some(ledger);
    }

    /// **STUDIO-1005 acceptance: the #210 replay.** An escalation's reason is a snapshot written
    /// once and never revalidated; when the author pushes past the head it was computed at, the
    /// sweep must say so — and must compare against the head the WATCHER OBSERVED, not the
    /// reviewer's `last_reviewed_sha`. In this fixture the escalation's head IS the reviewed head
    /// (the author pushed since, and the loop is stopped so nothing advanced `last_reviewed_sha`),
    /// so comparing against `last_reviewed_sha` would read as still-current and the operator would
    /// act on findings the author already addressed.
    ///
    /// MUTATION (the ticket's ⚠️): compare `adjudicated_head` against the watch row's
    /// `last_reviewed_sha` instead of `review_observed_head`, and the `superseded` assertion reds.
    #[test]
    fn a_superseded_escalation_is_reported_with_both_heads() {
        let o = &mut orch(false, "2026-09-22T16:14:00Z");
        reviewed_row(o, "alice", "STUDIO-1005"); // last_reviewed_sha == HEAD, and stays there
        escalated_at(o, HEAD);
        // The watcher observed a head that is NOT the escalation's: the author pushed after it.
        o.review_observed_head.insert(
            PrCoord::new("makewhatis", "rhapsody", 164),
            "b02fc72".to_string(),
        );

        o.reconcile_review_divergence();

        let found = o.review_divergences();
        assert_eq!(
            found.len(),
            1,
            "a superseded escalation must NOT be dropped — the push may be unrelated: {found:?}"
        );
        assert_eq!(found[0].kind, DivergenceKind::ReviewEscalated);
        assert!(
            found[0].superseded(),
            "a head move past the escalation's head marks it stale"
        );
        assert_eq!(found[0].adjudicated_head, HEAD);
        assert_eq!(found[0].current_head, "b02fc72");
        assert_eq!(
            found[0].reason, "sol's REQUEST CHANGES at 0052489 is still unaddressed",
            "the manager's own reason is still reported — as evidence, not silently deleted"
        );

        let rendered = crate::snapshot_json::render(&o.build_snapshot());
        let row = &rendered["review_divergence"][0];
        assert_eq!(row["superseded"], true);
        assert_eq!(row["adjudicated_head"], HEAD);
        assert_eq!(row["current_head"], "b02fc72");
        assert_eq!(
            row["reason"],
            "sol's REQUEST CHANGES at 0052489 is still unaddressed"
        );
        assert_eq!(
            row["findings"][0], "alice asked for changes at 0052489",
            "the stale findings travel with the reason"
        );
        let note = row["supersession"].as_str().unwrap_or_default();
        assert!(
            note.contains(HEAD) && note.contains("b02fc72"),
            "the notice must name both heads so the operator can see the snapshot moved, got: {note}"
        );
    }

    /// A supersession APPEARING is its own log transition: the sweep that learns the head moved
    /// says so at once rather than waiting out `RECONCILE_LOG_EVERY` (~30 min), which would leave the
    /// log repeating "still unaddressed" long after the branch moved.
    ///
    /// MUTATION: drop `prev_superseded != d.superseded()` from `annotation_changed` and the second
    /// sweep logs nothing, so this reds.
    #[test]
    fn a_newly_superseded_escalation_logs_on_the_sweep_that_learns_it() {
        let o = &mut orch(false, "2026-09-22T16:14:00Z");
        reviewed_row(o, "alice", "STUDIO-1005");
        escalated_at(o, HEAD);
        let pr = PrCoord::new("makewhatis", "rhapsody", 164);
        o.review_observed_head.insert(pr.clone(), HEAD.to_string());

        // First sweep: the escalation is current.
        o.reconcile_review_divergence();
        assert!(!o.review_divergences()[0].superseded());

        // The author pushes. The very NEXT sweep must log the supersession, not wait for the
        // steady-state rate limit.
        o.review_observed_head.insert(pr, "b02fc72".to_string());
        let (_, events) = crate::testsupport::capture_events(|| o.reconcile_review_divergence());
        assert!(
            events
                .iter()
                .any(|e| e.level == "WARN" && e.message.contains("SUPERSEDED")),
            "the sweep that learns the head moved must log it, got: {events:?}"
        );
    }

    /// The ticket's single-commit mutation: the supersession marker must fire on ANY head move, not
    /// only a move of more than one commit. The sweep compares two SHA strings and cannot count
    /// commits locally, so a one-commit difference is simply a difference.
    ///
    /// MUTATION: require the head to differ by more than one commit (e.g. a commit count the sweep
    /// cannot obtain) and this reds.
    #[test]
    fn a_single_commit_head_move_still_marks_the_escalation_superseded() {
        let o = &mut orch(false, "2026-09-22T16:14:00Z");
        reviewed_row(o, "alice", "STUDIO-1005");
        escalated_at(o, HEAD);
        o.review_observed_head.insert(
            PrCoord::new("makewhatis", "rhapsody", 164),
            "c0a54eb1".to_string(), // one commit past HEAD
        );

        o.reconcile_review_divergence();
        let found = o.review_divergences();
        assert_eq!(found.len(), 1);
        assert!(found[0].superseded(), "one commit is still a moved head");
    }

    /// **STUDIO-1005 acceptance: an escalation whose head has NOT moved adds NOTHING to the wire
    /// row.** This is the ticket's last acceptance and it is deliberately a test that passes against
    /// BOTH the old and the new code: it asserts the row's exact key set, referencing none of the
    /// supersession fields, so the pre-ticket implementation produces the same object.
    #[test]
    fn an_escalation_at_the_current_head_renders_byte_identically_to_today() {
        let o = &mut orch(false, "2026-09-22T16:14:00Z");
        reviewed_row(o, "alice", "STUDIO-1005");
        escalated_at(o, HEAD);
        // The watcher observed exactly the head the escalation was computed at: no supersession.
        o.review_observed_head.insert(
            PrCoord::new("makewhatis", "rhapsody", 164),
            HEAD.to_string(),
        );

        o.reconcile_review_divergence();
        assert!(
            !o.review_divergences()[0].superseded(),
            "the head has not moved, so the escalation is current"
        );

        let rendered = crate::snapshot_json::render(&o.build_snapshot());
        let row = rendered["review_divergence"][0]
            .as_object()
            .expect("a review_divergence row is an object");
        let mut keys: Vec<&str> = row.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec!["detail", "kind", "pr", "reviewer", "stale_secs", "ticket",],
            "a still-current escalation must carry today's fields and no supersession annotation, \
             got: {row:?}"
        );
        assert_eq!(row["kind"], "review_escalated");
    }

    /// **STUDIO-1005 acceptance: the sweep stays local-only.** The reconciliation sweep runs on
    /// `on_tick`, above the validate/drain/credential gates, and must never make a `gh`, tracker or
    /// model call. This asserts it directly on the source, so a future edit that reaches for a
    /// lookup to "refresh" the escalation's text reds here instead of shipping an agent run per tick.
    ///
    /// MUTATION (the ticket's ⚠️): introduce any of these calls into the sweep and this reds.
    #[test]
    fn the_reconciliation_sweep_makes_no_network_call() {
        let src = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/reviewreconcile.rs"
        ))
        .expect("read the sweep's own source");
        // Assembled rather than written literally so the tokens do not appear in THIS test's own
        // source and match themselves.
        let tokens = [
            format!(".pr_state{}", "("),
            format!(".pr_state_unconditional{}", "("),
            format!("sweep_pr_states{}", "("),
            format!("Command::{}", "new"),
            format!("req{}", "west"),
            format!("run{}", "_turn"),
            format!("post_pr{}", "_comment"),
        ];
        for token in tokens {
            assert!(
                !src.contains(token.as_str()),
                "the reconciliation sweep must stay local-only; found `{token}`"
            );
        }
    }

    /// **A shipped pull request the merge gate cannot clear is REPORTED, not hidden.** A `ship`
    /// adjudicates the open findings, never the gates — so if a live row still records findings
    /// rather than an approval at the head, the gate will never clear on its own and the loop is
    /// stopped: with the decision suppressed, nothing would ever mention it again.
    ///
    /// Mutation check (the ticket's ⚠️): making the shipped decision log-only (returning `None`
    /// here, as the first cut did) reds this.
    #[test]
    fn a_shipped_pull_request_the_merge_gate_cannot_clear_is_reported() {
        use crate::reviewadjudicate::{Adjudication, AdjudicationLedger};

        let o = &mut orch(false, "2026-09-14T21:20:00Z");
        reviewed_row(o, "alice", "STUDIO-170");
        o.review_rounds.insert(
            crate::reviewwatch::churn_key(&PrCoord::new("makewhatis", "rhapsody", 164)),
            3,
        );
        let ledger = Arc::new(AdjudicationLedger::default());
        ledger.record(
            &PrCoord::new("makewhatis", "rhapsody", 164),
            Adjudication::Ship {
                head: HEAD.to_string(),
                rounds: 3,
            },
        );
        o.adjudication_ledger = Some(ledger);

        // Warm-up for the new WARN callsite, then capture.
        o.reconcile_review_divergence();
        o.review_divergent.clear();
        let (_, events) = crate::testsupport::capture_events(|| o.reconcile_review_divergence());

        let warn = events
            .iter()
            .find(|e| e.level == "WARN")
            .unwrap_or_else(|| panic!("no WARN fired: {events:?}"));
        assert!(
            warn.message.contains("shipped"),
            "the line must name the decision, got: {}",
            warn.message
        );
        assert!(
            !warn.message.contains("nothing has reported it blocked"),
            "the copy that is false about a decided pull request must not be reused: {}",
            warn.message
        );

        let found = o.review_divergences();
        assert_eq!(found.len(), 1, "one divergence, got {found:?}");
        assert_eq!(found[0].kind, DivergenceKind::ReviewShipped);
        assert_eq!(found[0].pr, "makewhatis/rhapsody#164");
        assert_eq!(found[0].ticket, "STUDIO-170");
        assert_eq!(found[0].rounds, 3);
        assert_eq!(found[0].adjudicated_head, HEAD);

        let rendered = crate::snapshot_json::render(&o.build_snapshot());
        assert_eq!(rendered["review_divergence"][0]["kind"], "review_shipped");
    }

    /// A shipped pull request whose live rows are ALL approved is the merge gate's business, not a
    /// `review_shipped` report: auto-merge merges it, or `ApprovedStillOpen` reports the stuck gate
    /// after the staleness threshold. Pinned so the shipped rule cannot widen into crying wolf on
    /// every successful ship.
    #[test]
    fn a_shipped_pull_request_with_every_row_approved_is_not_reported_shipped() {
        use crate::reviewadjudicate::{Adjudication, AdjudicationLedger};

        let o = &mut orch(false, "2026-09-14T21:20:00Z");
        // The reviewer approved the head after the loop ran out.
        approved_row(o, "alice", "STUDIO-170");
        let ledger = Arc::new(AdjudicationLedger::default());
        ledger.record(
            &PrCoord::new("makewhatis", "rhapsody", 164),
            Adjudication::Ship {
                head: HEAD.to_string(),
                rounds: 3,
            },
        );
        o.adjudication_ledger = Some(ledger);

        o.reconcile_review_divergence();

        assert!(
            o.review_divergences().is_empty(),
            "a shipped pull request every row approved is not a stall, got {:?}",
            o.review_divergences()
        );
    }

    /// A decision still being made is not a stall: the loop is stopped deliberately while the
    /// manager's turn runs, so the row and budget rules must not report it.
    #[test]
    fn a_deciding_pull_request_is_not_reported_as_a_stall() {
        use crate::reviewadjudicate::{Adjudication, AdjudicationLedger};

        let o = &mut orch(false, "2026-09-14T21:20:00Z");
        reviewed_row(o, "alice", "STUDIO-170");
        run(
            o,
            &review_key("makewhatis", "rhapsody", 164, "alice"),
            "2026-09-14T21:10:00Z",
            "2026-09-14T21:19:00Z",
        );
        o.review_rounds.insert(
            crate::reviewwatch::churn_key(&PrCoord::new("makewhatis", "rhapsody", 164)),
            crate::reviewwatch::REVIEW_ROUNDS_PER_PR_CAP,
        );
        let ledger = Arc::new(AdjudicationLedger::default());
        ledger.record(
            &PrCoord::new("makewhatis", "rhapsody", 164),
            Adjudication::InFlight { rounds: 3 },
        );
        o.adjudication_ledger = Some(ledger);

        o.reconcile_review_divergence();

        assert!(
            o.review_divergences().is_empty(),
            "a pull request the manager is still deciding must not be reported, got {:?}",
            o.review_divergences()
        );
    }

    /// The budget is spent at DISPATCH, so the summoned author run that spent it is in flight for
    /// its WHOLE duration. Reporting the pull request then would be premature by an entire agent run
    /// and would flicker off when the run finished — the same defect this feature's first cut fixed
    /// for the review half, on the author half.
    #[test]
    fn a_spent_budget_reports_nothing_while_the_summoned_author_runs() {
        let o = &mut orch(false, "2026-09-14T21:20:00Z");
        reviewed_row(o, "alice", "STUDIO-170");
        // The reviewer's round finished at 21:19...
        run(
            o,
            &review_key("makewhatis", "rhapsody", 164, "alice"),
            "2026-09-14T21:10:00Z",
            "2026-09-14T21:19:00Z",
        );
        // ...and the author's summoned run STARTED 21:19:30 and has not ended. This is the run the
        // spent budget was charged for: the loop is progressing, not stalled.
        run_in_flight(o, "STUDIO-170", "2026-09-14T21:19:30Z");
        o.review_rounds.insert(
            crate::reviewwatch::churn_key(&PrCoord::new("makewhatis", "rhapsody", 164)),
            crate::reviewwatch::REVIEW_ROUNDS_PER_PR_CAP,
        );

        o.reconcile_review_divergence();
        assert!(
            o.review_divergences().is_empty(),
            "an author run in flight must silence the pull request, got {:?}",
            o.review_divergences()
        );
    }

    /// The other half of the same gap: an author who ran, answered and deliberately held without
    /// pushing leaves the row `reviewed`, but `ticket_run.started_at >= anchor` is a shape
    /// [`reconcile_pr`] calls healthy — so the budget report must not call it a stall either.
    #[test]
    fn a_spent_budget_reports_nothing_when_the_author_answered_and_held() {
        let o = &mut orch(false, "2026-09-14T21:20:00Z");
        reviewed_row(o, "alice", "STUDIO-170");
        run(
            o,
            &review_key("makewhatis", "rhapsody", 164, "alice"),
            "2026-09-14T21:10:00Z",
            "2026-09-14T21:19:00Z",
        );
        // The author answered the findings after the verdict landed and pushed nothing.
        run(
            o,
            "STUDIO-170",
            "2026-09-14T21:19:30Z",
            "2026-09-14T21:19:40Z",
        );
        o.review_rounds.insert(
            crate::reviewwatch::churn_key(&PrCoord::new("makewhatis", "rhapsody", 164)),
            crate::reviewwatch::REVIEW_ROUNDS_PER_PR_CAP,
        );

        o.reconcile_review_divergence();
        assert!(
            o.review_divergences().is_empty(),
            "a row the author already answered is not a stall, got {:?}",
            o.review_divergences()
        );
    }

    /// The budget rule is scoped: an in-flight round beside a spent budget is progressing, an
    /// approved pull request is the merge gate's business, and neither is reported. This is what
    /// keeps the report from crying wolf on a healthy (if expensive) loop.
    #[test]
    fn a_spent_budget_reports_nothing_while_a_round_is_in_flight_or_approved() {
        // A MIXED pull request: one reviewer's findings are outstanding (reviewed) while a second
        // reviewer's round is in flight. The running agent silences the whole pull request — a spent
        // budget beside it is not a stall — which a per-row rule would get wrong.
        let o = &mut orch(false, "2026-09-14T21:20:00Z");
        reviewed_row(o, "alice", "STUDIO-170");
        let running_key = ReviewWatchKey {
            owner: "makewhatis".to_string(),
            repo: "rhapsody".to_string(),
            number: 164,
            reviewer: "bob".to_string(),
        };
        o.store()
            .save_review_watch(ReviewWatchRow {
                key: running_key.clone(),
                author: "jimmy".to_string(),
                introduced_by: "handoff:STUDIO-170".to_string(),
                requested_sha: String::new(),
                last_reviewed_sha: String::new(),
                status: String::new(),
                open: true,
            })
            .expect("seed the second row");
        // `mark_review_requested` moves it to `in_flight`.
        o.store()
            .mark_review_requested(&running_key, HEAD)
            .expect("in-flight");
        o.review_rounds.insert(
            crate::reviewwatch::churn_key(&PrCoord::new("makewhatis", "rhapsody", 164)),
            crate::reviewwatch::REVIEW_ROUNDS_PER_PR_CAP,
        );
        o.reconcile_review_divergence();
        assert!(
            o.review_divergences().is_empty(),
            "a round in flight silences the pull request, got {:?}",
            o.review_divergences()
        );

        // Every live row approved, auto-merge off.
        let o = &mut orch(false, "2026-09-14T21:20:00Z");
        approved_row(o, "alice", "STUDIO-170");
        o.review_rounds.insert(
            crate::reviewwatch::churn_key(&PrCoord::new("makewhatis", "rhapsody", 164)),
            crate::reviewwatch::REVIEW_ROUNDS_PER_PR_CAP,
        );
        o.reconcile_review_divergence();
        assert!(
            o.review_divergences().is_empty(),
            "with auto-merge off an approved pull request waits for a human, got {:?}",
            o.review_divergences()
        );
    }

    /// The healthy case through the same path: the author answered the findings, so nothing is
    /// reported and NEITHER surface changes. This is the acceptance's "a healthy board reports
    /// nothing" — the property that decides whether the warning above is ever read.
    #[test]
    fn a_healthy_pull_request_reports_nothing_on_either_surface() {
        let o = &mut orch(false, "2026-09-14T21:20:00Z");
        reviewed_row(o, "alice", "STUDIO-893");
        run(
            o,
            &review_key("makewhatis", "rhapsody", 164, "alice"),
            "2026-09-14T14:50:00Z",
            "2026-09-14T15:20:00Z",
        );
        // The author's run STARTED after the verdict landed.
        run(
            o,
            "STUDIO-893",
            "2026-09-14T15:30:00Z",
            "2026-09-14T16:40:00Z",
        );

        o.reconcile_review_divergence();

        assert!(o.review_divergences().is_empty());
        assert!(
            o.project_statuses()
                .iter()
                .all(|p| !p.warnings.iter().any(|w| w == REVIEW_DIVERGENCE_WARNING))
        );
        let rendered = crate::snapshot_json::render(&o.build_snapshot());
        assert!(
            rendered.get("review_divergence").is_none(),
            "a healthy daemon serves the Go-identical payload"
        );
    }

    /// §16: a daemon with Teams off, or off the ticketless path, sweeps nothing and reports nothing —
    /// and CLEARS anything a previous configuration had reported, so a hot reload that turns the
    /// feature off does not leave a warning latched forever.
    #[test]
    fn a_dormant_daemon_reports_nothing_and_clears_what_it_had() {
        let o = &mut orch(false, "2026-09-14T21:20:00Z");
        reviewed_row(o, "alice", "STUDIO-893");
        run(
            o,
            &review_key("makewhatis", "rhapsody", 164, "alice"),
            "2026-09-14T14:50:00Z",
            "2026-09-14T15:20:00Z",
        );
        run(
            o,
            "STUDIO-893",
            "2026-09-14T13:00:00Z",
            "2026-09-14T14:40:00Z",
        );
        o.reconcile_review_divergence();
        assert_eq!(o.review_divergences().len(), 1, "reported while enabled");

        for mode in [ReviewMode::Off, ReviewMode::Tickets] {
            if let Some(t) = o.teams.as_mut() {
                t.review.mode = mode;
            }
            o.reconcile_review_divergence();
            assert!(
                o.review_divergences().is_empty(),
                "review.mode {mode:?} must sweep nothing"
            );
        }
    }

    /// The sweep REPORTS and does not ACT — the ticket's recommendation 4, pinned rather than merely
    /// documented. A sweep that re-dispatched on divergence is how a stall becomes a loop, and the
    /// difference is invisible in the report itself.
    #[test]
    fn the_sweep_dispatches_nothing_and_moves_nothing() {
        let o = &mut orch(true, "2026-09-14T21:20:00Z");
        reviewed_row(o, "alice", "STUDIO-893");
        run(
            o,
            &review_key("makewhatis", "rhapsody", 164, "alice"),
            "2026-09-14T14:50:00Z",
            "2026-09-14T15:20:00Z",
        );
        run(
            o,
            "STUDIO-893",
            "2026-09-14T13:00:00Z",
            "2026-09-14T14:40:00Z",
        );
        let watch_before = o.store().load_review_watch().expect("rows");
        let runs_before = o.store().list_runs(RunFilter::default()).expect("runs");

        o.reconcile_review_divergence();

        assert_eq!(o.review_divergences().len(), 1, "it did report");
        assert!(o.running.is_empty(), "nothing was dispatched");
        assert!(o.claimed.is_empty(), "nothing was claimed");
        assert_eq!(
            o.store().load_review_watch().expect("rows"),
            watch_before,
            "no watch row was rewritten"
        );
        assert_eq!(
            o.store()
                .list_runs(RunFilter::default())
                .expect("runs")
                .len(),
            runs_before.len(),
            "no run was started"
        );
    }

    /// STUDIO-923: when auto-merge has already declined the SAME pull request this sweep is
    /// independently reporting `ApprovedStillOpen`, the WARN names the reason instead of claiming
    /// nothing has reported it blocked. It states no count: this sweep's own `sweeps` field runs on
    /// a different cadence than auto-merge's attempts (`PR_STATE_POLL_INTERVAL` vs
    /// `polling.interval_ms`), so asserting it as auto-merge's tally would trade one false claim
    /// for another.
    ///
    /// Mutation check: revert the enriched arm's message back to the hardcoded "nothing has
    /// reported it blocked" wording and this test reds — it asserts on WHAT was said, not merely
    /// that a WARN fired.
    #[test]
    fn approved_and_open_names_the_auto_merge_reason() {
        let o = &mut orch(true, "2026-09-14T21:20:00Z");
        approved_row(o, "alice", "STUDIO-877");
        approved_row(o, "jimmy", "STUDIO-877");
        run(
            o,
            &review_key("makewhatis", "rhapsody", 164, "alice"),
            "2026-09-12T12:00:00Z",
            "2026-09-12T12:30:00Z",
        );
        run(
            o,
            &review_key("makewhatis", "rhapsody", 164, "jimmy"),
            "2026-09-12T12:00:00Z",
            "2026-09-12T12:45:00Z",
        );

        let ledger = Arc::new(crate::runautomerge::AutoMergeLedger::default());
        ledger.test_seed(
            &PrCoord::new("makewhatis", "rhapsody", 164),
            HEAD,
            "the pull request is still a draft",
        );
        o.automerge_ledger = Some(ledger);

        // `tracing`'s per-callsite Interest cache only gets REBUILT for a callsite that has
        // already executed at least once (`testsupport::TRACING_TEST_LOCK`'s own doc: a brand new
        // callsite's FIRST hit can race a concurrently-running test on another thread). This
        // enriched-message callsite is exercised by no other test in the suite, so one uncaptured
        // warm-up call registers it before the real, captured call depends on it; resetting the
        // sweep counter after keeps the assertion below about the FIRST crossing, not the second.
        o.reconcile_review_divergence();
        o.review_divergent.clear();

        let (_, events) = crate::testsupport::capture_events(|| o.reconcile_review_divergence());

        let warn = events
            .iter()
            .find(|e| e.level == "WARN")
            .unwrap_or_else(|| panic!("no WARN fired: {events:?}"));
        assert!(
            warn.message
                .contains("Auto-merge has been declining it: the pull request is still a draft."),
            "got: {}",
            warn.message
        );
        assert!(
            !warn.message.contains("nothing has reported it blocked"),
            "the enriched line must replace the fallback wording, not sit beside it: {}",
            warn.message
        );
    }

    /// The other half of the acceptance: approved, open, auto-merge ON, but the ledger holds
    /// nothing for this pull request — it never reached a gate, or this daemon's off-loop half was
    /// never built. The report keeps its original wording, unenriched.
    #[test]
    fn approved_and_open_with_no_ledger_entry_keeps_the_plain_wording() {
        let o = &mut orch(true, "2026-09-14T21:20:00Z");
        approved_row(o, "alice", "STUDIO-877");
        approved_row(o, "jimmy", "STUDIO-877");
        run(
            o,
            &review_key("makewhatis", "rhapsody", 164, "alice"),
            "2026-09-12T12:00:00Z",
            "2026-09-12T12:30:00Z",
        );
        run(
            o,
            &review_key("makewhatis", "rhapsody", 164, "jimmy"),
            "2026-09-12T12:00:00Z",
            "2026-09-12T12:45:00Z",
        );
        // No `o.automerge_ledger` at all — the review watcher never spawned.
        assert!(o.automerge_ledger.is_none());

        // See the sibling test above for why this warm-up call exists: this fallback-message
        // callsite is likewise exercised by no other test, so it needs the same registration
        // before the real, captured call.
        o.reconcile_review_divergence();
        o.review_divergent.clear();

        let (_, events) = crate::testsupport::capture_events(|| o.reconcile_review_divergence());

        let warn = events
            .iter()
            .find(|e| e.level == "WARN")
            .unwrap_or_else(|| panic!("no WARN fired: {events:?}"));
        assert!(
            warn.message.contains(
                "Nothing is progressing it and nothing has reported it blocked; this sweep only \
                 reports, so it needs a human."
            ),
            "got: {}",
            warn.message
        );
        assert!(
            !warn.message.contains("Auto-merge has declined"),
            "no ledger entry must never invent a decline count: {}",
            warn.message
        );
    }

    /// STUDIO-961: an approved-and-open pull request whose conflict the watcher has already routed
    /// back to its author is PROGRESSING, not waiting for a human — the transition is the progress.
    ///
    /// Mutation check: delete the `conflict_routed` branch in `reconcile_review_divergence` and this
    /// test reds — the control call below proves the divergence would otherwise be reported.
    #[test]
    fn a_conflict_route_back_in_flight_is_not_reported_as_needing_a_human() {
        let o = &mut orch(true, "2026-09-14T21:20:00Z");
        approved_row(o, "alice", "STUDIO-877");
        approved_row(o, "jimmy", "STUDIO-877");
        run(
            o,
            &review_key("makewhatis", "rhapsody", 164, "alice"),
            "2026-09-12T12:00:00Z",
            "2026-09-12T12:30:00Z",
        );
        run(
            o,
            &review_key("makewhatis", "rhapsody", 164, "jimmy"),
            "2026-09-12T12:00:00Z",
            "2026-09-12T12:45:00Z",
        );

        // The control: with no route-back on record, this IS reported as diverged.
        o.reconcile_review_divergence();
        assert_eq!(
            o.review_divergences().len(),
            1,
            "an approved-and-open pull request is diverged until something progresses it"
        );

        // The watcher routed it back for a conflict at HEAD, so the sweep must fall silent.
        o.conflict_routed.insert(
            crate::prstate::PrCoord::new("makewhatis", "rhapsody", 164),
            crate::reviewwatch::ConflictRoute {
                head: HEAD.to_string(),
                routed_at: t("2026-09-14T21:20:00Z"),
            },
        );
        o.reconcile_review_divergence();

        assert!(
            o.review_divergences().is_empty(),
            "a conflict route-back in flight must not be reported as needing a human"
        );
        assert!(
            o.project_statuses()
                .iter()
                .all(|p| !p.warnings.iter().any(|w| w == REVIEW_DIVERGENCE_WARNING)),
            "and the advisory must not light"
        );
        let rendered = crate::snapshot_json::render(&o.build_snapshot());
        assert!(
            rendered.get("review_divergence").is_none(),
            "nor may it reach /api/v1/state"
        );
    }

    /// STUDIO-961: the sweep's silence about a conflict route-back EXPIRES. A route-back the author
    /// never answers — or whose tracker move never landed — is progress only while it is fresh; past
    /// the sweep's own staleness horizon the pull request needs the human signal again, rather than
    /// being suppressed forever.
    ///
    /// Mutation check: drop the `stale_secs` fresh-check in `reconcile_review_divergence` (suppress
    /// on the record alone) and this test reds — the stale record would keep the divergence silent.
    #[test]
    fn a_stale_conflict_route_back_is_reported_as_needing_a_human_again() {
        let o = &mut orch(true, "2026-09-14T21:20:00Z");
        approved_row(o, "alice", "STUDIO-877");
        approved_row(o, "jimmy", "STUDIO-877");
        run(
            o,
            &review_key("makewhatis", "rhapsody", 164, "alice"),
            "2026-09-12T12:00:00Z",
            "2026-09-12T12:30:00Z",
        );
        run(
            o,
            &review_key("makewhatis", "rhapsody", 164, "jimmy"),
            "2026-09-12T12:00:00Z",
            "2026-09-12T12:45:00Z",
        );

        // Routed back four hours and twenty minutes ago: older than the ninety-minute horizon, so
        // the transition has stopped being progress.
        o.conflict_routed.insert(
            crate::prstate::PrCoord::new("makewhatis", "rhapsody", 164),
            crate::reviewwatch::ConflictRoute {
                head: HEAD.to_string(),
                routed_at: t("2026-09-14T17:00:00Z"),
            },
        );
        o.reconcile_review_divergence();

        assert_eq!(
            o.review_divergences().len(),
            1,
            "a stale route-back must not silence the human signal forever"
        );
        assert_eq!(
            o.review_divergences()[0].kind,
            DivergenceKind::ApprovedStillOpen
        );
    }

    /// ⚠️ STUDIO-961: the suppression is scoped to [`DivergenceKind::ApprovedStillOpen`], and the
    /// scoping is the point — not an implementation detail of where the branch happens to sit.
    ///
    /// [`DivergenceKind::ChangesRequestedNoRun`] is the sibling kind, and it is PRECISELY the signal
    /// that a route-back's tracker move landed but the author's run never reopened: findings (or a
    /// conflict) on the record, and no authoring run since. Silencing it on the same record would
    /// hide the one failure mode the route-back itself can produce — the summons that never took —
    /// and it would be hidden for the whole life of the record rather than for the freshness window,
    /// because this kind's own staleness clock keeps running.
    ///
    /// `a_conflict_route_back_in_flight_is_not_reported_as_needing_a_human` is the live control: the
    /// SAME fresh record, on an approved-and-open pull request, is silent.
    ///
    /// Mutation check: lift the `conflict_routed` branch out of the `d.kind ==
    /// DivergenceKind::ApprovedStillOpen` block in `reconcile_review_divergence` and this test reds
    /// (the divergence disappears), while the control above stays green.
    #[test]
    fn a_conflict_route_back_does_not_silence_a_changes_requested_divergence() {
        let o = &mut orch(false, "2026-09-14T21:20:00Z");
        reviewed_row(o, "alice", "STUDIO-893");
        run(
            o,
            &review_key("makewhatis", "rhapsody", 164, "alice"),
            "2026-09-14T14:50:00Z",
            "2026-09-14T15:20:00Z",
        );
        // The authoring run ended BEFORE the review, so nothing has answered the findings.
        run(
            o,
            "STUDIO-893",
            "2026-09-14T13:00:00Z",
            "2026-09-14T14:40:00Z",
        );
        // A conflict route-back fired for this very pull request, moments ago — as fresh as the
        // control's.
        o.conflict_routed.insert(
            crate::prstate::PrCoord::new("makewhatis", "rhapsody", 164),
            crate::reviewwatch::ConflictRoute {
                head: HEAD.to_string(),
                routed_at: t("2026-09-14T21:20:00Z"),
            },
        );

        o.reconcile_review_divergence();

        let found = o.review_divergences();
        assert_eq!(
            found.len(),
            1,
            "a route-back that moved the ticket but never reopened the author's run is exactly \
             what this kind reports; it must not be suppressed: {found:?}"
        );
        assert_eq!(found[0].kind, DivergenceKind::ChangesRequestedNoRun);
        assert!(
            o.project_statuses()
                .iter()
                .any(|p| p.warnings.iter().any(|w| w == REVIEW_DIVERGENCE_WARNING)),
            "and the advisory must still light"
        );
    }

    /// STUDIO-950 (round 20, alice's blocking finding; jimmy's round-20 BLOCKING 1): the unreadable
    /// annotation is not a capacity HOLD, so its suppression of the false page cannot be justified
    /// by the approved-and-open arm's `capacity_held: None`. During a `gh` outage the auto-merge
    /// ledger stays empty — the outage that sets `review_watch_unreadable` is the same one that
    /// keeps `peek` from answering — so `(None, None)` is the ORDINARY arm for an approved row, and
    /// without the annotation it reports "nothing has reported it blocked" while the watcher refutes
    /// that every tick.
    ///
    /// Mutation check: delete the `d.capacity_unreadable = self.unreadable_attempts(pr)` line in
    /// [`Orchestrator::reconcile_review_divergence`] and this reds on the fallback wording, its
    /// advisory reverting to [`REVIEW_DIVERGENCE_WARNING`].
    #[test]
    fn an_approved_row_whose_coordinate_is_unreadable_is_reported_as_unreadable() {
        let o = &mut orch(true, "2026-09-14T21:20:00Z");
        approved_row(o, "alice", "STUDIO-877");
        approved_row(o, "jimmy", "STUDIO-877");
        run(
            o,
            &review_key("makewhatis", "rhapsody", 164, "alice"),
            "2026-09-12T12:00:00Z",
            "2026-09-12T12:30:00Z",
        );
        run(
            o,
            &review_key("makewhatis", "rhapsody", 164, "jimmy"),
            "2026-09-12T12:00:00Z",
            "2026-09-12T12:45:00Z",
        );
        // No ledger: the `gh` outage that made the coordinate unreadable is the same one that kept
        // auto-merge from evaluating the pull request at all.
        assert!(o.automerge_ledger.is_none());
        o.review_watch_unreadable.insert(
            PrCoord::new("makewhatis", "rhapsody", 164),
            UNREADABLE_ATTEMPTS_TO_DROP_HOLD,
        );

        // See the sibling tests for why the warm-up call exists: this callsite is exercised by no
        // other test, so it needs registration before the real, captured call.
        o.reconcile_review_divergence();
        o.review_divergent.clear();

        let (_, events) = crate::testsupport::capture_events(|| o.reconcile_review_divergence());

        let warn = events
            .iter()
            .find(|e| e.level == "WARN")
            .unwrap_or_else(|| panic!("no WARN fired: {events:?}"));
        assert!(
            warn.message.contains("could not be read"),
            "an approved row's unreadable coordinate must be named, got: {}",
            warn.message
        );
        assert!(
            !warn.message.contains("nothing has reported it blocked"),
            "the unreadable line replaces the fallback wording, not sits beside it: {}",
            warn.message
        );

        let projects = o.project_statuses();
        assert!(
            projects.iter().any(|p| p
                .warnings
                .iter()
                .any(|w| w == REVIEW_DIVERGENCE_UNREADABLE_WARNING)),
            "the advisory must name the unreadable coordinate, got {projects:?}"
        );
        assert!(
            !projects
                .iter()
                .any(|p| p.warnings.iter().any(|w| w == REVIEW_DIVERGENCE_WARNING)),
            "it must not also claim nothing has reported it blocked, got {projects:?}"
        );

        let rendered = crate::snapshot_json::render(&o.build_snapshot());
        assert_eq!(
            rendered["review_divergence"][0]["capacity_unreadable"]["attempts"],
            UNREADABLE_ATTEMPTS_TO_DROP_HOLD
        );
    }
}
