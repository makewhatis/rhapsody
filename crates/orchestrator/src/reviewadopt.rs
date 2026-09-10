//! reviewadopt — REPAIRING a pull request the watch set never learned about (STUDIO-838).
//!
//! **No Go counterpart.** Ticketless review is a Rhapsody addition end to end; this is the slice
//! that makes it recoverable.
//!
//! # The hole this fills
//!
//! Under `review.mode: ticketless` a pull request gets a reviewer only if it holds a row in
//! `rhapsody_review_watch`, and until now [`Orchestrator::plan_review_intro`] at
//! `handle_handoff_run` was the ONLY thing that wrote one. That call needs a LIVE run, so one
//! transient tracker error at handoff — the review-state move failing, `plan_review_intro`
//! declining, the daemon restarting mid-handoff — left the pull request open, green, in the review
//! state and invisible to every mechanism that assigns a reviewer. Nothing could repair it:
//! `POST /api/v1/runs/{id}/handoff` answers `not_running` by then, the room lever is refused under
//! ticketless by design (§15-e), and [`crate::reviewwatch`] is edge-triggered over rows that
//! EXIST, so no push and no reopen produces an edge. The only repair was re-dispatching the ticket
//! — a whole agent run to re-issue one call.
//!
//! # A repair, never a second way to request a review
//!
//! That constraint decides the shape. This module contributes exactly one thing — a different
//! TRIGGER — and contributes nothing to the decision itself: a planned adoption is an ordinary
//! [`ReviewIntroRequest`] that travels the same channel to the same off-loop
//! [`run_review_intro_task`](crate::reviewintro::run_review_intro_task) and is written by the same
//! loop-side [`handle_review_introduce`](Orchestrator::handle_review_introduce). Every gate a
//! handoff-time introduction passes, an adoption passes, in the same code — the watched-repo
//! allowlist above all, which is §14.1 F-SEC's anchor and the reason a pull-request coordinate is
//! never trusted for what a caller says about it.
//!
//! The one thing an adoption adds is a REFUSAL the handoff path does not want:
//! [`ReviewIntroRequest::only_if_unwatched`]. A handoff re-arms an existing row on purpose (that is
//! how a re-run gets re-reviewed); an adoption may only ever create the row that is missing.
//!
//! # Where the trusted inputs come from without a live run
//!
//! `plan_review_intro` reads all three off the run. This planner has no run, so each has to be
//! re-derived from a source that is at least as trustworthy:
//!
//! * **The repository** comes from the ticket's own resolved project — that is, from CONFIG, which
//!   is where the allowlist itself lives and is strictly less reachable than a run's binding.
//! * **The author** comes from this daemon's OWN event ledger: the newest run of the ticket, and
//!   the `teams.route` row that run recorded ([`crate::lifecycle::run_identity`]). A ticket no
//!   teammate ran is not adopted at all, because there is then no author to exclude — and
//!   "the author is not the reviewer" is a guard, not a nicety.
//! * **The reviewers** are ranked by [`crate::quorum::rank_reviewers`] over a live
//!   [`LoadSnapshot`](crate::teams::LoadSnapshot), exactly as `plan_review_intro` ranks them.
//!
//! # Bounded, and cheap in the steady state
//!
//! The candidate set is the poll tick's OWN fetch, which is active ∪ review — so learning that a
//! ticket sits in a review state costs no tracker call at all. Nearly every candidate is then
//! dropped by two in-memory comparisons.
//!
//! What is NOT free is resolving a branch to a pull-request number, which is a `gh` round trip on
//! the off-loop task. Three bounds keep it rare:
//!
//! * a ticket whose watch set already carries a row introduced FOR it is skipped before any lookup
//!   ([`Orchestrator::review_adopt_origins`]) — which is every healthy handoff, i.e. almost all of
//!   them;
//! * [`REVIEW_ADOPT_PROBE_INTERVAL`] paces how often the same ticket may be asked about, so a
//!   ticket parked in review whose branch has no pull request costs one lookup a quarter of an hour
//!   rather than one per tick;
//! * [`MAX_REVIEW_ADOPTIONS_PER_TICK`] caps the burst, so a first tick after a long outage cannot
//!   turn into a run of blocking round trips.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use rhapsody_core::{Issue, normalize_state};
use rhapsody_workspace::sanitize_key;

use crate::orchestrator::Orchestrator;
use crate::reviewintro::{REVIEW_ORIGIN_ADOPT, REVIEW_ORIGIN_HANDOFF, ReviewIntroRequest};

/// How many adoptions ONE tick may plan — the blast-radius bound on a `gh` round trip per plan,
/// [`crate::prstate::MAX_PR_STATE_CALLS_PER_TICK`]'s idea at a tenth the size.
///
/// Four, because an orphan is by construction rare (it takes a failed handoff to make one) and the
/// cap is not a throughput knob: the remainder is not dropped, it is simply considered again on the
/// next tick. Sizing it for the ordinary case rather than the recovery case is deliberate — a
/// daemon coming up against a backlog of parked tickets should trickle, not stampede.
pub const MAX_REVIEW_ADOPTIONS_PER_TICK: usize = 4;

/// How long before the SAME ticket may be probed again.
///
/// The bound exists for the candidate this sweep can never settle: a ticket parked in a review
/// state whose branch has no open pull request at all. It is not an orphan and never becomes one,
/// but nothing on the control task can tell it apart from one — that takes the `gh` lookup — so
/// without a pace it would cost a round trip every poll interval, forever, per ticket.
///
/// Fifteen minutes against what is actually waiting on it: an orphan has already sat unreviewed for
/// however long the run took to fail, and a reviewer takes minutes to run, so a quarter of an hour
/// of extra latency on the repair path is imperceptible next to the alternative (re-dispatching the
/// ticket). It is [`crate::quorum::MAX_QUORUM_BACKOFF_MS`]'s value, and for the same reason: a
/// condition nobody is watching settles at one attempt per quarter hour rather than a hot loop.
pub const REVIEW_ADOPT_PROBE_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// What one sweep decided.
///
/// The two halves are reported separately because they mean opposite things to an operator. A
/// PLANNED adoption is the daemon repairing itself and needs no attention. A REFUSED one is a pull
/// request this daemon can see is unreviewed and is not allowed to fix — the fault STUDIO-838 asks
/// to be visible, and the only outcome that must outlive the tick.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct AdoptSweep {
    /// The introductions to hand to the off-loop task, in candidate order.
    pub(crate) planned: Vec<ReviewIntroRequest>,
    /// `(project group, ticket identifier, why)` for each candidate that could not be adopted.
    pub(crate) refused: Vec<(String, String, &'static str)>,
    /// `(project group, ticket identifier)` for each candidate that IS adoptable — the input to
    /// retiring an advisory an earlier sweep filed against it.
    ///
    /// Reported separately from [`planned`](Self::planned) rather than derived from it, because the
    /// advisory is keyed by (group, ticket) and a `ReviewIntroRequest` carries neither: it names a
    /// repository and an origin tag, and reconstructing a ticket identifier out of the tag would
    /// make the advisory depend on that tag's spelling.
    pub(crate) repaired: Vec<(String, String)>,
}

impl Orchestrator {
    /// Plans the adoptions this tick will ask for, from the candidates the poll already fetched.
    ///
    /// Runs ON the control task, where `plan_review_intro` runs and for its reasons: the reviewer
    /// ranking reads `running`, the in-flight test reads `running`/`claimed`, and the watch set is
    /// single-writer. Every gate is a comparison over data already in memory or an indexed local
    /// store read; nothing here touches the network.
    ///
    /// `candidates` pairs each polled issue with its project's index into
    /// [`Effective::projects`](crate::effective::Effective), which is how the multi-project ladder
    /// already tags them; `None` is the legacy single-tracker form.
    pub(crate) fn plan_review_adoptions<'a>(
        &mut self,
        candidates: impl Iterator<Item = (&'a Issue, Option<usize>)>,
        now: Instant,
    ) -> AdoptSweep {
        let mut out = AdoptSweep::default();
        if !self.review_ticketless_enabled() {
            return out;
        }
        // Read ONCE per tick rather than once per candidate: the origins are a whole-table read and
        // the candidate list is a whole poll's worth of tickets.
        let origins = self.review_adopt_origins();
        let load = crate::teams::LoadSnapshot::from_running(&self.running);
        let mut probed: Vec<String> = Vec::new();
        for (iss, proj) in candidates {
            if out.planned.len() >= MAX_REVIEW_ADOPTIONS_PER_TICK {
                break;
            }
            match self.adopt_verdict(iss, proj, &origins, load.counts(), now) {
                Verdict::Skip => {}
                Verdict::Adopt(req) => {
                    probed.push(iss.identifier.clone());
                    out.repaired
                        .push((self.adopt_group(proj), iss.identifier.clone()));
                    out.planned.push(*req);
                }
                Verdict::Refuse(why) => {
                    probed.push(iss.identifier.clone());
                    out.refused
                        .push((self.adopt_group(proj), iss.identifier.clone(), why));
                }
            }
        }
        for identifier in probed {
            self.review_adopt_probed.insert(identifier, now);
        }
        out
    }

    /// Plans this tick's adoptions and hands them to the off-loop introduction task.
    ///
    /// The whole per-tick entry point, so the two selection ladders each call exactly one thing and
    /// neither can drift from the other. A no-op the instant the subsystem is off: the planner
    /// returns nothing, and a daemon that never opened the introduction channel has nowhere to send
    /// it in any case (`review_intro_tx: None`), which makes an adoption on a default installation
    /// unrepresentable rather than merely skipped.
    pub(crate) fn sweep_review_adoptions<'a>(
        &mut self,
        candidates: impl Iterator<Item = (&'a Issue, Option<usize>)>,
        now: Instant,
    ) {
        let sweep = self.plan_review_adoptions(candidates, now);
        // Retired FIRST, so a ticket that is refused for a new reason in the same sweep — which it
        // cannot be, but the ordering should not be what makes that true — ends the tick recorded
        // rather than cleared.
        for (group, identifier) in sweep.repaired {
            self.warnings.clear_orphaned_review(&group, &identifier);
        }
        for (group, identifier, why) in sweep.refused {
            tracing::warn!(
                issue = %identifier, reason = why,
                "ticketless review: this ticket is parked in a review state with no watch row and \
                 cannot be adopted"
            );
            self.warnings
                .record_orphaned_review(&group, &identifier, why);
        }
        let Some(tx) = self.review_intro_tx.as_ref() else {
            return;
        };
        for req in sweep.planned {
            let origin = req.introduced_by.clone();
            if tx.send(req).is_err() {
                tracing::warn!(
                    origin = %origin,
                    "ticketless review: the introduction task is gone; nothing was adopted"
                );
                return;
            }
        }
    }

    /// What one candidate is. Every gate here is a comparison over data already in memory or an
    /// indexed local store read; the expensive question — "does this branch have an open pull
    /// request, and which number is it" — is deliberately NOT asked, because it is a `gh` round
    /// trip and belongs on the off-loop task exactly as it does for a handoff.
    ///
    /// The gates are ordered by cost, cheapest first, which is also roughly least-to-most
    /// informative: a tick over a healthy installation exits nearly every candidate at the state
    /// test without touching the store at all.
    fn adopt_verdict(
        &self,
        iss: &Issue,
        proj: Option<usize>,
        origins: &HashSet<String>,
        load: &std::collections::HashMap<String, i64>,
        now: Instant,
    ) -> Verdict {
        if iss.identifier.is_empty() {
            return Verdict::Skip;
        }
        let Some(eff) = self.eff.as_ref() else {
            return Verdict::Skip;
        };
        // Gate: parked in a REVIEW state, per the ticket's OWN project — the same test both
        // selection ladders make, so "parked" means here exactly what it means to dispatch.
        let (review_states, active_states, repo_url) = match proj.and_then(|i| eff.projects.get(i))
        {
            Some(p) => (&p.review_states, &p.active_states, p.repo.clone()),
            None => (&eff.review_states, &eff.active_states, eff.cfg.repo.clone()),
        };
        let st = normalize_state(&iss.state);
        if !review_states.contains(&st) || active_states.contains(&st) {
            return Verdict::Skip;
        }
        // Gate: no live run. A ticket mid-run introduces its own pull request at its own handoff,
        // which is the trusted path; adopting underneath it would race that handoff for the rows it
        // is about to write, and would ask a teammate to review a branch its author is still
        // pushing to.
        if self.running.contains_key(&iss.id) || self.claimed.contains(&iss.id) {
            return Verdict::Skip;
        }
        // Gate: the watch set holds no row introduced FOR this ticket. The cheap pre-filter, and
        // the reason a sweep every tick is affordable: every healthy handoff leaves one of these,
        // so almost every candidate stops here rather than at a `gh` lookup. It is not the
        // idempotency guarantee — that is `handle_review_introduce`'s `only_if_unwatched`, keyed on
        // the pull request itself, because an operator's console introduction and a row whose
        // origin was rewritten would both slip past a tag comparison.
        if origins.contains(&handoff_origin(&iss.identifier))
            || origins.contains(&adopt_origin(&iss.identifier))
        {
            return Verdict::Skip;
        }
        // Gate: a teammate of this daemon's own ran it. Without an author there is nobody to
        // EXCLUDE, and "the author is not the reviewer" is a guard rather than a nicety — so a
        // ticket parked in review that this daemon never ran as a teammate is not adopted at all.
        // `plan_review_intro` refuses the same case as `re.identity.is_empty()`.
        let Some(author) = self.adopt_author(&iss.identifier) else {
            return Verdict::Skip;
        };
        // Gate: paced. The candidate this exists for is the one no sweep can ever settle — a ticket
        // parked in review whose branch has no pull request at all — which is indistinguishable
        // from an orphan without the `gh` lookup this paces.
        if let Some(at) = self.review_adopt_probed.get(&iss.identifier)
            && now.duration_since(*at) < REVIEW_ADOPT_PROBE_INTERVAL
        {
            return Verdict::Skip;
        }
        // Gate: the repository parses. Everything past here is a REFUSAL rather than a skip: the
        // ticket is an orphan candidate on this daemon's own terms, and the reasons it cannot be
        // repaired are all conditions an operator has to act on.
        let Some((owner, repo)) = crate::ghsummons::parse_repo(&repo_url) else {
            return Verdict::Refuse("the ticket's project names no GitHub owner/repo");
        };
        // Gate: THE allowlist (§15-a, F-SEC). Checked here so a sweep on an unconfigured remote
        // never leaves the loop, and again at `handle_review_introduce`, which is the load-bearing
        // one — a guard that lives only at the far end of a channel is a guard the next sender can
        // forget.
        if !self.review_repo_is_configured(&owner, &repo) {
            return Verdict::Refuse("no configured project owns the ticket's repository");
        }
        let Some(teams) = self.teams.as_ref() else {
            return Verdict::Skip;
        };
        // Ranked over a LIVE load snapshot, exactly as `plan_review_intro` ranks: `quorum_load` is
        // always empty under `ticketless`, so ranking against it would name the same first teammate
        // for every pull request in the sweep.
        let mut reviewers = crate::quorum::rank_reviewers(teams, &author, load);
        reviewers.truncate(teams.review.effective_reviewers());
        if reviewers.is_empty() {
            return Verdict::Refuse("the roster holds nobody but the author");
        }
        Verdict::Adopt(Box::new(ReviewIntroRequest {
            owner,
            repo,
            repo_url,
            head_branch: head_branch_for(&iss.identifier),
            reviewers,
            author,
            introduced_by: adopt_origin(&iss.identifier),
            only_if_unwatched: true,
        }))
    }

    /// Every `introduced_by` tag the watch set currently carries — the pre-filter's input.
    ///
    /// An unreadable store answers an EMPTY set, which is the loud direction rather than the quiet
    /// one: every candidate then looks unintroduced and reaches the lookup. That is safe because
    /// the write itself re-reads the watch set (`review_pr_is_watched`) and refuses on the same
    /// failure — so a broken store costs `gh` calls, never a duplicate reviewer.
    fn review_adopt_origins(&self) -> HashSet<String> {
        match self.store().load_review_watch() {
            Ok(rows) => rows.into_iter().map(|r| r.introduced_by).collect(),
            Err(e) => {
                tracing::warn!(
                    err = %e,
                    "ticketless review: the watch set could not be read, so this tick's adoption \
                     sweep cannot tell an introduced pull request from an orphaned one"
                );
                HashSet::new()
            }
        }
    }

    /// The teammate whose run authored `identifier`'s pull request, from this daemon's OWN event
    /// ledger: the newest run of the ticket, and the `teams.route` row that run recorded.
    ///
    /// The ticket's `rhapsody:@<name>` LABEL is deliberately not consulted, though it is free and
    /// already on the polled issue. The label is who the ticket is assigned to TODAY; the route row
    /// is who actually ran it. They differ exactly when the ticket has been re-assigned since — and
    /// there the label would name a teammate who is not the author, leaving the real author
    /// eligible to be picked as their own reviewer. That is the one error this lookup must not
    /// make.
    fn adopt_author(&self, identifier: &str) -> Option<String> {
        let runs = self
            .store()
            .list_issue_runs(rhapsody_store::RunFilter {
                issue: identifier.to_string(),
                limit: 1,
                ..Default::default()
            })
            .inspect_err(|e| {
                tracing::warn!(
                    issue = %identifier, err = %e,
                    "ticketless review: the run history could not be read; nothing is adopted for \
                     this ticket"
                )
            })
            .ok()?;
        crate::lifecycle::routed_identity(self.store.as_ref(), runs.first()?.id)
    }

    /// The project group an adoption's advisory is filed under — [`crate::warnings`]'s key.
    fn adopt_group(&self, proj: Option<usize>) -> String {
        self.eff
            .as_ref()
            .and_then(|eff| proj.and_then(|i| eff.projects.get(i)))
            .map(|p| p.group.clone())
            .unwrap_or_default()
    }
}

/// The branch a run of `identifier` pushed — the frozen `symphony/<key>` contract
/// `plan_review_intro` names for the same reason: it is what makes the decision network-free.
fn head_branch_for(identifier: &str) -> String {
    format!("symphony/{}", sanitize_key(identifier))
}

/// The origin tag an adoption of `identifier` writes onto the rows it creates.
fn adopt_origin(identifier: &str) -> String {
    format!("{REVIEW_ORIGIN_ADOPT}:{identifier}")
}

/// The origin tag a HANDOFF of `identifier` wrote — the cheap pre-filter's other half.
fn handoff_origin(identifier: &str) -> String {
    format!("{REVIEW_ORIGIN_HANDOFF}:{identifier}")
}

/// What one candidate is.
enum Verdict {
    /// Not an adoption candidate — the ordinary answer for nearly every ticket the poll returns.
    /// Silent by design: a ticket being mid-run, or in an active state, or already watched, is not
    /// a fault and must not read like one.
    Skip,
    /// A candidate this daemon may repair.
    Adopt(Box<ReviewIntroRequest>),
    /// A candidate this daemon may NOT repair. Surfaced to the operator.
    Refuse(&'static str),
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rhapsody_config::teams::{Identity, Review, ReviewMode, Teams};
    use rhapsody_store::{Sqlite, StorePath};
    use rhapsody_tracker::fake::Fake;

    use super::*;
    use crate::orchestrator::RunningEntry;
    use crate::reviewintro::REVIEW_ORIGIN_ADOPT;
    use crate::testsupport::{empty_effective, empty_resolved_project, set_of};

    const REPO_URL: &str = "git@github.com:makewhatis/rhapsody.git";
    /// The repository of a project this daemon is NOT configured for — the allowlist's other side.
    const OUTSIDE_URL: &str = "https://github.com/attacker/evil.git";
    /// The ticket's state text as the tracker spells it. The configured sets are NORMALIZED
    /// (lower-cased) exactly as `build_effective` leaves them, so the fixture also pins that the
    /// sweep compares like for like.
    const REVIEW_STATE: &str = "In Review";
    const REVIEW_STATE_NORM: &str = "in review";

    fn ident(name: &str) -> Identity {
        Identity {
            name: name.to_string(),
            profile: "swe".to_string(),
            ..Identity::default()
        }
    }

    fn teams_with(enabled: bool, mode: ReviewMode, names: &[&str]) -> Teams {
        Teams {
            enabled,
            review: Review {
                mode,
                ..Review::default()
            },
            roster: names.iter().map(|n| ident(n)).collect(),
            ..Teams::disabled()
        }
    }

    /// An orchestrator with ONE enabled project owning [`REPO_URL`], `In Review` as its review
    /// state, and an in-memory store — the shape the whole sweep is decided against.
    fn orch(teams: Teams) -> Orchestrator {
        let tracker = Arc::new(Fake::new());
        let mut eff = empty_effective(tracker.clone());
        eff.active_states = set_of(&["todo"]);
        eff.review_states = set_of(&[REVIEW_STATE_NORM]);
        eff.max_concurrent = 10;
        let mut proj = empty_resolved_project("rhapsody", tracker);
        proj.repo = REPO_URL.to_string();
        proj.active_states = set_of(&["todo"]);
        proj.review_states = set_of(&[REVIEW_STATE_NORM]);
        eff.projects = vec![proj];
        let mut o = Orchestrator::new("WORKFLOW.md");
        o.eff = Some(eff);
        o.teams = Some(teams);
        o.set_store(Arc::new(
            Sqlite::open(StorePath::InMemory).expect("open in-memory store"),
        ));
        o
    }

    /// [`orch`] with a caller-supplied tracker, so a test can drive the real poll tick over a
    /// candidate list instead of handing the planner one.
    fn orch_with_tracker(
        teams: Teams,
        tracker: Arc<dyn rhapsody_tracker::Tracker>,
    ) -> Orchestrator {
        let mut o = orch(teams);
        if let Some(eff) = o.eff.as_mut() {
            eff.tracker = Arc::clone(&tracker);
            eff.projects[0].tracker = tracker;
            eff.poll_interval = Duration::from_secs(3600);
            eff.stall_timeout = Duration::from_secs(3600);
        }
        o
    }

    /// The ticket as the poll tick hands it over: parked in the review state, on the one project.
    fn parked(identifier: &str) -> Issue {
        Issue {
            id: format!("iss-{identifier}"),
            identifier: identifier.to_string(),
            team_id: "team-1".to_string(),
            state: REVIEW_STATE.to_string(),
            ..Default::default()
        }
    }

    /// Records that `identity`'s run of `identifier` happened and ended — this daemon's own ledger,
    /// which is where an adoption learns who authored the pull request.
    fn record_run(o: &Orchestrator, identifier: &str, identity: &str) {
        let store = o.store();
        let run = store
            .start_run(rhapsody_store::RunStart {
                issue_identifier: identifier.to_string(),
                ..rhapsody_store::RunStart::default()
            })
            .expect("start run");
        store
            .append_events(
                run,
                &[rhapsody_store::EventRow {
                    seq: 1,
                    at: "2026-09-10T00:00:00Z".into(),
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

    /// One sweep over `issues`, all on the single configured project.
    fn sweep(o: &mut Orchestrator, issues: &[Issue], now: Instant) -> AdoptSweep {
        o.plan_review_adoptions(issues.iter().map(|i| (i, Some(0))), now)
    }

    /// The wiring, through the daemon's real tick: a poll that returns an orphaned review-state
    /// ticket ends with an ADOPTION on the introduction task's channel — no live run, no
    /// re-dispatch, and nothing an operator had to press.
    ///
    /// Driven through `on_tick` rather than through the planner, because the planner being right
    /// and the tick calling it are different facts and the second is the one this slice adds.
    #[tokio::test]
    async fn a_poll_tick_hands_an_orphaned_ticket_to_the_introduction_task() {
        let mut tr = Fake::new();
        tr.candidates = vec![parked("STUDIO-836")];
        let mut o = orch_with_tracker(
            teams_with(true, ReviewMode::Ticketless, &["alice", "bob"]),
            Arc::new(tr),
        );
        record_run(&o, "STUDIO-836", "alice");
        let mut rx = o.open_review_intro_channel();

        o.on_tick().await;

        let req = rx.try_recv().expect("an adoption reached the intro task");
        assert_eq!(
            req.introduced_by,
            format!("{REVIEW_ORIGIN_ADOPT}:STUDIO-836")
        );
        assert_eq!(req.head_branch, "symphony/STUDIO-836");
        assert_eq!(req.reviewers, vec!["bob".to_string()]);
        assert!(req.only_if_unwatched);
    }

    /// The acceptance, from the state the ticket is about: a pull request whose ticket sits in the
    /// review state and which holds NO watch row is adopted — with no live run anywhere, and
    /// without re-dispatching the ticket.
    ///
    /// The fixture starts orphaned on purpose. A test that introduced the pull request normally
    /// first and then asserted a row exists would pass with this whole module deleted.
    #[test]
    fn an_orphaned_review_state_ticket_is_adopted() {
        let mut o = orch(teams_with(true, ReviewMode::Ticketless, &["alice", "bob"]));
        record_run(&o, "STUDIO-836", "alice");
        assert!(
            o.store().load_review_watch().expect("read").is_empty(),
            "the fixture must start orphaned"
        );

        let out = sweep(&mut o, &[parked("STUDIO-836")], Instant::now());

        assert_eq!(out.refused, vec![], "nothing to refuse");
        assert_eq!(out.planned.len(), 1, "{:?}", out.planned);
        let req = &out.planned[0];
        assert_eq!(req.owner, "makewhatis");
        assert_eq!(req.repo, "rhapsody");
        assert_eq!(
            req.repo_url, REPO_URL,
            "the CONFIGURED binding, not a guess"
        );
        assert_eq!(req.head_branch, "symphony/STUDIO-836");
        assert_eq!(req.author, "alice", "read off this daemon's own ledger");
        assert_eq!(req.reviewers, vec!["bob".to_string()], "never the author");
        assert_eq!(
            req.introduced_by,
            format!("{REVIEW_ORIGIN_ADOPT}:STUDIO-836")
        );
        assert!(
            req.only_if_unwatched,
            "an adoption is a repair: it may never disturb a row that already exists"
        );
    }

    /// The advisory's only exit. A ticket the sweep refused is named in the project advisory and
    /// stays named — nothing self-heals it — until the sweep can actually repair it, which here
    /// means the operator adding the teammate the roster was missing.
    #[test]
    fn adopting_a_previously_refused_ticket_retires_its_advisory() {
        let mut o = orch(teams_with(true, ReviewMode::Ticketless, &["alice"]));
        record_run(&o, "STUDIO-836", "alice");
        let t0 = Instant::now();

        o.sweep_review_adoptions([(&parked("STUDIO-836"), Some(0))].into_iter(), t0);
        let got = o.warnings.merged_for("rhapsody");
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(got[0].contains("STUDIO-836"), "{got:?}");

        // The operator adds the reviewer the roster was missing.
        o.teams = Some(teams_with(true, ReviewMode::Ticketless, &["alice", "bob"]));
        o.sweep_review_adoptions(
            [(&parked("STUDIO-836"), Some(0))].into_iter(),
            t0 + REVIEW_ADOPT_PROBE_INTERVAL,
        );
        assert!(
            o.warnings.merged_for("rhapsody").is_empty(),
            "the fault is repaired, so the advisory is retired"
        );
    }

    // ── one test per gate, each asserting adoption does NOT happen ───────────────────────────────

    /// Gate 1 (§16). The whole subsystem is dormant unless Teams is on AND the mode is
    /// `ticketless`; an adoption sweep is not a way in through the side.
    #[test]
    fn adoption_is_dormant_unless_teams_is_on_and_the_mode_is_ticketless() {
        for (enabled, mode) in [
            (false, ReviewMode::Ticketless),
            (true, ReviewMode::Off),
            (true, ReviewMode::Tickets),
        ] {
            let mut o = orch(teams_with(enabled, mode, &["alice", "bob"]));
            record_run(&o, "STUDIO-836", "alice");
            assert_eq!(
                sweep(&mut o, &[parked("STUDIO-836")], Instant::now()),
                AdoptSweep::default(),
                "enabled={enabled} mode={mode:?}"
            );
        }
    }

    /// Gate 2. A ticket that is NOT parked in a review state is not orphaned — it is live work, and
    /// introducing its pull request would ask a teammate to review a branch its author is still
    /// pushing to.
    #[test]
    fn a_ticket_that_is_not_in_a_review_state_is_not_adopted() {
        let mut o = orch(teams_with(true, ReviewMode::Ticketless, &["alice", "bob"]));
        record_run(&o, "STUDIO-836", "alice");
        let mut iss = parked("STUDIO-836");
        iss.state = "Todo".to_string();

        assert_eq!(sweep(&mut o, &[iss], Instant::now()), AdoptSweep::default());
    }

    /// Gate 3. A ticket with a LIVE run introduces its own pull request at its own handoff, which
    /// is the trusted path. Adopting underneath it would race that handoff for the same rows.
    #[test]
    fn a_ticket_with_a_live_run_is_not_adopted() {
        let mut o = orch(teams_with(true, ReviewMode::Ticketless, &["alice", "bob"]));
        record_run(&o, "STUDIO-836", "alice");
        let iss = parked("STUDIO-836");
        o.running
            .insert(iss.id.clone(), RunningEntry::empty(iss.clone()));

        assert_eq!(sweep(&mut o, &[iss], Instant::now()), AdoptSweep::default());
    }

    /// Gate 4 (F-SEC's anchor, restated where the coordinate is born). A candidate that resolves
    /// its repository through the TOP-LEVEL binding — the untagged, legacy shape — is refused when
    /// the only project owning that repository is PAUSED. Refused LOUDLY, because a pull request in
    /// review that this daemon may not read is a fault an operator has to see, not a silent skip.
    ///
    /// This is the shape the guard exists for (STUDIO-725): `resolve_projects` inherits `repo:`
    /// into every project that declares none, so the only state in which the top-level binding
    /// matches while no ENABLED project does is a project the operator explicitly paused — and
    /// re-admitting one is the thing the allowlist refuses.
    ///
    /// The load-bearing half of the same pin lives at the write itself
    /// (`an_adoption_in_an_unconfigured_repository_is_refused`, `reviewintro`): the planner runs on
    /// data that came from config, so its check is defence in depth, while
    /// `handle_review_introduce` is the only place in the daemon that writes an introduction.
    #[test]
    fn a_candidate_whose_repository_no_enabled_project_owns_is_refused() {
        let mut o = orch(teams_with(true, ReviewMode::Ticketless, &["alice", "bob"]));
        record_run(&o, "STUDIO-836", "alice");
        if let Some(eff) = o.eff.as_mut() {
            // The top-level binding `resolve_projects` would have inherited, and the project that
            // owns it, paused.
            eff.cfg.repo = REPO_URL.to_string();
            eff.projects[0].disabled = true;
            let mut live = empty_resolved_project("podium", Arc::new(Fake::new()));
            live.repo = OUTSIDE_URL.to_string();
            eff.projects.push(live);
        }

        // Untagged, so the repository resolves through the top-level binding.
        let out =
            o.plan_review_adoptions([(&parked("STUDIO-836"), None)].into_iter(), Instant::now());

        assert_eq!(out.planned, vec![], "nothing may be adopted");
        assert_eq!(
            out.refused,
            vec![(
                String::new(),
                "STUDIO-836".to_string(),
                "no configured project owns the ticket's repository",
            )]
        );
    }

    /// Gate 5. No teammate is recorded as having run this ticket, so there is no author to exclude
    /// — and "the author is not the reviewer" is a guard. A ticket parked in review that this
    /// daemon never ran as a teammate is simply not this mechanism's business.
    #[test]
    fn a_ticket_no_teammate_ran_is_not_adopted() {
        let mut o = orch(teams_with(true, ReviewMode::Ticketless, &["alice", "bob"]));
        // No run at all in the ledger.
        assert_eq!(
            sweep(&mut o, &[parked("STUDIO-836")], Instant::now()),
            AdoptSweep::default()
        );
    }

    /// Gate 6. The roster holds nobody but the author, so every candidate reviewer IS the author.
    /// Refused rather than skipped: a pull request nobody can be asked to review is exactly the
    /// condition an operator has to know about.
    #[test]
    fn a_roster_holding_nobody_but_the_author_is_refused() {
        let mut o = orch(teams_with(true, ReviewMode::Ticketless, &["alice"]));
        record_run(&o, "STUDIO-836", "alice");

        let out = sweep(&mut o, &[parked("STUDIO-836")], Instant::now());

        assert_eq!(out.planned, vec![]);
        assert_eq!(
            out.refused,
            vec![(
                "rhapsody".to_string(),
                "STUDIO-836".to_string(),
                "the roster holds nobody but the author",
            )]
        );
    }

    /// Gate 7, the cheap pre-filter. A ticket whose handoff DID introduce its pull request is not
    /// an orphan, and must cost no `gh` lookup to establish that — which is what makes a sweep that
    /// runs every tick affordable on a healthy installation.
    #[test]
    fn a_ticket_whose_handoff_already_introduced_it_is_not_probed() {
        let mut o = orch(teams_with(true, ReviewMode::Ticketless, &["alice", "bob"]));
        record_run(&o, "STUDIO-836", "alice");
        o.store()
            .save_review_watch(rhapsody_store::ReviewWatchRow {
                author: "alice".into(),
                key: rhapsody_store::ReviewWatchKey {
                    owner: "makewhatis".into(),
                    repo: "rhapsody".into(),
                    number: 144,
                    reviewer: "bob".into(),
                },
                introduced_by: "handoff:STUDIO-836".into(),
                requested_sha: String::new(),
                last_reviewed_sha: String::new(),
                status: rhapsody_store::REVIEW_STATUS_REQUESTED.into(),
                open: true,
            })
            .expect("seed row");

        assert_eq!(
            sweep(&mut o, &[parked("STUDIO-836")], Instant::now()),
            AdoptSweep::default()
        );
    }

    /// Gate 8. The same ticket is not asked about again until the probe interval has elapsed. The
    /// candidate this paces is the one the sweep can never settle — a ticket parked in review whose
    /// branch has no pull request — which would otherwise cost a `gh` round trip every poll.
    #[test]
    fn the_same_ticket_is_not_re_probed_until_the_interval_elapses() {
        let mut o = orch(teams_with(true, ReviewMode::Ticketless, &["alice", "bob"]));
        record_run(&o, "STUDIO-836", "alice");
        let t0 = Instant::now();

        assert_eq!(sweep(&mut o, &[parked("STUDIO-836")], t0).planned.len(), 1);
        assert_eq!(
            sweep(
                &mut o,
                &[parked("STUDIO-836")],
                t0 + Duration::from_secs(60)
            )
            .planned,
            vec![],
            "one minute later is still the same probe"
        );
        assert_eq!(
            sweep(
                &mut o,
                &[parked("STUDIO-836")],
                t0 + REVIEW_ADOPT_PROBE_INTERVAL
            )
            .planned
            .len(),
            1,
            "the interval has elapsed, so it may be asked about again"
        );
    }

    /// Gate 9. One tick plans at most [`MAX_REVIEW_ADOPTIONS_PER_TICK`], so a daemon coming up
    /// against a backlog of parked tickets trickles instead of turning one tick into a run of
    /// blocking `gh` round trips. The remainder is not dropped — it is considered again next tick.
    #[test]
    fn one_tick_plans_no_more_than_the_per_tick_cap() {
        let mut o = orch(teams_with(true, ReviewMode::Ticketless, &["alice", "bob"]));
        let issues: Vec<Issue> = (0..MAX_REVIEW_ADOPTIONS_PER_TICK + 3)
            .map(|n| {
                let id = format!("STUDIO-90{n}");
                record_run(&o, &id, "alice");
                parked(&id)
            })
            .collect();

        let out = sweep(&mut o, &issues, Instant::now());

        assert_eq!(out.planned.len(), MAX_REVIEW_ADOPTIONS_PER_TICK);
    }
}
