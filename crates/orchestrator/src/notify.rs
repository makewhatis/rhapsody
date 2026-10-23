//! notify — the one-way operator push (STUDIO-1504).
//!
//! **Notification is one-way; communication is bidirectional.** Everything in this module is
//! fire-and-forget: a `send`, no identity, no reply surface, no receipt. The bidirectional axis —
//! an agent identity and a reply that reaches a run — is a separate design and deliberately does
//! not leak into this one.
//!
//! # Why this exists
//!
//! Escalation *content* already existed and was good, but nothing delivered it: the room is
//! pull-only by design, and the reconciliation sweep's advisories surface on an API endpoint
//! nobody polls. On 2026-10-22/23 four correct stops sat between 1.5 and 10 hours because no push
//! ever reached a human. This module is the push.
//!
//! # What is sent, and what is not
//!
//! [`NotifyKind`] is a CLOSED enum, so widening what reaches a phone requires editing it and shows
//! in a diff. Only four events warrant a human:
//!
//! * [`NotifyKind::Escalation`] — a manager escalation that names a human (sent by the breaker's
//!   own path, [`crate::breaker`], which already carries the room line).
//! * [`NotifyKind::AdviceUnapplied`] — an `advise`-mode recommendation the daemon deliberately did
//!   not apply, so nothing else will act on it.
//! * [`NotifyKind::BlockedNoPr`] — a run that ended blocked with no pull request (STUDIO-1497): it
//!   has no reply surface and nothing will move it.
//! * [`NotifyKind::HumanHold`] — a ticket held by the `rhapsody:human` refusal, the first time it
//!   becomes a candidate (once per ticket, never per tick).
//!
//! Never sent: review rounds, rebases, CI churn, routine dispatch, or anything that resolves
//! itself. A notifier that cries wolf gets muted, and a muted notifier is worse than none.
//! [`divergence_notify_kind`] is the pure rule, and its test walks every
//! [`crate::reviewreconcile::DivergenceKind`] to pin the silence.
//!
//! # Delivery, and why it never fails the work
//!
//! The control task only RESERVES the dedup marker and enqueues an owned [`Notification`] on an
//! unbounded channel — no `.await`, no HTTP, no tracker. [`run_notify_task`] does the sending off
//! the control task, best-effort, with every failure logged where it happens. A notification
//! failure therefore never fails or delays the work that produced it.
//!
//! # Dedup
//!
//! Once per ticket per occurrence. The per-process half is [`Orchestrator::notify_sent`]; the
//! restart half is the durable [`rhapsody_store::Store::reserve_lead_report`] keyed
//! `notify:<kind>:<occurrence>`. A store that keeps no durable dedup
//! ([`rhapsody_store::Store::durable_dedup_enabled`] is false) uses the in-memory half alone.

use std::sync::Arc;

use crate::Orchestrator;
use crate::breaker::{
    MacosChannel, NotificationsState, NotifyChannel, NtfyChannel, WebhookChannel,
};
use rhapsody_config::Notify;
use rhapsody_config::notify::NotifyOrigin;

/// How many distinct occurrences [`Orchestrator::notify_sent`] remembers before it forgets the
/// oldest set wholesale — the [`crate::dispatch::HumanHoldLedger`] idiom, so a long-running daemon's
/// in-memory set cannot grow without limit. The cost of hitting it is one repeated notification;
/// the durable ledger still suppresses a repeat across a restart wherever storage is on.
const NOTIFY_SENT_CAPACITY: usize = 256;

/// The one-way notification kinds this daemon sends (STUDIO-1504). Closed on purpose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyKind {
    /// A manager escalation that names a human.
    Escalation,
    /// An `advise`-mode recommendation that was NOT applied.
    AdviceUnapplied,
    /// A run that ended blocked with no pull request.
    BlockedNoPr,
    /// A ticket held by the `rhapsody:human` refusal.
    HumanHold,
}

impl NotifyKind {
    /// The stable token used in the dedup key and the logs. Stable because an operator greps it.
    pub fn as_str(self) -> &'static str {
        match self {
            NotifyKind::Escalation => "escalation",
            NotifyKind::AdviceUnapplied => "advice-unapplied",
            NotifyKind::BlockedNoPr => "blocked-no-pr",
            NotifyKind::HumanHold => "human-hold",
        }
    }

    /// The notification title, always naming the ticket.
    pub fn title(self, ticket: &str) -> String {
        match self {
            NotifyKind::Escalation => format!("Escalation: {ticket}"),
            NotifyKind::AdviceUnapplied => format!("Advice not applied: {ticket}"),
            NotifyKind::BlockedNoPr => format!("Blocked, no pull request: {ticket}"),
            NotifyKind::HumanHold => format!("Held for a human: {ticket}"),
        }
    }
}

/// One notification to deliver, owned, on its way off the control task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    pub kind: NotifyKind,
    /// The origin ticket identifier, e.g. `STUDIO-1486`.
    pub ticket: String,
    pub title: String,
    pub body: String,
    /// A URL a phone can tap, or empty. Never a summon token or anything the agent could read as
    /// one — this is a message to the operator, not to a run.
    pub link: String,
    /// The dedup key (`notify:<kind>:<occurrence>`), logged for diagnosis.
    pub dedup_key: String,
}

/// The "say what to do" tail every notification carries: the decision wanted and where to answer.
/// After 1/5 the answer surface is a comment on the Linear ticket, reachable from a phone.
pub fn answer_hint(ticket: &str) -> String {
    format!(
        "What to do: answer with a comment on {ticket} in Linear — a reply from your phone reaches \
         the ticket."
    )
}

/// The GitHub URL for an `owner/repo#number` coordinate, or `None` when it is not that shape (a
/// `console:` row, or an empty coordinate). The link a phone taps.
pub fn pr_url_from_coordinate(pr: &str) -> Option<String> {
    let (repo_part, number) = pr.rsplit_once('#')?;
    let (owner, repo) = repo_part.split_once('/')?;
    if owner.is_empty() || repo.is_empty() || number.parse::<i64>().is_err() {
        return None;
    }
    Some(format!("https://github.com/{owner}/{repo}/pull/{number}"))
}

/// The notification a divergence warrants, or `None` for the never-send list (STUDIO-1504).
///
/// [`DivergenceKind::ReviewEscalated`] is deliberately absent: the breaker path
/// ([`crate::breaker::Orchestrator::notify_escalation`]) already notifies it, carrying the room
/// line the escalation owns, so routing it here too would page twice. Every other kind — a normal
/// review round, a rebase, an approval, a budget stall, a recovery — is silent.
pub fn divergence_notify_kind(kind: crate::reviewreconcile::DivergenceKind) -> Option<NotifyKind> {
    use crate::reviewreconcile::DivergenceKind as K;
    match kind {
        K::AdviceUnapplied => Some(NotifyKind::AdviceUnapplied),
        _ => None,
    }
}

/// Everything the off-loop notification task needs. No `Orchestrator`, no store, no control
/// channel — the off-loop guarantee, in the type, as [`crate::breaker::BreakerDeps`] states it.
pub struct NotifyDeps {
    /// Every configured notification backend, already resolved from `notify:`.
    pub channels: Vec<Arc<dyn NotifyChannel>>,
}

/// Delivers one notification through every channel, independently: one refusing backend never
/// suppresses another, and nothing here can fail the caller (it already returned).
pub async fn perform_notify(notification: &Notification, channels: &[Arc<dyn NotifyChannel>]) {
    if channels.is_empty() {
        tracing::warn!(
            ticket = %notification.ticket,
            kind = notification.kind.as_str(),
            "notify: an event warranted a human but no notification backend is configured, so no \
             push was delivered"
        );
        return;
    }
    for channel in channels {
        let sent = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            channel.send_notice(&notification.title, &notification.body, &notification.link),
        )
        .await
        .unwrap_or_else(|_| Err("notification timed out".into()));
        match sent {
            Ok(()) => tracing::warn!(
                ticket = %notification.ticket,
                kind = notification.kind.as_str(),
                channel = channel.name(),
                "notify: delivered an operator notification"
            ),
            Err(error) => tracing::warn!(
                ticket = %notification.ticket,
                kind = notification.kind.as_str(),
                channel = channel.name(),
                %error,
                "notify: a backend refused the notification; other backends are unaffected"
            ),
        }
    }
}

/// Consumes [`Notification`]s until `ctx` is cancelled or every sender is dropped.
pub async fn run_notify_task(
    mut ctx: crate::control_loop::CancelWait,
    deps: NotifyDeps,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<Notification>,
) {
    tracing::info!("notification task started (off-loop; a tick never waits on a push)");
    loop {
        let notification = tokio::select! {
            _ = ctx.cancelled() => return,
            r = rx.recv() => match r {
                Some(n) => n,
                None => return,
            },
        };
        perform_notify(&notification, &deps.channels).await;
    }
}

/// Builds the notification backends from the `notify:` block (STUDIO-1504).
///
/// The named backend resolves the way `harness:` and `memory.backend` do: an explicit
/// `notify.backend` wins, else the legacy channel keys are inherited (ntfy, then macOS, then
/// webhook), else `none`. The resolved value and its origin are logged once at boot — the
/// introspection the ticket asks for.
///
/// **An explicit backend selects exactly that one.** A recognized-but-unimplemented name
/// (`email`, `pager`) and an unknown name both WARN and deliver through nothing, rather than
/// failing the event that triggered the notification. **No explicit backend preserves the legacy
/// additive behaviour**: every configured channel is built, so an installation that sets both
/// `macos` and `ntfy` (as the shipped `WORKFLOW.md` does) keeps both.
pub fn channels_from_config(
    cfg: &Notify,
    state: Arc<NotificationsState>,
) -> Vec<Arc<dyn NotifyChannel>> {
    let resolved = cfg.resolve_backend();
    tracing::info!(
        backend = %resolved.backend,
        origin = resolved.origin.as_str(),
        "notify: resolved notification backend"
    );
    if resolved.origin == NotifyOrigin::Explicit {
        return explicit_channels(&resolved.backend, cfg, state);
    }
    // No explicit backend: inherit the legacy keys, additively, exactly as before STUDIO-1504.
    let mut channels: Vec<Arc<dyn NotifyChannel>> = Vec::new();
    if cfg.macos {
        channels.push(Arc::new(MacosChannel::new(state)));
    }
    if !cfg.webhook.trim().is_empty() {
        channels.push(Arc::new(WebhookChannel::new(cfg.webhook.trim().into())));
    }
    if !cfg.ntfy.trim().is_empty() {
        channels.push(Arc::new(NtfyChannel::new(cfg.ntfy.trim().into())));
    }
    channels
}

/// The one channel an explicit `notify.backend` selects, or none — with a warning for every reason
/// a name cannot be delivered through.
fn explicit_channels(
    name: &str,
    cfg: &Notify,
    state: Arc<NotificationsState>,
) -> Vec<Arc<dyn NotifyChannel>> {
    if !rhapsody_config::notify::notify_backend_is_recognized(name) {
        tracing::warn!(
            backend = name,
            known = rhapsody_config::notify::NOTIFY_BACKEND_NAMES.join(", "),
            "notify: `notify.backend` names an unknown backend; no notification will be sent"
        );
        return Vec::new();
    }
    if !rhapsody_config::notify::notify_backend_is_implemented(name) {
        tracing::warn!(
            backend = name,
            "notify: `notify.backend` names a recognized backend this build does not implement; \
             no notification will be sent"
        );
        return Vec::new();
    }
    match name {
        "none" => Vec::new(),
        "macos" => vec![Arc::new(MacosChannel::new(state))],
        "webhook" if !cfg.webhook.trim().is_empty() => {
            vec![Arc::new(WebhookChannel::new(cfg.webhook.trim().into()))]
        }
        "ntfy" if !cfg.ntfy.trim().is_empty() => {
            vec![Arc::new(NtfyChannel::new(cfg.ntfy.trim().into()))]
        }
        // Implemented, but its own setting is empty: say so rather than deliver nothing silently.
        "webhook" | "ntfy" => {
            tracing::warn!(
                backend = name,
                "notify: `notify.backend` selected a backend whose setting is empty, so no \
                 notification will be sent"
            );
            Vec::new()
        }
        // Unreachable while IMPLEMENTED_NOTIFY_BACKENDS and this match agree; a warning, not a
        // panic, keeps a future edit from crashing a tick.
        other => {
            tracing::warn!(
                backend = other,
                "notify: implemented backend has no channel arm; no notification will be sent"
            );
            Vec::new()
        }
    }
}

impl Orchestrator {
    /// Opens the notification task's channel, storing the sender and handing back the receiver for
    /// [`run_notify_task`]. Mirrors [`Orchestrator::open_breaker_channel`].
    pub fn open_notify_channel(&mut self) -> tokio::sync::mpsc::UnboundedReceiver<Notification> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.notify_tx = Some(tx);
        rx
    }
    /// Whether any notification backend is configured. The triggers gate on this (and on a task
    /// existing) so a daemon with no channels does nothing new.
    pub(crate) fn notify_backend_configured(&self) -> bool {
        self.eff.as_ref().is_some_and(|e| {
            let n = &e.cfg.notify;
            if !n.backend.trim().is_empty() {
                let r = n.resolve_backend();
                r.is_implemented() && r.backend != "none"
            } else {
                n.macos || !n.webhook.trim().is_empty() || !n.ntfy.trim().is_empty()
            }
        })
    }

    /// Fires the notification a reported divergence warrants, if any (STUDIO-1504). A no-op for
    /// the never-send list. Called from the reconciliation sweep on the control task; the send
    /// itself happens off it.
    ///
    /// The body says what happened, the head and round count the manager stopped at, and what to do
    /// — the ticket, the decision wanted and where to answer. Deduped per `(pull request, head)`,
    /// so a stable escalation pages once and a new head pages again.
    pub(crate) fn notify_divergence(&self, d: &crate::reviewreconcile::Divergence) -> bool {
        let Some(kind) = divergence_notify_kind(d.kind) else {
            return false;
        };
        let body = format!(
            "**{ticket}** — {detail}.\nHead {head}, {rounds} round(s). {reason}\n\n{answer}",
            ticket = d.ticket,
            detail = d.kind.detail(),
            head = if d.adjudicated_head.is_empty() {
                "unknown"
            } else {
                d.adjudicated_head.as_str()
            },
            rounds = d.rounds,
            reason = if d.reason.trim().is_empty() {
                "The manager did not state a reason."
            } else {
                d.reason.trim()
            },
            answer = answer_hint(&d.ticket),
        );
        let link = pr_url_from_coordinate(&d.pr).unwrap_or_default();
        self.request_notify(
            kind,
            &d.ticket,
            &format!("{}@{}", d.pr, d.adjudicated_head),
            body,
            link,
        )
    }

    /// Fires the notification for a run that ended blocked with no pull request (STUDIO-1497,
    /// STUDIO-1504). Deduped per hold id, so one blocked episode pages once and a later re-park
    /// pages again.
    pub(crate) fn notify_blocked_no_pr(&self, ticket: &str, hold_id: i64, need: &str) -> bool {
        let body = format!(
            "**{ticket}** — a run ended blocked with no pull request, so there is nothing to review \
             and nothing will move it on its own.\n{need}\n\n{answer}",
            answer = answer_hint(ticket),
        );
        self.request_notify(
            NotifyKind::BlockedNoPr,
            ticket,
            &format!("hold:{hold_id}"),
            body,
            String::new(),
        )
    }

    /// Fires the notification for a ticket held by the `rhapsody:human` refusal, the first time it
    /// becomes a candidate (STUDIO-1504). Deduped per ticket: once per ticket, never per tick. The
    /// caller has already deduped the per-process half through
    /// [`crate::dispatch::HumanHoldLedger::hold`]; this adds the durable restart half.
    pub(crate) fn notify_human_hold(&self, ticket: &str) -> bool {
        let body = format!(
            "**{ticket}** wears the `rhapsody:human` hold, so the daemon will not dispatch it. Only \
             a person can move this ticket.\n\n{answer}",
            answer = answer_hint(ticket),
        );
        self.request_notify(NotifyKind::HumanHold, ticket, ticket, body, String::new())
    }

    /// Reserves the once-per-occurrence marker and enqueues a notification.
    ///
    /// `occurrence` scopes the dedup to one trip (a pull request head, a hold id, a ticket). The
    /// per-process half is [`Self::notify_sent`]; the restart half is the store's
    /// `reserve_lead_report` ledger. Returns whether the notification was enqueued, so a caller can
    /// assert it in a test.
    ///
    /// Best-effort by contract: a gone task or an unreadable store logs and returns, never failing
    /// the work that produced the event.
    pub(crate) fn request_notify(
        &self,
        kind: NotifyKind,
        ticket: &str,
        occurrence: &str,
        body: String,
        link: String,
    ) -> bool {
        if !self.notify_backend_configured() {
            return false;
        }
        // Resolve the task FIRST: with no task there is nothing to deliver, and reserving a dedup
        // marker for an undelivered notification would silently swallow the occurrence.
        let Some(tx) = self.notify_tx.as_ref() else {
            return false;
        };
        let key = format!("notify:{}:{}", kind.as_str(), occurrence);
        // Per-process half: the once-per-ticket logging idiom, and the ONLY dedup when storage is
        // off. Bounded wholesale-clear, exactly as `HumanHoldLedger`'s announced set is, so a
        // long-running daemon's set cannot grow without limit.
        {
            let mut seen = self
                .notify_sent
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if seen.len() >= NOTIFY_SENT_CAPACITY && !seen.contains(&key) {
                seen.clear();
            }
            if !seen.insert(key.clone()) {
                return false;
            }
        }
        // Restart half: a durable ledger, when the store keeps one.
        if self.store().durable_dedup_enabled() {
            match self.store().reserve_lead_report(&key, 1) {
                Ok(true) => {}
                Ok(false) => return false, // already notified before a restart
                Err(error) => {
                    tracing::warn!(
                        ticket,
                        kind = kind.as_str(),
                        %error,
                        "notify: the dedup marker could not be persisted; skipping rather than \
                         risking a repeat"
                    );
                    return false;
                }
            }
        }
        let notification = Notification {
            kind,
            ticket: ticket.to_string(),
            title: kind.title(ticket),
            body,
            link,
            dedup_key: key,
        };
        if tx.send(notification).is_err() {
            tracing::warn!(
                ticket,
                kind = kind.as_str(),
                "notify: the notification task is gone; the event was not delivered"
            );
            return false;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reviewreconcile::DivergenceKind;
    use crate::testsupport::empty_effective;
    use rhapsody_store::{Sqlite, Store, StorePath};
    use rhapsody_tracker::fake::Fake;

    type SharedStore = Arc<dyn Store + Send + Sync>;

    fn store() -> SharedStore {
        Arc::new(Sqlite::open(StorePath::InMemory).expect("open in-memory"))
    }

    /// An orchestrator over `store` with the given `notify:` config, ready to enqueue.
    fn orch_with(store: SharedStore, notify: rhapsody_config::Notify) -> Orchestrator {
        let mut o = Orchestrator::new("WORKFLOW.md");
        let mut eff = empty_effective(Arc::new(Fake::new()));
        eff.cfg.notify = notify;
        o.eff = Some(eff);
        o.set_store(store);
        o
    }

    fn enabled() -> rhapsody_config::Notify {
        rhapsody_config::Notify {
            macos: true,
            ..rhapsody_config::Notify::default()
        }
    }

    fn divergence(kind: DivergenceKind) -> crate::reviewreconcile::Divergence {
        crate::reviewreconcile::Divergence {
            pr: "makewhatis/rhapsody#218".into(),
            kind,
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
        }
    }

    #[test]
    fn only_the_warranted_divergences_notify() {
        // The never-send list, pinned. ReviewEscalated is absent because the breaker path owns it.
        assert_eq!(
            divergence_notify_kind(DivergenceKind::AdviceUnapplied),
            Some(NotifyKind::AdviceUnapplied)
        );
        for kind in [
            DivergenceKind::ChangesRequestedNoRun,
            DivergenceKind::ReviewRequestedNoRun,
            DivergenceKind::AuthorTokenCeilingStopped,
            DivergenceKind::ReviewTokenCeilingStopped,
            DivergenceKind::ApprovedStillOpen,
            DivergenceKind::HeldForHuman,
            DivergenceKind::RoundBudgetExhausted,
            DivergenceKind::ReviewEscalated,
            DivergenceKind::ReviewShipped,
            DivergenceKind::MergedTicketNotTerminal,
            DivergenceKind::ManagerDeferred,
            DivergenceKind::ReviewInfrastructure,
            DivergenceKind::CredentialInfrastructure,
        ] {
            assert_eq!(
                divergence_notify_kind(kind),
                None,
                "{} must never page a human",
                kind.as_str()
            );
        }
    }

    // A normal review round and a rebase must produce nothing. `ChangesRequestedNoRun` is the
    // ordinary "a reviewer asked for changes" round; `ReviewRequestedNoRun` is a round owed after a
    // push. Neither warrants a human, and neither may enqueue.
    #[test]
    fn a_review_round_and_a_rebase_are_silent() {
        let mut o = orch_with(store(), enabled());
        let mut rx = o.open_notify_channel();
        for kind in [
            DivergenceKind::ChangesRequestedNoRun,
            DivergenceKind::ReviewRequestedNoRun,
        ] {
            assert!(
                !o.notify_divergence(&divergence(kind)),
                "{} must not notify",
                kind.as_str()
            );
        }
        assert!(rx.try_recv().is_err(), "the never-send list is silent");
    }

    // Each human-warranting trigger enqueues exactly one notification, and a repeat of the same
    // occurrence is deduped in-process.
    #[test]
    fn each_trigger_fires_exactly_one_notification() {
        let mut o = orch_with(store(), enabled());
        let mut rx = o.open_notify_channel();

        assert!(o.notify_divergence(&divergence(DivergenceKind::AdviceUnapplied)));
        assert!(!o.notify_divergence(&divergence(DivergenceKind::AdviceUnapplied)));

        assert!(o.notify_blocked_no_pr("STUDIO-1", 7, "the run ended with no pull request"));
        assert!(!o.notify_blocked_no_pr("STUDIO-1", 7, "the run ended with no pull request"));

        assert!(o.notify_human_hold("STUDIO-2"));
        assert!(!o.notify_human_hold("STUDIO-2"));

        let mut got = Vec::new();
        while let Ok(n) = rx.try_recv() {
            got.push(n);
        }
        assert_eq!(got.len(), 3, "one per trigger, deduped: {got:?}");
        assert_eq!(got[0].kind, NotifyKind::AdviceUnapplied);
        assert_eq!(got[1].kind, NotifyKind::BlockedNoPr);
        assert_eq!(got[2].kind, NotifyKind::HumanHold);
        // Every body names the ticket and says where to answer.
        for n in &got {
            assert!(n.body.contains(&n.ticket), "{}", n.body);
            assert!(n.body.contains("Linear"), "{}", n.body);
        }
    }

    // The dedup survives a daemon restart: a second orchestrator over the SAME durable store does
    // not re-send a trip the first already delivered. A NEW occurrence (a new hold id) still pages.
    #[test]
    fn dedup_survives_a_restart() {
        let store = store();

        let mut first = orch_with(Arc::clone(&store), enabled());
        let mut rx = first.open_notify_channel();
        assert!(first.notify_blocked_no_pr("STUDIO-9", 3, "need"));
        assert_eq!(rx.try_recv().expect("sent").kind, NotifyKind::BlockedNoPr);

        // A "restart": fresh in-memory state, same store.
        let mut second = orch_with(Arc::clone(&store), enabled());
        let mut rx2 = second.open_notify_channel();
        assert!(
            !second.notify_blocked_no_pr("STUDIO-9", 3, "need"),
            "a trip already notified must not be re-sent after a restart"
        );
        assert!(rx2.try_recv().is_err());

        // A new hold episode is a new occurrence.
        assert!(second.notify_blocked_no_pr("STUDIO-9", 4, "need"));
        assert_eq!(rx2.try_recv().expect("sent").kind, NotifyKind::BlockedNoPr);
    }

    // A recognized-but-unimplemented backend degrades: the event that triggered it still completes
    // (no panic, no error), and nothing is delivered.
    #[test]
    fn a_recognized_but_unimplemented_backend_degrades() {
        let cfg = rhapsody_config::Notify {
            backend: "email".into(),
            macos: true,
            ..rhapsody_config::Notify::default()
        };
        let mut o = orch_with(store(), cfg);
        assert!(!o.notify_backend_configured());
        let mut rx = o.open_notify_channel();
        assert!(
            !o.notify_divergence(&divergence(DivergenceKind::AdviceUnapplied)),
            "the trigger must not fail; it degrades to no delivery"
        );
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn answer_hint_names_the_ticket_and_the_surface() {
        let hint = answer_hint("STUDIO-1486");
        assert!(hint.contains("STUDIO-1486"), "{hint}");
        assert!(hint.contains("Linear"), "{hint}");
    }

    // The generic delivery path reaches a webhook (generic JSON) and the macOS cell independently —
    // a webhook-only install must not get an "unsupported" error per trigger.
    #[tokio::test]
    async fn perform_notify_reaches_webhook_and_macos_independently() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/notice", listener.local_addr().unwrap());
        let request = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = vec![0; 4096];
            let len = socket.read(&mut bytes).await.unwrap();
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
            String::from_utf8(bytes[..len].to_vec()).unwrap()
        });
        let state = Arc::new(NotificationsState::default());
        let channels: Vec<Arc<dyn NotifyChannel>> = vec![
            Arc::new(WebhookChannel::new(url)),
            Arc::new(MacosChannel::new(state.clone())),
        ];
        let notification = Notification {
            kind: NotifyKind::AdviceUnapplied,
            ticket: "STUDIO-9".into(),
            title: NotifyKind::AdviceUnapplied.title("STUDIO-9"),
            body: "body".into(),
            link: "https://example.test".into(),
            dedup_key: "notify:advice-unapplied:x".into(),
        };
        perform_notify(&notification, &channels).await;

        let wire = request.await.unwrap();
        assert!(wire.contains("\"title\""), "{wire}");
        assert!(wire.contains("STUDIO-9"), "{wire}");
        assert_eq!(
            state.pending().len(),
            1,
            "a webhook channel never suppresses the macOS push"
        );
    }

    #[test]
    fn titles_name_the_ticket() {
        for kind in [
            NotifyKind::Escalation,
            NotifyKind::AdviceUnapplied,
            NotifyKind::BlockedNoPr,
            NotifyKind::HumanHold,
        ] {
            assert!(kind.title("STUDIO-9").contains("STUDIO-9"));
        }
    }

    #[test]
    fn pr_url_from_coordinate_only_for_a_pr_shape() {
        assert_eq!(
            pr_url_from_coordinate("makewhatis/rhapsody#218").as_deref(),
            Some("https://github.com/makewhatis/rhapsody/pull/218")
        );
        assert_eq!(pr_url_from_coordinate("console:foo"), None);
        assert_eq!(pr_url_from_coordinate(""), None);
    }

    #[test]
    fn channels_from_config_degrades_a_recognized_but_unimplemented_backend() {
        let cfg = Notify {
            backend: "email".into(),
            ..Notify::default()
        };
        let channels = channels_from_config(&cfg, Arc::new(NotificationsState::default()));
        assert!(
            channels.is_empty(),
            "a recognized-but-unimplemented backend must degrade to no delivery"
        );
    }

    #[test]
    fn channels_from_config_degrades_an_unknown_backend() {
        let cfg = Notify {
            backend: "telegram".into(),
            ..Notify::default()
        };
        assert!(channels_from_config(&cfg, Arc::new(NotificationsState::default())).is_empty());
    }

    #[test]
    fn channels_from_config_builds_every_legacy_channel_when_no_backend_is_named() {
        let cfg = Notify {
            macos: true,
            ntfy: "rhapsody-alerts".into(),
            ..Notify::default()
        };
        let channels = channels_from_config(&cfg, Arc::new(NotificationsState::default()));
        let names: Vec<&str> = channels.iter().map(|c| c.name()).collect();
        assert!(
            names.contains(&"macos") && names.contains(&"ntfy"),
            "{names:?}"
        );
    }

    #[test]
    fn channels_from_config_explicit_backend_selects_one() {
        let cfg = Notify {
            backend: "ntfy".into(),
            macos: true,
            ntfy: "rhapsody-alerts".into(),
            ..Notify::default()
        };
        let channels = channels_from_config(&cfg, Arc::new(NotificationsState::default()));
        let names: Vec<&str> = channels.iter().map(|c| c.name()).collect();
        assert_eq!(
            names,
            vec!["ntfy"],
            "an explicit backend selects exactly one"
        );

        let off = Notify {
            backend: "none".into(),
            ntfy: "rhapsody-alerts".into(),
            ..Notify::default()
        };
        assert!(channels_from_config(&off, Arc::new(NotificationsState::default())).is_empty());
    }
}
