# Rhapsody.app (macOS desktop)

A native macOS app (Tauri v2 — Rust + system WKWebView) that supervises the `rhapsodyd` daemon,
shows its dashboard, and owns config + Linear credentials, so Rhapsody is a double-clickable tool
instead of a pile of CLI prerequisites. Parity port of the Go/Wails shell (`$REF/desktop`).

This is its **own cargo workspace** (see `Cargo.toml`), deliberately excluded from the repo-root
workspace, so Tauri's heavy dependency tree stays out of the `rhapsodyd` daemon build — the same
isolation the Go module boundary provided. Root `cargo test --workspace` / `make lint` never
compile it; the `desktop` CI job (`.github/workflows/ci.yml`) builds and tests it on its own.

## What it does (P7 chain)

- **Window shell**: the top-level window IS the `web/` React dashboard, served from the embedded
  bundle over a custom URI scheme (`rhapsody://localhost`) with the app's same-origin `/api/*` fetches
  reverse-proxied to the supervised daemon (`src/windowserver.rs` wiring the `apiproxy` core) — the
  analogue of Go's Wails `AssetServer` + middleware. A native overlay titlebar (traffic lights only)
  sits over the dashboard's Podium toolbar: one bar, no double chrome (TRA-251). The interim
  header-plus-iframe shell (`desktop/frontend/`) was retired in that migration.
- **Supervises `rhapsodyd`** as a bundled sidecar — launch on explicit `--port`, `/healthz`
  readiness, crash-restart backoff, clean SIGTERM drain — plus the same-origin `/api` + `/healthz`
  reverse proxy. **D2 (landed)** ships these as the `supervisor` / `apiproxy` / `tooldirs` library
  modules + the `fakedaemon` test stub, proven against the stub and (gated) the real release
  rhapsodyd; **D3** wires them into the window/tray.
- **Menu-bar tray** + app lifecycle (hide-on-close, quit drain) — **D3**.
- **Settings**: Keychain credential, prefs, Linear project picker, onboarding, tool doctor — **D4**.
- **Packaging** (**D5, landed**): unsigned `Rhapsody.app` + drag-to-Applications dmg; env-gated
  Developer ID signing (opt-in vars only, no secrets). See [Packaging](#packaging-unsigned-app--dmg)
  + [`SIGNING.md`](./SIGNING.md). The signed/notarized dmg + machine install stay David's manual steps.

## Layout

```
desktop/
├── Cargo.toml              # workspace root (members = ["src-tauri"])
├── src-tauri/              # the Rust Tauri app (≈ $REF/desktop/main.go + app.go)
│   ├── Cargo.toml
│   ├── build.rs            # tauri-build + version-stamp env wiring
│   ├── tauri.conf.json     # window (Rhapsody, 1100x760) + bundle config
│   ├── capabilities/       # Tauri v2 ACL
│   ├── icons/              # app icon (placeholder until D5)
│   ├── src/
│   │   ├── main.rs         # bin: #[tauri::command]s + windowserver::register + Builder::run
│   │   ├── lib.rs          # library root: pub app / supervisor / apiproxy / windowserver / …
│   │   ├── app.rs          # App lifecycle + StatusDto + daemon_base_url (≈ app.go)
│   │   ├── windowserver.rs # serve web/ over rhapsody:// + reverse-proxy /api (≈ main.go AssetServer)
│   │   ├── version.rs      # build stamp (≈ internal/version)
│   │   ├── supervisor/     # launch/health/restart/drain + env + resolve (≈ internal/supervisor)
│   │   ├── apiproxy.rs     # same-origin /api + /healthz reverse-proxy core (≈ desktop/apiproxy.go)
│   │   ├── tooldirs.rs     # agent-launch PATH, override dirs first (≈ app.go + toolcheck/dirs.go)
│   │   └── bin/fakedaemon.rs  # rhapsodyd test stub (≈ internal/supervisor/testdata/fakedaemon)
│   └── tests/              # supervisor lifecycle + packaging gate + gated real-rhapsodyd/parity e2e
└── (frontend) the top-level window renders web/ — no separate shell; frontendDist -> crates/httpapi/web-dist
```

## Build & test

Prerequisites: the repo-pinned Rust toolchain (`rustup show` installs it), Node + npm, and the
Xcode command-line tools. `tauri::generate_context!` embeds `frontendDist` (`../../crates/httpapi/web-dist`,
the `web/` vite build — git-ignored output kept only as a `.gitkeep` anchor), so the cargo steps
compile against an empty dist on a clean checkout; build the real `web/` bundle before running the app:

```sh
cd web && npm ci && npm run build   # -> crates/httpapi/web-dist (the Tauri frontend AND the daemon embed)
cd desktop && cargo build && cargo test                      # unsigned build + tests
```

Lint (the desktop workspace carries its own `cargo fmt` / `clippy -D warnings`, since the root
`lint` job excludes it):

```sh
cd desktop && cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings
```

`cargo tauri dev` runs the app with hot reload; `cargo tauri build` (the bundler) is driven by the
packaging targets below.

## Packaging (unsigned .app + dmg)

Packaging uses the Tauri bundler via the repo-root Makefile (parity of the Go `make app`/`make dmg`).
Prereq: the Tauri CLI — `cargo install tauri-cli --version "^2"`. From the **repo root**:

```sh
make app        # build the UNSIGNED Rhapsody.app (Tauri bundler) with rhapsodyd embedded as the sidecar
make dmg        # build the app, then package a drag-to-Applications Rhapsody.dmg installer
```

`make app` builds the `web/` dashboard (`crates/httpapi/web-dist` — the same bundle is both the daemon's
embedded dashboard AND the Tauri window's `frontendDist`) and the release `rhapsodyd`, bundles
`Rhapsody.app` with the `fakedaemon` test stub excluded (`--no-default-features`), and copies the sidecar into
`Contents/Resources/rhapsodyd` (where `supervisor/resolve.rs` looks). Output:
`desktop/target/release/bundle/macos/Rhapsody.app`. `make dmg` additionally writes
`desktop/build/bin/Rhapsody.dmg` (via `create-dmg`, or an `hdiutil` fallback needing no extra deps).

> **Unsigned by default.** A plain build is not code-signed or notarized (Tauri ad-hoc self-signs).
> Gatekeeper warns on first open (right-click → Open, or `xattr -dr com.apple.quarantine Rhapsody.app`).
> A **gated** Developer-ID code-signing + notarization path (a clean no-op without creds — the
> unsigned path never touches the keychain or the network) is wired into `make dmg` via
> `desktop/scripts/{sign,notarize}.sh` and keys off `APPLE_SIGNING_IDENTITY` / `NOTARY_PROFILE`. The
> signed+notarized dmg and the machine install stay **David's manual steps** — see
> [`SIGNING.md`](./SIGNING.md). The gating contract is pinned by `src-tauri/tests/packaging_gate.rs`.

## Version stamping

The build stamp shown in the app footer (`version.rs`, mirroring Go `internal/version` + the
Makefile `-ldflags` pattern) is overridden at build time via env vars — unset builds report the
`dev` / `none` / `unknown` defaults:

```sh
RHAPSODY_VERSION=1.2.0 \
RHAPSODY_COMMIT="$(git rev-parse --short HEAD)" \
RHAPSODY_BUILD_TIME="$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
  cargo build
```

The Makefile `app`/`dmg` targets set these from `VERSION` + git (see [Packaging](#packaging-unsigned-app--dmg)).

## Auto-update (in-app updater channel)

Rhapsody updates itself in place via [`tauri-plugin-updater`](https://v2.tauri.app/plugin/updater/)
(the backend + guarded install flow landed in **P11-U1**; `src/update.rs`). Each GitHub Release carries
two extra assets that drive it (**P11-U2**, `release.yml`'s `build` job):

- **`Rhapsody.app.tar.gz`** — a gzipped tarball of the **signed + notarized + stapled** `Rhapsody.app`,
  built *after* `make dmg` (from `desktop/target/release/bundle/macos/Rhapsody.app`, not the bundler's
  build-time artifact, which predates signing/notarization).
- **`latest.json`** — the manifest the plugin polls at
  `https://github.com/makewhatis/rhapsody/releases/latest/download/latest.json` (the endpoint pinned in
  `src-tauri/tauri.conf.json`). When its `version` is newer than the running app, the plugin downloads
  `platforms."darwin-aarch64".url` (the tarball) and verifies it against
  `platforms."darwin-aarch64".signature` (a **minisign** signature) using the `pubkey` pinned in
  `tauri.conf.json`. A bad or absent signature aborts the update — nothing unsigned is ever installed.
  `darwin-aarch64` is the only shipped target (Rhapsody is Apple-Silicon-only).

`desktop/scripts/render-latest-json.sh <version> <pub_date> <signature> <url> [notes]` is the single
source of truth for the manifest body; `render_latest_json_test.sh` (run by the `desktop` job via
`src-tauri/tests/packaging_gate.rs`) pins it. The signature is produced by `cargo tauri signer sign`
over the tarball — the same minisign key + format the bundler's `createUpdaterArtifacts` would use.

The whole step is **gated on the updater keypair** and cleanly no-ops (green, with a `::warning::`) when
it is absent, so the dmg/binary release never goes red for a missing key. The cask's `auto_updates true`
(see [Homebrew tap](#homebrew-tap)) makes `brew upgrade` defer to this in-app channel.

**Operator setup (one time):** the updater **public** key is already committed in `tauri.conf.json`
(pinned in U1). Set the matching **private** key + its password as repo secrets so `build` can sign the
tarball:

```sh
gh secret set TAURI_SIGNING_PRIVATE_KEY --repo makewhatis/rhapsody          # the minisign private key…
gh secret set TAURI_SIGNING_PRIVATE_KEY_PASSWORD --repo makewhatis/rhapsody # …and its password
```

The private key **must pair with the committed `pubkey`** (a fresh `cargo tauri signer generate` would
mismatch it, and every client would reject the update). Rotating the keypair means updating **both** the
secret and the committed `pubkey` — which changes the app, so it ships in a subsequent release. Confirm
the wiring with a `workflow_dispatch` dry-run against a draft/prerelease tag (same as the dmg dry-run):
it uploads `Rhapsody.app.tar.gz` + `latest.json` next to the installer assets.

## Homebrew tap

Rhapsody installs from a Homebrew cask served out of a public tap
([makewhatis/homebrew-tap](https://github.com/makewhatis/homebrew-tap)):

```sh
brew install --cask makewhatis/tap/rhapsody       # stable: tracks real releases
brew install --cask makewhatis/tap/rhapsody@rc    # rc: tracks prerelease tags (STUDIO-648)
```

The stable cask (`Casks/rhapsody.rb` in the tap) points at the `Rhapsody.dmg` asset on this repo's GitHub
Release, carries `auto_updates true` (the P11 in-app updater owns upgrades; `brew upgrade` won't clobber
it), and its `zap` stanza removes `~/.rhapsody` and the `is.makewhat.rhapsody` login-keychain item.

`desktop/scripts/render-cask.sh <version> <sha256> [channel]` is the single source of truth for both cask
bodies — it authored the committed casks and re-renders them on every release. `render_cask_test.sh` (run
by the `desktop` job via `src-tauri/tests/packaging_gate.rs`) pins each channel's output byte-for-byte.
This is still a simplified descendant of the Go reference's multi-channel `render-cask.sh`: two channels
served from GitHub Releases, no internal dist host, and no `verified:` stanza (Homebrew rejects
`verified:` as unnecessary when the url and homepage share the github.com domain). The reference's third
shape — per-feature-branch `rhapsody@<branch>` dogfood casks — is a **named follow-up**, not shipped: it
needs per-branch signed builds and a retention/cleanup story.

### The `@rc` channel

`rhapsody@rc` is opt-in and tracks **prerelease** tags (`v0.3.4-rc.1`). Its cask body is the stable body
plus the `rhapsody@rc` token and `conflicts_with cask: "rhapsody"` — both channels install the same
`/Applications/Rhapsody.app`, so at most one may be installed at a time. The conflict is declared one-way
(the stable cask is deliberately untouched by the rc channel), so brew blocks installing `@rc` over a
stable install; swapping back means `brew uninstall --cask rhapsody@rc` first.

Three isolation properties hold, and it's worth knowing *why* each one does:

- **`brew upgrade --cask rhapsody` never sees an rc.** The two casks are separate files with separate
  version stanzas, and the rc bump path writes only `Casks/rhapsody@rc.rb`. The renderer additionally
  refuses to emit an `@rc` cask for a non-prerelease version, so `@rc` can never name a final release.
- **The in-app auto-update channel stays stable-only, with no machinery.** tauri-plugin-updater polls
  `releases/latest/download/latest.json`, and GitHub never points `releases/latest` at a release flagged
  `prerelease`. That holds by construction — so instead of building anything, `homebrew-bump-rc` just
  *asserts* the flag and fails loudly if a prerelease tag was published as a full release.
- **An rc install auto-updates to the next FINAL release**, because tauri compares semver and
  `0.3.4-rc.1 < 0.3.4`. That is the intended exit ramp off the channel — there is no "downgrade" step.

### Release-time auto-bump

`release.yml`'s `homebrew-bump` job (runs after `build` on a real release) regenerates the cask with the
new version + the `Rhapsody.dmg` checksum from the release's `SHA256SUMS` asset and opens a bump PR
against the tap. It is **gated on the `HOMEBREW_TAP_TOKEN` secret** and cleanly skips (green, with a
`::warning::`) until that secret exists — the release itself never goes red for a missing token.

Its sibling `homebrew-bump-rc` does the same for `Casks/rhapsody@rc.rb`, reusing the same secret and the
same straight-push-to-tap-`main`. It runs on the **`workflow_dispatch`** path only — the flow that
actually builds an rc (`gh workflow run release.yml -f tag=v0.3.4-rc.1`) — and skips with a `::notice::`
when the dispatched tag is not a semver prerelease. It is deliberately *not* a `release: prereleased`
trigger: that event fires when a Release is published, i.e. before `build` has uploaded the `SHA256SUMS`
asset the bump reads its checksum from. release-please never cuts a prerelease itself, so no rc tag is
missed. The tap needs **no manual seed commit** for the rc cask — the first prerelease run creates
`Casks/rhapsody@rc.rb` and pushes it.

**Operator setup (one time, before the next release):** create a fine-grained PAT with **Contents:
read+write AND Pull requests: write** on `makewhatis/homebrew-tap`, then:

```sh
gh secret set HOMEBREW_TAP_TOKEN --repo makewhatis/rhapsody
```

(`GITHUB_TOKEN` is scoped to this repo and cannot push to the tap, which is why a cross-repo PAT is
required.) The **initial** cask was published manually, so `brew install` resolves today without the
token; the token only automates future bumps.

> **Note:** `brew install` downloads the dmg from this repo's Release. While `makewhatis/rhapsody` is a
> **private** repo the asset is not publicly fetchable (the cask still parses and `brew audit` passes,
> but the download 404s for anyone unauthenticated). Making the repo — or at least its releases —
> public is what lets `brew install --cask makewhatis/tap/rhapsody` complete for end users.
