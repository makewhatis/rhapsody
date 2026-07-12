# Rhapsody.app (macOS desktop)

A native macOS app (Tauri v2 — Rust + system WKWebView) that supervises the `rhapsodyd` daemon,
shows its dashboard, and owns config + Linear credentials, so Rhapsody is a double-clickable tool
instead of a pile of CLI prerequisites. Parity port of the Go/Wails shell (`$REF/desktop`).

This is its **own cargo workspace** (see `Cargo.toml`), deliberately excluded from the repo-root
workspace, so Tauri's heavy dependency tree stays out of the `rhapsodyd` daemon build — the same
isolation the Go module boundary provided. Root `cargo test --workspace` / `make lint` never
compile it; the `desktop` CI job (`.github/workflows/ci.yml`) builds and tests it on its own.

## What it does (P7 chain)

- **Window shell** (P7-D1, this scaffold): a status header + the daemon's loopback dashboard once
  healthy, with clear not-configured / starting / stopped / error states otherwise. The pure
  view-logic (`frontend/src/status.ts`) is ported 1:1 from the reference and unit-tested.
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
│   │   ├── main.rs         # D1 bin: commands (status/app_version) + Builder::run
│   │   ├── lib.rs          # D2 library root: pub supervisor / apiproxy / tooldirs (consumed by D3)
│   │   ├── status.rs       # D1: StatusDto + Configured() detection (≈ app.go)
│   │   ├── version.rs      # D1: build stamp (≈ internal/version)
│   │   ├── supervisor/     # D2: launch/health/restart/drain + env + resolve (≈ internal/supervisor)
│   │   ├── apiproxy.rs     # D2: same-origin /api + /healthz reverse proxy (≈ desktop/apiproxy.go)
│   │   ├── tooldirs.rs     # D2: agent-launch PATH, override dirs first (≈ app.go + toolcheck/dirs.go)
│   │   └── bin/fakedaemon.rs  # D2: rhapsodyd test stub (≈ internal/supervisor/testdata/fakedaemon)
│   └── tests/              # D2: supervisor lifecycle + gated real-rhapsodyd smoke
└── frontend/               # React + TS + Vite shell (≈ $REF/desktop/frontend)
    └── src/status.ts       # pure view-logic (status.test.ts asserts it)
```

## Build & test

Prerequisites: the repo-pinned Rust toolchain (`rustup show` installs it), Node + npm, and the
Xcode command-line tools. The **frontend bundle must be built before any `cargo` command**, because
`tauri::generate_context!` embeds `frontend/dist` (git-ignored build output, kept only as a
`.gitkeep` anchor — mirrors the Go shell's `frontend/dist`):

```sh
cd desktop/frontend && npm ci && npm test && npm run build   # -> frontend/dist
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

`make app` builds the daemon's embedded dashboard (`web/` → `crates/httpapi/web-dist`), the shell
frontend (`desktop/frontend/dist`), and the release `rhapsodyd`, bundles `Rhapsody.app` with the
`fakedaemon` test stub excluded (`--no-default-features`), and copies the sidecar into
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
