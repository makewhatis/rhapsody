//! Stores the Linear API token the daemon uses as its tracker credential (spec §7). Parity port of
//! `$REF/desktop/internal/credential/credential.go`.
//!
//! The primary v1 path is the macOS Keychain (via the `keyring` crate, whose `apple-native` backend
//! works in an unsigned build); a 0600 file under `~/.symphony` is the documented fallback for a
//! machine where Keychain access has friction (design §5). Both back the same [`Store`] trait so the
//! app composes them (Keychain first, file fallback) exactly as the Go `App` does.
//!
//! Tests inject an in-memory keychain double ([`mock`]) rather than the process-global provider the Go
//! tests swap via `keyring.MockInit` — a per-value seam keeps the port parallel-safe.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

/// The error a [`Store`] operation can return. The desktop crate has no `anyhow`; a boxed std error
/// mirrors Go's opaque `error` return while preserving the underlying cause's message for the UI.
pub type Error = Box<dyn std::error::Error + Send + Sync>;

/// Reads, writes, and clears a single secret (the Linear API token). [`Store::get`] returns "" (not an
/// error) when the secret is unset. Mirrors the Go `credential.Store` interface.
pub trait Store: Send + Sync {
    fn get(&self) -> Result<String, Error>;
    fn set(&self, token: &str) -> Result<(), Error>;
    fn delete(&self) -> Result<(), Error>;
    /// Names the active storage ("keychain" or "file") for the UI to surface.
    fn backend(&self) -> &'static str;
}

// ---- Keychain ------------------------------------------------------------------------------------

/// Outcome of a low-level keychain operation. `NoEntry` is the "absent" signal [`Keychain`] maps to an
/// empty read / idempotent delete (Go's `keyring.ErrNotFound`); `Other` is any real failure.
enum KeyringError {
    NoEntry,
    Other(Error),
}

/// The macOS Keychain operations [`Keychain`] needs, behind a trait so tests inject an in-memory
/// double instead of touching the real login keychain (the Go tests use `keyring.MockInit`).
trait Keyring: Send + Sync {
    fn get_password(&self) -> Result<String, KeyringError>;
    fn set_password(&self, token: &str) -> Result<(), KeyringError>;
    fn delete_credential(&self) -> Result<(), KeyringError>;
}

/// The production [`Keyring`], backed by the OS Keychain via the `keyring` crate. Service/account
/// namespace the item; a fresh `Entry` per call is cheap and points at the same login-keychain item,
/// matching Go's repeated `keyring.Get/Set/Delete(service, account)`.
struct OsKeyring {
    service: String,
    account: String,
}

impl OsKeyring {
    fn entry(&self) -> Result<keyring::Entry, KeyringError> {
        keyring::Entry::new(&self.service, &self.account).map_err(map_keyring_err)
    }
}

fn map_keyring_err(e: keyring::Error) -> KeyringError {
    match e {
        keyring::Error::NoEntry => KeyringError::NoEntry,
        other => KeyringError::Other(Box::new(other)),
    }
}

impl Keyring for OsKeyring {
    fn get_password(&self) -> Result<String, KeyringError> {
        self.entry()?.get_password().map_err(map_keyring_err)
    }
    fn set_password(&self, token: &str) -> Result<(), KeyringError> {
        self.entry()?.set_password(token).map_err(map_keyring_err)
    }
    fn delete_credential(&self) -> Result<(), KeyringError> {
        self.entry()?.delete_credential().map_err(map_keyring_err)
    }
}

/// Stores the token in the macOS Keychain. A `NoEntry` read maps to "" (unset) and a `NoEntry` delete
/// is a no-op (idempotent), mirroring Go `Keychain`.
pub struct Keychain {
    inner: Arc<dyn Keyring>,
}

impl Keychain {
    fn with_backend(inner: Arc<dyn Keyring>) -> Keychain {
        Keychain { inner }
    }
}

impl Store for Keychain {
    fn get(&self) -> Result<String, Error> {
        match self.inner.get_password() {
            Ok(v) => Ok(v),
            Err(KeyringError::NoEntry) => Ok(String::new()),
            Err(KeyringError::Other(e)) => Err(e),
        }
    }
    fn set(&self, token: &str) -> Result<(), Error> {
        self.inner.set_password(token).map_err(keyring_err_to_boxed)
    }
    fn delete(&self) -> Result<(), Error> {
        match self.inner.delete_credential() {
            Ok(()) | Err(KeyringError::NoEntry) => Ok(()),
            Err(KeyringError::Other(e)) => Err(e),
        }
    }
    fn backend(&self) -> &'static str {
        "keychain"
    }
}

fn keyring_err_to_boxed(e: KeyringError) -> Error {
    match e {
        // set_password never yields NoEntry; handle it defensively rather than silently succeeding.
        KeyringError::NoEntry => "keychain entry not found".into(),
        KeyringError::Other(e) => e,
    }
}

// ---- File fallback -------------------------------------------------------------------------------

/// Stores the token in a 0600 file (the fallback). [`Store::get`] returns "" when the file is absent.
/// Mirrors Go `credential.File`.
pub struct File {
    path: PathBuf,
}

impl File {
    pub fn new(path: impl Into<PathBuf>) -> File {
        File { path: path.into() }
    }
}

impl Store for File {
    fn get(&self) -> Result<String, Error> {
        match fs::read_to_string(&self.path) {
            Ok(s) => Ok(s.trim().to_string()),
            // Absent file reads as unset (first-launch). A read error that is NOT "missing" (e.g. a
            // directory, or permissions) surfaces — the app treats it as "unreadable, not absent".
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(e) => Err(Box::new(e)),
        }
    }

    fn set(&self, token: &str) -> Result<(), Error> {
        // Atomic 0600 write (temp + chmod + rename), shared with the prefs store — mirrors Go
        // File.Set's os.CreateTemp/Chmod/Rename.
        crate::atomicfile::write_0600(&self.path, token.as_bytes()).map_err(box_err)
    }

    fn delete(&self) -> Result<(), Error> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(Box::new(e)),
        }
    }

    fn backend(&self) -> &'static str {
        "file"
    }
}

fn box_err(e: std::io::Error) -> Error {
    Box::new(e)
}

// ---- Constructors --------------------------------------------------------------------------------

/// Default service/account namespace for the Linear token in the Keychain. Rebranded to Rhapsody's
/// bundle identifier (post-parity, per the ~/.rhapsody runtime move): the app is its own product with
/// its own credential namespace, and the app-level cutover has the operator paste the token fresh, so
/// there is no need to share the Go desktop's old keychain item.
pub const DEFAULT_SERVICE: &str = "is.makewhat.rhapsody";
pub const DEFAULT_ACCOUNT: &str = "linear-api-token";

/// The Keychain-backed store (the primary v1 path). Mirrors Go `credential.New`.
pub fn new() -> Keychain {
    Keychain::with_backend(Arc::new(OsKeyring {
        service: DEFAULT_SERVICE.to_string(),
        account: DEFAULT_ACCOUNT.to_string(),
    }))
}

/// The file-backed fallback store at `path`. Mirrors Go `credential.NewFile`.
pub fn new_file(path: impl Into<PathBuf>) -> File {
    File::new(path)
}

// ---- In-memory keychain double (tests) -----------------------------------------------------------

/// The Rust stand-in for `keyring.MockInit` / `MockInitWithError`: an in-memory [`Keyring`] shared via
/// `Arc` so multiple [`Keychain`] values (Go's repeated `credential.New()`) observe the SAME store,
/// with no process globals (parallel-safe). Used by this module's tests and the `app` credential tests.
#[cfg(test)]
pub(crate) mod mock {
    use super::*;
    use std::sync::Mutex;

    /// In-memory keychain: one secret slot plus an optional forced error (mirrors `MockInitWithError`,
    /// where every Get/Set/Delete fails).
    #[derive(Default)]
    pub struct MockKeyring {
        state: Mutex<Option<String>>,
        err: Mutex<Option<String>>,
    }

    impl MockKeyring {
        /// A working, empty keychain (`keyring.MockInit`).
        pub fn empty() -> Arc<MockKeyring> {
            Arc::new(MockKeyring::default())
        }

        /// A keychain whose every operation returns `msg` (`keyring.MockInitWithError`) — the
        /// "locked/unreadable" case, distinct from "absent".
        pub fn erroring(msg: &str) -> Arc<MockKeyring> {
            Arc::new(MockKeyring {
                state: Mutex::new(None),
                err: Mutex::new(Some(msg.to_string())),
            })
        }

        fn forced_error(&self) -> Option<KeyringError> {
            self.err
                .lock()
                .unwrap()
                .clone()
                .map(|m| KeyringError::Other(m.into()))
        }
    }

    impl Keyring for MockKeyring {
        fn get_password(&self) -> Result<String, KeyringError> {
            if let Some(e) = self.forced_error() {
                return Err(e);
            }
            match self.state.lock().unwrap().clone() {
                Some(v) => Ok(v),
                None => Err(KeyringError::NoEntry),
            }
        }
        fn set_password(&self, token: &str) -> Result<(), KeyringError> {
            if let Some(e) = self.forced_error() {
                return Err(e);
            }
            *self.state.lock().unwrap() = Some(token.to_string());
            Ok(())
        }
        fn delete_credential(&self) -> Result<(), KeyringError> {
            if let Some(e) = self.forced_error() {
                return Err(e);
            }
            let mut s = self.state.lock().unwrap();
            if s.is_none() {
                return Err(KeyringError::NoEntry);
            }
            *s = None;
            Ok(())
        }
    }

    /// A [`Keychain`] `Store` over an in-memory backend (the Go tests' `credential.New()` under a mock).
    pub fn keychain(backend: Arc<MockKeyring>) -> Keychain {
        Keychain::with_backend(backend)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};

    static DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_dir() -> PathBuf {
        let n = DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("rhapsody-d4-cred-{}-{n}", std::process::id()));
        fs::create_dir_all(&p).expect("create temp dir");
        p
    }

    // Mirrors TestFileStoreRoundTrip: an absent token reads as empty, Set persists 0600, Delete clears
    // it and is idempotent, and Set creates the parent dir as needed.
    #[test]
    fn file_store_round_trip() {
        let dir = temp_dir();
        let path = dir.join("nested").join("credentials");
        let s = File::new(&path);

        assert_eq!(
            s.get().expect("get absent"),
            "",
            "absent token reads as empty"
        );
        s.set("lin_api_file").expect("set");
        assert_eq!(s.get().expect("get"), "lin_api_file");

        let mode = fs::metadata(&path).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "credential file must be owner-only (600)");

        s.delete().expect("delete");
        assert_eq!(s.get().expect("get after delete"), "");
        s.delete().expect("delete is idempotent");

        fs::remove_dir_all(&dir).ok();
    }

    // Mirrors TestKeychainStoreRoundTrip: the Keychain store round-trips against the in-memory backend
    // (the Rust analog of keyring.MockInit) — NoEntry maps to empty, delete is idempotent.
    #[test]
    fn keychain_store_round_trip() {
        let s = mock::keychain(mock::MockKeyring::empty());

        assert_eq!(
            s.get().expect("get absent"),
            "",
            "an absent keychain entry (NoEntry) reads as empty, not an error"
        );
        s.set("lin_api_kc").expect("set");
        assert_eq!(s.get().expect("get"), "lin_api_kc");
        s.delete().expect("delete");
        assert_eq!(s.get().expect("get after delete"), "");
        s.delete().expect("delete of an absent key is a no-op");
        assert_eq!(s.backend(), "keychain");
    }

    // A forced-error keychain (keyring.MockInitWithError) surfaces the error on read rather than
    // masquerading as "absent" — the distinction the app's read-error handling depends on.
    #[test]
    fn keychain_read_error_surfaces() {
        let s = mock::keychain(mock::MockKeyring::erroring("keychain locked"));
        assert!(
            s.get().is_err(),
            "a locked keychain read must be an error, not empty"
        );
    }
}
