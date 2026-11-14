//! Off-loop recall diagnostics. Short bookkeeping locks never span backend I/O.
use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rhapsody_config::memory::{BackendFailure, MemoryError, Recalled};
use serde::Serialize;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct BankHealth {
    pub recalls: u64,
    pub failures: u64,
    pub p50_ms: u64,
    pub p95_ms: u64,
    pub samples: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct MemoryHealthView {
    pub decided_without_memory: u64,
    pub banks: BTreeMap<String, BankHealth>,
}

#[derive(Default)]
struct State {
    view: MemoryHealthView,
    latencies: BTreeMap<String, VecDeque<u64>>,
}

#[derive(Default)]
pub struct MemoryHealth(Mutex<State>);

impl MemoryHealth {
    pub fn view(&self) -> MemoryHealthView {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .view
            .clone()
    }

    /// Count once per decision context with any required bank unavailable, not per retry/bank.
    pub fn without_memory(&self) {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.view.decided_without_memory = state.view.decided_without_memory.saturating_add(1);
        tracing::warn!(
            decided_without_memory = state.view.decided_without_memory,
            "Manager decided without memory"
        );
    }

    fn record(&self, bank: &str, elapsed_ms: u64, result: &Result<Recalled, MemoryError>) {
        let outcome = result
            .as_ref()
            .err()
            .map_or_else(|| "ok".into(), MemoryError::reason);
        let result_count = result.as_ref().map_or(0, |r| r.facts.len());
        tracing::info!(
            bank,
            elapsed_ms,
            outcome,
            result_count,
            "memory recall finished"
        );
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let samples = state.latencies.entry(bank.into()).or_default();
        if samples.len() == 256 {
            samples.pop_front();
        }
        samples.push_back(elapsed_ms);
        let mut sorted: Vec<_> = samples.iter().copied().collect();
        sorted.sort_unstable();
        let health = state.view.banks.entry(bank.into()).or_default();
        health.recalls = health.recalls.saturating_add(1);
        health.failures = health.failures.saturating_add(u64::from(result.is_err()));
        health.samples = sorted.len();
        health.p50_ms = sorted[(sorted.len() * 50).div_ceil(100) - 1];
        health.p95_ms = sorted[(sorted.len() * 95).div_ceil(100) - 1];
    }

    /// Two independent attempts inside the original whole-prefetch budget. A timeout on the
    /// first attempt leaves room for a retry; each bank owns its deadline and retains its result.
    pub async fn recall<F, Fut>(
        &self,
        bank: &str,
        budget: Duration,
        mut call: F,
    ) -> Result<Recalled, MemoryError>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<Recalled, MemoryError>>,
    {
        let start = tokio::time::Instant::now();
        let deadline = start + budget;
        let jitter_cap = Duration::from_millis(250).min(budget / 10);
        let attempt_budget = (budget - jitter_cap) / 2;
        for attempt in 0..2 {
            let started = tokio::time::Instant::now();
            let until = (started + attempt_budget).min(deadline);
            let result = tokio::time::timeout_at(until, call())
                .await
                .unwrap_or_else(|_| {
                    Err(MemoryError::Backend {
                        reason: BackendFailure::Timeout,
                        detail: "recall prefetch deadline".into(),
                    })
                });
            self.record(
                bank,
                u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                &result,
            );
            let retry = matches!(
                &result,
                Err(MemoryError::Backend {
                    reason: BackendFailure::Timeout | BackendFailure::Http(500..=599),
                    ..
                })
            );
            if attempt == 1 || !retry {
                return result;
            }
            // No shared RNG/lock: independently jitter concurrent bank retries within the cap.
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .subsec_nanos();
            let jitter = jitter_cap.mul_f64(0.2 + 0.8 * f64::from(nanos % 1000) / 1000.0);
            tokio::time::sleep_until((tokio::time::Instant::now() + jitter).min(deadline)).await;
        }
        Err(MemoryError::Backend {
            reason: BackendFailure::Timeout,
            detail: "recall budget exhausted".into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_use_nearest_rank_and_a_bounded_window() {
        let health = MemoryHealth::default();
        for elapsed in 1..=100 {
            health.record("team", elapsed, &Ok(Recalled::default()));
        }
        let view = health.view();
        assert_eq!(view.banks["team"].p50_ms, 50);
        assert_eq!(view.banks["team"].p95_ms, 95);
        for elapsed in 101..=356 {
            health.record("team", elapsed, &Ok(Recalled::default()));
        }
        let view = health.view();
        assert_eq!(view.banks["team"].recalls, 356);
        assert_eq!(view.banks["team"].samples, 256);
        assert_eq!(view.banks["team"].p50_ms, 228);
        assert_eq!(view.banks["team"].p95_ms, 344);
    }

    #[tokio::test]
    async fn permanent_errors_are_not_retried() {
        let health = MemoryHealth::default();
        let result = health
            .recall("team", Duration::from_millis(100), || async {
                Err(MemoryError::Backend {
                    reason: BackendFailure::Http(401),
                    detail: "private".into(),
                })
            })
            .await;
        assert_eq!(result.unwrap_err().reason(), "http 401");
        assert_eq!(health.view().banks["team"].recalls, 1);
    }
}
