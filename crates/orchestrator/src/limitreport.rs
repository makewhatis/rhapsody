//! Account reporting (STUDIO-1128), no Go counterpart. Owned plans leave the control task;
//! tracker and notification I/O happen only on the consumer task.

use crate::Orchestrator;
use crate::limitpolicy::{HandoffOutcome, Level, LimitItem};
use chrono::{DateTime, Utc};
use rhapsody_config::profiles::EngineSpec;
use rhapsody_config::room::RoomLog;
use rhapsody_tracker::Tracker;
use serde::Serialize;
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Serialize, serde::Deserialize)]
pub struct LimitJobReport {
    pub ticket: String,
    pub account: String,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume_at_s: Option<i64>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub identity: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<std::path::PathBuf>,
}

pub(crate) fn local_time(seconds: i64) -> String {
    DateTime::from_timestamp(seconds, 0)
        .map(|t| t.with_timezone(&chrono::Local).to_rfc3339())
        .unwrap_or_else(|| "unknown".into())
}

pub(crate) fn level_label(level: Level) -> &'static str {
    match level {
        Level::Ok => "ok",
        Level::Warn => "warn",
        Level::StopNew => "stop-new",
        Level::Handoff => "handoff",
        Level::Wall => "wall",
    }
}

fn tokenless(mut body: String, token: &str) -> String {
    body = body.replace('@', "");
    if !token.is_empty() {
        // Strip after mentions too: `f@oo` must not become a custom `foo` summons. Repeat
        // to remove tokens formed by joining the remaining data.
        while body.contains(token) {
            body = body.replace(token, "");
        }
    }
    body
}

pub enum Report {
    Push {
        account: String,
        title: String,
        body: String,
    },
    Handoff {
        ticket: String,
        issue_id: String,
        body: String,
        at: DateTime<Utc>,
        tracker: Option<Arc<dyn Tracker>>,
        pr: Option<crate::teamsknow::PrRef>,
    },
}

impl Orchestrator {
    pub fn open_limit_report_channel(&mut self) -> tokio::sync::mpsc::UnboundedReceiver<Report> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.limit_policy.report_tx = Some(tx);
        rx
    }
    pub(crate) fn send_limit_report(&self, report: Report) {
        if let Some(tx) = &self.limit_policy.report_tx
            && tx.send(report).is_err()
        {
            tracing::warn!("limit: reporting task is unavailable");
        }
    }

    pub(crate) fn report_account_transitions(&mut self) {
        let now = (self.now)().timestamp();
        let cfg = self.limits_config();
        for view in self.accounts.snapshot(now) {
            let level = crate::limitpolicy::level(&self.accounts, &view.account, &cfg, now);
            let state = (level, view.stale);
            let old = self
                .limit_policy
                .reported_states
                .insert(view.account.clone(), state);
            if old == Some(state) || (old.is_none() && level == Level::Ok && !view.stale) {
                continue;
            }
            let window = self.accounts.tightest(&view.account, now);
            let utilization = window.as_ref().map_or(0.0, |w| w.utilization);
            let reset = window.as_ref().map_or(0, |w| w.resets_at_s);
            let label = if view.stale {
                "stale"
            } else {
                level_label(level)
            };
            tracing::warn!(account = %view.account, window = window.as_ref().map_or("unknown", |w| w.window.as_str()), utilization, reset, state = label, "limit: account state transition");
            let mut parked = Vec::new();
            let mut switched = Vec::new();
            let mut waiting = 0;
            if level >= Level::Handoff {
                for (id, run) in &self.running {
                    let account = self
                        .accounts
                        .account_for_run(&crate::accounts::run_key(id, run.started_at))
                        .unwrap_or_else(|| run.pricing.account.clone());
                    if account != view.account {
                        continue;
                    }
                    let list = self.engine_list(
                        &run.identity,
                        EngineSpec {
                            harness: self.effective_harness(&run.harness),
                            model: run.model.clone(),
                            effort: run.model_override.effort.clone(),
                        },
                    );
                    let next = list
                        .iter()
                        .enumerate()
                        .skip(run.engine_index.saturating_add(1))
                        .find_map(|(i, e)| self.engine_usable(e, &run.project_slug).then_some(i));
                    let item = LimitItem {
                        account: account.clone(),
                        windows: view.windows.clone(),
                        tickets: Vec::new(),
                        credits_policy: cfg.credits.clone(),
                    };
                    let outcome = if crate::managerrun::is_manager_key(id) {
                        HandoffOutcome::ManagerItem(item)
                    } else {
                        crate::limitpolicy::decide(&self.accounts, item, &cfg, now, next)
                    };
                    match outcome {
                        HandoffOutcome::Park { resume_at_s } => {
                            parked.push(local_time(resume_at_s))
                        }
                        HandoffOutcome::Switch { engine } => {
                            if let Some(spec) = list.get(engine) {
                                switched.push(spec.model.clone());
                            } else {
                                waiting += 1;
                            }
                        }
                        HandoffOutcome::ManagerItem(_) => waiting += 1,
                    }
                }
            }
            let count = parked.len() + switched.len() + waiting;
            let mut details = Vec::new();
            if let Some(time) = parked.first() {
                details.push(format!("{} parked until {time}", parked.len()));
            }
            if !switched.is_empty() {
                details.push(format!(
                    "{} switched to {}",
                    switched.len(),
                    switched.join(", ")
                ));
            }
            if waiting > 0 {
                details.push(format!("{waiting} waiting for a decision"));
            }
            let body = format!(
                "{} {:.0}%: {label}; {count} run{} handing off{}",
                view.account,
                utilization * 100.0,
                if count == 1 { "" } else { "s" },
                if details.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", details.join(", "))
                }
            );
            self.send_limit_report(Report::Push {
                account: view.account.clone(),
                title: format!("{}: {label}", view.account),
                body,
            });
        }
    }

    pub(crate) fn report_limit_handoff(
        &self,
        run: &crate::RunningEntry,
        account: &str,
        outcome: &HandoffOutcome,
        note: Option<&std::path::Path>,
    ) {
        let list = self.engine_list(
            &run.identity,
            EngineSpec {
                harness: self.effective_harness(&run.harness),
                model: run.model.clone(),
                effort: run.model_override.effort.clone(),
            },
        );
        let action = match outcome {
            HandoffOutcome::Park { resume_at_s } => format!(
                "parked on the same engine; resumes {}",
                local_time(*resume_at_s)
            ),
            HandoffOutcome::Switch { engine } => list
                .get(*engine)
                .map(|e| {
                    format!(
                        "switched to {} / {}; resumes on a healthy account when eligible",
                        e.harness, e.model
                    )
                })
                .unwrap_or_else(|| "waiting for a valid fallback engine".into()),
            HandoffOutcome::ManagerItem(_) => {
                "waiting on a limit decision; resumes after a manager or operator resolves it"
                    .into()
            }
        };
        // A model, identity or note path is data, and may itself carry a summon. Removing every
        // mention introducer keeps both brand aliases and a custom configured token inert.
        let body = format!(
            "{}: {} limit; {} on {} / {} → {action}. Handoff note: {}.",
            run.issue.identifier,
            account,
            run.identity,
            self.effective_harness(&run.harness),
            run.model,
            note.map_or_else(
                || "unavailable (human assistance required)".into(),
                |p| p.display().to_string()
            )
        );
        let body = tokenless(
            body,
            self.eff.as_ref().map_or("", |e| e.summon_token.as_str()),
        );
        let tracker = self.eff.as_ref().map(|eff| {
            eff.project_by_slug(&run.project_slug)
                .map_or_else(|| eff.tracker.clone(), |p| p.tracker.clone())
        });
        let (state, resume_at_s, model) = match outcome {
            HandoffOutcome::Park { resume_at_s } => ("parked", Some(*resume_at_s), String::new()),
            HandoffOutcome::Switch { engine } => (
                "switched",
                None,
                list.get(*engine)
                    .map(|e| e.model.clone())
                    .unwrap_or_default(),
            ),
            HandoffOutcome::ManagerItem(_) => ("waiting", None, String::new()),
        };
        let job = LimitJobReport {
            ticket: run.issue.identifier.clone(),
            account: account.into(),
            state: state.into(),
            resume_at_s,
            model,
            identity: run.identity.clone(),
            note: note.map(std::path::Path::to_path_buf),
        };
        match serde_json::to_string(&job) {
            Ok(text) => {
                let row = rhapsody_store::EventRow {
                    seq: run.event_seq + 1,
                    at: crate::persist::rfc3339((self.now)()),
                    kind: "limit.handoff".into(),
                    tool: String::new(),
                    text,
                };
                if let Err(error) = self.store.append_events(run.run_id, &[row]) {
                    tracing::warn!(ticket = %run.issue.identifier, %error, "limit: handoff report could not be recorded");
                }
            }
            Err(error) => tracing::warn!(%error, "limit: handoff report could not be serialized"),
        }
        let pr = if run.review.is_some() || crate::managerrun::is_manager_key(&run.issue.id) {
            crate::teamsknow::parse_pr_ref(&run.issue.identifier)
        } else {
            None
        };
        self.send_limit_report(Report::Handoff {
            ticket: run.issue.identifier.clone(),
            issue_id: run.issue.id.clone(),
            body,
            at: (self.now)(),
            tracker: if pr.is_some() { None } else { tracker },
            pr,
        });
    }

    pub(crate) fn limit_job_reports(&self) -> Vec<LimitJobReport> {
        let mut reports = Vec::new();
        for suspended in self.limit_policy.suspended.values() {
            let run = &suspended.run;
            let (state, resume_at_s, model) = match suspended.outcome {
                HandoffOutcome::Park { resume_at_s } => {
                    ("parked", Some(resume_at_s), String::new())
                }
                HandoffOutcome::Switch { engine } => {
                    let list = self.engine_list(
                        &run.identity,
                        EngineSpec {
                            harness: self.effective_harness(&run.harness),
                            model: run.model.clone(),
                            effort: run.model_override.effort.clone(),
                        },
                    );
                    (
                        "switched",
                        None,
                        list.get(engine)
                            .map(|e| e.model.clone())
                            .unwrap_or_default(),
                    )
                }
                HandoffOutcome::ManagerItem(_) => ("waiting", None, String::new()),
            };
            reports.push(LimitJobReport {
                ticket: run.issue.identifier.clone(),
                account: suspended
                    .account
                    .as_ref()
                    .map(|a| a.account.clone())
                    .unwrap_or_default(),
                state: state.into(),
                resume_at_s,
                model,
                identity: run.identity.clone(),
                note: suspended.note.clone(),
            });
        }
        for run in self.running.values().filter(|r| r.engine_index > 0) {
            reports.push(LimitJobReport {
                ticket: run.issue.identifier.clone(),
                account: run.pricing.account.clone(),
                state: "switched".into(),
                resume_at_s: None,
                model: run.model.clone(),
                identity: run.identity.clone(),
                note: run.engine.as_ref().and_then(|e| e.handoff_note.clone()),
            });
        }
        for held in self
            .budget_ledger
            .held(self.budget_hold_ttl())
            .iter()
            .filter(|h| h.reason.starts_with("waiting:") && h.reason.contains(" limit"))
        {
            reports.push(LimitJobReport {
                ticket: held.subject.clone(),
                account: held.provider.clone(),
                state: "waiting".into(),
                resume_at_s: None,
                model: String::new(),
                identity: String::new(),
                note: None,
            });
        }
        for (ticket, reason) in &self.limit_policy.holds {
            if reports.iter().any(|r| &r.ticket == ticket) {
                continue;
            }
            let account = reason
                .strip_prefix("waiting: ")
                .and_then(|r| r.split_once(" limit,"))
                .map(|(account, _)| account)
                .unwrap_or("unknown");
            reports.push(LimitJobReport {
                ticket: ticket.clone(),
                account: account.into(),
                state: "waiting".into(),
                resume_at_s: None,
                model: String::new(),
                identity: String::new(),
                note: None,
            });
        }
        reports.sort_by(|a, b| a.ticket.cmp(&b.ticket));
        reports
    }

    /// A manager's relabel is only an intention. Report an identity handoff when the shared
    /// dispatch funnel actually starts its next run, and only immediately after a limit stop.
    pub(crate) fn report_identity_limit_handoff(&self, run: &mut crate::RunningEntry) {
        if run.run_id == 0
            || run.identity.is_empty()
            || run.review.is_some()
            || crate::managerrun::is_manager_key(&run.issue.id)
        {
            return;
        }
        let rows = match self
            .store
            .issue_history(&run.issue.identifier, &run.project_slug, 2)
        {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!(%error, "limit: previous run could not be read for identity reporting");
                return;
            }
        };
        let Some(previous) = rows.iter().find(|r| r.id != run.run_id) else {
            return;
        };
        if previous.outcome != rhapsody_store::OUTCOME_LIMIT {
            return;
        }
        let hits = match self.store.search_events(rhapsody_store::EventQuery {
            issue: run.issue.identifier.clone(),
            kind: "limit.handoff".into(),
            limit: 1,
            ..Default::default()
        }) {
            Ok(hits) => hits,
            Err(error) => {
                tracing::warn!(%error, "limit: previous handoff could not be read");
                return;
            }
        };
        let Some(hit) = hits.first().filter(|h| h.run_id == previous.id) else {
            return;
        };
        let mut job: LimitJobReport = match serde_json::from_str(&hit.text) {
            Ok(job) => job,
            Err(error) => {
                tracing::warn!(%error, "limit: previous handoff report is unreadable");
                return;
            }
        };
        if job.identity.is_empty() || job.identity == run.identity {
            return;
        }
        let old_identity = job.identity.clone();
        job.identity = run.identity.clone();
        job.state = "handed_off".into();
        job.resume_at_s = Some((self.now)().timestamp());
        job.model = run.model.clone();
        let text = match serde_json::to_string(&job) {
            Ok(text) => text,
            Err(error) => {
                tracing::warn!(%error, "limit: identity handoff could not be serialized");
                return;
            }
        };
        run.event_seq += 1;
        if let Err(error) = self.store.append_events(
            run.run_id,
            &[rhapsody_store::EventRow {
                seq: run.event_seq,
                at: crate::persist::rfc3339((self.now)()),
                kind: "limit.handoff".into(),
                text,
                ..Default::default()
            }],
        ) {
            tracing::warn!(%error, "limit: identity handoff could not be recorded");
        }
        let body = format!(
            "{}: {} limit handoff; handed from {} to {} on {} / {}; resumes as run {} at {}. Handoff note: {}.",
            run.issue.identifier,
            job.account,
            old_identity,
            run.identity,
            self.effective_harness(&run.harness),
            run.model,
            run.run_id,
            local_time((self.now)().timestamp()),
            job.note
                .as_ref()
                .map_or_else(|| "unavailable".into(), |p| p.display().to_string())
        );
        let tracker = self.eff.as_ref().map(|eff| {
            eff.project_by_slug(&run.project_slug)
                .map_or_else(|| eff.tracker.clone(), |p| p.tracker.clone())
        });
        self.send_limit_report(Report::Handoff {
            ticket: run.issue.identifier.clone(),
            issue_id: run.issue.id.clone(),
            body: tokenless(
                body,
                self.eff.as_ref().map_or("", |e| e.summon_token.as_str()),
            ),
            at: (self.now)(),
            tracker,
            pr: None,
        });
    }
}

pub async fn perform_report(
    report: Report,
    room: Option<&dyn RoomLog>,
    channels: &[Arc<dyn crate::breaker::NotifyChannel>],
    comments: Option<&dyn crate::ghsummons::PrCommentSink>,
) {
    match report {
        Report::Push {
            account,
            title,
            body,
        } => {
            for channel in channels {
                if let Err(error) = tokio::time::timeout(
                    std::time::Duration::from_secs(15),
                    channel.send_account(&title, &body),
                )
                .await
                .unwrap_or_else(|_| Err("notification timed out".into()))
                {
                    tracing::warn!(%account, channel = channel.name(), %error, "limit: push could not be delivered");
                }
            }
        }
        Report::Handoff {
            ticket,
            issue_id,
            body,
            at,
            tracker,
            pr,
        } => {
            if let Some(room) = room {
                let mut message = rhapsody_config::room::Message::room("manager", at, body.clone());
                message.refs = vec![ticket.clone()];
                if let Err(error) = room.append(&message) {
                    tracing::warn!(%ticket, %error, "limit: room handoff could not be posted");
                }
            }
            if let Some(tracker) = tracker {
                match tokio::time::timeout(
                    std::time::Duration::from_secs(15),
                    tracker.create_comment(&issue_id, &body),
                )
                .await
                {
                    Ok(Ok(_)) => {}
                    Ok(Err(error)) => {
                        tracing::warn!(%ticket, %error, "limit: tracker handoff comment could not be posted")
                    }
                    Err(_) => tracing::warn!(%ticket, "limit: tracker handoff comment timed out"),
                }
            }
            if let (Some(pr), Some(comments)) = (pr, comments) {
                match tokio::time::timeout(
                    std::time::Duration::from_secs(15),
                    comments.post_pr_comment(&pr.owner, &pr.repo, pr.number, &body),
                )
                .await
                {
                    Ok(Ok(_)) => {}
                    Ok(Err(error)) => {
                        tracing::warn!(%ticket, %error, "limit: PR handoff comment could not be posted")
                    }
                    Err(_) => tracing::warn!(%ticket, "limit: PR handoff comment timed out"),
                }
            }
        }
    }
}

pub async fn run_limit_report_task(
    mut ctx: crate::control_loop::CancelWait,
    room: Option<Arc<dyn RoomLog>>,
    channels: Vec<Arc<dyn crate::breaker::NotifyChannel>>,
    comments: Arc<dyn crate::ghsummons::PrCommentSink>,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<Report>,
) {
    loop {
        let report = tokio::select! { _ = ctx.cancelled() => return, report = rx.recv() => match report { Some(report) => report, None => return } };
        perform_report(report, room.as_deref(), &channels, Some(comments.as_ref())).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::breaker::{MacosChannel, NotificationsState, NotifyChannel, NtfyChannel};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn account_push_reaches_ntfy_and_macos_independently() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/limits", listener.local_addr().unwrap());
        let request = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = vec![0; 4096];
            let len = socket.read(&mut bytes).await.unwrap();
            socket
                .write_all(
                    b"HTTP/1.1 503 Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            String::from_utf8(bytes[..len].to_vec()).unwrap()
        });
        let state = Arc::new(NotificationsState::default());
        let channels: Vec<Arc<dyn NotifyChannel>> = vec![
            Arc::new(NtfyChannel::new(url)),
            Arc::new(MacosChannel::new(state.clone())),
        ];
        perform_report(
            Report::Push {
                account: "claude-subscription".into(),
                title: "Claude: handoff".into(),
                body: "Claude 95%: 3 runs handing off".into(),
            },
            None,
            &channels,
            None,
        )
        .await;
        let wire = request.await.unwrap();
        assert!(wire.to_ascii_lowercase().contains("title: claude: handoff"));
        assert!(wire.contains("Claude 95%: 3 runs handing off"));
        let notifications = state.pending();
        assert_eq!(
            notifications.len(),
            1,
            "a failed ntfy channel never hides the macOS push"
        );
        assert_eq!(notifications[0].title, "Claude: handoff");
        assert!(
            notifications[0].ticket.is_empty(),
            "an account is not a ticket"
        );
    }

    #[derive(Default)]
    struct Comments(std::sync::Mutex<Vec<String>>);
    #[async_trait::async_trait]
    impl crate::ghsummons::PrCommentSink for Comments {
        async fn post_pr_comment(
            &self,
            owner: &str,
            repo: &str,
            number: i64,
            body: &str,
        ) -> crate::ghsummons::PrCommentResult {
            assert_eq!((owner, repo, number), ("owner", "repo", 7));
            self.0.lock().unwrap().push(body.into());
            Ok(())
        }
    }

    #[tokio::test]
    async fn synthetic_review_handoff_comments_on_its_pr() {
        let comments = Comments::default();
        let report = Report::Handoff {
            ticket: "pr:owner/repo#7@alice".into(),
            issue_id: "synthetic".into(),
            body: "review parked; resumes at reset; note path".into(),
            at: Utc::now(),
            tracker: None,
            pr: crate::teamsknow::parse_pr_ref("pr:owner/repo#7@alice"),
        };
        perform_report(report, None, &[], Some(&comments)).await;
        let calls = comments.0.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].contains("resumes"));
    }
}
