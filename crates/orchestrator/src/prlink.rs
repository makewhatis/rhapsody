//! prlink — the daemon puts a clickable pull-request link on the ticket (STUDIO-875, corrected by
//! STUDIO-882).
//!
//! **No Go v0.4.0 counterpart.** Symphony only ever READ GitHub attachments, on the assumption
//! that Linear's own GitHub integration had written them.
//!
//! # What this module does NOT do, stated first because it used to claim otherwise
//!
//! **This write does not feed `linked_prs`, and no write from this daemon can.** STUDIO-875 shipped
//! this module to repair summons routing on a repository whose GitHub integration is not connected,
//! reasoning that `attachmentLinkGitHubPR` — rather than the generic `attachmentLinkURL` — would
//! make Linear classify the attachment as `sourceType: "github"` and so admit it to
//! `Issue::linked_prs`. STUDIO-882 read a daemon-written attachment back off the live API and
//! measured otherwise:
//!
//! ```text
//! daemon-written, unconnected repo:  sourceType: "api"     metadata: {}
//! integration-written, connected:    sourceType: "github"  metadata: { url, number, status, … }
//! ```
//!
//! The mutation degrades to a plain API attachment on exactly the population 875 targeted, and it
//! fails `is_github_pr` twice: the `sourceType` gate rejects it, and `linked_prs` is built by
//! matching a pull-request url out of `metadata.url`, which is not there either. The write landed,
//! Linear showed it, and the ticket read `linked_prs_total=0` for eleven hours.
//!
//! Summons routing is therefore [`crate::ghenrich::DaemonPrLinks`]' job now — the daemon's OWN
//! record of which pull request belongs to which ticket, read out of the review watch set, which
//! needs nothing from the tracker.
//!
//! # Why the write is still made
//!
//! Because it earns its place for the one audience left: a person. The attachment renders on the
//! Linear issue as `makewhatis/rhapsody#159` linking to the pull request, which is how somebody
//! reading the ticket gets to the code — and on an unconnected repository nothing else puts it
//! there. It costs at most one tracker call per pull request, and a connected workspace pays none
//! at all (its attachment IS the resolved pull request, so the gate below skips).
//!
//! What it must never again be is load-bearing. Nothing in the daemon reads this attachment back,
//! and a failure to write it costs a link in the UI rather than a dropped review.
//!
//! # What it asks before writing
//!
//! Exactly one question, and it is asked of the pull request that was RESOLVED rather than of the
//! ticket's state: is this pull request already among the ones the ticket links to in this
//! repository? A healthy installation answers yes — its attachment IS the resolved pull request —
//! and pays no tracker write at all. Anything else writes. The reason it cannot be the more
//! obvious "does the ticket have an unmerged link here" is in [`pr_link_target`]: `merged` is
//! maintained by the tracker's GitHub integration, whose absence is this module's whole premise, so
//! on the installations that need a link it is a field nothing refreshes.
//!
//! On the quorum path the URL handed to the write is [`crate::quorum`]'s `resolve_open_pr` result,
//! which falls back to the ticket's own attachment when the `gh` lookup fails. That fallback URL
//! came OFF a link the ticket already has, so the worst it can produce is a duplicate write — never
//! a link to the wrong pull request.
//!
//! # Best-effort, and the word is load-bearing
//!
//! A failed link must never fail the thing that was actually asked for — the review introduction,
//! or the quorum fan-out. Since STUDIO-882 it costs a link in the tracker's UI and nothing else;
//! it is retried by whatever next resolves a pull request for that ticket: another handoff, or
//! [`crate::reviewadopt`]'s sweep. **That is not "every tick", and on the quorum path it is not
//! even every handoff** — `fan_out` returns at `AlreadyRequestedAtHead` before it resolves a
//! tracker, so a repeat handoff at an unchanged head retries nothing. That mattered when routing
//! depended on this write; it no longer does, because a ticket the daemon parked for review is
//! reachable through [`crate::ghenrich::DaemonPrLinks`] whether or not the attachment ever landed.
//! Every failure here is a warning and a return, never an error the caller has to thread.
//!
//! What it is NOT is silent: the two outcomes worth a line get one each, because "the daemon linked
//! it" and "the daemon tried and Linear refused" are the two facts an operator staring at an idle
//! board needs to tell apart.

use std::sync::Arc;

use async_trait::async_trait;

use rhapsody_tracker::{Tracker, TrackerError};

/// The ticket a resolved pull request should be attached to, and what that ticket already carries.
///
/// Decided on the control task, where the candidate snapshot lives, so the off-loop task never has
/// to ask what a ticket already carries. `None` on its `Option<PrLinkTarget>` holder means "there
/// is no ticket to attach to at all" — not "there is nothing to do", which is a question only the
/// resolved pull request can answer. See [`link_pr_best_effort`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PrLinkTarget {
    /// The tracker id the attachment is written against.
    pub issue_id: String,
    /// The human identifier, for the log line. An operator reads `STUDIO-872`, not a UUID.
    pub identifier: String,
    /// The repository the write is about, so the skip below compares pull requests and not merely
    /// numbers. Case-folded on comparison, as everywhere else that touches a GitHub coordinate.
    pub owner: String,
    pub repo: String,
    /// The pull-request numbers this ticket ALREADY carries a link to in that repository,
    /// ascending. Identity, not state: see [`pr_link_target`] for why no `merged` flag appears
    /// anywhere in this decision.
    pub linked: Vec<i64>,
}

/// The ticket a pull request could be attached to, and the numbers already attached to it.
///
/// `None` only when the issue carries no tracker id — nothing to attach to, and nothing a write
/// could fix. Everything else is carried through to [`link_pr_best_effort`], which is the only
/// place that knows WHICH pull request was resolved and can therefore tell "already linked" from
/// "linked to something else".
///
/// # Why the decision is identity and never `merged`
///
/// The obvious gate is the one this function used to apply: skip when the ticket already carries an
/// UNMERGED linked pull request here. It reads correctly and it cannot work, because `merged` comes from the attachment's
/// `metadata.status`/`mergedAt` — fields maintained by the tracker's GitHub integration, whose
/// ABSENCE is the entire premise of this module. On the installations that need the link, nothing
/// writes attachments and so nothing refreshes them: a link the daemon wrote reads `unmerged`
/// forever, including long after its pull request has merged.
///
/// That stale `unmerged` would then refuse the ticket's SECOND pull request, leaving the UI link
/// pointing at the wrong one. (When this gate was routing rather than decoration, the same
/// staleness was a dropped review: `apply_github_summons` counted the ticket as reachable on the
/// strength of the stale link and never reported it. STUDIO-882 moved routing off this path
/// entirely — see the module doc — and the watch set it moved to keeps its own liveness, so the
/// hazard no longer has a routing half.)
///
/// So the question asked is one whose answer this daemon maintains itself: **is the pull request we
/// just resolved already among the ones this ticket links to?** A healthy installation still pays
/// zero writes — its attachment IS the resolved pull request — and a stale one links the new pull
/// request correctly.
pub(crate) fn pr_link_target(
    iss: &rhapsody_core::Issue,
    owner: &str,
    repo: &str,
) -> Option<PrLinkTarget> {
    if iss.id.is_empty() {
        return None;
    }
    // GitHub owner/repo are case-insensitive, and the configured repo URL and a tracker attachment
    // URL can legitimately differ in casing — the same reason `apply_github_summons` case-folds.
    let mut linked: Vec<i64> = iss
        .linked_prs
        .iter()
        .flatten()
        .filter(|pr| pr.owner.eq_ignore_ascii_case(owner) && pr.repo.eq_ignore_ascii_case(repo))
        .map(|pr| pr.number)
        .collect();
    linked.sort_unstable();
    linked.dedup();
    Some(PrLinkTarget {
        issue_id: iss.id.clone(),
        identifier: iss.identifier.clone(),
        owner: owner.to_string(),
        repo: repo.to_string(),
        linked,
    })
}

/// The one write this module performs, behind a seam so the two call sites can pass what they
/// already hold — a [`ControlHandle`](crate::stop::ControlHandle) that resolves the live tracker,
/// or a tracker the quorum has already resolved for the parent's own project.
#[async_trait]
pub trait PrLinker: Send + Sync {
    /// Attaches `url` to `issue_id`. Callers do not depend on the tracker de-duplicating: the
    /// [`pr_link_target`] gate is what keeps a working installation from writing at all, and see
    /// [`Tracker::link_pull_request`] for what a duplicate would and would not cost.
    async fn link_pull_request(&self, issue_id: &str, url: &str) -> Result<(), TrackerError>;
}

/// A [`PrLinker`] over an already-resolved tracker — the quorum's shape, where `tracker_for` has
/// picked the parent project's own client before the fan-out starts.
pub(crate) struct TrackerLinker(pub(crate) Arc<dyn Tracker>);

#[async_trait]
impl PrLinker for TrackerLinker {
    async fn link_pull_request(&self, issue_id: &str, url: &str) -> Result<(), TrackerError> {
        self.0.link_pull_request(issue_id, url).await
    }
}

/// The daemon's own linker: the live tracker, resolved exactly as
/// [`ControlHandle::move_issue_state`](crate::stop::ControlHandle::move_issue_state) resolves it —
/// the `control()`-time snapshot, else the shared reads cell, so a handle built before the first
/// config load still writes once one arrives.
#[async_trait]
impl PrLinker for crate::stop::ControlHandle {
    async fn link_pull_request(&self, issue_id: &str, url: &str) -> Result<(), TrackerError> {
        let tracker = self.tracker.clone().or_else(|| {
            self.reads
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .tracker
                .clone()
        });
        match tracker {
            Some(tr) => tr.link_pull_request(issue_id, url).await,
            None => Err(TrackerError::Other("no effective tracker".to_string())),
        }
    }
}

/// Links `url` to `target`, reporting every outcome and failing nothing.
///
/// A `None` target is an issue with no tracker id and says nothing; a `None` linker is a daemon
/// with no tracker to write through, which is worth one debug line and no more. A `url` the ticket
/// demonstrably already links to is the configured-workspace case, and is the reason a healthy
/// installation sees no new tracker writes at all.
pub(crate) async fn link_pr_best_effort(
    linker: Option<&dyn PrLinker>,
    target: Option<&PrLinkTarget>,
    url: &str,
) {
    let Some(target) = target else {
        return;
    };
    if target.issue_id.is_empty() || url.is_empty() {
        return;
    }
    // The skip, and it is deliberately the WEAK direction: only a pull request we can name, in the
    // repository this target is about, and already among the ticket's links, buys silence. A URL
    // this daemon cannot parse (or one naming another repository — `resolve_open_pr`'s attachment
    // fallback can hand one back) falls through and writes, because a duplicate link costs the
    // tracker's UI one redundant row while a wrong skip costs an author their next review.
    if let Some(number) = resolved_number(url, &target.owner, &target.repo)
        && target.linked.contains(&number)
    {
        tracing::debug!(
            issue_identifier = %target.identifier, pr = %url,
            "pr-link: the ticket already links this pull request, so nothing is written"
        );
        return;
    }
    let Some(linker) = linker else {
        tracing::debug!(
            issue_identifier = %target.identifier, pr = %url,
            "pr-link: no tracker to write through, so the pull request stays unlinked"
        );
        return;
    };
    match linker.link_pull_request(&target.issue_id, url).await {
        Ok(()) => tracing::info!(
            issue_identifier = %target.identifier, pr = %url,
            "pr-link: attached the pull request to its ticket, so a summons on it can reach the author"
        ),
        // The consequence, not just the error: whoever reads this line is reading it because
        // nothing re-engaged an author, and the link is why.
        Err(e) => tracing::warn!(
            issue_identifier = %target.identifier, pr = %url, err = %e,
            "pr-link: could not attach the pull request to its ticket; a review that files findings \
             on it cannot re-engage the author until this succeeds"
        ),
    }
}

/// The pull-request number `url` names in `owner`/`repo`, or `None` when it names none.
///
/// Shares [`crate::teamsears::extract_pr_urls`] with the summons scanner rather than parsing a URL
/// a second way, so "the pull request this URL is" means one thing across the crate.
fn resolved_number(url: &str, owner: &str, repo: &str) -> Option<i64> {
    crate::teamsears::extract_pr_urls(url)
        .into_iter()
        .find(|p| p.owner.eq_ignore_ascii_case(owner) && p.repo.eq_ignore_ascii_case(repo))
        .map(|p| p.number)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Records what it was asked to link, and can refuse.
    struct Recording {
        calls: Mutex<Vec<(String, String)>>,
        err: Option<TrackerError>,
    }

    impl Recording {
        fn new(err: Option<TrackerError>) -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
                err,
            })
        }
        fn calls(&self) -> Vec<(String, String)> {
            self.calls.lock().expect("calls").clone()
        }
    }

    #[async_trait]
    impl PrLinker for Recording {
        async fn link_pull_request(&self, issue_id: &str, url: &str) -> Result<(), TrackerError> {
            self.calls
                .lock()
                .expect("calls")
                .push((issue_id.to_string(), url.to_string()));
            match &self.err {
                Some(e) => Err(TrackerError::Other(e.to_string())),
                None => Ok(()),
            }
        }
    }

    const PR: &str = "https://github.com/makewhatis/rhapsody/pull/154";

    fn target() -> PrLinkTarget {
        PrLinkTarget {
            issue_id: "iss-uuid".into(),
            identifier: "STUDIO-872".into(),
            owner: "makewhatis".into(),
            repo: "rhapsody".into(),
            linked: Vec::new(),
        }
    }

    #[tokio::test]
    async fn a_target_is_linked_to_the_pull_request() {
        let l = Recording::new(None);
        link_pr_best_effort(Some(l.as_ref()), Some(&target()), PR).await;
        assert_eq!(l.calls(), vec![("iss-uuid".to_string(), PR.to_string())]);
    }

    /// The gate that keeps a working installation's Linear write count at zero: the control task
    /// decided there was nothing to link, and the off-loop path must not second-guess it.
    #[tokio::test]
    async fn no_target_writes_nothing() {
        let l = Recording::new(None);
        link_pr_best_effort(Some(l.as_ref()), None, PR).await;
        assert!(l.calls().is_empty());
    }

    /// An incomplete coordinate is not a write worth making — and is the shape a future caller
    /// building a target from a half-filled snapshot would hand us.
    #[tokio::test]
    async fn an_incomplete_coordinate_writes_nothing() {
        let l = Recording::new(None);
        let empty_issue = PrLinkTarget {
            issue_id: String::new(),
            ..target()
        };
        link_pr_best_effort(Some(l.as_ref()), Some(&empty_issue), PR).await;
        link_pr_best_effort(Some(l.as_ref()), Some(&target()), "").await;
        assert!(l.calls().is_empty());
    }

    /// Best-effort means the caller is never handed a failure to thread: a refused link returns
    /// normally, having tried.
    #[tokio::test]
    async fn a_refused_link_is_swallowed_after_being_attempted() {
        let l = Recording::new(Some(TrackerError::Other("linear said no".into())));
        link_pr_best_effort(Some(l.as_ref()), Some(&target()), PR).await;
        assert_eq!(l.calls().len(), 1, "the attempt still happened");
    }

    // ── the gate ────────────────────────────────────────────────────────────────────────────────

    fn issue(prs: Vec<rhapsody_core::LinkedPRRef>) -> rhapsody_core::Issue {
        rhapsody_core::Issue {
            id: "iss-uuid".into(),
            identifier: "STUDIO-872".into(),
            linked_prs: (!prs.is_empty()).then_some(prs),
            ..Default::default()
        }
    }

    fn linked(owner: &str, repo: &str, number: i64, merged: bool) -> rhapsody_core::LinkedPRRef {
        rhapsody_core::LinkedPRRef {
            owner: owner.into(),
            repo: repo.into(),
            number,
            merged,
        }
    }

    /// STUDIO-875's population: `attachments: []` on every issue, so every ticket carries an
    /// empty link set and every resolved pull request is new to it.
    #[test]
    fn a_ticket_with_no_linked_pull_request_carries_an_empty_link_set() {
        assert_eq!(
            pr_link_target(&issue(vec![]), "makewhatis", "rhapsody"),
            Some(target())
        );
    }

    /// What the ticket links in THIS repository, whatever the tracker says about its state: the
    /// numbers are the gate, and `merged` is a field nothing on these installations maintains.
    #[test]
    fn the_target_carries_this_repository_s_linked_numbers_whatever_their_state() {
        let iss = issue(vec![
            linked("makewhatis", "rhapsody", 157, false),
            linked("makewhatis", "rhapsody", 154, true),
        ]);
        let got = pr_link_target(&iss, "makewhatis", "rhapsody").expect("a target");
        assert_eq!(got.linked, vec![154, 157], "ascending, merged included");
        assert_eq!(
            pr_link_target(&iss, "MakeWhatIs", "Rhapsody").map(|t| t.linked),
            Some(vec![154, 157]),
            "GitHub owner/repo are case-insensitive"
        );
    }

    /// The link set is per-repository, exactly as `apply_github_summons`' walk is: an attachment in
    /// another repository can never attribute a summons in this one, so it can never make this
    /// repository's pull request "already linked" either.
    #[test]
    fn a_link_in_another_repository_is_not_carried() {
        let iss = issue(vec![linked("makewhatis", "tally", 246, false)]);
        assert_eq!(
            pr_link_target(&iss, "makewhatis", "rhapsody").map(|t| t.linked),
            Some(Vec::new())
        );
    }

    /// Nothing to attach to is not a link worth attempting.
    #[test]
    fn an_issue_with_no_tracker_id_wants_nothing() {
        let iss = rhapsody_core::Issue {
            id: String::new(),
            ..issue(vec![])
        };
        assert_eq!(pr_link_target(&iss, "makewhatis", "rhapsody"), None);
    }

    // ── the stale-link trap (round 2) ───────────────────────────────────────────────────────────
    //
    // These compose the two halves — the control task's gate and the off-loop write — because the
    // defect they pin lives in neither alone: it is the gate answering a question about a FIELD
    // NOBODY MAINTAINS on the very installations this module exists for.

    /// The whole change is premised on Linear's GitHub integration being absent, so nothing keeps
    /// an attachment's `metadata.status` fresh either: a daemon-written link reads `unmerged`
    /// forever, including after its pull request has merged. A gate that trusted that field would
    /// refuse to attach the ticket's SECOND pull request — and a reviewer's findings on it would
    /// be dropped, silently, which is this ticket's own bug one round later.
    #[tokio::test]
    async fn a_stale_unmerged_link_does_not_block_the_ticket_s_next_pull_request() {
        let iss = issue(vec![linked("makewhatis", "rhapsody", 154, false)]);
        let target = pr_link_target(&iss, "makewhatis", "rhapsody");
        let l = Recording::new(None);
        link_pr_best_effort(
            Some(l.as_ref()),
            target.as_ref(),
            "https://github.com/makewhatis/rhapsody/pull/157",
        )
        .await;
        assert_eq!(
            l.calls(),
            vec![(
                "iss-uuid".to_string(),
                "https://github.com/makewhatis/rhapsody/pull/157".to_string()
            )],
            "a link to a DIFFERENT pull request says nothing about this one"
        );
    }

    /// A URL the ticket's link set says nothing about writes, and both shapes reach here for real:
    /// the quorum's `resolve_open_pr` falls back to the ticket's own attachment when the `gh`
    /// lookup fails, and a malformed URL is always one refusal away. Writing a duplicate costs the
    /// tracker one redundant row; skipping wrongly costs an author their next review.
    #[tokio::test]
    async fn a_pull_request_the_link_set_cannot_speak_for_is_written() {
        let iss = issue(vec![linked("makewhatis", "rhapsody", 154, false)]);
        let target = pr_link_target(&iss, "makewhatis", "rhapsody");
        let l = Recording::new(None);
        // #154 — the same NUMBER as the ticket's link, in a different repository.
        link_pr_best_effort(
            Some(l.as_ref()),
            target.as_ref(),
            "https://github.com/makewhatis/tally/pull/154",
        )
        .await;
        link_pr_best_effort(Some(l.as_ref()), target.as_ref(), "not-a-pull-request-url").await;
        assert_eq!(l.calls().len(), 2, "neither is provably already linked");
    }

    /// And the other side of the same gate, which is what keeps a healthy installation's Linear
    /// write count at zero: the ticket's attachment IS the pull request just resolved, so there is
    /// nothing to write.
    #[tokio::test]
    async fn a_ticket_already_linked_to_the_resolved_pull_request_costs_no_write() {
        let iss = issue(vec![linked("makewhatis", "rhapsody", 154, false)]);
        let target = pr_link_target(&iss, "makewhatis", "rhapsody");
        let l = Recording::new(None);
        link_pr_best_effort(Some(l.as_ref()), target.as_ref(), PR).await;
        assert!(
            l.calls().is_empty(),
            "PR is #154, and #154 is already linked"
        );
    }
}
