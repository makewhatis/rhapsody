# Rhapsody.app (macOS desktop)

A native macOS app (Tauri v2 — Rust + system WKWebView) that supervises the `symphonyd` daemon,
shows its dashboard, and owns config + Linear credentials, so Rhapsody is a double-clickable tool
instead of a pile of CLI prerequisites. Parity port of the Go/Wails shell (`$REF/desktop`).

This is its **own cargo workspace** (see `Cargo.toml`), deliberately excluded from the repo-root
workspace, so Tauri's heavy dependency tree stays out of the `symphonyd` daemon build — the same
isolation the Go module boundary provided. Root `cargo test --workspace` / `make lint` never
compile it; the `desktop` CI job (`.github/workflows/ci.yml`) builds and tests it on its own.

## What it does (P7 chain)

- **Window shell** (P7-D1, this scaffold): a status header + the daemon's loopback dashboard once
  healthy, with clear not-configured / starting / stopped / error states otherwise. The pure
  view-logic (`frontend/src/status.ts`) is ported 1:1 from the reference and unit-tested.
- **Supervises `symphonyd`** as a bundled sidecar — launch on explicit `--port`, `/healthz`
  readiness, crash-restart backoff, clean SIGTERM drain — plus the same-origin `/api` + `/healthz`
  reverse proxy. **D2 (landed)** ships these as the `supervisor` / `apiproxy` / `tooldirs` library
  modules + the `fakedaemon` test stub, proven against the stub and (gated) the real release
  symphonyd; **D3** wires them into the window/tray.
- **Menu-bar tray** + app lifecycle (hide-on-close, quit drain) — **D3**.
- **Settings**: Keychain credential, prefs, Linear project picker, onboarding, tool doctor — **D4**.
- **Packaging**: unsigned `Rhapsody.app` + drag-to-Applications dmg; env-gated Developer ID
  signing — **D5**. The signed/notarized dmg + machine install stay David's manual steps.

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
│   │   └── bin/fakedaemon.rs  # D2: symphonyd test stub (≈ internal/supervisor/testdata/fakedaemon)
│   └── tests/              # D2: supervisor lifecycle + gated real-symphonyd smoke
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

`cargo tauri dev` / `cargo tauri build` (the bundler) arrive with packaging (D5); until then the app
is exercised via `cargo build`/`cargo test` and the frontend's vitest suite.

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

The Makefile `app`/`dmg` targets that set these land with packaging (D5).
