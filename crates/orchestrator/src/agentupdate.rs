//! agentupdate — parity port of Go `internal/orchestrator/agentupdate.go` (`onAgentUpdate`).
//!
//! Folds one agent event into the running entry's live state and the aggregate token totals
//! (upstream §13.5). It is loop-confined — only the control task mutates [`Orchestrator`] state — so
//! it takes `&mut self` and touches no channels.
//!
//! Deviations from the Go source, all serial-chain deferrals (the state folding the O3 tests assert
//! is ported in full):
//!   * The store writes Go performs here — the coarse per-event history capture (`enqueueEvent`),
//!     the turn-boundary progress flush (`persistProgress`), and the operator-message delivery mark
//!     (`persistRunMessageDelivered`) — are O4's run-row persistence (`persist.go`); this module
//!     keeps the loop-confined state folding (tokens, pgid, session, `recent_events`, `event_seq`,
//!     the `cur_*` turn-boundary reset). The `event_seq` counter is still advanced so O4's history
//!     writer inherits a correct monotonic sequence.
//!   * The token metric (`o.metrics.Tokens(...)`) export is P6; the bounded label set it would carry
//!     lives in [`crate::telemetry_attrs`].
//!   * Go's `onTranscriptOpened` (same file) sets `transcript_path` then `persistProgress` onto the
//!     run row; it is a control-loop entry point delivered by O7's loop (the `evTranscriptOpened` event
//!     the worker's `on_transcript` callback posts), so it is ported here in O7.

use rhapsody_agent as agent;
use rhapsody_store as store;

use crate::orchestrator::{EventRecord, Orchestrator};

/// The bounded size of a running entry's `recent_events` ring surfaced in the API snapshot. Mirrors
/// Go `maxRecentEvents` (defined in Go's `orchestrator.go`; `onAgentUpdate` is its sole consumer, so
/// the Rust port defines it beside that consumer).
pub(crate) const MAX_RECENT_EVENTS: usize = 50;

/// One folded agent event: the issue whose running entry it updates, plus the event. Mirrors Go
/// `evAgentUpdate`; O7 wraps it as a control-event variant when the loop's event channel lands.
pub struct AgentUpdate {
    pub issue_id: String,
    pub ev: agent::Event,
}

impl Orchestrator {
    /// Records the concrete per-run transcript path on the running entry (control task) and
    /// immediately persists it so the run row points at its OWN transcript even before the first
    /// turn-boundary progress write. A no-op for an unknown issue (already terminated) or an empty
    /// path. Mirrors Go `onTranscriptOpened` (a control-loop entry point delivered by O7's loop).
    pub(crate) fn on_transcript_opened(&mut self, issue_id: &str, path: &str) {
        if path.is_empty() {
            return;
        }
        match self.running.get_mut(issue_id) {
            Some(re) => re.transcript_path = path.to_string(),
            None => return,
        }
        // Flush the concrete path onto runs.transcript_path right away (Go `persistProgress`).
        if let Some(re) = self.running.get(issue_id) {
            self.persist_progress(re);
        }
    }

    /// Folds one agent event into the running entry's live state and the aggregate token totals
    /// (upstream §13.5). A no-op for an unknown issue (already terminated). Mirrors Go `onAgentUpdate`.
    pub fn on_agent_update(&mut self, e: AgentUpdate) {
        // Operator-message delivery (INF-250) is synthesized by the runner when a message is actually
        // written to the live turn's stdin. Mark the oldest still-"sent" row delivered with its turn
        // (FIFO matches mailbox order) and return: this synthetic event must NOT be folded into the
        // agent's liveness/token/history state below (it carries no usage and is not a real agent
        // step). Handled up front via a SHARED borrow of the live entry so the delivery mark can reach
        // the store; an unknown issue is a no-op either way (Go's `!ok` guard precedes this). Mirrors
        // Go `onAgentUpdate` → `persistRunMessageDelivered`.
        if e.ev.event_type == agent::EVENT_OPERATOR_MESSAGE {
            if let Some(re) = self.running.get(&e.issue_id) {
                self.persist_run_message_delivered(re, e.ev.turn);
            }
            return;
        }
        let now = (self.now)();
        let re = match self.running.get_mut(&e.issue_id) {
            Some(re) => re,
            None => return,
        };
        re.last_event = e.ev.event_type.clone();
        re.last_event_at = now;
        if !e.ev.message.is_empty() {
            re.last_message = e.ev.message.clone();
        }

        // Track the process-group id of the CURRENT turn. Each turn is a separate `claude` process
        // (new pid, new group via Setpgid), so refresh whenever a new pid appears — not just the first
        // time — and reset the CPU baseline so the new group is measured fresh. Without this,
        // CPU-based liveness would only work on turn 1 and later turns would fall back to
        // event-silence detection.
        if e.ev.pid != 0 && e.ev.pid != i64::from(re.pgid) {
            re.pgid = e.ev.pid as i32;
            re.last_cpu_active_at = re.last_event_at;
            re.cpu_sampled = false;
            re.last_cpu_ticks = 0;
        }

        re.recent_events.push(EventRecord {
            at: re.last_event_at,
            event: e.ev.event_type.clone(),
            message: e.ev.message.clone(),
        });
        if re.recent_events.len() > MAX_RECENT_EVENTS {
            let excess = re.recent_events.len() - MAX_RECENT_EVENTS;
            re.recent_events.drain(0..excess);
        }

        if e.ev.event_type == agent::EVENT_SESSION_STARTED {
            if !e.ev.message.is_empty() {
                re.thread_id = e.ev.message.clone(); // claude session id carried in the init event
            }
            re.turn_count += 1;
            re.session_id = format!("{}-{}", re.thread_id, re.turn_count);
        }

        // Token usage has two sources, distinguished by event type, to keep committed totals
        // authoritative while still updating the dashboard mid-turn (upstream §13.5). Every Usage
        // carries the BILLED TotalTokens, so committed/live totals are billed-inclusive while
        // In/Out stay the uncached breakdown:
        //
        //   - result events (turn_completed/turn_failed) carry the AUTHORITATIVE per-turn total.
        //     Commit it: SUM across turns (each turn is a fresh process whose result is that turn's
        //     own total, so summing is correct). The live estimate (cur_*) is reset at the turn
        //     boundary below — decoupled from whether usage was reported.
        //   - assistant notifications carry the LIVE in-flight estimate. Within a turn the assistant
        //     message.usage is CUMULATIVE-within-the-turn, so the LATEST snapshot already IS the
        //     current turn total. Use LAST-WINS assignment (not +=); summing the K growing snapshots
        //     would massively over-count. Do NOT touch committed totals (the result event commits the
        //     real per-turn total, so committing here too would double-count).
        if let Some(u) = e.ev.usage {
            match e.ev.event_type.as_str() {
                agent::EVENT_TURN_COMPLETED | agent::EVENT_TURN_FAILED => {
                    re.input_tokens += u.input_tokens;
                    re.output_tokens += u.output_tokens;
                    re.total_tokens += u.total_tokens;
                    self.totals.input_tokens += u.input_tokens;
                    self.totals.output_tokens += u.output_tokens;
                    self.totals.total_tokens += u.total_tokens;
                    // Go emits `o.metrics.Tokens(...)` here; the token metric export is P6.
                }
                _ => {
                    re.cur_input_tokens = u.input_tokens;
                    re.cur_output_tokens = u.output_tokens;
                    re.cur_total_tokens = u.total_tokens;
                }
            }
        }

        // Phase 4: capture a coarse history event (async, non-blocking — this runs ON the control
        // task, so `enqueue_event` never blocks) and, on turn boundaries, write per-turn progress
        // synchronously. Both no-op when `re.run_id == 0` (store disabled). O3 advanced the monotonic
        // per-run `event_seq` and left these two store writes for O4 (`persist.rs`); they land here.
        re.event_seq += 1;
        let event_row = store::EventRow {
            seq: re.event_seq,
            at: crate::persist::rfc3339(re.last_event_at),
            kind: crate::persist::map_kind(&e.ev),
            tool: crate::persist::map_tool(&e.ev),
            text: crate::persist::map_text(&e.ev),
        };
        let run_id = re.run_id;
        let turn_boundary = e.ev.event_type == agent::EVENT_TURN_COMPLETED
            || e.ev.event_type == agent::EVENT_TURN_FAILED;
        if turn_boundary && e.ev.usage.is_some() {
            // Reset the live per-call estimate ONLY when this terminal event committed an
            // authoritative result usage (u.is_some() above). The committed total then supersedes the
            // in-flight estimate, so a continuation turn never displays a previous turn's stale cur_*.
            //
            // When the terminal event carried NO usage (the timeout turn_failed — runner.rs),
            // PRESERVE cur_*: it is the only token signal for this turn, and O4's `persist_end_run`
            // uses it as a floor so the run isn't recorded as 0 (INF-208). This cannot leak into a
            // later turn on the same entry: runTurns returns on any non-result turn, so a no-usage
            // terminal event always ends the run — there is no next turn here.
            re.cur_input_tokens = 0;
            re.cur_output_tokens = 0;
            re.cur_total_tokens = 0;
        }
        // The `&mut re` borrow ends here; the `&self` store writes follow (a re-borrow for progress).
        self.enqueue_event(run_id, event_row);
        if turn_boundary && let Some(re) = self.running.get(&e.issue_id) {
            self.persist_progress(re);
        }
        // STUDIO-967: enforce the per-run token ceiling on the freshly folded usage. This is the one
        // place a run's live spend is visible ON the control task (assistant notifications carry the
        // in-flight estimate, result events the authoritative per-turn total), which is what makes
        // the bound a STOP rather than a report. An unset ceiling returns immediately.
        self.enforce_run_token_ceiling(&e.issue_id);
    }

    /// Stops a run whose live spend has reached the configured per-run token ceiling (STUDIO-967).
    ///
    /// A positive `agent.max_run_tokens` bounds a single run's spend WITHIN its turn, which none of
    /// the round-counting bounds do. It acts on the run IN FLIGHT — deliberately, unlike STUDIO-957's
    /// per-provider daily budget, which refuses NEW dispatch: that rule's "do not kill an in-flight
    /// run" protects a run that is innocent of the budget, whereas here the run IS the runaway, and
    /// the 44.7M tokens already spent are the argument FOR stopping rather than against. The spend is
    /// the committed total across finished turns plus the current turn's in-flight estimate
    /// (`cur_total_tokens`, cumulative-within-a-turn and last-wins), so the check sees a single
    /// monster turn as it grows rather than only after it ends.
    ///
    /// The stop is `terminate` — SIGKILL the agent's process tree — and it deliberately leaves the
    /// WORKSPACE and its branch intact, so work the run already committed survives. The run records
    /// its own outcome ([`store::OUTCOME_TOKEN_CEILING`]) with a reason naming the ceiling and the
    /// spend, so it is distinguishable in the history table from `failed`, `interrupted` and
    /// `stopped`; the claim/retry rows are dropped and the issue id is added to `claimed`, so the
    /// daemon does not immediately re-dispatch the ticket and burn the ceiling again with nothing to
    /// show. The reconciliation sweep reports the stop so a human decides what to do with it — an
    /// unexplained halt would be exactly the silent stall this feature must not become.
    ///
    /// A ticketless review run is bounded by the same ceiling: it is a run like any other, and the
    /// review half is where much of the spend lives. Its watch row is parked `truncated` (the same
    /// disposition a `max_turns` backstop gives a round that delivered no verdict) so the head is
    /// offered again rather than recorded as a partial read.
    ///
    /// `0` (the default) bounds nothing and returns before any state is touched, which is what keeps
    /// an install that never configures the key byte-identical to one built before it existed.
    fn enforce_run_token_ceiling(&mut self, issue_id: &str) {
        let Some(limit) = self
            .eff
            .as_ref()
            .map(|eff| eff.max_run_tokens)
            .filter(|n| *n > 0)
        else {
            return;
        };
        let spent = match self.running.get(issue_id) {
            Some(re) => re.total_tokens.saturating_add(re.cur_total_tokens),
            None => return,
        };
        if spent < limit {
            return;
        }
        let Some(re) = self.terminate(issue_id) else {
            return;
        };
        tracing::warn!(
            issue_id = %issue_id,
            issue_identifier = %re.issue.identifier,
            run_id = re.run_id,
            spent_tokens = spent,
            max_run_tokens = limit,
            "run exceeded its per-run token ceiling; stopping it (the workspace is left intact)"
        );
        // A review row would otherwise sit `in_flight` with no live run, which the watcher reads as
        // a CRASH. Park it `truncated` instead — the honest disposition for a round that ended
        // without delivering a verdict — so the sweep and the watcher both name it correctly.
        if let Some(run) = re.review.as_ref() {
            self.record_review_truncated(run);
        }
        // The reason names both numbers, so a history reader can see how far past the bound the run
        // went without cross-referencing the config file.
        let reason = format!(
            "stopped at its per-run token ceiling: {spent} tokens spent against \
             agent.max_run_tokens={limit}"
        );
        self.completed.remove(issue_id);
        self.persist_end_run(&re, store::OUTCOME_TOKEN_CEILING, &reason);
        self.persist_totals();
        self.persist_complete(&re.issue.identifier);
        // Suppress this session's re-dispatch of the ticket. The sweep reports it; an operator (or a
        // restart's ordinary selection) resumes it once the ticket's shape is addressed.
        self.claimed.insert(issue_id.to_string());
    }
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, TimeZone, Utc};
    use rhapsody_agent::{
        EVENT_NOTIFICATION, EVENT_SESSION_STARTED, EVENT_TURN_COMPLETED, EVENT_TURN_FAILED, Event,
        Usage,
    };
    use std::sync::Arc;

    use super::*;
    use crate::testsupport::{empty_effective, issue, orch_with_store, running_entry};

    fn orch_with_running(id: &str) -> Orchestrator {
        let mut o = Orchestrator::new("WORKFLOW.md");
        o.running
            .insert(id.to_string(), running_entry(issue(id, "MT-1", ""), "", ""));
        o
    }

    fn update(o: &mut Orchestrator, id: &str, ev: Event) {
        o.on_agent_update(AgentUpdate {
            issue_id: id.to_string(),
            ev,
        });
    }

    fn notification(usage: Option<Usage>) -> Event {
        Event {
            event_type: EVENT_NOTIFICATION.to_string(),
            usage,
            timestamp: Some(Utc::now()),
            ..Default::default()
        }
    }

    fn result(event_type: &str, usage: Option<Usage>) -> Event {
        Event {
            event_type: event_type.to_string(),
            usage,
            timestamp: Some(Utc::now()),
            ..Default::default()
        }
    }

    // Mirrors Go `TestRecentEventsBufferBounded`.
    #[test]
    fn recent_events_buffer_bounded() {
        let mut o = orch_with_running("1");
        for _ in 0..(MAX_RECENT_EVENTS + 10) {
            update(
                &mut o,
                "1",
                Event {
                    event_type: EVENT_NOTIFICATION.to_string(),
                    message: "m".to_string(),
                    ..Default::default()
                },
            );
        }
        assert_eq!(o.running["1"].recent_events.len(), MAX_RECENT_EVENTS);
    }

    // Mirrors Go `TestOnAgentUpdateTracksSessionAndTurns`.
    #[test]
    fn on_agent_update_tracks_session_and_turns() {
        let mut o = orch_with_running("1");
        update(
            &mut o,
            "1",
            Event {
                event_type: EVENT_SESSION_STARTED.to_string(),
                message: "thread-9".to_string(),
                timestamp: Some(Utc::now()),
                ..Default::default()
            },
        );
        let re = &o.running["1"];
        assert_eq!(re.thread_id, "thread-9");
        assert_eq!(re.turn_count, 1);
        assert_eq!(re.session_id, "thread-9-1");
        assert_eq!(re.last_event, EVENT_SESSION_STARTED);
        assert_ne!(
            re.last_event_at,
            DateTime::from_timestamp(0, 0).unwrap(),
            "last event time must be updated"
        );
    }

    // Mirrors Go `TestOnAgentUpdateAccumulatesUsage`.
    #[test]
    fn on_agent_update_accumulates_usage() {
        let mut o = orch_with_running("1");
        let u1 = Usage {
            input_tokens: 100,
            output_tokens: 40,
            total_tokens: 140,
            ..Default::default()
        };
        let u2 = Usage {
            input_tokens: 50,
            output_tokens: 10,
            total_tokens: 60,
            ..Default::default()
        };
        update(&mut o, "1", result(EVENT_TURN_COMPLETED, Some(u1)));
        update(&mut o, "1", result(EVENT_TURN_COMPLETED, Some(u2)));
        let re = &o.running["1"];
        assert_eq!(re.total_tokens, 200);
        assert_eq!(re.input_tokens, 150);
        assert_eq!(re.output_tokens, 50);
        assert_eq!(o.totals.total_tokens, 200);
        assert_eq!(o.totals.input_tokens, 150);
    }

    // Mirrors Go `TestOnAgentUpdateLiveUsageSetsCurNotCommitted`.
    #[test]
    fn on_agent_update_live_usage_sets_cur_not_committed() {
        let mut o = orch_with_running("1");
        let live = Usage {
            input_tokens: 50,
            output_tokens: 10,
            total_tokens: 60,
            ..Default::default()
        };
        update(&mut o, "1", notification(Some(live)));
        let re = &o.running["1"];
        assert_eq!(
            (
                re.cur_input_tokens,
                re.cur_output_tokens,
                re.cur_total_tokens
            ),
            (50, 10, 60)
        );
        assert_eq!(
            (re.input_tokens, re.output_tokens, re.total_tokens),
            (0, 0, 0)
        );
        assert_eq!(
            (
                o.totals.total_tokens,
                o.totals.input_tokens,
                o.totals.output_tokens
            ),
            (0, 0, 0)
        );
    }

    // Mirrors Go `TestOnAgentUpdateLiveUsageLastWinsNotAccumulated`: cur_* is LAST-WINS, not summed.
    #[test]
    fn on_agent_update_live_usage_last_wins_not_accumulated() {
        let mut o = orch_with_running("1");
        for (input, output, total) in [
            (10000, 100, 10100),
            (30000, 300, 30300),
            (60000, 600, 60600),
        ] {
            update(
                &mut o,
                "1",
                notification(Some(Usage {
                    input_tokens: input,
                    output_tokens: output,
                    total_tokens: total,
                    ..Default::default()
                })),
            );
        }
        let re = &o.running["1"];
        assert_eq!(
            re.cur_input_tokens, 60000,
            "last-wins (the old += bug would give 100000)"
        );
        assert_eq!((re.cur_output_tokens, re.cur_total_tokens), (600, 60600));
        assert_eq!((re.input_tokens, re.total_tokens), (0, 0));
    }

    // Mirrors Go `TestOnAgentUpdateResultsCommitPerTurnAdditivelyAcrossTurns`.
    #[test]
    fn on_agent_update_results_commit_per_turn_additively_across_turns() {
        let mut o = orch_with_running("1");
        // Turn 1: live snapshots then result (per-turn billed total 40000).
        update(
            &mut o,
            "1",
            notification(Some(Usage {
                input_tokens: 10000,
                total_tokens: 10000,
                ..Default::default()
            })),
        );
        update(
            &mut o,
            "1",
            notification(Some(Usage {
                input_tokens: 35000,
                total_tokens: 35000,
                ..Default::default()
            })),
        );
        update(
            &mut o,
            "1",
            result(
                EVENT_TURN_COMPLETED,
                Some(Usage {
                    input_tokens: 30000,
                    output_tokens: 1000,
                    cache_creation_tokens: 2000,
                    cache_read_tokens: 7000,
                    total_tokens: 40000,
                }),
            ),
        );
        {
            let re = &o.running["1"];
            assert_eq!(
                (re.cur_total_tokens, re.cur_input_tokens),
                (0, 0),
                "cur_* must reset at turn boundary"
            );
            assert_eq!((re.total_tokens, re.input_tokens), (40000, 30000));
        }

        // Turn 2: another result (per-turn billed total 25000).
        update(
            &mut o,
            "1",
            notification(Some(Usage {
                input_tokens: 12000,
                total_tokens: 12000,
                ..Default::default()
            })),
        );
        update(
            &mut o,
            "1",
            result(
                EVENT_TURN_COMPLETED,
                Some(Usage {
                    input_tokens: 20000,
                    output_tokens: 500,
                    cache_read_tokens: 4500,
                    total_tokens: 25000,
                    ..Default::default()
                }),
            ),
        );
        let re = &o.running["1"];
        assert_eq!(re.total_tokens, 65000, "committed total across 2 turns");
        assert_eq!(
            re.input_tokens, 50000,
            "committed uncached input across 2 turns"
        );
        assert_eq!(o.totals.total_tokens, 65000, "global billed total");
    }

    // Mirrors Go `TestOnAgentUpdateResultCommitsAndResetsCur`: live usage then the authoritative
    // result commits (not adds) and resets cur_* to 0. The Go test additionally cross-checks the
    // displayed total via `buildSnapshot`; that snapshot assertion lands with O4 (`snapshot.go`).
    #[test]
    fn on_agent_update_result_commits_and_resets_cur() {
        let mut o = orch_with_running("1");
        let live = Usage {
            input_tokens: 50,
            output_tokens: 10,
            total_tokens: 60,
            ..Default::default()
        };
        let res = Usage {
            input_tokens: 60,
            output_tokens: 12,
            total_tokens: 72,
            ..Default::default()
        };
        update(&mut o, "1", notification(Some(live)));
        update(&mut o, "1", result(EVENT_TURN_COMPLETED, Some(res)));
        let re = &o.running["1"];
        assert_eq!(
            (re.input_tokens, re.output_tokens, re.total_tokens),
            (60, 12, 72)
        );
        assert_eq!(
            (
                re.cur_input_tokens,
                re.cur_output_tokens,
                re.cur_total_tokens
            ),
            (0, 0, 0)
        );
        assert_eq!(
            (
                o.totals.total_tokens,
                o.totals.input_tokens,
                o.totals.output_tokens
            ),
            (72, 60, 12)
        );
    }

    // Mirrors Go `TestOnAgentUpdateFailedResultResetsCur`.
    #[test]
    fn on_agent_update_failed_result_resets_cur() {
        let mut o = orch_with_running("1");
        update(
            &mut o,
            "1",
            notification(Some(Usage {
                input_tokens: 50,
                output_tokens: 10,
                total_tokens: 60,
                ..Default::default()
            })),
        );
        update(
            &mut o,
            "1",
            result(
                EVENT_TURN_FAILED,
                Some(Usage {
                    input_tokens: 60,
                    output_tokens: 12,
                    total_tokens: 72,
                    ..Default::default()
                }),
            ),
        );
        let re = &o.running["1"];
        assert_eq!(re.total_tokens, 72);
        assert_eq!(
            re.cur_total_tokens, 0,
            "cur must reset after a failed result with usage"
        );
    }

    // Mirrors Go `TestOnAgentUpdateTimeoutFailedPreservesCurEstimate` (INF-208): a terminal event with
    // NO usage must NOT zero the live cur_* estimate.
    #[test]
    fn on_agent_update_timeout_failed_preserves_cur_estimate() {
        let mut o = orch_with_running("1");
        let live = Usage {
            input_tokens: 139000,
            output_tokens: 8000,
            total_tokens: 412803,
            ..Default::default()
        };
        update(&mut o, "1", notification(Some(live)));
        update(
            &mut o,
            "1",
            Event {
                event_type: EVENT_TURN_FAILED.to_string(),
                message: "turn timeout".to_string(),
                ..Default::default()
            },
        );
        let re = &o.running["1"];
        assert_eq!(
            (
                re.cur_input_tokens,
                re.cur_output_tokens,
                re.cur_total_tokens
            ),
            (139000, 8000, 412803),
            "cur_* must survive a no-usage terminal event for the floor"
        );
        assert_eq!(
            re.total_tokens, 0,
            "committed must stay 0 with no result usage"
        );
    }

    // Mirrors Go `TestOnAgentUpdateUnknownIssueIsNoop`.
    #[test]
    fn on_agent_update_unknown_issue_is_noop() {
        let mut o = Orchestrator::new("WORKFLOW.md");
        update(
            &mut o,
            "ghost",
            Event {
                event_type: EVENT_NOTIFICATION.to_string(),
                ..Default::default()
            },
        );
        assert_eq!(
            o.totals.total_tokens, 0,
            "update for unknown issue must be ignored"
        );
    }

    // Mirrors Go `TestOnAgentUpdateTracksPgidAcrossTurns` (agentupdate_pgid_test.go).
    #[test]
    fn on_agent_update_tracks_pgid_across_turns() {
        let mut o = Orchestrator::new("WORKFLOW.md");
        let fixed = Utc.with_ymd_and_hms(2026, 5, 29, 12, 0, 0).unwrap();
        o.now = Box::new(move || fixed);
        o.running.insert(
            "1".to_string(),
            running_entry(issue("1", "MT-1", ""), "", ""),
        );

        // First PID-bearing event (turn 1) captures pgid and seeds last_cpu_active_at.
        update(
            &mut o,
            "1",
            Event {
                event_type: EVENT_SESSION_STARTED.to_string(),
                pid: 4242,
                ..Default::default()
            },
        );
        assert_eq!(o.running["1"].pgid, 4242);
        assert_eq!(o.running["1"].last_cpu_active_at, fixed);

        // A PID==0 event must NOT change the captured pgid.
        update(
            &mut o,
            "1",
            Event {
                event_type: EVENT_NOTIFICATION.to_string(),
                pid: 0,
                ..Default::default()
            },
        );
        assert_eq!(
            o.running["1"].pgid, 4242,
            "PID 0 event must not change pgid"
        );

        // A NEW non-zero pid (turn 2 = a new group) updates pgid and resets the CPU baseline.
        if let Some(re) = o.running.get_mut("1") {
            re.cpu_sampled = true;
            re.last_cpu_ticks = 999;
        }
        update(
            &mut o,
            "1",
            Event {
                event_type: EVENT_SESSION_STARTED.to_string(),
                pid: 5555,
                ..Default::default()
            },
        );
        let re = &o.running["1"];
        assert_eq!(re.pgid, 5555, "new turn pid must be tracked");
        assert!(!re.cpu_sampled, "cpu_sampled must reset on pgid change");
        assert_eq!(
            re.last_cpu_ticks, 0,
            "last_cpu_ticks must reset on pgid change"
        );
    }

    // Mirrors Go `TestOnAgentUpdateCapturesPgidOnNonSessionEvent`: pgid capture is not gated on the
    // session_started event.
    #[test]
    fn on_agent_update_captures_pgid_on_non_session_event() {
        let mut o = Orchestrator::new("WORKFLOW.md");
        let fixed = Utc.with_ymd_and_hms(2026, 5, 29, 12, 0, 0).unwrap();
        o.now = Box::new(move || fixed);
        o.running.insert(
            "1".to_string(),
            running_entry(issue("1", "MT-1", ""), "", ""),
        );
        update(
            &mut o,
            "1",
            Event {
                event_type: EVENT_NOTIFICATION.to_string(),
                pid: 777,
                ..Default::default()
            },
        );
        assert_eq!(
            o.running["1"].pgid, 777,
            "capture must not be gated on session_started"
        );
    }

    // ---------------------------------------------------------------------------------------------
    // STUDIO-967 — the per-run token ceiling.
    // ---------------------------------------------------------------------------------------------

    /// An orchestrator with an in-memory store and an `Effective` carrying `limit` as its
    /// `agent.max_run_tokens`. `limit == 0` is the unset key.
    fn orch_with_ceiling(limit: i64) -> (Orchestrator, Arc<dyn store::Store + Send + Sync>) {
        let (mut o, st) = orch_with_store();
        let mut eff = empty_effective(Arc::new(rhapsody_tracker::fake::Fake::new()));
        eff.max_run_tokens = limit;
        o.eff = Some(eff);
        (o, st)
    }

    /// One assistant notification carrying a billed cumulative-within-turn total — the live
    /// mid-turn signal the ceiling watches.
    fn live_usage(total: i64) -> Event {
        notification(Some(Usage {
            input_tokens: total,
            total_tokens: total,
            ..Default::default()
        }))
    }

    fn first_run(st: &(dyn store::Store + Send + Sync)) -> store::RunSummary {
        st.list_runs(store::RunFilter::default())
            .expect("list runs")
            .into_iter()
            .next()
            .expect("one run row")
    }

    // ⚠️ STUDIO-967 mutation discipline: an unset ceiling must bound NOTHING. The mutation this pins
    // is a default of some finite number — with `agent.max_run_tokens` left at 0, a run that has
    // accumulated a colossal spend must keep running and record no terminal outcome.
    #[test]
    fn unset_ceiling_bounds_nothing() {
        let (mut o, st) = orch_with_ceiling(0);
        let mut re = running_entry(issue("ID-1", "MT-1", "In Progress"), "", "");
        o.persist_start_run(&mut re, 0);
        o.running.insert("ID-1".into(), re);

        update(&mut o, "ID-1", live_usage(44_743_645));

        assert!(
            o.running.contains_key("ID-1"),
            "an unset ceiling must never stop a run"
        );
        assert_eq!(
            first_run(st.as_ref()).outcome,
            store::OUTCOME_RUNNING,
            "the run must keep its running outcome"
        );
    }

    // Acceptance: reconstruct the STUDIO-957 shape. Its run was ONE turn that reached 44.7M tokens
    // in 42 minutes and stayed green because every shipped bound counts rounds. Here a single
    // in-flight turn (a live usage notification, no turn boundary ever seen) crosses the ceiling and
    // the run stops.
    #[test]
    fn studio_957_single_turn_past_the_ceiling_stops() {
        let (mut o, st) = orch_with_ceiling(2_000_000);
        let mut re = running_entry(issue("ID-1", "STUDIO-957", "In Progress"), "", "");
        o.persist_start_run(&mut re, 0);
        o.running.insert("ID-1".into(), re);

        update(&mut o, "ID-1", live_usage(44_743_645));

        assert!(
            !o.running.contains_key("ID-1"),
            "a single turn past the ceiling must stop"
        );
        let run = first_run(st.as_ref());
        assert_eq!(
            run.total_tokens, 44_743_645,
            "the spend that triggered the stop is recorded on the run"
        );
        assert!(
            run.error.contains("max_run_tokens"),
            "the reason must name the ceiling, got {:?}",
            run.error
        );
    }

    // ⚠️ Acceptance + mutation discipline: the stop is its OWN outcome. Reusing `failed`,
    // `stopped`, `interrupted` or `completed` is a lie in the history table (a ceiling stop is none
    // of those), so this pins the distinct value AND its distinctness from each borrowed label.
    #[test]
    fn ceiling_stop_records_its_own_outcome() {
        let (mut o, st) = orch_with_ceiling(1_000);
        let mut re = running_entry(issue("ID-1", "MT-1", "In Progress"), "", "");
        o.persist_start_run(&mut re, 0);
        o.running.insert("ID-1".into(), re);

        update(&mut o, "ID-1", live_usage(5_000));

        let outcome = first_run(st.as_ref()).outcome;
        assert_eq!(outcome, store::OUTCOME_TOKEN_CEILING);
        for borrowed in [
            store::OUTCOME_FAILED,
            store::OUTCOME_STOPPED,
            store::OUTCOME_INTERRUPTED,
            store::OUTCOME_COMPLETED,
        ] {
            assert_ne!(outcome, borrowed, "must not borrow {borrowed}");
        }
    }

    // The stop halts THIS session's re-dispatch of the ticket (a fresh dispatch would re-burn the
    // ceiling with nothing to show), drops the claim/retry rows, and keeps the branch: nothing here
    // removes a workspace, and the sweep is what surfaces it to a human.
    #[test]
    fn ceiling_stop_holds_the_ticket_and_drops_its_claim() {
        let (mut o, st) = orch_with_ceiling(1_000);
        let mut re = running_entry(issue("ID-1", "MT-1", "In Progress"), "", "");
        o.persist_start_run(&mut re, 0);
        o.running.insert("ID-1".into(), re);
        o.claimed.insert("ID-1".into());

        update(&mut o, "ID-1", live_usage(5_000));

        assert!(o.claimed.contains("ID-1"), "the ticket must stay held");
        assert!(!o.running.contains_key("ID-1"));
        let rec = st.load_recovery().expect("load recovery");
        assert!(
            rec.retries.is_empty() && rec.claims.is_empty(),
            "claim/retry rows must be dropped: {rec:?}"
        );
    }

    // Acceptance: review runs are subject to the SAME ceiling as author runs — half the spend is
    // reviews. The mutation this pins is applying the ceiling only to author runs.
    #[test]
    fn a_review_run_is_subject_to_the_same_ceiling() {
        let (mut o, st) = orch_with_ceiling(1_000);
        let key = "pr:o/r#7@alice";
        let mut re = running_entry(issue(key, key, "In Progress"), "", "");
        re.review = Some(crate::review::ReviewRun {
            owner: "o".to_string(),
            repo: "r".to_string(),
            number: 7,
            reviewer: "alice".to_string(),
            author: "bob".to_string(),
            team_id: "T".to_string(),
            repo_url: String::new(),
            head_sha: "deadbeef".to_string(),
            introduced_by: "handoff".to_string(),
            prior_sha: String::new(),
        });
        o.persist_start_run(&mut re, 0);
        o.running.insert(key.to_string(), re);

        update(&mut o, key, live_usage(5_000));

        assert!(
            !o.running.contains_key(key),
            "a review run must be stopped by the same ceiling"
        );
        assert_eq!(first_run(st.as_ref()).outcome, store::OUTCOME_TOKEN_CEILING);
    }
}
