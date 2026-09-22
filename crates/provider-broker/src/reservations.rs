//! Atomic reservation primitives.
//!
//! One admission transaction reserves a forwarded-request slot, aggregate request bytes, per-request
//! response bytes, reserved token units against the turn cap, and token units against the
//! session/run cap — or it reserves nothing (design §8.2, §4.2). Every counter is checked and every
//! arithmetic operation is checked; the transaction runs under one mutex, so concurrent attempts
//! under a barrier cannot oversubscribe. Concurrency permits are the one releasable reservation.
//!
//! The same mutex also carries the turn's *liveness gate*. [`Reservations::close_and_snapshot`] is
//! the finalization transition: it sets `closed` and returns the counters in one critical section,
//! and every admission (check + commit) runs under that same lock. A revocation therefore cannot
//! publish a ledger between an admission that passed its liveness check and the reservation it
//! commits — the committed work is either snapshotted into the ledger, or the admission is refused
//! with `Unauthorized` because the turn already closed (design §4.3, §8.2).
//!
//! V1 never releases a *token* reservation based on reported usage; later slices parse and settle,
//! but the turn aggregates stay conservative.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::error::BrokerError;
use crate::policy::BrokerLimits;
use crate::state::lock;

/// The per-session/run reserved-token cap, shared across that session's turns. Token reservations
/// are charged atomically and never released by the generic adapter.
#[derive(Debug)]
pub(crate) struct SessionReservations {
    max_tokens: u64,
    reserved: AtomicU64,
}

impl SessionReservations {
    pub(crate) fn new(max_tokens: u64) -> Self {
        Self {
            max_tokens,
            reserved: AtomicU64::new(0),
        }
    }

    pub(crate) fn try_reserve(&self, tokens: u64) -> Result<(), BrokerError> {
        self.reserved
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current
                    .checked_add(tokens)
                    .filter(|next| *next <= self.max_tokens)
            })
            .map(|_| ())
            .map_err(|_| BrokerError::SessionBudgetExhausted)
    }

    pub(crate) fn reserved(&self) -> u64 {
        self.reserved.load(Ordering::Acquire)
    }

    pub(crate) fn remaining(&self) -> u64 {
        self.max_tokens.saturating_sub(self.reserved())
    }
}

/// What one admission transaction wants to reserve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ReserveRequest {
    pub request_bytes: u64,
    pub response_bytes: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ReservationSnapshot {
    pub forwarded_requests: u64,
    pub denied_requests: u64,
    pub concurrent_requests: u64,
    pub request_bytes: u64,
    pub response_bytes: u64,
    pub reserved_tokens: u64,
}

/// The counters plus the turn's liveness gate, both behind one mutex so an admission and a
/// finalization can never interleave between the check and the commit.
#[derive(Debug, Default)]
struct ReservationState {
    closed: bool,
    counters: ReservationSnapshot,
}

/// The grant's per-turn reservation counters. Held behind an `Arc` so a [`ConcurrencyPermit`] can
/// release its slot when it drops.
#[derive(Debug, Clone)]
pub(crate) struct Reservations {
    limits: BrokerLimits,
    state: Arc<Mutex<ReservationState>>,
}

impl Reservations {
    pub(crate) fn new(limits: BrokerLimits) -> Self {
        Self {
            limits,
            state: Arc::new(Mutex::new(ReservationState::default())),
        }
    }

    /// Atomically reserve one forwarded request. Returns `Err` and reserves nothing if the turn has
    /// closed, the `live` check refuses, or any limit would be exceeded. The `live` check runs under
    /// the same lock as the commit, so a concurrent finalization cannot snapshot between them.
    pub(crate) fn try_reserve(
        &self,
        session: &SessionReservations,
        request: ReserveRequest,
        live: impl FnOnce() -> Result<(), BrokerError>,
    ) -> Result<(), BrokerError> {
        let limits = &self.limits;
        let mut guard = lock(&self.state);
        if guard.closed {
            return Err(BrokerError::Unauthorized);
        }
        live()?;
        let state = &mut guard.counters;

        if request.request_bytes > limits.max_request_bytes {
            return Err(BrokerError::TurnBudgetExhausted("max_request_bytes"));
        }
        if request.response_bytes > limits.max_response_bytes {
            return Err(BrokerError::TurnBudgetExhausted("max_response_bytes"));
        }
        if request.output_tokens > limits.max_output_tokens_request {
            return Err(BrokerError::TurnBudgetExhausted(
                "max_output_tokens_request",
            ));
        }

        let forwarded = state
            .forwarded_requests
            .checked_add(1)
            .ok_or(BrokerError::TurnBudgetExhausted("max_forwarded_requests"))?;
        if forwarded > u64::from(limits.max_forwarded_requests) {
            return Err(BrokerError::TurnBudgetExhausted("max_forwarded_requests"));
        }

        let request_bytes = state
            .request_bytes
            .checked_add(request.request_bytes)
            .ok_or(BrokerError::TurnBudgetExhausted("max_request_bytes_turn"))?;
        if request_bytes > limits.max_request_bytes_turn {
            return Err(BrokerError::TurnBudgetExhausted("max_request_bytes_turn"));
        }

        let response_bytes = state
            .response_bytes
            .checked_add(request.response_bytes)
            .ok_or(BrokerError::TurnBudgetExhausted("max_response_bytes_turn"))?;
        if response_bytes > limits.max_response_bytes_turn {
            return Err(BrokerError::TurnBudgetExhausted("max_response_bytes_turn"));
        }

        let token_cost = request
            .request_bytes
            .checked_add(request.output_tokens)
            .ok_or(BrokerError::TurnBudgetExhausted("max_reserved_tokens_turn"))?;
        let turn_tokens = state
            .reserved_tokens
            .checked_add(token_cost)
            .ok_or(BrokerError::TurnBudgetExhausted("max_reserved_tokens_turn"))?;
        if turn_tokens > limits.max_reserved_tokens_turn {
            return Err(BrokerError::TurnBudgetExhausted("max_reserved_tokens_turn"));
        }

        // Charge the session/run cap atomically only after every turn check passed; if it refuses,
        // the turn counters are left untouched.
        session.try_reserve(token_cost)?;

        state.forwarded_requests = forwarded;
        state.request_bytes = request_bytes;
        state.response_bytes = response_bytes;
        state.reserved_tokens = turn_tokens;
        Ok(())
    }

    /// Acquire one of the turn's concurrent-request permits. The permit releases the slot on drop.
    /// The `live` check and the commit share one critical section with finalization.
    pub(crate) fn try_acquire_concurrency(
        &self,
        live: impl FnOnce() -> Result<(), BrokerError>,
    ) -> Result<ConcurrencyPermit, BrokerError> {
        let mut guard = lock(&self.state);
        if guard.closed {
            return Err(BrokerError::Unauthorized);
        }
        live()?;
        let next = guard
            .counters
            .concurrent_requests
            .checked_add(1)
            .ok_or(BrokerError::TurnBudgetExhausted("max_concurrent_requests"))?;
        if next > u64::from(self.limits.max_concurrent_requests) {
            return Err(BrokerError::TurnBudgetExhausted("max_concurrent_requests"));
        }
        guard.counters.concurrent_requests = next;
        Ok(ConcurrencyPermit {
            state: Arc::clone(&self.state),
        })
    }

    /// Count one locally denied authenticated request. *Reaching* the configured threshold refuses
    /// the request and returns an error so the caller can revoke the turn (design §8.2); the
    /// threshold denial is still counted, and further denials keep returning the same error. The
    /// `live` check and the commit share one critical section with finalization.
    pub(crate) fn record_denied(
        &self,
        live: impl FnOnce() -> Result<(), BrokerError>,
    ) -> Result<(), BrokerError> {
        let mut guard = lock(&self.state);
        if guard.closed {
            return Err(BrokerError::Unauthorized);
        }
        live()?;
        let next = guard
            .counters
            .denied_requests
            .checked_add(1)
            .ok_or(BrokerError::TurnBudgetExhausted("max_denied_requests"))?;
        if next > u64::from(self.limits.max_denied_requests) {
            return Err(BrokerError::TurnBudgetExhausted("max_denied_requests"));
        }
        guard.counters.denied_requests = next;
        if next >= u64::from(self.limits.max_denied_requests) {
            return Err(BrokerError::TurnBudgetExhausted("max_denied_requests"));
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn snapshot(&self) -> ReservationSnapshot {
        lock(&self.state).counters
    }

    /// Finalize admission: set the closed gate and return the committed counters in one critical
    /// section. Called exactly once, from [`TurnInner::finalize`](crate::state::TurnInner). Any
    /// admission under way either commits before this and is included, or is refused because the
    /// turn is already closed.
    pub(crate) fn close_and_snapshot(&self) -> ReservationSnapshot {
        let mut guard = lock(&self.state);
        guard.closed = true;
        guard.counters
    }
}

/// A hold on one concurrent-request slot. Dropping it releases the slot.
#[derive(Debug)]
pub struct ConcurrencyPermit {
    state: Arc<Mutex<ReservationState>>,
}

impl Drop for ConcurrencyPermit {
    fn drop(&mut self) {
        let mut guard = lock(&self.state);
        guard.counters.concurrent_requests = guard.counters.concurrent_requests.saturating_sub(1);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Barrier;
    use std::thread;

    use super::*;

    fn limits() -> BrokerLimits {
        BrokerLimits {
            max_forwarded_requests: 64,
            max_concurrent_requests: 4,
            max_request_bytes: 100_000,
            max_request_bytes_turn: 1_000_000,
            max_response_bytes: 100_000,
            max_response_bytes_turn: 1_000_000,
            max_output_tokens_request: 100_000,
            max_reserved_tokens_turn: 1_000_000,
            max_reserved_tokens_session: 5_000_000,
            ..BrokerLimits::default()
        }
    }

    #[test]
    fn session_reservation_refuses_oversubscription() {
        let session = SessionReservations::new(25);
        assert_eq!(session.try_reserve(10), Ok(()));
        assert_eq!(session.try_reserve(10), Ok(()));
        assert_eq!(
            session.try_reserve(10),
            Err(BrokerError::SessionBudgetExhausted)
        );
        assert_eq!(session.reserved(), 20);
        assert_eq!(session.remaining(), 5);
    }

    #[test]
    fn concurrent_forwarded_reservations_never_oversubscribe() {
        let reservations = Reservations::new(limits());
        let session = Arc::new(SessionReservations::new(50_000_000));
        let threads = 100;
        let barrier = Arc::new(Barrier::new(threads));
        let successes = Arc::new(AtomicU64::new(0));

        let mut handles = Vec::new();
        for _ in 0..threads {
            let reservations = reservations.clone();
            let session = Arc::clone(&session);
            let barrier = Arc::clone(&barrier);
            let successes = Arc::clone(&successes);
            handles.push(thread::spawn(move || {
                barrier.wait();
                let request = ReserveRequest {
                    request_bytes: 10_000,
                    response_bytes: 10_000,
                    output_tokens: 0,
                };
                if reservations
                    .try_reserve(&session, request, || Ok(()))
                    .is_ok()
                {
                    successes.fetch_add(1, Ordering::AcqRel);
                }
            }));
        }
        for handle in handles {
            handle.join().expect("thread");
        }

        // `max_forwarded_requests` is the binding constraint (64 < token/byte budget).
        assert_eq!(successes.load(Ordering::Acquire), 64);
        let snapshot = reservations.snapshot();
        assert_eq!(snapshot.forwarded_requests, 64);
        assert!(snapshot.forwarded_requests <= u64::from(limits().max_forwarded_requests));
        assert!(snapshot.reserved_tokens <= limits().max_reserved_tokens_turn);
        assert!(snapshot.request_bytes <= limits().max_request_bytes_turn);
    }

    #[test]
    fn concurrent_permits_never_exceed_the_concurrency_limit() {
        let reservations = Reservations::new(limits());
        let threads = 16;
        let barrier = Arc::new(Barrier::new(threads));
        let permits = Arc::new(Mutex::new(Vec::new()));

        let mut handles = Vec::new();
        for _ in 0..threads {
            let reservations = reservations.clone();
            let barrier = Arc::clone(&barrier);
            let permits = Arc::clone(&permits);
            handles.push(thread::spawn(move || {
                barrier.wait();
                if let Ok(permit) = reservations.try_acquire_concurrency(|| Ok(())) {
                    lock(&permits).push(permit);
                }
            }));
        }
        for handle in handles {
            handle.join().expect("thread");
        }

        assert_eq!(lock(&permits).len(), 4);
        let mut held = lock(&permits);
        drop(held.pop());
        drop(held);
        assert!(reservations.try_acquire_concurrency(|| Ok(())).is_ok());
    }

    #[test]
    fn an_exhausted_turn_limit_reserves_nothing() {
        let reservations = Reservations::new(BrokerLimits {
            max_reserved_tokens_turn: 1_000,
            max_reserved_tokens_session: 1_000,
            ..limits()
        });
        let session = SessionReservations::new(1_000);
        let big = ReserveRequest {
            request_bytes: 600,
            response_bytes: 0,
            output_tokens: 500,
        };
        assert_eq!(
            reservations.try_reserve(&session, big, || Ok(())),
            Err(BrokerError::TurnBudgetExhausted("max_reserved_tokens_turn"))
        );
        assert_eq!(reservations.snapshot().forwarded_requests, 0);
        assert_eq!(session.reserved(), 0);
    }

    #[test]
    fn reaching_the_denial_threshold_refuses_and_counts_the_threshold_denial() {
        let reservations = Reservations::new(BrokerLimits {
            max_denied_requests: 3,
            ..limits()
        });
        assert_eq!(reservations.record_denied(|| Ok(())), Ok(()));
        assert_eq!(reservations.record_denied(|| Ok(())), Ok(()));
        // The third denial *reaches* the threshold: it is counted, and it refuses so the caller
        // can revoke the turn (design §8.2).
        assert_eq!(
            reservations.record_denied(|| Ok(())),
            Err(BrokerError::TurnBudgetExhausted("max_denied_requests"))
        );
        assert_eq!(reservations.snapshot().denied_requests, 3);
        // Further denials keep refusing without incrementing past the cap.
        assert_eq!(
            reservations.record_denied(|| Ok(())),
            Err(BrokerError::TurnBudgetExhausted("max_denied_requests"))
        );
        assert_eq!(reservations.snapshot().denied_requests, 3);
    }

    #[test]
    fn finalization_cannot_snapshot_between_an_admissions_check_and_its_commit() {
        use std::sync::mpsc;
        use std::time::Duration;

        let reservations = Arc::new(Reservations::new(limits()));
        let session = Arc::new(SessionReservations::new(5_000_000));
        let (checked_tx, checked_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();

        // An admission whose liveness check blocks after it passes, while still holding the lock.
        let admitting = {
            let reservations = Arc::clone(&reservations);
            let session = Arc::clone(&session);
            thread::spawn(move || {
                reservations.try_reserve(
                    &session,
                    ReserveRequest {
                        request_bytes: 10,
                        response_bytes: 0,
                        output_tokens: 0,
                    },
                    || {
                        checked_tx.send(()).expect("signal the check");
                        release_rx.recv().expect("wait for finalization attempt");
                        Ok(())
                    },
                )
            })
        };
        checked_rx.recv().expect("the admission passed its check");

        // A concurrent finalization must not observe the ledger until the admission commits.
        let finalizing = {
            let reservations = Arc::clone(&reservations);
            thread::spawn(move || reservations.close_and_snapshot())
        };
        thread::sleep(Duration::from_millis(50));
        assert!(
            !finalizing.is_finished(),
            "finalization must not snapshot while an admission holds the gate"
        );

        release_tx.send(()).expect("release the admission");
        assert!(
            admitting.join().expect("admission thread").is_ok(),
            "the admission commits"
        );
        let snapshot = finalizing.join().expect("finalization thread");
        assert_eq!(
            snapshot.forwarded_requests, 1,
            "the committed reservation is in the finalized ledger"
        );

        // Once closed, no further admission commits even with a passing liveness check.
        assert_eq!(
            reservations
                .try_reserve(
                    &session,
                    ReserveRequest {
                        request_bytes: 10,
                        response_bytes: 0,
                        output_tokens: 0,
                    },
                    || Ok(()),
                )
                .unwrap_err(),
            BrokerError::Unauthorized
        );
        assert_eq!(reservations.snapshot().forwarded_requests, 1);
    }

    #[test]
    fn finalization_cannot_snapshot_between_a_concurrency_acquisitions_check_and_its_commit() {
        use std::sync::mpsc;
        use std::time::Duration;

        let reservations = Arc::new(Reservations::new(limits()));
        let (checked_tx, checked_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();

        // A permit acquisition whose liveness check blocks after it passes, while still holding the
        // lock.
        let acquiring = {
            let reservations = Arc::clone(&reservations);
            thread::spawn(move || {
                reservations.try_acquire_concurrency(|| {
                    checked_tx.send(()).expect("signal the check");
                    release_rx.recv().expect("wait for finalization attempt");
                    Ok(())
                })
            })
        };
        checked_rx.recv().expect("the acquisition passed its check");

        // A concurrent finalization must not observe the ledger until the admission commits.
        let finalizing = {
            let reservations = Arc::clone(&reservations);
            thread::spawn(move || reservations.close_and_snapshot())
        };
        thread::sleep(Duration::from_millis(50));
        assert!(
            !finalizing.is_finished(),
            "finalization must not snapshot while an acquisition holds the gate"
        );

        release_tx.send(()).expect("release the acquisition");
        let permit = acquiring
            .join()
            .expect("acquisition thread")
            .expect("the permit commits");
        let snapshot = finalizing.join().expect("finalization thread");
        assert_eq!(
            snapshot.concurrent_requests, 1,
            "the committed permit is in the finalized ledger"
        );

        // Once closed, no further acquisition commits even with a passing liveness check.
        assert_eq!(
            reservations.try_acquire_concurrency(|| Ok(())).unwrap_err(),
            BrokerError::Unauthorized
        );
        assert_eq!(reservations.snapshot().concurrent_requests, 1);
        drop(permit);
        assert_eq!(reservations.snapshot().concurrent_requests, 0);
    }

    #[test]
    fn finalization_cannot_snapshot_between_a_denials_check_and_its_commit() {
        use std::sync::mpsc;
        use std::time::Duration;

        let reservations = Arc::new(Reservations::new(limits()));
        let (checked_tx, checked_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();

        // A denial whose liveness check blocks after it passes, while still holding the lock.
        let recording = {
            let reservations = Arc::clone(&reservations);
            thread::spawn(move || {
                reservations.record_denied(|| {
                    checked_tx.send(()).expect("signal the check");
                    release_rx.recv().expect("wait for finalization attempt");
                    Ok(())
                })
            })
        };
        checked_rx.recv().expect("the denial passed its check");

        // A concurrent finalization must not observe the ledger until the denial commits.
        let finalizing = {
            let reservations = Arc::clone(&reservations);
            thread::spawn(move || reservations.close_and_snapshot())
        };
        thread::sleep(Duration::from_millis(50));
        assert!(
            !finalizing.is_finished(),
            "finalization must not snapshot while a denial holds the gate"
        );

        release_tx.send(()).expect("release the denial");
        recording
            .join()
            .expect("denial thread")
            .expect("the denial commits");
        let snapshot = finalizing.join().expect("finalization thread");
        assert_eq!(
            snapshot.denied_requests, 1,
            "the committed denial is in the finalized ledger"
        );

        // Once closed, no further denial commits even with a passing liveness check.
        assert_eq!(
            reservations.record_denied(|| Ok(())).unwrap_err(),
            BrokerError::Unauthorized
        );
        assert_eq!(reservations.snapshot().denied_requests, 1);
    }
}
