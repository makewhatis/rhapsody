//! snapshot — parity port of Go `internal/orchestrator/snapshot.go`.
//!
//! The synchronous runtime view (upstream §13.3): the data model P6 serves as `GET /api/v1/state`
//! (rendered by [`crate::snapshot_json`]). [`Orchestrator::build_snapshot`] assembles it from the
//! current scheduling state on the control task — runtime is reported as a LIVE aggregate
//! (ended-session seconds + active elapsed, plus the in-flight `cur_*` token estimate) so the
//! dashboard ticks up mid-turn (upstream §13.5).
//!
//! Deviations from Go, all serial-chain deferrals:
//!   * `Snapshot`/`Refresh` (the channel round-trip that requests a snapshot from the control task)
//!     plus `ErrSnapshotTimeout`/`ErrSnapshotUnavailable`/`RefreshResult` need the control-event
//!     channel, which lands with the loop (O7); O4 ports the assembly [`Orchestrator::build_snapshot`]
//!     the loop's handler calls, which every Go `buildSnapshot` test drives directly.
//!   * `ProjectStatus::warnings` (INF-277) are resolved off-loop by O6 (`warnings.go`); the field is
//!     carried for shape parity but stays empty until O6 wires `projectWarningsFor`.
//!   * `Snapshot::rate_limits` has no orchestrator source yet (Go copies its always-nil `rateLimits`
//!     here); it is left empty and the wire layer renders it as `[]` (P6 §2e populates real rows).

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use rhapsody_core::normalize_state;

use crate::orchestrator::{EventRecord, Orchestrator, RunningEntry, Totals};

// Per-project status enum values (INF-224, addendum #3). Mirror Go's `projectStatus*` constants.
/// Config-disabled (`enabled: false`).
const PROJECT_STATUS_PAUSED: &str = "paused";
/// >=1 in-flight agent on an active-state ticket.
const PROJECT_STATUS_RUNNING: &str = "running";
/// In-flight agent(s) only on review-state tickets (summon/handoff).
const PROJECT_STATUS_REVIEW: &str = "review";
/// No in-flight agents.
const PROJECT_STATUS_IDLE: &str = "idle";

/// The token fields in the snapshot JSON (§13.7.2). Mirrors Go `TokenCounts`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TokenCounts {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub total_tokens: i64,
}

/// One rate-limit row surfaced in the snapshot (§13.7.2). The orchestrator has no structured
/// rate-limit source yet, so this is currently always empty (P6 §2e populates real rows); the shape
/// mirrors Go httpapi's `rateLimitJSON` (`kind` renders as the wire field `type`). Mirrors the
/// element type of Go `Snapshot.RateLimits` (a nil `any` the wire layer renders as `[]`).
#[derive(Debug, Clone, PartialEq)]
pub struct RateLimit {
    pub kind: String,
    pub resets_at: String,
    pub used_percent: f64,
}

/// One active session in a snapshot (§13.3, §13.7.2). Mirrors Go `RunningRow`.
#[derive(Debug, Clone, PartialEq)]
pub struct RunningRow {
    pub issue_id: String,
    pub issue_identifier: String,
    pub title: String,
    pub state: String,
    pub session_id: String,
    pub turn_count: i64,
    pub last_event: String,
    pub last_message: String,
    pub started_at: DateTime<Utc>,
    pub last_event_at: DateTime<Utc>,
    pub workspace_path: String,
    pub tokens: TokenCounts,
    /// Reports that `tokens` leans on the live in-flight estimate (`cur_*`) rather than a committed
    /// result total — true mid-turn until a result commits, and for a run torn down without a clean
    /// result. Surfaced as an "est." badge in the dashboard (INF-208).
    pub usage_estimated: bool,
    pub recent_events: Vec<EventRecord>,
    pub transcript_path: String,
    /// The durable `runs`-table id for this in-flight run (0 when persistence is disabled / `StartRun`
    /// failed). Exposing it lets the dashboard open a live run by the SAME `run_id` key it uses for
    /// finished runs, so a single run-detail view survives the run's completion.
    pub run_id: i64,
    /// Mirrors the persisted `runs.attempt` for this run (`re.retry_attempt`).
    pub attempt: i64,
    /// Project routing (Phase 2; empty in single-project / legacy mode). `repo` is carried for Phase 3.
    pub project: String,
    pub repo: String,
}

/// One queued retry in a snapshot (§13.3, §13.7.2). Mirrors Go `RetryRow` (the snapshot row, distinct
/// from [`rhapsody_store::RetryRow`], the persisted retry-queue row).
#[derive(Debug, Clone, PartialEq)]
pub struct RetryRow {
    pub issue_id: String,
    pub issue_identifier: String,
    pub attempt: i64,
    pub due_at: DateTime<Utc>,
    pub error: String,
    pub workspace_path: String,
    pub transcript_path: String,
    /// Project routing (Phase 2; empty in single-project / legacy mode).
    pub project: String,
    pub repo: String,
}

/// One configured agent's live status for the Settings agents list (INF-224, addendum #3): an enum
/// (running/idle/review/paused) plus the live concurrent-run count. Mirrors Go `ProjectStatus`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProjectStatus {
    /// Stable per-project key (the project's first slug / group).
    pub slug: String,
    /// Display label (defaults to the first slug).
    pub name: String,
    /// One of the `PROJECT_STATUS_*` enum values.
    pub status: String,
    /// Live concurrent in-flight agent count for this project.
    pub running: i64,
    /// Per-project advisories (INF-277) — resolved off-loop by O6; empty until then.
    pub warnings: Vec<String>,
}

/// The synchronous runtime view (upstream §13.3). Mirrors Go `Snapshot`.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub generated_at: DateTime<Utc>,
    pub running: Vec<RunningRow>,
    pub retrying: Vec<RetryRow>,
    pub totals: Totals,
    pub rate_limits: Vec<RateLimit>,
    /// Per-project live status rollup (INF-224). One entry per configured project, in declaration
    /// order; empty when no resolved projects (test-injected effectives).
    pub projects: Vec<ProjectStatus>,
}

impl Orchestrator {
    /// Assembles a snapshot from current state. MUST run on the control task (it reads `running`,
    /// `retry_attempts`, `totals`, and `eff`). Runtime is reported as a live aggregate: ended-session
    /// seconds + active elapsed (upstream §13.5). Mirrors Go `buildSnapshot`.
    pub fn build_snapshot(&self) -> Snapshot {
        let now = (self.now)();
        let mut totals = self.totals.clone();

        // Token counts are reported LIVE: committed per-turn totals PLUS the in-flight current-turn
        // estimate (`cur_*`). The `cur_*` component resets to 0 when the turn result commits, so the
        // transition is smooth and never double-counts (upstream §13.5).
        let mut live_seconds = self.totals.seconds_running;
        let (mut live_cur_input, mut live_cur_output, mut live_cur_total) = (0i64, 0i64, 0i64);
        let mut running: Vec<RunningRow> = Vec::with_capacity(self.running.len());
        for re in self.running.values() {
            live_seconds += elapsed_seconds(now, re.started_at);
            live_cur_input += re.cur_input_tokens;
            live_cur_output += re.cur_output_tokens;
            live_cur_total += re.cur_total_tokens;
            running.push(RunningRow {
                issue_id: re.issue.id.clone(),
                issue_identifier: re.issue.identifier.clone(),
                title: re.issue.title.clone(),
                state: re.issue.state.clone(),
                session_id: re.session_id.clone(),
                turn_count: re.turn_count,
                last_event: re.last_event.clone(),
                last_message: re.last_message.clone(),
                started_at: re.started_at,
                last_event_at: re.last_event_at,
                workspace_path: self.workspace_path_for(&re.project_repo, &re.issue.identifier),
                tokens: TokenCounts {
                    input_tokens: re.input_tokens + re.cur_input_tokens,
                    output_tokens: re.output_tokens + re.cur_output_tokens,
                    total_tokens: re.total_tokens + re.cur_total_tokens,
                },
                // The displayed total includes the in-flight estimate whenever it is non-zero, so mark
                // it estimated then (matches `persist_end_run`'s floor flag).
                usage_estimated: re.cur_total_tokens > 0,
                recent_events: re.recent_events.clone(),
                // Prefer the CONCRETE per-run transcript file over the ticket's `latest.jsonl` alias.
                transcript_path: self.run_transcript_path(re),
                run_id: re.run_id,
                attempt: re.retry_attempt,
                project: re.project_slug.clone(),
                repo: re.project_repo.clone(),
            });
        }
        totals.seconds_running = live_seconds;
        totals.input_tokens += live_cur_input;
        totals.output_tokens += live_cur_output;
        totals.total_tokens += live_cur_total;

        let mut retrying: Vec<RetryRow> = Vec::with_capacity(self.retry_attempts.len());
        for r in self.retry_attempts.values() {
            retrying.push(RetryRow {
                issue_id: r.issue_id.clone(),
                issue_identifier: r.identifier.clone(),
                attempt: r.attempt,
                due_at: r.due_at,
                error: r.err.clone(),
                workspace_path: self.workspace_path_for(&r.project_repo, &r.identifier),
                transcript_path: self.transcript_path(&r.identifier),
                project: r.project_slug.clone(),
                repo: r.project_repo.clone(),
            });
        }

        // Deterministic ordering for stable output.
        running.sort_by(|a, b| a.issue_identifier.cmp(&b.issue_identifier));
        retrying.sort_by(|a, b| a.issue_identifier.cmp(&b.issue_identifier));

        Snapshot {
            generated_at: now,
            running,
            retrying,
            totals,
            // The orchestrator has no rate-limit source yet (P6 §2e); Go copies its always-nil
            // `rateLimits` here, which the wire layer renders as `[]`.
            rate_limits: Vec::new(),
            projects: self.project_statuses(),
        }
    }

    /// Rolls up per-project live status from the resolved project set + the running map (INF-224).
    /// One entry per project (deduped by the stable group key), in declaration order. Status priority:
    /// config-disabled => paused; else >=1 in-flight agent => running (or review when those agents sit
    /// only on review-state tickets); else idle. `running` is reported regardless of status. Mirrors
    /// Go `projectStatuses`.
    fn project_statuses(&self) -> Vec<ProjectStatus> {
        let Some(eff) = self.eff.as_ref() else {
            return Vec::new();
        };
        if eff.projects.is_empty() {
            return Vec::new();
        }
        struct Agg {
            name: String,
            disabled: bool,
            review: std::collections::HashSet<String>,
            running: i64,
            review_run: i64,
        }
        let mut by_group: HashMap<String, Agg> = HashMap::with_capacity(eff.projects.len());
        let mut order: Vec<String> = Vec::with_capacity(eff.projects.len());
        for p in &eff.projects {
            if by_group.contains_key(&p.group) {
                continue; // multi-slug project: one status per group
            }
            by_group.insert(
                p.group.clone(),
                Agg {
                    name: p.name.clone(),
                    disabled: p.disabled,
                    review: p.review_states.clone(),
                    running: 0,
                    review_run: 0,
                },
            );
            order.push(p.group.clone());
        }
        for re in self.running.values() {
            // Prefer the group key; fall back to the slug for the legacy/test path where group is unset.
            let key = if by_group.contains_key(&re.project_group) {
                &re.project_group
            } else if by_group.contains_key(&re.project_slug) {
                &re.project_slug
            } else {
                continue;
            };
            if let Some(g) = by_group.get_mut(key) {
                g.running += 1;
                if g.review.contains(&normalize_state(&re.issue.state)) {
                    g.review_run += 1;
                }
            }
        }
        let mut out = Vec::with_capacity(order.len());
        for group in &order {
            let Some(g) = by_group.get(group) else {
                continue;
            };
            let status = if g.disabled {
                PROJECT_STATUS_PAUSED
            } else if g.running == 0 {
                PROJECT_STATUS_IDLE
            } else if g.review_run > 0 && g.review_run == g.running {
                // review only when EVERY in-flight agent sits on a review-state ticket; any active or
                // unclassified-state run keeps the project "running".
                PROJECT_STATUS_REVIEW
            } else {
                PROJECT_STATUS_RUNNING
            };
            out.push(ProjectStatus {
                slug: group.clone(),
                name: g.name.clone(),
                status: status.to_string(),
                running: g.running,
                // Warnings (INF-277) are resolved off-loop by O6 (`warnings.go`); empty until then.
                warnings: Vec::new(),
            });
        }
        out
    }

    /// The workspace path for an issue under its owning project's repo, or `""` when no effective
    /// config is loaded (Go accesses `o.eff.workspace` directly, assuming the post-reload invariant;
    /// the Rust port degrades to `""` rather than panic on a `None` `eff`).
    fn workspace_path_for(&self, repo: &str, identifier: &str) -> String {
        self.eff
            .as_ref()
            .map(|eff| eff.workspace.path_for(repo, identifier))
            .unwrap_or_default()
    }

    /// The ticket's latest transcript path, or `""` if logging is off. Mirrors Go `transcriptPath`.
    fn transcript_path(&self, identifier: &str) -> String {
        match self.eff.as_ref() {
            Some(eff) if !eff.log_dir.is_empty() => eff.transcripts_latest(identifier),
            _ => String::new(),
        }
    }

    /// The running entry's CONCRETE per-run transcript file when the worker has reported it, falling
    /// back to the ticket's `latest.jsonl` alias otherwise. Mirrors Go `runTranscriptPath`.
    fn run_transcript_path(&self, re: &RunningEntry) -> String {
        if !re.transcript_path.is_empty() {
            re.transcript_path.clone()
        } else {
            self.transcript_path(&re.issue.identifier)
        }
    }
}

/// Live elapsed seconds from `started_at` to `now` (Go `now.Sub(re.startedAt).Seconds()`), at
/// millisecond resolution — plenty for the dashboard's runtime aggregate, and the fixtures normalize
/// `seconds_running` to `<NUM>` regardless.
fn elapsed_seconds(now: DateTime<Utc>, started_at: DateTime<Utc>) -> f64 {
    (now - started_at).num_milliseconds() as f64 / 1000.0
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use chrono::{TimeZone, Utc};
    use rhapsody_core::Issue;
    use rhapsody_tracker::fake::Fake;

    use super::*;
    use crate::obslog::Store as TranscriptStore;
    use crate::orchestrator::{EventRecord, RetryEntry, Totals};
    use crate::testsupport::{empty_effective, empty_resolved_project, issue, running_entry};

    fn fixed_now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 5, 28, 12, 0, 0)
            .single()
            .expect("valid fixed instant")
    }

    fn orch_for_snapshot() -> Orchestrator {
        let mut o = Orchestrator::new("WORKFLOW.md");
        o.eff = Some(empty_effective(Arc::new(Fake::new())));
        let now = fixed_now();
        o.now = Box::new(move || now);
        o
    }

    fn mk_retry(
        issue_id: &str,
        identifier: &str,
        attempt: i64,
        due_at: DateTime<Utc>,
        err: &str,
    ) -> RetryEntry {
        RetryEntry {
            issue_id: issue_id.to_string(),
            identifier: identifier.to_string(),
            attempt,
            due_at,
            err: err.to_string(),
            project_slug: String::new(),
            project_repo: String::new(),
            issue: Issue::default(),
            due_at_ms: 0,
            recovered: false,
        }
    }

    // Mirrors Go `TestBuildSnapshotIncludesTranscriptAndRecentEvents`.
    #[test]
    fn build_snapshot_includes_transcript_and_recent_events() {
        let mut o = orch_for_snapshot();
        {
            let eff = o.eff.as_mut().expect("eff");
            eff.log_dir = "/logs".to_string();
            eff.transcripts = Arc::new(TranscriptStore::new("/logs"));
        }
        let mut re = running_entry(issue("1", "MT-1", "In Progress"), "", "");
        re.started_at = (o.now)();
        re.recent_events = vec![EventRecord {
            at: (o.now)(),
            event: "turn_completed".to_string(),
            message: "did work".to_string(),
        }];
        o.running.insert("1".to_string(), re);

        let s = o.build_snapshot();
        assert_eq!(s.running.len(), 1);
        let r = &s.running[0];
        assert_eq!(r.transcript_path, "/logs/MT-1/latest.jsonl");
        assert_eq!(r.recent_events.len(), 1);
        assert_eq!(r.recent_events[0].event, "turn_completed");
    }

    // Mirrors Go `TestBuildSnapshotRunningAndRetrying`.
    #[test]
    fn build_snapshot_running_and_retrying() {
        let mut o = orch_for_snapshot();
        let start = (o.now)() - chrono::Duration::seconds(30);
        let mut re = running_entry(issue("1", "MT-1", "In Progress"), "", "");
        re.started_at = start;
        re.session_id = "thread-1-3".to_string();
        re.turn_count = 3;
        re.last_event = "turn_completed".to_string();
        re.last_message = "did work".to_string();
        re.last_event_at = (o.now)() - chrono::Duration::seconds(2);
        re.input_tokens = 100;
        re.output_tokens = 40;
        re.total_tokens = 140;
        o.running.insert("1".to_string(), re);
        o.retry_attempts.insert(
            "2".to_string(),
            mk_retry(
                "2",
                "MT-2",
                3,
                (o.now)() + chrono::Duration::minutes(1),
                "no available orchestrator slots",
            ),
        );
        o.totals = Totals {
            input_tokens: 500,
            output_tokens: 200,
            total_tokens: 700,
            seconds_running: 100.0,
        };

        let s = o.build_snapshot();

        assert_eq!(s.running.len(), 1);
        let r = &s.running[0];
        assert_eq!(r.issue_identifier, "MT-1");
        assert_eq!(r.session_id, "thread-1-3");
        assert_eq!(r.turn_count, 3);
        assert_eq!(r.tokens.total_tokens, 140);
        assert!(
            !r.workspace_path.is_empty(),
            "running row should carry a workspace path"
        );
        assert_eq!(s.retrying.len(), 1);
        assert_eq!(s.retrying[0].attempt, 3);
        assert!(!s.retrying[0].error.is_empty());
        // Live runtime = ended (100) + active elapsed (30s) = 130.
        assert_eq!(s.totals.seconds_running, 130.0);
        assert_eq!(s.totals.total_tokens, 700);
    }

    // Mirrors Go `TestBuildSnapshotShowsCommittedPlusLiveTokens`.
    #[test]
    fn build_snapshot_shows_committed_plus_live_tokens() {
        let mut o = orch_for_snapshot();
        let mut re1 = running_entry(issue("1", "MT-1", "In Progress"), "", "");
        re1.started_at = (o.now)();
        re1.input_tokens = 100;
        re1.output_tokens = 40;
        re1.total_tokens = 140;
        re1.cur_input_tokens = 7;
        re1.cur_output_tokens = 3;
        re1.cur_total_tokens = 10;
        o.running.insert("1".to_string(), re1);
        let mut re2 = running_entry(issue("2", "MT-2", "In Progress"), "", "");
        re2.started_at = (o.now)();
        re2.cur_input_tokens = 5;
        re2.cur_output_tokens = 1;
        re2.cur_total_tokens = 6;
        o.running.insert("2".to_string(), re2);
        o.totals = Totals {
            input_tokens: 100,
            output_tokens: 40,
            total_tokens: 140,
            seconds_running: 0.0,
        };

        let s = o.build_snapshot();
        assert_eq!(s.running.len(), 2);
        // Rows are sorted by identifier: MT-1, MT-2.
        let r1 = &s.running[0];
        assert_eq!(
            (
                r1.tokens.input_tokens,
                r1.tokens.output_tokens,
                r1.tokens.total_tokens
            ),
            (107, 43, 150)
        );
        let r2 = &s.running[1];
        assert_eq!(
            (
                r2.tokens.input_tokens,
                r2.tokens.output_tokens,
                r2.tokens.total_tokens
            ),
            (5, 1, 6)
        );
        // Live totals = committed (140 total) + sum of cur across running (10 + 6).
        assert_eq!(
            (
                s.totals.input_tokens,
                s.totals.output_tokens,
                s.totals.total_tokens
            ),
            (112, 44, 156)
        );
    }

    // Mirrors Go `TestSnapshotCarriesProjectAndRepo`.
    #[test]
    fn snapshot_carries_project_and_repo() {
        let mut o = orch_for_snapshot();
        let mut re = running_entry(issue("a1", "A-1", "In Progress"), "alpha", "alpha");
        re.started_at = (o.now)();
        re.project_repo = "git@github.com:o/r.git".to_string();
        o.running.insert("a1".to_string(), re);
        let mut retry = mk_retry("b1", "B-1", 1, (o.now)(), "");
        retry.project_slug = "beta".to_string();
        retry.project_repo = "git@github.com:o/r2.git".to_string();
        o.retry_attempts.insert("b1".to_string(), retry);

        let s = o.build_snapshot();
        assert_eq!(s.running.len(), 1);
        assert_eq!(s.running[0].project, "alpha");
        assert_eq!(s.running[0].repo, "git@github.com:o/r.git");
        assert_eq!(s.retrying.len(), 1);
        assert_eq!(s.retrying[0].project, "beta");
        assert_eq!(s.retrying[0].repo, "git@github.com:o/r2.git");
    }

    // Mirrors Go `TestSnapshotPerProjectStatus`.
    #[test]
    fn snapshot_per_project_status() {
        let mut o = orch_for_snapshot();
        let active = crate::testsupport::set_of(&[&normalize_state("In Progress")]);
        let review = crate::testsupport::set_of(&[&normalize_state("In Review")]);
        let tr: Arc<dyn rhapsody_tracker::Tracker> = Arc::new(Fake::new());
        let mut projects = Vec::new();
        for (slug, disabled, has_review) in [
            ("alpha", false, false),
            ("beta", false, true),
            ("gamma", true, false),
            ("delta", false, false),
            ("epsilon", false, true),
        ] {
            let mut p = empty_resolved_project(slug, Arc::clone(&tr));
            p.name = slug[..1].to_uppercase() + &slug[1..]; // "Alpha", "Beta", ...
            p.disabled = disabled;
            p.active_states = active.clone();
            if has_review {
                p.review_states = review.clone();
            }
            projects.push(p);
        }
        o.eff.as_mut().expect("eff").projects = projects;

        // alpha: one in-flight agent on an active-state ticket -> running, count 1.
        o.running.insert(
            "a1".to_string(),
            running_entry(issue("a1", "A-1", "In Progress"), "alpha", "alpha"),
        );
        // beta: one in-flight agent on a review-state ticket, none active -> review, count 1.
        o.running.insert(
            "b1".to_string(),
            running_entry(issue("b1", "B-1", "In Review"), "beta", "beta"),
        );
        // epsilon: one review-state run + one unclassified run -> stays running, count 2.
        o.running.insert(
            "e1".to_string(),
            running_entry(issue("e1", "E-1", "In Review"), "epsilon", "epsilon"),
        );
        o.running.insert(
            "e2".to_string(),
            running_entry(issue("e2", "E-2", "Backlog"), "epsilon", "epsilon"),
        );
        // gamma: config-disabled -> paused (count 0). delta: no runs -> idle (count 0).

        let s = o.build_snapshot();
        assert_eq!(
            s.projects.len(),
            5,
            "want 5 project statuses: {:?}",
            s.projects
        );
        let got: HashMap<String, ProjectStatus> = s
            .projects
            .iter()
            .map(|p| (p.name.clone(), p.clone()))
            .collect();
        for (name, want_status, want_running) in [
            ("Alpha", PROJECT_STATUS_RUNNING, 1),
            ("Beta", PROJECT_STATUS_REVIEW, 1),
            ("Gamma", PROJECT_STATUS_PAUSED, 0),
            ("Delta", PROJECT_STATUS_IDLE, 0),
            ("Epsilon", PROJECT_STATUS_RUNNING, 2),
        ] {
            let p = got
                .get(name)
                .unwrap_or_else(|| panic!("missing project {name}"));
            assert_eq!(p.status, want_status, "{name} status");
            assert_eq!(p.running, want_running, "{name} running count");
        }
    }
}
