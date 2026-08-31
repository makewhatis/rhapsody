//! select — parity port of Go `internal/orchestrator/select.go`.
//!
//! The per-tick selection pass: one greedy walk over the sorted candidates that admits intrinsically
//! eligible issues while respecting the shared global, per-project, and per-state slot budgets
//! (upstream §8.1–§8.3). It returns only ACTIVE-state picks; review-state reopens are collected
//! separately (a fresh summons that the loop, O7, must promote via `MoveIssueState` before
//! dispatching), sharing the SAME slot counters so a tick cannot over-admit across both paths.
//!
//! Deviations from the Go source, all behavior-preserving:
//!   * Go's `taggedIssue.proj *resolvedProject` (a pointer into `o.eff.projects`) becomes an INDEX
//!     [`Option<usize>`] into the effective's `projects`, so a pick never borrows the orchestrator —
//!     the loop (O7) can mutate scheduling state while holding picks. Both carry the same validity
//!     window (select + dispatch happen within one tick, before any reload swaps the effective).
//!   * `runningIDSet`/`runningStateCounts` (Go `map[string]bool`/`map[string]int`) are the crate's
//!     [`HashSet`](std::collections::HashSet)/[`HashMap`] helpers on [`Orchestrator`].
//!   * `o.eff` is `Option<Effective>`; the passes guard `None` (returning no picks — there is
//!     nothing to schedule without a loaded config) instead of Go's nil-deref-relies-on-invariant.

use std::collections::HashMap;

use rhapsody_core::{Issue, normalize_state};

use crate::concurrency::{global_slots, state_limit};
use crate::dispatch::{EligibilityGate, dispatch_cmp, eligibility};
use crate::orchestrator::Orchestrator;

/// Pairs a candidate with the INDEX of the project it was polled from (into the effective's
/// `projects`), so routing, slot accounting, and eligibility use the issue's owning project's
/// effective config. `proj == None` is the legacy single-tracker path. Mirrors Go `taggedIssue`
/// (whose `proj` is a `*resolvedProject`; see the module docs for the index rationale).
///
/// `pub` because it appears in the signatures of the `pub` selection/claim entry points the control
/// loop (O7) will drive.
pub struct TaggedIssue {
    pub iss: Issue,
    pub proj: Option<usize>,
}

/// Orders tagged issues by the global dispatch key so `select_dispatch_multi` admits in the global
/// order across all projects. Mirrors Go `sortTaggedStable` (defined in `dispatch.go`; placed here
/// next to [`TaggedIssue`], sharing [`dispatch_cmp`]'s ordering — the induced order is identical).
pub(crate) fn sort_tagged_stable(tagged: &mut [TaggedIssue]) {
    tagged.sort_by(|a, b| dispatch_cmp(&a.iss, &b.iss));
}

impl Orchestrator {
    /// Returns the issues to dispatch this tick: sorted, intrinsically eligible, and within global +
    /// per-state concurrency (upstream §8.1–§8.3). ACTIVE-state picks only. Mirrors Go
    /// `selectDispatch`.
    pub fn select_dispatch(&self, issues: Vec<Issue>) -> Vec<Issue> {
        self.select_dispatch_with_reopens(issues).0
    }

    /// One greedy pass over the sorted candidates, sharing the global/per-state slot counters between
    /// the active-dispatch picks and the review-reopen picks (so a tick cannot over-admit across both
    /// paths). Returns `(active, reopen)`: `active` holds issues to dispatch as-is; `reopen` holds
    /// review-state issues with a fresh summons the loop must promote before dispatching. With no
    /// review states configured, `reopen` is always empty and the active path is byte-identical.
    /// Mirrors Go `selectDispatchWithReopens`.
    pub fn select_dispatch_with_reopens(&self, mut issues: Vec<Issue>) -> (Vec<Issue>, Vec<Issue>) {
        let Some(eff) = self.eff.as_ref() else {
            return (Vec::new(), Vec::new());
        };
        crate::dispatch::sort_for_dispatch(&mut issues);

        let mut running = self.running_id_set();
        let mut state_counts = self.running_state_counts();
        let mut global_remaining = global_slots(eff.max_concurrent, self.running.len() as i64);
        // Boot-recovery guard: never dispatch an issue a pending recovered retry already owns by
        // IDENTIFIER (invisible to the opaque-ID-keyed `claimed`), or the recovered on-retry would
        // later release+delete the live run's claim row.
        let recovered_claims = self.recovered_claim_identifiers();

        let mut active = Vec::new();
        let mut reopen = Vec::new();
        // Set by the Teams assignment gate below; drained into ONE kick after the pass (§A.3.2).
        let mut held_for_triage = false;
        for iss in issues {
            if global_remaining <= 0 {
                break;
            }
            if recovered_claims.contains(&iss.identifier) {
                continue;
            }
            let st = normalize_state(&iss.state);
            // Review-state branch: a non-active state in the configured review set. NEVER eligible
            // (eligibility rejects non-active states), so handled only here — gated on a fresh
            // summons and counted against the promote state's caps. The label gate is intentionally
            // NOT applied here (an @symphony summons is an explicit human override of the proactive
            // label filter, scoped to this review-reopen branch).
            if eff.review_states.contains(&st) && !eff.active_states.contains(&st) {
                if !self.review_reopen_eligible(&iss, &running) {
                    continue;
                }
                let pst = normalize_state(&eff.review_promote_state);
                if count(&state_counts, &pst)
                    >= state_limit(
                        &eff.review_promote_state,
                        &eff.per_state_limits,
                        eff.max_concurrent,
                    )
                {
                    continue;
                }
                running.insert(iss.id.clone());
                *state_counts.entry(pst).or_insert(0) += 1;
                global_remaining -= 1;
                reopen.push(iss);
                continue;
            }
            let gate = EligibilityGate {
                active: &eff.active_states,
                terminal: &eff.terminal_states,
                required_labels: &eff.labels,
                mode: &eff.dependency_mode,
                review: &eff.review_states,
                canceled: &eff.canceled_states,
            };
            let elig = eligibility(&iss, &running, &self.claimed, &gate);
            if !elig.ok {
                // Surface the otherwise-silent blocker drop (INF-249); no-op for any other reason.
                self.log_blocked_skip(&iss, &elig.blocked_by);
                continue;
            }
            // Work already materialized as a linked PR with no newer summons → don't fresh-dispatch
            // on a state flap. Info-level so a suppressed issue isn't an unexplained live-list hang.
            if self.pr_suppressed(&iss) {
                tracing::info!(
                    issue_identifier = %iss.identifier,
                    "skipping dispatch: issue has a linked PR and no newer summons"
                );
                continue;
            }
            // The Teams work-assignment gate (STUDIO-669; design record
            // `~/.rhapsody/docs/STUDIO-668-multi-team.md` §A.3.1). A Teams-eligible candidate that
            // nothing has assigned yet waits for the manager instead of dispatching identity-less:
            // "if you have teams enabled, you want the work to go to the team". It is a SKIP, the
            // same shape as every other skip above — no slot is reserved, no counter moves, nothing
            // blocks, and the loop's cadence is untouched. Debug-level because a held ticket is a
            // normal one-or-two-tick state on a healthy daemon, not a fault.
            if self.teams_awaiting_assignment(&iss) {
                tracing::debug!(
                    issue_identifier = %iss.identifier,
                    "skipping dispatch: awaiting team assignment"
                );
                held_for_triage = true;
                continue;
            }
            if count(&state_counts, &st)
                >= state_limit(&iss.state, &eff.per_state_limits, eff.max_concurrent)
            {
                continue;
            }
            // Reserve so a single tick cannot over-dispatch.
            running.insert(iss.id.clone());
            *state_counts.entry(st).or_insert(0) += 1;
            global_remaining -= 1;
            active.push(iss);
        }
        self.kick_triage(held_for_triage);
        (active, reopen)
    }

    /// Counts running issues currently owned by the given project GROUP. The per-project cap is
    /// enforced across the whole group (all slugs fanned out from the same project), not per slug, so
    /// a multi-slug project admits at most its cap of concurrent agents in total. `group == slug` for
    /// single-slug / legacy modes. Mirrors Go `runningInProjectGroup`.
    pub(crate) fn running_in_project_group(&self, group: &str) -> i64 {
        self.running
            .values()
            .filter(|re| re.project_group == group)
            .count() as i64
    }

    /// Sorts tagged candidates by the global dispatch order and greedily admits eligible issues while
    /// (a) a GLOBAL slot remains, (b) the issue's PROJECT cap is free, and (c) the per-STATE cap is
    /// free — accounting for issues admitted earlier in this pass. Per-state accounting is GLOBAL
    /// (across all projects); the per-project ceiling is what scopes a project's footprint. Mirrors
    /// Go `selectDispatchMulti`.
    pub fn select_dispatch_multi(&self, tagged: Vec<TaggedIssue>) -> Vec<TaggedIssue> {
        self.select_dispatch_multi_with_reopens(tagged).0
    }

    /// [`Orchestrator::select_dispatch_multi`] plus the review-reopen branch, sharing the
    /// global/per-project/per-state slot counters in ONE greedy pass. `picked` holds active-dispatch
    /// issues; `reopen` holds review-state issues (tagged with their project) the loop must promote
    /// before dispatching. Mirrors Go `selectDispatchMultiWithReopens`.
    pub fn select_dispatch_multi_with_reopens(
        &self,
        mut tagged: Vec<TaggedIssue>,
    ) -> (Vec<TaggedIssue>, Vec<TaggedIssue>) {
        let Some(eff) = self.eff.as_ref() else {
            return (Vec::new(), Vec::new());
        };
        sort_tagged_stable(&mut tagged);

        let mut running = self.running_id_set();
        let mut global_remaining = global_slots(eff.max_concurrent, self.running.len() as i64);
        let mut per_project: HashMap<String, i64> = HashMap::new(); // group -> remaining slots this pass
        let mut state_counts = self.running_state_counts(); // normState -> running-in-state across ALL projects
        let recovered_claims = self.recovered_claim_identifiers();

        let mut picked = Vec::new();
        let mut reopen = Vec::new();
        let mut held_for_triage = false;
        for ti in tagged {
            if global_remaining <= 0 {
                break;
            }
            // The multi path always tags with a project (`pollAllProjects`, O7). A nil-proj entry
            // would panic in Go; the Rust port skips it defensively (it never occurs in practice).
            let Some(p) = ti.proj.and_then(|i| eff.projects.get(i)) else {
                continue;
            };
            if recovered_claims.contains(&ti.iss.identifier) {
                continue;
            }
            let st = normalize_state(&ti.iss.state);
            // Review-state branch (per the issue's owning project's review set); the label gate is
            // intentionally NOT applied (an @symphony summons is a manual override of the filter).
            if p.review_states.contains(&st) && !p.active_states.contains(&st) {
                if !self.review_reopen_eligible(&ti.iss, &running) {
                    continue;
                }
                if !self.ensure_project_budget(&mut per_project, &p.group, p.max_concurrent) {
                    continue;
                }
                let pst = normalize_state(&eff.review_promote_state);
                if count(&state_counts, &pst)
                    >= state_limit(
                        &eff.review_promote_state,
                        &p.per_state_limits,
                        eff.max_concurrent,
                    )
                {
                    continue;
                }
                running.insert(ti.iss.id.clone());
                global_remaining -= 1;
                *per_project.entry(p.group.clone()).or_insert(0) -= 1;
                *state_counts.entry(pst).or_insert(0) += 1;
                reopen.push(ti);
                continue;
            }
            let gate = EligibilityGate {
                active: &p.active_states,
                terminal: &p.terminal_states,
                required_labels: &p.labels,
                mode: &p.dependency_mode,
                review: &p.review_states,
                canceled: &p.canceled_states,
            };
            let elig = eligibility(&ti.iss, &running, &self.claimed, &gate);
            if !elig.ok {
                self.log_blocked_skip(&ti.iss, &elig.blocked_by);
                continue;
            }
            if self.pr_suppressed(&ti.iss) {
                tracing::info!(
                    issue_identifier = %ti.iss.identifier,
                    "skipping dispatch: issue has a linked PR and no newer summons"
                );
                continue;
            }
            // The Teams work-assignment gate (STUDIO-669; design record
            // `~/.rhapsody/docs/STUDIO-668-multi-team.md` §A.3.1). A Teams-eligible candidate that
            // nothing has assigned yet waits for the manager instead of dispatching identity-less:
            // "if you have teams enabled, you want the work to go to the team". It is a SKIP, the
            // same shape as every other skip above — no slot is reserved, no counter moves, nothing
            // blocks, and the loop's cadence is untouched. Debug-level because a held ticket is a
            // normal one-or-two-tick state on a healthy daemon, not a fault.
            if self.teams_awaiting_assignment(&ti.iss) {
                tracing::debug!(
                    issue_identifier = %ti.iss.identifier,
                    "skipping dispatch: awaiting team assignment"
                );
                held_for_triage = true;
                continue;
            }
            if !self.ensure_project_budget(&mut per_project, &p.group, p.max_concurrent) {
                continue;
            }
            // The per-state cap is a shared GLOBAL ceiling (stateCounts across ALL projects), so its
            // fallback must be the GLOBAL cap — not the per-project cap — matching single-project.
            if count(&state_counts, &st)
                >= state_limit(&ti.iss.state, &p.per_state_limits, eff.max_concurrent)
            {
                continue;
            }
            // Reserve all three counters per admit.
            let group = p.group.clone();
            running.insert(ti.iss.id.clone());
            global_remaining -= 1;
            *per_project.entry(group).or_insert(0) -= 1;
            *state_counts.entry(st).or_insert(0) += 1;
            picked.push(ti);
        }
        self.kick_triage(held_for_triage);
        (picked, reopen)
    }

    /// The arrival kick (STUDIO-669; design record §A.3.2): when this pass held one or more
    /// candidates for want of an assignment, ask the triage task to run a cycle **now** rather than
    /// wait out [`TRIAGE_INTERVAL`](crate::triage::TRIAGE_INTERVAL).
    ///
    /// Three properties, all deliberate:
    ///
    /// * **Once per pass, not once per ticket.** Ten held tickets want one cycle; a cycle already
    ///   sweeps up to `MAX_PER_CYCLE` of them.
    /// * **No per-ticket work is spawned.** This is a `Notify` permit, so the control task pays a
    ///   single atomic store and returns — the whole point of triage being off-loop stands.
    /// * **Steady state is untouched.** Nothing held ⇒ nothing sent ⇒ the triage task keeps its
    ///   own cadence exactly as before.
    fn kick_triage(&self, held: bool) {
        if !held {
            return;
        }
        if let Some(handle) = self.teams_triage.as_ref() {
            handle.kick();
        }
    }

    /// Returns whether the project group has a free slot this pass, lazily computing its remaining
    /// budget on first touch (Go's inline `reserveProject` closure). The caller decrements on admit.
    fn ensure_project_budget(
        &self,
        per_project: &mut HashMap<String, i64>,
        group: &str,
        max_concurrent: i64,
    ) -> bool {
        if !per_project.contains_key(group) {
            let free = (max_concurrent - self.running_in_project_group(group)).max(0);
            per_project.insert(group.to_string(), free);
        }
        count(per_project, group) > 0
    }
}

/// A running-count lookup with the Go `map[key]` zero-value default (0 for a missing key).
fn count(counts: &HashMap<String, i64>, key: &str) -> i64 {
    counts.get(key).copied().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::sync::Arc;

    use chrono::{Duration as ChronoDuration, TimeZone, Utc};
    use rhapsody_core::Issue;
    use rhapsody_tracker::fake::Fake;

    use super::*;
    use crate::orchestrator::{Orchestrator, RunningEntry};
    use crate::testsupport::*;

    const SKIP_BLOCKED: &str = "skipping dispatch: blocked by non-terminal blocker";

    /// A single-project select orchestrator (active `{todo, in progress}`, terminal `{done}`).
    /// Mirrors Go `orchForSelect` / `orchForSelectWithLog` (logging is captured via
    /// [`capture_events`] rather than a per-orchestrator buffer).
    fn orch_for_select(
        max: i64,
        per_state: HashMap<String, i64>,
        running: Option<HashMap<String, RunningEntry>>,
    ) -> Orchestrator {
        let mut o = Orchestrator::new("WORKFLOW.md");
        if let Some(r) = running {
            o.running = r;
        }
        let mut eff = empty_effective(Arc::new(Fake::new()));
        eff.active_states = set_of(&["todo", "in progress"]);
        eff.terminal_states = set_of(&["done"]);
        eff.per_state_limits = per_state;
        eff.max_concurrent = max;
        o.eff = Some(eff);
        o
    }

    /// A multi-project orchestrator with an injected resolved-project set. Mirrors Go `orchForMulti`.
    fn orch_for_multi(
        global_max: i64,
        projects: Vec<crate::effective::ResolvedProject>,
        running: Option<HashMap<String, RunningEntry>>,
    ) -> Orchestrator {
        let mut o = Orchestrator::new("WORKFLOW.md");
        if let Some(r) = running {
            o.running = r;
        }
        let mut eff = empty_effective(Arc::new(Fake::new()));
        eff.max_concurrent = global_max;
        eff.projects = projects;
        o.eff = Some(eff);
        o
    }

    /// A single-slug resolved project (group == slug) with active `{todo, in progress}`, terminal
    /// `{done}`. Mirrors Go `proj`.
    fn proj(
        slug: &str,
        cap: i64,
        per_state: HashMap<String, i64>,
    ) -> crate::effective::ResolvedProject {
        let mut p = empty_resolved_project(slug, Arc::new(Fake::new()));
        p.active_states = set_of(&["todo", "in progress"]);
        p.terminal_states = set_of(&["done"]);
        p.per_state_limits = per_state;
        p.max_concurrent = cap;
        p
    }

    /// Tags issues with the project at `idx` in the effective's `projects`. Mirrors Go `tagFor`
    /// (which passes a `*resolvedProject`; here the index into `eff.projects`).
    fn tag_for(idx: usize, issues: Vec<Issue>) -> Vec<TaggedIssue> {
        issues
            .into_iter()
            .map(|iss| TaggedIssue {
                iss,
                proj: Some(idx),
            })
            .collect()
    }

    /// The admitted issue ids as a set. Mirrors Go `pickedIDs`.
    fn picked_ids(picks: &[TaggedIssue]) -> HashSet<String> {
        picks.iter().map(|t| t.iss.id.clone()).collect()
    }

    fn running_state(id: &str, state: &str) -> Issue {
        Issue {
            id: id.to_string(),
            state: state.to_string(),
            ..Default::default()
        }
    }

    // --- select_test.go (single-project) ------------------------------------------------------

    // Mirrors Go `TestSelectDispatchRespectsGlobalSlots`.
    #[test]
    fn select_dispatch_respects_global_slots() {
        let o = orch_for_select(2, HashMap::new(), None);
        let input = vec![
            issue("1", "A-1", "Todo"),
            issue("2", "A-2", "Todo"),
            issue("3", "A-3", "Todo"),
        ];
        assert_eq!(o.select_dispatch(input).len(), 2, "global slots");
    }

    // Mirrors Go `TestSelectDispatchRespectsPerStateSlots`.
    #[test]
    fn select_dispatch_respects_per_state_slots() {
        let o = orch_for_select(10, HashMap::from([("in progress".to_string(), 1i64)]), None);
        let input = vec![
            issue("1", "A-1", "In Progress"),
            issue("2", "A-2", "In Progress"),
            issue("3", "A-3", "Todo"),
        ];
        let got = o.select_dispatch(input);
        assert_eq!(got.len(), 2, "1 In Progress (cap) + the Todo");
        let ids: HashSet<String> = got.iter().map(|i| i.id.clone()).collect();
        assert!(ids.contains("3"), "Todo issue should be selected");
        assert_ne!(
            ids.contains("1"),
            ids.contains("2"),
            "exactly one In Progress"
        );
    }

    // Mirrors Go `TestSelectDispatchSkipsRunningAndIneligible`.
    #[test]
    fn select_dispatch_skips_running_and_ineligible() {
        let running = HashMap::from([(
            "1".to_string(),
            running_entry(running_state("1", "In Progress"), "", ""),
        )]);
        let o = orch_for_select(10, HashMap::new(), Some(running));
        let input = vec![
            issue("1", "A-1", "Todo"),    // already running
            issue("2", "A-2", "Backlog"), // not active
            {
                let mut i = issue("3", "A-3", "Todo");
                i.blocked_by = Some(vec![blocker(None, Some("In Progress"))]); // blocked
                i
            },
            issue("4", "A-4", "Todo"), // eligible
        ];
        let got = o.select_dispatch(input);
        assert_eq!(ids(&got), vec!["A-4"], "expected only A-4");
    }

    // Mirrors Go `TestSelectDispatchLogsNonTerminalBlocker`.
    #[test]
    fn select_dispatch_logs_non_terminal_blocker() {
        let o = orch_for_select(10, HashMap::new(), None);
        let input = vec![
            {
                let mut i = issue("1", "A-1", "Todo");
                i.blocked_by = Some(vec![blocker(Some("A-9"), Some("In Review"))]);
                i
            },
            {
                let mut i = issue("2", "A-2", "Todo");
                i.blocked_by = Some(vec![blocker(Some("A-8"), Some("Done"))]);
                i
            },
        ];
        let (got, events) = capture_events(|| o.select_dispatch(input));
        assert_eq!(
            ids(&got),
            vec!["A-2"],
            "only the terminal-blocked issue dispatches"
        );

        let a1 = events
            .iter()
            .find(|e| {
                e.message == SKIP_BLOCKED
                    && e.fields.get("issue_identifier").map(String::as_str) == Some("A-1")
            })
            .expect("blocked-skip log for A-1");
        assert_eq!(a1.fields.get("blocker").map(String::as_str), Some("A-9"));
        assert_eq!(
            a1.fields.get("blocker_state").map(String::as_str),
            Some("In Review")
        );
        assert!(
            !events.iter().any(|e| e.message == SKIP_BLOCKED
                && e.fields.get("issue_identifier").map(String::as_str) == Some("A-2")),
            "A-2 (terminal blocker) must not be logged as blocked"
        );
    }

    // Mirrors Go `TestSelectDispatchLogsUnknownBlockerState`.
    #[test]
    fn select_dispatch_logs_unknown_blocker_state() {
        let o = orch_for_select(10, HashMap::new(), None);
        let input = vec![{
            let mut i = issue("1", "A-1", "Todo");
            i.blocked_by = Some(vec![blocker(Some("A-9"), None)]);
            i
        }];
        let (_got, events) = capture_events(|| o.select_dispatch(input));
        let ev = events
            .iter()
            .find(|e| e.message == SKIP_BLOCKED)
            .expect("skip log");
        assert_eq!(
            ev.fields.get("blocker_state").map(String::as_str),
            Some("unknown")
        );
    }

    // Mirrors Go `TestSelectDispatchLogsEveryNonTerminalBlocker`.
    #[test]
    fn select_dispatch_logs_every_non_terminal_blocker() {
        let o = orch_for_select(10, HashMap::new(), None);
        let input = vec![{
            let mut i = issue("1", "A-1", "Todo");
            i.blocked_by = Some(vec![
                blocker(Some("A-9"), Some("In Review")),
                blocker(Some("A-8"), Some("Done")), // terminal → no line
                blocker(Some("A-7"), Some("Backlog")),
            ]);
            i
        }];
        let (_got, events) = capture_events(|| o.select_dispatch(input));
        assert_eq!(count_messages(&events, SKIP_BLOCKED), 2, "A-9, A-7");
        let blockers: HashSet<String> = events
            .iter()
            .filter(|e| e.message == SKIP_BLOCKED)
            .filter_map(|e| e.fields.get("blocker").cloned())
            .collect();
        assert!(blockers.contains("A-9") && blockers.contains("A-7"));
        assert!(
            !blockers.contains("A-8"),
            "terminal blocker A-8 must not be logged"
        );
    }

    // Mirrors Go `TestSelectDispatchMultiLogsNonTerminalBlocker`.
    #[test]
    fn select_dispatch_multi_logs_non_terminal_blocker() {
        let o = orch_for_multi(10, vec![proj("a", 10, HashMap::new())], None);
        let mut tagged = tag_for(
            0,
            vec![{
                let mut i = issue("1", "A-1", "Todo");
                i.blocked_by = Some(vec![blocker(Some("A-9"), Some("In Review"))]);
                i
            }],
        );
        tagged.extend(tag_for(0, vec![issue("2", "A-2", "Todo")]));
        let (got, events) = capture_events(|| o.select_dispatch_multi(tagged));
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].iss.id, "2");
        let a1 = events
            .iter()
            .find(|e| {
                e.message == SKIP_BLOCKED
                    && e.fields.get("issue_identifier").map(String::as_str) == Some("A-1")
            })
            .expect("multi-project blocked-skip log");
        assert_eq!(a1.fields.get("blocker").map(String::as_str), Some("A-9"));
        assert_eq!(
            a1.fields.get("blocker_state").map(String::as_str),
            Some("In Review")
        );
    }

    // Mirrors Go `TestSelectDispatchSkipsPRSuppressed`.
    #[test]
    fn select_dispatch_skips_pr_suppressed() {
        let o = orch_for_select(10, HashMap::new(), None);
        let pr = Utc.with_ymd_and_hms(2026, 6, 3, 12, 0, 0).unwrap();
        let input = vec![
            {
                // Linked PR, no newer summons → suppressed (the flap case).
                let mut i = issue("1", "A-1", "In Progress");
                i.linked_pr = true;
                i.latest_pr_activity_at = Some(pr);
                i
            },
            {
                // Linked PR + a summons newer than the PR activity → reopened → dispatched.
                let mut i = issue("2", "A-2", "In Progress");
                i.linked_pr = true;
                i.latest_pr_activity_at = Some(pr);
                i.latest_summon_at = Some(pr + ChronoDuration::hours(1));
                i
            },
            issue("3", "A-3", "Todo"), // no PR → dispatched normally
        ];
        let got = o.select_dispatch(input);
        let ids: HashSet<String> = got.iter().map(|i| i.id.clone()).collect();
        assert!(
            !ids.contains("1"),
            "A-1 (linked PR, no newer summons) must be suppressed"
        );
        assert!(
            ids.contains("2") && ids.contains("3"),
            "A-2 (reopened) + A-3 (no PR) dispatched"
        );
    }

    // --- select_multi_test.go -----------------------------------------------------------------

    // Mirrors Go `TestSelectDispatchMultiGlobalCap`.
    #[test]
    fn select_dispatch_multi_global_cap() {
        let o = orch_for_multi(
            2,
            vec![proj("a", 10, HashMap::new()), proj("b", 10, HashMap::new())],
            None,
        );
        let mut input = tag_for(
            0,
            vec![issue("1", "A-1", "Todo"), issue("2", "A-2", "Todo")],
        );
        input.extend(tag_for(1, vec![issue("3", "B-1", "Todo")]));
        assert_eq!(o.select_dispatch_multi(input).len(), 2, "global cap 2");
    }

    // Mirrors Go `TestSelectDispatchMultiPerProjectCap`.
    #[test]
    fn select_dispatch_multi_per_project_cap() {
        let o = orch_for_multi(
            10,
            vec![proj("a", 1, HashMap::new()), proj("b", 10, HashMap::new())],
            None,
        );
        let mut input = tag_for(
            0,
            vec![
                issue("1", "A-1", "Todo"),
                issue("2", "A-2", "Todo"),
                issue("3", "A-3", "Todo"),
            ],
        );
        input.extend(tag_for(
            1,
            vec![issue("4", "B-1", "Todo"), issue("5", "B-2", "Todo")],
        ));
        let ids = picked_ids(&o.select_dispatch_multi(input));
        let a_count = ["1", "2", "3"]
            .iter()
            .filter(|id| ids.contains(**id))
            .count();
        assert_eq!(a_count, 1, "project A cap=1 admits exactly 1");
        assert!(
            ids.contains("4") && ids.contains("5"),
            "both B issues admitted"
        );
    }

    // Mirrors Go `TestSelectDispatchMultiCapIsPerGroupNotPerSlug`.
    #[test]
    fn select_dispatch_multi_cap_is_per_group_not_per_slug() {
        let mk = |slug: &str| {
            let mut p = empty_resolved_project(slug, Arc::new(Fake::new()));
            p.group = "grp".to_string(); // both slugs belong to the same project group
            p.active_states = set_of(&["todo", "in progress"]);
            p.terminal_states = set_of(&["done"]);
            p.max_concurrent = 2;
            p
        };
        let o = orch_for_multi(10, vec![mk("a1"), mk("a2")], None);
        let mut input = tag_for(
            0,
            vec![issue("1", "A1-1", "Todo"), issue("2", "A1-2", "Todo")],
        );
        input.extend(tag_for(
            1,
            vec![issue("3", "A2-1", "Todo"), issue("4", "A2-2", "Todo")],
        ));
        assert_eq!(
            o.select_dispatch_multi(input).len(),
            2,
            "project cap=2 bounds the whole group (both slugs) to 2 total"
        );
    }

    // Mirrors Go `TestSelectDispatchMultiPerStateCapIsGlobal`.
    #[test]
    fn select_dispatch_multi_per_state_cap_is_global() {
        let cap = || HashMap::from([("in progress".to_string(), 1i64)]);
        let o = orch_for_multi(10, vec![proj("a", 10, cap()), proj("b", 10, cap())], None);
        let mut input = tag_for(
            0,
            vec![
                issue("1", "A-1", "In Progress"),
                issue("2", "A-2", "In Progress"),
            ],
        );
        input.extend(tag_for(
            1,
            vec![
                issue("3", "B-1", "In Progress"),
                issue("4", "B-2", "In Progress"),
            ],
        ));
        let ids = picked_ids(&o.select_dispatch_multi(input));
        let ip_total = ["1", "2", "3", "4"]
            .iter()
            .filter(|id| ids.contains(**id))
            .count();
        assert_eq!(
            ip_total, 1,
            "per-state cap is GLOBAL: want 1 In Progress total"
        );
    }

    // Mirrors Go `TestSelectDispatchMultiPerStateCapGlobalWithPreexisting`.
    #[test]
    fn select_dispatch_multi_per_state_cap_global_with_preexisting() {
        let cap = || HashMap::from([("in progress".to_string(), 2i64)]);
        let running = HashMap::from([
            (
                "ra".to_string(),
                running_entry(issue("ra", "A-0", "In Progress"), "a", "a"),
            ),
            (
                "rb".to_string(),
                running_entry(issue("rb", "B-0", "In Progress"), "b", "b"),
            ),
        ]);
        let o = orch_for_multi(
            10,
            vec![proj("a", 10, cap()), proj("b", 10, cap())],
            Some(running),
        );
        let mut input = tag_for(0, vec![issue("1", "A-1", "In Progress")]);
        input.extend(tag_for(1, vec![issue("2", "B-1", "In Progress")]));
        assert_eq!(
            o.select_dispatch_multi(input).len(),
            0,
            "global in-progress cap=2 already filled by 2 running admits 0"
        );
    }

    // Mirrors Go `TestSelectDispatchMultiPerStateFallbackUsesGlobalCap`.
    #[test]
    fn select_dispatch_multi_per_state_fallback_uses_global_cap() {
        let o = orch_for_multi(
            5,
            vec![proj("a", 1, HashMap::new()), proj("b", 1, HashMap::new())],
            None,
        );
        let mut input = tag_for(
            0,
            vec![
                issue("1", "A-1", "In Progress"),
                issue("2", "A-2", "In Progress"),
            ],
        );
        input.extend(tag_for(
            1,
            vec![
                issue("3", "B-1", "In Progress"),
                issue("4", "B-2", "In Progress"),
            ],
        ));
        let ids = picked_ids(&o.select_dispatch_multi(input));
        assert_eq!(
            ids.len(),
            2,
            "per-state fallback uses GLOBAL cap: 2 admitted (1 per project)"
        );
        let from_a = ids.contains("1") || ids.contains("2");
        let from_b = ids.contains("3") || ids.contains("4");
        assert!(from_a && from_b, "one issue from EACH project");
    }

    // Mirrors Go `TestSelectDispatchMultiEligibilityUsesProjectStates`.
    #[test]
    fn select_dispatch_multi_eligibility_uses_project_states() {
        let mut a = empty_resolved_project("a", Arc::new(Fake::new()));
        a.active_states = set_of(&["started"]);
        a.terminal_states = set_of(&["done"]);
        a.max_concurrent = 10;
        let mut b = empty_resolved_project("b", Arc::new(Fake::new()));
        b.active_states = set_of(&["todo"]);
        b.terminal_states = set_of(&["done"]);
        b.max_concurrent = 10;
        let o = orch_for_multi(10, vec![a, b], None);
        let mut input = tag_for(0, vec![issue("1", "A-1", "Started")]); // active under A
        input.extend(tag_for(1, vec![issue("2", "B-1", "Started")])); // NOT active under B
        let ids = picked_ids(&o.select_dispatch_multi(input));
        assert!(
            ids.contains("1"),
            "active under project A's states should be admitted"
        );
        assert!(
            !ids.contains("2"),
            "not active under project B's states should be skipped"
        );
    }

    // Mirrors Go `TestSelectDispatchMultiDedupHandledByCaller`.
    #[test]
    fn select_dispatch_multi_dedup_handled_by_caller() {
        let o = orch_for_multi(10, vec![proj("a", 10, HashMap::new())], None);
        let input = tag_for(
            0,
            vec![issue("1", "A-1", "Todo"), issue("1", "A-1", "Todo")],
        );
        assert_eq!(
            o.select_dispatch_multi(input).len(),
            1,
            "a duplicate ID must not be admitted twice"
        );
    }

    // Mirrors Go `TestSelectDispatchMultiReservesAcrossExistingRunning`.
    #[test]
    fn select_dispatch_multi_reserves_across_existing_running() {
        let running = HashMap::from([(
            "r1".to_string(),
            running_entry(issue("r1", "A-0", "In Progress"), "a", "a"),
        )]);
        let o = orch_for_multi(10, vec![proj("a", 2, HashMap::new())], Some(running));
        let input = tag_for(
            0,
            vec![issue("1", "A-1", "Todo"), issue("2", "A-2", "Todo")],
        );
        assert_eq!(
            o.select_dispatch_multi(input).len(),
            1,
            "A cap=2 with 1 already running should admit 1 more"
        );
    }

    // Mirrors Go `TestSelectDispatchMultiPerProjectLabelGate`.
    #[test]
    fn select_dispatch_multi_per_project_label_gate() {
        let mut a = empty_resolved_project("a", Arc::new(Fake::new()));
        a.active_states = set_of(&["todo", "in progress"]);
        a.terminal_states = set_of(&["done"]);
        a.max_concurrent = 10;
        a.labels = label_set(&["symphony-do"]);
        let mut b = empty_resolved_project("b", Arc::new(Fake::new()));
        b.active_states = set_of(&["todo", "in progress"]);
        b.terminal_states = set_of(&["done"]);
        b.max_concurrent = 10;
        b.labels = label_set(&["symphony-b"]);
        let o = orch_for_multi(10, vec![a, b], None);

        let mut iss_a = issue("1", "A-1", "Todo");
        iss_a.labels = Some(vec!["symphony-do".to_string()]);
        let mut iss_b = issue("2", "B-1", "Todo");
        iss_b.labels = Some(vec!["symphony-b".to_string()]);

        let mut input = tag_for(0, vec![iss_a.clone()]);
        input.extend(tag_for(1, vec![iss_b]));
        let ids = picked_ids(&o.select_dispatch_multi(input));
        assert!(
            ids.contains("1"),
            "issA (project A's label) admitted under A"
        );
        assert!(
            ids.contains("2"),
            "issB (project B's label) admitted under B"
        );
        assert_eq!(ids.len(), 2);

        // Cross-check: issA tagged under B (wrong label for B) must be rejected.
        let cross = tag_for(1, vec![iss_a]);
        assert_eq!(
            o.select_dispatch_multi(cross).len(),
            0,
            "issA under project B (mismatched label) rejected"
        );
    }

    // ── the Teams work-assignment gate (STUDIO-669; §A of ~/.rhapsody/docs/STUDIO-668-multi-team.md)

    /// An enabled `labels+model` Teams with one topic-labelled teammate, plus the triage seam.
    /// Returns the handle so a test can observe the kick and drive the pending map.
    fn orch_with_teams_gate(roster_labels: &[&str]) -> (Orchestrator, Arc<crate::TriageHandle>) {
        let mut o = orch_for_select(10, HashMap::new(), None);
        o.teams = Some(rhapsody_config::teams::Teams {
            enabled: true,
            roster: vec![rhapsody_config::teams::Identity {
                name: "alice".to_string(),
                profile: "swe".to_string(),
                labels: roster_labels.iter().map(|s| (*s).to_string()).collect(),
                bank: String::new(),
                max_concurrent: 0,
            }],
            ..rhapsody_config::teams::Teams::disabled()
        });
        let handle = Arc::new(crate::TriageHandle::new());
        o.teams_triage = Some(Arc::clone(&handle));
        (o, handle)
    }

    /// A Todo candidate carrying exactly `labels`, in a real Linear team.
    fn teams_issue(id: &str, ident: &str, labels: &[&str]) -> Issue {
        Issue {
            team_id: "team-1".to_string(),
            labels: Some(labels.iter().map(|s| (*s).to_string()).collect()),
            ..issue(id, ident, "Todo")
        }
    }

    /// **The measured bug, as a test** (§A.1): a fresh unlabelled ticket, Teams on, a free seat and
    /// an idle roster. Before STUDIO-669 this dispatched within one 2s tick wearing no identity —
    /// "file it unlabelled and let the manager decide", the flagship flow, structurally losing a
    /// race with its own manager. It is now held for the tick and the triage task is kicked.
    #[test]
    fn an_unassigned_teams_candidate_is_held_and_kicks_triage() {
        let (o, handle) = orch_with_teams_gate(&["rust"]);
        let picked = o.select_dispatch(vec![teams_issue("1", "MT-1", &["docs"])]);

        assert!(
            picked.is_empty(),
            "an unassigned ticket must wait for the team, not dispatch anonymously"
        );
        // The kick is a Notify permit: a `notified()` that resolves at once proves it was sent.
        assert!(
            futures_lite_ready(handle.kicked()),
            "the gate must wake triage now rather than wait out TRIAGE_INTERVAL"
        );
    }

    /// Every way OUT of the hold, in one table — each one dispatches this tick, unheld.
    #[test]
    fn assigned_solo_and_matched_candidates_are_never_held() {
        for (name, labels) in [
            (
                "an identity label is the assignment",
                &["rhapsody:@alice"][..],
            ),
            ("a roster topic label matches", &["rust"][..]),
            ("rhapsody:solo is the opt-out", &["rhapsody:solo"][..]),
        ] {
            let (o, handle) = orch_with_teams_gate(&["rust"]);
            let picked = o.select_dispatch(vec![teams_issue("1", "MT-1", labels)]);
            assert_eq!(picked.len(), 1, "{name}");
            assert!(
                !futures_lite_ready(handle.kicked()),
                "{name}: nothing to kick for"
            );
        }
    }

    /// `default_identity` is the never-refuse floor, so it is also a catch that empties the gate:
    /// with one set, no ticket is ever unassigned and none is ever held.
    #[test]
    fn a_default_identity_empties_the_gate() {
        let (mut o, _) = orch_with_teams_gate(&["rust"]);
        if let Some(t) = o.teams.as_mut() {
            t.manager.default_identity = "alice".to_string();
        }
        assert_eq!(
            o.select_dispatch(vec![teams_issue("1", "MT-1", &["docs"])])
                .len(),
            1,
            "the default catches it, so nothing is pending"
        );
    }

    /// §A.3.4's liveness valve seen from the dispatch side: triage decided, the label write failed,
    /// and the run goes out NOW wearing the pending identity rather than stalling.
    #[test]
    fn a_pending_assignment_releases_the_hold() {
        let (o, handle) = orch_with_teams_gate(&["rust"]);
        let iss = teams_issue("1", "MT-1", &["docs"]);
        assert!(
            o.select_dispatch(vec![iss.clone()]).is_empty(),
            "held first"
        );

        handle.record_pending("1", "alice");
        assert_eq!(
            o.select_dispatch(vec![iss]).len(),
            1,
            "an identity-worn run beats a stalled ticket (§A.3.4)"
        );
    }

    /// The hold is never for a manager that does not exist: no triage seam, no gate. This is the
    /// Teams-off path AND the `mode: off` / hermetic-daemon path, and it is why Teams-off dispatch
    /// is byte-identical.
    #[test]
    fn no_triage_seam_means_no_hold() {
        let (mut o, _) = orch_with_teams_gate(&["rust"]);
        o.teams_triage = None;
        assert_eq!(
            o.select_dispatch(vec![teams_issue("1", "MT-1", &["docs"])])
                .len(),
            1
        );

        let mut off = orch_for_select(10, HashMap::new(), None);
        off.teams = None;
        assert_eq!(
            off.select_dispatch(vec![teams_issue("1", "MT-1", &["docs"])])
                .len(),
            1,
            "Teams off dispatches exactly as it always did"
        );
    }

    /// A ticket whose `rhapsody:@` label names nobody on the roster — the `someone-who-left` case
    /// §0.11.1 names — routes to no identity, but triage will never touch it either, because ANY
    /// identity label makes the field occupied. Holding it would be a hold nothing could release,
    /// and every tick's kick would be a cycle that could not act. It dispatches, exactly as it did
    /// before the gate existed.
    #[test]
    fn a_label_naming_nobody_on_the_roster_is_not_held() {
        let (o, handle) = orch_with_teams_gate(&["rust"]);
        let picked = o.select_dispatch(vec![teams_issue("1", "MT-1", &["rhapsody:@who-left"])]);
        assert_eq!(
            picked.len(),
            1,
            "a hold nothing could release is never taken"
        );
        assert_eq!(picked[0].id, "1");
        assert!(
            !futures_lite_ready(handle.kicked()),
            "and triage is not woken for it"
        );
    }

    /// A ticket with no team id can never be labelled, so holding it would be a hold nothing could
    /// release. Triage drops these candidates for the same reason.
    #[test]
    fn a_ticket_with_no_team_id_is_never_held() {
        let (o, _) = orch_with_teams_gate(&["rust"]);
        let iss = Issue {
            labels: Some(vec!["docs".to_string()]),
            ..issue("1", "MT-1", "Todo")
        };
        assert_eq!(o.select_dispatch(vec![iss]).len(), 1);
    }

    /// Held tickets do not consume slots: the gate is a SKIP, not a reservation, so a ticket behind
    /// a held one still dispatches on the same tick. Loop cadence and the slot budget are untouched.
    #[test]
    fn a_held_ticket_reserves_no_slot() {
        let (o, handle) = orch_with_teams_gate(&["rust"]);
        let picked = o.select_dispatch(vec![
            teams_issue("1", "MT-1", &["docs"]),
            teams_issue("2", "MT-2", &["rust"]),
        ]);
        assert_eq!(
            picked.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
            vec!["2"],
            "the matched ticket goes out on the same tick the unmatched one is held"
        );
        assert!(futures_lite_ready(handle.kicked()));
    }

    /// One kick per PASS, never one per ticket: ten held tickets want one triage cycle.
    #[test]
    fn many_held_tickets_send_one_kick() {
        let (o, handle) = orch_with_teams_gate(&["rust"]);
        let held: Vec<Issue> = (1..=5)
            .map(|n| teams_issue(&n.to_string(), &format!("MT-{n}"), &["docs"]))
            .collect();
        assert!(o.select_dispatch(held).is_empty());

        assert!(futures_lite_ready(handle.kicked()), "one permit");
        assert!(
            !futures_lite_ready(handle.kicked()),
            "and only one: `Notify` coalesces, so five held tickets are not five cycles"
        );
    }

    /// The multi-project pass gates identically — the invariant is about the ticket, not about how
    /// many projects the daemon polls.
    #[test]
    fn the_multi_project_pass_gates_the_same_way() {
        let projects = vec![proj("p1", 10, HashMap::new())];
        let mut o = orch_for_multi(10, projects, None);
        o.teams = Some(rhapsody_config::teams::Teams {
            enabled: true,
            roster: vec![rhapsody_config::teams::Identity {
                name: "alice".to_string(),
                profile: "swe".to_string(),
                labels: vec!["rust".to_string()],
                bank: String::new(),
                max_concurrent: 0,
            }],
            ..rhapsody_config::teams::Teams::disabled()
        });
        let handle = Arc::new(crate::TriageHandle::new());
        o.teams_triage = Some(Arc::clone(&handle));

        let picked = o.select_dispatch_multi(vec![
            TaggedIssue {
                iss: teams_issue("1", "MT-1", &["docs"]),
                proj: Some(0),
            },
            TaggedIssue {
                iss: teams_issue("2", "MT-2", &["rhapsody:solo"]),
                proj: Some(0),
            },
        ]);
        assert_eq!(
            picked.iter().map(|t| t.iss.id.as_str()).collect::<Vec<_>>(),
            vec!["2"],
            "unassigned held, solo through"
        );
        assert!(futures_lite_ready(handle.kicked()));
    }

    /// Polls a future once and reports whether it was already ready. Enough for `Notify`, whose
    /// `notified()` resolves immediately exactly when a permit is waiting — and it keeps these
    /// tests synchronous, like every other test in this module.
    fn futures_lite_ready(fut: impl std::future::Future<Output = ()>) -> bool {
        use std::task::{Context, Poll, Waker};
        let mut cx = Context::from_waker(Waker::noop());
        let mut fut = Box::pin(fut);
        matches!(fut.as_mut().poll(&mut cx), Poll::Ready(()))
    }
}
