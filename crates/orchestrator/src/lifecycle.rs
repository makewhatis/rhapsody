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
//! Two durable records answer it, in strict preference order:
//!
//!   1. **The run's own routing decision**, the `teams.route` events row every routed dispatch
//!      writes (`crate::teams::EVENT_ROUTE`). It is a fact about the RUN — this run wore this
//!      identity — so it is right even after a roster change, a re-label, or a re-assignment, and
//!      it is local, needing no tracker at all.
//!   2. **The `rhapsody:@<name>` label**, which IS the assignment (design record
//!      `~/.rhapsody/docs/STUDIO-572-rhapsody-teams.md`, §0.11.1). It answers where the events row
//!      does not: `storage.retention_days` prunes run history long before a ticket stops being
//!      interesting, and the label outlives the prune. Read with
//!      [`Tracker::fetch_issue_labels_by_ids`], which answers for a MERGED ticket — the case the
//!      whole decoration exists for.
//!
//! Both are silent about a ticket nobody was routed for: a solo (`rhapsody:solo`) or Teams-off run
//! writes no `teams.route` row and carries no identity label, so it answers "" and the column keeps
//! rendering "—". Nothing here consults the live roster, and nothing here can invent an assignee for
//! a run that had none.

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

/// One history row's ticket coordinates. Both halves are needed and neither substitutes for the
/// other: the tracker is queried by opaque `id`, while the store's own event ledger joins runs on
/// the human `identifier`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IssueKey {
    /// The tracker's opaque issue id — the key every resolved map is keyed by, matching the
    /// `issue_id` on the history row being decorated.
    pub id: String,
    /// The human identifier (`"STUDIO-735"`), as stored on the run row.
    pub identifier: String,
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
/// One cached assignee. An EMPTY `name` records "nobody was routed for this ticket" and is cached
/// exactly like a hit, so a solo or Teams-off ticket is not re-queried on every dashboard load.
struct AssigneeEntry {
    name: String,
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
    /// The two sources are consulted in the module doc's preference order and the second is only
    /// asked about what the first could not answer:
    ///
    ///   1. `store` — the ticket's newest `teams.route` events row. Local, and a fact about the RUN
    ///      rather than about the ticket's labels today.
    ///   2. `tracker` — the `rhapsody:@<name>` label, for the tickets whose route rows the retention
    ///      prune has already deleted. Skipped entirely when there is nothing left to ask about, so
    ///      a healthy Teams deployment pays no tracker call here at all.
    ///
    /// Best-effort throughout, exactly like [`Self::resolve`]: a store error, an absent tracker or a
    /// failed round-trip leaves a ticket unanswered rather than propagating, because the listing
    /// this decorates has already succeeded and no caller could act on the failure.
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
        let mut unanswered: Vec<IssueKey> = Vec::new();
        for key in &stale {
            match route_identity(store.as_ref(), &key.identifier) {
                Some(name) => {
                    answers.insert(key.id.clone(), name);
                }
                None => unanswered.push(key.clone()),
            }
        }
        // Then the label, for whatever is left — and `covered` is which of those the tracker
        // actually answered about, so a failed round-trip leaves its ids untouched instead of
        // caching "nobody" over an assignee the console already had.
        let mut covered: HashSet<String> = answers.keys().cloned().collect();
        if let Some(tracker) = tracker.filter(|_| !unanswered.is_empty()) {
            let (labelled, asked) = label_identities(tracker.as_ref(), &unanswered).await;
            answers.extend(labelled);
            covered.extend(asked);
        }
        let mut guard = self
            .assignees
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        for key in stale.iter().filter(|k| covered.contains(&k.id)) {
            // Caching the EMPTY name is deliberate: it is what stops a solo, unrouted or Teams-off
            // ticket being asked about again on every dashboard load.
            let name = answers.get(&key.id).cloned().unwrap_or_default();
            match name.is_empty() {
                true => out.remove(&key.id),
                false => out.insert(key.id.clone(), name.clone()),
            };
            guard.insert(key.id.clone(), AssigneeEntry { name, at: now });
        }
        prune_assignees(&mut guard, now);
        out
    }

    /// The assignee half of [`Self::partition`], and it follows the same two rules: a stale answer
    /// is returned AS WELL AS re-queried (it is what the caller gets when the refresh fails), and
    /// the refresh set is deduplicated and capped at [`MAX_LIFECYCLE_REFRESH`].
    ///
    /// A key with no `identifier` is still refreshable — the label lookup goes by `id` — but a key
    /// with no `id` is dropped: there is nothing to key the answer by.
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
            let entry = guard.get(&key.id);
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

/// The identity the ticket's NEWEST run wore, from the store's own event ledger — `teams.route`'s
/// `identity=<name> reason=<why>` text, parsed by [`crate::triage::route_event_identity`].
///
/// `limit: 1` with the query's `run_id DESC` ordering is what makes it the newest run's: the Jobs
/// row shows that run, so the assignee shown must be the one that ran it. A blank identifier has
/// nothing to look up, and a store error answers `None` — "ask the label instead" — rather than
/// propagating.
fn route_identity(
    store: &(dyn rhapsody_store::Store + Send + Sync),
    identifier: &str,
) -> Option<String> {
    if identifier.is_empty() {
        return None;
    }
    let q = rhapsody_store::EventQuery {
        text: String::new(),
        issue: identifier.to_string(),
        kind: crate::teams::EVENT_ROUTE.to_string(),
        limit: 1,
    };
    match store.search_events(q) {
        Ok(hits) => hits
            .first()
            .and_then(|h| crate::triage::route_event_identity(&h.text)),
        Err(err) => {
            tracing::warn!(
                issue = %identifier,
                error = %err,
                "assignee lookup could not read the run history; falling back to the ticket label",
            );
            None
        }
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

    /// An in-memory store carrying one FINISHED run per (identifier, identity) pair, each with the
    /// `teams.route` row a routed dispatch writes. Runs are inserted in order, so the LAST entry for
    /// an identifier is its newest run.
    fn store_with_routes(routes: &[(&str, &str)]) -> Arc<dyn rhapsody_store::Store + Send + Sync> {
        let store: Arc<dyn rhapsody_store::Store + Send + Sync> = Arc::new(
            rhapsody_store::Sqlite::open(rhapsody_store::StorePath::InMemory).expect("open store"),
        );
        for (identifier, identity) in routes {
            let run = store
                .start_run(rhapsody_store::RunStart {
                    issue_identifier: (*identifier).to_string(),
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
                        text: format!("identity={identity} reason=label"),
                    }],
                )
                .expect("append route event");
            store
                .end_run(run, rhapsody_store::RunEnd::default())
                .expect("end run");
        }
        store
    }

    fn empty_store() -> Arc<dyn rhapsody_store::Store + Send + Sync> {
        store_with_routes(&[])
    }

    fn key(id: &str, identifier: &str) -> IssueKey {
        IssueKey {
            id: id.to_string(),
            identifier: identifier.to_string(),
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
        let store = store_with_routes(&[("MT-1", "alice")]);
        let cache = LifecycleCache::default();

        let got = cache
            .resolve_assignees(&[key("a", "MT-1")], &store, None, Instant::now())
            .await;

        assert_eq!(
            got.get("a").map(String::as_str),
            Some("alice"),
            "the run's own routing record outlives the run: {got:?}",
        );
    }

    // The ticket's newest run is the one the Jobs row shows, so it is the one that names the row.
    #[tokio::test]
    async fn the_newest_run_names_the_ticket() {
        let store = store_with_routes(&[("MT-1", "alice"), ("MT-1", "jimmy")]);
        let cache = LifecycleCache::default();

        let got = cache
            .resolve_assignees(&[key("a", "MT-1")], &store, None, Instant::now())
            .await;

        assert_eq!(got.get("a").map(String::as_str), Some("jimmy"));
    }

    // Acceptance 3: nothing routed, nothing labelled — the column stays "—" rather than guessing.
    #[tokio::test]
    async fn an_unrouted_ticket_answers_nobody() {
        let tr = Arc::new(Fake::default());
        let cache = LifecycleCache::default();

        let got = cache
            .resolve_assignees(
                &[key("a", "MT-1")],
                &empty_store(),
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
        let cache = LifecycleCache::default();

        let got = cache
            .resolve_assignees(
                &[key("a", "MT-1")],
                &empty_store(),
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
        let cache = LifecycleCache::default();

        let got = cache
            .resolve_assignees(
                &[key("a", "MT-1")],
                &store_with_routes(&[("MT-1", "alice")]),
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
        let cache = LifecycleCache::default();

        let got = cache
            .resolve_assignees(
                &[key("a", "MT-1")],
                &empty_store(),
                Some(Arc::clone(&tr) as Arc<dyn Tracker>),
                Instant::now(),
            )
            .await;

        assert!(got.is_empty(), "the solo opt-out is absolute: {got:?}");
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
        let keys = [key("a", "MT-1"), key("b", "MT-2")];
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
        let keys = [key("a", "MT-1")];
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

        let dupes = [key("i0", "MT-1"), key("i0", "MT-1"), key("", "MT-2")];
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
            .map(|i| key(&format!("i{i}"), &format!("MT-{i}")))
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
            .issue_assignees(&[key("a", "MT-1"), key("b", "MT-2")])
            .await;

        assert_eq!(got.get("a").map(String::as_str), Some("alice"));
        assert!(!got.contains_key("b"));
    }
}
