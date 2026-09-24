//! runmanagerapply — the manager's off-loop APPLIER (STUDIO-1016, design record
//! `manager-agent-design.md` §7.6, §8.3). **No Go v0.4.0 counterpart.**
//!
//! Structured like [`crate::runautomerge`]: the control task writes every LOCAL record (the
//! `effects_json` status, the pending approval) and hands the external work here. Before EACH
//! external effect this task makes a cheap check round-trip to the control task
//! ([`ControlHandle::manager_apply_check`]), so a revoked decision stops early — the decisive check
//! is still the activation transaction (§7.7), and this only saves work.
//!
//! Every comment carries a hidden marker; after a timeout or error the applier searches for it
//! (bounded) and posts at most once more. The guarantee is **at-least-once**: a duplicate is
//! harmless because no manager comment carries a summon token or authority (§7.9).

use std::sync::Arc;

use crate::ghsummons::{PrCommentSearch, PrCommentSink};
use crate::managerapply::{
    MANAGER_EFFECT_DONE, MANAGER_EFFECT_EXPLANATION, MANAGER_EFFECT_FAILED,
    MANAGER_EFFECT_TICKET_MOVE, MANAGER_EFFECT_UNKNOWN, ManagerApplyRequest, ManagerEffectResult,
};
use crate::managerapply::{ManagerApplySink, PreEffectCheck};

/// The number of marker searches the applier makes after a comment error (§7.6): bounded, so a
/// late-completing request can be caught without spinning forever.
const MARKER_SEARCH_ATTEMPTS: usize = 3;

/// The pause between marker searches. Three searches over roughly a minute is deliberately far
/// inside the design's "at most 3 searches over 10 minutes": the applier runs on its own task and
/// must not hold the run forever, and a missed search only costs one duplicate comment.
const MARKER_SEARCH_BACKOFF: std::time::Duration = std::time::Duration::from_secs(20);

/// The applier performs the ticket move through
/// [`ControlHandle::move_issue_state`](crate::stop::ControlHandle) — the same by-NAME
/// `MoveIssueState` the daemon's own route-back uses.
/// Everything the applier task needs. No `Orchestrator`, no store: it performs external effects and
/// reports back through the control handle.
pub struct ManagerApplyDeps {
    /// The control round-trip (check + result) and the ticket move.
    pub control: crate::stop::ControlHandle,
    /// The comment POST seam. `None` when `gh` is unavailable — no comment can be delivered, so the
    /// explanation is `unknown` and never grants activation.
    pub comments: Option<Arc<dyn PrCommentSink>>,
    /// The comment SEARCH seam, for reconciling a timed-out post by its marker. `None` disables the
    /// search, and a failed post is then `unknown`.
    pub search: Option<Arc<dyn PrCommentSearch>>,
}

/// A sender that never blocks the control task: the control task's `submit` only enqueues.
pub struct ManagerApplyChannel(pub tokio::sync::mpsc::UnboundedSender<ManagerApplyRequest>);

impl ManagerApplySink for ManagerApplyChannel {
    fn submit(&self, request: ManagerApplyRequest) {
        let _ = self.0.send(request);
    }
}

/// The applier task: serially perform each submitted request (comments and ticket moves are
/// rate-limited by GitHub and the tracker, and one intervention at a time is the norm).
pub async fn run_manager_apply_task(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<ManagerApplyRequest>,
    deps: ManagerApplyDeps,
) {
    while let Some(request) = rx.recv().await {
        perform_manager_apply(request, &deps).await;
    }
}

/// Perform one request's effects in order, reporting the result to the control task. Infallible by
/// contract: every failure is folded into the reported outcome, never propagated.
pub async fn perform_manager_apply(request: ManagerApplyRequest, deps: &ManagerApplyDeps) {
    let mut outcomes: Vec<(String, String)> = Vec::new();
    let mut reason = String::new();
    for effect in &request.effects {
        // The §8.3 check before EACH external effect: a revoked decision stops early.
        match deps
            .control
            .manager_apply_check(&request.intervention_id)
            .await
        {
            PreEffectCheck::Proceed => {}
            check => {
                let halted = if check == PreEffectCheck::Stale {
                    "stale"
                } else {
                    "superseded"
                };
                deps.control.manager_effect_result(ManagerEffectResult {
                    intervention_id: request.intervention_id.clone(),
                    pr: request.pr.clone(),
                    outcomes,
                    reason: format!("stopped before `{effect}`: {halted}"),
                    halted: Some(halted.to_string()),
                });
                return;
            }
        }
        let state = match effect.as_str() {
            MANAGER_EFFECT_EXPLANATION | "escalation" | "unapplied" => {
                deliver_comment(&request, deps).await
            }
            MANAGER_EFFECT_TICKET_MOVE => match &request.ticket_move {
                Some(mv) => match deps
                    .control
                    .move_issue_state(&mv.issue_id, &mv.team_id, &mv.state)
                    .await
                {
                    Ok(()) => MANAGER_EFFECT_DONE.to_string(),
                    Err(e) => {
                        reason = format!("ticket move failed: {e}"); // A move that cannot be confirmed is `unknown`; the activation transaction
                        // refuses, and the wake obligation is never written (§7.9).
                        MANAGER_EFFECT_UNKNOWN.to_string()
                    }
                },
                None => {
                    reason = "ticket move requested with no resolved coordinates".to_string();
                    MANAGER_EFFECT_FAILED.to_string()
                }
            },
            other => {
                reason = format!("unknown effect `{other}`");
                MANAGER_EFFECT_FAILED.to_string()
            }
        };
        outcomes.push((effect.clone(), state));
    }
    deps.control.manager_effect_result(ManagerEffectResult {
        intervention_id: request.intervention_id.clone(),
        pr: request.pr.clone(),
        outcomes,
        reason,
        halted: None,
    });
}

/// Post one comment, reconciling a timeout or error by searching for its marker (§7.6). Bounded:
/// at most [`MARKER_SEARCH_ATTEMPTS`] searches, then at most one more post. At-least-once.
async fn deliver_comment(request: &ManagerApplyRequest, deps: &ManagerApplyDeps) -> String {
    let (owner, repo, number) = (
        request.owner.as_str(),
        request.repo.as_str(),
        request.number,
    );
    let Some(comments) = deps.comments.as_ref() else {
        return MANAGER_EFFECT_UNKNOWN.to_string();
    };
    match comments
        .post_pr_comment(owner, repo, number, &request.explanation)
        .await
    {
        Ok(()) => return MANAGER_EFFECT_DONE.to_string(),
        Err(e) => {
            if is_definitive_rejection(&e.to_string()) {
                // A 4xx rejection is `apply_failed` (§7.6): the comment will never land.
                return MANAGER_EFFECT_FAILED.to_string();
            }
        }
    }
    // The request may complete late: search for the marker before posting again.
    for _ in 0..MARKER_SEARCH_ATTEMPTS {
        if marker_present(request, deps).await {
            return MANAGER_EFFECT_DONE.to_string();
        }
        tokio::time::sleep(MARKER_SEARCH_BACKOFF).await;
    }
    if marker_present(request, deps).await {
        return MANAGER_EFFECT_DONE.to_string();
    }
    match comments
        .post_pr_comment(owner, repo, number, &request.explanation)
        .await
    {
        Ok(()) => MANAGER_EFFECT_DONE.to_string(),
        Err(_) => MANAGER_EFFECT_UNKNOWN.to_string(),
    }
}

/// Whether the request's marker is already among the pull request's comments.
async fn marker_present(request: &ManagerApplyRequest, deps: &ManagerApplyDeps) -> bool {
    let Some(search) = deps.search.as_ref() else {
        return false;
    };
    match search
        .pr_comment_bodies(&request.owner, &request.repo, request.number)
        .await
    {
        Ok(bodies) => bodies.iter().any(|b| b.contains(&request.marker)),
        Err(_) => false,
    }
}

/// Whether a `gh` error names a definitive rejection (a 4xx-class status), which makes the comment
/// `apply_failed` rather than merely uncertain.
fn is_definitive_rejection(err: &str) -> bool {
    ["403", "404", "410", "422"]
        .iter()
        .any(|code| err.contains(code))
}
