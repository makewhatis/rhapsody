//! Shared test helpers — the Rust analogue of the package-level helpers in
//! `$REF/internal/mcpfacade/*_test.go` (`clientForServer`, `connectInMemory`, `resultText`).
//! Compiled only under `cfg(test)`.

use crate::client::Client;
use axum::Router;
use rhapsody_config::Config;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static TEST_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

/// RAII temp directory mirroring Go's `t.TempDir()` (unique per pid+counter, auto-removed). The
/// crate avoids a `tempfile` dependency, matching `rhapsody-store` / `rhapsody-workspace`.
pub(crate) struct TempDir {
    pub path: PathBuf,
}

impl TempDir {
    pub(crate) fn new() -> TempDir {
        let n = TEST_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("rhapsody-mcp-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(&path).unwrap();
        TempDir { path }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Spawns `router` on a fresh loopback port and returns the bound port — the analogue of Go's
/// `httptest.NewServer`. A [`Client::for_port`] on the returned port dials exactly this stub (its
/// base is `http://127.0.0.1:<port>`), so no base override is needed.
pub(crate) async fn spawn_router(router: Router) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    port
}

/// A [`Client`] pointed at a stub bound to `port` (the mirror of Go's `clientForServer`).
pub(crate) fn client_for_port(port: u16) -> Client {
    Client::for_port(port as i64)
}

/// A defaulted [`Config`] — the analogue of Go's zero-value `&config.Config{}` (only the fields a
/// given test reads matter; the rest carry `decode`'s defaults).
pub(crate) fn test_config() -> Config {
    use rhapsody_config::workflow::{Definition, YamlMap};
    rhapsody_config::decode(&Definition {
        config: YamlMap::new(),
        prompt_template: String::new(),
    })
    .expect("decode default workflow")
}
