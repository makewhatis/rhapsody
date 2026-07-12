//! The desktop app's Tool-doctor preflight (INF-220, design §4): detects the external CLIs the daemon
//! shells out to (claude, gh, gt, git), reports presence + version + health, and honors per-tool path
//! overrides. Parity port of `$REF/desktop/internal/toolcheck/toolcheck.go`.
//!
//! (The override-dir helper `OverrideDirs` from `$REF/.../toolcheck/dirs.go` was already ported in
//! P7-D2 as [`crate::tooldirs::override_dirs`], so it is reused from there rather than duplicated.)

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use serde::Serialize;

use crate::supervisor::is_executable_file;

/// The default per-tool version-probe timeout when [`Prober::timeout`] is zero (Go's 5s default).
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

/// One external CLI to probe. Mirrors Go `toolcheck.Tool`.
pub struct Tool {
    /// Executable name (claude, gh, gt, git).
    pub name: &'static str,
    /// Args that print a version and exit 0 when healthy.
    pub version_args: &'static [&'static str],
}

/// The set the daemon depends on at runtime (GETTING_STARTED prerequisites). Mirrors `DefaultTools`.
pub fn default_tools() -> Vec<Tool> {
    vec![
        Tool {
            name: "claude",
            version_args: &["--version"],
        },
        Tool {
            name: "gh",
            version_args: &["--version"],
        },
        Tool {
            name: "gt",
            version_args: &["--version"],
        },
        Tool {
            name: "git",
            version_args: &["--version"],
        },
    ]
}

/// One tool's preflight status for the UI. Named `ToolResult` (not `Result`) to avoid shadowing
/// `std::result::Result`; the serde field names match Go `toolcheck.Result`'s json tags so the
/// webview's `ToolResult` sees the identical shape.
#[derive(Debug, Clone, Serialize)]
pub struct ToolResult {
    /// Tool name.
    pub name: String,
    /// Resolved executable path ("" when not found).
    pub path: String,
    /// An executable was resolved (override / search dirs).
    pub found: bool,
    /// The version probe exited 0.
    pub healthy: bool,
    /// First line of the version output ("" when unknown).
    pub version: String,
    /// Failure detail (not found, or the probe's error/output).
    pub detail: String,
}

/// Resolves and probes tools. `search_dirs` are the directories scanned for each tool (typically the
/// supervisor's known-good PATH dirs); `overrides` maps a tool name to an explicit path chosen via the
/// UI's file picker, which wins over the search dirs. Mirrors Go `toolcheck.Prober`.
pub struct Prober {
    pub search_dirs: Vec<String>,
    pub overrides: HashMap<String, String>,
    /// Per-tool probe timeout; [`Duration::ZERO`] means the 5s default.
    pub timeout: Duration,
}

impl Prober {
    /// Probes every tool and returns results in the same order. Mirrors `Probe`.
    pub async fn probe(&self, tools: &[Tool]) -> Vec<ToolResult> {
        let mut out = Vec::with_capacity(tools.len());
        for tool in tools {
            out.push(self.probe_one(tool).await);
        }
        out
    }

    /// Resolves a single tool (override first, then `search_dirs`), runs its version probe, and reports
    /// the outcome. Mirrors `ProbeOne`.
    ///
    /// Each tool gets its OWN fresh timeout: probes run sequentially, and a slow early tool must not
    /// starve a later, healthy one. Go achieves this by detaching the caller's deadline
    /// (`context.WithoutCancel`) and giving each probe a fresh `context.WithTimeout`; the Rust port
    /// gives each probe its own `tokio::time::timeout` by construction, so no shared shrinking deadline
    /// exists to leak across tools in the first place.
    pub async fn probe_one(&self, tool: &Tool) -> ToolResult {
        let mut res = ToolResult {
            name: tool.name.to_string(),
            path: String::new(),
            found: false,
            healthy: false,
            version: String::new(),
            detail: String::new(),
        };
        let path = match self.resolve(tool.name) {
            Some(p) => p,
            None => {
                res.detail = "not found on PATH (set an override)".to_string();
                return res;
            }
        };
        res.found = true;
        res.path.clone_from(&path);

        let timeout = if self.timeout.is_zero() {
            DEFAULT_TIMEOUT
        } else {
            self.timeout
        };
        let mut cmd = tokio::process::Command::new(&path);
        cmd.args(tool.version_args).kill_on_drop(true);
        let output = match tokio::time::timeout(timeout, cmd.output()).await {
            Ok(Ok(output)) => output,
            // The process could not be spawned/run (rare — resolve already checked the exec bit).
            Ok(Err(err)) => {
                res.detail = err.to_string();
                return res;
            }
            // The probe exceeded this tool's fresh timeout; kill_on_drop reaps the child.
            Err(_) => {
                res.detail = format!("version probe timed out after {timeout:?}");
                return res;
            }
        };

        // Go uses CombinedOutput (stdout+stderr); most CLIs print --version to stdout, so append
        // stderr after stdout to catch the few that use it.
        let mut combined = output.stdout;
        combined.extend_from_slice(&output.stderr);
        let combined = String::from_utf8_lossy(&combined);
        res.version = first_line(&combined);
        if !output.status.success() {
            res.detail = combined.trim().to_string();
            if res.detail.is_empty() {
                res.detail = format!("version probe failed ({})", output.status);
            }
            return res;
        }
        res.healthy = true;
        res
    }

    /// Returns the executable path for `name`: an override (when executable) wins, else the first
    /// executable match across `search_dirs`. Mirrors `resolve` (an empty/non-executable override
    /// falls through, since [`is_executable_file`] is false for it).
    fn resolve(&self, name: &str) -> Option<String> {
        if let Some(override_path) = self.overrides.get(name)
            && is_executable_file(Path::new(override_path))
        {
            return Some(override_path.clone());
        }
        for dir in &self.search_dirs {
            let candidate = Path::new(dir).join(name);
            if is_executable_file(&candidate) {
                return Some(candidate.to_string_lossy().into_owned());
            }
        }
        None
    }
}

/// Returns the first non-empty line of `s`, trimmed (the version line). Mirrors Go `firstLine`.
fn first_line(s: &str) -> String {
    let s = s.trim();
    match s.split_once('\n') {
        Some((first, _)) => first.trim().to_string(),
        None => s.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_dir() -> PathBuf {
        let n = DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("rhapsody-d4-tool-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&p).expect("create temp dir");
        p
    }

    /// Writes an executable shell stub that prints `output` and exits `code`. Mirror of `writeFakeTool`.
    fn write_fake_tool(dir: &Path, name: &str, output: &str, code: i32) -> PathBuf {
        write_tool_script(
            dir,
            name,
            &format!("#!/bin/sh\necho \"{output}\"\nexit {code}\n"),
        )
    }

    /// Writes an executable stub that sleeps `sleep_secs` before printing `output` and exiting `code`
    /// (to make a probe exhaust a deadline). Mirror of `writeSleepingTool`.
    fn write_sleeping_tool(
        dir: &Path,
        name: &str,
        sleep_secs: &str,
        output: &str,
        code: i32,
    ) -> PathBuf {
        write_tool_script(
            dir,
            name,
            &format!("#!/bin/sh\nsleep {sleep_secs}\necho \"{output}\"\nexit {code}\n"),
        )
    }

    fn write_tool_script(dir: &Path, name: &str, script: &str) -> PathBuf {
        std::fs::create_dir_all(dir).expect("mkdir");
        let path = dir.join(name);
        std::fs::write(&path, script).expect("write stub");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        path
    }

    fn result_by_name<'a>(rs: &'a [ToolResult], name: &str) -> &'a ToolResult {
        rs.iter().find(|r| r.name == name).expect("result present")
    }

    fn prober(search_dirs: Vec<String>, overrides: HashMap<String, String>) -> Prober {
        Prober {
            search_dirs,
            overrides,
            timeout: Duration::ZERO,
        }
    }

    // Mirrors TestProbeDetectsPresenceVersionAndHealth: a present tool is found with its version +
    // healthy; an absent tool is reported missing; a present-but-failing tool is found but unhealthy
    // with detail.
    #[tokio::test]
    async fn probe_detects_presence_version_and_health() {
        let dir = temp_dir();
        write_fake_tool(&dir, "claude", "1.2.3 (Claude Code)", 0);
        write_fake_tool(&dir, "git", "git version 2.40.0", 0);
        write_fake_tool(&dir, "gt", "broken", 1); // present but exits non-zero (unhealthy)
        // gh is intentionally absent.

        let p = prober(vec![dir.to_string_lossy().into_owned()], HashMap::new());
        let rs = p.probe(&default_tools()).await;

        let claude = result_by_name(&rs, "claude");
        assert!(
            claude.found && claude.healthy,
            "claude = {claude:?}; want found+healthy"
        );
        assert_eq!(claude.path, dir.join("claude").to_string_lossy());
        assert!(
            !claude.version.is_empty(),
            "claude.version empty; want the parsed version line"
        );

        let gh = result_by_name(&rs, "gh");
        assert!(
            !gh.found && !gh.healthy,
            "gh = {gh:?}; want not found (absent from PATH)"
        );

        let gt = result_by_name(&rs, "gt");
        assert!(
            gt.found && !gt.healthy,
            "gt = {gt:?}; want found but unhealthy (non-zero exit)"
        );
        assert!(
            !gt.detail.is_empty(),
            "gt.detail empty; want a failure detail for an unhealthy tool"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    // Mirrors TestProbeHonorsPerToolOverride: an explicit override path is used even when the tool is
    // not on the search dirs (the file-picker override from the UI).
    #[tokio::test]
    async fn probe_honors_per_tool_override() {
        let search_dir = temp_dir();
        let other_dir = temp_dir();
        let gh_path = write_fake_tool(&other_dir, "gh", "gh version 2.50.0", 0);

        let mut overrides = HashMap::new();
        overrides.insert("gh".to_string(), gh_path.to_string_lossy().into_owned());
        let p = prober(vec![search_dir.to_string_lossy().into_owned()], overrides);
        let rs = p.probe(&default_tools()).await;

        let gh = result_by_name(&rs, "gh");
        assert!(
            gh.found && gh.healthy && gh.path == gh_path.to_string_lossy(),
            "gh via override = {gh:?}; want found+healthy at {gh_path:?}"
        );

        std::fs::remove_dir_all(&search_dir).ok();
        std::fs::remove_dir_all(&other_dir).ok();
    }

    // Mirrors TestProbeIndependentPerToolTimeout: a slow earlier tool that would exhaust a shared
    // deadline must not starve a later, fast, healthy tool. (DefaultTools order is claude, gh, gt, git —
    // claude runs first and sleeps; git is probed last and must stay healthy under its own fresh timeout.)
    #[tokio::test]
    async fn probe_independent_per_tool_timeout() {
        let dir = temp_dir();
        write_sleeping_tool(&dir, "claude", "0.6", "1.0.0", 0); // sleeps, but under the 5s per-tool budget
        write_fake_tool(&dir, "git", "git version 2.40.0", 0); // instant + healthy
        // gh and gt are intentionally absent (resolve to not-found instantly).

        let p = prober(vec![dir.to_string_lossy().into_owned()], HashMap::new()); // 5s per-tool default
        let rs = p.probe(&default_tools()).await;

        let git = result_by_name(&rs, "git");
        assert!(
            git.found && git.healthy,
            "git = {git:?}; want found+healthy — a slow earlier tool must not starve it of its own timeout"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    // Mirrors TestDefaultToolsCoversTheFour: the daemon's four runtime CLIs are probed.
    #[test]
    fn default_tools_covers_the_four() {
        let names: Vec<&str> = default_tools().iter().map(|t| t.name).collect();
        for want in ["claude", "gh", "gt", "git"] {
            assert!(names.contains(&want), "default_tools missing {want}");
        }
    }
}
