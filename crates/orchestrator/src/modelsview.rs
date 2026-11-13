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

use rhapsody_agent::harness::{ProviderProtocol, harness_id_for_name, harness_supports_protocol};
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
    /// The roster entry's own routing fields — the identity tier the resolver folds between the
    /// profile and the installation-wide `agent.*` default.
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

/// The resolved tuple for a teammate wearing `profile`, with origins. `identity` is the roster
/// entry's OWN routing fields — the identity tier the resolver folds between the profile and the
/// installation-wide `agent.*` default (STUDIO-1976).
pub fn identity_tuple(
    cfg: &Config,
    profile: &ResolvedProfile,
    identity: &RoutingView,
) -> TupleView {
    let tiers = SelectionTiers {
        ticket: FieldSelection::default(),
        review: None,
        profile: FieldSelection {
            harness: profile.harness.clone(),
            provider: profile.provider.clone(),
            model: profile.model.clone(),
        },
        identity: FieldSelection {
            harness: identity.harness.clone(),
            provider: identity.provider.clone(),
            model: identity.model.clone(),
        },
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

/// Refuse a candidate provider this build cannot run on `harness`: not a canonical provider id, not
/// defined in WORKFLOW.md, an unknown protocol, or a protocol the harness adapter cannot consume —
/// `claude` with `openai-compatible` is the "a provider on `claude`" case. `harness` is the
/// EFFECTIVE harness the candidate resolves to (inherited fields included), so a blank harness that
/// inherits `claude` still refuses a provider.
fn validate_provider(cfg: &Config, harness: &str, provider: &str) -> Result<(), String> {
    if provider.is_empty() {
        return Ok(());
    }
    rhapsody_config::providers::canonical_provider_id(provider).map_err(|reason| {
        format!("provider {provider:?} is not a canonical provider id: {reason}")
    })?;
    let def = cfg
        .providers
        .get(provider)
        .ok_or_else(|| format!("provider {provider:?} is not defined in WORKFLOW.md"))?;
    let id = harness_id_for_name(harness)
        .ok_or_else(|| format!("harness {harness:?} is not one this build can run"))?;
    let protocol = ProviderProtocol::from_name(&def.protocol).ok_or_else(|| {
        format!(
            "provider {provider:?} names an unknown protocol {:?}",
            def.protocol
        )
    })?;
    if !harness_supports_protocol(id, protocol) {
        return Err(format!(
            "harness {harness:?} cannot consume provider protocol {:?}",
            def.protocol
        ));
    }
    Ok(())
}

/// A harness this build can run, or `""` (which the resolver reads as "inherit").
fn validate_harness(harness: &str) -> Result<(), String> {
    if harness.is_empty() || RUNNABLE_HARNESSES.contains(&harness) {
        Ok(())
    } else {
        Err(format!("harness {harness:?} is not one this build can run"))
    }
}

/// Refuse a candidate teammate tuple BEFORE it replaces a last-good one. `profile` is the front
/// matter the write would leave; `identity` is the roster entry's own fields. The COMPLETE effective
/// candidate — harness, provider AND model — is resolved field-wise (profile > identity > global)
/// through the very resolver dispatch uses, so a blank harness that inherits `claude` refuses a
/// provider AND an explicit provider with no model anywhere is refused as `MissingModel` rather than
/// written over a working profile only to refuse every dispatch. Legitimate inheritance (a model
/// supplied by a lower tier) still resolves and is accepted.
pub fn validate_identity_routing(
    cfg: &Config,
    profile: &RoutingView,
    identity: &RoutingView,
) -> Result<(), String> {
    resolve_selection(&SelectionRequest {
        tiers: SelectionTiers {
            ticket: FieldSelection::default(),
            review: None,
            profile: FieldSelection {
                harness: profile.harness.clone(),
                provider: profile.provider.clone(),
                model: profile.model.clone(),
            },
            identity: FieldSelection {
                harness: identity.harness.clone(),
                provider: identity.provider.clone(),
                model: identity.model.clone(),
            },
            project: FieldSelection::default(),
            global: FieldSelection::from_agent(&cfg.agent),
        },
        providers: &cfg.providers,
        turn_deadline_ms: provider_turn_deadline_ms(cfg.opencode.turn_timeout_ms),
    })
    .map(|_| ())
    .map_err(|e| e.to_string())
}

/// Refuse a candidate review override. The row is per-harness, so the harness is explicit and only
/// the provider needs the effective-harness check.
pub fn validate_review_routing(cfg: &Config, harness: &str, provider: &str) -> Result<(), String> {
    validate_harness(harness)?;
    validate_provider(cfg, harness, provider)
}

/// Refuse a candidate manager tuple. The manager's tuple borrows no teammate's, so the dispatch
/// resolver is the exact oracle.
pub fn validate_manager_routing(cfg: &Config, candidate: &RoutingView) -> Result<(), String> {
    resolve_manager_selection(
        &FieldSelection {
            harness: candidate.harness.clone(),
            provider: candidate.provider.clone(),
            model: candidate.model.clone(),
        },
        &cfg.providers,
        provider_turn_deadline_ms(cfg.opencode.turn_timeout_ms),
    )
    .map(|_| ())
    .map_err(|e| e.to_string())
}

/// Refuse a reloaded `teams.yaml` whose routing fields name a tuple this build could not run: an
/// unknown/non-canonical/undefined provider, a provider on `claude`, or an unrunnable harness. The
/// SAME oracle the Settings write uses, applied to every routing field the reload touches, so a bad
/// edit is refused at the reload and the last-good routing is kept.
pub fn validate_teams_routing(
    cfg: &Config,
    teams: &Teams,
    profiles_dir: Option<&Path>,
) -> Result<(), String> {
    let fallback = cfg.agent.backend.as_str();
    for harness in RUNNABLE_HARNESSES {
        let provider = teams
            .review
            .provider
            .for_harness(harness, fallback)
            .unwrap_or_default();
        validate_review_routing(cfg, harness, provider)?;
    }
    validate_manager_routing(
        cfg,
        &RoutingView {
            harness: teams.manager.harness.clone(),
            provider: teams.manager.provider.clone(),
            model: teams.manager.model.clone(),
            effort: teams.manager.effort.clone(),
        },
    )?;
    for id in &teams.roster {
        let profile = profiles_dir
            .and_then(|dir| rhapsody_config::profiles::resolve(dir, &id.profile).ok())
            .map(|p| RoutingView {
                harness: p.harness,
                provider: p.provider,
                model: p.model,
                effort: p.effort,
            })
            .unwrap_or_default();
        let identity = RoutingView {
            harness: id.harness.clone(),
            provider: id.provider.clone(),
            model: id.model.clone(),
            effort: id.effort.clone(),
        };
        validate_identity_routing(cfg, &profile, &identity)?;
    }
    Ok(())
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
        let identity = RoutingView {
            harness: id.harness.clone(),
            provider: id.provider.clone(),
            model: id.model.clone(),
            effort: id.effort.clone(),
        };
        let (resolved, configured, profile_error) =
            match rhapsody_config::profiles::resolve(profiles_dir, &id.profile) {
                Ok(p) => (
                    identity_tuple(cfg, &p, &identity),
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
            identity,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::testsupport::TempDir;

    /// A config whose `agent.backend` is `claude` and which defines one `openai-compatible`
    /// provider (`testprovider`) — the exact shape that separates "a provider on claude" from a
    /// legal provider on opencode.
    fn cfg() -> Config {
        crate::testsupport::empty_effective(Arc::new(rhapsody_tracker::fake::Fake::default())).cfg
    }

    fn routing(harness: &str, provider: &str, model: &str) -> RoutingView {
        RoutingView {
            harness: harness.into(),
            provider: provider.into(),
            model: model.into(),
            effort: String::new(),
        }
    }

    fn teams(text: &str) -> (TempDir, Teams) {
        let dir = TempDir::new();
        let path = dir.child("teams.yaml");
        std::fs::write(&path, text).expect("write teams");
        let teams = Teams::try_load(std::path::Path::new(&path)).expect("load teams");
        (dir, teams)
    }

    /// A provider on `claude` is refused even when the candidate names NO harness: a blank harness
    /// inherits the configured backend (`claude` here), so `provider: testprovider` is still a
    /// provider on Claude and must not be written.
    #[test]
    fn a_provider_on_an_inherited_claude_is_refused() {
        let cfg = cfg();
        let profile = routing("", "testprovider", "m");
        let err = validate_identity_routing(&cfg, &profile, &RoutingView::default())
            .expect_err("provider on claude must be refused");
        assert!(err.contains("cannot consume"), "{err}");
    }

    /// The same provider is legal on opencode, so the effective-harness check must not over-refuse.
    #[test]
    fn the_same_provider_is_accepted_on_opencode() {
        let cfg = cfg();
        assert!(
            validate_identity_routing(
                &cfg,
                &routing("opencode", "testprovider", "m"),
                &RoutingView::default()
            )
            .is_ok()
        );
    }

    /// STUDIO-1976 review B2: an explicit provider with NO model anywhere must be refused BEFORE the
    /// write. The old validator checked only harness/provider compatibility, so this tuple was
    /// written over a working profile and then refused every dispatch (`MissingModel`). The resolver
    /// now validates the complete effective candidate.
    #[test]
    fn an_explicit_provider_without_a_model_is_refused() {
        let cfg = cfg();
        let err = validate_identity_routing(
            &cfg,
            &routing("opencode", "testprovider", ""),
            &RoutingView::default(),
        )
        .expect_err("an explicit provider requires an exact model");
        assert!(err.contains("exact model"), "{err}");
    }

    /// Legitimate inheritance is retained: a provider with no model in the profile is legal when a
    /// lower tier (the roster identity) supplies the model — the write must not over-refuse it.
    #[test]
    fn an_explicit_provider_inherits_a_model_from_the_identity_tier() {
        let cfg = cfg();
        assert!(
            validate_identity_routing(
                &cfg,
                &routing("opencode", "testprovider", ""),
                &routing("", "", "identity-model"),
            )
            .is_ok(),
            "a model supplied by the identity tier keeps the tuple valid"
        );
    }

    /// A canonical-but-undefined provider is refused, and so is a non-canonical one.
    #[test]
    fn an_undefined_or_noncanonical_provider_is_refused() {
        let cfg = cfg();
        assert!(
            validate_identity_routing(
                &cfg,
                &routing("opencode", "not-defined", "m"),
                &RoutingView::default()
            )
            .is_err()
        );
        assert!(
            validate_review_routing(&cfg, "opencode", "not a provider!").is_err(),
            "a non-canonical provider id must be refused"
        );
    }

    /// `validate_teams_routing` refuses a reload whose review row names a canonical but UNDEFINED
    /// provider — the exact B2 reload probe — while accepting a defined one.
    #[test]
    fn a_reload_with_an_undefined_review_provider_is_refused() {
        let cfg = cfg();
        let (_d, bad) = teams(
            "enabled: true\nreview:\n  mode: ticketless\n  provider:\n    opencode: not-defined\n  model:\n    opencode: new-model\nmanager:\n  mode: labels\n  model: m\nroster:\n  - name: a\n    profile: swe\n",
        );
        let err = validate_teams_routing(&cfg, &bad, None).expect_err("must refuse");
        assert!(err.contains("not defined"), "{err}");

        let (_d, good) = teams(
            "enabled: true\nreview:\n  mode: ticketless\n  provider:\n    opencode: testprovider\n  model:\n    opencode: new-model\nmanager:\n  mode: labels\n  model: m\nroster:\n  - name: a\n    profile: swe\n",
        );
        assert!(validate_teams_routing(&cfg, &good, None).is_ok());
    }

    /// The identity tier participates in the resolved tuple: a roster entry's own model fills the
    /// gap a silent profile leaves, and the reported origin names the identity.
    #[test]
    fn the_identity_tier_supplies_a_silent_profiles_model() {
        let cfg = cfg();
        let dir = TempDir::new();
        let profiles = dir.child("profiles");
        std::fs::create_dir_all(&profiles).expect("profiles dir");
        std::fs::write(
            std::path::Path::new(&profiles).join("swe.md"),
            "---\nextends: swe\nharness: opencode\nprovider: testprovider\n---\nbody\n",
        )
        .expect("write profile");
        let (_d, teams) = teams(
            "enabled: true\nreview:\n  mode: ticketless\nmanager:\n  mode: labels\n  model: m\nroster:\n  - name: a\n    profile: swe\n    model: identity-model\n",
        );
        let view = build_view(&cfg, &teams, std::path::Path::new(&profiles));
        let row = &view.identities[0];
        assert_eq!(row.resolved.model, "identity-model");
        assert_eq!(row.resolved.model_origin, "identity");
        assert_eq!(row.resolved.harness, "opencode");
        assert_eq!(row.resolved.provider, "testprovider");
    }
}
