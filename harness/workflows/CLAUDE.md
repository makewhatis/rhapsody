# CLAUDE.md — harness/workflows

One file, `smoke.md`: a template WORKFLOW.md for booting the daemon against `linear-stub` +
`fake-claude` (the R3 Interfaces contract). Three sed placeholders — `__STUB_PORT__`,
`__FAKE_CLAUDE__`, `__STORE_PATH__` — and `no repo:` key, so the daemon provisions each per-issue
workspace as a plain `mkdir` (`EnsureFromRepo` → `createForIssue`) instead of a git checkout.

For how this dir relates to `harness/capture/workflows/`, see `harness/CLAUDE.md`'s "workflows/
smoke.md vs capture/workflows/*.md" section — not re-derived here.

## `smoke.md` is not actually wired into any script

`harness/CLAUDE.md` and `smoke.md`'s own header comment claim `e2e/boot.sh` fills this file in
directly. **That's stale — verify before trusting it.** As of this writing:

- `harness/capture/capture.sh` and `harness/e2e/boot.sh` both read and sed-fill
  `harness/capture/workflows/minimal.md` — never this directory. (`harness/CLAUDE.md`'s `capture/`
  section covers the sed mechanics; not repeated here.)
- `boot.sh` fills the placeholder as `__CLAUDE_CMD__` — **not** `__FAKE_CLAUDE__`, the name this
  file actually uses.

So today, nothing in the repo reads `harness/workflows/smoke.md` by path. It survives only as the
original R3 template that `harness/capture/workflows/minimal.md`'s own header comment says it
"mirrors" (a comment, not a generation step — the two files are hand-kept in sync, not derived).
Treat `smoke.md` as documentation/reference for the minimal WORKFLOW.md shape, not as a live
fixture input. If you're trying to change what `make fixtures` or CI's boot gate actually boots,
edit `harness/capture/workflows/minimal.md`, not this file.

## If you do add a real consumer of `smoke.md`

Match its placeholder names exactly — `__STUB_PORT__`, `__FAKE_CLAUDE__`, `__STORE_PATH__` — or
reconcile them with the `__CLAUDE_CMD__` name `capture.sh`/`boot.sh` already use elsewhere, and
update the `harness/CLAUDE.md` and `harness/capture/CLAUDE.md` sections that describe this file so
the "shared three-placeholder contract" claim stays accurate for whichever name you pick.
