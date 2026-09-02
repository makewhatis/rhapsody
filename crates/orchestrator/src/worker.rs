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
//!     `Model`, `DispatchSpanContext` — are dropped; the per-event `tracing::debug!` forwarding line
//!     is kept). The bounded metric labels live in [`crate::telemetry_attrs`]. `RunID` is NOT
//!     telemetry and IS carried: it reaches the session via `set_run_id` (STUDIO-675).
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
    /// Rendered capability-instruction text, prepended to the FIRST-turn prompt
    /// only, same as `stack_context`.
    pub capabilities_section: String,
    /// The Rhapsody Teams identity header + resolved profile text, prepended to the FIRST-turn
    /// prompt only, same as `capabilities_section` (STUDIO-643; design record
    /// `~/.rhapsody/docs/STUDIO-572-rhapsody-teams.md`, §2.4 row 5). Empty whenever Teams is off,
    /// nobody was routed, or the profile failed to resolve — and empty means the guard in
    /// [`build_turn_prompt`] skips it, leaving the prompt byte-identical.
    pub teammate_section: String,
    /// The GitHub label name the post-run labeler adds to every PR in this run's stack (AIE-301).
    pub pr_label: String,
    /// The store run-row id for this attempt (Go `WorkerDeps.RunID`), threaded onto the session so
    /// the agent child's env carries `SYMPHONY_RUN_ID` (STUDIO-675). `0` when the store is off or
    /// `start_run` failed, which [`Session::set_run_id`] treats as "unknown" and emits nothing for.
    ///
    /// This is what lets a dispatched teammate call `teams_post` / `teams_retain` at all: both
    /// resolve the calling run from that env alone, and the daemon resolves run → identity from the
    /// dispatch-time binding.
    pub run_id: i64,
    /// The review state a declared HANDOFF parks the ticket in (a review-state name; `None` ⇒ the
    /// feature is off, giving Go-identical behavior). Dispatched agents cannot move Linear state
    /// themselves — the runner injects the mcp_config with `--strict-mcp-config`, so only the
    /// `symphony` server is present, not a Linear-write server — so on a HANDOFF the daemon parks
    /// the ticket here on the agent's behalf and ends the turn loop. Post-parity divergence from Go
    /// (TRA-240): Go relied on the agent/merge moving the ticket, which the review-gated flow can't.
    pub review_handoff_state: Option<String>,
    /// Review-mode provisioning (STUDIO-715; design record
    /// `~/.rhapsody/docs/STUDIO-703-ticketless-pr-review.md`). `Some` makes this run a ticketless PR
    /// review: the workspace is a DETACHED worktree pinned to the carried head SHA rather than a
    /// fresh `symphony/<key>` branch, and the agent is told which SHA it is reading.
    ///
    /// `None` for every ticket dispatch — and, in this slice, for every dispatch, since nothing
    /// triggers a review yet.
    pub review: Option<crate::review::ReviewCheckout>,
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
                // TRA-238: the repo-relative prompt defaults moved to `.rhapsody/` (rebrand). A repo
                // that still ships the pre-rebrand `.symphony/PROMPT.md` (+ `.symphony/PROMPT.dep_mod.md`)
                // keeps resolving its prompt: when a `.rhapsody/…` prompt is absent from the checkout,
                // retry the SAME filename under the legacy `.symphony/` directory before soft-falling-back
                // to the inline prompt. A non-empty legacy file WINS; any other legacy outcome (absent,
                // empty, or unreadable) leaves the original inline soft-fallback intact — the fallback can
                // only upgrade a would-be inline run to the legacy repo prompt, never add a failure mode.
                if let Some(rest) = prompt_file.strip_prefix(".rhapsody/") {
                    let legacy = std::path::Path::new(ws_path).join(".symphony").join(rest);
                    if let Ok(b) = std::fs::read(&legacy) {
                        let text = String::from_utf8_lossy(&b);
                        if !text.trim().is_empty() {
                            return Ok((text.into_owned(), String::new()));
                        }
                    }
                }
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
/// (upstream §7.1, §12.3). Non-empty `capabilities_section` (BO-12), `teammate_section` (STUDIO-643)
/// and `stack_context` (INF-318) are prepended (as plain text, in that order) to the FIRST-turn
/// prompt only. Mirrors Go `buildTurnPrompt`.
///
/// The teammate section sits immediately after capabilities because §0.11.6 fixes that relative
/// order (capabilities → teammate header → room catch-up → memory recall); the later two tenants and
/// the composer that owns the total byte budget are T5's. **Every section is guarded by
/// `if !x.is_empty()`**, so a daemon with Teams off produces a byte-identical prompt to one built
/// before Teams existed — that guard IS the inertness proof (§2.4 row 5).
pub(crate) fn build_turn_prompt(
    tmpl: &str,
    capabilities_section: &str,
    teammate_section: &str,
    stack_context: &str,
    iss: &Issue,
    attempt: Option<i32>,
    turn: i64,
) -> Result<String, rhapsody_config::prompt::RenderError> {
    if turn <= 1 {
        let rendered = rhapsody_config::prompt::render(tmpl, iss, attempt)?;
        let mut out = String::new();
        if !capabilities_section.is_empty() {
            out.push_str(capabilities_section);
            out.push_str("\n\n");
        }
        if !teammate_section.is_empty() {
            out.push_str(teammate_section);
            out.push_str("\n\n");
        }
        if !stack_context.is_empty() {
            out.push_str(stack_context);
            out.push_str("\n\n");
        }
        out.push_str(&rendered);
        return Ok(out);
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
    // Review mode provisions a DETACHED worktree at the head SHA pinned at dispatch: a review reads
    // one pull request's commit and creates no branch to push (STUDIO-715). Checked first because it
    // overrides `workspace_mode` — a review is never a `symphony/<key>` checkout in either shape.
    // workspace_mode:clone provisions an independent clone (no cross-ticket checkout lock); anything
    // else uses the shared-mirror worktree path. All three run the same downstream pipeline.
    let ws = if let Some(rev) = &deps.review {
        deps.workspace
            .ensure_review_worktree(
                &deps.repo_url,
                &deps.project_slug,
                &issue.identifier,
                rev.pr_number,
                &rev.head_sha,
            )
            .await
    } else if deps.workspace_mode == WORKSPACE_MODE_CLONE {
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
    // Thread the store run id onto the session (Go: the optional-interface `SetRunID` right after
    // `StartSession`, before the first turn) so the agent child's env carries SYMPHONY_RUN_ID for
    // the injected MCP server's "me" default. A zero id is a no-op inside the setter's consumer.
    sess.set_run_id(deps.run_id);
    // Pin the reviewed head into the agent's env (STUDIO-715, F-SHA): the SHA the worktree above was
    // detached at, so the agent reports on the commit it is actually reading. A no-op for every
    // non-review run, which sets nothing and emits nothing.
    if let Some(rev) = &deps.review {
        sess.set_review_head(&rev.head_sha);
    }

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
            let p = match build_turn_prompt(
                prompt_tmpl,
                &self.capabilities_section,
                &self.teammate_section,
                &self.stack_context,
                &issue,
                attempt,
                turn,
            ) {
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
            // Review-mode wind-down (STUDIO-716; design record
            // `~/.rhapsody/docs/STUDIO-703-ticketless-pr-review.md` §14.2, "wind-down: team_id is a
            // red herring"). A review run's `pr:` key resolves to no tracker issue, so BOTH of the
            // loop-ending mechanisms below are wrong for it: the auto-park's `move_issue_state`
            // would be a guaranteed 404 on every review, and the per-turn refresh returns EMPTY,
            // which keeps the synthetic state exactly as it was and lets the loop spin fresh turns
            // until the whole budget is gone. The agent's OWN hand-off declaration is therefore the
            // only completion signal a review has, and when it arrives there is nothing to move.
            //
            // `max_turns` stays the backstop for an agent that never declares — bounded, and the
            // exit classifier records a review run completed rather than scheduling the
            // continuation retry that would re-dispatch it forever (see `retry::on_review_exit`).
            //
            // `review` unset ⇒ this whole block is inert, i.e. byte-identical to a daemon built
            // before review mode.
            if self.review.is_some() {
                if has_handoff_marker(&last_result) || turn >= self.max_turns {
                    return (issue.state.clone(), last_result, None);
                }
                turn += 1;
                continue;
            }
            // Handoff auto-park (TRA-240 loop fix): when the agent declares a HANDOFF but the ticket
            // is still active, the daemon moves it to the configured review state on the agent's
            // behalf (dispatched agents have no Linear-write MCP) and ENDS the loop here — otherwise
            // the ticket never leaves the active set and the loop spins fresh turns until max_turns.
            // `None` review_handoff_state ⇒ this whole block is inert, i.e. Go-identical.
            if let Some(state) = self.review_handoff_state.as_deref()
                && has_handoff_marker(&last_result)
                && self.active_states.contains(&normalize_state(&issue.state))
                && !issue.team_id.is_empty()
            {
                match self
                    .tracker
                    .move_issue_state(&issue.id, &issue.team_id, state)
                    .await
                {
                    Ok(()) => issue.state = state.to_string(),
                    Err(e) => tracing::warn!(
                        issue_identifier = %issue.identifier,
                        error = %e,
                        "handoff auto-park: could not move the ticket to the review state; \
                         ending the run anyway (a re-dispatch will retry the park)"
                    ),
                }
                // End the loop regardless of the move outcome: the agent declared it is done, so
                // further turns are pointless. On a successful move the ticket is now non-active
                // (caller classifies completed, no re-dispatch); on failure the run still ends
                // cleanly instead of burning the whole turn budget.
                return (issue.state.clone(), last_result, None);
            }
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
    use crate::testsupport::{TempDir, issue, recording_subscriber};

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
            capabilities_section: String::new(),
            teammate_section: String::new(),
            pr_label: String::new(),
            review_handoff_state: None,
            review: None,
            run_id: 0,
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
        let first = build_turn_prompt("Work {{ issue.identifier }}", "", "", "", &iss, None, 1)
            .expect("render");
        assert_eq!(first, "Work MT-1");
        let cont = build_turn_prompt("Work {{ issue.identifier }}", "", "", "", &iss, None, 2)
            .expect("render");
        assert_eq!(cont, CONTINUATION_GUIDANCE);
    }

    // Mirrors Go `TestBuildTurnPromptStackContext`: the stack context prepends the first-turn prompt
    // only; continuation turns are unchanged (INF-318).
    #[test]
    fn build_turn_prompt_stack_context() {
        let iss = issue("", "MT-2", "Todo");
        let stack = "STACK ON: feat/mt-1 (PR #7) — create your branch stacked on this predecessor.";
        let first = build_turn_prompt("Work {{ issue.identifier }}", "", "", stack, &iss, None, 1)
            .expect("render");
        assert_eq!(first, format!("{stack}\n\nWork MT-2"));
        let cont = build_turn_prompt("Work {{ issue.identifier }}", "", "", stack, &iss, None, 2)
            .expect("render");
        assert_eq!(cont, CONTINUATION_GUIDANCE);
    }

    // The capability section prepends the first-turn prompt only; continuation turns are unchanged.
    #[test]
    fn build_turn_prompt_capabilities_section() {
        let iss = issue("", "MT-3", "Todo");
        let caps = "## Required practices for this ticket\n\nReview your own diff.";
        let first = build_turn_prompt("Work {{ issue.identifier }}", caps, "", "", &iss, None, 1)
            .expect("render");
        assert_eq!(first, format!("{caps}\n\nWork MT-3"));
        let cont = build_turn_prompt("Work {{ issue.identifier }}", caps, "", "", &iss, None, 2)
            .expect("render");
        assert_eq!(cont, CONTINUATION_GUIDANCE);
    }

    // Capabilities render FIRST, then the stack context, then the rendered prompt (BO-12).
    #[test]
    fn build_turn_prompt_capabilities_and_stack_context_both_present() {
        let iss = issue("", "MT-4", "Todo");
        let caps = "## Required practices for this ticket\n\nReview your own diff.";
        let stack = "STACK ON: feat/mt-1 (PR #7) — create your branch stacked on this predecessor.";
        let first = build_turn_prompt(
            "Work {{ issue.identifier }}",
            caps,
            "",
            stack,
            &iss,
            None,
            1,
        )
        .expect("render");
        assert_eq!(first, format!("{caps}\n\n{stack}\n\nWork MT-4"));
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

    /// STUDIO-675: the worker MUST thread the store run id onto the session (Go's `SetRunID` right
    /// after `StartSession`), because the Claude backend turns it into the agent child's
    /// `SYMPHONY_RUN_ID` — the only thing `teams_post` / `teams_retain` resolve the calling run
    /// from. Without this the tools fail with "SYMPHONY_RUN_ID is not set" on every dispatch.
    #[tokio::test]
    async fn worker_threads_run_id_onto_the_session() {
        let ag = fake_agent(vec![succeeded_turn()]);
        let tr = fake_tracker_by_id(&[("1", "MT-1", "Done")]);
        let (ws, _root) = test_workspace(HookScripts::default());
        let mut d = make_deps(ws, ag.clone(), tr, "p", 20);
        d.run_id = 412;
        let (_last, _declared, err) =
            run_agent_attempt(&d, dispatched(), None, None, &noop_event(), None).await;
        assert!(err.is_none(), "expected normal exit, got {err:?}");
        assert_eq!(
            ag.last_run_id(),
            Some(412),
            "the dispatch run id must reach the session"
        );
    }

    /// STUDIO-715: `deps.review` makes the worker take the review provisioning path — a DETACHED
    /// worktree at the pinned head, no `symphony/<key>` branch — and hand that same SHA to the agent
    /// as its review head. Uses a real local origin, because the branch-vs-detached distinction only
    /// exists in git.
    #[tokio::test]
    async fn worker_provisions_a_detached_review_worktree_and_pins_the_head() {
        fn git_run(dir: &str, args: &[&str]) {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .expect("run git");
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        let origin = TempDir::new();
        git_run(&origin.path, &["init", "-b", "main"]);
        std::fs::write(origin.child("README.md"), "hello\n").expect("write README");
        git_run(&origin.path, &["add", "README.md"]);
        git_run(&origin.path, &["commit", "-m", "initial"]);
        git_run(&origin.path, &["commit", "--allow-empty", "-m", "pr head"]);
        let out = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&origin.path)
            .output()
            .expect("rev-parse");
        let head = String::from_utf8_lossy(&out.stdout).trim().to_string();
        git_run(&origin.path, &["update-ref", "refs/pull/12/head", &head]);

        let ag = fake_agent(vec![succeeded_turn()]);
        let tr = fake_tracker_by_id(&[("pr:o/r#12@alice", "pr:o/r#12@alice", "Done")]);
        let (ws, root) = test_workspace(HookScripts::default());
        let mut d = make_deps(Arc::clone(&ws), ag.clone(), tr, "p", 20);
        d.repo_url = origin.path.clone();
        d.project_slug = "rhapsody".to_string();
        d.review = Some(crate::review::ReviewCheckout {
            pr_number: 12,
            head_sha: head.clone(),
        });
        let iss = issue("pr:o/r#12@alice", "pr:o/r#12@alice", "In Progress");

        let (_last, _declared, err) =
            run_agent_attempt(&d, iss, None, None, &noop_event(), None).await;
        assert!(err.is_none(), "expected normal exit, got {err:?}");

        assert_eq!(
            ag.last_review_head(),
            Some(head.clone()),
            "the pinned head must reach the agent as its review head"
        );
        let path = ws.path_for(&origin.path, "pr:o/r#12@alice");
        assert!(
            std::fs::metadata(&path).is_ok(),
            "no review worktree at {path}"
        );
        let out = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&path)
            .output()
            .expect("rev-parse worktree");
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            head,
            "the review worktree is not at the pinned head"
        );
        let out = std::process::Command::new("git")
            .args(["symbolic-ref", "--quiet", "HEAD"])
            .current_dir(&path)
            .output()
            .expect("symbolic-ref");
        assert!(
            String::from_utf8_lossy(&out.stdout).trim().is_empty(),
            "the review worktree is on a branch, not detached"
        );
        drop(root);
    }

    /// The inertness half: with `review` unset — every run this slice ships — the worker takes the
    /// existing branch-based path and tells the agent no review head at all.
    #[tokio::test]
    async fn worker_without_review_takes_the_branch_path_and_pins_no_head() {
        let ag = fake_agent(vec![succeeded_turn()]);
        let tr = fake_tracker_by_id(&[("1", "MT-1", "Done")]);
        let (ws, _root) = test_workspace(HookScripts::default());
        let d = make_deps(ws, ag.clone(), tr, "p", 20);
        let (_last, _declared, err) =
            run_agent_attempt(&d, dispatched(), None, None, &noop_event(), None).await;
        assert!(err.is_none(), "expected normal exit, got {err:?}");
        assert_eq!(
            ag.last_review_head(),
            None,
            "a non-review run must not pin a review head"
        );
    }

    /// A store-disabled run (`run_id` 0) still calls the setter — the zero guard lives in the
    /// backend, which emits no env for it — so behaviour is unchanged when there is no run row.
    #[tokio::test]
    async fn worker_threads_zero_run_id_when_the_store_is_off() {
        let ag = fake_agent(vec![succeeded_turn()]);
        let tr = fake_tracker_by_id(&[("1", "MT-1", "Done")]);
        let (ws, _root) = test_workspace(HookScripts::default());
        let d = make_deps(ws, ag.clone(), tr, "p", 20);
        let (_last, _declared, err) =
            run_agent_attempt(&d, dispatched(), None, None, &noop_event(), None).await;
        assert!(err.is_none(), "expected normal exit, got {err:?}");
        assert_eq!(ag.last_run_id(), Some(0), "a zero id is still threaded");
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

    // TRA-238: the repo-relative prompt defaults moved to `.rhapsody/`. A repo that still ships the
    // pre-rebrand `.symphony/PROMPT.md` keeps resolving its prompt — the resolver retries the legacy
    // `.symphony/` counterpart when the `.rhapsody/` path is absent from the checkout. This is the
    // ticket's fallback acceptance (a `.symphony/PROMPT.md`-only repo still resolves).
    #[test]
    fn rhapsody_prompt_falls_back_to_legacy_symphony() {
        let td = TempDir::new();
        let ws = std::path::Path::new(&td.path);
        std::fs::create_dir_all(ws.join(".symphony")).unwrap();
        std::fs::write(
            ws.join(".symphony/PROMPT.md"),
            "legacy repo prompt {{ issue.identifier }}",
        )
        .unwrap();
        // Only the legacy `.symphony/PROMPT.md` exists — no `.rhapsody/PROMPT.md`.
        let (tmpl, warn) =
            resolve_prompt_template("inline body", ".rhapsody/PROMPT.md", &td.path).unwrap();
        assert_eq!(
            tmpl, "legacy repo prompt {{ issue.identifier }}",
            "the legacy .symphony/PROMPT.md must win over the inline prompt"
        );
        assert!(
            warn.is_empty(),
            "a resolved fallback is not a warning: {warn:?}"
        );
    }

    // The dep-mode prompt (`.rhapsody/PROMPT.dep_mod.md`) flows through the SAME resolver, so it
    // falls back to `.symphony/PROMPT.dep_mod.md` the same way.
    #[test]
    fn rhapsody_dep_mode_prompt_falls_back_to_legacy_symphony() {
        let td = TempDir::new();
        let ws = std::path::Path::new(&td.path);
        std::fs::create_dir_all(ws.join(".symphony")).unwrap();
        std::fs::write(
            ws.join(".symphony/PROMPT.dep_mod.md"),
            "legacy dep-mode prompt",
        )
        .unwrap();
        let (tmpl, warn) =
            resolve_prompt_template("inline", ".rhapsody/PROMPT.dep_mod.md", &td.path).unwrap();
        assert_eq!(tmpl, "legacy dep-mode prompt");
        assert!(warn.is_empty());
    }

    // When BOTH the new and legacy files exist, the new `.rhapsody/` path WINS (the fallback is only
    // consulted when the new path is absent).
    #[test]
    fn rhapsody_prompt_prefers_new_path_over_legacy() {
        let td = TempDir::new();
        let ws = std::path::Path::new(&td.path);
        std::fs::create_dir_all(ws.join(".rhapsody")).unwrap();
        std::fs::create_dir_all(ws.join(".symphony")).unwrap();
        std::fs::write(ws.join(".rhapsody/PROMPT.md"), "new rhapsody prompt").unwrap();
        std::fs::write(ws.join(".symphony/PROMPT.md"), "legacy prompt").unwrap();
        let (tmpl, warn) =
            resolve_prompt_template("inline", ".rhapsody/PROMPT.md", &td.path).unwrap();
        assert_eq!(
            tmpl, "new rhapsody prompt",
            "the new .rhapsody path wins; the legacy fallback is not consulted"
        );
        assert!(warn.is_empty());
    }

    // Neither the new nor the legacy path exists → the original inline soft-fallback, warn naming the
    // configured (new) path. Proves the fallback adds no new failure mode.
    #[test]
    fn rhapsody_prompt_absent_and_no_legacy_soft_falls_back() {
        let td = TempDir::new();
        let (tmpl, warn) = resolve_prompt_template(
            "inline body {{ issue.identifier }}",
            ".rhapsody/PROMPT.md",
            &td.path,
        )
        .unwrap();
        assert_eq!(
            tmpl, "inline body {{ issue.identifier }}",
            "no repo prompt → inline"
        );
        assert!(
            warn.contains(".rhapsody/PROMPT.md"),
            "warn must name the configured (new) path: {warn:?}"
        );
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

    // TRA-240: with review_handoff_state set, a HANDOFF on a still-active ticket makes the daemon
    // park it in the review state and END the loop on that turn — dispatched agents can't move
    // Linear state, so without this the loop spins fresh turns until max_turns.
    #[tokio::test]
    async fn handoff_parks_ticket_and_ends_loop() {
        let handoff = agentfake::TurnScript {
            result: TurnResult {
                status: TURN_SUCCEEDED.to_string(),
                result_text: "all done\nHANDOFF: in-review".to_string(),
                ..Default::default()
            },
            ..Default::default()
        };
        // Extra turns queued so a regression (no auto-park) would keep looping instead of stopping.
        let ag = fake_agent(vec![handoff.clone(), handoff.clone(), handoff]);
        let refreshes = Arc::new(AtomicUsize::new(0));
        let r2 = Arc::clone(&refreshes);
        let mut tr = trackerfake::Fake::new();
        tr.states_by_ids_func = Some(Box::new(move |_ids: &[String]| {
            r2.fetch_add(1, Ordering::SeqCst);
            Ok(vec![issue("1", "MT-1", "In Progress")]) // never leaves active on its own
        }));
        let tr = Arc::new(tr);
        let (ws, _root) = test_workspace(HookScripts::default());
        let mut d = make_deps(ws, ag, tr.clone() as Arc<dyn Tracker>, "do it", 20);
        d.review_handoff_state = Some("in review".to_string());
        // The dispatched issue must carry a team_id — Linear state resolution is team-scoped.
        let iss = Issue {
            team_id: "team-1".to_string(),
            ..issue("1", "MT-1", "In Progress")
        };
        let (last, handed_off, err) =
            run_agent_attempt(&d, iss, None, None, &noop_event(), None).await;
        assert!(err.is_none(), "expected normal exit, got {err:?}");
        assert!(handed_off, "handoff marker should be detected");
        assert_eq!(
            refreshes.load(Ordering::SeqCst),
            0,
            "loop must end on the handoff turn, before any state refresh (no max_turns spin)"
        );
        let moves = tr.move_calls();
        assert_eq!(moves.len(), 1, "exactly one park move");
        assert_eq!(moves[0].issue_id, "1");
        assert_eq!(moves[0].team_id, "team-1");
        assert_eq!(moves[0].state_name, "in review");
        assert_eq!(last, "in review", "returned state reflects the park");
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

    // TRA-242 e2e (fake run): after the daemon-mediated `symphony_handoff` moves the run's ticket to
    // the configured review state, the worker's per-turn state refresh sees it leave the active set and
    // winds the turn loop down on the FIRST turn — even with a generous `max_turns` budget (NO
    // max_turns spin). The agent still declares HANDOFF, so the clean exit classifies `completed` (see
    // `retry::classify_clean_exit`). The tracker returning "In Review" stands in for the persisted
    // daemon move (the move itself is proven end-to-end in `orchestrator::handoff` + the file-tracker
    // MoveIssueState suite; the tool/endpoint wiring in `rhapsody-mcp` / `rhapsody-httpapi`).
    #[tokio::test]
    async fn handoff_review_state_ends_turn_loop_first_turn_no_max_turns_spin() {
        let refreshes = Arc::new(AtomicUsize::new(0));
        let r2 = Arc::clone(&refreshes);
        let mut tr = trackerfake::Fake::new();
        tr.states_by_ids_func = Some(Box::new(move |_ids: &[String]| {
            r2.fetch_add(1, Ordering::SeqCst);
            Ok(vec![issue("1", "MT-1", "In Review")]) // the state symphony_handoff moved the ticket to
        }));
        let tr = Arc::new(tr);
        // ONE declaring-handoff turn is enough: if the loop wrongly spun to a 2nd turn the fake agent
        // would run out of scripted turns and the test would fail loudly.
        let ag = fake_agent(vec![agentfake::TurnScript {
            result: TurnResult {
                status: TURN_SUCCEEDED.to_string(),
                result_text: "wrapped up the work\nHANDOFF: in-review".to_string(),
                ..Default::default()
            },
            ..Default::default()
        }]);
        let (ws, _root) = test_workspace(HookScripts::default());
        let d = make_deps(ws, ag.clone(), tr, "do it", 20); // generous budget — only the handoff ends it
        let (last, declared, err) =
            run_agent_attempt(&d, dispatched(), None, None, &noop_event(), None).await;
        assert!(err.is_none(), "expected a clean exit, got {err:?}");
        assert_eq!(
            refreshes.load(Ordering::SeqCst),
            1,
            "exactly one turn ran — the review-state handoff ended the loop on turn 1, no max_turns spin"
        );
        assert_eq!(ag.start_calls(), 1, "worker starts exactly one session");
        assert_eq!(
            last, "In Review",
            "worker's last-known state is the review handoff state"
        );
        assert!(
            declared,
            "the agent declared HANDOFF, so the clean exit classifies completed"
        );
    }

    // STUDIO-716 (design record §14.2, "wind-down: team_id is a red herring"): a review run's
    // `pr:` key resolves to no tracker issue, so the per-turn refresh returns EMPTY and the
    // auto-park's `move_issue_state` would be a guaranteed 404. The agent's OWN hand-off
    // declaration is the only completion signal it has, and the tracker is never consulted.
    //
    // The declaring turn is the THIRD of a 20-turn budget on purpose: an implementation that
    // capped a review at one turn, and one that only ever wound down at `max_turns`, both fail
    // this assertion.
    #[tokio::test]
    async fn review_run_winds_down_on_the_agents_own_declaration() {
        let plain = |text: &str| agentfake::TurnScript {
            result: TurnResult {
                status: TURN_SUCCEEDED.to_string(),
                result_text: text.to_string(),
                ..Default::default()
            },
            ..Default::default()
        };
        let ag = fake_agent(vec![
            plain("read the diff"),
            plain("still reading"),
            plain("posted 2 findings\nHANDOFF: review-posted"),
        ]);
        // Every state this tracker could report is a fiction for a `pr:` key; a review run must not
        // ask it anything at all.
        let tr = fake_tracker_by_id(&[("1", "MT-1", "In Progress")]);
        let (ws, _root) = test_workspace(HookScripts::default());
        let mut d = make_deps(
            ws,
            ag.clone(),
            Arc::clone(&tr) as Arc<dyn Tracker>,
            "review it",
            20,
        );
        d.review_handoff_state = Some("in review".to_string()); // configured, and still not used
        d.review = Some(crate::review::ReviewCheckout {
            pr_number: 12,
            head_sha: "a".repeat(40),
        });
        let sess = ag
            .start_session("", review_issue(), None)
            .await
            .expect("session");
        let (last, result, err) = d
            .run_turns(
                sess.as_ref(),
                "review it",
                review_issue(),
                None,
                None,
                &noop_event(),
            )
            .await;
        assert!(err.is_none(), "expected a clean exit, got {err:?}");
        assert_eq!(
            sess.id(),
            "thread-fake-3",
            "the loop ends on the declaring turn — not at 1, not at max_turns"
        );
        assert_eq!(
            tr.by_id_calls(),
            0,
            "a review run must not refresh a Linear state"
        );
        assert!(
            tr.move_calls().is_empty(),
            "a review run must not move a `pr:` key (a guaranteed 404)"
        );
        assert!(
            has_handoff_marker(&result),
            "the declaring turn's text is the freshest result"
        );
        assert_eq!(last, "", "a synthetic review issue carries no state");
    }

    // The inertness half of STUDIO-716: with `review` unset the loop is byte-identical — the
    // hand-off auto-park still moves the ticket and the per-turn refresh still runs.
    #[tokio::test]
    async fn non_review_run_still_auto_parks_and_refreshes() {
        let ag = fake_agent(vec![agentfake::TurnScript {
            result: TurnResult {
                status: TURN_SUCCEEDED.to_string(),
                result_text: "done\nHANDOFF: in-review".to_string(),
                ..Default::default()
            },
            ..Default::default()
        }]);
        let tr = fake_tracker_by_id(&[("1", "MT-1", "In Progress")]);
        let (ws, _root) = test_workspace(HookScripts::default());
        let mut d = make_deps(
            ws,
            ag.clone(),
            Arc::clone(&tr) as Arc<dyn Tracker>,
            "do it",
            20,
        );
        d.review_handoff_state = Some("in review".to_string());
        // The dispatched issue must carry a team_id — Linear state resolution is team-scoped.
        let iss = Issue {
            team_id: "team-1".to_string(),
            ..dispatched()
        };
        let sess = ag
            .start_session("", iss.clone(), None)
            .await
            .expect("session");
        let (last, _result, err) = d
            .run_turns(sess.as_ref(), "do it", iss, None, None, &noop_event())
            .await;
        assert!(err.is_none(), "expected a clean exit, got {err:?}");
        assert_eq!(
            tr.move_calls().len(),
            1,
            "the auto-park still fires for a ticket run"
        );
        assert_eq!(last, "in review", "returned state reflects the park");
    }

    /// The synthetic issue a review run is dispatched with: a `pr:` key and NO state (STUDIO-715).
    fn review_issue() -> Issue {
        Issue {
            id: "pr:makewhat/rhapsody#12@alice".to_string(),
            identifier: "pr:makewhat/rhapsody#12@alice".to_string(),
            title: "Review makewhat/rhapsody#12".to_string(),
            ..Issue::default()
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

    // --- log_correlation_test.go: worker-path log correlation (O8 e2e) ---------------------------

    /// A fake scripting one turn that emits a single `notification` agent event then succeeds — the
    /// analogue of Go log_correlation_test.go's `agentfake` script.
    fn notify_then_succeed() -> Arc<agentfake::Fake> {
        fake_agent(vec![agentfake::TurnScript {
            events: vec![Event {
                event_type: EVENT_NOTIFICATION.to_string(),
                message: "thinking".to_string(),
                ..Default::default()
            }],
            result: TurnResult {
                status: TURN_SUCCEEDED.to_string(),
                ..Default::default()
            },
            err: None,
        }])
    }

    // The assertable-now slice of Go log_correlation_test.go `TestWorkerLogsCarryTraceContext`: a
    // worker-path agent event is logged with the STANDARDIZED `issue_identifier` key. (The Go test
    // additionally asserts the record carries a valid OTel trace context — the otelslog bridge attaches
    // trace_id/span_id from the run/turn span — which is telemetry P6; this port emits no OTel span
    // around the agent-event log yet, per the worker module docs. That half is the ignored full mirror
    // below, matching how the loop's `loop_spans_test` is split into an active slice + a P6 mirror.)
    #[tokio::test]
    async fn worker_logs_carry_issue_identifier() {
        let _serial = crate::testsupport::TRACING_TEST_LOCK.lock().await; // TRA-243
        let (events, subscriber) = recording_subscriber();
        let guard = tracing::subscriber::set_default(subscriber);

        // TRA-243: the agent-event log callsite is shared with other worker tests that run agents
        // WITHOUT a subscriber; in tracing-core the first thread to *register* a callsite pins its
        // cached Interest (see `control_loop_emits_poll_and_fetch_spans` for the mechanism). A parallel
        // no-subscriber test can cache it `Interest::never`, so this recording subscriber never sees the
        // event → empty capture (flaky). Warm up one throwaway attempt to force the callsite to
        // register, rebuild the interest cache against THIS thread's subscriber (RecordingLayer →
        // always), then capture a fresh attempt. Fresh deps each pass — the scripted agent fake is
        // single-shot.
        {
            let ag = notify_then_succeed();
            let tr = fake_tracker_by_id(&[("1", "MT-1", "Done")]);
            let (ws, _root) = test_workspace(HookScripts::default());
            let warm = make_deps(ws, ag, tr, "do it", 20);
            let _ = run_agent_attempt(
                &warm,
                issue("1", "MT-1", "In Progress"),
                None,
                None,
                &noop_event(),
                None,
            )
            .await;
        }
        tracing::callsite::rebuild_interest_cache();
        events.lock().expect("event buffer lock").clear();

        let ag = notify_then_succeed();
        let tr = fake_tracker_by_id(&[("1", "MT-1", "Done")]); // leaves the active set after turn 1
        let (ws, _root) = test_workspace(HookScripts::default());
        let d = make_deps(ws, ag, tr, "do it", 20);
        let (_last, _declared, err) = run_agent_attempt(
            &d,
            issue("1", "MT-1", "In Progress"),
            None,
            None,
            &noop_event(),
            None,
        )
        .await;
        drop(guard);
        assert!(err.is_none(), "expected a normal run, got {err:?}");

        let captured = events.lock().expect("event buffer lock");
        let agent_ev = captured
            .iter()
            .find(|e| e.fields.get("event").map(String::as_str) == Some(EVENT_NOTIFICATION))
            .expect("expected an agent-event log for the emitted notification");
        assert_eq!(
            agent_ev.fields.get("issue_identifier").map(String::as_str),
            Some("MT-1"),
            "agent-event log must carry the standardized issue_identifier key: {agent_ev:?}"
        );
    }

    // The full Go `TestWorkerLogsCarryTraceContext`: a worker-path agent event must be emitted UNDER
    // the run/turn span so the otelslog bridge can attach a valid trace context. That span + the OTel
    // export are telemetry P6 (O3 deferred them; the worker wraps no span around the agent-event log
    // yet — see the worker module docs), so the tracing-level analogue "emitted under a span" fails
    // today. Un-ignored when P6 wires the run/turn span + the OTel bridge.
    #[tokio::test]
    #[ignore = "telemetry P6: OTel trace-context on worker logs (run/turn span + otelslog bridge; O3 deferred it)"]
    async fn worker_logs_carry_trace_context() {
        use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
        use tracing_subscriber::registry::LookupSpan;

        // Records, per event carrying an `event` field, that field's value + whether the event was
        // emitted under a span (the tracing-level analogue of "the record's ctx carries a valid
        // trace_id"). Reuses no crate state so the ignored mirror stays self-contained.
        struct SpanCtxLayer {
            hits: Arc<Mutex<Vec<(String, bool)>>>,
        }
        impl<S> Layer<S> for SpanCtxLayer
        where
            S: tracing::Subscriber + for<'a> LookupSpan<'a>,
        {
            fn on_event(&self, event: &tracing::Event<'_>, ctx: Context<'_, S>) {
                struct EvField(Option<String>);
                impl tracing::field::Visit for EvField {
                    fn record_debug(
                        &mut self,
                        field: &tracing::field::Field,
                        value: &dyn std::fmt::Debug,
                    ) {
                        if field.name() == "event" {
                            self.0 = Some(format!("{value:?}"));
                        }
                    }
                }
                let mut v = EvField(None);
                event.record(&mut v);
                if let Some(name) = v.0 {
                    self.hits
                        .lock()
                        .expect("hits lock")
                        .push((name, ctx.event_span(event).is_some()));
                }
            }
        }

        let _serial = crate::testsupport::TRACING_TEST_LOCK.lock().await; // TRA-243
        let hits = Arc::new(Mutex::new(Vec::<(String, bool)>::new()));
        let guard =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(SpanCtxLayer {
                hits: Arc::clone(&hits),
            }));
        // TRA-243: warm up to force callsite registration, rebuild against this thread's subscriber,
        // then capture — see `worker_logs_carry_issue_identifier` for the full rationale.
        {
            let ag = notify_then_succeed();
            let tr = fake_tracker_by_id(&[("1", "MT-1", "Done")]);
            let (ws, _root) = test_workspace(HookScripts::default());
            let warm = make_deps(ws, ag, tr, "do it", 20);
            let _ = run_agent_attempt(
                &warm,
                issue("1", "MT-1", "In Progress"),
                None,
                None,
                &noop_event(),
                None,
            )
            .await;
        }
        tracing::callsite::rebuild_interest_cache();
        hits.lock().expect("hits lock").clear();

        let ag = notify_then_succeed();
        let tr = fake_tracker_by_id(&[("1", "MT-1", "Done")]);
        let (ws, _root) = test_workspace(HookScripts::default());
        let d = make_deps(ws, ag, tr, "do it", 20);
        let _ = run_agent_attempt(
            &d,
            issue("1", "MT-1", "In Progress"),
            None,
            None,
            &noop_event(),
            None,
        )
        .await;
        drop(guard);

        let hits = hits.lock().expect("hits lock");
        let agent_ev = hits
            .iter()
            .find(|(name, _)| name == EVENT_NOTIFICATION)
            .expect("expected an agent-event log for the emitted notification");
        assert!(
            agent_ev.1,
            "agent-event log must be emitted under the run/turn span for OTel trace correlation"
        );
    }
}
