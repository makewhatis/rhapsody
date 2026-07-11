//! `gtguard` — injects a Graphite-workflow guardrail into an agent's worktree (parity port of
//! `internal/workspace/gtguard`).
//!
//! When a dispatch's effective `git_flow` is `"graphite"`, Symphony writes a Claude Code PreToolUse
//! hook (`.claude/hooks/gt-guard.sh`) plus a `.claude/settings.local.json` that registers it for the
//! Bash tool, BEFORE the agent spawns. The hook blocks raw mutating git commands (commit/push/
//! bulk-add) and points the agent at the `gt …` equivalent — deterministic enforcement of the
//! workflow that prompts alone failed to hold (INF-251).
//!
//! `settings.local.json` (not `settings.json`) is used deliberately: Claude Code merges it on top of
//! any repo-committed `.claude/settings.json` (Local scope > Project scope), so a repo's own hooks
//! are never clobbered. It is conventionally git-ignored; because Symphony writes it rather than
//! Claude Code, the agent's explicit-path staging keeps it out of commits even when the repo does not
//! ignore it.
//!
//! The two shell assets are COPIED VERBATIM from the reference (`crates/workspace/gtguard/`) and
//! embedded with [`include_bytes!`] — runtime assets, not ports (exactly as `harness/stubs/
//! fake-claude` was copied). Go's `//go:embed` becomes `include_bytes!`; a canary test asserts the
//! embedded bytes are byte-identical to the on-disk assets.

use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use crate::Error;
use crate::safety::{join, mkdir_all};

/// The canonical guard hook, embedded verbatim from `../gtguard/gt-guard.sh` (mirror of Go's
/// `//go:embed gt-guard.sh`). Written into each graphite worktree as `.claude/hooks/gt-guard.sh`.
const GUARD_SCRIPT: &[u8] = include_bytes!("../gtguard/gt-guard.sh");

/// The Claude Code settings overlay registering the hook, embedded verbatim from
/// `../gtguard/settings.local.json` (mirror of Go's `//go:embed settings.local.json`).
const SETTINGS_LOCAL: &[u8] = include_bytes!("../gtguard/settings.local.json");

/// The `git_flow` value that enables enforcement.
pub const GRAPHITE_FLOW: &str = "graphite";

const CLAUDE_DIR: &str = ".claude";
const HOOKS_DIR: &str = "hooks";
const SCRIPT_NAME: &str = "gt-guard.sh";
const SETTINGS_NAME: &str = "settings.local.json";

/// Writes the guardrail files into `worktree_dir` when `git_flow == "graphite"`, and writes nothing
/// for any other value (incl. `""` / `"any"`). Reports whether files were written. The write is
/// idempotent: it overwrites existing files so a reused worktree always carries the current guard.
/// The hook script is mode 0755 (executable); the settings file 0644.
pub fn ensure_for_git_flow(worktree_dir: &str, git_flow: &str) -> Result<bool, Error> {
    if git_flow != GRAPHITE_FLOW {
        return Ok(false);
    }
    write(worktree_dir)?;
    Ok(true)
}

/// Installs the guard hook and `settings.local.json` into `worktree_dir`'s `.claude` tree,
/// unconditionally (callers gate on `git_flow` via [`ensure_for_git_flow`]). Existing files are
/// overwritten.
pub fn write(worktree_dir: &str) -> Result<(), Error> {
    let hooks_path = join(&[worktree_dir, CLAUDE_DIR, HOOKS_DIR]);
    mkdir_all(&hooks_path).map_err(|e| Error::Gtguard(format!("create {hooks_path}: {e}")))?;

    let script_path = join(&[&hooks_path, SCRIPT_NAME]);
    std::fs::write(&script_path, GUARD_SCRIPT)
        .map_err(|e| Error::Gtguard(format!("write {script_path}: {e}")))?;
    // `std::fs::write` honors a create mode only when creating; force 0755 even when overwriting an
    // existing file with a different mode (mirror of gtguard.go's explicit `os.Chmod` after
    // `WriteFile`, which keeps a reused worktree's hook executable).
    std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| Error::Gtguard(format!("chmod {script_path}: {e}")))?;

    let settings_path = join(&[worktree_dir, CLAUDE_DIR, SETTINGS_NAME]);
    // Create mode 0644 (applied through umask on creation, left unchanged on overwrite) — the exact
    // semantics of gtguard.go's `os.WriteFile(settingsPath, …, 0o644)` with no follow-up chmod.
    // `OpenOptions::mode` (unlike `std::fs::write`'s fixed 0666 default) makes the create mode match
    // Go's perm argument regardless of the daemon's umask.
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o644)
        .open(&settings_path)
        .and_then(|mut f| std::io::Write::write_all(&mut f, SETTINGS_LOCAL))
        .map_err(|e| Error::Gtguard(format!("write {settings_path}: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    /// Materializes the embedded hook to a temp file (mode 0755) and returns its path, so a test can
    /// exec the canonical script bytes — the same ones written into worktrees (mirror of Go's
    /// `writeScript`).
    fn write_script() -> (TempDir, String) {
        let dir = TempDir::new();
        let p = dir.child("gt-guard.sh");
        std::fs::write(&p, GUARD_SCRIPT).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        (dir, p)
    }

    /// Pipes a PreToolUse payload for `command` into the hook and returns `(exit_code, stderr)`
    /// (mirror of Go's `runGuard`). The payload is built with `serde_json` exactly as the Go test
    /// uses `encoding/json`, so a command containing quotes is escaped correctly.
    fn run_guard(script_path: &str, command: &str) -> (i32, String) {
        let payload = serde_json::json!({
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": command},
        })
        .to_string();
        run_guard_payload(script_path, &payload, None)
    }

    /// Runs the hook against a raw JSON `payload`, optionally with a replacement `PATH` (used to
    /// force the no-jq fallback). Returns `(exit_code, stderr)`. Mirrors Go's `runGuard` /
    /// `runGuardEnv` process plumbing.
    fn run_guard_payload(script_path: &str, payload: &str, path: Option<&str>) -> (i32, String) {
        // Resolve bash to an absolute path via the PARENT's PATH before overriding the child's PATH,
        // exactly as Go's exec.Command(LookPath) does — otherwise the restricted (no-jq) PATH we set
        // for the child would also hide bash itself from the OS program lookup.
        let bash = which("bash").unwrap_or_else(|| "bash".to_string());
        let mut cmd = Command::new(&bash);
        cmd.arg(script_path)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped());
        if let Some(p) = path {
            cmd.env("PATH", p);
        }
        let mut child = cmd.spawn().expect("spawn bash");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.as_bytes())
            .unwrap();
        let out = child.wait_with_output().expect("wait bash");
        let exit = out.status.code().unwrap_or(-1);
        (exit, String::from_utf8_lossy(&out.stderr).into_owned())
    }

    // Mirror of TestGuardBlocksRawGit: every raw mutating git form must be blocked (exit 2) with
    // stderr naming both the `gt …` replacement and the policy.
    #[test]
    fn guard_blocks_raw_git() {
        let (_d, script) = write_script();
        let cases: &[(&str, &str, &str)] = &[
            ("commit", r#"git commit -m "wip""#, "gt create"),
            ("commit no args", "git commit", "gt modify --update"),
            ("push", "git push", "gt submit"),
            (
                "push force-with-lease",
                "git push --force-with-lease origin HEAD",
                "gt submit",
            ),
            ("add -A", "git add -A", "explicit path"),
            ("add --all", "git add --all", "explicit path"),
            ("add dot", "git add .", "explicit path"),
            ("chained commit", "cd repo && git commit -m x", "gt create"),
            ("push in subshell", "(git push)", "gt submit"),
            ("commit via -C", "git -C /repo commit -m x", "gt create"),
            ("push via -C", "git -C /repo push", "gt submit"),
            (
                "commit via -c kv",
                "git -c user.name=x commit -m y",
                "gt create",
            ),
            ("add -A via -C", "git -C /repo add -A", "explicit path"),
            (
                "commit via --git-dir",
                "git --git-dir=/p/.git commit",
                "gt create",
            ),
            ("push then redirect", "git push>log", "gt submit"),
            (
                "push then redirect devnull",
                "git push>/dev/null 2>&1",
                "gt submit",
            ),
            ("add -- dot", "git add -- .", "explicit path"),
        ];
        for (name, command, want_stderr) in cases {
            let (exit, stderr) = run_guard(&script, command);
            assert_eq!(
                exit, 2,
                "case {name}: command {command:?} exit = {exit}, want 2 (blocked); stderr={stderr:?}"
            );
            assert!(
                stderr.contains(want_stderr),
                "case {name}: command {command:?} stderr {stderr:?} does not mention {want_stderr:?}"
            );
            assert!(
                stderr.contains("git_flow=graphite"),
                "case {name}: command {command:?} stderr {stderr:?} should name the policy"
            );
        }
    }

    // Mirror of TestGuardAllowsGraphiteAndOthers: Graphite commands, explicit-path staging, and
    // read-only git (plus keyword substrings) must pass untouched.
    #[test]
    fn guard_allows_graphite_and_others() {
        let (_d, script) = write_script();
        let allowed = [
            r#"gt create -m "feat: x""#,
            "gt modify --update",
            "gt submit --draft",
            "gt checkout main",
            "git add path/to/file.go",
            "git add ./internal/config/x.go",
            "git add cmd/ internal/",
            "git status",
            "git log --oneline -5",
            "git diff --stat",
            "git fetch origin",
            "git -C /repo status",
            "git -c user.name=x log",
            "git committed-helper",
            "git pushing-branch --set-up",
            "ls -la && echo done",
            "go test ./...",
        ];
        for command in allowed {
            let (exit, stderr) = run_guard(&script, command);
            assert_eq!(
                exit, 0,
                "command {command:?}: exit = {exit}, want 0 (allowed); stderr={stderr:?}"
            );
        }
    }

    // Mirror of TestGuardPassesNonBash: a non-Bash payload (no command) must pass untouched.
    #[test]
    fn guard_passes_non_bash() {
        let (_d, script) = write_script();
        let payload = r#"{"hook_event_name":"PreToolUse","tool_name":"Edit","tool_input":{"file_path":"/tmp/x"}}"#;
        let (exit, stderr) = run_guard_payload(&script, payload, None);
        assert_eq!(
            exit, 0,
            "non-Bash payload should pass (exit 0); stderr={stderr:?}"
        );
    }

    /// Builds a temp bin dir containing ONLY symlinks to cat+grep (the external tools gt-guard.sh
    /// needs) and deliberately excluding jq, so a test can exercise the no-jq fallback path even
    /// though the host has jq installed (mirror of Go's `restrictedPATH`).
    fn restricted_path() -> Option<TempDir> {
        let bin = TempDir::new();
        for tool in ["cat", "grep"] {
            let src = which(tool)?;
            std::os::unix::fs::symlink(&src, bin.child(tool)).unwrap();
        }
        Some(bin)
    }

    /// Resolves `tool` on the ambient PATH (the port of Go's `exec.LookPath`), returning the first
    /// executable match.
    fn which(tool: &str) -> Option<String> {
        let path = std::env::var_os("PATH")?;
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join(tool);
            if let Ok(md) = std::fs::metadata(&candidate)
                && md.is_file()
                && md.permissions().mode() & 0o111 != 0
            {
                return Some(candidate.to_string_lossy().into_owned());
            }
        }
        None
    }

    // Mirror of TestGuardNoJqFallback: with jq removed from PATH, a Bash `git push` is still blocked
    // (over-broad raw-payload match), while a non-Bash Edit whose content mentions a blocked phrase
    // still passes (the fallback is scoped to Bash tool calls).
    #[test]
    fn guard_no_jq_fallback() {
        if which("jq").is_none() {
            return; // jq not on host; the jq-present and jq-absent paths are identical here.
        }
        let Some(bin) = restricted_path() else {
            return; // missing cat/grep on host, cannot test the no-jq fallback.
        };
        let (_d, script) = write_script();

        let blocked = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"git push"}}"#;
        let (exit, _) = run_guard_payload(&script, blocked, Some(&bin.path));
        assert_eq!(
            exit, 2,
            "no-jq Bash git push: exit = {exit}, want 2 (blocked)"
        );

        let non_bash = r#"{"hook_event_name":"PreToolUse","tool_name":"Edit","tool_input":{"file_path":"/tmp/x","new_string":"run git push to deploy"}}"#;
        let (exit, _) = run_guard_payload(&script, non_bash, Some(&bin.path));
        assert_eq!(
            exit, 0,
            "no-jq non-Bash Edit: exit = {exit}, want 0 (passed)"
        );
    }

    // Mirror of TestEnsureForGitFlowWritesWhenGraphite: writes an executable hook whose bytes equal
    // the embedded script, plus a valid settings.local.json registering one PreToolUse Bash hook
    // that runs gt-guard.sh.
    #[test]
    fn ensure_for_git_flow_writes_when_graphite() {
        let dir = TempDir::new();
        let wrote = ensure_for_git_flow(&dir.path, "graphite").expect("ensure_for_git_flow");
        assert!(
            wrote,
            "ensure_for_git_flow(graphite) should report files written"
        );

        let script_path = join(&[&dir.path, ".claude", "hooks", "gt-guard.sh"]);
        let info = std::fs::metadata(&script_path).expect("hook script not written");
        assert!(
            info.permissions().mode() & 0o111 != 0,
            "hook script not executable: mode {:o}",
            info.permissions().mode()
        );
        let got = std::fs::read(&script_path).unwrap();
        assert_eq!(
            got, GUARD_SCRIPT,
            "hook script content differs from the embedded canonical script"
        );

        let settings_path = join(&[&dir.path, ".claude", "settings.local.json"]);
        let sb = std::fs::read_to_string(&settings_path).expect("settings.local.json not written");
        // Must be valid JSON registering a PreToolUse hook for the Bash tool that points at the guard.
        let parsed: serde_json::Value =
            serde_json::from_str(&sb).expect("settings.local.json is not valid JSON");
        let pre = &parsed["hooks"]["PreToolUse"];
        assert_eq!(
            pre.as_array().map(|a| a.len()),
            Some(1),
            "settings.local.json must register exactly one PreToolUse matcher, got {pre}"
        );
        assert_eq!(
            pre[0]["matcher"], "Bash",
            "PreToolUse matcher must be Bash, got {}",
            pre[0]["matcher"]
        );
        let h = &pre[0]["hooks"];
        assert_eq!(
            h.as_array().map(|a| a.len()),
            Some(1),
            "matcher must carry exactly one hook, got {h}"
        );
        assert_eq!(
            h[0]["type"], "command",
            "hook type must be command, got {}",
            h[0]["type"]
        );
        assert!(
            h[0]["command"]
                .as_str()
                .unwrap_or("")
                .contains("gt-guard.sh"),
            "hook command must run gt-guard.sh, got {}",
            h[0]["command"]
        );
    }

    // Mirror of TestEnsureForGitFlowSkipsWhenNotGraphite: "" and "any" write nothing and create no
    // .claude tree.
    #[test]
    fn ensure_for_git_flow_skips_when_not_graphite() {
        for gf in ["", "any"] {
            let dir = TempDir::new();
            let wrote = ensure_for_git_flow(&dir.path, gf)
                .unwrap_or_else(|e| panic!("ensure({gf:?}): {e}"));
            assert!(!wrote, "ensure_for_git_flow({gf:?}) should write nothing");
            assert!(
                matches!(
                    std::fs::symlink_metadata(join(&[&dir.path, ".claude"])),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound
                ),
                "ensure_for_git_flow({gf:?}) must not create .claude"
            );
        }
    }

    // Mirror of TestEnsureForGitFlowIdempotent: re-running over an existing worktree succeeds and
    // restores the hook's executable bit (idempotent overwrite).
    #[test]
    fn ensure_for_git_flow_idempotent() {
        let dir = TempDir::new();
        ensure_for_git_flow(&dir.path, "graphite").expect("first write");
        let script_path = join(&[&dir.path, ".claude", "hooks", "gt-guard.sh"]);
        // Simulate a stale non-exec file.
        std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        ensure_for_git_flow(&dir.path, "graphite").expect("second write");
        let info = std::fs::metadata(&script_path).unwrap();
        assert!(
            info.permissions().mode() & 0o111 != 0,
            "re-write must restore executable bit, mode {:o}",
            info.permissions().mode()
        );
    }

    // Canary: the embedded bytes are byte-identical to the on-disk runtime assets under
    // `crates/workspace/gtguard/`, which are copied VERBATIM from the reference (INF-251). Guards
    // against an accidental in-repo edit drifting from the reference — the copied-verbatim contract.
    #[test]
    fn embedded_assets_match_on_disk() {
        let root = env!("CARGO_MANIFEST_DIR");
        let script = std::fs::read(join(&[root, "gtguard", "gt-guard.sh"])).unwrap();
        assert_eq!(
            script, GUARD_SCRIPT,
            "gt-guard.sh embed drifted from the on-disk asset"
        );
        let settings = std::fs::read(join(&[root, "gtguard", "settings.local.json"])).unwrap();
        assert_eq!(
            settings, SETTINGS_LOCAL,
            "settings.local.json embed drifted from the on-disk asset"
        );
    }
}
