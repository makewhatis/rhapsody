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
//!   * **A deliberate divergence, not a porting detail (STUDIO-1045):** `pr_suppressed` and
//!     `review_reopen_eligible` measure a summons against the ticket's last author-run WINDOW (its
//!     start and end), not its start alone, so a summon-token comment created while the author run
//!     was live (or before it handed off) does not re-engage the author. The divergence from Go's
//!     run-START boundary is recorded in the README "Divergences" entry for GitHub summons
//!     (STUDIO-875/882).
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
/// needs a person while the daemon is gated — so keeping a populated set is the honest answer.
///
/// The converse is also load-bearing: on a daemon held by one of those gates since boot, NO pass has
/// ever READ THE BOARD, so an empty set is not "no hold" but "nothing has looked", and reading it as
/// the former is how a `rhapsody:human` ticket's pull request self-merges on a drained daemon
/// (STUDIO-949 rounds 11-13). [`HumanHoldState::primed`] carries that distinction; every `labelled()`
/// decision gate that does not flow through the per-tick candidate pass — the ticketless watcher's
/// round and auto-merge gates, the reconciliation sweep and the ticket-mode handoff quorum — fails
/// closed while it is `false`.
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
    /// Whether a selection pass has READ THE BOARD in this process — set by the first
    /// [`HumanHoldLedger::begin_pass`] that was told the candidate fetch succeeded, and never
    /// cleared (STUDIO-949 rounds 11-13).
    ///
    /// `labelled` is written ONLY from inside a selection pass (both ladders) or from the
    /// auto-promote pass that runs immediately after one, and every one of those writers sits BELOW
    /// `on_tick`'s three early-return gates (a failed config validation, an armed drain, a dead
    /// agent credential). On a daemon held by one of those gates `labelled` is therefore empty for
    /// the WHOLE process lifetime, and a decision gate reading it would see "no hold" rather than
    /// "no information". FOUR gates turn on it and none flows through the per-tick candidate pass:
    /// the ticketless watcher's round gate and its auto-merge gate (both `Event::ReviewSweep` from
    /// the watcher's 120s task), the reconciliation sweep's held-row filter (from `on_tick` ABOVE
    /// the gates) and the ticket-mode handoff quorum (from the `evHandoffRun` handler, which is not
    /// on `on_tick` at all). All four keep executing while dispatch is gated.
    ///
    /// `primed` is that distinction: `false` until a pass has actually looked, so those four gates
    /// fail CLOSED instead of silently open. `begin_pass` is the only writer on purpose — the
    /// auto-promote pass observes only Backlog dependents, a partial view, and must not be able to
    /// make an unknown label set look known.
    ///
    /// It is deliberately "read the WHOLE board", not "ran a pass" (STUDIO-949 rounds 13-15): a
    /// `projects:` install's ladder is reached unconditionally even when a project's candidate fetch
    /// failed (`poll_all_projects` `continue`s past each error), and `begin_pass` clears both sets
    /// WHOLESALE with no per-project scope. So a pass that could only see SOME of the board is the
    /// unknown-set case this latch exists to forbid — priming on it would erase the holds of the
    /// project that failed and mark the result known. The verdict is threaded in through
    /// [`HumanHoldLedger::begin_pass`], and it is "EVERY enabled project answered", with an
    /// all-paused install (`zero`) counting as NOT read; a pass that could not look neither clears
    /// nor primes, leaving the last good answer (or the un-primed state) standing.
    primed: bool,
}

impl Default for HumanHoldLedger {
    fn default() -> Self {
        Self {
            inner: Mutex::new(HumanHoldState::default()),
        }
    }
}

impl HumanHoldLedger {
    /// Starts a fresh selection pass over a board the caller COULD SEE: the CURRENT hold set is
    /// dropped, so a ticket that stopped wearing the label — or left the candidate set — stops being
    /// reported. The announced set is deliberately NOT touched, and the **primed** flag is set: this
    /// is the first moment the process can be said to have looked at all, which is what lets the
    /// fail-closed decision gates (the ticketless watcher's round and auto-merge gates, the
    /// reconciliation sweep's held-row filter, the ticket-mode handoff quorum) read an answer.
    ///
    /// `read_the_board` is the candidate fetch's verdict (STUDIO-949 rounds 13-15): `true` when
    /// EVERY enabled project answered. When it is `false` the pass could not see the whole board —
    /// any project's fetch failed, or there are no enabled projects at all (an all-paused install) —
    /// so this does NOTHING: the sets are neither cleared nor primed, and the last answer (or the
    /// un-primed state) stands. Clearing on a partial fetch would reopen every gate for the failed
    /// project's holds by emptying the set; priming would mark an unknown set known. The over-hold
    /// cost of the all-projects predicate is deliberate: a label that comes OFF keeps refusing while
    /// any project is unreadable. The legacy single-tracker path passes `true` by construction (its
    /// failed fetch returns before the ladder). No other method sets `primed`; see
    /// [`HumanHoldState::primed`].
    pub(crate) fn begin_pass(&self, read_the_board: bool) {
        if !read_the_board {
            return;
        }
        let mut st = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        st.held.clear();
        st.labelled.clear();
        st.primed = true;
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
    /// the daemon is running right now — lowercased — TOGETHER WITH the priming latch, read under
    /// ONE lock (STUDIO-949 round 13).
    ///
    /// This is the decision signal for a gate on a RUNNING ticket (the handoff review decision, the
    /// ticketless origin gate); the console reads [`Self::held`] instead, which excludes live work.
    /// The latch distinguishes "the last pass saw no hold" from "no pass has read the board yet":
    /// while it is `false` the set is an absence of information, and a gate that treated it as "no
    /// hold" would fail open on a daemon whose dispatch is gated (see [`HumanHoldState::primed`]).
    ///
    /// The two are returned together rather than by separate accessors because read separately a
    /// pass landing between the two calls looks like this: the gate reads an empty (un-primed) set,
    /// the pass primes it with a real one, the gate then reads `primed == true` and treats the empty
    /// set it already holds as a settled "no hold". One lock returns the pair one pass produced.
    pub(crate) fn labelled_and_primed(&self) -> (HashSet<String>, bool) {
        let st = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        (st.labelled.clone(), st.primed)
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
    ///
    /// `running` is the IMPLEMENTATION pool — the count the draw beside it actually used
    /// ([`Orchestrator::implementation_pool_holders`]) — not every live run (STUDIO-950). With
    /// `agent.max_concurrent_reviews` set, a daemon exactly at its implementation cap with two
    /// reviews in flight would otherwise log `max_concurrent=4 running=6`: a line that reads as an
    /// overrun where nothing overran, and the very line STUDIO-950's ticket quotes as the
    /// incident's evidence. [`Orchestrator::review_pool_holders`] makes this argument on the review
    /// side already (`holding=0` while four implementations spend the shared pool is a lie an
    /// operator tuning the key cannot act on); this is the same correction applied symmetrically.
    /// The total is not lost — it is beside it as `live_runs`, and the two are equal on every
    /// install that never sets the key.
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
            running = self.implementation_pool_holders(),
            live_runs = self.running.len(),
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
    /// when its Linear state briefly flaps back to active. Mirrors Go `prSuppressed`, except for the
    /// author-run window rule below (STUDIO-1045 — see README "Divergences").
    ///
    /// Re-open rule (INF-448): a summons strictly newer than the ticket's LAST RUN lifts the
    /// suppression. Comparing to the run's END (STUDIO-1045) rather than its start means a comment
    /// created while the run was live — or before it handed off — is not a summons: every agent
    /// posts from the operator's one GitHub account, so the author's own "I fixed it" reply must not
    /// re-dispatch the author. A summons posted after the window still lifts the suppression.
    /// Store-off fallback: with no run watermark the pre-INF-448 PR-activity comparison applies; a
    /// PR with no comparable activity time stays lenient so a legitimately-summoned issue is never
    /// wedged by missing metadata. Applied to FRESH pickups only.
    pub(crate) fn pr_suppressed(&self, iss: &Issue) -> bool {
        if !iss.linked_pr {
            return false;
        }
        let summon = match iss.latest_summon_at {
            Some(s) => s,
            // a linked PR but no summons → already-done work, nothing new → suppress.
            None => return true,
        };
        match self.last_run_window(&iss.identifier) {
            None => match iss.latest_pr_activity_at {
                // no watermark of any kind but a summons exists → be lenient.
                None => false,
                // pre-INF-448 fallback: suppress unless the summons is after the PR's last activity.
                Some(pr) => summon <= pr,
            },
            Some(w) => {
                if w.contains(summon) {
                    self.log_ignored_author_summons(iss, &w, summon);
                }
                match w.ended_at {
                    // A run still live (no end recorded) cannot have its window beaten: any summons
                    // the window does not contain predates it, and nothing newer has arrived.
                    None => true,
                    // Only a summons strictly newer than the run's END lifts the suppression.
                    Some(end) => summon <= end,
                }
            }
        }
    }

    /// Reports whether a review-state issue should be re-engaged this tick by a fresh summons
    /// (symphony-29). The review-branch counterpart to [`eligible`] (which intentionally rejects
    /// non-active states); a review issue is handled ONLY here. Eligible iff it is neither running
    /// nor claimed, carries a `team_id` (required to promote it), carries a summons, AND that
    /// summons is strictly newer than the END of the daemon's last run on it. No run / store
    /// disabled / unparseable start ⇒ NOT eligible (the daemon never grabs a human-managed review
    /// ticket it has never worked; the check converges). A `rhapsody:human` ticket is NEVER eligible
    /// (STUDIO-949) — this ladder bypasses `eligibility`, so the human gate must be repeated here or
    /// the label leaks dispatch through the one path that does not consult it. Mirrors Go
    /// `reviewReopenEligible`, except for the run-END boundary (STUDIO-1045 — see README
    /// "Divergences"): a comment created while the ticket's own author run was live, or before it
    /// handed off, is not a summons and must not re-engage the author.
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
        match self.last_run_window(&iss.identifier) {
            // never worked it (or store off / no start time) → don't grab it.
            None => false,
            Some(w) => {
                // ONLY a summons strictly newer than the run's END re-engages the author: the
                // author-run window is `[start, end]`, and a comment created inside it is the
                // author's own (every agent posts from the operator's account). A still-live run
                // (no end) is never beaten. This boundary is the STUDIO-1045 fix.
                let eligible = w.ended_at.is_some_and(|end| summon > end);
                if !eligible && w.contains(summon) {
                    self.log_ignored_author_summons(iss, &w, summon);
                }
                eligible
            }
        }
    }

    /// Logs, at most once per (ticket, summon instant), that a summons was IGNORED because the
    /// comment was created inside the live window of the ticket's own author run (STUDIO-1045).
    /// Every agent posts to GitHub under the operator's ONE account, so the daemon cannot tell the
    /// author's own reply from a human's by actor; the run window is what it does know. Repeating
    /// this on every poll (both predicates consult the window) would be background noise, hence the
    /// [`IgnoredSummonLog`] memo.
    fn log_ignored_author_summons(&self, iss: &Issue, w: &RunWindow, at: DateTime<Utc>) {
        if !self.ignored_author_summons.claim(&iss.identifier, at) {
            return;
        }
        tracing::info!(
            issue_identifier = %iss.identifier,
            summon_at = %at,
            run_started_at = %w.started_at,
            run_ended_at = ?w.ended_at,
            "ignoring a summons created while this ticket's own author run was live: the author's \
             own comment (same GitHub account as every agent) is not a request to re-engage it"
        );
    }

    /// The live window of the most recent non-interrupted, non-refused run the daemon recorded for
    /// `identifier`; `None` when there is no such run, the store is disabled, or no qualifying row
    /// has a parseable `started_at`. It deliberately includes a still-running newest row with
    /// `ended_at: None` (its window is open until it ends); INTERRUPTED rows are skipped (boot
    /// recovery re-dispatches them, so counting their start would bury the triggering summons), and
    /// so are `refused` rows (a zero-turn refusal is not a run a summons had to beat). Runs come
    /// back newest-first, so the first qualifying row is the newest.
    ///
    /// The END is the boundary both [`pr_suppressed`] and [`review_reopen_eligible`] measure a
    /// summons against (STUDIO-1045): a comment created before the run ended was posted while the
    /// author could have written it. Mirrors Go `lastRunStartedAt`'s row selection, widened to carry
    /// the run's end; its zero-`time.Time` sentinel becomes `None`.
    pub(crate) fn last_run_window(&self, identifier: &str) -> Option<RunWindow> {
        // Newest 10 rows only (Go's `issueHistory(…, 10)` bound) — enough to find a recent run and
        // bound the store read on the dispatch path. A ticket whose ten newest rows were ALL refusal
        // episodes (ten separate episodes, so unlikely) reads as "never worked"; the window is
        // deliberately not widened here (STUDIO-988 review round 5, jimmy smaller).
        let runs = self.store().issue_history(identifier, "", 10).ok()?;
        for r in runs {
            // `refused` rows are skipped for the same reason `interrupted` ones are: a zero-turn
            // refusal is not a run a summons had to beat, and counting its start would hide the
            // triggering summons from a later reopen (STUDIO-988 review round 4, jimmy #3).
            if r.started_at.is_empty()
                || r.outcome == OUTCOME_INTERRUPTED
                || r.outcome == rhapsody_store::OUTCOME_REFUSED
            {
                continue;
            }
            let Ok(start) = DateTime::parse_from_rfc3339(&r.started_at) else {
                continue;
            };
            // An empty `ended_at` is a row still running (`end_run` always stamps one otherwise).
            // A corrupt (unparseable) end is treated as absent too, which makes the window
            // open-ended: a daemon that cannot read when a run ended errs toward NOT re-engaging
            // the author rather than toward a spurious re-dispatch.
            let ended_at = if r.ended_at.is_empty() {
                None
            } else {
                DateTime::parse_from_rfc3339(&r.ended_at)
                    .ok()
                    .map(|t| t.with_timezone(&Utc))
            };
            return Some(RunWindow {
                started_at: start.with_timezone(&Utc),
                ended_at,
            });
        }
        None
    }
}

/// The live window of one author run: when it began and, once it ended, when it ended
/// (`ended_at: None` while the run is still going). A summon-token comment created inside this
/// window cannot be the author asking for new work — every agent posts from the operator's one
/// GitHub account — so it does not re-engage the author (STUDIO-1045).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RunWindow {
    pub(crate) started_at: DateTime<Utc>,
    pub(crate) ended_at: Option<DateTime<Utc>>,
}

impl RunWindow {
    /// Whether `at` falls inside the window: at or after the run's start and, once the run has
    /// ended, at or before that end. A run with no recorded end has an open-ended window.
    fn contains(&self, at: DateTime<Utc>) -> bool {
        at >= self.started_at
            && match self.ended_at {
                Some(end) => at <= end,
                None => true,
            }
    }
}

/// Which "this summon-token comment was created inside the ticket's own author-run window" notices
/// have already been logged (STUDIO-1045). Mirrors [`crate::ghenrich::SummonDropLog`]: interior-
/// mutable for the `&self` predicates, control-task-owned, and NOT an off-loop state seam. A
/// poisoned lock is recovered rather than propagated — the worst a lost set costs is a repeated
/// info line.
#[derive(Debug, Default)]
pub struct IgnoredSummonLog {
    seen: Mutex<HashSet<String>>,
}

impl IgnoredSummonLog {
    /// Claims the one log slot for (ticket, summon instant); `false` when already claimed.
    fn claim(&self, identifier: &str, at: DateTime<Utc>) -> bool {
        let key = format!("{identifier}@{}", at.to_rfc3339());
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key)
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

    // STUDIO-988 review round 5 (alice A): a zero-turn `refused` row must not count as "the last run
    // a summons has to beat", or the refusal hides the very summons that should re-offer the reopen
    // (the STUDIO-649 loss). The reopen fixtures cannot see this: their summons is dated 2030 while
    // the refusal row is dated now, so the row never post-dates it. Here the refusal lands AFTER the
    // summons, in the production ordering.
    //
    // MUTATION GUARD: drop the `OUTCOME_REFUSED` skip in `last_run_started_at` and this reds.
    #[test]
    fn a_refused_row_does_not_hide_a_newer_summons_from_a_reopen() {
        let mut o = Orchestrator::new("WORKFLOW.md");
        o.set_store(Arc::new(
            rhapsody_store::Sqlite::open(rhapsody_store::StorePath::InMemory).expect("store"),
        ));
        let store = o.store();
        // An ordinary run that ended BEFORE the summons.
        let run = store
            .start_run(rhapsody_store::RunStart {
                issue_identifier: "A-1".to_string(),
                started_at: "2029-01-01T00:00:00Z".to_string(),
                ..rhapsody_store::RunStart::default()
            })
            .expect("start run");
        store
            .end_run(run, rhapsody_store::RunEnd::default())
            .expect("end run");
        // A zero-turn refusal recorded AFTER the summons.
        let refused = store
            .start_run(rhapsody_store::RunStart {
                issue_identifier: "A-1".to_string(),
                started_at: "2029-06-15T00:00:00Z".to_string(),
                ..rhapsody_store::RunStart::default()
            })
            .expect("start refused run");
        store
            .end_run(
                refused,
                rhapsody_store::RunEnd {
                    outcome: rhapsody_store::OUTCOME_REFUSED.to_string(),
                    ..rhapsody_store::RunEnd::default()
                },
            )
            .expect("end refused run");

        let iss = Issue {
            id: "1".into(),
            identifier: "A-1".into(),
            team_id: "team-1".into(),
            state: "In Review".into(),
            latest_summon_at: Some(Utc.with_ymd_and_hms(2029, 6, 1, 0, 0, 0).unwrap()),
            ..Default::default()
        };
        assert!(
            o.review_reopen_eligible(&iss, &HashSet::new()),
            "a refusal row must not hide the newer summons that re-offers the reopen"
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

        // Summons AFTER the run's window (which `seed_run` ends a minute after `run_start`) → NOT
        // suppressed (re-dispatch); PR activity newer than the summons must no longer matter (the
        // INF-448 dead zone).
        let mut newer = base.clone();
        newer.latest_summon_at = Some(run_start + ChronoDuration::hours(1));
        newer.latest_pr_activity_at = Some(run_start + ChronoDuration::hours(2));
        assert!(
            !o.pr_suppressed(&newer),
            "summons newer than the last run's window must lift suppression"
        );

        // Summons BEFORE the run's window → suppressed (stale).
        let mut older = base.clone();
        older.latest_summon_at = Some(run_start - ChronoDuration::hours(1));
        assert!(
            o.pr_suppressed(&older),
            "summons older than the last run's window must stay suppressed"
        );
    }

    // STUDIO-1045 acceptance, the reported STUDIO-1002 shape: a summon-token comment created INSIDE
    // the ticket's own author-run window (the author's own "I fixed it" PR reply) must not reopen or
    // re-dispatch the author. The window is `[start, end]`; the comment sits inside it.
    //
    // MUTATION: measure the summons against the run's START (the pre-STUDIO-1045 rule) instead of
    // its window end and every `inside` assertion here reds.
    #[test]
    fn a_summons_created_inside_the_author_run_window_does_not_re_dispatch() {
        let (o, st) = orch_with_store();
        // `seed_run(ended_at)` seeds the window `[ended_at - 1m, ended_at]`.
        let win_start = Utc.with_ymd_and_hms(2026, 6, 3, 12, 0, 0).unwrap();
        seed_run(
            st.as_ref(),
            "ID-1",
            "MT-1",
            win_start + ChronoDuration::minutes(1),
        );
        let inside = win_start + ChronoDuration::seconds(30);

        let mut iss = base_issue();
        iss.id = "ID-1".into();
        iss.identifier = "MT-1".into();
        iss.linked_pr = true;
        iss.latest_summon_at = Some(inside);

        assert!(
            o.pr_suppressed(&iss),
            "a summons created while the author run was live must not lift linked-PR suppression"
        );

        let mut review = iss.clone();
        review.state = "In Review".into();
        review.team_id = "team-1".into();
        assert!(
            !o.review_reopen_eligible(&review, &HashSet::new()),
            "a summons created while the author run was live must not reopen the author"
        );

        // Control: the SAME comment, timestamped after the run ended (i.e. after it handed off),
        // still summons — the daemon's own review-completion comment and a genuine operator comment
        // land there, and neither may be blocked by the window rule.
        let after = win_start + ChronoDuration::hours(1);
        let mut outside = iss.clone();
        outside.latest_summon_at = Some(after);
        assert!(
            !o.pr_suppressed(&outside),
            "a summons after the run window still lifts suppression"
        );
        let mut outside_review = review.clone();
        outside_review.latest_summon_at = Some(after);
        assert!(
            o.review_reopen_eligible(&outside_review, &HashSet::new()),
            "a summons after the run window still reopens"
        );
    }

    // STUDIO-1045: a run still live (no `ended_at`) has an OPEN window — a summon posted after its
    // start is inside it, so it cannot reopen. Mirrors the incident's live window before the run
    // ended.
    #[test]
    fn a_summon_after_a_still_live_runs_start_is_inside_its_window() {
        let (o, st) = orch_with_store();
        let start = Utc.with_ymd_and_hms(2026, 6, 3, 12, 0, 0).unwrap();
        st.start_run(rhapsody_store::RunStart {
            issue_identifier: "MT-1".into(),
            started_at: "2026-06-03T12:00:00Z".into(),
            ..rhapsody_store::RunStart::default()
        })
        .expect("start run");

        let mut review = base_issue();
        review.id = "ID-1".into();
        review.identifier = "MT-1".into();
        review.state = "In Review".into();
        review.team_id = "team-1".into();
        review.latest_summon_at = Some(start + ChronoDuration::minutes(30));

        assert!(
            !o.review_reopen_eligible(&review, &HashSet::new()),
            "a live run's window is open: a mid-run summons cannot reopen"
        );
    }

    // STUDIO-1045: an ignored self-summons is reported ONCE at `info` — naming the run window — so
    // an operator can tell "ignored by design" from "the summons was lost", and a long wait on an
    // unchanged polling ticket does not re-log it every tick.
    #[test]
    fn an_ignored_self_summons_is_logged_once() {
        let (o, st) = orch_with_store();
        let win_start = Utc.with_ymd_and_hms(2026, 6, 3, 12, 0, 0).unwrap();
        seed_run(
            st.as_ref(),
            "ID-1",
            "MT-1",
            win_start + ChronoDuration::minutes(1),
        );

        let mut review = base_issue();
        review.id = "ID-1".into();
        review.identifier = "MT-1".into();
        review.state = "In Review".into();
        review.team_id = "team-1".into();
        review.latest_summon_at = Some(win_start + ChronoDuration::seconds(30));

        let (eligible, events) =
            capture_events(|| o.review_reopen_eligible(&review, &HashSet::new()));
        assert!(!eligible);
        assert_eq!(
            events
                .iter()
                .filter(|e| e.level == "INFO" && e.message.contains("ignoring a summons"))
                .count(),
            1,
            "an ignored self-summons must be logged exactly once at info, got {events:?}"
        );

        // A repeat poll carrying the SAME comment does not re-log it.
        let (_again, events) =
            capture_events(|| o.review_reopen_eligible(&review, &HashSet::new()));
        assert!(
            events
                .iter()
                .all(|e| !e.message.contains("ignoring a summons")),
            "the same summon instant must not be reported twice, got {events:?}"
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
        ledger.begin_pass(true);
        assert!(ledger.held().is_empty(), "begin_pass still clears the set");
        assert!(!ledger.hold(entry("booch")), "the announced set survives");
    }
}
