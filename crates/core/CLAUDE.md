# CLAUDE.md — crates/core

`rhapsody-core` ports Go `internal/core` (plus `internal/runtimeport`). Four independent modules,
each with its own thorough top-of-file doc comment naming the exact Go source it mirrors — read
that comment before touching a module; this file only adds what isn't visible from inside the
crate.

## This crate sits at the bottom of the dependency graph

`rhapsody-core` is a dependency of nearly every other workspace crate
(`grep -rn 'rhapsody-core' crates/*/Cargo.toml` to see the current set). A change to a public
type's shape here (adding/removing a field, changing an `Option<T>`/`Vec<T>` choice) can ripple
into every one of those crates' own parity goldens, not just this crate's tests. Before reshaping
`Issue`, `Project`, `Viewer`, or `Info`, check who else constructs or matches on them.

## No golden-fixture harness here — unlike most other porting crates

Root `CLAUDE.md`'s parity-testing model does **not** apply to this crate: `rhapsody-core` has no
`harness-fixtures` dependency and there is no `harness/fixtures/core/`. Parity is pinned entirely
by the inline `#[cfg(test)]` unit tests in each module (each one commented `// Mirrors Go TestX`).
That's intentional, not a gap to backfill — don't add a `harness-fixtures` dependency or a
`harness/fixtures/core/` directory to "match the other crates"; when porting a new Go test here,
just add the Rust equivalent in the same module with the same `Mirrors Go TestX` comment
convention so the two test suites stay diffable against each other.
