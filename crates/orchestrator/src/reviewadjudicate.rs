//! reviewadjudicate — the manager's decision that ends a review↔author loop that is not converging
//! (STUDIO-956).
//!
//! **No Go v0.4.0 counterpart.** Ticketless review is a Rhapsody addition end to end, and this is
//! the terminal edge the first version of STUDIO-956 got wrong: a round cap that STOPS is a stall
//! with a nicer name. The maintainer's shape is a DECIDER — at a configurable threshold
//! ([`Teams::review_adjudicate_after_rounds`](rhapsody_config::teams::Teams::review_adjudicate_after_rounds),
//! their number is 3) the loop stops arming rounds and hands the pull request to the manager for
//! exactly one decision: **ship it** (the open findings do not block) or **escalate** (a human is
//! needed, naming the specific open findings).
//!
//! # Off the loop, and why the decision is a ledger and not a return value
//!
//! The decision is made ON the control task — where the round counter and the watch set are
//! single-writer — and the model turn is performed OFF it, on the review watcher's own task, for
//! [`crate::triage`]'s reason: a model call on the dispatch path is the STUDIO-551 head-of-line
//! class. The control task therefore cannot await the turn. It hands the watcher a
//! [`ReviewAdjudicationPlan`] (the same shape [`crate::automerge::AutoMergePlan`] is: plain owned
//! data, performed on the far side), and the watcher writes the outcome into the shared
//! [`AdjudicationLedger`] both tasks hold. The control task reads that ledger back on the next
//! sweep, which is what makes "no further round arms" true without a second round-trip.
//!
//! # "Ship it" adjudicates the findings, NEVER the gates
//!
//! This is the ticket's first ⚠️ and the dangerous direction. An adjudication is a statement about
//! the OPEN REVIEW FINDINGS: are they blocking? It is not a merge. CI, approval-at-head, a draft, a
//! conflict and every other gate remain preconditions afterwards exactly as before — a manager that
//! could merge a red pull request would be worse than the loop it replaced. Nothing in this module
//! touches a merge gate; [`crate::reviewwatch`] only stops arming rounds once a verdict is present,
//! and the ordinary auto-merge path re-applies every gate on its own.
//!
//! # The decision is DURABLE; the in-flight marker deliberately is not (STUDIO-956, round 8)
//!
//! A decision the daemon forgets on restart is not a decision. The ledger therefore writes every
//! SETTLED verdict through to `rhapsody_review_bound` beside the pull request's round counter, and
//! [`crate::orchestrator::Orchestrator::rehydrate_review_bounds`] seeds it back at boot — measured
//! need: on 2026-09-20 five restarts in one day each refunded seven in-flight pull requests their
//! whole budget, and a pull request that had already been escalated resumed the loop from zero.
//!
//! The [`Adjudication::InFlight`] marker is the one thing that is NOT persisted, and that asymmetry
//! is load-bearing. It means "a turn is out right now, do not ask again", and the process that was
//! going to land it is exactly what a restart destroys. Persisted, it would stop every further
//! round for that pull request forever with no turn left anywhere to clear it — a permanent freeze
//! in place of the temporary refund this ticket is fixing. Unpersisted, a restart mid-turn costs
//! one re-asked turn. The failure tally ([`MAX_ADJUDICATION_ATTEMPTS`]) is per-boot for the same
//! reason: it only bounds the re-asking of an in-flight decision that never landed.
//!
//! # Its own gate, not `manager.mode`
//!
//! `manager.mode: labels` means there is no manager ASSIGNMENT turn today — assignment is
//! deterministic and spends nothing. An adjudication needs a turn, so it must not silently inherit
//! that mode. It does not: it is gated by `review.adjudicate_after_rounds` alone, and runs through
//! the daemon's one model-turn path ([`crate::triage::run_turn`]) with `manager.model` /
//! `manager.timeout_ms`. A `labels`-mode install that sets the key gets adjudication; one that does
//! not gets today's behaviour byte-for-byte.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use rhapsody_config::room::{Message, RoomLog};
use rhapsody_config::teams::{ReviewModelChoice, Teams};
use rhapsody_store::{
    REVIEW_ADJUDICATION_ESCALATE, REVIEW_ADJUDICATION_SHIP, ReviewAdjudication, Store,
};

use crate::ghsummons::PrCommentSink;
use crate::prstate::PrCoord;
use crate::reviewwatch::churn_key;

/// The `from` every adjudication post is host-stamped with — [`crate::triage::MANAGER_IDENTITY`]'s
/// value, restated rather than imported because that one is `pub(crate)` to the triage module and
/// the manager is one function however many of its halves exist.
pub const MANAGER_IDENTITY: &str = "@manager";

/// How many times a pull request's manager turn may FAIL before the loop stops re-asking and
/// ESCALATES the whole decision to a human (STUDIO-956).
///
/// A failed turn clears the in-flight marker so the next sweep re-asks — correct — but nothing used
/// to count the attempts, so a misconfigured `manager.model` or a broken `claude` command bought one
/// turn spawn per pull request per sweep, indefinitely. Three matches the rest of this subsystem's
/// treatment of an operation that cannot succeed: retry a bounded few times, then say so loudly
/// rather than loop.
pub const MAX_ADJUDICATION_ATTEMPTS: usize = 3;

/// The manager's one decision about a pull request that has run out its round threshold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The remaining open findings do not block. The loop stops; the pull request proceeds to the
    /// normal merge gates, which are untouched.
    Ship,
    /// A human is needed. The reason is the manager's own words; the plan's findings are named
    /// beside it wherever this is recorded.
    Escalate { reason: String },
}

/// One pull request the control task has handed the manager to adjudicate.
///
/// It carries what the escalation must name: the head the loop stopped at, how many rounds it ran,
/// and the open findings. Everything else the turn needs (the model, the command, the timeout) is
/// installation config and lives on [`AdjudicationDeps`], not per plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewAdjudicationPlan {
    pub pr: PrCoord,
    /// The head the loop stopped at.
    pub head: String,
    /// How many review↔author rounds the pull request ran before the threshold.
    pub rounds: usize,
    /// The decision-relevant open facts at the threshold, one human-readable line per live watch
    /// row. Not only "verdicts at this exact head": the head may have moved since the last review —
    /// an author summoned by that review's findings pushes before their run ends — in which case the
    /// line says which head was last read and that nobody has read the new one. It is verdict-neutral
    /// there because a head advance re-arms the row without preserving whether the old read was
    /// findings or an approval. The manager names these on an escalation, so an operator gets the
    /// specific findings rather than "needs a human".
    pub findings: Vec<String>,
    /// Whether a `ship` is AVAILABLE for this plan (STUDIO-977, C): true only when every required
    /// reviewer has READ the change `head` carries — an `approved` verdict or a `reviewed` one
    /// (findings) at `head` or at a head proven by patch-id to carry the same change. Decided on the
    /// control task from the watch rows ([`crate::automerge::ship_available`]) — a manager must not
    /// be able to ship a change nobody read, and making that structural rather than a turn-by-turn
    /// judgement is the point: when this is false, [`perform_adjudication`] records an escalation
    /// whatever the model answered. A `reviewed` row is READ, so it does NOT make `ship` unavailable
    /// — adjudicating findings is the manager's job (STUDIO-956); whether a shipped pull request may
    /// MERGE is the merge gate's, a separate question.
    pub ship_available: bool,
    /// Why `ship` was unavailable, when it was — the specific gate that failed, computed on the
    /// control task ([`crate::automerge::AutoMergeRefusal::why`]). Empty when `ship_available` is
    /// true. Carried so the escalation names the real reason ("a review round is still owed at this
    /// head") rather than a false "nobody read it".
    pub ship_unavailable_reason: String,
}

/// What the manager is asked, and the bounds it is asked under. The turn parameters mirror
/// [`crate::triage::TriageRequest`]'s so the one daemon model-turn path can serve both.
#[derive(Debug, Clone)]
pub struct AdjudicationRequest {
    pub pr: String,
    pub head: String,
    pub rounds: usize,
    pub findings: Vec<String>,
    pub command: String,
    pub billing_guard: bool,
    pub tracker_api_key: String,
    pub model: String,
    pub timeout: Duration,
}

/// The injectable model-turn seam, exactly as [`crate::triage::TriageArbiter`] is for assignment:
/// production installs [`ClaudeReviewAdjudicator`], tests inject a fake and never shell out.
#[async_trait]
pub trait ReviewAdjudicator: Send + Sync {
    /// Runs ONE bounded turn and returns the manager's decision. The implementation MUST bound
    /// itself by `req.timeout`; `Err` is the operator-facing reason and is treated as "no decision"
    /// — the in-flight marker is cleared so the next sweep asks again.
    async fn adjudicate(&self, req: &AdjudicationRequest) -> Result<Verdict, String>;
}

/// The production adjudicator: the same `claude -p` turn [`crate::triage`] uses, differing only in
/// prompt and answer shape.
#[derive(Debug, Default, Clone)]
pub struct ClaudeReviewAdjudicator;

#[async_trait]
impl ReviewAdjudicator for ClaudeReviewAdjudicator {
    async fn adjudicate(&self, req: &AdjudicationRequest) -> Result<Verdict, String> {
        let turn = crate::triage::TriageRequest {
            command: req.command.clone(),
            billing_guard: req.billing_guard,
            tracker_api_key: req.tracker_api_key.clone(),
            model: req.model.clone(),
            timeout: req.timeout,
            prompt: adjudication_prompt(req),
        };
        parse_verdict(&crate::triage::run_turn(&turn).await?)
    }
}

/// The state one pull request's adjudication is in.
///
/// A three-state enum rather than a `Option<Verdict>` because "the turn is running" is the state
/// that keeps the loop from re-requesting a decision every tick, and it is distinguishable from
/// "no decision yet" only before the plan is handed over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Adjudication {
    /// The plan is out for a decision; no round may arm until it lands.
    InFlight { rounds: usize },
    /// The manager shipped it. No further round arms.
    Ship { head: String, rounds: usize },
    /// The manager escalated it. No further round arms; a human is needed and the findings are
    /// named. `reason` is the manager's own words — carried here so it reaches the ledger and the
    /// reconciliation WARN, not only the room post and the pull-request comment.
    Escalate {
        head: String,
        rounds: usize,
        findings: Vec<String>,
        reason: String,
    },
}

impl Adjudication {
    /// Whether this is a settled decision (as opposed to one still being made).
    pub fn settled(&self) -> bool {
        !matches!(self, Adjudication::InFlight { .. })
    }

    /// The head the loop stopped at, or `""` while the decision is still in flight.
    pub fn head(&self) -> &str {
        match self {
            Adjudication::InFlight { .. } => "",
            Adjudication::Ship { head, .. } | Adjudication::Escalate { head, .. } => head,
        }
    }

    /// The round count the decision was made at.
    pub fn rounds(&self) -> usize {
        match self {
            Adjudication::InFlight { rounds }
            | Adjudication::Ship { rounds, .. }
            | Adjudication::Escalate { rounds, .. } => *rounds,
        }
    }

    /// Whether this decision still governs a pull request now at `head` (STUDIO-971).
    ///
    /// A decision is a statement about ONE head — the head the loop stopped at. It keeps the loop
    /// stopped only while the branch still is, or is again, that head. A later head carrying new
    /// work is a head the manager never adjudicated: the code the verdict describes no longer
    /// exists on the branch, so the loop must resume. `unchanged_from` is STUDIO-960's proof that a
    /// head move carried no new work — the change against the base is the same (a patch-id
    /// comparison, STUDIO-977) as the one the verdict was made against — and a move that changed
    /// nothing does not re-open the decision.
    ///
    /// **The proof must cover the ADJUDICATED head, not merely any head.** `unchanged_from` lists
    /// individual historical reviewed SHAs whose patch matches the new head; it is not a PR-wide
    /// statement. `handle_review_head_advanced` carries only the rows whose own
    /// `last_reviewed_sha` appears in it, so a single matching row can be carried while an
    /// unmatched row is re-armed and still owes a review of the new head. Reading a non-empty list
    /// as proof for the whole decision would suppress that owed row. A `Ship` therefore survives
    /// the move only when the head it was made at is itself proven unchanged to the new head
    /// (`unchanged_from` contains [`Self::head`]).
    ///
    /// Only a [`Verdict::Ship`] can stop governing. An [`Adjudication::Escalate`] names a HUMAN as
    /// the next actor; resuming it on a push the author made themselves would mean the escalation
    /// never reaches that human, so an escalation governs however far the head moves.
    ///
    /// An in-flight marker governs too — a turn is out right now — but callers test
    /// [`Self::settled`] first, because the resume question is about a LANDED verdict.
    pub fn governs(&self, head: &str, unchanged_from: &[String]) -> bool {
        match self {
            Adjudication::InFlight { .. } => true,
            // A push the author made themselves must never cancel a human escalation.
            Adjudication::Escalate { .. } => true,
            // The decision's own head, or a no-op move that PROVES that head is what the branch now
            // carries. Any other non-empty list is a partial proof for some other row's history.
            Adjudication::Ship { .. } => {
                self.head() == head || unchanged_from.iter().any(|old| old == self.head())
            }
        }
    }

    /// The durable form of a SETTLED decision, or `None` for one still in flight — see the module
    /// doc on why an in-flight marker must never reach the store.
    fn to_stored(&self) -> Option<ReviewAdjudication> {
        match self {
            Adjudication::InFlight { .. } => None,
            Adjudication::Ship { head, rounds } => Some(ReviewAdjudication {
                decision: REVIEW_ADJUDICATION_SHIP.to_string(),
                head: head.clone(),
                rounds: *rounds as i64,
                findings: Vec::new(),
                reason: String::new(),
            }),
            Adjudication::Escalate {
                head,
                rounds,
                findings,
                reason,
            } => Some(ReviewAdjudication {
                decision: REVIEW_ADJUDICATION_ESCALATE.to_string(),
                head: head.clone(),
                rounds: *rounds as i64,
                findings: findings.clone(),
                reason: reason.clone(),
            }),
        }
    }

    /// Rebuilds a settled decision from its durable form. `None` for a `decision` token this build
    /// does not know — the fail-open direction here, because the alternative is a pull request
    /// stopped forever by a value no branch matches (see `Store::load_review_bounds`, which already
    /// refuses to hand one over).
    pub(crate) fn from_stored(stored: &ReviewAdjudication) -> Option<Adjudication> {
        let rounds = stored.rounds.max(0) as usize;
        match stored.decision.as_str() {
            REVIEW_ADJUDICATION_SHIP => Some(Adjudication::Ship {
                head: stored.head.clone(),
                rounds,
            }),
            REVIEW_ADJUDICATION_ESCALATE => Some(Adjudication::Escalate {
                head: stored.head.clone(),
                rounds,
                findings: stored.findings.clone(),
                reason: stored.reason.clone(),
            }),
            _ => None,
        }
    }
}

/// What each pull request's adjudication is, shared between the control task (which reads it to
/// stop arming and to report) and the watcher's task (which writes it after the turn).
///
/// One `Mutex`-guarded map with no `.await` ever held across the lock, mirroring
/// [`crate::runautomerge::AutoMergeLedger`] — the control task only ever takes it briefly, and never
/// while awaiting.
#[derive(Default)]
pub struct AdjudicationLedger {
    entries: Mutex<HashMap<String, Adjudication>>,
    /// How many turns have FAILED per pull request since its last successful decision or its last
    /// `clear`. Kept apart from `entries` because a failure deliberately CLEARS the entry so the
    /// next sweep re-asks; this map is the memory that bounds the re-asking
    /// ([`MAX_ADJUDICATION_ATTEMPTS`]).
    failures: Mutex<HashMap<String, usize>>,
    /// Where a SETTLED decision is written so it survives a restart (STUDIO-956). `None` leaves the
    /// ledger exactly as it was before durability — per-boot — which is what the `Default` used by
    /// the tests and by a daemon built without a store gets.
    ///
    /// The ledger WRITES through this handle and never reads it: the boot read is
    /// [`crate::orchestrator::Orchestrator::rehydrate_review_bounds`]'s, on the control task, which
    /// seeds this map through [`AdjudicationLedger::seed`]. One reader keeps the rehydration
    /// ordered against the round counter's, which comes from the same rows.
    store: Option<Arc<dyn Store + Send + Sync>>,
}

impl std::fmt::Debug for AdjudicationLedger {
    /// Hand-written because `dyn Store` is not `Debug`; the handle is named by presence, which is
    /// the only thing about it a diagnostic could use.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdjudicationLedger")
            .field("entries", &self.entries)
            .field("failures", &self.failures)
            .field("durable", &self.store.is_some())
            .finish()
    }
}

impl AdjudicationLedger {
    /// A ledger whose settled decisions are written through to `store` (STUDIO-956). The daemon's
    /// composition root builds it with the same store handle the orchestrator holds, so the
    /// decision and the round counter it belongs beside land in one row.
    pub fn with_store(store: Arc<dyn Store + Send + Sync>) -> Self {
        Self {
            store: Some(store),
            ..Self::default()
        }
    }

    /// Installs a decision read back from the store at boot, WITHOUT writing it out again — the one
    /// entry point [`crate::orchestrator::Orchestrator::rehydrate_review_bounds`] uses. Never
    /// overwrites an entry already present: a decision this process has made is newer than the one
    /// on disk by construction.
    pub fn seed(&self, pr_key: &str, adjudication: Adjudication) {
        self.map().entry(pr_key.to_string()).or_insert(adjudication);
    }

    /// Locks the map, treating a poisoned lock as readable — [`crate::triage::TriageHandle`]'s
    /// stance: a panic in a two-line critical section cannot leave the map logically inconsistent,
    /// and refusing to read it would turn a cosmetic fault into a re-run loop.
    fn map(&self) -> std::sync::MutexGuard<'_, HashMap<String, Adjudication>> {
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Records that `pr`'s plan has been handed out for a decision, unless a decision (or another
    /// in-flight plan) is already there. Idempotent: the control task may reach this twice in a tick
    /// and the second call must not overwrite a landed verdict.
    pub fn mark_in_flight(&self, pr: &PrCoord, rounds: usize) {
        let mut m = self.map();
        m.entry(churn_key(pr))
            .or_insert(Adjudication::InFlight { rounds });
    }

    /// Records a settled decision for `pr`, and forgets any failed attempts: a decision that
    /// finally landed is not a decision that is still failing.
    pub fn record(&self, pr: &PrCoord, adjudication: Adjudication) {
        let key = churn_key(pr);
        // The two maps are locked one after the other, never at once: `self.map()` is a
        // statement-temporary whose guard drops at the semicolon, so the same `entries`-then-
        // `failures` order here as in `note_failure` and `clear` is a consistency of reading rather
        // than a deadlock rule. Holding one guard across the other would CREATE the deadlock the
        // old comment claimed to prevent — don't "tidy" this into a single expression expecting the
        // guards to stay ordered.
        // Durable BEFORE the in-memory insert (STUDIO-956). The control task reads `entries` to
        // decide that no further round arms, so a decision visible in memory but not on disk is
        // exactly the window a restart turns into a forgotten verdict and a resumed loop. Writing
        // first makes the durable state the leading edge; a write that fails still lands in memory,
        // so the decision is honoured for this lifetime and the daemon says so.
        if let (Some(store), Some(stored)) = (self.store.as_ref(), adjudication.to_stored())
            && let Err(e) = store.record_review_adjudication(&key, &stored)
        {
            tracing::warn!(
                pr = %key, err = %e,
                "ticketless review: the manager's decision could not be persisted; it holds for \
                 this daemon lifetime only"
            );
        }
        self.map().insert(key.clone(), adjudication);
        self.failures
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&key);
    }

    /// Records that `pr`'s turn FAILED — clearing the in-flight marker so the next sweep re-asks,
    /// and returning how many times it has now failed since the last success or [`Self::clear`].
    pub fn note_failure(&self, pr: &PrCoord) -> usize {
        let key = churn_key(pr);
        self.map().remove(&key);
        let mut f = self.failures.lock().unwrap_or_else(|e| e.into_inner());
        let count = f.entry(key).or_insert(0);
        *count += 1;
        *count
    }

    /// How many times `pr`'s turn has failed without a decision.
    pub fn failures(&self, pr: &PrCoord) -> usize {
        self.failures
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&churn_key(pr))
            .copied()
            .unwrap_or(0)
    }

    /// What is known about `pr`, or `None` when nothing is.
    pub fn peek(&self, pr: &PrCoord) -> Option<Adjudication> {
        self.map().get(&churn_key(pr)).cloned()
    }

    /// Forgets `pr` — used when a pull request leaves the watch set and by the operator's Clear, so
    /// the failure tally goes with it and a fresh adjudication may be attempted. Returns whether
    /// anything was there to forget, which the operator's Clear uses to answer `Applied` when it
    /// dropped a decision (or a failure tally) but no counter.
    pub fn clear(&self, pr: &PrCoord) -> bool {
        let key = churn_key(pr);
        // The decision only — never the round counter beside it in the same row. The operator's
        // re-run clears a decision while REFUNDING one round rather than resetting the budget, and
        // the callers that do want both gone call `Orchestrator::forget_review_bound` as well.
        if let Some(store) = self.store.as_ref()
            && let Err(e) = store.clear_review_adjudication(&key)
        {
            tracing::warn!(pr = %pr, err = %e, "ticketless review: the manager's decision could not be cleared durably");
        }
        let had_entry = self.map().remove(&key).is_some();
        let had_failure = self
            .failures
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&key)
            .is_some();
        had_entry || had_failure
    }
}

/// Everything an off-loop adjudication needs that is installation config rather than per plan.
pub struct AdjudicationDeps {
    pub adjudicator: Arc<dyn ReviewAdjudicator>,
    /// The room the decision is audited in. `None` when there is no on-disk runtime home — the
    /// decision still lands on the pull request and in the ledger.
    pub room: Option<Arc<dyn RoomLog>>,
    /// Where the decision is recorded on the pull request. `None` disables only that half.
    pub comments: Option<Arc<dyn PrCommentSink>>,
    pub ledger: Arc<AdjudicationLedger>,
    /// The turn's parameters, captured once at boot beside the Teams config.
    pub turn: AdjudicationTurn,
}

/// The harness an adjudication turn actually runs on. [`ClaudeReviewAdjudicator`] reuses
/// [`crate::triage::run_turn`], which spawns `claude -p`, so the model this turn may be given is a
/// `claude` model and nothing else — see [`adjudication_model`].
const ADJUDICATION_HARNESS: &str = "claude";

/// The model an adjudication turn runs on (STUDIO-956, round-8 finding 4).
///
/// `manager.model` defaults to empty and is empty on the installation that filed this, and
/// [`crate::triage::run_turn`] passes `--model` only when the value is non-empty. So the turn that
/// decides ship-or-escalate was running on whatever the CLI happens to default to, while every
/// review whose findings it is adjudicating ran on the pinned `review.model`. A decision made on a
/// weaker model than the reviews it is deciding about is not one an operator would sign off on, and
/// nothing anywhere said it was happening.
///
/// Resolved in order:
///
/// 1. `manager.model` when set — the operator named a model for the manager's turns and this is one
///    of them, so an explicit choice always wins;
/// 2. else `review.model` scoped to the `claude` harness this turn runs on
///    ([`Teams::review_model_for`]) — the model the reviews themselves ran on, which is the closest
///    thing to "decide this at the same standard it was reviewed at";
/// 3. else empty: the CLI's own default. That is what happened before this change and is the only
///    honest answer on an installation that has pinned nothing at all.
///
/// A `review.model` scoped to OTHER harnesses only ([`ReviewModelChoice::Refuse`]) does NOT apply:
/// handing a `claude` turn a model scoped to `opencode` is precisely the mistake STUDIO-908 exists
/// to prevent. It falls through to (3) rather than refusing the turn — refusing would freeze the
/// loop at the threshold with no decision, which is the outcome this whole feature replaces.
pub fn adjudication_model(teams: &Teams, backend: &str) -> String {
    if !teams.manager.model.trim().is_empty() {
        return teams.manager.model.clone();
    }
    match teams.review_model_for(ADJUDICATION_HARNESS, backend) {
        ReviewModelChoice::Use(model) => model.to_string(),
        ReviewModelChoice::Inherit | ReviewModelChoice::Refuse(_) => String::new(),
    }
}

/// The installation-wide turn parameters an adjudication runs under, captured at the composition
/// root from `manager.*`.
#[derive(Debug, Clone)]
pub struct AdjudicationTurn {
    pub command: String,
    pub billing_guard: bool,
    pub tracker_api_key: String,
    /// Resolved by [`adjudication_model`], NOT read raw from `manager.model` — see that function for
    /// why an empty `manager.model` must not silently decide on the CLI default.
    pub model: String,
    pub timeout: Duration,
}

/// Asks the manager to adjudicate ONE plan, off the control task, and records the outcome.
///
/// Infallible by contract, like [`crate::reviewwatch::ReviewWatchSink::merge`]: there is no caller
/// with anything to do about a failure. A failed turn is NOT a decision — the in-flight marker is
/// cleared so a later sweep re-asks, and the log says so.
pub async fn perform_adjudication(
    plan: &ReviewAdjudicationPlan,
    deps: &AdjudicationDeps,
    at: DateTime<Utc>,
) {
    let req = AdjudicationRequest {
        pr: plan.pr.to_string(),
        head: plan.head.clone(),
        rounds: plan.rounds,
        findings: plan.findings.clone(),
        command: deps.turn.command.clone(),
        billing_guard: deps.turn.billing_guard,
        tracker_api_key: deps.turn.tracker_api_key.clone(),
        model: deps.turn.model.clone(),
        timeout: deps.turn.timeout,
    };
    let verdict = match deps.adjudicator.adjudicate(&req).await {
        Ok(v) => v,
        Err(e) => {
            // Not a decision yet: clear the in-flight marker so the next sweep re-asks, and count
            // the attempt so a turn that can never succeed is bounded rather than re-spawned
            // forever.
            let attempts = deps.ledger.note_failure(&plan.pr);
            if attempts < MAX_ADJUDICATION_ATTEMPTS {
                tracing::warn!(
                    pr = %plan.pr,
                    err = %e,
                    attempts,
                    "review adjudication: the manager turn failed; the loop stays stopped and the \
                     decision is re-asked on a later sweep"
                );
                return;
            }
            // Bounded: stop re-asking and escalate to a human, through the SAME audit path every
            // other decision takes. A control-task-only escalation existed before this and reached
            // neither the room nor the pull request — the one escalation an operator most needs
            // pushed at them, because its cause is "your manager turn is broken", not "this review
            // is hard". Recording it here means the ledger, the room post and the comment can never
            // disagree, and the control task's settled-entry check stops arming on its own.
            tracing::warn!(
                pr = %plan.pr,
                err = %e,
                attempts,
                "review adjudication: the manager turn failed its bounded attempts; escalating the \
                 decision to a human"
            );
            Verdict::Escalate {
                reason: format!(
                    "the manager turn failed {attempts} times; no decision could be made"
                ),
            }
        }
    };

    // STUDIO-977 C, the structural half: `ship` is not available when not every required reviewer
    // has READ the change at `head` (a still-owed round, no watch row at all, a status this daemon
    // cannot read, or a verdict whose head was never proven the same). A manager that answers SHIP
    // over a change nobody read would stop the loop on a head nobody has looked at — the deadlock
    // `#209`/`#203` reached — so the answer is upgraded to an escalation here, where the two audit
    // writes and the ledger cannot disagree about it. A `reviewed` row is READ and does NOT block a
    // ship: the manager adjudicates findings, and the merge gate separately decides mergeability
    // (STUDIO-956 preserved). This is deliberately NOT a judgement re-derived inside the prompt each
    // turn, and the reason names the gate that actually failed rather than claiming nobody read it.
    let verdict = match verdict {
        Verdict::Ship if !plan.ship_available => {
            let reason = if plan.ship_unavailable_reason.trim().is_empty() {
                ship_unavailable_reason(&plan.head)
            } else {
                plan.ship_unavailable_reason.clone()
            };
            tracing::info!(
                pr = %plan.pr,
                head = %plan.head,
                reason = %reason,
                "review adjudication: the manager shipped, but approval-at-head does not hold for \
                 this change; escalating to a human instead"
            );
            Verdict::Escalate { reason }
        }
        v => v,
    };

    let body = decision_body(plan, &verdict);
    let adjudication = match verdict {
        Verdict::Ship => Adjudication::Ship {
            head: plan.head.clone(),
            rounds: plan.rounds,
        },
        Verdict::Escalate { reason } => Adjudication::Escalate {
            head: plan.head.clone(),
            rounds: plan.rounds,
            findings: plan.findings.clone(),
            reason,
        },
    };
    // Settle the ledger BEFORE the two audit writes. On the bounded-failure path above,
    // `note_failure` has just REMOVED the in-flight marker, so recording after the writes would
    // leave the ledger reading "no decision, not in flight, N failures" for the whole duration of
    // the (unbounded) `post_pr_comment`. Nothing on the control task reads the failure tally any
    // more, so that window is one in which `service_review_pr`'s settled-entry and threshold checks
    // BOTH fall through and a FOURTH manager turn is handed out — a turn that either erases the
    // settled escalation (`note_failure` removes the entry unconditionally) or overturns it with a
    // contradicting `SHIP`. Recording first makes the window unrepresentable rather than merely
    // brief. The success path's `InFlight` marker already survives its awaits, so this ordering is
    // only load-bearing for the failure path — but one ordering for both keeps them from drifting.
    deps.ledger.record(&plan.pr, adjudication.clone());

    let refs = vec![plan.pr.to_string()];
    if let Some(room) = deps.room.as_ref() {
        let mut msg = Message::room(MANAGER_IDENTITY, at, body.clone());
        msg.refs = refs.clone();
        if let Err(e) = room.append(&msg) {
            tracing::warn!(
                pr = %plan.pr,
                err = %e,
                "review adjudication: the decision could not be posted to the room; the pull \
                 request comment and the ledger are unaffected"
            );
        }
    }
    if let Some(comments) = deps.comments.as_ref()
        && let Err(e) = comments
            .post_pr_comment(&plan.pr.owner, &plan.pr.repo, plan.pr.number, &body)
            .await
    {
        tracing::warn!(
            pr = %plan.pr,
            err = %e,
            "review adjudication: the decision could not be recorded on the pull request; the \
             room post and the ledger are unaffected"
        );
    }

    let outcome = match &adjudication {
        Adjudication::Ship { .. } => "ship",
        Adjudication::Escalate { .. } => "escalate",
        Adjudication::InFlight { .. } => "in flight",
    };
    tracing::info!(
        pr = %plan.pr,
        head = %plan.head,
        rounds = plan.rounds,
        verdict = outcome,
        findings = plan.findings.len(),
        "review adjudication: the manager decided"
    );
}

/// The prompt the manager answers. Names the pull request, the head, the round count and every open
/// finding, and asks for exactly one of the two decisions.
pub fn adjudication_prompt(req: &AdjudicationRequest) -> String {
    let findings = if req.findings.is_empty() {
        "(none recorded)".to_string()
    } else {
        req.findings
            .iter()
            .map(|f| format!("- {f}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "You are the engineering manager for a software team. A pull request's review↔author loop \
         has run {rounds} rounds and reached its limit without converging. Decide ONE thing.\n\n\
         Pull request: {pr}\nHead: {head}\nRounds: {rounds}\nOpen findings:\n{findings}\n\n\
         Answer with exactly one line, one of:\n\
         SHIP\n\
         ESCALATE: <the specific reason a human is needed>\n\n\
         SHIP means the remaining open findings do not block and the pull request may proceed to \
         the normal merge gates (this does NOT merge it — CI, approvals and conflicts are still \
         checked separately). ESCALATE means a human must decide; name the specific findings, not \
         'needs a human'.",
        rounds = req.rounds,
        pr = req.pr,
        head = req.head,
        findings = findings,
    )
}

/// Reads `SHIP` or `ESCALATE: …` out of the turn's stdout.
///
/// Lenient about surrounding prose and punctuation, strict about the decision. It scans every line
/// and RANKS each decision-shaped line by how strongly it reads as the ANSWER rather than a mention
/// of the option the reply rejected, then keeps the strongest; ties break on position, so the last
/// of two equally-strong lines wins.
///
/// The ranks, highest first:
///
/// * **2 — an explicit `Decision:`/`Verdict:` label.** The model marked this line as its answer, so
///   it outranks every line that lacks one.
/// * **1 — an undecorated decision in exactly the form the prompt asked for**: a bare `SHIP`, or
///   `ESCALATE: <reason>`. A plain line is an answer.
/// * **0 — decorated, or a `SHIP: <prose>` line.** A bullet or emphasis is as likely a bulleted
///   mention of the option the reply rejected as an answer; the prompt asks for a bare `SHIP`, so
///   `SHIP: <prose>` is at least as likely an enumeration of the option being explained away as it
///   is the answer. Neither outranks the other, and neither outranks a real answer.
///
/// The ranking replaces a plain-beats-decorated rule that got the axis wrong: it let a plain
/// `SHIP: <prose>` explanatory line outrank a decorated `ESCALATE` answer below it, so a reply that
/// escalated shipped. It also treats the label as an answer marker rather than as decoration, so
/// `Decision: SHIP` beside a bulleted `ESCALATE` is a decision rather than an ambiguity.
///
/// A `SHIP` line must BE the decision — a bare `SHIP`, a `SHIP:` prefix, or a labelled answer — not
/// merely begin with the word. The prompt shows the model "SHIP means …" verbatim, and a line of
/// prose beginning `SHIP ` is an explanation, not an answer.
///
/// Leading markdown decoration and an optional `Decision:`/`Verdict:` label are stripped first
/// ([`strip_answer_decoration`]), because they carry no decision content but otherwise pushed an
/// obviously-correct reply into the error path.
///
/// An answer that names BOTH decisions and does so only weakly — every decision word behind a
/// bullet, an emphasis, or a `SHIP:` explanation — is ambiguous, not a decision: that is the shape
/// of a reply that merely ENUMERATES the two options ("- ESCALATE: …\n- SHIP: …"), and the last of
/// them would otherwise be read as the answer. It is an error, so the caller re-asks rather than
/// guessing.
///
/// An answer naming neither decision is an error, and the caller re-asks rather than guessing.
pub fn parse_verdict(stdout: &str) -> Result<Verdict, String> {
    // The strongest decision seen so far and its line's rank. `>=` below keeps the LAST of two
    // equally-ranked decisions, which is pure last-match among equals — what the module doc
    // promises for a reply that lays out both options before stating which one it picks.
    let mut best: Option<(u8, Verdict)> = None;
    let mut saw_ship = false;
    let mut saw_escalate = false;
    for raw in stdout.lines() {
        let (line, labelled, decorated) = strip_answer_decoration(raw);
        let line = line.as_str();
        if line.is_empty() {
            continue;
        }
        let upper = line.to_ascii_uppercase();
        let bare = upper.trim_end_matches(['.', '!', '*', '`', ' ']);
        // `requested_form` is whether the line is a decision in the exact shape the prompt asked
        // for. A `SHIP: <prose>` line is deliberately NOT one: the prompt asks for a bare `SHIP`,
        // and the explanation form is at least as likely to enumerate the rejected option.
        let (verdict, requested_form) = if bare == "SHIP" {
            (Verdict::Ship, true)
        } else if upper.starts_with("SHIP:") {
            (Verdict::Ship, false)
        } else if upper.starts_with("ESCALATE:") {
            let rest = line
                .get("ESCALATE:".len()..)
                .map(|r| r.trim().trim_end_matches(['*', '`', ' ']).trim())
                .unwrap_or_default();
            (
                Verdict::Escalate {
                    reason: if rest.is_empty() {
                        "the manager escalated without stating a reason".to_string()
                    } else {
                        rest.to_string()
                    },
                },
                true,
            )
        } else if bare == "ESCALATE" {
            (
                Verdict::Escalate {
                    reason: "the manager escalated without stating a reason".to_string(),
                },
                true,
            )
        } else {
            continue;
        };
        if matches!(&verdict, Verdict::Ship) {
            saw_ship = true;
        } else {
            saw_escalate = true;
        }
        let rank = if labelled {
            2
        } else if requested_form && !decorated {
            1
        } else {
            0
        };
        if best.as_ref().map(|(r, _)| rank >= *r).unwrap_or(true) {
            best = Some((rank, verdict));
        }
    }
    let (rank, verdict) = best.ok_or_else(|| {
        format!(
            "adjudication reply named neither SHIP nor ESCALATE: {}",
            snippet(stdout)
        )
    })?;
    if saw_ship && saw_escalate && rank == 0 {
        return Err(format!(
            "adjudication reply named BOTH decisions, each only weakly (decorated, or a `SHIP:` \
             explanation) rather than as an answer: {}",
            snippet(stdout)
        ));
    }
    Ok(verdict)
}

/// Strips the leading markdown/quoting decoration and an optional `Decision:`/`Verdict:` label a
/// model routinely wraps its one-line answer in, so `**SHIP**`, `- SHIP` and `Decision: SHIP` are
/// read as the decisions they are. Returns the undecorated content, whether an explicit
/// `Decision:`/`Verdict:` label came off, and whether any other decoration was stripped.
///
/// The label is reported separately because it is an ANSWER marker, not decoration:
/// [`parse_verdict`] ranks a labelled line above an unlabelled one, since a model that writes
/// `Decision: SHIP` has named the line as its answer. Treating the label itself as decoration (the
/// old behaviour) ranked the most explicit answer below any plain decision-shaped line above it,
/// so a labelled `ESCALATE` lost to the prompt's own echoed `SHIP` menu line.
///
/// Deliberately not a prose scanner: it removes LEADING decoration only, so a line that begins
/// `SHIP ` still carries its explanation and is still not a decision. Three such replies used to be
/// Err, which the caller counts as a failed turn and blames the model for being unreachable when it
/// answered clearly — a misdiagnosis, not a safety property.
fn strip_answer_decoration(line: &str) -> (String, bool, bool) {
    // Decoration can sit on either side of the label (`**Decision: SHIP**`), so this is applied
    // again after the label comes off.
    fn undecorate(s: &str) -> (&str, bool) {
        let trimmed = s.trim();
        let stripped = trimmed.trim_start_matches(['*', '_', '#', '>', '-', ' ']);
        (stripped, stripped.len() != trimmed.len())
    }
    let (s, decorated) = undecorate(line);
    let upper = s.to_ascii_uppercase();
    for label in ["DECISION:", "VERDICT:"] {
        if upper.starts_with(label) {
            // Byte-slicing is safe here: `starts_with` proved the prefix is these ASCII bytes.
            return (
                undecorate(s.get(label.len()..).unwrap_or("")).0.to_string(),
                true,
                decorated,
            );
        }
    }
    (s.to_string(), false, decorated)
}

/// A short, single-line excerpt of a reply for an error message, so a long transcript does not land
/// in one log line.
fn snippet(s: &str) -> String {
    let one = s.trim().replace('\n', " ");
    one.chars().take(160).collect()
}

/// Why a manager's `ship` became an escalation, used only when the plan carried no more specific
/// reason: not every required reviewer has READ the change at `head`, so there is nothing a ship
/// could stand in for (STUDIO-977, C).
pub fn ship_unavailable_reason(head: &str) -> String {
    format!(
        "the manager shipped, but not every required reviewer has read the change at `{}`; a human \
         must decide",
        head.trim()
    )
}

/// The human-readable decision, posted to the room and the pull request: which way it went, and why.
///
/// The escalation names the head, the round count and every open finding — the ticket's third ⚠️.
pub fn decision_body(plan: &ReviewAdjudicationPlan, verdict: &Verdict) -> String {
    let findings = if plan.findings.is_empty() {
        "none recorded".to_string()
    } else {
        plan.findings.join("; ")
    };
    match verdict {
        Verdict::Ship => format!(
            "@manager adjudicated {pr} after {rounds} review rounds: **ship it**. The remaining \
             open findings do not block. Head `{head}`. This does not merge the pull request — CI, \
             approval-at-head and conflicts remain the usual gates.\n\nOpen findings: {findings}",
            pr = plan.pr,
            rounds = plan.rounds,
            head = plan.head,
        ),
        Verdict::Escalate { reason } => format!(
            "@manager adjudicated {pr} after {rounds} review rounds: **escalate** — a human is \
             needed. Head `{head}`.\n\nReason: {reason}\nOpen findings: {findings}",
            pr = plan.pr,
            rounds = plan.rounds,
            head = plan.head,
            reason = if reason.trim().is_empty() {
                "not stated"
            } else {
                reason.trim()
            },
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A ticketless-review Teams config with `review.model` pinned the way this installation has
    /// it, and `manager.model` left at its default empty.
    fn adjudicating_teams(review_model: rhapsody_config::teams::HarnessScoped) -> Teams {
        let mut t = Teams::disabled();
        t.enabled = true;
        t.review.mode = rhapsody_config::teams::ReviewMode::Ticketless;
        t.review.model = review_model;
        t
    }

    /// **Round-8 finding 4.** `manager.model` is empty by default and empty on the installation
    /// that filed this, and `run_turn` passes `--model` only when the value is non-empty — so the
    /// ship-or-escalate decision was being made on the CLI's default while every review it was
    /// adjudicating ran on the pinned `review.model`. It now inherits that review model.
    ///
    /// MUTATION: return `teams.manager.model.clone()` unconditionally (the pre-fix behaviour) and
    /// this reds.
    #[test]
    fn an_unset_manager_model_falls_back_to_the_pinned_review_model() {
        let teams =
            adjudicating_teams(rhapsody_config::teams::HarnessScoped::bare("claude-opus-5"));
        assert_eq!(
            teams.manager.model, "",
            "the default this install actually has"
        );
        assert_eq!(adjudication_model(&teams, "claude"), "claude-opus-5");
    }

    /// An explicit `manager.model` always wins: the operator named a model for the manager's turns
    /// and an adjudication is one of them.
    #[test]
    fn an_explicit_manager_model_wins_over_the_review_model() {
        let mut teams =
            adjudicating_teams(rhapsody_config::teams::HarnessScoped::bare("claude-opus-5"));
        teams.manager.model = "claude-haiku-5".to_string();
        assert_eq!(adjudication_model(&teams, "claude"), "claude-haiku-5");
    }

    /// A `review.model` scoped to ANOTHER harness does not apply — the turn is a `claude -p` one,
    /// and handing it an `opencode` model is the mistake STUDIO-908 exists to prevent. It falls
    /// back to the CLI default rather than refusing the turn, because refusing would freeze the
    /// loop at the threshold with no decision at all.
    ///
    /// MUTATION: treat `ReviewModelChoice::Refuse` as a value to use and this reds.
    #[test]
    fn a_review_model_scoped_to_another_harness_is_not_borrowed() {
        let mut scoped = rhapsody_config::teams::HarnessScoped::default();
        scoped.insert("opencode", "some-opencode-model");
        let teams = adjudicating_teams(scoped);
        assert_eq!(
            adjudication_model(&teams, "opencode"),
            "",
            "an opencode model must never reach a claude turn"
        );
    }

    /// An installation that has pinned nothing gets exactly what it got before: no `--model`, the
    /// CLI's own default. The byte-identical-when-unset direction.
    #[test]
    fn an_installation_that_pins_nothing_still_gets_the_cli_default() {
        let teams = adjudicating_teams(rhapsody_config::teams::HarnessScoped::default());
        assert_eq!(adjudication_model(&teams, "claude"), "");
    }

    /// A plan whose change every required reviewer has READ (`ship_available`), which is the
    /// ordinary case this module's tests exercise. The unavailable case has its own plan and test.
    fn plan() -> ReviewAdjudicationPlan {
        ReviewAdjudicationPlan {
            pr: PrCoord::new("makewhatis", "rhapsody", 192),
            head: "be260a6b4366fac70fbc0e2dbabd9d51fe9d44e5".to_string(),
            rounds: 3,
            findings: vec![
                "alice asked for changes at a324d2d".to_string(),
                "bob asked for changes at c366a61".to_string(),
            ],
            ship_available: true,
            ship_unavailable_reason: String::new(),
        }
    }

    // ── the two verdicts parse, and nothing else does ────────────────────────────────────────────

    #[test]
    fn ship_parses_through_prose_and_punctuation() {
        for reply in ["SHIP", "ship", "SHIP.", "  SHIP  ", "SHIP: go"] {
            assert_eq!(parse_verdict(reply), Ok(Verdict::Ship), "({reply:?})");
        }
        // A model that prefixes its answer with a sentence still parses: the decision scans every
        // line, not just the first.
        assert_eq!(
            parse_verdict("Here is my decision.\n\nSHIP"),
            Ok(Verdict::Ship)
        );
    }

    /// **The unsafe direction.** A reply that explains BOTH options before deciding must parse as
    /// its decision, not as the first option it names. Taking the first match read the prompt's own
    /// "SHIP means …" sentence as the verdict and shipped a reply that escalated.
    #[test]
    fn a_reply_that_names_both_decisions_parses_as_the_last_one() {
        let reply = "Let me weigh this.\n\nSHIP means the remaining open findings do not block.\n\
                     ESCALATE means a human must decide.\n\nDecision:\n\
                     ESCALATE: the auth rewrite needs a security owner.";
        assert_eq!(
            parse_verdict(reply),
            Ok(Verdict::Escalate {
                reason: "the auth rewrite needs a security owner.".to_string()
            }),
            "the decision line is the ESCALATE, not the prose above it"
        );

        // …and the other way round: a reply that quotes both and ships must ship.
        let reply = "SHIP means the findings do not block.\nESCALATE: a human is needed.\n\n\
                     Decision:\nSHIP";
        assert_eq!(parse_verdict(reply), Ok(Verdict::Ship));
    }

    /// A line that merely begins with the word is prose, not a decision — the prompt shows the
    /// model "SHIP means …" verbatim. Only a bare `SHIP` or a `SHIP:` prefix is an answer.
    #[test]
    fn a_line_beginning_with_ship_but_continuing_in_prose_is_not_a_decision() {
        assert!(parse_verdict("SHIP means the remaining open findings do not block.").is_err());
    }

    /// The decorations a model routinely wraps its one-line answer in carry no decision content.
    /// Each of these used to be an `Err`, which the caller counts as a failed turn and eventually
    /// auto-escalates with "the manager turn failed 3 times" — blaming an unreachable model for a
    /// reply that answered clearly. Pinned so the strictness is a stated contract rather than an
    /// accident of `trim_end_matches`.
    #[test]
    fn decorated_answers_parse_as_their_decision() {
        for reply in [
            "Decision: SHIP",
            "Verdict: SHIP",
            "**SHIP**",
            "- SHIP",
            "> SHIP",
            "### SHIP",
            "Decision: **SHIP**",
            "**Decision: SHIP**",
        ] {
            assert_eq!(parse_verdict(reply), Ok(Verdict::Ship), "({reply:?})");
        }
        assert_eq!(
            parse_verdict("Decision: ESCALATE: the migration needs a DBA"),
            Ok(Verdict::Escalate {
                reason: "the migration needs a DBA".to_string()
            })
        );
        assert_eq!(
            parse_verdict("**ESCALATE: needs a security owner**"),
            Ok(Verdict::Escalate {
                reason: "needs a security owner".to_string()
            })
        );
    }

    /// **A reply that ENUMERATES both options is not a decision.** Once the leading `- ` strip
    /// landed, a reply that merely lists the two choices parsed as whichever it listed last, in the
    /// unsafe direction when that was `SHIP`. A decision word behind a bullet is a list item; a
    /// reply that lists both and states neither plainly is ambiguous, so the caller re-asks.
    #[test]
    fn a_reply_that_enumerates_both_options_on_decorated_lines_is_ambiguous() {
        for reply in [
            "I weighed both:\n- ESCALATE: the migration needs a DBA\n- SHIP: the rest are nits\n",
            "Options:\n  * ESCALATE: risky\n  * SHIP: fine\n",
            "**SHIP: the rest are nits**\n**ESCALATE: the migration needs a DBA**",
        ] {
            assert!(parse_verdict(reply).is_err(), "({reply:?})");
        }
        // …but one decision stated plainly still wins, even beside a bulleted mention of the other.
        assert_eq!(
            parse_verdict("- ESCALATE: risky\n- SHIP: fine\nMy decision:\nSHIP"),
            Ok(Verdict::Ship)
        );
    }

    /// **The mixed shape.** A decorated list item beside a plain decision resolves to the plain
    /// one, wherever it sits — the ordinary way a model states a call and then lists what it
    /// rejected. Pure last-match read the bullet, so a plainly-stated `ESCALATE` above a bulleted
    /// `SHIP` parsed as `Ship` and the escalation shipped. Pinned in this ordering specifically:
    /// the trailing-plain variant above passes under either rule and so does not discriminate.
    #[test]
    fn a_plain_decision_outranks_a_later_decorated_mention() {
        assert_eq!(
            parse_verdict("ESCALATE: the migration needs a DBA\n- SHIP: would be the alternative"),
            Ok(Verdict::Escalate {
                reason: "the migration needs a DBA".to_string()
            })
        );
        assert_eq!(
            parse_verdict("ESCALATE: needs a human\n**SHIP: not my call**"),
            Ok(Verdict::Escalate {
                reason: "needs a human".to_string()
            })
        );
        assert_eq!(
            parse_verdict(
                "My decision:\nESCALATE: the migration needs a DBA\n- SHIP: the alternative I rejected"
            ),
            Ok(Verdict::Escalate {
                reason: "the migration needs a DBA".to_string()
            })
        );
        // …and the plain `SHIP` above a bulleted `ESCALATE` still ships.
        assert_eq!(
            parse_verdict("SHIP\n- ESCALATE: the alternative"),
            Ok(Verdict::Ship)
        );
    }

    /// **The label is an answer marker, not decoration.** A reply that echoes the prompt's own menu
    /// (`SHIP` / `ESCALATE: <the specific reason a human is needed>`) and then states its answer on
    /// a `Decision:` line must resolve to the labelled line. Ranking the label as decoration made
    /// the echo win instead: the placeholder became the escalation reason, or a plain `SHIP` echo
    /// beat a real `Decision: ESCALATE` and the escalation shipped.
    #[test]
    fn a_labelled_answer_outranks_the_prompts_own_echoed_menu() {
        let echo = "Answer with exactly one line, one of:\nSHIP\n\
                    ESCALATE: <the specific reason a human is needed>\n\n";
        assert_eq!(
            parse_verdict(&format!(
                "{echo}Decision: ESCALATE: the schema migration needs DBA sign-off before this lands"
            )),
            Ok(Verdict::Escalate {
                reason: "the schema migration needs DBA sign-off before this lands".to_string()
            }),
            "the labelled answer is the decision, not the echoed placeholder"
        );
        assert_eq!(
            parse_verdict(&format!("{echo}Decision: SHIP")),
            Ok(Verdict::Ship),
            "a labelled SHIP is the answer even under the prompt's echoed ESCALATE menu line"
        );
    }

    /// **A plain `SHIP: <prose>` line is an explanation, not an answer.** The prompt asks for a bare
    /// `SHIP`, so `SHIP: <why it does not apply>` is at least as likely to be explaining the
    /// rejected option as stating the decision. It must not outrank the real answer below it — the
    /// previous plain-beats-decorated rule did exactly that and shipped an escalation.
    #[test]
    fn a_plain_ship_explanation_does_not_outrank_the_answer_below_it() {
        // The real answer is decorated (bold), the explanatory SHIP line is plain. Neither outranks
        // the other on that axis alone, so this is a weak tie and the safe answer is the ambiguity
        // error, never `Ship`.
        let reply = "Analysis: two nits plus an unreviewed schema migration.\n\
                     SHIP: not appropriate here — the migration has never been looked at by a DBA.\n\
                     **ESCALATE: the schema migration in 003 is unreviewed**";
        assert!(
            !matches!(parse_verdict(reply), Ok(Verdict::Ship)),
            "a plain `SHIP:` explanation must never resolve to ship: {:?}",
            parse_verdict(reply)
        );

        // With an explicit label the same reply is unambiguous — and it is the escalation, with the
        // model's own reason rather than any placeholder.
        assert_eq!(
            parse_verdict(
                "SHIP: the nits do not block\nDecision: ESCALATE: the schema change is unreviewed"
            ),
            Ok(Verdict::Escalate {
                reason: "the schema change is unreviewed".to_string()
            })
        );
        assert_eq!(
            parse_verdict(
                "ESCALATE: would mean a human decides.\nSHIP: would mean the nits do not block.\n\
                 **Decision: ESCALATE**"
            ),
            Ok(Verdict::Escalate {
                reason: "the manager escalated without stating a reason".to_string()
            })
        );
    }

    #[test]
    fn escalate_parses_and_carries_its_reason() {
        assert_eq!(
            parse_verdict("ESCALATE: the migration needs a DBA"),
            Ok(Verdict::Escalate {
                reason: "the migration needs a DBA".to_string()
            })
        );
        // A bare ESCALATE still escalates — the decision is the word, the reason is a detail.
        assert_eq!(
            parse_verdict("ESCALATE"),
            Ok(Verdict::Escalate {
                reason: "the manager escalated without stating a reason".to_string()
            })
        );
    }

    #[test]
    fn an_answer_naming_neither_decision_is_an_error() {
        for reply in ["", "maybe?", "I think we should keep going", "APPROVE"] {
            assert!(parse_verdict(reply).is_err(), "({reply:?})");
        }
    }

    /// The prompt carries everything the escalation must name, and both decisions it may pick.
    #[test]
    fn the_prompt_names_the_rounds_head_and_every_finding() {
        let req = AdjudicationRequest {
            pr: "makewhatis/rhapsody#192".to_string(),
            head: "be260a6".to_string(),
            rounds: 3,
            findings: vec!["alice asked for changes at a324d2d".to_string()],
            command: "claude".to_string(),
            billing_guard: true,
            tracker_api_key: String::new(),
            model: String::new(),
            timeout: Duration::from_secs(60),
        };
        let p = adjudication_prompt(&req);
        assert!(p.contains("makewhatis/rhapsody#192"));
        assert!(p.contains("be260a6"));
        assert!(p.contains("3"));
        assert!(p.contains("alice asked for changes at a324d2d"));
        assert!(p.contains("SHIP"));
        assert!(p.contains("ESCALATE"));
    }

    // ── the recorded decision names the findings and the rounds ──────────────────────────────────

    #[test]
    fn a_ship_body_says_it_does_not_merge_and_lists_the_findings() {
        let body = decision_body(&plan(), &Verdict::Ship);
        assert!(body.contains("ship it"));
        assert!(body.contains("does not merge"));
        assert!(body.contains("alice asked for changes at a324d2d"));
    }

    #[test]
    fn an_escalate_body_names_the_findings_rounds_head_and_reason() {
        let body = decision_body(
            &plan(),
            &Verdict::Escalate {
                reason: "the migration needs a DBA".to_string(),
            },
        );
        assert!(body.contains("escalate"));
        assert!(body.contains("3 review rounds"));
        assert!(body.contains("be260a6b4366fac70fbc0e2dbabd9d51fe9d44e5"));
        assert!(body.contains("alice asked for changes at a324d2d"));
        assert!(body.contains("bob asked for changes at c366a61"));
        assert!(body.contains("the migration needs a DBA"));
    }

    // ── STUDIO-971: a decision governs only the head it was made at ──────────────────────────────

    /// **Acceptance.** A `ship` decision applies to the head the loop stopped at and to a no-op head
    /// move that PROVES that same head (STUDIO-960's `unchanged_from`), and to nothing else. An
    /// escalation governs however far the head moves — it named a human, and a push the author made
    /// themselves must not cancel it.
    ///
    /// **The partial-proof case is the load-bearing one.** `unchanged_from` proves individual rows'
    /// histories; a list that does not contain the adjudicated head is not proof the decision's head
    /// is what the branch carries. Treating any non-empty list as PR-wide proof suppresses an
    /// unmatched row's owed review, so `["other"]` must NOT govern a `Ship` at `"aaa"`.
    ///
    /// MUTATION: make `governs` return `true` unconditionally (gate on `settled()` alone) and the
    /// `governs("bbb", &[])` case below reds; make it ignore `unchanged_from` and the
    /// `unchanged_from == ["aaa"]` case reds; accept any non-empty `unchanged_from` and the
    /// `unchanged_from == ["other"]` case reds.
    #[test]
    fn a_ship_decision_governs_only_the_head_it_was_made_at() {
        let ship = Adjudication::Ship {
            head: "aaa".to_string(),
            rounds: 3,
        };
        assert!(ship.governs("aaa", &[]), "the head it was made at");
        assert!(
            !ship.governs("bbb", &[]),
            "a content-changing push is a head the manager never adjudicated"
        );
        assert!(
            ship.governs("bbb", &["aaa".to_string()]),
            "a no-op head move carries the decision forward (STUDIO-960)"
        );
        assert!(
            !ship.governs("bbb", &["other".to_string()]),
            "a proof for a DIFFERENT head's history does not carry the decision — one matching row \
             must not suppress an unmatched row's owed round"
        );
        assert!(
            !ship.governs("bbb", &["other".to_string(), "another".to_string()]),
            "nor does a list of proofs that still does not cover the adjudicated head"
        );

        let escalated = Adjudication::Escalate {
            head: "aaa".to_string(),
            rounds: 3,
            findings: Vec::new(),
            reason: "a human is needed".to_string(),
        };
        assert!(
            escalated.governs("bbb", &[]),
            "an escalation names a human and is never resumed by a push"
        );

        assert!(
            Adjudication::InFlight { rounds: 3 }.governs("bbb", &[]),
            "a turn is out right now"
        );
    }

    // ── the ledger records, reads back, and refuses to overwrite a landed verdict ────────────────

    #[test]
    fn the_ledger_records_and_reads_back_a_verdict() {
        let l = AdjudicationLedger::default();
        let pr = plan().pr;
        assert_eq!(l.peek(&pr), None);
        l.mark_in_flight(&pr, 3);
        assert_eq!(l.peek(&pr), Some(Adjudication::InFlight { rounds: 3 }));
        l.record(
            &pr,
            Adjudication::Escalate {
                head: "abc".to_string(),
                rounds: 3,
                findings: vec!["x".to_string()],
                reason: "needs a human".to_string(),
            },
        );
        assert_eq!(l.peek(&pr).map(|a| a.settled()), Some(true));
        l.mark_in_flight(&pr, 99);
        assert_eq!(
            l.peek(&pr).map(|a| a.rounds()),
            Some(3),
            "an in-flight marker must not overwrite a landed decision"
        );
    }

    /// A failed turn CLEARS the marker so the next sweep re-asks.
    #[test]
    fn clearing_forgets_the_entry() {
        let l = AdjudicationLedger::default();
        let pr = plan().pr;
        l.mark_in_flight(&pr, 3);
        l.clear(&pr);
        assert_eq!(l.peek(&pr), None);
    }

    /// Failures are counted so the control task can bound the re-asking, and a landed decision or an
    /// operator Clear resets the tally.
    #[test]
    fn failures_are_counted_and_reset_by_a_decision_or_a_clear() {
        let l = AdjudicationLedger::default();
        let pr = plan().pr;
        assert_eq!(l.failures(&pr), 0);
        assert_eq!(l.note_failure(&pr), 1, "failures count up");
        assert_eq!(l.note_failure(&pr), 2);
        assert_eq!(l.peek(&pr), None, "a failure leaves no in-flight marker");

        l.record(
            &pr,
            Adjudication::Ship {
                head: "abc".to_string(),
                rounds: 3,
            },
        );
        assert_eq!(l.failures(&pr), 0, "a landed decision resets the tally");

        l.note_failure(&pr);
        l.clear(&pr);
        assert_eq!(l.failures(&pr), 0, "an operator Clear resets the tally too");
    }

    // ── the off-loop decision is recorded in the room AND on the pull request ─────────────────────

    use std::sync::Mutex;

    use async_trait::async_trait;
    use rhapsody_config::room::{CaughtUp, Cursor, RoomError};

    struct RecordingRoom(Mutex<Vec<Message>>);

    impl RoomLog for RecordingRoom {
        fn append(&self, msg: &Message) -> Result<String, RoomError> {
            self.0.lock().unwrap().push(msg.clone());
            Ok("2026-09-20:1".to_string())
        }
        fn read_since(&self, _: &str, _: &Cursor, _: usize) -> Result<CaughtUp, RoomError> {
            Err(RoomError::Invalid("unused in this test".to_string()))
        }
        fn read_forward(&self, _: &str, _: &Cursor, _: usize) -> Result<CaughtUp, RoomError> {
            Err(RoomError::Invalid("unused in this test".to_string()))
        }
    }

    #[derive(Default)]
    struct RecordingComments(Mutex<Vec<(String, String, i64, String)>>);

    #[async_trait]
    impl PrCommentSink for RecordingComments {
        async fn post_pr_comment(
            &self,
            owner: &str,
            repo: &str,
            number: i64,
            body: &str,
        ) -> crate::ghsummons::PrCommentResult {
            self.0.lock().unwrap().push((
                owner.to_string(),
                repo.to_string(),
                number,
                body.to_string(),
            ));
            Ok(())
        }
    }

    /// A comment sink that blocks once a post begins, so a test can read the ledger from the middle
    /// of the audit writes — the shape that catches a settled entry recorded AFTER the POST.
    #[derive(Default)]
    struct BlockingComments {
        posted: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }

    #[async_trait]
    impl PrCommentSink for BlockingComments {
        async fn post_pr_comment(
            &self,
            _owner: &str,
            _repo: &str,
            _number: i64,
            _body: &str,
        ) -> crate::ghsummons::PrCommentResult {
            self.posted.notify_one();
            self.release.notified().await;
            Ok(())
        }
    }

    struct FixedVerdict(Verdict);

    #[async_trait]
    impl ReviewAdjudicator for FixedVerdict {
        async fn adjudicate(&self, _: &AdjudicationRequest) -> Result<Verdict, String> {
            Ok(self.0.clone())
        }
    }

    struct BrokenVerdict;

    #[async_trait]
    impl ReviewAdjudicator for BrokenVerdict {
        async fn adjudicate(&self, _: &AdjudicationRequest) -> Result<Verdict, String> {
            Err("model is down".to_string())
        }
    }

    fn deps<C: PrCommentSink + 'static>(
        adjudicator: Arc<dyn ReviewAdjudicator>,
        room: Arc<RecordingRoom>,
        comments: Arc<C>,
        ledger: Arc<AdjudicationLedger>,
    ) -> AdjudicationDeps {
        AdjudicationDeps {
            adjudicator,
            room: Some(room as Arc<dyn RoomLog>),
            comments: Some(comments as Arc<dyn PrCommentSink>),
            ledger,
            turn: AdjudicationTurn {
                command: "claude".to_string(),
                billing_guard: true,
                tracker_api_key: String::new(),
                model: String::new(),
                timeout: Duration::from_secs(60),
            },
        }
    }

    /// **Acceptance: the decision is recorded in the room AND on the pull request**, both naming the
    /// way it went, the findings, the rounds and the head.
    #[tokio::test]
    async fn an_escalation_is_recorded_in_the_room_and_on_the_pull_request() {
        let room = Arc::new(RecordingRoom(Mutex::new(Vec::new())));
        let comments = Arc::new(RecordingComments::default());
        let ledger = Arc::new(AdjudicationLedger::default());
        let plan = plan();
        let deps = deps(
            Arc::new(FixedVerdict(Verdict::Escalate {
                reason: "the migration needs a DBA".to_string(),
            })),
            Arc::clone(&room),
            Arc::clone(&comments),
            Arc::clone(&ledger),
        );

        perform_adjudication(&plan, &deps, Utc::now()).await;

        let posted = room.0.lock().unwrap().clone();
        assert_eq!(posted.len(), 1, "one room post");
        assert_eq!(posted[0].from, MANAGER_IDENTITY);
        assert!(posted[0].body.contains("escalate"));
        assert!(
            posted[0]
                .body
                .contains("alice asked for changes at a324d2d")
        );
        assert!(
            posted[0]
                .refs
                .contains(&"makewhatis/rhapsody#192".to_string())
        );

        let on_pr = comments.0.lock().unwrap().clone();
        assert_eq!(on_pr.len(), 1, "one pull-request comment");
        assert_eq!(
            (on_pr[0].0.as_str(), on_pr[0].1.as_str(), on_pr[0].2),
            ("makewhatis", "rhapsody", 192)
        );
        assert!(on_pr[0].3.contains("escalate"));
        assert!(on_pr[0].3.contains("the migration needs a DBA"));

        assert_eq!(
            ledger.peek(&plan.pr),
            Some(Adjudication::Escalate {
                head: plan.head.clone(),
                rounds: 3,
                findings: plan.findings.clone(),
                reason: "the migration needs a DBA".to_string(),
            })
        );
    }

    #[tokio::test]
    async fn a_ship_decision_is_recorded_both_ways() {
        let room = Arc::new(RecordingRoom(Mutex::new(Vec::new())));
        let comments = Arc::new(RecordingComments::default());
        let ledger = Arc::new(AdjudicationLedger::default());
        let plan = plan();
        let deps = deps(
            Arc::new(FixedVerdict(Verdict::Ship)),
            Arc::clone(&room),
            Arc::clone(&comments),
            Arc::clone(&ledger),
        );

        perform_adjudication(&plan, &deps, Utc::now()).await;

        assert!(room.0.lock().unwrap()[0].body.contains("ship it"));
        assert!(comments.0.lock().unwrap()[0].3.contains("ship it"));
        assert_eq!(
            ledger.peek(&plan.pr),
            Some(Adjudication::Ship {
                head: plan.head.clone(),
                rounds: 3,
            })
        );
    }

    /// **Acceptance (STUDIO-977, C).** `ship` is NOT available when the change is unread — the model
    /// may answer SHIP, but the daemon records an ESCALATE, so a stopped loop over a head nobody has
    /// read can never masquerade as a decision. The escalation goes through the same two audit writes
    /// as any other, and names the gate that actually failed.
    ///
    /// MUTATION: drop the `ship_available` upgrade and this reds — the ledger holds a `Ship`.
    #[tokio::test]
    async fn a_ship_over_an_unread_change_escalates_instead() {
        let room = Arc::new(RecordingRoom(Mutex::new(Vec::new())));
        let comments = Arc::new(RecordingComments::default());
        let ledger = Arc::new(AdjudicationLedger::default());
        let plan = ReviewAdjudicationPlan {
            ship_available: false,
            ship_unavailable_reason: "the manager shipped, but not every required reviewer has \
                                      read the change at `be260a6` (a review round is still owed \
                                      at this head); a human must decide"
                .to_string(),
            ..plan()
        };
        let deps = deps(
            Arc::new(FixedVerdict(Verdict::Ship)),
            Arc::clone(&room),
            Arc::clone(&comments),
            Arc::clone(&ledger),
        );

        perform_adjudication(&plan, &deps, Utc::now()).await;

        assert!(
            room.0.lock().unwrap()[0].body.contains("escalate"),
            "the room post says it went that way"
        );
        assert!(comments.0.lock().unwrap()[0].3.contains("escalate"));
        match ledger.peek(&plan.pr) {
            Some(Adjudication::Escalate { head, reason, .. }) => {
                assert_eq!(head, plan.head);
                assert!(
                    reason.contains("not every required reviewer has read")
                        && reason.contains("a review round is still owed"),
                    "the reason names the gate that failed, not a blanket 'unread': {reason}"
                );
            }
            other => panic!("expected a settled escalation, got {other:?}"),
        }
    }

    /// **A turn that can never succeed escalates through the SAME audit path as a real decision.**
    /// The escalation the old control-task-only path recorded reached neither the room nor the pull
    /// request — the one escalation an operator most needs pushed at them, since its cause is "your
    /// manager turn is broken". This pins both writes and the settled ledger entry.
    #[tokio::test]
    async fn a_turn_that_failed_its_attempts_escalates_and_is_recorded_both_ways() {
        let room = Arc::new(RecordingRoom(Mutex::new(Vec::new())));
        let comments = Arc::new(RecordingComments::default());
        let ledger = Arc::new(AdjudicationLedger::default());
        let plan = plan();
        let deps = deps(
            Arc::new(BrokenVerdict),
            Arc::clone(&room),
            Arc::clone(&comments),
            Arc::clone(&ledger),
        );

        for _ in 0..MAX_ADJUDICATION_ATTEMPTS {
            perform_adjudication(&plan, &deps, Utc::now()).await;
        }

        assert_eq!(
            room.0.lock().unwrap().len(),
            1,
            "exactly one escalation post for the bounded failure"
        );
        assert!(
            room.0.lock().unwrap()[0].body.contains("escalate"),
            "the room post says it went that way"
        );
        let on_pr = comments.0.lock().unwrap().clone();
        assert_eq!(on_pr.len(), 1, "and one pull-request comment");
        assert!(on_pr[0].3.contains("escalate"));
        match ledger.peek(&plan.pr) {
            Some(Adjudication::Escalate {
                head,
                rounds,
                reason,
                findings,
            }) => {
                assert_eq!(head, plan.head);
                assert_eq!(rounds, 3);
                assert!(reason.contains("failed 3 times"), "{reason}");
                assert_eq!(findings, plan.findings);
            }
            other => panic!("expected a settled escalation, got {other:?}"),
        }
        assert_eq!(
            ledger.failures(&plan.pr),
            0,
            "a landed escalation resets the failure tally"
        );
    }

    /// **The failure-path window, pinned from the middle of the POST.** The bounded-failure
    /// escalation clears the in-flight marker (`note_failure`) before its audit writes, so if the
    /// settled `Escalate` were recorded only AFTER `post_pr_comment` returned, the control task
    /// would read "no decision, not in flight" for the whole of that unbounded await and hand out a
    /// FOURTH turn — one that erases the escalation or overturns it with a contradicting `SHIP`.
    /// The ledger must already be settled while the comment POST is outstanding. A comment sink that
    /// blocks (the shape above) is the only way to observe it; an end-state assertion passes either
    /// ordering.
    #[tokio::test]
    async fn the_failure_escalation_is_settled_before_the_comment_post_returns() {
        let room = Arc::new(RecordingRoom(Mutex::new(Vec::new())));
        let comments = Arc::new(BlockingComments::default());
        let ledger = Arc::new(AdjudicationLedger::default());
        let plan = plan();
        let deps = deps(
            Arc::new(BrokenVerdict),
            Arc::clone(&room),
            Arc::clone(&comments),
            Arc::clone(&ledger),
        );

        // The first two failures re-ask and never reach the audit writes.
        for _ in 0..(MAX_ADJUDICATION_ATTEMPTS - 1) {
            perform_adjudication(&plan, &deps, Utc::now()).await;
        }
        // The Nth, on its own task, so the test can read the ledger while the POST is blocked.
        let spawned_plan = plan.clone();
        let handle =
            tokio::spawn(
                async move { perform_adjudication(&spawned_plan, &deps, Utc::now()).await },
            );
        comments.posted.notified().await;
        match ledger.peek(&plan.pr) {
            Some(Adjudication::Escalate { reason, .. }) => {
                assert!(reason.contains("failed 3 times"), "{reason}");
            }
            other => {
                panic!("the ledger must be settled before the comment POST returns, got {other:?}")
            }
        }
        comments.release.notify_one();
        handle.await.expect("the adjudication task must not panic");
    }

    /// A failed turn is NOT a decision: nothing is recorded, the marker is cleared so the next
    /// sweep re-asks, and the loop stays stopped in the meantime.
    #[tokio::test]
    async fn a_failed_turn_records_nothing_and_re_asks() {
        let room = Arc::new(RecordingRoom(Mutex::new(Vec::new())));
        let comments = Arc::new(RecordingComments::default());
        let ledger = Arc::new(AdjudicationLedger::default());
        let plan = plan();
        ledger.mark_in_flight(&plan.pr, 3);
        let deps = deps(
            Arc::new(BrokenVerdict),
            Arc::clone(&room),
            Arc::clone(&comments),
            Arc::clone(&ledger),
        );

        perform_adjudication(&plan, &deps, Utc::now()).await;

        assert!(room.0.lock().unwrap().is_empty());
        assert!(comments.0.lock().unwrap().is_empty());
        assert_eq!(
            ledger.peek(&plan.pr),
            None,
            "cleared so the next sweep re-asks"
        );
        assert_eq!(
            ledger.failures(&plan.pr),
            1,
            "and the attempt is counted so the re-asking is bounded"
        );
    }
}
