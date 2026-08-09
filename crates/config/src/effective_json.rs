//! `effective_json` — parity port of the Go `internal/httpapi` config VIEW: `buildConfigJSON`,
//! `buildGlobalJSON`, `buildProjectsJSON`, `toEffectiveJSON`, `recommendClone` (see
//! `$REF/internal/httpapi/{config_view,responses}.go`).
//!
//! [`render`] reproduces the `GET /api/v1/config` response body byte-for-byte (after the shared
//! `harness_fixtures::normalize`). P6's HTTP handler reuses this module rather than reimplementing
//! the serialization (per the P1 plan).
//!
//! # Two views, one payload
//!
//! The response carries both the legacy verbatim view and the typed multi-agent view:
//!
//! - `config` — the on-disk front-matter map, VERBATIM (pre-`$VAR`/`~` resolution, so the `api_key`
//!   indirection is preserved). It comes from the [`Definition`], not the typed config.
//! - `prompt_body` — the trimmed prompt template.
//! - `generated_at` — an RFC3339 UTC timestamp (Go `time.Now().UTC().Format(time.RFC3339)`).
//! - `global` + `projects[]` — derived from a best-effort [`decode`] (omitted if the file does not
//!   decode, exactly like Go `buildConfigJSON`).
//!
//! # Deviation from the plan sketch (`render(&Resolved)`)
//!
//! The plan sketches `render(&Resolved)`, but the faithful port takes the [`Definition`], not a
//! `Resolved`, for two parity reasons proven by the committed fixtures.
//!
//! First, `config` is the raw front-matter map VERBATIM — the typed `Config`/`Resolved` does not
//! carry it, so it can only come from the `Definition`.
//!
//! Second, `global`/`projects` derive from `Decode(def)`, NOT `Resolve`: Go `buildConfigJSON` calls
//! `config.Decode`, never `Resolve`. The fixtures confirm it — `global.workspace.root` is
//! `"~/symphony_workspaces"` (full.md's explicit, unexpanded value), `global.logging.dir` is the
//! `"~/.rhapsody/logs"` default (the TRA-238 divergence from Go's `~/.symphony/logs`), and
//! `storage.retention_days` is `null` for the minimal/graphite configs, all pre-resolution values.
//! Rendering from a `Resolved` would absolutize those paths and default retention to 30, breaking
//! the golden. So [`render`] decodes the `Definition` internally (best-effort), matching the Go GET
//! handler exactly.

use chrono::{SecondsFormat, Utc};
use serde_json::{Map, Value};

use crate::decode::decode;
use crate::model::{
    Config, DEPENDENCY_MODE_DAG, DEPENDENCY_MODE_GRAPHITE, Project, WORKSPACE_MODE_WORKTREE,
};
use crate::projects::{EffectiveConfig, effective_for};
use crate::workflow::{Definition, YamlMap};

/// Renders a workflow [`Definition`] into the `GET /api/v1/config` response body (Go
/// `buildConfigJSON`): the verbatim `config` + `prompt_body`, a `generated_at` timestamp, and — when
/// the front matter decodes — the typed `global` + `projects[]` view.
pub fn render(def: &Definition) -> Value {
    let mut out = Map::new();
    out.insert("config".to_string(), yaml_map_to_json(&def.config));
    out.insert(
        "prompt_body".to_string(),
        Value::String(def.prompt_template.clone()),
    );
    // generated_at: RFC3339 UTC, second precision, `Z` offset — exactly Go's time.RFC3339 on a UTC
    // time. Nondeterministic by nature; `harness_fixtures::normalize` reduces it to <TIMESTAMP>.
    out.insert(
        "generated_at".to_string(),
        Value::String(Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)),
    );
    // Best-effort typed view — omitted (like Go) when the front matter does not decode.
    if let Ok(cfg) = decode(def) {
        out.insert("global".to_string(), build_global(&cfg));
        let projects = build_projects(&cfg);
        // Go's `projects` field is `omitempty`: an empty list is omitted from the payload.
        if !projects.is_empty() {
            out.insert("projects".to_string(), Value::Array(projects));
        }
    }
    Value::Object(out)
}

// ---------------------------------------------------------------------------
// global view (Go buildGlobalJSON + globalConfigJSON)
// ---------------------------------------------------------------------------

/// Projects the shared (top-level) knobs (Go `buildGlobalJSON`). The Linear key is reported only as
/// `api_key_set`; the secret/indirection value is never included.
fn build_global(c: &Config) -> Value {
    let mut agent = Map::new();
    agent.insert("backend".into(), s(&c.agent.backend));
    agent.insert(
        "max_concurrent_agents".into(),
        num(c.agent.max_concurrent_agents),
    );
    agent.insert("max_turns".into(), num(c.agent.max_turns));
    agent.insert(
        "max_retry_backoff_ms".into(),
        num(c.agent.max_retry_backoff_ms),
    );
    // max_concurrent_agents_by_state is `omitempty`: emitted only when non-empty.
    if !c.agent.max_concurrent_agents_by_state.is_empty() {
        let mut m = Map::new();
        for (k, v) in &c.agent.max_concurrent_agents_by_state {
            m.insert(k.clone(), num(*v));
        }
        agent.insert("max_concurrent_agents_by_state".into(), Value::Object(m));
    }

    let mut claude = Map::new();
    claude.insert("command".into(), s(&c.claude.command));
    claude.insert("model".into(), s(&c.claude.model));
    claude.insert("effort".into(), s(&c.claude.effort));
    claude.insert("permission_mode".into(), s(&c.claude.permission_mode));
    // BillingGuard: Go `nil || *ptr` — None (default) or Some(true) is true; only Some(false) is off.
    claude.insert(
        "billing_guard".into(),
        Value::Bool(c.claude.billing_guard != Some(false)),
    );
    claude.insert("ultracode".into(), Value::Bool(c.claude.ultracode));
    claude.insert("turn_timeout_ms".into(), num(c.claude.turn_timeout_ms));
    claude.insert("read_timeout_ms".into(), num(c.claude.read_timeout_ms));
    claude.insert("stall_timeout_ms".into(), num(c.claude.stall_timeout_ms));
    claude.insert("mcp_config".into(), s(&c.claude.mcp_config));
    // extra_args is `omitempty`.
    if !c.claude.extra_args.is_empty() {
        claude.insert("extra_args".into(), str_array(&c.claude.extra_args));
    }

    let mut otel = Map::new();
    otel.insert("enabled".into(), Value::Bool(c.otel.enabled));
    otel.insert("endpoint".into(), s(&c.otel.endpoint));
    otel.insert("protocol".into(), s(&c.otel.protocol));
    otel.insert("service_name".into(), s(&c.otel.service_name));
    otel.insert("insecure".into(), Value::Bool(c.otel.insecure));
    // headers is `omitempty`.
    if !c.otel.headers.is_empty() {
        let mut m = Map::new();
        for (k, v) in &c.otel.headers {
            m.insert(k.clone(), s(v));
        }
        otel.insert("headers".into(), Value::Object(m));
    }

    obj(vec![
        (
            "tracker",
            obj(vec![
                ("kind", s(&c.tracker.kind)),
                ("endpoint", s(&c.tracker.endpoint)),
                ("api_key_set", Value::Bool(!c.tracker.api_key.is_empty())),
            ]),
        ),
        (
            "polling",
            obj(vec![("interval_ms", num(c.polling.interval_ms))]),
        ),
        ("agent", Value::Object(agent)),
        ("claude", Value::Object(claude)),
        ("workspace", obj(vec![("root", s(&c.workspace.root))])),
        (
            "storage",
            obj(vec![
                ("path", s(&c.storage.path)),
                ("retention_days", int_or_null(c.storage.retention_days)),
            ]),
        ),
        ("otel", Value::Object(otel)),
        (
            "mcp",
            obj(vec![
                ("enabled", Value::Bool(c.mcp.enabled)),
                ("allow_send_message", Value::Bool(c.mcp.allow_send_message)),
                ("allow_stop", Value::Bool(c.mcp.allow_stop)),
                ("allow_resume", Value::Bool(c.mcp.allow_resume)),
            ]),
        ),
        ("server", obj(vec![("port", int_or_null(c.server.port))])),
        ("logging", obj(vec![("dir", s(&c.logging.dir))])),
        ("repo", s(&c.repo)),
        ("active_states", slice_or_null(&c.tracker.active_states)),
        ("terminal_states", slice_or_null(&c.tracker.terminal_states)),
        ("canceled_states", slice_or_null(&c.tracker.canceled_states)),
        ("review_states", slice_or_null(&c.tracker.review_states)),
        ("review_promote_state", s(&c.tracker.review_promote_state)),
        ("summon_token", s(&c.tracker.summon_token)),
        ("github_summons", Value::Bool(c.tracker.github_summons)),
        ("milestone", s(&c.tracker.milestone)),
        ("labels", slice_or_null(&c.tracker.labels)),
        // NOTE: `capabilities` is a Rhapsody-only field (no Go v0.4.0 counterpart). Unlike `labels`,
        // it is deliberately NOT surfaced in the always-emitted global config view: doing so would
        // inject a `"capabilities": null` key into the `GET /api/v1/config` body and break byte-parity
        // with the captured Go v0.4.0 config goldens (there is no legitimate recapture path — the
        // frozen reference has no such field). Same rationale as `mcp.allow_handoff` (TRA-242), which
        // is likewise kept out of this view. Surfacing it belongs with the httpapi/web ticket (which
        // owns the golden-affecting config-view shape), not this config-crate plumbing task.
        ("prompt", s(&c.prompt_template)),
        ("prompt_file", s(&c.prompt_file)),
        ("git_flow", s(&c.git_flow)),
        ("workspace_mode", s(&c.workspace_mode)),
        ("dependency_mode", s(&c.tracker.dependency_mode)),
        ("dep_mode_prompt_file", s(&c.tracker.dep_mode_prompt_file)),
        ("claim_mode", s(&c.tracker.claim_mode)),
    ])
}

// ---------------------------------------------------------------------------
// projects view (Go buildProjectsJSON + projectConfigJSON)
// ---------------------------------------------------------------------------

/// Projects the agent list (Go `buildProjectsJSON`): one entry per config project, or — for a legacy
/// single-project (`tracker.project_slug`) config — a single synthesized entry so the UI always sees
/// a consistent agent list. Returns an empty vector for a config with neither.
fn build_projects(c: &Config) -> Vec<Value> {
    if c.projects.is_empty() {
        if c.tracker.project_slug.is_empty() {
            return Vec::new();
        }
        let eff = effective_for(c, None);
        let mut entry = Map::new();
        entry.insert("name".into(), s(""));
        entry.insert(
            "slugs".into(),
            str_array(std::slice::from_ref(&c.tracker.project_slug)),
        );
        entry.insert("enabled".into(), Value::Bool(true));
        entry.insert("overrides".into(), build_overrides(None));
        entry.insert(
            "effective".into(),
            to_effective(c, &eff, "", &c.tracker.project_slug, &c.repo, true),
        );
        // Legacy single-project: no per-project override, so the global IS the project's setting.
        entry.insert(
            "workspace_mode_recommended".into(),
            Value::Bool(recommend_clone(
                &eff.dependency_mode,
                &eff.workspace_mode,
                false,
            )),
        );
        return vec![Value::Object(entry)];
    }

    let mut out = Vec::with_capacity(c.projects.len());
    for p in &c.projects {
        let eff = effective_for(c, Some(p));
        let enabled = p.enabled.unwrap_or(true);
        let mut entry = Map::new();
        entry.insert("name".into(), s(&p.name));
        entry.insert("slugs".into(), str_array(&p.slugs));
        // omitempty direct fields — emitted only when set.
        insert_if_non_empty(&mut entry, "repo", &p.repo);
        insert_if_non_empty(&mut entry, "milestone", &p.milestone);
        if !p.labels.is_empty() {
            entry.insert("labels".into(), str_array(&p.labels));
        }
        if !p.capabilities.is_empty() {
            entry.insert("capabilities".into(), str_array(&p.capabilities));
        }
        entry.insert("enabled".into(), Value::Bool(enabled));
        if !p.active_states.is_empty() {
            entry.insert("active_states".into(), str_array(&p.active_states));
        }
        if !p.terminal_states.is_empty() {
            entry.insert("terminal_states".into(), str_array(&p.terminal_states));
        }
        if !p.canceled_states.is_empty() {
            entry.insert("canceled_states".into(), str_array(&p.canceled_states));
        }
        if !p.review_states.is_empty() {
            entry.insert("review_states".into(), str_array(&p.review_states));
        }
        if let Some(n) = p.max_concurrent_agents {
            entry.insert("max_concurrent_agents".into(), num(n));
        }
        insert_if_non_empty(&mut entry, "prompt", &p.prompt);
        insert_if_non_empty(&mut entry, "prompt_file", &p.prompt_file);
        entry.insert("overrides".into(), build_overrides(Some(p)));
        let slug0 = p.slugs.first().map(String::as_str).unwrap_or("");
        entry.insert(
            "effective".into(),
            to_effective(c, &eff, &p.name, slug0, &p.repo, enabled),
        );
        entry.insert(
            "workspace_mode_recommended".into(),
            Value::Bool(recommend_clone(
                &eff.dependency_mode,
                &eff.workspace_mode,
                !p.workspace_mode.is_empty(),
            )),
        );
        out.push(Value::Object(entry));
    }
    out
}

/// The sparse per-project claude presence-map (Go `claudeOverridesJSON`): a present key overrides,
/// an absent key inherits. `None` (the legacy synthesized entry) yields an empty `{}`.
fn build_overrides(project: Option<&Project>) -> Value {
    let mut m = Map::new();
    if let Some(p) = project {
        if let Some(ov) = &p.claude {
            insert_opt_str(&mut m, "model", &ov.model);
            insert_opt_str(&mut m, "effort", &ov.effort);
            insert_opt_str(&mut m, "permission", &ov.permission_mode);
            if let Some(v) = ov.ultracode {
                m.insert("ultracode".into(), Value::Bool(v));
            }
            if let Some(v) = ov.turn_timeout_ms {
                m.insert("turn_timeout_ms".into(), num(v));
            }
            if let Some(v) = ov.stall_timeout_ms {
                m.insert("stall_timeout_ms".into(), num(v));
            }
            if let Some(v) = ov.billing_guard {
                m.insert("billing_guard".into(), Value::Bool(v));
            }
            insert_opt_str(&mut m, "command", &ov.command);
        }
        // git_flow / workspace_mode / dependency_mode / dep_mode_prompt_file / claim_mode live on the
        // top-level Project fields but surface in the overrides block; emitted only when set.
        insert_if_non_empty(&mut m, "git_flow", &p.git_flow);
        insert_if_non_empty(&mut m, "workspace_mode", &p.workspace_mode);
        insert_if_non_empty(&mut m, "dependency_mode", &p.dependency_mode);
        insert_if_non_empty(&mut m, "dep_mode_prompt_file", &p.dep_mode_prompt_file);
        insert_if_non_empty(&mut m, "claim_mode", &p.claim_mode);
    }
    Value::Object(m)
}

/// The resolved per-project display view (Go `toEffectiveJSON` / `effectiveConfigJSON`): global ⊕
/// overrides. `name` defaults to the first slug; `repo` falls back to the top-level repo;
/// `review_promote_state` is the global value (no per-project override exists).
fn to_effective(
    c: &Config,
    eff: &EffectiveConfig,
    raw_name: &str,
    slug0: &str,
    project_repo: &str,
    enabled: bool,
) -> Value {
    let name = if raw_name.is_empty() { slug0 } else { raw_name };
    let repo = if project_repo.is_empty() {
        c.repo.as_str()
    } else {
        project_repo
    };
    obj(vec![
        ("name", s(name)),
        ("repo", s(repo)),
        ("model", s(&eff.claude.model)),
        ("effort", s(&eff.claude.effort)),
        ("permission", s(&eff.claude.permission_mode)),
        ("ultracode", Value::Bool(eff.claude.ultracode)),
        ("turn_timeout_ms", num(eff.claude.turn_timeout_ms)),
        ("stall_timeout_ms", num(eff.claude.stall_timeout_ms)),
        ("active_states", slice_or_null(&eff.active_states)),
        ("terminal_states", slice_or_null(&eff.terminal_states)),
        ("canceled_states", slice_or_null(&eff.canceled_states)),
        ("review_states", slice_or_null(&eff.review_states)),
        ("review_promote_state", s(&c.tracker.review_promote_state)),
        ("max_concurrent_agents", num(eff.max_concurrent_agents)),
        ("milestone", s(&eff.milestone)),
        ("labels", slice_or_null(&eff.labels)),
        ("prompt", s(&eff.prompt)),
        ("prompt_file", s(&eff.prompt_file)),
        ("git_flow", s(&eff.git_flow)),
        ("workspace_mode", s(&eff.workspace_mode)),
        ("dependency_mode", s(&eff.dependency_mode)),
        ("dep_mode_prompt_file", s(&eff.dep_mode_prompt_file)),
        ("claim_mode", s(&eff.claim_mode)),
        ("enabled", Value::Bool(enabled)),
    ])
}

/// Whether the UI should recommend (never force) clone mode for a stacking project (Go
/// `recommendClone`): its effective `dependency_mode` is graphite/dag, its effective `workspace_mode`
/// is still worktree, AND it has not explicitly chosen a per-project `workspace_mode`. Display-only.
fn recommend_clone(
    effective_dependency_mode: &str,
    effective_workspace_mode: &str,
    per_project_explicit: bool,
) -> bool {
    if per_project_explicit {
        return false;
    }
    if effective_workspace_mode != WORKSPACE_MODE_WORKTREE {
        return false; // already clone (or a future mode) — nothing to recommend
    }
    effective_dependency_mode == DEPENDENCY_MODE_GRAPHITE
        || effective_dependency_mode == DEPENDENCY_MODE_DAG
}

// ---------------------------------------------------------------------------
// small value builders
// ---------------------------------------------------------------------------

/// A JSON string.
fn s(v: &str) -> Value {
    Value::String(v.to_string())
}

/// A JSON integer.
fn num(v: i64) -> Value {
    Value::from(v)
}

/// A JSON array of strings.
fn str_array(v: &[String]) -> Value {
    Value::Array(v.iter().map(|x| Value::String(x.clone())).collect())
}

/// A `[]string` field with NO `omitempty` tag: Go marshals a nil/empty slice as `null`, a non-empty
/// one as an array. (Rust `Vec` cannot carry Go's nil-vs-empty distinction, but a decoded config
/// never holds an explicit empty list — absent decodes to empty — so empty ⇒ `null` matches the
/// captured fixtures, whose empty state/label lists are all `null`.)
fn slice_or_null(v: &[String]) -> Value {
    if v.is_empty() {
        Value::Null
    } else {
        str_array(v)
    }
}

/// A nullable `*int` field with no `omitempty` (Go `retention_days` / `port`): `None` ⇒ `null`.
fn int_or_null(v: Option<i64>) -> Value {
    match v {
        Some(n) => num(n),
        None => Value::Null,
    }
}

/// Build a JSON object from ordered `(key, value)` pairs (order is irrelevant — the golden sorts).
fn obj(pairs: Vec<(&str, Value)>) -> Value {
    Value::Object(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

/// Insert `key => v` only when `v` is non-empty (mirrors a Go `string,omitempty` field).
fn insert_if_non_empty(m: &mut Map<String, Value>, key: &str, v: &str) {
    if !v.is_empty() {
        m.insert(key.to_string(), s(v));
    }
}

/// Insert `key => *v` only when the override pointer is set (mirrors a Go `*string,omitempty` field).
fn insert_opt_str(m: &mut Map<String, Value>, key: &str, v: &Option<String>) {
    if let Some(val) = v {
        m.insert(key.to_string(), s(val));
    }
}

// ---------------------------------------------------------------------------
// YAML front-matter -> JSON (the verbatim `config` block)
// ---------------------------------------------------------------------------

/// Convert the front-matter root map to a JSON object (Go returns `def.Config` verbatim and
/// `encoding/json` marshals the `map[string]any`; here we convert the parsed YAML mapping).
fn yaml_map_to_json(m: &YamlMap) -> Value {
    let mut out = Map::new();
    for (k, v) in m {
        out.insert(yaml_key_to_string(k), yaml_value_to_json(v));
    }
    Value::Object(out)
}

/// Convert one YAML value to its JSON equivalent, preserving integer-ness (Go's yaml.v3 →
/// `map[string]any` → `encoding/json` keeps `int` as an integer, `bool` as a bool, etc.).
fn yaml_value_to_json(v: &serde_yaml_ng::Value) -> Value {
    use serde_yaml_ng::Value as Y;
    match v {
        Y::Null => Value::Null,
        Y::Bool(b) => Value::Bool(*b),
        Y::Number(n) => yaml_number_to_json(n),
        Y::String(x) => Value::String(x.clone()),
        Y::Sequence(seq) => Value::Array(seq.iter().map(yaml_value_to_json).collect()),
        Y::Mapping(map) => {
            let mut out = Map::new();
            for (k, vv) in map {
                out.insert(yaml_key_to_string(k), yaml_value_to_json(vv));
            }
            Value::Object(out)
        }
        // Tags never appear in WORKFLOW.md front matter; unwrap defensively.
        Y::Tagged(t) => yaml_value_to_json(&t.value),
    }
}

/// Preserve integer vs float (config front matter uses only integers, but be exact).
fn yaml_number_to_json(n: &serde_yaml_ng::Number) -> Value {
    if let Some(i) = n.as_i64() {
        Value::from(i)
    } else if let Some(u) = n.as_u64() {
        Value::from(u)
    } else if let Some(f) = n.as_f64() {
        serde_json::Number::from_f64(f)
            .map(Value::Number)
            .unwrap_or(Value::Null)
    } else {
        Value::Null
    }
}

/// A YAML mapping key as a string. Front-matter keys are always strings (the loader rejects a
/// non-string-keyed root map); stringify defensively for any nested non-string key.
fn yaml_key_to_string(k: &serde_yaml_ng::Value) -> String {
    match k {
        serde_yaml_ng::Value::String(x) => x.clone(),
        other => serde_yaml_ng::to_string(other)
            .map(|s| s.trim().to_string())
            .unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    //! The `minimal`/`full`/`graphite` goldens cover the legacy single-project (`tracker.
    //! project_slug`) view exhaustively (see `tests/golden.rs`). These lock the branches the goldens
    //! do NOT exercise: the multi-agent `projects:` path (per-project overrides, inherit fallbacks,
    //! omitempty direct fields) and the neither-projects-nor-slug case. Expected values are derived
    //! from the Go `config_view.go` semantics, not from this implementation.
    use super::*;
    use crate::workflow::YamlMap;

    fn render_front(front: &str, body: &str) -> Value {
        let config: YamlMap = serde_yaml_ng::from_str(front).expect("front matter parses");
        let def = Definition {
            config,
            prompt_template: body.to_string(),
        };
        render(&def)
    }

    // The `projects:` (multi-agent) path: one entry per PROJECT (not per slug), with per-project
    // overrides winning and unset knobs inheriting the global effective value.
    #[test]
    fn multi_project_view_overrides_and_inherit() {
        let front = concat!(
            "tracker:\n  kind: linear\n  api_key: \"$X\"\n  active_states: [Todo]\n  terminal_states: [Done]\n",
            "repo: git@github.com:o/top.git\n",
            "claude:\n  model: opus\n",
            "projects:\n",
            "  - name: Infra\n    slugs: [infra-1, infra-2]\n    repo: git@github.com:o/infra.git\n    enabled: false\n    claude:\n      model: sonnet\n",
            "  - slugs: [web-1]\n",
        );
        let v = render_front(front, "body");
        let projects = v["projects"].as_array().expect("projects array");
        assert_eq!(projects.len(), 2, "one entry per project, not per slug");

        // Project 0 (Infra): explicit name + repo, paused, model override.
        let p0 = &projects[0];
        assert_eq!(p0["name"], "Infra");
        assert_eq!(p0["slugs"], serde_json::json!(["infra-1", "infra-2"]));
        assert_eq!(p0["repo"], "git@github.com:o/infra.git");
        assert_eq!(p0["enabled"], false);
        assert_eq!(p0["overrides"]["model"], "sonnet");
        assert_eq!(p0["effective"]["name"], "Infra");
        assert_eq!(p0["effective"]["repo"], "git@github.com:o/infra.git");
        assert_eq!(p0["effective"]["model"], "sonnet", "override wins");
        assert_eq!(p0["effective"]["enabled"], false);

        // Project 1 (web): defaults — name from first slug, repo from top-level, model inherits global.
        let p1 = &projects[1];
        assert_eq!(p1["name"], "");
        assert_eq!(p1["slugs"], serde_json::json!(["web-1"]));
        assert!(
            p1.get("repo").is_none(),
            "empty repo is omitted (omitempty)"
        );
        assert_eq!(p1["enabled"], true);
        assert_eq!(
            p1["overrides"],
            serde_json::json!({}),
            "no overrides => {{}}"
        );
        assert_eq!(
            p1["effective"]["name"], "web-1",
            "name defaults to first slug"
        );
        assert_eq!(
            p1["effective"]["repo"], "git@github.com:o/top.git",
            "repo falls back to top-level"
        );
        assert_eq!(
            p1["effective"]["model"], "opus",
            "inherits the global model"
        );
        assert_eq!(p1["effective"]["enabled"], true);
    }

    // Neither `projects:` nor `tracker.project_slug`: `projects` is omitted entirely (Go returns
    // nil), while the verbatim `config` and typed `global` are still present.
    #[test]
    fn no_projects_and_no_slug_omits_projects() {
        let front = "tracker:\n  kind: linear\n  api_key: \"$X\"\n  active_states: [Todo]\n  terminal_states: [Done]\n";
        let v = render_front(front, "body");
        assert!(
            v.get("projects").is_none(),
            "projects omitted when neither projects: nor project_slug is set"
        );
        assert!(v.get("config").is_some(), "verbatim config still present");
        assert!(v.get("global").is_some(), "typed global still present");
        assert_eq!(v["prompt_body"], "body");
    }
}
