//! WORKFLOW.md loader + save — parity port of Go `internal/workflow`
//! (`loader.go`, `save.go`).
//!
//! A WORKFLOW.md file is optional YAML front matter (delimited by `---` lines)
//! followed by a Markdown prompt body. [`load`] splits the two, parsing the
//! front matter into a [`Definition::config`] map and trimming the body into
//! [`Definition::prompt_template`]. [`save`] / [`marshal`] are the inverse.
//!
//! The sentinel error strings ([`WorkflowError`]) are part of the observable
//! contract — they surface in daemon logs and the config API — so they Display
//! the exact tokens the Go daemon emits.

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_yaml_ng::Value;

/// Front-matter root map type (a YAML mapping). Aliased so consumers name the
/// map without depending on the concrete YAML crate.
pub type YamlMap = serde_yaml_ng::Mapping;

/// A parsed WORKFLOW.md (Go `workflow.Definition`).
#[derive(Debug, Clone, PartialEq)]
pub struct Definition {
    /// Front-matter root object (an empty map when the file has no front matter).
    pub config: YamlMap,
    /// Trimmed Markdown body — the prompt template.
    pub prompt_template: String,
}

/// Errors from [`load`] / [`marshal`]. Display strings byte-match Go's sentinels
/// (`missing_workflow_file`, `workflow_parse_error`, `workflow_front_matter_not_a_map`).
#[derive(thiserror::Error, Debug)]
pub enum WorkflowError {
    /// The workflow file could not be read (missing, unreadable, ...). Mirrors
    /// Go's `ErrMissingWorkflowFile`: any `os.ReadFile` failure maps here.
    #[error("missing_workflow_file")]
    MissingWorkflowFile,
    /// The front matter is not valid YAML. Wraps the parser message, mirroring
    /// Go's `fmt.Errorf("%w: %w", ErrWorkflowParse, err)`.
    #[error("workflow_parse_error: {0}")]
    Parse(String),
    /// The front matter parsed to something other than a map (Go
    /// `ErrFrontMatterNotMap`).
    #[error("workflow_front_matter_not_a_map")]
    FrontMatterNotAMap,
}

/// Reads and parses the workflow file at `path` (Go `workflow.Load`).
pub fn load(path: &Path) -> Result<Definition, WorkflowError> {
    // Any read failure (missing, permission, is-a-dir, ...) maps to
    // MissingWorkflowFile, exactly as loader.go wraps every os.ReadFile error.
    let data = fs::read_to_string(path).map_err(|_| WorkflowError::MissingWorkflowFile)?;
    let Some((front, body)) = split_front_matter(&data) else {
        // No front matter: the whole (trimmed) file is the prompt body.
        return Ok(Definition {
            config: YamlMap::new(),
            prompt_template: data.trim().to_string(),
        });
    };
    let root: Value =
        serde_yaml_ng::from_str(&front).map_err(|e| WorkflowError::Parse(e.to_string()))?;
    match root {
        // Empty front matter (e.g. "---\n---\nbody") decodes to null; treat it
        // as an empty config map rather than a non-map error.
        Value::Null => Ok(Definition {
            config: YamlMap::new(),
            prompt_template: body.trim().to_string(),
        }),
        // Only a string-keyed mapping is a config. This mirrors loader.go's
        // `root.(map[string]any)` assertion: yaml.v3 decodes a mapping into an
        // `interface{}` as `map[string]any` only when every key is a string
        // (its `isStringMap`), otherwise as `map[interface{}]interface{}`,
        // which fails the assertion → ErrFrontMatterNotMap.
        Value::Mapping(config) if config.keys().all(Value::is_string) => Ok(Definition {
            config,
            prompt_template: body.trim().to_string(),
        }),
        _ => Err(WorkflowError::FrontMatterNotAMap),
    }
}

/// Splits `s` into `(front_matter, body)`. Front matter exists only when the
/// first line is exactly `---`; it ends at the next `---` line. Returns `None`
/// when there is no front matter (first line not `---`, or no closing `---`).
///
/// Mirrors loader.go `splitFrontMatter`: `str::lines()` reproduces Go's
/// `bufio.ScanLines` (split on `\n`, drop one trailing `\r`), and the extra
/// `trim_end_matches('\r')` matches Go's `strings.TrimRight(_, "\r")` on the
/// delimiter comparison.
fn split_front_matter(s: &str) -> Option<(String, String)> {
    let mut lines = s.lines();
    match lines.next() {
        Some(first) if first.trim_end_matches('\r') == "---" => {}
        _ => return None,
    }
    let mut front = String::new();
    let mut body = String::new();
    let mut in_front = true;
    for line in lines {
        if in_front && line.trim_end_matches('\r') == "---" {
            in_front = false;
            continue;
        }
        if in_front {
            front.push_str(line);
            front.push('\n');
        } else {
            body.push_str(line);
            body.push('\n');
        }
    }
    // Never saw a closing `---` → treat the whole file as having no front matter.
    if in_front {
        return None;
    }
    Some((front, body))
}

/// Serializes a [`Definition`] back into WORKFLOW.md form (Go `workflow.Marshal`):
/// YAML front matter delimited by `---` lines, followed by the prompt body. The
/// output always opens with `---` so [`load`] recognizes the front matter. An
/// empty `config` emits an empty front-matter block (`---\n---\n`) rather than a
/// literal `{}`, so a body-only definition round-trips cleanly.
pub fn marshal(def: &Definition) -> Result<Vec<u8>, WorkflowError> {
    let mut out = String::from("---\n");
    if !def.config.is_empty() {
        let front = serde_yaml_ng::to_string(&def.config)
            .map_err(|e| WorkflowError::Parse(e.to_string()))?;
        out.push_str(&front); // to_string output already ends in a newline
    }
    out.push_str("---\n");
    let body = def.prompt_template.trim_end_matches('\n');
    if !body.is_empty() {
        out.push_str(body);
        out.push('\n');
    }
    Ok(out.into_bytes())
}

/// Writes `def` to `path` in WORKFLOW.md form, atomically (Go `workflow.Save`):
/// marshal, write a sibling temp file, then rename it over `path` so a file
/// watcher observes one clean replacement (never a half-written file). An
/// existing file's mode is preserved; otherwise 0600 keeps the config (which may
/// carry a `$LINEAR_API_KEY` indirection) owner-only.
pub fn save(path: &Path, def: &Definition) -> io::Result<()> {
    let data =
        marshal(def).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    let perm = fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o777)
        .unwrap_or(0o600);
    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let (file, tmp_path) = create_temp(dir)?;
    match write_temp_and_rename(file, &tmp_path, &data, perm, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            // Best-effort cleanup if we bailed before the rename succeeded.
            let _ = fs::remove_file(&tmp_path);
            Err(e)
        }
    }
}

/// Process-global counter feeding unique temp-file names in [`create_temp`].
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Creates a uniquely named `.workflow-*.tmp` file in `dir`, owner-only (0600),
/// mirroring Go's `os.CreateTemp(dir, ".workflow-*.tmp")`.
fn create_temp(dir: &Path) -> io::Result<(fs::File, PathBuf)> {
    let pid = std::process::id();
    for _ in 0..10_000u64 {
        let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let candidate = dir.join(format!(".workflow-{pid}-{n}.tmp"));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&candidate)
        {
            Ok(file) => return Ok((file, candidate)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not create a unique temp file",
    ))
}

/// Writes `data` to the temp file, chmods it to `perm`, then renames it over
/// `dest`. The temp handle is closed before chmod/rename, mirroring save.go.
fn write_temp_and_rename(
    mut file: fs::File,
    tmp_path: &Path,
    data: &[u8],
    perm: u32,
    dest: &Path,
) -> io::Result<()> {
    file.write_all(data)?;
    drop(file); // close before chmod + rename
    fs::set_permissions(tmp_path, fs::Permissions::from_mode(perm))?;
    fs::rename(tmp_path, dest)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    static TEST_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// RAII temp directory mirroring Go's `t.TempDir()` (unique, auto-removed).
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new() -> TempDir {
            let n = TEST_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "rhapsody-workflow-{}-{}",
                std::process::id(),
                n
            ));
            fs::create_dir_all(&path).unwrap();
            TempDir { path }
        }

        fn join(&self, name: &str) -> PathBuf {
            self.path.join(name)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    /// Mirror of loader_test.go `write`: write `content` to WORKFLOW.md, return path.
    fn write_workflow(dir: &TempDir, content: &str) -> PathBuf {
        let p = dir.join("WORKFLOW.md");
        fs::write(&p, content).unwrap();
        p
    }

    /// Build a YAML mapping from `(key, value)` pairs (test ergonomics).
    fn map(pairs: Vec<(&str, Value)>) -> YamlMap {
        pairs
            .into_iter()
            .map(|(k, v)| (Value::from(k), v))
            .collect()
    }

    // ---- loader_test.go mirrors ----

    #[test]
    fn load_missing_file() {
        let dir = TempDir::new();
        let err = load(&dir.join("nope.md")).unwrap_err();
        assert!(
            matches!(err, WorkflowError::MissingWorkflowFile),
            "got {err:?}, want MissingWorkflowFile"
        );
    }

    #[test]
    fn load_no_front_matter() {
        let dir = TempDir::new();
        let def = load(&write_workflow(&dir, "  Just a prompt body.  \n")).unwrap();
        assert!(
            def.config.is_empty(),
            "config should be empty, got {:?}",
            def.config
        );
        assert_eq!(def.prompt_template, "Just a prompt body.");
    }

    #[test]
    fn load_with_front_matter() {
        let dir = TempDir::new();
        let src = "---\ntracker:\n  kind: linear\npolling:\n  interval_ms: 5000\n---\nDo the work for {{ issue.identifier }}.\n";
        let def = load(&write_workflow(&dir, src)).unwrap();
        let kind = def
            .config
            .get("tracker")
            .and_then(Value::as_mapping)
            .and_then(|m| m.get("kind"))
            .and_then(Value::as_str);
        assert_eq!(
            kind,
            Some("linear"),
            "front matter not parsed: {:?}",
            def.config
        );
        assert_eq!(
            def.prompt_template,
            "Do the work for {{ issue.identifier }}."
        );
    }

    #[test]
    fn load_empty_front_matter() {
        let dir = TempDir::new();
        let def = load(&write_workflow(&dir, "---\n---\nbody\n")).unwrap();
        assert!(
            def.config.is_empty(),
            "config should be empty non-nil map, got {:?}",
            def.config
        );
        assert_eq!(def.prompt_template, "body");
    }

    #[test]
    fn load_front_matter_not_map() {
        let dir = TempDir::new();
        let err = load(&write_workflow(&dir, "---\n- a\n- b\n---\nbody\n")).unwrap_err();
        assert!(
            matches!(err, WorkflowError::FrontMatterNotAMap),
            "got {err:?}, want FrontMatterNotAMap"
        );
    }

    #[test]
    fn load_front_matter_non_string_keys() {
        // A mapping with non-string keys decodes to map[interface{}]interface{}
        // in Go (yaml.v3 `isStringMap` == false), which loader.go's
        // `root.(map[string]any)` rejects as not-a-map. Mirror that.
        let dir = TempDir::new();
        let err = load(&write_workflow(&dir, "---\n1: x\n2: y\n---\nbody\n")).unwrap_err();
        assert!(
            matches!(err, WorkflowError::FrontMatterNotAMap),
            "got {err:?}, want FrontMatterNotAMap"
        );
    }

    #[test]
    fn load_front_matter_parse_error() {
        let dir = TempDir::new();
        let err = load(&write_workflow(&dir, "---\nkey: : bad\n---\nbody\n")).unwrap_err();
        assert!(
            matches!(err, WorkflowError::Parse(_)),
            "got {err:?}, want Parse"
        );
    }

    // ---- save_test.go mirrors ----

    #[test]
    fn marshal_load_round_trip() {
        let def = Definition {
            config: map(vec![
                (
                    "tracker",
                    Value::Mapping(map(vec![
                        ("kind", Value::from("linear")),
                        ("project_slug", Value::from("symphony")),
                        (
                            "active_states",
                            Value::Sequence(vec![Value::from("Todo"), Value::from("In Progress")]),
                        ),
                    ])),
                ),
                (
                    "agent",
                    Value::Mapping(map(vec![
                        ("backend", Value::from("claude")),
                        ("max_concurrent_agents", Value::from(2_i64)),
                    ])),
                ),
            ]),
            prompt_template: "Do the work for {{ issue.identifier }}.\n\nStep 1: read the ticket."
                .to_string(),
        };

        let data = marshal(&def).unwrap();
        // The serialized form must be re-parseable by load (leading --- recognized).
        assert!(
            data.starts_with(b"---\n"),
            "serialized form does not start with front-matter delimiter:\n{}",
            String::from_utf8_lossy(&data)
        );

        let dir = TempDir::new();
        let path = dir.join("WORKFLOW.md");
        fs::write(&path, &data).unwrap();
        let got = load(&path).unwrap();
        assert_eq!(got.config, def.config, "Config round-trip mismatch");
        assert_eq!(
            got.prompt_template, def.prompt_template,
            "PromptTemplate round-trip mismatch"
        );
    }

    #[test]
    fn marshal_empty_config_round_trips() {
        let def = Definition {
            config: YamlMap::new(),
            prompt_template: "just a body".to_string(),
        };
        let data = marshal(&def).unwrap();
        let dir = TempDir::new();
        let path = dir.join("WORKFLOW.md");
        fs::write(&path, &data).unwrap();
        let got = load(&path).unwrap();
        assert!(
            got.config.is_empty(),
            "Config = {:?}, want empty",
            got.config
        );
        assert_eq!(got.prompt_template, "just a body");
    }

    #[test]
    fn save_persists_and_reloads() {
        let dir = TempDir::new();
        let path = dir.join("WORKFLOW.md");
        fs::write(&path, "---\ntracker:\n  kind: linear\n---\nold body\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        let def = Definition {
            config: map(vec![(
                "tracker",
                Value::Mapping(map(vec![
                    ("kind", Value::from("linear")),
                    ("project_slug", Value::from("sym")),
                ])),
            )]),
            prompt_template: "new body".to_string(),
        };
        save(&path, &def).unwrap();
        let got = load(&path).unwrap();
        assert_eq!(got.config, def.config, "Config mismatch after Save");
        assert_eq!(got.prompt_template, "new body");
        // Mode of the existing file is preserved (0600 secrets posture not widened).
        let perm = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(perm, 0o600, "mode = {perm:o}, want 600 (preserved)");
    }

    // ---- error contract ----

    #[test]
    fn error_display_strings_match_go_sentinels() {
        assert_eq!(
            WorkflowError::MissingWorkflowFile.to_string(),
            "missing_workflow_file"
        );
        assert_eq!(
            WorkflowError::Parse("boom".to_string()).to_string(),
            "workflow_parse_error: boom"
        );
        assert_eq!(
            WorkflowError::FrontMatterNotAMap.to_string(),
            "workflow_front_matter_not_a_map"
        );
    }
}
