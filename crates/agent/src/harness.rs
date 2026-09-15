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
//!    **Deferred to slice 5** — needs a product call before the refusal path (§5) can be designed;
//!    see the doc comments on [`HarnessCapabilities::mcp`] and [`HarnessCapabilities::sandbox`].
//!    Claude's own values for the two fields ARE independent (it always honours both), so nothing
//!    about Claude's behavior is affected by leaving this unresolved.
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
//!    (`--resume <id>` / `-s <id>`). **Fixed**: [`Resume`] has a `Subcommand` variant alongside
//!    `Flags`, so the shape can name the difference instead of forcing every harness through one.
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
//! ## What this slice deliberately does NOT do
//!
//! - **No second harness.** [`HarnessId`] has exactly one variant. Every later variant is a real,
//!   reviewed change to every `match` on it — that is a feature of a closed enum, not a
//!   gap to paper over with a wildcard arm.
//! - **The runner is still built at effective-build time**, not at dispatch (design §4.1's "largest
//!   structural edit"). [`crate::Session::set_model_override`]'s doc comment already states why:
//!   Teams picks the identity at dispatch, but *swapping a whole harness* is slice 4's job, not
//!   this one's. `HarnessSpec` exists as a type here so slice 4 has somewhere to put the resolved
//!   choice; nothing in this crate constructs one anywhere but effective-build time yet.
//! - **`session_uuid` is untouched.** The open product call from STUDIO-872 §7.2 (whether a
//!   per-run-isolated session id can still serve as an identity key) is store/runs-schema territory
//!   (design §6.2, slice 3), and no type in this module carries or interprets one. Nothing here
//!   assumes a resolution either way.

use crate::Runner;

/// Which harness a resolved [`HarnessSpec`] names. Exactly one variant today (design D4 lists four:
/// claude, codex, goose, opencode; `pi` was dropped for having no MCP client) — adding the next one
/// is a real, reviewed change to every `match` on this type, not a default arm silently absorbing
/// it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HarnessId {
    Claude,
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
/// daemon does not write provider configs, so this is passed through, never stored).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderAuth {
    ApiKey(String),
    None,
}

/// The per-harness opaque knob block (design §4.2: "a normalized core plus an opaque per-harness
/// knob block," because the knobs do not line up across CLIs). One variant per [`HarnessId`] — the
/// claude arm carries the existing, untouched [`crate::claude::Config`], so wrapping a `Config` in
/// this enum changes nothing about what reaches [`crate::claude::runner::Runner`].
#[derive(Debug, Clone)]
pub enum HarnessKnobs {
    Claude(crate::claude::Config),
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

/// How a harness continues a prior conversation (design §3, fixed per defect 3). `Flags` covers
/// claude (`--resume <id>`) and opencode (`-s <id>`) — genuinely the same shape, an unchanged flag
/// added to an otherwise-normal invocation. `Subcommand` covers codex (`codex exec resume
/// <thread_id>`), whose continuation is a different positional-argument invocation shape entirely,
/// not an extra flag on the same one. Collapsing these into one `SameFlags` variant (the original
/// §3 shape) is exactly the defect being fixed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resume {
    None,
    Flags,
    Subcommand,
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
    /// Whether this harness can reach the daemon's own MCP tools at all. **Known incomplete**: for
    /// codex this is not independent of [`Self::sandbox`] — the CLI honours a sandbox or an MCP
    /// server, never both (defect 1, deferred to slice 5's product call). Claude's `mcp` and
    /// `sandbox` values are genuinely independent, so this deferral does not affect Claude.
    pub mcp: bool,
    /// See [`Self::mcp`] — the same deferred defect 1 from the other side.
    pub sandbox: Sandbox,
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
