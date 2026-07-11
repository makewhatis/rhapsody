//! rhapsody-core — parity port of Go `internal/core`.
//!
//! Domain types shared across orchestration, prompt rendering, and observability,
//! ported field-for-field from Symphony v0.4.0's `internal/core/{issue,project,summon}.go`
//! (Rhapsody P1 plan, Task C1). Go pointer fields (`*T`) and nil slices become `Option<…>`
//! so the unset-vs-zero distinction the Go daemon relies on is preserved.

pub mod issue;
pub mod project;
pub mod summon;

pub use issue::{BlockerRef, Comment, Issue, LinkedPRRef, normalize_state};
pub use project::{Project, Viewer};
pub use summon::compile_summon_re;

#[cfg(test)]
mod tests {
    // Mirrors Go `core.TestPackageBuilds` (sanity_test.go): compilation of this crate is
    // the assertion.
    #[test]
    fn package_builds() {}
}
