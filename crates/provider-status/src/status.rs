//! The non-secret provider credential-status cache and its compare-and-swap refresh publication
//! (STUDIO-990, §P9/§6).
//!
//! # It never holds a secret
//!
//! A [`StatusEntry`] holds a [`CredentialStatus`] tag, the config generation, the *expected*
//! (non-secret) [`Binding`], the last owner revision, and the two publication timestamps. There is
//! deliberately no credential value, no stored binding fingerprint, and no opaque credential
//! revision: none of those may cross this boundary, and none is needed to answer a status `GET`.
//!
//! # Publication is compare-and-swap
//!
//! [`ProviderStatusCache::apply_reload`] and [`ProviderStatusCache::begin_refresh`] issue an
//! unforgeable [`RefreshToken`] for the affected provider; a completion may publish only through
//! [`ProviderStatusCache::publish`], which discards the result unless the token is still current, the
//! generation and expected binding still match, and the observed owner revision is not older than the
//! token's starting revision. That is what makes a reordered off-loop read unable to overwrite newer
//! state.

use std::collections::BTreeMap;

use rhapsody_credential_ipc::domain::{Binding, CredentialRef, CredentialStateTag, Revision};
use serde::Serialize;

/// The non-secret credential status a provider can be in (`provider-auth-design.md` §6). Closed: a
/// provider is always exactly one of these, and every one is actionable without revealing a secret.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialStatus {
    /// No credential is stored.
    Absent,
    /// An item may exist, but the owner is locked or refused access.
    DeniedOrLocked,
    /// A stored item could not be parsed as a v1 envelope.
    Malformed,
    /// No credential owner is reachable at all (no bootstrap channel / daemon-to-owner link down).
    OwnerUnavailable,
    /// An owner connected but rejected this daemon (unauthenticated).
    OwnerUnauthorized,
    /// A well-formed credential is stored and bound to the current endpoint.
    Configured,
    /// A well-formed credential is stored under a DIFFERENT binding than the current canonical one.
    /// Never discloses the stored binding; only the desktop can explicitly Rebind.
    BindingMismatch,
    /// The definition reloaded and a bounded off-loop refresh is scheduled/underway; the previous
    /// status is no longer asserted. Reported with `cache_age_ms == None` so an unknown state is
    /// never misread as absent.
    UnknownRefreshing,
}

impl CredentialStatus {
    /// The stable wire spelling (used in `GET /api/v1/providers` and asserted by tests).
    pub fn wire(self) -> &'static str {
        match self {
            CredentialStatus::Absent => "absent",
            CredentialStatus::DeniedOrLocked => "denied_or_locked",
            CredentialStatus::Malformed => "malformed",
            CredentialStatus::OwnerUnavailable => "owner_unavailable",
            CredentialStatus::OwnerUnauthorized => "owner_unauthorized",
            CredentialStatus::Configured => "configured",
            CredentialStatus::BindingMismatch => "binding_mismatch",
            CredentialStatus::UnknownRefreshing => "unknown_refreshing",
        }
    }

    /// The one closed recovery action a UI may offer, or `None` when the credential is usable.
    /// Deliberately a closed set so the dashboard never invents an action.
    pub fn recovery(self) -> Option<&'static str> {
        match self {
            CredentialStatus::Absent => Some("connect"),
            CredentialStatus::DeniedOrLocked => Some("unlock"),
            CredentialStatus::Malformed => Some("remove"),
            CredentialStatus::OwnerUnavailable | CredentialStatus::OwnerUnauthorized => {
                Some("open_desktop")
            }
            CredentialStatus::BindingMismatch => Some("rebind"),
            CredentialStatus::UnknownRefreshing => Some("refresh"),
            CredentialStatus::Configured => None,
        }
    }

    /// Map a raw owner read tag onto this status.
    pub fn from_tag(tag: CredentialStateTag) -> CredentialStatus {
        match tag {
            CredentialStateTag::Present => CredentialStatus::Configured,
            CredentialStateTag::Absent => CredentialStatus::Absent,
            CredentialStateTag::DeniedOrLocked => CredentialStatus::DeniedOrLocked,
            CredentialStateTag::Malformed => CredentialStatus::Malformed,
            CredentialStateTag::BindingMismatch => CredentialStatus::BindingMismatch,
            CredentialStateTag::OwnerUnavailable => CredentialStatus::OwnerUnavailable,
            CredentialStateTag::OwnerUnauthorized => CredentialStatus::OwnerUnauthorized,
        }
    }
}

/// One provider definition's non-secret binding, as the cache tracks it. Built by the caller from
/// `rhapsody_config`'s `ProviderDefinition::credential_binding()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderBinding {
    pub provider_id: String,
    pub binding: Binding,
}

/// The one closed reason code a provider status may carry for an unavailable broker: the design's
/// typed `provider_broker_unavailable` refusal (design §11.2, §13). No other value is ever produced,
/// and it carries no listener address, capability, credential, or upstream body.
pub const BROKER_UNAVAILABLE: &str = "provider_broker_unavailable";

/// The non-secret status view a `GET` observes. Serialized straight to the API body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProviderStatusView {
    pub provider_id: String,
    pub status: &'static str,
    /// Milliseconds since this status was published. `None` while the entry is
    /// [`CredentialStatus::UnknownRefreshing`] (never previously published) — an unknown state must
    /// never be misreported as a fresh "absent".
    pub cache_age_ms: Option<u64>,
    /// Whether a refresh is currently scheduled or underway for this provider.
    pub refreshing: bool,
    /// PB4's live broker availability (the daemon's one private broker). `false` means credentialed
    /// dispatch is currently refused with `provider_broker_unavailable`.
    pub broker_available: bool,
    /// The closed reason the broker is unavailable, or `None` when it is available (design §13).
    /// Only [`BROKER_UNAVAILABLE`] can ever appear — never an endpoint, capability, credential, or
    /// raw provider response. Derived from `broker_available`, so the two cannot disagree.
    pub broker_reason: Option<&'static str>,
    pub recovery: Option<&'static str>,
}

/// An unforgeable, non-serializable publication permit. The only constructor is
/// [`ProviderStatusCache`]'s internal issuance, so no client can present one as a browser-supplied
/// concurrency token — the design's "the refresh token is unforgeable" rule.
#[derive(Clone, PartialEq, Eq)]
pub struct RefreshToken {
    seq: u64,
    provider_id: String,
    generation: u64,
    binding: Binding,
    starting_owner_revision: Revision,
}

impl std::fmt::Debug for RefreshToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RefreshToken")
            .field("provider_id", &self.provider_id)
            .field("seq", &self.seq)
            .finish_non_exhaustive()
    }
}

/// A scheduled off-loop status refresh. Carries only non-secret values: the provider id, its derived
/// owner account, the canonical binding, and the token the completion must publish with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshIntent {
    pub provider_id: String,
    pub account: String,
    pub binding: Binding,
    pub token: RefreshToken,
}

impl RefreshIntent {
    /// The token to publish the completing read with.
    pub fn token(&self) -> &RefreshToken {
        &self.token
    }
}

/// One completing status read, handed to [`ProviderStatusCache::publish`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObservedStatus {
    pub status: CredentialStatus,
    /// The owner's own CAS revision when the owner answered, else [`Revision::INITIAL`].
    pub owner_revision: Revision,
    /// The daemon availability generation (advances on owner availability transitions).
    pub availability_generation: Revision,
    /// Whether an owner actually answered this read (so `owner_revision` is a real owner revision).
    pub answered: bool,
}

/// Whether a publication was applied; every discard is a named, testable reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Publication {
    Applied,
    /// No entry, or the provider's current token is not this one (a newer reload/mutation issued one).
    StaleToken,
    /// The config generation moved since the token was issued.
    StaleGeneration,
    /// The expected binding moved since the token was issued.
    StaleBinding,
    /// The observed owner revision is older than the token's starting revision.
    StaleRevision,
}

#[derive(Debug, Clone)]
struct StatusEntry {
    status: CredentialStatus,
    expected_binding: Binding,
    generation: u64,
    owner_revision: Revision,
    availability_generation: Revision,
    published_at_ms: Option<u64>,
    refreshing: bool,
}

/// The one non-secret status cache. Cloneable for tests; shared behind a lock by the coordinator.
#[derive(Debug, Default, Clone)]
pub struct ProviderStatusCache {
    generation: u64,
    next_token: u64,
    entries: BTreeMap<String, StatusEntry>,
    tokens: BTreeMap<String, RefreshToken>,
}

impl ProviderStatusCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// The currently applied config generation.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Whether the cache tracks `provider_id`.
    pub fn tracks(&self, provider_id: &str) -> bool {
        self.entries.contains_key(provider_id)
    }

    /// The number of tracked providers.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Every tracked provider id, in deterministic order.
    pub fn ids(&self) -> Vec<String> {
        self.entries.keys().cloned().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn issue_token(
        &mut self,
        provider_id: &str,
        binding: &Binding,
        generation: u64,
    ) -> RefreshToken {
        self.next_token = self.next_token.saturating_add(1);
        let starting_owner_revision = self
            .entries
            .get(provider_id)
            .map(|e| e.owner_revision)
            .unwrap_or(Revision::INITIAL);
        RefreshToken {
            seq: self.next_token,
            provider_id: provider_id.to_string(),
            generation,
            binding: binding.clone(),
            starting_owner_revision,
        }
    }

    /// Apply a provider set at `generation` (from a config load or reload). Providers that are new,
    /// whose canonical binding moved, or whose generation moved are set to
    /// [`CredentialStatus::UnknownRefreshing`] and receive exactly one refresh intent; providers that
    /// disappeared are dropped. An unchanged set therefore produces no intents.
    pub fn apply_reload(
        &mut self,
        generation: u64,
        providers: &[ProviderBinding],
        _now_ms: u64,
    ) -> Vec<RefreshIntent> {
        self.generation = generation;
        let live: std::collections::BTreeSet<&str> =
            providers.iter().map(|p| p.provider_id.as_str()).collect();
        self.entries.retain(|id, _| live.contains(id.as_str()));
        self.tokens.retain(|id, _| live.contains(id.as_str()));

        let mut intents = Vec::new();
        for provider in providers {
            let changed = match self.entries.get(&provider.provider_id) {
                None => true,
                Some(entry) => {
                    entry.expected_binding != provider.binding || entry.generation != generation
                }
            };
            if !changed {
                continue;
            }
            let token = self.issue_token(&provider.provider_id, &provider.binding, generation);
            self.entries.insert(
                provider.provider_id.clone(),
                StatusEntry {
                    status: CredentialStatus::UnknownRefreshing,
                    expected_binding: provider.binding.clone(),
                    generation,
                    owner_revision: Revision::INITIAL,
                    availability_generation: Revision::INITIAL,
                    published_at_ms: None,
                    refreshing: true,
                },
            );
            self.tokens
                .insert(provider.provider_id.clone(), token.clone());
            intents.push(intent_for(&provider.provider_id, &provider.binding, token));
        }
        intents
    }

    /// A credential mutation (Connect/Replace/Rebind/Remove) or an owner availability transition
    /// invalidates the provider's current status and schedules a replacement refresh. `reset_to_unknown`
    /// is `true` for a mutation that may have changed the stored binding (so the previous status must
    /// not be asserted) and `false` for an explicit operator refresh that keeps the last known status
    /// but marks it refreshing.
    pub fn begin_refresh(
        &mut self,
        provider_id: &str,
        reset_to_unknown: bool,
    ) -> Option<RefreshIntent> {
        let binding = self.entries.get(provider_id)?.expected_binding.clone();
        let generation = self.generation;
        let token = self.issue_token(provider_id, &binding, generation);
        if let Some(entry) = self.entries.get_mut(provider_id) {
            entry.refreshing = true;
            if reset_to_unknown {
                entry.status = CredentialStatus::UnknownRefreshing;
                entry.published_at_ms = None;
                entry.owner_revision = Revision::INITIAL;
                entry.availability_generation = Revision::INITIAL;
            }
        }
        self.tokens.insert(provider_id.to_string(), token.clone());
        Some(intent_for(provider_id, &binding, token))
    }

    /// Publish a completing read, compare-and-swap. See module docs for the exact discard rules.
    pub fn publish(
        &mut self,
        token: &RefreshToken,
        observed: &ObservedStatus,
        now_ms: u64,
    ) -> Publication {
        // The token must be the provider's CURRENT one: a reload/mutation that issued a newer token
        // invalidates this completion even before generation/binding are compared.
        match self.tokens.get(&token.provider_id) {
            Some(current) if current == token => {}
            _ => return Publication::StaleToken,
        }
        let Some(entry) = self.entries.get_mut(&token.provider_id) else {
            return Publication::StaleToken;
        };
        if entry.generation != token.generation {
            return Publication::StaleGeneration;
        }
        if entry.expected_binding != token.binding {
            return Publication::StaleBinding;
        }
        if observed.answered
            && (observed.owner_revision < token.starting_owner_revision
                || observed.owner_revision < entry.owner_revision)
        {
            return Publication::StaleRevision;
        }
        if observed.availability_generation < entry.availability_generation {
            return Publication::StaleRevision;
        }
        entry.status = observed.status;
        entry.owner_revision = observed.owner_revision;
        entry.availability_generation = observed.availability_generation;
        entry.published_at_ms = Some(now_ms);
        entry.refreshing = false;
        Publication::Applied
    }

    /// Mark a provider's in-flight refresh finished without changing its status or timestamp (used
    /// when a refresh was refused before producing an observation).
    pub fn clear_refreshing(&mut self, token: &RefreshToken) {
        if self.tokens.get(&token.provider_id) == Some(token)
            && let Some(entry) = self.entries.get_mut(&token.provider_id)
        {
            entry.refreshing = false;
        }
    }

    /// The cache-only read. Pure: touches no owner, performs no IPC, contacts no provider.
    pub fn read(
        &self,
        provider_id: &str,
        now_ms: u64,
        broker_available: bool,
    ) -> Option<ProviderStatusView> {
        let entry = self.entries.get(provider_id)?;
        let cache_age_ms = entry.published_at_ms.map(|at| now_ms.saturating_sub(at));
        Some(ProviderStatusView {
            provider_id: provider_id.to_string(),
            status: entry.status.wire(),
            cache_age_ms,
            refreshing: entry.refreshing,
            broker_available,
            broker_reason: if broker_available {
                None
            } else {
                Some(BROKER_UNAVAILABLE)
            },
            recovery: entry.status.recovery(),
        })
    }
}

fn intent_for(provider_id: &str, binding: &Binding, token: RefreshToken) -> RefreshIntent {
    RefreshIntent {
        provider_id: provider_id.to_string(),
        account: CredentialRef::for_provider(provider_id)
            .map(|r| r.account().to_string())
            .unwrap_or_default(),
        binding: binding.clone(),
        token,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rhapsody_credential_ipc::domain::OPENAI_CHAT_COMPLETIONS_BEARER_V1;

    fn binding(url: &str) -> Binding {
        Binding {
            provider_id: "fireworks".into(),
            adapter: OPENAI_CHAT_COMPLETIONS_BEARER_V1.into(),
            base_url: url.into(),
        }
    }

    fn provider(url: &str) -> ProviderBinding {
        ProviderBinding {
            provider_id: "fireworks".into(),
            binding: binding(url),
        }
    }

    fn observed(status: CredentialStatus, rev: u64) -> ObservedStatus {
        ObservedStatus {
            status,
            owner_revision: Revision(rev),
            availability_generation: Revision::INITIAL,
            answered: !matches!(
                status,
                CredentialStatus::OwnerUnavailable | CredentialStatus::OwnerUnauthorized
            ),
        }
    }

    #[test]
    fn status_wire_and_recovery_table() {
        let rows = [
            (CredentialStatus::Absent, "absent", Some("connect")),
            (
                CredentialStatus::DeniedOrLocked,
                "denied_or_locked",
                Some("unlock"),
            ),
            (CredentialStatus::Malformed, "malformed", Some("remove")),
            (
                CredentialStatus::OwnerUnavailable,
                "owner_unavailable",
                Some("open_desktop"),
            ),
            (
                CredentialStatus::OwnerUnauthorized,
                "owner_unauthorized",
                Some("open_desktop"),
            ),
            (CredentialStatus::Configured, "configured", None),
            (
                CredentialStatus::BindingMismatch,
                "binding_mismatch",
                Some("rebind"),
            ),
            (
                CredentialStatus::UnknownRefreshing,
                "unknown_refreshing",
                Some("refresh"),
            ),
        ];
        for (status, wire, recovery) in rows {
            assert_eq!(status.wire(), wire);
            assert_eq!(status.recovery(), recovery);
        }
    }

    #[test]
    fn from_tag_maps_every_owner_state() {
        use CredentialStatus as S;
        let rows = [
            (CredentialStateTag::Present, S::Configured),
            (CredentialStateTag::Absent, S::Absent),
            (CredentialStateTag::DeniedOrLocked, S::DeniedOrLocked),
            (CredentialStateTag::Malformed, S::Malformed),
            (CredentialStateTag::BindingMismatch, S::BindingMismatch),
            (CredentialStateTag::OwnerUnavailable, S::OwnerUnavailable),
            (CredentialStateTag::OwnerUnauthorized, S::OwnerUnauthorized),
        ];
        for (tag, expected) in rows {
            assert_eq!(S::from_tag(tag), expected);
        }
    }

    // MUTATION GUARD (status reads only the cache): a reload marks unknown/refreshing with no age,
    // so an unknown state can never be misreported as a fresh "absent".
    #[test]
    fn reload_marks_unknown_refreshing_with_no_age() {
        let mut cache = ProviderStatusCache::new();
        let intents = cache.apply_reload(1, &[provider("https://api.example/v1")], 0);
        assert_eq!(intents.len(), 1);
        let view = cache.read("fireworks", 5_000, true).expect("tracked");
        assert_eq!(view.status, "unknown_refreshing");
        assert_eq!(view.cache_age_ms, None);
        assert!(view.refreshing);
    }

    #[test]
    fn reload_is_idempotent_for_an_unchanged_set() {
        let mut cache = ProviderStatusCache::new();
        let providers = [provider("https://api.example/v1")];
        let first = cache.apply_reload(1, &providers, 0);
        assert_eq!(first.len(), 1);
        let second = cache.apply_reload(1, &providers, 10);
        assert!(second.is_empty(), "an unchanged set must schedule nothing");
    }

    #[test]
    fn reload_of_a_changed_endpoint_reissues_a_token() {
        let mut cache = ProviderStatusCache::new();
        let _ = cache.apply_reload(1, &[provider("https://api.example/v1")], 0);
        let intents = cache.apply_reload(2, &[provider("https://api.example/v2")], 0);
        assert_eq!(intents.len(), 1);
    }

    #[test]
    fn publish_applies_for_the_current_token_and_reports_age() {
        let mut cache = ProviderStatusCache::new();
        let intents = cache.apply_reload(1, &[provider("https://api.example/v1")], 0);
        assert_eq!(
            cache.publish(
                &intents[0].token,
                &observed(CredentialStatus::Configured, 3),
                100
            ),
            Publication::Applied
        );
        let view = cache.read("fireworks", 350, false).expect("tracked");
        assert_eq!(view.status, "configured");
        assert_eq!(view.cache_age_ms, Some(250));
        assert!(!view.refreshing);
        assert_eq!(view.recovery, None);
    }

    // MUTATION GUARD (closed broker reason code): an unavailable broker reports exactly the closed
    // `provider_broker_unavailable` code and an available one reports none. Deriving the code from a
    // hardcoded `Some`/`None` (or dropping the field) reds this.
    #[test]
    fn broker_reason_is_a_closed_code_derived_from_availability() {
        let mut cache = ProviderStatusCache::new();
        let intents = cache.apply_reload(1, &[provider("https://api.example/v1")], 0);
        let _ = cache.publish(
            &intents[0].token,
            &observed(CredentialStatus::Configured, 1),
            10,
        );
        let down = cache.read("fireworks", 10, false).expect("tracked");
        assert!(!down.broker_available);
        assert_eq!(down.broker_reason, Some(BROKER_UNAVAILABLE));
        assert_eq!(down.broker_reason, Some("provider_broker_unavailable"));
        let up = cache.read("fireworks", 10, true).expect("tracked");
        assert!(up.broker_available);
        assert_eq!(up.broker_reason, None);
    }

    // MUTATION GUARD (reordered completion must not overwrite newer state): a mutation after the
    // token was issued replaces the token, so publishing the OLD completion is discarded rather than
    // regressing the status. An implementation that keyed only on provider id (not the token) would
    // apply it and red this.
    #[test]
    fn a_reordered_completion_after_a_mutation_is_discarded() {
        let mut cache = ProviderStatusCache::new();
        let intents = cache.apply_reload(1, &[provider("https://api.example/v1")], 0);
        let old = intents[0].token.clone();
        // Apply a configured status first (the current token).
        assert_eq!(
            cache.publish(&old, &observed(CredentialStatus::Configured, 1), 10),
            Publication::Applied
        );
        // A mutation issues a fresh token and resets to unknown.
        let mutation = cache
            .begin_refresh("fireworks", true)
            .expect("mutation intent");
        assert_eq!(
            cache.read("fireworks", 11, true).expect("tracked").status,
            "unknown_refreshing"
        );
        // The OLD completion arrives late: it must NOT overwrite the newer unknown state.
        assert_eq!(
            cache.publish(&old, &observed(CredentialStatus::Configured, 1), 12),
            Publication::StaleToken
        );
        assert_eq!(
            cache.read("fireworks", 13, true).expect("tracked").status,
            "unknown_refreshing"
        );
        // The newer token publishes fine.
        assert_eq!(
            cache.publish(
                &mutation.token,
                &observed(CredentialStatus::BindingMismatch, 2),
                14
            ),
            Publication::Applied
        );
        assert_eq!(
            cache.read("fireworks", 15, true).expect("tracked").status,
            "binding_mismatch"
        );
    }

    #[test]
    fn an_older_owner_revision_is_discarded() {
        let mut cache = ProviderStatusCache::new();
        let intents = cache.apply_reload(1, &[provider("https://api.example/v1")], 0);
        let token = intents[0].token.clone();
        assert_eq!(
            cache.publish(&token, &observed(CredentialStatus::Configured, 5), 10),
            Publication::Applied
        );
        // A second read for the SAME token that saw an older revision (started before the first
        // installed a newer one) is stale.
        assert_eq!(
            cache.publish(&token, &observed(CredentialStatus::Configured, 4), 11),
            Publication::StaleRevision
        );
    }

    #[test]
    fn an_owner_transition_always_publishes_even_with_initial_owner_revision() {
        // An availability transition carries Revision::INITIAL (no owner revision), which is NOT
        // "older than" a starting revision of INITIAL, so it is accepted.
        let mut cache = ProviderStatusCache::new();
        let intents = cache.apply_reload(1, &[provider("https://api.example/v1")], 0);
        assert_eq!(
            cache.publish(
                &intents[0].token,
                &observed(CredentialStatus::OwnerUnavailable, 0),
                10
            ),
            Publication::Applied
        );
        assert_eq!(
            cache.read("fireworks", 10, false).expect("tracked").status,
            "owner_unavailable"
        );
    }

    #[test]
    fn explicit_refresh_keeps_status_but_marks_refreshing() {
        let mut cache = ProviderStatusCache::new();
        let intents = cache.apply_reload(1, &[provider("https://api.example/v1")], 0);
        let _ = cache.publish(
            &intents[0].token,
            &observed(CredentialStatus::Configured, 1),
            10,
        );
        let explicit = cache
            .begin_refresh("fireworks", false)
            .expect("explicit intent");
        let view = cache.read("fireworks", 20, true).expect("tracked");
        assert_eq!(view.status, "configured", "the last known status survives");
        assert!(view.refreshing);
        // The explicit token can publish normally.
        assert_eq!(
            cache.publish(
                &explicit.token,
                &observed(CredentialStatus::Configured, 2),
                21
            ),
            Publication::Applied
        );
        assert!(
            !cache
                .read("fireworks", 21, true)
                .expect("tracked")
                .refreshing
        );
    }

    #[test]
    fn publish_for_an_untracked_or_unknown_token_is_stale() {
        let mut cache = ProviderStatusCache::new();
        let intents = cache.apply_reload(1, &[provider("https://api.example/v1")], 0);
        let mut other = intents[0].token.clone();
        other.seq = other.seq.saturating_add(99);
        assert_eq!(
            cache.publish(&other, &observed(CredentialStatus::Configured, 1), 1),
            Publication::StaleToken
        );
    }

    #[test]
    fn dropped_providers_stop_being_tracked() {
        let mut cache = ProviderStatusCache::new();
        let _ = cache.apply_reload(1, &[provider("https://api.example/v1")], 0);
        assert!(cache.tracks("fireworks"));
        let _ = cache.apply_reload(2, &[], 0);
        assert!(!cache.tracks("fireworks"));
        assert!(cache.read("fireworks", 0, true).is_none());
    }
}
