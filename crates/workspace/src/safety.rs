//! Filesystem safety invariants + lexical path helpers (`safety.go`, upstream §9.5).
//!
//! The path helpers ([`clean`], [`join`], [`dir`]) are byte-for-byte ports of Go's `path/filepath`
//! for the Unix separator (`macOS`/Linux are the only targets, exactly as Go's build has no
//! cross-platform abstraction here). They are LEXICAL — they never touch the filesystem or resolve
//! symlinks — because [`ensure_within_root`]'s containment guard depends on lexical semantics: it
//! trusts a daemon-owned root that no attacker-controlled component can plant a symlink into (keys
//! are sanitized to `[A-Za-z0-9._-]`), and the reuse paths reject symlinks separately with an
//! `Lstat` check.

use std::os::unix::fs::DirBuilderExt;

use crate::Error;

/// Creates `path` and any missing parents with mode `0o755`, the port of Go's
/// `os.MkdirAll(path, 0o755)`: idempotent on an existing directory. The daemon owns the dir (rwx)
/// and the agent process must traverse/read it (r-x).
pub(crate) fn mkdir_all(path: &str) -> std::io::Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create(path)
}

/// Removes `path` and any children, the port of Go's `os.RemoveAll`: a missing path is not an error
/// (returns `Ok`), a file/symlink is unlinked, and a directory is removed recursively.
pub(crate) fn remove_all(path: &str) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Lexically cleans `path`, the Unix-separator port of Go's `filepath.Clean`: it collapses
/// duplicate separators, eliminates `.` elements, and resolves `..` elements against the preceding
/// element (or drops a `..` that would escape a rooted path). An empty path cleans to `"."`. No
/// filesystem access; no symlink resolution.
pub(crate) fn clean(path: &str) -> String {
    if path.is_empty() {
        return ".".to_string();
    }
    let b = path.as_bytes();
    let n = b.len();
    let rooted = b[0] == b'/';
    // `out` accumulates the cleaned path; `dotdot` is the byte index in `out` before which we may
    // not backtrack (past a leading "/" or already-emitted leading ".." elements).
    let mut out: Vec<u8> = Vec::with_capacity(n + 1);
    let mut r = 0usize;
    let mut dotdot = 0usize;
    if rooted {
        out.push(b'/');
        r = 1;
        dotdot = 1;
    }
    while r < n {
        if b[r] == b'/' {
            // empty path element
            r += 1;
        } else if b[r] == b'.' && (r + 1 == n || b[r + 1] == b'/') {
            // . element
            r += 1;
        } else if b[r] == b'.' && b[r + 1] == b'.' && (r + 2 == n || b[r + 2] == b'/') {
            // .. element: back up to the previous separator
            r += 2;
            if out.len() > dotdot {
                let mut w = out.len() - 1;
                while w > dotdot && out[w] != b'/' {
                    w -= 1;
                }
                out.truncate(w);
            } else if !rooted {
                // cannot backtrack, but not rooted, so keep the .. element
                if !out.is_empty() {
                    out.push(b'/');
                }
                out.push(b'.');
                out.push(b'.');
                dotdot = out.len();
            }
        } else {
            // real path element; add a separator if one is needed
            if (rooted && out.len() != 1) || (!rooted && !out.is_empty()) {
                out.push(b'/');
            }
            while r < n && b[r] != b'/' {
                out.push(b[r]);
                r += 1;
            }
        }
    }
    if out.is_empty() {
        return ".".to_string();
    }
    // `out` is `path`'s bytes (valid UTF-8) plus ASCII '/' and '.', so it is always valid UTF-8.
    String::from_utf8_lossy(&out).into_owned()
}

/// Joins path elements with `/` and cleans the result, the port of Go's `filepath.Join`: empty
/// elements are ignored, and an all-empty join yields `""`.
pub(crate) fn join(parts: &[&str]) -> String {
    let joined = parts
        .iter()
        .filter(|p| !p.is_empty())
        .copied()
        .collect::<Vec<&str>>()
        .join("/");
    if joined.is_empty() {
        return String::new();
    }
    clean(&joined)
}

/// Returns all but the last element of `path`, cleaned — the Unix port of Go's `filepath.Dir`.
pub(crate) fn dir(path: &str) -> String {
    let b = path.as_bytes();
    let mut i = b.len();
    while i > 0 && b[i - 1] != b'/' {
        i -= 1;
    }
    // path[0..i] includes the trailing separator (or is empty for a separator-less path).
    clean(&path[0..i])
}

/// Enforces the pre-agent-launch invariants (upstream §9.5): the workspace path must stay inside
/// the workspace root, and the agent's working directory must equal the workspace path.
pub fn validate_launch(root: &str, workspace_path: &str, cwd: &str) -> Result<(), Error> {
    ensure_within_root(root, workspace_path)?;
    if clean(cwd) != clean(workspace_path) {
        return Err(Error::InvalidCwd(format!(
            "cwd {cwd:?} != workspace {workspace_path:?}"
        )));
    }
    Ok(())
}

/// Requires `p` to be the root or a descendant of it (upstream §9.5).
///
/// A LEXICAL containment check only: it cleans the paths and compares them as strings, and does
/// NOT resolve symlinks. It assumes a trusted workspace root not subject to adversarial symlink
/// injection — the daemon owns the root and keys are sanitized to `[A-Za-z0-9._-]`. This is the
/// port of Go's `filepath.Rel`-based check: `rel == ".."` or a `"../"` prefix (an escape) is
/// exactly "not equal to and not prefixed by `<root>/`" for the absolute paths this guards.
pub(crate) fn ensure_within_root(root: &str, p: &str) -> Result<(), Error> {
    let r = clean(root);
    let pp = clean(p);
    let within = if pp == r {
        true
    } else {
        let prefix = if r.ends_with('/') {
            r.clone()
        } else {
            format!("{r}/")
        };
        pp.starts_with(&prefix)
    };
    if !within {
        return Err(Error::PathOutsideRoot(format!("{pp:?} escapes root {r:?}")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    // Mirror of TestValidateLaunchOK (safety_test.go).
    #[test]
    fn validate_launch_ok() {
        let root = TempDir::new();
        let ws = join(&[&root.path, "MT-1"]);
        assert!(validate_launch(&root.path, &ws, &ws).is_ok());
    }

    // Mirror of TestValidateLaunchCwdMismatch.
    #[test]
    fn validate_launch_cwd_mismatch() {
        let root = TempDir::new();
        let ws = join(&[&root.path, "MT-1"]);
        let other = join(&[&root.path, "MT-2"]);
        assert!(matches!(
            validate_launch(&root.path, &ws, &other),
            Err(Error::InvalidCwd(_))
        ));
    }

    // Mirror of TestValidateLaunchOutsideRoot.
    #[test]
    fn validate_launch_outside_root() {
        let root = TempDir::new();
        let outside = join(&[&dir(&root.path), "elsewhere", "MT-1"]);
        assert!(matches!(
            validate_launch(&root.path, &outside, &outside),
            Err(Error::PathOutsideRoot(_))
        ));
    }

    // Mirror of TestValidateLaunchRootItselfRejectedAsWorkspace: a child is allowed; a parent
    // traversal is rejected.
    #[test]
    fn validate_launch_child_ok_traversal_rejected() {
        let root = TempDir::new();
        let child = join(&[&root.path, "x"]);
        assert!(validate_launch(&root.path, &child, &child).is_ok());
        let traversal = join(&[&root.path, "..", "y"]);
        assert!(matches!(
            validate_launch(&root.path, &traversal, &traversal),
            Err(Error::PathOutsideRoot(_))
        ));
    }

    // Extra guard: the lexical helpers agree with Go's filepath on the shapes this crate relies on.
    #[test]
    fn clean_and_dir_match_filepath_semantics() {
        assert_eq!(clean("/a/b/../c"), "/a/c");
        assert_eq!(clean("/a//b/./c/"), "/a/b/c");
        assert_eq!(clean(""), ".");
        assert_eq!(clean("/a/b/c/.."), "/a/b");
        assert_eq!(dir("/a/b/c"), "/a/b");
        assert_eq!(dir("/tmp/x/.mirrors/abc.git"), "/tmp/x/.mirrors");
        assert_eq!(join(&["/a", "", "b", "c"]), "/a/b/c");
        assert_eq!(join(&["", ""]), "");
    }
}
