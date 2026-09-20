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
    /// Divergence (b): every required reviewer approved the current head and the pull request is
    /// still open, with `review.auto_merge` on. STUDIO-881's draft loop and the `BEHIND` decline.
    ApprovedStillOpen,
    /// Not a divergence of intent and activity but of a BOUND: the pull request's shared
    /// review↔author round budget is spent, so neither the review half nor the author re-dispatch
    /// the review's findings would summon can run, and nothing will resume on its own (STUDIO-956).
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
            DivergenceKind::ApprovedStillOpen => {
                "every required reviewer approved and the pull request is still open"
            }
            DivergenceKind::RoundBudgetExhausted => {
                "the review↔author round budget is spent, so no further review or author re-run \
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
    /// The head the manager stopped at — only meaningful for [`DivergenceKind::ReviewEscalated`],
    /// `""` otherwise. Read by [`Orchestrator::set_review_divergences`] to name where the loop
    /// stopped, never rendered onto `/api/v1/state`.
    pub adjudicated_head: String,
    /// How many review↔author rounds the loop ran before the escalation — `0` for every kind but
    /// [`DivergenceKind::ReviewEscalated`]. Rendered onto the escalation log line, not the wire.
    pub rounds: usize,
    /// The open findings the manager escalated on — empty for every kind but
    /// [`DivergenceKind::ReviewEscalated`]. Named on the log line so the escalation is actionable.
    pub findings: Vec<String>,
    /// The manager's OWN words for an escalation — empty for every kind but
    /// [`DivergenceKind::ReviewEscalated`]. Read by [`Orchestrator::set_review_divergences`] so the
    /// WARN carries the reason rather than only the room post and the pull-request comment; like
    /// [`Divergence::adjudicated_head`] it is not rendered onto `/api/v1/state`.
    pub reason: String,
}

/// When one run started, and whether it has finished. The only two facts about a `runs` row the
/// rules need, so the rules can be driven by a test without a store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunMoment {
    pub started_at: DateTime<Utc>,
    /// `None` while the run is still in flight.
    pub ended_at: Option<DateTime<Utc>>,
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
                adjudicated_head: String::new(),
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
        adjudicated_head: String::new(),
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
        let now = (self.now)();
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
                        adjudicated_head: head,
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
                            adjudicated_head: head,
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
                            adjudicated_head: String::new(),
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
                    d.auto_merge_reason = self.automerge_ledger.as_ref().and_then(|l| l.peek(pr));
                }
                Some(d)
            })
            .collect();
        self.set_review_divergences(found);
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
        })
    }

    /// Replaces the reported set, logging the transitions and rate-limiting the steady state.
    fn set_review_divergences(&mut self, found: Vec<Divergence>) {
        for d in &found {
            let sweeps = self.review_divergent.entry(d.pr.clone()).or_insert(0);
            *sweeps += 1;
            let sweeps = *sweeps;
            // The crossing sweep and the rate-limited repeats in ONE condition: at the crossing the
            // count is 1, and `1 - 1` is a multiple of everything.
            if (sweeps - 1).is_multiple_of(RECONCILE_LOG_EVERY) {
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
                // STUDIO-923: when auto-merge has already said something about this exact pull
                // request, name it instead of claiming nothing has. The sentence states no count:
                // auto-merge's own attempts run on the review watcher's separate
                // `PR_STATE_POLL_INTERVAL` cadence (120s), not this sweep's `polling.interval_ms`
                // (default 30s), so this sweep's own `sweeps` field would misstate auto-merge's
                // tally as its own — trading the ticket's false negative for a false positive. No
                // ledger entry (auto-merge off, or this head never reached a gate) falls back to the
                // original wording unchanged.
                match d.auto_merge_reason {
                    Some(reason) => {
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
                    None => {
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
        })
    }

    /// A run still in flight.
    fn running(started: &str) -> Option<RunMoment> {
        Some(RunMoment {
            started_at: t(started),
            ended_at: None,
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
    use crate::testsupport::{empty_effective, empty_resolved_project, set_of};

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
            .mark_review_completed(&key, HEAD, REVIEW_STATUS_REVIEWED)
            .expect("completed");
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
                    outcome: "completed".to_string(),
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
            warn.message.contains("review↔author round budget is spent"),
            "the line must name the reason, got: {}",
            warn.message
        );
        assert!(
            !warn.message.contains("nothing has reported it blocked"),
            "the copy that was false about a capped pull request must not be reused: {}",
            warn.message
        );

        let found = o.review_divergences();
        assert_eq!(found.len(), 1, "one divergence, got {found:?}");
        assert_eq!(found[0].kind, DivergenceKind::RoundBudgetExhausted);
        assert_eq!(found[0].pr, "makewhatis/rhapsody#164");
        assert_eq!(found[0].ticket, "STUDIO-170");

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
}
