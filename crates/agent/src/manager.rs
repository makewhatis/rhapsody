//! manager — the manager run's isolated startup posture (STUDIO-1014;
//! design record `~/.rhapsody/docs/manager-agent-design.md` §4.2–§4.4, §4.7, §10.1).
//!
//! A manager run gets **no filesystem, no shell, no network and no repository checkout**: every
//! repository read is served by the host (§4.4). This module owns the pure half of that boundary —
//! the exact `claude` argv posture, the manager-only MCP config, and the dedicated configuration
//! directory env — so the contract can be asserted without a real model or the installed CLI. The
//! impure half (spawning the canary that verifies the installed CLI honours it) lives on the
//! orchestrator side (§4.7).
//!
//! # The three things a caller must not get wrong
//!
//! * **`--permission-mode default`**, never the install's `bypassPermissions` (§4.3). Inheriting
//!   `bypassPermissions` would make the deny-list advisory rather than enforced.
//! * **`--mcp-config` and `--strict-mcp-config` passed together, explicitly** (§4.2). Never rely on
//!   one implying the other.
//! * **`--allowedTools` lists only the manager MCP tools**, and `--disallowedTools` names every
//!   built-in (§4.3). The allowlist is the enforcement; the deny-list is belt-and-braces.
//!
//! Operator `extra_args` are deliberately NOT inherited: they are appended last by
//! [`build_args`](crate::claude::build_args) precisely so an operator can override a managed flag,
//! which on a manager run would be a way to walk the boundary back (e.g. re-add
//! `--permission-mode bypassPermissions`). The manager posture is daemon-controlled.

use crate::claude::{Config, build_args};

/// The role the manager run's MCP server is started with (§4.4). Passed as `--role manager` to
/// `rhapsodyd mcp`; the facade uses it to register the fixed manager tool set and nothing else.
pub const MANAGER_ROLE: &str = "manager";

/// The MCP server name the daemon injects under (the `"symphony"` key, §4.2). It fixes the agent's
/// tool namespace (`mcp__symphony__*`), so it is a live contract, not a cosmetic label.
pub const MANAGER_MCP_SERVER: &str = "symphony";

/// The manager configuration directory env var (§4.2): a daemon-owned directory containing only the
/// model credential, so none of the operator's user-level hooks, plugins, MCP servers or permission
/// rules load. Claude Code reads this variable to relocate its config root.
pub const MANAGER_CONFIG_DIR_ENV: &str = "CLAUDE_CONFIG_DIR";

/// `--setting-sources user` (§4.2): load only the USER settings source, which — because
/// [`MANAGER_CONFIG_DIR_ENV`] points at the dedicated directory — is the daemon's own. This is what
/// EXCLUDES the project and local sources (the run's cwd is empty anyway, but the flag is the
/// contract; §4.7's self-test verifies the CLI honours it).
pub const MANAGER_SETTING_SOURCES: &str = "user";

/// Every MCP tool a manager run may call (§4.4). This is exactly [`MANAGER_MCP_TOOLS`]; a tool
/// absent from this list is not registered by the manager role and is rejected on call.
///
/// Note `teams_retain` is the **only** write. It writes an observation to the manager's own bank.
pub const MANAGER_MCP_TOOLS: &[&str] = &[
    // Existing reads (§4.4).
    "symphony_state",
    "symphony_runs",
    "symphony_run",
    "symphony_run_status",
    "symphony_events",
    "symphony_logs",
    "symphony_ticket",
    "teams_recall",
    "teams_room_read",
    "teams_roster",
    // The one registered write: its own bank, observations only (§3.2, §4.4).
    "teams_retain",
    // Host-served reads (§4.4): PR metadata via the host's `gh`, and repository reads from git
    // objects in the bare mirror.
    "manager_pr",
    "manager_pr_activity",
    "manager_pr_commits",
    "manager_file",
    "manager_ls",
    "manager_grep",
    "manager_diff",
    "manager_interdiff",
    "manager_patch_id",
    "manager_findings",
];

/// Every built-in the pinned CLI exposes, named in `--disallowedTools` (§4.3). The allowlist is the
/// actual enforcement; this list is the explicit denial the design requires, kept generous so a
/// newly-exposed built-in is denied by NAME as well as by the allowlist.
pub const MANAGER_DISALLOWED_BUILTIN_TOOLS: &[&str] = &[
    "Bash",
    "BashOutput",
    "KillShell",
    "Read",
    "Grep",
    "Glob",
    "Edit",
    "MultiEdit",
    "Write",
    "NotebookEdit",
    "WebFetch",
    "WebSearch",
    "Task",
    "TodoWrite",
    "SlashCommand",
    "ExitPlanMode",
    "AskUserQuestion",
];

/// The fully-qualified (permission-rule) spelling of every manager MCP tool:
/// `mcp__<server>__<tool>`. This is the value `--allowedTools` carries.
pub fn manager_allowed_tools() -> String {
    MANAGER_MCP_TOOLS
        .iter()
        .map(|t| format!("mcp__{MANAGER_MCP_SERVER}__{t}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// The comma-joined `--disallowedTools` value.
pub fn manager_disallowed_tools() -> String {
    MANAGER_DISALLOWED_BUILTIN_TOOLS.join(",")
}

/// The manager-only MCP config document (§4.2): a single server — the daemon — started with the
/// `manager` role. Unlike the ordinary injected config it does NOT merge the operator's servers;
/// `--strict-mcp-config` then guarantees no inherited server can contribute a tool.
///
/// `workflow_path` is absolutized so the child resolves the SAME workflow (and daemon port)
/// regardless of the run's cwd; the role flag precedes it.
pub fn manager_mcp_config(daemon_bin: &str, workflow_path: &str) -> String {
    let mut args = vec![
        serde_json::Value::from("mcp"),
        serde_json::Value::from("--role"),
        serde_json::Value::from(MANAGER_ROLE),
    ];
    if !workflow_path.is_empty() {
        let abs = std::path::absolute(workflow_path)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| workflow_path.to_string());
        args.push(serde_json::Value::from(abs));
    }
    let doc = serde_json::json!({
        "mcpServers": {
            MANAGER_MCP_SERVER: {
                "command": daemon_bin,
                "args": args,
                "env": {},
            }
        }
    });
    serde_json::to_string_pretty(&doc).unwrap_or_else(|_| "{}".to_string())
}

/// The manager's `claude` argv posture, derived from a base config (which supplies the command,
/// model and effort from M6's config) but with every security-relevant field overridden:
///
/// * `permission_mode` = `default` (§4.3),
/// * `allowed_tools` = the manager MCP tools only (§4.3),
/// * `disallowed_tools` = every built-in (§4.3),
/// * `mcp_config` = the manager-only file, so `build_args` emits `--mcp-config` AND
///   `--strict-mcp-config` (§4.2),
/// * `setting_sources` = `user` (§4.2),
/// * `extra_args` = empty (see the module doc: operator overrides would be a boundary escape),
/// * no `add_dirs` (the run has no directory to add).
pub fn manager_args(base: &Config, mcp_config_path: &str) -> Vec<String> {
    let cfg = manager_config(base, mcp_config_path);
    build_args(&cfg, "")
}

/// The manager's full `claude` [`Config`] posture, from which [`manager_args`] builds the argv. The
/// command/model/effort are inherited from `base` (M6's config); everything security-relevant is
/// overridden.
pub fn manager_config(base: &Config, mcp_config_path: &str) -> Config {
    Config {
        command: base.command.clone(),
        model: base.model.clone(),
        effort: base.effort.clone(),
        permission_mode: "default".to_string(),
        allowed_tools: manager_allowed_tools(),
        disallowed_tools: manager_disallowed_tools(),
        mcp_config: mcp_config_path.to_string(),
        setting_sources: MANAGER_SETTING_SOURCES.to_string(),
        add_dirs: Vec::new(),
        workspace_root: base.workspace_root.clone(),
        turn_timeout: base.turn_timeout,
        extra_args: Vec::new(),
        billing_guard: base.billing_guard,
        ultracode: false,
        tracker_api_key: base.tracker_api_key.clone(),
        inject_mcp: false,
        daemon_bin: base.daemon_bin.clone(),
        workflow_path: base.workflow_path.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arg_after<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
        let i = args.iter().position(|a| a == flag)?;
        args.get(i + 1).map(String::as_str)
    }

    // §4.3: the manager never inherits the install's `bypassPermissions`.
    #[test]
    fn manager_permission_mode_is_default() {
        let base = Config {
            permission_mode: "bypassPermissions".to_string(),
            ..Default::default()
        };
        let args = manager_args(&base, "/run/manager-mcp.json");
        assert_eq!(arg_after(&args, "--permission-mode"), Some("default"));
        assert!(
            !args.iter().any(|a| a == "bypassPermissions"),
            "bypassPermissions must never survive into a manager argv: {args:?}"
        );
    }

    // §4.2: both MCP flags are passed explicitly, adjacent, in the managed block.
    #[test]
    fn manager_passes_mcp_config_and_strict_explicitly() {
        let args = manager_args(&Config::default(), "/run/manager-mcp.json");
        let mi = args
            .iter()
            .position(|a| a == "--mcp-config")
            .expect("--mcp-config present");
        assert_eq!(args[mi + 1], "/run/manager-mcp.json");
        assert_eq!(
            args[mi + 2], "--strict-mcp-config",
            "the strict flag must immediately follow the config: {args:?}"
        );
    }

    // §4.3: the allowlist is exactly the manager MCP tools, fully qualified.
    #[test]
    fn manager_allowlist_is_only_manager_tools() {
        let args = manager_args(&Config::default(), "/m.json");
        let allowed = arg_after(&args, "--allowedTools").expect("--allowedTools present");
        let names: Vec<&str> = allowed.split(',').collect();
        assert_eq!(names.len(), MANAGER_MCP_TOOLS.len());
        for t in MANAGER_MCP_TOOLS {
            assert!(
                names.contains(&format!("mcp__symphony__{t}").as_str()),
                "manager tool {t} missing from allowlist: {allowed}"
            );
        }
        // No built-in can appear in the allowlist.
        for b in MANAGER_DISALLOWED_BUILTIN_TOOLS {
            assert!(
                !names.contains(b),
                "built-in {b} must not be allowed for the manager"
            );
        }
        // The write tools the design says are NOT registered must not be allowed.
        for forbidden in [
            "symphony_send_message",
            "symphony_stop",
            "symphony_resume",
            "symphony_handoff",
            "teams_post",
            "teams_invalidate",
            "teams_reinstate",
        ] {
            assert!(
                !names.contains(&format!("mcp__symphony__{forbidden}").as_str()),
                "unregistered write tool {forbidden} must not be allowed"
            );
        }
    }

    // §4.3: every named built-in is denied.
    #[test]
    fn manager_denies_every_builtin() {
        let args = manager_args(&Config::default(), "/m.json");
        let denied = arg_after(&args, "--disallowedTools").expect("--disallowedTools present");
        let names: Vec<&str> = denied.split(',').collect();
        for want in [
            "Bash",
            "Read",
            "Grep",
            "Glob",
            "Edit",
            "Write",
            "NotebookEdit",
            "WebFetch",
            "WebSearch",
            "Task",
        ] {
            assert!(
                names.contains(&want),
                "built-in {want} missing from disallowedTools: {denied}"
            );
        }
    }

    // §4.2: project and local settings sources are excluded.
    #[test]
    fn manager_setting_sources_excludes_project_and_local() {
        let base = Config {
            setting_sources: "project,local".to_string(),
            ..Default::default()
        };
        let args = manager_args(&base, "/m.json");
        let sources = arg_after(&args, "--setting-sources").expect("--setting-sources present");
        assert_eq!(sources, "user");
        assert!(!sources.contains("project") && !sources.contains("local"));
    }

    // The module doc's dangerous inheritance: operator extra_args must not survive.
    #[test]
    fn manager_drops_operator_extra_args() {
        let base = Config {
            extra_args: vec![
                "--permission-mode".to_string(),
                "bypassPermissions".to_string(),
            ],
            ..Default::default()
        };
        let args = manager_args(&base, "/m.json");
        assert!(
            !args.iter().any(|a| a == "bypassPermissions"),
            "operator extra_args must be dropped for a manager run: {args:?}"
        );
    }

    // The manager-only MCP document carries exactly one server, started with the manager role.
    #[test]
    fn manager_mcp_config_single_server_with_role() {
        let raw = manager_mcp_config("/usr/bin/rhapsodyd", "/repo/WORKFLOW.md");
        let v: serde_json::Value = serde_json::from_str(&raw).expect("valid json");
        let servers = v.get("mcpServers").expect("mcpServers").as_object().unwrap();
        assert_eq!(servers.len(), 1, "manager config must not merge operator servers");
        let entry = servers.get("symphony").expect("symphony server");
        assert_eq!(entry["command"], "/usr/bin/rhapsodyd");
        let args: Vec<String> = entry["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_str().unwrap().to_string())
            .collect();
        assert_eq!(args[0], "mcp");
        assert_eq!(args[1], "--role");
        assert_eq!(args[2], "manager");
        // The workflow path (when given) is absolutized and last.
        assert!(args.len() == 4 && std::path::Path::new(&args[3]).is_absolute());
    }

    // With no workflow path, the role args are still emitted (no trailing absolute path).
    #[test]
    fn manager_mcp_config_without_workflow() {
        let raw = manager_mcp_config("/usr/bin/rhapsodyd", "");
        let v: serde_json::Value = serde_json::from_str(&raw).expect("valid json");
        let args = v["mcpServers"]["symphony"]["args"].as_array().unwrap();
        assert_eq!(args.len(), 3);
    }
}
