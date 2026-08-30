//! CLI argument construction for the claude backend — parity port of Go `args.go`.
//!
//! [`Config`] is the backend's configuration (the mirror of Go's exported `claude.Config`), and
//! [`build_args`] assembles the per-turn `claude` flag vector. Argv order is a byte-compatible
//! contract: `args_test.go` asserts the FULL vector, and operator `extra_args` must win last.

use std::time::Duration;

use crate::AgentError;

/// Configures the Claude backend (extension of upstream §5.3; design-spec §6.1). The zero value
/// mirrors Go's `Config{}`; the config layer supplies defaults (e.g. `command = "claude"`).
///
/// Go's `Logger *slog.Logger` field is intentionally dropped: the port logs via `tracing` (the
/// workspace convention, as in `rhapsody-tracker`) rather than a stored logger handle.
#[derive(Debug, Clone, Default)]
pub struct Config {
    /// default `"claude"`; shell-split into name+args by [`split_command`]
    pub command: String,
    pub model: String,
    /// `claude --effort` (low|medium|high|xhigh|max); omitted when empty
    pub effort: String,
    /// default `"bypassPermissions"`
    pub permission_mode: String,
    pub allowed_tools: String,
    /// `claude --disallowedTools`; omitted when empty
    pub disallowed_tools: String,
    /// path or inline JSON; includes the Linear MCP by default
    pub mcp_config: String,
    /// `claude --setting-sources` (e.g. `project`); omitted when empty
    pub setting_sources: String,
    /// `claude --add-dir`, repeated once per entry
    pub add_dirs: Vec<String>,
    /// absolute; used for the launch containment invariant
    pub workspace_root: String,
    pub turn_timeout: Duration,
    /// extra CLI args passed verbatim to the claude process (win last)
    pub extra_args: Vec<String>,
    /// An absent knob (`None`) defaults to enabled (`true`): the child runs with billing env vars
    /// scrubbed and every system/init must report `apiKeySource == "none"`. Explicit `Some(false)`
    /// disables both (API-billing escape hatch). The tracker-credential scrub is decoupled and
    /// always applied (see [`Config::tracker_api_key`]).
    pub billing_guard: Option<bool>,
    /// Enables Claude Code's "ultracode" setting. When `true`, [`build_args`] appends the managed
    /// flag `--settings {"ultracode":true}` among the other managed flags (before `extra_args`, so
    /// an operator can still override it last). Default `false` (omitted).
    pub ultracode: bool,
    /// The resolved tracker (Linear) credential. It is scrubbed from the child's environment by
    /// VALUE (in addition to `LINEAR_API_KEY` by name) so a tracker key supplied under a
    /// custom/non-standard env var name is still withheld from the agent (design §15.5). This scrub
    /// is ALWAYS applied, independent of [`Config::billing_guard`].
    pub tracker_api_key: String,

    /// MCP injection (INF-473). When `true`, the runner MERGES a `symphony` MCP server into
    /// [`Config::mcp_config`] (writing a per-workspace `.symphony-mcp.json` and pointing the
    /// session's `mcp_config` at it) so the dispatched agent can query run/daemon state. The merge
    /// preserves the operator's servers because `--mcp-config` implies `--strict-mcp-config`.
    pub inject_mcp: bool,
    /// Absolute path to the running daemon binary, used as the injected server's `command`.
    pub daemon_bin: String,
    /// Passed as `symphony mcp <workflow_path>` so the child resolves the SAME workflow (and thus
    /// the daemon's server port).
    pub workflow_path: String,
}

/// Whitespace-splits the configured command into name + args. Tokens must not contain quoted spaces
/// (operator-controlled, trusted config). An empty command is [`AgentError::InvalidCommand`].
pub fn split_command(command: &str) -> Result<(String, Vec<String>), AgentError> {
    let fields: Vec<&str> = command.split_whitespace().collect();
    match fields.split_first() {
        Some((name, args)) => Ok((
            (*name).to_string(),
            args.iter().map(|s| (*s).to_string()).collect(),
        )),
        None => Err(AgentError::InvalidCommand),
    }
}

/// Builds the per-turn claude flags. `resume_id` is the thread/session id to resume; empty on the
/// first turn (upstream §10.2). Managed flags come first, `cfg.extra_args` last (operators win).
pub fn build_args(cfg: &Config, resume_id: &str) -> Vec<String> {
    let pm = if cfg.permission_mode.is_empty() {
        "bypassPermissions"
    } else {
        cfg.permission_mode.as_str()
    };
    let mut args = vec![
        "-p".to_string(),
        "--output-format".to_string(),
        "stream-json".to_string(),
        // stream-json INPUT keeps stdin open as a JSONL channel so operator messages can be folded
        // into a live turn at the next step boundary (INF-250). The initial prompt is sent as one
        // user message; with no operator messages a turn behaves as before.
        "--input-format".to_string(),
        "stream-json".to_string(),
        "--verbose".to_string(), // required by claude for stream-json in print mode
        "--permission-mode".to_string(),
        pm.to_string(),
    ];
    if !cfg.model.is_empty() {
        args.push("--model".to_string());
        args.push(cfg.model.clone());
    }
    if !cfg.effort.is_empty() {
        args.push("--effort".to_string());
        args.push(cfg.effort.clone());
    }
    if !cfg.allowed_tools.is_empty() {
        args.push("--allowedTools".to_string());
        args.push(cfg.allowed_tools.clone());
    }
    if !cfg.disallowed_tools.is_empty() {
        args.push("--disallowedTools".to_string());
        args.push(cfg.disallowed_tools.clone());
    }
    if !cfg.mcp_config.is_empty() {
        // --mcp-config implies --strict-mcp-config so the operator's interactive MCP set never
        // perturbs a headless run (mirrors the reference argv.go).
        args.push("--mcp-config".to_string());
        args.push(cfg.mcp_config.clone());
        args.push("--strict-mcp-config".to_string());
    }
    if !cfg.setting_sources.is_empty() {
        args.push("--setting-sources".to_string());
        args.push(cfg.setting_sources.clone());
    }
    for d in &cfg.add_dirs {
        args.push("--add-dir".to_string());
        args.push(d.clone());
    }
    if !resume_id.is_empty() {
        args.push("--resume".to_string());
        args.push(resume_id.to_string());
    }
    if cfg.ultracode {
        // Managed flag: enable Claude Code's ultracode setting. Placed among the managed flags
        // (before extra_args) so an operator's extra_args still win last.
        args.push("--settings".to_string());
        args.push(r#"{"ultracode":true}"#.to_string());
    }
    // extra_args are appended LAST so operators can override any managed flag above.
    args.extend(cfg.extra_args.iter().cloned());
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    // Mirrors Go `claude.TestSplitCommand` (args_test.go).
    #[test]
    fn split_command_cases() {
        let (name, args) = split_command("claude").expect("claude splits");
        assert_eq!(name, "claude");
        assert!(args.is_empty());

        let (name, args) = split_command("bash /tmp/fake.sh").expect("splits");
        assert_eq!(name, "bash");
        assert_eq!(args, vec!["/tmp/fake.sh".to_string()]);

        assert_eq!(split_command("   "), Err(AgentError::InvalidCommand));
    }

    // Mirrors Go `claude.TestBuildArgsFirstTurn`: the FULL argv is asserted (order is the contract).
    #[test]
    fn build_args_first_turn() {
        let got = build_args(
            &Config {
                permission_mode: "bypassPermissions".to_string(),
                model: "opus".to_string(),
                mcp_config: "/cfg/mcp.json".to_string(),
                ..Default::default()
            },
            "",
        );
        let want = vec![
            "-p",
            "--output-format",
            "stream-json",
            "--input-format",
            "stream-json",
            "--verbose",
            "--permission-mode",
            "bypassPermissions",
            "--model",
            "opus",
            "--mcp-config",
            "/cfg/mcp.json",
            "--strict-mcp-config",
        ];
        assert_eq!(got, want);
    }

    // Mirrors Go `claude.TestBuildArgsDefaultsPermissionMode`.
    #[test]
    fn build_args_defaults_permission_mode() {
        let got = build_args(&Config::default(), "");
        let idx = got
            .iter()
            .position(|a| a == "--permission-mode")
            .expect("permission-mode flag present");
        assert_eq!(got[idx + 1], "bypassPermissions");
    }

    // Mirrors Go `claude.TestBuildArgsContinuationAddsResume`.
    #[test]
    fn build_args_continuation_adds_resume() {
        let got = build_args(&Config::default(), "sess-xyz");
        let idx = got
            .iter()
            .position(|a| a == "--resume")
            .expect("--resume present");
        assert_eq!(got[idx + 1], "sess-xyz");
    }

    // Mirrors Go `claude.TestBuildArgsExtraArgs`.
    #[test]
    fn build_args_extra_args() {
        let got = build_args(
            &Config {
                extra_args: vec![
                    "--settings".to_string(),
                    r#"{"ultracode":true}"#.to_string(),
                ],
                ..Default::default()
            },
            "",
        );
        let i = got
            .iter()
            .position(|a| a == "--settings")
            .expect("--settings present");
        assert_eq!(got[i + 1], r#"{"ultracode":true}"#);
    }

    // Mirrors Go `claude.TestBuildArgsUltracode`.
    #[test]
    fn build_args_ultracode() {
        let got = build_args(
            &Config {
                ultracode: true,
                ..Default::default()
            },
            "",
        );
        let si = got
            .iter()
            .position(|a| a == "--settings")
            .expect("--settings present");
        assert_eq!(got[si + 1], r#"{"ultracode":true}"#);
        // Omitted entirely when ultracode is false.
        assert!(
            !build_args(&Config::default(), "").contains(&"--settings".to_string()),
            "--settings must be omitted when ultracode is false"
        );
    }

    // Mirrors Go `claude.TestBuildArgsUltracodeBeforeExtraArgs`: the managed ultracode flag precedes
    // operator extra_args so an operator can still override it last.
    #[test]
    fn build_args_ultracode_before_extra_args() {
        let got = build_args(
            &Config {
                ultracode: true,
                extra_args: vec![
                    "--settings".to_string(),
                    r#"{"ultracode":false}"#.to_string(),
                ],
                ..Default::default()
            },
            "",
        );
        let managed = got
            .iter()
            .position(|a| a == "--settings")
            .expect("managed --settings present");
        assert_eq!(got[managed + 1], r#"{"ultracode":true}"#);
        // The operator override is the FINAL pair (extra_args win last).
        let n = got.len();
        assert_eq!(got[n - 2], "--settings");
        assert_eq!(got[n - 1], r#"{"ultracode":false}"#);
    }

    // Mirrors Go `claude.TestBuildArgsAllowedTools`.
    #[test]
    fn build_args_allowed_tools() {
        let got = build_args(
            &Config {
                allowed_tools: "Bash,Read,Edit".to_string(),
                ..Default::default()
            },
            "",
        );
        let idx = got
            .iter()
            .position(|a| a == "--allowedTools")
            .expect("--allowedTools present");
        assert_eq!(got[idx + 1], "Bash,Read,Edit");
    }

    // Mirrors Go `claude.TestBuildArgsEffort`.
    #[test]
    fn build_args_effort() {
        let got = build_args(
            &Config {
                effort: "xhigh".to_string(),
                ..Default::default()
            },
            "",
        );
        let idx = got
            .iter()
            .position(|a| a == "--effort")
            .expect("--effort present");
        assert_eq!(got[idx + 1], "xhigh");
        // Empty effort omits the flag entirely.
        assert!(!build_args(&Config::default(), "").contains(&"--effort".to_string()));
    }

    // Mirrors Go `claude.TestBuildArgsDisallowedTools`.
    #[test]
    fn build_args_disallowed_tools() {
        let got = build_args(
            &Config {
                disallowed_tools: "WebFetch,Bash".to_string(),
                ..Default::default()
            },
            "",
        );
        let idx = got
            .iter()
            .position(|a| a == "--disallowedTools")
            .expect("--disallowedTools present");
        assert_eq!(got[idx + 1], "WebFetch,Bash");
        assert!(!build_args(&Config::default(), "").contains(&"--disallowedTools".to_string()));
    }

    // Mirrors Go `claude.TestBuildArgsSettingSources`.
    #[test]
    fn build_args_setting_sources() {
        let got = build_args(
            &Config {
                setting_sources: "project".to_string(),
                ..Default::default()
            },
            "",
        );
        let idx = got
            .iter()
            .position(|a| a == "--setting-sources")
            .expect("--setting-sources present");
        assert_eq!(got[idx + 1], "project");
        assert!(!build_args(&Config::default(), "").contains(&"--setting-sources".to_string()));
    }

    // Mirrors Go `claude.TestBuildArgsAddDirsRepeated`: each entry produces a separate
    // `--add-dir <dir>` pair, in order.
    #[test]
    fn build_args_add_dirs_repeated() {
        let got = build_args(
            &Config {
                add_dirs: vec!["/a".to_string(), "/b".to_string(), "/c".to_string()],
                ..Default::default()
            },
            "",
        );
        let mut dirs = Vec::new();
        for i in 0..got.len().saturating_sub(1) {
            if got[i] == "--add-dir" {
                dirs.push(got[i + 1].clone());
            }
        }
        assert_eq!(dirs, vec!["/a", "/b", "/c"]);
        assert!(!build_args(&Config::default(), "").contains(&"--add-dir".to_string()));
    }

    // Mirrors Go `claude.TestBuildArgsMCPConfigAutoStrict`: `--strict-mcp-config` follows
    // `--mcp-config <path>` immediately, and neither appears without a config.
    #[test]
    fn build_args_mcp_config_auto_strict() {
        let got = build_args(
            &Config {
                mcp_config: "/cfg/mcp.json".to_string(),
                ..Default::default()
            },
            "",
        );
        let mi = got
            .iter()
            .position(|a| a == "--mcp-config")
            .expect("--mcp-config present");
        assert_eq!(got[mi + 1], "/cfg/mcp.json");
        let si = got
            .iter()
            .position(|a| a == "--strict-mcp-config")
            .expect("--strict-mcp-config present");
        assert_eq!(si, mi + 2);
        assert!(!build_args(&Config::default(), "").contains(&"--strict-mcp-config".to_string()));
    }

    // Mirrors Go `claude.TestBuildArgsExtraArgsLast`: every managed flag precedes the extra_args
    // block, and the extra_args tokens are the final elements.
    #[test]
    fn build_args_extra_args_last() {
        let got = build_args(
            &Config {
                model: "opus".to_string(),
                effort: "high".to_string(),
                allowed_tools: "Bash".to_string(),
                disallowed_tools: "WebFetch".to_string(),
                mcp_config: "/cfg/mcp.json".to_string(),
                setting_sources: "project".to_string(),
                add_dirs: vec!["/a".to_string()],
                extra_args: vec![
                    "--settings".to_string(),
                    r#"{"ultracode":true}"#.to_string(),
                ],
                ..Default::default()
            },
            "",
        );
        let si = got
            .iter()
            .position(|a| a == "--settings")
            .expect("extra args present");
        for flag in [
            "--effort",
            "--disallowedTools",
            "--setting-sources",
            "--add-dir",
            "--strict-mcp-config",
            "--allowedTools",
            "--mcp-config",
            "--model",
        ] {
            if let Some(fi) = got.iter().position(|a| a == flag) {
                assert!(
                    fi < si,
                    "{flag} (idx {fi}) must come before extra_args --settings (idx {si}): {got:?}"
                );
            }
        }
        let n = got.len();
        assert_eq!(got[n - 2], "--settings");
        assert_eq!(got[n - 1], r#"{"ultracode":true}"#);
    }
}
