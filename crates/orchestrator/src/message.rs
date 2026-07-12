//! message — parity port of Go `internal/orchestrator/message.go` (operator messages, INF-250 +
//! the mid-run summons router, INF-448).
//!
//! An operator can push a message to a LIVE run's agent. [`Orchestrator::handle_run_message`] (the
//! loop-side admission) locates the running entry by run id, admits the message to that run's bounded
//! mailbox via [`Orchestrator::deliver_to_mailbox`] (a non-blocking send — a full or absent mailbox
//! rejects with `backlog_full`), and persists the ORIGINAL (unwrapped) text as a "sent" row. The
//! mailbox carries the WRAPPED text ([`operator_wrap`]) so the agent treats it as authoritative
//! updated guidance; the store keeps the operator's original words.
//!
//! [`Orchestrator::deliver_mid_run_summons`] reuses that one admission path for the poll-side mid-run
//! summons router (INF-448): a summons newer than a run's start (and not already delivered) is routed
//! into the live run's mailbox, closing the half of the summon dead zone where a comment posted
//! mid-run never reached the live agent.
//!
//! Deviations from the Go source, all behavior-preserving:
//!   * Go's per-run mailbox is a field on the `runningEntry` (`chan string`); a Rust
//!     [`tokio::sync::mpsc`] channel splits into a non-`Clone`/non-`Sync` receiver + a sender, so the
//!     mailbox lives in [`Orchestrator::mailboxes`] (a `Mutex<HashMap<issue_id, Mailbox>>`, keyed by
//!     issue id — one live run per issue). An entry with no mailbox (legacy/test-injected) rejects
//!     delivery, exactly as Go's nil-channel `select` hits the default branch. The receiver is handed
//!     to the worker by O7's real spawn; O6 creates the channel at dispatch and drops it at run end.
//!   * Go's `SendRunMessage` round-trips the control-event channel (`o.events`) for loop-confined
//!     admission, awaiting the reply on the lifetime ctx. That channel + the running control loop are
//!     O7's; until then [`Orchestrator::send_run_message`] performs admission directly on the calling
//!     (control) task via [`handle_run_message`](Orchestrator::handle_run_message) — already the single
//!     mutator of `running`/mailboxes. When O7 lands the event loop this becomes the channel send +
//!     reply await, with `handle_run_message` unchanged as the on-loop body.
//!   * Diagnostics log via `tracing` (as the sibling crates do) instead of a threaded `slog` logger.

use tokio::sync::mpsc;

use rhapsody_core::Issue;

use crate::orchestrator::{Orchestrator, RunningEntry};
use crate::select::TaggedIssue;

/// Bounds the per-run operator-message backlog. 16 pending messages means delivery has stalled (or
/// someone is spamming) — reject further sends rather than grow unbounded or block the control task
/// (INF-250). Mirrors Go `operatorMailboxCap`.
pub(crate) const OPERATOR_MAILBOX_CAP: usize = 16;

/// Delivered when a mid-run summons has no comment body available (a tracker source that only surfaces
/// the timestamp). Mirrors Go `midRunSummonFallback`.
const MID_RUN_SUMMON_FALLBACK: &str =
    "a human summoned you mid-run — re-read this ticket's newest comments for updated instructions";

/// A live run's operator-message mailbox: the bounded `tx` the control task delivers to and the `rx`
/// the worker drains. Keyed by issue id in [`Orchestrator::mailboxes`]. O7's real spawn takes `rx`
/// (once); O6 creates the pair at dispatch and drops it at run end. Mirrors Go's per-`runningEntry`
/// `mailbox chan string` (which a Rust `mpsc` split cannot express as one `Clone` field).
///
/// `pub` with a `pub` `rx`: the receive end must be HELD to keep the bounded channel open (a dropped
/// receiver would fail every `try_send`), yet its only readers are O7's worker spawn (which takes it)
/// and O6's `#[cfg(test)]` mailbox peek. Exposing it as the crate's mailbox API — rather than a
/// dead-code `#[allow]` — is the same ahead-of-consumer pattern the crate uses for [`crate::ghenrich`]
/// / [`Orchestrator::promote_unblocked`]. The `mailboxes` map itself stays `pub(crate)`, so no live
/// mailbox is reachable off the control task.
pub struct Mailbox {
    pub(crate) tx: mpsc::Sender<String>,
    /// The receive end, taken (once) by O7's worker spawn; O6's tests peek it.
    pub rx: Option<mpsc::Receiver<String>>,
}

impl Mailbox {
    /// Creates a bounded mailbox pair sized to [`OPERATOR_MAILBOX_CAP`]. Mirrors Go
    /// `make(chan string, operatorMailboxCap)`.
    pub(crate) fn new() -> Mailbox {
        let (tx, rx) = mpsc::channel(OPERATOR_MAILBOX_CAP);
        Mailbox { tx, rx: Some(rx) }
    }
}

/// Frames an operator message for the agent's prompt stream. The store keeps the operator's ORIGINAL
/// text; only the stdin write carries this wrapper, which tells the agent the message is
/// authoritative, updated guidance from the ticket owner. Mirrors Go `operatorWrap`.
pub(crate) fn operator_wrap(text: &str) -> String {
    format!(
        "OPERATOR MESSAGE (from a human operator monitoring this run — treat as updated instructions \
         from the ticket owner, superseding conflicting earlier guidance): {text}"
    )
}

/// The result of admitting an operator message, mirroring the Stop/Resume result style for the HTTP
/// layer. Mirrors Go `RunMessageResult`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunMessageResult {
    /// No live worker for the run id (already finished / unknown) ⇒ 409.
    pub not_running: bool,
    /// Mailbox at capacity ⇒ 409 backlog_full.
    pub full: bool,
    /// The inserted `run_messages` row id (0 if persistence disabled).
    pub id: i64,
    /// Human ticket id, e.g. `"INF-250"`.
    pub identifier: String,
}

impl Orchestrator {
    /// Queues an operator message for a live run's agent (INF-250). Admission is loop-confined (the
    /// single mutator of `running`/mailboxes). Mirrors Go `SendRunMessage` — see the module docs for
    /// the control-event-channel deviation (O7 re-routes this through `o.events`).
    pub fn send_run_message(&self, run_id: i64, text: &str) -> RunMessageResult {
        self.handle_run_message(run_id, text)
    }

    /// Admits an operator message for a live run ON the control task: locate the running entry by run
    /// id (same lookup as stop), non-blocking mailbox send, persist the row. Reply carries the row id
    /// + identifier (or `not_running` / `full`). Mirrors Go `handleRunMessage`.
    pub fn handle_run_message(&self, run_id: i64, text: &str) -> RunMessageResult {
        let id = self.issue_id_for_run(run_id);
        let re = match self.running.get(&id) {
            Some(re) => re,
            None => {
                return RunMessageResult {
                    not_running: true,
                    ..Default::default()
                };
            }
        };
        let identifier = re.issue.identifier.clone();
        let (row_id, ok) = self.deliver_to_mailbox(re, text);
        if !ok {
            return RunMessageResult {
                full: true,
                identifier,
                ..Default::default()
            };
        }
        RunMessageResult {
            id: row_id,
            identifier,
            ..Default::default()
        }
    }

    /// Admits `body` to a live run's operator mailbox: a NON-BLOCKING wrapped send (a full — or absent,
    /// for legacy/test-injected entries — mailbox rejects), then a best-effort persist of the ORIGINAL
    /// body (status "sent"; no-op row id 0 when persistence is disabled). Shared by the HTTP path
    /// ([`handle_run_message`](Orchestrator::handle_run_message)) and the poll-side mid-run summon
    /// router ([`deliver_mid_run_summons`](Orchestrator::deliver_mid_run_summons), INF-448) so there is
    /// exactly ONE admission path honoring [`OPERATOR_MAILBOX_CAP`]. Mirrors Go `deliverToMailbox`.
    pub(crate) fn deliver_to_mailbox(&self, re: &RunningEntry, body: &str) -> (i64, bool) {
        let sent = {
            let mailboxes = self
                .mailboxes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match mailboxes.get(&re.issue.id) {
                Some(mb) => mb.tx.try_send(operator_wrap(body)).is_ok(),
                None => false, // nil mailbox (legacy/test-injected) → reject
            }
        };
        if !sent {
            return (0, false);
        }
        (self.persist_run_message(re, body), true)
    }

    /// Returns the opaque issue id of the live run whose `run_id` matches, or `""` when none does
    /// (⇒ `not_running`). Run ids are unique, so at most one entry matches. Mirrors Go `issueIDForRun`
    /// (Go places it in `stop.go`; O6 is its first consumer — O7's `stop.rs` reuses it).
    pub(crate) fn issue_id_for_run(&self, run_id: i64) -> String {
        self.running
            .iter()
            .find(|(_, re)| re.run_id == run_id)
            .map(|(id, _)| id.clone())
            .unwrap_or_default()
    }

    /// Routes each candidate that has an ACTIVE run and a summons newer than that run's start (and not
    /// already delivered) into the run's operator mailbox (INF-448), closing the half of the summon
    /// dead zone where a comment posted mid-run never reached the live agent. Runs ON the control task
    /// during the poll tick, BEFORE select — which then drops the running issue as usual — and reuses
    /// [`deliver_to_mailbox`](Orchestrator::deliver_to_mailbox) (the INF-250 admission path), so
    /// [`OPERATOR_MAILBOX_CAP`] still bounds the backlog. Idempotent per run: the per-run
    /// `last_delivered_summon_at` watermark prevents re-injecting a stable summons each poll; a
    /// full-mailbox rejection leaves the watermark unadvanced so the summons is retried on a later
    /// tick. `pub` — an O7 poll-tick entry point (tested end-to-end by O8's midrun-summon scenarios).
    /// Mirrors Go `deliverMidRunSummons`.
    pub fn deliver_mid_run_summons(&mut self, issues: &[Issue]) {
        for iss in issues {
            // Decide against the live entry (immutable), extracting the owned bits the admission +
            // logging need, so the borrow ends before the mailbox send / watermark mutation.
            let Some(re) = self.running.get(&iss.id) else {
                continue;
            };
            let Some(summon_at) = iss.latest_summon_at else {
                continue;
            };
            if summon_at <= re.started_at {
                continue; // not a mid-run summons (posted before this run began)
            }
            if summon_at <= re.last_delivered_summon_at {
                continue; // already delivered this (or a newer) summons to this run
            }
            let (run_id, identifier) = (re.run_id, re.issue.identifier.clone());
            let body = if iss.latest_summon_body.is_empty() {
                MID_RUN_SUMMON_FALLBACK.to_string()
            } else {
                iss.latest_summon_body.clone()
            };
            // Re-borrow the entry for the admission (both borrows shared: `re` from `running`, `&self`
            // for the method), then advance the watermark only on a successful admit.
            let admitted = match self.running.get(&iss.id) {
                Some(re) => self.deliver_to_mailbox(re, &body).1,
                None => continue,
            };
            if admitted {
                if let Some(re) = self.running.get_mut(&iss.id) {
                    re.last_delivered_summon_at = summon_at;
                }
                tracing::info!(issue_identifier = %identifier, run_id, summon_at = %summon_at, "mid-run summons delivered to live run");
            } else {
                tracing::info!(issue_identifier = %identifier, run_id, "mid-run summons deferred: operator mailbox full; will retry next tick");
            }
        }
    }

    /// The multi-project adapter for [`deliver_mid_run_summons`](Orchestrator::deliver_mid_run_summons):
    /// the tagged candidate copy carries the enriched summons time/body from its owning project. Mirrors
    /// Go `deliverMidRunSummonsTagged`.
    pub fn deliver_mid_run_summons_tagged(&mut self, tagged: &[TaggedIssue]) {
        for ti in tagged {
            self.deliver_mid_run_summons(std::slice::from_ref(&ti.iss));
        }
    }
}

#[cfg(test)]
impl Orchestrator {
    /// Non-blocking peek of a live run's mailbox (drains one wrapped message). Test-only — the
    /// analogue of Go's `<-re.mailbox` read; O7's worker drains the same receiver in production.
    pub(crate) fn mailbox_try_recv(&self, issue_id: &str) -> Option<String> {
        let mut mailboxes = self.mailboxes.lock().expect("mailboxes lock");
        mailboxes.get_mut(issue_id)?.rx.as_mut()?.try_recv().ok()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use chrono::{DateTime, Duration, Utc};
    use rhapsody_agent::{self as agent, EVENT_OPERATOR_MESSAGE};
    use rhapsody_core::Issue;
    use rhapsody_store::{
        RUN_MESSAGE_DELIVERED, RUN_MESSAGE_EXPIRED, RUN_MESSAGE_SENT, Sqlite, Store, StorePath,
    };
    use rhapsody_tracker::fake::Fake;

    use super::*;
    use crate::agentupdate::AgentUpdate;
    use crate::orchestrator::Orchestrator;
    use crate::retry::EvWorkerExit;
    use crate::testsupport::{empty_effective, issue, set_of, utc};

    /// An orchestrator on a fresh in-memory store with a minimal eff (so `dispatch_issue`'s
    /// `persist_start_run` stamps a real run id). Returns the store so a test can read back the
    /// persisted `run_messages` rows. Mirrors the store-backed setup Go's `orchWithStore` /
    /// `newStopHarness` build (minus the O7 control loop this port drives directly).
    fn message_orch() -> (Orchestrator, Arc<dyn Store + Send + Sync>) {
        let st: Arc<dyn Store + Send + Sync> =
            Arc::new(Sqlite::open(StorePath::InMemory).expect("open store"));
        let mut eff = empty_effective(Arc::new(Fake::new()));
        eff.active_states = set_of(&["todo", "in progress"]);
        eff.terminal_states = set_of(&["done"]);
        eff.max_concurrent = 10;
        let mut o = Orchestrator::new("WORKFLOW.md");
        o.eff = Some(eff);
        o.set_store(Arc::clone(&st));
        // Inject a no-op spawn so dispatch doesn't launch the real worker (which would drain this
        // run's mailbox receiver) — these tests assert on the mailbox admission path, not the worker.
        o.spawn = Some(Box::new(|_iss, _attempt, _re| {}));
        (o, st)
    }

    // operator_wrap must render byte-identically to Go `operatorWrap` (the wrapper is written to the
    // agent's stdin, so it is parity-sensitive) — pins the multi-line format string's spacing.
    #[test]
    fn operator_wrap_matches_go() {
        // A single-line literal (NO `\`-continuation) so this is an unambiguous byte check against Go
        // `operatorWrap`'s concatenated output — a spacing slip in the impl's multi-line string fails.
        assert_eq!(
            operator_wrap("hi"),
            "OPERATOR MESSAGE (from a human operator monitoring this run — treat as updated instructions from the ticket owner, superseding conflicting earlier guidance): hi"
        );
    }

    // TestSendRunMessage_AcceptsWrapsAndPersists: a message to a live run is accepted, the WRAPPED text
    // lands on the mailbox, and a run_messages row is persisted with the ORIGINAL (unwrapped) body and
    // status "sent" (INF-250).
    #[tokio::test]
    async fn send_run_message_accepts_wraps_and_persists() {
        let (mut o, st) = message_orch();
        o.dispatch_issue(
            issue("ID-1", "MT-1", "In Progress"),
            None,
            None,
            String::new(),
        );
        let run_id = o.running.get("ID-1").expect("running").run_id;

        let res = o.send_run_message(run_id, "watch the branch");
        assert!(
            !res.not_running && !res.full,
            "unexpected rejection: {res:?}"
        );
        assert!(res.id > 0, "result id should be a persisted row id");
        assert_eq!(res.identifier, "MT-1");

        let got = o.mailbox_try_recv("ID-1").expect("mailbox had no message");
        assert!(
            got.contains("watch the branch"),
            "mailbox payload missing text: {got:?}"
        );
        assert!(
            got.contains("OPERATOR MESSAGE"),
            "mailbox payload not wrapped: {got:?}"
        );

        let msgs = st.list_run_messages(run_id).expect("list run messages");
        assert_eq!(msgs.len(), 1);
        assert_eq!(
            msgs[0].body, "watch the branch",
            "stored body must be the unwrapped text"
        );
        assert_eq!(msgs[0].status, RUN_MESSAGE_SENT);
    }

    // TestSendRunMessage_NotRunning: an unknown run id is rejected (not_running) with no row persisted.
    #[tokio::test]
    async fn send_run_message_not_running() {
        let (o, st) = message_orch();
        let res = o.send_run_message(4242, "hi");
        assert!(
            res.not_running,
            "expected not_running for an unknown run id, got {res:?}"
        );
        assert_eq!(st.list_run_messages(4242).expect("list").len(), 0);
    }

    // TestSendRunMessage_BacklogFull: with no consumer draining the mailbox, the cap-th message fills it
    // and the next is rejected (full) with no extra row persisted.
    #[tokio::test]
    async fn send_run_message_backlog_full() {
        let (mut o, st) = message_orch();
        // The mailbox is never drained, so it fills to OPERATOR_MAILBOX_CAP.
        o.dispatch_issue(
            issue("ID-1", "MT-1", "In Progress"),
            None,
            None,
            String::new(),
        );
        let run_id = o.running.get("ID-1").expect("running").run_id;

        for i in 0..OPERATOR_MAILBOX_CAP {
            let res = o.send_run_message(run_id, "m");
            assert!(!res.full, "send {i} rejected full before reaching cap");
        }
        let res = o.send_run_message(run_id, "overflow");
        assert!(
            res.full,
            "expected full on the cap+1-th pending message, got {res:?}"
        );

        let msgs = st.list_run_messages(run_id).expect("list");
        assert_eq!(
            msgs.len(),
            OPERATOR_MAILBOX_CAP,
            "the overflow message must NOT be persisted"
        );
    }

    // TestRunMessageDeliveredMarking: an EVENT_OPERATOR_MESSAGE from the runner marks the oldest "sent"
    // row delivered with its turn (FIFO).
    #[tokio::test]
    async fn run_message_delivered_marking() {
        let (mut o, st) = message_orch();
        o.dispatch_issue(
            issue("ID-1", "MT-1", "In Progress"),
            None,
            None,
            String::new(),
        );
        let run_id = o.running.get("ID-1").expect("running").run_id;

        o.send_run_message(run_id, "first");
        // The runner reports the actual stdin write for turn 2.
        o.on_agent_update(AgentUpdate {
            issue_id: "ID-1".into(),
            ev: agent::Event {
                event_type: EVENT_OPERATOR_MESSAGE.into(),
                turn: 2,
                message: "first".into(),
                ..Default::default()
            },
        });

        let msgs = st.list_run_messages(run_id).expect("list");
        assert_eq!(msgs.len(), 1);
        assert_eq!(
            msgs[0].status, RUN_MESSAGE_DELIVERED,
            "row not delivered: {:?}",
            msgs[0]
        );
        assert_eq!(msgs[0].delivered_turn, Some(2));
    }

    // TestRunMessagesExpiredAtRunEnd: messages still "sent" when the run ends are expired
    // (persist_end_run).
    #[tokio::test]
    async fn run_messages_expired_at_run_end() {
        let (mut o, st) = message_orch();
        o.dispatch_issue(
            issue("ID-1", "MT-1", "In Progress"),
            None,
            None,
            String::new(),
        );
        let (run_id, started_at) = {
            let re = o.running.get("ID-1").expect("running");
            (re.run_id, re.started_at)
        };

        o.send_run_message(run_id, "never delivered");
        // The worker exits (failed → persist_end_run(failed) → expire_run_messages).
        o.on_worker_exit(EvWorkerExit {
            issue_id: "ID-1".into(),
            failed: true,
            started_at,
            err_msg: "boom".into(),
            last_state: "In Progress".into(),
            declared_handoff: false,
        });

        let msgs = st.list_run_messages(run_id).expect("list");
        assert_eq!(msgs.len(), 1);
        assert_eq!(
            msgs[0].status, RUN_MESSAGE_EXPIRED,
            "row must expire at run end"
        );
    }

    // TestDeliverToMailbox_SendsWrapsPersists: the extracted admission helper (shared by the HTTP path
    // and the mid-run summon router) sends the WRAPPED text, persists the ORIGINAL body (status sent)
    // returning its row id, and rejects on a full mailbox with no extra row.
    #[tokio::test]
    async fn deliver_to_mailbox_sends_wraps_persists() {
        let (mut o, st) = message_orch();
        o.dispatch_issue(
            issue("ID-1", "MT-1", "In Progress"),
            None,
            None,
            String::new(),
        );
        let (run_id, re) = {
            let re = o.running.get("ID-1").expect("running");
            (re.run_id, re.clone())
        };

        let (row_id, ok) = o.deliver_to_mailbox(&re, "watch the branch");
        assert!(ok, "deliver_to_mailbox should admit on an empty mailbox");
        assert!(row_id > 0, "row_id should be a persisted row id");

        let got = o.mailbox_try_recv("ID-1").expect("mailbox had no message");
        assert!(
            got.contains("OPERATOR MESSAGE") && got.contains("watch the branch"),
            "mailbox payload must be wrapped original text: {got:?}"
        );

        let msgs = st.list_run_messages(run_id).expect("list");
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].body, "watch the branch");
        assert_eq!(msgs[0].status, RUN_MESSAGE_SENT);

        // Fill the mailbox (the read above drained the one message) → admission rejected, no extra row.
        for i in 0..OPERATOR_MAILBOX_CAP {
            assert!(
                o.deliver_to_mailbox(&re, "fill").1,
                "fill {i} rejected before cap"
            );
        }
        assert!(
            !o.deliver_to_mailbox(&re, "overflow").1,
            "a full mailbox must reject admission"
        );
        let msgs = st.list_run_messages(run_id).expect("list");
        assert_eq!(
            msgs.len(),
            OPERATOR_MAILBOX_CAP + 1,
            "overflow not persisted (1 initial + cap fills)"
        );
    }

    // --- midrun_summon_test.go: the poll-side mid-run summons router (INF-448, O8 e2e) -----------
    //
    // One live fake run (ID-1/MT-1) pinned to `base`; `deliver_mid_run_summons` routes a candidate's
    // summons into that run's mailbox exactly once per newer watermark. Reuses `message_orch` (in-memory
    // store + no-op spawn, so the mailbox receiver is never taken by a real worker) and drives the
    // router directly, exactly as the poll tick would BEFORE select drops the running issue.

    /// Dispatches one fake run for ID-1/MT-1 whose `started_at` is pinned to `base`, returning the
    /// orchestrator plus `base`. The live entry is read back via `o.running` (Go returns the `*runningEntry`
    /// pointer; a Rust borrow is re-taken per use). Mirrors Go `midrunHarness`.
    fn midrun_harness() -> (Orchestrator, DateTime<Utc>) {
        let (mut o, _st) = message_orch();
        let base = utc(2026, 6, 3, 12, 0, 0);
        o.now = Box::new(move || base);
        o.dispatch_issue(
            issue("ID-1", "MT-1", "In Progress"),
            None,
            None,
            String::new(),
        );
        let started = o.running.get("ID-1").expect("running entry").started_at;
        assert_eq!(started, base, "running entry not pinned to base");
        (o, base)
    }

    // TestMidRunSummonDeliveredOnce: a candidate with an ACTIVE run and a summons newer than the run
    // start is delivered to the run's mailbox exactly once (wrapped body + persisted "sent" row); a
    // repeat poll carrying the SAME summons is NOT re-delivered; a NEWER summons is.
    #[tokio::test]
    async fn mid_run_summon_delivered_once() {
        let (mut o, base) = midrun_harness();
        let run_id = o.running.get("ID-1").expect("running").run_id;

        let summon = base + Duration::hours(1); // posted after the run started
        let cand = Issue {
            id: "ID-1".into(),
            identifier: "MT-1".into(),
            title: "do".into(),
            state: "In Progress".into(),
            latest_summon_at: Some(summon),
            latest_summon_body: "@symphony fix the MTU".into(),
            ..Default::default()
        };

        o.deliver_mid_run_summons(std::slice::from_ref(&cand));
        let got = o
            .mailbox_try_recv("ID-1")
            .expect("expected a mid-run summons delivery");
        assert!(
            got.contains("OPERATOR MESSAGE") && got.contains("fix the MTU"),
            "mailbox payload = {got:?}, want wrapped summons body"
        );

        // Repeat poll, SAME summons → no re-delivery (dedup watermark holds).
        o.deliver_mid_run_summons(std::slice::from_ref(&cand));
        assert!(
            o.mailbox_try_recv("ID-1").is_none(),
            "summons re-delivered on repeat poll"
        );

        let msgs = o
            .store()
            .list_run_messages(run_id)
            .expect("list run messages");
        assert_eq!(
            msgs.len(),
            1,
            "persisted rows should be exactly 1 (deduped)"
        );
        assert_eq!(
            msgs[0].body, "@symphony fix the MTU",
            "persisted body must be the ORIGINAL (unwrapped) summons body"
        );

        // A NEWER summons IS delivered again.
        let mut cand2 = cand.clone();
        cand2.latest_summon_at = Some(base + Duration::hours(2));
        cand2.latest_summon_body = "@symphony also bump the timeout".into();
        o.deliver_mid_run_summons(std::slice::from_ref(&cand2));
        let got = o
            .mailbox_try_recv("ID-1")
            .expect("a newer summons must be delivered");
        assert!(
            got.contains("bump the timeout"),
            "payload = {got:?}, want the newer summons' body"
        );
    }

    // TestMidRunSummonBeforeRunStartIgnored: a summons at or before the run start is not a mid-run
    // event (the round already had it from its start), so it is not injected.
    #[tokio::test]
    async fn mid_run_summon_before_run_start_ignored() {
        let (mut o, base) = midrun_harness();
        for at in [base - Duration::hours(1), base] {
            let cand = Issue {
                id: "ID-1".into(),
                identifier: "MT-1".into(),
                state: "In Progress".into(),
                latest_summon_at: Some(at),
                latest_summon_body: "@symphony old".into(),
                ..Default::default()
            };
            o.deliver_mid_run_summons(std::slice::from_ref(&cand));
            assert!(
                o.mailbox_try_recv("ID-1").is_none(),
                "summons at {at} (not after run start {base}) must NOT be delivered"
            );
        }
    }

    // TestMidRunSummonFallbackBody: a summons whose source could not surface a body still nudges the
    // agent with the generic fallback message.
    #[tokio::test]
    async fn mid_run_summon_fallback_body() {
        let (mut o, base) = midrun_harness();
        let cand = Issue {
            id: "ID-1".into(),
            identifier: "MT-1".into(),
            state: "In Progress".into(),
            latest_summon_at: Some(base + Duration::hours(1)), // no body
            ..Default::default()
        };
        o.deliver_mid_run_summons(std::slice::from_ref(&cand));
        let got = o
            .mailbox_try_recv("ID-1")
            .expect("a body-less mid-run summons must still be delivered (fallback)");
        assert!(
            got.contains(MID_RUN_SUMMON_FALLBACK),
            "payload = {got:?}, want the generic fallback"
        );
    }

    // TestMidRunSummonNotRunningIgnored: candidates with no active run, or with no summons at all, are
    // no-ops (no panic on the nil entry lookup).
    #[tokio::test]
    async fn mid_run_summon_not_running_ignored() {
        let (mut o, _st) = message_orch();
        let summon = utc(2026, 6, 3, 13, 0, 0);
        o.deliver_mid_run_summons(&[
            Issue {
                id: "NOPE".into(),
                identifier: "MT-9".into(),
                state: "In Progress".into(),
                latest_summon_at: Some(summon),
                latest_summon_body: "@symphony hi".into(),
                ..Default::default()
            },
            Issue {
                id: "ALSO-NOPE".into(),
                identifier: "MT-10".into(),
                state: "Todo".into(),
                ..Default::default()
            }, // no summons
        ]);
        assert!(o.running.is_empty(), "no run should have been created");
    }

    // TestMidRunSummonMailboxFullRetries: a full mailbox rejects the delivery WITHOUT advancing the
    // dedup watermark, so the same summons is retried (and admitted) on a later poll once the worker has
    // drained the backlog.
    #[tokio::test]
    async fn mid_run_summon_mailbox_full_retries() {
        let (mut o, base) = midrun_harness();
        let zero = o
            .running
            .get("ID-1")
            .expect("running")
            .last_delivered_summon_at;
        let re = o.running.get("ID-1").expect("running").clone();
        for i in 0..OPERATOR_MAILBOX_CAP {
            assert!(
                o.deliver_to_mailbox(&re, "fill").1,
                "fill {i} rejected before cap"
            );
        }
        let cand = Issue {
            id: "ID-1".into(),
            identifier: "MT-1".into(),
            state: "In Progress".into(),
            latest_summon_at: Some(base + Duration::hours(1)),
            latest_summon_body: "@symphony fix the MTU".into(),
            ..Default::default()
        };
        o.deliver_mid_run_summons(std::slice::from_ref(&cand));
        assert_eq!(
            o.running
                .get("ID-1")
                .expect("running")
                .last_delivered_summon_at,
            zero,
            "a rejected (mailbox-full) delivery must not advance the watermark"
        );

        // The worker drains one slot → the next poll delivers the same summons.
        o.mailbox_try_recv("ID-1").expect("drain one slot");
        o.deliver_mid_run_summons(std::slice::from_ref(&cand));
        assert_eq!(
            o.running
                .get("ID-1")
                .expect("running")
                .last_delivered_summon_at,
            base + Duration::hours(1),
            "watermark should equal the delivered summons' time"
        );
    }

    // TestMidRunSummonTagged: the multi-project adapter routes from the tagged candidate list.
    #[tokio::test]
    async fn mid_run_summon_tagged() {
        let (mut o, base) = midrun_harness();
        let cand = Issue {
            id: "ID-1".into(),
            identifier: "MT-1".into(),
            state: "In Progress".into(),
            latest_summon_at: Some(base + Duration::hours(1)),
            latest_summon_body: "@symphony fix the MTU".into(),
            ..Default::default()
        };
        o.deliver_mid_run_summons_tagged(&[TaggedIssue {
            iss: cand,
            proj: None,
        }]);
        let got = o
            .mailbox_try_recv("ID-1")
            .expect("tagged mid-run summons must be delivered");
        assert!(got.contains("fix the MTU"), "payload = {got:?}");
    }
}
