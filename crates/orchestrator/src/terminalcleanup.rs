//! terminalcleanup — the PERIODIC terminal-workspace sweep (STUDIO-1520; no Go counterpart).
//!
//! Startup removes the worktrees of tickets already terminal at boot (§8.6, `startupCleanup`). A
//! ticket that goes terminal AFTER its run ends — the ordinary case, where a pull request merges
//! later and `done_state` moves the ticket while no run is live — had no sweep at all: its worktree
//! (and the `target/` inside it) stayed on disk until the next restart. On a daemon that is not
//! restarted often that is roughly 20 GiB per merged Rust ticket.
//!
//! This module adds the missing pass: the SAME cleanup startup runs — the same per-project terminal
//! states, the same disk-driven walk and `runs`-ledger mapping, the same batched tracker read
//! (STUDIO-1466) — on a bounded interval while the daemon is up. The sweep core is shared with the
//! startup pass (`Orchestrator::startup_cleanup`), so the two can never disagree about what is
//! terminal.
//!
//! It runs OFF the control loop, exactly like the workspace GC: the slow half (list the worktrees,
//! read the `runs` ledger, one batched tracker read per project, remove worktrees) executes on a
//! spawned task, and the plan it consumes is snapshotted on the control task through ONE round-trip
//! ([`Event::TerminalCleanup`]). A sweep must never block the control loop — the startup pass is
//! allowed to sit between boot recovery and the loop, but a periodic one is not.
//!
//! Three things are protected from removal beyond "the ticket is terminal": a workspace with a live
//! run, one with a queued retry, and one whose pull request is under an active review watch. The
//! first two are read from the live maps on the control task; the third from the durable watch set,
//! through the ticket its `introduced_by` origin names ([`rhapsody_store::origin_ticket`]). The
//! protection is a belt-and-braces race guard — a terminal ticket should never be mid-dispatch — but
//! it is what makes the pass safe to run against a moving board.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use rhapsody_core::{Issue, normalize_state};
use rhapsody_store::Store;
use rhapsody_tracker::Tracker;
use rhapsody_workspace::{Manager, sanitize_key};

use crate::control_loop::{CancelWait, Event};
use crate::orchestrator::Orchestrator;
use crate::stop::ControlHandle;

/// How often the daemon runs the periodic terminal-workspace sweep while it is up. Bounded and
/// fixed: the pass is now cheap (a directory listing plus one batched tracker read per project), so
/// an hour bounds the disk a merged ticket can leak without adding a config knob nobody asked for.
pub const TERMINAL_CLEANUP_INTERVAL: Duration = Duration::from_secs(3600);

/// One project's inputs to a terminal-cleanup pass: its slug-bound tracker, its own terminal-state
/// list, and the workspace manager + repository its worktrees live under. Built on the control task
/// (`Orchestrator::build_terminal_cleanup_plan`) and consumed off-loop.
pub struct TerminalCleanupProject {
    pub tracker: Arc<dyn Tracker>,
    pub terminal: HashSet<String>,
    pub workspace: Arc<Manager>,
    pub repo: String,
    pub slug: String,
}

/// The race-free snapshot one sweep operates on. Built on the control task so the live scheduling
/// maps are read without aliasing, then handed to the off-loop sweep.
pub struct TerminalCleanupPlan {
    /// Per-project cleanup inputs; empty when no effective config is loaded yet (a no-op pass).
    pub projects: Vec<TerminalCleanupProject>,
    /// Sanitized worktree keys that must NOT be removed: a live run's ticket, a queued retry's
    /// ticket, or the ticket named by a live review watch row's origin.
    pub protect: HashSet<String>,
    /// The `runs` ledger the disk-driven mapping reads (`distinct_issue_ids`). The SAME handle the
    /// control task holds, so the store's own connection lock serializes the read.
    pub store: Arc<dyn Store + Send + Sync>,
    /// The retained-opencode-session root, so a terminal ticket's kept session is discarded beside
    /// its worktree. `None` when no config is loaded.
    pub opencode_state_root: Option<String>,
}

/// What one terminal-cleanup pass did: how many worktrees it found on disk, how many of those whose
/// ticket came back terminal, how many it removed, and how long it took. Shared by the startup pass
/// and the periodic one so both log the same numbers.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TerminalCleanupSummary {
    pub(crate) worktrees: usize,
    pub(crate) terminal: usize,
    pub(crate) removed: usize,
    pub(crate) elapsed_ms: u64,
}

/// Runs one terminal-cleanup pass: for each project, list the worktrees ON DISK, map each to its
/// tracker id through the store's `runs` ledger, resolve every state in ONE batched
/// `fetch_issue_states_by_ids` call per project (plus a per-identifier fallback for a worktree the
/// store cannot map), and remove only the worktrees whose ticket comes back terminal and is not in
/// `plan.protect`. The mechanism is inverted from Go's by-state page (STUDIO-1466, see the README
/// Divergences entry); the removal SET is the reference's "remove only what comes back terminal".
///
/// The caller logs the one INFO summary line (the startup pass and the periodic one use different
/// messages), so this core stays side-effect-free apart from the removals themselves and both passes
/// report the same numbers in the same shape.
///
/// Runs off the control loop. Pure with respect to `Orchestrator` state — everything it needs is on
/// `plan` — so the startup pass and the periodic one share exactly this body.
pub(crate) async fn run_terminal_sweep(plan: &TerminalCleanupPlan) -> TerminalCleanupSummary {
    let started = std::time::Instant::now();
    let mut summary = TerminalCleanupSummary::default();
    for g in &plan.projects {
        // 1. The worktrees that actually exist on disk, one key each.
        let keys = g.workspace.worktree_keys(&g.repo);
        summary.worktrees += keys.len();
        if keys.is_empty() {
            continue;
        }
        // 2. Map each key to its tracker id from the store's `runs` ledger so the state read is
        //    batched. The ledger records the identifier, not the sanitized directory key, so the
        //    match is made on the key the worktree path is derived from. A store read that fails (or
        //    a store that is off) leaves every key unmapped and takes the fallback below.
        //
        //    A review/manager/landing worktree is keyed by a synthetic `pr:…`/`lead:…` run key that
        //    no tracker can resolve, so it is skipped rather than mapped: the reference's by-state
        //    page never returned it either (no such issue exists), and sending the non-UUID key in
        //    the batched id read would fail the whole request (STUDIO-831).
        let mut by_key: HashMap<String, Option<String>> = HashMap::new();
        match plan.store.distinct_issue_ids(&g.slug) {
            Ok(pairs) => {
                for (identifier, id) in pairs {
                    let mapped = if crate::review::is_non_tracker_key(&identifier) {
                        None
                    } else {
                        Some(id)
                    };
                    by_key.entry(sanitize_key(&identifier)).or_insert(mapped);
                }
            }
            Err(e) => {
                tracing::warn!(project_slug = %g.slug, err = %e, "terminal cleanup store read failed; falling back to per-identifier reads");
            }
        }
        let mut ids: Vec<String> = Vec::new();
        let mut unmapped: Vec<String> = Vec::new();
        for key in &keys {
            if plan.protect.contains(key) {
                continue; // a live run, a queued retry or a watched PR owns this worktree
            }
            match by_key.get(key) {
                Some(Some(id)) => ids.push(id.clone()),
                Some(None) => {} // a review/manager/landing worktree: no tracker ticket to consult
                None => unmapped.push(key.clone()),
            }
        }
        // 3. One batched state read per project, plus per-identifier reads for the stragglers.
        let mut issues: Vec<Issue> = Vec::new();
        let mut batch_ok = true;
        if !ids.is_empty() {
            match g.tracker.fetch_issue_states_by_ids(&ids).await {
                Ok(v) => issues.extend(v),
                Err(e) => {
                    batch_ok = false;
                    tracing::warn!(project_slug = %g.slug, err = %e, "terminal cleanup fetch failed; continuing");
                }
            }
        }
        if batch_ok {
            for identifier in unmapped {
                match g.tracker.fetch_issue_by_identifier(&identifier).await {
                    Ok(Some(iss)) => issues.push(iss),
                    Ok(None) => {}
                    Err(e) => {
                        tracing::warn!(issue_identifier = %identifier, project_slug = %g.slug, err = %e, "terminal cleanup fetch failed; continuing")
                    }
                }
            }
        }
        // 4. Remove only the worktrees whose ticket came back terminal.
        for iss in issues {
            if !g.terminal.contains(&normalize_state(&iss.state)) {
                continue;
            }
            // The key-level skip above already refused a protected worktree before the state read;
            // this re-check is the defensive twin, so a tracker that returned an identifier whose
            // sanitized key is protected can still never be removed.
            if plan.protect.contains(&sanitize_key(&iss.identifier)) {
                continue;
            }
            summary.terminal += 1;
            // A worktree can vanish between the listing above and here; the guard keeps the removal
            // (a no-op for a missing path) from being counted as a removal.
            if g.workspace.worktree_exists(&g.repo, &iss.identifier) {
                match g
                    .workspace
                    .remove_worktree(&g.repo, &g.slug, &iss.identifier)
                    .await
                {
                    Ok(()) => summary.removed += 1,
                    Err(e) => {
                        tracing::warn!(issue_identifier = %iss.identifier, project_slug = %g.slug, err = %e, "terminal workspace cleanup failed")
                    }
                }
            }
            // The ticket is terminal: its retained opencode session (if any) is dead weight, in
            // legacy mode a copy of the operator's credential included (STUDIO-1043).
            if let Some(root) = plan.opencode_state_root.as_deref() {
                rhapsody_agent::opencode::resume::discard(root, &iss.identifier);
            }
        }
    }
    summary.elapsed_ms = started.elapsed().as_millis() as u64;
    summary
}

/// Runs the terminal-cleanup sweep once per `interval` until `ctx` is cancelled, off the control
/// loop. `sweep` is the off-loop driver (a [`ControlHandle::sweep_terminal_workspaces`] round-trip
/// in production); a cancelled `ctx` interrupts a sweep already in flight. Mirrors
/// `run_prune_schedule`'s cadence, with a much shorter interval.
pub async fn run_terminal_cleanup_schedule<SW, Fut>(
    mut ctx: CancelWait,
    interval: Duration,
    sweep: SW,
) where
    SW: Fn() -> Fut,
    Fut: std::future::Future<Output = usize>,
{
    // `interval_at(now + interval, interval)` matches Go's `time.NewTicker` — the first sweep runs
    // one interval in, since startup already ran the same pass at boot.
    let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + interval, interval);
    loop {
        tokio::select! {
            _ = ctx.cancelled() => return,
            _ = ticker.tick() => {}
        }
        tokio::select! {
            biased;
            _ = ctx.cancelled() => return,
            _ = sweep() => {}
        }
    }
}

impl Orchestrator {
    /// Snapshots everything a terminal-cleanup pass needs. MUST run on the control task: it reads
    /// `self.eff`, `self.running`, `self.retry_attempts` and `self.store`. Mirrors the inputs
    /// `startupCleanup` reads, plus (when `protect_live`) the protection set a periodic pass needs.
    ///
    /// The startup pass calls this with `protect_live == false` so its behaviour is exactly the
    /// reference's: at boot nothing is dispatched yet, and a run `boot_recovery` adopted just before
    /// cleanup is not treated differently from the frozen reference (which removed terminal
    /// worktrees unconditionally). The periodic pass protects live state.
    pub(crate) fn build_terminal_cleanup_plan(&self, protect_live: bool) -> TerminalCleanupPlan {
        let mut projects = Vec::new();
        if let Some(eff) = self.eff.as_ref() {
            if !eff.projects.is_empty() {
                for p in &eff.projects {
                    projects.push(TerminalCleanupProject {
                        tracker: Arc::clone(&p.tracker),
                        terminal: p
                            .mcfg
                            .tracker
                            .terminal_states
                            .iter()
                            .map(|s| normalize_state(s))
                            .collect(),
                        workspace: Arc::clone(&p.workspace),
                        repo: p.repo.clone(),
                        slug: p.slug.clone(),
                    });
                }
            } else {
                projects.push(TerminalCleanupProject {
                    tracker: Arc::clone(&eff.tracker),
                    terminal: eff
                        .cfg
                        .tracker
                        .terminal_states
                        .iter()
                        .map(|s| normalize_state(s))
                        .collect(),
                    workspace: Arc::clone(&eff.workspace),
                    repo: String::new(),
                    slug: String::new(),
                });
            }
        }

        let mut protect: HashSet<String> = HashSet::new();
        if protect_live {
            // The worktrees a running or queued ticket owns are never candidates. A retry is keyed by
            // the tracker id on some paths and the identifier on others (recovery), so both are
            // protected.
            for re in self.running.values() {
                protect.insert(sanitize_key(&re.issue.identifier));
            }
            for re in self.retry_attempts.values() {
                protect.insert(sanitize_key(&re.identifier));
                if !re.issue_id.is_empty() {
                    protect.insert(sanitize_key(&re.issue_id));
                }
            }
            // A watched pull request still owns its ticket's worktree. The watch row names the ticket
            // through its origin (`handoff:TICKET` / `adopt:TICKET`); a `console:` origin names an
            // operator and contributes nothing.
            match self.store().load_live_review_watch() {
                Ok(rows) => {
                    for row in rows {
                        if let Some(ticket) = rhapsody_store::origin_ticket(&row.introduced_by) {
                            protect.insert(sanitize_key(ticket));
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(err = %e, "terminal cleanup: the review watch set could not be read; a watched ticket's worktree may be swept this pass")
                }
            }
        }

        TerminalCleanupPlan {
            projects,
            protect,
            store: Arc::clone(&self.store),
            opencode_state_root: self.opencode_state_root(),
        }
    }

    /// Spawns the periodic terminal-cleanup sweep off the control loop until `ctx` is cancelled.
    /// Returns the task handle so the caller can abort it after the loop returns. The startup pass
    /// runs the same core once, on the loop, before the first tick.
    pub(crate) fn start_terminal_cleanup_schedule(
        &self,
        ctx: CancelWait,
        interval: Duration,
    ) -> tokio::task::JoinHandle<()> {
        let handle = self.control();
        tokio::spawn(async move {
            run_terminal_cleanup_schedule(ctx, interval, move || {
                let handle = handle.clone();
                async move { handle.sweep_terminal_workspaces().await }
            })
            .await;
        })
    }
}

impl ControlHandle {
    /// Requests the race-free terminal-cleanup plan, built on the control task. Backs
    /// [`Self::sweep_terminal_workspaces`].
    async fn terminal_cleanup_plan(&self) -> Option<TerminalCleanupPlan> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.events
            .send(Event::TerminalCleanup { reply: tx })
            .ok()?;
        rx.await.ok()
    }

    /// Runs one terminal-cleanup pass OFF the control loop: snapshots the plan via the control
    /// channel, removes every terminal ticket's worktree that is not protected, and logs one INFO
    /// summary line. Returns the number of worktrees removed. A missing plan (no effective config,
    /// or a gone loop) is a no-op.
    pub async fn sweep_terminal_workspaces(&self) -> usize {
        let Some(plan) = self.terminal_cleanup_plan().await else {
            return 0;
        };
        let summary = run_terminal_sweep(&plan).await;
        tracing::info!(
            worktrees = summary.worktrees,
            terminal = summary.terminal,
            removed = summary.removed,
            elapsed_ms = summary.elapsed_ms,
            "periodic terminal cleanup finished"
        );
        summary.removed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use rhapsody_store::{
        REVIEW_STATUS_REQUESTED, ReviewWatchKey, ReviewWatchRow, RunStart, Sqlite, StorePath,
    };
    use rhapsody_tracker::fake::Fake;

    use crate::control_loop::CancelSignal;
    use crate::testsupport::{
        TempDir, empty_effective, empty_resolved_project, issue, mk_workspace,
        recording_subscriber, retry_entry, running_entry,
    };

    /// Seeds one `runs` row so the store can map a worktree's key back to its tracker id — the
    /// ledger the disk-driven sweep reads (STUDIO-1466).
    fn seed_run(o: &Orchestrator, identifier: &str, id: &str, project: &str) {
        o.store()
            .start_run(RunStart {
                issue_id: id.to_string(),
                issue_identifier: identifier.to_string(),
                project_slug: project.to_string(),
                ..Default::default()
            })
            .expect("seed run");
    }

    /// One project `a` whose tracker resolves `by_id`, a real workspace rooted at a temp dir, and an
    /// in-memory store. `terminal_states` is `[Done]`, so any ticket the fake returns as `Done` is a
    /// removal candidate.
    fn orch_for_cleanup(
        by_id: HashMap<String, Issue>,
    ) -> (Orchestrator, Arc<Fake>, Arc<Manager>, TempDir) {
        let mut tr = Fake::new();
        tr.by_id = by_id;
        let tr = Arc::new(tr);
        let dir = TempDir::new();
        let ws = mk_workspace(&dir.path);
        let mut pa = empty_resolved_project("a", Arc::clone(&tr) as Arc<dyn Tracker>);
        pa.workspace = Arc::clone(&ws);
        pa.mcfg.tracker.terminal_states = vec!["Done".to_string()];
        let mut eff = empty_effective(Arc::new(Fake::new()));
        eff.projects = vec![pa];
        let mut o = Orchestrator::new("WORKFLOW.md");
        o.set_store(Arc::new(
            Sqlite::open(StorePath::InMemory).expect("in-memory store"),
        ));
        o.eff = Some(eff);
        (o, tr, ws, dir)
    }

    /// Seeds one LIVE review-watch row naming `ticket` through its origin.
    fn seed_watch(o: &Orchestrator, ticket: &str) {
        o.store()
            .save_review_watch(ReviewWatchRow {
                key: ReviewWatchKey {
                    owner: "makewhatis".into(),
                    repo: "rhapsody".into(),
                    number: 12,
                    reviewer: "alice".into(),
                },
                author: "bob".into(),
                introduced_by: format!("handoff:{ticket}"),
                requested_sha: "sha".into(),
                last_reviewed_sha: String::new(),
                status: REVIEW_STATUS_REQUESTED.into(),
                open: true,
            })
            .expect("seed watch");
    }

    // STUDIO-1520 acceptance (1): a worktree whose ticket went terminal AFTER its run ended — no live
    // run, no queued retry, no watch — is removed by the PERIODIC sweep against a live control loop,
    // with no restart. The startup pass cannot catch this: it runs once, at boot, before the ticket
    // goes terminal. The same call logs the one INFO summary line, in the startup pass's shape.
    #[tokio::test]
    async fn periodic_sweep_removes_a_terminal_worktree_through_the_control_loop() {
        // `tracing`'s callsite Interest cache is global (TRA-243); hold the lock so no parallel test
        // can cache this callsite as uninterested while the subscriber below is installed.
        let _serial = crate::testsupport::TRACING_TEST_LOCK.lock().await;
        let (mut o, _tr, ws, _dir) = orch_for_cleanup(HashMap::from([(
            "a1".to_string(),
            issue("a1", "A-1", "Done"),
        )]));
        seed_run(&o, "A-1", "a1", "a");
        let w = ws.create_for_issue("", "A-1").await.expect("create ws");
        assert!(
            std::fs::metadata(&w.path).is_ok(),
            "the worktree is on disk before the sweep"
        );

        let signal = CancelSignal::new();
        o.ctx = Some(signal.wait());
        let handle = o.control();
        let loop_ctx = signal.wait();
        let looped = tokio::spawn(async move { o.run_loaded(loop_ctx).await });

        let (events, subscriber) = recording_subscriber();
        let guard = tracing::subscriber::set_default(subscriber);
        tracing::callsite::rebuild_interest_cache();
        let removed = handle.sweep_terminal_workspaces().await;
        drop(guard);

        signal.cancel();
        looped.await.expect("control loop");

        assert_eq!(
            removed, 1,
            "the periodic sweep removes the ticket's worktree"
        );
        assert!(
            std::fs::metadata(&w.path).is_err(),
            "the terminal worktree is gone without a restart"
        );
        let events = events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let line = events
            .iter()
            .find(|e| e.message == "periodic terminal cleanup finished")
            .unwrap_or_else(|| {
                panic!(
                    "the one periodic cleanup summary line must be logged; got {:?}",
                    events.iter().map(|e| &e.message).collect::<Vec<_>>()
                )
            });
        assert_eq!(line.level, "INFO");
        assert_eq!(line.fields.get("worktrees").map(String::as_str), Some("1"));
        assert_eq!(line.fields.get("terminal").map(String::as_str), Some("1"));
        assert_eq!(line.fields.get("removed").map(String::as_str), Some("1"));
        assert!(
            line.fields.contains_key("elapsed_ms"),
            "the summary must carry how long the sweep took"
        );
    }

    // STUDIO-1520 acceptance (2a, production wiring): the LIVE control loop's terminal-cleanup
    // handler must ask for the protection set. The guard tests below call the plan builder
    // directly, so they pin the predicate but not the argument the ONE production caller passes;
    // this one drives `ControlHandle::sweep_terminal_workspaces()` through `run_loaded` with a live
    // run, so flipping the handler's `protect_live` to `false` — or dropping the running half of
    // the set — removes the worktree and asks the tracker about it, failing here.
    //
    // MUTATION: `crates/orchestrator/src/loop.rs`'s `Event::TerminalCleanup` arm
    // `build_terminal_cleanup_plan(true)` → `(false)` makes this test red.
    #[tokio::test]
    async fn periodic_sweep_leaves_a_live_runs_worktree_through_the_control_loop() {
        // The sweep logs through the same callsite AC1 asserts on; hold the lock so no parallel test
        // can cache that callsite as uninterested (TRA-243).
        let _serial = crate::testsupport::TRACING_TEST_LOCK.lock().await;
        let (mut o, tr, ws, _dir) = orch_for_cleanup(HashMap::from([(
            "a1".to_string(),
            issue("a1", "A-1", "Done"),
        )]));
        seed_run(&o, "A-1", "a1", "a");
        let w = ws.create_for_issue("", "A-1").await.expect("create ws");
        o.running.insert(
            "a1".into(),
            running_entry(issue("a1", "A-1", "Done"), "a", "a"),
        );

        let signal = CancelSignal::new();
        o.ctx = Some(signal.wait());
        let handle = o.control();
        let loop_ctx = signal.wait();
        let looped = tokio::spawn(async move { o.run_loaded(loop_ctx).await });

        let removed = handle.sweep_terminal_workspaces().await;

        signal.cancel();
        looped.await.expect("control loop");

        assert_eq!(
            removed, 0,
            "the periodic sweep never removes a live run's worktree"
        );
        assert!(
            std::fs::metadata(&w.path).is_ok(),
            "the live run's worktree survives the sweep without a restart"
        );
        assert_eq!(
            tr.by_id_calls(),
            0,
            "the protected worktree is skipped before the state read"
        );
    }

    // STUDIO-1520 acceptance (2a): a LIVE run's worktree is never removed, even though its ticket
    // comes back terminal. The skip happens before the state read, so the tracker is never asked.
    //
    // MUTATION: drop the running half of the protection set and the worktree is fetched, found
    // terminal, and removed.
    #[tokio::test]
    async fn sweep_leaves_a_live_runs_worktree() {
        let (mut o, tr, ws, _dir) = orch_for_cleanup(HashMap::from([(
            "a1".to_string(),
            issue("a1", "A-1", "Done"),
        )]));
        seed_run(&o, "A-1", "a1", "a");
        let w = ws.create_for_issue("", "A-1").await.expect("create ws");
        o.running.insert(
            "a1".into(),
            running_entry(issue("a1", "A-1", "Done"), "a", "a"),
        );

        let plan = o.build_terminal_cleanup_plan(true);
        let summary = run_terminal_sweep(&plan).await;

        assert_eq!(summary.removed, 0, "a live run's worktree is never removed");
        assert!(
            std::fs::metadata(&w.path).is_ok(),
            "the live run's worktree survives the sweep"
        );
        assert_eq!(
            tr.by_id_calls(),
            0,
            "a protected worktree is skipped before the state read"
        );
    }

    // STUDIO-1520 acceptance (2b): a worktree with a QUEUED RETRY is never removed.
    //
    // MUTATION: drop the retry half of the protection set and the worktree is removed.
    #[tokio::test]
    async fn sweep_leaves_a_pending_retrys_worktree() {
        let (mut o, tr, ws, _dir) = orch_for_cleanup(HashMap::from([(
            "a1".to_string(),
            issue("a1", "A-1", "Done"),
        )]));
        seed_run(&o, "A-1", "a1", "a");
        let w = ws.create_for_issue("", "A-1").await.expect("create ws");
        o.retry_attempts
            .insert("a1".into(), retry_entry("a1", "A-1", 1));

        let plan = o.build_terminal_cleanup_plan(true);
        let summary = run_terminal_sweep(&plan).await;

        assert_eq!(
            summary.removed, 0,
            "a queued retry's worktree is never removed"
        );
        assert!(
            std::fs::metadata(&w.path).is_ok(),
            "the queued retry's worktree survives the sweep"
        );
        assert_eq!(tr.by_id_calls(), 0, "it is skipped before the state read");
    }

    // STUDIO-1520 acceptance (2c): a worktree whose pull request is under an ACTIVE REVIEW WATCH is
    // never removed, even though its ticket comes back terminal. The watch row names the ticket
    // through its `handoff:` origin.
    //
    // MUTATION: drop the watch half of the protection set and the worktree is removed.
    #[tokio::test]
    async fn sweep_leaves_a_watched_tickets_worktree() {
        let (o, tr, ws, _dir) = orch_for_cleanup(HashMap::from([(
            "a1".to_string(),
            issue("a1", "A-1", "Done"),
        )]));
        seed_run(&o, "A-1", "a1", "a");
        seed_watch(&o, "A-1");
        let w = ws.create_for_issue("", "A-1").await.expect("create ws");

        let plan = o.build_terminal_cleanup_plan(true);
        let summary = run_terminal_sweep(&plan).await;

        assert_eq!(
            summary.removed, 0,
            "a watched ticket's worktree is never removed"
        );
        assert!(
            std::fs::metadata(&w.path).is_ok(),
            "the watched ticket's worktree survives the sweep"
        );
        assert_eq!(tr.by_id_calls(), 0, "it is skipped before the state read");
    }

    // STUDIO-1520: a watch row whose origin names an OPERATOR (`console:`) protects nothing — the
    // same ticket-bearing-origin rule `DaemonPrLinks` applies.
    #[tokio::test]
    async fn sweep_removes_a_terminal_worktree_with_only_a_console_origin_watch() {
        let (o, _tr, ws, _dir) = orch_for_cleanup(HashMap::from([(
            "a1".to_string(),
            issue("a1", "A-1", "Done"),
        )]));
        seed_run(&o, "A-1", "a1", "a");
        o.store()
            .save_review_watch(ReviewWatchRow {
                key: ReviewWatchKey {
                    owner: "makewhatis".into(),
                    repo: "rhapsody".into(),
                    number: 12,
                    reviewer: "alice".into(),
                },
                author: "bob".into(),
                introduced_by: "console:operator".into(),
                requested_sha: "sha".into(),
                last_reviewed_sha: String::new(),
                status: REVIEW_STATUS_REQUESTED.into(),
                open: true,
            })
            .expect("seed watch");
        let w = ws.create_for_issue("", "A-1").await.expect("create ws");

        let plan = o.build_terminal_cleanup_plan(true);
        let summary = run_terminal_sweep(&plan).await;

        assert_eq!(summary.removed, 1, "a console origin protects no ticket");
        assert!(std::fs::metadata(&w.path).is_err());
    }

    // STUDIO-1520: the schedule runs the sweep once per interval and stops on cancellation.
    #[tokio::test]
    async fn the_schedule_runs_the_sweep_on_its_interval_until_cancelled() {
        let signal = CancelSignal::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&calls);
        let task = tokio::spawn(run_terminal_cleanup_schedule(
            signal.wait(),
            Duration::from_millis(10),
            move || {
                counted.fetch_add(1, Ordering::SeqCst);
                async { 0usize }
            },
        ));

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while calls.load(Ordering::SeqCst) == 0 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the schedule never ran the sweep"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        signal.cancel();
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("the schedule stops on cancellation")
            .expect("schedule task panicked");
    }
}
