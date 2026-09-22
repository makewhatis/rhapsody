//! Shared internal state for sessions, turns, and the capability registry.
//!
//! Nothing in this module is public. The public handles in [`crate::session`], [`crate::turn`], and
//! [`crate::broker`] are thin, move-only owners of these values, which lets revocation and receipt
//! finalization be synchronous and deterministic (design §4.3, "Ways to get this wrong").

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};

use crate::binding::BoundCredentialLease;
use crate::broker::BrokerRegistrationPlan;
use crate::clock::{Clock, MonotonicTime};
use crate::error::BrokerError;
use crate::ledger::{ReservationCounters, TurnLedger, TurnOutcome};
use crate::policy::SessionPolicy;
use crate::random::RandomSource;
use crate::reservations::{
    ConcurrencyPermit, ReservationSnapshot, Reservations, ReserveRequest, SessionReservations,
};

/// Recover a poisoned lock instead of propagating: no broker method panics while holding one.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Exactly 32 CSPRNG bytes per turn token (design §4.1).
pub(crate) const TOKEN_BYTES: usize = 32;
/// The encoded token is 43 unpadded base64url ASCII characters.
pub(crate) const TOKEN_CHARS: usize = 43;
/// How many digest collisions a mint may retry before failing closed.
pub(crate) const MAX_TOKEN_RETRIES: usize = 8;
/// Internal session ids are a separately domain-typed 128-bit random value.
pub(crate) const SESSION_ID_BYTES: usize = 16;

const TOKEN_DOMAIN: &[u8] = b"rhapsody-provider-turn-v1\0";

/// A domain-typed 128-bit internal session id. Its `Debug` redacts the value; it is never a metric
/// label (design §4.2, §13).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct SessionId([u8; SESSION_ID_BYTES]);

impl SessionId {
    pub(crate) fn from_bytes(bytes: [u8; SESSION_ID_BYTES]) -> Self {
        SessionId(bytes)
    }
}

impl std::fmt::Debug for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<session-id>")
    }
}

/// The SHA-256 registry key for a presented turn token. The raw bearer value is never a key.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct TokenDigest([u8; 32]);

impl TokenDigest {
    pub(crate) fn of(presented: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(TOKEN_DOMAIN);
        hasher.update(presented);
        let digest: [u8; 32] = hasher.finalize().into();
        TokenDigest(digest)
    }

    /// The digest bytes are visible only to this module's tests (the digest-only canary).
    #[cfg(test)]
    pub(crate) fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl std::fmt::Debug for TokenDigest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<turn-token-digest>")
    }
}

/// Whether a presented value has the exact length and base64url alphabet of a turn token. Lookup
/// rejects anything else before hashing, so there is no prefix comparison or partial oracle.
pub(crate) fn is_valid_token_shape(presented: &str) -> bool {
    presented.len() == TOKEN_CHARS && presented.bytes().all(is_base64url_char)
}

fn is_base64url_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')
}

/// Encode 32 CSPRNG bytes as the 43-character unpadded base64url bearer value.
pub(crate) fn encode_token(bytes: &[u8; TOKEN_BYTES]) -> Vec<u8> {
    URL_SAFE_NO_PAD.encode(bytes).into_bytes()
}

/// A capacity-one gate. [`SessionInner`] holds one so exactly one attempt/access is live at a time;
/// the guard releases the gate when the attempt or access drops, never when the receipt drops.
#[derive(Debug)]
pub(crate) struct CapacityOne {
    held: AtomicBool,
}

impl CapacityOne {
    pub(crate) fn new() -> Self {
        Self {
            held: AtomicBool::new(false),
        }
    }

    pub(crate) fn try_acquire(self: &Arc<Self>) -> Option<TurnGateGuard> {
        self.held
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| TurnGateGuard {
                gate: Arc::clone(self),
            })
    }
}

/// Releases the capacity-one gate on drop.
#[derive(Debug)]
pub(crate) struct TurnGateGuard {
    gate: Arc<CapacityOne>,
}

impl Drop for TurnGateGuard {
    fn drop(&mut self) {
        self.gate.held.store(false, Ordering::Release);
    }
}

/// The capacity-one receipt slot. State transitions are the structural guarantee that the next turn
/// cannot arm before the prior receipt is drained (design §3.2).
#[derive(Debug)]
pub(crate) struct ReceiptSlot {
    state: Mutex<SlotState>,
}

#[derive(Debug)]
enum SlotState {
    Empty,
    Armed,
    AccessLive,
    Finalized(TurnLedger),
}

impl ReceiptSlot {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(SlotState::Empty),
        }
    }

    /// `Empty -> Armed`. A non-empty slot means the prior receipt was not drained.
    pub(crate) fn begin_armed(&self) -> Result<(), BrokerError> {
        let mut state = lock(&self.state);
        match *state {
            SlotState::Empty => {
                *state = SlotState::Armed;
                Ok(())
            }
            _ => Err(BrokerError::TurnAlreadyArmed),
        }
    }

    /// Undo a failed [`ReceiptSlot::begin_armed`].
    pub(crate) fn abandon_armed(&self) {
        let mut state = lock(&self.state);
        if matches!(*state, SlotState::Armed) {
            *state = SlotState::Empty;
        }
    }

    /// `Armed -> AccessLive` when a capability is minted.
    pub(crate) fn set_access_live(&self) {
        let mut state = lock(&self.state);
        if matches!(*state, SlotState::Armed) {
            *state = SlotState::AccessLive;
        }
    }

    /// Publish the finalized ledger exactly once. Returns whether this call finalized it.
    pub(crate) fn finalize(&self, ledger: TurnLedger) -> bool {
        let mut state = lock(&self.state);
        if matches!(*state, SlotState::Finalized(_)) {
            return false;
        }
        *state = SlotState::Finalized(ledger);
        true
    }

    /// Drain the finalized ledger that belongs to `ordinal` (`Finalized -> Empty`); `None` if the
    /// slot is not finalized or holds a *different* turn's ledger.
    ///
    /// The slot is session-local and reused across turns, so a receipt retained past `take` must
    /// not steal or erase the next turn's ledger: it acts only on the ledger with its own ordinal.
    pub(crate) fn take(&self, ordinal: u64) -> Option<TurnLedger> {
        let mut state = lock(&self.state);
        if matches!(&*state, SlotState::Finalized(ledger) if ledger.turn_ordinal() == ordinal) {
            let SlotState::Finalized(ledger) = std::mem::replace(&mut *state, SlotState::Empty)
            else {
                return None;
            };
            return Some(ledger);
        }
        None
    }

    /// Drop the finalized ledger that belongs to `ordinal` without handing it back, so the next
    /// turn may arm. A ledger for any other turn is left intact.
    pub(crate) fn drain(&self, ordinal: u64) {
        let mut state = lock(&self.state);
        if matches!(&*state, SlotState::Finalized(ledger) if ledger.turn_ordinal() == ordinal) {
            *state = SlotState::Empty;
        }
    }
}

/// One registered session's shared state. Held by [`crate::BrokerSession`] and referenced by its
/// turn grants.
pub(crate) struct SessionInner {
    pub(crate) id: SessionId,
    pub(crate) broker: Arc<BrokerInner>,
    pub(crate) plan: BrokerRegistrationPlan,
    pub(crate) policy: SessionPolicy,
    pub(crate) credential: Mutex<Option<BoundCredentialLease>>,
    pub(crate) revoked: AtomicBool,
    pub(crate) turn_gate: Arc<CapacityOne>,
    pub(crate) next_ordinal: AtomicU64,
    pub(crate) session_reservations: SessionReservations,
    pub(crate) receipt_slot: Arc<ReceiptSlot>,
}

impl SessionInner {
    pub(crate) fn new(
        id: SessionId,
        broker: Arc<BrokerInner>,
        plan: BrokerRegistrationPlan,
        policy: SessionPolicy,
        credential: BoundCredentialLease,
    ) -> Self {
        let session_reservations =
            SessionReservations::new(policy.limits().max_reserved_tokens_session);
        Self {
            id,
            broker,
            plan,
            policy,
            credential: Mutex::new(Some(credential)),
            revoked: AtomicBool::new(false),
            turn_gate: Arc::new(CapacityOne::new()),
            next_ordinal: AtomicU64::new(0),
            session_reservations,
            receipt_slot: Arc::new(ReceiptSlot::new()),
        }
    }

    pub(crate) fn is_revoked(&self) -> bool {
        self.revoked.load(Ordering::Acquire)
    }

    /// Revoke the session, release custody of the credential, and drop every child grant. Idempotent;
    /// called by [`crate::BrokerSession::revoke`] and on drop.
    pub(crate) fn revoke(&self) {
        self.revoked.store(true, Ordering::Release);
        if let Some(credential) = lock(&self.credential).take() {
            drop(credential);
        }
        lock(&self.broker.registry).revoke_session(&self.id);
    }

    pub(crate) fn next_ordinal(&self) -> u64 {
        self.next_ordinal.fetch_add(1, Ordering::AcqRel) + 1
    }

    pub(crate) fn has_custody(&self) -> bool {
        lock(&self.credential).is_some()
    }
}

/// One live turn grant. The registry indexes it by [`TokenDigest`]; the capability handle holds it.
pub(crate) struct TurnInner {
    pub(crate) session: Arc<SessionInner>,
    pub(crate) slot: Arc<ReceiptSlot>,
    pub(crate) ordinal: u64,
    pub(crate) not_after: MonotonicTime,
    pub(crate) issued: AtomicBool,
    pub(crate) revoked: AtomicBool,
    pub(crate) finalized: Mutex<Option<TurnOutcome>>,
    pub(crate) reservations: Reservations,
    pub(crate) digest: Mutex<Option<TokenDigest>>,
}

impl TurnInner {
    pub(crate) fn new(
        session: Arc<SessionInner>,
        slot: Arc<ReceiptSlot>,
        ordinal: u64,
        not_after: MonotonicTime,
    ) -> Self {
        let reservations = Reservations::new(*session.policy.limits());
        Self {
            session,
            slot,
            ordinal,
            not_after,
            issued: AtomicBool::new(false),
            revoked: AtomicBool::new(false),
            finalized: Mutex::new(None),
            reservations,
            digest: Mutex::new(None),
        }
    }

    /// Record the digest this grant was published under, and flip `issued`.
    ///
    /// Deliberately private and taking the registry guard: the only call site is
    /// [`Registry::publish`], which holds the lock while it inserts the grant. Recording the digest
    /// anywhere other than inside that critical section would let a concurrent [`Registry::revoke_grant`]
    /// miss a just-inserted grant and strand it in the map, so the signature makes such a split a
    /// compile error rather than a race a test would have to chase.
    fn mark_issued(&self, _registry: &mut Registry, digest: TokenDigest) {
        *lock(&self.digest) = Some(digest);
        self.issued.store(true, Ordering::Release);
    }

    pub(crate) fn digest(&self) -> Option<TokenDigest> {
        *lock(&self.digest)
    }

    pub(crate) fn is_expired(&self, now: MonotonicTime) -> bool {
        // Inclusive: a capability is dead *at* its absolute expiry, never a nanosecond later
        // (design §2 "expires no later than the turn deadline"; §4.2 `not_after <= deadline`).
        now >= self.not_after
    }

    pub(crate) fn is_revoked(&self) -> bool {
        self.revoked.load(Ordering::Acquire)
    }

    pub(crate) fn is_finalized(&self) -> bool {
        lock(&self.finalized).is_some()
    }

    /// Whether this grant may still admit work: neither it nor its session is revoked, and the
    /// capability has not expired. Called by the reservation primitives *inside* the admission
    /// lock, so a revocation cannot publish a ledger between this check and the reservation it
    /// commits.
    pub(crate) fn check_live(&self) -> Result<(), BrokerError> {
        if self.is_revoked() || self.session.is_revoked() {
            return Err(BrokerError::Unauthorized);
        }
        if self.is_expired(self.session.broker.clock.now()) {
            return Err(BrokerError::Unauthorized);
        }
        Ok(())
    }

    /// Atomically reserve one forwarded request against the turn and session limits.
    pub(crate) fn reserve_request(&self, request: ReserveRequest) -> Result<(), BrokerError> {
        self.reservations
            .try_reserve(&self.session.session_reservations, request, || {
                self.check_live()
            })
    }

    /// Acquire one concurrency permit for this turn.
    pub(crate) fn acquire_concurrency(&self) -> Result<ConcurrencyPermit, BrokerError> {
        self.reservations
            .try_acquire_concurrency(|| self.check_live())
    }

    /// Count one locally denied request against this turn.
    pub(crate) fn record_denied(&self) -> Result<(), BrokerError> {
        self.reservations.record_denied(|| self.check_live())
    }

    /// Finalize exactly once, publishing the ledger into the capacity-one slot. `close_and_snapshot`
    /// sets the reservation gate closed and returns the committed counters in one critical section,
    /// so no admission can commit after the ledger is built.
    pub(crate) fn finalize(&self, outcome: TurnOutcome) -> Option<TurnLedger> {
        let mut finalized = lock(&self.finalized);
        if finalized.is_some() {
            return None;
        }
        *finalized = Some(outcome);
        let counters = self.reservations.close_and_snapshot();
        let ledger = self.build_ledger(outcome, counters);
        self.slot.finalize(ledger.clone());
        Some(ledger)
    }

    /// Revoke the grant and finalize with the outcome implied by the declared one and the clock.
    pub(crate) fn revoke_and_finalize(&self, declared: TurnOutcome) -> Option<TurnLedger> {
        self.revoke_grant();
        let now = self.session.broker.clock.now();
        let outcome = if self.is_expired(now) {
            TurnOutcome::Expired
        } else if declared == TurnOutcome::Completed && !self.session.is_revoked() {
            TurnOutcome::Completed
        } else {
            TurnOutcome::Revoked
        };
        self.finalize(outcome)
    }

    /// The supervisor dropped the retained receipt without draining it (a caller bug). Fail closed:
    /// revoke the grant so a capability can no longer be minted or spend, then publish a
    /// `SupervisorReleased` receipt with the counters as of now. A turn that already finalized keeps
    /// its outcome; `finalize` is also exactly-once, so a racing attempt/access drop cannot double up.
    pub(crate) fn supervisor_release(&self) -> Option<TurnLedger> {
        if self.is_finalized() {
            return None;
        }
        self.revoke_grant();
        self.finalize(TurnOutcome::SupervisorReleased)
    }

    /// Set the revoked flag and drop the grant from the registry if one was issued.
    ///
    /// The whole operation is delegated to [`Registry::revoke_grant`] under the registry lock, the
    /// same lock [`Registry::publish`] holds. Keeping the flag set and the digest lookup inside that
    /// one method means neither can be hoisted above the lock: a mint that has inserted its grant
    /// but holds the lock is still publishing, and a revocation that read the digest before taking
    /// the lock could miss it and strand the grant.
    fn revoke_grant(&self) {
        lock(&self.session.broker.registry).revoke_grant(self);
    }

    fn build_ledger(&self, outcome: TurnOutcome, snapshot: ReservationSnapshot) -> TurnLedger {
        let counters = ReservationCounters {
            forwarded_requests: snapshot.forwarded_requests,
            denied_requests: snapshot.denied_requests,
            request_bytes: snapshot.request_bytes,
            response_bytes: snapshot.response_bytes,
            reserved_tokens: snapshot.reserved_tokens,
        };
        TurnLedger::new(
            self.ordinal,
            outcome,
            self.issued.load(Ordering::Acquire),
            counters,
            None,
        )
    }
}

/// Test-only rendezvous that pauses a mint *after* it has inserted its grant and released the
/// registry lock, so a race test can interleave a concurrent receipt drop deterministically. The
/// pause is the last thing that happens before the mint returns its handle. Compiled out of
/// production builds; a no-op unless a test arms it.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct MintRaceGate {
    rendezvous: Mutex<Option<MintRaceRendezvous>>,
}

#[cfg(test)]
struct MintRaceRendezvous {
    reached: std::sync::mpsc::Sender<()>,
    resume: std::sync::mpsc::Receiver<()>,
}

#[cfg(test)]
impl MintRaceGate {
    pub(crate) fn arm(
        &self,
        reached: std::sync::mpsc::Sender<()>,
        resume: std::sync::mpsc::Receiver<()>,
    ) {
        *lock(&self.rendezvous) = Some(MintRaceRendezvous { reached, resume });
    }

    /// Called by a mint that has already published (and digest-recorded) its grant. Signals the test
    /// that the grant is live and blocks until the test lets the mint continue.
    pub(crate) fn pause_after_insert(&self) {
        let rendezvous = lock(&self.rendezvous).take();
        if let Some(rendezvous) = rendezvous {
            let _ = rendezvous.reached.send(());
            let _ = rendezvous.resume.recv();
        }
    }
}

/// The shared broker internals: listener-independent registry plus the injected clock/RNG.
pub(crate) struct BrokerInner {
    pub(crate) base_url: String,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) rng: Arc<dyn RandomSource>,
    pub(crate) registry: Mutex<Registry>,
    #[cfg(test)]
    pub(crate) mint_race: MintRaceGate,
}

/// The capability registry. Sessions are held weakly (the [`crate::BrokerSession`] handle and live
/// grants are the strong owners); grants are keyed only by digest.
#[derive(Default)]
pub(crate) struct Registry {
    pub(crate) sessions: HashMap<SessionId, Weak<SessionInner>>,
    pub(crate) grants: HashMap<TokenDigest, Arc<TurnInner>>,
}

impl Registry {
    /// Publish a freshly minted grant atomically: insert it under `digest` and record that digest on
    /// the grant in the same critical section. `revoke_grant` removes a grant *by the digest recorded
    /// here*, so the insert and the record must not be separable — hence this method owns both, and
    /// [`TurnInner::mark_issued`] cannot be reached without this guard.
    pub(crate) fn publish(&mut self, digest: TokenDigest, inner: &Arc<TurnInner>) {
        self.grants.insert(digest, Arc::clone(inner));
        inner.mark_issued(self, digest);
    }

    /// Mark a grant revoked and, if it was published, remove it — both under the one registry lock.
    pub(crate) fn revoke_grant(&mut self, inner: &TurnInner) {
        inner.revoked.store(true, Ordering::Release);
        if let Some(digest) = inner.digest() {
            self.grants.remove(&digest);
        }
    }

    pub(crate) fn revoke_session(&mut self, id: &SessionId) {
        self.sessions.remove(id);
        self.grants.retain(|_, turn| &turn.session.id != id);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::thread;

    use super::*;
    use crate::ledger::TurnOutcome;

    #[test]
    fn capacity_one_refuses_a_second_holder_while_one_is_held() {
        let gate = Arc::new(CapacityOne::new());
        // Hold the single slot before any contender runs.
        let held = gate.try_acquire().expect("the first holder");
        let threads = 8;
        let barrier = Arc::new(std::sync::Barrier::new(threads));
        let (tx, rx) = std::sync::mpsc::channel();
        let mut handles = Vec::new();
        for _ in 0..threads {
            let gate = Arc::clone(&gate);
            let barrier = Arc::clone(&barrier);
            let tx = tx.clone();
            handles.push(thread::spawn(move || {
                barrier.wait();
                let acquired = gate.try_acquire().is_some();
                tx.send(acquired).expect("send");
            }));
        }
        drop(tx);
        for acquired in rx {
            assert!(!acquired, "no second holder while the gate is held");
        }
        for handle in handles {
            handle.join().expect("thread");
        }
        drop(held);
        assert!(
            gate.try_acquire().is_some(),
            "releasing the holder admits the next turn"
        );
    }

    #[test]
    fn receipt_slot_is_capacity_one() {
        let slot = ReceiptSlot::new();
        assert_eq!(slot.begin_armed(), Ok(()));
        assert_eq!(slot.begin_armed(), Err(BrokerError::TurnAlreadyArmed));
        let ledger = TurnLedger::new(
            1,
            TurnOutcome::NoCapability,
            false,
            ReservationCounters::default(),
            None,
        );
        assert!(slot.finalize(ledger.clone()));
        assert!(!slot.finalize(ledger.clone()));
        assert_eq!(slot.take(1), Some(ledger));
        assert_eq!(slot.take(1), None);
        assert_eq!(slot.begin_armed(), Ok(()));
    }

    #[test]
    fn receipt_slot_only_acts_on_its_own_ordinal() {
        let slot = ReceiptSlot::new();
        assert_eq!(slot.begin_armed(), Ok(()));

        let first = TurnLedger::new(
            1,
            TurnOutcome::Completed,
            true,
            ReservationCounters::default(),
            None,
        );
        assert!(slot.finalize(first.clone()));
        // A stale receipt for turn 2 must neither hand back nor drain turn 1's ledger.
        assert_eq!(slot.take(2), None);
        slot.drain(2);
        assert_eq!(slot.take(1), Some(first));

        // Turn 2 arms on the now-empty slot and finalizes; a stale turn-1 receipt must leave it be.
        assert_eq!(slot.begin_armed(), Ok(()));
        let second = TurnLedger::new(
            2,
            TurnOutcome::Completed,
            true,
            ReservationCounters::default(),
            None,
        );
        assert!(slot.finalize(second.clone()));
        assert_eq!(slot.take(1), None, "turn 1 must not steal turn 2's ledger");
        slot.drain(1);
        assert_eq!(slot.take(2), Some(second), "turn 2's ledger survives");
    }

    #[test]
    fn receipt_slot_drains_only_its_own_ordinal_after_a_stale_drop() {
        let slot = ReceiptSlot::new();
        let _ = slot.begin_armed();
        let second = TurnLedger::new(
            2,
            TurnOutcome::Completed,
            true,
            ReservationCounters::default(),
            None,
        );
        assert!(slot.finalize(second.clone()));
        // A dropped turn-1 receipt drains only its own ledger, never turn 2's.
        slot.drain(1);
        assert_eq!(slot.take(2), Some(second));
    }

    #[test]
    fn token_digest_never_contains_the_raw_token() {
        let raw = b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let digest = TokenDigest::of(raw);
        assert!(
            !digest
                .as_bytes()
                .windows(raw.len())
                .any(|window| window == raw)
        );
        assert_eq!(format!("{digest:?}"), "<turn-token-digest>");
    }

    #[test]
    fn token_shape_is_exact() {
        assert!(is_valid_token_shape(
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        ));
        assert!(!is_valid_token_shape("short"));
        assert!(!is_valid_token_shape(
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
        ));
        assert!(!is_valid_token_shape(
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA+"
        ));
        assert!(!is_valid_token_shape(
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA/"
        ));
    }
}
