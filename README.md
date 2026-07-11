# Rhapsody

Rust parity port of Symphony — the daemon that reads work from Linear, creates isolated
per-issue workspaces, and runs Claude Code agents inside them. The daemon binary ships as
`symphonyd` (drop-in sidecar for the existing desktop shell).

- Specs & plans: Linear project documents (Rhapsody project) — never committed to this repo.
- Parity reference (read-only, NOT in this repo):
  `/Users/david/Downloads/symphony-v0.4.0/golang/symphony` (Symphony v0.4.0)
- Golden fixtures: `harness/fixtures/` — captured via `make fixtures`, asserted by every crate.

Build: `cargo build --workspace` · Test: `make test` · Lint: `make lint`
