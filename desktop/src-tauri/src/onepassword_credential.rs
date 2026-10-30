//! The desktop 1Password service-account credential command surface (STUDIO-1584). No Go parity —
//! Symphony never had a 1Password concept, so this is a Rhapsody-only addition (README "Divergences").
//!
//! A 1Password service-account token is stored through the SAME credential owner the provider keys
//! use ([`crate::provider_credential::ProviderCredentialOwner`]), only under the separate
//! [`rhapsody_credential_ipc::domain::ONEPASSWORD_SERVICE`] Keychain namespace and pinned to the
//! fixed `onepassword-service-account-v1` binding. That reuse is the point: the CAS semantics, the
//! envelope shape, the size/syntax bounds and the zeroization are all the reviewed provider paths,
//! not a second implementation.
//!
//! There is no prepare/commit nonce here, and that is deliberate: the binding is DERIVED from the
//! canonical id (the browser supplies only the id and the value), so there is nothing a caller
//! could retarget, and the expected revision is read Rust-side immediately before the CAS. The
//! browser never supplies a binding or a revision. The command surface is deliberately Tauri-free
//! so every operation and error class is covered by in-memory tests without the real Keychain.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

use serde::Serialize;

use rhapsody_credential_ipc::domain::{
    Binding, CredentialRef, CredentialStateTag, ONEPASSWORD_SERVICE_ACCOUNT_V1,
};
use rhapsody_credential_ipc::owner::{CredentialOwner, MutationError, MutationOutcome};

use crate::provider_credential::ProviderCredentialOwner;

/// The canonical token prefix every 1Password service-account token carries (shared with the config
/// id parser, which refuses a value with this prefix where an id belongs).
pub const ONEPASSWORD_TOKEN_PREFIX: &str = "ops_";

/// The fixed, canonical binding a 1Password credential is pinned to. Derived from the id — never
/// accepted from the caller — so a stored token can never be read under a different id.
pub fn onepassword_binding(id: &str) -> Binding {
    Binding {
        provider_id: id.to_string(),
        adapter: ONEPASSWORD_SERVICE_ACCOUNT_V1.to_string(),
        base_url: String::new(),
    }
}

/// How the service creates (and caches) the per-id credential owner. Injected so tests use
/// in-memory keychain doubles and production uses the real Keychain under the 1Password namespace.
pub type OnePasswordOwnerFactory = Arc<dyn Fn(&str) -> Option<Arc<dyn CredentialOwner>> + Send + Sync>;

/// The production owner factory: the real OS Keychain under the 1Password service namespace, one
/// owner per canonical id. A non-canonical id yields no owner (never a panic).
pub fn production_owner_factory() -> OnePasswordOwnerFactory {
    Arc::new(|id: &str| -> Option<Arc<dyn CredentialOwner>> {
        let owner = ProviderCredentialOwner::for_onepassword(id).ok()?;
        Some(Arc::new(owner))
    })
}

/// A typed, non-secret failure of a 1Password command. Every variant is closed and carries no token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OnePasswordCommandError {
    /// The id is not canonical (or is token-shaped, which the canonical parser refuses).
    InvalidId(String),
    /// A Connect/Replace arrived without a token.
    MissingToken,
    /// The candidate token did not look like a 1Password service-account token.
    InvalidToken(String),
    /// The Keychain refused the read/write (locked or access denied).
    KeychainDenied,
    /// The owner's precondition for this operation did not hold (e.g. Connect over Present).
    PreconditionFailed,
    /// The invocation did not come from the bundled `main` window at the `rhapsody://localhost`
    /// origin.
    UnauthorizedInvocation,
}

impl OnePasswordCommandError {
    /// The stable, non-secret wire code.
    pub fn code(&self) -> &'static str {
        match self {
            OnePasswordCommandError::InvalidId(_) => "invalid_id",
            OnePasswordCommandError::MissingToken => "missing_token",
            OnePasswordCommandError::InvalidToken(_) => "invalid_token",
            OnePasswordCommandError::KeychainDenied => "keychain_denied",
            OnePasswordCommandError::PreconditionFailed => "precondition_failed",
            OnePasswordCommandError::UnauthorizedInvocation => "unauthorized_invocation",
        }
    }

    /// A closed, actionable, operator-facing message. Never echoes a token.
    pub fn message(&self) -> String {
        match self {
            OnePasswordCommandError::InvalidId(_) => {
                "the credential id is not a canonical 1Password id".to_string()
            }
            OnePasswordCommandError::MissingToken => {
                "this action requires a service-account token".to_string()
            }
            OnePasswordCommandError::InvalidToken(reason) => {
                format!("the entered token was refused: {reason}")
            }
            OnePasswordCommandError::KeychainDenied => {
                "the login keychain refused access; unlock it and try again".to_string()
            }
            OnePasswordCommandError::PreconditionFailed => {
                "this action's precondition does not hold for the current credential state"
                    .to_string()
            }
            OnePasswordCommandError::UnauthorizedInvocation => {
                "this action is only available from the Rhapsody desktop window".to_string()
            }
        }
    }
}

impl std::fmt::Display for OnePasswordCommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for OnePasswordCommandError {}

/// A 1Password credential's non-secret status for the Settings form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OnePasswordStatusDto {
    pub id: String,
    /// `configured` | `unconfigured` | `unavailable`.
    pub status: String,
    pub recovery: Option<String>,
    pub can_connect: bool,
    pub can_replace: bool,
    pub can_remove: bool,
}

/// The result of a committed mutation. `mutated` is `false` for an already-absent Remove.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OnePasswordMutationDto {
    pub id: String,
    pub operation: String,
    pub mutated: bool,
    pub status: String,
}

/// The desktop-only 1Password credential command service. Cheap to hold behind an `Arc`.
pub struct OnePasswordCommandService {
    owners: Mutex<BTreeMap<String, Arc<dyn CredentialOwner>>>,
    owner_factory: OnePasswordOwnerFactory,
}

impl OnePasswordCommandService {
    /// The production service over the real OS Keychain.
    pub fn new() -> Arc<Self> {
        OnePasswordCommandService::with_owner_factory(production_owner_factory())
    }

    /// The full constructor with the owner factory injected (tests use keychain doubles).
    pub fn with_owner_factory(owner_factory: OnePasswordOwnerFactory) -> Arc<Self> {
        Arc::new(OnePasswordCommandService {
            owners: Mutex::new(BTreeMap::new()),
            owner_factory,
        })
    }

    /// The owner reached by a wire account string (`op:v1:<id>`) — the routing the supervisor's
    /// credential listener uses, so the daemon reads through the SAME cached owner the command
    /// surface mutates (two instances over one Keychain item would keep independent revision
    /// counters). A non-1Password or non-canonical account yields `None`, never another owner.
    pub fn owner_for_account(&self, account: &str) -> Option<Arc<dyn CredentialOwner>> {
        let id = account.strip_prefix(rhapsody_credential_ipc::domain::ONEPASSWORD_ACCOUNT_PREFIX)?;
        if CredentialRef::for_onepassword(id).is_err() {
            return None;
        }
        self.owner(id)
    }

    /// The cached owner for `id`, created on first use.
    fn owner(&self, id: &str) -> Option<Arc<dyn CredentialOwner>> {
        let mut owners = lock(&self.owners);
        if let Some(owner) = owners.get(id) {
            return Some(owner.clone());
        }
        let owner = (self.owner_factory)(id)?;
        owners.insert(id.to_string(), owner.clone());
        Some(owner)
    }

    /// One credential's non-secret status and the operations its state permits.
    pub fn status(&self, id: &str) -> Result<OnePasswordStatusDto, OnePasswordCommandError> {
        let id = validate_id(id)?;
        let owner = self
            .owner(&id)
            .ok_or_else(|| OnePasswordCommandError::InvalidId(id.clone()))?;
        let tag = owner.read_bound(&onepassword_binding(&id)).state.tag();
        Ok(status_from(&id, tag))
    }

    /// Connect: store `token` under the id, requiring the id to be currently absent.
    pub fn connect(
        &self,
        id: &str,
        token: String,
    ) -> Result<OnePasswordMutationDto, OnePasswordCommandError> {
        let id = validate_id(id)?;
        let token = validate_token(token)?;
        let owner = self
            .owner(&id)
            .ok_or_else(|| OnePasswordCommandError::InvalidId(id.clone()))?;
        let binding = onepassword_binding(&id);
        let revision = owner.read_bound(&binding).revision;
        match owner.connect(revision, binding, token) {
            Ok(MutationOutcome::Advanced(_)) => Ok(mutation_dto(&id, "connect", true, "configured")),
            Ok(MutationOutcome::AlreadyAbsent(_)) => {
                Ok(mutation_dto(&id, "connect", false, "unconfigured"))
            }
            Err(error) => Err(map_mutation(error)),
        }
    }

    /// Replace: change only the stored value, requiring the id to be currently present.
    pub fn replace(
        &self,
        id: &str,
        token: String,
    ) -> Result<OnePasswordMutationDto, OnePasswordCommandError> {
        let id = validate_id(id)?;
        let token = validate_token(token)?;
        let owner = self
            .owner(&id)
            .ok_or_else(|| OnePasswordCommandError::InvalidId(id.clone()))?;
        let binding = onepassword_binding(&id);
        let revision = owner.read_bound(&binding).revision;
        match owner.replace(revision, &binding, token) {
            Ok(MutationOutcome::Advanced(_)) => Ok(mutation_dto(&id, "replace", true, "configured")),
            Ok(MutationOutcome::AlreadyAbsent(_)) => {
                Ok(mutation_dto(&id, "replace", false, "unconfigured"))
            }
            Err(error) => Err(map_mutation(error)),
        }
    }

    /// Remove: delete the stored token. Removing an absent id is not a mutation.
    pub fn remove(&self, id: &str) -> Result<OnePasswordMutationDto, OnePasswordCommandError> {
        let id = validate_id(id)?;
        let owner = self
            .owner(&id)
            .ok_or_else(|| OnePasswordCommandError::InvalidId(id.clone()))?;
        let binding = onepassword_binding(&id);
        let revision = owner.read_bound(&binding).revision;
        match owner.remove(revision) {
            Ok(MutationOutcome::Advanced(_)) => Ok(mutation_dto(&id, "remove", true, "unconfigured")),
            Ok(MutationOutcome::AlreadyAbsent(_)) => {
                Ok(mutation_dto(&id, "remove", false, "unconfigured"))
            }
            Err(error) => Err(map_mutation(error)),
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Validate the id through the ONE canonical parser (the config side's own), so a token pasted in
/// place of an id is refused here exactly as it is refused at config load. Returns the id on success.
fn validate_id(id: &str) -> Result<String, OnePasswordCommandError> {
    rhapsody_config::onepassword::canonical_onepassword_id(id)
        .map_err(|_| OnePasswordCommandError::InvalidId(id.to_string()))?;
    Ok(id.to_string())
}

/// A 1Password service-account token must carry the documented `ops_` prefix; anything else is a
/// refusal rather than a stored non-token.
fn validate_token(token: String) -> Result<String, OnePasswordCommandError> {
    if token.is_empty() {
        return Err(OnePasswordCommandError::MissingToken);
    }
    if !token.starts_with(ONEPASSWORD_TOKEN_PREFIX) {
        return Err(OnePasswordCommandError::InvalidToken(
            "a service-account token starts with 'ops_'".to_string(),
        ));
    }
    Ok(token)
}

fn map_mutation(error: MutationError) -> OnePasswordCommandError {
    match error {
        MutationError::PreconditionFailed | MutationError::StaleRevision(_) => {
            OnePasswordCommandError::PreconditionFailed
        }
        MutationError::DeniedOrLocked => OnePasswordCommandError::KeychainDenied,
        MutationError::InvalidValue(_) => OnePasswordCommandError::InvalidToken(
            "the token violates the credential store's size or syntax bound".to_string(),
        ),
    }
}

fn status_from(id: &str, tag: CredentialStateTag) -> OnePasswordStatusDto {
    OnePasswordStatusDto {
        id: id.to_string(),
        status: status_wire(tag).to_string(),
        recovery: recovery(tag).map(str::to_string),
        can_connect: matches!(tag, CredentialStateTag::Absent),
        can_replace: matches!(tag, CredentialStateTag::Present),
        can_remove: matches!(
            tag,
            CredentialStateTag::Present
                | CredentialStateTag::BindingMismatch
                | CredentialStateTag::Malformed
        ),
    }
}

fn status_wire(tag: CredentialStateTag) -> &'static str {
    match tag {
        CredentialStateTag::Present => "configured",
        CredentialStateTag::Absent => "unconfigured",
        CredentialStateTag::DeniedOrLocked
        | CredentialStateTag::Malformed
        | CredentialStateTag::BindingMismatch
        | CredentialStateTag::OwnerUnavailable
        | CredentialStateTag::OwnerUnauthorized => "unavailable",
    }
}

fn recovery(tag: CredentialStateTag) -> Option<&'static str> {
    match tag {
        CredentialStateTag::Absent => Some("connect"),
        CredentialStateTag::DeniedOrLocked => Some("unlock"),
        CredentialStateTag::Malformed => Some("remove"),
        CredentialStateTag::BindingMismatch => Some("remove"),
        CredentialStateTag::OwnerUnavailable | CredentialStateTag::OwnerUnauthorized => {
            Some("open_desktop")
        }
        CredentialStateTag::Present => None,
    }
}

fn mutation_dto(id: &str, operation: &str, mutated: bool, status: &str) -> OnePasswordMutationDto {
    OnePasswordMutationDto {
        id: id.to_string(),
        operation: operation.to_string(),
        mutated,
        status: status.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credential::mock::MockKeyring;
    use rhapsody_credential_ipc::domain::{CredentialState, Revision};

    fn service_over(id: &str, keyring: Arc<MockKeyring>) -> Arc<OnePasswordCommandService> {
        let id_owned = id.to_string();
        let factory: OnePasswordOwnerFactory = Arc::new(move |requested: &str| {
            (requested == id_owned).then(|| {
                Arc::new(ProviderCredentialOwner::for_test_onepassword(
                    requested,
                    keyring.clone(),
                )) as Arc<dyn CredentialOwner>
            })
        });
        OnePasswordCommandService::with_owner_factory(factory)
    }

    #[test]
    fn connect_then_status_then_replace_then_remove_round_trips() {
        let svc = service_over("studio-infra-readonly", MockKeyring::empty());

        let status = svc.status("studio-infra-readonly").expect("status");
        assert_eq!(status.status, "unconfigured");
        assert!(status.can_connect);
        assert!(!status.can_replace);

        let connected = svc
            .connect("studio-infra-readonly", "ops_first_token".into())
            .expect("connect");
        assert!(connected.mutated);
        assert_eq!(connected.status, "configured");

        let status = svc.status("studio-infra-readonly").expect("status");
        assert_eq!(status.status, "configured");
        assert!(status.can_replace);
        assert!(status.can_remove);
        assert!(!status.can_connect);

        // A second Connect over a Present credential is a precondition failure, not an overwrite.
        assert_eq!(
            svc.connect("studio-infra-readonly", "ops_second".into()),
            Err(OnePasswordCommandError::PreconditionFailed)
        );

        let replaced = svc
            .replace("studio-infra-readonly", "ops_second_token".into())
            .expect("replace");
        assert!(replaced.mutated);

        let removed = svc.remove("studio-infra-readonly").expect("remove");
        assert!(removed.mutated);
        assert_eq!(
            svc.status("studio-infra-readonly").expect("status").status,
            "unconfigured"
        );

        // Removing again is not a mutation.
        let again = svc.remove("studio-infra-readonly").expect("remove");
        assert!(!again.mutated);
    }

    #[test]
    fn a_token_shaped_id_is_refused_and_never_echoed() {
        let svc = service_over("studio-infra-readonly", MockKeyring::empty());
        let err = svc
            .connect("ops_SENTINELSECRETVALUE123", "ops_token".into())
            .expect_err("token-shaped id refused");
        assert_eq!(err.code(), "invalid_id");
        assert!(!err.to_string().contains("SENTINELSECRETVALUE"));
    }

    #[test]
    fn a_non_token_value_is_refused() {
        let svc = service_over("studio-infra-readonly", MockKeyring::empty());
        let err = svc
            .connect("studio-infra-readonly", "not-a-service-account-token".into())
            .expect_err("non-token refused");
        assert_eq!(err.code(), "invalid_token");
        assert!(err.to_string().contains("ops_"));
    }

    #[test]
    fn owner_for_account_routes_only_onepassword_accounts() {
        let svc = service_over("studio-infra-readonly", MockKeyring::empty());
        assert!(svc.owner_for_account("op:v1:studio-infra-readonly").is_some());
        assert!(svc.owner_for_account("v1:anthropic").is_none());
        assert!(svc.owner_for_account("op:v1:Not An Id").is_none());
        assert!(svc.owner_for_account("op:studio-infra").is_none());
    }

    #[test]
    fn the_stored_envelope_binds_the_id_and_carries_the_token() {
        let keyring = MockKeyring::empty();
        let svc = service_over("studio-infra-readonly", keyring.clone());
        svc.connect("studio-infra-readonly", "ops_secret_value".into())
            .expect("connect");
        let owner = svc.owner("studio-infra-readonly").expect("owner");
        match owner.read_bound(&onepassword_binding("studio-infra-readonly")).state {
            CredentialState::Present(lease) => {
                assert_eq!(lease.into_lease_payload().value, "ops_secret_value")
            }
            other => panic!("expected Present, got {other:?}"),
        }
        // A different id's binding does not match the stored envelope.
        assert_eq!(
            owner
                .read_bound(&onepassword_binding("other-project"))
                .state
                .tag(),
            CredentialStateTag::BindingMismatch
        );
        // The revision advanced from INITIAL.
        assert!(owner.read_bound(&onepassword_binding("studio-infra-readonly")).revision > Revision::INITIAL);
    }
}
