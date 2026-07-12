//! Test-only helpers shared across the daemon's unit tests.
//!
//! [`TempDir`] is a hand-rolled temp directory (auto-removed on drop), the stand-in for Go's
//! `t.TempDir()`. The workspace hand-rolls this (as `rhapsody-core`'s runtimeport tests + the
//! orchestrator's `testsupport` do) rather than pull in a `tempfile` dev-dependency.

use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tracing_subscriber::fmt::MakeWriter;

/// A shared in-memory writer capturing the daemon's stderr fan (Go's tests pass a `bytes.Buffer`).
/// Implements both [`Write`] (for the banner + fatal-error lines the boot writes directly) and
/// [`MakeWriter`] (for `telemetry::init`'s fmt layer), the two ways `run` consumes `stderr`. It is
/// cloneable and `Send`/`Sync`/`'static`, so it satisfies `run`'s writer bound and the captured
/// contents survive the daemon tasks.
#[derive(Clone)]
pub struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl SharedBuf {
    pub fn new() -> SharedBuf {
        SharedBuf(Arc::new(Mutex::new(Vec::new())))
    }
    pub fn contents(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap_or_else(|e| e.into_inner())).into_owned()
    }
}

impl Write for SharedBuf {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'w> MakeWriter<'w> for SharedBuf {
    type Writer = SharedBuf;
    fn make_writer(&'w self) -> Self::Writer {
        self.clone()
    }
}

/// A unique temp directory, removed on drop.
pub struct TempDir {
    pub path: PathBuf,
}

impl TempDir {
    pub fn new() -> TempDir {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("rhapsody-rhapsodyd-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&path).expect("create temp dir");
        TempDir { path }
    }

    /// Joins `name` onto the temp directory (a would-be child path; not created).
    pub fn child(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
