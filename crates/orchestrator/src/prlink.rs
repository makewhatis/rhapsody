//! prlink — the daemon writes the Linear↔GitHub link its own summons routing depends on
//! (STUDIO-875).
//!
//! **No Go v0.4.0 counterpart.** Symphony only ever READ GitHub attachments, on the assumption
//! that Linear's own GitHub integration had written them.
//!
//! # The drop this exists to end
//!
//! [`crate::ghenrich::apply_github_summons`] attributes a summoning pull-request comment to a
//! ticket by walking that ticket's `linked_prs`, which the tracker builds from the issue's GitHub
//! attachments. A Linear workspace whose repository is not connected in the GitHub integration
//! answers `attachments: []` on every issue, so the walk has nothing to walk — and a review that
//! files findings posts a perfectly good token-bearing comment which is then dropped on every poll,
//! forever. `latest_summon_at` is never advanced, so `review_reopen_eligible` can never fire, and
//! the board shows an idle ticket with an open pull request: indistinguishable from "the reviewer
//! approved and there is nothing left to do".
//!
//! Connecting the repository in Linear repairs it for that repository, invisibly to anyone reading
//! this code, and silently omits the next repository somebody adds. So the daemon writes the link
//! itself, at the one moment it has both halves in hand: when it has just resolved a pull request
//! for a ticket, off-loop, on the review-introduction path.
//!
//! # Best-effort, and the word is load-bearing
//!
//! A failed link must never fail the thing that was actually asked for — the review introduction,
//! or the quorum fan-out. It costs the NEXT summons on that pull request, and it is retried by
//! whatever next resolves a pull request for that ticket: another handoff, or
//! [`crate::reviewadopt`]'s sweep. **That is not "every tick", and on the quorum path it is not
//! even every handoff** — `fan_out` returns at `AlreadyRequestedAtHead` before it resolves a
//! tracker, so a repeat handoff at an unchanged head retries nothing. The backstop for a link that
//! never lands is therefore [`crate::ghenrich::UnlinkedSummons`], which names the ticket out loud
//! rather than leaving it to be discovered. Every failure here is a warning and a return, never an
//! error the caller has to thread.
//!
//! What it is NOT is silent: the two outcomes worth a line get one each, because "the daemon linked
//! it" and "the daemon tried and Linear refused" are the two facts an operator staring at an idle
//! board needs to tell apart.

use std::sync::Arc;

use async_trait::async_trait;

use rhapsody_tracker::{Tracker, TrackerError};

/// The ticket a resolved pull request should be attached to.
///
/// Decided on the control task, where the candidate snapshot lives, so the off-loop task never has
/// to ask what a ticket already carries. `None` on its `Option<PrLinkTarget>` holder means "nothing
/// to link" — which is the ordinary case on an installation whose Linear GitHub integration works,
/// and is why a correctly-configured workspace sees no new Linear writes at all.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PrLinkTarget {
    /// The tracker id the attachment is written against.
    pub issue_id: String,
    /// The human identifier, for the log line. An operator reads `STUDIO-872`, not a UUID.
    pub identifier: String,
}

/// The link a ticket needs, or `None` when it already has one.
///
/// `None` in two cases, and they are different facts that happen to want the same answer:
///
/// * the issue carries no tracker id — nothing to attach to, and nothing a write could fix;
/// * the issue already carries an UNMERGED linked pull request in this repository. Linear's own
///   GitHub integration is doing its job here (or a previous link of ours did), so
///   `apply_github_summons` can already attribute a summons and another write would buy nothing.
///
/// A MERGED linked pull request deliberately does NOT count, and the asymmetry mirrors
/// `apply_github_summons`'s own guard: it skips a merged PR when attributing, so a ticket whose
/// only attachment is a merged pull request is, for summons purposes, unlinked — and a second
/// round of work on that ticket opens a second pull request that must be attached in its own right.
pub(crate) fn pr_link_target(
    iss: &rhapsody_core::Issue,
    owner: &str,
    repo: &str,
) -> Option<PrLinkTarget> {
    if iss.id.is_empty() {
        return None;
    }
    // GitHub owner/repo are case-insensitive, and the configured repo URL and a Linear attachment
    // URL can legitimately differ in casing — the same reason `apply_github_summons` case-folds.
    let already = iss.linked_prs.iter().flatten().any(|pr| {
        !pr.merged && pr.owner.eq_ignore_ascii_case(owner) && pr.repo.eq_ignore_ascii_case(repo)
    });
    (!already).then(|| PrLinkTarget {
        issue_id: iss.id.clone(),
        identifier: iss.identifier.clone(),
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

/// Links `url` to `target`, reporting both outcomes and failing nothing.
///
/// A `None` target is the configured-workspace case and says nothing; a `None` linker is a daemon
/// with no tracker to write through, which is worth one debug line and no more.
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

    /// STUDIO-875's population: `attachments: []` on every issue, so every ticket wants a link.
    #[test]
    fn a_ticket_with_no_linked_pull_request_wants_one() {
        assert_eq!(
            pr_link_target(&issue(vec![]), "makewhatis", "rhapsody"),
            Some(target())
        );
    }

    /// A workspace whose Linear GitHub integration works must cost nothing: the daemon writes no
    /// attachment for a ticket that already has one.
    #[test]
    fn a_ticket_that_is_already_linked_wants_nothing() {
        let iss = issue(vec![linked("makewhatis", "rhapsody", 154, false)]);
        assert_eq!(pr_link_target(&iss, "makewhatis", "rhapsody"), None);
        assert_eq!(
            pr_link_target(&iss, "MakeWhatIs", "Rhapsody"),
            None,
            "GitHub owner/repo are case-insensitive"
        );
    }

    /// The guard is per-repository, exactly as `apply_github_summons`' walk is: an attachment in
    /// another repository can never attribute a summons in this one.
    #[test]
    fn a_link_in_another_repository_does_not_count() {
        let iss = issue(vec![linked("makewhatis", "tally", 246, false)]);
        assert!(pr_link_target(&iss, "makewhatis", "rhapsody").is_some());
    }

    /// A merged attachment is invisible to `apply_github_summons`, so it must be invisible here
    /// too — otherwise a ticket's SECOND pull request is never linked and its reviews go nowhere,
    /// which is the original defect with one extra step in front of it.
    #[test]
    fn a_merged_link_does_not_count() {
        let iss = issue(vec![linked("makewhatis", "rhapsody", 154, true)]);
        assert!(pr_link_target(&iss, "makewhatis", "rhapsody").is_some());
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
}
