//! dispatch — parity port of Go `internal/orchestrator/dispatch.go`.
//!
//! The dispatch ORDERING (`sort_for_dispatch`), the intrinsic eligibility predicate (`eligible` /
//! `eligibility`, incl. the blockedBy dependency-mode gate and the required-label gate), the
//! blocker-clearing rule (`blocker_cleared`, INF-318), and the FRESH-pickup suppression guards
//! (`pr_suppressed` / `review_reopen_eligible`, INF-448) the selection pass (`select.rs`) and the
//! retry path (O5) schedule against (upstream §8.2). Slot accounting lives in [`crate::concurrency`].
//!
//! Deviations from the Go source, all behavior-preserving:
//!   * Go's `Eligible(iss, running, claimed, active, terminal, requiredLabels, mode, review,
//!     canceled)` (9 positional args) groups its six state-config sets into an [`EligibilityGate`]
//!     borrow, so the predicate takes `(iss, running, claimed, gate)` — idiomatic arity without a
//!     `clippy::too_many_arguments` allow. The gate fields map one-to-one onto the effective config
//!     (single-project path) or a resolved project (multi path), exactly as the Go call sites pass.
//!   * `running`/`claimed`/state sets are Go `map[string]bool` SETs → Rust [`HashSet`].
//!   * Go's zero-`time.Time` sentinel from `lastRunStartedAt` becomes `Option<DateTime<Utc>>`.
//!   * The otherwise-silent blocked-skip diagnostic (INF-249) logs via `tracing` (as the sibling
//!     crates do) instead of a threaded `slog` logger; the fields (`issue_identifier`, `blocker`,
//!     `blocker_state`) and per-blocker cadence are preserved.

use std::cmp::Ordering;
use std::collections::HashSet;
use std::sync::{Mutex, PoisonError};

use chrono::{DateTime, Utc};
use rhapsody_config::{DEPENDENCY_MODE_DAG, DEPENDENCY_MODE_GRAPHITE};
use rhapsody_core::{BlockerRef, Issue, normalize_state};
use rhapsody_store::OUTCOME_INTERRUPTED;

use crate::orchestrator::Orchestrator;

/// Orders issues by priority asc (nil/`None` last), then created_at oldest first (nil/`None` last),
/// then identifier lexicographically (upstream §8.2). Stable, mirroring Go `SortForDispatch`
/// (`sort.SliceStable`).
pub fn sort_for_dispatch(issues: &mut [Issue]) {
    issues.sort_by(dispatch_cmp);
}

/// The shared global dispatch ordering (priority asc, created_at oldest, identifier lexicographic)
/// used by [`sort_for_dispatch`] and `select`'s `sort_tagged_stable`. Mirrors Go `dispatchLess`,
/// expressed as an [`Ordering`]-returning comparator (Rust's stable `sort_by` takes a comparator,
/// not a `less` predicate) — the induced total order is identical.
pub(crate) fn dispatch_cmp(a: &Issue, b: &Issue) -> Ordering {
    cmp_priority(a.priority, b.priority)
        .then_with(|| cmp_created_time(a.created_at, b.created_at))
        .then_with(|| a.identifier.cmp(&b.identifier))
}

/// Priority ordering with `None` (Go nil `*int`) sorting LAST. Mirrors Go `cmpPriority`.
fn cmp_priority(a: Option<i64>, b: Option<i64>) -> Ordering {
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(x), Some(y)) => x.cmp(&y),
    }
}

/// Created-time ordering with `None` (Go nil `*time.Time`) sorting LAST. Mirrors Go `cmpCreatedTime`.
fn cmp_created_time(a: Option<DateTime<Utc>>, b: Option<DateTime<Utc>>) -> Ordering {
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(x), Some(y)) => x.cmp(&y),
    }
}

/// The six state-config sets the eligibility predicate consults, grouped from Go's positional
/// `Eligible`/`eligibility` params (see the module docs). Every field is a borrow into the effective
/// config (single-project path) or a resolved project (multi path). `mode` is the dependency-mode
/// string (`""`/`"disabled"`/`"graphite"`/`"dag"`); `required_labels` empty ⇒ no label filter.
///
/// `pub` because it appears in the signature of the `pub` [`eligible`] predicate (which the retry
/// path, O5, will also call).
pub struct EligibilityGate<'a> {
    pub active: &'a HashSet<String>,
    pub terminal: &'a HashSet<String>,
    pub required_labels: &'a HashSet<String>,
    pub mode: &'a str,
    pub review: &'a HashSet<String>,
    pub canceled: &'a HashSet<String>,
}

/// One ticket the dispatcher is holding because it wears
/// [`HUMAN_LABEL`](crate::teams::HUMAN_LABEL) (STUDIO-949). The console board's own shape: the
/// ticket key, a title for the card, and the project slug it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldForHuman {
    pub issue_identifier: String,
    pub title: String,
    pub project: String,
}

/// How many distinct tickets [`HumanHoldLedger`] remembers having announced before it forgets the
/// oldest set wholesale. Comfortably above any plausible set of human-gated tickets; the cost of
/// hitting it is one repeated log line per ticket.
const HUMAN_HOLD_CAPACITY: usize = 256;

/// The once-per-ticket memory behind the human-hold report (STUDIO-949).
///
/// A `rhapsody:human` ticket in a dispatchable state is refused on EVERY selection pass — twice a
/// minute at the default poll interval — and a refusal repeated forever is indistinguishable from a
/// daemon that is stuck. So the LOG fires when the hold is NEWS: the first time this process sees
/// the ticket, and never again. That is [`crate::runautomerge::AutoMergeLedger`]'s idiom, and the
/// reason for it is the same.
///
/// It also carries the CURRENT hold set for the console's `/api/v1/state` key, and beside it the
/// CURRENT-LABEL set (`labelled`): every candidate the pass saw wearing `rhapsody:human`, live work
/// included. The two are kept apart because they answer different questions — "is this a deliberate
/// hold to show an operator" (no live work) and "does this ticket wear the label right now" (yes,
/// live work too) — and only the second is the dispatch refusal's signal on a RUNNING ticket. The
/// jobs live in one ledger because all are per-selection-pass facts:
/// [`begin_pass`](Self::begin_pass) clears both current sets (a ticket no longer held stops being
/// reported, a delisted label stops refusing), while the announced set survives so re-holding the
/// next tick is not news again.
///
/// Unlike [`Orchestrator::held_for_capacity`](crate::orchestrator::Orchestrator), the current set is
/// deliberately NOT retired on the tick's three early returns (a failed preflight, an armed drain, a
/// dead credential). A leftover capacity tally would be a stale claim about a pass that no longer
/// ran; a deliberate human hold does not depend on dispatch being enabled at all — the ticket still
/// needs a person while the daemon is gated — so keeping it is the honest answer.
///
/// Shared (`Arc`) rather than loop-confined because the selection pass takes `&self` by design and
/// the control task assembles the snapshot from the same cell. A `Mutex` held for two map operations
/// and never across an `.await`; see `crates/orchestrator/CLAUDE.md`'s seam list.
pub struct HumanHoldLedger {
    inner: Mutex<HumanHoldState>,
}

#[derive(Default)]
struct HumanHoldState {
    /// Ticket identifiers already announced this process lifetime (the dedupe).
    announced: HashSet<String>,
    /// Tickets held by the MOST RECENT selection pass, for the console.
    held: Vec<HeldForHuman>,
    /// Every ticket the most recent selection pass OBSERVED wearing
    /// [`HUMAN_LABEL`](crate::teams::HUMAN_LABEL), whether or not it already has a live run —
    /// lowercased, for case-insensitive comparison.
    ///
    /// This is the CURRENT-LABEL signal, deliberately separate from [`Self::held`]. The reporting
    /// rule excludes a ticket the daemon is running right now, because a live run is not yet a
    /// deliberate hold (see [`Self::held`]); but the refusal itself is absolute, and the handoff's
    /// review decision (STUDIO-949 round 8) must see a label added mid-run even though the run's own
    /// issue snapshot predates it. Kept separate so tightening a decision gate never makes the
    /// console call live work "held".
    labelled: HashSet<String>,
}

impl Default for HumanHoldLedger {
    fn default() -> Self {
        Self {
            inner: Mutex::new(HumanHoldState::default()),
        }
    }
}

impl HumanHoldLedger {
    /// Starts a fresh selection pass: the CURRENT hold set is dropped, so a ticket that stopped
    /// wearing the label — or left the candidate set — stops being reported. The announced set is
    /// deliberately NOT touched.
    pub(crate) fn begin_pass(&self) {
        let mut st = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        st.held.clear();
        st.labelled.clear();
    }

    /// Records a ticket the pass OBSERVED wearing [`HUMAN_LABEL`](crate::teams::HUMAN_LABEL), with
    /// no log and no console row — the CURRENT-LABEL half, independent of the reporting rule that
    /// excludes live work (STUDIO-949 round 8). Called for every candidate that wears the label,
    /// including one the daemon is running, so a decision made on the RUNNING ticket's handoff can
    /// see a label added after it was dispatched.
    pub(crate) fn note_human_label(&self, issue_identifier: &str) {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .labelled
            .insert(issue_identifier.to_ascii_lowercase());
    }

    /// Records a hold and returns whether it is NEWS (the first time this ticket has been announced
    /// this process lifetime). Only a news hold is logged.
    ///
    /// The CURRENT set is unique by identifier (STUDIO-949 round 5). It is cleared by
    /// [`begin_pass`](Self::begin_pass), but that only runs when a selection pass runs: on the
    /// legacy/top-level tracker path a candidate-fetch ERROR returns before either ladder calls it,
    /// while `promote_unblocked` still runs and re-notes the same Backlog dependent — so an
    /// unconditional `push` appended one identical `/api/v1/state.held_for_human` row per outage
    /// tick, and the Now strip's `+held_for_human` grew with it while the board still had one card.
    /// A second note for a ticket already held replaces the row (the later note carries the same
    /// facts; the project slug differs only between the ladders and the Backlog pass).
    pub(crate) fn hold(&self, entry: HeldForHuman) -> bool {
        let mut st = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        // A reported hold is by definition an observed current label, so the decision half is fed
        // here too (STUDIO-949 round 8).
        st.labelled
            .insert(entry.issue_identifier.to_ascii_lowercase());
        if st.announced.len() >= HUMAN_HOLD_CAPACITY
            && !st.announced.contains(&entry.issue_identifier)
        {
            st.announced.clear();
        }
        let news = st.announced.insert(entry.issue_identifier.clone());
        match st
            .held
            .iter_mut()
            .find(|h| h.issue_identifier == entry.issue_identifier)
        {
            Some(existing) => *existing = entry,
            None => st.held.push(entry),
        }
        news
    }

    /// The tickets the most recent selection pass OBSERVED wearing the human label, including any
    /// the daemon is running right now — lowercased. This is the decision signal for a gate on a
    /// RUNNING ticket (the handoff review decision, the ticketless origin gate); the console reads
    /// [`Self::held`] instead, which excludes live work.
    pub(crate) fn labelled(&self) -> HashSet<String> {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .labelled
            .clone()
    }

    /// The tickets held by the most recent selection pass, for the snapshot. Empty until a pass has
    /// run, which is what keeps a daemon with no human-gated ticket serving the Go-identical payload.
    pub(crate) fn held(&self) -> Vec<HeldForHuman> {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .held
            .clone()
    }
}

/// The STATE half of dispatch eligibility: is `st` (already normalized) a state the daemon
/// dispatches work FROM at all?
///
/// It is the exact test [`eligibility`] has always run — an ACTIVE state that is not also terminal
/// — lifted out of it verbatim so there can be **one** source of truth for "dispatchable"
/// (STUDIO-672). Lifting it changes nothing about eligibility; what it buys is a second caller.
/// [`DispatchStates::admits`] wraps it for the off-loop Teams triage task, which must consider
/// exactly the tickets the selection gate would hold and nothing else. Before this, triage filtered
/// on labels alone and happily assigned identities to **review-state** tickets the gate could never
/// hold — parked design records and work already under human review — which surprised the operator
/// in Linear, inflated the least-loaded assigner's load counts, and pre-decided the owner of a
/// reopen nobody had asked for.
///
/// Pure and allocation-free, so sharing it costs the dispatch path nothing.
pub(crate) fn dispatchable_state(
    st: &str,
    active: &HashSet<String>,
    terminal: &HashSet<String>,
) -> bool {
    active.contains(st) && !terminal.contains(st)
}

/// The resolved state sets an off-loop reader needs to answer [`dispatchable_state`] for itself
/// (STUDIO-672).
///
/// [`EligibilityGate`] cannot serve that reader: it is a borrow into the live `Effective`, which
/// belongs to the control task and is swapped on every reload. This is the same two sets, OWNED, so
/// the triage task can be handed a snapshot each cycle through the `reads` seam exactly as it is
/// already handed that reload's project trackers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DispatchStates {
    pub active: HashSet<String>,
    pub terminal: HashSet<String>,
    /// The configured REVIEW states. Not part of [`dispatchable_state`] — the selection gate
    /// handles review-state issues on its own reopen branch — but the reconcile sweep
    /// (STUDIO-672) needs them to scope itself to exactly "parked in review" rather than to every
    /// state that merely fails to be dispatchable, which would sweep Done, Canceled and Backlog
    /// tickets too.
    pub review: HashSet<String>,
    /// The configured CANCELED states — a SUBSET of [`Self::terminal`], carried separately for the
    /// same reason [`Self::review`] is: a reader that has to name a ticket's lifecycle for a human
    /// (STUDIO-702) must tell "Done" from "Won't Do", and `terminal` alone folds the two together.
    /// Not part of [`dispatchable_state`] — `terminal` already excludes every state in here.
    pub canceled: HashSet<String>,
}

impl DispatchStates {
    /// Whether `iss` is in a state the daemon would dispatch from — [`dispatchable_state`] over an
    /// issue, normalizing the state the way every other reader of `Issue::state` does.
    pub fn admits(&self, iss: &Issue) -> bool {
        dispatchable_state(&normalize_state(&iss.state), &self.active, &self.terminal)
    }

    /// Whether `iss` is parked in a configured REVIEW state — non-dispatchable, but alive: work
    /// awaiting a human, or a record deliberately left open. The complement of [`Self::admits`]
    /// within the candidate fetch, which is exactly active ∪ review.
    pub fn is_in_review(&self, iss: &Issue) -> bool {
        let st = normalize_state(&iss.state);
        self.review.contains(&st) && !self.active.contains(&st)
    }

    /// Whether any active state is configured at all. `false` means this snapshot admits NOTHING,
    /// which is a legitimate pre-first-reload state and an illegitimate post-reload one — the
    /// triage task logs the difference rather than sweeping silently past it (the STUDIO-671 class
    /// of wedge: a filter that quietly matches nothing looks exactly like a quiet daemon).
    pub fn is_empty(&self) -> bool {
        self.active.is_empty()
    }
}

/// The verdict of the dispatch-eligibility predicate plus, when the issue was rejected SOLELY
/// because of non-terminal blockers, the offending blockers in declaration order. `blocked_by` is
/// non-empty only when `ok` is false AND non-terminal blockers were the operative reason — so the
/// dispatch loop can surface exactly that (otherwise silent) drop and nothing else. Mirrors Go
/// `eligibilityResult`.
///
/// `held_for_human` is the Rhapsody-only third outcome (STUDIO-949): the issue wears
/// [`HUMAN_LABEL`](crate::teams::HUMAN_LABEL) and is refused because only a person can do it. It is
/// a deliberate hold, not ordinary ineligibility, so it is a named field rather than the all-default
/// [`EligibilityResult::default`] — a caller MUST be able to tell "held for a human" from "not a
/// candidate", or the ticket sits in Todo dispatching nothing and saying nothing, which is the new
/// silent-stall class this field exists to prevent.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct EligibilityResult {
    pub ok: bool,
    pub blocked_by: Vec<BlockerRef>,
    /// True when the refusal was [`HUMAN_LABEL`](crate::teams::HUMAN_LABEL). Never true together
    /// with a non-empty `blocked_by`: the human gate short-circuits above the blocker rule.
    pub held_for_human: bool,
}

/// Reports whether an issue is intrinsically dispatch-eligible (upstream §8.2). Slot availability is
/// checked separately by the caller. It is a pure predicate (no logging) used from multiple call
/// sites (the poll selection + retry.go, O5). When the dispatch loop needs to explain WHY a
/// candidate was skipped it calls [`eligibility`] directly for the structured reason (see
/// [`Orchestrator::log_blocked_skip`], INF-249). Mirrors Go `Eligible`.
///
/// The `mode`/`review`/`canceled` fields on the gate add the DAG dependency-mode dimension
/// (INF-318): in disabled mode (`""`/`"disabled"`) the blocker check collapses to the pre-feature
/// terminal-only rule, so every call with `mode == ""` is byte-identical; graphite/dag widen WHEN a
/// blocker clears.
pub fn eligible(
    iss: &Issue,
    running: &HashSet<String>,
    claimed: &HashSet<String>,
    gate: &EligibilityGate<'_>,
) -> bool {
    eligibility(iss, running, claimed, gate).ok
}

/// The shared core of [`eligible`]: computes the verdict and, for the blocker case, collects every
/// non-terminal blocker. The non-blocker gates are evaluated first and short-circuit with an empty
/// `blocked_by`, so a candidate dropped for some other reason (invalid fields, non-active state,
/// already running/claimed, label miss) is never mislabeled as blocked. The `ok` value is identical
/// to [`eligible`]. Mirrors Go `eligibility`.
pub(crate) fn eligibility(
    iss: &Issue,
    running: &HashSet<String>,
    claimed: &HashSet<String>,
    gate: &EligibilityGate<'_>,
) -> EligibilityResult {
    if iss.id.is_empty()
        || iss.identifier.is_empty()
        || iss.title.is_empty()
        || iss.state.is_empty()
    {
        return EligibilityResult::default();
    }
    let st = normalize_state(&iss.state);
    if !dispatchable_state(&st, gate.active, gate.terminal) {
        return EligibilityResult::default();
    }
    if running.contains(&iss.id) || claimed.contains(&iss.id) {
        return EligibilityResult::default();
    }
    // Human-only gate (STUDIO-949): a ticket wearing `rhapsody:human` cannot be done by an agent at
    // all, so the dispatcher refuses it here — the one chokepoint every dispatch path flows through,
    // Teams on or off. Deliberately BEFORE the required-label gate and the blocker rule: the hold is
    // absolute and owes no explanation from either. It sets `held_for_human` (rather than returning
    // the all-default result) so the caller can distinguish it from ordinary ineligibility and
    // report it once; `reviewreconcile.rs` records why the sweep cannot mistake a hold for a stall.
    if crate::teams::is_human(iss) {
        return EligibilityResult {
            ok: false,
            blocked_by: Vec::new(),
            held_for_human: true,
        };
    }
    // Label gate: when required labels are configured, the issue must carry AT LEAST ONE of them
    // (match-ANY, case-insensitive). A miss short-circuits with empty `blocked_by` so it is never
    // mislabeled as blocker-held. Empty set ⇒ no filter, so the verdict is byte-identical to the
    // pre-label logic.
    if !gate.required_labels.is_empty() && !has_any_label(iss, gate.required_labels) {
        return EligibilityResult::default();
    }
    if st == "todo" {
        let blocked: Vec<BlockerRef> = iss
            .blocked_by
            .iter()
            .flatten()
            .filter(|b| !blocker_cleared(b, gate.mode, gate.review, gate.terminal, gate.canceled))
            .cloned()
            .collect();
        if !blocked.is_empty() {
            return EligibilityResult {
                ok: false,
                blocked_by: blocked,
                held_for_human: false,
            };
        }
    }
    EligibilityResult {
        ok: true,
        blocked_by: Vec::new(),
        held_for_human: false,
    }
}

/// Reports whether `iss` carries at least one of the wanted labels (match-ANY). Each issue label is
/// normalized at compare time so the gate holds even if a tracker adapter fails to lowercase; `want`
/// is assumed already normalized. Mirrors Go `hasAnyLabel`.
pub(crate) fn has_any_label(iss: &Issue, want: &HashSet<String>) -> bool {
    iss.labels
        .iter()
        .flatten()
        .any(|l| want.contains(&normalize_state(l)))
}

/// Reports whether a resolved `dependency_mode` turns the DAG orchestration ON (graphite or dag).
/// disabled (`""`/`"disabled"`) is OFF — the single source of truth for "is the feature active?"
/// shared by [`blocker_cleared`] and (later) the auto-promote pass (INF-318). Mirrors Go
/// `dependencyModeEnabled`.
pub(crate) fn dependency_mode_enabled(mode: &str) -> bool {
    mode == DEPENDENCY_MODE_GRAPHITE || mode == DEPENDENCY_MODE_DAG
}

/// Reports whether a blocker no longer holds its dependent, per the dependency mode (INF-318). A
/// blocker with unknown (`None`) state is NEVER cleared (conservative). Mirrors Go `blockerCleared`:
///
///   * disabled (default; `""`/`"disabled"`): cleared ONLY when terminal — byte-identical to the
///     pre-feature terminal-only rule (the disabled-is-noop invariant).
///   * graphite: cleared when the blocker is in a review state OR terminal (In Review is enough).
///   * dag: cleared ONLY when terminal/merged.
///
/// A cancelled blocker (in `canceled`) is NEVER cleared in graphite/dag — the premise is gone, so
/// the dependent is surfaced as orphaned by the auto-promote pass, not promoted.
pub(crate) fn blocker_cleared(
    b: &BlockerRef,
    mode: &str,
    review: &HashSet<String>,
    terminal: &HashSet<String>,
    canceled: &HashSet<String>,
) -> bool {
    let state = match &b.state {
        Some(s) => s,
        None => return false,
    };
    let st = normalize_state(state);
    if !dependency_mode_enabled(mode) {
        // disabled (default): terminal-only, byte-identical to today.
        return terminal.contains(&st);
    }
    if canceled.contains(&st) {
        // cancelled never promotes (orphan) — graphite/dag only.
        return false;
    }
    if terminal.contains(&st) {
        // merged/done clears in both enabled modes.
        return true;
    }
    // In Review clears in graphite only.
    mode == DEPENDENCY_MODE_GRAPHITE && review.contains(&st)
}

/// Returns a human label for a blocker in log output, preferring the tracker identifier (e.g.
/// `"INF-243"`), then the opaque id, then `"unknown"`. Mirrors Go `blockerIdentifier`.
pub(crate) fn blocker_identifier(b: &BlockerRef) -> String {
    if let Some(id) = b.identifier.as_deref().filter(|s| !s.is_empty()) {
        return id.to_string();
    }
    if let Some(id) = b.id.as_deref().filter(|s| !s.is_empty()) {
        return id.to_string();
    }
    "unknown".to_string()
}

/// How many held candidates [`Orchestrator::log_capacity_hold`] names before it falls back to a
/// remainder count. The front of the sorted queue is what an operator needs; a board with fifty
/// candidates does not need fifty of them on one line every poll.
const HELD_SAMPLE: usize = 10;

/// Returns the blocker's state name for log output, with original casing preserved (e.g.
/// `"In Review"`). A `None`/empty state logs as `"unknown"` — the same value eligibility treats as
/// non-terminal (conservative; INF-249). Mirrors Go `blockerStateName`.
pub(crate) fn blocker_state_name(b: &BlockerRef) -> String {
    match &b.state {
        Some(s) if !s.is_empty() => s.clone(),
        _ => "unknown".to_string(),
    }
}

impl Orchestrator {
    /// Records and reports a `rhapsody:human` hold (STUDIO-949). The refusal itself is
    /// [`eligibility`]'s; this is the otherwise-silent half — one `tracing::info!` line the FIRST
    /// time the ticket is held, and the entry the console reads. A hold that repeats every tick is
    /// not a signal anyone reads, so the ledger dedupes it; `project` is the owning project slug
    /// (empty on the legacy single-tracker path), carried for the console card.
    pub(crate) fn note_human_hold(&self, iss: &Issue, project: &str) {
        let entry = HeldForHuman {
            issue_identifier: iss.identifier.clone(),
            title: iss.title.clone(),
            project: project.to_string(),
        };
        if self.human_holds.hold(entry) {
            tracing::info!(
                issue_identifier = %iss.identifier,
                "skipping dispatch: held for a human (rhapsody:human); only a person can do this ticket"
            );
        }
    }

    /// Surfaces the otherwise-silent drop of a Todo candidate held back by non-terminal blockers
    /// (INF-249): one `tracing::info!` line per non-terminal blocker, each naming the blocked issue,
    /// the blocker, and the blocker's state. `blockers` is the [`EligibilityResult::blocked_by`]
    /// slice; an empty slice (any non-blocker drop, or an eligible issue) logs nothing. Mirrors Go
    /// `logBlockedSkip` (emitted via `tracing` rather than `slog`; same fields, same per-blocker
    /// cadence).
    pub(crate) fn log_blocked_skip(&self, iss: &Issue, blockers: &[BlockerRef]) {
        for b in blockers {
            tracing::info!(
                issue_identifier = %iss.identifier,
                blocker = %blocker_identifier(b),
                blocker_state = %blocker_state_name(b),
                "skipping dispatch: blocked by non-terminal blocker"
            );
        }
    }

    /// Surfaces the otherwise-SILENT drop of the candidates a selection pass never reached because
    /// the GLOBAL concurrency cap was already full (STUDIO-885): one `tracing::info!` line naming
    /// the tickets, the cap, and how many runs are holding it. `held` is the unexamined tail of the
    /// sorted candidate list, minus the daemon's own in-flight work (see
    /// [`Orchestrator::is_unworked_candidate`]); an empty slice logs nothing.
    ///
    /// It says "not considered", not "would have dispatched", and the distinction is deliberate:
    /// the pass stops at the first of these, so none of them was assessed. A named ticket may still
    /// turn out to be blocked, unlabelled or suppressed once a slot frees. What the line reports
    /// honestly is that the cap, and not a verdict about the ticket, is why nothing happened.
    ///
    /// It exists because "the board is full" and "this ticket is correctly suppressed" were
    /// indistinguishable from outside. In the reported incident they SWAPPED with no signal at all:
    /// for four minutes the ticket was eligible and merely capped, which the pass reported by
    /// saying nothing, and thereafter it was suppressed again, which reads the same as the state it
    /// had been in all along. One of those resolves on its own and one does not, and an operator
    /// could not tell which they were looking at.
    ///
    /// The list is capped at [`HELD_SAMPLE`] names plus a remainder count: a busy board can hold
    /// dozens of candidates and the point of the line is to name the ones at the front of the
    /// queue, not to render the queue.
    pub(crate) fn log_capacity_hold(&self, held: &[String], max_concurrent: i64) {
        if held.is_empty() {
            return;
        }
        let mut sample = held
            .iter()
            .take(HELD_SAMPLE)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        if held.len() > HELD_SAMPLE {
            sample.push_str(&format!(" (+{} more)", held.len() - HELD_SAMPLE));
        }
        tracing::info!(
            not_considered = %sample,
            not_considered_count = held.len(),
            max_concurrent,
            running = self.running.len(),
            "skipping dispatch: no global concurrency slot; candidates not considered this tick"
        );
    }

    /// Whether `iss` is a candidate the selection pass would genuinely have ASSESSED, rather than
    /// the daemon's own work coming back round: the candidate fetch is a state query, so every
    /// running, claimed and recovery-owned issue is in it, and `eligibility` drops them further
    /// down the pass. Used only by [`Orchestrator::log_capacity_hold`], which stops ABOVE that
    /// point and would otherwise report a ticket that is running right now as one waiting for a
    /// slot.
    pub(crate) fn is_unworked_candidate(
        &self,
        iss: &Issue,
        running: &HashSet<String>,
        recovered: &HashSet<String>,
    ) -> bool {
        !running.contains(&iss.id)
            && !self.claimed.contains(&iss.id)
            && !recovered.contains(&iss.identifier)
    }

    /// Reports whether a FRESH dispatch of `iss` should be suppressed because a prior run already
    /// materialized work as a linked GitHub PR (open OR merged) and no newer summons has arrived. It
    /// is the dispatch-side guard against re-picking an issue whose work is already done/in-review
    /// when its Linear state briefly flaps back to active. Mirrors Go `prSuppressed`.
    ///
    /// Re-open rule (INF-448): a summons strictly newer than the ticket's LAST RUN START lifts the
    /// suppression. Comparing to run START (not PR activity) honors a summons posted while a run was
    /// in flight. Store-off fallback: with no run-start watermark the pre-INF-448 PR-activity
    /// comparison applies; a PR with no comparable activity time stays lenient so a legitimately-
    /// summoned issue is never wedged by missing metadata. Applied to FRESH pickups only.
    pub(crate) fn pr_suppressed(&self, iss: &Issue) -> bool {
        if !iss.linked_pr {
            return false;
        }
        let summon = match iss.latest_summon_at {
            Some(s) => s,
            // a linked PR but no summons → already-done work, nothing new → suppress.
            None => return true,
        };
        match self.last_run_started_at(&iss.identifier) {
            None => match iss.latest_pr_activity_at {
                // no watermark of any kind but a summons exists → be lenient.
                None => false,
                // pre-INF-448 fallback: suppress unless the summons is after the PR's last activity.
                Some(pr) => summon <= pr,
            },
            // suppress unless the summons arrived after the last run began (feedback the last round
            // could not have consumed from its start).
            Some(start) => summon <= start,
        }
    }

    /// Reports whether a review-state issue should be re-engaged this tick by a fresh summons
    /// (symphony-29). The review-branch counterpart to [`eligible`] (which intentionally rejects
    /// non-active states); a review issue is handled ONLY here. Eligible iff it is neither running
    /// nor claimed, carries a `team_id` (required to promote it), carries a summons, AND that
    /// summons is strictly newer than the START of the daemon's last run on it. No run / store
    /// disabled / unparseable start ⇒ NOT eligible (the daemon never grabs a human-managed review
    /// ticket it has never worked; the check converges). A `rhapsody:human` ticket is NEVER eligible
    /// (STUDIO-949) — this ladder bypasses `eligibility`, so the human gate must be repeated here or
    /// the label leaks dispatch through the one path that does not consult it. Mirrors Go
    /// `reviewReopenEligible`.
    pub(crate) fn review_reopen_eligible(&self, iss: &Issue, running: &HashSet<String>) -> bool {
        // Human-only gate (STUDIO-949). This ladder runs BEFORE `eligibility` — a review-state issue
        // is never active, so `eligibility` rejects it outright and the reopen path is the only one
        // that can move it back to an active state and dispatch it. Without the gate here, a
        // `rhapsody:human` ticket that had run once and then been parked in review with a newer
        // `@symphony` summons would leak straight back to an agent, contradicting the README's claim
        // that the label refuses every dispatch path. Absolute, like the gate in `eligibility`: it
        // does not consult the summons, the store, or Teams.
        if crate::teams::is_human(iss) {
            return false;
        }
        if iss.id.is_empty() || iss.identifier.is_empty() || iss.team_id.is_empty() {
            return false;
        }
        if running.contains(&iss.id) || self.claimed.contains(&iss.id) {
            return false;
        }
        let summon = match iss.latest_summon_at {
            Some(s) => s,
            None => return false,
        };
        match self.last_run_started_at(&iss.identifier) {
            None => false, // never worked it (or store off / no start time) → don't grab it.
            Some(last) => summon > last,
        }
    }

    /// Returns the `started_at` of the most recent non-interrupted run the daemon recorded for
    /// `identifier`, parsed as RFC3339; `None` when there is no such run, the store is disabled, or
    /// no recent run has a parseable start time. It deliberately counts a still-running newest row
    /// (its start IS the boundary a mid-run summons must beat, INF-448); INTERRUPTED rows are skipped
    /// (boot recovery re-dispatches them, so counting their start would bury the triggering summons).
    /// Runs come back newest-first, so the first qualifying start is the newest. Mirrors Go
    /// `lastRunStartedAt` (its zero-`time.Time` sentinel becomes `None`).
    pub(crate) fn last_run_started_at(&self, identifier: &str) -> Option<DateTime<Utc>> {
        let runs = self.store().issue_history(identifier, "", 10).ok()?;
        for r in runs {
            if r.started_at.is_empty() || r.outcome == OUTCOME_INTERRUPTED {
                continue;
            }
            if let Ok(t) = DateTime::parse_from_rfc3339(&r.started_at) {
                return Some(t.with_timezone(&Utc));
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use chrono::{Duration as ChronoDuration, TimeZone, Utc};
    use rhapsody_config::DEPENDENCY_MODE_DISABLED;
    use rhapsody_core::{BlockerRef, Issue, normalize_state};

    use super::*;
    use crate::orchestrator::Orchestrator;
    use crate::testsupport::*;
    use std::sync::Arc;

    // Mirrors Go `TestSortForDispatchPriorityThenCreatedThenIdentifier`.
    #[test]
    fn sort_for_dispatch_priority_then_created_then_identifier() {
        let t0 = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let t1 = t0 + ChronoDuration::hours(1);
        let mut input = vec![
            Issue {
                identifier: "C-3".into(),
                priority: Some(2),
                created_at: Some(t1),
                ..Default::default()
            },
            Issue {
                identifier: "C-1".into(),
                priority: Some(1),
                created_at: Some(t1),
                ..Default::default()
            },
            Issue {
                identifier: "C-2".into(),
                priority: Some(2),
                created_at: Some(t0),
                ..Default::default()
            },
            // nil priority sorts last.
            Issue {
                identifier: "C-4".into(),
                priority: None,
                created_at: Some(t0),
                ..Default::default()
            },
            // tie with C-3 → identifier.
            Issue {
                identifier: "C-5".into(),
                priority: Some(2),
                created_at: Some(t1),
                ..Default::default()
            },
        ];
        sort_for_dispatch(&mut input);
        assert_eq!(ids(&input), vec!["C-1", "C-2", "C-3", "C-5", "C-4"]);
    }

    // Mirrors Go `TestEligibleHappyPath`.
    #[test]
    fn eligible_happy_path() {
        let g = GateData::standard();
        assert!(eligible(&base_issue(), &no_ids(), &no_ids(), &g.gate()));
    }

    // Mirrors Go `TestEligibleMissingFields`.
    #[test]
    fn eligible_missing_fields() {
        let g = GateData::standard();
        let mut bad = base_issue();
        bad.title = String::new();
        assert!(
            !eligible(&bad, &no_ids(), &no_ids(), &g.gate()),
            "issue missing title must be ineligible"
        );
    }

    // Mirrors Go `TestEligibleNonActiveOrTerminalState`.
    #[test]
    fn eligible_non_active_or_terminal_state() {
        let g = GateData::standard();
        let mut backlog = base_issue();
        backlog.state = "Backlog".into();
        assert!(
            !eligible(&backlog, &no_ids(), &no_ids(), &g.gate()),
            "non-active"
        );
        let mut done = base_issue();
        done.state = "Done".into();
        assert!(
            !eligible(&done, &no_ids(), &no_ids(), &g.gate()),
            "terminal"
        );
    }

    // Mirrors Go `TestEligibleRunningOrClaimed`.
    #[test]
    fn eligible_running_or_claimed() {
        let g = GateData::standard();
        let i = base_issue();
        assert!(
            !eligible(&i, &id_set(&["1"]), &no_ids(), &g.gate()),
            "running"
        );
        assert!(
            !eligible(&i, &no_ids(), &id_set(&["1"]), &g.gate()),
            "claimed"
        );
    }

    // Mirrors Go `TestEligibleTodoBlockerRule`.
    #[test]
    fn eligible_todo_blocker_rule() {
        let g = GateData::standard();
        let mut blocked = issue("2", "MT-2", "Todo");
        blocked.blocked_by = Some(vec![blocker(Some("MT-9"), Some("In Progress"))]);
        assert!(
            !eligible(&blocked, &no_ids(), &no_ids(), &g.gate()),
            "Todo with non-terminal blocker must be ineligible"
        );

        let mut ok = blocked.clone();
        ok.blocked_by = Some(vec![blocker(Some("MT-9"), Some("Done"))]);
        assert!(
            eligible(&ok, &no_ids(), &no_ids(), &g.gate()),
            "Todo with terminal blocker should be eligible"
        );

        let mut unk = blocked.clone();
        unk.blocked_by = Some(vec![blocker(Some("MT-9"), None)]);
        assert!(
            !eligible(&unk, &no_ids(), &no_ids(), &g.gate()),
            "unknown-state blocker must be ineligible"
        );

        let mut ip = issue("3", "MT-3", "In Progress");
        ip.blocked_by = Some(vec![blocker(None, Some("In Progress"))]);
        assert!(
            eligible(&ip, &no_ids(), &no_ids(), &g.gate()),
            "blocker rule applies only to Todo"
        );

        let mut empty = blocked.clone();
        empty.blocked_by = Some(vec![blocker(Some("MT-9"), Some(""))]);
        assert!(
            !eligible(&empty, &no_ids(), &no_ids(), &g.gate()),
            "empty-string blocker state must be ineligible"
        );
    }

    // Mirrors Go `TestEligibleLabelGate`.
    #[test]
    fn eligible_label_gate() {
        assert!(
            eligible(
                &base_issue(),
                &no_ids(),
                &no_ids(),
                &GateData::standard().gate()
            ),
            "empty required-label set must not filter"
        );
        let g = GateData::standard().with_labels(&["symphony-do"]);

        let mut hit = base_issue();
        hit.labels = Some(vec!["symphony-do".into(), "infra".into()]);
        assert!(
            eligible(&hit, &no_ids(), &no_ids(), &g.gate()),
            "carrying the label → eligible"
        );

        let mut miss = base_issue();
        miss.labels = Some(vec!["infra".into()]);
        assert!(
            !eligible(&miss, &no_ids(), &no_ids(), &g.gate()),
            "lacking the label → ineligible"
        );

        assert!(
            !eligible(&base_issue(), &no_ids(), &no_ids(), &g.gate()),
            "no labels + a required set → ineligible"
        );

        let mut mixed = base_issue();
        mixed.labels = Some(vec!["Symphony-Do".into()]);
        assert!(
            eligible(&mixed, &no_ids(), &no_ids(), &g.gate()),
            "label match is case-insensitive"
        );

        // A label miss must NOT be reported as a blocker drop.
        let res = eligibility(&miss, &no_ids(), &no_ids(), &g.gate());
        assert!(
            !res.ok && res.blocked_by.is_empty(),
            "label-miss must not be a blocker drop"
        );
    }

    // STUDIO-949: a `rhapsody:human` ticket in a dispatchable state is refused outright, and the
    // refusal is DISTINGUISHABLE from ordinary ineligibility — `held_for_human` is set and
    // `blocked_by` stays empty, so a caller can tell "held for a human" from "not a candidate".
    //
    // MUTATION: delete the `is_human` gate from `eligibility` and this reds.
    #[test]
    fn eligible_refuses_human_label() {
        let g = GateData::standard();
        let mut human = base_issue();
        human.labels = Some(vec!["rhapsody:human".into()]);

        assert!(
            !eligible(&human, &no_ids(), &no_ids(), &g.gate()),
            "a human-gated ticket must never dispatch"
        );
        let res = eligibility(&human, &no_ids(), &no_ids(), &g.gate());
        assert!(!res.ok, "refused");
        assert!(
            res.held_for_human,
            "the refusal must be distinguishable from a plain miss"
        );
        assert!(res.blocked_by.is_empty(), "not a blocker drop");

        // An ordinary ticket is NOT reported as held.
        assert!(
            !eligibility(&base_issue(), &no_ids(), &no_ids(), &g.gate()).held_for_human,
            "only a labelled ticket is held"
        );
    }

    // STUDIO-949: the label match normalizes at compare time (`trim` + lowercase), exactly as
    // `has_any_label` does.
    #[test]
    fn human_label_match_is_case_insensitive() {
        let g = GateData::standard();
        for spelling in [
            "rhapsody:human",
            "Rhapsody:Human",
            "RHAPSODY:HUMAN",
            " rhapsody:human ",
        ] {
            let mut human = base_issue();
            human.labels = Some(vec![spelling.into()]);
            assert!(
                eligibility(&human, &no_ids(), &no_ids(), &g.gate()).held_for_human,
                "spelling {spelling:?} must be held"
            );
        }
    }

    // STUDIO-949: the review-reopen ladder is the one dispatch path that bypasses `eligibility` (a
    // review-state issue is never active), so `review_reopen_eligible` carries the human gate itself.
    // The control below — the same ticket without the label — IS eligible, so the refusal is the
    // label's work and this function is pinned directly, not only through the select ladders.
    //
    // MUTATION: delete the `is_human` gate from `review_reopen_eligible` and the second assertion reds.
    #[test]
    fn review_reopen_refuses_a_human_ticket() {
        let mut o = Orchestrator::new("WORKFLOW.md");
        o.set_store(Arc::new(
            rhapsody_store::Sqlite::open(rhapsody_store::StorePath::InMemory).expect("store"),
        ));
        let store = o.store();
        let run = store
            .start_run(rhapsody_store::RunStart {
                issue_identifier: "A-1".to_string(),
                ..rhapsody_store::RunStart::default()
            })
            .expect("start run");
        store
            .end_run(run, rhapsody_store::RunEnd::default())
            .expect("end run");

        let summoned = |labels: Option<Vec<String>>| Issue {
            id: "1".into(),
            identifier: "A-1".into(),
            team_id: "team-1".into(),
            state: "In Review".into(),
            latest_summon_at: Some(Utc.with_ymd_and_hms(2030, 1, 1, 0, 0, 0).unwrap()),
            labels,
            ..Default::default()
        };
        let none: HashSet<String> = HashSet::new();
        assert!(
            o.review_reopen_eligible(&summoned(None), &none),
            "the reopen path is otherwise live; the label is the only refusal"
        );
        assert!(
            !o.review_reopen_eligible(&summoned(Some(vec!["rhapsody:human".into()])), &none),
            "a human ticket is never reopened"
        );
    }

    // STUDIO-949 acceptance: a ticket WITHOUT the label behaves identically to today. Written
    // against `eligible()`'s bool ONLY — the exact call the pre-STUDIO-949 code answered — so it
    // compiles and passes against both the old and the new implementation.
    #[test]
    fn eligible_unaffected_without_human_label() {
        let g = GateData::standard();
        assert!(
            eligible(&base_issue(), &no_ids(), &no_ids(), &g.gate()),
            "an ordinary ticket still dispatches"
        );
        let mut labelled = base_issue();
        labelled.labels = Some(vec!["infra".into()]);
        assert!(
            eligible(&labelled, &no_ids(), &no_ids(), &g.gate()),
            "an unrelated label must not change the verdict"
        );
        // The ordinary blocker rule is untouched.
        let mut blocked = issue("2", "MT-2", "Todo");
        blocked.blocked_by = Some(vec![blocker(Some("MT-9"), Some("In Progress"))]);
        assert!(
            !eligible(&blocked, &no_ids(), &no_ids(), &g.gate()),
            "a blocked Todo is still refused for the ordinary reason"
        );
    }

    // Mirrors Go `TestEligibilityReportsAllNonTerminalBlockers`.
    #[test]
    fn eligibility_reports_all_non_terminal_blockers() {
        let g = GateData::standard();
        let mut iss = issue("2", "MT-2", "Todo");
        iss.blocked_by = Some(vec![
            blocker(Some("MT-9"), Some("In Review")), // non-terminal → included
            blocker(Some("MT-8"), Some("Done")),      // terminal → excluded
            blocker(Some("MT-7"), None),              // unknown → included (conservative)
        ]);
        let res = eligibility(&iss, &no_ids(), &no_ids(), &g.gate());
        assert!(!res.ok, "blocked Todo must be ineligible");
        assert_eq!(res.blocked_by.len(), 2, "want 2 non-terminal blockers");
        assert_eq!(
            blocker_identifier(&res.blocked_by[0]),
            "MT-9",
            "declaration order preserved"
        );
        assert_eq!(blocker_identifier(&res.blocked_by[1]), "MT-7");
    }

    // Mirrors Go `TestEligibilityOKReportsNoBlockers`.
    #[test]
    fn eligibility_ok_reports_no_blockers() {
        let g = GateData::standard();
        let res = eligibility(&base_issue(), &no_ids(), &no_ids(), &g.gate());
        assert!(res.ok && res.blocked_by.is_empty(), "eligible issue");

        let mut tb = issue("2", "MT-2", "Todo");
        tb.blocked_by = Some(vec![blocker(Some("MT-9"), Some("Done"))]);
        let res = eligibility(&tb, &no_ids(), &no_ids(), &g.gate());
        assert!(
            res.ok && res.blocked_by.is_empty(),
            "terminal-blocker issue"
        );
    }

    // Mirrors Go `TestEligibilityNonBlockerReasonsReportNoBlockers`.
    #[test]
    fn eligibility_non_blocker_reasons_report_no_blockers() {
        let g = GateData::standard();
        let with_blocker = |mut base: Issue| {
            base.blocked_by = Some(vec![blocker(Some("MT-9"), Some("In Review"))]);
            base
        };
        struct Case {
            name: &'static str,
            iss: Issue,
            running: std::collections::HashSet<String>,
            claimed: std::collections::HashSet<String>,
            want_ok: bool,
        }
        let cases = vec![
            Case {
                name: "running",
                iss: with_blocker(issue("1", "MT-1", "Todo")),
                running: id_set(&["1"]),
                claimed: no_ids(),
                want_ok: false,
            },
            Case {
                name: "claimed",
                iss: with_blocker(issue("1", "MT-1", "Todo")),
                running: no_ids(),
                claimed: id_set(&["1"]),
                want_ok: false,
            },
            Case {
                name: "non-active",
                iss: with_blocker(issue("1", "MT-1", "Backlog")),
                running: no_ids(),
                claimed: no_ids(),
                want_ok: false,
            },
            Case {
                name: "missing-fields",
                iss: with_blocker({
                    let mut i = issue("1", "MT-1", "Todo");
                    i.title = String::new();
                    i
                }),
                running: no_ids(),
                claimed: no_ids(),
                want_ok: false,
            },
            Case {
                name: "in-progress-eligible",
                iss: with_blocker(issue("1", "MT-1", "In Progress")),
                running: no_ids(),
                claimed: no_ids(),
                want_ok: true,
            },
        ];
        for tc in cases {
            let res = eligibility(&tc.iss, &tc.running, &tc.claimed, &g.gate());
            assert_eq!(res.ok, tc.want_ok, "{}: ok", tc.name);
            assert!(
                res.blocked_by.is_empty(),
                "{}: must not be a blocker drop",
                tc.name
            );
        }
    }

    // Mirrors Go `TestBlockerStateNameAndIdentifier`.
    #[test]
    fn blocker_state_name_and_identifier() {
        assert_eq!(
            blocker_state_name(&blocker(None, Some("In Review"))),
            "In Review"
        );
        assert_eq!(blocker_state_name(&blocker(None, None)), "unknown");
        assert_eq!(blocker_state_name(&blocker(None, Some(""))), "unknown");
        assert_eq!(blocker_identifier(&blocker(Some("MT-9"), None)), "MT-9");
        assert_eq!(
            blocker_identifier(&BlockerRef {
                id: Some("uuid-123".into()),
                identifier: None,
                state: None
            }),
            "uuid-123"
        );
        assert_eq!(
            blocker_identifier(&BlockerRef {
                id: None,
                identifier: None,
                state: None
            }),
            "unknown"
        );
    }

    // Mirrors Go `TestPRSuppressed`.
    #[test]
    fn pr_suppressed() {
        let (o, st) = orch_with_store();

        assert!(
            !o.pr_suppressed(&base_issue()),
            "no linked PR → not suppressed"
        );

        let mut pr_no_comment = base_issue();
        pr_no_comment.linked_pr = true;
        assert!(
            o.pr_suppressed(&pr_no_comment),
            "linked PR, no summons → suppressed"
        );

        // Linked PR + a summons but no run-start watermark and no PR-activity time → lenient.
        let mut lenient = pr_no_comment.clone();
        lenient.latest_summon_at = Some(Utc.with_ymd_and_hms(2026, 6, 3, 12, 0, 0).unwrap());
        assert!(
            !o.pr_suppressed(&lenient),
            "linked PR + summons but no watermark → lenient"
        );

        let run_start = Utc.with_ymd_and_hms(2026, 6, 3, 12, 0, 0).unwrap();
        seed_run(
            st.as_ref(),
            "u-1",
            "MT-1",
            run_start + ChronoDuration::minutes(1),
        );

        let mut base = base_issue();
        base.id = "u-1".into();
        base.identifier = "MT-1".into();
        base.linked_pr = true;

        // Summons AFTER run start → NOT suppressed (re-dispatch); PR activity newer than the summons
        // must no longer matter (the INF-448 dead zone).
        let mut newer = base.clone();
        newer.latest_summon_at = Some(run_start + ChronoDuration::hours(1));
        newer.latest_pr_activity_at = Some(run_start + ChronoDuration::hours(2));
        assert!(
            !o.pr_suppressed(&newer),
            "summons newer than last run START must lift suppression"
        );

        // Summons BEFORE run start → suppressed (stale).
        let mut older = base.clone();
        older.latest_summon_at = Some(run_start - ChronoDuration::hours(1));
        assert!(
            o.pr_suppressed(&older),
            "summons older than last run START must stay suppressed"
        );
    }

    // Mirrors Go `TestPRSuppressedStoreDisabled` (Noop store → PR-activity fallback).
    #[test]
    fn pr_suppressed_store_disabled() {
        let o = Orchestrator::new("WORKFLOW.md"); // Noop store (never set_store'd)
        let pr = Utc.with_ymd_and_hms(2026, 6, 3, 12, 0, 0).unwrap();
        let mut iss = base_issue();
        iss.linked_pr = true;
        assert!(
            o.pr_suppressed(&iss),
            "linked PR, no summons → suppressed even with the store off"
        );

        iss.latest_pr_activity_at = Some(pr);
        iss.latest_summon_at = Some(pr - ChronoDuration::hours(1));
        assert!(
            o.pr_suppressed(&iss),
            "store off: summons older than PR activity → suppressed"
        );

        iss.latest_summon_at = Some(pr + ChronoDuration::hours(1));
        assert!(
            !o.pr_suppressed(&iss),
            "store off: summons newer than PR activity → not suppressed"
        );

        iss.latest_pr_activity_at = None;
        assert!(
            !o.pr_suppressed(&iss),
            "store off, no PR-activity time, a summons → lenient"
        );
    }

    // Mirrors Go `TestDependencyModeEnabled` (dispatch_depmode_test.go).
    #[test]
    fn dependency_mode_enabled_table() {
        for (mode, want) in [
            ("", false),
            ("disabled", false),
            ("graphite", true),
            ("dag", true),
        ] {
            assert_eq!(dependency_mode_enabled(mode), want, "mode {mode:?}");
        }
    }

    // Mirrors Go `TestBlockerClearedTable`.
    #[test]
    fn blocker_cleared_table() {
        let (review, terminal, canceled) = (dep_review(), dep_terminal(), dep_canceled());
        let cases: &[(&str, &str, bool)] = &[
            // disabled: terminal-only; review/active NOT cleared; cancelled-but-terminal cleared; nil not.
            ("disabled", "Done", true),
            ("disabled", "In Review", false),
            ("disabled", "Todo", false),
            ("disabled", "Cancelled", true),
            ("disabled", "", false),
            ("", "Done", true), // unset == disabled
            ("", "In Review", false),
            // graphite: review OR terminal clears; active not; cancelled never; nil not.
            ("graphite", "In Review", true),
            ("graphite", "Done", true),
            ("graphite", "Todo", false),
            ("graphite", "Cancelled", false),
            ("graphite", "", false),
            // dag: terminal-only; review NOT cleared; cancelled never; nil not.
            ("dag", "In Review", false),
            ("dag", "Done", true),
            ("dag", "Cancelled", false),
            ("dag", "", false),
        ];
        for (mode, state, want) in cases {
            let got = blocker_cleared(&blocker_state(state), mode, &review, &terminal, &canceled);
            assert_eq!(got, *want, "blocker_cleared({state:?}, mode={mode:?})");
        }
    }

    // Mirrors Go `TestBlockerClearedDisabledEqualsPreFeature`.
    #[test]
    fn blocker_cleared_disabled_equals_pre_feature() {
        let (review, terminal, canceled) = (dep_review(), dep_terminal(), dep_canceled());
        let old_blocker_terminal = |b: &BlockerRef| -> bool {
            match &b.state {
                None => false,
                Some(s) => terminal.contains(&normalize_state(s)),
            }
        };
        for state in ["Todo", "In Review", "Done", "Cancelled", ""] {
            let b = blocker_state(state);
            let got = blocker_cleared(&b, DEPENDENCY_MODE_DISABLED, &review, &terminal, &canceled);
            assert_eq!(
                got,
                old_blocker_terminal(&b),
                "disabled blocker_cleared({state:?})"
            );
        }
    }

    // Mirrors Go `TestEligibilityModeAwareInReviewBlocker`.
    #[test]
    fn eligibility_mode_aware_in_review_blocker() {
        let mut iss = issue("1", "MT-2", "Todo");
        iss.blocked_by = Some(vec![blocker_state("In Review")]);
        let cases: &[(&str, bool, usize)] = &[
            ("graphite", true, 0),
            ("dag", false, 1),
            ("disabled", false, 1),
            ("", false, 1),
        ];
        for (mode, want_ok, want_blk) in cases {
            let g = GateData::dep().with_mode(mode);
            let res = eligibility(&iss, &no_ids(), &no_ids(), &g.gate());
            assert_eq!(res.ok, *want_ok, "mode={mode:?}");
            assert_eq!(
                res.blocked_by.len(),
                *want_blk,
                "mode={mode:?}: must surface for logging"
            );
        }
    }

    // STUDIO-949 round 5 — the CURRENT hold set must be unique by identifier across notes that never
    // had a `begin_pass` between them. `begin_pass` runs only when a selection pass runs; a
    // candidate-fetch error returns before either ladder clears the set, yet `promote_unblocked`
    // still notes the same Backlog dependent on every outage tick. Without this the set grows one
    // identical row per tick and the Now strip's `held_for_human` count inflates.
    //
    // MUTATION: revert the find/replace in `hold` to an unconditional `push` and this reds (2 rows).
    #[test]
    fn human_hold_set_is_unique_by_identifier_across_notes() {
        let ledger = HumanHoldLedger::default();
        let entry = |project: &str| HeldForHuman {
            issue_identifier: "STUDIO-939".into(),
            title: "wire the stores".into(),
            project: project.into(),
        };
        assert!(ledger.hold(entry("booch")), "the first note is news");
        assert!(
            !ledger.hold(entry("")),
            "a repeat is not news, so it is not logged again"
        );
        assert_eq!(
            ledger.held().len(),
            1,
            "two notes with no begin_pass between them are ONE held ticket: {:?}",
            ledger.held()
        );
        ledger.begin_pass();
        assert!(ledger.held().is_empty(), "begin_pass still clears the set");
        assert!(!ledger.hold(entry("booch")), "the announced set survives");
    }
}
