//! The provider-credential owner (ticket STUDIO-981 / P0c). No Go parity — Symphony never had a
//! provider-credential feature, so this is a Rhapsody-only addition, extending the pattern
//! [`crate::credential`] established for the Linear token to a second, separate Keychain service
//! namespace (`rhapsody_credential_ipc::domain::PROVIDER_SERVICE`) with atomic
//! Connect/Replace/Rebind/Remove semantics and a non-secret owner revision (design §2.4/§2.5).
//!
//! `ProviderCredentialOwner` is scoped to ONE credential (one [`CredentialRef`]), mirroring
//! `credential::Keychain`'s single-item shape rather than building a multi-provider registry here —
//! that generalization belongs to the "P1 — Shared credential abstraction" ticket
//! `provider-auth-design.md` names as this ticket's dependent, not to the P0c spike/decision gate.
//!
//! Every mutation is a linearizable compare-and-swap against one `Mutex`-guarded revision, matching
//! §2.5's "every operation is compare-and-swap against one owner snapshot" — the same lock that
//! guards a read also guards every mutation, so a blocked read and a racing mutation cannot
//! interleave into an inconsistent snapshot (see `a_blocked_read_forces_a_concurrent_remove_to_
//! wait_and_keeps_its_old_revision` below). Every `read_bound`/mutation makes exactly ONE
//! `get_password` call and takes ITS returned revision (never a separately captured one) from
//! [`ProviderCredentialOwner::locked_snapshot`] — see its doc comment for the two review-found
//! defect shapes this closes and what "closes" does and does not guarantee. Neither is provable
//! by a black-box test alone (see the race test's own doc comment); this module's actual defense
//! is that both shapes now require a visibly deliberate code change to reintroduce, not a subtle
//! one-line oversight.

use std::sync::{Arc, Mutex, MutexGuard};

use rhapsody_credential_ipc::bounds;
use rhapsody_credential_ipc::domain::{
    Binding, BoundCredentialLease, CredentialRead, CredentialRef, CredentialState, Revision,
};
use rhapsody_credential_ipc::owner::{CredentialOwner, CredentialStatus};

// The typed mutation vocabulary now lives at the shared crate/process boundary (P1) so the daemon
// and desktop agree on it; re-exported here for the callers that already name these paths.
pub use rhapsody_credential_ipc::owner::{MutationError, MutationOutcome};

use crate::credential::{Keyring, KeyringError, OsKeyring};

/// The envelope actually persisted in the Keychain item (design §2.4). `version`/`kind` are pinned
/// to the only currently-supported shape (`1`/`"api_key"`); anything else — or a value that fails
/// to parse as this shape at all — decodes as [`CredentialState::Malformed`], not `Absent`.
// No `Clone`: the `value` is a secret, and a derived clone would silently create a second, un-owned
// copy of it beyond the one `BoundCredentialLease`/`Envelope` zeroizes.
#[derive(serde::Serialize, serde::Deserialize)]
struct Envelope {
    version: u32,
    kind: String,
    value: String,
    binding: Binding,
}

const ENVELOPE_VERSION: u32 = 1;
const ENVELOPE_KIND: &str = "api_key";

/// The classified result of one [`ProviderCredentialOwner::locked_snapshot`] call. Deliberately no
/// `Debug` derive — `Present` carries the secret `Envelope`, and this type existing at all is meant
/// to make an accidental `{:?}` leak harder, not easier, matching `Envelope`'s own lack of `Debug`.
enum Snapshot {
    DeniedOrLocked,
    Malformed,
    Absent,
    Present(Envelope),
}

/// Recovers a poisoned lock rather than propagating the panic (mirrors `supervisor::lock`) — the
/// guarded critical section is a plain revision read/CAS with no I/O held across an `.await`, so a
/// poisoned value is still internally consistent; there is no recoverable "error value" a caller
/// could act on differently than simply continuing with the value as it stood.
fn lock_revision(m: &Mutex<Revision>) -> MutexGuard<'_, Revision> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Owns exactly one provider credential's Keychain item and in-memory revision. `keyring` is
/// injectable so tests never touch the real OS Keychain (mirrors `credential::Keychain`'s own
/// `mock` seam).
pub struct ProviderCredentialOwner {
    account: String,
    keyring: Arc<dyn Keyring>,
    revision: Mutex<Revision>,
}

impl ProviderCredentialOwner {
    /// The production owner for `credential_ref`, backed by the real OS Keychain under the
    /// provider service namespace.
    pub fn new(credential_ref: &CredentialRef) -> ProviderCredentialOwner {
        ProviderCredentialOwner::with_keyring(
            credential_ref,
            Arc::new(OsKeyring {
                service: rhapsody_credential_ipc::domain::PROVIDER_SERVICE.to_string(),
                account: credential_ref.account().to_string(),
            }),
        )
    }

    fn with_keyring(
        credential_ref: &CredentialRef,
        keyring: Arc<dyn Keyring>,
    ) -> ProviderCredentialOwner {
        ProviderCredentialOwner {
            account: credential_ref.account().to_string(),
            keyring,
            revision: Mutex::new(Revision::INITIAL),
        }
    }

    /// The derived account this owner is bound to (e.g. `v1:anthropic`) — not a secret, safe to
    /// compare against a wire request. A caller (e.g. the IPC server) must reject any request whose
    /// account does not match this exactly, rather than answering for whatever the sole configured
    /// owner happens to be regardless of which credential was actually asked for.
    pub fn account(&self) -> &str {
        &self.account
    }

    /// A test-only constructor for other modules' tests (e.g. `credential_bootstrap`) that need a
    /// real owner over an injected keychain double without reaching into this module's private
    /// `with_keyring`. The credential ref is fixed to the same disposable test provider id every
    /// owner-level test in this crate already uses.
    #[cfg(test)]
    pub(crate) fn for_test(keyring: Arc<dyn Keyring>) -> ProviderCredentialOwner {
        let credential_ref = CredentialRef::for_provider("spike-test-provider").expect("valid id");
        ProviderCredentialOwner::with_keyring(&credential_ref, keyring)
    }

    /// Reads the current revision AND classifies the Keychain item in exactly ONE `get_password`
    /// call, under the caller's already-held revision lock, returning both PAIRED from this single
    /// call so a caller physically receives them together rather than assembling them from two
    /// separate reads. `read_bound` and every mutation used to make the Keychain call in two pieces
    /// (`probe_access` for the denied/locked check, then `locked_read` for the decode) — sol's
    /// review of rhapsody#213 found and reproduced the exact defect that shape invited: a caller
    /// drops the guard between the two calls and re-acquires a fresh one before the second, letting
    /// a concurrent mutation interleave and pair an old revision with post-mutation state. Folding
    /// the Keychain access into one call closed that gap. jimmy's follow-up review found a second,
    /// narrower version of the same defect: a caller could still capture the revision from its OWN
    /// separate `*guard` read (or an even-earlier, already-dropped temporary lock) instead of from
    /// this call's result, reintroducing a torn pair without ever touching the Keychain call twice.
    /// Returning `(Revision, Snapshot)` from here closes that too: `read_bound` and every mutation
    /// below build their returned/compared revision only from this pair, so reintroducing either
    /// shape of the defect requires a caller to visibly discard this return value's revision and
    /// substitute a different one, not merely to add one extra line.
    fn locked_snapshot(&self, guard: &MutexGuard<'_, Revision>) -> (Revision, Snapshot) {
        let revision = **guard;
        let snapshot = match self.keyring.get_password() {
            Ok(raw) if raw.is_empty() => Snapshot::Absent,
            Ok(raw) => match serde_json::from_str::<Envelope>(&raw) {
                Ok(env) if env.version == ENVELOPE_VERSION && env.kind == ENVELOPE_KIND => {
                    Snapshot::Present(env)
                }
                // A wrong version/kind, or undecodable data, is still "malformed" from this
                // owner's point of view — a future envelope version this build does not
                // understand must not be silently treated as absent.
                Ok(_) | Err(_) => Snapshot::Malformed,
            },
            Err(KeyringError::NoEntry) => Snapshot::Absent,
            Err(KeyringError::Other(_)) => Snapshot::DeniedOrLocked,
        };
        (revision, snapshot)
    }

    /// §2.5 `read_bound`: `Present` only when the stored envelope's binding matches
    /// `expected_binding` exactly; a well-formed envelope with a different binding is
    /// `BindingMismatch`, never a partial/raw disclosure of the stored endpoint or key.
    pub fn read_bound(&self, expected_binding: &Binding) -> CredentialRead {
        let guard = lock_revision(&self.revision);
        let (revision, snapshot) = self.locked_snapshot(&guard);
        let state = match snapshot {
            Snapshot::DeniedOrLocked => CredentialState::DeniedOrLocked,
            Snapshot::Malformed => CredentialState::Malformed,
            Snapshot::Absent => CredentialState::Absent,
            Snapshot::Present(env) if env.binding == *expected_binding => {
                CredentialState::Present(BoundCredentialLease::new(env.binding, env.value))
            }
            Snapshot::Present(env) => {
                // The value was decoded while checking the binding; on a mismatch it must be
                // zeroized before the mismatch leaves the boundary (§2.5). Routing it through the
                // shared lease and dropping it immediately does exactly that, so the mismatch arm
                // never leaves key bytes in a bare `String`.
                drop(BoundCredentialLease::new(env.binding, env.value));
                CredentialState::BindingMismatch
            }
        };
        CredentialRead { revision, state }
    }

    /// The owner's configured/unconfigured status, without ever returning the value (§2.5). A
    /// decoded value observed while classifying is zeroized on the way out, exactly as the
    /// `BindingMismatch` arm of `read_bound` does.
    pub fn status(&self) -> CredentialStatus {
        let guard = lock_revision(&self.revision);
        let (_revision, snapshot) = self.locked_snapshot(&guard);
        match snapshot {
            Snapshot::Present(env) => {
                drop(BoundCredentialLease::new(env.binding, env.value));
                CredentialStatus::Configured
            }
            Snapshot::Absent => CredentialStatus::Unconfigured,
            Snapshot::Malformed | Snapshot::DeniedOrLocked => CredentialStatus::Unavailable,
        }
    }

    /// Connect: requires `Absent` at `expected_revision`. Stores `value` under `binding`.
    pub fn connect(
        &self,
        expected_revision: Revision,
        binding: Binding,
        value: String,
    ) -> Result<MutationOutcome, MutationError> {
        let mut guard = lock_revision(&self.revision);
        let (revision, snapshot) = self.locked_snapshot(&guard);
        if matches!(snapshot, Snapshot::DeniedOrLocked) {
            return Err(MutationError::DeniedOrLocked);
        }
        if revision != expected_revision {
            return Err(MutationError::StaleRevision(revision));
        }
        if !matches!(snapshot, Snapshot::Absent) {
            return Err(MutationError::PreconditionFailed);
        }
        self.store_envelope(binding, value)?;
        *guard = guard.next();
        Ok(MutationOutcome::Advanced(*guard))
    }

    /// Replace: requires `Present` (well-formed, current binding) at `expected_revision`. Only the
    /// value changes; the binding is preserved exactly.
    pub fn replace(
        &self,
        expected_revision: Revision,
        current_binding: &Binding,
        new_value: String,
    ) -> Result<MutationOutcome, MutationError> {
        let mut guard = lock_revision(&self.revision);
        let (revision, snapshot) = self.locked_snapshot(&guard);
        if matches!(snapshot, Snapshot::DeniedOrLocked) {
            return Err(MutationError::DeniedOrLocked);
        }
        if revision != expected_revision {
            return Err(MutationError::StaleRevision(revision));
        }
        let matches_binding =
            matches!(&snapshot, Snapshot::Present(env) if env.binding == *current_binding);
        if !matches_binding {
            return Err(MutationError::PreconditionFailed);
        }
        self.store_envelope(current_binding.clone(), new_value)?;
        *guard = guard.next();
        Ok(MutationOutcome::Advanced(*guard))
    }

    /// Rebind: requires a well-formed (not malformed, not absent) envelope at `expected_revision` —
    /// its CURRENT binding may already equal `new_binding` or differ (a `BindingMismatch` read is
    /// exactly what Rebind exists to resolve). The value is preserved; only the binding changes.
    pub fn rebind(
        &self,
        expected_revision: Revision,
        new_binding: Binding,
    ) -> Result<MutationOutcome, MutationError> {
        let mut guard = lock_revision(&self.revision);
        let (revision, snapshot) = self.locked_snapshot(&guard);
        if matches!(snapshot, Snapshot::DeniedOrLocked) {
            return Err(MutationError::DeniedOrLocked);
        }
        if revision != expected_revision {
            return Err(MutationError::StaleRevision(revision));
        }
        let value = match snapshot {
            Snapshot::Present(env) => env.value,
            _ => return Err(MutationError::PreconditionFailed),
        };
        self.store_envelope(new_binding, value)?;
        *guard = guard.next();
        Ok(MutationOutcome::Advanced(*guard))
    }

    /// Remove: any non-absent envelope (`Present` OR `Malformed`) at `expected_revision` is
    /// deleted. Absent at the expected revision returns `already_absent` with the revision
    /// UNCHANGED. The revision itself is never deleted — it lives in this owner's `Mutex`, not in
    /// the Keychain item, so it stays observable after the secret bytes are gone.
    pub fn remove(&self, expected_revision: Revision) -> Result<MutationOutcome, MutationError> {
        let mut guard = lock_revision(&self.revision);
        let (revision, snapshot) = self.locked_snapshot(&guard);
        if matches!(snapshot, Snapshot::DeniedOrLocked) {
            return Err(MutationError::DeniedOrLocked);
        }
        if revision != expected_revision {
            return Err(MutationError::StaleRevision(revision));
        }
        if matches!(snapshot, Snapshot::Absent) {
            return Ok(MutationOutcome::AlreadyAbsent(revision));
        }
        self.keyring
            .delete_credential()
            .or_else(|e| match e {
                KeyringError::NoEntry => Ok(()),
                other => Err(other),
            })
            .map_err(|_| MutationError::DeniedOrLocked)?;
        *guard = guard.next();
        Ok(MutationOutcome::Advanced(*guard))
    }

    /// The owner's current revision, without touching the Keychain — used by tests that need to
    /// observe the revision after a Remove without going through `read_bound` (which would report
    /// `Absent` and could otherwise be mistaken for "no revision to observe").
    pub fn current_revision(&self) -> Revision {
        *lock_revision(&self.revision)
    }

    /// Validate the candidate value and the serialized envelope against the broker's exact
    /// size/syntax bounds BEFORE anything is written, then store it. A violation is a typed
    /// [`MutationError::InvalidValue`] and the value is never trimmed, rewritten, or stored (§2.5 /
    /// PB1 §3.2). The envelope-size check runs on the exact bytes about to be persisted, so a
    /// value that fits the API-key cap can still be refused when its binding pushes the envelope
    /// over the independent envelope cap.
    fn store_envelope(&self, binding: Binding, value: String) -> Result<(), MutationError> {
        bounds::validate_api_key_value(value.as_bytes()).map_err(MutationError::InvalidValue)?;
        let envelope = Envelope {
            version: ENVELOPE_VERSION,
            kind: ENVELOPE_KIND.to_string(),
            value,
            binding,
        };
        let raw = serde_json::to_string(&envelope).map_err(|_| MutationError::DeniedOrLocked)?;
        bounds::validate_envelope_size(&raw).map_err(MutationError::InvalidValue)?;
        self.keyring
            .set_password(&raw)
            .map_err(|_| MutationError::DeniedOrLocked)
    }
}

/// The owner implements the shared [`CredentialOwner`] abstraction (P1) — the crate/process
/// boundary the daemon and desktop build units both program against. The inherent methods above are
/// the implementation; this impl exposes them through the trait so a consumer can hold a
/// `dyn CredentialOwner` without knowing about the Keychain.
impl CredentialOwner for ProviderCredentialOwner {
    fn read_bound(&self, expected_binding: &Binding) -> CredentialRead {
        ProviderCredentialOwner::read_bound(self, expected_binding)
    }

    fn connect(
        &self,
        expected_revision: Revision,
        binding: Binding,
        value: String,
    ) -> Result<MutationOutcome, MutationError> {
        ProviderCredentialOwner::connect(self, expected_revision, binding, value)
    }

    fn replace(
        &self,
        expected_revision: Revision,
        current_binding: &Binding,
        new_value: String,
    ) -> Result<MutationOutcome, MutationError> {
        ProviderCredentialOwner::replace(self, expected_revision, current_binding, new_value)
    }

    fn rebind(
        &self,
        expected_revision: Revision,
        new_binding: Binding,
    ) -> Result<MutationOutcome, MutationError> {
        ProviderCredentialOwner::rebind(self, expected_revision, new_binding)
    }

    fn remove(&self, expected_revision: Revision) -> Result<MutationOutcome, MutationError> {
        ProviderCredentialOwner::remove(self, expected_revision)
    }

    fn status(&self) -> CredentialStatus {
        ProviderCredentialOwner::status(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credential::mock::MockKeyring;
    use rhapsody_credential_ipc::domain::CredentialStateTag;
    use std::thread;

    fn test_ref() -> CredentialRef {
        CredentialRef::for_provider("spike-test-provider").expect("valid id")
    }

    fn binding(url: &str) -> Binding {
        Binding {
            provider_id: "spike-test-provider".into(),
            adapter: "openai-chat-completions-bearer-v1".into(),
            base_url: url.to_string(),
        }
    }

    fn owner_over(keyring: Arc<MockKeyring>) -> ProviderCredentialOwner {
        ProviderCredentialOwner::with_keyring(&test_ref(), keyring)
    }

    // --- read_bound outcomes -------------------------------------------------------------------

    #[test]
    fn read_bound_reports_absent_when_nothing_is_stored() {
        let owner = owner_over(MockKeyring::empty());
        let read = owner.read_bound(&binding("https://api.example/v1"));
        assert_eq!(read.state.tag(), CredentialStateTag::Absent);
        assert_eq!(read.revision, Revision::INITIAL);
    }

    #[test]
    fn read_bound_reports_denied_or_locked_without_prompting_or_leaking_state() {
        let owner = owner_over(MockKeyring::erroring("keychain locked"));
        let read = owner.read_bound(&binding("https://api.example/v1"));
        assert_eq!(read.state.tag(), CredentialStateTag::DeniedOrLocked);
    }

    #[test]
    fn read_bound_reports_present_only_on_an_exact_binding_match() {
        let kr = MockKeyring::empty();
        let owner = owner_over(kr);
        let want = binding("https://api.example/v1");
        owner
            .connect(Revision::INITIAL, want.clone(), "sk-secret".into())
            .expect("connect");

        let ok = owner.read_bound(&want);
        assert_eq!(ok.state.tag(), CredentialStateTag::Present);
        match ok.state {
            CredentialState::Present(lease) => {
                assert_eq!(lease.expose_for_broker(str::to_owned), "sk-secret")
            }
            other => panic!("expected Present, got {other:?}"),
        }

        let mismatched = owner.read_bound(&binding("https://api.example/v2"));
        assert_eq!(mismatched.state.tag(), CredentialStateTag::BindingMismatch);
    }

    #[test]
    fn read_bound_reports_malformed_for_undecodable_data() {
        let kr = MockKeyring::empty();
        kr.set_password("not json at all").expect("seed garbage");
        let owner = owner_over(kr);
        let read = owner.read_bound(&binding("https://api.example/v1"));
        assert_eq!(read.state.tag(), CredentialStateTag::Malformed);
    }

    // --- Connect ---------------------------------------------------------------------------------

    #[test]
    fn connect_from_absent_advances_revision_and_stores_the_binding() {
        let owner = owner_over(MockKeyring::empty());
        let b = binding("https://api.example/v1");
        let outcome = owner
            .connect(Revision::INITIAL, b.clone(), "sk-1".into())
            .expect("connect succeeds");
        assert_eq!(outcome, MutationOutcome::Advanced(Revision(1)));
        assert_eq!(
            owner.read_bound(&b).state.tag(),
            CredentialStateTag::Present
        );
    }

    #[test]
    fn connect_against_a_present_credential_is_a_precondition_failure_and_does_not_mutate() {
        let owner = owner_over(MockKeyring::empty());
        let b = binding("https://api.example/v1");
        owner
            .connect(Revision::INITIAL, b.clone(), "sk-1".into())
            .expect("first connect");
        let before = owner.current_revision();

        let err = owner
            .connect(before, b.clone(), "sk-2".into())
            .expect_err("connect over Present must fail");
        assert_eq!(err, MutationError::PreconditionFailed);
        assert_eq!(
            owner.current_revision(),
            before,
            "revision must not advance on failure"
        );
        match owner.read_bound(&b).state {
            CredentialState::Present(lease) => {
                assert_eq!(
                    lease.expose_for_broker(str::to_owned),
                    "sk-1",
                    "value must be untouched"
                )
            }
            other => panic!("expected Present, got {other:?}"),
        }
    }

    #[test]
    fn connect_with_a_stale_expected_revision_is_rejected_and_reports_the_real_one() {
        let owner = owner_over(MockKeyring::empty());
        let stale = Revision(41);
        let err = owner
            .connect(stale, binding("https://api.example/v1"), "sk-1".into())
            .expect_err("stale revision rejected");
        assert_eq!(err, MutationError::StaleRevision(Revision::INITIAL));
        assert_eq!(owner.current_revision(), Revision::INITIAL);
    }

    // --- Replace ---------------------------------------------------------------------------------

    #[test]
    fn replace_changes_only_the_value_and_preserves_the_binding() {
        let owner = owner_over(MockKeyring::empty());
        let b = binding("https://api.example/v1");
        let r1 = match owner
            .connect(Revision::INITIAL, b.clone(), "sk-1".into())
            .unwrap()
        {
            MutationOutcome::Advanced(r) => r,
            other => panic!("{other:?}"),
        };

        let r2 = match owner.replace(r1, &b, "sk-2".into()).expect("replace") {
            MutationOutcome::Advanced(r) => r,
            other => panic!("{other:?}"),
        };
        assert!(r2 > r1);
        match owner.read_bound(&b).state {
            CredentialState::Present(lease) => {
                assert_eq!(lease.expose_for_broker(str::to_owned), "sk-2")
            }
            other => panic!("expected Present, got {other:?}"),
        }
    }

    #[test]
    fn replace_against_absent_is_a_precondition_failure() {
        let owner = owner_over(MockKeyring::empty());
        let err = owner
            .replace(
                Revision::INITIAL,
                &binding("https://api.example/v1"),
                "sk-2".into(),
            )
            .expect_err("replace over Absent must fail");
        assert_eq!(err, MutationError::PreconditionFailed);
    }

    #[test]
    fn replace_against_a_binding_mismatch_is_a_precondition_failure_not_a_silent_rebind() {
        let owner = owner_over(MockKeyring::empty());
        let original = binding("https://api.example/v1");
        let r1 = match owner
            .connect(Revision::INITIAL, original, "sk-1".into())
            .unwrap()
        {
            MutationOutcome::Advanced(r) => r,
            other => panic!("{other:?}"),
        };
        let different = binding("https://api.example/v2");
        let err = owner
            .replace(r1, &different, "sk-2".into())
            .expect_err("replace must not silently rebind");
        assert_eq!(err, MutationError::PreconditionFailed);
        assert_eq!(owner.current_revision(), r1, "no mutation on failure");
    }

    #[test]
    fn replace_against_malformed_data_is_a_precondition_failure() {
        let kr = MockKeyring::empty();
        kr.set_password("garbage").expect("seed");
        let owner = owner_over(kr);
        let err = owner
            .replace(
                Revision::INITIAL,
                &binding("https://api.example/v1"),
                "sk-2".into(),
            )
            .expect_err("replace over Malformed must fail");
        assert_eq!(err, MutationError::PreconditionFailed);
    }

    // --- Rebind ----------------------------------------------------------------------------------

    #[test]
    fn rebind_preserves_the_value_and_changes_only_the_binding() {
        let owner = owner_over(MockKeyring::empty());
        let original = binding("https://api.example/v1");
        let r1 = match owner
            .connect(Revision::INITIAL, original, "sk-1".into())
            .unwrap()
        {
            MutationOutcome::Advanced(r) => r,
            other => panic!("{other:?}"),
        };
        let target = binding("https://api.example/v2");
        owner.rebind(r1, target.clone()).expect("rebind");

        match owner.read_bound(&target).state {
            CredentialState::Present(lease) => {
                assert_eq!(lease.expose_for_broker(str::to_owned), "sk-1")
            }
            other => panic!("expected Present under the new binding, got {other:?}"),
        }
    }

    #[test]
    fn rebind_resolves_an_existing_binding_mismatch() {
        let owner = owner_over(MockKeyring::empty());
        let original = binding("https://api.example/v1");
        let r1 = match owner
            .connect(Revision::INITIAL, original, "sk-1".into())
            .unwrap()
        {
            MutationOutcome::Advanced(r) => r,
            other => panic!("{other:?}"),
        };
        let target = binding("https://api.example/v2");
        // Before Rebind, reading with the NEW binding as "expected" is a mismatch.
        assert_eq!(
            owner.read_bound(&target).state.tag(),
            CredentialStateTag::BindingMismatch
        );
        owner
            .rebind(r1, target.clone())
            .expect("rebind resolves the mismatch");
        assert_eq!(
            owner.read_bound(&target).state.tag(),
            CredentialStateTag::Present
        );
    }

    #[test]
    fn rebind_against_absent_is_a_precondition_failure() {
        let owner = owner_over(MockKeyring::empty());
        let err = owner
            .rebind(Revision::INITIAL, binding("https://api.example/v2"))
            .expect_err("rebind over Absent must fail");
        assert_eq!(err, MutationError::PreconditionFailed);
    }

    #[test]
    fn rebind_against_malformed_data_is_a_precondition_failure() {
        let kr = MockKeyring::empty();
        kr.set_password("garbage").expect("seed");
        let owner = owner_over(kr);
        let err = owner
            .rebind(Revision::INITIAL, binding("https://api.example/v2"))
            .expect_err("rebind over Malformed must fail — only Remove may touch it");
        assert_eq!(err, MutationError::PreconditionFailed);
    }

    // --- Remove ----------------------------------------------------------------------------------

    #[test]
    fn remove_deletes_the_secret_but_the_revision_stays_observable() {
        let owner = owner_over(MockKeyring::empty());
        let b = binding("https://api.example/v1");
        let r1 = match owner
            .connect(Revision::INITIAL, b.clone(), "sk-1".into())
            .unwrap()
        {
            MutationOutcome::Advanced(r) => r,
            other => panic!("{other:?}"),
        };

        let r2 = match owner.remove(r1).expect("remove") {
            MutationOutcome::Advanced(r) => r,
            other => panic!("{other:?}"),
        };
        assert!(r2 > r1, "remove must advance the revision");

        // The secret is gone (a stale in-flight Present read cannot win)...
        let after = owner.read_bound(&b);
        assert_eq!(after.state.tag(), CredentialStateTag::Absent);
        // ...but the revision remains observable and matches what Remove returned, exactly the
        // property a stale caller's cached revision must be checked against.
        assert_eq!(after.revision, r2);
        assert_eq!(owner.current_revision(), r2);
    }

    #[test]
    fn remove_can_delete_malformed_data() {
        let kr = MockKeyring::empty();
        kr.set_password("garbage").expect("seed");
        let owner = owner_over(kr);
        let outcome = owner
            .remove(Revision::INITIAL)
            .expect("remove malformed succeeds");
        assert_eq!(outcome, MutationOutcome::Advanced(Revision(1)));
        assert_eq!(
            owner.read_bound(&binding("https://x")).state.tag(),
            CredentialStateTag::Absent
        );
    }

    #[test]
    fn remove_when_already_absent_reports_already_absent_and_does_not_advance_revision() {
        let owner = owner_over(MockKeyring::empty());
        let outcome = owner
            .remove(Revision::INITIAL)
            .expect("remove of absent succeeds");
        assert_eq!(outcome, MutationOutcome::AlreadyAbsent(Revision::INITIAL));
        assert_eq!(
            owner.current_revision(),
            Revision::INITIAL,
            "must not advance"
        );
    }

    // --- Stale/conflicting mutations across every operation ---------------------------------------

    #[test]
    fn every_mutation_rejects_a_stale_revision_without_changing_state_or_revision() {
        let owner = owner_over(MockKeyring::empty());
        let b = binding("https://api.example/v1");
        let r1 = match owner
            .connect(Revision::INITIAL, b.clone(), "sk-1".into())
            .unwrap()
        {
            MutationOutcome::Advanced(r) => r,
            other => panic!("{other:?}"),
        };
        let stale = Revision::INITIAL; // no longer current, now that r1 > stale

        assert_eq!(
            owner.connect(stale, b.clone(), "x".into()),
            Err(MutationError::StaleRevision(r1))
        );
        assert_eq!(
            owner.replace(stale, &b, "x".into()),
            Err(MutationError::StaleRevision(r1))
        );
        assert_eq!(
            owner.rebind(stale, binding("https://api.example/v2")),
            Err(MutationError::StaleRevision(r1))
        );
        assert_eq!(owner.remove(stale), Err(MutationError::StaleRevision(r1)));

        assert_eq!(
            owner.current_revision(),
            r1,
            "no failed CAS attempt may advance the revision"
        );
        match owner.read_bound(&b).state {
            CredentialState::Present(lease) => {
                assert_eq!(lease.expose_for_broker(str::to_owned), "sk-1")
            }
            other => panic!("expected untouched Present, got {other:?}"),
        }
    }

    // --- The race PB7 depends on: a blocked read must retain its OLD revision, never a torn one ---

    /// A `Keyring` double whose `get_password()` blocks on the FIRST call until the test explicitly
    /// releases it, signaling the test the instant it is entered. `locked_snapshot` now makes
    /// exactly one `get_password` call per `read_bound`/mutation, under one continuously held
    /// revision lock — so pausing that one call pins the WHOLE operation's critical section open
    /// deterministically (there is no second call, hence no gap a paused mock could straddle). A
    /// concurrently spawned mutation contending for the same real `Mutex` is therefore guaranteed
    /// by the lock itself, not merely likely by a wide sleep window, to be unable to proceed until
    /// the test releases this call.
    struct BlockingKeyring {
        inner: Arc<MockKeyring>,
        entered_tx: Mutex<Option<std::sync::mpsc::Sender<()>>>,
        release_rx: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    }

    impl BlockingKeyring {
        fn new(
            inner: Arc<MockKeyring>,
        ) -> (
            Arc<BlockingKeyring>,
            std::sync::mpsc::Receiver<()>,
            std::sync::mpsc::Sender<()>,
        ) {
            let (entered_tx, entered_rx) = std::sync::mpsc::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let kr = Arc::new(BlockingKeyring {
                inner,
                entered_tx: Mutex::new(Some(entered_tx)),
                release_rx: Mutex::new(Some(release_rx)),
            });
            (kr, entered_rx, release_tx)
        }
    }

    impl crate::credential::Keyring for BlockingKeyring {
        fn get_password(&self) -> Result<String, crate::credential::KeyringError> {
            if let Some(tx) = self.entered_tx.lock().unwrap().take() {
                let _ = tx.send(());
                if let Some(rx) = self.release_rx.lock().unwrap().take() {
                    let _ = rx.recv();
                }
            }
            self.inner.get_password()
        }
        fn set_password(&self, token: &str) -> Result<(), crate::credential::KeyringError> {
            self.inner.set_password(token)
        }
        fn delete_credential(&self) -> Result<(), crate::credential::KeyringError> {
            self.inner.delete_credential()
        }
    }

    // Mutation discipline: "Delete revision state with the credential; a blocked old Present read
    // after Remove must incorrectly win and fail the race test." This test pins what it CAN pin
    // deterministically: while `locked_snapshot`'s one `get_password` call is paused, it genuinely
    // holds the revision lock (proven below by `!remove_handle.is_finished()`, not merely asserted
    // in a comment), so a concurrent Remove cannot advance the revision until this read releases it
    // — real mutual exclusion, not a `Barrier` ordering two sequential operations on one thread
    // each, which is what an earlier shape of this test did (sol's review of rhapsody#213) and
    // which a Keychain-call-count mutation (probe/read split, jimmy's B4) later showed to still
    // pass green. What this test does NOT and cannot pin, because it is a black-box test with no
    // I/O to intercept in the gap: a caller that captures the revision from a SEPARATE, already-
    // released lock instead of from `locked_snapshot`'s returned pair (jimmy's follow-up, B6). That
    // shape is closed by `locked_snapshot` returning `(Revision, Snapshot)` together and every
    // caller using only that pair — see its doc comment — which makes the defect require a caller
    // to visibly discard the returned revision and substitute a different one, not something this
    // or any black-box test can observe directly.
    #[test]
    fn a_blocked_read_forces_a_concurrent_remove_to_wait_and_keeps_its_old_revision() {
        let backing = MockKeyring::empty();
        let b = binding("https://api.example/v1");
        let envelope = Envelope {
            version: ENVELOPE_VERSION,
            kind: ENVELOPE_KIND.to_string(),
            value: "sk-1".into(),
            binding: b.clone(),
        };
        backing
            .set_password(&serde_json::to_string(&envelope).unwrap())
            .expect("seed the backing keychain directly, bypassing the owner under test");

        let (blocking_kr, entered_rx, release_tx) = BlockingKeyring::new(backing);
        let owner = Arc::new(ProviderCredentialOwner::with_keyring(
            &test_ref(),
            blocking_kr,
        ));

        let read_owner = owner.clone();
        let read_binding = b.clone();
        let read_handle = thread::spawn(move || read_owner.read_bound(&read_binding));

        entered_rx
            .recv()
            .expect("read must signal it entered its single locked_snapshot call");

        let remove_owner = owner.clone();
        let remove_handle = thread::spawn(move || remove_owner.remove(Revision::INITIAL));

        // Deterministic, not a race: `read_bound` holds the revision lock for its entire body, so
        // `remove` cannot even acquire the lock — let alone finish — while the read is paused here,
        // guaranteed by the real `Mutex` rather than by outrunning the OS scheduler. A brief sleep
        // just gives `remove` a chance to actually reach and park on that lock before we check.
        thread::sleep(std::time::Duration::from_millis(50));
        assert!(
            !remove_handle.is_finished(),
            "remove must still be blocked on the revision lock while the read is paused inside it"
        );

        release_tx
            .send(())
            .expect("release the paused read so both threads can finish");

        let blocked_snapshot = read_handle.join().expect("read thread");
        let removed = remove_handle.join().expect("remove thread");

        match &blocked_snapshot.state {
            CredentialState::Present(lease) => {
                assert_eq!(lease.expose_for_broker(str::to_owned), "sk-1")
            }
            other => panic!(
                "a read already in flight when Remove starts must still observe the pre-Remove \
                 value as a self-consistent snapshot, got {other:?}"
            ),
        }
        assert_eq!(blocked_snapshot.revision, Revision::INITIAL);

        let removed_revision = match removed {
            Ok(MutationOutcome::Advanced(r)) => r,
            other => panic!("{other:?}"),
        };
        assert!(
            removed_revision > blocked_snapshot.revision,
            "post-remove revision must have moved past the pre-remove snapshot"
        );
        // PB7's rejection rule falls straight out of this: `blocked_snapshot.revision != current`.
        assert_eq!(owner.current_revision(), removed_revision);
        assert_ne!(blocked_snapshot.revision, owner.current_revision());
    }

    // --- Behavior across a simulated desktop/daemon restart ---------------------------------------

    // A "restart" is a fresh `ProviderCredentialOwner` over the SAME persisted Keychain backend
    // (the real OS Keychain survives a process restart; only this owner's in-memory revision
    // counter does not). The acceptance requirement is behavioral: BindingMismatch/Present
    // detection must still be correct against the persisted envelope after restart, with no lease
    // released until an explicit Rebind — restarting the revision counter's own numeric baseline is
    // a deliberate, documented P0c-scope simplification (a caller must always re-`read_bound`
    // rather than reuse a pre-restart cached revision across a reconnect; see the module doc), not
    // a violation of this requirement.
    #[test]
    fn binding_mismatch_and_present_detection_survive_a_simulated_restart() {
        let backend = MockKeyring::empty();
        let original = binding("https://api.example/v1");

        // "Before restart": connect under the original binding.
        let before_restart = owner_over(backend.clone());
        before_restart
            .connect(Revision::INITIAL, original.clone(), "sk-1".into())
            .expect("connect before restart");

        // "After restart": a brand-new owner instance, same persisted backend.
        let after_restart = owner_over(backend);

        // The persisted secret is still readable under its original binding...
        match after_restart.read_bound(&original).state {
            CredentialState::Present(lease) => {
                assert_eq!(lease.expose_for_broker(str::to_owned), "sk-1")
            }
            other => panic!("expected Present to survive restart, got {other:?}"),
        }
        // ...but a changed canonical endpoint reports BindingMismatch, not a lease.
        let changed = binding("https://api.example/v2");
        assert_eq!(
            after_restart.read_bound(&changed).state.tag(),
            CredentialStateTag::BindingMismatch
        );

        // Only an explicit Rebind (against the post-restart owner's own current revision) may
        // change the binding; it still preserves the value.
        let post_restart_revision = after_restart.current_revision();
        after_restart
            .rebind(post_restart_revision, changed.clone())
            .expect("rebind after restart");
        match after_restart.read_bound(&changed).state {
            CredentialState::Present(lease) => {
                assert_eq!(lease.expose_for_broker(str::to_owned), "sk-1")
            }
            other => panic!("expected Present under the new binding, got {other:?}"),
        }
    }

    // --- Redaction -------------------------------------------------------------------------------

    #[test]
    fn credential_read_debug_never_leaks_the_secret() {
        let owner = owner_over(MockKeyring::empty());
        let b = binding("https://api.example/v1");
        owner
            .connect(Revision::INITIAL, b.clone(), "sk-super-secret".into())
            .unwrap();
        let read = owner.read_bound(&b);
        let rendered = format!("{read:?}");
        assert!(!rendered.contains("sk-super-secret"), "leaked: {rendered}");
    }

    // --- Broker bounds enforced before storage (P1 acceptance / §2.5) ---------------------------

    // Mutation discipline: if the bounds check is moved after the write (or dropped), this test
    // fails because the refusals stop happening and/or a value lands in the Keychain.
    #[test]
    fn connect_refuses_an_out_of_bounds_value_before_storing_anything() {
        let kr = MockKeyring::empty();
        let owner = owner_over(kr.clone());
        let b = binding("https://api.example/v1");
        let cases: Vec<(&str, String)> = vec![
            ("empty", String::new()),
            ("space", "sk key with a space".into()),
            ("control", "sk\nkey".into()),
            ("too long", "a".repeat(bounds::MAX_API_KEY_BYTES + 1)),
        ];
        for (name, bad) in cases {
            let err = owner
                .connect(Revision::INITIAL, b.clone(), bad)
                .expect_err("an out-of-bounds value must be refused");
            assert!(
                matches!(err, MutationError::InvalidValue(_)),
                "{name}: expected InvalidValue, got {err:?}"
            );
        }
        assert_eq!(
            owner.current_revision(),
            Revision::INITIAL,
            "no refused write may advance the revision"
        );
        assert_eq!(
            owner.read_bound(&b).state.tag(),
            CredentialStateTag::Absent,
            "no refused write may store anything"
        );
    }

    #[test]
    fn connect_refuses_a_too_long_value_with_the_brokers_typed_reason() {
        let owner = owner_over(MockKeyring::empty());
        let too_long = "a".repeat(bounds::MAX_API_KEY_BYTES + 1);
        let err = owner
            .connect(
                Revision::INITIAL,
                binding("https://api.example/v1"),
                too_long,
            )
            .expect_err("over the API-key cap");
        assert_eq!(
            err,
            MutationError::InvalidValue(
                rhapsody_credential_ipc::bounds::CredentialRejection::TooLong
            )
        );
    }

    // A value that fits the API-key cap can still push the serialized envelope past the
    // independently-capped envelope size; the exact bytes about to be stored are checked, and the
    // caller is refused rather than having anything trimmed.
    #[test]
    fn connect_refuses_an_envelope_over_the_cap_even_with_a_valid_value() {
        let owner = owner_over(MockKeyring::empty());
        let huge = binding(&format!(
            "https://api.example/v1/{}",
            "x".repeat(bounds::MAX_CREDENTIAL_ENVELOPE_BYTES)
        ));
        let err = owner
            .connect(Revision::INITIAL, huge, "sk-valid".into())
            .expect_err("an oversized envelope must be refused");
        assert!(matches!(err, MutationError::InvalidValue(_)), "got {err:?}");
        assert_eq!(owner.current_revision(), Revision::INITIAL);
    }

    #[test]
    fn replace_refuses_an_out_of_bounds_value_and_leaves_the_old_value() {
        let owner = owner_over(MockKeyring::empty());
        let b = binding("https://api.example/v1");
        let r1 = match owner
            .connect(Revision::INITIAL, b.clone(), "sk-1".into())
            .unwrap()
        {
            MutationOutcome::Advanced(r) => r,
            other => panic!("{other:?}"),
        };
        let err = owner
            .replace(r1, &b, "sk has a space".into())
            .expect_err("replace with an invalid value must be refused");
        assert!(matches!(err, MutationError::InvalidValue(_)), "got {err:?}");
        assert_eq!(owner.current_revision(), r1);
        match owner.read_bound(&b).state {
            CredentialState::Present(lease) => {
                assert_eq!(lease.expose_for_broker(str::to_owned), "sk-1")
            }
            other => panic!("expected untouched Present, got {other:?}"),
        }
    }

    // A refusal must not echo the rejected value into its error string (secret canary across the
    // error surface).
    #[test]
    fn a_bounds_refusal_never_echoes_the_rejected_value() {
        let owner = owner_over(MockKeyring::empty());
        let err = owner
            .connect(
                Revision::INITIAL,
                binding("https://api.example/v1"),
                "sk-canary-secret with a space".into(),
            )
            .expect_err("refused");
        let rendered = format!("{err:?}");
        assert!(!rendered.contains("canary-secret"), "leaked: {rendered}");
    }

    // --- Configured/unconfigured status (shared abstraction, §2.5) -------------------------------

    #[test]
    fn status_reports_configured_unconfigured_and_unavailable() {
        let empty = owner_over(MockKeyring::empty());
        assert_eq!(empty.status(), CredentialStatus::Unconfigured);
        let b = binding("https://api.example/v1");
        empty
            .connect(Revision::INITIAL, b.clone(), "sk-1".into())
            .expect("connect");
        assert_eq!(empty.status(), CredentialStatus::Configured);

        assert_eq!(
            owner_over(MockKeyring::erroring("locked")).status(),
            CredentialStatus::Unavailable
        );

        let malformed = MockKeyring::empty();
        malformed.set_password("not an envelope").expect("seed");
        assert_eq!(
            owner_over(malformed).status(),
            CredentialStatus::Unavailable,
            "present-but-unusable data must never read as configured"
        );
    }

    // The desktop owner is usable through the shared abstraction the daemon programs against — this
    // is the build-unit boundary P1 requires, exercised as a trait object.
    #[test]
    fn the_owner_is_usable_through_the_shared_credential_owner_abstraction() {
        let owner: Box<dyn CredentialOwner> = Box::new(owner_over(MockKeyring::empty()));
        let b = binding("https://api.example/v1");
        assert_eq!(owner.status(), CredentialStatus::Unconfigured);
        owner
            .connect(Revision::INITIAL, b.clone(), "sk-1".into())
            .expect("connect through the trait");
        assert_eq!(owner.status(), CredentialStatus::Configured);
        match owner.read_bound(&b).state {
            CredentialState::Present(lease) => {
                // The value only ever leaves through a closure-scoped borrow or a move into the
                // broker — no ordinary string getter exists on the shared lease.
                assert_eq!(lease.expose_for_broker(str::to_owned), "sk-1");
            }
            other => panic!("expected Present, got {other:?}"),
        }
        assert_eq!(
            owner.remove(Revision(1)),
            Ok(MutationOutcome::Advanced(Revision(2)))
        );
    }
}
