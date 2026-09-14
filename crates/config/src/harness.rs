//! harness — the one list of recognized `agent.backend` values (design record
//! `pluggable-harnesses-design.md` §1.3/§4.3, STUDIO-893 slice 1).
//!
//! Before this module, [`validate`](crate::validate) and the orchestrator's runner constructor
//! each hardcoded their own notion of "which backend names exist," and the two disagreed: `codex`
//! passed config validation but had no `Runner` behind it. This module does not resolve that
//! disagreement — recognized-by-config and implemented-by-the-orchestrator are genuinely
//! different questions, and Go's own reference keeps them separate too (`ValidateDispatch`
//! accepts `codex`, `runnerForBackend` rejects it; the composite `ValidateConfig` pipeline is what
//! catches the gap before a config persists). What this module removes is the DUPLICATION: there
//! is now exactly one place that spells out the names `validate` accepts. `validate` reads it
//! directly; the orchestrator's `runner_for_backend` has a pinned regression test (`effective.rs`)
//! asserting its one implemented name stays a member of this list, so the two cannot silently
//! drift apart the way they already had.
//!
//! Deliberately **names only** (no capability metadata): a richer per-name contract is slice 2's
//! `HarnessCapabilities`, reviewed on its own terms, not this slice's job to anticipate.

/// The `agent.backend` values [`validate`](crate::validate::validate) accepts. Mirrors Go
/// `ValidateDispatch`'s `claude`/`codex` pair. Adding a name here makes config validation accept
/// it; it does NOT give the orchestrator a `Runner` for it — that remains a separate, per-build
/// decision (today: only `claude` is implemented).
pub const HARNESS_NAMES: &[&str] = &["claude", "codex"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_names_are_known() {
        assert!(HARNESS_NAMES.contains(&"claude"));
        assert!(HARNESS_NAMES.contains(&"codex"));
    }

    #[test]
    fn unknown_name_is_not_known() {
        assert!(!HARNESS_NAMES.contains(&"openai"));
        assert!(!HARNESS_NAMES.contains(&""));
    }
}
