//! leaditems — tech-lead triggers and durable queue (STUDIO-1134). No Go counterpart.
//! Durable subject admission guards and tech-lead triggers; no model judgment on the control task.

use rhapsody_store::Store;
pub use rhapsody_store::{LeadItem, LeadTrigger};

use crate::orchestrator::Orchestrator;
use crate::reviewreconcile::{Divergence, DivergenceKind};

#[derive(Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct ParkSnapshot {
    description: Option<String>,
    state: Option<String>,
    human: bool,
    heads: std::collections::BTreeMap<String, String>,
}

pub(crate) fn park_snapshot(
    store: &dyn Store,
    issue: &rhapsody_core::Issue,
    pr_head: Option<(&str, &str)>,
    blocked: bool,
) -> Result<String, String> {
    let mut snapshot = ParkSnapshot {
        description: Some(issue.description.clone().unwrap_or_default()),
        // A blocked ending can precede its own handoff state move. Establish the state baseline
        // at the next authoritative poll rather than treating that daemon move as operator action.
        state: (!blocked && !issue.state.is_empty()).then(|| issue.state.clone()),
        human: crate::teams::is_human(issue),
        ..Default::default()
    };
    for link in issue.linked_prs.iter().flatten() {
        let pr = format!("{}/{}#{}", link.owner, link.repo, link.number);
        if !issue.identifier.is_empty() {
            store
                .link_subject_pr(&issue.identifier, &pr)
                .map_err(|e| e.to_string())?;
        }
    }
    if let Some((pr, _)) = pr_head
        && !issue.identifier.is_empty()
    {
        store
            .link_subject_pr(&issue.identifier, pr)
            .map_err(|e| e.to_string())?;
    }
    for row in store.load_review_watch().map_err(|e| e.to_string())? {
        if crate::reviewdone::origin_ticket(&row.introduced_by)
            .is_some_and(|t| t.eq_ignore_ascii_case(&issue.identifier))
        {
            let head = if row.requested_sha.is_empty() {
                row.last_reviewed_sha
            } else {
                row.requested_sha
            };
            if !head.is_empty() {
                snapshot.heads.insert(
                    format!("{}/{}#{}", row.key.owner, row.key.repo, row.key.number)
                        .to_ascii_lowercase(),
                    head,
                );
            }
        }
    }
    if let Some((pr, head)) = pr_head.filter(|(_, head)| !head.is_empty()) {
        snapshot.heads.insert(pr.to_ascii_lowercase(), head.into());
    }
    serde_json::to_string(&snapshot).map_err(|e| e.to_string())
}

/// Bounded off-loop read, with a ticket-only filename; symlink notes are refused.
pub(crate) async fn read_progress(dir: Option<&std::path::Path>, ticket: &str) -> Option<String> {
    if ticket.is_empty()
        || !ticket
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return None;
    }
    let path = dir?.join(format!("{ticket}-progress.md"));
    let ticket = ticket.to_string();
    tokio::task::spawn_blocking(move || {
        use std::io::Read;
        let meta = std::fs::symlink_metadata(&path).ok()?;
        if !meta.file_type().is_file() || meta.len() > 128 * 1024 {
            tracing::warn!(ticket, "lead: progress note is not a bounded regular file; using final text only");
            return None;
        }
        let result = std::fs::File::open(&path).and_then(|file| {
            let mut text = String::new();
            file.take(128 * 1024 + 1).read_to_string(&mut text)?;
            Ok(text)
        });
        match result {
            Ok(text) if text.len() <= 128 * 1024 => Some(text),
            Ok(_) => None,
            Err(e) => {
                tracing::warn!(ticket, err = %e, "lead: progress note unreadable; using final text only");
                None
            }
        }
    }).await.inspect_err(|e| tracing::warn!(err = %e, "lead: progress reader failed; using final text only")).ok().flatten()
}

pub(crate) fn review_escalation_trigger(
    store: &dyn Store,
    pr: &str,
    head: &str,
) -> Result<LeadTrigger, rhapsody_store::StoreError> {
    let generation = store.review_bound(pr)?.map_or(0, |b| b.generation);
    let mut verdict = false;
    for row in store.load_live_review_watch()? {
        if format!("{}/{}#{}", row.key.owner, row.key.repo, row.key.number).eq_ignore_ascii_case(pr)
            && let Some(completed) = store.review_completed(&row.key)?
            && completed.generation == generation
            && completed.sha == head
            && matches!(
                completed.verdict.as_str(),
                rhapsody_store::REVIEW_COMPLETION_APPROVE
                    | rhapsody_store::REVIEW_COMPLETION_CHANGES
            )
        {
            verdict = true;
        }
    }
    Ok(if verdict {
        LeadTrigger::ReviewEscalation {
            pr: pr.to_ascii_lowercase(),
            head: head.into(),
        }
    } else {
        LeadTrigger::ImpossibleState {
            subject: pr.to_ascii_lowercase(),
            kind: "zero_verdict_escalation".into(),
        }
    })
}

/// Returns zero when the queue could not accept the item; callers keep their existing report.
pub fn enqueue(store: &dyn Store, t: LeadTrigger) -> i64 {
    if let Ok(Some(hold)) = store.subject_hold(t.subject())
        && hold.kind == "escalation"
        && !matches!(t, LeadTrigger::Overrule { .. })
    {
        return hold.id; // already parked: no new work and no duplicate fallback human report
    }
    let at = rhapsody_store::format_summon_at(chrono::Utc::now());
    match store.enqueue_lead_item(&t, &at) {
        Ok(id) if id > 0 => id,
        Ok(_) => {
            tracing::warn!(subject = %t.subject(), "lead queue unavailable: storage is disabled; keeping the existing report");
            0
        }
        Err(e) => {
            tracing::warn!(subject = %t.subject(), err = %e, "lead queue write failed; keeping the existing report");
            0
        }
    }
}

pub fn detect_blocked_handoff(final_text: &str, progress_md: Option<&str>) -> Option<String> {
    if let Some(question) = blocked_question(final_text) {
        return Some(question);
    }
    if final_declares_complete(final_text) {
        return None;
    }
    // An in-review marker alone does not claim completion: blocked agents use it too. Progress
    // notes are append-only, so fallback inspects their newest section rather than historical blocks.
    let progress = progress_md?;
    let start = progress
        .match_indices("\n##")
        .last()
        .map_or(0, |(i, _)| i + 1);
    blocked_question(&progress[start..])
}

pub(crate) fn final_declares_complete(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    !["not complete", "incomplete", "unmet", "pending decision"]
        .iter()
        .any(|s| lower.contains(s))
        && [
            "all acceptance met",
            "all acceptance criteria met",
            "ready for review",
            "review-ready",
            "work is complete",
            "task is complete",
            "previous block is resolved",
        ]
        .iter()
        .any(|s| lower.contains(s))
}

fn blocked_question(text: &str) -> Option<String> {
    let mut fenced = false;
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| {
            if line.trim_start().starts_with("```") {
                fenced = !fenced;
                return false;
            }
            !fenced && !line.trim_start().starts_with('>')
        })
        .collect();
    let start = lines.iter().position(|line| {
        let lower = line
            .trim_start_matches(['#', '*', '`', '-', ' ', '\t'])
            .to_ascii_lowercase();
        if [
            "unblocked",
            "no longer blocked",
            "not blocked",
            "previous block",
        ]
        .iter()
        .any(|s| lower.contains(s))
            || (lower.contains("resolved") && !lower.contains("unresolved"))
        {
            return false;
        }
        lower.contains("handoff: blocked")
            || lower.starts_with("blocked")
            || lower.starts_with("handoff is blocked")
            || (lower.contains("when blocked") && lower.contains("must decide"))
            || lower.contains("remains blocked")
            || (lower.starts_with("returning for the same required human decisions")
                && lower.contains("when blocked"))
    })?;
    let start = if lines[start]
        .trim_start()
        .to_ascii_lowercase()
        .starts_with("handoff: blocked")
    {
        0
    } else {
        start
    };
    let question = lines[start..]
        .iter()
        .take_while(|line| {
            let lower = line.to_ascii_lowercase();
            !lower.contains("verification") && !lower.contains("fresh gh pr checks")
        })
        .filter(|line| !line.trim_start().starts_with("HANDOFF:"))
        .copied()
        .collect::<Vec<_>>()
        .join("\n");
    let question = question.trim();
    if question.is_empty() {
        Some(
            "The agent declared a blocked handoff without a question; inspect its run ending."
                .into(),
        )
    } else {
        Some(question.chars().take(4000).collect())
    }
}

impl Orchestrator {
    fn park_state_is_terminal(&self, state: &str, project: Option<usize>) -> bool {
        let state = rhapsody_core::normalize_state(state);
        if state.is_empty() {
            return false;
        }
        let terminal = self.eff.as_ref().is_some_and(|eff| {
            project
                .and_then(|i| eff.projects.get(i))
                .map_or(&eff.terminal_states, |p| &p.terminal_states)
                .contains(&state)
        });
        terminal
            || self
                .teams
                .as_ref()
                .and_then(|t| t.review_done_state())
                .is_some_and(|done| rhapsody_core::normalize_state(done) == state)
    }
    /// Candidate queries intentionally exclude Backlog/Done. A park must therefore observe its
    /// ticket by identifier too, before dispatch gates, without treating absence as a state move.
    /// Owned tracker reads run off-task: at most 16 tickets per tick, 4 concurrent, 10 seconds total.
    /// Rotation prevents an unavailable ticket at the front of the ledger starving later holds.
    pub(crate) async fn reconcile_parked_subject_states(&mut self) {
        let Some(eff) = self.eff.as_ref() else { return };
        let trackers: Vec<_> = if eff.projects.is_empty() {
            vec![(String::new(), None, eff.tracker.clone())]
        } else {
            eff.projects
                .iter()
                .enumerate()
                .filter(|(_, p)| !p.disabled)
                .map(|(i, p)| (p.slug.clone(), Some(i), p.tracker.clone()))
                .collect()
        };
        if trackers.is_empty() {
            return;
        }
        let mut holds = match self.store().load_subject_observations() {
            Ok(holds) => holds
                .into_iter()
                .filter(|h| !h.subject.contains('#'))
                .collect::<Vec<_>>(),
            Err(error) => {
                tracing::warn!(%error, "parked state ledger unreadable; keeping holds");
                return;
            }
        };
        if holds.is_empty() {
            return;
        }
        let offset = self.parked_state_cursor % holds.len();
        holds.rotate_left(offset);
        self.parked_state_cursor = (offset + holds.len().min(16)) % holds.len();
        let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(4));
        let mut tasks = tokio::task::JoinSet::new();
        for hold in holds.into_iter().take(16) {
            let project = match self.store().list_issue_runs(rhapsody_store::RunFilter {
                issue: hold.subject.clone(),
                limit: 1,
                ..Default::default()
            }) {
                Ok(runs) => runs
                    .first()
                    .map(|r| r.project_slug.clone())
                    .unwrap_or_default(),
                Err(error) => {
                    tracing::warn!(ticket = %hold.subject, %error, "park project unreadable; keeping hold");
                    continue;
                }
            };
            let owned: Vec<_> = trackers
                .iter()
                .filter(|(slug, _, _)| project.is_empty() || *slug == project)
                .map(|(_, i, tr)| (*i, tr.clone()))
                .collect();
            let semaphore = semaphore.clone();
            tasks.spawn(async move {
                let Ok(_permit) = semaphore.acquire_owned().await else {
                    tracing::warn!(ticket = %hold.subject, "parked state read permit unavailable; keeping hold");
                    return None;
                };
                for (project, tracker) in owned {
                    match tracker.fetch_issue_by_identifier(&hold.subject).await {
                        Ok(Some(issue)) if issue.identifier.eq_ignore_ascii_case(&hold.subject) => {
                            return Some((hold, project, issue.state));
                        }
                        Ok(_) => {}
                        Err(error) => tracing::warn!(ticket = %hold.subject, %error, "parked ticket state read failed; keeping hold"),
                    }
                }
                None
            });
        }
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            match tokio::time::timeout_at(deadline, tasks.join_next()).await {
                Ok(Some(Ok(Some((hold, project, after))))) if !after.trim().is_empty() => {
                    let result = (|| -> Result<(), String> {
                        let terminal = self.park_state_is_terminal(&after, project);
                        self.store()
                            .observe_subject_state(&hold.subject, terminal)
                            .map_err(|e| e.to_string())?;
                        if terminal {
                            return Ok(());
                        }
                        let mut snapshot: ParkSnapshot = if hold.snapshot.is_empty() {
                            ParkSnapshot::default()
                        } else {
                            serde_json::from_str(&hold.snapshot).map_err(|e| e.to_string())?
                        };
                        if snapshot
                            .state
                            .as_ref()
                            .is_some_and(|before| before != &after)
                        {
                            self.store()
                                .release_subject_episode(
                                    hold.id,
                                    &hold.kind,
                                    "ticket state changed (scoped hold observation)",
                                )
                                .map_err(|e| e.to_string())?;
                        } else if snapshot.state.is_some() || hold.kind == "escalation" {
                            // A blocked handoff with state=None still lets candidate observation
                            // establish its own move; escalation folds need a scoped live baseline.
                            snapshot.state = Some(after);
                            self.store()
                                .confirm_subject_hold_state(
                                    hold.id,
                                    &serde_json::to_string(&snapshot).map_err(|e| e.to_string())?,
                                )
                                .map_err(|e| e.to_string())?;
                        }
                        Ok(())
                    })();
                    if let Err(error) = result {
                        tracing::warn!(ticket = %hold.subject, %error, "park state observation failed; keeping hold");
                    }
                }
                Ok(Some(Ok(_))) => {}
                Ok(Some(Err(error))) => {
                    tracing::warn!(%error, "parked state task failed; keeping hold")
                }
                Ok(None) => break,
                Err(_) => {
                    tracing::warn!(
                        "parked state observation timed out; unread holds remain parked"
                    );
                    break; // dropping JoinSet cancels unfinished reads
                }
            }
        }
    }

    pub(crate) fn subject_human_holds(&self) -> Vec<crate::dispatch::HeldForHuman> {
        let mut held = self.human_holds.held();
        match self.store().load_subject_holds() {
            Ok(rows) => {
                for hold in rows {
                    if held
                        .iter()
                        .any(|h| h.issue_identifier.eq_ignore_ascii_case(&hold.subject))
                        || self.ticket_run_live(&hold.subject)
                        || hold.subject.contains('#')
                    {
                        continue;
                    }
                    let run = self
                        .store()
                        .list_issue_runs(rhapsody_store::RunFilter {
                            issue: hold.subject.clone(),
                            limit: 1,
                            ..Default::default()
                        })
                        .ok()
                        .and_then(|rows| rows.into_iter().next());
                    held.push(crate::dispatch::HeldForHuman {
                        issue_identifier: hold.subject,
                        title: run.as_ref().map_or(hold.need, |r| r.title.clone()),
                        project: run.map_or_else(String::new, |r| r.project_slug),
                    });
                }
            }
            Err(error) => tracing::warn!(%error, "held subject snapshot unavailable"),
        }
        held
    }
    /// One durable gate at every admission path. A blocked author may still receive its first
    /// review and a lead judgment; an escalation parks all three work producers.
    pub(crate) fn subject_parked(&self, subject: &str, work: &str) -> bool {
        match self.store().subject_hold(subject) {
            Ok(Some(hold)) => match work {
                "lead" => hold.kind == "escalation",
                "review" => hold.kind != "blocked",
                _ => true,
            },
            Ok(None) => false,
            Err(error) => {
                tracing::warn!(subject, %error, "subject hold unreadable; refusing new work");
                true
            }
        }
    }

    pub(crate) fn observe_parked_issue(&self, issue: &rhapsody_core::Issue) {
        self.observe_parked_issue_in_project(issue, None);
    }

    pub(crate) fn observe_parked_issue_in_project(
        &self,
        issue: &rhapsody_core::Issue,
        project: Option<usize>,
    ) {
        let result = (|| -> Result<(), String> {
            if !issue.state.trim().is_empty() {
                let terminal = self.park_state_is_terminal(&issue.state, project);
                self.store()
                    .observe_subject_state(&issue.identifier, terminal)
                    .map_err(|e| e.to_string())?;
                if terminal {
                    return Ok(());
                }
            }
            let Some(hold) = self
                .store()
                .subject_hold(&issue.identifier)
                .map_err(|e| e.to_string())?
            else {
                return Ok(());
            };
            let mut snapshot: ParkSnapshot = if hold.snapshot.is_empty() {
                ParkSnapshot::default()
            } else {
                serde_json::from_str(&hold.snapshot).map_err(|e| e.to_string())?
            };
            let description = issue.description.clone().unwrap_or_default();
            let state = &issue.state;
            let human = crate::teams::is_human(issue);
            let changed = snapshot
                .description
                .as_ref()
                .is_some_and(|old| old != &description)
                || snapshot
                    .state
                    .as_ref()
                    .is_some_and(|old| !state.is_empty() && old != state)
                || (snapshot.human && !human);
            if changed {
                return self
                    .store()
                    .release_subject_episode(
                        hold.id,
                        &hold.kind,
                        "ticket description/state/hold label changed",
                    )
                    .map(|_| ())
                    .map_err(|e| e.to_string());
            }
            snapshot.description = Some(description);
            if !state.is_empty() {
                snapshot.state = Some(state.clone());
            }
            snapshot.human |= human;
            let encoded = serde_json::to_string(&snapshot).map_err(|e| e.to_string())?;
            if state.trim().is_empty() {
                self.store().update_hold_snapshot(hold.id, &encoded)
            } else {
                self.store().confirm_subject_hold_state(hold.id, &encoded)
            }
            .map_err(|e| e.to_string())
        })();
        if let Err(error) = result {
            tracing::warn!(ticket = %issue.identifier, %error, "material change could not be confirmed; keeping park");
        }
    }

    pub(crate) fn observe_parked_head(&self, pr: &str, head: &str) {
        if head.is_empty() {
            return;
        }
        let result = (|| -> Result<(), String> {
            let Some(hold) = self.store().subject_hold(pr).map_err(|e| e.to_string())? else {
                return Ok(());
            };
            let mut snapshot: ParkSnapshot = if hold.snapshot.is_empty() {
                ParkSnapshot::default()
            } else {
                serde_json::from_str(&hold.snapshot).map_err(|e| e.to_string())?
            };
            let key = pr.to_ascii_lowercase();
            if snapshot.heads.get(&key).is_some_and(|old| old != head) {
                return self
                    .store()
                    .release_subject_episode(hold.id, &hold.kind, "PR head moved")
                    .map(|_| ())
                    .map_err(|e| e.to_string());
            }
            snapshot.heads.insert(key, head.into());
            self.store()
                .update_hold_snapshot(
                    hold.id,
                    &serde_json::to_string(&snapshot).map_err(|e| e.to_string())?,
                )
                .map_err(|e| e.to_string())
        })();
        if let Err(error) = result {
            tracing::warn!(pr, %error, "head change unconfirmed; keeping park");
        }
    }

    pub(crate) fn lead_enabled(&self) -> bool {
        self.teams
            .as_ref()
            .is_some_and(|t| t.enabled && t.manager.lead.enabled)
    }

    pub(crate) fn route_lead_divergences(&self, found: &mut Vec<Divergence>) {
        if !self.lead_enabled() {
            return;
        }
        found.retain(|d| {
            let trigger = match d.kind {
                DivergenceKind::ReviewEscalated => match review_escalation_trigger(self.store(), &d.pr, &d.adjudicated_head) {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::warn!(pr = %d.pr, err = %e, "lead: review evidence unreadable; keeping the human report");
                        return true;
                    }
                },
                DivergenceKind::ReviewInfrastructure => LeadTrigger::ImpossibleState { subject: d.pr.clone(), kind: "repeated_failed_attempts".into() },
                _ => return true,
            };
            enqueue(self.store(), trigger) == 0
        });
    }

    pub(crate) fn route_lead_manager_endings(&self, found: &mut Vec<Divergence>) {
        if !self.lead_enabled() {
            return;
        }
        let rows = match self.store().load_manager_interventions() {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(err = %e, "lead: manager endings unreadable; keeping the human feed");
                return;
            }
        };
        for row in rows {
            if row.mode != rhapsody_store::MANAGER_MODE_ACT
                || !matches!(row.state.as_str(), "escalated" | "exhausted")
            {
                continue;
            }
            match self.store().review_bound(&row.pr) {
                Ok(Some(bound)) if bound.generation == row.generation => {}
                Ok(_) => continue,
                Err(e) => {
                    tracing::warn!(pr = %row.pr, err = %e, "lead: manager generation unreadable; keeping the human feed");
                    continue;
                }
            }
            let trigger = if row.state == "exhausted" {
                LeadTrigger::ImpossibleState {
                    subject: row.pr.clone(),
                    kind: "repeated_failed_attempts".into(),
                }
            } else {
                match review_escalation_trigger(self.store(), &row.pr, &row.decision_head) {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::warn!(pr = %row.pr, err = %e, "lead: manager review evidence unreadable; keeping the human feed");
                        continue;
                    }
                }
            };
            if enqueue(self.store(), trigger) > 0 {
                found.retain(|d| {
                    !d.pr.eq_ignore_ascii_case(&row.pr)
                        || (d.kind != DivergenceKind::ManagerDeferred
                            && crate::managerintervention::stall_kind_for(d.kind).is_none())
                });
            }
        }
    }

    pub(crate) fn lead_missing_pr(&self, ticket: &str) -> bool {
        self.lead_impossible(ticket, "in_review_no_pr")
    }

    /// A no-work resolution settles one occurrence, not all future incidents for this ticket.
    /// Reuse the poll's project-scoped board snapshot: explicit departures are authoritative;
    /// absence is authoritative only when every configured project answered. No tracker I/O here.
    pub(crate) fn observe_lead_review_departures<'a>(
        &mut self,
        candidates: impl Iterator<Item = (&'a rhapsody_core::Issue, Option<usize>)>,
        read_the_board: bool,
    ) {
        if !self.lead_enabled() {
            return;
        }
        let Some(eff) = self.eff.as_ref() else {
            return;
        };
        let reviewing: std::collections::HashMap<&str, bool> = candidates
            .map(|(issue, proj)| {
                let (review, active) = match proj.and_then(|i| eff.projects.get(i)) {
                    Some(p) => (&p.review_states, &p.active_states),
                    None => (&eff.review_states, &eff.active_states),
                };
                let state = rhapsody_core::normalize_state(&issue.state);
                // An unknown state cannot establish that the condition went away.
                (
                    issue.identifier.as_str(),
                    state.is_empty() || (review.contains(&state) && !active.contains(&state)),
                )
            })
            .collect();
        let rows = match self.store().load_lead_items() {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!(%error, "lead: resolved incidents unreadable; preserving dedupe keys");
                return;
            }
        };
        for row in rows {
            if row.state != "done"
                || !matches!(&row.trigger,
                LeadTrigger::ImpossibleState { kind, .. } if kind == "in_review_no_pr")
            {
                continue;
            }
            let departed = match reviewing.get(row.subject.as_str()) {
                Some(in_review) => !in_review,
                None => read_the_board,
            };
            if !departed {
                continue;
            }
            match self.store().retire_resolved_lead_missing_pr(row.id) {
                Ok(true) => {
                    self.review_adopt_probed.remove(&row.subject);
                }
                Ok(false) => {}
                Err(error) => tracing::warn!(item = row.id, %error,
                    "lead: could not retire resolved incident; preserving its dedupe key"),
            }
        }
    }

    pub(crate) fn lead_draft_exhausted(&self, pr: &str) -> bool {
        self.lead_impossible(pr, "draft_pokes_exhausted")
    }

    pub(crate) fn lead_impossible(&self, subject: &str, kind: &str) -> bool {
        self.lead_enabled()
            && enqueue(
                self.store(),
                LeadTrigger::ImpossibleState {
                    subject: subject.into(),
                    kind: kind.into(),
                },
            ) > 0
    }

    /// L5's wiring seam. Detection here queues a judgment; it never interrupts a run or spends credits.
    pub fn enqueue_limit_judgment(&self, account: &str) -> bool {
        self.lead_enabled()
            && enqueue(
                self.store(),
                LeadTrigger::LimitJudgment {
                    account: account.into(),
                },
            ) > 0
    }

    pub(crate) fn handle_lead_blocked(
        &self,
        issue_id: &str,
        started_at: chrono::DateTime<chrono::Utc>,
        question: String,
    ) {
        if !self.lead_enabled() {
            return;
        }
        if let Some(re) = self.running.get(issue_id)
            && re.started_at == started_at
            && re.review.is_none()
            && !crate::managerrun::is_manager_key(&re.issue.id)
        {
            let project = self
                .eff
                .as_ref()
                .and_then(|eff| eff.projects.iter().position(|p| p.slug == re.project_slug));
            self.observe_parked_issue_in_project(&re.issue, project);
            if self.park_state_is_terminal(&re.issue.state, project) {
                return;
            }
            enqueue(
                self.store(),
                LeadTrigger::BlockedHandoff {
                    ticket: re.issue.identifier.clone(),
                    question: question.clone(),
                },
            );
            let snapshot = park_snapshot(self.store(), &re.issue, None, true).and_then(|text| {
                let mut snapshot: ParkSnapshot =
                    serde_json::from_str(&text).map_err(|e| e.to_string())?;
                for (pr, observed) in &self.review_observed_head {
                    let key = pr.to_string().to_ascii_lowercase();
                    if snapshot.heads.contains_key(&key) && !observed.head.is_empty() {
                        snapshot.heads.insert(key, observed.head.clone());
                    }
                }
                serde_json::to_string(&snapshot).map_err(|e| e.to_string())
            });
            match snapshot.and_then(|snapshot| {
                self.store()
                    .park_subject(&rhapsody_store::SubjectHold {
                        subject: re.issue.identifier.clone(),
                        kind: "blocked".into(),
                        need: question,
                        at: rhapsody_store::format_summon_at(chrono::Utc::now()),
                        snapshot,
                        ..Default::default()
                    })
                    .map_err(|e| e.to_string())
            }) {
                Ok(_) => {}
                Err(error) => tracing::warn!(%error, "blocked ending could not be parked"),
            }
        }
    }

    pub(crate) fn handle_lead_missing_pr(&self, req: &crate::reviewintro::ReviewIntroRequest) {
        if self.lead_enabled()
            && req.only_if_unwatched
            && self.review_repo_is_configured(&req.owner, &req.repo)
            && let Some(ticket) = crate::reviewdone::origin_ticket(&req.introduced_by)
            && !self.ticket_run_live(ticket)
        {
            self.lead_missing_pr(ticket);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rhapsody_config::teams::Teams;
    use rhapsody_store::{Sqlite, StorePath};

    use super::*;
    use crate::reviewreconcile::DivergenceKind;

    #[test]
    fn terminal_observation_releases_park_and_notice() {
        for snapshot in [
            "",
            "{\"state\":\"Done\",\"human\":false,\"heads\":{}}",
            "invalid",
        ] {
            let mut o = orch(true);
            let mut eff =
                crate::testsupport::empty_effective(Arc::new(rhapsody_tracker::fake::Fake::new()));
            eff.terminal_states = crate::testsupport::set_of(&["done"]);
            o.eff = Some(eff);
            let id = o
                .store()
                .park_subject(&rhapsody_store::SubjectHold {
                    subject: "TEST-1".into(),
                    kind: "escalation".into(),
                    snapshot: snapshot.into(),
                    ..Default::default()
                })
                .unwrap();
            o.store()
                .observe_notices(
                    &[rhapsody_store::Notice {
                        source: format!("lead-park:{id}"),
                        kind: "lead_escalation".into(),
                        group: "needs_you".into(),
                        subject: "TEST-1".into(),
                        transient: true,
                        ..Default::default()
                    }],
                    "2026-10-10T16:00:00Z",
                )
                .unwrap();
            o.observe_parked_issue(&crate::testsupport::issue("id", "TEST-1", " Done "));
            assert!(
                o.store().subject_hold("TEST-1").unwrap().is_none(),
                "{snapshot}"
            );
            assert!(!o.store().notices().unwrap()[0].active);
            assert!(!o.store().subject_resume_pending("TEST-1").unwrap());
            // A notification collection already in flight cannot resurrect the released episode.
            let stale = o.store().notices().unwrap().remove(0);
            o.store()
                .observe_notices(&[stale], "2026-10-10T16:01:00Z")
                .unwrap();
            assert!(o.store().notices().unwrap().iter().all(|n| !n.active));
        }
    }

    #[tokio::test]
    async fn scoped_terminal_observation_covers_missing_and_invalid_baselines_and_done_state() {
        for state in ["Shipped", "Archived", "In Review", ""] {
            for baseline in [
                "",
                "invalid",
                "{\"state\":null,\"human\":false,\"heads\":{}}",
            ] {
                let mut tracker = rhapsody_tracker::fake::Fake::new();
                tracker.by_identifier.insert(
                    "TEST-1".into(),
                    crate::testsupport::issue("id", "TEST-1", state),
                );
                let mut o = orch(true);
                o.teams.as_mut().unwrap().review.done_state = "Archived".into();
                o.teams.as_mut().unwrap().review.mode =
                    rhapsody_config::teams::ReviewMode::Ticketless;
                let project =
                    crate::testsupport::proj_with_tracker("project", Arc::new(tracker), "project");
                let mut eff = crate::testsupport::empty_effective(Arc::new(
                    rhapsody_tracker::fake::Fake::new(),
                ));
                eff.projects = vec![project];
                eff.projects[0].terminal_states = crate::testsupport::set_of(&["shipped"]);
                o.eff = Some(eff);
                o.store()
                    .park_subject(&rhapsody_store::SubjectHold {
                        subject: "TEST-1".into(),
                        kind: "escalation".into(),
                        snapshot: baseline.into(),
                        ..Default::default()
                    })
                    .unwrap();
                o.reconcile_parked_subject_states().await;
                assert_eq!(
                    o.store().subject_hold("TEST-1").unwrap().is_none(),
                    matches!(state, "Shipped" | "Archived"),
                    "{state}/{baseline}"
                );
            }
        }
    }

    #[test]
    fn material_ticket_changes_release_and_identical_polls_keep_the_park() {
        for change in ["description", "state", "label", "unchanged"] {
            let o = orch(true);
            let mut issue = crate::testsupport::issue("id", "TEST-1", "In Review");
            issue.description = Some("original acceptance".into());
            issue.labels = Some(vec![crate::teams::HUMAN_LABEL.into()]);
            o.store()
                .park_subject(&rhapsody_store::SubjectHold {
                    subject: issue.identifier.clone(),
                    kind: "escalation".into(),
                    snapshot: park_snapshot(o.store(), &issue, Some(("o/r#1", "a")), false)
                        .unwrap(),
                    ..Default::default()
                })
                .unwrap();
            match change {
                "description" => issue.description = Some("corrected acceptance".into()),
                "state" => issue.state = "Todo".into(),
                "label" => issue.labels = None,
                _ => {}
            }
            o.observe_parked_issue(&issue);
            assert_eq!(
                o.subject_parked("TEST-1", "author"),
                change == "unchanged",
                "{change}"
            );
            assert_eq!(
                o.store().subject_resume_pending("TEST-1").unwrap(),
                change != "unchanged"
            );
        }
    }

    #[tokio::test]
    async fn parked_state_reads_rotate_past_the_per_tick_bound() {
        let mut tracker = rhapsody_tracker::fake::Fake::new();
        let mut o = orch(true);
        for n in 0..17 {
            let ticket = format!("TEST-{n}");
            let old = crate::testsupport::issue(&ticket, &ticket, "In Review");
            tracker.by_identifier.insert(
                ticket.clone(),
                crate::testsupport::issue(&ticket, &ticket, "Done"),
            );
            o.store()
                .park_subject(&rhapsody_store::SubjectHold {
                    subject: ticket,
                    kind: "escalation".into(),
                    snapshot: park_snapshot(o.store(), &old, None, false).unwrap(),
                    ..Default::default()
                })
                .unwrap();
        }
        o.eff = Some(crate::testsupport::empty_effective(Arc::new(tracker)));
        o.reconcile_parked_subject_states().await;
        assert_eq!(
            o.store().load_subject_holds().unwrap().len(),
            1,
            "one bounded batch, no unbounded read burst"
        );
        o.reconcile_parked_subject_states().await;
        assert!(
            o.store().load_subject_holds().unwrap().is_empty(),
            "the next batch reaches the remaining hold"
        );
    }

    fn orch(enabled: bool) -> Orchestrator {
        let mut o = Orchestrator::new("WORKFLOW.md");
        let mut teams = Teams::disabled();
        teams.enabled = true;
        teams.manager.lead.enabled = enabled;
        o.teams = Some(teams);
        o.store = Arc::new(Sqlite::open(StorePath::InMemory).expect("store"));
        o
    }

    fn divergence(kind: DivergenceKind) -> Divergence {
        Divergence {
            pr: "makewhatis/rhapsody#290".into(),
            kind,
            ticket: "STUDIO-1116".into(),
            reviewer: String::new(),
            stale_secs: 0,
            auto_merge_reason: None,
            capacity_held: None,
            capacity_unreadable: None,
            adjudicated_head: "c6cf294".into(),
            current_head: "c6cf294".into(),
            rounds: 3,
            findings: vec!["B1: review the quit path".into()],
            reason: "need a decision".into(),
        }
    }

    #[test]
    fn detects_real_blocked_phrasings() {
        // Survey: MH3 progress and L1 final text (2026-10-06/07), including in-review markers.
        let cases = [
            "## BLOCKED — spec §4.5 vs installed CLI resolved permissions\nStop per When blocked; human/spec owner must decide whether to retain strict failure or approve a narrow exception.",
            "Handoff is BLOCKED acceptance, not done: one real canary did not pass. Human must determine environment/model/login remedy.",
            "**Blocked pending two human decisions; L1 remains incomplete.**\nRequired to resume:\n1. Authorize a named, read-only OpenCode credential source.\n2. Clarify whether Event should gain an optional LimitObs payload.\nHANDOFF: in-review",
            "**Blocked; returned for a human decision. L1 is not complete.**\nProvide a permitted access-only credential copy.\nHANDOFF: in-review",
            "Returning for the same required human decisions under When blocked: authorize a named read-only native credential source; approve an additive observation payload.",
            "HANDOFF: blocked\nQuestion: which event representation is approved?",
            "BLOCKED: operator must supply a permitted measurement input.",
            "BLOCKED: the credential source remains unresolved; human must decide.",
            "When blocked: a human must decide which event representation is approved.",
            "**B1 remains blocked.**\nNext action: operator diagnosis of the environment/model/login remedy.",
        ];
        for text in cases {
            let question = detect_blocked_handoff(text, None).expect(text);
            assert!(!question.is_empty());
            assert!(
                question.contains("credential")
                    || question.contains("permission")
                    || question.contains("exception")
                    || question.contains("remedy")
                    || question.contains("event")
                    || question.contains("measurement"),
                "{question}"
            );
        }
        assert!(detect_blocked_handoff("", Some(cases[0])).is_some());
        for text in [
            "All acceptance met; ready for review. HANDOFF: in-review",
            "A dependency is blocked by this ticket.",
            "The previous block is resolved. All tests pass. HANDOFF: in-review",
        ] {
            assert_eq!(detect_blocked_handoff(text, None), None, "{text}");
        }
        assert_eq!(
            detect_blocked_handoff("All acceptance met; ready for review.", Some(cases[0])),
            None
        );
    }

    #[test]
    fn review_escalation_becomes_item_not_human_label() {
        let mut o = orch(true);
        if let Some(teams) = o.teams.as_mut() {
            teams.review.mode = rhapsody_config::teams::ReviewMode::Ticketless;
            teams.review.adjudicate_after_rounds = 3;
        }
        o.eff = Some(crate::testsupport::empty_effective(Arc::new(
            rhapsody_tracker::fake::Fake::new(),
        )));
        o.human_holds.begin_pass(true);
        let key = rhapsody_store::ReviewWatchKey {
            owner: "makewhatis".into(),
            repo: "rhapsody".into(),
            number: 290,
            reviewer: "alice".into(),
        };
        o.store()
            .ensure_review_generation("makewhatis/rhapsody#290")
            .expect("generation");
        o.store()
            .save_review_watch(rhapsody_store::ReviewWatchRow {
                key: key.clone(),
                author: "jerry".into(),
                introduced_by: "handoff:STUDIO-1116".into(),
                requested_sha: "c6cf294".into(),
                last_reviewed_sha: String::new(),
                status: "in_flight".into(),
                open: true,
            })
            .expect("watch");
        o.store()
            .record_review_completion(
                &key,
                "reviewed",
                &rhapsody_store::ReviewCompleted {
                    generation: 1,
                    sha: "c6cf294".into(),
                    patch_id: "patch".into(),
                    verdict: "changes".into(),
                },
            )
            .expect("verdict");
        let ledger = Arc::new(crate::reviewadjudicate::AdjudicationLedger::with_store(
            o.store.clone(),
        ));
        ledger.record(
            &crate::prstate::PrCoord::new("makewhatis", "rhapsody", 290),
            crate::reviewadjudicate::Adjudication::Escalate {
                head: "c6cf294".into(),
                rounds: 3,
                findings: vec!["B1: quit path".into()],
                reason: "needs adjudication".into(),
            },
        );
        o.adjudication_ledger = Some(ledger);
        o.reconcile_review_divergence();
        let items = o.store().load_lead_items().expect("items");
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].trigger,
            LeadTrigger::ReviewEscalation {
                pr: "makewhatis/rhapsody#290".into(),
                head: "c6cf294".into()
            }
        );
        assert!(o.review_divergence.is_empty());
        assert!(o.human_holds.labelled_and_primed().0.is_empty());
    }

    #[test]
    fn zero_verdict_escalation_is_impossible_state() {
        let mut o = orch(true);
        o.store()
            .record_review_adjudication(
                "makewhatis/rhapsody#290",
                &rhapsody_store::ReviewAdjudication {
                    decision: "escalate".into(),
                    head: "c6cf294".into(),
                    rounds: 3,
                    findings: Vec::new(),
                    reason: "three 401s; no review completed".into(),
                },
            )
            .expect("adjudication");
        // The STUDIO-1129 boot repair must capture the impossible state before clearing it.
        o.rehydrate_review_bounds();
        let items = o.store().load_lead_items().expect("items");
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].trigger,
            LeadTrigger::ImpossibleState {
                subject: "makewhatis/rhapsody#290".into(),
                kind: "zero_verdict_escalation".into()
            }
        );
        assert!(
            o.store()
                .review_bound("makewhatis/rhapsody#290")
                .expect("bound")
                .expect("row")
                .adjudication
                .is_none()
        );
    }

    #[tokio::test]
    async fn in_review_without_pr_is_impossible_state() {
        let mut o = orch(true);
        let mut eff =
            crate::testsupport::empty_effective(Arc::new(rhapsody_tracker::fake::Fake::new()));
        eff.cfg.repo = "https://github.com/makewhatis/rhapsody.git".into();
        o.eff = Some(eff);
        o.drive_event(crate::Event::ReviewMissingPr(
            crate::reviewintro::ReviewIntroRequest {
                owner: "makewhatis".into(),
                repo: "rhapsody".into(),
                repo_url: "https://github.com/makewhatis/rhapsody.git".into(),
                head_branch: "symphony/STUDIO-1123".into(),
                author: String::new(),
                reviewers: Vec::new(),
                introduced_by: "adopt:STUDIO-1123".into(),
                only_if_unwatched: true,
                link: None,
            },
        ))
        .await;
        let items = o.store().load_lead_items().expect("items");
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].trigger,
            LeadTrigger::ImpossibleState {
                subject: "STUDIO-1123".into(),
                kind: "in_review_no_pr".into()
            }
        );
    }

    #[test]
    fn resolved_missing_pr_departure_requires_an_authoritative_project_observation() {
        for (state, complete, retire) in [
            (Some("In Review"), true, false),
            (Some(""), true, false),
            (None, false, false), // partial/failed board read, not departure
            (None, true, true),   // terminal/disappeared on a fully read board
            (Some("Todo"), false, true), // explicit away observation, even on a partial poll
        ] {
            let mut o = orch(true);
            let tracker = Arc::new(rhapsody_tracker::fake::Fake::new());
            let mut eff = crate::testsupport::empty_effective(tracker.clone());
            eff.review_states = crate::testsupport::set_of(&["todo"]); // must use owning project's set
            let mut project = crate::testsupport::empty_resolved_project("rhapsody", tracker);
            project.review_states = crate::testsupport::set_of(&["in review"]);
            eff.projects.push(project);
            o.eff = Some(eff);
            o.lead_missing_pr("STUDIO-598");
            let id = o.store().load_lead_items().unwrap()[0].id;
            o.store()
                .save_lead_decision(&rhapsody_store::LeadDecisionRow {
                    item: id,
                    decision: "done: resolved".into(),
                    actions: r#"[{"action":"resolve"}]"#.into(),
                    ..Default::default()
                })
                .unwrap();
            o.store().set_lead_item_state(id, "done").unwrap();
            let issues: Vec<_> = state
                .map(|s| crate::testsupport::issue("598", "STUDIO-598", s))
                .into_iter()
                .collect();
            o.observe_lead_review_departures(issues.iter().map(|i| (i, Some(0))), complete);
            o.lead_missing_pr("STUDIO-598");
            assert_eq!(
                o.store().load_lead_items().unwrap().len(),
                if retire { 2 } else { 1 },
                "{state:?}/{complete}"
            );
        }
    }

    #[test]
    fn draft_pokes_exhausted_is_item() {
        let o = orch(true);
        assert!(o.lead_draft_exhausted("makewhatis/rhapsody#297"));
        let items = o.store().load_lead_items().expect("items");
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].trigger,
            LeadTrigger::ImpossibleState {
                subject: "makewhatis/rhapsody#297".into(),
                kind: "draft_pokes_exhausted".into()
            }
        );
    }

    #[test]
    fn dedupe_same_subject_question() {
        let o = orch(true);
        let t = LeadTrigger::BlockedHandoff {
            ticket: "STUDIO-1123".into(),
            question: "authorize credential source?".into(),
        };
        let id = enqueue(o.store(), t.clone());
        assert!(id > 0);
        assert_eq!(enqueue(o.store(), t), id);
        let other = enqueue(
            o.store(),
            LeadTrigger::BlockedHandoff {
                ticket: "STUDIO-1123".into(),
                question: "which Event representation?".into(),
            },
        );
        assert_ne!(other, id);
        let items = o.store().load_lead_items().expect("items");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].state, "queued");
        assert_eq!(items[0].attempts_on_question, 0);
        assert!(!items[0].created_at.is_empty());
    }

    #[test]
    fn lead_disabled_behaviour_unchanged() {
        let mut o = orch(false);
        let before = vec![
            divergence(DivergenceKind::ReviewEscalated),
            divergence(DivergenceKind::ReviewInfrastructure),
        ];
        let mut after = before.clone();
        o.review_divergence = before.clone();
        let before_bytes =
            serde_json::to_vec(&crate::snapshot_json::render(&o.build_snapshot())).expect("render");
        o.route_lead_divergences(&mut after);
        assert_eq!(
            after, before,
            "all fields and ordering of the human feed must survive"
        );
        assert!(!o.lead_missing_pr("STUDIO-1123"));
        assert!(!o.lead_draft_exhausted("makewhatis/rhapsody#297"));
        assert!(o.store().load_lead_items().expect("items").is_empty());
        o.review_divergence = after;
        let after_bytes =
            serde_json::to_vec(&crate::snapshot_json::render(&o.build_snapshot())).expect("render");
        assert_eq!(before_bytes, after_bytes);
    }

    #[test]
    fn lead_repeated_failed_attempts_leave_the_human_feed() {
        let o = orch(true);
        let mut feed = vec![divergence(DivergenceKind::ReviewInfrastructure)];
        o.route_lead_divergences(&mut feed);
        assert!(feed.is_empty());
        let items = o.store().load_lead_items().expect("items");
        assert_eq!(
            items[0].trigger,
            LeadTrigger::ImpossibleState {
                subject: "makewhatis/rhapsody#290".into(),
                kind: "repeated_failed_attempts".into()
            }
        );
    }

    #[test]
    fn lead_no_store_never_swallows_a_human_report() {
        let mut o = orch(true);
        o.store = Arc::new(rhapsody_store::Noop);
        let before = vec![divergence(DivergenceKind::ReviewEscalated)];
        let mut after = before.clone();
        o.route_lead_divergences(&mut after);
        assert_eq!(before, after);
        assert!(!o.lead_missing_pr("STUDIO-1123"));
    }

    #[test]
    fn lead_detector_does_not_mistake_a_completed_summary_for_a_block() {
        let text = "Implemented blocked human handoffs as tech-lead items.\nAll acceptance met.\nHANDOFF: in-review";
        assert_eq!(detect_blocked_handoff(text, None), None);
        let text = "Please approve the Event representation before resuming.\nHANDOFF: blocked";
        assert!(
            detect_blocked_handoff(text, None)
                .expect("question")
                .contains("Event representation")
        );
        let progress = "## BLOCKED\nHuman must decide.\n## Status\nThe block is resolved; L1 is complete and review-ready.";
        assert_eq!(detect_blocked_handoff("", Some(progress)), None);
    }

    #[test]
    fn lead_progress_can_declare_a_block_beside_an_in_review_marker() {
        let progress = "## Current blocker\nBLOCKED: Human must decide which Event representation is approved.";
        assert!(
            detect_blocked_handoff("HANDOFF: in-review", Some(progress))
                .expect("question")
                .contains("Event representation")
        );
        assert_eq!(
            detect_blocked_handoff(
                "All acceptance met; ready for review.\nHANDOFF: in-review",
                Some(progress)
            ),
            None
        );
    }

    #[tokio::test]
    async fn lead_control_transport_ignores_stale_and_non_ticket_endings() {
        let mut o = orch(true);
        let re = crate::testsupport::running_entry(
            crate::testsupport::issue("u-1123", "STUDIO-1123", "In Progress"),
            "",
            "",
        );
        let at = re.started_at;
        o.running.insert("u-1123".into(), re);
        o.handle_lead_blocked(
            "u-1123",
            at + chrono::Duration::seconds(1),
            "wrong attempt".into(),
        );
        assert!(o.store().load_lead_items().expect("items").is_empty());
        o.drive_event(crate::Event::LeadBlockedHandoff {
            issue_id: "u-1123".into(),
            started_at: at,
            question: "authorize the Event representation?".into(),
        })
        .await;
        let items = o.store().load_lead_items().expect("items");
        assert_eq!(items.len(), 1);
        assert!(
            matches!(&items[0].trigger, LeadTrigger::BlockedHandoff { ticket, question } if ticket == "STUDIO-1123" && question == "authorize the Event representation?")
        );
        assert!(o.human_holds.labelled_and_primed().0.is_empty());
    }

    #[test]
    fn lead_limit_wiring_only_queues_when_enabled() {
        for enabled in [false, true] {
            let o = orch(enabled);
            assert_eq!(o.enqueue_limit_judgment("claude-subscription"), enabled);
            let items = o.store().load_lead_items().expect("items");
            assert_eq!(items.len(), usize::from(enabled));
            assert!(o.running.is_empty());
        }
    }

    #[test]
    fn lead_current_manager_escalation_and_exhaustion_are_items() {
        for state in ["escalated", "exhausted"] {
            let o = orch(true);
            o.store()
                .ensure_review_generation("makewhatis/rhapsody#290")
                .expect("generation");
            o.store()
                .save_manager_intervention(rhapsody_store::ManagerInterventionRow {
                    id: "manager-case".into(),
                    pr: "makewhatis/rhapsody#290".into(),
                    generation: 1,
                    mode: "act".into(),
                    state: state.into(),
                    decision_head: "c6cf294".into(),
                    ..Default::default()
                })
                .expect("intervention");
            let mut feed = vec![divergence(DivergenceKind::ManagerDeferred)];
            o.route_lead_manager_endings(&mut feed);
            let items = o.store().load_lead_items().expect("items");
            assert_eq!(items.len(), 1);
            assert!(feed.is_empty());
            let expected = if state == "escalated" {
                "zero_verdict_escalation"
            } else {
                "repeated_failed_attempts"
            };
            assert_eq!(
                items[0].trigger,
                LeadTrigger::ImpossibleState {
                    subject: "makewhatis/rhapsody#290".into(),
                    kind: expected.into()
                }
            );
        }
    }
}
