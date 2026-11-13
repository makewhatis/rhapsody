//! Head-scoped CI route-back for ticketless reviews (STUDIO-1994); no Go counterpart.

use chrono::{DateTime, Duration, Utc};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CiFailure {
    pub name: String,
    pub workflow: String,
    pub id: String,
    pub url: String,
    pub tests: Vec<String>,
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CiSnapshot {
    pub head: String,
    pub failed: Vec<CiFailure>,
    pub pending: bool,
    /// Proven only from a complete PR inventory and file evidence for EVERY failing check.
    pub unrelated: bool,
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
    rerun: Option<(DateTime<Utc>, Vec<String>)>,
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
        let ids: Vec<String> = ci.failed.iter().map(|c| c.id.clone()).collect();
        if let Some((at, previous)) = &self.rerun {
            // GitHub may still show the old completed attempt before the rerun queues. Do not
            // summon on that old answer. A failed/never-started rerun cannot silence it forever.
            if *previous == ids && now.signed_duration_since(*at) < Duration::minutes(5) {
                return CiAction::None;
            }
        } else if ci.unrelated && ci.failed.iter().all(|c| action_job(&c.url).is_some()) {
            self.rerun = Some((now, ids));
            return CiAction::Rerun;
        }
        self.summoned_at = Some(now);
        CiAction::Summon
    }
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
        .filter(|l| l.contains("--- FAIL:") || l.contains('✘'))
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
    // A rerun at the same SHA can coexist with an older red attempt in the rollup. Only the
    // newest attempt of a context/app is authoritative, including when it is still queued.
    let mut latest = std::collections::BTreeMap::new();
    for c in contexts["nodes"].as_array()? {
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
        if !c["isRequired"].as_bool()? {
            continue;
        }
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
                reason: format!("required CI is red at {}; its author has not answered the summons after {} hours — needs a human; inspect the failed run and author dispatch/credentials", state.head, eff.cfg.review.ci_failure_escalation_hours),
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
        let state = self.ci_routed.entry(pr.clone()).or_default();
        let action = state.decide(ci, (self.now)(), held, live);
        state.ticket = ticket.into();
        if action == CiAction::None {
            return action;
        }
        let reran = state.rerun.is_some();
        let line = if action == CiAction::Rerun {
            format!(
                "CI route-back on {pr} at {}: failed check file evidence touches no file in the PR diff; rerunning failed jobs once before summoning {ticket}.",
                ci.head
            )
        } else {
            format!(
                "CI route-back on {pr} at {}: summoning {} ({ticket}) for failed required checks: {}.",
                ci.head,
                row.author,
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
            let completion = crate::reviewnotify::ReviewCompletion {
                reason: crate::reviewnotify::CompletionReason::CiFailure,
                owner: pr.owner.clone(),
                repo: pr.repo.clone(),
                number: pr.number,
                author: row.author.clone(),
                head_sha: ci.head.clone(),
                verdict_text: failure_details(ci, reran),
                summon_token: self.review_summon_token(),
                changes: self.plan_conflict_route_back(pr, &row.introduced_by),
                ..Default::default()
            };
            self.request_review_notify(Some(completion));
        }
        action
    }
}

fn failure_details(ci: &CiSnapshot, reran: bool) -> String {
    let mut out = String::new();
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
        out.push_str("\nThe failing checks' file evidence touches no file in this PR's diff; this may be unrelated or flaky. Try a rerun first.\n");
    }
    if reran {
        out.push_str("\nThe daemon already requested one rerun of the failed jobs at this head.\n");
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
        let payload = check_payload("FAILURE");
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
        assert_eq!(ci.failed[0].workflow, "ci");
        assert!(ci.unrelated);
        assert_eq!(ci.failed[0].tests.len(), 2);
        assert!(ci.failed[0].tests[0].contains("TestUsageTotals"));
        let details = failure_details(&ci, false);
        assert!(details.contains("usage.spec.ts:42"));
        assert!(details.contains("https://github.com/o/r/actions/runs/123/job/456"));
        let wrong_head = gh
            .review_ci(&crate::prstate::PrCoord::new("o", "r", 1), "b", None)
            .await
            .unwrap();
        assert!(wrong_head.is_none());
    }

    #[test]
    fn required_ci_ignores_optional_failures_and_unsettled_or_truncated_answers() {
        let mut payload = check_payload("FAILURE");
        let nodes = &mut payload["data"]["repository"]["pullRequest"]["commits"]["nodes"][0]["commit"]
            ["statusCheckRollup"]["contexts"];
        nodes["nodes"][0]["isRequired"] = false.into();
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
