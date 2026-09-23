//! Brokered OpenCode materialization (STUDIO-1001, slice PB6). Rhapsody-only; no Go counterpart.
//!
//! This module owns the pure, security-critical half of the managed OpenCode launch: the per-session
//! internal provider identity, the per-turn generated `OPENCODE_CONFIG_CONTENT`/`OPENCODE_AUTH_CONTENT`
//! values, the authoritative managed environment, the pinned argv, and the exact-byte capability/broker
//! redactor. It writes no files and spawns nothing — [`crate::opencode::runner`] wires it into a turn.
//!
//! The binding rules are `provider-broker-design.md` §9.1/§9.2 and `provider-auth-design.md` §4.3:
//!
//! * a slash-free `rhapsody-` + 16 CSPRNG bytes in 22-character unpadded base64url provider id,
//!   stable for one OpenCode session and never persisted as provenance ([`InternalProviderId`]);
//! * generated config naming only that provider — `enabled_providers`, top-level/small model, and
//!   every pinned model-calling agent, sharing disabled, title generation disabled
//!   ([`generate_config_json`]);
//! * exactly one auth entry mapping it to the turn capability ([`generate_auth_json`]);
//! * inherited `OPENCODE_*`/`XDG_DATA_HOME` removed before the adapter's managed allow-list is
//!   appended, all size-checked before spawn ([`BrokeredMaterial::apply_env`]); and
//! * the exact capability and broker base URL/authority redacted from child output across chunk
//!   boundaries ([`CapabilityRedactor`]).
//!
//! The generated config's shape is the one the PB0 capture used
//! (`harness/harness-spike/opencode/broker/capture.sh`); it is pinned by
//! `tests/opencode_broker_fixture.rs` so a drift reds a test rather than a real run.

use std::fmt;
use std::path::PathBuf;

use rhapsody_provider_broker::{OsRandom, RandomSource};
use serde_json::Value;

use crate::AgentError;
use crate::opencode::args::Config;

/// The slash-free prefix every managed internal provider id carries.
pub const INTERNAL_PROVIDER_PREFIX: &str = "rhapsody-";
/// The bundled OpenCode provider adapter the generated config names (`§9.1`).
pub const ADAPTER_PACKAGE: &str = "@ai-sdk/openai-compatible";
/// The display name of the generated provider block.
pub const BROKER_PROVIDER_NAME: &str = "Rhapsody Broker";
/// The one agent brokered turns run as (`§9.2`), pinned in the generated config AND the argv.
pub const PINNED_AGENT: &str = "build";
/// Every native OpenCode agent that makes a model call, pinned to the internal provider/model
/// (`§9.1`). `title` is deliberately absent: it is disabled, not pinned.
pub const PINNED_MODEL_AGENTS: &[&str] = &[
    "build",
    "plan",
    "general",
    "explore",
    "compaction",
    "summary",
];

/// 16 CSPRNG bytes (`§9.1`).
const PROVIDER_ID_BYTES: usize = 16;
/// The unpadded base64url length of [`PROVIDER_ID_BYTES`] bytes.
const PROVIDER_ID_CHARS: usize = 22;
const BASE64URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Bound on a generated config/auth blob before spawn, so a serialization failure cannot fall back
/// to another auth source (`§9.2`).
pub const MAX_GENERATED_JSON_BYTES: usize = 256 * 1024;
/// Bound on the complete child environment before `execve`, same rationale.
pub const MAX_ENV_BYTES: usize = 1024 * 1024;

/// The marker substituted for the exact turn capability in child output.
pub const CAPABILITY_MARKER: &[u8] = b"[redacted-capability]";
/// The marker substituted for the exact broker base URL/authority in child output.
pub const BROKER_URL_MARKER: &[u8] = b"[redacted-broker-url]";

/// A per-session internal provider id: `rhapsody-` plus 16 CSPRNG bytes in 22-character unpadded
/// base64url form (`§9.1`). Slash-free by construction, stable for one OpenCode session, and never
/// persisted as provider provenance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InternalProviderId(String);

impl InternalProviderId {
    /// Mint a fresh id from the OS CSPRNG. An entropy failure is a typed error, never a downgrade —
    /// a predictable provider id would weaken the collision defense the id exists for.
    pub fn generate() -> Result<Self, AgentError> {
        let mut raw = [0u8; PROVIDER_ID_BYTES];
        OsRandom::new().fill(&mut raw).map_err(|_| {
            AgentError::Other(
                "opencode_broker_id_failed: the OS CSPRNG could not produce a provider id"
                    .to_string(),
            )
        })?;
        Ok(Self(format!(
            "{INTERNAL_PROVIDER_PREFIX}{}",
            base64url_unpadded(&raw)
        )))
    }

    /// Validate an already-rendered id (a round-trip/parse helper). Unknown shape is refused rather
    /// than accepted, so a caller can never hand the generated config an id it did not mint.
    pub fn parse(id: impl Into<String>) -> Result<Self, AgentError> {
        let id = id.into();
        let Some(suffix) = id.strip_prefix(INTERNAL_PROVIDER_PREFIX) else {
            return Err(AgentError::Other(format!(
                "opencode_broker_id_invalid: {id:?} does not start with {INTERNAL_PROVIDER_PREFIX:?}"
            )));
        };
        if suffix.len() != PROVIDER_ID_CHARS || !suffix.bytes().all(|b| BASE64URL.contains(&b)) {
            return Err(AgentError::Other(format!(
                "opencode_broker_id_invalid: {id:?} is not {PROVIDER_ID_CHARS} base64url characters"
            )));
        }
        Ok(Self(id))
    }

    /// The rendered id, slash-free.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The `<provider>/<model>` form OpenCode's `-m`, `model`, `small_model`, and agent model fields
    /// all use.
    pub fn model_ref(&self, model: &str) -> String {
        format!("{}/{model}", self.0)
    }
}

impl fmt::Display for InternalProviderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Encodes bytes as unpadded base64url (`A-Za-z0-9-_`). Hand-rolled to keep the agent crate free of
/// a base64 dependency for one 22-character value.
fn base64url_unpadded(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = u32::from(*chunk.get(1).unwrap_or(&0));
        let b2 = u32::from(*chunk.get(2).unwrap_or(&0));
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(BASE64URL[((n >> 18) & 63) as usize] as char);
        out.push(BASE64URL[((n >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(BASE64URL[((n >> 6) & 63) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(BASE64URL[(n & 63) as usize] as char);
        }
    }
    out
}

/// The generated `OPENCODE_CONFIG_CONTENT` (`§9.1`): exactly one OpenAI-compatible provider on the
/// broker's `/v1` base URL, `enabled_providers` naming only it, the top-level and small models pinned
/// to it, the pinned built-in `build` agent as `default_agent`, every native model-calling agent
/// pinned to it, sharing disabled, and the built-in `title` agent disabled.
///
/// `mcp` is the daemon's own MCP server block, supplied authoritatively when the run requires Teams;
/// it is embedded here rather than written as an additive project-controlled file (`§9.2`).
pub fn generate_config_json(
    provider_id: &InternalProviderId,
    model: &str,
    base_url: &str,
    mcp: Option<Value>,
) -> Result<String, AgentError> {
    let model_ref = provider_id.model_ref(model);
    let mut agents = serde_json::Map::new();
    agents.insert("title".to_string(), serde_json::json!({ "disable": true }));
    for agent in PINNED_MODEL_AGENTS {
        agents.insert(
            (*agent).to_string(),
            serde_json::json!({ "model": model_ref.clone() }),
        );
    }
    let mut provider = serde_json::Map::new();
    provider.insert(
        provider_id.as_str().to_string(),
        serde_json::json!({
            "npm": ADAPTER_PACKAGE,
            "name": BROKER_PROVIDER_NAME,
            "options": { "baseURL": base_url },
            "models": { model: { "name": model } },
        }),
    );

    let mut doc = serde_json::Map::new();
    doc.insert(
        "enabled_providers".to_string(),
        serde_json::json!([provider_id.as_str()]),
    );
    doc.insert("provider".to_string(), Value::Object(provider));
    doc.insert("model".to_string(), Value::from(model_ref.clone()));
    doc.insert("small_model".to_string(), Value::from(model_ref.clone()));
    doc.insert("default_agent".to_string(), Value::from(PINNED_AGENT));
    doc.insert("share".to_string(), Value::from("disabled"));
    doc.insert("agent".to_string(), Value::Object(agents));
    if let Some(mcp) = mcp {
        doc.insert("mcp".to_string(), mcp);
    }

    let text = serde_json::to_string(&Value::Object(doc))
        .map_err(|e| AgentError::Other(format!("opencode_config_serialize_failed: {e}")))?;
    check_json_size("OPENCODE_CONFIG_CONTENT", &text)?;
    Ok(text)
}

/// The generated `OPENCODE_AUTH_CONTENT` (`§9.1`): exactly one API auth entry mapping the internal
/// provider id to the turn capability. No file is written; this value goes only into the child's
/// environment.
pub fn generate_auth_json(
    provider_id: &InternalProviderId,
    capability: &str,
) -> Result<String, AgentError> {
    let doc = serde_json::json!({
        provider_id.as_str(): { "type": "api", "key": capability },
    });
    let text = serde_json::to_string(&doc)
        .map_err(|e| AgentError::Other(format!("opencode_auth_serialize_failed: {e}")))?;
    check_json_size("OPENCODE_AUTH_CONTENT", &text)?;
    Ok(text)
}

fn check_json_size(label: &str, text: &str) -> Result<(), AgentError> {
    if text.len() > MAX_GENERATED_JSON_BYTES {
        return Err(AgentError::Other(format!(
            "opencode_config_too_large: {label} is {} bytes (limit {MAX_GENERATED_JSON_BYTES})",
            text.len()
        )));
    }
    Ok(())
}

/// The pinned brokered argv (`§9.1`/§9.2), matching the PB0 capture byte-for-byte:
/// `run --format json --pure --auto --dir <ws> --agent build -m <provider>/<model> [-s <id>] <prompt>`.
///
/// `--auto` is always emitted (an explicitly disabled `auto_approve` is refused before this point),
/// `--agent build` and `--pure` are owned, and no `--variant`/`extra_args` can appear because brokered
/// v1 refuses them upstream.
pub fn build_brokered_args(
    _cfg: &Config,
    ws_path: &str,
    resume_id: &str,
    prompt: &str,
    provider_id: &InternalProviderId,
    model: &str,
) -> Vec<String> {
    let mut args = vec![
        "run".to_string(),
        "--format".to_string(),
        "json".to_string(),
        "--pure".to_string(),
        "--auto".to_string(),
        "--dir".to_string(),
        ws_path.to_string(),
        "--agent".to_string(),
        PINNED_AGENT.to_string(),
        "-m".to_string(),
        provider_id.model_ref(model),
    ];
    if !resume_id.is_empty() {
        args.push("-s".to_string());
        args.push(resume_id.to_string());
    }
    // LAST, and deliberately: `message..` is a positional array, so nothing may follow it.
    args.push(prompt.to_string());
    args
}

/// One brokered turn's generated values, plus the private directories they point at. It performs the
/// pre-spawn size checks and knows how to restate the authoritative environment.
#[derive(Debug)]
pub struct BrokeredMaterial {
    config_content: String,
    auth_content: String,
    config_dir: PathBuf,
    xdg_data_home: PathBuf,
}

impl BrokeredMaterial {
    /// Build one turn's material from the minted capability and broker base URL.
    pub fn build(
        provider_id: &InternalProviderId,
        model: &str,
        base_url: &str,
        capability: &str,
        config_dir: PathBuf,
        xdg_data_home: PathBuf,
        mcp: Option<Value>,
    ) -> Result<Self, AgentError> {
        Ok(Self {
            config_content: generate_config_json(provider_id, model, base_url, mcp)?,
            auth_content: generate_auth_json(provider_id, capability)?,
            config_dir,
            xdg_data_home,
        })
    }

    /// The exact `OPENCODE_CONFIG_CONTENT` value.
    pub fn config_content(&self) -> &str {
        &self.config_content
    }

    /// The exact `OPENCODE_AUTH_CONTENT` value.
    pub fn auth_content(&self) -> &str {
        &self.auth_content
    }

    /// Removes every inherited `OPENCODE_*`/`XDG_DATA_HOME` value from `base_env`, then appends the
    /// adapter's explicit managed allow-list (`§9.2`), and size-checks the complete environment
    /// before spawn. The removal happens FIRST, so ambient auto-share/experimental/logging/config/
    /// auth variables cannot survive ahead of the generated values.
    pub fn apply_env(&self, base_env: Vec<String>) -> Result<Vec<String>, AgentError> {
        let mut env: Vec<String> = base_env
            .into_iter()
            .filter(|kv| !is_inherited_opencode_or_xdg(kv))
            .collect();
        env.extend([
            format!("OPENCODE_CONFIG_CONTENT={}", self.config_content),
            format!("OPENCODE_AUTH_CONTENT={}", self.auth_content),
            format!("OPENCODE_CONFIG_DIR={}", self.config_dir.display()),
            "OPENCODE_DISABLE_PROJECT_CONFIG=1".to_string(),
            "OPENCODE_DISABLE_EXTERNAL_SKILLS=1".to_string(),
            "OPENCODE_DISABLE_MODELS_FETCH=1".to_string(),
            "OPENCODE_DISABLE_AUTOUPDATE=1".to_string(),
            "OPENCODE_DISABLE_SHARE=1".to_string(),
            "OPENCODE_PRINT_LOGS=0".to_string(),
            "OPENCODE_LOG_LEVEL=INFO".to_string(),
            format!("XDG_DATA_HOME={}", self.xdg_data_home.display()),
        ]);
        let total: usize = env.iter().map(|kv| kv.len() + 1).sum();
        if total > MAX_ENV_BYTES {
            return Err(AgentError::Other(format!(
                "opencode_env_too_large: the child environment is {total} bytes (limit {MAX_ENV_BYTES})"
            )));
        }
        Ok(env)
    }
}

/// Whether an environment entry is one brokered mode must strip before appending generated values:
/// any `OPENCODE_*` variable (including `OPENCODE_CONFIG`, `OPENCODE_AUTO_SHARE`, and unknown future
/// flags) or `XDG_DATA_HOME`.
fn is_inherited_opencode_or_xdg(kv: &str) -> bool {
    kv.starts_with("OPENCODE_") || kv.starts_with("XDG_DATA_HOME=")
}

/// The `host:port` authority of a broker base URL, or `None` when it has none. Redacting the
/// authority as well as the full URL matches `§9.4`'s "exact broker base URL/authority".
pub fn broker_authority(base_url: &str) -> Option<&str> {
    let rest = base_url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(base_url);
    let host = rest.split('/').next().unwrap_or("");
    if host.is_empty() { None } else { Some(host) }
}

/// A streaming exact-byte redactor for the turn capability and the broker base URL/authority
/// (`§9.4`). It matches across arbitrary chunk boundaries by retaining only the longest suffix that
/// is a proper prefix of one of its secrets — the same technique `rhapsody-provider-broker`'s own
/// response redactor uses, reimplemented here because the agent crate deliberately links only the
/// broker's protocol-neutral core, not its HTTP-backed `loopback` feature.
#[derive(Debug)]
pub struct CapabilityRedactor {
    entries: Vec<(Vec<u8>, &'static [u8])>,
    pending: Vec<u8>,
    finished: bool,
}

impl CapabilityRedactor {
    /// Build a redactor for one turn's capability and broker base URL. Empty inputs are ignored (a
    /// broker base URL is always non-empty in practice).
    pub fn new(capability: &str, base_url: &str) -> Self {
        let mut entries: Vec<(Vec<u8>, &'static [u8])> = Vec::new();
        if !capability.is_empty() {
            entries.push((capability.as_bytes().to_vec(), CAPABILITY_MARKER));
        }
        if !base_url.is_empty() {
            entries.push((base_url.as_bytes().to_vec(), BROKER_URL_MARKER));
        }
        if let Some(authority) = broker_authority(base_url)
            && authority != base_url
        {
            entries.push((authority.as_bytes().to_vec(), BROKER_URL_MARKER));
        }
        // Longest first so a secret that is a prefix of another is not matched short.
        entries.sort_by_key(|entry| std::cmp::Reverse(entry.0.len()));
        Self {
            entries,
            pending: Vec::new(),
            finished: false,
        }
    }

    /// Feed one raw child chunk, returning the bytes safe to parse/tee/log now.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        if self.finished {
            return Vec::new();
        }
        self.pending.extend_from_slice(chunk);
        self.drain()
    }

    /// End the stream. The retained tail is a proper prefix of a secret, never a complete one, so it
    /// is emitted unchanged.
    pub fn finish(&mut self) -> Vec<u8> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        std::mem::take(&mut self.pending)
    }

    fn drain(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        let mut i = 0usize;
        while i < self.pending.len() {
            if let Some(len) = self.match_at(i) {
                let marker = self
                    .entries
                    .iter()
                    .find(|(s, _)| s.len() == len)
                    .map(|(_, m)| *m);
                if let Some(marker) = marker {
                    out.extend_from_slice(marker);
                    i += len;
                    continue;
                }
            }
            let rest = &self.pending[i..];
            if self
                .entries
                .iter()
                .any(|(secret, _)| secret.len() > rest.len() && secret.starts_with(rest))
            {
                break;
            }
            out.push(self.pending[i]);
            i += 1;
        }
        self.pending.drain(..i);
        out
    }

    /// The byte length of a complete secret at `i`, if any.
    fn match_at(&self, i: usize) -> Option<usize> {
        let rest = &self.pending[i..];
        self.entries
            .iter()
            .find(|(secret, _)| rest.len() >= secret.len() && rest[..secret.len()] == secret[..])
            .map(|(secret, _)| secret.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pid(id: &str) -> InternalProviderId {
        InternalProviderId::parse(id).expect("valid id")
    }

    #[test]
    fn generated_provider_ids_are_slash_free_base64url_and_unique() {
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..64 {
            let id = InternalProviderId::generate().expect("generate");
            assert!(id.as_str().starts_with(INTERNAL_PROVIDER_PREFIX));
            assert!(!id.as_str().contains('/'));
            assert_eq!(
                id.as_str().len(),
                INTERNAL_PROVIDER_PREFIX.len() + PROVIDER_ID_CHARS
            );
            assert!(seen.insert(id.as_str().to_string()), "ids must not repeat");
        }
    }

    #[test]
    fn parse_refuses_unknown_shapes() {
        for bad in [
            "fireworks-ai",
            "rhapsody-",
            "rhapsody-short",
            "rhapsody-!!!!!!!!!!!!!!!!!!!!!!",
            "rhapsody-abcdefghijklmnopqrstuv/",
        ] {
            assert!(
                InternalProviderId::parse(bad).is_err(),
                "should refuse {bad:?}"
            );
        }
    }

    #[test]
    fn base64url_encodes_sixteen_bytes_as_twenty_two_chars() {
        let encoded = base64url_unpadded(&[0u8; 16]);
        assert_eq!(encoded.len(), 22);
        assert!(encoded.bytes().all(|b| BASE64URL.contains(&b)));
    }

    #[test]
    fn config_pins_only_the_internal_provider() {
        let id = pid("rhapsody-AAAAAAAAAAAAAAAAAAAAAA");
        let json = generate_config_json(&id, "accounts/models/x", "http://127.0.0.1:9/v1", None)
            .expect("config");
        let v: Value = serde_json::from_str(&json).expect("json");

        assert_eq!(
            v.pointer("/enabled_providers"),
            Some(&serde_json::json!([id.as_str()]))
        );
        assert_eq!(
            v.pointer("/model").and_then(Value::as_str),
            Some(format!("{}/accounts/models/x", id.as_str()).as_str())
        );
        assert_eq!(
            v.pointer("/small_model").and_then(Value::as_str),
            Some(format!("{}/accounts/models/x", id.as_str()).as_str())
        );
        assert_eq!(
            v.pointer("/default_agent").and_then(Value::as_str),
            Some("build")
        );
        assert_eq!(
            v.pointer("/share").and_then(Value::as_str),
            Some("disabled")
        );
        assert_eq!(v.pointer("/agent/title/disable"), Some(&Value::Bool(true)));
        // Exactly one provider block, keyed by the internal id.
        let providers = v
            .pointer("/provider")
            .and_then(Value::as_object)
            .expect("provider");
        assert_eq!(
            providers.len(),
            1,
            "only the internal provider may be defined"
        );
        assert!(providers.contains_key(id.as_str()));
        assert_eq!(
            v.pointer(&format!("/provider/{}/options/baseURL", id.as_str()))
                .and_then(Value::as_str),
            Some("http://127.0.0.1:9/v1")
        );
        // Every native model-calling agent pinned to the internal provider/model.
        for agent in PINNED_MODEL_AGENTS {
            assert_eq!(
                v.pointer(&format!("/agent/{agent}/model"))
                    .and_then(Value::as_str),
                Some(format!("{}/accounts/models/x", id.as_str()).as_str()),
                "agent {agent} must pin the internal model"
            );
        }
        // The title agent carries disable, not a model.
        assert!(v.pointer("/agent/title/model").is_none());
    }

    #[test]
    fn config_embeds_an_authoritative_mcp_block_when_given() {
        let id = pid("rhapsody-AAAAAAAAAAAAAAAAAAAAAA");
        let mcp = serde_json::json!({ "symphony": { "type": "local", "enabled": true } });
        let json = generate_config_json(&id, "m", "http://127.0.0.1:9/v1", Some(mcp.clone()))
            .expect("config");
        let v: Value = serde_json::from_str(&json).expect("json");
        assert_eq!(v.pointer("/mcp"), Some(&mcp));
    }

    #[test]
    fn auth_maps_exactly_one_provider_to_the_capability() {
        let id = pid("rhapsody-AAAAAAAAAAAAAAAAAAAAAA");
        let json = generate_auth_json(&id, "rhp-cap").expect("auth");
        let v: Value = serde_json::from_str(&json).expect("json");
        let map = v.as_object().expect("object");
        assert_eq!(map.len(), 1);
        assert_eq!(
            v.pointer(&format!("/{}/type", id.as_str())),
            Some(&Value::from("api"))
        );
        assert_eq!(
            v.pointer(&format!("/{}/key", id.as_str())),
            Some(&Value::from("rhp-cap"))
        );
    }

    // ⚠️ Mutation target: append the managed values without first stripping inherited `OPENCODE_*`
    // and `XDG_DATA_HOME`, and an ambient `OPENCODE_AUTO_SHARE`/`OPENCODE_CONFIG` survives ahead of
    // the generated contract. The removal must happen first.
    #[test]
    fn apply_env_strips_every_inherited_opencode_and_xdg_value_first() {
        let id = pid("rhapsody-AAAAAAAAAAAAAAAAAAAAAA");
        let material = BrokeredMaterial::build(
            &id,
            "m",
            "http://127.0.0.1:9/v1",
            "cap",
            PathBuf::from("/state/cfg"),
            PathBuf::from("/state/xdg"),
            None,
        )
        .expect("material");
        let base = vec![
            "HOME=/home/op".to_string(),
            "PATH=/usr/bin".to_string(),
            "OPENCODE_AUTO_SHARE=1".to_string(),
            "OPENCODE_CONFIG=/operator/own.json".to_string(),
            "OPENCODE_PRINT_LOGS=1".to_string(),
            "OPENCODE_FUTURE_FLAG=surprise".to_string(),
            "XDG_DATA_HOME=/operator/state".to_string(),
            "XDG_CONFIG_HOME=/operator/config".to_string(),
        ];
        let env = material.apply_env(base).expect("env");
        for kv in &env {
            assert!(
                !kv.starts_with("OPENCODE_AUTO_SHARE=")
                    && !kv.starts_with("OPENCODE_CONFIG=")
                    && !kv.starts_with("OPENCODE_FUTURE_FLAG=")
                    && !kv.starts_with("OPENCODE_PRINT_LOGS=1"),
                "an inherited control survived: {kv}"
            );
            // `OPENCODE_CONFIG=` (the file path) is stripped; `OPENCODE_CONFIG_CONTENT` is managed.
            assert!(
                kv != "OPENCODE_CONFIG=/operator/own.json",
                "OPENCODE_CONFIG must be stripped for brokered mode: {kv}"
            );
        }
        // HOME/XDG_CONFIG_HOME are NOT redirected (they are the trusted host boundary).
        assert!(env.iter().any(|kv| kv == "HOME=/home/op"));
        assert!(
            env.iter()
                .any(|kv| kv == "XDG_CONFIG_HOME=/operator/config")
        );
        // The generated values are present exactly once, and the private XDG is the broker's.
        assert_eq!(
            env.iter()
                .filter(|kv| kv.starts_with("XDG_DATA_HOME="))
                .count(),
            1
        );
        assert!(env.iter().any(|kv| kv == "XDG_DATA_HOME=/state/xdg"));
        assert!(env.iter().any(|kv| kv == "OPENCODE_DISABLE_SHARE=1"));
        assert!(env.iter().any(|kv| kv == "OPENCODE_PRINT_LOGS=0"));
        assert!(env.iter().any(|kv| kv == "OPENCODE_LOG_LEVEL=INFO"));
    }

    // The generated JSON and the env are size-checked before spawn.
    #[test]
    fn oversized_env_is_refused() {
        let id = pid("rhapsody-AAAAAAAAAAAAAAAAAAAAAA");
        let material = BrokeredMaterial::build(
            &id,
            "m",
            "http://127.0.0.1:9/v1",
            "cap",
            PathBuf::from("/c"),
            PathBuf::from("/x"),
            None,
        )
        .expect("material");
        let base = vec![format!("BIG={}", "a".repeat(MAX_ENV_BYTES + 1))];
        let err = material.apply_env(base).expect_err("must refuse");
        assert!(
            err.to_string().starts_with("opencode_env_too_large:"),
            "{err}"
        );
    }

    #[test]
    fn brokered_argv_matches_the_pb0_capture_shape() {
        let id = pid("rhapsody-AAAAAAAAAAAAAAAAAAAAAA");
        let got = build_brokered_args(&Config::default(), "/ws", "", "do it", &id, "probe-model");
        assert_eq!(
            got,
            vec![
                "run",
                "--format",
                "json",
                "--pure",
                "--auto",
                "--dir",
                "/ws",
                "--agent",
                "build",
                "-m",
                format!("{}/probe-model", id.as_str()).as_str(),
                "do it",
            ]
        );
    }

    #[test]
    fn brokered_argv_adds_resume_but_keeps_the_prompt_last() {
        let id = pid("rhapsody-AAAAAAAAAAAAAAAAAAAAAA");
        let got = build_brokered_args(&Config::default(), "/ws", "ses_1", "p", &id, "m");
        let pos = got.iter().position(|a| a == "-s").expect("-s");
        assert_eq!(got[pos + 1], "ses_1");
        assert_eq!(got.last().map(String::as_str), Some("p"));
    }

    fn redact(secrets: (&str, &str), chunks: &[&[u8]]) -> Vec<u8> {
        let mut r = CapabilityRedactor::new(secrets.0, secrets.1);
        let mut out = Vec::new();
        for c in chunks {
            out.extend_from_slice(&r.push(c));
        }
        out.extend_from_slice(&r.finish());
        out
    }

    #[test]
    fn redacts_capability_and_broker_url_across_every_split() {
        let capability = "rhp-abcdefghij";
        let base_url = "http://127.0.0.1:41234/v1";
        let body = format!("token={capability} url={base_url} done");
        for split in 0..=body.len() {
            let out = redact(
                (capability, base_url),
                &[&body.as_bytes()[..split], &body.as_bytes()[split..]],
            );
            let text = String::from_utf8_lossy(&out);
            assert!(
                !out.windows(capability.len())
                    .any(|w| w == capability.as_bytes()),
                "capability leaked at {split}"
            );
            assert!(!text.contains(base_url), "base url leaked at {split}");
            assert!(
                !text.contains("127.0.0.1:41234"),
                "authority leaked at {split}"
            );
            assert!(text.contains("[redacted-capability]"), "{text}");
            assert!(text.contains("[redacted-broker-url]"), "{text}");
        }
    }

    #[test]
    fn redacts_one_byte_at_a_time() {
        let chunks: Vec<&[u8]> = b"xrhp-secrety".chunks(1).collect();
        let out = redact(("rhp-secret", "http://127.0.0.1:9/v1"), &chunks);
        assert_eq!(out, b"x[redacted-capability]y");
    }

    #[test]
    fn broker_authority_extracts_host_and_port() {
        assert_eq!(
            broker_authority("http://127.0.0.1:41234/v1"),
            Some("127.0.0.1:41234")
        );
        assert_eq!(
            broker_authority("http://127.0.0.1:41234"),
            Some("127.0.0.1:41234")
        );
        assert_eq!(broker_authority(""), None);
    }
}
