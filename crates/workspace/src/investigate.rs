//! Cached PR-head archive exports for the lead's Docker sandbox (STUDIO-1135).
//! No Go counterpart. No repository code runs on the host.
use crate::repo::is_commit_sha;
use crate::safety::ensure_within_root;
use crate::{Error, Manager, Workspace};

impl Manager {
    pub async fn ensure_investigate_export(
        &self,
        repo_url: &str,
        run_id: i64,
        sha: &str,
    ) -> Result<Workspace, Error> {
        if repo_url.is_empty() || run_id <= 0 || !is_commit_sha(sha) {
            return Err(Error::ReviewCheckout(
                "invalid investigation coordinates".into(),
            ));
        }
        let key = format!("investigate-{run_id}");
        let path = self.path_for(repo_url, &key);
        ensure_within_root(&self.root, &path)?;
        let _lock = self.repo_lock(repo_url).lock_owned().await;
        if std::fs::symlink_metadata(&path).is_ok() {
            return Err(Error::WorkspaceCreate(
                "investigation path already exists".into(),
            ));
        }
        let mirror = self.mirror_dir(repo_url);
        let info = std::fs::symlink_metadata(&mirror)
            .map_err(|_| Error::ReviewCheckout("cached mirror unavailable".into()))?;
        if info.file_type().is_symlink() || !info.is_dir() {
            return Err(Error::WorkspaceSymlink(
                "mirror is not a real directory".into(),
            ));
        }
        let parent = std::path::Path::new(&path)
            .parent()
            .ok_or_else(|| Error::PathOutsideRoot("investigation parent missing".into()))?;
        std::fs::create_dir_all(parent).map_err(|e| Error::WorkspaceCreate(e.to_string()))?;
        if std::fs::symlink_metadata(parent)
            .map_err(|e| Error::WorkspaceStat(e.to_string()))?
            .file_type()
            .is_symlink()
        {
            return Err(Error::WorkspaceSymlink(
                "investigation parent is a symlink".into(),
            ));
        }
        std::fs::create_dir(&path).map_err(|e| Error::WorkspaceCreate(e.to_string()))?;
        if let Err(error) = self.investigate_archive(&mirror, sha, &path).await {
            // Only this attempt owns the new directory. No mirror worktree admin state was created.
            if std::fs::remove_dir_all(&path).is_err() {
                return Err(Error::WorkspaceRemove(format!(
                    "partial investigation rollback failed after {error}"
                )));
            }
            return Err(error);
        }
        Ok(Workspace {
            path: format!("{path}/repo"),
            key,
            created_now: true,
        })
    }

    pub async fn remove_investigate_export(
        &self,
        repo_url: &str,
        run_id: i64,
    ) -> Result<(), Error> {
        let _lock = self.repo_lock(repo_url).lock_owned().await;
        let path = self.path_for(repo_url, &format!("investigate-{run_id}"));
        ensure_within_root(&self.root, &path)?;
        let info = match std::fs::symlink_metadata(&path) {
            Ok(info) => info,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(Error::WorkspaceStat(e.to_string())),
        };
        if info.file_type().is_symlink() {
            return Err(Error::WorkspaceSymlink(
                "investigation export is a symlink".into(),
            ));
        }
        if !info.is_dir() {
            return Err(Error::WorkspaceNotDir(
                "investigation export is not a directory".into(),
            ));
        }
        std::fs::remove_dir_all(path).map_err(|e| Error::WorkspaceRemove(e.to_string()))
    }

    // Git archive can apply content filters too. Use only the mirror's object database, never
    // its config/info/refs: this empty Git directory has no filter definitions or hooks.
    // Read attributes from a separate empty worktree, never the PR tree or tar's destination:
    // export-ignore/export-subst and checkout conversions must not hide or rewrite evidence.
    // Never use a custom archive format, a shell pipeline or lifecycle hooks.
    async fn investigate_archive(&self, mirror: &str, sha: &str, path: &str) -> Result<(), Error> {
        use std::process::Stdio;
        let git_dir = std::path::Path::new(path).join("git");
        let attributes = git_dir.join("attributes");
        let repo = std::path::Path::new(path).join("repo");
        std::fs::create_dir(&git_dir)
            .and_then(|_| std::fs::create_dir(git_dir.join("objects")))
            .and_then(|_| std::fs::create_dir(git_dir.join("refs")))
            .and_then(|_| std::fs::create_dir(&attributes))
            .and_then(|_| std::fs::write(git_dir.join("HEAD"), b"ref: refs/heads/export\n"))
            .and_then(|_| std::fs::create_dir(&repo))
            .map_err(|e| Error::WorkspaceCreate(e.to_string()))?;
        let mut cmd = tokio::process::Command::new("git");
        cmd.env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", "/nonexistent")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_ATTR_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env(
                "GIT_OBJECT_DIRECTORY",
                std::path::Path::new(mirror).join("objects"),
            )
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "core.fsmonitor=false",
                "-c",
                "core.attributesFile=/dev/null",
                "--git-dir",
            ])
            .arg(&git_dir)
            .arg("--work-tree")
            .arg(&attributes)
            .args(["archive", "--format=tar", "--worktree-attributes", sha])
            .kill_on_drop(true)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut archive = cmd
            .spawn()
            .map_err(|_| Error::ReviewCheckout("investigation archive could not start".into()))?;
        let pipe = archive
            .stdout
            .take()
            .ok_or_else(|| Error::ReviewCheckout("investigation archive pipe missing".into()))?;
        let input: Stdio = pipe
            .try_into()
            .map_err(|_| Error::ReviewCheckout("investigation archive pipe failed".into()))?;
        // Git trees cannot contain traversal paths or descendants beneath a symlink. Tar's
        // default secure extraction also refuses writes through symlinks; do not use -P/-U.
        let mut unpack = tokio::process::Command::new("tar");
        unpack
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", "/nonexistent")
            .args([
                "--extract",
                "--file",
                "-",
                "--directory",
                repo.to_str()
                    .ok_or_else(|| Error::PathOutsideRoot("export path is not UTF-8".into()))?,
                "--no-same-owner",
                "--no-same-permissions",
            ])
            .stdin(input)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut unpack = unpack
            .spawn()
            .map_err(|_| Error::ReviewCheckout("investigation unpack could not start".into()))?;
        let (archived, unpacked) =
            tokio::time::timeout(std::time::Duration::from_secs(60), async {
                tokio::try_join!(archive.wait(), unpack.wait())
            })
            .await
            .map_err(|_| Error::ReviewCheckout("investigation export timed out".into()))?
            .map_err(|_| Error::ReviewCheckout("investigation export failed".into()))?;
        if archived.success() && unpacked.success() {
            Ok(())
        } else {
            Err(Error::ReviewCheckout(
                "cached investigation export failed".into(),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use std::path::Path;

    fn git(path: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", "/nonexistent")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .args([
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.invalid",
                "-C",
            ])
            .arg(path)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().into()
    }

    #[tokio::test]
    async fn pr_export_runs_no_host_filters_or_hooks() {
        use std::os::unix::fs::PermissionsExt;
        let root = TempDir::new();
        let root_path = Path::new(&root.path);
        let origin = root_path.join("origin");
        std::fs::create_dir(&origin).unwrap();
        git(&origin, &["init", "-b", "main"]);
        std::fs::write(origin.join(".gitattributes"), "file filter=canary\n").unwrap();
        std::fs::write(origin.join("file"), "PR head content\n").unwrap();
        git(&origin, &["add", "."]);
        git(&origin, &["commit", "-m", "untrusted attributes"]);
        let sha = git(&origin, &["rev-parse", "HEAD"]);
        let mirror = root_path
            .join(".mirrors")
            .join(format!("{}.git", crate::repo_key(origin.to_str().unwrap())));
        std::fs::create_dir_all(mirror.parent().unwrap()).unwrap();
        git(
            root_path,
            &[
                "clone",
                "--bare",
                origin.to_str().unwrap(),
                mirror.to_str().unwrap(),
            ],
        );
        let filter_marker = root_path.join("host-filter-marker");
        let hook_marker = root_path.join("host-hook-marker");
        let filter = format!("touch '{}' ; cat", filter_marker.display());
        git(&mirror, &["config", "filter.canary.smudge", &filter]);
        git(&mirror, &["config", "filter.canary.clean", &filter]);
        git(&mirror, &["config", "filter.canary.required", "true"]);
        std::fs::write(
            mirror.join("hooks/post-checkout"),
            format!("#!/bin/sh\ntouch '{}'\n", hook_marker.display()),
        )
        .unwrap();
        std::fs::set_permissions(
            mirror.join("hooks/post-checkout"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let manager = Manager::new(crate::Config {
            root: root.path.clone(),
            hooks: crate::HookScripts {
                after_create: format!("touch '{}'", hook_marker.display()),
                before_remove: format!("touch '{}'", hook_marker.display()),
                ..Default::default()
            },
            hook_timeout: std::time::Duration::from_secs(1),
        })
        .unwrap();
        assert!(!filter_marker.exists(), "fixture setup ran the filter");
        let tree = manager
            .ensure_investigate_export(origin.to_str().unwrap(), 42, &sha)
            .await
            .unwrap();
        assert!(
            !filter_marker.exists(),
            "PR provisioning executed a host Git content filter"
        );
        assert!(
            !hook_marker.exists(),
            "PR provisioning executed a host hook"
        );
        assert_eq!(
            std::fs::read_to_string(Path::new(&tree.path).join("file")).unwrap(),
            "PR head content\n"
        );
        assert!(!Path::new(&tree.path).join(".git").exists());
        assert_eq!(
            git(&mirror, &["worktree", "list", "--porcelain"]),
            format!(
                "worktree {}\nbare",
                mirror.canonicalize().unwrap().display()
            )
        );
        // Process filters override smudge and have their own protocol. No definition from the
        // mirror may reach the archive process, even one that exits without a protocol reply.
        git(
            &mirror,
            &[
                "config",
                "filter.canary.process",
                &format!("touch '{}' ; exit 91", filter_marker.display()),
            ],
        );
        let second = manager
            .ensure_investigate_export(origin.to_str().unwrap(), 43, &sha)
            .await
            .unwrap();
        assert!(
            !filter_marker.exists(),
            "archive executed a host process filter"
        );
        assert_eq!(
            std::fs::read_to_string(Path::new(&second.path).join("file")).unwrap(),
            "PR head content\n"
        );
        manager
            .remove_investigate_export(origin.to_str().unwrap(), 43)
            .await
            .unwrap();
        manager
            .remove_investigate_export(origin.to_str().unwrap(), 42)
            .await
            .unwrap();
        assert!(!hook_marker.exists(), "export removal executed a host hook");
    }

    #[tokio::test]
    async fn pr_export_preserves_tracked_content_despite_archive_attributes() {
        use std::os::unix::fs::PermissionsExt;
        let root = TempDir::new();
        let root_path = Path::new(&root.path);
        let origin = root_path.join("origin");
        std::fs::create_dir(&origin).unwrap();
        git(&origin, &["init", "-b", "main"]);
        std::fs::create_dir(origin.join("directory")).unwrap();
        std::fs::create_dir(origin.join("nested")).unwrap();
        for (path, bytes) in [
            ("hidden", "tracked code must stay visible\n"),
            ("visible", "$Format:%H$\n$Id$\noriginal line endings\n"),
            ("directory/file", "ignored directories must stay visible\n"),
            ("nested/file", "$Format:%H$\nnested tracked code\n"),
            ("script", "#!/bin/sh\nexit 0\n"),
        ] {
            std::fs::write(origin.join(path), bytes).unwrap();
        }
        std::fs::set_permissions(
            origin.join("script"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        std::os::unix::fs::symlink("../hidden", origin.join("nested/link")).unwrap();
        // Stage raw blobs before attributes exist, so the fixture itself cannot convert bytes.
        git(&origin, &["add", "."]);
        std::fs::write(
            origin.join(".gitattributes"),
            "* export-subst text eol=crlf ident\nhidden export-ignore\ndirectory export-ignore\n",
        )
        .unwrap();
        std::fs::write(
            origin.join("nested/.gitattributes"),
            "* export-ignore export-subst\n",
        )
        .unwrap();
        git(&origin, &["add", ".gitattributes", "nested/.gitattributes"]);
        git(&origin, &["commit", "-m", "untrusted archive attributes"]);
        let sha = git(&origin, &["rev-parse", "HEAD"]);
        std::fs::write(origin.join("visible"), "newer content\n").unwrap();
        git(&origin, &["add", "visible"]);
        git(&origin, &["commit", "-m", "newer than the pinned head"]);
        let mirror = root_path
            .join(".mirrors")
            .join(format!("{}.git", crate::repo_key(origin.to_str().unwrap())));
        std::fs::create_dir_all(mirror.parent().unwrap()).unwrap();
        git(
            root_path,
            &[
                "clone",
                "--bare",
                origin.to_str().unwrap(),
                mirror.to_str().unwrap(),
            ],
        );
        let manager = Manager::new(crate::Config {
            root: root.path.clone(),
            hooks: crate::HookScripts::default(),
            hook_timeout: std::time::Duration::from_secs(1),
        })
        .unwrap();
        let tree = manager
            .ensure_investigate_export(origin.to_str().unwrap(), 42, &sha)
            .await
            .unwrap();
        let mut mismatches = Vec::new();
        for path in git(&mirror, &["ls-tree", "-r", "--name-only", &sha]).lines() {
            let exported = Path::new(&tree.path).join(path);
            if path == "nested/link" {
                if std::fs::read_link(&exported).ok().as_deref() != Some(Path::new("../hidden")) {
                    mismatches.push(format!("{path}: missing or altered symlink"));
                }
            } else if !exported.is_file() {
                mismatches.push(format!("{path}: tracked file omitted"));
            } else {
                let expected = git(&mirror, &["rev-parse", &format!("{sha}:{path}")]);
                let actual = git(
                    root_path,
                    &["hash-object", "--no-filters", exported.to_str().unwrap()],
                );
                if actual != expected {
                    mismatches.push(format!("{path}: tracked bytes rewritten"));
                }
            }
        }
        assert!(
            mismatches.is_empty(),
            "PR archive differs from the verified head: {mismatches:?}"
        );
        assert_ne!(
            std::fs::metadata(Path::new(&tree.path).join("script"))
                .unwrap()
                .permissions()
                .mode()
                & 0o111,
            0
        );
        assert!(!Path::new(&tree.path).join(".git").exists());
        manager
            .remove_investigate_export(origin.to_str().unwrap(), 42)
            .await
            .unwrap();
        assert!(!Path::new(&tree.path).exists());
    }

    #[tokio::test]
    async fn investigation_export_is_cached_only_and_hook_free() {
        let root = TempDir::new();
        let root_path = Path::new(&root.path);
        let origin = root_path.join("origin");
        std::fs::create_dir(&origin).unwrap();
        git(&origin, &["init", "-b", "main"]);
        std::fs::write(origin.join("file"), "content").unwrap();
        std::os::unix::fs::symlink(
            "/nonexistent/operator-credential",
            origin.join("credential-link"),
        )
        .unwrap();
        git(&origin, &["add", "."]);
        git(&origin, &["commit", "-m", "fixture"]);
        let sha = git(&origin, &["rev-parse", "HEAD"]);
        std::fs::write(origin.join("file"), "newer head content").unwrap();
        git(&origin, &["add", "file"]);
        git(&origin, &["commit", "-m", "newer than the pinned PR head"]);
        let mirror = root_path
            .join(".mirrors")
            .join(format!("{}.git", crate::repo_key(origin.to_str().unwrap())));
        std::fs::create_dir_all(mirror.parent().unwrap()).unwrap();
        git(
            root_path,
            &[
                "clone",
                "--bare",
                origin.to_str().unwrap(),
                mirror.to_str().unwrap(),
            ],
        );
        // Lifecycle and git hooks fail if accidentally invoked on the host.
        std::fs::write(mirror.join("hooks/post-checkout"), "#!/bin/sh\nexit 88\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            mirror.join("hooks/post-checkout"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let manager = Manager::new(crate::Config {
            root: root.path.clone(),
            hooks: crate::HookScripts {
                after_create: "exit 89".into(),
                before_remove: "exit 90".into(),
                ..Default::default()
            },
            hook_timeout: std::time::Duration::from_secs(1),
        })
        .unwrap();
        let tree = manager
            .ensure_investigate_export(origin.to_str().unwrap(), 42, &sha)
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(Path::new(&tree.path).join("file")).unwrap(),
            "content"
        );
        assert!(!Path::new(&tree.path).join(".git").exists());
        assert_eq!(
            std::fs::read_link(Path::new(&tree.path).join("credential-link")).unwrap(),
            Path::new("/nonexistent/operator-credential")
        );
        manager
            .remove_investigate_export(origin.to_str().unwrap(), 42)
            .await
            .unwrap();
        assert!(!Path::new(&tree.path).exists());
        manager
            .remove_investigate_export(origin.to_str().unwrap(), 42)
            .await
            .unwrap();
        assert!(
            manager
                .ensure_investigate_export(origin.to_str().unwrap(), 43, &"f".repeat(40))
                .await
                .is_err()
        );
        assert!(!Path::new(&manager.path_for(origin.to_str().unwrap(), "investigate-43")).exists());
        // Removal must not follow a planted export-root symlink or delete its target.
        let planted = manager.path_for(origin.to_str().unwrap(), "investigate-44");
        std::os::unix::fs::symlink(&origin, &planted).unwrap();
        assert!(matches!(
            manager
                .remove_investigate_export(origin.to_str().unwrap(), 44)
                .await,
            Err(Error::WorkspaceSymlink(_))
        ));
        assert!(origin.join("file").exists());
    }
}
