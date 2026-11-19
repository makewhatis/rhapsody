//! teamsprefetch — Rhapsody Teams' **off-loop memory prefetch** (STUDIO-660,
//! slice T8; design record `~/.rhapsody/docs/STUDIO-572-rhapsody-teams.md`, §5,
//! §5.2, §5.4).
//!
//! `memory.backend: hindsight` puts the bank on the far side of a tailnet, and
//! turn-1 recall still has to reach the prompt. This module is how those two
//! facts are reconciled **without** putting a network call anywhere near the
//! control task.
//!
//! # The rule, restated in the type
//!
//! [`Orchestrator::teams_bank`](crate::Orchestrator::teams_bank) is
//! `Option<Arc<LocalBank>>` and stays `None` for `hindsight` — unchanged by this
//! slice. `dispatch_issue` is `fn`, not `async fn`, so it *could not* await a
//! remote bank even if one were handed to it; that is the whole reason
//! `crates/config/src/memory.rs` documents the concrete-type choice. So the
//! network moves to a background task, exactly as §0.11.2 moved the triage model
//! turn, and dispatch reads only what that task has already finished.
//!
//! The shape is [`crate::triage`]'s and [`crate::quorum`]'s, deliberately:
//!
//! * spawn-gated at the composition root ([`prefetch_enabled`]) — with Teams
//!   off, or any backend but `hindsight`, **no task exists at all**;
//! * holds no [`Orchestrator`](crate::Orchestrator), sends no control event,
//!   takes no lock the control task blocks on;
//! * injectable seams ([`PrefetchDeps::backend`], [`PrefetchDeps::target`]) so no
//!   test ever dials anything;
//! * exponential back-off on failure, never a hot retry loop against a down API.
//!
//! # Why a lock and not a channel
//!
//! The cache is an `RwLock<HashMap<…>>` rather than a channel feeding a map, and
//! the reason is the reader, not the writer. Dispatch is **synchronous** and must
//! never wait: with a channel, somebody has to drain it into a map, and the only
//! places to do that are the control task (which is the stall we are avoiding)
//! or a third task (which needs the same lock at the end anyway, plus a task).
//! An `RwLock` gives the reader [`try_read`](std::sync::RwLock::try_read) —
//! succeed instantly or give up — which *is* the non-blocking read this slice
//! requires, with no extra moving part. The writer holds the write lock only
//! long enough to swap a map it built outside the lock, so the window a reader
//! can lose is one pointer move.
//!
//! # What a miss costs
//!
//! Nothing but the memory section. A miss, a stale entry, or a lost `try_read`
//! all take the same branch: dispatch proceeds with **no memory section**, which
//! is byte-for-byte what `memory.backend: none` produces. It never waits, never
//! retries inline, and never fails a run. So the degradation story for a laptop
//! with the tailnet down is exact — the team remembers nothing new until it comes
//! back.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use chrono::{DateTime, Utc};
use rhapsody_config::memory::{Fact, MemoryBackend};
use rhapsody_config::teams::{MemoryBackend as BackendKind, Teams};
use rhapsody_core::Issue;

use crate::control_loop::CancelWait;
use crate::teams::{LoadSnapshot, route};
use crate::teamscompose::{recall_query, team_recall_query};

/// The prefetch pass's own cadence. One minute matches [`triage`]'s and is
/// slower than the 30s poll on purpose: this is ahead-of-dispatch work nobody
/// waits on. Only missing, changed or aging entries spend remote recalls.
///
/// [`triage`]: crate::triage::TRIAGE_INTERVAL
pub const PREFETCH_INTERVAL: Duration = Duration::from_secs(60);

/// The back-off ceiling. A bank outage settles at one attempt per 15 minutes
/// rather than one per cadence — triage's "never a hot retry loop against a down
/// API" bound, at the same value.
pub const MAX_PREFETCH_BACKOFF_MS: i64 = 15 * 60 * 1000;

/// How long a prefetched entry may be served for. Past this it is a **miss**,
/// not a stale hit.
///
/// A recall is re-issued as soon as the ticket's recall inputs change
/// ([`TicketStamp`]) or this daemon writes to the bank behind it
/// ([`PrefetchCache::note_written`], [`PrefetchCache::note_invalidated`]), so
/// the TTL only bounds what nothing here can see: a record written to the bank
/// by another process. It is comfortably longer than [`PREFETCH_REFRESH_AGE`]
/// plus [`MAX_PREFETCH_BACKOFF_MS`], so one back-off step on a slow bank does
/// not empty the cache (STUDIO-2169).
pub const PREFETCH_TTL: Duration = Duration::from_secs(45 * 60);

/// How old a published candidate set may be before a cycle refuses it. Separate
/// from [`PREFETCH_TTL`] since STUDIO-2169: the candidate publication ages with
/// the poll loop, not with the bank, and a cycle routing a board that stopped
/// polling ten minutes ago would recall for tickets nobody is looking at.
pub const CANDIDATES_TTL: Duration = Duration::from_secs(10 * 60);

/// Refresh an unchanged entry after this long. Until STUDIO-2169 this was five
/// minutes, which re-recalled every unchanged candidate twelve times an hour
/// from both banks — about four recalls a minute on a ten-candidate board, for
/// facts that had not changed.
pub const PREFETCH_REFRESH_AGE: Duration = Duration::from_secs(30 * 60);

/// How many actual bank recalls one cycle may make, counting personal and shared
/// calls. Fresh cache entries spend nothing; missing/aging candidates past this
/// budget are picked up next cycle in rotating candidate order.
const MAX_PER_CYCLE: usize = 10;

/// The most entries the cache will hold.
///
/// A cycle recalls at most [`MAX_PER_CYCLE`] candidates but CARRIES the entries
/// of every other live candidate ([`PrefetchCache::sync`]), so the cache does
/// grow past one cycle's window — which is exactly what makes a rotating window
/// worth having, and exactly why this bound is a real ceiling rather than a
/// backstop. Comfortably above `MAX_PER_CYCLE`, so several cycles of rotation
/// accumulate before anything is dropped.
pub const MAX_CACHE_ENTRIES: usize = 64;

/// The most fact content, in bytes, the whole cache will hold. Every cached byte
/// is a byte that may reach a turn-1 prompt, so the cache carries the same kind
/// of hard ceiling `MAX_SECTION_BYTES` puts on the rendered section — enough for
/// [`MAX_CACHE_ENTRIES`] entries at a full section each, and no more.
pub const MAX_CACHE_BYTES: usize = 64 * 1024;

/// What a cached recall was recalled FROM: the ticket fields its query is built
/// of ([`recall_query`]: title and labels; the identifier is the key) plus the
/// ticket's state (STUDIO-2169).
///
/// An entry whose stamp no longer matches the candidate is refreshed on the
/// next cycle whatever its age, which is what lets an unchanged ticket keep its
/// entry for [`PREFETCH_REFRESH_AGE`] instead of five minutes. The strings are
/// kept rather than hashed so two different tickets can never compare equal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TicketStamp {
    title: String,
    labels: Vec<String>,
    state: String,
}

impl TicketStamp {
    pub(crate) fn of(iss: &Issue) -> Self {
        Self {
            title: iss.title.clone(),
            labels: iss.labels.clone().unwrap_or_default(),
            state: iss.state.clone(),
        }
    }
}

/// Which bank a failure backs off: an identity's own bank or the shared team
/// bank. Two variants rather than one string so an identity named like the team
/// bank cannot share its back-off.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum BankId {
    Own(String),
    Team(String),
}

/// One bank's failure back-off (STUDIO-2169): how many recalls in a row failed
/// and when the next may be tried.
#[derive(Debug, Clone, Copy)]
struct BankGate {
    failures: u32,
    retry_at: DateTime<Utc>,
}

/// The wait after `failures` consecutive failures: the cadence doubled per
/// failure, capped at `max_backoff_ms` (but never below one cadence). The FIRST
/// failure already waits two cadences, so a failed recall is never retried on
/// the very next cycle — the old schedule's 10s/20s/40s steps were all below the
/// 60s cadence and so retried every cycle for the first three failures.
pub(crate) fn backoff_delay(failures: u32, interval: Duration, max_backoff_ms: i64) -> Duration {
    let cap = Duration::from_millis(max_backoff_ms.max(0) as u64).max(interval);
    let factor = 1u32.checked_shl(failures.min(31)).unwrap_or(u32::MAX);
    interval.checked_mul(factor).unwrap_or(cap).min(cap)
}

/// What identifies one prefetched recall: the teammate whose bank was read and
/// the ticket the query was built from.
///
/// Both halves matter. The identity is the bank, so two teammates never see each
/// other's facts; the ticket is the query, so a hit is only a hit for the ticket
/// the facts were actually recalled *for* — which is what makes the cached facts
/// substitutable for the recall dispatch would otherwise have done itself.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PrefetchKey {
    pub identity: String,
    pub ticket: String,
}

impl PrefetchKey {
    pub fn new(identity: impl Into<String>, ticket: impl Into<String>) -> Self {
        Self {
            identity: identity.into(),
            ticket: ticket.into(),
        }
    }
}

/// One cached recall: the identity's own facts, the SHARED team bank's facts
/// recalled for the same ticket (STUDIO-1040), and when they were fetched.
#[derive(Debug, Clone)]
struct Entry {
    facts: Vec<Fact>,
    /// Empty when no `memory.team_bank` is configured, which is what keeps the
    /// team-off cache byte-identical to before this field existed.
    team: Vec<Fact>,
    at: DateTime<Utc>,
    /// The ticket this entry was recalled for (STUDIO-2169). `None` for entries
    /// installed through the stampless [`PrefetchCache::sync`] family, which
    /// then match any ticket and refresh on age alone.
    stamp: Option<TicketStamp>,
    /// Whether the team half is a real answer. `false` when the shared recall
    /// failed or its bank was backing off, so the entry is completed once the
    /// bank answers again instead of serving "no team facts" for a whole TTL.
    team_complete: bool,
    /// Set when this daemon wrote to a bank the entry was recalled from
    /// ([`PrefetchCache::note_written`]): the entry is still served, but the
    /// next cycle re-recalls it.
    dirty: bool,
}

/// Why a cycle should (or should not) spend recalls on one candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Refresh {
    /// Present, unchanged, young and complete: spend nothing.
    Fresh,
    /// Fresh except that the team half never got a real answer: only the
    /// shared bank needs asking.
    TeamOnly,
    /// Missing, changed, aging or dirty: recall both banks.
    Full,
}

/// The shared, bounded prefetch cache — written by the off-loop task, read
/// **non-blockingly** by dispatch.
///
/// Deliberately holds nothing but plain data. It never sees a tracker, a backend
/// or a clock: the writer stamps entries and the reader passes `now` in, which is
/// what lets both sides be tested without a runtime or a wall clock.
#[derive(Debug, Default)]
pub struct PrefetchCache {
    entries: RwLock<HashMap<PrefetchKey, Entry>>,
    /// Per-bank failure back-off (STUDIO-2169). Lives beside the entries
    /// because both describe what the prefetch task knows about the banks, but
    /// only the prefetch task ever reads or writes it — dispatch never does.
    gates: Mutex<HashMap<BankId, BankGate>>,
    /// The writes [`note_written`](Self::note_written) and
    /// [`note_invalidated`](Self::note_invalidated) recorded since the current
    /// cycle began. A cycle rebuilds the map from a snapshot and recalls that
    /// were in flight, so [`sync_entries`](Self::sync_entries) re-applies these
    /// under its write lock — a write that lands mid-cycle is never overwritten.
    notes: Mutex<Vec<Note>>,
}

/// A write this daemon made to a bank the cache was recalled from.
#[derive(Debug, Clone)]
enum Note {
    /// A retain or reinstate: `None` is the shared team bank.
    Written(Option<String>),
    /// An invalidation of `fact_id`: `None` is the shared team bank.
    Invalidated(Option<String>, String),
}

/// How many mid-cycle notes are kept for re-application. Writes are a handful
/// per run, so this is never reached in practice; past it a note still applies
/// to the live map at once, and the refresh age bounds the rest.
const MAX_NOTES: usize = 256;

impl Note {
    fn apply(&self, entries: &mut HashMap<PrefetchKey, Entry>) {
        for (key, entry) in entries.iter_mut() {
            match self {
                Note::Written(identity) => {
                    if identity.as_deref().is_none_or(|i| key.identity == i) {
                        entry.dirty = true;
                    }
                }
                Note::Invalidated(Some(i), fact_id) => {
                    if key.identity == *i {
                        entry.facts.retain(|f| f.id != *fact_id);
                        entry.dirty = true;
                    }
                }
                Note::Invalidated(None, fact_id) => {
                    entry.team.retain(|f| f.id != *fact_id);
                    entry.dirty = true;
                }
            }
        }
    }
}

impl PrefetchCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// **The dispatch-path read.** Returns the prefetched facts for
    /// `(identity, ticket)` if — and only if — they are present, fresh, and the
    /// lock was free right now.
    ///
    /// Every other outcome is `None`, and every `None` means the same thing to
    /// the caller: render no memory section. In particular a contended lock is a
    /// miss rather than a wait, which is what makes this callable from
    /// `dispatch_issue` at all. It cannot block, cannot fail, and does no I/O.
    pub fn try_get(&self, identity: &str, ticket: &str, now: DateTime<Utc>) -> Option<Vec<Fact>> {
        // `try_read`, never `read`: the writer holds the write lock for one map
        // swap, but "briefly" is not "never", and the control task may not wait
        // even briefly. A poisoned lock lands here too — as a miss.
        let guard = self.entries.try_read().ok()?;
        let entry = guard.get(&PrefetchKey::new(identity, ticket))?;
        if is_stale(entry.at, now) {
            return None;
        }
        Some(entry.facts.clone())
    }

    /// **The dispatch-path read for the SHARED team bank** (STUDIO-1040).
    ///
    /// The same non-blocking read as [`try_get`](Self::try_get), returning the
    /// shared facts recalled for `(identity, ticket)` instead of the identity's
    /// own. Empty is a miss, exactly as `None` is above; an entry that holds only
    /// own facts (no team bank configured) answers an empty vector rather than
    /// `None`, because the team section it feeds is empty either way.
    pub fn try_get_team(
        &self,
        identity: &str,
        ticket: &str,
        now: DateTime<Utc>,
    ) -> Option<Vec<Fact>> {
        let guard = self.entries.try_read().ok()?;
        let entry = guard.get(&PrefetchKey::new(identity, ticket))?;
        if is_stale(entry.at, now) {
            return None;
        }
        Some(entry.team.clone())
    }

    /// Off-loop freshness check for BOTH banks under this key, including empty
    /// successful answers. No lock or fact copy survives into a remote recall.
    ///
    /// An unchanged ticket stays [`Refresh::Fresh`] for [`PREFETCH_REFRESH_AGE`];
    /// a changed one ([`TicketStamp`]) or one this daemon has written to the bank
    /// behind is [`Refresh::Full`] at once (STUDIO-2169).
    fn needs_refresh(&self, key: &PrefetchKey, stamp: &TicketStamp, now: DateTime<Utc>) -> Refresh {
        let Ok(guard) = self.entries.read() else {
            return Refresh::Full;
        };
        let Some(entry) = guard.get(key) else {
            return Refresh::Full;
        };
        if entry.dirty || entry.stamp.as_ref().is_some_and(|s| s != stamp) {
            return Refresh::Full;
        }
        let aging = match now.signed_duration_since(entry.at).to_std() {
            Ok(age) => age >= PREFETCH_REFRESH_AGE,
            // Like the TTL check, a backwards clock keeps existing entries fresh.
            Err(_) => false,
        };
        if aging {
            Refresh::Full
        } else if !entry.team_complete {
            Refresh::TeamOnly
        } else {
            Refresh::Fresh
        }
    }

    /// The cached entry under `key`, if any — the off-loop half's read of what a
    /// partial refresh keeps.
    fn entry(&self, key: &PrefetchKey) -> Option<Entry> {
        self.entries.read().ok()?.get(key).cloned()
    }

    /// Whether `bank` may be asked at `now`, i.e. is not backing off.
    fn bank_ready(&self, bank: &BankId, now: DateTime<Utc>) -> bool {
        let Ok(gates) = self.gates.lock() else {
            // A poisoned gate map gates nothing: the cadence still bounds calls.
            return true;
        };
        gates.get(bank).is_none_or(|g| now >= g.retry_at)
    }

    /// Records a failed recall against `bank` and pushes its next attempt out
    /// by [`backoff_delay`]. Returns the consecutive-failure count, for the log.
    fn bank_failed(
        &self,
        bank: BankId,
        now: DateTime<Utc>,
        interval: Duration,
        max_ms: i64,
    ) -> u32 {
        let Ok(mut gates) = self.gates.lock() else {
            return 0;
        };
        let failures = gates.get(&bank).map_or(0, |g| g.failures).saturating_add(1);
        let wait = chrono::Duration::from_std(backoff_delay(failures, interval, max_ms))
            .unwrap_or(chrono::Duration::MAX);
        let retry_at = now
            .checked_add_signed(wait)
            .unwrap_or(DateTime::<Utc>::MAX_UTC);
        gates.insert(bank, BankGate { failures, retry_at });
        failures
    }

    /// Clears `bank`'s back-off after a recall it answered.
    fn bank_answered(&self, bank: &BankId) {
        if let Ok(mut gates) = self.gates.lock() {
            gates.remove(bank);
        }
    }

    /// **This daemon wrote to a bank** (a retain or a reinstate through
    /// `TeamsMemory`, STUDIO-2169): every entry recalled from it is re-recalled
    /// on the next cycle, but keeps being served until then — exactly what a
    /// dispatch would have seen before the write.
    ///
    /// `identity` names an identity's own bank; `None` names the shared team
    /// bank, which every entry's team half was recalled from.
    pub fn note_written(&self, identity: Option<&str>) {
        self.note(Note::Written(identity.map(str::to_string)));
    }

    /// **This daemon invalidated a record** (STUDIO-2169): the record is removed
    /// from every cached entry at once — a correction must not wait out a TTL —
    /// and the affected entries are re-recalled on the next cycle.
    ///
    /// `identity` names an identity's own bank; `None` names the shared team
    /// bank, so the record is removed from every entry's team half.
    pub fn note_invalidated(&self, identity: Option<&str>, fact_id: &str) {
        self.note(Note::Invalidated(
            identity.map(str::to_string),
            fact_id.to_string(),
        ));
    }

    /// Applies `note` to the live map now, and logs it for the running cycle's
    /// [`sync_entries`](Self::sync_entries).
    fn note(&self, note: Note) {
        if let Ok(mut notes) = self.notes.lock()
            && notes.len() < MAX_NOTES
        {
            notes.push(note.clone());
        }
        if let Ok(mut guard) = self.entries.write() {
            note.apply(&mut guard);
        }
    }

    /// Starts a prefetch cycle: notes from before it are already in the map
    /// (and in whatever this cycle snapshots), so only later ones need
    /// re-applying.
    fn begin_cycle(&self) {
        if let Ok(mut notes) = self.notes.lock() {
            notes.clear();
        }
    }

    /// Installs this cycle's `fresh` results and **evicts everything whose ticket
    /// is no longer a live candidate**.
    ///
    /// The eviction rule is `record_issue_states`'s (§0.11.3): being absent from
    /// the candidate set is meaningful, so a map that only ever grew would
    /// quietly hand a dispatch facts recalled for a ticket nobody is working any
    /// more. `live` — not `fresh` — is what defines "still a candidate", and the
    /// distinction is load-bearing: a cycle recalls at most [`MAX_PER_CYCLE`]
    /// candidates but *sees* all of them, so keying eviction on `fresh` would
    /// evict the entries for every candidate this cycle did not get to and the
    /// eleventh ticket on a busy board would never hold a cached recall at all.
    /// Carrying them lets [`prefetch_cycle`]'s rotating window cover the whole
    /// set across cycles, which is what makes "nothing is skipped, only spread"
    /// true rather than merely intended.
    ///
    /// A carried entry keeps its ORIGINAL timestamp, so carrying can extend
    /// nothing past [`PREFETCH_TTL`]; a stale one is dropped here as well as
    /// refused by [`try_get`](Self::try_get).
    ///
    /// The new map is built before the lock is taken, so the write lock is held
    /// for one move and a concurrent `try_get` is essentially never turned away.
    pub fn sync(
        &self,
        fresh: Vec<(PrefetchKey, Vec<Fact>)>,
        live: &HashSet<PrefetchKey>,
        now: DateTime<Utc>,
    ) {
        self.sync_both(
            fresh
                .into_iter()
                .map(|(key, facts)| (key, facts, Vec::new()))
                .collect(),
            live,
            now,
        );
    }

    /// [`sync`](Self::sync) carrying the SHARED team bank's facts beside the
    /// identity's own (STUDIO-1040). One entry per `(identity, ticket)` holds
    /// both, so the team facts expire, carry and evict on exactly the own facts'
    /// schedule — there is no second TTL to get wrong.
    pub fn sync_both(
        &self,
        fresh: Vec<(PrefetchKey, Vec<Fact>, Vec<Fact>)>,
        live: &HashSet<PrefetchKey>,
        now: DateTime<Utc>,
    ) {
        // A caller-built install is the whole answer as of now: no earlier
        // write is pending against it.
        self.begin_cycle();
        self.sync_entries(
            fresh
                .into_iter()
                .map(|(key, facts, team)| {
                    (
                        key,
                        Entry {
                            facts,
                            team,
                            at: now,
                            stamp: None,
                            team_complete: true,
                            dirty: false,
                        },
                    )
                })
                .collect(),
            live,
            now,
        );
    }

    /// The one writer every `sync` funnels into: `fresh` entries arrive fully
    /// built (the cycle stamps them), everything else live is carried.
    fn sync_entries(
        &self,
        fresh: Vec<(PrefetchKey, Entry)>,
        live: &HashSet<PrefetchKey>,
        now: DateTime<Utc>,
    ) {
        // Snapshot what survives under the read lock, then release it: the whole
        // rebuild happens outside any lock.
        let mut carried: Vec<(PrefetchKey, Entry)> = match self.entries.read() {
            Ok(guard) => guard
                .iter()
                .filter(|(k, e)| live.contains(*k) && !is_stale(e.at, now))
                .map(|(k, e)| (k.clone(), e.clone()))
                .collect(),
            // A poisoned lock carries nothing; the next cycle refills.
            Err(_) => Vec::new(),
        };
        // Newest first, so which carried entries a bound drops is deterministic
        // rather than whatever order the map happened to hash them into.
        carried.sort_by(|a, b| {
            b.1.at
                .cmp(&a.1.at)
                .then_with(|| a.0.ticket.cmp(&b.0.ticket))
        });

        let mut next: HashMap<PrefetchKey, Entry> = HashMap::new();
        let mut bytes = 0usize;
        // This cycle's answers first: they are the freshest, and a fresh entry
        // must win over a carried copy of the same key.
        for (key, entry) in fresh {
            insert_bounded(&mut next, &mut bytes, key, entry);
        }
        for (key, entry) in carried {
            if !next.contains_key(&key) {
                insert_bounded(&mut next, &mut bytes, key, entry);
            }
        }
        if let Ok(mut guard) = self.entries.write() {
            // Writes noted since the cycle began are re-applied while the write
            // lock is held, so none is lost to the swap; a later one applies to
            // the map this installs.
            if let Ok(mut notes) = self.notes.lock() {
                for note in notes.drain(..) {
                    note.apply(&mut next);
                }
            }
            *guard = next;
        }
        // A poisoned write lock leaves the previous contents in place, which the
        // TTL will retire on its own. There is nothing better to do here and
        // nothing worth failing a background cycle over.
    }

    /// [`sync`](Self::sync) where `fresh` IS the whole live set — "these entries
    /// and no others". The shape the dispatch-path tests want, and the shape a
    /// cycle that routed nothing uses to clear the cache.
    pub fn replace(&self, fresh: Vec<(PrefetchKey, Vec<Fact>)>, now: DateTime<Utc>) {
        let live: HashSet<PrefetchKey> = fresh.iter().map(|(k, _)| k.clone()).collect();
        self.sync(fresh, &live, now);
    }

    /// [`replace`](Self::replace) carrying shared team facts too (STUDIO-1040).
    pub fn replace_both(
        &self,
        fresh: Vec<(PrefetchKey, Vec<Fact>, Vec<Fact>)>,
        now: DateTime<Utc>,
    ) {
        let live: HashSet<PrefetchKey> = fresh.iter().map(|(k, _, _)| k.clone()).collect();
        self.sync_both(fresh, &live, now);
    }

    /// Whether `(identity, ticket)`'s entry is marked for re-recall.
    #[cfg(test)]
    pub(crate) fn is_dirty(&self, identity: &str, ticket: &str) -> bool {
        self.entries
            .read()
            .ok()
            .and_then(|g| g.get(&PrefetchKey::new(identity, ticket)).map(|e| e.dirty))
            .unwrap_or(false)
    }

    /// How many entries are cached. Test/observability only — never consulted by
    /// dispatch, which asks about exactly one key.
    pub fn len(&self) -> usize {
        self.entries.read().map(|g| g.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Whether an entry fetched at `at` is too old to serve at `now`.
///
/// A future timestamp (a clock step back over a cycle boundary) counts as fresh:
/// the entry is at most one cycle old in wall-clock terms, and treating a
/// backwards clock as "everything is stale" would empty the cache for the whole
/// interval it takes to catch up.
fn is_stale(at: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    is_older_than(at, now, PREFETCH_TTL)
}

/// [`is_stale`] against an explicit bound.
fn is_older_than(at: DateTime<Utc>, now: DateTime<Utc>, ttl: Duration) -> bool {
    match now.signed_duration_since(at).to_std() {
        Ok(age) => age > ttl,
        Err(_) => false,
    }
}

/// Inserts one entry if it fits [`MAX_CACHE_ENTRIES`] and [`MAX_CACHE_BYTES`].
///
/// Callers offer entries best-first (this cycle's results, then carried ones
/// newest-first), so what a bound drops is the least useful tail rather than an
/// arbitrary hash-map slice. An entry whose facts would breach the byte ceiling
/// is skipped **whole** rather than truncated — half a fact set is a prompt
/// nobody can reason about — and a later, smaller entry may still fit.
fn insert_bounded(
    out: &mut HashMap<PrefetchKey, Entry>,
    bytes: &mut usize,
    key: PrefetchKey,
    entry: Entry,
) {
    if out.len() >= MAX_CACHE_ENTRIES {
        return;
    }
    let cost: usize = entry
        .facts
        .iter()
        .chain(entry.team.iter())
        .map(|f| f.content.len())
        .sum();
    if *bytes + cost > MAX_CACHE_BYTES {
        return;
    }
    *bytes += cost;
    out.insert(key, entry);
}

/// Candidates already fetched by the poll loop, in first-project-wins order.
/// Carries no tracker handle: prefetch must never spend additional Linear quota.
#[derive(Clone)]
pub struct PrefetchTarget {
    pub issues: Vec<Issue>,
    /// Start of the candidate fetch, not publication time, so slow polls cannot
    /// make old candidates look newly fetched.
    pub fetched_at: DateTime<Utc>,
    /// The slug of the project each entry of `issues` arrived through, positionally
    /// (STUDIO-1896), so Teams triage can reuse this set — writing back through the
    /// owning project's client — instead of fetching every project's candidates again.
    /// Empty when the publisher cannot name them (the legacy single-tracker path);
    /// triage then fetches for itself.
    pub owner_slugs: Vec<String>,
}

/// Everything [`run_prefetch_schedule`] runs against. The absence of an
/// `Orchestrator`, a control channel and a store is the off-loop guarantee, in
/// the type.
pub struct PrefetchDeps<TF> {
    /// The boot-loaded `teams.yaml`. Teams config is not hot-reloaded (out of
    /// scope for this slice, as for T3b), so this is captured once at the
    /// composition root.
    pub teams: Arc<Teams>,
    /// Yields the last complete poll's candidates, or `None` before one is published.
    pub target: TF,
    /// The remote bank. A `dyn MemoryBackend` rather than a concrete
    /// `HindsightBackend` **because this is the off-loop side**: nothing here
    /// runs on the control task, so the argument that forces
    /// `Orchestrator::teams_bank` to be concrete does not apply — and the seam is
    /// what lets a test prefetch from a stub, or from a failing backend, without
    /// a socket.
    pub backend: Arc<dyn MemoryBackend>,
    /// The cache dispatch reads. The one thing shared with the control task, and
    /// shared as data behind a lock the control task never waits on.
    pub cache: Arc<PrefetchCache>,
    /// The cadence between cycles; [`PREFETCH_INTERVAL`] in production,
    /// milliseconds in tests.
    pub interval: Duration,
    /// The back-off ceiling; [`MAX_PREFETCH_BACKOFF_MS`] in production.
    pub max_backoff_ms: i64,
    /// The clock entries are stamped with. Injected for the same reason
    /// `Orchestrator::now` is: a TTL test that had to sleep would be a slow test
    /// that still could not pin the boundary.
    pub now: Box<dyn Fn() -> DateTime<Utc> + Send + Sync>,
}

/// What one cycle did — the input to the back-off decision, and the assertion
/// surface for the degradation tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CycleOutcome {
    /// Nothing to do: absent/stale candidates, or no candidate routed anywhere.
    Idle,
    /// This many `(identity, ticket)` pairs were cached.
    Prefetched(usize),
    /// The bank failed or timed out before anything was recalled. Back off;
    /// dispatch degrades to `none`.
    BankFailure,
    /// Something was recalled and cached, then a bank failed. Only that bank
    /// backs off (STUDIO-2169): the service answered this cycle, so the whole
    /// schedule does not.
    Partial,
}

impl CycleOutcome {
    fn is_failure(self) -> bool {
        matches!(self, CycleOutcome::BankFailure)
    }
}

/// Whether the prefetch task should exist at all: **only** with Teams enabled,
/// `memory.backend: hindsight`, and a roster to route to.
///
/// `local`, `none` and Teams-off spawn nothing — not a task that returns early,
/// nothing — which is what makes "byte-identical everything" true of those
/// configurations rather than merely likely. An empty roster is included here
/// rather than left to the cycle because [`route`] answers `None` for every
/// candidate without one, so every cycle would be a no-op.
pub fn prefetch_enabled(teams: &Teams) -> bool {
    teams.enabled && teams.memory.backend == BackendKind::Hindsight && !teams.roster.is_empty()
}

/// Runs the prefetch pass on its own cadence until `ctx` is cancelled.
///
/// The first thing it does is **wait**: a cycle at t=0 would race the daemon's
/// first candidate publication and find none. Cancellation is checked on both
/// sides of the sleep, so a shutdown never waits out a cycle.
pub async fn run_prefetch_schedule<TF>(mut ctx: CancelWait, deps: PrefetchDeps<TF>)
where
    TF: Fn() -> Option<PrefetchTarget>,
{
    // Defence in depth: the composition root already gates the spawn, so this can
    // only fire for a caller that built the task by hand. Answering here means no
    // configuration reaches a remote bank through a back door.
    if !prefetch_enabled(&deps.teams) {
        return;
    }
    tracing::info!(
        roster = deps.teams.roster.len(),
        interval_ms = deps.interval.as_millis() as u64,
        "teams memory prefetch task started (off-loop; dispatch never waits on it)"
    );
    let mut failures: u32 = 0;
    // Where the next cycle's window starts (see `prefetch_cycle`). Owned by the
    // schedule so a cycle stays a pure function of its inputs plus this one
    // number, which is also what lets a test drive a cycle at a chosen offset.
    let mut cursor: usize = 0;
    loop {
        // Back off LONGER than the normal cadence: the first failed cycle
        // already waits two cadences (STUDIO-2169). The old 10s-based steps sat
        // under the 60s cadence for three failures, so a down bank was retried
        // every cycle while the log said "backing off".
        let delay = if failures > 0 {
            backoff_delay(failures, deps.interval, deps.max_backoff_ms)
        } else {
            deps.interval
        };
        tokio::select! {
            _ = ctx.cancelled() => return,
            _ = tokio::time::sleep(delay) => {}
        }
        if ctx.is_cancelled() {
            return;
        }
        let outcome = prefetch_cycle(&ctx, &deps, &mut cursor).await;
        if outcome.is_failure() {
            failures = failures.saturating_add(1);
            // LOUD, because the symptom otherwise is a team that silently stops
            // remembering: dispatch degrades to exactly what `backend: none`
            // gives, which looks like nothing at all going wrong.
            tracing::warn!(
                consecutive_failures = failures,
                "teams memory prefetch cycle failed; backing off. Dispatch continues with NO \
                 memory section until the bank answers again (the tailnet or the service is \
                 likely down)"
            );
        } else {
            failures = 0;
        }
    }
}

/// One prefetch pass: read published candidates, route each with the **existing pure
/// [`route`]**, recall from the remote bank for a rotating window of them, and
/// sync the cache to the live candidate set.
///
/// Routing here is the same function dispatch will run, on the same ticket, so a
/// hit is a hit for the identity that actually takes the ticket. The one input it
/// cannot have is live load: this task holds no `Orchestrator` and so no
/// `running` map, and [`LoadSnapshot`] only ever breaks *ties* in the
/// label-overlap tier. A tie broken differently here is a cache miss at dispatch
/// — no memory section, the safe degradation — never a wrong teammate's facts,
/// because the key carries the identity the facts were recalled for.
/// `cursor` is where this cycle's window starts, and is advanced past what it
/// covered. The window examines at most one traversal and spends at most
/// [`MAX_PER_CYCLE`] actual bank calls. Fresh entries are skipped without spending
/// that budget, and the cache carries entries the window did not refresh
/// ([`PrefetchCache::sync`]).
pub(crate) async fn prefetch_cycle<TF>(
    ctx: &CancelWait,
    deps: &PrefetchDeps<TF>,
    cursor: &mut usize,
) -> CycleOutcome
where
    TF: Fn() -> Option<PrefetchTarget>,
{
    deps.cache.begin_cycle();
    let Some(target) = (deps.target)() else {
        return CycleOutcome::Idle;
    };
    let now = (deps.now)();
    if is_older_than(target.fetched_at, now, CANDIDATES_TTL) {
        return CycleOutcome::Idle;
    }
    // Poll order is first-project-wins. Preserve that for injected publications too.
    let mut seen = HashSet::new();
    let issues: Vec<Issue> = target
        .issues
        .into_iter()
        .filter(|i| seen.insert(i.id.clone()))
        .collect();
    let routed = routed_candidates(&deps.teams, &issues);
    if routed.is_empty() {
        // Nothing routes anywhere: clear the cache rather than leaving
        // yesterday's entries to age out. Being absent from the candidate set is
        // meaningful (§0.11.3).
        deps.cache.replace(Vec::new(), now);
        *cursor = 0;
        return CycleOutcome::Idle;
    }
    // Every routed candidate is still LIVE and keeps whatever the cache holds
    // for it, whether or not this cycle's window reaches it. Only tickets that
    // have left the candidate set are evicted.
    let live: HashSet<PrefetchKey> = routed
        .iter()
        .map(|(identity, iss)| PrefetchKey::new(identity, &iss.identifier))
        .collect();
    // The rotating window. `routed` is in candidate order, which is stable
    // across cycles, so advancing the cursor is what makes coverage fair.
    let start = *cursor % routed.len();
    let team_bank = deps
        .teams
        .memory
        .team_bank_enabled()
        .then(|| BankId::Team(deps.teams.memory.team_bank.clone()));
    let mut calls = 0;
    let mut fresh: Vec<(PrefetchKey, Entry)> = Vec::new();
    for offset in 0..routed.len() {
        // A shutdown must not have to wait out a whole cycle of remote recalls.
        if ctx.is_cancelled() {
            break;
        }
        let (identity, iss) = &routed[(start + offset) % routed.len()];
        let key = PrefetchKey::new(identity, &iss.identifier);
        let stamp = TicketStamp::of(iss);
        let refresh = deps.cache.needs_refresh(&key, &stamp, now);
        let own_bank = BankId::Own(identity.clone());
        let team_ready = team_bank
            .as_ref()
            .is_some_and(|b| deps.cache.bank_ready(b, now));
        // What this candidate would spend, and whether its banks will take it.
        // A backing-off bank is SKIPPED, not retried (STUDIO-2169): its
        // candidates keep whatever the cache already holds for them.
        let cost = match refresh {
            Refresh::Fresh => None,
            Refresh::TeamOnly => team_ready.then_some(1),
            Refresh::Full => deps
                .cache
                .bank_ready(&own_bank, now)
                .then_some(1 + usize::from(team_ready)),
        };
        let Some(cost) = cost else {
            *cursor = start + offset + 1;
            continue;
        };
        if calls + cost > MAX_PER_CYCLE {
            break;
        }
        *cursor = start + offset + 1;
        // Reserve both calls before starting so a shared recall cannot overspend.
        calls += cost;
        let previous = deps.cache.entry(&key);
        let own = if refresh == Refresh::TeamOnly {
            // Only the shared half was missing: keep the own facts AND their
            // timestamp, so a completed team half extends nothing.
            previous.as_ref().map(|e| (e.facts.clone(), e.at))
        } else {
            None
        };
        let (facts, at) = match own {
            Some(kept) => kept,
            None => {
                let q = recall_query(&deps.teams, iss);
                match deps.backend.recall(identity, &q).await {
                    Ok(recalled) => {
                        deps.cache.bank_answered(&own_bank);
                        // "A record that could not be read is skipped LOUDLY, never
                        // fatal" — the same contract `local` has, and this is the
                        // caller that owns the log.
                        for (what, why) in &recalled.skipped {
                            tracing::warn!(
                                identity = %identity,
                                item = %what,
                                reason = %why,
                                "teams memory prefetch: skipping an unusable bank record"
                            );
                        }
                        (recalled.facts, now)
                    }
                    Err(e) => {
                        // Stop the cycle rather than walking the rest of the backlog
                        // into the same failure: whatever broke is almost certainly
                        // still broken, and burning every candidate against it is
                        // the hot loop. What was already recalled is still worth
                        // caching, and this bank sits out its back-off (STUDIO-2169).
                        let failures = deps.cache.bank_failed(
                            own_bank,
                            now,
                            deps.interval,
                            deps.max_backoff_ms,
                        );
                        tracing::warn!(
                            identity = %identity,
                            ticket = %iss.identifier,
                            error = %e,
                            consecutive_failures = failures,
                            "teams memory prefetch: the remote bank failed; keeping what this \
                             cycle already recalled and backing off"
                        );
                        let any = !fresh.is_empty();
                        deps.cache.sync_entries(fresh, &live, now);
                        return if any {
                            CycleOutcome::Partial
                        } else {
                            CycleOutcome::BankFailure
                        };
                    }
                }
            }
        };
        let (team, team_complete) = match team_bank.as_ref() {
            None => (Vec::new(), true),
            Some(bank) if team_ready => match prefetch_team_facts(deps, iss).await {
                Some(team) => {
                    deps.cache.bank_answered(bank);
                    (team, true)
                }
                None => {
                    deps.cache
                        .bank_failed(bank.clone(), now, deps.interval, deps.max_backoff_ms);
                    (carried_team(previous.as_ref(), &stamp), false)
                }
            },
            Some(_) => (carried_team(previous.as_ref(), &stamp), false),
        };
        fresh.push((
            key,
            Entry {
                facts,
                team,
                at,
                stamp: Some(stamp),
                team_complete,
                dirty: false,
            },
        ));
    }
    let n = fresh.len();
    deps.cache.sync_entries(fresh, &live, now);
    if n == 0 {
        CycleOutcome::Idle
    } else {
        CycleOutcome::Prefetched(n)
    }
}

/// The team facts a candidate keeps when its shared recall could not be made:
/// the previous entry's, if it was recalled for this same ticket, else none.
fn carried_team(previous: Option<&Entry>, stamp: &TicketStamp) -> Vec<Fact> {
    previous
        .filter(|e| e.stamp.as_ref().is_none_or(|s| s == stamp))
        .map(|e| e.team.clone())
        .unwrap_or_default()
}

/// Recalls the SHARED team bank for one candidate (STUDIO-1040), bounded by
/// `team_recall_top_k`.
///
/// **Best-effort and isolated from the identity's recall**: a team bank that
/// fails degrades the team section to empty and the cycle continues, because one
/// shared bank being down must not cost every candidate its OWN memory too. The
/// shared recall is the same ticket-scoped query the identity's own is (see
/// [`team_recall_query`]), so the two are ranked against the same terms.
async fn prefetch_team_facts<TF>(deps: &PrefetchDeps<TF>, iss: &Issue) -> Option<Vec<Fact>> {
    let teams = &deps.teams;
    if !teams.memory.team_bank_enabled() {
        return Some(Vec::new());
    }
    let bank = teams.memory.team_bank.as_str();
    let q = team_recall_query(teams, iss);
    match deps.backend.recall_shared(bank, &q).await {
        Ok(recalled) => {
            for (what, why) in &recalled.skipped {
                tracing::warn!(
                    bank = %bank,
                    item = %what,
                    reason = %why,
                    "teams memory prefetch: skipping an unusable team-bank record"
                );
            }
            Some(recalled.facts)
        }
        Err(e) => {
            tracing::warn!(
                bank = %bank,
                ticket = %iss.identifier,
                error = %e,
                "teams memory prefetch: the shared team bank failed; backing it off and keeping \
                 this ticket's previous team facts, if any (the identity's own recall is \
                 unaffected)"
            );
            None
        }
    }
}

/// Every candidate that routes to somebody, paired with that identity.
///
/// Uses the **existing pure [`route`]** — not a copy of its rules — so the
/// prefetch and the dispatch cannot drift apart. A ticket that routes nowhere
/// (`manager.mode: off` with no default, an unmatched ticket with no default) is
/// dropped here: there is no bank to read on its behalf.
fn routed_candidates<'a>(teams: &Teams, issues: &'a [Issue]) -> Vec<(String, &'a Issue)> {
    let load = LoadSnapshot::default();
    issues
        .iter()
        .filter(|i| !i.identifier.is_empty())
        .filter_map(|i| route(teams, i, &load).identity.map(|name| (name, i)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use rhapsody_config::memory::{MemoryError, Query, Recalled, Record};
    use rhapsody_config::teams::{Identity, Manager, ManagerMode, Memory, Teams};
    use rhapsody_tracker::fake::Fake;
    use std::sync::Mutex;

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_756_000_000 + secs, 0).expect("timestamp")
    }

    fn fact(id: &str, content: &str) -> Fact {
        Fact {
            id: id.to_string(),
            identity: "alice".to_string(),
            ticket: "MT-1".to_string(),
            content: content.to_string(),
            ..Fact::default()
        }
    }

    fn issue(id: &str, key: &str, labels: &[&str]) -> Issue {
        Issue {
            id: id.to_string(),
            identifier: key.to_string(),
            title: format!("work on {key}"),
            labels: Some(labels.iter().map(|s| s.to_string()).collect()),
            team_id: "team-1".to_string(),
            ..Issue::default()
        }
    }

    fn teams(enabled: bool, backend: BackendKind) -> Teams {
        Teams {
            enabled,
            manager: Manager {
                mode: ManagerMode::Labels,
                ..Manager::default()
            },
            memory: Memory {
                backend,
                ..Memory::default()
            },
            roster: vec![
                Identity {
                    name: "alice".to_string(),
                    labels: vec!["rust".to_string()],
                    ..Identity::default()
                },
                Identity {
                    name: "bob".to_string(),
                    labels: vec!["web".to_string()],
                    ..Identity::default()
                },
            ],
            ..Teams::disabled()
        }
    }

    /// A backend that answers from a canned map and records what it was asked.
    #[derive(Default)]
    struct FakeBank {
        facts: HashMap<String, Vec<Fact>>,
        /// The facts every shared-bank recall answers with (STUDIO-1040).
        team_facts: Vec<Fact>,
        fail: bool,
        /// Fails only the shared team bank (STUDIO-2169's per-bank back-off).
        fail_shared: bool,
        asked: Mutex<Vec<(String, String)>>,
        /// `(bank, ticket, top_k)` for each shared-bank recall. The cap is
        /// recorded, not just the target, so a test can pin that the request
        /// honours `memory.team_recall_top_k` rather than the identity's own
        /// `recall_top_k`.
        asked_shared: Mutex<Vec<(String, String, usize)>>,
    }

    #[async_trait]
    impl MemoryBackend for FakeBank {
        async fn retain(&self, _rec: &Record) -> Result<String, MemoryError> {
            Ok(String::new())
        }

        async fn recall(&self, identity: &str, q: &Query) -> Result<Recalled, MemoryError> {
            self.asked
                .lock()
                .expect("asked")
                .push((identity.to_string(), q.ticket.clone()));
            if self.fail {
                return Err(MemoryError::Io("the tailnet is down".to_string()));
            }
            Ok(Recalled {
                facts: self.facts.get(identity).cloned().unwrap_or_default(),
                skipped: Vec::new(),
            })
        }

        async fn recall_shared(&self, bank: &str, q: &Query) -> Result<Recalled, MemoryError> {
            self.asked_shared.lock().expect("asked_shared").push((
                bank.to_string(),
                q.ticket.clone(),
                q.top_k,
            ));
            if self.fail || self.fail_shared {
                return Err(MemoryError::Io("the tailnet is down".to_string()));
            }
            Ok(Recalled {
                facts: self.team_facts.clone(),
                skipped: Vec::new(),
            })
        }

        async fn invalidate(
            &self,
            _identity: &str,
            _fact_id: &str,
            _reason: &str,
        ) -> Result<bool, MemoryError> {
            Ok(false)
        }

        async fn revalidate(&self, _identity: &str, _fact_id: &str) -> Result<bool, MemoryError> {
            Ok(false)
        }
    }

    /// The shared programmable tracker (`rhapsody_tracker::fake::Fake`), which
    /// every other off-loop task's tests already drive.
    fn tracker(issues: Vec<Issue>) -> Arc<Fake> {
        let mut f = Fake::new();
        f.candidates = issues;
        Arc::new(f)
    }

    fn deps(
        t: Teams,
        bank: Arc<FakeBank>,
        tracker: Arc<Fake>,
        cache: Arc<PrefetchCache>,
        now: i64,
    ) -> PrefetchDeps<impl Fn() -> Option<PrefetchTarget>> {
        let o = crate::Orchestrator::new("WORKFLOW.md");
        o.set_reads_target(tracker.clone(), "");
        o.set_reads_triage_snapshot(crate::reads::TriageSnapshot {
            trackers: vec![crate::reads::ProjectTracker {
                slug: "work".into(),
                tracker: tracker.clone(),
            }],
            ..Default::default()
        });
        o.set_reads_candidates(PrefetchTarget {
            issues: tracker.candidates.clone(),
            fetched_at: at(now),
            owner_slugs: Vec::new(),
        });
        let handle = o.control();
        PrefetchDeps {
            teams: Arc::new(t),
            target: move || handle.reads_candidates(),
            backend: bank as Arc<dyn MemoryBackend>,
            cache,
            interval: Duration::from_millis(5),
            max_backoff_ms: 20,
            now: Box::new(move || at(now)),
        }
    }

    fn bank_with(entries: &[(&str, Vec<Fact>)]) -> Arc<FakeBank> {
        Arc::new(FakeBank {
            facts: entries
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
            ..FakeBank::default()
        })
    }

    // ── the spawn gate ──────────────────────────────────────────────────────────────────────────

    /// The acceptance criterion for "byte-identical everything" on the other
    /// backends: `local`, `none` and Teams-off spawn NOTHING, so there is no task
    /// that could have a behaviour delta.
    #[test]
    fn only_hindsight_with_a_roster_spawns_a_task() {
        assert!(prefetch_enabled(&teams(true, BackendKind::Hindsight)));
        assert!(!prefetch_enabled(&teams(true, BackendKind::Local)));
        assert!(!prefetch_enabled(&teams(true, BackendKind::None)));
        assert!(!prefetch_enabled(&teams(false, BackendKind::Hindsight)));
        let mut empty = teams(true, BackendKind::Hindsight);
        empty.roster.clear();
        assert!(!prefetch_enabled(&empty), "nobody to route to");
    }

    /// Defence in depth: a task built by hand against a non-hindsight config
    /// returns immediately without touching the tracker or the bank.
    #[tokio::test]
    async fn a_hand_built_task_on_the_wrong_backend_returns_at_once() {
        let bank = bank_with(&[]);
        let tracker = tracker(vec![issue("1", "MT-1", &["rust"])]);
        let cache = Arc::new(PrefetchCache::new());
        let d = deps(
            teams(true, BackendKind::Local),
            Arc::clone(&bank),
            tracker,
            Arc::clone(&cache),
            0,
        );
        run_prefetch_schedule(CancelWait::default(), d).await;
        assert!(bank.asked.lock().expect("asked").is_empty());
        assert!(cache.is_empty());
    }

    // ── the cycle ───────────────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn fresh_cache_entries_are_not_re_recalled() {
        // Successful empty answers are fresh too, not misses to retry each minute.
        for with_facts in [false, true] {
            let mut cfg = teams(true, BackendKind::Hindsight);
            cfg.memory.team_bank = "team".into();
            let bank = Arc::new(FakeBank {
                facts: if with_facts {
                    [("alice".into(), vec![fact("own", "personal note")])]
                        .into_iter()
                        .collect()
                } else {
                    HashMap::new()
                },
                team_facts: if with_facts {
                    vec![fact("shared", "team note")]
                } else {
                    Vec::new()
                },
                ..FakeBank::default()
            });
            let cache = Arc::new(PrefetchCache::new());
            let mut d = deps(
                cfg,
                bank.clone(),
                tracker(vec![
                    issue("1", "MT-1", &["rust"]),
                    issue("2", "MT-2", &["web"]),
                ]),
                cache.clone(),
                0,
            );
            let mut cursor = 0;
            assert_eq!(
                prefetch_cycle(&CancelWait::default(), &d, &mut cursor).await,
                CycleOutcome::Prefetched(2)
            );
            bank.asked.lock().unwrap().clear();
            bank.asked_shared.lock().unwrap().clear();
            d.now = Box::new(|| at(60));
            assert_eq!(
                prefetch_cycle(&CancelWait::default(), &d, &mut cursor).await,
                CycleOutcome::Idle
            );
            assert!(
                bank.asked.lock().unwrap().is_empty(),
                "fresh personal cache must cost zero calls"
            );
            assert!(
                bank.asked_shared.lock().unwrap().is_empty(),
                "fresh team cache must cost zero calls"
            );
            assert!(cache.try_get("alice", "MT-1", at(60)).is_some());
            assert!(cache.try_get_team("alice", "MT-1", at(60)).is_some());
            assert!(cache.try_get("bob", "MT-2", at(60)).is_some());
            assert_eq!(
                cache.entries.read().unwrap()[&PrefetchKey::new("alice", "MT-1")].at,
                at(0),
                "skipping must not renew the timestamp"
            );
        }
    }

    #[tokio::test]
    async fn aging_entries_are_refreshed_before_ttl() {
        let mut cfg = teams(true, BackendKind::Hindsight);
        cfg.memory.team_bank = "team".into();
        let bank = Arc::new(FakeBank {
            facts: [("alice".into(), vec![fact("new-own", "personal note")])]
                .into_iter()
                .collect(),
            team_facts: vec![fact("new-team", "team note")],
            ..FakeBank::default()
        });
        let cache = Arc::new(PrefetchCache::new());
        cache.replace_both(
            vec![(
                PrefetchKey::new("alice", "MT-1"),
                vec![fact("old-own", "old")],
                vec![fact("old-team", "old")],
            )],
            at(0),
        );
        let refresh = PREFETCH_REFRESH_AGE.as_secs() as i64;
        // Published at the refresh boundary so the candidate set is still
        // current there; the earlier clocks below read it as "from the future",
        // which the staleness check treats as fresh.
        let mut d = deps(
            cfg,
            bank.clone(),
            tracker(vec![issue("1", "MT-1", &["rust"])]),
            cache.clone(),
            refresh,
        );
        let mut cursor = 0;
        for now in [-60, refresh - 1] {
            d.now = Box::new(move || at(now));
            assert_eq!(
                prefetch_cycle(&CancelWait::default(), &d, &mut cursor).await,
                CycleOutcome::Idle
            );
        }
        assert!(bank.asked.lock().unwrap().is_empty());
        assert!(bank.asked_shared.lock().unwrap().is_empty());
        d.now = Box::new(move || at(refresh));
        assert_eq!(
            prefetch_cycle(&CancelWait::default(), &d, &mut cursor).await,
            CycleOutcome::Prefetched(1)
        );
        assert_eq!(bank.asked.lock().unwrap().len(), 1);
        assert_eq!(bank.asked_shared.lock().unwrap().len(), 1);
        let past_original_ttl = at(PREFETCH_TTL.as_secs() as i64 + 1);
        assert_eq!(
            cache.try_get("alice", "MT-1", past_original_ttl).unwrap()[0].id,
            "new-own"
        );
        assert_eq!(
            cache
                .try_get_team("alice", "MT-1", past_original_ttl)
                .unwrap()[0]
                .id,
            "new-team"
        );
        assert!(
            cache
                .try_get(
                    "alice",
                    "MT-1",
                    at(refresh + PREFETCH_TTL.as_secs() as i64 + 1)
                )
                .is_none()
        );
    }

    #[tokio::test]
    async fn fresh_entries_do_not_spend_the_actual_recall_budget() {
        for shared in [false, true] {
            let mut cfg = teams(true, BackendKind::Hindsight);
            if shared {
                cfg.memory.team_bank = "team".into();
            }
            let bank = bank_with(&[]);
            let cache = Arc::new(PrefetchCache::new());
            let issues: Vec<_> = (0..MAX_PER_CYCLE * 2 + 1)
                .map(|i| issue(&i.to_string(), &format!("MT-{i}"), &["rust"]))
                .collect();
            cache.replace(
                (0..MAX_PER_CYCLE)
                    .map(|i| (PrefetchKey::new("alice", format!("MT-{i}")), Vec::new()))
                    .collect(),
                at(0),
            );
            let d = deps(
                cfg,
                bank.clone(),
                tracker(issues.clone()),
                cache.clone(),
                60,
            );
            let mut cursor = 0;
            let expected = if shared {
                MAX_PER_CYCLE / 2
            } else {
                MAX_PER_CYCLE
            };
            assert_eq!(
                prefetch_cycle(&CancelWait::default(), &d, &mut cursor).await,
                CycleOutcome::Prefetched(expected)
            );
            assert_eq!(
                bank.asked.lock().unwrap().len() + bank.asked_shared.lock().unwrap().len(),
                MAX_PER_CYCLE,
                "both bank calls count; fresh entries cost nothing"
            );
            assert_eq!(
                bank.asked.lock().unwrap()[0].1,
                format!("MT-{MAX_PER_CYCLE}")
            );
            assert_eq!(cursor, MAX_PER_CYCLE + expected);
            assert!(
                cache
                    .try_get("alice", &format!("MT-{}", cursor), at(60))
                    .is_none(),
                "work past the budget waits for the next cycle"
            );
            assert_eq!(cache.len(), MAX_PER_CYCLE + expected);
            prefetch_cycle(&CancelWait::default(), &d, &mut cursor).await;
            assert!(
                cache
                    .try_get(
                        "alice",
                        &issues[MAX_PER_CYCLE + expected].identifier,
                        at(60)
                    )
                    .is_some(),
                "rotation reaches the deferred candidate"
            );
        }
    }

    #[tokio::test]
    async fn prefetch_cycle_makes_no_tracker_calls_when_candidates_are_published() {
        let bank = bank_with(&[("alice", vec![fact("f1", "x")])]);
        let account = tracker(Vec::new());
        let first = tracker(vec![issue("9", "MT-9", &["rust"])]);
        let second = tracker(Vec::new());
        let cache = Arc::new(PrefetchCache::new());
        let o = crate::Orchestrator::new("WORKFLOW.md");
        o.set_reads_target(account.clone(), "");
        o.set_reads_triage_snapshot(crate::reads::TriageSnapshot {
            trackers: vec![
                crate::reads::ProjectTracker {
                    slug: "first".into(),
                    tracker: first.clone(),
                },
                crate::reads::ProjectTracker {
                    slug: "second".into(),
                    tracker: second.clone(),
                },
            ],
            ..Default::default()
        });
        // The publication, not what another query would return, is authoritative.
        o.set_reads_candidates(PrefetchTarget {
            issues: vec![issue("1", "MT-1", &["rust"]), issue("2", "MT-2", &["rust"])],
            fetched_at: at(0),
            owner_slugs: Vec::new(),
        });
        let handle = o.control();
        let d = PrefetchDeps {
            teams: Arc::new(teams(true, BackendKind::Hindsight)),
            target: || handle.reads_candidates(),
            backend: bank.clone() as Arc<dyn MemoryBackend>,
            cache: cache.clone(),
            interval: PREFETCH_INTERVAL,
            max_backoff_ms: MAX_PREFETCH_BACKOFF_MS,
            now: Box::new(|| at(0)),
        };
        assert_eq!(
            prefetch_cycle(&CancelWait::default(), &d, &mut 0).await,
            CycleOutcome::Prefetched(2)
        );
        for tracker in [account, first, second] {
            assert_eq!(
                tracker.candidate_calls(),
                0,
                "prefetch must spend no Linear quota"
            );
        }
        assert_eq!(
            *bank.asked.lock().unwrap(),
            vec![
                ("alice".into(), "MT-1".into()),
                ("alice".into(), "MT-2".into())
            ]
        );
        assert_eq!(cache.try_get("alice", "MT-1", at(1)).unwrap()[0].id, "f1");
        assert_eq!(cache.try_get("alice", "MT-2", at(1)).unwrap()[0].id, "f1");
    }

    #[tokio::test]
    async fn prefetch_cycle_is_idle_when_published_candidates_are_stale_or_absent() {
        let bank = bank_with(&[]);
        let tracker = tracker(vec![issue("1", "MT-1", &["rust"])]);
        let cache = Arc::new(PrefetchCache::new());
        cache.replace(
            vec![(PrefetchKey::new("alice", "MT-2"), vec![fact("old", "x")])],
            at(600),
        );
        for fetched_at in [None, Some(at(0))] {
            let d = PrefetchDeps {
                teams: Arc::new(teams(true, BackendKind::Hindsight)),
                target: || {
                    fetched_at.map(|fetched_at| PrefetchTarget {
                        issues: tracker.candidates.clone(),
                        fetched_at,
                        owner_slugs: Vec::new(),
                    })
                },
                backend: bank.clone() as Arc<dyn MemoryBackend>,
                cache: cache.clone(),
                interval: PREFETCH_INTERVAL,
                max_backoff_ms: MAX_PREFETCH_BACKOFF_MS,
                now: Box::new(|| at(CANDIDATES_TTL.as_secs() as i64 + 1)),
            };
            let mut cursor = 3;
            assert_eq!(
                prefetch_cycle(&CancelWait::default(), &d, &mut cursor).await,
                CycleOutcome::Idle
            );
            assert_eq!(
                cursor, 3,
                "an unread board does not advance the recall window"
            );
            assert!(
                cache.try_get("alice", "MT-2", at(601)).is_some(),
                "missing/stale candidates do not imply departure"
            );
        }
        assert_eq!(tracker.candidate_calls(), 0);
        assert!(bank.asked.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_cycle_recalls_all_projects_and_deduplicates_first_project_wins() {
        let bank = bank_with(&[]);
        let first = tracker(vec![issue("1", "MT-1", &["rust"])]);
        let second = tracker(vec![
            issue("1", "MT-1", &["web"]),
            issue("2", "MT-2", &["web"]),
        ]);
        let cache = Arc::new(PrefetchCache::new());
        let o = crate::Orchestrator::new("WORKFLOW.md");
        o.set_reads_target(first.clone(), "");
        o.set_reads_triage_snapshot(crate::reads::TriageSnapshot {
            trackers: vec![
                crate::reads::ProjectTracker {
                    slug: "first".into(),
                    tracker: first.clone(),
                },
                crate::reads::ProjectTracker {
                    slug: "second".into(),
                    tracker: second.clone(),
                },
            ],
            ..Default::default()
        });
        o.set_reads_candidates(PrefetchTarget {
            issues: first
                .candidates
                .iter()
                .chain(&second.candidates)
                .cloned()
                .collect(),
            fetched_at: at(0),
            owner_slugs: Vec::new(),
        });
        let handle = o.control();
        let d = PrefetchDeps {
            teams: Arc::new(teams(true, BackendKind::Hindsight)),
            target: || handle.reads_candidates(),
            backend: bank.clone() as Arc<dyn MemoryBackend>,
            cache: cache.clone(),
            interval: PREFETCH_INTERVAL,
            max_backoff_ms: MAX_PREFETCH_BACKOFF_MS,
            now: Box::new(|| at(0)),
        };
        assert_eq!(
            prefetch_cycle(&CancelWait::default(), &d, &mut 0).await,
            CycleOutcome::Prefetched(2)
        );
        assert_eq!(first.candidate_calls(), 0);
        assert_eq!(second.candidate_calls(), 0);
        assert_eq!(
            *bank.asked.lock().unwrap(),
            vec![
                ("alice".into(), "MT-1".into()),
                ("bob".into(), "MT-2".into())
            ]
        );
        assert!(cache.try_get("alice", "MT-1", at(1)).is_some());
        assert!(cache.try_get("bob", "MT-1", at(1)).is_none());
    }

    /// The happy path: each candidate is routed with the existing `route()` and
    /// recalled for under the identity that ticket routes to.
    #[tokio::test]
    async fn a_cycle_caches_one_entry_per_routed_candidate() {
        let bank = bank_with(&[
            ("alice", vec![fact("f1", "alice knows this")]),
            ("bob", vec![fact("f2", "bob knows this")]),
        ]);
        let tracker = tracker(vec![
            issue("1", "MT-1", &["rust"]),
            issue("2", "MT-2", &["web"]),
            // Routes nowhere: no label overlap and no default identity.
            issue("3", "MT-3", &["mystery"]),
        ]);
        let cache = Arc::new(PrefetchCache::new());
        let d = deps(
            teams(true, BackendKind::Hindsight),
            Arc::clone(&bank),
            tracker,
            Arc::clone(&cache),
            0,
        );
        assert_eq!(
            prefetch_cycle(&CancelWait::default(), &d, &mut 0).await,
            CycleOutcome::Prefetched(2)
        );
        assert_eq!(
            bank.asked.lock().expect("asked").clone(),
            vec![
                ("alice".to_string(), "MT-1".to_string()),
                ("bob".to_string(), "MT-2".to_string())
            ],
            "routed with the existing route(); the unroutable ticket asked nothing"
        );
        assert_eq!(
            cache.try_get("alice", "MT-1", at(1)).expect("hit")[0].id,
            "f1"
        );
        assert_eq!(
            cache.try_get("bob", "MT-2", at(1)).expect("hit")[0].id,
            "f2"
        );
        assert!(
            cache.try_get("alice", "MT-2", at(1)).is_none(),
            "a hit is per (identity, ticket), never per identity"
        );
    }

    /// Replace-not-merge, the `record_issue_states` precedent: a ticket that has
    /// left the candidate set stops being served.
    #[tokio::test]
    async fn a_ticket_that_leaves_the_candidate_set_is_evicted() {
        let bank = bank_with(&[("alice", vec![fact("f1", "x")])]);
        let cache = Arc::new(PrefetchCache::new());
        let first = deps(
            teams(true, BackendKind::Hindsight),
            Arc::clone(&bank),
            tracker(vec![
                issue("1", "MT-1", &["rust"]),
                issue("2", "MT-2", &["rust"]),
            ]),
            Arc::clone(&cache),
            0,
        );
        prefetch_cycle(&CancelWait::default(), &first, &mut 0).await;
        assert_eq!(cache.len(), 2);

        let second = deps(
            teams(true, BackendKind::Hindsight),
            bank,
            tracker(vec![issue("1", "MT-1", &["rust"])]),
            Arc::clone(&cache),
            1,
        );
        prefetch_cycle(&CancelWait::default(), &second, &mut 0).await;
        assert_eq!(cache.len(), 1);
        assert!(cache.try_get("alice", "MT-1", at(2)).is_some());
        assert!(
            cache.try_get("alice", "MT-2", at(2)).is_none(),
            "MT-2 left the candidate set; a merging cache would still serve it"
        );
    }

    /// A candidate set that routes nowhere clears the cache rather than letting
    /// yesterday's entries age out on their own.
    #[tokio::test]
    async fn an_idle_cycle_clears_the_cache() {
        let bank = bank_with(&[("alice", vec![fact("f1", "x")])]);
        let cache = Arc::new(PrefetchCache::new());
        let d = deps(
            teams(true, BackendKind::Hindsight),
            Arc::clone(&bank),
            tracker(vec![issue("1", "MT-1", &["rust"])]),
            Arc::clone(&cache),
            0,
        );
        prefetch_cycle(&CancelWait::default(), &d, &mut 0).await;
        assert_eq!(cache.len(), 1);

        let idle = deps(
            teams(true, BackendKind::Hindsight),
            bank,
            tracker(vec![issue("9", "MT-9", &["mystery"])]),
            Arc::clone(&cache),
            1,
        );
        assert_eq!(
            prefetch_cycle(&CancelWait::default(), &idle, &mut 0).await,
            CycleOutcome::Idle
        );
        assert!(cache.is_empty());
    }

    /// **The bug the rotating window fixes.** With more candidates than one
    /// cycle recalls, a fixed window would re-recall the same first
    /// `MAX_PER_CYCLE` every minute and the ones past it would NEVER hold a
    /// cached recall — so `dispatch_issue` would silently render no memory
    /// section for them forever, on exactly the busy board where memory helps
    /// most. Successive cycles must cover the whole set.
    #[tokio::test]
    async fn the_window_rotates_so_every_candidate_is_eventually_cached() {
        let n = MAX_PER_CYCLE + 3;
        let issues: Vec<Issue> = (0..n)
            .map(|i| issue(&i.to_string(), &format!("MT-{i}"), &["rust"]))
            .collect();
        let cache = Arc::new(PrefetchCache::new());
        let mut cursor = 0usize;
        for cycle in 0..2 {
            let d = deps(
                teams(true, BackendKind::Hindsight),
                bank_with(&[("alice", vec![fact("f1", "x")])]),
                tracker(issues.clone()),
                Arc::clone(&cache),
                cycle,
            );
            prefetch_cycle(&CancelWait::default(), &d, &mut cursor).await;
        }
        // Two cycles of 10 cover 13 candidates.
        for i in 0..n {
            assert!(
                cache.try_get("alice", &format!("MT-{i}"), at(2)).is_some(),
                "MT-{i} was never cached after two cycles (cache has {} entries)",
                cache.len()
            );
        }
    }

    /// The other half of that fix: a candidate this cycle's window did NOT reach
    /// keeps what the cache already holds for it, because it is still live. Only
    /// a ticket that left the candidate set is evicted — which is the eviction
    /// rule `record_issue_states` actually states.
    #[tokio::test]
    async fn a_live_candidate_outside_the_window_keeps_its_entry() {
        let issues: Vec<Issue> = (0..MAX_PER_CYCLE + 1)
            .map(|i| issue(&i.to_string(), &format!("MT-{i}"), &["rust"]))
            .collect();
        let cache = Arc::new(PrefetchCache::new());
        let mut cursor = 0usize;
        let first = deps(
            teams(true, BackendKind::Hindsight),
            bank_with(&[("alice", vec![fact("f1", "x")])]),
            tracker(issues.clone()),
            Arc::clone(&cache),
            0,
        );
        prefetch_cycle(&CancelWait::default(), &first, &mut cursor).await;
        assert!(cache.try_get("alice", "MT-0", at(1)).is_some());

        // Cycle two's window starts at MT-10 and wraps, so MT-1..MT-9 are live
        // but untouched. They must still be served.
        let second = deps(
            teams(true, BackendKind::Hindsight),
            bank_with(&[("alice", vec![fact("f1", "x")])]),
            tracker(issues),
            Arc::clone(&cache),
            1,
        );
        prefetch_cycle(&CancelWait::default(), &second, &mut cursor).await;
        for i in 0..=MAX_PER_CYCLE {
            assert!(
                cache.try_get("alice", &format!("MT-{i}"), at(2)).is_some(),
                "MT-{i} lost its entry to a cycle that simply did not reach it"
            );
        }
    }

    /// A carried entry keeps its ORIGINAL timestamp, so carrying can never
    /// extend a fact set past the TTL — otherwise a ticket that stayed a
    /// candidate would be served from a recall made hours ago.
    #[test]
    fn carrying_does_not_refresh_a_timestamp() {
        let cache = PrefetchCache::new();
        let key = PrefetchKey::new("alice", "MT-1");
        let live: HashSet<PrefetchKey> = [key.clone()].into_iter().collect();
        cache.sync(vec![(key.clone(), vec![fact("f1", "x")])], &live, at(0));

        let ttl = PREFETCH_TTL.as_secs() as i64;
        // Carried, repeatedly, right up to the boundary.
        cache.sync(Vec::new(), &live, at(ttl - 1));
        assert!(cache.try_get("alice", "MT-1", at(ttl)).is_some());
        // And past it, the carry itself drops it.
        cache.sync(Vec::new(), &live, at(ttl + 1));
        assert!(cache.try_get("alice", "MT-1", at(ttl + 1)).is_none());
        assert!(
            cache.is_empty(),
            "a stale entry is dropped by the sync, not merely refused"
        );
    }

    // ── degradation ─────────────────────────────────────────────────────────────────────────────

    /// The tailnet is down: the cycle reports a failure (so the schedule backs
    /// off) and stops rather than walking the rest of the backlog into the same
    /// error.
    #[tokio::test]
    async fn a_failing_bank_backs_off_without_burning_the_backlog() {
        let _serial = crate::testsupport::TRACING_TEST_LOCK.lock().await;
        let (events, subscriber) = crate::testsupport::recording_subscriber();
        let _guard = tracing::subscriber::set_default(subscriber);
        tracing::callsite::rebuild_interest_cache();
        let bank = Arc::new(FakeBank {
            fail: true,
            ..FakeBank::default()
        });
        let cache = Arc::new(PrefetchCache::new());
        let d = deps(
            teams(true, BackendKind::Hindsight),
            Arc::clone(&bank),
            tracker(vec![
                issue("1", "MT-1", &["rust"]),
                issue("2", "MT-2", &["rust"]),
                issue("3", "MT-3", &["rust"]),
            ]),
            Arc::clone(&cache),
            0,
        );
        assert_eq!(
            prefetch_cycle(&CancelWait::default(), &d, &mut 0).await,
            CycleOutcome::BankFailure
        );
        assert_eq!(
            bank.asked.lock().expect("asked").len(),
            1,
            "one failure ends the cycle; the rest is picked up after the back-off"
        );
        assert!(cache.is_empty(), "and dispatch degrades to exactly `none`");
        assert!(
            events
                .lock()
                .unwrap()
                .iter()
                .any(|event| event.level == "WARN"
                    && event.message.contains("the remote bank failed")
                    && event
                        .fields
                        .get("error")
                        .is_some_and(|e| e.contains("the tailnet is down"))),
            "an attempted failing recall must not swallow the warning"
        );
    }

    /// The published set is usable right up to its TTL boundary.
    #[tokio::test]
    async fn candidates_at_the_ttl_boundary_are_still_recalled() {
        let bank = bank_with(&[("alice", vec![fact("f1", "x")])]);
        let cache = Arc::new(PrefetchCache::new());
        let mut boundary = deps(
            teams(true, BackendKind::Hindsight),
            Arc::clone(&bank),
            tracker(vec![issue("1", "MT-1", &["rust"])]),
            Arc::clone(&cache),
            0,
        );
        boundary.now = Box::new(|| at(CANDIDATES_TTL.as_secs() as i64));
        assert_eq!(
            prefetch_cycle(&CancelWait::default(), &boundary, &mut 0).await,
            CycleOutcome::Prefetched(1)
        );
        assert_eq!(bank.asked.lock().unwrap().len(), 1);
    }

    /// A cycle that fails partway keeps what it already recalled, so one bad
    /// candidate does not cost the prompt the candidates before it. It reports
    /// `Partial`, not `BankFailure` (STUDIO-2169): the service answered this
    /// cycle, so only the failing bank backs off, not the whole schedule.
    #[tokio::test]
    async fn a_partial_cycle_keeps_what_it_got() {
        struct FlakyBank {
            calls: Mutex<usize>,
        }
        #[async_trait]
        impl MemoryBackend for FlakyBank {
            async fn retain(&self, _rec: &Record) -> Result<String, MemoryError> {
                Ok(String::new())
            }
            async fn recall(&self, _identity: &str, _q: &Query) -> Result<Recalled, MemoryError> {
                let mut n = self.calls.lock().expect("calls");
                *n += 1;
                if *n > 1 {
                    return Err(MemoryError::Io("gone".to_string()));
                }
                Ok(Recalled {
                    facts: vec![fact("f1", "first")],
                    skipped: Vec::new(),
                })
            }
            async fn invalidate(
                &self,
                _identity: &str,
                _fact_id: &str,
                _reason: &str,
            ) -> Result<bool, MemoryError> {
                Ok(false)
            }

            async fn revalidate(
                &self,
                _identity: &str,
                _fact_id: &str,
            ) -> Result<bool, MemoryError> {
                Ok(false)
            }
        }
        let cache = Arc::new(PrefetchCache::new());
        let d = PrefetchDeps {
            teams: Arc::new(teams(true, BackendKind::Hindsight)),
            target: || {
                Some(PrefetchTarget {
                    issues: vec![issue("1", "MT-1", &["rust"]), issue("2", "MT-2", &["rust"])],
                    fetched_at: at(0),
                    owner_slugs: Vec::new(),
                })
            },
            backend: Arc::new(FlakyBank {
                calls: Mutex::new(0),
            }) as Arc<dyn MemoryBackend>,
            cache: Arc::clone(&cache),
            interval: Duration::from_millis(5),
            max_backoff_ms: 20,
            now: Box::new(|| at(0)),
        };
        assert_eq!(
            prefetch_cycle(&CancelWait::default(), &d, &mut 0).await,
            CycleOutcome::Partial
        );
        assert!(cache.try_get("alice", "MT-1", at(1)).is_some());
        assert!(cache.try_get("alice", "MT-2", at(1)).is_none());
    }

    // ── the cache itself ────────────────────────────────────────────────────────────────────────

    /// A stale entry is a MISS, not a stale hit: a fact set this old has had time
    /// to be invalidated by the dashboard button §5.2.3 exists for.
    #[test]
    fn a_stale_entry_is_a_miss() {
        let cache = PrefetchCache::new();
        cache.replace(
            vec![(PrefetchKey::new("alice", "MT-1"), vec![fact("f1", "x")])],
            at(0),
        );
        let ttl = PREFETCH_TTL.as_secs() as i64;
        assert!(
            cache.try_get("alice", "MT-1", at(ttl)).is_some(),
            "at the boundary"
        );
        assert!(
            cache.try_get("alice", "MT-1", at(ttl + 1)).is_none(),
            "past it"
        );
        // A clock that stepped backwards must not empty the cache for a whole TTL.
        assert!(cache.try_get("alice", "MT-1", at(-60)).is_some());
    }

    /// **The non-blocking read.** A held write lock is a miss, never a wait —
    /// which is the property that makes `try_get` callable from `dispatch_issue`.
    #[test]
    fn a_contended_lock_is_a_miss_not_a_wait() {
        let cache = PrefetchCache::new();
        cache.replace(
            vec![(PrefetchKey::new("alice", "MT-1"), vec![fact("f1", "x")])],
            at(0),
        );
        assert!(cache.try_get("alice", "MT-1", at(1)).is_some());
        let held = cache.entries.write().expect("write lock");
        let started = std::time::Instant::now();
        assert!(
            cache.try_get("alice", "MT-1", at(1)).is_none(),
            "a contended read gives up instead of waiting"
        );
        assert!(
            started.elapsed() < Duration::from_millis(50),
            "it gave up in {:?}",
            started.elapsed()
        );
        drop(held);
        assert!(cache.try_get("alice", "MT-1", at(1)).is_some());
    }

    /// The entry bound. Extra entries are dropped from the tail — candidate
    /// order, so what goes is the backlog least likely to dispatch.
    #[test]
    fn the_cache_is_bounded_by_entries() {
        let cache = PrefetchCache::new();
        let fresh: Vec<_> = (0..MAX_CACHE_ENTRIES * 2)
            .map(|i| {
                (
                    PrefetchKey::new("alice", format!("MT-{i}")),
                    vec![fact("f", "x")],
                )
            })
            .collect();
        cache.replace(fresh, at(0));
        assert_eq!(cache.len(), MAX_CACHE_ENTRIES);
        assert!(cache.try_get("alice", "MT-0", at(0)).is_some());
        assert!(cache.try_get("alice", "MT-100", at(0)).is_none());
    }

    /// The byte bound, because every cached byte is a byte that may reach a
    /// turn-1 prompt. An entry that would breach it is dropped whole.
    #[test]
    fn the_cache_is_bounded_by_bytes() {
        let cache = PrefetchCache::new();
        let big = "z".repeat(MAX_CACHE_BYTES / 4);
        let fresh: Vec<_> = (0..10)
            .map(|i| {
                (
                    PrefetchKey::new("alice", format!("MT-{i}")),
                    vec![fact("f", &big)],
                )
            })
            .collect();
        cache.replace(fresh, at(0));
        assert_eq!(cache.len(), 4, "four fit, the rest are dropped whole");
        for f in cache.try_get("alice", "MT-0", at(0)).expect("hit") {
            assert_eq!(f.content.len(), big.len(), "never half a fact");
        }
    }

    /// An empty cache answers instantly and says nothing — the cold-start path
    /// every first dispatch after a daemon restart takes.
    #[test]
    fn a_cold_cache_is_a_clean_miss() {
        let cache = PrefetchCache::new();
        assert!(cache.try_get("alice", "MT-1", at(0)).is_none());
        assert!(cache.is_empty());
    }

    // ── the schedule ────────────────────────────────────────────────────────────────────────────

    /// The task is cancelled by the daemon's lifetime signal, on both sides of
    /// the sleep, so a shutdown never waits out a cycle.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_schedule_stops_on_cancel() {
        // `new()`, not `default()`: the default signal is deliberately UNARMED.
        let signal = crate::CancelSignal::new();
        let ctx = signal.wait();
        let cache = Arc::new(PrefetchCache::new());
        let d = deps(
            teams(true, BackendKind::Hindsight),
            bank_with(&[("alice", vec![fact("f1", "x")])]),
            tracker(vec![issue("1", "MT-1", &["rust"])]),
            Arc::clone(&cache),
            0,
        );
        let task = tokio::spawn(async move { run_prefetch_schedule(ctx, d).await });
        // Let at least one cycle land, then cancel.
        for _ in 0..200 {
            if !cache.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(cache.try_get("alice", "MT-1", at(1)).is_some());
        signal.cancel();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("the task stops promptly")
            .expect("joined");
    }

    /// The back-off bound, and the acceptance criterion the ticket words as
    /// "loop cadence unchanged (the triage standard)": a permanently down bank
    /// must not produce one remote recall per cadence. With a 5ms cadence and a
    /// 20ms ceiling, an un-backed-off loop would fire tens of recalls in the
    /// window below; a backed-off one fires a handful.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_schedule_backs_off_a_down_bank() {
        let bank = Arc::new(FakeBank {
            fail: true,
            ..FakeBank::default()
        });
        let cache = Arc::new(PrefetchCache::new());
        let d = deps(
            teams(true, BackendKind::Hindsight),
            Arc::clone(&bank),
            tracker(vec![issue("1", "MT-1", &["rust"])]),
            Arc::clone(&cache),
            0,
        );
        let signal = crate::CancelSignal::new();
        let ctx = signal.wait();
        let task = tokio::spawn(async move { run_prefetch_schedule(ctx, d).await });
        tokio::time::sleep(Duration::from_millis(300)).await;
        signal.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;

        let attempts = bank.asked.lock().expect("asked").len();
        assert!(attempts >= 1, "the schedule must have tried at least once");
        assert!(
            attempts <= 20,
            "a down bank must be backed off, not retried hot: {attempts} attempts in 300ms"
        );
        assert!(cache.is_empty(), "and dispatch degrades to exactly `none`");
    }

    // ── the shared team bank in the cycle (STUDIO-1040) ──────────────────

    /// With a `team_bank` configured the cycle queries it too, once per window
    /// item, and caches its facts beside the identity's own under the same key.
    #[tokio::test]
    async fn the_cycle_recalls_the_team_bank_and_caches_it_beside_own_facts() {
        let mut cfg = teams(true, BackendKind::Hindsight);
        cfg.memory.team_bank = "agent-team".to_string();
        cfg.memory.team_recall_top_k = 2;
        let mut bank = FakeBank {
            facts: [("alice".to_string(), vec![fact("own", "my own note")])]
                .into_iter()
                .collect(),
            team_facts: vec![fact("team1", "goldens are recaptured only")],
            ..FakeBank::default()
        };
        // Deterministic map contents for the one candidate.
        bank.facts
            .insert("alice".to_string(), vec![fact("own", "my own note")]);
        let bank = Arc::new(bank);
        let cache = Arc::new(PrefetchCache::new());
        let d = deps(
            cfg,
            Arc::clone(&bank),
            tracker(vec![issue("1", "MT-1", &["rust"])]),
            Arc::clone(&cache),
            0,
        );
        let mut cursor = 0usize;
        let outcome = prefetch_cycle(&CancelWait::default(), &d, &mut cursor).await;
        assert_eq!(outcome, CycleOutcome::Prefetched(1));

        let shared = bank.asked_shared.lock().expect("asked_shared").clone();
        assert_eq!(
            shared,
            vec![("agent-team".to_string(), "MT-1".to_string(), 2)],
            "the team bank must be queried with the candidate's ticket and capped at \
             team_recall_top_k (2 here), not the identity's own recall_top_k"
        );
        let own = cache.try_get("alice", "MT-1", at(1)).expect("own hit");
        assert_eq!(own[0].id, "own");
        let team = cache
            .try_get_team("alice", "MT-1", at(1))
            .expect("team hit");
        assert_eq!(team[0].id, "team1", "the team facts are cached beside own");
    }

    /// With no `team_bank` the cycle makes NO shared recall: the team-off path
    /// issues exactly the requests it did before STUDIO-1040.
    #[tokio::test]
    async fn a_team_off_cycle_makes_no_shared_recall() {
        let bank = bank_with(&[("alice", vec![fact("f1", "x")])]);
        let cache = Arc::new(PrefetchCache::new());
        let d = deps(
            teams(true, BackendKind::Hindsight),
            Arc::clone(&bank),
            tracker(vec![issue("1", "MT-1", &["rust"])]),
            Arc::clone(&cache),
            0,
        );
        let mut cursor = 0usize;
        prefetch_cycle(&CancelWait::default(), &d, &mut cursor).await;
        assert!(
            bank.asked_shared.lock().expect("asked_shared").is_empty(),
            "team bank off ⇒ no shared recall"
        );
        assert!(
            cache
                .try_get_team("alice", "MT-1", at(1))
                .expect("hit")
                .is_empty(),
            "and the cached entry carries no team facts"
        );
    }

    // ── STUDIO-2169: change-driven refresh and a real back-off ─────────────────────────────────

    /// One cycle at `secs` against the production cadence and back-off ceiling.
    /// The cache (and so every bank's back-off gate) is shared across calls; the
    /// bank and the published candidates are whatever this cycle sees.
    async fn cycle_at(
        cfg: &Teams,
        bank: &Arc<FakeBank>,
        issues: Vec<Issue>,
        cache: &Arc<PrefetchCache>,
        cursor: &mut usize,
        secs: i64,
    ) -> CycleOutcome {
        let mut d = deps(
            cfg.clone(),
            Arc::clone(bank),
            tracker(issues),
            Arc::clone(cache),
            secs,
        );
        d.interval = PREFETCH_INTERVAL;
        d.max_backoff_ms = MAX_PREFETCH_BACKOFF_MS;
        prefetch_cycle(&CancelWait::default(), &d, cursor).await
    }

    fn calls(bank: &FakeBank) -> (usize, usize) {
        (
            bank.asked.lock().unwrap().len(),
            bank.asked_shared.lock().unwrap().len(),
        )
    }

    fn with_team_bank() -> Teams {
        let mut cfg = teams(true, BackendKind::Hindsight);
        cfg.memory.team_bank = "team".into();
        cfg
    }

    /// Acceptance: a second poll cycle with unchanged candidates issues no new
    /// recall — not at the next cycle, and not at six minutes either, which the
    /// old five-minute refresh re-recalled every candidate at.
    #[tokio::test]
    async fn a_second_cycle_with_unchanged_candidates_issues_no_recall() {
        let cfg = with_team_bank();
        let bank = bank_with(&[("alice", vec![fact("own", "note")])]);
        let cache = Arc::new(PrefetchCache::new());
        let board = || vec![issue("1", "MT-1", &["rust"]), issue("2", "MT-2", &["web"])];
        let mut cursor = 0;
        assert_eq!(
            cycle_at(&cfg, &bank, board(), &cache, &mut cursor, 0).await,
            CycleOutcome::Prefetched(2)
        );
        assert_eq!(calls(&bank), (2, 2));
        for secs in [60, 6 * 60, 29 * 60] {
            assert_eq!(
                cycle_at(&cfg, &bank, board(), &cache, &mut cursor, secs).await,
                CycleOutcome::Idle,
                "unchanged at {secs}s"
            );
            assert_eq!(calls(&bank), (2, 2), "no new recall at {secs}s");
            assert!(cache.try_get("alice", "MT-1", at(secs)).is_some());
        }
        // The bounded TTL still refreshes an unchanged ticket eventually.
        let refresh = PREFETCH_REFRESH_AGE.as_secs() as i64;
        assert_eq!(
            cycle_at(&cfg, &bank, board(), &cache, &mut cursor, refresh).await,
            CycleOutcome::Prefetched(2)
        );
        assert_eq!(calls(&bank), (4, 4));
    }

    /// Acceptance: a changed ticket triggers a fresh recall — of that ticket
    /// only — whether its title, its labels or its state changed.
    #[tokio::test]
    async fn a_changed_ticket_triggers_a_fresh_recall() {
        type Edit = fn(&mut Issue);
        let edits: [(&str, Edit); 3] = [
            ("title", |i| i.title = "work on MT-1, rescoped".into()),
            ("labels", |i| {
                i.labels = Some(vec!["rust".into(), "bug".into()])
            }),
            ("state", |i| i.state = "In Progress".into()),
        ];
        for (what, edit) in edits {
            let cfg = with_team_bank();
            let bank = bank_with(&[]);
            let cache = Arc::new(PrefetchCache::new());
            let board = || vec![issue("1", "MT-1", &["rust"]), issue("2", "MT-2", &["web"])];
            let mut cursor = 0;
            cycle_at(&cfg, &bank, board(), &cache, &mut cursor, 0).await;
            let mut changed = board();
            edit(&mut changed[0]);
            assert_eq!(
                cycle_at(&cfg, &bank, changed, &cache, &mut cursor, 60).await,
                CycleOutcome::Prefetched(1),
                "{what} changed"
            );
            assert_eq!(calls(&bank), (3, 3), "{what}: one ticket, both banks");
            assert_eq!(
                bank.asked.lock().unwrap()[2],
                ("alice".to_string(), "MT-1".to_string()),
                "{what}: the changed ticket is the one re-recalled"
            );
            assert_eq!(
                cache.entries.read().unwrap()[&PrefetchKey::new("alice", "MT-1")].at,
                at(60),
                "{what}: and its entry is the fresh answer"
            );
        }
    }

    /// Acceptance: a failed recall backs off and is NOT retried on the next
    /// cycle; it is retried once the back-off (two cadences) has passed.
    #[tokio::test]
    async fn a_failed_recall_is_not_retried_on_the_next_cycle() {
        let cfg = teams(true, BackendKind::Hindsight);
        let down = Arc::new(FakeBank {
            fail: true,
            ..FakeBank::default()
        });
        let cache = Arc::new(PrefetchCache::new());
        let board = || vec![issue("1", "MT-1", &["rust"])];
        let mut cursor = 0;
        assert_eq!(
            cycle_at(&cfg, &down, board(), &cache, &mut cursor, 0).await,
            CycleOutcome::BankFailure
        );
        assert_eq!(calls(&down), (1, 0));
        // The next cycle — even with the bank back — leaves it alone.
        let up = bank_with(&[("alice", vec![fact("own", "note")])]);
        assert_eq!(
            cycle_at(&cfg, &up, board(), &cache, &mut cursor, 60).await,
            CycleOutcome::Idle
        );
        assert_eq!(calls(&up), (0, 0), "backing off means not asking");
        assert_eq!(
            cycle_at(&cfg, &up, board(), &cache, &mut cursor, 120).await,
            CycleOutcome::Prefetched(1)
        );
        assert_eq!(calls(&up), (1, 0));
        assert!(cache.try_get("alice", "MT-1", at(120)).is_some());
    }

    /// Consecutive failures double the wait: retried at 2, then 6, then 14
    /// minutes — never once a cycle.
    #[tokio::test]
    async fn consecutive_failures_back_off_exponentially() {
        let cfg = teams(true, BackendKind::Hindsight);
        let down = Arc::new(FakeBank {
            fail: true,
            ..FakeBank::default()
        });
        let cache = Arc::new(PrefetchCache::new());
        let mut cursor = 0;
        let mut asked_at = Vec::new();
        for minute in 0..15 {
            let before = calls(&down).0;
            cycle_at(
                &cfg,
                &down,
                vec![issue("1", "MT-1", &["rust"])],
                &cache,
                &mut cursor,
                minute * 60,
            )
            .await;
            if calls(&down).0 > before {
                asked_at.push(minute);
            }
        }
        assert_eq!(asked_at, vec![0, 2, 6, 14]);
    }

    /// A team bank that fails backs off on its own: the next cycle asks
    /// nothing, and after the back-off only the missing team half is recalled —
    /// the identity's own facts are kept, not re-recalled.
    #[tokio::test]
    async fn a_failed_team_recall_backs_off_and_refreshes_only_the_team_half() {
        let cfg = with_team_bank();
        let flaky = Arc::new(FakeBank {
            facts: [("alice".into(), vec![fact("own", "note")])]
                .into_iter()
                .collect(),
            fail_shared: true,
            ..FakeBank::default()
        });
        let cache = Arc::new(PrefetchCache::new());
        let board = || vec![issue("1", "MT-1", &["rust"]), issue("2", "MT-2", &["rust"])];
        let mut cursor = 0;
        assert_eq!(
            cycle_at(&cfg, &flaky, board(), &cache, &mut cursor, 0).await,
            CycleOutcome::Prefetched(2)
        );
        assert_eq!(
            calls(&flaky),
            (2, 1),
            "the first team failure gates the second candidate's team recall"
        );
        assert_eq!(cache.try_get("alice", "MT-1", at(0)).unwrap().len(), 1);
        let up = Arc::new(FakeBank {
            team_facts: vec![fact("shared", "team note")],
            ..FakeBank::default()
        });
        assert_eq!(
            cycle_at(&cfg, &up, board(), &cache, &mut cursor, 60).await,
            CycleOutcome::Idle
        );
        assert_eq!(calls(&up), (0, 0), "the team bank is backing off");
        assert_eq!(
            cycle_at(&cfg, &up, board(), &cache, &mut cursor, 120).await,
            CycleOutcome::Prefetched(2)
        );
        assert_eq!(calls(&up), (0, 2), "only the team half is recalled");
        assert_eq!(
            cache.try_get("alice", "MT-1", at(120)).unwrap()[0].id,
            "own",
            "the own facts are kept"
        );
        assert_eq!(
            cache.try_get_team("alice", "MT-1", at(120)).unwrap()[0].id,
            "shared"
        );
        assert_eq!(
            cache.entries.read().unwrap()[&PrefetchKey::new("alice", "MT-1")].at,
            at(0),
            "completing the team half extends nothing"
        );
    }

    #[test]
    fn backoff_starts_at_two_cadences_and_is_capped() {
        let min = Duration::from_secs(60);
        let cap = MAX_PREFETCH_BACKOFF_MS;
        let secs = |n| backoff_delay(n, min, cap).as_secs();
        assert_eq!(
            (1..=6).map(secs).collect::<Vec<_>>(),
            vec![120, 240, 480, 900, 900, 900]
        );
        assert_eq!(secs(u32::MAX), 900, "no overflow");
        assert_eq!(
            backoff_delay(1, min, 0),
            min,
            "a ceiling under the cadence never makes it retry faster"
        );
    }

    /// A write through `TeamsMemory` makes the next cycle re-recall the bank it
    /// wrote to, while the entry keeps serving until then.
    #[tokio::test]
    async fn a_write_marks_its_bank_for_the_next_cycle() {
        let cfg = with_team_bank();
        let bank = bank_with(&[]);
        let cache = Arc::new(PrefetchCache::new());
        let board = || vec![issue("1", "MT-1", &["rust"]), issue("2", "MT-2", &["web"])];
        let mut cursor = 0;
        cycle_at(&cfg, &bank, board(), &cache, &mut cursor, 0).await;
        cache.note_written(Some("alice"));
        assert!(
            cache.try_get("alice", "MT-1", at(30)).is_some(),
            "still served"
        );
        assert_eq!(
            cycle_at(&cfg, &bank, board(), &cache, &mut cursor, 60).await,
            CycleOutcome::Prefetched(1),
            "only alice's entry"
        );
        cache.note_written(None);
        assert_eq!(
            cycle_at(&cfg, &bank, board(), &cache, &mut cursor, 120).await,
            CycleOutcome::Prefetched(2),
            "a team-bank write touches every entry"
        );
    }

    /// An invalidation takes the record out of the cache at once, not after
    /// the (now longer) refresh age.
    #[test]
    fn an_invalidation_drops_the_record_at_once() {
        let cache = PrefetchCache::new();
        let alice = PrefetchKey::new("alice", "MT-1");
        let bob = PrefetchKey::new("bob", "MT-2");
        cache.replace_both(
            vec![
                (
                    alice.clone(),
                    vec![fact("a1", "x"), fact("a2", "y")],
                    vec![fact("t1", "z")],
                ),
                (
                    bob.clone(),
                    vec![fact("a1", "same id, bob's bank")],
                    vec![fact("t1", "z")],
                ),
            ],
            at(0),
        );
        cache.note_invalidated(Some("alice"), "a1");
        let own = cache.try_get("alice", "MT-1", at(1)).unwrap();
        assert_eq!(
            own.iter().map(|f| f.id.as_str()).collect::<Vec<_>>(),
            ["a2"]
        );
        assert_eq!(
            cache.try_get("bob", "MT-2", at(1)).unwrap().len(),
            1,
            "another bank"
        );
        cache.note_invalidated(None, "t1");
        assert!(
            cache
                .try_get_team("alice", "MT-1", at(1))
                .unwrap()
                .is_empty()
        );
        assert!(cache.try_get_team("bob", "MT-2", at(1)).unwrap().is_empty());
        let guard = cache.entries.read().unwrap();
        assert!(
            guard[&alice].dirty && guard[&bob].dirty,
            "and both are re-recalled"
        );
    }

    /// A write that lands while a cycle's recall is in flight is not lost to
    /// the cycle's swap: the recall may predate it, so the entry it installs
    /// is re-recalled next cycle, and an invalidated record stays out.
    #[tokio::test]
    async fn a_write_during_a_cycle_survives_its_swap() {
        /// Answers with `facts`, and while "in flight" sees `TeamsMemory`
        /// invalidate one of them — exactly the interleaving a swap could undo.
        struct WriteMidRecall {
            cache: Arc<PrefetchCache>,
            asked: Mutex<usize>,
        }
        #[async_trait]
        impl MemoryBackend for WriteMidRecall {
            async fn retain(&self, _rec: &Record) -> Result<String, MemoryError> {
                Ok(String::new())
            }
            async fn recall(&self, _identity: &str, _q: &Query) -> Result<Recalled, MemoryError> {
                *self.asked.lock().unwrap() += 1;
                self.cache.note_invalidated(Some("alice"), "wrong");
                Ok(Recalled {
                    facts: vec![
                        fact("wrong", "corrected since"),
                        fact("right", "still true"),
                    ],
                    skipped: Vec::new(),
                })
            }
            async fn invalidate(
                &self,
                _identity: &str,
                _fact_id: &str,
                _reason: &str,
            ) -> Result<bool, MemoryError> {
                Ok(false)
            }
            async fn revalidate(
                &self,
                _identity: &str,
                _fact_id: &str,
            ) -> Result<bool, MemoryError> {
                Ok(false)
            }
        }
        let cache = Arc::new(PrefetchCache::new());
        let bank = Arc::new(WriteMidRecall {
            cache: Arc::clone(&cache),
            asked: Mutex::new(0),
        });
        let mut d = deps(
            teams(true, BackendKind::Hindsight),
            bank_with(&[]),
            tracker(vec![issue("1", "MT-1", &["rust"])]),
            Arc::clone(&cache),
            0,
        );
        d.backend = Arc::clone(&bank) as Arc<dyn MemoryBackend>;
        let mut cursor = 0;
        prefetch_cycle(&CancelWait::default(), &d, &mut cursor).await;
        let own = cache.try_get("alice", "MT-1", at(0)).unwrap();
        assert_eq!(
            own.iter().map(|f| f.id.as_str()).collect::<Vec<_>>(),
            ["right"]
        );
        assert!(
            cache.entries.read().unwrap()[&PrefetchKey::new("alice", "MT-1")].dirty,
            "the answer may predate the write, so it is re-recalled"
        );
        d.now = Box::new(|| at(60));
        prefetch_cycle(&CancelWait::default(), &d, &mut cursor).await;
        assert_eq!(*bank.asked.lock().unwrap(), 2);
    }
}
