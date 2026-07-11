//! Tracker construction — port of Go `internal/tracker/factory.go`.

use crate::{Tracker, file, linear};

/// Spec is the union of construction inputs across the tracker call sites. `kind` selects the
/// adapter; the remaining fields are populated as each adapter needs (the file adapter ignores
/// `endpoint`/`api_key`; the banner site passes only the linear subset). It is the single place
/// the daemon decides which tracker implementation to build (INF-303).
#[derive(Debug, Clone, Default)]
pub struct Spec {
    pub kind: String,
    pub endpoint: String,
    pub api_key: String,
    pub project_slug: String,
    /// Path to the JSON issue file when `kind == "file"`.
    pub source: String,
    pub active_states: Vec<String>,
    pub review_states: Vec<String>,
    pub summon_token: String,
    pub milestone: String,
    /// The resolved ticket-claim policy ("assignee" | "pool"). In "pool" the Linear adapter's
    /// candidate query filters UNASSIGNED issues instead of assignee == viewer; every other query
    /// is unchanged. Empty is treated as "assignee". INF-477.
    pub claim_mode: String,
}

/// Builds the [`Tracker`] for `spec.kind`, switching on the configured tracker kind (mirrors Go's
/// `tracker.New`). Kinds are validated up front by config's dispatch validation, so an
/// unknown/empty kind never reaches here; it falls back to the Linear adapter (the historical
/// default) for safety.
pub fn new(spec: Spec) -> Box<dyn Tracker> {
    match spec.kind.as_str() {
        "file" => Box::new(file::new(file::Config {
            source: spec.source,
            project_slug: spec.project_slug,
            active_states: spec.active_states,
            review_states: spec.review_states,
            summon_token: spec.summon_token,
            milestone: spec.milestone,
        })),
        _ => Box::new(linear::new(linear::Config {
            endpoint: spec.endpoint,
            api_key: spec.api_key,
            project_slug: spec.project_slug,
            active_states: spec.active_states,
            review_states: spec.review_states,
            summon_token: spec.summon_token,
            milestone: spec.milestone,
            claim_mode: spec.claim_mode,
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::any::Any;

    /// Downcasts a boxed tracker to its concrete adapter, the Rust equivalent of Go's
    /// `.(*file.Tracker)` / `.(*linear.Client)` type assertions (the trait's `Any` supertrait
    /// makes the `&dyn Tracker -> &dyn Any` upcast available).
    fn is<T: Any>(t: &dyn Tracker) -> bool {
        (t as &dyn Any).is::<T>()
    }

    // Mirrors Go `tracker.TestNewRoutesOnKind`.
    #[test]
    fn new_routes_on_kind() {
        let t = new(Spec {
            kind: "file".into(),
            source: "/tmp/x.json".into(),
            ..Spec::default()
        });
        assert!(
            is::<file::Tracker>(&*t),
            "kind: file must build a file::Tracker"
        );

        let t = new(Spec {
            kind: "linear".into(),
            api_key: "tok".into(),
            project_slug: "p".into(),
            ..Spec::default()
        });
        assert!(
            is::<linear::Client>(&*t),
            "kind: linear must build a linear::Client"
        );

        // Empty/unknown kind falls back to the historical default (Linear); validation rejects
        // unknown kinds upstream, so `new` is never reached with one in practice.
        let t = new(Spec {
            kind: String::new(),
            ..Spec::default()
        });
        assert!(
            is::<linear::Client>(&*t),
            "empty kind must fall back to linear::Client"
        );
    }
}
