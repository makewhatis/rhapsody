//! rhapsody-desktop library — the P7-D2 sidecar layer the Tauri app (D3+) composes.
//!
//! Parity port of the Go/Wails shell's daemon plumbing:
//!   - [`supervisor`] — launch `rhapsodyd` on an explicit `--port` + a known-good PATH, poll
//!     `/healthz` for readiness, restart on crash with backoff, and drain on SIGTERM. Ports
//!     `$REF/desktop/internal/supervisor` (`supervisor.go` + `env.go` + `resolve.go`).
//!   - [`tooldirs`] — the daemon's agent-launch PATH dirs (per-tool override dirs first, then the
//!     known-good defaults). Ports `$REF/desktop/app.go`'s `agentToolDirs` + `$REF/desktop/internal/
//!     toolcheck/dirs.go`'s `OverrideDirs`.
//!   - [`apiproxy`] — the same-origin reverse proxy that forwards the app's `/api/*` + `/healthz`
//!     fetches to the supervised sidecar. Ports `$REF/desktop/apiproxy.go`.
//!
//! These are `pub` library API rather than app-internal modules: the D3 tray/lifecycle task wires
//! them into the Tauri `App`, exactly as `$REF/desktop/app.go` + `main.go` consume the Go packages.
//! The bin ([`main`](../rhapsody_desktop/index.html)) keeps its own D1 window-shell modules
//! (`status`, `version`).

pub mod apiproxy;
pub mod app;
pub(crate) mod atomicfile;
pub mod credential;
pub mod linearoauth;
pub mod linearprojects;
pub mod logbridge;
pub mod menu;
pub mod onboarding;
pub mod prefs;
pub mod supervisor;
pub mod toolcheck;
pub mod tooldirs;
pub mod update;
pub mod windowserver;
