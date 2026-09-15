//! CLI argument construction for the opencode backend (STUDIO-902). Rhapsody-only.
//!
//! [`Config`] is the backend's configuration and [`build_args`] assembles the per-turn `opencode
//! run` argv. The managed flag set is the one the STUDIO-869 spike actually ran — recorded verbatim
//! in `harness/harness-spike/README.md`'s Provenance section:
//!
//! ```text
//! opencode run --format json --auto --dir "$SB" -m <provider/model> "$PROMPT"
//! #   resume.jsonl: same, plus -s <sessionID>
//! ```
//!
//! ⚠️ **The prompt is a positional argument, not stdin.** That is why [`Config::extra_args`] are
//! placed BEFORE it rather than last as the claude backend places its own: `opencode run`'s
//! positional is `message..`, an array, so anything appended after the prompt is absorbed into the
//! message instead of being parsed as a flag. Operators still win over every managed flag; they
//! just cannot win over the prompt, because there is nothing after the prompt to win.

use std::fmt;
use std::time::Duration;

/// Configures the opencode backend.
///
/// `Debug` is hand-written to redact [`Config::tracker_api_key`], for exactly the reason
/// `crate::claude::Config`'s is: this type is reachable from `crate::harness::HarnessSpec`
/// (`HarnessKnobs::Opencode`), whose `Debug` a `tracing::debug!(?spec)` would use, and the resolved
/// Linear credential must never reach the rotating file logs.
#[derive(Clone, Default)]
pub struct Config {
    /// default `"opencode"`; shell-split into name+args by [`crate::claude::split_command`].
    ///
    /// ⚠️ An operator should set this to an ABSOLUTE path. On the spike machine
    /// `/opt/homebrew/bin/opencode` was a broken npm-global symlink that exits 1 without running
    /// anything, while the working binary lived under `/opt/homebrew/Cellar/…`
    /// (`STUDIO-869-harness-spike-findings.md` §4.1) — a daemon resolving the bare name from `PATH`
    /// there would get the dead one.
    pub command: String,
    /// `-m`, in opencode's `provider/model` form (e.g.
    /// `fireworks-ai/accounts/fireworks/models/deepseek-v4p1-flash`); omitted when empty.
    pub model: String,
    /// `--variant` — opencode's provider-specific reasoning-effort knob (`high`, `max`, `minimal`).
    /// This is where a teammate profile's `effort` lands for this harness, the counterpart of
    /// `claude --effort`; omitted when empty.
    pub variant: String,
    /// `--agent`; omitted when empty.
    pub agent: String,
    /// `--auto` (auto-approve permissions that are not explicitly denied) — opencode's counterpart
    /// of `claude --permission-mode bypassPermissions`, and what the spike captures all ran with.
    /// An absent knob (`None`) defaults to ENABLED, because a dispatched agent has no operator to
    /// answer a permission prompt and would otherwise block until the turn deadline.
    pub auto_approve: Option<bool>,
    /// absolute; used for the launch containment invariant
    pub workspace_root: String,
    pub turn_timeout: Duration,
    /// extra CLI args passed verbatim (win over every managed flag; see the module doc for why they
    /// are not literally last)
    pub extra_args: Vec<String>,
    /// The resolved tracker (Linear) credential, scrubbed from the child's environment by VALUE as
    /// well as by name — identical treatment to `crate::claude::Config::tracker_api_key`, and
    /// likewise independent of any billing guard.
    pub tracker_api_key: String,

    /// MCP injection. When `true`, the runner writes a config declaring a `symphony` server into
    /// the run's PRIVATE STATE DIRECTORY and points the child's `OPENCODE_CONFIG` at it, so the
    /// dispatched agent can reach the daemon's own tools.
    ///
    /// ⚠️ Nothing is written into the workspace. `opencode.json` is a name a real project may
    /// already track, and the agent is about to `git add -A` in that worktree — see
    /// [`crate::opencode::mcpinject`]'s module doc, which owns the reasoning, and the test
    /// `nothing_is_written_into_the_workspace`.
    pub inject_mcp: bool,
    /// Absolute path to the running daemon binary, used as the injected server's command.
    pub daemon_bin: String,
    /// Passed as `<daemon_bin> mcp <workflow_path>` so the child resolves the SAME workflow.
    pub workflow_path: String,

    /// Where per-run `XDG_DATA_HOME` directories are created. Empty ⇒ the system temp dir.
    ///
    /// ⚠️ This is never the worktree: opencode's state directory is a SQLite database plus a
    /// snapshot tree, and putting it inside the git worktree the agent is committing from would
    /// put it in `git status`.
    pub state_root: String,
    /// Absolute path to the operator's own `auth.json` to seed each per-run state directory from.
    /// Empty ⇒ resolved from the daemon's environment (see
    /// [`crate::opencode::state::default_auth_source`]).
    pub auth_source: String,
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("command", &self.command)
            .field("model", &self.model)
            .field("variant", &self.variant)
            .field("agent", &self.agent)
            .field("auto_approve", &self.auto_approve)
            .field("workspace_root", &self.workspace_root)
            .field("turn_timeout", &self.turn_timeout)
            .field("extra_args", &self.extra_args)
            .field("tracker_api_key", &"***")
            .field("inject_mcp", &self.inject_mcp)
            .field("daemon_bin", &self.daemon_bin)
            .field("workflow_path", &self.workflow_path)
            .field("state_root", &self.state_root)
            .field("auth_source", &self.auth_source)
            .finish()
    }
}

/// Whether `--auto` is on. An absent knob defaults to enabled (see [`Config::auto_approve`]).
pub fn auto_approve_enabled(knob: Option<bool>) -> bool {
    knob.unwrap_or(true)
}

/// Builds the per-turn opencode argv after the command name. `resume_id` is the session id to
/// continue (`-s`); empty on the first turn. `ws_path` is the worktree, passed as `--dir` in
/// addition to being the child's cwd — the spike captures ran with both, and `--dir` is what
/// opencode itself uses to scope the session.
pub fn build_args(cfg: &Config, ws_path: &str, resume_id: &str, prompt: &str) -> Vec<String> {
    let mut args = vec![
        "run".to_string(),
        "--format".to_string(),
        "json".to_string(),
    ];
    if auto_approve_enabled(cfg.auto_approve) {
        args.push("--auto".to_string());
    }
    args.push("--dir".to_string());
    args.push(ws_path.to_string());
    if !cfg.model.is_empty() {
        args.push("-m".to_string());
        args.push(cfg.model.clone());
    }
    if !cfg.variant.is_empty() {
        args.push("--variant".to_string());
        args.push(cfg.variant.clone());
    }
    if !cfg.agent.is_empty() {
        args.push("--agent".to_string());
        args.push(cfg.agent.clone());
    }
    if !resume_id.is_empty() {
        args.push("-s".to_string());
        args.push(resume_id.to_string());
    }
    args.extend(cfg.extra_args.iter().cloned());
    // LAST, and deliberately: `message..` is a positional array, so nothing may follow it.
    args.push(prompt.to_string());
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        Config {
            command: "opencode".to_string(),
            model: "fireworks-ai/accounts/fireworks/models/deepseek-v4p1-flash".to_string(),
            ..Default::default()
        }
    }

    // The managed argv must reproduce the flag set the spike actually ran, which is the only
    // invocation of this CLI anyone has evidence for. The literal is taken from
    // `harness/harness-spike/README.md`'s Provenance block.
    #[test]
    fn first_turn_argv_matches_the_captured_invocation() {
        let got = build_args(&cfg(), "/ws", "", "do the thing");
        assert_eq!(
            got,
            vec![
                "run",
                "--format",
                "json",
                "--auto",
                "--dir",
                "/ws",
                "-m",
                "fireworks-ai/accounts/fireworks/models/deepseek-v4p1-flash",
                "do the thing",
            ]
        );
    }

    // Resume is the same flags plus `-s <id>` (design §3's `Resume::Flags`), which is what
    // `harness/harness-spike/opencode/resume.jsonl` was captured with.
    #[test]
    fn resume_adds_only_the_session_flag() {
        let first = build_args(&cfg(), "/ws", "", "p");
        let resumed = build_args(&cfg(), "/ws", "ses_abc", "p");
        assert_eq!(resumed.len(), first.len() + 2);
        let pos = resumed.iter().position(|a| a == "-s").expect("-s present");
        assert_eq!(resumed[pos + 1], "ses_abc");
        // Everything except the inserted pair is unchanged and in the same order.
        let stripped: Vec<&String> = resumed
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != pos && *i != pos + 1)
            .map(|(_, a)| a)
            .collect();
        assert_eq!(stripped, first.iter().collect::<Vec<_>>());
    }

    // The prompt is a positional, so it must be the final element even when an operator supplies
    // extra args — otherwise their flags are swallowed into opencode's `message..` array and the
    // agent is told to do something that includes the text `--foo`.
    #[test]
    fn extra_args_precede_the_positional_prompt() {
        let mut c = cfg();
        c.extra_args = vec!["--log-level".to_string(), "DEBUG".to_string()];
        let got = build_args(&c, "/ws", "", "PROMPT");
        assert_eq!(got.last().map(String::as_str), Some("PROMPT"));
        let i = got
            .iter()
            .position(|a| a == "--log-level")
            .expect("present");
        assert_eq!(got[i + 1], "DEBUG");
        assert!(i + 2 < got.len(), "extra args are not last; the prompt is");
    }

    #[test]
    fn empty_optional_knobs_are_omitted_and_auto_defaults_on() {
        let got = build_args(&Config::default(), "/ws", "", "p");
        assert_eq!(
            got,
            vec!["run", "--format", "json", "--auto", "--dir", "/ws", "p"]
        );
        assert!(auto_approve_enabled(None));
        assert!(!auto_approve_enabled(Some(false)));

        let mut c = Config {
            auto_approve: Some(false),
            variant: "high".to_string(),
            agent: "build".to_string(),
            ..Default::default()
        };
        c.model = "p/m".to_string();
        let got = build_args(&c, "/ws", "", "p");
        assert!(!got.contains(&"--auto".to_string()));
        assert_eq!(
            got,
            vec![
                "run",
                "--format",
                "json",
                "--dir",
                "/ws",
                "-m",
                "p/m",
                "--variant",
                "high",
                "--agent",
                "build",
                "p"
            ]
        );
    }

    // The tracker credential must not be reachable through a `{:?}` on the knobs block.
    #[test]
    fn debug_redacts_the_tracker_key() {
        let c = Config {
            tracker_api_key: "lin_api_secret_value".to_string(),
            ..Default::default()
        };
        let rendered = format!("{c:?}");
        assert!(!rendered.contains("lin_api_secret_value"), "{rendered}");
        assert!(rendered.contains("***"));
    }
}
