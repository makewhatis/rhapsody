//! The off-loop, concurrency-bounded provider refresh coordinator (STUDIO-990, §P9/§6).
//!
//! Every credentialed operation lives here and nowhere else. `GET` handlers call the pure
//! [`RefreshCoordinator::status_view`]/[`RefreshCoordinator::catalog_view`] reads, which touch the
//! caches only. A reload, a credential mutation, or an explicit operator refresh calls
//! [`RefreshCoordinator::apply_reload`] / [`RefreshCoordinator::begin_catalog_refresh`] to obtain an
//! intent, which the daemon schedules with [`RefreshCoordinator::spawn_status_refresh`].
//!
//! # Guarantees the tests pin
//!
//! * A status refresh reads through [`CredentialReadSource::read_bound`] for the provider's CURRENT
//!   canonical binding and publishes through the compare-and-swap cache, so a reordered completion
//!   cannot overwrite newer state.
//! * A catalog refresh reads through `read_bound` first; on a binding mismatch it performs NO provider
//!   I/O and publishes the mismatch.
//! * Concurrent catalog refreshes are bounded by a semaphore and by a per-provider in-flight set: a
//!   second explicit refresh for a provider already refreshing is refused with `InFlight`.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use rhapsody_credential_ipc::domain::{
    Binding, BoundCredentialLease, CredentialRef, CredentialStateTag, Revision,
};
use tokio::sync::Semaphore;

use crate::catalog::{CatalogCache, CatalogKey, CatalogSnapshot};
use crate::discovery::{DiscoveryRequest, ModelDiscovery};
use crate::error::CatalogError;
use crate::status::{
    CredentialStatus, ObservedStatus, ProviderBinding, ProviderStatusCache, ProviderStatusView,
    RefreshIntent,
};

/// The most concurrent provider refreshes the coordinator will run. Bounds the off-loop work a burst
/// of reloads/mutations can schedule.
pub const MAX_CONCURRENT_REFRESHES: usize = 4;

/// The one clock the coordinator reads, in epoch milliseconds. Injected so tests are deterministic.
pub type NowFn = Arc<dyn Fn() -> u64 + Send + Sync>;

fn system_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

/// The owner read outcome, with the lease when the owner answered `Present`. This mirrors
/// `rhapsody_credential_ipc::domain::CredentialState`, but is `Send`-friendly for the async
/// coordinator and carries the two independent counters its publication CAS needs.
#[derive(Debug)]
pub enum ObservedState {
    Present(BoundCredentialLease),
    Absent,
    DeniedOrLocked,
    Malformed,
    BindingMismatch,
    OwnerUnavailable,
    OwnerUnauthorized,
}

impl ObservedState {
    fn tag(&self) -> CredentialStateTag {
        match self {
            ObservedState::Present(_) => CredentialStateTag::Present,
            ObservedState::Absent => CredentialStateTag::Absent,
            ObservedState::DeniedOrLocked => CredentialStateTag::DeniedOrLocked,
            ObservedState::Malformed => CredentialStateTag::Malformed,
            ObservedState::BindingMismatch => CredentialStateTag::BindingMismatch,
            ObservedState::OwnerUnavailable => CredentialStateTag::OwnerUnavailable,
            ObservedState::OwnerUnauthorized => CredentialStateTag::OwnerUnauthorized,
        }
    }

    /// The non-secret status this read maps to.
    pub fn status(&self) -> CredentialStatus {
        CredentialStatus::from_tag(self.tag())
    }

    /// Whether an owner actually answered — `false` for the two availability failures, whose
    /// `owner_revision` is `Revision::INITIAL` and carries no owner revision to compare.
    pub fn answered(&self) -> bool {
        !matches!(
            self,
            ObservedState::OwnerUnavailable | ObservedState::OwnerUnauthorized
        )
    }
}

/// One completed owner read: the state plus the owner revision and availability generation. This is
/// the coordinator's view of `rhapsodyd`'s `ObservedRead`, decoupled from that crate so the
/// coordinator is testable in memory.
#[derive(Debug)]
pub struct ObservedRead {
    pub state: ObservedState,
    pub owner_revision: Revision,
    pub availability_generation: Revision,
}

/// The credential-read seam: the coordinator asks an adapter (the daemon's `CredentialResolver`) to
/// read a bound credential for one provider. Never called on a request path.
#[async_trait]
pub trait CredentialReadSource: Send + Sync {
    async fn read_bound(&self, account: String, binding: Binding) -> ObservedRead;
}

/// One provider's configuration the coordinator needs: its id, canonical binding, and endpoint TLS
/// policy. Built from `rhapsody_config`'s `ProviderDefinition` by the composition root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderConfig {
    pub provider_id: String,
    pub binding: Binding,
    pub allow_insecure_http: bool,
}

/// The coordinator. Shared behind an `Arc`; every handler and the off-loop tasks hold the same one.
pub struct RefreshCoordinator {
    status: Mutex<ProviderStatusCache>,
    catalog: Mutex<CatalogCache>,
    providers: Mutex<BTreeMap<String, ProviderConfig>>,
    source: Arc<dyn CredentialReadSource>,
    discovery: Arc<dyn ModelDiscovery>,
    now: NowFn,
    permits: Arc<Semaphore>,
    catalog_in_flight: Arc<Mutex<BTreeSet<String>>>,
}

impl RefreshCoordinator {
    pub fn new(
        source: Arc<dyn CredentialReadSource>,
        discovery: Arc<dyn ModelDiscovery>,
    ) -> Arc<Self> {
        Self::with_now(source, discovery, Arc::new(system_millis))
    }

    pub fn with_now(
        source: Arc<dyn CredentialReadSource>,
        discovery: Arc<dyn ModelDiscovery>,
        now: NowFn,
    ) -> Arc<Self> {
        Arc::new(Self {
            status: Mutex::new(ProviderStatusCache::new()),
            catalog: Mutex::new(CatalogCache::new()),
            providers: Mutex::new(BTreeMap::new()),
            source,
            discovery,
            now,
            permits: Arc::new(Semaphore::new(MAX_CONCURRENT_REFRESHES)),
            catalog_in_flight: Arc::new(Mutex::new(BTreeSet::new())),
        })
    }

    fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
        mutex
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn now_ms(&self) -> u64 {
        (self.now)()
    }

    /// Apply a provider set at `generation` (config load or reload). Returns the status-refresh
    /// intents the daemon must schedule; an unchanged set produces none.
    pub fn apply_reload(
        &self,
        generation: u64,
        providers: &[ProviderConfig],
    ) -> Vec<RefreshIntent> {
        let now = self.now_ms();
        {
            let mut map = Self::lock(&self.providers);
            map.clear();
            for provider in providers {
                map.insert(provider.provider_id.clone(), provider.clone());
            }
        }
        let bindings: Vec<ProviderBinding> = providers
            .iter()
            .map(|p| ProviderBinding {
                provider_id: p.provider_id.clone(),
                binding: p.binding.clone(),
            })
            .collect();
        let intents = Self::lock(&self.status).apply_reload(generation, &bindings, now);
        // A definition reload invalidates the affected catalog so a stale list is never served.
        Self::lock(&self.catalog).invalidate_all();
        intents
    }

    /// A credential mutation for one provider: invalidate its status to unknown/refreshing and
    /// schedule one replacement refresh, and drop its catalog. `None` when the provider is unknown.
    pub fn begin_mutation_refresh(&self, provider_id: &str) -> Option<RefreshIntent> {
        Self::lock(&self.catalog).invalidate(provider_id);
        Self::lock(&self.status).begin_refresh(provider_id, true)
    }

    /// An explicit operator refresh: keep the last known status but mark it refreshing and schedule
    /// one replacement refresh.
    pub fn begin_explicit_status_refresh(&self, provider_id: &str) -> Option<RefreshIntent> {
        Self::lock(&self.status).begin_refresh(provider_id, false)
    }

    /// Whether `provider_id` is one of the currently configured providers.
    pub fn knows(&self, provider_id: &str) -> bool {
        Self::lock(&self.providers).contains_key(provider_id)
    }

    /// The pure, cache-only status read.
    pub fn status_view(
        &self,
        provider_id: &str,
        broker_available: bool,
    ) -> Option<ProviderStatusView> {
        let now = self.now_ms();
        Self::lock(&self.status).read(provider_id, now, broker_available)
    }

    /// Every tracked provider's status view, in id order.
    pub fn status_views(&self, broker_available: bool) -> Vec<ProviderStatusView> {
        let now = self.now_ms();
        let cache = Self::lock(&self.status);
        cache
            .ids()
            .into_iter()
            .filter_map(|id| cache.read(&id, now, broker_available))
            .collect()
    }

    /// The pure, cache-only catalog read.
    pub fn catalog_view(&self, provider_id: &str) -> Option<CatalogSnapshot> {
        let now = self.now_ms();
        Self::lock(&self.catalog).read(provider_id, now)
    }

    /// Run one status refresh off-loop and publish its result through the CAS cache. This is the only
    /// path that calls [`CredentialReadSource::read_bound`] for status.
    pub async fn refresh_status(&self, intent: &RefreshIntent) {
        let observed = self
            .source
            .read_bound(intent.account.clone(), intent.binding.clone())
            .await;
        let publish = ObservedStatus {
            status: observed.state.status(),
            owner_revision: observed.owner_revision,
            availability_generation: observed.availability_generation,
            answered: observed.state.answered(),
        };
        Self::lock(&self.status).publish(&intent.token, &publish, self.now_ms());
    }

    /// Spawn a bounded status refresh. The semaphore bounds total concurrency; the CAS cache bounds
    /// correctness. The task is detached on purpose — a `GET` never waits on it.
    pub fn spawn_status_refresh(self: &Arc<Self>, intent: RefreshIntent) {
        let coordinator = Arc::clone(self);
        tokio::spawn(async move {
            let _permit = coordinator.permits.acquire().await;
            coordinator.refresh_status(&intent).await;
        });
    }

    /// Run one catalog refresh for `provider_id`. Calls `read_bound` for the CURRENT canonical
    /// endpoint first; on a binding mismatch it performs NO provider I/O. Updates the catalog cache
    /// (or its bounded error) under the cache key.
    pub async fn refresh_catalog(
        &self,
        provider_id: &str,
    ) -> Result<CatalogSnapshot, CatalogError> {
        let config = match Self::lock(&self.providers).get(provider_id).cloned() {
            Some(config) => config,
            None => return Err(CatalogError::Unsupported),
        };
        let Some(guard) = self.begin_catalog_in_flight(provider_id) else {
            return Err(CatalogError::InFlight);
        };
        let account = CredentialRef::for_provider(provider_id)
            .map(|r| r.account().to_string())
            .unwrap_or_default();
        // The generation this refresh is scoped to. If a definition reload moves it while the
        // discovery request is in flight, `apply_reload` has already invalidated every catalog; a
        // late publish must not resurrect a stale list keyed to the old generation.
        let generation = Self::lock(&self.status).generation();
        let observed = self
            .source
            .read_bound(account, config.binding.clone())
            .await;
        let key = CatalogKey {
            provider_id: provider_id.to_string(),
            endpoint: config.binding.base_url.clone(),
            generation,
            credential_revision: observed.owner_revision.0,
        };
        let result = match observed.state {
            ObservedState::Present(lease) => {
                let request = DiscoveryRequest {
                    endpoint: config.binding.base_url.clone(),
                    allow_insecure_http: config.allow_insecure_http,
                    lease,
                };
                self.discovery.list_models(request).await
            }
            // No provider I/O on a binding mismatch, and none without a usable credential.
            ObservedState::BindingMismatch => Err(CatalogError::BindingMismatch),
            _ => Err(CatalogError::NoCredential),
        };
        // Re-check AFTER the discovery await: a definition reload that raced this request already
        // invalidated every catalog, so publishing here would resurrect a list keyed to the old
        // generation. Leave the cache invalidated and report the unknown state instead.
        if Self::lock(&self.status).generation() != generation {
            return Ok(unknown_catalog(provider_id));
        }
        let mut catalog = Self::lock(&self.catalog);
        catalog.publish(key, result, self.now_ms());
        let snapshot = catalog
            .read(provider_id, self.now_ms())
            .ok_or(CatalogError::Malformed)?;
        drop(catalog);
        drop(guard);
        Ok(snapshot)
    }

    /// Spawn a bounded catalog refresh (the explicit operator POST path).
    pub fn spawn_catalog_refresh(self: &Arc<Self>, provider_id: String) {
        let coordinator = Arc::clone(self);
        tokio::spawn(async move {
            let _permit = coordinator.permits.acquire().await;
            let _ = coordinator.refresh_catalog(&provider_id).await;
        });
    }

    fn begin_catalog_in_flight(&self, provider_id: &str) -> Option<CatalogInFlightGuard> {
        let mut set = Self::lock(&self.catalog_in_flight);
        if !set.insert(provider_id.to_string()) {
            return None;
        }
        Some(CatalogInFlightGuard {
            provider_id: provider_id.to_string(),
            in_flight: Arc::clone(&self.catalog_in_flight),
        })
    }
}

/// The empty, unknown-aged catalog a provider serves before its first successful refresh — and the
/// answer a refresh whose definition reloaded mid-flight returns instead of resurrecting a stale
/// list.
fn unknown_catalog(provider_id: &str) -> CatalogSnapshot {
    CatalogSnapshot {
        provider_id: provider_id.to_string(),
        models: Vec::new(),
        truncated: false,
        cache_age_ms: None,
        error: None,
        error_message: None,
        manual_entry_allowed: true,
    }
}

/// Holds one provider's catalog in-flight slot and releases it on drop — including when the refresh
/// future is cancelled at an `.await`, which is what keeps a dropped request from wedging the
/// provider permanently.
struct CatalogInFlightGuard {
    provider_id: String,
    in_flight: Arc<Mutex<BTreeSet<String>>>,
}

impl Drop for CatalogInFlightGuard {
    fn drop(&mut self) {
        let mut set = RefreshCoordinator::lock(&self.in_flight);
        set.remove(&self.provider_id);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use crate::catalog::ModelEntry;
    use crate::discovery::DiscoveredCatalog;
    use rhapsody_credential_ipc::domain::OPENAI_CHAT_COMPLETIONS_BEARER_V1;

    fn binding(url: &str) -> Binding {
        Binding {
            provider_id: "fireworks".into(),
            adapter: OPENAI_CHAT_COMPLETIONS_BEARER_V1.into(),
            base_url: url.into(),
        }
    }

    fn config(url: &str) -> ProviderConfig {
        ProviderConfig {
            provider_id: "fireworks".into(),
            binding: binding(url),
            allow_insecure_http: false,
        }
    }

    /// A scripted source: returns a fixed read, and counts how many times it was called so a test
    /// can prove a path performed (or did not perform) owner I/O.
    struct FakeSource {
        state: Mutex<ObservedState>,
        revision: Revision,
        calls: AtomicU64,
    }

    impl FakeSource {
        fn new(state: ObservedState, revision: u64) -> Arc<Self> {
            Arc::new(Self {
                state: Mutex::new(state),
                revision: Revision(revision),
                calls: AtomicU64::new(0),
            })
        }
    }

    #[async_trait]
    impl CredentialReadSource for FakeSource {
        async fn read_bound(&self, _account: String, _binding: Binding) -> ObservedRead {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let state = std::mem::replace(
                &mut *self.state.lock().unwrap_or_else(|e| e.into_inner()),
                ObservedState::Absent,
            );
            ObservedRead {
                state,
                owner_revision: self.revision,
                availability_generation: Revision::INITIAL,
            }
        }
    }

    struct FakeDiscovery {
        calls: AtomicU64,
        result: Mutex<Result<DiscoveredCatalog, CatalogError>>,
    }

    impl FakeDiscovery {
        fn ok() -> Arc<Self> {
            Arc::new(Self {
                calls: AtomicU64::new(0),
                result: Mutex::new(Ok(DiscoveredCatalog {
                    entries: vec![ModelEntry {
                        id: "m".into(),
                        display_name: None,
                        capabilities: Vec::new(),
                    }],
                    truncated: false,
                })),
            })
        }
    }

    #[async_trait]
    impl ModelDiscovery for FakeDiscovery {
        async fn list_models(
            &self,
            _request: DiscoveryRequest,
        ) -> Result<DiscoveredCatalog, CatalogError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let mut slot = self.result.lock().unwrap_or_else(|e| e.into_inner());
            std::mem::replace(&mut *slot, Err(CatalogError::Malformed)).clone()
        }
    }

    fn owned_lease() -> BoundCredentialLease {
        BoundCredentialLease::new(binding("https://api.example/v1"), "sk-fake".into())
    }

    fn fixed_now(ms: u64) -> NowFn {
        Arc::new(move || ms)
    }

    #[tokio::test]
    async fn reload_issues_intents_and_status_refresh_publishes() {
        let source = FakeSource::new(ObservedState::Present(owned_lease()), 4);
        let coordinator =
            RefreshCoordinator::with_now(source, FakeDiscovery::ok(), fixed_now(1_000));
        let intents = coordinator.apply_reload(1, &[config("https://api.example/v1")]);
        assert_eq!(intents.len(), 1);
        coordinator.refresh_status(&intents[0]).await;
        let view = coordinator.status_view("fireworks", true).expect("tracked");
        assert_eq!(view.status, "configured");
        assert!(!view.refreshing);
    }

    // MUTATION GUARD (cache-only read never performs owner I/O): after a reload but BEFORE any
    // refresh, reading status does not call the source. An implementation whose read triggered a
    // read_bound would red this.
    #[tokio::test]
    async fn a_status_read_never_calls_the_source() {
        let source = FakeSource::new(ObservedState::Absent, 0);
        let coordinator =
            RefreshCoordinator::with_now(source.clone(), FakeDiscovery::ok(), fixed_now(0));
        let _ = coordinator.apply_reload(1, &[config("https://api.example/v1")]);
        let before = source.calls.load(Ordering::SeqCst);
        let _ = coordinator.status_view("fireworks", true);
        let _ = coordinator.status_views(true);
        let _ = coordinator.catalog_view("fireworks");
        assert_eq!(
            source.calls.load(Ordering::SeqCst),
            before,
            "a GET must never read the credential owner"
        );
    }

    // MUTATION GUARD (no provider I/O on binding mismatch): a catalog refresh whose read is a
    // mismatch must publish the mismatch and call the discovery adapter ZERO times.
    #[tokio::test]
    async fn a_catalog_binding_mismatch_performs_no_provider_io() {
        let source = FakeSource::new(ObservedState::BindingMismatch, 9);
        let discovery = FakeDiscovery::ok();
        let coordinator = RefreshCoordinator::with_now(
            source,
            Arc::clone(&discovery) as Arc<dyn ModelDiscovery>,
            fixed_now(0),
        );
        let _ = coordinator.apply_reload(1, &[config("https://api.example/v1")]);
        let snap = coordinator
            .refresh_catalog("fireworks")
            .await
            .expect("snapshot");
        assert_eq!(
            snap.error,
            Some(crate::catalog::CatalogErrorCode::BindingMismatch)
        );
        assert_eq!(discovery.calls.load(Ordering::SeqCst), 0, "no provider I/O");
    }

    #[tokio::test]
    async fn a_catalog_refresh_with_a_credential_fetches_and_caches() {
        let source = FakeSource::new(ObservedState::Present(owned_lease()), 1);
        let discovery = FakeDiscovery::ok();
        let coordinator = RefreshCoordinator::with_now(
            source,
            Arc::clone(&discovery) as Arc<dyn ModelDiscovery>,
            fixed_now(0),
        );
        let _ = coordinator.apply_reload(1, &[config("https://api.example/v1")]);
        let snap = coordinator
            .refresh_catalog("fireworks")
            .await
            .expect("snapshot");
        assert_eq!(snap.models.len(), 1);
        assert!(snap.error.is_none());
        assert_eq!(discovery.calls.load(Ordering::SeqCst), 1);
    }

    /// A discovery whose single call blocks until the test releases it, so a definition reload can
    /// race the in-flight discovery deterministically.
    struct GatedDiscovery {
        entered: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }

    #[async_trait]
    impl ModelDiscovery for GatedDiscovery {
        async fn list_models(
            &self,
            _request: DiscoveryRequest,
        ) -> Result<DiscoveredCatalog, CatalogError> {
            self.entered.notify_one();
            self.release.notified().await;
            Ok(DiscoveredCatalog {
                entries: vec![ModelEntry {
                    id: "stale".into(),
                    display_name: None,
                    capabilities: Vec::new(),
                }],
                truncated: false,
            })
        }
    }

    // MUTATION GUARD (a reload invalidates the old catalog; a late completion may not resurrect it):
    // a refresh whose definition reloads mid-flight returns the unknown state and does NOT cache the
    // stale list. An implementation without the generation check would re-insert `stale`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_reload_during_a_catalog_refresh_is_not_resurrected() {
        let source = FakeSource::new(ObservedState::Present(owned_lease()), 1);
        let gated = Arc::new(GatedDiscovery {
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        let coordinator = RefreshCoordinator::with_now(
            source,
            Arc::clone(&gated) as Arc<dyn ModelDiscovery>,
            fixed_now(0),
        );
        let _ = coordinator.apply_reload(1, &[config("https://api.example/v1")]);
        let runner = Arc::clone(&coordinator);
        let task = tokio::spawn(async move { runner.refresh_catalog("fireworks").await });
        // Wait until the discovery call is in flight, then reload the definition.
        gated.entered.notified().await;
        let _ = coordinator.apply_reload(2, &[config("https://api.example/v1")]);
        gated.release.notify_one();
        let snapshot = task.await.expect("join").expect("snapshot");
        assert!(snapshot.models.is_empty(), "the stale list was resurrected");
        assert!(snapshot.cache_age_ms.is_none());
        assert!(
            coordinator.catalog_view("fireworks").is_none(),
            "the reload invalidated the catalog and it stayed invalidated"
        );
    }

    #[tokio::test]
    async fn concurrent_catalog_refreshes_are_refused_while_one_is_in_flight() {
        let source = FakeSource::new(ObservedState::Present(owned_lease()), 1);
        let coordinator = RefreshCoordinator::with_now(source, FakeDiscovery::ok(), fixed_now(0));
        let _ = coordinator.apply_reload(1, &[config("https://api.example/v1")]);
        // Hold the in-flight slot manually to simulate a long refresh.
        let guard = coordinator
            .begin_catalog_in_flight("fireworks")
            .expect("first in-flight");
        assert_eq!(
            coordinator.refresh_catalog("fireworks").await,
            Err(CatalogError::InFlight)
        );
        drop(guard);
        // Once released, a refresh proceeds.
        assert!(coordinator.refresh_catalog("fireworks").await.is_ok());
    }
}
