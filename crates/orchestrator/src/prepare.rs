//! prepare — the asynchronous prepared-dispatch and zero-turn refusal foundation (STUDIO-988, P6).
//!
//! # Why this exists
//!
//! Provider/credential resolution must never run on the orchestrator's single control task, and a
//! missing/denied/malformed credential is a persisted refusal outcome — not a failed agent attempt.
//! This module owns the generic, provider-agnostic half of that: the loop-owned `preparing`
//! reservation, the completion token/generation revalidation, cancellation, and the durable
//! zero-turn refusal. It deliberately knows nothing about Keychain, IPC, or the broker; PB7 injects
//! the real resolver and the resolved credential revision.
//!
//! # The state machine
//!
//! [`Orchestrator::begin_preparation`] synchronously inserts a [`PreparingEntry`] keyed by the
//! ticket/review identity and spawns the resolver off the control task. The resolver sends
//! [`Event::DispatchPrepared`] back; [`Orchestrator::handle_dispatch_prepared`] accepts the
//! completion only when the token, config generation, and current eligibility all still match, and
//! otherwise drops the move-only payload without touching loop state. An accepted success runs the
//! ordinary claim/workspace/run mutation; an accepted refusal writes exactly one zero-turn run row
//! and arms the refusal gate.
//!
//! # The injected seam
//!
//! With no resolver installed, every dispatch path is byte-identical to a daemon built before this
//! feature: `begin_preparation` is never reached and `dispatch_issue` runs inline as it always did.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rhapsody_core::Issue;

use crate::control_loop::{CancelSignal, Event};
use crate::orchestrator::Orchestrator;
use crate::retry::DispatchRoute;

/// The default per-preparation timeout. Well under the poll interval; a resolver that does not answer
/// within this releases the loop state exactly as a cancellation would, while its task still holds
/// its concurrency permit until it really exits.
pub const DEFAULT_PREPARATION_TIMEOUT: Duration = Duration::from_secs(30);

/// The daemon-wide bound on concurrent off-loop resolver tasks. The permit is held by the spawned
/// task for its whole lifetime — including after a loop-side timeout — so retries and backoff cannot
/// accumulate an unbounded resolver-task pileup even when the underlying work cannot be cancelled.
pub const MAX_PREPARATION_CONCURRENCY: usize = 4;

/// The first refusal-gate backoff: how long an identical refusal suppresses a re-probe.
pub const REFUSAL_BACKOFF_BASE_MS: i64 = 30_000;

/// The refusal-gate backoff ceiling. Bounded so a lock state or an external mutation is still
/// eventually re-probed without polling a credential store every tick.
pub const REFUSAL_BACKOFF_MAX_MS: i64 = 15 * 60 * 1000;

/// Identifies what a preparation is for. Ticket and review dispatch share the whole machinery; only
/// the identity and the resumable target differ.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PreparationKey {
    /// A ticket dispatch, keyed by the tracker's opaque issue id.
    Ticket { issue_id: String },
    /// A ticketless review dispatch, keyed by the review identity (`review:owner/repo#n@reviewer`).
    Review { review_id: String },
}

impl PreparationKey {
    /// The map/claim key: the opaque issue id for a ticket, the review identity for a review.
    pub fn id(&self) -> &str {
        match self {
            PreparationKey::Ticket { issue_id } => issue_id,
            PreparationKey::Review { review_id } => review_id,
        }
    }

    /// The stable fingerprint prefix that distinguishes a ticket from a review with a colliding id.
    fn kind(&self) -> &'static str {
        match self {
            PreparationKey::Ticket { .. } => "ticket",
            PreparationKey::Review { .. } => "review",
        }
    }
}

/// The typed preparation refusal reasons. Each maps to a closed, bounded reason code for logs,
/// metrics, and the refusal gate; none is a fallback to a weaker mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefusalReason {
    /// No credential is stored for the resolved binding.
    CredentialAbsent,
    /// The credential owner is locked or the read was denied.
    CredentialDeniedOrLocked,
    /// The stored envelope is malformed.
    CredentialMalformed,
    /// The stored credential is bound to a different endpoint/adapter than the current config.
    BindingMismatch,
    /// The credential owner is not reachable in this daemon instance.
    OwnerUnavailable,
    /// The credential owner refused this daemon instance.
    OwnerUnauthorized,
    /// The resolver exceeded the preparation timeout.
    ResolverTimedOut,
    /// The resolver failed for a reason it could not classify (already an actionable string).
    ResolverFailed(String),
}

impl RefusalReason {
    /// The closed reason code carried by the refusal gate, logs, and metrics.
    pub fn code(&self) -> &'static str {
        match self {
            RefusalReason::CredentialAbsent => "credential_absent",
            RefusalReason::CredentialDeniedOrLocked => "credential_denied_or_locked",
            RefusalReason::CredentialMalformed => "credential_malformed",
            RefusalReason::BindingMismatch => "binding_mismatch",
            RefusalReason::OwnerUnavailable => "owner_unavailable",
            RefusalReason::OwnerUnauthorized => "owner_unauthorized",
            RefusalReason::ResolverTimedOut => "resolver_timed_out",
            RefusalReason::ResolverFailed(_) => "resolver_failed",
        }
    }

    /// The operator-facing, actionable message stored on the zero-turn run row.
    pub fn message(&self) -> String {
        match self {
            RefusalReason::CredentialAbsent => "no provider credential is configured".to_string(),
            RefusalReason::CredentialDeniedOrLocked => {
                "the provider credential is locked or its read was denied".to_string()
            }
            RefusalReason::CredentialMalformed => {
                "the stored provider credential is malformed".to_string()
            }
            RefusalReason::BindingMismatch => {
                "the stored provider credential is bound to a different endpoint".to_string()
            }
            RefusalReason::OwnerUnavailable => {
                "the provider credential owner is unavailable".to_string()
            }
            RefusalReason::OwnerUnauthorized => {
                "the provider credential owner refused this daemon instance".to_string()
            }
            RefusalReason::ResolverTimedOut => {
                "provider preparation did not answer before its timeout".to_string()
            }
            RefusalReason::ResolverFailed(why) => why.clone(),
        }
    }
}

/// The resolved, move-only payload of a successful preparation. Deliberately **not** `Clone`: PB7
/// extends it with the broker session and credential lease, and cloning one must remain impossible.
#[derive(Debug)]
pub struct PreparedDispatch {
    pub harness: String,
    pub model: String,
    pub provider: String,
    /// The opaque credential revision this preparation read; empty in P6's injected-resolver tests.
    pub credential_revision: String,
}

impl PreparedDispatch {
    pub fn new(
        harness: impl Into<String>,
        model: impl Into<String>,
        provider: impl Into<String>,
        credential_revision: impl Into<String>,
    ) -> PreparedDispatch {
        PreparedDispatch {
            harness: harness.into(),
            model: model.into(),
            provider: provider.into(),
            credential_revision: credential_revision.into(),
        }
    }
}

/// A resolver's verdict: an accepted success carries the move-only prepared payload, an accepted
/// refusal carries the typed reason.
#[derive(Debug)]
pub enum PreparationOutcome {
    Ready(PreparedDispatch),
    Refused(RefusalReason),
}

/// One resolver completion. The `observed_revision` is the credential revision the resolver read
/// (empty when it never reached a credential); the loop folds it into the refusal-gate fingerprint.
#[derive(Debug)]
pub struct PreparationCompletion {
    pub outcome: PreparationOutcome,
    pub observed_revision: String,
}

impl PreparationCompletion {
    /// The typed timeout completion the loop substitutes when the resolver overruns its bound.
    pub fn timed_out() -> PreparationCompletion {
        PreparationCompletion {
            outcome: PreparationOutcome::Refused(RefusalReason::ResolverTimedOut),
            observed_revision: String::new(),
        }
    }

    /// The typed completion an unreachable resolver failure produces.
    pub fn failed(why: impl Into<String>) -> PreparationCompletion {
        PreparationCompletion {
            outcome: PreparationOutcome::Refused(RefusalReason::ResolverFailed(why.into())),
            observed_revision: String::new(),
        }
    }
}

/// The injected resolver seam. Production has none until PB7 installs the real credential/broker
/// resolver; tests inject a fake. Object-safe async, the same idiom the `Tracker` / `CredentialProbe`
/// traits use.
#[async_trait]
pub trait PreparationResolver: Send + Sync {
    /// Resolves the complete selection/bound-credential tuple for `req`. MUST NOT be called on the
    /// control task and MUST NOT block indefinitely — the caller bounds it with the preparation
    /// timeout.
    async fn prepare(&self, req: &PreparationRequest) -> PreparationCompletion;
}

/// The inputs a resolver needs. Deliberately minimal and secret-free: the resolved tuple and the
/// expected binding are derived by the resolver itself in PB7.
#[derive(Debug, Clone)]
pub struct PreparationRequest {
    /// What is being prepared (a ticket or a review).
    pub key: PreparationKey,
    /// The loop's stable selection fingerprint for this preparation.
    pub selection: String,
    /// The config generation the loop began this preparation under.
    pub config_generation: u64,
    /// The credential revision the loop expects, empty in P6 (PB7 supplies it).
    pub expected_revision: String,
}

/// What a completion resumes: the dispatch context captured when the reservation was inserted.
#[derive(Debug, Clone)]
pub(crate) enum PreparedTarget {
    /// A ticket dispatch, resuming the ordinary `dispatch_issue` path.
    Ticket {
        issue: Issue,
        attempt: Option<i64>,
        route: Option<DispatchRoute>,
        stack_context: String,
        /// Whether this is a POOL-MODE pick. A pool pick is prepared BEFORE its cross-daemon claim
        /// election, so the election is deferred to `finish_prepared`. Detection cannot be
        /// `pool_proj.is_some()`: the legacy single-project pool ladder carries `proj == None` too,
        /// so the two are kept apart.
        pool: bool,
        /// The owning project INDEX for a pool pick (`None` on the legacy single-project pool
        /// ladder), used at completion to resolve the slug-bound tracker for the election.
        pool_proj: Option<usize>,
    },
    /// A review dispatch, resuming the ticketless review's watch-set writes then dispatch.
    Review {
        issue: Issue,
        run: Box<crate::review::ReviewRun>,
        route: DispatchRoute,
    },
}

impl PreparedTarget {
    /// The stable, secret-free selection input folded into the refusal fingerprint. Deliberately
    /// WITHOUT the retry `attempt`: the credential requirement of a fresh dispatch and a retry of
    /// the same ticket are identical, so a refusal on a retry must suppress the fresh re-probe that
    /// follows the claim's release rather than reading as a different selection.
    fn selection(&self) -> String {
        match self {
            PreparedTarget::Ticket { issue, route, .. } => {
                let labels = issue.labels.as_ref().map_or_else(String::new, |ls| {
                    let mut v: Vec<&str> = ls.iter().map(String::as_str).collect();
                    v.sort_unstable();
                    v.join(",")
                });
                let slug = route.as_ref().map_or("", |r| r.slug.as_str());
                format!("ticket|{}|{}|{}|{}", issue.id, issue.state, labels, slug)
            }
            PreparedTarget::Review { run, route, .. } => format!(
                "review|{}|{}|{}|{}",
                run.key(),
                run.head_sha,
                run.reviewer,
                route.slug
            ),
        }
    }
}

/// An unforgeable preparation completion token. Its fields are private and only
/// [`PreparingReservations::begin`] mints one, pairing a per-reservation sequence with the config
/// generation it began under, so a completion minted for one reservation can never be accepted for
/// another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreparationToken {
    generation: u64,
    seq: u64,
}

impl PreparationToken {
    /// The config generation this reservation was created under.
    pub fn generation(&self) -> u64 {
        self.generation
    }
}

/// A loop-owned preparation reservation. It carries the same identity/slot a running entry would,
/// which is what makes `preparing` participate in the duplicate and concurrency gates.
pub(crate) struct PreparingEntry {
    pub token: PreparationToken,
    pub key: PreparationKey,
    /// The stable selection fingerprint (identity + selection) WITHOUT the credential revision; the
    /// gate appends the revision observed at completion.
    pub fingerprint: String,
    pub config_generation: u64,
    /// The cancellation trigger for this reservation (fired by Stop/reload/shutdown/drain).
    pub cancel: CancelSignal,
    /// The dispatch context a successful completion resumes.
    pub target: PreparedTarget,
    /// When true, the identity's claim is held by THIS same piece of work (a retry/continuation),
    /// so the claim is not evidence of a competing dispatch.
    pub claim_already_held: bool,
    /// When this reservation began, for bounded-age logging.
    pub started_at: DateTime<Utc>,
}

impl PreparingEntry {
    /// The id of the issue/review this reservation is for.
    pub fn id(&self) -> &str {
        self.key.id()
    }

    /// The ticket's state when the reservation was made (empty for a review); used to fold
    /// `preparing` into the per-state slot counts without re-deriving it.
    pub fn issue_state(&self) -> &str {
        match &self.target {
            PreparedTarget::Ticket { issue, .. } => issue.state.as_str(),
            PreparedTarget::Review { .. } => "",
        }
    }

    /// The owning project slug for this reservation (empty for the legacy path).
    pub fn project_slug(&self) -> &str {
        match &self.target {
            PreparedTarget::Ticket { route, .. } => route.as_ref().map_or("", |r| r.slug.as_str()),
            PreparedTarget::Review { route, .. } => route.slug.as_str(),
        }
    }

    /// The owning project GROUP for this reservation — the key the per-project concurrency cap
    /// counts by, with the same slug fallback a [`RunningEntry`](crate::orchestrator::RunningEntry)
    /// uses when a project carries no group.
    pub fn project_group(&self) -> &str {
        match &self.target {
            PreparedTarget::Ticket { route, .. } => route.as_ref().map_or("", |r| {
                if r.group.is_empty() {
                    r.slug.as_str()
                } else {
                    r.group.as_str()
                }
            }),
            PreparedTarget::Review { route, .. } => {
                if route.group.is_empty() {
                    route.slug.as_str()
                } else {
                    route.group.as_str()
                }
            }
        }
    }

    /// Whether this reservation is for a ticketless REVIEW (which draws the separate review pool)
    /// rather than a ticket implementation.
    pub fn is_review(&self) -> bool {
        matches!(self.target, PreparedTarget::Review { .. })
    }
}

impl std::fmt::Debug for PreparingEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparingEntry")
            .field("token", &self.token)
            .field("key", &self.key)
            .field("fingerprint", &self.fingerprint)
            .field("config_generation", &self.config_generation)
            .field("started_at", &self.started_at)
            .finish()
    }
}

/// The outcome of asking to begin a preparation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BeginPreparation {
    /// The reservation was inserted and the resolver task was spawned.
    Started(PreparationToken),
    /// A reservation already exists for this identity; no second resolver task was spawned.
    AlreadyPreparing,
    /// The issue/review is already running or claimed; no reservation was made.
    AlreadyInFlight,
    /// The refusal gate suppresses this fingerprint until its next probe time; no work was spawned.
    Suppressed,
    /// No resolver is installed, so preparation is not part of this dispatch and the caller should
    /// dispatch inline exactly as before the feature existed.
    NoResolver,
}

/// The loop-owned `preparing` reservation set. Control-task-confined, like every scheduling map on
/// [`Orchestrator`].
#[derive(Default)]
pub(crate) struct PreparingReservations {
    entries: HashMap<String, PreparingEntry>,
    seq: u64,
}

impl PreparingReservations {
    /// Whether a reservation exists for `id`.
    pub fn contains(&self, id: &str) -> bool {
        self.entries.contains_key(id)
    }

    /// The reservation for `id`, if any.
    pub fn get(&self, id: &str) -> Option<&PreparingEntry> {
        self.entries.get(id)
    }

    /// Every reserved id, for the duplicate/concurrency gates.
    pub fn ids(&self) -> impl Iterator<Item = &String> {
        self.entries.keys()
    }

    /// The reservations, for slot accounting.
    pub fn values(&self) -> impl Iterator<Item = &PreparingEntry> {
        self.entries.values()
    }

    /// How many reservations are held. The concurrency accounting reads it, as do tests.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no reservation is held.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Inserts a reservation for `id`, minting the unforgeable token. `AlreadyPreparing` when one
    /// already exists — the duplicate-dispatch guard.
    fn begin(
        &mut self,
        id: &str,
        key: PreparationKey,
        config_generation: u64,
        target: PreparedTarget,
        claim_already_held: bool,
        started_at: DateTime<Utc>,
    ) -> Result<PreparationToken, BeginPreparation> {
        if self.entries.contains_key(id) {
            return Err(BeginPreparation::AlreadyPreparing);
        }
        self.seq = self.seq.wrapping_add(1);
        let token = PreparationToken {
            generation: config_generation,
            seq: self.seq,
        };
        let fingerprint = RefusalGate::key(key.kind(), id, &target.selection());
        self.entries.insert(
            id.to_string(),
            PreparingEntry {
                token,
                key,
                fingerprint,
                config_generation,
                cancel: CancelSignal::new(),
                target,
                claim_already_held,
                started_at,
            },
        );
        Ok(token)
    }

    /// Removes and returns the reservation for `id` (a completion or an explicit cancel), if any.
    /// Firing its cancel signal is the caller's job — the reservation may be handed back so the
    /// completion path can drop the payload it carried.
    pub fn take(&mut self, id: &str) -> Option<PreparingEntry> {
        self.entries.remove(id)
    }

    /// Cancels one reservation explicitly (Stop / issue disappearance), firing its cancel signal and
    /// returning the released entry so the caller can give back any claim it was holding.
    pub fn cancel(&mut self, id: &str) -> Option<PreparingEntry> {
        let entry = self.entries.remove(id)?;
        entry.cancel.cancel();
        Some(entry)
    }

    /// Cancels every reservation (reload / shutdown), firing each cancel signal. Returns the
    /// cancelled entries so the caller can drop any payloads they were carrying AND give back any
    /// claims they held (the reload path) — an earlier bug cleared the vector before returning it,
    /// so the `count = cancelled.len()` logs and every claim-return were silently dead.
    pub fn cancel_all(&mut self) -> Vec<PreparingEntry> {
        let out: Vec<PreparingEntry> = self.entries.drain().map(|(_, e)| e).collect();
        for e in &out {
            e.cancel.cancel();
        }
        out
    }
}

/// The bounded refusal gate: it suppresses the SAME `(identity, selection)` refusal until an input
/// changes or its scheduled next-probe time arrives. Repeated identical refusals advance bounded
/// backoff without appending a second history row.
///
/// **The credential revision is stored ON the entry, never in its key.** The design names the gate
/// key as `(identity, selection fingerprint, credential revision)`, but a revision the resolver has
/// yet to observe cannot be part of the key `begin_preparation` checks *before* spawning that
/// resolver — a key that included it matched only when the revision was empty, so the moment PB7
/// supplied a real revision every refusal was re-probed on every tick. Keying on identity+selection
/// and remembering the observed revision keeps both properties: the pre-spawn check suppresses, and
/// a revision CHANGE is a new episode (fresh base backoff and a new history row) because the gate
/// compares the stored revision. A workflow reload, an explicit refresh and a provider credential
/// mutation all re-arm the gate directly.
#[derive(Default)]
pub struct RefusalGate {
    entries: HashMap<String, RefusalGateEntry>,
}

#[derive(Debug, Clone)]
struct RefusalGateEntry {
    reason_code: String,
    /// The opaque credential revision observed at the refusal that armed this entry (empty in P6).
    revision: String,
    next_probe_at: DateTime<Utc>,
    backoff_ms: i64,
}

impl RefusalGate {
    /// The gate key: identity + selection, deliberately WITHOUT the credential revision (see the
    /// type docs). Secret-free by construction.
    pub fn key(kind: &str, id: &str, selection: &str) -> String {
        format!("{kind}|{id}|{selection}")
    }

    /// Whether an identical refusal is still suppressing re-work at `now`.
    pub fn suppressed(&self, key: &str, now: DateTime<Utc>) -> bool {
        self.entries.get(key).is_some_and(|e| now < e.next_probe_at)
    }

    /// Records a refusal (or its repeat) and returns the new bounded backoff. The first refusal — or
    /// one at a CHANGED credential revision — arms the base backoff; each repeat of the identical
    /// refusal doubles it up to [`REFUSAL_BACKOFF_MAX_MS`].
    pub fn record(
        &mut self,
        key: &str,
        reason_code: &str,
        revision: &str,
        now: DateTime<Utc>,
    ) -> i64 {
        let backoff = match self.entries.get(key) {
            Some(prev) if prev.revision == revision => {
                (prev.backoff_ms.saturating_mul(2)).min(REFUSAL_BACKOFF_MAX_MS)
            }
            _ => REFUSAL_BACKOFF_BASE_MS,
        };
        self.entries.insert(
            key.to_string(),
            RefusalGateEntry {
                reason_code: reason_code.to_string(),
                revision: revision.to_string(),
                next_probe_at: now + chrono::Duration::milliseconds(backoff),
                backoff_ms: backoff,
            },
        );
        backoff
    }

    /// Re-arms one key (a changed credential revision / explicit refresh).
    pub fn rearm(&mut self, key: &str) {
        self.entries.remove(key);
    }

    /// Re-arms every gate (workflow reload / explicit refresh): a reload can change what a refusal
    /// meant, so every suppressed key is released exactly once.
    pub fn rearm_all(&mut self) {
        self.entries.clear();
    }

    /// How many keys are currently gated (0 when nothing is refusing).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the gate holds nothing.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The closed reason code recorded for a key, if any (for the API surface).
    pub fn reason_code(&self, key: &str) -> Option<&str> {
        self.entries.get(key).map(|e| e.reason_code.as_str())
    }

    /// The credential revision the refusal at `key` last observed, if any. The completion path
    /// compares it against the revision a new completion carries: the same revision is a REPEAT (no
    /// second history row); a different one (or no entry at all) is a fresh episode.
    pub fn observed_revision(&self, key: &str) -> Option<&str> {
        self.entries.get(key).map(|e| e.revision.as_str())
    }
}

impl Orchestrator {
    /// Installs the preparation resolver. Without one, every dispatch path is byte-identical to a
    /// daemon built before this feature (the default for tests and any build PB7 has not wired yet).
    pub fn set_preparation_resolver(&mut self, resolver: Arc<dyn PreparationResolver>) {
        self.prepare_resolver = Some(resolver);
    }

    /// Whether asynchronous preparation is active for this daemon.
    pub fn preparation_enabled(&self) -> bool {
        self.prepare_resolver.is_some()
    }

    /// The current preparation config generation, bumped on every reload so a completion minted
    /// under an older config can never mutate state.
    pub fn prepare_generation(&self) -> u64 {
        self.prepare_generation
    }

    /// Begins an asynchronous preparation for `target`, or reports why it should not dispatch. When
    /// no resolver is installed this returns [`BeginPreparation::NoResolver`] and the caller
    /// dispatches inline, which is what keeps the feature inert by default.
    ///
    /// `claim_already_held` is true when this is a retry/continuation of work that legitimately holds
    /// its claim across the backoff window; the claim then is not evidence of a competing dispatch.
    ///
    /// The reservation is inserted synchronously (the duplicate gate) BEFORE the resolver task is
    /// spawned, so two concurrent tick/retry/review paths cannot both dispatch the same identity.
    pub(crate) fn begin_preparation(
        &mut self,
        target: PreparedTarget,
        claim_already_held: bool,
    ) -> BeginPreparation {
        let Some(resolver) = self.prepare_resolver.clone() else {
            return BeginPreparation::NoResolver;
        };
        let key = match &target {
            PreparedTarget::Ticket { issue, .. } => PreparationKey::Ticket {
                issue_id: issue.id.clone(),
            },
            PreparedTarget::Review { run, .. } => PreparationKey::Review {
                review_id: run.key(),
            },
        };
        let id = key.id().to_string();
        // The duplicate-dispatch guard: a live run, a competing claim, or an existing reservation.
        if self.running.contains_key(&id) {
            return BeginPreparation::AlreadyInFlight;
        }
        if !claim_already_held && self.claimed.contains(&id) {
            return BeginPreparation::AlreadyInFlight;
        }
        if self.preparing.contains(&id) {
            return BeginPreparation::AlreadyPreparing;
        }
        let selection = target.selection();
        let fingerprint = RefusalGate::key(key.kind(), &id, &selection);
        let now = (self.now)();
        if self.refusal_gate.suppressed(&fingerprint, now) {
            return BeginPreparation::Suppressed;
        }
        let token = match self.preparing.begin(
            &id,
            key.clone(),
            self.prepare_generation,
            target,
            claim_already_held,
            now,
        ) {
            Ok(t) => t,
            Err(already) => return already,
        };
        // The resolver task's inputs are all owned/cloned before the spawn, so no borrow of `self`
        // crosses the spawn. The permit is acquired INSIDE the task and retained for its lifetime.
        let events = self.events.clone();
        let semaphore = Arc::clone(&self.prepare_semaphore);
        let timeout = self.prepare_timeout;
        let cancel = self
            .preparing
            .get(&id)
            .map(|e| e.cancel.clone())
            .unwrap_or_default();
        let wg = self.wg.add();
        let req = PreparationRequest {
            key,
            selection,
            config_generation: token.generation,
            expected_revision: String::new(),
        };
        tokio::spawn(async move {
            let _guard = wg;
            // The permit is moved into the task and held until the task really exits — including
            // after the loop-side timeout — so retries cannot pile up resolver work.
            let _permit = semaphore.acquire_owned().await;
            let mut cancel_wait = cancel.wait();
            let completion = tokio::select! {
                _ = cancel_wait.cancelled() => None,
                r = tokio::time::timeout(timeout, resolver.prepare(&req)) => match r {
                    Ok(c) => Some(c),
                    Err(_) => Some(PreparationCompletion::timed_out()),
                },
            };
            // A closed receiver (the loop is gone) or a stale token drops the payload here without
            // touching loop state.
            if let Some(completion) = completion {
                let _ = events.send(Event::DispatchPrepared {
                    id,
                    token,
                    completion,
                });
            }
        });
        BeginPreparation::Started(token)
    }

    /// Handles a resolver completion on the control task. Accepts only the CURRENT token and config
    /// generation, revalidates drain/eligibility, then either dispatches or writes the zero-turn
    /// refusal. Any stale completion drops its move-only payload and changes no state.
    pub(crate) async fn handle_dispatch_prepared(
        &mut self,
        id: String,
        token: PreparationToken,
        completion: PreparationCompletion,
    ) {
        // Only the current reservation's completion is accepted. A completion for a reservation that
        // was cancelled (Stop/reload/shutdown) or superseded has no stored entry: drop the payload.
        let Some(entry) = self.preparing.get(&id) else {
            return;
        };
        if entry.token != token {
            return; // a newer reservation holds this id; drop the stale payload
        }
        // A completion minted under an older config generation is stale by definition.
        if token.generation != self.prepare_generation {
            if let Some(entry) = self.preparing.take(&id) {
                self.abandon_prepared(entry, AbandonCause::Superseded);
            }
            return;
        }
        // Revalidate current eligibility: an armed drain defers without refusing (the work may be
        // re-offered after the drain), and a live run/claim means this completion is moot.
        if self.drain.is_draining() {
            if let Some(entry) = self.preparing.take(&id) {
                self.abandon_prepared(entry, AbandonCause::Drained);
            }
            return;
        }
        let target = {
            let Some(entry) = self.preparing.get(&id) else {
                return;
            };
            let competing_claim = !entry.claim_already_held && self.claimed.contains(&id);
            if self.running.contains_key(&id) || competing_claim {
                let _ = self.preparing.take(&id);
                return;
            }
            entry.target.clone()
        };
        // The reservation is consumed exactly once, here.
        let Some(entry) = self.preparing.take(&id) else {
            return;
        };
        match completion.outcome {
            PreparationOutcome::Ready(prepared) => {
                // Ready to run the ordinary side effects. A successful preparation clears any stale
                // gate for this fingerprint.
                self.refusal_gate.rearm(&entry.fingerprint);
                self.finish_prepared(id, target, prepared).await;
            }
            PreparationOutcome::Refused(reason) => {
                let now = (self.now)();
                // The key holds identity+selection; the revision is the entry's own data. A changed
                // revision (or no prior entry) is a fresh episode and gets its own history row; a
                // repeat of the identical refusal only advances the backoff.
                let new_episode = self.refusal_gate.observed_revision(&entry.fingerprint)
                    != Some(completion.observed_revision.as_str());
                self.refusal_gate.record(
                    &entry.fingerprint,
                    reason.code(),
                    &completion.observed_revision,
                    now,
                );
                // A refusal is not agent work: give back the claim a retry/continuation held, so
                // the ticket is not stranded. The gate — not a retry timer — drives its re-probe, and
                // the released ticket is re-selected fresh only once the gate's probe time arrives.
                self.release_retry_claim(&entry);
                if new_episode {
                    self.persist_refusal(&entry, &reason);
                } else {
                    tracing::debug!(
                        id = %id,
                        code = reason.code(),
                        "repeat refusal; backoff advanced without a second history row"
                    );
                }
            }
        }
    }

    /// The shared ticket dispatch entry point for the loop and retry paths. With no resolver
    /// installed it dispatches inline, byte-identical to the pre-feature behavior; with one it begins
    /// an asynchronous preparation and the dispatch resumes when the completion is accepted.
    pub(crate) fn dispatch_or_prepare(
        &mut self,
        issue: Issue,
        attempt: Option<i64>,
        route: Option<DispatchRoute>,
        stack_context: String,
    ) {
        let claim_already_held = attempt.is_some();
        let target = PreparedTarget::Ticket {
            issue: issue.clone(),
            attempt,
            route: route.clone(),
            stack_context: stack_context.clone(),
            pool: false,
            pool_proj: None,
        };
        match self.begin_preparation(target, claim_already_held) {
            BeginPreparation::NoResolver => {
                self.dispatch_issue(issue, attempt, route, stack_context)
            }
            BeginPreparation::Started(_) => {}
            BeginPreparation::AlreadyPreparing => {
                tracing::debug!(
                    issue = %issue.identifier,
                    "preparation already in flight for this issue; not dispatching again"
                );
            }
            BeginPreparation::AlreadyInFlight => {
                tracing::debug!(
                    issue = %issue.identifier,
                    "preparation skipped: a run or competing claim is already in flight"
                );
            }
            BeginPreparation::Suppressed => {
                tracing::debug!(
                    issue = %issue.identifier,
                    "preparation suppressed by the refusal gate until its next probe"
                );
            }
        }
    }

    /// Begins preparation for ONE pool-mode pick. Preparation runs BEFORE the cross-daemon claim
    /// election (STUDIO-988): a refused or abandoned preparation must leave the ticket unassigned,
    /// so the election — which is what a refusal would otherwise strand — moves to
    /// [`finish_prepared`], after acceptance. With no resolver installed this is today's
    /// claim-then-dispatch, byte-identical.
    pub(crate) async fn dispatch_or_prepare_pool(&mut self, pick: crate::select::TaggedIssue) {
        let route = self.route_for(pick.proj);
        let target = PreparedTarget::Ticket {
            issue: pick.iss.clone(),
            attempt: None,
            route: route.clone(),
            stack_context: String::new(),
            pool: true,
            pool_proj: pick.proj,
        };
        match self.begin_preparation(target, false) {
            BeginPreparation::NoResolver => {
                for winner in self.claim_winners(vec![pick]).await {
                    let winner_route = self.route_for(winner.proj);
                    self.dispatch_issue(winner.iss, None, winner_route, String::new());
                }
            }
            BeginPreparation::Started(_) => {}
            BeginPreparation::AlreadyPreparing => {
                tracing::debug!(
                    issue = %pick.iss.identifier,
                    "pool preparation already in flight; not claiming or dispatching again"
                );
            }
            BeginPreparation::AlreadyInFlight => {
                tracing::debug!(
                    issue = %pick.iss.identifier,
                    "pool preparation skipped: a run or competing claim is already in flight"
                );
            }
            BeginPreparation::Suppressed => {
                tracing::debug!(
                    issue = %pick.iss.identifier,
                    "pool preparation suppressed by the refusal gate until its next probe"
                );
            }
        }
    }

    /// Runs the ordinary dispatch side effects for an accepted successful preparation. A pool pick
    /// runs its cross-daemon claim election HERE — after preparation, so a refusal never claims — and
    /// dispatches only if it won.
    async fn finish_prepared(
        &mut self,
        _id: String,
        target: PreparedTarget,
        prepared: PreparedDispatch,
    ) {
        match target {
            PreparedTarget::Ticket {
                issue,
                attempt,
                route,
                stack_context,
                pool,
                pool_proj,
            } => {
                if pool {
                    tracing::info!(
                        issue = %issue.identifier,
                        harness = %prepared.harness,
                        "preparation accepted; running the pool claim election"
                    );
                    let pick = crate::select::TaggedIssue {
                        iss: issue,
                        proj: pool_proj,
                    };
                    for winner in self.claim_winners(vec![pick]).await {
                        let winner_route = self.route_for(winner.proj);
                        self.dispatch_issue(winner.iss, None, winner_route, String::new());
                    }
                    return;
                }
                tracing::info!(
                    issue = %issue.identifier,
                    harness = %prepared.harness,
                    model = %prepared.model,
                    provider = %prepared.provider,
                    "preparation accepted; dispatching"
                );
                self.dispatch_issue(issue, attempt, route, stack_context);
            }
            PreparedTarget::Review { issue, run, route } => {
                tracing::info!(
                    review = %run.key(),
                    harness = %prepared.harness,
                    "review preparation accepted; dispatching"
                );
                self.finish_review_dispatch(*run, route, issue);
            }
        }
    }

    /// Gives back the claim a retry/continuation held when its preparation ended in a REFUSAL. The
    /// design forbids a claim on a refusal and, more practically, the released ticket is re-selected
    /// fresh and suppressed by the refusal gate until its probe time — without this, `on_retry` has
    /// already removed the retry entry, leaving `claimed` set with nothing to fire it again.
    fn release_retry_claim(&mut self, entry: &PreparingEntry) {
        let PreparedTarget::Ticket { issue, .. } = &entry.target else {
            return;
        };
        if !entry.claim_already_held {
            return;
        }
        self.claimed.remove(&issue.id);
        self.completed.remove(&issue.id);
        self.persist_release(&issue.identifier);
        tracing::info!(
            issue = %issue.identifier,
            "refused preparation released the retry claim so the ticket can re-probe"
        );
    }

    /// Releases or re-parks the claim of a claim-held preparation that ended WITHOUT dispatching for
    /// a reason that is not a refusal. `Drained` parks the retry (keeping its claim and attempt) exactly
    /// as [`on_retry`](Orchestrator::on_retry)'s own drain gate does; `Superseded` (reload, stale
    /// generation, issue disappearance) gives the claim back so the ticket re-selects under the new
    /// state. A fresh dispatch holds no claim and this is a no-op.
    fn abandon_prepared(&mut self, entry: PreparingEntry, cause: AbandonCause) {
        let PreparedTarget::Ticket {
            issue,
            attempt,
            route,
            ..
        } = &entry.target
        else {
            return;
        };
        if !entry.claim_already_held {
            return;
        }
        match cause {
            AbandonCause::Drained => {
                let attempt = attempt.unwrap_or(0);
                let (slug, repo) = route
                    .as_ref()
                    .map_or(("", ""), |r| (r.slug.as_str(), r.repo.as_str()));
                self.schedule_retry_for(
                    crate::retry::RetryTarget {
                        id: &issue.id,
                        identifier: &issue.identifier,
                        project_slug: slug,
                        project_repo: repo,
                    },
                    attempt,
                    crate::drain::DRAIN_REQUEUE_DELAY_MS,
                    "drain: preparation deferred",
                    issue.clone(),
                    String::new(),
                );
                tracing::debug!(
                    issue = %issue.identifier,
                    "drain: re-parked a preparation-terminated retry; claim and attempt kept"
                );
            }
            AbandonCause::Superseded => {
                self.claimed.remove(&issue.id);
                self.completed.remove(&issue.id);
                self.persist_release(&issue.identifier);
                tracing::info!(
                    issue = %issue.identifier,
                    "superseded preparation released the retry claim so the ticket re-selects"
                );
            }
        }
    }

    /// Writes exactly one zero-turn refusal run row: outcome `refused`, zero turns/tokens, reason and
    /// resolved provenance, with NO claim, mailbox, active-run entry, workspace, or review-watch row.
    fn persist_refusal(&mut self, entry: &PreparingEntry, reason: &RefusalReason) {
        let reason_msg = reason.message();
        match &entry.target {
            PreparedTarget::Ticket { issue, .. } => {
                tracing::warn!(
                    issue = %issue.identifier,
                    reason = %reason_msg,
                    code = reason.code(),
                    "preparation refused; recording a zero-turn refusal (no claim, no workspace)"
                );
                self.write_refusal_run(issue, entry.project_slug(), &reason_msg);
            }
            PreparedTarget::Review { run, route, .. } => {
                tracing::warn!(
                    review = %run.key(),
                    reason = %reason_msg,
                    code = reason.code(),
                    "review preparation refused; recording a zero-turn refusal (watch row untouched)"
                );
                let issue = run.synthetic_issue();
                self.write_refusal_run(&issue, &route.slug, &reason_msg);
            }
        }
    }

    /// The durable zero-turn refusal record. Uses the store directly rather than
    /// `persist_start_run`/`persist_end_run` so an ordinary claim is never written — a stranded
    /// claim would greet boot recovery as live work.
    fn write_refusal_run(&self, issue: &Issue, project_slug: &str, reason: &str) {
        let now = (self.now)();
        let run_id = match self.store.start_run(rhapsody_store::RunStart {
            issue_id: issue.id.clone(),
            issue_identifier: issue.identifier.clone(),
            title: issue.title.clone(),
            attempt: 0,
            started_at: crate::persist::rfc3339(now),
            project_slug: project_slug.to_string(),
            repo: String::new(),
            team_id: issue.team_id.clone(),
            ..Default::default()
        }) {
            Ok(id) => id,
            Err(e) => {
                tracing::error!(
                    issue_identifier = %issue.identifier,
                    error = %e,
                    "persist refusal run failed"
                );
                return;
            }
        };
        if let Err(e) = self.store.end_run(
            run_id,
            rhapsody_store::RunEnd {
                outcome: rhapsody_store::OUTCOME_REFUSED.to_string(),
                ended_at: crate::persist::rfc3339(now),
                turns: 0,
                input_tokens: 0,
                output_tokens: 0,
                total_tokens: 0,
                usage_estimated: false,
                error: reason.to_string(),
                transcript_path: String::new(),
            },
        ) {
            tracing::error!(
                issue_identifier = %issue.identifier,
                error = %e,
                "persist refusal end run failed"
            );
        }
    }

    /// Cancels one preparation reservation explicitly (Stop / issue disappearance / ineligibility),
    /// giving back any claim it held. Returns whether a reservation was held and released.
    pub(crate) fn cancel_preparation(&mut self, id: &str) -> bool {
        let Some(entry) = self.preparing.cancel(id) else {
            return false;
        };
        tracing::debug!(id = %id, "preparation cancelled");
        self.abandon_prepared(entry, AbandonCause::Superseded);
        true
    }

    /// Cancels every ticket preparation whose issue is NOT in `present` — the current candidate set.
    /// An issue that disappeared from the board (terminal, reassigned, filtered out) must not keep a
    /// reservation, a resolver task, or a concurrency slot. Review keys are untouched: they are never
    /// in the candidate set by construction. A claim-held retry is released (mirrors `on_retry`'s
    /// release-on-gone) rather than stranded.
    pub(crate) fn cancel_dropped_preparations(
        &mut self,
        present: &std::collections::HashSet<String>,
    ) {
        let dropped: Vec<String> = self
            .preparing
            .values()
            .filter(|e| {
                matches!(e.key, PreparationKey::Ticket { .. }) && !present.contains(e.key.id())
            })
            .map(|e| e.id().to_string())
            .collect();
        for id in dropped {
            tracing::info!(issue_id = %id, "issue left the candidate set; cancelling its preparation");
            self.cancel_preparation(&id);
        }
    }

    /// Cancels every preparation reservation and re-arms the refusal gate — the reload path: a new
    /// config changes what a preparation would resolve and what a refusal meant. A claim-held retry
    /// is RELEASED so the ticket re-selects under the new config, instead of keeping a claim nothing
    /// in `retry_attempts` will ever give back.
    pub(crate) fn reload_preparations(&mut self) {
        self.prepare_generation = self.prepare_generation.wrapping_add(1);
        let cancelled = self.preparing.cancel_all();
        if !cancelled.is_empty() {
            tracing::info!(
                count = cancelled.len(),
                generation = self.prepare_generation,
                "reload cancelled in-flight preparations and re-armed the refusal gate"
            );
        }
        for entry in cancelled {
            self.abandon_prepared(entry, AbandonCause::Superseded);
        }
        self.refusal_gate.rearm_all();
    }

    /// Cancels every preparation reservation without re-arming the gate — the shutdown path. The
    /// claims are deliberately LEFT in place and persisted: the retry rows were written when the
    /// retry was scheduled, so boot recovery re-arms them; releasing here would DELETE them and lose
    /// the recovery. Only the in-memory reservations and their resolver tasks are torn down.
    pub(crate) fn cancel_all_preparations(&mut self) {
        let cancelled = self.preparing.cancel_all();
        if !cancelled.is_empty() {
            tracing::debug!(
                count = cancelled.len(),
                "shutdown cancelled in-flight preparations"
            );
        }
    }

    /// Re-arms the refusal gate on an explicit operator refresh (the API `/refresh` path), so a
    /// refusal that was suppressing work is re-probed immediately.
    pub(crate) fn rearm_refusal_gate(&mut self) {
        self.refusal_gate.rearm_all();
    }
}

/// Why a claim-held preparation ended without dispatching. The two causes differ in what happens to
/// the claim the retry was holding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AbandonCause {
    /// An armed drain deferred the work: PARK the retry (keep the claim and attempt, re-arm).
    Drained,
    /// A reload, a stale generation or the issue leaving the board: GIVE the claim back.
    Superseded,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use chrono::TimeZone;

    use crate::testsupport::{DispatchedEntries, empty_effective, issue, record_entries, set_of};
    use rhapsody_tracker::fake::Fake;

    fn fixed_now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 22, 12, 0, 0)
            .single()
            .expect("valid fixed instant")
    }

    /// A fake resolver the test drives by hand: it records how many times it was called and replies
    /// with a scripted completion (optionally after a delay, to model a hung/blocked resolver).
    struct FakeResolver {
        calls: Arc<AtomicUsize>,
        outcome: Mutex<Scripted>,
    }

    enum Scripted {
        Ready,
        Refused(RefusalReason),
        Hang,
    }

    #[async_trait]
    impl PreparationResolver for FakeResolver {
        async fn prepare(&self, _req: &PreparationRequest) -> PreparationCompletion {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let scripted = {
                let guard = self
                    .outcome
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                match &*guard {
                    Scripted::Ready => 0,
                    Scripted::Refused(_) => 1,
                    Scripted::Hang => 2,
                }
            };
            match scripted {
                0 => PreparationCompletion {
                    outcome: PreparationOutcome::Ready(PreparedDispatch::new(
                        "claude",
                        "opus",
                        "anthropic",
                        "rev-1",
                    )),
                    observed_revision: "rev-1".to_string(),
                },
                1 => {
                    let reason = {
                        let guard = self
                            .outcome
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        match &*guard {
                            Scripted::Refused(r) => r.clone(),
                            _ => RefusalReason::CredentialAbsent,
                        }
                    };
                    PreparationCompletion {
                        outcome: PreparationOutcome::Refused(reason),
                        observed_revision: String::new(),
                    }
                }
                _ => {
                    std::future::pending::<()>().await;
                    unreachable!("a hanging resolver never resolves")
                }
            }
        }
    }

    /// Builds a legacy-path orchestrator with a fake resolver and one Todo candidate, wired like the
    /// preflight/loop tests. Returns the orchestrator, dispatch sink, and resolver call counter.
    fn orch_with_resolver(
        scripted: Scripted,
    ) -> (Orchestrator, DispatchedEntries, Arc<AtomicUsize>) {
        let mut tr = Fake::new();
        tr.candidates = vec![issue("1", "MT-1", "Todo")];
        let mut eff = empty_effective(Arc::new(tr));
        eff.active_states = set_of(&["todo", "in progress"]);
        eff.terminal_states = set_of(&["done"]);
        eff.max_concurrent = 10;
        eff.poll_interval = Duration::from_secs(3600);
        eff.max_retry_backoff_ms = 300_000;
        let mut o = Orchestrator::new("WORKFLOW.md");
        o.eff = Some(eff);
        let calls = Arc::new(AtomicUsize::new(0));
        o.prepare_resolver = Some(Arc::new(FakeResolver {
            calls: Arc::clone(&calls),
            outcome: Mutex::new(scripted),
        }));
        let sink: DispatchedEntries = Arc::new(Mutex::new(Vec::new()));
        o.spawn = Some(record_entries(&sink));
        (o, sink, calls)
    }

    fn ticket_target(state: &str) -> PreparedTarget {
        PreparedTarget::Ticket {
            issue: issue("1", "MT-1", state),
            attempt: None,
            route: None,
            stack_context: String::new(),
            pool: false,
            pool_proj: None,
        }
    }

    // --- no resolver: byte-identical / inert by default -------------------------------------------

    #[test]
    fn no_resolver_makes_preparation_inert() {
        let mut o = Orchestrator::new("WORKFLOW.md");
        assert!(!o.preparation_enabled());
        assert_eq!(
            o.begin_preparation(ticket_target("Todo"), false),
            BeginPreparation::NoResolver,
            "without a resolver the caller must dispatch inline exactly as before"
        );
        assert!(o.preparing.is_empty());
    }

    // --- the loop-owned reservation dedupes concurrent paths --------------------------------------

    // MUTATION GUARD: remove the `preparing` reservation (or its dedupe) and a second concurrent
    // tick/retry path dispatches the same issue — this test reds.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_second_begin_for_the_same_issue_is_refused_while_preparing() {
        let (mut o, _sink, calls) = orch_with_resolver(Scripted::Hang);
        let first = o.begin_preparation(ticket_target("Todo"), false);
        assert!(matches!(first, BeginPreparation::Started(_)));
        assert_eq!(
            o.begin_preparation(ticket_target("Todo"), false),
            BeginPreparation::AlreadyPreparing,
            "a second path must not spawn a duplicate resolver/dispatch"
        );
        assert_eq!(o.preparing.len(), 1, "exactly one reservation is held");
        // The reservation counts against the duplicate/concurrency gates.
        assert!(o.running_id_set().contains("1"));
        assert!(o.running_state_counts().contains_key("todo"));
        // Give the spawned task a moment to report, then confirm it was called exactly once.
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "exactly one resolver task was spawned for the deduped identity"
        );
    }

    #[test]
    fn a_running_or_claimed_issue_refuses_preparation() {
        let (mut o, _sink, _calls) = orch_with_resolver(Scripted::Hang);
        o.claimed.insert("1".to_string());
        assert_eq!(
            o.begin_preparation(ticket_target("Todo"), false),
            BeginPreparation::AlreadyInFlight
        );
        assert!(
            o.preparing.is_empty(),
            "no reservation is made for live work"
        );
    }

    // --- accepted success: the reservation is consumed and the run dispatches ----------------------

    #[tokio::test(flavor = "multi_thread")]
    async fn accepted_success_dispatches_the_issue() {
        let (mut o, sink, _calls) = orch_with_resolver(Scripted::Ready);
        assert!(matches!(
            o.begin_preparation(ticket_target("Todo"), false),
            BeginPreparation::Started(_)
        ));
        // Drive the loop-side completion exactly as the event channel would.
        let completion = PreparationCompletion {
            outcome: PreparationOutcome::Ready(PreparedDispatch::new(
                "claude",
                "opus",
                "anthropic",
                "rev-1",
            )),
            observed_revision: "rev-1".to_string(),
        };
        let token = o.preparing.get("1").map(|e| e.token);
        let Some(token) = token else {
            panic!("a reservation should exist");
        };
        o.handle_dispatch_prepared("1".to_string(), token, completion)
            .await;
        assert_eq!(
            sink.lock().expect("dispatch sink").len(),
            1,
            "an accepted success must dispatch the ticket"
        );
        assert!(
            o.preparing.is_empty(),
            "the reservation is consumed exactly once"
        );
    }

    // --- accepted refusal: a zero-turn row, no claim/run/workspace --------------------------------

    // MUTATION GUARD: create a claim/run/mailbox before preparation success and a refusal
    // side-effect inventory fails — this test asserts the refusal leaves none of them.
    #[tokio::test(flavor = "multi_thread")]
    async fn accepted_refusal_records_zero_turn_row_without_side_effects() {
        let (mut o, sink, _calls) =
            orch_with_resolver(Scripted::Refused(RefusalReason::CredentialDeniedOrLocked));
        let store: Arc<dyn rhapsody_store::Store + Send + Sync> = Arc::new(
            rhapsody_store::Sqlite::open(rhapsody_store::StorePath::InMemory).expect("store"),
        );
        o.set_store(Arc::clone(&store));
        o.now = Box::new(fixed_now);
        assert!(matches!(
            o.begin_preparation(ticket_target("Todo"), false),
            BeginPreparation::Started(_)
        ));
        let token = o.preparing.get("1").map(|e| e.token).expect("reservation");
        let completion = PreparationCompletion {
            outcome: PreparationOutcome::Refused(RefusalReason::CredentialDeniedOrLocked),
            observed_revision: String::new(),
        };
        o.handle_dispatch_prepared("1".to_string(), token, completion)
            .await;

        assert!(
            sink.lock().expect("dispatch sink").is_empty(),
            "a refusal must not dispatch an agent"
        );
        assert!(
            o.running.is_empty(),
            "a refusal creates no active-run entry"
        );
        assert!(o.claimed.is_empty(), "a refusal creates no claim");
        assert!(
            o.mailboxes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty(),
            "a refusal creates no mailbox"
        );
        assert!(
            o.preparing.is_empty(),
            "the reservation is released exactly once"
        );
        assert_eq!(
            o.refusal_gate.len(),
            1,
            "the refusal gate holds the fingerprinted refusal"
        );

        let runs = store
            .runs_for_issues(&["MT-1".to_string()], 10)
            .expect("runs query");
        assert_eq!(runs.len(), 1, "exactly one zero-turn refusal row");
        assert_eq!(runs[0].outcome, rhapsody_store::OUTCOME_REFUSED);
        assert_eq!(runs[0].turns, 0);
        assert_eq!(runs[0].total_tokens, 0);
        assert!(
            runs[0].error.contains("locked") || runs[0].error.contains("denied"),
            "the row carries the actionable reason: {}",
            runs[0].error
        );
    }

    // --- stale token / generation completions cannot mutate state ---------------------------------

    // MUTATION GUARD: accept a stale completion token or generation and deterministic reordered
    // completions mutate state — this test reds.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_stale_token_completion_is_dropped() {
        let (mut o, sink, _calls) = orch_with_resolver(Scripted::Ready);
        assert!(matches!(
            o.begin_preparation(ticket_target("Todo"), false),
            BeginPreparation::Started(_)
        ));
        let stale = PreparationToken {
            generation: 0,
            seq: 999,
        };
        let completion = PreparationCompletion {
            outcome: PreparationOutcome::Ready(PreparedDispatch::new(
                "claude",
                "opus",
                "anthropic",
                "rev-1",
            )),
            observed_revision: "rev-1".to_string(),
        };
        o.handle_dispatch_prepared("1".to_string(), stale, completion)
            .await;
        assert!(
            sink.lock().expect("dispatch sink").is_empty(),
            "a stale token must not dispatch"
        );
        assert_eq!(
            o.preparing.len(),
            1,
            "a stale completion must not release the current reservation"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_completion_from_an_old_config_generation_is_dropped() {
        let (mut o, sink, _calls) = orch_with_resolver(Scripted::Ready);
        assert!(matches!(
            o.begin_preparation(ticket_target("Todo"), false),
            BeginPreparation::Started(_)
        ));
        let token = o.preparing.get("1").map(|e| e.token).expect("reservation");
        // A reload bumps the generation and cancels the reservation; a late completion is stale.
        o.reload_preparations();
        let completion = PreparationCompletion {
            outcome: PreparationOutcome::Ready(PreparedDispatch::new(
                "claude",
                "opus",
                "anthropic",
                "rev-1",
            )),
            observed_revision: "rev-1".to_string(),
        };
        o.handle_dispatch_prepared("1".to_string(), token, completion)
            .await;
        assert!(
            sink.lock().expect("dispatch sink").is_empty(),
            "a completion minted under an older config generation must not dispatch"
        );
        assert!(o.preparing.is_empty());
    }

    // --- cancellation releases exactly once, and a late completion is stale -----------------------

    #[tokio::test(flavor = "multi_thread")]
    async fn cancel_releases_the_reservation_and_a_late_completion_is_inert() {
        let (mut o, sink, _calls) = orch_with_resolver(Scripted::Ready);
        assert!(matches!(
            o.begin_preparation(ticket_target("Todo"), false),
            BeginPreparation::Started(_)
        ));
        let token = o.preparing.get("1").map(|e| e.token).expect("reservation");
        o.cancel_preparation("1");
        assert!(
            o.preparing.is_empty(),
            "cancel releases the reservation once"
        );
        assert!(!o.cancel_preparation("1"), "a second cancel is a no-op");

        let completion = PreparationCompletion {
            outcome: PreparationOutcome::Ready(PreparedDispatch::new(
                "claude",
                "opus",
                "anthropic",
                "rev-1",
            )),
            observed_revision: "rev-1".to_string(),
        };
        o.handle_dispatch_prepared("1".to_string(), token, completion)
            .await;
        assert!(
            sink.lock().expect("dispatch sink").is_empty(),
            "a completion delivered after cancellation is stale and must not dispatch"
        );
    }

    // --- the refusal gate suppresses the identical refusal and re-arms on input change -------------

    #[test]
    fn refusal_gate_suppresses_identical_and_rearms_on_change() {
        let now = fixed_now();
        let key = RefusalGate::key("ticket", "1", "sel");
        let mut gate = RefusalGate::default();
        assert!(!gate.suppressed(&key, now));
        let backoff = gate.record(&key, "credential_absent", "", now);
        assert_eq!(backoff, REFUSAL_BACKOFF_BASE_MS);
        assert!(gate.suppressed(&key, now));
        assert!(gate.suppressed(&key, now + chrono::Duration::seconds(1)));
        // A repeat at the SAME revision advances bounded backoff and is not a new episode.
        assert_eq!(gate.observed_revision(&key), Some(""));
        let next = gate.record(&key, "credential_absent", "", now);
        assert_eq!(next, REFUSAL_BACKOFF_BASE_MS * 2);
        // Past the probe time the gate no longer suppresses.
        let far = now + chrono::Duration::milliseconds(next + 1);
        assert!(!gate.suppressed(&key, far));
        // A CHANGED credential revision is a fresh episode at the same key: base backoff again.
        let fresh = gate.record(&key, "credential_absent", "rev-2", now);
        assert_eq!(fresh, REFUSAL_BACKOFF_BASE_MS);
        assert_eq!(gate.observed_revision(&key), Some("rev-2"));
        // The key deliberately excludes the revision, so the pre-spawn check suppresses even after a
        // revision was observed — PB7 re-arms on a credential mutation instead.
        assert!(gate.suppressed(&key, now));
        // A different identity is a different key and never suppressed.
        let other = RefusalGate::key("ticket", "2", "sel");
        assert!(!gate.suppressed(&other, now));
        // An explicit re-arm releases the key.
        gate.rearm(&key);
        assert!(!gate.suppressed(&key, now));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_suppressed_refusal_does_not_spawn_or_dispatch() {
        let (mut o, sink, calls) =
            orch_with_resolver(Scripted::Refused(RefusalReason::CredentialAbsent));
        o.now = Box::new(fixed_now);
        assert!(matches!(
            o.begin_preparation(ticket_target("Todo"), false),
            BeginPreparation::Started(_)
        ));
        let token = o.preparing.get("1").map(|e| e.token).expect("reservation");
        o.handle_dispatch_prepared(
            "1".to_string(),
            token,
            PreparationCompletion {
                outcome: PreparationOutcome::Refused(RefusalReason::CredentialAbsent),
                observed_revision: String::new(),
            },
        )
        .await;
        // Let the first resolver task actually run before sampling its call count.
        std::thread::sleep(Duration::from_millis(30));
        let calls_after_first = calls.load(Ordering::SeqCst);
        // The identical refusal is still suppressing: a second begin neither spawns nor dispatches.
        assert_eq!(
            o.begin_preparation(ticket_target("Todo"), false),
            BeginPreparation::Suppressed
        );
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            calls_after_first,
            "a suppressed fingerprint must not spawn resolver work"
        );
        assert!(sink.lock().expect("dispatch sink").is_empty());
        // A reload re-arms the gate and the generation, so the next begin is accepted again.
        o.reload_preparations();
        assert!(matches!(
            o.begin_preparation(ticket_target("Todo"), false),
            BeginPreparation::Started(_)
        ));
    }

    // --- the resolver runs off the control task: a hung resolver cannot delay an independent tick --

    #[tokio::test(flavor = "multi_thread")]
    async fn a_hung_resolver_does_not_block_the_control_task() {
        let (mut o, _sink, _calls) = orch_with_resolver(Scripted::Hang);
        o.prepare_timeout = Duration::from_secs(3600); // the resolver stays hung for the test
        let started = std::time::Instant::now();
        assert!(matches!(
            o.begin_preparation(ticket_target("Todo"), false),
            BeginPreparation::Started(_)
        ));
        // begin_preparation must return immediately even though the resolver never answers.
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "begin_preparation must not await the resolver on the control task"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_resolver_overrunning_its_timeout_releases_loop_state() {
        let (mut o, sink, _calls) = orch_with_resolver(Scripted::Hang);
        o.prepare_timeout = Duration::from_millis(30);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        o.events = tx;
        assert!(matches!(
            o.begin_preparation(ticket_target("Todo"), false),
            BeginPreparation::Started(_)
        ));
        // The timeout produces a typed completion on the event channel.
        let ev = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("a completion arrives")
            .expect("channel open");
        match ev {
            Event::DispatchPrepared {
                id,
                token,
                completion,
            } => {
                assert_eq!(id, "1");
                match completion.outcome {
                    PreparationOutcome::Refused(RefusalReason::ResolverTimedOut) => {}
                    other => panic!("expected a typed timeout refusal, got {other:?}"),
                }
                o.handle_dispatch_prepared(id, token, completion).await;
            }
            _ => panic!("expected DispatchPrepared"),
        }
        assert!(o.preparing.is_empty(), "the timeout releases loop state");
        assert!(
            sink.lock().expect("dispatch sink").is_empty(),
            "a timed-out preparation never dispatches"
        );
    }

    // --- drain / issue disappearance / review target / concurrency bound / refresh re-arm ---------

    fn ticket_target_id(id: &str, ident: &str, state: &str) -> PreparedTarget {
        PreparedTarget::Ticket {
            issue: issue(id, ident, state),
            attempt: None,
            route: None,
            stack_context: String::new(),
            pool: false,
            pool_proj: None,
        }
    }

    // MUTATION GUARD: accept a completion after a drain is armed and an event-order test dispatches;
    // this test asserts an armed drain defers instead.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_armed_drain_defers_an_accepted_completion() {
        let (mut o, sink, _calls) = orch_with_resolver(Scripted::Ready);
        assert!(matches!(
            o.begin_preparation(ticket_target("Todo"), false),
            BeginPreparation::Started(_)
        ));
        o.drain
            .arm(fixed_now(), crate::drain::DrainReason::Operator);
        let token = o.preparing.get("1").map(|e| e.token).expect("reservation");
        o.handle_dispatch_prepared(
            "1".to_string(),
            token,
            PreparationCompletion {
                outcome: PreparationOutcome::Ready(PreparedDispatch::new(
                    "claude",
                    "opus",
                    "anthropic",
                    "rev-1",
                )),
                observed_revision: "rev-1".to_string(),
            },
        )
        .await;
        assert!(
            sink.lock().expect("dispatch sink").is_empty(),
            "an armed drain must defer an otherwise-accepted completion"
        );
        assert!(o.preparing.is_empty(), "the reservation is released");
        assert!(
            o.refusal_gate.is_empty(),
            "a deferred completion is not a refusal and must not arm the gate"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_departed_issue_cancels_its_preparation() {
        let (mut o, _sink, _calls) = orch_with_resolver(Scripted::Hang);
        assert!(matches!(
            o.begin_preparation(ticket_target_id("1", "MT-1", "Todo"), false),
            BeginPreparation::Started(_)
        ));
        assert!(matches!(
            o.begin_preparation(ticket_target_id("2", "MT-2", "Todo"), false),
            BeginPreparation::Started(_)
        ));
        // Only issue 1 is still on the board; issue 2 must be released.
        let present: std::collections::HashSet<String> = ["1".to_string()].into_iter().collect();
        o.cancel_dropped_preparations(&present);
        assert_eq!(o.preparing.len(), 1);
        assert!(o.preparing.contains("1"));
        assert!(!o.preparing.contains("2"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn shutdown_cancels_every_preparation_and_a_late_completion_is_inert() {
        let (mut o, sink, _calls) = orch_with_resolver(Scripted::Ready);
        for (id, ident) in [("1", "MT-1"), ("2", "MT-2")] {
            assert!(matches!(
                o.begin_preparation(ticket_target_id(id, ident, "Todo"), false),
                BeginPreparation::Started(_)
            ));
        }
        let token = o.preparing.get("2").map(|e| e.token).expect("reservation");
        o.cancel_all_preparations();
        assert!(
            o.preparing.is_empty(),
            "shutdown releases every reservation"
        );
        // A completion delivered after shutdown is stale and must not dispatch.
        o.handle_dispatch_prepared(
            "2".to_string(),
            token,
            PreparationCompletion {
                outcome: PreparationOutcome::Ready(PreparedDispatch::new(
                    "claude",
                    "opus",
                    "anthropic",
                    "rev-1",
                )),
                observed_revision: "rev-1".to_string(),
            },
        )
        .await;
        assert!(sink.lock().expect("dispatch sink").is_empty());
    }

    // The review path shares the same machinery: the same reservation map, token and gates.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_review_target_uses_the_same_preparation_machinery() {
        let (mut o, _sink, _calls) = orch_with_resolver(Scripted::Ready);
        let run = crate::review::ReviewRun {
            owner: "o".to_string(),
            repo: "r".to_string(),
            number: 7,
            reviewer: "alice".to_string(),
            head_sha: "abc".to_string(),
            ..Default::default()
        };
        let route = DispatchRoute {
            slug: "alpha".to_string(),
            group: "alpha".to_string(),
            repo: "https://example.test/o/r".to_string(),
            model: "opus".to_string(),
            workspace_mode: String::new(),
        };
        let target = PreparedTarget::Review {
            issue: run.synthetic_issue(),
            run: Box::new(run.clone()),
            route,
        };
        assert!(matches!(
            o.begin_preparation(target, false),
            BeginPreparation::Started(_)
        ));
        let key = run.key();
        assert!(
            o.preparing.contains(&key),
            "the review reservation is keyed by the review identity"
        );
        assert!(
            o.running_id_set().contains(&key),
            "a review preparation counts against the duplicate/concurrency gates exactly as a ticket does"
        );
        // The ticket review identity is distinct from a ticket key with the same raw id.
        assert!(matches!(
            o.begin_preparation(
                PreparedTarget::Review {
                    issue: run.synthetic_issue(),
                    run: Box::new(run),
                    route: DispatchRoute {
                        slug: "alpha".to_string(),
                        group: "alpha".to_string(),
                        repo: "https://example.test/o/r".to_string(),
                        model: "opus".to_string(),
                        workspace_mode: String::new(),
                    },
                },
                false
            ),
            BeginPreparation::AlreadyPreparing
        ));
    }

    // The daemon-wide permit bound: a hung resolver cannot accumulate more than
    // MAX_PREPARATION_CONCURRENCY in-flight tasks however many preparations are begun.
    #[tokio::test(flavor = "multi_thread")]
    async fn resolver_concurrency_is_bounded() {
        let (mut o, _sink, calls) = orch_with_resolver(Scripted::Hang);
        o.prepare_timeout = Duration::from_secs(3600);
        for i in 0..(MAX_PREPARATION_CONCURRENCY + 3) {
            let id = (i + 10).to_string();
            let ident = format!("MT-{id}");
            assert!(matches!(
                o.begin_preparation(ticket_target_id(&id, &ident, "Todo"), false),
                BeginPreparation::Started(_)
            ));
        }
        // Let the spawned tasks run; only the permit-holding tasks ever reach the resolver.
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            MAX_PREPARATION_CONCURRENCY,
            "no more than the daemon-wide bound of resolver tasks may run at once"
        );
        assert_eq!(
            o.preparing.len(),
            MAX_PREPARATION_CONCURRENCY + 3,
            "every reservation is still tracked even while its resolver waits for a permit"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_explicit_refresh_rearms_the_refusal_gate() {
        let (mut o, _sink, _calls) =
            orch_with_resolver(Scripted::Refused(RefusalReason::CredentialAbsent));
        o.now = Box::new(fixed_now);
        assert!(matches!(
            o.begin_preparation(ticket_target("Todo"), false),
            BeginPreparation::Started(_)
        ));
        let token = o.preparing.get("1").map(|e| e.token).expect("reservation");
        o.handle_dispatch_prepared(
            "1".to_string(),
            token,
            PreparationCompletion {
                outcome: PreparationOutcome::Refused(RefusalReason::CredentialAbsent),
                observed_revision: String::new(),
            },
        )
        .await;
        assert_eq!(
            o.begin_preparation(ticket_target("Todo"), false),
            BeginPreparation::Suppressed
        );
        // An explicit operator refresh re-arms the gate without bumping the generation.
        let generation = o.prepare_generation;
        o.rearm_refusal_gate();
        assert_eq!(
            o.prepare_generation, generation,
            "refresh does not bump the generation"
        );
        assert!(matches!(
            o.begin_preparation(ticket_target("Todo"), false),
            BeginPreparation::Started(_)
        ));
    }

    // --- STUDIO-988 review round: caps, claim hand-back, gate dedup, pool ordering ---------------

    fn ticket_target_at(
        id: &str,
        ident: &str,
        state: &str,
        route: Option<DispatchRoute>,
    ) -> PreparedTarget {
        PreparedTarget::Ticket {
            issue: issue(id, ident, state),
            attempt: None,
            route,
            stack_context: String::new(),
            pool: false,
            pool_proj: None,
        }
    }

    fn sample_route() -> DispatchRoute {
        DispatchRoute {
            slug: "alpha".to_string(),
            group: "alpha".to_string(),
            repo: "https://example.test/o/r".to_string(),
            model: "opus".to_string(),
            workspace_mode: String::new(),
        }
    }

    // MUTATION GUARD: drop `preparing` from `implementation_pool_holders` /
    // `running_in_project_group` / `review_pool_holders` and a hung preparation spends no slot —
    // this test reds.
    #[tokio::test(flavor = "multi_thread")]
    async fn preparing_counts_against_the_global_project_and_review_caps() {
        let (mut o, _sink, _calls) = orch_with_resolver(Scripted::Hang);
        let target = ticket_target_at("1", "MT-1", "Todo", Some(sample_route()));
        assert!(matches!(
            o.begin_preparation(target, false),
            BeginPreparation::Started(_)
        ));
        assert_eq!(
            o.implementation_pool_holders(),
            1,
            "a ticket preparation spends the global implementation pool"
        );
        assert_eq!(
            o.running_in_project_group("alpha"),
            1,
            "a ticket preparation spends its project group's slot"
        );
        assert_eq!(
            o.review_pool_holders(),
            1,
            "with no separate review pool a preparation spends the shared pool the watcher draws"
        );

        // A review preparation draws the REVIEW pool, not the implementation pool (STUDIO-950).
        let mut o = Orchestrator::new("WORKFLOW.md");
        let mut eff = empty_effective(Arc::new(Fake::new()));
        eff.max_concurrent_reviews = Some(2);
        o.eff = Some(eff);
        let calls = Arc::new(AtomicUsize::new(0));
        o.prepare_resolver = Some(Arc::new(FakeResolver {
            calls,
            outcome: Mutex::new(Scripted::Hang),
        }));
        let run = crate::review::ReviewRun {
            owner: "o".to_string(),
            repo: "r".to_string(),
            number: 7,
            reviewer: "alice".to_string(),
            head_sha: "abc".to_string(),
            ..Default::default()
        };
        let target = PreparedTarget::Review {
            issue: run.synthetic_issue(),
            run: Box::new(run.clone()),
            route: sample_route(),
        };
        assert!(matches!(
            o.begin_preparation(target, false),
            BeginPreparation::Started(_)
        ));
        assert_eq!(
            o.ticketless_review_holders(),
            1,
            "a review preparation holds a review-pool slot"
        );
        assert_eq!(
            o.review_pool_holders(),
            1,
            "a review preparation spends the separate review pool"
        );
        assert_eq!(
            o.implementation_pool_holders(),
            0,
            "a review preparation must not spend the implementation pool"
        );
    }

    // Alice's round-1 repro, at the tick boundary: with one global slot and candidates in two
    // different states, two ticks must admit exactly ONE preparation. MUTATION GUARD: exclude
    // `preparing` from the global pool draw and the second tick over-reserves — this test reds.
    #[tokio::test(flavor = "multi_thread")]
    async fn two_ticks_do_not_over_reserve_a_single_global_slot() {
        let mut tr = Fake::new();
        tr.candidates = vec![
            issue("1", "MT-1", "Todo"),
            issue("2", "MT-2", "In Progress"),
        ];
        let mut eff = empty_effective(Arc::new(tr));
        eff.active_states = set_of(&["todo", "in progress"]);
        eff.terminal_states = set_of(&["done"]);
        eff.max_concurrent = 1;
        eff.poll_interval = Duration::from_secs(3600);
        eff.max_retry_backoff_ms = 300_000;
        let mut o = Orchestrator::new("WORKFLOW.md");
        o.eff = Some(eff);
        o.prepare_timeout = Duration::from_secs(3600);
        o.prepare_resolver = Some(Arc::new(FakeResolver {
            calls: Arc::new(AtomicUsize::new(0)),
            outcome: Mutex::new(Scripted::Hang),
        }));
        let sink: DispatchedEntries = Arc::new(Mutex::new(Vec::new()));
        o.spawn = Some(record_entries(&sink));

        o.on_tick().await;
        o.on_tick().await;
        assert_eq!(
            o.preparing.len(),
            1,
            "a single global slot admits exactly one preparation across two ticks"
        );
    }

    // MUTATION GUARD: `PreparingReservations::cancel_all` used to clear the vector before returning
    // it (always empty), so the reload path released no claims and its count log never fired — this
    // test reds if `cancel_all` stops returning its entries.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_reload_releases_a_claim_held_retry_preparation() {
        let (mut o, _sink, _calls) = orch_with_resolver(Scripted::Hang);
        o.claimed.insert("1".to_string());
        o.dispatch_or_prepare(issue("1", "MT-1", "Todo"), Some(3), None, String::new());
        assert!(o.preparing.contains("1"));
        o.reload_preparations();
        assert!(o.preparing.is_empty(), "reload cancels the reservation");
        assert!(
            !o.claimed.contains("1"),
            "reload gives back the retry claim so the ticket re-selects"
        );
        assert!(o.refusal_gate.is_empty(), "reload re-arms the gate");
    }

    // MUTATION GUARD: keep the claim on a refusal and the retry is stranded (nothing in
    // `retry_attempts` will ever fire it again) — this test reds.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_refused_retry_releases_its_claim_instead_of_stranding() {
        let (mut o, sink, _calls) =
            orch_with_resolver(Scripted::Refused(RefusalReason::CredentialAbsent));
        o.claimed.insert("1".to_string()); // on_retry kept the claim for this retry
        o.dispatch_or_prepare(issue("1", "MT-1", "Todo"), Some(1), None, String::new());
        assert!(o.preparing.contains("1"), "the retry began preparing");
        let token = o.preparing.get("1").map(|e| e.token).expect("reservation");
        o.handle_dispatch_prepared(
            "1".to_string(),
            token,
            PreparationCompletion {
                outcome: PreparationOutcome::Refused(RefusalReason::CredentialAbsent),
                observed_revision: String::new(),
            },
        )
        .await;
        assert!(
            !o.claimed.contains("1"),
            "a refusal must release the retry's claim so the ticket re-probes"
        );
        assert!(
            o.retry_attempts.is_empty(),
            "a refusal does not enter agent retry"
        );
        assert!(o.completed.is_empty());
        assert!(sink.lock().expect("dispatch sink").is_empty());
    }

    // MUTATION GUARD: drop a retry's preparation on a drain and on_retry's own parking promise
    // ("keeps its claim, its due time and its attempt") is broken — this test reds.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_drain_defers_a_retry_by_re_parking_it_not_dropping_it() {
        let (mut o, sink, _calls) = orch_with_resolver(Scripted::Ready);
        o.claimed.insert("1".to_string());
        o.dispatch_or_prepare(issue("1", "MT-1", "Todo"), Some(2), None, String::new());
        let token = o.preparing.get("1").map(|e| e.token).expect("reservation");
        o.drain
            .arm(fixed_now(), crate::drain::DrainReason::Operator);
        o.handle_dispatch_prepared(
            "1".to_string(),
            token,
            PreparationCompletion {
                outcome: PreparationOutcome::Ready(PreparedDispatch::new(
                    "claude",
                    "opus",
                    "anthropic",
                    "rev-1",
                )),
                observed_revision: "rev-1".to_string(),
            },
        )
        .await;
        assert!(
            sink.lock().expect("dispatch sink").is_empty(),
            "a drain must not dispatch"
        );
        assert!(
            o.claimed.contains("1"),
            "a drain parks the retry, keeping its claim"
        );
        let re = o
            .retry_attempts
            .get("1")
            .expect("the deferred retry is re-parked");
        assert_eq!(
            re.attempt, 2,
            "the attempt number is preserved across the drain"
        );
        assert!(o.preparing.is_empty());
    }

    // MUTATION GUARD: append a row on every accepted refusal and a locked credential writes one row
    // per backoff step forever — this test reds.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_repeated_identical_refusal_appends_no_second_history_row() {
        let (mut o, _sink, _calls) =
            orch_with_resolver(Scripted::Refused(RefusalReason::CredentialAbsent));
        let store: Arc<dyn rhapsody_store::Store + Send + Sync> = Arc::new(
            rhapsody_store::Sqlite::open(rhapsody_store::StorePath::InMemory).expect("store"),
        );
        o.set_store(Arc::clone(&store));
        let cell = Arc::new(Mutex::new(fixed_now()));
        let clock = Arc::clone(&cell);
        o.now = Box::new(move || *clock.lock().unwrap_or_else(|e| e.into_inner()));

        let refuse = |o: &mut Orchestrator| {
            assert!(matches!(
                o.begin_preparation(ticket_target("Todo"), false),
                BeginPreparation::Started(_)
            ));
        };
        refuse(&mut o);
        let token = o.preparing.get("1").map(|e| e.token).expect("reservation");
        o.handle_dispatch_prepared(
            "1".to_string(),
            token,
            PreparationCompletion {
                outcome: PreparationOutcome::Refused(RefusalReason::CredentialAbsent),
                observed_revision: String::new(),
            },
        )
        .await;
        let rows = store
            .runs_for_issues(&["MT-1".to_string()], 10)
            .expect("runs query");
        assert_eq!(rows.len(), 1, "the first refusal writes exactly one row");

        // Advance past the base backoff so the gate re-probes, then refuse identically again.
        *cell.lock().unwrap_or_else(|e| e.into_inner()) =
            fixed_now() + chrono::Duration::milliseconds(REFUSAL_BACKOFF_BASE_MS + 1);
        refuse(&mut o);
        let token = o.preparing.get("1").map(|e| e.token).expect("reservation");
        o.handle_dispatch_prepared(
            "1".to_string(),
            token,
            PreparationCompletion {
                outcome: PreparationOutcome::Refused(RefusalReason::CredentialAbsent),
                observed_revision: String::new(),
            },
        )
        .await;
        let rows = store
            .runs_for_issues(&["MT-1".to_string()], 10)
            .expect("runs query");
        assert_eq!(
            rows.len(),
            1,
            "a repeat of the identical refusal advances the backoff but writes no second row"
        );
        assert_eq!(o.refusal_gate.len(), 1, "one gated fingerprint");
    }

    // MUTATION GUARD: key the gate by the credential revision and begin_preparation (which cannot
    // know a revision a resolver has yet to observe) never suppresses once PB7 supplies one — this
    // test reds.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_gate_suppresses_a_refusal_that_observed_a_credential_revision() {
        let (mut o, _sink, _calls) = orch_with_resolver(Scripted::Hang);
        o.now = Box::new(fixed_now);
        assert!(matches!(
            o.begin_preparation(ticket_target("Todo"), false),
            BeginPreparation::Started(_)
        ));
        let token = o.preparing.get("1").map(|e| e.token).expect("reservation");
        o.handle_dispatch_prepared(
            "1".to_string(),
            token,
            PreparationCompletion {
                outcome: PreparationOutcome::Refused(RefusalReason::CredentialAbsent),
                observed_revision: "rev-7".to_string(),
            },
        )
        .await;
        assert_eq!(
            o.begin_preparation(ticket_target("Todo"), false),
            BeginPreparation::Suppressed,
            "the pre-spawn check must suppress even after a non-empty revision was observed"
        );
    }

    // MUTATION GUARD: the stale-generation branch is unreachable through the reload path (which
    // cancels first), so nothing else protects it — this test pins it directly.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_bumped_generation_without_cancellation_drops_the_completion() {
        let (mut o, sink, _calls) = orch_with_resolver(Scripted::Ready);
        assert!(matches!(
            o.begin_preparation(ticket_target("Todo"), false),
            BeginPreparation::Started(_)
        ));
        let token = o.preparing.get("1").map(|e| e.token).expect("reservation");
        // A generation bump that did NOT cancel the reservation (the shape a future PB7 path could
        // produce): the generation guard, not the missing entry, must drop the completion.
        o.prepare_generation = token.generation + 1;
        o.handle_dispatch_prepared(
            "1".to_string(),
            token,
            PreparationCompletion {
                outcome: PreparationOutcome::Ready(PreparedDispatch::new(
                    "claude",
                    "opus",
                    "anthropic",
                    "rev-1",
                )),
                observed_revision: "rev-1".to_string(),
            },
        )
        .await;
        assert!(
            sink.lock().expect("dispatch sink").is_empty(),
            "a completion minted under an older generation must not dispatch"
        );
        assert!(o.preparing.is_empty(), "the stale reservation is released");
    }

    fn pool_orch(scripted: Scripted) -> (Orchestrator, Arc<Fake>, DispatchedEntries) {
        let mut tr = Fake::new();
        tr.viewer = rhapsody_core::Viewer {
            id: "me".to_string(),
            ..Default::default()
        };
        tr.candidates = vec![issue("1", "MT-1", "Todo")];
        let tr = Arc::new(tr);
        let mut eff = empty_effective(Arc::clone(&tr) as Arc<dyn rhapsody_tracker::Tracker>);
        eff.active_states = set_of(&["todo", "in progress"]);
        eff.terminal_states = set_of(&["done"]);
        eff.max_concurrent = 10;
        eff.poll_interval = Duration::from_secs(3600);
        eff.claim_ttl = Duration::from_secs(60);
        eff.claim_settle_delay = Duration::from_millis(1);
        eff.review_promote_state = String::new();
        let mut o = Orchestrator::new("WORKFLOW.md");
        o.eff = Some(eff);
        o.prepare_resolver = Some(Arc::new(FakeResolver {
            calls: Arc::new(AtomicUsize::new(0)),
            outcome: Mutex::new(scripted),
        }));
        let sink: DispatchedEntries = Arc::new(Mutex::new(Vec::new()));
        o.spawn = Some(record_entries(&sink));
        (o, tr, sink)
    }

    // MUTATION GUARD: claim the pool pick before preparation (the pre-fix order) and a refused pick
    // is left assigned and moved with no run — this test reds.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_pool_pick_is_prepared_before_its_claim_election() {
        let (mut o, tr, sink) = pool_orch(Scripted::Hang);
        let pick = crate::select::TaggedIssue {
            iss: issue("1", "MT-1", "Todo"),
            proj: None,
        };
        o.dispatch_or_prepare_pool(pick).await;
        assert_eq!(
            tr.create_comment_calls().len(),
            0,
            "no claim comment may be posted before preparation completes"
        );
        assert_eq!(
            tr.assign_calls().len(),
            0,
            "no assignment may be made before preparation completes"
        );
        assert!(sink.lock().expect("dispatch sink").is_empty());

        // A refusal leaves the ticket UNCLAIMED — the whole point of preparing first.
        let token = o.preparing.get("1").map(|e| e.token).expect("reservation");
        o.handle_dispatch_prepared(
            "1".to_string(),
            token,
            PreparationCompletion {
                outcome: PreparationOutcome::Refused(RefusalReason::CredentialAbsent),
                observed_revision: String::new(),
            },
        )
        .await;
        assert_eq!(
            tr.create_comment_calls().len(),
            0,
            "a refused pool pick is never claimed"
        );
        assert_eq!(
            tr.assign_calls().len(),
            0,
            "a refused pool pick is never assigned"
        );
        assert!(sink.lock().expect("dispatch sink").is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_pool_pick_claims_and_dispatches_only_after_preparation_succeeds() {
        let (mut o, tr, sink) = pool_orch(Scripted::Ready);
        let pick = crate::select::TaggedIssue {
            iss: issue("1", "MT-1", "Todo"),
            proj: None,
        };
        o.dispatch_or_prepare_pool(pick).await;
        let token = o.preparing.get("1").map(|e| e.token).expect("reservation");
        o.handle_dispatch_prepared(
            "1".to_string(),
            token,
            PreparationCompletion {
                outcome: PreparationOutcome::Ready(PreparedDispatch::new(
                    "claude",
                    "opus",
                    "anthropic",
                    "rev-1",
                )),
                observed_revision: "rev-1".to_string(),
            },
        )
        .await;
        assert_eq!(
            tr.assign_calls().len(),
            1,
            "an accepted pool preparation claims the ticket"
        );
        assert_eq!(
            sink.lock().expect("dispatch sink").len(),
            1,
            "an accepted pool preparation dispatches the winner"
        );
    }
}
