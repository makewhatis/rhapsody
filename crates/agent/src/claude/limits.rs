//! Active subscription usage probe (STUDIO-1269); no Go counterpart.

use crate::ratelimit::{LimitObs, parse_claude_rate_limit};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct ProbeDir(PathBuf);

impl ProbeDir {
    fn new(state_root: &Path) -> Result<Self, &'static str> {
        // Claude names its home project directory after cwd, even without session persistence.
        // One stable path bounds that footprint; recreate it to discard crash leftovers/permissions.
        let path = state_root.join("claude-probe");
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(state_root)
            .map_err(|_| "usage_scratch")?;
        match std::fs::remove_dir_all(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err("usage_scratch"),
        }
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .map_err(|_| "usage_scratch")?;
        Ok(Self(path))
    }
}

impl Drop for ProbeDir {
    fn drop(&mut self) {
        if std::fs::remove_dir_all(&self.0).is_err() {
            tracing::warn!("Claude usage probe scratch cleanup failed");
        }
    }
}

/// The configured command owns authentication. Never open a credential store or quote CLI output.
pub async fn probe(
    command: &str,
    timeout: Duration,
    state_root: &Path,
) -> Result<Vec<LimitObs>, &'static str> {
    let deadline = tokio::time::Instant::now()
        .checked_add(timeout)
        .ok_or("usage_timeout")?;
    let (name, prefix) = super::split_command(command).map_err(|_| "usage_command")?;
    let scratch = ProbeDir::new(state_root)?;
    let cfg = super::Config {
        model: "haiku".into(),
        permission_mode: "default".into(),
        tools: Some(String::new()),
        mcp_config: r#"{"mcpServers":{}}"#.into(),
        extra_args: vec![
            "--max-turns".into(),
            "1".into(),
            "--no-session-persistence".into(),
        ],
        ..Default::default()
    };
    let mut cmd = tokio::process::Command::new(name);
    cmd.args(prefix)
        .args(super::build_args(&cfg, ""))
        .current_dir(&scratch.0)
        .process_group(0)
        .kill_on_drop(true)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    for key in super::BILLING_ENV_VARS
        .iter()
        .chain(super::TRACKER_ENV_VARS)
    {
        cmd.env_remove(key);
    }
    let mut child = cmd.spawn().map_err(|_| "usage_spawn")?;
    let pid = child.id().unwrap_or(0);
    let mut tree = crate::proctree::KillTreeOnDrop::new(pid);
    let mut reaped = false;
    let work = async {
        let mut stdin = child.stdin.take().ok_or("usage_io")?;
        let mut stdout = child.stdout.take().ok_or("usage_io")?;
        let prompt = super::runner::encode_user_message("Reply OK.").map_err(|_| "usage_io")?;
        // An early exit still needs its exit status, rather than a broken-pipe classification.
        let _ = stdin.write_all(&prompt).await;
        drop(stdin);
        let mut observations = Vec::new();
        let mut line = Vec::new();
        let mut chunk = [0u8; 8192];
        let mut output_bytes = 0;
        loop {
            let n = stdout.read(&mut chunk).await.map_err(|_| "usage_io")?;
            output_bytes += n;
            if output_bytes > 4 * 1024 * 1024 {
                return Err("usage_output_limit");
            }
            if n == 0 {
                if let Some(mut obs) = parse_claude_rate_limit(&line) {
                    obs.source = "probe";
                    observations.push(obs);
                }
                break;
            }
            for byte in &chunk[..n] {
                if *byte == b'\n' {
                    if let Some(mut obs) = parse_claude_rate_limit(&line) {
                        obs.source = "probe";
                        observations.push(obs);
                    }
                    line.clear();
                    if observations.len() > 1024 {
                        return Err("usage_output_limit");
                    }
                } else {
                    line.push(*byte);
                    if line.len() > 1024 * 1024 {
                        return Err("usage_output_limit");
                    }
                }
            }
        }
        let status = child.wait().await.map_err(|_| "usage_io")?;
        reaped = true;
        if !status.success() {
            Err("usage_exit")
        } else if observations.is_empty() {
            Err("usage_missing")
        } else {
            Ok(observations)
        }
    };
    let result = tokio::time::timeout_at(deadline, work)
        .await
        .unwrap_or(Err("usage_timeout"));
    if !reaped {
        crate::proctree::kill_tree(pid);
        let _ = child.wait().await;
    }
    tree.disarm();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opencode::testdir::TempDir;
    use std::time::Duration;

    #[tokio::test]
    async fn claude_probe_reads_every_event_with_runner_argv_in_empty_directory() {
        let _env = crate::ENV_GUARD.read().await;
        let dir = TempDir::new();
        let script = dir.path().join("probe.sh");
        let fixture = include_str!("../../testdata/limits/allowed.jsonl");
        std::fs::write(&script, format!(r#"
test -z "$(ls -A)" || exit 10
args=" $* "
for pair in '-p' '--output-format stream-json' '--input-format stream-json' '--verbose' '--model haiku' '--max-turns 1' '--no-session-persistence'; do
  case "$args" in *" $pair "*) ;; *) exit 11 ;; esac
done
read -r prompt
case "$prompt" in *'"type":"user"'*'"content":"Reply OK."'*) ;; *) exit 12 ;; esac
printf '%s\n' '{fixture}' '{fixture}'
"#)).unwrap();
        let observations = probe(
            &format!("bash {}", script.display()),
            Duration::from_secs(10),
            dir.path(),
        )
        .await
        .unwrap();
        assert_eq!(observations.len(), 2);
        for obs in observations {
            assert_eq!(obs.source, "probe");
            assert_eq!(
                obs.windows
                    .iter()
                    .find(|w| w.window == "five_hour")
                    .unwrap()
                    .utilization,
                0.05
            );
            assert_eq!(
                obs.windows
                    .iter()
                    .find(|w| w.window == "seven_day")
                    .unwrap()
                    .utilization,
                0.02
            );
        }
    }

    #[tokio::test]
    async fn claude_probe_reuses_one_working_directory() {
        use std::os::unix::fs::PermissionsExt;
        let _env = crate::ENV_GUARD.read().await;
        let dir = TempDir::new();
        let state_root = dir.path().join(".rhapsody");
        let scratch = state_root.join("claude-probe");
        let cwd_file = dir.path().join("cwd");
        let script = dir.path().join("probe.sh");
        std::fs::write(
            &script,
            format!(
                "test -z \"$(ls -A)\" || exit 10\ntest \"$(stat -c %a . 2>/dev/null || stat -f %Lp .)\" = 700 || exit 11\npwd >> '{}'\nprintf '%s\\n' '{}'\nmkdir leftover\nprintf junk > leftover/file",
                cwd_file.display(),
                include_str!("../../testdata/limits/allowed.jsonl")
            ),
        )
        .unwrap();
        let command = format!("bash {}", script.display());
        for _ in 0..2 {
            // Include crash leftovers and loose permissions: every launch must start empty/private.
            std::fs::create_dir_all(scratch.join("old/nested")).unwrap();
            std::fs::write(scratch.join("old/nested/file"), "junk").unwrap();
            std::fs::set_permissions(&scratch, std::fs::Permissions::from_mode(0o755)).unwrap();
            probe(&command, Duration::from_secs(10), &state_root)
                .await
                .unwrap();
        }
        let recorded = std::fs::read_to_string(cwd_file).unwrap();
        let paths: Vec<_> = recorded.lines().collect();
        assert_eq!(paths.len(), 2);
        assert_eq!(paths[0], paths[1], "every probe must reuse the same cwd");
        assert!(std::path::Path::new(paths[0]).starts_with(state_root.canonicalize().unwrap()));
    }

    #[tokio::test]
    async fn claude_probe_failure_reasons_are_closed_and_timeout_kills_process() {
        let _env = crate::ENV_GUARD.read().await;
        let dir = TempDir::new();
        let script = dir.path().join("probe.sh");
        for (body, reason, timeout) in [
            ("exit 7", "usage_exit", Duration::from_secs(10)),
            (
                "printf '%s\\n' not-a-rate-limit-event",
                "usage_missing",
                Duration::from_secs(10),
            ),
            ("exec sleep 30", "usage_timeout", Duration::from_millis(100)),
        ] {
            std::fs::write(&script, body).unwrap();
            assert_eq!(
                probe(&format!("bash {}", script.display()), timeout, dir.path()).await,
                Err(reason)
            );
        }
    }

    #[tokio::test]
    async fn claude_probe_keeps_unknown_windows() {
        let _env = crate::ENV_GUARD.read().await;
        let dir = TempDir::new();
        let script = dir.path().join("probe.sh");
        let mut fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../testdata/limits/allowed.jsonl")).unwrap();
        fixture["rate_limit_info"]["unifiedWindows"]["seven_day_opus"] =
            serde_json::json!({"utilization":0.97,"resetsAt":1791912600});
        std::fs::write(&script, format!("printf '%s\\n' '{fixture}'")).unwrap();
        let observations = probe(
            &format!("bash {}", script.display()),
            Duration::from_secs(10),
            dir.path(),
        )
        .await
        .unwrap();
        assert_eq!(
            observations[0]
                .windows
                .iter()
                .find(|w| w.window == "seven_day_opus")
                .unwrap()
                .utilization,
            0.97
        );
    }

    #[tokio::test]
    async fn timeout_and_cancellation_kill_probe_and_remove_scratch() {
        let _env = crate::ENV_GUARD.read().await;
        let dir = TempDir::new();
        for cancel in [false, true] {
            let script = dir.path().join("sleep.sh");
            let pid_file = dir.path().join("pid");
            let cwd_file = dir.path().join("cwd");
            std::fs::write(
                &script,
                format!(
                    "printf '%s' $$ > '{}'\npwd > '{}'\nexec sleep 30",
                    pid_file.display(),
                    cwd_file.display()
                ),
            )
            .unwrap();
            let command = format!("bash {}", script.display());
            let state_root = dir.path().to_path_buf();
            let task = tokio::spawn(async move {
                probe(
                    &command,
                    if cancel {
                        Duration::from_secs(30)
                    } else {
                        Duration::from_secs(1)
                    },
                    &state_root,
                )
                .await
            });
            tokio::time::timeout(Duration::from_secs(10), async {
                while !cwd_file.exists() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            let pid: i32 = std::fs::read_to_string(&pid_file).unwrap().parse().unwrap();
            let cwd = std::fs::read_to_string(&cwd_file).unwrap();
            if cancel {
                task.abort();
                assert!(task.await.unwrap_err().is_cancelled());
            } else {
                assert_eq!(task.await.unwrap(), Err("usage_timeout"));
            }
            tokio::time::timeout(Duration::from_secs(10), async {
                // SAFETY: signal 0 only checks this test's captured child PID.
                while unsafe { libc::kill(pid, 0) } == 0 {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            assert!(!std::path::Path::new(cwd.trim()).exists());
            std::fs::remove_file(pid_file).unwrap();
            std::fs::remove_file(cwd_file).unwrap();
        }
    }
}
