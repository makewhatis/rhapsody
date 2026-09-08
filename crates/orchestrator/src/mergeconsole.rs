//! mergeconsole — the console merge action's LOOP-SIDE half: what the control task decides before
//! a merge may be attempted, and what it records after one was (STUDIO-767, §7 slices 2–4 of
//! `~/.rhapsody/docs/STUDIO-767-console-merge-action.md`).
//!
//! **No Go v0.4.0 counterpart.** [`crate::reviewconsole`]'s sibling, and deliberately its shape: a
//! loopback `POST` becomes an in-process control [`Event`], and the control task re-derives and
//! re-validates every coordinate it is handed rather than trusting the request that carried it.
//!
//! # Why the trigger is an endpoint and not a room post
//!
//! §14.1's **F-SEC** finding — a `from: operator` room line is forgeable by any local process, so
//! whoever decides which coordinate the daemon acts on decides what the daemon does — applies to a
//! merge with more force than it applied to a review, and §2 of this ticket's record argues the
//! room path out explicitly. **[`crate::teamsears::Intent`] gains no `Merge` variant, and that
//! non-change is a deliverable.** The four action intents are safe because forging one buys
//! nothing the quorum does not already do autonomously; a merge has no such autonomous
//! counterpart, so the bounding argument that licenses the room's action floor does not reach it.
//!
//! What the loopback endpoint replaces is a coordinate lifted out of room TEXT. It is not, by
//! itself, an authentication boundary — `handlers_reviews`'s module doc says so outright, and §G5
//! of the record says the honest version: a local process under `bypassPermissions` already holds
//! the operator's `gh` login and could merge this pull request itself. What the guardrails buy is
//! that the OPERATOR cannot merge the wrong pull request by clicking, the DAEMON cannot merge a
//! red or foreign one at all, and every attempt is attributable.
//!
//! # The split with [`crate::runmerge`]
//!
//! Every `gh` call is over there, off-loop, because they block. Everything here needs loop state
//! and makes no network call at all:
//!
//! * [`Orchestrator::plan_run_merge`] — the Teams gate, the run row, [`parse_repo`], the
//!   branch-belongs-to-ticket cross-check, the live-review snapshot, and the single-flight claim.
//! * [`Orchestrator::settle_run_merge`] — releasing that claim, the audit row on the run, and the
//!   manager's room line.
//!
//! [`Event`]: crate::Event

use std::time::Instant;

use chrono::Utc;
use rhapsody_config::room::Message;
use rhapsody_workspace::sanitize_key;

use crate::control_loop::Event;
use crate::ghsummons::parse_repo;
use crate::orchestrator::Orchestrator;
use crate::runmerge::{MergeControlOutcome, MergePlan, MergeReceipt};
use crate::stop::ControlHandle;
use crate::triage::MANAGER_IDENTITY;

/// The `events` row kind an attempted merge is recorded under (§3/G4's audit half). A **data**
/// value in the existing `kind` column, exactly as [`crate::teams::EVENT_ROUTE`] is — no schema
/// change, no new table, no golden move.
pub const EVENT_MERGE: &str = "teams.merge";

/// How long a single-flight claim survives without being settled.
///
/// The claim is normally released by [`Orchestrator::settle_run_merge`] on the way out, whatever
/// the outcome. It leaks in exactly one case: the HTTP task is dropped mid-merge — the operator
/// closed the tab, the daemon is shutting down — so the settle event is never sent. A claim that
/// lived that long is therefore assumed dead rather than kept forever, because the alternative is
/// a pull request that can never be merged from the console again until the daemon restarts.
///
/// Two minutes is comfortably longer than the three bounded `gh` calls a merge makes and far
/// shorter than an operator's patience with a button that does nothing.
const MERGE_CLAIM_TTL: std::time::Duration = std::time::Duration::from_secs(120);

/// What [`Orchestrator::plan_run_merge`] decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergePlanOutcome {
    /// Everything the control task can check holds; the off-loop half may proceed on this plan.
    Ready(MergePlan),
    /// It may not, and this is what the operator is told. Never [`MergeControlOutcome::Applied`]
    /// or [`MergeControlOutcome::ConfirmRequired`] — nothing has been resolved yet.
    Denied(MergeControlOutcome),
}

/// The single-flight key for a run's pull request: the coordinate, not the run.
///
/// Two runs of one ticket (a first attempt and a retry) share a branch and therefore share a pull
/// request, and a second click on either of them is the same merge — which is precisely what §3/G4
/// asks to be single-flighted. Case-folded because GitHub logins and repository names are.
fn claim_key(owner: &str, repo: &str, branch: &str) -> String {
    format!(
        "{}/{}:{branch}",
        owner.to_ascii_lowercase(),
        repo.to_ascii_lowercase()
    )
}

impl Orchestrator {
    /// Validates a console merge request against everything the control task owns, and claims the
    /// pull request for the caller (§7 slice 2's loop half).
    ///
    /// The order matters twice over. The claim is taken LAST, so a request that was going to be
    /// refused anyway never blocks the next one; and the coordinate is derived entirely from the
    /// run row before that, so what gets claimed is what will be merged.
    ///
    /// The branch cross-check is the "this pull request belongs to this ticket" assertion, made
    /// against two independently stored fields rather than assumed: `runs.repo` is written from
    /// the project's configured remote (never from an agent) and `runs.branch` from the worktree
    /// the daemon created, so a run whose branch is not the one its own ticket names is a row
    /// nothing in this daemon should act on.
    pub(crate) fn plan_run_merge(&mut self, run_id: i64) -> MergePlanOutcome {
        // §16's gate, and the same one every Rhapsody-additive write surface takes: with Teams off
        // there is no manager to act as, no room to report in, and the console renders Merge as
        // dependency-named rather than offering a control that would refuse.
        if !self.teams.as_ref().is_some_and(|t| t.enabled) {
            return MergePlanOutcome::Denied(MergeControlOutcome::Dormant);
        }
        let run = match self.store().get_run(run_id) {
            Ok(Some(run)) => run,
            Ok(None) => return MergePlanOutcome::Denied(MergeControlOutcome::NotFound),
            Err(e) => {
                return MergePlanOutcome::Denied(MergeControlOutcome::Failed(e.to_string()));
            }
        };
        let Some((owner, repo)) = parse_repo(&run.repo) else {
            // `parse_repo` also refuses a look-alike host (`evilgithub.com`, `…/github.com/…`;
            // STUDIO-721), so this arm covers "no remote" and "a remote we will not vouch for"
            // alike — and the latter must never reach `gh`.
            return self.deny(
                run_id,
                &run.issue_identifier,
                "this run has no GitHub repository to merge in",
            );
        };
        let want = format!("symphony/{}", sanitize_key(&run.issue_identifier));
        if run.branch != want {
            tracing::warn!(
                run = run_id,
                issue = %run.issue_identifier,
                branch = %run.branch,
                "console merge: this run's branch does not belong to its ticket; refusing"
            );
            return self.deny(
                run_id,
                &run.issue_identifier,
                "this run's branch does not belong to its ticket",
            );
        }
        // The review gate's raw material, read here because only the control task may read the
        // watch set (`reviewconsole`'s single-writer rule). Live rows only: a dropped or retired
        // review is not a review in flight.
        let watched = match self.store().load_live_review_watch() {
            Ok(rows) => rows
                .into_iter()
                .filter(|r| {
                    r.key.owner.eq_ignore_ascii_case(&owner)
                        && r.key.repo.eq_ignore_ascii_case(&repo)
                })
                .map(|r| r.key.number)
                .collect::<Vec<i64>>(),
            // Fails closed on the gate rather than merging with it unchecked: an unreadable watch
            // set is not an empty one.
            Err(e) => {
                return MergePlanOutcome::Denied(MergeControlOutcome::Failed(e.to_string()));
            }
        };
        let key = claim_key(&owner, &repo, &run.branch);
        if let Some(since) = self.merge_inflight.get(&key) {
            if since.elapsed() < MERGE_CLAIM_TTL {
                return MergePlanOutcome::Denied(MergeControlOutcome::Refused(
                    "a merge of that pull request is already in flight",
                ));
            }
            tracing::warn!(
                run = run_id,
                pr = %key,
                "console merge: a stale in-flight claim was never settled; taking it over"
            );
        }
        self.merge_inflight.insert(key, Instant::now());
        MergePlanOutcome::Ready(MergePlan {
            run_id,
            issue: run.issue_identifier,
            owner,
            repo,
            branch: run.branch,
            watched,
        })
    }

    /// A plan-time refusal, recorded on its way out. See [`Orchestrator::record_merge_attempt`]
    /// for why these two arms are recorded and the in-flight one is not.
    fn deny(&self, run_id: i64, issue: &str, why: &'static str) -> MergePlanOutcome {
        let outcome = MergeControlOutcome::Refused(why);
        if let Some(line) = attempt_line(issue, &outcome) {
            self.record_merge_attempt(run_id, issue, &line);
        }
        MergePlanOutcome::Denied(outcome)
    }

    /// Releases the single-flight claim and records what happened (§7 slice 4).
    ///
    /// Called for EVERY outcome the off-loop half returns, so the claim's lifetime is exactly the
    /// attempt's. The two records it writes are best-effort in the strict sense — a failed audit
    /// write is logged and never turns a merge that already happened into an error, because the
    /// merge is the irreversible part and the record is not.
    pub(crate) fn settle_run_merge(&mut self, plan: &MergePlan, outcome: &MergeControlOutcome) {
        self.merge_inflight
            .remove(&claim_key(&plan.owner, &plan.repo, &plan.branch));
        // Nothing was attempted: the operator has been shown the receipt and has not confirmed it.
        // Recording a "merge" here would put a row in the ledger for something that did not happen
        // and post a room line for a click nobody finished.
        let Some(line) = attempt_line(&plan.issue, outcome) else {
            return;
        };
        self.record_merge_attempt(plan.run_id, &plan.issue, &line);
    }

    /// Both halves of the record, together: the audit row on the run and the manager's room line.
    ///
    /// Also called from [`Orchestrator::plan_run_merge`] for the refusals that never reach the
    /// off-loop half, because those are the ones most worth having in the record: a run whose
    /// branch does not belong to its own ticket, or whose remote this daemon will not vouch for,
    /// is an anomaly somebody should see, and a click that was refused for one is not a click that
    /// left no trace. The in-flight refusal is deliberately NOT recorded — nothing was attempted
    /// that the first click is not already recording.
    fn record_merge_attempt(&self, run_id: i64, issue: &str, line: &str) {
        self.record_merge_event(run_id, line);
        self.post_merge_line(run_id, issue, line);
    }

    /// The audit row: one `teams.merge` event on the run itself (§3/G4).
    ///
    /// Written straight through the store rather than through the batching event writer, because
    /// the run this lands on has usually ENDED — the writer's queue is keyed by a live run's entry
    /// and its sequence counter lives on one, and neither exists here. The sequence number is
    /// probed the way [`crate::lifecycle`] probes for a route: one indexed `LIMIT 1` read of the
    /// run's newest row.
    fn record_merge_event(&self, run_id: i64, text: &str) {
        let seq = match self.store().search_events(rhapsody_store::EventQuery {
            run: run_id,
            limit: 1,
            ..rhapsody_store::EventQuery::default()
        }) {
            Ok(hits) => hits.first().map_or(1, |h| h.seq + 1),
            Err(e) => {
                // The probe failed, so the row's ORDER within the run is unknown. It is still
                // written — an audit record out of order beats none — and `events` has an index on
                // `(run_id, seq)` rather than a unique constraint, so a collision costs display
                // order and nothing else.
                tracing::warn!(run = run_id, err = %e, "console merge: could not read the run's event sequence");
                1
            }
        };
        if let Err(e) = self.store().append_events(
            run_id,
            &[rhapsody_store::EventRow {
                seq,
                at: crate::persist::rfc3339(Utc::now()),
                kind: EVENT_MERGE.to_string(),
                tool: String::new(),
                text: text.to_string(),
            }],
        ) {
            tracing::warn!(run = run_id, err = %e, "console merge: the audit event could not be written");
        }
    }

    /// The manager's room line (§4/G4): the team's lead reporting what it did.
    ///
    /// This is the sense in which the ticket's *"an agent merges the PR"* is satisfied — the
    /// already-trusted team-lead actor performs the merge and says so — while the irreversible
    /// command itself stays deterministic and host-side (§9.1: no LLM between the click and
    /// `main`). The room is advisory (§0.11.4: Linear is the ledger), so a room that cannot be
    /// written is logged and nothing else.
    fn post_merge_line(&self, run_id: i64, issue: &str, line: &str) {
        let Some(room) = self.teams_room.as_ref() else {
            return;
        };
        if let Err(e) = room.append(
            &Message::room(MANAGER_IDENTITY, Utc::now(), line).with_refs([issue.to_string()]),
        ) {
            tracing::warn!(run = run_id, err = %e, "console merge: the room line could not be posted");
        }
    }
}

/// The one sentence an attempt is recorded and reported as, or `None` when there was no attempt.
///
/// Composed by the HOST from the daemon's own values — the ticket, the coordinate it resolved,
/// `gh`'s own words — and never from anything a client sent, which is the trust line
/// [`crate::quorum::review_description`] draws for every other host-composed line.
fn attempt_line(issue: &str, outcome: &MergeControlOutcome) -> Option<String> {
    let what = |r: &MergeReceipt| {
        let how = if r.auto {
            format!("{}, auto", r.method)
        } else {
            r.method.clone()
        };
        format!("merged {issue} — {} ({how})", r.url)
    };
    match outcome {
        MergeControlOutcome::Applied(r) => Some(what(r)),
        MergeControlOutcome::Refused(why) => Some(format!(
            "did not merge {issue} — {why} (asked from the console)"
        )),
        MergeControlOutcome::Failed(err) => {
            Some(format!("could not merge {issue} — GitHub refused: {err}"))
        }
        // Nothing happened: the handshake's first leg, or a state that never reached a plan.
        MergeControlOutcome::ConfirmRequired(_)
        | MergeControlOutcome::Dormant
        | MergeControlOutcome::NotFound => None,
    }
}

impl ControlHandle {
    /// The operator's **merge** (`POST /api/v1/runs/{id}/merge`) — the whole action, end to end.
    ///
    /// Three phases, and the middle one is why this lives on the handle rather than in the loop:
    ///
    /// 1. **Plan**, on the control task, where the run row and the watch set are.
    /// 2. **Resolve and merge**, HERE — on the HTTP request's own task — because every step of it
    ///    blocks ([`crate::runmerge`]'s module doc). A stalled `gh` parks this request and leaves
    ///    the daemon ticking.
    /// 3. **Settle**, back on the control task: release the claim, write the audit row, post the
    ///    manager's room line.
    ///
    /// `confirm` is the head SHA the operator is confirming, or empty for the first leg of the
    /// handshake. It is the ONLY value from the request body that reaches any of this, and all it
    /// can do is fail to match.
    ///
    /// A daemon with no merge seam at all — Teams off — answers [`MergeControlOutcome::Dormant`]
    /// without a round trip, which is the same answer the control task would give.
    pub async fn merge_run(&self, run_id: i64, confirm: &str) -> MergeControlOutcome {
        let Some(deps) = self.merge.as_ref() else {
            return MergeControlOutcome::Dormant;
        };
        let plan = match self.plan_merge(run_id).await {
            MergePlanOutcome::Ready(plan) => plan,
            MergePlanOutcome::Denied(outcome) => return outcome,
        };
        let outcome = crate::runmerge::resolve_and_merge(&plan, confirm, deps).await;
        // Unconditional, and the claim's only reliable release: every arm above this line either
        // returned before a claim was taken or is on its way through here.
        self.settle_merge(plan, outcome.clone()).await;
        outcome
    }

    /// Phase 1: the control task's verdict on a merge request.
    ///
    /// A gone or cancelled control task answers `Dormant`, following
    /// [`ControlHandle::list_reviews`]: the daemon is shutting down, and a 500 would send the
    /// operator looking for a fault in the merge path.
    async fn plan_merge(&self, run_id: i64) -> MergePlanOutcome {
        let (tx, rx) = tokio::sync::oneshot::channel();
        if self
            .events
            .send(Event::RunMergePlan { run_id, reply: tx })
            .is_err()
        {
            return MergePlanOutcome::Denied(MergeControlOutcome::Dormant);
        }
        let mut lifetime = self.ctx.clone();
        tokio::select! {
            r = rx => r.unwrap_or(MergePlanOutcome::Denied(MergeControlOutcome::Dormant)),
            _ = lifetime.cancelled() => MergePlanOutcome::Denied(MergeControlOutcome::Dormant),
        }
    }

    /// Phase 3: hand the outcome back for its claim release, its audit row and its room line.
    ///
    /// Waits for the acknowledgement rather than firing and forgetting, so a test — and an
    /// operator's next click — can rely on the claim being gone by the time the response is
    /// written. A gone loop needs no release: its claims went with it.
    async fn settle_merge(&self, plan: MergePlan, outcome: MergeControlOutcome) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        if self
            .events
            .send(Event::RunMergeSettle {
                plan,
                outcome,
                reply: tx,
            })
            .is_err()
        {
            return;
        }
        let mut lifetime = self.ctx.clone();
        tokio::select! {
            _ = rx => {},
            _ = lifetime.cancelled() => {},
        }
    }
}

#[cfg(test)]
mod tests {
    //! The control task's half of the merge action, against a real store and a real room. What is
    //! asserted here is what the LOOP decides — the gate, the run row, the branch cross-check, the
    //! single-flight claim and the two records — never the `gh` half, which is
    //! [`crate::runmerge`]'s and is tested there against injected seams.

    use std::sync::Arc;

    use rhapsody_config::room::{Cursor, LocalRoom};
    use rhapsody_config::teams::{Identity, Teams};
    use rhapsody_store::{ReviewWatchKey, ReviewWatchRow, RunStart, Sqlite, StorePath};
    use rhapsody_tracker::fake::Fake;

    use super::*;
    use crate::testsupport::{TempDir, empty_effective, empty_resolved_project, set_of};

    const REPO_URL: &str = "git@github.com:makewhatis/rhapsody.git";
    const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn teams(enabled: bool) -> Teams {
        Teams {
            enabled,
            roster: vec![Identity {
                name: "alice".to_string(),
                profile: "swe".to_string(),
                ..Identity::default()
            }],
            ..Teams::disabled()
        }
    }

    fn orch(enabled: bool) -> Orchestrator {
        let tracker = Arc::new(Fake::new());
        let mut eff = empty_effective(tracker.clone());
        eff.active_states = set_of(&["todo"]);
        eff.max_concurrent = 10;
        let mut proj = empty_resolved_project("rhapsody", tracker);
        proj.repo = REPO_URL.to_string();
        eff.projects = vec![proj];
        let mut o = Orchestrator::new("WORKFLOW.md");
        o.eff = Some(eff);
        o.teams = Some(teams(enabled));
        o.set_store(Arc::new(
            Sqlite::open(StorePath::InMemory).expect("open in-memory store"),
        ));
        o
    }

    /// A finished run of `issue`, on `branch`, in `repo` — the row every decision is derived from.
    fn run_row(o: &Orchestrator, issue: &str, branch: &str, repo: &str) -> i64 {
        o.store()
            .start_run(RunStart {
                issue_identifier: issue.to_string(),
                branch: branch.to_string(),
                repo: repo.to_string(),
                ..RunStart::default()
            })
            .expect("start run")
    }

    /// The plan a healthy STUDIO-767 run produces, or a panic naming what was denied instead.
    fn ready(o: &mut Orchestrator, run_id: i64) -> MergePlan {
        match o.plan_run_merge(run_id) {
            MergePlanOutcome::Ready(plan) => plan,
            other => panic!("want Ready, got {other:?}"),
        }
    }

    fn receipt(url: &str) -> MergeReceipt {
        MergeReceipt {
            run_id: 1,
            issue: "STUDIO-767".to_string(),
            pr: "makewhatis/rhapsody#64".to_string(),
            url: url.to_string(),
            number: 64,
            head_sha: HEAD.to_string(),
            method: "squash".to_string(),
            auto: true,
            said: "✓ will be automatically merged".to_string(),
        }
    }

    /// Every room line the manager has posted, newest last.
    fn room_lines(room: &LocalRoom) -> Vec<String> {
        room.read_since("reader", &Cursor::default(), 100)
            .expect("read room")
            .messages
            .into_iter()
            .map(|m| format!("{}: {}", m.from, m.body))
            .collect()
    }

    /// Every `teams.merge` audit row on a run, in the order it would be read.
    fn audit(o: &Orchestrator, run_id: i64) -> Vec<String> {
        o.store()
            .run_events(run_id)
            .expect("run events")
            .into_iter()
            .filter(|e| e.kind == EVENT_MERGE)
            .map(|e| e.text)
            .collect()
    }

    /// **§16's gate.** With Teams off the surface is dormant: nothing is read, nothing is claimed,
    /// and the console renders Merge as dependency-named rather than a control that would refuse.
    #[test]
    fn with_teams_off_the_merge_surface_is_dormant() {
        let mut o = orch(false);
        let run = run_row(&o, "STUDIO-767", "symphony/STUDIO-767", REPO_URL);
        assert_eq!(
            o.plan_run_merge(run),
            MergePlanOutcome::Denied(MergeControlOutcome::Dormant)
        );
        assert!(
            o.merge_inflight.is_empty(),
            "a dormant surface claims nothing"
        );
    }

    /// A run id nobody ever minted is NotFound and not a refusal — the console renders it as a
    /// dead link rather than as the daemon saying no.
    #[test]
    fn a_run_that_does_not_exist_is_not_found() {
        let mut o = orch(true);
        assert_eq!(
            o.plan_run_merge(4242),
            MergePlanOutcome::Denied(MergeControlOutcome::NotFound)
        );
    }

    /// A run whose stored remote is not a GitHub one — or is a LOOK-ALIKE, which `parse_repo`
    /// refuses for STUDIO-721's reason — never reaches `gh`.
    #[test]
    fn a_run_without_a_github_repository_is_refused() {
        for repo in [
            "",
            "git@gitlab.com:o/r.git",
            "https://evilgithub.com/attacker/evil",
            "https://evil.test/github.com/attacker/evil",
        ] {
            let mut o = orch(true);
            let run = run_row(&o, "STUDIO-767", "symphony/STUDIO-767", repo);
            assert_eq!(
                o.plan_run_merge(run),
                MergePlanOutcome::Denied(MergeControlOutcome::Refused(
                    "this run has no GitHub repository to merge in"
                )),
                "repo {repo:?}"
            );
            assert!(o.merge_inflight.is_empty(), "a refusal claims nothing");
        }
    }

    /// **The "this pull request belongs to this ticket" check (§3/G1 step 3).** The branch is
    /// compared against the one the run's OWN ticket names, so a row whose two independently
    /// stored fields disagree is refused rather than merged.
    #[test]
    fn a_run_whose_branch_is_not_its_tickets_is_refused() {
        for branch in [
            "",
            "main",
            "symphony/STUDIO-999",
            "symphony/STUDIO-767-extra",
        ] {
            let dir = TempDir::new();
            let room = Arc::new(LocalRoom::new(dir.child("room")));
            let mut o = orch(true);
            o.teams_room = Some(Arc::clone(&room));
            let run = run_row(&o, "STUDIO-767", branch, REPO_URL);
            assert_eq!(
                o.plan_run_merge(run),
                MergePlanOutcome::Denied(MergeControlOutcome::Refused(
                    "this run's branch does not belong to its ticket"
                )),
                "branch {branch:?}"
            );
            // Recorded even though nothing reached GitHub: a run whose two independently stored
            // fields disagree is an anomaly, and a click refused for one is not a click that left
            // no trace.
            assert_eq!(
                audit(&o, run),
                vec![
                    "did not merge STUDIO-767 — this run's branch does not belong to its ticket \
                     (asked from the console)"
                        .to_string()
                ],
                "branch {branch:?}"
            );
            assert_eq!(room_lines(&room).len(), 1, "branch {branch:?}");
        }
    }

    /// The in-flight refusal is the one plan-time refusal that is NOT recorded: nothing was
    /// attempted that the first click is not already recording, and a double-click would otherwise
    /// put a second line in the room for one merge.
    #[test]
    fn a_second_click_leaves_no_second_record() {
        let dir = TempDir::new();
        let room = Arc::new(LocalRoom::new(dir.child("room")));
        let mut o = orch(true);
        o.teams_room = Some(Arc::clone(&room));
        let run = run_row(&o, "STUDIO-767", "symphony/STUDIO-767", REPO_URL);
        ready(&mut o, run);

        assert!(matches!(
            o.plan_run_merge(run),
            MergePlanOutcome::Denied(MergeControlOutcome::Refused(_))
        ));

        assert!(audit(&o, run).is_empty());
        assert!(room_lines(&room).is_empty());
    }

    /// The healthy plan: every coordinate comes from the run row, and nothing on it could have
    /// come from a request body.
    #[test]
    fn a_healthy_run_plans_from_its_own_row() {
        let mut o = orch(true);
        let run = run_row(&o, "STUDIO-767", "symphony/STUDIO-767", REPO_URL);
        assert_eq!(
            ready(&mut o, run),
            MergePlan {
                run_id: run,
                issue: "STUDIO-767".to_string(),
                owner: "makewhatis".to_string(),
                repo: "rhapsody".to_string(),
                branch: "symphony/STUDIO-767".to_string(),
                watched: Vec::new(),
            }
        );
    }

    /// **The review gate's raw material (§9.3).** Live review rows in the run's OWN repository ride
    /// on the plan; a review of some other repository's pull request is not this merge's business.
    #[test]
    fn the_plan_carries_the_live_reviews_of_this_runs_repository() {
        let mut o = orch(true);
        for (owner, repo, number) in [
            ("makewhatis", "rhapsody", 64),
            ("MakeWhatIs", "Rhapsody", 65),
            ("makewhatis", "podium", 90),
        ] {
            o.store()
                .save_review_watch(ReviewWatchRow {
                    key: ReviewWatchKey {
                        owner: owner.to_string(),
                        repo: repo.to_string(),
                        number,
                        reviewer: "bob".to_string(),
                    },
                    open: true,
                    ..ReviewWatchRow::default()
                })
                .expect("save watch row");
        }
        let run = run_row(&o, "STUDIO-767", "symphony/STUDIO-767", REPO_URL);
        let mut watched = ready(&mut o, run).watched;
        watched.sort_unstable();
        assert_eq!(
            watched,
            vec![64, 65],
            "the repository is matched case-insensitively, and another repo's review is not ours"
        );
    }

    /// **Single-flight (§3/G4).** A second click while one merge is in flight is refused, and
    /// settling the first releases the claim so a later click works.
    #[test]
    fn a_second_click_is_refused_until_the_first_settles() {
        let mut o = orch(true);
        let run = run_row(&o, "STUDIO-767", "symphony/STUDIO-767", REPO_URL);
        let plan = ready(&mut o, run);

        assert_eq!(
            o.plan_run_merge(run),
            MergePlanOutcome::Denied(MergeControlOutcome::Refused(
                "a merge of that pull request is already in flight"
            ))
        );
        // A SECOND run of the same ticket shares the branch, so it shares the claim: the pull
        // request a second click would merge is the same one.
        let retry = run_row(&o, "STUDIO-767", "symphony/STUDIO-767", REPO_URL);
        assert_eq!(
            o.plan_run_merge(retry),
            MergePlanOutcome::Denied(MergeControlOutcome::Refused(
                "a merge of that pull request is already in flight"
            ))
        );

        o.settle_run_merge(&plan, &MergeControlOutcome::ConfirmRequired(receipt("u")));
        assert!(o.merge_inflight.is_empty(), "settling releases the claim");
        assert!(matches!(o.plan_run_merge(run), MergePlanOutcome::Ready(_)));
    }

    /// A claim whose HTTP task died before it could settle — the operator closed the tab — expires
    /// rather than blocking the pull request for the daemon's lifetime.
    #[test]
    fn a_stale_claim_is_taken_over() {
        let mut o = orch(true);
        let run = run_row(&o, "STUDIO-767", "symphony/STUDIO-767", REPO_URL);
        ready(&mut o, run);
        let key = claim_key("makewhatis", "rhapsody", "symphony/STUDIO-767");
        let stale = Instant::now() - MERGE_CLAIM_TTL - std::time::Duration::from_secs(1);
        o.merge_inflight.insert(key, stale);
        assert!(matches!(o.plan_run_merge(run), MergePlanOutcome::Ready(_)));
    }

    /// **The audit half of §3/G4.** An applied merge leaves a `teams.merge` row on the run naming
    /// the ticket, the pull request and how it was merged, and one manager room line saying the
    /// same thing to the team.
    #[test]
    fn an_applied_merge_is_recorded_on_the_run_and_in_the_room() {
        let dir = TempDir::new();
        let room = Arc::new(LocalRoom::new(dir.child("room")));
        let mut o = orch(true);
        o.teams_room = Some(Arc::clone(&room));
        let run = run_row(&o, "STUDIO-767", "symphony/STUDIO-767", REPO_URL);
        let plan = ready(&mut o, run);

        o.settle_run_merge(
            &plan,
            &MergeControlOutcome::Applied(receipt(
                "https://github.com/makewhatis/rhapsody/pull/64",
            )),
        );

        assert_eq!(
            audit(&o, run),
            vec![
                "merged STUDIO-767 — https://github.com/makewhatis/rhapsody/pull/64 (squash, auto)"
                    .to_string()
            ]
        );
        assert_eq!(
            room_lines(&room),
            vec![
                "@manager: merged STUDIO-767 — https://github.com/makewhatis/rhapsody/pull/64 \
                 (squash, auto)"
                    .to_string()
            ],
            "the team's lead reports the merge it performed"
        );
    }

    /// A refusal and a failure are recorded too — an operator's click that did NOT merge is
    /// exactly as worth having in the record as one that did.
    #[test]
    fn a_refusal_and_a_failure_are_recorded_as_attempts() {
        for (outcome, want) in [
            (
                MergeControlOutcome::Refused("that pull request is closed"),
                "did not merge STUDIO-767 — that pull request is closed (asked from the console)",
            ),
            (
                MergeControlOutcome::Failed("merge conflicts".to_string()),
                "could not merge STUDIO-767 — GitHub refused: merge conflicts",
            ),
        ] {
            let dir = TempDir::new();
            let room = Arc::new(LocalRoom::new(dir.child("room")));
            let mut o = orch(true);
            o.teams_room = Some(Arc::clone(&room));
            let run = run_row(&o, "STUDIO-767", "symphony/STUDIO-767", REPO_URL);
            let plan = ready(&mut o, run);
            o.settle_run_merge(&plan, &outcome);
            assert_eq!(audit(&o, run), vec![want.to_string()]);
            assert_eq!(
                room_lines(&room),
                vec![format!("{MANAGER_IDENTITY}: {want}")]
            );
        }
    }

    /// The handshake's first leg is not an attempt: nothing was merged, so nothing is recorded and
    /// the room stays quiet. Otherwise every merge would be reported twice and every abandoned
    /// confirmation once.
    #[test]
    fn an_unconfirmed_request_records_nothing() {
        let dir = TempDir::new();
        let room = Arc::new(LocalRoom::new(dir.child("room")));
        let mut o = orch(true);
        o.teams_room = Some(Arc::clone(&room));
        let run = run_row(&o, "STUDIO-767", "symphony/STUDIO-767", REPO_URL);
        let plan = ready(&mut o, run);

        o.settle_run_merge(&plan, &MergeControlOutcome::ConfirmRequired(receipt("u")));

        assert!(audit(&o, run).is_empty(), "nothing happened to record");
        assert!(room_lines(&room).is_empty(), "and nothing to tell the team");
    }

    /// The audit row lands AFTER the run's own events rather than colliding with seq 1, so a
    /// reader sees the merge where it belongs in the run's timeline.
    #[test]
    fn the_audit_row_follows_the_runs_own_events() {
        let mut o = orch(true);
        let run = run_row(&o, "STUDIO-767", "symphony/STUDIO-767", REPO_URL);
        o.store()
            .append_events(
                run,
                &[
                    rhapsody_store::EventRow {
                        seq: 1,
                        kind: "turn".to_string(),
                        ..rhapsody_store::EventRow::default()
                    },
                    rhapsody_store::EventRow {
                        seq: 7,
                        kind: "turn".to_string(),
                        ..rhapsody_store::EventRow::default()
                    },
                ],
            )
            .expect("append events");
        let plan = ready(&mut o, run);

        o.settle_run_merge(&plan, &MergeControlOutcome::Applied(receipt("u")));

        let merged: Vec<i64> = o
            .store()
            .run_events(run)
            .expect("run events")
            .into_iter()
            .filter(|e| e.kind == EVENT_MERGE)
            .map(|e| e.seq)
            .collect();
        assert_eq!(merged, vec![8], "one row, after the run's newest");
    }
}
