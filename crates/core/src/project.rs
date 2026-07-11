//! Tracker project + viewer identity types ported from Go `internal/core/project.go`.

use serde::{Deserialize, Serialize};

/// `Project` is a normalized tracker project for the read-only projects API that powers the
/// Settings add-agent picker (INF-224). `slug` is Linear's project `slugId`; `team` is the
/// owning team's label and `color` is the project's swatch (both rendered by the picker,
/// addendum #5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub slug: String,
    pub team: String,
    pub color: String,
}

/// `Viewer` is the resolved owner of the configured tracker API key — the user whose assigned
/// issues Symphony works. It backs the "connected as" identity surface (INF-224). `name` is the
/// account's full name; `display_name` is the shorter handle Linear shows inline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Viewer {
    pub id: String,
    pub name: String,
    pub display_name: String,
    pub email: String,
    /// `url_key` is the workspace's Linear slug (`organization.urlKey`), used to build issue
    /// deep links like `https://linear.app/<url_key>/issue/<IDENTIFIER>`. Empty if the org
    /// could not be resolved.
    pub url_key: String,
}
