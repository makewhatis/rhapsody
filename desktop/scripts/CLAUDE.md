# CLAUDE.md — desktop/scripts

macOS packaging/signing/notarization/distribution tooling for the desktop app — see
`desktop/CLAUDE.md` for why nothing here needs cargo/npm. Invoked by the repo-**root** `Makefile`'s
`app`/`dmg`/`verify-icon` targets (there is no `desktop/Makefile`) and by
`.github/workflows/release.yml`.

## Testing — there is no test runner here, and coverage is uneven across scripts

Three of these scripts are pure-bash tests for their sibling: `notarize_args_test.sh` (tests
`notarize.sh`), `render_cask_test.sh` (tests `render-cask.sh`), `render_latest_json_test.sh` (tests
`render-latest-json.sh`).

Don't assume the rest are covered the same way — verified against
`desktop/src-tauri/tests/packaging_gate.rs`:
- `sign.sh` and `notarize.sh` are each also driven **directly** by `packaging_gate.rs`
  (`run_script("sign.sh", ...)` / `run_script("notarize.sh", ...)`), which asserts their gated-skip
  behavior (exit 0 + "skip" message when signing/notary credentials are unset) and their
  usage-guard behavior (non-zero exit with no args).
- `make-dmg.sh` and `verify-icon.sh` have **no test coverage at all**. `packaging_gate.rs` never
  references either one — `grep -n "run_script\|desktop_dir().join" packaging_gate.rs` has no hits
  for `make-dmg.sh` or `verify-icon.sh`. If you're touching either script, there is no automated
  safety net anywhere in the repo; verify by hand (`make dmg`, `make verify-icon` from repo root)
  before trusting a change.
- `render_latest_json_test.sh` is covered the same way the sibling `_test.sh` files are described in
  `desktop/CLAUDE.md`'s Non-obvious bullet — `packaging_gate.rs` shells out to it as a subprocess
  too, it's just not named there.

Each `_test.sh` is also directly runnable standalone for fast iteration, no cargo build needed:
`./notarize_args_test.sh`, `./render_cask_test.sh`, `./render_latest_json_test.sh` (each is
self-locating via `dirname "${BASH_SOURCE[0]}"`, so it works from any cwd). Prefer this loop while
editing a renderer/lib script — a full `cargo test` recompiles the whole `desktop` crate first.

## Shared conventions across these scripts

- **Gated no-op is the default posture.** `sign.sh` and `notarize.sh` exit 0 and print a "skipping"
  message when their required credential env var (`APPLE_SIGNING_IDENTITY`, `NOTARY_PROFILE`/`ASC_*`)
  is unset, rather than failing — an unsigned/autonomous build must stay green. `make-dmg.sh`'s
  `sign_dmg` follows the same gate. Never make one of these hard-fail on missing credentials; that
  breaks CI and every contributor's non-release build.
- **Fail loud on partial/malformed input, never silently guess.** `notarize.sh`'s
  `notary_auth_args` returns a distinct exit code (2) for a half-set `ASC_*` trio instead of falling
  back to profile mode; `render-cask.sh`/`render-latest-json.sh` regex-validate version/sha256/date/
  signature/url shape before rendering anything, so a malformed release input fails at the source
  script rather than producing a broken cask/manifest that fails later (or silently) downstream.
- **Required positional args use `"${1:?usage: ...}"`**, not a hand-rolled `[ $# -lt 1 ] && exit`.
  Match this idiom if you add a new script or argument.
- **`render-cask.sh` and `render-latest-json.sh` are single sources of truth**, not scratch scripts:
  the cask output must stay byte-identical to the committed `Casks/rhapsody.rb` in the
  `makewhatis/homebrew-tap` repo, and `release.yml`'s auto-bump job re-runs `render-cask.sh` at
  release time expecting the same shape. Edit the heredoc/jq body and its `_test.sh` together.
- **`notarize.sh` doubles as a sourceable library.** `source notarize.sh --lib-only` (used by
  `notarize_args_test.sh`) loads `resolve_asc_key`/`notary_auth_args`/`notarize_target_kind` without
  requiring a target arg or touching `xcrun`/the network — the `BASH_SOURCE[0]` != `$0` check near
  the bottom of the file is what makes that work. Preserve it if you restructure the script.

## Non-obvious pitfalls

- **`notarize_args_test.sh` is kept bash 3.2-compatible on purpose** (comment at its top) because it
  runs under macOS's ancient system `/bin/bash`, not a Homebrew bash — don't introduce `${var,,}`,
  associative arrays, or other bash 4+ syntax into it or `notarize.sh`'s sourced functions.
- **Test scripts scrub credential env vars before running**, both their own (`notarize_args_test.sh`
  unsets `NOTARY_*`/`ASC_*` per-case) and via `packaging_gate.rs`'s `scrubbed_env()` — so a developer
  with `APPLE_SIGNING_IDENTITY`/notary creds exported locally can't accidentally make a test attempt
  real signing/notarization. Don't remove a scrub when editing a test case.
- **These scripts assume real macOS tooling on `PATH`**: `codesign`, `xcrun` (`notarytool`,
  `stapler`), `hdiutil`/`ditto`/`diskutil`, `iconutil`/`sips`, and `jq` (ships at `/usr/bin/jq` on the
  macOS runners this targets). None of it runs on Linux CI — packaging/signing/dmg/icon jobs are
  macOS-only by construction, not an oversight.
- **`make-dmg.sh`'s two-path fallback (`create-dmg` → image+ditto) is load-bearing, not redundant**:
  `hdiutil create -srcfolder` breaks on macOS 15+ once the app has been launched once (it acquires a
  kernel-only `com.apple.provenance` xattr `hdiutil` can't replicate). If you're debugging a dmg
  build failure locally, check whether you've run the `.app` first before assuming the script is
  broken.
