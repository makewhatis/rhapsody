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
//! interleave into an inconsistent snapshot (see the `race_*` test below).

use std::sync::{Arc, Mutex, MutexGuard};

use rhapsody_credential_ipc::domain::{
    Binding, BoundCredentialLease, CredentialRead, CredentialRef, CredentialState, Revision,
};

use crate::credential::{Keyring, KeyringError, OsKeyring};

/// The envelope actually persisted in the Keychain item (design §2.4). `version`/`kind` are pinned
/// to the only currently-supported shape (`1`/`"api_key"`); anything else — or a value that fails
/// to parse as this shape at all — decodes as [`CredentialState::Malformed`], not `Absent`.
#[derive(serde::Serialize, serde::Deserialize, Clone)]
struct Envelope {
    version: u32,
    kind: String,
    value: String,
    binding: Binding,
}

const ENVELOPE_VERSION: u32 = 1;
const ENVELOPE_KIND: &str = "api_key";

/// One CAS mutation's success outcome (§2.5's table).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MutationOutcome {
    /// The mutation applied; the owner's revision is now this value.
    Advanced(Revision),
    /// Remove against an already-`Absent` credential: no state change, per §2.5's table — the
    /// returned revision is the unchanged current one.
    AlreadyAbsent(Revision),
}

/// One CAS mutation's failure. Every variant leaves the owner's state and revision untouched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MutationError {
    /// `expected_revision` did not match the owner's actual current revision — covers a stale
    /// read, a concurrent conflicting mutation, and a previously failed/rolled-back attempt alike.
    /// Carries the real current revision so a caller can decide whether to retry.
    StaleRevision(Revision),
    /// The expected revision matched, but this operation's state precondition did not (e.g.
    /// Connect against a `Present` item, Replace/Rebind against `Absent`, Replace against a
    /// binding mismatch, or Replace/Rebind against `Malformed` data).
    PreconditionFailed,
    /// The Keychain itself refused the read/write right now (locked or access denied).
    DeniedOrLocked,
}

/// Owns exactly one provider credential's Keychain item and in-memory revision. `keyring` is
/// injectable so tests never touch the real OS Keychain (mirrors `credential::Keychain`'s own
/// `mock` seam).
pub struct ProviderCredentialOwner {
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
        _credential_ref: &CredentialRef,
        keyring: Arc<dyn Keyring>,
    ) -> ProviderCredentialOwner {
        ProviderCredentialOwner {
            keyring,
            revision: Mutex::new(Revision::INITIAL),
        }
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

    /// Reads the raw envelope, mapping every outcome to a `(decoded envelope option, revision)`
    /// pair under the single lock — the atomicity §2.5 requires between state and revision. Held
    /// only internally; callers get a [`CredentialRead`] from [`Self::read_bound`].
    fn locked_read(&self, guard: &MutexGuard<'_, Revision>) -> Result<Option<Envelope>, ()> {
        match self.keyring.get_password() {
            Ok(raw) if raw.is_empty() => Ok(None),
            Ok(raw) => match serde_json::from_str::<Envelope>(&raw) {
                Ok(env) if env.version == ENVELOPE_VERSION && env.kind == ENVELOPE_KIND => {
                    Ok(Some(env))
                }
                // A wrong version/kind is still "malformed" from this owner's point of view — a
                // future envelope version this build does not understand must not be silently
                // treated as absent.
                Ok(_) => Err(()),
                Err(_) => Err(()),
            },
            Err(KeyringError::NoEntry) => Ok(None),
            Err(KeyringError::Other(_)) => {
                // Denied/locked is reported to the caller of `read_bound`/mutations, not via this
                // internal `Result`; see the callers, which special-case it before calling here.
                // (Kept as a distinct branch, not folded into `Err(())`, purely for readability —
                // callers never actually reach it because they probe with `probe_access` first.)
                let _ = guard;
                Err(())
            }
        }
    }

    /// Distinguishes "denied/locked" from "malformed"/"absent" without yet deciding which one a
    /// caller needs — both `locked_read`'s `Err`-on-decode-failure and `Err`-on-Keychain-failure
    /// collapse to the same `Result<_, ()>`, so mutation/read paths call this first.
    fn probe_access(&self) -> Result<(), MutationError> {
        match self.keyring.get_password() {
            Ok(_) => Ok(()),
            Err(KeyringError::NoEntry) => Ok(()),
            Err(KeyringError::Other(_)) => Err(MutationError::DeniedOrLocked),
        }
    }

    /// §2.5 `read_bound`: `Present` only when the stored envelope's binding matches
    /// `expected_binding` exactly; a well-formed envelope with a different binding is
    /// `BindingMismatch`, never a partial/raw disclosure of the stored endpoint or key.
    pub fn read_bound(&self, expected_binding: &Binding) -> CredentialRead {
        let guard = self.revision.lock().expect("revision mutex poisoned");
        if self.probe_access().is_err() {
            return CredentialRead {
                revision: *guard,
                state: CredentialState::DeniedOrLocked,
            };
        }
        let state = match self.locked_read(&guard) {
            Err(()) => CredentialState::Malformed,
            Ok(None) => CredentialState::Absent,
            Ok(Some(env)) if env.binding == *expected_binding => {
                CredentialState::Present(BoundCredentialLease::new(env.binding, env.value))
            }
            Ok(Some(_)) => CredentialState::BindingMismatch,
        };
        CredentialRead {
            revision: *guard,
            state,
        }
    }

    /// Connect: requires `Absent` at `expected_revision`. Stores `value` under `binding`.
    pub fn connect(
        &self,
        expected_revision: Revision,
        binding: Binding,
        value: String,
    ) -> Result<MutationOutcome, MutationError> {
        let mut guard = self.revision.lock().expect("revision mutex poisoned");
        self.probe_access()?;
        if *guard != expected_revision {
            return Err(MutationError::StaleRevision(*guard));
        }
        let current = self.locked_read(&guard);
        let is_absent = matches!(current, Ok(None));
        if !is_absent {
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
        let mut guard = self.revision.lock().expect("revision mutex poisoned");
        self.probe_access()?;
        if *guard != expected_revision {
            return Err(MutationError::StaleRevision(*guard));
        }
        let current = self.locked_read(&guard);
        let matches_binding = matches!(&current, Ok(Some(env)) if env.binding == *current_binding);
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
        let mut guard = self.revision.lock().expect("revision mutex poisoned");
        self.probe_access()?;
        if *guard != expected_revision {
            return Err(MutationError::StaleRevision(*guard));
        }
        let current = self.locked_read(&guard);
        let value = match current {
            Ok(Some(env)) => env.value,
            Ok(None) | Err(()) => return Err(MutationError::PreconditionFailed),
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
        let mut guard = self.revision.lock().expect("revision mutex poisoned");
        self.probe_access()?;
        if *guard != expected_revision {
            return Err(MutationError::StaleRevision(*guard));
        }
        let current = self.locked_read(&guard);
        let is_absent = matches!(current, Ok(None));
        if is_absent {
            return Ok(MutationOutcome::AlreadyAbsent(*guard));
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
        *self.revision.lock().expect("revision mutex poisoned")
    }

    fn store_envelope(&self, binding: Binding, value: String) -> Result<(), MutationError> {
        let envelope = Envelope {
            version: ENVELOPE_VERSION,
            kind: ENVELOPE_KIND.to_string(),
            value,
            binding,
        };
        let raw = serde_json::to_string(&envelope).map_err(|_| MutationError::DeniedOrLocked)?;
        self.keyring
            .set_password(&raw)
            .map_err(|_| MutationError::DeniedOrLocked)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credential::mock::MockKeyring;
    use rhapsody_credential_ipc::domain::CredentialStateTag;
    use std::sync::Barrier;
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
            CredentialState::Present(lease) => assert_eq!(lease.expose_secret(), "sk-secret"),
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
                assert_eq!(lease.expose_secret(), "sk-1", "value must be untouched")
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
            CredentialState::Present(lease) => assert_eq!(lease.expose_secret(), "sk-2"),
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
            CredentialState::Present(lease) => assert_eq!(lease.expose_secret(), "sk-1"),
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
            CredentialState::Present(lease) => assert_eq!(lease.expose_secret(), "sk-1"),
            other => panic!("expected untouched Present, got {other:?}"),
        }
    }

    // --- The race PB7 depends on: a blocked read must retain its OLD revision, never a torn one ---

    // Mutation discipline: "Delete revision state with the credential; a blocked old Present read
    // after Remove must incorrectly win and fail the race test." This test pins the opposite: the
    // snapshot a blocked reader captured BEFORE a concurrent Remove must keep its pre-Remove
    // revision, so a caller comparing it against the post-Remove revision correctly sees it as
    // stale and rejects it — exactly what a defect deleting revision alongside the secret breaks.
    #[test]
    fn a_read_snapshot_taken_before_a_concurrent_remove_keeps_its_old_revision() {
        let owner = Arc::new(owner_over(MockKeyring::empty()));
        let b = binding("https://api.example/v1");
        let r1 = match owner
            .connect(Revision::INITIAL, b.clone(), "sk-1".into())
            .unwrap()
        {
            MutationOutcome::Advanced(r) => r,
            other => panic!("{other:?}"),
        };

        // `read_bound` and every mutation take the SAME mutex, so a "blocked owner read" racing a
        // mutation can never observe a torn state — one completes fully before the other starts.
        // We pin exactly that ordering property here: a snapshot taken before Remove keeps its
        // pre-Remove revision even once Remove has run on another thread, so a caller (PB7) that
        // compares a cached revision against the current one correctly rejects the stale snapshot
        // rather than treating it as still current.
        let barrier = Arc::new(Barrier::new(2));
        let blocked_snapshot = owner.read_bound(&b);
        assert_eq!(blocked_snapshot.revision, r1);

        let owner2 = owner.clone();
        let barrier2 = barrier.clone();
        let handle = thread::spawn(move || {
            barrier2.wait();
            owner2.remove(r1)
        });
        barrier.wait();
        let removed = handle.join().expect("remove thread");
        assert!(matches!(removed, Ok(MutationOutcome::Advanced(_))));

        let current = owner.current_revision();
        assert!(
            current > blocked_snapshot.revision,
            "post-remove revision must have moved past the pre-remove snapshot"
        );
        // PB7's rejection rule falls straight out of this: `blocked_snapshot.revision != current`.
        assert_ne!(blocked_snapshot.revision, current);
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
            CredentialState::Present(lease) => assert_eq!(lease.expose_secret(), "sk-1"),
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
            CredentialState::Present(lease) => assert_eq!(lease.expose_secret(), "sk-1"),
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
}
