//! MCP config injection + "me" identity env — parity port of Go `mcpinject.go` (+ `appendMeEnv`).
//!
//! [`inject_symphony_mcp`] MERGES a `symphony` MCP server into the operator's `mcp_config` (writing
//! a per-workspace `.symphony-mcp.json`) so a dispatched agent can query run/daemon state, while
//! preserving the operator's servers (a single `--mcp-config` + implied `--strict-mcp-config`
//! loads both). [`append_me_env`] adds the `SYMPHONY_ISSUE` / `SYMPHONY_RUN_ID` "me" identity env
//! (INF-473). Both are consumed by the A3 runner's `StartSession` / turn spawn.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::value::RawValue;

use crate::AgentError;

/// The per-workspace file the merged mcp_config is written to. It sits alongside the worktree the
/// runner `cd`s into, so a single `--mcp-config` path (+ implied `--strict-mcp-config`) loads BOTH
/// the operator's servers and the injected symphony server.
pub const MERGED_MCP_CONFIG_NAME: &str = ".symphony-mcp.json";

/// A top-level JSON object with raw (unparsed) values — the mirror of Go's
/// `map[string]json.RawMessage`. A [`BTreeMap`] gives sorted-key output, matching Go's
/// `encoding/json` map-key sorting so the merged file is deterministic.
type Doc = BTreeMap<String, Box<RawValue>>;

/// Writes a merged mcp_config into the workspace and returns `(path, kept_operator_symphony)`. The
/// merge takes the operator's base config (path, inline JSON, or empty) and adds a `symphony`
/// server unless the operator already defined one (theirs wins, and `kept_operator_symphony` is
/// `true`). On any failure it returns `Err`, so the caller keeps the operator's `mcp_config`
/// unchanged — a bad merge never breaks the existing MCP set. INF-473.
pub fn inject_symphony_mcp(
    ws_path: &str,
    base: &str,
    symphony_bin: &str,
    workflow_path: &str,
) -> Result<(String, bool), AgentError> {
    if symphony_bin.is_empty() {
        return Err(AgentError::Other("no symphony binary path".to_string()));
    }
    let mut doc = load_mcp_doc(ws_path, base)?;
    let mut servers: Doc = match doc.get("mcpServers") {
        Some(raw) if !raw.get().is_empty() => {
            // Unmarshaling a JSON `null` yields None here; re-initialize to an empty map so the
            // write below doesn't fail on `"mcpServers": null`.
            serde_json::from_str::<Option<Doc>>(raw.get())
                .map_err(|e| AgentError::Other(format!("parse mcpServers: {e}")))?
                .unwrap_or_default()
        }
        _ => Doc::new(),
    };

    let kept_operator_symphony = servers.contains_key("symphony");
    if !kept_operator_symphony {
        let sym = symphony_server(symphony_bin, workflow_path);
        let sym_raw =
            serde_json::value::to_raw_value(&sym).map_err(|e| AgentError::Other(e.to_string()))?;
        servers.insert("symphony".to_string(), sym_raw);
    }

    let servers_raw =
        serde_json::value::to_raw_value(&servers).map_err(|e| AgentError::Other(e.to_string()))?;
    doc.insert("mcpServers".to_string(), servers_raw);

    let out = serde_json::to_string_pretty(&doc).map_err(|e| AgentError::Other(e.to_string()))?;
    let dst = Path::new(ws_path).join(MERGED_MCP_CONFIG_NAME);
    std::fs::write(&dst, out)
        .map_err(|e| AgentError::Other(format!("write merged mcp_config: {e}")))?;
    Ok((dst.to_string_lossy().into_owned(), kept_operator_symphony))
}

/// The injected server entry: run the daemon binary as `symphony mcp [workflow_path]`. The absolute
/// workflow path lets the child resolve the SAME workflow (and thus the daemon's server port)
/// regardless of the agent's cwd.
fn symphony_server(symphony_bin: &str, workflow_path: &str) -> serde_json::Value {
    let mut args = vec![serde_json::Value::from("mcp")];
    if !workflow_path.is_empty() {
        // `std::path::absolute` mirrors Go's `filepath.Abs`: lexical absolutization, no existence
        // check / symlink resolution. On error, fall back to the path as given (as Go does).
        let abs = std::path::absolute(workflow_path)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| workflow_path.to_string());
        args.push(serde_json::Value::from(abs));
    }
    serde_json::json!({
        "command": symphony_bin,
        "args": args,
        "env": {},
    })
}

/// Resolves the operator's base mcp_config into a top-level JSON object (preserving any
/// non-`mcpServers` keys). `base` is: `""` ⇒ empty doc; inline JSON (leading `{`) ⇒ parsed; else a
/// path (absolute read as-is; relative resolved against `ws_path`). A bare `null` doc parses to an
/// empty map (Go's nil-map re-init).
fn load_mcp_doc(ws_path: &str, base: &str) -> Result<Doc, AgentError> {
    let trimmed = base.trim();
    if trimmed.is_empty() {
        return Ok(Doc::new());
    }
    let raw: Vec<u8> = if trimmed.starts_with('{') {
        trimmed.as_bytes().to_vec()
    } else {
        let p = if Path::new(base).is_absolute() {
            PathBuf::from(base)
        } else {
            Path::new(ws_path).join(base)
        };
        std::fs::read(&p)
            .map_err(|e| AgentError::Other(format!("read mcp_config {}: {e}", p.display())))?
    };
    let doc: Option<Doc> = serde_json::from_slice(&raw)
        .map_err(|e| AgentError::Other(format!("parse mcp_config: {e}")))?;
    Ok(doc.unwrap_or_default())
}

/// Appends the agent's "me" identity env so the injected daemon MCP server can default its tools to
/// this run (INF-473). Only known (non-empty / non-zero) values are added — a coordinator session
/// has neither.
///
/// STUDIO-603: each variable is emitted under BOTH the `SYMPHONY_*` and the `RHAPSODY_*` spelling,
/// with identical values. Purely additive — a child reading either wins, so an agent or MCP server
/// still looking for `SYMPHONY_RUN_ID` is unaffected. Nothing is removed here.
pub fn append_me_env(mut env: Vec<String>, issue: &str, run_id: i64) -> Vec<String> {
    if !issue.is_empty() {
        env.push(format!("SYMPHONY_ISSUE={issue}"));
        env.push(format!("RHAPSODY_ISSUE={issue}"));
    }
    if run_id != 0 {
        env.push(format!("SYMPHONY_RUN_ID={run_id}"));
        env.push(format!("RHAPSODY_RUN_ID={run_id}"));
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A unique scratch directory removed on drop — the Rust equivalent of Go's `t.TempDir()`,
    /// without a temp-file dependency (mirrors `rhapsody-tracker`'s `TempSource`).
    struct TempDir {
        dir: PathBuf,
    }

    impl TempDir {
        fn new() -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let seq = SEQ.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir()
                .join(format!("rhapsody-mcpinject-{}-{seq}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("create temp dir");
            TempDir { dir }
        }

        fn path(&self) -> String {
            self.dir.to_string_lossy().into_owned()
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// Reads the `mcpServers` map out of a merged config file (Go `parseServers`).
    fn parse_servers(path: &str) -> Doc {
        let raw = std::fs::read(path).expect("read merged config");
        #[derive(serde::Deserialize)]
        struct Wrapper {
            #[serde(rename = "mcpServers")]
            mcp_servers: Doc,
        }
        let doc: Wrapper = serde_json::from_slice(&raw).expect("parse merged config");
        doc.mcp_servers
    }

    #[derive(serde::Deserialize)]
    struct ServerEntry {
        command: String,
        #[serde(default)]
        args: Vec<String>,
    }

    fn decode_entry(raw: &RawValue) -> ServerEntry {
        serde_json::from_str(raw.get()).expect("decode server entry")
    }

    // Mirrors Go `claude.TestInjectMergesPreservingOperatorServers`: the symphony server is merged
    // into an operator's inline mcp_config; the Linear MCP is preserved.
    #[test]
    fn inject_merges_preserving_operator_servers() {
        let ws = TempDir::new();
        let base = r#"{"mcpServers":{"linear":{"command":"npx","args":["-y","linear-mcp"]}}}"#;
        let (path, kept_operator) = inject_symphony_mcp(
            &ws.path(),
            base,
            "/usr/local/bin/symphony",
            "/repo/WORKFLOW.md",
        )
        .expect("inject");
        assert!(
            !kept_operator,
            "kept_operator should be false — operator had no symphony server"
        );
        let servers = parse_servers(&path);
        assert!(
            servers.contains_key("linear"),
            "linear MCP server was dropped — merge must preserve operator servers"
        );
        let sym = servers.get("symphony").expect("symphony server injected");
        let entry = decode_entry(sym);
        assert_eq!(entry.command, "/usr/local/bin/symphony");
        assert_eq!(entry.args.first().map(String::as_str), Some("mcp"));
        // The workflow path is passed (absolutized) as the second arg.
        assert_eq!(entry.args.len(), 2);
        assert!(
            Path::new(&entry.args[1]).is_absolute(),
            "arg 2 should be an absolute workflow path, got {:?}",
            entry.args
        );
    }

    // Mirrors Go `claude.TestInjectDoesNotClobberOperatorSymphony`: an operator-defined `symphony`
    // server is NOT clobbered (theirs wins).
    #[test]
    fn inject_does_not_clobber_operator_symphony() {
        let ws = TempDir::new();
        let base =
            r#"{"mcpServers":{"symphony":{"command":"operator-symphony","args":["custom"]}}}"#;
        let (path, kept_operator) = inject_symphony_mcp(
            &ws.path(),
            base,
            "/usr/local/bin/symphony",
            "/repo/WORKFLOW.md",
        )
        .expect("inject");
        assert!(
            kept_operator,
            "kept_operator should be true — operator defined their own"
        );
        let servers = parse_servers(&path);
        let entry = decode_entry(servers.get("symphony").expect("symphony present"));
        assert_eq!(
            entry.command, "operator-symphony",
            "operator symphony server was clobbered"
        );
    }

    // Mirrors Go `claude.TestInjectNullMCPServers`: a base whose mcpServers is JSON null must not
    // panic (null → empty map); symphony is still injected.
    #[test]
    fn inject_null_mcp_servers() {
        let ws = TempDir::new();
        let (path, _) =
            inject_symphony_mcp(&ws.path(), r#"{"mcpServers":null}"#, "/bin/symphony", "")
                .expect("inject with null mcpServers");
        assert!(
            parse_servers(&path).contains_key("symphony"),
            "symphony not injected when mcpServers was null"
        );
    }

    // Mirrors Go `claude.TestInjectNullDoc`: a config file that is the bare literal `null` parses to
    // an empty top-level map; injection must not panic and must still write a symphony server.
    #[test]
    fn inject_null_doc() {
        let ws = TempDir::new();
        let base = Path::new(&ws.path()).join("mcp.json");
        std::fs::write(&base, "null").expect("write null config");
        let (path, _) =
            inject_symphony_mcp(&ws.path(), &base.to_string_lossy(), "/bin/symphony", "")
                .expect("inject with null config file");
        assert!(
            parse_servers(&path).contains_key("symphony"),
            "symphony not injected when config file was bare null"
        );
    }

    // Mirrors Go `claude.TestInjectEmptyBase`: empty base ⇒ a single-server config with just
    // symphony (additive).
    #[test]
    fn inject_empty_base() {
        let ws = TempDir::new();
        let (path, _) = inject_symphony_mcp(&ws.path(), "", "/bin/symphony", "").expect("inject");
        let servers = parse_servers(&path);
        assert_eq!(servers.len(), 1);
        assert!(servers.contains_key("symphony"));
    }

    // Mirrors Go `claude.TestAppendMeEnv`: SYMPHONY_ISSUE / SYMPHONY_RUN_ID added only for known
    // values; empty identity injects nothing.
    #[test]
    fn append_me_env_known_and_empty() {
        let got = append_me_env(vec!["A=1".to_string()], "INF-42", 7);
        assert!(
            got.contains(&"SYMPHONY_ISSUE=INF-42".to_string()),
            "{got:?}"
        );
        assert!(got.contains(&"SYMPHONY_RUN_ID=7".to_string()), "{got:?}");
        // Unknown values are omitted (coordinator session).
        let none = append_me_env(vec!["A=1".to_string()], "", 0);
        assert_eq!(
            none,
            vec!["A=1".to_string()],
            "empty identity injects nothing"
        );
    }

    // STUDIO-603: the identity is ALSO emitted under the RHAPSODY_* spelling, same values, and the
    // known/unknown gating applies to both spellings together.
    #[test]
    fn append_me_env_emits_both_spellings() {
        let got = append_me_env(vec!["A=1".to_string()], "INF-42", 7);
        for want in [
            "SYMPHONY_ISSUE=INF-42",
            "RHAPSODY_ISSUE=INF-42",
            "SYMPHONY_RUN_ID=7",
            "RHAPSODY_RUN_ID=7",
        ] {
            assert!(got.contains(&want.to_string()), "{want} missing: {got:?}");
        }
        // A run id without an issue adds only the run-id pair (and vice versa).
        let run_only = append_me_env(vec![], "", 7);
        assert_eq!(
            run_only,
            vec![
                "SYMPHONY_RUN_ID=7".to_string(),
                "RHAPSODY_RUN_ID=7".to_string()
            ]
        );
        let issue_only = append_me_env(vec![], "INF-42", 0);
        assert_eq!(
            issue_only,
            vec![
                "SYMPHONY_ISSUE=INF-42".to_string(),
                "RHAPSODY_ISSUE=INF-42".to_string()
            ]
        );
    }
}
