//! Daemon-owned notification centre and acknowledgements (STUDIO-1145). No Go counterpart.
//! Reads project existing local snapshots/ledgers into compact notices, never tracker/model I/O.

use crate::{
    handlers::{require_get, require_post},
    logs::LogSource,
    responses::{write_error, write_json},
    server::StateProvider,
};
use axum::{
    extract::{Path, State},
    http::{Method, StatusCode},
    response::Response,
};
use rhapsody_store::{EventQuery, Notice, StoreError};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    LeadEscalation,
    StuckPr,
    HumanHold,
    LimitDecision,
    LeadDecision,
    ManagerDisabled,
    LoginCheck,
    BudgetWall,
    LimitWall,
    AutoMerge,
    RcCut,
    LimitPark,
    LimitSwitch,
    CreditSpend,
}
impl Kind {
    fn group(self) -> &'static str {
        match self {
            Self::LeadEscalation | Self::StuckPr | Self::HumanHold | Self::LimitDecision => {
                "needs_you"
            }
            Self::LeadDecision => "decisions",
            Self::ManagerDisabled | Self::LoginCheck | Self::BudgetWall | Self::LimitWall => {
                "system"
            }
            Self::AutoMerge
            | Self::RcCut
            | Self::LimitPark
            | Self::LimitSwitch
            | Self::CreditSpend => "activity",
        }
    }
    fn name(self) -> &'static str {
        match self {
            Self::LeadEscalation => "lead_escalation",
            Self::StuckPr => "stuck_pr",
            Self::HumanHold => "human_hold",
            Self::LimitDecision => "limit_decision",
            Self::LeadDecision => "lead_decision",
            Self::ManagerDisabled => "manager_disabled",
            Self::LoginCheck => "login_check",
            Self::BudgetWall => "budget_wall",
            Self::LimitWall => "limit_wall",
            Self::AutoMerge => "auto_merge",
            Self::RcCut => "rc_cut",
            Self::LimitPark => "limit_park",
            Self::LimitSwitch => "limit_switch",
            Self::CreditSpend => "credit_spend",
        }
    }
}
fn compact(text: &str) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() <= 140 {
        line
    } else {
        format!("{}…", line.chars().take(139).collect::<String>())
    }
}
fn notice(
    kind: Kind,
    source: String,
    subject: &str,
    summary: &str,
    href: String,
    at: &str,
    transient: bool,
) -> Notice {
    Notice {
        source,
        kind: kind.name().into(),
        group: kind.group().into(),
        subject: compact(subject),
        summary: compact(summary),
        href,
        at: at.into(),
        transient,
        active: true,
        ..Default::default()
    }
}
fn job_link(subject: &str) -> String {
    format!("#job/{subject}")
}
fn pr_link(subject: &str) -> String {
    if let Some((repo, number)) = subject.rsplit_once('#')
        && number.parse::<u64>().is_ok_and(|n| n > 0)
        && repo.split('/').count() == 2
        && repo
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./".contains(c))
    {
        return format!("https://github.com/{repo}/pull/{number}");
    }
    job_link(subject)
}

async fn observe(
    provider: &dyn StateProvider,
    logs: Option<&dyn LogSource>,
) -> Result<Vec<Notice>, String> {
    let snapshot = provider.snapshot().await.map_err(|e| e.to_string())?;
    let at = rhapsody_store::format_summon_at(snapshot.generated_at);
    let mut rows = Vec::new();
    if provider.teams_enabled() {
        let teams = provider.teams_overview().await.map_err(|e| e.to_string())?;
        if teams.manager_mode == "off" {
            rows.push(notice(
                Kind::ManagerDisabled,
                "manager:routing-off".into(),
                "Routing manager",
                "Automatic routing is disabled",
                "#teams".into(),
                &at,
                true,
            ));
        }
    }
    if let Some(reports) = provider.lead_reports() {
        let view = reports.decisions("")?;
        for decision in view["decisions"].as_array().into_iter().flatten() {
            let text = decision["decision"].as_str().unwrap_or_default();
            if text == "applying" || !decision["overruled_at"].is_null() {
                continue;
            }
            let kind = if text.starts_with("escalate:") {
                Kind::LeadEscalation
            } else {
                Kind::LeadDecision
            };
            let subject = decision["subject"].as_str().unwrap_or_default();
            let id = decision["id"].as_i64().ok_or("lead decision id missing")?;
            let verb = if kind == Kind::LeadEscalation {
                "Lead escalated"
            } else if text.starts_with("proposed:") {
                "Lead proposed"
            } else {
                "Lead decided"
            };
            let summary = text.split_once(':').map_or(text, |(_, s)| s.trim());
            rows.push(notice(
                kind,
                format!("lead:{id}"),
                subject,
                &format!("{verb}: {summary}"),
                format!("#lead/decision-{id}"),
                decision["at"].as_str().unwrap_or(&at),
                kind == Kind::LeadEscalation,
            ));
        }
    }
    for d in &snapshot.review_divergence {
        // Ignore only the host's legacy lead projection, not a real reviewer named lead.
        if d.reviewer == "lead"
            && d.kind.as_str() == "manager_deferred"
            && d.reason.starts_with("Lead decision ")
        {
            continue;
        }
        let system = d.capacity_held.is_some() || d.kind.as_str() == "manager_deferred";
        let kind = if system {
            Kind::ManagerDisabled
        } else {
            Kind::StuckPr
        };
        let summary = if d.reason.is_empty() {
            d.kind.detail()
        } else {
            &d.reason
        };
        let summary = if d.capacity_held.is_some() {
            "Review waiting for capacity"
        } else {
            summary
        };
        let summary = if d.superseded() {
            format!("Earlier escalation superseded by a new head: {summary}")
        } else {
            summary.to_string()
        };
        let when =
            snapshot.generated_at - chrono::Duration::seconds(d.stale_secs.clamp(0, 315_360_000));
        rows.push(notice(
            kind,
            format!(
                "pr:{}:{}:{}:{}",
                d.pr,
                d.reviewer,
                d.kind.as_str(),
                d.adjudicated_head
            ),
            &d.pr,
            &summary,
            pr_link(&d.pr),
            &rhapsody_store::format_summon_at(when),
            true,
        ));
    }
    for held in &snapshot.held_for_human {
        // An approved PR's human hold is already represented by its PR notice.
        if snapshot
            .review_divergence
            .iter()
            .any(|d| d.ticket == held.issue_identifier && d.kind.as_str() == "held_for_human")
        {
            continue;
        }
        rows.push(notice(
            Kind::HumanHold,
            format!("hold:{}", held.issue_identifier),
            &held.issue_identifier,
            "Held for a human step",
            job_link(&held.issue_identifier),
            &at,
            true,
        ));
    }
    for item in &snapshot.limit_items {
        rows.push(notice(
            Kind::LimitDecision,
            format!("limit-decision:{}", item.account),
            &item.account,
            "Limit needs a human decision; no automatic switch is available",
            "#accounts".into(),
            &at,
            true,
        ));
    }
    for budget in &snapshot.budget_held {
        rows.push(notice(
            Kind::BudgetWall,
            format!("budget:{}:{}", budget.provider, budget.subject),
            &budget.provider,
            "Daily budget reached; new work is held",
            "#accounts".into(),
            &at,
            true,
        ));
    }
    for account in provider.accounts() {
        if matches!(account.status.as_str(), "rejected" | "warning")
            || account.level.as_deref().is_some_and(|s| s != "ok")
        {
            rows.push(notice(
                Kind::LimitWall,
                format!(
                    "account:{}:{}:{}",
                    account.account,
                    account.status,
                    account.level.as_deref().unwrap_or("ok")
                ),
                &account.account,
                &format!(
                    "Account limit: {}",
                    account.level.as_deref().unwrap_or(&account.status)
                ),
                "#accounts".into(),
                &at,
                true,
            ));
        }
    }
    for failure in provider.manager_notices() {
        rows.push(notice(
            Kind::LoginCheck,
            format!("manager:{failure}"),
            "Manager",
            &failure,
            "#accounts".into(),
            &at,
            true,
        ));
    }
    // These are already-recorded events. Query each known kind independently: a busy
    // run's tool events must not crowd notifications out of a generic recent-event page.
    for (event, kind) in [
        ("teams.merge", Kind::AutoMerge),
        ("release.rc_cut", Kind::RcCut),
        ("limit.handoff", Kind::LimitPark),
        ("limit.credit_spend", Kind::CreditSpend),
    ] {
        for hit in provider
            .history()
            .search_events(EventQuery {
                kind: event.into(),
                limit: 200,
                ..Default::default()
            })
            .map_err(|e| e.to_string())?
        {
            let (kind, summary) = if event == "limit.handoff" {
                let report: rhapsody_orchestrator::limitreport::LimitJobReport =
                    match serde_json::from_str(&hit.text) {
                        Ok(report) => report,
                        Err(error) => {
                            tracing::warn!(%error, run = hit.run_id, "invalid limit handoff notice");
                            continue;
                        }
                    };
                if report.state == "switched" {
                    (
                        Kind::LimitSwitch,
                        format!("Limit switched engine to {}", report.model),
                    )
                } else if report.state == "parked" {
                    (
                        Kind::LimitPark,
                        "Limit parked until the account resets".into(),
                    )
                } else {
                    (Kind::LimitPark, "Limit paused for a decision".into())
                }
            } else if kind == Kind::CreditSpend {
                (kind, "Account credits in use".into())
            } else {
                (kind, hit.text.clone())
            };
            let subject = if hit.issue_identifier.starts_with("lead:") {
                "Lead"
            } else {
                &hit.issue_identifier
            };
            rows.push(notice(
                kind,
                format!("event:{}:{}", hit.run_id, hit.seq),
                subject,
                &summary,
                job_link(subject),
                &hit.at,
                false,
            ));
        }
    }
    // Auto-merge's existing producer records structured audit logs rather than run events.
    // Capture those exact success records, never infer a merge from arbitrary log prose.
    if let Some(logs) = logs {
        for log in logs
            .snapshot()
            .into_iter()
            .filter(|log| log.msg == "auto-merge: merged")
        {
            let Some(pr) = log.attrs.get("pr") else {
                continue;
            };
            rows.push(notice(
                Kind::AutoMerge,
                format!("log:{}:{}", logs.epoch(), log.seq),
                pr,
                "Automatically merged after review",
                pr_link(pr),
                &log.time,
                false,
            ));
        }
    }
    Ok(rows)
}

pub(crate) async fn handle_notifications(
    State(provider): State<Arc<dyn StateProvider>>,
    State(logs): State<Option<Arc<dyn LogSource>>>,
    method: Method,
) -> Response {
    if let Some(response) = require_get(&method) {
        return response;
    }
    let Some(store) = provider.notification_store() else {
        return write_json(StatusCode::OK, &serde_json::json!({"notifications": []}));
    };
    // Admission timestamp, not completion time: an older concurrent GET must not
    // resolve/reopen episodes after a newer snapshot has already committed.
    let at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    let rows = match observe(provider.as_ref(), logs.as_deref()).await {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(%error, "notifications snapshot unavailable");
            return write_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "notifications_unavailable",
                "Notifications could not be refreshed",
                None,
            );
        }
    };
    match store
        .observe_notices(&rows, &at)
        .and_then(|()| store.notices())
    {
        Ok(rows) => write_json(
            StatusCode::OK,
            &serde_json::json!({"notifications": rows.iter().map(|n| serde_json::json!({
            "id": n.id, "kind": n.kind, "group": n.group, "subject": n.subject,
            "summary": n.summary, "href": n.href, "at": n.at, "read_at": n.read_at, "active": n.active,
        })).collect::<Vec<_>>()}),
        ),
        Err(error) => unavailable(error),
    }
}
fn unavailable(error: StoreError) -> Response {
    tracing::warn!(%error, "notification store unavailable");
    write_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "notifications_unavailable",
        "Notification state could not be saved or read",
        None,
    )
}
pub(crate) async fn handle_read(
    State(provider): State<Arc<dyn StateProvider>>,
    Path(id): Path<String>,
    method: Method,
) -> Response {
    if let Some(response) = require_post(&method, "use POST to mark a notification read") {
        return response;
    }
    let Ok(id) = id.parse::<i64>() else {
        return write_error(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "provide a positive notification id",
            None,
        );
    };
    if id <= 0 {
        return write_error(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "provide a positive notification id",
            None,
        );
    }
    let Some(store) = provider.notification_store() else {
        return write_error(
            StatusCode::NOT_FOUND,
            "not_found",
            "notification not found",
            None,
        );
    };
    match store.read_notice(id, &rhapsody_store::format_summon_at(chrono::Utc::now())) {
        Ok(true) => write_json(StatusCode::OK, &serde_json::json!({"id": id, "read": true})),
        Ok(false) => write_error(
            StatusCode::NOT_FOUND,
            "not_found",
            "notification not found",
            None,
        ),
        Err(error) => unavailable(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{FakeProvider, empty_snapshot, spawn_router};
    use rhapsody_store::{EventRow, LeadDecisionRow, LeadTrigger, RunStart, Store};
    use rhapsody_store::{Sqlite, StorePath};
    fn divergence(
        kind: rhapsody_orchestrator::reviewreconcile::DivergenceKind,
    ) -> rhapsody_orchestrator::reviewreconcile::Divergence {
        rhapsody_orchestrator::reviewreconcile::Divergence {
            pr: "owner/repo#7".into(),
            kind,
            ticket: "TEST-7".into(),
            reviewer: "lead".into(),
            stale_secs: 3600,
            auto_merge_reason: None,
            capacity_held: None,
            capacity_unreadable: None,
            adjudicated_head: "head".into(),
            current_head: "head".into(),
            rounds: 0,
            findings: vec![],
            reason: String::new(),
        }
    }
    #[tokio::test]
    async fn projects_real_lead_pr_system_and_activity_sources_without_legacy_duplicates_or_reasoning()
     {
        use rhapsody_orchestrator::reviewreconcile::DivergenceKind as D;
        let store = Arc::new(Sqlite::open(StorePath::InMemory).unwrap());
        let item = store
            .enqueue_lead_item(
                &LeadTrigger::BlockedHandoff {
                    ticket: "TEST-1".into(),
                    question: "retry?".into(),
                },
                "2026-10-08T10:00:00Z",
            )
            .unwrap();
        for decision in [
            "done: requeue",
            "proposed: commission",
            "escalate: needs login",
            "applying",
        ] {
            store
                .save_lead_decision(&LeadDecisionRow {
                    item,
                    at: "2026-10-08T10:00:00Z".into(),
                    decision: decision.into(),
                    reasoning: "Secretly a full paragraph that must never appear in the panel"
                        .into(),
                    ..Default::default()
                })
                .unwrap();
        }
        let run = store
            .start_run(RunStart {
                issue_identifier: "TEST-9".into(),
                ..Default::default()
            })
            .unwrap();
        for (seq, kind, text) in [
            (1, "teams.merge", "merged TEST-9"),
            (2, "release.rc_cut", "RC v1 cut"),
            (
                3,
                "limit.handoff",
                r#"{"ticket":"TEST-9","account":"test","state":"parked","resume_at_s":123,"note":null}"#,
            ),
            (
                4,
                "limit.handoff",
                r#"{"ticket":"TEST-9","account":"test","state":"switched","model":"new-model","note":"/a/path/containing/park"}"#,
            ),
            (5, "limit.credit_spend", "credits in use"),
        ] {
            store
                .append_events(
                    run,
                    &[EventRow {
                        seq,
                        kind: kind.into(),
                        text: text.into(),
                        at: "2026-10-08T10:00:00Z".into(),
                        ..Default::default()
                    }],
                )
                .unwrap();
        }
        let mut snapshot = empty_snapshot();
        for kind in [
            D::ChangesRequestedNoRun,
            D::ReviewRequestedNoRun,
            D::AuthorTokenCeilingStopped,
            D::ReviewTokenCeilingStopped,
            D::ApprovedStillOpen,
            D::HeldForHuman,
            D::RoundBudgetExhausted,
            D::ReviewEscalated,
            D::ReviewShipped,
            D::MergedTicketNotTerminal,
            D::ReviewInfrastructure,
            D::ManagerDeferred,
        ] {
            snapshot.review_divergence.push(divergence(kind));
        }
        let mut legacy = divergence(D::ManagerDeferred);
        legacy.reason =
            "Lead decision 3 — escalate: needs login. Reasoning: a full paragraph".into();
        snapshot.review_divergence.push(legacy);
        snapshot
            .budget_held
            .push(rhapsody_orchestrator::budget::BudgetHeld {
                provider: "test".into(),
                subject: "TEST-9".into(),
                ..Default::default()
            });
        snapshot
            .limit_items
            .push(rhapsody_orchestrator::limitpolicy::LimitItem {
                account: "test-account".into(),
                windows: vec![],
                tickets: vec![],
                credits_policy: "never".into(),
                resets_at_s: 0,
                budgets: Default::default(),
                manager_status: String::new(),
                proposal: None,
                manager_runs: 0,
            });
        let provider = FakeProvider::ok(snapshot)
            .with_history(store.clone())
            .with_accounts(vec![rhapsody_orchestrator::accounts::AccountView {
                account: "test-account".into(),
                windows: vec![],
                status: "rejected".into(),
                using_credits: false,
                last_seen_s: 123,
                source: "stream".into(),
                stale: false,
                detection: "known".into(),
                level: Some("wall".into()),
                today_usd: None,
                cost_kind: None,
            }])
            .with_lead_reports(Arc::new(rhapsody_orchestrator::leadreport::LeadReports {
                store,
                memory: None,
            }));
        let rows = observe(&provider, None).await.unwrap();
        assert_eq!(rows.iter().filter(|n| n.kind == "lead_decision").count(), 2);
        assert_eq!(
            rows.iter().filter(|n| n.kind == "lead_escalation").count(),
            1
        );
        assert_eq!(rows.iter().filter(|n| n.kind == "stuck_pr").count(), 11);
        assert_eq!(rows.iter().filter(|n| n.group == "system").count(), 3);
        assert_eq!(rows.iter().filter(|n| n.group == "activity").count(), 5);
        assert!(
            rows.iter()
                .any(|n| n.kind == "limit_decision" && n.group == "needs_you")
        );
        assert!(rows.iter().any(|n| n.kind == "limit_switch"));
        assert!(
            rows.iter()
                .any(|n| n.href == "https://github.com/owner/repo/pull/7")
        );
        assert!(
            !rows
                .iter()
                .any(|n| n.summary.contains("paragraph") || n.summary.contains("Reasoning"))
        );
        assert!(
            rows.iter()
                .all(|n| n.summary.chars().count() <= 140 && !n.summary.contains('\n'))
        );
    }
    #[test]
    fn every_current_notice_kind_has_exactly_one_home() {
        for (kinds, group) in [
            (
                vec![
                    Kind::LeadEscalation,
                    Kind::StuckPr,
                    Kind::HumanHold,
                    Kind::LimitDecision,
                ],
                "needs_you",
            ),
            (vec![Kind::LeadDecision], "decisions"),
            (
                vec![
                    Kind::ManagerDisabled,
                    Kind::LoginCheck,
                    Kind::BudgetWall,
                    Kind::LimitWall,
                ],
                "system",
            ),
            (
                vec![
                    Kind::AutoMerge,
                    Kind::RcCut,
                    Kind::LimitPark,
                    Kind::LimitSwitch,
                    Kind::CreditSpend,
                ],
                "activity",
            ),
        ] {
            for kind in kinds {
                assert_eq!(kind.group(), group, "{kind:?}");
            }
        }
    }
    #[tokio::test]
    async fn read_state_round_trips_between_independent_clients_without_deleting_the_notice() {
        let store = Arc::new(Sqlite::open(StorePath::InMemory).unwrap());
        let mut snapshot = empty_snapshot();
        snapshot
            .held_for_human
            .push(rhapsody_orchestrator::dispatch::HeldForHuman {
                issue_identifier: "TEST-1".into(),
                title: "Login".into(),
                project: "test".into(),
            });
        let url = spawn_router(crate::new_handler(
            Arc::new(FakeProvider::ok(snapshot).with_notification_store(store.clone())),
            None,
        ))
        .await;
        let path = format!("{url}/api/v1/notifications");
        let read = |path: String| async move {
            reqwest::get(path)
                .await
                .unwrap()
                .json::<serde_json::Value>()
                .await
                .unwrap()
        };
        let browser = read(path.clone()).await;
        assert_eq!(browser["notifications"][0]["group"], "needs_you");
        assert!(browser["notifications"][0]["read_at"].is_null());
        let id = browser["notifications"][0]["id"].as_i64().unwrap();
        let write = format!("{path}/{id}/read");
        let desktop = reqwest::Client::new();
        assert_eq!(
            desktop.post(&write).send().await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
        for _ in 0..2 {
            assert_eq!(
                desktop
                    .post(&write)
                    .header("X-Rhapsody-Operator", "1")
                    .json(&serde_json::json!({}))
                    .send()
                    .await
                    .unwrap()
                    .status(),
                StatusCode::OK
            );
        }
        let browser = read(path.clone()).await;
        assert_eq!(browser["notifications"].as_array().unwrap().len(), 1);
        assert!(browser["notifications"][0]["read_at"].is_string());
        assert_eq!(browser["notifications"][0]["id"], id);
        assert_eq!(
            reqwest::get(&write).await.unwrap().status(),
            StatusCode::METHOD_NOT_ALLOWED
        );
        assert_eq!(
            desktop
                .post(format!("{path}/999/read"))
                .header("X-Rhapsody-Operator", "1")
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn failed_snapshot_preserves_unread_notices_instead_of_treating_failure_as_resolution() {
        let store = Arc::new(Sqlite::open(StorePath::InMemory).unwrap());
        store
            .observe_notices(
                &[notice(
                    Kind::StuckPr,
                    "pr:test".into(),
                    "owner/repo#7",
                    "Held",
                    pr_link("owner/repo#7"),
                    "2026-10-08T10:00:00Z",
                    true,
                )],
                "2026-10-08T10:00:00Z",
            )
            .unwrap();
        let url = spawn_router(crate::new_handler(
            Arc::new(FakeProvider::failing("gone").with_notification_store(store.clone())),
            None,
        ))
        .await;
        assert_eq!(
            reqwest::get(format!("{url}/api/v1/notifications"))
                .await
                .unwrap()
                .status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        let rows = store.notices().unwrap();
        assert!(rows[0].read_at.is_none());
        assert!(rows[0].active);
    }

    #[tokio::test]
    async fn auto_merge_activity_uses_only_the_existing_structured_success_log() {
        struct Logs;
        impl LogSource for Logs {
            fn snapshot(&self) -> Vec<crate::logs::LogEntry> {
                [
                    "auto-merge: merged",
                    "auto-merge: declined",
                    "an agent said auto-merge: merged",
                ]
                .into_iter()
                .enumerate()
                .map(|(seq, msg)| crate::logs::LogEntry {
                    seq: seq as u64,
                    time: "2026-10-08T10:00:00Z".into(),
                    level: "INFO".into(),
                    msg: msg.into(),
                    attrs: [("pr".into(), "owner/repo#7".into())].into_iter().collect(),
                })
                .collect()
            }
            fn epoch(&self) -> u64 {
                42
            }
            fn subscribe(&self) -> tokio::sync::broadcast::Receiver<crate::logs::LogEntry> {
                tokio::sync::broadcast::channel(1).1
            }
        }
        let provider = FakeProvider::ok(empty_snapshot());
        let rows = observe(&provider, Some(&Logs)).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].group, "activity");
        assert_eq!(rows[0].kind, "auto_merge");
        assert_eq!(rows[0].href, "https://github.com/owner/repo/pull/7");
    }
}
