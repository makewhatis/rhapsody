//! backoff — parity port of Go `internal/orchestrator/backoff.go`.
//!
//! The retry-cadence constants + the exponential failure backoff the retry queue schedules against
//! (upstream §8.4): a short fixed [`CONTINUATION_DELAY_MS`] for a clean worker exit that wants more
//! turns, and [`failure_backoff_ms`] — `min(10000 * 2^(attempt-1), max_backoff_ms)`, overflow-safe —
//! for a genuine failure.

/// The short fixed delay before a continuation retry after a clean worker exit (upstream §8.4).
/// Mirrors Go `ContinuationDelayMS`.
pub const CONTINUATION_DELAY_MS: i64 = 1000;

/// The base of the exponential failure backoff (10s). Mirrors Go `baseBackoffMS`.
const BASE_BACKOFF_MS: i64 = 10000;

/// Returns `min(10000 * 2^(attempt-1), max_backoff_ms)`, overflow-safe (upstream §8.4). `attempt < 1`
/// is treated as `1`. Mirrors Go `FailureBackoffMS`: the `shift < 31` bound plus the `d > 0` guard
/// keep the doubled base from wrapping past the cap for a large attempt (attempt 50 → cap, no
/// overflow).
pub fn failure_backoff_ms(attempt: i64, max_backoff_ms: i64) -> i64 {
    let attempt = if attempt < 1 { 1 } else { attempt };
    let mut delay = max_backoff_ms;
    let shift = attempt - 1;
    if shift < 31 {
        // shift is in [0, 30] here, so `BASE_BACKOFF_MS << shift` is at most `10000 << 30`
        // (~1.07e13), well within `i64` and positive — no overflow, mirroring Go's `d > 0` guard.
        let d = BASE_BACKOFF_MS << shift;
        if d > 0 && d < delay {
            delay = d;
        }
    }
    delay
}

#[cfg(test)]
mod tests {
    use super::*;

    // Mirrors Go `TestFailureBackoffMS`: the doubling table with the cap and the large-attempt
    // (no-overflow) case.
    #[test]
    fn failure_backoff_ms_doubles_and_caps() {
        const MAX: i64 = 300000;
        let cases = [
            (1, 10000),
            (2, 20000),
            (3, 40000),
            (4, 80000),
            (5, 160000),
            (6, 300000), // 320000 capped to max
            (7, 300000),
            (50, 300000), // large attempt: capped, no overflow
        ];
        for (attempt, want) in cases {
            assert_eq!(
                failure_backoff_ms(attempt, MAX),
                want,
                "failure_backoff_ms({attempt})"
            );
        }
    }

    // Mirrors Go `TestFailureBackoffClampsAttemptFloor`: attempt < 1 behaves as attempt 1.
    #[test]
    fn failure_backoff_clamps_attempt_floor() {
        assert_eq!(
            failure_backoff_ms(0, 300000),
            10000,
            "attempt<1 should behave as attempt 1"
        );
    }

    // Mirrors Go `TestContinuationDelay`.
    #[test]
    fn continuation_delay_is_1000() {
        assert_eq!(CONTINUATION_DELAY_MS, 1000);
    }
}
