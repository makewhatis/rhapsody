//! Hook-free cached detached worktrees for the lead's Docker sandbox (STUDIO-1135).
//! No Go counterpart. No repository code runs on the host.
use crate::repo::is_commit_sha;
use crate::safety::ensure_within_root;
use crate::{Error, Manager, Workspace};

impl Manager {
    pub async fn ensure_investigate_worktree(
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
        if let Err(error) = self
            .investigate_git(&mirror, &["worktree", "add", "--detach", &path, sha])
            .await
        {
            // The path was absent under the repo lock before this attempt, so any partial tree
            // here belongs to us. Preserve the original failure if rollback also fails.
            if std::fs::symlink_metadata(&path).is_ok()
                && self
                    .investigate_git(&mirror, &["worktree", "remove", "--force", &path])
                    .await
                    .is_err()
            {
                return Err(Error::WorktreeRemove(format!(
                    "partial investigation rollback failed after {error}"
                )));
            }
            return Err(error);
        }
        Ok(Workspace {
            path,
            key,
            created_now: true,
        })
    }

    pub async fn remove_investigate_worktree(
        &self,
        repo_url: &str,
        run_id: i64,
    ) -> Result<(), Error> {
        let _lock = self.repo_lock(repo_url).lock_owned().await;
        let path = self.path_for(repo_url, &format!("investigate-{run_id}"));
        ensure_within_root(&self.root, &path)?;
        if matches!(std::fs::symlink_metadata(&path), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
        {
            return Ok(());
        }
        self.investigate_git(
            &self.mirror_dir(repo_url),
            &["worktree", "remove", "--force", &path],
        )
        .await
    }

    // Lifecycle provisioning executes hooks. A clean environment also excludes global git
    // filters, SSH agents and credentials; cached checkout never needs network/authentication.
    async fn investigate_git(&self, mirror: &str, args: &[&str]) -> Result<(), Error> {
        let mut cmd = tokio::process::Command::new("git");
        cmd.env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", "/nonexistent")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "core.fsmonitor=false",
                "-C",
                mirror,
            ])
            .args(args)
            .kill_on_drop(true)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let status = tokio::time::timeout(std::time::Duration::from_secs(60), cmd.status())
            .await
            .map_err(|_| Error::ReviewCheckout("investigation git timed out".into()))?
            .map_err(|_| Error::ReviewCheckout("investigation git could not start".into()))?;
        if status.success() {
            Ok(())
        } else {
            Err(Error::ReviewCheckout(
                "cached investigation checkout failed".into(),
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
    async fn investigation_checkout_is_detached_cached_only_and_hook_free() {
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
            .ensure_investigate_worktree(origin.to_str().unwrap(), 42, &sha)
            .await
            .unwrap();
        assert_eq!(git(Path::new(&tree.path), &["rev-parse", "HEAD"]), sha);
        assert_eq!(
            git(Path::new(&tree.path), &["branch", "--show-current"]),
            ""
        );
        assert_eq!(
            std::fs::read_link(Path::new(&tree.path).join("credential-link")).unwrap(),
            Path::new("/nonexistent/operator-credential")
        );
        manager
            .remove_investigate_worktree(origin.to_str().unwrap(), 42)
            .await
            .unwrap();
        assert!(!Path::new(&tree.path).exists());
        assert!(
            manager
                .ensure_investigate_worktree(origin.to_str().unwrap(), 43, &"f".repeat(40))
                .await
                .is_err()
        );
    }
}
