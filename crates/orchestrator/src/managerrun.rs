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
///
/// It no longer tells the manager to end with a `HANDOFF:` line (STUDIO-1054): that instruction is
/// exactly what the three flux#87 shadow runs obeyed, producing prose and a `HANDOFF:` marker and
/// no decision block. The block named by [`MANAGER_DECISION_CONTRACT`] is the answer, and
/// [`manager_instructions`] is the ONE builder that states it.
pub const MANAGER_BASE_PROMPT: &str = "You are the manager for this pull request's review loop. \
Review the evidence the host serves you through your MCP tools, then answer with the decision \
block described below — the `rhapsody-manager-decision` block, not a `HANDOFF:` line, is your \
answer.";

/// The manager's TOOL contract, as the live run must be told it (STUDIO-1054; design record
/// `manager-agent-design.md` §4.4, §4.8). The manager run's MCP role registers reads and exactly one
/// write — `teams_retain`, its own bank, observations only. Everything else it might reach for
/// (`teams_post`, `teams_invalidate`, `symphony_send_message`, `symphony_handoff`, …) is NOT
/// registered, so a call is refused. Stating that here is what stops a run spending its turn trying
/// to post a proposal the host never enabled.
pub const MANAGER_TOOL_CONTRACT: &str = "## Your tools

The host registers your reads and exactly ONE write: `teams_retain`, which records an observation \
in your own bank. You have no `teams_post`, no `teams_invalidate`, no `symphony_send_message`, no \
`symphony_handoff` and no other write tool — a call to one of those is refused, so do not try. \
Your decision is the only thing you write that has an effect; the daemon performs every action. Do \
not attempt to post a proposal, mark a finding, move a ticket or approve a pull request yourself.";

/// The manager's OUTPUT CONTRACT: the exact fenced block the strict parser
/// ([`crate::managerdecision`]) accepts, the one-JSON-object rule, the four decision verbs and
/// their payloads, and the dismissal/finding-revision rule (STUDIO-1054; design record
/// `manager-agent-design.md` §6.1, §6.3).
///
/// This is the production copy. It used to live only in the M12 replay harness
/// (`managerfixtures.rs`), which is why the release gate passed on a prompt every live run was
/// never sent. [`manager_instructions`] is now the single builder; the harness composes its prompt
/// from it rather than carrying a private duplicate.
pub const MANAGER_DECISION_CONTRACT: &str = r#"## The decision block (required output)

End your final message with exactly one fenced block whose info string is
`rhapsody-manager-decision`; its body is one JSON object. The block is how you answer — a prose
`HANDOFF:` line is NOT a decision and the daemon reads nothing from it. The parser is STRICT: no
unknown keys, no duplicate keys, and a field that does not apply to your variant must be ABSENT
(JSON `null` counts as present and is invalid).

Common fields, on every variant:
- `decision`: one of `RERUN_REVIEW`, `ROUTE_TO_AUTHOR`, `APPROVE`, `ESCALATE`.
- `head`: the current head, exactly as the data below gives it.
- `evidence_rev`: the data below's `evidence_rev`.
- `rationale`: required, at most 4000 characters.

Variant payloads:
- `RERUN_REVIEW`: `rerun` is optional: {"reviewers": ["..."], "note": "..."}. Invalid when there is
  no eligible row (every live reviewer row already approved at the current patch).
- `ROUTE_TO_AUTHOR`: `route` required: {"fix": [{"finding": "alice:F1", "revision": 1}],
  "instructions": "..."}. Every `fix` entry must name an open, blocking finding revision listed
  below.
- `APPROVE`: no payload beyond an optional `dismiss`:
  [{"finding": "alice:F1", "revision": 1, "rationale": "..."}]. Eligible only when the round
  threshold is reached, the reviewer quorum is met, every live row read the current patch-id, diff
  coverage holds, and every open blocking finding is resolved or dismissed in this same decision.
- `ESCALATE`: `escalate` required: {"question": "...", "checked": "..."}.

A `dismiss` is bound to the exact finding REVISION it names; a later revision of the same finding
is re-evaluated and can reopen it. A field that does not apply to the variant must be absent, never
`null`.

Example:

```json
{"decision":"RERUN_REVIEW","head":"<head>","evidence_rev":<rev>,"rerun":{"note":"what changed"},"rationale":"the evidence"}
```
"#;

/// The manager's full instruction set — the base task prompt, the tool contract and the decision
/// contract — built by the ONE builder the live launch and the M12 harness share (STUDIO-1054).
///
/// This is the host's own BASE instruction and the output contract; the harness additionally
/// prepends the shipped `manager` profile prose (`manager_profile_prompt`), and the case packet is
/// appended on top by [`manager_live_prompt`]. NOTE: a live run is dispatched under the reserved
/// `rhapsody:@manager` label, which the Teams router resolves to the configured
/// `manager.default_identity` teammate rather than to the `manager` profile — so today the live run
/// does NOT receive that profile prose, only this builder's contract. Closing that divergence is a
/// separate follow-up; do not assume `manager.v1.md` reaches a live run.
pub fn manager_instructions() -> String {
    format!("{MANAGER_BASE_PROMPT}\n\n{MANAGER_TOOL_CONTRACT}\n\n{MANAGER_DECISION_CONTRACT}")
}

/// The prompt a LIVE manager run sends: [`manager_instructions`] plus the case packet (§7.2, §8),
/// which is the host's own record of the stall rendered as DATA. An empty packet (an older path)
/// sends the instructions alone.
///
/// [`worker::run_manager_attempt`](crate::worker) is the production caller; the M12 harness
/// (`crate::managerfixtures`) composes its replay prompt from [`manager_instructions`] too, so a
/// live run and the release gate can never again be told different contracts.
pub fn manager_live_prompt(case_packet: &str) -> String {
    if case_packet.is_empty() {
        manager_instructions()
    } else {
        format!("{}\n\n{case_packet}", manager_instructions())
    }
}

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
    /// The case packet (§7.2, §8) the host assembles at launch and hands the run as DATA. Empty
    /// means no packet (an older code path); the worker then sends only the shared
    /// [`manager_instructions`].
    pub case_packet: String,
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
    /// The rendered case packet (§7.2, §8) the launch assembled. Emptied by the launch when the
    /// caller supplies none; see [`ManagerCheckout::case_packet`].
    pub case_packet: String,
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
            case_packet: self.case_packet.clone(),
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
    /// gate, then the authority, then the coordinates and the route (the route's project names the
    /// `claude` command the self-test gate re-probes), then the §4.7 self-test, then the drain gate,
    /// and only then the overwrite guard and the dispatch.
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
        // THE overwrite guard: never point a second agent at a live manager run's identity. Checked
        // before the self-test gate's version re-probe so a repeated sweep for an in-flight run does
        // no work.
        if self.running.contains_key(&id) || self.claimed.contains(&id) {
            return ManagerDispatchOutcome::AlreadyInFlight;
        }
        // §4.7/§10.2: the self-test must have passed on the CURRENT CLI version before the manager
        // acts again. Re-probe the installed version HERE — and record it — so a CLI that updated
        // itself in place mid-process can never be acted on before the off-loop self-test watcher
        // re-runs the canary (the watcher re-runs because the verdict's version no longer matches).
        // `claude --version` is ~15 ms and manager launches are rare.
        let command = self.manager_cli_command(&route.slug);
        let probed = crate::managerselftest::probe_cli_version(&command);
        self.manager_selftest.observe_probe(&probed);
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
        let iss = run.synthetic_issue();
        self.finish_manager_dispatch(run, route, iss);
        ManagerDispatchOutcome::Dispatched
    }

    /// The `claude` command a manager run for `slug` would launch: the routed project's resolved
    /// command, else the CLI name. Used only to re-probe `--version` at the §4.7 gate.
    fn manager_cli_command(&self, slug: &str) -> String {
        self.eff
            .as_ref()
            .and_then(|e| e.projects.iter().find(|p| p.slug == slug))
            .map(|p| p.mcfg.claude.command.clone())
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| "claude".to_string())
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
        // M8: settle the intervention the run belonged to (§7.2). The run has ended, so the
        // intervention must not stay `running` until its lease expires — a clean exit with a valid
        // decision becomes `decided`/`validated`, and anything else a `failed_attempt`.
        self.settle_manager_intervention(&re.issue.id, e);
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

    /// A hermetic stand-in for the `claude` command that answers `--version` with a deterministic
    /// string and writes nothing (so the CI tmp-leak guard stays green). `echo 9.9.9 --version`
    /// prints `9.9.9 --version`; [`test_cli_version`] reads back exactly that, so the recorded
    /// verdict matches what the gate's re-probe will see.
    fn test_cli_command() -> String {
        "/bin/echo 9.9.9".to_string()
    }

    /// The version [`test_cli_command`] actually reports, probed the same way the gate does.
    fn test_cli_version() -> String {
        crate::managerselftest::probe_cli_version(&test_cli_command()).expect("probe fake cli")
    }

    fn record_entries(sink: &DispatchedEntries) -> crate::orchestrator::SpawnFn {
        let sink = Arc::clone(sink);
        Box::new(move |_iss, _attempt, re| {
            sink.lock().expect("dispatched lock").push(re.clone());
        })
    }

    /// An orchestrator with Teams ON + ticketless review mode (which is what makes
    /// `manager_review_authority()` non-`off`), one project owning [`REPO_URL`], an in-memory store,
    /// and a recording spawn seam. The project's `claude` command is the fake above, so the §4.7
    /// gate's version re-probe is deterministic.
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
        proj.mcfg.claude.command = test_cli_command();
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
            case_packet: String::new(),
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

    // §4.7/§10.2 + the ticket's B3: a verdict measured on a version other than the one installed NOW
    // refuses at the launch gate, even after a successful boot pass. The gate re-probes, so a
    // mid-process CLI update cannot be acted on.
    #[test]
    fn dispatch_refuses_when_the_installed_cli_version_changed() {
        let (mut o, dispatched) = orch(ReviewAuthority::Act);
        pass_self_test(&o, "0.0.1"); // a verdict measured on an OLD version
        assert!(
            matches!(
                o.dispatch_manager(manager_run()),
                ManagerDispatchOutcome::SelfTestFailed(_)
            ),
            "a stale verdict must refuse"
        );
        assert!(dispatched.lock().expect("lock").is_empty());
    }

    // §10.2/§12: `review_authority: off` refuses before anything is touched.
    #[test]
    fn dispatch_refuses_when_authority_is_off() {
        let (mut o, dispatched) = orch(ReviewAuthority::Off);
        pass_self_test(&o, &test_cli_version()); // even with a passing self-test, off means off
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
        pass_self_test(&o, &test_cli_version());
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
        pass_self_test(&o, &test_cli_version());
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
        pass_self_test(&o, &test_cli_version());
        assert_eq!(
            o.dispatch_manager(manager_run()),
            ManagerDispatchOutcome::TeamsOff
        );
    }

    // The manager run's harness is always `claude` and its model/effort come from M6's config
    // (`manager.model`/`manager.effort`). Pinned because alice's review noted the override could be
    // disabled (`if false && manager.is_some()`) with every test still green.
    #[test]
    fn a_dispatched_manager_run_carries_the_manager_model_effort_and_harness() {
        let (mut o, dispatched) = orch(ReviewAuthority::Act);
        {
            let teams = o.teams.as_mut().expect("teams");
            teams.manager.model = "claude-opus-5-5".to_string();
            teams.manager.effort = "high".to_string();
        }
        pass_self_test(&o, &test_cli_version());
        assert_eq!(
            o.dispatch_manager(manager_run()),
            ManagerDispatchOutcome::Dispatched
        );
        let d = dispatched.lock().expect("lock");
        assert_eq!(d.len(), 1);
        assert_eq!(
            d[0].harness, "claude",
            "a manager run is always the claude harness"
        );
        assert_eq!(d[0].model_override.model, "claude-opus-5-5");
        assert_eq!(d[0].model_override.effort, "high");
    }

    // The exit path of a manager run (STUDIO-1049): its `pr:` key resolves to no tracker issue, so
    // it ends exactly as a review does — recording the outcome and releasing the persisted claim —
    // without the ticket classifier and without scheduling any retry. M8 owns the decision effects.
    #[test]
    fn a_manager_exit_ends_the_run_and_schedules_no_retry() {
        let (mut o, _) = orch(ReviewAuthority::Act);
        pass_self_test(&o, &test_cli_version());
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
            manager_text: None,
            refused: false,
        });
        assert!(!o.running.contains_key(key));
        assert!(
            !o.retry_attempts.contains_key(key),
            "a manager run schedules no retry"
        );
        assert!(!o.claimed.contains(key), "the claim is released");
    }

    /// **STUDIO-1054 acceptance, item 2.** The PRODUCTION prompt — the one
    /// [`crate::worker::run_manager_attempt`] sends — carries the exact decision contract. This is
    /// the test the release gate lacked: it targets [`manager_live_prompt`], not the harness.
    /// Removing the contract from the shared builder reds it, even though the harness composes from
    /// the same builder (which is the point: they can no longer disagree).
    #[test]
    fn the_live_manager_prompt_carries_the_decision_contract() {
        let prompt = manager_live_prompt("CASE PACKET DATA");
        assert!(
            prompt.contains(crate::managerdecision::MANAGER_DECISION_TAG),
            "the live prompt must name the block tag: {prompt}"
        );
        for verb in ["RERUN_REVIEW", "ROUTE_TO_AUTHOR", "APPROVE", "ESCALATE"] {
            assert!(
                prompt.contains(verb),
                "the live prompt must name {verb}: {prompt}"
            );
        }
        for field in ["evidence_rev", "rationale", "dismiss", "route", "escalate"] {
            assert!(
                prompt.contains(field),
                "the live prompt must state the {field} field: {prompt}"
            );
        }
        assert!(
            prompt.contains("teams_retain"),
            "the live prompt must tell the run its only write is teams_retain: {prompt}"
        );
        assert!(
            prompt.contains("no `teams_post`"),
            "the live prompt must say teams_post is not registered: {prompt}"
        );
        assert!(
            prompt.contains("CASE PACKET DATA"),
            "the case packet must still accompany the contract: {prompt}"
        );
        // The empty-packet path is the instructions alone (the M7 shape).
        assert_eq!(manager_live_prompt(""), manager_instructions());
    }

    /// The contract names the tag the strict parser actually looks for. Mutation: rename
    /// `MANAGER_DECISION_TAG` and this reds, so the two can never drift.
    #[test]
    fn the_contract_names_the_parser_tag() {
        assert!(
            manager_instructions().contains(crate::managerdecision::MANAGER_DECISION_TAG),
            "the shared builder must name the parser's tag"
        );
    }
}
