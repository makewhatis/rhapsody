//! summonwatermark — the durable per-ticket summons watermark (STUDIO-885). No Go counterpart.
//!
//! # The defect
//!
//! A summons is a DURABLE fact: an `@symphony` comment that still exists on the pull request. The
//! daemon nevertheless only ever saw it as a transient one. GitHub enrichment asks the source for
//! comments newer than `now - DEFAULT_GH_LOOKBACK` — five minutes — so
//! [`Issue::latest_summon_at`] is re-derived from scratch on every poll and reverts to unset the
//! moment the comment ages out of that window.
//!
//! `pr_suppressed` ([`crate::dispatch`]) meanwhile treats a ticket with a linked pull request as
//! suppressed UNLESS a summons is newer than the ticket's last run start. So a summons opened a
//! five-minute window in which the ticket could be dispatched, and then closed it — permanently.
//! On the reported incident the board was at its concurrency cap for the whole of that window
//! (four running agents against `max_concurrent_agents: 4`, an entirely ordinary busy period), the
//! selection pass admitted nothing, the comment aged out, and the ticket went back to reading as
//! correctly suppressed for the next twelve hours. Nothing re-read the comment, and nothing
//! remembered that it had been seen.
//!
//! # The fix, and what it deliberately does NOT change
//!
//! Remember the newest summons ever OBSERVED for a ticket, in the store
//! ([`rhapsody_store::SummonWatermark`]), and put it back on the candidate when the live
//! enrichment cannot see it any more. The comparison `pr_suppressed` actually makes is then
//! between two durable facts — the summons and the ticket's last author-run window (its END, since
//! STUDIO-1045) — so it keeps its meaning however long the ticket waits for a slot.
//!
//! The suppression rule itself is otherwise untouched, which is the point. A ticket does NOT become
//! permanently dispatchable because it was summoned once: the watermark only ever lifts the
//! suppression while it is newer than the last author run's window, and dispatching the ticket
//! advances that window past it. A merged pull request with an old summons stays suppressed exactly
//! as before.
//!
//! Widening the lookback instead was rejected: it converts "stranded after five minutes of
//! contention" into "stranded after N minutes of contention" and closes nothing.
//!
//! # Where it runs
//!
//! At the candidate-fetch seam of BOTH dispatch ladders — the end of `poll_all_projects` for a
//! `projects:` install and the legacy single-tracker fetch — so a candidate is already carrying
//! everything the daemon knows about its summons before ANY consumer sees it: mid-run delivery,
//! the selection pass, `pr_suppressed` and `review_reopen_eligible` alike. Deliberately not inside
//! `pr_suppressed`, which would fix one reader and leave the other four with the amnesia.
//!
//! It is NOT gated on the GitHub-summons feature. A Linear-comment summons reaches
//! `latest_summon_at` through the tracker rather than through enrichment, and remembering it costs
//! the same nothing; gating would make the repair depend on which source happened to observe it.
//!
//! With storage off ([`rhapsody_store::Noop`]) every call is a no-op in both directions and the
//! daemon keeps the pre-STUDIO-885 behaviour: there is nowhere to remember an observation, so it
//! sees only what is inside the lookback window right now.

use chrono::DateTime;
use rhapsody_core::Issue;
use rhapsody_store::SummonWatermark;

use crate::orchestrator::Orchestrator;

impl Orchestrator {
    /// Reconciles each candidate's `latest_summon_at`/`latest_summon_body` with what the store
    /// remembers: the NEWER of the two wins, and whichever it is becomes both the candidate's view
    /// and the remembered one.
    ///
    /// Every store failure is a logged warning and a candidate left exactly as the enrichment
    /// produced it — the daemon degrades to the pre-STUDIO-885 behaviour for that ticket rather
    /// than losing a poll over a history-database problem.
    pub(crate) fn restore_summon_watermarks<'a>(
        &self,
        issues: impl IntoIterator<Item = &'a mut Issue>,
    ) {
        for iss in issues {
            if iss.identifier.is_empty() {
                continue; // the row is keyed by identifier; there is nothing to key on
            }
            let remembered = match self.store().summon_watermark(&iss.identifier) {
                Ok(w) => w,
                Err(e) => {
                    tracing::warn!(
                        issue_identifier = %iss.identifier,
                        err = %e,
                        "summons watermark read failed; using only this poll's view"
                    );
                    continue;
                }
            };
            // Compared as the store's ONE canonical rendering rather than as instants, so that a
            // source reporting sub-second precision does not re-write the same comment on every
            // poll just because the stored form is second-precision (`format_summon_at`).
            let observed = iss.latest_summon_at.map(rhapsody_store::format_summon_at);
            match (observed, remembered) {
                (Some(obs), None) => self.remember_summon(iss, obs),
                (Some(obs), Some(w)) => {
                    if obs > w.at {
                        self.remember_summon(iss, obs);
                    } else if obs < w.at {
                        self.restore_summon(iss, w);
                    }
                    // Equal: this poll saw exactly what is remembered. Neither write nor restore —
                    // in particular, do not overwrite the live body with the stored copy of the
                    // same comment.
                }
                (None, Some(w)) => self.restore_summon(iss, w),
                (None, None) => {}
            }
        }
    }

    /// Records this poll's observation as the ticket's newest summons. Time and body are written
    /// together because they describe the SAME comment (INF-448).
    fn remember_summon(&self, iss: &Issue, at: String) {
        if let Err(e) = self.store().record_summon_watermark(SummonWatermark {
            identifier: iss.identifier.clone(),
            at,
            body: iss.latest_summon_body.clone(),
        }) {
            tracing::warn!(
                issue_identifier = %iss.identifier,
                err = %e,
                "summons watermark write failed; a summons observed now may not survive the lookback window"
            );
        }
    }

    /// Puts a remembered summons back on a candidate whose live view has lost it (or never had
    /// it). An unparseable stored timestamp leaves the candidate untouched — the same tolerance
    /// `last_run_window` applies to a stored run start it cannot read.
    fn restore_summon(&self, iss: &mut Issue, w: SummonWatermark) {
        let Ok(at) = DateTime::parse_from_rfc3339(&w.at) else {
            tracing::warn!(
                issue_identifier = %iss.identifier,
                summon_at = %w.at,
                "remembered summons has an unparseable timestamp; ignoring it"
            );
            return;
        };
        tracing::debug!(
            issue_identifier = %iss.identifier,
            summon_at = %w.at,
            "restored a remembered summons the lookback window no longer covers"
        );
        iss.latest_summon_at = Some(at.with_timezone(&chrono::Utc));
        iss.latest_summon_body = w.body;
    }
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, TimeZone, Utc};
    use rhapsody_store::{Sqlite, StorePath, SummonWatermark};
    use std::sync::Arc;

    use crate::orchestrator::Orchestrator;
    use crate::testsupport::issue;

    fn at(min: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 13, 4, min, 0)
            .single()
            .expect("timestamp")
    }

    /// An orchestrator over a fresh in-memory store (persistence ON — the watermark's whole point).
    fn orch_with_store() -> Orchestrator {
        let mut o = Orchestrator::new("WORKFLOW.md");
        o.store = Arc::new(Sqlite::open(StorePath::InMemory).expect("open in-memory"));
        o
    }

    // A summons nothing has seen before is remembered, verbatim, time and body together.
    #[test]
    fn an_observed_summons_is_remembered() {
        let o = orch_with_store();
        let mut iss = issue("1", "A-1", "Todo");
        iss.latest_summon_at = Some(at(29));
        iss.latest_summon_body = "@symphony have another look".into();

        o.restore_summon_watermarks(std::iter::once(&mut iss));

        assert_eq!(
            o.store().summon_watermark("A-1").expect("read"),
            Some(SummonWatermark {
                identifier: "A-1".into(),
                at: "2026-09-13T04:29:00Z".into(),
                body: "@symphony have another look".into(),
            })
        );
    }

    // THE DEFECT, at its smallest: the comment has aged out of the lookback window, so this poll
    // sees no summons at all — and the candidate gets the remembered one back.
    #[test]
    fn a_summons_outside_the_lookback_is_restored() {
        let o = orch_with_store();
        o.store()
            .record_summon_watermark(SummonWatermark {
                identifier: "A-1".into(),
                at: "2026-09-13T04:29:00Z".into(),
                body: "@symphony have another look".into(),
            })
            .expect("seed");
        let mut iss = issue("1", "A-1", "Todo"); // no latest_summon_at: outside the window

        o.restore_summon_watermarks(std::iter::once(&mut iss));

        assert_eq!(iss.latest_summon_at, Some(at(29)));
        assert_eq!(iss.latest_summon_body, "@symphony have another look");
    }

    // A NEWER comment replaces the remembered one — the watermark tracks the newest summons, not
    // the first.
    #[test]
    fn a_newer_observation_advances_the_watermark() {
        let o = orch_with_store();
        o.store()
            .record_summon_watermark(SummonWatermark {
                identifier: "A-1".into(),
                at: "2026-09-13T04:29:00Z".into(),
                body: "older".into(),
            })
            .expect("seed");
        let mut iss = issue("1", "A-1", "Todo");
        iss.latest_summon_at = Some(at(33));
        iss.latest_summon_body = "newer".into();

        o.restore_summon_watermarks(std::iter::once(&mut iss));

        assert_eq!(
            iss.latest_summon_at,
            Some(at(33)),
            "the live view must not be dragged backwards"
        );
        assert_eq!(iss.latest_summon_body, "newer");
        assert_eq!(
            o.store()
                .summon_watermark("A-1")
                .expect("read")
                .map(|w| w.at),
            Some("2026-09-13T04:33:00Z".into())
        );
    }

    // The mirror image, and the arm that stops a live view from dragging a candidate BACKWARDS: a
    // Linear-comment summons reaches `latest_summon_at` through the tracker rather than through
    // enrichment, and `apply_github_summons`' unmerged-only rule can surface an older comment than
    // the watermark already holds. Either way the newer fact wins, and it is the remembered one.
    #[test]
    fn an_older_observation_is_overridden_by_the_remembered_one() {
        let o = orch_with_store();
        o.store()
            .record_summon_watermark(SummonWatermark {
                identifier: "A-1".into(),
                at: "2026-09-13T04:33:00Z".into(),
                body: "newer".into(),
            })
            .expect("seed");
        let mut iss = issue("1", "A-1", "Todo");
        iss.latest_summon_at = Some(at(29));
        iss.latest_summon_body = "older".into();

        o.restore_summon_watermarks(std::iter::once(&mut iss));

        assert_eq!(
            iss.latest_summon_at,
            Some(at(33)),
            "an older live view must not drag the candidate backwards"
        );
        assert_eq!(iss.latest_summon_body, "newer");
        assert_eq!(
            o.store()
                .summon_watermark("A-1")
                .expect("read")
                .map(|w| w.at),
            Some("2026-09-13T04:33:00Z".into()),
            "and the watermark itself must not move backwards either"
        );
    }

    // Equal times: this poll saw exactly the comment that is already remembered, so the pass does
    // NEITHER a write nor a restore. The two bodies differ only so that both halves of that are
    // observable at once — a re-write would push the live body into the store, and a restore would
    // clobber the live body with the stored copy of the same comment. Neither may happen, and the
    // steady state (the same comment re-observed on every poll of a long wait) must stay a pure
    // read rather than re-writing the row forever.
    #[test]
    fn an_unchanged_observation_neither_writes_nor_restores() {
        let o = orch_with_store();
        o.store()
            .record_summon_watermark(SummonWatermark {
                identifier: "A-1".into(),
                at: "2026-09-13T04:29:00Z".into(),
                body: "as remembered".into(),
            })
            .expect("seed");
        let mut iss = issue("1", "A-1", "Todo");
        iss.latest_summon_at = Some(at(29));
        iss.latest_summon_body = "as observed".into();

        o.restore_summon_watermarks(std::iter::once(&mut iss));

        assert_eq!(
            iss.latest_summon_body, "as observed",
            "the live body must not be overwritten with the stored copy"
        );
        assert_eq!(
            o.store().summon_watermark("A-1").expect("read"),
            Some(SummonWatermark {
                identifier: "A-1".into(),
                at: "2026-09-13T04:29:00Z".into(),
                body: "as remembered".into(),
            }),
            "an unchanged observation must not re-write the row"
        );
    }

    // A ticket nobody has ever summoned gains nothing and writes nothing — the overwhelmingly
    // common case must stay a pure read.
    #[test]
    fn a_never_summoned_ticket_is_untouched() {
        let o = orch_with_store();
        let mut iss = issue("1", "A-1", "Todo");

        o.restore_summon_watermarks(std::iter::once(&mut iss));

        assert_eq!(iss.latest_summon_at, None);
        assert_eq!(o.store().summon_watermark("A-1").expect("read"), None);
    }

    // With persistence off there is nowhere to remember an observation, and the candidate is left
    // exactly as the enrichment produced it.
    #[test]
    fn storage_off_is_a_no_op_in_both_directions() {
        let o = Orchestrator::new("WORKFLOW.md"); // defaults to the Noop store
        let mut iss = issue("1", "A-1", "Todo");
        iss.latest_summon_at = Some(at(29));

        o.restore_summon_watermarks(std::iter::once(&mut iss));

        assert_eq!(iss.latest_summon_at, Some(at(29)));
        assert_eq!(o.store().summon_watermark("A-1").expect("read"), None);
    }
}
