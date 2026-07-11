# Rhapsody

Rust parity port of Symphony — the daemon that reads work from Linear, creates isolated
per-issue workspaces, and runs Claude Code agents inside them. The daemon binary ships as
`symphonyd` (drop-in sidecar for the existing desktop shell).

- Specs & plans: Linear project documents (Rhapsody project) — never committed to this repo.
- Parity reference (read-only, NOT in this repo): `$REF` (operator-provided path to the frozen
  Symphony v0.4.0 tree).
- Golden fixtures: `harness/fixtures/` — captured via `make fixtures`, asserted by every crate.

Build: `cargo build --workspace` · Test: `make test` · Lint: `make lint`

## Parity testing

Porting crates take `harness-fixtures` as a dev-dependency and assert their output equals the
committed goldens (after `normalize`). The crate exposes `load`/`load_json` (read a fixture by
path relative to `harness/fixtures/`) and `normalize`/`normalize_with_home` — a Rust mirror of
`harness/capture/normalize.sh`, kept in lockstep by a canary that runs the shell script and
requires byte-identical output. Editing, corrupting, or losing a committed golden turns
`cargo test -p harness-fixtures` red. Fixture provenance + recapture: `harness/capture/README.md`.
