//! File-backed tracker — port of Go `internal/tracker/file` (`tracker.kind: file`).
//!
//! Reads issues from a local JSON file instead of Linear, for Linear-free smoke tests (INF-303):
//! combined with the committed fake-claude agent stub it gives a full daemon smoke (poll → claim →
//! dispatch → worktree → runner → store → Runs UI → state classification) with zero Linear account,
//! zero spend, and zero PRs — the hermetic e2e path P6 reuses. It is the daemon-usable sibling of
//! the test-only [`fake`](crate::fake).
//!
//! The file is the single source of truth: a [`Mutex`] serializes access; every read re-loads and
//! re-parses the file (live edits are picked up on the next poll); every write is a
//! read-modify-write committed via an atomic temp-file + rename, so reads never observe a torn
//! file. A write only mutates the target issue's state and re-serializes the parsed document, so
//! every other issue and every documented field of the target issue is preserved (a JSON key
//! outside the documented schema is not round-tripped — the schema is the contract). State
//! comparisons use [`normalize_state`], matching the Linear adapter's case-insensitive semantics.
//!
//! Go splits the on-disk schema into `schema.go`; here the schema types ([`Doc`] et al.) and the
//! tracker share one module (the split does not warrant a separate file — the ticket deliverable is
//! `file.rs`).

use crate::TrackerError;
use async_trait::async_trait;
use chrono::{DateTime, FixedOffset, Utc};
use rhapsody_core::{BlockerRef, Comment, Issue, Project, Viewer, normalize_state};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

/// Display prefix of Go's `file.ErrLoad` sentinel — any failure to read or parse the source file.
/// The file adapter's load/write failures are the parity mirror of Go's `%w`-wrapped `ErrLoad`
/// values; carried on [`TrackerError::Other`] with this token so a caller (and the ported tests)
/// can recognize the category by its Display, as Go does with `errors.Is(err, ErrLoad)`.
const ERR_LOAD: &str = "file_tracker_load";
/// Display prefix of Go's `file.ErrIssueNotFound` — the write methods hit an id absent from the file.
const ERR_ISSUE_NOT_FOUND: &str = "file_tracker_issue_not_found";
/// Display of Go's `file.errClaimUnsupported` — the pool-mode `create_comment` write the file
/// tracker does not implement (pool claiming is a Linear-only feature; INF-477).
const ERR_CLAIM_UNSUPPORTED: &str = "file_tracker_claim_unsupported";

/// Wraps `msg` as a load-category error (mirrors Go's `fmt.Errorf("%w: …", ErrLoad, …)`).
fn load_err(msg: impl AsRef<str>) -> TrackerError {
    TrackerError::Other(format!("{ERR_LOAD}: {}", msg.as_ref()))
}

/// The parity mirror of Go's `fmt.Errorf("%w: %s", ErrIssueNotFound, id)`.
fn issue_not_found_err(id: &str) -> TrackerError {
    TrackerError::Other(format!("{ERR_ISSUE_NOT_FOUND}: {id}"))
}

/// The parity mirror of Go's `fmt.Errorf("%w: no state mapped for type %q", ErrLinearStateNotFound,
/// stateType)` — [`TrackerError::StateNotFound`] composes the `linear_state_not_found` token.
fn state_not_found_err(state_type: &str) -> TrackerError {
    TrackerError::StateNotFound(format!("no state mapped for type {state_type:?}"))
}

/// Backs `move_issue_to_type` / `fetch_blocked_backlog_issues` when the file omits a `state_types`
/// section ENTIRELY (the section is optional). Mirrors Go's `defaultStateTypes` map
/// `{"backlog": "Backlog", "unstarted": "Todo"}`; an unmapped type returns `None`. When the section
/// IS present, a missing type is treated as unmapped ([`TrackerError::StateNotFound`]).
fn default_state_type(state_type: &str) -> Option<String> {
    match state_type {
        "backlog" => Some("Backlog".to_string()),
        "unstarted" => Some("Todo".to_string()),
        _ => None,
    }
}

/// Configures a file [`Tracker`]. It carries the same filtering inputs as [`linear::Config`] so the
/// factory can populate both from one [`Spec`], but `project_slug`, `summon_token` and `milestone`
/// are accepted for parity and unused in v1 (no per-project / milestone / assignee filtering).
///
/// [`linear::Config`]: crate::linear::Config
/// [`Spec`]: crate::Spec
#[derive(Debug, Clone, Default)]
pub struct Config {
    /// Path to the JSON issue file (required).
    pub source: String,
    pub project_slug: String,
    pub active_states: Vec<String>,
    pub review_states: Vec<String>,
    pub summon_token: String,
    pub milestone: String,
}

/// A file-backed [`Tracker`](crate::Tracker). All access to the source file is serialized by `mu`
/// (the read-modify-write of the write methods must be atomic across the orchestrator's shared
/// tasks, exactly as Go guards it with `sync.Mutex`).
pub struct Tracker {
    /// Guards every touch of the source file. `()` because the file itself — not any in-memory
    /// value — is the shared state; this is the "right to read/rewrite the file" lock.
    mu: Mutex<()>,
    source: String,
    active_states: Vec<String>,
    review_states: Vec<String>,
}

/// Builds a file [`Tracker`] from `config` (mirrors Go's `file.New`). Only the fields the adapter
/// uses are retained; the rest of [`Config`] is parity-only (see its docs).
pub fn new(config: Config) -> Tracker {
    Tracker {
        mu: Mutex::new(()),
        source: config.source,
        active_states: config.active_states,
        review_states: config.review_states,
    }
}

impl Tracker {
    /// Acquires the file lock, recovering a poisoned mutex rather than panicking (the adapter is
    /// non-test-cfg code; a panic in one task must not poison-crash the others — same policy as
    /// [`fake`](crate::fake)).
    fn lock(&self) -> MutexGuard<'_, ()> {
        self.mu.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Reads and parses the source file. The caller must hold [`Self::lock`]. Mirrors Go's
    /// `loadLocked`: read then unmarshal, both failures wrapping the load sentinel.
    fn load_locked(&self) -> Result<Doc, TrackerError> {
        let bytes = std::fs::read(&self.source)
            .map_err(|e| load_err(format!("read {}: {e}", self.source)))?;
        serde_json::from_slice(&bytes).map_err(|e| load_err(format!("parse {}: {e}", self.source)))
    }

    /// Serializes `doc` back to the source file via an atomic temp-file + rename, so a concurrent
    /// reader never observes a partially written file. The caller must hold [`Self::lock`]. Mirrors
    /// Go's `writeLocked` (2-space indent + trailing newline, source-mode preservation, temp cleanup
    /// on every error path).
    fn write_locked(&self, doc: &Doc) -> Result<(), TrackerError> {
        let json =
            serde_json::to_string_pretty(doc).map_err(|e| load_err(format!("marshal: {e}")))?;
        // Match Go `json.MarshalIndent`'s default HTML escaping (`SetEscapeHTML(true)`): `<`, `>`,
        // `&` and the U+2028/U+2029 line/paragraph separators are emitted as `\uXXXX`. In any valid
        // JSON document these characters appear ONLY inside string literals (they are not JSON
        // structural or number characters), so a global replace is exact and cannot corrupt
        // structure — it keeps a Rust-written file byte-identical to a Go-written one.
        let mut data = json
            .replace('<', "\\u003c")
            .replace('>', "\\u003e")
            .replace('&', "\\u0026")
            .replace('\u{2028}', "\\u2028")
            .replace('\u{2029}', "\\u2029");
        data.push('\n');

        // Go writes the temp file in the source's directory (`filepath.Dir`), so the rename is a
        // same-filesystem atomic swap. `filepath.Dir("issues.json")` is ".", so treat an empty
        // parent the same way.
        let dir = match Path::new(&self.source).parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => PathBuf::from("."),
        };

        let (tmp_path, mut file) = create_temp(&dir)?;
        let write_result = file.write_all(data.as_bytes());
        drop(file); // close the fd before chmod/rename, mirroring Go's `tmp.Close()`
        if let Err(e) = write_result {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(load_err(format!("write temp: {e}")));
        }

        // `create_new` makes the temp 0600; preserve the source file's existing mode (default 0644
        // for a fresh file) so a write-back doesn't silently tighten permissions on a file a human
        // or CI created. Unix-only, mirroring Go's `os.Chmod` (the daemon's target platform).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = match std::fs::metadata(&self.source) {
                Ok(meta) => meta.permissions().mode() & 0o777,
                Err(_) => 0o644,
            };
            if let Err(e) =
                std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(mode))
            {
                let _ = std::fs::remove_file(&tmp_path);
                return Err(load_err(format!("chmod temp: {e}")));
            }
        }

        if let Err(e) = std::fs::rename(&tmp_path, &self.source) {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(load_err(format!("rename temp: {e}")));
        }
        Ok(())
    }

    /// The normalized set of active ∪ review states. Mirroring the Linear adapter, candidates
    /// include review-state issues so the orchestrator's summon-reopen branch can evaluate them.
    fn candidate_state_set(&self) -> HashSet<String> {
        self.active_states
            .iter()
            .chain(self.review_states.iter())
            .map(|s| normalize_state(s))
            .collect()
    }
}

/// Generates unique temp-file names within a process run (combined with the pid for cross-process
/// distinctness); `create_new` (`O_EXCL`) is the actual collision guard.
static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Creates a fresh `.tracker-*.json.tmp` file in `dir`, retrying on the rare name collision. The
/// std-only mirror of Go's `os.CreateTemp(dir, ".tracker-*.json.tmp")` (random suffix + `O_EXCL`).
fn create_temp(dir: &Path) -> Result<(PathBuf, File), TrackerError> {
    let pid = std::process::id();
    for _ in 0..10_000 {
        let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!(".tracker-{pid}-{seq}.json.tmp"));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(load_err(format!("temp file: {e}"))),
        }
    }
    Err(load_err("temp file: exhausted candidate names"))
}

// --- on-disk JSON schema (port of Go `internal/tracker/file/schema.go`) ---
//
// `Doc` is the on-disk JSON schema (INF-303) and the single source of truth: every read re-parses
// it and every write re-serializes the parsed document, so untouched issues and all documented
// fields of the touched issue survive a state write. `viewer`/`projects`/`state_types` are optional
// — minimal defaults are synthesized when absent (see `resolve_viewer` / `list_projects` /
// `default_state_type`). The serde field attributes mirror Go's `encoding/json` tags, including
// `omitempty`; every non-`Option` field also carries `null_to_default` so an explicit JSON `null`
// coerces to the zero value instead of failing the parse, exactly as Go's `encoding/json` does.
// A Rust-written file is byte-identical to a Go-written one (write-back matches Go's HTML escaping;
// see `write_locked`) with one known exception: sub-second timestamp fractions — chrono pads them
// to 3/6/9 digits where Go's RFC3339Nano trims trailing zeros. Whole-second/`Z`/offset timestamps
// (all realistic data) round-trip identically.

/// serde `deserialize_with` shim that coerces a JSON `null` to `T::default()` instead of erroring.
/// Go's `encoding/json` treats `null` into any non-pointer type as a no-op leaving the zero value;
/// serde by default rejects `null` for a non-`Option` field, so every such field uses this to stay
/// behavior-identical (an absent key is still handled by the companion `#[serde(default)]`).
fn null_to_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Debug, Serialize, Deserialize)]
struct Doc {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    viewer: Option<ViewerJson>,
    #[serde(
        default,
        deserialize_with = "null_to_default",
        skip_serializing_if = "Vec::is_empty"
    )]
    projects: Vec<ProjectJson>,
    /// `state_types` is an [`Option`] so an absent section (`None`) is distinguishable from a
    /// present-but-empty one (`Some({})`): absent => the built-in defaults apply; present (even
    /// empty) => the file's own map is authoritative and an unmapped type is
    /// [`TrackerError::StateNotFound`]. See `move_issue_to_type`. A [`BTreeMap`] keeps the
    /// serialized key order deterministic (sorted), matching Go's map marshaling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    state_types: Option<BTreeMap<String, String>>,
    #[serde(default, deserialize_with = "null_to_default")]
    issues: Vec<IssueJson>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ViewerJson {
    #[serde(default, deserialize_with = "null_to_default")]
    id: String,
    #[serde(
        default,
        deserialize_with = "null_to_default",
        skip_serializing_if = "String::is_empty"
    )]
    name: String,
    #[serde(
        default,
        deserialize_with = "null_to_default",
        skip_serializing_if = "String::is_empty"
    )]
    display_name: String,
    #[serde(
        default,
        deserialize_with = "null_to_default",
        skip_serializing_if = "String::is_empty"
    )]
    email: String,
    #[serde(
        default,
        deserialize_with = "null_to_default",
        skip_serializing_if = "String::is_empty"
    )]
    url_key: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct ProjectJson {
    #[serde(default, deserialize_with = "null_to_default")]
    id: String,
    #[serde(
        default,
        deserialize_with = "null_to_default",
        skip_serializing_if = "String::is_empty"
    )]
    name: String,
    #[serde(
        default,
        deserialize_with = "null_to_default",
        skip_serializing_if = "String::is_empty"
    )]
    slug: String,
    #[serde(
        default,
        deserialize_with = "null_to_default",
        skip_serializing_if = "String::is_empty"
    )]
    team: String,
    #[serde(
        default,
        deserialize_with = "null_to_default",
        skip_serializing_if = "String::is_empty"
    )]
    color: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct BlockerJson {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    identifier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    state: Option<String>,
}

/// A friendly subset of [`core::Issue`](Issue). Pointer/time fields decode straight from
/// null/absent JSON; timestamps are explicit RFC3339 ([`DateTime<FixedOffset>`] preserves the
/// parsed offset on write-back, exactly as Go's `time.Time` round-trips).
#[derive(Debug, Serialize, Deserialize)]
struct IssueJson {
    #[serde(default, deserialize_with = "null_to_default")]
    id: String,
    #[serde(default, deserialize_with = "null_to_default")]
    identifier: String,
    #[serde(default, deserialize_with = "null_to_default")]
    title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    priority: Option<i64>,
    #[serde(default, deserialize_with = "null_to_default")]
    state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    branch_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    #[serde(
        default,
        deserialize_with = "null_to_default",
        skip_serializing_if = "String::is_empty"
    )]
    team_id: String,
    #[serde(
        default,
        deserialize_with = "null_to_default",
        skip_serializing_if = "Vec::is_empty"
    )]
    labels: Vec<String>,
    #[serde(
        default,
        deserialize_with = "null_to_default",
        skip_serializing_if = "Vec::is_empty"
    )]
    blocked_by: Vec<BlockerJson>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    created_at: Option<DateTime<FixedOffset>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated_at: Option<DateTime<FixedOffset>>,
    #[serde(
        default,
        deserialize_with = "null_to_default",
        skip_serializing_if = "is_false"
    )]
    linked_pr: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    latest_pr_activity_at: Option<DateTime<FixedOffset>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    latest_summon_at: Option<DateTime<FixedOffset>>,
    #[serde(
        default,
        deserialize_with = "null_to_default",
        skip_serializing_if = "String::is_empty"
    )]
    latest_summon_body: String,
    #[serde(
        default,
        deserialize_with = "null_to_default",
        skip_serializing_if = "String::is_empty"
    )]
    milestone_id: String,
    #[serde(
        default,
        deserialize_with = "null_to_default",
        skip_serializing_if = "String::is_empty"
    )]
    milestone_name: String,
    #[serde(
        default,
        deserialize_with = "null_to_default",
        skip_serializing_if = "String::is_empty"
    )]
    assignee_id: String,
    #[serde(
        default,
        deserialize_with = "null_to_default",
        skip_serializing_if = "String::is_empty"
    )]
    assignee_name: String,
}

/// serde `skip_serializing_if` predicate for the `omitempty` bool field (`linked_pr`).
fn is_false(b: &bool) -> bool {
    !*b
}

/// Maps an on-disk [`IssueJson`] to the normalized [`core::Issue`](Issue) the orchestrator
/// consumes, mirroring the Linear adapter's normalization where it matters: labels are lowercased
/// ([`normalize_state`]), and timestamps are coerced to UTC so comparisons match the linear path.
/// A nil/empty labels or blocked_by slice becomes `None` (Go's `append`-to-nil leaves it nil).
fn to_core_issue(j: &IssueJson) -> Issue {
    Issue {
        id: j.id.clone(),
        identifier: j.identifier.clone(),
        title: j.title.clone(),
        description: j.description.clone(),
        priority: j.priority,
        state: j.state.clone(),
        branch_name: j.branch_name.clone(),
        url: j.url.clone(),
        team_id: j.team_id.clone(),
        labels: if j.labels.is_empty() {
            None
        } else {
            Some(j.labels.iter().map(|l| normalize_state(l)).collect())
        },
        blocked_by: if j.blocked_by.is_empty() {
            None
        } else {
            Some(
                j.blocked_by
                    .iter()
                    .map(|b| BlockerRef {
                        id: b.id.clone(),
                        identifier: b.identifier.clone(),
                        state: b.state.clone(),
                    })
                    .collect(),
            )
        },
        created_at: utc(j.created_at),
        updated_at: utc(j.updated_at),
        linked_pr: j.linked_pr,
        latest_pr_activity_at: utc(j.latest_pr_activity_at),
        // The file schema carries no linked_prs list (Go's `toCoreIssue` leaves it nil too).
        linked_prs: None,
        latest_summon_at: utc(j.latest_summon_at),
        latest_summon_body: j.latest_summon_body.clone(),
        milestone_id: j.milestone_id.clone(),
        milestone_name: j.milestone_name.clone(),
        assignee_id: j.assignee_id.clone(),
        assignee_name: j.assignee_name.clone(),
    }
}

/// Coerces a parsed timestamp to UTC (nil-safe), matching the Linear adapter's `parseTime` so
/// downstream time comparisons (summon watermark) behave identically across trackers.
fn utc(t: Option<DateTime<FixedOffset>>) -> Option<DateTime<Utc>> {
    t.map(|t| t.with_timezone(&Utc))
}

#[async_trait]
impl crate::Tracker for Tracker {
    /// Reloads the file and returns issues whose normalized state is in active ∪ review states (no
    /// assignee/milestone filter in v1).
    async fn fetch_candidate_issues(&self) -> Result<Vec<Issue>, TrackerError> {
        let _guard = self.lock();
        let doc = self.load_locked()?;
        let want = self.candidate_state_set();
        Ok(doc
            .issues
            .iter()
            .filter(|j| want.contains(&normalize_state(&j.state)))
            .map(to_core_issue)
            .collect())
    }

    /// Reloads the file and returns issues whose normalized state is in `states`. An empty slice
    /// returns an empty result without reading the file (mirrors the Linear adapter).
    async fn fetch_issues_by_states(&self, states: &[String]) -> Result<Vec<Issue>, TrackerError> {
        if states.is_empty() {
            return Ok(Vec::new());
        }
        let _guard = self.lock();
        let doc = self.load_locked()?;
        let want: HashSet<String> = states.iter().map(|s| normalize_state(s)).collect();
        Ok(doc
            .issues
            .iter()
            .filter(|j| want.contains(&normalize_state(&j.state)))
            .map(to_core_issue)
            .collect())
    }

    /// Reloads the file and returns issues whose id is in `ids` (the teardown-detection path). An
    /// empty slice returns an empty result without reading the file.
    async fn fetch_issue_states_by_ids(&self, ids: &[String]) -> Result<Vec<Issue>, TrackerError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let _guard = self.lock();
        let doc = self.load_locked()?;
        let want: HashSet<&str> = ids.iter().map(String::as_str).collect();
        Ok(doc
            .issues
            .iter()
            .filter(|j| want.contains(j.id.as_str()))
            .map(to_core_issue)
            .collect())
    }

    /// Reloads the file and returns issues whose state matches the Backlog state TYPE (resolved via
    /// the file's `state_types` map, defaulting to "Backlog" when the section is absent — mirroring
    /// `move_issue_to_type`). `blocked_by` edges are populated by [`to_core_issue`]. INF-318.
    async fn fetch_blocked_backlog_issues(&self) -> Result<Vec<Issue>, TrackerError> {
        let _guard = self.lock();
        let doc = self.load_locked()?;
        let backlog_name = match &doc.state_types {
            Some(map) => map.get("backlog").cloned(),
            None => default_state_type("backlog"),
        };
        let backlog_name = match backlog_name {
            Some(name) if !name.is_empty() => name,
            _ => return Ok(Vec::new()), // no Backlog state mapped → no Backlog candidates
        };
        let want = normalize_state(&backlog_name);
        Ok(doc
            .issues
            .iter()
            .filter(|j| normalize_state(&j.state) == want)
            .map(to_core_issue)
            .collect())
    }

    /// Reloads the file and returns the issue's `branch_name` (best-effort; the file schema carries
    /// no PR number, so the PR number is always 0). A missing/empty id returns `("", 0)` — the
    /// stacking hint is advisory, never fatal (INF-318).
    async fn fetch_issue_branch_by_id(&self, id: &str) -> Result<(String, i64), TrackerError> {
        if id.is_empty() {
            return Ok((String::new(), 0));
        }
        let _guard = self.lock();
        let doc = self.load_locked()?;
        match doc.issues.iter().find(|j| j.id == id) {
            Some(j) => match &j.branch_name {
                Some(branch) => Ok((branch.clone(), 0)),
                None => Ok((String::new(), 0)),
            },
            None => Ok((String::new(), 0)),
        }
    }

    /// Sets the named state on the issue with the given id and rewrites the file atomically.
    /// `team_id` is ignored (the file is keyed by id, the single source of truth). An unknown id is
    /// an issue-not-found error.
    async fn move_issue_state(
        &self,
        issue_id: &str,
        _team_id: &str,
        state_name: &str,
    ) -> Result<(), TrackerError> {
        if issue_id.is_empty() || state_name.is_empty() {
            return Err(load_err(format!(
                "move requires issueID and stateName (got {issue_id:?},{state_name:?})"
            )));
        }
        let _guard = self.lock();
        let mut doc = self.load_locked()?;
        match doc.issues.iter().position(|j| j.id == issue_id) {
            Some(i) => {
                doc.issues[i].state = state_name.to_string();
                self.write_locked(&doc)
            }
            None => Err(issue_not_found_err(issue_id)),
        }
    }

    /// Resolves the Linear state TYPE ("backlog"/"unstarted") to a display name via the file's
    /// `state_types` map (falling back to a built-in default map only when the file omits the
    /// section ENTIRELY — a present-but-empty `{}` is authoritative and maps nothing), sets that
    /// state on the issue, rewrites the file, and returns the name. A type with no mapping is
    /// [`TrackerError::StateNotFound`] (Linear's contract). An unknown id is an issue-not-found
    /// error. The type resolution happens BEFORE the id lookup, mirroring Go.
    async fn move_issue_to_type(
        &self,
        issue_id: &str,
        _team_id: &str,
        state_type: &str,
    ) -> Result<String, TrackerError> {
        if issue_id.is_empty() || state_type.is_empty() {
            return Err(load_err(format!(
                "move-to-type requires issueID and type (got {issue_id:?},{state_type:?})"
            )));
        }
        let _guard = self.lock();
        let mut doc = self.load_locked()?;
        // An absent section (None) uses the built-in defaults; a present section (even an empty
        // `{}`) is authoritative, so an unmapped type there is StateNotFound.
        let mapped = match &doc.state_types {
            Some(map) => map.get(state_type).cloned(),
            None => default_state_type(state_type),
        };
        let state_name = match mapped {
            Some(name) if !name.is_empty() => name,
            _ => return Err(state_not_found_err(state_type)),
        };
        match doc.issues.iter().position(|j| j.id == issue_id) {
            Some(i) => {
                doc.issues[i].state = state_name.clone();
                self.write_locked(&doc)?;
                Ok(state_name)
            }
            None => Err(issue_not_found_err(issue_id)),
        }
    }

    /// Returns the file's viewer, synthesizing a minimal default when absent or when it carries no
    /// id (a tracker must always resolve to a stable, non-empty identity).
    async fn resolve_viewer(&self) -> Result<Viewer, TrackerError> {
        let _guard = self.lock();
        let doc = self.load_locked()?;
        match &doc.viewer {
            Some(v) if !v.id.is_empty() => Ok(Viewer {
                id: v.id.clone(),
                name: v.name.clone(),
                display_name: v.display_name.clone(),
                email: v.email.clone(),
                url_key: v.url_key.clone(),
            }),
            _ => Ok(Viewer {
                id: "file-viewer".to_string(),
                name: "File Tracker".to_string(),
                display_name: "File Tracker".to_string(),
                email: String::new(),
                url_key: String::new(),
            }),
        }
    }

    /// Returns the file's projects, synthesizing a single minimal default when absent.
    async fn list_projects(&self) -> Result<Vec<Project>, TrackerError> {
        let _guard = self.lock();
        let doc = self.load_locked()?;
        if doc.projects.is_empty() {
            return Ok(vec![Project {
                id: "file-project".to_string(),
                name: "File".to_string(),
                slug: "file".to_string(),
                team: String::new(),
                color: String::new(),
            }]);
        }
        Ok(doc
            .projects
            .iter()
            .map(|p| Project {
                id: p.id.clone(),
                name: p.name.clone(),
                slug: p.slug.clone(),
                team: p.team.clone(),
                color: p.color.clone(),
            })
            .collect())
    }

    // The pool-mode claim protocol (assign / claim comments / read-back) is a Linear-only feature
    // (INF-477): the file tracker backs single-project, assignee-mode smoke tests, so it does not
    // implement claiming. These methods satisfy the `Tracker` contract but are inert — pool mode
    // should never be configured against a file tracker.

    /// A no-op for the file tracker (pool claiming is Linear-only; INF-477).
    async fn assign_issue(&self, _issue_id: &str, _assignee_id: &str) -> Result<(), TrackerError> {
        Ok(())
    }

    /// Always reports "unassigned" for the file tracker (INF-477).
    async fn fetch_issue_assignee(&self, _issue_id: &str) -> Result<String, TrackerError> {
        Ok(String::new())
    }

    /// Unsupported by the file tracker (pool claiming is Linear-only; INF-477).
    async fn create_comment(&self, _issue_id: &str, _body: &str) -> Result<String, TrackerError> {
        Err(TrackerError::Other(ERR_CLAIM_UNSUPPORTED.to_string()))
    }

    /// Returns no comments for the file tracker (INF-477).
    async fn list_comments(&self, _issue_id: &str) -> Result<Vec<Comment>, TrackerError> {
        Ok(Vec::new())
    }

    /// A no-op for the file tracker (INF-477).
    async fn delete_comment(&self, _comment_id: &str) -> Result<(), TrackerError> {
        Ok(())
    }

    /// Appends `label_name` to the issue's labels and rewrites the file atomically (STUDIO-644).
    /// `team_id` is ignored — the file has no label registry to find-or-create in, so there is
    /// nothing to resolve. Strictly additive and idempotent, exactly as the trait requires: an
    /// issue that already carries the label (compared case-insensitively, matching the Linear
    /// adapter's lowercasing) is a successful no-op that does not even rewrite the file. An
    /// unknown id is an issue-not-found error.
    async fn add_issue_label(
        &self,
        issue_id: &str,
        _team_id: &str,
        label_name: &str,
    ) -> Result<(), TrackerError> {
        if issue_id.is_empty() || label_name.is_empty() {
            return Err(load_err(format!(
                "add label requires issueID and labelName (got {issue_id:?},{label_name:?})"
            )));
        }
        let _guard = self.lock();
        let mut doc = self.load_locked()?;
        match doc.issues.iter().position(|j| j.id == issue_id) {
            Some(i) => {
                if doc.issues[i]
                    .labels
                    .iter()
                    .any(|l| l.eq_ignore_ascii_case(label_name))
                {
                    return Ok(());
                }
                doc.issues[i].labels.push(label_name.to_string());
                self.write_locked(&doc)
            }
            None => Err(issue_not_found_err(issue_id)),
        }
    }

    /// Drops `label_name` from the issue's labels and rewrites the file atomically (STUDIO-672).
    /// `team_id` is ignored for the same reason the add ignores it: the file has no label registry
    /// to resolve against. Idempotent — an issue that does not carry the label is a successful
    /// no-op that does not even rewrite the file. Comparison is case-insensitive, matching the add.
    /// An unknown id is an issue-not-found error.
    async fn remove_issue_label(
        &self,
        issue_id: &str,
        _team_id: &str,
        label_name: &str,
    ) -> Result<(), TrackerError> {
        if issue_id.is_empty() || label_name.is_empty() {
            return Err(load_err(format!(
                "remove label requires issueID and labelName (got {issue_id:?},{label_name:?})"
            )));
        }
        let _guard = self.lock();
        let mut doc = self.load_locked()?;
        match doc.issues.iter().position(|j| j.id == issue_id) {
            Some(i) => {
                let before = doc.issues[i].labels.len();
                doc.issues[i]
                    .labels
                    .retain(|l| !l.eq_ignore_ascii_case(label_name));
                if doc.issues[i].labels.len() == before {
                    return Ok(());
                }
                self.write_locked(&doc)
            }
            None => Err(issue_not_found_err(issue_id)),
        }
    }

    /// Reloads the file and returns issues carrying ANY of `label_names` whose state is NOT
    /// terminal, with `id`, `identifier` and (lowercased) `labels` populated — the per-identity
    /// load read (STUDIO-644). An empty slice returns an empty result without reading the file
    /// (mirrors the Linear adapter).
    ///
    /// "Open" is the file analogue of the Linear adapter's state-TYPE exclusion: a state the file's
    /// optional `state_types` section maps to `completed` or `canceled` is terminal, and everything
    /// else is open. A file with no such mapping — the common case, since the built-in default map
    /// covers only `backlog`/`unstarted` — has no terminal states, so nothing is excluded. That is
    /// deliberately the permissive direction: over-counting an identity's load can only make the
    /// triage turn spread work wider, while under-counting would pile work on a busy teammate.
    async fn fetch_open_issues_by_labels(
        &self,
        label_names: &[String],
    ) -> Result<Vec<Issue>, TrackerError> {
        if label_names.is_empty() {
            return Ok(Vec::new());
        }
        let want: HashSet<String> = label_names.iter().map(|l| normalize_state(l)).collect();
        let _guard = self.lock();
        let doc = self.load_locked()?;
        let terminal: HashSet<String> = doc
            .state_types
            .iter()
            .flatten()
            .filter(|(kind, _)| kind.as_str() == "completed" || kind.as_str() == "canceled")
            .map(|(_, name)| normalize_state(name))
            .collect();
        Ok(doc
            .issues
            .iter()
            .filter(|j| !terminal.contains(&normalize_state(&j.state)))
            .map(to_core_issue)
            .filter(|iss| {
                iss.labels
                    .iter()
                    .flatten()
                    .any(|l| want.contains(l.as_str()))
            })
            .map(|iss| Issue {
                id: iss.id,
                identifier: iss.identifier,
                labels: iss.labels,
                ..Issue::default()
            })
            .collect())
    }

    /// Appends a new issue to the file and rewrites it atomically (STUDIO-659) — the review-quorum
    /// fan-out's write, so the file tracker's smoke-test loop can exercise the quorum end to end
    /// without Linear.
    ///
    /// The file has no id server, so the identifier is minted here: `<team_id or "FILE">-<n>` where
    /// `n` is one past the highest numeric suffix already in the file. It is unique within the file
    /// and stable across reloads, which is all any caller needs — `id` and `identifier` are the
    /// same string, as they already are for every issue `SAMPLE`-style fixtures declare.
    ///
    /// `state_name` is written verbatim (the file has no workflow-state registry to resolve
    /// against), and labels are written verbatim for the same reason `add_issue_label` ignores
    /// `team_id`: there is nothing to find-or-create in.
    async fn create_issue(&self, spec: &crate::NewIssue) -> Result<String, TrackerError> {
        if spec.team_id.is_empty() || spec.title.is_empty() {
            return Err(load_err(format!(
                "create issue requires teamID and title (got {:?},{:?})",
                spec.team_id, spec.title
            )));
        }
        let _guard = self.lock();
        let mut doc = self.load_locked()?;
        let identifier = mint_identifier(&doc, &spec.team_id);
        doc.issues.push(IssueJson {
            id: identifier.clone(),
            identifier: identifier.clone(),
            title: spec.title.clone(),
            description: (!spec.description.is_empty()).then(|| spec.description.clone()),
            priority: None,
            state: spec.state_name.clone(),
            branch_name: None,
            url: None,
            team_id: spec.team_id.clone(),
            labels: spec.labels.clone(),
            blocked_by: Vec::new(),
            created_at: None,
            updated_at: None,
            linked_pr: false,
            latest_pr_activity_at: None,
            latest_summon_at: None,
            latest_summon_body: String::new(),
            milestone_id: String::new(),
            milestone_name: String::new(),
            assignee_id: spec.assignee_id.clone(),
            assignee_name: String::new(),
        });
        self.write_locked(&doc)?;
        Ok(identifier)
    }
}

/// Mints an identifier for a created issue: `<prefix>-<n>`, where the prefix is the issue's team
/// and `n` is one past the largest numeric suffix any existing identifier carries (0 ⇒ 1). Scanning
/// the whole file rather than counting rows means a deleted issue's number is never reused, which
/// keeps identifiers stable references in a file an operator also hand-edits.
fn mint_identifier(doc: &Doc, team_id: &str) -> String {
    let highest = doc
        .issues
        .iter()
        .filter_map(|j| j.identifier.rsplit_once('-'))
        .filter_map(|(_, n)| n.parse::<u64>().ok())
        .max()
        .unwrap_or(0);
    format!("{team_id}-{}", highest + 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Tracker as _;
    use chrono::TimeZone;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A unique on-disk source file that removes its directory on drop — the Rust equivalent of
    /// Go's `t.TempDir()` + `writeSource`, without pulling in a temp-file dependency.
    struct TempSource {
        dir: PathBuf,
        path: PathBuf,
    }

    impl TempSource {
        fn new(content: &str) -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let seq = SEQ.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "rhapsody-file-tracker-{}-{seq}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).expect("create temp dir");
            let path = dir.join("issues.json");
            std::fs::write(&path, content).expect("write source");
            TempSource { dir, path }
        }

        fn path(&self) -> String {
            self.path.to_string_lossy().into_owned()
        }
    }

    impl Drop for TempSource {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// Mirrors Go `file_test.go`'s `newTracker`: a tracker over a temp source with the standard
    /// active/review states. Returns the [`TempSource`] guard so the caller keeps it alive.
    fn new_tracker(content: &str) -> (Tracker, TempSource) {
        let src = TempSource::new(content);
        let tracker = new(Config {
            source: src.path(),
            active_states: vec!["Todo".to_string(), "In Progress".to_string()],
            review_states: vec!["In Review".to_string()],
            ..Default::default()
        });
        (tracker, src)
    }

    const SAMPLE: &str = r#"{
  "viewer":   { "id": "u-smoke", "name": "Smoke Runner", "display_name": "smoke" },
  "projects": [ { "id": "p-1", "name": "Smoke", "slug": "smoke-sandbox" } ],
  "state_types": { "backlog": "Backlog", "unstarted": "Todo" },
  "issues": [
    { "id": "SMK-1", "identifier": "SMK-1", "title": "Todo ticket", "state": "Todo", "team_id": "team-1", "labels": ["Bug"] },
    { "id": "SMK-2", "identifier": "SMK-2", "title": "Active ticket", "state": "In Progress", "team_id": "team-1" },
    { "id": "SMK-3", "identifier": "SMK-3", "title": "Review ticket", "state": "In Review", "team_id": "team-1" },
    { "id": "SMK-4", "identifier": "SMK-4", "title": "Done ticket", "state": "Done", "team_id": "team-1" }
  ]
}"#;

    const BACKLOG_SAMPLE: &str = r#"{
  "state_types": { "backlog": "Backlog", "unstarted": "Todo" },
  "issues": [
    { "id": "MT-1", "identifier": "MT-1", "title": "root", "state": "In Review", "branch_name": "feat/mt-1" },
    { "id": "MT-2", "identifier": "MT-2", "title": "dependent", "state": "Backlog",
      "blocked_by": [ { "id": "MT-1", "identifier": "MT-1", "state": "In Review" } ] },
    { "id": "MT-3", "identifier": "MT-3", "title": "active", "state": "Todo" }
  ]
}"#;

    fn ids(issues: &[Issue]) -> Vec<String> {
        issues.iter().map(|i| i.id.clone()).collect()
    }

    // Mirrors Go `file.TestFetchCandidateIssuesIncludesActiveAndReview`.
    #[tokio::test]
    async fn fetch_candidate_issues_includes_active_and_review() {
        let (tr, _src) = new_tracker(SAMPLE);
        let got = tr.fetch_candidate_issues().await.expect("no error");
        // active (Todo, In Progress) ∪ review (In Review); Done excluded.
        let want: HashSet<&str> = ["SMK-1", "SMK-2", "SMK-3"].into_iter().collect();
        assert_eq!(got.len(), want.len(), "got {:?}, want {want:?}", ids(&got));
        for i in &got {
            assert!(
                want.contains(i.id.as_str()),
                "unexpected candidate {:?}",
                i.id
            );
        }
    }

    // Mirrors Go `file.TestFetchCandidateIssuesNoReviewStates`.
    #[tokio::test]
    async fn fetch_candidate_issues_no_review_states() {
        let src = TempSource::new(SAMPLE);
        let tr = new(Config {
            source: src.path(),
            active_states: vec!["Todo".to_string(), "In Progress".to_string()],
            ..Default::default()
        });
        let got = tr.fetch_candidate_issues().await.expect("no error");
        let want: HashSet<&str> = ["SMK-1", "SMK-2"].into_iter().collect();
        assert_eq!(got.len(), 2, "got {:?}, want {want:?}", ids(&got));
        for i in &got {
            assert!(
                want.contains(i.id.as_str()),
                "unexpected candidate {:?}",
                i.id
            );
        }
    }

    // Mirrors Go `file.TestFetchCandidateMapsFieldsAndLowercasesLabels`.
    #[tokio::test]
    async fn fetch_candidate_maps_fields_and_lowercases_labels() {
        let (tr, _src) = new_tracker(SAMPLE);
        let got = tr.fetch_candidate_issues().await.expect("no error");
        let smk1 = got
            .iter()
            .find(|i| i.id == "SMK-1")
            .expect("SMK-1 not found");
        assert_eq!(smk1.identifier, "SMK-1", "fields not mapped: {smk1:?}");
        assert_eq!(smk1.title, "Todo ticket", "fields not mapped: {smk1:?}");
        assert_eq!(smk1.team_id, "team-1", "fields not mapped: {smk1:?}");
        assert_eq!(
            smk1.labels,
            Some(vec!["bug".to_string()]),
            "labels not lowercased: {:?}",
            smk1.labels
        );
    }

    // Mirrors Go `file.TestFetchIssuesByStates`.
    #[tokio::test]
    async fn fetch_issues_by_states() {
        let (tr, _src) = new_tracker(SAMPLE);
        // case-insensitive.
        let got = tr
            .fetch_issues_by_states(&["done".to_string()])
            .await
            .expect("no error");
        assert_eq!(got.len(), 1, "got {:?}, want [SMK-4]", ids(&got));
        assert_eq!(got[0].id, "SMK-4", "got {:?}, want [SMK-4]", ids(&got));
    }

    // STUDIO-644 (Teams T3b), design §0.11.1: the label write is ADDITIVE and idempotent. The
    // human-conflict rule turns on it — the manager may only ever add where a label is absent, so a
    // second write of the same label must change nothing and must not drop the labels already
    // there.
    #[tokio::test]
    async fn add_issue_label_is_additive_and_idempotent() {
        let (tr, _src) = new_tracker(SAMPLE);
        tr.add_issue_label("SMK-1", "team-1", "rhapsody:@alice")
            .await
            .expect("add");
        // Case-differing repeat: Linear treats label names case-insensitively, and so must this.
        tr.add_issue_label("SMK-1", "team-1", "Rhapsody:@Alice")
            .await
            .expect("repeat add");

        let got = tr.fetch_candidate_issues().await.expect("no error");
        let smk1 = got.iter().find(|i| i.id == "SMK-1").expect("SMK-1");
        assert_eq!(
            smk1.labels,
            Some(vec!["bug".to_string(), "rhapsody:@alice".to_string()]),
            "the pre-existing label must survive and the identity label must be written once"
        );
    }

    // STUDIO-672: the removal drops exactly the named label, leaves every other one alone, and is
    // idempotent + case-insensitive in both directions — the same contract as the add.
    #[tokio::test]
    async fn remove_issue_label_drops_only_the_named_label() {
        let (tr, _src) = new_tracker(SAMPLE);
        tr.add_issue_label("SMK-1", "team-1", "rhapsody:@alice")
            .await
            .expect("add");
        // Case-differing removal, and then a repeat of it: neither may disturb `bug`.
        tr.remove_issue_label("SMK-1", "team-1", "Rhapsody:@Alice")
            .await
            .expect("remove");
        tr.remove_issue_label("SMK-1", "team-1", "rhapsody:@alice")
            .await
            .expect("removing what is not there is a no-op, not an error");

        let got = tr.fetch_candidate_issues().await.expect("no error");
        let smk1 = got.iter().find(|i| i.id == "SMK-1").expect("SMK-1");
        assert_eq!(
            smk1.labels,
            Some(vec!["bug".to_string()]),
            "only the identity label is gone"
        );
    }

    // An unknown id is an error rather than a silent success, so a caller can tell the removal did
    // not land — the same shape `add_issue_label` has.
    #[tokio::test]
    async fn remove_issue_label_unknown_issue_errors() {
        let (tr, _src) = new_tracker(SAMPLE);
        tr.remove_issue_label("NOPE-1", "team-1", "rhapsody:@alice")
            .await
            .expect_err("unknown issue");
    }

    // STUDIO-659, design §0.12: a created review ticket lands in the file with everything the
    // dispatcher needs — an unused identifier, the requested state, the assignee, the labels — and
    // is immediately a candidate, so the file tracker can drive the whole quorum without Linear.
    #[tokio::test]
    async fn create_issue_appends_a_dispatchable_ticket() {
        let (tr, _src) = new_tracker(SAMPLE);
        let identifier = tr
            .create_issue(&crate::NewIssue {
                team_id: "team-1".into(),
                title: "Review: SMK-1 Todo ticket".into(),
                description: "review https://github.com/o/r/pull/7".into(),
                state_name: "Todo".into(),
                assignee_id: "u-smoke".into(),
                labels: vec!["rhapsody:@bob".into()],
            })
            .await
            .expect("create");
        assert_eq!(
            identifier, "team-1-5",
            "one past the highest numeric suffix in SAMPLE (SMK-4)"
        );

        let got = tr.fetch_candidate_issues().await.expect("no error");
        let made = got
            .iter()
            .find(|i| i.identifier == identifier)
            .expect("the created ticket is a candidate");
        assert_eq!(made.title, "Review: SMK-1 Todo ticket");
        assert_eq!(made.state, "Todo");
        assert_eq!(made.team_id, "team-1");
        assert_eq!(made.assignee_id, "u-smoke");
        assert_eq!(made.labels, Some(vec!["rhapsody:@bob".to_string()]));
        assert_eq!(
            made.description.as_deref(),
            Some("review https://github.com/o/r/pull/7")
        );
    }

    // Two creates make two DISTINCT issues — the create is deliberately not idempotent, and the
    // once-per-parent guard is the caller's (the quorum's marker label).
    #[tokio::test]
    async fn create_issue_is_not_idempotent_and_never_reuses_an_identifier() {
        let (tr, _src) = new_tracker(SAMPLE);
        let spec = crate::NewIssue {
            team_id: "T".into(),
            title: "same title".into(),
            state_name: "Todo".into(),
            ..crate::NewIssue::default()
        };
        let first = tr.create_issue(&spec).await.expect("first");
        let second = tr.create_issue(&spec).await.expect("second");
        assert_ne!(first, second, "each create mints a fresh identifier");
        assert_eq!((first.as_str(), second.as_str()), ("T-5", "T-6"));
    }

    // STUDIO-659: the required arguments are checked before the file is touched.
    #[tokio::test]
    async fn create_issue_requires_a_team_and_a_title() {
        let (tr, _src) = new_tracker(SAMPLE);
        tr.create_issue(&crate::NewIssue {
            title: "t".into(),
            ..crate::NewIssue::default()
        })
        .await
        .expect_err("no team must error");
        tr.create_issue(&crate::NewIssue {
            team_id: "T".into(),
            ..crate::NewIssue::default()
        })
        .await
        .expect_err("no title must error");
    }

    // STUDIO-644: a label write against an id the file does not know is an error, not a silent
    // success — the triage task must be able to tell the assignment did not land.
    #[tokio::test]
    async fn add_issue_label_unknown_issue_errors() {
        let (tr, _src) = new_tracker(SAMPLE);
        tr.add_issue_label("NOPE-1", "team-1", "rhapsody:@alice")
            .await
            .expect_err("unknown issue must error");
        tr.add_issue_label("", "team-1", "rhapsody:@alice")
            .await
            .expect_err("empty issue id must error");
        tr.add_issue_label("SMK-1", "team-1", "")
            .await
            .expect_err("empty label must error");
    }

    // STUDIO-644, design §0.11.1: the per-identity load read returns open issues carrying the
    // label, with id + identifier + lowercased labels. `SAMPLE` maps no completed/canceled state
    // type, so nothing is terminal here — the terminal case is covered separately.
    #[tokio::test]
    async fn fetch_open_issues_by_labels_returns_labelled_issues() {
        let (tr, _src) = new_tracker(SAMPLE);
        tr.add_issue_label("SMK-1", "team-1", "rhapsody:@alice")
            .await
            .expect("add");

        let got = tr
            .fetch_open_issues_by_labels(&["rhapsody:@alice".to_string()])
            .await
            .expect("no error");
        assert_eq!(ids(&got), vec!["SMK-1".to_string()]);
        assert_eq!(got[0].identifier, "SMK-1");
        assert_eq!(
            got[0].labels,
            Some(vec!["bug".to_string(), "rhapsody:@alice".to_string()])
        );

        assert!(
            tr.fetch_open_issues_by_labels(&[])
                .await
                .expect("empty is Ok")
                .is_empty(),
            "an empty label list returns an empty result"
        );
    }

    // STUDIO-644: a state the file maps to the `completed` type is terminal, so its ticket is not
    // open load. Load counts work in flight, not work finished.
    #[tokio::test]
    async fn fetch_open_issues_by_labels_excludes_terminal_states() {
        let src = TempSource::new(
            r#"{
  "state_types": { "backlog": "Backlog", "unstarted": "Todo", "completed": "Done" },
  "issues": [
    { "id": "SMK-1", "identifier": "SMK-1", "title": "Open", "state": "Todo", "labels": ["rhapsody:@alice"] },
    { "id": "SMK-4", "identifier": "SMK-4", "title": "Finished", "state": "Done", "labels": ["rhapsody:@alice"] }
  ]
}"#,
        );
        let tr = new(Config {
            source: src.path(),
            active_states: vec!["Todo".to_string()],
            ..Default::default()
        });

        let got = tr
            .fetch_open_issues_by_labels(&["rhapsody:@alice".to_string()])
            .await
            .expect("no error");
        assert_eq!(
            ids(&got),
            vec!["SMK-1".to_string()],
            "a Done ticket is not open load"
        );
    }

    // Mirrors Go `file.TestFetchIssuesByStatesEmptyReturnsNil`.
    #[tokio::test]
    async fn fetch_issues_by_states_empty_returns_nil() {
        let (tr, _src) = new_tracker(SAMPLE);
        let got = tr.fetch_issues_by_states(&[]).await.expect("no error");
        assert!(
            got.is_empty(),
            "empty states: got {:?}, want empty",
            ids(&got)
        );
    }

    // Mirrors Go `file.TestFetchIssueStatesByIDs`.
    #[tokio::test]
    async fn fetch_issue_states_by_ids() {
        let (tr, _src) = new_tracker(SAMPLE);
        let got = tr
            .fetch_issue_states_by_ids(&[
                "SMK-2".to_string(),
                "SMK-4".to_string(),
                "missing".to_string(),
            ])
            .await
            .expect("no error");
        assert_eq!(got.len(), 2, "got {:?}, want 2", ids(&got));
    }

    // Mirrors Go `file.TestFetchIssueStatesByIDsEmptyReturnsNil`.
    #[tokio::test]
    async fn fetch_issue_states_by_ids_empty_returns_nil() {
        let (tr, _src) = new_tracker(SAMPLE);
        let got = tr.fetch_issue_states_by_ids(&[]).await.expect("no error");
        assert!(got.is_empty(), "empty ids: got {:?}, want empty", ids(&got));
    }

    // Mirrors Go `file.TestLiveReloadReflectsEdits`.
    #[tokio::test]
    async fn live_reload_reflects_edits() {
        let (tr, src) = new_tracker(SAMPLE);
        // First poll: SMK-4 is Done, so it is not a candidate.
        let got = tr.fetch_candidate_issues().await.expect("no error");
        assert_eq!(
            got.len(),
            3,
            "pre-edit candidates = {:?}, want 3",
            ids(&got)
        );
        // Edit the file out-of-band (simulating a human/CI moving a card to Todo).
        let edited = r#"{ "issues": [ { "id": "SMK-4", "identifier": "SMK-4", "title": "t", "state": "Todo" } ] }"#;
        std::fs::write(src.path(), edited).expect("rewrite source");
        let got = tr.fetch_candidate_issues().await.expect("no error");
        assert_eq!(
            got.len(),
            1,
            "post-edit candidates = {:?}, want [SMK-4]",
            ids(&got)
        );
        assert_eq!(got[0].id, "SMK-4", "post-edit candidates = {:?}", ids(&got));
    }

    // Mirrors Go `file.TestMoveIssueStatePersistsAndPreservesOthers`.
    #[tokio::test]
    async fn move_issue_state_persists_and_preserves_others() {
        let (tr, src) = new_tracker(SAMPLE);
        tr.move_issue_state("SMK-3", "team-1", "In Progress")
            .await
            .expect("move");
        // Re-read from disk with a fresh tracker to confirm the write persisted.
        let tr2 = new(Config {
            source: src.path(),
            active_states: vec!["Todo".to_string(), "In Progress".to_string()],
            review_states: vec!["In Review".to_string()],
            ..Default::default()
        });
        let got = tr2
            .fetch_issue_states_by_ids(&["SMK-3".to_string(), "SMK-1".to_string()])
            .await
            .expect("by ids");
        let by_id: std::collections::HashMap<&str, &Issue> =
            got.iter().map(|i| (i.id.as_str(), i)).collect();
        assert_eq!(
            by_id.get("SMK-3").map(|i| i.state.as_str()),
            Some("In Progress"),
            "SMK-3 state not persisted"
        );
        // Untouched issue preserved.
        assert_eq!(
            by_id.get("SMK-1").map(|i| i.title.as_str()),
            Some("Todo ticket"),
            "SMK-1 clobbered"
        );
    }

    // Mirrors Go `file.TestMoveIssueStateUnknownID`.
    #[tokio::test]
    async fn move_issue_state_unknown_id() {
        let (tr, _src) = new_tracker(SAMPLE);
        let err = tr
            .move_issue_state("NOPE", "team-1", "Todo")
            .await
            .expect_err("want issue-not-found");
        assert!(
            matches!(&err, TrackerError::Other(m) if m.starts_with(ERR_ISSUE_NOT_FOUND)),
            "want ErrIssueNotFound, got {err:?}"
        );
    }

    // Mirrors Go `file.TestMoveIssueToTypeResolvesViaStateTypes`.
    #[tokio::test]
    async fn move_issue_to_type_resolves_via_state_types() {
        let (tr, _src) = new_tracker(SAMPLE);
        let name = tr
            .move_issue_to_type("SMK-1", "team-1", "backlog")
            .await
            .expect("move-to-type");
        assert_eq!(name, "Backlog", "returned name");
        let got = tr
            .fetch_issues_by_states(&["Backlog".to_string()])
            .await
            .expect("by states");
        assert_eq!(got.len(), 1, "SMK-1 not moved to Backlog: {:?}", ids(&got));
        assert_eq!(
            got[0].id,
            "SMK-1",
            "SMK-1 not moved to Backlog: {:?}",
            ids(&got)
        );
    }

    // Mirrors Go `file.TestMoveIssueToTypeUnmappedTypeIsStateNotFound`.
    #[tokio::test]
    async fn move_issue_to_type_unmapped_type_is_state_not_found() {
        // state_types present but lacks "started" → unmapped → StateNotFound.
        let (tr, _src) = new_tracker(SAMPLE);
        let err = tr
            .move_issue_to_type("SMK-1", "team-1", "started")
            .await
            .expect_err("want state-not-found");
        assert!(
            matches!(err, TrackerError::StateNotFound(_)),
            "want StateNotFound, got {err:?}"
        );
    }

    // Mirrors Go `file.TestMoveIssueToTypePresentButEmptyStateTypesIsStateNotFound`.
    #[tokio::test]
    async fn move_issue_to_type_present_but_empty_state_types_is_state_not_found() {
        // A present-but-empty state_types ({}) is authoritative and maps nothing → unmapped type
        // is StateNotFound (NOT the built-in defaults, which apply only when the section is omitted).
        let src = r#"{ "state_types": {}, "issues": [ { "id": "X", "identifier": "X", "title": "t", "state": "In Review" } ] }"#;
        let (tr, _src) = new_tracker(src);
        let err = tr
            .move_issue_to_type("X", "", "unstarted")
            .await
            .expect_err("want state-not-found");
        assert!(
            matches!(err, TrackerError::StateNotFound(_)),
            "present-but-empty state_types: want StateNotFound, got {err:?}"
        );
    }

    // Mirrors Go `file.TestMoveIssueToTypeDefaultsWhenSectionAbsent`.
    #[tokio::test]
    async fn move_issue_to_type_defaults_when_section_absent() {
        // No state_types section → built-in defaults {backlog:Backlog, unstarted:Todo} apply.
        let src = r#"{ "issues": [ { "id": "X", "identifier": "X", "title": "t", "state": "In Review" } ] }"#;
        let (tr, _src) = new_tracker(src);
        let name = tr
            .move_issue_to_type("X", "", "unstarted")
            .await
            .expect("move-to-type");
        assert_eq!(name, "Todo", "default unstarted name");
    }

    // Mirrors Go `file.TestResolveViewerFromFile`.
    #[tokio::test]
    async fn resolve_viewer_from_file() {
        let (tr, _src) = new_tracker(SAMPLE);
        let v = tr.resolve_viewer().await.expect("resolve viewer");
        assert_eq!(v.id, "u-smoke", "viewer = {v:?}");
        assert_eq!(v.name, "Smoke Runner", "viewer = {v:?}");
    }

    // Mirrors Go `file.TestResolveViewerSyntheticDefault`.
    #[tokio::test]
    async fn resolve_viewer_synthetic_default() {
        let (tr, _src) = new_tracker(r#"{ "issues": [] }"#);
        let v = tr.resolve_viewer().await.expect("resolve viewer");
        assert!(
            !v.id.is_empty(),
            "synthetic viewer must have a non-empty id"
        );
    }

    // Mirrors Go `file.TestListProjectsFromFileAndDefault`.
    #[tokio::test]
    async fn list_projects_from_file_and_default() {
        let (tr, _src) = new_tracker(SAMPLE);
        let ps = tr.list_projects().await.expect("list projects");
        assert_eq!(ps.len(), 1, "projects = {ps:?}");
        assert_eq!(ps[0].slug, "smoke-sandbox", "projects = {ps:?}");

        let (tr_empty, _src2) = new_tracker(r#"{ "issues": [] }"#);
        let ps = tr_empty.list_projects().await.expect("list projects");
        assert_eq!(ps.len(), 1, "synthetic projects = {ps:?}");
        assert!(!ps[0].id.is_empty(), "synthetic projects = {ps:?}");
    }

    // Mirrors Go `file.TestTimestampsDecodeRFC3339ToUTC`.
    #[tokio::test]
    async fn timestamps_decode_rfc3339_to_utc() {
        let src = r#"{ "issues": [ { "id": "T", "identifier": "T", "title": "t", "state": "In Review",
          "latest_summon_at": "2026-06-17T12:00:00+02:00", "linked_pr": true } ] }"#;
        let (tr, _src) = new_tracker(src);
        let got = tr.fetch_candidate_issues().await.expect("no error");
        assert_eq!(got.len(), 1, "got {} issues", got.len());
        let iss = &got[0];
        // +02:00 12:00 == 10:00 UTC; the core type is DateTime<Utc>, so the zone is UTC by type.
        let want = Utc.with_ymd_and_hms(2026, 6, 17, 10, 0, 0).unwrap();
        assert_eq!(iss.latest_summon_at, Some(want), "latest_summon_at");
        assert!(iss.linked_pr, "linked_pr not decoded");
    }

    // Mirrors Go `file.TestSummonBodyDecodes`: latest_summon_body maps onto
    // core.Issue.LatestSummonBody; absent → empty (INF-448).
    #[tokio::test]
    async fn summon_body_decodes() {
        let src = r#"{ "issues": [
          { "id": "T", "identifier": "T", "title": "t", "state": "In Progress",
            "latest_summon_at": "2026-06-17T12:00:00Z", "latest_summon_body": "@symphony fix the MTU" },
          { "id": "U", "identifier": "U", "title": "t", "state": "Todo" } ] }"#;
        let (tr, _src) = new_tracker(src);
        let got = tr.fetch_candidate_issues().await.expect("no error");
        assert_eq!(got.len(), 2, "got {} issues", got.len());
        assert_eq!(
            got[0].latest_summon_body, "@symphony fix the MTU",
            "LatestSummonBody"
        );
        assert_eq!(
            got[1].latest_summon_body, "",
            "absent latest_summon_body must decode empty"
        );
    }

    // Mirrors Go `file.TestLoadErrorOnMissingFile`.
    #[tokio::test]
    async fn load_error_on_missing_file() {
        // A path in a temp dir that exists, but the file itself does not.
        let src = TempSource::new("{}");
        let missing = Path::new(&src.dir).join("nope.json");
        let tr = new(Config {
            source: missing.to_string_lossy().into_owned(),
            ..Default::default()
        });
        let err = tr
            .fetch_candidate_issues()
            .await
            .expect_err("want ErrLoad for missing file");
        assert!(
            matches!(&err, TrackerError::Other(m) if m.starts_with(ERR_LOAD)),
            "want ErrLoad, got {err:?}"
        );
    }

    // Mirrors Go `file.TestLoadErrorOnBadJSON`.
    #[tokio::test]
    async fn load_error_on_bad_json() {
        let (tr, _src) = new_tracker("{ not json");
        let err = tr
            .fetch_candidate_issues()
            .await
            .expect_err("want ErrLoad for bad json");
        assert!(
            matches!(&err, TrackerError::Other(m) if m.starts_with(ERR_LOAD)),
            "want ErrLoad, got {err:?}"
        );
    }

    // Mirrors Go `file.TestWriteIsAtomicValidJSON`: the rewrite leaves a complete, re-parseable
    // file (atomic temp-file + rename never leaves a torn file) and no stray temp files.
    #[tokio::test]
    async fn write_is_atomic_valid_json() {
        let (tr, src) = new_tracker(SAMPLE);
        tr.move_issue_state("SMK-1", "", "Done")
            .await
            .expect("move");
        let bytes = std::fs::read(src.path()).expect("read back");
        let doc: Doc = serde_json::from_slice(&bytes).expect("rewritten file is valid JSON");
        assert_eq!(doc.issues.len(), 4, "issue count changed after write");
        // No stray temp files left behind in the dir.
        for entry in std::fs::read_dir(&src.dir).expect("read_dir") {
            let name = entry.expect("entry").file_name();
            let name = name.to_string_lossy();
            assert!(!name.ends_with(".tmp"), "temp file left behind: {name}");
        }
    }

    // Mirrors Go `file.TestFileFetchBlockedBacklogIssues` (backlog_test.go).
    #[tokio::test]
    async fn file_fetch_blocked_backlog_issues() {
        let (tr, _src) = new_tracker(BACKLOG_SAMPLE);
        let got = tr.fetch_blocked_backlog_issues().await.expect("no error");
        assert_eq!(
            got.len(),
            1,
            "backlog issues = {:?}, want [MT-2]",
            ids(&got)
        );
        assert_eq!(
            got[0].id,
            "MT-2",
            "backlog issues = {:?}, want [MT-2]",
            ids(&got)
        );
        let blocked_by = got[0].blocked_by.as_ref().expect("BlockedBy populated");
        assert_eq!(
            blocked_by.len(),
            1,
            "BlockedBy not populated: {blocked_by:?}"
        );
        assert_eq!(
            blocked_by[0].id.as_deref(),
            Some("MT-1"),
            "BlockedBy not populated: {blocked_by:?}"
        );
    }

    // Mirrors Go `file.TestFileFetchIssueBranchByID` (backlog_test.go).
    #[tokio::test]
    async fn file_fetch_issue_branch_by_id() {
        let (tr, _src) = new_tracker(BACKLOG_SAMPLE);
        let (branch, pr) = tr
            .fetch_issue_branch_by_id("MT-1")
            .await
            .expect("branch by id");
        assert_eq!(branch, "feat/mt-1", "branch");
        assert_eq!(pr, 0, "pr");
        // Missing id and empty id → empty result, no error.
        let (branch, pr) = tr
            .fetch_issue_branch_by_id("nope")
            .await
            .expect("missing id");
        assert_eq!((branch.as_str(), pr), ("", 0), "missing id");
        let (branch, pr) = tr.fetch_issue_branch_by_id("").await.expect("empty id");
        assert_eq!((branch.as_str(), pr), ("", 0), "empty id");
    }

    // Not a mirror of a specific Go test — locks in Go `encoding/json`'s documented behavior that
    // unmarshaling a JSON `null` into any non-pointer type is a no-op yielding the zero value with
    // no error, for the non-`Option` schema fields (verified against the Go reference). A Go-tolerated
    // hand/CI edit that nulls a field must not become a hard load error in the Rust port.
    #[tokio::test]
    async fn load_tolerates_null_valued_fields() {
        // Every optional section AND every non-Option issue field explicitly null: still one issue,
        // all zero-valued (not an ERR_LOAD).
        let src = r#"{
          "viewer": null, "projects": null, "state_types": null,
          "issues": [ { "id": "N", "identifier": null, "title": null, "state": "Todo",
            "team_id": null, "labels": null, "blocked_by": null, "linked_pr": null,
            "latest_summon_body": null, "milestone_id": null, "milestone_name": null,
            "assignee_id": null, "assignee_name": null } ]
        }"#;
        let (tr, _src) = new_tracker(src);
        let got = tr
            .fetch_candidate_issues()
            .await
            .expect("null-valued fields must load like Go, not ERR_LOAD");
        assert_eq!(got.len(), 1, "got {:?}", ids(&got));
        let iss = &got[0];
        assert_eq!(iss.id, "N");
        assert_eq!(iss.identifier, "", "null string → empty");
        assert!(iss.labels.is_none(), "null slice → None");
        assert!(iss.blocked_by.is_none(), "null slice → None");
        assert!(!iss.linked_pr, "null bool → false");
        assert_eq!(iss.latest_summon_body, "", "null string → empty");
        // A null top-level `issues` array must also load as empty, not error.
        let (tr2, _src2) = new_tracker(r#"{ "issues": null }"#);
        let empty = tr2
            .fetch_candidate_issues()
            .await
            .expect("null issues must load");
        assert!(empty.is_empty(), "null issues → empty");
    }

    // Not a mirror of a specific Go test — locks in Go `json.MarshalIndent`'s default HTML escaping
    // (`SetEscapeHTML(true)`): `<`, `>`, `&` are emitted as their JSON unicode escapes, so a
    // Rust-written file stays byte-identical to a Go-written one (and still re-parses losslessly).
    #[tokio::test]
    async fn write_back_html_escapes_like_go() {
        let src = r#"{ "issues": [
          { "id": "H", "identifier": "H", "title": "fix <a> & <b> tags", "state": "Todo" } ] }"#;
        let (tr, src_guard) = new_tracker(src);
        tr.move_issue_state("H", "", "In Progress")
            .await
            .expect("move");
        let text = String::from_utf8(std::fs::read(src_guard.path()).expect("read back"))
            .expect("utf8 output");
        // Build the expected escaped title (e.g. '<' -> the 6 bytes backslash-u-0-0-3-c), matching
        // Go's lowercase 4-hex-digit `\uXXXX` escapes.
        let esc = |c: char| format!("\\u{:04x}", c as u32);
        let want = format!(
            "fix {lt}a{gt} {amp} {lt}b{gt} tags",
            lt = esc('<'),
            gt = esc('>'),
            amp = esc('&')
        );
        assert!(text.contains(&want), "HTML not escaped like Go: {text}");
        // No raw <, >, or & survive (they occur only in the escaped title).
        assert!(
            !text.contains('<') && !text.contains('>') && !text.contains('&'),
            "raw HTML chars left unescaped: {text}"
        );
        // Escaping is lossless: the file re-parses to the original title.
        let doc: Doc = serde_json::from_slice(text.as_bytes()).expect("valid JSON");
        assert_eq!(doc.issues[0].title, "fix <a> & <b> tags", "title corrupted");
    }
}
