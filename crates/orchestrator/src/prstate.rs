//! prstate — the off-loop sweep that asks GitHub where a set of pull requests stands (STUDIO-710,
//! slice 1 of the ticketless PR-review subsystem; design record
//! `~/.rhapsody/docs/STUDIO-703-ticketless-pr-review.md`, §14.4).
//!
//! **No Go v0.4.0 counterpart** — ticketless review is a Rhapsody addition end to end, and this
//! module is its foundation: [`crate::ghsummons::PrStateSource`] answers for ONE pull request, and
//! this is the shape in which many are asked.
//!
//! # Why the sweep exists at all, rather than the watcher just calling the primitive
//!
//! [`crate::ghsummons::GH`] shells out through a synchronous `std::process::Command`. Its future
//! has no await point, so it runs to completion in its first poll: a `tokio::time::timeout` around
//! it cannot cancel it, and whatever task drives it is blocked for the whole round-trip. N pull
//! requests in review would therefore be N serial blocking round-trips on whichever task asked —
//! and on the control task that would be a daemon that stops dispatching every time GitHub is slow.
//!
//! The containment is structural rather than temporal, exactly as [`crate::quorum`]'s is: this
//! function takes no `Orchestrator`, sends no control event and holds no lock the control task
//! takes, so a later slice drives it from its own spawned task and a stalled `gh` parks that task
//! and nothing else. `pr_state_is_never_called_from_the_control_loop` is the standing check that it
//! stays that way as the subsystem grows.
//!
//! Two bounds make the sweep safe to run on a timer:
//!
//! * [`PR_STATE_POLL_INTERVAL`] — the pinned cadence, so the cost is a known rate rather than a
//!   function of how busy the team is.
//! * [`MAX_PR_STATE_CALLS_PER_TICK`] — the per-tick call budget, so one large watch set cannot turn
//!   a single tick into an unbounded run of blocking round-trips. The remainder is not dropped, it
//!   is reported as [`PrSweep::deferred`] and picked up next tick.
//!
//! # Teams-gating (§16)
//!
//! The whole subsystem must be dormant unless Teams is on, so [`sweep_pr_states`] answers an empty
//! sweep — having spawned no process — when it is not. Defence in depth: a later slice gates the
//! spawn of its task too, and its watch set can only fill from the Teams handoff path. The gate
//! grows the `review.mode == ticketless` half in slice 7, which is the slice that adds the config
//! key; until then `teams.enabled` is the whole of it and nothing constructs a sweep in any case.

use std::time::Duration;

use rhapsody_config::teams::Teams;

use crate::control_loop::CancelWait;
use crate::ghsummons::{HeadAllowlist, PrLookup, PrStateSource};

/// How often a watched pull request is re-asked about.
///
/// Two minutes is chosen against what is actually waiting on it: the answer drives a re-review of
/// an author's pushed fixes, and a review run takes minutes, so shaving the detection latency below
/// a couple of minutes buys nothing anybody can perceive. Against GitHub's 5,000-request hourly
/// budget for an authenticated account it is deliberately cheap — a full budget every tick is 600
/// requests an hour, roughly a tenth — because this daemon shares that budget with the summons
/// enrichment poll, the quorum's PR lookups and every `gh` call an agent makes inside a run.
pub const PR_STATE_POLL_INTERVAL: Duration = Duration::from_secs(120);

/// How many pull requests ONE tick will ask about, the blast-radius bound on a blocking round-trip
/// per call. Twenty is above any plausible number of simultaneously in-review pull requests for one
/// team and far below the point where a tick could outlast its own cadence; the remainder is
/// deferred to the next tick rather than dropped, so nothing is skipped — only spread.
pub const MAX_PR_STATE_CALLS_PER_TICK: usize = 20;

/// Whether the PR-state poll may run at all: Teams enabled (§16's master gate).
///
/// A free function over the config rather than a method on the orchestrator, because the sweep runs
/// where no `Orchestrator` is reachable — which is the point of it.
pub fn pr_state_polling_enabled(teams: &Teams) -> bool {
    teams.enabled
}

/// A pull request named the only way GitHub can answer for it: by repository and number.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PrCoord {
    pub owner: String,
    pub repo: String,
    pub number: i64,
}

impl PrCoord {
    pub fn new(owner: &str, repo: &str, number: i64) -> PrCoord {
        PrCoord {
            owner: owner.to_string(),
            repo: repo.to_string(),
            number,
        }
    }
}

impl std::fmt::Display for PrCoord {
    /// `owner/repo#number` — the form the room, the logs and the design record all use.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}#{}", self.owner, self.repo, self.number)
    }
}

/// One pull request the sweep got an ANSWER for. A lookup that failed is not here — it is counted
/// in [`PrSweep::failed`], because a caller must not read the absence of an observation as a state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrObservation {
    pub pr: PrCoord,
    pub lookup: PrLookup,
}

/// What one tick learned. `deferred` and `failed` are reported rather than logged-and-forgotten so
/// the caller can tell "every watched PR is up to date" from "we ran out of budget" and from
/// "GitHub would not answer" — three situations that look identical in a bare list of observations.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PrSweep {
    /// One entry per pull request that answered, in the order asked.
    pub observed: Vec<PrObservation>,
    /// Pull requests the per-tick budget (or a cancellation) did not reach this tick.
    pub deferred: usize,
    /// Lookups that could not be made. Warned about here and left for the next tick; a failure is
    /// never an answer, and in particular never [`PrLookup::Gone`].
    pub failed: usize,
}

/// Asks GitHub about up to [`MAX_PR_STATE_CALLS_PER_TICK`] of `prs`, off the control loop.
///
/// Serially, deliberately: the calls block, so running them concurrently would occupy several
/// runtime workers at once and multiply the rate-limit pressure of a subsystem nobody is waiting
/// on. Confining them to one non-control task is what makes serial acceptable.
///
/// Cancellation is checked between calls (a blocking call cannot be interrupted mid-flight), so a
/// shutting-down daemon stops after at most one more round-trip and reports the rest as deferred.
pub async fn sweep_pr_states(
    ctx: &CancelWait,
    teams: &Teams,
    src: &dyn PrStateSource,
    allow: &HeadAllowlist,
    prs: &[PrCoord],
) -> PrSweep {
    // §16: dormant and side-effect-free when Teams is off — no process spawned, nothing observed.
    if !pr_state_polling_enabled(teams) {
        return PrSweep::default();
    }
    let mut sweep = PrSweep {
        deferred: prs.len().saturating_sub(MAX_PR_STATE_CALLS_PER_TICK),
        ..PrSweep::default()
    };
    for (asked, pr) in prs.iter().take(MAX_PR_STATE_CALLS_PER_TICK).enumerate() {
        if ctx.is_cancelled() {
            sweep.deferred += MAX_PR_STATE_CALLS_PER_TICK.min(prs.len()) - asked;
            return sweep;
        }
        match src.pr_state(&pr.owner, &pr.repo, pr.number, allow).await {
            Ok(lookup) => sweep.observed.push(PrObservation {
                pr: pr.clone(),
                lookup,
            }),
            Err(e) => {
                sweep.failed += 1;
                tracing::warn!(
                    pr = %pr,
                    error = %e,
                    "pr-state lookup failed; the pull request stays watched and is re-asked next tick"
                );
            }
        }
    }
    sweep
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;

    use super::*;
    use crate::control_loop::CancelSignal;
    use crate::ghsummons::{PrSnapshot, PrStateResult, PrStatus};

    /// Teams on, roster empty — everything this slice gates on.
    fn teams_on() -> Teams {
        Teams {
            enabled: true,
            ..Teams::disabled()
        }
    }

    /// A [`PrStateSource`] answering every lookup the same way, recording what it was asked.
    struct FakeSource {
        answer: Box<dyn Fn(i64) -> PrStateResult + Send + Sync>,
        seen: Arc<Mutex<Vec<String>>>,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl PrStateSource for FakeSource {
        async fn pr_state(
            &self,
            owner: &str,
            repo: &str,
            number: i64,
            _allow: &HeadAllowlist,
        ) -> PrStateResult {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.seen
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(format!("{owner}/{repo}#{number}"));
            (self.answer)(number)
        }
    }

    /// A source that answers `Found` at an open head for every PR, plus its call log.
    fn ok_source() -> (FakeSource, Arc<Mutex<Vec<String>>>, Arc<AtomicUsize>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let calls = Arc::new(AtomicUsize::new(0));
        (
            FakeSource {
                answer: Box::new(|n| {
                    Ok(PrLookup::Found(PrSnapshot {
                        head_sha: format!("sha{n}"),
                        status: PrStatus::Open,
                        merged_at: None,
                        head_repo: "o/r".to_string(),
                    }))
                }),
                seen: Arc::clone(&seen),
                calls: Arc::clone(&calls),
            },
            seen,
            calls,
        )
    }

    fn coords(n: i64) -> Vec<PrCoord> {
        (1..=n).map(|i| PrCoord::new("o", "r", i)).collect()
    }

    /// §16, the invariant every slice carries: with Teams off the sweep spawns NO process and
    /// observes nothing. Not "returns early after asking" — asking is the side effect.
    #[tokio::test]
    async fn sweep_is_dormant_when_teams_is_off() {
        let (src, seen, calls) = ok_source();
        let sweep = sweep_pr_states(
            &CancelWait::default(),
            &Teams::disabled(),
            &src,
            &HeadAllowlist::none(),
            &coords(3),
        )
        .await;

        assert_eq!(sweep, PrSweep::default());
        assert_eq!(calls.load(Ordering::SeqCst), 0, "no gh lookup may be made");
        assert!(seen.lock().unwrap_or_else(|e| e.into_inner()).is_empty());
    }

    /// The happy path: every watched pull request is asked about exactly once, by number, and the
    /// answers come back in the order asked.
    #[tokio::test]
    async fn sweep_asks_once_per_pull_request() {
        let (src, seen, calls) = ok_source();
        let sweep = sweep_pr_states(
            &CancelWait::default(),
            &teams_on(),
            &src,
            &HeadAllowlist::none(),
            &coords(3),
        )
        .await;

        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert_eq!(
            seen.lock().unwrap_or_else(|e| e.into_inner()).clone(),
            vec!["o/r#1", "o/r#2", "o/r#3"]
        );
        assert_eq!(sweep.observed.len(), 3);
        assert_eq!(sweep.deferred, 0);
        assert_eq!(sweep.failed, 0);
        assert_eq!(sweep.observed[1].pr, PrCoord::new("o", "r", 2));
    }

    /// The per-tick call budget is the bound that keeps one big watch set from turning a tick into
    /// an unbounded run of BLOCKING round-trips. The overflow is deferred, not dropped — a caller
    /// reading `deferred == 0` must be entitled to conclude the whole set was covered.
    #[tokio::test]
    async fn sweep_spends_at_most_the_per_tick_budget() {
        let over = MAX_PR_STATE_CALLS_PER_TICK as i64 + 5;
        let (src, _seen, calls) = ok_source();
        let sweep = sweep_pr_states(
            &CancelWait::default(),
            &teams_on(),
            &src,
            &HeadAllowlist::none(),
            &coords(over),
        )
        .await;

        assert_eq!(calls.load(Ordering::SeqCst), MAX_PR_STATE_CALLS_PER_TICK);
        assert_eq!(sweep.observed.len(), MAX_PR_STATE_CALLS_PER_TICK);
        assert_eq!(sweep.deferred, 5);
    }

    /// A failed lookup is counted, never observed, and never stops the sweep: one unreachable pull
    /// request must not cost every other watched one its tick. In particular it does not become
    /// `Gone`, which is what would retire it from review permanently.
    #[tokio::test]
    async fn sweep_counts_a_failure_without_dropping_the_rest() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let calls = Arc::new(AtomicUsize::new(0));
        let src = FakeSource {
            answer: Box::new(|n| {
                if n == 2 {
                    Err("gh: API rate limit exceeded".into())
                } else {
                    Ok(PrLookup::Gone)
                }
            }),
            seen: Arc::clone(&seen),
            calls: Arc::clone(&calls),
        };

        let sweep = sweep_pr_states(
            &CancelWait::default(),
            &teams_on(),
            &src,
            &HeadAllowlist::none(),
            &coords(3),
        )
        .await;

        assert_eq!(calls.load(Ordering::SeqCst), 3, "the sweep runs to the end");
        assert_eq!(sweep.failed, 1);
        assert_eq!(
            sweep
                .observed
                .iter()
                .map(|o| o.pr.number)
                .collect::<Vec<_>>(),
            vec![1, 3],
            "the failed lookup yields no observation at all"
        );
    }

    /// A shutting-down daemon stops asking. The calls block, so the check is between them: an
    /// already-cancelled sweep makes no call at all, and everything unasked is reported deferred
    /// rather than silently lost.
    #[tokio::test]
    async fn sweep_stops_on_cancellation_and_defers_the_rest() {
        let signal = CancelSignal::new();
        signal.cancel();
        let (src, _seen, calls) = ok_source();

        let sweep = sweep_pr_states(
            &signal.wait(),
            &teams_on(),
            &src,
            &HeadAllowlist::none(),
            &coords(3),
        )
        .await;

        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(sweep.observed.is_empty());
        assert_eq!(sweep.deferred, 3, "nothing asked is nothing lost");
    }

    /// The workspace root (two levels up from `crates/orchestrator`).
    fn workspace_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    /// Every `.rs` file under `dir`, recursively.
    fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                rust_sources(&p, out);
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push(p);
            }
        }
    }

    /// The modules entitled to call the primitive: this one, and the module that defines it. A
    /// later slice's watcher adds its own off-loop module here — and the point of the list is that
    /// doing so is a deliberate edit a reviewer sees, rather than a call site nobody notices.
    const OFF_LOOP_CALLERS: &[&str] = &["prstate.rs", "ghsummons.rs"];

    /// The control task's own modules, named so that widening [`OFF_LOOP_CALLERS`] to include one
    /// still fails. `loop.rs` IS the control loop; `dispatch.rs`, `select.rs` and `orchestrator.rs`
    /// run on it.
    const CONTROL_LOOP_MODULES: &[&str] =
        &["loop.rs", "dispatch.rs", "select.rs", "orchestrator.rs"];

    /// The architecture check (design record §14.2): the PR-state lookup shells out through a
    /// blocking `std::process::Command`, so a call site on the control task is a daemon that stops
    /// dispatching for as long as GitHub is slow — with N watched pull requests, N times over.
    ///
    /// This asserts on CALL SITES across the whole workspace rather than on a type, because the
    /// mistake it guards against is a later slice reaching for a convenient `self.pr_source` from
    /// an `Orchestrator` method, which no signature forbids.
    #[test]
    fn pr_state_is_never_called_from_the_control_loop() {
        let mut files = Vec::new();
        rust_sources(&workspace_root().join("crates"), &mut files);
        assert!(files.len() > 50, "the source scan found nothing to scan");

        let mut call_sites: Vec<String> = Vec::new();
        for f in &files {
            let Ok(text) = std::fs::read_to_string(f) else {
                continue;
            };
            if !text.contains(".pr_state(") && !text.contains("sweep_pr_states(") {
                continue;
            }
            let name = f
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_string();
            assert!(
                !CONTROL_LOOP_MODULES.contains(&name.as_str()),
                "{name} runs on the control task and must not call the blocking PR-state lookup"
            );
            assert!(
                OFF_LOOP_CALLERS.contains(&name.as_str()),
                "{name} calls the PR-state lookup but is not a known off-loop module; \
                 confirm it cannot run on the control task, then add it to OFF_LOOP_CALLERS"
            );
            call_sites.push(name);
        }
        assert!(
            call_sites.iter().any(|n| n == "prstate.rs"),
            "the check found no call site at all — it has stopped testing anything"
        );
    }
}
