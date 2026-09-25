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
    MANAGER_EFFECT_DONE, MANAGER_EFFECT_ESCALATION, MANAGER_EFFECT_EXPLANATION,
    MANAGER_EFFECT_FAILED, MANAGER_EFFECT_TICKET_MOVE, MANAGER_EFFECT_UNAPPLIED,
    MANAGER_EFFECT_UNKNOWN, ManagerApplyRequest, ManagerEffectResult, ManagerTicketMove,
};
use crate::managerapply::{ManagerApplySink, PreEffectCheck};

/// The number of marker searches the applier makes after a comment error (§7.6): bounded, so a
/// late-completing request can be caught without spinning forever.
const MARKER_SEARCH_ATTEMPTS: usize = 3;

/// The pause between marker searches. Three searches over roughly a minute is deliberately far
/// inside the design's "at most 3 searches over 10 minutes": the applier runs on its own task and
/// must not hold the run forever, and a missed search only costs one duplicate comment.
const MARKER_SEARCH_BACKOFF: std::time::Duration = std::time::Duration::from_secs(20);

/// The number of times a ticket move's state is read back after an uncertain move (§7.6). The
/// design's bound: after 3 reads the effect is `unknown` and the intervention `apply_uncertain`.
const TICKET_MOVE_READS: usize = 3;

/// The pause between ticket-move read-backs.
const TICKET_MOVE_READ_BACKOFF: std::time::Duration = std::time::Duration::from_secs(1);

/// Whether an effect is a BEST-EFFORT NOTICE rather than a grant. The explanation and the ticket
/// move are mandatory and gated by the §8.3 check; the `unapplied` follow-up and the `escalation`
/// question grant nothing, so the check that exists to stop a grant must not stop them — otherwise
/// a refusal's own notice is halted by the very cause it reports.
fn is_best_effort(effect: &str) -> bool {
    effect == MANAGER_EFFECT_UNAPPLIED || effect == MANAGER_EFFECT_ESCALATION
}

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
        // The §8.3 check before EACH external effect: a revoked decision stops early. A best-effort
        // notice is exempt — it grants nothing, so it must post even when the decision it reports
        // was refused.
        if !is_best_effort(effect) {
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
        }
        let state = match effect.as_str() {
            MANAGER_EFFECT_EXPLANATION | MANAGER_EFFECT_ESCALATION | MANAGER_EFFECT_UNAPPLIED => {
                deliver_comment(&request, deps).await
            }
            MANAGER_EFFECT_TICKET_MOVE => match &request.ticket_move {
                Some(mv) => {
                    let moved = deps
                        .control
                        .move_issue_state(&mv.issue_id, &mv.team_id, &mv.state)
                        .await;
                    // The move is CONFIRMED by reading the state back (§7.6), not by the command
                    // returning: a move that timed out may still have landed. A move to a state the
                    // ticket is already in is a no-op whose read-back confirms it.
                    if confirm_ticket_move(deps, mv).await {
                        MANAGER_EFFECT_DONE.to_string()
                    } else {
                        reason = match moved {
                            Ok(()) => format!(
                                "ticket move to `{}` was not confirmed by reading the state back",
                                mv.state
                            ),
                            Err(e) => format!("ticket move failed: {e}"),
                        };
                        MANAGER_EFFECT_UNKNOWN.to_string()
                    }
                }
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
    // The request may complete late: search for the marker before posting again. At most
    // [`MARKER_SEARCH_ATTEMPTS`] searches (§7.6) — the last one is made after the final backoff, so
    // the loop itself is the whole bound and there is no fourth search after it.
    for attempt in 0..MARKER_SEARCH_ATTEMPTS {
        if marker_present(request, deps).await {
            return MANAGER_EFFECT_DONE.to_string();
        }
        if attempt + 1 < MARKER_SEARCH_ATTEMPTS {
            tokio::time::sleep(MARKER_SEARCH_BACKOFF).await;
        }
    }
    match comments
        .post_pr_comment(owner, repo, number, &request.explanation)
        .await
    {
        Ok(()) => MANAGER_EFFECT_DONE.to_string(),
        Err(_) => MANAGER_EFFECT_UNKNOWN.to_string(),
    }
}

/// §7.6: confirm a ticket move by reading the state back, bounded by [`TICKET_MOVE_READS`]. A move
/// is done only when the ticket's CURRENT state equals the moved-to state; anything else (including
/// an unreadable tracker) is unconfirmed, and the caller reports `unknown`.
///
/// The comparison is on [`rhapsody_core::normalize_state`], not the raw strings: the tracker
/// resolves a configured state NAME case-insensitively (`linear/move_state.rs`), so a config value
/// like `in progress` moves to Linear's `In Progress` and the read-back returns the display name.
/// Comparing raw strings would report every such successful move `unknown`.
async fn confirm_ticket_move(deps: &ManagerApplyDeps, mv: &ManagerTicketMove) -> bool {
    let want = rhapsody_core::normalize_state(&mv.state);
    for attempt in 0..TICKET_MOVE_READS {
        if deps
            .control
            .read_issue_state(&mv.issue_id)
            .await
            .is_some_and(|got| rhapsody_core::normalize_state(&got) == want)
        {
            return true;
        }
        if attempt + 1 < TICKET_MOVE_READS {
            tokio::time::sleep(TICKET_MOVE_READ_BACKOFF).await;
        }
    }
    false
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
///
/// Recognized ONLY in `gh`'s own `HTTP <status>` framing, so a pull-request number of 404 or a sha
/// containing 422 cannot be mistaken for a rejection. Any phrasing this does not recognize is
/// merely `unknown`, which is the at-least-once safe direction (the marker search and a re-post
/// follow).
fn is_definitive_rejection(err: &str) -> bool {
    const NEEDLE: &[u8] = b"http 4";
    let lower = err.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut i = 0;
    while i + NEEDLE.len() <= bytes.len() {
        if &bytes[i..i + NEEDLE.len()] == NEEDLE {
            let rest = &bytes[i + NEEDLE.len()..];
            if rest.len() >= 2 && rest[0].is_ascii_digit() && rest[1].is_ascii_digit() {
                return true;
            }
        }
        i += 1;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ghsummons::{PrCommentResult, PrCommentSearchResult};
    use crate::orchestrator::Orchestrator;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct RecordingComments(Mutex<Vec<String>>);

    #[async_trait::async_trait]
    impl PrCommentSink for RecordingComments {
        async fn post_pr_comment(
            &self,
            _owner: &str,
            _repo: &str,
            _number: i64,
            body: &str,
        ) -> PrCommentResult {
            self.0.lock().expect("comments").push(body.to_string());
            Ok(())
        }
    }

    struct EmptySearch;

    #[async_trait::async_trait]
    impl PrCommentSearch for EmptySearch {
        async fn pr_comment_bodies(
            &self,
            _owner: &str,
            _repo: &str,
            _number: i64,
        ) -> PrCommentSearchResult {
            Ok(Vec::new())
        }
    }

    /// A control handle whose §8.3 check answers `check`, whose ticket writes go to `tracker`, and
    /// which records every result the applier reports back. Built on [`Orchestrator::control`] with
    /// its event channel replaced, exactly as the HTTP task drives a handle with no control loop.
    fn fake_control(
        check: PreEffectCheck,
        tracker: Option<Arc<rhapsody_tracker::fake::Fake>>,
    ) -> (
        crate::stop::ControlHandle,
        Arc<Mutex<Vec<ManagerEffectResult>>>,
    ) {
        let o = Orchestrator::new("WORKFLOW.md");
        let mut handle = o.control();
        if let Some(t) = tracker {
            handle.tracker = Some(t);
        }
        let results: Arc<Mutex<Vec<ManagerEffectResult>>> = Arc::new(Mutex::new(Vec::new()));
        let results2 = Arc::clone(&results);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        handle.events = tx;
        tokio::spawn(async move {
            while let Some(ev) = rx.recv().await {
                match ev {
                    crate::control_loop::Event::ManagerApplyCheck { reply, .. } => {
                        let _ = reply.send(check);
                    }
                    crate::control_loop::Event::ManagerEffect(result) => {
                        results2.lock().expect("results").push(*result);
                    }
                    _ => {}
                }
            }
        });
        (handle, results)
    }

    fn deps(
        check: PreEffectCheck,
        tracker: Option<Arc<rhapsody_tracker::fake::Fake>>,
        comments: Arc<RecordingComments>,
    ) -> (ManagerApplyDeps, Arc<Mutex<Vec<ManagerEffectResult>>>) {
        let (control, results) = fake_control(check, tracker);
        (
            ManagerApplyDeps {
                control,
                comments: Some(comments as Arc<dyn PrCommentSink>),
                search: Some(Arc::new(EmptySearch) as Arc<dyn PrCommentSearch>),
            },
            results,
        )
    }

    /// Wait until the applier's result has reached the fake's event loop, then return it.
    async fn take_results(
        results: &Arc<Mutex<Vec<ManagerEffectResult>>>,
    ) -> Vec<ManagerEffectResult> {
        for _ in 0..200 {
            {
                let r = results.lock().expect("results");
                if !r.is_empty() {
                    return r.clone();
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        results.lock().expect("results").clone()
    }

    fn comment_request(effect: &str) -> ManagerApplyRequest {
        ManagerApplyRequest {
            intervention_id: "iv-1".to_string(),
            pr: "o/r#1".to_string(),
            owner: "o".to_string(),
            repo: "r".to_string(),
            number: 1,
            effects: vec![effect.to_string()],
            explanation: format!("notice\n<!-- rhapsody-manager:iv-1:{effect} -->"),
            marker: format!("<!-- rhapsody-manager:iv-1:{effect} -->"),
            ticket_move: None,
        }
    }

    // §7.6/§15.4 (B2): a refusal's best-effort "not applied" notice posts even though the §8.3 check
    // answers `Superseded` — the check exists to stop a GRANT, and the notice grants nothing. Without
    // the exemption the applier halts before the post and the already-published explanation is never
    // corrected. MUTATION: run the check for every effect and this reds.
    #[tokio::test]
    async fn a_not_applied_notice_posts_despite_a_halting_check() {
        let comments = Arc::new(RecordingComments::default());
        let (deps, _results) = deps(PreEffectCheck::Superseded, None, Arc::clone(&comments));
        perform_manager_apply(comment_request(MANAGER_EFFECT_UNAPPLIED), &deps).await;
        assert_eq!(
            comments.0.lock().expect("comments").len(),
            1,
            "the not-applied notice is best effort and is not halted by its own cause"
        );
    }

    // The same exemption for the escalation question.
    #[tokio::test]
    async fn an_escalation_question_posts_despite_a_halting_check() {
        let comments = Arc::new(RecordingComments::default());
        let (deps, _results) = deps(PreEffectCheck::Superseded, None, Arc::clone(&comments));
        perform_manager_apply(comment_request(MANAGER_EFFECT_ESCALATION), &deps).await;
        assert_eq!(comments.0.lock().expect("comments").len(), 1);
    }

    // The MANDATORY explanation is NOT exempt: a halting check stops it before it posts.
    #[tokio::test]
    async fn a_mandatory_explanation_is_still_halted_by_the_check() {
        let comments = Arc::new(RecordingComments::default());
        let (deps, results) = deps(PreEffectCheck::Superseded, None, Arc::clone(&comments));
        perform_manager_apply(comment_request(MANAGER_EFFECT_EXPLANATION), &deps).await;
        assert!(
            comments.0.lock().expect("comments").is_empty(),
            "a revoked decision must not post its mandatory explanation"
        );
        let results = take_results(&results).await;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].halted.as_deref(), Some("superseded"));
    }

    // §7.6 (B3): a ticket move is confirmed by reading the state back, so a move that returned a
    // transient error but actually landed is `done` rather than `apply_uncertain`.
    #[tokio::test]
    async fn a_ticket_move_is_confirmed_by_reading_the_state_back() {
        let mut fake = rhapsody_tracker::fake::Fake::new();
        fake.move_err = Some(rhapsody_tracker::TrackerError::Other(
            "timed out".to_string(),
        ));
        fake.by_id.insert(
            "uuid-1".to_string(),
            rhapsody_core::Issue {
                id: "uuid-1".to_string(),
                state: "changes".to_string(),
                ..Default::default()
            },
        );
        let fake = Arc::new(fake);
        let comments = Arc::new(RecordingComments::default());
        let (deps, results) = deps(PreEffectCheck::Proceed, Some(Arc::clone(&fake)), comments);
        let mut request = comment_request(MANAGER_EFFECT_TICKET_MOVE);
        request.effects = vec![MANAGER_EFFECT_TICKET_MOVE.to_string()];
        request.ticket_move = Some(ManagerTicketMove {
            issue_id: "uuid-1".to_string(),
            team_id: "team".to_string(),
            state: "changes".to_string(),
        });
        perform_manager_apply(request, &deps).await;
        let results = take_results(&results).await;
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].outcomes,
            vec![(
                MANAGER_EFFECT_TICKET_MOVE.to_string(),
                MANAGER_EFFECT_DONE.to_string()
            )],
            "a move that landed is confirmed by the read-back even though the command errored"
        );
    }

    // §7.6 (B6): the read-back compares NORMALIZED states, because the tracker resolves a
    // configured state name case-insensitively. A config value `in progress` moves the ticket to
    // Linear's `In Progress`, and the read-back returns the display name; a raw-string compare would
    // report every such successful move `unknown` and stop the generation. MUTATION: compare the raw
    // strings and this reds.
    #[tokio::test]
    async fn a_ticket_move_read_back_compares_normalized_states() {
        let mut fake = rhapsody_tracker::fake::Fake::new();
        fake.by_id.insert(
            "uuid-1".to_string(),
            rhapsody_core::Issue {
                id: "uuid-1".to_string(),
                state: "In Progress".to_string(),
                ..Default::default()
            },
        );
        let fake = Arc::new(fake);
        let comments = Arc::new(RecordingComments::default());
        let (deps, results) = deps(PreEffectCheck::Proceed, Some(Arc::clone(&fake)), comments);
        let mut request = comment_request(MANAGER_EFFECT_TICKET_MOVE);
        request.effects = vec![MANAGER_EFFECT_TICKET_MOVE.to_string()];
        request.ticket_move = Some(ManagerTicketMove {
            issue_id: "uuid-1".to_string(),
            team_id: "team".to_string(),
            state: "in progress".to_string(),
        });
        perform_manager_apply(request, &deps).await;
        let results = take_results(&results).await;
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].outcomes,
            vec![(
                MANAGER_EFFECT_TICKET_MOVE.to_string(),
                MANAGER_EFFECT_DONE.to_string()
            )],
            "a name resolved case-insensitively is confirmed by the normalized read-back"
        );
    }

    // A move whose state cannot be read back is `unknown`, which makes the intervention
    // `apply_uncertain` — the fail-closed direction.
    #[tokio::test]
    async fn an_unconfirmed_ticket_move_is_unknown() {
        let comments = Arc::new(RecordingComments::default());
        let (deps, results) = deps(
            PreEffectCheck::Proceed,
            Some(Arc::new(rhapsody_tracker::fake::Fake::new())),
            comments,
        );
        let mut request = comment_request(MANAGER_EFFECT_TICKET_MOVE);
        request.effects = vec![MANAGER_EFFECT_TICKET_MOVE.to_string()];
        request.ticket_move = Some(ManagerTicketMove {
            issue_id: "uuid-1".to_string(),
            team_id: "team".to_string(),
            state: "changes".to_string(),
        });
        perform_manager_apply(request, &deps).await;
        let results = take_results(&results).await;
        assert_eq!(
            results[0].outcomes,
            vec![(
                MANAGER_EFFECT_TICKET_MOVE.to_string(),
                MANAGER_EFFECT_UNKNOWN.to_string()
            )]
        );
    }

    // A 4xx rejection is recognized only in gh's own `HTTP <status>` framing, so a PR number or a
    // sha containing a 4xx-looking run of digits is not mistaken for one.
    #[test]
    fn is_definitive_rejection_recognizes_only_http_4xx() {
        assert!(is_definitive_rejection(
            "gh pr comment 5 --repo o/r: gh ... exited with 1: HTTP 422: Validation Failed"
        ));
        assert!(is_definitive_rejection("HTTP 401: Bad credentials"));
        assert!(!is_definitive_rejection(
            "gh pr comment 404 --repo o/r: boom"
        ));
        assert!(!is_definitive_rejection("deadbeef422 sha in a message"));
        assert!(!is_definitive_rejection("HTTP 500: server error"));
        assert!(!is_definitive_rejection("timed out after 60s"));
    }

    // §7.6 / §15.4 (Activation and delivery): a comment request that TIMES OUT may still have
    // landed. The applier makes its bounded marker search before posting again, finds the late
    // comment, and reports `done` — so the duplicate the design accepts as harmless is not even
    // produced when the marker is visible. MUTATION: skip the marker search and re-post blindly;
    // this reds (two attempts on a request that had already succeeded).
    struct FlakyComments {
        attempts: Mutex<usize>,
    }

    #[async_trait::async_trait]
    impl PrCommentSink for FlakyComments {
        async fn post_pr_comment(
            &self,
            _owner: &str,
            _repo: &str,
            _number: i64,
            _body: &str,
        ) -> PrCommentResult {
            let mut n = self.attempts.lock().expect("attempts");
            *n += 1;
            // A timeout, not a 4xx: not definitive, so the marker search runs. The request may
            // still have completed on GitHub's side — which the search then proves.
            if *n == 1 {
                Err("HTTP 504: gateway timeout".into())
            } else {
                Ok(())
            }
        }
    }

    struct MarkerSearch(String);

    #[async_trait::async_trait]
    impl PrCommentSearch for MarkerSearch {
        async fn pr_comment_bodies(
            &self,
            _owner: &str,
            _repo: &str,
            _number: i64,
        ) -> PrCommentSearchResult {
            Ok(vec![self.0.clone()])
        }
    }

    #[tokio::test]
    async fn a_late_comment_is_reconciled_by_its_marker() {
        let comments = Arc::new(FlakyComments {
            attempts: Mutex::new(0),
        });
        let request = comment_request(MANAGER_EFFECT_EXPLANATION);
        let (control, results) = fake_control(PreEffectCheck::Proceed, None);
        let deps = ManagerApplyDeps {
            control,
            comments: Some(Arc::clone(&comments) as Arc<dyn PrCommentSink>),
            search: Some(Arc::new(MarkerSearch(request.marker.clone())) as Arc<dyn PrCommentSearch>),
        };

        perform_manager_apply(request, &deps).await;
        let results = take_results(&results).await;
        assert_eq!(
            results[0].outcomes,
            vec![(
                MANAGER_EFFECT_EXPLANATION.to_string(),
                MANAGER_EFFECT_DONE.to_string()
            )],
            "the late comment is found by its marker and reported done"
        );
        assert_eq!(
            *comments.attempts.lock().expect("attempts"),
            1,
            "the marker search avoids a second post entirely"
        );
    }

    // §7.6 / §8.3 / §15.4 (Merge freshness): the check before EACH effect. A decision revoked
    // between two mandatory effects stops at the second, and the report names what ran and what
    // did not — a done/cancelled report with nothing further applied. MUTATION: check only once
    // before the first effect and the ticket move would run after the authority was revoked.
    fn fake_control_seq(
        checks: Vec<PreEffectCheck>,
        tracker: Option<Arc<rhapsody_tracker::fake::Fake>>,
    ) -> (
        crate::stop::ControlHandle,
        Arc<Mutex<Vec<ManagerEffectResult>>>,
    ) {
        let o = Orchestrator::new("WORKFLOW.md");
        let mut handle = o.control();
        if let Some(t) = tracker {
            handle.tracker = Some(t);
        }
        let results: Arc<Mutex<Vec<ManagerEffectResult>>> = Arc::new(Mutex::new(Vec::new()));
        let results2 = Arc::clone(&results);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        handle.events = tx;
        tokio::spawn(async move {
            let mut queue: std::collections::VecDeque<PreEffectCheck> = checks.into();
            while let Some(ev) = rx.recv().await {
                match ev {
                    crate::control_loop::Event::ManagerApplyCheck { reply, .. } => {
                        let check = queue.pop_front().unwrap_or(PreEffectCheck::Proceed);
                        let _ = reply.send(check);
                    }
                    crate::control_loop::Event::ManagerEffect(result) => {
                        results2.lock().expect("results").push(*result);
                    }
                    _ => {}
                }
            }
        });
        (handle, results)
    }

    #[tokio::test]
    async fn a_revoked_decision_mid_effects_reports_done_and_cancelled() {
        let comments = Arc::new(RecordingComments::default());
        let (control, results) = fake_control_seq(
            vec![PreEffectCheck::Proceed, PreEffectCheck::Superseded],
            None,
        );
        let deps = ManagerApplyDeps {
            control,
            comments: Some(Arc::clone(&comments) as Arc<dyn PrCommentSink>),
            search: Some(Arc::new(EmptySearch) as Arc<dyn PrCommentSearch>),
        };
        let mut request = comment_request(MANAGER_EFFECT_EXPLANATION);
        request.effects = vec![
            MANAGER_EFFECT_EXPLANATION.to_string(),
            MANAGER_EFFECT_TICKET_MOVE.to_string(),
        ];
        request.ticket_move = Some(ManagerTicketMove {
            issue_id: "uuid-1".to_string(),
            team_id: "team".to_string(),
            state: "changes".to_string(),
        });

        perform_manager_apply(request, &deps).await;
        let results = take_results(&results).await;
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].outcomes,
            vec![(
                MANAGER_EFFECT_EXPLANATION.to_string(),
                MANAGER_EFFECT_DONE.to_string()
            )],
            "the first effect ran; the second never did"
        );
        assert_eq!(
            results[0].halted.as_deref(),
            Some("superseded"),
            "the report names the revocation and what was left undone"
        );
        assert_eq!(
            comments.0.lock().expect("comments").len(),
            1,
            "the revoked ticket move was never attempted"
        );
    }
}
