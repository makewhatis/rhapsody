//! `encode` — parity port of Go `internal/config/encode.go`.
//!
//! [`encode`] is the inverse of [`crate::decode::decode`]: it serializes a typed [`Config`] back
//! into a workflow [`Definition`] (front-matter map + prompt body) that re-parses, via `decode`, to
//! an EQUIVALENT `Config`. Equality holds at the RESOLVED level (`resolve_projects`), not always
//! field-for-field: a single trivial agent is canonicalized to the legacy single-slug form (see
//! [`collapsible_to_single`]), so verbose `projects:` input for that case re-decodes to the
//! collapsed shape — same resolved projects, different raw `Config`. The dashboard config editor
//! persists the result with [`crate::workflow::save`] (INF-224).
//!
//! Strategy — a faithful mirror of Go's `Encode`: build the yaml-tagged [`Raw`] mirror from the
//! `Config` ([`raw_from_config`]), serialize it to a generic YAML value, then [`prune_empty`] the
//! tree. Defaulted scalars are emitted verbatim (`decode` re-reads them as no-op defaults);
//! per-project override blocks and the `enabled` flag are emitted only when set, so an unset
//! (inherit) field stays `None`/empty after the round-trip. Go's `raw` struct carries NO
//! `omitempty` tags — every field marshals, and `prune_empty` does ALL the trimming — so this port
//! serializes the whole [`Raw`] and prunes afterward, exactly as `yaml.Marshal` + `pruneEmpty` do.
//!
//! Form selection ([`raw_from_config`]):
//!   - no projects (legacy) OR exactly one "trivial" agent (single slug, no name, no overrides,
//!     enabled) ⇒ the single-project form (`tracker.project_slug` + top-level `repo`, no
//!     `projects:` list);
//!   - otherwise ⇒ the `projects:` list, each entry emitting only its set fields.

use chrono::Duration;
use serde_yaml_ng::{Mapping, Value};

use crate::decode::ConfigError;
use crate::model::{Config, Project, Raw, RawClaudeOverride, RawHooks, RawProject};
use crate::workflow::Definition;

/// Serializes a typed [`Config`] into a workflow [`Definition`] whose front matter re-decodes to an
/// equivalent config (Go `config.Encode`). Errors only if the raw mirror fails to (re)serialize,
/// surfaced with Go's `workflow_parse_error` sentinel token (the same one `decode` uses).
pub fn encode(c: &Config) -> Result<Definition, ConfigError> {
    let raw = raw_from_config(c);
    // Mirror Go's `yaml.Marshal(raw)` → `yaml.Unmarshal(bytes, &m)`: serialize the typed mirror,
    // then re-read it as a generic value tree so `prune_empty` can walk it uniformly.
    let text = serde_yaml_ng::to_string(&raw).map_err(|e| ConfigError::Parse(e.to_string()))?;
    let value: Value =
        serde_yaml_ng::from_str(&text).map_err(|e| ConfigError::Parse(e.to_string()))?;
    // `pruned` is a map, or empty when everything pruned away (Go's `front, _ := pruned.(map)` +
    // nil-guard). `pr_label` defaults to "symphony" (non-empty) so the map is never actually empty.
    let config = match prune_empty(value) {
        Some(Value::Mapping(m)) => m,
        _ => Mapping::new(),
    };
    Ok(Definition {
        config,
        prompt_template: c.prompt_template.clone(),
    })
}

/// Builds the [`Raw`] mirror from a typed [`Config`] (Go `rawFromConfig`), choosing the single-
/// project vs `projects:` form. The always-emit numeric knobs are wrapped in `Some(..)` (Go's
/// `ptrOf`) so a defaulted value survives as a no-op default on re-decode; the presence-flag knobs
/// (`github_summons`, `ultracode`, `claim_ttl`, `claim_settle_delay`) are set only when active so an
/// absent/false knob is pruned.
fn raw_from_config(c: &Config) -> Raw {
    let mut r = Raw::default();

    r.tracker.kind = c.tracker.kind.clone();
    r.tracker.endpoint = c.tracker.endpoint.clone();
    r.tracker.api_key = c.tracker.api_key.clone();
    r.tracker.source = c.tracker.source.clone();
    r.tracker.active_states = c.tracker.active_states.clone();
    r.tracker.terminal_states = c.tracker.terminal_states.clone();
    r.tracker.canceled_states = c.tracker.canceled_states.clone();
    r.tracker.review_states = c.tracker.review_states.clone();
    r.tracker.summon_token = c.tracker.summon_token.clone();
    // github_summons defaults to false; emit only when true so an absent/false knob is pruned.
    if c.tracker.github_summons {
        r.tracker.github_summons = Some(true);
    }
    r.tracker.review_promote_state = c.tracker.review_promote_state.clone();
    r.tracker.milestone = c.tracker.milestone.clone();
    r.tracker.labels = c.tracker.labels.clone();
    r.tracker.dependency_mode = c.tracker.dependency_mode.clone();
    r.tracker.dep_mode_prompt_file = c.tracker.dep_mode_prompt_file.clone();
    r.tracker.claim_mode = c.tracker.claim_mode.clone();
    // claim_ttl / claim_settle_delay: emit the Go-duration string form only for a non-zero value,
    // so an unset knob (the assignee-mode common case) is pruned and re-decodes as "use the default".
    if c.tracker.claim_ttl > Duration::nanoseconds(0) {
        r.tracker.claim_ttl = go_duration_string(c.tracker.claim_ttl);
    }
    if c.tracker.claim_settle_delay > Duration::nanoseconds(0) {
        r.tracker.claim_settle_delay = go_duration_string(c.tracker.claim_settle_delay);
    }

    r.polling.interval_ms = Some(c.polling.interval_ms);

    r.workspace.root = c.workspace.root.clone();

    r.hooks.after_create = c.hooks.after_create.clone();
    r.hooks.before_run = c.hooks.before_run.clone();
    r.hooks.after_run = c.hooks.after_run.clone();
    r.hooks.before_remove = c.hooks.before_remove.clone();
    r.hooks.timeout_ms = Some(c.hooks.timeout_ms);

    r.agent.backend = c.agent.backend.clone();
    r.agent.max_concurrent_agents = Some(c.agent.max_concurrent_agents);
    r.agent.max_turns = Some(c.agent.max_turns);
    r.agent.max_retry_backoff_ms = Some(c.agent.max_retry_backoff_ms);
    r.agent.handoff_drain_grace_ms = Some(c.agent.handoff_drain_grace_ms);
    if !c.agent.max_concurrent_agents_by_state.is_empty() {
        r.agent.max_concurrent_agents_by_state = c
            .agent
            .max_concurrent_agents_by_state
            .iter()
            .map(|(k, v)| (k.clone(), Value::from(*v)))
            .collect();
    }

    r.codex.command = c.codex.command.clone();
    r.codex.approval_policy = c.codex.approval_policy.clone();
    r.codex.thread_sandbox = c.codex.thread_sandbox.clone();
    r.codex.turn_sandbox_policy = c.codex.turn_sandbox_policy.clone();
    r.codex.turn_timeout_ms = Some(c.codex.turn_timeout_ms);
    r.codex.read_timeout_ms = Some(c.codex.read_timeout_ms);
    r.codex.stall_timeout_ms = Some(c.codex.stall_timeout_ms);

    r.claude.command = c.claude.command.clone();
    r.claude.model = c.claude.model.clone();
    r.claude.effort = c.claude.effort.clone();
    r.claude.permission_mode = c.claude.permission_mode.clone();
    r.claude.allowed_tools = c.claude.allowed_tools.clone();
    r.claude.disallowed_tools = c.claude.disallowed_tools.clone();
    r.claude.mcp_config = c.claude.mcp_config.clone();
    r.claude.setting_sources = c.claude.setting_sources.clone();
    r.claude.add_dirs = c.claude.add_dirs.clone();
    r.claude.turn_timeout_ms = Some(c.claude.turn_timeout_ms);
    r.claude.read_timeout_ms = Some(c.claude.read_timeout_ms);
    r.claude.stall_timeout_ms = Some(c.claude.stall_timeout_ms);
    r.claude.extra_args = c.claude.extra_args.clone();
    r.claude.billing_guard = c.claude.billing_guard;
    // ultracode defaults to false; emit only when true so an unset/false knob is pruned.
    if c.claude.ultracode {
        r.claude.ultracode = Some(true);
    }

    r.server.port = c.server.port;
    r.logging.dir = c.logging.dir.clone();
    r.storage.path = c.storage.path.clone();
    r.storage.retention_days = c.storage.retention_days;

    // Otel/MCP presence-pointers: emit the resolved bool as a non-null value so `prune_empty` keeps
    // it and a Settings save always round-trips the explicit enabled/toggle values (INF-442/INF-473).
    r.otel.enabled = Some(c.otel.enabled);
    r.otel.endpoint = c.otel.endpoint.clone();
    r.otel.protocol = c.otel.protocol.clone();
    r.otel.service_name = c.otel.service_name.clone();
    r.otel.headers = c.otel.headers.clone();
    r.otel.insecure = c.otel.insecure;
    r.otel.operator = c.otel.operator.clone(); // must round-trip so a save never drops an explicit operator

    r.mcp.enabled = Some(c.mcp.enabled);
    r.mcp.allow_send_message = Some(c.mcp.allow_send_message);
    r.mcp.allow_stop = Some(c.mcp.allow_stop);
    r.mcp.allow_resume = Some(c.mcp.allow_resume);

    r.repo = c.repo.clone();
    r.prompt_file = c.prompt_file.clone();
    r.git_flow = c.git_flow.clone();
    r.workspace_mode = c.workspace_mode.clone();
    r.pr_label = c.pr_label.clone(); // defaulted "symphony" is non-empty, survives pruning (AIE-301)
    r.tracker.project_slug = c.tracker.project_slug.clone();

    if collapsible_to_single(c) {
        // Exactly one trivial agent: emit the clean single-project form.
        let p = &c.projects[0];
        r.tracker.project_slug = p.slugs[0].clone();
        if !p.repo.is_empty() {
            r.repo = p.repo.clone();
        }
    } else if !c.projects.is_empty() {
        r.projects = c.projects.iter().map(raw_project_from_project).collect();
    }

    r
}

/// Reports whether `c` is exactly one agent the single-project form can represent without loss (Go
/// `collapsibleToSingle`): a single slug, no display name, no per-project overrides, NO explicitly-
/// set `enabled` flag, and no repo distinct from the top-level `repo`. A named / paused / multi-slug
/// / overriding project must keep the `projects:` form. The `enabled` guard requires the flag to be
/// UNSET (`None`): an explicit `enabled: true` is a set field, so collapsing it (which emits no
/// enabled key) would drop it and break the Decode→Encode→Decode equality.
fn collapsible_to_single(c: &Config) -> bool {
    if c.projects.len() != 1 {
        return false;
    }
    let p = &c.projects[0];
    p.slugs.len() == 1
        && p.name.is_empty()
        && (p.repo.is_empty() || p.repo == c.repo)
        && p.active_states.is_empty()
        && p.terminal_states.is_empty()
        && p.canceled_states.is_empty()
        && p.review_states.is_empty()
        && p.prompt.is_empty()
        && p.prompt_file.is_empty()
        && p.git_flow.is_empty()
        && p.workspace_mode.is_empty()
        && p.claude.is_none()
        && p.hooks.is_none()
        && p.max_concurrent_agents.is_none()
        && p.milestone.is_empty()
        && p.labels.is_empty()
        && p.dependency_mode.is_empty()
        && p.dep_mode_prompt_file.is_empty()
        && p.claim_mode.is_empty()
        && p.enabled.is_none()
}

/// Maps one typed [`Project`] to a [`RawProject`] (Go `rawProjectFromProject`), the inverse of
/// `decode`'s per-project field copy. Override pointers/slices are copied verbatim (`None`/empty ⇒
/// inherit) so `prune_empty` drops them and the round-trip preserves inherit-vs-set fidelity.
fn raw_project_from_project(p: &Project) -> RawProject {
    let mut rp = RawProject {
        name: p.name.clone(),
        repo: p.repo.clone(),
        slugs: p.slugs.clone(),
        active_states: p.active_states.clone(),
        terminal_states: p.terminal_states.clone(),
        canceled_states: p.canceled_states.clone(),
        review_states: p.review_states.clone(),
        prompt: p.prompt.clone(),
        prompt_file: p.prompt_file.clone(),
        git_flow: p.git_flow.clone(),
        workspace_mode: p.workspace_mode.clone(),
        dependency_mode: p.dependency_mode.clone(),
        dep_mode_prompt_file: p.dep_mode_prompt_file.clone(),
        claim_mode: p.claim_mode.clone(),
        claude: None,
        hooks: None,
        max_concurrent_agents: p.max_concurrent_agents,
        milestone: p.milestone.clone(),
        labels: p.labels.clone(),
        enabled: p.enabled,
    };
    if let Some(ov) = &p.claude {
        rp.claude = Some(RawClaudeOverride {
            command: ov.command.clone(),
            model: ov.model.clone(),
            effort: ov.effort.clone(),
            permission_mode: ov.permission_mode.clone(),
            allowed_tools: ov.allowed_tools.clone(),
            disallowed_tools: ov.disallowed_tools.clone(),
            mcp_config: ov.mcp_config.clone(),
            setting_sources: ov.setting_sources.clone(),
            add_dirs: ov.add_dirs.clone(),
            turn_timeout_ms: ov.turn_timeout_ms,
            read_timeout_ms: ov.read_timeout_ms,
            stall_timeout_ms: ov.stall_timeout_ms,
            extra_args: ov.extra_args.clone(),
            billing_guard: ov.billing_guard,
            ultracode: ov.ultracode,
        });
    }
    if let Some(h) = &p.hooks {
        rp.hooks = Some(RawHooks {
            after_create: h.after_create.clone(),
            before_run: h.before_run.clone(),
            after_run: h.after_run.clone(),
            before_remove: h.before_remove.clone(),
            timeout_ms: Some(h.timeout_ms),
        });
    }
    rp
}

/// Recursively drops `null`, empty-string, empty-sequence and empty-map values from a decoded YAML
/// value, returning `Some(pruned)` or `None` when the whole value is empty (Go `pruneEmpty`).
/// Numbers (including `0`) and booleans (including `false`) are ALWAYS kept, so an explicit zero/
/// false survives the round-trip; only genuinely absent/empty values are removed, so `decode` reads
/// them as unset and applies its defaults (or preserves `None` for inherit-vs-set overrides).
fn prune_empty(v: Value) -> Option<Value> {
    match v {
        Value::Null => None,
        Value::String(s) if s.is_empty() => None,
        Value::Mapping(m) => {
            let mut out = Mapping::new();
            for (k, vv) in m {
                if let Some(pv) = prune_empty(vv) {
                    out.insert(k, pv);
                }
            }
            if out.is_empty() {
                None
            } else {
                Some(Value::Mapping(out))
            }
        }
        Value::Sequence(seq) => {
            let mut out = Vec::with_capacity(seq.len());
            for vv in seq {
                if let Some(pv) = prune_empty(vv) {
                    out.push(pv);
                }
            }
            if out.is_empty() {
                None
            } else {
                Some(Value::Sequence(out))
            }
        }
        // Numbers (incl 0), booleans (incl false), non-empty strings, and tagged values are kept.
        other => Some(other),
    }
}

/// Port of Go `time.Duration.String()`: renders a duration the way Go does (2 minutes ⇒ `"2m0s"`,
/// 1 second ⇒ `"1s"`, 1500 ms ⇒ `"1.5s"`, 500 ms ⇒ `"500ms"`, 0 ⇒ `"0s"`). The exact inverse of
/// [`crate::decode`]'s `parse_go_duration`, so a `claim_ttl` / `claim_settle_delay` knob round-trips
/// through Encode → Decode. Nanosecond-exact; builds the text from the right into a fixed buffer,
/// mirroring Go's `[32]byte` + descending write index.
fn go_duration_string(d: Duration) -> String {
    const NS_PER_US: u64 = 1_000;
    const NS_PER_MS: u64 = 1_000_000;
    const NS_PER_S: u64 = 1_000_000_000;

    // A config duration always fits i64 ns; fall back to 0 defensively (never hit in practice).
    let total = d.num_nanoseconds().unwrap_or(0);
    let neg = total < 0;
    let mut u: u64 = total.unsigned_abs();

    let mut buf = [0u8; 32];
    let mut w = buf.len();

    if u < NS_PER_S {
        // Sub-second: pick the ns / µs / ms unit and precision.
        let prec: usize;
        w -= 1;
        buf[w] = b's';
        w -= 1;
        if u == 0 {
            return "0s".to_string();
        } else if u < NS_PER_US {
            prec = 0;
            buf[w] = b'n';
        } else if u < NS_PER_MS {
            prec = 3;
            // "µs": the micro sign is two UTF-8 bytes (0xC2 0xB5); make room for the extra byte.
            w -= 1;
            buf[w..w + 3].copy_from_slice("µs".as_bytes());
        } else {
            prec = 6;
            buf[w] = b'm';
        }
        let (nw, nu) = fmt_frac(&mut buf, w, u, prec);
        w = nw;
        u = nu;
        w = fmt_int(&mut buf, w, u);
    } else {
        w -= 1;
        buf[w] = b's';
        let (nw, nu) = fmt_frac(&mut buf, w, u, 9);
        w = nw;
        u = nu;
        // u is now whole seconds.
        w = fmt_int(&mut buf, w, u % 60);
        u /= 60;
        if u > 0 {
            w -= 1;
            buf[w] = b'm';
            w = fmt_int(&mut buf, w, u % 60);
            u /= 60;
            if u > 0 {
                w -= 1;
                buf[w] = b'h';
                w = fmt_int(&mut buf, w, u);
            }
        }
    }

    if neg {
        w -= 1;
        buf[w] = b'-';
    }
    // buf[w..] is the ASCII/UTF-8 we wrote; lossy conversion never alters it (kept panic-free).
    String::from_utf8_lossy(&buf[w..]).into_owned()
}

/// Go `fmtFrac`: writes the fraction of `v` (up to `prec` digits, trailing zeros and a bare decimal
/// point omitted) into `buf` ending at index `w`, returning the new write index and `v / 10^prec`.
fn fmt_frac(buf: &mut [u8], mut w: usize, mut v: u64, prec: usize) -> (usize, u64) {
    let mut print = false;
    for _ in 0..prec {
        let digit = v % 10;
        print = print || digit != 0;
        if print {
            w -= 1;
            buf[w] = (digit as u8) + b'0';
        }
        v /= 10;
    }
    if print {
        w -= 1;
        buf[w] = b'.';
    }
    (w, v)
}

/// Go `fmtInt`: writes the decimal digits of `v` (a bare `0` when zero) into `buf` ending at index
/// `w`, returning the new write index.
fn fmt_int(buf: &mut [u8], mut w: usize, mut v: u64) -> usize {
    if v == 0 {
        w -= 1;
        buf[w] = b'0';
    } else {
        while v > 0 {
            w -= 1;
            buf[w] = (v % 10) as u8 + b'0';
            v /= 10;
        }
    }
    w
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::decode;
    use crate::projects::{effective_for, resolve_projects};
    use crate::workflow::YamlMap;

    /// Decode a YAML front-matter string + prompt body into a [`Config`] (Go `decodeMap`, which
    /// passes an equivalent `map[string]any`). Empty front matter decodes an empty map.
    fn decode_map(front: &str, body: &str) -> Config {
        let config: YamlMap = if front.trim().is_empty() {
            YamlMap::new()
        } else {
            serde_yaml_ng::from_str(front).expect("front matter must parse")
        };
        let def = Definition {
            config,
            prompt_template: body.to_string(),
        };
        decode(&def).expect("decode should succeed")
    }

    /// Run `c` through Encode then Decode, asserting the serializer is the faithful inverse of
    /// Decode (Go `reEncodeDecode`).
    fn re_encode_decode(c: &Config) -> Config {
        let def = encode(c).expect("encode");
        decode(&def).expect("re-decode")
    }

    /// Look up a nested `config[outer][inner]` value in an encoded front-matter map.
    fn nested<'a>(cfg: &'a YamlMap, outer: &str, inner: &str) -> Option<&'a Value> {
        cfg.get(outer)
            .and_then(Value::as_mapping)
            .and_then(|m| m.get(inner))
    }

    // Mirrors Go `TestEncodeFileTrackerSourceRoundTrip`.
    #[test]
    fn file_tracker_source_round_trip() {
        let c1 = decode_map(
            "tracker:\n  kind: file\n  source: /tmp/smoke-issues.json\n",
            "do {{ issue.identifier }}",
        );
        assert_eq!(c1.tracker.source, "/tmp/smoke-issues.json");
        let c2 = re_encode_decode(&c1);
        assert_eq!(c2.tracker.kind, "file");
        assert_eq!(c2.tracker.source, "/tmp/smoke-issues.json");
    }

    // Mirrors Go `TestEncodeMultiProjectRoundTripStable`.
    #[test]
    fn multi_project_round_trip_stable() {
        let front = concat!(
            "tracker:\n",
            "  kind: linear\n",
            "  api_key: \"$LINEAR_API_KEY\"\n",
            "  active_states: [Todo, In Progress]\n",
            "  terminal_states: [Done, Canceled]\n",
            "  review_states: [In Review]\n",
            "  review_promote_state: In Progress\n",
            "  summon_token: \"@symphony\"\n",
            "repo: git@github.com:o/default.git\n",
            "agent:\n  max_concurrent_agents: 5\n  max_turns: 30\n",
            "claude:\n  model: claude-opus-4-8\n  permission_mode: bypassPermissions\n",
            "projects:\n",
            "  - name: Infra\n",
            "    slugs: [infra-1, infra-2]\n",
            "    repo: git@github.com:o/infra.git\n",
            "    milestone: \"David's Tasks\"\n",
            "    claude:\n      model: claude-sonnet-4-6\n",
            "    active_states: [Todo, Started]\n",
            "  - name: Web\n    slugs: [web-1]\n    enabled: false\n    max_concurrent_agents: 2\n",
            "  - slugs: [api-1]\n",
        );
        let c1 = decode_map(front, "global body {{ issue.identifier }}");
        let c2 = re_encode_decode(&c1);
        assert_eq!(c1, c2, "3-agent config not stable through Encode->Decode");
    }

    // Mirrors Go `TestEncodeUltracodeRoundTrip`.
    #[test]
    fn ultracode_round_trip() {
        let front = concat!(
            "tracker:\n  kind: linear\n  api_key: \"$X\"\n  active_states: [Todo]\n  terminal_states: [Done]\n",
            "repo: git@github.com:o/r.git\n",
            "claude:\n  model: opus\n  ultracode: true\n",
            "projects:\n",
            "  - slugs: [a-1]\n    claude:\n      ultracode: false\n",
            "  - slugs: [b-1]\n",
        );
        let c1 = decode_map(front, "body");
        assert!(c1.claude.ultracode, "global ultracode should decode true");
        let def = encode(&c1).expect("encode");
        // The global true must be emitted so it survives a future re-decode.
        assert_eq!(
            nested(&def.config, "claude", "ultracode"),
            Some(&Value::Bool(true)),
            "global ultracode true must be emitted"
        );
        let c2 = re_encode_decode(&c1);
        assert_eq!(c1, c2, "ultracode config not stable");
        // Project 0 pinned ultracode=false (a non-None override); project 1 inherits (None).
        let p0 = c2.projects[0].claude.as_ref().expect("p0 claude");
        assert_eq!(
            p0.ultracode,
            Some(false),
            "project[0] override (false) lost"
        );
        if let Some(p1) = &c2.projects[1].claude {
            assert_eq!(p1.ultracode, None, "project[1] ultracode must stay None");
        }
        // Effective: the inheriting project sees the global true; the overriding one sees false.
        assert!(
            effective_for(&c1, Some(&c1.projects[1])).claude.ultracode,
            "inheriting project should resolve ultracode=true"
        );
        assert!(
            !effective_for(&c1, Some(&c1.projects[0])).claude.ultracode,
            "overriding project should resolve ultracode=false"
        );
    }

    // Mirrors Go `TestEncodeOtelOperatorRoundTrip`.
    #[test]
    fn otel_operator_round_trip() {
        let front = concat!(
            "tracker:\n  kind: linear\n  api_key: \"$X\"\n  active_states: [Todo]\n  terminal_states: [Done]\n",
            "repo: git@github.com:o/r.git\n",
            "otel:\n  enabled: true\n  operator: fleet-1\n",
        );
        let c1 = decode_map(front, "body");
        assert_eq!(c1.otel.operator, "fleet-1");
        let def = encode(&c1).expect("encode");
        assert_eq!(
            nested(&def.config, "otel", "operator"),
            Some(&Value::String("fleet-1".to_string())),
            "encoded otel.operator must be fleet-1"
        );
        assert_eq!(re_encode_decode(&c1).otel.operator, "fleet-1");
    }

    // Mirrors Go `TestEncodeOtelEnabledRoundTrip`.
    #[test]
    fn otel_enabled_round_trip() {
        for enabled in [true, false] {
            let front = format!(
                concat!(
                    "tracker:\n  kind: linear\n  api_key: \"$X\"\n  active_states: [Todo]\n  terminal_states: [Done]\n",
                    "repo: git@github.com:o/r.git\n",
                    "otel:\n  enabled: {}\n",
                ),
                enabled
            );
            let c1 = decode_map(&front, "body");
            assert_eq!(c1.otel.enabled, enabled);
            let def = encode(&c1).expect("encode");
            assert_eq!(
                nested(&def.config, "otel", "enabled"),
                Some(&Value::Bool(enabled)),
                "prune_empty must keep the explicit otel.enabled"
            );
            assert_eq!(re_encode_decode(&c1).otel.enabled, enabled);
        }
    }

    // Mirrors Go `TestEncodePromptFileRoundTrip`.
    #[test]
    fn prompt_file_round_trip() {
        let front = concat!(
            "tracker:\n  kind: linear\n  api_key: \"$X\"\n  active_states: [Todo]\n  terminal_states: [Done]\n",
            "repo: git@github.com:o/r.git\n",
            "prompt_file: prompts/global.md\n",
            "projects:\n",
            "  - slugs: [a-1]\n    prompt_file: prompts/a.md\n",
            "  - slugs: [b-1]\n",
        );
        let c1 = decode_map(front, "inline body {{ issue.identifier }}");
        assert_eq!(c1.prompt_file, "prompts/global.md");
        let def = encode(&c1).expect("encode");
        assert_eq!(
            def.config.get("prompt_file"),
            Some(&Value::String("prompts/global.md".to_string())),
            "global prompt_file must be emitted"
        );
        let c2 = re_encode_decode(&c1);
        assert_eq!(c1, c2, "prompt_file config not stable");
        assert_eq!(c2.projects[0].prompt_file, "prompts/a.md");
        assert_eq!(
            c2.projects[1].prompt_file, "",
            "project[1] must stay inherit"
        );
        // Effective resolution: inheriting project sees the global path; overriding one its own.
        assert_eq!(
            effective_for(&c1, Some(&c1.projects[1])).prompt_file,
            "prompts/global.md"
        );
        assert_eq!(
            effective_for(&c1, Some(&c1.projects[0])).prompt_file,
            "prompts/a.md"
        );
    }

    // Mirrors Go `TestEncodePromptFileProjectDoesNotCollapse`.
    #[test]
    fn prompt_file_project_does_not_collapse() {
        let front = concat!(
            "tracker:\n  kind: linear\n  api_key: \"$X\"\n  active_states: [Todo]\n  terminal_states: [Done]\n",
            "repo: git@github.com:o/r.git\n",
            "projects:\n  - slugs: [solo]\n    prompt_file: prompts/solo.md\n",
        );
        let c1 = decode_map(front, "body");
        let def = encode(&c1).expect("encode");
        assert!(
            def.config.contains_key("projects"),
            "a project carrying a prompt_file override must stay in projects: form"
        );
        assert_eq!(re_encode_decode(&c1), c1, "prompt_file project not stable");
    }

    // Mirrors Go `TestEncodePreservesInheritVsOverride`.
    #[test]
    fn preserves_inherit_vs_override() {
        let front = concat!(
            "tracker:\n  kind: linear\n  api_key: \"$X\"\n  active_states: [Todo]\n  terminal_states: [Done]\n",
            "repo: git@github.com:o/r.git\n",
            "claude:\n  model: opus\n  effort: high\n",
            "projects:\n  - slugs: [solo]\n    claude:\n      model: sonnet\n",
        );
        let c1 = decode_map(front, "body");
        let def = encode(&c1).expect("encode");
        assert!(
            def.config.contains_key("projects"),
            "a project carrying an override must serialize in projects: form"
        );
        let c2 = re_encode_decode(&c1);
        assert_eq!(c1, c2, "override config not stable");
        let p0 = c2.projects[0].claude.as_ref().expect("p0 claude");
        assert_eq!(p0.model.as_deref(), Some("sonnet"), "overridden model lost");
        assert_eq!(p0.effort, None, "unset override must stay None (inherit)");
    }

    // Mirrors Go `TestEncodeLegacySingleProjectRoundTrip`.
    #[test]
    fn legacy_single_project_round_trip() {
        let front = concat!(
            "tracker:\n  kind: linear\n  api_key: \"$X\"\n  project_slug: my-proj\n",
            "  active_states: [Todo]\n  terminal_states: [Done]\n",
        );
        let c1 = decode_map(front, "do the work");
        let def = encode(&c1).expect("encode");
        assert!(
            !def.config.contains_key("projects"),
            "a legacy single-project config must not emit a projects: list"
        );
        assert_eq!(
            nested(&def.config, "tracker", "project_slug"),
            Some(&Value::String("my-proj".to_string()))
        );
        assert_eq!(re_encode_decode(&c1), c1, "legacy config not stable");
    }

    // Mirrors Go `TestEncodeCollapsesSingleTrivialProject`.
    #[test]
    fn collapses_single_trivial_project() {
        let front = concat!(
            "tracker:\n  kind: linear\n  api_key: \"$X\"\n  active_states: [Todo]\n  terminal_states: [Done]\n",
            "repo: git@github.com:o/r.git\n",
            "projects:\n  - slugs: [solo]\n",
        );
        let c1 = decode_map(front, "body");
        let def = encode(&c1).expect("encode");
        assert!(
            !def.config.contains_key("projects"),
            "a single trivial agent should collapse (no projects:)"
        );
        assert_eq!(
            nested(&def.config, "tracker", "project_slug"),
            Some(&Value::String("solo".to_string()))
        );
        assert_eq!(
            def.config.get("repo"),
            Some(&Value::String("git@github.com:o/r.git".to_string())),
            "top-level repo must be the project's repo"
        );
        // The collapsed form resolves to the same single routed project.
        let c2 = decode_map(
            &serde_yaml_ng::to_string(&def.config).unwrap(),
            &def.prompt_template,
        );
        assert_eq!(
            resolve_projects(&c1),
            resolve_projects(&c2),
            "resolved projects differ after collapse"
        );
    }

    // Mirrors Go `TestDecodeEmptyClaudeBlockIsNil` (the Encode canonicalization half).
    #[test]
    fn empty_claude_block_collapses_and_is_stable() {
        let front = concat!(
            "tracker:\n  kind: linear\n  api_key: \"$X\"\n  active_states: [Todo]\n  terminal_states: [Done]\n",
            "repo: git@github.com:o/r.git\n",
            "projects:\n  - slugs: [solo]\n    claude: {}\n",
        );
        let c = decode_map(front, "body");
        assert_eq!(c.projects.len(), 1);
        assert!(
            c.projects[0].claude.is_none(),
            "all-empty claude: {{}} must decode to None"
        );
        let def1 = encode(&c).expect("encode");
        assert!(
            !def1.config.contains_key("projects"),
            "empty claude override must not block collapse on the first Encode"
        );
        // The on-disk shape must be stable across a second Encode->Decode->Encode pass.
        let c2 = decode_map(
            &serde_yaml_ng::to_string(&def1.config).unwrap(),
            &def1.prompt_template,
        );
        let def2 = encode(&c2).expect("re-encode");
        assert_eq!(
            def1.config, def2.config,
            "on-disk shape oscillated across saves"
        );
    }

    // Mirrors Go `TestEncodeDoesNotCollapseNamedOrPausedProject`.
    #[test]
    fn does_not_collapse_named_or_paused_project() {
        let cases = [
            ("named", "name: Bot\n    slugs: [solo]\n"),
            ("paused", "slugs: [solo]\n    enabled: false\n"),
            (
                "explicit-enabled-true",
                "slugs: [solo]\n    enabled: true\n",
            ),
            ("multi-slug", "slugs: [a, b]\n"),
            (
                "distinct-repo",
                "slugs: [solo]\n    repo: git@github.com:o/other.git\n",
            ),
        ];
        for (desc, proj) in cases {
            let front = format!(
                concat!(
                    "tracker:\n  kind: linear\n  api_key: \"$X\"\n  active_states: [Todo]\n  terminal_states: [Done]\n",
                    "repo: git@github.com:o/r.git\n",
                    "projects:\n  - {}",
                ),
                proj
            );
            let c1 = decode_map(&front, "body");
            let def = encode(&c1).expect("encode");
            assert!(
                def.config.contains_key("projects"),
                "{desc} project must stay in projects: form"
            );
            assert_eq!(re_encode_decode(&c1), c1, "{desc} project not stable");
        }
    }

    // Mirrors Go `TestEncodeLabelsRoundTrip` + `TestEffectiveLabelsOverride`.
    #[test]
    fn labels_round_trip_and_effective_override() {
        let front = concat!(
            "tracker:\n  kind: linear\n  api_key: \"$X\"\n  active_states: [Todo]\n  terminal_states: [Done]\n",
            "  labels: [global-label, other]\n",
            "repo: git@github.com:o/r.git\n",
            "projects:\n  - slugs: [a]\n    labels: [proj-label]\n  - slugs: [b]\n",
        );
        let c1 = decode_map(front, "body");
        assert_eq!(c1.tracker.labels, vec!["global-label", "other"]);
        assert_eq!(c1.projects[0].labels, vec!["proj-label"]);
        assert!(c1.projects[1].labels.is_empty(), "project[1] inherits");
        // Effective override vs inherit.
        assert_eq!(
            effective_for(&c1, Some(&c1.projects[0])).labels,
            vec!["proj-label"]
        );
        assert_eq!(
            effective_for(&c1, Some(&c1.projects[1])).labels,
            vec!["global-label", "other"]
        );
        // Round-trip preserves both.
        let c2 = re_encode_decode(&c1);
        assert_eq!(c1, c2, "labels config not stable");
        assert_eq!(c2.projects[0].labels, vec!["proj-label"]);
        assert!(c2.projects[1].labels.is_empty());
    }

    // Mirrors Go `TestEncodeLabelsProjectDoesNotCollapse`.
    #[test]
    fn labels_project_does_not_collapse() {
        let front = concat!(
            "tracker:\n  kind: linear\n  api_key: \"$X\"\n  active_states: [Todo]\n  terminal_states: [Done]\n",
            "repo: git@github.com:o/r.git\n",
            "projects:\n  - slugs: [solo]\n    labels: [must-survive]\n",
        );
        let c1 = decode_map(front, "body");
        let def = encode(&c1).expect("encode");
        assert!(
            def.config.contains_key("projects"),
            "a project carrying a labels override must stay in projects: form"
        );
        assert_eq!(re_encode_decode(&c1), c1, "labels-only project not stable");
    }

    // Mirrors Go `TestGitHubSummonsDefaultAndRoundTrip`.
    #[test]
    fn github_summons_default_and_round_trip() {
        // Default false when absent.
        let c = decode_map(
            "repo: git@github.com:o/r.git\ntracker:\n  kind: linear\n  summon_token: \"@symphony\"\n",
            "",
        );
        assert!(!c.tracker.github_summons, "default false when key absent");
        // Explicit true survives encode->decode.
        let c2 = decode_map(
            "repo: git@github.com:o/r.git\ntracker:\n  kind: linear\n  github_summons: true\n",
            "",
        );
        assert!(c2.tracker.github_summons);
        assert!(
            re_encode_decode(&c2).tracker.github_summons,
            "github_summons=true must survive round-trip"
        );
    }

    // Locks the Go-duration formatter (inverse of decode's parse_go_duration) so a claim_ttl /
    // claim_settle_delay knob round-trips. Exact strings + a parse round-trip on each.
    #[test]
    fn go_duration_string_matches_go() {
        let cases = [
            (Duration::minutes(2), "2m0s"),
            (Duration::seconds(1), "1s"),
            (Duration::seconds(90), "1m30s"),
            (Duration::milliseconds(500), "500ms"),
            (Duration::milliseconds(1500), "1.5s"),
            (Duration::hours(1), "1h0m0s"),
            (Duration::seconds(-1), "-1s"),
        ];
        for (d, want) in cases {
            assert_eq!(go_duration_string(d), want, "go_duration_string({d:?})");
        }
        assert_eq!(go_duration_string(Duration::nanoseconds(0)), "0s");
        // The full Encode->Decode round-trip through the real parser is covered by
        // `claim_durations_round_trip`.
    }

    // A non-zero claim_ttl / claim_settle_delay survives an Encode->Decode round-trip.
    #[test]
    fn claim_durations_round_trip() {
        let front = concat!(
            "tracker:\n  kind: linear\n  api_key: \"$X\"\n  project_slug: p\n",
            "  active_states: [Todo]\n  terminal_states: [Done]\n",
            "  claim_mode: pool\n  claim_ttl: 2m\n  claim_settle_delay: 1s\n",
        );
        let c1 = decode_map(front, "body");
        assert_eq!(c1.tracker.claim_ttl, Duration::minutes(2));
        assert_eq!(c1.tracker.claim_settle_delay, Duration::seconds(1));
        let c2 = re_encode_decode(&c1);
        assert_eq!(c2.tracker.claim_ttl, Duration::minutes(2));
        assert_eq!(c2.tracker.claim_settle_delay, Duration::seconds(1));
    }
}
