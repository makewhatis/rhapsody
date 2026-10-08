//! Isolated OpenCode manager posture (STUDIO-1120); no Go counterpart.
//!
//! Measurements on OpenCode 1.18.30 (2026-10-06): `debug agent build` with only the
//! wildcard deny and manager-tool allow (no `tools` map) resolved every reported built-in to
//! `false`. We still name every known built-in explicitly, and MH3's self-test must fail closed
//! on any resolved tool that is not `false`, including future names.
//!
//! `OPENCODE_DISABLE_MODELS_FETCH=1` did NOT make `openai/gpt-6.1-sol` resolvable in a fresh
//! private environment: stderr reported `ProviderModelNotFoundError`. The control without that
//! flag reported the same missing model, so this does not establish the flag caused it. The
//! flag is omitted: the required model has not been proven to resolve with fetching disabled.
//! Both probes used only an OpenAI login with a present-but-empty refresh field, and their
//! private trees were removed by RAII; no run credential was read back or copied back.
//!
//! The operator reproduced a cold models.dev catalogue failing the first private-cache turn
//! (2026-10-07). Both manager sessions and canaries seed only `~/.cache/opencode/models.json`
//! before launching; they never inherit the operator's cache directory or its other contents.

use std::path::{Path, PathBuf};

use crate::AgentError;

/// Built-ins measured on OpenCode 1.18.30. The self-test must also refuse future resolved tools.
pub const OPENCODE_KNOWN_BUILTINS: &[&str] = &[
    "invalid",
    "question",
    "bash",
    "read",
    "glob",
    "grep",
    "edit",
    "write",
    "task",
    "webfetch",
    "todowrite",
    "websearch",
    "skill",
    "apply_patch",
];

/// Inline config: one manager-role MCP server, no plugins, no built-ins.
pub fn manager_config_content(
    model: &str,
    daemon_bin: &str,
    workflow_path: &str,
    real_home: &str,
    run_id: &str,
) -> String {
    let tools: serde_json::Map<String, serde_json::Value> = OPENCODE_KNOWN_BUILTINS
        .iter()
        .map(|name| (name.to_string(), serde_json::Value::Bool(false)))
        .collect();
    let mut command = vec![
        daemon_bin.to_string(),
        "mcp".to_string(),
        "--role".to_string(),
        "manager".to_string(),
    ];
    if !workflow_path.is_empty() {
        command.push(
            std::path::absolute(workflow_path)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| workflow_path.to_string()),
        );
    }
    serde_json::json!({
        "model": model, "autoupdate": false, "share": "disabled", "plugin": [],
        "tools": tools, "permission": {"*":"deny", "symphony_*":"allow"},
        "mcp": {"symphony": {"type":"local", "command":command,
            "environment":{"HOME":real_home, "SYMPHONY_RUN_ID":run_id}}}
    })
    .to_string()
}

/// Daemon-controlled argv; operator extra arguments and approval knobs are never inherited.
pub fn manager_args(cwd: &str, model: &str, effort: &str, prompt: &str) -> Vec<String> {
    let mut args: Vec<String> = [
        "run", "--format", "json", "--dir", cwd, "--agent", "build", "-m", model,
    ]
    .into_iter()
    .map(str::to_string)
    .collect();
    if !effort.is_empty() {
        args.extend(["--variant".to_string(), effort.to_string()]);
    }
    args.push(prompt.to_string());
    args
}

/// A complete allow-list, to be applied after `Command::env_clear`.
pub fn manager_env(
    run_home: &Path,
    xdg_root: &Path,
    config_content: &str,
    run_id: &str,
    path: &str,
) -> Vec<(String, String)> {
    let mut env = vec![
        ("HOME".to_string(), run_home.to_string_lossy().into_owned()),
        ("PATH".to_string(), path.to_string()),
    ];
    for (key, dir) in [
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_CACHE_HOME", "cache"),
        ("XDG_STATE_HOME", "state"),
    ] {
        env.push((
            key.to_string(),
            xdg_root.join(dir).to_string_lossy().into_owned(),
        ));
    }
    for key in [
        "OPENCODE_DISABLE_PROJECT_CONFIG",
        "OPENCODE_DISABLE_EXTERNAL_SKILLS",
        "OPENCODE_DISABLE_AUTOUPDATE",
        "OPENCODE_DISABLE_SHARE",
    ] {
        env.push((key.to_string(), "1".to_string()));
    }
    env.extend([
        (
            "OPENCODE_CONFIG_CONTENT".to_string(),
            config_content.to_string(),
        ),
        ("SYMPHONY_RUN_ID".to_string(), run_id.to_string()),
    ]);
    env
}

/// Seeds the public models.dev catalogue before OpenCode starts in a cold private cache.
pub fn seed_manager_catalogue(real_home: &Path, xdg_cache_home: &Path) -> Result<(), AgentError> {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let bytes = std::fs::read(real_home.join(".cache/opencode/models.json")).map_err(|e| {
        AgentError::Other(if e.kind() == std::io::ErrorKind::NotFound {
            "no OpenCode model catalogue; run opencode once as the daemon's user".to_string()
        } else {
            "manager_model_catalogue_seed_failed: could not read OpenCode model catalogue"
                .to_string()
        })
    })?;
    let write = || -> std::io::Result<()> {
        let dir = xdg_cache_home.join("opencode");
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)?;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(dir.join("models.json"))?;
        f.write_all(&bytes)
    };
    write().map_err(|_| {
        AgentError::Other(
            "manager_model_catalogue_seed_failed: could not write private OpenCode model catalogue"
                .to_string(),
        )
    })
}

/// Writes only OpenAI, with a present-but-empty refresh field. Never reads or copies the result back.
pub fn seed_manager_credential(
    operator_auth: &Path,
    xdg_data_home: &Path,
) -> Result<(), AgentError> {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let mut login = read_login(operator_auth).map_err(AgentError::AuthFailed)?;
    login.insert(
        "refresh".to_string(),
        serde_json::Value::String(String::new()),
    );
    let doc = serde_json::json!({"openai":login}).to_string();
    let dir = xdg_data_home.join("opencode");
    let write = || -> std::io::Result<()> {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)?;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(dir.join("auth.json"))?;
        f.write_all(doc.as_bytes())
    };
    write().map_err(|_| {
        AgentError::Other(
            "manager_credential_seed_failed: could not write private OpenAI login".to_string(),
        )
    })
}

#[derive(Debug, PartialEq, Eq)]
pub enum OpenAiLoginStatus {
    Valid { expires_ms: i64 },
    Expired { expires_ms: i64 },
    Missing(String),
}

/// Reads only the login's expiry; no token is exposed to the caller.
pub fn openai_login_status(operator_auth: &Path) -> OpenAiLoginStatus {
    match read_login(operator_auth) {
        Ok(login) => {
            let Some(expires_ms) = login.get("expires").and_then(serde_json::Value::as_i64) else {
                return OpenAiLoginStatus::Missing(
                    "OpenAI login has no numeric expires".to_string(),
                );
            };
            if expires_ms <= chrono::Utc::now().timestamp_millis() {
                OpenAiLoginStatus::Expired { expires_ms }
            } else {
                OpenAiLoginStatus::Valid { expires_ms }
            }
        }
        Err(reason) => OpenAiLoginStatus::Missing(reason),
    }
}

/// SHA-256 of the operator's OpenAI entry, excluding unrelated providers. Never hashes a run copy.
pub fn openai_login_fingerprint(operator_auth: &Path) -> Option<String> {
    use sha2::Digest;
    let login = read_login(operator_auth).ok()?;
    Some(
        sha2::Sha256::digest(serde_json::Value::Object(login).to_string().as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
    )
}

/// Locates the operator's source before any private environment is applied.
pub fn operator_auth_path() -> Option<PathBuf> {
    if let Some(data) = std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(data).join("opencode/auth.json"));
    }
    std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .map(|h| PathBuf::from(h).join(".local/share/opencode/auth.json"))
}

fn read_login(path: &Path) -> Result<serde_json::Map<String, serde_json::Value>, String> {
    use std::os::unix::fs::PermissionsExt;
    // Closed diagnostics: JSON parser errors can quote source bytes, which must never reach logs.
    let metadata = std::fs::metadata(path).map_err(|_| {
        "operator OpenCode login is missing or unreadable; run opencode auth login".to_string()
    })?;
    if metadata.permissions().mode() & 0o444 == 0 {
        return Err("operator OpenCode login is unreadable".to_string());
    }
    let bytes =
        std::fs::read(path).map_err(|_| "operator OpenCode login is unreadable".to_string())?;
    let doc: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|_| "operator OpenCode login is malformed JSON".to_string())?;
    let login = doc
        .get("openai")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| "operator has no OpenAI login; run opencode auth login".to_string())?;
    if login
        .get("expires")
        .and_then(serde_json::Value::as_i64)
        .is_none()
    {
        return Err("OpenAI login has no numeric expires".to_string());
    }
    Ok(login.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opencode::testdir::TempDir;
    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn config_disables_every_known_builtin_and_denies_star() {
        let v: Value = serde_json::from_str(&manager_config_content(
            "openai/gpt-6.1-sol",
            "/bin/rhapsodyd",
            "/repo/WORKFLOW.md",
            "/operator",
            "42",
        ))
        .unwrap();
        assert_eq!(v["model"], "openai/gpt-6.1-sol");
        for name in OPENCODE_KNOWN_BUILTINS {
            assert_eq!(v["tools"][name], false, "{name}");
        }
        assert_eq!(v["permission"], json!({"*":"deny", "symphony_*":"allow"}));
        for name in ["docs_read", "docs_list", "tracker_documents"] {
            assert!(crate::manager::MANAGER_MCP_TOOLS.contains(&name));
            assert_eq!(v["permission"]["symphony_*"], "allow", "symphony_{name}");
        }
        assert_eq!(v["plugin"], json!([]));
        assert_eq!(v["autoupdate"], false);
        assert_eq!(v["share"], "disabled");
        assert_eq!(v["mcp"].as_object().unwrap().len(), 1);
        assert_eq!(v["mcp"]["symphony"]["type"], "local");
        assert_eq!(
            v["mcp"]["symphony"]["command"],
            json!([
                "/bin/rhapsodyd",
                "mcp",
                "--role",
                "manager",
                "/repo/WORKFLOW.md"
            ])
        );
        assert_eq!(
            v["mcp"]["symphony"]["environment"],
            json!({"HOME":"/operator", "SYMPHONY_RUN_ID":"42"})
        );
    }

    #[test]
    fn args_never_auto() {
        for effort in ["", "xhigh"] {
            let args = manager_args("/cwd", "openai/gpt-6.1-sol", effort, "prompt");
            assert!(!args.iter().any(|s| s == "--auto"));
            let mut expected = vec![
                "run",
                "--format",
                "json",
                "--dir",
                "/cwd",
                "--agent",
                "build",
                "-m",
                "openai/gpt-6.1-sol",
            ];
            if !effort.is_empty() {
                expected.extend(["--variant", effort]);
            }
            expected.push("prompt");
            assert_eq!(args, expected);
        }
    }

    #[test]
    fn env_is_an_allow_list() {
        let env: std::collections::BTreeMap<_, _> = manager_env(
            Path::new("/home"),
            Path::new("/xdg"),
            "config",
            "42",
            "/bin",
        )
        .into_iter()
        .collect();
        let expected = [
            ("HOME", "/home"),
            ("PATH", "/bin"),
            ("XDG_CONFIG_HOME", "/xdg/config"),
            ("XDG_DATA_HOME", "/xdg/data"),
            ("XDG_CACHE_HOME", "/xdg/cache"),
            ("XDG_STATE_HOME", "/xdg/state"),
            ("OPENCODE_DISABLE_PROJECT_CONFIG", "1"),
            ("OPENCODE_DISABLE_EXTERNAL_SKILLS", "1"),
            ("OPENCODE_DISABLE_AUTOUPDATE", "1"),
            ("OPENCODE_DISABLE_SHARE", "1"),
            ("OPENCODE_CONFIG_CONTENT", "config"),
            ("SYMPHONY_RUN_ID", "42"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        assert_eq!(env, expected);
    }

    fn source(dir: &TempDir) -> PathBuf {
        let p = dir.path().join("source.json");
        std::fs::write(&p, json!({"openai":{"type":"oauth","access":"synthetic-access","refresh":"synthetic-refresh","expires":4102444800000_i64},"fireworks-ai":{"key":"unrelated"}}).to_string()).unwrap();
        p
    }

    #[test]
    fn seeded_credential_is_openai_only_with_empty_refresh() {
        let d = TempDir::new();
        let src = source(&d);
        let data = d.path().join("data");
        seed_manager_credential(&src, &data).unwrap();
        // Synthetic test credential only; production never reads the seeded file back.
        let dst = data.join("opencode/auth.json");
        let v: Value = serde_json::from_slice(&std::fs::read(&dst).unwrap()).unwrap();
        assert_eq!(v.as_object().unwrap().len(), 1);
        assert_eq!(v["openai"]["refresh"], "");
        let original: Value = serde_json::from_slice(&std::fs::read(&src).unwrap()).unwrap();
        let mut expected = original["openai"].clone();
        expected["refresh"] = json!("");
        assert_eq!(v["openai"], expected);
        assert_eq!(
            std::fs::metadata(&dst).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn seed_never_modifies_source() {
        let d = TempDir::new();
        let src = source(&d);
        let digest = || Sha256::digest(std::fs::read(&src).unwrap());
        let before = digest();
        let mtime = std::fs::metadata(&src).unwrap().modified().unwrap();
        seed_manager_credential(&src, &d.path().join("data")).unwrap();
        assert!(d.path().join("data/opencode/auth.json").is_file());
        assert_eq!(digest(), before);
        assert_eq!(std::fs::metadata(&src).unwrap().modified().unwrap(), mtime);
    }

    #[test]
    fn credential_status_handles_bad_files() {
        let d = TempDir::new();
        let p = d.path().join("bad.json");
        let check = || {
            let OpenAiLoginStatus::Missing(reason) = openai_login_status(&p) else {
                panic!("bad credential must be unavailable")
            };
            assert!(!reason.contains("secret-canary"));
            assert!(openai_login_fingerprint(&p).is_none());
            assert!(seed_manager_credential(&p, &d.path().join("data")).is_err());
        };
        check();
        for raw in [
            "secret-canary",
            r#"{"other":"secret-canary"}"#,
            r#"{"openai":{"expires":"secret-canary"}}"#,
            r#"{"openai":null}"#,
        ] {
            std::fs::write(&p, raw).unwrap();
            check();
        }
        std::fs::write(&p, r#"{"openai":{"expires":4102444800000}}"#).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o000)).unwrap();
        check();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    #[test]
    fn login_status_reads_expires() {
        let d = TempDir::new();
        let p = d.path().join("auth.json");
        for (expires, expected) in [
            (1, OpenAiLoginStatus::Expired { expires_ms: 1 }),
            (
                4102444800000_i64,
                OpenAiLoginStatus::Valid {
                    expires_ms: 4102444800000,
                },
            ),
        ] {
            std::fs::write(
                &p,
                json!({"openai":{"expires":expires,"access":"synthetic"}}).to_string(),
            )
            .unwrap();
            assert_eq!(openai_login_status(&p), expected);
            assert_eq!(openai_login_fingerprint(&p).unwrap().len(), 64);
        }
    }
}
