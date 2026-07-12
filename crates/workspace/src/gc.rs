//! Workspace garbage collection (`gc.go`): [`Manager::prune_stale_worktrees`] removes per-issue
//! worktrees (and clone-mode checkouts) whose most-recent activity predates `now - max_age` and that
//! are neither in the `keep` snapshot nor reported in-use by the authoritative `live` callback. It is
//! the workspace counterpart to the store's retention prune: nothing else GCs worktrees, so a durable
//! `~/.rhapsody` root would otherwise grow without bound.
//!
//! Deviations from Go, per the crate's established conventions: `ctx context.Context` becomes
//! implicit async cancellation (drop the future — the per-worktree awaits are the cancellation
//! points, replacing Go's `ctx.Err()` loop guard); the `Config.Logger` is elided, so Go's
//! best-effort `logger.Warn/Info` calls become silent error handling with identical control flow. The
//! per-repo mutex is the crate's `tokio::sync::Mutex` (as in `repo.rs`), so `live` — which may itself
//! acquire that lock via the orchestrator round-trip — is an async callback ([`LiveCheck`]).

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, SystemTime};

use crate::Manager;
use crate::repo::{MIRRORS_DIR_NAME, clear_stale_locks, dir_is_git_checkout, looks_like_repo_key};
use crate::safety::{base, join, remove_all};

/// An async liveness predicate: returns `true` iff `path` is currently in use by a live worker (a
/// `true` verdict aborts that worktree's removal). It is the AUTHORITATIVE final check, consulted
/// immediately before each removal to close the `keep`-snapshot TOCTOU (a worker that adopted a
/// worktree after the snapshot but has written no files yet reads as stale by mtime).
///
/// For repo-backed worktrees it is invoked BEFORE the per-repo lock is taken — never under it. In
/// production `live` round-trips through the orchestrator control loop, which itself takes that same
/// per-repo lock inside `remove_worktree` during a reconcile tick; holding the lock across `live`
/// would deadlock (this task holds the lock and waits on the loop while the loop blocks on the lock).
/// Calling it lock-free breaks the cycle. It is async precisely so a callback CAN acquire that lock.
///
/// Boxed (not generic) so the common `None` case needs no turbofish; the returned future is boxed
/// because its concrete type is caller-defined, and takes an owned `String` so it can be `'static`.
pub type LiveCheck<'a> =
    &'a (dyn Fn(String) -> Pin<Box<dyn Future<Output = bool> + Send>> + Send + Sync);

/// Folds `t` into `newest`, keeping the later of the two — the `Option` port of `gc.go`'s `bump`
/// closure, where a `None` newest stands in for Go's zero `time.Time` (always before any cutoff).
fn keep_newer(newest: &mut Option<SystemTime>, t: SystemTime) {
    if newest.is_none_or(|n| t > n) {
        *newest = Some(t);
    }
}

/// Reports whether `newest` (a [`Manager::recent_activity`] result) is strictly after `cutoff` — the
/// port of Go's `recentActivity(...).After(cutoff)`. A `None` newest (nothing observed / vanished
/// worktree) is never after the cutoff, so it reads as stale, exactly as Go's zero time does.
fn activity_after(newest: Option<SystemTime>, cutoff: SystemTime) -> bool {
    matches!(newest, Some(t) if t > cutoff)
}

impl Manager {
    /// Removes per-issue worktrees whose most-recent activity predates `now - max_age` and that are
    /// not in `keep` (absolute worktree paths to preserve — e.g. currently-running issues) nor
    /// reported in-use by `live`. Returns the number of worktrees removed. `max_age` zero disables
    /// pruning (returns 0), matching the store's "retention_days<=0 => keep forever" convention.
    ///
    /// Layout. Repo-backed worktrees live at `<root>/<RepoKey>/<key>` with a sibling bare mirror at
    /// `<root>/.mirrors/<RepoKey>.git`; legacy hook-populated worktrees live at `<root>/<key>`. A
    /// top-level dir is a repo-namespace PARENT (its children are worktrees) iff a sibling mirror
    /// exists for it (worktree mode) OR its name has the RepoKey shape, has no mirror, and is not
    /// itself a git checkout (clone mode — independent clones carry no mirror). The reserved
    /// `.mirrors` store is never scanned, and mirrors are NEVER pruned (bounded by the small number
    /// of distinct repo URLs and expensive to rebuild — the object cache stays hot across restarts).
    ///
    /// Freshness. "Most-recent activity" is the newest mtime among the worktree dir, its immediate
    /// children, and (for repo-backed worktrees) the mirror's per-worktree admin dir — so a reused
    /// worktree whose top-level mtime is stale but whose files were just rewritten still reads fresh.
    ///
    /// Safety. Removal mirrors `remove_worktree`'s mechanics (`git worktree remove --force` + prune
    /// under the per-repo lock, with an `rm -rf` fallback) but deliberately SKIPS before_remove: GC
    /// is janitorial, not a lifecycle transition, and the hook's `SYMPHONY_*` identity cannot be
    /// reconstructed from a path alone. For repo-backed worktrees freshness is re-checked under the
    /// lock, so a worktree a concurrent `ensure_from_repo` just created is never pruned from under a
    /// live worker. Best-effort throughout: a per-worktree error is skipped; the scan continues.
    pub async fn prune_stale_worktrees(
        &self,
        max_age: Duration,
        keep: &HashSet<String>,
        live: Option<LiveCheck<'_>>,
    ) -> usize {
        if max_age.is_zero() {
            return 0;
        }
        let cutoff = SystemTime::now()
            .checked_sub(max_age)
            .unwrap_or(SystemTime::UNIX_EPOCH);

        // os.ReadDir reads all entries eagerly; collect so no ReadDir handle is held across an await.
        // GC's outcome (which worktrees are removed, and the count) is order-independent, so the
        // arbitrary read_dir order vs Go's sorted os.ReadDir is immaterial.
        let entries: Vec<std::fs::DirEntry> = match std::fs::read_dir(&self.root) {
            Ok(rd) => rd.flatten().collect(),
            Err(_) => return 0, // missing/unreadable root => nothing to prune (Go logs non-NotExist).
        };
        let mirror_keys = self.mirror_key_set();

        let mut removed = 0usize;
        for e in entries {
            let name = e.file_name().to_string_lossy().into_owned();
            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
            if name == MIRRORS_DIR_NAME || !is_dir {
                continue;
            }
            // A top-level dir is a repo-namespace PARENT when a sibling bare mirror exists (worktree
            // mode) OR its name is RepoKey-shaped, has no mirror, and is not itself a git checkout
            // (clone mode). The "no own .git" guard keeps a legacy hook-populated workspace whose
            // sanitized key happens to be 24-hex classified as a single leaf worktree (INF-418).
            // repo_backed is true only in the worktree case: clone children have their own .git and
            // are removed via rm -rf, worktree children via git worktree remove on the mirror.
            if mirror_keys.contains(&name)
                || (looks_like_repo_key(&name) && !dir_is_git_checkout(&join(&[&self.root, &name])))
            {
                let repo_backed = mirror_keys.contains(&name);
                let repo_dir = join(&[&self.root, &name]);
                let children: Vec<std::fs::DirEntry> = match std::fs::read_dir(&repo_dir) {
                    Ok(rd) => rd.flatten().collect(),
                    Err(_) => continue, // Go logs "read repo dir failed" and continues.
                };
                for c in children {
                    if !c.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                        continue;
                    }
                    let wt = join(&[&repo_dir, &c.file_name().to_string_lossy()]);
                    if self
                        .prune_one_worktree(&wt, &name, repo_backed, cutoff, keep, live)
                        .await
                    {
                        removed += 1;
                    }
                }
                continue;
            }
            // Legacy one-level worktree (no sibling mirror, not a RepoKey-shaped namespace).
            let wt = join(&[&self.root, &name]);
            if self
                .prune_one_worktree(&wt, "", false, cutoff, keep, live)
                .await
            {
                removed += 1;
            }
        }
        removed
    }

    /// Returns the set of RepoKeys that currently have a bare mirror under `<root>/.mirrors` (the
    /// `<key>.git` leaf, with the suffix stripped). A missing `.mirrors` dir yields the empty set, so
    /// every top-level dir is then classified as a legacy worktree (mirror of `mirrorKeySet`).
    fn mirror_key_set(&self) -> HashSet<String> {
        let mut out = HashSet::new();
        let dir = join(&[&self.root, MIRRORS_DIR_NAME]);
        let rd = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(_) => return out,
        };
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if let Some(key) = n.strip_suffix(".git") {
                out.insert(key.to_string());
            }
        }
        out
    }

    /// Removes `wt` if it is stale (no activity since `cutoff`), not in `keep`, and (when `live` is
    /// `Some`) not reported in-use by the authoritative liveness check. repo_backed worktrees are
    /// removed via the owning mirror's git worktree admin under the per-repo lock (with a freshness
    /// re-check once the lock is held); legacy/clone worktrees are `rm -rf`'d. `live` is consulted
    /// BEFORE the lock for repo-backed worktrees, to avoid the control-loop deadlock. Returns `true`
    /// iff removed.
    async fn prune_one_worktree(
        &self,
        wt: &str,
        repo_key: &str,
        repo_backed: bool,
        cutoff: SystemTime,
        keep: &HashSet<String>,
        live: Option<LiveCheck<'_>>,
    ) -> bool {
        if keep.contains(wt) {
            return false;
        }
        if activity_after(self.recent_activity(wt, repo_key, repo_backed), cutoff) {
            return false;
        }

        if !repo_backed {
            // Final authoritative liveness check immediately before removal closes the snapshot
            // TOCTOU: a worker that adopted this worktree after the keep snapshot is caught here.
            if let Some(live) = live
                && live(wt.to_string()).await
            {
                return false;
            }
            // Go logs "remove legacy worktree failed" and returns false on error.
            return remove_all(wt).is_ok();
        }

        // Authoritative liveness check BEFORE taking the per-repo lock — NOT under it (deadlock
        // avoidance; see [`LiveCheck`]). Checking live() lock-free here matches the legacy branch's
        // ordering and breaks the cycle.
        if let Some(live) = live
            && live(wt.to_string()).await
        {
            return false;
        }

        let mirror = join(&[&self.root, MIRRORS_DIR_NAME, &format!("{repo_key}.git")]);
        let lk = self.lock_for_key(repo_key);
        let _guard = lk.lock().await;
        // Re-check freshness under the lock to re-close the small TOCTOU the lock-free live() check
        // above reopens: a concurrent ensure_from_repo holds this same lock and its `git worktree
        // add` writes fresh child mtimes, so a just-dispatched worktree reads fresh here and is spared.
        if activity_after(self.recent_activity(wt, repo_key, repo_backed), cutoff) {
            return false;
        }
        let _ = clear_stale_locks(&mirror); // Go logs on error; best-effort, continue.
        let (_out, err) = self
            .git(&mirror, &["worktree", "remove", "--force", wt])
            .await;
        if err.is_some() {
            // git worktree remove failed; rm -rf fallback so cleanup never wedges.
            let _ = remove_all(wt);
        }
        let _ = self.git(&mirror, &["worktree", "prune"]).await; // Go logs on error; ignored.
        if std::fs::metadata(wt).is_ok() && remove_all(wt).is_err() {
            // rm -rf after git worktree remove failed.
            return false;
        }
        true
    }

    /// Returns the newest mtime observed for a worktree: the dir itself, its immediate children, and
    /// (when repo_backed) the mirror's per-worktree admin dir (`<mirror>/worktrees/<leaf>`), which
    /// git touches on checkout/commit. A vanished worktree returns `None` (Go's zero time — always
    /// before any cutoff), which is harmless: removing an already-gone dir is a no-op.
    ///
    /// `os.Stat` (follows symlinks) → [`std::fs::metadata`]; `os.ReadDir` + `e.Info()` (the dirent
    /// info, NOT following the final symlink) → [`std::fs::DirEntry::metadata`], which matches.
    fn recent_activity(&self, wt: &str, repo_key: &str, repo_backed: bool) -> Option<SystemTime> {
        let mut newest: Option<SystemTime> = None;
        if let Ok(md) = std::fs::metadata(wt)
            && let Ok(t) = md.modified()
        {
            keep_newer(&mut newest, t);
        }
        if let Ok(rd) = std::fs::read_dir(wt) {
            for e in rd.flatten() {
                if let Ok(md) = e.metadata()
                    && let Ok(t) = md.modified()
                {
                    keep_newer(&mut newest, t);
                }
            }
        }
        if repo_backed {
            let admin = join(&[
                &self.root,
                MIRRORS_DIR_NAME,
                &format!("{repo_key}.git"),
                "worktrees",
                &base(wt),
            ]);
            if let Ok(md) = std::fs::metadata(&admin)
                && let Ok(t) = md.modified()
            {
                keep_newer(&mut newest, t);
            }
        }
        newest
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HookScripts;
    use crate::repo::repo_key;
    use crate::testutil::{init_local_origin, repo_test_manager};
    use std::sync::Arc;
    use tokio::time::timeout;

    /// Returns a `SystemTime` `h` hours in the past (the port of `time.Now().Add(-h*time.Hour)`).
    fn hours_ago(h: u64) -> SystemTime {
        SystemTime::now()
            .checked_sub(Duration::from_secs(h * 3600))
            .expect("hours_ago underflow")
    }

    /// Sets a path's atime+mtime (the port of Go's `os.Chtimes`), used to make a worktree look idle.
    /// Works on files AND directories via `utimes(2)`, exactly as `os.Chtimes` does.
    fn chtimes(path: &str, t: SystemTime) {
        let d = t
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("time before unix epoch");
        let tv = libc::timeval {
            tv_sec: d.as_secs() as libc::time_t,
            tv_usec: d.subsec_micros() as libc::suseconds_t,
        };
        let times = [tv, tv];
        let c = std::ffi::CString::new(path).expect("path has interior NUL");
        // SAFETY: `c` is a valid NUL-terminated C string live for the call, and `times` points to a
        // 2-element timeval array (atime, mtime) as utimes(2) requires; utimes reads both and retains
        // no pointers.
        let rc = unsafe { libc::utimes(c.as_ptr(), times.as_ptr()) };
        assert_eq!(
            rc,
            0,
            "utimes({path}) failed: {}",
            std::io::Error::last_os_error()
        );
    }

    /// Backdates a repo-backed worktree — the worktree dir, each immediate child, and the mirror's
    /// per-worktree admin dir (the paths [`Manager::recent_activity`] inspects). Without the admin
    /// dir the just-created worktree's git admin entry keeps it "fresh" (mirror of `backdateWorktree`).
    fn backdate_worktree(root: &str, key: &str, leaf: &str, old: SystemTime) {
        let wt = join(&[root, key, leaf]);
        for e in std::fs::read_dir(&wt).expect("read worktree").flatten() {
            chtimes(e.path().to_str().expect("utf8 path"), old);
        }
        chtimes(&wt, old);
        let admin = join(&[root, ".mirrors", &format!("{key}.git"), "worktrees", leaf]);
        chtimes(&admin, old);
    }

    /// Backdates a workspace_mode:clone standalone clone — the clone dir and each immediate child
    /// (including the `.git` store). A clone has no shared-mirror admin dir (mirror of `backdateClone`).
    fn backdate_clone(clone_dir: &str, old: SystemTime) {
        for e in std::fs::read_dir(clone_dir).expect("read clone").flatten() {
            chtimes(e.path().to_str().expect("utf8 path"), old);
        }
        chtimes(clone_dir, old);
    }

    // Mirror of TestPruneStaleWorktrees_RemovesStaleKeepsFreshAndMirror.
    #[tokio::test]
    async fn removes_stale_keeps_fresh_and_mirror() {
        let origin = init_local_origin();
        let (m, root) = repo_test_manager(HookScripts::default());

        let stale = m.ensure_from_repo(&origin.path, "", "OLD-1").await.unwrap();
        let fresh = m.ensure_from_repo(&origin.path, "", "NEW-1").await.unwrap();
        let key = repo_key(&origin.path);
        let mirror = join(&[&root.path, ".mirrors", &format!("{key}.git")]);

        // Age OLD-1 past the retention window; leave NEW-1 fresh.
        backdate_worktree(&root.path, &key, "OLD-1", hours_ago(48));

        let removed = m
            .prune_stale_worktrees(Duration::from_secs(3600), &HashSet::new(), None)
            .await;
        assert_eq!(removed, 1, "want 1 removed (only the stale worktree)");
        assert!(
            matches!(std::fs::symlink_metadata(&stale.path), Err(e) if e.kind() == std::io::ErrorKind::NotFound),
            "stale worktree must be gone"
        );
        assert!(
            std::fs::metadata(&fresh.path).is_ok(),
            "fresh worktree must survive"
        );
        // The bare mirror (shared object cache) is never pruned.
        assert!(std::fs::metadata(&mirror).is_ok(), "mirror must survive GC");
        // git's admin entry for the removed worktree must be pruned (so a later add can reuse it).
        assert!(
            matches!(std::fs::symlink_metadata(join(&[&mirror, "worktrees", "OLD-1"])), Err(e) if e.kind() == std::io::ErrorKind::NotFound),
            "worktree admin entry must be pruned"
        );
    }

    // Mirror of TestPruneStaleWorktrees_KeepSetProtectsRunning.
    #[tokio::test]
    async fn keep_set_protects_running() {
        let origin = init_local_origin();
        let (m, root) = repo_test_manager(HookScripts::default());

        let ws = m.ensure_from_repo(&origin.path, "", "RUN-1").await.unwrap();
        let key = repo_key(&origin.path);
        backdate_worktree(&root.path, &key, "RUN-1", hours_ago(48));

        // Even though it is stale, a worktree in the keep set is preserved.
        let keep: HashSet<String> = [ws.path.clone()].into_iter().collect();
        let removed = m
            .prune_stale_worktrees(Duration::from_secs(3600), &keep, None)
            .await;
        assert_eq!(removed, 0, "keep set must protect it");
        assert!(
            std::fs::metadata(&ws.path).is_ok(),
            "kept worktree must survive"
        );
    }

    // Mirror of TestPruneStaleWorktrees_LiveCallbackProtectsAdoptedWorktree: a stale worktree absent
    // from keep but reported in-use by the authoritative live callback is protected, while a
    // never-running stale worktree is removed. Deterministic — driven by the callback's return.
    #[tokio::test]
    async fn live_callback_protects_adopted_worktree() {
        let origin = init_local_origin();
        let (m, root) = repo_test_manager(HookScripts::default());

        let adopted = m
            .ensure_from_repo(&origin.path, "", "ADOPT-1")
            .await
            .unwrap();
        let gone = m
            .ensure_from_repo(&origin.path, "", "GONE-1")
            .await
            .unwrap();
        let key = repo_key(&origin.path);
        // Both stale, NEITHER in keep — exactly the race window (a worker reused ADOPT-1 after the
        // snapshot but has written nothing yet).
        backdate_worktree(&root.path, &key, "ADOPT-1", hours_ago(48));
        backdate_worktree(&root.path, &key, "GONE-1", hours_ago(48));

        let adopted_path = adopted.path.clone();
        let live = move |path: String| -> Pin<Box<dyn Future<Output = bool> + Send>> {
            let adopted = adopted_path.clone();
            Box::pin(async move { path == adopted })
        };

        let removed = m
            .prune_stale_worktrees(Duration::from_secs(3600), &HashSet::new(), Some(&live))
            .await;
        assert_eq!(
            removed, 1,
            "want 1 removed (only the never-running stale worktree)"
        );
        assert!(
            std::fs::metadata(&adopted.path).is_ok(),
            "live-adopted worktree must survive (TOCTOU guard)"
        );
        assert!(
            matches!(std::fs::symlink_metadata(&gone.path), Err(e) if e.kind() == std::io::ErrorKind::NotFound),
            "never-running stale worktree must be gone"
        );
    }

    // Mirror of TestPruneStaleWorktrees_LiveCalledOutsideRepoLock: the control-loop deadlock
    // regression. For a repo-backed worktree, live() acquires the SAME per-repo lock the prune path
    // uses; with live() hoisted ABOVE the lock this succeeds, whereas an under-lock ordering would
    // deadlock. The whole prune is timeout-guarded so a regression surfaces as a caught hang rather
    // than wedging the suite. (Go spawns a goroutine + channel to observe the lock acquisition; in
    // the single-task async model the timeout on the awaited prune is a sufficient and simpler probe:
    // if prune held the lock across live's `.lock().await`, that await could never resolve.)
    #[tokio::test]
    async fn live_called_outside_repo_lock() {
        let origin = init_local_origin();
        let (m, root) = repo_test_manager(HookScripts::default());
        let m = Arc::new(m);

        let ws = m
            .ensure_from_repo(&origin.path, "", "LOCK-1")
            .await
            .unwrap();
        let key = repo_key(&origin.path);
        backdate_worktree(&root.path, &key, "LOCK-1", hours_ago(48));

        // The callback stands in for the control loop's evWorkspaceInUse handler: it needs the very
        // per-repo lock the prune path takes. If prune still held it here, this lock().await would
        // deadlock. We report not-in-use so removal proceeds (proving liveness was consulted lock-free).
        let m2 = Arc::clone(&m);
        let origin_path = origin.path.clone();
        let live = move |_path: String| -> Pin<Box<dyn Future<Output = bool> + Send>> {
            let m = Arc::clone(&m2);
            let origin = origin_path.clone();
            Box::pin(async move {
                let lk = m.repo_lock(&origin);
                let _g = lk.lock().await;
                false
            })
        };

        let removed = timeout(
            Duration::from_secs(5),
            m.prune_stale_worktrees(Duration::from_secs(3600), &HashSet::new(), Some(&live)),
        )
        .await
        .expect("deadlock: prune held the per-repo lock across live()");
        assert_eq!(removed, 1);
        assert!(
            matches!(std::fs::symlink_metadata(&ws.path), Err(e) if e.kind() == std::io::ErrorKind::NotFound),
            "worktree must be gone after a not-in-use verdict"
        );
    }

    // Mirror of TestPruneStaleWorktrees_ZeroMaxAgeIsNoop.
    #[tokio::test]
    async fn zero_max_age_is_noop() {
        let origin = init_local_origin();
        let (m, root) = repo_test_manager(HookScripts::default());

        let ws = m
            .ensure_from_repo(&origin.path, "", "KEEP-1")
            .await
            .unwrap();
        backdate_worktree(
            &root.path,
            &repo_key(&origin.path),
            "KEEP-1",
            hours_ago(1000),
        );

        // max_age zero means "keep forever" — no scan, no removal, regardless of age.
        let removed = m
            .prune_stale_worktrees(Duration::ZERO, &HashSet::new(), None)
            .await;
        assert_eq!(removed, 0, "want 0 for max_age=0");
        assert!(
            std::fs::metadata(&ws.path).is_ok(),
            "worktree must survive max_age=0"
        );
    }

    // Mirror of TestPruneStaleWorktrees_LegacyOneLevelWorktree: no mirror => the top-level dir is
    // itself a legacy (hook-populated) worktree, removed via rm -rf.
    #[tokio::test]
    async fn legacy_one_level_worktree() {
        let (m, _root) = repo_test_manager(HookScripts::default());

        let ws = m.create_for_issue("", "LEG-1").await.unwrap();
        chtimes(&ws.path, hours_ago(48));

        let removed = m
            .prune_stale_worktrees(Duration::from_secs(3600), &HashSet::new(), None)
            .await;
        assert_eq!(removed, 1, "want 1 (stale legacy worktree)");
        assert!(
            matches!(std::fs::symlink_metadata(&ws.path), Err(e) if e.kind() == std::io::ErrorKind::NotFound),
            "legacy worktree must be gone"
        );
    }

    // Mirror of TestPruneStaleWorktrees_MissingRootIsNoop.
    #[tokio::test]
    async fn missing_root_is_noop() {
        let (m, root) = repo_test_manager(HookScripts::default());
        std::fs::remove_dir_all(&root.path).unwrap();
        let removed = m
            .prune_stale_worktrees(Duration::from_secs(3600), &HashSet::new(), None)
            .await;
        assert_eq!(removed, 0, "want 0 for missing root");
    }

    // Mirror of TestPruneStaleWorktrees_HexLegacyWorkspaceWithGitIsLeaf (gc_clone_test.go): a legacy
    // hook-populated workspace whose sanitized identifier coincidentally matches the 24-hex RepoKey
    // shape (and was `git clone`d into place, so it has its own .git) must be treated as a single
    // LEAF worktree — removed wholesale — NOT a namespace whose children get pruned individually.
    #[tokio::test]
    async fn hex_legacy_workspace_with_git_is_leaf() {
        let (m, root) = repo_test_manager(HookScripts::default());

        let hex_name = "0123456789abcdef01234567"; // 24 lowercase hex — collides with the RepoKey shape
        let leaf = join(&[&root.path, hex_name]);
        std::fs::create_dir_all(join(&[&leaf, ".git"])).unwrap();
        let inner = join(&[&leaf, "src"]);
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::write(join(&[&inner, "main.go"]), "package main\n").unwrap();

        // Backdate everything so the leaf reads as stale.
        let old = hours_ago(48);
        for p in [
            join(&[&inner, "main.go"]),
            inner.clone(),
            join(&[&leaf, ".git"]),
            leaf.clone(),
        ] {
            chtimes(&p, old);
        }

        let removed = m
            .prune_stale_worktrees(Duration::from_secs(3600), &HashSet::new(), None)
            .await;
        assert_eq!(removed, 1, "want 1 (the whole leaf workspace)");
        // Leaf treatment: the entire dir is gone. A misclassification-as-parent would instead delete
        // the `src` child while leaving the <24-hex> dir behind.
        assert!(
            matches!(std::fs::symlink_metadata(&leaf), Err(e) if e.kind() == std::io::ErrorKind::NotFound),
            "legacy hex workspace must be removed wholesale (leaf)"
        );
    }

    // Mirror of TestPruneStaleWorktrees_CloneModeParentPrunesChildren (gc_clone_test.go): a clone-mode
    // repo-namespace dir (<root>/<RepoKey>/<key> with NO sibling mirror) is a PARENT whose stale
    // per-issue clones are pruned individually — NOT misclassified as a legacy one-level worktree and
    // rm -rf'd wholesale (which would destroy a live sibling clone).
    #[tokio::test]
    async fn clone_mode_parent_prunes_children() {
        let origin = init_local_origin();
        let (m, root) = repo_test_manager(HookScripts::default());

        let stale = m
            .ensure_clone_from_repo(&origin.path, "", "CL-OLD")
            .await
            .unwrap();
        let fresh = m
            .ensure_clone_from_repo(&origin.path, "", "CL-NEW")
            .await
            .unwrap();
        let repo_parent = join(&[&root.path, &repo_key(&origin.path)]);
        // Clone mode creates NO bare mirror, so the parent has no sibling under .mirrors.
        assert!(
            matches!(std::fs::symlink_metadata(m.mirror_dir(&origin.path)), Err(e) if e.kind() == std::io::ErrorKind::NotFound),
            "clone mode must not create a mirror"
        );

        backdate_clone(&stale.path, hours_ago(48));

        let removed = m
            .prune_stale_worktrees(Duration::from_secs(3600), &HashSet::new(), None)
            .await;
        assert_eq!(removed, 1, "want 1 (only the stale clone)");
        assert!(
            matches!(std::fs::symlink_metadata(&stale.path), Err(e) if e.kind() == std::io::ErrorKind::NotFound),
            "stale clone must be gone"
        );
        assert!(
            std::fs::metadata(&fresh.path).is_ok(),
            "fresh clone must survive"
        );
        // The repo-namespace parent dir must survive (it is a parent, not a worktree leaf).
        assert!(
            std::fs::metadata(&repo_parent).is_ok(),
            "repo-namespace parent must survive GC"
        );
    }
}
