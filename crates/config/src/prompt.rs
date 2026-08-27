//! Strict Liquid rendering of the WORKFLOW.md prompt body.
//!
//! Parity port of Go `internal/prompt/render.go` (Rhapsody P1, Task C7). A WORKFLOW.md prompt
//! body is a Liquid template (upstream §5.4, §12) rendered with `issue` + `attempt` bindings
//! under strict-variables semantics: an unknown variable, an unknown key on a bound object, or an
//! unknown filter is a rendering error — never a silently-empty expansion. An empty body falls
//! back to a fixed default prompt. This is the contract for every template operators have
//! written (the live Tally/Booch/Rhapsody prompts), so it must match the Go daemon.

use chrono::{DateTime, SecondsFormat, Utc};
use liquid::model::Value;
use rhapsody_core::{BlockerRef, Issue};

/// Prompt returned when the template body is empty. Mirrors Go `prompt.defaultPrompt`.
const DEFAULT_PROMPT: &str = "You are working on an issue from Linear.";

/// Error returned when a prompt template fails to parse or render.
///
/// Its `Display` carries the Go sentinel prefix `template_render_error: ` — the same token Go
/// produces via `fmt.Errorf("template_render_error: %w", err)`. The token is part of the
/// observable contract (it surfaces in logs and API error bodies). The wrapped detail is the
/// `liquid` crate's own message; it need not byte-match Go's `osteele/liquid` message because no
/// caller parses past the prefix.
#[derive(thiserror::Error, Debug)]
pub enum RenderError {
    /// The template failed to parse (e.g. an unknown filter) or to render (e.g. an unknown
    /// variable or nested key under strict-variables mode).
    #[error("template_render_error: {0}")]
    Template(String),
}

/// Renders `template` with `issue` + `attempt` bindings. Unknown variables and unknown filters
/// fail rendering. An empty `template` yields the default prompt. Mirrors Go `prompt.Render`.
pub fn render(template: &str, issue: &Issue, attempt: Option<i32>) -> Result<String, RenderError> {
    if template.is_empty() {
        return Ok(DEFAULT_PROMPT.to_string());
    }
    // The `liquid` crate is strict by default for `{{ … }}` output and `{% … %}` tags: an
    // undefined top-level variable errors ("Unknown variable"), an undefined key on a bound
    // object errors ("Unknown index"), and an unknown filter errors at parse time ("Unknown
    // filter"). This reproduces the `engine.StrictVariables()` the Go daemon sets on osteele/liquid.
    let parser = liquid::ParserBuilder::with_stdlib()
        .build()
        .map_err(|e| RenderError::Template(e.to_string()))?;
    let tpl = parser
        .parse(template)
        .map_err(|e| RenderError::Template(e.to_string()))?;

    let mut globals = liquid::Object::new();
    globals.insert("issue".into(), issue_bindings(issue));
    globals.insert("attempt".into(), attempt_binding(attempt));

    tpl.render(&globals)
        .map_err(|e| RenderError::Template(e.to_string()))
}

/// Binds `attempt` exactly as Go's `attemptBinding`: an unset attempt is the empty *string* (not
/// a Liquid nil), a present attempt is its integer. The empty-string choice is load-bearing for
/// parity: like Go's osteele/liquid (`Test()` = `value != nil && value != false`), the `liquid`
/// crate treats an empty string as *truthy*, so `{% if attempt %}` behaves identically whether
/// attempt is unset (empty string → truthy) or set. Binding a Liquid nil would make it falsy and
/// diverge from the Go daemon.
fn attempt_binding(attempt: Option<i32>) -> Value {
    match attempt {
        None => Value::scalar(String::new()),
        Some(n) => Value::scalar(i64::from(n)),
    }
}

/// Exposes issue fields as a string-keyed object (upstream §12.2), replicating Go's
/// `issueBindings` field-for-field. Every key is always present with a concrete value so strict
/// rendering treats an unset optional as empty (`""`) rather than an undefined-index error; only
/// keys outside this set (e.g. `issue.bogus`, or non-exposed `Issue` fields like `team_id`) error
/// under strict mode.
fn issue_bindings(issue: &Issue) -> Value {
    let mut m = liquid::Object::new();
    m.insert("id".into(), Value::scalar(issue.id.clone()));
    m.insert("identifier".into(), Value::scalar(issue.identifier.clone()));
    m.insert("title".into(), Value::scalar(issue.title.clone()));
    m.insert("description".into(), opt_str(&issue.description));
    m.insert("priority".into(), opt_int(issue.priority));
    m.insert("state".into(), Value::scalar(issue.state.clone()));
    m.insert("branch_name".into(), opt_str(&issue.branch_name));
    m.insert("url".into(), opt_str(&issue.url));
    m.insert("labels".into(), labels_binding(issue.labels.as_deref()));
    m.insert(
        "blocked_by".into(),
        blockers_binding(issue.blocked_by.as_deref()),
    );
    m.insert("created_at".into(), opt_time(issue.created_at));
    m.insert("updated_at".into(), opt_time(issue.updated_at));
    Value::Object(m)
}

/// Blocker refs as a list of `{id, identifier, state}` objects, mirroring Go's `blockerBindings`
/// (each field deref'd to its string or `""`).
fn blockers_binding(blockers: Option<&[BlockerRef]>) -> Value {
    let items = blockers.unwrap_or_default().iter().map(|b| {
        let mut bm = liquid::Object::new();
        bm.insert("id".into(), opt_str(&b.id));
        bm.insert("identifier".into(), opt_str(&b.identifier));
        bm.insert("state".into(), opt_str(&b.state));
        Value::Object(bm)
    });
    Value::array(items)
}

/// Labels as a Liquid array; a nil slice becomes an empty array (0 iterations), mirroring Go's
/// `"labels": i.Labels` over a nil `[]string`.
fn labels_binding(labels: Option<&[String]>) -> Value {
    Value::array(
        labels
            .unwrap_or_default()
            .iter()
            .map(|s| Value::scalar(s.clone())),
    )
}

/// Mirrors Go `derefStr`: the string, or `""` when unset.
fn opt_str(p: &Option<String>) -> Value {
    Value::scalar(p.clone().unwrap_or_default())
}

/// Mirrors Go `derefInt`: the integer, or `""` (empty string) when unset.
fn opt_int(p: Option<i64>) -> Value {
    match p {
        Some(n) => Value::scalar(n),
        None => Value::scalar(String::new()),
    }
}

/// Mirrors Go `derefTime`: an RFC3339 string, or `""` when unset. `SecondsFormat::Secs` + `use_z`
/// reproduce Go's `time.RFC3339` layout (`2006-01-02T15:04:05Z07:00`) for the UTC times the
/// tracker supplies — seconds precision, `Z` zone.
fn opt_time(p: Option<DateTime<Utc>>) -> Value {
    match p {
        Some(t) => Value::scalar(t.to_rfc3339_opts(SecondsFormat::Secs, true)),
        None => Value::scalar(String::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rhapsody_core::{BlockerRef, Issue};

    /// Mirrors Go `prompt.sampleIssue` (render_test.go).
    fn sample_issue() -> Issue {
        Issue {
            id: "abc".to_string(),
            identifier: "MT-649".to_string(),
            title: "Broken login".to_string(),
            description: Some("Fix the thing".to_string()),
            priority: Some(2),
            state: "Todo".to_string(),
            labels: Some(vec!["bug".to_string(), "auth".to_string()]),
            blocked_by: Some(vec![BlockerRef {
                id: None,
                identifier: Some("MT-1".to_string()),
                state: Some("Done".to_string()),
            }]),
            ..Default::default()
        }
    }

    /// A sparse issue: only the always-present fields set, every optional `None`. Mirrors the
    /// zero-optional issue used by Go `TestRenderNilOptionalFieldsRenderEmpty` /
    /// `TestRenderExampleWorkflowWithNilDescription`.
    fn sparse_issue() -> Issue {
        Issue {
            id: "abc".to_string(),
            identifier: "MT-649".to_string(),
            title: "Broken login".to_string(),
            state: "Todo".to_string(),
            ..Default::default()
        }
    }

    // Mirrors Go `TestRenderBasicFields`.
    #[test]
    fn render_basic_fields() {
        let out = render(
            "Work on {{ issue.identifier }}: {{ issue.title }}",
            &sample_issue(),
            None,
        )
        .expect("render");
        assert_eq!(out, "Work on MT-649: Broken login");
    }

    // Mirrors Go `TestRenderLabelsAndAttempt`.
    #[test]
    fn render_labels_and_attempt() {
        let out = render(
            "attempt={{ attempt }} labels={% for l in issue.labels %}{{ l }},{% endfor %}",
            &sample_issue(),
            Some(3),
        )
        .expect("render");
        assert_eq!(out, "attempt=3 labels=bug,auth,");
    }

    // Mirrors Go `TestRenderNilAttempt`.
    #[test]
    fn render_nil_attempt() {
        let out = render("attempt={{ attempt }}", &sample_issue(), None).expect("render");
        assert_eq!(out, "attempt=");
    }

    // Mirrors Go `TestRenderUnknownVariableFails`.
    #[test]
    fn render_unknown_variable_fails() {
        assert!(
            render("{{ nope }}", &sample_issue(), None).is_err(),
            "expected error for unknown variable",
        );
    }

    // Mirrors Go `TestRenderUnknownNestedKeyFails`: an unknown key on the bound `issue` object
    // errors under strict-variables mode.
    #[test]
    fn render_unknown_nested_key_fails() {
        assert!(
            render("{{ issue.bogus }}", &sample_issue(), None).is_err(),
            "expected error for unknown nested map key under strict variables",
        );
    }

    // Mirrors Go `TestRenderNilOptionalFieldsRenderEmpty`: known fields with unset (`None`) values
    // render as empty, not an error — the regression this strict-but-always-present design fixes.
    #[test]
    fn render_nil_optional_fields_render_empty() {
        let tmpl = "d=[{{ issue.description }}] p=[{{ issue.priority }}] u=[{{ issue.url }}] \
                    b=[{{ issue.branch_name }}] c=[{{ issue.created_at }}] up=[{{ issue.updated_at }}]";
        let out = render(tmpl, &sparse_issue(), None).expect("render nil optionals");
        assert_eq!(out, "d=[] p=[] u=[] b=[] c=[] up=[]");
    }

    // Not a Go render_test case, but locks the value-level `issueBindings` surface that
    // render_test.go leaves unexercised and the ticket requires "field-for-field": an integer
    // `priority`, `branch_name`/`url`, the RFC3339 `created_at` (Go's `time.RFC3339` renders a UTC
    // time as `…Z` at seconds precision — chrono's default `to_rfc3339` would emit `+00:00` and
    // diverge), and `blocked_by` iteration with its nested `identifier`/`state` keys.
    #[test]
    fn render_populated_binding_surface() {
        use chrono::{TimeZone, Utc};
        let issue = Issue {
            id: "abc".to_string(),
            identifier: "MT-649".to_string(),
            title: "Broken login".to_string(),
            priority: Some(2),
            state: "Todo".to_string(),
            branch_name: Some("feat/mt-649".to_string()),
            url: Some("https://linear.app/x/MT-649".to_string()),
            created_at: Some(Utc.with_ymd_and_hms(2020, 1, 2, 3, 4, 5).unwrap()),
            blocked_by: Some(vec![BlockerRef {
                id: None,
                identifier: Some("MT-1".to_string()),
                state: Some("Done".to_string()),
            }]),
            ..Default::default()
        };
        let tmpl = "id={{ issue.id }} p={{ issue.priority }} b={{ issue.branch_name }} \
                    u={{ issue.url }} c={{ issue.created_at }} \
                    bl={% for b in issue.blocked_by %}{{ b.identifier }}/{{ b.state }};{% endfor %}";
        let out = render(tmpl, &issue, None).expect("render populated");
        assert_eq!(
            out,
            "id=abc p=2 b=feat/mt-649 u=https://linear.app/x/MT-649 c=2020-01-02T03:04:05Z bl=MT-1/Done;"
        );
    }

    // Mirrors Go `TestRenderUnknownFilterFails`.
    #[test]
    fn render_unknown_filter_fails() {
        assert!(
            render("{{ issue.title | no_such_filter }}", &sample_issue(), None).is_err(),
            "expected error for unknown filter",
        );
    }

    // Mirrors Go `TestRenderEmptyBodyFallback`.
    #[test]
    fn render_empty_body_fallback() {
        let out = render("", &sample_issue(), None).expect("render empty");
        assert!(
            out.contains("working on an issue from Linear"),
            "fallback prompt = {out:?}",
        );
    }

    // Adapted from Go `TestRenderExampleWorkflowWithNilDescription`. The Go test renders
    // `$REF/WORKFLOW.example.md`'s body against a nil-description issue; CI runners have no `$REF`
    // (P1 plan), so we render the committed capture-workflow prompt bodies — the plan's designated
    // golden inputs — proving every real user template renders cleanly against a sparse issue. The
    // front-matter split is a minimal test helper; the real loader is Task C2, which C7 does not
    // depend on.
    #[test]
    fn render_committed_workflow_bodies_with_sparse_issue() {
        for wf in ["minimal", "full", "graphite"] {
            let path = format!("../../harness/capture/workflows/{wf}.md");
            let contents =
                std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
            let body = workflow_body(&contents);
            render(&body, &sparse_issue(), None)
                .unwrap_or_else(|e| panic!("render {wf} body: {e}"));
        }
    }

    // Additional assertion on the C7 deliverable contract (ticket: "unknown variable/filter →
    // error with `template_render_error: ` prefix"). Not a Go render_test case, but the prefix is
    // the observable error contract, so it is pinned here.
    #[test]
    fn render_error_display_has_template_render_error_prefix() {
        let err = render("{{ nope }}", &sample_issue(), None).expect_err("expected error");
        assert!(
            err.to_string().starts_with("template_render_error: "),
            "display = {err}",
        );
    }

    /// Minimal front-matter split for the committed capture workflows: returns the prompt body
    /// (everything after the leading `---`…`---` YAML block), trimmed. Test-scoped — the real
    /// WORKFLOW.md loader is Task C2.
    fn workflow_body(contents: &str) -> String {
        let mut lines = contents.lines();
        if lines.next().map(str::trim_end) != Some("---") {
            return contents.trim().to_string(); // no front matter
        }
        for line in lines.by_ref() {
            if line.trim_end() == "---" {
                break;
            }
        }
        lines.collect::<Vec<_>>().join("\n").trim().to_string()
    }
}
