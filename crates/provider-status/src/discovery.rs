//! The model-discovery seam and the exact-secret reflection filter.
//!
//! [`ModelDiscovery`] is the one place a catalog refresh reaches a provider. The coordinator builds a
//! [`DiscoveryRequest`] from the CURRENT canonical binding plus the credential lease returned by a
//! `read_bound` for exactly that binding, so a refresh can only ever contact the operator's configured
//! endpoint; a binding mismatch produces NO request at all.
//!
//! The real adapter is [`OpenAiCompatibleDiscovery`] behind the `discovery` feature. It is a thin
//! wrapper over the broker's fixed-endpoint `/models` fetch
//! ([`rhapsody_provider_broker`]), so the credentialed leg shares the broker's redirect/proxy/TLS/
//! header policy by construction rather than by a mirrored copy — and it never mints or returns an
//! agent capability.

use async_trait::async_trait;
use rhapsody_credential_ipc::domain::BoundCredentialLease;

use crate::catalog::ModelEntry;
use crate::error::CatalogError;

/// The sanitized result of one discovery. `truncated` is `true` when the provider returned
/// duplicate/invalid/excess entries, so the incompleteness is visible rather than silent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredCatalog {
    pub entries: Vec<ModelEntry>,
    pub truncated: bool,
}

/// Everything a discovery adapter may use: the canonical binding it is scoped to, the endpoint TLS
/// policy, and the credential lease read under that binding. The lease is move-only and is consumed
/// by the adapter.
pub struct DiscoveryRequest {
    /// The normalized endpoint (the binding's own `base_url`, already canonical).
    pub endpoint: String,
    pub allow_insecure_http: bool,
    /// The lease read for exactly `endpoint`. Consumed by the adapter.
    pub lease: BoundCredentialLease,
}

/// The one discovery seam. A fake answers scripted results in tests; the production adapter is
/// [`OpenAiCompatibleDiscovery`]. The coordinator is the only caller.
#[async_trait]
pub trait ModelDiscovery: Send + Sync {
    async fn list_models(
        &self,
        request: DiscoveryRequest,
    ) -> Result<DiscoveredCatalog, CatalogError>;
}

/// Drop any entry whose id, display name, or capability hint contains the exact credential, returning
/// `(kept, dropped_any)`. This is the adapter-side half of the design's exact-upstream-secret rule;
/// the broker applies the same rule with the real secret bytes inside its own boundary. As in the
/// broker, this cannot detect encoded or transformed reflection by a malicious provider — it removes
/// exact reflection only, which is what the contract promises.
pub fn filter_reflected(
    entries: Vec<ModelEntry>,
    contains_secret: impl Fn(&str) -> bool,
) -> (Vec<ModelEntry>, bool) {
    let mut kept = Vec::with_capacity(entries.len());
    let mut dropped = false;
    for entry in entries {
        let reflected = contains_secret(&entry.id)
            || entry.display_name.as_deref().is_some_and(&contains_secret)
            || entry.capabilities.iter().any(|c| contains_secret(c));
        if reflected {
            dropped = true;
        } else {
            kept.push(entry);
        }
    }
    (kept, dropped)
}

/// The production, OpenAI-compatible `/models` adapter. It delegates the credentialed GET to the
/// broker's fixed-endpoint fetch, which shares PB2's redirect/proxy/TLS/header/redaction policy and
/// exposes the credential-lease bytes only inside the broker boundary.
#[cfg(feature = "discovery")]
pub struct OpenAiCompatibleDiscovery;

#[cfg(feature = "discovery")]
#[async_trait]
impl ModelDiscovery for OpenAiCompatibleDiscovery {
    async fn list_models(
        &self,
        request: DiscoveryRequest,
    ) -> Result<DiscoveredCatalog, CatalogError> {
        let lease = request
            .lease
            .into_broker_lease()
            .map_err(|_| CatalogError::NoCredential)?;
        let fetched = rhapsody_provider_broker::catalog::fetch_models(
            &request.endpoint,
            request.allow_insecure_http,
            lease,
        )
        .await
        .map_err(|error| match error {
            rhapsody_provider_broker::catalog::FetchError::Endpoint => CatalogError::Endpoint,
            rhapsody_provider_broker::catalog::FetchError::Timeout => CatalogError::Timeout,
            rhapsody_provider_broker::catalog::FetchError::Transport => CatalogError::Transport,
            rhapsody_provider_broker::catalog::FetchError::Status(status) => {
                CatalogError::Status(status)
            }
            rhapsody_provider_broker::catalog::FetchError::Malformed => CatalogError::Malformed,
            rhapsody_provider_broker::catalog::FetchError::BodyTooLarge => {
                CatalogError::BodyTooLarge
            }
        })?;
        Ok(DiscoveredCatalog {
            entries: fetched
                .models
                .into_iter()
                .map(|m| ModelEntry {
                    id: m.id,
                    display_name: m.display_name,
                    capabilities: m.capabilities,
                })
                .collect(),
            truncated: fetched.truncated,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, display: Option<&str>, caps: &[&str]) -> ModelEntry {
        ModelEntry {
            id: id.into(),
            display_name: display.map(str::to_string),
            capabilities: caps.iter().map(|c| c.to_string()).collect(),
        }
    }

    // MUTATION GUARD (exact credential reflection is removed before fields cross the adapter
    // boundary): an entry whose id, display, OR capability reflects the exact secret is dropped, and
    // the drop is visible. An implementation that only checked the id would keep rows 2 and 3.
    #[test]
    fn reflected_entries_are_dropped_from_every_field() {
        let secret = "sk-live-CANARY";
        let entries = vec![
            entry("gpt-4o", None, &[]),
            entry(secret, None, &[]),
            entry("ok", Some(secret), &[]),
            entry("ok2", None, &[secret]),
        ];
        let (kept, dropped) = filter_reflected(entries, |value| value.contains(secret));
        assert!(dropped);
        assert_eq!(
            kept.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
            ["gpt-4o"],
            "an entry reflecting the secret in ANY field is dropped"
        );
    }

    #[test]
    fn clean_entries_are_untouched() {
        let entries = vec![entry("a", Some("A"), &["tools"])];
        let (kept, dropped) = filter_reflected(entries, |_| false);
        assert!(!dropped);
        assert_eq!(kept.len(), 1);
    }
}
