//! The capabilities registry — user-editable practices (code review, simplify,
//! deep research, ...) an agent can be told to follow, injected as prompt text.
//! Data, not code, so it can grow without a Rhapsody release. Seeded from
//! [`default_capabilities`] into `~/.rhapsody/capabilities.yaml` on first read.

use serde::{Deserialize, Serialize};
use std::os::unix::fs::DirBuilderExt;
use std::path::Path;

use crate::workflow::{create_temp, write_temp_and_rename};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapabilityDef {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub instruction: String,
}

#[derive(thiserror::Error, Debug)]
pub enum CapabilitiesError {
    #[error("capabilities_io_error: {0}")]
    Io(String),
    #[error("capabilities_parse_error: {0}")]
    Parse(String),
}

/// The bundled default set. A capability only works if its underlying skill/
/// command is actually available in the target repo's Claude Code environment —
/// Rhapsody can tell the agent to use `/code-review`, but can't make a plugin
/// exist there.
pub fn default_capabilities() -> Vec<CapabilityDef> {
    vec![
        CapabilityDef {
            name: "code-review".to_string(),
            label: "Code Review".to_string(),
            description: "Self-review the diff for bugs before handing off".to_string(),
            instruction: "Before declaring HANDOFF, review your own diff as if reviewing someone else's PR: look for logic errors, edge cases, and regressions you introduced.".to_string(),
        },
        CapabilityDef {
            name: "simplify".to_string(),
            label: "Simplify".to_string(),
            description: "Look for unnecessary complexity before handing off".to_string(),
            instruction: "Before declaring HANDOFF, review your diff for unnecessary abstraction, unused code paths, or complexity beyond what the ticket required.".to_string(),
        },
        CapabilityDef {
            name: "deep-research".to_string(),
            label: "Deep Research".to_string(),
            description: "Investigate unfamiliar APIs/prior art before implementing".to_string(),
            instruction: "If the ticket involves an unfamiliar API, library, or ambiguous requirement, research it thoroughly before writing code rather than guessing at behavior.".to_string(),
        },
        CapabilityDef {
            name: "security-review".to_string(),
            label: "Security Review".to_string(),
            description: "Check the diff for common vulnerability classes before handoff".to_string(),
            instruction: "Before declaring HANDOFF, review your diff for injection, auth, and other OWASP-class issues introduced by the change.".to_string(),
        },
        CapabilityDef {
            name: "test-coverage".to_string(),
            label: "Test Coverage Pass".to_string(),
            description: "Make sure new/changed behavior has tests before handoff".to_string(),
            instruction: "Before declaring HANDOFF, confirm every new or changed behavior has a test exercising it; add tests for any gaps.".to_string(),
        },
        CapabilityDef {
            name: "systematic-debugging".to_string(),
            label: "Systematic Debugging".to_string(),
            description: "For bug-fix tickets, root-cause methodically".to_string(),
            instruction: "If this ticket is a bug fix, reproduce the bug, isolate the root cause, and confirm the fix resolves it — don't pattern-match a plausible patch without confirming the failure mode first.".to_string(),
        },
        CapabilityDef {
            name: "design-first".to_string(),
            label: "Design First".to_string(),
            description: "Sketch a short plan before implementing, for ambiguous/large tickets".to_string(),
            instruction: "If the ticket's scope or approach is ambiguous, write a short plan (files touched, approach, tradeoffs) before writing code.".to_string(),
        },
        CapabilityDef {
            name: "adversarial-verify".to_string(),
            label: "Adversarial Verify".to_string(),
            description: "Try to disprove your own fix before declaring it done".to_string(),
            instruction: "Before declaring HANDOFF, actively try to find a case where your fix does NOT work or your claim is NOT true, rather than assuming success.".to_string(),
        },
        CapabilityDef {
            name: "second-opinion".to_string(),
            label: "Second-Opinion Review".to_string(),
            description: "Review your approach from a fresh angle before committing".to_string(),
            instruction: "Before committing to your implementation approach, step back and evaluate it as if seeing it for the first time — is there a simpler or more robust way to solve this?".to_string(),
        },
    ]
}

/// Loads `~/.rhapsody/capabilities.yaml`, seeding it with [`default_capabilities`]
/// if it doesn't exist yet. On every read, any bundled default whose `name`
/// isn't already present in the file is appended and the merged result is
/// written back — so the registry evolves across upgrades (new bundled
/// capabilities reach existing users, and an empty/truncated file self-heals)
/// while file entries always win: a user's edits to a built-in, or an entirely
/// custom entry, are never overwritten.
pub fn load_or_seed(path: &Path) -> Result<Vec<CapabilityDef>, CapabilitiesError> {
    let defaults = default_capabilities();
    if !path.exists() {
        write_registry(path, &defaults)?;
        return Ok(defaults);
    }
    let text = std::fs::read_to_string(path).map_err(|e| CapabilitiesError::Io(e.to_string()))?;
    let mut loaded: Vec<CapabilityDef> =
        serde_yaml_ng::from_str(&text).map_err(|e| CapabilitiesError::Parse(e.to_string()))?;
    let existing: std::collections::HashSet<&str> =
        loaded.iter().map(|c| c.name.as_str()).collect();
    let missing: Vec<CapabilityDef> = defaults
        .into_iter()
        .filter(|d| !existing.contains(d.name.as_str()))
        .collect();
    if !missing.is_empty() {
        loaded.extend(missing);
        write_registry(path, &loaded)?;
    }
    Ok(loaded)
}

/// Serializes `registry` to YAML and writes it to `path` atomically, reusing
/// the crate's `~/.rhapsody` write convention (temp file + chmod + rename, so
/// a watcher never observes a half-written file — see [`crate::workflow::save`]).
/// The parent directory is created owner-only (0700), matching how the daemon
/// creates `~/.rhapsody` itself: this may be the first code to touch it.
fn write_registry(path: &Path, registry: &[CapabilityDef]) -> Result<(), CapabilitiesError> {
    let yaml =
        serde_yaml_ng::to_string(registry).map_err(|e| CapabilitiesError::Parse(e.to_string()))?;
    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| CapabilitiesError::Io(e.to_string()))?;
    let (file, tmp_path) =
        create_temp(dir, "capabilities").map_err(|e| CapabilitiesError::Io(e.to_string()))?;
    write_temp_and_rename(file, &tmp_path, yaml.as_bytes(), 0o600, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp_path);
        CapabilitiesError::Io(e.to_string())
    })
}

/// Renders the selected capability names (in registry order) into a prompt
/// section. Unknown names (stale/typo'd) are silently skipped — a dangling
/// capability reference is a no-op, never a hard error. Returns an empty
/// string when nothing in `selected` matches the registry (the no-op case the
/// caller checks with `.is_empty()` before prepending).
pub fn render_section(selected: &[String], registry: &[CapabilityDef]) -> String {
    let matched: Vec<&CapabilityDef> = registry
        .iter()
        .filter(|c| selected.iter().any(|s| s == &c.name))
        .collect();
    if matched.is_empty() {
        return String::new();
    }
    let mut out = String::from("## Required practices for this ticket\n\n");
    for c in matched {
        out.push_str(&c.instruction);
        out.push_str("\n\n");
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_or_seed_writes_defaults_when_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("capabilities.yaml");
        assert!(!path.exists());
        let loaded = load_or_seed(&path).expect("seed");
        assert_eq!(loaded, default_capabilities());
        assert!(path.exists());
    }

    /// Exercises the actual bytes written to disk, not just the in-memory
    /// return value: the second call re-reads what the first call wrote,
    /// so a serialized-shape regression (missing key, serializer swap) would
    /// fail here even though the first call's return value never touches disk.
    #[test]
    fn load_or_seed_round_trips_through_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("capabilities.yaml");
        let seeded = load_or_seed(&path).expect("seed");
        let reloaded = load_or_seed(&path).expect("reload from disk");
        assert_eq!(seeded, reloaded);
        assert_eq!(reloaded, default_capabilities());
    }

    #[test]
    fn load_or_seed_keeps_custom_entries_and_appends_missing_defaults() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("capabilities.yaml");
        let custom = vec![CapabilityDef {
            name: "custom".to_string(),
            label: "Custom".to_string(),
            description: "d".to_string(),
            instruction: "i".to_string(),
        }];
        std::fs::write(&path, serde_yaml_ng::to_string(&custom).unwrap()).unwrap();
        let loaded = load_or_seed(&path).expect("load");
        // The custom entry survives, unmodified, in its original position...
        assert_eq!(loaded[0], custom[0]);
        // ...and every bundled default the file was missing gets appended.
        for def in default_capabilities() {
            assert!(
                loaded.iter().any(|c| c.name == def.name),
                "missing default {:?} was not appended",
                def.name
            );
        }
        // The merge is written back — a second load sees the same set, not a
        // growing one.
        let reloaded = load_or_seed(&path).expect("reload");
        assert_eq!(loaded, reloaded);
    }

    /// The scenario `load_or_seed`'s merge-by-name exists for: an
    /// already-seeded file from an older release, missing a capability a
    /// newer `default_capabilities()` added. It must reach existing users
    /// on the next load rather than staying stuck at the seed-time set.
    #[test]
    fn load_or_seed_evolves_an_older_seeded_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("capabilities.yaml");
        let mut stale = default_capabilities();
        let dropped = stale.pop().expect("defaults are non-empty");
        std::fs::write(&path, serde_yaml_ng::to_string(&stale).unwrap()).unwrap();

        let loaded = load_or_seed(&path).expect("load");
        assert!(loaded.iter().any(|c| c.name == dropped.name));
        assert_eq!(loaded.len(), default_capabilities().len());
    }

    /// A present-but-empty file (0 bytes truncated, or genuinely `[]`) must
    /// self-heal back to the full default set rather than yielding zero
    /// practices forever.
    #[test]
    fn load_or_seed_self_heals_an_empty_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("capabilities.yaml");
        std::fs::write(&path, "[]\n").unwrap();
        let loaded = load_or_seed(&path).expect("load");
        assert_eq!(loaded, default_capabilities());
    }

    /// `#[serde(default)]` on every field means a hand-edited entry missing
    /// some keys degrades to empty strings for those keys rather than
    /// rejecting the whole file — matching `render_section`'s own
    /// no-op-over-hard-error contract for the registry.
    #[test]
    fn capability_def_tolerates_partial_entries() {
        let parsed: Vec<CapabilityDef> = serde_yaml_ng::from_str("- name: custom\n").unwrap();
        assert_eq!(parsed[0].name, "custom");
        assert_eq!(parsed[0].label, "");
        assert_eq!(parsed[0].description, "");
        assert_eq!(parsed[0].instruction, "");
    }

    #[test]
    fn capabilities_io_error_has_stable_prefix() {
        let err = CapabilitiesError::Io("boom".to_string());
        assert!(err.to_string().starts_with("capabilities_io_error:"));
    }

    #[test]
    fn capabilities_parse_error_has_stable_prefix() {
        let err = CapabilitiesError::Parse("boom".to_string());
        assert!(err.to_string().starts_with("capabilities_parse_error:"));
    }

    #[test]
    fn render_section_empty_selection_is_noop() {
        let registry = default_capabilities();
        assert_eq!(render_section(&[], &registry), "");
    }

    #[test]
    fn render_section_skips_unknown_names() {
        let registry = default_capabilities();
        let selected = vec!["not-a-real-capability".to_string()];
        assert_eq!(render_section(&selected, &registry), "");
    }

    #[test]
    fn render_section_renders_selected_in_registry_order() {
        let registry = default_capabilities();
        let selected = vec!["simplify".to_string(), "code-review".to_string()];
        let rendered = render_section(&selected, &registry);
        assert!(rendered.starts_with("## Required practices for this ticket"));
        let code_review_pos = rendered.find("review your own diff").unwrap();
        let simplify_pos = rendered.find("unnecessary abstraction").unwrap();
        assert!(
            code_review_pos < simplify_pos,
            "code-review (registry order) must render before simplify"
        );
    }
}
