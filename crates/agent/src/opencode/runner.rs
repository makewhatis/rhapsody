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

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

use async_trait::async_trait;
use chrono::Utc;
use rhapsody_core::Issue;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::claude::{TRACKER_ENV_VARS, append_me_env, append_review_env, scrub_env, split_command};
use crate::harness::{
    EventFidelity, Harness, HarnessCapabilities, HarnessId, Resume, Sandbox, StdinPolicy, Steering,
    ToolEventGranularity, ToolNaming, UsageDetail,
};
use crate::opencode::args::{Config, build_args};
use crate::opencode::mcpinject::{inject_daemon_mcp, rewrite_tool_names};
use crate::opencode::parse::{Failure, add_usage, classify};
use crate::opencode::state::RunState;
use crate::proctree::{KillTreeOnDrop, kill_tree};
use crate::{
    AgentError, EVENT_SESSION_STARTED, EVENT_STARTUP_FAILED, EVENT_TURN_COMPLETED,
    EVENT_TURN_FAILED, Event, Session, TURN_FAILED, TURN_SUCCEEDED, TURN_TIMED_OUT, Transcript,
    TurnResult, Usage,
};

/// Mirrors the claude runner's bounds so a pathological stream is capped the same way on both.
const MAX_STDOUT_LINE: usize = 10 * 1024 * 1024;
const MAX_STDERR_CAPTURE: usize = 256 * 1024;
const READ_CHUNK: usize = 64 * 1024;
const MAX_STDERR_MESSAGE: usize = 2048;
/// Bounds the final result text (the `HANDOFF:` marker is at the END, so the TAIL is kept).
const MAX_RESULT_TEXT: usize = 4096;

/// Builds opencode sessions.
pub struct Runner {
    cfg: Config,
}

impl Runner {
    /// Materializes the zero-value defaults the config layer does not supply.
    pub fn new(mut cfg: Config) -> Runner {
        if cfg.command.is_empty() {
            cfg.command = "opencode".to_string();
        }
        Runner { cfg }
    }
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
const CAPABILITIES: HarnessCapabilities = HarnessCapabilities {
    events: EventFidelity::Structured {
        tool_level: ToolEventGranularity::FileLevel,
    },
    steering: Steering::BetweenTurns,
    resume: Resume::Flags,
    mcp: true,
    sandbox: Sandbox::None,
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
        let state = RunState::provision(
            &self.cfg.state_root,
            &self.cfg.auth_source,
            &issue.identifier,
        )?;

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
                Ok(Some((path, _))) => config_path = path.to_string_lossy().into_owned(),
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
            session_id: Mutex::new(String::new()),
            turn_n: AtomicI64::new(0),
            transcript: Mutex::new(transcript),
            transcript_warned: AtomicBool::new(false),
            run_id: AtomicI64::new(0),
            review_head: Mutex::new(String::new()),
            model_override: Mutex::new(crate::ModelOverride::default()),
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
    /// opencode's `ses_…` id, captured from the first line that carries one.
    session_id: Mutex<String>,
    turn_n: AtomicI64,
    transcript: Mutex<Option<Transcript>>,
    transcript_warned: AtomicBool,
    run_id: AtomicI64,
    review_head: Mutex<String>,
    model_override: Mutex<crate::ModelOverride>,
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

    /// Removes the session's private state directory. Unlike claude's no-op `stop`, this backend
    /// really does hold a resource past the turn.
    async fn stop(&self) -> Result<(), AgentError> {
        self.state.cleanup();
        Ok(())
    }

    async fn run_turn(
        &self,
        prompt: &str,
        attempt: Option<i64>,
        mut messages: Option<&mut mpsc::Receiver<String>>,
        on_event: &(dyn Fn(Event) + Send + Sync),
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
        tracing::info!(
            issue = %self.issue.identifier,
            turn = turn_n,
            attempt = attempt.unwrap_or(-1),
            resume = !resume.is_empty(),
            "opencode turn start"
        );

        // ⚠️ The prompt is rewritten into opencode's tool-name spelling before it is sent. Rhapsody's
        // prompts name tools literally, including the one that ends the run.
        let prompt = rewrite_tool_names(prompt);
        let cfg = self.turn_cfg();
        let mut args = self.cmd_args.clone();
        args.extend(build_args(&cfg, &self.ws_path, &resume, &prompt));

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
        let mut env = append_review_env(env, &self.locked_review_head());
        // ⚠️ The isolation that stops concurrent turns being LOST (see `super::state`). Pushed AFTER
        // the scrub so it cannot be filtered out, and set unconditionally so an operator's own
        // XDG_DATA_HOME is overridden rather than merged with.
        env.retain(|kv| !kv.starts_with("XDG_DATA_HOME="));
        env.push(format!(
            "XDG_DATA_HOME={}",
            self.state.xdg_data_home().to_string_lossy()
        ));
        if !self.config_path.is_empty() {
            env.retain(|kv| !kv.starts_with("OPENCODE_CONFIG="));
            env.push(format!("OPENCODE_CONFIG={}", self.config_path));
        }
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

        let mut tree_kill = KillTreeOnDrop::new(pid);

        let mut usage = Usage::default();
        let mut result_text = String::new();
        let mut terminal_seen = false;
        let mut failure: Option<Failure> = None;
        let mut scan_err: Option<String> = None;
        let mut timed_out = false;
        let mut stderr_open = true;
        let mut session_announced = !self.thread_id().is_empty();
        let mut mailbox_open = messages.is_some();
        let mut dropped_messages = 0usize;
        let mut stderr_buf = CappedBuffer::new(MAX_STDERR_CAPTURE);
        let mut acc: Vec<u8> = Vec::with_capacity(READ_CHUNK);
        let mut out_chunk = vec![0u8; READ_CHUNK];
        let mut err_chunk = vec![0u8; READ_CHUNK];

        let deadline = tokio::time::sleep_until(Instant::now() + cfg.turn_timeout);
        tokio::pin!(deadline);

        'outer: loop {
            tokio::select! {
                _ = &mut deadline => {
                    kill_tree(pid);
                    timed_out = true;
                    break 'outer;
                }
                r = stdout.read(&mut out_chunk) => {
                    match r {
                        Ok(0) => break 'outer, // EOF — the terminal condition for this harness
                        Ok(n) => {
                            acc.extend_from_slice(&out_chunk[..n]);
                            loop {
                                let Some(pos) = acc.iter().position(|&b| b == b'\n') else {
                                    if acc.len() > MAX_STDOUT_LINE {
                                        scan_err = Some("token too long".to_string());
                                        break 'outer;
                                    }
                                    break;
                                };
                                let mut line: Vec<u8> = acc.drain(..=pos).collect();
                                line.pop();
                                if line.len() > MAX_STDOUT_LINE {
                                    scan_err = Some("token too long".to_string());
                                    break 'outer;
                                }
                                self.tee_stdout(&line);
                                let c = classify(&line);
                                // Every line carries the session id and there is no announcement
                                // line, so seed from whichever arrives first — on the measured
                                // captures that is a `step_start`, which is not surfaced as an
                                // event at all.
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
                                if !c.text.is_empty() {
                                    result_text = c.text.clone();
                                }
                                if let Some(f) = c.failure.clone() {
                                    // First error wins: later ones are consequences of it.
                                    failure.get_or_insert(f);
                                }
                                if c.ok {
                                    let mut ev = c.event.clone();
                                    ev.pid = pid as i64;
                                    on_event(ev);
                                }
                                if c.terminal {
                                    terminal_seen = true;
                                }
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
                        Ok(0) => stderr_open = false,
                        Ok(n) => {
                            stderr_buf.write(&err_chunk[..n]);
                            self.tee_stderr(&err_chunk[..n]);
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
                            stderr_buf.write(&buf[..n]);
                            self.tee_stderr(&buf[..n]);
                        }
                    }
                }
            }
        };
        tokio::join!(drain_out, drain_err);
        let wait_res = child.wait().await;
        tree_kill.disarm();

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
            on_event(Event {
                event_type: EVENT_TURN_FAILED.to_string(),
                timestamp: Some(Utc::now()),
                pid: pid as i64,
                message: f.summary(),
                ..Default::default()
            });
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
