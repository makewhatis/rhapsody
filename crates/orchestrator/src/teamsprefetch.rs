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

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use chrono::{DateTime, Utc};
use rhapsody_config::memory::{Fact, MemoryBackend};
use rhapsody_config::teams::{MemoryBackend as BackendKind, Teams};
use rhapsody_core::Issue;
use rhapsody_tracker::Tracker;

use crate::backoff::failure_backoff_ms;
use crate::control_loop::CancelWait;
use crate::teams::{LoadSnapshot, route};
use crate::teamscompose::recall_query;

/// The prefetch pass's own cadence. One minute matches [`triage`]'s and is
/// slower than the 30s poll on purpose: this is ahead-of-dispatch work nobody
/// waits on, and each cycle spends one remote recall per candidate.
///
/// [`triage`]: crate::triage::TRIAGE_INTERVAL
pub const PREFETCH_INTERVAL: Duration = Duration::from_secs(60);

/// The back-off ceiling. A bank outage settles at one attempt per 15 minutes
/// rather than one per cadence — triage's "never a hot retry loop against a down
/// API" bound, at the same value.
pub const MAX_PREFETCH_BACKOFF_MS: i64 = 15 * 60 * 1000;

/// How long a prefetched entry may be served for. Past this it is a **miss**,
/// not a stale hit: a fact set fetched an hour ago has had an hour to be
/// invalidated by the dashboard button §5.2.3 exists for, and rendering it would
/// undo the correction someone just made.
///
/// Comfortably longer than [`PREFETCH_INTERVAL`], so a single skipped cycle (a
/// slow bank, one back-off step) does not empty the cache.
pub const PREFETCH_TTL: Duration = Duration::from_secs(10 * 60);

/// How many candidates one cycle will recall for. A freshly-enabled Teams
/// pointed at a large backlog would otherwise fire one remote recall per
/// candidate in a burst; the remainder is picked up next cycle, in candidate
/// order, so nothing is skipped — only spread. Triage's `MAX_PER_CYCLE`, at the
/// same value and for the same reason.
const MAX_PER_CYCLE: usize = 10;

/// The most entries the cache will hold. The candidate set is already bounded by
/// [`MAX_PER_CYCLE`] per cycle and replaced wholesale each cycle, so this is a
/// backstop against a future caller that merges instead of replacing — the bound
/// is stated rather than inferred from the caller's good behaviour.
pub const MAX_CACHE_ENTRIES: usize = 64;

/// The most fact content, in bytes, the whole cache will hold. Every cached byte
/// is a byte that may reach a turn-1 prompt, so the cache carries the same kind
/// of hard ceiling `MAX_SECTION_BYTES` puts on the rendered section — enough for
/// [`MAX_CACHE_ENTRIES`] entries at a full section each, and no more.
pub const MAX_CACHE_BYTES: usize = 64 * 1024;

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

/// One cached recall: the facts, and when they were fetched.
#[derive(Debug, Clone)]
struct Entry {
    facts: Vec<Fact>,
    at: DateTime<Utc>,
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

    /// **Replaces** the whole cache with `fresh`, bounded first.
    ///
    /// Replace, never merge — the `record_issue_states` precedent (§0.11.3): a
    /// ticket that has left the candidate set must stop being served from a
    /// stale entry, and a map that only ever grows would quietly hand a dispatch
    /// facts recalled for a ticket nobody is working any more. Eviction is
    /// therefore not a separate pass; it is what replacement *is*.
    ///
    /// The bounded map is built before the lock is taken, so the write lock is
    /// held for one move and a concurrent [`try_get`](Self::try_get) is
    /// essentially never turned away.
    pub fn replace(&self, fresh: Vec<(PrefetchKey, Vec<Fact>)>, now: DateTime<Utc>) {
        let bounded = bound(fresh, now);
        if let Ok(mut guard) = self.entries.write() {
            *guard = bounded;
        }
        // A poisoned lock leaves the previous contents in place, which the TTL
        // will retire on its own. There is nothing better to do here and nothing
        // worth failing a background cycle over.
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
    match now.signed_duration_since(at).to_std() {
        Ok(age) => age > PREFETCH_TTL,
        Err(_) => false,
    }
}

/// Applies [`MAX_CACHE_ENTRIES`] and [`MAX_CACHE_BYTES`] to a cycle's results.
///
/// Entries are taken in the order the cycle produced them, which is candidate
/// order — so what a bound drops is the tail of the backlog, the tickets least
/// likely to dispatch this tick, rather than an arbitrary hash-map slice. An
/// entry whose facts would breach the byte ceiling is dropped whole rather than
/// truncated: half a fact set is a prompt nobody can reason about.
fn bound(fresh: Vec<(PrefetchKey, Vec<Fact>)>, now: DateTime<Utc>) -> HashMap<PrefetchKey, Entry> {
    let mut out = HashMap::with_capacity(fresh.len().min(MAX_CACHE_ENTRIES));
    let mut bytes = 0usize;
    for (key, facts) in fresh {
        if out.len() >= MAX_CACHE_ENTRIES {
            break;
        }
        let cost: usize = facts.iter().map(|f| f.content.len()).sum();
        if bytes + cost > MAX_CACHE_BYTES {
            continue;
        }
        bytes += cost;
        out.insert(key, Entry { facts, at: now });
    }
    out
}

/// The live tracker one cycle reads candidates from. Mirrors
/// [`TriageTarget`](crate::triage::TriageTarget), and for the same reason: the
/// handle exists before the first config load, so the tracker arrives later.
pub struct PrefetchTarget {
    pub tracker: Arc<dyn Tracker>,
}

/// Everything [`run_prefetch_schedule`] runs against. The absence of an
/// `Orchestrator`, a control channel and a store is the off-loop guarantee, in
/// the type.
pub struct PrefetchDeps<TF> {
    /// The boot-loaded `teams.yaml`. Teams config is not hot-reloaded (out of
    /// scope for this slice, as for T3b), so this is captured once at the
    /// composition root.
    pub teams: Arc<Teams>,
    /// Yields the live tracker, or `None` when no config has loaded yet.
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
    /// Nothing to do: no config loaded yet, or no candidate routed anywhere.
    Idle,
    /// This many `(identity, ticket)` pairs were cached.
    Prefetched(usize),
    /// A tracker read failed. Back off; the cache keeps serving what it has.
    TrackerFailure,
    /// The bank failed or timed out. Back off; dispatch degrades to `none`.
    BankFailure,
}

impl CycleOutcome {
    fn is_failure(self) -> bool {
        matches!(
            self,
            CycleOutcome::TrackerFailure | CycleOutcome::BankFailure
        )
    }
}

/// Whether the prefetch task should exist at all: **only** with Teams enabled,
/// `memory.backend: hindsight`, and a roster to route to.
///
/// `local`, `none` and Teams-off spawn nothing — not a task that returns early,
/// nothing — which is what makes "byte-identical everything" true of those
/// configurations rather than merely likely. An empty roster is included here
/// rather than left to the cycle because [`route`] answers `None` for every
/// candidate without one, so every cycle would be a no-op that still fetched.
pub fn prefetch_enabled(teams: &Teams) -> bool {
    teams.enabled && teams.memory.backend == BackendKind::Hindsight && !teams.roster.is_empty()
}

/// Runs the prefetch pass on its own cadence until `ctx` is cancelled.
///
/// The first thing it does is **wait**: a cycle at t=0 would race the daemon's
/// first config load for a tracker and find none. Cancellation is checked on both
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
    let mut failures: i64 = 0;
    loop {
        // Back off AT LEAST the normal cadence: retrying a down bank sooner than
        // we would poll a healthy one is the hot loop the design forbids.
        let delay = if failures > 0 {
            deps.interval.max(Duration::from_millis(
                failure_backoff_ms(failures, deps.max_backoff_ms).max(0) as u64,
            ))
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
        let outcome = prefetch_cycle(&ctx, &deps).await;
        if outcome.is_failure() {
            failures += 1;
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

/// One prefetch pass: fetch candidates, route each with the **existing pure
/// [`route`]**, recall from the remote bank, and replace the cache.
///
/// Routing here is the same function dispatch will run, on the same ticket, so a
/// hit is a hit for the identity that actually takes the ticket. The one input it
/// cannot have is live load: this task holds no `Orchestrator` and so no
/// `running` map, and [`LoadSnapshot`] only ever breaks *ties* in the
/// label-overlap tier. A tie broken differently here is a cache miss at dispatch
/// — no memory section, the safe degradation — never a wrong teammate's facts,
/// because the key carries the identity the facts were recalled for.
pub(crate) async fn prefetch_cycle<TF>(ctx: &CancelWait, deps: &PrefetchDeps<TF>) -> CycleOutcome
where
    TF: Fn() -> Option<PrefetchTarget>,
{
    let Some(target) = (deps.target)() else {
        return CycleOutcome::Idle; // no config loaded yet
    };
    let issues = match target.tracker.fetch_candidate_issues().await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(err = %e, "teams memory prefetch could not fetch candidates");
            return CycleOutcome::TrackerFailure;
        }
    };
    let routed = routed_candidates(&deps.teams, &issues);
    if routed.is_empty() {
        // Nothing routes anywhere: replace with an empty cache rather than
        // leaving yesterday's entries to age out. Being absent from the candidate
        // set is meaningful (§0.11.3).
        deps.cache.replace(Vec::new(), (deps.now)());
        return CycleOutcome::Idle;
    }
    let mut fresh: Vec<(PrefetchKey, Vec<Fact>)> = Vec::with_capacity(routed.len());
    for (identity, iss) in routed.into_iter().take(MAX_PER_CYCLE) {
        // A shutdown must not have to wait out a whole cycle of remote recalls.
        if ctx.is_cancelled() {
            break;
        }
        let q = recall_query(&deps.teams, iss);
        match deps.backend.recall(&identity, &q).await {
            Ok(recalled) => {
                // "A record that could not be read is skipped LOUDLY, never
                // fatal" — the same contract `local` has, and this is the caller
                // that owns the log.
                for (what, why) in &recalled.skipped {
                    tracing::warn!(
                        identity = %identity,
                        item = %what,
                        reason = %why,
                        "teams memory prefetch: skipping an unusable bank record"
                    );
                }
                fresh.push((
                    PrefetchKey::new(identity, iss.identifier.clone()),
                    recalled.facts,
                ));
            }
            Err(e) => {
                // Stop the cycle rather than walking the rest of the backlog into
                // the same failure: whatever broke is almost certainly still
                // broken, and burning every candidate against it is the hot loop.
                // What was already recalled is still worth caching.
                tracing::warn!(
                    identity = %identity,
                    ticket = %iss.identifier,
                    error = %e,
                    "teams memory prefetch: the remote bank failed; keeping what this cycle \
                     already recalled and backing off"
                );
                if !fresh.is_empty() {
                    deps.cache.replace(fresh, (deps.now)());
                }
                return CycleOutcome::BankFailure;
            }
        }
    }
    let n = fresh.len();
    deps.cache.replace(fresh, (deps.now)());
    if n == 0 {
        CycleOutcome::Idle
    } else {
        CycleOutcome::Prefetched(n)
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
    use rhapsody_tracker::TrackerError;
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
        fail: bool,
        asked: Mutex<Vec<(String, String)>>,
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

        async fn invalidate(
            &self,
            _identity: &str,
            _fact_id: &str,
            _reason: &str,
        ) -> Result<bool, MemoryError> {
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

    /// The same, but every candidate fetch fails — "Linear is down".
    fn broken_tracker() -> Arc<Fake> {
        let mut f = Fake::new();
        f.candidates_err = Some(TrackerError::Other("linear is down".to_string()));
        Arc::new(f)
    }

    fn deps(
        t: Teams,
        bank: Arc<FakeBank>,
        tracker: Arc<Fake>,
        cache: Arc<PrefetchCache>,
        now: i64,
    ) -> PrefetchDeps<impl Fn() -> Option<PrefetchTarget>> {
        PrefetchDeps {
            teams: Arc::new(t),
            target: move || {
                Some(PrefetchTarget {
                    tracker: Arc::clone(&tracker) as Arc<dyn Tracker>,
                })
            },
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
            prefetch_cycle(&CancelWait::default(), &d).await,
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
        prefetch_cycle(&CancelWait::default(), &first).await;
        assert_eq!(cache.len(), 2);

        let second = deps(
            teams(true, BackendKind::Hindsight),
            bank,
            tracker(vec![issue("1", "MT-1", &["rust"])]),
            Arc::clone(&cache),
            1,
        );
        prefetch_cycle(&CancelWait::default(), &second).await;
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
        prefetch_cycle(&CancelWait::default(), &d).await;
        assert_eq!(cache.len(), 1);

        let idle = deps(
            teams(true, BackendKind::Hindsight),
            bank,
            tracker(vec![issue("9", "MT-9", &["mystery"])]),
            Arc::clone(&cache),
            1,
        );
        assert_eq!(
            prefetch_cycle(&CancelWait::default(), &idle).await,
            CycleOutcome::Idle
        );
        assert!(cache.is_empty());
    }

    // ── degradation ─────────────────────────────────────────────────────────────────────────────

    /// The tailnet is down: the cycle reports a failure (so the schedule backs
    /// off) and stops rather than walking the rest of the backlog into the same
    /// error.
    #[tokio::test]
    async fn a_failing_bank_backs_off_without_burning_the_backlog() {
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
            prefetch_cycle(&CancelWait::default(), &d).await,
            CycleOutcome::BankFailure
        );
        assert_eq!(
            bank.asked.lock().expect("asked").len(),
            1,
            "one failure ends the cycle; the rest is picked up after the back-off"
        );
        assert!(cache.is_empty(), "and dispatch degrades to exactly `none`");
    }

    /// A tracker outage is a failure too, and leaves the cache alone: what was
    /// prefetched is still valid until it goes stale.
    #[tokio::test]
    async fn a_failing_tracker_leaves_the_cache_serving() {
        let bank = bank_with(&[("alice", vec![fact("f1", "x")])]);
        let cache = Arc::new(PrefetchCache::new());
        let ok = deps(
            teams(true, BackendKind::Hindsight),
            Arc::clone(&bank),
            tracker(vec![issue("1", "MT-1", &["rust"])]),
            Arc::clone(&cache),
            0,
        );
        prefetch_cycle(&CancelWait::default(), &ok).await;

        let broken = deps(
            teams(true, BackendKind::Hindsight),
            bank,
            broken_tracker(),
            Arc::clone(&cache),
            1,
        );
        assert_eq!(
            prefetch_cycle(&CancelWait::default(), &broken).await,
            CycleOutcome::TrackerFailure
        );
        assert!(cache.try_get("alice", "MT-1", at(2)).is_some());
    }

    /// A cycle that fails partway keeps what it already recalled, so one bad
    /// candidate does not cost the prompt the candidates before it.
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
        }
        let cache = Arc::new(PrefetchCache::new());
        let d = PrefetchDeps {
            teams: Arc::new(teams(true, BackendKind::Hindsight)),
            target: || {
                Some(PrefetchTarget {
                    tracker: tracker(vec![
                        issue("1", "MT-1", &["rust"]),
                        issue("2", "MT-2", &["rust"]),
                    ]) as Arc<dyn Tracker>,
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
            prefetch_cycle(&CancelWait::default(), &d).await,
            CycleOutcome::BankFailure
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
}
