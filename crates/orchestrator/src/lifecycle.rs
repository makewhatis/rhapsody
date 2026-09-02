//! lifecycle — the ticket's CURRENT tracker state, for the read path (STUDIO-702).
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
#[derive(Default)]
pub struct LifecycleCache {
    entries: Mutex<HashMap<String, Entry>>,
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
                match row.clone() {
                    // A refreshed answer replaces whatever stale one `partition` handed back.
                    Some(row) => {
                        out.insert(id.clone(), row);
                    }
                    // The tracker no longer knows this id: drop the stale answer rather than keep
                    // reporting a state nothing confirms.
                    None => {
                        out.remove(id);
                    }
                }
                guard.insert(id.clone(), Entry { row, at: now });
            }
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
}
