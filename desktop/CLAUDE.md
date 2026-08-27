# CLAUDE.md — desktop

You must `cd desktop` yourself before building, testing, or linting anything here — nothing at
repo root does it for you. Full narrative docs already live in `README.md` (layout, packaging,
version stamping, auto-update) and `SIGNING.md` (Developer ID signing/notarization runbook); this
file only adds what those don't cover. Read them before this if you need the "what"/"why" — this
file is the "how to not break it."

## Build / lint / test (run from inside `desktop/`, not repo root)

```sh
cd desktop
cargo fmt --all --check                                  # own fmt gate, not covered by root `make lint`
cargo clippy --workspace --all-targets -- -D warnings     # own clippy gate
cargo test                                                 # unit + supervisor-lifecycle + packaging-gate
```

Plain `cargo build` / `cargo test` work on a clean checkout even though `frontendDist`
(`../crates/httpapi/web-dist`) is git-ignored — the committed `.gitkeep` anchor is enough for
`tauri::generate_context!` to embed an empty dist. You only need a real `cd web && npm run build`
first if you're going to *run* the app (`cargo tauri dev`) and expect a working dashboard.

Two test files are **gated off by default** (`cargo test` skips them silently, no failure) because
they need artifacts `cargo test` alone doesn't produce:
- `tests/real_rhapsodyd_smoke.rs` — needs `RHAPSODY_SMOKE_RHAPSODYD=1` + a real release `rhapsodyd`.
- `tests/parity_e2e.rs` — needs `RHAPSODY_PARITY_E2E=1` + a `make app` bundle already built (reads
  `Contents/Resources/rhapsodyd` from the packaged `.app`).

Non-obvious: `scripts/render_cask_test.sh` and `scripts/notarize_args_test.sh` are pure-bash tests
with no cargo harness of their own, but they're *not* orphaned — `tests/packaging_gate.rs` shells
out to them, so plain `cargo test` does exercise them. Don't assume "no `.rs` test file references
this script" means it's untested.

## Non-obvious layout facts

- **`src-tauri/`** is the actual Rust crate (`rhapsody-desktop`). Read `src/lib.rs`'s module doc
  comment for the module map before editing — each module's own top comment names its exact Go
  source file under `$REF/desktop`, extending the crate-map doc-comment convention from root
  CLAUDE.md into this workspace.
- **`scripts/`** and **`build/`** hold non-Rust packaging inputs (bash helpers, entitlements,
  `make dmg`'s `.dmg` output) — nothing there needs cargo. Read the files (and `build/README.md`)
  directly rather than relying on a directory map here.

## Pitfalls specific to this crate

- **Two bin targets, one default.** The crate builds both `rhapsody-desktop` (the real app) and
  `fakedaemon` (a supervisor test stub). `default-run = "rhapsody-desktop"` in `src-tauri/Cargo.toml`
  exists *only* so `cargo tauri build` embeds the right one — `cargo tauri build` bundles whatever
  binary cargo treats as default, and without that pin it picks `fakedaemon` alphabetically,
  producing a broken `.app`. If you add a third bin, re-check this.
- **`fakedaemon` feature is ON by default.** A plain `cargo build`/`cargo test`/`cargo clippy`
  compiles the stub because the supervisor + e2e tests need `CARGO_BIN_EXE_fakedaemon` at compile
  time. The packaging build (`make app`, root Makefile) explicitly passes `--no-default-features` to
  drop it — the Tauri bundler copies *every* built bin into `Contents/MacOS/`, so leaving it on would
  ship an unsigned stray Mach-O inside the app bundle and break hardened-runtime signing.
- **Debugging dashboard rendering bugs:** `frontendDist` here is the same build output root
  CLAUDE.md's `web/` bullet describes the daemon embedding too. If a rendering bug reproduces in
  both the desktop app and `rhapsodyd`'s own web UI, it's almost certainly in `web/`, not here —
  don't go looking for a second copy of the frontend in `desktop/`.
