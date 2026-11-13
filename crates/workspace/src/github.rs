//! Shared GitHub credential policy for subprocesses and REST (STUDIO-1804).
//! References hot-reload; secrets are resolved off-loop and owned only by a run/call.

use rhapsody_config::github::Github;
use std::sync::{Arc, RwLock};

pub type Notify = Arc<dyn Fn(&str) + Send + Sync>;

pub trait Keychain: Send + Sync {
    /// Errors must be closed, secret-free reasons, never subprocess output.
    fn read(&self, service: &str) -> Result<String, String>;
}

pub struct SystemKeychain;

impl Keychain for SystemKeychain {
    fn read(&self, service: &str) -> Result<String, String> {
        let account = std::env::var("USER").map_err(|_| "USER is not set")?;
        let mut child = std::process::Command::new("/usr/bin/security")
            .args(["find-generic-password", "-s", service, "-a", &account, "-w"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|_| "keychain reader unavailable")?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            match child.try_wait() {
                Ok(Some(status)) if status.success() => break,
                Ok(Some(_)) => return Err("keychain item absent, locked or denied".into()),
                Ok(None) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(20))
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("keychain read timed out or failed".into());
                }
            }
        }
        let output = child
            .wait_with_output()
            .map_err(|_| "keychain read failed")?;
        let token = String::from_utf8(output.stdout).map_err(|_| "keychain item is not UTF-8")?;
        let token = token.trim().to_string();
        if token.is_empty() || token.contains(['\r', '\n', '\0']) {
            return Err("keychain item is empty or malformed".into());
        }
        Ok(token)
    }
}

pub struct Credentials {
    config: RwLock<Option<Github>>,
    source: Arc<dyn Keychain>,
    notify: Notify,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GithubCredentials([references only])")
    }
}

/// Clone the policy handle before blocking I/O; never hold a configuration lock across it.
pub async fn resolve_repo(
    policy: Arc<Credentials>,
    repo: String,
) -> Result<Option<Arc<RunAuth>>, String> {
    if !policy.configured() {
        return Ok(None);
    }
    tokio::task::spawn_blocking(move || policy.for_repo(&repo))
        .await
        .map_err(|_| "GitHub credential task failed".to_string())?
}

pub async fn resolve_owner(
    policy: Arc<Credentials>,
    owner: String,
) -> Result<Option<Arc<RunAuth>>, String> {
    if !policy.configured() {
        return Ok(None);
    }
    tokio::task::spawn_blocking(move || policy.for_owner(&owner))
        .await
        .map_err(|_| "GitHub credential task failed".to_string())?
}

impl Default for Credentials {
    fn default() -> Self {
        Self::new(Arc::new(SystemKeychain), Arc::new(|_| {}))
    }
}

impl Credentials {
    pub fn new(source: Arc<dyn Keychain>, notify: Notify) -> Self {
        Self {
            config: RwLock::new(None),
            source,
            notify,
        }
    }

    pub fn update(&self, config: Option<Github>) {
        *self.config.write().unwrap_or_else(|e| e.into_inner()) = config;
    }

    pub fn configured(&self) -> bool {
        self.config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|c| c.strict || !c.credentials.is_empty())
    }

    fn refusal(&self, owner: &str, reason: &str) -> String {
        let message = format!("GitHub credentials refused for owner {owner}: {reason}");
        (self.notify)(&message);
        message
    }

    pub fn for_repo(&self, repo: &str) -> Result<Option<Arc<RunAuth>>, String> {
        let owner = rhapsody_config::github::repo(repo).map(|(owner, _)| owner);
        self.for_owner(owner.as_deref().unwrap_or("<unknown>"))
    }

    pub fn for_owner(&self, owner: &str) -> Result<Option<Arc<RunAuth>>, String> {
        let config = self
            .config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let Some(config) = config else {
            return Ok(None);
        };
        let Some(credential) = config.credentials.get(&owner.to_ascii_lowercase()) else {
            return if config.strict {
                Err(self.refusal(owner, "no configured credential"))
            } else {
                Ok(None)
            };
        };
        let token = self
            .source
            .read(&credential.keychain)
            .map_err(|_| self.refusal(owner, "keychain item absent, locked or unreadable"))?;
        if token.is_empty() || token.contains(['\r', '\n', '\0']) {
            return Err(self.refusal(owner, "keychain item is empty or malformed"));
        }
        RunAuth::new(token)
            .map(Arc::new)
            .map(Some)
            .map_err(|_| self.refusal(owner, "cannot create private gh config directory"))
    }

    /// All daemon gh commands name a repo with --repo/-R, a REST repos endpoint, a PR URL,
    /// or the explicit `owner` variable of a GraphQL request.
    /// Do not search arbitrary body/query arguments for an owner: unscoped commands fail closed.
    pub fn for_gh(&self, args: &[&str]) -> Result<Option<Arc<RunAuth>>, String> {
        if !self.configured() {
            return Ok(None);
        }
        let owner = args
            .windows(2)
            .find(|a| matches!(a[0], "--repo" | "-R"))
            .and_then(|a| a[1].split_once('/').map(|(o, _)| o.to_string()))
            .or_else(|| {
                if args.get(..2) == Some(&["api", "graphql"]) {
                    return args
                        .windows(2)
                        .find(|a| {
                            matches!(a[0], "-f" | "-F" | "--field" | "--raw-field")
                                && a[1].starts_with("owner=")
                        })
                        .and_then(|a| a[1].strip_prefix("owner="))
                        .filter(|o| {
                            !o.is_empty()
                                && o.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                        })
                        .map(str::to_string);
                }
                if args.first() == Some(&"api") {
                    args.iter()
                        .skip(1)
                        .find(|s| !matches!(**s, "--paginate" | "--slurp"))
                        .and_then(|s| s.trim_start_matches('/').strip_prefix("repos/"))
                        .and_then(|s| s.split_once('/').map(|(o, _)| o.to_string()))
                } else if args.first() == Some(&"pr") {
                    args.get(2)
                        .and_then(|s| s.strip_prefix("https://github.com/"))
                        .and_then(|s| s.split_once('/').map(|(o, _)| o.to_string()))
                } else {
                    None
                }
            });
        self.for_owner(owner.as_deref().unwrap_or("<unknown>"))
    }
}

/// No Debug/Serialize: neither logs nor APIs can accidentally expose the token.
pub struct RunAuth {
    token: String,
    config_dir: tempfile::TempDir,
}

impl RunAuth {
    pub fn new(token: String) -> std::io::Result<Self> {
        let config_dir = tempfile::Builder::new().prefix("rhapsody-gh-").tempdir()?;
        Ok(Self { token, config_dir })
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn apply(&self, env: &mut Vec<String>) {
        env.retain(|entry| {
            let name = entry.split_once('=').map_or(entry.as_str(), |(k, _)| k);
            !matches!(
                name,
                "GH_TOKEN"
                    | "GITHUB_TOKEN"
                    | "GH_CONFIG_DIR"
                    | "GH_HOST"
                    | "GH_ENTERPRISE_TOKEN"
                    | "GITHUB_ENTERPRISE_TOKEN"
                    | "GIT_SSH"
                    | "GIT_SSH_COMMAND"
                    | "GIT_ASKPASS"
                    | "SSH_ASKPASS"
                    | "GIT_TERMINAL_PROMPT"
            ) && !name.starts_with("GIT_CONFIG")
        });
        env.extend([
            format!("GH_TOKEN={}", self.token),
            format!("GITHUB_TOKEN={}", self.token),
            format!("GH_CONFIG_DIR={}", self.config_dir.path().display()),
            "GH_HOST=github.com".into(),
            "GIT_SSH_COMMAND=false".into(),
            "GIT_TERMINAL_PROMPT=0".into(),
            "GIT_CONFIG_GLOBAL=/dev/null".into(),
            "GIT_CONFIG_NOSYSTEM=1".into(),
        ]);
        let config = [
            ("url.https://github.com/.insteadOf", "git@github.com:"),
            ("url.https://github.com/.insteadOf", "ssh://git@github.com/"),
            ("credential.helper", ""),
            ("credential.helper", "!gh auth git-credential"),
        ];
        env.push(format!("GIT_CONFIG_COUNT={}", config.len()));
        for (i, (key, value)) in config.iter().enumerate() {
            env.push(format!("GIT_CONFIG_KEY_{i}={key}"));
            env.push(format!("GIT_CONFIG_VALUE_{i}={value}"));
        }
    }

    pub fn environment(&self) -> Vec<(String, String)> {
        let mut env: Vec<String> = std::env::vars_os()
            .map(|(k, v)| format!("{}={}", k.to_string_lossy(), v.to_string_lossy()))
            .collect();
        self.apply(&mut env);
        env.into_iter()
            .filter_map(|s| s.split_once('=').map(|(k, v)| (k.into(), v.into())))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fake;
    impl Keychain for Fake {
        fn read(&self, service: &str) -> Result<String, String> {
            assert_eq!(service, "rhapsody-github-studio49dev");
            Ok("fake-owner-secret".into())
        }
    }
    #[test]
    fn owner_environment_is_private_and_cannot_fall_back() {
        let policy = Credentials::new(Arc::new(Fake), Arc::new(|_| {}));
        policy.update(Some(Github {
            strict: true,
            credentials: [(
                "studio49dev".into(),
                rhapsody_config::github::Credential {
                    keychain: "rhapsody-github-studio49dev".into(),
                },
            )]
            .into(),
        }));
        assert!(
            policy
                .for_gh(&[
                    "api",
                    "graphql",
                    "-f",
                    "query=query($owner:String!){repository(owner:$owner,name:\"example\"){name}}",
                    "-f",
                    "owner=studio49dev"
                ])
                .unwrap()
                .is_some()
        );
        assert!(
            policy
                .for_gh(&["api", "graphql", "-f", "query=owner=studio49dev"])
                .is_err(),
            "an owner hidden in query text is not a credential scope"
        );
        let auth = policy
            .for_repo("git@github.com:studio49dev/example.git")
            .unwrap()
            .unwrap();
        let mut env = vec![
            "GH_CONFIG_DIR=/operator/.config/gh".into(),
            "GH_TOKEN=personal".into(),
            "GIT_CONFIG_COUNT=99".into(),
            "GIT_SSH_COMMAND=ssh".into(),
        ];
        auth.apply(&mut env);
        assert!(env.contains(&"GH_TOKEN=fake-owner-secret".into()));
        assert!(env.contains(&"GITHUB_TOKEN=fake-owner-secret".into()));
        assert!(env.contains(&"GIT_SSH_COMMAND=false".into()));
        assert!(env.contains(&"GIT_CONFIG_VALUE_3=!gh auth git-credential".into()));
        assert!(env.contains(&"GIT_CONFIG_VALUE_0=git@github.com:".into()));
        assert!(env.contains(&"GIT_CONFIG_VALUE_1=ssh://git@github.com/".into()));
        assert!(
            !env.iter()
                .any(|s| s.contains("personal") || s.contains("/operator/.config/gh"))
        );
        assert_eq!(
            std::fs::read_dir(auth.config_dir.path()).unwrap().count(),
            0
        );
        let dir = auth.config_dir.path().to_path_buf();
        drop(auth);
        assert!(!dir.exists());
    }
    #[test]
    fn unconfigured_policy_is_identity_and_strict_refusal_notifies_owner() {
        let notices = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = notices.clone();
        let policy = Credentials::new(
            Arc::new(Fake),
            Arc::new(move |s| sink.lock().unwrap().push(s.to_string())),
        );
        assert!(
            policy
                .for_repo("git@github.com:fake-owner/example.git")
                .unwrap()
                .is_none()
        );
        policy.update(Some(Github::default()));
        assert!(!policy.configured());
        assert!(
            policy
                .for_gh(&["pr", "view", "1", "--repo", "fake-owner/example"])
                .unwrap()
                .is_none()
        );
        policy.update(Some(Github {
            strict: true,
            ..Default::default()
        }));
        assert!(
            policy
                .for_repo("git@github.com:fake-owner/example.git")
                .err()
                .unwrap()
                .contains("fake-owner")
        );
        assert!(
            policy
                .for_gh(&["pr", "view", "1", "--repo", "fake-owner/example"])
                .err()
                .unwrap()
                .contains("fake-owner")
        );
        assert_eq!(notices.lock().unwrap().len(), 2);
        assert!(
            notices
                .lock()
                .unwrap()
                .iter()
                .all(|s| s.contains("fake-owner"))
        );
    }

    #[test]
    fn hot_reload_changes_future_credentials_but_not_an_existing_run() {
        struct Source;
        impl Keychain for Source {
            fn read(&self, service: &str) -> Result<String, String> {
                Ok(format!("fake-{service}"))
            }
        }
        let policy = Credentials::new(Arc::new(Source), Arc::new(|_| {}));
        let config = |service: &str| {
            Some(Github {
                strict: true,
                credentials: [(
                    "studio49dev".into(),
                    rhapsody_config::github::Credential {
                        keychain: service.into(),
                    },
                )]
                .into(),
            })
        };
        policy.update(config("old-ref"));
        let run = policy
            .for_repo("https://github.com/Studio49dev/example")
            .unwrap()
            .unwrap();
        policy.update(config("new-ref"));
        assert_eq!(run.token(), "fake-old-ref");
        let next = policy
            .for_gh(&[
                "api",
                "--paginate",
                "--slurp",
                "repos/studio49dev/example/issues/comments",
            ])
            .unwrap()
            .unwrap();
        assert_eq!(next.token(), "fake-new-ref");
        policy.update(Some(Github {
            strict: true,
            ..Default::default()
        }));
        assert!(
            policy
                .for_gh(&["pr", "view", "1", "--repo", "studio49dev/example"])
                .is_err()
        );
        policy.update(None);
        assert!(
            policy
                .for_repo("https://github.com/studio49dev/example")
                .unwrap()
                .is_none()
        );
        assert_eq!(run.token(), "fake-old-ref");
    }

    #[test]
    fn git_uses_the_owner_helper_instead_of_a_personal_global_helper() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let gh = dir.path().join("gh");
        std::fs::write(&gh, "#!/bin/sh\n[ \"$1 $2\" = 'auth git-credential' ] || exit 1\nprintf 'username=fake-owner\\npassword=%s\\n' \"$GH_TOKEN\"\n").unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(dir.path().join(".gitconfig"), "[credential]\n helper = \"!printf 'username=personal\\npassword=personal-secret\\n'\"\n").unwrap();
        let auth = RunAuth::new("fake-scoped-secret".into()).unwrap();
        let lookup = |scoped: bool| {
            let mut cmd = std::process::Command::new("git");
            cmd.args(["credential", "fill"])
                .current_dir(dir.path())
                .env_clear()
                .envs(auth.environment())
                .env("HOME", dir.path())
                .env(
                    "PATH",
                    format!(
                        "{}:{}",
                        dir.path().display(),
                        std::env::var("PATH").unwrap()
                    ),
                )
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            if !scoped {
                cmd.env("GIT_CONFIG_GLOBAL", dir.path().join(".gitconfig"))
                    .env("GIT_CONFIG_COUNT", "0");
            }
            let mut child = cmd.spawn().unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(b"protocol=https\nhost=github.com\n\n")
                .unwrap();
            let output = child.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap()
        };
        assert!(
            lookup(false).contains("password=personal-secret"),
            "the control must reproduce personal credential fallback"
        );
        let output = lookup(true);
        assert!(output.contains("password=fake-scoped-secret"));
        assert!(!output.contains("personal"));
    }
}
