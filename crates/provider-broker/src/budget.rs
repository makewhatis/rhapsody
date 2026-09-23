//! Broker-wide weighted memory budgets (design §5.1, §7.1).
//!
//! Per-grant byte ceilings alone would let many independent valid grants multiply into unbounded
//! daemon memory. Two compile-time broker-wide budgets bound that: request working memory and
//! buffered non-streaming responses. Each is a weighted sum — a request with a valid bounded
//! `Content-Length` is charged `3 * length + 4 MiB`, a missing/chunked length is charged using the
//! grant's full request-byte maximum, and a buffered non-streaming response is charged
//! `3 * max_response_bytes + 2 MiB`. A checked-arithmetic overflow or an impossible reservation
//! fails closed.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Broker-wide request working-memory budget: 256 MiB.
pub const REQUEST_MEMORY_BUDGET: u64 = 256 * 1024 * 1024;
/// Broker-wide buffered non-streaming response budget: 256 MiB.
pub const BUFFERED_RESPONSE_BUDGET: u64 = 256 * 1024 * 1024;
/// Fixed working-memory overhead added to a weighted request charge.
pub const REQUEST_MEMORY_OVERHEAD: u64 = 4 * 1024 * 1024;
/// Fixed buffered-response overhead added to a weighted response charge.
pub const BUFFERED_RESPONSE_OVERHEAD: u64 = 2 * 1024 * 1024;

/// A broker-wide budget. Cloneable as an `Arc` handle; held by the listener for its lifetime.
#[derive(Debug)]
pub struct WeightedBudget {
    max: u64,
    used: AtomicU64,
}

impl WeightedBudget {
    /// A fresh budget with a compile-time ceiling.
    pub fn new(max: u64) -> Arc<Self> {
        Arc::new(Self {
            max,
            used: AtomicU64::new(0),
        })
    }

    /// The currently reserved weight.
    pub fn used(&self) -> u64 {
        self.used.load(Ordering::Acquire)
    }

    /// The ceiling.
    pub fn max(&self) -> u64 {
        self.max
    }

    /// Reserve `weight`, returning a guard that releases it on drop. `None` when the budget would be
    /// exceeded or the arithmetic would overflow (fails closed).
    pub fn try_acquire(self: &Arc<Self>, weight: u64) -> Option<WeightedGuard> {
        self.used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_add(weight).filter(|next| *next <= self.max)
            })
            .ok()
            .map(|_| WeightedGuard {
                budget: Arc::clone(self),
                weight,
            })
    }
}

/// A held reservation; releases its weight on drop.
#[derive(Debug)]
pub struct WeightedGuard {
    budget: Arc<WeightedBudget>,
    weight: u64,
}

impl Drop for WeightedGuard {
    fn drop(&mut self) {
        self.budget
            .used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                Some(current.saturating_sub(self.weight))
            })
            .ok();
    }
}

/// The weighted request charge for a declared `Content-Length` (design §5.2).
pub fn request_weight(declared_length: Option<u64>, grant_max_request_bytes: u64) -> Option<u64> {
    match declared_length {
        Some(length) => length.checked_mul(3)?.checked_add(REQUEST_MEMORY_OVERHEAD),
        // Missing/chunked length is charged at the grant's full request-byte maximum.
        None => grant_max_request_bytes
            .checked_mul(3)?
            .checked_add(REQUEST_MEMORY_OVERHEAD),
    }
}

/// The weighted charge for one buffered non-streaming response (design §7.1).
pub fn buffered_response_weight(max_response_bytes: u64) -> Option<u64> {
    max_response_bytes
        .checked_mul(3)?
        .checked_add(BUFFERED_RESPONSE_OVERHEAD)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_budget_refuses_oversubscription_and_releases_on_drop() {
        let budget = WeightedBudget::new(100);
        let first = budget.try_acquire(60).expect("first");
        assert!(budget.try_acquire(50).is_none());
        assert_eq!(budget.used(), 60);
        drop(first);
        assert_eq!(budget.used(), 0);
        assert!(budget.try_acquire(100).is_some());
    }

    #[test]
    fn overflow_fails_closed() {
        let budget = WeightedBudget::new(u64::MAX);
        let held = budget.try_acquire(u64::MAX).expect("full reservation");
        assert!(budget.try_acquire(1).is_none());
        drop(held);
        assert!(request_weight(Some(u64::MAX), 0).is_none());
    }

    #[test]
    fn request_weight_uses_declared_length_or_grant_max() {
        assert_eq!(
            request_weight(Some(10), 1_000),
            Some(30 + REQUEST_MEMORY_OVERHEAD)
        );
        assert_eq!(
            request_weight(None, 1_000),
            Some(3_000 + REQUEST_MEMORY_OVERHEAD)
        );
    }
}
