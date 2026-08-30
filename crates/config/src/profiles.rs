//! Rhapsody Teams profiles — `~/.rhapsody/teams/profiles/<name>.md`: the role
//! documents an identity wears (design record
//! `~/.rhapsody/docs/STUDIO-572-rhapsody-teams.md`, §1, §2.2, §4).
//!
//! A **profile is not an identity** (§1). It is a document you would hand to a
//! new hire: front matter plus a prompt body, with no person's name in it, no
//! memory-bank id and no history. Identities live in [`crate::teams`]'s roster
//! and merely *name* the profile they wear, which is what stops a profile
//! collapsing into "a rename of `.claude/agents/`".
//!
//! The file idiom is Rhapsody's own — front matter + body, parsed by
//! [`crate::workflow::load`], the same loader `WORKFLOW.md` uses (§1).
//!
//! # The upgrade strategy: layered defaults with explicit fork (§4)
//!
//! Built-in profiles ship compiled into the binary and are **versioned**
//! ([`builtin_profiles`]: `swe`, `reviewer`, `sre`, each shipping v1 and — since
//! T4 taught every role to retain (§5.1) — v2). A user's file is an
//! **overlay** on one of them:
//!
//! * `extends: swe` — track the newest built-in. Fields the user never set
//!   improve for free on upgrade; fields the user set stay exactly as written.
//! * `extends: swe@3` — **pin**. Upgrades do not move it; when the built-in has
//!   moved on, resolution *reports* the [`Drift`] and never merges it.
//! * `extends: none` — **fork**. The file is the whole profile and Rhapsody
//!   contributes nothing to it.
//!
//! **Rhapsody only ever READS a user's profile file.** The single exception in
//! the whole feature is `rhapsodyd teams fork`, which materialises
//! [`fork_text`] on an explicit operator command.
//!
//! # `{{ base }}` is a literal splice, deliberately not Liquid
//!
//! [`BASE_TOKEN`] is replaced by the built-in's prompt body with
//! [`str::replace`]. Profile bodies are **never** routed through
//! [`crate::prompt`]'s Liquid renderer: a profile body is prose that a user (or
//! an agent editing on their behalf) writes, and handing it to a template engine
//! would turn every stray `{{` in a prompt into a render error and every
//! interpolation into an injection surface. §9 names this token as the one new
//! templating affordance the design adds; keeping it a literal token splice is
//! what keeps it small.
//!
//! # T2 is prompt-only (§0.11.7)
//!
//! `model` and `effort` resolve here and are consumed by **nothing** — wiring
//! them into the dispatched invocation is T2b. `tools` is parsed and explicitly
//! unused: per-profile tool allowlists are deferred (they need permission-flag
//! plumbing), not implied.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::teams::{Teams, is_label_safe};
use crate::workflow::{self, Definition, YamlMap};

/// The literal token a profile body uses to splice in the built-in's prompt
/// body (§2.2). Matched exactly — not a Liquid tag, not whitespace-flexible.
pub const BASE_TOKEN: &str = "{{ base }}";

/// What a profile file's `extends:` says its base is (§4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Extends {
    /// `extends: swe` — track the newest built-in named `swe`.
    Latest(String),
    /// `extends: swe@3` — pin to that exact built-in version.
    Pinned { name: String, version: u32 },
    /// `extends: none`, or no `extends:` at all — a fork: the file is the whole
    /// profile.
    ///
    /// An ABSENT `extends` is a fork rather than an implicit overlay of the
    /// same-named built-in, because §4's whole point is that "I never touched
    /// this" and "I own this" must be *distinguishable in the file itself*. A
    /// file that declares no base has not declared one, and guessing would
    /// reintroduce exactly the ambiguity that makes clobbering possible.
    None,
}

impl Extends {
    /// Parses an `extends:` value: `""`/`none` ⇒ [`Extends::None`], `name@N` ⇒
    /// [`Extends::Pinned`], `name` ⇒ [`Extends::Latest`].
    fn parse(raw: &str) -> Result<Self, ProfileError> {
        let raw = raw.trim();
        if raw.is_empty() || raw == "none" {
            return Ok(Extends::None);
        }
        let (name, version) = match raw.split_once('@') {
            None => return Self::checked_name(raw).map(Extends::Latest),
            Some((n, v)) => (n, v),
        };
        let name = Self::checked_name(name)?;
        let version: u32 = version.parse().map_err(|_| {
            ProfileError::Invalid(format!(
                "extends {raw:?}: version after `@` must be a positive integer"
            ))
        })?;
        if version == 0 {
            return Err(ProfileError::Invalid(format!(
                "extends {raw:?}: version after `@` must be a positive integer"
            )));
        }
        Ok(Extends::Pinned { name, version })
    }

    fn checked_name(name: &str) -> Result<String, ProfileError> {
        if !is_label_safe(name) {
            return Err(ProfileError::Invalid(format!(
                "extends target {name:?} is not a profile name (must match ^[a-z][a-z0-9-]*$)"
            )));
        }
        Ok(name.to_string())
    }
}

/// A built-in profile: shipped compiled into the binary and **versioned**, so a
/// user who pinned `swe@1` keeps `swe@1` when `swe@2` ships (§4).
///
/// A new version is a NEW entry, never an edit to an existing one — editing one
/// in place would silently move every pin that names it, which is the mutation
/// §4 forbids.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuiltinProfile {
    pub name: &'static str,
    pub version: u32,
    /// Empty ⇒ inherit the daemon's configured model (§2.2). Consumed by T2b.
    pub model: &'static str,
    /// Empty ⇒ inherit. Consumed by T2b.
    pub effort: &'static str,
    /// Names from the BO-11 registry ([`crate::capabilities`]) — referenced
    /// here, resolved by whoever renders them.
    pub capabilities: &'static [&'static str],
    /// Parsed and explicitly unused in T2 (§0.11.7).
    pub tools: &'static [&'static str],
    pub body: &'static str,
}

/// The bundled, versioned default profiles (§4).
///
/// Bodies live beside this file as real markdown documents (`profiles/builtin/
/// <name>.v<N>.md`) because that is what a profile *is* (§1) — one file per
/// version, so bumping a role prompt is an added file and never an edit to a
/// shipped one.
const BUILTINS: &[BuiltinProfile] = &[
    BuiltinProfile {
        name: "swe",
        version: 1,
        model: "",
        effort: "",
        capabilities: &[
            "design-first",
            "test-coverage",
            "code-review",
            "adversarial-verify",
        ],
        tools: &[],
        body: include_str!("profiles/builtin/swe.v1.md"),
    },
    BuiltinProfile {
        name: "reviewer",
        version: 1,
        model: "",
        effort: "",
        capabilities: &["code-review", "security-review", "simplify"],
        tools: &[],
        body: include_str!("profiles/builtin/reviewer.v1.md"),
    },
    BuiltinProfile {
        name: "sre",
        version: 1,
        model: "",
        effort: "",
        capabilities: &["systematic-debugging", "adversarial-verify"],
        tools: &[],
        body: include_str!("profiles/builtin/sre.v1.md"),
    },
    // v2 (STUDIO-645, T4): every role gains the "retain what the next run will
    // need" section — the prose half of §5.1's split, where Rhapsody supplies
    // the evidence and the agent supplies the words. Shipped as ADDED files, as
    // §4 requires: `swe@1` still resolves byte-for-byte for anyone who pinned
    // it, and `extends: swe` picks these up on upgrade for free.
    BuiltinProfile {
        name: "swe",
        version: 2,
        model: "",
        effort: "",
        capabilities: &[
            "design-first",
            "test-coverage",
            "code-review",
            "adversarial-verify",
        ],
        tools: &[],
        body: include_str!("profiles/builtin/swe.v2.md"),
    },
    BuiltinProfile {
        name: "reviewer",
        version: 2,
        model: "",
        effort: "",
        capabilities: &["code-review", "security-review", "simplify"],
        tools: &[],
        body: include_str!("profiles/builtin/reviewer.v2.md"),
    },
    BuiltinProfile {
        name: "sre",
        version: 2,
        model: "",
        effort: "",
        capabilities: &["systematic-debugging", "adversarial-verify"],
        tools: &[],
        body: include_str!("profiles/builtin/sre.v2.md"),
    },
];

/// The bundled default profiles, newest version last for any given name.
pub fn builtin_profiles() -> &'static [BuiltinProfile] {
    BUILTINS
}

/// Why a profile could not be read, parsed or resolved. Sentinel prefixes match
/// [`crate::teams::TeamsError`]'s convention: `rhapsody-config` does no logging
/// of its own, so the error carries the reason to the caller that owns the log.
#[derive(thiserror::Error, Debug)]
pub enum ProfileError {
    #[error("profile_io_error: {0}")]
    Io(String),
    #[error("profile_parse_error: {0}")]
    Parse(String),
    #[error("profile_invalid: {0}")]
    Invalid(String),
    #[error("profile_unknown: {0}")]
    Unknown(String),
}

/// The front matter of a profile file, before defaulting (§2.2).
///
/// Every field is `Option`, not just `#[serde(default)]`, so a key written with
/// nothing under it (`capabilities:` — a YAML null, which is what commenting out
/// the entries leaves behind) is the unset value rather than a type error.
#[derive(Debug, Default, Deserialize)]
struct RawProfile {
    #[serde(default)]
    extends: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    effort: Option<String>,
    #[serde(default)]
    capabilities: Option<Vec<String>>,
    #[serde(default)]
    tools: Option<Vec<String>>,
}

/// A parsed profile file: its front matter plus its (trimmed) prompt body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileFile {
    pub extends: Extends,
    pub model: String,
    pub effort: String,
    pub capabilities: Vec<String>,
    /// Parsed, explicitly unused (§0.11.7).
    pub tools: Vec<String>,
    pub body: String,
}

/// Where a resolved field came from — what `teams show` prints so "what prompt
/// does Alice actually get" is answerable in one command (§4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// Inherited from the built-in base.
    Base,
    /// Set by the user's overlay file.
    Overlay,
    /// Set by neither; the empty value, which means "inherit from the daemon's
    /// own config" for `model`/`effort` (§2.2).
    Unset,
}

/// How the resolved prompt body was composed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyOrigin {
    /// The overlay's body was empty ⇒ "no change to the prompt" (§2.2).
    Base,
    /// The overlay's body had no [`BASE_TOKEN`] ⇒ it replaces wholesale.
    Overlay,
    /// [`BASE_TOKEN`] spliced the base body into the overlay's body.
    Spliced,
}

/// The built-in a profile actually resolved against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseRef {
    pub name: String,
    pub version: u32,
    /// True when the file pinned this version with `@N` (§4).
    pub pinned: bool,
}

/// A pin that the built-in has moved past: "`alice`'s profile overlays `swe@1`;
/// the built-in is now `swe@2`" (§4). **Reported, never merged.**
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Drift {
    pub name: String,
    pub pinned: u32,
    pub latest: u32,
}

/// Where every part of a [`ResolvedProfile`] came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provenance {
    /// The built-in base, or `None` for a fork (`extends: none`).
    pub base: Option<BaseRef>,
    /// The overlay file that was read, or `None` when the built-in resolved on
    /// its own (no user file exists — the shipped state).
    pub overlay: Option<PathBuf>,
    pub drift: Option<Drift>,
    pub model: Origin,
    pub effort: Origin,
    pub capabilities: Origin,
    pub tools: Origin,
    pub body: BodyOrigin,
}

/// A profile with its base applied — the answer to "what prompt does this
/// identity actually get" (§4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedProfile {
    pub name: String,
    /// Empty ⇒ inherit the daemon's configured model. Consumed by T2b, not T2.
    pub model: String,
    /// Empty ⇒ inherit. Consumed by T2b, not T2.
    pub effort: String,
    pub capabilities: Vec<String>,
    /// Parsed, explicitly unused (§0.11.7).
    pub tools: Vec<String>,
    pub prompt: String,
    pub provenance: Provenance,
}

/// The path a profile named `name` would occupy under `dir`. Naming it does not
/// create it — nothing in this module creates `dir` (§4's read-only invariant);
/// only `teams fork` writes, and only the one named file.
pub fn profile_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.md"))
}

/// Parses profile-file text: front matter + prompt body, via the same
/// [`crate::workflow`] loader `WORKFLOW.md` uses (§1). A file with no front
/// matter at all is a body-only fork.
fn parse_definition(def: Definition) -> Result<ProfileFile, ProfileError> {
    let raw: RawProfile = if def.config.is_empty() {
        RawProfile::default()
    } else {
        serde_yaml_ng::from_value(serde_yaml_ng::Value::Mapping(def.config))
            .map_err(|e| ProfileError::Parse(e.to_string()))?
    };
    Ok(ProfileFile {
        extends: Extends::parse(raw.extends.unwrap_or_default().as_str())?,
        model: raw.model.unwrap_or_default(),
        effort: raw.effort.unwrap_or_default(),
        capabilities: raw.capabilities.unwrap_or_default(),
        tools: raw.tools.unwrap_or_default(),
        body: def.prompt_template,
    })
}

/// Loads and parses the profile file at `path`. The caller has already
/// established that it exists — this never creates anything.
fn load_profile(path: &Path) -> Result<ProfileFile, ProfileError> {
    let def = workflow::load(path).map_err(|e| match e {
        workflow::WorkflowError::MissingWorkflowFile => {
            ProfileError::Io(format!("cannot read {}", path.display()))
        }
        other => ProfileError::Parse(other.to_string()),
    })?;
    parse_definition(def)
}

/// The newest built-in named `name`, or `None` when nothing ships under it.
fn newest(builtins: &'static [BuiltinProfile], name: &str) -> Option<&'static BuiltinProfile> {
    builtins
        .iter()
        .filter(|b| b.name == name)
        .max_by_key(|b| b.version)
}

/// Composes the resolved prompt body from the base body and the overlay body
/// (§2.2, §4), pinning the three cases the design names:
///
/// * an EMPTY overlay body means "no change to the prompt" ⇒ the base body;
/// * a body containing [`BASE_TOKEN`] **composes** — every occurrence of the
///   literal token is replaced by the base body (the documented one-token shape
///   therefore splices exactly once);
/// * a body without the token **replaces wholesale** — §4's "the body composes
///   via `{{ base }}` *rather than* replacing wholesale" makes the token the
///   thing that opts into composition, so its absence is the replacement case.
///
/// For a fork there is no base, so the base body is empty and the same three
/// cases apply unchanged.
fn compose_body(base: Option<&str>, overlay: &str) -> (String, BodyOrigin) {
    if overlay.trim().is_empty() {
        return match base {
            Some(b) => (b.trim().to_string(), BodyOrigin::Base),
            None => (String::new(), BodyOrigin::Overlay),
        };
    }
    if overlay.contains(BASE_TOKEN) {
        let spliced = overlay.replace(BASE_TOKEN, base.unwrap_or("").trim());
        return (spliced.trim().to_string(), BodyOrigin::Spliced);
    }
    (overlay.trim().to_string(), BodyOrigin::Overlay)
}

/// Picks a string field: the overlay's when set, else the base's (§4 — "unset
/// fields inherit; set fields never move").
fn pick_str(overlay: &str, base: &str) -> (String, Origin) {
    if !overlay.is_empty() {
        (overlay.to_string(), Origin::Overlay)
    } else if !base.is_empty() {
        (base.to_string(), Origin::Base)
    } else {
        (String::new(), Origin::Unset)
    }
}

/// Picks a list field. A list the user set is the WHOLE list — overlay lists
/// replace rather than append, so a user who removed a capability keeps it
/// removed across upgrades ("set fields never move", §4).
fn pick_list(overlay: &[String], base: &[&'static str]) -> (Vec<String>, Origin) {
    if !overlay.is_empty() {
        (overlay.to_vec(), Origin::Overlay)
    } else if !base.is_empty() {
        (
            base.iter().map(|s| (*s).to_string()).collect(),
            Origin::Base,
        )
    } else {
        (Vec::new(), Origin::Unset)
    }
}

/// Resolves the profile named `name` against the profile files in `dir` and the
/// bundled built-ins — the pure function §4 requires, with no side effects and,
/// in particular, **no directory creation**: `dir` not existing simply means
/// every profile resolves to its built-in.
pub fn resolve(dir: &Path, name: &str) -> Result<ResolvedProfile, ProfileError> {
    resolve_with(dir, name, builtin_profiles())
}

/// [`resolve`] against an injected built-in registry, so tests can simulate a
/// built-in version bump (`swe@1` + `swe@2`) without shipping one.
fn resolve_with(
    dir: &Path,
    name: &str,
    builtins: &'static [BuiltinProfile],
) -> Result<ResolvedProfile, ProfileError> {
    if !is_label_safe(name) {
        // Also the path-traversal guard: the name becomes `<name>.md` under
        // `dir`, so it must never carry a separator, a `.` or a `..`.
        return Err(ProfileError::Invalid(format!(
            "profile name {name:?} is not a profile name (must match ^[a-z][a-z0-9-]*$)"
        )));
    }
    let path = profile_path(dir, name);
    if !path.exists() {
        // No user file: the built-in IS the profile, at its newest version.
        let base = newest(builtins, name).ok_or_else(|| {
            ProfileError::Unknown(format!(
                "no profile {name:?}: no file at {} and no built-in of that name",
                path.display()
            ))
        })?;
        return Ok(from_builtin(name, base));
    }
    let file = load_profile(&path)?;
    // The base is always a BUILT-IN, never another user file. That is what makes
    // an `extends` chain — and therefore a cycle — unreachable: built-ins carry
    // no `extends`, so resolution is depth-1 by construction. It also makes a
    // same-named overlay (`swe.md` with `extends: swe`) the ordinary overlay
    // case rather than self-recursion: it layers on the BUILT-IN `swe`.
    let (base, drift) = match &file.extends {
        Extends::None => (None, None),
        Extends::Latest(base_name) => {
            let b = newest(builtins, base_name).ok_or_else(|| {
                ProfileError::Unknown(format!(
                    "profile {name:?} extends {base_name:?}, which is not a built-in profile"
                ))
            })?;
            (Some((b, false)), None)
        }
        Extends::Pinned {
            name: base_name,
            version,
        } => {
            let b = builtins
                .iter()
                .find(|b| b.name == base_name && b.version == *version)
                .ok_or_else(|| {
                    ProfileError::Unknown(format!(
                        "profile {name:?} extends {base_name}@{version}, which is not a built-in profile version"
                    ))
                })?;
            // Drift is REPORTED, never merged (§4).
            let drift = newest(builtins, base_name)
                .filter(|l| l.version > *version)
                .map(|l| Drift {
                    name: base_name.clone(),
                    pinned: *version,
                    latest: l.version,
                });
            (Some((b, true)), drift)
        }
    };
    let (base_profile, pinned) = match base {
        Some((b, p)) => (Some(b), p),
        None => (None, false),
    };
    let (model, model_origin) = pick_str(&file.model, base_profile.map_or("", |b| b.model));
    let (effort, effort_origin) = pick_str(&file.effort, base_profile.map_or("", |b| b.effort));
    let (capabilities, capabilities_origin) = pick_list(
        &file.capabilities,
        base_profile.map_or(&[][..], |b| b.capabilities),
    );
    let (tools, tools_origin) = pick_list(&file.tools, base_profile.map_or(&[][..], |b| b.tools));
    let (prompt, body) = compose_body(base_profile.map(|b| b.body), &file.body);
    Ok(ResolvedProfile {
        name: name.to_string(),
        model,
        effort,
        capabilities,
        tools,
        prompt,
        provenance: Provenance {
            base: base_profile.map(|b| BaseRef {
                name: b.name.to_string(),
                version: b.version,
                pinned,
            }),
            overlay: Some(path),
            drift,
            model: model_origin,
            effort: effort_origin,
            capabilities: capabilities_origin,
            tools: tools_origin,
            body,
        },
    })
}

/// The resolution of a built-in with no user file over it — the shipped state.
fn from_builtin(name: &str, base: &'static BuiltinProfile) -> ResolvedProfile {
    let list_origin = |v: &[&'static str]| {
        if v.is_empty() {
            Origin::Unset
        } else {
            Origin::Base
        }
    };
    let str_origin = |s: &str| {
        if s.is_empty() {
            Origin::Unset
        } else {
            Origin::Base
        }
    };
    ResolvedProfile {
        name: name.to_string(),
        model: base.model.to_string(),
        effort: base.effort.to_string(),
        capabilities: base.capabilities.iter().map(|s| (*s).to_string()).collect(),
        tools: base.tools.iter().map(|s| (*s).to_string()).collect(),
        prompt: base.body.trim().to_string(),
        provenance: Provenance {
            base: Some(BaseRef {
                name: base.name.to_string(),
                version: base.version,
                pinned: false,
            }),
            overlay: None,
            drift: None,
            model: str_origin(base.model),
            effort: str_origin(base.effort),
            capabilities: list_origin(base.capabilities),
            tools: list_origin(base.tools),
            body: BodyOrigin::Base,
        },
    }
}

/// Something worth saying out loud about the roster's profiles at boot (§4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RosterIssue {
    /// A roster entry names a profile that does not resolve — the "broken agent
    /// discovered at dispatch time" that §4 exists to prevent.
    Unresolvable {
        identity: String,
        profile: String,
        reason: String,
    },
    /// A pinned profile whose built-in has moved on. Reported; never merged.
    Drift {
        identity: String,
        profile: String,
        drift: Drift,
    },
}

impl std::fmt::Display for RosterIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RosterIssue::Unresolvable {
                identity,
                profile,
                reason,
            } => write!(f, "{identity} names profile {profile:?}: {reason}"),
            RosterIssue::Drift {
                identity,
                profile,
                drift,
            } => write!(
                f,
                "{identity}'s profile {profile:?} overlays {}@{}; the built-in is now {}@{}",
                drift.name, drift.pinned, drift.name, drift.latest
            ),
        }
    }
}

/// Resolves every roster entry's profile and reports what an operator needs to
/// know — unknown profiles and pin drift (§4). Read-only, like everything else
/// here: it never creates `dir`.
///
/// A roster entry with an EMPTY `profile` is skipped rather than reported: it
/// names no profile, so there is no unknown one to complain about.
pub fn check_roster(teams: &Teams, dir: &Path) -> Vec<RosterIssue> {
    let mut issues = Vec::new();
    for identity in &teams.roster {
        if identity.profile.is_empty() {
            continue;
        }
        match resolve(dir, &identity.profile) {
            Ok(resolved) => {
                if let Some(drift) = resolved.provenance.drift {
                    issues.push(RosterIssue::Drift {
                        identity: identity.name.clone(),
                        profile: identity.profile.clone(),
                        drift,
                    });
                }
            }
            Err(e) => issues.push(RosterIssue::Unresolvable {
                identity: identity.name.clone(),
                profile: identity.profile.clone(),
                reason: e.to_string(),
            }),
        }
    }
    issues
}

/// Renders `resolved` as a self-contained profile file with `extends: none` —
/// what `rhapsodyd teams fork` materialises (§4).
///
/// Every front-matter key is emitted, including the empty ones, so the forked
/// file is a complete document the user can edit rather than a fragment they
/// have to remember the schema for. Returning a [`Definition`] lets the caller
/// write it with [`workflow::save`], reusing the crate's atomic-write
/// convention (temp file in the same dir, 0600, rename over).
pub fn fork_definition(resolved: &ResolvedProfile) -> Definition {
    use serde_yaml_ng::Value;
    let mut config = YamlMap::new();
    config.insert(Value::from("extends"), Value::from("none"));
    config.insert(Value::from("model"), Value::from(resolved.model.clone()));
    config.insert(Value::from("effort"), Value::from(resolved.effort.clone()));
    config.insert(
        Value::from("capabilities"),
        Value::Sequence(
            resolved
                .capabilities
                .iter()
                .map(|c| Value::from(c.as_str()))
                .collect(),
        ),
    );
    config.insert(
        Value::from("tools"),
        Value::Sequence(
            resolved
                .tools
                .iter()
                .map(|t| Value::from(t.as_str()))
                .collect(),
        ),
    );
    Definition {
        config,
        prompt_template: resolved.prompt.clone(),
    }
}

/// [`fork_definition`] rendered to the file text it will be written as.
pub fn fork_text(resolved: &ResolvedProfile) -> Result<String, ProfileError> {
    let bytes = workflow::marshal(&fork_definition(resolved))
        .map_err(|e| ProfileError::Parse(e.to_string()))?;
    String::from_utf8(bytes).map_err(|e| ProfileError::Parse(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::teams::Identity;

    /// A synthetic registry carrying TWO versions of one profile, so the §4
    /// upgrade behaviours (track-latest vs pin, and the drift a pin produces)
    /// are testable before a real built-in is ever bumped.
    const TWO_VERSIONS: &[BuiltinProfile] = &[
        BuiltinProfile {
            name: "swe",
            version: 1,
            model: "sonnet",
            effort: "medium",
            capabilities: &["code-review"],
            tools: &["read"],
            body: "v1 body",
        },
        BuiltinProfile {
            name: "swe",
            version: 2,
            model: "opus",
            effort: "high",
            capabilities: &["code-review", "test-coverage"],
            tools: &["read", "write"],
            body: "v2 body",
        },
    ];

    fn write_profile(dir: &Path, name: &str, text: &str) {
        std::fs::create_dir_all(dir).expect("create profiles dir");
        std::fs::write(profile_path(dir, name), text).expect("write profile");
    }

    // ---- built-ins ----

    /// The shipped set is §4's three roles, each with a real prompt body.
    /// Pinned here because the names are a user-facing contract
    /// (`extends: swe`) and the versions are the thing pins name.
    ///
    /// T4 bumped every role to v2 (the "retain what the next run will need"
    /// section, §5.1). **v1 is still listed, unedited**: §4's upgrade story is
    /// that a bump is an ADDED file, so anyone who wrote `extends: swe@1` keeps
    /// exactly the bytes they pinned.
    #[test]
    fn builtins_ship_v1_and_v2_of_swe_reviewer_sre() {
        let got: Vec<(&str, u32)> = builtin_profiles()
            .iter()
            .map(|b| (b.name, b.version))
            .collect();
        assert_eq!(
            got,
            vec![
                ("swe", 1),
                ("reviewer", 1),
                ("sre", 1),
                ("swe", 2),
                ("reviewer", 2),
                ("sre", 2)
            ]
        );
        // Every v2 teaches the retain half of §5.1, and every v1 predates it.
        for b in builtin_profiles() {
            let teaches_retain = b.body.contains("teams_retain");
            assert_eq!(
                teaches_retain,
                b.version >= 2,
                "{}@{} must teach `teams_retain` iff it is v2 or newer",
                b.name,
                b.version
            );
        }
        for b in builtin_profiles() {
            assert!(
                b.body.trim().len() > 400,
                "{}@{} ships a stub body ({} bytes) — §1 wants a document you would hand a new hire",
                b.name,
                b.version,
                b.body.trim().len()
            );
            assert!(
                !b.body.contains(BASE_TOKEN),
                "{}@{} must not itself contain {BASE_TOKEN}: built-ins have no base",
                b.name,
                b.version
            );
        }
    }

    /// A built-in resolves with no file on disk at all — the shipped state —
    /// and does not create the profiles directory on the way (§4).
    #[test]
    fn builtin_resolves_with_no_file_and_creates_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let profiles = dir.path().join("teams").join("profiles");
        assert!(!profiles.exists());

        // `extends`-less resolution takes the NEWEST built-in of that name, so
        // this asserts against the newest rather than a fixed index — the whole
        // point of §4's versioning is that the newest moves.
        let newest_swe = builtin_profiles()
            .iter()
            .filter(|b| b.name == "swe")
            .max_by_key(|b| b.version)
            .expect("a swe built-in ships");
        let r = resolve(&profiles, "swe").expect("swe resolves from the built-in");
        assert_eq!(r.name, "swe");
        assert_eq!(r.prompt, newest_swe.body.trim());
        assert_eq!(
            r.provenance.base,
            Some(BaseRef {
                name: "swe".to_string(),
                version: newest_swe.version,
                pinned: false
            })
        );
        assert_eq!(r.provenance.overlay, None);
        assert_eq!(r.provenance.drift, None);
        assert!(
            !profiles.exists(),
            "resolving must never create {}",
            profiles.display()
        );
    }

    /// Never-create-on-read, the other two entry points (§4, acceptance).
    #[test]
    fn nothing_read_only_creates_the_profiles_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let profiles = dir.path().join("teams").join("profiles");
        let teams = Teams {
            roster: vec![Identity {
                name: "alice".to_string(),
                profile: "swe".to_string(),
                ..Identity::default()
            }],
            ..Teams::disabled()
        };
        assert!(check_roster(&teams, &profiles).is_empty());
        let _ = profile_path(&profiles, "swe");
        let _ = resolve(&profiles, "reviewer");
        assert!(
            !profiles.exists(),
            "check_roster / profile_path / resolve must not create {}",
            profiles.display()
        );
    }

    // ---- §4: extends latest vs pinned ----

    /// Track-latest: fields the user never set improve for free when the
    /// built-in is bumped; fields the user set stay exactly as written (§4).
    #[test]
    fn extends_latest_inherits_unset_fields_after_a_builtin_bump() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path();
        // The user set `effort` and nothing else.
        write_profile(p, "alice", "---\nextends: swe\neffort: low\n---\n");

        // Against v1 only.
        let v1_only: &'static [BuiltinProfile] = &TWO_VERSIONS[..1];
        let before = resolve_with(p, "alice", v1_only).expect("resolves against v1");
        assert_eq!(before.model, "sonnet", "unset model inherits v1's");
        assert_eq!(before.effort, "low", "set effort is the user's");
        assert_eq!(before.capabilities, vec!["code-review".to_string()]);
        assert_eq!(before.prompt, "v1 body");

        // The built-in is bumped. Unset fields move; the set one does not.
        let after = resolve_with(p, "alice", TWO_VERSIONS).expect("resolves against v2");
        assert_eq!(
            after.model, "opus",
            "unset model tracks the newest built-in"
        );
        assert_eq!(after.effort, "low", "a set field never moves");
        assert_eq!(
            after.capabilities,
            vec!["code-review".to_string(), "test-coverage".to_string()],
            "an unset list tracks the newest built-in"
        );
        assert_eq!(after.tools, vec!["read".to_string(), "write".to_string()]);
        assert_eq!(
            after.prompt, "v2 body",
            "an empty body means no change (§2.2)"
        );
        assert_eq!(after.provenance.model, Origin::Base);
        assert_eq!(after.provenance.effort, Origin::Overlay);
        assert_eq!(after.provenance.body, BodyOrigin::Base);
        assert_eq!(
            after.provenance.drift, None,
            "an unpinned overlay never drifts — it is already on the newest"
        );
    }

    /// A pin does not move, and the gap is REPORTED rather than merged (§4).
    #[test]
    fn pinned_does_not_move_but_reports_drift() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path();
        write_profile(p, "alice", "---\nextends: swe@1\n---\n");

        let r = resolve_with(p, "alice", TWO_VERSIONS).expect("pinned resolves");
        assert_eq!(r.model, "sonnet", "a pin stays on v1 after v2 ships");
        assert_eq!(r.prompt, "v1 body");
        assert_eq!(
            r.provenance.base,
            Some(BaseRef {
                name: "swe".to_string(),
                version: 1,
                pinned: true
            })
        );
        assert_eq!(
            r.provenance.drift,
            Some(Drift {
                name: "swe".to_string(),
                pinned: 1,
                latest: 2
            }),
            "the built-in moving past a pin is reported"
        );
    }

    /// A pin on the newest version is not drift.
    #[test]
    fn pin_on_the_newest_version_is_not_drift() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path();
        write_profile(p, "alice", "---\nextends: swe@2\n---\n");
        let r = resolve_with(p, "alice", TWO_VERSIONS).expect("pinned resolves");
        assert_eq!(r.provenance.drift, None);
        assert_eq!(r.prompt, "v2 body");
    }

    // ---- §2.2 / §4: body composition ----

    /// `{{ base }}` splices the base body in, exactly once for the documented
    /// one-token shape — and does not ALSO prepend or append it (§2.2).
    #[test]
    fn base_token_splices_exactly_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path();
        write_profile(
            p,
            "alice",
            "---\nextends: swe\n---\nHouse rules first.\n\n{{ base }}\n\nAnd last.\n",
        );
        let r = resolve_with(p, "alice", TWO_VERSIONS).expect("resolves");
        assert_eq!(r.prompt, "House rules first.\n\nv2 body\n\nAnd last.");
        assert_eq!(
            r.prompt.matches("v2 body").count(),
            1,
            "the base body must appear exactly once"
        );
        assert!(
            !r.prompt.contains(BASE_TOKEN),
            "the token must not survive into the resolved prompt"
        );
        assert_eq!(r.provenance.body, BodyOrigin::Spliced);
    }

    /// A body WITHOUT the token replaces the base body wholesale — §4 makes
    /// `{{ base }}` the thing that opts into composition, so its absence is the
    /// replacement case. The base's non-body fields still inherit.
    #[test]
    fn body_without_the_token_replaces_wholesale() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path();
        write_profile(p, "alice", "---\nextends: swe\n---\nOnly my words.\n");
        let r = resolve_with(p, "alice", TWO_VERSIONS).expect("resolves");
        assert_eq!(r.prompt, "Only my words.");
        assert!(!r.prompt.contains("v2 body"), "the base body is replaced");
        assert_eq!(r.provenance.body, BodyOrigin::Overlay);
        assert_eq!(r.model, "opus", "non-body fields still inherit");
    }

    /// Every occurrence of the literal token splices, so a two-token file gets
    /// the base twice. Pinned because it is the direct consequence of the token
    /// being a literal splice rather than a template tag.
    #[test]
    fn every_occurrence_of_the_token_splices() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path();
        write_profile(
            p,
            "alice",
            "---\nextends: swe\n---\n{{ base }}\n---\n{{ base }}\n",
        );
        let r = resolve_with(p, "alice", TWO_VERSIONS).expect("resolves");
        assert_eq!(r.prompt.matches("v2 body").count(), 2);
    }

    /// The splice is a LITERAL token match, deliberately not Liquid (§9): a
    /// whitespace variant is left alone, and a stray `{{ ... }}` that Liquid
    /// would choke on renders untouched instead of failing the profile.
    #[test]
    fn splicing_is_literal_not_liquid() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path();
        write_profile(
            p,
            "alice",
            "---\nextends: swe\n---\n{{base}} {{ issue.identifier }} {% if x %}\n",
        );
        let r = resolve_with(p, "alice", TWO_VERSIONS).expect("a non-Liquid body still resolves");
        assert_eq!(r.prompt, "{{base}} {{ issue.identifier }} {% if x %}");
        assert_eq!(r.provenance.body, BodyOrigin::Overlay);
    }

    // ---- §4: fork ----

    /// A fork is self-contained: Rhapsody contributes nothing, so no field
    /// inherits and the body is the file's own (§4).
    #[test]
    fn fork_is_self_contained() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path();
        write_profile(p, "alice", "---\nextends: none\n---\nMine alone.\n");
        let r = resolve_with(p, "alice", TWO_VERSIONS).expect("resolves");
        assert_eq!(r.prompt, "Mine alone.");
        assert_eq!(r.model, "", "a fork inherits no model");
        assert_eq!(r.effort, "");
        assert!(r.capabilities.is_empty(), "a fork inherits no capabilities");
        assert!(r.tools.is_empty());
        assert_eq!(r.provenance.base, None);
        assert_eq!(r.provenance.drift, None);
        assert_eq!(r.provenance.model, Origin::Unset);
    }

    /// An ABSENT `extends:` is a fork, not an implicit overlay of the same-named
    /// built-in: §4 requires "I never touched this" and "I own this" to be
    /// distinguishable in the file itself, so a file that declares no base has
    /// none.
    #[test]
    fn absent_extends_is_a_fork() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path();
        write_profile(p, "swe", "---\nmodel: haiku\n---\nJust this.\n");
        let r = resolve_with(p, "swe", TWO_VERSIONS).expect("resolves");
        assert_eq!(r.provenance.base, None, "no `extends:` ⇒ no base");
        assert_eq!(r.model, "haiku");
        assert_eq!(r.prompt, "Just this.");
    }

    /// A file with no front matter at all is a body-only fork.
    #[test]
    fn no_front_matter_is_a_body_only_fork() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path();
        write_profile(p, "alice", "Just a prompt.\n");
        let r = resolve_with(p, "alice", TWO_VERSIONS).expect("resolves");
        assert_eq!(r.prompt, "Just a prompt.");
        assert_eq!(r.provenance.base, None);
    }

    /// `fork_text` round-trips: the materialised file re-resolves to the same
    /// prompt and fields with no base at all, which is what makes `teams fork`
    /// a one-way door the user then owns (§4).
    #[test]
    fn fork_text_round_trips_to_the_same_resolution() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path();
        let resolved = resolve_with(p, "swe", TWO_VERSIONS).expect("built-in resolves");
        let text = fork_text(&resolved).expect("fork text");
        assert!(text.starts_with("---\nextends: none\n"), "text = {text:?}");

        let forked = dir.path().join("forked");
        write_profile(&forked, "swe", &text);
        let again = resolve_with(&forked, "swe", TWO_VERSIONS).expect("the fork resolves");
        assert_eq!(again.prompt, resolved.prompt);
        assert_eq!(again.model, resolved.model);
        assert_eq!(again.effort, resolved.effort);
        assert_eq!(again.capabilities, resolved.capabilities);
        assert_eq!(again.tools, resolved.tools);
        assert_eq!(again.provenance.base, None, "a fork has no base");
    }

    /// Forking a PINNED profile materialises the pinned text and drops the pin,
    /// so the drift report goes away with it — the user now owns the prose.
    #[test]
    fn forking_a_pinned_profile_drops_the_pin_and_its_drift() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path();
        write_profile(p, "alice", "---\nextends: swe@1\n---\n");
        let pinned = resolve_with(p, "alice", TWO_VERSIONS).expect("resolves");
        assert!(pinned.provenance.drift.is_some());

        let forked_dir = dir.path().join("forked");
        write_profile(
            &forked_dir,
            "alice",
            &fork_text(&pinned).expect("fork text"),
        );
        let forked = resolve_with(&forked_dir, "alice", TWO_VERSIONS).expect("resolves");
        assert_eq!(forked.prompt, "v1 body", "the pinned prose is materialised");
        assert_eq!(forked.provenance.drift, None);
        assert_eq!(forked.provenance.base, None);
    }

    // ---- loud rejection ----

    /// An `extends` naming something that is not a built-in is rejected — and
    /// this is also what makes an `extends` CHAIN, and therefore a cycle,
    /// unreachable: a base may only ever be a built-in, and built-ins carry no
    /// `extends` of their own, so resolution is depth-1 by construction.
    #[test]
    fn unknown_extends_target_is_rejected_loudly() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path();
        write_profile(p, "alice", "---\nextends: nosuch\n---\n");
        write_profile(p, "nosuch", "---\nextends: none\n---\nA user file.\n");
        let err = resolve_with(p, "alice", TWO_VERSIONS).expect_err("must reject");
        let msg = err.to_string();
        assert!(msg.starts_with("profile_unknown:"), "msg = {msg:?}");
        assert!(
            msg.contains("not a built-in profile"),
            "the message must say WHY a user file is not a valid base: {msg:?}"
        );
    }

    /// Self-extends: `alice.md` with `extends: alice` cannot recurse into
    /// itself, because a base is looked up in the built-ins and `alice` is not
    /// one. It terminates with a loud error rather than spinning.
    #[test]
    fn self_extends_is_rejected_loudly() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path();
        write_profile(p, "alice", "---\nextends: alice\n---\nbody\n");
        let err = resolve_with(p, "alice", TWO_VERSIONS).expect_err("must reject");
        assert!(err.to_string().starts_with("profile_unknown:"));
    }

    /// The one self-NAMED case that is legal and is NOT a cycle: `swe.md` with
    /// `extends: swe` overlays the BUILT-IN `swe`, which is §2.2's documented
    /// common case. It must resolve, not be mistaken for self-recursion.
    #[test]
    fn a_same_named_overlay_layers_on_the_builtin_not_itself() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path();
        write_profile(
            p,
            "swe",
            "---\nextends: swe\n---\n{{ base }}\n\nHouse rule.\n",
        );
        let r = resolve_with(p, "swe", TWO_VERSIONS).expect("a same-named overlay resolves");
        assert_eq!(r.prompt, "v2 body\n\nHouse rule.");
        assert_eq!(
            r.provenance.base,
            Some(BaseRef {
                name: "swe".to_string(),
                version: 2,
                pinned: false
            })
        );
    }

    /// A pin at a version that does not exist is rejected rather than silently
    /// snapping to the nearest one.
    #[test]
    fn unknown_pinned_version_is_rejected_loudly() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path();
        write_profile(p, "alice", "---\nextends: swe@99\n---\n");
        let err = resolve_with(p, "alice", TWO_VERSIONS).expect_err("must reject");
        let msg = err.to_string();
        assert!(msg.starts_with("profile_unknown:"), "msg = {msg:?}");
        assert!(msg.contains("swe@99"), "msg = {msg:?}");
    }

    /// A malformed `extends` value is an error, not a silent fork.
    #[test]
    fn malformed_extends_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path();
        for bad in ["swe@", "swe@x", "swe@0", "swe@-1", "SWE", "../swe"] {
            write_profile(p, "alice", &format!("---\nextends: {bad}\n---\n"));
            let err = resolve_with(p, "alice", TWO_VERSIONS)
                .err()
                .unwrap_or_else(|| panic!("extends {bad:?} must be rejected"));
            assert!(
                err.to_string().starts_with("profile_invalid:"),
                "extends {bad:?} → {err}"
            );
        }
    }

    /// A profile NAME is charset-checked before it becomes `<name>.md`, so a
    /// traversal attempt can never escape the profiles directory.
    #[test]
    fn profile_name_is_charset_checked() {
        let dir = tempfile::tempdir().expect("tempdir");
        for bad in ["../etc/passwd", "a/b", "Alice", "", ".", "swe.md"] {
            let err = resolve(dir.path(), bad).expect_err("must reject {bad:?}");
            assert!(
                err.to_string().starts_with("profile_invalid:"),
                "{bad:?} → {err}"
            );
        }
    }

    /// A profile that is neither a file nor a built-in is unknown, and the
    /// message names both places that were looked at.
    #[test]
    fn absent_profile_with_no_builtin_is_unknown() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = resolve(dir.path(), "nobody").expect_err("must reject");
        let msg = err.to_string();
        assert!(msg.starts_with("profile_unknown:"), "msg = {msg:?}");
        assert!(msg.contains("no built-in"), "msg = {msg:?}");
    }

    /// Front matter that is not a map, and front matter that will not parse,
    /// both surface as errors rather than an empty profile.
    #[test]
    fn malformed_front_matter_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path();
        write_profile(p, "alice", "---\n- a\n- b\n---\nbody\n");
        assert!(
            resolve(p, "alice")
                .expect_err("not-a-map must error")
                .to_string()
                .starts_with("profile_parse_error:")
        );
        write_profile(p, "alice", "---\nextends: 3\n---\nbody\n");
        assert!(
            resolve(p, "alice")
                .expect_err("a non-string extends must error")
                .to_string()
                .starts_with("profile_parse_error:")
        );
    }

    /// A key written with nothing under it (`capabilities:` — a YAML null, which
    /// is what commenting out the entries leaves) is the UNSET value, not a
    /// type error, so it inherits.
    #[test]
    fn null_valued_keys_are_unset_not_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path();
        write_profile(
            p,
            "alice",
            "---\nextends: swe\nmodel:\neffort:\ncapabilities:\ntools:\n---\n",
        );
        let r = resolve_with(p, "alice", TWO_VERSIONS).expect("resolves");
        assert_eq!(r.model, "opus", "a null key inherits");
        assert_eq!(r.capabilities.len(), 2);
    }

    // ---- roster validation (§4, T1's disable-loudly semantics) ----

    /// With profiles now resolvable, a roster entry naming an unknown one is
    /// reported — the "broken agent discovered at dispatch time" §4 exists to
    /// prevent.
    #[test]
    fn unknown_roster_profile_is_reported() {
        let dir = tempfile::tempdir().expect("tempdir");
        let teams = Teams {
            roster: vec![
                Identity {
                    name: "alice".to_string(),
                    profile: "swe".to_string(),
                    ..Identity::default()
                },
                Identity {
                    name: "bob".to_string(),
                    profile: "nosuch".to_string(),
                    ..Identity::default()
                },
                Identity {
                    name: "carol".to_string(),
                    profile: String::new(),
                    ..Identity::default()
                },
            ],
            ..Teams::disabled()
        };
        let issues = check_roster(&teams, dir.path());
        assert_eq!(issues.len(), 1, "issues = {issues:?}");
        match &issues[0] {
            RosterIssue::Unresolvable {
                identity, profile, ..
            } => {
                assert_eq!(identity, "bob");
                assert_eq!(profile, "nosuch");
            }
            other => panic!("want Unresolvable, got {other:?}"),
        }
        assert!(
            issues[0].to_string().contains("nosuch"),
            "the log line must name the profile: {}",
            issues[0]
        );
    }

    /// Drift reaches the roster report in §4's own words, so the startup warning
    /// reads like the design's example.
    #[test]
    fn roster_drift_is_reported_in_the_designs_words() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path();
        write_profile(p, "swe", "---\nextends: swe@1\n---\n");
        let teams = Teams {
            roster: vec![Identity {
                name: "alice".to_string(),
                profile: "swe".to_string(),
                ..Identity::default()
            }],
            ..Teams::disabled()
        };
        let issue = RosterIssue::Drift {
            identity: "alice".to_string(),
            profile: "swe".to_string(),
            drift: resolve_with(p, "swe", TWO_VERSIONS)
                .expect("resolves")
                .provenance
                .drift
                .expect("pinned to v1 while v2 ships ⇒ drift"),
        };
        assert_eq!(
            issue.to_string(),
            "alice's profile \"swe\" overlays swe@1; the built-in is now swe@2"
        );
        // The SHIPPED registry now ships swe@2 too (T4's retain section), so
        // this pin is real drift and the boot-time roster check reports it —
        // exactly the operator warning §4 designed the pin to earn.
        assert_eq!(
            check_roster(&teams, p)
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            vec!["alice's profile \"swe\" overlays swe@1; the built-in is now swe@2"]
        );
    }
}
