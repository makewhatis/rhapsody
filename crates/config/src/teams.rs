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

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
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
const DEFAULT_TIMEOUT_MS: i64 = 5000;
/// `memory.bank_prefix` — a bank id is `<bank_prefix><name>`.
const DEFAULT_BANK_PREFIX: &str = "agent-";
/// `memory.recall_top_k` — how many facts a recall returns.
const DEFAULT_RECALL_TOP_K: i64 = 8;

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
    /// `hindsight` backend: the MCP base URL.
    #[serde(default)]
    pub endpoint: String,
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
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Teams {
    /// The one toggle the whole feature lives behind (§2). Default false.
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub manager: Manager,
    #[serde(default)]
    pub memory: Memory,
    #[serde(default)]
    pub roster: Vec<Identity>,
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
fn is_label_safe(name: &str) -> bool {
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
    /// A present-but-empty file (`""`, `{}`, `null`) is the fully-defaulted —
    /// i.e. disabled — config rather than a parse error.
    fn parse(text: &str) -> Result<Self, TeamsError> {
        if text.trim().is_empty() {
            return Ok(Self::disabled());
        }
        serde_yaml_ng::from_str(text).map_err(|e| TeamsError::Parse(e.to_string()))
    }

    /// Syntactic validation only — profiles are T2, so `profile` is not
    /// resolved here and an unknown one is not an error yet.
    ///
    /// Checked: every roster `name` is label-safe (§0.11.1); no two entries
    /// share a name; `manager.default_identity`, when set, names a roster
    /// entry. Runs regardless of `enabled` so a user editing the file sees the
    /// complaint before they flip the toggle, not after.
    fn validate(&self) -> Result<(), TeamsError> {
        let mut seen: HashSet<&str> = HashSet::with_capacity(self.roster.len());
        for entry in &self.roster {
            if !is_label_safe(&entry.name) {
                return Err(TeamsError::Invalid(format!(
                    "roster name {:?} is not label-safe (must match ^[a-z][a-z0-9-]*$; it becomes a `rhapsody:@<name>` label)",
                    entry.name
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
            assert_eq!(t.manager.timeout_ms, 5000, "({text:?})");
            assert_eq!(t.memory.backend, MemoryBackend::Local, "({text:?})");
            assert_eq!(t.memory.path, "", "({text:?})");
            assert_eq!(t.memory.endpoint, "", "({text:?})");
            assert_eq!(t.memory.bank_prefix, "agent-", "({text:?})");
            assert_eq!(t.memory.recall_top_k, 8, "({text:?})");
            assert!(t.roster.is_empty(), "({text:?})");
            assert_eq!(t, Teams::disabled(), "({text:?})");
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
        assert_eq!(t.manager.timeout_ms, 5000);
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
                bank_prefix: "team-".to_string(),
                recall_top_k: 3,
            },
            roster: vec![Identity {
                name: "alice".to_string(),
                profile: "swe".to_string(),
                labels: vec!["rust".to_string()],
                bank: "b".to_string(),
                max_concurrent: 2,
            }],
        };
        let yaml = serde_yaml_ng::to_string(&t).expect("serialize");
        assert_eq!(Teams::parse(&yaml).expect("reparse"), t);
        t.validate().expect("the round-tripped value is valid");
    }
}
