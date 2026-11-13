//! Per-run scratch directories (STUDIO-1993). Rhapsody-only — the frozen Go reference has no
//! counterpart.
//!
//! Every dispatched run gets its own directory under one daemon-owned root
//! (`<scratch_root>/<run-id>/`), and its child processes are pointed at it: `TMPDIR` always, and
//! `CARGO_TARGET_DIR` when the inherited environment does not already set one. Before this, agents
//! chose their own `$TMPDIR/opencode/<name>` paths for cargo target dirs, probe checkouts and review
//! worktrees, and nothing ever removed them (180 GiB and growing on the operator's Mac). The
//! daemon-owned root is what lets the orchestrator's janitor find and reclaim them.
//!
//! The directory is created lazily on a session's first turn (the run id is only known after the
//! worker calls `set_run_id`), and removed when the run ends — unless the session is retained for a
//! resume, in which case the runner marks it kept and the record carries the path so the resumed run
//! adopts and eventually removes it. Best-effort throughout: a scratch directory that cannot be
//! created or removed is a disk-hygiene problem, never a reason to fail a run.

use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// The per-run scratch directory. Dropping it removes the directory unless [`ScratchDir::keep`] was
/// called — the same keep-or-remove contract as `opencode::state::RunState`, so a retained resume
/// keeps its scratch tree alongside the session it resumes.
pub struct ScratchDir {
    dir: PathBuf,
    kept: AtomicBool,
}

impl ScratchDir {
    /// Provisions `<root>/<run_id>`, creating the root if needed. Returns `None` — the feature off —
    /// when there is no root configured or no run id (a store-disabled or coordinator session), and
    /// on any filesystem failure, which is logged and never fails the run.
    pub fn provision(root: &str, run_id: i64) -> Option<Self> {
        if root.is_empty() || run_id <= 0 {
            return None;
        }
        let dir = Path::new(root).join(run_id.to_string());
        if let Err(e) = std::fs::create_dir_all(root) {
            tracing::warn!(
                root, err = %e,
                "scratch: could not create the scratch root; running without a managed scratch dir"
            );
            return None;
        }
        match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => {
                tracing::warn!(
                    dir = %dir.display(), err = %e,
                    "scratch: could not create the per-run scratch directory; running without one"
                );
                return None;
            }
        }
        Some(Self {
            dir,
            kept: AtomicBool::new(false),
        })
    }

    /// Adopts an existing directory (a retained resume's scratch tree named by its record). No
    /// creation: the caller has already validated it is a managed child of the configured root.
    pub fn adopt(dir: PathBuf) -> Self {
        Self {
            dir,
            kept: AtomicBool::new(false),
        }
    }

    /// Adopts a retained scratch tree for a new run, renaming it to `<root>/<run_id>`.
    ///
    /// The rename is not cosmetic: the janitor protects a live run's scratch directory by the LIVE
    /// run's id, so a tree left under the cut-off run's id (which is no longer live) could be
    /// reclaimed from under the resumed run once it passes `scratch.ttl_hours`. Renaming it to the
    /// resumed run's id closes that hole. It is also the coordination point against a sweep already
    /// under way (STUDIO-1993 review B3): the janitor takes ownership of a candidate by renaming the
    /// SAME source path to a private `.trash-*` sibling before traversing it, so this rename and
    /// that claim are atomic with each other — whichever wins outright. If the janitor won, `old` is
    /// already gone and this falls through to `provision`, giving the resumed run a fresh directory
    /// instead of one being deleted from under it. When the rename is otherwise impossible (a
    /// cross-device root, a target that already exists) it falls back to the original path —
    /// correct, but re-exposed to the TTL — and to `provision` when the original is gone.
    pub fn adopt_renamed(root: &str, run_id: i64, old: &Path) -> Option<Self> {
        if root.is_empty() || run_id <= 0 {
            return None;
        }
        let new = Path::new(root).join(run_id.to_string());
        if new.is_dir() {
            return Some(Self {
                dir: new,
                kept: AtomicBool::new(false),
            });
        }
        if old.is_dir()
            && std::fs::create_dir_all(root).is_ok()
            && std::fs::rename(old, &new).is_ok()
        {
            return Some(Self {
                dir: new,
                kept: AtomicBool::new(false),
            });
        }
        if old.is_dir() {
            tracing::warn!(
                from = %old.display(), to = %new.display(),
                "scratch: could not rename an adopted scratch directory to the resumed run's id; \
                 using the original path"
            );
            return Some(Self {
                dir: old.to_path_buf(),
                kept: AtomicBool::new(false),
            });
        }
        Self::provision(root, run_id)
    }

    /// The directory the child processes are pointed at.
    pub fn path(&self) -> &Path {
        &self.dir
    }

    /// Marks the directory retained past this run, so `Drop` will not remove it.
    pub fn keep(&self) {
        self.kept.store(true, Ordering::SeqCst);
    }

    /// Whether the directory is marked retained.
    pub fn is_kept(&self) -> bool {
        self.kept.load(Ordering::SeqCst)
    }

    /// Removes the directory. Idempotent and best-effort: by the time this runs the run's outcome is
    /// already decided, and a leaked directory is a disk problem rather than a correctness one.
    pub fn cleanup(&self) {
        if let Err(e) = std::fs::remove_dir_all(&self.dir)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(
                dir = %self.dir.display(), err = %e,
                "scratch: could not remove the per-run scratch directory"
            );
        }
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        if !self.kept.load(Ordering::SeqCst) {
            self.cleanup();
        }
    }
}

/// Points the child environment at `dir`: `TMPDIR` is overridden unconditionally (the whole point
/// is that the agent's temp files land where the daemon owns them), and `CARGO_TARGET_DIR` is set
/// only when the inherited environment does not already name one, so an operator's explicit target
/// dir is honored.
pub fn apply_env(env: &mut Vec<String>, dir: &Path) {
    let value = dir.to_string_lossy().into_owned();
    env.retain(|kv| !kv.starts_with("TMPDIR="));
    env.push(format!("TMPDIR={value}"));
    if !env.iter().any(|kv| kv.starts_with("CARGO_TARGET_DIR=")) {
        env.push(format!("CARGO_TARGET_DIR={value}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "rhapsody-scratch-test-{}-{tag}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&root);
        root
    }

    #[test]
    fn provision_creates_the_run_directory_and_drop_removes_it() {
        let root = temp_root("prov");
        {
            let s = ScratchDir::provision(&root.to_string_lossy(), 42).expect("provisioned");
            assert!(s.path().is_dir(), "the run directory must exist");
            assert_eq!(s.path(), root.join("42"));
        }
        assert!(!root.join("42").exists(), "drop must remove the directory");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_kept_directory_survives_drop() {
        let root = temp_root("keep");
        {
            let s = ScratchDir::provision(&root.to_string_lossy(), 7).expect("provisioned");
            s.keep();
        }
        assert!(root.join("7").is_dir(), "a kept directory must survive");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn provision_is_off_without_a_root_or_run_id() {
        let root = temp_root("off");
        assert!(ScratchDir::provision("", 5).is_none(), "no root => off");
        assert!(
            ScratchDir::provision(&root.to_string_lossy(), 0).is_none(),
            "no run id => off"
        );
        assert!(
            !root.exists(),
            "nothing may be created when the feature is off"
        );
    }

    #[test]
    fn apply_env_sets_tmpdir_and_only_defaults_cargo_target_dir() {
        let dir = Path::new("/scratch/9");
        let mut env = vec!["PATH=/bin".to_string(), "TMPDIR=/old".to_string()];
        apply_env(&mut env, dir);
        assert!(env.contains(&"TMPDIR=/scratch/9".to_string()));
        assert!(
            !env.contains(&"TMPDIR=/old".to_string()),
            "TMPDIR is overridden"
        );
        assert!(env.contains(&"CARGO_TARGET_DIR=/scratch/9".to_string()));

        let mut env = vec!["CARGO_TARGET_DIR=/operator/target".to_string()];
        apply_env(&mut env, dir);
        assert!(
            env.contains(&"CARGO_TARGET_DIR=/operator/target".to_string()),
            "an inherited CARGO_TARGET_DIR must be honored"
        );
        assert_eq!(
            env.iter()
                .filter(|kv| kv.starts_with("CARGO_TARGET_DIR="))
                .count(),
            1
        );
    }

    #[test]
    fn adopt_points_at_the_existing_directory() {
        let root = temp_root("adopt");
        std::fs::create_dir_all(root.join("3")).expect("mkdir");
        let s = ScratchDir::adopt(root.join("3"));
        assert_eq!(s.path(), root.join("3"));
        drop(s);
        assert!(
            !root.join("3").exists(),
            "an adopted dir is removed on drop"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn adopt_renamed_moves_the_tree_to_the_live_run_id() {
        let root = temp_root("rename");
        std::fs::create_dir_all(root.join("11")).expect("mkdir old");
        std::fs::write(root.join("11/cargo-target"), b"x").expect("write");
        let s = ScratchDir::adopt_renamed(&root.to_string_lossy(), 12, &root.join("11"))
            .expect("adopted");
        assert_eq!(s.path(), root.join("12"), "the tree takes the live run id");
        assert!(!root.join("11").exists(), "the old id is gone");
        assert!(
            root.join("12/cargo-target").is_file(),
            "contents moved with it"
        );
        drop(s);
        assert!(!root.join("12").exists(), "removed on drop");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn adopt_renamed_provisions_fresh_when_the_tree_is_gone() {
        let root = temp_root("rename-gone");
        let s = ScratchDir::adopt_renamed(&root.to_string_lossy(), 5, &root.join("4"))
            .expect("provisions fresh");
        assert_eq!(s.path(), root.join("5"));
        drop(s);
        let _ = std::fs::remove_dir_all(&root);
    }
}
