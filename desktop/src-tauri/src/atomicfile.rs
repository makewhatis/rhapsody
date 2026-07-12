//! Atomic owner-only file writes (temp + rename), shared by the credential file fallback
//! ([`crate::credential::File`]) and the prefs store ([`crate::prefs`]) — both mirror the same Go
//! idiom (`os.MkdirAll` + `os.CreateTemp` + `Chmod 0600` + `os.Rename`) from
//! `$REF/desktop/internal/{credential,prefs}`.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// Monotonic counter for unique temp-file names (no `tempfile` dep in the desktop workspace); paired
/// with the pid so concurrent processes never collide either.
static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Writes `data` to `path` atomically with 0600 permissions, creating the parent directory (0700) as
/// needed. It writes to a uniquely-named temp file in the SAME directory (an atomic rename requires
/// one filesystem), chmods it 0600 — explicitly, so the mode holds regardless of the process umask,
/// matching Go's `os.Chmod` on the temp before rename — then renames over `path`. A reader therefore
/// never sees a torn write, and the file is never group/other-readable. On any failure the temp file
/// is best-effort removed (Go's `defer os.Remove`).
pub fn write_0600(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(
        ".tmp-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let res = (|| {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(data)?;
        drop(f); // close before the chmod + rename, matching Go's tmp.Close() ordering
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
        fs::rename(&tmp, path)
    })();
    if res.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    res
}
