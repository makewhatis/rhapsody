//! The capabilities registry — user-editable practices (code review, simplify,
//! deep research, ...) an agent can be told to follow, injected as prompt text.
//! Data, not code, so it can grow without a Rhapsody release. Seeded from
//! [`default_capabilities`] into `~/.rhapsody/capabilities.yaml` on first read.

use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapabilityDef {
    pub name: String,
    pub label: String,
    pub description: String,
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
        CapabilityDef {
            name: "claude-md-maintenance".to_string(),
            label: "CLAUDE.md Maintenance Sweep".to_string(),
            description: "Detect and fix drift in this repo's nested CLAUDE.md files".to_string(),
            instruction: "Read and follow .claude/skills/claude-md-maintenance/SKILL.md in this repo verbatim — it documents the full drift-detection and targeted-fix process for this ticket.".to_string(),
        },
    ]
}

/// Loads `~/.rhapsody/capabilities.yaml`, seeding it with [`default_capabilities`]
/// if it doesn't exist yet.
pub fn load_or_seed(path: &Path) -> Result<Vec<CapabilityDef>, CapabilitiesError> {
    if !path.exists() {
        let defaults = default_capabilities();
        let yaml = serde_yaml_ng::to_string(&defaults)
            .map_err(|e| CapabilitiesError::Parse(e.to_string()))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| CapabilitiesError::Io(e.to_string()))?;
        }
        std::fs::write(path, yaml).map_err(|e| CapabilitiesError::Io(e.to_string()))?;
        return Ok(defaults);
    }
    let text = std::fs::read_to_string(path).map_err(|e| CapabilitiesError::Io(e.to_string()))?;
    serde_yaml_ng::from_str(&text).map_err(|e| CapabilitiesError::Parse(e.to_string()))
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

    #[test]
    fn load_or_seed_reads_existing_file_without_overwriting() {
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
        assert_eq!(loaded, custom);
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

    #[test]
    fn default_capabilities_includes_claude_md_maintenance() {
        let found = default_capabilities()
            .into_iter()
            .find(|c| c.name == "claude-md-maintenance")
            .expect("claude-md-maintenance capability present");
        assert_eq!(found.label, "CLAUDE.md Maintenance Sweep");
        assert!(found.instruction.contains("claude-md-maintenance/SKILL.md"));
    }
}
