//! lifecycle — the ticket's CURRENT tracker state and its DURABLE assignee, for the read path
//! (STUDIO-702, STUDIO-735).
//!
//! Rhapsody-only; no Go v0.4.0 counterpart. The dashboard's Jobs worklist folds the issue-level
//! history listing, which is one row per ticket that has EVER had a run, and had nothing but the
//! run's OUTCOME to colour it with — so a cleanly-completed run read as "in review" forever, the
//! "in review" count grew monotonically with history, and no ticket ever reached "done".
//!
//! The missing signal is the ticket's lifecycle state in the tracker, which the daemon's own poll
//! cannot supply: [`Tracker::fetch_candidate_issues`] fetches active ∪ review, so a ticket that
//! merged has already dropped OUT of everything the scheduling state knows. Answering "is it done?"
//! means asking the tracker about it BY ID, which is what [`Tracker::fetch_issue_states_by_ids`]
//! does — the reconciliation read, reused here.
//!
//! Two properties keep that affordable and safe on an HTTP read path:
//!
//!   * **Cached with a TTL** ([`LIFECYCLE_TTL`]). The Jobs view fetches its listing once per mount,
//!     and a page is at most the store's page size of ids; within the window every further read is
//!     served from memory and costs no tracker call at all. Nothing polls in the background, so a
//!     daemon nobody is looking at makes no requests on this path.
//!   * **Best-effort, never fatal.** No tracker (before the first config load), a failed round-trip
//!     or an id the tracker does not return all yield "no answer" for that ticket, and the console
//!     falls back to exactly the run-outcome mapping it used before. A lifecycle lookup can make
//!     the Jobs list better; it can never make it fail.
//!
//! The second decoration is the ticket's DURABLE ASSIGNEE (STUDIO-735) — who did this work — and it
//! is here because it has the same shape: an off-loop, TTL-cached, best-effort per-ticket lookup
//! the listing does not wait on being right. The console previously read the assignee from the LIVE
//! Teams roster, which lists each teammate's currently-active tickets, so the moment a run finished
//! its teammate's name vanished from the row and the historical "who did what" was lost.
//!
//! Two records answer it, in strict preference order, and they are NOT the same kind of fact:
//!
//!   1. **The DISPLAYED RUN's own routing decision** — the routing row that dispatch wrote into
//!      that run's ledger (`crate::teams::EVENT_ROUTE` / `EVENT_UNROUTED`). This is the durable
//!      who-did-what record the ticket asks for: a fact about the RUN — this run wore this identity
//!      — so it is right even after a roster change, a re-label, or a re-assignment, and it is
//!      local, needing no tracker at all.
//!   2. **The `rhapsody:@<name>` label**, which IS the assignment (design record
//!      `~/.rhapsody/docs/STUDIO-572-rhapsody-teams.md`, §0.11.1). Read with
//!      [`Tracker::fetch_issue_labels_by_ids`], which answers for a MERGED ticket — the case the
//!      whole decoration exists for.
//!
//!      Be exact about what it contributes, because it is weaker than the first: it is who the
//!      ticket is assigned to **today**, not who ran the run on screen. It is NOT a way to recover
//!      a route row `storage.retention_days` deleted — `Store::prune` drops a run's events and the
//!      run row itself in one transaction, so a ticket whose route row was pruned has no history
//!      row left to decorate either. What it genuinely covers is a displayed run whose ledger is
//!      silent: Teams was off when it dispatched, or its event batch never landed. Resolving two
//!      identity labels by `min()` has the same caveat — deterministic, which is the property that
//!      matters for a column that must not flicker, but the name it picks is not necessarily the
//!      one who ran the work.
//!
//! **"The displayed run" is the precise scope, and the imprecise version was a bug.** The Jobs row
//! shows the ticket's newest run (`Store::list_issue_runs`, `started_at DESC`), so the teammate
//! shown must be that run's. Searching the TICKET for its newest `teams.route` row instead is not
//! an approximation of that, it is wrong in one direction: `crate::teams::route_teams` records
//! `teams.unrouted` for a solo or unmatched dispatch and NO event at all with Teams off, so a later
//! run of either kind cannot shadow an earlier route — and the ticket-wide search would keep naming
//! the teammate of a run that is no longer the one on screen. Every read here goes by `run_id`
//! ([`IssueKey::run_id`]), which also settles an ordering mismatch hiding in the same code: the
//! event search ordered by `run_id`, the row by `started_at`.
//!
//! That scoping is what keeps the column's "—" honest, and it makes the two records asymmetric: a
//! run that recorded `teams.unrouted` answers "nobody" and STOPS, because the run itself says so
//! and a label added afterwards cannot rewrite it. Only a run whose ledger is silent falls through
//! to the label.
//!
//! **What that costs a Teams-off daemon, stated rather than implied.** Teams off means every
//! displayed run's ledger is silent, so every row falls through and one page costs at most one
//! `fetch_issue_labels_by_ids` batch per [`LIFECYCLE_TTL`] window — a Linear round trip a Teams-off
//! daemon did not make before this decoration existed. That is deliberate and not an oversight:
//! a Teams-off daemon CAN still meet an identity label, on a ticket routed before Teams was turned
//! off, and naming that teammate is the true historical answer, which is the whole point. Gating
//! the read on `teams.enabled` would buy back the round trip at the price of the answer. Nothing
//! polls in the background, so the cost is bounded by someone actually watching the Jobs list;
//! a Teams-ON daemon now pays strictly LESS than it did before the run scoping, since an unrouted
//! run answers locally instead of falling through.
//!
//! Nothing here consults the live roster, and nothing here can invent an assignee for a run that
//! had none.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use rhapsody_core::normalize_state;
use rhapsody_tracker::Tracker;

use crate::dispatch::DispatchStates;
use crate::stop::ControlHandle;

/// How long a resolved lifecycle stays fresh before the next read re-queries the tracker. A minute
/// is well inside how fast an operator notices a merge and well outside the burst of reads one
/// dashboard load produces.
pub const LIFECYCLE_TTL: Duration = Duration::from_secs(60);

/// The most ids one tracker round-trip asks about. `QUERY_BY_IDS` is unpaginated (it passes
/// `first: len(ids)`), so the batch size IS the page size.
const LIFECYCLE_BATCH: usize = 100;

/// The most ids ONE lookup will refresh, across batches — a ceiling on the work a single request
/// can provoke, since `limit` on the listing endpoint is caller-supplied. Ids past it keep whatever
/// they had cached (usually nothing), which reads as "no answer" and falls back.
const MAX_LIFECYCLE_REFRESH: usize = 200;

/// The most answers the cache retains. A memo that only ever grows is a leak on a daemon that runs
/// for months while an operator pages through history, so once the map passes this the expired
/// entries are dropped, and if that is not enough the whole memo is. Discarding a memo is only ever
/// a cost — the next read re-queries — so the crude bound is the right one here.
const MAX_CACHE_ENTRIES: usize = 2_000;

/// Where a ticket sits in its tracker's lifecycle, normalized across workspaces whose state NAMES
/// differ. Derived from the configured state sets, never from Linear's own state `type`: the sets
/// are what the daemon already schedules by, so this classification and the selection gate cannot
/// disagree about what "terminal" means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueLifecycle {
    /// Live work — an active state, or any state that is none of the others (Backlog, Triage).
    Open,
    /// Parked in a configured REVIEW state: finished work awaiting a human.
    InReview,
    /// A configured TERMINAL state that is not a cancellation — merged/Done/Closed.
    Done,
    /// A configured CANCELED state — Cancelled / Won't Do / Duplicate.
    Canceled,
}

impl IssueLifecycle {
    /// The wire spelling, the vocabulary the dashboard maps its status Pill from.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::InReview => "in_review",
            Self::Done => "done",
            Self::Canceled => "canceled",
        }
    }
}

/// One ticket's resolved lifecycle: the tracker's workflow-state NAME verbatim beside the bucket it
/// normalizes to. The raw name rides along because it is the auditable ground truth for the
/// normalization — an operator asking "why does this say done?" needs the state the tracker actually
/// reported, not only the bucket this crate put it in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueLifecycleRow {
    pub state: String,
    pub lifecycle: IssueLifecycle,
}

/// Classifies a tracker workflow-state NAME against the configured state sets. `None` for a blank
/// state — "the tracker told us nothing", which must stay distinguishable from [`IssueLifecycle::Open`].
///
/// Order matters: `canceled` is a subset of `terminal`, so it is tested first; `review` is tested
/// against a state that is not ALSO active, matching [`DispatchStates::is_in_review`] exactly. A
/// state in none of the sets (Backlog, Triage) is [`IssueLifecycle::Open`] — it is live work the
/// gate simply is not dispatching yet, which is nothing like done.
pub fn classify(state: &str, states: &DispatchStates) -> Option<IssueLifecycle> {
    let st = normalize_state(state);
    if st.is_empty() {
        return None;
    }
    if states.canceled.contains(&st) {
        return Some(IssueLifecycle::Canceled);
    }
    if states.terminal.contains(&st) {
        return Some(IssueLifecycle::Done);
    }
    if states.review.contains(&st) && !states.active.contains(&st) {
        return Some(IssueLifecycle::InReview);
    }
    Some(IssueLifecycle::Open)
}

/// One history row's coordinates. Both halves are needed and neither substitutes for the other: the
/// tracker is queried by opaque issue `id`, while the store's own event ledger is read by `run_id`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IssueKey {
    /// The tracker's opaque issue id — the key every resolved map is keyed by, matching the
    /// `issue_id` on the history row being decorated.
    pub id: String,
    /// The id of the run the row DISPLAYS, i.e. the one `Store::list_issue_runs` kept for this
    /// ticket. Attribution is scoped to exactly this run and never to the ticket: a ticket routed
    /// to a teammate and later re-run solo, unrouted or with Teams off must show the SECOND run's
    /// answer, which is "nobody" (STUDIO-735 route-back).
    pub run_id: i64,
}

/// Bounds the memo at [`MAX_CACHE_ENTRIES`]: expired entries go first, and if the map is still over
/// the cap it is cleared. Called with the lock already held, right after a batch of inserts.
fn prune(entries: &mut HashMap<String, Entry>, now: Instant) {
    if entries.len() <= MAX_CACHE_ENTRIES {
        return;
    }
    entries.retain(|_, e| now.duration_since(e.at) < LIFECYCLE_TTL);
    if entries.len() > MAX_CACHE_ENTRIES {
        entries.clear();
    }
}

/// One cached answer. `row: None` records that the tracker did NOT return the id — a deleted or
/// inaccessible issue — and is cached exactly like a hit so a permanently-missing ticket is not
/// re-queried on every dashboard load.
struct Entry {
    row: Option<IssueLifecycleRow>,
    at: Instant,
}

/// The TTL cache behind [`ControlHandle::issue_lifecycles`]. Shared (`Arc`) between the orchestrator
/// and every clone of its control handle, so the whole daemon has ONE window rather than one per
/// HTTP task.
/// One cached assignee. An EMPTY `name` records "nobody was routed for this run" and is cached
/// exactly like a hit, so a solo or Teams-off ticket is not re-queried on every dashboard load.
///
/// `run_id` is the run the answer was resolved FOR. It is part of the freshness test, not just
/// bookkeeping: a new run of the same ticket makes the memo's answer an answer to a different
/// question, and serving it for the TTL's remainder would show the previous run's teammate on the
/// new run's row.
struct AssigneeEntry {
    name: String,
    run_id: i64,
    at: Instant,
}

#[derive(Default)]
pub struct LifecycleCache {
    entries: Mutex<HashMap<String, Entry>>,
    /// The assignee memo (STUDIO-735), keyed by tracker issue id and bounded exactly as `entries`
    /// is. A second map rather than a second field on [`Entry`] because the two decorations resolve
    /// from different sources and must fail independently: a tracker that cannot say what state a
    /// ticket is in must not also erase who worked it, which the store alone can answer.
    assignees: Mutex<HashMap<String, AssigneeEntry>>,
}

impl LifecycleCache {
    /// Resolves `ids` (tracker issue ids) to their current lifecycles, refreshing whatever has gone
    /// stale from `target` first. Ids with no answer are simply absent from the result.
    ///
    /// `target` is `None` before the first config load, and the whole refresh is then skipped —
    /// already-cached rows are still served, so a hot-reload gap degrades to staleness rather than
    /// to blankness. A tracker error stops the refresh and is logged; it never propagates, because
    /// there is no caller who could act on it (the listing itself has already succeeded).
    ///
    /// Two concurrent reads over the same cold ids can both fetch. That is deliberate: the
    /// alternative is holding a lock across a network round-trip, and a duplicate read-only query
    /// costs less than serializing every dashboard load behind one.
    pub async fn resolve(
        &self,
        ids: &[String],
        target: Option<(Arc<dyn Tracker>, DispatchStates)>,
        now: Instant,
    ) -> HashMap<String, IssueLifecycleRow> {
        let (mut out, stale) = self.partition(ids, now);
        let Some((tracker, states)) = target else {
            return out;
        };
        if stale.is_empty() {
            return out;
        }
        for chunk in stale.chunks(LIFECYCLE_BATCH) {
            let issues = match tracker.fetch_issue_states_by_ids(chunk).await {
                Ok(issues) => issues,
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        ids = chunk.len(),
                        "lifecycle lookup failed; serving cached ticket states",
                    );
                    break;
                }
            };
            let by_id: HashMap<&str, &str> = issues
                .iter()
                .map(|iss| (iss.id.as_str(), iss.state.as_str()))
                .collect();
            let mut guard = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
            for id in chunk {
                let row = by_id.get(id.as_str()).and_then(|state| {
                    classify(state, &states).map(|lifecycle| IssueLifecycleRow {
                        state: (*state).to_string(),
                        lifecycle,
                    })
                });
                match &row {
                    // A refreshed answer replaces whatever stale one `partition` handed back.
                    Some(row) => {
                        out.insert(id.clone(), row.clone());
                    }
                    // The tracker no longer knows this id: drop the stale answer rather than keep
                    // reporting a state nothing confirms.
                    None => {
                        out.remove(id);
                    }
                }
                guard.insert(id.clone(), Entry { row, at: now });
            }
            prune(&mut guard, now);
        }
        out
    }

    /// Splits `ids` into every answer already CACHED at `now` — fresh or not — and the
    /// (deduplicated, capped) ids a refresh must ask about. Blank ids are dropped: a run row with
    /// no tracker id has nothing to look up.
    ///
    /// A stale row is returned as well as re-queried on purpose. It is what the caller gets when
    /// the refresh cannot happen (no config loaded yet) or fails, and a state from a minute ago
    /// beats no state at all; a successful refresh overwrites it in [`Self::resolve`].
    fn partition(
        &self,
        ids: &[String],
        now: Instant,
    ) -> (HashMap<String, IssueLifecycleRow>, Vec<String>) {
        let mut cached = HashMap::new();
        let mut stale = Vec::new();
        let mut seen = HashSet::new();
        let guard = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        for id in ids {
            if id.is_empty() || !seen.insert(id.as_str()) {
                continue;
            }
            let entry = guard.get(id);
            if let Some(row) = entry.and_then(|e| e.row.as_ref()) {
                cached.insert(id.clone(), row.clone());
            }
            let fresh = entry.is_some_and(|e| now.duration_since(e.at) < LIFECYCLE_TTL);
            if !fresh && stale.len() < MAX_LIFECYCLE_REFRESH {
                stale.push(id.clone());
            }
        }
        (cached, stale)
    }

    /// Resolves `keys` to each ticket's DURABLE assignee (STUDIO-735), refreshing whatever has gone
    /// stale, and answers the empty string for a ticket nobody was routed for. Tickets with no
    /// answer at all are simply absent from the result.
    ///
    /// The two sources are consulted in the module doc's preference order, and the second is only
    /// asked about the rows the first was SILENT on — which is not the same as the rows it did not
    /// name a teammate for:
    ///
    ///   1. `store` — the displayed run's own routing row ([`run_identity`]). Local, and a fact
    ///      about the RUN rather than about the ticket's labels today. It answers three ways, and
    ///      the third is why this is not a two-way fallback: a run that recorded
    ///      `teams.unrouted` answers "nobody" DEFINITIVELY and stops here, because the run itself
    ///      says it was solo or unmatched and a label cannot overrule it.
    ///   2. `tracker` — the `rhapsody:@<name>` label, for the rows whose routing evidence is simply
    ///      gone: the retention prune deleted it, or the dispatch happened with Teams off and never
    ///      wrote one. Skipped entirely when there is nothing left to ask about, so a healthy Teams
    ///      deployment pays no tracker call here at all.
    ///
    /// Best-effort throughout, exactly like [`Self::resolve`]: a store error, an absent tracker or a
    /// failed round-trip leaves a ticket unanswered rather than propagating, because the listing
    /// this decorates has already succeeded and no caller could act on the failure.
    ///
    /// The store reads are synchronous and run on the calling HTTP task, as every other store read
    /// on this layer does. **"Off-loop" here means off the control LOOP, not off its LOCK**:
    /// `Sqlite` serializes every caller through one `Mutex<Connection>` that the control task also
    /// takes to append events, and this refresh acquires it once per probe. Three things bound
    /// that, and they are the reason it is acceptable rather than merely small:
    ///
    ///   * Each probe is a run-scoped `LIMIT 1` seek on `idx_events_run_seq` — NOT
    ///     [`rhapsody_store::Store::run_events`], which returns a whole run's transcript-sized
    ///     ledger to read one row, and not the ticket-scoped `events`⋈`runs` search it replaced,
    ///     which sorted every event of every run of the ticket to return one.
    ///   * A routed run costs ONE probe; only a run with no route row pays the second (unrouted)
    ///     probe, so the lock is taken at most twice per stale key.
    ///   * The whole refresh is capped at [`MAX_LIFECYCLE_REFRESH`] keys and happens once per TTL
    ///     window, not once per read.
    pub async fn resolve_assignees(
        &self,
        keys: &[IssueKey],
        store: &Arc<dyn rhapsody_store::Store + Send + Sync>,
        tracker: Option<Arc<dyn Tracker>>,
        now: Instant,
    ) -> HashMap<String, String> {
        let (mut out, stale) = self.partition_assignees(keys, now);
        if stale.is_empty() {
            return out;
        }
        // The local ledger first: it needs no network, and it is the truer record.
        let mut answers: HashMap<String, String> = HashMap::new();
        let mut covered: HashSet<String> = HashSet::new();
        let mut unanswered: Vec<IssueKey> = Vec::new();
        for key in &stale {
            match run_identity(store.as_ref(), key.run_id) {
                RunIdentity::Routed(name) => {
                    answers.insert(key.id.clone(), name);
                    covered.insert(key.id.clone());
                }
                // The run said, on the record, that it was routed to nobody. That is an ANSWER —
                // it is covered, it caches, and the label is never asked.
                RunIdentity::Unrouted => {
                    covered.insert(key.id.clone());
                }
                RunIdentity::Unknown => unanswered.push(key.clone()),
            }
        }
        // Then the label, for whatever is left — and `covered` grows by which of those the tracker
        // actually answered about, so a failed round-trip leaves its ids untouched instead of
        // caching "nobody" over an assignee the console already had.
        match tracker.filter(|_| !unanswered.is_empty()) {
            Some(tracker) => {
                let (labelled, asked) = label_identities(tracker.as_ref(), &unanswered).await;
                answers.extend(labelled);
                covered.extend(asked);
            }
            // No tracker to ask AT ALL — before the first config load — is not a failed round-trip,
            // it is a complete answer for this window: nothing else could have spoken. Covering
            // these ids is what caches it. Without this the whole store loop re-ran on every
            // dashboard load until a config landed, which is exactly what the memo exists to stop.
            None => covered.extend(unanswered.iter().map(|k| k.id.clone())),
        }
        let mut guard = self
            .assignees
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        for key in stale.iter().filter(|k| covered.contains(&k.id)) {
            // Caching the EMPTY name is deliberate: it is what stops a solo, unrouted or Teams-off
            // ticket being asked about again on every dashboard load.
            let name = answers.get(&key.id).cloned().unwrap_or_default();
            if name.is_empty() {
                // A refresh that now answers "nobody" drops the stale answer rather than keep
                // reporting an attribution nothing confirms — the rule `resolve` follows too.
                out.remove(&key.id);
            } else {
                out.insert(key.id.clone(), name.clone());
            }
            guard.insert(
                key.id.clone(),
                AssigneeEntry {
                    name,
                    run_id: key.run_id,
                    at: now,
                },
            );
        }
        prune_assignees(&mut guard, now);
        out
    }

    /// The assignee half of [`Self::partition`], and it follows the same two rules: a stale answer
    /// is returned AS WELL AS re-queried (it is what the caller gets when the refresh fails), and
    /// the refresh set is deduplicated and capped at [`MAX_LIFECYCLE_REFRESH`].
    ///
    /// It adds a third rule, and the first rule bends to it: an entry resolved for a DIFFERENT run
    /// than the one this row displays is not stale, it is IRRELEVANT — it answers a different
    /// question — so it is neither served nor counted, exactly as if the memo held nothing. Serving
    /// it as the "beats no answer" fallback would put the previous run's teammate back on the new
    /// run's row whenever the refresh is capped or the tracker is down, which is the very
    /// mis-attribution the run scoping exists to prevent.
    ///
    /// A key with no `run_id` is still refreshable — the label lookup goes by `id` — but a key with
    /// no `id` is dropped: there is nothing to key the answer by.
    fn partition_assignees(
        &self,
        keys: &[IssueKey],
        now: Instant,
    ) -> (HashMap<String, String>, Vec<IssueKey>) {
        let mut cached = HashMap::new();
        let mut stale = Vec::new();
        let mut seen = HashSet::new();
        let guard = self
            .assignees
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        for key in keys {
            if key.id.is_empty() || !seen.insert(key.id.as_str()) {
                continue;
            }
            let entry = guard.get(&key.id).filter(|e| e.run_id == key.run_id);
            if let Some(name) = entry.map(|e| e.name.as_str()).filter(|n| !n.is_empty()) {
                cached.insert(key.id.clone(), name.to_string());
            }
            let fresh = entry.is_some_and(|e| now.duration_since(e.at) < LIFECYCLE_TTL);
            if !fresh && stale.len() < MAX_LIFECYCLE_REFRESH {
                stale.push(key.clone());
            }
        }
        (cached, stale)
    }
}

/// What ONE run's ledger says about who ran it. The third variant is the load-bearing one: "no
/// routing row" and "a routing row that named nobody" are different facts, and only the first may
/// fall through to the ticket's label.
enum RunIdentity {
    /// A `teams.route` row naming this teammate.
    Routed(String),
    /// A `teams.unrouted` row: this dispatch was solo or matched nobody, on the record.
    Unrouted,
    /// No routing row at all — Teams was off when this run dispatched, the retention prune has
    /// deleted the row, or the store could not be read. The label may still know.
    Unknown,
}

/// What identity the run `run_id` wore, from the store's own event ledger — `teams.route`'s
/// `identity=<name> reason=<why>` text, parsed by [`crate::triage::route_event_identity`].
///
/// **Scoped to the DISPLAYED run, never to the ticket** (STUDIO-735 route-back). A ticket-wide
/// search for the newest `teams.route` row gets a ticket re-run solo, unrouted or with Teams off
/// wrong in the one direction that matters: `crate::teams::route_teams` writes `teams.unrouted` for
/// those dispatches and NO event at all with Teams off, so neither can shadow an older
/// `teams.route`, and the row would keep naming a teammate who did not do this run's work. It also
/// dissolves an ordering mismatch that was invisible in the same code: `list_issue_runs` picks the
/// displayed run by `started_at DESC`, while the event search ordered by `run_id DESC`.
///
/// Two probes rather than one whole-ledger read: each is `LIMIT 1` against `idx_events_run_seq`,
/// and the second only runs for a run that recorded no route — so a routed run, the common case,
/// costs exactly one indexed row. A store error answers [`RunIdentity::Unknown`] — "ask the label
/// instead" — rather than propagating.
fn run_identity(store: &(dyn rhapsody_store::Store + Send + Sync), run_id: i64) -> RunIdentity {
    if run_id <= 0 {
        return RunIdentity::Unknown;
    }
    let probe = |kind: &str| {
        let q = rhapsody_store::EventQuery {
            text: String::new(),
            issue: String::new(),
            kind: kind.to_string(),
            run: run_id,
            limit: 1,
        };
        match store.search_events(q) {
            Ok(hits) => Ok(hits.into_iter().next()),
            Err(err) => {
                tracing::warn!(
                    run_id,
                    error = %err,
                    "assignee lookup could not read the run history; falling back to the ticket label",
                );
                Err(())
            }
        }
    };
    // A `teams.route` row whose text somehow carries no `identity=` is treated as no route at all,
    // which is what the parse already said; it then falls to the unrouted probe and, failing that,
    // to the label.
    match probe(crate::teams::EVENT_ROUTE) {
        Ok(Some(hit)) => {
            if let Some(name) = crate::triage::route_event_identity(&hit.text) {
                return RunIdentity::Routed(name);
            }
        }
        Ok(None) => {}
        Err(()) => return RunIdentity::Unknown,
    }
    match probe(crate::teams::EVENT_UNROUTED) {
        Ok(Some(_)) => RunIdentity::Unrouted,
        Ok(None) | Err(()) => RunIdentity::Unknown,
    }
}

/// The `rhapsody:@<name>` label of each of `keys`, batched exactly as the lifecycle refresh batches
/// its own lookup, beside the set of ids a round-trip actually COVERED — which is not the same
/// thing: a ticket the tracker answered about but that carries no identity label is covered with no
/// label, and that distinction is what lets the caller cache "nobody" without also caching it over
/// a chunk that simply failed. A failed round-trip stops the refresh and is logged; it never
/// propagates.
async fn label_identities(
    tracker: &dyn Tracker,
    keys: &[IssueKey],
) -> (Vec<(String, String)>, HashSet<String>) {
    let mut out = Vec::new();
    let mut covered = HashSet::new();
    for chunk in keys.chunks(LIFECYCLE_BATCH) {
        let ids: Vec<String> = chunk.iter().map(|k| k.id.clone()).collect();
        let issues = match tracker.fetch_issue_labels_by_ids(&ids).await {
            Ok(issues) => issues,
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    ids = ids.len(),
                    "assignee label lookup failed; serving cached ticket assignees",
                );
                break;
            }
        };
        out.extend(
            issues
                .iter()
                .filter_map(|iss| Some((iss.id.clone(), label_identity(iss)?))),
        );
        covered.extend(ids);
    }
    (out, covered)
}

/// The identity a ticket's labels name, or `None` for a ticket that names none.
///
/// Two rules mirror the router that wrote the label ([`crate::teams::route`]) rather than restating
/// it loosely. `rhapsody:solo` is checked FIRST and is absolute: a solo ticket dispatches
/// identity-less however it is otherwise labelled, so an identity label beside the opt-out never
/// described who ran it. And a ticket wearing two identity labels resolves to the smallest by name
/// rather than to whichever Linear happened to list first — the router breaks that tie by roster
/// order, which this read path does not have, and a tie broken by response order would make the
/// column flicker between loads.
fn label_identity(iss: &rhapsody_core::Issue) -> Option<String> {
    if crate::teams::is_solo(iss) {
        return None;
    }
    iss.labels
        .iter()
        .flatten()
        .filter_map(|l| l.strip_prefix(crate::teams::IDENTITY_LABEL_PREFIX))
        .filter(|name| !name.is_empty())
        .min()
        .map(str::to_string)
}

/// [`prune`] for the assignee memo — same bound, same "expired first, then give up" rule.
fn prune_assignees(entries: &mut HashMap<String, AssigneeEntry>, now: Instant) {
    if entries.len() <= MAX_CACHE_ENTRIES {
        return;
    }
    entries.retain(|_, e| now.duration_since(e.at) < LIFECYCLE_TTL);
    if entries.len() > MAX_CACHE_ENTRIES {
        entries.clear();
    }
}

impl ControlHandle {
    /// The daemon's off-loop "what state are these tickets in?" surface, backing the lifecycle
    /// fields on `GET /api/v1/history/issues` (STUDIO-702). Read-only and infallible: an id with no
    /// answer is absent from the map.
    ///
    /// It reads the SAME shared reads cell as the other off-loop surfaces, so a hot-reload that
    /// changes the tracker or the configured state sets is reflected without a restart.
    pub async fn issue_lifecycles(&self, ids: &[String]) -> HashMap<String, IssueLifecycleRow> {
        self.lifecycle
            .resolve(ids, self.reads_lifecycle_target(), Instant::now())
            .await
    }

    /// The daemon's off-loop "who did this work?" surface, backing the `assignee` field on
    /// `GET /api/v1/history/issues` (STUDIO-735). Read-only and infallible: a ticket with no answer
    /// is absent from the map, and one nobody was routed for answers the empty string.
    ///
    /// It takes the SAME account-level tracker as [`Self::issue_lifecycles`], for the same reason —
    /// the label read filters on `id: { in: … }` and carries no project scope — and reads it from
    /// the same hot-reloaded cell.
    pub async fn issue_assignees(&self, keys: &[IssueKey]) -> HashMap<String, String> {
        self.lifecycle
            .resolve_assignees(keys, &self.store, self.reads_tracker(), Instant::now())
            .await
    }

    /// The account-level tracker plus the configured state sets, under ONE lock acquisition (the
    /// discipline every other `reads_*_target` on this handle follows: a pair read separately can
    /// straddle a reload and classify one config's states with another's sets).
    ///
    /// The ACCOUNT tracker is the right client here even though it is the slug-bound one a
    /// `projects:` config leaves without a project — `fetch_issue_states_by_ids` filters on
    /// `id: { in: … }` and carries no project scope at all, so the workspace-wide answer does not
    /// depend on which project the client was built for. (The STUDIO-671 wedge was specific to the
    /// project-FILTERED candidate query.)
    ///
    /// `None` before the first config load — including when the published state sets are still
    /// empty, which is that same pre-load condition seen from the other side. Classifying against
    /// empty sets would call every ticket in the workspace "open", which is worse than no answer.
    fn reads_lifecycle_target(&self) -> Option<(Arc<dyn Tracker>, DispatchStates)> {
        let r = self.reads.read().unwrap_or_else(PoisonError::into_inner);
        let tracker = Arc::clone(r.tracker.as_ref()?);
        (!r.states.is_empty()).then(|| (tracker, r.states.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testsupport::set_of;
    use rhapsody_core::Issue;
    use rhapsody_tracker::fake::Fake;

    fn states() -> DispatchStates {
        DispatchStates {
            active: set_of(&["todo", "in progress"]),
            terminal: set_of(&["done", "canceled", "duplicate"]),
            review: set_of(&["in review"]),
            canceled: set_of(&["canceled", "duplicate"]),
        }
    }

    fn issue(id: &str, state: &str) -> Issue {
        Issue {
            id: id.to_string(),
            identifier: id.to_string(),
            state: state.to_string(),
            ..Issue::default()
        }
    }

    #[test]
    fn classify_names_every_bucket() {
        let st = states();
        for (state, want) in [
            ("Todo", Some(IssueLifecycle::Open)),
            ("In Progress", Some(IssueLifecycle::Open)),
            // Neither active nor terminal nor review: live work the gate is not dispatching yet.
            ("Backlog", Some(IssueLifecycle::Open)),
            ("In Review", Some(IssueLifecycle::InReview)),
            ("Done", Some(IssueLifecycle::Done)),
            // Canceled is a SUBSET of terminal and must not read as Done.
            ("Canceled", Some(IssueLifecycle::Canceled)),
            ("Duplicate", Some(IssueLifecycle::Canceled)),
            // Case and surrounding space normalize exactly as every other state reader does.
            ("  dOnE ", Some(IssueLifecycle::Done)),
            // A blank state is "no answer", never Open.
            ("", None),
            ("   ", None),
        ] {
            assert_eq!(classify(state, &st), want, "classify({state:?})");
        }
    }

    // A state configured as BOTH active and review stays dispatchable work, mirroring
    // `DispatchStates::is_in_review`.
    #[test]
    fn classify_prefers_active_over_review_for_an_overlapping_state() {
        let mut st = states();
        st.review.insert("in progress".to_string());
        assert_eq!(classify("In Progress", &st), Some(IssueLifecycle::Open));
    }

    fn fake_with(issues: &[(&str, &str)]) -> Arc<Fake> {
        let mut f = Fake::default();
        for (id, state) in issues {
            f.by_id.insert((*id).to_string(), issue(id, state));
        }
        Arc::new(f)
    }

    #[tokio::test]
    async fn resolve_classifies_each_id_and_omits_the_ones_the_tracker_does_not_know() {
        let tr = fake_with(&[("a", "Done"), ("b", "In Review"), ("c", "Todo")]);
        let cache = LifecycleCache::default();
        let ids: Vec<String> = ["a", "b", "c", "gone"]
            .iter()
            .map(|s| s.to_string())
            .collect();

        let got = cache
            .resolve(
                &ids,
                Some((Arc::clone(&tr) as Arc<dyn Tracker>, states())),
                Instant::now(),
            )
            .await;

        assert_eq!(
            got.len(),
            3,
            "the unknown id must be absent, not guessed: {got:?}"
        );
        assert_eq!(
            got.get("a"),
            Some(&IssueLifecycleRow {
                state: "Done".into(),
                lifecycle: IssueLifecycle::Done
            }),
        );
        assert_eq!(got["b"].lifecycle, IssueLifecycle::InReview);
        assert_eq!(got["c"].lifecycle, IssueLifecycle::Open);
        assert!(!got.contains_key("gone"));
    }

    // The whole point of the cache: a dashboard that reloads inside the TTL costs no tracker call.
    #[tokio::test]
    async fn resolve_serves_a_second_read_from_cache_and_re_queries_after_the_ttl() {
        let tr = fake_with(&[("a", "Done")]);
        let cache = LifecycleCache::default();
        let ids = vec!["a".to_string()];
        let target = || Some((Arc::clone(&tr) as Arc<dyn Tracker>, states()));
        let t0 = Instant::now();

        let first = cache.resolve(&ids, target(), t0).await;
        assert_eq!(first["a"].lifecycle, IssueLifecycle::Done);
        assert_eq!(tr.by_id_calls(), 1, "the cold read queries once");

        let second = cache.resolve(&ids, target(), t0 + LIFECYCLE_TTL / 2).await;
        assert_eq!(second["a"].lifecycle, IssueLifecycle::Done);
        assert_eq!(tr.by_id_calls(), 1, "a fresh entry must not re-query");

        let third = cache.resolve(&ids, target(), t0 + LIFECYCLE_TTL).await;
        assert_eq!(third["a"].lifecycle, IssueLifecycle::Done);
        assert_eq!(tr.by_id_calls(), 2, "past the TTL it re-queries");
    }

    // An id the tracker does not return is cached as a miss, so it is not re-asked every load.
    #[tokio::test]
    async fn resolve_caches_a_miss() {
        let tr = fake_with(&[]);
        let cache = LifecycleCache::default();
        let ids = vec!["gone".to_string()];
        let target = || Some((Arc::clone(&tr) as Arc<dyn Tracker>, states()));
        let t0 = Instant::now();

        assert!(cache.resolve(&ids, target(), t0).await.is_empty());
        assert!(cache.resolve(&ids, target(), t0).await.is_empty());
        assert_eq!(tr.by_id_calls(), 1, "a miss is cached like a hit");
    }

    #[tokio::test]
    async fn resolve_dedupes_and_drops_blank_ids() {
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&seen);
        let mut f = Fake::default();
        f.states_by_ids_func = Some(Box::new(move |ids| {
            recorder
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .extend(ids.iter().cloned());
            Ok(ids.iter().map(|id| issue(id, "Done")).collect())
        }));
        let tr = Arc::new(f);
        let cache = LifecycleCache::default();
        let ids: Vec<String> = ["a", "a", "", "a"].iter().map(|s| s.to_string()).collect();

        let got = cache
            .resolve(
                &ids,
                Some((Arc::clone(&tr) as Arc<dyn Tracker>, states())),
                Instant::now(),
            )
            .await;

        assert_eq!(got.len(), 1);
        assert_eq!(tr.by_id_calls(), 1);
        assert_eq!(
            *seen.lock().unwrap_or_else(PoisonError::into_inner),
            vec!["a".to_string()],
            "a blank id has nothing to look up and a repeat is one lookup",
        );
    }

    // No config loaded yet: serve what is cached, ask nothing, and never fail.
    #[tokio::test]
    async fn resolve_without_a_target_serves_cache_only() {
        let tr = fake_with(&[("a", "Done")]);
        let cache = LifecycleCache::default();
        let ids = vec!["a".to_string(), "b".to_string()];
        let t0 = Instant::now();
        cache
            .resolve(
                &ids,
                Some((Arc::clone(&tr) as Arc<dyn Tracker>, states())),
                t0,
            )
            .await;

        let got = cache.resolve(&ids, None, t0 + LIFECYCLE_TTL * 10).await;

        assert_eq!(
            got.len(),
            1,
            "the stale cached row is still the best answer"
        );
        assert_eq!(got["a"].lifecycle, IssueLifecycle::Done);
        assert_eq!(tr.by_id_calls(), 1, "no target means no query");
    }

    // A failing tracker degrades to "no answer" — the listing it decorates has already succeeded.
    #[tokio::test]
    async fn resolve_swallows_a_tracker_error() {
        let mut f = Fake::default();
        f.by_id_err = Some(rhapsody_tracker::TrackerError::Other("linear down".into()));
        let tr = Arc::new(f);
        let cache = LifecycleCache::default();
        let ids = vec!["a".to_string()];

        let got = cache
            .resolve(
                &ids,
                Some((Arc::clone(&tr) as Arc<dyn Tracker>, states())),
                Instant::now(),
            )
            .await;

        assert!(
            got.is_empty(),
            "a failed lookup answers nothing rather than guessing"
        );
    }

    // A ticket the tracker stops returning must not keep reporting its last known state.
    #[tokio::test]
    async fn a_refresh_that_loses_an_id_drops_the_stale_answer() {
        let hits: Arc<Mutex<Vec<(&str, &str)>>> = Arc::new(Mutex::new(vec![("a", "Done")]));
        let seed = Arc::clone(&hits);
        let mut f = Fake::default();
        f.states_by_ids_func = Some(Box::new(move |ids| {
            let known = seed.lock().unwrap_or_else(PoisonError::into_inner).clone();
            Ok(ids
                .iter()
                .filter_map(|id| {
                    known
                        .iter()
                        .find(|(k, _)| k == id)
                        .map(|(_, st)| issue(id, st))
                })
                .collect())
        }));
        let tr = Arc::new(f);
        let cache = LifecycleCache::default();
        let ids = vec!["a".to_string()];
        let target = || Some((Arc::clone(&tr) as Arc<dyn Tracker>, states()));
        let t0 = Instant::now();

        assert_eq!(
            cache.resolve(&ids, target(), t0).await["a"].lifecycle,
            IssueLifecycle::Done
        );
        hits.lock().unwrap_or_else(PoisonError::into_inner).clear();

        let after = cache.resolve(&ids, target(), t0 + LIFECYCLE_TTL).await;
        assert!(
            after.is_empty(),
            "a state nothing confirms must not survive: {after:?}"
        );
    }

    // The handle's own gate: a tracker with no published state sets is the pre-first-reload
    // condition, and classifying against empty sets would call every ticket in the workspace
    // "open" — worse than saying nothing.
    #[tokio::test]
    async fn the_handle_answers_nothing_until_a_config_has_published_its_state_sets() {
        let tr = fake_with(&[("a", "Done")]);
        let (o, _store) = crate::testsupport::orch_with_store();
        let ids = vec!["a".to_string()];

        assert!(
            o.control().issue_lifecycles(&ids).await.is_empty(),
            "no tracker captured yet",
        );

        o.set_reads_target(Arc::clone(&tr) as Arc<dyn Tracker>, "lin_key");
        assert!(
            o.control().issue_lifecycles(&ids).await.is_empty(),
            "a tracker without state sets cannot classify anything",
        );
        assert_eq!(tr.by_id_calls(), 0, "and it must not even ask");

        o.set_reads_triage_snapshot(crate::reads::TriageSnapshot {
            trackers: Vec::new(),
            states: states(),
            facts: Vec::new(),
            summon_token: String::new(),
        });
        let got = o.control().issue_lifecycles(&ids).await;
        assert_eq!(got["a"].lifecycle, IssueLifecycle::Done);
    }

    // The memo must not grow without bound on a daemon that runs for months.
    #[test]
    fn prune_drops_the_expired_first_and_only_then_gives_up() {
        let row = IssueLifecycleRow {
            state: "Done".into(),
            lifecycle: IssueLifecycle::Done,
        };
        let t0 = Instant::now();
        let now = t0 + LIFECYCLE_TTL * 2;

        // Under the cap: nothing is touched, however stale.
        let mut small: HashMap<String, Entry> = (0..8)
            .map(|i| {
                (
                    format!("i{i}"),
                    Entry {
                        row: Some(row.clone()),
                        at: t0,
                    },
                )
            })
            .collect();
        prune(&mut small, now);
        assert_eq!(small.len(), 8, "a small memo is left alone");

        // Over the cap with expired entries to give up: they go, the fresh ones stay.
        let mut mixed: HashMap<String, Entry> = (0..MAX_CACHE_ENTRIES + 10)
            .map(|i| {
                (
                    format!("i{i}"),
                    Entry {
                        row: Some(row.clone()),
                        at: if i < 20 { now } else { t0 },
                    },
                )
            })
            .collect();
        prune(&mut mixed, now);
        assert_eq!(mixed.len(), 20, "only the fresh entries survive");

        // Over the cap and ALL fresh: the memo is discarded rather than grown.
        let mut fresh: HashMap<String, Entry> = (0..MAX_CACHE_ENTRIES + 10)
            .map(|i| {
                (
                    format!("i{i}"),
                    Entry {
                        row: Some(row.clone()),
                        at: now,
                    },
                )
            })
            .collect();
        prune(&mut fresh, now);
        assert!(
            fresh.is_empty(),
            "an unprunable memo is dropped, never grown"
        );
    }

    // The per-request ceiling: a caller asking about thousands of tickets provokes a bounded number
    // of round-trips, and the ids past the cap simply go unanswered.
    #[tokio::test]
    async fn resolve_caps_the_ids_one_lookup_refreshes() {
        let ids: Vec<String> = (0..MAX_LIFECYCLE_REFRESH + 50)
            .map(|i| format!("i{i}"))
            .collect();
        let pairs: Vec<(&str, &str)> = ids.iter().map(|id| (id.as_str(), "Done")).collect();
        let tr = fake_with(&pairs);
        let cache = LifecycleCache::default();

        let got = cache
            .resolve(
                &ids,
                Some((Arc::clone(&tr) as Arc<dyn Tracker>, states())),
                Instant::now(),
            )
            .await;

        assert_eq!(got.len(), MAX_LIFECYCLE_REFRESH);
        assert_eq!(
            tr.by_id_calls(),
            MAX_LIFECYCLE_REFRESH.div_ceil(LIFECYCLE_BATCH),
            "batched, not one query per id",
        );
    }

    // ─── the durable assignee (STUDIO-735) ───────────────────────────────────────────────────

    /// An in-memory store the assignee tests seed one FINISHED run at a time, each seeding call
    /// returning that run's id — because the row's answer is scoped to the run it displays, so a
    /// test that cannot name a run cannot state what it is asserting.
    struct Ledger(Arc<dyn rhapsody_store::Store + Send + Sync>);

    impl Ledger {
        fn new() -> Self {
            Self(Arc::new(
                rhapsody_store::Sqlite::open(rhapsody_store::StorePath::InMemory)
                    .expect("open store"),
            ))
        }

        /// One finished run of `identifier`, carrying `routing` (kind, text) — or NO routing row at
        /// all, which is exactly what a Teams-off dispatch leaves behind (`route_teams` returns
        /// `None` before any event is described).
        fn run(&self, identifier: &str, routing: Option<(&str, String)>) -> i64 {
            let run = self
                .0
                .start_run(rhapsody_store::RunStart {
                    issue_identifier: identifier.to_string(),
                    ..rhapsody_store::RunStart::default()
                })
                .expect("start run");
            if let Some((kind, text)) = routing {
                self.0
                    .append_events(
                        run,
                        &[rhapsody_store::EventRow {
                            seq: 1,
                            at: "2026-09-02T00:00:00Z".into(),
                            kind: kind.into(),
                            tool: String::new(),
                            text,
                        }],
                    )
                    .expect("append routing event");
            }
            self.0
                .end_run(run, rhapsody_store::RunEnd::default())
                .expect("end run");
            run
        }

        fn routed(&self, identifier: &str, identity: &str) -> i64 {
            let text = format!("identity={identity} reason=label");
            self.run(identifier, Some((crate::teams::EVENT_ROUTE, text)))
        }

        fn unrouted(&self, identifier: &str, reason: &str) -> i64 {
            let text = format!("reason={reason}");
            self.run(identifier, Some((crate::teams::EVENT_UNROUTED, text)))
        }

        fn teams_off(&self, identifier: &str) -> i64 {
            self.run(identifier, None)
        }

        fn store(&self) -> Arc<dyn rhapsody_store::Store + Send + Sync> {
            Arc::clone(&self.0)
        }
    }

    /// A store holding no runs at all: every key's routing evidence is absent, so every answer
    /// these tests get comes from the label. Used where the ledger is not what is under test.
    fn empty_store() -> Arc<dyn rhapsody_store::Store + Send + Sync> {
        Ledger::new().store()
    }

    fn key(id: &str, run_id: i64) -> IssueKey {
        IssueKey {
            id: id.to_string(),
            run_id,
        }
    }

    fn labelled(id: &str, labels: &[&str]) -> Issue {
        Issue {
            id: id.to_string(),
            identifier: id.to_string(),
            labels: Some(labels.iter().map(|l| (*l).to_string()).collect()),
            ..Issue::default()
        }
    }

    // THE BUG (acceptance 1): the run is over — it has an outcome and an end time, and its teammate
    // has long since dropped off the live roster — and the ASSIGNED column must still name her.
    #[tokio::test]
    async fn a_finished_run_keeps_the_teammate_that_ran_it() {
        let ledger = Ledger::new();
        let run = ledger.routed("MT-1", "alice");
        let cache = LifecycleCache::default();

        let got = cache
            .resolve_assignees(&[key("a", run)], &ledger.store(), None, Instant::now())
            .await;

        assert_eq!(
            got.get("a").map(String::as_str),
            Some("alice"),
            "the run's own routing record outlives the run: {got:?}",
        );
    }

    // The row displays ONE run, and that run names it. The earlier run of the same ticket is not
    // consulted even though it is the same ticket — attribution is per-run, not per-ticket.
    #[tokio::test]
    async fn the_displayed_run_names_the_row() {
        let ledger = Ledger::new();
        let first = ledger.routed("MT-1", "alice");
        let second = ledger.routed("MT-1", "jimmy");
        let cache = LifecycleCache::default();

        let got = cache
            .resolve_assignees(&[key("a", second)], &ledger.store(), None, Instant::now())
            .await;
        assert_eq!(got.get("a").map(String::as_str), Some("jimmy"));

        let got = LifecycleCache::default()
            .resolve_assignees(&[key("a", first)], &ledger.store(), None, Instant::now())
            .await;
        assert_eq!(
            got.get("a").map(String::as_str),
            Some("alice"),
            "the older run still answers for itself: {got:?}",
        );
    }

    // THE ROUTE-BACK BLOCKER (acceptance 3). A ticket alice ran, re-run solo/unmatched: dispatch
    // records `teams.unrouted`, which is a NEWER row of a DIFFERENT kind, so nothing about the
    // ticket's `teams.route` history shadows it. The row must read "—", never "alice".
    //
    // Both halves are load-bearing. The run-scoped read is what stops run 1's route answering; the
    // `teams.unrouted` row being an ANSWER (not a miss) is what stops the still-present
    // `rhapsody:@alice` label answering in its place.
    #[tokio::test]
    async fn a_re_run_that_routed_to_nobody_does_not_inherit_the_earlier_teammate() {
        let ledger = Ledger::new();
        let _routed = ledger.routed("MT-1", "alice");
        let solo = ledger.unrouted("MT-1", "solo");
        let mut f = Fake::default();
        f.by_id
            .insert("a".into(), labelled("a", &["rhapsody:@alice"]));
        let tr = Arc::new(f);
        let cache = LifecycleCache::default();

        let got = cache
            .resolve_assignees(
                &[key("a", solo)],
                &ledger.store(),
                Some(Arc::clone(&tr) as Arc<dyn Tracker>),
                Instant::now(),
            )
            .await;

        assert!(
            got.is_empty(),
            "the displayed run routed to nobody; the previous run's teammate is not this run's: {got:?}",
        );
        assert_eq!(
            tr.labels_by_id_calls(),
            0,
            "an on-the-record `teams.unrouted` is an answer, not a gap for the label to fill",
        );
    }

    // The same blocker with Teams OFF for the re-run: `route_teams` returns before describing any
    // event, so run 2's ledger is silent rather than saying "nobody". The label is then the only
    // record left — and where there is none either, the row reads "—" instead of borrowing run 1's.
    #[tokio::test]
    async fn a_teams_off_re_run_does_not_inherit_the_earlier_teammate() {
        let ledger = Ledger::new();
        let _routed = ledger.routed("MT-1", "alice");
        let off = ledger.teams_off("MT-1");
        let tr = Arc::new(Fake::default());
        let cache = LifecycleCache::default();

        let got = cache
            .resolve_assignees(
                &[key("a", off)],
                &ledger.store(),
                Some(Arc::clone(&tr) as Arc<dyn Tracker>),
                Instant::now(),
            )
            .await;

        assert!(
            got.is_empty(),
            "a Teams-off run wore no identity, and the ticket's older route row is not its: {got:?}",
        );
    }

    // Acceptance 3: nothing routed, nothing labelled — the column stays "—" rather than guessing.
    #[tokio::test]
    async fn an_unrouted_ticket_answers_nobody() {
        let tr = Arc::new(Fake::default());
        let ledger = Ledger::new();
        let run = ledger.teams_off("MT-1");
        let cache = LifecycleCache::default();

        let got = cache
            .resolve_assignees(
                &[key("a", run)],
                &ledger.store(),
                Some(Arc::clone(&tr) as Arc<dyn Tracker>),
                Instant::now(),
            )
            .await;

        assert!(
            got.is_empty(),
            "no record anywhere must not invent one: {got:?}"
        );
    }

    // The label answers where the events row cannot: `storage.retention_days` deletes run history
    // long before a ticket stops being worth attributing.
    #[tokio::test]
    async fn the_label_answers_when_the_route_row_is_gone() {
        let mut f = Fake::default();
        f.by_id
            .insert("a".into(), labelled("a", &["rhapsody:@alice", "backend"]));
        let tr = Arc::new(f);
        let ledger = Ledger::new();
        let run = ledger.teams_off("MT-1");
        let cache = LifecycleCache::default();

        let got = cache
            .resolve_assignees(
                &[key("a", run)],
                &ledger.store(),
                Some(Arc::clone(&tr) as Arc<dyn Tracker>),
                Instant::now(),
            )
            .await;

        assert_eq!(got.get("a").map(String::as_str), Some("alice"));
        assert_eq!(tr.labels_by_id_calls(), 1);
    }

    // Preference order, and the reason for it: the label says who the ticket is assigned to NOW,
    // the route row says who actually ran it. A re-assignment must not rewrite history.
    #[tokio::test]
    async fn the_run_record_outranks_the_label_and_costs_no_tracker_call() {
        let mut f = Fake::default();
        f.by_id
            .insert("a".into(), labelled("a", &["rhapsody:@jimmy"]));
        let tr = Arc::new(f);
        let ledger = Ledger::new();
        let run = ledger.routed("MT-1", "alice");
        let cache = LifecycleCache::default();

        let got = cache
            .resolve_assignees(
                &[key("a", run)],
                &ledger.store(),
                Some(Arc::clone(&tr) as Arc<dyn Tracker>),
                Instant::now(),
            )
            .await;

        assert_eq!(got.get("a").map(String::as_str), Some("alice"));
        assert_eq!(
            tr.labels_by_id_calls(),
            0,
            "the tracker is asked only about what the local ledger could not answer",
        );
    }

    // Acceptance 3, the other half: `rhapsody:solo` is the dispatch opt-out and outranks every
    // label beside it, so a solo ticket has no assignee however it is otherwise labelled.
    #[tokio::test]
    async fn a_solo_ticket_answers_nobody_even_wearing_an_identity_label() {
        let mut f = Fake::default();
        f.by_id.insert(
            "a".into(),
            labelled("a", &["rhapsody:@alice", "rhapsody:solo"]),
        );
        let tr = Arc::new(f);
        let ledger = Ledger::new();
        let run = ledger.teams_off("MT-1");
        let cache = LifecycleCache::default();

        let got = cache
            .resolve_assignees(
                &[key("a", run)],
                &ledger.store(),
                Some(Arc::clone(&tr) as Arc<dyn Tracker>),
                Instant::now(),
            )
            .await;

        assert!(got.is_empty(), "the solo opt-out is absolute: {got:?}");
    }

    // The memo answers PER RUN. A cached answer for the previous run is not a stale answer to this
    // row's question, it is an answer to a different one — so a new run re-resolves even inside the
    // TTL, and the old name is not served in the meantime.
    #[tokio::test]
    async fn a_new_run_of_the_same_ticket_does_not_wear_the_previous_run_s_answer() {
        let ledger = Ledger::new();
        let first = ledger.routed("MT-1", "alice");
        let cache = LifecycleCache::default();
        let t0 = Instant::now();

        let got = cache
            .resolve_assignees(&[key("a", first)], &ledger.store(), None, t0)
            .await;
        assert_eq!(got.get("a").map(String::as_str), Some("alice"));

        // Well inside the TTL, and with the refresh unable to answer (no tracker, and the new run
        // recorded nothing): the memo must NOT serve alice as its "beats no answer" fallback.
        let second = ledger.teams_off("MT-1");
        let got = cache
            .resolve_assignees(
                &[key("a", second)],
                &ledger.store(),
                None,
                t0 + LIFECYCLE_TTL / 2,
            )
            .await;
        assert!(
            got.is_empty(),
            "the memo answered the previous run's question, not this one: {got:?}",
        );
    }

    // "Nobody" is cached even when there was no tracker to ask — before the first config load,
    // there is nothing that could have answered, so "no answer" is the complete answer for the
    // window rather than a gap to re-derive. Without this the store loop ran again on every
    // dashboard load until a config landed.
    #[tokio::test]
    async fn the_negative_caches_even_with_no_tracker_to_ask() {
        let ledger = Ledger::new();
        let run = ledger.teams_off("MT-1");
        let mut f = Fake::default();
        f.by_id
            .insert("a".into(), labelled("a", &["rhapsody:@alice"]));
        let tr = Arc::new(f);
        let cache = LifecycleCache::default();
        let keys = [key("a", run)];
        let store = ledger.store();
        let t0 = Instant::now();

        assert!(
            cache
                .resolve_assignees(&keys, &store, None, t0)
                .await
                .is_empty(),
            "no ledger row and no tracker => no answer",
        );

        // A config lands inside the window. The cached negative stands for the rest of it — the
        // TTL's ordinary contract — and costs no round trip.
        let after = cache
            .resolve_assignees(
                &keys,
                &store,
                Some(Arc::clone(&tr) as Arc<dyn Tracker>),
                t0 + LIFECYCLE_TTL / 2,
            )
            .await;
        assert!(after.is_empty(), "the negative is cached: {after:?}");
        assert_eq!(tr.labels_by_id_calls(), 0, "and it costs no round trip");

        let past = cache
            .resolve_assignees(
                &keys,
                &store,
                Some(Arc::clone(&tr) as Arc<dyn Tracker>),
                t0 + LIFECYCLE_TTL,
            )
            .await;
        assert_eq!(past.get("a").map(String::as_str), Some("alice"));
        assert_eq!(tr.labels_by_id_calls(), 1, "past the TTL it asks");
    }

    // Two identity labels resolve deterministically, whatever order the tracker lists them in — a
    // column that flickered between loads would be worse than one that said nothing.
    #[test]
    fn two_identity_labels_resolve_the_same_way_whatever_the_order() {
        let one = labelled("a", &["rhapsody:@jimmy", "rhapsody:@alice"]);
        let other = labelled("a", &["rhapsody:@alice", "rhapsody:@jimmy"]);
        assert_eq!(label_identity(&one), Some("alice".to_string()));
        assert_eq!(label_identity(&other), label_identity(&one));
        assert_eq!(label_identity(&labelled("a", &["rhapsody:@"])), None);
        assert_eq!(label_identity(&labelled("a", &["backend"])), None);
    }

    // Acceptance 4: the decoration is TTL-cached, so a dashboard reloading inside the window costs
    // neither a tracker call nor a fresh store read — including for the tickets that answered
    // NOBODY, which are the ones a naive cache would re-ask about forever.
    #[tokio::test]
    async fn a_second_read_inside_the_ttl_asks_nothing_even_about_the_unassigned() {
        let mut f = Fake::default();
        f.by_id
            .insert("a".into(), labelled("a", &["rhapsody:@alice"]));
        let tr = Arc::new(f);
        let cache = LifecycleCache::default();
        let keys = [key("a", 1), key("b", 2)];
        let target = || Some(Arc::clone(&tr) as Arc<dyn Tracker>);
        let store = empty_store();
        let t0 = Instant::now();

        let first = cache.resolve_assignees(&keys, &store, target(), t0).await;
        assert_eq!(first.get("a").map(String::as_str), Some("alice"));
        assert!(!first.contains_key("b"));
        assert_eq!(tr.labels_by_id_calls(), 1);

        let second = cache
            .resolve_assignees(&keys, &store, target(), t0 + LIFECYCLE_TTL / 2)
            .await;
        assert_eq!(second.get("a").map(String::as_str), Some("alice"));
        assert_eq!(
            tr.labels_by_id_calls(),
            1,
            "a fresh entry must not re-query"
        );

        let third = cache
            .resolve_assignees(&keys, &store, target(), t0 + LIFECYCLE_TTL)
            .await;
        assert_eq!(third.get("a").map(String::as_str), Some("alice"));
        assert_eq!(tr.labels_by_id_calls(), 2, "past the TTL it re-queries");
    }

    // A failed round-trip must not blank a column the console already had right. The refresh stops
    // and the cached answer stands, exactly as the lifecycle refresh behaves.
    #[tokio::test]
    async fn a_failed_label_lookup_keeps_the_assignee_it_already_had() {
        let mut f = Fake::default();
        f.by_id
            .insert("a".into(), labelled("a", &["rhapsody:@alice"]));
        let tr = Arc::new(f);
        let cache = LifecycleCache::default();
        let keys = [key("a", 1)];
        let store = empty_store();
        let t0 = Instant::now();

        assert_eq!(
            cache
                .resolve_assignees(&keys, &store, Some(Arc::clone(&tr) as Arc<dyn Tracker>), t0)
                .await
                .get("a")
                .map(String::as_str),
            Some("alice"),
        );

        let mut broken = Fake::default();
        broken.labels_by_id_err = Some(rhapsody_tracker::TrackerError::Other("linear down".into()));
        let after = cache
            .resolve_assignees(
                &keys,
                &store,
                Some(Arc::new(broken) as Arc<dyn Tracker>),
                t0 + LIFECYCLE_TTL,
            )
            .await;

        assert_eq!(
            after.get("a").map(String::as_str),
            Some("alice"),
            "a stale answer beats no answer when the lookup itself failed: {after:?}",
        );
    }

    // The same bounds the lifecycle refresh carries: blank ids are dropped, a repeat is one lookup,
    // and one request can only provoke so much work.
    #[tokio::test]
    async fn the_assignee_refresh_dedupes_drops_blank_ids_and_caps_the_batch() {
        let mut f = Fake::default();
        for i in 0..MAX_LIFECYCLE_REFRESH + 50 {
            let id = format!("i{i}");
            f.by_id
                .insert(id.clone(), labelled(&id, &["rhapsody:@alice"]));
        }
        let tr = Arc::new(f);
        let cache = LifecycleCache::default();
        let store = empty_store();

        let dupes = [key("i0", 1), key("i0", 1), key("", 2)];
        let got = cache
            .resolve_assignees(
                &dupes,
                &store,
                Some(Arc::clone(&tr) as Arc<dyn Tracker>),
                Instant::now(),
            )
            .await;
        assert_eq!(got.len(), 1, "a blank id has nothing to key an answer by");
        assert_eq!(tr.labels_by_id_calls(), 1);

        // The per-request ceiling, on a cache of its own: ids past it keep whatever they had (here,
        // nothing), which reads as "no answer" and falls back to the live roster.
        let many: Vec<IssueKey> = (0..MAX_LIFECYCLE_REFRESH + 50)
            .map(|i| key(&format!("i{i}"), i as i64 + 1))
            .collect();
        let got = LifecycleCache::default()
            .resolve_assignees(
                &many,
                &store,
                Some(Arc::clone(&tr) as Arc<dyn Tracker>),
                Instant::now(),
            )
            .await;
        assert_eq!(got.len(), MAX_LIFECYCLE_REFRESH);
    }

    // The memo is bounded exactly like the lifecycle memo — same rule, same order.
    #[test]
    fn prune_assignees_drops_the_expired_first_and_only_then_gives_up() {
        let t0 = Instant::now();
        let now = t0 + LIFECYCLE_TTL * 2;
        let entry = |at| AssigneeEntry {
            name: "alice".into(),
            run_id: 1,
            at,
        };

        let mut small: HashMap<String, AssigneeEntry> =
            (0..8).map(|i| (format!("i{i}"), entry(t0))).collect();
        prune_assignees(&mut small, now);
        assert_eq!(small.len(), 8, "a small memo is left alone");

        let mut mixed: HashMap<String, AssigneeEntry> = (0..MAX_CACHE_ENTRIES + 10)
            .map(|i| (format!("i{i}"), entry(if i < 20 { now } else { t0 })))
            .collect();
        prune_assignees(&mut mixed, now);
        assert_eq!(mixed.len(), 20, "only the fresh entries survive");

        let mut fresh: HashMap<String, AssigneeEntry> = (0..MAX_CACHE_ENTRIES + 10)
            .map(|i| (format!("i{i}"), entry(now)))
            .collect();
        prune_assignees(&mut fresh, now);
        assert!(
            fresh.is_empty(),
            "an unprunable memo is dropped, never grown"
        );
    }

    // The handle's own wiring: the store is the one the orchestrator holds, and the tracker is the
    // hot-reloaded account client — so the surface answers before any config has loaded (from the
    // store alone) and gains the label fallback once one has.
    #[tokio::test]
    async fn the_handle_answers_from_the_store_before_a_config_has_loaded() {
        let (o, store) = crate::testsupport::orch_with_store();
        let run = store
            .start_run(rhapsody_store::RunStart {
                issue_identifier: "MT-1".into(),
                ..rhapsody_store::RunStart::default()
            })
            .expect("start run");
        store
            .append_events(
                run,
                &[rhapsody_store::EventRow {
                    seq: 1,
                    at: "2026-09-02T00:00:00Z".into(),
                    kind: crate::teams::EVENT_ROUTE.into(),
                    tool: String::new(),
                    text: "identity=alice reason=label".into(),
                }],
            )
            .expect("append route event");

        let got = o
            .control()
            .issue_assignees(&[key("a", run), key("b", run + 1)])
            .await;

        assert_eq!(got.get("a").map(String::as_str), Some("alice"));
        assert!(!got.contains_key("b"));
    }
}
