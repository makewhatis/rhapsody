//! Locating the `rhapsodyd` sidecar binary. Parity port of
//! `$REF/desktop/internal/supervisor/resolve.go`.

use std::path::{Path, PathBuf};

/// The rhapsodyd executable name embedded as the app's sidecar.
pub(crate) const BINARY_NAME: &str = "rhapsodyd";

/// Returned by [`resolve_binary`] when no `rhapsodyd` can be located. Mirrors the Go error string so
/// the caller can surface a clear "daemon not found" instead of silently launching nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolveError {
    dev_override: String,
    bundle_resources_dir: String,
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "could not resolve the {BINARY_NAME} sidecar (checked dev override {:?}, bundle {:?}, and PATH)",
            self.dev_override, self.bundle_resources_dir
        )
    }
}

impl std::error::Error for ResolveError {}

/// Locates the rhapsodyd sidecar, in priority order:
///
///  1. `dev_override` — an explicit path (e.g. a freshly-built `./rhapsodyd` used by `tauri dev` or
///     the `SYMPHONY_DAEMON` env override), when it exists.
///  2. `<bundle_resources_dir>/rhapsodyd` — the sidecar `make app` copies into the app bundle's
///     `Contents/Resources`.
///  3. `rhapsodyd` on PATH — a last-resort dev convenience.
///
/// Returns the resolved path, or a [`ResolveError`] if none is found.
pub fn resolve_binary(
    dev_override: &str,
    bundle_resources_dir: &str,
) -> Result<PathBuf, ResolveError> {
    if !dev_override.is_empty() && is_executable_file(Path::new(dev_override)) {
        return Ok(PathBuf::from(dev_override));
    }
    if !bundle_resources_dir.is_empty() {
        let candidate = Path::new(bundle_resources_dir).join(BINARY_NAME);
        if is_executable_file(&candidate) {
            return Ok(candidate);
        }
    }
    if let Some(p) = look_path(BINARY_NAME) {
        return Ok(p);
    }
    Err(ResolveError {
        dev_override: dev_override.to_string(),
        bundle_resources_dir: bundle_resources_dir.to_string(),
    })
}

/// Searches `$PATH` for an executable named `name` (the Rust stand-in for Go's `exec.LookPath`,
/// which is not in the std library). An empty PATH element is treated as "." — matching
/// `exec.LookPath` on Unix.
fn look_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let dir = if dir.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            dir
        };
        let candidate = dir.join(name);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// Reports whether `path` is a regular file with at least one execute bit set. Mirrors Go's
/// `os.Stat` + `!info.IsDir()` + `mode.Perm()&0o111 != 0`.
pub fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        Ok(info) if !info.is_dir() => info.permissions().mode() & 0o111 != 0,
        _ => false,
    }
}

/// Returns the `Contents/Resources` directory of the running app bundle, given the executable path
/// (`Contents/MacOS/<exe>` -> `Contents/Resources`). Returns `None` when the executable is not laid
/// out as a macOS `.app` bundle (e.g. `cargo run`), in which case the caller falls back to a dev
/// override or PATH. Mirrors Go `ResourcesDirFor`.
pub fn resources_dir_for(executable_path: &str) -> Option<PathBuf> {
    let exe = Path::new(executable_path);
    let macos_dir = exe.parent()?; // .../Contents/MacOS
    let contents = macos_dir.parent()?; // .../Contents
    if macos_dir.file_name()? != "MacOS" || contents.file_name()? != "Contents" {
        return None;
    }
    let res = contents.join("Resources");
    match std::fs::metadata(&res) {
        Ok(info) if info.is_dir() => Some(res),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// A unique, freshly-created temp dir for a test (no `tempfile` dep in the desktop workspace).
    fn temp_dir() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let p =
            std::env::temp_dir().join(format!("rhapsody-d2-resolve-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&p).expect("create temp dir");
        p
    }

    /// Writes an executable (0755) file at `path`, creating parent dirs. Mirror of `mkExec`.
    fn mk_exec(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(path.parent().unwrap()).expect("mkdir");
        std::fs::write(path, b"#!/bin/sh\n").expect("write");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }

    // Mirrors TestResolveBinaryPrefersDevOverride.
    #[test]
    fn resolve_binary_prefers_dev_override() {
        let dir = temp_dir();
        let dev = dir.join("dev").join("rhapsodyd");
        let res = dir.join("Resources");
        mk_exec(&dev);
        mk_exec(&res.join("rhapsodyd"));

        let got =
            resolve_binary(dev.to_str().unwrap(), res.to_str().unwrap()).expect("resolve_binary");
        assert_eq!(got, dev, "want dev override");
        std::fs::remove_dir_all(&dir).ok();
    }

    // Mirrors TestResolveBinaryFallsBackToBundle.
    #[test]
    fn resolve_binary_falls_back_to_bundle() {
        let dir = temp_dir();
        let res = dir.join("Resources");
        let bundle_bin = res.join("rhapsodyd");
        mk_exec(&bundle_bin);

        let got = resolve_binary("", res.to_str().unwrap()).expect("resolve_binary");
        assert_eq!(got, bundle_bin, "want bundle binary");
        std::fs::remove_dir_all(&dir).ok();
    }

    // Mirrors TestResolveBinaryMissing: with an isolated (empty) PATH, nothing resolves -> error.
    #[test]
    fn resolve_binary_missing() {
        let dir = temp_dir();
        let empty = dir.join("emptybin");
        std::fs::create_dir_all(&empty).expect("mkdir");
        // Scope a PATH override to this test only — restore it after so parallel tests are unaffected.
        let prev = std::env::var_os("PATH");
        // SAFETY: set_var/remove_var are unsafe in Rust 2024. The test crate runs sequentially per
        // test binary but tests within it share the process env; scope the override tightly and
        // restore. No other test in this module reads PATH.
        unsafe { std::env::set_var("PATH", &empty) };
        let got = resolve_binary(
            dir.join("nope").to_str().unwrap(),
            dir.join("Resources").to_str().unwrap(),
        );
        match prev {
            Some(v) => unsafe { std::env::set_var("PATH", v) },
            None => unsafe { std::env::remove_var("PATH") },
        }
        assert!(got.is_err(), "expected an error when no rhapsodyd resolves");
        std::fs::remove_dir_all(&dir).ok();
    }

    // Additive (the Go reference has no ResourcesDirFor test): a proper Contents/MacOS/<exe> layout
    // with a Resources dir resolves; a non-bundle path does not.
    #[test]
    fn resources_dir_for_detects_bundle_layout() {
        let dir = temp_dir();
        let macos = dir.join("Contents").join("MacOS");
        let resources = dir.join("Contents").join("Resources");
        std::fs::create_dir_all(&macos).expect("mkdir MacOS");
        std::fs::create_dir_all(&resources).expect("mkdir Resources");
        let exe = macos.join("rhapsody-desktop");

        assert_eq!(resources_dir_for(exe.to_str().unwrap()), Some(resources));
        assert_eq!(resources_dir_for("/usr/local/bin/rhapsody-desktop"), None);
        std::fs::remove_dir_all(&dir).ok();
    }
}
