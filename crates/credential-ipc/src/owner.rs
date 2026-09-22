//! The shared credential-owner abstraction (P1, design §2.5).
//!
//! This is the crate/process boundary STUDIO-981 (P0c) selected: the desktop build unit *implements*
//! the abstraction over the macOS Keychain (`desktop::provider_credential::ProviderCredentialOwner`),
//! and the daemon build unit *consumes* it — either directly, through
//! `rhapsodyd::credential_client`'s authenticated IPC client, or through any future adapter. Every
//! type the contract mentions lives here, at the boundary, so there is exactly one definition of
//! the read states, the mutation outcomes, and the revision, shared by both build units.
//!
//! The abstraction is deliberately the *only* mutation surface: there is no unconditional `set` or
//! `delete`. Connect/Replace/Rebind/Remove each carry an expected opaque [`Revision`] and are
//! compare-and-swap operations against one owner snapshot (design §2.5's table). A browser-directed
//! unconditional write is not merely absent from the API — it has no variant to express.

use crate::bounds::CredentialRejection;
use crate::domain::{Binding, CredentialRead, Revision};

/// One CAS mutation's success outcome (design §2.5's table).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MutationOutcome {
    /// The mutation applied; the owner's revision is now this value.
    Advanced(Revision),
    /// Remove against an already-`Absent` credential: no state change, per §2.5's table — the
    /// returned revision is the unchanged current one.
    AlreadyAbsent(Revision),
}

/// One CAS mutation's failure. No variant writes a partial secret: a stale revision, a changed
/// precondition, a denied/locked owner, and a value that violates the broker's bounds are all typed
/// refusals, never a partial write.
///
/// The owner's SECRET state is left untouched by every variant, but the opaque [`Revision`] is not
/// universally frozen: an availability transition observed *during* the attempt advances it (design
/// §2.5 — those transitions must change the revision so a refusal gate re-arms). [`StaleRevision`]
/// carries whatever the current revision then is; a [`DeniedOrLocked`](MutationError::DeniedOrLocked)
/// caller is not told the new value, so it must re-`read_bound` before retrying rather than reuse the
/// revision it passed in — the unlock is itself a second transition, so a blind retry at the old
/// revision is refused with `StaleRevision`.
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
    /// The candidate value or envelope violated the broker's size/syntax bound. Carries the broker's
    /// own typed reason; the value was NOT trimmed or written.
    InvalidValue(CredentialRejection),
}

/// Configured/unconfigured status without returning the value (design §2.5). `Unavailable` covers
/// both an owner that cannot be reached and one that exists but cannot be used (locked, or holding
/// malformed data) — in none of those cases may a caller treat the credential as usable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialStatus {
    /// A well-formed credential is stored.
    Configured,
    /// No credential is stored.
    Unconfigured,
    /// An item may exist, but the owner cannot currently yield a usable credential (locked/denied,
    /// or malformed data).
    Unavailable,
}

/// The credential-owner abstraction: the one testable seam every build unit programs against.
///
/// Implementations are expected to serialize every operation under one lock so a read and a racing
/// mutation cannot interleave into a torn snapshot, and to carry the current non-secret
/// [`Revision`] on every outcome — including the non-`Present` ones. Only
/// [`CredentialState::Present`](crate::domain::CredentialState::Present) carries a
/// [`BoundCredentialLease`](crate::domain::BoundCredentialLease), and only when the expected binding
/// matches the stored one exactly.
pub trait CredentialOwner: Send + Sync {
    /// Read the bound credential. `Present` requires an exact match of `expected_binding`; a
    /// well-formed stored envelope under a different binding is a typed
    /// [`BindingMismatch`](crate::domain::CredentialState::BindingMismatch) that discloses neither
    /// the stored endpoint/binding nor the key.
    fn read_bound(&self, expected_binding: &Binding) -> CredentialRead;

    /// Connect: requires `Absent` at `expected_revision`; stores the value under `binding`.
    fn connect(
        &self,
        expected_revision: Revision,
        binding: Binding,
        value: String,
    ) -> Result<MutationOutcome, MutationError>;

    /// Replace: requires `Present` at `expected_revision` under `current_binding`; changes only the
    /// value, preserving the binding exactly.
    fn replace(
        &self,
        expected_revision: Revision,
        current_binding: &Binding,
        new_value: String,
    ) -> Result<MutationOutcome, MutationError>;

    /// Rebind: requires a well-formed envelope at `expected_revision`; preserves the value and
    /// changes only the binding.
    fn rebind(
        &self,
        expected_revision: Revision,
        new_binding: Binding,
    ) -> Result<MutationOutcome, MutationError>;

    /// Remove: deletes any non-absent envelope at `expected_revision`, retaining only the
    /// non-secret revision metadata; `already_absent` returns the unchanged revision.
    fn remove(&self, expected_revision: Revision) -> Result<MutationOutcome, MutationError>;

    /// Configured/unconfigured status, without returning the value.
    fn status(&self) -> CredentialStatus;
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::domain::{BoundCredentialLease, CredentialState};

    /// A minimal injected-storage `CredentialOwner` double. Its only purpose is to prove the
    /// abstraction is implementable and testable entirely in memory at the boundary — downstream
    /// slices can hold a `dyn CredentialOwner` without a Keychain, a socket, or a desktop build.
    /// The production CAS semantics (and their mutation-discipline tests) live in the desktop owner,
    /// which is the one owner that touches bytes; this double is deliberately simple and is not a
    /// second production path.
    struct InMemoryOwner {
        state: Mutex<Option<(Binding, String)>>,
        revision: Mutex<Revision>,
    }

    impl InMemoryOwner {
        fn new() -> InMemoryOwner {
            InMemoryOwner {
                state: Mutex::new(None),
                revision: Mutex::new(Revision::INITIAL),
            }
        }
    }

    fn binding(url: &str) -> Binding {
        Binding {
            provider_id: "p".into(),
            adapter: "openai-chat-completions-bearer-v1".into(),
            base_url: url.into(),
        }
    }

    impl CredentialOwner for InMemoryOwner {
        fn read_bound(&self, expected_binding: &Binding) -> CredentialRead {
            let revision = *self.revision.lock().expect("revision lock");
            let state = self.state.lock().expect("state lock");
            let state = match state.as_ref() {
                None => CredentialState::Absent,
                // The double copies for the lease; the production owner moves the owner-held bytes.
                Some((b, v)) if b == expected_binding => {
                    CredentialState::Present(BoundCredentialLease::new(b.clone(), v.clone()))
                }
                Some(_) => CredentialState::BindingMismatch,
            };
            CredentialRead { revision, state }
        }

        fn connect(
            &self,
            expected_revision: Revision,
            binding: Binding,
            value: String,
        ) -> Result<MutationOutcome, MutationError> {
            let current = self.current();
            if expected_revision != current {
                return Err(MutationError::StaleRevision(current));
            }
            let mut state = self.state.lock().expect("state lock");
            if state.is_some() {
                return Err(MutationError::PreconditionFailed);
            }
            *state = Some((binding, value));
            Ok(MutationOutcome::Advanced(self.advance()))
        }

        fn replace(
            &self,
            expected_revision: Revision,
            current_binding: &Binding,
            new_value: String,
        ) -> Result<MutationOutcome, MutationError> {
            let current = self.current();
            if expected_revision != current {
                return Err(MutationError::StaleRevision(current));
            }
            let mut state = self.state.lock().expect("state lock");
            match state.as_ref() {
                Some((b, _)) if b == current_binding => {}
                _ => return Err(MutationError::PreconditionFailed),
            }
            let existing = state.take().expect("present");
            *state = Some((existing.0, new_value));
            Ok(MutationOutcome::Advanced(self.advance()))
        }

        fn rebind(
            &self,
            expected_revision: Revision,
            new_binding: Binding,
        ) -> Result<MutationOutcome, MutationError> {
            let current = self.current();
            if expected_revision != current {
                return Err(MutationError::StaleRevision(current));
            }
            let mut state = self.state.lock().expect("state lock");
            let existing = state.take().ok_or(MutationError::PreconditionFailed)?;
            *state = Some((new_binding, existing.1));
            Ok(MutationOutcome::Advanced(self.advance()))
        }

        fn remove(&self, expected_revision: Revision) -> Result<MutationOutcome, MutationError> {
            let current = self.current();
            if expected_revision != current {
                return Err(MutationError::StaleRevision(current));
            }
            let mut state = self.state.lock().expect("state lock");
            if state.is_none() {
                return Ok(MutationOutcome::AlreadyAbsent(current));
            }
            *state = None;
            Ok(MutationOutcome::Advanced(self.advance()))
        }

        fn status(&self) -> CredentialStatus {
            if self.state.lock().expect("state lock").is_some() {
                CredentialStatus::Configured
            } else {
                CredentialStatus::Unconfigured
            }
        }
    }

    impl InMemoryOwner {
        fn current(&self) -> Revision {
            *self.revision.lock().expect("revision lock")
        }
        fn advance(&self) -> Revision {
            let mut r = self.revision.lock().expect("revision lock");
            *r = r.next();
            *r
        }
    }

    // The abstraction is object-safe and can be held behind a trait object at the boundary — the
    // property PB7 depends on when it stores a `dyn CredentialOwner` off the control task.
    #[test]
    fn the_abstraction_is_object_safe_and_usable_behind_a_trait_object() {
        let owner: Box<dyn CredentialOwner> = Box::new(InMemoryOwner::new());
        assert_eq!(owner.status(), CredentialStatus::Unconfigured);
        owner
            .connect(Revision::INITIAL, binding("https://x/v1"), "sk-1".into())
            .expect("connect");
        assert_eq!(owner.status(), CredentialStatus::Configured);
        assert_eq!(
            owner.read_bound(&binding("https://x/v1")).state.tag(),
            crate::domain::CredentialStateTag::Present
        );
    }
}
