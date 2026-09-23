//! rhapsody-provider-status — the non-secret provider-status and model-catalog layer for STUDIO-990
//! (`provider-auth-design.md` §P9, §6; `2.6 Model catalogs are additive`). Rhapsody-only: the frozen
//! Go daemon has no provider concept at all, so this whole crate is an ADDITIVE divergence (README
//! "Divergences").
//!
//! # What lives here
//!
//! The four P9 pieces, kept deliberately free of Keychain/IPC/provider I/O where they can be:
//!
//! * [`status`] — the non-secret *status* cache. It holds only a [`CredentialStatus`] tag, a cache
//!   timestamp, the config generation, the expected (non-secret) [`Binding`], and the last owner
//!   revision. It NEVER holds a credential, a stored binding fingerprint, or a credential revision.
//!   Reads are pure: a `GET` handler observes cached state and can never trigger owner I/O.
//! * [`catalog`] — the bounded, provider-scoped model catalog cache (body/entry caps, duplicate
//!   first-wins, visible truncation, a cache key over provider id + endpoint + config generation +
//!   opaque credential revision).
//! * [`discovery`] — the [`ModelDiscovery`] seam plus the exact-secret reflection filter. The real
//!   fixed-endpoint adapter is [`discovery::OpenAiCompatibleDiscovery`] behind the `discovery`
//!   feature, which delegates its egress to the broker's PB2 stack.
//! * [`coordinator`] — the off-loop, concurrency-bounded refresh coordinator. Every credentialed
//!   operation goes through it: it calls [`CredentialReadSource::read_bound`] for the *current*
//!   canonical endpoint, and on a `BindingMismatch` performs NO provider I/O.
//!
//! # The one boundary rule
//!
//! The two `GET` routes are cache-only. This crate enforces that by construction: [`status::ProviderStatusCache::read`]
//! and [`catalog::CatalogCache::read`] take `&self` and touch no source. Only
//! [`coordinator::RefreshCoordinator`] calls a [`CredentialReadSource`] or a [`ModelDiscovery`], and
//! it is only ever driven off the request path.
//!
//! # Compare-and-swap publication
//!
//! A reload, a mutation, or an explicit refresh issues an unforgeable [`status::RefreshToken`] for the
//! affected provider. A completion may publish ONLY when that token is still current, the config
//! generation and expected binding still match, and the observed owner revision is not older than the
//! token's starting revision. Reordered completions are therefore discarded rather than overwriting
//! newer state — the table tests in `status` pin exactly that.

pub mod catalog;
pub mod coordinator;
pub mod discovery;
pub mod error;
pub mod status;

pub use catalog::{CatalogCache, CatalogErrorCode, CatalogSnapshot, ModelEntry};
pub use coordinator::{
    CredentialReadSource, ObservedRead, ObservedState, ProviderConfig, RefreshCoordinator,
};
#[cfg(feature = "discovery")]
pub use discovery::OpenAiCompatibleDiscovery;
pub use discovery::{DiscoveredCatalog, DiscoveryRequest, ModelDiscovery};
pub use error::CatalogError;
pub use status::{
    CredentialStatus, ObservedStatus, ProviderBinding, ProviderStatusCache, ProviderStatusView,
    RefreshIntent, RefreshToken,
};
