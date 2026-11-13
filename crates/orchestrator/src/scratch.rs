//! scratch — the per-run scratch-directory janitor and disk-pressure guard (STUDIO-1993).
//! Rhapsody-only; the frozen Go reference has no counterpart.
//!
//! Each dispatched run gets a daemon-owned scratch directory (`<scratch.root>/<run-id>/`, see
//! `rhapsody_agent::scratch`). This module reclaims them:
//!
//! * [`sweep_managed`] removes a managed-root child whose run id is no longer live and whose mtime is
//!   older than `scratch.ttl_hours`;
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

/// The race-free inputs the off-loop sweep needs, snapshotted on the control task: the policy, the
/// run ids of every currently-live run, and the scratch paths of those runs (so the legacy sweep can
/// recognize a directory a live run still sits under).
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

/// Free GiB on the volume containing `path`, via `statvfs`. `None` when the path cannot be statted
/// (a missing root, an unreadable mount) — the caller then does not hold dispatch, since a guard that
/// holds on an unknown reading would strand every run.
pub fn free_gib(path: &Path) -> Option<f64> {
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

/// Removes managed-root children whose run id is not live and whose mtime is older than `ttl_hours`.
/// `root` is scanned for direct children only; a child that does not parse as a run id is left alone.
/// `ttl_hours <= 0` disables the managed sweep.
pub fn sweep_managed(
    root: &str,
    ttl_hours: i64,
    now: SystemTime,
    live_run_ids: &HashSet<i64>,
) -> Vec<Deleted> {
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
        if remove(&path) {
            out.push(Deleted { path, bytes });
        }
    }
    out
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

/// Runs the whole sweep against a plan, at `now`. Pure over the filesystem, so the off-loop driver
/// and the tests share one code path.
pub fn run_sweep(plan: &ScratchPlan, now: SystemTime) -> SweepReport {
    SweepReport {
        managed: sweep_managed(
            &plan.policy.root,
            plan.policy.ttl_hours,
            now,
            &plan.live_run_ids,
        ),
        legacy: sweep_legacy(
            &legacy_root(),
            plan.policy.legacy_ttl_hours,
            now,
            &plan.live_paths,
        ),
    }
}

/// Runs [`run_sweep`] and logs every removal with its size. The off-loop driver and the pressure
/// path call this so the logging lives in one place.
pub fn run_sweep_and_log(plan: &ScratchPlan, now: SystemTime) -> SweepReport {
    let report = run_sweep(plan, now);
    for deleted in report.managed.iter().chain(report.legacy.iter()) {
        tracing::info!(
            path = %deleted.path.display(),
            bytes = deleted.bytes,
            "scratch: removed a stale scratch directory"
        );
    }
    report
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
}

impl ControlHandle {
    /// The scratch-sweep plan via a control round-trip. `None` when the loop is gone.
    pub async fn scratch_sweep_plan(&self) -> Option<ScratchPlan> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.events.send(Event::ScratchSweep { reply: tx }).ok()?;
        rx.await.ok()
    }

    /// Runs the janitor OFF the control loop: snapshots the plan, then sweeps and logs every removal.
    /// Backs the daemon's boot/hourly scratch scheduler; a gone loop is a no-op.
    pub async fn sweep_scratch(&self) -> SweepReport {
        let Some(plan) = self.scratch_sweep_plan().await else {
            return SweepReport::default();
        };
        run_sweep_and_log(&plan, SystemTime::now())
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
        assert!(root.join("notes").is_dir(), "a non-run-id child is left alone");
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
        assert!(root.join("live-run").is_dir(), "a live run's path is protected");
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
        assert!(dispatch_hold(&policy, 49.9).is_some(), "under the floor holds");
        assert!(dispatch_hold(&policy, 50.0).is_none(), "at the floor releases");
        assert!(dispatch_hold(&policy, 200.0).is_none(), "above releases");
        let reason = dispatch_hold(&policy, 12.34).unwrap();
        assert!(reason.contains("12.3 GiB"), "the reason names the reading");
        assert!(reason.contains("50 GiB floor"), "the reason names the floor");
    }

    #[test]
    fn free_gib_reads_a_real_volume() {
        let free = free_gib(Path::new("/")).expect("statvfs /");
        assert!(free > 0.0, "free space on / must be positive: {free}");
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
