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
use std::collections::{BTreeMap, HashSet};
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
    /// roster-labels ∩ ticket-labels (§0.11.2 Tier 0 + fallback).
    #[serde(rename = "labels")]
    Labels,
    /// Deterministic, plus an off-loop model turn (§0.11.2) — for tickets no
    /// label matched, and for reading intent out of an operator's room post
    /// (§0.13).
    ///
    /// **The default, changed from `labels` by STUDIO-678.** §2.2 shipped
    /// `labels` as the cheap, model-free default, and §0.13 makes that choice
    /// cost something it did not used to: without a model turn the manager can
    /// still file a review, confirm an assignment and ask a question, but it
    /// cannot read INTENT out of prose — so David's ruling ("if I post something
    /// in there, it should be actionable") is only partly met on a fresh
    /// install. §0.13 asks for the default to move, in as many words, and this
    /// is where "enabling Teams" is actually decided: a hand-written
    /// `teams.yaml` with no `manager:` block lands here, and so does the
    /// dashboard's enable flow, which renders whatever this answers.
    ///
    /// It is not a cost surprise. The turn already had a budget it can finish
    /// inside (STUDIO-673's 60s), it already runs off the control task and can
    /// never stall dispatch (§0.11.2), and every failure of it already degrades
    /// to the deterministic answer. An installation that wants the old
    /// behaviour writes `mode: labels`, which is now a choice rather than a
    /// silence — and Teams remains off entirely unless `enabled: true`, so
    /// nothing changes for a daemon that never opted in at all.
    #[default]
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

/// `review.reviewers` — how many teammates review one pull request on the
/// ticketless path (STUDIO-721; design decision C). One, not the quorum's two:
/// each reviewer is a whole agent run against the pull request's head.
pub const DEFAULT_REVIEW_REVIEWERS: i64 = 1;

fn default_review_reviewers() -> i64 {
    DEFAULT_REVIEW_REVIEWERS
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

/// What kind of review a handoff triggers — the `review.mode` key
/// (STUDIO-719, design `~/.rhapsody/docs/STUDIO-703-ticketless-pr-review.md`
/// §15-d, §16).
///
/// One enum rather than a second boolean beside [`Quorum::enabled`], because
/// the two paths review the same handoff and must never both fire: §14.2's
/// "config cutover double-fire or silent flip" is exactly the bug where a
/// handoff fans review TICKETS out *and* dispatches a ticketless review — two
/// agent runs per head, twice the spend. An enum makes "both" unrepresentable
/// at the one place an operator writes it down.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ReviewMode {
    /// No review path of this subsystem's own — the DEFAULT, and what every
    /// installation that predates the key gets. `quorum.enabled` still decides
    /// the ticket fan-out exactly as it did before, so an upgrade changes
    /// nothing (§15-d: `quorum.enabled` is not repurposed, so nothing flips
    /// silently).
    #[default]
    #[serde(rename = "off")]
    Off,
    /// Review by fanning Linear review TICKETS out to teammates — today's
    /// quorum, spelled explicitly. Behaviourally identical to [`Off`](Self::Off)
    /// here: it is `quorum.enabled` that turns the fan-out on, and this only
    /// records which of the two paths the installation has chosen.
    #[serde(rename = "tickets")]
    Tickets,
    /// Review a PR directly, with no Linear ticket — the STUDIO-703 subsystem.
    /// Guards the ticket fan-out OFF (see [`Teams::review_ticketless`]) so one
    /// handoff fires exactly one review path.
    #[serde(rename = "ticketless")]
    Ticketless,
}

/// A review value scoped by the HARNESS it is for: `review.model` and `review.effort`
/// (STUDIO-908).
///
/// A model name has no meaning on its own — `claude-opus-5` is a Claude model, and
/// `fireworks-ai/accounts/fireworks/models/deepseek-v4p1-flash` is not — so a single bare string
/// cannot say what a review run should use on a roster whose teammates run different harnesses
/// (STUDIO-902 made `harness` a per-teammate fact). Scoping the key by harness is the shape that
/// states "premium review on every harness" rather than "premium review on Claude, broken
/// elsewhere", which is the seam STUDIO-901 and STUDIO-902 landed on top of each other.
///
/// Two YAML spellings parse — the legacy scalar:
///
/// ```yaml
/// review:
///   model: claude-opus-5
/// ```
///
/// and the per-harness map:
///
/// ```yaml
/// review:
///   model:
///     claude: claude-opus-5
///     opencode: fireworks-ai/accounts/fireworks/models/deepseek-v4p1-flash
/// ```
///
/// The bare scalar is the spelling STUDIO-901 shipped, written when every teammate was Claude and
/// a model name was unambiguous; it is stored UNRESOLVED because which harness it belongs to is
/// not knowable at parse time (alice's blocking finding on PR #172). Its harness is the
/// installation's configured `agent.backend` — the harness a bare name was unambiguous for on a
/// single-harness install — so an all-opencode installation that wrote a bare `review.model`
/// keeps working, while a reviewer on a *different* harness is still refused rather than handed
/// the wrong model. [`HarnessScoped::for_harness`] takes that fallback.
///
/// An empty scalar and a null both mean "nothing configured" — the same absent-means-inherit rule
/// every other field in this file follows. Empty values are dropped, so `{ claude: "" }` is unset
/// rather than an override to the empty string.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HarnessScoped {
    /// The legacy bare-scalar spelling, unresolved until [`HarnessScoped::for_harness`] is handed
    /// the fallback harness. `values` is empty whenever this is `Some`; the two spellings never
    /// mix, because each is produced by its own [`Deserialize`] branch.
    bare: Option<String>,
    values: BTreeMap<String, String>,
}

impl HarnessScoped {
    /// The value that applies to `harness`, resolving the legacy bare spelling against `fallback`
    /// (the installation's `agent.backend`), or `None` — including when values exist for OTHER
    /// harnesses. A caller that must tell "unset everywhere" from "set, but not for this harness"
    /// asks [`HarnessScoped::is_empty`] first; [`Teams::review_model_for`] is that caller.
    pub fn for_harness(&self, harness: &str, fallback: &str) -> Option<&str> {
        match &self.bare {
            Some(value) => (harness == fallback).then_some(value.as_str()),
            None => self.values.get(harness).map(String::as_str),
        }
    }

    /// Whether any value is configured at all. An absent scalar, an empty scalar and an empty map
    /// all read as unset.
    pub fn is_empty(&self) -> bool {
        self.bare.is_none() && self.values.is_empty()
    }

    /// Every configured `(harness, value)` pair, in harness order, with the legacy bare spelling
    /// resolved to `fallback` — `teams show` and the refusal message both render the set back to
    /// the operator, so nothing here is silently dropped.
    pub fn resolved<'a>(&'a self, fallback: &'a str) -> Vec<(&'a str, &'a str)> {
        match &self.bare {
            Some(value) => vec![(fallback, value.as_str())],
            None => self
                .values
                .iter()
                .map(|(h, v)| (h.as_str(), v.as_str()))
                .collect(),
        }
    }

    /// The value the legacy bare scalar spelled, or `None` for the per-harness map spelling. Only
    /// `teams show` needs it: the origin label differs between the two spellings (`review.model`
    /// vs `review.model.<harness>`), and naming a harness the operator never wrote is the sort of
    /// misattribution this ticket exists to remove.
    pub fn legacy(&self) -> Option<&str> {
        self.bare.as_deref()
    }

    /// The legacy bare-scalar spelling: `value` is resolved against the installation's
    /// `agent.backend` at dispatch, the harness a bare model name was unambiguous for when
    /// STUDIO-901 shipped. Empty is unset.
    pub fn bare(value: &str) -> Self {
        Self {
            bare: (!value.is_empty()).then(|| value.to_string()),
            values: BTreeMap::new(),
        }
    }

    /// Sets `harness`'s value, or removes it when `value` is empty — so a builder can never leave
    /// an entry that [`HarnessScoped::is_empty`]/[`HarnessScoped::for_harness`] would treat as
    /// unset. This is the per-harness map spelling; [`HarnessScoped::bare`] is the other, and a
    /// value built with one is never mixed with the other.
    pub fn insert(&mut self, harness: &str, value: &str) -> &mut Self {
        if value.is_empty() {
            self.values.remove(harness);
        } else {
            self.values.insert(harness.to_string(), value.to_string());
        }
        self
    }
}

impl Serialize for HarnessScoped {
    /// Serialized in the spelling it was parsed from, so a legacy bare scalar round-trips to the
    /// same YAML `Teams::save` read. Writing it as a `{claude: …}` map would silently RE-SCOPE it:
    /// the bare value belongs to `agent.backend`, which may not be `claude` (STUDIO-908).
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match &self.bare {
            Some(value) => serializer.serialize_str(value),
            None => self.values.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for HarnessScoped {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Bare(String),
            PerHarness(BTreeMap<String, String>),
        }
        // `Option<_>` so a YAML null (`model:` with nothing under it, which is what commenting out
        // the sub-keys leaves behind) is the unset value rather than a type error — the same
        // tolerance every `#[serde(default)]` field in this file already has.
        let raw = Option::<Raw>::deserialize(deserializer)?;
        Ok(match raw {
            None => Self::default(),
            Some(Raw::Bare(value)) => Self::bare(&value),
            Some(Raw::PerHarness(values)) => Self {
                bare: None,
                values: values.into_iter().filter(|(_, v)| !v.is_empty()).collect(),
            },
        })
    }
}

/// What a review run should do about [`Review::model`] for the harness it will actually run on
/// (STUDIO-908) — the three-way answer [`Teams::review_model_for`] returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewModelChoice<'a> {
    /// Nothing is configured for any harness: a review run inherits exactly what it would have
    /// used anyway (the reviewer's profile, else the installation-wide model). STUDIO-901's
    /// byte-identical-without-the-key property.
    Inherit,
    /// Use this value: the operator configured it for the routed reviewer's own harness.
    Use(&'a str),
    /// The operator configured a review model, but not for this reviewer's harness. The run must
    /// be REFUSED with this message rather than sent to a provider that will reject it — and
    /// rather than silently downgrading the review to the reviewer's own (cheap) profile model.
    Refuse(String),
}

/// The `review:` block (STUDIO-719) — nested under `teams`, sibling to
/// [`quorum`](Teams::quorum) and never a top-level key, which is what makes
/// §16's "the whole subsystem is dormant unless `teams.enabled`" structural
/// rather than remembered: there is no way to spell an active review mode
/// outside Teams.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Review {
    #[serde(default)]
    pub mode: ReviewMode,
    /// How many teammates review ONE pull request, clamped to the roster minus
    /// the author (STUDIO-721; design decision C, §13.5/§15-f). Default **1** —
    /// deliberately not [`Quorum::reviewers`]'s 2: a ticketless review is a full
    /// agent run against the pull request's head, so the second reviewer is a
    /// second bill rather than a second comment on a ticket.
    ///
    /// A distinct key from `quorum.reviewers` for [`ReviewMode`]'s reason: the
    /// two paths are mutually exclusive, and reusing the quorum's count would
    /// silently give a ticketless installation the quorum's default.
    #[serde(default = "default_review_reviewers")]
    pub reviewers: i64,
    /// The terminal state NAME a watched pull request's implementation ticket is
    /// moved to when that pull request MERGES (STUDIO-712). Empty — the default
    /// — means the transition is off.
    ///
    /// A NAME rather than a boolean over `tracker.terminal_states[0]` for two
    /// reasons. Linear workflow-state names vary per workspace, so the daemon
    /// cannot know an installation's spelling of "Done"; and `terminal_states`
    /// is an unordered *set* of endings whose conventional second member is
    /// `Canceled` — reading position 0 out of it would let a reordered config
    /// silently CANCEL finished work. Naming the target makes the destructive
    /// spelling unreachable by accident, and makes configuring it the opt-in.
    ///
    /// Empty-means-off mirrors `review_states`: the handoff feature is off
    /// precisely when nobody named a state for it, so there is no second key to
    /// keep in agreement with this one.
    #[serde(default)]
    pub done_state: String,
    /// The state NAME a watched pull request's implementation ticket is moved
    /// BACK to when a review round files findings on it (STUDIO-839). Empty —
    /// the default — means the transition is off.
    ///
    /// The sibling of [`Review::done_state`], and shaped like it for the same
    /// reasons: a NAME because workspace state spellings vary, and
    /// empty-means-off because an installation whose workflow has no such
    /// state must not have one invented for it. The two are the only writers of
    /// ticket state off the back of a review outcome, and they act on opposite
    /// edges — findings (the round's exit) route the ticket back to work, a
    /// MERGE finishes it — so neither can stand in for the other.
    ///
    /// Without it the review state means three things at once: waiting for a
    /// reviewer, being reviewed, and reviewed-with-findings while the author
    /// implements. Naming a state here separates the third from the first two.
    #[serde(default)]
    pub changes_state: String,
    /// Whether the daemon MERGES a watched pull request once every reviewer has
    /// recorded a non-blocking verdict at its current head and CI is green
    /// (STUDIO-874). `false` — the default — means only a human merges.
    ///
    /// Opt-in, and a boolean rather than an inferred always-on for Teams
    /// installs, because merging is the one action in this subsystem that
    /// cannot be undone by the next tick: a wrong review state re-reviews, a
    /// wrong ticket move is re-moved, a wrong merge is on `main`. An operator
    /// therefore says so once, explicitly.
    ///
    /// It lives in `review:` and not in a `merge:` block of its own because the
    /// thing it consumes is this section's output — the per-(PR, reviewer)
    /// verdict the ticketless path records — so it is dead without
    /// `mode: ticketless` exactly as [`Review::done_state`] is, and
    /// [`Teams::review_auto_merge`] gates it on the same predicate.
    ///
    /// This is the installation-wide DEFAULT (STUDIO-927): a per-project entry
    /// under [`Teams::projects`] may override it, and every caller that knows
    /// which project a pull request belongs to reads
    /// [`Teams::review_auto_merge_for`] rather than this field directly.
    #[serde(default)]
    pub auto_merge: bool,
    /// The model a REVIEW run uses, per HARNESS, regardless of what the routed teammate's own
    /// profile asks for (STUDIO-901, scoped by harness in STUDIO-908). Unset — the default — means
    /// a review run inherits whatever model it would have used anyway (the routed teammate's
    /// profile, else the installation-wide `claude.model`), exactly the "absent means whatever
    /// would have happened" rule [`Review::done_state`] and STUDIO-868's profile `model` both
    /// already follow — an installation that never sets this key is byte-identical to one built
    /// before it existed.
    ///
    /// Review-scoped rather than a teammate field on purpose: STUDIO-868's profile `model` answers
    /// "what model does THIS PERSON use", and cannot express "what model does REVIEW use" — every
    /// teammate both implements and reviews, so a role-based intent has no person to attach to.
    /// When a routed reviewer's own profile ALSO names a model, this one wins for a review run: the
    /// operator who wrote `review: { model: … }` is stating role-based intent explicitly, and it is
    /// the PR under review being priced, not that teammate's own work.
    ///
    /// Read through [`Teams::review_model_for`], never raw, exactly as [`Review::done_state`] is
    /// read through [`Teams::review_done_state`] and for the same reason: `dispatch_review` refuses
    /// a review whose reviewer's harness has no entry here, and `dispatch_issue` applies the entry
    /// only to a run `dispatch_review` staged — and only `mode: ticketless` ever stages one.
    #[serde(default)]
    pub model: HarnessScoped,
    /// The effort a REVIEW run uses, paired with [`Review::model`] for the same reason `Config` and
    /// a teammate's profile pair the two everywhere else in this codebase: setting `model` alone
    /// would leave whatever effort was already in play — profile or installation-wide — applying to
    /// a cheap review model, which can cost more than the swap saves. Unset means inherit, exactly
    /// as `model` does, and it is scoped by harness for the same reason.
    ///
    /// Ticketless-only in the same sense as [`Review::model`] — read it through
    /// [`Teams::review_effort`]. Unlike `model`, a missing entry for the reviewer's harness leaves
    /// the effort inherited rather than refusing the run: an effort value cannot make a provider
    /// reject a model, so the "refuse rather than run on the wrong value" rule is `model`'s alone.
    #[serde(default)]
    pub effort: HarnessScoped,
    /// Identities pinned as **required reviewers** (STUDIO-951): selected first on EVERY pull
    /// request, regardless of load or roster order, and never counted as a ranked fill.
    ///
    /// The gap this closes: reviewer selection is roster-minus-author sorted by
    /// `(load, roster_index)` (`quorum::rank_reviewers`), so whether a teammate reviews anything
    /// depends on how busy everyone else happens to be. That is fine while reviewers are
    /// interchangeable, and stops being fine the moment a teammate exists **for** reviewing — a
    /// different model family, a specialist, a second opinion wanted on every change. Such a
    /// teammate's participation was decided by unrelated scheduling, and moving it to the front of
    /// the roster only won the tie until one labelled ticket gave it load.
    ///
    /// A LIST of names on the review block rather than a `review_only: true` flag on a roster
    /// entry (the other credible shape): selection is the whole of what these names do, and a
    /// roster flag would also have to gate implementation dispatch, which is a second behaviour
    /// with its own blast radius and no acceptance criterion here. Pinning is exactly this key.
    ///
    /// Read through [`Teams::review_required`], never raw, so the trim / empty / unknown-name rules
    /// live in one place. An unset list leaves selection byte-identical to before the key existed.
    #[serde(default)]
    pub required: Vec<String>,
    /// How many review↔author ROUNDS a watched pull request may run before the loop stops arming
    /// rounds and hands the pull request to the MANAGER for one adjudication — ship it, or escalate
    /// (STUDIO-956). `0` — the default — leaves the loop exactly as it was: the hard
    /// `REVIEW_ROUNDS_PER_PR_CAP` × reviewers cap and its current stop.
    ///
    /// The maintainer's number is **3**: "we do 3 rounds at work, after three rounds, we escalate."
    /// At the threshold the daemon dispatches no further review or author round and instead runs one
    /// manager turn that adjudicates the OPEN FINDINGS (never the merge gates — see the ticket's
    /// first ⚠️) and records the outcome in the room and on the pull request. `Unset` is
    /// byte-identical to an install built before this key existed.
    ///
    /// **Its own gate, deliberately not `manager.mode`.** `manager.mode: labels` means there is no
    /// manager ASSIGNMENT turn today — assignment is deterministic and spends nothing — so an
    /// adjudication cannot silently inherit that mode. It does not have to: adjudication is a turn
    /// of its own, gated by THIS key, and it runs through the daemon's one model-turn path with
    /// `manager.model` / `manager.timeout_ms`. A `labels`-mode install that sets this key therefore
    /// DOES get adjudication; one that does not set it gets nothing, exactly as before.
    #[serde(default)]
    pub adjudicate_after_rounds: i64,
}

impl Default for Review {
    fn default() -> Self {
        Self {
            mode: ReviewMode::default(),
            reviewers: DEFAULT_REVIEW_REVIEWERS,
            done_state: String::new(),
            changes_state: String::new(),
            auto_merge: false,
            model: HarnessScoped::default(),
            effort: HarnessScoped::default(),
            required: Vec::new(),
            adjudicate_after_rounds: 0,
        }
    }
}

impl Review {
    /// [`Review::reviewers`] with the floor applied — the number an introduction
    /// asks for, before the roster clamps it further.
    ///
    /// Floored at one for [`Quorum::effective_reviewers`]'s reason: zero
    /// reviewers is not a review policy, it is the subsystem switched off, and
    /// `mode: off` is how that is spelled.
    pub fn effective_reviewers(&self) -> usize {
        usize::try_from(self.reviewers.max(MIN_QUORUM_REVIEWERS))
            .unwrap_or(DEFAULT_REVIEW_REVIEWERS as usize)
    }
}

/// One `projects:` entry in `teams.yaml` (STUDIO-927): a per-project overlay of
/// the review knobs, keyed by the Linear project slugs it applies to.
///
/// Shaped like `WORKFLOW.md`'s own `projects:` list (a `slugs:` list plus a
/// nested override block) and resolved the same presence-based way: an unset
/// override inherits the top-level `review.auto_merge`; a set one wins for this
/// project. The slugs are matched against a resolved project's slug exactly as
/// the orchestrator routes a run, so a project that no entry names is
/// byte-identical to one built before this block existed.
///
/// Per-PROJECT rather than per-repo on purpose: the thing the operator writes is
/// the Linear project, and its repo belongs to it — a repo shared by two projects
/// is two overrides to state, not one. When two resolved projects do share a repo,
/// the orchestrator ANDs every owning project's answer: naming any ONE owning slug
/// holds the merge back, while opting in under a global `false` takes naming EVERY
/// owning slug, since an unnamed sibling inherits the global and holds it. An
/// override that resolves `false` must never be lost to a first-match scan.
///
/// Unknown keys are rejected ([`serde`] `deny_unknown_fields`), deliberately unlike
/// the lenient top-level blocks. These are brand-new types with no legacy spellings,
/// and a misspelled key (`auto-merge:`) or a mis-indent (the key beside `slugs`
/// instead of under `review:`) would otherwise parse cleanly, leave `auto_merge`
/// unset, and silently inherit the global — the exact unsafe direction this knob
/// exists to prevent. A rejected `teams.yaml` degrades to Teams-off (no auto-merge),
/// the safe side.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TeamsProject {
    /// The Linear project SLUG IDs this entry applies to — the same `slugs:` values
    /// `WORKFLOW.md`'s `projects:` list carries (Linear `slugId` hex, e.g.
    /// `4f4a2350682f`), never the project NAME. A slug no resolved project carries
    /// leaves its entry inert, and the boot reports every such slug with a warning,
    /// so a name written where an id belongs is visible rather than silently ignored.
    #[serde(default)]
    pub slugs: Vec<String>,
    /// The per-project review overrides.
    #[serde(default)]
    pub review: ProjectReview,
}

/// The per-project half of [`Review`] (STUDIO-927), scoped to the project whose
/// slugs name the enclosing [`TeamsProject`].
///
/// Only the knobs that make sense per project and are read per project live
/// here. `auto_merge` is the first: with one global flag, a repo that must not
/// self-merge had no way to say so inside `teams.yaml` — the only brake was an
/// adjective in a prompt.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectReview {
    /// `Some(false)` keeps THIS project human-merged even when the top-level
    /// `review.auto_merge` is on; `Some(true)` turns it on for a project whose
    /// default would be off. `None` — the default — inherits the top-level
    /// value, which is the asymmetric safety property: a project never named
    /// here behaves exactly as it did before this key existed.
    #[serde(default)]
    pub auto_merge: Option<bool>,
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
    /// The `review:` block (STUDIO-719, slice 7 of the §14.4 plan). An ABSENT
    /// section is [`Review::default`], i.e. [`ReviewMode::Off`] — the state
    /// every existing `teams.yaml` is already in.
    #[serde(default)]
    pub review: Review,
    /// The per-project review overrides (STUDIO-927). An ABSENT block leaves
    /// every project on the top-level `review.auto_merge`, so an existing
    /// `teams.yaml` — including one that spells only the bare top-level key —
    /// parses to exactly the behaviour it had. Read through
    /// [`Teams::review_auto_merge_for`], never raw.
    #[serde(default)]
    pub projects: Vec<TeamsProject>,
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
            review: Review::default(),
            projects: Vec::new(),
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

    /// Whether review runs on the TICKETLESS path: Teams on AND
    /// `review.mode: ticketless` (§16's gate, and the reason the key is nested
    /// under `teams` — `enabled: false` makes it structurally unreachable).
    ///
    /// The ONE spelling of that predicate, so the orchestrator's gate, the
    /// quorum cutover and the daemon's decision to spawn the fan-out task can
    /// never disagree about which review path an installation is on.
    pub fn review_ticketless(&self) -> bool {
        self.enabled && self.review.mode == ReviewMode::Ticketless
    }

    /// The terminal state name a merged pull request's implementation ticket is
    /// moved to, or `None` when that transition is off (STUDIO-712).
    ///
    /// Gated on [`review_ticketless`](Self::review_ticketless) as well as on
    /// the state being named, because the merge edge this rides is the
    /// ticketless watcher's watch set: on any other installation there is no
    /// set of daemon-parked pull requests to observe a merge on, so a
    /// configured state name there would promise a transition that can never
    /// fire. Trimmed, so a whitespace-only value reads as "off" rather than as
    /// a state name Linear will refuse.
    pub fn review_done_state(&self) -> Option<&str> {
        if !self.review_ticketless() {
            return None;
        }
        let name = self.review.done_state.trim();
        (!name.is_empty()).then_some(name)
    }

    /// The state name a ticket is moved BACK to when its review round files
    /// findings, or `None` when that transition is off (STUDIO-839).
    ///
    /// Gated on [`review_ticketless`](Self::review_ticketless) for
    /// [`review_done_state`](Self::review_done_state)'s reason: the findings
    /// edge this rides is the ticketless review's own exit, so on any other
    /// installation a configured name would promise a transition that can never
    /// fire. Trimmed, so a whitespace-only value reads as "off" rather than as
    /// a state name Linear will refuse.
    pub fn review_changes_state(&self) -> Option<&str> {
        if !self.review_ticketless() {
            return None;
        }
        let name = self.review.changes_state.trim();
        (!name.is_empty()).then_some(name)
    }

    /// How many review↔author ROUNDS a watched pull request may run before the manager adjudicates
    /// it, or `None` when adjudication is off (STUDIO-956).
    ///
    /// Gated on [`review_ticketless`](Self::review_ticketless) for
    /// [`review_done_state`](Self::review_done_state)'s reason: the round counters this reads live
    /// only on the ticketless watcher's watch set, so on any other installation the key is dead
    /// config and must read as off rather than promise a decision that can never fire.
    ///
    /// Floored at one: a non-positive value is "off", and the smallest meaningful threshold is one
    /// round. There is deliberately no upper clamp — a threshold above the hard cap simply never
    /// fires, because the legacy cap stops the loop first, which is the same behaviour an install
    /// that never set the key gets.
    pub fn review_adjudicate_after_rounds(&self) -> Option<usize> {
        if !self.review_ticketless() {
            return None;
        }
        let n = self.review.adjudicate_after_rounds;
        (n > 0).then(|| usize::try_from(n).unwrap_or(usize::MAX))
    }

    /// Whether a watched pull request that has cleared every gate may be MERGED
    /// by the daemon (STUDIO-874).
    ///
    /// Gated on [`review_ticketless`](Self::review_ticketless) for
    /// [`review_done_state`](Self::review_done_state)'s reason, with one extra
    /// edge to it: the verdict this consumes — the per-(PR, reviewer)
    /// `approved`/`reviewed` status keyed to a reviewed head — is written ONLY
    /// by the ticketless path. On a `tickets` install the watch set is empty, so
    /// an auto-merge there would not be conservative, it would be a merge with
    /// no reviewer verdict to read at all.
    ///
    /// This is the installation-wide default. A caller that knows which project
    /// a pull request belongs to reads [`review_auto_merge_for`](Self::review_auto_merge_for)
    /// instead; this accessor remains the fallback for a pull request whose
    /// project cannot be resolved.
    pub fn review_auto_merge(&self) -> bool {
        self.review_ticketless() && self.review.auto_merge
    }

    /// Whether the project named `project_slug` may be auto-merged (STUDIO-927):
    /// the per-project overlay of [`review_auto_merge`](Self::review_auto_merge).
    ///
    /// The first `projects:` entry whose `slugs` contains `project_slug` wins, and
    /// its `review.auto_merge` — when set — overrides the top-level value. A slug
    /// no entry names answers exactly as [`review_auto_merge`](Self::review_auto_merge)
    /// does, so a project that has never been configured behaves exactly as it
    /// did before this key existed. An unset per-project override inherits in
    /// BOTH directions: a project with no entry under a global `true` still
    /// merges, and one named with no `auto_merge` under a global `false` still
    /// does not.
    ///
    /// Gated on [`review_ticketless`](Self::review_ticketless) for
    /// [`review_auto_merge`](Self::review_auto_merge)'s reason: a per-project
    /// `true` on a Teams-off or `mode: tickets` install is dead config and reads
    /// as `false`, never as an override that cannot fire.
    pub fn review_auto_merge_for(&self, project_slug: &str) -> bool {
        if !self.review_ticketless() {
            return false;
        }
        self.projects
            .iter()
            .find(|p| p.slugs.iter().any(|s| s.trim() == project_slug.trim()))
            .and_then(|p| p.review.auto_merge)
            .unwrap_or(self.review.auto_merge)
    }

    /// The `slugs:` values in [`Teams::projects`] that match no resolved project
    /// slug in `known` (STUDIO-927), in declaration order.
    ///
    /// An unmatched slug is inert by construction — no resolved project can route
    /// to it, so its `review` override can never fire and the project silently keeps
    /// the top-level `auto_merge`. For a safety brake that is the wrong failure mode:
    /// an operator who writes the project NAME (`slugs: [booch]`) where the Linear
    /// `slugId` belongs (`4f4a2350682f`) gets a valid `teams.yaml` that does nothing.
    /// This exists so the boot can name every such slug and turn a typo into
    /// something the operator can see, rather than leaving booch self-merging.
    ///
    /// `known` are the resolved project slugs — `projects::resolve_projects`'s
    /// output, the same set the orchestrator routes against. Empty only when there
    /// are no entries at all.
    ///
    /// Both sides are trimmed: `validate` trims `projects[].slugs[]` in place before
    /// the orchestrator resolves them, but the boot calls this on the unvalidated
    /// config, so a slug padded with whitespace must not read as a typo here when
    /// routing would have matched it.
    pub fn unmatched_project_slugs<'a>(&'a self, known: &[String]) -> Vec<&'a str> {
        let mut out = Vec::new();
        for project in &self.projects {
            for slug in &project.slugs {
                let slug = slug.trim();
                if !known.iter().any(|k| k.trim() == slug) {
                    out.push(slug);
                }
            }
        }
        out
    }

    /// What a REVIEW run dispatched to a reviewer on `harness` should do about `review.model`
    /// (STUDIO-901, scoped by harness in STUDIO-908). `harness` is the harness the run will
    /// ACTUALLY use — the routed reviewer's resolved profile harness, else the configured
    /// `agent.backend` — never the reviewer's raw profile field. `fallback` is that configured
    /// `agent.backend` itself, the harness the legacy bare-scalar spelling belongs to.
    ///
    /// Gated on [`review_ticketless`](Self::review_ticketless) for
    /// [`review_done_state`](Self::review_done_state)'s reason: `dispatch_review`'s and
    /// `dispatch_issue`'s `review.model` block only ever runs for a run `dispatch_review` staged,
    /// and only `mode: ticketless` ever stages one — on any other installation, including the
    /// default `mode: off`, a set value is dead config and reads as [`ReviewModelChoice::Inherit`].
    ///
    /// Three answers, and the third is the one this accessor exists for:
    ///
    /// * nothing configured anywhere → [`ReviewModelChoice::Inherit`];
    /// * configured for this harness → [`ReviewModelChoice::Use`];
    /// * configured, but only for OTHER harnesses → [`ReviewModelChoice::Refuse`], so a review on
    ///   this harness fails loudly at dispatch naming the harness, the configured model and the
    ///   `review.model` origin, rather than being handed to a provider that will reject it or
    ///   silently downgrading to the reviewer's own profile model.
    pub fn review_model_for(&self, harness: &str, fallback: &str) -> ReviewModelChoice<'_> {
        if !self.review_ticketless() || self.review.model.is_empty() {
            return ReviewModelChoice::Inherit;
        }
        if let Some(value) = self.review.model.for_harness(harness, fallback) {
            return ReviewModelChoice::Use(value);
        }
        let listed = self
            .review
            .model
            .resolved(fallback)
            .iter()
            .map(|(h, v)| format!("{h} (model {v})"))
            .collect::<Vec<_>>()
            .join(", ");
        ReviewModelChoice::Refuse(format!(
            "review.model is set for {listed}, but this reviewer runs the {harness} harness, which \
             cannot serve a model scoped to another harness. The review was refused rather than run \
             on the wrong model — add a `review.model.{harness}` entry, or remove `review.model` so \
             every reviewer inherits its own profile's model (origin: review.model)"
        ))
    }

    /// The effort a REVIEW run on `harness` uses; the pair to
    /// [`review_model_for`](Self::review_model_for), gated the same way and for the same reason.
    /// `fallback` is read for [`review_model_for`](Self::review_model_for)'s reason: the legacy
    /// bare effort belongs to the configured `agent.backend`.
    ///
    /// Deliberately one answer where [`review_model_for`](Self::review_model_for) has three: an
    /// effort value cannot be rejected by a provider, so a harness the operator did not write an
    /// entry for simply inherits its profile's effort instead of refusing the run.
    pub fn review_effort(&self, harness: &str, fallback: &str) -> Option<&str> {
        if !self.review_ticketless() {
            return None;
        }
        self.review.effort.for_harness(harness, fallback)
    }

    /// The identities pinned as required reviewers (STUDIO-951), in declaration order.
    ///
    /// **Ungated by design.** Every other `review.*` accessor gates on
    /// [`review_ticketless`](Self::review_ticketless) because the value it reads is written by the
    /// ticketless path alone. Pinning is not: both review paths choose reviewers through the same
    /// `quorum::rank_reviewers`, and the ticket's whole point is that a required reviewer is
    /// selected whichever path an installation runs. So this answers on `mode: tickets` and
    /// `mode: offline` alike.
    ///
    /// Entries are trimmed and blanks dropped, so a `required: [""]` or a whitespace-only entry is
    /// "unset" rather than a name no roster could ever hold. Duplicates are left to the selector to
    /// dedupe (it already scans the list) — a repeated name is harmless there and a parse error
    /// here would disable Teams over a typo, which §2.1 forbids.
    pub fn review_required(&self) -> Vec<&str> {
        self.review
            .required
            .iter()
            .map(|name| name.trim())
            .filter(|name| !name.is_empty())
            .collect()
    }

    /// How many reviewers the review path this installation actually runs asks for, or `None` when
    /// no review path is on (Teams off, or neither `quorum.enabled` nor `mode: ticketless`).
    ///
    /// The two paths count differently and are mutually exclusive (`validate` rejects a config
    /// that sets both), so there is exactly one right answer per installation: `review.reviewers`
    /// on the ticketless path, `quorum.reviewers` on the fan-out path. [`over_pinned_reviewers`]
    /// and the boot warning need that ONE number to compare a pin list against; naming the wrong
    /// path's count would warn about a value nothing reads.
    ///
    /// [`over_pinned_reviewers`]: Self::over_pinned_reviewers
    pub fn active_reviewer_count(&self) -> Option<usize> {
        if !self.enabled {
            return None;
        }
        if self.review_ticketless() {
            Some(self.review.effective_reviewers())
        } else if self.quorum.enabled {
            Some(self.quorum.effective_reviewers())
        } else {
            None
        }
    }

    /// `Some((required, total))` when more identities are pinned than the active review path can
    /// select (STUDIO-951), else `None`. The daemon's boot turns this into ONE warning naming both
    /// numbers.
    ///
    /// Only **on-roster** pins are counted: a name the selector can never name occupies no
    /// reviewer slot and must not make this claim. Counting such a name produced a false warning —
    /// `reviewers: 1` with `required: [ghost, sol]` and only `sol` on the roster warned that a pin
    /// was dropped, while selection in fact kept `sol` and dropped nothing. Off-roster names get
    /// their own diagnostic, [`unknown_required_reviewers`](Self::unknown_required_reviewers), so
    /// the operator still learns about the typo without this count lying.
    ///
    /// Selection **clamps** rather than refusing: pins are ranked first and the caller truncates to
    /// `total`, so a list longer than `total` simply drops the tail. That is the safe direction —
    /// the alternative, disabling Teams over an over-long pin list, loses every reviewer, not the
    /// extra ones. But silently dropping a reviewer the operator explicitly required is the worst
    /// outcome the ticket names, so the drop is reported at boot where it cannot be missed, and the
    /// two numbers are named so the fix (raise `reviewers` or shorten `required`) is obvious.
    pub fn over_pinned_reviewers(&self) -> Option<(usize, usize)> {
        let total = self.active_reviewer_count()?;
        // Counted the way the selector consumes them: trimmed, blanks dropped, duplicates folded,
        // and only names the selector can actually name (roster members).
        let mut seen: HashSet<&str> = HashSet::new();
        let required = self
            .review_required()
            .into_iter()
            .filter(|name| seen.insert(*name))
            .filter(|name| self.roster.iter().any(|i| i.name == *name))
            .count();
        (required > total).then_some((required, total))
    }

    /// The `review.required` names that are **not on the roster** (STUDIO-951), in declaration
    /// order, trimmed, blanks dropped and duplicates folded.
    ///
    /// Separate from [`over_pinned_reviewers`](Self::over_pinned_reviewers) because the selector can
    /// only ever name a roster member: an unknown name never takes a slot and never clamps anything,
    /// so folding it into that count made the boot warning claim a drop that never happened. It is
    /// still worth a boot line of its own — the operator wrote the name believing it would review,
    /// and the live `rank_reviewers` warning only fires once a round is actually built. An empty
    /// list means every pin names somebody, which is the normal state.
    pub fn unknown_required_reviewers(&self) -> Vec<&str> {
        let mut seen: HashSet<&str> = HashSet::new();
        self.review_required()
            .into_iter()
            .filter(|name| seen.insert(*name))
            .filter(|name| !self.roster.iter().any(|i| i.name == *name))
            .collect()
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
        // Mutual exclusion, §15-d: the two review paths read the same handoff,
        // so an installation that asks for BOTH has asked for two agent runs
        // per pushed head and a doubled bill. Rejected here rather than
        // silently resolved by precedence, because either precedence would be a
        // guess about which of two explicitly-written keys the operator meant.
        // Like every other rule in this function it fires regardless of
        // `enabled`, so the complaint arrives while the file is still being
        // edited.
        if self.quorum.enabled && self.review.mode == ReviewMode::Ticketless {
            return Err(TeamsError::Invalid(
                "quorum.enabled: true and review.mode: ticketless are mutually exclusive — they \
                 are two review paths over the same handoff, and running both fans review tickets \
                 out AND dispatches a ticketless review (two agent runs per head). Pick one: keep \
                 `quorum.enabled: true` with `review.mode: tickets`, or set `quorum.enabled: \
                 false` to move to `ticketless`"
                    .to_string(),
            ));
        }
        // STUDIO-891: the CEILING `review.reviewers` never had. `pick_reviewer`
        // names only non-authors, so a roster of N can hold at most N−1 of one
        // pull request's required reviews, and an introduction that asks for
        // more truncates a ranked list already shorter than the count. The
        // operator gets fewer eyes than they wrote, with nothing above `debug!`
        // saying so — the fail-OPEN direction, which is why this refuses the
        // file instead of clamping the count down to fit.
        //
        // Scoped to `ticketless` because that is the only path that READS
        // `review.reviewers`; a Teams-off or fan-out installation validates
        // exactly as before (the D5 invariant). Like every other rule here it
        // fires regardless of `enabled`, so the complaint arrives while the file
        // is still being edited.
        //
        // `quorum.reviewers` is deliberately left WITHOUT a ceiling even though
        // its path has the same non-author constraint. Two reasons, and the
        // first is decisive: it defaults to 2, so a two-person roster running
        // the shipped quorum config would stop booting on an upgrade that
        // changed nothing in the operator's file — the check would turn working
        // installations off. And the quorum's degradation is a recorded design
        // decision rather than an oversight (`select_reviewers`: "too few
        // candidates degrades to however many exist — never an error, and never
        // a wait"). `review.reviewers` defaults to 1, which every roster of two
        // or more satisfies, so this ceiling can only ever reject a number an
        // operator explicitly wrote.
        //
        // A roster of exactly N with `reviewers: N−1` is ALLOWED, not warned
        // about: an introduction excludes only the author, so all N−1 rows are
        // assignable. The peer exclusion that can strand one of them bites at
        // REASSIGNMENT time — a teammate removed from the roster mid-flight —
        // which is a runtime condition no boot check can see, and is surfaced by
        // `reviewwatch`'s unassignable-round warning instead.
        //
        // `teams.yaml` is boot-only today, so validating at boot is sufficient.
        // **If it is ever made hot-reloadable, this check has to move with it**
        // — a roster shrunk by a reload would otherwise walk straight past it.
        if self.review.mode == ReviewMode::Ticketless {
            // The floor first: `reviewers: 0` is measured as the one reviewer it
            // actually becomes, not as a free pass under the ceiling.
            let asked = self.review.effective_reviewers();
            let roster = self.roster.len();
            let ceiling = roster.saturating_sub(1);
            // The second conjunct keeps the promise the paragraph above makes:
            // this rejects only a count an operator WROTE. One reviewer is the
            // floor and the default, so a single-teammate roster — which cannot
            // review anything whatever the count says — keeps booting and keeps
            // getting `plan_review_intro`'s "the roster holds nobody but the
            // author" warning, rather than having Teams switched off underneath
            // it by an upgrade. That is a pre-existing, already-reported
            // condition and not this ceiling's to escalate.
            let written = asked > usize::try_from(MIN_QUORUM_REVIEWERS).unwrap_or(1);
            if written && asked > ceiling {
                // "Lower it to 0" is not advice — the floor clamps 0 back up to
                // one — so a roster that cannot review at all is told the only
                // remedy it actually has.
                let or_lower = if ceiling == 0 {
                    String::new()
                } else {
                    format!(", or lower `review.reviewers` to {ceiling}")
                };
                return Err(TeamsError::Invalid(format!(
                    "review.reviewers is {asked}, but a roster of {roster} can satisfy at most \
                     {ceiling} ({roster} teammates − 1 author): a pull request's reviewers must \
                     be non-authors, so every round would arm {ceiling} of the {asked} reviews \
                     asked for and drop the rest without saying so. To fix, add {} more \
                     teammate(s) to `roster:`{or_lower}",
                    asked + 1 - roster
                )));
            }
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

    /// **§0.13's "enabling Teams should default the manager to `labels+model`"**, pinned as a
    /// property rather than as a constant (STUDIO-678).
    ///
    /// The mode matters because `labels` gives the manager no model turn, and without one it cannot
    /// read intent out of an operator's room post — so a fresh install would meet David's ruling
    /// only in part. What this asserts is the shape of the failure it prevents: a `teams.yaml` a
    /// human wrote by hand with nothing but `enabled` and a roster is a config that opted IN to
    /// Teams and said nothing about the manager, and that config must hear the operator.
    #[test]
    fn enabling_teams_defaults_to_the_mode_that_hears_the_operator() {
        let t = Teams::parse("enabled: true\nroster:\n  - name: alice\n").expect("parse");
        assert!(t.enabled);
        assert_eq!(
            t.manager.mode,
            ManagerMode::LabelsModel,
            "a teams.yaml that enables Teams and says nothing about the manager must get the mode \
             that can act on an operator's room post (§0.13)"
        );
        // And saying so explicitly still wins: the default is a default, not a policy.
        let explicit =
            Teams::parse("enabled: true\nmanager:\n  mode: labels\nroster:\n  - name: alice\n")
                .expect("parse");
        assert_eq!(explicit.manager.mode, ManagerMode::Labels);
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
            // `labels+model` since STUDIO-678 — see [`ManagerMode::LabelsModel`] for why the
            // default moved, and `enabling_teams_defaults_to_the_mode_that_hears_the_operator`
            // for the property that move exists to hold.
            assert_eq!(t.manager.mode, ManagerMode::LabelsModel, "({text:?})");
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

    // ── review.mode (STUDIO-719, design §15-d/§16) ──────────────────────────

    /// The cutover's whole safety property: `review.mode` is a NEW key and its
    /// default is `off`, so every `teams.yaml` written before it existed —
    /// including one with `quorum.enabled: true` — parses to exactly the
    /// behaviour it had. An absent, null and empty `review:` block all mean off.
    #[test]
    fn review_mode_is_absent_means_off() {
        for text in [
            "enabled: true\nroster:\n  - name: alice\n",
            "enabled: true\nreview:\nroster:\n  - name: alice\n",
            "enabled: true\nreview: {}\nroster:\n  - name: alice\n",
            // The pre-STUDIO-719 shape of an install that opted into the quorum.
            "enabled: true\nquorum:\n  enabled: true\nroster:\n  - name: alice\n",
        ] {
            let t = Teams::parse(text).unwrap_or_else(|e| panic!("parse {text:?}: {e}"));
            assert_eq!(t.review, Review::default(), "({text:?})");
            assert_eq!(t.review.mode, ReviewMode::Off, "({text:?})");
            assert!(
                !t.review_ticketless(),
                "an absent review section must not reach the ticketless path ({text:?})"
            );
            t.validate()
                .unwrap_or_else(|e| panic!("must stay valid {text:?}: {e}"));
        }
        assert_eq!(Teams::disabled().review.mode, ReviewMode::Off);
    }

    // ── review.auto_merge (STUDIO-874) ──────────────────────────────────────

    /// The auto-merge gate's default is OFF, so every `teams.yaml` written
    /// before the key existed keeps merging exactly nobody. An absent, null and
    /// empty `review:` block all mean off, and so does an explicit `false`.
    #[test]
    fn review_auto_merge_is_absent_means_off() {
        for text in [
            "enabled: true\nroster:\n  - name: alice\n",
            "enabled: true\nreview:\nroster:\n  - name: alice\n",
            "enabled: true\nreview: {}\nroster:\n  - name: alice\n",
            "enabled: true\nreview:\n  mode: ticketless\nroster:\n  - name: alice\n",
            "enabled: true\nreview:\n  mode: ticketless\n  auto_merge: false\nroster:\n  - name: alice\n",
        ] {
            let t = Teams::parse(text).unwrap_or_else(|e| panic!("parse {text:?}: {e}"));
            assert!(
                !t.review_auto_merge(),
                "auto-merge must be off unless asked for ({text:?})"
            );
            t.validate()
                .unwrap_or_else(|e| panic!("must stay valid {text:?}: {e}"));
        }
        assert!(!Teams::disabled().review_auto_merge());
    }

    /// The D5 invariant, in the one predicate every caller reads: `auto_merge:
    /// true` reaches nothing unless Teams is on AND review is ticketless. A
    /// Teams-off install, and a `tickets`/`off` install, are unchanged however
    /// the key is spelled — which matters because the verdict this gate consumes
    /// is written only by the ticketless path.
    #[test]
    fn review_auto_merge_needs_teams_and_the_ticketless_path() {
        let on = "enabled: true\nreview:\n  mode: ticketless\n  auto_merge: true\nroster:\n  - name: alice\n";
        assert!(Teams::parse(on).expect("parse").review_auto_merge());

        for text in [
            // Teams off — structurally unreachable, however the block reads.
            "enabled: false\nreview:\n  mode: ticketless\n  auto_merge: true\nroster:\n  - name: alice\n",
            // A review path that records no machine-readable verdict to gate on.
            "enabled: true\nreview:\n  mode: tickets\n  auto_merge: true\nroster:\n  - name: alice\n",
            "enabled: true\nreview:\n  mode: off\n  auto_merge: true\nroster:\n  - name: alice\n",
        ] {
            let t = Teams::parse(text).unwrap_or_else(|e| panic!("parse {text:?}: {e}"));
            assert!(
                !t.review_auto_merge(),
                "auto-merge must not reach a non-ticketless install ({text:?})"
            );
        }
    }

    // ── per-project review.auto_merge (STUDIO-927) ──────────────────────────

    /// The ticket's headline: one project may opt OUT of a global auto-merge
    /// while its sibling keeps it. Mutation: drop the `projects` lookup in
    /// [`Teams::review_auto_merge_for`] so the top-level value always wins —
    /// `booch` then reads `true` and this goes red.
    #[test]
    fn a_per_project_false_beats_a_global_true() {
        let t = Teams::parse(
            "enabled: true\nreview:\n  mode: ticketless\n  auto_merge: true\n\
             projects:\n  - slugs: [booch]\n    review:\n      auto_merge: false\n\
             roster:\n  - name: alice\n",
        )
        .expect("parse");
        assert!(
            !t.review_auto_merge_for("booch"),
            "the named project must not auto-merge"
        );
        assert!(
            t.review_auto_merge_for("rhapsody"),
            "a sibling project with no entry still inherits the global true"
        );
        assert!(
            t.review_auto_merge_for("never-configured"),
            "a project that has never been configured behaves exactly as it did before the block"
        );
    }

    /// A per-project `true` under a global `false` turns auto-merge ON for that
    /// project alone, and an entry that sets no `auto_merge` inherits — in both
    /// directions, which is the asymmetry the ticket calls safety-critical.
    #[test]
    fn an_unset_per_project_auto_merge_inherits_the_global_in_both_directions() {
        // Global OFF: naming a project with no `auto_merge` must not turn it on.
        let off = Teams::parse(
            "enabled: true\nreview:\n  mode: ticketless\n\
             projects:\n  - slugs: [quiet]\n\
             roster:\n  - name: alice\n",
        )
        .expect("parse");
        assert!(!off.review_auto_merge_for("quiet"), "unset inherits false");
        assert!(!off.review_auto_merge_for("other"), "and so does unlisted");

        // Global OFF, an explicit per-project true is the override.
        let on = Teams::parse(
            "enabled: true\nreview:\n  mode: ticketless\n\
             projects:\n  - slugs: [loud]\n    review:\n      auto_merge: true\n\
             roster:\n  - name: alice\n",
        )
        .expect("parse");
        assert!(
            on.review_auto_merge_for("loud"),
            "set wins for that project"
        );
        assert!(!on.review_auto_merge_for("other"), "and only that project");
    }

    /// The multi-slug spelling: a fanned project's override applies to every slug
    /// it names, and an absent `projects:` block leaves no project changed.
    #[test]
    fn a_multi_slug_entry_applies_to_every_slug_it_names() {
        let t = Teams::parse(
            "enabled: true\nreview:\n  mode: ticketless\n  auto_merge: true\n\
             projects:\n  - slugs: [alpha, alpha-2]\n    review:\n      auto_merge: false\n\
             roster:\n  - name: alice\n",
        )
        .expect("parse");
        for slug in ["alpha", "alpha-2"] {
            assert!(!t.review_auto_merge_for(slug), "{slug}");
        }
        assert!(
            t.review_auto_merge_for("beta"),
            "beta is a different project"
        );
    }

    /// The legacy decode the ticket names: a `teams.yaml` with only the bare
    /// top-level `review.auto_merge` parses to an EMPTY `projects` block and
    /// answers identically to the pre-STUDIO-927 accessor for every slug.
    #[test]
    fn a_legacy_bare_auto_merge_decodes_to_no_projects_and_unchanged_behaviour() {
        for (yaml, want) in [
            (
                "enabled: true\nreview:\n  mode: ticketless\n  auto_merge: true\nroster:\n  - name: alice\n",
                true,
            ),
            (
                "enabled: true\nreview:\n  mode: ticketless\n  auto_merge: false\nroster:\n  - name: alice\n",
                false,
            ),
            (
                "enabled: true\nreview:\n  mode: ticketless\nroster:\n  - name: alice\n",
                false,
            ),
        ] {
            let t = Teams::parse(yaml).unwrap_or_else(|e| panic!("parse {yaml:?}: {e}"));
            assert!(t.projects.is_empty(), "legacy config carries no projects");
            for slug in ["booch", "rhapsody", "anything"] {
                assert_eq!(t.review_auto_merge_for(slug), want, "{slug} under {yaml:?}");
                assert_eq!(
                    t.review_auto_merge_for(slug),
                    t.review_auto_merge(),
                    "the per-project accessor must agree with the global one when nothing is scoped"
                );
            }
        }
    }

    /// The D5 invariant, per project: a per-project `true` is dead config unless
    /// Teams is on AND review is ticketless. Mutation: drop the
    /// `review_ticketless` gate from [`Teams::review_auto_merge_for`] and the
    /// Teams-off row below reads `true`.
    #[test]
    fn a_per_project_override_cannot_escape_the_ticketless_gate() {
        for (yaml, want) in [
            (
                "enabled: false\nreview:\n  mode: ticketless\n\
                 projects:\n  - slugs: [booch]\n    review:\n      auto_merge: true\n\
                 roster:\n  - name: alice\n",
                false,
            ),
            (
                "enabled: true\nreview:\n  mode: tickets\n\
                 projects:\n  - slugs: [booch]\n    review:\n      auto_merge: true\n\
                 roster:\n  - name: alice\n",
                false,
            ),
            (
                "enabled: true\nreview:\n  mode: ticketless\n\
                 projects:\n  - slugs: [booch]\n    review:\n      auto_merge: true\n\
                 roster:\n  - name: alice\n",
                true,
            ),
        ] {
            let t = Teams::parse(yaml).unwrap_or_else(|e| panic!("parse {yaml:?}: {e}"));
            assert_eq!(t.review_auto_merge_for("booch"), want, "{yaml:?}");
        }
        assert!(!Teams::disabled().review_auto_merge_for("booch"));
    }

    /// The visibility half of the ticket's trap: an override that names a slug no
    /// resolved project carries is inert, and [`Teams::unmatched_project_slugs`]
    /// names it so the boot can warn. A project NAME where the Linear `slugId`
    /// belongs is the exact case — the documented example `slugs: [booch]` can
    /// never match a resolved project.
    #[test]
    fn an_override_naming_no_resolved_project_slug_is_reported() {
        let t = Teams::parse(
            "enabled: true\nreview:\n  mode: ticketless\n  auto_merge: true\n\
             projects:\n  - slugs: [booch]\n    review:\n      auto_merge: false\n\
             roster:\n  - name: alice\n",
        )
        .expect("parse");
        assert_eq!(
            t.unmatched_project_slugs(&["4f4a2350682f".to_string()]),
            vec!["booch"],
            "a name where a slugId belongs must be reported"
        );

        // A matching slug is not reported, and a partially-matching entry reports
        // only the half that matches nothing.
        let mixed = Teams::parse(
            "enabled: true\nreview:\n  mode: ticketless\n\
             projects:\n  - slugs: [4f4a2350682f, typo]\n\
             roster:\n  - name: alice\n",
        )
        .expect("parse");
        assert_eq!(
            mixed.unmatched_project_slugs(&["4f4a2350682f".to_string()]),
            vec!["typo"]
        );
        assert!(Teams::disabled().unmatched_project_slugs(&[]).is_empty());

        // Whitespace around a slug is trimmed on both sides, matching `validate`'s
        // in-place trim before the orchestrator resolves: padded, it is neither a
        // false-positive report nor a miss in the override lookup.
        let padded = Teams::parse(
            "enabled: true\nreview:\n  mode: ticketless\n  auto_merge: true\n\
             projects:\n  - slugs: [' 4f4a2350682f ']\n    review:\n      auto_merge: false\n\
             roster:\n  - name: alice\n",
        )
        .expect("parse");
        assert!(
            padded
                .unmatched_project_slugs(&["4f4a2350682f".to_string()])
                .is_empty()
        );
        assert!(!padded.review_auto_merge_for("4f4a2350682f"));
    }

    /// A `projects:` entry may be written with an empty or null `review:` block —
    /// it then inherits, exactly as the null-block tolerance elsewhere in this
    /// file does. Everything the types cannot accept is a loud parse error rather
    /// than a silently-inherited override: a wrong type AND an unknown key (a
    /// misspelling, or the easy mis-indent placing `auto_merge` beside `slugs`
    /// instead of under `review:`) both fail, so a typo cannot leave `auto_merge`
    /// unset and a repo the operator meant to hold back merging itself.
    #[test]
    fn a_project_entry_with_no_review_block_inherits_and_a_malformed_one_is_rejected() {
        let t = Teams::parse(
            "enabled: true\nreview:\n  mode: ticketless\n  auto_merge: true\n\
             projects:\n  - slugs: [booch]\n  - slugs: [b]\n    review:\n\
             roster:\n  - name: alice\n",
        )
        .expect("parse");
        assert!(t.review_auto_merge_for("booch"), "no review block inherits");
        assert!(
            t.review_auto_merge_for("b"),
            "an empty review block inherits"
        );

        for bad in [
            // `projects` must be a list, not a map or a scalar.
            "enabled: true\nprojects:\n  booch: false\n",
            "enabled: true\nprojects: booch\n",
            // `slugs` must be a list.
            "enabled: true\nprojects:\n  - slugs: booch\n",
            // `auto_merge` must be a boolean.
            "enabled: true\nprojects:\n  - slugs: [booch]\n    review:\n      auto_merge: nope\n",
            // MIS-INDENT: `auto_merge` beside `slugs` rather than under `review:`
            // — with a lenient decode this would parse, leave the override unset
            // and inherit the global ON.
            "enabled: true\nprojects:\n  - slugs: [booch]\n    auto_merge: false\n",
            // MISSPELLED keys, at the entry and inside the review block.
            "enabled: true\nprojects:\n  - slug: [booch]\n    review:\n      auto_merge: false\n",
            "enabled: true\nprojects:\n  - slugs: [booch]\n    review:\n      auto-merge: false\n",
        ] {
            let err = Teams::parse(bad).expect_err("must not parse");
            assert!(matches!(err, TeamsError::Parse(_)), "{bad:?}: got {err}");
        }
    }

    /// All three spellings decode, and nothing else does: a typo must be a loud
    /// parse error rather than a silent fall back to `off`, which would leave an
    /// operator who asked for review with none and no complaint.
    #[test]
    fn review_mode_decodes_exactly_three_spellings() {
        for (yaml, want) in [
            ("review:\n  mode: off\n", ReviewMode::Off),
            ("review:\n  mode: tickets\n", ReviewMode::Tickets),
            ("review:\n  mode: ticketless\n", ReviewMode::Ticketless),
        ] {
            let t = Teams::parse(yaml).unwrap_or_else(|e| panic!("parse {yaml:?}: {e}"));
            assert_eq!(t.review.mode, want, "({yaml:?})");
        }
        for yaml in [
            "review:\n  mode: quorum\n",
            "review:\n  mode: Ticketless\n",
            "review:\n  mode: true\n",
        ] {
            let err = Teams::parse(yaml).expect_err("an unknown mode must not parse");
            assert!(matches!(err, TeamsError::Parse(_)), "{yaml:?}: got {err}");
        }
    }

    /// §16's gate, at the config layer: ticketless review is reachable ONLY with
    /// Teams enabled AND `mode: ticketless`. Teams off is dormant for every mode,
    /// which is the invariant the later slices inherit rather than re-check.
    #[test]
    fn review_ticketless_requires_teams_enabled_and_the_ticketless_mode() {
        for (enabled, mode, want) in [
            (true, ReviewMode::Ticketless, true),
            (true, ReviewMode::Tickets, false),
            (true, ReviewMode::Off, false),
            (false, ReviewMode::Ticketless, false),
            (false, ReviewMode::Tickets, false),
            (false, ReviewMode::Off, false),
        ] {
            let t = Teams {
                enabled,
                review: Review {
                    mode,
                    ..Review::default()
                },
                ..Teams::disabled()
            };
            assert_eq!(
                t.review_ticketless(),
                want,
                "enabled={enabled} mode={mode:?}"
            );
        }
    }

    /// §15-d's mutual exclusion: the two review paths read the same handoff, so
    /// asking for both is rejected outright — and, because `try_load` validates,
    /// a daemon booting that file falls back to Teams OFF rather than to double
    /// review. The complaint names both keys and a way out of it.
    #[test]
    fn quorum_enabled_with_ticketless_review_is_rejected() {
        let both = "enabled: true\nquorum:\n  enabled: true\nreview:\n  mode: ticketless\nroster:\n  - name: alice\n";
        let err = Teams::parse(both)
            .expect("parses")
            .validate()
            .expect_err("both review paths at once must be rejected");
        let TeamsError::Invalid(msg) = &err else {
            panic!("expected Invalid, got {err}")
        };
        assert!(msg.contains("quorum.enabled"), "{msg}");
        assert!(msg.contains("review.mode: ticketless"), "{msg}");
        assert!(
            msg.contains("mutually exclusive — they are two review paths"),
            "the continued literal must render as one sentence: {msg}"
        );
        assert!(!msg.contains("  "), "leaked source indentation: {msg}");

        // The same rule while the toggle is off, like every other rule here: the
        // complaint arrives while the file is being edited, not after.
        let disabled = both.replace("enabled: true\nquorum", "enabled: false\nquorum");
        assert!(matches!(
            Teams::parse(&disabled).expect("parses").validate(),
            Err(TeamsError::Invalid(_))
        ));

        // And the boot path turns it into the off state rather than a crash.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("teams.yaml");
        std::fs::write(&path, both).expect("write");
        assert!(matches!(
            Teams::try_load(&path),
            Err(TeamsError::Invalid(_))
        ));
        assert_eq!(Teams::load(&path), Teams::disabled());
    }

    /// The other three combinations are legal, and the important one is the
    /// first: `quorum.enabled: true` with `mode: tickets` is today's install,
    /// spelled explicitly, and it must keep validating.
    #[test]
    fn every_other_quorum_and_review_combination_is_accepted() {
        for (quorum, mode) in [
            (true, ReviewMode::Tickets),
            (true, ReviewMode::Off),
            (false, ReviewMode::Ticketless),
            (false, ReviewMode::Tickets),
            (false, ReviewMode::Off),
        ] {
            let t = Teams {
                enabled: true,
                quorum: Quorum {
                    enabled: quorum,
                    ..Quorum::default()
                },
                review: Review {
                    mode,
                    ..Review::default()
                },
                roster: vec![Identity {
                    name: "alice".to_string(),
                    ..Identity::default()
                }],
                ..Teams::disabled()
            };
            t.validate()
                .unwrap_or_else(|e| panic!("quorum={quorum} mode={mode:?}: {e}"));
        }
    }

    /// STUDIO-721 (decision C): the ticketless path asks ONE teammate by default
    /// — not the quorum's two — and a nonsensical count clamps up to one rather
    /// than turning an enabled review mode into a silent no-op.
    #[test]
    fn review_reviewers_defaults_to_one_and_floors_at_one() {
        assert_eq!(Review::default().reviewers, DEFAULT_REVIEW_REVIEWERS);
        assert_eq!(Review::default().effective_reviewers(), 1);
        assert_eq!(
            Teams::disabled().review.reviewers,
            1,
            "an absent `review:` block is one reviewer, not the quorum's two"
        );
        for (configured, want) in [(-7, 1), (0, 1), (1, 1), (2, 2), (5, 5)] {
            let r = Review {
                reviewers: configured,
                ..Review::default()
            };
            assert_eq!(r.effective_reviewers(), want, "reviewers: {configured}");
        }
    }

    /// STUDIO-891: `review.reviewers` has a CEILING as well as a floor, and the
    /// ceiling is the roster minus the author.
    ///
    /// `pick_reviewer` only ever names non-authors, so a roster of N supports at
    /// most N−1 required reviews of one pull request. Asked for more, the
    /// introduction path truncates a ranked list that is already shorter than the
    /// count and arms fewer rows than the operator wrote — silently, with nothing
    /// above `debug!` to say the config was not honoured. Rejecting the file is
    /// the fail-CLOSED direction; reviewing with fewer eyes than were asked for
    /// is the fail-open one.
    ///
    /// The boundary is the whole point, so it is asserted at all three points
    /// either side of it.
    #[test]
    fn review_reviewers_above_the_rosters_ceiling_is_rejected() {
        let yaml = |names: &[&str], reviewers: i64| {
            let roster: String = names
                .iter()
                .map(|n| format!("  - name: {n}\n"))
                .collect::<String>();
            format!(
                "enabled: true\nreview:\n  mode: ticketless\n  reviewers: {reviewers}\nroster:\n{roster}"
            )
        };
        // roster 3 / reviewers 2 — satisfiable (decision 3: exactly N−1 is
        // ALLOWED, not warned about; the author is the only exclusion an
        // introduction makes).
        Teams::parse(&yaml(&["alice", "jimmy", "jerry"], 2))
            .expect("parses")
            .validate()
            .expect("roster 3 / reviewers 2 is satisfiable");
        // roster 2 / reviewers 1 — satisfiable.
        Teams::parse(&yaml(&["alice", "jimmy"], 1))
            .expect("parses")
            .validate()
            .expect("roster 2 / reviewers 1 is satisfiable");
        // roster 2 / reviewers 2 — the case that motivated the ticket.
        let err = Teams::parse(&yaml(&["alice", "jimmy"], 2))
            .expect("parses")
            .validate()
            .expect_err("roster 2 / reviewers 2 is unsatisfiable");
        let TeamsError::Invalid(msg) = &err else {
            panic!("expected Invalid, got {err}")
        };
        // The message names BOTH numbers and the arithmetic, so the operator can
        // act without reading the source.
        assert!(msg.contains("review.reviewers is 2"), "{msg}");
        assert!(msg.contains("roster of 2"), "{msg}");
        assert!(msg.contains("at most 1"), "{msg}");
        assert!(msg.contains("non-author"), "{msg}");
        assert!(!msg.contains("  "), "leaked source indentation: {msg}");

        // A roster of one is NOT escalated to a boot rejection: one reviewer is
        // the floor and the shipped default, so nobody wrote it, and rejecting
        // here would switch Teams off under an installation whose file did not
        // change. It cannot review anything either way, and `plan_review_intro`
        // already warns that it holds nobody but the author.
        for count in [0, 1] {
            Teams::parse(&yaml(&["alice"], count))
                .expect("parses")
                .validate()
                .unwrap_or_else(|e| panic!("reviewers: {count} on a solo roster: {e}"));
        }
        // But a count that WAS written is still measured against that roster.
        let solo = Teams::parse(&yaml(&["alice"], 2))
            .expect("parses")
            .validate()
            .expect_err("a roster of 1 has no non-author, so two is unsatisfiable");
        assert!(solo.to_string().contains("at most 0"), "{solo}");
    }

    /// The D5 invariant: the ceiling is a rule about the path that READS
    /// `review.reviewers`, so it fires only under `mode: ticketless`. A Teams-off
    /// install, and any installation on the ticket fan-out, validate exactly as
    /// they did before.
    ///
    /// `quorum.reviewers` is deliberately NOT given the same ceiling — see
    /// [`Teams::validate`]'s note. Pinned here because it is a DECISION, not an
    /// omission: a two-person roster running the quorum's default of two would
    /// otherwise stop booting on upgrade, with nothing in the operator's file
    /// having changed.
    #[test]
    fn the_reviewer_ceiling_is_scoped_to_the_ticketless_path() {
        Teams::disabled()
            .validate()
            .expect("the off state is unaffected");
        for mode in ["off", "tickets"] {
            let t = Teams::parse(&format!(
                "enabled: true\nreview:\n  mode: {mode}\n  reviewers: 9\nroster:\n  - name: alice\n  - name: jimmy\n"
            ))
            .expect("parses");
            t.validate()
                .unwrap_or_else(|e| panic!("mode {mode} must not consult the ceiling: {e}"));
        }
        // The quorum's own count, at its default, on the roster size that
        // motivated the ticket.
        Teams::parse(
            "enabled: true\nquorum:\n  enabled: true\nroster:\n  - name: alice\n  - name: jimmy\n",
        )
        .expect("parses")
        .validate()
        .expect("quorum.reviewers has no ceiling; an upgrade must not turn this install off");
    }

    // ── review.model / review.effort (STUDIO-901, scoped by harness in STUDIO-908) ───

    /// Absent means inherit, never reset — the same rule STUDIO-868's profile
    /// `model`/`effort` and [`Review::done_state`] already follow. An installation
    /// that never writes `review.model`/`review.effort` parses to an empty map,
    /// and a bare `""` is the unset value rather than an override to nothing.
    #[test]
    fn review_model_and_effort_default_to_empty_inherit() {
        assert!(Review::default().model.is_empty());
        assert!(Review::default().effort.is_empty());
        for text in [
            "enabled: true\nreview:\n  mode: ticketless\nroster:\n  - name: alice\n",
            "enabled: true\nreview:\n  mode: ticketless\n  model: \"\"\nroster:\n  - name: alice\n",
            "enabled: true\nreview:\n  mode: ticketless\n  model:\nroster:\n  - name: alice\n",
        ] {
            let t = Teams::parse(text).unwrap_or_else(|e| panic!("parse {text:?}: {e}"));
            assert!(t.review.model.is_empty(), "({text:?})");
            assert!(t.review.effort.is_empty(), "({text:?})");
            assert_eq!(
                t.review_model_for("claude", "claude"),
                ReviewModelChoice::Inherit,
                "{text:?}"
            );
        }
    }

    /// Both wire spellings parse. The bare scalar is the legacy STUDIO-901 spelling: it is stored
    /// unresolved and resolved against the caller's fallback harness at dispatch, which is the
    /// installation's `agent.backend` (`a_legacy_bare_review_model_belongs_to_the_configured_backend…`
    /// covers the non-claude case). The map is the STUDIO-908 spelling and scopes a value per
    /// harness by name.
    #[test]
    fn review_model_parses_both_the_legacy_scalar_and_the_per_harness_map() {
        let bare = Teams::parse(
            "enabled: true\nreview:\n  mode: ticketless\n  model: claude-opus-5\n  effort: high\nroster:\n  - name: alice\n",
        )
        .expect("parses");
        assert_eq!(bare.review.model.legacy(), Some("claude-opus-5"));
        assert_eq!(bare.review.effort.legacy(), Some("high"));
        assert_eq!(
            bare.review_model_for("claude", "claude"),
            ReviewModelChoice::Use("claude-opus-5")
        );

        let scoped = Teams::parse(
            "enabled: true\nreview:\n  mode: ticketless\n  model:\n    claude: claude-opus-5\n    opencode: fireworks-ai/accounts/fireworks/models/deepseek-v4p1-flash\nroster:\n  - name: alice\n",
        )
        .expect("parses");
        assert_eq!(scoped.review.model.legacy(), None);
        assert_eq!(
            scoped.review.model.for_harness("claude", "claude"),
            Some("claude-opus-5")
        );
        assert_eq!(
            scoped.review.model.for_harness("opencode", "claude"),
            Some("fireworks-ai/accounts/fireworks/models/deepseek-v4p1-flash")
        );
        assert_eq!(
            scoped.review_model_for("opencode", "claude"),
            ReviewModelChoice::Use("fireworks-ai/accounts/fireworks/models/deepseek-v4p1-flash")
        );
    }

    /// **alice's blocking finding on PR #172.** The legacy bare scalar is NOT pinned to the literal
    /// `claude`: it belongs to the installation's configured `agent.backend`, so an all-opencode
    /// installation whose bare `review.model` works today keeps working. Resolving it against a
    /// hardcoded `claude` refused every ticketless review on that install and blamed the operator's
    /// opencode model on a `claude` key they never wrote.
    ///
    /// The other direction is the acceptance that must not regress: the same bare value is still
    /// refused for a reviewer on a DIFFERENT harness, never silently re-scoped.
    #[test]
    fn a_legacy_bare_review_model_belongs_to_the_configured_backend_not_a_hardcoded_claude() {
        let t = Teams::parse(
            "enabled: true\nreview:\n  mode: ticketless\n  model: some-opencode-model\nroster:\n  - name: alice\n",
        )
        .expect("parses");

        // All-opencode installation: the bare scalar is an opencode model, and it applies.
        assert_eq!(
            t.review_model_for("opencode", "opencode"),
            ReviewModelChoice::Use("some-opencode-model")
        );
        // Same install, a reviewer on another harness: refused, naming the value's own harness.
        let ReviewModelChoice::Refuse(msg) = t.review_model_for("claude", "opencode") else {
            panic!("a claude reviewer must not be handed the opencode model");
        };
        assert!(
            msg.contains("opencode (model some-opencode-model)"),
            "the value's harness must be named as opencode, not claude: {msg}"
        );

        // All-claude installation: the same bare scalar applies to claude, exactly as STUDIO-901
        // shipped it.
        assert_eq!(
            t.review_model_for("claude", "claude"),
            ReviewModelChoice::Use("some-opencode-model")
        );
    }

    /// **The seam this ticket closes (STUDIO-908), at the config layer.** A value scoped to one
    /// harness is not applied to a reviewer on another: the answer is `Refuse`, and its message
    /// names the reviewer's harness, the configured model and the `review.model` origin. Mutation
    /// check: making the lookup ignore the harness (returning the first entry) turns the `Refuse`
    /// assertions red.
    #[test]
    fn a_review_model_scoped_to_another_harness_is_refused_and_names_harness_model_and_origin() {
        let t = Teams::parse(
            "enabled: true\nreview:\n  mode: ticketless\n  model:\n    claude: claude-opus-5\nroster:\n  - name: alice\n",
        )
        .expect("parses");
        let ReviewModelChoice::Refuse(msg) = t.review_model_for("opencode", "claude") else {
            panic!("an opencode reviewer must not be handed the claude model");
        };
        assert!(
            msg.contains("opencode"),
            "must name the reviewer's harness: {msg}"
        );
        assert!(msg.contains("claude-opus-5"), "must name the model: {msg}");
        assert!(msg.contains("review.model"), "must name the origin: {msg}");
        // The harness the value WAS written for is named too, so the operator sees the mismatch.
        assert!(msg.contains("claude (model claude-opus-5)"), "{msg}");
    }

    /// The refusal is `model`'s alone. `review.effort` scoped to another harness leaves the effort
    /// inherited rather than refusing the run: an effort value cannot make a provider reject a
    /// model, so it is not worth a failed review round.
    #[test]
    fn a_review_effort_scoped_to_another_harness_is_inherited_not_refused() {
        let t = Teams::parse(
            "enabled: true\nreview:\n  mode: ticketless\n  model:\n    opencode: cheap\n  effort:\n    claude: high\nroster:\n  - name: alice\n",
        )
        .expect("parses");
        assert_eq!(
            t.review_model_for("opencode", "claude"),
            ReviewModelChoice::Use("cheap")
        );
        assert_eq!(t.review_effort("opencode", "claude"), None);
        assert_eq!(t.review_effort("claude", "claude"), Some("high"));
    }

    /// A harness with no entry does not refuse when NOTHING is configured anywhere — the
    /// byte-identical-without-the-key property STUDIO-901 promised, preserved per harness.
    #[test]
    fn an_absent_review_model_inherits_for_every_harness() {
        let t = Teams {
            enabled: true,
            review: Review {
                mode: ReviewMode::Ticketless,
                ..Review::default()
            },
            ..Teams::disabled()
        };
        for h in ["claude", "opencode"] {
            assert_eq!(
                t.review_model_for(h, "claude"),
                ReviewModelChoice::Inherit,
                "{h}"
            );
            assert_eq!(t.review_effort(h, "claude"), None, "{h}");
        }
    }

    /// `review.required` is read on BOTH review paths, unlike every other `review.*` accessor: a
    /// required reviewer is pinned whichever path an installation runs, so an installation on the
    /// quorum (`mode: off`/`tickets`) must still answer the list. Blanks and surrounding whitespace
    /// are dropped, so a `required: [""]` reads as unset rather than as a name no roster can hold.
    #[test]
    fn review_required_is_ungated_and_trims() {
        let t = Teams::parse(
            "enabled: true\nquorum:\n  enabled: true\nreview:\n  required:\n    - ' sol '\n    - ''\n    - bob\nroster:\n  - name: sol\n  - name: bob\n",
        )
        .expect("parses");
        assert!(!t.review_ticketless(), "this is the quorum path");
        assert_eq!(t.review_required(), vec!["sol", "bob"]);
    }

    /// An absent key is the empty list — the state every pre-STUDIO-951 file is already in, and
    /// what makes selection byte-identical to before the key existed.
    #[test]
    fn an_unset_required_list_is_empty() {
        let t = Teams::parse("enabled: true\nroster:\n  - name: alice\n").expect("parses");
        assert!(t.review_required().is_empty());
        assert!(t.over_pinned_reviewers().is_none());
    }

    /// The active path's count is the one an over-long pin list is compared against, and it
    /// follows which path is actually on: `review.reviewers` on ticketless, `quorum.reviewers` on
    /// the fan-out, and `None` when neither is enabled.
    #[test]
    fn active_reviewer_count_follows_the_path_that_is_on() {
        let quorum = Teams::parse(
            "enabled: true\nquorum:\n  enabled: true\n  reviewers: 3\nroster:\n  - name: alice\n",
        )
        .expect("parses");
        assert_eq!(quorum.active_reviewer_count(), Some(3));

        let ticketless = Teams::parse(
            "enabled: true\nreview:\n  mode: ticketless\n  reviewers: 2\nroster:\n  - name: alice\n",
        )
        .expect("parses");
        assert_eq!(ticketless.active_reviewer_count(), Some(2));

        // Teams enabled but no review path on: `review.required` is dead config, not a warning.
        let neither = Teams::parse("enabled: true\nroster:\n  - name: alice\n").expect("parses");
        assert_eq!(neither.active_reviewer_count(), None);

        // Teams off is not on any path whatever else is set.
        let off =
            Teams::parse("enabled: false\nquorum:\n  enabled: true\nroster:\n  - name: alice\n")
                .expect("parses");
        assert_eq!(off.active_reviewer_count(), None);
    }

    /// Edge 2's diagnostic: fewer reviewer slots than pins is `Some((required, total))`, the two
    /// numbers the boot warning names. Duplicates occupy one slot, so they are not counted twice.
    #[test]
    fn over_pinned_reviewers_reports_both_numbers() {
        let over = Teams::parse(
            "enabled: true\nquorum:\n  enabled: true\n  reviewers: 1\nreview:\n  required: [sol, bob]\nroster:\n  - name: sol\n  - name: bob\n",
        )
        .expect("parses");
        assert_eq!(over.over_pinned_reviewers(), Some((2, 1)));

        let fits = Teams::parse(
            "enabled: true\nquorum:\n  enabled: true\n  reviewers: 2\nreview:\n  required: [sol]\nroster:\n  - name: sol\n  - name: bob\n",
        )
        .expect("parses");
        assert_eq!(fits.over_pinned_reviewers(), None);

        let duplicated = Teams::parse(
            "enabled: true\nquorum:\n  enabled: true\n  reviewers: 2\nreview:\n  required: [sol, sol]\nroster:\n  - name: sol\n  - name: bob\n",
        )
        .expect("parses");
        assert_eq!(
            duplicated.over_pinned_reviewers(),
            None,
            "a repeated name is one reviewer, not two"
        );
    }

    /// **sol round-2 finding 3 on PR #190.** A pin that is not on the roster can never be selected,
    /// so it occupies no slot and must not make the over-pin count claim a clamp. It is reported by
    /// `unknown_required_reviewers` instead — a boot diagnostic of its own, with a count that never
    /// disagrees with selection.
    ///
    /// Mutation check: drop the roster filter from `over_pinned_reviewers` and the first assertion
    /// goes red at `Some((2, 1))`, the false warning this pins down.
    #[test]
    fn off_roster_pins_are_reported_separately_from_the_clamp() {
        let over = Teams::parse(
            "enabled: true\nquorum:\n  enabled: true\n  reviewers: 1\nreview:\n  required: [ghost, sol]\nroster:\n  - name: sol\n  - name: bob\n",
        )
        .expect("parses");
        assert_eq!(
            over.over_pinned_reviewers(),
            None,
            "ghost cannot be selected, so the one real pin fits and nothing clamps"
        );
        assert_eq!(over.unknown_required_reviewers(), vec!["ghost"]);

        // The same typo with no real pin at all: still no clamp warning, still reported as unknown.
        let only_unknown = Teams::parse(
            "enabled: true\nquorum:\n  enabled: true\n  reviewers: 1\nreview:\n  required: [ghost]\nroster:\n  - name: sol\n  - name: bob\n",
        )
        .expect("parses");
        assert_eq!(only_unknown.over_pinned_reviewers(), None);
        assert_eq!(only_unknown.unknown_required_reviewers(), vec!["ghost"]);

        // An empty list is the normal state: nothing unknown, nothing clamped.
        let pinned = Teams::parse(
            "enabled: true\nquorum:\n  enabled: true\n  reviewers: 2\nreview:\n  required: [sol]\nroster:\n  - name: sol\n  - name: bob\n",
        )
        .expect("parses");
        assert!(pinned.unknown_required_reviewers().is_empty());
    }

    /// **jimmy/alice round-1 finding 2 on PR #168.** `review_model_for`/`review_effort` must not
    /// claim an override that cannot fire — scoped to the ticketless path exactly as
    /// `review_done_state`/`review_changes_state`/`review_auto_merge` already are, on the SAME
    /// installations those tests exercise (`mode: off`/`tickets`, and Teams disabled entirely).
    #[test]
    fn review_model_and_effort_are_off_until_the_review_path_is_ticketless() {
        assert_eq!(
            Teams::disabled().review_model_for("claude", "claude"),
            ReviewModelChoice::Inherit
        );
        assert_eq!(Teams::disabled().review_effort("claude", "claude"), None);

        let with = |enabled: bool, mode: ReviewMode| Teams {
            enabled,
            review: Review {
                mode,
                model: HarnessScoped::bare("claude-opus-5"),
                effort: HarnessScoped::bare("high"),
                ..Review::default()
            },
            ..Teams::disabled()
        };
        for (enabled, mode) in [
            (true, ReviewMode::Off),
            (true, ReviewMode::Tickets),
            (false, ReviewMode::Ticketless),
        ] {
            let t = with(enabled, mode);
            assert_eq!(
                t.review_model_for("claude", "claude"),
                ReviewModelChoice::Inherit,
                "enabled={enabled} mode={mode:?}: a set-but-inert value must read as unset"
            );
            assert_eq!(
                t.review_effort("claude", "claude"),
                None,
                "enabled={enabled} mode={mode:?}"
            );
        }

        let live = with(true, ReviewMode::Ticketless);
        assert_eq!(
            live.review_model_for("claude", "claude"),
            ReviewModelChoice::Use("claude-opus-5")
        );
        assert_eq!(live.review_effort("claude", "claude"), Some("high"));
    }

    /// An unset value stays unset even on the one installation where it could take effect —
    /// `review_ticketless()` alone must not manufacture an override nobody configured.
    #[test]
    fn review_model_and_effort_stay_absent_on_a_ticketless_team_that_never_set_them() {
        let t = Teams {
            enabled: true,
            review: Review {
                mode: ReviewMode::Ticketless,
                ..Review::default()
            },
            ..Teams::disabled()
        };
        assert_eq!(
            t.review_model_for("claude", "claude"),
            ReviewModelChoice::Inherit
        );
        assert_eq!(t.review_effort("claude", "claude"), None);
    }

    /// STUDIO-712: the auto-Done transition is OFF unless somebody named the
    /// terminal state, and naming it is not enough on an installation whose
    /// review path cannot produce a merge edge to act on.
    #[test]
    fn review_done_state_is_off_until_named_on_a_ticketless_team() {
        assert_eq!(Review::default().done_state, "");
        assert_eq!(Teams::disabled().review_done_state(), None);

        let with = |enabled: bool, mode: ReviewMode, done: &str| Teams {
            enabled,
            review: Review {
                mode,
                done_state: done.to_string(),
                ..Review::default()
            },
            ..Teams::disabled()
        };
        assert_eq!(
            with(true, ReviewMode::Ticketless, "Done").review_done_state(),
            Some("Done"),
        );
        for off in [
            with(true, ReviewMode::Ticketless, ""),
            with(true, ReviewMode::Ticketless, "   "),
            with(true, ReviewMode::Tickets, "Done"),
            with(true, ReviewMode::Off, "Done"),
            with(false, ReviewMode::Ticketless, "Done"),
        ] {
            assert_eq!(off.review_done_state(), None, "{off:?}");
        }
        assert_eq!(
            with(true, ReviewMode::Ticketless, "  Shipped  ").review_done_state(),
            Some("Shipped"),
            "a padded state name is a state name",
        );
    }

    /// An absent `done_state:` key is the off default while an explicit one is
    /// carried verbatim — the `review:` block predates the key, so every
    /// installation that has one must keep parsing.
    #[test]
    fn review_done_state_absent_is_off_and_present_is_honoured() {
        let absent: Review = serde_yaml_ng::from_str("mode: ticketless").expect("parse");
        assert_eq!(absent.done_state, "");
        let present: Review =
            serde_yaml_ng::from_str("mode: ticketless\ndone_state: Shipped").expect("parse");
        assert_eq!(present.done_state, "Shipped");
    }

    /// STUDIO-839: the route-back transition is OFF unless somebody named the
    /// state, and naming it is not enough on an installation whose review path
    /// cannot produce a findings verdict to act on.
    ///
    /// The pair with `review_done_state` is deliberate — both write ticket
    /// state off the back of a review outcome, so they share one gate and one
    /// empty-means-off discipline rather than each inventing their own.
    #[test]
    fn review_changes_state_is_off_until_named_on_a_ticketless_team() {
        assert_eq!(Review::default().changes_state, "");
        assert_eq!(Teams::disabled().review_changes_state(), None);

        let with = |enabled: bool, mode: ReviewMode, changes: &str| Teams {
            enabled,
            review: Review {
                mode,
                changes_state: changes.to_string(),
                ..Review::default()
            },
            ..Teams::disabled()
        };
        assert_eq!(
            with(true, ReviewMode::Ticketless, "In Progress").review_changes_state(),
            Some("In Progress"),
        );
        for off in [
            with(true, ReviewMode::Ticketless, ""),
            with(true, ReviewMode::Ticketless, "   "),
            with(true, ReviewMode::Tickets, "In Progress"),
            with(true, ReviewMode::Off, "In Progress"),
            with(false, ReviewMode::Ticketless, "In Progress"),
        ] {
            assert_eq!(off.review_changes_state(), None, "{off:?}");
        }
        assert_eq!(
            with(true, ReviewMode::Ticketless, "  Doing  ").review_changes_state(),
            Some("Doing"),
            "a padded state name is a state name",
        );
    }

    /// An absent `changes_state:` key is the off default while an explicit one
    /// is carried verbatim — the `review:` block predates the key, so every
    /// installation that has one must keep parsing, and an upgrade must not
    /// invent a transition nobody asked for.
    #[test]
    fn review_changes_state_absent_is_off_and_present_is_honoured() {
        let absent: Review = serde_yaml_ng::from_str("mode: ticketless").expect("parse");
        assert_eq!(absent.changes_state, "");
        let present: Review =
            serde_yaml_ng::from_str("mode: ticketless\nchanges_state: Doing").expect("parse");
        assert_eq!(present.changes_state, "Doing");
    }

    /// An absent `reviewers:` key keeps the default while an explicit one is
    /// honoured — the serde default has to be the CONST, not `i64::default()`.
    #[test]
    fn review_reviewers_absent_is_the_default_and_present_is_honoured() {
        let absent: Review = serde_yaml_ng::from_str("mode: ticketless").expect("parse");
        assert_eq!(absent.reviewers, DEFAULT_REVIEW_REVIEWERS);
        let present: Review =
            serde_yaml_ng::from_str("mode: ticketless\nreviewers: 4").expect("parse");
        assert_eq!(present.reviewers, 4);
    }

    /// The Settings editor writes a `Teams` and the daemon boots the same one —
    /// the round-trip property `save_creates_the_file_and_round_trips_through_load`
    /// pins for the rest of the file, extended to the new block so a save can
    /// never silently drop the mode an operator chose.
    #[test]
    fn review_mode_round_trips_through_save_and_load() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("teams.yaml");
        let teams = Teams {
            enabled: true,
            review: Review {
                mode: ReviewMode::Ticketless,
                reviewers: 3,
                done_state: "Done".to_string(),
                changes_state: "In Progress".to_string(),
                auto_merge: true,
                model: HarnessScoped::bare("claude-opus-5"),
                effort: HarnessScoped::bare("high"),
                required: vec!["jimmy".to_string()],
                adjudicate_after_rounds: 3,
            },
            // Four, because `reviewers: 3` must be a config the ceiling accepts
            // (STUDIO-891: a roster of N satisfies at most N−1). The property
            // under test is the round-trip, and it is unchanged by the width.
            roster: ["alice", "jimmy", "jerry", "june"]
                .into_iter()
                .map(|name| Identity {
                    name: name.to_string(),
                    ..Identity::default()
                })
                .collect(),
            ..Teams::disabled()
        };

        Teams::save(&path, &teams).expect("save");
        let yaml = std::fs::read_to_string(&path).expect("read back");
        assert!(
            yaml.contains("review:\n  mode: ticketless\n"),
            "the canonical serialization must carry the block: {yaml}"
        );
        assert_eq!(Teams::load(&path), teams);
        assert!(Teams::load(&path).review_ticketless());
        assert_eq!(Teams::load(&path).review_done_state(), Some("Done"));
        assert_eq!(
            Teams::load(&path).review_changes_state(),
            Some("In Progress")
        );
        assert_eq!(
            Teams::load(&path)
                .review
                .model
                .for_harness("claude", "claude"),
            Some("claude-opus-5")
        );
        assert_eq!(
            Teams::load(&path)
                .review
                .effort
                .for_harness("claude", "claude"),
            Some("high")
        );
        assert_eq!(
            Teams::load(&path).review_adjudicate_after_rounds(),
            Some(3),
            "the opt-in adjudication threshold must survive a save/load round-trip"
        );
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
            // STUDIO-719: `tickets` and not `ticketless`, because this config
            // also enables the quorum and `validate` rejects that pair.
            review: Review {
                mode: ReviewMode::Tickets,
                ..Review::default()
            },
            // STUDIO-927: the one new block, round-tripped like the rest.
            projects: vec![TeamsProject {
                slugs: vec!["booch".to_string()],
                review: ProjectReview {
                    auto_merge: Some(false),
                },
            }],
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
