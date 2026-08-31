//! reads — parity port of Go `internal/orchestrator/reads.go`.
//!
//! The read-only Linear surfaces (INF-224): the Settings "connected as" identity endpoint and the
//! add-agent projects picker. These are served OFF the control loop by the future P6 HTTP path, so
//! the account-level tracker + resolved key they read live behind [`Orchestrator::reads`]'s
//! `RwLock` (Go `readsMu`) rather than in the loop-confined scheduling state. The masked-token
//! boundary ([`mask_token`]) guarantees the raw secret never crosses the package boundary.
//!
//! Deviations from Go, all mechanical:
//!   * Go's `(Identity, error)` for `ConnectedViewer` becomes `(Identity, Option<TrackerError>)` —
//!     the identity is ALWAYS meaningful (best-effort, carrying the masked token even on a
//!     resolution failure), and the second value is the failure surfaced only for logging.
//!   * Go's `ErrConfigNotLoaded` sentinel becomes the [`ReadsError::ConfigNotLoaded`] variant; its
//!     `Display` reproduces the exact `"config_not_loaded"` string the HTTP layer maps to 503.
//!   * The methods take no `context.Context`: the Rust `Tracker` async methods carry no context
//!     (cancellation is task-abort), so the ctx parameter is dropped.

use std::sync::{Arc, PoisonError, RwLock};

use rhapsody_core::{Project, Viewer};
use rhapsody_tracker::{Tracker, TrackerError};

use crate::orchestrator::Orchestrator;
use crate::stop::ControlHandle;

/// The error surface of the read-only Linear endpoints. [`ReadsError::ConfigNotLoaded`] is the
/// sentinel returned before the daemon's first successful config load (no account tracker captured
/// yet); its `Display` is the exact `config_not_loaded` string the HTTP layer maps to 503. Mirrors
/// Go `ErrConfigNotLoaded` plus the bare tracker error `ListProjects` returns.
#[derive(Debug, thiserror::Error)]
pub enum ReadsError {
    /// No account tracker has been captured yet (before the first config load). Go `ErrConfigNotLoaded`.
    #[error("config_not_loaded")]
    ConfigNotLoaded,
    /// A tracker call (e.g. `ListProjects`) failed; surfaced verbatim for the HTTP layer.
    #[error(transparent)]
    Tracker(#[from] TrackerError),
}

/// The account-level tracker + resolved key backing the read-only Linear surfaces, guarded by
/// [`Orchestrator::reads`]. `tracker` is `None` before the first config load; `api_key` is kept
/// ONLY to render a masked indicator ([`mask_token`]) and is never returned raw. Mirrors the Go
/// `readsTracker`/`readsAPIKey` pair. No `Debug` (the trait object is not `Debug`, and the key
/// must never be logged).
#[derive(Default, Clone)]
pub struct ReadsTarget {
    pub tracker: Option<Arc<dyn Tracker>>,
    pub api_key: String,
    /// Every ENABLED project's slug-bound tracker, in poll order (STUDIO-671).
    ///
    /// `tracker` above is the top-level/legacy client, and in the `projects:` config form it is
    /// bound to `tracker.project_slug` — which validation deliberately allows to be EMPTY, because
    /// the `projects:` block supplies the slugs (`config::validate`'s `missing_tracker_project_slug`
    /// fires only when BOTH are absent). A candidate query through it filters
    /// `project.slugId == ""`, which Linear answers with zero rows and no error: the exact silent
    /// wedge STUDIO-671 reported. Anything that must see the daemon's WORK — as opposed to the
    /// account — reads these instead, the same clients the poll loop fans out over.
    pub project_trackers: Vec<Arc<dyn Tracker>>,
}

/// The resolved "connected as" account for the Settings identity endpoint (INF-224). `masked_token`
/// is ALWAYS redacted (e.g. `"lin_***…1234"`); the raw secret never leaves the orchestrator.
/// `connected` is false before the first config load, when no key is configured, or when viewer
/// resolution fails (the masked token still indicates a key is present). Mirrors Go `Identity`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Identity {
    pub connected: bool,
    pub viewer: Viewer,
    pub masked_token: String,
}

impl Orchestrator {
    /// Records the account-level tracker + resolved key for the read-only Linear endpoints. Called
    /// from the reload path (control task) on every (re)load; read under the lock by the HTTP tasks.
    /// Takes `&self` (the mutation is interior, behind the `RwLock`), mirroring Go's pointer-receiver
    /// `setReadsTarget` that locks internally. Mirrors Go `setReadsTarget`.
    pub fn set_reads_target(&self, tracker: Arc<dyn Tracker>, api_key: impl Into<String>) {
        let mut w = self.reads.write().unwrap_or_else(PoisonError::into_inner);
        w.tracker = Some(tracker);
        w.api_key = api_key.into();
    }

    /// Records every ENABLED project's slug-bound tracker for the off-loop readers that need the
    /// daemon's WORK rather than its account (STUDIO-671). Called from the reload path beside
    /// [`Orchestrator::set_reads_target`], on every (re)load, so a hot-reload that adds, removes or
    /// pauses a project is reflected without a restart.
    ///
    /// Additive rather than folded into `set_reads_target` because that one mirrors Go
    /// `setReadsTarget`, whose two fields back the account-level Settings surfaces; the Go daemon
    /// has no reader of this list at all. The two writes are separate, and that is harmless: no
    /// reader correlates the account tracker with the project list, so a cycle that lands between
    /// them sees one of the two a moment stale and nothing inconsistent.
    pub fn set_reads_projects(&self, trackers: Vec<Arc<dyn Tracker>>) {
        let mut w = self.reads.write().unwrap_or_else(PoisonError::into_inner);
        w.project_trackers = trackers;
    }

    /// Lists the workspace's Linear projects for the add-agent picker (INF-224), reusing the
    /// account-level tracker captured at load time. Returns [`ReadsError::ConfigNotLoaded`] before
    /// the first successful config load (the HTTP layer maps it to 503). Mirrors Go `ListLinearProjects`.
    pub async fn list_linear_projects(&self) -> Result<Vec<Project>, ReadsError> {
        list_linear_projects_from(&self.reads).await
    }

    /// Resolves the owner of the configured Linear key for the "connected as" identity endpoint
    /// (INF-224). Best-effort: a `None` tracker or empty key yields `{connected: false}` with no
    /// error; a resolution failure yields `{connected: false, masked_token: …}` WITH the error (for
    /// logging). The token is masked here so the raw secret never crosses the package boundary.
    /// Mirrors Go `ConnectedViewer` (whose `(Identity, error)` return becomes `(Identity, Option<_>)`).
    pub async fn connected_viewer(&self) -> (Identity, Option<TrackerError>) {
        connected_viewer_from(&self.reads).await
    }
}

impl ControlHandle {
    /// The daemon's off-loop `GET /api/v1/linear/projects` surface — the [`ControlHandle`] mirror of
    /// [`Orchestrator::list_linear_projects`], reading the SAME shared reads cell so a hot-reload is
    /// reflected. F1 (the assembly) wires this into the httpapi provider adapter.
    pub async fn list_linear_projects(&self) -> Result<Vec<Project>, ReadsError> {
        list_linear_projects_from(&self.reads).await
    }

    /// The daemon's off-loop `GET /api/v1/linear/identity` surface — the [`ControlHandle`] mirror of
    /// [`Orchestrator::connected_viewer`], reading the SAME shared reads cell. F1 wires it into the
    /// httpapi provider adapter.
    pub async fn connected_viewer(&self) -> (Identity, Option<TrackerError>) {
        connected_viewer_from(&self.reads).await
    }

    /// The account-level tracker captured by the most recent config load, or `None` before the
    /// first one (STUDIO-644). It reads the SAME shared reads cell as the surfaces above, so a
    /// hot-reload is reflected without the caller round-tripping the control channel.
    ///
    /// It exists for the off-loop Teams triage task, which is spawned at the composition root
    /// BEFORE the daemon's first reload and needs a tracker each cycle rather than once. Cloning
    /// the handle out releases the lock immediately — it is never held across the caller's `await`.
    pub fn reads_tracker(&self) -> Option<Arc<dyn Tracker>> {
        reads_snapshot(&self.reads).0
    }

    /// Every ENABLED project's slug-bound tracker, or `None` before the first config load
    /// (STUDIO-671). It reads the SAME shared reads cell as the surfaces above, so a hot-reload is
    /// reflected without the caller round-tripping the control channel.
    ///
    /// The `Option` and the `Vec` mean different things, and the off-loop Teams tasks report both:
    /// `None` is "no config has loaded yet" (the pre-load state [`Self::reads_tracker`] also
    /// reports), while `Some(vec![])` is "a config IS loaded and every project in it is paused" —
    /// a daemon that legitimately has no work to sweep. Collapsing the two would make a
    /// misconfiguration and a boot race look identical in the log, which is the class of silence
    /// STUDIO-671 was about.
    ///
    /// Cloning the handles out releases the lock immediately — it is never held across the
    /// caller's `await`.
    pub fn reads_project_trackers(&self) -> Option<Vec<Arc<dyn Tracker>>> {
        let r = self.reads.read().unwrap_or_else(PoisonError::into_inner);
        r.tracker.as_ref()?;
        Some(r.project_trackers.clone())
    }
}

/// Snapshots the current tracker handle + key from a shared reads cell (Go `readsTarget`). Clones the
/// handle out so the lock is released before the caller's async tracker round-trip — never held across
/// an `await`. Shared by [`Orchestrator`] and the daemon's [`ControlHandle`], which point at the same
/// [`Arc`]-shared cell.
fn reads_snapshot(reads: &RwLock<ReadsTarget>) -> (Option<Arc<dyn Tracker>>, String) {
    let r = reads.read().unwrap_or_else(PoisonError::into_inner);
    (r.tracker.clone(), r.api_key.clone())
}

/// Shared engine behind [`Orchestrator::list_linear_projects`] + [`ControlHandle::list_linear_projects`].
async fn list_linear_projects_from(
    reads: &RwLock<ReadsTarget>,
) -> Result<Vec<Project>, ReadsError> {
    let (tracker, _) = reads_snapshot(reads);
    match tracker {
        None => Err(ReadsError::ConfigNotLoaded),
        Some(tr) => Ok(tr.list_projects().await?),
    }
}

/// Shared engine behind [`Orchestrator::connected_viewer`] + [`ControlHandle::connected_viewer`].
async fn connected_viewer_from(reads: &RwLock<ReadsTarget>) -> (Identity, Option<TrackerError>) {
    let (tracker, key) = reads_snapshot(reads);
    let tr = match tracker {
        Some(tr) if !key.trim().is_empty() => tr,
        _ => return (Identity::default(), None),
    };
    match tr.resolve_viewer().await {
        Ok(viewer) => (
            Identity {
                connected: true,
                viewer,
                masked_token: mask_token(&key),
            },
            None,
        ),
        Err(e) => (
            Identity {
                connected: false,
                viewer: Viewer::default(),
                masked_token: mask_token(&key),
            },
            Some(e),
        ),
    }
}

/// Redacts a Linear API key for display: keeps a short head (the provider prefix) and the last 4
/// characters, e.g. `"lin_api_abcdef1234"` -> `"lin_***…1234"`. The head+tail are revealed ONLY when
/// the token is long enough (>=13) that at least 5 characters stay hidden between them; shorter (or
/// empty) values are fully redacted, so the function never exposes most of a secret. Real Linear
/// keys are ~40+ chars, so they always get the head+tail form. Mirrors Go `maskToken`.
///
/// Go slices bytes freely; here the head/tail slices are taken with a char-boundary guard so a
/// pathological multi-byte token can never panic — it falls back to full redaction. Real keys are
/// ASCII, so the observable output is byte-identical to Go for every realistic input.
pub(crate) fn mask_token(tok: &str) -> String {
    let t = tok.trim();
    if t.is_empty() {
        return String::new();
    }
    if t.len() < 13 {
        return "***".to_string();
    }
    match (t.get(..4), t.get(t.len() - 4..)) {
        (Some(head), Some(tail)) => format!("{head}***…{tail}"),
        _ => "***".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rhapsody_core::{Project, Viewer};
    use rhapsody_tracker::TrackerError;
    use rhapsody_tracker::fake::Fake;

    use super::*;

    fn reads_test_orch() -> Orchestrator {
        Orchestrator::new("WORKFLOW.md")
    }

    // Mirrors Go `TestMaskToken`: empty -> "", anything < 13 chars fully redacted, and a realistic
    // key reveals only a 4-char head + 4-char tail (never the raw value, never the middle).
    #[test]
    fn mask_token_boundary() {
        let cases = [
            ("", ""),
            ("   ", ""),
            ("short", "***"),
            ("abcdefgh", "***"),               // 8
            ("abcdefghijkl", "***"), // 12 — still fully redacted (would otherwise expose 8/12)
            ("abcdefghijklm", "abcd***…jklm"), // 13 — boundary: 5 chars hidden
            ("lin_api_0123456789abcdef", "lin_***…cdef"),
        ];
        for (input, want) in cases {
            let got = mask_token(input);
            assert_eq!(got, want, "mask_token({input:?})");
            // Hard invariant: never the raw secret, and revealed tokens redact the middle.
            let raw = input.trim();
            if !raw.is_empty() {
                assert_ne!(got, raw, "mask_token({input:?}) returned the raw secret");
            }
            if raw.len() >= 13 {
                assert!(
                    got.contains("***…"),
                    "mask_token({input:?}) = {got:?} must redact the middle"
                );
            }
        }
    }

    // Mirrors Go `TestConnectedViewerMatrix`: the four states of the connected-as resolution, with
    // the token always masked (never the raw key).
    #[tokio::test]
    async fn connected_viewer_no_tracker_before_first_load() {
        let o = reads_test_orch();
        let (id, err) = o.connected_viewer().await;
        assert!(err.is_none());
        assert!(!id.connected);
        assert_eq!(id.masked_token, "");
    }

    #[tokio::test]
    async fn connected_viewer_empty_key() {
        let o = reads_test_orch();
        o.set_reads_target(Arc::new(Fake::new()), "");
        let (id, err) = o.connected_viewer().await;
        assert!(err.is_none());
        assert!(!id.connected);
    }

    #[tokio::test]
    async fn connected_viewer_resolved() {
        const RAW_KEY: &str = "lin_api_secretvalue_9999";
        let viewer = Viewer {
            id: "v1".to_string(),
            name: "Jane Q".to_string(),
            display_name: "jane".to_string(),
            email: "jane@example.com".to_string(),
            ..Default::default()
        };
        let o = reads_test_orch();
        let mut tr = Fake::new();
        tr.viewer = viewer.clone();
        o.set_reads_target(Arc::new(tr), RAW_KEY);
        let (id, err) = o.connected_viewer().await;
        assert!(err.is_none());
        assert!(id.connected);
        assert_eq!(id.viewer, viewer);
        assert_ne!(id.masked_token, RAW_KEY, "token must be masked");
        assert!(id.masked_token.contains("***…"));
    }

    #[tokio::test]
    async fn connected_viewer_resolution_error() {
        const RAW_KEY: &str = "lin_api_secretvalue_9999";
        let o = reads_test_orch();
        let mut tr = Fake::new();
        tr.viewer_err = Some(TrackerError::Other("boom".to_string()));
        o.set_reads_target(Arc::new(tr), RAW_KEY);
        let (id, err) = o.connected_viewer().await;
        assert!(
            err.is_some(),
            "a resolution failure must return the error for logging"
        );
        assert!(
            !id.connected,
            "a failed resolution must report not-connected"
        );
        assert_ne!(id.masked_token, RAW_KEY);
        assert!(
            !id.masked_token.is_empty(),
            "a configured-but-unresolved key still surfaces a masked indicator"
        );
    }

    // Mirrors Go `TestListLinearProjects`: the not-loaded sentinel and the happy path.
    #[tokio::test]
    async fn list_linear_projects_sentinel_then_happy_path() {
        let o = reads_test_orch();
        match o.list_linear_projects().await {
            Err(ReadsError::ConfigNotLoaded) => {}
            other => panic!("before first load want ConfigNotLoaded, got {other:?}"),
        }
        let mut tr = Fake::new();
        tr.projects = vec![Project {
            id: "p1".to_string(),
            name: "Alpha".to_string(),
            slug: "alpha".to_string(),
            team: "Foundation".to_string(),
            color: "#10b981".to_string(),
        }];
        o.set_reads_target(Arc::new(tr), "lin_api_key_value_1234");
        let got = o.list_linear_projects().await.expect("list projects");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].slug, "alpha");
    }

    // F1 daemon-wiring guard: the off-loop `ControlHandle` (built BEFORE the orchestrator moves into
    // the control-loop task, mirroring the daemon) shares the SAME `Arc`-backed reads cell, so a later
    // reload-path `set_reads_target` is reflected in the handle's `list_linear_projects` /
    // `connected_viewer` — the property the httpapi provider adapter relies on for live Linear reads.
    #[tokio::test]
    async fn control_handle_reads_reflect_live_reload() {
        let o = reads_test_orch();
        let handle = o.control(); // built pre-load, exactly as the daemon does

        // Before any config load: the shared cell is empty, so both surfaces report not-loaded.
        match handle.list_linear_projects().await {
            Err(ReadsError::ConfigNotLoaded) => {}
            other => panic!("pre-load want ConfigNotLoaded, got {other:?}"),
        }
        let (id0, _) = handle.connected_viewer().await;
        assert!(!id0.connected, "pre-load handle must report not-connected");

        // The reload path publishes into the SAME shared cell...
        let mut tr = Fake::new();
        tr.viewer = Viewer {
            id: "v1".to_string(),
            ..Default::default()
        };
        tr.projects = vec![Project {
            id: "p1".to_string(),
            name: "Alpha".to_string(),
            slug: "alpha".to_string(),
            team: "Foundation".to_string(),
            color: "#10b981".to_string(),
        }];
        o.set_reads_target(Arc::new(tr), "lin_api_key_value_1234");

        // ...and the handle (Arc-shared) now sees it live.
        let got = handle.list_linear_projects().await.expect("list projects");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].slug, "alpha");
        let (id1, err) = handle.connected_viewer().await;
        assert!(err.is_none());
        assert!(id1.connected, "handle must resolve the reloaded viewer");
        assert!(id1.masked_token.contains("***…"), "token stays masked");
    }

    // Mirrors Go `TestReadsTargetRace`: concurrent set_reads_target (reload path) interleaved with
    // connected_viewer/list_linear_projects (HTTP tasks) must be race-free (the RwLock guard).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn reads_target_race() {
        let o = Arc::new(reads_test_orch());
        let mut tr = Fake::new();
        tr.viewer = Viewer {
            id: "v1".to_string(),
            ..Default::default()
        };
        o.set_reads_target(Arc::new(tr), "lin_api_key_value_1234");

        let mut handles = Vec::new();
        for _ in 0..16 {
            let ow = Arc::clone(&o);
            handles.push(tokio::spawn(async move {
                ow.set_reads_target(Arc::new(Fake::new()), "lin_api_key_value_5678");
            }));
            let or = Arc::clone(&o);
            handles.push(tokio::spawn(async move {
                let _ = or.connected_viewer().await;
                let _ = or.list_linear_projects().await;
            }));
        }
        for h in handles {
            h.await.expect("task joins");
        }
    }
}
