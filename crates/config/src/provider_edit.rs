//! `provider_edit` — byte-preserving add/edit/remove of one top-level `providers:` definition
//! (STUDIO-1048). **Rhapsody-only** (the frozen Go reference has no provider concept).
//!
//! The Settings → Providers screen lets an operator author provider definitions without
//! hand-editing WORKFLOW.md. The operator's file carries dated explanatory comments and
//! hot-reloads, so the write MUST NOT re-serialize it: `crate::encode` + `workflow::save` would
//! reformat (and drop every comment in) the whole front matter. Instead [`apply_provider_edit`]
//! splices ONLY the top-level `providers:` block, leaving every other byte of the file — body
//! included — exactly as it was.
//!
//! The one YAML block written is produced by `encode::provider_definition_value`, i.e. the same
//! emit-only-when-non-default rules a full `encode` uses, so an operator file edited here stays
//! canonical. The daemon's own load pipeline (`decode` → `resolve` → `validate`) remains the only
//! validator: this module never second-guesses a provider definition, and the HTTP layer validates
//! the spliced result through [`crate::validate`].

use std::collections::BTreeMap;

use serde_yaml_ng::{Mapping, Value};

use crate::model::Config;
use crate::providers::{ProviderDefinition, canonical_provider_id};
use crate::teams::Teams;

/// Which mutation the caller wants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderOp {
    Add,
    Edit,
    Remove,
}

/// A single reference holding a provider id, so a removal can name everything still selecting it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderReference {
    /// A stable machine kind (`global`, `manager`, `roster`, `review`, `profile`) the UI can group
    /// on without parsing the label.
    pub kind: String,
    /// The human description shown to the operator, e.g. `roster entry "jerry"`.
    pub label: String,
}

/// Why an edit was refused. Every variant carries the daemon's own actionable text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditError {
    /// The id is not a canonical provider id (`canonical_provider_id`).
    InvalidId(String),
    /// The provider id to edit/remove is not defined.
    NotFound(String),
    /// Adding an id that already exists (or renaming onto one).
    AlreadyExists(String),
    /// The existing `providers:` value is present but not a mapping, so it cannot be edited
    /// without destroying operator content.
    NotAMap,
    /// The spliced YAML failed to serialize.
    Serialize(String),
}

impl std::fmt::Display for EditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EditError::InvalidId(reason) => write!(f, "invalid provider id: {reason}"),
            EditError::NotFound(id) => write!(f, "provider {id:?} is not defined"),
            EditError::AlreadyExists(id) => write!(f, "provider {id:?} is already defined"),
            EditError::NotAMap => write!(
                f,
                "the existing providers: block is not a mapping and cannot be edited safely"
            ),
            EditError::Serialize(reason) => write!(f, "could not serialize providers: {reason}"),
        }
    }
}

impl std::error::Error for EditError {}

/// The byte span of the front-matter region: `(front_start, front_end, body_start)`. `front_start`
/// is just after the opening `---\n`; `front_end` is the start of the closing `---` line;
/// `body_start` is just after that line. `None` when the file has no (complete) front matter.
fn front_matter_bounds(text: &str) -> Option<(usize, usize, usize)> {
    let first_end = text.find('\n')?;
    if text[..first_end].trim_end_matches('\r') != "---" {
        return None;
    }
    let front_start = first_end + 1;
    let mut pos = front_start;
    loop {
        let (line_end, next) = match text[pos..].find('\n') {
            Some(i) => (pos + i, pos + i + 1),
            None => (text.len(), text.len()),
        };
        if text[pos..line_end].trim_end_matches('\r') == "---" {
            return Some((front_start, pos, next));
        }
        if next == pos {
            return None; // no closing `---`
        }
        pos = next;
    }
}

/// The byte span (within `front`) of the top-level `providers:` key plus its indented block. A
/// column-0 `providers:` line starts the block; every following blank or indented line belongs to
/// it; the first non-blank non-indented line ends it.
fn providers_block_span(front: &str) -> Option<(usize, usize)> {
    let mut start = None;
    let mut end = 0usize;
    let mut pos = 0usize;
    for chunk in front.split_inclusive('\n') {
        let content = chunk.trim_end_matches('\n').trim_end_matches('\r');
        let next = pos + chunk.len();
        match start {
            None => {
                if content.starts_with("providers:") {
                    start = Some(pos);
                    end = next;
                }
            }
            Some(_) => {
                let blank = content.trim().is_empty();
                let indented = content.starts_with(' ') || content.starts_with('\t');
                if blank || indented {
                    end = next;
                } else {
                    return start.map(|s| (s, end));
                }
            }
        }
        pos = next;
    }
    start.map(|s| (s, end))
}

/// Serialize a provider map as a `providers:` block (two-space indented), or `""` for an empty map
/// (the caller then drops the block entirely).
fn serialize_providers_block(map: &BTreeMap<String, Value>) -> Result<String, EditError> {
    if map.is_empty() {
        return Ok(String::new());
    }
    let mut mapping = Mapping::new();
    for (id, value) in map {
        mapping.insert(Value::String(id.clone()), value.clone());
    }
    let inner = serde_yaml_ng::to_string(&Value::Mapping(mapping))
        .map_err(|e| EditError::Serialize(e.to_string()))?;
    let mut out = String::from("providers:\n");
    for line in inner.lines() {
        out.push_str("  ");
        out.push_str(line);
        out.push('\n');
    }
    Ok(out)
}

/// One provider definition as its canonical pruned YAML value, with `EditError` wrapping.
fn provider_value(def: &ProviderDefinition) -> Result<Value, EditError> {
    crate::encode::provider_definition_value(def).map_err(EditError::Serialize)
}

/// The current top-level `providers:` map parsed from a front-matter string, or `None` when the key
/// is absent. `Err(NotAMap)` when the key is present but not a mapping.
fn providers_map(front: &str) -> Result<BTreeMap<String, Value>, EditError> {
    let root: Value = if front.trim().is_empty() {
        Value::Null
    } else {
        serde_yaml_ng::from_str(front).map_err(|e| EditError::Serialize(e.to_string()))?
    };
    let map = match root {
        Value::Null => Mapping::new(),
        Value::Mapping(m) => m,
        _ => return Err(EditError::NotAMap),
    };
    let Some(value) = map.get(Value::String("providers".to_string())) else {
        return Ok(BTreeMap::new());
    };
    match value {
        Value::Mapping(m) => {
            let mut out = BTreeMap::new();
            for (k, v) in m {
                let Value::String(key) = k else {
                    return Err(EditError::NotAMap);
                };
                out.insert(key.clone(), v.clone());
            }
            Ok(out)
        }
        Value::Null => Ok(BTreeMap::new()),
        _ => Err(EditError::NotAMap),
    }
}

/// Apply an add/edit/remove to `text`, returning the new file contents. Every byte outside the
/// top-level `providers:` block is preserved exactly; a file with no front matter gains one whose
/// body is the original text unchanged. `definition` is required for add/edit and ignored for
/// remove. `previous_id` renames an existing key on edit (default: `id`).
pub fn apply_provider_edit(
    text: &str,
    op: ProviderOp,
    id: &str,
    previous_id: Option<&str>,
    definition: Option<&ProviderDefinition>,
) -> Result<String, EditError> {
    canonical_provider_id(id).map_err(EditError::InvalidId)?;
    let key = previous_id.unwrap_or(id).to_string();
    if previous_id.is_some() {
        canonical_provider_id(&key).map_err(EditError::InvalidId)?;
    }

    let bounds = front_matter_bounds(text);
    let front = match bounds {
        Some((s, e, _)) => &text[s..e],
        // No front matter yet: an add can create one; anything else has nothing to edit.
        None => {
            if op == ProviderOp::Add {
                let mut map = BTreeMap::new();
                let def = definition.ok_or_else(|| EditError::NotFound(id.to_string()))?;
                map.insert(id.to_string(), provider_value(def)?);
                let block = serialize_providers_block(&map)?;
                return Ok(format!("---\n{block}---\n{text}"));
            }
            return Err(EditError::NotFound(key));
        }
    };

    let mut map = providers_map(front)?;
    match op {
        ProviderOp::Add => {
            if map.contains_key(id) {
                return Err(EditError::AlreadyExists(id.to_string()));
            }
            let def = definition.ok_or_else(|| EditError::NotFound(id.to_string()))?;
            map.insert(id.to_string(), provider_value(def)?);
        }
        ProviderOp::Edit => {
            if !map.contains_key(&key) {
                return Err(EditError::NotFound(key));
            }
            if id != key && map.contains_key(id) {
                return Err(EditError::AlreadyExists(id.to_string()));
            }
            let def = definition.ok_or_else(|| EditError::NotFound(id.to_string()))?;
            let value = provider_value(def)?;
            map.remove(&key);
            map.insert(id.to_string(), value);
        }
        ProviderOp::Remove => {
            if map.remove(&key).is_none() {
                return Err(EditError::NotFound(key));
            }
        }
    }

    let block = serialize_providers_block(&map)?;
    let (front_start, front_end, _) = bounds.expect("bounds checked above");
    let new_front = match providers_block_span(front) {
        Some((bs, be)) => {
            let mut s = String::with_capacity(front.len() + block.len());
            s.push_str(&front[..bs]);
            s.push_str(&block);
            s.push_str(&front[be..]);
            s
        }
        None => {
            let mut s = String::with_capacity(front.len() + block.len());
            s.push_str(front);
            s.push_str(&block);
            s
        }
    };
    let mut out = String::with_capacity(text.len() + new_front.len());
    out.push_str(&text[..front_start]);
    out.push_str(&new_front);
    out.push_str(&text[front_end..]);
    Ok(out)
}

/// Every place `id` is selected, in a stable order. `config` is the resolved workflow; `teams` is
/// the loaded `teams.yaml` (when Teams is on); `profiles` is `(profile name, provider)` for every
/// user profile file that sets one. The daemon computes this so the client never has to duplicate
/// the selection surfaces.
pub fn provider_references(
    id: &str,
    config: &Config,
    teams: Option<&Teams>,
    profiles: &[(String, String)],
) -> Vec<ProviderReference> {
    let mut out = Vec::new();
    if config.agent.provider == id {
        out.push(ProviderReference {
            kind: "global".to_string(),
            label: "the global default (agent.provider)".to_string(),
        });
    }
    if let Some(teams) = teams {
        if teams.manager.provider == id {
            out.push(ProviderReference {
                kind: "manager".to_string(),
                label: "the manager (manager.provider)".to_string(),
            });
        }
        for identity in &teams.roster {
            if identity.provider == id {
                out.push(ProviderReference {
                    kind: "roster".to_string(),
                    label: format!("roster entry {:?}", identity.name),
                });
            }
        }
        for (harness, value) in teams.review.provider.resolved("") {
            if value == id {
                let label = if harness.is_empty() {
                    "the review override (review.provider)".to_string()
                } else {
                    format!("the review override (review.provider.{harness})")
                };
                out.push(ProviderReference {
                    kind: "review".to_string(),
                    label,
                });
            }
        }
    }
    for (name, provider) in profiles {
        if provider == id {
            out.push(ProviderReference {
                kind: "profile".to_string(),
                label: format!("profile {name:?}"),
            });
        }
    }
    out
}

/// Every user profile file in `dir` that sets a provider, as `(profile name, provider)`. Unreadable
/// or unresolved profiles are skipped (best-effort, matching the daemon's degradable-profile
/// stance); a provider-naming profile that fails to resolve is not a removal blocker it can name.
pub fn profile_providers(dir: &std::path::Path) -> Vec<(String, String)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if let Ok(resolved) = crate::profiles::resolve(dir, name)
            && !resolved.provider.is_empty()
        {
            out.push((name.to_string(), resolved.provider));
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::BrokerLimits;
    use crate::workflow::load;

    /// A comment-rich operator file — exactly the thing a re-serializing write would destroy.
    /// A raw literal (NOT `\` line continuations, which strip the next line's indentation).
    const COMMENT_RICH: &str = r"---
# why this file exists
tracker:
  kind: linear
  api_key: $LINEAR_API_KEY
  project_slug: symphony
agent:
  backend: opencode
# a dated note: this timeout was raised after the Oct incident
  max_turns: 20
providers:
  # the old one, kept for reference
  legacy:
    protocol: openai-compatible
    base_url: https://legacy.example/v1
    credential:
      source: keychain
claude:
  # another dated note next to a tunable
  turn_timeout_ms: 1800000
---
Do the work for {{ issue.identifier }}.
";

    fn def(id: &str, base_url: &str) -> ProviderDefinition {
        ProviderDefinition {
            id: id.to_string(),
            protocol: crate::providers::PROTOCOL_OPENAI_COMPATIBLE.to_string(),
            display_name: "Fireworks".to_string(),
            base_url: base_url.to_string(),
            allow_insecure_http: false,
            credential: Default::default(),
            broker_limits: BrokerLimits::default(),
        }
    }

    /// The set of lines with the `providers:` block removed — the "everything else" a byte-preserving
    /// write must leave untouched.
    fn without_providers_block(text: &str) -> String {
        let Some((front_start, front_end, body_start)) = front_matter_bounds(text) else {
            return text.to_string();
        };
        let front = &text[front_start..front_end];
        let stripped = match providers_block_span(front) {
            Some((bs, be)) => {
                let mut s = front.to_string();
                s.replace_range(bs..be, "");
                s
            }
            None => front.to_string(),
        };
        format!(
            "{}{}{}",
            &text[..front_start],
            stripped,
            &text[body_start..]
        )
    }

    // MUTATION GUARD: if the write re-serializes the whole file (encode + workflow::save), the
    // comments below vanish and this test turns red. It must stay red under that mutation.
    #[test]
    fn add_preserves_every_other_line_byte_for_byte() {
        let text = apply_provider_edit(
            COMMENT_RICH,
            ProviderOp::Add,
            "fireworks",
            None,
            Some(&def("fireworks", "https://api.fireworks.ai/inference/v1")),
        )
        .expect("add");
        assert!(
            text.contains("# a dated note: this timeout was raised after the Oct incident"),
            "comment lost:\n{text}"
        );
        // Comments OUTSIDE the edited block survive (the block itself is the thing being rewritten).
        assert_eq!(
            without_providers_block(&text),
            without_providers_block(COMMENT_RICH),
            "the add rewrote bytes outside the providers block"
        );
        // The new definition is present and canonical.
        let reloaded = load_from(&text);
        assert!(text.contains("fireworks:"), "{text}");
        assert!(
            text.contains("base_url: https://api.fireworks.ai/inference/v1"),
            "{text}"
        );
        assert!(!reloaded.is_empty());
    }

    #[test]
    fn edit_rewrites_only_the_block() {
        let text = apply_provider_edit(
            COMMENT_RICH,
            ProviderOp::Edit,
            "legacy",
            None,
            Some(&def("legacy", "https://new.example/inference/v1")),
        )
        .expect("edit");
        assert!(
            text.contains("base_url: https://new.example/inference/v1"),
            "{text}"
        );
        assert!(!text.contains("https://legacy.example/v1"), "{text}");
        assert_eq!(
            without_providers_block(&text),
            without_providers_block(COMMENT_RICH)
        );
    }

    #[test]
    fn remove_drops_only_the_block() {
        let text = apply_provider_edit(COMMENT_RICH, ProviderOp::Remove, "legacy", None, None)
            .expect("remove");
        assert!(!text.contains("providers:"), "{text}");
        assert!(!text.contains("legacy.example"), "{text}");
        assert_eq!(
            without_providers_block(&text),
            without_providers_block(COMMENT_RICH)
        );
    }

    #[test]
    fn remove_refuses_an_unknown_provider() {
        let err = apply_provider_edit(COMMENT_RICH, ProviderOp::Remove, "nope", None, None)
            .expect_err("must refuse");
        assert_eq!(err, EditError::NotFound("nope".to_string()));
    }

    #[test]
    fn add_refuses_a_duplicate() {
        let err = apply_provider_edit(
            COMMENT_RICH,
            ProviderOp::Add,
            "legacy",
            None,
            Some(&def("legacy", "https://x/v1")),
        )
        .expect_err("must refuse");
        assert_eq!(err, EditError::AlreadyExists("legacy".to_string()));
    }

    #[test]
    fn add_refuses_a_non_canonical_id() {
        let err = apply_provider_edit(
            COMMENT_RICH,
            ProviderOp::Add,
            "Not Canonical",
            None,
            Some(&def("Not Canonical", "https://x/v1")),
        )
        .expect_err("must refuse");
        assert!(matches!(err, EditError::InvalidId(_)), "{err}");
    }

    /// A file with no front matter gains one, and the original bytes become the body unchanged.
    #[test]
    fn add_to_a_front_matterless_file_preserves_the_body() {
        let body = "Just a prompt for {{ issue.identifier }}.\n";
        let text = apply_provider_edit(
            body,
            ProviderOp::Add,
            "fireworks",
            None,
            Some(&def("fireworks", "https://api.fireworks.ai/inference/v1")),
        )
        .expect("add");
        assert!(text.ends_with(body), "{text}");
        assert!(text.starts_with("---\nproviders:\n"), "{text}");
    }

    #[test]
    fn add_emits_insecure_http_only_when_true() {
        let mut insecure = def("plain", "http://plain.example/v1");
        insecure.allow_insecure_http = true;
        let text = apply_provider_edit(
            COMMENT_RICH,
            ProviderOp::Add,
            "plain",
            None,
            Some(&insecure),
        )
        .expect("add");
        assert!(text.contains("allow_insecure_http: true"), "{text}");

        // An https provider never carries the opt-in.
        let text = apply_provider_edit(
            COMMENT_RICH,
            ProviderOp::Add,
            "plain",
            None,
            Some(&def("plain", "https://plain.example/v1")),
        )
        .expect("add");
        assert!(!text.contains("allow_insecure_http"), "{text}");
    }

    #[test]
    fn references_name_every_selection_surface() {
        let front = serde_yaml_ng::from_str::<Mapping>(concat!(
            "tracker:\n  kind: linear\n  api_key: tok\n",
            "agent:\n  backend: opencode\n  provider: fireworks\n  model: m\n",
        ))
        .expect("front");
        let config = crate::decode::decode(&crate::workflow::Definition {
            config: front,
            prompt_template: String::new(),
        })
        .expect("decode");
        let teams: Teams = serde_yaml_ng::from_str(concat!(
            "manager:\n  harness: opencode\n  provider: fireworks\n  model: m\n",
            "review:\n  provider:\n    opencode: fireworks\n",
            "roster:\n  - name: jerry\n    profile: swe\n    provider: fireworks\n",
        ))
        .expect("teams");
        let profiles = vec![("swe".to_string(), "fireworks".to_string())];

        let refs = provider_references("fireworks", &config, Some(&teams), &profiles);
        let labels: Vec<&str> = refs.iter().map(|r| r.label.as_str()).collect();
        assert!(
            labels.contains(&"the global default (agent.provider)"),
            "{labels:?}"
        );
        assert!(
            labels.contains(&"the manager (manager.provider)"),
            "{labels:?}"
        );
        assert!(
            labels.contains(&"the review override (review.provider.opencode)"),
            "{labels:?}"
        );
        assert!(
            labels.iter().any(|l| l.contains("roster entry")),
            "{labels:?}"
        );
        assert!(
            labels.iter().any(|l| l.contains("profile \"swe\"")),
            "{labels:?}"
        );

        // A provider nothing selects has no references.
        assert!(provider_references("unused", &config, Some(&teams), &profiles).is_empty());
    }

    fn load_from(text: &str) -> String {
        // Round-trip through the real loader so the spliced YAML is proven parseable. The scratch
        // dir is removed before returning — the CI temp-leak gate (STUDIO-1031) reds on any
        // `rhapsody-*` dir a test leaves behind.
        let dir =
            std::env::temp_dir().join(format!("rhapsody-provider-edit-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("WORKFLOW.md");
        std::fs::write(&path, text).expect("write");
        let got = load(&path).map(|d| d.prompt_template).unwrap_or_default();
        let _ = std::fs::remove_dir_all(&dir);
        got
    }
}
