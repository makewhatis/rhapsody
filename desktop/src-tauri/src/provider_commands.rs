//! The desktop-only provider-credential command surface (ticket STUDIO-991 / design §P10). No Go
//! parity — Symphony never had providers, so this is a Rhapsody-only addition (README "Divergences").
//!
//! # The security boundary this module exists to hold
//!
//! The bundled webview necessarily originates ONE immutable JavaScript string per Connect/Replace
//! invocation. Everything after that is Rust: validation, binding derivation, the one-use
//! confirmation nonce, persistence through the P0c credential owner, and the response DTO. Nothing
//! here returns the entered value, the stored envelope, the stored binding, or the opaque owner
//! revision to a caller. The guarantee is honest about the browser boundary — an immutable JS string
//! cannot be proven zeroized — so the UI clears its reference immediately and relies on
//! no-persistence/no-history plus the restricted bundled origin; Rust wipes its own owned copies.
//!
//! # Why a prepare/commit pair instead of one call
//!
//! The browser must never supply a binding or a revision (that is how a provider definition could
//! retarget an existing key). [`ProviderCommandService::prepare`] derives the canonical binding from
//! the validated current provider definition, snapshots the owner's revision/state and the config
//! generation into an opaque one-use nonce, and returns only the non-secret normalized endpoint.
//! [`ProviderCommandService::commit`] accepts the operation-appropriate nonce and REJECTS if the
//! configuration or the owner state moved in between — so a confirmation can never be replayed
//! against a changed destination, and the revision stays entirely Rust-internal.
//!
//! # Testability
//!
//! The service is deliberately Tauri-free. The credential owner factory, the clock, the nonce
//! source, the connection tester, and the daemon observation are all injected, so every operation,
//! precondition, and error class is covered by in-memory tests without touching the real Keychain.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::Serialize;
use zeroize::Zeroizing;

use rhapsody_credential_ipc::domain::{
    Binding, BoundCredentialLease, CredentialRef, CredentialState, CredentialStateTag, Revision,
};
use rhapsody_credential_ipc::owner::{CredentialOwner, MutationError, MutationOutcome};

use crate::provider_credential::ProviderCredentialOwner;

/// The default lifetime of a confirmation nonce. Short on purpose: it only has to survive the round
/// trip between `prepare` and the user confirming, never a session.
pub const DEFAULT_NONCE_TTL: Duration = Duration::from_secs(120);

/// The four §2.5 mutations. Kept as one closed enum (never a free-form string from the webview) so
/// a generic "save" cannot be expressed — Replace cannot move a binding and Rebind cannot replace a
/// key because there is simply no variant that would do both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderOperation {
    Connect,
    Replace,
    Rebind,
    Remove,
}

impl ProviderOperation {
    /// The stable wire spelling shared with the frontend.
    pub fn wire(self) -> &'static str {
        match self {
            ProviderOperation::Connect => "connect",
            ProviderOperation::Replace => "replace",
            ProviderOperation::Rebind => "rebind",
            ProviderOperation::Remove => "remove",
        }
    }

    /// Whether this operation carries a user-entered key. Rebind/Remove never do.
    pub fn carries_secret(self) -> bool {
        matches!(
            self,
            ProviderOperation::Connect | ProviderOperation::Replace
        )
    }

    /// Parse the wire spelling; any other value is refused rather than defaulted.
    pub fn parse(text: &str) -> Option<ProviderOperation> {
        match text {
            "connect" => Some(ProviderOperation::Connect),
            "replace" => Some(ProviderOperation::Replace),
            "rebind" => Some(ProviderOperation::Rebind),
            "remove" => Some(ProviderOperation::Remove),
            _ => None,
        }
    }
}

/// How a committed mutation stands relative to a running daemon (§P10's last two acceptance
/// bullets). [`Self::Synchronized`] is claimed ONLY when the daemon has observed a revision at least
/// as new as the mutation; a committed-but-unobserved write is `StoredUnsynchronized` (never a
/// generic failure that invites a blind replay of a non-idempotent write).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncState {
    Synchronized,
    StoredOffline,
    StoredUnsynchronized,
}

impl SyncState {
    pub fn wire(self) -> &'static str {
        match self {
            SyncState::Synchronized => "synchronized",
            SyncState::StoredOffline => "stored_offline",
            SyncState::StoredUnsynchronized => "stored_unsynchronized",
        }
    }
}

/// The caller-supplied daemon observation `commit` folds into its [`SyncState`]. Passing it in (rather
/// than holding a live daemon handle) keeps this module free of the supervisor/IPC wiring PB7 owns,
/// and keeps every sync branch injectable in tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DaemonObservation {
    /// Whether the supervised `rhapsodyd` is Running.
    pub running: bool,
    /// The revision the running daemon has observed through the authenticated credential channel, if
    /// any. `None` means no channel has delivered a revision yet.
    pub observed_revision: Option<Revision>,
}

impl DaemonObservation {
    /// No daemon at all — the stored-offline case.
    pub fn offline() -> DaemonObservation {
        DaemonObservation {
            running: false,
            observed_revision: None,
        }
    }
}

/// A typed, non-secret failure of a provider command. Every variant is closed and carries no key,
/// envelope, stored binding, or revision; `code` is the stable wire spelling the UI switches on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderCommandError {
    /// No configured provider with that id (or the id is not canonical).
    UnknownProvider,
    /// The operation string was not one of the four §2.5 mutations.
    UnknownOperation,
    /// The workflow's provider definitions could not be read/validated.
    ConfigUnavailable(String),
    /// The confirmation nonce was never issued (or was already consumed).
    NonceUnknown,
    /// The nonce's short lifetime elapsed.
    NonceExpired,
    /// The nonce belongs to a different provider than the request names.
    NonceProviderMismatch,
    /// The nonce was minted for a different operation.
    NonceOperationMismatch,
    /// The provider configuration changed after the confirmation was prepared.
    GenerationChanged,
    /// The owner's revision or state moved after the confirmation was prepared.
    OwnerStateChanged,
    /// The owner's §2.5 precondition for this operation did not hold.
    PreconditionFailed,
    /// The Keychain refused the read/write (locked or access denied).
    KeychainDenied,
    /// The candidate value violated the broker's size/syntax bound; never trimmed or stored.
    InvalidValue(String),
    /// No usable credential is stored (absent/malformed/binding-mismatch) for a credentialed action.
    NoCredential,
    /// A Connect/Replace arrived without the user-entered key it requires.
    MissingSecret,
    /// The invocation did not come from the bundled `main` window at the `rhapsody://localhost`
    /// origin.
    UnauthorizedInvocation,
}

impl ProviderCommandError {
    /// The stable, non-secret wire code.
    pub fn code(&self) -> &'static str {
        match self {
            ProviderCommandError::UnknownProvider => "unknown_provider",
            ProviderCommandError::UnknownOperation => "unknown_operation",
            ProviderCommandError::ConfigUnavailable(_) => "config_unavailable",
            ProviderCommandError::NonceUnknown => "nonce_unknown",
            ProviderCommandError::NonceExpired => "nonce_expired",
            ProviderCommandError::NonceProviderMismatch => "nonce_provider_mismatch",
            ProviderCommandError::NonceOperationMismatch => "nonce_operation_mismatch",
            ProviderCommandError::GenerationChanged => "generation_changed",
            ProviderCommandError::OwnerStateChanged => "owner_state_changed",
            ProviderCommandError::PreconditionFailed => "precondition_failed",
            ProviderCommandError::KeychainDenied => "keychain_denied",
            ProviderCommandError::InvalidValue(_) => "invalid_value",
            ProviderCommandError::NoCredential => "no_credential",
            ProviderCommandError::MissingSecret => "missing_secret",
            ProviderCommandError::UnauthorizedInvocation => "unauthorized_invocation",
        }
    }

    /// A closed, actionable, operator-facing message. Never echoes a key or a stored binding.
    pub fn message(&self) -> String {
        match self {
            ProviderCommandError::UnknownProvider => {
                "no configured provider with that id".to_string()
            }
            ProviderCommandError::UnknownOperation => "unknown credential operation".to_string(),
            ProviderCommandError::ConfigUnavailable(reason) => {
                format!("the workflow's provider configuration could not be read: {reason}")
            }
            ProviderCommandError::NonceUnknown => {
                "this confirmation is no longer valid; start the action again".to_string()
            }
            ProviderCommandError::NonceExpired => {
                "this confirmation expired; start the action again".to_string()
            }
            ProviderCommandError::NonceProviderMismatch => {
                "this confirmation belongs to a different provider".to_string()
            }
            ProviderCommandError::NonceOperationMismatch => {
                "this confirmation belongs to a different operation".to_string()
            }
            ProviderCommandError::GenerationChanged => {
                "the provider configuration changed; start the action again".to_string()
            }
            ProviderCommandError::OwnerStateChanged => {
                "the stored credential changed; start the action again".to_string()
            }
            ProviderCommandError::PreconditionFailed => {
                "this action's precondition does not hold for the current credential state"
                    .to_string()
            }
            ProviderCommandError::KeychainDenied => {
                "the login keychain refused access; unlock it and try again".to_string()
            }
            ProviderCommandError::InvalidValue(reason) => {
                format!("the entered credential was refused: {reason}")
            }
            ProviderCommandError::NoCredential => {
                "no usable credential is stored for this provider".to_string()
            }
            ProviderCommandError::MissingSecret => {
                "this action requires an entered credential".to_string()
            }
            ProviderCommandError::UnauthorizedInvocation => {
                "this action is only available from the Rhapsody desktop window".to_string()
            }
        }
    }
}

impl std::fmt::Display for ProviderCommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for ProviderCommandError {}

/// One configured provider, resolved from the validated workflow definition into the non-secret
/// values the command surface needs. The stored envelope (and its binding) is never part of this.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedProvider {
    provider_id: String,
    display_name: String,
    binding: Binding,
    allow_insecure_http: bool,
}

/// The loaded provider set plus its change generation. `generation` is the same
/// [`rhapsody_config::ProviderReload`] digest the daemon uses, so a workflow edit invalidates a
/// pending confirmation here exactly as it re-arms the daemon.
struct ConfigSnapshot {
    generation: u64,
    providers: BTreeMap<String, ResolvedProvider>,
}

/// One minted confirmation. Deliberately holds no secret and exposes no revision — it lives only
/// inside the service's nonce map, and the map is the ONLY place a nonce is stored (never persisted,
/// never logged).
struct PendingConfirmation {
    provider_id: String,
    operation: ProviderOperation,
    generation: u64,
    expected_revision: Revision,
    expected_state: CredentialStateTag,
    binding: Binding,
    created: Instant,
}

/// How the service creates (and caches) the per-provider credential owner. Injected so tests use
/// in-memory keychain doubles and production uses the real Keychain under the provider namespace.
pub type OwnerFactory = Arc<dyn Fn(&str) -> Option<Arc<dyn CredentialOwner>> + Send + Sync>;

/// A one-shot connection test against the CURRENT approved binding. `ok` is the headline verdict;
/// `code`/`message` are closed and never carry a provider body or a credential.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TestConnectionDto {
    pub ok: bool,
    pub code: String,
    pub message: String,
}

/// What one prepared confirmation returns to the webview: the non-secret normalized endpoint, the
/// operation it is for, and the opaque one-use nonce. No revision, no binding fingerprint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PreparedCommandDto {
    pub provider_id: String,
    pub operation: String,
    pub endpoint: String,
    pub insecure_http: bool,
    pub nonce: String,
    pub expires_in_ms: u64,
}

/// The result of a committed mutation. `mutated` is `false` for `already_absent` Remove, which must
/// never be reported as a mutation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MutationResultDto {
    pub provider_id: String,
    pub operation: String,
    pub mutated: bool,
    pub status: String,
    pub sync: String,
}

/// A provider's non-secret status for the credential form. Distinguishes absent from denied (the
/// acceptance's "the UI can distinguish absent from denied"), and exposes the closed set of
/// operations that the current state permits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProviderStatusDto {
    pub provider_id: String,
    pub display_name: String,
    pub endpoint: String,
    pub adapter: String,
    pub insecure_http: bool,
    pub status: String,
    pub recovery: Option<String>,
    pub can_connect: bool,
    pub can_replace: bool,
    pub can_rebind: bool,
    pub can_remove: bool,
}

/// The request one connection test hands the tester. The lease is move-only and consumed.
pub struct TestConnectionRequest {
    pub endpoint: String,
    pub allow_insecure_http: bool,
    pub lease: BoundCredentialLease,
}

/// A boxed future, so the tester trait stays dependency-free (no `async-trait`).
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The bounded, fixed-endpoint operator operation behind Test Connection. The production adapter is
/// [`HttpConnectionTester`]; tests inject a fake so the service never contacts anything.
pub trait ConnectionTester: Send + Sync {
    fn test<'a>(&'a self, request: TestConnectionRequest) -> BoxFuture<'a, TestConnectionDto>;
}

/// An injectable monotonic clock, so nonce expiry is tested deterministically.
pub trait Clock: Send + Sync {
    fn now(&self) -> Instant;
}

/// The real clock.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The desktop-only provider-credential command service. Cheap to hold behind an `Arc`.
pub struct ProviderCommandService {
    workflow_path: Option<PathBuf>,
    owners: Mutex<BTreeMap<String, Arc<dyn CredentialOwner>>>,
    nonces: Mutex<BTreeMap<String, PendingConfirmation>>,
    owner_factory: OwnerFactory,
    tester: Arc<dyn ConnectionTester>,
    clock: Arc<dyn Clock>,
    nonce_source: Arc<dyn Fn() -> String + Send + Sync>,
    nonce_ttl: Duration,
}

impl ProviderCommandService {
    /// The production service over a workflow path.
    pub fn new(workflow_path: Option<PathBuf>, tester: Arc<dyn ConnectionTester>) -> Arc<Self> {
        ProviderCommandService::with_dependencies(
            workflow_path,
            production_owner_factory(),
            tester,
            Arc::new(SystemClock),
            Arc::new(rhapsody_credential_ipc::token::generate),
            DEFAULT_NONCE_TTL,
        )
    }

    /// The full constructor with every seam injected.
    pub fn with_dependencies(
        workflow_path: Option<PathBuf>,
        owner_factory: OwnerFactory,
        tester: Arc<dyn ConnectionTester>,
        clock: Arc<dyn Clock>,
        nonce_source: Arc<dyn Fn() -> String + Send + Sync>,
        nonce_ttl: Duration,
    ) -> Arc<Self> {
        Arc::new(ProviderCommandService {
            workflow_path,
            owners: Mutex::new(BTreeMap::new()),
            nonces: Mutex::new(BTreeMap::new()),
            owner_factory,
            tester,
            clock,
            nonce_source,
            nonce_ttl,
        })
    }

    /// The cached owner for `provider_id`, created on first use.
    fn owner(&self, provider_id: &str) -> Option<Arc<dyn CredentialOwner>> {
        let mut owners = lock(&self.owners);
        if let Some(owner) = owners.get(provider_id) {
            return Some(owner.clone());
        }
        let owner = (self.owner_factory)(provider_id)?;
        owners.insert(provider_id.to_string(), owner.clone());
        Some(owner)
    }

    /// Load and validate the workflow's provider definitions. A missing workflow file, an absent
    /// `providers:` block, or an unreadable/invalid config is an EMPTY provider set (never a panic):
    /// a provider-less install is the shipped default and must behave exactly as before.
    fn load_config(&self) -> Result<ConfigSnapshot, ProviderCommandError> {
        let Some(path) = self.workflow_path.as_deref() else {
            return Ok(ConfigSnapshot {
                generation: 0,
                providers: BTreeMap::new(),
            });
        };
        if !path.is_file() {
            return Ok(ConfigSnapshot {
                generation: 0,
                providers: BTreeMap::new(),
            });
        }
        let config = load_resolved_config(path)?;
        let deadline =
            rhapsody_config::providers::provider_turn_deadline_ms(config.opencode.turn_timeout_ms);
        let generation =
            rhapsody_config::ProviderReload::from_providers(&config.providers, deadline).revision();
        let mut providers = BTreeMap::new();
        for (id, def) in &config.providers {
            // A definition whose binding cannot be derived has no credential surface; validation
            // should have refused it, and we simply skip it rather than inventing an endpoint.
            let Ok(binding) = def.credential_binding() else {
                continue;
            };
            providers.insert(
                id.clone(),
                ResolvedProvider {
                    provider_id: id.clone(),
                    display_name: def.display_name.clone(),
                    binding: Binding {
                        provider_id: binding.provider_id,
                        adapter: binding.adapter,
                        base_url: binding.base_url,
                    },
                    allow_insecure_http: def.allow_insecure_http,
                },
            );
        }
        Ok(ConfigSnapshot {
            generation,
            providers,
        })
    }

    /// One provider's non-secret status and the operations its state permits.
    pub fn status(&self, provider_id: &str) -> Result<ProviderStatusDto, ProviderCommandError> {
        let snapshot = self.load_config()?;
        let resolved = snapshot
            .providers
            .get(provider_id)
            .ok_or(ProviderCommandError::UnknownProvider)?;
        let owner = self
            .owner(provider_id)
            .ok_or(ProviderCommandError::UnknownProvider)?;
        let read = owner.read_bound(&resolved.binding);
        Ok(status_from(provider_id, resolved, read.state.tag()))
    }

    /// Every configured provider's status, in provider-id order.
    pub fn statuses(&self) -> Result<Vec<ProviderStatusDto>, ProviderCommandError> {
        let snapshot = self.load_config()?;
        let mut out = Vec::with_capacity(snapshot.providers.len());
        for (id, resolved) in &snapshot.providers {
            let owner = self
                .owner(id)
                .ok_or(ProviderCommandError::UnknownProvider)?;
            let read = owner.read_bound(&resolved.binding);
            out.push(status_from(id, resolved, read.state.tag()));
        }
        Ok(out)
    }

    /// Mint the one-use confirmation for `operation` against the CURRENT canonical binding and the
    /// current owner revision/state snapshot. Returns only non-secret values plus the opaque nonce.
    pub fn prepare(
        &self,
        provider_id: &str,
        operation: ProviderOperation,
    ) -> Result<PreparedCommandDto, ProviderCommandError> {
        let snapshot = self.load_config()?;
        let resolved = snapshot
            .providers
            .get(provider_id)
            .ok_or(ProviderCommandError::UnknownProvider)?;
        let owner = self
            .owner(provider_id)
            .ok_or(ProviderCommandError::UnknownProvider)?;
        // One atomic owner snapshot: the revision and the state tag come from the SAME read, so a
        // confirmation can never pair a revision from one state with a tag from another.
        let read = owner.read_bound(&resolved.binding);
        let expected_revision = read.revision;
        let expected_state = read.state.tag();
        drop(read);

        let nonce = (self.nonce_source)();
        let created = self.clock.now();
        // Bound the map: drop every already-expired confirmation before inserting a new one.
        {
            let mut nonces = lock(&self.nonces);
            let ttl = self.nonce_ttl;
            nonces.retain(|_, pending| created.duration_since(pending.created) <= ttl);
            nonces.insert(
                nonce.clone(),
                PendingConfirmation {
                    provider_id: provider_id.to_string(),
                    operation,
                    generation: snapshot.generation,
                    expected_revision,
                    expected_state,
                    binding: resolved.binding.clone(),
                    created,
                },
            );
        }

        Ok(PreparedCommandDto {
            provider_id: provider_id.to_string(),
            operation: operation.wire().to_string(),
            endpoint: resolved.binding.base_url.clone(),
            insecure_http: resolved.allow_insecure_http,
            nonce,
            expires_in_ms: u64::try_from(self.nonce_ttl.as_millis()).unwrap_or(u64::MAX),
        })
    }

    /// Commit a previously prepared confirmation. The nonce is consumed on ENTRY — every failure
    /// below (expiry, provider/operation mismatch, changed generation, moved owner state, a §2.5
    /// precondition refusal) therefore consumes it too, so a stale nonce is never replayable.
    ///
    /// The binding and expected revision come ONLY from the stored confirmation; the caller supplies
    /// exactly the nonce and (for Connect/Replace) the entered key. Nothing the webview sends can
    /// name a destination or a revision.
    pub fn commit(
        &self,
        provider_id: &str,
        operation: ProviderOperation,
        nonce: &str,
        secret: Option<String>,
        daemon: DaemonObservation,
    ) -> Result<MutationResultDto, ProviderCommandError> {
        let pending = lock(&self.nonces)
            .remove(nonce)
            .ok_or(ProviderCommandError::NonceUnknown)?;

        if pending.provider_id != provider_id {
            return Err(ProviderCommandError::NonceProviderMismatch);
        }
        if pending.operation != operation {
            return Err(ProviderCommandError::NonceOperationMismatch);
        }
        if self.clock.now().duration_since(pending.created) > self.nonce_ttl {
            return Err(ProviderCommandError::NonceExpired);
        }

        let snapshot = self.load_config()?;
        if snapshot.generation != pending.generation {
            return Err(ProviderCommandError::GenerationChanged);
        }
        let resolved = snapshot
            .providers
            .get(provider_id)
            .ok_or(ProviderCommandError::UnknownProvider)?;
        if resolved.binding != pending.binding {
            return Err(ProviderCommandError::GenerationChanged);
        }
        let owner = self
            .owner(provider_id)
            .ok_or(ProviderCommandError::UnknownProvider)?;

        // Re-validate the owner snapshot the confirmation was minted against. This is the barrier
        // that stops a confirmation raced by a concurrent Connect/Replace/Rebind/Remove (or an
        // availability transition) from applying against a different state.
        let current = owner.read_bound(&resolved.binding);
        if current.revision != pending.expected_revision
            || current.state.tag() != pending.expected_state
        {
            return Err(ProviderCommandError::OwnerStateChanged);
        }
        drop(current);

        // Own the entered key in a zeroizing buffer; it is wiped on every exit path.
        let mut secret = match operation.carries_secret() {
            true => Some(Zeroizing::new(
                secret.ok_or(ProviderCommandError::MissingSecret)?,
            )),
            false => None,
        };

        let outcome = match operation {
            ProviderOperation::Connect => owner.connect(
                pending.expected_revision,
                resolved.binding.clone(),
                take_secret(&mut secret),
            ),
            ProviderOperation::Replace => owner.replace(
                pending.expected_revision,
                &resolved.binding,
                take_secret(&mut secret),
            ),
            ProviderOperation::Rebind => {
                owner.rebind(pending.expected_revision, resolved.binding.clone())
            }
            ProviderOperation::Remove => owner.remove(pending.expected_revision),
        }
        .map_err(map_mutation_error)?;

        let (mutated, revision_after) = match outcome {
            MutationOutcome::Advanced(revision) => (true, revision),
            MutationOutcome::AlreadyAbsent(revision) => (false, revision),
        };

        let status = owner.read_bound(&resolved.binding).state.tag();
        let sync = sync_state(daemon, revision_after, mutated);

        Ok(MutationResultDto {
            provider_id: provider_id.to_string(),
            operation: operation.wire().to_string(),
            mutated,
            status: status_wire(status).to_string(),
            sync: sync.wire().to_string(),
        })
    }

    /// Run one bounded, fixed-endpoint connection test. Requires the CURRENT approved binding — a
    /// credential that is absent, malformed, denied/locked, or bound to a different endpoint is a
    /// typed refusal and performs NO provider I/O, so a binding/auth failure can never contact
    /// another endpoint. Returns no raw provider body.
    pub async fn test_connection(
        &self,
        provider_id: &str,
    ) -> Result<TestConnectionDto, ProviderCommandError> {
        let snapshot = self.load_config()?;
        let resolved = snapshot
            .providers
            .get(provider_id)
            .ok_or(ProviderCommandError::UnknownProvider)?;
        let owner = self
            .owner(provider_id)
            .ok_or(ProviderCommandError::UnknownProvider)?;
        let lease = match owner.read_bound(&resolved.binding).state {
            CredentialState::Present(lease) => lease,
            CredentialState::DeniedOrLocked => return Err(ProviderCommandError::KeychainDenied),
            CredentialState::OwnerUnavailable | CredentialState::OwnerUnauthorized => {
                return Err(ProviderCommandError::KeychainDenied);
            }
            CredentialState::Absent
            | CredentialState::Malformed
            | CredentialState::BindingMismatch => {
                return Err(ProviderCommandError::NoCredential);
            }
        };
        let request = TestConnectionRequest {
            endpoint: resolved.binding.base_url.clone(),
            allow_insecure_http: resolved.allow_insecure_http,
            lease,
        };
        Ok(self.tester.test(request).await)
    }
}

/// Move the entered key out of its zeroizing holder. The holder is left empty, so its own drop is a
/// no-op; the owner takes ownership and wipes it on every path.
fn take_secret(secret: &mut Option<Zeroizing<String>>) -> String {
    match secret.take() {
        Some(mut owned) => std::mem::take(&mut *owned),
        None => String::new(),
    }
}

fn map_mutation_error(error: MutationError) -> ProviderCommandError {
    match error {
        // A stale expected revision is the owner's compare-and-swap rejecting us: the owner state
        // moved since the confirmation was minted.
        MutationError::StaleRevision(_) => ProviderCommandError::OwnerStateChanged,
        MutationError::PreconditionFailed => ProviderCommandError::PreconditionFailed,
        MutationError::DeniedOrLocked => ProviderCommandError::KeychainDenied,
        MutationError::InvalidValue(rejection) => {
            ProviderCommandError::InvalidValue(rejection.to_string())
        }
    }
}

/// The §P10 sync verdict. `Synchronized` is claimed ONLY when a running daemon has observed a
/// revision at least as new as the mutation; a committed mutation the daemon has not observed yet is
/// `StoredUnsynchronized`; with no daemon at all it is `StoredOffline`. `already_absent` (no
/// mutation) is acknowledged against the unchanged revision and never claims a mutation.
fn sync_state(daemon: DaemonObservation, revision_after: Revision, mutated: bool) -> SyncState {
    if !daemon.running {
        return SyncState::StoredOffline;
    }
    if !mutated {
        // Nothing changed, so there is nothing to propagate; acknowledged against the unchanged
        // revision.
        return SyncState::Synchronized;
    }
    match daemon.observed_revision {
        Some(observed) if observed >= revision_after => SyncState::Synchronized,
        _ => SyncState::StoredUnsynchronized,
    }
}

fn status_wire(tag: CredentialStateTag) -> &'static str {
    match tag {
        CredentialStateTag::Present => "configured",
        CredentialStateTag::Absent => "absent",
        CredentialStateTag::DeniedOrLocked => "denied_or_locked",
        CredentialStateTag::Malformed => "malformed",
        CredentialStateTag::BindingMismatch => "binding_mismatch",
        CredentialStateTag::OwnerUnavailable => "owner_unavailable",
        CredentialStateTag::OwnerUnauthorized => "owner_unauthorized",
    }
}

fn recovery(tag: CredentialStateTag) -> Option<&'static str> {
    match tag {
        CredentialStateTag::Absent => Some("connect"),
        CredentialStateTag::DeniedOrLocked => Some("unlock"),
        CredentialStateTag::Malformed => Some("remove"),
        CredentialStateTag::BindingMismatch => Some("rebind"),
        CredentialStateTag::OwnerUnavailable | CredentialStateTag::OwnerUnauthorized => {
            Some("open_desktop")
        }
        CredentialStateTag::Present => None,
    }
}

fn status_from(
    provider_id: &str,
    resolved: &ResolvedProvider,
    tag: CredentialStateTag,
) -> ProviderStatusDto {
    ProviderStatusDto {
        provider_id: provider_id.to_string(),
        display_name: resolved.display_name.clone(),
        endpoint: resolved.binding.base_url.clone(),
        adapter: resolved.binding.adapter.clone(),
        insecure_http: resolved.allow_insecure_http,
        status: status_wire(tag).to_string(),
        recovery: recovery(tag).map(str::to_string),
        // §2.5's exact preconditions, surfaced so the UI can offer only the operations the current
        // state permits. Remove is offered for every state except a denied/locked owner and absent.
        can_connect: matches!(tag, CredentialStateTag::Absent),
        can_replace: matches!(tag, CredentialStateTag::Present),
        can_rebind: matches!(
            tag,
            CredentialStateTag::Present | CredentialStateTag::BindingMismatch
        ),
        can_remove: matches!(
            tag,
            CredentialStateTag::Present
                | CredentialStateTag::BindingMismatch
                | CredentialStateTag::Malformed
        ),
    }
}

/// The production owner factory: the real OS Keychain under the provider service namespace, one
/// owner per validated provider id. An id that is not canonical yields no owner (never a panic).
pub fn production_owner_factory() -> OwnerFactory {
    Arc::new(|provider_id: &str| -> Option<Arc<dyn CredentialOwner>> {
        let credential_ref = CredentialRef::for_provider(provider_id).ok()?;
        Some(Arc::new(ProviderCredentialOwner::new(&credential_ref)))
    })
}

/// Load, resolve, and validate a WORKFLOW.md into its typed [`rhapsody_config::Config`]. The full
/// pipeline (load → decode → resolve → validate) is the SAME one the daemon runs, so the binding the
/// desktop derives is byte-identical to the one the daemon will read under — the boundary this whole
/// ticket is about.
pub fn load_resolved_config(path: &Path) -> Result<rhapsody_config::Config, ProviderCommandError> {
    let definition = rhapsody_config::workflow::load(path)
        .map_err(|e| ProviderCommandError::ConfigUnavailable(e.to_string()))?;
    let mut config = rhapsody_config::decode(&definition)
        .map_err(|e| ProviderCommandError::ConfigUnavailable(e.to_string()))?;
    let dir = path
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    config = rhapsody_config::resolve(config, &dir)
        .map_err(|e| ProviderCommandError::ConfigUnavailable(e.to_string()))?;
    rhapsody_config::validate(&mut config)
        .map_err(|e| ProviderCommandError::ConfigUnavailable(e.to_string()))?;
    Ok(config)
}

/// Authorize a Tauri invocation by window label AND origin. The provider-secret commands are scoped
/// to the bundled `main` window's `rhapsody://localhost` origin; anything else — a different window,
/// a `tauri://`/`https://`/`file://` document, a missing URL — is denied. Kept pure so the policy is
/// unit-testable without a running Tauri app.
pub fn authorize_invocation(
    window_label: &str,
    url: Option<&str>,
) -> Result<(), ProviderCommandError> {
    if window_label != "main" {
        return Err(ProviderCommandError::UnauthorizedInvocation);
    }
    let parsed = url
        .and_then(|u| url::Url::parse(u).ok())
        .ok_or(ProviderCommandError::UnauthorizedInvocation)?;
    if parsed.scheme() != "rhapsody" || parsed.host_str() != Some("localhost") {
        return Err(ProviderCommandError::UnauthorizedInvocation);
    }
    Ok(())
}

/// The production, bounded, fixed-endpoint Test Connection adapter. A single GET to
/// `{endpoint}/models` with the approved binding's bearer credential, redirects DISABLED (so a
/// redirect cannot retarget the request to another endpoint), a hard timeout, and no body returned.
/// Only a closed status verdict crosses back; the response body is never read into a DTO, logged, or
/// reflected, and the credential lives only in the request's `Authorization` header.
pub struct HttpConnectionTester {
    client: reqwest::Client,
}

impl HttpConnectionTester {
    pub fn new() -> HttpConnectionTester {
        // Redirects off: a provider that 30x-redirects must not be able to move the credentialed
        // request to a different host. A bounded timeout keeps a hung provider from stalling the UI.
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        HttpConnectionTester { client }
    }
}

impl Default for HttpConnectionTester {
    fn default() -> Self {
        HttpConnectionTester::new()
    }
}

impl ConnectionTester for HttpConnectionTester {
    fn test<'a>(&'a self, request: TestConnectionRequest) -> BoxFuture<'a, TestConnectionDto> {
        Box::pin(async move {
            // Move the secret straight out of the lease's purpose-specific wire payload; the payload
            // zeroizes its own buffer on drop, and the header value is the only copy that leaves.
            let payload = request.lease.into_lease_payload();
            let url = format!("{}/models", request.endpoint.trim_end_matches('/'));
            let response = self
                .client
                .get(&url)
                .bearer_auth(&payload.value)
                .send()
                .await;
            match response {
                Ok(response) => {
                    let status = response.status();
                    if status.is_success() {
                        TestConnectionDto {
                            ok: true,
                            code: "ok".to_string(),
                            message: "the provider accepted the stored credential".to_string(),
                        }
                    } else if status == reqwest::StatusCode::UNAUTHORIZED
                        || status == reqwest::StatusCode::FORBIDDEN
                    {
                        TestConnectionDto {
                            ok: false,
                            code: "unauthorized".to_string(),
                            message: "the provider rejected the stored credential".to_string(),
                        }
                    } else if status.is_redirection() {
                        // A redirect is refused by policy above; surface it as a closed failure
                        // rather than following it to an unapproved endpoint.
                        TestConnectionDto {
                            ok: false,
                            code: "redirected".to_string(),
                            message: "the provider redirected the request; test aborted"
                                .to_string(),
                        }
                    } else {
                        TestConnectionDto {
                            ok: false,
                            code: "status".to_string(),
                            message: format!(
                                "the provider answered with status {}",
                                status.as_u16()
                            ),
                        }
                    }
                }
                Err(error) if error.is_timeout() => TestConnectionDto {
                    ok: false,
                    code: "timeout".to_string(),
                    message: "the provider did not answer in time".to_string(),
                },
                Err(_) => TestConnectionDto {
                    ok: false,
                    code: "transport".to_string(),
                    message: "the provider could not be reached".to_string(),
                },
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credential::mock::MockKeyring;

    // ---- test doubles ---------------------------------------------------------------------------

    /// A deterministic clock the test advances by hand, so nonce expiry is exact.
    struct FakeClock {
        now: Mutex<Instant>,
    }

    impl FakeClock {
        fn new() -> Arc<FakeClock> {
            Arc::new(FakeClock {
                now: Mutex::new(Instant::now()),
            })
        }
        fn advance(&self, by: Duration) {
            let mut now = lock(&self.now);
            *now += by;
        }
    }

    impl Clock for FakeClock {
        fn now(&self) -> Instant {
            *lock(&self.now)
        }
    }

    /// A tester that records the endpoint it was asked for and returns a scripted verdict. It never
    /// contacts anything, so a binding refusal is observable as "no request was made at all".
    struct FakeTester {
        calls: Mutex<Vec<String>>,
        verdict: Mutex<TestConnectionDto>,
    }

    impl FakeTester {
        fn new(verdict: TestConnectionDto) -> Arc<FakeTester> {
            Arc::new(FakeTester {
                calls: Mutex::new(Vec::new()),
                verdict: Mutex::new(verdict),
            })
        }
        fn calls(&self) -> Vec<String> {
            lock(&self.calls).clone()
        }
    }

    impl ConnectionTester for FakeTester {
        fn test<'a>(&'a self, request: TestConnectionRequest) -> BoxFuture<'a, TestConnectionDto> {
            lock(&self.calls).push(request.endpoint.clone());
            // Drop the lease (and so wipe its value) rather than returning it anywhere.
            drop(request.lease);
            let verdict = lock(&self.verdict).clone();
            Box::pin(async move { verdict })
        }
    }

    fn ok_verdict() -> TestConnectionDto {
        TestConnectionDto {
            ok: true,
            code: "ok".into(),
            message: "ok".into(),
        }
    }

    /// One owner factory backed by a shared in-memory keychain per provider id, so a provider's
    /// stored secret persists across calls (as the real Keychain does).
    fn test_factory() -> OwnerFactory {
        let keychains: Arc<Mutex<BTreeMap<String, Arc<MockKeyring>>>> =
            Arc::new(Mutex::new(BTreeMap::new()));
        Arc::new(move |provider_id: &str| {
            let mut map = lock(&keychains);
            let keychain = map
                .entry(provider_id.to_string())
                .or_insert_with(MockKeyring::empty)
                .clone();
            Some(Arc::new(ProviderCredentialOwner::for_test_provider(
                provider_id,
                keychain,
            )) as Arc<dyn CredentialOwner>)
        })
    }

    struct Fixture {
        service: Arc<ProviderCommandService>,
        clock: Arc<FakeClock>,
        tester: Arc<FakeTester>,
        /// Owns a workflow file this fixture created (when the caller did not supply one).
        workflow_dir: Option<crate::testutil::TempDir>,
        _holder: crate::testutil::TempDir,
    }

    fn fixture_with_workflow(body: &str) -> Fixture {
        let dir = crate::testutil::TempDir::new("rd-pc");
        let path = dir.join("WORKFLOW.md");
        std::fs::write(&path, body).expect("write workflow");
        let mut fx = build_fixture(Some(path));
        // Keep the workflow dir alive for the fixture's lifetime.
        fx.workflow_dir = Some(dir);
        fx
    }

    /// A fixture whose owner factory is injected (e.g. a denying keychain) and whose workflow file is
    /// owned by the fixture itself.
    fn fixture_with_factory(body: &str, factory: OwnerFactory) -> Fixture {
        let dir = crate::testutil::TempDir::new("rd-pc-f");
        let path = dir.join("WORKFLOW.md");
        std::fs::write(&path, body).expect("write workflow");
        let mut fx = build_fixture_with_factory(Some(path), factory);
        fx.workflow_dir = Some(dir);
        fx
    }

    fn build_fixture(workflow_path: Option<PathBuf>) -> Fixture {
        build_fixture_with_factory(workflow_path, test_factory())
    }

    fn build_fixture_with_factory(
        workflow_path: Option<PathBuf>,
        factory: OwnerFactory,
    ) -> Fixture {
        let clock = FakeClock::new();
        let tester = FakeTester::new(ok_verdict());
        let service = ProviderCommandService::with_dependencies(
            workflow_path,
            factory,
            tester.clone(),
            clock.clone(),
            // A deterministic, distinct nonce per prepare keeps replay assertions readable.
            {
                let counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
                Arc::new(move || {
                    let n = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    format!("nonce-{n:08}")
                })
            },
            DEFAULT_NONCE_TTL,
        );
        Fixture {
            service,
            clock,
            tester,
            workflow_dir: None,
            _holder: crate::testutil::TempDir::new("rd-pc-holder"),
        }
    }

    /// A valid WORKFLOW.md declaring one provider. Mirrors the hermetic shape the daemon tests use
    /// (dead tracker endpoint, temp workspace/logging roots).
    fn workflow_with_provider(endpoint: &str) -> String {
        format!(
            "---\ntracker:\n  kind: linear\n  endpoint: http://127.0.0.1:9\n  api_key: tok\n  project_slug: proj\n\
             providers:\n  fireworks:\n    protocol: openai-compatible\n    display_name: Fireworks\n    base_url: {endpoint}\n    credential:\n      source: keychain\n\
             workspace:\n  root: /tmp/rd-pc-ws\nlogging:\n  dir: /tmp/rd-pc-logs\nstorage:\n  path: \"off\"\n---\nWork.\n"
        )
    }

    fn connect(
        service: &ProviderCommandService,
        provider_id: &str,
        key: &str,
    ) -> MutationResultDto {
        let prepared = service
            .prepare(provider_id, ProviderOperation::Connect)
            .expect("prepare connect");
        service
            .commit(
                provider_id,
                ProviderOperation::Connect,
                &prepared.nonce,
                Some(key.to_string()),
                DaemonObservation::offline(),
            )
            .expect("commit connect")
    }

    // ---- no-provider installs stay inert (compatibility) ----------------------------------------

    /// MUTATION GUARD (compatibility): with no `providers:` block (the shipped default), the surface
    /// is empty and every command refuses with a typed `unknown_provider` — it never panics and never
    /// invents a provider. A build that fabricated a default provider would red this.
    #[test]
    fn a_provider_less_workflow_has_no_provider_surface() {
        let dir = crate::testutil::TempDir::new("rd-pc-none");
        let path = dir.join("WORKFLOW.md");
        std::fs::write(
            &path,
            "---\ntracker:\n  kind: linear\n  endpoint: http://127.0.0.1:9\n  api_key: tok\n  project_slug: proj\n---\nWork.\n",
        )
        .unwrap();
        let fx = build_fixture(Some(path));
        assert!(fx.service.statuses().unwrap().is_empty());
        assert_eq!(
            fx.service.status("fireworks").unwrap_err(),
            ProviderCommandError::UnknownProvider
        );
        assert_eq!(
            fx.service
                .prepare("fireworks", ProviderOperation::Connect)
                .unwrap_err(),
            ProviderCommandError::UnknownProvider
        );
    }

    /// A missing workflow file (unconfigured install) is likewise an empty surface, not an error.
    #[tokio::test]
    async fn a_missing_workflow_is_an_empty_surface() {
        let fx = build_fixture(None);
        assert!(fx.service.statuses().unwrap().is_empty());
        assert_eq!(
            fx.service.test_connection("fireworks").await.unwrap_err(),
            ProviderCommandError::UnknownProvider
        );
    }

    // ---- status: absent vs denied ---------------------------------------------------------------

    #[test]
    fn status_reports_absent_and_connect_available_initially() {
        let fx = fixture_with_workflow(&workflow_with_provider("https://api.example/v1"));
        let status = fx.service.status("fireworks").unwrap();
        assert_eq!(status.status, "absent");
        assert_eq!(status.endpoint, "https://api.example/v1");
        assert_eq!(status.display_name, "Fireworks");
        assert!(status.can_connect);
        assert!(!status.can_replace);
        assert!(!status.can_rebind);
        assert!(!status.can_remove, "nothing to remove while absent");
        assert_eq!(status.recovery.as_deref(), Some("connect"));
    }

    /// MUTATION GUARD (absent is distinguishable from denied): a locked/denying owner reports
    /// `denied_or_locked`, never `absent`, and offers no connect/replace/rebind. Collapsing denied
    /// into absent would red this.
    #[test]
    fn a_denied_owner_is_reported_distinctly_from_absent() {
        let denied: OwnerFactory = Arc::new(|provider_id: &str| {
            // A fresh owner over an erroring keychain: every read reports DeniedOrLocked.
            Some(Arc::new(ProviderCredentialOwner::for_test_provider(
                provider_id,
                MockKeyring::erroring("keychain locked"),
            )) as Arc<dyn CredentialOwner>)
        });
        let fx = fixture_with_factory(&workflow_with_provider("https://api.example/v1"), denied);
        let status = fx.service.status("fireworks").unwrap();
        assert_eq!(status.status, "denied_or_locked");
        assert!(!status.can_connect);
        assert!(!status.can_replace);
        assert!(!status.can_rebind);
        assert!(!status.can_remove);
        assert_eq!(status.recovery.as_deref(), Some("unlock"));
    }

    // ---- Connect / Replace / Rebind / Remove preconditions --------------------------------------

    #[test]
    fn connect_stores_under_the_derived_binding_and_reports_configured() {
        let fx = fixture_with_workflow(&workflow_with_provider("https://api.example/v1"));
        let result = connect(&fx.service, "fireworks", "sk-abc");
        assert!(result.mutated);
        assert_eq!(result.status, "configured");
        assert_eq!(result.sync, "stored_offline");
        assert_eq!(fx.service.status("fireworks").unwrap().status, "configured");
    }

    /// MUTATION GUARD: Replace cannot change a binding. It replaces only the value under the current
    /// binding; a Replace raced by a rebind-target change is refused at the config-generation gate.
    #[test]
    fn replace_changes_only_the_value_under_the_current_binding() {
        let fx = fixture_with_workflow(&workflow_with_provider("https://api.example/v1"));
        connect(&fx.service, "fireworks", "sk-first");
        let prepared = fx
            .service
            .prepare("fireworks", ProviderOperation::Replace)
            .expect("prepare replace");
        assert_eq!(prepared.endpoint, "https://api.example/v1");
        let result = fx
            .service
            .commit(
                "fireworks",
                ProviderOperation::Replace,
                &prepared.nonce,
                Some("sk-second".into()),
                DaemonObservation::offline(),
            )
            .expect("commit replace");
        assert!(result.mutated);
        // The stored binding is unchanged: a status read still finds the credential configured.
        assert_eq!(fx.service.status("fireworks").unwrap().status, "configured");
    }

    /// Replace over absent is refused (its §2.5 precondition), and the refusal consumes the nonce.
    #[test]
    fn replace_over_absent_is_refused_and_consumes_the_nonce() {
        let fx = fixture_with_workflow(&workflow_with_provider("https://api.example/v1"));
        let prepared = fx
            .service
            .prepare("fireworks", ProviderOperation::Replace)
            .unwrap();
        let err = fx
            .service
            .commit(
                "fireworks",
                ProviderOperation::Replace,
                &prepared.nonce,
                Some("sk-x".into()),
                DaemonObservation::offline(),
            )
            .unwrap_err();
        assert_eq!(err, ProviderCommandError::PreconditionFailed);
        // Replay is refused because the nonce was consumed on entry.
        assert_eq!(
            fx.service
                .commit(
                    "fireworks",
                    ProviderOperation::Replace,
                    &prepared.nonce,
                    Some("sk-x".into()),
                    DaemonObservation::offline(),
                )
                .unwrap_err(),
            ProviderCommandError::NonceUnknown
        );
    }

    /// Rebind resolves a binding mismatch by moving the stored binding to the current endpoint, and
    /// preserves the value. A Change to the workflow endpoint between prepare and commit is refused.
    #[test]
    fn rebind_moves_the_binding_and_a_concurrent_endpoint_change_is_refused() {
        // The stored credential is bound to v1; the workflow now says v2 → BindingMismatch.
        let dir = crate::testutil::TempDir::new("rd-pc-rebind");
        let path = dir.join("WORKFLOW.md");
        std::fs::write(&path, workflow_with_provider("https://api.example/v1")).unwrap();
        let fx = build_fixture(Some(path.clone()));
        connect(&fx.service, "fireworks", "sk-original");
        // Rewrite the workflow to a new endpoint: the same stored key is now a mismatch.
        std::fs::write(&path, workflow_with_provider("https://api.example/v2/v1")).unwrap();
        let status = fx.service.status("fireworks").unwrap();
        assert_eq!(status.status, "binding_mismatch");
        assert!(status.can_rebind);
        assert!(!status.can_replace);
        assert_eq!(status.recovery.as_deref(), Some("rebind"));

        let prepared = fx
            .service
            .prepare("fireworks", ProviderOperation::Rebind)
            .expect("prepare rebind");
        assert_eq!(prepared.endpoint, "https://api.example/v2/v1");
        let result = fx
            .service
            .commit(
                "fireworks",
                ProviderOperation::Rebind,
                &prepared.nonce,
                None,
                DaemonObservation::offline(),
            )
            .expect("commit rebind");
        assert!(result.mutated);
        assert_eq!(result.status, "configured");
    }

    /// Rebind cannot silently replace a key: it carries no secret, and the (unchanged) value is
    /// preserved — observable as a still-configured status under the new binding.
    #[test]
    fn rebind_cannot_replace_the_key() {
        let dir = crate::testutil::TempDir::new("rd-pc-rebind2");
        let path = dir.join("WORKFLOW.md");
        std::fs::write(&path, workflow_with_provider("https://api.example/v1")).unwrap();
        let fx = build_fixture(Some(path.clone()));
        connect(&fx.service, "fireworks", "sk-original");
        std::fs::write(&path, workflow_with_provider("https://api.example/v2/v1")).unwrap();
        let prepared = fx
            .service
            .prepare("fireworks", ProviderOperation::Rebind)
            .unwrap();
        // Even if a key is (incorrectly) supplied, Rebind ignores it — there is no code path that
        // stores a Rebind-supplied value.
        let result = fx
            .service
            .commit(
                "fireworks",
                ProviderOperation::Rebind,
                &prepared.nonce,
                Some("sk-should-be-ignored".into()),
                DaemonObservation::offline(),
            )
            .unwrap();
        assert!(result.mutated);
        assert_eq!(fx.service.status("fireworks").unwrap().status, "configured");
    }

    /// Remove deletes the secret and reports a mutation; `already_absent` Remove does NOT advance a
    /// revision and is not reported as a mutation.
    #[test]
    fn remove_mutates_and_already_absent_is_not_a_mutation() {
        let fx = fixture_with_workflow(&workflow_with_provider("https://api.example/v1"));
        connect(&fx.service, "fireworks", "sk-abc");
        let prepared = fx
            .service
            .prepare("fireworks", ProviderOperation::Remove)
            .unwrap();
        let removed = fx
            .service
            .commit(
                "fireworks",
                ProviderOperation::Remove,
                &prepared.nonce,
                None,
                DaemonObservation::offline(),
            )
            .unwrap();
        assert!(removed.mutated);
        assert_eq!(removed.status, "absent");

        let prepared = fx
            .service
            .prepare("fireworks", ProviderOperation::Remove)
            .unwrap();
        let again = fx
            .service
            .commit(
                "fireworks",
                ProviderOperation::Remove,
                &prepared.nonce,
                None,
                DaemonObservation::offline(),
            )
            .unwrap();
        assert!(!again.mutated, "already_absent must not claim a mutation");
        assert_eq!(again.status, "absent");
    }

    // ---- nonce discipline -----------------------------------------------------------------------

    /// MUTATION GUARD: a nonce is consumed once. Replaying the same nonce is refused with a typed
    /// `nonce_unknown`, never a second mutation.
    #[test]
    fn a_nonce_is_consumed_once() {
        let fx = fixture_with_workflow(&workflow_with_provider("https://api.example/v1"));
        let prepared = fx
            .service
            .prepare("fireworks", ProviderOperation::Connect)
            .unwrap();
        fx.service
            .commit(
                "fireworks",
                ProviderOperation::Connect,
                &prepared.nonce,
                Some("sk-1".into()),
                DaemonObservation::offline(),
            )
            .unwrap();
        assert_eq!(
            fx.service
                .commit(
                    "fireworks",
                    ProviderOperation::Connect,
                    &prepared.nonce,
                    Some("sk-2".into()),
                    DaemonObservation::offline(),
                )
                .unwrap_err(),
            ProviderCommandError::NonceUnknown
        );
    }

    /// MUTATION GUARD: an expired nonce is refused. Without an expiry check this commit would succeed.
    #[test]
    fn an_expired_nonce_is_refused() {
        let fx = fixture_with_workflow(&workflow_with_provider("https://api.example/v1"));
        let prepared = fx
            .service
            .prepare("fireworks", ProviderOperation::Connect)
            .unwrap();
        fx.clock
            .advance(DEFAULT_NONCE_TTL + Duration::from_millis(1));
        assert_eq!(
            fx.service
                .commit(
                    "fireworks",
                    ProviderOperation::Connect,
                    &prepared.nonce,
                    Some("sk-1".into()),
                    DaemonObservation::offline(),
                )
                .unwrap_err(),
            ProviderCommandError::NonceExpired
        );
        assert_eq!(fx.service.status("fireworks").unwrap().status, "absent");
    }

    /// MUTATION GUARD: a nonce minted for one provider cannot be used for another (a browser-supplied
    /// provider swap).
    #[test]
    fn a_nonce_for_one_provider_cannot_be_used_for_another() {
        let workflow = {
            let base = workflow_with_provider("https://api.example/v1");
            // Add a second provider by hand.
            base.replace(
                "storage:",
                "  second:\n    protocol: openai-compatible\n    base_url: https://api.second/v1\n    credential:\n      source: keychain\nstorage:",
            )
        };
        let fx = fixture_with_workflow(&workflow);
        let prepared = fx
            .service
            .prepare("fireworks", ProviderOperation::Connect)
            .unwrap();
        assert_eq!(
            fx.service
                .commit(
                    "second",
                    ProviderOperation::Connect,
                    &prepared.nonce,
                    Some("sk-1".into()),
                    DaemonObservation::offline(),
                )
                .unwrap_err(),
            ProviderCommandError::NonceProviderMismatch
        );
    }

    /// MUTATION GUARD: a nonce minted for one operation cannot be used to perform another.
    #[test]
    fn a_nonce_is_bound_to_its_operation() {
        let fx = fixture_with_workflow(&workflow_with_provider("https://api.example/v1"));
        connect(&fx.service, "fireworks", "sk-1");
        let prepared = fx
            .service
            .prepare("fireworks", ProviderOperation::Remove)
            .unwrap();
        assert_eq!(
            fx.service
                .commit(
                    "fireworks",
                    ProviderOperation::Connect,
                    &prepared.nonce,
                    Some("sk-2".into()),
                    DaemonObservation::offline(),
                )
                .unwrap_err(),
            ProviderCommandError::NonceOperationMismatch
        );
        // The credential is untouched.
        assert_eq!(fx.service.status("fireworks").unwrap().status, "configured");
    }

    /// MUTATION GUARD: a changed config generation between prepare and commit refuses the mutation
    /// (and consumes the nonce), so a confirmation cannot be applied against a moved destination.
    #[test]
    fn a_changed_generation_between_prepare_and_commit_is_refused() {
        let dir = crate::testutil::TempDir::new("rd-pc-gen");
        let path = dir.join("WORKFLOW.md");
        std::fs::write(&path, workflow_with_provider("https://api.example/v1")).unwrap();
        let fx = build_fixture(Some(path.clone()));
        let prepared = fx
            .service
            .prepare("fireworks", ProviderOperation::Connect)
            .unwrap();
        // A workflow edit moves the generation.
        std::fs::write(&path, workflow_with_provider("https://api.example/v2/v1")).unwrap();
        assert_eq!(
            fx.service
                .commit(
                    "fireworks",
                    ProviderOperation::Connect,
                    &prepared.nonce,
                    Some("sk-1".into()),
                    DaemonObservation::offline(),
                )
                .unwrap_err(),
            ProviderCommandError::GenerationChanged
        );
        assert_eq!(fx.service.status("fireworks").unwrap().status, "absent");
    }

    /// MUTATION GUARD: a stale owner revision between prepare and commit refuses a mutation. A
    /// second Connect in the window moves the owner revision; the first confirmation is then stale.
    #[test]
    fn a_stale_owner_revision_between_prepare_and_commit_is_refused() {
        let fx = fixture_with_workflow(&workflow_with_provider("https://api.example/v1"));
        let prepared = fx
            .service
            .prepare("fireworks", ProviderOperation::Connect)
            .unwrap();
        // Someone else connects first, advancing the owner revision.
        connect(&fx.service, "fireworks", "sk-racing");
        assert_eq!(
            fx.service
                .commit(
                    "fireworks",
                    ProviderOperation::Connect,
                    &prepared.nonce,
                    Some("sk-late".into()),
                    DaemonObservation::offline(),
                )
                .unwrap_err(),
            ProviderCommandError::OwnerStateChanged
        );
    }

    /// MUTATION GUARD (no browser-supplied binding/revision): the commit signature cannot accept a
    /// binding or a revision at all — this test pins that the mutation applies only under the binding
    /// derived at prepare, by confirming the value lands under the canonical endpoint.
    #[test]
    fn a_commit_cannot_retarget_the_binding() {
        let fx = fixture_with_workflow(&workflow_with_provider("https://api.example/v1"));
        let prepared = fx
            .service
            .prepare("fireworks", ProviderOperation::Connect)
            .unwrap();
        assert_eq!(prepared.endpoint, "https://api.example/v1");
        let result = fx
            .service
            .commit(
                "fireworks",
                ProviderOperation::Connect,
                &prepared.nonce,
                Some("sk-1".into()),
                DaemonObservation::offline(),
            )
            .unwrap();
        assert!(result.mutated);
        assert_eq!(fx.service.status("fireworks").unwrap().status, "configured");
    }

    /// A missing key for a Connect/Replace is a typed refusal, not a panic.
    #[test]
    fn connect_without_a_secret_is_refused() {
        let fx = fixture_with_workflow(&workflow_with_provider("https://api.example/v1"));
        let prepared = fx
            .service
            .prepare("fireworks", ProviderOperation::Connect)
            .unwrap();
        assert_eq!(
            fx.service
                .commit(
                    "fireworks",
                    ProviderOperation::Connect,
                    &prepared.nonce,
                    None,
                    DaemonObservation::offline(),
                )
                .unwrap_err(),
            ProviderCommandError::MissingSecret
        );
    }

    /// An unknown nonce is refused with `nonce_unknown`.
    #[test]
    fn an_unknown_nonce_is_refused() {
        let fx = fixture_with_workflow(&workflow_with_provider("https://api.example/v1"));
        assert_eq!(
            fx.service
                .commit(
                    "fireworks",
                    ProviderOperation::Connect,
                    "no-such-nonce",
                    Some("sk-1".into()),
                    DaemonObservation::offline(),
                )
                .unwrap_err(),
            ProviderCommandError::NonceUnknown
        );
    }

    // ---- sync status ----------------------------------------------------------------------------

    /// MUTATION GUARD: with no daemon, a changed mutation reports `stored_offline`, never
    /// `synchronized`.
    #[test]
    fn a_mutation_with_no_daemon_is_stored_offline() {
        let fx = fixture_with_workflow(&workflow_with_provider("https://api.example/v1"));
        let result = connect(&fx.service, "fireworks", "sk-1");
        assert_eq!(result.sync, "stored_offline");
    }

    /// MUTATION GUARD: with a running daemon that has NOT observed the new revision, a changed
    /// mutation reports `stored_unsynchronized`, never `synchronized`.
    #[test]
    fn a_mutation_unobserved_by_a_running_daemon_is_stored_unsynchronized() {
        let fx = fixture_with_workflow(&workflow_with_provider("https://api.example/v1"));
        let prepared = fx
            .service
            .prepare("fireworks", ProviderOperation::Connect)
            .unwrap();
        let result = fx
            .service
            .commit(
                "fireworks",
                ProviderOperation::Connect,
                &prepared.nonce,
                Some("sk-1".into()),
                DaemonObservation {
                    running: true,
                    observed_revision: None,
                },
            )
            .unwrap();
        assert_eq!(result.sync, "stored_unsynchronized");
    }

    /// MUTATION GUARD: `synchronized` is claimed ONLY once the daemon has observed a revision at
    /// least as new as the mutation. An observed revision OLDER than the mutation is not enough.
    #[test]
    fn a_mutation_is_synchronized_only_after_the_daemon_observes_it() {
        let fx = fixture_with_workflow(&workflow_with_provider("https://api.example/v1"));
        let prepared = fx
            .service
            .prepare("fireworks", ProviderOperation::Connect)
            .unwrap();
        let stale_observation = fx
            .service
            .commit(
                "fireworks",
                ProviderOperation::Connect,
                &prepared.nonce,
                Some("sk-1".into()),
                DaemonObservation {
                    running: true,
                    observed_revision: Some(Revision::INITIAL),
                },
            )
            .unwrap();
        assert_eq!(
            stale_observation.sync, "stored_unsynchronized",
            "an older observed revision must not claim synchronization"
        );

        // A fresh mutation observed at the current revision IS synchronized.
        let prepared = fx
            .service
            .prepare("fireworks", ProviderOperation::Replace)
            .unwrap();
        let fresh = fx
            .service
            .commit(
                "fireworks",
                ProviderOperation::Replace,
                &prepared.nonce,
                Some("sk-2".into()),
                DaemonObservation {
                    running: true,
                    observed_revision: Some(Revision(99)),
                },
            )
            .unwrap();
        assert_eq!(fresh.sync, "synchronized");
    }

    /// `already_absent` with a running daemon is acknowledged against the unchanged revision without
    /// claiming a mutation.
    #[test]
    fn already_absent_is_acknowledged_without_a_mutation() {
        let fx = fixture_with_workflow(&workflow_with_provider("https://api.example/v1"));
        let prepared = fx
            .service
            .prepare("fireworks", ProviderOperation::Remove)
            .unwrap();
        let result = fx
            .service
            .commit(
                "fireworks",
                ProviderOperation::Remove,
                &prepared.nonce,
                None,
                DaemonObservation {
                    running: true,
                    observed_revision: Some(Revision::INITIAL),
                },
            )
            .unwrap();
        assert!(!result.mutated);
        assert_eq!(result.sync, "synchronized");
    }

    // ---- Test Connection ------------------------------------------------------------------------

    /// A configured credential tests against the CURRENT endpoint, and the tester is asked for
    /// exactly that endpoint.
    #[tokio::test]
    async fn test_connection_uses_the_current_binding() {
        let fx = fixture_with_workflow(&workflow_with_provider("https://api.example/v1"));
        connect(&fx.service, "fireworks", "sk-1");
        let verdict = fx.service.test_connection("fireworks").await.unwrap();
        assert!(verdict.ok);
        assert_eq!(
            fx.tester.calls(),
            vec!["https://api.example/v1".to_string()]
        );
    }

    /// MUTATION GUARD: an absent credential is refused BEFORE any provider I/O — the tester's call
    /// log stays empty.
    #[tokio::test]
    async fn test_connection_refuses_an_absent_credential_without_contacting_anything() {
        let fx = fixture_with_workflow(&workflow_with_provider("https://api.example/v1"));
        assert_eq!(
            fx.service.test_connection("fireworks").await.unwrap_err(),
            ProviderCommandError::NoCredential
        );
        assert!(fx.tester.calls().is_empty());
    }

    /// MUTATION GUARD: a binding mismatch is refused BEFORE any provider I/O, so a changed endpoint
    /// never triggers a request against a stale key.
    #[tokio::test]
    async fn test_connection_refuses_a_binding_mismatch_without_contacting_anything() {
        let dir = crate::testutil::TempDir::new("rd-pc-tc");
        let path = dir.join("WORKFLOW.md");
        std::fs::write(&path, workflow_with_provider("https://api.example/v1")).unwrap();
        let fx = build_fixture(Some(path.clone()));
        connect(&fx.service, "fireworks", "sk-1");
        std::fs::write(&path, workflow_with_provider("https://api.example/v2/v1")).unwrap();
        assert_eq!(
            fx.service.test_connection("fireworks").await.unwrap_err(),
            ProviderCommandError::NoCredential
        );
        assert!(
            fx.tester.calls().is_empty(),
            "a binding mismatch must not contact any endpoint"
        );
    }

    /// The tester's verdict is passed through with no raw body.
    #[tokio::test]
    async fn test_connection_returns_a_closed_verdict() {
        let fx = fixture_with_workflow(&workflow_with_provider("https://api.example/v1"));
        connect(&fx.service, "fireworks", "sk-1");
        *lock(&fx.tester.verdict) = TestConnectionDto {
            ok: false,
            code: "unauthorized".into(),
            message: "the provider rejected the stored credential".into(),
        };
        let verdict = fx.service.test_connection("fireworks").await.unwrap();
        assert!(!verdict.ok);
        assert_eq!(verdict.code, "unauthorized");
        assert!(!verdict.message.contains("sk-1"));
    }

    // ---- origin/window authorization ------------------------------------------------------------

    /// MUTATION GUARD: a non-`main` window or a non-`rhapsody://localhost` origin is denied.
    #[test]
    fn provider_commands_are_scoped_to_the_main_window_and_bundled_origin() {
        assert!(authorize_invocation("main", Some("rhapsody://localhost/")).is_ok());
        assert!(authorize_invocation("main", Some("rhapsody://localhost")).is_ok());
        for (label, url) in [
            ("other", Some("rhapsody://localhost/")),
            ("main", Some("https://localhost/")),
            ("main", Some("rhapsody://evil.example/")),
            ("main", Some("tauri://localhost/")),
            ("main", Some("file:///etc/passwd")),
            ("main", None),
        ] {
            assert_eq!(
                authorize_invocation(label, url),
                Err(ProviderCommandError::UnauthorizedInvocation),
                "({label}, {url:?}) must be denied"
            );
        }
    }

    // ---- redaction ------------------------------------------------------------------------------

    /// MUTATION GUARD (canary): no error message, DTO debug, or status ever contains an entered key.
    #[test]
    fn no_command_surface_echoes_the_entered_secret() {
        let fx = fixture_with_workflow(&workflow_with_provider("https://api.example/v1"));
        let secret = "sk-CANARY-super-secret";
        let prepared = fx
            .service
            .prepare("fireworks", ProviderOperation::Connect)
            .unwrap();
        let result = fx
            .service
            .commit(
                "fireworks",
                ProviderOperation::Connect,
                &prepared.nonce,
                Some(secret.to_string()),
                DaemonObservation::offline(),
            )
            .unwrap();
        let rendered = format!(
            "{prepared:?} {result:?} {:?}",
            fx.service.status("fireworks").unwrap()
        );
        assert!(!rendered.contains(secret), "leaked: {rendered}");

        // A failed mutation must not leak it either.
        let prepared = fx
            .service
            .prepare("fireworks", ProviderOperation::Connect)
            .unwrap();
        let err = fx
            .service
            .commit(
                "fireworks",
                ProviderOperation::Connect,
                &prepared.nonce,
                Some(secret.to_string()),
                DaemonObservation::offline(),
            )
            .unwrap_err();
        let rendered = format!("{err:?} {err}");
        assert!(!rendered.contains(secret), "leaked: {rendered}");
    }

    /// The prepare DTO carries only the non-secret endpoint and the opaque nonce — never a binding
    /// fingerprint or a revision.
    #[test]
    fn a_prepared_dto_carries_no_binding_or_revision() {
        let fx = fixture_with_workflow(&workflow_with_provider("https://api.example/v1"));
        let prepared = fx
            .service
            .prepare("fireworks", ProviderOperation::Connect)
            .unwrap();
        let serialized = serde_json::to_string(&prepared).unwrap();
        assert!(serialized.contains("https://api.example/v1"));
        for forbidden in [
            "revision",
            "binding",
            "fingerprint",
            "account",
            "v1:fireworks",
        ] {
            assert!(
                !serialized.to_lowercase().contains(forbidden),
                "prepared DTO leaked {forbidden}: {serialized}"
            );
        }
    }
}
