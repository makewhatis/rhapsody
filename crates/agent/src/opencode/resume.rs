//! Resume bookkeeping for the opencode backend (STUDIO-1043). Rhapsody-only.
//!
//! OpenCode usually does a whole ticket in ONE turn, and that turn's private `XDG_DATA_HOME`
//! (see [`super::state`]) holds the entire session history. When the turn is cut off —
//! `turn_timeout`, an in-band failure, a stall, a daemon shutdown — the old code deleted that
//! directory when the run ended and the retry started a fresh session, so an hour of investigation
//! was redone from scratch (STUDIO-1002 attempt 1: 1h, 256 tool calls, 63.6M tokens, then killed).
//!
//! This module is the fix's bookkeeping half. It keeps ONE record per issue, next to the state
//! root the directories live under:
//!
//! ```text
//! <state_root>/rhapsody-opencode-resume/<sanitized-issue>.json
//! ```
//!
//! A run that ends with a retryable failure writes the record and preserves its directory
//! ([`super::state::RunState::keep`]). The next dispatch of the SAME issue resolves that record
//! ([`select`]) and adopts the directory, so the child could resume the recorded session with the
//! same `-s <sessionID>` it already understands as a continuation turn. Every mismatch — a
//! different harness, model or workspace, a missing directory, a record past its retention window —
//! removes the stale record and returns a cold start with a reason the runner logs.
//!
//! Nothing here holds a credential: the record is the issue's own session id and directory path.
//! A kept directory carries exactly what the run mode already allowed (for a brokered session, no
//! `auth.json` at all — `provider-broker-design.md` §9.1).

use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::AgentError;

/// The harness name recorded and matched. A record written by another backend can never resume an
/// opencode session.
pub const HARNESS: &str = "opencode";
/// The subdirectory of `state_root` that holds the per-issue records.
pub const RECORDS_DIR: &str = "rhapsody-opencode-resume";
/// How long a retained session is eligible for resume. Older records (and their directories) are
/// discarded on the next [`select`].
pub const RETENTION_MS: i64 = 24 * 60 * 60 * 1000;

/// One issue's retained opencode session. Serialized as JSON; knowingly only ever written by this
/// module and read back by [`select`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeRecord {
    /// The backend that wrote it — [`HARNESS`] for every record this module writes.
    pub harness: String,
    /// The harness-config model the session ran on (see the module doc on model overrides).
    pub model: String,
    /// The worktree path the session ran in. The same issue is dispatched in the same workspace,
    /// so this doubles as the branch check.
    pub workspace: String,
    /// opencode's `ses_…` id, to be passed back as `-s <id>`.
    pub session_id: String,
    /// The private `XDG_DATA_HOME` directory the session's history lives in.
    pub dir: String,
    /// When the record was written, in Unix milliseconds — the retention clock.
    pub saved_at_ms: i64,
}

/// The outcome of looking for a resumable session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeDecision {
    /// A record matched and its directory is present; adopt it.
    Resume(ResumeRecord),
    /// Start cold. The string is the reason, empty when there was simply no record and non-empty
    /// when a record existed and was rejected (so the runner logs it exactly once).
    Cold(String),
}

/// `<state_root>/rhapsody-opencode-resume`, resolved the same way the state directories are: an
/// empty `state_root` means the canonicalized system temp dir.
fn resolved_root(state_root: &str) -> std::io::Result<PathBuf> {
    if state_root.is_empty() {
        std::env::temp_dir().canonicalize()
    } else {
        Ok(PathBuf::from(state_root))
    }
}

/// The record file for `issue` under the resolved root. The identifier is sanitized so a hostile
/// tracker key cannot escape the records directory.
pub fn record_path(state_root: &str, issue: &str) -> Option<PathBuf> {
    let root = resolved_root(state_root).ok()?;
    Some(
        root.join(RECORDS_DIR)
            .join(format!("{}.json", rhapsody_workspace::sanitize_key(issue))),
    )
}

/// Whether `dir` is one of THIS state root's per-session directories. Deleting a path named by a
/// record is only ever safe when it is a direct child of the root and carries the session prefix;
/// a hand-edited or corrupt record must never be able to point the cleanup at an arbitrary path.
fn is_managed_dir(root: &Path, dir: &Path) -> bool {
    dir.parent() == Some(root)
        && dir
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("rhapsody-opencode-"))
}

/// Reads the record for `issue`, if one exists and parses.
fn load_at(root: &Path, issue: &str) -> Option<ResumeRecord> {
    let path = root
        .join(RECORDS_DIR)
        .join(format!("{}.json", rhapsody_workspace::sanitize_key(issue)));
    let text = std::fs::read_to_string(&path).ok()?;
    match serde_json::from_str::<ResumeRecord>(&text) {
        Ok(rec) => Some(rec),
        Err(e) => {
            tracing::warn!(
                path = %path.display(), err = %e,
                "opencode: the retained-session record is unreadable; discarding it"
            );
            let _ = std::fs::remove_file(&path);
            None
        }
    }
}

/// Removes the record file for `issue`, best-effort. Never touches the directory.
pub fn clear(state_root: &str, issue: &str) {
    if let Some(path) = record_path(state_root, issue) {
        remove_file_best_effort(&path);
    }
}

/// Removes the record AND the directory it names, when that directory is one of this root's own
/// per-session directories. Used for retention, terminal completion and mismatch cleanup, so
/// "never keep more than one per issue" holds.
pub fn discard(state_root: &str, issue: &str) {
    let Ok(root) = resolved_root(state_root) else {
        return;
    };
    if let Some(rec) = load_at(&root, issue) {
        discard_record_dir(&root, &rec);
    }
    clear(state_root, issue);
}

/// Removes the directory a record names, if it is a managed child of `root`.
fn discard_record_dir(root: &Path, rec: &ResumeRecord) {
    let dir = Path::new(&rec.dir);
    if is_managed_dir(root, dir) {
        if let Err(e) = std::fs::remove_dir_all(dir)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(
                dir = %dir.display(), err = %e,
                "opencode: could not remove a retained session directory"
            );
        }
    } else {
        tracing::warn!(
            dir = %dir.display(),
            "opencode: a retained-session record names a directory outside the state root; \
             refusing to remove it"
        );
    }
}

fn remove_file_best_effort(path: &Path) {
    if let Err(e) = std::fs::remove_file(path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(
            path = %path.display(), err = %e,
            "opencode: could not remove a retained-session record"
        );
    }
}

/// Writes `rec` for `issue`, replacing any existing record. A previous record that named a
/// DIFFERENT directory has that directory removed first, so an issue never retains two sessions.
/// The write is atomic (a temp file renamed into place) so a crash cannot leave a partial record.
pub fn save(state_root: &str, issue: &str, rec: &ResumeRecord) -> Result<(), AgentError> {
    let root = resolved_root(state_root).map_err(|e| {
        AgentError::Other(format!(
            "opencode resume record root could not be resolved: {e}"
        ))
    })?;
    let dir = root.join(RECORDS_DIR);
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .map_err(|e| {
            AgentError::Other(format!("opencode resume record dir {}: {e}", dir.display()))
        })?;
    // A record for the same issue that pointed at a different directory is stale: drop its dir
    // before this one takes its place.
    if let Some(old) = load_at(&root, issue)
        && old.dir != rec.dir
    {
        discard_record_dir(&root, &old);
    }
    let path = dir.join(format!("{}.json", rhapsody_workspace::sanitize_key(issue)));
    let body = serde_json::to_vec(rec)
        .map_err(|e| AgentError::Other(format!("opencode resume record serialize: {e}")))?;
    let tmp = path.with_extension("json.tmp");
    write_private(&tmp, &body)?;
    std::fs::rename(&tmp, &path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        AgentError::Other(format!("opencode resume record {}: {e}", path.display()))
    })
}

fn write_private(path: &Path, body: &[u8]) -> Result<(), AgentError> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| {
            AgentError::Other(format!("opencode resume record {}: {e}", path.display()))
        })?;
    f.write_all(body)
        .map_err(|e| AgentError::Other(format!("opencode resume record {}: {e}", path.display())))
}

/// Decides whether `issue`'s retained session can be resumed for a dispatch with the given
/// harness/model/workspace, discarding anything stale so at most one session is ever kept.
///
/// `now_ms` is injected so the retention window is testable without sleeping.
pub fn select(
    state_root: &str,
    issue: &str,
    harness: &str,
    model: &str,
    workspace: &str,
    now_ms: i64,
) -> ResumeDecision {
    let Ok(root) = resolved_root(state_root) else {
        return ResumeDecision::Cold(String::new());
    };
    let Some(rec) = load_at(&root, issue) else {
        return ResumeDecision::Cold(String::new());
    };

    if now_ms.saturating_sub(rec.saved_at_ms) > RETENTION_MS {
        discard_record_dir(&root, &rec);
        clear(state_root, issue);
        return ResumeDecision::Cold(
            "the retained session is past its retention window".to_string(),
        );
    }
    if rec.harness != harness {
        discard_record_dir(&root, &rec);
        clear(state_root, issue);
        return ResumeDecision::Cold(format!(
            "the retained session belongs to another harness ({})",
            rec.harness
        ));
    }
    if rec.model != model {
        discard_record_dir(&root, &rec);
        clear(state_root, issue);
        return ResumeDecision::Cold(format!(
            "the retained session ran on a different model ({})",
            rec.model
        ));
    }
    if rec.workspace != workspace {
        discard_record_dir(&root, &rec);
        clear(state_root, issue);
        return ResumeDecision::Cold(
            "the retained session ran in a different workspace".to_string(),
        );
    }
    let dir = Path::new(&rec.dir);
    if !is_managed_dir(&root, dir) {
        clear(state_root, issue);
        return ResumeDecision::Cold(
            "the retained session's directory is outside the state root".to_string(),
        );
    }
    if !dir.is_dir() {
        clear(state_root, issue);
        return ResumeDecision::Cold("the retained session's directory is missing".to_string());
    }
    ResumeDecision::Resume(rec)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opencode::testdir::TempDir;

    fn root(tmp: &TempDir) -> String {
        std::fs::canonicalize(tmp.path())
            .expect("canonicalize")
            .to_string_lossy()
            .into_owned()
    }

    /// Creates a fake retained session directory (named like a real one) and returns its path.
    fn session_dir(root: &str, name: &str) -> PathBuf {
        let dir = Path::new(root).join(name);
        std::fs::create_dir_all(&dir).expect("mkdir session dir");
        dir
    }

    fn record(dir: &Path, model: &str) -> ResumeRecord {
        ResumeRecord {
            harness: HARNESS.to_string(),
            model: model.to_string(),
            workspace: "/ws".to_string(),
            session_id: "ses_keep".to_string(),
            dir: dir.to_string_lossy().into_owned(),
            saved_at_ms: 1_000_000,
        }
    }

    #[test]
    fn no_record_is_a_silent_cold_start() {
        let tmp = TempDir::new();
        assert_eq!(
            select(&root(&tmp), "STUDIO-1043", HARNESS, "m", "/ws", 1_000_000),
            ResumeDecision::Cold(String::new())
        );
    }

    #[test]
    fn a_matching_record_resumes() {
        let tmp = TempDir::new();
        let r = root(&tmp);
        let dir = session_dir(&r, "rhapsody-opencode-STUDIO-1043-1-2-0");
        let rec = record(&dir, "m");
        save(&r, "STUDIO-1043", &rec).expect("save");
        let got = select(&r, "STUDIO-1043", HARNESS, "m", "/ws", 1_000_100);
        assert_eq!(got, ResumeDecision::Resume(rec));
    }

    #[test]
    fn a_harness_mismatch_colds_with_a_reason_and_discards() {
        let tmp = TempDir::new();
        let r = root(&tmp);
        let dir = session_dir(&r, "rhapsody-opencode-STUDIO-1043-1-2-0");
        save(&r, "STUDIO-1043", &record(&dir, "m")).expect("save");
        let got = select(&r, "STUDIO-1043", "claude", "m", "/ws", 1_000_100);
        match got {
            ResumeDecision::Cold(reason) => assert!(reason.contains("another harness"), "{reason}"),
            other => panic!("expected cold start, got {other:?}"),
        }
        assert!(
            !dir.exists(),
            "the mismatched session directory must be discarded"
        );
        assert!(record_path(&r, "STUDIO-1043").is_some_and(|p| !p.exists()));
    }

    #[test]
    fn a_model_mismatch_colds_with_a_reason() {
        let tmp = TempDir::new();
        let r = root(&tmp);
        let dir = session_dir(&r, "rhapsody-opencode-STUDIO-1043-1-2-0");
        save(&r, "STUDIO-1043", &record(&dir, "old/model")).expect("save");
        match select(&r, "STUDIO-1043", HARNESS, "new/model", "/ws", 1_000_100) {
            ResumeDecision::Cold(reason) => assert!(reason.contains("different model"), "{reason}"),
            other => panic!("expected cold start, got {other:?}"),
        }
        assert!(!dir.exists());
    }

    #[test]
    fn a_workspace_mismatch_colds_with_a_reason() {
        let tmp = TempDir::new();
        let r = root(&tmp);
        let dir = session_dir(&r, "rhapsody-opencode-STUDIO-1043-1-2-0");
        save(&r, "STUDIO-1043", &record(&dir, "m")).expect("save");
        match select(&r, "STUDIO-1043", HARNESS, "m", "/other-ws", 1_000_100) {
            ResumeDecision::Cold(reason) => {
                assert!(reason.contains("different workspace"), "{reason}")
            }
            other => panic!("expected cold start, got {other:?}"),
        }
    }

    #[test]
    fn a_missing_directory_colds_with_a_reason() {
        let tmp = TempDir::new();
        let r = root(&tmp);
        let dir = session_dir(&r, "rhapsody-opencode-STUDIO-1043-1-2-0");
        save(&r, "STUDIO-1043", &record(&dir, "m")).expect("save");
        std::fs::remove_dir_all(&dir).expect("remove");
        match select(&r, "STUDIO-1043", HARNESS, "m", "/ws", 1_000_100) {
            ResumeDecision::Cold(reason) => assert!(reason.contains("missing"), "{reason}"),
            other => panic!("expected cold start, got {other:?}"),
        }
        assert!(record_path(&r, "STUDIO-1043").is_some_and(|p| !p.exists()));
    }

    #[test]
    fn the_retention_window_discards_the_session() {
        let tmp = TempDir::new();
        let r = root(&tmp);
        let dir = session_dir(&r, "rhapsody-opencode-STUDIO-1043-1-2-0");
        let mut rec = record(&dir, "m");
        rec.saved_at_ms = 1_000_000;
        save(&r, "STUDIO-1043", &rec).expect("save");
        // 25h later.
        let got = select(
            &r,
            "STUDIO-1043",
            HARNESS,
            "m",
            "/ws",
            1_000_000 + 25 * 60 * 60 * 1000,
        );
        match got {
            ResumeDecision::Cold(reason) => assert!(reason.contains("retention"), "{reason}"),
            other => panic!("expected cold start, got {other:?}"),
        }
        assert!(!dir.exists(), "a session past retention must be removed");
    }

    #[test]
    fn saving_a_second_record_removes_the_first_directory() {
        let tmp = TempDir::new();
        let r = root(&tmp);
        let first = session_dir(&r, "rhapsody-opencode-STUDIO-1043-1-1-0");
        let second = session_dir(&r, "rhapsody-opencode-STUDIO-1043-2-2-1");
        save(&r, "STUDIO-1043", &record(&first, "m")).expect("save first");
        save(&r, "STUDIO-1043", &record(&second, "m")).expect("save second");
        assert!(
            !first.exists(),
            "the previous session directory must not be kept"
        );
        assert!(second.exists());
    }

    #[test]
    fn discard_refuses_to_remove_a_directory_outside_the_state_root() {
        let tmp = TempDir::new();
        let r = root(&tmp);
        let outside = TempDir::new();
        let victim = outside.path().join("important");
        std::fs::create_dir_all(&victim).expect("mkdir victim");
        let mut rec = record(&victim, "m");
        rec.dir = victim.to_string_lossy().into_owned();
        save(&r, "STUDIO-1043", &rec).expect("save");
        discard(&r, "STUDIO-1043");
        assert!(
            victim.is_dir(),
            "a record naming an unmanaged path must never let cleanup delete it"
        );
    }

    #[test]
    fn a_corrupt_record_colds_and_is_removed() {
        let tmp = TempDir::new();
        let r = root(&tmp);
        let path = record_path(&r, "STUDIO-1043").expect("path");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, b"{ not json").expect("write");
        assert_eq!(
            select(&r, "STUDIO-1043", HARNESS, "m", "/ws", 1_000_100),
            ResumeDecision::Cold(String::new())
        );
        assert!(!path.exists(), "a corrupt record is removed");
    }
}
