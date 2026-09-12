//! The Claude Code subprocess runner — parity port of Go `runner.go`.
//!
//! [`Runner`] drives `claude` headlessly, one process per turn, inside the per-issue worktree. Each
//! [`Session::run_turn`] spawns the child with the exact argv ([`build_args`]), in its own Unix
//! process group so a stall/turn-deadline kill can `SIGKILL` the whole group (the agent's own
//! children — e.g. a background stdin drain — die with it). stdin is held open as an operator-message
//! mailbox that is continuously drained and folded into the live turn at the next step boundary,
//! closed the instant the terminal result lands so nothing is ever written after it (INF-250). The
//! turn deadline (Go's `context.WithTimeout`) is the single timeout: a hang produces `TurnTimedOut`
//! (the "stalled" lifecycle), exactly as the reference does — the reference has no separate
//! runner-level "stall timeout" (that knob is orchestrator-level; see `fake_claude_test.go`).
//!
//! Porting shape: Go runs two goroutines (a stdin writer + an stdout scanner) coordinated by a
//! `stdinDone` channel. Async Rust folds both — plus the stderr drain and the deadline — into ONE
//! `tokio::select!` task. The borrowed `messages`/`on_event` parameters have no `'static` lifetime,
//! so they cannot move into a spawned task; a single cancel-safe loop is the faithful equivalent and
//! makes "no write after result" structural (once the terminal line is read, the mailbox arm is
//! guarded off). Go's OTel span + `TRACEPARENT` injection (`tracing_test.go`) are NOT ported: the
//! crate carries no OpenTelemetry dependency and `args.rs` already dropped the Go logger — that
//! observability readiness is out of the port's behavioral scope.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use rhapsody_core::Issue;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::claude::{
    Config, TRACKER_ENV_VARS, append_me_env, append_review_env, billing_guard_enabled,
    billing_guard_ok, build_args, classify, inject_daemon_mcp, scrub_env, scrubbed_env_vars,
    split_command,
};
use crate::{
    AgentError, EVENT_OPERATOR_MESSAGE, EVENT_SESSION_STARTED, EVENT_STARTUP_FAILED,
    EVENT_TURN_FAILED, Event, Session, TURN_FAILED, TURN_SUCCEEDED, TURN_TIMED_OUT, Transcript,
    TurnResult, Usage,
};

/// Max bytes in one stream-json stdout line before the scan fails (upstream §10.1). A single line
/// larger than this is the parity mirror of Go's `bufio.ErrTooLong`.
const MAX_STDOUT_LINE: usize = 10 * 1024 * 1024; // 10 MB

/// How much stderr we retain per turn. A noisy agent can emit unbounded stderr; without a cap the
/// daemon's memory grows with it (Go `maxStderrCapture`).
const MAX_STDERR_CAPTURE: usize = 256 * 1024; // 256 KB

/// Read-chunk size for the stdout/stderr pipes (the initial `bufio.Scanner` buffer in Go).
const READ_CHUNK: usize = 64 * 1024;

/// Bounds the stderr text folded into a failure error (Go `truncateStderr`).
const MAX_STDERR_MESSAGE: usize = 2048;

/// The Claude Code agent backend (Go `Runner`).
pub struct Runner {
    cfg: Config,
}

impl Runner {
    /// Builds a Claude [`Runner`], applying the same defaults as Go `New`: an empty command becomes
    /// `"claude"`, and a zero turn timeout becomes one hour (upstream §5.3.6 default
    /// `turn_timeout_ms = 3600000`).
    pub fn new(mut cfg: Config) -> Runner {
        if cfg.command.is_empty() {
            cfg.command = "claude".to_string();
        }
        if cfg.turn_timeout.is_zero() {
            cfg.turn_timeout = Duration::from_secs(3600);
        }
        Runner { cfg }
    }
}

#[async_trait]
impl crate::Runner for Runner {
    async fn start_session(
        &self,
        workspace_path: &str,
        issue: Issue,
        transcript: Option<Transcript>,
    ) -> Result<Box<dyn Session>, AgentError> {
        let (name, base_args) = split_command(&self.cfg.command)?;
        // sess.cfg is a value copy of the runner's cfg, so overriding its mcp_config affects only
        // this session.
        let mut cfg = self.cfg.clone();
        // MCP injection (INF-473): MERGE this daemon's server into the session's mcp_config (under
        // the `symphony` key — the agent's tool namespace, a live contract) so the dispatched agent
        // can query run/daemon state. Best-effort — on any failure keep the operator's original
        // config unchanged; injection never blocks a run.
        if cfg.inject_mcp {
            match inject_daemon_mcp(
                workspace_path,
                &cfg.mcp_config,
                &cfg.daemon_bin,
                &cfg.workflow_path,
            ) {
                Ok((path, kept_operator)) => {
                    cfg.mcp_config = path;
                    if kept_operator {
                        tracing::warn!(
                            issue = %issue.identifier,
                            "mcp injection: operator already defines a `symphony` MCP server; keeping theirs"
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        issue = %issue.identifier, err = %e,
                        "mcp injection failed; proceeding with the operator's mcp_config unchanged"
                    );
                }
            }
        }
        Ok(Box::new(ClaudeSession {
            cfg,
            cmd_name: name,
            cmd_args: base_args,
            ws_path: workspace_path.to_string(),
            issue,
            thread_id: Mutex::new(String::new()),
            turn_n: AtomicI64::new(0),
            transcript: Mutex::new(transcript),
            transcript_warned: AtomicBool::new(false),
            run_id: AtomicI64::new(0),
            review_head: Mutex::new(String::new()),
            model_override: Mutex::new(crate::ModelOverride::default()),
        }))
    }
}

/// One live Claude conversation for one issue (Go `session`). Per-turn state that Go mutates on the
/// value receiver (`turnN`, `threadID`, the transcript sink, the warn-once flag) lives behind
/// interior mutability here because the [`Session`] trait's `run_turn` takes `&self`.
struct ClaudeSession {
    cfg: Config,
    cmd_name: String,
    cmd_args: Vec<String>,
    ws_path: String,
    issue: Issue,
    /// The stable backend thread/session id, seeded from the first line carrying a `session_id`
    /// (Go `s.threadID`). Read for `--resume` and by [`Session::thread_id`]/[`Session::id`].
    thread_id: Mutex<String>,
    /// 1-based turn counter (Go `s.turnN`).
    turn_n: AtomicI64,
    /// Local raw-I/O capture sink (Go `s.transcript`); `None` disables capture.
    transcript: Mutex<Option<Transcript>>,
    /// Warn-once guard for a failing transcript sink (Go `s.transcriptWarned` + `bestEffortStderr`'s
    /// `sync.Once`, unified here — the warning is observability only, never asserted).
    transcript_warned: AtomicBool,
    /// The store run-row id (Go `s.runID`), set once by the worker via [`Session::set_run_id`]
    /// after `start_session` and before the first turn; `0` when unknown (store disabled, or a
    /// coordinator session that has no run). Injected into the agent child's env as
    /// `SYMPHONY_RUN_ID`/`RHAPSODY_RUN_ID` so the daemon's own MCP server can resolve WHICH run is
    /// calling — the whole basis on which `teams_post` / `teams_retain` stamp an author
    /// (STUDIO-675).
    ///
    /// Atomic rather than plain: `set_run_id` takes `&self`, and the value is read on every turn.
    run_id: AtomicI64,
    /// The pull-request head SHA this session is REVIEWING, set once by the worker via
    /// [`Session::set_review_head`] before the first turn; empty for every non-review run, which
    /// emits no review env at all (STUDIO-715). Behind a `Mutex` for the same reason `thread_id`
    /// is: the setter takes `&self` and the value is read on every turn.
    review_head: Mutex<String>,
    /// The dispatched teammate's profile model/effort, set once by the worker via
    /// [`Session::set_model_override`] before the first turn; empty for every run that routed to
    /// nobody or whose profile names neither, which leaves the argv byte-identical (STUDIO-868).
    /// Behind a `Mutex` for the same reason `review_head` is.
    model_override: Mutex<crate::ModelOverride>,
}

impl ClaudeSession {
    /// Locks `thread_id`, recovering the guard on poison (a panic while holding it, which this code
    /// never does) so the accessor stays panic-free under `-D warnings`.
    fn locked_thread_id(&self) -> std::sync::MutexGuard<'_, String> {
        self.thread_id.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Locks `review_head`, recovering the guard on poison, exactly as [`Self::locked_thread_id`] does.
    fn locked_review_head(&self) -> std::sync::MutexGuard<'_, String> {
        self.review_head.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Locks `model_override`, recovering the guard on poison, exactly as [`Self::locked_thread_id`]
    /// does.
    fn locked_model_override(&self) -> std::sync::MutexGuard<'_, crate::ModelOverride> {
        self.model_override
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// The `[teammate …]` clause a failure message carries when this session's model/effort came
    /// from a dispatched teammate's profile; EMPTY when it did not, so an unrouted run's error text
    /// is unchanged (STUDIO-868). Only the fields the profile actually named are listed — reporting
    /// an inherited global as though the profile had chosen it would misdirect the reader.
    fn model_attribution(&self) -> String {
        let over = self.locked_model_override();
        if over.is_empty() {
            return String::new();
        }
        let mut parts = Vec::new();
        if !over.model.is_empty() {
            parts.push(format!("--model {}", over.model));
        }
        if !over.effort.is_empty() {
            parts.push(format!("--effort {}", over.effort));
        }
        let who = if over.identity.is_empty() {
            "a profile".to_string()
        } else {
            format!("teammate {}", over.identity)
        };
        format!("[{who} asked for {}] ", parts.join(" "))
    }

    /// The config THIS turn's argv is built from: the session's own copy of the runner's config
    /// (`start_session` already clones it, which is how per-session `mcp_config` injection works),
    /// with the dispatched teammate's profile model/effort substituted where the profile names one
    /// (STUDIO-868).
    ///
    /// Per-turn rather than applied once at `start_session`, because the override arrives AFTER the
    /// session is built — the worker calls `set_model_override` on a `Box<dyn Session>`, exactly as
    /// it calls `set_run_id`. One `Config` clone per turn, against a process spawn.
    ///
    /// Precedence is per FIELD and non-empty-wins: a profile naming only a model keeps the global
    /// effort. Empty means INHERIT, never "clear", so an installation with no profiles directory —
    /// and every built-in profile, all of which ship `model: ""` — builds the same argv it always
    /// did.
    fn turn_cfg(&self) -> Config {
        let over = self.locked_model_override();
        if over.is_empty() {
            return self.cfg.clone();
        }
        let mut cfg = self.cfg.clone();
        if !over.model.is_empty() {
            cfg.model = over.model.clone();
        }
        if !over.effort.is_empty() {
            cfg.effort = over.effort.clone();
        }
        cfg
    }

    /// Tees one raw stdout line (+ newline) to the transcript BEFORE classification so even
    /// unclassified lines are captured at full fidelity (design spec §3). Best-effort: a sink write
    /// failure is warned once and never aborts the turn.
    fn tee_stdout(&self, line: &[u8]) {
        let mut guard = self.transcript.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(w) = guard.as_mut().and_then(|t| t.stdout.as_mut()) {
            use std::io::Write as _;
            if w.write_all(line).and_then(|()| w.write_all(b"\n")).is_err() {
                self.warn_transcript_once();
            }
        }
    }

    /// Tees a stderr chunk to the transcript sink (best-effort). The capped in-memory buffer — the
    /// authoritative diagnostic — is written by the caller regardless, so a failing sink here can
    /// never discard the real stderr (Go `bestEffortStderr`).
    fn tee_stderr(&self, chunk: &[u8]) {
        let mut guard = self.transcript.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(w) = guard.as_mut().and_then(|t| t.stderr.as_mut()) {
            use std::io::Write as _;
            if w.write_all(chunk).is_err() {
                self.warn_transcript_once();
            }
        }
    }

    fn warn_transcript_once(&self) {
        if !self.transcript_warned.swap(true, Ordering::SeqCst) {
            tracing::warn!(
                issue = %self.issue.identifier,
                "transcript write failed; agent output may be incompletely logged"
            );
        }
    }
}

#[async_trait]
impl Session for ClaudeSession {
    fn id(&self) -> String {
        format!(
            "{}-{}",
            self.locked_thread_id(),
            self.turn_n.load(Ordering::SeqCst)
        )
    }

    fn thread_id(&self) -> String {
        self.locked_thread_id().clone()
    }

    /// Mirrors Go `func (s *session) SetRunID(id int64) { s.runID = id }` — a plain store, guarded
    /// against a zero id by [`append_me_env`], which omits the variable entirely for `0`.
    fn set_run_id(&self, id: i64) {
        self.run_id.store(id, Ordering::SeqCst);
    }

    /// Stores the review-mode head SHA (STUDIO-715). Guarded against an empty value by
    /// [`append_review_env`], which emits nothing for it — so a non-review session's child env is
    /// unchanged.
    fn set_review_head(&self, sha: &str) {
        *self.locked_review_head() = sha.to_string();
    }

    /// Stores the dispatched teammate's profile model/effort (STUDIO-868). An all-empty override is
    /// stored as-is and read back as "inherit everything" by [`ClaudeSession::turn_cfg`], so it is
    /// indistinguishable from never having been called.
    fn set_model_override(&self, over: crate::ModelOverride) {
        *self.locked_model_override() = over;
    }

    async fn stop(&self) -> Result<(), AgentError> {
        Ok(()) // per-turn processes; nothing persistent
    }

    async fn run_turn(
        &self,
        prompt: &str,
        attempt: Option<i64>,
        mut messages: Option<&mut mpsc::Receiver<String>>,
        on_event: &(dyn Fn(Event) + Send + Sync),
    ) -> (TurnResult, Option<AgentError>) {
        // Safety invariant: the workspace must be inside the root and equal to the cwd (upstream
        // §9.5). Go's runner imports `internal/workspace` for this exact check; the port reuses the
        // committed `validate_launch` rather than re-deriving a security invariant.
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
        tracing::info!(
            issue = %self.issue.identifier,
            turn = turn_n,
            attempt = attempt.unwrap_or(-1),
            resume = !self.thread_id().is_empty(),
            "agent turn start"
        );

        let guard_on = billing_guard_enabled(self.cfg.billing_guard);

        // argv: base command args ++ per-turn flags, resuming from the captured thread id.
        let resume = self.thread_id();
        let mut args = self.cmd_args.clone();
        args.extend(build_args(&self.turn_cfg(), &resume));

        let mut cmd = Command::new(&self.cmd_name);
        cmd.args(&args);
        cmd.current_dir(&self.ws_path);
        // Env scrub (re-applied every turn, incl. --resume turns): the tracker credential is ALWAYS
        // withheld (by name AND by value); billing/routing vars are withheld only when the guard is
        // on. appendMeEnv adds the "me" identity AFTER the scrub so it survives, including
        // SYMPHONY_RUN_ID when the worker threaded one on via `set_run_id` (STUDIO-675). A session
        // nobody set an id on still emits SYMPHONY_ISSUE alone, matching Go's SetRunID-uncalled
        // default.
        let drop_names: Vec<&str> = if guard_on {
            scrubbed_env_vars()
        } else {
            TRACKER_ENV_VARS.to_vec()
        };
        let base_env: Vec<String> = std::env::vars_os()
            .map(|(k, v)| format!("{}={}", k.to_string_lossy(), v.to_string_lossy()))
            .collect();
        let scrubbed = scrub_env(&base_env, &drop_names, &[self.cfg.tracker_api_key.as_str()]);
        let env = append_me_env(
            scrubbed,
            &self.issue.identifier,
            self.run_id.load(Ordering::SeqCst),
        );
        // Review-mode only, and additive: empty (every non-review run) emits nothing (STUDIO-715).
        let env = append_review_env(env, &self.locked_review_head());
        cmd.env_clear();
        for kv in &env {
            if let Some((k, v)) = kv.split_once('=') {
                cmd.env(k, v);
            }
        }
        // New process group so the deadline/stall kill can SIGKILL the whole group (Go's
        // `SysProcAttr{Setpgid: true}` + `syscall.Kill(-pid, SIGKILL)`).
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
        let (mut stdin, mut stdout, mut stderr) =
            match (child.stdin.take(), child.stdout.take(), child.stderr.take()) {
                (Some(i), Some(o), Some(e)) => (Some(i), o, e),
                _ => {
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                    return (failed(Usage::default()), Some(AgentError::StartupFailed));
                }
            };

        // Every await below can be reached by a DROP rather than a return: an operator Stop (and a
        // daemon shutdown) cancels a run by dropping this future, and the child leads its own process
        // group, so it would otherwise run on unattended. Arm the group kill for the whole turn and
        // disarm it only once the child is reaped (STUDIO-840).
        let mut group_kill = KillGroupOnDrop(pid);

        // The FIRST stdin line is the prompt as one stream-json user message (INF-250). A write
        // failure (child never drained stdin / exited early) is not fatal — the scan loop still runs.
        match encode_user_message(prompt) {
            Ok(line) => {
                if let Some(s) = stdin.as_mut() {
                    let _ = s.write_all(&line).await;
                }
            }
            Err(e) => tracing::error!(
                issue = %self.issue.identifier, err = %e, "encode initial prompt failed"
            ),
        }

        let mut usage = Usage::default();
        let mut result: Option<TurnResult> = None;
        let mut scan_err: Option<String> = None;
        let mut billing_failed = false;
        // Per-turn (not per-session): each turn is a fresh `claude` process and `--resume` re-emits
        // its own system/init with apiKeySource every turn, so the assertion fires on the first
        // system/init of EVERY turn (Go `billingCheckedThisTurn`).
        let mut billing_checked = false;
        let mut terminal_seen = false;
        let mut timed_out = false;
        let mut stderr_open = true;
        let mut mailbox_open = messages.is_some();
        let mut stderr_buf = CappedBuffer::new(MAX_STDERR_CAPTURE);
        let mut acc: Vec<u8> = Vec::with_capacity(READ_CHUNK);
        let mut out_chunk = vec![0u8; READ_CHUNK];
        let mut err_chunk = vec![0u8; READ_CHUNK];

        let deadline = tokio::time::sleep_until(Instant::now() + self.cfg.turn_timeout);
        tokio::pin!(deadline);

        'outer: loop {
            tokio::select! {
                // Turn deadline: kill the whole process group so a hung child (and its children)
                // dies and the pipes EOF. A captured result still wins post-loop.
                _ = &mut deadline => {
                    kill_group(pid);
                    timed_out = true;
                    break 'outer;
                }
                // Stdout: accumulate bytes and classify each complete line.
                r = stdout.read(&mut out_chunk) => {
                    match r {
                        Ok(0) => break 'outer, // EOF
                        Ok(n) => {
                            acc.extend_from_slice(&out_chunk[..n]);
                            loop {
                                let Some(pos) = acc.iter().position(|&b| b == b'\n') else {
                                    // No terminator yet: an unterminated token past the cap is the
                                    // ErrTooLong analogue.
                                    if acc.len() > MAX_STDOUT_LINE {
                                        scan_err = Some("token too long".to_string());
                                        break 'outer;
                                    }
                                    break;
                                };
                                let mut line: Vec<u8> = acc.drain(..=pos).collect();
                                line.pop(); // strip '\n'
                                if line.len() > MAX_STDOUT_LINE {
                                    scan_err = Some("token too long".to_string());
                                    break 'outer;
                                }
                                // Tee the raw line, then classify.
                                self.tee_stdout(&line);
                                let c = classify(&line);
                                // Capture the session id for EVERY classified line, BEFORE the !ok
                                // gate, so a session id arriving on a non-surfaced line still seeds
                                // the thread id (else --resume / billing-guard association is lost).
                                if !c.session_id.is_empty() {
                                    let mut tid = self.locked_thread_id();
                                    if tid.is_empty() {
                                        *tid = c.session_id.clone();
                                    }
                                }
                                if !c.ok {
                                    continue;
                                }
                                // Billing guard on the first system/init of THIS turn: apiKeySource
                                // must be "none". Otherwise kill the group and abort.
                                if guard_on
                                    && c.event.event_type == EVENT_SESSION_STARTED
                                    && !billing_checked
                                {
                                    billing_checked = true;
                                    if !billing_guard_ok(&c.api_key_source) {
                                        kill_group(pid);
                                        billing_failed = true;
                                        break 'outer;
                                    }
                                }
                                let mut ev = c.event.clone();
                                ev.pid = pid as i64;
                                // Only the terminal result carries the AUTHORITATIVE per-turn total.
                                if c.terminal && let Some(u) = ev.usage {
                                    usage = u;
                                }
                                on_event(ev);
                                if c.terminal {
                                    // The terminal result is in: close stdin NOW so the process can
                                    // exit and nothing is ever written after the result (INF-250).
                                    terminal_seen = true;
                                    stdin = None;
                                    let mut tr = c.result.clone();
                                    tr.usage = usage;
                                    result = Some(tr);
                                }
                            }
                        }
                        Err(e) => {
                            scan_err = Some(e.to_string());
                            break 'outer;
                        }
                    }
                }
                // Stderr: drain into the capped buffer (+ transcript) so the child never blocks on a
                // full pipe.
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
                // Operator mailbox: fold a queued message into the live turn as a second user
                // message. Disabled the instant the terminal result lands (no write after result).
                m = recv_opt(&mut messages), if mailbox_open && !terminal_seen => {
                    match m {
                        Some(msg) => {
                            // Re-check terminal: a message dequeued in the same instant the result
                            // landed must NOT be written (it would risk a second result cycle). In
                            // that rare race the message is dropped — an honest "not delivered".
                            // Skip an unencodable message (Go `continue`).
                            if !terminal_seen && let Ok(line) = encode_user_message(&msg) {
                                let wrote = match stdin.as_mut() {
                                    Some(s) => s.write_all(&line).await.is_ok(),
                                    None => false,
                                };
                                if wrote {
                                    // Synthesized LOCALLY on the actual write (not parsed from
                                    // claude output): records the delivery.
                                    on_event(Event {
                                        event_type: EVENT_OPERATOR_MESSAGE.to_string(),
                                        timestamp: Some(Utc::now()),
                                        message: msg,
                                        turn: turn_n,
                                        pid: pid as i64,
                                        ..Default::default()
                                    });
                                } else {
                                    mailbox_open = false;
                                }
                            }
                        }
                        None => mailbox_open = false, // channel closed
                    }
                }
            }
        }

        // The scan loop has exited (terminal result, EOF, deadline, scan error, or billing abort):
        // close stdin, drain both pipes so a finishing child never blocks, then reap it. Every exit
        // path leaves the child already dead (EOF / deadline kill / billing kill) or finishing
        // finite output (scan error), so this completes promptly without extra deadline bounding.
        drop(stdin); // close stdin (idempotent — the terminal branch may already have dropped it)
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
        group_kill.disarm(); // reaped — nothing left to signal, and the pid may now be recycled

        // Billing abort (a system/init reported a non-"none" apiKeySource): refuse to bill.
        if billing_failed {
            return (failed(usage), Some(AgentError::BillingGuard));
        }
        // Fail-closed billing guard: a result but no system/init observed → apiKeySource could not
        // be verified, so we cannot prove the turn ran on the logged-in subscription. Refuse it
        // (this only fires when a result would otherwise be returned; the timeout/scan-error/wait
        // paths below already fail). The `billing_guard_failed` leading token is the `errors.Is`
        // analogue; the appended context mirrors Go's wrapped message.
        if result.is_some() && guard_on && !billing_checked {
            return (
                failed(usage),
                Some(AgentError::Other(
                    "billing_guard_failed: no system/init observed; cannot verify apiKeySource"
                        .to_string(),
                )),
            );
        }

        // A terminal result observed on the stream is authoritative — return it even if a deadline
        // kill landed immediately after, so a real result is never discarded as a spurious timeout.
        if let Some(tr) = result {
            if tr.status == TURN_SUCCEEDED {
                return (tr, None);
            }
            return (tr, Some(AgentError::TurnFailed));
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
        // No terminal result observed.
        let exit_bad = matches!(&wait_res, Ok(s) if !s.success()) || wait_res.is_err();
        if exit_bad {
            let mut msg = truncate_stderr(stderr_buf.bytes());
            if stderr_buf.truncated {
                msg.push_str(" (stderr capped)");
            }
            let detail = match &wait_res {
                Ok(s) => format!("{s}"),
                Err(e) => format!("{e}"),
            };
            // A profile-supplied model is the first thing to suspect on a bare non-zero exit with no
            // result event: the CLI refuses an unknown `--model` exactly here, and its stderr names
            // neither the teammate nor the fact that the value came from a profile rather than from
            // `claude.model` (STUDIO-868, decision 1). The run still FAILS — this only makes the
            // failure attributable. Empty for every run with no override, which keeps the message
            // byte-identical to a daemon built before this existed.
            let whose = self.model_attribution();
            return (
                failed(usage),
                Some(AgentError::Other(format!(
                    "turn_failed: {detail}: {whose}{msg}"
                ))),
            );
        }
        (
            failed(usage),
            Some(AgentError::Other(
                "turn_failed: stream ended without a result event".to_string(),
            )),
        )
    }
}

/// Builds a `TURN_FAILED` result carrying the given usage.
fn failed(usage: Usage) -> TurnResult {
    TurnResult {
        status: TURN_FAILED.to_string(),
        usage,
        result_text: String::new(),
    }
}

/// Builds a `TURN_TIMED_OUT` result carrying the given usage.
fn timed_out_result(usage: Usage) -> TurnResult {
    TurnResult {
        status: TURN_TIMED_OUT.to_string(),
        usage,
        result_text: String::new(),
    }
}

/// SIGKILLs the child's whole process group when a turn is abandoned by having its future DROPPED,
/// unless [`disarm`](KillGroupOnDrop::disarm)ed first. Go gets this from `exec.CommandContext(tctx,
/// …)` + `cmd.Cancel`: cancelling the run's context kills the agent's group. Rust's cancellation IS
/// the drop, and a dropped `tokio::process::Child` signals nothing, so before STUDIO-840 an operator
/// Stop removed the running entry, moved the ticket and answered 200 while the real `claude` kept
/// committing. Disarmed once the child has been reaped, so a kill can never reach a recycled pid.
struct KillGroupOnDrop(u32);

impl KillGroupOnDrop {
    /// Stands the kill down (the child is reaped).
    fn disarm(&mut self) {
        self.0 = 0; // `kill_group` skips pid 0
    }
}

impl Drop for KillGroupOnDrop {
    fn drop(&mut self) {
        kill_group(self.0);
    }
}

/// Signals the whole process group led by `pid` with `SIGKILL` (Go `syscall.Kill(-pid, SIGKILL)`).
/// A pid of 0 is skipped — `kill(0, …)` would target the daemon's OWN group.
fn kill_group(pid: u32) {
    if pid == 0 {
        return;
    }
    // SAFETY: `kill(2)` with a negative pid signals the process group led by `pid`. SIGKILL cannot
    // be caught, and the return is best-effort (mirrors Go's `_ = syscall.Kill(...)`).
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
}

/// Awaits the next operator message, or blocks forever when there is no mailbox (a `None` receiver
/// — the parity mirror of Go's nil channel, which blocks forever in `select`).
async fn recv_opt(messages: &mut Option<&mut mpsc::Receiver<String>>) -> Option<String> {
    match messages {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// Frames one stream-json user-message INPUT line (newline-terminated). String content is the
/// proven-working shape (INF-250): a held-open stdin folds a second user message into the ongoing
/// turn at the next step boundary.
fn encode_user_message(text: &str) -> Result<Vec<u8>, serde_json::Error> {
    #[derive(serde::Serialize)]
    struct UserMsg<'a> {
        #[serde(rename = "type")]
        kind: &'a str,
        message: Inner<'a>,
    }
    #[derive(serde::Serialize)]
    struct Inner<'a> {
        role: &'a str,
        content: &'a str,
    }
    let mut line = serde_json::to_vec(&UserMsg {
        kind: "user",
        message: Inner {
            role: "user",
            content: text,
        },
    })?;
    line.push(b'\n');
    Ok(line)
}

/// Renders the captured stderr for a failure error, bounded so a noisy agent can't inflate it (Go
/// `truncateStderr`).
fn truncate_stderr(b: &[u8]) -> String {
    if b.len() <= MAX_STDERR_MESSAGE {
        return String::from_utf8_lossy(b).into_owned();
    }
    let mut s = String::from_utf8_lossy(&b[..MAX_STDERR_MESSAGE]).into_owned();
    s.push_str("...(truncated)");
    s
}

/// Accumulates up to a fixed cap of bytes, then silently drops further writes and records that
/// truncation occurred (Go `cappedBuffer`). Every write reports the full length consumed and never
/// errors, so the stderr drain never blocks.
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
        let room = self.cap.saturating_sub(self.buf.len());
        if room > 0 {
            let take = if p.len() > room {
                self.truncated = true;
                &p[..room]
            } else {
                p
            };
            self.buf.extend_from_slice(take);
        } else if !p.is_empty() {
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
    use crate::{EVENT_TURN_COMPLETED, Runner as _}; // trait method + the completed-event constant
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, AtomicUsize};
    use std::time::Instant as StdInstant;

    use rhapsody_core::Issue;

    // Serializes the two env-scrub tests' `set_var`/`remove_var` (edition-2024 `unsafe`, and racy vs
    // another test's `std::env::vars_os()` because Rust's std::env is not internally synchronized).
    // A write lock excludes all readers; every run_turn-invoking test takes a read lock while its
    // child env is built. Go's os package is mutex-guarded and needs no such lock; this restores that
    // invariant for the port. A tokio RwLock (not std) is used so the guard may be held across the
    // run_turn await without tripping `clippy::await_holding_lock`.
    static ENV_GUARD: tokio::sync::RwLock<()> = tokio::sync::RwLock::const_new(());

    /// RAII temp dir, unique per pid+counter, auto-removed (the port of Go's `t.TempDir()`).
    struct TempDir {
        dir: std::path::PathBuf,
    }

    impl TempDir {
        fn new() -> TempDir {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let seq = SEQ.fetch_add(1, Ordering::Relaxed);
            let dir =
                std::env::temp_dir().join(format!("rhapsody-runner-{}-{seq}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("create temp dir");
            TempDir { dir }
        }
        fn path(&self) -> String {
            self.dir.to_string_lossy().into_owned()
        }
        fn join(&self, name: &str) -> String {
            self.dir.join(name).to_string_lossy().into_owned()
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// Writes a bash fake-claude script and returns its owning temp dir + path. Invoked as
    /// `bash <script>` (Go `writeFakeClaude`), so the script needs no executable bit.
    fn write_fake_claude(body: &str) -> (TempDir, String) {
        let dir = TempDir::new();
        let p = dir.join("fakeclaude.sh");
        std::fs::write(&p, body).expect("write fake script");
        (dir, p)
    }

    /// Builds a runner over `bash <script>` with a 5s turn timeout (Go `newRunner`).
    fn new_runner(script: &str, root: &str) -> Runner {
        Runner::new(Config {
            command: format!("bash {script}"),
            workspace_root: root.to_string(),
            turn_timeout: Duration::from_secs(5),
            ..Default::default()
        })
    }

    /// Creates an in-root workspace dir and returns its path (Go's `os.MkdirAll(root/id)`).
    fn make_ws(root: &TempDir, id: &str) -> String {
        let ws = root.join(id);
        std::fs::create_dir_all(&ws).expect("mkdir ws");
        ws
    }

    fn issue(id: &str, identifier: &str) -> Issue {
        Issue {
            id: id.to_string(),
            identifier: identifier.to_string(),
            ..Default::default()
        }
    }

    /// A `std::io::Write` sink backed by a shared buffer, so a test can read what the runner wrote to
    /// a transcript after the (owned) sink has been moved into the session.
    #[derive(Clone)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);

    impl SharedBuf {
        fn new() -> SharedBuf {
            SharedBuf(Arc::new(Mutex::new(Vec::new())))
        }
        fn string(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
        }
    }

    impl std::io::Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A transcript sink that always fails, counting attempts (Go `failingWriter`).
    #[derive(Clone)]
    struct FailingWriter(Arc<AtomicUsize>);

    impl std::io::Write for FailingWriter {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(std::io::Error::other("disk full"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Collects `on_event` event types into a shared vec, returning (collector, closure).
    fn type_collector() -> (Arc<Mutex<Vec<String>>>, impl Fn(Event) + Send + Sync) {
        let types = Arc::new(Mutex::new(Vec::new()));
        let sink = types.clone();
        (types, move |e: Event| {
            sink.lock().unwrap().push(e.event_type);
        })
    }

    /// Non-empty lines a fake captured from stdin (Go `readCaptureLines`).
    fn read_capture_lines(path: &str) -> Vec<String> {
        match std::fs::read_to_string(path) {
            Ok(s) => s
                .split('\n')
                .filter(|l| !l.trim().is_empty())
                .map(str::to_string)
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Parses a captured stream-json INPUT line and returns its message content (Go `decodeUserMsg`).
    fn decode_user_msg(line: &str) -> String {
        let v: serde_json::Value = serde_json::from_str(line).expect("captured stdin line is JSON");
        assert_eq!(v["type"], "user", "line not a user message: {line}");
        assert_eq!(v["message"]["role"], "user", "line role not user: {line}");
        v["message"]["content"].as_str().unwrap().to_string()
    }

    // Mirrors Go `claude.TestRunTurnSuccessStreamsEventsAndUsage`.
    #[tokio::test]
    async fn run_turn_success_streams_events_and_usage() {
        let _env = ENV_GUARD.read().await;
        let root = TempDir::new();
        let ws = make_ws(&root, "MT-1");
        let (_s, script) = write_fake_claude(
            "#!/usr/bin/env bash\n\
             head -n 1 >/dev/null\n\
             echo '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"sess-abc\",\"apiKeySource\":\"none\"}'\n\
             echo '{\"type\":\"assistant\",\"session_id\":\"sess-abc\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"working\"}]}}'\n\
             echo '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"session_id\":\"sess-abc\",\"usage\":{\"input_tokens\":100,\"output_tokens\":40}}'\n",
        );
        let r = new_runner(&script, &root.path());
        let sess = r
            .start_session(&ws, issue("1", "MT-1"), None)
            .await
            .expect("start session");
        let (types, on_event) = type_collector();
        let (res, err) = sess.run_turn("do the work", None, None, &on_event).await;
        assert!(err.is_none(), "RunTurn err = {err:?}");
        assert_eq!(res.status, TURN_SUCCEEDED, "status");
        assert_eq!(res.usage.total_tokens, 140, "usage total");
        let types = types.lock().unwrap();
        assert!(types.len() >= 3, "events = {types:?}");
        assert_eq!(types[0], EVENT_SESSION_STARTED);
        assert_eq!(types[types.len() - 1], EVENT_TURN_COMPLETED);
        assert_eq!(sess.thread_id(), "sess-abc");
        assert_eq!(sess.id(), "sess-abc-1");
    }

    // Mirrors Go `claude.TestRunTurnTeesRawTranscript`.
    #[tokio::test]
    async fn run_turn_tees_raw_transcript() {
        let _env = ENV_GUARD.read().await;
        let root = TempDir::new();
        let ws = make_ws(&root, "MT-T");
        let (_s, script) = write_fake_claude(
            "#!/usr/bin/env bash\n\
             head -n 1 >/dev/null\n\
             echo '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s\",\"apiKeySource\":\"none\"}'\n\
             echo '{\"type\":\"user\",\"note\":\"some-unclassified-line\"}'\n\
             echo '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"session_id\":\"s\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}'\n\
             echo 'a stderr diagnostic' 1>&2\n",
        );
        let r = new_runner(&script, &root.path());
        let raw = SharedBuf::new();
        let err_buf = SharedBuf::new();
        let transcript = Transcript {
            stdout: Some(Box::new(raw.clone())),
            stderr: Some(Box::new(err_buf.clone())),
        };
        let sess = r
            .start_session(&ws, issue("t", "MT-T"), Some(transcript))
            .await
            .expect("start session");
        let (_types, on_event) = type_collector();
        let (_res, err) = sess.run_turn("p", None, None, &on_event).await;
        assert!(err.is_none(), "err = {err:?}");
        let raw = raw.string();
        assert!(
            raw.contains(r#""subtype":"init""#)
                && raw.contains("some-unclassified-line")
                && raw.contains(r#""type":"result""#),
            "raw transcript missing lines:\n{raw}"
        );
        assert!(
            err_buf.string().contains("stderr diagnostic"),
            "stderr not captured: {:?}",
            err_buf.string()
        );
    }

    // Mirrors Go `claude.TestRunTurnTranscriptStderrFailureStillCapturesRealStderr`.
    #[tokio::test]
    async fn run_turn_transcript_stderr_failure_still_captures_real_stderr() {
        let _env = ENV_GUARD.read().await;
        let root = TempDir::new();
        let ws = make_ws(&root, "MT-SE");
        let (_s, script) = write_fake_claude(
            "#!/usr/bin/env bash\n\
             head -n 1 >/dev/null\n\
             echo '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s\",\"apiKeySource\":\"none\"}'\n\
             echo 'real-stderr-diagnostic' 1>&2\n\
             exit 3\n",
        );
        let r = new_runner(&script, &root.path());
        let writes = Arc::new(AtomicUsize::new(0));
        let fail_sink = FailingWriter(writes.clone());
        let transcript = Transcript {
            stdout: None,
            stderr: Some(Box::new(fail_sink)),
        };
        let sess = r
            .start_session(&ws, issue("se", "MT-SE"), Some(transcript))
            .await
            .expect("start session");
        let (_types, on_event) = type_collector();
        let (res, err) = sess.run_turn("p", None, None, &on_event).await;
        let err = err.expect("expected error for non-zero exit without a result");
        assert_eq!(res.status, TURN_FAILED, "status");
        assert!(
            err.to_string().contains("real-stderr-diagnostic"),
            "real stderr not captured despite failing transcript sink: {err}"
        );
        assert!(
            writes.load(Ordering::SeqCst) > 0,
            "expected the failing transcript stderr sink to have been written to"
        );
    }

    // Mirrors Go `claude.TestRunTurnContinuationSendsResume`.
    #[tokio::test]
    async fn run_turn_continuation_sends_resume() {
        let _env = ENV_GUARD.read().await;
        let root = TempDir::new();
        let ws = make_ws(&root, "MT-2");
        let (_s, script) = write_fake_claude(
            "#!/usr/bin/env bash\n\
             echo \"$@\" >> args.log\n\
             head -n 1 >/dev/null\n\
             echo '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"sess-xyz\",\"apiKeySource\":\"none\"}'\n\
             echo '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"session_id\":\"sess-xyz\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}'\n",
        );
        let r = new_runner(&script, &root.path());
        let sess = r
            .start_session(&ws, issue("2", "MT-2"), None)
            .await
            .expect("start session");
        let (_t, on_event) = type_collector();
        let (_r1, e1) = sess.run_turn("first", None, None, &on_event).await;
        assert!(e1.is_none(), "turn 1: {e1:?}");
        let (_r2, e2) = sess.run_turn("continue", Some(2), None, &on_event).await;
        assert!(e2.is_none(), "turn 2: {e2:?}");
        let log = std::fs::read_to_string(format!("{ws}/args.log")).expect("read args.log");
        let lines: Vec<&str> = log.trim().split('\n').collect();
        assert_eq!(lines.len(), 2, "expected 2 invocations: {log:?}");
        assert!(
            !lines[0].contains("--resume"),
            "first turn must not resume: {:?}",
            lines[0]
        );
        assert!(
            lines[1].contains("--resume sess-xyz"),
            "continuation turn must resume sess-xyz: {:?}",
            lines[1]
        );
    }

    // Mirrors Go `claude.TestRunTurnFailureResultReturnsError`.
    #[tokio::test]
    async fn run_turn_failure_result_returns_error() {
        let _env = ENV_GUARD.read().await;
        let root = TempDir::new();
        let ws = make_ws(&root, "MT-3");
        let (_s, script) = write_fake_claude(
            "#!/usr/bin/env bash\n\
             head -n 1 >/dev/null\n\
             echo '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s\",\"apiKeySource\":\"none\"}'\n\
             echo '{\"type\":\"result\",\"subtype\":\"error_during_execution\",\"is_error\":true,\"session_id\":\"s\",\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}'\n",
        );
        let r = new_runner(&script, &root.path());
        let sess = r
            .start_session(&ws, issue("3", "MT-3"), None)
            .await
            .expect("start session");
        let (_t, on_event) = type_collector();
        let (res, err) = sess.run_turn("p", None, None, &on_event).await;
        assert!(err.is_some(), "expected error for failed turn");
        assert_eq!(res.status, TURN_FAILED, "status");
    }

    // Mirrors Go `claude.TestRunTurnTimeout`.
    #[tokio::test]
    async fn run_turn_timeout() {
        let _env = ENV_GUARD.read().await;
        let root = TempDir::new();
        let ws = make_ws(&root, "MT-4");
        let (_s, script) = write_fake_claude(
            "#!/usr/bin/env bash\n\
             head -n 1 >/dev/null\n\
             echo '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s\",\"apiKeySource\":\"none\"}'\n\
             sleep 30\n",
        );
        let r = Runner::new(Config {
            command: format!("bash {script}"),
            workspace_root: root.path(),
            turn_timeout: Duration::from_millis(200),
            ..Default::default()
        });
        let sess = r
            .start_session(&ws, issue("4", "MT-4"), None)
            .await
            .expect("start session");
        let (_t, on_event) = type_collector();
        let start = StdInstant::now();
        let (res, err) = sess.run_turn("p", None, None, &on_event).await;
        assert!(
            matches!(err, Some(AgentError::TurnTimeout)),
            "got {err:?}, want TurnTimeout"
        );
        assert_eq!(res.status, TURN_TIMED_OUT, "status");
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "timeout too slow: {:?}",
            start.elapsed()
        );
    }

    // Mirrors Go `claude.TestRunTurnResultBeforeDeadlineReturnsResultNotTimeout`.
    #[tokio::test]
    async fn run_turn_result_before_deadline_returns_result_not_timeout() {
        let _env = ENV_GUARD.read().await;
        let root = TempDir::new();
        let ws = make_ws(&root, "MT-8");
        let (_s, script) = write_fake_claude(
            "#!/usr/bin/env bash\n\
             head -n 1 >/dev/null\n\
             echo '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s\",\"apiKeySource\":\"none\"}'\n\
             echo '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"session_id\":\"s\",\"usage\":{\"input_tokens\":2,\"output_tokens\":3}}'\n\
             sleep 30\n",
        );
        let r = Runner::new(Config {
            command: format!("bash {script}"),
            workspace_root: root.path(),
            turn_timeout: Duration::from_millis(300),
            ..Default::default()
        });
        let sess = r
            .start_session(&ws, issue("8", "MT-8"), None)
            .await
            .expect("start session");
        let (_t, on_event) = type_collector();
        let (res, err) = sess.run_turn("p", None, None, &on_event).await;
        assert!(
            err.is_none(),
            "RunTurn err = {err:?}, want none (terminal result captured before deadline)"
        );
        assert_eq!(
            res.status, TURN_SUCCEEDED,
            "must not be discarded as TurnTimedOut"
        );
        assert_eq!(res.usage.total_tokens, 5, "usage total");
    }

    // Mirrors Go `claude.TestStartSessionRejectsWorkspaceOutsideRoot`.
    #[tokio::test]
    async fn start_session_rejects_workspace_outside_root() {
        let _env = ENV_GUARD.read().await;
        let root = TempDir::new();
        let outside = format!(
            "{}/evil/MT-9",
            std::path::Path::new(&root.path())
                .parent()
                .unwrap()
                .display()
        );
        let r = Runner::new(Config {
            command: "claude".to_string(),
            workspace_root: root.path(),
            turn_timeout: Duration::from_secs(1),
            ..Default::default()
        });
        let sess = r
            .start_session(&outside, issue("9", "MT-9"), None)
            .await
            .expect("start session");
        let (_t, on_event) = type_collector();
        let (_res, err) = sess.run_turn("p", None, None, &on_event).await;
        assert!(
            err.is_some(),
            "RunTurn must reject a workspace outside the root before launching"
        );
    }

    // Mirrors Go `claude.TestStartSessionInvalidCommand`.
    #[tokio::test]
    async fn start_session_invalid_command() {
        let r = Runner::new(Config {
            command: "   ".to_string(),
            workspace_root: TempDir::new().path(),
            turn_timeout: Duration::from_secs(1),
            ..Default::default()
        });
        let got = r
            .start_session(&TempDir::new().path(), Issue::default(), None)
            .await;
        assert!(
            matches!(got.err(), Some(AgentError::InvalidCommand)),
            "want InvalidCommand"
        );
    }

    // Mirrors Go `claude.TestRunTurnScannerErrorOnOversizedLine`.
    #[tokio::test]
    async fn run_turn_scanner_error_on_oversized_line() {
        let _env = ENV_GUARD.read().await;
        let root = TempDir::new();
        let ws = make_ws(&root, "MT-5");
        let (_s, script) = write_fake_claude(
            "#!/usr/bin/env bash\n\
             head -n 1 >/dev/null\n\
             echo '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s\",\"apiKeySource\":\"none\"}'\n\
             head -c 11000000 /dev/zero | tr '\\0' 'a'\n\
             echo\n",
        );
        let r = new_runner(&script, &root.path());
        let sess = r
            .start_session(&ws, issue("5", "MT-5"), None)
            .await
            .expect("start session");
        let (_t, on_event) = type_collector();
        let (res, err) = sess.run_turn("p", None, None, &on_event).await;
        let err = err.expect("expected error for oversized stream line");
        assert!(
            err.to_string().contains("stream read error"),
            "error = {err:?}, want it to contain 'stream read error'"
        );
        assert_eq!(res.status, TURN_FAILED, "status");
    }

    // Mirrors Go `claude.TestRunTurnInitialPromptEncoding` (INF-250).
    #[tokio::test]
    async fn run_turn_initial_prompt_encoding() {
        let _env = ENV_GUARD.read().await;
        let root = TempDir::new();
        let ws = make_ws(&root, "MT-OM1");
        let cap_dir = TempDir::new();
        let cap = cap_dir.join("stdin_capture");
        let (_s, script) = write_fake_claude(&format!(
            "#!/usr/bin/env bash\n\
             IFS= read -r l1; printf '%s\\n' \"$l1\" >> {cap}\n\
             echo '{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s\",\"apiKeySource\":\"none\"}}'\n\
             echo '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"session_id\":\"s\",\"usage\":{{\"input_tokens\":1,\"output_tokens\":1}}}}'\n",
        ));
        let r = new_runner(&script, &root.path());
        let sess = r
            .start_session(&ws, issue("om1", "MT-OM1"), None)
            .await
            .expect("start session");
        let (_t, on_event) = type_collector();
        let (res, err) = sess.run_turn("do the work", None, None, &on_event).await;
        assert!(err.is_none(), "RunTurn err = {err:?}");
        assert_eq!(res.status, TURN_SUCCEEDED, "status");
        let lines = read_capture_lines(&cap);
        assert_eq!(
            lines.len(),
            1,
            "captured {} stdin lines, want 1: {lines:?}",
            lines.len()
        );
        assert_eq!(
            decode_user_msg(&lines[0]),
            "do the work",
            "initial prompt content"
        );
    }

    // Mirrors Go `claude.TestRunTurnMidTurnInjection` (INF-250).
    #[tokio::test]
    async fn run_turn_mid_turn_injection() {
        let _env = ENV_GUARD.read().await;
        let root = TempDir::new();
        let ws = make_ws(&root, "MT-OM2");
        let cap_dir = TempDir::new();
        let cap = cap_dir.join("stdin_capture");
        let (_s, script) = write_fake_claude(&format!(
            "#!/usr/bin/env bash\n\
             IFS= read -r l1; printf '%s\\n' \"$l1\" >> {cap}\n\
             echo '{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s\",\"apiKeySource\":\"none\"}}'\n\
             echo '{{\"type\":\"assistant\",\"session_id\":\"s\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"working\"}}]}}}}'\n\
             IFS= read -r l2; printf '%s\\n' \"$l2\" >> {cap}\n\
             sleep 0.3\n\
             echo '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"session_id\":\"s\",\"usage\":{{\"input_tokens\":1,\"output_tokens\":1}}}}'\n",
        ));
        let r = new_runner(&script, &root.path());
        let sess = r
            .start_session(&ws, issue("om2", "MT-OM2"), None)
            .await
            .expect("start session");
        let (tx, mut rx) = mpsc::channel::<String>(4);
        tx.try_send("watch the branch".to_string())
            .expect("queue message");
        let op_events = Arc::new(Mutex::new(Vec::<Event>::new()));
        let sink = op_events.clone();
        let on_event = move |e: Event| {
            if e.event_type == EVENT_OPERATOR_MESSAGE {
                sink.lock().unwrap().push(e);
            }
        };
        let (res, err) = sess
            .run_turn("do the work", None, Some(&mut rx), &on_event)
            .await;
        assert!(err.is_none(), "RunTurn err = {err:?}");
        assert_eq!(res.status, TURN_SUCCEEDED, "status");
        let lines = read_capture_lines(&cap);
        assert_eq!(
            lines.len(),
            2,
            "captured {} stdin lines, want 2: {lines:?}",
            lines.len()
        );
        assert_eq!(decode_user_msg(&lines[0]), "do the work", "line0 = prompt");
        assert_eq!(
            decode_user_msg(&lines[1]),
            "watch the branch",
            "injected content"
        );
        let op = op_events.lock().unwrap();
        assert_eq!(op.len(), 1, "EventOperatorMessage count");
        assert_eq!(op[0].message, "watch the branch", "event message");
        assert_eq!(op[0].turn, 1, "event turn");
    }

    // Mirrors Go `claude.TestRunTurnNoWriteAfterResult` (INF-250).
    #[tokio::test]
    async fn run_turn_no_write_after_result() {
        let _env = ENV_GUARD.read().await;
        let root = TempDir::new();
        let ws = make_ws(&root, "MT-OM3");
        let cap_dir = TempDir::new();
        let cap = cap_dir.join("stdin_capture");
        let (_s, script) = write_fake_claude(&format!(
            "#!/usr/bin/env bash\n\
             IFS= read -r l1; printf '%s\\n' \"$l1\" >> {cap}\n\
             echo '{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s\",\"apiKeySource\":\"none\"}}'\n\
             echo '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"session_id\":\"s\",\"usage\":{{\"input_tokens\":1,\"output_tokens\":1}}}}'\n\
             if IFS= read -r l2; then printf 'POST:%s\\n' \"$l2\" >> {cap}; fi\n",
        ));
        let r = new_runner(&script, &root.path());
        let sess = r
            .start_session(&ws, issue("om3", "MT-OM3"), None)
            .await
            .expect("start session");
        let (tx, mut rx) = mpsc::channel::<String>(4);
        let (_t, on_event) = type_collector();
        let (res, err) = tokio::time::timeout(
            Duration::from_secs(5),
            sess.run_turn("do the work", None, Some(&mut rx), &on_event),
        )
        .await
        .expect("RunTurn hung after terminal result");
        assert!(err.is_none(), "RunTurn err = {err:?}");
        assert_eq!(res.status, TURN_SUCCEEDED, "status");
        // The run has ended; a late send must not be delivered.
        let _ = tx.try_send("too late".to_string());
        tokio::time::sleep(Duration::from_millis(200)).await;
        let lines = read_capture_lines(&cap);
        assert_eq!(
            lines.len(),
            1,
            "captured {} stdin lines, want 1 (prompt only): {lines:?}",
            lines.len()
        );
        assert_eq!(decode_user_msg(&lines[0]), "do the work", "line0 = prompt");
    }

    // Mirrors Go `claude.TestRunTurnStdinNotDrained`.
    #[tokio::test]
    async fn run_turn_stdin_not_drained() {
        let _env = ENV_GUARD.read().await;
        let root = TempDir::new();
        let ws = make_ws(&root, "MT-6");
        let (_s, script) = write_fake_claude(
            "#!/usr/bin/env bash\n\
             echo '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s\",\"apiKeySource\":\"none\"}'\n\
             echo '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"session_id\":\"s\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}'\n",
        );
        let r = new_runner(&script, &root.path());
        let sess = r
            .start_session(&ws, issue("6", "MT-6"), None)
            .await
            .expect("start session");
        let (_t, on_event) = type_collector();
        let start = StdInstant::now();
        let (res, err) = sess.run_turn("p", None, None, &on_event).await;
        assert!(err.is_none(), "RunTurn err = {err:?}");
        assert_eq!(res.status, TURN_SUCCEEDED, "status");
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "turn too slow: {:?}",
            start.elapsed()
        );
    }

    // Mirrors Go `claude.TestRunTurnBillingGuardFailsOnNonNoneAPIKeySource`.
    #[tokio::test]
    async fn run_turn_billing_guard_fails_on_non_none_api_key_source() {
        let _env = ENV_GUARD.read().await;
        let root = TempDir::new();
        let ws = make_ws(&root, "MT-BG");
        let (_s, script) = write_fake_claude(
            "#!/usr/bin/env bash\n\
             head -n 1 >/dev/null\n\
             echo '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s\",\"apiKeySource\":\"ANTHROPIC_API_KEY\"}'\n\
             echo '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"session_id\":\"s\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}'\n",
        );
        let r = Runner::new(Config {
            command: format!("bash {script}"),
            workspace_root: root.path(),
            turn_timeout: Duration::from_secs(5),
            billing_guard: Some(true),
            ..Default::default()
        });
        let sess = r
            .start_session(&ws, issue("bg", "MT-BG"), None)
            .await
            .expect("start session");
        let (_t, on_event) = type_collector();
        let (res, err) = sess.run_turn("p", None, None, &on_event).await;
        assert!(
            matches!(err, Some(AgentError::BillingGuard)),
            "got {err:?}, want BillingGuard"
        );
        assert_eq!(res.status, TURN_FAILED, "status");
    }

    // Mirrors Go `claude.TestRunTurnBillingGuardPassesOnNone`.
    #[tokio::test]
    async fn run_turn_billing_guard_passes_on_none() {
        let _env = ENV_GUARD.read().await;
        let root = TempDir::new();
        let ws = make_ws(&root, "MT-BG2");
        let (_s, script) = write_fake_claude(
            "#!/usr/bin/env bash\n\
             head -n 1 >/dev/null\n\
             echo '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s\",\"apiKeySource\":\"none\"}'\n\
             echo '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"session_id\":\"s\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}'\n",
        );
        let r = Runner::new(Config {
            command: format!("bash {script}"),
            workspace_root: root.path(),
            turn_timeout: Duration::from_secs(5),
            billing_guard: Some(true),
            ..Default::default()
        });
        let sess = r
            .start_session(&ws, issue("bg2", "MT-BG2"), None)
            .await
            .expect("start session");
        let (_t, on_event) = type_collector();
        let (res, err) = sess.run_turn("p", None, None, &on_event).await;
        assert!(err.is_none(), "RunTurn err = {err:?}");
        assert_eq!(res.status, TURN_SUCCEEDED, "status");
    }

    // Mirrors Go `claude.TestRunTurnBillingGuardDisabledSkipsAssertion`.
    #[tokio::test]
    async fn run_turn_billing_guard_disabled_skips_assertion() {
        let _env = ENV_GUARD.read().await;
        let root = TempDir::new();
        let ws = make_ws(&root, "MT-BG3");
        let (_s, script) = write_fake_claude(
            "#!/usr/bin/env bash\n\
             head -n 1 >/dev/null\n\
             echo '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s\",\"apiKeySource\":\"user\"}'\n\
             echo '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"session_id\":\"s\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}'\n",
        );
        let r = Runner::new(Config {
            command: format!("bash {script}"),
            workspace_root: root.path(),
            turn_timeout: Duration::from_secs(5),
            billing_guard: Some(false),
            ..Default::default()
        });
        let sess = r
            .start_session(&ws, issue("bg3", "MT-BG3"), None)
            .await
            .expect("start session");
        let (_t, on_event) = type_collector();
        let (res, err) = sess.run_turn("p", None, None, &on_event).await;
        assert!(
            err.is_none(),
            "RunTurn err = {err:?} (guard disabled should not assert)"
        );
        assert_eq!(res.status, TURN_SUCCEEDED, "status");
    }

    // env-dump fake: writes the child's environment to env.dump in cwd, then a passing turn.
    fn env_dump_script() -> (TempDir, String) {
        write_fake_claude(
            "#!/usr/bin/env bash\n\
             env > env.dump\n\
             head -n 1 >/dev/null\n\
             echo '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s\",\"apiKeySource\":\"none\"}'\n\
             echo '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"session_id\":\"s\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}'\n",
        )
    }

    fn read_env_dump(path: &str) -> std::collections::HashMap<String, String> {
        let mut m = std::collections::HashMap::new();
        if let Ok(s) = std::fs::read_to_string(path) {
            for line in s.split('\n') {
                if let Some((k, v)) = line.split_once('=') {
                    m.insert(k.to_string(), v.to_string());
                }
            }
        }
        m
    }

    // Mirrors Go `claude.TestRunTurnGuardOnScrubsBillingAndTrackerEnv`.
    #[tokio::test]
    async fn run_turn_guard_on_scrubs_billing_and_tracker_env() {
        let _env = ENV_GUARD.write().await; // exclusive: mutates the process environment
        let tracker_secret = "lin_api_value_secret";
        // SAFETY: the write lock excludes every reader, so no other test observes the environment
        // mid-mutation; the vars are removed before the lock is released.
        unsafe {
            std::env::set_var("ANTHROPIC_API_KEY", "sk-should-be-scrubbed");
            std::env::set_var("CLAUDE_CODE_USE_BEDROCK", "1");
            std::env::set_var("LINEAR_API_KEY", tracker_secret);
            std::env::set_var("MY_CUSTOM_TRACKER", tracker_secret);
            std::env::set_var("KEEP_ME", "ok");
        }
        let root = TempDir::new();
        let ws = make_ws(&root, "MT-ENV1");
        let (_s, script) = env_dump_script();
        let r = Runner::new(Config {
            command: format!("bash {script}"),
            workspace_root: root.path(),
            turn_timeout: Duration::from_secs(5),
            billing_guard: Some(true),
            tracker_api_key: tracker_secret.to_string(),
            ..Default::default()
        });
        let sess = r
            .start_session(&ws, issue("e1", "MT-ENV1"), None)
            .await
            .expect("start session");
        let (_t, on_event) = type_collector();
        let (_res, err) = sess.run_turn("p", None, None, &on_event).await;
        assert!(err.is_none(), "RunTurn err = {err:?}");
        let env = read_env_dump(&format!("{ws}/env.dump"));
        for name in scrubbed_env_vars() {
            assert!(
                !env.contains_key(name),
                "guard on: scrubbed var {name} leaked"
            );
        }
        assert!(
            !env.contains_key("MY_CUSTOM_TRACKER"),
            "tracker secret under custom name leaked"
        );
        assert_eq!(
            env.get("KEEP_ME").map(String::as_str),
            Some("ok"),
            "KEEP_ME"
        );
        assert!(
            env.get("PATH").is_some_and(|p| !p.is_empty()),
            "PATH must be inherited"
        );
        unsafe {
            std::env::remove_var("ANTHROPIC_API_KEY");
            std::env::remove_var("CLAUDE_CODE_USE_BEDROCK");
            std::env::remove_var("LINEAR_API_KEY");
            std::env::remove_var("MY_CUSTOM_TRACKER");
            std::env::remove_var("KEEP_ME");
        }
    }

    // Mirrors Go `claude.TestRunTurnGuardOffStillScrubsTrackerEnv`.
    #[tokio::test]
    async fn run_turn_guard_off_still_scrubs_tracker_env() {
        let _env = ENV_GUARD.write().await; // exclusive: mutates the process environment
        let tracker_secret = "lin_api_value_secret2";
        // SAFETY: see the sibling guard-on test — the write lock makes the mutation exclusive.
        unsafe {
            std::env::set_var("ANTHROPIC_API_KEY", "sk-billing-allowed-when-off");
            std::env::set_var("CLAUDE_CODE_USE_BEDROCK", "1");
            std::env::set_var("LINEAR_API_KEY", tracker_secret);
            std::env::set_var("MY_CUSTOM_TRACKER", tracker_secret);
        }
        let root = TempDir::new();
        let ws = make_ws(&root, "MT-ENV2");
        let (_s, script) = env_dump_script();
        let r = Runner::new(Config {
            command: format!("bash {script}"),
            workspace_root: root.path(),
            turn_timeout: Duration::from_secs(5),
            billing_guard: Some(false),
            tracker_api_key: tracker_secret.to_string(),
            ..Default::default()
        });
        let sess = r
            .start_session(&ws, issue("e2", "MT-ENV2"), None)
            .await
            .expect("start session");
        let (_t, on_event) = type_collector();
        let (_res, err) = sess.run_turn("p", None, None, &on_event).await;
        assert!(err.is_none(), "RunTurn err = {err:?}");
        let env = read_env_dump(&format!("{ws}/env.dump"));
        assert_eq!(
            env.get("ANTHROPIC_API_KEY").map(String::as_str),
            Some("sk-billing-allowed-when-off"),
            "guard off: ANTHROPIC_API_KEY should be inherited"
        );
        assert_eq!(
            env.get("CLAUDE_CODE_USE_BEDROCK").map(String::as_str),
            Some("1"),
            "guard off: CLAUDE_CODE_USE_BEDROCK should be inherited"
        );
        assert!(
            !env.contains_key("LINEAR_API_KEY"),
            "guard off: LINEAR_API_KEY must still be scrubbed"
        );
        assert!(
            !env.contains_key("MY_CUSTOM_TRACKER"),
            "guard off: tracker secret under custom name must still be scrubbed"
        );
        unsafe {
            std::env::remove_var("ANTHROPIC_API_KEY");
            std::env::remove_var("CLAUDE_CODE_USE_BEDROCK");
            std::env::remove_var("LINEAR_API_KEY");
            std::env::remove_var("MY_CUSTOM_TRACKER");
        }
    }

    // Mirrors Go `claude.TestRunTurnResultBeforeInitCapturesThreadIDFromResult`.
    #[tokio::test]
    async fn run_turn_result_before_init_captures_thread_id_from_result() {
        let _env = ENV_GUARD.read().await;
        let root = TempDir::new();
        let ws = make_ws(&root, "MT-7");
        let (_s, script) = write_fake_claude(
            "#!/usr/bin/env bash\n\
             head -n 1 >/dev/null\n\
             echo '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"session_id\":\"from-result\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}'\n",
        );
        let r = Runner::new(Config {
            command: format!("bash {script}"),
            workspace_root: root.path(),
            turn_timeout: Duration::from_secs(5),
            billing_guard: Some(false),
            ..Default::default()
        });
        let sess = r
            .start_session(&ws, issue("7", "MT-7"), None)
            .await
            .expect("start session");
        let (_t, on_event) = type_collector();
        let (res, err) = sess.run_turn("p", None, None, &on_event).await;
        assert!(err.is_none(), "RunTurn err = {err:?}");
        assert_eq!(res.status, TURN_SUCCEEDED, "status");
        assert_eq!(sess.thread_id(), "from-result", "ThreadID");
    }

    // Mirrors Go `claude.TestRunTurnNonSurfacedLineCapturesThreadID`.
    #[tokio::test]
    async fn run_turn_non_surfaced_line_captures_thread_id() {
        let _env = ENV_GUARD.read().await;
        let root = TempDir::new();
        let ws = make_ws(&root, "MT-NS");
        let (_s, script) = write_fake_claude(
            "#!/usr/bin/env bash\n\
             head -n 1 >/dev/null\n\
             echo '{\"type\":\"system\",\"subtype\":\"status\",\"session_id\":\"sess-x\"}'\n\
             echo '{\"type\":\"future_unknown_event\",\"session_id\":\"sess-ignored\"}'\n\
             echo '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"sess-y\",\"apiKeySource\":\"none\"}'\n\
             echo '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"session_id\":\"sess-y\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}'\n",
        );
        let r = new_runner(&script, &root.path());
        let sess = r
            .start_session(&ws, issue("NS", "MT-NS"), None)
            .await
            .expect("start session");
        let (_t, on_event) = type_collector();
        let (res, err) = sess.run_turn("p", None, None, &on_event).await;
        assert!(err.is_none(), "RunTurn err = {err:?}");
        assert_eq!(res.status, TURN_SUCCEEDED, "status");
        assert_eq!(
            sess.thread_id(),
            "sess-x",
            "session id from the first non-surfaced line must be captured"
        );
    }

    // Mirrors Go `claude.TestRunTurnBillingGuardFailsClosedWithoutSystemInit`.
    #[tokio::test]
    async fn run_turn_billing_guard_fails_closed_without_system_init() {
        let _env = ENV_GUARD.read().await;
        let root = TempDir::new();
        let ws = make_ws(&root, "MT-FC");
        let (_s, script) = write_fake_claude(
            "#!/usr/bin/env bash\n\
             head -n 1 >/dev/null\n\
             echo '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"session_id\":\"s\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}'\n",
        );
        let r = new_runner(&script, &root.path()); // BillingGuard unset => enabled (default)
        let sess = r
            .start_session(&ws, issue("fc", "MT-FC"), None)
            .await
            .expect("start session");
        let (_t, on_event) = type_collector();
        let (res, err) = sess.run_turn("p", None, None, &on_event).await;
        let err = err.expect("want BillingGuard (fail closed on no system/init)");
        assert!(
            err.to_string().contains("billing_guard_failed"),
            "want billing_guard_failed category: {err}"
        );
        assert!(
            err.to_string().contains("no system/init observed"),
            "error should explain missing system/init: {err}"
        );
        assert_eq!(res.status, TURN_FAILED, "status");
    }

    // Mirrors Go `claude.TestCappedBufferBoundsAndRecordsTruncation`.
    #[test]
    fn capped_buffer_bounds_and_records_truncation() {
        let mut c = CappedBuffer::new(16);
        for _ in 0..10 {
            c.write(b"0123456789"); // 10 bytes
        }
        assert_eq!(c.bytes().len(), 16, "buffered bytes, want cap 16");
        assert!(c.truncated, "truncated flag not set after exceeding cap");
        // Exact-fit / under-cap writes must not flag truncation.
        let mut c2 = CappedBuffer::new(16);
        c2.write(b"0123456789");
        assert!(!c2.truncated, "truncated set before reaching cap");
    }

    // Mirrors Go `claude.TestRunTurnStderrIsBounded`.
    #[tokio::test]
    async fn run_turn_stderr_is_bounded() {
        let _env = ENV_GUARD.read().await;
        let root = TempDir::new();
        let ws = make_ws(&root, "MT-CAP");
        let (_s, script) = write_fake_claude(
            "#!/usr/bin/env bash\n\
             head -n 1 >/dev/null\n\
             head -c 1048576 /dev/zero | tr '\\0' 'x' >&2\n\
             exit 3\n",
        );
        let r = new_runner(&script, &root.path());
        let sess = r
            .start_session(&ws, issue("9", "MT-CAP"), None)
            .await
            .expect("start session");
        let (_t, on_event) = type_collector();
        let (_res, err) = sess.run_turn("p", None, None, &on_event).await;
        let err = err.expect("expected error from non-zero exit without result");
        assert!(
            err.to_string().contains("stderr capped"),
            "error should note capping: {err}"
        );
        assert!(
            err.to_string().len() <= 8 * 1024,
            "formatted error unexpectedly large ({} bytes); stderr not bounded",
            err.to_string().len()
        );
    }

    /// STUDIO-675: the run id the worker threads onto the session MUST reach the agent child as
    /// `SYMPHONY_RUN_ID` (and its `RHAPSODY_*` twin), because `teams_post` / `teams_retain` resolve
    /// the posting run from that env and nothing else. Before this wiring the runner hardcoded 0,
    /// so every dispatched teammate's post failed with "SYMPHONY_RUN_ID is not set".
    ///
    /// Mirrors Go `worker.go`'s `SetRunID` call right after `StartSession`.
    #[tokio::test]
    async fn run_turn_emits_run_id_env_after_set_run_id() {
        let _env = ENV_GUARD.read().await;
        let root = TempDir::new();
        let ws = make_ws(&root, "MT-RUNID");
        let (_s, script) = env_dump_script();
        let r = new_runner(&script, &root.path());
        let sess = r
            .start_session(&ws, issue("r1", "MT-RUNID"), None)
            .await
            .expect("start session");
        sess.set_run_id(412);
        let (_t, on_event) = type_collector();
        let (_res, err) = sess.run_turn("p", None, None, &on_event).await;
        assert!(err.is_none(), "RunTurn err = {err:?}");
        let env = read_env_dump(&format!("{ws}/env.dump"));
        assert_eq!(
            env.get("SYMPHONY_RUN_ID").map(String::as_str),
            Some("412"),
            "SYMPHONY_RUN_ID must carry the threaded run id"
        );
        assert_eq!(
            env.get("RHAPSODY_RUN_ID").map(String::as_str),
            Some("412"),
            "RHAPSODY_RUN_ID (STUDIO-603's twin spelling) must match"
        );
        assert_eq!(
            env.get("SYMPHONY_ISSUE").map(String::as_str),
            Some("MT-RUNID"),
            "SYMPHONY_ISSUE must still be set"
        );
    }

    /// The un-set default is unchanged: a session nobody called `set_run_id` on (a coordinator
    /// session, or any Teams-off dispatch) emits NO run-id env at all, exactly as Go does when
    /// `SetRunID` is never called. This is the byte-identical half of the change.
    #[tokio::test]
    async fn run_turn_without_set_run_id_emits_no_run_id_env() {
        let _env = ENV_GUARD.read().await;
        let root = TempDir::new();
        let ws = make_ws(&root, "MT-NORUNID");
        let (_s, script) = env_dump_script();
        let r = new_runner(&script, &root.path());
        let sess = r
            .start_session(&ws, issue("r2", "MT-NORUNID"), None)
            .await
            .expect("start session");
        let (_t, on_event) = type_collector();
        let (_res, err) = sess.run_turn("p", None, None, &on_event).await;
        assert!(err.is_none(), "RunTurn err = {err:?}");
        let env = read_env_dump(&format!("{ws}/env.dump"));
        assert!(
            !env.contains_key("SYMPHONY_RUN_ID") && !env.contains_key("RHAPSODY_RUN_ID"),
            "an unset run id must emit no run-id env"
        );
    }

    /// STUDIO-715 (design record `~/.rhapsody/docs/STUDIO-703-ticketless-pr-review.md`, §14.1
    /// F-SHA): a review run's head SHA is pinned ONCE, when its detached worktree is checked out,
    /// and reaches the agent as `SYMPHONY_REVIEW_HEAD` so the reviewer reports on the commit it is
    /// actually reading rather than re-asking GitHub for a head that may have moved since.
    #[tokio::test]
    async fn run_turn_emits_review_head_env_after_set_review_head() {
        let _env = ENV_GUARD.read().await;
        let root = TempDir::new();
        let ws = make_ws(&root, "pr_o_r_7_alice");
        let (_s, script) = env_dump_script();
        let r = new_runner(&script, &root.path());
        let sess = r
            .start_session(&ws, issue("pr:o/r#7@alice", "pr:o/r#7@alice"), None)
            .await
            .expect("start session");
        let head = "0123456789abcdef0123456789abcdef01234567";
        sess.set_review_head(head);
        let (_t, on_event) = type_collector();
        let (_res, err) = sess.run_turn("p", None, None, &on_event).await;
        assert!(err.is_none(), "RunTurn err = {err:?}");
        let env = read_env_dump(&format!("{ws}/env.dump"));
        assert_eq!(
            env.get("SYMPHONY_REVIEW_HEAD").map(String::as_str),
            Some(head),
            "SYMPHONY_REVIEW_HEAD must carry the pinned head"
        );
        assert_eq!(
            env.get("RHAPSODY_REVIEW_HEAD").map(String::as_str),
            Some(head),
            "RHAPSODY_REVIEW_HEAD (STUDIO-603's twin spelling) must match"
        );
    }

    /// The inertness half: every non-review run — which is every run this slice ships, since nothing
    /// dispatches a review yet — emits no review env at all, so the agent child's environment is
    /// byte-identical to one built before review mode existed.
    #[tokio::test]
    async fn run_turn_without_set_review_head_emits_no_review_env() {
        let _env = ENV_GUARD.read().await;
        let root = TempDir::new();
        let ws = make_ws(&root, "MT-NOREVIEW");
        let (_s, script) = env_dump_script();
        let r = new_runner(&script, &root.path());
        let sess = r
            .start_session(&ws, issue("r4", "MT-NOREVIEW"), None)
            .await
            .expect("start session");
        // An empty pin is the same as never pinning: a non-review dispatch must not emit the var.
        sess.set_review_head("");
        let (_t, on_event) = type_collector();
        let (_res, err) = sess.run_turn("p", None, None, &on_event).await;
        assert!(err.is_none(), "RunTurn err = {err:?}");
        let env = read_env_dump(&format!("{ws}/env.dump"));
        assert!(
            !env.contains_key("SYMPHONY_REVIEW_HEAD") && !env.contains_key("RHAPSODY_REVIEW_HEAD"),
            "a non-review run must emit no review env"
        );
    }

    /// A zero id is a no-op, mirroring Go's `SetRunID` guard: the store being disabled (or
    /// `start_run` having failed) must not emit `SYMPHONY_RUN_ID=0`, which would resolve to no run
    /// and produce a confusing failure instead of the honest "not set".
    #[tokio::test]
    async fn set_run_id_zero_emits_no_run_id_env() {
        let _env = ENV_GUARD.read().await;
        let root = TempDir::new();
        let ws = make_ws(&root, "MT-ZERORUNID");
        let (_s, script) = env_dump_script();
        let r = new_runner(&script, &root.path());
        let sess = r
            .start_session(&ws, issue("r3", "MT-ZERORUNID"), None)
            .await
            .expect("start session");
        sess.set_run_id(0);
        let (_t, on_event) = type_collector();
        let (_res, err) = sess.run_turn("p", None, None, &on_event).await;
        assert!(err.is_none(), "RunTurn err = {err:?}");
        let env = read_env_dump(&format!("{ws}/env.dump"));
        assert!(
            !env.contains_key("SYMPHONY_RUN_ID") && !env.contains_key("RHAPSODY_RUN_ID"),
            "a zero run id must emit no run-id env"
        );
    }

    /// STUDIO-840. An operator Stop cancels a run by DROPPING the worker's future (`spawn_worker`'s
    /// `tokio::select!` on the run's `CancelSignal`), and a dropped `tokio::process::Child` kills
    /// nothing — the `claude` group, spawned into its OWN process group, simply carried on. In
    /// production that meant a stopped run kept committing for 35 minutes and opened a pull request
    /// for a ticket the same Stop had already parked in Backlog.
    ///
    /// The property is "no process of the run survives the drop", which the entry map cannot observe.
    /// So this asserts on a GRANDCHILD the fake forks: it outlives its parent, and it writes the
    /// marker only if something the group kill missed was still running a second after the drop.
    /// Killing just the direct child (`kill_on_drop`) leaves it, so the assertion reds on that too.
    #[tokio::test]
    async fn dropping_a_turn_kills_the_whole_agent_process_group() {
        let _env = ENV_GUARD.read().await;
        let root = TempDir::new();
        let ws = make_ws(&root, "MT-840");
        let survivor = root.join("survivor");
        // Fork the grandchild BEFORE announcing the session, so the init event proves it exists.
        let (_s, script) = write_fake_claude(&format!(
            "#!/usr/bin/env bash\n\
             head -n 1 >/dev/null\n\
             ( sleep 1; : > {survivor} ) &\n\
             echo '{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s\",\"apiKeySource\":\"none\"}}'\n\
             sleep 30\n"
        ));
        // A long turn timeout: the deadline kill must not be what stops this run, or the test would
        // pass while the cancellation path is still inert.
        let r = Runner::new(Config {
            command: format!("bash {script}"),
            workspace_root: root.path(),
            turn_timeout: Duration::from_secs(60),
            ..Default::default()
        });
        let sess = r
            .start_session(&ws, issue("840", "MT-840"), None)
            .await
            .expect("start session");
        let (types, on_event) = type_collector();
        let mut turn = Box::pin(sess.run_turn("p", None, None, &on_event));
        // Drive the turn until the child has announced itself (and so has forked the grandchild).
        let started = async {
            loop {
                tokio::select! {
                    _ = &mut turn => panic!("the fake turn ended on its own"),
                    _ = tokio::time::sleep(Duration::from_millis(20)) => {
                        if !types.lock().expect("types lock").is_empty() {
                            break;
                        }
                    }
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(10), started)
            .await
            .expect("the agent child never started");

        drop(turn); // the Stop: `terminate` fires the cancel and the worker drops this future

        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(
            !std::path::Path::new(&survivor).exists(),
            "a process of the stopped run survived the cancellation and kept working ({survivor})"
        );
    }

    // ── Per-teammate model/effort (STUDIO-868) ──────────────────────────────────────────────────
    //
    // The property under test is WHICH ARGV a given identity produces, so these assert on the
    // argv the child process actually received (`args.log`, written by the fake from `"$@"`) —
    // never on the config that argv was built from.

    /// Writes a fake claude that appends its own argv to `args.log` in its cwd and then emits a
    /// successful one-turn stream.
    fn write_argv_recording_claude() -> (TempDir, String) {
        write_fake_claude(
            "#!/usr/bin/env bash\n\
             echo \"$@\" >> args.log\n\
             head -n 1 >/dev/null\n\
             echo '{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s\",\"apiKeySource\":\"none\"}'\n\
             echo '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"session_id\":\"s\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}'\n",
        )
    }

    /// Runs ONE turn in its own workspace under `root` and returns the argv line the child saw.
    async fn argv_of_one_turn(
        r: &Runner,
        root: &TempDir,
        ws_id: &str,
        over: Option<crate::ModelOverride>,
    ) -> String {
        let ws = make_ws(root, ws_id);
        let sess = r
            .start_session(&ws, issue(ws_id, ws_id), None)
            .await
            .expect("start session");
        if let Some(o) = over {
            sess.set_model_override(o);
        }
        let (_t, on_event) = type_collector();
        let (_res, err) = sess.run_turn("p", None, None, &on_event).await;
        assert!(err.is_none(), "{ws_id}: {err:?}");
        std::fs::read_to_string(format!("{ws}/args.log"))
            .expect("read args.log")
            .trim()
            .to_string()
    }

    /// A runner over the argv-recording fake carrying the INSTALLATION-WIDE model/effort — the
    /// single global pair every run shared before STUDIO-868.
    fn global_model_runner(script: &str, root: &str) -> Runner {
        Runner::new(Config {
            command: format!("bash {script}"),
            workspace_root: root.to_string(),
            turn_timeout: Duration::from_secs(5),
            model: "global-model".to_string(),
            effort: "medium".to_string(),
            ..Default::default()
        })
    }

    /// **The STUDIO-868 acceptance criterion.** Two teammates whose profiles name different
    /// model/effort produce different `--model`/`--effort` on the wire — from ONE runner, which is
    /// the whole point: a runner is built per project and shared, and Teams picks the identity
    /// later.
    #[tokio::test]
    async fn two_teammates_argv_carry_their_own_profile_model_and_effort() {
        let _env = ENV_GUARD.read().await;
        let root = TempDir::new();
        let (_s, script) = write_argv_recording_claude();
        let r = global_model_runner(&script, &root.path());

        let staff = argv_of_one_turn(
            &r,
            &root,
            "MT-STAFF",
            Some(crate::ModelOverride {
                identity: "alice".to_string(),
                model: "strong-model".to_string(),
                effort: "xhigh".to_string(),
            }),
        )
        .await;
        let swe1 = argv_of_one_turn(
            &r,
            &root,
            "MT-SWE1",
            Some(crate::ModelOverride {
                identity: "jerry".to_string(),
                model: "cheap-model".to_string(),
                effort: "low".to_string(),
            }),
        )
        .await;

        assert!(
            staff.contains("--model strong-model") && staff.contains("--effort xhigh"),
            "the staff engineer's profile must reach the argv: {staff:?}"
        );
        assert!(
            swe1.contains("--model cheap-model") && swe1.contains("--effort low"),
            "the SWE I's profile must reach the argv: {swe1:?}"
        );
        assert!(
            !staff.contains("global-model") && !swe1.contains("global-model"),
            "a profile value REPLACES the global, it does not join it: {staff:?} / {swe1:?}"
        );
        assert_ne!(
            staff, swe1,
            "alice and jerry must stop being the same engineer"
        );
    }

    /// **The regression that would silently re-model every existing run.** A teammate whose profile
    /// leaves model/effort empty — which is every built-in (`swe`, `reviewer`, `sre` all ship
    /// `model: ""`) — gets argv BYTE-IDENTICAL to the global-only path. Asserted as equality
    /// against a session that was never given an override at all, not as a substring.
    #[tokio::test]
    async fn an_inheriting_teammate_gets_argv_byte_identical_to_the_global_only_path() {
        let _env = ENV_GUARD.read().await;
        let root = TempDir::new();
        let (_s, script) = write_argv_recording_claude();
        let r = global_model_runner(&script, &root.path());

        // No override at all: an installation with no `teams/profiles/` directory, or Teams off.
        let untouched = argv_of_one_turn(&r, &root, "MT-GLOBAL", None).await;
        // Routed to a teammate whose resolved profile names neither value.
        let inheriting = argv_of_one_turn(
            &r,
            &root,
            "MT-INHERIT",
            Some(crate::ModelOverride {
                identity: "jimmy".to_string(),
                model: String::new(),
                effort: String::new(),
            }),
        )
        .await;

        assert_eq!(
            inheriting, untouched,
            "an empty profile must leave the argv byte-identical to today"
        );
        assert!(
            untouched.contains("--model global-model") && untouched.contains("--effort medium"),
            "and that argv is still the global pair: {untouched:?}"
        );
    }

    /// Precedence is per-FIELD, not all-or-nothing: a profile that names only a model inherits the
    /// global effort, and vice versa.
    #[tokio::test]
    async fn a_half_filled_profile_overrides_only_the_field_it_names() {
        let _env = ENV_GUARD.read().await;
        let root = TempDir::new();
        let (_s, script) = write_argv_recording_claude();
        let r = global_model_runner(&script, &root.path());

        let model_only = argv_of_one_turn(
            &r,
            &root,
            "MT-MODEL",
            Some(crate::ModelOverride {
                identity: "alice".to_string(),
                model: "strong-model".to_string(),
                effort: String::new(),
            }),
        )
        .await;
        assert!(
            model_only.contains("--model strong-model") && model_only.contains("--effort medium"),
            "an unnamed effort inherits the global: {model_only:?}"
        );

        let effort_only = argv_of_one_turn(
            &r,
            &root,
            "MT-EFFORT",
            Some(crate::ModelOverride {
                identity: "alice".to_string(),
                model: String::new(),
                effort: "xhigh".to_string(),
            }),
        )
        .await;
        assert!(
            effort_only.contains("--model global-model") && effort_only.contains("--effort xhigh"),
            "an unnamed model inherits the global: {effort_only:?}"
        );
    }

    /// The override is re-read every turn, so a `--resume` continuation stays on the teammate's
    /// model. `build_args` runs per turn, not per runner — this pins that that stays true.
    #[tokio::test]
    async fn a_continuation_turn_keeps_the_teammates_model() {
        let _env = ENV_GUARD.read().await;
        let root = TempDir::new();
        let ws = make_ws(&root, "MT-RESUME");
        let (_s, script) = write_argv_recording_claude();
        let r = global_model_runner(&script, &root.path());
        let sess = r
            .start_session(&ws, issue("RESUME", "MT-RESUME"), None)
            .await
            .expect("start session");
        sess.set_model_override(crate::ModelOverride {
            identity: "alice".to_string(),
            model: "strong-model".to_string(),
            effort: "xhigh".to_string(),
        });
        let (_t, on_event) = type_collector();
        let (_r1, e1) = sess.run_turn("first", None, None, &on_event).await;
        assert!(e1.is_none(), "turn 1: {e1:?}");
        let (_r2, e2) = sess.run_turn("continue", Some(2), None, &on_event).await;
        assert!(e2.is_none(), "turn 2: {e2:?}");

        let log = std::fs::read_to_string(format!("{ws}/args.log")).expect("read args.log");
        let lines: Vec<&str> = log.trim().split('\n').collect();
        assert_eq!(lines.len(), 2, "expected 2 invocations: {log:?}");
        for (n, line) in lines.iter().enumerate() {
            assert!(
                line.contains("--model strong-model") && line.contains("--effort xhigh"),
                "turn {} lost the teammate's model: {line:?}",
                n + 1
            );
        }
        assert!(
            lines[1].contains("--resume s"),
            "turn 2 resumes: {:?}",
            lines[1]
        );
    }

    /// **Decision 1 of STUDIO-868.** A profile naming a model the CLI rejects FAILS the run — that
    /// is not softened — but the failure must say whose profile asked for it. The CLI's own refusal
    /// is a non-zero exit with a one-line stderr that names neither the teammate nor the fact that
    /// the value came from a profile at all.
    #[tokio::test]
    async fn a_rejected_profile_model_fails_naming_the_teammate_and_the_model() {
        let _env = ENV_GUARD.read().await;
        let root = TempDir::new();
        let ws = make_ws(&root, "MT-BADMODEL");
        let (_s, script) = write_fake_claude(
            "#!/usr/bin/env bash\n\
             echo 'error: unknown model' >&2\n\
             exit 1\n",
        );
        let r = global_model_runner(&script, &root.path());
        let sess = r
            .start_session(&ws, issue("BAD", "MT-BADMODEL"), None)
            .await
            .expect("start session");
        sess.set_model_override(crate::ModelOverride {
            identity: "jerry".to_string(),
            model: "no-such-model".to_string(),
            effort: String::new(),
        });
        let (res, err) = sess.run_turn("p", None, None, &type_collector().1).await;
        assert_eq!(res.status, TURN_FAILED, "the run still fails");
        let msg = err
            .expect("a rejected model must fail the turn")
            .to_string();
        assert!(
            msg.contains("jerry") && msg.contains("no-such-model"),
            "the failure must name the teammate and the model it asked for: {msg:?}"
        );
        assert!(
            msg.contains("unknown model"),
            "and must not swallow the CLI's own stderr: {msg:?}"
        );
    }

    /// The other half of the same decision: with no profile override in play the failure text is
    /// BYTE-IDENTICAL to what a daemon built before STUDIO-868 produced — no empty brackets, no
    /// dangling "teammate ".
    #[tokio::test]
    async fn a_failure_with_no_override_carries_no_attribution_at_all() {
        let _env = ENV_GUARD.read().await;
        let root = TempDir::new();
        let ws = make_ws(&root, "MT-BADPLAIN");
        let (_s, script) = write_fake_claude(
            "#!/usr/bin/env bash\n\
             echo 'error: unknown model' >&2\n\
             exit 1\n",
        );
        let r = global_model_runner(&script, &root.path());
        let sess = r
            .start_session(&ws, issue("PLAIN", "MT-BADPLAIN"), None)
            .await
            .expect("start session");
        let (_res, err) = sess.run_turn("p", None, None, &type_collector().1).await;
        let msg = err.expect("the turn fails").to_string();
        assert_eq!(
            msg, "turn_failed: exit status: 1: error: unknown model\n",
            "an unrouted run's failure text must be unchanged"
        );
    }
}
