//! managerrun — the manager RUN KIND (STUDIO-1049, split from STUDIO-1014's item 1; design record
//! `~/.rhapsody/docs/manager-agent-design.md` §4, §10.1, §15.4 "Startup boundary").
//!
//! A manager run is keyed `pr:<owner>/<repo>#<n>@manager` and is launched **through the existing
//! dispatch path**, exactly as a ticketless review is — no second path. It reuses review mode's
//! identity trick (`pr:` never collides with a tracker identifier) and the same synthetic-`Issue`
//! dispatch, but it is deliberately NOT a review:
//!
//! * it has **no watch-set row** and no reviewer, so nothing here writes `rhapsody_review_watch`;
//! * its key ends `@manager`, and `manager` is a RESERVED roster name (`rhapsody_config::room`), so
//!   no teammate's review key can ever collide with it — [`crate::review::Orchestrator::
//!   dispatch_review`] additionally refuses a reviewer literally named `manager`;
//! * it is reachable only from [`Orchestrator::dispatch_manager`], which is the manager's own launch
//!   (M8 is its first production caller).
//!
//! The isolated startup the launch rides on lives in `rhapsody_agent::manager` (the argv posture)
//! and the worker (the empty daemon-owned cwd, the dedicated config directory, the manager MCP
//! config file, the env scrub). This module only decides WHETHER and WITH WHAT key to dispatch; the
//! §4.7 self-test gate is consulted here — at every launch — via
//! [`Orchestrator::manager_launch_permitted`].

use rhapsody_core::Issue;

use rhapsody_config::teams::ReviewAuthority;

use crate::orchestrator::Orchestrator;
use crate::retry::DispatchRoute;

/// The manager role token that terminates a manager run's key (`pr:<owner>/<repo>#<n>@manager`). It
/// is also the reserved roster name (`rhapsody_config::room::MANAGER_IDENTITY`), which is what keeps
/// it out of every teammate's reach.
pub const MANAGER_KEY_SUFFIX: &str = "@manager";

/// The base task prompt a manager run's first turn carries. M8 supplies the real intervention case
/// packet; M7b lands the launch, and this prompt is the host's own instruction to the manager
/// identity (whose profile, policy and bank the dispatch other-wise attaches via the
/// `rhapsody:@manager` label). It contains no `{{ … }}` so the prompt renderer cannot fail on it.
pub const MANAGER_BASE_PROMPT: &str = "You are the manager for this pull request's review loop. \
Review the evidence the host serves you through your MCP tools, then end your final message with \
a `HANDOFF:` line describing the decision you reached.";

/// Pending manager runs staged by [`Orchestrator::dispatch_manager`] and consumed by the dispatch
/// funnel, keyed by the run's key.
pub type PendingManagers = std::collections::HashMap<String, ManagerRun>;

/// The checkout coordinates a manager launch carries onto its [`RunningEntry`](crate::orchestrator::RunningEntry)
/// and hands to the worker. Deliberately narrow: the worker provisions an EMPTY cwd and never a
/// repository, so all it needs is the run's key (to name the per-run directory) and the run timeout.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ManagerCheckout {
    /// The run's key (`pr:owner/repo#n@manager`), used to name the daemon-owned per-run directory.
    pub key: String,
    /// `manager.run_timeout_ms` (§10.1), the run's wall-clock ceiling.
    pub run_timeout_ms: i64,
}

/// The dispatch-time coordinates of one manager run: WHICH pull request it is convened for, and the
/// trusted repository origin its project route is resolved from.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ManagerRun {
    pub owner: String,
    pub repo: String,
    pub number: i64,
    /// The clone URL of the pull request's repository, from a TRUSTED origin (a handoff's resolved
    /// project binding, or the authenticated console) — never from room text. The dispatch refuses
    /// any URL no configured project owns, exactly as a review does.
    pub repo_url: String,
    /// The reviewer's team id, carried onto the synthetic issue. Empty is valid (a manager run is
    /// not a tracker issue).
    pub team_id: String,
}

impl ManagerRun {
    /// The run's issue id and identifier: `pr:owner/repo#number@manager`.
    pub(crate) fn key(&self) -> String {
        manager_key(&self.owner, &self.repo, self.number)
    }

    /// The worker-facing checkout coordinates.
    pub(crate) fn checkout(&self, run_timeout_ms: i64) -> ManagerCheckout {
        ManagerCheckout {
            key: self.key(),
            run_timeout_ms,
        }
    }

    /// Builds the synthetic [`Issue`] the dispatch path is typed against. Like a review's, its
    /// `id`/`identifier` are the key and `state` is empty (a `pr:` key resolves to no tracker
    /// issue). Its one label is `rhapsody:@manager`, which routing reads to attach the built-in
    /// manager identity, its profile and its own memory bank (M6, STUDIO-1013).
    pub(crate) fn synthetic_issue(&self) -> Issue {
        let key = self.key();
        Issue {
            id: key.clone(),
            title: format!(
                "Manager run for {}/{}#{}",
                self.owner, self.repo, self.number
            ),
            identifier: key,
            team_id: self.team_id.clone(),
            labels: Some(vec![format!("rhapsody:{MANAGER_KEY_SUFFIX}")]),
            ..Issue::default()
        }
    }
}

/// Formats a manager run's issue key: `pr:owner/repo#number@manager`.
pub fn manager_key(owner: &str, repo: &str, number: i64) -> String {
    format!(
        "{}{owner}/{repo}#{number}{MANAGER_KEY_SUFFIX}",
        crate::review::REVIEW_KEY_PREFIX
    )
}

/// Reports whether an issue id/identifier is a MANAGER run's key rather than a review's or a
/// tracker ticket's. A manager key is also a `pr:` key, so this must be checked BEFORE
/// [`crate::review::is_review_key`] wherever the two are told apart.
pub fn is_manager_key(id: &str) -> bool {
    id.ends_with(MANAGER_KEY_SUFFIX)
}

/// Why a manager dispatch was refused or deferred. Mirrors [`ReviewDispatchOutcome`](crate::review::ReviewDispatchOutcome)
/// in shape, minus the review-only arms (no watch row, no round budget).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManagerDispatchOutcome {
    /// The run was accepted and dispatched.
    Dispatched,
    /// Teams is off — the manager is a team identity and cannot run without it.
    TeamsOff,
    /// `manager.review_authority` is `off` (or a storage/CLI gate forced it off), so the manager
    /// does not act. Nothing was touched.
    AuthorityOff,
    /// The §4.7 self-test has not passed on the current CLI version (or could not be run). Carries
    /// the typed reason. Nothing was touched.
    SelfTestFailed(crate::managerselftest::ManagerUnavailable),
    /// A drain is armed; the daemon takes no new work.
    Draining,
    /// A manager run for this exact pull request is already running or claimed (the overwrite
    /// guard).
    AlreadyInFlight,
    /// The coordinates cannot produce a manager run; the payload names which.
    Refused(String),
}

impl Orchestrator {
    /// Dispatches one manager run, or refuses and explains why. This is the manager's own launch
    /// (design §10.1); M8 is its first production caller. It rides the SAME dispatch funnel a
    /// ticketless review does — [`Orchestrator::dispatch_issue_prepared`] — so there is no second
    /// dispatch path.
    ///
    /// Refusal is ordered so nothing observable happens before every check has passed: the Teams
    /// gate, then the authority, then the §4.7 self-test, then the drain gate, then the coordinates,
    /// then the routing, and only then the overwrite guard and the dispatch.
    pub fn dispatch_manager(&mut self, run: ManagerRun) -> ManagerDispatchOutcome {
        // The manager is a built-in TEAM identity (M6): with Teams off there is no manager to run.
        if !self.teams.as_ref().is_some_and(|t| t.enabled) {
            return ManagerDispatchOutcome::TeamsOff;
        }
        // §10.2: `review_authority: off` means the manager does not act. This is the config gate the
        // whole feature hangs on, and `off` must remain byte-identical — so it is checked first.
        if self.manager_review_authority() == ReviewAuthority::Off {
            return ManagerDispatchOutcome::AuthorityOff;
        }
        // §4.7/§10.2: the self-test must have passed on the CURRENT CLI version before the manager
        // acts again. Fail-closed — a missing, crashed or version-stale verdict refuses here.
        if let Err(reason) = self.manager_launch_permitted() {
            tracing::warn!(
                pr = %format!("{}/{}#{}", run.owner, run.repo, run.number),
                reason = %reason.message(),
                "manager run refused: the startup self-test has not passed on the current CLI"
            );
            return ManagerDispatchOutcome::SelfTestFailed(reason);
        }
        if self.drain.is_draining() {
            return ManagerDispatchOutcome::Draining;
        }
        if run.owner.is_empty() || run.repo.is_empty() {
            return ManagerDispatchOutcome::Refused("pull request has no owner/repo".to_string());
        }
        if run.number <= 0 {
            return ManagerDispatchOutcome::Refused(
                "pull-request number is not positive".to_string(),
            );
        }
        // A repo no configured project owns has no agent, no model and no workspace root to run
        // under — and refusing it also keeps a manager run confined to repositories this daemon is
        // configured for (the same trusted-origin property a review enforces).
        let Some(route) = self.review_route(&run.repo_url) else {
            return ManagerDispatchOutcome::Refused(
                "no configured project owns the PR's repo".to_string(),
            );
        };
        let id = run.key();
        // THE overwrite guard: never point a second agent at a live manager run's identity.
        if self.running.contains_key(&id) || self.claimed.contains(&id) {
            return ManagerDispatchOutcome::AlreadyInFlight;
        }
        let iss = run.synthetic_issue();
        self.finish_manager_dispatch(run, route, iss);
        ManagerDispatchOutcome::Dispatched
    }

    /// The tail of a manager dispatch: stage the coordinates for
    /// [`Orchestrator::dispatch_issue_prepared`] and dispatch the synthetic issue. Kept as its own
    /// function for the same reason [`Orchestrator::finish_review_dispatch`] is — the dispatch itself
    /// consumes `pending_manager` inside the funnel.
    pub(crate) fn finish_manager_dispatch(
        &mut self,
        run: ManagerRun,
        route: DispatchRoute,
        iss: Issue,
    ) {
        let id = run.key();
        self.pending_manager.insert(id, run);
        self.dispatch_issue_prepared(iss, None, Some(route), String::new(), None);
    }

    /// The exit path of a manager run (STUDIO-1049): its `pr:` key resolves to no tracker issue, so
    /// it ends exactly as a review does — recording the outcome and releasing the persisted claim —
    /// without the ticket classifier and without scheduling any retry. M8 owns the decision effects.
    pub(crate) fn on_manager_exit(
        &mut self,
        re: &crate::orchestrator::RunningEntry,
        e: &crate::retry::EvWorkerExit,
    ) {
        self.completed.remove(&re.issue.id);
        self.claimed.remove(&re.issue.id);
        let (outcome, reason) = if e.failed {
            (rhapsody_store::OUTCOME_FAILED, e.err_msg.as_str())
        } else {
            (rhapsody_store::OUTCOME_COMPLETED, "")
        };
        self.persist_end_run(re, outcome, reason);
        self.persist_complete(&re.issue.identifier);
        self.persist_totals();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use rhapsody_config::teams::{Identity, Manager, Review, ReviewAuthority, ReviewMode, Teams};
    use rhapsody_store::{Sqlite, StorePath};
    use rhapsody_tracker::fake::Fake;

    use super::*;
    use crate::managerselftest::{SelfTestRecord, SelfTestVerdict};
    use crate::testsupport::{DispatchedEntries, empty_effective, empty_resolved_project};

    const REPO_URL: &str = "git@github.com:makewhatis/rhapsody.git";

    fn record_entries(sink: &DispatchedEntries) -> crate::orchestrator::SpawnFn {
        let sink = Arc::clone(sink);
        Box::new(move |_iss, _attempt, re| {
            sink.lock().expect("dispatched lock").push(re.clone());
        })
    }

    /// An orchestrator with Teams ON + ticketless review mode (which is what makes
    /// `manager_review_authority()` non-`off`), one project owning [`REPO_URL`], an in-memory store,
    /// and a recording spawn seam.
    fn orch(authority: ReviewAuthority) -> (Orchestrator, DispatchedEntries) {
        let tracker = Arc::new(Fake::new());
        let mut eff = empty_effective(tracker.clone());
        eff.active_states = ["todo".to_string(), "in progress".to_string()]
            .into_iter()
            .collect();
        eff.terminal_states = ["done".to_string()].into_iter().collect();
        eff.max_concurrent = 10;
        let mut proj = empty_resolved_project("rhapsody", tracker);
        proj.repo = REPO_URL.to_string();
        eff.projects = vec![proj];
        let mut o = Orchestrator::new("WORKFLOW.md");
        o.eff = Some(eff);
        o.teams = Some(Teams {
            enabled: true,
            review: Review {
                mode: ReviewMode::Ticketless,
                ..Review::default()
            },
            roster: vec![Identity {
                name: "alice".to_string(),
                profile: "swe".to_string(),
                ..Default::default()
            }],
            manager: Manager {
                review_authority: authority,
                ..Default::default()
            },
            ..Teams::disabled()
        });
        o.set_store(Arc::new(
            Sqlite::open(StorePath::InMemory).expect("open in-memory store"),
        ));
        let dispatched: DispatchedEntries = Arc::new(Mutex::new(Vec::new()));
        o.spawn = Some(record_entries(&dispatched));
        (o, dispatched)
    }

    fn manager_run() -> ManagerRun {
        ManagerRun {
            owner: "makewhatis".to_string(),
            repo: "rhapsody".to_string(),
            number: 12,
            repo_url: REPO_URL.to_string(),
            team_id: String::new(),
        }
    }

    /// Mark the §4.7 self-test as passed on the installed version, so the launch gate opens.
    fn pass_self_test(o: &Orchestrator, version: &str) {
        o.manager_selftest.record(SelfTestRecord {
            cli_version: version.to_string(),
            verdict: SelfTestVerdict::Passed,
        });
    }

    // The key shape the manager run is addressed by, and the property that distinguishes it from a
    // review key and a tracker identifier.
    #[test]
    fn manager_key_shape_and_is_manager_key() {
        let key = manager_key("makewhatis", "rhapsody", 12);
        assert_eq!(key, "pr:makewhatis/rhapsody#12@manager");
        assert!(is_manager_key(&key));
        assert!(crate::review::is_review_key(&key));
        assert!(!is_manager_key("pr:makewhatis/rhapsody#12@alice"));
        assert!(!is_manager_key("MT-12"));
    }

    // `manager` is a reserved roster name; the review dispatch refuses it outright, so no teammate's
    // review key can ever end `@manager` and collide with the manager's own key.
    #[test]
    fn a_review_key_cannot_end_in_manager() {
        let (mut o, _) = orch(ReviewAuthority::Act);
        let mut run = crate::review::ReviewRun {
            owner: "makewhatis".to_string(),
            repo: "rhapsody".to_string(),
            number: 12,
            reviewer: "manager".to_string(),
            team_id: "team-1".to_string(),
            repo_url: REPO_URL.to_string(),
            head_sha: "a".repeat(40),
            ..crate::review::ReviewRun::default()
        };
        let outcome = o.dispatch_review(run.clone());
        assert!(
            matches!(outcome, crate::review::ReviewDispatchOutcome::Refused(_)),
            "a reviewer named `manager` must be refused, got {outcome:?}"
        );
        // …and the alias spelling too, since the reserved identity is `@manager`.
        run.reviewer = "@manager".to_string();
        assert!(matches!(
            o.dispatch_review(run),
            crate::review::ReviewDispatchOutcome::Refused(_)
        ));
    }

    // §10.2/§12: `review_authority: off` refuses before anything is touched.
    #[test]
    fn dispatch_refuses_when_authority_is_off() {
        let (mut o, dispatched) = orch(ReviewAuthority::Off);
        pass_self_test(&o, "1.0.0"); // even with a passing self-test, off means off
        assert_eq!(
            o.dispatch_manager(manager_run()),
            ManagerDispatchOutcome::AuthorityOff
        );
        assert!(dispatched.lock().expect("lock").is_empty());
        assert!(o.running.is_empty() && o.claimed.is_empty());
    }

    // §4.7/§10.2: the self-test gate is fail-closed — no recorded verdict, or a failed one, refuses.
    #[test]
    fn dispatch_refuses_when_the_self_test_has_not_passed() {
        let (mut o, dispatched) = orch(ReviewAuthority::Act);
        // No recorded verdict at all.
        assert!(matches!(
            o.dispatch_manager(manager_run()),
            ManagerDispatchOutcome::SelfTestFailed(_)
        ));
        // A FAILED verdict for the installed version also refuses.
        o.manager_selftest.record(SelfTestRecord {
            cli_version: "1.0.0".to_string(),
            verdict: SelfTestVerdict::Failed(crate::managerselftest::ManagerUnavailable {
                cli_version: "1.0.0".to_string(),
                detail: "Bash succeeded".to_string(),
            }),
        });
        assert!(matches!(
            o.dispatch_manager(manager_run()),
            ManagerDispatchOutcome::SelfTestFailed(_)
        ));
        assert!(dispatched.lock().expect("lock").is_empty());
    }

    // The happy path: the launch rides the SAME dispatch funnel a review does, and writes NO review
    // watch row (a manager is not a reviewer).
    #[test]
    fn dispatch_manager_rides_the_shared_funnel_with_no_watch_row() {
        let (mut o, dispatched) = orch(ReviewAuthority::Act);
        pass_self_test(&o, "1.0.0");
        assert_eq!(
            o.dispatch_manager(manager_run()),
            ManagerDispatchOutcome::Dispatched
        );
        {
            let d = dispatched.lock().expect("lock");
            assert_eq!(d.len(), 1, "exactly one manager run spawned");
            assert_eq!(d[0].issue.identifier, "pr:makewhatis/rhapsody#12@manager");
            // The label routes the run to the built-in manager identity.
            assert_eq!(
                d[0].issue.labels.as_deref(),
                Some(["rhapsody:@manager".to_string()].as_slice())
            );
        }
        assert!(
            o.pending_manager.is_empty(),
            "the pending manager run must be consumed by the dispatch"
        );
        // No watch row was written — a manager has no reviewer identity.
        let watch = o
            .store()
            .get_review_watch(&rhapsody_store::ReviewWatchKey {
                owner: "makewhatis".to_string(),
                repo: "rhapsody".to_string(),
                number: 12,
                reviewer: "manager".to_string(),
            })
            .expect("read watch");
        assert!(watch.is_none(), "a manager dispatch writes no watch row");
    }

    // The overwrite guard: a second launch for the same pull request while one is live is refused.
    #[test]
    fn dispatch_manager_refuses_an_already_in_flight_run() {
        let (mut o, dispatched) = orch(ReviewAuthority::Act);
        pass_self_test(&o, "1.0.0");
        assert_eq!(
            o.dispatch_manager(manager_run()),
            ManagerDispatchOutcome::Dispatched
        );
        assert_eq!(
            o.dispatch_manager(manager_run()),
            ManagerDispatchOutcome::AlreadyInFlight
        );
        assert_eq!(dispatched.lock().expect("lock").len(), 1);
    }

    // Teams off refuses (the manager is a built-in team identity).
    #[test]
    fn dispatch_manager_refuses_when_teams_is_off() {
        let (mut o, _) = orch(ReviewAuthority::Act);
        o.teams = Some(Teams::disabled());
        pass_self_test(&o, "1.0.0");
        assert_eq!(
            o.dispatch_manager(manager_run()),
            ManagerDispatchOutcome::TeamsOff
        );
    }

    // A manager run's `pr:` key resolves to no ticket, so its exit must NOT go through the ticket
    // classifier (which would schedule a continuation retry forever). It ends the run, releases the
    // claim, and schedules nothing.
    #[test]
    fn a_manager_exit_ends_the_run_and_schedules_no_retry() {
        let (mut o, _) = orch(ReviewAuthority::Act);
        pass_self_test(&o, "1.0.0");
        assert_eq!(
            o.dispatch_manager(manager_run()),
            ManagerDispatchOutcome::Dispatched
        );
        let key = "pr:makewhatis/rhapsody#12@manager";
        let re = o.running.get(key).cloned().expect("live manager entry");
        o.on_worker_exit(crate::retry::EvWorkerExit {
            issue_id: key.to_string(),
            failed: false,
            started_at: re.started_at,
            err_msg: String::new(),
            last_state: String::new(),
            declared_handoff: true,
            review_verdict: None,
            refused: false,
        });
        assert!(!o.running.contains_key(key));
        assert!(
            !o.retry_attempts.contains_key(key),
            "a manager run schedules no retry"
        );
        assert!(!o.claimed.contains(key), "the claim is released");
    }
}
