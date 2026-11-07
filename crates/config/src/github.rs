//! Opt-in owner-scoped GitHub credential references (STUDIO-1804; no Go counterpart).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Github {
    pub credentials: BTreeMap<String, Credential>,
    pub strict: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Credential {
    pub keychain: String,
}

const INVALID: &str = "github: use owner -> {keychain: service} references; literal tokens are forbidden in WORKFLOW.md";

fn token_like(value: &str) -> bool {
    ["github_pat_", "ghp_", "gho_", "ghu_", "ghs_", "ghr_"]
        .iter()
        .any(|p| value.starts_with(p))
}

/// Validate before Raw deserialization: serde's usual diagnostics can echo a pasted secret.
pub fn parse(value: &serde_yaml_ng::Value) -> Result<Github, String> {
    let cfg: Github = serde_yaml_ng::from_value(value.clone()).map_err(|_| INVALID.to_string())?;
    for (owner, credential) in &cfg.credentials {
        if owner.is_empty()
            || owner.len() > 39
            || token_like(owner)
            || !owner
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            || credential.keychain.is_empty()
            || credential.keychain.len() > 128
            || token_like(&credential.keychain)
            || !credential
                .keychain
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        {
            return Err(INVALID.to_string());
        }
    }
    let mut normalized = BTreeMap::new();
    for (owner, credential) in cfg.credentials {
        if normalized
            .insert(owner.to_ascii_lowercase(), credential)
            .is_some()
        {
            return Err("github: duplicate owner (names are case-insensitive)".into());
        }
    }
    Ok(Github {
        credentials: normalized,
        strict: cfg.strict,
    })
}

/// Only GitHub.com repo coordinates, without credentials, queries or arbitrary URL hosts.
pub fn repo(url: &str) -> Option<(String, String)> {
    let path = url
        .trim()
        .strip_prefix("https://github.com/")
        .or_else(|| url.trim().strip_prefix("git@github.com:"))
        .or_else(|| url.trim().strip_prefix("ssh://git@github.com/"))?;
    let path = path
        .trim_end_matches('/')
        .strip_suffix(".git")
        .unwrap_or(path.trim_end_matches('/'));
    let (owner, name) = path.split_once('/')?;
    if owner.is_empty()
        || name.is_empty()
        || !owner
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        || matches!(name, "." | "..")
    {
        return None;
    }
    Some((owner.to_ascii_lowercase(), name.to_string()))
}
