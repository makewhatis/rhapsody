//! `decode` — parity port of Go `internal/config.Decode` (+ its file-local helpers).
//!
//! Converts a workflow [`Definition`] into a typed [`Config`] with per-field defaults
//! applied. It deliberately does NOT resolve `$VAR`/`~` or validate — Go's `Decode`
//! header says the same, and both are downstream tasks (Resolve = C4, Validate = C5).
//! In particular `tracker.api_key` is stored verbatim here; `$VAR` expansion happens
//! only in `resolve.go`'s `resolveVar` (the sole `os.Getenv` in the package), so this
//! port performs no environment-variable expansion.
//!
//! The Go `raw`/`rawProject`/… structs live in [`crate::model`] as the private `Raw`
//! tree; every default comes from a `yaml.v3` zero value there plus an `or*` fallback here.

use std::collections::HashMap;

use chrono::Duration;
use rhapsody_core::normalize_state;
use serde_yaml_ng::Value;

use crate::model::{
    Agent, Claude, ClaudeOverride, Codex, Config, DEFAULT_OTEL_ENDPOINT, Hooks, Logging, Mcp, Otel,
    Polling, Project, Raw, RawClaudeOverride, RawProject, Server, Storage, Tracker, Workspace,
};
use crate::workflow::Definition;

/// Errors from [`decode`]. `Parse` Displays with Go's `workflow_parse_error` sentinel token
/// (the same one `internal/workflow` uses) so daemon logs and config-API bodies byte-match.
/// Later config tasks (Resolve/Validate) extend this enum with their own variants.
#[derive(thiserror::Error, Debug)]
pub enum ConfigError {
    /// A front-matter (re)serialization failure, a type mismatch while decoding into the raw
    /// schema, or a malformed duration knob. Mirrors Go's `fmt.Errorf("%w: …", ErrWorkflowParse)`.
    #[error("workflow_parse_error: {0}")]
    Parse(String),
    /// The current working directory could not be determined while making a relative
    /// `workspace.root` / `logging.dir` absolute (Go `Resolve` returns the bare `filepath.Abs` →
    /// `os.Getwd` error here — the config package attaches no sentinel token, and no test exercises
    /// it, since a missing cwd aborts config load). Surfaced as a value, not a panic, to keep the
    /// crate `unwrap`-free. Constructed by [`crate::resolve::resolve`].
    #[error("could not resolve working directory: {0}")]
    WorkingDir(String),
}

/// Decodes `def` into a typed [`Config`] with defaults applied (Go `config.Decode`).
///
/// Mirrors Go's two-step `yaml.Marshal(def.Config)` → `yaml.Unmarshal(bytes, &raw)`: we
/// re-serialize the already-parsed front-matter mapping and deserialize it into [`Raw`].
/// Both error sites map to [`ConfigError::Parse`], exactly as Go wraps them in
/// `ErrWorkflowParse`.
pub fn decode(def: &Definition) -> Result<Config, ConfigError> {
    let bytes =
        serde_yaml_ng::to_string(&def.config).map_err(|e| ConfigError::Parse(e.to_string()))?;
    let r: Raw = serde_yaml_ng::from_str(&bytes).map_err(|e| ConfigError::Parse(e.to_string()))?;

    // tracker — endpoint/summon/promote/state-lists carry defaults; api_key stored verbatim
    // ($VAR resolved in C4); dependency_mode/claim_mode map verbatim (defaults materialized in
    // resolve); claim_ttl/claim_settle_delay parse from duration strings (empty ⇒ zero).
    let tracker = Tracker {
        kind: r.tracker.kind,
        endpoint: or_str(r.tracker.endpoint, "https://api.linear.app/graphql"),
        api_key: r.tracker.api_key,
        project_slug: r.tracker.project_slug,
        source: r.tracker.source,
        active_states: or_slice(
            r.tracker.active_states,
            vec!["Todo".to_string(), "In Progress".to_string()],
        ),
        terminal_states: or_slice(
            r.tracker.terminal_states,
            vec![
                "Closed".to_string(),
                "Cancelled".to_string(),
                "Canceled".to_string(),
                "Duplicate".to_string(),
                "Done".to_string(),
            ],
        ),
        canceled_states: or_slice(
            r.tracker.canceled_states,
            vec![
                "Cancelled".to_string(),
                "Canceled".to_string(),
                "Duplicate".to_string(),
            ],
        ),
        review_states: r.tracker.review_states,
        summon_token: or_str(r.tracker.summon_token, "@symphony"),
        github_summons: or_bool(r.tracker.github_summons, false),
        review_promote_state: or_str(r.tracker.review_promote_state, "In Progress"),
        milestone: r.tracker.milestone,
        labels: r.tracker.labels,
        capabilities: r.tracker.capabilities,
        dependency_mode: r.tracker.dependency_mode,
        dep_mode_prompt_file: r.tracker.dep_mode_prompt_file,
        claim_mode: r.tracker.claim_mode,
        claim_ttl: parse_optional_duration(&r.tracker.claim_ttl, "claim_ttl")?,
        claim_settle_delay: parse_optional_duration(
            &r.tracker.claim_settle_delay,
            "claim_settle_delay",
        )?,
    };

    let polling = Polling {
        interval_ms: or_int(r.polling.interval_ms, 30000),
    };

    // workspace.root resolved in C4; keep raw value for now.
    let workspace = Workspace {
        root: r.workspace.root,
    };

    let hooks = Hooks {
        after_create: r.hooks.after_create,
        before_run: r.hooks.before_run,
        after_run: r.hooks.after_run,
        before_remove: r.hooks.before_remove,
        timeout_ms: or_int(r.hooks.timeout_ms, 60000),
    };

    let agent = Agent {
        backend: or_str(r.agent.backend, "claude"),
        max_concurrent_agents: or_int(r.agent.max_concurrent_agents, 10),
        max_turns: or_int(r.agent.max_turns, 20),
        max_retry_backoff_ms: or_int(r.agent.max_retry_backoff_ms, 300000),
        max_concurrent_agents_by_state: normalize_state_map(r.agent.max_concurrent_agents_by_state),
        handoff_drain_grace_ms: or_int(r.agent.handoff_drain_grace_ms, 10000),
    };

    let codex = Codex {
        command: or_str(r.codex.command, "codex app-server"),
        approval_policy: r.codex.approval_policy,
        thread_sandbox: r.codex.thread_sandbox,
        turn_sandbox_policy: r.codex.turn_sandbox_policy,
        turn_timeout_ms: or_int(r.codex.turn_timeout_ms, 3600000),
        read_timeout_ms: or_int(r.codex.read_timeout_ms, 5000),
        stall_timeout_ms: or_int(r.codex.stall_timeout_ms, 300000),
    };

    let claude = Claude {
        command: or_str(r.claude.command, "claude"),
        model: r.claude.model,
        effort: r.claude.effort,
        permission_mode: or_str(r.claude.permission_mode, "bypassPermissions"),
        allowed_tools: r.claude.allowed_tools,
        disallowed_tools: r.claude.disallowed_tools,
        mcp_config: r.claude.mcp_config,
        setting_sources: r.claude.setting_sources,
        add_dirs: r.claude.add_dirs,
        turn_timeout_ms: or_int(r.claude.turn_timeout_ms, 3600000),
        read_timeout_ms: or_int(r.claude.read_timeout_ms, 5000),
        stall_timeout_ms: or_int(r.claude.stall_timeout_ms, 300000),
        extra_args: r.claude.extra_args,
        // billing_guard: absent/nil ⇒ enabled (true); explicit value honored verbatim. Kept as a
        // pointer (always Some after decode) so Encode round-trips an explicit false.
        billing_guard: Some(or_bool(r.claude.billing_guard, true)),
        // ultracode: absent/nil ⇒ disabled (false); explicit value honored verbatim.
        ultracode: or_bool(r.claude.ultracode, false),
    };

    let server = Server {
        port: r.server.port,
    };

    // logging.dir default: ~/.rhapsody/logs — the TRA-238 divergence from Go v0.4.0's ~/.symphony/logs
    // (Rhapsody's runtime home; see resolve.rs + the root README DIVERGENCES section). Resolve carries
    // the same default for hand-built configs.
    let logging = Logging {
        dir: or_str(r.logging.dir, "~/.rhapsody/logs"),
    };

    // storage mapped verbatim; path/retention defaults are applied in Resolve (C4).
    let storage = Storage {
        path: r.storage.path,
        retention_days: r.storage.retention_days,
    };

    // otel — default ON + hub endpoint + OTLP/HTTP when unset (INF-442/INF-299); explicit values win.
    let otel = Otel {
        enabled: or_bool(r.otel.enabled, true),
        endpoint: or_str(r.otel.endpoint, DEFAULT_OTEL_ENDPOINT),
        protocol: or_str(r.otel.protocol, "http"),
        service_name: or_str(r.otel.service_name, "symphony"),
        headers: r.otel.headers,
        insecure: r.otel.insecure,
        // No string default: empty operator means "derive in telemetry" (OS user → host).
        operator: r.otel.operator,
    };

    // mcp — default-ON injection + send_message + handoff (opt-outs); stop/resume opt-in (default OFF).
    // allow_handoff (TRA-242) gates the daemon-mediated review handoff tool; NEW beyond Go v0.4.0.
    let mcp = Mcp {
        enabled: or_bool(r.mcp.enabled, true),
        allow_send_message: or_bool(r.mcp.allow_send_message, true),
        allow_stop: or_bool(r.mcp.allow_stop, false),
        allow_resume: or_bool(r.mcp.allow_resume, false),
        allow_handoff: or_bool(r.mcp.allow_handoff, true),
    };

    // multi-project routing — overrides mapped verbatim (nil preserved) so ResolveProjects can
    // tell inherit from set; a workflow without `projects:` decodes to repo == "" and no projects.
    let projects = r.projects.into_iter().map(decode_project).collect();

    Ok(Config {
        tracker,
        polling,
        workspace,
        hooks,
        agent,
        codex,
        claude,
        server,
        logging,
        otel,
        mcp,
        storage,
        repo: r.repo,
        projects,
        prompt_template: def.prompt_template.clone(),
        prompt_file: r.prompt_file,
        // WorkflowDir is set in Resolve (C4); WorkflowPath is stamped by the orchestrator.
        workflow_dir: String::new(),
        workflow_path: String::new(),
        git_flow: r.git_flow,
        workspace_mode: r.workspace_mode,
        // pr_label defaults to "symphony" so the post-run labeler is on by default but tunable.
        pr_label: or_str(r.pr_label, "symphony"),
    })
}

/// Maps one raw project entry to a typed [`Project`] (Go `decodeProject`). Override fields are
/// preserved verbatim (nil/None ⇒ inherit) with no defaults applied, except per-project hook
/// timeout which defaults like the top level.
fn decode_project(rp: RawProject) -> Project {
    // An all-empty `claude: {}` block decodes to a non-nil zero override; Go normalizes it to
    // absent so the first Encode is already canonical and the on-disk shape is save-stable
    // (INF-224). `Option::filter` drops the empty override to None, mirroring that.
    let claude = rp
        .claude
        .filter(|c| !raw_claude_override_is_empty(c))
        .map(|c| ClaudeOverride {
            command: c.command,
            model: c.model,
            effort: c.effort,
            permission_mode: c.permission_mode,
            allowed_tools: c.allowed_tools,
            disallowed_tools: c.disallowed_tools,
            mcp_config: c.mcp_config,
            setting_sources: c.setting_sources,
            add_dirs: c.add_dirs,
            turn_timeout_ms: c.turn_timeout_ms,
            read_timeout_ms: c.read_timeout_ms,
            stall_timeout_ms: c.stall_timeout_ms,
            extra_args: c.extra_args,
            billing_guard: c.billing_guard,
            ultracode: c.ultracode,
        });

    let hooks = rp.hooks.map(|h| Hooks {
        after_create: h.after_create,
        before_run: h.before_run,
        after_run: h.after_run,
        before_remove: h.before_remove,
        timeout_ms: or_int(h.timeout_ms, 60000),
    });

    Project {
        name: rp.name,
        repo: rp.repo,
        slugs: rp.slugs,
        active_states: rp.active_states,
        terminal_states: rp.terminal_states,
        canceled_states: rp.canceled_states,
        review_states: rp.review_states,
        prompt: rp.prompt,
        prompt_file: rp.prompt_file,
        claude,
        hooks,
        max_concurrent_agents: rp.max_concurrent_agents,
        milestone: rp.milestone,
        labels: rp.labels,
        capabilities: rp.capabilities,
        git_flow: rp.git_flow,
        workspace_mode: rp.workspace_mode,
        dependency_mode: rp.dependency_mode,
        dep_mode_prompt_file: rp.dep_mode_prompt_file,
        claim_mode: rp.claim_mode,
        // Mapped verbatim (None preserved) so the enabled default is applied at resolve time.
        enabled: rp.enabled,
    }
}

/// Reports whether an override carries no settings — every pointer None, every slice empty
/// (Go `rawClaudeOverrideIsEmpty`, minus its nil short-circuit which `Option::filter` handles).
fn raw_claude_override_is_empty(c: &RawClaudeOverride) -> bool {
    c.command.is_none()
        && c.model.is_none()
        && c.effort.is_none()
        && c.permission_mode.is_none()
        && c.allowed_tools.is_none()
        && c.disallowed_tools.is_none()
        && c.mcp_config.is_none()
        && c.setting_sources.is_none()
        && c.add_dirs.is_empty()
        && c.turn_timeout_ms.is_none()
        && c.read_timeout_ms.is_none()
        && c.stall_timeout_ms.is_none()
        && c.extra_args.is_empty()
        && c.billing_guard.is_none()
        && c.ultracode.is_none()
}

/// Go `orStr`: the default when `v` is empty, else `v` verbatim.
fn or_str(v: String, def: &str) -> String {
    if v.is_empty() { def.to_string() } else { v }
}

/// Go `orInt`: the default when the pointer is unset, else `*p` — an explicit `0` survives.
fn or_int(p: Option<i64>, def: i64) -> i64 {
    p.unwrap_or(def)
}

/// Go `*orBoolPtr(p, def)`: the default when the pointer is unset, else the explicit bool.
fn or_bool(p: Option<bool>, def: bool) -> bool {
    p.unwrap_or(def)
}

/// Go `orSlice`: the default when `v` is empty (len 0), else `v` verbatim.
fn or_slice(v: Vec<String>, def: Vec<String>) -> Vec<String> {
    if v.is_empty() { def } else { v }
}

/// Go `parseOptionalDuration`: an empty (trimmed) value is zero (the orchestrator materializes a
/// default later); a malformed value is a decode error, wrapped as `workflow_parse_error` and
/// labeled with the field name so the config API can surface it (INF-477).
fn parse_optional_duration(v: &str, field: &str) -> Result<Duration, ConfigError> {
    if v.trim().is_empty() {
        return Ok(Duration::nanoseconds(0));
    }
    parse_go_duration(v).map_err(|e| ConfigError::Parse(format!("{field}: {e}")))
}

/// Go `normalizeStateMap`: lowercase keys, keep only positive-integer values (upstream §5.3.5).
fn normalize_state_map(input: HashMap<String, Value>) -> HashMap<String, i64> {
    let mut out = HashMap::new();
    for (k, v) in input {
        match as_int(&v) {
            Some(n) if n > 0 => {
                out.insert(normalize_state(&k), n);
            }
            _ => {}
        }
    }
    out
}

/// Go `asInt`: an int-typed YAML value, or a whole-number float, else None. A non-numeric value
/// (string/bool) yields None. Uses a fractional-part test (never a float `==`) for the float case.
fn as_int(v: &Value) -> Option<i64> {
    if let Some(n) = v.as_i64() {
        return Some(n);
    }
    let f = v.as_f64()?;
    if f.is_finite() && f.fract().abs() < f64::EPSILON {
        Some(f as i64)
    } else {
        None
    }
}

/// Faithful port of Go `time.ParseDuration`: an optionally-signed sequence of decimal numbers,
/// each with an optional fraction and a required unit (`ns`, `us`/`µs`/`μs`, `ms`, `s`, `m`, `h`).
/// `"0"` is the only unitless form. Overflow (past `1<<63` ns) is an error, as in Go. The returned
/// [`Duration`] is nanosecond-exact; a leading `-` yields a negative duration.
fn parse_go_duration(orig: &str) -> Result<Duration, String> {
    const MAX: u64 = 1u64 << 63;
    let invalid = || format!("time: invalid duration \"{orig}\"");
    let b = orig.as_bytes();
    let mut i = 0usize;

    let mut neg = false;
    if i < b.len() && (b[i] == b'-' || b[i] == b'+') {
        neg = b[i] == b'-';
        i += 1;
    }
    // Special case: "0" (with any leading sign already consumed) parses to zero with no unit.
    if &orig[i..] == "0" {
        return Ok(Duration::nanoseconds(0));
    }
    if i >= b.len() {
        return Err(invalid());
    }

    let mut total: u64 = 0; // nanoseconds
    while i < b.len() {
        // Each term must open with a digit or '.'.
        if !(b[i] == b'.' || b[i].is_ascii_digit()) {
            return Err(invalid());
        }
        // Integer part.
        let int_start = i;
        let (v_int, ni) = leading_int(b, i).map_err(|()| invalid())?;
        i = ni;
        let pre = i != int_start;

        // Fraction part.
        let mut frac: u64 = 0;
        let mut scale: f64 = 1.0;
        let mut post = false;
        if i < b.len() && b[i] == b'.' {
            i += 1;
            let frac_start = i;
            let (f, sc, ni) = leading_fraction(b, i);
            frac = f;
            scale = sc;
            i = ni;
            post = i != frac_start;
        }
        if !pre && !post {
            return Err(invalid());
        }

        // Unit: run of non-digit, non-'.' bytes (may be the multi-byte µs).
        let unit_start = i;
        while i < b.len() && !(b[i] == b'.' || b[i].is_ascii_digit()) {
            i += 1;
        }
        if i == unit_start {
            return Err(format!("time: missing unit in duration \"{orig}\""));
        }
        let unit_str = &orig[unit_start..i];
        let unit = unit_ns(unit_str)
            .ok_or_else(|| format!("time: unknown unit \"{unit_str}\" in duration \"{orig}\""))?;

        if v_int > MAX / unit {
            return Err(invalid());
        }
        let mut v = v_int * unit;
        if frac > 0 {
            // v += f * (unit / scale)
            let add = (frac as f64 * (unit as f64 / scale)) as u64;
            v = v.checked_add(add).ok_or_else(invalid)?;
            if v > MAX {
                return Err(invalid());
            }
        }
        total = total.checked_add(v).ok_or_else(invalid)?;
        if total > MAX {
            return Err(invalid());
        }
    }

    if neg {
        // total <= MAX; -(MAX) as i64 is i64::MIN, exactly Go's -Duration(1<<63).
        return Ok(Duration::nanoseconds(-(total as i128) as i64));
    }
    if total > MAX - 1 {
        return Err(invalid());
    }
    Ok(Duration::nanoseconds(total as i64))
}

/// Nanoseconds per unit for [`parse_go_duration`] (Go `unitMap`).
fn unit_ns(u: &str) -> Option<u64> {
    match u {
        "ns" => Some(1),
        "us" | "µs" | "μs" => Some(1_000),
        "ms" => Some(1_000_000),
        "s" => Some(1_000_000_000),
        "m" => Some(60_000_000_000),
        "h" => Some(3_600_000_000_000),
        _ => None,
    }
}

/// Go `leadingInt`: consume a run of ASCII digits into a `u64`, erroring on overflow past `1<<63`.
fn leading_int(b: &[u8], mut i: usize) -> Result<(u64, usize), ()> {
    const MAX: u64 = 1u64 << 63;
    let mut x: u64 = 0;
    while i < b.len() && b[i].is_ascii_digit() {
        if x > MAX / 10 {
            return Err(());
        }
        x = x * 10 + u64::from(b[i] - b'0');
        if x > MAX {
            return Err(());
        }
        i += 1;
    }
    Ok((x, i))
}

/// Go `leadingFraction`: consume a run of ASCII digits as a fraction, tracking the power-of-ten
/// scale; digits past overflow are dropped (they cannot affect the result), matching Go.
fn leading_fraction(b: &[u8], mut i: usize) -> (u64, f64, usize) {
    const MAX: u64 = 1u64 << 63;
    let mut x: u64 = 0;
    let mut scale: f64 = 1.0;
    let mut overflow = false;
    while i < b.len() && b[i].is_ascii_digit() {
        if overflow {
            i += 1;
            continue;
        }
        if x > (MAX - 1) / 10 {
            overflow = true;
            i += 1;
            continue;
        }
        let y = x * 10 + u64::from(b[i] - b'0');
        if y > MAX {
            overflow = true;
            i += 1;
            continue;
        }
        x = y;
        scale *= 10.0;
        i += 1;
    }
    (x, scale, i)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::YamlMap;

    /// Build a [`Definition`] from a YAML front-matter string + prompt body, then decode it.
    /// The front matter is parsed into the same `YamlMap` the workflow loader produces, so this
    /// exercises the identical path the daemon takes (`workflow::load` → `decode`). Mirrors the Go
    /// `decode`/`decodeMap` test helpers, which pass an equivalent `map[string]any`.
    fn decode_yaml(front: &str, prompt: &str) -> Config {
        let config: YamlMap = if front.trim().is_empty() {
            YamlMap::new()
        } else {
            serde_yaml_ng::from_str(front).expect("test front matter must parse")
        };
        let def = Definition {
            config,
            prompt_template: prompt.to_string(),
        };
        decode(&def).expect("decode should succeed")
    }

    // ---- config_test.go mirrors ----

    // Mirrors Go `TestDecodeAppliesDefaults`.
    #[test]
    fn decode_applies_defaults() {
        let c = decode_yaml("", "body");
        assert_eq!(c.polling.interval_ms, 30000);
        assert_eq!(c.agent.max_concurrent_agents, 10);
        assert_eq!(c.agent.max_turns, 20);
        assert_eq!(c.agent.max_retry_backoff_ms, 300000);
        assert_eq!(c.agent.handoff_drain_grace_ms, 10000);
        assert_eq!(c.agent.backend, "claude");
        assert_eq!(c.hooks.timeout_ms, 60000);
        assert_eq!(c.tracker.endpoint, "https://api.linear.app/graphql");
        assert_eq!(c.tracker.active_states.len(), 2);
        assert_eq!(c.tracker.active_states[0], "Todo");
        assert_eq!(c.tracker.terminal_states.len(), 5);
        assert_eq!(c.tracker.canceled_states.len(), 3);
        assert_eq!(c.tracker.canceled_states[0], "Cancelled");
        assert_eq!(c.claude.command, "claude");
        assert_eq!(c.codex.command, "codex app-server");
        assert_eq!(c.prompt_template, "body");
    }

    // Mirrors Go `TestDecodeCanceledStates`.
    #[test]
    fn decode_canceled_states() {
        let c = decode_yaml(
            "tracker:\n  project_slug: p\n  canceled_states:\n    - \"Won't Do\"\n    - Obsolete\n",
            "body",
        );
        assert_eq!(c.tracker.canceled_states.len(), 2);
        assert_eq!(c.tracker.canceled_states[0], "Won't Do");
        // Absent/empty falls back to the default (orSlice semantics).
        let none = decode_yaml("tracker:\n  project_slug: p\n", "body");
        assert_eq!(none.tracker.canceled_states.len(), 3);
    }

    // Mirrors Go `TestDecodeMilestone`.
    #[test]
    fn decode_milestone() {
        let c = decode_yaml("tracker:\n  project_slug: p\n  milestone: v2.0\n", "body");
        assert_eq!(c.tracker.milestone, "v2.0");
        let none = decode_yaml("tracker:\n  project_slug: p\n", "body");
        assert_eq!(none.tracker.milestone, "");
    }

    // Mirrors Go `TestDecodeOverridesAndPerStateNormalization`.
    #[test]
    fn decode_overrides_and_per_state_normalization() {
        let front = "polling:\n  interval_ms: 5000\nagent:\n  backend: claude\n  max_concurrent_agents: 3\n  max_concurrent_agents_by_state:\n    In Progress: 2\n    Bad: 0\n    AlsoBad: x\n";
        let c = decode_yaml(front, "");
        assert_eq!(c.polling.interval_ms, 5000);
        assert_eq!(c.agent.max_concurrent_agents, 3);
        assert_eq!(
            c.agent.max_concurrent_agents_by_state.get("in progress"),
            Some(&2)
        );
        assert!(
            !c.agent.max_concurrent_agents_by_state.contains_key("bad"),
            "non-positive per-state entry should be ignored"
        );
        assert!(
            !c.agent
                .max_concurrent_agents_by_state
                .contains_key("alsobad"),
            "non-numeric per-state entry should be ignored"
        );
    }

    // Mirrors Go `TestDecodeClaudeExtraArgs`.
    #[test]
    fn decode_claude_extra_args() {
        let c = decode_yaml(
            "claude:\n  extra_args:\n    - \"--settings\"\n    - '{\"ultracode\": true}'\n",
            "",
        );
        assert_eq!(c.claude.extra_args.len(), 2);
        assert_eq!(c.claude.extra_args[0], "--settings");
        assert_eq!(c.claude.extra_args[1], "{\"ultracode\": true}");
    }

    // Mirrors Go `TestDecodeClaudeNewKnobs`.
    #[test]
    fn decode_claude_new_knobs() {
        let c = decode_yaml(
            "claude:\n  effort: xhigh\n  disallowed_tools: WebFetch,Bash\n  setting_sources: project\n  add_dirs:\n    - /a\n    - /b\n",
            "",
        );
        assert_eq!(c.claude.effort, "xhigh");
        assert_eq!(c.claude.disallowed_tools, "WebFetch,Bash");
        assert_eq!(c.claude.setting_sources, "project");
        assert_eq!(c.claude.add_dirs.len(), 2);
        assert_eq!(c.claude.add_dirs[0], "/a");
        assert_eq!(c.claude.add_dirs[1], "/b");
    }

    // Mirrors Go `TestDecodeClaudeNewKnobsDefaultsEmpty`.
    #[test]
    fn decode_claude_new_knobs_defaults_empty() {
        let c = decode_yaml("", "");
        assert_eq!(c.claude.effort, "");
        assert_eq!(c.claude.disallowed_tools, "");
        assert_eq!(c.claude.setting_sources, "");
        assert!(c.claude.add_dirs.is_empty());
    }

    // Mirrors Go `TestDecodeBillingGuardDefaultsTrue`.
    #[test]
    fn decode_billing_guard_defaults_true() {
        let c = decode_yaml("", "");
        assert_eq!(
            c.claude.billing_guard,
            Some(true),
            "BillingGuard absent should default to Some(true)"
        );
    }

    // Mirrors Go `TestDecodeBillingGuardExplicitFalse`.
    #[test]
    fn decode_billing_guard_explicit_false() {
        let c = decode_yaml("claude:\n  billing_guard: false\n", "");
        assert_eq!(c.claude.billing_guard, Some(false));
    }

    // Mirrors Go `TestDecodeBillingGuardExplicitTrue`.
    #[test]
    fn decode_billing_guard_explicit_true() {
        let c = decode_yaml("claude:\n  billing_guard: true\n", "");
        assert_eq!(c.claude.billing_guard, Some(true));
    }

    // Mirrors Go `TestDecodeUltracodeDefaultsFalse`.
    #[test]
    fn decode_ultracode_defaults_false() {
        let c = decode_yaml("", "");
        assert!(
            !c.claude.ultracode,
            "ultracode absent should default to false"
        );
    }

    // Mirrors Go `TestDecodeUltracodeExplicitTrue`.
    #[test]
    fn decode_ultracode_explicit_true() {
        let c = decode_yaml("claude:\n  ultracode: true\n", "");
        assert!(c.claude.ultracode);
    }

    // Mirrors Go `TestDecodeUltracodeExplicitFalse`.
    #[test]
    fn decode_ultracode_explicit_false() {
        let c = decode_yaml("claude:\n  ultracode: false\n", "");
        assert!(!c.claude.ultracode);
    }

    // Mirrors Go `TestDecodeLoggingDefault`, but asserts Rhapsody's ~/.rhapsody/logs default — the
    // TRA-238 divergence from Go v0.4.0's ~/.symphony/logs.
    #[test]
    fn decode_logging_default() {
        let c = decode_yaml("", "body");
        assert_eq!(c.logging.dir, "~/.rhapsody/logs");
    }

    // Mirrors Go `TestDecodeLoggingOverride`.
    #[test]
    fn decode_logging_override() {
        let c = decode_yaml("logging:\n  dir: /var/log/symphony\n", "");
        assert_eq!(c.logging.dir, "/var/log/symphony");
    }

    // Mirrors Go `TestDecodeOtelDefaults`.
    #[test]
    fn decode_otel_defaults() {
        let c = decode_yaml("", "body");
        assert!(
            c.otel.enabled,
            "otel should default ON with no otel block (INF-442)"
        );
        assert_eq!(c.otel.endpoint, DEFAULT_OTEL_ENDPOINT);
        assert_eq!(c.otel.protocol, "http");
        assert_eq!(c.otel.service_name, "symphony");
        assert_eq!(c.otel.operator, "");
    }

    // Mirrors Go `TestDecodeOtelEnabledOptOut`.
    #[test]
    fn decode_otel_enabled_opt_out() {
        let c = decode_yaml("otel:\n  enabled: false\n", "body");
        assert!(
            !c.otel.enabled,
            "explicit otel.enabled: false must stay false"
        );
        assert_eq!(c.otel.endpoint, DEFAULT_OTEL_ENDPOINT);
    }

    // Mirrors Go `TestDecodeOtelEnabledExplicitTrue`.
    #[test]
    fn decode_otel_enabled_explicit_true() {
        let c = decode_yaml("otel:\n  enabled: true\n", "body");
        assert!(c.otel.enabled);
    }

    // Mirrors Go `TestDecodeOtelEndpointRespected`.
    #[test]
    fn decode_otel_endpoint_respected() {
        let c = decode_yaml("otel:\n  endpoint: https://my-collector:4318\n", "body");
        assert_eq!(c.otel.endpoint, "https://my-collector:4318");
    }

    // Mirrors Go `TestDecodeOtelProtocolGRPCRespected`.
    #[test]
    fn decode_otel_protocol_grpc_respected() {
        let c = decode_yaml("otel:\n  protocol: grpc\n", "body");
        assert_eq!(c.otel.protocol, "grpc");
    }

    // Mirrors Go `TestDecodeOtelOverride`.
    #[test]
    fn decode_otel_override() {
        let c = decode_yaml(
            "otel:\n  enabled: true\n  endpoint: localhost:4317\n  protocol: http\n  service_name: sym2\n  headers:\n    authorization: \"Bearer x\"\n  operator: fleet-1\n",
            "",
        );
        assert!(c.otel.enabled);
        assert_eq!(c.otel.endpoint, "localhost:4317");
        assert_eq!(c.otel.protocol, "http");
        assert_eq!(c.otel.service_name, "sym2");
        assert_eq!(
            c.otel.headers.get("authorization").map(String::as_str),
            Some("Bearer x")
        );
        assert_eq!(c.otel.operator, "fleet-1");
    }

    // Mirrors Go `TestDecodeProjectsFrontMatter`.
    #[test]
    fn decode_projects_front_matter() {
        let front = concat!(
            "repo: \"git@github.com:o/top.git\"\n",
            "projects:\n",
            "  - repo: \"git@github.com:o/r1.git\"\n",
            "    slugs:\n",
            "      - a\n",
            "      - b\n",
            "    claude:\n",
            "      model: sonnet\n",
            "      allowed_tools: OnlyRead\n",
            "      extra_args:\n",
            "        - --x\n",
            "    max_concurrent_agents: 2\n",
            "  - slugs:\n",
            "      - c\n",
            "    active_states:\n",
            "      - Started\n",
            "    terminal_states:\n",
            "      - Shipped\n",
            "    prompt: proj prompt\n",
            "    hooks:\n",
            "      after_create: echo a\n",
        );
        let c = decode_yaml(front, "body");
        assert_eq!(c.repo, "git@github.com:o/top.git");
        assert_eq!(c.projects.len(), 2);

        let p0 = &c.projects[0];
        assert_eq!(p0.repo, "git@github.com:o/r1.git");
        assert_eq!(p0.slugs.len(), 2);
        assert_eq!(p0.slugs[0], "a");
        let p0c = p0.claude.as_ref().expect("p0.claude should be Some");
        // Override pointers preserve None for unset fields (no defaults applied).
        assert_eq!(p0c.model.as_deref(), Some("sonnet"));
        assert_eq!(p0c.allowed_tools.as_deref(), Some("OnlyRead"));
        assert_eq!(p0c.effort, None);
        assert_eq!(p0c.permission_mode, None);
        assert_eq!(p0c.extra_args.len(), 1);
        assert_eq!(p0c.extra_args[0], "--x");
        assert_eq!(p0.max_concurrent_agents, Some(2));

        let p1 = &c.projects[1];
        assert_eq!(p1.slugs.len(), 1);
        assert_eq!(p1.slugs[0], "c");
        assert!(
            p1.claude.is_none(),
            "p1.claude should be None (no claude block)"
        );
        assert_eq!(p1.active_states.len(), 1);
        assert_eq!(p1.active_states[0], "Started");
        assert_eq!(p1.prompt, "proj prompt");
        let p1h = p1.hooks.as_ref().expect("p1.hooks should be Some");
        assert_eq!(p1h.after_create, "echo a");
    }

    // Mirrors Go `TestDecodeNoProjectsLeavesEmpty`.
    #[test]
    fn decode_no_projects_leaves_empty() {
        let c = decode_yaml("", "body");
        assert_eq!(c.repo, "");
        assert!(
            c.projects.is_empty(),
            "projects should be empty when absent"
        );
    }

    // ---- pr_label_test.go / review_test.go / dependency_mode_test.go decode mirrors ----
    // The Encode round-trip halves of these files land in C6; their EffectiveFor/Validate halves
    // live in `projects.rs` / `validate.rs`. These are the pure-Decode cases (the C5 ticket names
    // pr_label_test.go, review_test.go, and the validation half of dependency_mode_test.go).

    // Mirrors Go `TestDecodePRLabelDefault`.
    #[test]
    fn decode_pr_label_default() {
        let c = decode_yaml("", "body");
        assert_eq!(c.pr_label, "symphony", "pr_label default (AIE-301)");
    }

    // Mirrors Go `TestDecodeReviewSummonDefaults`.
    #[test]
    fn decode_review_summon_defaults() {
        let c = decode_yaml("", "");
        assert!(
            c.tracker.review_states.is_empty(),
            "review_states default empty (feature off)"
        );
        assert_eq!(c.tracker.summon_token, "@symphony");
        assert_eq!(c.tracker.review_promote_state, "In Progress");
    }

    // Mirrors Go `TestDecodeReviewSummonOverrides`.
    #[test]
    fn decode_review_summon_overrides() {
        let c = decode_yaml(
            "tracker:\n  review_states:\n    - In Review\n    - Needs QA\n  summon_token: \"@bot\"\n  review_promote_state: Doing\n",
            "",
        );
        assert_eq!(
            c.tracker.review_states,
            vec!["In Review".to_string(), "Needs QA".to_string()]
        );
        assert_eq!(c.tracker.summon_token, "@bot");
        assert_eq!(c.tracker.review_promote_state, "Doing");
    }

    // Mirrors Go `TestDecodeDependencyMode`: dependency_mode + dep_mode_prompt_file decode verbatim
    // at the global (tracker) level and per-project; an absent value stays "" so the effective layer
    // applies the default.
    #[test]
    fn decode_dependency_mode() {
        let c = decode_yaml(
            concat!(
                "tracker:\n  kind: linear\n  api_key: \"$X\"\n  active_states:\n    - Todo\n  terminal_states:\n    - Done\n",
                "  dependency_mode: dag\n  dep_mode_prompt_file: .symphony/CUSTOM.md\n",
                "repo: \"git@github.com:o/r.git\"\n",
                "projects:\n",
                "  - slugs:\n      - a-1\n    dependency_mode: graphite\n    dep_mode_prompt_file: .symphony/A.md\n",
                "  - slugs:\n      - b-1\n",
            ),
            "body",
        );
        assert_eq!(c.tracker.dependency_mode, "dag");
        assert_eq!(c.tracker.dep_mode_prompt_file, ".symphony/CUSTOM.md");
        assert_eq!(c.projects[0].dependency_mode, "graphite");
        assert_eq!(c.projects[0].dep_mode_prompt_file, ".symphony/A.md");
        assert_eq!(c.projects[1].dependency_mode, "", "project[1] inherits");
        assert_eq!(
            c.projects[1].dep_mode_prompt_file, "",
            "project[1] inherits"
        );
    }

    // ---- config_mcp_test.go mirrors (the Decode-related cases; the Encode round-trip is C6) ----

    /// Minimal front matter Decode needs for the MCP cases (tracker + repo), per Go `baseTrackerMCP`.
    fn base_tracker_mcp() -> String {
        "tracker:\n  kind: linear\n  api_key: \"$X\"\n  active_states:\n    - Todo\n  terminal_states:\n    - Done\nrepo: \"git@github.com:o/r.git\"\n".to_string()
    }

    // Mirrors Go `TestMCPDefaults`.
    #[test]
    fn mcp_defaults() {
        let c = decode_yaml(&base_tracker_mcp(), "body {{ issue.identifier }}");
        assert!(c.mcp.enabled, "mcp.enabled default should be true");
        assert!(
            c.mcp.allow_send_message,
            "mcp.allow_send_message default should be true"
        );
        assert!(!c.mcp.allow_stop, "mcp.allow_stop default should be false");
        assert!(
            !c.mcp.allow_resume,
            "mcp.allow_resume default should be false"
        );
        // TRA-242: allow_handoff is a Rhapsody-only knob (NEW beyond Go v0.4.0), default ON.
        assert!(
            c.mcp.allow_handoff,
            "mcp.allow_handoff default should be true"
        );
    }

    // Mirrors Go `TestMCPExplicitRespected` (+ the Rhapsody-only allow_handoff opt-out, TRA-242).
    #[test]
    fn mcp_explicit_respected() {
        let front = base_tracker_mcp()
            + "mcp:\n  enabled: false\n  allow_send_message: false\n  allow_stop: true\n  allow_resume: true\n  allow_handoff: false\n";
        let c = decode_yaml(&front, "body {{ issue.identifier }}");
        assert!(
            !c.mcp.enabled,
            "explicit mcp.enabled:false was re-defaulted"
        );
        assert!(
            !c.mcp.allow_send_message,
            "explicit allow_send_message:false was re-defaulted"
        );
        assert!(c.mcp.allow_stop, "explicit allow_stop:true not respected");
        assert!(
            c.mcp.allow_resume,
            "explicit allow_resume:true not respected"
        );
        assert!(
            !c.mcp.allow_handoff,
            "explicit allow_handoff:false was re-defaulted"
        );
    }

    // ---- parseOptionalDuration / parse_go_duration helper coverage ----
    // These verify the duration parser that decode wires in (Go `parseOptionalDuration`, called by
    // Decode). The decode-integration mirrors of claim_mode_test.go land with Resolve (C4).

    // An empty/blank duration knob is zero (Go: empty ⇒ 0, default materialized later).
    #[test]
    fn parse_optional_duration_empty_is_zero() {
        assert_eq!(
            parse_optional_duration("", "claim_ttl").unwrap(),
            Duration::nanoseconds(0)
        );
        assert_eq!(
            parse_optional_duration("   ", "claim_settle_delay").unwrap(),
            Duration::nanoseconds(0)
        );
    }

    // Single units, compound forms (the "2m0s" Encode emits), fractions, sign, and "0".
    #[test]
    fn parse_go_duration_matches_go_semantics() {
        assert_eq!(parse_go_duration("90s").unwrap(), Duration::seconds(90));
        assert_eq!(
            parse_go_duration("500ms").unwrap(),
            Duration::milliseconds(500)
        );
        assert_eq!(parse_go_duration("2m").unwrap(), Duration::minutes(2));
        assert_eq!(parse_go_duration("2m0s").unwrap(), Duration::minutes(2));
        assert_eq!(parse_go_duration("1.5h").unwrap(), Duration::minutes(90));
        assert_eq!(parse_go_duration("-1s").unwrap(), Duration::seconds(-1));
        assert_eq!(parse_go_duration("0").unwrap(), Duration::nanoseconds(0));
    }

    // Malformed values are errors, exactly as Go `time.ParseDuration` rejects them.
    #[test]
    fn parse_go_duration_rejects_malformed() {
        for bad in ["not-a-duration", "5x", "", "m", "1.2.3s"] {
            assert!(
                parse_go_duration(bad).is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    // The wrapped error carries the sentinel token + field name (the observable contract), so a
    // malformed claim_ttl surfaces identically to the Go daemon.
    #[test]
    fn parse_optional_duration_error_has_sentinel_and_field() {
        let err = parse_optional_duration("not-a-duration", "claim_ttl").unwrap_err();
        assert!(
            err.to_string()
                .starts_with("workflow_parse_error: claim_ttl: "),
            "unexpected error string: {err}"
        );
    }
}
