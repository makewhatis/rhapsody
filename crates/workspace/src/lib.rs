//! rhapsody-workspace — parity port of Go `internal/workspace` (Symphony v0.4.0).
//!
//! Task W1 ships the git layer: the [`Manager`] workspace-creation API in clone + worktree modes,
//! the per-repo bare-mirror cache, `symphony/<key>` branch naming, path-containment safety
//! ([`validate_launch`]), and issue-identifier [`sanitize_key`]-ation. git is driven as a
//! subprocess exactly as Go shells out (`os/exec` → [`tokio::process`]); no libgit2/gitoxide.
//!
//! The Go package is one compilation unit, so `repo.go`'s methods hang off the same `Manager` as
//! `manager.go`/`hooks.go`. To mirror `repo_test.go`/`repo_clone_test.go` (which build a `Manager`,
//! run lifecycle hooks, and delegate empty-URL calls to the legacy path), W1 laid down the
//! [`Manager`] + [`hooks::HookRunner`] scaffold those four test files exercise. W2 completes the
//! per-issue lifecycle on top: the public `create_for_issue`/`before_run`/`after_run`/`remove`
//! surface P5 drives, the post-run [`labeler`], and the full process-group hook-timeout semantics
//! (`hooks_test.go`). GC + the graphite guard land in W3.
//!
//! Go's `ctx context.Context` threading becomes implicit async cancellation (drop the future); the
//! hook timeout stays explicit ([`hooks::HookRunner`]). Go's bare `error` returns wrapped around
//! `errors.New` sentinels become the typed [`Error`] enum, whose `Display` reproduces the errors.go
//! category string as its leading token so `errors.Is`-style category checks map to variant matches.

mod hooks;
mod labeler;
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
/// `ErrGhFailed` (post-run labeler, AIE-301) lands in W2 with the [`labeler`](crate::labeler) that
/// constructs it; every other category is constructed by W1.
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
    /// `gh_failed` — a `gh` CLI invocation exited non-zero (or timed out) during the post-run
    /// labeler (AIE-301). Constructed by [`crate::labeler`]; callers log and swallow it.
    #[error("gh_failed: {0}")]
    GhFailed(String),
}

/// Shared test scaffolding: the RAII [`testutil::TempDir`] (the port of Go's `t.TempDir()`), the
/// [`testutil::repo_test_manager`] constructor, and the real-temp-git-repo helpers
/// (`initLocalOrigin`/`addOriginCommit`/`addOriginBranch`/`underRoot`) the `repo_test.go` /
/// `repo_clone_test.go` mirrors build on. Kept in one place so every module's test block reuses it,
/// exactly as the Go package shares these helpers across its `_test.go` files.
#[cfg(test)]
pub(crate) mod testutil {
    use std::ffi::OsString;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    use crate::safety::join;
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

    // ---- labeler_test.go helpers (post-run PR labeler) ----

    /// A fake `gh` on a private PATH (parity mirror of Go's `writeFakeGh`, parallel-safe). Holds the
    /// temp dir alive, the log/prmap paths, and the [`Manager::gh_env_overlay`] a test installs so
    /// the labeler's `gh` subprocess finds this fake and reads `$GH_LOG`/`$GH_PRMAP` — WITHOUT
    /// mutating the process environment (Rust 2024 forbids the `unsafe set_var` Go's `t.Setenv`
    /// relies on). Each test gets its own, so parallel labeler tests never collide.
    pub(crate) struct FakeGh {
        /// Kept alive so the fake `gh`, its log, and prmap survive for the test's lifetime.
        pub _dir: TempDir,
        pub log_path: String,
        pub prmap_path: String,
        pub overlay: Vec<(OsString, OsString)>,
    }

    /// The standard fake `gh` (mirror of Go's `writeFakeGh` script): logs one `ARGS: …` line per
    /// call, fails `label create` with an "already exists"-style message, answers `pr list --head`
    /// from the prmap control file, and exits 0 for `pr edit`.
    const FAKE_GH_SCRIPT: &str = r#"#!/usr/bin/env bash
echo "ARGS: $*" >> "$GH_LOG"
if [ "$1" = "label" ] && [ "$2" = "create" ]; then
  echo "label already exists" >&2
  exit 1
fi
if [ "$1" = "pr" ] && [ "$2" = "list" ]; then
  head=""
  while [ $# -gt 0 ]; do
    if [ "$1" = "--head" ]; then head="$2"; fi
    shift
  done
  grep -E "^${head}=" "$GH_PRMAP" 2>/dev/null | sed -E "s/^[^=]+=//"
  exit 0
fi
if [ "$1" = "pr" ] && [ "$2" = "edit" ]; then
  exit 0
fi
exit 0
"#;

    /// Installs the standard fake `gh` (mirror of `writeFakeGh`).
    pub(crate) fn write_fake_gh() -> FakeGh {
        write_gh_with_script(FAKE_GH_SCRIPT)
    }

    /// Installs a fake `gh` running `script`, returning a [`FakeGh`] whose `overlay` a test assigns
    /// to `Manager::gh_env_overlay`. `PATH` is the fake's dir prepended to the ambient PATH (so the
    /// labeler resolves this `gh` while `bash`/`grep`/`sed` still resolve normally), plus `GH_LOG`
    /// and `GH_PRMAP`.
    pub(crate) fn write_gh_with_script(script: &str) -> FakeGh {
        let dir = TempDir::new();
        let log_path = dir.child("gh.log");
        let prmap_path = dir.child("prmap");
        std::fs::write(&prmap_path, b"").unwrap();
        let gh_path = dir.child("gh");
        std::fs::write(&gh_path, script.as_bytes()).unwrap();
        std::fs::set_permissions(&gh_path, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut new_path = OsString::from(&dir.path);
        new_path.push(":");
        new_path.push(std::env::var_os("PATH").unwrap_or_default());
        let overlay = vec![
            (OsString::from("PATH"), new_path),
            (OsString::from("GH_LOG"), OsString::from(log_path.clone())),
            (
                OsString::from("GH_PRMAP"),
                OsString::from(prmap_path.clone()),
            ),
        ];
        FakeGh {
            _dir: dir,
            log_path,
            prmap_path,
            overlay,
        }
    }

    /// Reads the fake gh log, panicking if absent (mirror of Go's `readLog`; use only where the
    /// labeler is expected to have invoked gh at least once).
    pub(crate) fn read_log(path: &str) -> String {
        std::fs::read_to_string(path).expect("read gh log")
    }

    /// Builds a real bare-mirror-backed worktree via `ensure_from_repo` and a two-level
    /// Graphite-style stack inside it — the worktree's own `symphony/<key>` branch (stack base) ->
    /// `branchA` -> `branchB`(HEAD) — plus a sibling branch off the trunk that is NOT an ancestor of
    /// HEAD (to assert enumeration excludes sibling-run branches). Returns the worktree path and its
    /// base branch name (mirror of Go's `buildStackWorktree`).
    pub(crate) async fn build_stack_worktree(m: &Manager, origin: &str) -> (String, String) {
        let ws = m
            .ensure_from_repo(origin, "", "AIE-999")
            .await
            .expect("EnsureFromRepo");
        let wt = ws.path;

        let out = Command::new("git")
            .args(["-C", &wt, "symbolic-ref", "--short", "HEAD"])
            .output()
            .unwrap();
        assert!(out.status.success(), "resolve worktree branch");
        let base = String::from_utf8_lossy(&out.stdout).trim().to_string();

        // Stack: commit on the base branch, then branchA, then branchB(HEAD).
        git_run(&wt, &["commit", "--allow-empty", "-m", "A1"]);
        git_run(&wt, &["branch", "branchA"]);
        git_run(&wt, &["checkout", "branchA"]);
        git_run(&wt, &["commit", "--allow-empty", "-m", "B1"]);
        git_run(&wt, &["branch", "branchB"]);
        git_run(&wt, &["checkout", "branchB"]);

        // Sibling run's branch in the SHARED bare mirror: carries its own commit (not merged into
        // origin/main) and is NOT an ancestor of HEAD, so `--merged HEAD` excludes it — the
        // concurrent-run guarantee that matters for the shared mirror. Built in a throwaway worktree
        // that is then removed (the branch persists in the mirror).
        let sib_root = TempDir::new();
        let sib = join(&[&sib_root.path, "sibling-wt"]);
        git_run(
            &wt,
            &[
                "worktree",
                "add",
                "-b",
                "symphony/sibling-run",
                &sib,
                "origin/main",
            ],
        );
        git_run(&sib, &["commit", "--allow-empty", "-m", "sibling work"]);
        git_run(&wt, &["worktree", "remove", "--force", &sib]);

        (wt, base)
    }
}
