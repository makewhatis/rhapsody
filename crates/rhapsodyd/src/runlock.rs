//! runlock — the single-instance daemon guard: an exclusive, non-blocking advisory `flock` keyed on
//! the workflow config path. Parity port of the run-lock section of `$REF/cmd/symphony/run.go`
//! (`acquireSingleInstanceLock` / `canonicalLockPath`).
//!
//! Two daemons for the SAME config (two app copies, or a CLI daemon alongside the app) would each poll
//! Linear with their own in-memory dedup state, so both would dispatch the same ticket — duplicate
//! agents on one issue. An exclusive `flock(LOCK_EX|LOCK_NB)` on a per-config lock file blocks a
//! duplicate of THIS config while letting distinct configs run concurrently; the OS releases it when
//! the process dies (including SIGKILL), so there is no stale-lock cleanup. The returned
//! [`InstanceLock`] must be held (not dropped) for the process lifetime.

use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

/// Holds the open lock file so the advisory `flock` stays held for the process lifetime. Dropping it
/// closes the fd, which releases the lock (the OS also releases it on process exit). Mirrors the Go
/// `*os.File` the daemon `defer lock.Close()`s.
#[derive(Debug)]
pub struct InstanceLock {
    // The fd must stay open to hold the flock; the file is never touched again after locking.
    _file: std::fs::File,
}

/// Why the single-instance lock could not be taken.
#[derive(Debug)]
pub enum LockError {
    /// Another Symphony daemon already holds the lock for THIS config (the `flock` would block).
    /// Mirrors Go `errAnotherInstance`.
    AnotherInstance,
    /// Opening the lock file or the `flock` syscall failed for any other reason.
    Io(std::io::Error),
}

impl std::fmt::Display for LockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // Byte-identical to Go `errAnotherInstance.Error()` (surfaced on stderr as
            // `symphony: another Symphony daemon is already running for this config`).
            LockError::AnotherInstance => {
                write!(
                    f,
                    "another Symphony daemon is already running for this config"
                )
            }
            LockError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for LockError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            LockError::AnotherInstance => None,
            LockError::Io(e) => Some(e),
        }
    }
}

/// Takes an exclusive, non-blocking advisory lock keyed on the workflow config path, so two daemons
/// for the SAME config cannot both run (and thus cannot both dispatch the same ticket). The lock is
/// scoped PER-CONFIG — distinct WORKFLOW.md files run concurrently — and the OS releases it
/// automatically when the process exits. Mirrors Go `acquireSingleInstanceLock`.
pub fn acquire_single_instance_lock(workflow_path: &Path) -> Result<InstanceLock, LockError> {
    // Go: `filepath.Abs(workflowPath)`, falling back to the raw path on error. `std::path::absolute`
    // is the lexical `filepath.Abs` analog (joins the cwd for a relative path); the real symlink /
    // `/var`→`/private/var` resolution happens in `canonical_lock_path`.
    let abs = std::path::absolute(workflow_path).unwrap_or_else(|_| workflow_path.to_path_buf());
    let lock_path = canonical_lock_path(&abs);

    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .open(&lock_path)
        .map_err(LockError::Io)?;

    // SAFETY: `flock(2)` takes a valid open fd + a lock-operation flag and has no other preconditions;
    // `file` owns the fd for the duration of the call (and for the lock's lifetime via `InstanceLock`).
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        // A held lock surfaces as EWOULDBLOCK (== EAGAIN) under LOCK_NB — Go's
        // `errors.Is(err, syscall.EWOULDBLOCK)` → `errAnotherInstance`.
        if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
            return Err(LockError::AnotherInstance);
        }
        return Err(LockError::Io(err));
    }
    Ok(InstanceLock { _file: file })
}

/// Derives a per-config lock path that is stable across different spellings of the SAME file, so a
/// symlinked path, the macOS `/var`→`/private/var` indirection, or a relative-vs-absolute spelling all
/// key on the real target (otherwise two daemons could guard one config with independent flocks and
/// double-dispatch). When the config file exists we resolve the FULL path (this resolves
/// `/var`→`/private/var` AND a config file that is itself a symlink); when the leaf is missing (the
/// lock is taken before the workflow is loaded) we resolve only the parent directory and re-join the
/// leaf, falling back to the abs path when even the parent can't be resolved. Mirrors Go
/// `canonicalLockPath` (`filepath.EvalSymlinks` → `std::fs::canonicalize`).
fn canonical_lock_path(abs: &Path) -> PathBuf {
    if let Ok(resolved) = std::fs::canonicalize(abs) {
        return with_lock_suffix(&resolved);
    }
    if let (Some(parent), Some(leaf)) = (abs.parent(), abs.file_name())
        && let Ok(resolved_dir) = std::fs::canonicalize(parent)
    {
        return with_lock_suffix(&resolved_dir.join(leaf));
    }
    with_lock_suffix(abs)
}

/// Appends the literal `.lock` suffix to a path (Go's string concat `resolved + ".lock"` — a suffix,
/// NOT a `set_extension` that would replace `.md`), so `/w/WORKFLOW.md` → `/w/WORKFLOW.md.lock`.
fn with_lock_suffix(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".lock");
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    // Mirrors Go `TestSingleInstanceLockBlocksSecondDaemon`: a second daemon for the SAME config is
    // rejected, and the lock frees once the holder releases it.
    #[test]
    fn single_instance_lock_blocks_second_daemon() {
        let dir = TempDir::new();
        let wf = dir.child("WORKFLOW.md");

        let l1 = acquire_single_instance_lock(&wf).expect("first lock should succeed");
        match acquire_single_instance_lock(&wf) {
            Err(LockError::AnotherInstance) => {}
            other => panic!("a second daemon for the same config must be rejected, got: {other:?}"),
        }
        drop(l1); // release
        let l2 = acquire_single_instance_lock(&wf).expect("lock after release should succeed");
        drop(l2);
    }

    // Mirrors Go `TestSingleInstanceLockCanonicalizesSpelling`: two spellings of the SAME config file
    // (via a symlinked parent directory) must take the SAME lock. The macOS `/var`→`/private/var` case
    // is the same `canonicalize` code path and needs no platform-specific test.
    #[test]
    fn single_instance_lock_canonicalizes_spelling() {
        let real = TempDir::new();
        let link_parent = TempDir::new();
        let link = link_parent.child("link");
        std::os::unix::fs::symlink(&real.path, &link).expect("symlink");
        let direct = real.path.join("WORKFLOW.md");
        let via_link = link.join("WORKFLOW.md");

        let _l1 = acquire_single_instance_lock(&direct).expect("first lock should succeed");
        match acquire_single_instance_lock(&via_link) {
            Err(LockError::AnotherInstance) => {}
            other => {
                panic!("same file via a symlinked path must hit the same lock, got: {other:?}")
            }
        }
    }

    // Mirrors Go `TestSingleInstanceLockResolvesFileSymlink`: a config file that is itself a symlink to
    // a file in a DIFFERENT real directory must take the SAME lock as its target.
    #[test]
    fn single_instance_lock_resolves_file_symlink() {
        let target_dir = TempDir::new();
        let target = target_dir.child("WORKFLOW.md");
        std::fs::write(&target, b"workflow").expect("write target");
        let link_dir = TempDir::new(); // separate real dir
        let link = link_dir.child("WORKFLOW.md");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");

        let _l1 =
            acquire_single_instance_lock(&link).expect("first lock via symlink should succeed");
        match acquire_single_instance_lock(&target) {
            Err(LockError::AnotherInstance) => {}
            other => panic!("a file-symlink and its target must hit the same lock, got: {other:?}"),
        }
    }

    // Mirrors Go `TestSingleInstanceLockDistinctConfigsCoexist`: the lock is per-config, so daemons for
    // different WORKFLOW.md files run concurrently.
    #[test]
    fn single_instance_lock_distinct_configs_coexist() {
        let dir = TempDir::new();
        let _a = acquire_single_instance_lock(&dir.child("a.md")).expect("lock a");
        let _b = acquire_single_instance_lock(&dir.child("b.md"))
            .expect("a distinct config must run concurrently");
    }
}
