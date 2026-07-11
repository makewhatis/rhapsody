//! symphonyd library — boot-support modules for the daemon binary.
//!
//! The `symphonyd` package carries both a binary (`main.rs`, the daemon entrypoint) and this
//! library of pieces the binary composes. P6-T1 seeds it with [`banner`] (Go `internal/banner`,
//! whose sole importer is `cmd/symphony`); the F1 assembly ticket ports the rest of `cmd/symphony`
//! (boot order, run-lock, `mcp` subcommand) on top.

pub mod banner;
