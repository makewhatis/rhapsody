//! telemetry_attrs — parity port of Go `internal/orchestrator/telemetry_attrs.go`.
//!
//! Bounded metric-label builders (the cardinality contract, design spec §cross-cutting #1): every
//! metric attribute is one of project / model / harness / provider / outcome / reason. NEVER add an
//! issue/run/session identifier here — high-cardinality identity belongs on spans and logs. The
//! label values come from data already in scope (the running entry / worker deps); an empty
//! slug/model (legacy single-project or test-injected paths) records as an empty-string label,
//! which stays bounded.
//!
//! The metric-emitting call sites (dispatch/worker/retry) live in later tickets (O2/O3/O5); the
//! real OpenTelemetry export wiring is P6. Go reads the attribute KEYS from its `telemetry` package
//! (`telemetry.AttrProject` etc.); the Rust port has no telemetry crate this phase, so the
//! orchestrator — the sole producer of these metrics — owns the key constants and the `Attr`
//! stand-in for now.

/// Metric attribute key: the owning project's slug.
pub const ATTR_PROJECT: &str = "project";
/// Metric attribute key: the model the run actually used. NOT "the claude model" — that single-
/// provider assumption was invalidated by the multi-harness work (STUDIO-902/909) and nothing had
/// revisited this doc (STUDIO-957).
pub const ATTR_MODEL: &str = "model";
/// Metric attribute key: the harness the run actually used (`claude` | `opencode`) — the backend
/// that produced the run, which `model` alone cannot name (STUDIO-957).
pub const ATTR_HARNESS: &str = "harness";
/// Metric attribute key: the provider account the run billed. A model name is not a substitute: the
/// quota pool is a property of the account, not the model string (STUDIO-908), so a token metric
/// without this dimension cannot answer "how much did each account spend" (STUDIO-957).
pub const ATTR_PROVIDER: &str = "provider";
/// Metric attribute key: the terminal run outcome.
pub const ATTR_OUTCOME: &str = "outcome";
/// Metric attribute key: the bounded failure reason.
pub const ATTR_REASON: &str = "reason";

/// `reason` value for a worker failure that was a stall (`issues.failed{reason=stalled}`, the
/// documented non-summable subset of `issues.failed`). Mirrors Go `reasonStalled`.
pub const REASON_STALLED: &str = "stalled";
/// `reason` value for every other worker failure (`issues.failed{reason=error}`). Mirrors Go
/// `reasonError`.
pub const REASON_ERROR: &str = "error";

/// One metric attribute (bounded key → value). The Rust stand-in for OpenTelemetry's
/// `attribute.KeyValue` (there is no telemetry crate this phase); P6 maps these onto the real
/// exporter. The key is always one of the bounded [`ATTR_PROJECT`]/[`ATTR_MODEL`]/[`ATTR_HARNESS`]/
/// [`ATTR_PROVIDER`]/[`ATTR_OUTCOME`]/[`ATTR_REASON`] constants.
pub type Attr = (&'static str, String);

/// Labels the project-only instruments (dispatched/completed/retried/stalled). Mirrors Go
/// `projectAttrs`.
pub fn project_attrs(slug: &str) -> Vec<Attr> {
    vec![(ATTR_PROJECT, slug.to_string())]
}

/// Labels `issues.failed` with the project + the bounded failure reason. Mirrors Go `failedAttrs`.
pub fn failed_attrs(slug: &str, reason: &str) -> Vec<Attr> {
    vec![
        (ATTR_PROJECT, slug.to_string()),
        (ATTR_REASON, reason.to_string()),
    ]
}

/// Labels run/turn duration histograms with project + model + terminal outcome. Mirrors Go
/// `runAttrs`.
pub fn run_attrs(slug: &str, model: &str, outcome: &str) -> Vec<Attr> {
    vec![
        (ATTR_PROJECT, slug.to_string()),
        (ATTR_MODEL, model.to_string()),
        (ATTR_OUTCOME, outcome.to_string()),
    ]
}

/// Labels the token counters with project + model + harness + provider (STUDIO-957). Additive over
/// Go's `project`/`model` pair: a model name is not a provider, so the provider dimension is what
/// makes "how much did each account spend" answerable from the metrics at all.
pub fn token_attrs(slug: &str, model: &str, harness: &str, provider: &str) -> Vec<Attr> {
    vec![
        (ATTR_PROJECT, slug.to_string()),
        (ATTR_MODEL, model.to_string()),
        (ATTR_HARNESS, harness.to_string()),
        (ATTR_PROVIDER, provider.to_string()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    // O1-authored unit coverage for the pure attr builders (the Go behavioral telemetry_test.go
    // exercises them indirectly through dispatch/worker/retry + the OTel metric subsystem, which are
    // owned by O2/O3/O5 + P6 and mirrored there). Locks the bounded-label schema: every attribute
    // key is one of project/model/outcome/reason.
    #[test]
    fn project_attrs_labels_project_only() {
        assert_eq!(
            project_attrs("alpha"),
            vec![(ATTR_PROJECT, "alpha".to_string())]
        );
    }

    #[test]
    fn failed_attrs_labels_project_and_reason() {
        assert_eq!(
            failed_attrs("alpha", REASON_STALLED),
            vec![
                (ATTR_PROJECT, "alpha".to_string()),
                (ATTR_REASON, "stalled".to_string()),
            ]
        );
    }

    #[test]
    fn run_attrs_labels_project_model_outcome() {
        assert_eq!(
            run_attrs("alpha", "opus", "completed"),
            vec![
                (ATTR_PROJECT, "alpha".to_string()),
                (ATTR_MODEL, "opus".to_string()),
                (ATTR_OUTCOME, "completed".to_string()),
            ]
        );
    }

    #[test]
    fn token_attrs_labels_project_model_harness_and_provider() {
        assert_eq!(
            token_attrs("alpha", "opus", "claude", "anthropic"),
            vec![
                (ATTR_PROJECT, "alpha".to_string()),
                (ATTR_MODEL, "opus".to_string()),
                (ATTR_HARNESS, "claude".to_string()),
                (ATTR_PROVIDER, "anthropic".to_string()),
            ]
        );
    }

    // The provider dimension is its own key, distinct from the model. Dropping it (or reusing
    // `model`) is the mutation this pins (STUDIO-957).
    #[test]
    fn provider_is_not_model() {
        assert_ne!(ATTR_PROVIDER, ATTR_MODEL);
        assert_ne!(ATTR_HARNESS, ATTR_MODEL);
        let attrs = token_attrs("alpha", "opus", "claude", "anthropic");
        assert!(attrs.iter().any(|(k, _)| *k == ATTR_PROVIDER));
    }

    // The reason constants are the exact Go string values (issues.failed's bounded subset).
    #[test]
    fn reason_constants() {
        assert_eq!(REASON_ERROR, "error");
        assert_eq!(REASON_STALLED, "stalled");
    }
}
