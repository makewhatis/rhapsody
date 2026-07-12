# Rhapsody

Rust parity port of Symphony — the daemon that reads work from Linear, creates isolated
per-issue workspaces, and runs Claude Code agents inside them. The daemon binary ships as
`rhapsodyd` — a standalone Rust daemon whose runtime behavior is a faithful clone of the Go
`symphony` daemon, with the deliberate exceptions listed under [Divergences](#divergences)
(the binary name and the runtime filesystem paths).

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

## Divergences

Rhapsody is a byte-for-byte parity port of Go Symphony v0.4.0 EXCEPT where this section says
otherwise. Each entry is a deliberate, reviewed decision; nothing else may drift from the frozen
reference (the parity goldens stay byte-strict).

### Runtime paths → `~/.rhapsody` + `rhapsody.db` (TRA-238)

Rhapsody gets its own runtime home. The daemon's filesystem paths and the history DB filename are
rebranded off Symphony's `~/.symphony`:

| Purpose | Go Symphony v0.4.0 | Rhapsody |
| --- | --- | --- |
| Workspace root default | `~/.symphony/symphony_workspaces` | `~/.rhapsody/workspaces` |
| Log/transcript dir default | `~/.symphony/logs` | `~/.rhapsody/logs` |
| History DB default | `~/.symphony/symphony.db` | `~/.rhapsody/rhapsody.db` |
| Runtime port file | `~/.symphony/runtime.json` | `~/.rhapsody/runtime.json` |
| Desktop supervised WORKFLOW.md | `~/.symphony/WORKFLOW.md` | `~/.rhapsody/WORKFLOW.md` |
| Repo-relative prompt defaults | `.symphony/PROMPT.md`, `.symphony/PROMPT.dep_mod.md` | `.rhapsody/PROMPT.md`, `.rhapsody/PROMPT.dep_mod.md` |

The repo-relative prompt defaults **fall back to the legacy `.symphony/` names** when the new
`.rhapsody/` path is absent from a checkout, so target repos that still ship `.symphony/PROMPT.md`
keep resolving their prompt untouched (the daemon's prompt resolver retries the `.symphony/`
counterpart before soft-falling-back to the inline prompt).

**Out of scope (unchanged live wire contracts):** the `SYMPHONY_RUN_ID` / `SYMPHONY_ISSUE` (and
sibling) agent env vars, the `symphony_*` MCP tool names, the `symphony/<key>` git branch prefix,
and the `@symphony` summon token — all cross-process contracts that a path rebrand must not break.

**Fixture policy:** the config goldens (`harness/fixtures/config/*.json` + `api/config.json`) encode
the daemon's resolved DEFAULTS, which now diverge. `harness/capture/capture.sh` applies a documented,
idempotent `sed` (the two default strings above) to those files after capturing from the Go daemon,
so `make fixtures` re-derives the committed state deterministically. Every other golden — including
the Go-written transcript paths in `api/history.json` + `db/go-daemon-rows.json` — stays a byte-exact
record of Go's output, and the red-on-drift canary is unchanged.
