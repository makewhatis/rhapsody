# CLAUDE.md — desktop/build

See `desktop/CLAUDE.md`'s "Non-obvious layout facts" section and this directory's own
[`README.md`](README.md) for what lives here and why (layout, the Wails `Info.plist` /
`appicon.png` non-equivalents, where the signed/unsigned app bundle lands). This file only adds
what neither of those covers: how `darwin/entitlements.plist` is actually validated, and the rule
for editing it.

## What actually validates this directory

Nothing in `desktop/build/` is exercised by building it — there's no crate here for `cargo build`
to touch. The only thing that checks `darwin/entitlements.plist` is
`desktop/src-tauri/tests/packaging_gate.rs::entitlements_plist_valid_and_has_key`, which:

- asserts the file exists and contains the literal key
  `com.apple.security.cs.disable-library-validation`,
- runs `plutil -lint` on it when `plutil` is available (macOS only; silently skipped elsewhere —
  don't mistake a green CI run on a non-macOS runner for a real lint pass).

That test runs as part of plain `cargo test` inside `desktop/` (see `desktop/CLAUDE.md`) — there is
no separate command to invoke just for this directory. If you edit the plist, run the gate test
locally on macOS before trusting it; a substring match without the `plutil` pass can hide a
malformed plist.

## Editing the entitlements file

- Keep it minimal. The file's own header comment documents which entitlements were deliberately
  left *out* (`allow-jit`, `allow-unsigned-executable-memory`,
  `allow-dyld-environment-variables`) and why — don't add one back without a real signed-launch
  test proving it's needed, and update that comment when you do.
- The required key is asserted by exact string match in the gate test (see above), not by any
  structural plist diff — renaming or restructuring around it will fail the test even if the
  resulting entitlements are semantically equivalent.

## `bin/`

Not present on a clean checkout; created by `make dmg` (git-ignored). If you find stale contents
here during local debugging, it's safe to delete — nothing reads from `bin/`, only `make dmg`
writes `Rhapsody.dmg` into it.
