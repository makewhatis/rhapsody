//! Lifecycle hook execution (`hooks.go`, upstream §9.4).
//!
//! Runs a `bash -lc` hook in a directory with a layered environment, bounded by a timeout, mapping a
//! non-zero exit to [`Error::HookFailed`] and a deadline to [`Error::HookTimeout`]. W2 adds the
//! process-group `SIGKILL` on timeout that `hooks.go` performs via
//! `cmd.Cancel = syscall.Kill(-pid, SIGKILL)`: the hook runs in its own process group
//! ([`process_group(0)`](tokio::process::Command::process_group)) so a deadline kills the *whole*
//! group — a backgrounded grandchild included — rather than only `bash`. Without it a grandchild
//! survives, holds the inherited output pipe open, and the reap blocks (hooks_test.go's
//! `TestHookTimeoutKillsBackgroundedGrandchild`).
//!
//! Go's `WaitDelay` backstop (a *successful* hook that leaks a pipe-holding background process is
//! bounded to 10s then treated as success) is a documented misuse path with no mirrored test; the
//! Rust runner instead bounds it by the hook timeout and reports it as a timeout. See the PR notes.

use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

use crate::Error;

/// Caps how many bytes of hook output are kept in errors, to bound size from a chatty hook.
const MAX_HOOK_OUTPUT: usize = 4096;

/// Executes a single workspace hook script (upstream §9.4). The parity mirror of Go's unexported
/// `hookRunner`; the logger it carries is elided in W1 (best-effort logs are not observable parity
/// and no mirrored test asserts them).
#[derive(Debug, Clone)]
pub struct HookRunner {
    timeout: Duration,
}

impl HookRunner {
    /// Builds a runner with the given per-hook timeout.
    pub(crate) fn new(timeout: Duration) -> HookRunner {
        HookRunner { timeout }
    }

    /// Executes `script` via `bash -lc` in `dir` with the inherited environment unchanged — the
    /// parity mirror of Go's `run` (it passes no extra env, so the hook sees the daemon's
    /// environment byte-for-byte). An empty script is a no-op.
    ///
    /// `#[cfg(test)]`: production always threads the SYMPHONY_* env via [`Self::run_env`], so this
    /// no-extra-env form is currently exercised only by the `hooks_test.go` mirrors (which drive the
    /// runner directly). Kept as a faithful mirror of Go's `run`; promote to non-test if a caller
    /// ever needs it.
    #[cfg(test)]
    pub(crate) async fn run(&self, name: &str, script: &str, dir: &str) -> Result<(), Error> {
        self.run_env(name, script, dir, None).await
    }

    /// Executes `script` via `bash -lc` in `dir`, bounded by the runner timeout, with extra
    /// `KEY=VALUE` entries layered on top of the inherited environment. An empty script is a no-op.
    /// On timeout the whole process group is `SIGKILL`ed and [`Error::HookTimeout`] returned; a
    /// non-zero exit yields [`Error::HookFailed`] with truncated combined output.
    ///
    /// `extra == None` leaves the environment inherited unchanged (byte-for-byte the legacy hook
    /// process, matching Go's `run`); `extra == Some(..)` adds/overrides those keys — `exec` uses
    /// the last duplicate, and setting a key on the command has the same last-wins effect.
    pub(crate) async fn run_env(
        &self,
        name: &str,
        script: &str,
        dir: &str,
        extra: Option<&[String]>,
    ) -> Result<(), Error> {
        if script.is_empty() {
            return Ok(());
        }
        let mut cmd = Command::new("bash");
        cmd.arg("-lc")
            .arg(script)
            .current_dir(dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Own process group so a timeout can SIGKILL the whole group (mirrors hooks.go's
            // `cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}`).
            .process_group(0)
            // Backstop for an *external* cancellation of this future (task abort): reap `bash`.
            .kill_on_drop(true);
        if let Some(extra) = extra {
            for kv in extra {
                if let Some((k, v)) = kv.split_once('=') {
                    cmd.env(k, v);
                }
            }
        }
        let child = cmd
            .spawn()
            .map_err(|e| Error::HookFailed(format!("hook {name:?}: spawn: {e}")))?;
        // The child leads its own process group (pgid == its pid, from `process_group(0)`); capture
        // it before `wait_with_output` consumes `child`.
        let pgid = child.id().map(|p| p as i32);

        // `wait_with_output` drains stdout+stderr concurrently while waiting, so a chatty hook can't
        // deadlock on a full pipe.
        let fut = child.wait_with_output();
        tokio::pin!(fut);
        tokio::select! {
            res = fut.as_mut() => {
                let output = res.map_err(|e| Error::HookFailed(format!("hook {name:?}: {e}")))?;
                if output.status.success() {
                    return Ok(());
                }
                let mut combined = output.stdout;
                combined.extend_from_slice(&output.stderr);
                Err(Error::HookFailed(format!(
                    "hook {name:?}: {}: {}",
                    output.status,
                    truncate_output(&combined)
                )))
            }
            _ = tokio::time::sleep(self.timeout) => {
                // Deadline exceeded. SIGKILL the ENTIRE process group so a backgrounded grandchild
                // dies too (else it holds the output pipe open and the reap below never returns),
                // mirroring hooks.go's `cmd.Cancel`. Kill BEFORE reaping to avoid a pid-reuse race
                // on the group id.
                if let Some(pgid) = pgid {
                    // SAFETY: `kill(2)` with a negative pgid signals a process group. `pgid` is this
                    // child's own group (`process_group(0)`), still live because it is unreaped.
                    unsafe {
                        libc::kill(-pgid, libc::SIGKILL);
                    }
                }
                // The group is dead: the pipe closes and `wait_with_output` completes promptly,
                // reaping the child (no orphan, no zombie).
                let _ = fut.as_mut().await;
                Err(Error::HookTimeout(format!(
                    "hook {name:?} timed out after {:?}",
                    self.timeout
                )))
            }
        }
    }
}

/// Truncates hook/git output to [`MAX_HOOK_OUTPUT`] bytes, backing off to a UTF-8 leading byte so
/// truncation never splits a multi-byte sequence (mirrors Go's `truncateOutput`).
pub(crate) fn truncate_output(b: &[u8]) -> String {
    if b.len() <= MAX_HOOK_OUTPUT {
        return String::from_utf8_lossy(b).into_owned();
    }
    let mut end = MAX_HOOK_OUTPUT;
    // Back off while b[end] is a UTF-8 continuation byte (0b10xxxxxx).
    while end > 0 && (b[end] & 0xC0) == 0x80 {
        end -= 1;
    }
    format!("{}...(truncated)", String::from_utf8_lossy(&b[..end]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use std::time::Instant;

    // Mirror of TestHookSuccessRunsInWorkspaceDir: a relative-path write proves cwd == dir.
    #[tokio::test]
    async fn hook_success_runs_in_workspace_dir() {
        let dir = TempDir::new();
        let r = HookRunner::new(Duration::from_secs(5));
        r.run("after_create", "echo hi > marker.txt", &dir.path)
            .await
            .unwrap();
        assert!(
            std::fs::metadata(dir.child("marker.txt")).is_ok(),
            "hook did not run in workspace dir"
        );
    }

    // Mirror of TestHookEmptyScriptIsNoop.
    #[tokio::test]
    async fn hook_empty_script_is_noop() {
        let r = HookRunner::new(Duration::from_secs(5));
        let dir = TempDir::new();
        r.run("before_run", "", &dir.path)
            .await
            .expect("empty script should be a no-op");
    }

    // Mirror of TestHookFailureReturnsErrHookFailedAndLogs. The crate elides best-effort logging (a
    // W1 decision — no mirrored test asserts observable log output), so this mirrors the two
    // error-VALUE assertions: category ErrHookFailed and the hook output carried in the message.
    #[tokio::test]
    async fn hook_failure_returns_err_hook_failed() {
        let r = HookRunner::new(Duration::from_secs(5));
        let dir = TempDir::new();
        let err = r
            .run("before_run", "echo boom-output; exit 3", &dir.path)
            .await
            .unwrap_err();
        assert!(
            matches!(err, Error::HookFailed(_)),
            "got {err}, want HookFailed"
        );
        assert!(
            err.to_string().contains("boom-output"),
            "error should include hook output, got {err}"
        );
    }

    // Mirror of TestHookTimeoutReturnsErrHookTimeout.
    #[tokio::test]
    async fn hook_timeout_returns_err_hook_timeout() {
        let r = HookRunner::new(Duration::from_millis(100));
        let dir = TempDir::new();
        let start = Instant::now();
        let err = r
            .run("after_create", "sleep 5", &dir.path)
            .await
            .unwrap_err();
        assert!(
            matches!(err, Error::HookTimeout(_)),
            "got {err}, want HookTimeout"
        );
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "timeout took too long: {:?}",
            start.elapsed()
        );
    }

    // Mirror of TestHookOutputTruncated: emit > 4KB then fail, so the error carries truncated output.
    #[tokio::test]
    async fn hook_output_truncated() {
        let r = HookRunner::new(Duration::from_secs(5));
        let dir = TempDir::new();
        let err = r
            .run(
                "before_run",
                "head -c 10000 /dev/zero | tr '\\0' 'a'; exit 1",
                &dir.path,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, Error::HookFailed(_)),
            "got {err}, want HookFailed"
        );
        assert!(
            err.to_string().contains("truncated"),
            "large output should be truncated, got {err}"
        );
    }

    // Mirror of TestHookRunEnvInjectsExtraVars.
    #[tokio::test]
    async fn hook_run_env_injects_extra_vars() {
        let dir = TempDir::new();
        let r = HookRunner::new(Duration::from_secs(5));
        let script = r#"printf '%s|%s' "$SYMPHONY_REPO" "$SYMPHONY_ISSUE" > seen.txt"#;
        r.run_env(
            "after_create",
            script,
            &dir.path,
            Some(&[
                "SYMPHONY_REPO=git@x/y.git".to_string(),
                "SYMPHONY_ISSUE=MT-1".to_string(),
            ]),
        )
        .await
        .unwrap();
        let seen = std::fs::read_to_string(dir.child("seen.txt")).unwrap();
        assert_eq!(seen, "git@x/y.git|MT-1", "env not injected");
    }

    // Mirror of TestHookTimeoutKillsBackgroundedGrandchild: the backgrounded grandchild must be
    // killed via the process group, else it holds the output pipe open and the reap blocks ~30s.
    // Because the runner SIGKILLs the group and THEN reaps, a regressed group-kill would make the
    // reap hang here — so this timing bound genuinely proves the grandchild died.
    #[tokio::test]
    async fn hook_timeout_kills_backgrounded_grandchild() {
        let r = HookRunner::new(Duration::from_millis(200));
        let dir = TempDir::new();
        let start = Instant::now();
        let err = r
            .run("before_run", "sleep 30 & sleep 30", &dir.path)
            .await
            .unwrap_err();
        assert!(
            matches!(err, Error::HookTimeout(_)),
            "got {err}, want HookTimeout"
        );
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "timeout did not kill the grandchild promptly: took {:?}",
            start.elapsed()
        );
    }
}
