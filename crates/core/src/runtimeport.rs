//! runtimeport — parity port of Go `internal/runtimeport`: publishes and reads the daemon's
//! live loopback HTTP port.
//!
//! The daemon's observability/API server can bind a port that does NOT match `server.port` in
//! WORKFLOW.md: the desktop app launches the daemon with a dynamic `--port <n>` (and `--port 0` asks
//! the OS for an ephemeral port). `symphony mcp` — both an operator's CLI and the workers the daemon
//! injects — must dial the REAL port to reach the daemon, so the daemon writes it here at startup
//! (after the listener binds) and removes it on clean shutdown; `symphony mcp` reads it and prefers
//! it over the stale config port (INF-473).
//!
//! The file lives at a single well-known path (`~/.rhapsody/runtime.json`) so writer and reader
//! agree with no configuration. (TRA-238: Rhapsody's runtime home is `~/.rhapsody`, an intentional
//! divergence from Go v0.4.0's `~/.symphony` — the Rust daemon and the Rust `rhapsodyd mcp` reader
//! both use `.rhapsody`, so they still agree.) This assumes ONE daemon per machine, which the daemon's
//! single-instance flock enforces per config; concurrent daemons for distinct configs share this one
//! file (last writer wins), so a `symphony mcp` for a non-owning config falls back to its config
//! `server.port` — an accepted limitation for that uncommon setup.
//!
//! Consumed by the P6 httpapi write handlers (H3, runtime.json publication on bind) and the mcp
//! facade (M, port discovery). Mirrors `$REF/internal/runtimeport/runtimeport.go`.

use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

/// The runtime file's name under the Symphony home directory. Mirrors Go `runtimeport.FileName`.
pub const FILE_NAME: &str = "runtime.json";

/// The daemon's published runtime state. `port` is the actual bound loopback port; `pid` identifies
/// the writing daemon (for debugging). Unknown fields are ignored on read, so future additions are
/// non-breaking. Mirrors Go `runtimeport.Info`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Info {
    /// The actual bound loopback port.
    pub port: i32,
    /// PID of the writing daemon.
    pub pid: i32,
}

/// Go `os.UserHomeDir` (Unix branch): `$HOME` when set and non-empty. The daemon targets macOS/Linux
/// (the sole platforms it runs on), so only the Unix path is ported; an undiscoverable home is an
/// error, mirroring Go `UserHomeDir`'s `$HOME is not defined`.
fn home_dir() -> io::Result<PathBuf> {
    match std::env::var_os("HOME") {
        Some(h) if !h.is_empty() => Ok(PathBuf::from(h)),
        _ => Err(io::Error::new(
            io::ErrorKind::NotFound,
            "$HOME is not defined",
        )),
    }
}

/// `<home>/.rhapsody/runtime.json` — the same durable home the DB and logs default to (TRA-238).
fn runtime_path_in(home: &Path) -> PathBuf {
    home.join(".rhapsody").join(FILE_NAME)
}

/// Returns `~/.rhapsody/runtime.json`. Mirrors Go `Path` (rebranded home, TRA-238).
pub fn path() -> io::Result<PathBuf> {
    Ok(runtime_path_in(&home_dir()?))
}

/// Publishes the daemon's live loopback port, creating `~/.rhapsody` if needed and recording the
/// current PID. Writes atomically (unique temp file + rename) so a concurrent reader never observes a
/// torn file, and overwrites any existing record (a fresh daemon supersedes a stale one). Mirrors Go
/// `Write`.
pub fn write(port: i32) -> io::Result<()> {
    write_in(&home_dir()?, port)
}

/// Returns the published runtime info. A missing file surfaces as an `io::ErrorKind::NotFound` error
/// (Go's `os.IsNotExist`), which the caller treats as "no runtime port — fall back to config
/// `server.port`". Mirrors Go `Read`.
pub fn read() -> io::Result<Info> {
    read_in(&home_dir()?)
}

/// Deletes the runtime file on clean daemon shutdown, but ONLY when THIS process still owns it (the
/// recorded PID matches the current PID). Distinct workflow configs can run concurrent daemons that
/// share this one file (last writer wins); without the ownership check, one daemon's shutdown would
/// delete the file a still-running survivor had since overwritten, stripping its port discovery until
/// it restarts (Bugbot). A missing file, a file now owned by another PID, or an unreadable/corrupt
/// file are all left as-is and return `Ok` — there is nothing of OURS to clean up. Mirrors Go
/// `Remove`.
pub fn remove() -> io::Result<()> {
    remove_in(&home_dir()?)
}

/// Reports whether the process that published the runtime file is still running, so a STALE file left
/// by a crashed / `kill -9`'d daemon (never `remove`d) doesn't pin `symphony mcp` to a dead port —
/// the caller falls back to `config.server.port` instead. A PID ≤ 0, or an unknown/dead process,
/// returns `false`; `signal 0` on a live process we may not own returns `EPERM` (still "alive"). PID
/// reuse is possible but harmless (worst case: dialing a dead port). Mirrors Go `ProcessAlive`.
pub fn process_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    // signal 0 delivers nothing; kill(2) only performs the permission/existence check. 0 → alive;
    // EPERM → alive but owned by another user; anything else (ESRCH) → dead.
    // SAFETY: `kill(2)` with signal 0 has no preconditions and delivers no signal; `pid > 0` is
    // enforced above, so it targets a single process, never a group.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

// --- internal, home-parameterized core -------------------------------------------------------
//
// The public `write`/`read`/`remove` resolve `$HOME`; these take the home directory explicitly so
// the tests drive them against a per-test temp home — never touching a live daemon's
// ~/.rhapsody/runtime.json, and running race-free without mutating process-global env (the same
// directory-injection the sibling `obslog::Store::new(dir)` / `liveness::group_cpu(root, …)` ports
// use in place of Go's `t.Setenv`).

/// Process-global counter feeding unique temp-file names (mirrors the `rhapsody_config` `workflow`
/// atomic-write convention).
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn write_in(home: &Path, port: i32) -> io::Result<()> {
    let dir = home.join(".rhapsody");
    // The tree defaults to owner-only (the DB and transcripts under it may hold secrets): dir 0700.
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)?;
    let info = Info {
        port,
        pid: std::process::id() as i32,
    };
    let data =
        serde_json::to_vec(&info).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let final_path = dir.join(FILE_NAME);
    let (file, tmp_path) = create_temp(&dir)?;
    match write_temp_and_rename(file, &tmp_path, &data, &final_path) {
        Ok(()) => Ok(()),
        Err(e) => {
            // Best-effort cleanup if we bailed before the rename succeeded.
            let _ = std::fs::remove_file(&tmp_path);
            Err(e)
        }
    }
}

/// Creates a uniquely named `runtime.json.<pid>-<n>.tmp` in `dir`, owner-only (0600), mirroring Go's
/// `os.CreateTemp(dir, FileName+".*.tmp")`: two daemons for distinct configs share the final path
/// (last writer wins), and a per-writer temp (pid prefix) keeps their renames genuinely atomic
/// instead of clobbering a shared `<p>.tmp` mid-write. `create_new` (`O_EXCL`) is the collision
/// guard.
fn create_temp(dir: &Path) -> io::Result<(std::fs::File, PathBuf)> {
    let pid = std::process::id();
    for _ in 0..10_000u64 {
        let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let candidate = dir.join(format!("{FILE_NAME}.{pid}-{n}.tmp"));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&candidate)
        {
            Ok(file) => return Ok((file, candidate)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not create a unique temp file",
    ))
}

/// Writes `data` to the temp file, forces mode 0600 (Go's explicit `Chmod`, umask-independent), then
/// renames it over `dest`. The handle is closed before chmod/rename, mirroring Go `Write`.
fn write_temp_and_rename(
    mut file: std::fs::File,
    tmp_path: &Path,
    data: &[u8],
    dest: &Path,
) -> io::Result<()> {
    file.write_all(data)?;
    drop(file); // close before chmod + rename
    std::fs::set_permissions(tmp_path, std::fs::Permissions::from_mode(0o600))?;
    std::fs::rename(tmp_path, dest)?;
    Ok(())
}

fn read_in(home: &Path) -> io::Result<Info> {
    let data = std::fs::read(runtime_path_in(home))?;
    serde_json::from_slice(&data).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

fn remove_in(home: &Path) -> io::Result<()> {
    let info = match read_in(home) {
        Ok(i) => i,
        // Missing (NotFound), or unreadable/corrupt: nothing we can confirm as ours to remove.
        Err(_) => return Ok(()),
    };
    if info.pid != std::process::id() as i32 {
        // A newer daemon (different config) owns the file now — leave it for them.
        return Ok(());
    }
    match std::fs::remove_file(runtime_path_in(home)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A per-test temp home dir, auto-removed on drop. Stands in for Go's `isolateHome` (`t.Setenv`)
    /// but injects the home explicitly (see the module note) so the tests never mutate process env
    /// and can run in parallel.
    struct TempHome {
        path: PathBuf,
    }

    impl TempHome {
        fn new() -> TempHome {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("rhapsody-runtimeport-{}-{n}", std::process::id()));
            std::fs::create_dir_all(&path).expect("create temp home");
            TempHome { path }
        }

        fn runtime_file(&self) -> PathBuf {
            self.path.join(".rhapsody").join(FILE_NAME)
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    // Mirrors Go `TestWriteReadRemoveRoundTrip`: the file lands at ~/.rhapsody/runtime.json, reads
    // back the port + writing PID, and Read after Remove is NotFound.
    #[test]
    fn write_read_remove_round_trip() {
        let home = TempHome::new();
        write_in(&home.path, 51981).expect("write");
        assert!(home.runtime_file().exists(), "runtime file must exist");

        let info = read_in(&home.path).expect("read");
        assert_eq!(info.port, 51981);
        assert_eq!(info.pid, std::process::id() as i32);

        remove_in(&home.path).expect("remove");
        let err = read_in(&home.path).expect_err("read after remove");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    // Mirrors Go `TestReadMissingIsNotExist`.
    #[test]
    fn read_missing_is_not_found() {
        let home = TempHome::new();
        let err = read_in(&home.path).expect_err("read on missing file");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    // Mirrors Go `TestRemoveMissingIsNoError`.
    #[test]
    fn remove_missing_is_no_error() {
        let home = TempHome::new();
        remove_in(&home.path).expect("remove on missing file is a no-op");
    }

    // Mirrors Go `TestWriteOverwrites`: a fresh daemon supersedes a stale record.
    #[test]
    fn write_overwrites() {
        let home = TempHome::new();
        write_in(&home.path, 1111).expect("write");
        write_in(&home.path, 2222).expect("overwrite");
        assert_eq!(read_in(&home.path).expect("read").port, 2222);
    }

    // Mirrors Go `TestProcessAlive`: self is alive; non-positive PIDs and an (almost certainly dead)
    // max-int PID are not.
    #[test]
    fn process_alive_cases() {
        assert!(process_alive(std::process::id() as i32), "self is alive");
        assert!(!process_alive(0));
        assert!(!process_alive(-1));
        // `1<<31 - 1`: never a real PID (macOS caps ~99998, Linux at pid_max), so it reads as dead —
        // exactly how a stale runtime.json from a crashed daemon is detected.
        assert!(!process_alive(i32::MAX));
    }

    // Mirrors Go `TestRemoveLeavesFileOwnedByAnotherProcess`: Remove must NOT delete a runtime file a
    // DIFFERENT daemon (distinct config) now owns.
    #[test]
    fn remove_leaves_file_owned_by_another_process() {
        let home = TempHome::new();
        let dir = home.path.join(".rhapsody");
        std::fs::create_dir_all(&dir).expect("mkdir .rhapsody");
        let other = std::process::id() as i32 + 1;
        std::fs::write(
            dir.join(FILE_NAME),
            format!(r#"{{"port":51981,"pid":{other}}}"#),
        )
        .expect("write other's file");

        remove_in(&home.path).expect("remove");
        assert!(
            home.runtime_file().exists(),
            "must not delete a file owned by pid {other}"
        );
    }

    // Mirrors Go `TestRemoveDeletesOwnFile`: the common single-daemon shutdown removes our own file.
    #[test]
    fn remove_deletes_own_file() {
        let home = TempHome::new();
        write_in(&home.path, 51981).expect("write"); // records our pid
        remove_in(&home.path).expect("remove");
        let err = read_in(&home.path).expect_err("read after remove of own file");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }
}
