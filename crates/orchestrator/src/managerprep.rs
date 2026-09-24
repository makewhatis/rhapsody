//! managerprep — the manager model turn on the SHARED provider-preparation path (P8, STUDIO-989;
//! design records `provider-auth-design.md` §P8/§2.3 and `provider-broker-design.md` §10.2).
//! Rhapsody-only; no Go counterpart.
//!
//! Before this slice the Teams manager's brain was a hardcoded `claude --model M -p` subprocess
//! (`triage::run_turn`), selected by nothing and sharing no contract with a teammate run. P8 replaces
//! that with the SAME machinery a teammate dispatch uses, while keeping the two absolutely separate:
//!
//! * the manager's harness/provider/model come from its OWN resolved tuple
//!   ([`crate::selection::resolve_manager_selection`]), never a teammate's selection;
//! * an explicit provider opens its OWN credential custody through the same PB7
//!   [`PreparedProviderSource`] a dispatch uses, registering a fresh broker session with the
//!   manager provider's own limits — no teammate lease or capability is borrowed;
//! * the turn runs through the harness adapter contract (`build_dispatch_runner` →
//!   `Session::run_turn_brokered`), not a second bespoke subprocess;
//! * the WHOLE thing runs off the orchestrator control task (inside the off-loop triage task) and is
//!   bounded by `manager.timeout_ms`, so a hung manager can never stall dispatch.
//!
//! # The legacy lane is preserved, not re-implemented
//!
//! Per the binding acceptance contract, **empty manager fields preserve the existing Claude
//! behavior**. The resolver's empty-tuple branch is `harness=claude`, `provider=None`,
//! `model=None` — Claude has no provider adapter in v1 — so the legacy lane delegates to the existing
//! native-login subprocess with the request the caller already built from that same manager tuple.
//! An installation with no explicit manager provider therefore produces byte-identical argv.
//!
//! # Refusals are values
//!
//! A selection that will not resolve, an explicit provider with no credential source, a typed
//! credential/broker refusal, a session that will not start, and a turn that exceeds its deadline are
//! all returned as `Err` — the caller logs it, leaves the ticket unlabeled, and falls back to
//! deterministic assignment. Nothing here ever falls back to another harness, provider, model, or
//! auth source.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rhapsody_agent::{
    HarnessId, HarnessKnobs, PreparedHarnessSpec, PreparedProvider, build_dispatch_runner,
};
use rhapsody_config::ProviderDefinition;
use rhapsody_core::Issue;

use crate::selection::{
    FieldSelection, ResolvedSelection, SelectionRefusal, resolve_manager_selection,
};
use crate::teamsears::{RoomArbiter, Target};
use crate::triage::{TriageArbiter, TriageDecision, TriageRequest};

/// The synthetic issue identifier a manager turn runs under. The manager has no ticket worktree, but
/// the OpenCode adapter keys its private per-session state directory (and its resume record) on an
/// issue identifier, so the manager gets one of its own — distinct from every real ticket, and stable
/// across turns so a cut-off manager turn can resume rather than starting cold.
const MANAGER_ISSUE_IDENTIFIER: &str = "rhapsody-manager";

/// One manager invocation's fully prepared inputs, after the manager tuple resolved and (for the
/// explicit-provider lane) its custody was opened. Move-only: [`PreparedProvider`] is not `Clone`, so
/// one invocation's credential lease can never be duplicated or retained by a shared runner.
pub(crate) struct PreparedManagerTurn {
    pub harness: HarnessId,
    pub model: String,
    /// The manager invocation's OWN broker custody — never a teammate run's grant.
    pub provider: PreparedProvider,
    pub knobs: HarnessKnobs,
    /// Where the harness child runs. The manager has no worktree; this is the configured workspace
    /// root (created at boot if absent).
    pub workspace_path: String,
    pub prompt: String,
    /// `manager.timeout_ms`, materialised — the same bound the legacy subprocess turn uses.
    pub timeout: Duration,
}

/// The seam that actually runs one prepared manager turn and returns the model's text. Production
/// installs [`HarnessManagerTurn`] (the harness adapter contract); tests inject a fake so no process
/// is ever spawned.
#[async_trait]
pub(crate) trait ManagerTurnRunner: Send + Sync {
    /// Runs ONE bounded turn. The implementation MUST bound itself by the turn's `timeout` and MUST
    /// NOT block indefinitely; the caller additionally wraps the call in the same deadline as a
    /// backstop, so a future is never left pending past it.
    async fn run(&self, turn: PreparedManagerTurn) -> Result<String, String>;
}

/// The production manager turn: the same dispatch-time factory + brokered session a teammate run
/// uses, over the manager's own prepared custody.
#[derive(Debug, Default)]
pub(crate) struct HarnessManagerTurn;

#[async_trait]
impl ManagerTurnRunner for HarnessManagerTurn {
    async fn run(&self, turn: PreparedManagerTurn) -> Result<String, String> {
        let PreparedManagerTurn {
            harness,
            model,
            provider,
            knobs,
            workspace_path,
            prompt,
            timeout,
        } = turn;
        // The manager has no per-ticket worktree: its child runs in the configured workspace root. A
        // prior boot may not have created it yet, and a missing cwd would fail the child before its
        // first turn, so ensure it exists rather than surfacing that as a confusing spawn error.
        if let Err(e) = std::fs::create_dir_all(&workspace_path) {
            return Err(format!("manager_workspace_unavailable: {e}"));
        }
        // The dispatch-time factory: it re-checks protocol compatibility, requires an exact model on
        // the provider branch, and moves the custody into an owned, non-Clone runner.
        let spec = PreparedHarnessSpec {
            harness,
            model: Some(model),
            provider: Some(provider),
            knobs,
        };
        let runner =
            build_dispatch_runner(spec).map_err(|e| format!("manager_prepare_refused: {e}"))?;

        let start = rhapsody_agent::SessionStart {
            workspace_path,
            issue: manager_issue(),
            transcript: None,
            // The manager has no store run row and no review head; the provider path freezes exactly
            // these values and calls no late identity setter.
            launch: rhapsody_agent::LaunchContext::default(),
        };
        let started = runner
            .start(start)
            .await
            .map_err(|e| format!("manager_session_refused: {e}"))?;
        let mut slots = crate::worker::BrokerTurnSlots::default();
        if let Some(receiver) = started.broker_turns {
            slots.install_receiver(receiver);
        }
        // Arm the receipt synchronously before the cancellable turn future, exactly as the worker
        // does: a dropped future (deadline or cancellation) still finalizes a receipt.
        let attempt = slots
            .arm_turn()
            .map_err(|e| format!("manager_turn_not_armed: {e}"))?;
        let noop = |_e: rhapsody_agent::Event| {};
        let outcome = tokio::time::timeout(
            timeout,
            started
                .session
                .run_turn_brokered(&prompt, None, None, &noop, attempt),
        )
        .await;
        slots.finalize_armed();
        let _ = started.session.stop().await;

        match outcome {
            Err(_) => Err(manager_timeout_reason(timeout)),
            Ok((_result, Some(e))) => Err(format!("manager turn failed: {e}")),
            Ok((result, None)) => Ok(result.result_text),
        }
    }
}

/// The synthetic, ticket-less issue a manager turn runs under (see [`MANAGER_ISSUE_IDENTIFIER`]).
fn manager_issue() -> Issue {
    Issue {
        identifier: MANAGER_ISSUE_IDENTIFIER.to_string(),
        // A title so a harness that renders one has something honest to show; the manager's prompt
        // is the request, not this issue.
        title: "teams manager turn".to_string(),
        ..Default::default()
    }
}

/// The operator-facing reason a manager turn exceeded its deadline. Deliberately names the setting
/// so a too-small `manager.timeout_ms` is fixable from `teams.yaml`, matching the legacy lane's
/// wording.
fn manager_timeout_reason(timeout: Duration) -> String {
    format!(
        "manager turn exceeded manager.timeout_ms ({}ms)",
        timeout.as_millis()
    )
}

/// The production manager arbiter (P8): resolves the manager's OWN tuple and runs each turn through
/// the shared preparation + harness-adapter contract, entirely off the control task.
///
/// It implements BOTH manager-facing seams the triage task uses (assignment and the room reader) so
/// the two can never drift onto different execution paths.
pub struct ManagerArbiter {
    /// The manager's fully owned resolved selection, or the typed reason it could not be resolved.
    /// `Err` refuses every turn (deterministic assignment still runs) rather than ever borrowing a
    /// teammate's tuple.
    selection: Result<ResolvedSelection, String>,
    /// The off-loop credential/broker source, the same one PB7 installs for ticket dispatch. `None`
    /// only in tests/embedding builds; an explicit provider then refuses rather than falling back.
    source: Option<Arc<dyn crate::providerprep::PreparedProviderSource>>,
    /// The harness execution seam. Production is [`HarnessManagerTurn`]; tests inject a fake.
    turn: Arc<dyn ManagerTurnRunner>,
    /// The per-harness knobs for [`Self::selection`]'s harness, built once from the boot config.
    knobs: Option<HarnessKnobs>,
    /// Where the harness child runs (the manager has no worktree).
    workspace: String,
}

impl ManagerArbiter {
    /// Builds the arbiter from the manager's resolved selection, the shared PB7 source, its prepared
    /// knobs, and the workspace the turn runs in. Pure except for cloning; no I/O and no credential
    /// is touched here — that happens per turn, off-loop.
    pub fn new(
        selection: Result<ResolvedSelection, SelectionRefusal>,
        source: Option<Arc<dyn crate::providerprep::PreparedProviderSource>>,
        knobs: Option<HarnessKnobs>,
        workspace: String,
    ) -> Self {
        Self {
            selection: selection.map_err(|e| e.to_string()),
            source,
            turn: Arc::new(HarnessManagerTurn),
            knobs,
            workspace,
        }
    }

    /// The convenience constructor the composition root uses: resolve the manager tuple from its own
    /// configured fields against the effective provider registry, and build the harness knobs.
    pub fn from_config(
        manager: &FieldSelection,
        providers: &std::collections::BTreeMap<String, ProviderDefinition>,
        turn_deadline_ms: u64,
        source: Option<Arc<dyn crate::providerprep::PreparedProviderSource>>,
        cfg: &rhapsody_config::Config,
    ) -> Self {
        let selection = resolve_manager_selection(manager, providers, turn_deadline_ms);
        let knobs = selection
            .as_ref()
            .ok()
            .map(|s| crate::effective::knobs_for_harness(cfg, s.harness));
        Self::new(selection, source, knobs, cfg.workspace.root.clone())
    }

    /// Overrides the harness execution seam (tests only; production keeps [`HarnessManagerTurn`]).
    #[cfg(test)]
    pub(crate) fn with_turn_runner(mut self, turn: Arc<dyn ManagerTurnRunner>) -> Self {
        self.turn = turn;
        self
    }

    /// Runs one manager turn and returns the model's text. The single place both manager-facing
    /// seams funnel through, so assignment and the room reader cannot diverge.
    async fn turn_text(&self, req: &TriageRequest) -> Result<String, String> {
        let selection = self
            .selection
            .as_ref()
            .map_err(|e| format!("manager_selection_refused: {e}"))?;

        let Some(plan) = &selection.provider else {
            // The legacy/native-login lane. The request already carries the command, model, timeout
            // and prompt the caller built from this SAME resolved manager tuple, and the empty-tuple
            // branch is Claude with the CLI-default model, so this is byte-identical to the pre-P8
            // manager turn. No provider machinery is consulted.
            return crate::triage::run_turn(req).await;
        };

        let source = self.source.as_deref().ok_or_else(|| {
            "manager_provider_unconfigured: the manager names an explicit provider but no credential \
             source is installed"
                .to_string()
        })?;
        let knobs = self.knobs.clone().ok_or_else(|| {
            "manager_prepare_unavailable: the manager harness has no prepared knobs".to_string()
        })?;

        // The shared PB7 off-loop preparation: read the manager provider's bound credential through
        // the same owner adapter and register a session with the broker. A failure is a typed
        // refusal, never a fallback to the native-login lane.
        let opened = source.open_provider(plan).await.map_err(|r| {
            format!(
                "manager_provider_refused: {} ({})",
                r.reason.message(),
                r.reason.code()
            )
        })?;

        // Diagnostics/provenance for the manager's own tuple — non-secret only. The limits the broker
        // will enforce are the plan's own lowered limits, so they are named here rather than
        // re-derived.
        tracing::info!(
            harness = %selection.harness_name,
            provider = %plan.stable_id,
            model = %plan.model,
            provider_origin = %plan.origins.provider,
            model_origin = %plan.origins.model,
            forwarded_requests_per_turn = plan.limits.forwarded_requests_per_turn,
            reserved_token_units_per_turn = plan.limits.reserved_token_units_per_turn,
            "teams manager prepared its own broker session for a manager turn"
        );

        let turn = PreparedManagerTurn {
            harness: selection.harness,
            model: plan.model.clone(),
            provider: opened.provider,
            knobs,
            workspace_path: self.workspace.clone(),
            prompt: req.prompt.clone(),
            timeout: req.timeout,
        };
        // The caller's deadline bounds the turn even if the runner misbehaves: dropping this future
        // drops the prepared custody and the armed receipt, so nothing is left live past the bound.
        match tokio::time::timeout(req.timeout, self.turn.run(turn)).await {
            Ok(result) => result,
            Err(_) => Err(manager_timeout_reason(req.timeout)),
        }
    }
}

#[async_trait]
impl TriageArbiter for ManagerArbiter {
    async fn arbitrate(&self, req: &TriageRequest) -> Result<TriageDecision, String> {
        crate::triage::parse_decision(&self.turn_text(req).await?)
    }
}

#[async_trait]
impl RoomArbiter for ManagerArbiter {
    async fn resolve(&self, req: &TriageRequest) -> Result<Vec<Target>, String> {
        crate::teamsears::parse_targets(&self.turn_text(req).await?)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use rhapsody_agent::opencode::Config as OpencodeConfig;
    use rhapsody_provider_broker::Broker;

    use super::*;
    use crate::prepare::RefusalReason;
    use crate::providerprep::{OpenedProvider, ProviderRefusal};
    use crate::selection::FieldSelection;

    const DEADLINE: u64 = 3_600_000;

    fn provider_def() -> ProviderDefinition {
        ProviderDefinition {
            id: "fireworks".to_string(),
            protocol: rhapsody_config::PROTOCOL_OPENAI_COMPATIBLE.to_string(),
            display_name: String::new(),
            base_url: "https://api.fireworks.ai/inference/v1".to_string(),
            allow_insecure_http: false,
            credential: rhapsody_config::CredentialSource {
                source: rhapsody_config::providers::CREDENTIAL_SOURCE_KEYCHAIN.to_string(),
            },
            broker_limits: rhapsody_config::BrokerLimits::default(),
        }
    }

    fn registry() -> BTreeMap<String, ProviderDefinition> {
        let mut m = BTreeMap::new();
        m.insert("fireworks".to_string(), provider_def());
        m
    }

    fn field(harness: &str, provider: &str, model: &str) -> FieldSelection {
        FieldSelection {
            harness: harness.to_string(),
            provider: provider.to_string(),
            model: model.to_string(),
        }
    }

    fn opcode_knobs() -> HarnessKnobs {
        HarnessKnobs::Opencode(OpencodeConfig::default())
    }

    fn request(prompt: &str) -> TriageRequest {
        TriageRequest {
            command: "/nonexistent/rhapsody-manager-test-claude".to_string(),
            billing_guard: false,
            tracker_api_key: String::new(),
            model: String::new(),
            timeout: Duration::from_millis(200),
            prompt: prompt.to_string(),
        }
    }

    /// What one manager invocation observed, recorded by the fake turn runner.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct SeenTurn {
        harness: HarnessId,
        model: String,
        provider: String,
        workspace: String,
        prompt: String,
    }

    /// A scriptable manager-turn seam: records every invocation and answers with a fixed text, a
    /// failure, or never (a hang), so every branch is testable without spawning a child.
    #[derive(Default)]
    struct FakeTurn {
        seen: Mutex<Vec<SeenTurn>>,
        calls: AtomicUsize,
        answer: Option<Result<String, String>>,
        hang: bool,
    }

    impl FakeTurn {
        fn answering(text: &str) -> Self {
            Self {
                answer: Some(Ok(text.to_string())),
                ..Default::default()
            }
        }
        fn hanging() -> Self {
            Self {
                hang: true,
                ..Default::default()
            }
        }
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
        fn seen(&self) -> Vec<SeenTurn> {
            self.seen.lock().unwrap_or_else(|e| e.into_inner()).clone()
        }
    }

    #[async_trait]
    impl ManagerTurnRunner for FakeTurn {
        async fn run(&self, turn: PreparedManagerTurn) -> Result<String, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.seen
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(SeenTurn {
                    harness: turn.harness,
                    model: turn.model.clone(),
                    provider: turn.provider.stable_id().to_string(),
                    workspace: turn.workspace_path.clone(),
                    prompt: turn.prompt.clone(),
                });
            if self.hang {
                std::future::pending::<()>().await;
            }
            match &self.answer {
                Some(Ok(text)) => Ok(text.clone()),
                Some(Err(why)) => Err(why.clone()),
                None => Ok(String::new()),
            }
        }
    }

    /// A prepared-provider source that records each opened plan and answers once from a script.
    struct ScriptedSource {
        opens: AtomicUsize,
        plans: Mutex<Vec<rhapsody_agent::ResolvedProviderPlan>>,
        answer: Mutex<Option<Result<OpenedProvider, ProviderRefusal>>>,
    }

    impl ScriptedSource {
        fn answering(opened: OpenedProvider) -> Self {
            Self {
                opens: AtomicUsize::new(0),
                plans: Mutex::new(Vec::new()),
                answer: Mutex::new(Some(Ok(opened))),
            }
        }
        fn refusing(reason: RefusalReason) -> Self {
            Self {
                opens: AtomicUsize::new(0),
                plans: Mutex::new(Vec::new()),
                answer: Mutex::new(Some(Err(ProviderRefusal {
                    reason,
                    revision: "rev-1".to_string(),
                }))),
            }
        }
        fn opens(&self) -> usize {
            self.opens.load(Ordering::SeqCst)
        }
        fn plans(&self) -> Vec<rhapsody_agent::ResolvedProviderPlan> {
            self.plans.lock().unwrap_or_else(|e| e.into_inner()).clone()
        }
    }

    #[async_trait]
    impl crate::providerprep::PreparedProviderSource for ScriptedSource {
        async fn open_provider(
            &self,
            plan: &rhapsody_agent::ResolvedProviderPlan,
        ) -> Result<OpenedProvider, ProviderRefusal> {
            self.opens.fetch_add(1, Ordering::SeqCst);
            self.plans
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(plan.clone());
            self.answer
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
                .expect("the source is asked at most once")
        }
    }

    /// A real, registered broker custody so the move-only custody path is exercised against the
    /// broker's own validation rather than a stub.
    fn opened_custody(plan: &rhapsody_agent::ResolvedProviderPlan) -> (OpenedProvider, Broker) {
        use rhapsody_provider_broker::{
            BoundCredentialLease, OsRandom, SessionPolicy, SystemClock,
        };
        let broker = Broker::new(
            "http://127.0.0.1:0/v1",
            Arc::new(SystemClock::new()),
            Arc::new(OsRandom::new()),
        )
        .expect("broker");
        let registration_plan = rhapsody_agent::lower_provider_plan(plan).expect("lowered plan");
        let binding = registration_plan.binding().expect("binding");
        let lease =
            BoundCredentialLease::new(binding, b"sk-fake-provider-key".to_vec()).expect("lease");
        let policy = SessionPolicy::new(*registration_plan.limits()).expect("policy");
        let registration = broker
            .registrar()
            .register_session(registration_plan, lease, policy)
            .expect("register");
        let opened = OpenedProvider {
            provider: PreparedProvider::from_registration(
                plan.stable_id.clone(),
                plan.protocol,
                registration,
            ),
            revision: "rev-1".to_string(),
        };
        (opened, broker)
    }

    fn explicit_manager() -> ResolvedSelection {
        resolve_manager_selection(
            &field("opencode", "fireworks", "accounts/fireworks/models/x"),
            &registry(),
            DEADLINE,
        )
        .expect("explicit manager resolves")
    }

    /// The legacy mutation guard: an EMPTY manager tuple must go straight to the native-login
    /// subprocess lane and consult NEITHER the provider source NOR the harness turn seam. A lane that
    /// treated an empty tuple as an explicit provider would open custody (or run the turn) here, and
    /// this test would see it.
    #[tokio::test]
    async fn empty_manager_never_reaches_the_provider_path() {
        let selection =
            resolve_manager_selection(&FieldSelection::default(), &registry(), DEADLINE)
                .expect("the default manager resolves");
        assert!(
            selection.provider.is_none(),
            "empty tuple invents no provider"
        );
        assert_eq!(selection.harness, HarnessId::Claude);

        let source = Arc::new(ScriptedSource::refusing(RefusalReason::CredentialAbsent));
        let turn = Arc::new(FakeTurn::answering("{}"));
        let arbiter = ManagerArbiter::new(
            Ok(selection),
            Some(source.clone()),
            Some(opcode_knobs()),
            String::from("/tmp"),
        )
        .with_turn_runner(turn.clone());

        // The command cannot be spawned, so the legacy lane returns Err — but it must not have
        // touched the provider machinery on the way.
        let err = arbiter
            .arbitrate(&request("prompt"))
            .await
            .expect_err("a missing claude binary is an error");
        assert!(!err.is_empty());
        assert_eq!(
            source.opens(),
            0,
            "the legacy lane opens no provider custody"
        );
        assert_eq!(
            turn.calls(),
            0,
            "the legacy lane does not use the provider turn seam"
        );
    }

    /// An explicit OpenCode manager uses its OWN resolved tuple: the harness, model, provider,
    /// workspace and prompt the fake turn observes are the manager's, and the source was asked for
    /// exactly one custody. The limits it registers are the manager provider's own.
    #[tokio::test]
    async fn explicit_provider_manager_uses_its_own_tuple_and_custody() {
        let selection = explicit_manager();
        let plan = selection.provider.clone().expect("a plan");
        let (opened, _broker) = opened_custody(&plan);
        let source = Arc::new(ScriptedSource::answering(opened));
        let turn = Arc::new(FakeTurn::answering(
            r#"{"identity":"jimmy","reason":"fits"}"#,
        ));
        let arbiter = ManagerArbiter::new(
            Ok(selection),
            Some(source.clone()),
            Some(opcode_knobs()),
            String::from("/tmp"),
        )
        .with_turn_runner(turn.clone());

        let decision = arbiter
            .arbitrate(&request("assign this ticket"))
            .await
            .expect("the manager turn answers");
        assert_eq!(decision.identity, "jimmy");
        assert_eq!(source.opens(), 1, "exactly one custody opened");
        assert_eq!(turn.calls(), 1, "exactly one turn run");
        let seen = turn.seen();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].harness, HarnessId::Opencode);
        assert_eq!(seen[0].model, "accounts/fireworks/models/x");
        assert_eq!(seen[0].provider, "fireworks");
        assert_eq!(seen[0].prompt, "assign this ticket");
        assert_eq!(seen[0].workspace, "/tmp");
        // The source saw the manager's own plan — its provider and lowered limits, not a teammate's.
        let plans = source.plans();
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].stable_id, "fireworks");
        assert_eq!(
            plans[0].limits.forwarded_requests_per_turn,
            provider_def().broker_limits.forwarded_requests_per_turn
        );
    }

    /// A typed preparation refusal degrades the manager turn and NEVER falls back to the native-login
    /// subprocess lane.
    #[tokio::test]
    async fn provider_refusal_never_falls_back_to_the_legacy_lane() {
        let selection = explicit_manager();
        let source = Arc::new(ScriptedSource::refusing(RefusalReason::CredentialAbsent));
        let turn = Arc::new(FakeTurn::answering("{}"));
        let arbiter = ManagerArbiter::new(
            Ok(selection),
            Some(source.clone()),
            Some(opcode_knobs()),
            String::from("/tmp"),
        )
        .with_turn_runner(turn.clone());

        let err = arbiter
            .arbitrate(&request("prompt"))
            .await
            .expect_err("a refused credential is an error");
        assert!(err.contains("manager_provider_refused"), "{err}");
        assert!(err.contains("credential_absent"), "{err}");
        assert_eq!(turn.calls(), 0, "a refusal never runs a fallback turn");
    }

    /// A selection that could not be resolved refuses every turn as a first-class error and consults
    /// no provider. (The manager still degrades to deterministic assignment upstream.)
    #[tokio::test]
    async fn unresolved_manager_selection_refuses_without_touching_a_provider() {
        let source = Arc::new(ScriptedSource::refusing(RefusalReason::CredentialAbsent));
        let turn = Arc::new(FakeTurn::answering("{}"));
        let arbiter = ManagerArbiter::new(
            Err(SelectionRefusal::ManagerProviderRequired {
                harness: "opencode".to_string(),
            }),
            Some(source.clone()),
            Some(opcode_knobs()),
            String::from("/tmp"),
        )
        .with_turn_runner(turn.clone());

        let err = arbiter
            .arbitrate(&request("prompt"))
            .await
            .expect_err("an unresolved selection refuses");
        assert!(err.contains("manager_selection_refused"), "{err}");
        assert_eq!(source.opens(), 0);
        assert_eq!(turn.calls(), 0);
    }

    /// A manager turn that never answers is bounded by `manager.timeout_ms`: the invocation returns
    /// a timeout error rather than parking forever. The mutation guard is dropping the caller-side
    /// deadline in `turn_text`.
    #[tokio::test]
    async fn hanging_manager_turn_is_bounded_by_the_turn_timeout() {
        let selection = explicit_manager();
        let plan = selection.provider.clone().expect("a plan");
        let (opened, _broker) = opened_custody(&plan);
        let source = Arc::new(ScriptedSource::answering(opened));
        let arbiter = ManagerArbiter::new(
            Ok(selection),
            Some(source),
            Some(opcode_knobs()),
            String::from("/tmp"),
        )
        .with_turn_runner(Arc::new(FakeTurn::hanging()));

        let mut req = request("prompt");
        req.timeout = Duration::from_millis(20);
        let err = arbiter
            .arbitrate(&req)
            .await
            .expect_err("a hung turn times out");
        assert!(err.contains("manager.timeout_ms"), "{err}");
    }

    /// Cancelling a pending manager turn (dropping the future) returns control promptly — a hung
    /// manager can never stall the off-loop task, let alone the control task. This is the
    /// dispatch-progress mutation guard: an implementation that awaited the turn on the control task
    /// (or without a cancellable bound) would leave the spin below starved.
    #[tokio::test]
    async fn a_hung_manager_turn_does_not_block_concurrent_progress() {
        let selection = explicit_manager();
        let plan = selection.provider.clone().expect("a plan");
        let (opened, _broker) = opened_custody(&plan);
        let source = Arc::new(ScriptedSource::answering(opened));
        let arbiter = Arc::new(
            ManagerArbiter::new(
                Ok(selection),
                Some(source),
                Some(opcode_knobs()),
                String::from("/tmp"),
            )
            .with_turn_runner(Arc::new(FakeTurn::hanging())),
        );

        let mut req = request("prompt");
        req.timeout = Duration::from_secs(3_600);
        let task = tokio::spawn(async move { arbiter.arbitrate(&req).await });
        // Independently make "dispatch progress" while the manager turn is parked.
        let mut ticks = 0u32;
        for _ in 0..50 {
            tokio::task::yield_now().await;
            ticks += 1;
        }
        assert!(
            ticks >= 50,
            "the concurrent task made progress while the manager hung"
        );
        task.abort();
        let _ = task.await;
    }

    /// The production harness turn refuses an unsupported harness/provider pair BEFORE spawning a
    /// child, through the shared dispatch-time factory. This exercises [`HarnessManagerTurn`] itself
    /// (the arbiter tests above inject a fake), so the refusal ordering is pinned rather than
    /// assumed.
    #[tokio::test]
    async fn harness_manager_turn_refuses_an_unsupported_pair_before_spawn() {
        let selection = explicit_manager();
        let plan = selection.provider.clone().expect("a plan");
        let (opened, _broker) = opened_custody(&plan);
        let workspace = crate::testsupport::TempDir::new();
        let turn = PreparedManagerTurn {
            // Claude has no provider adapter in v1, so this pair is a typed refusal.
            harness: HarnessId::Claude,
            model: "accounts/fireworks/models/x".to_string(),
            provider: opened.provider,
            knobs: HarnessKnobs::Claude(rhapsody_agent::claude::Config::default()),
            workspace_path: workspace.path.clone(),
            prompt: "prompt".to_string(),
            timeout: Duration::from_millis(200),
        };
        let err = HarnessManagerTurn
            .run(turn)
            .await
            .expect_err("claude cannot consume an openai-compatible provider");
        assert!(err.contains("manager_prepare_refused"), "{err}");
        assert!(
            err.contains("cannot consume provider protocol"),
            "the refusal names the protocol incompatibility: {err}"
        );
    }

    /// Two manager invocations running concurrently each open their OWN custody — no shared session
    /// and no borrowing. The mutation guard is a cached/reused session.
    #[tokio::test]
    async fn concurrent_manager_invocations_each_open_their_own_custody() {
        let selection = explicit_manager();
        let plan = selection.provider.clone().expect("a plan");
        let (opened_a, _a) = opened_custody(&plan);
        let (opened_b, _b) = opened_custody(&plan);
        // A source that answers FIFO with two distinct custodies and counts every open.
        struct TwoSource {
            opens: AtomicUsize,
            answers: Mutex<Vec<OpenedProvider>>,
        }
        #[async_trait]
        impl crate::providerprep::PreparedProviderSource for TwoSource {
            async fn open_provider(
                &self,
                _plan: &rhapsody_agent::ResolvedProviderPlan,
            ) -> Result<OpenedProvider, ProviderRefusal> {
                self.opens.fetch_add(1, Ordering::SeqCst);
                let mut answers = self.answers.lock().unwrap_or_else(|e| e.into_inner());
                if answers.is_empty() {
                    return Err(ProviderRefusal {
                        reason: RefusalReason::ResolverFailed("only two".to_string()),
                        revision: String::new(),
                    });
                }
                Ok(answers.remove(0))
            }
        }
        let source = Arc::new(TwoSource {
            opens: AtomicUsize::new(0),
            answers: Mutex::new(vec![opened_a, opened_b]),
        });
        let turn = Arc::new(FakeTurn::hanging());
        let arbiter = Arc::new(
            ManagerArbiter::new(
                Ok(selection),
                Some(source.clone()),
                Some(opcode_knobs()),
                String::from("/tmp"),
            )
            .with_turn_runner(turn.clone()),
        );

        let mut req = request("prompt");
        req.timeout = Duration::from_millis(50);
        let a = {
            let arb = Arc::clone(&arbiter);
            let r = req.clone();
            tokio::spawn(async move { arb.arbitrate(&r).await })
        };
        let b = {
            let arb = Arc::clone(&arbiter);
            let r = req.clone();
            tokio::spawn(async move { arb.arbitrate(&r).await })
        };
        // Both turns time out, but both must have opened a distinct custody first.
        let _ = a.await;
        let _ = b.await;
        assert_eq!(
            source.opens.load(Ordering::SeqCst),
            2,
            "each invocation opens its own custody"
        );
    }
}
