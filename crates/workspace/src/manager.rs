//! Per-issue workspace [`Manager`]: construction, the per-repo lock registry, path derivation, and
//! the legacy (empty-URL) create/remove paths (`manager.go`, the subset W1's `repo_test.go` mirror
//! exercises). `BeforeRun`/`AfterRun`, the public `CreateForIssue`/`Remove` wrappers, and the
//! labeler land in W2.

use std::collections::HashMap;
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
    pub(crate) async fn create_for_issue(
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

    /// Legacy terminal cleanup: before_remove (best-effort) if the workspace exists, then delete the
    /// directory (upstream §9.4, §8.5). A missing workspace is a no-op. repoURL is "" here.
    pub(crate) async fn remove(&self, project_slug: &str, identifier: &str) -> Result<(), Error> {
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
