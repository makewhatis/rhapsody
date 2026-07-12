//! Persists the desktop app's small local preferences (currently the Tool-doctor per-tool path
//! overrides) as JSON under `~/.symphony`, owner-readable only. Parity port of
//! `$REF/desktop/internal/prefs/prefs.go`.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::Path;

/// A boxed std error mirrors Go's opaque `error` return (the desktop crate has no `anyhow`).
type Error = Box<dyn std::error::Error + Send + Sync>;

/// Reads the tool-override map from `path`. A missing file yields an empty map (not an error) — the
/// first-launch case before any override is set. Mirrors Go `LoadToolOverrides`.
pub fn load_tool_overrides(path: &Path) -> Result<HashMap<String, String>, Error> {
    let data = match fs::read(path) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(e) => return Err(Box::new(e)),
    };
    if data.is_empty() {
        return Ok(HashMap::new());
    }
    serde_json::from_slice(&data).map_err(|e| format!("parse {}: {e}", path.display()).into())
}

/// Writes the map to `path` atomically (temp + rename), creating the parent directory and using 0600
/// (the file records local tool paths only — still owner-only). Mirrors Go `SaveToolOverrides`.
pub fn save_tool_overrides(path: &Path, m: &HashMap<String, String>) -> Result<(), Error> {
    // Sort the keys for a deterministic file — Go's `json.Marshal` sorts map keys, and serde_json's
    // pretty printer uses the same 2-space indent as Go's `MarshalIndent(m, "", "  ")`.
    let sorted: BTreeMap<&String, &String> = m.iter().collect();
    let data = serde_json::to_vec_pretty(&sorted).map_err(|e| Box::new(e) as Error)?;
    crate::atomicfile::write_0600(path, &data).map_err(|e| Box::new(e) as Error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_dir() -> std::path::PathBuf {
        let n = DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("rhapsody-d4-prefs-{}-{n}", std::process::id()));
        fs::create_dir_all(&p).expect("create temp dir");
        p
    }

    // Mirrors TestToolOverridesRoundTrip: saved overrides load back identically, and Save creates the
    // parent dir as needed (the prefs live under ~/.symphony).
    #[test]
    fn tool_overrides_round_trip() {
        let dir = temp_dir();
        let path = dir.join("nested").join("tools.json");
        let mut want = HashMap::new();
        want.insert("gh".to_string(), "/opt/homebrew/bin/gh".to_string());
        want.insert(
            "claude".to_string(),
            "/Users/x/.local/bin/claude".to_string(),
        );

        save_tool_overrides(&path, &want).expect("save");
        let got = load_tool_overrides(&path).expect("load");
        assert_eq!(got, want, "round-trip mismatch");

        fs::remove_dir_all(&dir).ok();
    }

    // Mirrors TestLoadMissingIsEmpty: a missing prefs file is not an error — it yields an empty map
    // (first launch, before any override is set).
    #[test]
    fn load_missing_is_empty() {
        let dir = temp_dir();
        let got = load_tool_overrides(&dir.join("absent.json")).expect("load missing");
        assert!(
            got.is_empty(),
            "a missing prefs file must load as empty, got {got:?}"
        );
        fs::remove_dir_all(&dir).ok();
    }
}
