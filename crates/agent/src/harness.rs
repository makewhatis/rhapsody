//! The pluggable-harnesses contract (STUDIO-900, slice 2 of
//! `~/.rhapsody/docs/pluggable-harnesses-design.md`, §3). Rhapsody-only — no Go counterpart, since
//! the frozen reference runs exactly one backend.
//!
//! This module sits **above** the existing [`crate::Runner`]/[`crate::Session`] traits, which stay
//! unchanged (design §3: "the existing traits stay; what is added sits above and below them"):
//! [`HarnessSpec`] is what a dispatch resolved ("claude, this model, these knobs"), declared
//! [`HarnessCapabilities`] is what that harness can do, and [`Harness`] is a [`crate::Runner`] that
//! also knows its own identity and capabilities. The claude backend implements it
//! ([`crate::claude::runner::Runner`]); no second harness exists yet on purpose (design §9's
//! adapter slices 8-10 are opencode, goose, then codex, in that order).
//!
//! ## The four defects §3 names in its own contract, and what this slice does about each
//!
//! The design record ran all four target CLIs (STUDIO-869/872 spikes) and found the shape below is
//! measured-wrong in four places. Implementing §3 verbatim was explicitly called out as the most
//! likely way to get this slice wrong, so each defect gets a named resolution or a named deferral:
//!
//! 1. **`mcp` and `sandbox` are not independent fields** (codex honours one or the other, never
//!    both — a real CLI constraint `HarnessCapabilities` cannot express as two plain fields).
//!    **Fixed (STUDIO-978, slice 5)**: [`HarnessCapabilities::mcp_sandbox`] declares the coupling,
//!    and [`validate`] refuses a dispatch that requires both on a
//!    [`McpSandboxCoupling::MutuallyExclusive`] harness even when it can provide either alone.
//!    Claude's and opencode's own values for the two fields ARE independent (they honour both), so
//!    they declare [`McpSandboxCoupling::Independent`] and nothing about their behavior changes.
//! 2. **A single declared `FailureSignal` value is too coarse** (codex needs three-way
//!    discrimination between two non-terminal `error` shapes and a terminal `turn.failed`;
//!    classification has to be a per-adapter *function*, not a value). **Resolved by omission**:
//!    `HarnessCapabilities` deliberately carries no `failure` field. The function the design asks
//!    for already exists, structurally, as each adapter's own [`crate::Session::run_turn`]
//!    implementation — its `(TurnResult, Option<AgentError>)` return is the normalized answer to
//!    "did this turn succeed," computed by code that knows the harness (Claude's lives in
//!    `crate::claude::runner`, keyed on `is_error`/`api_error_status`, never `subtype`). A capability
//!    *value* claiming to summarize that function is exactly the wrong shape being fixed; adding one
//!    back — even a richer one — would reintroduce the defect this fix removes.
//! 3. **`Resume` assumed continuation differs only by flags.** Codex resumes via a subcommand with a
//!    positional id (`codex exec resume <thread_id>`), not a flag `claude`/`opencode` share
//!    (`--resume <id>` / `-s <id>`). **Fully fixed (STUDIO-978, slice 5)**: [`Resume`] carries
//!    `Flags`, `Subcommand` AND `Protocol` (goose over ACP's `session/load`), so all three measured
//!    continuation shapes are representable.
//! 4. **`EventFidelity::Structured { tool_level: bool }` couldn't separate opencode from codex**
//!    (opencode emits typed `read`/`edit`/`bash` events; codex has no file-level events at all — it
//!    shells `cat`/`printf`, so everything is `command_execution`). **Fixed**: `tool_level` is now
//!    [`ToolEventGranularity`] (`FileLevel` | `CommandOnly`) instead of a `bool`.
//!
//! Also named because the design flags it as unmodelled and "it will bite": **child stdin**. Claude
//! requires it held open as the operator-message mailbox (INF-250); codex hangs forever if it is
//! (§7.2). **Fixed**: [`HarnessCapabilities::stdin`] ([`StdinPolicy`]) declares which a harness
//! needs, and [`crate::Runner::start_session`]'s own doc comment now states that the requirement is
//! per-harness, not a shared assumption.
//!
//! ## STUDIO-872 (goose over ACP) widened the list after §3 and STUDIO-869 were both written
//!
//! STUDIO-872's spike (**[RAN]**, 2026-09-12) found goose defeats two more shapes that the four
//! defects above do not cover, because that spike ran after STUDIO-869 named them. Recorded here so
//! "Fixed" above is not read as the full story:
//!
//! - **`Resume` still doesn't fit, even with `Subcommand`.** goose resumes via `session/load
//!   {sessionId, cwd, mcpServers}` — a *protocol method* over the SAME argv (`goose acp` on every
//!   turn), not a subcommand and not a flag. STUDIO-872 §7 concludes codex and goose together
//!   "retire `SameFlags | Narrowed | None` entirely," and this slice's fix only carries the codex
//!   half of that. **Deferred to slice 9** — a protocol-call case belongs with the goose adapter
//!   that can validate it, not guessed at here with nothing to test it against. Separately, and for
//!   the same "nothing to validate it against" reason, §3's own `Resume::Narrowed { drops }` variant
//!   is dropped rather than carried forward: no measured harness (STUDIO-869 or STUDIO-872)
//!   exercises a resume that narrows scope.
//! - **[`Resume`] now carries goose's protocol-call shape too** (STUDIO-978): the `Protocol` variant
//!   was added to the shape without a goose adapter to exercise it, which is the one measured shape
//!   a future adapter no longer has to widen this enum for.
//! - **[`ToolNaming`]'s three variants have no case for goose.** goose's real tool name lives in a
//!   vendor `_meta.goose.toolCall.toolName` field; ACP's own standard `title` field is lossy human
//!   prose — for the daemon's own `symphony_state` tool it renders as `"symphony: symphony state"`,
//!   not `symphony__symphony_state`, so a prompt template keyed on the standard field cannot address
//!   it. **Deferred to slice 9** for the same reason as `Resume` above — a fourth variant added now,
//!   with no adapter to exercise it, would be a guess about ACP's shape rather than a measured one.
//!
//! ## What this slice deliberately does NOT do
//!
//! - **No second harness.** [`HarnessId`] had exactly one variant when this slice landed. Every
//!   later variant is a real, reviewed change to every `match` on it — that is a feature of a
//!   closed enum, not a gap to paper over with a wildcard arm. ⚠️ **STUDIO-902 has since added
//!   [`HarnessId::Opencode`]** (`crate::opencode`), ahead of the slice order, to move
//!   implementation onto a different billing pool; it went in as the real, reviewed change this
//!   paragraph describes rather than through a wildcard, and it left claude's behaviour untouched.
//! - **The runner is still built at effective-build time**, not at dispatch (design §4.1's "largest
//!   structural edit"). [`crate::Session::set_model_override`]'s doc comment already states why:
//!   Teams picks the identity at dispatch, but *swapping a whole harness* is slice 4's job, not
//!   this one's. `HarnessSpec` exists as a type here so slice 4 has somewhere to put the resolved
//!   choice; nothing in this crate constructs one anywhere but effective-build time yet.
//! - **`session_uuid` is untouched.** The open product call from STUDIO-872 §7.2 (whether a
//!   per-run-isolated session id can still serve as an identity key) is store/runs-schema territory
//!   (design §6.2, slice 3), and no type in this module carries or interprets one. Nothing here
//!   assumes a resolution either way.

use std::fmt;

use crate::Runner;

/// Which harness a resolved [`HarnessSpec`] names. Two variants today, of design D4's four
/// (claude, codex, goose, opencode; `pi` was dropped for having no MCP client) — adding the next
/// one is a real, reviewed change to every `match` on this type, not a default arm silently
/// absorbing it. STUDIO-902 added [`HarnessId::Opencode`] exactly that way: this slice's module doc
/// said a second variant would be a real, reviewed change, and it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HarnessId {
    Claude,
    Opencode,
}

impl HarnessId {
    /// The harness's config-facing name (`"claude"`/`"opencode"`), the same spelling
    /// [`harness_id_for_name`] parses. Exhaustive, so a new harness must add its name rather than
    /// inherit one.
    pub const fn name(self) -> &'static str {
        match self {
            HarnessId::Claude => "claude",
            HarnessId::Opencode => "opencode",
        }
    }
}

/// A provider protocol a harness adapter can consume. One variant in v1: OpenAI Chat Completions
/// with Bearer API-key auth — the reviewed adapter `provider-auth-design.md` §3 means by the config
/// value `openai-compatible`, NOT arbitrary auth headers or fields.
///
/// The `as_str`/`adapter_id` pair is a cross-surface contract: `as_str` matches the YAML protocol
/// name config validates, and `adapter_id` is the identity half of every credential binding. The
/// agreement with `rhapsody-config`'s constants is pinned by
/// [`tests::protocol_and_adapter_names_are_pinned_to_the_config_crate`], so a rename on either side
/// reds a test rather than silently drifting the two crates apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderProtocol {
    /// `openai-compatible`: Chat Completions + Bearer API-key auth.
    OpenAiCompatible,
}

impl ProviderProtocol {
    /// The config-facing protocol name (`providers.<id>.protocol`).
    pub const fn as_str(self) -> &'static str {
        "openai-compatible"
    }

    /// The reviewed adapter identity, part of the canonical credential binding
    /// `(provider_id, adapter, base_url)`. Must equal `rhapsody_config`'s
    /// `ADAPTER_OPENAI_CHAT_COMPLETIONS_BEARER_V1`.
    pub const fn adapter_id(self) -> &'static str {
        "openai-chat-completions-bearer-v1"
    }

    /// The protocol for a config-facing name, or `None` when unknown. The one parser; callers must
    /// not re-spell the values.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "openai-compatible" => Some(Self::OpenAiCompatible),
            _ => None,
        }
    }
}

/// The pure, non-secret validated broker limits carried by a [`ResolvedProviderPlan`]
/// (`provider-broker-design.md` §3.1's `limits`).
///
/// The config-side `rhapsody_config::BrokerLimits` is the authoritative definition; this crate must
/// not depend on `rhapsody-config` at runtime, so the fields are mirrored here and a cross-crate pin
/// test asserts the defaults agree field-for-field. PB5 lowers a plan's `limits` into the broker's
/// own `BrokerLimits`/`BrokerRegistrationPlan`; keeping the block on the plan means PB5 needs no
/// second input and cannot re-derive (or drift from) the config defaults.
///
/// A raw reusable key is unrepresentable: every field is a number or `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderLimits {
    pub forwarded_requests_per_turn: u32,
    pub denied_requests_before_revocation: u32,
    pub concurrent_upstream_requests_per_turn: u32,
    pub json_request_bytes: u64,
    pub aggregate_request_bytes_per_turn: u64,
    pub response_bytes_per_request: u64,
    pub aggregate_response_bytes_per_turn: u64,
    pub requested_output_tokens_per_request: u64,
    pub reserved_token_units_per_turn: u64,
    pub reserved_token_units_per_session: u64,
    pub capability_lifetime_ms: u64,
    /// Optional durable UTC-day cap; `None` means no Rhapsody daily cap.
    pub max_reserved_token_units_per_utc_day: Option<u64>,
}

impl Default for ProviderLimits {
    /// The V1 default column (`provider-broker-design.md` §8.1) with no daily cap. Kept identical to
    /// `rhapsody_config::BrokerLimits::default()` by `provider_limits_agree_with_the_config_crate`.
    fn default() -> Self {
        Self {
            forwarded_requests_per_turn: 64,
            denied_requests_before_revocation: 16,
            concurrent_upstream_requests_per_turn: 4,
            json_request_bytes: 8 * 1024 * 1024,
            aggregate_request_bytes_per_turn: 32 * 1024 * 1024,
            response_bytes_per_request: 16 * 1024 * 1024,
            aggregate_response_bytes_per_turn: 64 * 1024 * 1024,
            requested_output_tokens_per_request: 32_000,
            reserved_token_units_per_turn: 1_000_000,
            reserved_token_units_per_session: 20_000_000,
            capability_lifetime_ms: 3_600_000,
            max_reserved_token_units_per_utc_day: None,
        }
    }
}

/// The pure, non-secret result of selecting and normalizing a provider for one dispatch
/// (`provider-auth-design.md` §3's `ResolvedProviderPlan`; STUDIO-984 owns this shape, PB5 owns
/// `PreparedProvider`).
///
/// **A raw reusable API key is unrepresentable here** — there is no value/token/key field, and none
/// may be added. The plan carries only stable metadata, the normalized endpoint, the canonical
/// credential *binding identity*, a credential *source kind*, the validated broker limits, and the
/// model/provider/origin inputs P4/PB5 need. `credential_binding`/`credential_ref` are non-secret
/// identifiers: the binding names WHICH credential to read, never the credential itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedProviderPlan {
    /// The canonical operator-chosen provider id (the cross-surface identifier contract).
    pub stable_id: String,
    pub protocol: ProviderProtocol,
    /// The normalized protocol root immediately above `/chat/completions`.
    pub normalized_endpoint: String,
    /// Operator policy reaching the plan as policy, never child-controlled input.
    pub allow_insecure_http: bool,
    /// The canonical `(provider_id, adapter, base_url)` binding identity. Non-secret; names which
    /// credential to read. Empty when no binding could be derived.
    pub credential_binding: String,
    /// The credential *source kind* (e.g. `keychain`). Never an account and never a value.
    pub credential_ref: String,
    /// The validated broker limits PB5 lowers into the broker's registration plan.
    pub limits: ProviderLimits,
    /// The exact model selection, preserved for P4/PB5.
    pub model: String,
    /// Where each field came from, preserved for P4.
    pub origins: ProviderOrigins,
}

/// The origin of each provider-tuple field (which selection tier supplied it),
/// `provider-auth-design.md` §2.3's `origins` half. Values are surface names (`"ticket"`,
/// `"profile"`, `"project"`, `"global"`, …), non-secret and for diagnostics/provenance.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProviderOrigins {
    pub provider: String,
    pub model: String,
}

/// The per-harness opaque knob block (design §4.2: "a normalized core plus an opaque per-harness
/// knob block," because the knobs do not line up across CLIs). One variant per [`HarnessId`] — the
/// claude arm carries the existing, untouched [`crate::claude::Config`], so wrapping a `Config` in
/// this enum changes nothing about what reaches [`crate::claude::runner::Runner`].
///
/// The two knob blocks demonstrate why §4.2 asked for an opaque block rather than one shared
/// struct: they overlap on `command`/`model`/`extra_args` and agree on nothing else. opencode has
/// no permission MODE (it has one `--auto` approval boolean), no tool allowlist, and no
/// `mcp_config` path — but it does have a `--variant` reasoning knob and a per-run state directory,
/// neither of which claude has any use for.
#[derive(Debug, Clone)]
pub enum HarnessKnobs {
    Claude(crate::claude::Config),
    Opencode(crate::opencode::Config),
}

/// What was asked for, fully resolved (design §3): the harness, its model, an optional non-default
/// provider plan, and its per-harness knobs. Constructed today only at effective-build time (see the
/// module doc's "what this slice does not do") from the installation's static config; `model` and
/// `provider` are `None` at every call site that exists today because Claude's equivalent values
/// already live inside its own `knobs` block, not because the fields are unused in principle —
/// slice 4's dispatch-time resolution chain is what will populate them independently of a
/// per-harness knob block (e.g. a teammate profile naming a model, STUDIO-868, already does this
/// on `Session` directly, ahead of and independent of this type).
///
/// `provider` is a [`ResolvedProviderPlan`] — pure and non-secret. A prepared, move-only provider
/// (PB5's `PreparedProvider`, carrying an opaque broker session) is what a live dispatch will carry;
/// this slice deliberately does not add it. A raw reusable key is unrepresentable in both.
#[derive(Debug, Clone)]
pub struct HarnessSpec {
    pub harness: HarnessId,
    pub model: Option<String>,
    pub provider: Option<ResolvedProviderPlan>,
    pub knobs: HarnessKnobs,
}

/// Event stream fidelity a harness can report (design §3, fixed per defect 4). `FinalTextOnly`
/// harnesses are still dispatchable (design §5.1) — a degraded Trace console, never a refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventFidelity {
    FinalTextOnly,
    Structured { tool_level: ToolEventGranularity },
}

/// How finely a harness's structured events name a tool call. `FileLevel` distinguishes an edit
/// from a read from a shell command (Claude's `tool_use` blocks, opencode's typed `read`/`edit`/
/// `bash` events); `CommandOnly` cannot, because the harness reads and edits files by shelling
/// `cat`/`printf` and reports one undifferentiated `command_execution` (codex, per the STUDIO-869
/// spike). Splitting this out of `EventFidelity::Structured` is defect 4's fix: the two granularities
/// used to share one `bool`, which could not tell opencode and codex apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolEventGranularity {
    FileLevel,
    CommandOnly,
}

/// Whether an operator can steer a live turn (design D7). Where this is `None` the console hides
/// the steering affordance entirely — never a control that silently drops what it's given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Steering {
    Live,
    BetweenTurns,
    None,
}

/// How a harness continues a prior conversation (design §3, defect 3; completed by STUDIO-978).
///
/// The design's original `SameFlags | Narrowed | None` assumed continuation differs only in flags.
/// Three measured shapes disagree, and this enum names all three (the ticket's "resume must
/// represent flag, positional-subcommand, and protocol-call continuation"):
///
/// * `Flags` — claude (`--resume <id>`) and opencode (`-s <id>`): an unchanged flag added to an
///   otherwise-normal invocation.
/// * `Subcommand` — codex (`codex exec resume <thread_id> …`): a different positional-argument
///   invocation shape, not an extra flag on the same one (STUDIO-869).
/// * `Protocol` — goose over ACP (`session/load {sessionId, cwd, mcpServers}`): a *protocol method*
///   over the SAME argv on every turn, neither a flag nor a subcommand (STUDIO-872). STUDIO-900
///   deferred this variant because no adapter could validate it; no goose adapter exists yet, but
///   the measured shape must be representable so slice 9 does not have to widen this enum under
///   pressure. Declaring `Protocol` here does NOT implement goose — it only stops the contract
///   collapsing the third shape.
///
/// The design's `Narrowed { drops }` variant stays dropped: no measured harness exercises a resume
/// that narrows scope (STUDIO-869/872), so carrying it would be a guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resume {
    None,
    Flags,
    Subcommand,
    Protocol,
}

/// Whether [`HarnessCapabilities::mcp`] and [`HarnessCapabilities::sandbox`] can be honored
/// TOGETHER. This is the contract's answer to the STUDIO-869 finding that the two are not
/// independent fields for every harness: codex can honour a sandbox or an MCP server, never both,
/// and fails by reporting `turn.completed` after refusing every tool call. A pair of plain booleans
/// could not express that, so §5's refuse-if-`mcp`-absent rule could not be evaluated correctly for
/// codex. `MutuallyExclusive` lets the validator refuse a dispatch that requires both even when the
/// harness can provide either one alone. The shipped claude and opencode adapters honour both
/// simultaneously and declare [`McpSandboxCoupling::Independent`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpSandboxCoupling {
    /// Both may be required at once.
    Independent,
    /// At most one of `mcp` / `sandbox` may be required; a requirement for both is refused.
    MutuallyExclusive,
}

/// How a harness constrains what its tool calls can touch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sandbox {
    Modes,
    ToolAllowlist,
    None,
}

/// What usage a harness reports back (design D8: dollar caps where cost is reported, token caps
/// otherwise).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageDetail {
    None,
    Tokens,
    TokensAndCost,
}

/// How a harness spells an injected MCP tool name in its own event/tool-call vocabulary (design §3:
/// "three harnesses, three spellings," [RAN] against real CLIs). Prompt text naming a tool must be
/// templated per this value, or an agent is instructed to call a tool name that does not exist.
/// Covers claude/opencode/codex only — goose's `_meta.goose.toolCall.toolName` spelling has no case
/// here yet (module doc's STUDIO-872 section).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolNaming {
    /// `mcp__<server>__<tool>` (claude).
    McpDoubleUnderscore,
    /// `<server>_<tool>` (opencode).
    ServerUnderscoreTool,
    /// Server and tool arrive as separate event fields rather than one flattened name (codex).
    SeparateFields,
}

/// Whether a harness's child process needs stdin held open or closed (design §3's "also
/// unmodelled" callout, §7.2). Claude requires it held open as the INF-250 operator-message
/// mailbox; codex hangs forever if it is. There is no shared default — every adapter must declare
/// one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StdinPolicy {
    HeldOpen,
    ClosedAtStart,
}

/// What a harness can do, declared and read BEFORE spawning it (design §3: "declare capabilities,
/// refuse what cannot be honored," D3). Enforcement against these (the refusal path, §5) is slice
/// 5's job, not this one's — this slice only makes the shape able to describe what the STUDIO-869/
/// 872 spikes actually measured.
///
/// Deliberately has **no `failure` field** — see the module doc's defect 2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HarnessCapabilities {
    pub events: EventFidelity,
    pub steering: Steering,
    pub resume: Resume,
    /// Whether this harness can reach the daemon's own MCP tools at all. For codex this is not
    /// independent of [`Self::sandbox`] — the CLI honours a sandbox or an MCP server, never both
    /// (defect 1) — which is why [`Self::mcp_sandbox`] must be consulted alongside this field. A
    /// harness that offers MCP but couples it to the sandbox sets this `true` AND declares
    /// [`McpSandboxCoupling::MutuallyExclusive`].
    pub mcp: bool,
    /// See [`Self::mcp`] — the other half of the coupled pair.
    pub sandbox: Sandbox,
    /// Whether [`Self::mcp`] and [`Self::sandbox`] may both be honored at once (defect 1's fix).
    /// Independent booleans cannot say "either one, never both"; this field can, and
    /// [`validate`] consults it so a dispatch requiring both is refused rather than half-honored.
    pub mcp_sandbox: McpSandboxCoupling,
    pub usage: UsageDetail,
    /// Whether the harness's own CLI enforces its own wall-clock/tool-call limits, independent of
    /// the daemon's timeout (design §7.4).
    pub budgets: bool,
    pub tool_naming: ToolNaming,
    pub stdin: StdinPolicy,
}

/// A [`Runner`] that also declares its own identity and capabilities (design §3). The claude
/// backend is the one implementation today (`crate::claude::runner::Runner`); see the module doc
/// for why a second one is deliberately not introduced by this slice.
pub trait Harness: Runner {
    fn id(&self) -> HarnessId;
    fn capabilities(&self) -> &HarnessCapabilities;

    /// Start a BROKERED session for an explicit-provider dispatch (PB7, STUDIO-1002): the session's
    /// turns run through [`crate::Session::run_turn_brokered`] against the broker loopback rather
    /// than a harness-native login. `None` (the default) refuses with a typed, non-secret reason
    /// rather than starting a legacy session — a prepared dispatch must never silently fall back to
    /// the native-login path. `crate::opencode::Runner` overrides this with its brokered
    /// materialization; Claude has no provider adapter in v1, so a prepared spec for it is refused
    /// earlier as an unsupported protocol.
    fn start_brokered_session(
        &self,
        _workspace_path: &str,
        _issue: rhapsody_core::Issue,
        _transcript: Option<crate::Transcript>,
    ) -> Result<Box<dyn crate::Session>, crate::AgentError> {
        Err(crate::AgentError::Other(format!(
            "brokered_start_unsupported: harness {:?} has no brokered session",
            self.id()
        )))
    }
}

/// What one dispatch NEEDS from its resolved harness (design §5's table). This is the PURE input to
/// [`validate`]: it is computed from the work and the installation, never from the harness, so the
/// validator can answer "can this harness do this work?" before anything is spawned.
///
/// Every field is a *requirement*, and the split between refusing and degrading is the design's
/// §5.1 line: `team_tools`/`multi_turn`/`sandbox` change whether the work is done correctly (a
/// missing one refuses the dispatch); `trace_console`/`steering` change only what an operator can
/// see or do (a missing one degrades visibly).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WorkRequirements {
    /// The agent must reach the daemon's own MCP tools (room, memory, handoff). Refused when absent
    /// because a run that cannot reach the team's tools produces work that looks finished and is not.
    pub team_tools: bool,
    /// The work may run more than one turn, so the harness must continue a prior conversation.
    /// Refused when absent because turn 2 would start cold (design §5.1).
    pub multi_turn: bool,
    /// The work must run under a declared sandbox. Refused when absent — and, together with
    /// [`Self::team_tools`], subject to the [`McpSandboxCoupling`] rule.
    pub sandbox: bool,
    /// The operator console wants the structured Trace spine. Degrades visibly, never refuses.
    pub trace_console: bool,
    /// The operator wants to steer a live turn. Where the harness cannot, the console hides the
    /// field (D7) rather than offering a control that silently drops what it is given.
    pub steering: bool,
}

/// A dispatch-time REFUSAL: the resolved harness cannot honor a correctness requirement, or the
/// resolved harness is not one this build can run (design §5.1, ticket's "Known-but-unimplemented
/// and unknown harnesses remain typed refusals"). Typed rather than a formatted string so every
/// caller can name exactly what was refused, and so a refusal is never a silent downgrade.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapabilityRefusal {
    /// `team_tools` was required but the harness cannot reach the daemon's MCP tools.
    McpUnavailable { harness: HarnessId },
    /// `multi_turn` was required but the harness declares [`Resume::None`].
    ResumeUnavailable { harness: HarnessId },
    /// `sandbox` was required but the harness declares [`Sandbox::None`].
    SandboxUnavailable { harness: HarnessId },
    /// Both `team_tools` and `sandbox` were required, but the harness declares
    /// [`McpSandboxCoupling::MutuallyExclusive`] — it can honour one or the other, never both.
    McpSandboxMutuallyExclusive { harness: HarnessId },
    /// A profile named a harness this build has no runner for (known-but-unimplemented, e.g. codex,
    /// or unknown). A typed refusal — NEVER a fall back to `agent.backend`.
    HarnessNotImplemented { name: String },
}

impl fmt::Display for CapabilityRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::McpUnavailable { harness } => write!(
                f,
                "harness {harness:?} cannot reach the daemon's MCP tools, but this run requires \
                 team participation (room, memory, handoff)"
            ),
            Self::ResumeUnavailable { harness } => write!(
                f,
                "harness {harness:?} cannot resume a prior turn, but this run may take more than \
                 one turn; turn 2 would start cold"
            ),
            Self::SandboxUnavailable { harness } => write!(
                f,
                "harness {harness:?} declares no sandbox, but this run requires one"
            ),
            Self::McpSandboxMutuallyExclusive { harness } => write!(
                f,
                "harness {harness:?} can honour MCP or a sandbox, never both, but this run requires \
                 both"
            ),
            Self::HarnessNotImplemented { name } => write!(
                f,
                "harness {name:?} is not implemented by this build; refusing rather than falling \
                 back to another harness"
            ),
        }
    }
}

/// An observability-only loss the operator should see stated where it shows (design §5.1). These
/// never refuse a dispatch; a `FinalTextOnly` harness is dispatchable and its run detail reports the
/// reduced fidelity instead of rendering a blank Trace spine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Degradation {
    /// The harness emits no structured per-step events, so the Trace spine cannot be built.
    NoStructuredEvents { harness: HarnessId },
    /// The harness cannot be steered, so the console hides the steering field (D7).
    SteeringHidden { harness: HarnessId },
}

impl fmt::Display for Degradation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoStructuredEvents { harness } => write!(
                f,
                "harness {harness:?} emits only final text; the trace shows the run result, not \
                 per-step events"
            ),
            Self::SteeringHidden { harness } => write!(
                f,
                "harness {harness:?} cannot be steered; the steering field is hidden"
            ),
        }
    }
}

/// The outcome of a successful capability check: the run is dispatchable, and any observability-only
/// losses are stated rather than silently absorbed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DispatchVerdict {
    pub degradations: Vec<Degradation>,
}

impl DispatchVerdict {
    /// Whether the operator loses any observability the console should state.
    pub fn is_degraded(&self) -> bool {
        !self.degradations.is_empty()
    }
}

/// The PURE capability/requirement validator (design §5, ticket's "one pure capability/requirement
/// validator, one typed refusal contract"). It takes the DECLARED capabilities and the computed
/// requirements and returns either the degradations to state or the refusal that stops the dispatch
/// — it performs no I/O, spawns nothing, and can be called before any workspace exists.
///
/// The order is deliberate and mirrors §5.1: every CORRECTNESS requirement is checked first, so a
/// refusal names a capability that would have changed whether the work is done correctly, ahead of
/// any observability loss. `mcp`-and-`sandbox` is checked as a COUPLED requirement after the
/// individual fields, so a harness that provides neither is refused for the field it lacks rather
/// than for the coupling.
pub fn validate(
    harness: HarnessId,
    capabilities: &HarnessCapabilities,
    needs: &WorkRequirements,
) -> Result<DispatchVerdict, CapabilityRefusal> {
    if needs.team_tools && !capabilities.mcp {
        return Err(CapabilityRefusal::McpUnavailable { harness });
    }
    if needs.multi_turn && capabilities.resume == Resume::None {
        return Err(CapabilityRefusal::ResumeUnavailable { harness });
    }
    if needs.sandbox && capabilities.sandbox == Sandbox::None {
        return Err(CapabilityRefusal::SandboxUnavailable { harness });
    }
    // The coupled case (design §3 defect 1): both are individually available, but the harness can
    // honour only one at a time. Refuse rather than half-honor.
    if needs.team_tools
        && needs.sandbox
        && capabilities.mcp_sandbox == McpSandboxCoupling::MutuallyExclusive
    {
        return Err(CapabilityRefusal::McpSandboxMutuallyExclusive { harness });
    }

    let mut degradations = Vec::new();
    if needs.trace_console && !matches!(capabilities.events, EventFidelity::Structured { .. }) {
        degradations.push(Degradation::NoStructuredEvents { harness });
    }
    if needs.steering && capabilities.steering == Steering::None {
        degradations.push(Degradation::SteeringHidden { harness });
    }
    Ok(DispatchVerdict { degradations })
}

/// The [`HarnessId`] a recorded or configured name denotes, when this build implements it. `None`
/// for a known-but-unimplemented name (`codex`, `goose`) and for anything unknown — the same
/// boundary that makes those a [`CapabilityRefusal::HarnessNotImplemented`] rather than a fall back.
pub fn harness_id_for_name(name: &str) -> Option<HarnessId> {
    match name {
        "claude" => Some(HarnessId::Claude),
        "opencode" => Some(HarnessId::Opencode),
        _ => None,
    }
}

/// The declared capabilities of an implemented harness, addressed by id rather than through a live
/// [`Harness`] object. For a reader that has only the harness NAME a run recorded (the console's run
/// provenance, for one) and must render its fidelity honestly, without constructing a runner.
///
/// SINGLE-SOURCED: each arm returns the adapter's own `CAPABILITIES` constant, so a reader here and
/// a validator reading `Harness::capabilities()` can never disagree. A test in each adapter pins
/// that equality (`declared_capabilities(id) == *Runner::new(...).capabilities()`), which is what
/// keeps this function honest rather than a second declaration to drift.
pub fn declared_capabilities(id: HarnessId) -> HarnessCapabilities {
    match id {
        HarnessId::Claude => crate::claude::runner::CAPABILITIES,
        HarnessId::Opencode => crate::opencode::runner::CAPABILITIES,
    }
}

// ---------------------------------------------------------------------------
// The harness registry (provider-protocol compatibility, single-sourced)
// ---------------------------------------------------------------------------

/// How a harness materializes a provider credential (`provider-auth-design.md` §3's compatibility
/// table). A brokered harness receives a per-turn bounded capability over a loopback URL; it NEVER
/// receives the reusable upstream key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialTransport {
    /// The CLI's own login (Claude today). No explicit Rhapsody provider in v1.
    NativeLogin,
    /// A loopback broker URL plus a per-turn bounded capability (OpenCode in v1).
    BrokeredLoopback,
}

/// One harness's declared provider compatibility: which protocols its adapter can consume and how it
/// materializes credentials. This is the source of truth for provider compatibility
/// (`provider-auth-design.md` §3: "It is not acceptable to add a second provider compatibility switch
/// in config"). `rhapsody-config` cannot depend on this crate (layering), so it declares the same
/// accepted-backend subset in its `PROVIDER_HARNESS_BACKENDS` constant and the cross-crate pin test
/// [`config_provider_policy_agrees_with_the_harness_registry`] asserts the two cannot disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HarnessRegistryEntry {
    pub id: HarnessId,
    /// The provider protocols this harness's adapter supports. EMPTY means "no explicit Rhapsody
    /// provider in v1" — the legacy/native-login branch.
    pub protocols: &'static [ProviderProtocol],
    pub credential_transport: CredentialTransport,
}

/// The harness registry, in the design's compatibility-table order. V1 deliberately enables only the
/// measured OpenCode row: OpenCode may consume `openai-compatible`; Claude has no provider adapter
/// yet. Goose and Codex are absent because no adapter exists — adding one is a real, reviewed change
/// to this table (and to every `match` on [`HarnessId`]).
pub const HARNESS_REGISTRY: &[HarnessRegistryEntry] = &[
    HarnessRegistryEntry {
        id: HarnessId::Claude,
        protocols: &[],
        credential_transport: CredentialTransport::NativeLogin,
    },
    HarnessRegistryEntry {
        id: HarnessId::Opencode,
        protocols: &[ProviderProtocol::OpenAiCompatible],
        credential_transport: CredentialTransport::BrokeredLoopback,
    },
];

/// Whether `id`'s adapter can consume `protocol`. Single-sourced from [`HARNESS_REGISTRY`], so a
/// resolver and a validator cannot disagree.
pub fn harness_supports_protocol(id: HarnessId, protocol: ProviderProtocol) -> bool {
    HARNESS_REGISTRY
        .iter()
        .any(|e| e.id == id && e.protocols.contains(&protocol))
}

/// The credential transport for `id`. Falls back to [`CredentialTransport::NativeLogin`] for a
/// harness with no registry row — the conservative default (an unknown harness gets no brokered
/// credential). `registry_covers_every_harness_id` pins that every current id HAS a row.
pub fn credential_transport(id: HarnessId) -> CredentialTransport {
    HARNESS_REGISTRY
        .iter()
        .find(|e| e.id == id)
        .map(|e| e.credential_transport)
        .unwrap_or(CredentialTransport::NativeLogin)
}

// ---------------------------------------------------------------------------
// Brokered OpenCode compatibility (fail-closed, version-gated)
// ---------------------------------------------------------------------------

/// The supported managed-OpenCode compatibility table. SINGLE-SOURCED from the PB0 probe
/// (`crate::opencode::probe::SUPPORTED`) so a dispatch-time compatibility check and the probe cannot
/// disagree. Fail-closed: an unknown or unmeasured version refuses rather than assuming a configured
/// `opencode` binary honors the pinned controls (`provider-auth-design.md` §3, `provider-broker-design.md`
/// §9.1).
pub const SUPPORTED_OPENCODE_VERSIONS: &[crate::opencode::probe::CompatibilityRow] =
    crate::opencode::probe::SUPPORTED;

/// A typed, pre-credential refusal for a brokered OpenCode control the initial compatibility row
/// does not accept. These run BEFORE any credential read, because the managed controls and closed
/// request schema are version-sensitive (`provider-broker-design.md` §9.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrokeredOpenCodeRefusal {
    /// The configured `opencode` version is outside [`SUPPORTED_OPENCODE_VERSIONS`].
    UnsupportedVersion { found: String },
    /// `opencode.agent` is neither empty nor the pinned `build` agent.
    AgentUnsupported { agent: String },
    /// `opencode.variant` (its reasoning-effort knob) is non-empty.
    VariantUnsupported { variant: String },
    /// `opencode.auto_approve` is explicitly `false`; brokered v1 always emits `--auto`.
    AutoApprovalDisabled,
    /// `opencode.extra_args` is non-empty; brokered v1 pins the argv rather than maintaining a
    /// security deny-list over one CLI release's aliases.
    ExtraArgsUnsupported { count: usize },
    /// `opencode.command` is not exactly one executable (embedded arguments / shell fragments).
    CommandHasEmbeddedArgs,
}

impl fmt::Display for BrokeredOpenCodeRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedVersion { found } => write!(
                f,
                "opencode version {found:?} is not a supported managed version; refusing before any \
                 credential read"
            ),
            Self::AgentUnsupported { agent } => write!(
                f,
                "opencode.agent {agent:?} is unsupported for brokered mode (only \"build\" or empty \
                 is accepted until fixture-backed)"
            ),
            Self::VariantUnsupported { variant } => write!(
                f,
                "opencode.variant {variant:?} is unsupported for brokered mode (only empty is \
                 accepted until fixture-backed)"
            ),
            Self::AutoApprovalDisabled => write!(
                f,
                "opencode.auto_approve: false is unsupported for brokered mode (brokered turns are \
                 always unattended)"
            ),
            Self::ExtraArgsUnsupported { count } => write!(
                f,
                "opencode.extra_args has {count} entries; brokered mode requires an empty extra_args"
            ),
            Self::CommandHasEmbeddedArgs => write!(
                f,
                "opencode.command must be exactly one executable with no embedded arguments for \
                 brokered mode"
            ),
        }
    }
}

/// Resolves the compatibility row for an exact reported OpenCode version, or a typed refusal. A near
/// miss is not compatibility evidence.
pub fn brokered_opencode_version_row(
    version: &str,
) -> Result<&'static crate::opencode::probe::CompatibilityRow, BrokeredOpenCodeRefusal> {
    SUPPORTED_OPENCODE_VERSIONS
        .iter()
        .find(|row| row.opencode_version == version)
        .ok_or_else(|| BrokeredOpenCodeRefusal::UnsupportedVersion {
            found: version.to_string(),
        })
}

/// Checks the OpenCode knobs the initial brokered compatibility row accepts
/// (`provider-broker-design.md` §9.2): only an empty or `build` agent, an empty variant/effort, an
/// `auto_approve` that is absent or `true`, no `extra_args`, and a `command` that is exactly one
/// executable. Everything through here is credential-free and must run BEFORE the credential owner
/// is contacted.
pub fn check_brokered_opencode_controls(
    agent: &str,
    variant: &str,
    auto_approve: Option<bool>,
    extra_args: &[String],
    command: &str,
) -> Result<(), BrokeredOpenCodeRefusal> {
    if !command_is_single_executable(command) {
        return Err(BrokeredOpenCodeRefusal::CommandHasEmbeddedArgs);
    }
    if !variant.is_empty() {
        return Err(BrokeredOpenCodeRefusal::VariantUnsupported {
            variant: variant.to_string(),
        });
    }
    if !(agent.is_empty() || agent == "build") {
        return Err(BrokeredOpenCodeRefusal::AgentUnsupported {
            agent: agent.to_string(),
        });
    }
    if auto_approve == Some(false) {
        return Err(BrokeredOpenCodeRefusal::AutoApprovalDisabled);
    }
    if !extra_args.is_empty() {
        return Err(BrokeredOpenCodeRefusal::ExtraArgsUnsupported {
            count: extra_args.len(),
        });
    }
    Ok(())
}

/// Whether `command` is exactly one executable: non-empty and lacking any whitespace. `env …
/// opencode`, `npx opencode`, and shell fragments all contain whitespace and are refused; a directly
/// executable wrapper path is allowed (its version is what the probe gates).
fn command_is_single_executable(command: &str) -> bool {
    !command.is_empty() && !command.chars().any(char::is_whitespace)
}

#[cfg(test)]
mod tests {
    use super::*;

    // A full `HarnessSpec`'s `{:?}` reaches the one credential still in the type — `HarnessKnobs::
    // Claude`'s `tracker_api_key` — and it must never appear raw in a `tracing::debug!(?spec)` line.
    // The provider half of the spec is now a `ResolvedProviderPlan`, which carries no secret at all,
    // so this test also pins that the plan's Debug output stays non-secret.
    #[test]
    fn harness_spec_debug_redacts_every_credential_it_carries() {
        let spec = HarnessSpec {
            harness: HarnessId::Opencode,
            model: Some("accounts/fireworks/models/deepseek-v4p1-flash".to_string()),
            provider: Some(resolved_plan()),
            knobs: HarnessKnobs::Claude(crate::claude::Config {
                tracker_api_key: "lin-tracker-secret".to_string(),
                ..Default::default()
            }),
        };
        let rendered = format!("{spec:?}");
        assert!(
            !rendered.contains("lin-tracker-secret"),
            "Debug output leaked the tracker key: {rendered}"
        );
        // The plan's own Debug shape is pinned so ADDING a field to it — the mutation the ticket
        // names ("add a secret field to … ResolvedProviderPlan") — changes this string and reds the
        // test. No secret-bearing field exists today, and this is the tripwire for one appearing.
        let plan = resolved_plan();
        assert_eq!(
            format!("{plan:?}"),
            "ResolvedProviderPlan { stable_id: \"fireworks\", protocol: OpenAiCompatible, \
             normalized_endpoint: \"https://api.example/v1\", allow_insecure_http: false, \
             credential_binding: \"fireworks\\u{1f}openai-chat-completions-bearer-v1\\u{1f}https://api.example/v1\", \
             credential_ref: \"keychain\", limits: ProviderLimits { forwarded_requests_per_turn: 64, \
             denied_requests_before_revocation: 16, concurrent_upstream_requests_per_turn: 4, \
             json_request_bytes: 8388608, aggregate_request_bytes_per_turn: 33554432, \
             response_bytes_per_request: 16777216, aggregate_response_bytes_per_turn: 67108864, \
             requested_output_tokens_per_request: 32000, reserved_token_units_per_turn: 1000000, \
             reserved_token_units_per_session: 20000000, capability_lifetime_ms: 3600000, \
             max_reserved_token_units_per_utc_day: None }, model: \"m\", \
             origins: ProviderOrigins { provider: \"global\", model: \"global\" } }",
            "ResolvedProviderPlan's Debug shape changed; if a field was added, justify it and \
             confirm it cannot carry a reusable secret"
        );
    }

    /// A fully-populated, non-secret resolved provider plan, used by the registry/binding tests.
    fn resolved_plan() -> ResolvedProviderPlan {
        ResolvedProviderPlan {
            stable_id: "fireworks".to_string(),
            protocol: ProviderProtocol::OpenAiCompatible,
            normalized_endpoint: "https://api.example/v1".to_string(),
            allow_insecure_http: false,
            credential_binding:
                "fireworks\u{1f}openai-chat-completions-bearer-v1\u{1f}https://api.example/v1"
                    .to_string(),
            credential_ref: "keychain".to_string(),
            limits: ProviderLimits::default(),
            model: "m".to_string(),
            origins: ProviderOrigins {
                provider: "global".to_string(),
                model: "global".to_string(),
            },
        }
    }

    /// A fully-capable harness: the baseline every table row starts from, so each row isolates the
    /// ONE field it flips. Mirrors what claude and opencode actually declare.
    fn capable() -> HarnessCapabilities {
        HarnessCapabilities {
            events: EventFidelity::Structured {
                tool_level: ToolEventGranularity::FileLevel,
            },
            steering: Steering::Live,
            resume: Resume::Flags,
            mcp: true,
            sandbox: Sandbox::ToolAllowlist,
            mcp_sandbox: McpSandboxCoupling::Independent,
            usage: UsageDetail::TokensAndCost,
            budgets: false,
            tool_naming: ToolNaming::McpDoubleUnderscore,
            stdin: StdinPolicy::HeldOpen,
        }
    }

    /// Every requirement off: any harness is dispatchable and nothing degrades. The zero value of
    /// [`WorkRequirements`] must be the permissive one, or a caller that forgets a field silently
    /// refuses work.
    #[test]
    fn no_requirements_never_refuses_and_never_degrades() {
        let bare = HarnessCapabilities {
            events: EventFidelity::FinalTextOnly,
            steering: Steering::None,
            resume: Resume::None,
            mcp: false,
            sandbox: Sandbox::None,
            mcp_sandbox: McpSandboxCoupling::MutuallyExclusive,
            usage: UsageDetail::None,
            budgets: false,
            tool_naming: ToolNaming::SeparateFields,
            stdin: StdinPolicy::ClosedAtStart,
        };
        let v = validate(HarnessId::Claude, &bare, &WorkRequirements::default())
            .expect("nothing required");
        assert!(!v.is_degraded(), "nothing wanted, nothing degraded: {v:?}");
    }

    /// The §5 table as a table. Each row names the one requirement + the one capability flip and the
    /// expected outcome, so a regression in any single rule is isolated by the failing row rather
    /// than by an all-or-nothing assertion.
    #[test]
    fn requirement_capability_table() {
        // (label, needs, caps-mutation, expected)
        struct Row {
            label: &'static str,
            needs: WorkRequirements,
            caps: HarnessCapabilities,
            expect: Result<DispatchVerdict, CapabilityRefusal>,
        }
        let caps = capable();
        let only_team = WorkRequirements {
            team_tools: true,
            ..Default::default()
        };
        let only_multi = WorkRequirements {
            multi_turn: true,
            ..Default::default()
        };
        let only_sandbox = WorkRequirements {
            sandbox: true,
            ..Default::default()
        };
        let both = WorkRequirements {
            team_tools: true,
            sandbox: true,
            ..Default::default()
        };
        let trace = WorkRequirements {
            trace_console: true,
            ..Default::default()
        };
        let steer = WorkRequirements {
            steering: true,
            ..Default::default()
        };
        let rows = vec![
            Row {
                label: "team_tools + mcp:false ⇒ refuse",
                needs: only_team,
                caps: HarnessCapabilities { mcp: false, ..caps },
                expect: Err(CapabilityRefusal::McpUnavailable {
                    harness: HarnessId::Claude,
                }),
            },
            Row {
                label: "team_tools + mcp:true ⇒ ok",
                needs: only_team,
                caps,
                expect: Ok(DispatchVerdict::default()),
            },
            Row {
                label: "multi_turn + resume:None ⇒ refuse",
                needs: only_multi,
                caps: HarnessCapabilities {
                    resume: Resume::None,
                    ..caps
                },
                expect: Err(CapabilityRefusal::ResumeUnavailable {
                    harness: HarnessId::Claude,
                }),
            },
            Row {
                label: "multi_turn + resume:Flags ⇒ ok",
                needs: only_multi,
                caps,
                expect: Ok(DispatchVerdict::default()),
            },
            Row {
                label: "sandbox + sandbox:None ⇒ refuse",
                needs: only_sandbox,
                caps: HarnessCapabilities {
                    sandbox: Sandbox::None,
                    ..caps
                },
                expect: Err(CapabilityRefusal::SandboxUnavailable {
                    harness: HarnessId::Claude,
                }),
            },
            Row {
                label: "team+sandbox + MutuallyExclusive ⇒ refuse",
                needs: both,
                caps: HarnessCapabilities {
                    mcp_sandbox: McpSandboxCoupling::MutuallyExclusive,
                    ..caps
                },
                expect: Err(CapabilityRefusal::McpSandboxMutuallyExclusive {
                    harness: HarnessId::Claude,
                }),
            },
            Row {
                label: "team+sandbox + Independent ⇒ ok",
                needs: both,
                caps,
                expect: Ok(DispatchVerdict::default()),
            },
            Row {
                // THE §5.1 LINE: FinalTextOnly is DISPATCHABLE, degraded, never refused.
                label: "trace_console + FinalTextOnly ⇒ degrade, not refuse",
                needs: trace,
                caps: HarnessCapabilities {
                    events: EventFidelity::FinalTextOnly,
                    ..caps
                },
                expect: Ok(DispatchVerdict {
                    degradations: vec![Degradation::NoStructuredEvents {
                        harness: HarnessId::Claude,
                    }],
                }),
            },
            Row {
                label: "trace_console + Structured ⇒ no degradation",
                needs: trace,
                caps,
                expect: Ok(DispatchVerdict::default()),
            },
            Row {
                label: "steering + Steering::None ⇒ hide, not refuse",
                needs: steer,
                caps: HarnessCapabilities {
                    steering: Steering::None,
                    ..caps
                },
                expect: Ok(DispatchVerdict {
                    degradations: vec![Degradation::SteeringHidden {
                        harness: HarnessId::Claude,
                    }],
                }),
            },
            Row {
                label: "steering + BetweenTurns ⇒ no degradation",
                needs: steer,
                caps: HarnessCapabilities {
                    steering: Steering::BetweenTurns,
                    ..caps
                },
                expect: Ok(DispatchVerdict::default()),
            },
        ];
        for row in rows {
            let got = validate(HarnessId::Claude, &row.caps, &row.needs);
            assert_eq!(got, row.expect, "row: {}", row.label);
        }
    }

    /// [`declared_capabilities`] is what a console reader uses when it has only a recorded harness
    /// NAME, and it must return exactly what the live adapter declares — otherwise the run detail
    /// would render fidelity the validator disagreed with.
    #[test]
    fn declared_capabilities_and_name_lookup_agree_with_the_adapters() {
        assert_eq!(
            declared_capabilities(HarnessId::Claude),
            crate::claude::runner::CAPABILITIES
        );
        assert_eq!(
            declared_capabilities(HarnessId::Opencode),
            crate::opencode::runner::CAPABILITIES
        );
        assert_eq!(harness_id_for_name("claude"), Some(HarnessId::Claude));
        assert_eq!(harness_id_for_name("opencode"), Some(HarnessId::Opencode));
        assert_eq!(
            harness_id_for_name("codex"),
            None,
            "known-but-unimplemented is not addressable"
        );
        assert_eq!(
            harness_id_for_name(""),
            None,
            "empty means inherit, not a harness"
        );
    }

    /// Compile-time shape check: [`validate`] returns a [`DispatchVerdict`] (so callers can read
    /// `.degradations`), not a bare list — a regression that returned the list directly would drop
    /// the "is this dispatch degraded" question the console asks.
    #[test]
    fn validate_returns_a_dispatch_verdict() {
        let _: DispatchVerdict =
            validate(HarnessId::Claude, &capable(), &WorkRequirements::default()).expect("ok");
    }

    /// MUTATION GUARD: a dispatch that needs both MCP and a sandbox must be REFUSED on a harness
    /// that declares they are mutually exclusive — even though each is individually available. Model
    /// them as independent booleans and this row returns `Ok`, silently dispatching work that cannot
    /// reach the team's tools.
    #[test]
    fn coupled_requirements_are_not_independent_booleans() {
        let needs = WorkRequirements {
            team_tools: true,
            sandbox: true,
            ..Default::default()
        };
        let exclusive = HarnessCapabilities {
            mcp: true,
            sandbox: Sandbox::Modes,
            mcp_sandbox: McpSandboxCoupling::MutuallyExclusive,
            ..capable()
        };
        assert_eq!(
            validate(HarnessId::Claude, &exclusive, &needs),
            Err(CapabilityRefusal::McpSandboxMutuallyExclusive {
                harness: HarnessId::Claude
            }),
        );

        // Either requirement ALONE is still honored: the coupling refuses the combination, not the
        // harness.
        let team_only = WorkRequirements {
            team_tools: true,
            ..Default::default()
        };
        assert!(validate(HarnessId::Claude, &exclusive, &team_only).is_ok());
        let sandbox_only = WorkRequirements {
            sandbox: true,
            ..Default::default()
        };
        assert!(validate(HarnessId::Claude, &exclusive, &sandbox_only).is_ok());
    }

    /// MUTATION GUARD: `FinalTextOnly` must NEVER be treated as structured. If `validate` (or a
    /// renderer keyed on `matches!(Structured { .. })`) accepted it, the trace would render a blank
    /// spine the operator cannot distinguish from a run that emitted nothing.
    #[test]
    fn final_text_only_is_never_structured() {
        let needs = WorkRequirements {
            trace_console: true,
            ..Default::default()
        };
        let caps = HarnessCapabilities {
            events: EventFidelity::FinalTextOnly,
            ..capable()
        };
        let v = validate(HarnessId::Opencode, &caps, &needs).expect("dispatchable");
        assert!(
            v.degradations.contains(&Degradation::NoStructuredEvents {
                harness: HarnessId::Opencode
            }),
            "FinalTextOnly must degrade the trace: {v:?}"
        );
    }

    /// MUTATION GUARD: the three measured continuation shapes must all be representable and
    /// DISTINCT. Collapse resume back to flags-only (the original §3 defect) and this fails to
    /// compile or the distinctness assertion reds.
    #[test]
    fn resume_names_flag_subcommand_and_protocol_continuation() {
        assert_ne!(Resume::Flags, Resume::Subcommand);
        assert_ne!(Resume::Subcommand, Resume::Protocol);
        assert_ne!(Resume::Flags, Resume::Protocol);
        // The contract can still say "no continuation", which is what the multi-turn rule needs.
        assert_ne!(Resume::None, Resume::Protocol);
    }

    /// MEASURED FUTURE-ADAPTER SHAPES (design §3's four defects; STUDIO-869/872). This slice does
    /// not implement goose or codex, but the contract must be able to EXPRESS what those spikes
    /// measured — otherwise the next adapter widens these enums under pressure. Each assertion pins
    /// one measured shape so collapsing it back to a shared default reds here.
    ///
    /// MUTATION GUARD: collapse event fidelity to one structured boolean, or child stdin to one
    /// default, and the corresponding assertion fails to compile or reds.
    #[test]
    fn measured_future_adapter_shapes_are_representable_and_distinct() {
        // Defect 4: opencode emits typed file-level `read`/`edit`/`bash`; codex emits only an
        // undifferentiated `command_execution`. Two granularities, not one boolean.
        assert_ne!(
            ToolEventGranularity::FileLevel,
            ToolEventGranularity::CommandOnly,
            "event fidelity must separate file-level tools from command-only events"
        );
        // §7.2: claude REQUIRES stdin held open (the INF-250 mailbox); codex hangs forever if it is.
        // Not one shared default.
        assert_ne!(
            StdinPolicy::HeldOpen,
            StdinPolicy::ClosedAtStart,
            "child stdin must be a per-harness policy, not one shared default"
        );

        // A codex-shaped harness: command-only events, closed stdin, subcommand resume, separate
        // tool-name fields, and the mcp/sandbox coupling its approval policy forces. It is
        // representable (this compiles), and the validator refuses a dispatch that requires BOTH
        // the daemon's tools and a sandbox on it. The id only labels the refusal — codex has no
        // `HarnessId` variant on purpose, so a shipped one stands in.
        let codex_shaped = HarnessCapabilities {
            events: EventFidelity::Structured {
                tool_level: ToolEventGranularity::CommandOnly,
            },
            steering: Steering::BetweenTurns,
            resume: Resume::Subcommand,
            mcp: true,
            sandbox: Sandbox::Modes,
            mcp_sandbox: McpSandboxCoupling::MutuallyExclusive,
            usage: UsageDetail::Tokens,
            budgets: false,
            tool_naming: ToolNaming::SeparateFields,
            stdin: StdinPolicy::ClosedAtStart,
        };
        let both = WorkRequirements {
            team_tools: true,
            sandbox: true,
            ..Default::default()
        };
        assert_eq!(
            validate(HarnessId::Claude, &codex_shaped, &both),
            Err(CapabilityRefusal::McpSandboxMutuallyExclusive {
                harness: HarnessId::Claude
            }),
            "the measured codex coupling must be expressible and refused"
        );
    }

    /// The refusal's `Display` must be actionable: it names what could not be honored. The reason is
    /// the whole point of a typed refusal, so a generic "refused" string would be a regression.
    #[test]
    fn refusal_display_names_the_unhonored_requirement() {
        let r = CapabilityRefusal::McpUnavailable {
            harness: HarnessId::Claude,
        };
        let s = r.to_string();
        assert!(s.contains("MCP"), "refusal reason must name MCP: {s}");

        let r = CapabilityRefusal::HarnessNotImplemented {
            name: "codex".to_string(),
        };
        let s = r.to_string();
        assert!(
            s.contains("codex") && s.contains("not implemented"),
            "unknown-harness refusal must name the harness and say it is unimplemented: {s}"
        );
    }

    // ---- STUDIO-984 harness registry / provider-protocol compatibility ----

    /// Every current `HarnessId` has exactly one registry row, so a reader can never silently get a
    /// fallback for an implemented harness. Adding a variant without a row reds this.
    #[test]
    fn registry_covers_every_harness_id() {
        for id in [HarnessId::Claude, HarnessId::Opencode] {
            let rows: Vec<_> = HARNESS_REGISTRY.iter().filter(|e| e.id == id).collect();
            assert_eq!(rows.len(), 1, "exactly one registry row for {id:?}");
        }
    }

    /// The V1 compatibility table, as a table: OpenCode may consume `openai-compatible` via a
    /// brokered loopback; Claude has no provider protocol and uses native login. A registry that
    /// claimed Claude could consume a provider would red the first row.
    #[test]
    fn registry_protocol_compatibility_matches_the_v1_table() {
        assert!(
            !harness_supports_protocol(HarnessId::Claude, ProviderProtocol::OpenAiCompatible),
            "Claude has no provider adapter in v1"
        );
        assert!(harness_supports_protocol(
            HarnessId::Opencode,
            ProviderProtocol::OpenAiCompatible
        ));
        assert_eq!(
            credential_transport(HarnessId::Claude),
            CredentialTransport::NativeLogin
        );
        assert_eq!(
            credential_transport(HarnessId::Opencode),
            CredentialTransport::BrokeredLoopback
        );
    }

    /// The protocol names are a cross-surface contract: `as_str` matches the YAML protocol config
    /// validates, and `adapter_id` is the binding identity. These literals pin THIS crate's spelling;
    /// the cross-crate agreement is pinned separately by
    /// [`protocol_and_adapter_names_are_pinned_to_the_config_crate`].
    #[test]
    fn protocol_names_are_the_cross_surface_contract() {
        assert_eq!(
            ProviderProtocol::OpenAiCompatible.as_str(),
            "openai-compatible"
        );
        assert_eq!(
            ProviderProtocol::OpenAiCompatible.adapter_id(),
            "openai-chat-completions-bearer-v1"
        );
        assert_eq!(
            ProviderProtocol::from_name("openai-compatible"),
            Some(ProviderProtocol::OpenAiCompatible)
        );
        assert_eq!(ProviderProtocol::from_name("anthropic-messages"), None);
    }

    /// CROSS-CRATE PIN (STUDIO-984 review): the agent's protocol name and adapter id must equal the
    /// config crate's constants, so a rename on EITHER side reds this test. The literals in
    /// [`protocol_names_are_the_cross_surface_contract`] cannot catch drift — they compare this
    /// crate's own spelling to itself.
    #[test]
    fn protocol_and_adapter_names_are_pinned_to_the_config_crate() {
        assert_eq!(
            ProviderProtocol::OpenAiCompatible.as_str(),
            rhapsody_config::PROTOCOL_OPENAI_COMPATIBLE
        );
        assert_eq!(
            ProviderProtocol::OpenAiCompatible.adapter_id(),
            rhapsody_config::ADAPTER_OPENAI_CHAT_COMPLETIONS_BEARER_V1
        );
    }

    /// CROSS-CRATE PIN (STUDIO-984 review, sol): the canonical credential-binding adapter identity
    /// is ONE value used by config, agent AND broker registration. Comparing agent's literal to
    /// config's constant was not enough — renaming the broker's `canonical_id` (which is actually
    /// hashed into the binding fingerprint) left every test green. This compares all three crates.
    #[test]
    fn canonical_adapter_identity_agrees_across_config_agent_and_broker() {
        assert_eq!(
            ProviderProtocol::OpenAiCompatible.adapter_id(),
            rhapsody_config::ADAPTER_OPENAI_CHAT_COMPLETIONS_BEARER_V1
        );
        assert_eq!(
            rhapsody_config::ADAPTER_OPENAI_CHAT_COMPLETIONS_BEARER_V1,
            rhapsody_provider_broker::BrokerProtocol::OpenAiChatCompletions.canonical_id(),
            "the broker hashes this id into its credential binding, so it must be the same value"
        );
    }

    /// CROSS-CRATE PIN (STUDIO-984 review, sol): `ResolvedProviderPlan.limits` carries the validated
    /// V1 default column, not a second, divergent copy. There is no shared type across the layering
    /// (broker must not depend on config; agent must not depend on config at runtime), so this
    /// asserts the agent mirror equals config's default field-for-field — changing a default in one
    /// crate reds it.
    #[test]
    fn provider_limits_agree_with_the_config_crate() {
        let ours = ProviderLimits::default();
        let theirs = rhapsody_config::BrokerLimits::default();
        assert_eq!(
            ours.forwarded_requests_per_turn,
            theirs.forwarded_requests_per_turn
        );
        assert_eq!(
            ours.denied_requests_before_revocation,
            theirs.denied_requests_before_revocation
        );
        assert_eq!(
            ours.concurrent_upstream_requests_per_turn,
            theirs.concurrent_upstream_requests_per_turn
        );
        assert_eq!(ours.json_request_bytes, theirs.json_request_bytes);
        assert_eq!(
            ours.aggregate_request_bytes_per_turn,
            theirs.aggregate_request_bytes_per_turn
        );
        assert_eq!(
            ours.response_bytes_per_request,
            theirs.response_bytes_per_request
        );
        assert_eq!(
            ours.aggregate_response_bytes_per_turn,
            theirs.aggregate_response_bytes_per_turn
        );
        assert_eq!(
            ours.requested_output_tokens_per_request,
            theirs.requested_output_tokens_per_request
        );
        assert_eq!(
            ours.reserved_token_units_per_turn,
            theirs.reserved_token_units_per_turn
        );
        assert_eq!(
            ours.reserved_token_units_per_session,
            theirs.reserved_token_units_per_session
        );
        // Config keeps the lifetime `None` (derived at read time); the plan carries the materialized V1
        // value. Pin the plan's concrete default to config's derived default on a 1-hour deadline.
        assert_eq!(
            ours.capability_lifetime_ms,
            rhapsody_config::BrokerLimits::default()
                .effective_capability_lifetime_ms(rhapsody_config::DEFAULT_CAPABILITY_LIFETIME_MS)
        );
        assert_eq!(
            ours.max_reserved_token_units_per_utc_day,
            theirs.max_reserved_token_units_per_utc_day
        );
    }

    /// CROSS-CRATE PIN: config's provider-selection policy agrees with the ONE harness registry. For
    /// every registered harness, config permits an explicit provider on exactly the backends whose
    /// registry row can consume `openai-compatible`. Giving Claude a protocol in [`HARNESS_REGISTRY`]
    /// without teaching `rhapsody-config` reds this test — the drift the design forbids when it says
    /// there must not be a second compatibility switch in config.
    #[test]
    fn config_provider_policy_agrees_with_the_harness_registry() {
        for entry in HARNESS_REGISTRY {
            let name = match entry.id {
                HarnessId::Claude => "claude",
                HarnessId::Opencode => "opencode",
            };
            let config_allows = rhapsody_config::PROVIDER_HARNESS_BACKENDS.contains(&name);
            let registry_supports = entry
                .protocols
                .contains(&ProviderProtocol::OpenAiCompatible);
            assert_eq!(
                config_allows, registry_supports,
                "config's provider policy and HARNESS_REGISTRY disagree for harness {name:?}"
            );
        }
    }

    /// MUTATION GUARD: the supported-version table is fail-closed and single-sourced from the PB0
    /// probe. An implementation that "assumes any version is fine" cannot resolve an unknown one and
    /// reds the unknown row.
    #[test]
    fn supported_opencode_versions_are_fail_closed_and_single_sourced() {
        assert_eq!(SUPPORTED_OPENCODE_VERSIONS.len(), 1);
        let row =
            brokered_opencode_version_row("1.18.30").expect("the pinned version is supported");
        assert_eq!(row.adapter_version, "2.0.41");
        assert_eq!(
            SUPPORTED_OPENCODE_VERSIONS,
            crate::opencode::probe::SUPPORTED,
            "the registry table must BE the probe's table, not an independent copy"
        );
        for unknown in ["1.18.31", "9.9.9", ""] {
            let err = brokered_opencode_version_row(unknown).unwrap_err();
            assert!(
                matches!(err, BrokeredOpenCodeRefusal::UnsupportedVersion { .. }),
                "{unknown:?}: {err:?}"
            );
        }
    }

    /// MUTATION GUARD: the initial brokered compatibility row accepts only the pinned knobs. A check
    /// that permitted an unknown agent, a variant, `auto_approve: false`, or `extra_args` reds a row
    /// here, and the refusal is the typed pre-credential one.
    #[test]
    fn brokered_opencode_controls_table() {
        // Accepted shapes.
        assert!(check_brokered_opencode_controls("", "", None, &[], "opencode").is_ok());
        assert!(
            check_brokered_opencode_controls("build", "", Some(true), &[], "/opt/opencode").is_ok()
        );

        // Refused shapes.
        assert_eq!(
            check_brokered_opencode_controls("plan", "", None, &[], "opencode"),
            Err(BrokeredOpenCodeRefusal::AgentUnsupported {
                agent: "plan".to_string()
            })
        );
        assert_eq!(
            check_brokered_opencode_controls("", "high", None, &[], "opencode"),
            Err(BrokeredOpenCodeRefusal::VariantUnsupported {
                variant: "high".to_string()
            })
        );
        assert_eq!(
            check_brokered_opencode_controls("", "", Some(false), &[], "opencode"),
            Err(BrokeredOpenCodeRefusal::AutoApprovalDisabled)
        );
        assert_eq!(
            check_brokered_opencode_controls(
                "",
                "",
                Some(true),
                &["--log-level".to_string(), "DEBUG".to_string()],
                "opencode"
            ),
            Err(BrokeredOpenCodeRefusal::ExtraArgsUnsupported { count: 2 })
        );
        assert_eq!(
            check_brokered_opencode_controls("", "", None, &[], "env opencode"),
            Err(BrokeredOpenCodeRefusal::CommandHasEmbeddedArgs)
        );
    }

    /// Each brokered refusal's Display is actionable — it names what was refused and why, so an
    /// operator can fix it. A generic string would be a regression.
    #[test]
    fn brokered_refusal_display_is_actionable() {
        let cases = [
            (
                BrokeredOpenCodeRefusal::UnsupportedVersion {
                    found: "9.9.9".to_string(),
                },
                "9.9.9",
            ),
            (
                BrokeredOpenCodeRefusal::AgentUnsupported {
                    agent: "plan".to_string(),
                },
                "plan",
            ),
            (
                BrokeredOpenCodeRefusal::VariantUnsupported {
                    variant: "high".to_string(),
                },
                "high",
            ),
            (
                BrokeredOpenCodeRefusal::AutoApprovalDisabled,
                "auto_approve",
            ),
            (
                BrokeredOpenCodeRefusal::ExtraArgsUnsupported { count: 1 },
                "extra_args",
            ),
            (
                BrokeredOpenCodeRefusal::CommandHasEmbeddedArgs,
                "embedded arguments",
            ),
        ];
        for (refusal, needle) in cases {
            let s = refusal.to_string();
            assert!(
                s.contains(needle),
                "refusal {refusal:?} must name {needle:?}: {s}"
            );
        }
    }
}
