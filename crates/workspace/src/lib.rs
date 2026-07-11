//! rhapsody-workspace — parity port of Go `internal/workspace` (Symphony v0.4.0).
//!
//! Task W1 ships the git layer: the [`Manager`] workspace-creation API in clone + worktree modes,
//! the per-repo bare-mirror cache, `symphony/<key>` branch naming, path-containment safety
//! ([`validate_launch`]), and issue-identifier [`sanitize_key`]-ation. git is driven as a
//! subprocess exactly as Go shells out (`os/exec` → [`tokio::process`]); no libgit2/gitoxide.
//!
//! The Go package is one compilation unit, so `repo.go`'s methods hang off the same `Manager` as
//! `manager.go`/`hooks.go`. To mirror `repo_test.go`/`repo_clone_test.go` (which build a `Manager`,
//! run lifecycle hooks, and delegate empty-URL calls to the legacy path), W1 lays down the
//! [`Manager`] + [`hooks::HookRunner`] scaffold those four test files exercise. The remaining
//! lifecycle surface — `BeforeRun`/`AfterRun`, the public `CreateForIssue`/`Remove` wrappers, the
//! labeler, GC, and the full process-group hook-timeout semantics (`hooks_test.go`) — lands in
//! W2/W3, mirroring how the tracker crate shipped adapter skeletons its later tasks filled in.
//!
//! Go's `ctx context.Context` threading becomes implicit async cancellation (drop the future); the
//! hook timeout stays explicit ([`hooks::HookRunner`]). Go's bare `error` returns wrapped around
//! `errors.New` sentinels become the typed [`Error`] enum, whose `Display` reproduces the errors.go
//! category string as its leading token so `errors.Is`-style category checks map to variant matches.

mod hooks;
mod manager;
mod repo;
mod safety;
mod sanitize;

pub use hooks::HookRunner;
pub use manager::{Config, HookScripts, Manager};
pub use repo::repo_key;
pub use safety::validate_launch;
pub use sanitize::{Workspace, sanitize_key};

/// Typed workspace error categories — the parity mirror of `errors.go`'s `errors.New` sentinels
/// (upstream §9, §10.6, §14.1) plus the repo-backed (bare-mirror + git-worktree) categories.
///
/// Each variant carries the `fmt.Errorf("%w: …")` context Go layers onto the sentinel; its
/// `Display` renders `"<category>: <context>"`, so the leading token is byte-identical to the Go
/// sentinel string (e.g. `git_failed`). Go's `errors.Is(err, ErrX)` becomes a `matches!` on the
/// variant: the outermost wrap determines the category, exactly as Go reports the outer `%w`.
///
/// `ErrGhFailed` (post-run labeler, AIE-301) is intentionally deferred to W2 with the labeler that
/// constructs it; every category defined here is constructed by W1.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// `workspace_create_failed` — mkdir/stat failure creating a legacy workspace directory.
    #[error("workspace_create_failed: {0}")]
    WorkspaceCreate(String),
    /// `workspace_remove_failed` — RemoveAll failure deleting a workspace directory.
    #[error("workspace_remove_failed: {0}")]
    WorkspaceRemove(String),
    /// `workspace_not_directory` — the workspace path exists but is not a directory.
    #[error("workspace_not_directory: {0}")]
    WorkspaceNotDir(String),
    /// `workspace_is_symlink` — the workspace path exists and is a symlink (rejected pre-launch).
    #[error("workspace_is_symlink: {0}")]
    WorkspaceSymlink(String),
    /// `invalid_workspace_path` — the workspace path escapes the workspace root (or the root is not
    /// absolute).
    #[error("invalid_workspace_path: {0}")]
    PathOutsideRoot(String),
    /// `invalid_workspace_cwd` — the agent cwd does not equal the workspace path.
    #[error("invalid_workspace_cwd: {0}")]
    InvalidCwd(String),
    /// `hook_failed` — a lifecycle hook exited non-zero.
    #[error("hook_failed: {0}")]
    HookFailed(String),
    /// `hook_timeout` — a lifecycle hook exceeded its timeout.
    #[error("hook_timeout: {0}")]
    HookTimeout(String),
    /// `git_failed` — a git invocation exited non-zero (or failed to spawn).
    #[error("git_failed: {0}")]
    GitFailed(String),
    /// `mirror_clone_failed` — bare clone / initial-fetch of the mirror failed (fatal).
    #[error("mirror_clone_failed: {0}")]
    MirrorClone(String),
    /// `clone_failed` — a workspace_mode:clone full clone failed (fatal).
    #[error("clone_failed: {0}")]
    CloneFailed(String),
    /// `worktree_add_failed` — `git worktree add` failed.
    #[error("worktree_add_failed: {0}")]
    WorktreeAdd(String),
    /// `worktree_remove_failed` — the worktree dir could not be removed.
    #[error("worktree_remove_failed: {0}")]
    WorktreeRemove(String),
    /// `worktree_outside_root` — the computed worktree path escaped root (or took the reserved
    /// `.mirrors` name).
    #[error("worktree_outside_root: {0}")]
    WorktreeOutsideRoot(String),
    /// `workspace_stat_failed` — stat of the worktree path failed (a non-NotExist error).
    #[error("workspace_stat_failed: {0}")]
    WorkspaceStat(String),
}

/// Shared test scaffolding: the RAII [`testutil::TempDir`] (the port of Go's `t.TempDir()`), the
/// [`testutil::repo_test_manager`] constructor, and the real-temp-git-repo helpers
/// (`initLocalOrigin`/`addOriginCommit`/`addOriginBranch`/`underRoot`) the `repo_test.go` /
/// `repo_clone_test.go` mirrors build on. Kept in one place so every module's test block reuses it,
/// exactly as the Go package shares these helpers across its `_test.go` files.
#[cfg(test)]
pub(crate) mod testutil {
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    use crate::{Config, HookScripts, Manager};

    static TEST_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// RAII temp directory mirroring Go's `t.TempDir()` (unique per pid+counter, auto-removed).
    /// Paths are kept as `String`s to match the whole crate's Go-style lexical path handling.
    pub(crate) struct TempDir {
        pub path: String,
    }

    impl TempDir {
        pub(crate) fn new() -> TempDir {
            let n = TEST_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "rhapsody-workspace-{}-{}",
                std::process::id(),
                n
            ));
            std::fs::create_dir_all(&path).unwrap();
            TempDir {
                path: path.to_string_lossy().into_owned(),
            }
        }

        /// Joins `name` under this directory (Go-lexical), returning the path string.
        pub(crate) fn child(&self, name: &str) -> String {
            crate::safety::join(&[&self.path, name])
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    /// Builds a Manager over a fresh temp root with the given hooks (mirror of `repoTestManager`).
    /// The returned [`TempDir`] must be kept alive for the root to persist.
    pub(crate) fn repo_test_manager(hooks: HookScripts) -> (Manager, TempDir) {
        let root = TempDir::new();
        let m = Manager::new(Config {
            root: root.path.clone(),
            hooks,
            hook_timeout: Duration::from_secs(30),
        })
        .unwrap();
        (m, root)
    }

    /// Runs a git command in `dir` with deterministic author/committer identity; panics on failure
    /// (test helper). Mirrors the `run` closure inside `initLocalOrigin`.
    pub(crate) fn git_run(dir: &str, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Creates a temp NON-bare git repo with one commit on `main` (mirror of `initLocalOrigin`). The
    /// returned [`TempDir`] is the "origin" the mirror tests clone from (no network); keep it alive.
    pub(crate) fn init_local_origin() -> TempDir {
        let dir = TempDir::new();
        git_run(&dir.path, &["init", "-b", "main"]);
        std::fs::write(dir.child("README.md"), "hello\n").unwrap();
        git_run(&dir.path, &["add", "README.md"]);
        git_run(&dir.path, &["commit", "-m", "initial"]);
        dir
    }

    /// Adds an empty commit to a non-bare origin so a fetch has something new (mirror of
    /// `addOriginCommit`).
    pub(crate) fn add_origin_commit(origin: &str) {
        git_run(origin, &["commit", "--allow-empty", "-m", "more"]);
    }

    /// Creates `branch` off the current HEAD (with an extra commit) so a clone can later check it
    /// out — models a Graphite stack's branches (mirror of `addOriginBranch`).
    pub(crate) fn add_origin_branch(origin: &str, branch: &str) {
        git_run(origin, &["checkout", "-b", branch]);
        let msg = format!("on {branch}");
        git_run(origin, &["commit", "--allow-empty", "-m", &msg]);
        git_run(origin, &["checkout", "main"]);
    }

    /// Reports whether `p` is `root` or a descendant, resolving symlinks first so macOS
    /// `/var` vs `/private/var` aliasing does not produce false negatives (mirror of `underRoot`).
    pub(crate) fn under_root(p: &str, root: &str) -> bool {
        let resolve = |s: &str| {
            std::fs::canonicalize(s)
                .map(|x| x.to_string_lossy().into_owned())
                .unwrap_or_else(|_| s.to_string())
        };
        let p = crate::safety::clean(&resolve(p));
        let root = crate::safety::clean(&resolve(root));
        if p == root {
            return true;
        }
        p.starts_with(&format!("{root}/"))
    }
}
