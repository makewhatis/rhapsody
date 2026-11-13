//! scratch — the per-run scratch-directory janitor and disk-pressure guard (STUDIO-1993).
//! Rhapsody-only; the frozen Go reference has no counterpart.
//!
//! Each dispatched run gets a daemon-owned scratch directory (`<scratch.root>/<run-id>/`, see
//! `rhapsody_agent::scratch`). This module reclaims them:
//!
//! * [`managed_candidates`] selects managed-root children whose run id is no longer live and whose
//!   mtime is older than `scratch.ttl_hours`; the off-loop driver
//!   ([`ControlHandle::sweep_scratch`]) re-checks each against CURRENT ownership and then removes it,
//!   so a tree a resume adopted (renamed) after the plan snapshot is spared;
//! * [`sweep_legacy`] removes a child of the historical `$TMPDIR/opencode` root that no live run's
//!   path sits under, older than `scratch.legacy_ttl_hours` — the paths agents chose themselves
//!   before the managed root existed (the 180 GiB this ticket is about);
//! * [`dispatch_hold`] is the disk-pressure guard: hold new dispatches while the scratch volume has
//!   less than `scratch.min_free_gib` free.
//!
//! The janitor never touches anything outside those two roots: the managed root is scanned only for
//! direct children that parse as a run id, and the legacy root only for its own direct children.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::control_loop::Event;
use crate::orchestrator::Orchestrator;
use crate::stop::ControlHandle;

/// How long a due retry is parked for when the disk-pressure guard holds dispatch (STUDIO-1993
/// review B1), in milliseconds. Mirrors [`crate::drain::DRAIN_REQUEUE_DELAY_MS`]: the entry keeps
/// its claim and attempt number, and only the timer is re-armed, so the gate is re-checked until
/// free space recovers.
pub const SCRATCH_REQUEUE_DELAY_MS: i64 = 5_000;

/// The scratch policy the janitor and guard read, lowered from the config block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScratchPolicy {
    pub root: String,
    pub ttl_hours: i64,
    pub legacy_ttl_hours: i64,
    pub min_free_gib: u64,
}

impl Default for ScratchPolicy {
    fn default() -> Self {
        Self {
            root: String::new(),
            ttl_hours: 24,
            legacy_ttl_hours: 48,
            min_free_gib: 50,
        }
    }
}

impl ScratchPolicy {
    /// Lowers the decoded `scratch:` block into the policy the janitor and guard read.
    pub fn from_config(cfg: &rhapsody_config::Scratch) -> Self {
        Self {
            root: cfg.root.clone(),
            ttl_hours: cfg.ttl_hours,
            legacy_ttl_hours: cfg.legacy_ttl_hours,
            min_free_gib: cfg.min_free_gib,
        }
    }
}

/// One directory the janitor removed, with its measured size for the log line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deleted {
    pub path: PathBuf,
    pub bytes: u64,
}

/// What a sweep removed, split by root.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SweepReport {
    pub managed: Vec<Deleted>,
    pub legacy: Vec<Deleted>,
}

impl SweepReport {
    pub fn total(&self) -> usize {
        self.managed.len() + self.legacy.len()
    }
}

/// The inputs the off-loop sweep snapshots on the control task: the policy, the run ids of every
/// currently-live run, and the scratch paths of those runs (so the legacy sweep can recognize a
/// directory a live run still sits under).
///
/// `live_run_ids` is a cheap FIRST-PASS filter, NOT the authoritative protection: a resume can adopt
/// (rename) a tree to a live run id AFTER this snapshot, so the managed driver re-checks each
/// candidate against CURRENT ownership before removing it ([`ControlHandle::sweep_scratch`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScratchPlan {
    pub policy: ScratchPolicy,
    pub live_run_ids: HashSet<i64>,
    pub live_paths: Vec<String>,
}

/// The historical per-agent scratch root agents chose themselves: `$TMPDIR/opencode`.
pub fn legacy_root() -> PathBuf {
    std::env::temp_dir().join("opencode")
}

/// The managed scratch root's usage: its direct-child count and total bytes. Best-effort.
pub fn usage(root: &str) -> (u64, u64) {
    if root.is_empty() {
        return (0, 0);
    }
    let Ok(entries) = std::fs::read_dir(root) else {
        return (0, 0);
    };
    let mut dirs = 0u64;
    let mut bytes = 0u64;
    for entry in entries.flatten() {
        if entry.path().is_dir() {
            dirs += 1;
        }
        bytes += dir_size(&entry.path());
    }
    (dirs, bytes)
}

/// The dashboard's scratch view: the managed root's usage and the volume's free space, plus whether
/// the disk-pressure guard is currently holding dispatch. Rhapsody-only.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ScratchStatus {
    pub root: String,
    /// Direct children of the managed root (one per run that still has a scratch directory).
    pub dirs: u64,
    pub used_bytes: u64,
    /// Free GiB on the scratch volume; `None` when it could not be read.
    pub free_gib: Option<f64>,
    pub min_free_gib: u64,
    /// Whether free space is currently below the floor (dispatch is held).
    pub held: bool,
}

/// Free GiB on the volume containing `path`, via `statvfs`.
///
/// The managed root is created lazily by the first dispatch, so on a fresh install (or right after
/// an operator configures a new root) it does not exist yet. `statvfs` needs an existing path, so
/// when `path` itself cannot be statted we walk up to its nearest existing ancestor and measure the
/// volume the root WILL live on. Without this, a missing root read as "unknown" and the guard
/// admitted work regardless of the free space on the volume it was about to fill (STUDIO-1993
/// review B2). `None` only when no ancestor can be statted (an unreadable mount) — the caller then
/// does not hold dispatch, since a guard that holds on an unknown reading would strand every run.
pub fn free_gib(path: &Path) -> Option<f64> {
    let mut p = path;
    loop {
        if let Some(free) = statvfs_free_gib(p) {
            return Some(free);
        }
        p = p.parent()?;
    }
}

/// Free GiB on exactly `path`, or `None` when `statvfs` fails (a missing or unreadable path).
fn statvfs_free_gib(path: &Path) -> Option<f64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `statvfs` writes into a fully-initialized zeroed struct and reports failure via its
    // return code, which we check before reading any field.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    let avail = st.f_bavail as u128 * st.f_frsize as u128;
    Some(avail as f64 / (1024.0 * 1024.0 * 1024.0))
}

/// The disk-pressure guard: a plain reason to hold new dispatches while free space is below the
/// configured floor, or `None` when dispatch may proceed.
pub fn dispatch_hold(policy: &ScratchPolicy, free_gib: f64) -> Option<String> {
    if free_gib < policy.min_free_gib as f64 {
        Some(format!(
            "disk pressure: {free_gib:.1} GiB free on the scratch volume, below the {} GiB floor",
            policy.min_free_gib
        ))
    } else {
        None
    }
}

/// Whether `modified` is older than `ttl`. An mtime in the future (a clock skew) or an unreadable
/// time is treated as NOT old, so the janitor never removes a directory it cannot date.
fn older_than(now: SystemTime, modified: SystemTime, ttl: Duration) -> bool {
    matches!(now.duration_since(modified), Ok(age) if age > ttl)
}

/// The total size in bytes of the tree at `path`, best-effort (unreadable entries count as 0).
fn dir_size(path: &Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    if meta.is_file() {
        return meta.len();
    }
    if !meta.is_dir() {
        return 0;
    }
    let mut total = 0;
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            total += dir_size(&entry.path());
        }
    }
    total
}

/// Removes a directory, best-effort, logging nothing (the caller logs the aggregate). Returns
/// `false` on failure so the report never claims a removal that did not happen.
fn remove(path: &Path) -> bool {
    match std::fs::remove_dir_all(path) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        Err(e) => {
            tracing::warn!(
                path = %path.display(), err = %e,
                "scratch: could not remove a stale scratch directory"
            );
            false
        }
    }
}

/// One managed-root child the TTL/liveness screen selected for removal, with its measured size.
/// A CANDIDATE, not a decision: the off-loop driver re-checks each against CURRENT ownership
/// immediately before removing it, so a tree that became live after the plan snapshot is spared
/// (STUDIO-1993 review B3). See [`ControlHandle::sweep_scratch`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedCandidate {
    pub path: PathBuf,
    pub run_id: i64,
    pub bytes: u64,
}

/// Enumerates managed-root children whose run id is not in `live_run_ids` and whose mtime is older
/// than `ttl_hours`, WITHOUT removing them. `root` is scanned for direct children only; a child that
/// does not parse as a run id is left alone. `ttl_hours <= 0` disables the sweep.
pub fn managed_candidates(
    root: &str,
    ttl_hours: i64,
    now: SystemTime,
    live_run_ids: &HashSet<i64>,
) -> Vec<ManagedCandidate> {
    if root.is_empty() || ttl_hours <= 0 {
        return Vec::new();
    }
    let ttl = Duration::from_secs(ttl_hours as u64 * 3600);
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let Some(run_id) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<i64>().ok())
        else {
            continue;
        };
        if live_run_ids.contains(&run_id) {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(modified) = meta.modified() else {
            continue;
        };
        if !older_than(now, modified, ttl) {
            continue;
        }
        let path = entry.path();
        let bytes = dir_size(&path);
        out.push(ManagedCandidate {
            path,
            run_id,
            bytes,
        });
    }
    out
}

/// Removes managed-root children whose run id is not live and whose mtime is older than `ttl_hours`.
/// Pure snapshot semantics — the caller must have re-validated ownership against the CURRENT live
/// set, which the production driver ([`ControlHandle::sweep_scratch`]) does per candidate. Kept as
/// the pure building block the unit tests exercise; `ttl_hours <= 0` disables the managed sweep.
pub fn sweep_managed(
    root: &str,
    ttl_hours: i64,
    now: SystemTime,
    live_run_ids: &HashSet<i64>,
) -> Vec<Deleted> {
    managed_candidates(root, ttl_hours, now, live_run_ids)
        .into_iter()
        .filter_map(|c| {
            remove(&c.path).then_some(Deleted {
                path: c.path,
                bytes: c.bytes,
            })
        })
        .collect()
}

/// Removes legacy-root children older than `ttl_hours`, unless a live run's scratch path sits under
/// them. `root` is scanned for direct children only. `ttl_hours <= 0` disables the legacy sweep.
pub fn sweep_legacy(
    root: &Path,
    ttl_hours: i64,
    now: SystemTime,
    live_paths: &[String],
) -> Vec<Deleted> {
    if ttl_hours <= 0 {
        return Vec::new();
    }
    let ttl = Duration::from_secs(ttl_hours as u64 * 3600);
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        // Never touch a directory a live run's own path lives under.
        if live_paths
            .iter()
            .any(|live| Path::new(live).starts_with(&path))
        {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(modified) = meta.modified() else {
            continue;
        };
        if !older_than(now, modified, ttl) {
            continue;
        }
        let bytes = dir_size(&path);
        if remove(&path) {
            out.push(Deleted { path, bytes });
        }
    }
    out
}

/// Applies the managed deletions, re-checking CURRENT ownership per candidate via `live` (which
/// round-trips the control loop in production) immediately before each removal, and logging every
/// removal with its size. Extracted from the driver so the stale-plan/adoption interleaving is
/// testable without a live control loop (STUDIO-1993 review B3).
pub async fn apply_managed_deletions<F, Fut>(
    candidates: Vec<ManagedCandidate>,
    live: F,
) -> Vec<Deleted>
where
    F: Fn(i64) -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let mut deleted = Vec::new();
    for candidate in candidates {
        if live(candidate.run_id).await {
            continue;
        }
        let path = candidate.path.clone();
        let removed = tokio::task::spawn_blocking(move || remove(&path))
            .await
            .unwrap_or(false);
        if removed {
            tracing::info!(
                path = %candidate.path.display(),
                bytes = candidate.bytes,
                "scratch: removed a stale scratch directory"
            );
            deleted.push(Deleted {
                path: candidate.path,
                bytes: candidate.bytes,
            });
        }
    }
    deleted
}

impl Orchestrator {
    /// The scratch policy from the effective config (defaults before the first reload).
    pub fn scratch_policy(&self) -> ScratchPolicy {
        self.eff
            .as_ref()
            .map(|e| ScratchPolicy::from_config(&e.cfg.scratch))
            .unwrap_or_default()
    }

    /// Builds the race-free sweep plan. MUST run on the control task: it reads `self.eff` and
    /// `self.running`, both control-task-owned.
    pub fn build_scratch_plan(&self) -> ScratchPlan {
        let policy = self.scratch_policy();
        let mut live_run_ids = HashSet::new();
        let mut live_paths = Vec::new();
        for re in self.running.values() {
            if re.run_id > 0 {
                live_run_ids.insert(re.run_id);
                if !policy.root.is_empty() {
                    live_paths.push(
                        Path::new(&policy.root)
                            .join(re.run_id.to_string())
                            .to_string_lossy()
                            .into_owned(),
                    );
                }
            }
        }
        ScratchPlan {
            policy,
            live_run_ids,
            live_paths,
        }
    }

    /// The plain reason dispatch is held for disk pressure, or `None` when dispatch may proceed or
    /// the scratch root is unconfigured/unreadable. MUST run on the control task.
    pub fn scratch_dispatch_hold(&self) -> Option<String> {
        let policy = self.scratch_policy();
        if policy.root.is_empty() {
            return None;
        }
        let free = free_gib(Path::new(&policy.root))?;
        dispatch_hold(&policy, free)
    }

    /// Whether `run_id` owns a live run RIGHT NOW. MUST run on the control task: it reads the live
    /// `self.running`. The authoritative, snapshot-free liveness re-check the off-loop janitor
    /// consults immediately before each managed-root removal — the same TOCTOU guard the workspace
    /// GC's `worktree_in_use` provides (STUDIO-1993 review B3). A resume ADOPTS a retained tree by
    /// renaming it to the resumed run's id; because the run is registered in `self.running` before
    /// the agent renames the tree, a directory the enumeration saw on disk is guaranteed live here
    /// whenever its run is live — so a stale plan can never reclaim a resumed tree.
    pub fn scratch_run_in_use(&self, run_id: i64) -> bool {
        run_id > 0 && self.running.values().any(|re| re.run_id == run_id)
    }

    /// The reconciliation feed's row for a disk-pressure hold (STUDIO-1993 change item 4): the plain
    /// reason dispatch is held, so it surfaces in `/api/v1/state`'s `review_divergence` instead of
    /// only the log. Empty while dispatch is not held, which keeps the wire payload — and the golden
    /// — unchanged on a healthy daemon.
    pub(crate) fn scratch_human_feed(&self) -> Vec<crate::reviewreconcile::Divergence> {
        self.scratch_hold
            .as_ref()
            .map(|reason| {
                vec![crate::reviewreconcile::Divergence {
                    pr: "scratch volume".to_string(),
                    kind: crate::reviewreconcile::DivergenceKind::ScratchPressure,
                    ticket: String::new(),
                    reviewer: String::new(),
                    stale_secs: 0,
                    auto_merge_reason: None,
                    capacity_held: None,
                    capacity_unreadable: None,
                    adjudicated_head: String::new(),
                    current_head: String::new(),
                    rounds: 0,
                    findings: Vec::new(),
                    reason: reason.clone(),
                }]
            })
            .unwrap_or_default()
    }
}

impl ControlHandle {
    /// The scratch-sweep plan via a control round-trip. `None` when the loop is gone.
    pub async fn scratch_sweep_plan(&self) -> Option<ScratchPlan> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.events.send(Event::ScratchSweep { reply: tx }).ok()?;
        rx.await.ok()
    }

    /// Whether `run_id` owns a live run RIGHT NOW, via a control round-trip. `false` when the loop
    /// is gone (a removal-under-doubt is the safe default). Backs the janitor's pre-removal
    /// ownership re-check.
    pub async fn scratch_run_in_use(&self, run_id: i64) -> bool {
        let (tx, rx) = tokio::sync::oneshot::channel();
        if self
            .events
            .send(Event::ScratchRunInUse { run_id, reply: tx })
            .is_err()
        {
            return false;
        }
        rx.await.unwrap_or(false)
    }

    /// Runs the janitor OFF the control loop: snapshots the plan, enumerates the stale managed-root
    /// children, re-checks each against CURRENT ownership immediately before removing it, then
    /// sweeps the legacy root. Backs the daemon's boot/hourly scheduler AND the disk-pressure path;
    /// a gone loop is a no-op.
    ///
    /// The per-candidate re-check is the fix for STUDIO-1993 review B3: a plan captured before a
    /// resume adopted (renamed) a retained tree would otherwise delete that now-live tree, because
    /// the plan's live set predates the adoption. Coordination is cheap — one round-trip per
    /// candidate, and only candidates already past the TTL are checked at all.
    pub async fn sweep_scratch(&self) -> SweepReport {
        let Some(plan) = self.scratch_sweep_plan().await else {
            return SweepReport::default();
        };
        let now = SystemTime::now();
        let root = plan.policy.root.clone();
        let ttl_hours = plan.policy.ttl_hours;
        let live_run_ids = plan.live_run_ids.clone();
        // The walk is the expensive half; keep it off the control loop.
        let candidates = tokio::task::spawn_blocking(move || {
            managed_candidates(&root, ttl_hours, now, &live_run_ids)
        })
        .await
        .unwrap_or_default();
        let managed =
            apply_managed_deletions(candidates, |run_id| self.scratch_run_in_use(run_id)).await;
        let mut report = SweepReport {
            managed,
            legacy: Vec::new(),
        };
        // The legacy root is never renamed by resume adoption, and a live run's legacy directory is
        // created fresh at run start, so its snapshot live-path protection cannot go stale the way
        // the managed one can; sweep it with the plan's snapshot in one blocking pass.
        let legacy_ttl_hours = plan.policy.legacy_ttl_hours;
        let live_paths = plan.live_paths.clone();
        let legacy = tokio::task::spawn_blocking(move || {
            sweep_legacy(&legacy_root(), legacy_ttl_hours, now, &live_paths)
        })
        .await
        .unwrap_or_default();
        for deleted in &legacy {
            tracing::info!(
                path = %deleted.path.display(),
                bytes = deleted.bytes,
                "scratch: removed a stale scratch directory"
            );
        }
        report.legacy = legacy;
        report
    }

    /// The dashboard's scratch view: the managed root's usage and the volume's free space, read off
    /// the control loop. Backs the additive `GET /api/v1/system` endpoint.
    pub async fn scratch_status(&self) -> Option<ScratchStatus> {
        let plan = self.scratch_sweep_plan().await?;
        let root = plan.policy.root.clone();
        let min_free_gib = plan.policy.min_free_gib;
        tokio::task::spawn_blocking(move || {
            if root.is_empty() {
                return ScratchStatus::default();
            }
            let (dirs, used_bytes) = usage(&root);
            let free = free_gib(Path::new(&root));
            let held = free.is_some_and(|f| f < min_free_gib as f64);
            ScratchStatus {
                root,
                dirs,
                used_bytes,
                free_gib: free,
                min_free_gib,
                held,
            }
        })
        .await
        .ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_dir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "rhapsody-orch-scratch-{}-{tag}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).expect("mkdir");
        p
    }

    fn make_dir(path: &Path, age_secs: u64) {
        fs::create_dir_all(path).expect("mkdir child");
        fs::write(path.join("blob"), vec![0u8; 64]).expect("write");
        let when = SystemTime::now() - Duration::from_secs(age_secs);
        let f = fs::File::open(path).expect("open dir");
        f.set_modified(when).expect("set mtime");
    }

    // STUDIO-1993 acceptance: the janitor deletes only stale directories under the managed root,
    // never a live run's, and never a child that is not a run id.
    #[test]
    fn sweep_managed_deletes_only_stale_non_live_run_dirs() {
        let root = temp_dir("managed");
        make_dir(&root.join("1"), 100 * 3600); // stale, not live => deleted
        make_dir(&root.join("2"), 100 * 3600); // stale but LIVE => kept
        make_dir(&root.join("3"), 60); // fresh => kept
        make_dir(&root.join("notes"), 100 * 3600); // not a run id => kept
        let now = SystemTime::now();
        let live: HashSet<i64> = [2].into_iter().collect();
        let deleted = sweep_managed(&root.to_string_lossy(), 24, now, &live);
        let names: Vec<String> = deleted
            .iter()
            .map(|d| d.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["1".to_string()], "only the stale non-live run");
        assert!(deleted[0].bytes >= 64, "the removal records its size");
        assert!(!root.join("1").exists());
        assert!(root.join("2").is_dir(), "a live run's dir is never touched");
        assert!(root.join("3").is_dir(), "a fresh dir is kept");
        assert!(
            root.join("notes").is_dir(),
            "a non-run-id child is left alone"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn sweep_managed_is_disabled_by_a_zero_ttl_or_empty_root() {
        let root = temp_dir("off");
        make_dir(&root.join("1"), 100 * 3600);
        let live = HashSet::new();
        assert!(sweep_managed(&root.to_string_lossy(), 0, SystemTime::now(), &live).is_empty());
        assert!(sweep_managed("", 24, SystemTime::now(), &live).is_empty());
        assert!(root.join("1").is_dir());
        let _ = fs::remove_dir_all(&root);
    }

    // STUDIO-1993 acceptance: the legacy sweep deletes only stale children of the legacy root, and
    // never one a live run's path sits under.
    #[test]
    fn sweep_legacy_deletes_stale_children_but_never_a_live_path() {
        let root = temp_dir("legacy");
        make_dir(&root.join("carini-181-jimmy-target"), 100 * 3600); // stale => deleted
        make_dir(&root.join("live-run"), 100 * 3600); // stale but a live path under it => kept
        make_dir(&root.join("fresh"), 60); // fresh => kept
        let live_paths = vec![root.join("live-run/sub").to_string_lossy().into_owned()];
        let deleted = sweep_legacy(&root, 48, SystemTime::now(), &live_paths);
        assert_eq!(deleted.len(), 1);
        assert!(deleted[0].path.ends_with("carini-181-jimmy-target"));
        assert!(!root.join("carini-181-jimmy-target").exists());
        assert!(
            root.join("live-run").is_dir(),
            "a live run's path is protected"
        );
        assert!(root.join("fresh").is_dir());
        let _ = fs::remove_dir_all(&root);
    }

    // STUDIO-1993 acceptance: the guard holds dispatch under the threshold and releases above it.
    #[test]
    fn the_guard_holds_under_the_floor_and_releases_above_it() {
        let policy = ScratchPolicy {
            min_free_gib: 50,
            ..Default::default()
        };
        assert!(
            dispatch_hold(&policy, 49.9).is_some(),
            "under the floor holds"
        );
        assert!(
            dispatch_hold(&policy, 50.0).is_none(),
            "at the floor releases"
        );
        assert!(dispatch_hold(&policy, 200.0).is_none(), "above releases");
        let reason = dispatch_hold(&policy, 12.34).unwrap();
        assert!(reason.contains("12.3 GiB"), "the reason names the reading");
        assert!(
            reason.contains("50 GiB floor"),
            "the reason names the floor"
        );
    }

    #[test]
    fn free_gib_reads_a_real_volume() {
        let free = free_gib(Path::new("/")).expect("statvfs /");
        assert!(free > 0.0, "free space on / must be positive: {free}");
    }

    // STUDIO-1993 review B2: an absent managed root must still measure the volume it WILL live on,
    // so the guard cannot be bypassed on a fresh install (or right after a new root is configured)
    // just because the final directory does not exist yet.
    #[test]
    fn free_gib_measures_the_containing_volume_when_the_root_is_absent() {
        let parent = temp_dir("absent-parent");
        let absent = parent.join("not-created-yet");
        assert!(!absent.exists(), "the child must not exist for this test");
        let parent_free = free_gib(&parent).expect("the parent volume is readable");
        let absent_free =
            free_gib(&absent).expect("an absent child measures its containing volume");
        assert!(
            (parent_free - absent_free).abs() < 0.5,
            "the absent root must read the SAME volume as its parent: {parent_free} vs {absent_free}"
        );
        let _ = fs::remove_dir_all(&parent);
    }

    // STUDIO-1993 review B2: the first tick on a fresh install (an absent managed root under an
    // accessible parent) must still be held by the floor, rather than dispatching because
    // `statvfs(root)` failed and the guard read "unknown".
    #[tokio::test(flavor = "multi_thread")]
    async fn an_absent_scratch_root_still_holds_dispatch_under_the_floor() {
        use std::sync::Arc;

        use rhapsody_tracker::Tracker;
        use rhapsody_tracker::fake::Fake;

        use crate::orchestrator::Orchestrator;
        use crate::testsupport::{empty_effective, issue};

        let parent = temp_dir("absent-guard");
        let root = parent.join("not-created-yet");
        assert!(!root.exists());
        let mut tr = Fake::new();
        tr.candidates = vec![issue("1", "MT-1", "Todo")];
        let tr = Arc::new(tr);
        let mut eff = empty_effective(Arc::clone(&tr) as Arc<dyn Tracker>);
        eff.active_states = ["todo".to_string()].into_iter().collect();
        eff.terminal_states = ["done".to_string()].into_iter().collect();
        eff.max_concurrent = 10;
        eff.poll_interval = Duration::from_secs(3600);
        eff.cfg.scratch.root = root.to_string_lossy().into_owned();
        eff.cfg.scratch.min_free_gib = u64::MAX; // always under the floor
        eff.cfg.scratch.legacy_ttl_hours = 0;
        let mut o = Orchestrator::new("WORKFLOW.md");
        o.eff = Some(eff);
        o.spawn = Some(Box::new(|_, _, _| {}));
        o.on_tick().await;
        if let Some(t) = o.tick_timer.take() {
            t.abort();
        }
        assert!(
            o.running.is_empty(),
            "an absent scratch root must not bypass the disk floor"
        );
        assert!(
            !root.join("1").exists(),
            "the guard runs before dispatch, so no scratch dir is created"
        );
        let _ = fs::remove_dir_all(&parent);
    }

    // STUDIO-1993 review B3: a stale sweep plan (captured before a resume) must not delete a tree the
    // resume ADOPTED — renamed to a newly live run id — even though the plan's live set predates it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_stale_plan_spares_a_tree_adopted_by_a_newly_live_run() {
        use crate::orchestrator::Orchestrator;
        use crate::testsupport::{issue, running_entry};

        let root = temp_dir("adopt-race");
        make_dir(&root.join("11"), 100 * 3600);
        let now = SystemTime::now();
        // Capture the plan BEFORE the resume: nothing is live, so `11` is a deletion candidate.
        let stale = managed_candidates(&root.to_string_lossy(), 24, now, &HashSet::new());
        assert_eq!(
            stale.len(),
            1,
            "the aged tree is a candidate under the stale plan"
        );
        assert_eq!(stale[0].run_id, 11);

        // The resume adopts the retained tree: rename it to the resumed run's id and register the run.
        let adopted = rhapsody_agent::scratch::ScratchDir::adopt_renamed(
            &root.to_string_lossy(),
            12,
            &root.join("11"),
        )
        .expect("adopted");
        assert_eq!(
            adopted.path(),
            root.join("12"),
            "renamed to the live run id"
        );
        let mut o = Orchestrator::new("WORKFLOW.md");
        let mut re = running_entry(issue("1", "MT-1", "In Progress"), "", "");
        re.run_id = 12;
        o.running.insert("1".to_string(), re);

        // Enumerate against the STALE live set: the renamed tree now parses as run id 12, which the
        // stale plan does not know is live.
        let candidates = managed_candidates(&root.to_string_lossy(), 24, now, &HashSet::new());
        assert_eq!(candidates.len(), 1);
        assert_eq!(
            candidates[0].run_id, 12,
            "the stale plan sees the renamed tree as run 12"
        );

        // The coordinated sweep re-checks CURRENT ownership per candidate and spares the live tree.
        let deleted = apply_managed_deletions(candidates, |id| {
            std::future::ready(o.scratch_run_in_use(id))
        })
        .await;
        assert!(
            deleted.is_empty(),
            "a live resumed tree is never deleted: {deleted:?}"
        );
        assert!(root.join("12").is_dir(), "the resumed tree survives");
        drop(adopted);
        let _ = fs::remove_dir_all(&root);
    }

    // STUDIO-1993 change item 4 / review B4: the plain disk-pressure reason must reach the
    // reconciliation surface (the snapshot's `review_divergence`), and CLEAR when the hold releases.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_disk_pressure_reason_reaches_the_reconciliation_feed() {
        use std::sync::Arc;

        use rhapsody_tracker::Tracker;
        use rhapsody_tracker::fake::Fake;

        use crate::orchestrator::Orchestrator;
        use crate::reviewreconcile::DivergenceKind;
        use crate::testsupport::{empty_effective, issue};

        let root = temp_dir("feed");
        let mut tr = Fake::new();
        tr.candidates = vec![issue("1", "MT-1", "Todo")];
        let tr = Arc::new(tr);
        let mut eff = empty_effective(Arc::clone(&tr) as Arc<dyn Tracker>);
        eff.active_states = ["todo".to_string()].into_iter().collect();
        eff.terminal_states = ["done".to_string()].into_iter().collect();
        eff.max_concurrent = 10;
        eff.poll_interval = Duration::from_secs(3600);
        eff.cfg.scratch.root = root.to_string_lossy().into_owned();
        eff.cfg.scratch.legacy_ttl_hours = 0;
        let mut o = Orchestrator::new("WORKFLOW.md");
        o.eff = Some(eff);
        o.spawn = Some(Box::new(|_, _, _| {}));

        // Held: the reason is on the feed.
        o.eff.as_mut().expect("eff").cfg.scratch.min_free_gib = u64::MAX;
        o.on_tick().await;
        let held: Vec<_> = o
            .build_snapshot()
            .review_divergence
            .into_iter()
            .filter(|d| d.kind == DivergenceKind::ScratchPressure)
            .collect();
        assert_eq!(held.len(), 1, "the hold must be reported once");
        assert!(
            held[0].reason.contains("disk pressure"),
            "the feed carries the plain reason: {}",
            held[0].reason
        );

        // Released: the reason is gone from the feed.
        o.eff.as_mut().expect("eff").cfg.scratch.min_free_gib = 0;
        o.on_tick().await;
        if let Some(t) = o.tick_timer.take() {
            t.abort();
        }
        assert!(
            !o.build_snapshot()
                .review_divergence
                .iter()
                .any(|d| d.kind == DivergenceKind::ScratchPressure),
            "the hold must clear from the feed once free space recovers"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn older_than_treats_a_future_mtime_as_not_old() {
        let now = SystemTime::now();
        assert!(older_than(
            now,
            now - Duration::from_secs(3600),
            Duration::from_secs(60)
        ));
        assert!(!older_than(
            now,
            now + Duration::from_secs(3600),
            Duration::from_secs(60)
        ));
    }

    // STUDIO-1993 acceptance: a tick under the free-space floor holds dispatch entirely, and one at
    // or above it dispatches. The floor is driven to `u64::MAX` (always under) and `0` (always above)
    // so the test does not depend on the machine's real free space.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_tick_holds_dispatch_below_the_floor_and_releases_above_it() {
        use std::sync::Arc;

        use rhapsody_tracker::Tracker;
        use rhapsody_tracker::fake::Fake;

        use crate::orchestrator::Orchestrator;
        use crate::testsupport::{empty_effective, issue};

        let root = temp_dir("guard");
        for (min_free_gib, want_dispatched) in [(u64::MAX, false), (0u64, true)] {
            let mut tr = Fake::new();
            tr.candidates = vec![issue("1", "MT-1", "Todo")];
            let tr = Arc::new(tr);
            let mut eff = empty_effective(Arc::clone(&tr) as Arc<dyn Tracker>);
            eff.active_states = ["todo".to_string()].into_iter().collect();
            eff.terminal_states = ["done".to_string()].into_iter().collect();
            eff.max_concurrent = 10;
            eff.poll_interval = Duration::from_secs(3600);
            eff.cfg.scratch.root = root.to_string_lossy().into_owned();
            eff.cfg.scratch.min_free_gib = min_free_gib;
            // Keep the test hermetic: never let the pressure path sweep the real `$TMPDIR/opencode`.
            eff.cfg.scratch.legacy_ttl_hours = 0;
            let mut o = Orchestrator::new("WORKFLOW.md");
            o.eff = Some(eff);
            o.spawn = Some(Box::new(|_, _, _| {}));
            o.on_tick().await;
            if let Some(t) = o.tick_timer.take() {
                t.abort();
            }
            assert_eq!(
                o.running.contains_key("1"),
                want_dispatched,
                "min_free_gib={min_free_gib}: dispatch disposition"
            );
        }
        let _ = fs::remove_dir_all(&root);
    }
}
