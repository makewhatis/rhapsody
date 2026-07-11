//! worker — parity port of Go `internal/orchestrator/worker.go` (the per-run agent driver).
//!
//! One worker attempt (upstream §16.5): ensure the per-issue workspace, install the graphite guard,
//! resolve + render the prompt, run `before_run`, open a transcript, start the agent session, drive
//! the continuation-turn loop while the issue stays active (up to `max_turns`), then stop the
//! session and run `after_run` + the post-run PR labeler. It touches NO orchestrator state — it
//! communicates outward through the `on_event` callback (Go's event forwarding to the control loop)
//! and returns `(last_state, declared_handoff, err)` so the caller can classify the exit (INF-266 /
//! INF-272). The spawn of this attempt as a task and the wiring of `on_event` back into the control
//! loop are O7's (the loop + the control-event channel).
//!
//! Deviations from the Go source, all behavior-preserving:
//!   * Go's goroutine-per-worker + `ctx` cancellation maps to an `async fn`: cancellation is a
//!     dropped future (O7 owns the task's abort handle), so there is no `ctx` parameter.
//!   * Telemetry is P6: Go's `symphony.run`/`worktree.ensure`/`turn` spans and the `turn.duration` /
//!     token metrics are NOT emitted here (the `WorkerDeps` telemetry fields — `Tracer`, `Metrics`,
//!     `Model`, `DispatchSpanContext` — and `RunID` are dropped; the per-event `tracing::debug!`
//!     forwarding line is kept). The bounded metric labels live in [`crate::telemetry_attrs`].
//!   * Go threads the store run id onto the session (`SetRunID`); the A1 `Session` trait exposes no
//!     setter yet (a documented P5 wiring concern in the agent crate), so that call is a no-op here.
//!   * The operator-message mailbox (`messages`, INF-250) is threaded to [`Session::run_turn`] for
//!     parity; its delivery source (the mailbox + control-loop routing) is O6/O7, so every O3 caller
//!     passes `None`.

use std::collections::HashSet;
use std::sync::Arc;

use rhapsody_agent::{self as agent, Event, Runner, Session, Transcript};
use rhapsody_config::WORKSPACE_MODE_CLONE;
use rhapsody_core::{Issue, normalize_state};
use rhapsody_tracker::Tracker;
use rhapsody_workspace::{self as workspace, Manager, gtguard};
use tokio::sync::mpsc;

use crate::obslog;

/// Sent on continuation turns instead of re-rendering the full task prompt, which is already in the
/// thread history (upstream §7.1). Mirrors Go `continuationGuidance`.
pub const CONTINUATION_GUIDANCE: &str = "Continue working on this issue on the existing thread. \
If the work is complete, ensure the ticket has been moved to the appropriate handoff state and any \
pull request is linked, and end your final message with a 'HANDOFF:' line. Otherwise, keep going.";

/// A prompt-source resolution failure (missing/unreadable/empty prompt file). Its `Display` names the
/// offending path, mirroring Go's `fmt.Errorf("prompt_file %q: %w", …)` sentinels — the worker tests
/// assert the path appears in the message.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct PromptFileError(String);

/// The unified worker-attempt error. Mirrors Go's single `error` return, categorized by source so the
/// `Display` strings stay the underlying sentinels the callers/tests observe.
#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    /// A workspace-manager failure (ensure/before_run/after_run). Go returns it verbatim.
    #[error(transparent)]
    Workspace(#[from] workspace::Error),
    /// A graphite guardrail install failure — enforcement must be deterministic, so a write failure
    /// fails the run. Mirrors Go `fmt.Errorf("install graphite guard: %w", gerr)`.
    #[error("install graphite guard: {0}")]
    GraphiteGuard(workspace::Error),
    /// A prompt-file resolution failure (path named in the message).
    #[error(transparent)]
    PromptFile(#[from] PromptFileError),
    /// A strict-variable prompt render failure (`template_render_error: …`).
    #[error(transparent)]
    Render(#[from] rhapsody_config::prompt::RenderError),
    /// An agent session/turn failure (start, run_turn).
    #[error(transparent)]
    Agent(#[from] agent::AgentError),
    /// A tracker state-refresh failure during the turn loop.
    #[error(transparent)]
    Tracker(#[from] rhapsody_tracker::TrackerError),
}

/// The dependencies a single worker attempt needs (Go `WorkerDeps`). The `Arc`-held deps are the
/// resolved-project runner/tracker/workspace; the rest is the effective per-dispatch config.
///
/// Dropped-from-Go fields are telemetry (P6): `Logger`, `Tracer`, `Metrics`, `Model`,
/// `DispatchSpanContext`, and the `RunID` that fed the unported `SetRunID` (see the module docs).
pub struct WorkerDeps {
    pub workspace: Arc<Manager>,
    pub agent: Arc<dyn Runner>,
    pub tracker: Arc<dyn Tracker>,
    pub prompt_tmpl: String,
    /// When non-empty, WINS over `prompt_tmpl`: the template is read from this path at run time (a
    /// relative path from the per-issue checkout, an absolute/`~` path from the daemon host).
    pub prompt_file: String,
    pub max_turns: i64,
    /// Normalized (lowercase) active state names.
    pub active_states: HashSet<String>,
    /// Local raw-transcript store; `None` disables local logging.
    pub transcripts: Option<Arc<obslog::Store>>,
    /// The owning project's repo URL; empty ⇒ the legacy mkdir workspace (no bare mirror).
    pub repo_url: String,
    /// The owning resolved project's slug; surfaced to lifecycle hooks as SYMPHONY_PROJECT.
    pub project_slug: String,
    /// Effective git-workflow policy; `"graphite"` injects the guard hook before spawn (INF-251).
    pub git_flow: String,
    /// Effective workspace-provisioning policy: `"clone"` provisions an independent clone, anything
    /// else uses the shared-mirror worktree path (INF-418).
    pub workspace_mode: String,
    /// The graphite-mode predecessor stacking hint, prepended to the FIRST-turn prompt only (INF-318).
    pub stack_context: String,
    /// The GitHub label name the post-run labeler adds to every PR in this run's stack (AIE-301).
    pub pr_label: String,
}

/// Returns the prompt template for this run. When `prompt_file` is empty the inline `prompt_tmpl` is
/// used unchanged; otherwise the file WINS and is read at run time. Mirrors Go `resolvePromptTemplate`.
///
/// Soft fallback (INF-279): a NOT-FOUND or empty RELATIVE (repo-relative) `prompt_file` is NOT fatal —
/// it returns the inline `prompt_tmpl` with a non-empty warn so the caller logs the fallback rather
/// than failing the run. The soft path is restricted to genuine absence and emptiness: a relative
/// path that exists but cannot be read (permission denied, a directory) HARD-fails so a real fault
/// never masquerades as "not found". ABSOLUTE and `~`-prefixed host paths keep HARD-failing on every
/// error. `warn` is empty on success and on every hard-fail; the error is non-nil only on hard-fails.
pub(crate) fn resolve_prompt_template(
    prompt_tmpl: &str,
    prompt_file: &str,
    ws_path: &str,
) -> Result<(String, String), PromptFileError> {
    // Trim once: a whitespace-only path means "unset" (inline wins); a real path with stray
    // whitespace must resolve/read by its trimmed form.
    let prompt_file = prompt_file.trim();
    if prompt_file.is_empty() {
        return Ok((prompt_tmpl.to_string(), String::new()));
    }
    // Classify the path BEFORE reading so the fallback branch knows whether a miss is soft
    // (repo-relative) or hard (absolute/host).
    let (path, relative): (std::path::PathBuf, bool) =
        if prompt_file == "~" || prompt_file.starts_with("~/") {
            let home = std::env::var_os("HOME").ok_or_else(|| {
                PromptFileError(format!(
                    "prompt_file {prompt_file:?}: cannot expand ~: HOME not set"
                ))
            })?;
            let home = std::path::PathBuf::from(home);
            if prompt_file == "~" {
                (home, false)
            } else {
                (home.join(&prompt_file[2..]), false)
            }
        } else if std::path::Path::new(prompt_file).is_absolute() {
            // absolute: read verbatim from the daemon host
            (std::path::PathBuf::from(prompt_file), false)
        } else {
            // relative: repo-relative, resolved against the per-issue checkout
            (std::path::Path::new(ws_path).join(prompt_file), true)
        };
    match std::fs::read(&path) {
        Ok(b) => {
            let text = String::from_utf8_lossy(&b);
            if text.trim().is_empty() {
                if relative {
                    return Ok((
                        prompt_tmpl.to_string(),
                        format!("prompt_file {prompt_file:?} is empty — using the inline prompt"),
                    ));
                }
                return Err(PromptFileError(format!(
                    "prompt_file {prompt_file:?} is empty"
                )));
            }
            Ok((text.into_owned(), String::new()))
        }
        Err(e) => {
            // Repo-relative prompt genuinely ABSENT: fall back to the inline prompt. A non-not-found
            // read error (permission, a directory) is a real fault and hard-fails below; absolute/host
            // paths always hard-fail.
            if relative && e.kind() == std::io::ErrorKind::NotFound {
                return Ok((
                    prompt_tmpl.to_string(),
                    format!(
                        "prompt_file {prompt_file:?} not found in checkout — using the inline prompt"
                    ),
                ));
            }
            Err(PromptFileError(format!("prompt_file {prompt_file:?}: {e}")))
        }
    }
}

/// Renders the full template on the first turn and returns fixed continuation guidance on later turns
/// (upstream §7.1, §12.3). A non-empty `stack_context` is prepended (as plain text) to the FIRST-turn
/// prompt only (INF-318). Mirrors Go `buildTurnPrompt`.
pub(crate) fn build_turn_prompt(
    tmpl: &str,
    stack_context: &str,
    iss: &Issue,
    attempt: Option<i32>,
    turn: i64,
) -> Result<String, rhapsody_config::prompt::RenderError> {
    if turn <= 1 {
        let rendered = rhapsody_config::prompt::render(tmpl, iss, attempt)?;
        if !stack_context.is_empty() {
            return Ok(format!("{stack_context}\n\n{rendered}"));
        }
        return Ok(rendered);
    }
    Ok(CONTINUATION_GUIDANCE.to_string())
}

/// Reports whether `result_text` declares hand-off: any line whose trimmed form begins with
/// `"HANDOFF:"`. Liberal on payload (INF-272). Mirrors Go `hasHandoffMarker` (defined in Go's
/// `retry.go`; the worker is its first consumer here, and O5 reuses it).
pub(crate) fn has_handoff_marker(result_text: &str) -> bool {
    result_text
        .lines()
        .any(|ln| ln.trim().starts_with("HANDOFF:"))
}

/// Performs one worker attempt (upstream §16.5). Returns the worker's last-known issue state — the
/// per-turn refresh from [`WorkerDeps::run_turns`], or the dispatch-time snapshot when the run fails
/// before any turn completes — alongside the hand-off declaration and the abnormal-exit error (`None`
/// on a normal exit). `on_transcript` is invoked (best-effort) with the CONCRETE per-run transcript
/// path the moment the transcript opens. Mirrors Go `runAgentAttempt`.
pub async fn run_agent_attempt(
    deps: &WorkerDeps,
    issue: Issue,
    attempt: Option<i32>,
    messages: Option<&mut mpsc::Receiver<String>>,
    on_event: &(dyn Fn(Event) + Send + Sync),
    on_transcript: Option<&(dyn Fn(&str) + Send + Sync)>,
) -> (String, bool, Option<WorkerError>) {
    // workspace_mode:clone provisions an independent clone (no cross-ticket checkout lock); anything
    // else uses the shared-mirror worktree path. Both run the same downstream pipeline.
    let ws = if deps.workspace_mode == WORKSPACE_MODE_CLONE {
        deps.workspace
            .ensure_clone_from_repo(&deps.repo_url, &deps.project_slug, &issue.identifier)
            .await
    } else {
        deps.workspace
            .ensure_from_repo(&deps.repo_url, &deps.project_slug, &issue.identifier)
            .await
    };
    let ws = match ws {
        Ok(w) => w,
        Err(e) => return (issue.state.clone(), false, Some(e.into())),
    };
    // Inject the Graphite guardrail hook into the worktree BEFORE spawn when git_flow is "graphite"
    // (INF-251). Idempotent on a reused worktree; a no-op for any other policy. A write failure fails
    // the run — we never spawn an agent that was supposed to be guarded but is not.
    match gtguard::ensure_for_git_flow(&ws.path, &deps.git_flow) {
        Ok(wrote) => {
            if wrote {
                tracing::debug!(
                    issue_identifier = %issue.identifier,
                    worktree = %ws.path,
                    "installed graphite guardrail hook"
                );
            }
        }
        Err(e) => {
            return (
                issue.state.clone(),
                false,
                Some(WorkerError::GraphiteGuard(e)),
            );
        }
    }
    // Resolve the prompt template now the repo is checked out. Read ONCE; continuation turns keep
    // using CONTINUATION_GUIDANCE. A missing/unreadable/empty file fails the run before any agent work.
    let (prompt_tmpl, warn) =
        match resolve_prompt_template(&deps.prompt_tmpl, &deps.prompt_file, &ws.path) {
            Ok(v) => v,
            Err(e) => return (issue.state.clone(), false, Some(e.into())),
        };
    // A soft fallback (relative prompt_file missing/empty) does not fail the run; surface it.
    if !warn.is_empty() {
        tracing::warn!(
            issue_identifier = %issue.identifier,
            detail = %warn,
            "prompt_file fallback"
        );
    }
    if let Err(e) = deps
        .workspace
        .before_run(&ws, &deps.repo_url, &deps.project_slug, &issue.identifier)
        .await
    {
        return (issue.state.clone(), false, Some(e.into()));
    }

    // Open a per-run transcript (best-effort: a failure logs and continues without local logging).
    let mut transcript: Option<Transcript> = None;
    let mut _run_guard: Option<obslog::Run> = None;
    if let Some(store) = &deps.transcripts {
        match store.open(&issue.identifier) {
            Err(e) => {
                tracing::warn!(
                    issue_identifier = %issue.identifier,
                    err = %e,
                    "transcript open failed; continuing without local logging"
                );
            }
            Ok(run) => match (run.stdout(), run.stderr()) {
                (Ok(so), Ok(se)) => {
                    transcript = Some(Transcript {
                        stdout: Some(Box::new(so)),
                        stderr: Some(Box::new(se)),
                    });
                    // Report the CONCRETE per-run transcript file (timestamped *.jsonl, not the
                    // latest.jsonl alias) so the control goroutine can record it on the run row.
                    if let Some(cb) = on_transcript {
                        cb(run.path());
                    }
                    _run_guard = Some(run);
                }
                _ => {
                    tracing::warn!(
                        issue_identifier = %issue.identifier,
                        "transcript handle clone failed; continuing without local logging"
                    );
                }
            },
        }
    }

    let sess = match deps
        .agent
        .start_session(&ws.path, issue.clone(), transcript)
        .await
    {
        Ok(s) => s,
        Err(e) => {
            // best-effort cleanup (§9.4)
            let _ = deps
                .workspace
                .after_run(&ws, &deps.repo_url, &deps.project_slug, &issue.identifier)
                .await;
            return (issue.state.clone(), false, Some(e.into()));
        }
    };
    // (Go threads the store run id onto the session via SetRunID here; the A1 `Session` trait exposes
    // no setter yet, so it is a no-op — see the module docs.)

    let (final_state, result_text, loop_err) = deps
        .run_turns(
            sess.as_ref(),
            &prompt_tmpl,
            issue.clone(),
            attempt,
            messages,
            on_event,
        )
        .await;

    let _ = sess.stop().await; // best-effort
    let _ = deps
        .workspace
        .after_run(&ws, &deps.repo_url, &deps.project_slug, &issue.identifier)
        .await; // best-effort (§9.4)
    // Tag every PR in this run's stack with the configured label using the agent's inherited gh auth
    // (AIE-301). Best-effort post-run cleanup; label_run_prs swallows every gh/git error so labeling
    // never changes the run outcome.
    deps.workspace
        .label_run_prs(&deps.repo_url, &ws.path, &deps.pr_label)
        .await;

    (final_state, has_handoff_marker(&result_text), loop_err)
}

impl WorkerDeps {
    /// Drives the continuation-turn loop on a live session. `prompt_tmpl` is the resolved first-turn
    /// template. Returns the worker's last-known issue state (refreshed after every completed turn)
    /// alongside the freshest final result text and any abnormal-exit error. Mirrors Go
    /// `WorkerDeps.runTurns`.
    pub(crate) async fn run_turns(
        &self,
        sess: &dyn Session,
        prompt_tmpl: &str,
        issue: Issue,
        attempt: Option<i32>,
        mut messages: Option<&mut mpsc::Receiver<String>>,
        on_event: &(dyn Fn(Event) + Send + Sync),
    ) -> (String, String, Option<WorkerError>) {
        let mut turn: i64 = 1;
        let mut last_result = String::new();
        let mut issue = issue;
        loop {
            let p = match build_turn_prompt(prompt_tmpl, &self.stack_context, &issue, attempt, turn)
            {
                Ok(p) => p,
                Err(e) => return (issue.state.clone(), last_result, Some(e.into())),
            };
            let ident = issue.identifier.clone();
            let (tr, terr) = {
                // Emit the agent event as a (trace-correlated) log line and forward it. Scoped so the
                // forwarding closure is dropped before `issue.state` is mutated below.
                let wrapped = move |e: Event| {
                    tracing::debug!(
                        issue_identifier = %ident,
                        event = %e.event_type,
                        message = %e.message,
                        "agent event"
                    );
                    on_event(e);
                };
                sess.run_turn(
                    &p,
                    attempt.map(i64::from),
                    messages.as_deref_mut(),
                    &wrapped,
                )
                .await
            };
            if let Some(e) = terr {
                return (issue.state.clone(), last_result, Some(e.into()));
            }
            // Remember the freshest final result text; the HANDOFF: marker (if any) is on the last
            // completed turn, which is what the caller classifies against (INF-272).
            last_result = tr.result_text;
            let ids = [issue.id.clone()];
            let refreshed = match self.tracker.fetch_issue_states_by_ids(&ids).await {
                Ok(r) => r,
                Err(e) => return (issue.state.clone(), last_result, Some(e.into())),
            };
            // Empty refresh (issue not found / partial result): keep the prior state and treat the
            // issue as still in its last-known state.
            if !refreshed.is_empty() {
                issue.state = refreshed[0].state.clone();
            }
            if !self.active_states.contains(&normalize_state(&issue.state)) {
                // issue no longer active → normal exit (winds down, no new turns)
                return (issue.state.clone(), last_result, None);
            }
            if turn >= self.max_turns {
                // turn budget exhausted → normal exit
                return (issue.state.clone(), last_result, None);
            }
            turn += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use rhapsody_agent::fake as agentfake;
    use rhapsody_agent::{
        AgentError, EVENT_NOTIFICATION, EVENT_SESSION_STARTED, Event, TURN_FAILED, TURN_SUCCEEDED,
        TurnResult,
    };
    use rhapsody_core::Issue;
    use rhapsody_tracker::fake as trackerfake;
    use rhapsody_workspace::{self as workspace, HookScripts, Manager};

    use super::*;
    use crate::testsupport::{TempDir, issue};

    fn test_workspace(hooks: HookScripts) -> (Arc<Manager>, TempDir) {
        let root = TempDir::new();
        let m = Manager::new(workspace::Config {
            root: root.path.clone(),
            hooks,
            hook_timeout: Duration::from_secs(5),
        })
        .expect("workspace manager");
        (Arc::new(m), root)
    }

    /// Builds `WorkerDeps` over the given workspace with the standard active set (Go `baseDeps`).
    fn make_deps(
        ws: Arc<Manager>,
        ag: Arc<dyn Runner>,
        tr: Arc<dyn Tracker>,
        tmpl: &str,
        max_turns: i64,
    ) -> WorkerDeps {
        WorkerDeps {
            workspace: ws,
            agent: ag,
            tracker: tr,
            prompt_tmpl: tmpl.to_string(),
            prompt_file: String::new(),
            max_turns,
            active_states: ["todo".to_string(), "in progress".to_string()]
                .into_iter()
                .collect(),
            transcripts: None,
            repo_url: String::new(),
            project_slug: String::new(),
            git_flow: String::new(),
            workspace_mode: String::new(),
            stack_context: String::new(),
            pr_label: String::new(),
        }
    }

    fn succeeded_turn() -> agentfake::TurnScript {
        agentfake::TurnScript {
            result: TurnResult {
                status: TURN_SUCCEEDED.to_string(),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn fake_agent(turns: Vec<agentfake::TurnScript>) -> Arc<agentfake::Fake> {
        let mut ag = agentfake::Fake::new();
        ag.turns = turns;
        Arc::new(ag)
    }

    fn fake_tracker_by_id(entries: &[(&str, &str, &str)]) -> Arc<trackerfake::Fake> {
        let mut tr = trackerfake::Fake::new();
        tr.by_id = entries
            .iter()
            .map(|(id, ident, state)| (id.to_string(), issue(id, ident, state)))
            .collect();
        Arc::new(tr)
    }

    fn noop_event() -> impl Fn(Event) + Send + Sync {
        |_e: Event| {}
    }

    fn dispatched() -> Issue {
        issue("1", "MT-1", "In Progress")
    }

    // Mirrors Go `TestBuildTurnPromptFirstVsContinuation`.
    #[test]
    fn build_turn_prompt_first_vs_continuation() {
        let iss = issue("", "MT-1", "Todo");
        let first =
            build_turn_prompt("Work {{ issue.identifier }}", "", &iss, None, 1).expect("render");
        assert_eq!(first, "Work MT-1");
        let cont =
            build_turn_prompt("Work {{ issue.identifier }}", "", &iss, None, 2).expect("render");
        assert_eq!(cont, CONTINUATION_GUIDANCE);
    }

    // Mirrors Go `TestBuildTurnPromptStackContext`: the stack context prepends the first-turn prompt
    // only; continuation turns are unchanged (INF-318).
    #[test]
    fn build_turn_prompt_stack_context() {
        let iss = issue("", "MT-2", "Todo");
        let stack = "STACK ON: feat/mt-1 (PR #7) — create your branch stacked on this predecessor.";
        let first =
            build_turn_prompt("Work {{ issue.identifier }}", stack, &iss, None, 1).expect("render");
        assert_eq!(first, format!("{stack}\n\nWork MT-2"));
        let cont =
            build_turn_prompt("Work {{ issue.identifier }}", stack, &iss, None, 2).expect("render");
        assert_eq!(cont, CONTINUATION_GUIDANCE);
    }

    // Mirrors Go `TestResolvePromptTemplate`: inline when no file; repo-relative reads from the
    // checkout; absolute reads from the host; a missing/empty RELATIVE file soft-falls-back with a
    // warning; a missing/empty ABSOLUTE file and an unreadable relative path hard-fail naming the path.
    #[test]
    fn resolve_prompt_template_cases() {
        let ws = TempDir::new();
        std::fs::create_dir_all(ws.child("prompts")).unwrap();
        std::fs::write(ws.child("prompts/rel.md"), "rel {{ issue.identifier }}").unwrap();
        let abs_dir = TempDir::new();
        let abs_file = abs_dir.child("abs.md");
        std::fs::write(&abs_file, "abs body").unwrap();
        let empty_file = abs_dir.child("empty.md");
        std::fs::write(&empty_file, "   \n").unwrap();
        let abs_missing = abs_dir.child("gone.md");

        // inline when no file
        let (got, warn) = resolve_prompt_template("inline body", "", &ws.path).unwrap();
        assert_eq!((got.as_str(), warn.as_str()), ("inline body", ""));
        // repo-relative reads from the checkout
        let (got, warn) =
            resolve_prompt_template("inline body", "prompts/rel.md", &ws.path).unwrap();
        assert_eq!(got, "rel {{ issue.identifier }}");
        assert_eq!(warn, "");
        // absolute reads from the host
        let (got, warn) = resolve_prompt_template("inline body", &abs_file, &ws.path).unwrap();
        assert_eq!((got.as_str(), warn.as_str()), ("abs body", ""));
        // trims surrounding whitespace in the path
        let (got, warn) =
            resolve_prompt_template("inline body", "  prompts/rel.md\n", &ws.path).unwrap();
        assert_eq!(got, "rel {{ issue.identifier }}");
        assert_eq!(warn, "");
        // missing RELATIVE file → soft fallback with a warning naming the path
        let (got, warn) =
            resolve_prompt_template("inline body", "prompts/nope.md", &ws.path).unwrap();
        assert_eq!(got, "inline body");
        assert!(warn.contains("prompts/nope.md"), "warn = {warn:?}");
        // empty RELATIVE file → soft fallback with a warning
        std::fs::write(ws.child("prompts/blank.md"), "  \n").unwrap();
        let (got, warn) =
            resolve_prompt_template("inline body", "prompts/blank.md", &ws.path).unwrap();
        assert_eq!(got, "inline body");
        assert!(!warn.is_empty());
        // unreadable RELATIVE path (a directory) → hard fail, no soft-fallback warn
        std::fs::create_dir_all(ws.child("prompts/isdir.md")).unwrap();
        let err = resolve_prompt_template("inline body", "prompts/isdir.md", &ws.path).unwrap_err();
        assert!(err.to_string().contains("prompts/isdir.md"), "err = {err}");
        // missing ABSOLUTE file → hard fail naming the path
        let err = resolve_prompt_template("inline body", &abs_missing, &ws.path).unwrap_err();
        assert!(err.to_string().contains("gone.md"), "err = {err}");
        // empty ABSOLUTE file → hard fail with an 'empty' error
        let err = resolve_prompt_template("inline body", &empty_file, &ws.path).unwrap_err();
        assert!(err.to_string().contains("empty"), "err = {err}");
    }

    // Mirrors Go `TestWorkerReadsRepoRelativePromptFile`: a repo-relative prompt_file is read from
    // this run's checkout (ws.path) and rendered as the turn-1 prompt (file wins over inline).
    #[tokio::test]
    async fn worker_reads_repo_relative_prompt_file() {
        let ag = fake_agent(vec![succeeded_turn()]);
        let tr = fake_tracker_by_id(&[("1", "MT-1", "Done")]);
        let (ws, root) = test_workspace(HookScripts::default());
        // The legacy (empty repo) path mkdir's <root>/<key>; seed the prompt file there.
        std::fs::create_dir_all(root.child("MT-1")).unwrap();
        std::fs::write(
            root.child("MT-1/PROMPT.md"),
            "from file {{ issue.identifier }}",
        )
        .unwrap();

        let mut d = make_deps(ws, ag.clone(), tr, "inline (should be ignored)", 20);
        d.prompt_file = "PROMPT.md".to_string();
        let (_last, _declared, err) =
            run_agent_attempt(&d, dispatched(), None, None, &noop_event(), None).await;
        assert!(err.is_none(), "expected normal exit, got {err:?}");
        assert_eq!(ag.last_prompt(), "from file MT-1", "file wins over inline");
    }

    // Mirrors Go `TestWorkerInstallsGraphiteGuard`: git_flow=graphite writes the guard hook +
    // settings.local.json into the checkout before spawn; "" / "any" write nothing (INF-251).
    #[tokio::test]
    async fn worker_installs_graphite_guard() {
        async fn run(git_flow: &str) -> TempDir {
            let ag = fake_agent(vec![succeeded_turn()]);
            let tr = fake_tracker_by_id(&[("1", "MT-1", "Done")]);
            let (ws, root) = test_workspace(HookScripts::default());
            let mut d = make_deps(ws, ag, tr, "inline", 20);
            d.git_flow = git_flow.to_string();
            let (_l, _h, err) =
                run_agent_attempt(&d, dispatched(), None, None, &noop_event(), None).await;
            assert!(err.is_none(), "expected normal exit, got {err:?}");
            root
        }

        let root = run("graphite").await;
        assert!(
            std::fs::metadata(
                std::path::Path::new(&root.child("MT-1")).join(".claude/hooks/gt-guard.sh")
            )
            .is_ok(),
            "gt-guard.sh not installed"
        );
        assert!(
            std::fs::metadata(
                std::path::Path::new(&root.child("MT-1")).join(".claude/settings.local.json")
            )
            .is_ok(),
            "settings.local.json not installed"
        );
        for gf in ["", "any"] {
            let root = run(gf).await;
            assert!(
                std::fs::metadata(std::path::Path::new(&root.child("MT-1")).join(".claude"))
                    .is_err(),
                "git_flow={gf:?} must not create .claude"
            );
        }
    }

    // Mirrors Go `TestWorkerMissingRelativePromptFileFallsBack`: a missing RELATIVE prompt_file
    // soft-falls-back to the inline prompt and the agent runs normally (INF-279).
    #[tokio::test]
    async fn worker_missing_relative_prompt_file_falls_back() {
        let ag = fake_agent(vec![succeeded_turn()]);
        let tr = fake_tracker_by_id(&[("1", "MT-1", "Done")]);
        let (ws, _root) = test_workspace(HookScripts::default());
        let mut d = make_deps(ws, ag.clone(), tr, "inline body {{ issue.identifier }}", 20);
        d.prompt_file = "does-not-exist.md".to_string();
        let (_l, _h, err) =
            run_agent_attempt(&d, dispatched(), None, None, &noop_event(), None).await;
        assert!(
            err.is_none(),
            "a missing relative prompt_file must soft-fall-back: {err:?}"
        );
        assert!(
            ag.start_calls() > 0,
            "agent must start on the soft fallback path"
        );
        assert_eq!(
            ag.last_prompt(),
            "inline body MT-1",
            "rendered inline fallback"
        );
    }

    // Mirrors Go `TestWorkerMissingAbsolutePromptFileFailsRun`: a missing ABSOLUTE prompt_file fails
    // the run before any agent work, with an error naming the path, and no session start (INF-279).
    #[tokio::test]
    async fn worker_missing_absolute_prompt_file_fails_run() {
        let ag = fake_agent(vec![succeeded_turn()]);
        let tr = fake_tracker_by_id(&[("1", "MT-1", "Done")]);
        let (ws, _root) = test_workspace(HookScripts::default());
        let host = TempDir::new();
        let mut d = make_deps(ws, ag.clone(), tr, "inline body", 20);
        d.prompt_file = host.child("absent-host-prompt.md");
        let (_l, _h, err) =
            run_agent_attempt(&d, dispatched(), None, None, &noop_event(), None).await;
        let err = err.expect("missing absolute prompt_file must fail the run");
        assert!(
            err.to_string().contains("absent-host-prompt.md"),
            "err = {err}"
        );
        assert_eq!(ag.start_calls(), 0, "agent must not start");
    }

    // Mirrors Go `TestWorkerWritesTranscript`: the transcript dir is created and the worker passes a
    // transcript to the agent.
    #[tokio::test]
    async fn worker_writes_transcript() {
        let ag = fake_agent(vec![agentfake::TurnScript {
            events: vec![Event {
                event_type: EVENT_SESSION_STARTED.to_string(),
                message: "thread-1".to_string(),
                ..Default::default()
            }],
            result: TurnResult {
                status: TURN_SUCCEEDED.to_string(),
                ..Default::default()
            },
            ..Default::default()
        }]);
        let tr = fake_tracker_by_id(&[("1", "MT-1", "Done")]);
        let (ws, _root) = test_workspace(HookScripts::default());
        let log_dir = TempDir::new();
        let mut d = make_deps(ws, ag.clone(), tr, "do it", 20);
        d.transcripts = Some(Arc::new(obslog::Store::new(log_dir.path.clone())));
        let (_l, _h, err) =
            run_agent_attempt(&d, dispatched(), None, None, &noop_event(), None).await;
        assert!(err.is_none(), "{err:?}");
        assert!(
            std::fs::metadata(log_dir.child("MT-1")).is_ok(),
            "transcript dir not created"
        );
        assert!(
            ag.last_transcript_present(),
            "worker should pass a transcript to the agent"
        );
    }

    // Mirrors Go `TestWorkerSuccessSingleTurn`.
    #[tokio::test]
    async fn worker_success_single_turn() {
        let ag = fake_agent(vec![succeeded_turn()]);
        // After turn 1 the issue is no longer active → loop stops.
        let tr = fake_tracker_by_id(&[("1", "MT-1", "Done")]);
        let (ws, _root) = test_workspace(HookScripts::default());
        let d = make_deps(ws, ag.clone(), tr, "do {{ issue.identifier }}", 20);
        let (_l, _h, err) =
            run_agent_attempt(&d, dispatched(), None, None, &noop_event(), None).await;
        assert!(err.is_none(), "expected normal exit, got {err:?}");
        assert_eq!(ag.start_calls(), 1);
    }

    // Mirrors Go `TestWorkerContinuesWhileActiveUpToMaxTurns`: three state refreshes (active, active,
    // then inactive after turn 3).
    #[tokio::test]
    async fn worker_continues_while_active_up_to_max_turns() {
        let ag = fake_agent(vec![succeeded_turn(), succeeded_turn(), succeeded_turn()]);
        let calls = Arc::new(AtomicUsize::new(0));
        let calls2 = Arc::clone(&calls);
        let mut tr = trackerfake::Fake::new();
        tr.states_by_ids_func = Some(Box::new(move |_ids: &[String]| {
            let n = calls2.fetch_add(1, Ordering::SeqCst) + 1;
            let state = if n >= 3 { "Done" } else { "In Progress" };
            Ok(vec![issue("1", "MT-1", state)])
        }));
        let tr = Arc::new(tr);
        let (ws, _root) = test_workspace(HookScripts::default());
        let d = make_deps(ws, ag, tr, "do it", 20);
        let (_l, _h, err) =
            run_agent_attempt(&d, dispatched(), None, None, &noop_event(), None).await;
        assert!(err.is_none(), "expected normal exit, got {err:?}");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "expected 3 state refreshes"
        );
    }

    // Mirrors Go `TestWorkerReturnsLastKnownState`: the worker propagates its last-known state.
    #[tokio::test]
    async fn worker_returns_last_known_state_non_active_flip() {
        let ag = fake_agent(vec![succeeded_turn()]);
        let mut tr = trackerfake::Fake::new();
        tr.states_by_ids_func = Some(Box::new(|_ids: &[String]| {
            Ok(vec![issue("1", "MT-1", "In Review")])
        }));
        let tr = Arc::new(tr);
        let (ws, _root) = test_workspace(HookScripts::default());
        let d = make_deps(ws, ag, tr, "do it", 20);
        let (last, _h, err) =
            run_agent_attempt(&d, dispatched(), None, None, &noop_event(), None).await;
        assert!(err.is_none(), "expected normal exit, got {err:?}");
        assert_eq!(last, "In Review");
    }

    // Mirrors Go `TestWorkerReturnsLastKnownState` (still-active at max turns).
    #[tokio::test]
    async fn worker_returns_last_known_state_active_at_max() {
        let ag = fake_agent(vec![succeeded_turn()]);
        let mut tr = trackerfake::Fake::new();
        tr.states_by_ids_func = Some(Box::new(|_ids: &[String]| {
            Ok(vec![issue("1", "MT-1", "In Progress")])
        }));
        let tr = Arc::new(tr);
        let (ws, _root) = test_workspace(HookScripts::default());
        let d = make_deps(ws, ag, tr, "do it", 1); // exhaust the turn budget after turn 1
        let (last, _h, err) =
            run_agent_attempt(&d, dispatched(), None, None, &noop_event(), None).await;
        assert!(err.is_none(), "expected normal exit, got {err:?}");
        assert_eq!(last, "In Progress");
    }

    // Mirrors Go `TestWorkerPropagatesHandoffDeclaration`: a trailing HANDOFF: line → declared true;
    // its absence → false (INF-272).
    #[tokio::test]
    async fn worker_propagates_handoff_declaration() {
        let cases: &[(&str, bool)] = &[
            ("Wrapped up the work.\nHANDOFF: in-review", true),
            ("done\n  HANDOFF: in-review  ", true),
            ("Moved the ticket to In Review and opened the PR.", false),
        ];
        for (result_text, want) in cases {
            let ag = fake_agent(vec![agentfake::TurnScript {
                result: TurnResult {
                    status: TURN_SUCCEEDED.to_string(),
                    result_text: (*result_text).to_string(),
                    ..Default::default()
                },
                ..Default::default()
            }]);
            let mut tr = trackerfake::Fake::new();
            // Park the ticket in a non-active state so the loop winds down and the declaration is
            // evaluated at exit.
            tr.states_by_ids_func = Some(Box::new(|_ids: &[String]| {
                Ok(vec![issue("1", "MT-1", "In Review")])
            }));
            let tr = Arc::new(tr);
            let (ws, _root) = test_workspace(HookScripts::default());
            let d = make_deps(ws, ag, tr, "do it", 20);
            let (_l, declared, err) =
                run_agent_attempt(&d, dispatched(), None, None, &noop_event(), None).await;
            assert!(err.is_none(), "expected normal exit, got {err:?}");
            assert_eq!(declared, *want, "result_text = {result_text:?}");
        }
    }

    // Mirrors Go `TestWorkerStopsAtMaxTurns`: always active → only max_turns stops it.
    #[tokio::test]
    async fn worker_stops_at_max_turns() {
        let ag = fake_agent(vec![succeeded_turn(), succeeded_turn()]);
        let mut tr = trackerfake::Fake::new();
        tr.states_by_ids_func = Some(Box::new(|_ids: &[String]| {
            Ok(vec![issue("1", "MT-1", "In Progress")])
        }));
        let tr = Arc::new(tr);
        let (ws, _root) = test_workspace(HookScripts::default());
        let d = make_deps(ws, ag, tr, "do it", 2);
        let (_l, _h, err) =
            run_agent_attempt(&d, dispatched(), None, None, &noop_event(), None).await;
        assert!(
            err.is_none(),
            "expected normal exit at max turns, got {err:?}"
        );
    }

    // Mirrors Go `TestWorkerBeforeRunFailureAborts`: a failing before_run hook aborts before the
    // agent starts.
    #[tokio::test]
    async fn worker_before_run_failure_aborts() {
        let ag = fake_agent(vec![]);
        let tr = Arc::new(trackerfake::Fake::new());
        let (ws, _root) = test_workspace(HookScripts {
            before_run: "exit 1".to_string(),
            ..Default::default()
        });
        let d = make_deps(ws, ag.clone(), tr, "do it", 20);
        let (_l, _h, err) =
            run_agent_attempt(&d, dispatched(), None, None, &noop_event(), None).await;
        assert!(err.is_some(), "before_run failure must abort the attempt");
        assert_eq!(
            ag.start_calls(),
            0,
            "agent must not start when before_run fails"
        );
    }

    // Mirrors Go `TestWorkerAgentStartFailure`.
    #[tokio::test]
    async fn worker_agent_start_failure() {
        let mut ag = agentfake::Fake::new();
        ag.start_err = Some(AgentError::Other("no agent".to_string()));
        let ag = Arc::new(ag);
        let tr = Arc::new(trackerfake::Fake::new());
        let (ws, _root) = test_workspace(HookScripts::default());
        let d = make_deps(ws, ag, tr, "do it", 20);
        let (_l, _h, err) =
            run_agent_attempt(&d, dispatched(), None, None, &noop_event(), None).await;
        assert!(err.is_some(), "agent start failure must error");
    }

    // Mirrors Go `TestWorkerTurnFailure`.
    #[tokio::test]
    async fn worker_turn_failure() {
        let ag = fake_agent(vec![agentfake::TurnScript {
            result: TurnResult {
                status: TURN_FAILED.to_string(),
                ..Default::default()
            },
            err: Some(AgentError::Other("turn boom".to_string())),
            ..Default::default()
        }]);
        let tr = fake_tracker_by_id(&[("1", "MT-1", "In Progress")]);
        let (ws, _root) = test_workspace(HookScripts::default());
        let d = make_deps(ws, ag, tr, "do it", 20);
        let (_l, _h, err) =
            run_agent_attempt(&d, dispatched(), None, None, &noop_event(), None).await;
        assert!(err.is_some(), "turn failure must error");
    }

    // Mirrors Go `TestWorkerPromptRenderFailure`: a strict-render failure errors, but the session was
    // already started (StartCalls == 1) since it starts before the first turn prompt is built.
    #[tokio::test]
    async fn worker_prompt_render_failure() {
        let ag = fake_agent(vec![succeeded_turn()]);
        let tr = Arc::new(trackerfake::Fake::new());
        let (ws, _root) = test_workspace(HookScripts::default());
        let d = make_deps(ws, ag.clone(), tr, "{{ unknown_var }}", 20); // strict render fails
        let (_l, _h, err) =
            run_agent_attempt(&d, dispatched(), None, None, &noop_event(), None).await;
        assert!(err.is_some(), "prompt render failure must error");
        assert_eq!(
            ag.start_calls(),
            1,
            "session starts before the first turn prompt is built"
        );
    }

    // Mirrors Go `TestWorkerForwardsEvents`: agent events are forwarded to on_event in order.
    #[tokio::test]
    async fn worker_forwards_events() {
        let ag = fake_agent(vec![agentfake::TurnScript {
            events: vec![
                Event {
                    event_type: EVENT_SESSION_STARTED.to_string(),
                    ..Default::default()
                },
                Event {
                    event_type: EVENT_NOTIFICATION.to_string(),
                    message: "x".to_string(),
                    ..Default::default()
                },
            ],
            result: TurnResult {
                status: TURN_SUCCEEDED.to_string(),
                ..Default::default()
            },
            ..Default::default()
        }]);
        let tr = fake_tracker_by_id(&[("1", "MT-1", "Done")]);
        let (ws, _root) = test_workspace(HookScripts::default());
        let d = make_deps(ws, ag, tr, "do it", 20);

        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen2 = Arc::clone(&seen);
        let on_event = move |e: Event| seen2.lock().unwrap().push(e.event_type);
        let (_l, _h, err) = run_agent_attempt(&d, dispatched(), None, None, &on_event, None).await;
        assert!(err.is_none(), "{err:?}");
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "events not forwarded: {seen:?}");
        assert_eq!(seen[0], EVENT_SESSION_STARTED);
    }

    // Mirrors Go `TestWorkerEmptyRepoURLUsesLegacyWorkspace`: RepoURL="" creates the plain per-issue
    // dir <root>/<key> and never a .mirrors bare-mirror store.
    #[tokio::test]
    async fn worker_empty_repo_url_uses_legacy_workspace() {
        let ag = fake_agent(vec![succeeded_turn()]);
        let tr = fake_tracker_by_id(&[("1", "MT-1", "Done")]);
        let (ws, root) = test_workspace(HookScripts::default());
        let d = make_deps(ws, ag, tr, "do it", 20);
        assert_eq!(d.repo_url, "", "precondition: RepoURL must be empty");
        let (_l, _h, err) =
            run_agent_attempt(&d, dispatched(), None, None, &noop_event(), None).await;
        assert!(err.is_none(), "expected normal exit, got {err:?}");
        // Plain per-issue dir was created by the legacy mkdir path.
        assert!(
            std::fs::metadata(root.child("MT-1"))
                .map(|m| m.is_dir())
                .unwrap_or(false),
            "legacy workspace dir <root>/MT-1 not created"
        );
        // No bare-mirror store.
        assert!(
            std::fs::metadata(root.child(".mirrors")).is_err(),
            "empty-RepoURL worker must NOT create a .mirrors store"
        );
    }

    // Mirrors Go `TestWorkerEmptyRefreshKeepsStateAndContinues`: an empty refresh keeps the last-known
    // (active) state and continues to the next turn.
    #[tokio::test]
    async fn worker_empty_refresh_keeps_state_and_continues() {
        let ag = fake_agent(vec![succeeded_turn(), succeeded_turn()]);
        let calls = Arc::new(AtomicUsize::new(0));
        let calls2 = Arc::clone(&calls);
        let mut tr = trackerfake::Fake::new();
        tr.states_by_ids_func = Some(Box::new(move |_ids: &[String]| {
            let n = calls2.fetch_add(1, Ordering::SeqCst) + 1;
            if n == 1 {
                Ok(Vec::new()) // empty refresh → state kept (still active)
            } else {
                Ok(vec![issue("1", "MT-1", "Done")]) // now inactive → stop
            }
        }));
        let tr = Arc::new(tr);
        let (ws, _root) = test_workspace(HookScripts::default());
        let d = make_deps(ws, ag, tr, "do it", 20);
        let (_l, _h, err) =
            run_agent_attempt(&d, dispatched(), None, None, &noop_event(), None).await;
        assert!(err.is_none(), "expected normal exit, got {err:?}");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "empty refresh should keep active and continue"
        );
    }
}
