//! Lifecycle hook execution (`hooks.go`, upstream §9.4).
//!
//! W1 ships the run surface `repo_test.go` exercises: run a `bash -lc` hook in a directory with a
//! layered environment, bounded by a timeout, mapping a non-zero exit to [`Error::HookFailed`] and
//! a deadline to [`Error::HookTimeout`]. The full process-group `SIGKILL` on timeout, the
//! `WaitDelay` backstop for a backgrounded grandchild, and the dedicated `hooks_test.go` timeout
//! mirrors land in W2 (which adds the syscall dependency); no W1 test exercises those paths.

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

    /// Executes `script` via `bash -lc` in `dir`, bounded by the runner timeout, with extra
    /// `KEY=VALUE` entries layered on top of the inherited environment. An empty script is a no-op.
    /// On timeout the child is killed and [`Error::HookTimeout`] returned; a non-zero exit yields
    /// [`Error::HookFailed`] with truncated combined output.
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

        let output = match tokio::time::timeout(self.timeout, child.wait_with_output()).await {
            Ok(Ok(out)) => out,
            Ok(Err(e)) => return Err(Error::HookFailed(format!("hook {name:?}: {e}"))),
            Err(_elapsed) => {
                // The future owning the child is dropped here; `kill_on_drop` reaps the child. W2
                // upgrades this to a process-group SIGKILL so a backgrounded grandchild cannot
                // survive and hold the output pipe open past the deadline.
                return Err(Error::HookTimeout(format!(
                    "hook {name:?} timed out after {:?}",
                    self.timeout
                )));
            }
        };
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
