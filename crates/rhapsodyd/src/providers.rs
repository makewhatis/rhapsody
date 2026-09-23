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

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use rhapsody_agent::{
    PreparedProvider, ResolvedProviderPlan, lower_provider_limits, lower_provider_plan,
};
use rhapsody_config::{
    Config, ProviderDefinition, ProviderReload, providers::provider_turn_deadline_ms,
};
use rhapsody_credential_ipc::domain::{Binding, CredentialRef, CredentialState};
use rhapsody_orchestrator::{
    OpenedProvider, PreparedProviderSource, ProviderRefusal, ProviderReloadSink, RefusalReason,
};
use rhapsody_provider_broker::{BrokerError, BrokerRegistrar, SessionPolicy};
use rhapsody_provider_status::{
    CatalogError, CatalogSnapshot, CredentialReadSource, ObservedRead, ObservedState,
    OpenAiCompatibleDiscovery, ProviderConfig, ProviderStatusView, RefreshCoordinator,
};

use crate::credential_client::CredentialResolver;

/// Test seam over the daemon's provider-status wiring (STUDIO-990, P9). Production passes `None`;
/// the integration tests inject an observer that captures the live [`ProviderRuntime`] once the
/// composition root has built it, so the boot apply and the hot-reload apply are exercised as wired
/// rather than only as units.
pub(crate) struct ProviderSeam {
    /// Called once, immediately after the runtime is built and its reload sink installed.
    pub observe: Option<Box<dyn FnOnce(ProviderObservation) + Send>>,
}

/// The live provider runtime a [`ProviderSeam`] observer receives (STUDIO-990, P9). Only the
/// integration tests read it, so the non-test lib target has no reader: `allow(dead_code)` is scoped
/// to exactly that build.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct ProviderObservation {
    pub runtime: Arc<ProviderRuntime>,
}

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
    pub fn apply_config(&self, config: &Config) -> usize {
        self.apply_providers(
            &config.providers,
            provider_turn_deadline_ms(config.opencode.turn_timeout_ms),
        )
    }

    /// Apply a provider set (config load OR hot reload). `turn_deadline_ms` scopes each provider's
    /// derived capability lifetime, so it participates in the [`ProviderReload`] generation exactly
    /// as it does at boot. An EMPTY set is applied too — a reload that removes the last provider must
    /// drop every tracked status, not keep serving the boot-time map.
    pub fn apply_providers(
        &self,
        providers: &BTreeMap<String, ProviderDefinition>,
        turn_deadline_ms: u64,
    ) -> usize {
        let configs = provider_configs(providers);
        let generation = ProviderReload::from_providers(providers, turn_deadline_ms).revision();
        let intents = self.coordinator.apply_reload(generation, &configs);
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
    ///
    /// `knows` is checked FIRST: a provider the current config no longer defines is a 404 even if a
    /// cache entry somehow survived a removal. That is defence in depth for a refresh that publishes
    /// after its provider was removed — the coordinator already refuses that publish, and the second
    /// check keeps the route honest if it ever slips through.
    pub fn catalog(&self, provider_id: &str) -> Option<CatalogSnapshot> {
        if !self.coordinator.knows(provider_id) {
            return None;
        }
        if let Some(snapshot) = self.coordinator.catalog_view(provider_id) {
            return Some(snapshot);
        }
        Some(CatalogSnapshot {
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
fn provider_configs(providers: &BTreeMap<String, ProviderDefinition>) -> Vec<ProviderConfig> {
    let mut out = Vec::with_capacity(providers.len());
    for (id, def) in providers {
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

/// The runtime is the orchestrator's provider-set reload sink (STUDIO-990, P9): on every successful
/// WORKFLOW.md (re)load the control task hands it the resolved `providers:` map, and this applies it
/// through the same [`ProviderRuntime::apply_providers`] the boot path uses. Synchronous and
/// in-memory only — the credentialed work it schedules is spawned off-loop by the coordinator.
impl ProviderReloadSink for ProviderRuntime {
    fn provider_reload(
        &self,
        providers: &BTreeMap<String, ProviderDefinition>,
        turn_deadline_ms: u64,
    ) {
        let scheduled = self.apply_providers(providers, turn_deadline_ms);
        if scheduled > 0 {
            tracing::info!(
                providers = scheduled,
                "scheduled provider status refreshes after a workflow reload"
            );
        }
    }
}

/// A fresh resolver that never learned a bootstrap frame — the daemon's owner adapter when
/// `--credential-bootstrap` was not passed. It resolves `OwnerUnavailable` consistently.
pub fn unavailable_owner() -> Arc<CredentialResolver> {
    Arc::new(CredentialResolver::new())
}

/// The daemon's prepared-provider source (PB7, STUDIO-1002): the off-loop operation that reads the
/// bound credential through the daemon's authenticated [`CredentialResolver`] and registers the
/// pure plan with the live broker, returning the move-only [`PreparedProvider`].
///
/// It is the ONLY place the daemon's credential boundary and the broker meet, and every failure is
/// mapped to a typed [`RefusalReason`] — a missing/locked/malformed credential, an owner that is
/// unavailable or refuses us, a binding mismatch, or a broker that is down. It never reads the
/// credential value itself beyond moving it into the broker, so no secret leaves this call.
pub struct DaemonProviderSource {
    resolver: Arc<CredentialResolver>,
    registrar: BrokerRegistrar,
}

impl DaemonProviderSource {
    pub fn new(resolver: Arc<CredentialResolver>, registrar: BrokerRegistrar) -> Self {
        Self {
            resolver,
            registrar,
        }
    }
}

fn provider_refusal(reason: RefusalReason, revision: &str) -> ProviderRefusal {
    ProviderRefusal {
        reason,
        revision: revision.to_string(),
    }
}

#[async_trait]
impl PreparedProviderSource for DaemonProviderSource {
    async fn open_provider(
        &self,
        plan: &ResolvedProviderPlan,
    ) -> Result<OpenedProvider, ProviderRefusal> {
        let binding = Binding {
            provider_id: plan.stable_id.clone(),
            adapter: plan.protocol.adapter_id().to_string(),
            base_url: plan.normalized_endpoint.clone(),
        };
        let account = CredentialRef::for_provider(&plan.stable_id)
            .map_or_else(|_| String::new(), |r| r.account().to_string());
        let observed = self.resolver.read_bound(account, binding).await;
        let revision = observed.read.revision.0.to_string();
        match observed.read.state {
            CredentialState::Present(lease) => {
                // Move the value into the broker's own move-only lease. A shape violation is a
                // malformed stored credential, never a retried direct-key path.
                let broker_lease = match lease.into_broker_lease() {
                    Ok(lease) => lease,
                    Err(BrokerError::InvalidCredential(_) | BrokerError::InvalidBinding) => {
                        return Err(provider_refusal(
                            RefusalReason::CredentialMalformed,
                            &revision,
                        ));
                    }
                    Err(e) => {
                        return Err(provider_refusal(
                            RefusalReason::ResolverFailed(e.to_string()),
                            &revision,
                        ));
                    }
                };
                let registration_plan = match lower_provider_plan(plan) {
                    Ok(plan) => plan,
                    Err(e) => {
                        return Err(provider_refusal(
                            RefusalReason::ResolverFailed(e.to_string()),
                            &revision,
                        ));
                    }
                };
                let policy = match SessionPolicy::new(lower_provider_limits(&plan.limits)) {
                    Ok(policy) => policy,
                    Err(e) => {
                        return Err(provider_refusal(
                            RefusalReason::ResolverFailed(e.to_string()),
                            &revision,
                        ));
                    }
                };
                match self
                    .registrar
                    .register_session(registration_plan, broker_lease, policy)
                {
                    Ok(registration) => Ok(OpenedProvider {
                        provider: PreparedProvider::from_registration(
                            plan.stable_id.clone(),
                            plan.protocol,
                            registration,
                        ),
                        revision,
                    }),
                    Err(BrokerError::Unavailable) => Err(provider_refusal(
                        RefusalReason::ProviderBrokerUnavailable,
                        &revision,
                    )),
                    Err(BrokerError::BindingMismatch) => {
                        Err(provider_refusal(RefusalReason::BindingMismatch, &revision))
                    }
                    Err(e) => Err(provider_refusal(
                        RefusalReason::ResolverFailed(e.to_string()),
                        &revision,
                    )),
                }
            }
            CredentialState::Absent => {
                Err(provider_refusal(RefusalReason::CredentialAbsent, &revision))
            }
            CredentialState::DeniedOrLocked => Err(provider_refusal(
                RefusalReason::CredentialDeniedOrLocked,
                &revision,
            )),
            CredentialState::Malformed => Err(provider_refusal(
                RefusalReason::CredentialMalformed,
                &revision,
            )),
            CredentialState::BindingMismatch => {
                Err(provider_refusal(RefusalReason::BindingMismatch, &revision))
            }
            CredentialState::OwnerUnavailable => {
                Err(provider_refusal(RefusalReason::OwnerUnavailable, &revision))
            }
            CredentialState::OwnerUnauthorized => Err(provider_refusal(
                RefusalReason::OwnerUnauthorized,
                &revision,
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rhapsody_config::providers::CREDENTIAL_SOURCE_KEYCHAIN;
    use rhapsody_credential_ipc::domain::OPENAI_CHAT_COMPLETIONS_BEARER_V1;

    /// One configured provider definition, built directly (no WORKFLOW.md needed).
    fn provider_definition(id: &str) -> ProviderDefinition {
        ProviderDefinition {
            id: id.to_string(),
            protocol: rhapsody_config::PROTOCOL_OPENAI_COMPATIBLE.to_string(),
            display_name: String::new(),
            base_url: format!("https://{id}.example/v1"),
            allow_insecure_http: false,
            credential: rhapsody_config::CredentialSource {
                source: CREDENTIAL_SOURCE_KEYCHAIN.to_string(),
            },
            broker_limits: rhapsody_config::BrokerLimits::default(),
        }
    }

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

    /// STUDIO-990 (P9), B2: the `broker_available` field must report PB4's LIVE broker state on
    /// every status, not a hardcoded `true`. The registrar observes the same shared broker the
    /// supervisor flips on an unexpected serving-task exit.
    ///
    /// MUTATION GUARD: hardcode `status_views(true)` (or drop `broker_available` from the view) and
    /// this reds.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unavailable_broker_is_reported_on_every_status() {
        let runtime = crate::broker::BrokerRuntime::bind().expect("bind the loopback broker");
        let registrar = runtime.registrar();
        // Flip the shared broker unavailable: the registrar observes the same state.
        runtime.broker_handle().mark_unavailable();
        assert!(
            !registrar.is_available(),
            "the fixture broker must read unavailable"
        );

        let provider_runtime = ProviderRuntime::new(unavailable_owner(), registrar);
        let providers =
            BTreeMap::from([("fireworks".to_string(), provider_definition("fireworks"))]);
        assert_eq!(
            provider_runtime.apply_providers(&providers, 3_600_000),
            1,
            "the new provider must schedule exactly one status refresh"
        );

        for _ in 0..500 {
            if let Some(view) = provider_runtime.status("fireworks")
                && !view.refreshing
                && view.cache_age_ms.is_some()
            {
                assert!(
                    !view.broker_available,
                    "a down broker must be reported unavailable on the status"
                );
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!(
            "status did not converge; statuses={:?}",
            provider_runtime.statuses()
        );
    }

    /// STUDIO-990 (P9), J1 removal variant: a reload that removes the last provider must drop its
    /// status AND its catalog, so `GET /api/v1/providers/{id}` and `.../{id}/models` stop answering
    /// for an id the current config no longer defines.
    ///
    /// MUTATION GUARD: restoring an `if providers.is_empty() { return; }` early return in
    /// `apply_providers` would skip the reload, keep the removed provider alive, and red this.
    /// (`catalog`'s `knows` check is defence in depth on top of this — the coordinator already
    /// refuses a publish that raced a removal, so that ordering is not separately observable here.)
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn removing_the_last_provider_drops_its_status_and_catalog() {
        let broker = crate::broker::BrokerRuntime::bind().expect("bind the loopback broker");
        let provider_runtime = ProviderRuntime::new(unavailable_owner(), broker.registrar());
        let providers =
            BTreeMap::from([("fireworks".to_string(), provider_definition("fireworks"))]);
        assert_eq!(
            provider_runtime.apply_providers(&providers, 3_600_000),
            1,
            "the new provider must schedule exactly one status refresh"
        );
        assert!(provider_runtime.status("fireworks").is_some());
        assert!(provider_runtime.catalog("fireworks").is_some());

        assert_eq!(
            provider_runtime.apply_providers(&BTreeMap::new(), 3_600_000),
            0,
            "an empty set schedules nothing"
        );
        assert!(
            provider_runtime.status("fireworks").is_none(),
            "a removed provider must not keep serving a status"
        );
        assert!(
            provider_runtime.catalog("fireworks").is_none(),
            "a removed provider must 404 its catalog, not serve a stale list"
        );
    }

    fn prepared_plan() -> rhapsody_agent::ResolvedProviderPlan {
        rhapsody_agent::ResolvedProviderPlan {
            stable_id: "fireworks".to_string(),
            protocol: rhapsody_agent::ProviderProtocol::OpenAiCompatible,
            normalized_endpoint: "https://fireworks.example/v1".to_string(),
            allow_insecure_http: false,
            credential_binding: String::new(),
            credential_ref: rhapsody_config::providers::CREDENTIAL_SOURCE_KEYCHAIN.to_string(),
            limits: rhapsody_agent::ProviderLimits::default(),
            model: "m".to_string(),
            origins: rhapsody_agent::ProviderOrigins::default(),
        }
    }

    /// PB7: with no credential owner channel, the prepared-provider source refuses with the typed
    /// `owner_unavailable` rather than ever reaching for a direct key. The mutation guard is
    /// inventing a lease for an owner that never answered.
    #[tokio::test]
    async fn prepared_source_refuses_when_the_owner_is_unavailable() {
        let runtime = crate::broker::BrokerRuntime::bind().expect("broker");
        let source = DaemonProviderSource::new(unavailable_owner(), runtime.registrar());
        let err = source
            .open_provider(&prepared_plan())
            .await
            .expect_err("no owner channel");
        assert_eq!(
            err.reason,
            rhapsody_orchestrator::RefusalReason::OwnerUnavailable
        );
    }
}
