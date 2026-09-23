//! The optional durable cumulative day-budget authority (design §8.1).
//!
//! The finite per-turn limits are always-on, so a turn is bounded even with no operator budget. The
//! same typed limits block may additionally set `max_reserved_token_units_per_utc_day`; when it is
//! present the daemon injects a durable, non-secret [`CumulativeBudgetAuthority`] into the session
//! policy. Every request reservation is then charged atomically under
//! `(stable_provider_id, UTC day)` before egress, so concurrent runs and daemon restarts cannot
//! oversubscribe the day cap.
//!
//! This crate owns only the contract and the per-admission call. A later slice (the spend-budget
//! slice's durable UTC-day store) supplies the implementation; the broker has no store dependency
//! and never reads or writes the authority's backing state itself.

use std::fmt;

/// A UTC calendar day: whole days since the Unix epoch (1970-01-01). It is the authority's bucket
/// key, so a charge made before midnight does not count against the next day.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UtcDay(i64);

impl UtcDay {
    /// Build a day from whole days since the Unix epoch.
    pub const fn from_days_since_epoch(days: i64) -> Self {
        UtcDay(days)
    }

    /// Whole days since the Unix epoch.
    pub const fn days_since_epoch(self) -> i64 {
        self.0
    }
}

/// Why a durable day-budget charge was refused. Both reasons fail closed: nothing is charged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DayBudgetRefusal {
    /// Charging the reservation would exceed the configured UTC-day cap.
    Exhausted,
    /// The durable authority could not be reached. A configured day cap is a hard boundary, never a
    /// best-effort one, so an unreachable store refuses the request rather than admitting it.
    Unavailable,
}

/// The daemon-injected durable, non-secret cumulative day-budget authority (design §8.1).
///
/// An implementation must charge atomically under `(stable_provider_id, current UTC day)`: two
/// concurrent runs, or a restart that re-reads persisted state, must never be able to oversubscribe
/// the configured cap. The broker calls only [`CumulativeBudgetAuthority::try_charge`] on the
/// admission path and [`CumulativeBudgetAuthority::charged_today`] for the pre-spawn refusal; it
/// holds no store dependency of its own and never releases a generic-adapter charge based on a
/// provider report.
pub trait CumulativeBudgetAuthority: Send + Sync + fmt::Debug {
    /// Atomically charge `tokens` under `(provider_id, today)` against `cap`. Must charge nothing
    /// and return [`DayBudgetRefusal::Exhausted`] when `cap` would be exceeded.
    fn try_charge(&self, provider_id: &str, tokens: u64, cap: u64) -> Result<(), DayBudgetRefusal>;

    /// The tokens already charged under `(provider_id, today)`.
    fn charged_today(&self, provider_id: &str) -> u64;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_day_round_trips_days_since_epoch() {
        let day = UtcDay::from_days_since_epoch(20_600);
        assert_eq!(day.days_since_epoch(), 20_600);
        assert!(UtcDay::from_days_since_epoch(1) < UtcDay::from_days_since_epoch(2));
    }
}
