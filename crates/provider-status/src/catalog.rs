//! The bounded, provider-scoped model-catalog cache (STUDIO-990, §2.6 "Model catalogs are additive").
//!
//! # A catalog is a convenience, never a gate
//!
//! A provider does not need a working catalog to be usable: [`CatalogSnapshot::manual_entry_allowed`]
//! is always `true`, and a catalog failure only surfaces a bounded, redacted [`CatalogErrorCode`]. It
//! never disables dispatch or manual model entry, and it is never treated as an allow-list.
//!
//! # Bounds
//!
//! V1 caps a provider body at [`MAX_CATALOG_BODY_BYTES`] and retains at most [`MAX_CATALOG_ENTRIES`]
//! unique model entries after id validation; the first valid occurrence wins a duplicate id, and any
//! duplicate or excess produces a visible `truncated` status rather than unbounded cache growth.
//!
//! # The cache key never carries a secret
//!
//! [`CatalogKey`] is `(provider id, normalized endpoint, config generation, opaque credential
//! revision)`. The credential revision is the owner's opaque counter — never a credential value — and
//! it is internal: it is never serialized to a response. A key change is what makes
//! [`CatalogCache::publish`] replace an entry so a catalog authorized by one credential is never
//! presented as fresh for another.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::discovery::DiscoveredCatalog;
use crate::error::CatalogError;

/// The v1 catalog body cap (design §2.6).
pub const MAX_CATALOG_BODY_BYTES: usize = 8 * 1024 * 1024;
/// The v1 cap on unique model entries per provider (design §2.6).
pub const MAX_CATALOG_ENTRIES: usize = 10_000;
/// The model id byte cap, mirroring `rhapsody_config::providers::MODEL_ID_MAX_BYTES` and the
/// broker's own copy. Kept local so this crate takes no config dependency for one constant; a
/// cross-crate pin test in `rhapsodyd` (the one crate that depends on all three) asserts they agree.
pub const MAX_MODEL_ID_BYTES: usize = 512;

/// One discovered model. Only the fields the API may show: an id, an optional display name, and
/// capability hints. Arbitrary provider headers and unparsed fields are never cached.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelEntry {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
}

/// The bounded, non-secret reason a catalog refresh failed. The wire enum; the message travels
/// alongside it from [`CatalogError::message`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogErrorCode {
    Unsupported,
    NoCredential,
    BindingMismatch,
    Timeout,
    Transport,
    Status,
    Malformed,
    BodyTooLarge,
    Endpoint,
    InFlight,
}

/// The catalog view a `GET` observes. Cache-only; never contains a credential or a raw provider body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CatalogSnapshot {
    pub provider_id: String,
    pub models: Vec<ModelEntry>,
    /// Whether the provider list was longer than the v1 cap (or had duplicate ids), so the result is
    /// visibly incomplete rather than silently truncated.
    pub truncated: bool,
    pub cache_age_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<CatalogErrorCode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    /// Always `true`: catalog discovery is additive and manual model entry is first-class.
    pub manual_entry_allowed: bool,
}

/// The cache key (design §2.6). Never serialized.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogKey {
    pub provider_id: String,
    pub endpoint: String,
    pub generation: u64,
    /// The opaque owner credential revision (a counter, never a value).
    pub credential_revision: u64,
}

#[derive(Debug, Clone)]
struct StoredCatalog {
    key: CatalogKey,
    models: Vec<ModelEntry>,
    truncated: bool,
    error: Option<CatalogError>,
    published_at_ms: Option<u64>,
}

/// The catalog cache. `publish` stores a sanitized result; `read` is pure.
#[derive(Debug, Default, Clone)]
pub struct CatalogCache {
    entries: BTreeMap<String, StoredCatalog>,
}

impl CatalogCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Store the outcome of one completed catalog refresh under `key`. The entries are re-bounded
    /// here as a defensive second line behind the adapter's own cap. A different key replaces the
    /// entry wholesale, so a catalog authorized under one key can never be served for another.
    pub fn publish(
        &mut self,
        key: CatalogKey,
        result: Result<DiscoveredCatalog, CatalogError>,
        now_ms: u64,
    ) {
        let stored = match result {
            Ok(discovered) => {
                let (models, truncated) = bound_entries(discovered.entries);
                StoredCatalog {
                    key: key.clone(),
                    models,
                    truncated: truncated || discovered.truncated,
                    error: None,
                    published_at_ms: Some(now_ms),
                }
            }
            Err(error) => StoredCatalog {
                key: key.clone(),
                models: Vec::new(),
                truncated: false,
                error: Some(error),
                published_at_ms: Some(now_ms),
            },
        };
        self.entries.insert(key.provider_id.clone(), stored);
    }

    /// Mark a provider's catalog unknown because its definition or credential changed. A later `GET`
    /// then reports a dated, key-corrected state instead of a stale one.
    pub fn invalidate(&mut self, provider_id: &str) {
        self.entries.remove(provider_id);
    }

    /// Drop every cached catalog (used when the provider set is replaced wholesale).
    pub fn invalidate_all(&mut self) {
        self.entries.clear();
    }

    /// The key the current entry was published under, if any. Exposed so a caller can confirm the
    /// entry is keyed to the current generation/credential revision without re-deriving it.
    pub fn cached_key(&self, provider_id: &str) -> Option<CatalogKey> {
        self.entries
            .get(provider_id)
            .map(|stored| stored.key.clone())
    }

    /// The cache-only read. Pure: performs no provider I/O.
    pub fn read(&self, provider_id: &str, now_ms: u64) -> Option<CatalogSnapshot> {
        let stored = self.entries.get(provider_id)?;
        Some(CatalogSnapshot {
            provider_id: provider_id.to_string(),
            models: stored.models.clone(),
            truncated: stored.truncated,
            cache_age_ms: stored.published_at_ms.map(|at| now_ms.saturating_sub(at)),
            error: stored.error.as_ref().map(|e| e.code()),
            error_message: stored.error.as_ref().map(|e| e.message()),
            manual_entry_allowed: true,
        })
    }
}

/// The first-valid-wins dedup and cap, returning `(entries, truncated)`.
pub fn bound_entries(entries: Vec<ModelEntry>) -> (Vec<ModelEntry>, bool) {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut out: Vec<ModelEntry> = Vec::new();
    let mut truncated = false;
    for entry in entries {
        if !valid_model_id(&entry.id) {
            truncated = true;
            continue;
        }
        if !seen.insert(entry.id.clone()) {
            truncated = true;
            continue;
        }
        if out.len() >= MAX_CATALOG_ENTRIES {
            truncated = true;
            break;
        }
        out.push(entry);
    }
    (out, truncated)
}

/// The model-id transport bounds mirrored from config (non-empty, ≤ 512 bytes, no surrounding
/// whitespace, no control/NUL bytes). Membership is never required.
pub fn valid_model_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_MODEL_ID_BYTES
        && id.trim() == id
        && !id.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(endpoint: &str, generation: u64, revision: u64) -> CatalogKey {
        CatalogKey {
            provider_id: "fireworks".into(),
            endpoint: endpoint.into(),
            generation,
            credential_revision: revision,
        }
    }

    fn entry(id: &str) -> ModelEntry {
        ModelEntry {
            id: id.into(),
            display_name: None,
            capabilities: Vec::new(),
        }
    }

    fn discovered(ids: &[&str], truncated: bool) -> DiscoveredCatalog {
        DiscoveredCatalog {
            entries: ids.iter().map(|id| entry(id)).collect(),
            truncated,
        }
    }

    #[test]
    fn publish_then_read_returns_entries_and_age() {
        let mut cache = CatalogCache::new();
        cache.publish(
            key("https://api.example/v1", 1, 0),
            Ok(discovered(&["a", "b"], false)),
            100,
        );
        let snap = cache.read("fireworks", 400).expect("cached");
        assert_eq!(
            snap.models
                .iter()
                .map(|m| m.id.as_str())
                .collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert_eq!(snap.cache_age_ms, Some(300));
        assert!(!snap.truncated);
        assert!(snap.error.is_none());
        assert!(snap.manual_entry_allowed, "manual entry is always allowed");
        let stored = cache.cached_key("fireworks").expect("key recorded");
        assert_eq!(stored.credential_revision, 0);
    }

    // MUTATION GUARD (a catalog authorized by one key is not served fresh for another): a credential
    // mutation (new revision) invalidates the entry, so the next read is empty until a new refresh.
    #[test]
    fn a_key_change_replaces_the_entry() {
        let mut cache = CatalogCache::new();
        cache.publish(
            key("https://api.example/v1", 1, 0),
            Ok(discovered(&["a"], false)),
            100,
        );
        // The definition reload invalidates; then a new key publishes different models.
        cache.invalidate("fireworks");
        assert!(cache.read("fireworks", 200).is_none());
        cache.publish(
            key("https://api.example/v1", 2, 7),
            Ok(discovered(&["z"], false)),
            200,
        );
        let snap = cache.read("fireworks", 200).expect("cached");
        assert_eq!(snap.models[0].id, "z");
        assert_eq!(snap.cache_age_ms, Some(0));
    }

    // MUTATION GUARD (bounded cache, visible truncation): duplicate ids and over-cap lists produce a
    // visible `truncated` flag and never unbounded growth. An implementation that just extended a
    // Vec would red the duplicate and cap assertions.
    #[test]
    fn duplicates_and_excess_are_first_wins_and_visible() {
        let mut cache = CatalogCache::new();
        let mut ids: Vec<String> = vec!["dup".into(), "dup".into(), "keep".into()];
        for i in 0..(MAX_CATALOG_ENTRIES + 5) {
            ids.push(format!("m{i}"));
        }
        let entries: Vec<ModelEntry> = ids.iter().map(|id| entry(id)).collect();
        cache.publish(
            key("https://api.example/v1", 1, 0),
            Ok(DiscoveredCatalog {
                entries,
                truncated: false,
            }),
            0,
        );
        let snap = cache.read("fireworks", 0).expect("cached");
        assert!(snap.truncated, "duplicate/excess must be visible");
        assert_eq!(snap.models.len(), MAX_CATALOG_ENTRIES);
        assert_eq!(snap.models[0].id, "dup");
        assert_eq!(snap.models[1].id, "keep");
    }

    #[test]
    fn invalid_or_oversized_model_ids_are_dropped_and_flagged() {
        let (entries, truncated) = bound_entries(vec![
            entry("ok"),
            entry(""),
            entry(" padded "),
            entry("has\nnewline"),
        ]);
        assert!(truncated);
        assert_eq!(
            entries.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
            ["ok"]
        );
    }

    #[test]
    fn a_failed_catalog_is_visible_and_actionable_but_keeps_manual_entry() {
        let mut cache = CatalogCache::new();
        cache.publish(
            key("https://api.example/v1", 1, 0),
            Err(CatalogError::Timeout),
            50,
        );
        let snap = cache.read("fireworks", 80).expect("cached failure");
        assert_eq!(snap.error, Some(CatalogErrorCode::Timeout));
        assert!(
            snap.error_message
                .as_deref()
                .unwrap_or("")
                .contains("deadline")
        );
        assert!(snap.models.is_empty());
        assert!(snap.manual_entry_allowed);
        assert_eq!(snap.cache_age_ms, Some(30));
    }
}
