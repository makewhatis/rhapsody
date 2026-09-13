//! drain — the "stop starting new work, let what is running finish" signal (STUDIO-880;
//! Rhapsody-only, no Go v0.4.0 counterpart).
//!
//! # Why a drain, and why the turn boundary
//!
//! Upgrading the daemon means restarting it, and a restart today throws away whatever turn was in
//! flight. It cannot do otherwise: the runner spawns the agent with piped stdin/stdout/stderr and the
//! DAEMON owns the read end ([`rhapsody_agent::claude`]), so a live turn dies with the process that
//! is reading it — a pipe cannot be handed to a successor. Re-attaching a running agent to a fresh
//! daemon is therefore impossible, not merely unbuilt.
//!
//! The unit that DOES survive a restart is the **turn boundary**. Between turns the agent process has
//! exited, its output is committed, and the run's durable state (claim, workspace, branch, retry row)
//! is all that is left. So a drain means exactly this:
//!
//! * **Claim nothing more.** Dispatch is gated at the same seam the BO-59 credential preflight uses —
//!   before candidate fetch — so a draining daemon never claims a ticket it then abandons.
//! * **Do not interrupt what is running.** The current turn runs to completion.
//! * **Refuse the NEXT turn.** [`crate::worker::WorkerDeps::run_turns`] returns NORMALLY at its
//!   boundary, so the run is classified `continued` and its claim is KEPT — not `interrupted`.
//!
//! # What a drained run loses (and what it does not)
//!
//! It loses the agent's conversation thread, and only that. `--resume` is driven by the session's
//! IN-MEMORY thread id, seeded from the first turn's stream; a re-dispatch builds a fresh
//! [`rhapsody_agent::Session`] whose thread id starts empty, and nothing reads a stored id back into
//! it. (`runs.session_uuid` cannot help: `persist_start_run` leaves that column empty — it is
//! reserved, never written.) Everything durable survives: the worktree, the branch, the commits, the
//! claim and the retry row. The continuation prompt is what re-orients the new session.
//!
//! That is precisely why the turn boundary is the right cut: it is the point at which the agent has
//! just finished a unit of work, so the conversation is the cheapest thing on the table.
//!
//! # What this type is NOT
//!
//! It never kills anything and it owns no budget. Arming a drain has exactly two effects — the
//! dispatch gate closes and the turn loops wind down — and a drain left armed simply stays armed.
//! Deciding how long to WAIT for `counts.running` to reach zero, and what to do when that wait runs
//! out, belongs to whoever asked for the drain (the desktop supervisor's restart path), never here:
//! a budget that expired inside the daemon could only express itself by interrupting the very work
//! the drain exists to protect.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, Ordering};

use chrono::{DateTime, TimeZone, Utc};

/// The operator advisory surfaced on each project's `/api/v1/projects` status while a drain is armed
/// — the state-visible half of the "a paused dispatch must be legible" requirement, mirroring
/// [`crate::preflight::CREDENTIAL_DEAD_WARNING`]. Silence is the failure mode a drain has: a daemon
/// that quietly stops dispatching is indistinguishable from a wedged one.
pub const DRAINING_WARNING: &str =
    "drain requested — dispatch paused; in-flight runs finish their current turn";

/// Who asked for the drain. Carried so the console can say whether an operator asked to settle the
/// daemon or an upgrade is waiting to install, which are the same mechanism with different urgency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DrainReason {
    /// A human asked for it (the tray/console action).
    #[default]
    Operator,
    /// An update is staged and wants a clean restart to install into.
    Update,
}

impl DrainReason {
    /// The wire spelling (`/api/v1/drain`, `/api/v1/state`).
    pub fn as_str(self) -> &'static str {
        match self {
            DrainReason::Operator => "operator",
            DrainReason::Update => "update",
        }
    }

    /// Parses a wire spelling. Anything unrecognized reads as [`DrainReason::Operator`] — the reason
    /// is an annotation on a request that has already been made, so a typo must not be able to
    /// refuse a drain.
    pub fn parse(s: &str) -> DrainReason {
        match s {
            "update" => DrainReason::Update,
            _ => DrainReason::Operator,
        }
    }

    /// The compact repr stored in the signal's atomic.
    fn to_bits(self) -> u8 {
        match self {
            DrainReason::Operator => 0,
            DrainReason::Update => 1,
        }
    }

    /// The inverse of [`Self::to_bits`], total by construction: an unknown byte (only reachable if
    /// this enum grows and an old value is read) answers `Operator` rather than panicking.
    fn from_bits(b: u8) -> DrainReason {
        match b {
            1 => DrainReason::Update,
            _ => DrainReason::Operator,
        }
    }
}

/// A drain's observable state — what `/api/v1/drain` answers and what `/api/v1/state` renders while
/// a drain is armed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrainStatus {
    /// Whether dispatch is currently gated.
    pub active: bool,
    /// When the CURRENT drain was armed; `None` whenever `active` is false. Re-arming an already
    /// armed drain does NOT move it, so an operator watching a drain sees how long it has really
    /// been waiting rather than a clock reset by a repeated request.
    pub requested_at: Option<DateTime<Utc>>,
    /// Who asked. Meaningless (and always [`DrainReason::Operator`]) while `active` is false.
    pub reason: DrainReason,
}

/// The shared drain flag: one `Arc` cell cloned onto the control loop, every in-flight worker, and
/// the off-loop [`crate::stop::ControlHandle`] the HTTP layer drives.
///
/// It is deliberately lock-free rather than routed through the control-event channel. Both readers
/// are on paths that must not queue behind the current tick — the dispatch gate runs ON the tick, and
/// a worker's turn boundary runs on its own task — and the writer is an HTTP handler that must answer
/// immediately even when the control task is mid-reconcile. This is the same `Arc`-shared-atomic
/// idiom `ControlHandle::retention_days` already uses for state both sides touch.
///
/// [`Default`] is "not draining", which is what every construction site that predates the feature
/// gets, and it makes the whole mechanism inert.
#[derive(Clone, Default)]
pub struct DrainSignal {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    /// The gate itself. Written last on arm and first on disarm (see the ordering note on [`arm`]).
    active: AtomicBool,
    /// Epoch milliseconds of the arming instant; `0` means "never armed".
    requested_at_ms: AtomicI64,
    /// [`DrainReason::to_bits`] of the arming request.
    reason: AtomicU8,
}

impl std::fmt::Debug for DrainSignal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DrainSignal")
            .field("status", &self.status())
            .finish()
    }
}

impl DrainSignal {
    /// A fresh, un-armed signal.
    pub fn new() -> DrainSignal {
        DrainSignal::default()
    }

    /// Whether dispatch is gated right now — the one question both the dispatch gate and the turn
    /// boundary ask.
    pub fn is_draining(&self) -> bool {
        self.inner.active.load(Ordering::Acquire)
    }

    /// Arms the drain, returning `true` only if this call is what armed it.
    ///
    /// Idempotent: arming an already-armed drain keeps the ORIGINAL `requested_at` and reason and
    /// answers `false`, so a client that retries (or a second operator pressing the same button)
    /// cannot reset the clock a waiter is measuring against.
    ///
    /// The two annotation fields are stored BEFORE `active` is released, so any thread that observes
    /// `active == true` also observes the annotations that go with it.
    pub fn arm(&self, at: DateTime<Utc>, reason: DrainReason) -> bool {
        if self.inner.active.load(Ordering::Acquire) {
            return false;
        }
        self.inner
            .requested_at_ms
            .store(at.timestamp_millis(), Ordering::Relaxed);
        self.inner
            .reason
            .store(reason.to_bits(), Ordering::Relaxed);
        // `swap` rather than `store`: two concurrent arms must agree on which one won, so exactly one
        // caller is told it armed the drain.
        !self.inner.active.swap(true, Ordering::AcqRel)
    }

    /// Cancels the drain, returning `true` only if this call is what cancelled it. Dispatch resumes
    /// on the next tick.
    ///
    /// Nothing was lost while it was armed: the gate runs BEFORE candidate fetch, so a drained daemon
    /// claimed nothing it has to give back, and the tickets that piled up behind it are simply
    /// candidates again. A run that wound down at a turn boundary kept its claim and sits in the
    /// retry queue, so it re-dispatches on the next pass.
    pub fn disarm(&self) -> bool {
        if !self.inner.active.swap(false, Ordering::AcqRel) {
            return false;
        }
        self.inner.requested_at_ms.store(0, Ordering::Relaxed);
        self.inner
            .reason
            .store(DrainReason::default().to_bits(), Ordering::Relaxed);
        true
    }

    /// The current observable state.
    pub fn status(&self) -> DrainStatus {
        if !self.inner.active.load(Ordering::Acquire) {
            return DrainStatus {
                active: false,
                requested_at: None,
                reason: DrainReason::default(),
            };
        }
        DrainStatus {
            active: true,
            requested_at: from_millis(self.inner.requested_at_ms.load(Ordering::Relaxed)),
            reason: DrainReason::from_bits(self.inner.reason.load(Ordering::Relaxed)),
        }
    }
}

/// Epoch milliseconds back to a timestamp. `0` (never armed) and any value the calendar cannot
/// represent both answer `None` — the field is an annotation, so an unrenderable instant degrades to
/// "no timestamp", never to a panic.
fn from_millis(ms: i64) -> Option<DateTime<Utc>> {
    if ms == 0 {
        return None;
    }
    Utc.timestamp_millis_opt(ms).single()
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn at(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_780_000_000 + secs, 0)
            .single()
            .expect("valid instant")
    }

    // A fresh signal is inert, which is what every pre-drain construction site inherits.
    #[test]
    fn default_is_not_draining() {
        let d = DrainSignal::new();
        assert!(!d.is_draining());
        assert_eq!(
            d.status(),
            DrainStatus {
                active: false,
                requested_at: None,
                reason: DrainReason::Operator,
            }
        );
    }

    // Arming is visible through every clone — the property the whole design rests on, since the loop,
    // the workers and the HTTP handler each hold their own.
    #[test]
    fn arm_is_visible_through_a_clone() {
        let d = DrainSignal::new();
        let worker_copy = d.clone();
        assert!(!worker_copy.is_draining());
        assert!(d.arm(at(0), DrainReason::Update));
        assert!(worker_copy.is_draining());
        assert_eq!(
            worker_copy.status(),
            DrainStatus {
                active: true,
                requested_at: Some(at(0)),
                reason: DrainReason::Update,
            }
        );
    }

    // Re-arming keeps the original instant: a waiter measuring "how long has this drain run" must not
    // be reset by a client that retries its request.
    #[test]
    fn rearming_does_not_move_the_clock_or_the_reason() {
        let d = DrainSignal::new();
        assert!(d.arm(at(0), DrainReason::Update));
        assert!(
            !d.arm(at(600), DrainReason::Operator),
            "the second arm did not arm anything, so it must say so"
        );
        assert_eq!(
            d.status(),
            DrainStatus {
                active: true,
                requested_at: Some(at(0)),
                reason: DrainReason::Update,
            },
            "a re-arm must not reset the clock a waiter is measuring against"
        );
    }

    // Cancelling resumes dispatch and forgets the annotations, and only the call that really
    // cancelled reports having done so.
    #[test]
    fn disarm_resumes_dispatch_exactly_once() {
        let d = DrainSignal::new();
        assert!(!d.disarm(), "nothing was armed, so nothing was cancelled");
        d.arm(at(0), DrainReason::Update);
        assert!(d.disarm());
        assert!(!d.disarm());
        assert!(!d.is_draining());
        assert_eq!(d.status().requested_at, None);
        assert_eq!(d.status().reason, DrainReason::Operator);
    }

    // A cancelled drain can be armed afresh, with its OWN clock (the re-arm guard above must not
    // survive the cancel).
    #[test]
    fn a_cancelled_drain_rearms_with_a_new_clock() {
        let d = DrainSignal::new();
        d.arm(at(0), DrainReason::Operator);
        d.disarm();
        assert!(d.arm(at(900), DrainReason::Update));
        assert_eq!(d.status().requested_at, Some(at(900)));
        assert_eq!(d.status().reason, DrainReason::Update);
    }

    // The wire spellings both ways, including the deliberately total parse: an unrecognized reason
    // must never be able to refuse a drain that was genuinely asked for.
    #[test]
    fn reason_wire_spellings_round_trip_and_parse_is_total() {
        assert_eq!(DrainReason::Operator.as_str(), "operator");
        assert_eq!(DrainReason::Update.as_str(), "update");
        assert_eq!(DrainReason::parse("operator"), DrainReason::Operator);
        assert_eq!(DrainReason::parse("update"), DrainReason::Update);
        assert_eq!(DrainReason::parse("nonsense"), DrainReason::Operator);
        assert_eq!(DrainReason::parse(""), DrainReason::Operator);
    }

    // Exactly one of N concurrent arms reports that it armed the drain, so a caller can key a "drain
    // started" log line (or a one-shot notification) off the return value.
    #[test]
    fn concurrent_arms_elect_exactly_one_winner() {
        let d = DrainSignal::new();
        let winners = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        std::thread::scope(|s| {
            for _ in 0..8 {
                let d = d.clone();
                let winners = std::sync::Arc::clone(&winners);
                s.spawn(move || {
                    if d.arm(at(0), DrainReason::Operator) {
                        winners.fetch_add(1, Ordering::Relaxed);
                    }
                });
            }
        });
        assert_eq!(winners.load(Ordering::Relaxed), 1);
        assert!(d.is_draining());
    }
}
