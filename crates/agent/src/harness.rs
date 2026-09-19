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
//!    (`--resume <id>` / `-s <id>`). **Fixed for codex, NOT for goose** (see the STUDIO-872 section
//!    below — this fix is not the full close of the defect): [`Resume`] has a `Subcommand` variant
//!    alongside `Flags`, so the shape can name codex's difference instead of forcing every harness
//!    through one.
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

/// How a harness continues a prior conversation (design §3, defect 3 — fixed for codex, NOT for
/// goose; see the module doc's STUDIO-872 section). `Flags` covers claude (`--resume <id>`) and
/// opencode (`-s <id>`) — genuinely the same shape, an unchanged flag added to an otherwise-normal
/// invocation. `Subcommand` covers codex (`codex exec resume <thread_id>`), whose continuation is a
/// different positional-argument invocation shape entirely, not an extra flag on the same one.
/// Collapsing these into one `SameFlags` variant (the original §3 shape) was the defect codex
/// motivated fixing — but goose's `session/load` protocol-method resume fits neither variant here,
/// so this enum does not yet cover every harness the design names.
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
}
