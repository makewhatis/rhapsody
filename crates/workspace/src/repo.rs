//! The git layer (`repo.go`): the workspace-creation API in clone + worktree modes, the per-repo
//! bare-mirror cache, and `symphony/<key>` branch naming — driven via the `git` subprocess exactly
//! as Go shells out (no libgit2/gitoxide).
//!
//! Every method hangs off [`Manager`] (as in Go: `repo.go` and `manager.go` share one package).
//! Go's `ctx context.Context` becomes implicit async cancellation (drop the future); the git helper
//! returns `(output, Option<Error>)` so a caller gets the combined stdout+stderr on success AND
//! failure — exactly as Go's `m.git` returns `(out, err)` — to re-wrap into a specific sentinel or
//! classify (e.g. [`git_path_absent`]).

use std::ffi::{OsStr, OsString};
use std::fmt::Write as _;
use std::process::Stdio;

use sha2::{Digest, Sha256};
use tokio::process::Command;

use crate::Error;
use crate::Manager;
use crate::hooks::truncate_output;
use crate::safety::{dir, ensure_within_root, join, mkdir_all, remove_all};
use crate::sanitize::{Workspace, sanitize_key};

/// The single reserved root-level component that holds every bare mirror
/// (`<root>/.mirrors/<RepoKey>.git`). A literal identifier of `.mirrors` sanitizes to `.mirrors`
/// (`'.'` is permitted), so ensure/remove reject that collision defensively. `pub(crate)` so the
/// workspace GC ([`crate::gc`]) can skip it while scanning root and locate a worktree's mirror.
pub(crate) const MIRRORS_DIR_NAME: &str = ".mirrors";

/// Derives a stable, filesystem-safe single path component from a repo URL: the first 24 hex chars
/// of SHA-256 over the trimmed URL. It never contains a path separator and is identical across runs
/// and processes — and byte-identical to Go's `RepoKey` (crypto/sha256), so a Rust-provisioned
/// workspace lands at the same `<root>/<RepoKey>/<key>` path the Go daemon would use.
pub fn repo_key(repo_url: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(repo_url.trim().as_bytes());
    let digest = hasher.finalize();
    // The first 24 hex chars == the first 12 bytes rendered lowercase-hex.
    let mut s = String::with_capacity(24);
    for b in digest.iter().take(12) {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Reports whether `name` has the exact shape [`repo_key`] produces: 24 lowercase hex chars. The
/// workspace GC uses it to classify a top-level dir as a repo-namespace PARENT even when no sibling
/// bare mirror exists — the workspace_mode:clone case, where per-issue clones live at
/// `<root>/<RepoKey>/<key>` with NO mirror. A legacy one-level worktree dir is `sanitize_key`-ed,
/// which for a real tracker identifier (e.g. `"INF-418"`) is never pure 24-hex, so this never
/// reclassifies a genuine legacy worktree as a parent (INF-418).
pub(crate) fn looks_like_repo_key(name: &str) -> bool {
    name.len() == 24 && name.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f'))
}

/// Reports whether `dir` is itself a git checkout — it contains a `.git` entry (a directory for a
/// standalone clone, or a file for a worktree's gitdir link). The workspace GC uses it to
/// distinguish a clone-namespace PARENT (no `.git` of its own; its children are the checkouts) from
/// a leaf worktree/clone (has `.git`). This keeps a legacy hook-populated workspace whose
/// `sanitize_key`-ed identifier coincidentally matches the 24-hex RepoKey shape (and so was
/// `git clone`d into place, leaving a `.git`) classified as a single leaf — never as a namespace
/// whose children get pruned individually (INF-418). Uses `symlink_metadata` (Go's `os.Lstat`), so a
/// symlinked `.git` still counts.
pub(crate) fn dir_is_git_checkout(dir: &str) -> bool {
    std::fs::symlink_metadata(join(&[dir, ".git"])).is_ok()
}

/// Applies the hardened, non-interactive git environment on top of `base` (the port of Go's
/// `gitEnv` building on `os.Environ()`): no credential/SSH prompts (a missing key fails fast rather
/// than hanging the daemon) and no system config. An operator-set `GIT_SSH_COMMAND` in `base` is
/// preserved — the default is only appended when absent, so (since `exec` uses the last duplicate)
/// it can never clobber an operator-provided command at exec time.
fn git_env_from(mut env: Vec<(OsString, OsString)>) -> Vec<(OsString, OsString)> {
    let has_ssh_command = env
        .iter()
        .any(|(k, _)| k.as_os_str() == OsStr::new("GIT_SSH_COMMAND"));
    env.push(("GIT_TERMINAL_PROMPT".into(), "0".into()));
    env.push(("GIT_ASKPASS".into(), "".into()));
    env.push(("SSH_ASKPASS".into(), "".into()));
    env.push(("GIT_CONFIG_NOSYSTEM".into(), "1".into()));
    if !has_ssh_command {
        env.push(("GIT_SSH_COMMAND".into(), "ssh -oBatchMode=yes".into()));
    }
    env
}

/// The hardened git environment layered on the current process environment.
fn git_env() -> Vec<(OsString, OsString)> {
    git_env_from(std::env::vars_os().collect())
}

/// Removes well-known stale git lock files under `git_dir` (best-effort, idempotent): the top-level
/// locks AND any `*.lock` under `<git_dir>/worktrees/*/` (per-worktree admin), so a worktree can be
/// safely reused after a hard SIGKILL. Non-`.lock` files are never touched. `pub(crate)` so the
/// workspace GC ([`crate::gc`]) can clear the mirror's stale locks before `git worktree remove`.
pub(crate) fn clear_stale_locks(git_dir: &str) -> std::io::Result<()> {
    for name in ["index.lock", "HEAD.lock", "config.lock", "packed-refs.lock"] {
        match std::fs::remove_file(join(&[git_dir, name])) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    // Glob equivalent of <git_dir>/worktrees/*/*.lock.
    let worktrees = join(&[git_dir, "worktrees"]);
    match std::fs::read_dir(&worktrees) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry?;
                if !entry.file_type()?.is_dir() {
                    continue;
                }
                for f in std::fs::read_dir(entry.path())? {
                    let f = f?;
                    let p = f.path();
                    let is_lock = p
                        .file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.ends_with(".lock"));
                    if is_lock {
                        match std::fs::remove_file(&p) {
                            Ok(()) => {}
                            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                            Err(e) => return Err(e),
                        }
                    }
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {} // no worktrees dir → no matches
        Err(e) => return Err(e),
    }
    Ok(())
}

/// Reports whether a `git cat-file` failure output indicates a genuinely-absent tree path rather
/// than a transient infrastructure error. Git prints "…does not exist…" for a path missing from the
/// tree; anything else (timeout, lock contention, I/O, a corrupt object) is unverifiable, so
/// [`Manager::prompt_file_in_repo`] never reports a false "missing" on a transient error.
fn git_path_absent(cat_file_output: &str) -> bool {
    cat_file_output.to_lowercase().contains("does not exist")
}

impl Manager {
    /// The bare-mirror directory for `repo_url`: `<root>/.mirrors/<RepoKey>.git`. A sibling of every
    /// per-issue worktree under root, so the existing containment invariant covers it.
    pub(crate) fn mirror_dir(&self, repo_url: &str) -> String {
        let bare = format!("{}.git", repo_key(repo_url));
        join(&[&self.root, MIRRORS_DIR_NAME, &bare])
    }

    /// Runs `git -c gc.auto=0 [-C dir] args...` with the hardened env, returning the combined
    /// stdout+stderr and, on a non-zero exit or spawn failure, an [`Error::GitFailed`]. The output
    /// is returned on success AND failure (mirroring Go's `m.git` returning `(out, err)`).
    ///
    /// `pub(crate)` so the post-run [`crate::labeler`] can enumerate stack branches through the same
    /// helper Go's labeler does (`m.git`).
    pub(crate) async fn git(&self, dir: &str, args: &[&str]) -> (String, Option<Error>) {
        let mut full: Vec<&str> = Vec::with_capacity(args.len() + 4);
        full.push("-c");
        full.push("gc.auto=0");
        if !dir.is_empty() {
            full.push("-C");
            full.push(dir);
        }
        full.extend_from_slice(args);

        let mut cmd = Command::new("git");
        cmd.args(&full)
            .env_clear()
            .envs(git_env())
            .stdin(Stdio::null());
        match cmd.output().await {
            Ok(output) => {
                let mut combined = output.stdout;
                combined.extend_from_slice(&output.stderr);
                let out = String::from_utf8_lossy(&combined).into_owned();
                if output.status.success() {
                    (out, None)
                } else {
                    let err = Error::GitFailed(format!(
                        "git {}: {}: {}",
                        args.join(" "),
                        output.status,
                        truncate_output(&combined)
                    ));
                    (out, Some(err))
                }
            }
            Err(e) => (
                String::new(),
                Some(Error::GitFailed(format!("git {}: {e}", args.join(" ")))),
            ),
        }
    }

    /// Returns the bare-mirror dir for `repo_url`, cloning it (bare, gc.auto=0) if absent and
    /// otherwise freshening it with a pruning fetch. The caller MUST hold `repo_lock(repo_url)`.
    /// Clone failure is fatal; fetch failure on reuse is tolerated (stale mirror contents are
    /// acceptable — the daemon must never crash on a fetch error).
    ///
    /// CRITICAL: the mirror fetches origin's heads into `refs/remotes/origin/*` (NOT
    /// `refs/heads/*`). Worktrees create local branches `refs/heads/symphony/<key>`; a
    /// `+refs/heads/*:refs/heads/*` pruning fetch would delete those and corrupt every live
    /// worktree. The remote-tracking namespace keeps `symphony/*` intact while advancing
    /// `origin/<branch>`. A freshly-cloned bare repo has an empty `remote.origin.fetch`, so we set
    /// it.
    async fn ensure_mirror(&self, repo_url: &str) -> Result<String, Error> {
        let dir_path = self.mirror_dir(repo_url);
        // 3-way stat: Ok → reuse; NotFound → clone; other → fatal (a real stat error must not be
        // silently treated as reuse).
        match std::fs::metadata(&dir_path) {
            Ok(_) => {} // reuse: handled below
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                mkdir_all(&dir(&dir_path))
                    .map_err(|e| Error::MirrorClone(format!("mkdir .mirrors: {e}")))?;
                // dir=="" => no -C; the clone destination is the absolute mirror path.
                let (out, err) = self
                    .git(
                        "",
                        &[
                            "clone",
                            "--bare",
                            "--config",
                            "gc.auto=0",
                            repo_url,
                            &dir_path,
                        ],
                    )
                    .await;
                if err.is_some() {
                    let _ = remove_all(&dir_path);
                    return Err(Error::MirrorClone(format!(
                        "clone --bare: {}",
                        truncate_output(out.as_bytes())
                    )));
                }
                // Route future fetches into the remote-tracking namespace.
                let (out, err) = self
                    .git(
                        &dir_path,
                        &[
                            "config",
                            "remote.origin.fetch",
                            "+refs/heads/*:refs/remotes/origin/*",
                        ],
                    )
                    .await;
                if err.is_some() {
                    let _ = remove_all(&dir_path);
                    return Err(Error::MirrorClone(format!(
                        "config remote.origin.fetch: {}",
                        truncate_output(out.as_bytes())
                    )));
                }
                let (out, err) = self.git(&dir_path, &["fetch", "--prune", "origin"]).await;
                if err.is_some() {
                    let _ = remove_all(&dir_path);
                    return Err(Error::MirrorClone(format!(
                        "initial fetch: {}",
                        truncate_output(out.as_bytes())
                    )));
                }
                // Record origin/HEAD; non-fatal (defaultBranch falls back to origin/main|master).
                let _ = self
                    .git(&dir_path, &["remote", "set-head", "origin", "-a"])
                    .await;
                return Ok(dir_path);
            }
            Err(e) => {
                return Err(Error::MirrorClone(format!(
                    "stat mirror dir {dir_path:?}: {e}"
                )));
            }
        }
        // Reuse: clear any stale locks left by a hard kill, then freshen (tolerate fetch failure).
        let _ = clear_stale_locks(&dir_path);
        let _ = self.git(&dir_path, &["fetch", "--prune", "origin"]).await;
        Ok(dir_path)
    }

    /// Resolves the SHORT name of the mirror's default branch (e.g. "main"). Prefers the recorded
    /// `refs/remotes/origin/HEAD` symbolic ref (whose `--short` form is `origin/main`, from which
    /// the leading `origin/` is stripped); falls back to the remote-tracking `main` then `master`.
    /// Probes `refs/remotes/origin/*` (the live fetched refs), NOT `refs/heads/*` (frozen at clone
    /// time under the remote-tracking refspec).
    ///
    /// `pub(crate)` so the post-run [`crate::labeler`] can resolve the trunk from a worktree (which
    /// shares the mirror's remote-tracking refs), exactly as Go's labeler calls `m.defaultBranch`.
    pub(crate) async fn default_branch(&self, mirror_dir: &str) -> Result<String, Error> {
        let (out, err) = self
            .git(
                mirror_dir,
                &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
            )
            .await;
        if err.is_none() {
            let trimmed = out.trim();
            let name = trimmed.strip_prefix("origin/").unwrap_or(trimmed);
            if !name.is_empty() {
                return Ok(name.to_string());
            }
        }
        for cand in ["main", "master"] {
            let refname = format!("refs/remotes/origin/{cand}");
            let (_out, err) = self
                .git(mirror_dir, &["rev-parse", "--verify", "--quiet", &refname])
                .await;
            if err.is_none() {
                return Ok(cand.to_string());
            }
        }
        Err(Error::GitFailed(format!(
            "could not resolve default branch for mirror {mirror_dir}"
        )))
    }

    /// Reports whether `rel_path` is a USABLE prompt at the default-branch tip of `repo_url`'s
    /// already-synced bare mirror — present AND non-empty (whitespace-only counts as empty). It is
    /// deliberately READ-ONLY and best-effort: it NEVER clones or fetches (an unsynced mirror
    /// returns `(false, false)` = "cannot verify"). Returns `(exists, checked)`: when `checked` is
    /// true, `exists` is authoritative; any infrastructure failure yields `(false, false)`, an
    /// empty repoURL/relPath likewise (INF-279).
    pub async fn prompt_file_in_repo(&self, repo_url: &str, rel_path: &str) -> (bool, bool) {
        let rel_path = rel_path.trim();
        if repo_url.trim().is_empty() || rel_path.is_empty() {
            return (false, false);
        }
        let mirror = self.mirror_dir(repo_url);
        match std::fs::metadata(&mirror) {
            Ok(m) if m.is_dir() => {}
            _ => return (false, false), // mirror not synced yet — cannot verify, no flag
        }
        let branch = match self.default_branch(&mirror).await {
            Ok(b) => b,
            Err(_) => return (false, false), // default branch unresolved — cannot verify
        };
        // The bare mirror has no working tree, so read the blob against the remote-tracking ref
        // worktrees are based on (Unix ToSlash is a no-op, so rel_path is used as-is).
        let obj = format!("refs/remotes/origin/{branch}:{rel_path}");
        let (out, err) = self.git(&mirror, &["cat-file", "-p", &obj]).await;
        if err.is_some() {
            if git_path_absent(&out) {
                return (false, true); // genuinely absent at that tree path
            }
            return (false, false); // transient git/infra error — cannot verify, no false flag
        }
        if out.trim().is_empty() {
            return (false, true); // present but empty → unusable, treated as missing
        }
        (true, true)
    }

    /// The EXTRA SYMPHONY_* env entries to layer on top of the inherited environment (mirrors Go's
    /// `hookEnv`). SYMPHONY_PROJECT is always set (possibly empty) so hooks can rely on its presence.
    pub(crate) fn hook_env(
        &self,
        repo_url: &str,
        project_slug: &str,
        identifier: &str,
    ) -> Vec<String> {
        vec![
            format!("SYMPHONY_REPO={repo_url}"),
            format!("SYMPHONY_PROJECT={project_slug}"),
            format!("SYMPHONY_ISSUE={identifier}"),
        ]
    }

    /// The single workspace entry point. `repo_url == ""` falls back to the legacy hook-populated
    /// workspace via [`Manager::create_for_issue`]. A non-empty repoURL clones/freshens a per-repo
    /// bare mirror and adds a worktree on branch `symphony/<key>` off the mirror's default branch
    /// (CreatedNow=true, runs fatal after_create). On reuse it freshens the mirror but NEVER resets
    /// or checks out the worktree — in-progress work is preserved; CreatedNow=false and after_create
    /// does not run. All mirror mutations are serialized by the per-repo mutex.
    pub async fn ensure_from_repo(
        &self,
        repo_url: &str,
        project_slug: &str,
        identifier: &str,
    ) -> Result<Workspace, Error> {
        if repo_url.is_empty() {
            return self.create_for_issue(project_slug, identifier).await;
        }
        let key = sanitize_key(identifier);
        if key == MIRRORS_DIR_NAME {
            return Err(Error::WorktreeOutsideRoot(format!(
                "identifier {identifier:?} sanitizes to the reserved mirror dir {MIRRORS_DIR_NAME:?}"
            )));
        }
        // Worktrees are namespaced by repo: <root>/<RepoKey(repoURL)>/<key>, so two DIFFERENT repos
        // sharing an issue identifier never collide.
        let repo_dir = join(&[&self.root, &repo_key(repo_url)]);
        let path = join(&[&repo_dir, &key]);
        let branch = format!("symphony/{key}");

        ensure_within_root(&self.root, &path).map_err(|e| {
            Error::WorktreeOutsideRoot(format!("unsafe worktree path {path:?}: {e}"))
        })?;

        // Hold the per-repo mirror lock only for the mirror-mutating phase; release it BEFORE the
        // (arbitrarily long) after_create hook, which only touches the unshared per-issue dir.
        let lk = self.repo_lock(repo_url);
        let mut guard = Some(lk.clone().lock_owned().await);

        let mirror = self.ensure_mirror(repo_url).await?;

        // Lstat (symlink_metadata) so an existing symlink is reported as a symlink rather than
        // followed to its target — ensure_within_root is a LEXICAL check only.
        match std::fs::symlink_metadata(&path) {
            Ok(fi) => {
                if fi.file_type().is_symlink() {
                    return Err(Error::WorkspaceSymlink(format!("{path:?} is a symlink")));
                }
                if !fi.is_dir() {
                    return Err(Error::WorkspaceNotDir(format!(
                        "{path:?} exists and is not a directory"
                    )));
                }
                // Reuse: do NOT reset/checkout (preserve WIP). ensure_mirror already cleared stale
                // mirror locks on this reuse path.
                return Ok(Workspace {
                    path,
                    key,
                    created_now: false,
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {} // fall through to create
            Err(e) => return Err(Error::WorkspaceStat(e.to_string())),
        }

        // Create: add a worktree on a fresh symphony/<key> branch off the default branch.
        let _ = clear_stale_locks(&mirror);
        let base = self.default_branch(&mirror).await?;
        // Best-effort prune of stale worktree admin entries before adding (idempotent).
        let _ = self.git(&mirror, &["worktree", "prune"]).await;
        // The <root>/<RepoKey> parent must exist before the first worktree add for this repo.
        mkdir_all(&repo_dir).map_err(|e| Error::WorktreeAdd(format!("mkdir repo dir: {e}")))?;
        // Base the new branch on the freshly-fetched remote-tracking ref (origin/<default>).
        let origin_ref = format!("origin/{base}");
        let (out, err) = self
            .git(
                &mirror,
                &["worktree", "add", "-B", &branch, &path, &origin_ref],
            )
            .await;
        if err.is_some() {
            return Err(Error::WorktreeAdd(format!(
                "worktree add: {}",
                truncate_output(out.as_bytes())
            )));
        }

        let ws = Workspace {
            path: path.clone(),
            key: key.clone(),
            created_now: true,
        };

        // The per-issue worktree now exists and is unshared: release the mirror lock BEFORE the
        // after_create hook so concurrent same-repo workers are not serialized across it.
        drop(guard.take());

        // after_create is FATAL: roll back the brand-new worktree (remove --force + prune, with an
        // rm -rf fallback) on failure.
        if !self.hooks.after_create.is_empty()
            && let Err(e) = self
                .runner
                .run_env(
                    "after_create",
                    &self.hooks.after_create,
                    &path,
                    Some(&self.hook_env(repo_url, project_slug, identifier)),
                )
                .await
        {
            // Re-acquire the mirror lock: the rollback mutates shared mirror admin state.
            let _rollback_guard = lk.clone().lock_owned().await;
            let (_out, rm_err) = self
                .git(&mirror, &["worktree", "remove", "--force", &path])
                .await;
            if rm_err.is_some() {
                // Force-remove the dir so a failed `git worktree remove` can't leave an orphan
                // that the next ensure would reuse (silently skipping after_create).
                let _ = remove_all(&path);
            }
            let _ = self.git(&mirror, &["worktree", "prune"]).await;
            return Err(e);
        }
        Ok(ws)
    }

    /// The `workspace_mode:clone` provisioning entry point — the sibling of [`Self::ensure_from_repo`].
    /// Instead of a shared bare mirror + git worktree, it performs a full `git clone` into the
    /// per-issue dir (its own `.git`, NO shared mirror and NO `--reference`), so the checkout has no
    /// cross-ticket worktree lock and any origin branch is freely checkout-able. The path scheme,
    /// fresh `symphony/<key>` branch, fatal-on-create after_create, and WIP-preserving reuse all
    /// match the worktree path; the clone is serialized by the per-repo mutex (released before the
    /// hook).
    pub async fn ensure_clone_from_repo(
        &self,
        repo_url: &str,
        project_slug: &str,
        identifier: &str,
    ) -> Result<Workspace, Error> {
        if repo_url.is_empty() {
            return self.create_for_issue(project_slug, identifier).await;
        }
        let key = sanitize_key(identifier);
        if key == MIRRORS_DIR_NAME {
            return Err(Error::WorktreeOutsideRoot(format!(
                "identifier {identifier:?} sanitizes to the reserved mirror dir {MIRRORS_DIR_NAME:?}"
            )));
        }
        let repo_dir = join(&[&self.root, &repo_key(repo_url)]);
        let path = join(&[&repo_dir, &key]);
        let branch = format!("symphony/{key}");

        ensure_within_root(&self.root, &path)
            .map_err(|e| Error::WorktreeOutsideRoot(format!("unsafe clone path {path:?}: {e}")))?;

        let lk = self.repo_lock(repo_url);
        let mut guard = Some(lk.clone().lock_owned().await);

        // Lstat so an existing symlink is reported rather than followed — the same reuse-path escape
        // defense as ensure_from_repo.
        match std::fs::symlink_metadata(&path) {
            Ok(fi) => {
                if fi.file_type().is_symlink() {
                    return Err(Error::WorkspaceSymlink(format!("{path:?} is a symlink")));
                }
                if !fi.is_dir() {
                    return Err(Error::WorkspaceNotDir(format!(
                        "{path:?} exists and is not a directory"
                    )));
                }
                // Reuse: preserve WIP; CreatedNow=false; after_create does not run. (Go additionally
                // logs LOUDLY when reusing a non-clone checkout; that warning has no behavioral
                // effect and is elided in W1.)
                return Ok(Workspace {
                    path,
                    key,
                    created_now: false,
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {} // fall through to create
            Err(e) => return Err(Error::WorkspaceStat(e.to_string())),
        }

        mkdir_all(&repo_dir).map_err(|e| Error::CloneFailed(format!("mkdir repo dir: {e}")))?;
        // Full clone (all branches into refs/remotes/origin/*, default branch checked out), gc.auto
        // persisted off. dir=="" => no -C; the clone destination is the absolute per-issue path.
        let (out, err) = self
            .git("", &["clone", "--config", "gc.auto=0", repo_url, &path])
            .await;
        if err.is_some() {
            let _ = remove_all(&path); // roll back any partial clone
            return Err(Error::CloneFailed(format!(
                "clone: {}",
                truncate_output(out.as_bytes())
            )));
        }
        // Create + switch to symphony/<key> off the cloned default HEAD (analog of the worktree
        // path's `worktree add -B branch … origin/<default>`).
        let (out, err) = self.git(&path, &["checkout", "-B", &branch]).await;
        if err.is_some() {
            let _ = remove_all(&path);
            return Err(Error::CloneFailed(format!(
                "checkout -B {branch}: {}",
                truncate_output(out.as_bytes())
            )));
        }

        let ws = Workspace {
            path: path.clone(),
            key: key.clone(),
            created_now: true,
        };

        // Release the per-repo lock BEFORE the after_create hook (the hook only touches `path`).
        drop(guard.take());

        // after_create is FATAL: a standalone clone has no mirror admin, so rm -rf is the whole
        // rollback.
        if !self.hooks.after_create.is_empty()
            && let Err(e) = self
                .runner
                .run_env(
                    "after_create",
                    &self.hooks.after_create,
                    &path,
                    Some(&self.hook_env(repo_url, project_slug, identifier)),
                )
                .await
        {
            let _ = remove_all(&path);
            return Err(e);
        }
        Ok(ws)
    }

    /// Terminal cleanup of a repo-backed worktree. `repo_url == ""` delegates to the legacy
    /// [`Manager::remove`]. Otherwise it runs best-effort before_remove (logged+ignored), then
    /// `git worktree remove --force` + `git worktree prune` on the mirror, serialized by the
    /// per-repo mutex. Removing a worktree that does not exist is a no-op and does NOT run
    /// before_remove. A standalone clone (`.git` is a directory) is removed directly instead.
    pub async fn remove_worktree(
        &self,
        repo_url: &str,
        project_slug: &str,
        identifier: &str,
    ) -> Result<(), Error> {
        if repo_url.is_empty() {
            return self.remove(project_slug, identifier).await;
        }
        let key = sanitize_key(identifier);
        if key == MIRRORS_DIR_NAME {
            return Err(Error::WorktreeOutsideRoot(format!(
                "identifier {identifier:?} sanitizes to the reserved mirror dir {MIRRORS_DIR_NAME:?}"
            )));
        }
        let path = join(&[&self.root, &repo_key(repo_url), &key]);
        ensure_within_root(&self.root, &path).map_err(|e| {
            Error::WorktreeOutsideRoot(format!("unsafe worktree path {path:?}: {e}"))
        })?;

        // Serialize the whole removal under the per-repo mutex (symmetric with ensure_from_repo).
        let _guard = self.repo_lock(repo_url).lock_owned().await;

        // Lstat so a planted symlink is rejected BEFORE before_remove runs inside the target.
        let fi = match std::fs::symlink_metadata(&path) {
            Ok(fi) => fi,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()), // no-op, no hook
            Err(e) => return Err(Error::WorkspaceStat(e.to_string())),
        };
        if fi.file_type().is_symlink() {
            return Err(Error::WorkspaceSymlink(format!("{path:?} is a symlink")));
        }

        // Detect a standalone clone (its `.git` is a DIRECTORY) vs a shared-mirror worktree (whose
        // `.git` is a FILE, a `gitdir: …` admin link). `git worktree remove` does not apply to a
        // standalone clone, so remove it directly with the same before_remove semantics.
        let git_entry = join(&[&path, ".git"]);
        let is_standalone_clone =
            matches!(std::fs::symlink_metadata(&git_entry), Ok(gi) if gi.is_dir());
        if is_standalone_clone {
            if !self.hooks.before_remove.is_empty() {
                let _ = self
                    .runner
                    .run_env(
                        "before_remove",
                        &self.hooks.before_remove,
                        &path,
                        Some(&self.hook_env(repo_url, project_slug, identifier)),
                    )
                    .await;
            }
            remove_all(&path).map_err(|e| Error::WorktreeRemove(e.to_string()))?;
            return Ok(());
        }

        if !self.hooks.before_remove.is_empty() {
            let _ = self
                .runner
                .run_env(
                    "before_remove",
                    &self.hooks.before_remove,
                    &path,
                    Some(&self.hook_env(repo_url, project_slug, identifier)),
                )
                .await;
        }

        let mirror = self.mirror_dir(repo_url);
        let _ = clear_stale_locks(&mirror);
        let (_out, err) = self
            .git(&mirror, &["worktree", "remove", "--force", &path])
            .await;
        if err.is_some() {
            // rm -rf fallback so cleanup never wedges.
            let _ = remove_all(&path);
        }
        let _ = self.git(&mirror, &["worktree", "prune"]).await;
        if std::fs::metadata(&path).is_ok() {
            remove_all(&path).map_err(|e| Error::WorktreeRemove(e.to_string()))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HookScripts;
    use crate::testutil::{
        TempDir, add_origin_branch, add_origin_commit, git_run, init_local_origin,
        repo_test_manager, under_root,
    };
    use std::sync::Arc;
    use tokio::task::JoinSet;

    fn after(script: &str) -> HookScripts {
        HookScripts {
            after_create: script.to_string(),
            ..Default::default()
        }
    }
    fn before(script: &str) -> HookScripts {
        HookScripts {
            before_remove: script.to_string(),
            ..Default::default()
        }
    }

    // ---------------------------------------------------------------------------------------------
    // Task 1: error sentinels (TestErrorSentinelsDistinct) + gitEnv (TestGitEnv_*)
    // ---------------------------------------------------------------------------------------------

    // Mirror of TestErrorSentinelsDistinct: the six repo-backed sentinels are distinct categories.
    // Rust enum variants are distinct by construction, so we assert each matches only its own
    // variant, and that its Display leading token is the exact errors.go sentinel string.
    #[test]
    fn error_sentinels_distinct() {
        let git = Error::GitFailed("x".into());
        assert!(matches!(git, Error::GitFailed(_)));
        assert!(!matches!(git, Error::MirrorClone(_)));
        assert!(!matches!(git, Error::WorktreeOutsideRoot(_)));
    }

    // The plan's acceptance: every error Display's leading token equals the errors.go sentinel.
    #[test]
    fn error_display_matches_errors_go_sentinels() {
        let cases: &[(Error, &str)] = &[
            (
                Error::WorkspaceCreate("x".into()),
                "workspace_create_failed: ",
            ),
            (
                Error::WorkspaceRemove("x".into()),
                "workspace_remove_failed: ",
            ),
            (
                Error::WorkspaceNotDir("x".into()),
                "workspace_not_directory: ",
            ),
            (
                Error::WorkspaceSymlink("x".into()),
                "workspace_is_symlink: ",
            ),
            (
                Error::PathOutsideRoot("x".into()),
                "invalid_workspace_path: ",
            ),
            (Error::InvalidCwd("x".into()), "invalid_workspace_cwd: "),
            (Error::HookFailed("x".into()), "hook_failed: "),
            (Error::HookTimeout("x".into()), "hook_timeout: "),
            (Error::GitFailed("x".into()), "git_failed: "),
            (Error::MirrorClone("x".into()), "mirror_clone_failed: "),
            (Error::CloneFailed("x".into()), "clone_failed: "),
            (Error::WorktreeAdd("x".into()), "worktree_add_failed: "),
            (
                Error::WorktreeRemove("x".into()),
                "worktree_remove_failed: ",
            ),
            (
                Error::WorktreeOutsideRoot("x".into()),
                "worktree_outside_root: ",
            ),
            (Error::WorkspaceStat("x".into()), "workspace_stat_failed: "),
            (Error::GhFailed("x".into()), "gh_failed: "),
        ];
        for (err, want_prefix) in cases {
            assert!(
                err.to_string().starts_with(want_prefix),
                "{err} does not start with {want_prefix:?}"
            );
        }
    }

    fn last_env(env: &[(OsString, OsString)], key: &str) -> Option<String> {
        env.iter()
            .rev()
            .find(|(k, _)| k.as_os_str() == OsStr::new(key))
            .map(|(_, v)| v.to_string_lossy().into_owned())
    }
    fn count_env(env: &[(OsString, OsString)], key: &str) -> usize {
        env.iter()
            .filter(|(k, _)| k.as_os_str() == OsStr::new(key))
            .count()
    }

    // Mirror of TestGitEnv_PreservesOperatorSSHCommand. Exercises the pure hardening logic on a
    // synthetic base env (no global env mutation → parallel-safe, unlike Go's t.Setenv).
    #[test]
    fn git_env_preserves_operator_ssh_command() {
        let custom = "ssh -i /run/secrets/deploy_key -oBatchMode=yes";
        let base = vec![(OsString::from("GIT_SSH_COMMAND"), OsString::from(custom))];
        let env = git_env_from(base);
        assert_eq!(last_env(&env, "GIT_SSH_COMMAND").as_deref(), Some(custom));
        // Must not have appended a second (default) entry that would win at exec.
        assert_eq!(count_env(&env, "GIT_SSH_COMMAND"), 1);
    }

    // Mirror of TestGitEnv_AddsDefaultSSHCommandWhenUnset.
    #[test]
    fn git_env_adds_default_ssh_command_when_unset() {
        let env = git_env_from(Vec::new());
        assert_eq!(
            last_env(&env, "GIT_SSH_COMMAND").as_deref(),
            Some("ssh -oBatchMode=yes")
        );
        for k in [
            "GIT_TERMINAL_PROMPT",
            "GIT_ASKPASS",
            "SSH_ASKPASS",
            "GIT_CONFIG_NOSYSTEM",
        ] {
            assert!(last_env(&env, k).is_some(), "unconditional var {k} missing");
        }
    }

    // ---------------------------------------------------------------------------------------------
    // Task 2: per-repo lock registry (TestRepoLock_*)
    // ---------------------------------------------------------------------------------------------

    // Mirror of TestRepoLock_SameURLSamePointer.
    #[test]
    fn repo_lock_same_url_same_pointer() {
        let (m, _root) = repo_test_manager(HookScripts::default());
        let a = m.repo_lock("git@github.com:example/tally.git");
        let b = m.repo_lock("git@github.com:example/tally.git");
        assert!(Arc::ptr_eq(&a, &b), "same URL must map to the same mutex");
        let c = m.repo_lock("git@github.com:example/other.git");
        assert!(
            !Arc::ptr_eq(&a, &c),
            "distinct URLs must get distinct mutexes"
        );
    }

    // Mirror of TestRepoLock_ConcurrentRegistryIsRaceFree.
    #[test]
    fn repo_lock_concurrent_registry_is_race_free() {
        let (m, _root) = repo_test_manager(HookScripts::default());
        let m = Arc::new(m);
        let mut handles = Vec::new();
        for _ in 0..50 {
            let m = m.clone();
            handles.push(std::thread::spawn(move || {
                let _ = m.repo_lock("git@github.com:example/tally.git");
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
    }

    // ---------------------------------------------------------------------------------------------
    // Task 3: RepoKey / mirrorDir / git / clearStaleLocks
    // ---------------------------------------------------------------------------------------------

    // Mirror of TestRepoKey_StableAndSafe.
    #[test]
    fn repo_key_stable_and_safe() {
        let url = "git@github.com:example/tally.git";
        assert_eq!(repo_key(url), repo_key(url), "must be deterministic");
        assert_ne!(
            repo_key(url),
            repo_key("git@github.com:example/other.git"),
            "must differ for different URLs"
        );
        assert_eq!(
            repo_key(&format!(" {url} ")),
            repo_key(url),
            "must trim surrounding whitespace"
        );
        let k = repo_key(url);
        assert!(!k.is_empty());
        assert!(
            !k.contains(['/', '\\', ':', ' ', '@', '.']),
            "RepoKey {k:?} must be a safe single path component"
        );
    }

    // rhapsodyd matches the Go daemon's on-disk `<root>/<RepoKey>` layout (parity), so RepoKey MUST
    // be byte-identical to Go's `hex(crypto/sha256(trim(url)))[:24]`. Lock
    // the exact digest against a known SHA-256 (of "hello", trimmed) so any drift is caught without
    // needing the Go binary at test time.
    #[test]
    fn repo_key_matches_go_sha256() {
        assert_eq!(repo_key("hello"), "2cf24dba5fb0a30e26e83b2a");
        assert_eq!(repo_key("  hello  "), "2cf24dba5fb0a30e26e83b2a");
    }

    // Mirror of TestMirrorDir_UnderMirrorsRoot.
    #[test]
    fn mirror_dir_under_mirrors_root() {
        let (m, root) = repo_test_manager(HookScripts::default());
        let url = "git@github.com:example/tally.git";
        let bare = format!("{}.git", repo_key(url));
        let want = join(&[&root.path, ".mirrors", &bare]);
        assert_eq!(m.mirror_dir(url), want);
    }

    // Mirror of TestGit_RunsInDirAndReportsFailure.
    #[tokio::test]
    async fn git_runs_in_dir_and_reports_failure() {
        let origin = init_local_origin();
        let (m, _root) = repo_test_manager(HookScripts::default());
        let (out, err) = m
            .git(&origin.path, &["rev-parse", "--abbrev-ref", "HEAD"])
            .await;
        assert!(err.is_none(), "git rev-parse failed: {out}");
        assert_eq!(out.trim(), "main");
        let (_out, err) = m.git(&origin.path, &["no-such-subcommand-xyz"]).await;
        assert!(matches!(err, Some(Error::GitFailed(_))));
    }

    // Mirror of TestClearStaleLocks_RemovesTopLevelAndWorktreeAdmin.
    #[test]
    fn clear_stale_locks_removes_top_level_and_worktree_admin() {
        let git_dir = TempDir::new();
        std::fs::write(git_dir.child("index.lock"), "x").unwrap();
        let admin = join(&[&git_dir.path, "worktrees", "MT-1"]);
        std::fs::create_dir_all(&admin).unwrap();
        std::fs::write(join(&[&admin, "index.lock"]), "x").unwrap();
        std::fs::write(join(&[&admin, "HEAD.lock"]), "x").unwrap();
        // A non-lock file must be preserved.
        std::fs::write(join(&[&admin, "HEAD"]), "ref: refs/heads/symphony/MT-1\n").unwrap();

        clear_stale_locks(&git_dir.path).unwrap();
        assert!(std::fs::metadata(git_dir.child("index.lock")).is_err());
        assert!(std::fs::metadata(join(&[&admin, "index.lock"])).is_err());
        assert!(std::fs::metadata(join(&[&admin, "HEAD.lock"])).is_err());
        assert!(
            std::fs::metadata(join(&[&admin, "HEAD"])).is_ok(),
            "non-lock file must be preserved"
        );
    }

    // ---------------------------------------------------------------------------------------------
    // Task 4: ensureMirror / defaultBranch / PromptFileInRepo / gitPathAbsent
    // ---------------------------------------------------------------------------------------------

    // Mirror of TestEnsureMirror_ClonesThenReusesAndFetches.
    #[tokio::test]
    async fn ensure_mirror_clones_then_reuses_and_fetches() {
        let origin = init_local_origin();
        let (m, _root) = repo_test_manager(HookScripts::default());
        let mirror = m.ensure_mirror(&origin.path).await.unwrap();
        assert_eq!(mirror, m.mirror_dir(&origin.path));
        // Bare repo: HEAD file present.
        assert!(std::fs::metadata(join(&[&mirror, "HEAD"])).is_ok());
        // gc.auto == 0.
        let (gc, err) = m.git(&mirror, &["config", "--get", "gc.auto"]).await;
        assert!(err.is_none() && gc.trim() == "0", "gc.auto={gc:?}");
        // Reuse must pick up a new origin commit via fetch (assert on origin/main, not refs/heads).
        let (before, _) = m.git(&mirror, &["rev-parse", "origin/main"]).await;
        add_origin_commit(&origin.path);
        let mirror2 = m.ensure_mirror(&origin.path).await.unwrap();
        assert_eq!(mirror2, mirror, "reuse changed mirror dir");
        let (after, _) = m.git(&mirror, &["rev-parse", "origin/main"]).await;
        assert_ne!(
            before.trim(),
            after.trim(),
            "reuse did not fetch new commit"
        );
    }

    // Mirror of TestEnsureMirror_CloneFailureIsFatal.
    #[tokio::test]
    async fn ensure_mirror_clone_failure_is_fatal() {
        let (m, _root) = repo_test_manager(HookScripts::default());
        let bogus_root = TempDir::new();
        let bogus = bogus_root.child("not-a-repo");
        std::fs::create_dir_all(&bogus).unwrap();
        assert!(matches!(
            m.ensure_mirror(&bogus).await,
            Err(Error::MirrorClone(_))
        ));
    }

    // Mirror of TestDefaultBranch_Main.
    #[tokio::test]
    async fn default_branch_main() {
        let origin = init_local_origin();
        let (m, _root) = repo_test_manager(HookScripts::default());
        let mirror = m.ensure_mirror(&origin.path).await.unwrap();
        assert_eq!(m.default_branch(&mirror).await.unwrap(), "main");
    }

    // Mirror of TestPromptFileInRepo (INF-279).
    #[tokio::test]
    async fn prompt_file_in_repo() {
        let origin = init_local_origin();
        let (m, _root) = repo_test_manager(HookScripts::default());

        // Before any sync: cannot verify (no flag).
        assert_eq!(
            m.prompt_file_in_repo(&origin.path, ".symphony/PROMPT.md")
                .await,
            (false, false)
        );

        // First dispatch clones the mirror; the file is absent → checked, not present.
        m.ensure_from_repo(&origin.path, "", "MT-1").await.unwrap();
        assert_eq!(
            m.prompt_file_in_repo(&origin.path, ".symphony/PROMPT.md")
                .await,
            (false, true)
        );

        // Add the file to origin, then freshen the mirror via a reuse ensure (which fetches).
        std::fs::create_dir_all(join(&[&origin.path, ".symphony"])).unwrap();
        std::fs::write(
            join(&[&origin.path, ".symphony", "PROMPT.md"]),
            "repo prompt\n",
        )
        .unwrap();
        git_run(&origin.path, &["add", ".symphony/PROMPT.md"]);
        git_run(&origin.path, &["commit", "-m", "add repo prompt"]);
        m.ensure_from_repo(&origin.path, "", "MT-2").await.unwrap();
        assert_eq!(
            m.prompt_file_in_repo(&origin.path, ".symphony/PROMPT.md")
                .await,
            (true, true)
        );

        // A present-but-whitespace-only blob is reported NOT usable (matches the run's soft
        // fallback on an empty relative file).
        std::fs::write(join(&[&origin.path, ".symphony", "PROMPT.md"]), "   \n\t\n").unwrap();
        git_run(&origin.path, &["add", ".symphony/PROMPT.md"]);
        git_run(&origin.path, &["commit", "-m", "blank the repo prompt"]);
        m.ensure_from_repo(&origin.path, "", "MT-3").await.unwrap();
        assert_eq!(
            m.prompt_file_in_repo(&origin.path, ".symphony/PROMPT.md")
                .await,
            (false, true)
        );

        // Empty inputs cannot be checked.
        assert_eq!(
            m.prompt_file_in_repo("", ".symphony/PROMPT.md").await,
            (false, false)
        );
        assert_eq!(
            m.prompt_file_in_repo(&origin.path, "  ").await,
            (false, false)
        );
    }

    // Mirror of TestGitPathAbsent (INF-279).
    #[test]
    fn git_path_absent_classifies_output() {
        for s in [
            "fatal: path '.symphony/PROMPT.md' does not exist in 'refs/remotes/origin/main'",
            "fatal: Path 'X' does not exist in 'refs/remotes/origin/master'",
        ] {
            assert!(git_path_absent(s), "expected absent for {s:?}");
        }
        for s in [
            "error: git cat-file: signal: killed",
            "fatal: unable to read tree object",
            "context deadline exceeded",
            "fatal: Unable to create '.../index.lock': File exists.",
            "",
        ] {
            assert!(!git_path_absent(s), "expected non-absent for {s:?}");
        }
    }

    // ---------------------------------------------------------------------------------------------
    // Task 6: EnsureFromRepo / RemoveWorktree / hookEnv
    // ---------------------------------------------------------------------------------------------

    // Mirror of TestEnsureFromRepo_CreatesWorktreeOnBranchThenReusesNoReset.
    #[tokio::test]
    async fn ensure_from_repo_creates_worktree_on_branch_then_reuses_no_reset() {
        let origin = init_local_origin();
        let (m, root) = repo_test_manager(after("echo created >> .created"));

        let ws = m.ensure_from_repo(&origin.path, "", "MT-1").await.unwrap();
        assert!(ws.created_now);
        assert_eq!(ws.key, "MT-1");
        assert_eq!(
            ws.path,
            join(&[&root.path, &repo_key(&origin.path), "MT-1"])
        );
        // README from origin checked out.
        assert!(std::fs::metadata(join(&[&ws.path, "README.md"])).is_ok());
        // Branch is symphony/MT-1.
        let (br, err) = m
            .git(&ws.path, &["rev-parse", "--abbrev-ref", "HEAD"])
            .await;
        assert!(
            err.is_none() && br.trim() == "symphony/MT-1",
            "branch={br:?}"
        );
        // after_create ran exactly once.
        assert_eq!(
            std::fs::read_to_string(join(&[&ws.path, ".created"])).unwrap(),
            "created\n"
        );

        // Simulate in-progress work.
        std::fs::write(join(&[&ws.path, "README.md"]), "DIRTY WIP\n").unwrap();
        std::fs::write(join(&[&ws.path, "scratch.txt"]), "wip\n").unwrap();

        let ws2 = m.ensure_from_repo(&origin.path, "", "MT-1").await.unwrap();
        assert!(!ws2.created_now, "reuse must report created_now=false");
        assert_eq!(
            std::fs::read_to_string(join(&[&ws.path, "README.md"])).unwrap(),
            "DIRTY WIP\n",
            "reuse clobbered WIP"
        );
        assert!(std::fs::metadata(join(&[&ws.path, "scratch.txt"])).is_ok());
        assert_eq!(
            std::fs::read_to_string(join(&[&ws.path, ".created"])).unwrap(),
            "created\n",
            "after_create re-ran on reuse"
        );
    }

    // Mirror of TestEnsureFromRepo_SameIdentifierDifferentReposNoCollision.
    #[tokio::test]
    async fn ensure_from_repo_same_identifier_different_repos_no_collision() {
        let origin_a = init_local_origin();
        let origin_b = init_local_origin();
        std::fs::write(join(&[&origin_b.path, "B_ONLY.txt"]), "from repo B\n").unwrap();
        git_run(&origin_b.path, &["add", "B_ONLY.txt"]);
        git_run(&origin_b.path, &["commit", "-m", "b only"]);

        let (m, root) = repo_test_manager(HookScripts::default());
        let ws_a = m
            .ensure_from_repo(&origin_a.path, "", "MT-1")
            .await
            .unwrap();
        let ws_b = m
            .ensure_from_repo(&origin_b.path, "", "MT-1")
            .await
            .unwrap();

        assert_ne!(
            ws_a.path, ws_b.path,
            "same identifier across repos collided"
        );
        assert_eq!(
            ws_a.path,
            join(&[&root.path, &repo_key(&origin_a.path), "MT-1"])
        );
        assert_eq!(
            ws_b.path,
            join(&[&root.path, &repo_key(&origin_b.path), "MT-1"])
        );
        assert!(ws_a.created_now && ws_b.created_now);
        // Content proves no checkout reuse: B_ONLY.txt only in repo B's worktree.
        assert!(std::fs::metadata(join(&[&ws_b.path, "B_ONLY.txt"])).is_ok());
        assert!(std::fs::metadata(join(&[&ws_a.path, "B_ONLY.txt"])).is_err());
    }

    // Mirror of TestEnsureFromRepo_FetchDoesNotPruneSymphonyBranch (highest-stakes guard).
    #[tokio::test]
    async fn ensure_from_repo_fetch_does_not_prune_symphony_branch() {
        let origin = init_local_origin();
        let (m, _root) = repo_test_manager(HookScripts::default());

        let ws = m.ensure_from_repo(&origin.path, "", "MT-42").await.unwrap();
        let mirror = m.mirror_dir(&origin.path);
        let (_o, err) = m
            .git(
                &mirror,
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    "refs/heads/symphony/MT-42",
                ],
            )
            .await;
        assert!(err.is_none(), "symphony branch missing right after create");

        add_origin_commit(&origin.path);
        m.ensure_mirror(&origin.path).await.unwrap();
        let (_o, err) = m
            .git(
                &mirror,
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    "refs/heads/symphony/MT-42",
                ],
            )
            .await;
        assert!(err.is_none(), "pruning fetch deleted the symphony branch");
        let (br, err) = m
            .git(&ws.path, &["rev-parse", "--abbrev-ref", "HEAD"])
            .await;
        assert!(err.is_none() && br.trim() == "symphony/MT-42");
    }

    // Mirror of TestEnsureFromRepo_AfterCreateFailureRollsBackWorktree.
    #[tokio::test]
    async fn ensure_from_repo_after_create_failure_rolls_back_worktree() {
        let origin = init_local_origin();
        let (m, root) = repo_test_manager(after("exit 7"));
        let res = m.ensure_from_repo(&origin.path, "", "MT-2").await;
        assert!(
            matches!(res, Err(Error::HookFailed(_))),
            "want ErrHookFailed"
        );
        let wt = join(&[&root.path, &repo_key(&origin.path), "MT-2"]);
        assert!(
            matches!(std::fs::symlink_metadata(&wt), Err(e) if e.kind() == std::io::ErrorKind::NotFound),
            "failed after_create must remove the partial worktree dir"
        );
        // Worktree admin entry pruned (no dangling registration).
        let md = m.mirror_dir(&origin.path);
        let (out, _) = m.git(&md, &["worktree", "list", "--porcelain"]).await;
        assert!(!out.contains(&wt), "partial worktree not pruned:\n{out}");
    }

    // Mirror of TestEnsureFromRepo_SanitizedSeparatorStaysUnderRoot.
    #[tokio::test]
    async fn ensure_from_repo_sanitized_separator_stays_under_root() {
        let origin = init_local_origin();
        let (m, root) = repo_test_manager(HookScripts::default());
        let ws = m
            .ensure_from_repo(&origin.path, "", "a/b/MT-1")
            .await
            .unwrap();
        assert_eq!(ws.key, "a_b_MT-1");
        assert!(under_root(&ws.path, &root.path), "worktree escaped root");
        let (br, err) = m
            .git(&ws.path, &["rev-parse", "--abbrev-ref", "HEAD"])
            .await;
        assert!(
            err.is_none() && br.trim() == "symphony/a_b_MT-1",
            "branch={br:?}"
        );
    }

    // Mirror of TestRemoveWorktree_RemovesDirAndPrunesAdmin.
    #[tokio::test]
    async fn remove_worktree_removes_dir_and_prunes_admin() {
        let origin = init_local_origin();
        // before_remove writes a marker into the worktree's parent (outside the worktree).
        let (m, _root) = repo_test_manager(before("echo bye > ../.removed"));
        let ws = m.ensure_from_repo(&origin.path, "", "MT-9").await.unwrap();
        m.remove_worktree(&origin.path, "", "MT-9").await.unwrap();
        assert!(
            std::fs::metadata(&ws.path).is_err(),
            "must delete worktree dir"
        );
        // before_remove ran in the worktree dir, so "../.removed" lands in the per-repo dir.
        assert!(
            std::fs::metadata(join(&[&dir(&ws.path), ".removed"])).is_ok(),
            "before_remove did not run"
        );
        let md = m.mirror_dir(&origin.path);
        let (out, _) = m.git(&md, &["worktree", "list", "--porcelain"]).await;
        assert!(!out.contains(&ws.path), "worktree not pruned:\n{out}");
    }

    // Mirror of TestRemoveWorktree_MissingIsNoopAndSkipsHook.
    #[tokio::test]
    async fn remove_worktree_missing_is_noop_and_skips_hook() {
        let origin = init_local_origin();
        let (m, root) = repo_test_manager(before("echo nope > ../.ran"));
        m.ensure_mirror(&origin.path).await.unwrap();
        m.remove_worktree(&origin.path, "", "MT-404").await.unwrap();
        assert!(
            std::fs::metadata(join(&[&root.path, ".ran"])).is_err(),
            "before_remove must not run when the worktree does not exist"
        );
    }

    // Mirror of TestRemoveWorktree_RejectsSymlinkWorktree.
    #[tokio::test]
    async fn remove_worktree_rejects_symlink_worktree() {
        let origin = init_local_origin();
        let (m, root) = repo_test_manager(before("echo ran > .before_remove_ran"));

        let key = sanitize_key("MT-SYM");
        let repo_dir = join(&[&root.path, &repo_key(&origin.path)]);
        std::fs::create_dir_all(&repo_dir).unwrap();
        let outside = TempDir::new();
        let link = join(&[&repo_dir, &key]);
        std::os::unix::fs::symlink(&outside.path, &link).unwrap();

        let res = m.remove_worktree(&origin.path, "", "MT-SYM").await;
        assert!(matches!(res, Err(Error::WorkspaceSymlink(_))));
        // before_remove must NOT have run through the followed symlink.
        assert!(std::fs::metadata(join(&[&outside.path, ".before_remove_ran"])).is_err());
        assert_eq!(
            std::fs::read_dir(&outside.path).unwrap().count(),
            0,
            "symlink target was written into (followed)"
        );
    }

    // Mirror of TestEnsureFromRepo_AfterCreateSeesSymphonyEnv.
    #[tokio::test]
    async fn ensure_from_repo_after_create_sees_symphony_env() {
        let origin = init_local_origin();
        let script = r#"printf 'repo=%s project=%s issue=%s\n' "$SYMPHONY_REPO" "$SYMPHONY_PROJECT" "$SYMPHONY_ISSUE" > .env_seen"#;
        let (m, _root) = repo_test_manager(after(script));
        let ws = m
            .ensure_from_repo(&origin.path, "team-alpha", "MT-7")
            .await
            .unwrap();
        let seen = std::fs::read_to_string(join(&[&ws.path, ".env_seen"])).unwrap();
        let want = format!("repo={} project=team-alpha issue=MT-7\n", origin.path);
        assert_eq!(seen, want);
    }

    // Mirror of TestHookEnv_InjectsSymphonyVars.
    #[test]
    fn hook_env_injects_symphony_vars() {
        let (m, _root) = repo_test_manager(HookScripts::default());
        let env = m.hook_env("git@github.com:x/y.git", "proj-x", "MT-1");
        assert!(env.contains(&"SYMPHONY_REPO=git@github.com:x/y.git".to_string()));
        assert!(env.contains(&"SYMPHONY_ISSUE=MT-1".to_string()));
        assert!(env.contains(&"SYMPHONY_PROJECT=proj-x".to_string()));
    }

    // Mirror of TestEnsureFromRepo_ConcurrentDistinctIssuesSameRepo.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn ensure_from_repo_concurrent_distinct_issues_same_repo() {
        let origin = init_local_origin();
        let (m, _root) = repo_test_manager(HookScripts::default());
        let m = Arc::new(m);

        const N: usize = 12;
        let mut set = JoinSet::new();
        for i in 0..N {
            let m = m.clone();
            let op = origin.path.clone();
            set.spawn(async move {
                let id = format!("MT-{i}");
                let ws = m.ensure_from_repo(&op, "", &id).await?;
                if !ws.created_now {
                    return Err(Error::GitFailed(format!("{id}: expected created_now")));
                }
                Ok::<(), Error>(())
            });
        }
        while let Some(joined) = set.join_next().await {
            joined.expect("task panicked").expect("ensure failed");
        }

        // Exactly N worktrees registered; count branch lines (path substrings are unreliable on
        // macOS due to /var vs /private/var).
        let md = m.mirror_dir(&origin.path);
        let (out, err) = m.git(&md, &["worktree", "list", "--porcelain"]).await;
        assert!(err.is_none());
        let got = out.matches("branch refs/heads/symphony/MT-").count();
        assert_eq!(got, N, "registered worktrees:\n{out}");
    }

    // Mirror of TestEnsureFromRepo_HookRunsUnlocked: two concurrent same-repo after_create hooks
    // must OVERLAP (the mirror lock is released before the hook).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn ensure_from_repo_hook_runs_unlocked() {
        let origin = init_local_origin();
        // Perl Time::HiRes (present on macOS + Linux) rather than $EPOCHREALTIME (bash >= 5 only).
        // The hook stamps a start time, sleeps, then stamps an end time; the two concurrent hooks'
        // [start,end] intervals must OVERLAP to prove the mirror lock is NOT held across the hook.
        // The sleep is the jitter tolerance: it must exceed the scheduling skew between the two
        // spawned tasks' hook starts, or a concurrent-but-skewed pair reads as non-overlapping under
        // load (a false failure on busy CI). 1s comfortably exceeds real task-startup skew while a
        // SERIALIZED pair (start2 == end1) still clearly fails the overlap check. (TRA-243)
        let hook = r#"now() { perl -MTime::HiRes=time -e 'printf "%.6f\n", time'; }; now > .hook_times; sleep 1; now >> .hook_times"#;
        let (m, _root) = repo_test_manager(after(hook));
        let m = Arc::new(m);

        let mut set = JoinSet::new();
        for i in 0..2 {
            let m = m.clone();
            let op = origin.path.clone();
            set.spawn(async move {
                m.ensure_from_repo(&op, "", &format!("MT-OVL-{i}"))
                    .await
                    .map(|_| ())
            });
        }
        while let Some(joined) = set.join_next().await {
            joined.expect("task panicked").expect("ensure failed");
        }

        let read_interval = |id: &str| -> (f64, f64) {
            let p = join(&[
                &m.root,
                &repo_key(&origin.path),
                &sanitize_key(id),
                ".hook_times",
            ]);
            let body = std::fs::read_to_string(&p).unwrap();
            let nums: Vec<f64> = body
                .split_whitespace()
                .map(|x| x.parse().unwrap())
                .collect();
            assert_eq!(nums.len(), 2, "hook times malformed: {body:?}");
            (nums[0], nums[1])
        };
        let (s0, e0) = read_interval("MT-OVL-0");
        let (s1, e1) = read_interval("MT-OVL-1");
        // Two intervals overlap iff each starts before the other ends.
        assert!(
            s0 < e1 && s1 < e0,
            "after_create hooks did not overlap (mirror lock held across hook?): \
             [{s0:.6},{e0:.6}] [{s1:.6},{e1:.6}]"
        );
    }

    // Mirror of TestEnsureFromRepo_ClearsStaleLockBeforeMutating.
    #[tokio::test]
    async fn ensure_from_repo_clears_stale_lock_before_mutating() {
        let origin = init_local_origin();
        let (m, _root) = repo_test_manager(HookScripts::default());
        let mirror = m.ensure_mirror(&origin.path).await.unwrap();
        std::fs::write(join(&[&mirror, "index.lock"]), "stale").unwrap();

        let ws = m
            .ensure_from_repo(&origin.path, "", "MT-LOCK")
            .await
            .unwrap();
        assert!(ws.created_now);
        assert!(
            std::fs::metadata(join(&[&mirror, "index.lock"])).is_err(),
            "stale index.lock should have been cleared"
        );
    }

    // Mirror of TestEnsureFromRepo_PrunesStaleWorktreeAdminBeforeAdd.
    #[tokio::test]
    async fn ensure_from_repo_prunes_stale_worktree_admin_before_add() {
        let origin = init_local_origin();
        let (m, _root) = repo_test_manager(HookScripts::default());
        let ws = m
            .ensure_from_repo(&origin.path, "", "MT-CRASH")
            .await
            .unwrap();
        let mirror = m.mirror_dir(&origin.path);

        // Simulate the crash: remove only the worktree dir, skip the prune.
        std::fs::remove_dir_all(&ws.path).unwrap();
        let (out, _) = m.git(&mirror, &["worktree", "list", "--porcelain"]).await;
        assert!(
            out.contains("worktree "),
            "precondition: dangling admin entry:\n{out}"
        );

        // Re-create: the pre-add prune must clear the stale entry so the add succeeds.
        let ws2 = m
            .ensure_from_repo(&origin.path, "", "MT-CRASH")
            .await
            .unwrap();
        assert!(ws2.created_now);
        assert!(std::fs::metadata(&ws2.path).is_ok());
    }

    // ---------------------------------------------------------------------------------------------
    // Task 8: back-compat (empty URL) + reserved key + reuse symlink + PathFor
    // ---------------------------------------------------------------------------------------------

    // Mirror of TestEnsureFromRepo_EmptyURLDelegatesToCreateForIssue.
    #[tokio::test]
    async fn ensure_from_repo_empty_url_delegates_to_create_for_issue() {
        let (m, root) = repo_test_manager(after("echo x > created.txt"));
        let ws = m.ensure_from_repo("", "", "MT-1").await.unwrap();
        assert!(ws.created_now);
        assert_eq!(ws.path, join(&[&root.path, "MT-1"]));
        assert!(std::fs::metadata(join(&[&ws.path, "created.txt"])).is_ok());
        assert!(
            std::fs::metadata(join(&[&root.path, ".mirrors"])).is_err(),
            "empty-URL path must NOT create a mirror"
        );
    }

    // Mirror of TestRemoveWorktree_EmptyURLDelegatesToRemove.
    #[tokio::test]
    async fn remove_worktree_empty_url_delegates_to_remove() {
        let (m, root) = repo_test_manager(HookScripts::default());
        m.ensure_from_repo("", "", "MT-2").await.unwrap();
        m.remove_worktree("", "", "MT-2").await.unwrap();
        assert!(std::fs::metadata(join(&[&root.path, "MT-2"])).is_err());
    }

    // Mirror of TestRepoWorktree_ReservedMirrorsKeyRejected.
    #[tokio::test]
    async fn repo_worktree_reserved_mirrors_key_rejected() {
        let (m, root) = repo_test_manager(HookScripts::default());
        assert_eq!(sanitize_key(".mirrors"), ".mirrors");
        let url = "git@github.com:example/tally.git";
        assert!(matches!(
            m.ensure_from_repo(url, "", ".mirrors").await,
            Err(Error::WorktreeOutsideRoot(_))
        ));
        assert!(matches!(
            m.remove_worktree(url, "", ".mirrors").await,
            Err(Error::WorktreeOutsideRoot(_))
        ));
        // The guard fires before ensureMirror, so no mirror store was created.
        assert!(std::fs::metadata(join(&[&root.path, ".mirrors"])).is_err());
    }

    // Mirror of TestEnsureFromRepo_RejectsSymlinkWorktreeOnReuse.
    #[tokio::test]
    async fn ensure_from_repo_rejects_symlink_worktree_on_reuse() {
        let origin = init_local_origin();
        let (m, root) = repo_test_manager(HookScripts::default());

        let key = sanitize_key("MT-SYM");
        let repo_dir = join(&[&root.path, &repo_key(&origin.path)]);
        std::fs::create_dir_all(&repo_dir).unwrap();
        let outside = TempDir::new();
        let link = join(&[&repo_dir, &key]);
        std::os::unix::fs::symlink(&outside.path, &link).unwrap();

        let res = m.ensure_from_repo(&origin.path, "", "MT-SYM").await;
        assert!(matches!(res, Err(Error::WorkspaceSymlink(_))));
        assert_eq!(
            std::fs::read_dir(&outside.path).unwrap().count(),
            0,
            "symlink target was written into (followed)"
        );
    }

    // Mirror of TestPathFor_RepoNamespacedAndLegacy.
    #[test]
    fn path_for_repo_namespaced_and_legacy() {
        let (m, root) = repo_test_manager(HookScripts::default());
        let repo = "git@github.com:example/tally.git";
        assert_eq!(
            m.path_for(repo, "MT-7"),
            join(&[&root.path, &repo_key(repo), "MT-7"])
        );
        assert_eq!(m.path_for("", "MT-7"), join(&[&root.path, "MT-7"]));
        // Identifier sanitization applies under both schemes.
        assert_eq!(
            m.path_for(repo, "team/MT 9"),
            join(&[&root.path, &repo_key(repo), "team_MT_9"])
        );
        assert_eq!(
            m.path_for("", "team/MT 9"),
            join(&[&root.path, "team_MT_9"])
        );
    }

    // ---------------------------------------------------------------------------------------------
    // repo_clone_test.go: EnsureCloneFromRepo + standalone-clone removal
    // ---------------------------------------------------------------------------------------------

    // Mirror of TestEnsureCloneFromRepo_CreatesStandaloneCloneThenReusesNoReset.
    #[tokio::test]
    async fn ensure_clone_from_repo_creates_standalone_clone_then_reuses_no_reset() {
        let origin = init_local_origin();
        let (m, root) = repo_test_manager(after("echo created >> .created"));

        let ws = m
            .ensure_clone_from_repo(&origin.path, "", "CL-1")
            .await
            .unwrap();
        assert!(ws.created_now);
        assert_eq!(ws.key, "CL-1");
        assert_eq!(
            ws.path,
            join(&[&root.path, &repo_key(&origin.path), "CL-1"])
        );
        // (a) Standalone clone: <path>/.git is a real DIRECTORY.
        let gi = std::fs::metadata(join(&[&ws.path, ".git"])).unwrap();
        assert!(gi.is_dir(), "clone .git must be a directory");
        // No shared bare mirror for clone mode.
        assert!(
            std::fs::metadata(m.mirror_dir(&origin.path)).is_err(),
            "clone mode must NOT create a shared bare mirror"
        );
        // (b) origin points at the source repo.
        let (out, err) = m.git(&ws.path, &["remote", "get-url", "origin"]).await;
        assert!(err.is_none() && out.trim() == origin.path, "origin={out:?}");
        // (c) Branch symphony/CL-1 + README checked out.
        let (br, err) = m
            .git(&ws.path, &["rev-parse", "--abbrev-ref", "HEAD"])
            .await;
        assert!(
            err.is_none() && br.trim() == "symphony/CL-1",
            "branch={br:?}"
        );
        assert!(std::fs::metadata(join(&[&ws.path, "README.md"])).is_ok());
        assert_eq!(
            std::fs::read_to_string(join(&[&ws.path, ".created"])).unwrap(),
            "created\n"
        );

        // Reuse: WIP survives, after_create does NOT re-run.
        std::fs::write(join(&[&ws.path, "README.md"]), "DIRTY WIP\n").unwrap();
        std::fs::write(join(&[&ws.path, "scratch.txt"]), "wip\n").unwrap();
        let ws2 = m
            .ensure_clone_from_repo(&origin.path, "", "CL-1")
            .await
            .unwrap();
        assert!(!ws2.created_now);
        assert_eq!(
            std::fs::read_to_string(join(&[&ws.path, "README.md"])).unwrap(),
            "DIRTY WIP\n"
        );
        assert!(std::fs::metadata(join(&[&ws.path, "scratch.txt"])).is_ok());
        assert_eq!(
            std::fs::read_to_string(join(&[&ws.path, ".created"])).unwrap(),
            "created\n"
        );
    }

    // Mirror of TestEnsureCloneFromRepo_SameStackBranchesNoCheckoutLock.
    #[tokio::test]
    async fn ensure_clone_from_repo_same_stack_branches_no_checkout_lock() {
        let origin = init_local_origin();
        add_origin_branch(&origin.path, "feat-a");
        let (m, _root) = repo_test_manager(HookScripts::default());
        let ws1 = m
            .ensure_clone_from_repo(&origin.path, "", "CL-1")
            .await
            .unwrap();
        let ws2 = m
            .ensure_clone_from_repo(&origin.path, "", "CL-2")
            .await
            .unwrap();
        assert_ne!(ws1.path, ws2.path, "two clone workspaces collided");
        // Both clones check out the SAME stack branch with no `already used by worktree` error.
        let (_o, err) = m.git(&ws1.path, &["checkout", "feat-a"]).await;
        assert!(err.is_none(), "CL-1 checkout feat-a failed");
        let (_o, err) = m.git(&ws2.path, &["checkout", "feat-a"]).await;
        assert!(
            err.is_none(),
            "CL-2 checkout feat-a (no checkout lock in clone mode)"
        );
    }

    // Mirror of TestEnsureCloneFromRepo_ReusesExistingWorktreePreservingWIP (INF-418).
    #[tokio::test]
    async fn ensure_clone_from_repo_reuses_existing_worktree_preserving_wip() {
        let origin = init_local_origin();
        let (m, _root) = repo_test_manager(HookScripts::default());

        let wt = m
            .ensure_from_repo(&origin.path, "", "FLIP-1")
            .await
            .unwrap();
        // A worktree's .git is a FILE (gitdir link), not a directory.
        let gi = std::fs::symlink_metadata(join(&[&wt.path, ".git"])).unwrap();
        assert!(!gi.is_dir(), "expected a worktree (.git is a file)");
        std::fs::write(join(&[&wt.path, "scratch.txt"]), "in-progress work\n").unwrap();

        let ws = m
            .ensure_clone_from_repo(&origin.path, "", "FLIP-1")
            .await
            .unwrap();
        assert!(
            !ws.created_now,
            "reuse of an existing checkout must report created_now=false"
        );
        assert_eq!(ws.path, wt.path);
        assert!(
            std::fs::metadata(join(&[&wt.path, "scratch.txt"])).is_ok(),
            "in-progress work must be preserved across a mode flip"
        );
    }

    // Mirror of TestRemoveWorktree_RemovesStandaloneClone.
    #[tokio::test]
    async fn remove_worktree_removes_standalone_clone() {
        let origin = init_local_origin();
        let logdir = TempDir::new();
        let logfile = logdir.child("removed.log");
        let (m, root) = repo_test_manager(before(&format!("echo bye >> {logfile}")));

        let ws = m
            .ensure_clone_from_repo(&origin.path, "", "CL-9")
            .await
            .unwrap();
        assert!(
            std::fs::metadata(&ws.path).is_ok(),
            "clone dir missing before remove"
        );
        m.remove_worktree(&origin.path, "", "CL-9").await.unwrap();
        assert!(
            std::fs::metadata(&ws.path).is_err(),
            "clone dir should be gone"
        );
        // Path scheme sanity: it lived where PathFor reports, under root.
        assert_eq!(ws.path, m.path_for(&origin.path, "CL-9"));
        assert!(ws.path.starts_with(&root.path));
    }
}
