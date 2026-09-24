//! `warnings` — operator-facing legacy / migration diagnostics (STUDIO-994, provider-auth P13).
//!
//! **Rhapsody-only** (no Go v0.4.0 counterpart): the frozen Go reference has no provider registry,
//! so it has nothing to migrate *to*. This module exists because `provider-auth-design.md` §7
//! requires that "config warnings identify legacy fields and suggest the new form **without
//! rewriting the operator's workflow automatically**". It is therefore a pure, read-only scan:
//! [`legacy_warnings`] borrows a resolved [`Config`] and returns advisory notes; it never mutates
//! the config, never touches disk, and never fails a load.
//!
//! These are DIAGNOSTICS, not validation. The parity contract lives in [`crate::validate`], which
//! refuses an unusable config with byte-pinned Go sentinels; a warning here means "this still works,
//! and here is the migration", not "this is rejected". Nothing in the boot or dispatch path reads
//! them — `rhapsodyd doctor` is the one consumer — so an installation that never runs the doctor is
//! byte-identical to one built before this module existed.

use crate::model::Config;

/// The compiled-in default for the deprecated [`crate::model::Agent::handoff_drain_grace_ms`]
/// (decoded at `decode.rs`). A value other than this was written by an operator and can be reported
/// as the no-op it now is.
const DEFAULT_HANDOFF_DRAIN_GRACE_MS: i64 = 10_000;

/// One non-fatal, actionable migration note.
///
/// `code` is the stable machine token (stable enough for an operator script to grep and for a test
/// to assert); `field` is the dotted front-matter key the note is about; `message` states why the
/// legacy spelling still works today; `suggestion` states the new form. A warning is never a
/// rejection — the daemon continues exactly as before.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyWarning {
    pub code: &'static str,
    /// The dotted config key, e.g. `opencode.auth_source`.
    pub field: &'static str,
    pub message: String,
    pub suggestion: String,
}

impl LegacyWarning {
    fn new(code: &'static str, field: &'static str, message: String, suggestion: String) -> Self {
        Self {
            code,
            field,
            message,
            suggestion,
        }
    }
}

/// Scan a resolved config for legacy provider/auth spellings and report the migration path.
///
/// Order is deterministic and independent of the `providers` map's iteration order: the fixed rules
/// fire in source order and the per-provider-prefix rule iterates the map's canonical (sorted)
/// keys. An empty result means the config already uses only current spellings.
pub fn legacy_warnings(config: &Config) -> Vec<LegacyWarning> {
    let mut warnings = Vec::new();
    let providers_configured = !config.providers.is_empty();

    // §7: "opencode.auth_source remains valid and is used when no provider credential reference is
    // configured". It is the native-login path, not a target-state registry.
    if !config.opencode.auth_source.trim().is_empty() {
        warnings.push(LegacyWarning::new(
            "legacy_opencode_auth_source",
            "opencode.auth_source",
            "opencode.auth_source is the legacy native-login path: OpenCode runs against a copy of \
             the operator's own auth.json. It still works with no provider configured."
                .to_string(),
            "Configure a `providers:` entry and select it with `agent.provider`/`agent.model` to use \
             brokered credential custody instead."
                .to_string(),
        ));
    }

    // §7: "agent.backend: opencode plus opencode.model keeps existing behavior". With no
    // agent.provider the run stays on that legacy path.
    if config.agent.backend == "opencode" && config.agent.provider.trim().is_empty() {
        warnings.push(LegacyWarning::new(
            "legacy_opencode_backend",
            "agent.provider",
            "agent.backend is `opencode` with no agent.provider, so runs use the legacy \
             native-login OpenCode path."
                .to_string(),
            "Add `agent.provider: <id>` (and `agent.model`) and a matching `providers:` block to \
             select a pooled provider."
                .to_string(),
        ));
    }

    // §3/SCOPE: explicit providers are OpenCode-only in v1. A Claude install that also defines
    // providers would otherwise believe they take effect.
    if config.agent.backend != "opencode" && providers_configured {
        warnings.push(LegacyWarning::new(
            "providers_ignored_for_non_opencode_backend",
            "providers",
            format!(
                "{} provider(s) are configured but agent.backend is {:?}; explicit providers are \
                 OpenCode-only in v1, so they are not used by this run.",
                config.providers.len(),
                config.agent.backend
            ),
            "Set `agent.backend: opencode` with `agent.provider`/`agent.model`, or remove the \
             unused `providers:` block."
                .to_string(),
        ));
    }

    // §7: legacy opencode.model provider prefixes may seed a provider ID only when an exact
    // configured mapping exists. Report the seed when it does; a prefix with no configured provider
    // is left to the dispatch refusal, not invented here.
    if config.agent.provider.trim().is_empty()
        && let Some(prefix) = legacy_model_provider_prefix(&config.opencode.model)
        && config.providers.contains_key(prefix)
    {
        warnings.push(LegacyWarning::new(
            "legacy_model_provider_prefix",
            "opencode.model",
            format!(
                "opencode.model {:?} is prefixed with {:?}, which exactly matches a configured \
                 provider.",
                config.opencode.model, prefix
            ),
            format!(
                "Select it explicitly with `agent.provider: {prefix}` and put only the bare model \
                 in `agent.model`."
            ),
        ));
    }

    // INF-266 made this a no-op; it is still parsed so an old workflow loads. A non-default value
    // is dead configuration the operator should remove.
    if config.agent.handoff_drain_grace_ms != DEFAULT_HANDOFF_DRAIN_GRACE_MS {
        warnings.push(LegacyWarning::new(
            "deprecated_handoff_drain_grace_ms",
            "agent.handoff_drain_grace_ms",
            "agent.handoff_drain_grace_ms is a deprecated no-op (INF-266); it is still parsed for \
             backward compatibility but has no effect."
                .to_string(),
            "Remove `agent.handoff_drain_grace_ms`.".to_string(),
        ));
    }

    warnings
}

/// The `provider/model` prefix of an OpenCode model id, or `None` when the id has no `/`. OpenCode
/// spells a model as `<provider>/<model>`, so only the first segment is a candidate provider id.
fn legacy_model_provider_prefix(model: &str) -> Option<&str> {
    model
        .split_once('/')
        .map(|(prefix, _)| prefix)
        .filter(|prefix| !prefix.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{
        CREDENTIAL_SOURCE_KEYCHAIN, PROTOCOL_OPENAI_COMPATIBLE, ProviderDefinition,
    };
    use crate::{BrokerLimits, CredentialSource};
    use std::collections::BTreeMap;

    /// A minimal provider definition (no WORKFLOW.md needed).
    fn provider(id: &str) -> ProviderDefinition {
        ProviderDefinition {
            id: id.to_string(),
            protocol: PROTOCOL_OPENAI_COMPATIBLE.to_string(),
            display_name: String::new(),
            base_url: format!("https://{id}.example/v1"),
            allow_insecure_http: false,
            credential: CredentialSource {
                source: CREDENTIAL_SOURCE_KEYCHAIN.to_string(),
            },
            broker_limits: BrokerLimits::default(),
        }
    }

    /// A `Config` with decode defaults, mutated per test.
    fn config() -> Config {
        crate::decode(&crate::workflow::Definition {
            config: crate::workflow::YamlMap::new(),
            prompt_template: String::new(),
        })
        .expect("blank config decodes")
    }

    fn codes(config: &Config) -> Vec<&'static str> {
        legacy_warnings(config).iter().map(|w| w.code).collect()
    }

    /// A blank/default config is already on current spellings: no warnings. This is the additive
    /// guarantee — every install that never wrote the new keys sees exactly zero output.
    #[test]
    fn a_default_config_has_no_legacy_warnings() {
        assert!(
            legacy_warnings(&config()).is_empty(),
            "the default config must emit no migration noise"
        );
    }

    /// §7: `opencode.auth_source` is the legacy native-login path; it warns with a migration hint.
    #[test]
    fn a_configured_auth_source_is_reported_as_legacy() {
        let mut c = config();
        c.opencode.auth_source = "/home/op/.local/share/opencode/auth.json".to_string();
        let warnings = legacy_warnings(&c);
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].code, "legacy_opencode_auth_source");
        assert_eq!(warnings[0].field, "opencode.auth_source");
        assert!(
            warnings[0].suggestion.contains("agent.provider"),
            "the suggestion must name the new form: {}",
            warnings[0].suggestion
        );
    }

    /// §7: `agent.backend: opencode` with no provider is the legacy path.
    #[test]
    fn an_opencode_backend_without_a_provider_is_reported_as_legacy() {
        let mut c = config();
        c.agent.backend = "opencode".to_string();
        c.opencode.model = "anthropic/claude-sonnet-4-6".to_string();
        assert_eq!(codes(&c), vec!["legacy_opencode_backend"]);
    }

    /// An explicit provider selects the new path, so the legacy-backend warning must NOT fire.
    /// MUTATION: drop the `config.agent.provider.trim().is_empty()` guard and this reds.
    #[test]
    fn an_opencode_backend_with_a_provider_is_not_reported_as_legacy() {
        let mut c = config();
        c.agent.backend = "opencode".to_string();
        c.agent.provider = "fireworks".to_string();
        c.agent.model = "m".to_string();
        assert!(legacy_warnings(&c).is_empty(), "{:?}", legacy_warnings(&c));
    }

    /// Providers are OpenCode-only in v1; a Claude install defining them should be told they are
    /// inert rather than believing they take effect.
    #[test]
    fn providers_configured_under_claude_are_reported_as_inert() {
        let mut c = config();
        c.providers = BTreeMap::from([("fireworks".to_string(), provider("fireworks"))]);
        let warnings = legacy_warnings(&c);
        assert_eq!(warnings.len(), 1);
        assert_eq!(
            warnings[0].code,
            "providers_ignored_for_non_opencode_backend"
        );
        assert_eq!(warnings[0].field, "providers");
    }

    /// §7: a `provider/model` prefix seeds a provider id only when an exact configured mapping
    /// exists. An unknown prefix is NOT reported here (dispatch refuses it, this does not guess).
    /// MUTATION: warn on any prefix (drop the `contains_key` check) and this reds.
    #[test]
    fn a_model_prefix_matching_a_configured_provider_is_reported() {
        let mut c = config();
        c.agent.backend = "opencode".to_string();
        c.opencode.model = "fireworks/accounts/fireworks/models/deepseek-v4p1-flash".to_string();
        c.providers = BTreeMap::from([("fireworks".to_string(), provider("fireworks"))]);
        let warnings = legacy_warnings(&c);
        let prefix = warnings
            .iter()
            .find(|w| w.code == "legacy_model_provider_prefix")
            .expect("the exact configured prefix must be reported");
        assert!(
            prefix.suggestion.contains("agent.provider: fireworks"),
            "{}",
            prefix.suggestion
        );

        let mut unknown = config();
        unknown.opencode.model = "not-configured/model".to_string();
        unknown.agent.backend = "opencode".to_string();
        unknown.providers = BTreeMap::from([("fireworks".to_string(), provider("fireworks"))]);
        assert!(
            codes(&unknown)
                .iter()
                .all(|code| *code != "legacy_model_provider_prefix"),
            "an unmatched prefix must not be reported as a seed"
        );
    }

    /// The deprecated no-op warns only when the operator changed it from its default.
    #[test]
    fn a_non_default_handoff_drain_grace_warns_and_the_default_does_not() {
        let mut c = config();
        assert!(
            !codes(&c).contains(&"deprecated_handoff_drain_grace_ms"),
            "the default must not warn"
        );
        c.agent.handoff_drain_grace_ms = 0;
        assert!(codes(&c).contains(&"deprecated_handoff_drain_grace_ms"));
    }

    /// The scan is read-only: running it does not mutate the config it borrows.
    /// MUTATION: make `legacy_warnings` take `&mut Config` and rewrite a field and this reds.
    #[test]
    fn warnings_do_not_rewrite_the_config() {
        let mut c = config();
        c.opencode.auth_source = "/tmp/auth.json".to_string();
        c.agent.backend = "opencode".to_string();
        c.opencode.model = "fireworks/m".to_string();
        c.providers = BTreeMap::from([("fireworks".to_string(), provider("fireworks"))]);
        let before = c.clone();
        let _ = legacy_warnings(&c);
        assert_eq!(c, before, "the warning scan must not mutate the config");
    }
}
