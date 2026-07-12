//! Smoke test: supervise the REAL release-built `rhapsodyd` (the D2 acceptance's "smoke against the
//! real release-built rhapsodyd"). Gated behind `RHAPSODY_SMOKE_RHAPSODYD=1` so the required
//! `desktop` CI job (a plain `cargo test`) does not shell out to a second `cargo build` on the shared
//! runner (which would contend with the root `test` job for the root workspace's target dir).
//!
//! It builds `rhapsodyd` in release mode from the repo-root workspace, resolves it exactly as the app
//! would, and drives the supervisor against it:
//!   - Once P6-F1 makes rhapsodyd a `/healthz`-serving daemon on `--port`, the smoke asserts the full
//!     start -> healthy -> stop lifecycle.
//!   - Until then rhapsodyd is the P0 stub (prints its version and exits; no `--port`/`/healthz`), so
//!     the smoke instead asserts the supervisor drives the REAL binary to a clean terminal `Stopped`
//!     state (launch + resolve + restart-budget path exercised end to end, no hang/panic).
//!
//! Run: `RHAPSODY_SMOKE_RHAPSODYD=1 cargo test --test real_rhapsodyd_smoke -- --nocapture`.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use rhapsody_desktop::supervisor::{Options, State, Supervisor, resolve_binary};

#[tokio::test]
async fn smoke_supervises_real_release_rhapsodyd() {
    if std::env::var_os("RHAPSODY_SMOKE_RHAPSODYD").is_none() {
        eprintln!(
            "skip: set RHAPSODY_SMOKE_RHAPSODYD=1 to run the real-rhapsodyd smoke (builds the \
             release rhapsodyd and supervises it)"
        );
        return;
    }

    // CARGO_MANIFEST_DIR is .../desktop/src-tauri; the repo-root workspace is two levels up.
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("resolve repo root");
    let manifest = root.join("Cargo.toml");

    // Build the RELEASE rhapsodyd from the root workspace (a distinct target dir from desktop's).
    let status = Command::new(env!("CARGO"))
        .args(["build", "--release", "-p", "rhapsodyd", "--manifest-path"])
        .arg(&manifest)
        .status()
        .expect("run cargo build");
    assert!(
        status.success(),
        "cargo build --release -p rhapsodyd failed"
    );

    let bin = root.join("target/release/rhapsodyd");
    assert!(
        bin.exists(),
        "release rhapsodyd missing at {}",
        bin.display()
    );

    // Resolving the real binary proves the resolve path works against the actual sidecar.
    let resolved =
        resolve_binary(bin.to_str().expect("utf-8 path"), "").expect("resolve real rhapsodyd");

    let sup = Supervisor::new(Options {
        binary_path: resolved,
        // Keep the real PATH so a healthz-serving daemon can find its tools.
        base_env: Some(vec![format!(
            "PATH={}",
            std::env::var("PATH").unwrap_or_default()
        )]),
        startup_timeout: Duration::from_secs(3),
        max_restarts: 1,
        ..Default::default()
    });

    match sup.start(tokio::time::sleep(Duration::from_secs(20))).await {
        Ok(()) => {
            // Post-F1: rhapsodyd serves /healthz on --port.
            assert_eq!(
                sup.status().state,
                State::Running,
                "want Running once healthy"
            );
            assert!(sup.healthy().await, "real rhapsodyd must answer /healthz");
            sup.stop().await;
            assert_eq!(sup.status().state, State::Stopped, "clean stop");
            eprintln!("smoke OK: real rhapsodyd went start -> healthy -> stop");
        }
        Err(e) => {
            // Pre-F1 stub: rhapsodyd exits immediately. The supervisor must reach a clean terminal
            // Stopped state with the failure surfaced — the launch/resolve/restart machinery is
            // exercised against the REAL binary even though it can never become healthy yet.
            assert_eq!(
                sup.status().state,
                State::Stopped,
                "supervisor must reach terminal Stopped against the real binary"
            );
            assert!(
                !sup.status().last_err.is_empty(),
                "the launch failure must be surfaced via Status.last_err"
            );
            eprintln!(
                "smoke OK (pre-F1): real rhapsodyd is still the P0 stub (no /healthz); supervisor \
                 reached terminal Stopped, last_err = {:?}; err = {e}",
                sup.status().last_err
            );
        }
    }
}
