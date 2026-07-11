//! concurrency — parity port of Go `internal/orchestrator/concurrency.go`.
//!
//! The two pure slot-accounting helpers the selection path (`select.rs`) counts against: the
//! global dispatch budget and the per-state limit (with its global fallback). Both are total
//! functions of their arguments — no orchestrator state — so they are free functions, mirroring
//! Go's package-level `GlobalSlots` / `StateLimit` (upstream §8.3).

use std::collections::HashMap;

use rhapsody_core::normalize_state;

/// Returns the available global dispatch slots (upstream §8.3): `max_concurrent - running`, clamped
/// at 0 so an over-capacity daemon (more running than the cap, e.g. after a cap-lowering reload)
/// never yields a negative budget. Mirrors Go `GlobalSlots`.
pub fn global_slots(max_concurrent: i64, running: i64) -> i64 {
    let n = max_concurrent - running;
    if n > 0 { n } else { 0 }
}

/// Returns the concurrency limit for a state: the per-state override under the NORMALIZED state key
/// when present, otherwise the global limit (upstream §8.3). Mirrors Go `StateLimit`. `per_state`
/// keys are already normalized (built by the effective config), but the lookup normalizes `state`
/// so a raw tracker state name (e.g. `"In Progress"`) matches the `"in progress"` key.
pub fn state_limit(state: &str, per_state: &HashMap<String, i64>, global: i64) -> i64 {
    match per_state.get(&normalize_state(state)) {
        Some(v) => *v,
        None => global,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Mirrors Go `TestGlobalSlots` (concurrency_test.go).
    #[test]
    fn global_slots_available_and_clamped() {
        assert_eq!(global_slots(10, 3), 7);
        assert_eq!(global_slots(2, 5), 0, "over-capacity should clamp to 0");
    }

    // Mirrors Go `TestStateLimit` (concurrency_test.go).
    #[test]
    fn state_limit_override_then_global_fallback() {
        let per_state = HashMap::from([("in progress".to_string(), 2i64)]);
        assert_eq!(
            state_limit("In Progress", &per_state, 10),
            2,
            "per-state override"
        );
        assert_eq!(
            state_limit("Todo", &per_state, 10),
            10,
            "fallback to global"
        );
    }
}
