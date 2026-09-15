//! MCP server injection and tool-name spelling for the opencode backend (STUDIO-902).
//! Rhapsody-only.
//!
//! Two jobs, both of which the claude backend also has but solves differently enough that sharing
//! code would obscure rather than help.
//!
//! ## 1. Reaching the daemon's own tools, without touching the worktree
//!
//! [`inject_daemon_mcp`] writes a config holding this daemon's `symphony` MCP server and returns
//! its path for the child's `OPENCODE_CONFIG`. It is written into the run's PRIVATE STATE
//! DIRECTORY, not into the worktree — unlike the claude backend, which writes
//! `.symphony-mcp.json` into the working directory (and which this repository's own `.gitignore`
//! has an entry for, precisely because a dispatched run working in this repo would otherwise sweep
//! it into a commit). A target repository has no such entry, and `opencode.json` is a name a real
//! project may already use and track, so injecting by rewriting a file in the worktree could put a
//! modified tracked file in front of an agent that is about to `git add -A`.
//!
//! **[RAN] `OPENCODE_CONFIG` MERGES with the project and global configs; it does not replace
//! them.** Measured against opencode 1.18.30 with `opencode mcp list`: a project `opencode.json`
//! declaring one server plus an `OPENCODE_CONFIG` file declaring another lists **both** (2
//! servers), where a replacing variable would have listed one. So the operator's own
//! `opencode.json` and their `~/.config/opencode/` config survive injection untouched, and this
//! file needs to carry only the daemon's server rather than a merge of everything.
//!
//! The server KEY stays `"symphony"`: it determines the agent's tool namespace and is a live
//! cross-process contract, exactly as it is for the claude backend (STUDIO-603). If the workspace's
//! own config already defines a server under that key, the operator's wins and nothing is injected.
//!
//! ## 2. ⚠️ Spelling the tool names the way opencode spells them
//!
//! [`rewrite_tool_names`] rewrites `mcp__<server>__<tool>` into `<server>_<tool>` in prompt text.
//!
//! This is not cosmetic. Rhapsody's shipped prompt template names tools literally — the run-ending
//! instruction is *"Call the daemon-mediated `mcp__symphony__symphony_handoff` tool"* — and the
//! STUDIO-869 spike measured that opencode spells the same tool `symphony_symphony_state`, i.e.
//! `<server>_<tool>` (`harness/harness-spike/opencode/happy.jsonl`, and the design's
//! `ToolNaming::ServerUnderscoreTool`). An unrewritten prompt therefore instructs an opencode agent
//! to call a tool that does not exist, and the specific tool it cannot call is the one that ENDS
//! ITS RUN. The failure is silent: the agent finishes its work and simply never hands off.

use std::path::{Path, PathBuf};

use crate::AgentError;

/// The file written into the run's state directory and pointed at by `OPENCODE_CONFIG`.
pub const INJECTED_CONFIG_NAME: &str = "symphony-opencode.json";

/// The MCP server key — the agent's tool namespace, and a live contract (see the module doc).
pub const SERVER_KEY: &str = "symphony";

/// The claude-side tool-name prefix that [`rewrite_tool_names`] rewrites away.
const CLAUDE_PREFIX: &str = "mcp__";

/// Writes the daemon's MCP config into `state_dir` and returns `(path, kept_operator_server)`.
///
/// Returns `Ok(None)` — inject nothing — when the workspace's own `opencode.json` already defines a
/// `symphony` server, so the operator's definition wins exactly as it does for the claude backend.
/// Any failure is an `Err`, and the caller proceeds with no injected config rather than failing the
/// run: injection never blocks a turn, which is the claude backend's rule too.
pub fn inject_daemon_mcp(
    state_dir: &Path,
    ws_path: &str,
    daemon_bin: &str,
    workflow_path: &str,
) -> Result<Option<(PathBuf, bool)>, AgentError> {
    if daemon_bin.is_empty() {
        return Err(AgentError::Other("no daemon binary path".to_string()));
    }
    if workspace_defines_symphony(ws_path) {
        return Ok(None);
    }

    let mut args = vec![serde_json::Value::from("mcp")];
    if !workflow_path.is_empty() {
        // Lexical absolutization, no existence check — the same `filepath.Abs` behaviour the claude
        // injector uses, so the child resolves the SAME workflow whatever its cwd.
        let abs = std::path::absolute(workflow_path)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| workflow_path.to_string());
        args.push(serde_json::Value::from(abs));
    }
    // opencode's own shape for a local server: ONE `command` array (argv0 first), not claude's
    // `{command, args}` pair. Taken from the config that produced every committed opencode capture,
    // `harness/harness-spike/opencode/opencode.json`.
    let mut command = vec![serde_json::Value::from(daemon_bin)];
    command.extend(args);
    let doc = serde_json::json!({
        "$schema": "https://opencode.ai/config.json",
        "mcp": { SERVER_KEY: { "type": "local", "enabled": true, "command": command } },
    });

    let out = serde_json::to_string_pretty(&doc).map_err(|e| AgentError::Other(e.to_string()))?;
    let dst = state_dir.join(INJECTED_CONFIG_NAME);
    std::fs::write(&dst, out)
        .map_err(|e| AgentError::Other(format!("write opencode mcp config: {e}")))?;
    Ok(Some((dst, false)))
}

/// Whether the workspace's own `opencode.json` already defines a `symphony` MCP server. Any read or
/// parse failure answers `false`: an unreadable operator config is not a declaration, and the
/// injected server is additive (the measured merge in the module doc) so a false negative costs a
/// duplicate key that opencode itself resolves, never a lost operator server.
fn workspace_defines_symphony(ws_path: &str) -> bool {
    for name in ["opencode.json", "opencode.jsonc"] {
        let p = Path::new(ws_path).join(name);
        let Ok(raw) = std::fs::read(&p) else { continue };
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(&raw) else {
            continue;
        };
        if v.pointer(&format!("/mcp/{SERVER_KEY}")).is_some() {
            return true;
        }
    }
    false
}

/// Rewrites claude's `mcp__<server>__<tool>` tool names into opencode's `<server>_<tool>` spelling.
///
/// Hand-rolled rather than a regex so the crate gains no dependency for one substitution. A name
/// token is `mcp__`, then a server of one or more `[A-Za-z0-9_-]` characters, then `__`, then a
/// tool of one or more of the same. Because `_` is itself a name character, the SERVER is taken as
/// the SHORTEST run ending at the first `__` that is followed by a tool character — matching how
/// claude composes the name (`mcp__` + server + `__` + tool), where the server never contains a
/// double underscore.
///
/// Anything that does not match that shape is copied through byte-for-byte, so ordinary prose
/// containing the letters `mcp` is untouched.
pub fn rewrite_tool_names(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;
    while i < b.len() {
        if !b[i..].starts_with(CLAUDE_PREFIX.as_bytes()) {
            // Advance one CHARACTER, not one byte, so multi-byte text is never split.
            let step = text[i..].chars().next().map(char::len_utf8).unwrap_or(1);
            out.push_str(&text[i..i + step]);
            i += step;
            continue;
        }
        let after_prefix = i + CLAUDE_PREFIX.len();
        match split_server_tool(&text[after_prefix..]) {
            Some((server, tool, consumed)) => {
                out.push_str(server);
                out.push('_');
                out.push_str(tool);
                i = after_prefix + consumed;
            }
            None => {
                out.push_str(CLAUDE_PREFIX);
                i = after_prefix;
            }
        }
    }
    out
}

/// Splits `"<server>__<tool>…"` into `(server, tool, bytes_consumed)`, or `None` when the text does
/// not open with that shape.
fn split_server_tool(s: &str) -> Option<(&str, &str, usize)> {
    let b = s.as_bytes();
    let name_char = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'-';

    // The server ends at the first `__` that has at least one name character after it. Scanning
    // from the left takes the SHORTEST server, which is how claude composes the name.
    let mut sep = None;
    let mut j = 0usize;
    while j + 1 < b.len() {
        if !name_char(b[j]) {
            break;
        }
        if b[j] == b'_' && b[j + 1] == b'_' && j > 0 {
            if b.get(j + 2).is_some_and(|&c| name_char(c) && c != b'_') {
                sep = Some(j);
                break;
            }
            // A `__` with nothing usable after it is not a separator; skip past it.
            j += 2;
            continue;
        }
        j += 1;
    }
    let sep = sep?;
    let server = &s[..sep];
    if server.is_empty() {
        return None;
    }

    let mut k = sep + 2;
    while k < b.len() && name_char(b[k]) {
        k += 1;
    }
    let tool = &s[sep + 2..k];
    if tool.is_empty() {
        return None;
    }
    Some((server, tool, k))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opencode::testdir::TempDir;

    #[test]
    fn writes_a_config_naming_the_daemon_and_the_workflow() {
        let tmp = TempDir::new();
        let ws = TempDir::new();
        let (path, kept) = inject_daemon_mcp(
            tmp.path(),
            &ws.path().to_string_lossy(),
            "/opt/rhapsodyd",
            "/home/op/.rhapsody/WORKFLOW.md",
        )
        .expect("inject")
        .expect("a config was written");
        assert!(!kept);
        assert_eq!(path, tmp.path().join(INJECTED_CONFIG_NAME));

        let v: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).expect("read")).expect("json");
        let cmd = v.pointer("/mcp/symphony/command").expect("command");
        assert_eq!(
            cmd,
            &serde_json::json!(["/opt/rhapsodyd", "mcp", "/home/op/.rhapsody/WORKFLOW.md"]),
            "opencode takes ONE argv array, not claude's command+args pair"
        );
        assert_eq!(
            v.pointer("/mcp/symphony/enabled"),
            Some(&serde_json::json!(true))
        );
        assert_eq!(
            v.pointer("/mcp/symphony/type"),
            Some(&serde_json::json!("local"))
        );
    }

    // The config must NOT be written into the worktree: an injected file there can be swept into a
    // commit by the very agent it was written for.
    #[test]
    fn nothing_is_written_into_the_workspace() {
        let tmp = TempDir::new();
        let ws = TempDir::new();
        inject_daemon_mcp(
            tmp.path(),
            &ws.path().to_string_lossy(),
            "/opt/rhapsodyd",
            "",
        )
        .expect("inject")
        .expect("written");
        let left: Vec<_> = std::fs::read_dir(ws.path())
            .expect("read ws")
            .filter_map(Result::ok)
            .map(|e| e.file_name())
            .collect();
        assert!(left.is_empty(), "the worktree was modified: {left:?}");
    }

    #[test]
    fn an_operator_symphony_server_wins_and_nothing_is_injected() {
        let tmp = TempDir::new();
        let ws = TempDir::new();
        std::fs::write(
            ws.path().join("opencode.json"),
            br#"{"mcp":{"symphony":{"type":"local","command":["/their/own"]}}}"#,
        )
        .expect("write");
        let got = inject_daemon_mcp(
            tmp.path(),
            &ws.path().to_string_lossy(),
            "/opt/rhapsodyd",
            "",
        )
        .expect("inject");
        assert!(got.is_none(), "the operator's own server must win");
        assert!(!tmp.path().join(INJECTED_CONFIG_NAME).exists());
    }

    #[test]
    fn an_operator_config_without_symphony_does_not_block_injection() {
        let tmp = TempDir::new();
        let ws = TempDir::new();
        std::fs::write(
            ws.path().join("opencode.json"),
            br#"{"mcp":{"theirs":{"type":"local","command":["/x"]}}}"#,
        )
        .expect("write");
        assert!(
            inject_daemon_mcp(
                tmp.path(),
                &ws.path().to_string_lossy(),
                "/opt/rhapsodyd",
                ""
            )
            .expect("inject")
            .is_some(),
            "an unrelated server must not suppress injection — OPENCODE_CONFIG merges, so both survive"
        );
    }

    #[test]
    fn an_empty_daemon_bin_is_an_error() {
        let tmp = TempDir::new();
        assert!(inject_daemon_mcp(tmp.path(), "/ws", "", "").is_err());
    }

    // ⚠️ The spelling the measured capture uses. `symphony_symphony_state` is the literal tool name
    // in `harness/harness-spike/opencode/happy.jsonl`.
    #[test]
    fn rewrites_the_measured_spelling() {
        assert_eq!(
            rewrite_tool_names("mcp__symphony__symphony_state"),
            "symphony_symphony_state"
        );
    }

    // The run-ending tool. An unrewritten prompt tells the agent to call a name opencode does not
    // have, and the run simply never hands off.
    #[test]
    fn rewrites_the_handoff_tool_inside_real_prompt_prose() {
        let prompt = "Call the daemon-mediated `mcp__symphony__symphony_handoff` tool (no \
                      arguments), or fall back to `mcp__claude_ai_Linear__save_issue`.";
        let got = rewrite_tool_names(prompt);
        assert!(got.contains("`symphony_symphony_handoff`"), "{got}");
        assert!(got.contains("`claude_ai_Linear_save_issue`"), "{got}");
        assert!(!got.contains("mcp__"), "no claude spelling survives: {got}");
    }

    // Ordinary prose must survive byte-for-byte — a prompt is mostly not tool names.
    #[test]
    fn leaves_everything_else_untouched() {
        for s in [
            "",
            "no tools here at all",
            "the mcp server is configured",
            "mcp__",
            "mcp__onlyserver",
            "mcp__server__",
            "a__b is not a tool name",
            "unicode: café ☕ mcp__x__y ✅",
        ] {
            let got = rewrite_tool_names(s);
            if s.contains("mcp__x__y") {
                assert_eq!(got, "unicode: café ☕ x_y ✅");
            } else {
                assert_eq!(got, s, "unexpected rewrite of {s:?}");
            }
        }
    }

    // A server name containing an underscore is the common case (`claude_ai_Linear`), and the tool
    // name always does. The split must land on the `__` pair, not on any single `_`.
    #[test]
    fn splits_on_the_double_underscore_not_on_single_underscores() {
        assert_eq!(
            rewrite_tool_names("mcp__claude_ai_Linear__get_issue"),
            "claude_ai_Linear_get_issue"
        );
        assert_eq!(rewrite_tool_names("mcp__a__b"), "a_b");
        assert_eq!(
            rewrite_tool_names("mcp__symphony__teams_post and mcp__symphony__teams_retain"),
            "symphony_teams_post and symphony_teams_retain"
        );
    }

    // The rewrite must be idempotent: a prompt that has already been rewritten (or was authored in
    // opencode's spelling) must not be mangled by a second pass.
    #[test]
    fn rewriting_twice_changes_nothing_more() {
        let once = rewrite_tool_names("call mcp__symphony__symphony_handoff now");
        assert_eq!(rewrite_tool_names(&once), once);
    }
}
