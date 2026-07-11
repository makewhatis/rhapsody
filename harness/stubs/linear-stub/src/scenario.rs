//! Scenario JSON (v1) — the scripted Linear world the stub serves.
//!
//! Schema (v1), the R3 Interfaces contract:
//! ```json
//! {
//!   "viewer":  { "id": "...", "name": "..." },
//!   "project": { "id": "...", "name": "...", "slugId": "..." },
//!   "issues":  [ { "id": "...", "identifier": "...", "title": "...",
//!                  "description": "...", "state": "...",
//!                  "labels": [], "blockedBy": [] } ]
//! }
//! ```
//! `description`, `labels`, and `blockedBy` are optional (default empty). Every other
//! field is required. Issue `state` is a display name (e.g. "Todo"); it maps to the
//! stub's fixed workflow-state table in `lib.rs`.

use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
pub struct Scenario {
    pub viewer: Viewer,
    pub project: Project,
    pub issues: Vec<Issue>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Viewer {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    #[serde(rename = "slugId")]
    pub slug_id: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Issue {
    pub id: String,
    pub identifier: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    pub state: String,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default, rename = "blockedBy")]
    pub blocked_by: Vec<String>,
}

impl Scenario {
    /// Load a scenario JSON file. Errors (with context) if the path is unreadable or the
    /// JSON does not match the v1 schema.
    pub fn from_path(p: impl AsRef<std::path::Path>) -> anyhow::Result<Self> {
        let path = p.as_ref();
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("read scenario {}: {e}", path.display()))?;
        serde_json::from_str(&text)
            .map_err(|e| anyhow::anyhow!("parse scenario {}: {e}", path.display()))
    }
}
