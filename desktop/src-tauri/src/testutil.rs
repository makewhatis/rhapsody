//! Shared test scaffolding (compiled only under `cfg(test)`).
//!
//! [`TempDir`] is this workspace's hand-rolled `t.TempDir()` — no `tempfile` dev-dependency, matching
//! the rest of the workspace. It is the single fix for STUDIO-1031's scratch-dir leak: every module's
//! old `fn temp_dir() -> PathBuf` created a directory nothing ever removed, so `$TMPDIR` accumulated
//! `rhapsody-*` entries across runs. A guard removes it on drop, its name carries a nanosecond nonce
//! beside the pid+counter so a recycled pid can never adopt an earlier run's name, and
//! `RHAPSODY_KEEP_TEST_DIRS=1` keeps it for debugging.
//!
//! It `Deref`s to `Path` so an existing `let dir = temp_dir(); dir.join(..)` keeps compiling.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A scratch directory under `base`, removed on drop.
pub(crate) struct TempDir {
    path: PathBuf,
}

impl TempDir {
    /// A fresh scratch dir under the OS temp dir with the given `prefix`.
    pub(crate) fn new(prefix: &str) -> TempDir {
        TempDir::new_in(&std::env::temp_dir(), prefix)
    }

    /// A fresh scratch dir under an explicit `base`. `credential_bootstrap`'s unix-socket tests use
    /// `/tmp` directly: `sun_path` is capped at ~104 bytes and the per-session macOS `$TMPDIR` alone
    /// can be most of that.
    pub(crate) fn new_in(base: &Path, prefix: &str) -> TempDir {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = base.join(format!("{prefix}-{}-{n}-{nonce}", std::process::id()));
        std::fs::create_dir_all(&path).expect("create temp dir");
        TempDir { path }
    }
}

impl std::ops::Deref for TempDir {
    type Target = Path;
    fn deref(&self) -> &Self::Target {
        &self.path
    }
}

impl AsRef<Path> for TempDir {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        if std::env::var_os("RHAPSODY_KEEP_TEST_DIRS").is_none() {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}
