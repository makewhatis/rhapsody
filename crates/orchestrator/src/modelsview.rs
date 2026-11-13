//! modelsview — the Settings → Models view (STUDIO-1976). Rhapsody-only; no Go counterpart.
//!
//! This is the structured half of `rhapsodyd teams show`: the same pure resolver
//! ([`resolve_selection`](crate::selection::resolve_selection) /
//! [`resolve_manager_selection`](crate::selection::resolve_manager_selection)) the dispatch path
//! uses, so the page can never disagree with what a run would actually select — the ticket's
//! "do not recompute a second selection algorithm in UI/CLI".
//!
//! It returns, per teammate, the RESOLVED harness/provider/model with each field's origin (the tuple
//! a dispatched run gets) beside the profile front matter the page edits, plus the review overrides
//! per harness and the manager's own tuple. It performs no I/O and returns no credential.

use std::path::Path;

use rhapsody_config::Config;
use rhapsody_config::profiles::ResolvedProfile;
use rhapsody_config::providers::provider_turn_deadline_ms;
use rhapsody_config::teams::Teams;
use serde::{Deserialize, Serialize};

use crate::selection::{
    FieldSelection, ResolvedSelection, SelectionRefusal, SelectionRequest, SelectionTiers,
    resolve_manager_selection, resolve_selection,
};

/// The harnesses THIS build can actually run — `agent.backend`'s implemented set, not
/// [`HARNESS_NAMES`](rhapsody_config::harness::HARNESS_NAMES) (which also admits `codex`, a name
/// config accepts but no runner backs).
pub const RUNNABLE_HARNESSES: &[&str] = &["claude", "opencode"];

/// One teammate's routing tuple: the resolved harness/provider/model with each field's origin, or
/// the refusal the resolver returned.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TupleView {
    pub harness: String,
    pub harness_origin: String,
    pub provider: String,
    pub provider_origin: String,
    pub model: String,
    pub model_origin: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refused: Option<String>,
}

/// The four routing values a Settings write edits: a profile's front matter for a teammate, or a
/// `teams.yaml` field for the review/manager.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RoutingView {
    pub harness: String,
    pub provider: String,
    pub model: String,
    pub effort: String,
}

/// One row per teammate.
#[derive(Debug, Clone, Serialize)]
pub struct IdentityModelsRow {
    pub name: String,
    pub profile: String,
    /// The tuple a run routed to this teammate resolves, with origins.
    pub resolved: TupleView,
    /// The profile front matter this row edits (empty means inherit).
    pub configured: RoutingView,
    /// The roster entry's own routing fields, shown for reference (dispatch does not feed them yet).
    pub identity: RoutingView,
    pub code_reviewer: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile_error: Option<String>,
}

/// One harness's review override.
#[derive(Debug, Clone, Serialize)]
pub struct ReviewModelsRow {
    pub harness: String,
    pub model: String,
    pub provider: String,
    pub effort: String,
}

/// The manager's tuple.
#[derive(Debug, Clone, Serialize)]
pub struct ManagerModelsRow {
    pub resolved: TupleView,
    pub configured: RoutingView,
}

/// `GET /api/v1/teams/models` — the Settings → Models view.
#[derive(Debug, Clone, Serialize)]
pub struct TeamsModelsView {
    pub identities: Vec<IdentityModelsRow>,
    pub review: Vec<ReviewModelsRow>,
    pub manager: ManagerModelsRow,
    /// The harnesses this build can run.
    pub harnesses: Vec<String>,
    /// The providers defined in WORKFLOW.md.
    pub providers: Vec<String>,
}

/// One `(name, routing)` pair — `move_all`'s undo payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutingEntry {
    pub name: String,
    pub routing: RoutingView,
}

/// `POST /api/v1/teams/models` body.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "target", rename_all = "snake_case")]
pub enum TeamsModelsRequest {
    /// Change ONE teammate's profile front matter.
    Identity { name: String, routing: RoutingView },
    /// Change one harness's review override.
    Review {
        harness: String,
        #[serde(default)]
        model: String,
        #[serde(default)]
        provider: String,
        #[serde(default)]
        effort: String,
    },
    /// Change the manager's tuple.
    Manager { routing: RoutingView },
    /// Apply one tuple to every non-reviewer teammate; the reply carries the undo payload.
    MoveAll { routing: RoutingView },
    /// Restore a `move_all`'s previous tuples.
    Undo { previous: Vec<RoutingEntry> },
}

/// `POST /api/v1/teams/models` reply: the refreshed view, the undo payload for a `move_all`, and the
/// room lines the change produced.
#[derive(Debug, Clone, Serialize)]
pub struct TeamsModelsApplyResult {
    pub view: TeamsModelsView,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub previous: Vec<RoutingEntry>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub changed: Vec<String>,
}

/// Why a Settings → Models read or write failed.
#[derive(Debug)]
pub enum TeamsModelsError {
    /// Teams is off or has no runtime home.
    Disabled,
    /// The edit was refused by the daemon's own validation (400).
    Invalid(String),
    /// A file could not be read or written (500).
    Io(String),
    /// The named identity/profile/target does not exist (404).
    Unknown(String),
}

impl std::fmt::Display for TeamsModelsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disabled => f.write_str("teams_disabled"),
            Self::Invalid(m) | Self::Io(m) | Self::Unknown(m) => f.write_str(m),
        }
    }
}

/// The resolved tuple for a teammate wearing `profile`, with origins.
pub fn identity_tuple(cfg: &Config, profile: &ResolvedProfile) -> TupleView {
    let tiers = SelectionTiers {
        ticket: FieldSelection::default(),
        review: None,
        profile: FieldSelection {
            harness: profile.harness.clone(),
            provider: profile.provider.clone(),
            model: profile.model.clone(),
        },
        identity: FieldSelection::default(),
        project: FieldSelection::default(),
        global: FieldSelection::from_agent(&cfg.agent),
    };
    tuple_from(resolve_selection(&SelectionRequest {
        tiers,
        providers: &cfg.providers,
        turn_deadline_ms: provider_turn_deadline_ms(cfg.opencode.turn_timeout_ms),
    }))
}

/// The manager's own tuple, which never borrows a teammate's (design §5 / parent D6).
pub fn manager_tuple(cfg: &Config, teams: &Teams) -> TupleView {
    let manager = FieldSelection {
        harness: teams.manager.harness.clone(),
        provider: teams.manager.provider.clone(),
        model: teams.manager.model.clone(),
    };
    tuple_from(resolve_manager_selection(
        &manager,
        &cfg.providers,
        provider_turn_deadline_ms(cfg.opencode.turn_timeout_ms),
    ))
}

fn tuple_from(result: Result<ResolvedSelection, SelectionRefusal>) -> TupleView {
    match result {
        Ok(sel) => TupleView {
            harness: sel.harness_name,
            harness_origin: sel.origins.harness.as_str().to_string(),
            provider: sel.provider_id,
            provider_origin: sel
                .origins
                .provider
                .map(|o| o.as_str().to_string())
                .unwrap_or_default(),
            model: sel.model.unwrap_or_default(),
            model_origin: sel
                .origins
                .model
                .map(|o| o.as_str().to_string())
                .unwrap_or_default(),
            refused: None,
        },
        Err(e) => TupleView {
            refused: Some(e.to_string()),
            ..TupleView::default()
        },
    }
}

/// The full Settings → Models view for `teams`, resolving each identity's profile against
/// `profiles_dir` and the installation-wide `agent.*` default in `cfg`.
pub fn build_view(cfg: &Config, teams: &Teams, profiles_dir: &Path) -> TeamsModelsView {
    let mut identities = Vec::with_capacity(teams.roster.len());
    for id in &teams.roster {
        let (resolved, configured, profile_error) =
            match rhapsody_config::profiles::resolve(profiles_dir, &id.profile) {
                Ok(p) => (
                    identity_tuple(cfg, &p),
                    RoutingView {
                        harness: p.harness.clone(),
                        provider: p.provider.clone(),
                        model: p.model.clone(),
                        effort: p.effort.clone(),
                    },
                    None,
                ),
                Err(e) => (
                    TupleView::default(),
                    read_profile_routing(profiles_dir, &id.profile),
                    Some(e.to_string()),
                ),
            };
        identities.push(IdentityModelsRow {
            name: id.name.clone(),
            profile: id.profile.clone(),
            resolved,
            configured,
            identity: RoutingView {
                harness: id.harness.clone(),
                provider: id.provider.clone(),
                model: id.model.clone(),
                effort: id.effort.clone(),
            },
            code_reviewer: teams.review.is_code_reviewer(id),
            profile_error,
        });
    }
    let fallback = cfg.agent.backend.as_str();
    let review = RUNNABLE_HARNESSES
        .iter()
        .map(|h| ReviewModelsRow {
            harness: (*h).to_string(),
            model: teams
                .review
                .model
                .for_harness(h, fallback)
                .unwrap_or_default()
                .to_string(),
            provider: teams
                .review
                .provider
                .for_harness(h, fallback)
                .unwrap_or_default()
                .to_string(),
            effort: teams
                .review
                .effort
                .for_harness(h, fallback)
                .unwrap_or_default()
                .to_string(),
        })
        .collect();
    let manager = ManagerModelsRow {
        resolved: manager_tuple(cfg, teams),
        configured: RoutingView {
            harness: teams.manager.harness.clone(),
            provider: teams.manager.provider.clone(),
            model: teams.manager.model.clone(),
            effort: teams.manager.effort.clone(),
        },
    };
    let providers: Vec<String> = cfg.providers.keys().cloned().collect();
    TeamsModelsView {
        identities,
        review,
        manager,
        harnesses: RUNNABLE_HARNESSES
            .iter()
            .map(|h| (*h).to_string())
            .collect(),
        providers,
    }
}

/// Best-effort read of a profile file's raw routing front matter, for a row whose profile failed to
/// RESOLVE (so the page can still show and correct what the file says).
fn read_profile_routing(dir: &Path, profile: &str) -> RoutingView {
    let path = rhapsody_config::profiles::profile_path(dir, profile);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return RoutingView::default();
    };
    let r = rhapsody_config::profiles::read_routing(&text);
    RoutingView {
        harness: r.harness,
        provider: r.provider,
        model: r.model,
        effort: r.effort,
    }
}
