//! Rhapsody Teams config — `~/.rhapsody/teams.yaml`: the toggle, the manager
//! settings, the memory-backend settings and the roster of identities
//! (design record `~/.rhapsody/docs/STUDIO-572-rhapsody-teams.md`, §2.2).
//!
//! Like [`crate::capabilities`] (BO-11) this is user-editable, non-parity data
//! that lives in its OWN file rather than in `WORKFLOW.md` front matter:
//! `encode.rs` rebuilds front matter from the typed `Raw` mirror and prunes
//! anything it does not model, so a hand-written `teams:` block would silently
//! vanish the first time the dashboard's config editor saved (§2.1).
//!
//! One deliberate divergence from the capabilities precedent, also §2.1:
//! **`teams.yaml` is never seeded.** `capabilities::load_or_seed` writes the
//! file on first read, which is harmless there and would be a behaviour change
//! here — a disabled feature must not create a file. **An absent file IS the
//! off state, and it is the shipped state.** The file is created only by an
//! explicit enable.
//!
//! This slice (T1, §0.11.8) is inert: the types are carried as config and
//! nothing reads them. Every `manager` and `memory` field below is parsed,
//! defaulted and validated here and consumed by NOTHING — the routing (T3a),
//! triage (T3b) and memory (T4) slices are where they acquire behaviour.

use crate::workflow::{create_temp, write_temp_and_rename};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::os::unix::fs::DirBuilderExt;
use std::path::Path;

/// How the manager decides which identity takes a ticket (§2.2, §3.2, §3.5).
/// Config only in T1 — nothing consumes it yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ManagerMode {
    /// Single-identity Teams: no routing at all (§3.5).
    #[serde(rename = "off")]
    Off,
    /// Deterministic only: the ticket's `rhapsody:@<name>` label, then
    /// roster-labels ∩ ticket-labels (§0.11.2 Tier 0 + fallback). The §2.2
    /// default.
    #[default]
    #[serde(rename = "labels")]
    Labels,
    /// Deterministic, plus an off-loop triage model turn for tickets no label
    /// matched (§0.11.2). The model turn is a FUTURE slice (T3b), not this one.
    #[serde(rename = "labels+model")]
    LabelsModel,
}

/// Where a teammate's memory bank lives (§2.2, §5.4). Config only in T1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum MemoryBackend {
    /// No memory at all.
    #[serde(rename = "none")]
    None,
    /// On-disk banks under `memory.path` — the laptop-native default (§5.4),
    /// and so the §2.2 default.
    #[default]
    #[serde(rename = "local")]
    Local,
    /// A remote Hindsight MCP endpoint (§5.4); the T8 slice.
    #[serde(rename = "hindsight")]
    Hindsight,
}

/// `manager.max_tokens` — the hard cap on the (future) triage arbitration turn.
const DEFAULT_MAX_TOKENS: i64 = 4000;
/// `manager.timeout_ms` — exceeded ⇒ fall back to the deterministic answer.
///
/// **60 seconds, raised from the 5000ms §2.2 specified (STUDIO-673).** A triage
/// turn spawns a `claude -p` subprocess, authenticates it, and waits on a
/// model; measured across a full day of live triage on v0.3.4-rc.8
/// (2026-08-31), *every* turn lost that race and 100% of the day's assignments
/// came from the deterministic fallback — `labels+model` was silently pure
/// `labels`. A bound the real work can never meet is not a bound, it is the
/// feature switched off, so the budget is now one a turn can finish inside.
/// Nothing else about it changes: exceeded still means the deterministic answer
/// stands, and dispatch still never waits on it.
const DEFAULT_TIMEOUT_MS: i64 = 60000;

/// The smallest `manager.timeout_ms` a *model* turn can realistically finish
/// inside — the floor the daemon WARNS about at boot (STUDIO-673) and never
/// clamps to. Subprocess spawn plus one model round-trip is seconds, not
/// milliseconds, so a smaller value starves the manager: the turn always times
/// out and the deterministic router decides every ticket, visibly only to
/// whoever reads the room's failure reasons. The operator's explicit value
/// still wins — this number buys a diagnosis, not a policy.
pub const MIN_MODEL_TIMEOUT_MS: i64 = 15000;

/// `memory.bank_prefix` — a bank id is `<bank_prefix><name>`.
const DEFAULT_BANK_PREFIX: &str = "agent-";
/// `memory.recall_top_k` — how many facts a recall returns.
const DEFAULT_RECALL_TOP_K: i64 = 8;
/// `prompt_budget_bytes` — the ONE total byte budget the Teams composer spends
/// across the whole teammate prepend (§0.11.6). See [`Teams::prompt_budget_bytes`]
/// for why the default is this size and not smaller.
pub const DEFAULT_PROMPT_BUDGET_BYTES: i64 = 16000;

fn default_max_tokens() -> i64 {
    DEFAULT_MAX_TOKENS
}

fn default_timeout_ms() -> i64 {
    DEFAULT_TIMEOUT_MS
}

fn default_bank_prefix() -> String {
    DEFAULT_BANK_PREFIX.to_string()
}

fn default_recall_top_k() -> i64 {
    DEFAULT_RECALL_TOP_K
}

fn default_prompt_budget_bytes() -> i64 {
    DEFAULT_PROMPT_BUDGET_BYTES
}

/// `quorum.reviewers` — how many teammates a handoff fans review tickets out
/// to. §0.12: "at least two" is both the floor of §0.6 and the default.
pub const DEFAULT_QUORUM_REVIEWERS: i64 = 2;

/// The smallest reviewer count honoured. A quorum of zero is not a quorum; it
/// is the feature switched off, and `enabled: false` is how you say that. So a
/// nonsensical `reviewers` (0, negative) clamps UP to one rather than silently
/// turning an enabled quorum into a no-op — the same "a non-positive bound must
/// not mean two different things" stance [`Memory::recall_top_k`] takes.
pub const MIN_QUORUM_REVIEWERS: i64 = 1;

fn default_quorum_reviewers() -> i64 {
    DEFAULT_QUORUM_REVIEWERS
}

/// The `manager:` block (§2.2). Carried as config in T1; the routing function
/// that reads `default_identity` is T3a and the model turn that reads `model` /
/// `max_tokens` / `timeout_ms` is T3b's off-loop triage task (§0.11.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manager {
    #[serde(default)]
    pub mode: ManagerMode,
    /// Who takes a ticket nothing matched; empty ⇒ run without an identity.
    /// Validated to name a roster entry when non-empty.
    #[serde(default)]
    pub default_identity: String,
    /// Consulted ONLY in `labels+model`, and only on a Tier-1 miss.
    #[serde(default)]
    pub model: String,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: i64,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: i64,
}

impl Default for Manager {
    fn default() -> Self {
        Self {
            mode: ManagerMode::default(),
            default_identity: String::new(),
            model: String::new(),
            max_tokens: DEFAULT_MAX_TOKENS,
            timeout_ms: DEFAULT_TIMEOUT_MS,
        }
    }
}

/// The `quorum:` block — Rhapsody Teams' **notified review** (§0.6, and the
/// trigger/cap decision recorded as §0.12 on 2026-08-30).
///
/// When a teammate hands off a PR, the daemon fans review tickets out to the
/// least-loaded other teammates so at least two pairs of eyes read the work
/// independently. §0.6 calls this "the most expensive item in the revision" —
/// it costs `reviewers` extra agent runs per handoff — which is why
/// [`enabled`](Self::enabled) defaults to **false** and an absent `quorum:`
/// section is the off state, exactly as an absent `teams.yaml` is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Quorum {
    /// The opt-in switch (§0.12's "cost control"). Default **false**: the
    /// quorum is per-installation opt-in, never ambient.
    #[serde(default)]
    pub enabled: bool,
    /// How many teammates review one handoff, clamped to the roster minus the
    /// author. Default 2; see [`Quorum::effective_reviewers`] for the floor.
    #[serde(default = "default_quorum_reviewers")]
    pub reviewers: i64,
}

impl Default for Quorum {
    fn default() -> Self {
        Self {
            enabled: false,
            reviewers: DEFAULT_QUORUM_REVIEWERS,
        }
    }
}

impl Quorum {
    /// [`Quorum::reviewers`] with the floor applied — the number the fan-out
    /// actually asks for, before the roster clamps it further.
    pub fn effective_reviewers(&self) -> usize {
        usize::try_from(self.reviewers.max(MIN_QUORUM_REVIEWERS)).unwrap_or(
            // Unreachable for any i64 >= 1 on a 64-bit target; a 32-bit target
            // with an absurd `reviewers` degrades to the default rather than
            // panicking, because a config value must never take the daemon down.
            DEFAULT_QUORUM_REVIEWERS as usize,
        )
    }
}

/// The `memory:` block (§2.2). Carried as config in T1 — no backend is
/// constructed, no endpoint is dialled, no bank directory is created. §2.4
/// row 8: when Teams is off there is no code path here at all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Memory {
    #[serde(default)]
    pub backend: MemoryBackend,
    /// `local` backend: the bank directory. Empty ⇒ `~/.rhapsody/teams/banks/`.
    #[serde(default)]
    pub path: String,
    /// `hindsight` backend: the service base URL (§2.2 spells the example with a
    /// `/mcp/` suffix; the deployed contract is the REST surface under `/v1/`, and
    /// [`HindsightBackend`](crate::hindsight::HindsightBackend) accepts either
    /// spelling). Empty with `backend: hindsight` ⇒ the daemon warns and runs
    /// memoryless.
    #[serde(default)]
    pub endpoint: String,
    /// `hindsight` backend: the credential sent as the `Authorization` header.
    ///
    /// **Not in §2.2's sketch — it comes from the deployed service** (STUDIO-660):
    /// hindsight 0.9.1 answers every `/v1/**` path with
    /// `401 {"detail":"Authentication failed: Invalid API key"}` when the header is
    /// absent or wrong, so a URL alone cannot reach a bank. Additive and defaulted
    /// to empty, so `local`, `none` and Teams-off parse byte-identically.
    ///
    /// A bare `$NAME` is read from the environment instead of used literally — the
    /// same indirection `tracker.api_key` uses in `WORKFLOW.md`
    /// ([`crate::resolve::resolve_var`]) — so the secret need not sit in
    /// `teams.yaml`. Empty ⇒ no `Authorization` header at all, which is what an
    /// unauthenticated deployment wants.
    #[serde(default)]
    pub api_key: String,
    #[serde(default = "default_bank_prefix")]
    pub bank_prefix: String,
    #[serde(default = "default_recall_top_k")]
    pub recall_top_k: i64,
}

impl Default for Memory {
    fn default() -> Self {
        Self {
            backend: MemoryBackend::default(),
            path: String::new(),
            endpoint: String::new(),
            api_key: String::new(),
            bank_prefix: default_bank_prefix(),
            recall_top_k: DEFAULT_RECALL_TOP_K,
        }
    }
}

/// One roster entry — an *identity* (§1). Deliberately a short structured
/// record with **no prompt field**: prompt text belongs to a profile, and
/// keeping the two in different kinds of storage is what stops the identity
/// collapsing into "a rename of `.claude/agents/`" (§1). `profile` is a plain
/// string here because profiles are T2; T1 does not resolve or read one.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Identity {
    /// The teammate's name. Label-safe by validation: it appears inside a
    /// `rhapsody:@<name>` Linear label (§0.11.1), so the charset is pinned.
    #[serde(default)]
    pub name: String,
    /// The profile this identity wears. Unresolved in T1 (profiles are T2).
    #[serde(default)]
    pub profile: String,
    /// What the deterministic router matches against the ticket's labels (T3a).
    #[serde(default)]
    pub labels: Vec<String>,
    /// Memory bank id; empty ⇒ `<memory.bank_prefix><name>`.
    #[serde(default)]
    pub bank: String,
    /// 0 ⇒ unlimited (§3.4).
    #[serde(default)]
    pub max_concurrent: i64,
}

/// The parsed `~/.rhapsody/teams.yaml`.
///
/// [`Teams::default`] is [`Teams::disabled`]: the schema's own defaults with
/// `enabled: false`, which is exactly what an absent file means (§2.1) and what
/// an empty-but-present file parses to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Teams {
    /// The one toggle the whole feature lives behind (§2). Default false.
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub manager: Manager,
    #[serde(default)]
    pub memory: Memory,
    /// The `quorum:` block (STUDIO-659, T7; §0.6, §0.12). An ABSENT section is
    /// [`Quorum::default`], i.e. disabled — the whole point of the opt-in.
    #[serde(default)]
    pub quorum: Quorum,
    #[serde(default)]
    pub roster: Vec<Identity>,
    /// **The one total byte budget** for the whole Teams turn-1 prepend
    /// (STUDIO-650, T5; §0.11.6). Optional, and the only new key this slice
    /// adds.
    ///
    /// §0.11.6 gives the turn-1 prompt a single budget owner because by T5 it
    /// has four independent growing tenants — capabilities, the identity
    /// header + profile prose, room catch-up and memory recall — each with a
    /// local bound and no aggregate. The composer spends this budget in the
    /// fixed order capabilities → teammate header → room catch-up → memory
    /// recall, and on overflow drops **oldest room items first, then recall
    /// items, never the identity header**.
    ///
    /// The default is deliberately generous rather than tight: it must be large
    /// enough that a room-empty prompt is **byte-identical to T4's**, so
    /// enabling the room changes nothing for a team that has not used it. Zero
    /// or negative ⇒ [`DEFAULT_PROMPT_BUDGET_BYTES`], for
    /// [`Memory::recall_top_k`]'s reason: a non-positive bound must not silently
    /// mean "unbounded" in one place and "nothing" in another.
    #[serde(default = "default_prompt_budget_bytes")]
    pub prompt_budget_bytes: i64,
}

/// Hand-written rather than derived, for the reason [`Manager`] and [`Memory`]
/// are: `prompt_budget_bytes` has a non-zero schema default, and a derived
/// `Default` would make `Teams::disabled()` disagree with what parsing an empty
/// file yields. `disabled_matches_an_empty_file` pins the two together.
impl Default for Teams {
    fn default() -> Self {
        Self {
            enabled: false,
            manager: Manager::default(),
            memory: Memory::default(),
            quorum: Quorum::default(),
            roster: Vec::new(),
            prompt_budget_bytes: DEFAULT_PROMPT_BUDGET_BYTES,
        }
    }
}

impl Teams {
    /// [`Teams::prompt_budget_bytes`] with the non-positive fallback applied —
    /// the number the composer actually spends.
    pub fn effective_prompt_budget(&self) -> usize {
        usize::try_from(self.prompt_budget_bytes)
            .ok()
            .filter(|n| *n > 0)
            .unwrap_or(DEFAULT_PROMPT_BUDGET_BYTES as usize)
    }

    /// The configured `manager.timeout_ms` when it is too small for the model
    /// turn it bounds, else `None` — the whole decision behind the daemon's
    /// boot-time starvation warning (STUDIO-673), kept beside the constants it
    /// compares so the boot path only has to render it.
    ///
    /// Three things it deliberately does not do. It does not fire outside
    /// `labels+model`: no other mode runs a model turn, so no other mode can be
    /// starved by this value. It does not fire on a non-positive value: that
    /// means "no value", and the triage task substitutes the schema default for
    /// it. And it does not clamp — it returns the operator's own number, to be
    /// named back to them.
    pub fn starved_manager_timeout_ms(&self) -> Option<i64> {
        (self.enabled
            && self.manager.mode == ManagerMode::LabelsModel
            && self.manager.timeout_ms > 0
            && self.manager.timeout_ms < MIN_MODEL_TIMEOUT_MS)
            .then_some(self.manager.timeout_ms)
    }
}

/// Why a `teams.yaml` was rejected. The daemon boot turns any of these into ONE
/// loud log line plus [`Teams::disabled`] — never a startup failure (§2.1).
/// `rhapsody-config` deliberately does no logging of its own (like `core`,
/// `store` and `workspace`), so the error carries the reason to the caller that
/// owns the log; see [`Teams::try_load`].
#[derive(thiserror::Error, Debug)]
pub enum TeamsError {
    #[error("teams_io_error: {0}")]
    Io(String),
    #[error("teams_parse_error: {0}")]
    Parse(String),
    #[error("teams_invalid: {0}")]
    Invalid(String),
}

/// `^[a-z][a-z0-9-]*$`, hand-rolled because `rhapsody-config` has no `regex`
/// dependency and this slice adds none.
///
/// The charset is pinned NOW, before anything reads the roster, because a name
/// is interpolated into a `rhapsody:@<name>` Linear label (§0.11.1) and into a
/// `<bank_prefix><name>` bank id (§2.2). Widening it later would be a
/// compatibility break in two external namespaces at once.
pub(crate) fn is_label_safe(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

impl Teams {
    /// The off state: the feature's shipped configuration, and what an absent,
    /// unreadable, malformed or invalid `teams.yaml` yields (§2.4 row 1).
    pub fn disabled() -> Self {
        Self::default()
    }

    /// Loads `path`, best-effort and TOTAL: an absent, unreadable, malformed or
    /// invalid file all yield [`Teams::disabled`] (§2.4 row 1). Never seeds —
    /// reading a `teams.yaml` that is not there does not create one (§2.1).
    ///
    /// Callers that need to REPORT why a present file was rejected use
    /// [`Teams::try_load`]; that is what the daemon boot does, so a broken file
    /// is loud rather than silently off.
    pub fn load(path: &Path) -> Self {
        Self::try_load(path).unwrap_or_else(|_| Self::disabled())
    }

    /// [`Teams::load`] with the reason preserved. An ABSENT file is
    /// `Ok(Teams::disabled())`, not an error: absence is the shipped state, not
    /// a failure. A present file that cannot be read, parsed or validated is
    /// `Err` — the caller logs it and falls back to [`Teams::disabled`].
    pub fn try_load(path: &Path) -> Result<Self, TeamsError> {
        if !path.exists() {
            return Ok(Self::disabled());
        }
        let text = std::fs::read_to_string(path).map_err(|e| TeamsError::Io(e.to_string()))?;
        let teams = Self::parse(&text)?;
        teams.validate()?;
        Ok(teams)
    }

    /// Parses YAML into [`Teams`], applying the §2.2 defaults to absent keys.
    ///
    /// Every field is `#[serde(default)]`, which covers three shapes an
    /// operator's file actually takes: a key that is absent, a key written with
    /// nothing under it (`manager:` — a null, which is what commenting out the
    /// sub-keys leaves behind), and a wholly empty/`---`-only document. All
    /// three are the fully-defaulted — i.e. disabled — config, not a parse
    /// error. `parses_null_valued_blocks` and `empty_file_applies_schema_defaults`
    /// pin that, since it is serde's behaviour rather than ours.
    fn parse(text: &str) -> Result<Self, TeamsError> {
        serde_yaml_ng::from_str(text).map_err(|e| TeamsError::Parse(e.to_string()))
    }

    /// Syntactic validation only — profiles are T2, so `profile` is not
    /// resolved here and an unknown one is not an error yet.
    ///
    /// Checked: every roster `name` is label-safe (§0.11.1); no name is one of
    /// the daemon's own [reserved speakers](crate::room::RESERVED_IDENTITIES)
    /// (STUDIO-661); no two entries share a name; `manager.default_identity`,
    /// when set, names a roster entry. Runs regardless of `enabled` so a user
    /// editing the file sees the complaint before they flip the toggle, not
    /// after.
    ///
    /// `pub` since STUDIO-652 so the Settings-page enable flow rejects a
    /// candidate roster with **exactly** the daemon's own complaint, verbatim,
    /// rather than with a second implementation of these rules that could
    /// disagree with the one that decides whether the file loads at boot.
    pub fn validate(&self) -> Result<(), TeamsError> {
        let mut seen: HashSet<&str> = HashSet::with_capacity(self.roster.len());
        for entry in &self.roster {
            if !is_label_safe(&entry.name) {
                return Err(TeamsError::Invalid(format!(
                    "roster name {:?} is not label-safe (must match ^[a-z][a-z0-9-]*$; it becomes a `rhapsody:@<name>` label)",
                    entry.name
                )));
            }
            // Reserved before duplicate-checking, so a roster with two entries
            // named `operator` is told the real problem rather than the second
            // one.
            if crate::room::RESERVED_IDENTITIES.contains(&entry.name.as_str()) {
                return Err(TeamsError::Invalid(format!(
                    "roster name {:?} is reserved: `{}` and `manager` are the daemon's own voices in the team room, not teammates, so a roster entry wearing either would be indistinguishable from one in every catch-up line — rename this entry",
                    entry.name,
                    crate::room::OPERATOR_IDENTITY,
                )));
            }
            if !seen.insert(entry.name.as_str()) {
                return Err(TeamsError::Invalid(format!(
                    "duplicate roster name {:?}",
                    entry.name
                )));
            }
        }
        if !self.manager.default_identity.is_empty()
            && !seen.contains(self.manager.default_identity.as_str())
        {
            return Err(TeamsError::Invalid(format!(
                "manager.default_identity {:?} is not a roster entry",
                self.manager.default_identity
            )));
        }
        Ok(())
    }

    /// Writes `teams.yaml` — **the only code in the tree that creates it**
    /// (STUDIO-652), and only ever when a caller explicitly asks.
    ///
    /// This does not weaken §2.1's never-seed rule; it is what that rule leaves
    /// room for. "Absent ≡ off, and nothing creates it implicitly" is about
    /// *reads*: [`Teams::load`] and [`Teams::try_load`] still never write, so
    /// booting, reading, resolving and `teams show` all leave an absent file
    /// absent. An operator deliberately enabling Teams is the explicit act the
    /// rule names as the one way the file appears.
    ///
    /// Validation runs FIRST and a rejection writes nothing, so a bad edit can
    /// never replace a working file — the discipline `POST /api/v1/config`
    /// already applies to `WORKFLOW.md`. The write itself is the crate's
    /// `~/.rhapsody` convention (temp file + chmod + rename), so no reader ever
    /// observes half a config.
    ///
    /// It writes the CANONICAL serialization: every schema default made
    /// explicit, in field order, with comments and hand-written key order in an
    /// existing file not preserved. That is the same property `workflow::save`
    /// has for `WORKFLOW.md`, and the caller is expected to say so before
    /// overwriting a file a human wrote — the Settings enable flow does.
    pub fn save(path: &Path, teams: &Teams) -> Result<(), TeamsError> {
        teams.validate()?;
        let yaml = serde_yaml_ng::to_string(teams).map_err(|e| TeamsError::Parse(e.to_string()))?;
        let dir = match path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            _ => Path::new("."),
        };
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|e| TeamsError::Io(e.to_string()))?;
        let (file, tmp_path) =
            create_temp(dir, "teams").map_err(|e| TeamsError::Io(e.to_string()))?;
        write_temp_and_rename(file, &tmp_path, yaml.as_bytes(), 0o600, path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp_path);
            TeamsError::Io(e.to_string())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// §2.4 row 1, the inertness claim's first line: an absent `teams.yaml` is
    /// the off state AND stays absent. This is the deliberate divergence from
    /// `capabilities::load_or_seed`, which WOULD have written the file here —
    /// a disabled feature must not create one (§2.1).
    #[test]
    fn absent_file_is_disabled_and_is_never_seeded() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("teams.yaml");
        assert!(!path.exists());

        assert_eq!(Teams::load(&path), Teams::disabled());
        assert!(
            !path.exists(),
            "teams.yaml must never be seeded: reading an absent file created {}",
            path.display()
        );
        // The fallible entry point agrees, and calls absence a success rather
        // than an error — there is nothing for the daemon to log.
        assert_eq!(
            Teams::try_load(&path).expect("absent is Ok"),
            Teams::disabled()
        );
        assert!(!path.exists(), "try_load must not seed either");
    }

    /// **`save` is the explicit enable §2.1 leaves room for** (STUDIO-652): it
    /// creates the file, and a `load` of what it wrote is the value that went
    /// in. Round-tripping through YAML is the property that matters — the
    /// Settings editor writes a `Teams` and the daemon boots the same one.
    #[test]
    fn save_creates_the_file_and_round_trips_through_load() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("teams.yaml");
        let teams = Teams {
            enabled: true,
            manager: Manager {
                mode: ManagerMode::LabelsModel,
                default_identity: "alice".to_string(),
                ..Manager::default()
            },
            memory: Memory {
                backend: MemoryBackend::None,
                ..Memory::default()
            },
            roster: vec![Identity {
                name: "alice".to_string(),
                profile: "swe".to_string(),
                labels: vec!["rust".to_string(), "config".to_string()],
                ..Identity::default()
            }],
            ..Teams::disabled()
        };

        Teams::save(&path, &teams).expect("save");
        assert!(path.exists(), "save must create teams.yaml");
        assert_eq!(Teams::load(&path), teams, "save → load round-trips");
    }

    /// A rejected config writes NOTHING — not a new file, and not over a
    /// working one. The same discipline `POST /api/v1/config` applies to
    /// WORKFLOW.md: a bad edit can never corrupt a config that loads.
    #[test]
    fn save_validates_first_and_leaves_the_previous_file_untouched() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("teams.yaml");
        let good = Teams {
            enabled: true,
            roster: vec![Identity {
                name: "alice".to_string(),
                ..Identity::default()
            }],
            ..Teams::disabled()
        };
        Teams::save(&path, &good).expect("save the good one");

        let bad = Teams {
            roster: vec![Identity {
                name: "Alice".to_string(), // not label-safe
                ..Identity::default()
            }],
            ..good.clone()
        };
        let err = Teams::save(&path, &bad).expect_err("an invalid roster is rejected");
        assert!(
            matches!(err, TeamsError::Invalid(_)),
            "expected a validation error, got {err}"
        );
        assert_eq!(
            Teams::load(&path),
            good,
            "a rejected save must leave the working file exactly as it was"
        );

        // And onto a path that does not exist yet, a rejection creates nothing at all.
        let fresh = dir.path().join("nested").join("teams.yaml");
        assert!(Teams::save(&fresh, &bad).is_err());
        assert!(!fresh.exists(), "a rejected save must create no file");
    }

    /// STUDIO-673: the shipped `manager.timeout_ms` must be a budget a REAL
    /// triage turn can finish inside. Measured on 2026-08-31 against
    /// v0.3.4-rc.8, the 5000ms this shipped with lost every race in a day of
    /// live triage, so `labels+model` was silently pure `labels`. Pinned twice:
    /// the literal the schema ships, and — in a const block, so the compiler
    /// holds it — its relationship to the floor the daemon warns below.
    #[test]
    fn the_shipped_manager_timeout_clears_the_model_floor() {
        assert_eq!(DEFAULT_TIMEOUT_MS, 60000);
        // A const block, so a future edit that drops the default back under the
        // floor fails to COMPILE rather than to run.
        const {
            assert!(
                DEFAULT_TIMEOUT_MS >= MIN_MODEL_TIMEOUT_MS,
                "the shipped default must not be a value the daemon itself warns about"
            )
        };
        let t = Teams {
            enabled: true,
            manager: Manager {
                mode: ManagerMode::LabelsModel,
                ..Manager::default()
            },
            ..Teams::default()
        };
        assert_eq!(t.starved_manager_timeout_ms(), None);
    }

    /// The boot warning's whole decision (STUDIO-673): a model-consulting
    /// manager whose timeout is below the floor, and nothing else. It reports
    /// the configured value, never a clamped one — the operator's explicit
    /// number still wins.
    #[test]
    fn starved_manager_timeout_reports_only_a_model_mode_below_the_floor() {
        let teams = |mode: ManagerMode, enabled: bool, timeout_ms: i64| Teams {
            enabled,
            manager: Manager {
                mode,
                timeout_ms,
                ..Manager::default()
            },
            ..Teams::default()
        };

        assert_eq!(
            teams(ManagerMode::LabelsModel, true, 5000).starved_manager_timeout_ms(),
            Some(5000),
            "the shipped-5000 case this ticket exists for"
        );
        assert_eq!(
            teams(ManagerMode::LabelsModel, true, MIN_MODEL_TIMEOUT_MS)
                .starved_manager_timeout_ms(),
            None,
            "the floor itself is not starved"
        );
        // Non-positive is "no value", and the triage task substitutes the
        // schema default for it — warning here would name a number nothing
        // ever uses.
        for ms in [0, -1] {
            assert_eq!(
                teams(ManagerMode::LabelsModel, true, ms).starved_manager_timeout_ms(),
                None,
                "({ms})"
            );
        }
        // No other mode runs a model turn, so no other mode can be starved.
        for mode in [ManagerMode::Labels, ManagerMode::Off] {
            assert_eq!(
                teams(mode, true, 5000).starved_manager_timeout_ms(),
                None,
                "({mode:?})"
            );
        }
        assert_eq!(
            teams(ManagerMode::LabelsModel, false, 5000).starved_manager_timeout_ms(),
            None,
            "teams off ⇒ no triage task ⇒ nothing to starve"
        );
    }

    /// The off state is the schema's defaults with the toggle off, and it is
    /// what `Default` yields — so a future consumer that reaches for either
    /// spelling gets the same thing.
    #[test]
    fn disabled_is_default_and_is_off() {
        let t = Teams::disabled();
        assert_eq!(t, Teams::default());
        assert!(!t.enabled);
        assert!(t.roster.is_empty());
    }

    /// §2.2's defaults, pinned: every key absent ⇒ the documented default.
    #[test]
    fn empty_file_applies_schema_defaults() {
        for text in ["", "   \n", "{}", "---\n"] {
            let t = Teams::parse(text).unwrap_or_else(|e| panic!("parse {text:?}: {e}"));
            assert!(!t.enabled, "enabled defaults false ({text:?})");
            assert_eq!(t.manager.mode, ManagerMode::Labels, "({text:?})");
            assert_eq!(t.manager.default_identity, "", "({text:?})");
            assert_eq!(t.manager.model, "", "({text:?})");
            assert_eq!(t.manager.max_tokens, 4000, "({text:?})");
            assert_eq!(t.manager.timeout_ms, 60000, "({text:?})");
            assert_eq!(t.memory.backend, MemoryBackend::Local, "({text:?})");
            assert_eq!(t.memory.path, "", "({text:?})");
            assert_eq!(t.memory.endpoint, "", "({text:?})");
            assert_eq!(t.memory.bank_prefix, "agent-", "({text:?})");
            assert_eq!(t.memory.recall_top_k, 8, "({text:?})");
            assert!(!t.quorum.enabled, "quorum defaults OFF ({text:?})");
            assert_eq!(t.quorum.reviewers, 2, "({text:?})");
            assert!(t.roster.is_empty(), "({text:?})");
            assert_eq!(t, Teams::disabled(), "({text:?})");
        }
    }

    /// A key written with nothing under it — `manager:` with its sub-keys
    /// commented out — is a YAML *null*, not an absent key, and is the shape an
    /// operator's half-edited file actually takes. It must default, not fail:
    /// `#[serde(default)]` covers it today, and this pins that so a serde /
    /// serde_yaml_ng bump cannot quietly turn a plausible file into a parse
    /// error that disables Teams.
    #[test]
    fn parses_null_valued_blocks() {
        for text in [
            "manager:\nmemory:\nroster:\n",
            "enabled: true\nmanager:\n",
            "enabled: true\nroster:\n",
            "enabled: true\nmemory:\n",
            "# every line commented out\n",
        ] {
            let t = Teams::parse(text).unwrap_or_else(|e| panic!("parse {text:?}: {e}"));
            assert_eq!(t.manager, Manager::default(), "({text:?})");
            assert_eq!(t.memory, Memory::default(), "({text:?})");
            assert!(t.roster.is_empty(), "({text:?})");
        }
    }

    /// The §2.2 schema example, verbatim, parses — and the per-entry defaults
    /// fill in for `bob` and `jimmy`, who omit `bank` and `max_concurrent`.
    #[test]
    fn well_formed_file_parses_with_defaults_applied() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("teams.yaml");
        std::fs::write(
            &path,
            concat!(
                "enabled: true\n",
                "manager:\n",
                "  mode: labels+model\n",
                "  default_identity: alice\n",
                "  model: claude-opus-5\n",
                "memory:\n",
                "  backend: hindsight\n",
                "  endpoint: https://hindsight.example.ts.net/mcp/\n",
                "roster:\n",
                "  - name: alice\n",
                "    profile: swe\n",
                "    labels: [rust, config, parity]\n",
                "    bank: \"\"\n",
                "    max_concurrent: 0\n",
                "  - name: bob\n",
                "    profile: swe\n",
                "    labels: [web, ui]\n",
                "  - name: jimmy\n",
                "    profile: reviewer\n",
                "    labels: [review]\n",
            ),
        )
        .expect("write");

        let t = Teams::try_load(&path).expect("well-formed file loads");
        assert!(t.enabled);
        assert_eq!(t.manager.mode, ManagerMode::LabelsModel);
        assert_eq!(t.manager.default_identity, "alice");
        assert_eq!(t.manager.model, "claude-opus-5");
        // Unset manager keys still take the §2.2 defaults.
        assert_eq!(t.manager.max_tokens, 4000);
        assert_eq!(t.manager.timeout_ms, 60000);
        assert_eq!(t.memory.backend, MemoryBackend::Hindsight);
        assert_eq!(t.memory.endpoint, "https://hindsight.example.ts.net/mcp/");
        // Unset memory keys likewise.
        assert_eq!(t.memory.bank_prefix, "agent-");
        assert_eq!(t.memory.recall_top_k, 8);
        assert_eq!(t.memory.path, "");

        assert_eq!(t.roster.len(), 3);
        assert_eq!(t.roster[0].name, "alice");
        assert_eq!(t.roster[0].profile, "swe");
        assert_eq!(t.roster[0].labels, vec!["rust", "config", "parity"]);
        // Per-entry defaults for the entries that omit them.
        assert_eq!(t.roster[1].name, "bob");
        assert_eq!(t.roster[1].bank, "");
        assert_eq!(t.roster[1].max_concurrent, 0);
        assert_eq!(t.roster[2].name, "jimmy");
        assert_eq!(t.roster[2].labels, vec!["review"]);
        // Loading is pure: parsing a file never rewrites it (§2.1, never seed).
        let before = std::fs::read_to_string(&path).expect("read");
        let _ = Teams::load(&path);
        assert_eq!(std::fs::read_to_string(&path).expect("read"), before);
    }

    /// `mode` and `backend` are closed sets: an unrecognized value is a parse
    /// error (⇒ disabled + loud), not a silent fallback to the default. A typo
    /// that quietly disabled routing would be worse than a rejected file.
    #[test]
    fn manager_mode_and_memory_backend_are_closed_sets() {
        for (text, want) in [
            ("manager:\n  mode: off\n", ManagerMode::Off),
            ("manager:\n  mode: labels\n", ManagerMode::Labels),
            ("manager:\n  mode: labels+model\n", ManagerMode::LabelsModel),
        ] {
            assert_eq!(Teams::parse(text).expect("valid mode").manager.mode, want);
        }
        for (text, want) in [
            ("memory:\n  backend: none\n", MemoryBackend::None),
            ("memory:\n  backend: local\n", MemoryBackend::Local),
            ("memory:\n  backend: hindsight\n", MemoryBackend::Hindsight),
        ] {
            assert_eq!(
                Teams::parse(text).expect("valid backend").memory.backend,
                want
            );
        }
        for bad in [
            "manager:\n  mode: labels+models\n",
            "manager:\n  mode: Labels\n",
            "memory:\n  backend: sqlite\n",
        ] {
            assert!(
                matches!(Teams::parse(bad), Err(TeamsError::Parse(_))),
                "{bad:?} should be a parse error"
            );
        }
    }

    /// A malformed file disables Teams — it never propagates a failure that
    /// could take the daemon down (§2.1). `try_load` still reports WHY so the
    /// boot can log one loud line.
    #[test]
    fn malformed_file_is_disabled_not_a_failure() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("teams.yaml");
        // A roster that is a scalar where a sequence belongs, plus unbalanced
        // indentation — YAML that cannot become a `Teams`.
        std::fs::write(&path, "enabled: true\nroster: \"not a list\"\n").expect("write");

        assert_eq!(Teams::load(&path), Teams::disabled());
        assert!(
            !Teams::load(&path).enabled,
            "a broken file must not enable Teams"
        );
        let err = Teams::try_load(&path).expect_err("malformed file reports why");
        assert!(
            matches!(err, TeamsError::Parse(_)),
            "want a parse error, got {err}"
        );
        // And the daemon's fallback for that error is the off state.
        assert_eq!(
            Teams::try_load(&path).unwrap_or_else(|_| Teams::disabled()),
            Teams::disabled()
        );
    }

    /// Two entries claiming the same name would make `rhapsody:@alice`
    /// ambiguous — which identity a label names must be a function.
    #[test]
    fn duplicate_roster_names_are_rejected() {
        let text = "roster:\n  - name: alice\n  - name: bob\n  - name: alice\n";
        let err = Teams::parse(text)
            .expect("parses")
            .validate()
            .expect_err("duplicate name rejected");
        assert!(
            matches!(err, TeamsError::Invalid(_)),
            "want an invalid error, got {err}"
        );
        assert!(
            err.to_string().contains("alice"),
            "error names the offender: {err}"
        );
    }

    /// `operator` and `manager` are the daemon's OWN voices in the room
    /// (STUDIO-661), so a roster may not claim either. Both spellings are
    /// label-safe, which is exactly the problem: without this rule a teammate
    /// named `operator` would render as the human in every teammate's catch-up
    /// line, and there is no way to tell the two apart after the fact.
    #[test]
    fn reserved_speaker_names_are_rejected() {
        for name in crate::room::RESERVED_IDENTITIES {
            let text = format!("roster:\n  - name: {name}\n");
            let err = Teams::parse(&text)
                .unwrap_or_else(|e| panic!("parse {name:?}: {e}"))
                .validate()
                .unwrap_err();
            assert!(
                matches!(err, TeamsError::Invalid(_)),
                "{name:?}: want an invalid error, got {err}"
            );
            let msg = err.to_string();
            assert!(msg.contains(name), "the message names the offender: {msg}");
            assert!(
                msg.contains("reserved"),
                "the message names the reservation: {msg}"
            );
        }
    }

    /// The reservation is exact, not a prefix or a substring match: a real
    /// teammate called `operators` or `manager-bot` is an ordinary name and
    /// stays legal. Widening this rule would break rosters for no reason —
    /// only the two names the daemon itself stamps are unavailable.
    #[test]
    fn names_that_merely_resemble_a_reserved_one_are_still_legal() {
        for name in ["operators", "manager-bot", "op", "co-operator", "manag"] {
            let text = format!("roster:\n  - name: {name}\n");
            Teams::parse(&text)
                .unwrap_or_else(|e| panic!("parse {name:?}: {e}"))
                .validate()
                .unwrap_or_else(|e| panic!("roster name {name:?} must stay legal: {e}"));
        }
    }

    /// The charset a name is pinned to, because it is interpolated into a
    /// `rhapsody:@<name>` Linear label (§0.11.1) and a `<prefix><name>` bank
    /// id (§2.2). Widening this later breaks both namespaces, so it is pinned
    /// in T1 — before anything reads the roster.
    #[test]
    fn roster_name_charset_is_label_safe() {
        let ok = ["a", "alice", "bob2", "a-b", "x9-y-2", "alice-the-second"];
        for name in ok {
            assert!(is_label_safe(name), "{name:?} should be label-safe");
        }
        let bad = [
            "",              // empty
            "Alice",         // uppercase
            "alice Smith",   // space
            "-alice",        // leading dash
            "9alice",        // leading digit
            "alice_smith",   // underscore
            "alice.smith",   // dot
            "alice@team",    // the label separator itself
            "alice:1",       // the label namespace separator
            "álice",         // non-ASCII
            "rhapsody:@bob", // a whole label as a name
        ];
        for name in bad {
            assert!(!is_label_safe(name), "{name:?} must NOT be label-safe");
        }
        // And validation enforces it on the roster, not just in the helper.
        for name in bad {
            let text = format!("roster:\n  - name: {name:?}\n");
            let parsed = Teams::parse(&text).unwrap_or_else(|e| panic!("parse {name:?}: {e}"));
            let err = parsed
                .validate()
                .expect_err(&format!("roster name {name:?} must be rejected"));
            assert!(matches!(err, TeamsError::Invalid(_)), "{name:?}: got {err}");
        }
        // ...and every good one is accepted by validation too.
        for name in ok {
            let text = format!("roster:\n  - name: {name}\n");
            Teams::parse(&text)
                .unwrap_or_else(|e| panic!("parse {name:?}: {e}"))
                .validate()
                .unwrap_or_else(|e| panic!("roster name {name:?} must be accepted: {e}"));
        }
    }

    /// A roster entry with no `name` at all is rejected by the same rule —
    /// `#[serde(default)]` makes it an empty string, which is not label-safe.
    #[test]
    fn roster_entry_without_a_name_is_rejected() {
        let err = Teams::parse("roster:\n  - profile: swe\n")
            .expect("parses")
            .validate()
            .expect_err("a nameless entry is rejected");
        assert!(matches!(err, TeamsError::Invalid(_)), "got {err}");
    }

    /// `default_identity` must name someone who exists: a dangling default
    /// would silently route nothing (T3a) instead of failing loudly here.
    #[test]
    fn default_identity_must_name_a_roster_entry() {
        let good = "manager:\n  default_identity: alice\nroster:\n  - name: alice\n";
        Teams::parse(good)
            .expect("parses")
            .validate()
            .expect("a default_identity that exists is valid");

        let dangling = "manager:\n  default_identity: carol\nroster:\n  - name: alice\n";
        let err = Teams::parse(dangling)
            .expect("parses")
            .validate()
            .expect_err("a dangling default_identity is rejected");
        assert!(matches!(err, TeamsError::Invalid(_)), "got {err}");
        assert!(err.to_string().contains("carol"), "error names it: {err}");

        // Empty (the §2.2 default) means "run without an identity" and is fine
        // even with an empty roster.
        Teams::parse("roster: []\n")
            .expect("parses")
            .validate()
            .expect("an empty default_identity is valid");
    }

    /// A validation failure reaches the daemon exactly like a parse failure
    /// does: `Teams::disabled()` from `load`, an `Err` from `try_load`.
    #[test]
    fn invalid_file_is_disabled_not_a_failure() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("teams.yaml");
        std::fs::write(&path, "enabled: true\nroster:\n  - name: Alice\n").expect("write");

        assert_eq!(Teams::load(&path), Teams::disabled());
        assert!(matches!(
            Teams::try_load(&path),
            Err(TeamsError::Invalid(_))
        ));
    }

    /// Validation runs even with the toggle off, so a user editing the file
    /// sees the complaint before they flip `enabled`, not after.
    #[test]
    fn validation_runs_while_disabled() {
        let err = Teams::parse("enabled: false\nroster:\n  - name: alice\n  - name: alice\n")
            .expect("parses")
            .validate()
            .expect_err("validated even while off");
        assert!(matches!(err, TeamsError::Invalid(_)), "got {err}");
    }

    /// STUDIO-659 (T7), §0.12's "cost control": the quorum is opt-in per
    /// installation. An absent `quorum:` section and a present-but-empty one
    /// both mean OFF — the same "absence is the shipped state" rule the whole
    /// file is built on. Teams itself being ON must not turn it on.
    #[test]
    fn quorum_is_absent_means_disabled() {
        for text in [
            "enabled: true\nroster:\n  - name: alice\n",
            "enabled: true\nquorum:\nroster:\n  - name: alice\n",
            "enabled: true\nquorum: {}\nroster:\n  - name: alice\n",
        ] {
            let t = Teams::parse(text).unwrap_or_else(|e| panic!("parse {text:?}: {e}"));
            assert!(t.enabled, "({text:?})");
            assert!(
                !t.quorum.enabled,
                "an absent/empty quorum section must be OFF ({text:?})"
            );
            assert_eq!(t.quorum, Quorum::default(), "({text:?})");
            assert_eq!(t.quorum.reviewers, DEFAULT_QUORUM_REVIEWERS, "({text:?})");
        }
    }

    /// §0.12's cap: `reviewers` defaults to 2 ("at least two" is the floor AND
    /// the default) and is honoured when set.
    #[test]
    fn quorum_reviewers_defaults_to_two_and_is_settable() {
        let t = Teams::parse("quorum:\n  enabled: true\n").expect("parses");
        assert!(t.quorum.enabled);
        assert_eq!(t.quorum.reviewers, 2);
        assert_eq!(t.quorum.effective_reviewers(), 2);

        let t = Teams::parse("quorum:\n  enabled: true\n  reviewers: 3\n").expect("parses");
        assert_eq!(t.quorum.effective_reviewers(), 3);
    }

    /// The floor: a `reviewers` of 0 or below is a config mistake, and clamping
    /// UP to one is the only reading that keeps `enabled: true` meaningful —
    /// clamping down to zero would make an enabled quorum a silent no-op, which
    /// is what `enabled: false` is already for.
    #[test]
    fn quorum_reviewers_clamps_up_to_one() {
        for (yaml, want) in [
            ("quorum:\n  enabled: true\n  reviewers: 0\n", 1usize),
            ("quorum:\n  enabled: true\n  reviewers: -7\n", 1),
        ] {
            let t = Teams::parse(yaml).unwrap_or_else(|e| panic!("parse {yaml:?}: {e}"));
            assert_eq!(t.quorum.effective_reviewers(), want, "({yaml:?})");
        }
    }

    /// Unknown keys are ignored rather than fatal, matching `CapabilityDef`'s
    /// tolerance for partial entries: a `teams.yaml` written by a NEWER
    /// Rhapsody must not disable the feature on an older one.
    #[test]
    fn unknown_keys_are_tolerated() {
        let t = Teams::parse(
            "enabled: true\nfuture_key: 1\nroster:\n  - name: alice\n    unknown: x\n",
        )
        .expect("unknown keys do not fail the parse");
        assert!(t.enabled);
        assert_eq!(t.roster[0].name, "alice");
    }

    /// The error prefixes are part of the log line an operator greps for, so
    /// they are pinned — the same treatment `CapabilitiesError` gets.
    #[test]
    fn teams_error_prefixes_are_stable() {
        assert!(
            TeamsError::Io("boom".into())
                .to_string()
                .starts_with("teams_io_error:")
        );
        assert!(
            TeamsError::Parse("boom".into())
                .to_string()
                .starts_with("teams_parse_error:")
        );
        assert!(
            TeamsError::Invalid("boom".into())
                .to_string()
                .starts_with("teams_invalid:")
        );
    }

    /// Serialization round-trips, so the future `rhapsody teams init` / Settings
    /// writer (the ONLY things allowed to create the file) can emit a document
    /// this loader reads back identically.
    #[test]
    fn round_trips_through_yaml() {
        let t = Teams {
            enabled: true,
            manager: Manager {
                mode: ManagerMode::Off,
                default_identity: "alice".to_string(),
                model: "m".to_string(),
                max_tokens: 1,
                timeout_ms: 2,
            },
            memory: Memory {
                backend: MemoryBackend::None,
                path: "/tmp/banks".to_string(),
                endpoint: String::new(),
                // STUDIO-660, T8: the one new key, round-tripped like the rest.
                api_key: "$HINDSIGHT_API_KEY".to_string(),
                bank_prefix: "team-".to_string(),
                recall_top_k: 3,
            },
            quorum: Quorum {
                enabled: true,
                reviewers: 3,
            },
            roster: vec![Identity {
                name: "alice".to_string(),
                profile: "swe".to_string(),
                labels: vec!["rust".to_string()],
                bank: "b".to_string(),
                max_concurrent: 2,
            }],
            // STUDIO-650, T5: the one new key, round-tripped like the rest.
            prompt_budget_bytes: 9000,
        };
        let yaml = serde_yaml_ng::to_string(&t).expect("serialize");
        assert_eq!(Teams::parse(&yaml).expect("reparse"), t);
        t.validate().expect("the round-tripped value is valid");

        // `labels+model` is the one enum value whose YAML scalar form is not
        // obvious (the `+`), and `hindsight` the one backend a round-trip has
        // not otherwise touched — so round-trip those too, and assert the wire
        // spelling is the schema's, not serde's derived variant name.
        let other = Teams {
            manager: Manager {
                mode: ManagerMode::LabelsModel,
                ..Manager::default()
            },
            memory: Memory {
                backend: MemoryBackend::Hindsight,
                ..Memory::default()
            },
            ..Teams::disabled()
        };
        let yaml = serde_yaml_ng::to_string(&other).expect("serialize");
        assert!(yaml.contains("labels+model"), "wire spelling: {yaml}");
        assert!(yaml.contains("hindsight"), "wire spelling: {yaml}");
        assert_eq!(Teams::parse(&yaml).expect("reparse"), other);
    }
}
