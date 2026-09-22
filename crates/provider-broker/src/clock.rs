//! Injected monotonic clock.
//!
//! The absolute turn expiry is the earlier of the adapter's turn deadline and the broker's
//! configured maximum capability lifetime, measured with a **monotonic** clock and never extended by
//! traffic (design §4.3). Tests inject [`ManualClock`] so mint/lookup/expiry/finish/drop are
//! deterministic.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// An opaque monotonic instant. Only the injected [`Clock`] produces one; monotonicity is the whole
/// contract, so no wall-clock or epoch meaning is exposed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MonotonicTime(Duration);

impl MonotonicTime {
    /// The origin of the monotonic domain.
    pub const ZERO: Self = MonotonicTime(Duration::ZERO);

    /// Build an instant from whole nanoseconds since the broker's monotonic origin (test seam).
    pub const fn from_nanos(nanos: u64) -> Self {
        MonotonicTime(Duration::from_nanos(nanos))
    }

    /// Nanoseconds since the monotonic origin.
    pub const fn as_nanos(&self) -> u64 {
        self.0.as_nanos() as u64
    }

    /// Saturating addition; a monotonic deadline never wraps.
    pub fn saturating_add(self, delta: Duration) -> Self {
        MonotonicTime(self.0.saturating_add(delta))
    }

    /// The earlier of two instants.
    pub fn min(self, other: Self) -> Self {
        if self.0 <= other.0 { self } else { other }
    }
}

/// A monotonic time source. The broker holds one for its lifetime; tests hold a [`ManualClock`].
pub trait Clock: Send + Sync + 'static {
    /// The current instant in this clock's monotonic domain.
    fn now(&self) -> MonotonicTime;
}

/// The production clock: elapsed time since the broker started, from [`Instant`].
#[derive(Debug)]
pub struct SystemClock {
    epoch: Instant,
}

impl SystemClock {
    /// Start the clock at the moment of construction.
    pub fn new() -> Self {
        Self {
            epoch: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now(&self) -> MonotonicTime {
        MonotonicTime(self.epoch.elapsed())
    }
}

/// A deterministic test clock advanced explicitly by the test.
#[derive(Debug)]
pub struct ManualClock {
    nanos: AtomicU64,
}

impl ManualClock {
    /// A clock starting at the monotonic origin.
    pub fn new() -> Self {
        Self {
            nanos: AtomicU64::new(0),
        }
    }

    /// Advance the clock by `delta`, saturating at [`u64::MAX`].
    pub fn advance(&self, delta: Duration) {
        let add = delta.as_nanos().min(u64::MAX as u128) as u64;
        let _ = self
            .nanos
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                Some(n.saturating_add(add))
            });
    }
}

impl Default for ManualClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for ManualClock {
    fn now(&self) -> MonotonicTime {
        MonotonicTime::from_nanos(self.nanos.load(Ordering::Acquire))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_clock_advances_monotonically() {
        let clock = ManualClock::new();
        assert_eq!(clock.now(), MonotonicTime::ZERO);
        clock.advance(Duration::from_secs(1));
        assert_eq!(clock.now().as_nanos(), 1_000_000_000);
        clock.advance(Duration::from_millis(500));
        assert_eq!(clock.now().as_nanos(), 1_500_000_000);
    }

    #[test]
    fn system_clock_does_not_go_backwards() {
        let clock = SystemClock::new();
        let first = clock.now();
        let second = clock.now();
        assert!(second >= first);
    }

    #[test]
    fn min_returns_earlier_instant() {
        let a = MonotonicTime::from_nanos(10);
        let b = MonotonicTime::from_nanos(20);
        assert_eq!(a.min(b), a);
        assert_eq!(b.min(a), a);
    }
}
