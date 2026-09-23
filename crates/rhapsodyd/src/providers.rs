//! providers — the daemon's provider status + model-catalog composition (STUDIO-990, P9). Rhapsody
//! only; no Go counterpart.
//!
//! This is the one place the P9 pieces meet the real daemon: it adapts the daemon's authenticated
//! [`CredentialResolver`] into the coordinator's [`CredentialReadSource`], exposes PB4's live broker
//! availability, and applies the resolved workflow's `providers:` block to the non-secret status cache.
//!
//! Everything the two `GET` routes serve comes from the cache this applies; the ONE credentialed
//! operation is [`ProviderRuntime::refresh_catalog`], reached only through the operator-guarded POST.
//! A daemon with no `providers:` block, or with no credential owner, simply serves empty/unknown
//! statuses and never contacts anything.

use std::sync::Arc;

use async_trait::async_trait;
use rhapsody_config::{Config, ProviderReload, providers::provider_turn_deadline_ms};
use rhapsody_credential_ipc::domain::{Binding, CredentialState};
use rhapsody_provider_broker::BrokerRegistrar;
use rhapsody_provider_status::{
    CatalogError, CatalogSnapshot, CredentialReadSource, ObservedRead, ObservedState,
    OpenAiCompatibleDiscovery, ProviderConfig, ProviderStatusView, RefreshCoordinator,
};

use crate::credential_client::CredentialResolver;

/// Adapts the daemon's authenticated credential resolver to the coordinator's read seam. The
/// resolver already folds owner availability into its two counters, so this is a pure state mapping
/// and touches no Keychain itself.
pub struct ResolverSource {
    resolver: Arc<CredentialResolver>,
}

impl ResolverSource {
    pub fn new(resolver: Arc<CredentialResolver>) -> Self {
        Self { resolver }
    }
}

#[async_trait]
impl CredentialReadSource for ResolverSource {
    async fn read_bound(&self, account: String, binding: Binding) -> ObservedRead {
        let observed = self.resolver.read_bound(account, binding).await;
        ObservedRead {
            state: map_state(observed.read.state),
            owner_revision: observed.read.revision,
            availability_generation: observed.availability_generation,
        }
    }
}

/// Map the credential-owner state onto the coordinator's observed state. The lease moves straight
/// through — it is never read here.
fn map_state(state: CredentialState) -> ObservedState {
    match state {
        CredentialState::Present(lease) => ObservedState::Present(lease),
        CredentialState::Absent => ObservedState::Absent,
        CredentialState::DeniedOrLocked => ObservedState::DeniedOrLocked,
        CredentialState::Malformed => ObservedState::Malformed,
        CredentialState::BindingMismatch => ObservedState::BindingMismatch,
        CredentialState::OwnerUnavailable => ObservedState::OwnerUnavailable,
        CredentialState::OwnerUnauthorized => ObservedState::OwnerUnauthorized,
    }
}

/// The daemon's provider runtime: the coordinator plus the live broker handle used only for the
/// non-secret `broker_available` status field.
pub struct ProviderRuntime {
    coordinator: Arc<RefreshCoordinator>,
    broker: BrokerRegistrar,
}

impl ProviderRuntime {
    /// Build the runtime. `resolver` is the daemon's credential owner adapter (a bare
    /// [`CredentialResolver::new`] when no bootstrap channel was established — it then resolves
    /// `OwnerUnavailable` for the process's lifetime, which is the honest status).
    pub fn new(resolver: Arc<CredentialResolver>, broker: BrokerRegistrar) -> Arc<Self> {
        let source = Arc::new(ResolverSource::new(resolver));
        let discovery = Arc::new(OpenAiCompatibleDiscovery);
        Arc::new(Self {
            coordinator: RefreshCoordinator::new(source, discovery),
            broker,
        })
    }

    /// Apply the resolved workflow's global `providers:` block at boot: set the cache's generation
    /// from [`ProviderReload`], mark every affected provider unknown/refreshing, and spawn one bounded
    /// off-loop status refresh per intent. Returns the number of refreshes scheduled.
    pub fn apply_config(self: &Arc<Self>, config: &Config) -> usize {
        let providers = provider_configs(config);
        if providers.is_empty() {
            return 0;
        }
        let deadline = provider_turn_deadline_ms(config.opencode.turn_timeout_ms);
        let generation = ProviderReload::from_providers(&config.providers, deadline).revision();
        let intents = self.coordinator.apply_reload(generation, &providers);
        let scheduled = intents.len();
        for intent in intents {
            self.coordinator.spawn_status_refresh(intent);
        }
        scheduled
    }

    /// The cache-only status list (`GET /api/v1/providers`).
    pub fn statuses(&self) -> Vec<ProviderStatusView> {
        self.coordinator.status_views(self.broker.is_available())
    }

    /// One provider's cache-only status, or `None` for an unknown id.
    pub fn status(&self, provider_id: &str) -> Option<ProviderStatusView> {
        self.coordinator
            .status_view(provider_id, self.broker.is_available())
    }

    /// One provider's cache-only catalog. A configured provider with no cache yet answers an empty,
    /// unknown-aged snapshot (never misreported as a fetched empty list); an unknown id is `None`.
    pub fn catalog(&self, provider_id: &str) -> Option<CatalogSnapshot> {
        if let Some(snapshot) = self.coordinator.catalog_view(provider_id) {
            return Some(snapshot);
        }
        self.coordinator
            .knows(provider_id)
            .then(|| CatalogSnapshot {
                provider_id: provider_id.to_string(),
                models: Vec::new(),
                truncated: false,
                cache_age_ms: None,
                error: None,
                error_message: None,
                manual_entry_allowed: true,
            })
    }

    /// The explicit, bounded catalog refresh (`POST …/models/refresh`). Unknown provider ⇒
    /// [`CatalogError::Unsupported`] (the handler's 404); otherwise a snapshot whose own `error`
    /// carries any catalog failure.
    pub async fn refresh_catalog(
        &self,
        provider_id: &str,
    ) -> Result<CatalogSnapshot, CatalogError> {
        if !self.coordinator.knows(provider_id) {
            return Err(CatalogError::Unsupported);
        }
        self.coordinator.refresh_catalog(provider_id).await
    }
}

/// Build the coordinator config list from a resolved workflow's global providers. A provider whose
/// canonical binding cannot be derived is skipped (validation should have refused it); it simply has
/// no status.
fn provider_configs(config: &Config) -> Vec<ProviderConfig> {
    let mut out = Vec::with_capacity(config.providers.len());
    for (id, def) in &config.providers {
        let binding = match def.credential_binding() {
            Ok(binding) => Binding {
                provider_id: binding.provider_id,
                adapter: binding.adapter,
                base_url: binding.base_url,
            },
            Err(_) => continue,
        };
        out.push(ProviderConfig {
            provider_id: id.clone(),
            binding,
            allow_insecure_http: def.allow_insecure_http,
        });
    }
    out
}

/// A fresh resolver that never learned a bootstrap frame — the daemon's owner adapter when
/// `--credential-bootstrap` was not passed. It resolves `OwnerUnavailable` consistently.
pub fn unavailable_owner() -> Arc<CredentialResolver> {
    Arc::new(CredentialResolver::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rhapsody_credential_ipc::domain::OPENAI_CHAT_COMPLETIONS_BEARER_V1;

    #[test]
    fn map_state_carries_every_non_secret_state() {
        let lease = rhapsody_credential_ipc::domain::BoundCredentialLease::new(
            Binding {
                provider_id: "p".into(),
                adapter: OPENAI_CHAT_COMPLETIONS_BEARER_V1.into(),
                base_url: "https://x/v1".into(),
            },
            "sk-fake".into(),
        );
        assert!(matches!(
            map_state(CredentialState::Present(lease)),
            ObservedState::Present(_)
        ));
        for (state, expected) in [
            (CredentialState::Absent, "absent"),
            (CredentialState::DeniedOrLocked, "denied"),
            (CredentialState::Malformed, "malformed"),
            (CredentialState::BindingMismatch, "mismatch"),
            (CredentialState::OwnerUnavailable, "unavailable"),
            (CredentialState::OwnerUnauthorized, "unauthorized"),
        ] {
            let mapped = map_state(state);
            assert_eq!(
                mapped.status().wire(),
                match expected {
                    "absent" => "absent",
                    "denied" => "denied_or_locked",
                    "malformed" => "malformed",
                    "mismatch" => "binding_mismatch",
                    "unavailable" => "owner_unavailable",
                    "unauthorized" => "owner_unauthorized",
                    _ => unreachable!(),
                }
            );
        }
    }

    /// Pins the three independent copies of the model-id/catalog bounds against each other. Config,
    /// `rhapsody-provider-status` and `rhapsody-provider-broker` each keep a local constant to avoid
    /// a dependency edge; this is the one crate that depends on all three, so it is where drift is
    /// caught.
    #[test]
    fn model_id_and_catalog_bounds_agree_across_crates() {
        assert_eq!(
            rhapsody_config::MODEL_ID_MAX_BYTES,
            rhapsody_provider_status::catalog::MAX_MODEL_ID_BYTES
        );
        assert_eq!(
            rhapsody_config::MODEL_ID_MAX_BYTES,
            rhapsody_provider_broker::catalog::MAX_MODEL_ID_BYTES
        );
        assert_eq!(
            rhapsody_provider_status::catalog::MAX_CATALOG_BODY_BYTES,
            rhapsody_provider_broker::catalog::MAX_CATALOG_BODY_BYTES
        );
        assert_eq!(
            rhapsody_provider_status::catalog::MAX_CATALOG_ENTRIES,
            rhapsody_provider_broker::catalog::MAX_CATALOG_ENTRIES
        );
    }
}
