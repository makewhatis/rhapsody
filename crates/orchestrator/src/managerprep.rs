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
//! * the WHOLE thing runs off the orchestrator control task — the off-loop triage task for
//!   assignment and room turns, the review watcher's task for adjudication — and is bounded by
//!   `manager.timeout_ms`, so a hung manager can never stall dispatch.
//! * the manager never retains a session across invocations: each gets a fresh synthetic identity and
//!   drops its own session state, so a timed-out turn cannot leak its OpenCode session into the next
//!   decision.
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

use crate::reviewadjudicate::{
    AdjudicationRequest, ReviewAdjudicator, Verdict, adjudication_prompt, parse_verdict,
};
use crate::selection::{
    FieldSelection, ResolvedSelection, SelectionRefusal, resolve_manager_selection,
};
use crate::teamsears::{RoomArbiter, Target};
use crate::triage::{TriageArbiter, TriageDecision, TriageRequest};

/// A synthetic issue identifier a SINGLE manager invocation runs under. The manager has no ticket
/// worktree, but the OpenCode adapter keys its private per-session state directory (and its resume
/// record) on an issue identifier, so the manager gets one of its own — distinct from every real
/// ticket.
///
/// It is deliberately UNIQUE PER INVOCATION, not stable across turns (design §10.2: "an
/// independently prepared broker session and synthetic operation identifier, and drop[s] it after
/// the manager invocation"). A stable identifier let the resume machinery (`opencode::resume::select`,
/// STUDIO-1043) hand a timed-out manager turn's retained session to the NEXT manager turn — a
/// different ticket's assignment, or a room turn — which then resumed that session and was told to
/// continue its work. Every invocation now mints a fresh identifier, so no record can ever match a
/// later turn.
fn manager_invocation_identifier() -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // The pid, a monotonic clock and a per-process counter together: two invocations in one process
    // can never collide, and two daemons sharing a state root are separated by pid.
    format!("rhapsody-manager-{}-{nanos:x}-{seq:x}", std::process::id())
}

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
        let state_root = manager_state_root(&knobs);
        let spec = PreparedHarnessSpec {
            harness,
            model: Some(model),
            provider: Some(provider),
            knobs,
        };
        let runner =
            build_dispatch_runner(spec).map_err(|e| format!("manager_prepare_refused: {e}"))?;

        // The manager never retains a session across invocations: a fresh synthetic identifier (so
        // no earlier manager turn's resume record can match) is paired with a best-effort discard of
        // this invocation's own record and state directory once the turn is done.
        let identifier = manager_invocation_identifier();

        let start = rhapsody_agent::SessionStart {
            workspace_path,
            issue: manager_issue(identifier.clone()),
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
        // Drop the manager's own session state. The session's `Drop` runs after this (the box is
        // still alive), but by then the directory is gone and `persist_resume` refuses a missing
        // directory, so nothing is re-written.
        if !state_root.is_empty() {
            rhapsody_agent::opencode::resume::discard(&state_root, &identifier);
        }

        match outcome {
            Err(_) => Err(manager_timeout_reason(timeout)),
            Ok((_result, Some(e))) => Err(format!("manager turn failed: {e}")),
            Ok((result, None)) => Ok(result.result_text),
        }
    }
}

/// The manager's OpenCode state root, if its knobs are OpenCode. Empty for the (refused) Claude
/// provider branch, where there is no session state to discard.
fn manager_state_root(knobs: &HarnessKnobs) -> String {
    match knobs {
        HarnessKnobs::Opencode(cfg) => cfg.state_root.clone(),
        HarnessKnobs::Claude(_) => String::new(),
    }
}

/// The synthetic, ticket-less issue one manager invocation runs under (see
/// [`manager_invocation_identifier`]).
fn manager_issue(identifier: String) -> Issue {
    Issue {
        identifier,
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

/// Review adjudication (STUDIO-956) is a manager turn, so it runs through this SAME arbiter and its
/// resolved tuple. Before this the adjudicator was a hardcoded `claude -p` turn that took
/// `manager.model` verbatim, so an explicit OpenCode manager (whose model is an OpenCode model id)
/// was adjudicated by `claude --model <opencode-model>` on native Claude auth — a silent fallback to
/// another harness and auth source (STUDIO-989 review B3). Routing through `turn_text` gives an
/// explicit manager its own provider session, limits and diagnostics, and leaves an EMPTY manager
/// tuple on the legacy `claude -p` lane byte-for-byte as before (the request still carries the
/// command/model the composition root resolved, including the `review.model` fallback).
#[async_trait]
impl ReviewAdjudicator for ManagerArbiter {
    async fn adjudicate(&self, req: &AdjudicationRequest) -> Result<Verdict, String> {
        let turn = TriageRequest {
            command: req.command.clone(),
            billing_guard: req.billing_guard,
            tracker_api_key: req.tracker_api_key.clone(),
            model: req.model.clone(),
            timeout: req.timeout,
            prompt: adjudication_prompt(req),
        };
        parse_verdict(&self.turn_text(&turn).await?)
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

    /// An `AdjudicationRequest` shaped like the one the composition root builds: a Claude-lane
    /// `command` and a resolved `model`, whose provider lane (when the manager tuple is explicit)
    /// must ignore both.
    fn adjudication_request(pr: &str) -> AdjudicationRequest {
        AdjudicationRequest {
            pr: pr.to_string(),
            head: "be260a6b4366fac70fbc0e2dbabd9d51fe9d44e5".to_string(),
            rounds: 3,
            findings: vec!["alice asked for changes at a324d2d".to_string()],
            command: "/nonexistent/rhapsody-manager-test-claude".to_string(),
            billing_guard: false,
            tracker_api_key: String::new(),
            model: "accounts/fireworks/models/x".to_string(),
            timeout: Duration::from_millis(200),
        }
    }

    /// Writes a fake `claude` that records its own argv to a log and prints `payload`. The command
    /// is `bash <script>`, so the recorded `$*` is exactly the argument tail `run_turn` appended.
    fn fake_claude(dir: &crate::testsupport::TempDir, payload: &str) -> (String, String) {
        let script = dir.child("fake-claude.sh");
        let log = dir.child("argv.txt");
        let body = format!(
            "#!/usr/bin/env bash\nprintf '%s' \"$*\" > {0:?}\nprintf '%s\\n' {1:?}\n",
            log, payload,
        );
        std::fs::write(&script, body).expect("write fake claude");
        (format!("bash {script}"), log)
    }

    /// Writes an executable fake `opencode` 1.18.30 whose body runs for turns. `#!/bin/sh` and an
    /// absolute `/bin/sleep` keep it working under the probe's scrubbed (`env_clear`) environment.
    fn fake_opencode(dir: &crate::testsupport::TempDir, name: &str, body: &str) -> String {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        let script = dir.child(name);
        let tmp = dir.child(&format!("{name}.tmp"));
        {
            let mut f = std::fs::File::create(&tmp).expect("create fake opencode");
            writeln!(f, "#!/bin/sh").expect("shebang");
            write!(f, "{body}").expect("body");
            f.sync_all().expect("flush");
        }
        std::fs::rename(&tmp, &script).expect("publish");
        let mut perms = std::fs::metadata(&script).expect("stat").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).expect("chmod");
        script
    }

    /// Canonicalized path string (the launch containment invariant compares canonical paths; on
    /// macOS `/var` is a symlink to `/private/var`).
    fn canonical(path: &str) -> String {
        std::fs::canonicalize(path)
            .expect("canonicalize")
            .to_string_lossy()
            .into_owned()
    }

    /// OpenCode knobs for a real brokered manager turn over `command`, with the manager's own
    /// workspace and state roots.
    fn opencode_knobs(command: &str, workspace_root: &str, state_root: &str) -> HarnessKnobs {
        HarnessKnobs::Opencode(OpencodeConfig {
            command: command.to_string(),
            model: "accounts/fireworks/models/x".to_string(),
            workspace_root: workspace_root.to_string(),
            state_root: state_root.to_string(),
            turn_timeout: Duration::from_secs(30),
            ..Default::default()
        })
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
    /// manager can never stall the off-loop task, and the manager turn stays genuinely PARKED (it is
    /// not silently finished) while independent work proceeds. The deadline that makes even a
    /// misbehaving runner future cancellable is pinned separately by
    /// [`hanging_manager_turn_is_bounded_by_the_turn_timeout`]; the control-loop-level property that
    /// a hung model turn never delays dispatch is `triage::tests::a_hung_model_turn_does_not_delay_dispatch`.
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
        assert!(
            !task.is_finished(),
            "the manager turn must still be parked, not silently finished"
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

    // ── the legacy lane's argv is pinned (review B2) ─────────────────────────────────────────────

    /// The empty manager tuple's ASSIGNMENT turn is the pre-P8 `claude -p <prompt>` subprocess, byte
    /// for byte: the fake records its own argv, and an empty `manager.model` must add no `--model`.
    ///
    /// MUTATION: any change to the legacy lane's argv — a `--model`, an extra flag, a different
    /// command — reds here. This replaced a test that used a `/nonexistent` command and so could not
    /// see the argv at all (STUDIO-989 review B2).
    #[tokio::test]
    async fn empty_manager_legacy_lane_pins_the_claude_argv_for_a_decision() {
        let selection =
            resolve_manager_selection(&FieldSelection::default(), &registry(), DEADLINE)
                .expect("the default manager resolves");
        let dir = crate::testsupport::TempDir::new();
        let (command, log) = fake_claude(&dir, r#"{"identity":"jimmy","reason":"fits"}"#);
        // No source, no knobs: the legacy lane must not need either.
        let arbiter = ManagerArbiter::new(Ok(selection), None, None, String::from("/tmp"));
        let mut req = request("assign this ticket");
        req.command = command;

        let decision = arbiter
            .arbitrate(&req)
            .await
            .expect("the legacy lane decides");
        assert_eq!(decision.identity, "jimmy");
        let argv = std::fs::read_to_string(&log).expect("the fake recorded its argv");
        assert_eq!(
            argv, "-p assign this ticket",
            "the empty tuple's assignment argv is the legacy `claude -p <prompt>`: {argv:?}"
        );
        assert!(
            !argv.contains("--model"),
            "an empty manager.model must add no --model: {argv:?}"
        );
    }

    /// The same pin for the ROOM lane: an empty manager tuple's room reply goes through the same
    /// legacy subprocess with the same argv shape.
    #[tokio::test]
    async fn empty_manager_legacy_lane_pins_the_claude_argv_for_a_room_reply() {
        let selection =
            resolve_manager_selection(&FieldSelection::default(), &registry(), DEADLINE)
                .expect("the default manager resolves");
        let dir = crate::testsupport::TempDir::new();
        let (command, log) = fake_claude(
            &dir,
            r#"{"targets":[{"ticket":"STUDIO-1","intent":"ask"}]}"#,
        );
        let arbiter = ManagerArbiter::new(Ok(selection), None, None, String::from("/tmp"));
        let mut req = request("what is happening?");
        req.command = command;

        let targets = arbiter
            .resolve(&req)
            .await
            .expect("the legacy lane replies");
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].key, "STUDIO-1");
        let argv = std::fs::read_to_string(&log).expect("the fake recorded its argv");
        assert_eq!(
            argv, "-p what is happening?",
            "the empty tuple's room argv is the legacy `claude -p <prompt>`: {argv:?}"
        );
        assert!(!argv.contains("--model"), "{argv:?}");
    }

    // ── adjudication is a manager turn too (review B3) ───────────────────────────────────────────

    /// An explicit OpenCode manager adjudicates through its OWN tuple and broker session — not the
    /// hardcoded `claude --model <opencode-model>` turn the pre-fix adjudicator ran. The mutation
    /// guard is any path that sends the request's model to a Claude turn instead of the manager's
    /// resolved provider.
    #[tokio::test]
    async fn explicit_provider_manager_adjudicates_through_its_own_tuple() {
        let selection = explicit_manager();
        let plan = selection.provider.clone().expect("a plan");
        let (opened, _broker) = opened_custody(&plan);
        let source = Arc::new(ScriptedSource::answering(opened));
        let turn = Arc::new(FakeTurn::answering("SHIP"));
        let arbiter = ManagerArbiter::new(
            Ok(selection),
            Some(source.clone()),
            Some(opcode_knobs()),
            String::from("/tmp"),
        )
        .with_turn_runner(turn.clone());

        let verdict = arbiter
            .adjudicate(&adjudication_request("makewhatis/rhapsody#192"))
            .await
            .expect("the manager adjudicates");
        assert_eq!(verdict, Verdict::Ship);
        assert_eq!(
            source.opens(),
            1,
            "adjudication opened the manager's own custody"
        );
        assert_eq!(turn.calls(), 1);
        let seen = turn.seen();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].harness, HarnessId::Opencode);
        assert_eq!(seen[0].provider, "fireworks");
        // The OpenCode manager's own resolved model — NOT the request's model handed to claude.
        assert_eq!(seen[0].model, "accounts/fireworks/models/x");
        assert!(
            seen[0].prompt.contains("SHIP"),
            "the adjudication prompt was what the turn was asked: {}",
            seen[0].prompt
        );
    }

    /// An EMPTY manager tuple's adjudication stays on the legacy `claude -p` lane, exactly as before
    /// P8: the request's resolved command and model (including the `review.model` fallback computed
    /// at the composition root) reach the subprocess unchanged.
    #[tokio::test]
    async fn empty_manager_adjudication_stays_on_the_legacy_claude_lane() {
        let selection =
            resolve_manager_selection(&FieldSelection::default(), &registry(), DEADLINE)
                .expect("the default manager resolves");
        let dir = crate::testsupport::TempDir::new();
        let (command, log) = fake_claude(&dir, "ESCALATE: a human must decide");
        let arbiter = ManagerArbiter::new(Ok(selection), None, None, String::from("/tmp"));

        let mut req = adjudication_request("makewhatis/rhapsody#192");
        req.command = command;
        req.model = "claude-opus-5".to_string();
        let verdict = arbiter
            .adjudicate(&req)
            .await
            .expect("the legacy adjudication decides");
        assert_eq!(
            verdict,
            Verdict::Escalate {
                reason: "a human must decide".to_string()
            }
        );
        let argv = std::fs::read_to_string(&log).expect("the fake recorded its argv");
        assert!(
            argv.starts_with("--model claude-opus-5 -p "),
            "the empty tuple's adjudication argv is the legacy `--model M -p <prompt>`: {argv:?}"
        );
    }

    // ── a manager turn never resumes a prior manager invocation's session (review B1) ─────────────

    /// **The end-to-end guard for B1.** Two manager invocations through the REAL
    /// [`HarnessManagerTurn`] and a real broker custody: the first is cut off by `manager.timeout_ms`
    /// after it announces a session id, and the second (a different decision) must start COLD. Before
    /// the fix a stable `rhapsody-manager` identifier let `resolve_state` hand the first turn's
    /// retained session to the second, which then carried `-s <session>` and the "resuming" note —
    /// continuing ticket A's conversation while deciding ticket B. The fake `opencode` records the
    /// argv of each turn, and the assertions are on the SECOND turn's argv.
    #[tokio::test]
    async fn a_manager_turn_never_resumes_a_previous_invocations_session() {
        let selection = explicit_manager();
        let plan = selection.provider.clone().expect("a plan");
        let scripts = crate::testsupport::TempDir::new();
        let state_root = crate::testsupport::TempDir::new();
        let root = crate::testsupport::TempDir::new();
        let ws = canonical(&root.path);
        let state = canonical(&state_root.path);

        // Attempt 1: announces a session id, then never returns — the common manager failure.
        let log1 = scripts.child("argv-1.txt");
        let body1 = format!(
            r#"if [ "${{1:-}}" = "--version" ]; then printf '1.18.30\n'; exit 0; fi
printf '%s' "$*" >> {0:?}
printf '{{"type":"step_start","sessionID":"ses_manager_a","part":{{"type":"step-start"}}}}\n'
/bin/sleep 5
"#,
            log1
        );
        let script1 = fake_opencode(&scripts, "one.sh", &body1);
        let (opened_a, _broker_a) = opened_custody(&plan);
        let turn_a = PreparedManagerTurn {
            harness: HarnessId::Opencode,
            model: "accounts/fireworks/models/x".to_string(),
            provider: opened_a.provider,
            knobs: opencode_knobs(&script1, &ws, &state),
            workspace_path: ws.clone(),
            prompt: "TRIAGE TICKET-A".to_string(),
            timeout: Duration::from_millis(400),
        };
        let err = HarnessManagerTurn
            .run(turn_a)
            .await
            .expect_err("the first turn is cut off by the deadline");
        assert!(err.contains("manager.timeout_ms"), "{err}");

        // Attempt 2: a normal, complete turn for a DIFFERENT manager decision.
        let log2 = scripts.child("argv-2.txt");
        let body2 = format!(
            r#"if [ "${{1:-}}" = "--version" ]; then printf '1.18.30\n'; exit 0; fi
printf '%s' "$*" >> {0:?}
printf '{{"type":"step_start","sessionID":"ses_manager_b","part":{{"type":"step-start"}}}}\n'
printf '{{"type":"text","sessionID":"ses_manager_b","part":{{"type":"text","text":"SHIP"}}}}\n'
printf '{{"type":"step_finish","sessionID":"ses_manager_b","part":{{"type":"step-finish","reason":"stop","tokens":{{"total":1,"input":1,"output":0,"reasoning":0,"cache":{{"write":0,"read":0}}}}}}}}\n'
"#,
            log2
        );
        let script2 = fake_opencode(&scripts, "two.sh", &body2);
        let (opened_b, _broker_b) = opened_custody(&plan);
        let turn_b = PreparedManagerTurn {
            harness: HarnessId::Opencode,
            model: "accounts/fireworks/models/x".to_string(),
            provider: opened_b.provider,
            knobs: opencode_knobs(&script2, &ws, &state),
            workspace_path: ws.clone(),
            prompt: "TRIAGE TICKET-B".to_string(),
            timeout: Duration::from_secs(5),
        };
        let _ = HarnessManagerTurn.run(turn_b).await;

        let argv2 = std::fs::read_to_string(&log2).expect("attempt 2 reached its child");
        assert!(
            !argv2.contains("-s "),
            "a manager turn must never resume a previous manager invocation's session: {argv2}"
        );
        assert!(
            !argv2.contains("Resuming a cut-off attempt"),
            "and must never be told it is resuming: {argv2}"
        );
    }
}
