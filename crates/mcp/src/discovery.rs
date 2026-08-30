//! Read-side runtime.json port discovery — the daemon-port resolution `symphony mcp` uses to dial
//! the daemon. Parity port of `$REF/cmd/symphony/mcp.go`'s `daemonPort` + the READ side of
//! `$REF/internal/runtimeport/runtimeport.go` (`Read` / `Path` / `ProcessAlive`).
//!
//! A running daemon publishes its live loopback port to `~/.rhapsody/runtime.json` (TRA-238: the
//! Rhapsody runtime home, diverging from Go v0.4.0's `~/.symphony`; the Rust daemon writes and this
//! reader reads the same `.rhapsody` path) — which reflects
//! a dynamic/ephemeral `--port` (the desktop app's launch mode) — so that wins when present AND its
//! writer is still alive; otherwise the config `server.port` (a fixed-port CLI daemon) is used. The
//! WRITE side (publish/remove, atomic-rename semantics) is the infra lane's (T1); the facade only
//! reads. This is the sole disk access the facade performs — all run state still comes from the
//! daemon's loopback API (INF-473).

use crate::client::port_from_config;
use rhapsody_config::Config;
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// The runtime file's name under the daemon's home directory (runtimeport.go's `FileName`).
const FILE_NAME: &str = "runtime.json";

/// The daemon's published runtime state (runtimeport.go's `Info`). `port` is the actual bound
/// loopback port; `pid` identifies the writing daemon. Unknown fields are ignored on read, so
/// future additions are non-breaking.
#[derive(Debug, Clone, Default, Deserialize)]
struct RuntimeInfo {
    #[serde(default)]
    port: i64,
    #[serde(default)]
    pid: i64,
}

/// `<home>/.rhapsody/runtime.json` (runtimeport.go's `Path`, parameterized on the home dir; the
/// rebranded Rhapsody home, TRA-238 — kept in lockstep with `rhapsody_core::runtimeport`).
fn runtime_path_in(home: &Path) -> PathBuf {
    home.join(".rhapsody").join(FILE_NAME)
}

/// Reads the published runtime info under `home` (the read core of runtimeport.go's `Read`). `None`
/// when the file is missing, unreadable, or not valid JSON — the caller falls back to config.
fn read_runtime_info_in(home: &Path) -> Option<RuntimeInfo> {
    let bytes = std::fs::read(runtime_path_in(home)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// The daemon's home directory — `$HOME`, matching Go's `os.UserHomeDir()` on Unix (the daemon /
/// desktop app target). `None` when `$HOME` is unset/empty, so discovery falls back to config.
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

/// Whether the process that published the runtime file is still running (runtimeport.go's
/// `ProcessAlive`), so a STALE file left by a crashed / kill -9'd daemon doesn't pin `symphony mcp`
/// to a dead port — the caller falls back to config `server.port` instead. A PID ≤ 0 or an
/// unknown/dead process returns false. On Unix this is `kill(pid, 0)`: success — or EPERM ("alive,
/// just not ours") — means alive. PID reuse is possible but harmless.
fn process_alive(pid: i64) -> bool {
    if pid <= 0 {
        return false;
    }
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    // SAFETY: `kill` with signal 0 performs no action — it only probes existence/permission and
    // touches no memory. Mirrors Go's `proc.Signal(syscall.Signal(0))`.
    let rc = unsafe { libc::kill(pid, 0) };
    if rc == 0 {
        return true;
    }
    // EPERM: the process exists but is owned by another user — still alive.
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Resolves the loopback port `symphony mcp` should dial (mcp.go's `daemonPort`): a running
/// daemon's published runtime port when present AND its writer is still alive, else the config
/// `server.port`. Returns 0 when neither is available, so the client surfaces a clear
/// `daemon_unreachable` error rather than dialing nothing.
pub fn resolve_daemon_port(cfg: &Config) -> i64 {
    resolve_with(cfg, home_dir().as_deref())
}

/// The pure resolution (mcp.go's `daemonPort` body), parameterized on the home dir so it is
/// testable without mutating the process environment.
fn resolve_with(cfg: &Config, home: Option<&Path>) -> i64 {
    // Trust the published port only when it is valid AND its writer is still alive.
    if let Some(home) = home
        && let Some(info) = read_runtime_info_in(home)
        && info.port > 0
        && process_alive(info.pid)
    {
        return info.port;
    }
    port_from_config(cfg)
}

#[cfg(test)]
mod tests {
    //! Read-side mirror of `$REF/internal/runtimeport/runtimeport_test.go` (`TestProcessAlive`,
    //! `TestReadMissingIsNotExist`) + the `daemonPort` resolution (mcp.go).
    use super::*;
    use crate::testutil::{TempDir, test_config};

    /// Writes `~/.rhapsody/runtime.json` under `home` with the given port + pid.
    fn write_runtime(home: &Path, port: i64, pid: i64) {
        let dir = home.join(".rhapsody");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("runtime.json"),
            format!(r#"{{"port":{port},"pid":{pid}}}"#),
        )
        .unwrap();
    }

    #[test]
    fn process_alive_taxonomy() {
        // The current process is alive.
        assert!(process_alive(std::process::id() as i64));
        // Non-positive PIDs are never alive.
        assert!(!process_alive(0));
        assert!(!process_alive(-1));
        // A PID that is almost certainly not a running process (max i32) is dead — how a stale
        // runtime.json from a crashed daemon is detected.
        assert!(!process_alive((1i64 << 31) - 1));
    }

    #[test]
    fn read_missing_is_none() {
        let home = TempDir::new();
        assert!(read_runtime_info_in(home.path()).is_none());
    }

    #[test]
    fn read_round_trips() {
        let home = TempDir::new();
        write_runtime(home.path(), 51981, std::process::id() as i64);
        let info = read_runtime_info_in(home.path()).expect("read");
        assert_eq!(info.port, 51981);
        assert_eq!(info.pid, std::process::id() as i64);
    }

    #[test]
    fn resolve_prefers_live_runtime_port() {
        // A published port whose writer is alive (this process) wins over config.
        let home = TempDir::new();
        write_runtime(home.path(), 51981, std::process::id() as i64);
        let mut cfg = test_config();
        cfg.server.port = Some(8799);
        assert_eq!(resolve_with(&cfg, Some(home.path())), 51981);
    }

    #[test]
    fn resolve_falls_back_on_stale_runtime() {
        // A published port whose writer is DEAD (max-i32 pid) must not pin discovery — fall back to
        // config server.port.
        let home = TempDir::new();
        write_runtime(home.path(), 51981, (1i64 << 31) - 1);
        let mut cfg = test_config();
        cfg.server.port = Some(8799);
        assert_eq!(resolve_with(&cfg, Some(home.path())), 8799);
    }

    #[test]
    fn resolve_falls_back_when_no_runtime() {
        // No runtime.json at all ⇒ config server.port.
        let home = TempDir::new();
        let mut cfg = test_config();
        cfg.server.port = Some(8799);
        assert_eq!(resolve_with(&cfg, Some(home.path())), 8799);
        // …and 0 when config is unset too (⇒ daemon_unreachable at request time).
        let cfg0 = test_config();
        assert_eq!(resolve_with(&cfg0, Some(home.path())), 0);
    }
}
