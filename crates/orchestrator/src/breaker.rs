//! breaker — the runaway-loop circuit breaker (STUDIO-1026).
//!
//! **No Go counterpart.** Ticketless review is a Rhapsody addition end to end, and so is this.
//!
//! # The incident this exists for
//!
//! On 2026-09-22/23 two pull requests ran review loops unattended until they had cost far more than
//! the work was worth: STUDIO-988 took 14 author attempts and 26 reviews for **259M tokens**, and
//! STUDIO-984 took 6 author attempts and about 20 reviews. Every signal was a console banner and an
//! operator happening to look. The maintainer's ask: be **notified and able to intervene after
//! about five review rounds**, and cap SPEND per ticket, not only per run (`max_run_tokens`) or per
//! provider per day (`budgets.<provider>.daily_tokens`).
//!
//! # What it counts, and why that matters
//!
//! ⚠️ Round crossings count COMPLETED REVIEW RUNS whose reviewer verdict ASKED FOR CHANGES
//! (STUDIO-1440), read from the `runs` ledger joined to the per-run `rhapsody_review_verdicts`
//! (`pr:<owner>/<repo>#<n>@*`), never the review watcher's own dispatch counter
//! (`rhapsody_review_bound.dispatches`). The limit exists to stop a loop that is NOT converging, so
//! it must not count rounds that are evidence of convergence: a round that APPROVED, a re-review
//! armed only by merging main, and the manager's own intervention run (`@manager`) are all
//! non-events. A `@manager` run is not a review round at all. An operator `clear` resets both
//! counters (STUDIO-1134), but a dispatched round that never ran spent nothing: only completed runs
//! since clear count. [`crate::reviewwatch`]'s in-memory round count is the same story — it does not
//! survive a restart.
//!
//! A round crossing also does NOT apply the hold while the most recent completed verdict at the
//! pull request's CURRENT HEAD is an approval: an approved-and-open pull request has converged, and
//! holding it would be the exact false action the limit exists to avoid. The HEAD SCOPE comes from
//! the LIVE WATCH ROWS (`last_reviewed_sha` == head, or a head proven patch-identical) — never from
//! the per-run verdict ledger, which carries no head, so a round that completed at a SUPERSEDED head
//! (an in-flight review that finished after the head moved on) would otherwise shadow the approval. A
//! sibling still owing a round is not a completed verdict and does not defeat the approval. Within
//! that head scope the ORDER comes from the ledger: two reviewers can complete different verdicts at
//! the SAME head, in either order, so the NEWEST completed verdict at the head is the one that
//! decides — an approval suppresses only when it is that newest verdict.
//!
//! Spend crossings sum the ticket's author runs PLUS the review runs on its pull request, split by
//! provider through `rhapsody_run_provenance` — exactly the join STUDIO-957 introduced, because the
//! incident's whole Claude bill was reviews.
//!
//! # What it DOES
//!
//! When a limit is crossed, once:
//!
//! 1. the existing `rhapsody:human` hold is applied to the ticket, so no NEW run or review round is
//!    dispatched. A run already in flight finishes — the ticket is held, the run is not killed,
//!    for [`crate::budget`]'s reason: killing a running agent wastes everything it has already spent;
//! 2. one line is posted to the team room;
//! 3. a notification goes out through every configured `notify:` channel (`macos`, `webhook`,
//!    `ntfy`).
//!
//! The crossing is PERSISTED ([`rhapsody_store::BreakerCrossingRow`]) before either write, so it
//! survives a restart and never repeats while the ticket stays held. The operator un-holds by
//! removing the label; the NEXT crossing (round ten after five) can notify again.
//!
//! # Manager escalations notify too
//!
//! The reconciliation sweep's `review_escalated` divergence ([`crate::reviewreconcile`]) rides the
//! same channels, so the operator does not have to watch the console banner. It is deduped in
//! memory per (PR, head): the sweep recomputes the divergence every tick from the durable
//! adjudication, and a restart re-notifies at worst once.
//!
//! # Off the loop
//!
//! The control task makes the DECISION (it owns the store and the watch set); the label write, the
//! room append and the HTTP notifications happen on this module's own task, for
//! [`crate::draftpoke`]'s reason: a slow tracker round-trip or a hung webhook must park the task
//! that owns this subsystem's I/O and nothing else.

use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use rhapsody_config::room::{Message, RoomLog};
use rhapsody_tracker::Tracker;
use serde::Serialize;

use crate::orchestrator::Orchestrator;

/// The identity the breaker's room line is stamped with. The daemon is speaking, not a teammate —
/// the manager identity would misattribute a system event to a person.
pub const BREAKER_IDENTITY: &str = "@rhapsody";

/// How many notifications are kept for the desktop to pick up on `/api/v1/state`. Bounded so a
/// long-lived daemon cannot grow the payload without limit; the desktop de-dupes by `id`.
pub const MAX_NOTIFICATIONS: usize = 32;

/// Which limit a [`BreakerPlan`] crossed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrossingKind {
    /// The pull request's changes-requested review-round count crossed `review.hold_after_rounds`.
    Rounds,
    /// A provider's per-ticket token spend crossed `budgets.<provider>.per_ticket`.
    Spend,
    /// The manager adjudicated the loop and escalated it ([`crate::reviewreconcile::DivergenceKind::ReviewEscalated`]).
    Escalation,
}

impl CrossingKind {
    /// The stable token used in the log line and the notification body.
    pub fn as_str(self) -> &'static str {
        match self {
            CrossingKind::Rounds => "review rounds",
            CrossingKind::Spend => "per-ticket spend",
            CrossingKind::Escalation => "manager escalation",
        }
    }
}

/// One provider's spend against its per-ticket cap, for the notification body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderSpend {
    pub provider: String,
    pub spent: i64,
    /// The configured cap, or `None` for a provider with no per-ticket cap (reported so the
    /// operator sees the whole bill, not only the constrained account).
    pub cap: Option<i64>,
}

/// One crossing, planned on the control task and performed off it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BreakerPlan {
    /// Which limit(s) crossed on this tick. Usually one; a round and a spend crossing can coincide.
    pub kinds: Vec<CrossingKind>,
    /// The origin ticket identifier, e.g. `STUDIO-988`. Non-empty — a `console:` pull request with
    /// no ticket has nothing to hold and is never planned here.
    pub ticket: String,
    /// The opaque tracker issue id the hold addresses, resolved on the control task.
    pub issue_id: String,
    /// The tracker team id the label write needs.
    pub team_id: String,
    pub owner: String,
    pub repo: String,
    pub number: i64,
    /// Completed review runs on the pull request, as counted from the `runs` ledger.
    pub rounds: i64,
    /// The round threshold crossed, or `0` on a spend-only crossing.
    pub threshold: i64,
    /// The ticket's author runs.
    pub attempts: i64,
    /// Spend per provider (author + review runs), in the store's own order.
    pub spend: Vec<ProviderSpend>,
    /// The latest review row's status (`reviewed`/`approved`/`truncated`), or empty when the pull
    /// request has no live watch row. The durable store keeps no review RESULT TEXT, so this states
    /// the latest review's OUTCOME rather than its first line (see the PR body).
    pub latest_review: String,
    /// The manager's own words, for a [`CrossingKind::Escalation`]; empty otherwise.
    pub reason: String,
}

/// Whether a crossing applies the `rhapsody:human` hold. A ROUND or SPEND crossing holds; a
/// manager ESCALATION only notifies (the loop is already stopped by the adjudication). Both
/// [`perform_crossing`] and [`crossing_body`] read this one predicate so the headline the operator
/// reads can never disagree with whether the label was actually written.
fn crossing_applies_hold(kinds: &[CrossingKind]) -> bool {
    kinds
        .iter()
        .any(|k| matches!(k, CrossingKind::Rounds | CrossingKind::Spend))
}

impl BreakerPlan {
    /// The pull request URL, for the notification body.
    pub fn pr_url(&self) -> String {
        format!(
            "https://github.com/{}/{}/pull/{}",
            self.owner, self.repo, self.number
        )
    }

    /// `owner/repo#n`, the coordinate the logs and the room use.
    pub fn pr(&self) -> String {
        format!("{}/{}#{}", self.owner, self.repo, self.number)
    }
}

impl std::fmt::Display for BreakerPlan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.pr())
    }
}

/// Human-readable token count for the notification body (`30000000` → `30.0M`).
fn human_tokens(n: i64) -> String {
    let n = n.max(0);
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// The host-composed notification, shared by the room line and every notify channel.
///
/// Deliberately carries NO summon token: this is a message to the OPERATOR, not to the author's
/// agent, and re-opening the author's run is the one thing the breaker exists to stop.
pub fn crossing_body(plan: &BreakerPlan) -> String {
    let crossed = plan
        .kinds
        .iter()
        .map(|k| k.as_str())
        .collect::<Vec<_>>()
        .join(" and ");
    // Only a ROUND or SPEND crossing applies the hold (see `perform_crossing`); a manager
    // ESCALATION notifies without holding. The headline must match, or every channel and the room
    // tell the operator a `rhapsody:human` label exists that was never applied and never will be.
    let held = crossing_applies_hold(&plan.kinds);
    let mut body = if held {
        format!(
            "**{ticket}** crossed its {crossed} limit on `{pr}` — the ticket is now held \
             (`rhapsody:human`), so no new run or review round will start. Remove the label to \
             resume.\n",
            ticket = plan.ticket,
            pr = plan.pr(),
        )
    } else {
        format!(
            "**{ticket}** — the manager escalated the review loop on `{pr}`; a person needs to \
             decide how to proceed.\n",
            ticket = plan.ticket,
            pr = plan.pr(),
        )
    };
    if plan.kinds.contains(&CrossingKind::Rounds) {
        body.push_str(&format!(
            "Rounds: **{}** review round(s) that asked for changes (threshold {}). ",
            plan.rounds, plan.threshold
        ));
    }
    // Omit the line entirely when the count is unknown (an unreadable store reports 0, and a
    // false "0 attempts" reads as a fact the daemon does not have).
    if plan.attempts > 0 {
        body.push_str(&format!("Author attempts: **{}**. ", plan.attempts));
    }
    let spend = plan
        .spend
        .iter()
        .filter(|s| s.spent > 0)
        .map(|s| match s.cap {
            // Report the spend that actually HAPPENED, with the cap beside it — reporting
            // `min(spent, cap)` would name the cap as the spend and hide the overshoot.
            Some(cap) => format!(
                "{} {} (cap {})",
                s.provider,
                human_tokens(s.spent),
                human_tokens(cap)
            ),
            None => format!("{} {}", s.provider, human_tokens(s.spent)),
        })
        .collect::<Vec<_>>();
    if !spend.is_empty() {
        body.push_str(&format!("Spend: {}. ", spend.join(", ")));
    }
    if !plan.latest_review.is_empty() {
        body.push_str(&format!("Latest review: {}. ", plan.latest_review));
    }
    if plan.kinds.contains(&CrossingKind::Escalation) && !plan.reason.is_empty() {
        body.push_str(&format!("Manager: {}", plan.reason));
    }
    // STUDIO-1504: an escalation is one of the four events that warrants a human, so it says what
    // to do — the decision wanted and where to answer — before the pull-request link.
    if plan.kinds.contains(&CrossingKind::Escalation) {
        body.push_str(&format!("\n{}", crate::notify::answer_hint(&plan.ticket)));
    }
    body.push_str(&format!("\n{}", plan.pr_url()));
    body
}

/// One JSON notification body posted to a `notify.webhook`. Flat and stable so an operator can
/// pipe it anywhere without a schema.
#[derive(Debug, Serialize)]
pub struct WebhookPayload<'a> {
    pub event: &'static str,
    pub ticket: &'a str,
    pub pr: String,
    pub pr_url: String,
    pub rounds: i64,
    pub attempts: i64,
    pub spend: Vec<WebhookSpend<'a>>,
    pub latest_review: &'a str,
    pub reason: &'a str,
}

#[derive(Debug, Serialize)]
pub struct WebhookSpend<'a> {
    pub provider: &'a str,
    pub spent: i64,
    pub cap: Option<i64>,
}

/// The JSON body a webhook receives.
pub fn webhook_payload(plan: &BreakerPlan) -> WebhookPayload<'_> {
    WebhookPayload {
        event: match plan.kinds.first() {
            Some(CrossingKind::Escalation) => "review_escalated",
            _ => "breaker_crossed",
        },
        ticket: &plan.ticket,
        pr: plan.pr(),
        pr_url: plan.pr_url(),
        rounds: plan.rounds,
        attempts: plan.attempts,
        spend: plan
            .spend
            .iter()
            .map(|s| WebhookSpend {
                provider: &s.provider,
                spent: s.spent,
                cap: s.cap,
            })
            .collect(),
        latest_review: &plan.latest_review,
        reason: &plan.reason,
    }
}

/// The title a native (desktop) notification shows.
pub fn notification_title(plan: &BreakerPlan) -> String {
    if plan.kinds.contains(&CrossingKind::Escalation) {
        format!("Review escalation: {}", plan.ticket)
    } else {
        format!("Review loop held: {}", plan.ticket)
    }
}

// ---------------------------------------------------------------------------------------------
// macOS desktop notifications
// ---------------------------------------------------------------------------------------------

/// One notification waiting for the desktop to pick it up on `/api/v1/state`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Notification {
    /// Monotonic per process; the desktop de-dupes on it so a poll never re-shows one.
    pub id: i64,
    pub at: String,
    pub title: String,
    pub body: String,
    pub ticket: String,
    pub pr: String,
}

/// The bounded, in-memory queue of pending desktop notifications. Shared behind an [`Arc`] for
/// [`crate::budget::BudgetLedger`]'s reason: the off-loop breaker task writes it, and the control
/// task's snapshot reads it, so neither can own it. It is never held across an `.await`.
#[derive(Default)]
pub struct NotificationsState {
    inner: Mutex<NotificationsInner>,
}

#[derive(Default)]
struct NotificationsInner {
    next_id: i64,
    pending: Vec<Notification>,
}

impl NotificationsState {
    /// Appends one notification, dropping the OLDEST once [`MAX_NOTIFICATIONS`] is reached.
    pub fn push(&self, at: DateTime<Utc>, title: String, body: String, ticket: String, pr: String) {
        let mut st = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let id = st.next_id + 1;
        st.next_id = id;
        st.pending.push(Notification {
            id,
            at: at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            title,
            body,
            ticket,
            pr,
        });
        let len = st.pending.len();
        if len > MAX_NOTIFICATIONS {
            st.pending.drain(..len - MAX_NOTIFICATIONS);
        }
    }

    /// The pending notifications, oldest first.
    pub fn pending(&self) -> Vec<Notification> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending
            .clone()
    }
}

// ---------------------------------------------------------------------------------------------
// The off-loop task and its seams
// ---------------------------------------------------------------------------------------------

/// Applies the `rhapsody:human` hold. A trait so the task is testable without a tracker, exactly
/// as [`crate::ghsummons::PrCommentSink`] is.
#[async_trait::async_trait]
pub trait BreakerHoldSink: Send + Sync {
    /// Adds `rhapsody:human` to one ticket. Infallible by contract: a failed label write is logged
    /// where it happens, and the crossing is already persisted (see the module docs), so nothing
    /// here has a caller with anything to do about the failure.
    async fn hold(&self, ticket: &str, issue_id: &str, team_id: &str);
}

/// The production hold: the live tracker's `add_issue_label`, resolved per call so a reload is
/// picked up without re-wiring the task (the same stance as the review intro task's linker).
pub struct TrackerHoldSink<TF> {
    tracker: TF,
}

impl<TF> TrackerHoldSink<TF> {
    pub fn new(tracker: TF) -> TrackerHoldSink<TF> {
        TrackerHoldSink { tracker }
    }
}

#[cfg(test)]
mod tracker_hold_tests {
    use super::*;

    #[tokio::test]
    async fn non_tracker_breaker_hold_never_labels_a_synthetic_ticket() {
        let tracker = Arc::new(rhapsody_tracker::fake::Fake::new());
        let sink = TrackerHoldSink::new(|| Some(tracker.clone() as Arc<dyn Tracker>));
        for key in [
            "lead:example/repo#0:1@manager",
            "limit:account@manager",
            "LIMIT:ACCOUNT@MANAGER",
            "pr:example/repo#1@reviewer",
        ] {
            sink.hold(key, key, "TEAM-1").await;
        }
        assert!(tracker.add_label_calls().is_empty());
        sink.hold("STUDIO-123", "ID-1", "TEAM-1").await;
        assert_eq!(tracker.add_label_calls().len(), 1);
    }
}

#[async_trait::async_trait]
impl<TF> BreakerHoldSink for TrackerHoldSink<TF>
where
    TF: Fn() -> Option<Arc<dyn Tracker>> + Send + Sync,
{
    async fn hold(&self, ticket: &str, issue_id: &str, team_id: &str) {
        if crate::review::is_non_tracker_key(issue_id) {
            tracing::debug!(issue_id, "skipping tracker hold for a local run key");
            return;
        }
        let Some(tracker) = (self.tracker)() else {
            tracing::warn!(
                ticket,
                "breaker: no tracker is loaded, so the hold could not be applied; the crossing is \
                 recorded and will not notify again"
            );
            return;
        };
        match tracker
            .add_issue_label(issue_id, team_id, crate::teams::HUMAN_LABEL)
            .await
        {
            Ok(()) => tracing::warn!(
                ticket,
                label = crate::teams::HUMAN_LABEL,
                "breaker: held the ticket for a human; no new run or review round will start"
            ),
            Err(e) => tracing::warn!(
                ticket,
                err = %e,
                "breaker: the ticket could not be held; the crossing is recorded, so a later tick \
                 will not hold or notify again — apply the label by hand"
            ),
        }
    }
}

/// One notification channel (`macos`, `webhook`, `ntfy`). A trait so tests can record deliveries
/// and assert on a failure without an HTTP server.
#[async_trait::async_trait]
pub trait NotifyChannel: Send + Sync {
    async fn send_lead(&self, title: &str, body: &str, _link: &str) -> Result<(), String> {
        self.send_account(title, body).await
    }
    /// Delivers one generic one-way notification (STUDIO-1504): a title, a body and an optional
    /// link. Defaults to [`Self::send_lead`], whose shape is already exactly this — a channel that
    /// has a richer lead format overrides it rather than gaining a second method.
    async fn send_notice(&self, title: &str, body: &str, link: &str) -> Result<(), String> {
        self.send_lead(title, body, link).await
    }
    /// Account-level reporting shares the transport, without pretending an account is a PR.
    async fn send_account(&self, _title: &str, _body: &str) -> Result<(), String> {
        Err("account notifications unsupported on this channel".into())
    }
    /// Delivers one notification. `Err` is logged by the caller; delivery is best-effort, so one
    /// refusing channel never stops another.
    async fn send(&self, plan: &BreakerPlan, body: &str) -> Result<(), String>;

    /// The channel's name, for the log line.
    fn name(&self) -> &'static str;
}

/// One generic one-way notification's JSON body posted to a `notify.webhook` (STUDIO-1504). Flat and
/// stable, and distinct from [`WebhookPayload`] (which carries `event`), so an operator's endpoint
/// can tell a breaker crossing from a generic notification.
#[derive(Debug, Serialize)]
pub struct WebhookNotice<'a> {
    pub title: &'a str,
    pub body: &'a str,
    pub link: &'a str,
}

/// A `notify.webhook` URL: POST a JSON body.
pub struct WebhookChannel {
    url: String,
    client: reqwest::Client,
}

impl WebhookChannel {
    pub fn new(url: String) -> WebhookChannel {
        WebhookChannel {
            url,
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait::async_trait]
impl NotifyChannel for WebhookChannel {
    async fn send(&self, plan: &BreakerPlan, _body: &str) -> Result<(), String> {
        let payload = webhook_payload(plan);
        let res = self
            .client
            .post(&self.url)
            .json(&payload)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if res.status().is_success() {
            Ok(())
        } else {
            Err(format!("webhook returned HTTP {}", res.status()))
        }
    }
    /// A generic one-way notification (STUDIO-1504), so a webhook-only install still receives the
    /// four human-warranting events rather than an "unsupported" error per trigger.
    async fn send_notice(&self, title: &str, body: &str, link: &str) -> Result<(), String> {
        let res = self
            .client
            .post(&self.url)
            .json(&WebhookNotice { title, body, link })
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if res.status().is_success() {
            Ok(())
        } else {
            Err(format!("webhook returned HTTP {}", res.status()))
        }
    }
    fn name(&self) -> &'static str {
        "webhook"
    }
}

/// A `notify.ntfy` topic: push to `https://ntfy.sh/<topic>`, or to the URL the operator wrote when
/// it already carries a scheme. The body is the plain notification text, which is what ntfy renders.
pub struct NtfyChannel {
    url: String,
    client: reqwest::Client,
}

impl NtfyChannel {
    /// Builds the channel from the configured value: a bare topic lands on `ntfy.sh`, a value that
    /// looks like a URL is used verbatim.
    pub fn new(value: String) -> NtfyChannel {
        let url = if value.contains("://") {
            value
        } else {
            format!("https://ntfy.sh/{value}")
        };
        NtfyChannel {
            url,
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait::async_trait]
impl NotifyChannel for NtfyChannel {
    async fn send_lead(&self, title: &str, body: &str, link: &str) -> Result<(), String> {
        let request = self
            .client
            .post(&self.url)
            .header("Title", title)
            .body(body.to_string());
        let request = if link.is_empty() {
            request
        } else {
            request.header("Click", link)
        };
        let res = request.send().await.map_err(|e| e.to_string())?;
        if res.status().is_success() {
            Ok(())
        } else {
            Err(format!("ntfy returned HTTP {}", res.status()))
        }
    }
    async fn send_account(&self, title: &str, body: &str) -> Result<(), String> {
        let res = self
            .client
            .post(&self.url)
            .header("Title", title)
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if res.status().is_success() {
            Ok(())
        } else {
            Err(format!("ntfy returned HTTP {}", res.status()))
        }
    }
    async fn send(&self, plan: &BreakerPlan, body: &str) -> Result<(), String> {
        let res = self
            .client
            .post(&self.url)
            .header("Title", notification_title(plan))
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if res.status().is_success() {
            Ok(())
        } else {
            Err(format!("ntfy returned HTTP {}", res.status()))
        }
    }
    fn name(&self) -> &'static str {
        "ntfy"
    }
}

/// A `notify.macos: true` channel: pushes onto the shared [`NotificationsState`], which
/// `/api/v1/state` renders for the desktop app to turn into a native notification.
pub struct MacosChannel {
    state: Arc<NotificationsState>,
}

impl MacosChannel {
    pub fn new(state: Arc<NotificationsState>) -> MacosChannel {
        MacosChannel { state }
    }
}

#[async_trait::async_trait]
impl NotifyChannel for MacosChannel {
    async fn send_account(&self, title: &str, body: &str) -> Result<(), String> {
        self.state.push(
            Utc::now(),
            title.into(),
            body.into(),
            String::new(),
            String::new(),
        );
        Ok(())
    }
    async fn send(&self, plan: &BreakerPlan, body: &str) -> Result<(), String> {
        self.state.push(
            Utc::now(),
            notification_title(plan),
            body.to_string(),
            plan.ticket.clone(),
            plan.pr(),
        );
        Ok(())
    }
    fn name(&self) -> &'static str {
        "macos"
    }
}

/// Everything the off-loop breaker task needs. No `Orchestrator`, no store, no control channel —
/// the off-loop guarantee, in the type, as [`crate::reviewnotify::ReviewNotifyDeps`] states it.
pub struct BreakerDeps {
    /// Where the `rhapsody:human` hold is applied. `None` disables the hold but not the
    /// notification: a daemon with no tracker can still tell the operator.
    pub hold: Option<Arc<dyn BreakerHoldSink>>,
    /// The room the one-line post goes to. `None` when there is no on-disk runtime home.
    pub room: Option<Arc<dyn RoomLog>>,
    /// Every configured notification channel.
    pub channels: Vec<Arc<dyn NotifyChannel>>,
}

/// Performs one crossing, off the control task. Infallible by contract, like
/// [`crate::draftpoke::perform_nudge`]: every branch is logged where it happens, and the crossing
/// was already recorded, so a failed write is not retried.
pub async fn perform_crossing(plan: &BreakerPlan, deps: &BreakerDeps, at: DateTime<Utc>) {
    let body = crossing_body(plan);
    // 1. The hold, first: stopping NEW work is the point, and the operator can act the moment the
    //    notification lands. Only a ROUND or SPEND crossing holds — a manager escalation is a
    //    decision to NOTIFY (the loop is already stopped by the adjudication), not a new hold.
    if crossing_applies_hold(&plan.kinds)
        && let Some(hold) = deps.hold.as_ref()
    {
        hold.hold(&plan.ticket, &plan.issue_id, &plan.team_id).await;
    }
    // 2. One line to the team room. The ticket is the ref (it re-grounds against the candidate map).
    if let Some(room) = deps.room.as_ref() {
        let mut msg = Message::room(BREAKER_IDENTITY, at, body.clone());
        msg.refs = vec![plan.ticket.clone(), plan.pr()];
        if let Err(e) = room.append(&msg) {
            tracing::warn!(
                ticket = %plan.ticket,
                err = %e,
                "breaker: the crossing could not be posted to the room"
            );
        }
    }
    // 3. Every configured channel, independently: one refusing channel never suppresses another.
    if deps.channels.is_empty() {
        tracing::warn!(
            ticket = %plan.ticket,
            "breaker: a limit was crossed but no notify: channel is configured, so no operator was \
             notified"
        );
    }
    for channel in &deps.channels {
        match channel.send(plan, &body).await {
            Ok(()) => tracing::warn!(
                ticket = %plan.ticket,
                pr = %plan.pr(),
                channel = channel.name(),
                "breaker: notified the operator of a crossed limit"
            ),
            Err(e) => tracing::warn!(
                ticket = %plan.ticket,
                channel = channel.name(),
                err = %e,
                "breaker: a notify channel refused the crossing; other channels are unaffected"
            ),
        }
    }
}

/// Consumes [`BreakerPlan`]s until `ctx` is cancelled or every sender is dropped.
pub async fn run_breaker_task(
    mut ctx: crate::control_loop::CancelWait,
    deps: BreakerDeps,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<BreakerPlan>,
) {
    tracing::info!(
        "runaway-loop breaker task started (off-loop; a tick never waits on a notification)"
    );
    loop {
        let plan = tokio::select! {
            _ = ctx.cancelled() => return,
            r = rx.recv() => match r {
                Some(r) => r,
                None => return,
            },
        };
        perform_crossing(&plan, &deps, Utc::now()).await;
    }
}

// ---------------------------------------------------------------------------------------------
// The control-task planner
// ---------------------------------------------------------------------------------------------

impl Orchestrator {
    /// The shared pending-notifications cell (STUDIO-1026), for the composition root to hand the
    /// macOS channel so the off-loop breaker task can push to the same cell `/api/v1/state` reads.
    pub fn notifications_state(&self) -> Arc<NotificationsState> {
        Arc::clone(&self.notifications)
    }

    /// Opens the breaker task's channel, storing the sender and handing back the receiver for
    /// [`run_breaker_task`]. A method rather than a public field, mirroring
    /// [`Orchestrator::open_review_notify_channel`].
    pub fn open_breaker_channel(&mut self) -> tokio::sync::mpsc::UnboundedReceiver<BreakerPlan> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.breaker_tx = Some(tx);
        rx
    }

    /// Whether any breaker LIMIT is configured — a round threshold or a per-ticket cap. The tick
    /// pass tests this first so a daemon that configures neither does no store work at all.
    pub(crate) fn breaker_limits_configured(&self) -> bool {
        self.round_threshold().is_some() || self.per_ticket_caps().next().is_some()
    }

    /// Whether any `notify:` channel is configured. Escalation notifications are gated on this (and
    /// on a task existing) so a daemon with no channels does nothing new. Delegates to
    /// [`Orchestrator::notify_backend_configured`] so an explicit `notify.backend` (STUDIO-1504)
    /// resolves the same way here as in the generic notify seam.
    pub(crate) fn notify_channels_configured(&self) -> bool {
        self.notify_backend_configured()
    }

    fn round_threshold(&self) -> Option<i64> {
        self.teams
            .as_ref()
            .and_then(|t| t.review_hold_after_rounds())
            .and_then(|n| i64::try_from(n).ok())
            .filter(|n| *n > 0)
    }

    /// Every provider with a positive per-ticket cap, in deterministic (BTreeMap) order.
    fn per_ticket_caps(&self) -> impl Iterator<Item = (&str, i64)> {
        self.eff
            .as_ref()
            .into_iter()
            .flat_map(|e| e.cfg.budgets.iter())
            .filter(|(_, b)| b.per_ticket > 0)
            .map(|(p, b)| (p.as_str(), b.per_ticket))
    }

    /// Sends a planned crossing to the off-loop task. A no-op when no task is running or it has
    /// stopped — neither is worth failing a tick over; the crossing is already persisted.
    pub(crate) fn request_crossing(&self, plan: BreakerPlan) {
        let Some(tx) = self.breaker_tx.as_ref() else {
            return;
        };
        let pr = plan.pr();
        if tx.send(plan).is_err() {
            tracing::warn!(
                pr = %pr,
                "breaker: the notification task is gone; the crossing is recorded but no operator \
                 was notified"
            );
        }
    }

    /// The tick pass: scans the live watch set for limit crossings, persists each new one, and
    /// hands it to the off-loop task.
    ///
    /// Runs ON the control task, beside [`Orchestrator::reconcile_review_divergence`]. It reads the
    /// store and the watch set, both loop-owned, and sends an owned plan outward.
    pub(crate) fn reconcile_breaker(&mut self) {
        if !self.breaker_limits_configured() || !self.review_ticketless_enabled() {
            return;
        }
        if self.breaker_tx.is_none() && !self.lead_enabled() {
            return;
        }
        // Fail closed while the hold ledger is un-primed, for the reconciliation sweep's reason
        // (STUDIO-949): with no pass having read the board, an empty label set is "unknown", and
        // re-holding a ticket an operator deliberately held is the false action to avoid.
        let (labelled, primed) = self.human_holds.labelled_and_primed();
        if !primed {
            return;
        }
        let rows = match self.store().load_live_review_watch() {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(
                    err = %e,
                    "breaker: the watch set could not be read; no crossing was decided"
                );
                return;
            }
        };
        // Load once; a small table (one row per ticket that has ever crossed).
        let persisted = match self.store().load_breaker_crossings() {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(
                    err = %e,
                    "breaker: the persisted crossings could not be read; no crossing was decided"
                );
                return;
            }
        };
        // Unique by TICKET, preserving the watch set's stable order. A ticket with several
        // reviewers, or (pathologically) several watched pull requests, is ONE ticket and must
        // cross once — its persisted row is per ticket, so a second plan in the same pass would
        // race the first save against a stale `persisted` snapshot.
        let mut seen: Vec<(String, String, String, i64)> = Vec::new();
        for row in &rows {
            let Some(ticket) = crate::reviewdone::origin_ticket(&row.introduced_by) else {
                continue; // a console: row has no ticket to hold
            };
            if ticket.is_empty() || labelled.contains(&ticket.to_ascii_lowercase()) {
                continue; // already held (by a prior crossing or by the operator)
            }
            if seen.iter().any(|(t, _, _, _)| t == ticket) {
                continue;
            }
            seen.push((
                ticket.to_string(),
                row.key.owner.clone(),
                row.key.repo.clone(),
                row.key.number,
            ));
        }
        for (ticket, owner, repo, number) in seen {
            if let Some(plan) = self.plan_crossing(&ticket, &owner, &repo, number, &persisted) {
                if self.lead_enabled() {
                    crate::leaditems::enqueue(
                        self.store(),
                        rhapsody_store::LeadTrigger::BreakerHold {
                            ticket: plan.plan.ticket.clone(),
                            pr: plan.plan.pr(),
                            kinds: plan
                                .plan
                                .kinds
                                .iter()
                                .map(|k| k.as_str().to_string())
                                .collect(),
                        },
                    );
                    // Admission closes synchronously, before the remote label task runs. The lead
                    // is additive and enqueue failure cannot bypass either hold.
                    if let Err(error) = self.store().park_subject(&rhapsody_store::SubjectHold {
                        subject: ticket.clone(),
                        kind: "breaker".into(),
                        need:
                            "Circuit breaker crossed; concrete change or operator release required"
                                .into(),
                        at: rhapsody_store::format_summon_at(Utc::now()),
                        ..Default::default()
                    }) {
                        tracing::warn!(%error, "breaker local hold could not be persisted");
                    }
                    self.human_holds.note_human_label(&ticket);
                }
                // Persist BEFORE handing off, so a restart in the gap still counts the crossing.
                let row = rhapsody_store::BreakerCrossingRow {
                    ticket: plan.plan.ticket.clone(),
                    notified_rounds: plan.new_notified_rounds,
                    notified_providers: plan.new_notified_providers.clone(),
                };
                if let Err(e) = self.store().save_breaker_crossing(&row) {
                    tracing::warn!(
                        ticket = %plan.plan.ticket,
                        err = %e,
                        "breaker: the crossing could not be persisted; it may notify again after a \
                         restart"
                    );
                }
                tracing::warn!(
                    ticket = %plan.plan.ticket,
                    pr = %plan.plan.pr(),
                    kinds = ?plan.plan.kinds,
                    "breaker: a limit was crossed; holding the ticket and notifying the operator"
                );
                self.request_crossing(plan.plan);
            }
        }
    }

    /// Plans one ticket's crossing, or `None` when nothing crossed (or the pull request has no
    /// ticket to hold).
    fn plan_crossing(
        &self,
        ticket: &str,
        owner: &str,
        repo: &str,
        number: i64,
        persisted: &[rhapsody_store::BreakerCrossingRow],
    ) -> Option<PlannedCrossing> {
        let prev = persisted
            .iter()
            .find(|r| r.ticket == ticket)
            .cloned()
            .unwrap_or_default();
        let rounds = self
            .store()
            .count_changes_requested_review_runs(owner, repo, number)
            .unwrap_or(0);
        let attempts = self.store().count_runs_for(ticket).unwrap_or(0);
        // This coordinate's live watch rows, read once: their statuses give each reviewer's LATEST
        // review OUTCOME and the head it was made at.
        let live = self.store().load_live_review_watch().unwrap_or_default();
        let mine: Vec<&rhapsody_store::ReviewWatchRow> = live
            .iter()
            .filter(|r| r.key.owner == owner && r.key.repo == repo && r.key.number == number)
            .collect();
        let latest_review = mine.first().map(|r| r.status.clone()).unwrap_or_default();
        // "The most recent completed verdict at the PR's current head is an approval" (STUDIO-1440).
        //
        // Two sources, each answering the half it can: the LIVE ROWS give the HEAD SCOPE, the per-run
        // LEDGER gives the ORDER. The ledger is keyed by run and carries no head, so it cannot name a
        // head by itself — a review that was IN FLIGHT when the head moved completes against the head
        // pinned at its dispatch (F-SHA), so its verdict is the newest by `ended_at` while being about
        // a head the pull request has already left. A PR-wide "newest verdict" therefore let that
        // superseded completion shadow a genuine approval at the head (PR #368 round 2, B2).
        //
        // The live rows carry the head each round READ (`last_reviewed_sha`, advanced only by a
        // completed round), so a terminal row is a verdict at the head it read:
        //   * `approved`/`reviewed` with `last_reviewed_sha == current_head` is a completed verdict
        //     AT the current head — a patch-changing advance re-arms every row it can reach back to
        //     `requested`, so a surviving terminal row read the head the pull request still stands
        //     at (a patch-preserving move advances `last_reviewed_sha` and keeps the status);
        //   * `requested`/`in_flight`/`truncated` is no completed verdict at the current head at all,
        //     so a sibling still owing a round must not defeat an approval (round 1, B1).
        // A head proven patch-identical to the current one (a base-branch merge) counts too — the
        // same carried-approval proof auto-merge uses.
        //
        // Within that head scope the question is which verdict is NEWEST, and only the ledger knows:
        // two reviewers can complete different verdicts at the SAME head, in either order, so "every
        // reviewer at this head approves" is not "the most recent completed verdict approves" (round
        // 3, B3). The store ranks the head-scoped reviewers' completed rounds by a durable
        // COMPLETION SEQUENCE assigned at the verdict write — not by `ended_at`, which is truncated
        // to seconds and would fall through to `runs.id` (allocated at run START) for a same-second
        // pair, letting an older findings round outrank a newer approval (round 4) — and returns the
        // newest one's verdict.
        //
        // The current head is the watcher's last observation of the coordinate. With no observation
        // the head is unknown, so the hold fails CLOSED (it applies) rather than claim an approval
        // stands at a head the daemon cannot name.
        let coord = crate::prstate::PrCoord::new(owner, repo, number);
        let (current_head, proven) = match self.review_observed_head.get(&coord) {
            Some(observed) => (observed.head.as_str(), observed.unchanged_from.as_slice()),
            None => ("", &[][..]),
        };
        let at_head_reviewers: Vec<String> = mine
            .iter()
            .filter(|r| {
                matches!(
                    r.status.as_str(),
                    rhapsody_store::REVIEW_STATUS_APPROVED | rhapsody_store::REVIEW_STATUS_REVIEWED
                ) && !r.last_reviewed_sha.is_empty()
                    && !current_head.is_empty()
                    && (r.last_reviewed_sha == current_head
                        || proven.iter().any(|p| p == &r.last_reviewed_sha))
            })
            .map(|r| r.key.reviewer.clone())
            .collect();
        // Suppress only when the NEWEST completed verdict among those head-scoped reviewers approves
        // — the pull request has converged at the head. A `reviewed` (findings) verdict that is the
        // newest at the head is an unanswered round, so the hold still applies; so is an empty set or
        // an unreadable ledger (fail closed).
        let approved_at_head = !at_head_reviewers.is_empty()
            && self
                .store()
                .newest_completed_review_verdict_among(owner, repo, number, &at_head_reviewers)
                .ok()
                .flatten()
                .as_deref()
                == Some(rhapsody_store::REVIEW_VERDICT_APPROVED);

        let mut kinds = Vec::new();
        let mut threshold = 0i64;
        let mut notified_rounds = prev.notified_rounds;
        if let Some(h) = self.round_threshold()
            && rounds >= notified_rounds + h
            && !approved_at_head
        {
            kinds.push(CrossingKind::Rounds);
            threshold = h;
            // The multiple actually reached, so the NEXT crossing is a further `h` rounds away
            // (round ten after five), never the same one again.
            notified_rounds = (rounds / h) * h;
        }
        let mut notified_providers = prev.notified_providers.clone();
        let caps: Vec<(&str, i64)> = self.per_ticket_caps().collect();
        let spend: Vec<ProviderSpend> = if caps.is_empty() {
            Vec::new()
        } else {
            let mut spent = self
                .store()
                .ticket_spend_by_provider(ticket, owner, repo, number)
                .unwrap_or_default();
            spent.sort_by(|a, b| a.provider.cmp(&b.provider));
            spent
                .into_iter()
                .map(|s| ProviderSpend {
                    cap: caps.iter().find(|(p, _)| *p == s.provider).map(|(_, c)| *c),
                    provider: s.provider,
                    spent: s.total_tokens,
                })
                .collect()
        };
        for (provider, cap) in &caps {
            if notified_providers.iter().any(|p| p == provider) {
                continue;
            }
            let spent = spend
                .iter()
                .find(|s| s.provider == *provider)
                .map(|s| s.spent)
                .unwrap_or(0);
            if spent >= *cap {
                kinds.push(CrossingKind::Spend);
                notified_providers.push((*provider).to_string());
            }
        }
        if kinds.is_empty() {
            return None;
        }
        // The opaque tracker ids the hold needs, resolved from the ticket's newest run.
        let (issue_id, team_id) = self
            .store()
            .list_issue_runs(rhapsody_store::RunFilter {
                issue: ticket.to_string(),
                limit: 1,
                ..rhapsody_store::RunFilter::default()
            })
            .ok()
            .and_then(|runs| runs.into_iter().next())
            .map(|r| (r.issue_id, r.team_id))
            .unwrap_or_default();
        Some(PlannedCrossing {
            plan: BreakerPlan {
                kinds,
                ticket: ticket.to_string(),
                issue_id,
                team_id,
                owner: owner.to_string(),
                repo: repo.to_string(),
                number,
                rounds,
                threshold,
                attempts,
                spend,
                latest_review,
                reason: String::new(),
            },
            new_notified_rounds: notified_rounds,
            new_notified_providers: notified_providers,
        })
    }

    /// Sends a notification for a NEW manager escalation (STUDIO-1026, acceptance 3), deduped per
    /// (pull request, head) in memory. Called from the reconciliation sweep's `set_review_divergences`.
    pub(crate) fn notify_escalation(&mut self, div: &crate::reviewreconcile::Divergence) {
        if !self.notify_channels_configured() || self.breaker_tx.is_none() {
            return;
        }
        let key = format!("notify:escalation:{}@{}", div.pr, div.adjudicated_head);
        if !self.escalation_notified.insert(key.clone()) {
            return; // already told them about this head
        }
        // STUDIO-1504: the restart half of the dedup. The in-memory set above is pruned when an
        // escalation recovers (so a recurrence at a NEW head notifies again); this durable ledger
        // stops a restart from re-sending a trip the operator already saw.
        if self.store().durable_dedup_enabled() {
            match self.store().reserve_lead_report(&key, 1) {
                Ok(true) => {}
                Ok(false) => return,
                Err(error) => {
                    tracing::warn!(
                        ticket = %div.ticket,
                        %error,
                        "breaker: the escalation dedup marker could not be persisted; skipping \
                         rather than risking a repeat"
                    );
                    return;
                }
            }
        }
        let Some((owner, repo, number)) = parse_pr(&div.pr) else {
            return;
        };
        let (issue_id, team_id) = self
            .store()
            .list_issue_runs(rhapsody_store::RunFilter {
                issue: div.ticket.clone(),
                limit: 1,
                ..rhapsody_store::RunFilter::default()
            })
            .ok()
            .and_then(|runs| runs.into_iter().next())
            .map(|r| (r.issue_id, r.team_id))
            .unwrap_or_default();
        // The real author-run count, not a hard-coded zero: the escalation body is read by the
        // operator and the room, and a false "0 attempts" is worse than omitting the line.
        let attempts = self.store().count_runs_for(&div.ticket).unwrap_or(0);
        let plan = BreakerPlan {
            kinds: vec![CrossingKind::Escalation],
            ticket: div.ticket.clone(),
            issue_id,
            team_id,
            owner,
            repo,
            number,
            rounds: i64::try_from(div.rounds).unwrap_or(0),
            threshold: 0,
            attempts,
            spend: Vec::new(),
            latest_review: div.findings.join("; "),
            reason: div.reason.clone(),
        };
        tracing::warn!(
            ticket = %plan.ticket,
            pr = %plan.pr(),
            "review reconciliation: a manager escalation will notify the operator through the \
             configured channels"
        );
        self.request_crossing(plan);
    }

    /// Drops escalation dedupe keys that are no longer present, so an escalation that recovers and
    /// later recurs notifies again.
    pub(crate) fn prune_escalations(&mut self, found: &[crate::reviewreconcile::Divergence]) {
        let live: std::collections::HashSet<String> = found
            .iter()
            .filter(|d| d.kind == crate::reviewreconcile::DivergenceKind::ReviewEscalated)
            .map(|d| format!("notify:escalation:{}@{}", d.pr, d.adjudicated_head))
            .collect();
        self.escalation_notified.retain(|k| live.contains(k));
    }
}

/// A planned crossing plus the persisted-row fields it advances.
struct PlannedCrossing {
    plan: BreakerPlan,
    new_notified_rounds: i64,
    new_notified_providers: Vec<String>,
}

/// Splits `owner/repo#number` into its parts, or `None` when it is not that shape.
fn parse_pr(pr: &str) -> Option<(String, String, i64)> {
    let (repo_part, number) = pr.rsplit_once('#')?;
    let (owner, repo) = repo_part.split_once('/')?;
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some((owner.to_string(), repo.to_string(), number.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;
    use rhapsody_config::room::{CaughtUp, Cursor, RoomError};
    use rhapsody_config::teams::{Identity, Review, ReviewMode, Teams};
    use rhapsody_store::{
        OUTCOME_COMPLETED, REVIEW_STATUS_APPROVED, REVIEW_STATUS_REQUESTED, REVIEW_STATUS_REVIEWED,
        REVIEW_VERDICT_APPROVED, REVIEW_VERDICT_CHANGES_REQUESTED, ReviewWatchKey, ReviewWatchRow,
        RunEnd, RunProvenance, RunStart, Sqlite, Store, StorePath,
    };
    use rhapsody_tracker::fake::Fake;

    use super::*;
    use crate::testsupport::{empty_effective, empty_resolved_project};

    const REPO_URL: &str = "git@github.com:makewhatis/rhapsody.git";

    type SharedStore = Arc<dyn Store + Send + Sync>;

    fn store() -> SharedStore {
        Arc::new(Sqlite::open(StorePath::InMemory).expect("open in-memory"))
    }

    /// Seeds one run for `key`, ending it with `outcome`/`tokens` and recording `provider`.
    fn seed_run(store: &SharedStore, key: &str, outcome: &str, tokens: i64, provider: &str) {
        let at = "2026-09-20T00:00:00Z";
        let id = store
            .start_run(RunStart {
                issue_id: format!("iss-{key}"),
                issue_identifier: key.into(),
                team_id: "team-1".into(),
                started_at: at.into(),
                ..Default::default()
            })
            .expect("start_run");
        store
            .end_run(
                id,
                RunEnd {
                    outcome: outcome.into(),
                    total_tokens: tokens,
                    ended_at: at.into(),
                    ..Default::default()
                },
            )
            .expect("end_run");
        if !provider.is_empty() {
            store
                .set_run_provenance(
                    id,
                    &RunProvenance {
                        provider: provider.into(),
                        harness: "claude".into(),
                        model: "m".into(),
                        ..Default::default()
                    },
                )
                .expect("provenance");
        }
    }

    fn seed_watch(store: &SharedStore, ticket: &str) {
        store
            .save_review_watch(ReviewWatchRow {
                key: ReviewWatchKey {
                    owner: "makewhatis".into(),
                    repo: "rhapsody".into(),
                    number: 12,
                    reviewer: "alice".into(),
                },
                author: "bob".into(),
                introduced_by: format!("handoff:{ticket}"),
                requested_sha: "sha".into(),
                last_reviewed_sha: "sha".into(),
                status: REVIEW_STATUS_REQUESTED.into(),
                open: true,
            })
            .expect("save watch");
    }

    /// Seeds one COMPLETED review run for `key` and records `verdict` against it — the shape the
    /// breaker's Rounds count reads (STUDIO-1440). A completed review run with NO verdict row (or an
    /// approving one) is not a round the limit counts. The run carries the `anthropic` provider, so
    /// the per-ticket spend join sees it as the original incident's reviews did.
    fn seed_review_round(store: &SharedStore, key: &str, verdict: &str) {
        seed_review_round_at(store, key, verdict, "2026-09-20T00:00:00Z");
    }

    /// [`seed_review_round`] with an explicit completion time. Ordering no longer matters to the
    /// breaker's suppression (it is head-scoped off the live rows, not the ledger), but the round
    /// timestamps keep the seeds realistic and distinct (STUDIO-1440).
    fn seed_review_round_at(store: &SharedStore, key: &str, verdict: &str, at: &str) {
        let id = store
            .start_run(RunStart {
                issue_id: format!("iss-{key}"),
                issue_identifier: key.into(),
                team_id: "team-1".into(),
                started_at: at.into(),
                ..Default::default()
            })
            .expect("start_run");
        store
            .end_run(
                id,
                RunEnd {
                    outcome: OUTCOME_COMPLETED.into(),
                    total_tokens: 10,
                    ended_at: at.into(),
                    ..Default::default()
                },
            )
            .expect("end_run");
        store
            .set_run_provenance(
                id,
                &RunProvenance {
                    provider: "anthropic".into(),
                    harness: "claude".into(),
                    model: "m".into(),
                    ..Default::default()
                },
            )
            .expect("provenance");
        store.set_review_verdict(id, verdict).expect("verdict");
    }

    /// A live watch row already settled at `status` on the head it read — the shape that makes
    /// `plan_crossing` read "the most recent completed verdict at the current head".
    fn seed_watch_status(store: &SharedStore, ticket: &str, status: &str) {
        seed_watch_row(store, ticket, "alice", status);
    }

    /// [`seed_watch_status`] for a named reviewer — so a test can put a pending SIBLING beside the
    /// approving reviewer (STUDIO-1440).
    fn seed_watch_row(store: &SharedStore, ticket: &str, reviewer: &str, status: &str) {
        seed_watch_row_at(store, ticket, reviewer, status, "sha");
    }

    /// [`seed_watch_row`] with an explicit head the row READ — the head the breaker scopes its
    /// suppression to (`last_reviewed_sha`), so a test can place a verdict at a head other than the
    /// one the watcher observes (STUDIO-1440).
    fn seed_watch_row_at(
        store: &SharedStore,
        ticket: &str,
        reviewer: &str,
        status: &str,
        read_sha: &str,
    ) {
        store
            .save_review_watch(ReviewWatchRow {
                key: ReviewWatchKey {
                    owner: "makewhatis".into(),
                    repo: "rhapsody".into(),
                    number: 12,
                    reviewer: reviewer.into(),
                },
                author: "bob".into(),
                introduced_by: format!("handoff:{ticket}"),
                requested_sha: read_sha.into(),
                last_reviewed_sha: read_sha.into(),
                status: status.into(),
                open: true,
            })
            .expect("save watch");
    }

    /// The watcher's last observation of this test PR's head — the head the breaker scopes its
    /// suppression to (STUDIO-1440). With no observation the head is unknown and the hold fails
    /// closed.
    fn observe_head(o: &mut Orchestrator, head: &str) {
        o.review_observed_head.insert(
            crate::prstate::PrCoord::new("makewhatis", "rhapsody", 12),
            crate::prepare::ReviewHeadObservation {
                open: true,
                head: head.into(),
                unchanged_from: Vec::new(),
            },
        );
    }

    fn teams(hold_after: i64) -> Teams {
        Teams {
            enabled: true,
            review: Review {
                mode: ReviewMode::Ticketless,
                hold_after_rounds: hold_after,
                ..Review::default()
            },
            roster: vec![Identity {
                name: "alice".into(),
                ..Default::default()
            }],
            ..Teams::disabled()
        }
    }

    fn orch(store: SharedStore, hold_after: i64, caps: &[(&str, i64)]) -> Orchestrator {
        let tracker = Arc::new(Fake::new());
        let mut eff = empty_effective(tracker.clone());
        let mut proj = empty_resolved_project("rhapsody", tracker);
        proj.repo = REPO_URL.to_string();
        eff.projects = vec![proj];
        for (provider, cap) in caps {
            eff.cfg.budgets.insert(
                (*provider).to_string(),
                rhapsody_config::ProviderBudget {
                    daily_usd: 0.0,
                    daily_tokens: 0,
                    per_ticket: *cap,
                },
            );
        }
        let mut o = Orchestrator::new("WORKFLOW.md");
        o.eff = Some(eff);
        o.teams = Some(teams(hold_after));
        o.set_store(store);
        // A selection pass has read the board, so the hold ledger is a real answer.
        o.human_holds.begin_pass(true);
        o
    }

    /// The acceptance shape: at the fifth changes-requested review round the ticket is planned for a
    /// hold, the crossing is persisted, and the plan names the PR, the rounds and the spend.
    ///
    /// Mutation: count every completed review run (or DISPATCHES, or drop the threshold) and either
    /// no crossing is planned at five or an approving round counts toward it.
    #[test]
    fn the_fifth_changes_requested_round_crosses_and_names_pr_rounds_and_spend() {
        let store = store();
        seed_watch(&store, "STUDIO-988");
        seed_run(&store, "STUDIO-988", OUTCOME_COMPLETED, 100, "anthropic");
        for r in ["alice", "bob", "carol", "dan", "erin"] {
            seed_review_round(
                &store,
                &format!("pr:makewhatis/rhapsody#12@{r}"),
                REVIEW_VERDICT_CHANGES_REQUESTED,
            );
        }
        // Not completed: must not push the count to six.
        seed_run(
            &store,
            "pr:makewhatis/rhapsody#12@zoe",
            "failed",
            10,
            "fireworks-ai",
        );
        let mut o = orch(store.clone(), 5, &[("anthropic", 30_000_000)]);
        let mut rx = o.open_breaker_channel();

        o.reconcile_breaker();

        let rows = store.load_breaker_crossings().expect("crossings");
        assert_eq!(rows.len(), 1, "one persisted crossing");
        assert!(
            store.load_lead_items().expect("items").is_empty(),
            "disabled lead keeps the old plan"
        );
        assert_eq!(rows[0].ticket, "STUDIO-988");
        assert_eq!(rows[0].notified_rounds, 5);
        let plan = rx.try_recv().expect("one plan was sent");
        assert_eq!(plan.kinds, vec![CrossingKind::Rounds]);
        assert_eq!(
            plan.rounds, 5,
            "the plan names the round count that crossed"
        );
        assert_eq!(plan.attempts, 1, "the plan names the author attempts");
        assert_eq!(plan.number, 12);
        assert!(
            plan.spend.iter().any(|s| s.provider == "anthropic"),
            "the plan reports the spend by provider"
        );
    }

    /// The "at the current head" half of the suppression (STUDIO-1440): an approving verdict that
    /// was read at an OLD head — the author pushed since — is not a verdict at the head the pull
    /// request stands at, so the hold still applies.
    ///
    /// Mutation: drop the head comparison (count any terminal row) and a stale approval holds a pull
    /// request nobody has approved at its current head.
    #[test]
    fn a_stale_approval_does_not_suppress_the_rounds_hold() {
        let store = store();
        // The reviewer approved, but at the head the author has since moved off; the watcher's
        // observation names a different current head.
        seed_watch_row_at(
            &store,
            "STUDIO-988",
            "alice",
            REVIEW_STATUS_APPROVED,
            "old-sha",
        );
        for at in [
            "2026-09-20T00:00:00Z",
            "2026-09-20T00:01:00Z",
            "2026-09-20T00:02:00Z",
            "2026-09-20T00:03:00Z",
            "2026-09-20T00:04:00Z",
        ] {
            seed_review_round_at(
                &store,
                "pr:makewhatis/rhapsody#12@alice",
                REVIEW_VERDICT_CHANGES_REQUESTED,
                at,
            );
        }
        let mut o = orch(store.clone(), 5, &[]);
        observe_head(&mut o, "sha");
        let mut rx = o.open_breaker_channel();
        o.reconcile_breaker();
        let plan = rx
            .try_recv()
            .expect("a stale approval must not suppress the hold");
        assert_eq!(plan.kinds, vec![CrossingKind::Rounds]);
        assert_eq!(plan.rounds, 5);
    }

    /// The boundary of the suppression: a head whose newest verdict ASKED FOR CHANGES is not
    /// converged, so five changes-requested rounds still cross and hold (STUDIO-1440).
    ///
    /// Mutation: suppress on any live row rather than only an approval and this stops crossing.
    #[test]
    fn a_findings_verdict_at_head_still_crosses() {
        let store = store();
        seed_watch_status(&store, "STUDIO-988", REVIEW_STATUS_REVIEWED);
        for r in ["alice", "bob", "carol", "dan", "erin"] {
            seed_review_round(
                &store,
                &format!("pr:makewhatis/rhapsody#12@{r}"),
                REVIEW_VERDICT_CHANGES_REQUESTED,
            );
        }
        let mut o = orch(store.clone(), 5, &[]);
        observe_head(&mut o, "sha");
        let mut rx = o.open_breaker_channel();
        o.reconcile_breaker();
        let plan = rx.try_recv().expect("a not-converged PR still crosses");
        assert_eq!(plan.kinds, vec![CrossingKind::Rounds]);
        assert_eq!(plan.rounds, 5);
    }

    /// The booch#644 sequence (STUDIO-1440): two findings rounds, then three approvals at successive
    /// heads (the last two merge-from-main re-reviews). Only the two findings rounds count, so the
    /// five-round limit is never reached — and the PR sits approved at its current head, so the hold
    /// could not apply even if the count had crossed.
    ///
    /// Mutation: restore the old counting (every completed review run of the coordinate) and the
    /// count is five, not two.
    #[test]
    fn a_converged_pr_with_a_few_findings_rounds_does_not_cross() {
        let store = store();
        // The PR's final state: the reviewer approved the head the last round read.
        seed_watch_status(&store, "STUDIO-1426", REVIEW_STATUS_APPROVED);
        for r in ["alice", "bob"] {
            seed_review_round(
                &store,
                &format!("pr:makewhatis/rhapsody#12@{r}"),
                REVIEW_VERDICT_CHANGES_REQUESTED,
            );
        }
        for r in ["carol", "dan", "erin"] {
            seed_review_round(
                &store,
                &format!("pr:makewhatis/rhapsody#12@{r}"),
                REVIEW_VERDICT_APPROVED,
            );
        }
        assert_eq!(
            store
                .count_changes_requested_review_runs("makewhatis", "rhapsody", 12)
                .expect("count"),
            2,
            "only the two findings rounds count toward the limit"
        );
        let mut o = orch(store.clone(), 5, &[]);
        let mut rx = o.open_breaker_channel();
        o.reconcile_breaker();
        assert!(
            rx.try_recv().is_err(),
            "two findings rounds never reach a five-round limit"
        );
        assert!(
            store
                .load_breaker_crossings()
                .expect("crossings")
                .is_empty(),
            "no crossing is persisted"
        );
    }

    /// The booch#641 sequence (STUDIO-1440): three findings rounds, one approval, then the
    /// reconciliation sweep's manager intervention run, which approves. A `@manager` run is not a
    /// review round, so only the three findings rounds count and nothing crosses.
    ///
    /// Mutation: restore the old counting (every completed review run of the coordinate, the
    /// approving round and the `@manager` run included) and the count is five, not three.
    #[test]
    fn a_manager_intervention_is_not_a_review_round() {
        let store = store();
        seed_watch_status(&store, "STUDIO-1417", REVIEW_STATUS_APPROVED);
        for r in ["alice", "bob", "carol"] {
            seed_review_round(
                &store,
                &format!("pr:makewhatis/rhapsody#12@{r}"),
                REVIEW_VERDICT_CHANGES_REQUESTED,
            );
        }
        seed_review_round(
            &store,
            "pr:makewhatis/rhapsody#12@dan",
            REVIEW_VERDICT_APPROVED,
        );
        // The manager intervention: a `pr:` key, but never a review round.
        seed_run(
            &store,
            "pr:makewhatis/rhapsody#12@manager",
            OUTCOME_COMPLETED,
            10,
            "anthropic",
        );
        assert_eq!(
            store
                .count_changes_requested_review_runs("makewhatis", "rhapsody", 12)
                .expect("count"),
            3,
            "the approval and the manager run are not counted rounds"
        );
        let mut o = orch(store.clone(), 5, &[]);
        let mut rx = o.open_breaker_channel();
        o.reconcile_breaker();
        assert!(
            rx.try_recv().is_err(),
            "three findings rounds never reach a five-round limit"
        );
        assert!(
            store
                .load_breaker_crossings()
                .expect("crossings")
                .is_empty()
        );
    }

    /// An approval at the current head suppresses the Rounds hold: five findings rounds have counted,
    /// but the pull request converged, so no `rhapsody:human` label is written (STUDIO-1440).
    ///
    /// Mutation: drop the `!approved_at_head` guard and the crossing applies the hold.
    #[tokio::test]
    async fn an_approval_at_head_suppresses_the_rounds_hold() {
        let store = store();
        seed_watch_status(&store, "STUDIO-988", REVIEW_STATUS_APPROVED);
        for r in ["alice", "bob", "carol", "dan", "erin"] {
            seed_review_round(
                &store,
                &format!("pr:makewhatis/rhapsody#12@{r}"),
                REVIEW_VERDICT_CHANGES_REQUESTED,
            );
        }
        // The reviewer's live row stands approved at the head the watcher observes.
        seed_review_round_at(
            &store,
            "pr:makewhatis/rhapsody#12@alice",
            REVIEW_VERDICT_APPROVED,
            "2026-09-20T00:05:00Z",
        );
        let mut o = orch(store.clone(), 5, &[]);
        observe_head(&mut o, "sha");
        let mut rx = o.open_breaker_channel();
        o.reconcile_breaker();
        assert!(
            rx.try_recv().is_err(),
            "a converged pull request must not be held"
        );
        // And no actual label write, the way the acceptance states it: drive anything the pass
        // queued through the real hold sink.
        let tracker = Arc::new(rhapsody_tracker::fake::Fake::new());
        let hold_tracker = tracker.clone();
        let deps = BreakerDeps {
            hold: Some(Arc::new(TrackerHoldSink::new(move || {
                Some(hold_tracker.clone() as Arc<dyn Tracker>)
            }))),
            room: None,
            channels: Vec::new(),
        };
        while let Ok(plan) = rx.try_recv() {
            perform_crossing(&plan, &deps, Utc::now()).await;
        }
        assert!(
            tracker.add_label_calls().is_empty(),
            "an approval at the current head must write no rhapsody:human label"
        );
        assert!(
            store
                .load_breaker_crossings()
                .expect("crossings")
                .is_empty(),
            "no crossing is persisted for an approved head"
        );
    }

    /// A pending SIBLING must not defeat an approval at head (PR #368 round 1, B1). Five findings
    /// rounds, then a LATER approving round for alice, with alice's live row `approved` at the head
    /// she read — while carol's row still owes a round (`requested`) and has no completed verdict.
    /// The most recent COMPLETED verdict at the current head is alice's approval, so nothing crosses
    /// and no `rhapsody:human` label is written.
    ///
    /// Mutation: require every live row to approve (`mine.iter().all(..)`) and carol's pending row
    /// arms the hold again, writing the label.
    #[tokio::test]
    async fn a_pending_sibling_does_not_defeat_an_approval_at_head() {
        let store = store();
        for at in [
            "2026-09-20T00:00:00Z",
            "2026-09-20T00:01:00Z",
            "2026-09-20T00:02:00Z",
            "2026-09-20T00:03:00Z",
            "2026-09-20T00:04:00Z",
        ] {
            seed_review_round_at(
                &store,
                "pr:makewhatis/rhapsody#12@alice",
                REVIEW_VERDICT_CHANGES_REQUESTED,
                at,
            );
        }
        seed_review_round_at(
            &store,
            "pr:makewhatis/rhapsody#12@alice",
            REVIEW_VERDICT_APPROVED,
            "2026-09-20T00:05:00Z",
        );
        seed_watch_row(&store, "STUDIO-988", "alice", REVIEW_STATUS_APPROVED);
        seed_watch_row(&store, "STUDIO-988", "carol", REVIEW_STATUS_REQUESTED);
        assert_eq!(
            store
                .count_changes_requested_review_runs("makewhatis", "rhapsody", 12)
                .expect("count"),
            5,
            "five findings rounds are counted"
        );
        let mut o = orch(store.clone(), 5, &[]);
        observe_head(&mut o, "sha");
        let mut rx = o.open_breaker_channel();
        o.reconcile_breaker();
        // Perform anything the pass queued, against a real tracker sink: the assertion is that NO
        // label write happens, which is exactly what B1 reported.
        let tracker = Arc::new(rhapsody_tracker::fake::Fake::new());
        let hold_tracker = tracker.clone();
        let deps = BreakerDeps {
            hold: Some(Arc::new(TrackerHoldSink::new(move || {
                Some(hold_tracker.clone() as Arc<dyn Tracker>)
            }))),
            room: None,
            channels: Vec::new(),
        };
        while let Ok(plan) = rx.try_recv() {
            perform_crossing(&plan, &deps, Utc::now()).await;
        }
        assert!(
            tracker.add_label_calls().is_empty(),
            "a pending sibling must not defeat the approval at head and write rhapsody:human"
        );
        assert!(
            store
                .load_breaker_crossings()
                .expect("crossings")
                .is_empty(),
            "no crossing is persisted for an approved head"
        );
    }

    /// A review that completed at a SUPERSEDED head must not shadow an approval at the current head
    /// (PR #368 round 2, B2). Five findings rounds; carol is reviewing `old-sha` in flight when alice
    /// approves the current head `sha`; carol then completes her findings against `old-sha` — the
    /// head pinned at her dispatch (F-SHA) — and the watcher re-arms her. The per-run verdict
    /// ledger's newest verdict is carol's old-head findings, but the most recent completed verdict
    /// at the CURRENT head is alice's approval, so nothing crosses and no `rhapsody:human` label is
    /// written.
    ///
    /// Mutation: read the PR-wide newest verdict from the ledger (drop the head scoping) and carol's
    /// late old-head completion arms the hold, writing the label.
    #[tokio::test]
    async fn a_late_old_head_findings_verdict_does_not_shadow_a_current_head_approval() {
        let store = store();
        seed_run(&store, "STUDIO-988", OUTCOME_COMPLETED, 100, "anthropic");
        for _ in 0..5 {
            seed_review_round(
                &store,
                "pr:makewhatis/rhapsody#12@alice",
                REVIEW_VERDICT_CHANGES_REQUESTED,
            );
        }
        seed_watch_row(&store, "STUDIO-988", "alice", REVIEW_STATUS_APPROVED);
        seed_watch_row(&store, "STUDIO-988", "carol", REVIEW_STATUS_REQUESTED);
        // Carol is dispatched against `old-sha` and is still running when the head moves on.
        let run = crate::review::ReviewRun {
            owner: "makewhatis".into(),
            repo: "rhapsody".into(),
            number: 12,
            reviewer: "carol".into(),
            head_sha: "old-sha".into(),
            generation: 1,
            introduced_by: "handoff:STUDIO-988".into(),
            ..Default::default()
        };
        let id = store
            .start_run(RunStart {
                issue_identifier: run.key(),
                started_at: "2026-09-20T00:04:00Z".into(),
                ..Default::default()
            })
            .expect("start old-head review");
        store
            .mark_review_requested(&run.watch_key(), "old-sha")
            .expect("dispatch carol at old head");
        // Alice approves the CURRENT head.
        seed_review_round_at(
            &store,
            "pr:makewhatis/rhapsody#12@alice",
            REVIEW_VERDICT_APPROVED,
            "2026-09-20T00:05:00Z",
        );
        let mut o = orch(store.clone(), 5, &[]);
        observe_head(&mut o, "sha");
        let re = crate::orchestrator::RunningEntry {
            issue: rhapsody_core::Issue {
                id: run.key(),
                identifier: run.key(),
                ..Default::default()
            },
            run_id: id,
            review: Some(run.clone()),
            ..crate::testsupport::running_entry(
                rhapsody_core::Issue::default(),
                "rhapsody",
                "rhapsody",
            )
        };
        o.running.insert(run.key(), re.clone());
        let pr = crate::prstate::PrCoord::new("makewhatis", "rhapsody", 12);
        // The real head-advance handler skips an in-flight row.
        o.handle_review_head_advanced(&pr, "sha", &[]);
        assert_eq!(
            store
                .get_review_watch(&run.watch_key())
                .expect("watch")
                .expect("carol")
                .status,
            rhapsody_store::REVIEW_STATUS_IN_FLIGHT
        );
        o.running.remove(&run.key());
        // The real completion writes the head pinned at dispatch, not the now-current head.
        o.on_review_exit(
            &re,
            &run,
            &crate::retry::EvWorkerExit {
                declared_handoff: true,
                last_state: "findings".into(),
                issue_id: run.key(),
                started_at: Utc::now(),
                failed: false,
                err_msg: String::new(),
                review_verdict: None,
                verdict_text: None,
                refused: false,
                onepassword_hold: None,
                auth_needed: false,
            },
        );
        let carol = store
            .get_review_watch(&run.watch_key())
            .expect("watch")
            .expect("carol");
        assert_eq!(carol.status, REVIEW_STATUS_REVIEWED);
        assert_eq!(carol.last_reviewed_sha, "old-sha");
        let alice_key = ReviewWatchKey {
            reviewer: "alice".into(),
            ..run.watch_key()
        };
        let alice = store
            .get_review_watch(&alice_key)
            .expect("watch")
            .expect("alice");
        assert_eq!(alice.status, REVIEW_STATUS_APPROVED);
        assert_eq!(alice.last_reviewed_sha, "sha");
        let mut rx = o.open_breaker_channel();
        let tracker = Arc::new(rhapsody_tracker::fake::Fake::new());
        let hold_tracker = tracker.clone();
        let deps = BreakerDeps {
            hold: Some(Arc::new(TrackerHoldSink::new(move || {
                Some(hold_tracker.clone() as Arc<dyn Tracker>)
            }))),
            room: None,
            channels: Vec::new(),
        };
        // (a) BEFORE the watcher re-arms carol: her findings row is TERMINAL, but at the OLD head.
        // The HEAD scoping — not her status — is what must keep it from shadowing the approval.
        o.reconcile_breaker();
        while let Ok(plan) = rx.try_recv() {
            perform_crossing(&plan, &deps, Utc::now()).await;
        }
        assert!(
            tracker.add_label_calls().is_empty(),
            "a terminal old-head findings verdict must not shadow the current-head approval; wrote \
             {:?}",
            tracker.add_label_calls()
        );
        // A subsequent watcher sweep re-arms carol, but cannot change the per-run verdict ledger.
        o.handle_review_head_advanced(&pr, "sha", &[]);
        assert_eq!(
            store
                .get_review_watch(&run.watch_key())
                .expect("watch")
                .expect("carol")
                .status,
            REVIEW_STATUS_REQUESTED
        );
        assert_eq!(
            store
                .count_changes_requested_review_runs("makewhatis", "rhapsody", 12)
                .expect("count"),
            6,
            "the five findings rounds and carol's late old-head findings are all counted"
        );
        // (b) The re-armed state suppresses too.
        o.reconcile_breaker();
        while let Ok(plan) = rx.try_recv() {
            perform_crossing(&plan, &deps, Utc::now()).await;
        }
        assert!(
            tracker.add_label_calls().is_empty(),
            "a re-armed sibling must not shadow the current-head approval; wrote {:?}",
            tracker.add_label_calls()
        );
        assert!(
            store
                .load_breaker_crossings()
                .expect("crossings")
                .is_empty(),
            "no crossing is persisted for an approved head"
        );
    }

    /// An older SAME-head findings verdict must not defeat a NEWER approval at that head (PR #368
    /// round 3, B3). Two reviewers complete different verdicts at the SAME current head, in order:
    /// carol asks for changes (the fifth counted round), then alice approves — both at `sha`, neither
    /// superseded. The most recent COMPLETED verdict at the head is alice's approval, so nothing
    /// crosses and no `rhapsody:human` label is written. (The stricter all-reviewer merge gate may
    /// still refuse to merge; the Rounds hold is not that gate.)
    ///
    /// Mutation: require EVERY head-scoped verdict to approve (`at_head_verdicts.iter().all(..)`)
    /// and carol's older findings arm the hold again, writing the label.
    #[tokio::test]
    async fn a_newer_approval_at_head_beats_an_older_sibling_findings_verdict() {
        let store = store();
        for _ in 0..4 {
            seed_review_round(
                &store,
                "pr:makewhatis/rhapsody#12@alice",
                REVIEW_VERDICT_CHANGES_REQUESTED,
            );
        }
        seed_watch_row(&store, "STUDIO-988", "alice", REVIEW_STATUS_REQUESTED);
        seed_watch_row(&store, "STUDIO-988", "carol", REVIEW_STATUS_REQUESTED);
        let mut o = orch(store.clone(), 5, &[]);
        let config = o.teams.as_mut().expect("teams");
        config.review.reviewers = 2;
        config.roster.push(Identity {
            name: "carol".into(),
            ..Default::default()
        });
        observe_head(&mut o, "sha");
        // Real production exits, in order: carol asks for changes at `sha` (the fifth counted
        // round), then alice approves the SAME head.
        let mut ended = Vec::new();
        for (reviewer, last_state) in [
            ("carol", crate::review::REVIEW_STATE_FINDINGS),
            ("alice", crate::review::REVIEW_STATE_APPROVED),
        ] {
            let run = crate::review::ReviewRun {
                owner: "makewhatis".into(),
                repo: "rhapsody".into(),
                number: 12,
                reviewer: reviewer.into(),
                head_sha: "sha".into(),
                generation: 1,
                introduced_by: "handoff:STUDIO-988".into(),
                ..Default::default()
            };
            let id = store
                .start_run(RunStart {
                    issue_identifier: run.key(),
                    started_at: "2026-09-20T00:04:00Z".into(),
                    ..Default::default()
                })
                .expect("start review");
            store
                .mark_review_requested(&run.watch_key(), "sha")
                .expect("dispatch review");
            let re = crate::orchestrator::RunningEntry {
                issue: rhapsody_core::Issue {
                    id: run.key(),
                    identifier: run.key(),
                    ..Default::default()
                },
                run_id: id,
                review: Some(run.clone()),
                ..crate::testsupport::running_entry(
                    rhapsody_core::Issue::default(),
                    "rhapsody",
                    "rhapsody",
                )
            };
            o.on_review_exit(
                &re,
                &run,
                &crate::retry::EvWorkerExit {
                    declared_handoff: true,
                    last_state: last_state.into(),
                    issue_id: run.key(),
                    started_at: Utc::now(),
                    failed: false,
                    err_msg: String::new(),
                    review_verdict: None,
                    verdict_text: None,
                    refused: false,
                    onepassword_hold: None,
                    auth_needed: false,
                },
            );
            ended.push(id);
        }
        // Both completions are at `sha`; alice's is the NEWER by completion order.
        let carol_run = store.get_run(ended[0]).expect("get").expect("carol run");
        let alice_run = store.get_run(ended[1]).expect("get").expect("alice run");
        assert_eq!(carol_run.outcome, OUTCOME_COMPLETED);
        assert_eq!(alice_run.outcome, OUTCOME_COMPLETED);
        assert!(
            alice_run.id > carol_run.id,
            "alice's approving round completed after carol's findings round"
        );
        assert_eq!(
            store
                .count_changes_requested_review_runs("makewhatis", "rhapsody", 12)
                .expect("count"),
            5,
            "carol's findings is the fifth counted round"
        );
        let rows = store.load_live_review_watch().expect("rows");
        assert!(rows.iter().any(|r| r.key.reviewer == "carol"
            && r.status == REVIEW_STATUS_REVIEWED
            && r.last_reviewed_sha == "sha"));
        assert!(rows.iter().any(|r| r.key.reviewer == "alice"
            && r.status == REVIEW_STATUS_APPROVED
            && r.last_reviewed_sha == "sha"));
        // A head advance at the SAME head leaves both terminal rows where they are.
        let pr = crate::prstate::PrCoord::new("makewhatis", "rhapsody", 12);
        o.handle_review_head_advanced(&pr, "sha", &[]);
        let mut rx = o.open_breaker_channel();
        let tracker = Arc::new(rhapsody_tracker::fake::Fake::new());
        let hold_tracker = tracker.clone();
        let deps = BreakerDeps {
            hold: Some(Arc::new(TrackerHoldSink::new(move || {
                Some(hold_tracker.clone() as Arc<dyn Tracker>)
            }))),
            room: None,
            channels: Vec::new(),
        };
        o.reconcile_breaker();
        while let Ok(plan) = rx.try_recv() {
            perform_crossing(&plan, &deps, Utc::now()).await;
        }
        assert!(
            tracker.add_label_calls().is_empty(),
            "the newest completed CURRENT-head verdict approves, but an older sibling's findings \
             wrote {:?}",
            tracker.add_label_calls()
        );
        assert!(
            store
                .load_breaker_crossings()
                .expect("crossings")
                .is_empty(),
            "no crossing is persisted for an approved head"
        );
    }

    /// Completion ORDER within one wall-clock second (STUDIO-1440 round 4): two reviewers can
    /// complete different verdicts at the SAME head inside a single second, and `ended_at` is
    /// truncated to seconds while `runs.id` is allocated at run START — so the OLDER findings round
    /// must not be named the newest completed verdict merely because its run started later. The
    /// suppression ranks by the durable completion sequence, so the approval that COMPLETED last
    /// wins and no `rhapsody:human` label is written.
    ///
    /// Mutation: order the ledger by `ended_at DESC, id DESC` instead of the completion sequence and
    /// carol's older findings (higher start id) shadows alice's approval, writing the label.
    #[tokio::test]
    async fn a_same_second_completion_orders_by_completion_not_start() {
        let store = store();
        for _ in 0..4 {
            seed_review_round(
                &store,
                "pr:makewhatis/rhapsody#12@alice",
                REVIEW_VERDICT_CHANGES_REQUESTED,
            );
        }
        seed_watch_row(&store, "STUDIO-988", "alice", REVIEW_STATUS_REQUESTED);
        seed_watch_row(&store, "STUDIO-988", "carol", REVIEW_STATUS_REQUESTED);
        let mut o = orch(store.clone(), 5, &[]);
        let config = o.teams.as_mut().expect("teams");
        config.review.reviewers = 2;
        config.roster.push(Identity {
            name: "carol".into(),
            ..Default::default()
        });
        observe_head(&mut o, "sha");
        // Pin the clock so BOTH completions persist the SAME `ended_at` second — the case that used
        // to fall through to start order.
        o.now = Box::new(|| {
            chrono::DateTime::from_timestamp(1_760_000_000, 0).expect("a fixed, in-range instant")
        });
        // Start ALICE first (lower run id), then CAROL.
        let mut runs = Vec::new();
        for reviewer in ["alice", "carol"] {
            let run = crate::review::ReviewRun {
                owner: "makewhatis".into(),
                repo: "rhapsody".into(),
                number: 12,
                reviewer: reviewer.into(),
                head_sha: "sha".into(),
                generation: 1,
                introduced_by: "handoff:STUDIO-988".into(),
                ..Default::default()
            };
            let id = store
                .start_run(RunStart {
                    issue_identifier: run.key(),
                    started_at: "2026-09-20T00:04:00Z".into(),
                    ..Default::default()
                })
                .expect("start review");
            store
                .mark_review_requested(&run.watch_key(), "sha")
                .expect("dispatch review");
            runs.push((run, id));
        }
        // Complete CAROL (findings — the fifth counted round) FIRST, then ALICE (approval): the
        // reverse of their start order, inside one second.
        for (index, last_state) in [
            (1usize, crate::review::REVIEW_STATE_FINDINGS),
            (0usize, crate::review::REVIEW_STATE_APPROVED),
        ] {
            let (run, id) = runs[index].clone();
            let re = crate::orchestrator::RunningEntry {
                issue: rhapsody_core::Issue {
                    id: run.key(),
                    identifier: run.key(),
                    ..Default::default()
                },
                run_id: id,
                review: Some(run.clone()),
                ..crate::testsupport::running_entry(
                    rhapsody_core::Issue::default(),
                    "rhapsody",
                    "rhapsody",
                )
            };
            o.on_review_exit(
                &re,
                &run,
                &crate::retry::EvWorkerExit {
                    declared_handoff: true,
                    last_state: last_state.into(),
                    issue_id: run.key(),
                    started_at: Utc::now(),
                    failed: false,
                    err_msg: String::new(),
                    review_verdict: None,
                    verdict_text: None,
                    refused: false,
                    onepassword_hold: None,
                    auth_needed: false,
                },
            );
        }
        let alice_run = store.get_run(runs[0].1).expect("get").expect("alice run");
        let carol_run = store.get_run(runs[1].1).expect("get").expect("carol run");
        assert!(
            alice_run.id < carol_run.id,
            "alice's approving round started first, so it carries the lower run id"
        );
        assert_eq!(
            alice_run.ended_at, carol_run.ended_at,
            "both rounds completed inside the same second"
        );
        assert_eq!(carol_run.outcome, OUTCOME_COMPLETED);
        assert_eq!(alice_run.outcome, OUTCOME_COMPLETED);
        assert_eq!(
            store
                .count_changes_requested_review_runs("makewhatis", "rhapsody", 12)
                .expect("count"),
            5,
            "carol's findings is the fifth counted round"
        );
        let rows = store.load_live_review_watch().expect("rows");
        assert!(rows.iter().any(|r| r.key.reviewer == "carol"
            && r.status == REVIEW_STATUS_REVIEWED
            && r.last_reviewed_sha == "sha"));
        assert!(rows.iter().any(|r| r.key.reviewer == "alice"
            && r.status == REVIEW_STATUS_APPROVED
            && r.last_reviewed_sha == "sha"));
        let mut rx = o.open_breaker_channel();
        let tracker = Arc::new(rhapsody_tracker::fake::Fake::new());
        let hold_tracker = tracker.clone();
        let deps = BreakerDeps {
            hold: Some(Arc::new(TrackerHoldSink::new(move || {
                Some(hold_tracker.clone() as Arc<dyn Tracker>)
            }))),
            room: None,
            channels: Vec::new(),
        };
        o.reconcile_breaker();
        while let Ok(plan) = rx.try_recv() {
            perform_crossing(&plan, &deps, Utc::now()).await;
        }
        assert!(
            tracker.add_label_calls().is_empty(),
            "the newest completed CURRENT-head verdict approves, but a same-second older sibling's \
             findings wrote {:?}",
            tracker.add_label_calls()
        );
        assert!(
            store
                .load_breaker_crossings()
                .expect("crossings")
                .is_empty(),
            "no crossing is persisted for an approved head"
        );
    }

    #[tokio::test]
    async fn breaker_crossing_holds_with_lead_enabled() {
        let store = store();
        seed_watch(&store, "STUDIO-1123");
        for _ in 0..5 {
            seed_review_round(
                &store,
                "pr:makewhatis/rhapsody#12@alice",
                REVIEW_VERDICT_CHANGES_REQUESTED,
            );
        }
        let mut o = orch(store.clone(), 5, &[]);
        o.teams.as_mut().expect("teams").manager.lead.enabled = true;
        let mut rx = o.open_breaker_channel();
        o.reconcile_breaker();
        let items = store.load_lead_items().expect("items");
        assert_eq!(items.len(), 1, "the production crossing queues lead work");
        assert_eq!(items[0].subject, "STUDIO-1123");
        assert_eq!(
            items[0].trigger,
            rhapsody_store::LeadTrigger::BreakerHold {
                ticket: "STUDIO-1123".into(),
                pr: "makewhatis/rhapsody#12".into(),
                kinds: vec!["review rounds".into()],
            }
        );
        let plan = rx.try_recv().expect("lead must never replace the hold");
        assert!(crossing_applies_hold(&plan.kinds));
        let held = Arc::new(RecordingHold(Mutex::new(Vec::new())));
        perform_crossing(
            &plan,
            &BreakerDeps {
                hold: Some(held.clone()),
                room: None,
                channels: Vec::new(),
            },
            Utc::now(),
        )
        .await;
        assert_eq!(
            held.0.lock().unwrap().len(),
            1,
            "the production crossing applies the remote hold too"
        );
        assert!(o.subject_parked("STUDIO-1123", "author"));
        assert_eq!(
            store.load_breaker_crossings().unwrap()[0].notified_rounds,
            5
        );
        o.reconcile_breaker();
        let mut restarted = orch(store.clone(), 5, &[]);
        restarted
            .teams
            .as_mut()
            .expect("teams")
            .manager
            .lead
            .enabled = true;
        restarted.reconcile_breaker();
        assert_eq!(store.load_lead_items().expect("items"), items);
    }

    #[test]
    fn lead_breaker_queues_all_crossed_kinds_without_a_notification_task() {
        let store = store();
        seed_watch(&store, "STUDIO-1123");
        for _ in 0..5 {
            seed_review_round(
                &store,
                "pr:makewhatis/rhapsody#12@alice",
                REVIEW_VERDICT_CHANGES_REQUESTED,
            );
        }
        let mut o = orch(store.clone(), 5, &[("anthropic", 40)]);
        o.teams.as_mut().expect("teams").manager.lead.enabled = true;
        o.reconcile_breaker();
        assert_eq!(
            store.load_lead_items().expect("items")[0].trigger,
            rhapsody_store::LeadTrigger::BreakerHold {
                ticket: "STUDIO-1123".into(),
                pr: "makewhatis/rhapsody#12".into(),
                kinds: vec!["review rounds".into(), "per-ticket spend".into()],
            }
        );
    }

    #[test]
    fn enqueue_failure_still_holds() {
        let dir = crate::testsupport::TempDir::new();
        let path = std::path::PathBuf::from(dir.child("queue-failure.db"));
        let store: SharedStore = Arc::new(Sqlite::open(StorePath::Disk(path.clone())).unwrap());
        seed_watch(&store, "STUDIO-1156");
        seed_review_round(
            &store,
            "pr:makewhatis/rhapsody#12@alice",
            REVIEW_VERDICT_CHANGES_REQUESTED,
        );
        let mut o = orch(store.clone(), 1, &[]);
        o.teams.as_mut().unwrap().manager.lead.enabled = true;
        rusqlite::Connection::open(&path).unwrap().execute_batch(
            "CREATE TRIGGER refuse_lead_enqueue BEFORE INSERT ON rhapsody_lead_items BEGIN SELECT RAISE(ABORT, 'injected queue failure'); END;"
        ).unwrap();
        let mut rx = o.open_breaker_channel();
        o.reconcile_breaker();
        assert!(crossing_applies_hold(&rx.try_recv().unwrap().kinds));
        assert!(o.subject_parked("STUDIO-1156", "author"));
        assert!(o.subject_parked("makewhatis/rhapsody#12", "review"));
        assert!(
            store.load_lead_items().unwrap().is_empty(),
            "queue really refused the enqueue"
        );
        assert_eq!(store.load_breaker_crossings().unwrap().len(), 1);
    }

    #[test]
    fn review_clear_resets_breaker_rounds_and_counts_only_later_completions() {
        let store = store();
        seed_watch(&store, "STUDIO-1123");
        for _ in 0..7 {
            seed_review_round(
                &store,
                "pr:makewhatis/rhapsody#12@alice",
                REVIEW_VERDICT_CHANGES_REQUESTED,
            );
        }
        let in_flight = store
            .start_run(RunStart {
                issue_identifier: "pr:makewhatis/rhapsody#12@bob".into(),
                ..Default::default()
            })
            .expect("in flight");
        let mut o = orch(store.clone(), 5, &[]);
        let mut rx = o.open_breaker_channel();
        o.reconcile_breaker();
        assert_eq!(rx.try_recv().expect("first hold").rounds, 7);
        let pr = crate::prstate::PrCoord::new("MakeWhatIs", "Rhapsody", 12);
        assert_eq!(
            o.handle_review_clear(&pr),
            crate::reviewconsole::ReviewControlOutcome::Applied(1)
        );
        assert_eq!(
            store
                .count_changes_requested_review_runs("makewhatis", "rhapsody", 12)
                .expect("rounds"),
            0
        );
        assert_eq!(
            store.load_breaker_crossings().expect("crossings")[0].notified_rounds,
            0
        );
        // A fresh control task proves the reset is durable, not an in-memory refund.
        let mut o = orch(store.clone(), 5, &[]);
        let mut rx = o.open_breaker_channel();
        o.reconcile_breaker();
        assert!(rx.try_recv().is_err());
        store
            .end_run(
                in_flight,
                RunEnd {
                    outcome: OUTCOME_COMPLETED.into(),
                    ..Default::default()
                },
            )
            .expect("complete after clear");
        // A run that was in flight at clear and completes afterwards still counts, and it counts
        // only once it carries a changes-requested verdict.
        store
            .set_review_verdict(in_flight, REVIEW_VERDICT_CHANGES_REQUESTED)
            .expect("verdict");
        for _ in 0..3 {
            seed_review_round(
                &store,
                "pr:makewhatis/rhapsody#12@alice",
                REVIEW_VERDICT_CHANGES_REQUESTED,
            );
        }
        // Lifetime count is 11 here: the former next lifetime multiple must NOT hold.
        o.reconcile_breaker();
        assert!(
            rx.try_recv().is_err(),
            "four post-clear completions stay below five"
        );
        seed_run(&store, "pr:makewhatis/rhapsody#12@alice", "failed", 10, "");
        o.reconcile_breaker();
        assert!(rx.try_recv().is_err(), "failed reviews are not rounds");
        seed_review_round(
            &store,
            "pr:makewhatis/rhapsody#12@alice",
            REVIEW_VERDICT_CHANGES_REQUESTED,
        );
        o.reconcile_breaker();
        assert_eq!(rx.try_recv().expect("new threshold").rounds, 5);
        assert_eq!(
            store.load_breaker_crossings().expect("crossings")[0].notified_rounds,
            5
        );
        assert_eq!(
            o.handle_review_clear(&pr),
            crate::reviewconsole::ReviewControlOutcome::Applied(1)
        );
        assert_eq!(
            store
                .count_changes_requested_review_runs("makewhatis", "rhapsody", 12)
                .expect("rounds"),
            0
        );
    }

    /// The persisted crossing suppresses the SAME crossing on the next pass — the restart property.
    #[test]
    fn a_crossing_is_not_re_planned_while_it_stays_held() {
        let store = store();
        seed_watch(&store, "STUDIO-988");
        for r in ["alice", "bob", "carol", "dan", "erin"] {
            seed_review_round(
                &store,
                &format!("pr:makewhatis/rhapsody#12@{r}"),
                REVIEW_VERDICT_CHANGES_REQUESTED,
            );
        }
        let mut o = orch(store.clone(), 5, &[]);
        let _rx = o.open_breaker_channel();
        o.reconcile_breaker();
        let persisted = store.load_breaker_crossings().expect("crossings");
        assert_eq!(persisted.len(), 1);

        assert!(
            o.plan_crossing("STUDIO-988", "makewhatis", "rhapsody", 12, &persisted)
                .is_none(),
            "the same crossing must not be planned again while the count stays under the next \
             multiple"
        );
        // A second reconcile over the same store writes nothing new and sends nothing.
        let mut o2 = orch(store.clone(), 5, &[]);
        let mut rx2 = o2.open_breaker_channel();
        o2.reconcile_breaker();
        assert!(rx2.try_recv().is_err(), "no second notification");
    }

    /// A persisted crossing is not re-planned while the count stays under the next multiple; a
    /// restart (a fresh orchestrator over the same store) therefore does not notify again.
    ///
    /// Mutation: ignore the persisted row and the same crossing is planned again.
    #[test]
    fn a_persisted_crossing_does_not_re_notify_after_a_restart() {
        let store = store();
        seed_watch(&store, "STUDIO-988");
        for r in ["alice", "bob", "carol", "dan", "erin"] {
            seed_review_round(
                &store,
                &format!("pr:makewhatis/rhapsody#12@{r}"),
                REVIEW_VERDICT_CHANGES_REQUESTED,
            );
        }
        let mut o = orch(store.clone(), 5, &[]);
        let _keep = o.open_breaker_channel();
        o.reconcile_breaker();
        assert_eq!(store.load_breaker_crossings().expect("crossings").len(), 1);

        // A fresh daemon over the SAME store: no tx, so nothing sends, but the planner is what
        // matters. Drive it directly.
        let mut o2 = orch(store.clone(), 5, &[]);
        let _rx2 = o2.open_breaker_channel();
        let persisted = store.load_breaker_crossings().expect("crossings");
        assert!(
            o2.plan_crossing("STUDIO-988", "makewhatis", "rhapsody", 12, &persisted)
                .is_none(),
            "the same crossing must not be planned again after a restart"
        );
        // The NEXT multiple can notify again (round ten after five).
        for r in ["f", "g", "h", "i", "j"] {
            seed_review_round(
                &store,
                &format!("pr:makewhatis/rhapsody#12@{r}"),
                REVIEW_VERDICT_CHANGES_REQUESTED,
            );
        }
        let planned = o2
            .plan_crossing("STUDIO-988", "makewhatis", "rhapsody", 12, &persisted)
            .expect("round ten crosses again");
        assert_eq!(planned.plan.rounds, 10);
        assert_eq!(planned.new_notified_rounds, 10);
    }

    /// A per-ticket cap crossing holds the ticket even below the round limit, and the plan reports
    /// the constrained provider.
    #[test]
    fn a_per_ticket_spend_cap_crosses_below_the_round_limit() {
        let store = store();
        seed_watch(&store, "STUDIO-988");
        seed_run(&store, "STUDIO-988", OUTCOME_COMPLETED, 40_000, "anthropic");
        seed_review_round(
            &store,
            "pr:makewhatis/rhapsody#12@alice",
            REVIEW_VERDICT_CHANGES_REQUESTED,
        );
        let o = orch(store.clone(), 5, &[("anthropic", 30_000)]);
        let persisted = Vec::new();
        let planned = o
            .plan_crossing("STUDIO-988", "makewhatis", "rhapsody", 12, &persisted)
            .expect("spend crosses even with one round");
        assert_eq!(planned.plan.kinds, vec![CrossingKind::Spend]);
        assert_eq!(planned.plan.rounds, 1);
        assert!(planned.new_notified_providers.contains(&"anthropic".into()));
        let line = planned
            .plan
            .spend
            .iter()
            .find(|s| s.provider == "anthropic")
            .expect("anthropic reported");
        assert_eq!(line.spent, 40_010);
        assert_eq!(line.cap, Some(30_000));
    }

    /// Nothing configured ⇒ nothing planned, no store write, and the pass is a no-op — the
    /// behaviour-preservation property the ticket demands.
    #[test]
    fn with_nothing_configured_no_crossing_is_planned() {
        let store = store();
        seed_watch(&store, "STUDIO-988");
        for r in ["alice", "bob", "carol", "dan", "erin", "f", "g"] {
            seed_run(
                &store,
                &format!("pr:makewhatis/rhapsody#12@{r}"),
                OUTCOME_COMPLETED,
                10,
                "anthropic",
            );
        }
        let mut o = orch(store.clone(), 0, &[]); // hold_after 0 = off, no caps
        assert!(!o.breaker_limits_configured());
        let _rx = o.open_breaker_channel();
        o.reconcile_breaker();
        assert!(
            store
                .load_breaker_crossings()
                .expect("crossings")
                .is_empty(),
            "an unconfigured daemon writes no crossing"
        );
    }

    /// The crossing body carries no summon token (it must not reopen the author) and names the PR.
    #[test]
    fn the_notification_body_is_tokenless_and_names_the_pr() {
        let plan = BreakerPlan {
            kinds: vec![CrossingKind::Rounds],
            ticket: "STUDIO-988".into(),
            issue_id: "iss".into(),
            team_id: "team".into(),
            owner: "makewhatis".into(),
            repo: "rhapsody".into(),
            number: 218,
            rounds: 5,
            threshold: 5,
            attempts: 14,
            spend: vec![ProviderSpend {
                provider: "anthropic".into(),
                spent: 259_000_000,
                cap: None,
            }],
            latest_review: "reviewed".into(),
            reason: String::new(),
        };
        let body = crossing_body(&plan);
        assert!(
            !crate::reviewnotify::summons_author(&body, "@symphony"),
            "the breaker must never summon the author: {body}"
        );
        assert!(body.contains("makewhatis/rhapsody#218"), "{body}");
        assert!(body.contains("STUDIO-988"), "{body}");
        assert!(body.contains("**5**"), "names the rounds: {body}");
        assert!(
            body.contains("round(s) that asked for changes"),
            "the Rounds line must name the counted rounds as changes-requested, not every \
             completed review run: {body}"
        );
        assert!(body.contains("14"), "names the attempts: {body}");
        assert!(body.contains("259.0M"), "names the spend: {body}");
    }

    /// A crossing body never reports an attempt count it does not have: a zero is omitted (a
    /// hard-coded `0` in the escalation path was the review's blocking finding). Mutation: print
    /// the line unconditionally and this reds.
    #[test]
    fn the_notification_body_omits_an_unknown_attempt_count() {
        let mut plan = violation_plan();
        plan.attempts = 0;
        let body = crossing_body(&plan);
        assert!(
            !body.contains("Author attempts"),
            "an unknown attempts count must be omitted: {body}"
        );
        plan.attempts = 3;
        assert!(crossing_body(&plan).contains("Author attempts: **3**."));
    }

    /// A spend crossing names the spend that HAPPENED **and** the cap it crossed — never the cap
    /// alone. Mutation: report `min(spent, cap)` and the overshoot disappears from the body.
    #[test]
    fn the_notification_body_names_the_real_spend_beside_the_cap() {
        let plan = BreakerPlan {
            kinds: vec![CrossingKind::Spend],
            ticket: "STUDIO-984".into(),
            issue_id: "iss".into(),
            team_id: "team".into(),
            owner: "makewhatis".into(),
            repo: "rhapsody".into(),
            number: 222,
            rounds: 1,
            threshold: 0,
            attempts: 6,
            spend: vec![ProviderSpend {
                provider: "anthropic".into(),
                spent: 40_000_000,
                cap: Some(30_000_000),
            }],
            latest_review: String::new(),
            reason: String::new(),
        };
        let body = crossing_body(&plan);
        assert!(body.contains("anthropic 40.0M"), "the real spend: {body}");
        assert!(body.contains("cap 30.0M"), "the cap it crossed: {body}");
    }

    // ── the off-loop perform ─────────────────────────────────────────────────────────────────────

    #[derive(Default)]
    struct RecordingHold(Mutex<Vec<String>>);

    #[async_trait]
    impl BreakerHoldSink for RecordingHold {
        async fn hold(&self, ticket: &str, issue_id: &str, team_id: &str) {
            self.0
                .lock()
                .expect("lock")
                .push(format!("{ticket}|{issue_id}|{team_id}"));
        }
    }

    #[derive(Default)]
    struct RecordingRoom(Mutex<Vec<Message>>);

    impl RoomLog for RecordingRoom {
        fn append(&self, msg: &Message) -> Result<String, RoomError> {
            self.0.lock().expect("lock").push(msg.clone());
            Ok("room:1".into())
        }
        fn read_since(&self, _: &str, _: &Cursor, _: usize) -> Result<CaughtUp, RoomError> {
            Err(RoomError::Invalid("unused".into()))
        }
        fn read_forward(&self, _: &str, _: &Cursor, _: usize) -> Result<CaughtUp, RoomError> {
            Err(RoomError::Invalid("unused".into()))
        }
    }

    #[derive(Default)]
    struct RecordingChannel(Mutex<Vec<String>>);

    #[async_trait]
    impl NotifyChannel for RecordingChannel {
        async fn send(&self, _plan: &BreakerPlan, body: &str) -> Result<(), String> {
            self.0.lock().expect("lock").push(body.to_string());
            Ok(())
        }
        fn name(&self) -> &'static str {
            "recording"
        }
    }

    struct FailingChannel;

    #[async_trait]
    impl NotifyChannel for FailingChannel {
        async fn send(&self, _plan: &BreakerPlan, _body: &str) -> Result<(), String> {
            Err("down".into())
        }
        fn name(&self) -> &'static str {
            "failing"
        }
    }

    fn violation_plan() -> BreakerPlan {
        BreakerPlan {
            kinds: vec![CrossingKind::Rounds],
            ticket: "STUDIO-988".into(),
            issue_id: "iss".into(),
            team_id: "team".into(),
            owner: "makewhatis".into(),
            repo: "rhapsody".into(),
            number: 218,
            rounds: 5,
            threshold: 5,
            attempts: 14,
            spend: Vec::new(),
            latest_review: String::new(),
            reason: String::new(),
        }
    }

    /// perform_crossing applies the hold, appends one room line and sends to every channel — and a
    /// channel that refuses does not stop the others.
    #[tokio::test]
    async fn performing_a_crossing_holds_posts_and_notifies_on_every_channel() {
        let hold = Arc::new(RecordingHold::default());
        let room = Arc::new(RecordingRoom::default());
        let ok = Arc::new(RecordingChannel::default());
        let deps = BreakerDeps {
            hold: Some(Arc::clone(&hold) as Arc<dyn BreakerHoldSink>),
            room: Some(Arc::clone(&room) as Arc<dyn RoomLog>),
            channels: vec![
                Arc::new(FailingChannel) as Arc<dyn NotifyChannel>,
                Arc::clone(&ok) as Arc<dyn NotifyChannel>,
            ],
        };
        perform_crossing(&violation_plan(), &deps, Utc::now()).await;
        assert_eq!(
            hold.0.lock().expect("lock").as_slice(),
            ["STUDIO-988|iss|team".to_string()],
            "the hold is applied first"
        );
        assert_eq!(room.0.lock().expect("lock").len(), 1);
        assert_eq!(
            ok.0.lock().expect("lock").len(),
            1,
            "a failing channel must not suppress a healthy one"
        );
    }

    /// An escalation notifies but does NOT hold the ticket: the loop is already stopped by the
    /// adjudication, and the ticket's ask is the notification.
    #[tokio::test]
    async fn an_escalation_notifies_without_holding_the_ticket() {
        let hold = Arc::new(RecordingHold::default());
        let channel = Arc::new(RecordingChannel::default());
        let deps = BreakerDeps {
            hold: Some(Arc::clone(&hold) as Arc<dyn BreakerHoldSink>),
            room: None,
            channels: vec![Arc::clone(&channel) as Arc<dyn NotifyChannel>],
        };
        let mut plan = violation_plan();
        plan.kinds = vec![CrossingKind::Escalation];
        perform_crossing(&plan, &deps, Utc::now()).await;
        assert!(
            hold.0.lock().expect("lock").is_empty(),
            "an escalation must not apply the human hold"
        );
        let sent = channel.0.lock().expect("lock");
        assert_eq!(sent.len(), 1);
        // The delivered body must not claim a hold that was never applied: no other channel and no
        // room line can be checked here, so this pins the shared body.
        assert!(
            !sent[0].contains("rhapsody:human"),
            "an escalation must not tell the operator a hold exists: {}",
            sent[0]
        );
        assert!(
            !sent[0].contains("Remove the label"),
            "an escalation must not ask the operator to remove a label: {}",
            sent[0]
        );
        assert!(
            sent[0].contains("escalated the review loop"),
            "an escalation needs its own headline: {}",
            sent[0]
        );
        // STUDIO-1504: the escalation says what to do — the decision wanted and where to answer.
        assert!(
            sent[0].contains("Linear"),
            "an escalation must say where to answer: {}",
            sent[0]
        );
    }

    /// The macOS channel pushes onto the shared cell, and `NotificationsState` bounds its queue.
    #[tokio::test]
    async fn the_macos_channel_fills_the_shared_notifications_cell() {
        let state = Arc::new(NotificationsState::default());
        let channel = MacosChannel::new(Arc::clone(&state));
        channel
            .send(&violation_plan(), "body")
            .await
            .expect("macos send");
        let pending = state.pending();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].ticket, "STUDIO-988");
        assert_eq!(pending[0].pr, "makewhatis/rhapsody#218");
        assert!(pending[0].body.contains("body"));
    }

    /// A manager escalation rides the same channels once per (PR, head), and only when a channel is
    /// configured.
    #[test]
    fn a_manager_escalation_notifies_once_per_head_and_only_when_configured() {
        let store = store();
        let mut o = orch(store, 0, &[]);
        let mut rx = o.open_breaker_channel();
        let div = || crate::reviewreconcile::Divergence {
            pr: "makewhatis/rhapsody#218".into(),
            kind: crate::reviewreconcile::DivergenceKind::ReviewEscalated,
            ticket: "STUDIO-988".into(),
            reviewer: String::new(),
            stale_secs: 0,
            auto_merge_reason: None,
            capacity_held: None,
            capacity_unreadable: None,
            adjudicated_head: "abc123".into(),
            current_head: String::new(),
            rounds: 4,
            findings: vec!["finding one".into()],
            reason: "the loop is not converging".into(),
        };
        // With NO channel configured, nothing is sent even though the tx exists.
        o.notify_escalation(&div());
        assert!(
            rx.try_recv().is_err(),
            "an escalation with no notify channel must send nothing"
        );

        // With a channel configured, the escalation is sent once and deduped per (PR, head).
        o.eff.as_mut().expect("eff").cfg.notify.macos = true;
        o.notify_escalation(&div());
        let plan = rx.try_recv().expect("one escalation notification");
        assert_eq!(plan.kinds, vec![CrossingKind::Escalation]);
        assert_eq!(plan.reason, "the loop is not converging");
        assert_eq!(plan.latest_review, "finding one");
        o.notify_escalation(&div());
        assert!(rx.try_recv().is_err(), "deduped per (PR, head)");
    }

    /// `parse_pr` is the coordinate parser the escalation notify uses.
    #[test]
    fn parse_pr_splits_a_coordinate_and_refuses_a_malformed_one() {
        assert_eq!(
            parse_pr("makewhatis/rhapsody#218"),
            Some(("makewhatis".into(), "rhapsody".into(), 218))
        );
        assert_eq!(parse_pr("nope"), None);
        assert_eq!(parse_pr("a/b#x"), None);
    }
}
