//! Per-issue workspace [`Manager`]: construction, the per-repo lock registry, path derivation, the
//! legacy (empty-URL) create/remove paths, and the before_run/after_run lifecycle hooks
//! (`manager.go`). W1 laid down construction + the legacy paths its `repo_test.go` mirror exercises;
//! W2 makes the `create_for_issue`/`remove` surface public, adds `before_run`/`after_run`, and ports
//! the `manager_test.go` cases. The post-run labeler lives in [`crate::labeler`].

use std::collections::HashMap;
use std::ffi::OsString;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Mutex as AsyncMutex;

use crate::Error;
use crate::hooks::HookRunner;
use crate::repo::repo_key;
use crate::safety::{clean, ensure_within_root, join, remove_all};
use crate::sanitize::{Workspace, sanitize_key};

/// The four lifecycle hook scripts (upstream §5.3.4). An empty string means "no hook".
#[derive(Debug, Clone, Default)]
pub struct HookScripts {
    pub after_create: String,
    pub before_run: String,
    pub after_run: String,
    pub before_remove: String,
}

/// Configures a workspace [`Manager`]. The Go `Config.Logger` field is dropped: best-effort logging
/// is elided in W1 (see [`crate::hooks`]).
#[derive(Debug, Clone)]
pub struct Config {
    /// Absolute workspace root.
    pub root: String,
    /// The lifecycle hook scripts.
    pub hooks: HookScripts,
    /// Per-hook timeout; a zero value defaults to 60s (mirrors Go's `<= 0` guard).
    pub hook_timeout: Duration,
}

/// Maps issue identifiers to per-issue workspaces and runs lifecycle hooks (upstream §9).
///
/// Legacy create/remove derive every path deterministically from the identifier + root and need no
/// locks. The repo-backed ensure/remove ([`crate::repo`]) mutate a shared per-repo bare mirror, so
/// those are serialized by a per-repo async mutex held in `repo_locks`: the registry map is guarded
/// by the std [`Mutex`] (locked only for the O(1) get-or-insert, never across an `.await`), while
/// each per-repo [`AsyncMutex`] is held across the async git ops.
pub struct Manager {
    pub(crate) root: String,
    pub(crate) hooks: HookScripts,
    pub(crate) runner: HookRunner,
    pub(crate) repo_locks: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
    /// Extra environment entries layered onto the post-run labeler's `gh` subprocesses (see
    /// [`crate::labeler`]). Empty in production, so `gh` inherits the daemon's environment unchanged
    /// — byte-for-byte Go's `cmd.Env = os.Environ()`. It exists solely as the labeler tests' seam
    /// for injecting a fake `gh` (a temp dir prepended to `PATH`, plus `GH_LOG`/`GH_PRMAP`) WITHOUT
    /// mutating the process environment, keeping those tests sound under Rust 2024's parallel-test
    /// model (`std::env::set_var` is `unsafe` and the codebase forbids it — cf. the linear/config
    /// crates' `$HOME`-based tests). Each test's Manager carries its own overlay, so parallel tests
    /// never collide.
    pub(crate) gh_env_overlay: Vec<(OsString, OsString)>,
}

impl Manager {
    /// Validates the root (must be absolute) and returns a Manager. A zero hook timeout defaults to
    /// 60s.
    pub fn new(cfg: Config) -> Result<Manager, Error> {
        if !cfg.root.starts_with('/') {
            return Err(Error::PathOutsideRoot(format!(
                "workspace root {:?} is not absolute",
                cfg.root
            )));
        }
        let timeout = if cfg.hook_timeout.is_zero() {
            Duration::from_secs(60)
        } else {
            cfg.hook_timeout
        };
        Ok(Manager {
            root: clean(&cfg.root),
            hooks: cfg.hooks,
            runner: HookRunner::new(timeout),
            repo_locks: Mutex::new(HashMap::new()),
            gh_env_overlay: Vec::new(),
        })
    }

    /// Returns the per-repo mutex that serializes all mirror mutations for `repo_url`. The same URL
    /// always maps to the same `Arc`; distinct URLs get distinct mutexes. Safe for concurrent
    /// callers.
    pub(crate) fn repo_lock(&self, repo_url: &str) -> Arc<AsyncMutex<()>> {
        self.lock_for_key(&repo_key(repo_url))
    }

    /// Returns the per-repo mutex for an already-computed RepoKey (backs [`Self::repo_lock`]; W3's
    /// GC discovers keys from the on-disk `<root>/<RepoKey>` layout and holds the key, not the URL).
    pub(crate) fn lock_for_key(&self, key: &str) -> Arc<AsyncMutex<()>> {
        let mut locks = self.repo_locks.lock().unwrap_or_else(|p| p.into_inner());
        locks
            .entry(key.to_string())
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    }

    /// The absolute workspace root.
    pub fn root(&self) -> &str {
        &self.root
    }

    /// Returns the (unvalidated) workspace path for an identifier, mirroring
    /// [`Self::ensure_from_repo`]'s path scheme: an empty repoURL maps to the legacy `<root>/<key>`,
    /// a non-empty repoURL to the repo-namespaced `<root>/<RepoKey(repoURL)>/<key>`. Callers MUST
    /// pass the same repoURL they pass to ensure so the reported path matches the actual location.
    pub fn path_for(&self, repo_url: &str, identifier: &str) -> String {
        if repo_url.is_empty() {
            join(&[&self.root, &sanitize_key(identifier)])
        } else {
            join(&[&self.root, &repo_key(repo_url), &sanitize_key(identifier)])
        }
    }

    /// Legacy (mkdir-backed) create: ensures the per-issue workspace dir exists, runs after_create
    /// on fresh creation with SYMPHONY_* env (including `project_slug`), and returns the workspace
    /// (upstream §9.2, §9.3). repoURL is "" on this path, so SYMPHONY_REPO is empty.
    ///
    /// This folds Go's public `CreateForIssue(identifier)` and private
    /// `createForIssue(projectSlug, identifier)` into one method (Rust cannot overload): calling it
    /// with `project_slug == ""` is exactly Go's exported `CreateForIssue`. It is `pub` so the
    /// orchestrator (P5) drives the same slug-less legacy path Go exposes; [`crate::repo`]'s
    /// empty-URL delegates thread the real slug.
    pub async fn create_for_issue(
        &self,
        project_slug: &str,
        identifier: &str,
    ) -> Result<Workspace, Error> {
        let key = sanitize_key(identifier);
        let path = join(&[&self.root, &key]);
        ensure_within_root(&self.root, &path)?;

        let mut created_now = false;
        // Lstat (symlink_metadata) so a planted symlink is reported as a symlink rather than
        // followed — ensure_within_root above is a LEXICAL check only.
        match std::fs::symlink_metadata(&path) {
            Ok(info) => {
                if info.file_type().is_symlink() {
                    return Err(Error::WorkspaceSymlink(format!("{path:?} is a symlink")));
                }
                if !info.is_dir() {
                    return Err(Error::WorkspaceNotDir(format!(
                        "{path:?} exists and is not a directory"
                    )));
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                crate::safety::mkdir_all(&path)
                    .map_err(|e| Error::WorkspaceCreate(e.to_string()))?;
                created_now = true;
            }
            Err(e) => return Err(Error::WorkspaceCreate(e.to_string())),
        }

        if created_now
            && !self.hooks.after_create.is_empty()
            && let Err(e) = self
                .runner
                .run_env(
                    "after_create",
                    &self.hooks.after_create,
                    &path,
                    Some(&self.hook_env("", project_slug, identifier)),
                )
                .await
        {
            // after_create failure is fatal to creation; remove the partial dir (§9.3).
            let _ = remove_all(&path);
            return Err(e);
        }
        Ok(Workspace {
            path,
            key,
            created_now,
        })
    }

    /// Runs the before_run hook (upstream §9.4) in the workspace with the SYMPHONY_* env
    /// (`repo_url`/`project_slug`/`identifier`; SYMPHONY_PROJECT always present). Its failure is
    /// FATAL to the current attempt — the caller MUST abort on a non-`Ok` return.
    pub async fn before_run(
        &self,
        ws: &Workspace,
        repo_url: &str,
        project_slug: &str,
        identifier: &str,
    ) -> Result<(), Error> {
        self.runner
            .run_env(
                "before_run",
                &self.hooks.before_run,
                &ws.path,
                Some(&self.hook_env(repo_url, project_slug, identifier)),
            )
            .await
    }

    /// Runs the after_run hook (upstream §9.4) in the workspace with the SYMPHONY_* env. It is
    /// best-effort: it RETURNS any error for the caller to log, but the caller ignores it (parity
    /// with Go, where AfterRun surfaces the error and the caller logs+ignores it).
    pub async fn after_run(
        &self,
        ws: &Workspace,
        repo_url: &str,
        project_slug: &str,
        identifier: &str,
    ) -> Result<(), Error> {
        self.runner
            .run_env(
                "after_run",
                &self.hooks.after_run,
                &ws.path,
                Some(&self.hook_env(repo_url, project_slug, identifier)),
            )
            .await
    }

    /// Legacy terminal cleanup: before_remove (best-effort) if the workspace exists, then delete the
    /// directory (upstream §9.4, §8.5). A missing workspace is a no-op. repoURL is "" here.
    ///
    /// Like [`Self::create_for_issue`], this folds Go's public `Remove(identifier)` and private
    /// `remove(projectSlug, identifier)`: calling it with `project_slug == ""` is Go's exported
    /// `Remove`. `pub` so P5 drives the slug-less legacy path; [`crate::repo`] threads the slug.
    pub async fn remove(&self, project_slug: &str, identifier: &str) -> Result<(), Error> {
        let key = sanitize_key(identifier);
        let path = join(&[&self.root, &key]);
        ensure_within_root(&self.root, &path)?;
        // Lstat so a planted symlink is rejected before before_remove runs inside the target.
        if let Ok(info) = std::fs::symlink_metadata(&path) {
            if info.file_type().is_symlink() {
                return Err(Error::WorkspaceSymlink(format!("{path:?} is a symlink")));
            }
            if info.is_dir() && !self.hooks.before_remove.is_empty() {
                // before_remove failure is ignored (§9.4).
                let _ = self
                    .runner
                    .run_env(
                        "before_remove",
                        &self.hooks.before_remove,
                        &path,
                        Some(&self.hook_env("", project_slug, identifier)),
                    )
                    .await;
            }
        }
        remove_all(&path).map_err(|e| Error::WorkspaceRemove(format!("remove {path:?}: {e}")))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{TempDir, repo_test_manager};
    use std::os::unix::fs::PermissionsExt;

    // The manager_test.go mirrors drive the legacy (empty-URL) lifecycle. Go's public
    // `CreateForIssue(id)` / `Remove(id)` are this crate's `create_for_issue("", id)` /
    // `remove("", id)` (the slug-less fold documented on those methods).

    fn scripts(
        after_create: &str,
        before_run: &str,
        after_run: &str,
        before_remove: &str,
    ) -> HookScripts {
        HookScripts {
            after_create: after_create.to_string(),
            before_run: before_run.to_string(),
            after_run: after_run.to_string(),
            before_remove: before_remove.to_string(),
        }
    }

    // Mirror of TestNewManagerRejectsRelativeRoot. (Matches on the Result rather than `unwrap_err`
    // so `Manager` need not implement `Debug`, mirroring Go's `errors.Is` check on the returned err.)
    #[test]
    fn new_manager_rejects_relative_root() {
        let res = Manager::new(Config {
            root: "relative/path".to_string(),
            hooks: HookScripts::default(),
            hook_timeout: Duration::from_secs(5),
        });
        assert!(
            matches!(res, Err(Error::PathOutsideRoot(_))),
            "want PathOutsideRoot for relative root"
        );
    }

    // Mirror of TestCreateForIssueCreatesThenReuses.
    #[tokio::test]
    async fn create_for_issue_creates_then_reuses() {
        let (m, root) = repo_test_manager(HookScripts::default());
        let ws = m.create_for_issue("", "MT-1").await.unwrap();
        assert_eq!(ws.path, join(&[&root.path, "MT-1"]));
        assert_eq!(ws.key, "MT-1");
        assert!(ws.created_now, "first create should set created_now=true");
        assert!(
            std::fs::metadata(&ws.path).is_ok_and(|i| i.is_dir()),
            "workspace dir not created"
        );

        let ws2 = m.create_for_issue("", "MT-1").await.unwrap();
        assert!(
            !ws2.created_now,
            "second create should reuse: created_now=false"
        );
    }

    // Mirror of TestCreateForIssueSanitizesIdentifier.
    #[tokio::test]
    async fn create_for_issue_sanitizes_identifier() {
        let (m, root) = repo_test_manager(HookScripts::default());
        let ws = m.create_for_issue("", "team/MT 9").await.unwrap();
        assert_eq!(ws.path, join(&[&root.path, "team_MT_9"]));
    }

    // Mirror of TestCreateForIssueAfterCreateRunsOnlyOnNewCreation.
    #[tokio::test]
    async fn create_for_issue_after_create_runs_only_on_new_creation() {
        let (m, _root) = repo_test_manager(scripts("echo x > created.txt", "", "", ""));
        let ws = m.create_for_issue("", "MT-2").await.unwrap();
        assert!(
            std::fs::metadata(join(&[&ws.path, "created.txt"])).is_ok(),
            "after_create did not run on new workspace"
        );
        // Remove the marker, recreate (reuse) — hook must NOT run again.
        std::fs::remove_file(join(&[&ws.path, "created.txt"])).unwrap();
        m.create_for_issue("", "MT-2").await.unwrap();
        assert!(
            matches!(std::fs::metadata(join(&[&ws.path, "created.txt"])), Err(e) if e.kind() == std::io::ErrorKind::NotFound),
            "after_create must not run on reuse"
        );
    }

    // Mirror of TestCreateForIssueAfterCreateFailureRemovesPartialDir.
    #[tokio::test]
    async fn create_for_issue_after_create_failure_removes_partial_dir() {
        let (m, root) = repo_test_manager(scripts("exit 1", "", "", ""));
        let err = m.create_for_issue("", "MT-3").await.unwrap_err();
        assert!(
            matches!(err, Error::HookFailed(_)),
            "got {err}, want HookFailed"
        );
        assert!(
            matches!(std::fs::metadata(join(&[&root.path, "MT-3"])), Err(e) if e.kind() == std::io::ErrorKind::NotFound),
            "partial workspace dir should be removed after after_create failure"
        );
    }

    // Mirror of TestCreateForIssueNonDirectoryCollision.
    #[tokio::test]
    async fn create_for_issue_non_directory_collision() {
        let (m, root) = repo_test_manager(HookScripts::default());
        // Pre-create a FILE where the workspace dir would go.
        std::fs::write(join(&[&root.path, "MT-4"]), b"x").unwrap();
        let err = m.create_for_issue("", "MT-4").await.unwrap_err();
        assert!(
            matches!(err, Error::WorkspaceNotDir(_)),
            "got {err}, want WorkspaceNotDir"
        );
    }

    // Mirror of TestCreateForIssueRejectsSymlinkOutsideRoot.
    #[tokio::test]
    async fn create_for_issue_rejects_symlink_outside_root() {
        let (m, root) = repo_test_manager(HookScripts::default());
        // Plant a symlink at <root>/<key> pointing outside root: lexical containment passes, but
        // reusing it would put the agent cwd at the target — an escape.
        let outside = TempDir::new();
        let link = join(&[&root.path, "MT-SYM"]);
        std::os::unix::fs::symlink(&outside.path, &link).unwrap();
        let err = m.create_for_issue("", "MT-SYM").await.unwrap_err();
        assert!(
            matches!(err, Error::WorkspaceSymlink(_)),
            "got {err}, want WorkspaceSymlink for symlink workspace path"
        );
        // The symlink must not have been followed/reused: the target dir stays empty.
        assert_eq!(
            std::fs::read_dir(&outside.path).unwrap().count(),
            0,
            "symlink target was written into"
        );
    }

    // Mirror of TestBeforeRunReturnsHookErrorFatal.
    #[tokio::test]
    async fn before_run_returns_hook_error_fatal() {
        let (m, _root) = repo_test_manager(scripts("", "exit 2", "", ""));
        let ws = m.create_for_issue("", "MT-5").await.unwrap();
        let err = m.before_run(&ws, "", "", "MT-5").await.unwrap_err();
        assert!(
            matches!(err, Error::HookFailed(_)),
            "before_run failure must be returned, got {err}"
        );
    }

    // Mirror of TestBeforeRunRunsInWorkspace.
    #[tokio::test]
    async fn before_run_runs_in_workspace() {
        let (m, _root) = repo_test_manager(scripts("", "echo ran > before.txt", "", ""));
        let ws = m.create_for_issue("", "MT-6").await.unwrap();
        m.before_run(&ws, "", "", "MT-6").await.unwrap();
        assert!(
            std::fs::metadata(join(&[&ws.path, "before.txt"])).is_ok(),
            "before_run did not run"
        );
    }

    // Mirror of TestAfterRunRunsAndSurfacesError: AfterRun surfaces the error (caller logs+ignores).
    #[tokio::test]
    async fn after_run_runs_and_surfaces_error() {
        let (m, _root) = repo_test_manager(scripts("", "", "exit 1", ""));
        let ws = m.create_for_issue("", "MT-7").await.unwrap();
        let err = m.after_run(&ws, "", "", "MT-7").await.unwrap_err();
        assert!(
            matches!(err, Error::HookFailed(_)),
            "after_run should surface the error, got {err}"
        );
    }

    // Mirror of TestBeforeRunSeesSymphonyEnv: before_run receives SYMPHONY_REPO/PROJECT/ISSUE.
    #[tokio::test]
    async fn before_run_sees_symphony_env() {
        let (m, _root) = repo_test_manager(scripts(
            "",
            r#"printf 'repo=%s project=%s issue=%s' "$SYMPHONY_REPO" "$SYMPHONY_PROJECT" "$SYMPHONY_ISSUE" > before_env.txt"#,
            "",
            "",
        ));
        let ws = m.create_for_issue("", "MT-8").await.unwrap();
        m.before_run(&ws, "git@github.com:o/r.git", "proj-z", "MT-8")
            .await
            .unwrap();
        let got = std::fs::read_to_string(join(&[&ws.path, "before_env.txt"])).unwrap();
        assert_eq!(got, "repo=git@github.com:o/r.git project=proj-z issue=MT-8");
    }

    // Mirror of TestRemoveRunsBeforeRemoveThenDeletes: before_remove writes a marker OUTSIDE the
    // workspace (so we can see it ran even after the dir is deleted). Built manually because the
    // marker path must be known before the Manager is constructed.
    #[tokio::test]
    async fn remove_runs_before_remove_then_deletes() {
        let root = TempDir::new();
        let marker = join(&[&root.path, "removed.flag"]);
        let m = Manager::new(Config {
            root: root.path.clone(),
            hooks: scripts("", "", "", &format!("echo 1 > {marker}")),
            hook_timeout: Duration::from_secs(5),
        })
        .unwrap();
        let ws = m.create_for_issue("", "MT-8").await.unwrap();
        m.remove("", "MT-8").await.unwrap();
        assert!(
            matches!(std::fs::metadata(&ws.path), Err(e) if e.kind() == std::io::ErrorKind::NotFound),
            "workspace should be deleted"
        );
        assert!(
            std::fs::metadata(&marker).is_ok(),
            "before_remove should have run"
        );
    }

    // Mirror of TestRemoveBeforeRemoveFailureStillDeletes.
    #[tokio::test]
    async fn remove_before_remove_failure_still_deletes() {
        let (m, root) = repo_test_manager(scripts("", "", "", "exit 1"));
        m.create_for_issue("", "MT-9").await.unwrap();
        m.remove("", "MT-9")
            .await
            .expect("Remove should ignore before_remove failure");
        assert!(
            matches!(std::fs::metadata(join(&[&root.path, "MT-9"])), Err(e) if e.kind() == std::io::ErrorKind::NotFound),
            "workspace should be deleted despite before_remove failure"
        );
    }

    // Mirror of TestRemoveRejectsSymlinkAndSkipsBeforeRemove.
    #[tokio::test]
    async fn remove_rejects_symlink_and_skips_before_remove() {
        let root = TempDir::new();
        let marker = join(&[&root.path, "before_remove_ran.flag"]);
        let m = Manager::new(Config {
            root: root.path.clone(),
            hooks: scripts("", "", "", &format!("echo 1 > {marker}")),
            hook_timeout: Duration::from_secs(5),
        })
        .unwrap();
        let outside = TempDir::new();
        let link = join(&[&root.path, "MT-SYM-RM"]);
        std::os::unix::fs::symlink(&outside.path, &link).unwrap();

        let err = m.remove("", "MT-SYM-RM").await.unwrap_err();
        assert!(
            matches!(err, Error::WorkspaceSymlink(_)),
            "got {err}, want WorkspaceSymlink for symlink workspace path"
        );
        // before_remove must NOT have run (no marker), and the symlink target stays untouched.
        assert!(
            matches!(std::fs::metadata(&marker), Err(e) if e.kind() == std::io::ErrorKind::NotFound),
            "before_remove must not run when the workspace path is a symlink"
        );
        assert_eq!(
            std::fs::read_dir(&outside.path).unwrap().count(),
            0,
            "symlink target was written into"
        );
    }

    // Mirror of TestRemoveMissingWorkspaceIsNoError.
    #[tokio::test]
    async fn remove_missing_workspace_is_no_error() {
        let (m, _root) = repo_test_manager(HookScripts::default());
        m.remove("", "never-existed")
            .await
            .expect("removing a missing workspace should be a no-op");
    }

    // Mirror of TestRemoveFailureWrapsErrWorkspaceRemove: drop write+exec on the workspace dir so
    // RemoveAll cannot unlink the child entry and fails inside it.
    #[tokio::test]
    async fn remove_failure_wraps_err_workspace_remove() {
        // SAFETY: geteuid() takes no arguments and has no preconditions; it is `unsafe` only as an
        // FFI import. Skip when root, where permission bits cannot block RemoveAll.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let (m, root) = repo_test_manager(HookScripts::default());
        let ws = m.create_for_issue("", "MT-10").await.unwrap();
        std::fs::write(join(&[&ws.path, "child"]), b"x").unwrap();
        std::fs::set_permissions(&ws.path, std::fs::Permissions::from_mode(0o500)).unwrap();
        // Restore perms so TempDir cleanup can succeed regardless of the outcome.
        struct Restore(String);
        impl Drop for Restore {
            fn drop(&mut self) {
                let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
            }
        }
        let _restore = Restore(join(&[&root.path, "MT-10"]));

        let err = m.remove("", "MT-10").await.unwrap_err();
        assert!(
            matches!(err, Error::WorkspaceRemove(_)),
            "got {err}, want WorkspaceRemove"
        );
    }
}
