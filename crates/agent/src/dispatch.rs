//! Dispatch-time provider ownership and the immutable launch context (STUDIO-1000, slice B5 of
//! `~/.rhapsody/docs/provider-broker-design.md` §3.1 / §10.2, and `provider-auth-design.md` §3).
//! Rhapsody-only — no Go counterpart; the frozen reference prebuilds one static runner and has no
//! provider broker.
//!
//! # Why this module exists
//!
//! The parent harness design prebuilds one runner per harness at effective-build time and then calls
//! three late session setters (run id, review head, model override) once a dispatch is chosen.
//! Provider preparation cannot safely be bolted onto that shape: a prepared broker session belongs to
//! exactly ONE dispatch and must never be retained by a shared, cloneable runner. This module is the
//! move-only layer that fixes that:
//!
//! * [`PreparedHarnessSpec`] is the frozen dispatch input — the harness, its resolved model, and (for
//!   a brokered run) the [`PreparedProvider`] custody. It is **not `Clone`**; cloning a prepared
//!   dispatch could silently extend credential custody past cancellation.
//! * [`DispatchRunner`] is what the dispatch-time factory returns: an owned, **non-`Clone`** runner
//!   that owns the prepared spec exactly once. It is the answer to "the factory must not put prepared
//!   custody in the effective config's shared `Arc<dyn Runner>`".
//! * [`DispatchRunner::start`] **consumes** the runner and returns an explicit [`StartedSession`]
//!   split between the live adapter session (which owns the [`BrokerSession`]) and the worker-owned,
//!   non-secret [`BrokerLedgerReceiver`]. It does not return only the session while retaining custody
//!   on the side.
//! * [`SessionStart`]/[`LaunchContext`] carry run id and review head so both exist **before** the
//!   adapter session is built, instead of being patched on afterwards.
//!
//! # Legacy bridge (deliberate, and why it is not a fallback)
//!
//! The parent migration keeps the ported [`crate::Runner`]/[`crate::Session`] traits unchanged. With
//! no explicit provider selected, [`DispatchRunner::start`] bridges to the existing legacy runner and
//! re-applies the launch context through the legacy setters — **byte-identical** behavior to a daemon
//! built before this layer existed. A shared legacy `Arc<dyn Harness>` may sit behind that bridge, but
//! the bridge owns no [`BrokerSession`]: custody lives only in the non-`Clone` [`DispatchRunner`].
//! The provider path never uses a late setter: the launch context is part of the start value and is
//! frozen before the adapter session is built.
//!
//! # What this slice does NOT wire (explicit non-goals)
//!
//! Prepared-dispatch loop integration (PB7), OpenCode's brokered materialization (PB6), credential
//! owner reads, and durable receipt persistence are all later slices. Nothing here changes the
//! production worker path; a real brokered adapter is exercised by tests through a fake.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rhapsody_core::Issue;
use rhapsody_provider_broker::{
    BoundCredentialLease, BrokerLedgerReceiver, BrokerLimits, BrokerProtocol, BrokerRegistrar,
    BrokerRegistration, BrokerRegistrationPlan, BrokerSession, SessionPolicy,
};

use crate::harness::{
    Harness, HarnessId, HarnessKnobs, ProviderLimits, ProviderProtocol, ResolvedProviderPlan,
    harness_supports_protocol,
};
use crate::{AgentError, ModelOverride, Session, Transcript};

/// The immutable identity/launch values a dispatch carries, frozen before the adapter session is
/// built (design §10.2). These used to arrive as three late setters after `start_session`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LaunchContext {
    /// The store run-row id for this dispatch (`0` when the store is disabled or `start_run` failed,
    /// exactly as the legacy `set_run_id` treated a zero id).
    pub run_id: i64,
    /// The pull-request head SHA a REVIEW run was dispatched against; `None` for every non-review run.
    pub review_head: Option<String>,
}

/// The one start value that replaces the late identity setters (design §10.2). It is passed whole to
/// [`DispatchRunner::start`], so run id, review head, workspace and issue all exist before any adapter
/// session is constructed.
pub struct SessionStart {
    pub workspace_path: String,
    pub issue: Issue,
    pub transcript: Option<Transcript>,
    pub launch: LaunchContext,
}

impl fmt::Debug for SessionStart {
    /// `Transcript` holds boxed writers and is not `Debug`, so the transcript is reported by presence
    /// only — the launch context and issue are the fields that matter for diagnostics.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionStart")
            .field("workspace_path", &self.workspace_path)
            .field("issue", &self.issue)
            .field("transcript", &self.transcript.is_some())
            .field("launch", &self.launch)
            .finish()
    }
}

/// One dispatch's prepared, move-only provider custody (design §3.1's `PreparedProvider`).
///
/// It pairs the stable, non-secret provider metadata with the opaque [`BrokerSession`] (which owns the
/// bound credential lease) and its non-secret [`BrokerLedgerReceiver`]. It has **no credential
/// accessor** and is **not `Clone`**: custody is consumed exactly once, by
/// [`DispatchRunner::start`].
pub struct PreparedProvider {
    stable_id: String,
    protocol: ProviderProtocol,
    access: Option<BrokerSession>,
    ledgers: Option<BrokerLedgerReceiver>,
}

impl PreparedProvider {
    /// Wrap a successful broker registration (design §10.1 step 4) as one dispatch's prepared
    /// provider. The registration's move-only session and non-secret ledger receiver are both taken.
    pub fn from_registration(
        stable_id: impl Into<String>,
        protocol: ProviderProtocol,
        registration: BrokerRegistration,
    ) -> Self {
        Self {
            stable_id: stable_id.into(),
            protocol,
            access: Some(registration.session),
            ledgers: Some(registration.ledgers),
        }
    }

    /// The stable, non-secret provider id (provenance/diagnostics only).
    pub fn stable_id(&self) -> &str {
        &self.stable_id
    }

    /// The provider protocol this custody was registered for.
    pub fn protocol(&self) -> ProviderProtocol {
        self.protocol
    }

    /// The opaque session custody handle, transferred at start. `None` once already taken — a second
    /// take cannot succeed, which is what makes the transfer exactly-once.
    pub(crate) fn take_access(&mut self) -> Option<BrokerSession> {
        self.access.take()
    }

    /// The worker-owned ledger receiver, transferred at start. `None` once already taken.
    pub(crate) fn take_ledgers(&mut self) -> Option<BrokerLedgerReceiver> {
        self.ledgers.take()
    }
}

impl fmt::Debug for PreparedProvider {
    /// Deliberately non-exhaustive and secret-free: the stable id and protocol are non-secret
    /// diagnostics, while the session and ledger handles redact themselves.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PreparedProvider")
            .field("stable_id", &self.stable_id)
            .field("protocol", &self.protocol)
            .finish_non_exhaustive()
    }
}

/// The frozen dispatch input: the harness, its resolved model, and (for a brokered run) its prepared
/// provider custody. Move-only — see the module docs. Consumed exactly once by
/// [`build_dispatch_runner`].
pub struct PreparedHarnessSpec {
    pub harness: HarnessId,
    /// The resolved model, or `None` when the branch preserves the CLI default.
    pub model: Option<String>,
    /// The prepared provider custody, or `None` on the legacy/native-login branch. A raw reusable key
    /// is unrepresentable here — this field is the opaque, move-only [`PreparedProvider`].
    pub provider: Option<PreparedProvider>,
    pub knobs: HarnessKnobs,
}

impl fmt::Debug for PreparedHarnessSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PreparedHarnessSpec")
            .field("harness", &self.harness)
            .field("model", &self.model)
            .field("provider", &self.provider)
            .field("knobs", &self.knobs)
            .finish()
    }
}

/// What a dispatch-time factory returns for one dispatch: the live adapter session plus the
/// worker-owned, non-secret ledger receiver when the run is brokered (design §10.2/§10.3). The split
/// is explicit rather than implicit: the session owns the [`BrokerSession`], the worker keeps the
/// receiver and arms each turn outside the cancellable turn future.
pub struct StartedSession {
    pub session: Box<dyn Session>,
    /// `Some` for a brokered run, `None` on the legacy/native-login branch.
    pub broker_turns: Option<BrokerLedgerReceiver>,
}

impl fmt::Debug for StartedSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StartedSession")
            .field("broker_turns", &self.broker_turns.is_some())
            .finish_non_exhaustive()
    }
}

/// A dispatch-time refusal (design §10.2: "unsupported combinations refuse rather than fall back").
/// Every variant names what was refused; none is a silent downgrade to another harness, provider,
/// model, or auth source.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DispatchRefusal {
    /// The prepared harness has no adapter for the prepared provider's protocol.
    #[error(
        "dispatch_refusal: harness {harness:?} cannot consume provider protocol {protocol:?}; \
         refusing rather than falling back"
    )]
    UnsupportedProtocol {
        harness: HarnessId,
        protocol: ProviderProtocol,
    },
    /// A prepared spec's harness disagrees with the harness the factory was given.
    #[error(
        "dispatch_refusal: the dispatch-time harness {configured:?} does not match the prepared \
         spec's harness {expected:?}"
    )]
    HarnessMismatch {
        configured: HarnessId,
        expected: HarnessId,
    },
    /// An explicit provider was prepared with no exact model.
    #[error("dispatch_refusal: prepared provider {provider:?} requires an exact model")]
    MissingModel { provider: String },
    /// The broker refused registration or the lowered plan was invalid. The message is the broker's
    /// typed, non-secret refusal text.
    #[error("dispatch_refusal: {0}")]
    Broker(String),
    /// The prepared custody was already consumed (a second `start`/`take`). Unreachable through
    /// [`DispatchRunner::start`], which consumes the runner; kept typed so a future non-consuming
    /// caller cannot silently succeed twice.
    #[error("dispatch_refusal: the prepared provider custody was already taken")]
    CustodyAlreadyTaken,
}

/// The dispatch-time factory: consume a frozen [`PreparedHarnessSpec`] and return an owned,
/// non-`Clone` [`DispatchRunner`], building the harness from the spec's resolved [`HarnessKnobs`] and
/// applying the resolved model to them (design §10.2's "model/provider are live construction
/// inputs").
///
/// It is the one place that:
/// * builds the adapter from the resolved knobs, so the resolved model reaches the construction
///   inputs (argv) rather than being discarded;
/// * re-checks harness↔protocol compatibility, refusing rather than falling back;
/// * requires an exact model on the explicit-provider branch; and
/// * moves the prepared provider custody into the returned runner (never into a shared `Arc`).
pub fn build_dispatch_runner(spec: PreparedHarnessSpec) -> Result<DispatchRunner, DispatchRefusal> {
    let harness = harness_from_knobs(&spec)?;
    finish_dispatch_runner(harness, spec)
}

/// The shared-runner bridge constructor: use an already-built (possibly shared) legacy `Arc<dyn
/// Harness>` for a prepared spec instead of building one from its knobs. This is the migration seam
/// (design §10.2: "a shared legacy runner may remain behind that bridge") and the test seam.
///
/// Custody is unaffected: the bridge owns no [`BrokerSession`]; it lives only in the non-`Clone`
/// [`DispatchRunner`] this returns.
pub fn bridge_dispatch_runner(
    harness: Arc<dyn Harness>,
    spec: PreparedHarnessSpec,
) -> Result<DispatchRunner, DispatchRefusal> {
    finish_dispatch_runner(harness, spec)
}

/// Validate the prepared spec against the harness it will run on and move its custody into a
/// [`DispatchRunner`]. Shared by the factory and the bridge so the two cannot disagree.
fn finish_dispatch_runner(
    harness: Arc<dyn Harness>,
    spec: PreparedHarnessSpec,
) -> Result<DispatchRunner, DispatchRefusal> {
    if harness.id() != spec.harness {
        return Err(DispatchRefusal::HarnessMismatch {
            configured: harness.id(),
            expected: spec.harness,
        });
    }
    if let Some(provider) = &spec.provider {
        // Unsupported transport/protocol refuses here, before the session is ever started.
        if !harness_supports_protocol(spec.harness, provider.protocol()) {
            return Err(DispatchRefusal::UnsupportedProtocol {
                harness: spec.harness,
                protocol: provider.protocol(),
            });
        }
        if spec.model.as_deref().is_none_or(str::is_empty) {
            return Err(DispatchRefusal::MissingModel {
                provider: provider.stable_id().to_string(),
            });
        }
    }
    let PreparedHarnessSpec {
        model,
        provider,
        knobs: _,
        harness: _,
    } = spec;
    Ok(DispatchRunner {
        harness,
        model,
        provider,
    })
}

/// Build the adapter a prepared spec names from its resolved knobs, applying the resolved model to
/// the knobs' own model field so it is a live construction input. Exhaustive on [`HarnessKnobs`] with
/// no wildcard arm: a new harness must stop this compiling rather than silently resolve to claude.
fn harness_from_knobs(spec: &PreparedHarnessSpec) -> Result<Arc<dyn Harness>, DispatchRefusal> {
    let knobs = resolved_knobs(spec);
    let (id, harness): (HarnessId, Arc<dyn Harness>) = match knobs {
        HarnessKnobs::Claude(config) => (
            HarnessId::Claude,
            Arc::new(crate::claude::Runner::new(config)),
        ),
        HarnessKnobs::Opencode(config) => (
            HarnessId::Opencode,
            Arc::new(crate::opencode::Runner::new(config)),
        ),
    };
    if id != spec.harness {
        return Err(DispatchRefusal::HarnessMismatch {
            configured: id,
            expected: spec.harness,
        });
    }
    Ok(harness)
}

/// The construction inputs the factory builds the adapter from: the prepared spec's knobs with its
/// resolved model applied as a live construction input. [`harness_from_knobs`] routes through exactly
/// this function, so a test that asserts its result observes the block the harness is actually built
/// from — the argv source — rather than a value stored separately on the runner.
fn resolved_knobs(spec: &PreparedHarnessSpec) -> HarnessKnobs {
    apply_model(spec.knobs.clone(), spec.model.clone())
}

/// Apply a resolved model to a harness's own knob block (the argv construction input). `None`
/// preserves the block's configured model, which is what keeps the legacy/native-login path's model
/// handling unchanged. Exhaustive on [`HarnessKnobs`] with no wildcard arm.
fn apply_model(knobs: HarnessKnobs, model: Option<String>) -> HarnessKnobs {
    match knobs {
        HarnessKnobs::Claude(mut config) => {
            if let Some(model) = model {
                config.model = model;
            }
            HarnessKnobs::Claude(config)
        }
        HarnessKnobs::Opencode(mut config) => {
            if let Some(model) = model {
                config.model = model;
            }
            HarnessKnobs::Opencode(config)
        }
    }
}

/// The lowered session/capability limits a prepared plan's provider will enforce, built from the
/// plan's own validated [`ProviderLimits`] — the broker never re-derives config defaults.
pub fn lower_provider_limits(limits: &ProviderLimits) -> BrokerLimits {
    BrokerLimits {
        max_forwarded_requests: limits.forwarded_requests_per_turn,
        max_denied_requests: limits.denied_requests_before_revocation,
        max_concurrent_requests: limits.concurrent_upstream_requests_per_turn,
        max_request_bytes: limits.json_request_bytes,
        max_request_bytes_turn: limits.aggregate_request_bytes_per_turn,
        max_response_bytes: limits.response_bytes_per_request,
        max_response_bytes_turn: limits.aggregate_response_bytes_per_turn,
        max_output_tokens_request: limits.requested_output_tokens_per_request,
        max_reserved_tokens_turn: limits.reserved_token_units_per_turn,
        max_reserved_tokens_session: limits.reserved_token_units_per_session,
        max_reserved_token_units_per_utc_day: limits.max_reserved_token_units_per_utc_day,
        max_capability_lifetime: Duration::from_millis(limits.capability_lifetime_ms),
    }
}

/// The one explicit lowering from the config-owned, non-secret [`ResolvedProviderPlan`] into the
/// broker-owned [`BrokerRegistrationPlan`] (design §3.1: "PB5/PB7 perform the one explicit lowering
/// … after all pure validation and before credential registration").
///
/// The broker crate imports no config-layer provider type: the lowering lives here, in the layer that
/// depends on both the pure plan and the broker's policy types, and the broker only ever validates
/// its own `BrokerRegistrationPlan`.
pub fn lower_provider_plan(
    plan: &ResolvedProviderPlan,
) -> Result<BrokerRegistrationPlan, DispatchRefusal> {
    let protocol = match plan.protocol {
        // Exhaustive on purpose: a new protocol must stop this compiling rather than silently map
        // to the OpenAI adapter.
        ProviderProtocol::OpenAiCompatible => BrokerProtocol::OpenAiChatCompletions,
    };
    BrokerRegistrationPlan::new(
        plan.stable_id.clone(),
        protocol,
        plan.normalized_endpoint.clone(),
        plan.allow_insecure_http,
        plan.model.clone(),
        lower_provider_limits(&plan.limits),
    )
    .map_err(|e| DispatchRefusal::Broker(e.to_string()))
}

/// Register one prepared dispatch with the broker: lower the pure plan once, then move the bound
/// credential lease into the broker and wrap the resulting move-only custody as a
/// [`PreparedProvider`]. `policy` must carry the same limits the plan lowered to (the broker refuses
/// a mismatch), and is the injection point for an optional durable UTC-day authority.
pub fn prepare_provider(
    plan: &ResolvedProviderPlan,
    registrar: &BrokerRegistrar,
    lease: BoundCredentialLease,
    policy: SessionPolicy,
) -> Result<PreparedProvider, DispatchRefusal> {
    let registration_plan = lower_provider_plan(plan)?;
    let registration = registrar
        .register_session(registration_plan, lease, policy)
        .map_err(|e| DispatchRefusal::Broker(e.to_string()))?;
    Ok(PreparedProvider::from_registration(
        plan.stable_id.clone(),
        plan.protocol,
        registration,
    ))
}

/// An owned, non-`Clone` dispatch runner: the resolved harness, its resolved model, and (for a
/// brokered run) the prepared provider custody. Returned by [`build_dispatch_runner`]; consumed by
/// [`DispatchRunner::start`].
pub struct DispatchRunner {
    harness: Arc<dyn Harness>,
    model: Option<String>,
    provider: Option<PreparedProvider>,
}

impl fmt::Debug for DispatchRunner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DispatchRunner")
            .field("harness", &self.harness.id())
            .field("model", &self.model)
            .field("provider", &self.provider)
            .finish_non_exhaustive()
    }
}

impl DispatchRunner {
    /// Start the dispatch, consuming it exactly once. The launch context (run id, review head) is
    /// already frozen on `start`, so no identity setter runs on the provider path.
    ///
    /// * Provider branch: the live adapter session owns the [`BrokerSession`] and the returned
    ///   [`StartedSession::broker_turns`] carries the non-secret ledger receiver to the worker. No
    ///   late setter is called.
    /// * Legacy branch: bridges to the ported [`Runner::start_session`] and re-applies the launch
    ///   context through the legacy setters, preserving byte-identical behavior for an installation
    ///   with no explicit provider.
    pub async fn start(mut self, start: SessionStart) -> Result<StartedSession, AgentError> {
        // The adapter session is built from the SAME frozen start value on both branches, so the
        // workspace/issue/transcript inputs cannot diverge between them.
        let inner = self
            .harness
            .start_session(&start.workspace_path, start.issue.clone(), start.transcript)
            .await?;

        match self.provider.take() {
            None => {
                // Legacy bridge: the ported traits keep their late setters. This is what keeps the
                // no-provider path byte-identical to a daemon built before this layer existed.
                inner.set_run_id(start.launch.run_id);
                if let Some(sha) = start.launch.review_head.as_deref()
                    && !sha.is_empty()
                {
                    inner.set_review_head(sha);
                }
                if let Some(model) = self.model.as_deref()
                    && !model.is_empty()
                {
                    inner.set_model_override(ModelOverride {
                        model: model.to_string(),
                        ..Default::default()
                    });
                }
                Ok(StartedSession {
                    session: inner,
                    broker_turns: None,
                })
            }
            Some(mut provider) => {
                let access = provider
                    .take_access()
                    .ok_or(DispatchRefusal::CustodyAlreadyTaken)
                    .map_err(|e| AgentError::Other(e.to_string()))?;
                let ledgers = provider
                    .take_ledgers()
                    .ok_or(DispatchRefusal::CustodyAlreadyTaken)
                    .map_err(|e| AgentError::Other(e.to_string()))?;
                // The provider path has ONE immutable source of truth: the launch context is moved
                // into the broker-owned adapter session, and no legacy setter is called.
                let session: Box<dyn Session> = Box::new(BrokeredSession {
                    inner,
                    access,
                    launch: start.launch,
                });
                Ok(StartedSession {
                    session,
                    broker_turns: Some(ledgers),
                })
            }
        }
    }

    /// Whether this runner carries prepared broker custody (test/diagnostic).
    pub fn is_brokered(&self) -> bool {
        self.provider.is_some()
    }

    /// The exact resolved model this runner was built with, or `None` when the CLI default is
    /// preserved. A provenance/diagnostics reader; the mutation guard is dropping it in the factory.
    pub fn resolved_model(&self) -> Option<&str> {
        self.model.as_deref()
    }
}

/// The live adapter session for a brokered dispatch: it owns the [`BrokerSession`] (revoking it on
/// drop) and the frozen [`LaunchContext`], and delegates turn behavior to the wrapped session. PB6's
/// real OpenCode session will consume a per-turn capability from the access; PB5 owns only the
/// custody transfer and revocation.
struct BrokeredSession {
    inner: Box<dyn Session>,
    access: BrokerSession,
    launch: LaunchContext,
}

impl fmt::Debug for BrokeredSession {
    /// The launch context and the (self-redacting) custody handle: both are part of this session's
    /// identity, so formatting them is a real read, not decoration.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BrokeredSession")
            .field("launch", &self.launch)
            .field("access", &self.access)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl Session for BrokeredSession {
    fn id(&self) -> String {
        self.inner.id()
    }

    fn thread_id(&self) -> String {
        self.inner.thread_id()
    }

    async fn run_turn(
        &self,
        prompt: &str,
        attempt: Option<i64>,
        messages: Option<&mut tokio::sync::mpsc::Receiver<String>>,
        on_event: &(dyn Fn(crate::Event) + Send + Sync),
    ) -> (crate::TurnResult, Option<AgentError>) {
        self.inner
            .run_turn(prompt, attempt, messages, on_event)
            .await
    }

    async fn stop(&self) -> Result<(), AgentError> {
        self.inner.stop().await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use static_assertions::assert_not_impl_any;

    use super::*;
    use crate::Runner;
    use crate::fake::Fake;
    use crate::harness::{HarnessCapabilities, declared_capabilities};
    use rhapsody_provider_broker::{Broker, CredentialBinding, OsRandom, SystemClock, TurnMeta};

    // --- compile-time ownership guards (mutation: make any of these Clone and the build fails) ---

    assert_not_impl_any!(DispatchRunner: Clone);
    assert_not_impl_any!(PreparedHarnessSpec: Clone);
    assert_not_impl_any!(PreparedProvider: Clone);
    assert_not_impl_any!(StartedSession: Clone);

    /// A [`Harness`] that reports `opencode` while reusing [`Fake`]'s recorded-session behavior, so
    /// the provider path (only opencode consumes a provider protocol in v1) can be driven without a
    /// real CLI.
    struct OpencodeFake {
        inner: Fake,
        caps: HarnessCapabilities,
    }

    impl OpencodeFake {
        fn new() -> Self {
            Self {
                inner: Fake::new(),
                caps: declared_capabilities(HarnessId::Opencode),
            }
        }
    }

    #[async_trait]
    impl Runner for OpencodeFake {
        async fn start_session(
            &self,
            workspace_path: &str,
            issue: Issue,
            transcript: Option<Transcript>,
        ) -> Result<Box<dyn Session>, AgentError> {
            self.inner
                .start_session(workspace_path, issue, transcript)
                .await
        }
    }

    impl Harness for OpencodeFake {
        fn id(&self) -> HarnessId {
            HarnessId::Opencode
        }

        fn capabilities(&self) -> &HarnessCapabilities {
            &self.caps
        }
    }

    /// A pure plan with distinct, non-default limits so a dropping lowering is visible. No credential
    /// and no secret: every field is a number, an id, or a URL.
    fn plan() -> ResolvedProviderPlan {
        ResolvedProviderPlan {
            stable_id: "fireworks".to_string(),
            protocol: ProviderProtocol::OpenAiCompatible,
            normalized_endpoint: "https://api.fireworks.ai/inference/v1".to_string(),
            allow_insecure_http: false,
            credential_binding:
                "fireworks|openai-chat-completions-bearer-v1|https://api.fireworks.ai/inference/v1"
                    .to_string(),
            credential_ref: "keychain".to_string(),
            limits: ProviderLimits {
                forwarded_requests_per_turn: 7,
                denied_requests_before_revocation: 3,
                concurrent_upstream_requests_per_turn: 2,
                json_request_bytes: 1_000_000,
                aggregate_request_bytes_per_turn: 4_000_000,
                response_bytes_per_request: 2_000_000,
                aggregate_response_bytes_per_turn: 8_000_000,
                requested_output_tokens_per_request: 12_345,
                reserved_token_units_per_turn: 111_111,
                reserved_token_units_per_session: 222_222,
                capability_lifetime_ms: 1_900_000,
                max_reserved_token_units_per_utc_day: None,
            },
            model: "accounts/fireworks/models/x".to_string(),
            origins: crate::ProviderOrigins {
                provider: "ticket".to_string(),
                model: "ticket".to_string(),
            },
        }
    }

    /// Register a real broker session whose plan/binding matches [`plan`]'s lowered registration, so
    /// the move-only custody path is exercised against the broker's own validation rather than a
    /// stub.
    fn prepare(plan: &ResolvedProviderPlan) -> (PreparedProvider, Broker) {
        let broker = Broker::new(
            "http://127.0.0.1:0/v1",
            Arc::new(SystemClock::new()),
            Arc::new(OsRandom::new()),
        )
        .expect("broker");
        let registrar = broker.registrar();
        let registration_plan = lower_provider_plan(plan).expect("lowered");
        let binding = registration_plan.binding().expect("binding");
        let lease = BoundCredentialLease::new(binding, b"sk-fake-provider-key".to_vec())
            .expect("test lease");
        let policy = SessionPolicy::new(*registration_plan.limits()).expect("policy");
        let provider = prepare_provider(plan, &registrar, lease, policy).expect("prepare");
        (provider, broker)
    }

    fn spec(provider: Option<PreparedProvider>) -> PreparedHarnessSpec {
        PreparedHarnessSpec {
            harness: HarnessId::Opencode,
            model: Some("accounts/fireworks/models/x".to_string()),
            provider,
            knobs: HarnessKnobs::Opencode(crate::opencode::Config::default()),
        }
    }

    /// The legacy/native-login branch: Claude, no provider custody.
    fn legacy_spec() -> PreparedHarnessSpec {
        PreparedHarnessSpec {
            harness: HarnessId::Claude,
            model: Some("accounts/fireworks/models/x".to_string()),
            provider: None,
            knobs: HarnessKnobs::Claude(crate::claude::Config::default()),
        }
    }

    fn start(labels: LaunchContext) -> SessionStart {
        SessionStart {
            workspace_path: "/ws/MT-1".to_string(),
            issue: Issue {
                id: "1".into(),
                identifier: "MT-1".into(),
                ..Default::default()
            },
            transcript: None,
            launch: labels,
        }
    }

    /// The lowering carries EVERY field of the pure plan into the broker-owned plan — the mutation
    /// guard is dropping or re-defaulting any one field (e.g. the model or a limit) and watching this
    /// fail. Calling it twice is byte-identical: it is pure and re-derives nothing.
    #[test]
    fn lowering_carries_every_field_and_is_deterministic() {
        let p = plan();
        let got = lower_provider_plan(&p).expect("lowered");
        assert_eq!(got.stable_provider_id(), "fireworks");
        assert_eq!(got.protocol(), BrokerProtocol::OpenAiChatCompletions);
        assert_eq!(
            got.normalized_endpoint(),
            "https://api.fireworks.ai/inference/v1"
        );
        assert!(!got.allow_insecure_http());
        assert_eq!(got.model_id(), "accounts/fireworks/models/x");

        let l = got.limits();
        assert_eq!(l.max_forwarded_requests, 7);
        assert_eq!(l.max_denied_requests, 3);
        assert_eq!(l.max_concurrent_requests, 2);
        assert_eq!(l.max_request_bytes, 1_000_000);
        assert_eq!(l.max_request_bytes_turn, 4_000_000);
        assert_eq!(l.max_response_bytes, 2_000_000);
        assert_eq!(l.max_response_bytes_turn, 8_000_000);
        assert_eq!(l.max_output_tokens_request, 12_345);
        assert_eq!(l.max_reserved_tokens_turn, 111_111);
        assert_eq!(l.max_reserved_tokens_session, 222_222);
        assert_eq!(l.max_reserved_token_units_per_utc_day, None);
        assert_eq!(l.max_capability_lifetime, Duration::from_millis(1_900_000));

        // The optional UTC-day cap lowers verbatim too (it cannot be registered without a durable
        // authority, so it is exercised directly on the lowering rather than through a session).
        let mut capped = p.limits;
        capped.max_reserved_token_units_per_utc_day = Some(9_999);
        assert_eq!(
            lower_provider_limits(&capped).max_reserved_token_units_per_utc_day,
            Some(9_999)
        );

        let again = lower_provider_plan(&p).expect("lowered again");
        assert_eq!(again.model_id(), got.model_id());
        assert_eq!(again.limits(), got.limits());
    }

    /// The broker-owned plan's binding is the canonical credential binding the pure plan named, so a
    /// lease minted from the plan's identity matches — the one-crossing identity that PB7 reads.
    #[test]
    fn lowered_binding_matches_the_pure_plan_binding() {
        let p = plan();
        let lowered = lower_provider_plan(&p).expect("lowered");
        let expected = CredentialBinding::new(
            p.stable_id.clone(),
            BrokerProtocol::OpenAiChatCompletions,
            p.normalized_endpoint.clone(),
        )
        .expect("binding");
        assert!(
            lowered
                .binding()
                .expect("lowered binding")
                .fingerprint()
                .matches(&expected.fingerprint()),
            "the lowered binding must be the same canonical identity the pure plan named"
        );
    }

    /// An explicit provider on a harness whose adapter cannot consume the protocol refuses — it never
    /// falls back to the native-login path.
    #[test]
    fn prepared_provider_on_an_incompatible_harness_refuses() {
        let (provider, _broker) = prepare(&plan());
        let spec = PreparedHarnessSpec {
            harness: HarnessId::Claude,
            model: Some("m".to_string()),
            provider: Some(provider),
            knobs: HarnessKnobs::Claude(crate::claude::Config::default()),
        };
        let harness: Arc<dyn Harness> = Arc::new(Fake::new()); // Fake reports Claude
        let err =
            bridge_dispatch_runner(harness, spec).expect_err("claude has no provider adapter");
        assert_eq!(
            err,
            DispatchRefusal::UnsupportedProtocol {
                harness: HarnessId::Claude,
                protocol: ProviderProtocol::OpenAiCompatible,
            }
        );
    }

    /// A prepared spec whose harness disagrees with the supplied harness refuses rather than running
    /// on the wrong adapter.
    #[test]
    fn harness_mismatch_refuses() {
        let spec = spec(None);
        let harness: Arc<dyn Harness> = Arc::new(Fake::new()); // Claude, spec says Opencode
        let err = bridge_dispatch_runner(harness, spec).expect_err("mismatch");
        assert_eq!(
            err,
            DispatchRefusal::HarnessMismatch {
                configured: HarnessId::Claude,
                expected: HarnessId::Opencode,
            }
        );
    }

    /// An explicit provider with no exact model refuses (mirrors the selection contract).
    #[test]
    fn missing_model_refuses() {
        let (provider, _broker) = prepare(&plan());
        let mut spec = spec(Some(provider));
        spec.model = None;
        let harness: Arc<dyn Harness> = Arc::new(OpencodeFake::new());
        let err = bridge_dispatch_runner(harness, spec).expect_err("no model");
        assert_eq!(
            err,
            DispatchRefusal::MissingModel {
                provider: "fireworks".to_string()
            }
        );
    }

    /// The legacy bridge: no provider means no broker custody, and the launch context reaches the
    /// session through the ported setters — the byte-identical behavior an installation with no
    /// explicit provider keeps.
    #[tokio::test]
    async fn legacy_branch_applies_launch_context_and_holds_no_custody() {
        let fake = Arc::new(Fake::new());
        let spec = legacy_spec();
        let runner = bridge_dispatch_runner(fake.clone(), spec).expect("runner");
        assert!(!runner.is_brokered());

        let started = runner
            .start(start(LaunchContext {
                run_id: 7,
                review_head: Some("deadbeef".to_string()),
            }))
            .await
            .expect("start");
        assert!(
            started.broker_turns.is_none(),
            "the legacy branch owns no broker custody"
        );
        assert_eq!(fake.last_run_id(), Some(7));
        assert_eq!(fake.last_review_head(), Some("deadbeef".to_string()));
        assert_eq!(
            fake.last_model_override().map(|m| m.model),
            Some("accounts/fireworks/models/x".to_string())
        );
    }

    /// A zero run id and an absent/empty review head are no-ops on the legacy branch, exactly as the
    /// ported setters define.
    #[tokio::test]
    async fn legacy_branch_zero_id_and_no_review_are_noops() {
        let fake = Arc::new(Fake::new());
        let runner = bridge_dispatch_runner(fake.clone(), legacy_spec()).expect("runner");
        let _ = runner
            .start(start(LaunchContext {
                run_id: 0,
                review_head: None,
            }))
            .await
            .expect("start");
        assert_eq!(fake.last_run_id(), Some(0));
        assert_eq!(fake.last_review_head(), None);
    }

    /// The provider branch transfers custody ONCE: the started session owns the `BrokerSession` and
    /// the worker half receives the ledger receiver. The launch context is frozen before the session
    /// is built — no legacy setter runs (`Fake` records nothing).
    #[tokio::test]
    async fn brokered_start_splits_session_and_ledger_and_uses_no_late_setter() {
        let (provider, _broker) = prepare(&plan());
        let fake = Arc::new(OpencodeFake::new());
        let runner = bridge_dispatch_runner(fake.clone(), spec(Some(provider))).expect("runner");
        assert!(runner.is_brokered());

        let started = runner
            .start(start(LaunchContext {
                run_id: 42,
                review_head: Some("cafebabe".to_string()),
            }))
            .await
            .expect("start");
        assert!(
            started.broker_turns.is_some(),
            "the worker half must be returned explicitly"
        );
        assert_eq!(
            fake.inner.last_run_id(),
            None,
            "the provider path must NOT use a late run-id setter"
        );
        assert_eq!(
            fake.inner.last_review_head(),
            None,
            "the provider path must NOT use a late review-head setter"
        );
        assert_eq!(started.session.id(), "thread-fake-0");
    }

    /// Dropping the live brokered session revokes its custody: the worker's retained ledger receiver
    /// can no longer arm a turn. This is the "once the session exists, it owns revocation" half.
    #[tokio::test]
    async fn dropping_the_started_session_revokes_custody() {
        let (provider, _broker) = prepare(&plan());
        let runner = bridge_dispatch_runner(Arc::new(OpencodeFake::new()), spec(Some(provider)))
            .expect("runner");
        let started = runner
            .start(start(LaunchContext::default()))
            .await
            .expect("start");
        let StartedSession {
            session,
            mut broker_turns,
        } = started;
        let mut receiver = broker_turns.take().expect("ledger receiver");
        // The session is live first: arming succeeds.
        assert!(
            receiver.arm_turn(TurnMeta::without_deadline()).is_ok(),
            "a live brokered session arms its first turn"
        );
        // Drop the session (and its `BrokerSession`), then the retained receiver must observe the
        // revocation. (Capacity-one would otherwise refuse a second arm; revocation is the point.)
        drop(session);
        assert_eq!(
            receiver.arm_turn(TurnMeta::without_deadline()).unwrap_err(),
            rhapsody_provider_broker::BrokerError::SessionRevoked,
            "dropping the adapter session must revoke the broker session"
        );
    }

    /// Dropping a not-yet-consumed prepared provider (the `DispatchRunner` dropped before `start`)
    /// revokes the session: the separately retained ledger receiver can no longer arm. This is the
    /// "startup never happened, so the prepared session is revoked" half.
    #[test]
    fn dropping_an_unstarted_prepared_provider_revokes_custody() {
        let broker = Broker::new(
            "http://127.0.0.1:0/v1",
            Arc::new(SystemClock::new()),
            Arc::new(OsRandom::new()),
        )
        .expect("broker");
        let registration_plan = lower_provider_plan(&plan()).expect("lowered");
        let binding = registration_plan.binding().expect("binding");
        let lease =
            BoundCredentialLease::new(binding, b"sk-fake-provider-key".to_vec()).expect("lease");
        let policy = SessionPolicy::new(*registration_plan.limits()).expect("policy");
        let registration = broker
            .registrar()
            .register_session(registration_plan, lease, policy)
            .expect("register");
        // Split the registration: the ledger receiver is retained, the session goes into a prepared
        // provider that is dropped unstarted.
        let BrokerRegistration { session, ledgers } = registration;
        let provider = PreparedProvider {
            stable_id: "fireworks".to_string(),
            protocol: ProviderProtocol::OpenAiCompatible,
            access: Some(session),
            ledgers: None,
        };
        let mut receiver = ledgers;
        assert!(
            receiver.arm_turn(TurnMeta::without_deadline()).is_ok(),
            "the session is live before the drop"
        );
        drop(provider);
        assert_eq!(
            receiver.arm_turn(TurnMeta::without_deadline()).unwrap_err(),
            rhapsody_provider_broker::BrokerError::SessionRevoked,
            "dropping an unstarted prepared provider must revoke the session"
        );
    }

    /// The registry stores only non-secret metadata: a prepared provider's `Debug` must not carry a
    /// credential, and its stable id/protocol survive.
    #[test]
    fn prepared_provider_debug_is_secret_free() {
        let (provider, _broker) = prepare(&plan());
        let rendered = format!("{provider:?}");
        assert!(rendered.contains("fireworks"), "{rendered}");
        assert!(!rendered.contains("sk-fake-provider-key"), "{rendered}");
    }

    /// The factory preserves the resolved model AND makes it a live construction input: the resolved
    /// model is written into the harness's own knob block (the argv source). The mutation guard is
    /// dropping the model at the factory's construction-input call site — replacing the body of
    /// `resolved_knobs` with `spec.knobs.clone()` must red this, because `harness_from_knobs` builds
    /// the adapter from exactly this block.
    #[test]
    fn factory_applies_the_resolved_model_to_the_knobs() {
        let spec = PreparedHarnessSpec {
            harness: HarnessId::Opencode,
            model: Some("accounts/fireworks/models/x".to_string()),
            provider: None,
            knobs: HarnessKnobs::Opencode(crate::opencode::Config {
                model: "model-from-config".to_string(),
                ..Default::default()
            }),
        };
        // The exact block the factory builds the adapter from: under the mutation this still reads
        // "model-from-config", so a brokered run would run the config's model with no test noise.
        let HarnessKnobs::Opencode(config) = resolved_knobs(&spec) else {
            panic!("opencode spec must stay opencode");
        };
        assert_eq!(
            config.model, "accounts/fireworks/models/x",
            "the resolved model must reach the argv construction input"
        );

        let runner = build_dispatch_runner(spec).expect("factory builds from knobs");
        assert_eq!(runner.resolved_model(), Some("accounts/fireworks/models/x"));
    }

    /// A start failure returns no session AND revokes the prepared custody: the not-yet-consumed
    /// `DispatchRunner` is dropped by the `?` on the failed `start_session`, so its `BrokerSession`
    /// drops with it. The mutation guard is leaking that custody on the error path (e.g.
    /// `std::mem::forget(self.provider.take())` before returning the error): the separately retained
    /// ledger receiver would still arm, and this assertion would not see `SessionRevoked`.
    #[tokio::test]
    async fn start_failure_revokes_the_prepared_custody() {
        // Split the registration so the ledger receiver survives the failed start — that is the only
        // way to observe whether revocation happened.
        let broker = Broker::new(
            "http://127.0.0.1:0/v1",
            Arc::new(SystemClock::new()),
            Arc::new(OsRandom::new()),
        )
        .expect("broker");
        let registration_plan = lower_provider_plan(&plan()).expect("lowered");
        let binding = registration_plan.binding().expect("binding");
        let lease =
            BoundCredentialLease::new(binding, b"sk-fake-provider-key".to_vec()).expect("lease");
        let policy = SessionPolicy::new(*registration_plan.limits()).expect("policy");
        let registration = broker
            .registrar()
            .register_session(registration_plan, lease, policy)
            .expect("register");
        let BrokerRegistration { session, ledgers } = registration;
        let provider = PreparedProvider {
            stable_id: "fireworks".to_string(),
            protocol: ProviderProtocol::OpenAiCompatible,
            access: Some(session),
            ledgers: None,
        };

        let mut fake = OpencodeFake::new();
        fake.inner.start_err = Some(AgentError::Other("boom".to_string()));
        let runner = bridge_dispatch_runner(Arc::new(fake), spec(Some(provider))).expect("runner");
        let err = runner
            .start(start(LaunchContext::default()))
            .await
            .expect_err("start must fail");
        assert_eq!(err, AgentError::Other("boom".to_string()));

        let mut receiver = ledgers;
        assert_eq!(
            receiver.arm_turn(TurnMeta::without_deadline()).unwrap_err(),
            rhapsody_provider_broker::BrokerError::SessionRevoked,
            "a failed start must revoke the prepared broker session, not leak its custody"
        );
    }

    /// SURFACE SCAN: the raw reusable-key representation this slice replaces must not reappear as a
    /// real type in the agent crate's public surface. `ProviderAuth::ApiKey` was the P0 direct-key
    /// experiment's shipping type; the pure plan and the prepared provider must never carry a
    /// reusable key. (This module's own prose names the removed type, so only the definition/re-export
    /// sites are scanned.)
    #[test]
    fn no_raw_api_key_type_in_agent_source() {
        let root = env!("CARGO_MANIFEST_DIR");
        for file in ["src/lib.rs", "src/harness.rs"] {
            let path = format!("{root}/{file}");
            let source = std::fs::read_to_string(&path).expect("read source");
            assert!(
                !source.contains("ProviderAuth"),
                "{file} reintroduced ProviderAuth (the raw-key representation this slice removes)"
            );
        }
    }
}
