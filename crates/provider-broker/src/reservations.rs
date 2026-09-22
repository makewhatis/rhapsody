//! Atomic reservation primitives.
//!
//! One admission transaction reserves a forwarded-request slot, aggregate request bytes, per-request
//! response bytes, reserved token units against the turn cap, and token units against the
//! session/run cap — or it reserves nothing (design §8.2, §4.2). Every counter is checked and every
//! arithmetic operation is checked; the transaction runs under one mutex, so concurrent attempts
//! under a barrier cannot oversubscribe. Concurrency permits are the one releasable reservation.
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

/// The grant's per-turn reservation counters. Held behind an `Arc` so a [`ConcurrencyPermit`] can
/// release its slot when it drops.
#[derive(Debug, Clone)]
pub(crate) struct Reservations {
    limits: BrokerLimits,
    counters: Arc<Mutex<ReservationSnapshot>>,
}

impl Reservations {
    pub(crate) fn new(limits: BrokerLimits) -> Self {
        Self {
            limits,
            counters: Arc::new(Mutex::new(ReservationSnapshot::default())),
        }
    }

    /// Atomically reserve one forwarded request. Returns `Err` and reserves nothing if any limit
    /// would be exceeded.
    pub(crate) fn try_reserve(
        &self,
        session: &SessionReservations,
        request: ReserveRequest,
    ) -> Result<(), BrokerError> {
        let limits = &self.limits;
        let mut state = lock(&self.counters);

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
    pub(crate) fn try_acquire_concurrency(&self) -> Result<ConcurrencyPermit, BrokerError> {
        let mut state = lock(&self.counters);
        let next = state
            .concurrent_requests
            .checked_add(1)
            .ok_or(BrokerError::TurnBudgetExhausted("max_concurrent_requests"))?;
        if next > u64::from(self.limits.max_concurrent_requests) {
            return Err(BrokerError::TurnBudgetExhausted("max_concurrent_requests"));
        }
        state.concurrent_requests = next;
        Ok(ConcurrencyPermit {
            counters: Arc::clone(&self.counters),
        })
    }

    /// Count one locally denied authenticated request. Reaching the configured threshold refuses
    /// further denials so the caller can revoke the turn.
    pub(crate) fn record_denied(&self) -> Result<(), BrokerError> {
        let mut state = lock(&self.counters);
        let next = state
            .denied_requests
            .checked_add(1)
            .ok_or(BrokerError::TurnBudgetExhausted("max_denied_requests"))?;
        if next > u64::from(self.limits.max_denied_requests) {
            return Err(BrokerError::TurnBudgetExhausted("max_denied_requests"));
        }
        state.denied_requests = next;
        Ok(())
    }

    pub(crate) fn snapshot(&self) -> ReservationSnapshot {
        *lock(&self.counters)
    }
}

/// A hold on one concurrent-request slot. Dropping it releases the slot.
#[derive(Debug)]
pub struct ConcurrencyPermit {
    counters: Arc<Mutex<ReservationSnapshot>>,
}

impl Drop for ConcurrencyPermit {
    fn drop(&mut self) {
        let mut state = lock(&self.counters);
        state.concurrent_requests = state.concurrent_requests.saturating_sub(1);
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
                if reservations.try_reserve(&session, request).is_ok() {
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
                if let Ok(permit) = reservations.try_acquire_concurrency() {
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
        assert!(reservations.try_acquire_concurrency().is_ok());
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
            reservations.try_reserve(&session, big),
            Err(BrokerError::TurnBudgetExhausted("max_reserved_tokens_turn"))
        );
        assert_eq!(reservations.snapshot().forwarded_requests, 0);
        assert_eq!(session.reserved(), 0);
    }
}
