//! Head-scoped CI route-back for ticketless reviews (STUDIO-1994); no Go counterpart.

use chrono::{DateTime, Duration, Utc};

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CiFailure {
    pub name: String,
    pub workflow: String,
    pub id: String,
    pub url: String,
    pub tests: Vec<String>,
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CiSnapshot {
    pub head: String,
    /// Routing fallback when no context in the complete inventory is marked required.
    pub all_checks: bool,
    pub failed: Vec<CiFailure>,
    pub pending: bool,
    /// Proven only from a complete PR inventory and file evidence for EVERY failing check.
    pub unrelated: bool,
    /// Current routing-gate jobs, including successful reruns, for the operator flake log.
    #[serde(default)]
    pub jobs: Vec<CiFailure>,
    #[serde(default)]
    pub rerun_error: Option<String>,
}

impl CiSnapshot {
    fn gate(&self) -> &'static str {
        if self.all_checks {
            "all checks (this repository marks none required)"
        } else {
            "required checks"
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CiAction {
    None,
    Rerun,
    Summon,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct CiState {
    pub head: String,
    pub ticket: String,
    pub summoned_at: Option<DateTime<Utc>>,
    rerun: Option<(DateTime<Utc>, Vec<CiFailure>)>,
    rerun_error: Option<String>,
    pub red: bool,
}

impl CiState {
    fn unanswered(
        &self,
        now: DateTime<Utc>,
        hours: u32,
        last_started: Option<DateTime<Utc>>,
    ) -> bool {
        self.red
            && self.summoned_at.is_some_and(|at| {
                now.signed_duration_since(at) >= Duration::hours(i64::from(hours))
                    && last_started.is_none_or(|started| started < at)
            })
    }
    fn decide(&mut self, ci: &CiSnapshot, now: DateTime<Utc>, held: bool, live: bool) -> CiAction {
        if ci.head.is_empty() {
            return CiAction::None;
        }
        if self.head != ci.head {
            *self = Self {
                head: ci.head.clone(),
                ..Self::default()
            };
        }
        self.red = !ci.pending && !ci.failed.is_empty();
        if !self.red || held || live || self.summoned_at.is_some() {
            return CiAction::None;
        }
        if let Some((_, previous)) = &self.rerun {
            // GitHub can keep showing the original completed attempt until the rerun queues.
            // Time passing is not evidence of a second failure.
            if ci.rerun_error.is_none()
                && self.rerun_error.is_none()
                && ci
                    .failed
                    .iter()
                    .any(|c| previous.iter().any(|p| p.id == c.id))
            {
                return CiAction::None;
            }
        } else if ci.unrelated && ci.failed.iter().all(|c| action_job(&c.url).is_some()) {
            self.rerun = Some((now, ci.failed.clone()));
            return CiAction::Rerun;
        }
        self.summoned_at = Some(now);
        CiAction::Summon
    }
}

fn durable_decide(
    store: &dyn rhapsody_store::Store,
    pr: &crate::prstate::PrCoord,
    state: &mut CiState,
    ci: &CiSnapshot,
    now: DateTime<Utc>,
    held: bool,
    live: bool,
) -> Result<CiAction, Box<dyn std::error::Error + Send + Sync>> {
    if state.head != ci.head {
        *state = CiState {
            head: ci.head.clone(),
            ..Default::default()
        };
    }
    let saved = store.get_ci_rerun(&pr.owner, &pr.repo, pr.number, &ci.head)?;
    if let Some(row) = &saved {
        let original: CiSnapshot = serde_json::from_str(&row.original)?;
        state.rerun = Some((
            DateTime::parse_from_rfc3339(&row.at)?.with_timezone(&Utc),
            original.failed,
        ));
        if row.outcome == "request_failed" {
            let result: CiSnapshot = serde_json::from_str(&row.result)?;
            state.rerun_error = result.rerun_error;
        }
    }
    if ci.rerun_error.is_some() {
        state.rerun_error = ci.rerun_error.clone();
    }
    let mut next = state.clone();
    let action = next.decide(ci, now, held, live);
    if action == CiAction::Rerun {
        let row = rhapsody_store::CiRerun {
            owner: pr.owner.clone(),
            repo: pr.repo.clone(),
            number: pr.number,
            head: ci.head.clone(),
            at: now.to_rfc3339(),
            original: serde_json::to_string(ci)?,
            ..Default::default()
        };
        if !store.reserve_ci_rerun(&row)? {
            if store
                .get_ci_rerun(&pr.owner, &pr.repo, pr.number, &ci.head)?
                .is_some()
            {
                return Ok(CiAction::None);
            }
            // No durable reservation means no GitHub write. Disabled storage retains immediate routing.
            next.rerun = None;
            next.summoned_at = None;
            let mut immediate = ci.clone();
            immediate.unrelated = false;
            let action = next.decide(&immediate, now, held, live);
            *state = next;
            return Ok(action);
        }
    }
    // Outcomes are observations, not notification effects: persist them even while held,
    // while the author is live, or while the notification performer is unavailable.
    let completed_rerun = !ci.pending
        && state.rerun_error.is_none()
        && state.rerun.as_ref().is_some_and(|(_, previous)| {
            if ci.failed.is_empty() {
                previous.iter().all(|old| {
                    ci.jobs.iter().any(|job| {
                        job.name == old.name && job.workflow == old.workflow && job.id != old.id
                    })
                })
            } else {
                ci.failed
                    .iter()
                    .all(|job| previous.iter().all(|old| old.id != job.id))
            }
        });
    if let Some(mut row) = saved
        && (ci.rerun_error.is_some() || completed_rerun)
    {
        row.outcome = if ci.rerun_error.is_some() {
            "request_failed"
        } else if ci.failed.is_empty() {
            "passed"
        } else {
            "failed"
        }
        .into();
        let result = serde_json::to_string(ci)?;
        if row.result != result {
            row.result = result;
            store.finish_ci_rerun(&row)?;
        }
    }
    *state = next;
    Ok(action)
}

/// Bounded operator log, projected from the same durable reservations used for dedupe.
pub fn flakes(store: &dyn rhapsody_store::Store) -> Result<serde_json::Value, String> {
    let mut entries = Vec::new();
    for row in store.list_ci_reruns().map_err(|e| e.to_string())? {
        let original: CiSnapshot =
            serde_json::from_str(&row.original).map_err(|e| e.to_string())?;
        let result: Option<CiSnapshot> = if row.result.is_empty() {
            None
        } else {
            Some(serde_json::from_str(&row.result).map_err(|e| e.to_string())?)
        };
        for check in original.failed {
            let rerun = result.as_ref().and_then(|ci| {
                ci.jobs
                    .iter()
                    .chain(&ci.failed)
                    .find(|job| job.name == check.name && job.workflow == check.workflow)
            });
            let outcome = if row.outcome == "failed"
                && rerun.is_some()
                && result.as_ref().is_some_and(|ci| {
                    !ci.failed
                        .iter()
                        .any(|job| job.name == check.name && job.workflow == check.workflow)
                }) {
                "passed"
            } else {
                &row.outcome
            };
            entries.push(serde_json::json!({
                "repo": format!("{}/{}", row.owner, row.repo), "pr": row.number,
                "head": row.head, "at": row.at, "check": check.name, "workflow": check.workflow,
                "tests": check.tests.iter().filter_map(|line| test_name(line)).collect::<Vec<_>>(), "original_job_url": check.url,
                "rerun_job_url": rerun.map(|job| &job.url), "outcome": outcome,
            }));
        }
    }
    Ok(serde_json::json!({"flakes": entries}))
}

fn test_name(line: &str) -> Option<&str> {
    if let Some((_, rest)) = line.split_once("--- FAIL:") {
        return rest.split_whitespace().next();
    }
    if let Some((_, rest)) = line.split_once("test ")
        && let Some((name, _)) = rest.split_once(" ... FAILED")
    {
        return Some(name.trim());
    }
    line.split_once('✘').map(|(_, rest)| rest.trim())
}

/// An Actions job link, with numeric IDs only; never turn an arbitrary details URL into argv.
pub(crate) fn action_job(url: &str) -> Option<(&str, &str)> {
    let (_, rest) = url.split_once("/actions/runs/")?;
    let (run, job) = rest.split_once("/job/")?;
    let job = job.split(['?', '#']).next()?;
    (!run.is_empty()
        && !job.is_empty()
        && run.bytes().all(|c| c.is_ascii_digit())
        && job.bytes().all(|c| c.is_ascii_digit()))
    .then_some((run, job))
}

pub(crate) fn test_lines(log: &str) -> Vec<String> {
    log.lines()
        .filter(|l| {
            l.contains("--- FAIL:")
                || l.contains('✘')
                || (l.contains("test ") && l.contains(" ... FAILED"))
        })
        .take(20)
        .map(|l| l.chars().take(500).collect())
        .collect()
}

pub(crate) fn test_paths(lines: &[String]) -> Vec<String> {
    lines
        .iter()
        .flat_map(|l| l.split_whitespace())
        .filter_map(|word| {
            let path = word.trim_matches(['\'', '"', '`']).split(':').next()?;
            (path.contains('/')
                && [
                    ".spec.ts",
                    ".spec.tsx",
                    ".spec.js",
                    ".test.ts",
                    ".test.tsx",
                    ".test.js",
                ]
                .iter()
                .any(|ext| path.ends_with(ext)))
            .then(|| path.trim_start_matches("./").to_string())
        })
        .collect()
}

pub(crate) fn parse_snapshot(value: &serde_json::Value, head: &str) -> Option<CiSnapshot> {
    let pr = &value["data"]["repository"]["pullRequest"];
    if pr["headRefOid"].as_str()? != head || pr["state"].as_str()? != "OPEN" {
        return None;
    }
    let commit = &pr["commits"]["nodes"].as_array()?.last()?["commit"];
    if commit["oid"].as_str()? != head {
        return None;
    }
    let contexts = &commit["statusCheckRollup"]["contexts"];
    if contexts["pageInfo"]["hasNextPage"].as_bool()? {
        return None;
    }
    let mut ci = CiSnapshot {
        head: head.into(),
        ..Default::default()
    };
    let nodes = contexts["nodes"].as_array()?;
    let mut has_required = false;
    for c in nodes {
        has_required |= c["isRequired"].as_bool()?;
    }
    ci.all_checks = !has_required;
    // A rerun at the same SHA can coexist with an older red attempt in the rollup. Only the
    // newest attempt of a context/app is authoritative, including when it is still queued.
    let mut latest = std::collections::BTreeMap::new();
    for c in nodes {
        let name = c["name"].as_str().or_else(|| c["context"].as_str())?;
        let app = c["checkSuite"]["app"]["slug"].as_str().unwrap_or("");
        let workflow = c["checkSuite"]["workflowRun"]["workflow"]["name"]
            .as_str()
            .unwrap_or("");
        let at = c["startedAt"]
            .as_str()
            .or_else(|| c["createdAt"].as_str())
            .unwrap_or("");
        let entry = latest.entry((name, app, workflow)).or_insert((at, c));
        let queued_without_time =
            at.is_empty() && c["__typename"] == "CheckRun" && c["status"] != "COMPLETED";
        let previous_queued_without_time = entry.0.is_empty()
            && entry.1["__typename"] == "CheckRun"
            && entry.1["status"] != "COMPLETED";
        if queued_without_time || (!previous_queued_without_time && at >= entry.0) {
            *entry = (at, c);
        }
    }
    for (_, c) in latest.into_values() {
        if !ci.all_checks && !c["isRequired"].as_bool()? {
            continue;
        }
        ci.jobs.push(CiFailure {
            name: c["name"].as_str().or_else(|| c["context"].as_str())?.into(),
            workflow: c["checkSuite"]["workflowRun"]["workflow"]["name"]
                .as_str()
                .unwrap_or("")
                .into(),
            id: c["id"].as_str().unwrap_or("").into(),
            url: c["detailsUrl"]
                .as_str()
                .or_else(|| c["targetUrl"].as_str())
                .unwrap_or("")
                .into(),
            ..Default::default()
        });
        let state = if c["__typename"] == "CheckRun" {
            if c["status"] != "COMPLETED" {
                ci.pending = true;
                continue;
            }
            c["conclusion"].as_str().unwrap_or("")
        } else {
            c["state"].as_str().unwrap_or("")
        };
        match state {
            "SUCCESS" | "NEUTRAL" | "SKIPPED" => {}
            "FAILURE" | "ERROR" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED"
            | "STARTUP_FAILURE" => {
                let name = c["name"].as_str().or_else(|| c["context"].as_str())?;
                let paths = c["annotations"]["nodes"]
                    .as_array()
                    .filter(|_| c["annotations"]["pageInfo"]["hasNextPage"] == false)
                    .map(|a| {
                        a.iter()
                            .filter(|a| a["annotationLevel"] == "FAILURE")
                            .filter_map(|a| {
                                a["path"]
                                    .as_str()
                                    .filter(|p| *p != ".github" && !p.is_empty())
                                    .map(str::to_string)
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                ci.failed.push(CiFailure {
                    name: name.into(),
                    workflow: c["checkSuite"]["workflowRun"]["workflow"]["name"]
                        .as_str()
                        .unwrap_or("")
                        .into(),
                    id: c["id"].as_str().unwrap_or(name).into(),
                    url: c["detailsUrl"]
                        .as_str()
                        .or_else(|| c["targetUrl"].as_str())
                        .unwrap_or("")
                        .into(),
                    paths,
                    ..Default::default()
                });
            }
            _ => ci.pending = true,
        }
    }
    Some(ci)
}

impl crate::orchestrator::Orchestrator {
    pub(crate) fn ci_divergences(
        &self,
        now: DateTime<Utc>,
    ) -> Vec<crate::reviewreconcile::Divergence> {
        use crate::reviewreconcile::{Divergence, DivergenceKind};
        let Some(eff) = &self.eff else {
            return Vec::new();
        };
        if !eff.cfg.review.ci_failure_routing {
            return Vec::new();
        }
        let (held, primed) = self.human_holds.labelled_and_primed();
        if !primed {
            return Vec::new();
        }
        let rows = match self.store().load_live_review_watch() {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!(%error, "CI reconciliation: watch rows unreadable");
                return Vec::new();
            }
        };
        let mut found = Vec::new();
        for (pr, state) in &self.ci_routed {
            if !state.red
                || state.summoned_at.is_none()
                || held.contains(&state.ticket.to_ascii_lowercase())
                || !rows.iter().any(|r| {
                    r.key.owner == pr.owner
                        && r.key.repo == pr.repo
                        && r.key.number == pr.number
                        && crate::reviewdone::origin_ticket(&r.introduced_by)
                            == Some(state.ticket.as_str())
                })
                || self
                    .running
                    .values()
                    .any(|r| r.issue.identifier.eq_ignore_ascii_case(&state.ticket))
                || !self
                    .review_observed_head
                    .get(pr)
                    .is_some_and(|o| o.open && o.head == state.head)
            {
                continue;
            }
            let runs = match self.store().list_issue_runs(rhapsody_store::RunFilter {
                issue: state.ticket.clone(),
                limit: 1,
                ..Default::default()
            }) {
                Ok(runs) => runs,
                Err(error) => {
                    tracing::warn!(%pr, %error, "CI reconciliation: run ledger unreadable");
                    continue;
                }
            };
            let last_started = runs
                .first()
                .and_then(|r| DateTime::parse_from_rfc3339(&r.started_at).ok())
                .map(|d| d.with_timezone(&Utc));
            if !state.unanswered(
                now,
                eff.cfg.review.ci_failure_escalation_hours,
                last_started,
            ) {
                continue;
            }
            found.push(Divergence {
                pr: pr.to_string(), kind: DivergenceKind::CiFailureNoRun, ticket: state.ticket.clone(),
                reviewer: String::new(), stale_secs: state.summoned_at.map_or(0, |at| now.signed_duration_since(at).num_seconds()),
                auto_merge_reason: None, capacity_held: None, capacity_unreadable: None,
                adjudicated_head: String::new(), current_head: state.head.clone(), rounds: 0, findings: Vec::new(),
                reason: format!("CI is red at {}; its author has not answered the summons after {} hours — needs a human; inspect the failed run and author dispatch/credentials", state.head, eff.cfg.review.ci_failure_escalation_hours),
            });
        }
        found
    }

    pub(crate) fn observe_review_ci(
        &mut self,
        pr: &crate::prstate::PrCoord,
        ci: &CiSnapshot,
    ) -> CiAction {
        if !self
            .eff
            .as_ref()
            .is_some_and(|e| e.cfg.review.ci_failure_routing)
            || !self.review_ticketless_enabled()
        {
            return CiAction::None;
        }
        if !self
            .review_observed_head
            .get(pr)
            .is_some_and(|o| o.open && o.head == ci.head)
        {
            return CiAction::None;
        }
        let rows = match self.store().load_live_review_watch() {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(%pr, err = %e, "CI route-back: watch rows unreadable");
                return CiAction::None;
            }
        };
        let Some(row) = rows.iter().find(|r| {
            r.key.owner == pr.owner && r.key.repo == pr.repo && r.key.number == pr.number
        }) else {
            return CiAction::None;
        };
        let Some(ticket) = crate::reviewdone::origin_ticket(&row.introduced_by) else {
            return CiAction::None;
        };
        let (held, primed) = self.human_holds.labelled_and_primed();
        let live = self
            .running
            .values()
            .any(|r| r.issue.identifier.eq_ignore_ascii_case(ticket));
        let performer_missing = self
            .review_notify_tx
            .as_ref()
            .is_none_or(|tx| tx.is_closed());
        // Still record recovery/pending observations if the notification task is unavailable;
        // otherwise stale red evidence could keep escalating a check that has already passed.
        let held = !primed || held.contains(&ticket.to_ascii_lowercase()) || performer_missing;
        let mut state = self.ci_routed.get(pr).cloned().unwrap_or_default();
        let action = match durable_decide(
            self.store(),
            pr,
            &mut state,
            ci,
            (self.now)(),
            held,
            live,
        ) {
            Ok(action) => action,
            Err(error) => {
                tracing::warn!(%pr, %error, "CI route-back: durable rerun state unavailable; taking no action");
                return CiAction::None;
            }
        };
        state.ticket = ticket.into();
        let original = state.rerun.as_ref().map(|(_, failures)| failures.clone());
        let mut evidence = ci.clone();
        evidence.rerun_error = state.rerun_error.clone();
        self.ci_routed.insert(pr.clone(), state);
        if action == CiAction::None {
            return action;
        }
        let line = if action == CiAction::Rerun {
            format!(
                "CI route-back on {pr} at {} using {}: failed check file evidence touches no file in the PR diff; rerunning failed jobs once before summoning {ticket}.",
                ci.head,
                ci.gate()
            )
        } else {
            format!(
                "CI route-back on {pr} at {}: summoning {} ({ticket}) for failed {}: {}.",
                ci.head,
                row.author,
                ci.gate(),
                ci.failed
                    .iter()
                    .map(|c| c.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        tracing::info!(%pr, head = %ci.head, "{line}");
        if let Some(room) = &self.teams_room
            && let Err(e) = room.append(
                &rhapsody_config::room::Message::room("manager", (self.now)(), line)
                    .with_refs([pr.to_string()]),
            )
        {
            tracing::warn!(%pr, err = %e, "CI route-back: room post failed");
        }
        if action == CiAction::Summon {
            if let Some(ledger) = &self.automerge_ledger {
                ledger.remove_from_line(pr);
            }
            let completion = crate::reviewnotify::ReviewCompletion {
                reason: crate::reviewnotify::CompletionReason::CiFailure,
                owner: pr.owner.clone(),
                repo: pr.repo.clone(),
                number: pr.number,
                author: row.author.clone(),
                head_sha: ci.head.clone(),
                verdict_text: failure_details(&evidence, original.as_deref()),
                summon_token: self.review_summon_token(),
                changes: self.plan_conflict_route_back(pr, &row.introduced_by),
                ..Default::default()
            };
            self.request_review_notify(Some(completion));
        }
        action
    }
}

fn failure_details(ci: &CiSnapshot, original: Option<&[CiFailure]>) -> String {
    let mut out = format!("Routing gate: {}.\n\n", ci.gate());
    for c in &ci.failed {
        out.push_str(&format!(
            "- {} / {} — {}\n",
            one_line(&c.workflow),
            one_line(&c.name),
            one_line(&c.url)
        ));
        for line in &c.tests {
            out.push_str(&format!("  > {}\n", line.replace(['\r', '\n'], " ")));
        }
    }
    if ci.unrelated {
        out.push_str("\nThe failing checks' file evidence touches no file in this PR's diff; this may be unrelated or flaky.\n");
    }
    if let Some(original) = original.filter(|o| !o.is_empty()) {
        if let Some(error) = &ci.rerun_error {
            out.push_str(&format!(
                "\nThe daemon's one rerun request failed: {}.\n",
                one_line(error)
            ));
        } else {
            out.push_str("\nThe failed jobs were rerun once and failed again at this head. Original job links:\n");
        }
        for c in original {
            out.push_str(&format!("- {} — {}\n", one_line(&c.name), one_line(&c.url)));
        }
    }
    if out.len() > 48_000 {
        let mut end = 48_000;
        while !out.is_char_boundary(end) {
            end -= 1;
        }
        out.truncate(end);
        out.push_str("\n[CI evidence truncated; follow the run links for the full logs.]\n");
    }
    out
}

fn one_line(s: &str) -> String {
    s.chars()
        .take(2_000)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ghsummons::{GH, PrStateSource};

    #[test]
    fn deferred_request_failure_survives_restart_without_resending() {
        use rhapsody_store::{Sqlite, Store, StorePath};
        for (held, live) in [(true, false), (false, true)] {
            let dir = crate::testsupport::TempDir::new();
            let path = std::path::PathBuf::from(dir.child("deferred.db"));
            let store = Sqlite::open(StorePath::Disk(path.clone())).unwrap();
            let pr = crate::prstate::PrCoord::new("o", "r", 1);
            let now = Utc::now();
            let mut ci = red("a");
            ci.unrelated = true;
            ci.failed[0].url = "https://github.com/o/r/actions/runs/1/job/2".into();
            let mut state = CiState::default();
            assert_eq!(
                durable_decide(&store, &pr, &mut state, &ci, now, false, false).unwrap(),
                CiAction::Rerun
            );
            let mut error = ci.clone();
            error.rerun_error = Some("HTTP 403".into());
            assert_eq!(
                durable_decide(&store, &pr, &mut state, &error, now, held, live).unwrap(),
                CiAction::None
            );
            assert_eq!(store.list_ci_reruns().unwrap()[0].outcome, "request_failed");
            drop(store);
            let store = Sqlite::open(StorePath::Disk(path)).unwrap();
            let mut state = CiState::default();
            assert_eq!(
                durable_decide(
                    &store,
                    &pr,
                    &mut state,
                    &ci,
                    now + Duration::hours(24),
                    false,
                    false
                )
                .unwrap(),
                CiAction::Summon
            );
            assert_eq!(store.list_ci_reruns().unwrap().len(), 1);
            assert_eq!(store.list_ci_reruns().unwrap()[0].outcome, "request_failed");
        }
    }

    #[test]
    fn deferred_failed_rerun_is_logged_before_notification() {
        use rhapsody_store::{Sqlite, Store, StorePath};
        for (held, live) in [(true, false), (false, true)] {
            let store = Sqlite::open(StorePath::InMemory).unwrap();
            let pr = crate::prstate::PrCoord::new("o", "r", 1);
            let now = Utc::now();
            let mut ci = red("a");
            ci.unrelated = true;
            ci.failed[0].url = "https://github.com/o/r/actions/runs/1/job/2".into();
            let mut state = CiState::default();
            assert_eq!(
                durable_decide(&store, &pr, &mut state, &ci, now, false, false).unwrap(),
                CiAction::Rerun
            );
            ci.failed[0].id = "new".into();
            ci.failed[0].url = "https://github.com/o/r/actions/runs/1/job/3".into();
            assert_eq!(
                durable_decide(&store, &pr, &mut state, &ci, now, held, live).unwrap(),
                CiAction::None
            );
            assert_eq!(store.list_ci_reruns().unwrap()[0].outcome, "failed");
            let log = flakes(&store).unwrap();
            assert_eq!(
                log["flakes"][0]["original_job_url"],
                "https://github.com/o/r/actions/runs/1/job/2"
            );
            assert_eq!(
                log["flakes"][0]["rerun_job_url"],
                "https://github.com/o/r/actions/runs/1/job/3"
            );
            assert_eq!(
                durable_decide(&store, &pr, &mut CiState::default(), &ci, now, false, false)
                    .unwrap(),
                CiAction::Summon
            );
        }
    }

    #[test]
    fn durable_rerun_survives_restart_and_records_both_attempts() {
        use rhapsody_store::{Sqlite, Store, StorePath};
        let dir = crate::testsupport::TempDir::new();
        let path = std::path::PathBuf::from(dir.child("reruns.db"));
        let store = Sqlite::open(StorePath::Disk(path.clone())).unwrap();
        let pr = crate::prstate::PrCoord::new("o", "r", 1);
        let now = Utc::now();
        let mut ci = red("a");
        ci.unrelated = true;
        ci.failed[0].url = "https://github.com/o/r/actions/runs/1/job/2".into();
        assert_eq!(
            durable_decide(&store, &pr, &mut CiState::default(), &ci, now, false, false).unwrap(),
            CiAction::Rerun
        );
        drop(store);
        let store = Sqlite::open(StorePath::Disk(path)).unwrap();
        let decide = |state: &mut CiState, ci: &CiSnapshot| {
            durable_decide(&store, &pr, state, ci, now, false, false).unwrap()
        };
        let mut restarted = CiState::default();
        assert_eq!(decide(&mut restarted, &ci), CiAction::None);
        assert_eq!(
            durable_decide(
                &store,
                &pr,
                &mut restarted,
                &ci,
                now + Duration::hours(1),
                false,
                false
            )
            .unwrap(),
            CiAction::None,
            "an unchanged old attempt is not a failed rerun"
        );
        ci.failed[0].id = "new".into();
        ci.failed[0].url = "https://github.com/o/r/actions/runs/1/job/3".into();
        assert_eq!(decide(&mut restarted, &ci), CiAction::Summon);
        let details = failure_details(
            &ci,
            restarted
                .rerun
                .as_ref()
                .map(|(_, failures)| failures.as_slice()),
        );
        assert!(details.contains("rerun once and failed again"));
        assert!(details.contains("/job/2"));
        assert!(details.contains("/job/3"));
        assert_eq!(store.list_ci_reruns().unwrap().len(), 1);
        assert_eq!(store.list_ci_reruns().unwrap()[0].outcome, "failed");
        let log = flakes(&store).unwrap();
        assert_eq!(
            log["flakes"][0]["original_job_url"],
            "https://github.com/o/r/actions/runs/1/job/2"
        );
        assert_eq!(
            log["flakes"][0]["rerun_job_url"],
            "https://github.com/o/r/actions/runs/1/job/3"
        );
    }

    #[test]
    fn durable_rerun_passes_without_summoning_and_waits_for_running_checks() {
        use rhapsody_store::{Sqlite, Store, StorePath};
        let store = Sqlite::open(StorePath::InMemory).unwrap();
        let pr = crate::prstate::PrCoord::new("o", "r", 1);
        let mut ci = red("a");
        ci.unrelated = true;
        ci.failed[0].url = "https://github.com/o/r/actions/runs/1/job/2".into();
        let mut state = CiState::default();
        ci.pending = true;
        assert_eq!(
            durable_decide(&store, &pr, &mut state, &ci, Utc::now(), false, false).unwrap(),
            CiAction::None
        );
        assert!(store.list_ci_reruns().unwrap().is_empty());
        ci.pending = false;
        assert_eq!(
            durable_decide(&store, &pr, &mut state, &ci, Utc::now(), false, false).unwrap(),
            CiAction::Rerun
        );
        let mut job = ci.failed[0].clone();
        ci.failed.clear();
        ci.jobs = vec![job.clone()];
        assert_eq!(
            durable_decide(&store, &pr, &mut state, &ci, Utc::now(), false, false).unwrap(),
            CiAction::None
        );
        assert_eq!(
            store.list_ci_reruns().unwrap()[0].outcome,
            "requested",
            "a stale or disappearing original failure does not prove a green rerun"
        );
        job.id = "new".into();
        job.url = "https://github.com/o/r/actions/runs/1/job/3".into();
        ci.jobs = vec![job];
        assert_eq!(
            durable_decide(
                &store,
                &pr,
                &mut CiState::default(),
                &ci,
                Utc::now(),
                false,
                false
            )
            .unwrap(),
            CiAction::None
        );
        assert_eq!(store.list_ci_reruns().unwrap()[0].outcome, "passed");
        let log = flakes(&store).unwrap();
        assert_eq!(
            log["flakes"][0]["rerun_job_url"],
            "https://github.com/o/r/actions/runs/1/job/3"
        );
    }

    #[test]
    fn evidence_inside_the_diff_routes_immediately_without_a_reservation() {
        use rhapsody_store::{Sqlite, Store, StorePath};
        let store = Sqlite::open(StorePath::InMemory).unwrap();
        let pr = crate::prstate::PrCoord::new("o", "r", 1);
        let mut state = CiState::default();
        let ci = red("a");
        assert_eq!(
            durable_decide(&store, &pr, &mut state, &ci, Utc::now(), false, false).unwrap(),
            CiAction::Summon
        );
        assert_eq!(
            durable_decide(&store, &pr, &mut state, &ci, Utc::now(), false, false).unwrap(),
            CiAction::None
        );
        assert!(store.list_ci_reruns().unwrap().is_empty());
    }

    #[test]
    fn flake_test_names_parse_go_rust_and_browser_failure_lines() {
        assert_eq!(
            test_name("2026-11-17 --- FAIL: TestUsage (0.1s)"),
            Some("TestUsage")
        );
        assert_eq!(
            test_name("test tests::usage ... FAILED"),
            Some("tests::usage")
        );
        assert_eq!(
            test_name("✘ 1 tests/usage.spec.ts:42 › usage totals"),
            Some("1 tests/usage.spec.ts:42 › usage totals")
        );
        assert_eq!(test_name("not a failing test"), None);
    }

    fn check_payload(state: &str) -> serde_json::Value {
        serde_json::json!({"data":{"repository":{"pullRequest":{
            "state":"OPEN", "headRefOid":"a", "commits":{"nodes":[{"commit":{"oid":"a",
            "statusCheckRollup":{"contexts":{"pageInfo":{"hasNextPage":false},"nodes":[{
                "__typename":"CheckRun", "id":"job-1", "name":"go test", "status":"COMPLETED", "conclusion":state,
                "isRequired":true,"detailsUrl":"https://github.com/o/r/actions/runs/123/job/456",
                "checkSuite":{"workflowRun":{"workflow":{"name":"ci"}}}, "annotations":{"pageInfo":{"hasNextPage":false},"nodes":[{"path":"tests/usage_test.go","annotationLevel":"FAILURE"}]}
            }]}}}}]}
        }}}})
    }

    #[tokio::test]
    async fn fake_github_required_checks_and_job_logs_form_the_author_evidence() {
        for required in [true, false] {
            let mut payload = check_payload("FAILURE");
            payload["data"]["repository"]["pullRequest"]["commits"]["nodes"][0]["commit"]["statusCheckRollup"]
                ["contexts"]["nodes"][0]["isRequired"] = required.into();
            let gh = GH::new(
                "",
                Some(Box::new(move |args| {
                    if args[..2] == ["api", "graphql"] {
                        assert!(
                            args.iter()
                                .any(|a| a.contains("isRequired(pullRequestNumber:$number)"))
                        );
                        return Ok(serde_json::to_vec(&payload).unwrap());
                    }
                    if args[0] == "api" {
                        return match args[1] {
                    "repos/o/r/pulls/1" => Ok(br#"{"head":{"sha":"a"},"base":{"sha":"base"},"changed_files":1,"labels":[]}"#.to_vec()),
                     "repos/o/r/compare/base...a" => Ok(br#"{"files":[{"filename":"src/usage.go","status":"modified"}]}"#.to_vec()),
                     "repos/o/r/actions/runs/123" => Ok(br#"{"head_sha":"a","status":"completed"}"#.to_vec()),
                    other => panic!("unexpected endpoint: {other}"),
                };
                    }
                    assert_eq!(
                        args,
                        [
                            "run",
                            "view",
                            "--repo",
                            "o/r",
                            "--job",
                            "456",
                            "--log-failed"
                        ]
                    );
                    Ok("noise\n2026-11-13 --- FAIL: TestUsageTotals (0.02s)\n2026-11-13 ✘ 1 tests/usage.spec.ts:42\nmore noise".as_bytes().to_vec())
                })),
            );
            let ci = gh
                .review_ci(&crate::prstate::PrCoord::new("o", "r", 1), "a", None)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(ci.failed.len(), 1);
            assert_eq!(ci.all_checks, !required);
            assert_eq!(ci.failed[0].workflow, "ci");
            assert!(ci.unrelated);
            assert_eq!(ci.failed[0].tests.len(), 2);
            assert!(ci.failed[0].tests[0].contains("TestUsageTotals"));
            let details = failure_details(&ci, None);
            assert!(details.contains("usage.spec.ts:42"));
            assert!(details.contains("https://github.com/o/r/actions/runs/123/job/456"));
            let mut previous = ci.clone();
            previous.all_checks = !ci.all_checks;
            let refreshed = gh
                .review_ci(
                    &crate::prstate::PrCoord::new("o", "r", 1),
                    "a",
                    Some(&previous),
                )
                .await
                .unwrap()
                .unwrap();
            assert_eq!(refreshed.all_checks, ci.all_checks);
            let wrong_head = gh
                .review_ci(&crate::prstate::PrCoord::new("o", "r", 1), "b", None)
                .await
                .unwrap();
            assert!(wrong_head.is_none());
        }
    }

    #[tokio::test]
    async fn unrelated_failure_waits_for_the_workflow_even_if_its_required_job_finished() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        let finished = Arc::new(AtomicBool::new(false));
        let observed = finished.clone();
        let gh = GH::new(
            "",
            Some(Box::new(move |args| {
                if args[..2] == ["api", "graphql"] {
                    return Ok(serde_json::to_vec(&check_payload("FAILURE")).unwrap());
                }
                if args[0] == "api" {
                    return match args[1] {
                    "repos/o/r/pulls/1" => Ok(br#"{"head":{"sha":"a"},"base":{"sha":"base"},"changed_files":1,"labels":[]}"#.to_vec()),
                    "repos/o/r/compare/base...a" => Ok(br#"{"files":[{"filename":"src/usage.go","status":"modified"}]}"#.to_vec()),
                    "repos/o/r/actions/runs/123" => Ok(serde_json::to_vec(&serde_json::json!({"head_sha":"a", "status":if observed.load(Ordering::SeqCst) { "completed" } else { "in_progress" }})).unwrap()),
                    endpoint => panic!("unexpected endpoint {endpoint}"),
                };
                }
                assert_eq!(args[0], "run");
                assert_eq!(args[1], "view");
                Ok(Vec::new())
            })),
        );
        let pr = crate::prstate::PrCoord::new("o", "r", 1);
        let ci = gh.review_ci(&pr, "a", None).await.unwrap().unwrap();
        assert!(ci.pending);
        let store = rhapsody_store::Sqlite::open(rhapsody_store::StorePath::InMemory).unwrap();
        let mut state = CiState::default();
        assert_eq!(
            durable_decide(&store, &pr, &mut state, &ci, Utc::now(), false, false).unwrap(),
            CiAction::None
        );
        finished.store(true, Ordering::SeqCst);
        let ci = gh.review_ci(&pr, "a", Some(&ci)).await.unwrap().unwrap();
        assert!(!ci.pending);
        assert_eq!(
            durable_decide(&store, &pr, &mut state, &ci, Utc::now(), false, false).unwrap(),
            CiAction::Rerun
        );
    }

    #[test]
    fn required_ci_ignores_optional_failures_and_unsettled_or_truncated_answers() {
        let mut payload = check_payload("FAILURE");
        let nodes = &mut payload["data"]["repository"]["pullRequest"]["commits"]["nodes"][0]["commit"]
            ["statusCheckRollup"]["contexts"];
        nodes["nodes"][0]["isRequired"] = false.into();
        let mut required = nodes["nodes"][0].clone();
        required["name"] = "required passing check".into();
        required["isRequired"] = true.into();
        required["conclusion"] = "SUCCESS".into();
        nodes["nodes"].as_array_mut().unwrap().push(required);
        assert!(parse_snapshot(&payload, "a").unwrap().failed.is_empty());
        let mut payload = check_payload("FAILURE");
        payload["data"]["repository"]["pullRequest"]["commits"]["nodes"][0]["commit"]["statusCheckRollup"]
            ["contexts"]["nodes"][0]["status"] = "QUEUED".into();
        let ci = parse_snapshot(&payload, "a").unwrap();
        assert!(ci.pending);
        assert!(ci.failed.is_empty());
        payload["data"]["repository"]["pullRequest"]["commits"]["nodes"][0]["commit"]["statusCheckRollup"]
            ["contexts"]["pageInfo"]["hasNextPage"] = true.into();
        assert!(parse_snapshot(&payload, "a").is_none());
        assert!(
            parse_snapshot(&check_payload("SUCCESS"), "a")
                .unwrap()
                .failed
                .is_empty()
        );
    }

    #[test]
    fn no_required_checks_summons_the_author_and_defers_pending_or_truncated_answers() {
        let mut payload = check_payload("FAILURE");
        payload["data"]["repository"]["pullRequest"]["commits"]["nodes"][0]["commit"]["statusCheckRollup"]
            ["contexts"]["nodes"][0]["isRequired"] = false.into();
        let ci = parse_snapshot(&payload, "a").unwrap();
        let now = Utc::now();
        let mut state = CiState::default();
        assert_eq!(state.decide(&ci, now, false, false), CiAction::Summon);
        assert_eq!(state.decide(&ci, now, false, false), CiAction::None);

        let contexts = &mut payload["data"]["repository"]["pullRequest"]["commits"]["nodes"][0]["commit"]
            ["statusCheckRollup"]["contexts"];
        let mut pending = contexts["nodes"][0].clone();
        pending["name"] = "pending check".into();
        pending["status"] = "QUEUED".into();
        contexts["nodes"].as_array_mut().unwrap().push(pending);
        let ci = parse_snapshot(&payload, "a").unwrap();
        assert!(ci.pending);
        assert_eq!(ci.failed.len(), 1);
        assert_eq!(
            CiState::default().decide(&ci, now, false, false),
            CiAction::None
        );
        payload["data"]["repository"]["pullRequest"]["commits"]["nodes"][0]["commit"]["statusCheckRollup"]
            ["contexts"]["pageInfo"]["hasNextPage"] = true.into();
        assert!(parse_snapshot(&payload, "a").is_none());
    }

    #[test]
    fn a_passed_rerun_at_the_same_head_supersedes_old_red_and_a_queued_rerun_waits() {
        let mut payload = check_payload("FAILURE");
        let nodes = payload["data"]["repository"]["pullRequest"]["commits"]["nodes"][0]["commit"]
            ["statusCheckRollup"]["contexts"]["nodes"].as_array_mut().unwrap();
        nodes[0]["startedAt"] = "2026-11-13T07:00:00Z".into();
        let mut rerun = nodes[0].clone();
        rerun["id"] = "job-2".into();
        rerun["startedAt"] = "2026-11-13T08:00:00Z".into();
        rerun["conclusion"] = "SUCCESS".into();
        nodes.insert(0, rerun);
        assert!(
            parse_snapshot(&payload, "a").unwrap().failed.is_empty(),
            "API ordering must not revive an old failure"
        );
        let rerun = &mut payload["data"]["repository"]["pullRequest"]["commits"]["nodes"][0]["commit"]
            ["statusCheckRollup"]["contexts"]["nodes"][0];
        rerun["status"] = "QUEUED".into();
        rerun["startedAt"] = serde_json::Value::Null;
        rerun["conclusion"] = serde_json::Value::Null;
        let ci = parse_snapshot(&payload, "a").unwrap();
        assert!(ci.pending);
        assert!(ci.failed.is_empty());
    }

    #[tokio::test]
    async fn one_rerun_reaches_github_with_failed_only_and_the_scoped_repo() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let gh = GH::new(
            "",
            Some(Box::new(move |args| {
                if args[0] == "pr" {
                    return Ok(br#"{"headRefOid":"a","state":"OPEN"}"#.to_vec());
                }
                assert_eq!(args, ["run", "rerun", "123", "--repo", "o/r", "--failed"]);
                seen.fetch_add(1, Ordering::SeqCst);
                Ok(Vec::new())
            })),
        );
        let mut ci = red("a");
        ci.unrelated = true;
        ci.failed[0].url = "https://github.com/o/r/actions/runs/123/job/456".into();
        let mut state = CiState::default();
        for _ in 0..2 {
            if state.decide(&ci, Utc::now(), false, false) == CiAction::Rerun {
                gh.rerun_ci(&crate::prstate::PrCoord::new("o", "r", 1), &ci)
                    .await
                    .unwrap();
            }
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        ci.head = "b".into();
        gh.rerun_ci(&crate::prstate::PrCoord::new("o", "r", 1), &ci)
            .await
            .unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "a moved head cannot rerun old CI"
        );
    }

    fn red(head: &str) -> CiSnapshot {
        CiSnapshot {
            head: head.into(),
            failed: vec![CiFailure {
                name: "test".into(),
                id: "1".into(),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn ci_routing_is_once_per_head_and_defers_pending_human_and_live_author() {
        let mut state = CiState::default();
        let now = chrono::Utc::now();
        let mut snapshot = red("a");
        for (held, live, pending) in [
            (true, false, false),
            (false, true, false),
            (false, false, true),
        ] {
            snapshot.pending = pending;
            assert_eq!(state.decide(&snapshot, now, held, live), CiAction::None);
        }
        snapshot.pending = false;
        assert_eq!(state.decide(&snapshot, now, false, false), CiAction::Summon);
        assert_eq!(state.decide(&snapshot, now, false, false), CiAction::None);
        assert_eq!(state.decide(&red("b"), now, false, false), CiAction::Summon);
    }

    #[test]
    fn unrelated_ci_gets_one_rerun_then_a_summons_or_recovers() {
        let now = chrono::Utc::now();
        let mut state = CiState::default();
        let mut snapshot = red("a");
        snapshot.unrelated = true;
        snapshot.failed[0].url = "https://github.com/o/r/actions/runs/1/job/2".into();
        assert_eq!(state.decide(&snapshot, now, false, false), CiAction::Rerun);
        assert_eq!(state.decide(&snapshot, now, false, false), CiAction::None);
        snapshot.pending = true;
        assert_eq!(state.decide(&snapshot, now, false, false), CiAction::None);
        snapshot.pending = false;
        snapshot.failed[0].id = "2".into();
        assert_eq!(state.decide(&snapshot, now, false, false), CiAction::Summon);
        assert_eq!(state.decide(&snapshot, now, false, false), CiAction::None);
        snapshot.failed.clear();
        assert_eq!(state.decide(&snapshot, now, false, false), CiAction::None);
        assert!(!state.red);
    }

    #[test]
    fn ci_escalation_honors_configured_hours_and_an_author_answer() {
        let now = chrono::Utc::now();
        let mut state = CiState::default();
        state.decide(&red("a"), now, false, false);
        assert!(!state.unanswered(now + Duration::minutes(119), 2, None));
        assert!(state.unanswered(now + Duration::hours(2), 2, None));
        assert!(!state.unanswered(now + Duration::hours(2), 3, None));
        assert!(!state.unanswered(
            now + Duration::hours(2),
            2,
            Some(now + Duration::minutes(1))
        ));
    }
}
