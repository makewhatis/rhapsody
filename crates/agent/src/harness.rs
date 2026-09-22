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

/// A non-default model provider/endpoint (design §3's "the multi-provider axis": base URL + auth
/// kind). No adapter constructs one yet — Claude always uses its own CLI's default auth — so this
/// is the type slice 4's resolution chain will populate, not yet a live code path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provider {
    pub base_url: String,
    pub auth: ProviderAuth,
}

/// How a [`Provider`] authenticates. `ApiKey` carries the credential itself (design §4.5: the
/// daemon does not write provider configs, so this is passed through, never stored). `Debug` is
/// hand-written to redact the key, because `HarnessSpec` derives `Debug` transitively and a future
/// `tracing::debug!(?spec)` must never write a raw credential to the rotating file logs.
/// `HarnessKnobs::Claude`'s own `crate::claude::Config` carries a second credential
/// (`tracker_api_key`, the resolved Linear key) reachable the same way, so its `Debug` is likewise
/// hand-written rather than derived — both fields are covered, not just this one. This is a new
/// redacting-`Debug` pattern in the crate, not a repeat of an existing one: the repo's other
/// secret-bearing type, `crates/orchestrator/src/reads.rs`'s `ReadsTarget`, has no `Debug` impl at
/// all and masks only at its reporting boundary (`mask_token`) — the convention both share is that a
/// secret-bearing type never lets the raw value reach a log line, not the specific mechanism.
#[derive(Clone, PartialEq, Eq)]
pub enum ProviderAuth {
    ApiKey(String),
    None,
}

impl fmt::Debug for ProviderAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ApiKey(_) => f.write_str("ApiKey(***)"),
            Self::None => f.write_str("None"),
        }
    }
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
/// provider, and its per-harness knobs. Constructed today only at effective-build time (see the
/// module doc's "what this slice does not do") from the installation's static config; `model` and
/// `provider` are `None` at every call site that exists today because Claude's equivalent values
/// already live inside its own `knobs` block, not because the fields are unused in principle —
/// slice 4's dispatch-time resolution chain is what will populate them independently of a
/// per-harness knob block (e.g. a teammate profile naming a model, STUDIO-868, already does this
/// on `Session` directly, ahead of and independent of this type).
#[derive(Debug, Clone)]
pub struct HarnessSpec {
    pub harness: HarnessId,
    pub model: Option<String>,
    pub provider: Option<Provider>,
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

#[cfg(test)]
mod tests {
    use super::*;

    // A `{:?}` on a `ProviderAuth::ApiKey` must never print the credential it carries — `Debug`
    // is hand-written specifically to redact it (see the doc comment on `ProviderAuth`), and the
    // derive that would print it is the mistake this test catches on any regression back to it.
    #[test]
    fn provider_auth_debug_redacts_the_api_key() {
        let secret = ProviderAuth::ApiKey("sk-super-secret-value".to_string());
        let rendered = format!("{secret:?}");
        assert!(
            !rendered.contains("sk-super-secret-value"),
            "Debug output leaked the raw key: {rendered}"
        );
        assert_eq!(rendered, "ApiKey(***)");

        assert_eq!(format!("{:?}", ProviderAuth::None), "None");
    }

    // A full `HarnessSpec`'s `{:?}` reaches two credentials transitively — `Provider::auth` and
    // `HarnessKnobs::Claude`'s `tracker_api_key` — and neither must ever appear raw in a
    // `tracing::debug!(?spec)` line. This is the round-3 review finding: `ProviderAuth`'s own test
    // above only proves that ONE of the two is redacted in isolation, not that a real, fully
    // populated spec is safe end to end.
    #[test]
    fn harness_spec_debug_redacts_every_credential_it_carries() {
        let spec = HarnessSpec {
            harness: HarnessId::Claude,
            model: None,
            provider: Some(Provider {
                base_url: "https://example".to_string(),
                auth: ProviderAuth::ApiKey("sk-provider-secret".to_string()),
            }),
            knobs: HarnessKnobs::Claude(crate::claude::Config {
                tracker_api_key: "lin-tracker-secret".to_string(),
                ..Default::default()
            }),
        };
        let rendered = format!("{spec:?}");
        assert!(
            !rendered.contains("sk-provider-secret"),
            "Debug output leaked the provider key: {rendered}"
        );
        assert!(
            !rendered.contains("lin-tracker-secret"),
            "Debug output leaked the tracker key: {rendered}"
        );
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
}
