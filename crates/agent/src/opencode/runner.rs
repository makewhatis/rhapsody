//! The opencode subprocess runner (STUDIO-902). Rhapsody-only.
//!
//! Spawns `opencode run --format json` once per turn in the run's worktree and maps its JSONL
//! stream to the normalized [`crate::Event`] vocabulary. It follows `crate::claude::runner`'s shape
//! deliberately — same process-group discipline, same `proctree::kill_tree` arming, same capped
//! stderr, same containment invariant — and departs from it only where the STUDIO-869 spike
//! measured a real difference:
//!
//! | | claude | opencode |
//! |---|---|---|
//! | prompt delivery | one stream-json message on stdin | a POSITIONAL argv argument |
//! | child stdin | ⚠️ HELD OPEN as the INF-250 operator mailbox | ⚠️ **CLOSED at start** |
//! | turn end | a terminal `result` line | `step_finish{reason:"stop"}`, then EOF |
//! | per-turn usage | authoritative, on the result line | per-STEP; the adapter SUMS |
//! | state directory | none | ⚠️ a PRIVATE `XDG_DATA_HOME` per session, or turns are LOST |
//! | billing guard | `apiKeySource == "none"` | none exists; see below |
//!
//! ⚠️ **stdin is closed at start, and that is measured, not assumed.** The spike drove opencode in
//! `drive.py`'s `arg` mode — prompt on argv, `p.stdin.close()` immediately (`harness-spike/README.md`'s
//! Provenance block records the invocation; `sandbox/drive.py` records the handling). So this
//! backend has no operator-message mailbox: a message that arrives mid-turn cannot be delivered,
//! and [`crate::Session::run_turn`]'s `messages` receiver is therefore drained but NOT written,
//! with a warning naming what was dropped. That is the honest reading of
//! `HarnessCapabilities::steering: BetweenTurns` — the console (design D7, slice 5) is what will
//! stop offering the affordance; until then a dropped message is loud in the log rather than
//! silent.
//!
//! **No billing guard, on purpose.** Claude's guard exists to force subscription billing and is
//! verified from `apiKeySource` on each `system/init`; opencode emits no such signal and the entire
//! point of this backend is to bill a DIFFERENT provider deliberately. So the billing env scrub is
//! not applied here — scrubbing provider variables could silently unauthenticate a provider an
//! operator configured by environment. ⚠️ The TRACKER credential scrub still is, by name AND by
//! value, exactly as for claude (design §15.5): withholding the Linear key from the agent is not a
//! billing decision.

use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use rhapsody_core::Issue;
use rhapsody_provider_broker::BrokerTurnAttempt;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::claude::{TRACKER_ENV_VARS, append_me_env, append_review_env, scrub_env, split_command};
use crate::harness::{
    EventFidelity, Harness, HarnessCapabilities, HarnessId, McpSandboxCoupling, Resume, Sandbox,
    StdinPolicy, Steering, ToolEventGranularity, ToolNaming, UsageDetail,
    check_brokered_opencode_controls,
};
use crate::opencode::args::{Config, build_args};
use crate::opencode::brokered::{
    BrokeredMaterial, CapabilityRedactor, InternalProviderId, build_brokered_args,
};
use crate::opencode::mcpinject::{
    SERVER_KEY, daemon_mcp_server, inject_daemon_mcp, rewrite_tool_names,
};
use crate::opencode::parse::{Failure, add_usage, classify};
use crate::opencode::probe;
use crate::opencode::resume::{self, ResumeDecision, ResumeRecord};
use crate::opencode::state::RunState;
use crate::proctree::{KillTreeOnDrop, kill_tree};
use crate::{
    AgentError, EVENT_NOTIFICATION, EVENT_SESSION_STARTED, EVENT_STARTUP_FAILED,
    EVENT_TURN_COMPLETED, EVENT_TURN_FAILED, Event, Session, TURN_FAILED, TURN_SUCCEEDED,
    TURN_TIMED_OUT, Transcript, TurnResult, Usage,
};

/// Mirrors the claude runner's bounds so a pathological stream is capped the same way on both.
const MAX_STDOUT_LINE: usize = 10 * 1024 * 1024;
const MAX_STDERR_CAPTURE: usize = 256 * 1024;
const READ_CHUNK: usize = 64 * 1024;
const MAX_STDERR_MESSAGE: usize = 2048;
/// Bounds the final result text (the `HANDOFF:` marker is at the END, so the TAIL is kept).
const MAX_RESULT_TEXT: usize = 4096;

/// Prepended to the FIRST turn's prompt when the session was resumed from a retained directory
/// (STUDIO-1043), so the agent knows its prior investigation is intact and continues rather than
/// restarting. The session history itself is already open to it.
const RESUME_NOTE: &str = "[Resuming a cut-off attempt: this continues the same OpenCode session \
from a previous interrupted run of this ticket. Your earlier work and the session history are \
intact — continue from where you left off rather than starting over. If you had completed an \
investigation and recorded findings, act on them now.]";

/// Builds opencode sessions.
pub struct Runner {
    cfg: Config,
}

impl Runner {
    /// Materializes the zero-value defaults the config layer does not supply: an empty command
    /// becomes `"opencode"`, and a zero turn timeout becomes one hour.
    ///
    /// ⚠️ The turn-timeout default is deliberately the SAME materialization
    /// [`crate::claude::Runner::new`] does, because the value is reachable from config and the two
    /// backends must not read one operator input two different ways. `decode` defaults an ABSENT
    /// `opencode.turn_timeout_ms` to 3600000, but an EXPLICIT `0` (the natural way to write "no
    /// limit", and where a negative value also lands via `.max(0)` in the orchestrator's config
    /// mapping) arrives here as [`Duration::ZERO`] — which would make the turn deadline already
    /// expired when the `select!` is entered, killing every turn of every run and reporting it as
    /// `turn_timeout`.
    pub fn new(mut cfg: Config) -> Runner {
        if cfg.command.is_empty() {
            cfg.command = "opencode".to_string();
        }
        if cfg.turn_timeout.is_zero() {
            cfg.turn_timeout = Duration::from_secs(3600);
        }
        Runner { cfg }
    }

    /// Resolves the private state directory for a dispatch of `issue`, preferring a session
    /// retained by a cut-off earlier attempt (STUDIO-1043).
    ///
    /// A matching record is adopted, and its opencode session id returned so the first turn can
    /// continue it (`-s <id>`). Otherwise `provision` makes a fresh directory — the cold start
    /// this backend has always done — and the reason a record was rejected is logged exactly once.
    ///
    /// The resume key is the harness-config model (`self.cfg.model`). A per-teammate `--model`
    /// override (`Session::set_model_override`, applied after this call) is deliberately NOT part
    /// of the key: the override changes the `-m` of the resumed turn, but opencode continues the
    /// session regardless, and folding an override in would make a teammate's own retry unable to
    /// resume itself.
    fn resolve_state(
        &self,
        workspace_path: &str,
        issue: &Issue,
        provision: impl FnOnce() -> Result<RunState, AgentError>,
    ) -> Result<(RunState, String, bool), AgentError> {
        let now = Utc::now().timestamp_millis();
        match resume::select(
            &self.cfg.state_root,
            &issue.identifier,
            resume::HARNESS,
            &self.cfg.model,
            workspace_path,
            now,
        ) {
            ResumeDecision::Resume(rec) => {
                let state = RunState::adopt(PathBuf::from(&rec.dir))?;
                tracing::info!(
                    issue = %issue.identifier, session = %rec.session_id, dir = %rec.dir,
                    "opencode: resuming the retained session of a cut-off attempt"
                );
                Ok((state, rec.session_id, true))
            }
            ResumeDecision::Cold(reason) => {
                if !reason.is_empty() {
                    tracing::info!(
                        issue = %issue.identifier, reason = %reason,
                        "opencode: not resuming a previous attempt; starting a cold session"
                    );
                }
                provision().map(|state| (state, String::new(), false))
            }
        }
    }

    /// The brokered preparation sequence (see [`start_brokered_session`]). Every refusal here is
    /// typed and happens before a credential could be read: brokered mode never reads one, and the
    /// version probe is credential-free.
    fn start_brokered(
        &self,
        workspace_path: &str,
        issue: Issue,
        transcript: Option<Transcript>,
    ) -> Result<Box<dyn Session>, AgentError> {
        if self.cfg.model.is_empty() {
            return Err(AgentError::Other(
                "opencode_model_missing: brokered mode requires an exact selected model"
                    .to_string(),
            ));
        }
        // The brokered compatibility row refuses unsupported agent/variant/approval knobs, a
        // non-empty extra_args, and a command that is not exactly one executable — all BEFORE any
        // probe or credential contact (`§9.2`).
        check_brokered_opencode_controls(
            &self.cfg.agent,
            &self.cfg.variant,
            self.cfg.auto_approve,
            &self.cfg.extra_args,
            &self.cfg.command,
        )
        .map_err(|e| AgentError::Other(format!("unsupported_brokered_opencode: {e}")))?;

        // A brokered command is exactly one executable; `split_command` must therefore yield no
        // embedded base arguments. Checked (not asserted) so the invariant is a typed refusal even
        // if the compatibility check above ever drifts.
        let (name, base_args) = split_command(&self.cfg.command)?;
        if !base_args.is_empty() {
            return Err(AgentError::Other(
                "unsupported_brokered_opencode: command must be exactly one executable".to_string(),
            ));
        }

        // The exact supported version is probed before anything is provisioned (`§9.1`). Unknown,
        // unparseable, or unreachable versions refuse with their own typed reason.
        probe::probe(&self.cfg.command)
            .map_err(|e| AgentError::Other(format!("{}: {}", e.reason(), e.message())))?;

        // A credential-FREE private state directory: no auth.json is created or copied. A retained
        // brokered session is resumed from its kept directory (STUDIO-1043), which likewise holds
        // no reusable credential — only what the per-turn capabilities left behind.
        let (state, resume_session, adopted) =
            self.resolve_state(workspace_path, &issue, || {
                RunState::provision_brokered(
                    &self.cfg.state_root,
                    &issue.identifier,
                    &self.cfg.workspace_root,
                )
            })?;
        // A fresh brokered provision creates it; an adopted one may predate it.
        state.ensure_config_dir()?;
        let provider_id = InternalProviderId::generate()?;

        Ok(Box::new(OpencodeSession {
            cfg: self.cfg.clone(),
            cmd_name: name,
            cmd_args: base_args,
            ws_path: workspace_path.to_string(),
            issue,
            state,
            config_path: String::new(),
            brokered: Some(BrokeredParams {
                provider_id,
                model: self.cfg.model.clone(),
                command: self.cfg.command.clone(),
            }),
            session_id: Mutex::new(resume_session),
            turn_n: AtomicI64::new(0),
            transcript: Mutex::new(transcript),
            transcript_warned: AtomicBool::new(false),
            run_id: AtomicI64::new(0),
            review_head: Mutex::new(String::new()),
            model_override: Mutex::new(crate::ModelOverride::default()),
            poisoned: AtomicBool::new(false),
            adopted,
            last_turn_failed: AtomicBool::new(false),
        }))
    }
}

/// One brokered session's non-secret, per-session invariants (PB6): the internal provider id stable
/// for this session, the resolved exact model, and the trusted single-executable command whose
/// version is re-probed before every turn.
struct BrokeredParams {
    provider_id: InternalProviderId,
    model: String,
    command: String,
}

/// Holds one turn's live [`TurnAccess`] and pins its revocation ordering. Declared AFTER the turn's
/// `KillTreeOnDrop`, so a cancelled turn drops this first: the capability is revoked before
/// process-tree teardown can block. The explicit [`BrokerTurnGuard::finish`]/[`BrokerTurnGuard::revoke`]
/// calls pin the same ordering on the normal path.
struct BrokerTurnGuard {
    access: Option<rhapsody_provider_broker::TurnAccess>,
}

impl BrokerTurnGuard {
    /// Declare normal completion. The following drop finalizes the receipt as `completed`.
    fn finish(&mut self) {
        if let Some(access) = self.access.take() {
            access.finish();
        }
    }

    /// Revoke the capability now (turn error, timeout, or Stop) rather than waiting for drop.
    fn revoke(&mut self) {
        drop(self.access.take());
    }
}

impl Drop for BrokerTurnGuard {
    fn drop(&mut self) {
        // A guard dropped without an explicit finish/revoke (a cancelled future) still revokes.
        self.revoke();
    }
}

/// One turn's teardown, owning its capability guard and its process-tree guard. Its [`Drop`]
/// delegates to [`teardown_turn`], which revokes the capability BEFORE the process-tree kill can
/// block (`§9.4`) — a property pinned by a unit test, not by the field declaration order a comment
/// used to describe (the old code's two `let` bindings could be reordered with every test green).
struct TurnTeardown {
    access: BrokerTurnGuard,
    tree: KillTreeOnDrop,
}

impl TurnTeardown {
    /// Declare normal completion. The following drop finalizes the receipt as `completed`.
    fn finish(&mut self) {
        self.access.finish();
    }

    /// Revoke the capability now (turn error, timeout, or Stop) rather than waiting for drop.
    fn revoke(&mut self) {
        self.access.revoke();
    }

    /// Stand the process-tree kill down once the child has been reaped (the normal path).
    fn disarm_tree(&mut self) {
        self.tree.disarm();
    }
}

impl Drop for TurnTeardown {
    fn drop(&mut self) {
        // Take both guards out and run them through the one ordering function, so the
        // revoke-then-kill order is a tested property rather than an implicit field-drop order.
        let access = std::mem::replace(&mut self.access, BrokerTurnGuard { access: None });
        let tree = std::mem::replace(&mut self.tree, KillTreeOnDrop::new(0));
        teardown_turn(|| drop(access), || drop(tree));
    }
}

/// Runs the two teardown actions in the pinned order (`§9.4`): revoke the turn capability FIRST,
/// then tear down the process tree. [`KillTreeOnDrop`]'s kill shells out to `ps` and can block for
/// the sweep's whole bounded budget; a capability left live across that window is replayable. Kept a
/// pure function so a unit test can pin the order with two recording closures — swapping the two
/// lines reds the test and flips the production order, because [`TurnTeardown::drop`] delegates here.
fn teardown_turn(revoke: impl FnOnce(), kill_tree: impl FnOnce()) {
    revoke();
    kill_tree();
}

/// Prepares a BROKERED OpenCode session (STUDIO-1001 / PB6).
///
/// Fail-closed ordering, all before any credential contact: unsupported brokered knobs refuse, then
/// the exact supported OpenCode version is probed, then a credential-free private state directory
/// (no `auth.json`) is provisioned and the per-session internal provider id is minted. The returned
/// session consumes a per-turn `BrokerTurnAttempt` through [`Session::run_turn_brokered`].
pub async fn start_brokered_session(
    cfg: Config,
    workspace_path: &str,
    issue: Issue,
    transcript: Option<Transcript>,
) -> Result<Box<dyn Session>, AgentError> {
    Runner::new(cfg).start_brokered(workspace_path, issue, transcript)
}

/// opencode's declared capabilities, each matching what this file actually does and what the
/// STUDIO-869 spike measured, never the CLI's documentation.
///
/// * `events` / `tool_naming` — `[RAN]`: typed `read`/`edit`/`bash` tool events (so `FileLevel`,
///   the granularity codex cannot reach), and `symphony_symphony_state` in
///   `harness/harness-spike/opencode/happy.jsonl` (so `ServerUnderscoreTool`).
/// * `steering: BetweenTurns` — stdin is closed at start; there is no mid-turn channel (module doc).
/// * `resume: Flags` — `-s <sessionID>`, an unchanged flag on an otherwise-normal invocation, which
///   is the same shape claude's `--resume` has and the one `resume.jsonl` was captured with.
/// * `mcp: true` — `[RAN]`, the daemon's own tool was called in the happy capture.
/// * `sandbox: None` — opencode has permission approval (`--auto`), not a sandbox mode or a tool
///   allowlist, so neither existing variant describes it and `None` is the honest value. The
///   mcp/sandbox exclusivity that `crate::harness` defers to slice 5 is a codex constraint and does
///   not apply: this harness honours both at once, as the capture proves.
/// * `usage: TokensAndCost` — `[RAN]` `step_finish.part.cost` alongside a full token breakdown.
///   As with claude, the cost half is declared but not yet extracted: `crate::Usage` has no cost
///   field, and adding one is slice 7 / §7.4's spend-budget work, not this adapter's.
/// * `budgets: false` — the turn deadline below is the daemon's, not a CLI-enforced budget;
///   opencode has no budget flag at all (design §7.2).
/// * `stdin: ClosedAtStart` — the measured difference from claude (module doc).
pub(crate) const CAPABILITIES: HarnessCapabilities = HarnessCapabilities {
    events: EventFidelity::Structured {
        tool_level: ToolEventGranularity::FileLevel,
    },
    steering: Steering::BetweenTurns,
    resume: Resume::Flags,
    mcp: true,
    sandbox: Sandbox::None,
    mcp_sandbox: McpSandboxCoupling::Independent,
    usage: UsageDetail::TokensAndCost,
    budgets: false,
    tool_naming: ToolNaming::ServerUnderscoreTool,
    stdin: StdinPolicy::ClosedAtStart,
};

impl Harness for Runner {
    fn id(&self) -> HarnessId {
        HarnessId::Opencode
    }

    fn capabilities(&self) -> &HarnessCapabilities {
        &CAPABILITIES
    }

    /// The brokered materialization (PB7, STUDIO-1002): provision a credential-FREE run state and a
    /// managed session whose turns mint a per-turn capability from the broker attempt. This is the
    /// one entry point a prepared provider dispatch uses; it never reads a harness-native credential.
    fn start_brokered_session(
        &self,
        workspace_path: &str,
        issue: Issue,
        transcript: Option<Transcript>,
    ) -> Result<Box<dyn Session>, AgentError> {
        self.start_brokered(workspace_path, issue, transcript)
    }
}

#[async_trait]
impl crate::Runner for Runner {
    /// ⚠️ Provisions the session's private state directory BEFORE returning, so a missing
    /// credential is refused here — at dispatch, with a message naming the fix — rather than
    /// surfacing mid-turn as a 401 indistinguishable from a real provider problem. This is the
    /// "fail loudly and early" path this backend has until slice 5's refusal path exists.
    async fn start_session(
        &self,
        workspace_path: &str,
        issue: Issue,
        transcript: Option<Transcript>,
    ) -> Result<Box<dyn Session>, AgentError> {
        let (name, base_args) = split_command(&self.cfg.command)?;
        // A retained session is resumed from its kept directory (which already holds the seeded
        // credential); otherwise a fresh one is provisioned, refusing a missing credential before
        // anything is spawned.
        let (state, resume_session, adopted) =
            self.resolve_state(workspace_path, &issue, || {
                RunState::provision_legacy(
                    &self.cfg.state_root,
                    &self.cfg.auth_source,
                    &issue.identifier,
                    &self.cfg.workspace_root,
                )
            })?;

        // MCP injection is best-effort, exactly as it is for claude: on any failure the run
        // proceeds without the daemon's server rather than not running at all.
        let mut config_path = String::new();
        if self.cfg.inject_mcp {
            match inject_daemon_mcp(
                state.xdg_data_home(),
                workspace_path,
                &self.cfg.daemon_bin,
                &self.cfg.workflow_path,
            ) {
                Ok(Some(path)) => config_path = path.to_string_lossy().into_owned(),
                Ok(None) => tracing::warn!(
                    issue = %issue.identifier,
                    "mcp injection: the workspace already defines a `symphony` MCP server; keeping theirs"
                ),
                Err(e) => tracing::warn!(
                    issue = %issue.identifier, err = %e,
                    "mcp injection failed; proceeding without the daemon's MCP server"
                ),
            }
        }

        Ok(Box::new(OpencodeSession {
            cfg: self.cfg.clone(),
            cmd_name: name,
            cmd_args: base_args,
            ws_path: workspace_path.to_string(),
            issue,
            state,
            config_path,
            brokered: None,
            session_id: Mutex::new(resume_session),
            turn_n: AtomicI64::new(0),
            transcript: Mutex::new(transcript),
            transcript_warned: AtomicBool::new(false),
            run_id: AtomicI64::new(0),
            review_head: Mutex::new(String::new()),
            model_override: Mutex::new(crate::ModelOverride::default()),
            poisoned: AtomicBool::new(false),
            adopted,
            last_turn_failed: AtomicBool::new(false),
        }))
    }
}

/// One live opencode conversation for one issue.
struct OpencodeSession {
    cfg: Config,
    cmd_name: String,
    cmd_args: Vec<String>,
    ws_path: String,
    issue: Issue,
    /// The private `XDG_DATA_HOME` for this session. Dropping the session removes it.
    state: RunState,
    /// The injected `OPENCODE_CONFIG` path; empty when nothing was injected.
    config_path: String,
    /// `Some` for a brokered session (PB6), `None` for the legacy native-login path. An explicit
    /// provider selects brokered mode; a legacy run always selects legacy — the two never infer
    /// their behavior from an empty path (`§9.1`).
    brokered: Option<BrokeredParams>,
    /// opencode's `ses_…` id, captured from the first line that carries one.
    session_id: Mutex<String>,
    turn_n: AtomicI64,
    transcript: Mutex<Option<Transcript>>,
    transcript_warned: AtomicBool,
    run_id: AtomicI64,
    review_head: Mutex<String>,
    model_override: Mutex<crate::ModelOverride>,
    /// Set when a re-probe between turns finds the command changed: the already prepared session is
    /// dropped (its state removed) and every later turn refuses before minting or spawning (`§9.1`).
    poisoned: AtomicBool,
    /// Whether this session was adopted from a retained record (`resolve_state`). An adopted session
    /// that ends BEFORE its first turn leaves the retained directory and record untouched rather than
    /// deleting the very session the resume feature exists to keep (STUDIO-1043).
    adopted: bool,
    /// Whether the most recent turn ended in a retryable failure (STUDIO-1043). Set `true` as a
    /// turn begins and cleared only on the successful return, so every early return and a cancelled
    /// future leave it set. [`OpencodeSession::end_of_run`] retains the state directory and writes
    /// the resume record when it is set, and cleans up when it is not.
    last_turn_failed: AtomicBool,
}

impl OpencodeSession {
    /// A poisoned lock is recovered from rather than propagated: a panic in another thread must not
    /// turn into a panic here, on a production path. Mirrors the claude session's handling.
    fn locked_session_id(&self) -> std::sync::MutexGuard<'_, String> {
        self.session_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn locked_review_head(&self) -> std::sync::MutexGuard<'_, String> {
        self.review_head
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn locked_model_override(&self) -> std::sync::MutexGuard<'_, crate::ModelOverride> {
        self.model_override
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// This turn's config, with the routed teammate's profile model/effort applied (STUDIO-868).
    /// An empty override field inherits the installation-wide value, so a teammate whose profile
    /// names neither produces a byte-identical argv.
    ///
    /// `effort` maps onto opencode's `--variant`, its provider-specific reasoning-effort knob —
    /// the counterpart of `claude --effort`, under a different flag name.
    fn turn_cfg(&self) -> Config {
        let over = self.locked_model_override().clone();
        let mut cfg = self.cfg.clone();
        if !over.model.is_empty() {
            cfg.model = over.model;
        }
        if !over.effort.is_empty() {
            cfg.variant = over.effort;
        }
        cfg
    }

    /// Names whose model a turn ran on, for a failure that the CLI's own stderr attributes to
    /// nobody. Empty for every run with no override — see the claude session's equivalent.
    fn model_attribution(&self) -> String {
        let over = self.locked_model_override();
        if over.is_empty() || over.identity.is_empty() {
            return String::new();
        }
        format!(
            "{}'s profile asked for model={:?} effort={:?}: ",
            over.identity, over.model, over.effort
        )
    }

    fn tee_stdout(&self, line: &[u8]) {
        let mut guard = self
            .transcript
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(t) = guard.as_mut()
            && let Some(w) = t.stdout.as_mut()
            && (w.write_all(line).is_err() || w.write_all(b"\n").is_err())
        {
            drop(guard);
            self.warn_transcript_once();
        }
    }

    fn tee_stderr(&self, chunk: &[u8]) {
        let mut guard = self
            .transcript
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(t) = guard.as_mut()
            && let Some(w) = t.stderr.as_mut()
            && w.write_all(chunk).is_err()
        {
            drop(guard);
            self.warn_transcript_once();
        }
    }

    fn warn_transcript_once(&self) {
        if !self.transcript_warned.swap(true, Ordering::SeqCst) {
            tracing::warn!(
                issue = %self.issue.identifier,
                "transcript write failed; continuing without local raw logging"
            );
        }
    }

    /// The end-of-run disposition of the private state directory (STUDIO-1043).
    ///
    /// A run whose last turn FAILED (or that was cancelled) retains its directory and records the
    /// session for the next dispatch; a run whose last turn succeeded — or a FRESH run that never ran
    /// a turn — removes the directory and clears any record, exactly as before. An ADOPTED session
    /// that ends before its first turn (a launch refusal, a daemon shutdown between dispatch and the
    /// first turn) is the one case that does neither: it keeps the retained directory and the record
    /// exactly as they were, because deleting them would throw away the session this feature exists
    /// to keep. Called from [`Session::stop`] on the normal path and from [`Drop`] on cancellation.
    fn end_of_run(&self) {
        let ran_a_turn = self.turn_n.load(Ordering::SeqCst) > 0;
        if ran_a_turn && self.last_turn_failed.load(Ordering::SeqCst) {
            self.persist_resume();
        } else if self.adopted && !ran_a_turn {
            // `adopt` does not mark the directory kept, so mark it here or `Drop` (which also runs
            // this method) would remove it after this returns.
            self.state.keep();
        } else {
            self.state.cleanup();
            resume::clear(&self.cfg.state_root, &self.issue.identifier);
        }
    }

    /// Writes the resume record for this issue and marks the state directory retained. Best-effort:
    /// a write failure logs and lets the directory be removed rather than leaking one that nothing
    /// can find. Idempotent, so the `stop` path and the `Drop` path can both reach it.
    fn persist_resume(&self) {
        if self.state.is_kept() {
            return;
        }
        let session_id = self.locked_session_id().clone();
        let dir = self.state.xdg_data_home().to_path_buf();
        if session_id.is_empty() || !dir.is_dir() {
            return;
        }
        let rec = ResumeRecord {
            harness: resume::HARNESS.to_string(),
            model: self.cfg.model.clone(),
            workspace: self.ws_path.clone(),
            session_id,
            dir: dir.to_string_lossy().into_owned(),
            saved_at_ms: Utc::now().timestamp_millis(),
        };
        match resume::save(&self.cfg.state_root, &self.issue.identifier, &rec) {
            Ok(()) => self.state.keep(),
            Err(e) => tracing::warn!(
                issue = %self.issue.identifier, err = %e,
                "opencode: could not record the session for resume; its state directory will be \
                 removed rather than left unreachable"
            ),
        }
    }
}

impl Drop for OpencodeSession {
    fn drop(&mut self) {
        // A cancelled run (an operator Stop, or a daemon shutdown dropping the worker future) never
        // reaches `stop`, so the retention decision is repeated here (STUDIO-1043). A run that
        // completed a successful turn already cleaned up in `stop`; `persist_resume`'s idempotence
        // and `cleanup`'s make the second pass a no-op for it.
        self.end_of_run();
    }
}

#[async_trait]
impl Session for OpencodeSession {
    fn id(&self) -> String {
        format!(
            "{}-{}",
            self.locked_session_id(),
            self.turn_n.load(Ordering::SeqCst)
        )
    }

    fn thread_id(&self) -> String {
        self.locked_session_id().clone()
    }

    fn set_run_id(&self, id: i64) {
        self.run_id.store(id, Ordering::SeqCst);
    }

    fn set_review_head(&self, sha: &str) {
        *self.locked_review_head() = sha.to_string();
    }

    fn set_model_override(&self, over: crate::ModelOverride) {
        *self.locked_model_override() = over;
    }

    /// Releases the session's private state directory. Unlike claude's no-op `stop`, this backend
    /// really does hold a resource past the turn — but a run that ended in a retryable failure
    /// KEEPS it and records the session, so the next dispatch of the same issue resumes rather than
    /// starting cold (STUDIO-1043). A run whose last turn succeeded (including one that resumed)
    /// cleans up as before.
    async fn stop(&self) -> Result<(), AgentError> {
        self.end_of_run();
        Ok(())
    }

    async fn run_turn(
        &self,
        prompt: &str,
        attempt: Option<i64>,
        messages: Option<&mut mpsc::Receiver<String>>,
        on_event: &(dyn Fn(Event) + Send + Sync),
    ) -> (TurnResult, Option<AgentError>) {
        self.run_turn_inner(prompt, attempt, messages, on_event, None)
            .await
    }

    /// A brokered turn: see [`Session::run_turn_brokered`]. A brokered session requires its
    /// per-turn attempt — a missing one refuses rather than silently running an unbrokered child.
    async fn run_turn_brokered(
        &self,
        prompt: &str,
        attempt: Option<i64>,
        messages: Option<&mut mpsc::Receiver<String>>,
        on_event: &(dyn Fn(Event) + Send + Sync),
        broker: Option<BrokerTurnAttempt>,
    ) -> (TurnResult, Option<AgentError>) {
        self.run_turn_inner(prompt, attempt, messages, on_event, broker)
            .await
    }
}

impl OpencodeSession {
    /// The one turn implementation both trait entry points delegate to. `broker` is `Some` only for
    /// a brokered turn; a legacy session's entry point passes `None`.
    async fn run_turn_inner(
        &self,
        prompt: &str,
        attempt: Option<i64>,
        mut messages: Option<&mut mpsc::Receiver<String>>,
        on_event: &(dyn Fn(Event) + Send + Sync),
        broker: Option<BrokerTurnAttempt>,
    ) -> (TurnResult, Option<AgentError>) {
        // The same containment invariant the claude runner enforces before every exec (§9.5): the
        // workspace must be inside the root and equal to the cwd.
        if let Err(e) = rhapsody_workspace::validate_launch(
            &self.cfg.workspace_root,
            &self.ws_path,
            &self.ws_path,
        ) {
            return (
                failed(Usage::default()),
                Some(AgentError::Other(e.to_string())),
            );
        }
        let turn_n = self.turn_n.fetch_add(1, Ordering::SeqCst) + 1;
        let resume = self.thread_id();
        // Assume failure until the successful return below: an early refusal, a deadline, an
        // in-band error, or a cancelled future must all leave the state directory retained for the
        // next dispatch to resume (STUDIO-1043).
        self.last_turn_failed.store(true, Ordering::SeqCst);
        tracing::info!(
            issue = %self.issue.identifier,
            turn = turn_n,
            attempt = attempt.unwrap_or(-1),
            resume = !resume.is_empty(),
            "opencode turn start"
        );
        // A session adopted from a retained record (STUDIO-1043) seeds `session_id`, so turn 1
        // carries the recorded id as its `-s` continuation AND is told it is resuming.
        let resumed_first_turn = turn_n == 1 && !resume.is_empty();

        // ⚠️ The prompt is rewritten into opencode's tool-name spelling before it is sent. Rhapsody's
        // prompts name tools literally, including the one that ends the run.
        let prompt = rewrite_tool_names(prompt);
        let prompt = if resumed_first_turn {
            // A daemon-authored notice on the run's own event stream, so the attempt records that it
            // continued the previous one (the console's run detail reads these events).
            on_event(Event {
                event_type: EVENT_NOTIFICATION.to_string(),
                timestamp: Some(Utc::now()),
                message: format!("resumed the cut-off opencode session {resume}"),
                ..Default::default()
            });
            format!("{RESUME_NOTE}\n\n{prompt}")
        } else {
            prompt
        };
        let cfg = self.turn_cfg();

        // Resolve the brokered inputs for THIS turn. A brokered session without its per-turn attempt
        // refuses; a legacy session drops any attempt (fail-closed: the capability is never minted).
        let brokered = match (self.brokered.as_ref(), broker) {
            (Some(params), Some(attempt)) => Some((params, attempt)),
            (Some(_), None) => {
                return (
                    failed(Usage::default()),
                    Some(AgentError::Other(
                        "opencode_broker_attempt_missing: a brokered session requires a per-turn \
                         broker attempt"
                            .to_string(),
                    )),
                );
            }
            (None, attempt) => {
                drop(attempt);
                None
            }
        };

        // The live turn's capability, held until after the loop so its `finish`/drop ordering is
        // explicit, and the two per-stream redactors (`§9.4`). A redactor is applied to raw child
        // chunks BEFORE parsing, transcript teeing, event extraction, or error construction.
        let mut brokered_access: Option<rhapsody_provider_broker::TurnAccess> = None;
        let mut stdout_redactor: Option<CapabilityRedactor> = None;
        let mut stderr_redactor: Option<CapabilityRedactor> = None;
        let mut brokered_material: Option<BrokeredMaterial> = None;

        let args = if let Some((params, attempt)) = brokered {
            if self.poisoned.load(Ordering::SeqCst) {
                return (
                    failed(Usage::default()),
                    Some(AgentError::Other(
                        "opencode_command_changed: the prepared opencode session was dropped"
                            .to_string(),
                    )),
                );
            }
            // ⚠️ Re-probe BEFORE minting. A command changed after preparation fails this turn with no
            // new capability and no child, and the already prepared session is dropped (`§9.1`).
            if let Err(e) = probe::probe(&params.command) {
                self.poisoned.store(true, Ordering::SeqCst);
                self.state.cleanup();
                on_event(Event {
                    event_type: EVENT_STARTUP_FAILED.to_string(),
                    timestamp: Some(Utc::now()),
                    message: e.message(),
                    ..Default::default()
                });
                return (
                    failed(Usage::default()),
                    Some(AgentError::Other(format!(
                        "{}: {}",
                        e.reason(),
                        e.message()
                    ))),
                );
            }
            let access = match attempt.mint_access() {
                Ok(access) => access,
                Err(e) => {
                    return (
                        failed(Usage::default()),
                        Some(AgentError::Other(format!(
                            "opencode_broker_mint_failed: {e}"
                        ))),
                    );
                }
            };
            let base_url = access.base_url.clone();
            let capability = access.api_key.expose_for_child(str::to_owned);
            let mcp = if self.cfg.inject_mcp {
                Some(serde_json::json!({
                    SERVER_KEY: daemon_mcp_server(&self.cfg.daemon_bin, &self.cfg.workflow_path),
                }))
            } else {
                None
            };
            let material = match BrokeredMaterial::build(
                &params.provider_id,
                &params.model,
                &base_url,
                &capability,
                self.state.config_dir(),
                self.state.xdg_data_home().to_path_buf(),
                mcp,
            ) {
                Ok(material) => material,
                Err(e) => {
                    // Dropping the access revokes the capability before any child exists.
                    drop(access);
                    return (failed(Usage::default()), Some(e));
                }
            };
            stdout_redactor = Some(CapabilityRedactor::new(&capability, &base_url));
            stderr_redactor = Some(CapabilityRedactor::new(&capability, &base_url));
            brokered_access = Some(access);
            brokered_material = Some(material);
            build_brokered_args(
                &self.ws_path,
                &resume,
                &prompt,
                &params.provider_id,
                &params.model,
            )
        } else {
            let mut args = self.cmd_args.clone();
            args.extend(build_args(&cfg, &self.ws_path, &resume, &prompt));
            args
        };

        let mut cmd = Command::new(&self.cmd_name);
        cmd.args(&args);
        cmd.current_dir(&self.ws_path);

        // Env: withhold the tracker credential by name AND by value (always), then add the "me"
        // identity so the injected MCP server can default its tools to this run. No billing scrub —
        // see the module doc.
        let base_env: Vec<String> = std::env::vars_os()
            .map(|(k, v)| format!("{}={}", k.to_string_lossy(), v.to_string_lossy()))
            .collect();
        let scrubbed = scrub_env(
            &base_env,
            TRACKER_ENV_VARS,
            &[self.cfg.tracker_api_key.as_str()],
        );
        let env = append_me_env(
            scrubbed,
            &self.issue.identifier,
            self.run_id.load(Ordering::SeqCst),
        );
        let env = append_review_env(env, &self.locked_review_head());
        let env = if let Some(material) = brokered_material.as_ref() {
            // ⚠️ Brokered mode strips every inherited OPENCODE_*/XDG_DATA_HOME BEFORE appending the
            // authoritative managed allow-list (`§9.2`), and size-checks the result before spawn.
            match material.apply_env(env) {
                Ok(env) => env,
                Err(e) => {
                    drop(brokered_access.take());
                    return (failed(Usage::default()), Some(e));
                }
            }
        } else {
            let mut env = env;
            // ⚠️ The isolation that stops concurrent turns being LOST (see `super::state`). Pushed
            // AFTER the scrub so it cannot be filtered out, and set unconditionally so an operator's
            // own XDG_DATA_HOME is overridden rather than merged with.
            env.retain(|kv| !kv.starts_with("XDG_DATA_HOME="));
            env.push(format!(
                "XDG_DATA_HOME={}",
                self.state.xdg_data_home().to_string_lossy()
            ));
            if !self.config_path.is_empty() {
                env.retain(|kv| !kv.starts_with("OPENCODE_CONFIG="));
                env.push(format!("OPENCODE_CONFIG={}", self.config_path));
            }
            env
        };
        cmd.env_clear();
        for kv in &env {
            if let Some((k, v)) = kv.split_once('=') {
                cmd.env(k, v);
            }
        }

        // Own process group so a deadline kill can signal it. Necessary but NOT sufficient: the
        // spike measured opencode's tool shell escaping into a group of its own (2 survivors of 3
        // descendants, `harness/harness-spike/opencode/killtest.txt`), which is why every kill below
        // is `kill_tree` (STUDIO-871) and never a bare group kill.
        cmd.process_group(0);
        cmd.stdin(std::process::Stdio::piped());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                on_event(Event {
                    event_type: EVENT_STARTUP_FAILED.to_string(),
                    timestamp: Some(Utc::now()),
                    message: e.to_string(),
                    ..Default::default()
                });
                let err = if e.kind() == std::io::ErrorKind::NotFound {
                    AgentError::AgentNotFound
                } else {
                    AgentError::StartupFailed
                };
                return (failed(Usage::default()), Some(err));
            }
        };
        let pid = child.id().unwrap_or(0);
        let (stdin, mut stdout, mut stderr) =
            match (child.stdin.take(), child.stdout.take(), child.stderr.take()) {
                (Some(i), Some(o), Some(e)) => (i, o, e),
                _ => {
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                    return (failed(Usage::default()), Some(AgentError::StartupFailed));
                }
            };
        // ⚠️ CLOSED AT START (`StdinPolicy::ClosedAtStart`). The prompt is already on the argv; a
        // held-open stdin is claude's mailbox, not this harness's contract.
        drop(stdin);

        // ⚠️ §9.4 ordering: one composite owns both guards and revokes the capability BEFORE the
        // process-tree kill can block (see [`TurnTeardown`]). Before this point the access lives in
        // `brokered_access`; there is no await between its mint and this construction, so a
        // cancelled future can only drop it here or later, through this composite.
        let mut teardown = TurnTeardown {
            access: BrokerTurnGuard {
                access: brokered_access.take(),
            },
            tree: KillTreeOnDrop::new(pid),
        };

        let mut usage = Usage::default();
        let mut result_text = String::new();
        let mut terminal_seen = false;
        let mut failure: Option<Failure> = None;
        // Whether the line that set `failure` was ALREADY emitted as an `EVENT_TURN_FAILED` in the
        // loop below, which for an `error` line it is (its classifier sets `ok`). Claude reports a
        // failed turn exactly ONCE — `claude/parse.rs` classifies the terminal `result` line as
        // `EVENT_TURN_FAILED`, the loop emits it, and the post-loop path returns the error without a
        // second event — and this keeps that true here without the post-loop path having to assume
        // how the classifier flags an error line. A duplicate is not cosmetic: `agentupdate.rs`
        // appends every event to `re.recent_events` and `persist` writes it, so the console and the
        // run history would show each provider failure twice.
        let mut failure_surfaced = false;
        let mut scan_err: Option<String> = None;
        let mut timed_out = false;
        let mut stderr_open = true;
        // ⚠️ Per TURN, not per session — always starting `false`. The consumer counts turns off this
        // event: `agentupdate.rs` does `re.turn_count += 1` and rebuilds `re.session_id` on every
        // `EVENT_SESSION_STARTED` (design §1.5's "session_id is derived from the turn counter").
        // Claude satisfies that because each of its turns is a fresh process emitting its own
        // `system/init`, and opencode likewise spawns one `opencode run` per turn — so one event per
        // turn is the matching behaviour. Seeding this from `thread_id` instead (announce only when
        // the session is new) would emit it once per RUN, and every multi-turn opencode run would
        // report as a single turn for its whole life.
        let mut session_announced = false;
        let mut mailbox_open = messages.is_some();
        let mut dropped_messages = 0usize;
        let mut stderr_buf = CappedBuffer::new(MAX_STDERR_CAPTURE);
        let mut acc: Vec<u8> = Vec::with_capacity(READ_CHUNK);
        let mut out_chunk = vec![0u8; READ_CHUNK];
        let mut err_chunk = vec![0u8; READ_CHUNK];

        let deadline = tokio::time::sleep_until(Instant::now() + cfg.turn_timeout);
        tokio::pin!(deadline);

        // Extracted so it can run both on every stdout chunk AND once more after the redactor's
        // `finish()` flushes its retained prefix on EOF. A macro (not a closure) so it can mutate
        // the captured turn accumulators directly.
        macro_rules! scan_stdout_lines {
            () => {{
                let mut abort = false;
                loop {
                    let Some(pos) = acc.iter().position(|&b| b == b'\n') else {
                        if acc.len() > MAX_STDOUT_LINE {
                            scan_err = Some("token too long".to_string());
                            abort = true;
                        }
                        break;
                    };
                    let mut line: Vec<u8> = acc.drain(..=pos).collect();
                    line.pop();
                    if line.len() > MAX_STDOUT_LINE {
                        scan_err = Some("token too long".to_string());
                        abort = true;
                        break;
                    }
                    self.tee_stdout(&line);
                    let c = classify(&line);
                    // Every line carries the session id and there is no announcement line, so seed
                    // from whichever arrives first — on the measured captures that is a
                    // `step_start`, which is not surfaced as an event at all.
                    if !c.session_id.is_empty() {
                        let mut sid = self.locked_session_id();
                        if sid.is_empty() {
                            *sid = c.session_id.clone();
                        }
                    }
                    // Synthesize the session event opencode never emits (design §6.1's
                    // `SessionEstablished`, once per process).
                    if !session_announced && !c.session_id.is_empty() {
                        session_announced = true;
                        on_event(Event {
                            event_type: EVENT_SESSION_STARTED.to_string(),
                            timestamp: Some(Utc::now()),
                            pid: pid as i64,
                            message: c.session_id.clone(),
                            ..Default::default()
                        });
                    }
                    if let Some(step) = c.step_usage {
                        // Per-STEP: summed, never replaced (see `super::parse`).
                        add_usage(&mut usage, &step);
                    }
                    let mut ev = c.event.clone();
                    ev.pid = pid as i64;
                    // ⚠️ A usage-bearing NOTIFICATION must carry the RUNNING TURN TOTAL, not this
                    // step's own figures. The orchestrator's live estimate is LAST-WINS, not
                    // additive (`agentupdate.rs`: "the assistant message.usage is
                    // CUMULATIVE-within-the-turn, so the LATEST snapshot already IS the current turn
                    // total"). Claude satisfies that because its per-message usage really is
                    // cumulative; opencode's `step_finish.tokens` is PER STEP and resets every step,
                    // so passing it through unchanged would make the dashboard's live token count
                    // jump DOWN at each step boundary and finish reporting one step instead of the
                    // turn. Substituting the accumulator restores the contract the consumer
                    // documents.
                    if c.step_usage.is_some() {
                        ev.usage = Some(usage);
                    }
                    if !c.text.is_empty() {
                        result_text = c.text.clone();
                    }
                    if let Some(f) = c.failure.clone() {
                        // First error wins: later ones are consequences of it, so the `surfaced`
                        // flag tracks THAT line's emit (just below).
                        if failure.is_none() {
                            failure = Some(f);
                            failure_surfaced = c.ok;
                        }
                    }
                    if c.ok {
                        on_event(ev);
                    }
                    if c.terminal {
                        terminal_seen = true;
                    }
                }
                abort
            }};
        }

        'outer: loop {
            tokio::select! {
                _ = &mut deadline => {
                    kill_tree(pid);
                    timed_out = true;
                    break 'outer;
                }
                r = stdout.read(&mut out_chunk) => {
                    match r {
                        Ok(0) => {
                            // ⚠️ EOF: flush the redactor's retained prefix FIRST, so a capability
                            // split across the final read boundary is still redacted before the last
                            // line is parsed or teed (`§9.4`).
                            if let Some(redactor) = stdout_redactor.as_mut() {
                                let tail = redactor.finish();
                                acc.extend_from_slice(&tail);
                            }
                            if scan_stdout_lines!() {
                                break 'outer;
                            }
                            break 'outer; // EOF — the terminal condition for this harness
                        }
                        Ok(n) => {
                            // Redact the raw chunk before it can reach the line accumulator,
                            // transcript, or classifier.
                            let redacted = match stdout_redactor.as_mut() {
                                Some(redactor) => redactor.push(&out_chunk[..n]),
                                None => out_chunk[..n].to_vec(),
                            };
                            acc.extend_from_slice(&redacted);
                            if scan_stdout_lines!() {
                                break 'outer;
                            }
                        }
                        Err(e) => {
                            scan_err = Some(e.to_string());
                            break 'outer;
                        }
                    }
                }
                r = stderr.read(&mut err_chunk), if stderr_open => {
                    match r {
                        Ok(0) => {
                            if let Some(redactor) = stderr_redactor.as_mut() {
                                let tail = redactor.finish();
                                stderr_buf.write(&tail);
                                self.tee_stderr(&tail);
                            }
                            stderr_open = false;
                        }
                        Ok(n) => {
                            let redacted = match stderr_redactor.as_mut() {
                                Some(redactor) => redactor.push(&err_chunk[..n]),
                                None => err_chunk[..n].to_vec(),
                            };
                            stderr_buf.write(&redacted);
                            self.tee_stderr(&redacted);
                        }
                        Err(_) => stderr_open = false,
                    }
                }
                // ⚠️ Drained, never delivered. stdin is closed, so there is nowhere to write an
                // operator message. Draining (rather than ignoring) keeps the channel from filling
                // and stalling its sender, and every dropped message is counted and named below —
                // a message this backend cannot deliver must not vanish quietly.
                m = recv_opt(&mut messages), if mailbox_open => {
                    match m {
                        Some(msg) => {
                            dropped_messages += 1;
                            tracing::warn!(
                                issue = %self.issue.identifier, turn = turn_n, message = %msg,
                                "opencode cannot steer a live turn (stdin is closed at start); \
                                 operator message NOT delivered"
                            );
                        }
                        None => mailbox_open = false,
                    }
                }
            }
        }

        // ⚠️ §9.4 ordering: declare normal completion (or revoke) for the turn capability BEFORE
        // process-tree teardown can block. `finish` marks a clean turn; dropping without it revokes
        // the grant and records the turn as revoked. Either way no capability remains valid between
        // outer turns.
        if terminal_seen && failure.is_none() {
            teardown.finish();
        } else {
            teardown.revoke();
        }

        let drain_out = async {
            let mut buf = [0u8; 4096];
            while let Ok(n) = stdout.read(&mut buf).await {
                if n == 0 {
                    break;
                }
            }
        };
        let drain_err = async {
            let mut buf = [0u8; 4096];
            if stderr_open {
                loop {
                    match stderr.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            // Redact the post-loop drain too: it reaches the error buffer.
                            let redacted = match stderr_redactor.as_mut() {
                                Some(redactor) => redactor.push(&buf[..n]),
                                None => buf[..n].to_vec(),
                            };
                            stderr_buf.write(&redacted);
                            self.tee_stderr(&redacted);
                        }
                    }
                }
                if let Some(redactor) = stderr_redactor.as_mut() {
                    let tail = redactor.finish();
                    stderr_buf.write(&tail);
                    self.tee_stderr(&tail);
                }
            }
        };
        tokio::join!(drain_out, drain_err);
        let wait_res = child.wait().await;
        teardown.disarm_tree();

        if dropped_messages > 0 {
            tracing::warn!(
                issue = %self.issue.identifier, turn = turn_n, dropped = dropped_messages,
                "opencode turn finished with undelivered operator messages"
            );
        }

        // A deadline kill that landed after a completed turn must not discard the turn, which is
        // why `terminal_seen` is checked before `timed_out` — the same precedence the claude runner
        // gives a captured result.
        if terminal_seen && failure.is_none() {
            // A clean turn: the run may keep going (a continuation) or end, but it is no longer a
            // cut-off one — `end_of_run` cleans up unless a LATER turn fails (STUDIO-1043).
            self.last_turn_failed.store(false, Ordering::SeqCst);
            let tr = TurnResult {
                status: TURN_SUCCEEDED.to_string(),
                usage,
                result_text: truncate_tail(&result_text, MAX_RESULT_TEXT),
            };
            on_event(Event {
                event_type: EVENT_TURN_COMPLETED.to_string(),
                timestamp: Some(Utc::now()),
                pid: pid as i64,
                usage: Some(usage),
                ..Default::default()
            });
            return (tr, None);
        }
        // An in-band error line. opencode is the only measured harness that reports an HTTP status
        // and an `isRetryable` boolean directly, so both are carried into the message rather than
        // re-derived from its text (design §7.1).
        if let Some(f) = failure {
            if !failure_surfaced {
                on_event(Event {
                    event_type: EVENT_TURN_FAILED.to_string(),
                    timestamp: Some(Utc::now()),
                    pid: pid as i64,
                    message: f.summary(),
                    ..Default::default()
                });
            }
            return (
                failed_with_text(usage, &result_text),
                Some(AgentError::Other(format!("turn_failed: {}", f.summary()))),
            );
        }
        if timed_out {
            on_event(Event {
                event_type: EVENT_TURN_FAILED.to_string(),
                timestamp: Some(Utc::now()),
                pid: pid as i64,
                message: "turn timeout".to_string(),
                ..Default::default()
            });
            return (timed_out_result(usage), Some(AgentError::TurnTimeout));
        }
        if let Some(e) = scan_err {
            return (
                failed(usage),
                Some(AgentError::Other(format!(
                    "turn_failed: stream read error: {e}"
                ))),
            );
        }

        // ⚠️ No terminal `step_finish{reason:"stop"}`. For this harness that is a FIRST-CLASS
        // failure, not an impossible state: a turn that loses the `database is locked` race exits 1
        // in under a second with a COMPLETELY EMPTY event stream and the reason only on stderr
        // (spike README, "a turn can fail with zero events"). Reporting the stderr verbatim is what
        // makes that case diagnosable instead of anonymous.
        let mut msg = truncate_stderr(stderr_buf.bytes());
        if stderr_buf.truncated {
            msg.push_str(" (stderr capped)");
        }
        let detail = match &wait_res {
            Ok(s) => format!("{s}"),
            Err(e) => format!("{e}"),
        };
        let whose = self.model_attribution();
        let empty = if session_announced {
            ""
        } else {
            " (no events were emitted at all — if stderr names `database is locked`, two turns \
              shared one opencode state directory)"
        };
        (
            failed_with_text(usage, &result_text),
            Some(AgentError::Other(format!(
                "turn_failed: stream ended without a terminal step_finish: {detail}: {whose}{msg}{empty}"
            ))),
        )
    }
}

fn failed(usage: Usage) -> TurnResult {
    TurnResult {
        status: TURN_FAILED.to_string(),
        usage,
        result_text: String::new(),
    }
}

/// A failed turn that still carries whatever the agent said. A failure after real work must not
/// discard the agent's own account of it.
fn failed_with_text(usage: Usage, text: &str) -> TurnResult {
    TurnResult {
        status: TURN_FAILED.to_string(),
        usage,
        result_text: truncate_tail(text, MAX_RESULT_TEXT),
    }
}

fn timed_out_result(usage: Usage) -> TurnResult {
    TurnResult {
        status: TURN_TIMED_OUT.to_string(),
        usage,
        result_text: String::new(),
    }
}

/// `None` never yields, so a backend with no mailbox pays nothing for the select arm.
async fn recv_opt(messages: &mut Option<&mut mpsc::Receiver<String>>) -> Option<String> {
    match messages {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// Keeps the TAIL of the result text: the `HANDOFF:` marker is on the last line.
fn truncate_tail(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut start = s.len() - max;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    s[start..].to_string()
}

fn truncate_stderr(b: &[u8]) -> String {
    let s = String::from_utf8_lossy(b);
    let t = s.trim();
    if t.len() <= MAX_STDERR_MESSAGE {
        return t.to_string();
    }
    let mut end = MAX_STDERR_MESSAGE;
    while end > 0 && !t.is_char_boundary(end) {
        end -= 1;
    }
    t[..end].to_string()
}

/// A bounded sink that records whether it dropped anything — the mirror of the claude runner's.
struct CappedBuffer {
    buf: Vec<u8>,
    cap: usize,
    truncated: bool,
}

impl CappedBuffer {
    fn new(cap: usize) -> CappedBuffer {
        CappedBuffer {
            buf: Vec::new(),
            cap,
            truncated: false,
        }
    }

    fn write(&mut self, p: &[u8]) {
        if self.buf.len() >= self.cap {
            self.truncated = true;
            return;
        }
        let room = self.cap - self.buf.len();
        if p.len() <= room {
            self.buf.extend_from_slice(p);
        } else {
            self.buf.extend_from_slice(&p[..room]);
            self.truncated = true;
        }
    }

    fn bytes(&self) -> &[u8] {
        &self.buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opencode::testdir::TempDir;
    use crate::{ENV_GUARD, EVENT_NOTIFICATION, Runner as _};
    use std::sync::Arc;

    /// A fake `opencode` that reproduces the ONE failure this adapter exists to prevent.
    ///
    /// ⚠️ It models the real hazard rather than mimicking a success: on entry it tries to
    /// `mkdir "$XDG_DATA_HOME/opencode/.lock"`, and **if that directory already exists it behaves
    /// exactly as a real opencode turn that loses the `database is locked` race** — nothing at all
    /// on stdout, the CLI's own two-line error on stderr, exit 1. `$XDG_DATA_HOME/opencode/` is
    /// where the real SQLite database lives, so "two turns reached the same state directory" is the
    /// same condition in the fake and in the CLI.
    ///
    /// That is what makes the concurrency test below a real test: delete the per-run isolation and
    /// it goes red the way production does, with an empty event stream, rather than staying green
    /// because a stub always succeeds.
    const FAKE_OPENCODE: &str = r#"
set -u
lock="$XDG_DATA_HOME/opencode/.lock"
if ! mkdir "$lock" 2>/dev/null; then
  # A real losing turn: zero events, the reason only on stderr, exit 1.
  printf 'Error: Unexpected error\n' >&2
  printf 'database is locked\n' >&2
  exit 1
fi
# The session id is derived from the state directory, so two isolated turns necessarily report
# different ones — the real CLI mints a random `ses_…` per session and the committed
# `concurrency-trials-isolated-xdg.txt` shows ten isolated turns with ten distinct ids.
sid="ses_$(basename "$XDG_DATA_HOME" | tr -cd 'A-Za-z0-9')"
printf '{"type":"step_start","sessionID":"%s","part":{"type":"step-start"}}\n' "$sid"
# Hold the state directory long enough that a concurrent partner really does overlap with it.
sleep 0.4
printf '{"type":"tool_use","sessionID":"%s","part":{"type":"tool","tool":"read","state":{"status":"completed","input":{"filePath":"/x"},"output":"ok"}}}\n' "$sid"
printf '{"type":"text","sessionID":"%s","part":{"type":"text","text":"done"}}\n' "$sid"
printf '{"type":"step_finish","sessionID":"%s","part":{"type":"step-finish","reason":"stop","tokens":{"total":10,"input":6,"output":3,"reasoning":1,"cache":{"write":0,"read":0}}}}\n' "$sid"
exit 0
"#;

    fn write_script(dir: &TempDir, name: &str, body: &str) -> String {
        let p = dir.path().join(name);
        std::fs::write(&p, body).expect("write fake opencode");
        p.to_string_lossy().into_owned()
    }

    fn seeded_auth(dir: &TempDir) -> String {
        let p = dir.path().join("auth.json");
        std::fs::write(&p, b"{\"fireworks-ai\":{\"type\":\"api\"}}").expect("write auth");
        p.to_string_lossy().into_owned()
    }

    fn make_ws(root: &TempDir, id: &str) -> String {
        let p = root.path().join(id);
        std::fs::create_dir_all(&p).expect("create workspace");
        // The runner's containment invariant compares canonical paths; on macOS `/var` is a symlink
        // to `/private/var`, so both sides must be canonicalized the same way.
        std::fs::canonicalize(&p)
            .expect("canonicalize workspace")
            .to_string_lossy()
            .into_owned()
    }

    fn root_path(root: &TempDir) -> String {
        std::fs::canonicalize(root.path())
            .expect("canonicalize root")
            .to_string_lossy()
            .into_owned()
    }

    fn issue(identifier: &str) -> rhapsody_core::Issue {
        rhapsody_core::Issue {
            id: identifier.to_string(),
            identifier: identifier.to_string(),
            ..Default::default()
        }
    }

    fn runner_for(script: &str, root: &TempDir, auth: &str, state_root: &str) -> Runner {
        Runner::new(Config {
            command: format!("bash {script}"),
            workspace_root: root_path(root),
            turn_timeout: std::time::Duration::from_secs(30),
            auth_source: auth.to_string(),
            state_root: state_root.to_string(),
            ..Default::default()
        })
    }

    /// Collects the normalized events a turn emits, so assertions are on the EVENT STREAM.
    fn collector() -> (Arc<Mutex<Vec<Event>>>, impl Fn(Event) + Send + Sync) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        (seen, move |e: Event| {
            sink.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(e);
        })
    }

    fn events_of(seen: &Arc<Mutex<Vec<Event>>>) -> Vec<Event> {
        seen.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    // ⚠️⚠️ THE ACCEPTANCE TEST. "Two concurrent opencode runs lose no turns. Assert on the event
    // streams, not on exit codes — the failure mode is an empty stream with exit 1."
    //
    // Both turns run at once against a runner whose fake CLI refuses any state directory that is
    // already in use. They pass only because each SESSION was provisioned its own `XDG_DATA_HOME`.
    #[tokio::test]
    async fn two_concurrent_turns_lose_no_turns() {
        let _env = ENV_GUARD.read().await;
        let scripts = TempDir::new();
        let script = write_script(&scripts, "fake-opencode.sh", FAKE_OPENCODE);
        let auth = seeded_auth(&scripts);
        let state_root = TempDir::new();
        let root = TempDir::new();
        let ws_a = make_ws(&root, "a");
        let ws_b = make_ws(&root, "b");

        let runner = Arc::new(runner_for(
            &script,
            &root,
            &auth,
            &state_root.path().to_string_lossy(),
        ));
        let sess_a = runner
            .start_session(&ws_a, issue("STUDIO-902-A"), None)
            .await
            .expect("session a");
        let sess_b = runner
            .start_session(&ws_b, issue("STUDIO-902-B"), None)
            .await
            .expect("session b");

        let (seen_a, on_a) = collector();
        let (seen_b, on_b) = collector();
        let (res_a, res_b) = tokio::join!(
            sess_a.run_turn("prompt a", None, None, &on_a),
            sess_b.run_turn("prompt b", None, None, &on_b),
        );

        for (label, (tr, err), seen) in [("a", res_a, &seen_a), ("b", res_b, &seen_b)] {
            let evs = events_of(seen);
            // The event stream first: a lost turn emits NOTHING, which is the shape being ruled out.
            assert!(
                !evs.is_empty(),
                "turn {label} emitted an EMPTY event stream — the `database is locked` signature"
            );
            assert!(
                evs.iter().any(|e| e.event_type == EVENT_SESSION_STARTED),
                "turn {label} never established a session: {evs:#?}"
            );
            assert!(
                evs.iter().any(|e| e.event_type == EVENT_TURN_COMPLETED),
                "turn {label} never completed: {evs:#?}"
            );
            assert!(
                !evs.iter().any(|e| e.event_type == EVENT_TURN_FAILED),
                "turn {label} reported a failure: {evs:#?}"
            );
            assert_eq!(tr.status, TURN_SUCCEEDED, "turn {label}: {err:?}");
            assert_eq!(tr.result_text, "done", "turn {label}");
            assert!(err.is_none(), "turn {label}: {err:?}");
        }

        // ⚠️ And the reason they both survived: two DIFFERENT state directories, hence two different
        // session ids. Equal ids here would mean one directory was shared and the test passed by
        // luck rather than by isolation.
        assert_ne!(
            sess_a.thread_id(),
            sess_b.thread_id(),
            "both turns reported the same session id, so they shared a state directory"
        );
        assert!(sess_a.thread_id().starts_with("ses_"));
    }

    /// The same fake, proving the test above can actually fail: pointed at ONE shared state
    /// directory, a concurrent pair really does lose a turn, and it loses it as an EMPTY event
    /// stream. Without this, "both turns completed" could just mean the stub never refuses
    /// anything.
    #[tokio::test]
    async fn sharing_one_state_directory_loses_a_turn_with_an_empty_stream() {
        let _env = ENV_GUARD.read().await;
        let scripts = TempDir::new();
        let script = write_script(&scripts, "fake-opencode.sh", FAKE_OPENCODE);
        let root = TempDir::new();
        let ws = make_ws(&root, "shared");
        // One directory, pre-created and seeded exactly as `RunState` would, then handed to both
        // turns directly — the "no isolation" arrangement this adapter exists to avoid.
        let shared = TempDir::new();
        std::fs::create_dir_all(shared.path().join("opencode")).expect("mkdir");

        let run_one = |label: &'static str| {
            let script = script.clone();
            let ws = ws.clone();
            let xdg = shared.path().to_string_lossy().into_owned();
            async move {
                let out = tokio::process::Command::new("bash")
                    .arg(&script)
                    .current_dir(&ws)
                    .env("XDG_DATA_HOME", &xdg)
                    .output()
                    .await
                    .expect("spawn fake opencode");
                (label, out)
            }
        };
        let (a, b) = tokio::join!(run_one("a"), run_one("b"));

        let losers: Vec<&str> = [&a, &b]
            .iter()
            .filter(|(_, o)| !o.status.success())
            .map(|(l, _)| *l)
            .collect();
        assert_eq!(
            losers.len(),
            1,
            "exactly one of the pair must lose the race; got {losers:?}"
        );
        let (_, lost) = if losers[0] == "a" { &a } else { &b };
        assert!(
            lost.stdout.is_empty(),
            "the losing turn must emit ZERO events; got {:?}",
            String::from_utf8_lossy(&lost.stdout)
        );
        assert!(
            String::from_utf8_lossy(&lost.stderr).contains("database is locked"),
            "stderr must carry the real reason: {:?}",
            String::from_utf8_lossy(&lost.stderr)
        );
    }

    // ⚠️ The live token estimate must only ever go UP, and must finish at the turn total.
    //
    // The consumer (`rhapsody_orchestrator::agentupdate`) treats a notification's usage as
    // LAST-WINS, not additive, because claude's per-message usage is cumulative within a turn.
    // opencode's `step_finish.tokens` is PER STEP and resets every step, so forwarding it unchanged
    // would make the dashboard's live count fall at each step boundary and settle on one step's
    // figures. This pins the substitution that fixes it — and it is a regression test for a real
    // defect found by running the adapter against the live CLI, not a hypothetical.
    #[tokio::test]
    async fn live_usage_notifications_are_the_running_total_not_one_step() {
        let _env = ENV_GUARD.read().await;
        let scripts = TempDir::new();
        // Three steps of 10 billed tokens each: 6 in + 3 out + 1 reasoning (folded into output).
        // A raw string keeps the JSON's own quotes readable; `{reason}` is substituted by
        // `replace` rather than `format!` so no brace in the JSON needs doubling.
        let step = |reason: &str| {
            const TMPL: &str = r#"printf '{"type":"step_finish","sessionID":"ses_x","part":{"reason":"REASON","tokens":{"total":10,"input":6,"output":3,"reasoning":1,"cache":{"write":0,"read":0}}}}\n'
"#;
            TMPL.replace("REASON", reason)
        };
        let body = format!(
            "{}{}{}",
            step("tool-calls"),
            step("tool-calls"),
            step("stop")
        );
        let script = write_script(&scripts, "steps.sh", &body);
        let auth = seeded_auth(&scripts);
        let state_root = TempDir::new();
        let root = TempDir::new();
        let ws = make_ws(&root, "w");

        let runner = runner_for(&script, &root, &auth, &state_root.path().to_string_lossy());
        let sess = runner
            .start_session(&ws, issue("STUDIO-902"), None)
            .await
            .expect("session");
        let (seen, on_event) = collector();
        let (tr, err) = sess.run_turn("p", None, None, &on_event).await;
        assert_eq!(tr.status, TURN_SUCCEEDED, "{err:?}");

        let live: Vec<i64> = events_of(&seen)
            .iter()
            .filter(|e| e.event_type == EVENT_NOTIFICATION)
            .filter_map(|e| e.usage.map(|u| u.total_tokens))
            .collect();
        assert_eq!(
            live,
            vec![10, 20, 30],
            "each notification must carry the RUNNING total, not its own step's 10"
        );
        assert_eq!(
            tr.usage.total_tokens, 30,
            "and the committed turn total is the sum of every step"
        );
        // The invariant stated plainly: a last-wins consumer reading only the final notification
        // must land on the same number the turn commits.
        assert_eq!(live.last().copied(), Some(tr.usage.total_tokens));
    }

    // ⚠️ EVERY turn announces its session, and a continuation turn resumes with `-s <id>`.
    //
    // The consumer counts turns off `EVENT_SESSION_STARTED` (`agentupdate.rs` increments
    // `turn_count` and rebuilds `session_id` on each one), so emitting it only when the session id
    // is NEW would report every multi-turn run as a single turn forever. Claude emits one per turn
    // because each turn is a fresh process with its own `system/init`; this asserts opencode does
    // the same, and that turn 2 really did carry the resume flag.
    #[tokio::test]
    async fn every_turn_announces_its_session_and_turn_two_resumes() {
        let _env = ENV_GUARD.read().await;
        let scripts = TempDir::new();
        let argv_log = scripts.path().join("argv.log");
        // Appends its whole argv per invocation, then emits a one-step turn with a fixed session
        // id. A raw string keeps the shell and JSON quoting readable; LOG is substituted rather
        // than formatted so no brace or dollar needs escaping.
        const TMPL: &str = r#"printf '%s\n' "$*" >> "LOG"
printf '{"type":"step_finish","sessionID":"ses_stable","part":{"reason":"stop"}}\n'
"#;
        let body = TMPL.replace("LOG", &argv_log.display().to_string());
        let script = write_script(&scripts, "twoturn.sh", &body);
        let auth = seeded_auth(&scripts);
        let state_root = TempDir::new();
        let root = TempDir::new();
        let ws = make_ws(&root, "w");

        let runner = runner_for(&script, &root, &auth, &state_root.path().to_string_lossy());
        let sess = runner
            .start_session(&ws, issue("STUDIO-902"), None)
            .await
            .expect("session");

        let (seen, on_event) = collector();
        for turn in 1..=2 {
            let (tr, err) = sess.run_turn("p", None, None, &on_event).await;
            assert_eq!(tr.status, TURN_SUCCEEDED, "turn {turn}: {err:?}");
        }

        let starts = events_of(&seen)
            .iter()
            .filter(|e| e.event_type == EVENT_SESSION_STARTED)
            .count();
        assert_eq!(
            starts, 2,
            "one session event PER TURN — the consumer counts turns with them"
        );

        let log = std::fs::read_to_string(&argv_log).expect("argv log");
        let lines: Vec<&str> = log.lines().collect();
        assert_eq!(lines.len(), 2, "two invocations: {lines:?}");
        assert!(
            !lines[0].contains("-s "),
            "turn 1 must not resume: {}",
            lines[0]
        );
        assert!(
            lines[1].contains("-s ses_stable"),
            "turn 2 must resume the captured session: {}",
            lines[1]
        );
        // And the session/turn pair the orchestrator reads is the second turn's.
        assert_eq!(sess.thread_id(), "ses_stable");
        assert_eq!(sess.id(), "ses_stable-2");
    }

    // ⚠️ An in-band `error` line is reported ONCE, the way claude reports its own failure once.
    //
    // The line is already surfaced as an `EVENT_TURN_FAILED` inside the read loop (its classifier
    // sets `ok`), so a post-loop emit of the same failure would double it — and duplicates are not
    // cosmetic: `agentupdate.rs` appends every event to `re.recent_events` and `persist` writes it,
    // so the console and the run history would show each provider failure twice. Driven by the REAL
    // 401 capture (the whole measured stream is that one line, and the run it came from exited 1),
    // so this pins the shape the CLI actually produces rather than one this file invented.
    #[tokio::test]
    async fn an_in_band_error_is_reported_exactly_once() {
        let _env = ENV_GUARD.read().await;
        let scripts = TempDir::new();
        let capture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../harness/harness-spike/opencode/failure-401.jsonl");
        let script = write_script(
            &scripts,
            "error401.sh",
            &format!("cat '{}'\nexit 1\n", capture.display()),
        );
        let auth = seeded_auth(&scripts);
        let state_root = TempDir::new();
        let root = TempDir::new();
        let ws = make_ws(&root, "w");

        let runner = runner_for(&script, &root, &auth, &state_root.path().to_string_lossy());
        let sess = runner
            .start_session(&ws, issue("STUDIO-902"), None)
            .await
            .expect("session");
        let (seen, on_event) = collector();
        let (tr, err) = sess.run_turn("p", None, None, &on_event).await;

        let evs = events_of(&seen);
        let failed: Vec<&Event> = evs
            .iter()
            .filter(|e| e.event_type == EVENT_TURN_FAILED)
            .collect();
        assert_eq!(
            failed.len(),
            1,
            "one provider failure must produce ONE turn_failed event: {evs:#?}"
        );
        assert!(
            failed[0].message.contains("status 401"),
            "the surviving event must carry the reason: {:?}",
            failed[0].message
        );
        assert!(
            !evs.iter().any(|e| e.event_type == EVENT_TURN_COMPLETED),
            "a failed turn must not also report completion: {evs:#?}"
        );
        assert_eq!(tr.status, TURN_FAILED);
        let msg = err.expect("an error line must be an error").to_string();
        assert!(msg.contains("turn_failed: APIError"), "{msg}");
    }

    // ⚠️ An EXPLICIT `opencode.turn_timeout_ms: 0` must mean one hour, not "every turn times out".
    //
    // `decode` defaults an ABSENT value to 3600000, but an explicit `0` — the natural way to write
    // "no limit", and where a negative value also lands via the orchestrator's `.max(0)` — reaches
    // the runner as `Duration::ZERO`, which would make the deadline already expired when the read
    // loop is entered. `claude::Runner::new` materializes the same input to one hour, and the two
    // backends must not read one operator input two different ways. The fake emits a clean terminal
    // turn, so a red assertion here means the deadline fired, not that the CLI failed.
    #[tokio::test]
    async fn an_explicit_zero_turn_timeout_means_one_hour_not_instant_expiry() {
        let _env = ENV_GUARD.read().await;
        let scripts = TempDir::new();
        let script = write_script(
            &scripts,
            "clean.sh",
            "printf '{\"type\":\"text\",\"sessionID\":\"ses_x\",\"part\":{\"text\":\"done\"}}\n'\n\
             printf '{\"type\":\"step_finish\",\"sessionID\":\"ses_x\",\"part\":{\"reason\":\"stop\"}}\n'\n",
        );
        let auth = seeded_auth(&scripts);
        let state_root = TempDir::new();
        let root = TempDir::new();
        let ws = make_ws(&root, "w");

        let runner = Runner::new(Config {
            command: format!("bash {script}"),
            workspace_root: root_path(&root),
            // The defect's exact input, straight from config.
            turn_timeout: Duration::ZERO,
            auth_source: auth.clone(),
            state_root: state_root.path().to_string_lossy().into_owned(),
            ..Default::default()
        });
        let sess = runner
            .start_session(&ws, issue("STUDIO-902"), None)
            .await
            .expect("session");
        let (seen, on_event) = collector();
        let (tr, err) = sess.run_turn("p", None, None, &on_event).await;

        let evs = events_of(&seen);
        assert!(
            !evs.iter()
                .any(|e| e.event_type == EVENT_TURN_FAILED && e.message == "turn timeout"),
            "a zero turn timeout must not expire the turn: {evs:#?}"
        );
        assert_eq!(tr.status, TURN_SUCCEEDED, "{err:?}");
        assert_eq!(tr.result_text, "done");
        assert!(err.is_none(), "{err:?}");
    }

    // ⚠️ The empty-stream case as the RUNNER sees it: "stream ended with no terminal event" must be
    // a named, diagnosable failure, not an impossible state. The message has to carry the stderr,
    // because that is the only place the reason exists.
    #[tokio::test]
    async fn a_turn_with_zero_events_fails_with_the_stderr_reason() {
        let _env = ENV_GUARD.read().await;
        let scripts = TempDir::new();
        let script = write_script(
            &scripts,
            "loser.sh",
            "printf 'Error: Unexpected error\\ndatabase is locked\\n' >&2\nexit 1\n",
        );
        let auth = seeded_auth(&scripts);
        let state_root = TempDir::new();
        let root = TempDir::new();
        let ws = make_ws(&root, "w");

        let runner = runner_for(&script, &root, &auth, &state_root.path().to_string_lossy());
        let sess = runner
            .start_session(&ws, issue("STUDIO-902"), None)
            .await
            .expect("session");
        let (seen, on_event) = collector();
        let (tr, err) = sess.run_turn("p", None, None, &on_event).await;

        assert!(events_of(&seen).is_empty(), "the fixture emits no events");
        assert_eq!(tr.status, TURN_FAILED);
        let msg = err.expect("a zero-event turn must be an error").to_string();
        assert!(
            msg.contains("stream ended without a terminal step_finish"),
            "{msg}"
        );
        assert!(
            msg.contains("database is locked"),
            "the reason must survive: {msg}"
        );
        assert!(
            msg.contains("two turns shared one opencode state directory"),
            "a zero-event turn must name its most likely cause: {msg}"
        );
    }

    // ⚠️ STUDIO-840/871, for THIS backend. The point is not that `kill_tree` works — `proctree.rs`
    // owns that test — but that this runner ARMS it: a turn killed by its deadline must take the
    // tool child the agent put in its OWN process group, not just the leader. A bare
    // `kill(-leader, SIGKILL)` here would leave the grandchild running and re-certify STUDIO-840.
    #[tokio::test]
    async fn a_deadline_kill_takes_a_tool_child_that_escaped_the_process_group() {
        let _env = ENV_GUARD.read().await;
        let scripts = TempDir::new();
        let auth = seeded_auth(&scripts);
        let pgid_file = scripts.path().join("escaped.pgid");
        // `set -m` puts the background job in a process group of its OWN — exactly what the spike
        // measured opencode doing to the shell it runs a tool command in (2 of 3 descendants
        // survived a group kill, `harness/harness-spike/opencode/killtest.txt`). The child reports
        // its own pgid, written-then-renamed so a reader never sees a partial file.
        let body = format!(
            "set -m\n\
             bash -c 'ps -o pgid= -p $$ | tr -d \" \" > \"$1.tmp\"; mv \"$1.tmp\" \"$1\"; \
             while true; do sleep 3600; done' _ \"{p}\" &\n\
             set +m\n\
             while true; do sleep 3600; done\n",
            p = pgid_file.display()
        );
        let script = write_script(&scripts, "slow.sh", &body);
        let state_root = TempDir::new();
        let root = TempDir::new();
        let ws = make_ws(&root, "w");

        let mut cfg = Config {
            command: format!("bash {script}"),
            workspace_root: root_path(&root),
            // Long enough for the grandchild to exist and report, short enough to keep the test fast.
            turn_timeout: std::time::Duration::from_secs(6),
            auth_source: auth.clone(),
            state_root: state_root.path().to_string_lossy().into_owned(),
            ..Default::default()
        };
        cfg.inject_mcp = false;
        let runner = Runner::new(cfg);
        let sess = runner
            .start_session(&ws, issue("STUDIO-902"), None)
            .await
            .expect("session");

        let (_seen, on_event) = collector();
        let (tr, err) = sess.run_turn("p", None, None, &on_event).await;
        assert_eq!(tr.status, TURN_TIMED_OUT, "{err:?}");
        assert!(matches!(err, Some(AgentError::TurnTimeout)), "{err:?}");

        let escaped: i32 = std::fs::read_to_string(&pgid_file)
            .expect("the fixture never reported an escaped process group")
            .trim()
            .parse()
            .expect("parse escaped pgid");
        // Assert on OS process state, never on a return value.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let survivors = live_in_group(escaped);
            if survivors.is_empty() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the escaped tool child survived the deadline kill: {survivors:?}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    /// Live (non-zombie) processes in `pgid`, as `ps` reports them.
    fn live_in_group(pgid: i32) -> Vec<String> {
        let out = std::process::Command::new("ps")
            .args(["-Ao", "pid=,pgid=,stat=,comm="])
            .output()
            .expect("ps");
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| {
                let mut f = l.split_whitespace();
                let pid = f.next()?;
                let grp: i32 = f.next()?.parse().ok()?;
                let stat = f.next()?;
                let comm = f.next().unwrap_or("");
                (grp == pgid && !stat.starts_with('Z')).then(|| format!("{pid} {stat} {comm}"))
            })
            .collect()
    }

    // The loud-and-early refusal: no credential, no process. This is the "what an unsupported
    // capability does until slice 5" path, and the whole point is that it happens at
    // `start_session` rather than as a 401 mid-turn that reads like a provider problem.
    #[tokio::test]
    async fn start_session_refuses_a_missing_credential_before_spawning_anything() {
        let scripts = TempDir::new();
        let script = write_script(&scripts, "never-runs.sh", "touch \"$0.RAN\"\n");
        let root = TempDir::new();
        let ws = make_ws(&root, "w");
        let state_root = TempDir::new();

        let runner = runner_for(
            &script,
            &root,
            &scripts.path().join("absent.json").to_string_lossy(),
            &state_root.path().to_string_lossy(),
        );
        let err = runner
            .start_session(&ws, issue("STUDIO-902"), None)
            .await
            .err()
            .expect("must refuse");
        assert!(
            err.to_string().starts_with("opencode_auth_missing:"),
            "{err}"
        );
        assert!(
            !std::path::Path::new(&format!("{script}.RAN")).exists(),
            "the refusal must happen before anything is spawned"
        );
    }

    // ⚠️ The prompt reaches the child in OPENCODE's tool spelling, as a positional argument, with
    // stdin closed. All three at once, because they are one decision: no mailbox, prompt on argv.
    #[tokio::test]
    async fn the_prompt_is_a_positional_in_opencodes_tool_spelling_and_stdin_is_closed() {
        let _env = ENV_GUARD.read().await;
        let scripts = TempDir::new();
        let dump = scripts.path().join("argv.txt");
        let stdin_state = scripts.path().join("stdin.txt");
        let body = format!(
            "for a in \"$@\"; do printf '%s\\n' \"$a\"; done > \"{argv}\"\n\
             if head -c 1 /dev/stdin >/dev/null 2>&1; then echo open > \"{si}\"; else echo closed > \"{si}\"; fi\n\
             printf '{{\"type\":\"step_finish\",\"sessionID\":\"ses_x\",\"part\":{{\"reason\":\"stop\"}}}}\\n'\n",
            argv = dump.display(),
            si = stdin_state.display()
        );
        let script = write_script(&scripts, "argv.sh", &body);
        let auth = seeded_auth(&scripts);
        let state_root = TempDir::new();
        let root = TempDir::new();
        let ws = make_ws(&root, "w");

        let runner = runner_for(&script, &root, &auth, &state_root.path().to_string_lossy());
        let sess = runner
            .start_session(&ws, issue("STUDIO-902"), None)
            .await
            .expect("session");
        let (_seen, on_event) = collector();
        let (tr, err) = sess
            .run_turn(
                "Call `mcp__symphony__symphony_handoff` when done.",
                None,
                None,
                &on_event,
            )
            .await;
        assert_eq!(tr.status, TURN_SUCCEEDED, "{err:?}");

        let argv: Vec<String> = std::fs::read_to_string(&dump)
            .expect("argv dump")
            .lines()
            .map(str::to_string)
            .collect();
        assert_eq!(
            argv.last().map(String::as_str),
            Some("Call `symphony_symphony_handoff` when done."),
            "the prompt must be LAST and rewritten into opencode's spelling: {argv:?}"
        );
        assert!(
            !argv.iter().any(|a| a.contains("mcp__")),
            "no claude tool spelling may reach the child: {argv:?}"
        );
        assert_eq!(argv[0], "run");
        assert!(argv.contains(&"--dir".to_string()) && argv.contains(&ws));
    }

    // ⚠️ The env the child actually gets: a PRIVATE XDG_DATA_HOME (never the operator's), the "me"
    // identity that the injected MCP server defaults its tools to, and no tracker credential.
    #[tokio::test]
    async fn the_child_gets_a_private_xdg_and_no_tracker_credential() {
        let _env = ENV_GUARD.read().await;
        let scripts = TempDir::new();
        let dump = scripts.path().join("env.txt");
        let body = format!(
            "env > \"{d}\"\n\
             printf '{{\"type\":\"step_finish\",\"sessionID\":\"ses_x\",\"part\":{{\"reason\":\"stop\"}}}}\\n'\n",
            d = dump.display()
        );
        let script = write_script(&scripts, "env.sh", &body);
        let auth = seeded_auth(&scripts);
        let state_root = TempDir::new();
        let root = TempDir::new();
        let ws = make_ws(&root, "w");

        let mut cfg = Config {
            command: format!("bash {script}"),
            workspace_root: root_path(&root),
            turn_timeout: std::time::Duration::from_secs(30),
            auth_source: auth.clone(),
            state_root: state_root.path().to_string_lossy().into_owned(),
            tracker_api_key: "lin_api_SECRET".to_string(),
            ..Default::default()
        };
        cfg.inject_mcp = false;
        let runner = Runner::new(cfg);
        let sess = runner
            .start_session(&ws, issue("STUDIO-902"), None)
            .await
            .expect("session");
        sess.set_run_id(77);
        let (_seen, on_event) = collector();
        let (tr, err) = sess.run_turn("p", None, None, &on_event).await;
        assert_eq!(tr.status, TURN_SUCCEEDED, "{err:?}");

        let text = std::fs::read_to_string(&dump).expect("env dump");
        let get = |k: &str| {
            text.lines()
                .find_map(|l| l.strip_prefix(&format!("{k}=")))
                .map(str::to_string)
        };
        let xdg = get("XDG_DATA_HOME").expect("XDG_DATA_HOME must be set");
        assert!(
            xdg.starts_with(&state_root.path().to_string_lossy().into_owned()),
            "the child must get its PRIVATE state dir, got {xdg}"
        );
        assert!(
            std::path::Path::new(&xdg)
                .join("opencode/auth.json")
                .is_file(),
            "the private state dir must carry the seeded credential"
        );
        assert_eq!(get("SYMPHONY_ISSUE").as_deref(), Some("STUDIO-902"));
        assert_eq!(get("SYMPHONY_RUN_ID").as_deref(), Some("77"));
        assert!(
            !text.contains("lin_api_SECRET"),
            "the tracker credential must be withheld by VALUE, not only by name"
        );
    }

    // `stop` really releases the state directory — unlike claude's no-op, this backend holds a
    // resource past the turn, and a daemon that leaked one per run would fill the disk.
    #[tokio::test]
    async fn stop_removes_the_private_state_directory() {
        let scripts = TempDir::new();
        let script = write_script(&scripts, "noop.sh", "exit 0\n");
        let auth = seeded_auth(&scripts);
        let state_root = TempDir::new();
        let root = TempDir::new();
        let ws = make_ws(&root, "w");

        let runner = runner_for(&script, &root, &auth, &state_root.path().to_string_lossy());
        let sess = runner
            .start_session(&ws, issue("STUDIO-902"), None)
            .await
            .expect("session");
        let before: Vec<_> = std::fs::read_dir(state_root.path())
            .expect("read")
            .filter_map(Result::ok)
            .collect();
        assert_eq!(before.len(), 1, "one state dir was provisioned");

        sess.stop().await.expect("stop");
        let after: Vec<_> = std::fs::read_dir(state_root.path())
            .expect("read")
            .filter_map(Result::ok)
            .map(|e| e.file_name())
            .collect();
        assert!(
            after.is_empty(),
            "stop must remove the state dir, left: {after:?}"
        );
    }

    // A turn is refused if the workspace escapes the configured root — the same containment
    // invariant the claude runner enforces, not a weaker one because this backend is newer.
    #[tokio::test]
    async fn a_workspace_outside_the_root_is_refused() {
        let _env = ENV_GUARD.read().await;
        let scripts = TempDir::new();
        let script = write_script(&scripts, "noop.sh", "exit 0\n");
        let auth = seeded_auth(&scripts);
        let state_root = TempDir::new();
        let root = TempDir::new();
        let outside = TempDir::new();
        let ws = make_ws(&outside, "elsewhere");

        let runner = runner_for(&script, &root, &auth, &state_root.path().to_string_lossy());
        let sess = runner
            .start_session(&ws, issue("STUDIO-902"), None)
            .await
            .expect("session");
        let (_seen, on_event) = collector();
        let (tr, err) = sess.run_turn("p", None, None, &on_event).await;
        assert_eq!(tr.status, TURN_FAILED);
        assert!(
            err.is_some(),
            "a workspace outside the root must be refused"
        );
    }

    /// CAPABILITY HONESTY (design §8; the ticket's mutation discipline). Every declared capability
    /// the committed STUDIO-869 capture can witness is crossed against it, so a declaration that
    /// claims something the measured CLI does not do reds here rather than silently breaking the
    /// refusal logic. Committed in-tree; no provider or CLI needed.
    #[test]
    fn declared_capabilities_match_the_committed_capture() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../harness/harness-spike/opencode");
        let read = |name: &str| {
            let p = root.join(name);
            std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
        };
        let happy = read("happy.jsonl");
        let resume = read("resume.jsonl");
        let caps = *Runner::new(Config::default()).capabilities();
        assert_eq!(
            crate::harness::declared_capabilities(HarnessId::Opencode),
            caps,
            "the by-name reader must return the SAME constant this adapter declares"
        );

        // events: FileLevel — the happy capture carries typed read/edit/bash tool events, the
        // granularity a command-only harness (codex) cannot reach.
        assert_eq!(
            caps.events,
            EventFidelity::Structured {
                tool_level: ToolEventGranularity::FileLevel
            }
        );
        assert!(
            happy.contains("\"tool\":\"read\"")
                && happy.contains("\"tool\":\"edit\"")
                && happy.contains("\"tool\":\"bash\""),
            "FileLevel evidence: happy.jsonl must carry typed read/edit/bash events"
        );

        // mcp: true + tool_naming: ServerUnderscoreTool — the capture calls the daemon's own tool
        // and spells it `<server>_<tool>`.
        assert!(
            caps.mcp,
            "the capture proves opencode reaches the daemon's tools"
        );
        assert!(
            happy.contains("symphony_symphony_state"),
            "ServerUnderscoreTool evidence: happy.jsonl must name the daemon's tool that way"
        );
        assert_eq!(caps.tool_naming, ToolNaming::ServerUnderscoreTool);

        // resume: Flags — the resume capture continues the SAME session the happy capture opened.
        assert_eq!(caps.resume, Resume::Flags);
        let session_id = |text: &str| {
            text.split("\"sessionID\":\"")
                .nth(1)
                .and_then(|rest| rest.split('"').next())
                .map(str::to_string)
        };
        let happy_sid = session_id(&happy).expect("happy capture carries a sessionID");
        let resume_sid = session_id(&resume).expect("resume capture carries a sessionID");
        assert_eq!(
            happy_sid, resume_sid,
            "resume evidence: the resume capture must continue the happy capture's session"
        );

        // opencode has approval (`--auto`), not a sandbox mode or a tool allowlist; it honours MCP
        // and that approval at once, so the pair is independent.
        assert_eq!(caps.sandbox, Sandbox::None);
        assert_eq!(caps.mcp_sandbox, McpSandboxCoupling::Independent);

        // stdin: ClosedAtStart is the measured difference from claude; the runner test
        // `the_prompt_is_a_positional_...` asserts the child observes it closed.
        assert_eq!(caps.stdin, StdinPolicy::ClosedAtStart);
    }

    // ⚠️ §9.4 mutation target: the turn must revoke its capability BEFORE the process-tree kill can
    // block. The production `TurnTeardown::drop` delegates to `teardown_turn`, so swapping the two
    // calls there — the exact reordering that used to leave every test green — reds this.
    #[test]
    fn teardown_revokes_the_capability_before_it_kills_the_process_tree() {
        let log = Arc::new(std::sync::Mutex::new(Vec::<&'static str>::new()));
        let revoke_sink = Arc::clone(&log);
        let kill_sink = Arc::clone(&log);
        teardown_turn(
            || revoke_sink.lock().expect("log").push("revoke"),
            || kill_sink.lock().expect("log").push("kill-tree"),
        );
        assert_eq!(
            *log.lock().expect("log"),
            vec!["revoke", "kill-tree"],
            "the turn capability must be revoked before the process-tree kill can block"
        );
    }

    // ---------------------------------------------------------------------------------------------
    // STUDIO-1043: a cut-off run keeps its session and the retry resumes it.
    // ---------------------------------------------------------------------------------------------

    /// A fake opencode that begins a session (`ses_cut`) and then blocks past the turn deadline —
    /// the shape of the STUDIO-1002 attempt 1 that burned an hour and was killed by `turn_timeout`.
    const FAKE_CUT_OFF: &str = r#"
printf '{"type":"step_start","sessionID":"ses_cut","part":{"type":"step-start"}}\n'
sleep 30
"#;

    fn record_file(state_root: &str, issue: &str) -> std::path::PathBuf {
        crate::opencode::resume::record_path(state_root, issue).expect("record path")
    }

    /// The per-session state directories under `state_root` (excluding the resume-record dir).
    fn session_dirs(state_root: &std::path::Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(state_root)
            .expect("read state root")
            .filter_map(Result::ok)
            .filter(|e| e.path().is_dir())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("rhapsody-opencode-") && n != "rhapsody-opencode-resume")
            .collect();
        v.sort();
        v
    }

    fn runner_with(
        script: &str,
        root: &TempDir,
        auth: &str,
        state_root: &str,
        model: &str,
        timeout_secs: u64,
    ) -> Runner {
        Runner::new(Config {
            command: format!("bash {script}"),
            workspace_root: root_path(root),
            turn_timeout: std::time::Duration::from_secs(timeout_secs),
            auth_source: auth.to_string(),
            state_root: state_root.to_string(),
            model: model.to_string(),
            ..Default::default()
        })
    }

    /// ⚠️ THE ACCEPTANCE TEST for STUDIO-1043. A turn killed by `turn_timeout` must keep its private
    /// state directory, and the next dispatch of the SAME issue must resume it: the OpenCode
    /// invocation carries `-s <sessionID>` and the SAME `XDG_DATA_HOME`, and the agent is told it is
    /// resuming. It turns red if the directory is deleted or `resume` stays false.
    #[tokio::test]
    async fn a_turn_timeout_retains_the_session_and_the_retry_resumes_it() {
        let _env = ENV_GUARD.read().await;
        let scripts = TempDir::new();
        let cut = write_script(&scripts, "cut.sh", FAKE_CUT_OFF);
        let auth = seeded_auth(&scripts);
        let state_root = TempDir::new();
        let root = TempDir::new();
        let ws = make_ws(&root, "w");
        let sr = state_root.path().to_string_lossy().into_owned();

        // --- Attempt 1: cut off by the deadline. ---
        let runner = runner_with(&cut, &root, &auth, &sr, "m", 1);
        let sess = runner
            .start_session(&ws, issue("STUDIO-1043"), None)
            .await
            .expect("session");
        let (_seen, on_event) = collector();
        let (tr, err) = sess.run_turn("first attempt", None, None, &on_event).await;
        assert_eq!(tr.status, TURN_TIMED_OUT, "{err:?}");
        assert_eq!(sess.thread_id(), "ses_cut");
        sess.stop().await.expect("stop");

        let dirs = session_dirs(state_root.path());
        assert_eq!(
            dirs.len(),
            1,
            "the cut-off session's state directory must survive: {dirs:?}"
        );
        let kept_dir = state_root.path().join(&dirs[0]);
        let rec = record_file(&sr, "STUDIO-1043");
        assert!(rec.is_file(), "the session must be recorded for resume");

        // --- Attempt 2: the retry resumes the recorded session. ---
        let argv_log = scripts.path().join("argv.txt");
        let env_log = scripts.path().join("env.txt");
        const RESUME: &str = r#"
printf '%s\n' "$*" > "ARGV"
env > "ENV"
printf '{"type":"step_finish","sessionID":"ses_cut","part":{"reason":"stop"}}\n'
"#;
        let body = RESUME
            .replace("ARGV", &argv_log.display().to_string())
            .replace("ENV", &env_log.display().to_string());
        let clean = write_script(&scripts, "clean.sh", &body);
        let retry = runner_with(&clean, &root, &auth, &sr, "m", 30);
        let sess2 = retry
            .start_session(&ws, issue("STUDIO-1043"), None)
            .await
            .expect("retry session");
        assert_eq!(
            sess2.thread_id(),
            "ses_cut",
            "the retry must adopt the recorded session id"
        );

        let (seen2, on2) = collector();
        let (tr2, err2) = sess2.run_turn("second attempt", None, None, &on2).await;
        assert_eq!(tr2.status, TURN_SUCCEEDED, "{err2:?}");
        assert!(
            events_of(&seen2)
                .iter()
                .any(|e| e.event_type == EVENT_NOTIFICATION
                    && e.message.contains("resumed the cut-off opencode session")),
            "the run's event stream must record that the attempt resumed: {:#?}",
            events_of(&seen2)
        );

        let argv = std::fs::read_to_string(&argv_log).expect("argv log");
        assert!(
            argv.contains("-s ses_cut"),
            "the retry must carry the resume/session argument: {argv}"
        );
        assert!(
            argv.contains("Resuming a cut-off attempt"),
            "the agent must be told it is resuming: {argv}"
        );
        assert!(
            argv.trim_end().ends_with("second attempt"),
            "the resume note must not displace the prompt as the final positional: {argv}"
        );
        let env = std::fs::read_to_string(&env_log).expect("env log");
        let xdg = env
            .lines()
            .find_map(|l| l.strip_prefix("XDG_DATA_HOME="))
            .expect("XDG_DATA_HOME");
        assert_eq!(
            std::path::Path::new(xdg),
            kept_dir,
            "the retry must use the SAME private data root"
        );

        // A successful run releases the retained session.
        sess2.stop().await.expect("stop");
        assert!(
            session_dirs(state_root.path()).is_empty(),
            "a completed run must remove the retained directory"
        );
        assert!(!rec.exists(), "a completed run must clear the record");
    }

    /// A cancelled run — an operator Stop, or a daemon shutdown dropping the worker future — never
    /// reaches `stop`, so `Drop` must make the same retention decision. Dropping the session future
    /// mid-turn must leave the directory and the record behind (STUDIO-1043 review F1).
    #[tokio::test]
    async fn a_cancelled_run_keeps_the_session_without_calling_stop() {
        let _env = ENV_GUARD.read().await;
        let scripts = TempDir::new();
        let cut = write_script(&scripts, "cut.sh", FAKE_CUT_OFF);
        let auth = seeded_auth(&scripts);
        let state_root = TempDir::new();
        let root = TempDir::new();
        let ws = make_ws(&root, "w");
        let sr = state_root.path().to_string_lossy().into_owned();

        // A turn deadline long enough that only the outer timeout can end the turn, so the drop is
        // the cancellation path and not a `turn_timeout` return.
        let runner = runner_with(&cut, &root, &auth, &sr, "m", 30);
        let sess = runner
            .start_session(&ws, issue("STUDIO-1043"), None)
            .await
            .expect("session");
        let (_s, on) = collector();
        let cancelled = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            sess.run_turn("cut off mid-turn", None, None, &on),
        )
        .await;
        assert!(
            cancelled.is_err(),
            "the outer timeout must cut the turn off"
        );
        // The worker future is dropped without `stop()` — exactly the operator-Stop shape.
        drop(sess);

        let dirs = session_dirs(state_root.path());
        assert_eq!(
            dirs.len(),
            1,
            "a cancelled run must keep its state directory: {dirs:?}"
        );
        assert!(
            record_file(&sr, "STUDIO-1043").is_file(),
            "a cancelled run must be recorded for the next dispatch"
        );
    }

    /// An adopted session that ends before its first turn (a launch refusal, a shutdown between
    /// dispatch and turn 1) must not delete the session it was resuming (STUDIO-1043 review F2).
    #[tokio::test]
    async fn an_adopted_session_that_ends_before_its_first_turn_keeps_the_session() {
        let _env = ENV_GUARD.read().await;
        let scripts = TempDir::new();
        let cut = write_script(&scripts, "cut.sh", FAKE_CUT_OFF);
        let auth = seeded_auth(&scripts);
        let state_root = TempDir::new();
        let root = TempDir::new();
        let ws = make_ws(&root, "w");
        let sr = state_root.path().to_string_lossy().into_owned();

        // Attempt 1 is cut off and retained.
        let runner = runner_with(&cut, &root, &auth, &sr, "m", 1);
        let sess = runner
            .start_session(&ws, issue("STUDIO-1043"), None)
            .await
            .expect("session");
        let (_s, on) = collector();
        let (_tr, _e) = sess.run_turn("p", None, None, &on).await;
        sess.stop().await.expect("stop");
        let kept = session_dirs(state_root.path());
        assert_eq!(kept.len(), 1);
        let rec = record_file(&sr, "STUDIO-1043");
        assert!(rec.is_file());

        // Attempt 2 adopts the session, then ends without ever running a turn.
        let retry = runner_with(&cut, &root, &auth, &sr, "m", 30);
        let sess2 = retry
            .start_session(&ws, issue("STUDIO-1043"), None)
            .await
            .expect("adopting session");
        assert_eq!(sess2.thread_id(), "ses_cut", "attempt 2 adopts the session");
        sess2.stop().await.expect("stop");
        drop(sess2);

        assert_eq!(
            session_dirs(state_root.path()),
            kept,
            "the adopted session's directory must survive a pre-turn ending"
        );
        assert!(
            rec.is_file(),
            "the record must survive a pre-turn ending so a later dispatch can resume"
        );
    }

    /// A model mismatch falls back to a cold start with its own directory and no `-s`.
    #[tokio::test]
    async fn a_model_mismatch_starts_cold() {
        let _env = ENV_GUARD.read().await;
        let scripts = TempDir::new();
        let cut = write_script(&scripts, "cut.sh", FAKE_CUT_OFF);
        let auth = seeded_auth(&scripts);
        let state_root = TempDir::new();
        let root = TempDir::new();
        let ws = make_ws(&root, "w");
        let sr = state_root.path().to_string_lossy().into_owned();

        let runner = runner_with(&cut, &root, &auth, &sr, "model-one", 1);
        let sess = runner
            .start_session(&ws, issue("STUDIO-1043"), None)
            .await
            .expect("session");
        let (_s, on) = collector();
        let (_tr, _e) = sess.run_turn("p", None, None, &on).await;
        sess.stop().await.expect("stop");
        let kept = session_dirs(state_root.path());
        assert_eq!(kept.len(), 1);

        let argv_log = scripts.path().join("argv.txt");
        const CLEAN: &str = r#"
printf '%s\n' "$*" > "ARGV"
printf '{"type":"step_finish","sessionID":"ses_new","part":{"reason":"stop"}}\n'
"#;
        let body = CLEAN.replace("ARGV", &argv_log.display().to_string());
        let clean = write_script(&scripts, "clean.sh", &body);
        let retry = runner_with(&clean, &root, &auth, &sr, "model-two", 30);
        let sess2 = retry
            .start_session(&ws, issue("STUDIO-1043"), None)
            .await
            .expect("cold session");
        assert!(
            sess2.thread_id().is_empty(),
            "a model mismatch must start cold, got {}",
            sess2.thread_id()
        );
        let (_s2, on2) = collector();
        let (tr, err) = sess2.run_turn("p", None, None, &on2).await;
        assert_eq!(tr.status, TURN_SUCCEEDED, "{err:?}");
        let argv = std::fs::read_to_string(&argv_log).expect("argv log");
        assert!(
            !argv.contains(" -s "),
            "a cold start must carry no resume argument: {argv}"
        );
        // The rejected session's directory was discarded, not kept alongside the new one.
        assert_eq!(
            session_dirs(state_root.path()).len(),
            1,
            "only the new session may remain"
        );
    }

    /// A missing directory falls back to a cold start.
    #[tokio::test]
    async fn a_missing_directory_starts_cold() {
        let _env = ENV_GUARD.read().await;
        let scripts = TempDir::new();
        let cut = write_script(&scripts, "cut.sh", FAKE_CUT_OFF);
        let auth = seeded_auth(&scripts);
        let state_root = TempDir::new();
        let root = TempDir::new();
        let ws = make_ws(&root, "w");
        let sr = state_root.path().to_string_lossy().into_owned();

        let runner = runner_with(&cut, &root, &auth, &sr, "m", 1);
        let sess = runner
            .start_session(&ws, issue("STUDIO-1043"), None)
            .await
            .expect("session");
        let (_s, on) = collector();
        let (_tr, _e) = sess.run_turn("p", None, None, &on).await;
        sess.stop().await.expect("stop");

        for d in session_dirs(state_root.path()) {
            std::fs::remove_dir_all(state_root.path().join(d)).expect("remove kept dir");
        }
        let clean = write_script(
            &scripts,
            "clean.sh",
            "printf '{\"type\":\"step_finish\",\"sessionID\":\"ses_x\",\"part\":{\"reason\":\"stop\"}}\n'\n",
        );
        let retry = runner_with(&clean, &root, &auth, &sr, "m", 30);
        let sess2 = retry
            .start_session(&ws, issue("STUDIO-1043"), None)
            .await
            .expect("cold session");
        assert!(
            sess2.thread_id().is_empty(),
            "a missing directory must start cold"
        );
    }

    /// A successful cold run removes its directory and leaves no record — the legacy cleanup the
    /// ticket requires to be unchanged.
    #[tokio::test]
    async fn a_successful_run_leaves_no_session_and_no_record() {
        let _env = ENV_GUARD.read().await;
        let scripts = TempDir::new();
        let clean = write_script(
            &scripts,
            "clean.sh",
            "printf '{\"type\":\"step_finish\",\"sessionID\":\"ses_x\",\"part\":{\"reason\":\"stop\"}}\n'\n",
        );
        let auth = seeded_auth(&scripts);
        let state_root = TempDir::new();
        let root = TempDir::new();
        let ws = make_ws(&root, "w");
        let sr = state_root.path().to_string_lossy().into_owned();

        let runner = runner_with(&clean, &root, &auth, &sr, "m", 30);
        let sess = runner
            .start_session(&ws, issue("STUDIO-1043"), None)
            .await
            .expect("session");
        let (_s, on) = collector();
        let (tr, err) = sess.run_turn("p", None, None, &on).await;
        assert_eq!(tr.status, TURN_SUCCEEDED, "{err:?}");
        sess.stop().await.expect("stop");
        assert!(
            session_dirs(state_root.path()).is_empty(),
            "a successful run cleans its state directory up, as before"
        );
        assert!(!record_file(&sr, "STUDIO-1043").exists());
    }
}
