//! rhapsody-config — parity port of Go `internal/config`.
//!
//! `internal/workflow` and `internal/prompt` become modules of this crate (P0
//! crate layout): [`workflow`] ports the WORKFLOW.md front-matter loader + save
//! (Go `internal/workflow`); [`prompt`] ports strict Liquid rendering of the
//! prompt body (Go `internal/prompt`, Task C7).

pub mod prompt;
pub mod workflow;
