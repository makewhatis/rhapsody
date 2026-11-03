//! read — the manager's host-served repository reads: git objects from the bare mirror (STUDIO-1014;
//! design record `~/.rhapsody/docs/manager-agent-design.md` §4.4).
//!
//! **No Go v0.4.0 counterpart.** The manager run has no checkout, no `gh` and no `git`; the host
//! serves every repository read to it. These methods are that read half, driven through the SAME
//! hardened `git` subprocess the rest of the crate uses, against the per-repo bare mirror — never a
//! worktree.
//!
//! # The two boundaries that are security properties, not conveniences
//!
//! * **A symlink is returned as its blob text and is NEVER followed** (§4.4). Reading
//!   `git cat-file -p <blob>` on a mode-`120000` tree entry returns the link TARGET STRING, exactly
//!   what the design requires — the host never resolves the path on disk, so a symlink to a
//!   credential file yields the link, not the secret. [`BlobRead::symlink`] carries the mode fact.
//! * **Every call is bounded** (§4.4). A blob larger than [`MAX_READ_BYTES`] is refused before it is
//!   read; a grep or diff result is cut at the bound and flagged as [`GrepRead::truncated`] /
//!   returned with the caller able to see the cut.
//!
//! The revision argument is validated with the crate's own [`crate::repo`]`::is_commit_sha` guard, so
//! a value interpolated into the git argument list can never name a branch, a flag or a revision
//! expression rather than one commit.

use std::process::Stdio;

use crate::Manager;
use crate::repo::{git_env, is_commit_sha};

/// The largest blob [`Manager::read_blob`] will serve. A caller gets [`ReadError::TooLarge`] rather
/// than an unbounded read.
pub const MAX_READ_BYTES: usize = 512 * 1024;

/// The largest total output [`Manager::grep`] will return before cutting it.
pub const MAX_GREP_BYTES: usize = 512 * 1024;

/// The largest diff [`Manager::diff`] will serve (diff and interdiff are meant to be read whole; a
/// larger one is refused rather than silently cut).
pub const MAX_DIFF_BYTES: usize = 4 * 1024 * 1024;

/// The most tree entries [`Manager::ls_tree`] will return.
pub const MAX_TREE_ENTRIES: usize = 2000;

/// A failed host-served read, typed so the daemon can map it to a stable error code.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ReadError {
    /// The revision is not a plausible commit sha.
    #[error("invalid_revision")]
    InvalidRevision,
    /// No such object at that revision/path.
    #[error("not_found")]
    NotFound,
    /// The path names a directory; `ls` is the right read.
    #[error("is_a_directory")]
    IsDirectory,
    /// The read would exceed the bound.
    #[error("too_large")]
    TooLarge,
    /// Git itself failed (a transient mirror error, a corrupt object, a missing mirror).
    #[error("git_failed: {0}")]
    Git(String),
}

/// One blob as served to a manager run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobRead {
    /// The blob's bytes as text (git object content). For a symlink this is the link TARGET text.
    pub content: String,
    /// The tree entry's mode was `120000` — the content is a link target and MUST NOT be followed.
    pub symlink: bool,
}

impl BlobRead {
    /// A one-line, model-facing rendering. A symlink is labelled as such so the manager can never be
    /// misled into treating a link target as file content. The target text is shown ONCE — the
    /// label carries it — so the rendering never implies the link has content of its own.
    pub fn render(&self) -> String {
        if self.symlink {
            format!("[symlink -> {}]", self.content.trim_end())
        } else {
            self.content.clone()
        }
    }
}

/// One `ls-tree` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    /// The git mode, e.g. `100644`, `100755`, `120000`, `040000`.
    pub mode: String,
    /// The object type: `blob`, `tree` or `commit`.
    pub kind: String,
    /// The object id.
    pub sha: String,
    /// The full repository-relative path.
    pub path: String,
}

/// A tree listing, possibly cut at [`MAX_TREE_ENTRIES`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeRead {
    pub entries: Vec<TreeEntry>,
    /// The listing was cut at the bound. Like [`GrepRead::truncated`], the manager must be told so a
    /// cut listing is never read as a complete one.
    pub truncated: bool,
}

/// A grep result, possibly cut at [`MAX_GREP_BYTES`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrepRead {
    pub text: String,
    /// The result was cut at the bound; the manager is told so rather than reading a partial answer
    /// as complete.
    pub truncated: bool,
}

/// Whether `s` is a plausible git ref NAME (a branch) safe to interpolate into a git argument list
/// under a `refs/remotes/origin/` prefix: no leading dash, no `..`/`@{` revision syntax, and only
/// characters git branch names use. Conservative on purpose — a false negative merely refuses the
/// mirror path and falls back to the compare API.
fn is_safe_ref_name(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('-')
        && !s.contains("..")
        && !s.contains("@{")
        && !s.ends_with('/')
        && !s.ends_with(".lock")
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/'))
}

/// Parses one `git ls-tree` line: `<mode> <type> <sha>\t<path>`.
fn parse_ls_tree_line(line: &str) -> Option<TreeEntry> {
    let (meta, path) = line.split_once('\t')?;
    let mut it = meta.split_whitespace();
    let mode = it.next()?.to_string();
    let kind = it.next()?.to_string();
    let sha = it.next()?.to_string();
    Some(TreeEntry {
        mode,
        kind,
        sha,
        path: path.to_string(),
    })
}

impl Manager {
    /// Reads one blob at `sha:path` from the bare mirror. A symlink is returned as its blob text and
    /// never followed.
    pub async fn read_blob(
        &self,
        repo_url: &str,
        sha: &str,
        path: &str,
    ) -> Result<BlobRead, ReadError> {
        if !is_commit_sha(sha) {
            return Err(ReadError::InvalidRevision);
        }
        if path.is_empty() {
            // The root is a tree, never a blob.
            return Err(ReadError::IsDirectory);
        }
        let mirror = self.mirror_dir(repo_url);
        let (out, err) = self.git(&mirror, &["ls-tree", sha, "--", path]).await;
        if err.is_some() {
            if out.trim().is_empty() || out.to_lowercase().contains("does not exist") {
                return Err(ReadError::NotFound);
            }
            return Err(ReadError::Git(out));
        }
        let line = out.lines().find(|l| !l.trim().is_empty());
        let Some(entry) = line.and_then(parse_ls_tree_line) else {
            return Err(ReadError::NotFound);
        };
        match entry.kind.as_str() {
            "tree" => return Err(ReadError::IsDirectory),
            "blob" => {}
            // A gitlink (submodule) names a commit, not file content.
            _ => return Err(ReadError::NotFound),
        }
        let symlink = entry.mode == "120000";
        let (size_out, size_err) = self.git(&mirror, &["cat-file", "-s", &entry.sha]).await;
        if size_err.is_some() {
            return Err(ReadError::Git(size_out));
        }
        let size: usize = size_out
            .trim()
            .parse()
            .map_err(|_| ReadError::Git("unreadable object size".to_string()))?;
        if size > MAX_READ_BYTES {
            return Err(ReadError::TooLarge);
        }
        let (content, cerr) = self.git(&mirror, &["cat-file", "-p", &entry.sha]).await;
        if cerr.is_some() {
            return Err(ReadError::Git(content));
        }
        Ok(BlobRead { content, symlink })
    }

    /// Lists the tree at `sha:path` (or the root when `path` is empty) from the bare mirror. A
    /// listing over [`MAX_TREE_ENTRIES`] is cut and flagged, so a truncated listing can never be
    /// read as a complete one.
    pub async fn ls_tree(
        &self,
        repo_url: &str,
        sha: &str,
        path: &str,
    ) -> Result<TreeRead, ReadError> {
        if !is_commit_sha(sha) {
            return Err(ReadError::InvalidRevision);
        }
        let mirror = self.mirror_dir(repo_url);
        let mut args: Vec<&str> = vec!["ls-tree", sha];
        if !path.is_empty() {
            args.push("--");
            args.push(path);
        }
        let (out, err) = self.git(&mirror, &args).await;
        if err.is_some() {
            if out.to_lowercase().contains("does not exist") {
                return Err(ReadError::NotFound);
            }
            return Err(ReadError::Git(out));
        }
        let mut entries: Vec<TreeEntry> = out.lines().filter_map(parse_ls_tree_line).collect();
        let truncated = entries.len() > MAX_TREE_ENTRIES;
        if truncated {
            entries.truncate(MAX_TREE_ENTRIES);
        }
        Ok(TreeRead { entries, truncated })
    }

    /// Searches the tree at `sha` for `pattern` (fixed regex, `git grep -e`), optionally restricted
    /// to `path`. A no-match is an empty result, not an error; a result over [`MAX_GREP_BYTES`] is
    /// cut and flagged.
    pub async fn grep(
        &self,
        repo_url: &str,
        sha: &str,
        pattern: &str,
        path: &str,
    ) -> Result<GrepRead, ReadError> {
        if !is_commit_sha(sha) {
            return Err(ReadError::InvalidRevision);
        }
        if pattern.is_empty() {
            return Err(ReadError::Git("empty pattern".to_string()));
        }
        let mirror = self.mirror_dir(repo_url);
        let mut args: Vec<&str> = vec!["grep", "-n", "-I", "--no-color", "-e", pattern, sha];
        if !path.is_empty() {
            args.push("--");
            args.push(path);
        }
        let (out, err) = self.git(&mirror, &args).await;
        if err.is_some() && !out.trim().is_empty() {
            // Output with a failure is a real error (e.g. a malformed pattern); exit 1 with no
            // output is git grep's "no matches".
            return Err(ReadError::Git(out));
        }
        let truncated = out.len() > MAX_GREP_BYTES;
        let text = if truncated {
            let mut cut = out;
            cut.truncate(MAX_GREP_BYTES);
            cut
        } else {
            out
        };
        Ok(GrepRead { text, truncated })
    }

    /// The commit sha the bare mirror's default remote branch points at — the pull request's usual
    /// base. `origin/HEAD` when the mirror records it, else `origin/main`, else `origin/master`. The
    /// revs are daemon-chosen constants (never caller input), so none can name a revision
    /// expression.
    pub async fn default_base_sha(&self, repo_url: &str) -> Result<String, ReadError> {
        let mirror = self.mirror_dir(repo_url);
        for rev in [
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
            "refs/remotes/origin/master",
        ] {
            let expr = format!("{rev}^{{commit}}");
            let (out, err) = self.git(&mirror, &["rev-parse", "--verify", &expr]).await;
            if err.is_none() {
                let s = out.trim();
                if is_commit_sha(s) {
                    return Ok(s.to_string());
                }
            }
        }
        Err(ReadError::Git(
            "the mirror has no default branch to use as a base".to_string(),
        ))
    }

    /// The merge base of two commits, from the bare mirror.
    pub async fn merge_base(&self, repo_url: &str, a: &str, b: &str) -> Result<String, ReadError> {
        if !is_commit_sha(a) || !is_commit_sha(b) {
            return Err(ReadError::InvalidRevision);
        }
        let mirror = self.mirror_dir(repo_url);
        let (out, err) = self.git(&mirror, &["merge-base", a, b]).await;
        if err.is_some() {
            return Err(ReadError::Git(out));
        }
        Ok(out.trim().to_string())
    }

    /// The diff `from..to` from the bare mirror, bounded by [`MAX_DIFF_BYTES`].
    pub async fn diff(&self, repo_url: &str, from: &str, to: &str) -> Result<String, ReadError> {
        if !is_commit_sha(from) || !is_commit_sha(to) {
            return Err(ReadError::InvalidRevision);
        }
        let mirror = self.mirror_dir(repo_url);
        let (out, err) = self.git(&mirror, &["diff", "--no-color", from, to]).await;
        if err.is_some() {
            return Err(ReadError::Git(out));
        }
        if out.len() > MAX_DIFF_BYTES {
            return Err(ReadError::TooLarge);
        }
        Ok(out)
    }

    /// The difference between the two pull-request patches `merge-base(base, from)..from` and
    /// `merge-base(base, to)..to` — the comparison `git range-diff` makes (§5.5). Used after a
    /// rebase or force-push, where `from` is not an ancestor of `to`. Bounded by [`MAX_DIFF_BYTES`].
    pub async fn range_diff(
        &self,
        repo_url: &str,
        base: &str,
        from: &str,
        to: &str,
    ) -> Result<String, ReadError> {
        if !is_commit_sha(base) || !is_commit_sha(from) || !is_commit_sha(to) {
            return Err(ReadError::InvalidRevision);
        }
        let mirror = self.mirror_dir(repo_url);
        let mb_from = self.merge_base(repo_url, base, from).await?;
        let mb_to = self.merge_base(repo_url, base, to).await?;
        if mb_from.is_empty() || mb_to.is_empty() {
            return Err(ReadError::Git(
                "no merge base with the pull request's base".to_string(),
            ));
        }
        let range_from = format!("{mb_from}..{from}");
        let range_to = format!("{mb_to}..{to}");
        let (out, err) = self
            .git(
                &mirror,
                &["range-diff", "--no-color", "-p", &range_from, &range_to],
            )
            .await;
        if err.is_some() {
            return Err(ReadError::Git(out));
        }
        if out.len() > MAX_DIFF_BYTES {
            return Err(ReadError::TooLarge);
        }
        Ok(out)
    }

    /// A stable patch-id for `sha`, computed over `base..sha` (`git patch-id --stable`). Content, not
    /// commit identity.
    pub async fn patch_id(
        &self,
        repo_url: &str,
        base: &str,
        sha: &str,
    ) -> Result<String, ReadError> {
        if !is_commit_sha(base) || !is_commit_sha(sha) {
            return Err(ReadError::InvalidRevision);
        }
        let mirror = self.mirror_dir(repo_url);
        let (out, err) = self.git(&mirror, &["diff", "--no-color", base, sha]).await;
        if err.is_some() {
            return Err(ReadError::Git(out));
        }
        if out.len() > MAX_DIFF_BYTES {
            return Err(ReadError::TooLarge);
        }
        self.patch_id_stdin(&mirror, &out, false).await
    }

    /// The tree object id of `sha` from the bare mirror (`git rev-parse <sha>^{tree}`). An unknown
    /// `sha` is an error, never an empty string, so a caller can tell "not in the mirror" from a real
    /// tree. Used to prove a head move carried no work by tree EQUALITY alone (STUDIO-1672): it is
    /// cheap, and it covers a re-stack onto the same tree exactly.
    pub async fn tree_id(&self, repo_url: &str, sha: &str) -> Result<String, ReadError> {
        if !is_commit_sha(sha) {
            return Err(ReadError::InvalidRevision);
        }
        let mirror = self.mirror_dir(repo_url);
        let expr = format!("{sha}^{{tree}}");
        let (out, err) = self
            .git(&mirror, &["rev-parse", "--verify", "--quiet", &expr])
            .await;
        if err.is_some() {
            return Err(ReadError::Git(out));
        }
        let tree = out.trim();
        if tree.is_empty() {
            return Err(ReadError::NotFound);
        }
        Ok(tree.to_string())
    }

    /// A content patch-id for the change `sha` carries against `base` — the THREE-dot change
    /// `merge-base(base, sha)..sha` (`git patch-id --verbatim`), not the two-dot [`Self::patch_id`].
    /// Content, not commit identity, and blind to the hunk line numbers and blob hashes a
    /// base-preserving merge rewrites, so it is the mirror-side proof that a head move carried no
    /// work (STUDIO-1672). `base` is a branch name (GitHub's `pulls.base.ref`) or a commit SHA;
    /// [`Self::resolve_base`] turns it into a commit. Errors when there is no merge base, when the
    /// diff is unreadable, or when the change is empty.
    ///
    /// **`--verbatim`, deliberately, not `--stable`.** `--stable` strips ALL whitespace before
    /// hashing, including whitespace inside a line, so a head that differs from the reviewed one only
    /// in whitespace (re-indentation, a string literal) would hash to the same id and be reported
    /// unchanged — carrying an approval over a real change (STUDIO-1672 review B1). `--verbatim`
    /// keeps whitespace and is still independent of file order. The two-dot [`Self::patch_id`], used
    /// by the manager's own reads, keeps `--stable`; only this three-dot no-op-rebase proof is
    /// verbatim.
    pub async fn merge_base_patch_id(
        &self,
        repo_url: &str,
        base: &str,
        sha: &str,
    ) -> Result<String, ReadError> {
        if !is_commit_sha(sha) {
            return Err(ReadError::InvalidRevision);
        }
        let base_sha = self.resolve_base(repo_url, base).await?;
        let mirror = self.mirror_dir(repo_url);
        let mb = self.merge_base(repo_url, &base_sha, sha).await?;
        if mb.is_empty() {
            return Err(ReadError::Git(
                "no merge base with the pull request's base".to_string(),
            ));
        }
        let (out, err) = self.git(&mirror, &["diff", "--no-color", &mb, sha]).await;
        if err.is_some() {
            return Err(ReadError::Git(out));
        }
        if out.len() > MAX_DIFF_BYTES {
            return Err(ReadError::TooLarge);
        }
        self.patch_id_stdin(&mirror, &out, true).await
    }

    /// Resolves a pull request's base REF (GitHub's `pulls.base.ref`, a branch name) to a commit in
    /// the bare mirror. A commit SHA is accepted as-is; anything else is resolved under
    /// `refs/remotes/origin/`, so a value interpolated into the git argument list can never name a
    /// flag or an arbitrary revision expression. `main`/`master` are ordinary branch names here —
    /// the mirror's remote-tracking refs are what the review worktree fetch keeps current.
    pub async fn resolve_base(&self, repo_url: &str, base: &str) -> Result<String, ReadError> {
        let base = base.trim();
        if is_commit_sha(base) {
            return Ok(base.to_string());
        }
        if !is_safe_ref_name(base) {
            return Err(ReadError::InvalidRevision);
        }
        let mirror = self.mirror_dir(repo_url);
        let refname = format!("refs/remotes/origin/{base}^{{commit}}");
        let (out, err) = self
            .git(&mirror, &["rev-parse", "--verify", "--quiet", &refname])
            .await;
        if err.is_some() {
            return Err(ReadError::Git(out));
        }
        let sha = out.trim();
        if !is_commit_sha(sha) {
            return Err(ReadError::NotFound);
        }
        Ok(sha.to_string())
    }

    /// Feeds `diff` to `git patch-id` on stdin and returns the first id. `verbatim` selects
    /// `--verbatim` (keep whitespace, for the no-op-rebase proof) over `--stable` (strip it, for the
    /// manager's own [`Self::patch_id`] reads). `git patch-id` has no file-input mode, so this is the
    /// one read that needs a pipe; it reuses the crate's hardened git environment. Runs on the
    /// blocking pool, since the std child API is synchronous.
    async fn patch_id_stdin(
        &self,
        mirror: &str,
        diff: &str,
        verbatim: bool,
    ) -> Result<String, ReadError> {
        let mirror = mirror.to_string();
        let diff = diff.to_string();
        tokio::task::spawn_blocking(move || patch_id_blocking(&mirror, &diff, verbatim))
            .await
            .map_err(|e| ReadError::Git(format!("patch-id task: {e}")))?
    }
}

/// The synchronous half of [`Manager::patch_id_stdin`] — `spawn_blocking`'s closure, factored out so
/// it is testable without a runtime. `verbatim` picks `--verbatim` over `--stable`; see
/// [`Manager::patch_id_stdin`].
fn patch_id_blocking(mirror: &str, diff: &str, verbatim: bool) -> Result<String, ReadError> {
    use std::io::Write;
    let algorithm = if verbatim { "--verbatim" } else { "--stable" };
    let mut child = std::process::Command::new("git")
        .arg("-c")
        .arg("gc.auto=0")
        .arg("-C")
        .arg(mirror)
        .args(["patch-id", algorithm])
        .env_clear()
        .envs(git_env())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| ReadError::Git(format!("spawn patch-id: {e}")))?;
    if let Some(mut si) = child.stdin.take() {
        si.write_all(diff.as_bytes())
            .map_err(|e| ReadError::Git(format!("write patch-id: {e}")))?;
        // Drop closes the pipe, signalling EOF to `git patch-id`.
    }
    let out = child
        .wait_with_output()
        .map_err(|e| ReadError::Git(format!("patch-id: {e}")))?;
    if !out.status.success() {
        return Err(ReadError::Git(
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let id = text.split_whitespace().next().unwrap_or("").to_string();
    if id.is_empty() {
        return Err(ReadError::Git("patch-id produced nothing".to_string()));
    }
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HookScripts;
    use crate::testutil::{git_run, init_local_origin, repo_test_manager};

    /// Creates the mirror for `origin` by provisioning one worktree, and returns the origin's HEAD
    /// sha. The mirror is then the read source.
    async fn mirror_for(m: &Manager, origin: &str) -> String {
        m.ensure_from_repo(origin, "", "AIE-1")
            .await
            .expect("provision worktree/mirror");
        let out = std::process::Command::new("git")
            .args(["-C", origin, "rev-parse", "HEAD"])
            .output()
            .expect("rev-parse");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[tokio::test]
    async fn read_blob_returns_file_content() {
        let (m, _root) = repo_test_manager(HookScripts::default());
        let origin = init_local_origin();
        std::fs::write(origin.child("src.rs"), "fn main() {}\n").unwrap();
        git_run(&origin.path, &["add", "src.rs"]);
        git_run(&origin.path, &["commit", "-m", "add src"]);
        let sha = mirror_for(&m, &origin.path).await;

        let got = m
            .read_blob(&origin.path, &sha, "src.rs")
            .await
            .expect("read");
        assert_eq!(got.content, "fn main() {}\n");
        assert!(!got.symlink);
    }

    // §4.4 / §15.4: a symlink to a credential file returns the LINK TEXT, never the target; the
    // reader must never follow it.
    #[tokio::test]
    async fn read_blob_symlink_returns_link_text_not_target() {
        let (m, _root) = repo_test_manager(HookScripts::default());
        let origin = init_local_origin();
        std::fs::write(origin.child("credential"), "SUPER_SECRET\n").unwrap();
        // A symlink committed into the tree pointing at the credential.
        std::os::unix::fs::symlink("credential", origin.child("link")).unwrap();
        git_run(&origin.path, &["add", "credential", "link"]);
        git_run(&origin.path, &["commit", "-m", "add symlink"]);
        let sha = mirror_for(&m, &origin.path).await;

        let got = m
            .read_blob(&origin.path, &sha, "link")
            .await
            .expect("read symlink");
        assert!(got.symlink, "the tree mode must be reported as a symlink");
        assert_eq!(
            got.content, "credential",
            "a symlink must yield its target TEXT, never the target's content"
        );
        assert!(
            !got.content.contains("SUPER_SECRET"),
            "the credential content must never be served through a symlink"
        );
    }

    #[tokio::test]
    async fn read_blob_rejects_bad_revision_and_missing_path() {
        let (m, _root) = repo_test_manager(HookScripts::default());
        let origin = init_local_origin();
        let sha = mirror_for(&m, &origin.path).await;
        assert_eq!(
            m.read_blob(&origin.path, "not-a-sha", "README.md").await,
            Err(ReadError::InvalidRevision)
        );
        assert_eq!(
            m.read_blob(&origin.path, &sha, "nope/missing.rs").await,
            Err(ReadError::NotFound)
        );
        // A directory is not a blob read.
        assert_eq!(
            m.read_blob(&origin.path, &sha, "").await,
            Err(ReadError::IsDirectory)
        );
    }

    #[tokio::test]
    async fn ls_tree_lists_entries_not_truncated() {
        let (m, _root) = repo_test_manager(HookScripts::default());
        let origin = init_local_origin();
        std::fs::write(origin.child("a.rs"), "a\n").unwrap();
        git_run(&origin.path, &["add", "a.rs"]);
        git_run(&origin.path, &["commit", "-m", "a"]);
        let sha = mirror_for(&m, &origin.path).await;
        let got = m.ls_tree(&origin.path, &sha, "").await.expect("ls");
        let names: Vec<&str> = got.entries.iter().map(|e| e.path.as_str()).collect();
        assert!(names.contains(&"README.md"), "{names:?}");
        assert!(names.contains(&"a.rs"), "{names:?}");
        assert!(!got.truncated, "a small listing must not be flagged cut");
    }

    // review follow-up: a listing cut at MAX_TREE_ENTRIES must SAY so — a silently cut listing reads
    // as complete.
    #[tokio::test]
    async fn ls_tree_flags_a_truncated_listing() {
        let (m, _root) = repo_test_manager(HookScripts::default());
        let origin = init_local_origin();
        // One more entry than the bound, so the cut is observable.
        for i in 0..=(MAX_TREE_ENTRIES) {
            std::fs::write(origin.child(&format!("f{i:04}.rs")), "x\n").unwrap();
        }
        git_run(&origin.path, &["add", "-A"]);
        git_run(&origin.path, &["commit", "-m", "many files"]);
        let sha = mirror_for(&m, &origin.path).await;
        let got = m.ls_tree(&origin.path, &sha, "").await.expect("ls");
        assert_eq!(got.entries.len(), MAX_TREE_ENTRIES);
        assert!(got.truncated, "an over-bound listing must be flagged cut");
    }

    // review follow-up: the symlink rendering names the target ONCE, never as both a label and a
    // body.
    #[test]
    fn symlink_render_shows_the_target_once() {
        let b = BlobRead {
            content: "target/path".to_string(),
            symlink: true,
        };
        assert_eq!(b.render(), "[symlink -> target/path]");
        let plain = BlobRead {
            content: "hello\n".to_string(),
            symlink: false,
        };
        assert_eq!(plain.render(), "hello\n");
    }

    #[tokio::test]
    async fn range_diff_shows_the_change_between_two_patches() {
        let (m, _root) = repo_test_manager(HookScripts::default());
        let origin = init_local_origin();
        let rev = |rev: &str| {
            String::from_utf8_lossy(
                &std::process::Command::new("git")
                    .args(["-C", &origin.path, "rev-parse", rev])
                    .output()
                    .unwrap()
                    .stdout,
            )
            .trim()
            .to_string()
        };
        let lines = |seven: &str| {
            (1..=20)
                .map(|i| {
                    if i == 7 {
                        seven.to_string()
                    } else {
                        format!("line{i}")
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
                + "\n"
        };
        // `b.rs` exists in the BASE, so the two patches MODIFY it rather than create it; that is
        // what makes git pair the two commits and emit the interdiff body.
        std::fs::write(origin.child("b.rs"), lines("x")).unwrap();
        git_run(&origin.path, &["add", "b.rs"]);
        git_run(&origin.path, &["commit", "-m", "base b.rs"]);
        let base = rev("HEAD");
        for (branch, body, msg) in [
            ("old", lines("v1"), "change to v1"),
            ("new", lines("v2"), "change to v2"),
        ] {
            git_run(&origin.path, &["checkout", "-b", branch, &base]);
            std::fs::write(origin.child("b.rs"), body).unwrap();
            git_run(&origin.path, &["add", "b.rs"]);
            git_run(&origin.path, &["commit", "-m", msg]);
        }
        git_run(&origin.path, &["checkout", "main"]);
        m.ensure_from_repo(&origin.path, "", "AIE-2")
            .await
            .expect("provision mirror");
        let from = rev("old");
        let to = rev("new");
        let rd = m
            .range_diff(&origin.path, &base, &from, &to)
            .await
            .expect("range-diff");
        assert!(
            rd.contains("v1"),
            "range-diff should name the old patch: {rd}"
        );
        assert!(
            rd.contains("v2"),
            "range-diff should name the new patch: {rd}"
        );
        // The commit SUBJECT alone would satisfy `contains("v2")`; the point of a range-diff is the
        // inner patch BODY. These lines (`-+v1` / `++v2`) appear only when the two patches' bodies
        // are actually compared.
        assert!(
            rd.contains("-+v1") && rd.contains("++v2"),
            "range-diff must compare the patch body, not just name the commits: {rd}"
        );
        assert!(!rd.trim().is_empty(), "range-diff must not be empty");
    }

    #[tokio::test]
    async fn grep_finds_and_misses() {
        let (m, _root) = repo_test_manager(HookScripts::default());
        let origin = init_local_origin();
        std::fs::write(origin.child("a.rs"), "needle here\n").unwrap();
        git_run(&origin.path, &["add", "a.rs"]);
        git_run(&origin.path, &["commit", "-m", "a"]);
        let sha = mirror_for(&m, &origin.path).await;
        let hit = m
            .grep(&origin.path, &sha, "needle", "")
            .await
            .expect("grep");
        assert!(hit.text.contains("a.rs"), "{}", hit.text);
        let miss = m
            .grep(&origin.path, &sha, "zzz-not-present", "")
            .await
            .expect("grep miss");
        assert!(
            miss.text.is_empty(),
            "no match must be empty: {}",
            miss.text
        );
    }

    #[tokio::test]
    async fn diff_merge_base_and_patch_id() {
        let (m, _root) = repo_test_manager(HookScripts::default());
        let origin = init_local_origin();
        let (base_out, _) = (
            std::process::Command::new("git")
                .args(["-C", &origin.path, "rev-parse", "HEAD"])
                .output()
                .unwrap(),
            (),
        );
        let base = String::from_utf8_lossy(&base_out.stdout).trim().to_string();
        std::fs::write(origin.child("b.rs"), "b\n").unwrap();
        git_run(&origin.path, &["add", "b.rs"]);
        git_run(&origin.path, &["commit", "-m", "b"]);
        let head = mirror_for(&m, &origin.path).await;

        let d = m.diff(&origin.path, &base, &head).await.expect("diff");
        assert!(d.contains("b.rs"), "{d}");
        assert_eq!(
            m.merge_base(&origin.path, &head, &base).await.expect("mb"),
            base
        );

        let id1 = m
            .patch_id(&origin.path, &base, &head)
            .await
            .expect("patch-id");
        // Same content, different commits: a rebase onto an equivalent tree keeps the patch-id.
        assert!(!id1.is_empty());
        let id2 = m
            .patch_id(&origin.path, &base, &head)
            .await
            .expect("patch-id");
        assert_eq!(id1, id2, "a stable patch-id is deterministic");
    }

    /// STUDIO-1672: the mirror-side no-op-rebase proof. A "large" file (the lockfile shape the
    /// compare API cannot render) is in the diff, and the three outcomes the watcher must tell
    /// apart are: the SAME tree at a new SHA (tree equality), the SAME change at a new parent (a
    /// rebase — patch-id equality), and a changed line in that large file (patch-id differs).
    #[tokio::test]
    async fn tree_id_and_merge_base_patch_id_prove_a_no_op_rebase() {
        let (m, _root) = repo_test_manager(HookScripts::default());
        let origin = init_local_origin();
        let rev = |r: &str| {
            String::from_utf8_lossy(
                &std::process::Command::new("git")
                    .args(["-C", &origin.path, "rev-parse", r])
                    .output()
                    .expect("rev-parse")
                    .stdout,
            )
            .trim()
            .to_string()
        };
        let big = |marker: &str| {
            (1..=2000)
                .map(|i| {
                    if i == 1000 {
                        format!("line{i}-{marker}")
                    } else {
                        format!("line{i}")
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
                + "\n"
        };
        // The base carries the large file; then main advances by an UNRELATED file, so a rebase
        // onto the new main keeps the large file's own change identical.
        std::fs::write(origin.child("package-lock.json"), big("v1")).unwrap();
        git_run(&origin.path, &["add", "package-lock.json"]);
        git_run(&origin.path, &["commit", "-m", "lock v1"]);
        let base = rev("HEAD");
        std::fs::write(origin.child("unrelated.txt"), "x\n").unwrap();
        git_run(&origin.path, &["add", "unrelated.txt"]);
        git_run(&origin.path, &["commit", "-m", "main advance"]);
        let base2 = rev("HEAD");

        // `reviewed`: the large file edited, off the old base.
        git_run(&origin.path, &["checkout", "-b", "reviewed", &base]);
        std::fs::write(origin.child("package-lock.json"), big("v2")).unwrap();
        git_run(&origin.path, &["add", "package-lock.json"]);
        git_run(&origin.path, &["commit", "-m", "reviewed change"]);
        let reviewed = rev("reviewed");

        // `restacked`: the SAME change, same parent, different commit message — identical tree.
        git_run(&origin.path, &["checkout", "-b", "restacked", &base]);
        std::fs::write(origin.child("package-lock.json"), big("v2")).unwrap();
        git_run(&origin.path, &["add", "package-lock.json"]);
        git_run(&origin.path, &["commit", "-m", "restacked change"]);
        let restacked = rev("restacked");

        // `rebased`: the SAME change, off the advanced main — a different tree, same patch-id.
        git_run(&origin.path, &["checkout", "-b", "rebased", &base2]);
        std::fs::write(origin.child("package-lock.json"), big("v2")).unwrap();
        git_run(&origin.path, &["add", "package-lock.json"]);
        git_run(&origin.path, &["commit", "-m", "rebased change"]);
        let rebased = rev("rebased");

        // `changed`: only the large file's content differs.
        git_run(&origin.path, &["checkout", "-b", "changed", &base2]);
        std::fs::write(origin.child("package-lock.json"), big("v3")).unwrap();
        git_run(&origin.path, &["add", "package-lock.json"]);
        git_run(&origin.path, &["commit", "-m", "changed change"]);
        let changed = rev("changed");
        git_run(&origin.path, &["checkout", "main"]);

        m.sync_mirror(&origin.path).await.expect("sync mirror");

        assert_eq!(
            m.tree_id(&origin.path, &reviewed).await.expect("tree"),
            m.tree_id(&origin.path, &restacked).await.expect("tree"),
            "a re-stack onto an identical tree must be caught by tree equality"
        );
        assert_ne!(
            m.tree_id(&origin.path, &reviewed).await.expect("tree"),
            m.tree_id(&origin.path, &rebased).await.expect("tree"),
            "the rebased head carries main's unrelated file, so its tree differs"
        );
        assert_eq!(
            m.merge_base_patch_id(&origin.path, &base2, &reviewed)
                .await
                .expect("patch-id"),
            m.merge_base_patch_id(&origin.path, &base2, &rebased)
                .await
                .expect("patch-id"),
            "the same change at a new parent must keep its patch-id"
        );
        assert_ne!(
            m.merge_base_patch_id(&origin.path, &base2, &reviewed)
                .await
                .expect("patch-id"),
            m.merge_base_patch_id(&origin.path, &base2, &changed)
                .await
                .expect("patch-id"),
            "a changed line in the large file must change the patch-id"
        );
        // The base arrives from GitHub as a branch NAME, not a SHA; `resolve_base` maps it onto the
        // mirror's remote-tracking ref and the patch-id is the same either way.
        assert_eq!(
            m.resolve_base(&origin.path, "main").await.expect("resolve"),
            base2
        );
        assert_eq!(
            m.merge_base_patch_id(&origin.path, "main", &reviewed)
                .await
                .expect("branch-name base"),
            m.merge_base_patch_id(&origin.path, &base2, &reviewed)
                .await
                .expect("sha base"),
        );
        // A base that is not a plausible ref name is refused rather than interpolated.
        assert_eq!(
            m.resolve_base(&origin.path, "--upload-pack=evil").await,
            Err(ReadError::InvalidRevision)
        );
    }

    /// STUDIO-1672 review B1: the mirror-side no-op-rebase proof must keep whitespace. `git patch-id
    /// --stable` strips whitespace before hashing, so two changes differing only in an in-line space
    /// collapse to the same id and an approval would carry over a real edit. `merge_base_patch_id`
    /// uses `--verbatim`, which keeps it; the manager's two-dot `patch_id` keeps `--stable`.
    #[tokio::test]
    async fn merge_base_patch_id_keeps_whitespace_that_stable_strips() {
        let (m, _root) = repo_test_manager(HookScripts::default());
        let origin = init_local_origin();
        let rev = |r: &str| {
            String::from_utf8_lossy(
                &std::process::Command::new("git")
                    .args(["-C", &origin.path, "rev-parse", r])
                    .output()
                    .expect("rev-parse")
                    .stdout,
            )
            .trim()
            .to_string()
        };
        let big = |marker: &str| {
            (1..=2000)
                .map(|i| {
                    if i == 1000 {
                        format!("line{i}-{marker}")
                    } else {
                        format!("line{i}")
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
                + "\n"
        };
        std::fs::write(origin.child("package-lock.json"), big("v1")).unwrap();
        git_run(&origin.path, &["add", "package-lock.json"]);
        git_run(&origin.path, &["commit", "-m", "lock v1"]);
        let base = rev("HEAD");

        // Two heads off the same base whose only difference is one in-line space.
        git_run(&origin.path, &["checkout", "-b", "reviewed", &base]);
        std::fs::write(origin.child("package-lock.json"), big("v2")).unwrap();
        git_run(&origin.path, &["add", "package-lock.json"]);
        git_run(&origin.path, &["commit", "-m", "reviewed"]);
        let reviewed = rev("reviewed");
        git_run(&origin.path, &["checkout", "-b", "spaced", &base]);
        std::fs::write(origin.child("package-lock.json"), big("v 2")).unwrap();
        git_run(&origin.path, &["add", "package-lock.json"]);
        git_run(&origin.path, &["commit", "-m", "spaced"]);
        let spaced = rev("spaced");
        git_run(&origin.path, &["checkout", "main"]);

        m.sync_mirror(&origin.path).await.expect("sync mirror");

        // The content genuinely differs, so a round is owed.
        assert_ne!(
            m.tree_id(&origin.path, &reviewed).await.expect("tree"),
            m.tree_id(&origin.path, &spaced).await.expect("tree"),
        );
        // The proof the watcher uses keeps the whitespace, so it sees new work.
        assert_ne!(
            m.merge_base_patch_id(&origin.path, &base, &reviewed)
                .await
                .expect("patch-id"),
            m.merge_base_patch_id(&origin.path, &base, &spaced)
                .await
                .expect("patch-id"),
            "a whitespace-only edit must change the verbatim patch-id"
        );
        // The manager's own stable patch-id is the whitespace-BLIND one; this pins the divergence so
        // nobody "unifies" the two and re-opens the regression.
        assert_eq!(
            m.patch_id(&origin.path, &base, &reviewed)
                .await
                .expect("stable patch-id"),
            m.patch_id(&origin.path, &base, &spaced)
                .await
                .expect("stable patch-id"),
            "`--stable` is documented to strip whitespace; only the no-op-rebase proof is verbatim"
        );
    }
}
