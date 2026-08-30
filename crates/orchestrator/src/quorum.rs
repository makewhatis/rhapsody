//! quorum — Rhapsody Teams' **notified review**: a teammate's handoff fans review tickets out to
//! the least-loaded other teammates, once, opt-in (STUDIO-659, slice T7; design record
//! `~/.rhapsody/docs/STUDIO-572-rhapsody-teams.md`, §0.6 and the trigger/cap decision recorded as
//! §0.12 on 2026-08-30). **No Go v0.4.0 counterpart** — Teams is a Rhapsody addition end to end.
//!
//! §0.6 asks for one thing: when a teammate hands off a PR, **at least two other teammates review
//! it, independently**. §0.12 decided how, on one principle — *use only machinery that already
//! exists and already dispatches* — and every structural choice here follows from it:
//!
//! * **Reviewers are ordinary tickets.** One Todo issue per reviewer, labelled
//!   `rhapsody:@<reviewer>`, is the whole fan-out. Separate tickets sidestep the
//!   one-live-run-per-issue invariant, need **no new dispatch machinery at all**, and give §0.6
//!   rule 2's context isolation for free: separate worktrees, separate prompts, no shared findings.
//! * **The room notifies; Linear dispatches** (§0.6 rule 1, §0.2's "the room has no dispatch power,
//!   ever"). The manager posts *"requested review of <PR> from bob, carol"*; the TICKETS are what
//!   wake anybody. A room post that failed costs a paragraph of history and nothing else.
//! * **The findings go back on the PR, not into an aggregator.** Each reviewer posts summon
//!   comments on the pull request, which re-engages the author through the machinery INF-448 and
//!   STUDIO-649 already built. There is no synthesis step and deliberately so — the author's
//!   re-engagement IS the loop.
//!
//! # The shape is triage's, deliberately
//!
//! This module is [`crate::triage`]'s sibling in every structural respect, because the constraint
//! is the same one: **nothing here may stall the control task.** [`run_quorum_task`] takes no
//! `Orchestrator`, sends no control event, and holds no lock the control task takes. It is fed
//! [`QuorumRequest`]s — plain owned data, decided on the loop — over a channel, backs off
//! exponentially on tracker failure ([`MAX_QUORUM_BACKOFF_MS`], triage's ceiling), and posts to the
//! room through the same best-effort seam. A Linear that never answers parks *this* task and
//! nothing else.
//!
//! The one difference from triage is the clock: triage has a cadence, the quorum has an event. It
//! wakes on a handoff rather than on a timer, which is why it is a channel consumer rather than a
//! schedule.
//!
//! # Off costs exactly nothing
//!
//! `quorum.enabled` defaults **false** (§0.12's cost control: §0.6 calls notified review the most
//! expensive item in the whole revision, at ≥2 extra agent runs per handoff). With it off — or with
//! Teams off — [`Orchestrator::quorum_enabled`] is false, so: no task is spawned, the per-tick
//! candidate sweep in [`Orchestrator::record_quorum_state`] returns immediately, no
//! [`QuorumRequest`] is ever built, and no tracker method is called. That is the acceptance
//! criterion, and it is enforced at four independent points rather than one.
//!
//! # Every write is best-effort, and a partial fan-out is REPORTED, not retried
//!
//! The tradeoff, stated because it is a decision and not an accident: when 1 of 2 review tickets is
//! created, the quorum **marks the parent anyway** and names the shortfall in the room post. The
//! alternative — leaving the parent unmarked so a later handoff retries — would re-create the
//! ticket that already succeeded, and duplicate review tickets are worse than a stated gap: a human
//! reading the room can create the missing one in seconds, while a duplicate wakes a real agent
//! against a real PR for no reason. A fan-out that creates **nothing** does not mark the parent, so
//! a later handoff of the same ticket may still try.
//!
//! # What this slice deliberately does not do
//!
//! * **No human-label trigger.** Requesting a quorum on any ticket by hand is the natural
//!   follow-up; §0.12 explicitly calls it an override path, not the v1 default.
//! * **No aggregation of the reviews.** See above.
//! * **No reviewer-run catch-up suppression.** §0.12 names the leak and accepts it: a reviewer's
//!   turn-1 room catch-up could in principle show them a post the other reviewer made mid-review.
//!   In practice both dispatch before either posts, findings belong on the PR rather than the room,
//!   and suppressing catch-up for review runs is machinery §0.6's value does not justify.
//! * **Only the daemon-mediated handoff triggers it.** An agent that moves its own ticket to the
//!   review state through the Linear-MCP fallback path is not observed here. §0.12 chose "the
//!   moment the daemon already observes, because it executes the handoff", and
//!   `symphony_handoff` is that moment.

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use rhapsody_config::room::{Message, RoomLog};
use rhapsody_config::teams::Teams;
use rhapsody_core::Issue;
use rhapsody_tracker::{NewIssue, Tracker};

use crate::backoff::failure_backoff_ms;
use crate::control_loop::CancelWait;
use crate::orchestrator::Orchestrator;
use crate::teams::IDENTITY_LABEL_PREFIX;
use crate::triage::MANAGER_IDENTITY;

/// The idempotency record: the parent ticket gains this label when its quorum fires, and a ticket
/// already carrying it never fans out again (§0.12's "once per ticket").
///
/// A LABEL rather than a database row for §0.11.1's reason: Linear is the ledger, labels are
/// additive (adding one can never remove a ticket from candidacy), and the record therefore
/// survives a daemon restart, a database wipe and a second daemon — none of which an in-memory set
/// would. It sits in the same `rhapsody:` namespace as capabilities and identity labels; `quorum-`
/// cannot collide with an identity (those carry the `@`) and an unknown `rhapsody:*` label is a
/// documented silent no-op in the capabilities registry.
pub const QUORUM_REQUESTED_LABEL: &str = "rhapsody:quorum-requested";

/// The ceiling on the failure back-off, [`crate::triage::MAX_TRIAGE_BACKOFF_MS`]'s value and its
/// reason: a tracker outage settles at one attempt per 15 minutes rather than a hot retry loop.
pub const MAX_QUORUM_BACKOFF_MS: i64 = 15 * 60 * 1000;

/// What the per-tick candidate sweep learned about ONE ticket, so the handoff moment can decide
/// without asking the tracker anything (§0.12: the check reads "the candidate's already-fetched
/// labels, not a fresh read").
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct QuorumFacts {
    /// The ticket's first UNMERGED linked PR as a browser URL, or empty when it has none.
    ///
    /// Unmerged, because a merged PR needs no review. Derived from the same Linear GitHub-attachment
    /// data `linked_pr` comes from, which is why it can be read off a candidate rather than fetched.
    pub pr_url: String,
    /// Whether the ticket already carries [`QUORUM_REQUESTED_LABEL`].
    pub already_requested: bool,
}

/// One fan-out, decided on the control task and handed to the off-loop task as plain owned data.
///
/// Everything expensive is already resolved by the time this exists: the reviewers are chosen, the
/// PR is known, the state name is resolved. The task's whole job is the writes. That split is what
/// makes "no network on the dispatch path" checkable — this struct contains no handle, no `Arc`,
/// and nothing borrowed from the orchestrator.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QuorumRequest {
    /// The handed-off ticket: the tracker id (for the marker-label write), its team (review tickets
    /// are created beside the work they review) and its human identifier + title (for the fan-out's
    /// titles, descriptions and room post).
    pub parent_issue_id: String,
    pub parent_team_id: String,
    pub parent_identifier: String,
    pub parent_title: String,
    /// The pull request under review. Never empty — a handoff with no linked PR builds no request
    /// at all, because there would be nothing for a reviewer to read.
    pub pr_url: String,
    /// The roster identity the handed-off run wore. Excluded from its own review (§0.6: "at least
    /// two OTHER teammates").
    pub author: String,
    /// The chosen reviewers, least-loaded first. **May be empty** — a roster of one produces a
    /// request whose whole effect is the loud room post §0.12 asks for, which is information a team
    /// needs and an error nobody can act on.
    pub reviewers: Vec<String>,
    /// The workflow state review tickets open in — the run's own project's first configured active
    /// state. See [`Orchestrator::quorum_create_state`] for why that and not the literal "Todo".
    pub state_name: String,
    /// The configured summon token (e.g. `@symphony`), so the reviewer's instructions name the
    /// token THIS installation actually re-engages on rather than a hard-coded guess.
    pub summon_token: String,
}

/// The live tracker one fan-out runs against, read fresh per request so a hot-reloaded tracker is
/// honoured — [`crate::triage::TriageTarget`]'s shape and its reason.
pub struct QuorumTarget {
    pub tracker: Arc<dyn Tracker>,
}

/// Everything [`run_quorum_task`] runs against. The absence of an `Orchestrator`, a control channel
/// and a store here is the off-loop guarantee, in the type.
pub struct QuorumDeps<TF> {
    /// The boot-loaded `teams.yaml`, captured once at the composition root (Teams config is not
    /// hot-reloaded in this slice, matching triage).
    pub teams: Arc<Teams>,
    /// Yields the live tracker, or `None` when no config has loaded yet.
    pub target: TF,
    /// The room the manager's post goes to; `None` when there is no room to write to (Teams without
    /// an on-disk runtime home), in which case the fan-out still happens and only the history is
    /// lost. A `dyn RoomLog` for triage's reason: nothing here runs on the control task, and the
    /// seam is what lets a test substitute a failing room to prove the fan-out survives one.
    pub room: Option<Arc<dyn RoomLog>>,
    /// The back-off ceiling; [`MAX_QUORUM_BACKOFF_MS`] in production, milliseconds in tests.
    pub max_backoff_ms: i64,
}

/// What one fan-out did — the input to the back-off decision and to the once-per-ticket bookkeeping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FanOutcome {
    /// `created` review tickets were made out of `wanted`. `created > 0`; `created < wanted` is the
    /// partial case, which still marks the parent and reports the shortfall.
    Fanned { created: usize, wanted: usize },
    /// The roster had nobody to ask. Nothing was written; a loud room post was attempted.
    NoReviewers,
    /// The tracker refused everything (no viewer to assign to, or every create failed). Back off,
    /// and leave the parent UNMARKED so a later handoff may still try.
    TrackerFailure,
}

impl FanOutcome {
    /// Whether this outcome should extend the back-off.
    fn is_failure(self) -> bool {
        matches!(self, FanOutcome::TrackerFailure)
    }

    /// Whether the parent should be considered handled for this process's lifetime. A total failure
    /// is not.
    fn settles_the_parent(self) -> bool {
        !self.is_failure()
    }
}

/// Consumes [`QuorumRequest`]s until `ctx` is cancelled or the sender is dropped (§0.12).
///
/// One request at a time, serially — there is no `spawn` in this module, so at most one fan-out is
/// ever in flight and a slow Linear cannot multiply into a burst of concurrent writes. A failed
/// fan-out delays the NEXT one by the exponential back-off, which is the "never a hot retry loop
/// against a down API" bound rather than a retry of the failed request: the quorum has no retry,
/// because a re-handoff is the only thing that should ever ask again.
pub async fn run_quorum_task<TF>(
    mut ctx: CancelWait,
    deps: QuorumDeps<TF>,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<QuorumRequest>,
) where
    TF: Fn() -> Option<QuorumTarget>,
{
    tracing::info!(
        roster = deps.teams.roster.len(),
        reviewers = deps.teams.quorum.effective_reviewers(),
        "teams review quorum task started (off-loop; a handoff is never blocked on it)"
    );
    let mut failures: i64 = 0;
    // Parents already settled in THIS process. The durable record is the marker label in Linear —
    // this only closes the window the label cannot: two handoffs of the same run before a poll
    // could refresh the candidate's labels. It grows by one entry per fan-out, which is bounded by
    // how many tickets a daemon hands off in its lifetime; a few thousand short strings.
    let mut settled: HashSet<String> = HashSet::new();
    loop {
        let req = tokio::select! {
            _ = ctx.cancelled() => return,
            r = rx.recv() => match r {
                Some(r) => r,
                // Every sender dropped: the control handle is gone, so no further handoff can
                // arrive. Nothing left to do.
                None => return,
            },
        };
        if settled.contains(&req.parent_issue_id) {
            tracing::debug!(
                issue = %req.parent_identifier,
                "teams quorum already fanned out for this ticket; ignoring the repeat handoff"
            );
            continue;
        }
        if failures > 0 {
            let delay = Duration::from_millis(
                failure_backoff_ms(failures, deps.max_backoff_ms).max(0) as u64,
            );
            tokio::select! {
                _ = ctx.cancelled() => return,
                _ = tokio::time::sleep(delay) => {}
            }
        }
        if ctx.is_cancelled() {
            return;
        }
        let outcome = fan_out(&deps, &req).await;
        if outcome.settles_the_parent() {
            settled.insert(req.parent_issue_id.clone());
        }
        if outcome.is_failure() {
            failures += 1;
            tracing::warn!(
                issue = %req.parent_identifier,
                consecutive_failures = failures,
                "teams quorum fan-out failed; backing off (the handoff itself already succeeded)"
            );
        } else {
            failures = 0;
        }
    }
}

/// One fan-out: create a review ticket per reviewer, mark the parent, tell the room.
///
/// A create that fails does NOT abort the rest — that is what produces the partial fan-out the
/// module docs describe, and it is bounded by `reviewers` (2 by default), so a down tracker costs
/// two failed calls rather than a retry storm.
pub(crate) async fn fan_out<TF>(deps: &QuorumDeps<TF>, req: &QuorumRequest) -> FanOutcome
where
    TF: Fn() -> Option<QuorumTarget>,
{
    let Some(target) = (deps.target)() else {
        tracing::warn!(
            issue = %req.parent_identifier,
            "teams quorum has no tracker yet; the fan-out is skipped"
        );
        return FanOutcome::TrackerFailure;
    };
    let tracker = target.tracker;

    if req.reviewers.is_empty() {
        // §0.12: "zero ⇒ skip with a loud room post", never an error. A team of one is a valid
        // configuration; it just cannot hold a quorum, and the operator should be able to see that
        // said out loud rather than infer it from tickets that never appeared.
        tracing::warn!(
            issue = %req.parent_identifier,
            author = %req.author,
            "teams quorum has nobody to ask: the roster holds no teammate other than the author"
        );
        post(
            deps,
            Message::room(
                MANAGER_IDENTITY,
                Utc::now(),
                format!(
                    "NO REVIEW QUORUM for {}: {} handed off {} but the roster holds no other \
                     teammate to review it. Nothing was requested. Add a teammate to \
                     `teams.yaml`, or set `quorum.enabled: false` if a one-person team is what \
                     you meant.",
                    req.parent_identifier, req.author, req.pr_url
                ),
            )
            .with_refs([req.parent_identifier.clone()]),
        );
        return FanOutcome::NoReviewers;
    }

    // §0.12's claim rule: the review ticket must be ASSIGNED, because the default candidate query
    // is keyed on `assignee == viewer` — an unassigned ticket is simply never picked up, so a
    // fan-out that could not resolve the viewer would create work nobody ever does. Cached by the
    // Linear client for its lifetime, so this is one call per daemon, not one per fan-out.
    let assignee = match tracker.resolve_viewer().await {
        Ok(v) if !v.id.is_empty() => v.id,
        other => {
            let why = match other {
                Ok(_) => "the tracker returned a viewer with no id".to_string(),
                Err(e) => e.to_string(),
            };
            tracing::warn!(
                issue = %req.parent_identifier,
                err = %why,
                "teams quorum could not resolve the viewer to assign review tickets to; nothing created"
            );
            post(
                deps,
                Message::room(
                    MANAGER_IDENTITY,
                    Utc::now(),
                    format!(
                        "REVIEW QUORUM FAILED for {}: could not resolve the tracker viewer to \
                         assign review tickets to ({why}). No review of {} was requested; an \
                         unassigned ticket is never picked up, so creating one would have been \
                         worse than creating none.",
                        req.parent_identifier, req.pr_url
                    ),
                )
                .with_refs([req.parent_identifier.clone()]),
            );
            return FanOutcome::TrackerFailure;
        }
    };

    let mut created: Vec<(String, String)> = Vec::with_capacity(req.reviewers.len());
    let mut failed: Vec<String> = Vec::new();
    for reviewer in &req.reviewers {
        let spec = NewIssue {
            team_id: req.parent_team_id.clone(),
            title: review_title(&req.parent_identifier, &req.parent_title),
            description: review_description(req, reviewer),
            state_name: req.state_name.clone(),
            assignee_id: assignee.clone(),
            labels: vec![format!("{IDENTITY_LABEL_PREFIX}{reviewer}")],
        };
        match tracker.create_issue(&spec).await {
            Ok(identifier) => {
                tracing::info!(
                    parent = %req.parent_identifier,
                    %reviewer,
                    review_ticket = %identifier,
                    "teams quorum created a review ticket"
                );
                created.push((reviewer.clone(), identifier));
            }
            Err(e) => {
                tracing::warn!(
                    parent = %req.parent_identifier,
                    %reviewer,
                    err = %e,
                    "teams quorum could not create a review ticket"
                );
                failed.push(reviewer.clone());
            }
        }
    }

    if created.is_empty() {
        post(
            deps,
            Message::room(
                MANAGER_IDENTITY,
                Utc::now(),
                format!(
                    "REVIEW QUORUM FAILED for {}: no review ticket could be created for {} \
                     (asked: {}). {} is unreviewed and the ticket is NOT marked, so a later \
                     handoff may try again.",
                    req.parent_identifier,
                    req.pr_url,
                    req.reviewers.join(", "),
                    req.parent_identifier,
                ),
            )
            .with_refs([req.parent_identifier.clone()]),
        );
        return FanOutcome::TrackerFailure;
    }

    // The marker is written even for a partial fan-out (see the module docs). Its failure is not
    // fatal either — it costs idempotency across restarts, and the in-process `settled` set still
    // holds for this daemon's lifetime — but it IS worth saying out loud, so it joins the post.
    let mut marker_err = String::new();
    if let Err(e) = tracker
        .add_issue_label(
            &req.parent_issue_id,
            &req.parent_team_id,
            QUORUM_REQUESTED_LABEL,
        )
        .await
    {
        marker_err = e.to_string();
        tracing::warn!(
            issue = %req.parent_identifier,
            err = %marker_err,
            "teams quorum could not mark the parent; a later handoff could fan out a second time"
        );
    }

    post(
        deps,
        Message::room(
            MANAGER_IDENTITY,
            Utc::now(),
            fan_out_post(req, &created, &failed, &marker_err),
        )
        .with_refs(
            std::iter::once(req.parent_identifier.clone())
                .chain(created.iter().map(|(_, id)| id.clone())),
        ),
    );
    FanOutcome::Fanned {
        created: created.len(),
        wanted: req.reviewers.len(),
    }
}

/// §0.6 rule 1's post, verbatim in substance: *"requested review of &lt;PR&gt; from bob, carol"*,
/// plus whatever went wrong. The post INFORMS; the tickets dispatch.
fn fan_out_post(
    req: &QuorumRequest,
    created: &[(String, String)],
    failed: &[String],
    marker_err: &str,
) -> String {
    let names: Vec<&str> = created.iter().map(|(n, _)| n.as_str()).collect();
    let tickets: Vec<&str> = created.iter().map(|(_, t)| t.as_str()).collect();
    let mut out = format!(
        "Requested review of {} from {} ({}), for {} handed off by {}. Findings go on the PR as \
         {} comments, which pulls {} back in; reviewers never merge.",
        req.pr_url,
        names.join(", "),
        tickets.join(", "),
        req.parent_identifier,
        req.author,
        req.summon_token,
        req.author,
    );
    if !failed.is_empty() {
        out.push_str(&format!(
            " SHORTFALL: no review ticket could be created for {} — {} of {} reviewers were asked. \
             {} is marked as requested anyway, so this will NOT be retried automatically; create \
             the missing ticket by hand if you want the full quorum.",
            failed.join(", "),
            created.len(),
            req.reviewers.len(),
            req.parent_identifier,
        ));
    }
    if !marker_err.is_empty() {
        out.push_str(&format!(
            " WARNING: the `{QUORUM_REQUESTED_LABEL}` marker could not be written to {} \
             ({marker_err}); a later handoff from a restarted daemon could fan out a second time.",
            req.parent_identifier,
        ));
    }
    out
}

/// The host-templated review-ticket title (§0.12: "host-templated title `Review: <parent-title>`").
/// The parent's identifier leads so a reviewer scanning a backlog can see what is being reviewed
/// without opening anything.
pub(crate) fn review_title(parent_identifier: &str, parent_title: &str) -> String {
    format!("Review: {parent_identifier} {parent_title}")
}

/// The host-templated review-ticket description (§0.12). It names the PR, the parent, and the job.
///
/// **Written by the host, never by an agent.** The reviewer's instructions are the one place where
/// "never merge" has to hold, and a description an agent could author is a description an agent
/// could rewrite. §0.11.5 already treats agent-authored text as untrusted; this keeps the
/// instruction on the trusted side of that line.
pub(crate) fn review_description(req: &QuorumRequest, reviewer: &str) -> String {
    let QuorumRequest {
        pr_url,
        parent_identifier,
        parent_title,
        author,
        summon_token,
        ..
    } = req;
    format!(
        "You are **{reviewer}**, reviewing a teammate's work.\n\
         \n\
         **Pull request:** {pr_url}\n\
         **Reviewing:** {parent_identifier} — {parent_title}\n\
         **Author:** {author}\n\
         \n\
         Review that pull request independently. Another teammate is reviewing it at the same \
         time, from their own workspace, and you are not meant to agree with them — two \
         independent passes beat one agreed pass, which is the entire reason this ticket exists. \
         Do not go looking for their findings.\n\
         \n\
         What to do:\n\
         \n\
         1. Read the pull request: `gh pr view {pr_url}` and `gh pr diff {pr_url}`.\n\
         2. Judge it on correctness first, then on whether it does what {parent_identifier} \
            actually asked for, then on hygiene. Say what you would say to a colleague.\n\
         3. Post your findings as comments **on the pull request**, each one starting with \
            `{summon_token}` so {author} is re-engaged on them. The pull request is where a review \
            belongs — not this ticket, and not the team room.\n\
         4. **Approve or request changes explicitly.** \"Looks fine\" is not a review; say which \
            one it is and why.\n\
         5. **Never merge, and never push to the author's branch.** {author} owns the work; you own \
            the opinion.\n\
         \n\
         When you have posted your findings, you are done — hand this ticket off.\n"
    )
}

/// Appends one manager post to the room, best-effort and never fatal to the fan-out — triage's
/// [`post`](crate::triage) helper and its reason (§0.11.4: the room is advisory, Linear is the
/// ledger). The tickets have already been created or already refused; this can neither undo nor
/// block them, which is why it returns nothing to check.
fn post<TF>(deps: &QuorumDeps<TF>, msg: Message) {
    let Some(room) = deps.room.as_ref() else {
        return;
    };
    if let Err(e) = room.append(&msg) {
        tracing::warn!(
            err = %e,
            "teams quorum could not post to the room; the review tickets and the Linear history \
             are unaffected"
        );
    }
}

/// Chooses who reviews: the roster **minus the author**, least-loaded first, capped at
/// `quorum.reviewers` (§0.12). Pure — the whole selection is a comparison over data already in
/// hand, which is what lets it run on the control task.
///
/// Ties break on roster order, exactly as [`crate::teams::route`]'s label-overlap fallback does, so
/// the choice is deterministic and a test can pin it. `load` is §0.11.1's load: open tickets
/// carrying `rhapsody:@x`, counted from the daemon's own in-memory candidate snapshot.
///
/// Too few candidates degrades to however many exist — never an error, and never a wait.
pub(crate) fn select_reviewers(
    teams: &Teams,
    author: &str,
    load: &HashMap<String, i64>,
) -> Vec<String> {
    let mut ranked: Vec<(i64, usize, &str)> = teams
        .roster
        .iter()
        .enumerate()
        .filter(|(_, i)| i.name != author)
        .map(|(idx, i)| {
            (
                load.get(&i.name).copied().unwrap_or(0),
                idx,
                i.name.as_str(),
            )
        })
        .collect();
    ranked.sort_unstable();
    ranked
        .into_iter()
        .take(teams.quorum.effective_reviewers())
        .map(|(_, _, name)| name.to_string())
        .collect()
}

/// The first UNMERGED linked pull request as a browser URL, or empty when the issue has none.
///
/// Unmerged because a merged PR needs no review. The refs come from the same Linear
/// GitHub-integration attachments `linked_pr` is derived from, so this reads off a candidate the
/// poller already fetched rather than costing a call.
pub(crate) fn open_pr_url(iss: &Issue) -> String {
    iss.linked_prs
        .iter()
        .flatten()
        .find(|p| !p.merged && !p.owner.is_empty() && !p.repo.is_empty() && p.number > 0)
        .map(|p| {
            format!(
                "https://github.com/{}/{}/pull/{}",
                p.owner, p.repo, p.number
            )
        })
        .unwrap_or_default()
}

impl Orchestrator {
    /// Opens the review quorum's channel: stores the sender (which every
    /// [`ControlHandle`](crate::stop::ControlHandle) built afterwards clones) and hands the caller
    /// the receiver to give [`run_quorum_task`].
    ///
    /// A method rather than a public field so the sender cannot be reached from outside this crate:
    /// injecting a [`QuorumRequest`] is injecting Linear writes, and the only thing entitled to do
    /// that is a handoff the daemon itself executed. Call it BEFORE
    /// [`control`](Orchestrator::control), and only when the quorum is enabled — a daemon that
    /// never calls it has `quorum_tx: None`, which makes the fan-out unrepresentable rather than
    /// merely skipped.
    pub fn open_quorum_channel(&mut self) -> tokio::sync::mpsc::UnboundedReceiver<QuorumRequest> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.quorum_tx = Some(tx);
        rx
    }

    /// Whether the review quorum is on: Teams enabled AND `quorum.enabled`. Both, always — the
    /// quorum is opt-in ON TOP of an already opt-in feature (§0.12's cost control).
    pub(crate) fn quorum_enabled(&self) -> bool {
        self.teams
            .as_ref()
            .is_some_and(|t| t.enabled && t.quorum.enabled)
    }

    /// Snapshots what the poller just observed, so a handoff arriving between ticks can choose
    /// reviewers and check the marker without asking the tracker anything (§0.12: "from the
    /// daemon's in-memory candidate state — no extra tracker read").
    ///
    /// Called from the same place — and for the same reason — as
    /// [`record_issue_states`](Orchestrator::record_issue_states), and a hard no-op when the quorum
    /// is off, which is what makes "zero behaviour change with the quorum disabled" true of the
    /// control loop as well as of the task.
    ///
    /// **The load count is the candidate set's, and that is narrower than §0.11.1's definition.**
    /// §0.11.1 counts open tickets in any non-terminal state; the candidate fetch is active ∪ review
    /// (and, in the default claim mode, assignee-scoped), so a Backlog ticket wearing
    /// `rhapsody:@alice` does not count here. Deliberate: §0.12 chose the free in-memory count over
    /// a tracker read on purpose, and the direction of the error is benign — under-counting a
    /// teammate's backlog can only make the quorum spread reviews a little wider. Review tickets
    /// themselves ARE counted, because the fan-out creates them assigned and active, so a teammate
    /// holding two open reviews is correctly seen as busier than one holding none.
    ///
    /// Replaces rather than merges, [`record_issue_states`](Orchestrator::record_issue_states)'s
    /// rule and its reason: being absent is meaningful, so a map that only ever grew would keep
    /// asserting a PR link or a marker that was true a week ago.
    pub(crate) fn record_quorum_state<'a>(&mut self, issues: impl Iterator<Item = &'a Issue>) {
        if !self.quorum_enabled() {
            return;
        }
        let roster: HashSet<&str> = self
            .teams
            .as_ref()
            .map(|t| t.roster.iter().map(|i| i.name.as_str()).collect())
            .unwrap_or_default();
        let mut load: HashMap<String, i64> = HashMap::new();
        let mut facts: HashMap<String, QuorumFacts> = HashMap::new();
        for iss in issues {
            for label in iss.labels.iter().flatten() {
                let Some(name) = label.strip_prefix(IDENTITY_LABEL_PREFIX) else {
                    continue;
                };
                if roster.contains(name) {
                    *load.entry(name.to_string()).or_default() += 1;
                }
            }
            if iss.id.is_empty() {
                continue;
            }
            facts.insert(
                iss.id.clone(),
                QuorumFacts {
                    pr_url: open_pr_url(iss),
                    already_requested: iss
                        .labels
                        .iter()
                        .flatten()
                        .any(|l| l.eq_ignore_ascii_case(QUORUM_REQUESTED_LABEL)),
                },
            );
        }
        self.quorum_load = load;
        self.quorum_facts = facts;
    }

    /// Builds the fan-out plan for a handoff, or `None` when this handoff is not one the quorum
    /// fires on. Runs ON the control task, and every gate here is a comparison over data already in
    /// memory — no tracker call, no `.await`, nothing that could stall a tick.
    ///
    /// The gates, in the order §0.12 states them:
    ///
    /// 1. Teams and `quorum.enabled` are both on.
    /// 2. The run was dispatched AS a roster identity. A run with no identity is an ordinary
    ///    Rhapsody run and the quorum has no author to exclude and no team to ask.
    /// 3. The ticket has an open linked PR. A handoff with nothing to read fans out nothing.
    /// 4. The ticket is not already marked (§0.12's "once per ticket"): a re-handoff after review
    ///    fixes must NOT fan out a second time.
    pub(crate) fn plan_quorum(
        &self,
        re: &crate::orchestrator::RunningEntry,
    ) -> Option<QuorumRequest> {
        if !self.quorum_enabled() || re.identity.is_empty() {
            return None;
        }
        let teams = self.teams.as_ref()?;
        let facts = self.quorum_facts.get(&re.issue.id);
        // A ticket the poller has not seen since it acquired a PR simply has no URL yet, and a
        // review request with no PR in it is not worth making.
        let pr_url = facts.map(|f| f.pr_url.clone()).unwrap_or_default();
        if pr_url.is_empty() {
            tracing::debug!(
                issue = %re.issue.identifier,
                "teams quorum: the handed-off ticket has no open linked PR in the last candidate \
                 snapshot; nothing to review"
            );
            return None;
        }
        // The marker is checked from the candidate's already-fetched labels, and from the RUN's own
        // copy too: a fresh dispatch stamps the issue it was dispatched with, so a re-handoff after
        // review fixes carries the marker even if a tick has not landed yet.
        let marked = facts.is_some_and(|f| f.already_requested)
            || re
                .issue
                .labels
                .iter()
                .flatten()
                .any(|l| l.eq_ignore_ascii_case(QUORUM_REQUESTED_LABEL));
        if marked {
            tracing::debug!(
                issue = %re.issue.identifier,
                "teams quorum already requested for this ticket; the re-handoff fans out nothing"
            );
            return None;
        }
        Some(QuorumRequest {
            parent_issue_id: re.issue.id.clone(),
            parent_team_id: re.issue.team_id.clone(),
            parent_identifier: re.issue.identifier.clone(),
            parent_title: re.issue.title.clone(),
            pr_url,
            author: re.identity.clone(),
            reviewers: select_reviewers(teams, &re.identity, &self.quorum_load),
            state_name: self.quorum_create_state(&re.project_slug),
            summon_token: self.quorum_summon_token(),
        })
    }

    /// The workflow state a review ticket is created in: the run's owning project's FIRST
    /// configured active state, resolved per-project ⊕ top-level exactly as
    /// `review_handoff_state` resolves `review_states[0]`.
    ///
    /// §0.12 says "Todo", and this is what "Todo" means in a workspace-agnostic port. Hard-coding
    /// the literal would be worse than wrong: state names vary per workspace, and a review ticket
    /// created in a state this daemon's `active_states` does not list is not a candidate — the
    /// fan-out would appear to work and no reviewer would ever wake. The first active state is by
    /// construction a state the daemon dispatches from, which is the property that actually matters.
    fn quorum_create_state(&self, project_slug: &str) -> String {
        let Some(eff) = self.eff.as_ref() else {
            return String::new();
        };
        let project = if project_slug.is_empty() {
            None
        } else {
            eff.cfg
                .projects
                .iter()
                .find(|p| p.slugs.iter().any(|s| s == project_slug))
        };
        rhapsody_config::effective_for(&eff.cfg, project)
            .active_states
            .into_iter()
            .next()
            .unwrap_or_default()
    }

    /// The configured summon token the reviewer's instructions tell them to lead their PR comments
    /// with. Empty config ⇒ the shipped default, because an instruction that names no token would
    /// silently produce reviews that never re-engage the author.
    fn quorum_summon_token(&self) -> String {
        let token = self
            .eff
            .as_ref()
            .map(|e| e.cfg.tracker.summon_token.trim())
            .unwrap_or_default();
        if token.is_empty() {
            rhapsody_core::SUMMON_TOKEN_SYMPHONY.to_string()
        } else {
            token.to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testsupport::{TempDir, issue};
    use rhapsody_config::room::{Cursor, LocalRoom, RoomError};
    use rhapsody_config::teams::{Identity, Quorum};
    use rhapsody_core::LinkedPRRef;
    use rhapsody_tracker::TrackerError;
    use rhapsody_tracker::fake::Fake;

    fn ident(name: &str) -> Identity {
        Identity {
            name: name.to_string(),
            profile: "swe".to_string(),
            labels: Vec::new(),
            bank: String::new(),
            max_concurrent: 0,
        }
    }

    /// Teams ON with the quorum ON — the only configuration the fan-out runs under.
    fn teams_quorum(names: &[&str], reviewers: i64) -> Teams {
        Teams {
            enabled: true,
            quorum: Quorum {
                enabled: true,
                reviewers,
            },
            roster: names.iter().map(|n| ident(n)).collect(),
            ..Teams::disabled()
        }
    }

    fn request(reviewers: &[&str]) -> QuorumRequest {
        QuorumRequest {
            parent_issue_id: "iss-1".into(),
            parent_team_id: "team-1".into(),
            parent_identifier: "MT-1".into(),
            parent_title: "do the thing".into(),
            pr_url: "https://github.com/o/r/pull/7".into(),
            author: "alice".into(),
            reviewers: reviewers.iter().map(|r| (*r).to_string()).collect(),
            state_name: "Todo".into(),
            summon_token: "@symphony".into(),
        }
    }

    fn deps(teams: Teams, tr: Arc<Fake>) -> QuorumDeps<impl Fn() -> Option<QuorumTarget>> {
        QuorumDeps {
            teams: Arc::new(teams),
            target: move || {
                Some(QuorumTarget {
                    tracker: Arc::clone(&tr) as Arc<dyn Tracker>,
                })
            },
            room: None,
            max_backoff_ms: 20,
        }
    }

    fn deps_with_room(
        teams: Teams,
        tr: Arc<Fake>,
        room: Arc<dyn RoomLog>,
    ) -> QuorumDeps<impl Fn() -> Option<QuorumTarget>> {
        QuorumDeps {
            room: Some(room),
            ..deps(teams, tr)
        }
    }

    /// A tracker with a viewer to assign to — the fan-out refuses to create anything without one.
    fn tracker_with_viewer() -> Fake {
        let mut tr = Fake::new();
        tr.viewer = rhapsody_core::Viewer {
            id: "viewer-1".into(),
            ..Default::default()
        };
        tr
    }

    /// Every message in the room, oldest first.
    fn room_posts(room: &LocalRoom) -> Vec<rhapsody_config::room::Message> {
        room.read_since("reader", &Cursor::default(), 0)
            .expect("catch up")
            .messages
    }

    // ── reviewer selection (pure) ───────────────────────────────────────────────────────────────

    // The acceptance criterion, pinned with a KNOWN load state: the author is excluded, the roster
    // is ordered least-loaded first, and exactly `reviewers` are taken. Roster order here is
    // deliberately the OPPOSITE of load order, so a test that merely returned the roster would fail.
    #[test]
    fn reviewers_are_least_loaded_first_and_never_the_author() {
        let teams = teams_quorum(&["alice", "bob", "carol", "dave"], 2);
        let load = HashMap::from([
            ("alice".to_string(), 0), // the author: excluded regardless of load
            ("bob".to_string(), 5),
            ("carol".to_string(), 1),
            ("dave".to_string(), 3),
        ]);
        assert_eq!(
            select_reviewers(&teams, "alice", &load),
            vec!["carol".to_string(), "dave".to_string()],
            "least-loaded first, author excluded, capped at reviewers"
        );
    }

    // An identity with no open tickets counts as zero rather than being skipped — the idlest
    // teammate is the best reviewer, and `HashMap::get` returning `None` must not read as "unknown".
    #[test]
    fn an_absent_load_entry_counts_as_idle() {
        let teams = teams_quorum(&["alice", "bob", "carol"], 1);
        let load = HashMap::from([("bob".to_string(), 4)]);
        assert_eq!(select_reviewers(&teams, "alice", &load), vec!["carol"]);
    }

    // Ties break on ROSTER ORDER, exactly as `teams::route`'s label-overlap fallback does, so the
    // choice is deterministic across runs and a fresh team does not fan out at random.
    #[test]
    fn a_tie_breaks_on_roster_order() {
        let teams = teams_quorum(&["alice", "bob", "carol", "dave"], 2);
        let load = HashMap::new();
        assert_eq!(
            select_reviewers(&teams, "alice", &load),
            vec!["bob".to_string(), "carol".to_string()]
        );
    }

    // Too few candidates degrades to however many exist — never an error, never a wait (§0.12).
    #[test]
    fn a_short_roster_degrades_to_as_many_as_exist() {
        let teams = teams_quorum(&["alice", "bob"], 2);
        assert_eq!(
            select_reviewers(&teams, "alice", &HashMap::new()),
            vec!["bob"]
        );

        let solo = teams_quorum(&["alice"], 2);
        assert!(
            select_reviewers(&solo, "alice", &HashMap::new()).is_empty(),
            "a roster of one has nobody to ask"
        );
    }

    // ── the PR read ─────────────────────────────────────────────────────────────────────────────

    // A merged PR needs no review, so the URL is the first UNMERGED ref; an issue with none has no
    // URL at all, which is what makes "a handoff with no PR fires nothing" decidable in memory.
    #[test]
    fn open_pr_url_picks_the_first_unmerged_ref() {
        let pr = |number: i64, merged: bool| LinkedPRRef {
            owner: "o".into(),
            repo: "r".into(),
            number,
            merged,
        };
        let mut iss = issue("i1", "MT-1", "In Progress");
        assert_eq!(open_pr_url(&iss), "", "no refs ⇒ no url");

        iss.linked_prs = Some(vec![pr(1, true)]);
        assert_eq!(open_pr_url(&iss), "", "a merged PR needs no review");

        iss.linked_prs = Some(vec![pr(1, true), pr(7, false)]);
        assert_eq!(open_pr_url(&iss), "https://github.com/o/r/pull/7");
    }

    // ── the fan-out ─────────────────────────────────────────────────────────────────────────────

    // The headline acceptance: exactly `reviewers` review tickets, each Todo, viewer-assigned and
    // labelled `rhapsody:@<reviewer>`; the parent marked; ONE manager post naming the PR and the
    // reviewers.
    #[tokio::test]
    async fn a_fan_out_creates_a_ticket_per_reviewer_marks_the_parent_and_tells_the_room() {
        let dir = TempDir::new();
        let room = Arc::new(LocalRoom::new(dir.child("room")));
        let tr = Arc::new(tracker_with_viewer());
        let d = deps_with_room(
            teams_quorum(&["alice", "bob", "carol"], 2),
            Arc::clone(&tr),
            Arc::clone(&room) as Arc<dyn RoomLog>,
        );

        assert_eq!(
            fan_out(&d, &request(&["bob", "carol"])).await,
            FanOutcome::Fanned {
                created: 2,
                wanted: 2
            }
        );

        let calls = tr.create_issue_calls();
        assert_eq!(calls.len(), 2, "one review ticket per reviewer: {calls:?}");
        for (call, reviewer) in calls.iter().zip(["bob", "carol"]) {
            let s = &call.spec;
            assert_eq!(s.team_id, "team-1", "created beside the work it reviews");
            assert_eq!(s.state_name, "Todo");
            assert_eq!(
                s.assignee_id, "viewer-1",
                "unassigned tickets are never picked up"
            );
            assert_eq!(s.labels, vec![format!("rhapsody:@{reviewer}")]);
            assert_eq!(s.title, "Review: MT-1 do the thing");
            assert!(
                s.description.contains("https://github.com/o/r/pull/7"),
                "the description names the PR: {}",
                s.description
            );
            assert!(
                s.description.contains("MT-1"),
                "and the parent: {}",
                s.description
            );
            assert!(
                s.description.contains("@symphony"),
                "and the summon token findings must carry: {}",
                s.description
            );
            assert!(
                s.description.contains("Never merge"),
                "and the one instruction that must always hold: {}",
                s.description
            );
        }

        let labels = tr.add_label_calls();
        assert_eq!(labels.len(), 1, "the parent is marked once: {labels:?}");
        assert_eq!(labels[0].issue_id, "iss-1");
        assert_eq!(labels[0].label_name, QUORUM_REQUESTED_LABEL);

        let posts = room_posts(&room);
        assert_eq!(posts.len(), 1, "exactly one manager post: {posts:?}");
        let m = &posts[0];
        assert_eq!(m.from, MANAGER_IDENTITY, "`from` is host-stamped");
        assert_eq!(m.to, rhapsody_config::room::Audience::Room);
        assert!(
            m.body
                .contains("Requested review of https://github.com/o/r/pull/7"),
            "{}",
            m.body
        );
        assert!(m.body.contains("bob, carol"), "{}", m.body);
        assert!(
            m.refs.contains(&"MT-1".to_string()),
            "the post refs the parent: {:?}",
            m.refs
        );
    }

    // §0.12: "zero ⇒ skip with a loud room post". A one-person team is a valid configuration; it
    // just cannot hold a quorum, and nothing is written to the tracker at all.
    #[tokio::test]
    async fn a_roster_of_one_writes_nothing_and_posts_loudly() {
        let dir = TempDir::new();
        let room = Arc::new(LocalRoom::new(dir.child("room")));
        let tr = Arc::new(tracker_with_viewer());
        let d = deps_with_room(
            teams_quorum(&["alice"], 2),
            Arc::clone(&tr),
            Arc::clone(&room) as Arc<dyn RoomLog>,
        );

        assert_eq!(fan_out(&d, &request(&[])).await, FanOutcome::NoReviewers);
        assert!(tr.create_issue_calls().is_empty(), "nothing is created");
        assert!(tr.add_label_calls().is_empty(), "and nothing is marked");

        let posts = room_posts(&room);
        assert_eq!(posts.len(), 1);
        assert!(
            posts[0].body.contains("NO REVIEW QUORUM"),
            "{}",
            posts[0].body
        );
        assert!(posts[0].body.contains("MT-1"), "{}", posts[0].body);
    }

    // Scope 7's stated tradeoff: 1 of 2 created still marks the parent and REPORTS the shortfall,
    // rather than leaving the parent unmarked so a later handoff re-creates the ticket that already
    // succeeded. A duplicate review ticket wakes a real agent for no reason; a stated gap does not.
    #[tokio::test]
    async fn a_partial_fan_out_marks_the_parent_and_names_the_shortfall() {
        let dir = TempDir::new();
        let room = Arc::new(LocalRoom::new(dir.child("room")));
        let mut fake = tracker_with_viewer();
        fake.create_issue_fail_first = 1; // bob's create fails, carol's succeeds
        let tr = Arc::new(fake);
        let d = deps_with_room(
            teams_quorum(&["alice", "bob", "carol"], 2),
            Arc::clone(&tr),
            Arc::clone(&room) as Arc<dyn RoomLog>,
        );

        assert_eq!(
            fan_out(&d, &request(&["bob", "carol"])).await,
            FanOutcome::Fanned {
                created: 1,
                wanted: 2
            }
        );
        assert_eq!(
            tr.create_issue_calls().len(),
            2,
            "one create failing does not abort the other"
        );
        assert_eq!(
            tr.add_label_calls().len(),
            1,
            "the parent is marked anyway, so this is never retried"
        );

        let body = &room_posts(&room)[0].body;
        assert!(body.contains("SHORTFALL"), "{body}");
        assert!(body.contains("bob"), "the failed reviewer is named: {body}");
        assert!(body.contains("1 of 2"), "{body}");
    }

    // Every create failing is a TrackerFailure: the parent is NOT marked (so a later handoff may
    // still try), the room is told loudly, and the caller backs off.
    #[tokio::test]
    async fn a_total_failure_leaves_the_parent_unmarked_and_posts_loudly() {
        let dir = TempDir::new();
        let room = Arc::new(LocalRoom::new(dir.child("room")));
        let mut fake = tracker_with_viewer();
        fake.create_issue_err = Some(TrackerError::Other("linear_api_status: 503".into()));
        let tr = Arc::new(fake);
        let d = deps_with_room(
            teams_quorum(&["alice", "bob", "carol"], 2),
            Arc::clone(&tr),
            Arc::clone(&room) as Arc<dyn RoomLog>,
        );

        assert_eq!(
            fan_out(&d, &request(&["bob", "carol"])).await,
            FanOutcome::TrackerFailure
        );
        assert!(
            tr.add_label_calls().is_empty(),
            "nothing was requested, so nothing is marked"
        );
        let body = &room_posts(&room)[0].body;
        assert!(body.contains("REVIEW QUORUM FAILED"), "{body}");
        assert!(body.contains("NOT marked"), "{body}");
    }

    // Without a viewer to assign to, creating the tickets would be worse than creating none: an
    // unassigned ticket is never picked up, so the fan-out would look like it worked.
    #[tokio::test]
    async fn no_resolvable_viewer_creates_nothing() {
        let dir = TempDir::new();
        let room = Arc::new(LocalRoom::new(dir.child("room")));
        let mut fake = Fake::new();
        fake.viewer_err = Some(TrackerError::Other("linear_api_request: boom".into()));
        let tr = Arc::new(fake);
        let d = deps_with_room(
            teams_quorum(&["alice", "bob", "carol"], 2),
            Arc::clone(&tr),
            Arc::clone(&room) as Arc<dyn RoomLog>,
        );

        assert_eq!(
            fan_out(&d, &request(&["bob", "carol"])).await,
            FanOutcome::TrackerFailure
        );
        assert!(tr.create_issue_calls().is_empty());
        assert!(room_posts(&room)[0].body.contains("REVIEW QUORUM FAILED"));
    }

    // The room is advisory and Linear is the ledger (§0.11.4): a room that cannot be written costs
    // the team a paragraph of history and costs the fan-out nothing.
    #[tokio::test]
    async fn a_failing_room_does_not_cost_the_fan_out() {
        struct BrokenRoom;
        impl RoomLog for BrokenRoom {
            fn append(&self, _msg: &Message) -> Result<String, RoomError> {
                Err(RoomError::Io("disk on fire".into()))
            }
            fn read_since(
                &self,
                _reader: &str,
                _cursor: &Cursor,
                _limit: usize,
            ) -> Result<rhapsody_config::room::CaughtUp, RoomError> {
                Err(RoomError::Io("disk on fire".into()))
            }
        }
        let tr = Arc::new(tracker_with_viewer());
        let d = deps_with_room(
            teams_quorum(&["alice", "bob", "carol"], 2),
            Arc::clone(&tr),
            Arc::new(BrokenRoom) as Arc<dyn RoomLog>,
        );

        assert_eq!(
            fan_out(&d, &request(&["bob", "carol"])).await,
            FanOutcome::Fanned {
                created: 2,
                wanted: 2
            }
        );
        assert_eq!(tr.create_issue_calls().len(), 2);
        assert_eq!(tr.add_label_calls().len(), 1);
    }

    // A marker write that fails does not undo a fan-out that succeeded — but it IS said out loud,
    // because it means a restarted daemon could fan out a second time.
    #[tokio::test]
    async fn a_failed_marker_still_reports_the_created_tickets() {
        let dir = TempDir::new();
        let room = Arc::new(LocalRoom::new(dir.child("room")));
        let mut fake = tracker_with_viewer();
        fake.add_label_err = Some(TrackerError::Other("linear_move_rejected: nope".into()));
        let tr = Arc::new(fake);
        let d = deps_with_room(
            teams_quorum(&["alice", "bob", "carol"], 2),
            Arc::clone(&tr),
            Arc::clone(&room) as Arc<dyn RoomLog>,
        );

        assert_eq!(
            fan_out(&d, &request(&["bob", "carol"])).await,
            FanOutcome::Fanned {
                created: 2,
                wanted: 2
            }
        );
        let body = &room_posts(&room)[0].body;
        assert!(body.contains("WARNING"), "{body}");
        assert!(body.contains(QUORUM_REQUESTED_LABEL), "{body}");
    }

    // No tracker yet (the daemon has not loaded a config) is a failure the caller backs off on, not
    // a silent success.
    #[tokio::test]
    async fn no_tracker_is_a_backed_off_failure() {
        let d: QuorumDeps<fn() -> Option<QuorumTarget>> = QuorumDeps {
            teams: Arc::new(teams_quorum(&["alice", "bob"], 2)),
            target: || None,
            room: None,
            max_backoff_ms: 20,
        };
        assert_eq!(
            fan_out(&d, &request(&["bob"])).await,
            FanOutcome::TrackerFailure
        );
    }

    // ── the per-tick candidate snapshot ─────────────────────────────────────────────────────────

    fn orch_with(teams: Teams) -> Orchestrator {
        let mut o = Orchestrator::new("WORKFLOW.md");
        o.teams = Some(teams);
        o
    }

    fn with_labels(id: &str, labels: &[&str]) -> Issue {
        let mut iss = issue(id, id, "Todo");
        iss.labels = Some(labels.iter().map(|s| (*s).to_string()).collect());
        iss
    }

    // The sweep tallies §0.11.1's load per identity and reads each ticket's PR + marker, so a
    // handoff arriving between ticks costs no tracker read at all.
    #[test]
    fn the_candidate_sweep_records_load_and_facts() {
        let mut o = orch_with(teams_quorum(&["alice", "bob"], 2));
        let mut parent = with_labels("iss-1", &["rhapsody:@alice"]);
        parent.linked_prs = Some(vec![LinkedPRRef {
            owner: "o".into(),
            repo: "r".into(),
            number: 7,
            merged: false,
        }]);
        let marked = with_labels("iss-2", &["rhapsody:@bob", QUORUM_REQUESTED_LABEL]);
        let other = with_labels("iss-3", &["rhapsody:@bob"]);
        // A label naming nobody on the roster is not load: §0.11.1 makes a present label
        // authoritative for ROUTING, but a departed teammate is not somebody to hand a review to.
        let stray = with_labels("iss-4", &["rhapsody:@mallory"]);

        o.record_quorum_state([parent, marked, other, stray].iter());

        assert_eq!(o.quorum_load.get("alice"), Some(&1));
        assert_eq!(o.quorum_load.get("bob"), Some(&2));
        assert_eq!(o.quorum_load.get("mallory"), None, "off-roster is not load");
        assert_eq!(
            o.quorum_facts["iss-1"].pr_url,
            "https://github.com/o/r/pull/7"
        );
        assert!(!o.quorum_facts["iss-1"].already_requested);
        assert!(o.quorum_facts["iss-2"].already_requested);
        assert!(!o.quorum_facts["iss-3"].already_requested);
    }

    // Replaces rather than merges: a ticket that has left the candidate set stops asserting a PR
    // link or a marker that was true a week ago.
    #[test]
    fn the_candidate_sweep_replaces_rather_than_merges() {
        let mut o = orch_with(teams_quorum(&["alice", "bob"], 2));
        o.record_quorum_state([with_labels("iss-1", &["rhapsody:@alice"])].iter());
        assert!(o.quorum_facts.contains_key("iss-1"));

        o.record_quorum_state([with_labels("iss-2", &["rhapsody:@bob"])].iter());
        assert!(
            !o.quorum_facts.contains_key("iss-1"),
            "a departed ticket must not linger"
        );
        assert_eq!(o.quorum_load.get("alice"), None);
        assert_eq!(o.quorum_load.get("bob"), Some(&1));
    }

    // The acceptance criterion, at the sweep: with the quorum off — or with Teams off entirely —
    // the per-tick pass is a hard no-op, not a pass whose result is merely unread.
    #[test]
    fn the_candidate_sweep_is_a_hard_no_op_when_the_quorum_is_off() {
        let mut teams_on_quorum_off = teams_quorum(&["alice", "bob"], 2);
        teams_on_quorum_off.quorum.enabled = false;
        let mut teams_off = teams_quorum(&["alice", "bob"], 2);
        teams_off.enabled = false;

        for teams in [Some(teams_on_quorum_off), Some(teams_off), None] {
            let mut o = Orchestrator::new("WORKFLOW.md");
            o.teams = teams.clone();
            assert!(!o.quorum_enabled(), "{teams:?}");
            o.record_quorum_state([with_labels("iss-1", &["rhapsody:@alice"])].iter());
            assert!(o.quorum_load.is_empty(), "{teams:?}");
            assert!(o.quorum_facts.is_empty(), "{teams:?}");
        }
    }

    // ── the task ────────────────────────────────────────────────────────────────────────────────

    // §0.12's "once per ticket", in-process half: a second handoff of the SAME parent fans out
    // nothing, even before a poll could refresh the marker label onto the candidate.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_second_handoff_of_the_same_parent_fans_out_nothing() {
        let tr = Arc::new(tracker_with_viewer());
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let signal = crate::control_loop::CancelSignal::new();
        let d = deps(teams_quorum(&["alice", "bob", "carol"], 2), Arc::clone(&tr));
        let task = tokio::spawn(run_quorum_task(signal.wait(), d, rx));

        tx.send(request(&["bob", "carol"])).expect("first handoff");
        tx.send(request(&["bob", "carol"])).expect("second handoff");
        // Drop the sender so the task drains and returns rather than being killed mid-write.
        drop(tx);
        task.await.expect("task joins");

        assert_eq!(
            tr.create_issue_calls().len(),
            2,
            "the repeat handoff must create nothing: {:?}",
            tr.create_issue_calls()
        );
        assert_eq!(tr.add_label_calls().len(), 1);
        signal.cancel();
    }

    // Cancellation returns promptly rather than waiting out a queued request.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_task_stops_on_cancel() {
        let tr = Arc::new(tracker_with_viewer());
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel::<QuorumRequest>();
        let signal = crate::control_loop::CancelSignal::new();
        let d = deps(teams_quorum(&["alice", "bob"], 2), tr);
        let task = tokio::spawn(run_quorum_task(signal.wait(), d, rx));
        signal.cancel();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("the task stops promptly on cancel")
            .expect("task joins");
    }

    // A dropped sender ends the task: no handoff can arrive once the control handle is gone.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_task_ends_when_every_sender_is_dropped() {
        let tr = Arc::new(tracker_with_viewer());
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<QuorumRequest>();
        let signal = crate::control_loop::CancelSignal::new();
        let d = deps(teams_quorum(&["alice", "bob"], 2), tr);
        let task = tokio::spawn(run_quorum_task(signal.wait(), d, rx));
        drop(tx);
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("the task ends")
            .expect("task joins");
        signal.cancel();
    }

    // A tracker that refuses everything must not turn into a hot retry loop: a failed fan-out
    // delays the NEXT one, and the failed one is never retried at all — a re-handoff is the only
    // thing entitled to ask again.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_fan_out_backs_off_and_is_never_retried() {
        let mut fake = tracker_with_viewer();
        fake.create_issue_err = Some(TrackerError::Other("linear_api_status: 503".into()));
        let tr = Arc::new(fake);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let signal = crate::control_loop::CancelSignal::new();
        let d = deps(teams_quorum(&["alice", "bob", "carol"], 2), Arc::clone(&tr));
        let task = tokio::spawn(run_quorum_task(signal.wait(), d, rx));

        for n in 1..=3 {
            let mut req = request(&["bob"]);
            req.parent_issue_id = format!("iss-{n}");
            tx.send(req).expect("send");
        }
        drop(tx);
        task.await.expect("task joins");

        assert_eq!(
            tr.create_issue_calls().len(),
            3,
            "one attempt per REQUEST, never a retry of a failed one"
        );
        signal.cancel();
    }
}
