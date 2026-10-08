//! Tech-lead reporting and operator overrules (STUDIO-1138). No Go counterpart.

use rhapsody_config::memory::{MemoryError, Record};
use rhapsody_store::{LeadDecisionRow, LeadTrigger, Store};
use std::sync::Arc;

pub struct LeadReports {
    pub store: Arc<dyn Store + Send + Sync>,
    pub memory: Option<Arc<dyn OverruleMemory>>,
}

/// The host-only T3 retain seam, never a teammate/personal-bank write.
#[async_trait::async_trait]
pub trait OverruleMemory: Send + Sync {
    async fn retain_overrule(&self, record: &Record) -> Result<String, MemoryError>;
}

#[async_trait::async_trait]
impl OverruleMemory for rhapsody_config::hindsight::OperatorMemory {
    async fn retain_overrule(&self, record: &Record) -> Result<String, MemoryError> {
        rhapsody_config::hindsight::OperatorMemory::retain_overrule(self, record).await
    }
}

impl crate::ControlHandle {
    pub fn lead_reports(&self) -> Option<Arc<LeadReports>> {
        self.lead_reports.clone()
    }
    pub fn wake_lead_reports(&self) {
        if let Some(tx) = &self.lead_report_tx
            && tx.send(()).is_err()
        {
            tracing::warn!("lead reporting task unavailable");
        }
    }
}

impl crate::Orchestrator {
    pub(crate) fn lead_human_feed(&self) -> Vec<crate::reviewreconcile::Divergence> {
        if !self.lead_enabled() {
            return Vec::new();
        }
        let Some(reports) = &self.lead_reports else {
            return Vec::new();
        };
        let view = match reports.decisions("") {
            Ok(view) => view,
            Err(error) => {
                tracing::warn!(%error, "lead human feed unavailable");
                return Vec::new();
            }
        };
        view["decisions"].as_array().into_iter().flatten().filter(|r| r["decision"].as_str().is_some_and(|s| s.starts_with("proposed:") || s.starts_with("escalate:")) && r["overruled_at"].is_null()).map(|r| crate::reviewreconcile::Divergence {
            pr: r["subject"].as_str().unwrap_or_default().into(),
            kind: crate::reviewreconcile::DivergenceKind::ManagerDeferred,
            ticket: String::new(), reviewer: "lead".into(), stale_secs: 0,
            auto_merge_reason: None, capacity_held: None, capacity_unreadable: None,
            adjudicated_head: String::new(), current_head: String::new(), rounds: 0,
            findings: Vec::new(),
            reason: format!("Lead decision {} — {} Reasoning: {}. See the Lead page for evidence and Overrule.", r["id"], r["decision"].as_str().unwrap_or_default(), r["reasoning"].as_str().unwrap_or_default()),
        }).collect()
    }
}

impl LeadReports {
    pub fn decisions(&self, since: &str) -> Result<serde_json::Value, String> {
        let since_time = if since.is_empty() {
            None
        } else {
            Some(
                chrono::DateTime::parse_from_rfc3339(since)
                    .map_err(|_| "since must be RFC3339")?
                    .with_timezone(&chrono::Utc),
            )
        };
        let since = if since.is_empty() {
            String::new()
        } else {
            rhapsody_store::format_summon_at(
                chrono::DateTime::parse_from_rfc3339(since)
                    .map_err(|_| "since must be RFC3339")?
                    .with_timezone(&chrono::Utc),
            )
        };
        let items = self.store.load_lead_items().map_err(|e| e.to_string())?;
        let rows = self
            .store
            .lead_digest_entries(&since)
            .map_err(|e| e.to_string())?;
        let mut decisions = Vec::new();
        for row in rows {
            if let Some(since) = since_time {
                let at = chrono::DateTime::parse_from_rfc3339(&row.at)
                    .map_err(|_| "lead decision timestamp invalid")?;
                if at < since {
                    continue;
                }
            }
            let item = items
                .iter()
                .find(|i| i.id == row.item)
                .ok_or("lead decision subject missing")?;
            let mut value = serde_json::json!({"id": row.id, "item": row.item, "at": row.at, "decision": row.decision, "reasoning": row.reasoning, "evidence": row.evidence, "actions": row.actions, "harness": row.harness, "model": row.model, "overruled_at": row.overruled_at, "overrule_note": row.overrule_note});
            value["subject"] = item.subject.clone().into();
            value["trigger"] = trigger_name(&item.trigger).into();
            decisions.push(value);
        }
        let queued = items.iter().filter(|i| matches!(i.state.as_str(), "queued" | "running" | "parked")).map(|i| serde_json::json!({"id": i.id, "subject": i.subject, "state": i.state, "trigger": trigger_name(&i.trigger)})).collect::<Vec<_>>();
        Ok(serde_json::json!({"decisions": decisions, "queued": queued}))
    }
    pub async fn overrule(&self, id: i64, note: &str) -> Result<serde_json::Value, String> {
        let note = note.trim();
        if id <= 0
            || note.is_empty()
            || note.len() > 3000
            || crate::managerdecision::contains_secret_shape(note)
        {
            return Err(
                "overrule requires a positive decision id and a non-secret note of 1–3000 bytes"
                    .into(),
            );
        }
        let note = crate::managerapply::strip_summon_tokens(note);
        if note.trim().is_empty() {
            return Err("overrule requires a non-empty tokenless note".into());
        }
        let at = rhapsody_store::format_summon_at(chrono::Utc::now());
        let item = self
            .store
            .overrule_lead_decision(id, &note, &at)
            .map_err(|e| e.to_string())?
            .ok_or("decision not found or still applying")?;
        let row = self
            .store
            .load_lead_decisions()
            .map_err(|e| e.to_string())?
            .into_iter()
            .find(|r| r.id == id)
            .ok_or("decision not found")?;
        if row.overrule_note.as_deref() != Some(&note) {
            return Err("decision was already overruled with a different note".into());
        }
        let at = row
            .overruled_at
            .as_deref()
            .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
            .map(|at| at.with_timezone(&chrono::Utc))
            .ok_or("overrule timestamp missing")?;
        let memory_retained = if self
            .store
            .lead_report_count(&format!("overrule-memory:{id}"))
            .map_err(|e| e.to_string())?
            > 0
        {
            true
        } else if let Some(memory) = &self.memory {
            let record = Record {
                identity: "David".into(),
                document_id: format!("lead-overrule-{id}"),
                at,
                content: format!(
                    "by: David; operator preference. The operator overruled decision {id}: {note}"
                ),
                ..Default::default()
            };
            match tokio::time::timeout(
                std::time::Duration::from_secs(15),
                memory.retain_overrule(&record),
            )
            .await
            {
                Ok(Ok(id)) if !id.is_empty() => {
                    self.store
                        .reserve_lead_report(&format!("overrule-memory:{}", row.id), 1)
                        .map_err(|e| e.to_string())?;
                    true
                }
                _ => {
                    tracing::warn!(
                        decision = row.id,
                        "overrule saved and work queued; operator preference memory unavailable"
                    );
                    false
                }
            }
        } else {
            false
        };
        Ok(serde_json::json!({"decision": id, "item": item, "memory_retained": memory_retained}))
    }
    pub fn digest<Tz: chrono::TimeZone>(
        &self,
        now: chrono::DateTime<Tz>,
        at: &str,
    ) -> Result<Option<String>, String> {
        let at = chrono::NaiveTime::parse_from_str(at, "%H:%M").map_err(|_| "invalid digest_at")?;
        if now.time() < at {
            return Ok(None);
        }
        let day = now.date_naive();
        // Resolve yesterday's boundaries with the zone's transition rules, not today's offset.
        let yesterday = day.pred_opt().ok_or("digest date out of range")?;
        let midnight = chrono::NaiveTime::MIN;
        let start = now
            .timezone()
            .from_local_datetime(&yesterday.and_time(midnight))
            .earliest()
            .ok_or("yesterday midnight unavailable")?;
        let end = now
            .timezone()
            .from_local_datetime(&day.and_time(midnight))
            .earliest()
            .ok_or("today midnight unavailable")?;
        let rows = self
            .store
            .lead_digest_entries(&rhapsody_store::format_summon_at(
                start.with_timezone(&chrono::Utc),
            ))
            .map_err(|e| e.to_string())?;
        let end = rhapsody_store::format_summon_at(end.with_timezone(&chrono::Utc));
        let rows: Vec<_> = rows
            .iter()
            .filter(|r| r.at < end && r.decision != "applying")
            .collect();
        let escalations = rows
            .iter()
            .filter(|r| r.decision.starts_with("escalate:"))
            .count();
        let queued = self
            .store
            .load_lead_items()
            .map_err(|e| e.to_string())?
            .iter()
            .filter(|i| matches!(i.state.as_str(), "queued" | "parked"))
            .count();
        if !self
            .store
            .reserve_lead_report(&format!("digest:{day}"), 1)
            .map_err(|e| e.to_string())?
        {
            return Ok(None);
        }
        Ok(Some(format!(
            "Lead: {} decisions yesterday, {escalations} escalations; {queued} items queued",
            rows.len()
        )))
    }
    pub fn pages(&self) -> Result<Vec<String>, String> {
        let items = self.store.load_lead_items().map_err(|e| e.to_string())?;
        let mut rows = self
            .store
            .load_lead_decisions()
            .map_err(|e| e.to_string())?;
        let mut pages = Vec::new();
        for item in &items {
            if let LeadTrigger::Escalation { need, .. } = &item.trigger
                && item.state != "done"
            {
                if !rows.iter().any(|r| r.item == item.id) {
                    let mut row = LeadDecisionRow {
                        item: item.id,
                        at: rhapsody_store::format_summon_at(chrono::Utc::now()),
                        decision: format!(
                            "escalate: {}",
                            crate::managerapply::strip_summon_tokens(need)
                        ),
                        reasoning:
                            "This item already states the operator step; no model run needed."
                                .into(),
                        evidence: format!("Queued escalation item {}", item.id),
                        actions: "[]".into(),
                        ..Default::default()
                    };
                    row.id = self
                        .store
                        .save_lead_decision(&row)
                        .map_err(|e| e.to_string())?;
                    rows.push(row);
                }
                self.store
                    .set_lead_item_state(item.id, "done")
                    .map_err(|e| e.to_string())?;
            }
        }
        rows.sort_by_key(|r| r.item);
        for row in rows {
            if let Some(need) = row.decision.strip_prefix("escalate: ")
                && self
                    .store
                    .reserve_lead_report(&format!("page-decision:{}", row.id), 1)
                    .map_err(|e| e.to_string())?
            {
                let subject = items
                    .iter()
                    .find(|i| i.id == row.item)
                    .map_or("unknown subject", |i| i.subject.as_str());
                pages.push(format!("{subject}: {need}"));
            }
        }
        Ok(pages)
    }
}

pub fn trigger_name(trigger: &LeadTrigger) -> &'static str {
    match trigger {
        LeadTrigger::BlockedHandoff { .. } => "blocked_handoff",
        LeadTrigger::ReviewEscalation { .. } => "review_escalation",
        LeadTrigger::ImpossibleState { .. } => "impossible_state",
        LeadTrigger::LimitJudgment { .. } => "limit_judgment",
        LeadTrigger::BreakerHold { .. } => "breaker_hold",
        LeadTrigger::Overrule { .. } => "overrule",
        LeadTrigger::Escalation { .. } => "escalation",
    }
}

/// Local-only recovery/digest sweep. Immediate pages also wake this task from the executor.
pub async fn run_report_task(
    mut ctx: crate::control_loop::CancelWait,
    reports: Arc<LeadReports>,
    digest_at: String,
    dashboard: String,
    channels: Vec<Arc<dyn crate::breaker::NotifyChannel>>,
    mut wake: tokio::sync::mpsc::UnboundedReceiver<()>,
) {
    let mut timer = tokio::time::interval(std::time::Duration::from_secs(30));
    loop {
        tokio::select! { _ = ctx.cancelled() => return, _ = timer.tick() => {}, message = wake.recv() => if message.is_none() { return; } }
        let pages = reports.pages();
        match pages {
            Ok(pages) => {
                for body in pages {
                    send(&channels, "Lead needs you", &body, &dashboard).await;
                }
            }
            Err(error) => tracing::warn!(%error, "lead pages unavailable"),
        }
        match reports.digest(chrono::Local::now(), &digest_at) {
            Ok(Some(body)) => send(&channels, "Lead daily digest", &body, &dashboard).await,
            Ok(None) => {}
            Err(error) => tracing::warn!(%error, "lead digest unavailable"),
        }
    }
}

async fn send(
    channels: &[Arc<dyn crate::breaker::NotifyChannel>],
    title: &str,
    body: &str,
    dashboard: &str,
) {
    if channels.is_empty() {
        tracing::warn!(title, "lead push has no configured notification channel");
    }
    for channel in channels {
        let body = if dashboard.is_empty() {
            body.to_string()
        } else {
            format!("{body}\n{dashboard}")
        };
        if let Err(error) = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            channel.send_lead(title, &body, dashboard),
        )
        .await
        .unwrap_or_else(|_| Err("lead notification timed out".into()))
        {
            tracing::warn!(channel = channel.name(), %error, "lead push delivery failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rhapsody_store::{LeadDecisionRow, LeadTrigger, Sqlite, StorePath};

    #[derive(Default)]
    struct RecordingMemory(std::sync::Mutex<Vec<Record>>);
    #[async_trait::async_trait]
    impl OverruleMemory for RecordingMemory {
        async fn retain_overrule(&self, record: &Record) -> Result<String, MemoryError> {
            self.0.lock().unwrap().push(record.clone());
            Ok("retained-overrule".into())
        }
    }

    fn reports() -> LeadReports {
        LeadReports {
            store: Arc::new(Sqlite::open(StorePath::InMemory).unwrap()),
            memory: None,
        }
    }
    fn decision(reports: &LeadReports, at: &str, summary: &str) -> i64 {
        let item = reports
            .store
            .enqueue_lead_item(
                &LeadTrigger::BlockedHandoff {
                    ticket: "TEST-1".into(),
                    question: "what should happen?".into(),
                },
                at,
            )
            .unwrap();
        reports
            .store
            .save_lead_decision(&LeadDecisionRow {
                item,
                at: at.into(),
                decision: summary.into(),
                reasoning: "The spec supports this.".into(),
                evidence: "src/test.rs and the ticket".into(),
                actions: "[]".into(),
                ..Default::default()
            })
            .unwrap()
    }
    #[test]
    fn decisions_api_since() {
        let r = reports();
        decision(&r, "2026-10-07T10:00:00Z", "done: requeue");
        let id = decision(&r, "2026-10-08T10:00:00Z", "done: route_back");
        let view = r.decisions("2026-10-08T11:00:00+01:00").unwrap();
        assert_eq!(view["decisions"].as_array().unwrap().len(), 1);
        assert_eq!(view["decisions"][0]["id"], id);
        assert_eq!(view["decisions"][0]["subject"], "TEST-1");
        assert_eq!(view["decisions"][0]["reasoning"], "The spec supports this.");
        assert_eq!(
            view["decisions"][0]["evidence"],
            "src/test.rs and the ticket"
        );
        assert!(r.decisions("garbage").is_err());
        assert!(
            r.decisions("2026-10-08T10:00:00.001Z").unwrap()["decisions"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }
    #[tokio::test]
    async fn overrule_retains_and_opens_item() {
        let mut r = reports();
        let memory = Arc::new(RecordingMemory::default());
        r.memory = Some(memory.clone());
        let id = decision(&r, "2026-10-07T10:00:00Z", "done: requeue");
        let view = r
            .overrule(id, "Prefer diagnosis before retry.")
            .await
            .unwrap();
        assert_eq!(view["memory_retained"], true);
        let rows = r.store.load_lead_decisions().unwrap();
        assert_eq!(
            rows[0].overrule_note.as_deref(),
            Some("Prefer diagnosis before retry.")
        );
        let items = r.store.load_lead_items().unwrap();
        assert_eq!(items.len(), 2);
        assert!(format!("{:?}", items[1].trigger).contains("Prefer diagnosis"));
        let remembered = memory.0.lock().unwrap().clone();
        assert_eq!(remembered.len(), 1);
        assert_eq!(remembered[0].identity, "David");
        assert!(remembered[0].content.contains("by: David"));
        assert!(
            remembered[0]
                .content
                .contains("Prefer diagnosis before retry.")
        );
        r.overrule(id, "Prefer diagnosis before retry.")
            .await
            .unwrap();
        assert_eq!(r.store.load_lead_items().unwrap().len(), 2);
        assert_eq!(memory.0.lock().unwrap().len(), 1);
    }
    #[test]
    fn digest_at_configured_time_one_push() {
        let r = reports();
        decision(&r, "2026-10-07T18:00:00Z", "done: requeue");
        let time = |s: &str| chrono::DateTime::parse_from_rfc3339(s).unwrap();
        assert_eq!(
            r.digest(time("2026-10-08T09:29:59+02:00"), "09:30")
                .unwrap(),
            None
        );
        let body = r
            .digest(time("2026-10-08T09:30:00+02:00"), "09:30")
            .unwrap()
            .unwrap();
        assert!(body.contains("1 decisions"));
        assert!(body.contains("0 escalations"));
        assert_eq!(
            r.digest(time("2026-10-08T10:30:00+02:00"), "09:30")
                .unwrap(),
            None
        );
    }
    #[test]
    fn escalation_pages_immediately_with_need() {
        let r = reports();
        decision(
            &r,
            "2026-10-08T10:00:00Z",
            "escalate: needs a B2 console key: scope X, file Y",
        );
        assert_eq!(
            r.pages().unwrap(),
            vec!["TEST-1: needs a B2 console key: scope X, file Y"]
        );
        assert!(r.pages().unwrap().is_empty());
    }
    #[test]
    fn cap_queues_non_escalations_for_tomorrow() {
        let r = reports();
        use rhapsody_store::{LeadExecution, LeadRunReservation};
        let queued = |ticket: &str| {
            let item = r
                .store
                .enqueue_lead_item(
                    &LeadTrigger::BlockedHandoff {
                        ticket: ticket.into(),
                        question: "retry?".into(),
                    },
                    "2026-10-08T00:00:00Z",
                )
                .unwrap();
            r.store
                .save_lead_execution(&LeadExecution {
                    item,
                    ..Default::default()
                })
                .unwrap();
            item
        };
        let a = queued("TEST-1");
        let b = queued("TEST-2");
        assert_eq!(
            r.store
                .reserve_lead_run_daily(a, "o/r#1", 12, "2026-10-08", 1)
                .unwrap(),
            LeadRunReservation::Reserved
        );
        assert_eq!(
            r.store
                .reserve_lead_run_daily(b, "o/r#2", 12, "2026-10-08", 1)
                .unwrap(),
            LeadRunReservation::DailyCap
        );
        assert_eq!(r.store.load_lead_items().unwrap()[1].state, "queued");
        assert_eq!(r.store.lead_execution(b).unwrap().unwrap().run_attempts, 0);
        assert_eq!(
            r.store.manager_budget("o/r#2").unwrap().unwrap().runs_used,
            0
        );
        assert_eq!(
            r.store
                .reserve_lead_run_daily(b, "o/r#2", 12, "2026-10-09", 1)
                .unwrap(),
            LeadRunReservation::Reserved
        );
    }
    #[test]
    fn cap_never_delays_pages() {
        let r = reports();
        assert!(r.store.reserve_lead_report("runs:2026-10-08", 1).unwrap());
        assert!(!r.store.reserve_lead_report("runs:2026-10-08", 1).unwrap());
        let id = r
            .store
            .enqueue_lead_item(
                &LeadTrigger::Escalation {
                    subject: "TEST-2".into(),
                    need: "needs B2 console key: scope X, file Y".into(),
                },
                "2026-10-08T10:00:00Z",
            )
            .unwrap();
        decision(
            &r,
            "2026-10-08T10:00:00Z",
            "escalate: operator login needed",
        );
        assert_eq!(
            r.pages().unwrap(),
            vec![
                "TEST-2: needs B2 console key: scope X, file Y",
                "TEST-1: operator login needed"
            ]
        );
        assert!(r.pages().unwrap().is_empty());
        assert_eq!(
            r.store
                .load_lead_items()
                .unwrap()
                .iter()
                .find(|i| i.id == id)
                .unwrap()
                .state,
            "done"
        );
    }

    #[tokio::test]
    async fn digest_push_has_real_ntfy_click_and_no_repeat() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let channel = Arc::new(crate::breaker::NtfyChannel::new(format!(
            "http://{}/lead",
            listener.local_addr().unwrap()
        )));
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = vec![0; 4096];
            let size = socket.read(&mut bytes).await.unwrap();
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(50), listener.accept())
                    .await
                    .is_err()
            );
            String::from_utf8(bytes[..size].to_vec()).unwrap()
        });
        let r = reports();
        let now = chrono::DateTime::parse_from_rfc3339("2026-10-08T08:00:00+02:00").unwrap();
        let body = r.digest(now, "08:00").unwrap().unwrap();
        send(
            &[channel],
            "Lead daily digest",
            &body,
            "http://127.0.0.1:1234/#lead",
        )
        .await;
        assert!(r.digest(now, "08:00").unwrap().is_none());
        let wire = server.await.unwrap();
        assert!(
            wire.to_ascii_lowercase()
                .contains("click: http://127.0.0.1:1234/#lead")
        );
        assert!(wire.contains("Lead: 0 decisions yesterday, 0 escalations"));
    }

    #[test]
    fn advise_decisions_are_visible_in_the_human_feed() {
        let r = Arc::new(reports());
        decision(&r, "2026-10-08T10:00:00Z", "proposed: requeue");
        let mut o = crate::Orchestrator::new("WORKFLOW.md");
        let mut teams = rhapsody_config::teams::Teams::disabled();
        teams.enabled = true;
        teams.manager.lead.enabled = true;
        o.teams = Some(teams);
        o.lead_reports = Some(r);
        let feed = o.build_snapshot().review_divergence;
        assert_eq!(feed.len(), 1);
        assert!(feed[0].reason.contains("proposed: requeue"));
        assert!(feed[0].reason.contains("The spec supports this."));
    }

    #[tokio::test]
    async fn overruling_an_overrule_preserves_account_subject_resolution() {
        let r = reports();
        let item = r
            .store
            .enqueue_lead_item(
                &LeadTrigger::LimitJudgment {
                    account: "openai-chatgpt".into(),
                },
                "2026-10-08T00:00:00Z",
            )
            .unwrap();
        let first = r
            .store
            .save_lead_decision(&LeadDecisionRow {
                item,
                decision: "done: wait".into(),
                ..Default::default()
            })
            .unwrap();
        let overrule = r.overrule(first, "Switch instead.").await.unwrap()["item"]
            .as_i64()
            .unwrap();
        let second = r
            .store
            .save_lead_decision(&LeadDecisionRow {
                item: overrule,
                decision: "done: switch".into(),
                ..Default::default()
            })
            .unwrap();
        let latest = r.overrule(second, "Wait instead.").await.unwrap()["item"]
            .as_i64()
            .unwrap();
        let queued = r
            .store
            .load_lead_items()
            .unwrap()
            .into_iter()
            .find(|i| i.id == latest)
            .unwrap();
        let (original, context) = crate::leadexec::overrule_context(r.store.as_ref(), &queued)
            .unwrap()
            .unwrap();
        assert_eq!(
            original.trigger,
            LeadTrigger::LimitJudgment {
                account: "openai-chatgpt".into()
            }
        );
        assert_eq!(context.overrule_note.as_deref(), Some("Wait instead."));
        assert!(r.overrule(first, "Change the first note.").await.is_err());
    }

    #[test]
    fn daily_and_delivery_reservations_survive_restart() {
        let dir = crate::testsupport::TempDir::new();
        let path = StorePath::Disk(dir.child("reporting.db").into());
        let store = Sqlite::open(path.clone()).unwrap();
        assert!(store.reserve_lead_report("digest:2026-10-08", 1).unwrap());
        assert!(store.reserve_lead_report("page-decision:7", 1).unwrap());
        assert!(store.reserve_lead_report("runs:2026-10-08", 1).unwrap());
        drop(store);
        let store = Sqlite::open(path).unwrap();
        for key in ["digest:2026-10-08", "page-decision:7", "runs:2026-10-08"] {
            assert!(!store.reserve_lead_report(key, 1).unwrap());
        }
        assert!(store.reserve_lead_report("runs:2026-10-09", 1).unwrap());
    }
}
