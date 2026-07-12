//! Renders the initial WORKFLOW.md the first-launch wizard writes (design §6): a minimal, valid config
//! seeded with the chosen Linear project slug and the api_key indirection, plus a concise default
//! prompt body the user can later refine in Settings. Parity port of
//! `$REF/desktop/internal/onboarding/onboarding.go`.

use serde::Serialize;

/// A boxed std error mirrors Go's opaque `error` return.
type Error = Box<dyn std::error::Error + Send + Sync>;

/// Canonical repo-relative prompt path the onboarding config seeds into `prompt_file` (repo-level
/// prompt feature, INF-279). Mirrors `config.DefaultRepoPromptFile`; kept in sync with the daemon's
/// `rhapsody-config`, which the desktop cannot import (separate build).
const REPO_PROMPT_FILE: &str = ".symphony/PROMPT.md";

/// The internal fleet-observability hub the onboarding config seeds as `otel.endpoint` (INF-442).
/// Mirrors `config.DefaultOtelEndpoint`. Kept VERBATIM for runtime parity — the generated WORKFLOW.md
/// must match the Go desktop's byte-for-byte in its values; this is a runtime infra endpoint, not a
/// packaging/brand token (the D5 brand guard scopes to the `.app` identifier + name).
const DEFAULT_OTEL_ENDPOINT: &str = "https://otel-symphony.ops-oma-prod.makewhat.is";

/// A concise, valid starting prompt kept as the inline FALLBACK used when the seeded repo `prompt_file`
/// is absent from the checkout — a relative `prompt_file` soft-falls-back to this body rather than
/// failing the run (INF-279). Mirrors `defaultPromptBody`.
const DEFAULT_PROMPT_BODY: &str = r#"You are an autonomous engineer working ticket {{ issue.identifier }}: "{{ issue.title }}".

{{ issue.description }}

Implement the change test-first, keep the build and tests green, open a draft PR, move the issue
to review when done, and end your final message with a line: `HANDOFF: in-review` — Symphony
records the run as completed only when this declaration is present. Do not merge."#;

/// The onboarding config front matter. The fields are declared in the SAME alphabetical order Go's
/// `yaml.Marshal(map[string]any)` emits (it sorts map keys), so the generated YAML matches the Go
/// desktop's structure. The nested structs likewise order their fields alphabetically.
#[derive(Serialize)]
struct InitialConfig {
    agent: Agent,
    claude: Claude,
    otel: Otel,
    prompt_file: &'static str,
    storage: Storage,
    tracker: Tracker,
}

#[derive(Serialize)]
struct Agent {
    backend: &'static str,
    max_concurrent_agents: u32,
}

#[derive(Serialize)]
struct Claude {
    model: &'static str,
    /// 6h per-turn cap (the daemon default is 1h); autonomous, multi-step tickets routinely run a
    /// single agent turn past an hour, so seed a generous ceiling.
    turn_timeout_ms: u64,
}

#[derive(Serialize)]
struct Otel {
    enabled: bool,
    endpoint: &'static str,
    insecure: bool,
    protocol: &'static str,
    service_name: &'static str,
}

#[derive(Serialize)]
struct Storage {
    path: &'static str,
}

#[derive(Serialize)]
struct Tracker {
    active_states: Vec<&'static str>,
    api_key: &'static str,
    kind: &'static str,
    project_slug: String,
    terminal_states: Vec<&'static str>,
}

/// Builds a WORKFLOW.md (YAML front matter + prompt body) for the given Linear project slug. The
/// api_key is stored as the `$LINEAR_API_KEY` indirection (the desktop app supplies the value from the
/// Keychain at launch) so no secret is written to disk. Mirrors `RenderInitialWorkflow`.
pub fn render_initial_workflow(project_slug: &str) -> Result<Vec<u8>, Error> {
    let slug = project_slug.trim();
    if slug.is_empty() {
        return Err("a Linear project slug is required".into());
    }
    let config = InitialConfig {
        // Default new projects to the repo's own prompt: prompt_file WINS over the inline body when the
        // file is present, and soft-falls-back to the body below when it is absent (INF-279).
        prompt_file: REPO_PROMPT_FILE,
        tracker: Tracker {
            kind: "linear",
            api_key: "$LINEAR_API_KEY",
            project_slug: slug.to_string(),
            active_states: vec!["Todo", "In Progress"],
            terminal_states: vec!["Done", "Cancelled", "Canceled", "Duplicate"],
        },
        agent: Agent {
            backend: "claude",
            max_concurrent_agents: 1,
        },
        claude: Claude {
            model: "claude-opus-4-8",
            turn_timeout_ms: 21_600_000,
        },
        // Persist run history under ~/.symphony so it survives reboots (also the resolved default;
        // kept explicit so the generated WORKFLOW.md the user sees is self-documenting).
        storage: Storage {
            path: "~/.symphony/symphony.db",
        },
        // Export telemetry to the internal fleet-observability hub by default (INF-299). The endpoint is
        // tailnet-only; off-tailnet the exporter drops/retries silently and is never fatal. Users opt
        // out via the General tab's Observability toggle (otel.enabled: false).
        otel: Otel {
            enabled: true,
            endpoint: DEFAULT_OTEL_ENDPOINT,
            protocol: "http",
            service_name: "symphony",
            insecure: false,
        },
    };
    let yaml = serde_yaml_ng::to_string(&config).map_err(|e| Box::new(e) as Error)?;
    let mut out = String::from("---\n");
    out.push_str(&yaml); // serde_yaml_ng ends the document with a newline
    out.push_str("---\n");
    out.push_str(DEFAULT_PROMPT_BODY.trim_end_matches('\n'));
    out.push('\n');
    Ok(out.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Splits the rendered output into (front matter, body), mirroring the Go tests'
    /// `strings.SplitN(strings.TrimPrefix(s, "---\n"), "\n---\n", 2)`.
    fn split_front_body(data: &[u8]) -> (String, String) {
        let s = String::from_utf8(data.to_vec()).expect("utf8 output");
        assert!(
            s.starts_with("---\n"),
            "output must open with the front-matter delimiter:\n{s}"
        );
        let rest = s.strip_prefix("---\n").expect("delimiter");
        let (front, body) = rest
            .split_once("\n---\n")
            .expect("expected front matter and body separated by ---");
        (front.to_string(), body.to_string())
    }

    fn front_yaml(data: &[u8]) -> serde_yaml_ng::Value {
        let (front, _) = split_front_body(data);
        serde_yaml_ng::from_str(&front).expect("front matter is valid YAML")
    }

    // Mirrors TestRenderInitialWorkflowIsValid: the rendered file carries the chosen project slug, the
    // api_key indirection (never a literal secret), linear/claude defaults, and a non-empty body.
    #[test]
    fn render_initial_workflow_is_valid() {
        let data = render_initial_workflow("my-project").expect("render");
        let (_, body) = split_front_body(&data);
        let cfg = front_yaml(&data);

        assert_eq!(cfg["tracker"]["kind"].as_str(), Some("linear"));
        assert_eq!(cfg["tracker"]["project_slug"].as_str(), Some("my-project"));
        assert_eq!(
            cfg["tracker"]["api_key"].as_str(),
            Some("$LINEAR_API_KEY"),
            "want the $LINEAR_API_KEY indirection (no literal secret)"
        );
        assert_eq!(cfg["agent"]["backend"].as_str(), Some("claude"));
        assert_eq!(
            cfg["claude"]["turn_timeout_ms"].as_u64(),
            Some(21_600_000),
            "want 21600000 (6h)"
        );
        assert_eq!(
            cfg["storage"]["path"].as_str(),
            Some("~/.symphony/symphony.db"),
            "want persistent history under ~/.symphony"
        );
        assert!(!body.trim().is_empty(), "prompt body is empty");
    }

    // Mirrors TestRenderInitialWorkflowSeedsOtelExportOn: fresh installs export to the internal hub by
    // default with the canonical ops-oma-prod values (#1794).
    #[test]
    fn render_initial_workflow_seeds_otel_export_on() {
        let cfg = front_yaml(&render_initial_workflow("my-project").expect("render"));
        assert_eq!(
            cfg["otel"]["enabled"].as_bool(),
            Some(true),
            "export on by default"
        );
        assert_eq!(
            cfg["otel"]["endpoint"].as_str(),
            Some("https://otel-symphony.ops-oma-prod.makewhat.is")
        );
        assert_eq!(
            cfg["otel"]["protocol"].as_str(),
            Some("http"),
            "the hub collector is OTLP/HTTP-only"
        );
        assert_eq!(cfg["otel"]["service_name"].as_str(), Some("symphony"));
        assert_eq!(
            cfg["otel"]["insecure"].as_bool(),
            Some(false),
            "TLS to the hub"
        );
    }

    // Mirrors TestRenderInitialWorkflowBodyDeclaresHandoff: the seeded prompt must instruct the agent to
    // end its final message with the `HANDOFF: in-review` marker.
    #[test]
    fn render_initial_workflow_body_declares_handoff() {
        let data = render_initial_workflow("my-project").expect("render");
        let (_, body) = split_front_body(&data);
        assert!(
            body.contains("HANDOFF: in-review"),
            "prompt body must declare the `HANDOFF: in-review` hand-off marker:\n{body}"
        );
    }

    // Mirrors TestRenderInitialWorkflowSeedsRepoPromptFile: new projects default to the repo's own
    // prompt while keeping the inline body as the soft-fallback (INF-279).
    #[test]
    fn render_initial_workflow_seeds_repo_prompt_file() {
        let data = render_initial_workflow("my-project").expect("render");
        let cfg = front_yaml(&data);
        assert_eq!(
            cfg["prompt_file"].as_str(),
            Some(".symphony/PROMPT.md"),
            "want the repo-prompt default"
        );
        let (_, body) = split_front_body(&data);
        assert!(
            !body.trim().is_empty(),
            "the inline prompt body must be retained as the soft-fallback"
        );
    }

    // Mirrors TestRenderInitialWorkflowRejectsEmptySlug: a project slug is required.
    #[test]
    fn render_initial_workflow_rejects_empty_slug() {
        assert!(
            render_initial_workflow("  ").is_err(),
            "expected an error for an empty project slug"
        );
    }

    // Mirrors TestRenderInitialWorkflowQuotesSlug: a slug with YAML-special characters round-trips
    // intact (it must not silently corrupt the front matter).
    #[test]
    fn render_initial_workflow_quotes_slug() {
        let data = render_initial_workflow("weird: slug #1").expect("render");
        let cfg = front_yaml(&data);
        assert_eq!(
            cfg["tracker"]["project_slug"].as_str(),
            Some("weird: slug #1"),
            "want the slug preserved verbatim"
        );
    }
}
