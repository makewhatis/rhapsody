# CLAUDE.md — harness/capture

No manifest of its own (`capture.sh` is plain bash, `linear-stub` lives in `../stubs/`). Read
**`README.md` in this directory first** — it is the detailed, current spec for `make fixtures`
(what each fixture captures, the determinism contract, and the capture-fidelity rationale for
every non-obvious wait/retry in `capture.sh`). The parent `harness/CLAUDE.md` also has a
`capture/` section covering the `REF` fallback, the placeholder-normalization contract with
`normalize.sh`, and the TRA-238 `~/.symphony`→`~/.rhapsody` divergence rewrite. Don't duplicate
either here — this file only adds what neither says.

## scenarios/ and workflows/

- `scenarios/{success,error,hang}.json` map to the three outcomes capture.sh drives
  (`fake-claude`, `fake-claude-error`, `fake-claude-hang`) — one scenario file per outcome.
- `workflows/{minimal,full,graphite,hang}.md` feed capture.sh's four scenarios: `minimal` produces
  the `api/`/`schema.sql`/success-run fixtures plus the error/stalled runs; `full` and `graphite`
  each produce only their own `config/*.json` snapshot. For how these relate to
  `../workflows/smoke.md`, see `harness/CLAUDE.md`'s "workflows/smoke.md vs
  capture/workflows/*.md" section — not repeated here.

## Running it

There is no narrower entry point than root's `make fixtures`: `capture.sh` always regenerates the
entire `harness/fixtures/` tree in one pass, so you cannot recapture a single fixture in isolation.
