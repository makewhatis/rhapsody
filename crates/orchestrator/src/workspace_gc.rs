//! workspace_gc — parity port of Go `internal/orchestrator/workspace_gc.go` (the workspace GC pass).
//!
//! The GC prunes per-issue worktrees idle beyond `retention_days`, never touching one that belongs
//! to a currently-running issue. Go runs the slow filesystem/git removal OFF the control goroutine
//! and round-trips through it (via the `evWorkspaceGC` / `evWorkspaceInUse` control events) for a
//! race-free snapshot + an authoritative pre-removal liveness re-check.
//!
//! O3 ports the loop-confined, channel-free helpers the GC is built from — the plan snapshot
//! ([`WorkspaceGcPlan`] + [`Orchestrator::build_workspace_gc_plan`]), the on-disk worktree path
//! reconstruction ([`worktree_path_for`]), and the authoritative liveness check
//! ([`Orchestrator::worktree_in_use`]). The off-loop driver `PruneStaleWorkspaces` and the
//! `evWorkspaceGC`/`evWorkspaceInUse` round-trip depend on the control-loop mpsc event channel +
//! `handle` dispatcher, which are O7 (`loop.rs`); they — and Go's
//! `TestEvWorkspaceInUse_RoundTripsThroughControlLoop`, which spins up that loop — land with O7's
//! event channel, wiring these helpers behind it. This is a `semantics over structure` split: the
//! observable behavior these helpers assert is complete and independently tested here.

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use rhapsody_workspace::Manager;

use crate::orchestrator::{Orchestrator, RunningEntry};
use crate::stop::ControlHandle;

/// The race-free snapshot the workspace GC operates on: the live workspace [`Manager`] and the set of
/// worktree paths that must NOT be pruned because a worker is currently running on them. Built on the
/// control task ([`Orchestrator::build_workspace_gc_plan`]) and consumed off-loop by the O7 prune
/// driver. Mirrors Go `workspaceGCPlan`.
pub struct WorkspaceGcPlan {
    /// All projects share the one effective workspace manager (see `effective.rs`), so a single
    /// manager + root covers every worktree. `None` when no effective config is built yet. Read by
    /// the O7 off-loop prune driver (`PruneStaleWorkspaces`).
    pub mgr: Option<Arc<Manager>>,
    /// The absolute worktree paths of every currently-running issue (the keep set). Read by the O7
    /// prune driver and passed to `Manager::prune_stale_worktrees`.
    pub keep: HashSet<String>,
}

/// Reconstructs a running entry's exact on-disk worktree path. `path_for` mirrors the workspace
/// provisioning layout: repo-backed => `<root>/<RepoKey>/<key>`, legacy (empty repo) => `<root>/<key>`.
/// `project_repo` is the same URL the worker dispatched with. Mirrors Go `worktreePathFor` (a method
/// that ignores its receiver; a free fn in Rust, per `semantics over structure`).
pub(crate) fn worktree_path_for(mgr: &Manager, re: &RunningEntry) -> String {
    mgr.path_for(&re.project_repo, &re.issue.identifier)
}

impl Orchestrator {
    /// Assembles the GC plan from current state. MUST run on the control task: it reads `self.eff`
    /// and `self.running`, both control-task-owned. Mirrors Go `buildWorkspaceGCPlan`.
    pub fn build_workspace_gc_plan(&self) -> WorkspaceGcPlan {
        let mgr = self.eff.as_ref().map(|e| Arc::clone(&e.workspace));
        let mut keep = HashSet::with_capacity(self.running.len());
        if let Some(m) = &mgr {
            for re in self.running.values() {
                keep.insert(worktree_path_for(m, re));
            }
        }
        WorkspaceGcPlan { mgr, keep }
    }

    /// Reports whether `path` is the worktree of a currently-running issue. MUST run on the control
    /// task: it reads the live `self.running`. It is the authoritative, snapshot-free liveness check
    /// the GC consults immediately before each removal. Mirrors Go `worktreeInUse`.
    ///
    /// `mgr` is the GC plan's manager (NOT `self.eff.workspace`): paths must be reconstructed against
    /// the SAME root the scan walks. A mid-prune `workspace.root` reload swaps `self.eff.workspace`
    /// for a differently-rooted manager; using it here would answer liveness for the new root while
    /// the scan walks the old one, and a live worktree could be pruned. Only the path COMPUTATION is
    /// pinned to the plan's `mgr`; the live `self.running` is still read. `None` mgr ⇒ not in use.
    pub fn worktree_in_use(&self, mgr: Option<&Manager>, path: &str) -> bool {
        let Some(m) = mgr else {
            return false;
        };
        self.running
            .values()
            .any(|re| worktree_path_for(m, re) == path)
    }
}

impl ControlHandle {
    /// The effective `storage.retention_days` (default 30 until the first reload), read each prune
    /// cycle from the shared atomic without racing the control task's reload. Mirrors Go
    /// `CurrentRetentionDays`.
    pub fn current_retention_days(&self) -> i64 {
        self.retention_days.load(Ordering::Relaxed)
    }

    /// Whether the reload path has stored the effective retention_days at least once. The prune
    /// scheduler reads it to skip the STARTUP worktree GC while `current_retention_days` would still
    /// return the `New` default. Mirrors Go `RetentionLoaded`.
    pub fn retention_loaded(&self) -> bool {
        self.retention_loaded.load(Ordering::Relaxed)
    }

    /// Prunes per-issue worktrees idle beyond `retention_days`, OFF the control loop: snapshots the
    /// GC plan (the live [`Manager`] + the running keep-set) via the control channel, then removes
    /// each stale worktree that is neither in the keep-set nor reported in-use by the authoritative
    /// liveness re-check (itself a control round-trip carrying the plan's manager, so liveness paths
    /// track the scanned root even across a `workspace.root` reload). Returns the count removed; a
    /// `retention_days <= 0`, a missing plan, or no built manager is a no-op. Backs the daemon's prune
    /// scheduler. Mirrors Go `PruneStaleWorkspaces`.
    pub async fn prune_stale_workspaces(&self, retention_days: i64) -> usize {
        if retention_days <= 0 {
            return 0;
        }
        let Some(plan) = self.workspace_gc_plan().await else {
            return 0;
        };
        let Some(mgr) = plan.mgr else {
            return 0;
        };
        let max_age = Duration::from_secs(retention_days as u64 * 24 * 3600);
        // The liveness callback round-trips the loop (evWorkspaceInUse). Cloning the handle + manager
        // per call keeps the closure `Fn`; a dropped loop reports not-in-use, but the prune scheduler
        // cancels its ctx before shutdown, so a removal-under-doubt cannot occur in practice.
        let handle = self.clone();
        let live_mgr = Arc::clone(&mgr);
        let live = move |path: String| -> Pin<Box<dyn Future<Output = bool> + Send>> {
            let handle = handle.clone();
            let mgr = Arc::clone(&live_mgr);
            Box::pin(async move { handle.worktree_in_use(Some(mgr), path).await })
        };
        mgr.prune_stale_worktrees(max_age, &plan.keep, Some(&live))
            .await
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use rhapsody_tracker::Tracker;
    use rhapsody_workspace::{self as workspace, HookScripts, Manager};

    use super::*;
    use crate::testsupport::{TempDir, empty_effective, issue, running_entry};

    /// Builds an orchestrator whose effective workspace manager is rooted at a fresh temp dir, plus
    /// that manager and the root guard. Mirrors Go `orchForGC`.
    fn orch_for_gc() -> (Orchestrator, Arc<Manager>, TempDir) {
        let root = TempDir::new();
        let wm = Arc::new(
            Manager::new(workspace::Config {
                root: root.path.clone(),
                hooks: HookScripts::default(),
                hook_timeout: Duration::from_secs(1),
            })
            .expect("workspace manager"),
        );
        let mut o = Orchestrator::new("WORKFLOW.md");
        let tr: Arc<dyn Tracker> = Arc::new(rhapsody_tracker::fake::Fake::new());
        let mut eff = empty_effective(tr);
        eff.workspace = Arc::clone(&wm);
        o.eff = Some(eff);
        (o, wm, root)
    }

    // Mirrors Go `TestWorktreeInUse_ReflectsLiveRunning`: the liveness check sees o.running as it is
    // AT CALL TIME. A worktree registered after a hypothetical snapshot is reported in-use; an
    // unrelated path is not.
    #[test]
    fn worktree_in_use_reflects_live_running() {
        let (mut o, mgr, _root) = orch_for_gc();
        let live = mgr.path_for("", "LIVE-1");
        assert!(
            !o.worktree_in_use(Some(mgr.as_ref()), &live),
            "path must not be in use before any worker registers it"
        );

        // A worker adopts the worktree AFTER the snapshot would have been taken.
        o.running.insert(
            "1".to_string(),
            running_entry(issue("1", "LIVE-1", "In Progress"), "", ""),
        );
        assert!(
            o.worktree_in_use(Some(mgr.as_ref()), &live),
            "path of a now-running issue must be reported in use"
        );
        assert!(
            !o.worktree_in_use(Some(mgr.as_ref()), &mgr.path_for("", "OTHER-9")),
            "unrelated path must not be reported in use"
        );
    }

    // Mirrors Go `TestWorktreeInUse_UsesPlanMgrNotCurrentEff`: liveness paths are computed with the
    // GC plan's manager, not the (possibly reloaded) o.eff.workspace. Swapping o.eff.workspace for a
    // differently-rooted manager must not change the answer when asked with the OLD plan mgr.
    #[test]
    fn worktree_in_use_uses_plan_mgr_not_current_eff() {
        let (mut o, plan_mgr, _root) = orch_for_gc();
        o.running.insert(
            "1".to_string(),
            running_entry(issue("1", "RUN-1", "In Progress"), "", ""),
        );
        let path_under_old_root = plan_mgr.path_for("", "RUN-1");

        // Simulate a mid-prune workspace.root reload: o.eff.workspace now points at a new root.
        let new_root = TempDir::new();
        let new_mgr = Arc::new(
            Manager::new(workspace::Config {
                root: new_root.path.clone(),
                hooks: HookScripts::default(),
                hook_timeout: Duration::from_secs(1),
            })
            .expect("workspace manager"),
        );
        o.eff.as_mut().unwrap().workspace = Arc::clone(&new_mgr);

        // Asked with the PLAN mgr, the live worktree under the old root is recognized.
        assert!(
            o.worktree_in_use(Some(plan_mgr.as_ref()), &path_under_old_root),
            "plan-mgr path of a running issue must be reported in use after a root reload"
        );
        // Using the CURRENT (new) mgr would compute a different path and MISS it — the skew bug.
        assert!(
            !o.worktree_in_use(Some(new_mgr.as_ref()), &path_under_old_root),
            "new-mgr computation must not match the old-root path"
        );
    }

    // Verifies [`Orchestrator::build_workspace_gc_plan`]: the keep set is every running issue's
    // worktree path, and a plan with no effective config yields no manager and an empty keep set.
    // Go exercises this indirectly through `PruneStaleWorkspaces` (the off-loop driver, O7); this
    // pins the plan-builder contract the round-trip consumes.
    #[test]
    fn build_workspace_gc_plan_collects_running_worktrees() {
        let (mut o, mgr, _root) = orch_for_gc();
        o.running.insert(
            "1".to_string(),
            running_entry(issue("1", "RUN-1", "In Progress"), "", ""),
        );
        o.running.insert(
            "2".to_string(),
            running_entry(issue("2", "RUN-2", "In Progress"), "", ""),
        );
        let plan = o.build_workspace_gc_plan();
        assert!(plan.mgr.is_some());
        assert_eq!(plan.keep.len(), 2);
        assert!(plan.keep.contains(&mgr.path_for("", "RUN-1")));
        assert!(plan.keep.contains(&mgr.path_for("", "RUN-2")));

        // No effective config yet ⇒ no manager, empty keep set (the retention no-op precondition).
        let empty = Orchestrator::new("WORKFLOW.md");
        let plan = empty.build_workspace_gc_plan();
        assert!(plan.mgr.is_none());
        assert!(plan.keep.is_empty());
    }
}
