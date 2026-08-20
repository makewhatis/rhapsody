# CLAUDE.md — .github/workflows

No ancestor `.github/CLAUDE.md` exists; the nearest ancestor is the repo-root `CLAUDE.md`.
`release.yml` is directly coupled to `harness/release/`'s validator scripts (see harness/CLAUDE.md,
which documents that pairing from the harness side) — this file documents it from the workflow side.

## Shared conventions across all three files

- Every job runs on `[self-hosted, macOS, ARM64]` — this repo has no GitHub-hosted runners. `rustup`
  lives in `~/.cargo/bin`, which is **not** on the runner's seeded PATH; every Rust job starts with
  `echo "$HOME/.cargo/bin" >> "$GITHUB_PATH"` then `rustup show` (installs the pinned toolchain).
  `actions/setup-node@v4` jobs skip `cache: npm` deliberately — the runner's npm cache is already
  local/persistent, and cache-upload would ship ~1GB to GitHub every run.
- Fork-PR guard, repeated verbatim as a per-job `if:` (not a workflow-level gate) in `ci.yml`:
  `github.event_name != 'pull_request' || github.event.pull_request.head.repo.full_name ==
  github.repository`. A fork PR skips every job, so its required checks never report and it stays
  unmergeable until pulled to a same-repo branch — untrusted code never runs on the self-hosted Mac.
  Carry this `if:` onto any new job added to `ci.yml`.
- Untrusted text (a PR title, a commit message) is passed to a script only via `env:`, never
  interpolated as `${{ ... }}` inside a `run:` block — that interpolation is a shell-injection hole.
  `pr-title.yml` and `release.yml`'s "nothing releasable" step both follow this.
- Job `id`s are branch-protection required-context strings (`lint`, `test`, `web`, `boot-e2e`,
  `desktop`, `pr-title`). Renaming one silently breaks branch protection — nothing here re-adds it;
  that's a manual, admin-gated step (see PR bodies for the ones already wired).

## ci.yml

- `boot-e2e` builds the web dist first (rust-embed needs `crates/httpapi/web-dist` to exist before
  `cargo build -p rhapsodyd`), then runs `harness/e2e/boot.sh` — one of three places in this repo
  that exercise the fully assembled daemon rather than a crate's unit tests, alongside `release.yml`
  and this same file's `desktop` job (`RHAPSODY_PARITY_E2E=1 cargo test -p rhapsody-desktop --test
  parity_e2e`, see below).
- `desktop` runs `make app` (root CLAUDE.md covers what that target produces), **not** `make dmg`:
  `hdiutil` fails with "Operation not permitted" on the self-hosted dev Mac whenever a volume named
  `Rhapsody` is already mounted (e.g. the app running from a previously-built DMG). DMG packaging
  happens only in `release.yml`. The parity e2e step (`RHAPSODY_PARITY_E2E=1 cargo test ...
  parity_e2e`) consumes the `.app` bundle's `Contents/Resources/rhapsodyd`, never a `.dmg`.
- The Tauri CLI install (`cargo install tauri-cli --version "^2" --locked`) is idempotent and left to
  persist on the runner between jobs/runs — don't add a cache step for it.

## pr-title.yml

- Its own workflow rather than a `ci.yml` job specifically so it can trigger on `types: [opened,
  edited, reopened, synchronize]`. Adding `edited` to `ci.yml` instead would re-run the entire
  lint/test/web/boot-e2e/desktop matrix on every title/body edit — that's the reason for the split.
- `concurrency: { group: pr-title-<PR#>, cancel-in-progress: true }` — the only workflow here that
  cancels in-flight runs. `release.yml` explicitly sets `cancel-in-progress: false`; `ci.yml`
  defines no `concurrency:` block at all. Safe here because `edited` re-fires without a new commit
  SHA, so only the newest title matters.
- Validates via two scripts, both under `harness/release/`, not `make test`:
  `pr_title_test.sh` (self-test of the validator's case table) runs first, then
  `check-pr-title.sh "$PR_TITLE"` (the actual gate). If you change what release-please's
  conventional-commit types accept, update both `check-pr-title.sh` and its case table together —
  this workflow only proves they still agree with each other, not with upstream release-please.

## release.yml

Three sequential jobs via `needs`: `release-please` → `build` (sign/notarize/verify/upload) →
`homebrew-bump`. `release-please-config.json` / `.release-please-manifest.json` (repo root, not in
this directory) define the "simple" single-package release type — no source-file version rewrites.

- Two triggers converge on `build`: a real `push`-to-main release-please cut
  (`release_created == 'true'`), or a manual `workflow_dispatch` dry-run pointed at an existing
  draft/prerelease `tag` input. `release-please` itself always runs on `push` (never skipped), which
  is what lets `build`'s `if:` be a plain OR with no `always()`/status-check gymnastics.
- Signing uses a **dedicated** keychain (`rhapsody-signing.keychain-db`), unlocked by the throwaway
  `KEYCHAIN_PASSWORD` secret — never the runner's login keychain (TRA-257). The keychain-prep steps
  (unlock, add to search list, partition-list) run unconditionally before `make dmg`, and the whole
  block retries once on `errSecInternalComponent` specifically (a known non-interactive-codesign
  flake), re-raising any other failure.
- The RD2 notarization gate asserts on the literal string `"Notarized Developer ID"` in `spctl`
  output, not `spctl`'s exit code — the exit code's meaning varies by assessment type across macOS
  versions. `-t open` for the `.dmg`, `-t exec` for the `.app` mounted out of it.
- Optional-secret jobs/steps (updater artifacts on `TAURI_SIGNING_PRIVATE_KEY`, `homebrew-bump` on
  `HOMEBREW_TAP_TOKEN`) no-op with a `::warning::` when the secret is unset rather than failing the
  release — only `APPLE_SIGNING_IDENTITY` (a repo *variable*, not a secret) is a hard `::error::` if
  missing. Preserve that asymmetry if you add another gated step: required build inputs fail loud,
  optional publish channels degrade quietly.
- `homebrew-bump` pushes straight to `makewhatis/homebrew-tap`'s unprotected `main` — no branch, no
  PR (TRA-270; the old branch+PR flow left casks unmerged behind already-shipped releases). It reads
  the DMG checksum back from the `build` job's uploaded `SHA256SUMS` Release asset rather than
  recomputing it, since jobs share no filesystem.
- Both `build` and `homebrew-bump` call `desktop/scripts/render-*.sh` (`render-cask.sh`,
  `render-latest-json.sh`) as the single source of truth for generated content — don't hand-edit a
  cask or `latest.json` shape here; change the renderer.
