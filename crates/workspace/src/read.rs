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
        self.patch_id_stdin(&mirror, &out).await
    }

    /// Feeds `diff` to `git patch-id --stable` on stdin and returns the first id. `git patch-id`
    /// has no file-input mode, so this is the one read that needs a pipe; it reuses the crate's
    /// hardened git environment. Runs on the blocking pool, since the std child API is synchronous.
    async fn patch_id_stdin(&self, mirror: &str, diff: &str) -> Result<String, ReadError> {
        let mirror = mirror.to_string();
        let diff = diff.to_string();
        tokio::task::spawn_blocking(move || patch_id_blocking(&mirror, &diff))
            .await
            .map_err(|e| ReadError::Git(format!("patch-id task: {e}")))?
    }
}

/// The synchronous half of [`Manager::patch_id_stdin`] — `spawn_blocking`'s closure, factored out so
/// it is testable without a runtime.
fn patch_id_blocking(mirror: &str, diff: &str) -> Result<String, ReadError> {
    use std::io::Write;
    let mut child = std::process::Command::new("git")
        .arg("-c")
        .arg("gc.auto=0")
        .arg("-C")
        .arg(mirror)
        .args(["patch-id", "--stable"])
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
        let base = String::from_utf8_lossy(
            &std::process::Command::new("git")
                .args(["-C", &origin.path, "rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .trim()
        .to_string();
        // A first patch (v1) and a rebased/amended second patch (v2) that changes the file again.
        std::fs::write(origin.child("b.rs"), "v1\n").unwrap();
        git_run(&origin.path, &["add", "b.rs"]);
        git_run(&origin.path, &["commit", "-m", "v1"]);
        let from = mirror_for(&m, &origin.path).await;
        std::fs::write(origin.child("b.rs"), "v2\n").unwrap();
        git_run(&origin.path, &["add", "b.rs"]);
        git_run(&origin.path, &["commit", "-m", "v2"]);
        let to = String::from_utf8_lossy(
            &std::process::Command::new("git")
                .args(["-C", &origin.path, "rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .trim()
        .to_string();
        m.ensure_from_repo(&origin.path, "", "AIE-2")
            .await
            .expect("refresh mirror");
        let rd = m
            .range_diff(&origin.path, &base, &from, &to)
            .await
            .expect("range-diff");
        assert!(
            rd.contains("v2"),
            "range-diff should name the added patch: {rd}"
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
}
