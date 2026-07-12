//! The window shell's status snapshot. In P7-D1 the daemon supervisor is not wired yet (that is D2),
//! so this mirrors the `sup == nil` branch of Go `App.Status` (`$REF/desktop/app.go`): state
//! "stopped" plus the real `Configured()` detection (whether a WORKFLOW.md exists to run). D2
//! replaces the stub state with the live supervisor snapshot (state / pid / restarts / last_err /
//! url / healthy / agent_count).

use std::path::{Path, PathBuf};

use serde::Serialize;

/// The frontend-facing status snapshot. Mirrors the Go `StatusDTO` (`$REF/desktop/app.go`); the
/// serde field names match its json tags so the webview shell (`frontend/src/bindings.ts`) sees the
/// identical shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StatusDto {
    pub state: String,
    pub pid: i64,
    pub restarts: i64,
    pub last_err: String,
    pub url: String,
    pub healthy: bool,
    pub agent_count: i64,
    pub configured: bool,
}

/// Returns the current status for the UI. D1 has no supervisor, so — like Go `App.Status` when
/// `sup == nil` — it reports `StateStopped` ("stopped") with the real `Configured()` value; the
/// remaining fields are their zero values until D2 wires the supervisor.
pub fn snapshot() -> StatusDto {
    StatusDto {
        state: "stopped".to_string(), // supervisor StateStopped.String(); no supervisor in D1
        pid: 0,
        restarts: 0,
        last_err: String::new(),
        url: String::new(),
        healthy: false,
        agent_count: 0,
        configured: configured(),
    }
}

/// Reports whether a WORKFLOW.md exists to run. Mirrors Go `App.Configured` (`$REF/desktop/app.go`):
/// the onboarding flow (D4) writes it; until then the UI shows "not configured" and the daemon is
/// not started.
pub fn configured() -> bool {
    resolve_workflow_path()
        .map(|path| path_is_file(&path))
        .unwrap_or(false)
}

/// Resolves the WORKFLOW.md the app supervises from the environment: a `SYMPHONY_WORKFLOW` override
/// (dev), else `~/.symphony/WORKFLOW.md`. Mirrors Go `resolveWorkflowPath` (`$REF/desktop/app.go`);
/// `HOME` is read directly, matching `os.UserHomeDir` on macOS.
fn resolve_workflow_path() -> Option<PathBuf> {
    resolve_workflow_path_from(
        std::env::var("SYMPHONY_WORKFLOW").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )
}

/// Pure resolver for [`resolve_workflow_path`], taking the env values so it is unit-testable without
/// mutating the process environment. A non-empty override wins; otherwise a non-empty home yields
/// `<home>/.symphony/WORKFLOW.md`; an empty/absent home (Go's `os.UserHomeDir` error) yields `None`.
fn resolve_workflow_path_from(
    workflow_override: Option<&str>,
    home: Option<&str>,
) -> Option<PathBuf> {
    if let Some(p) = workflow_override
        && !p.is_empty()
    {
        return Some(PathBuf::from(p));
    }
    match home {
        Some(h) if !h.is_empty() => Some(Path::new(h).join(".symphony").join("WORKFLOW.md")),
        _ => None,
    }
}

/// Reports whether `path` names an existing non-directory, matching Go's `os.Stat` +
/// `!info.IsDir()` in `App.Configured`.
fn path_is_file(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|m| !m.is_dir())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_prefers_a_non_empty_override() {
        assert_eq!(
            resolve_workflow_path_from(Some("/tmp/custom/WORKFLOW.md"), Some("/home/u")),
            Some(PathBuf::from("/tmp/custom/WORKFLOW.md")),
        );
    }

    #[test]
    fn resolve_ignores_an_empty_override_and_uses_home() {
        assert_eq!(
            resolve_workflow_path_from(Some(""), Some("/home/u")),
            Some(PathBuf::from("/home/u/.symphony/WORKFLOW.md")),
        );
    }

    #[test]
    fn resolve_defaults_to_home_when_no_override() {
        assert_eq!(
            resolve_workflow_path_from(None, Some("/home/u")),
            Some(PathBuf::from("/home/u/.symphony/WORKFLOW.md")),
        );
    }

    #[test]
    fn resolve_is_none_without_override_or_home() {
        assert_eq!(resolve_workflow_path_from(None, None), None);
        assert_eq!(resolve_workflow_path_from(None, Some("")), None);
    }

    #[test]
    fn path_is_file_true_for_a_regular_file_false_for_dir_or_missing() {
        let dir = std::env::temp_dir().join(format!("rhapsody-d1-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let file = dir.join("WORKFLOW.md");
        std::fs::write(&file, b"---\n").expect("write temp file");

        assert!(path_is_file(&file), "a regular file is configured");
        assert!(!path_is_file(&dir), "a directory is not");
        assert!(!path_is_file(&dir.join("nope")), "a missing path is not");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn snapshot_reports_the_stopped_parity_shape() {
        let v = serde_json::to_value(snapshot()).expect("serialize");
        let obj = v.as_object().expect("object");
        // The 8 fields (and json names) of the Go StatusDTO, no more, no fewer.
        assert_eq!(obj.len(), 8);
        assert_eq!(v["state"], "stopped");
        assert_eq!(v["pid"], 0);
        assert_eq!(v["restarts"], 0);
        assert_eq!(v["last_err"], "");
        assert_eq!(v["url"], "");
        assert_eq!(v["healthy"], false);
        assert_eq!(v["agent_count"], 0);
        assert!(v["configured"].is_boolean());
    }
}
