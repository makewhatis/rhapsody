//! rhapsody-config — parity port of Go `internal/config`. Filled in by its phase.
//!
//! `internal/workflow` and `internal/prompt` become modules of this crate (P0 crate layout);
//! Task C7 ports the `prompt` module — strict Liquid rendering of the WORKFLOW.md prompt body.

pub mod prompt;
