//! providerreload — the orchestrator's provider-set reload notification seam (STUDIO-990, P9).
//! Rhapsody-only; no Go counterpart.
//!
//! A `providers:` block is APPLIED by the daemon's composition root, not by the orchestrator: the
//! non-secret status cache and the model-catalog coordinator live in `rhapsody-provider-status` and
//! `rhapsodyd`, and this crate must not grow a dependency on them. What the orchestrator DOES own is
//! the reload itself — [`Orchestrator::reload_from_disk`](crate::orchestrator::Orchestrator) is the
//! one place a `WORKFLOW.md` change is decoded, resolved, validated and swapped — so it is the one
//! place a provider-set change can be observed.
//!
//! [`ProviderReloadSink`] is that observation: a narrow, SYNCHRONOUS callback the composition root
//! installs before `Run`. On every successful (re)load the orchestrator hands it the resolved
//! `providers:` map and the effective turn deadline. The sink must be cheap and must perform NO I/O
//! on the control task: the daemon's implementation mutates an in-memory cache and spawns the bounded
//! off-loop credential read, exactly as §6 requires ("a provider-definition reload immediately
//! invalidates the affected entry to `unknown/refreshing` and schedules the same concurrency-bounded
//! off-loop `read_bound` status refresh against the new expected binding").
//!
//! A reload that fails anywhere before the config swap keeps the last-good config and therefore
//! delivers NOTHING to the sink, so a malformed edit can never invalidate live provider status. The
//! `providers:` map is non-secret by construction (`rhapsody_config::ProviderDefinition` has no
//! value/key field), so nothing secret can cross this seam.

use std::collections::BTreeMap;
use std::sync::Arc;

use rhapsody_config::ProviderDefinition;

use crate::orchestrator::Orchestrator;

/// The daemon's provider-set reload observation. See the module docs for the contract: synchronous,
/// non-secret, no I/O on the control task.
pub trait ProviderReloadSink: Send + Sync {
    /// The workflow's resolved global `providers:` map was just swapped in, along with the effective
    /// turn deadline (`rhapsody_config::providers::provider_turn_deadline_ms`) that scopes each
    /// provider's derived capability lifetime. The map is non-secret and may be empty when a reload
    /// removes the last provider.
    fn provider_reload(
        &self,
        providers: &BTreeMap<String, ProviderDefinition>,
        turn_deadline_ms: u64,
    );
}

impl Orchestrator {
    /// Installs the provider-set reload sink, before `Run` moves the orchestrator into the control
    /// task. `None` (the default for tests and any embedding with no provider runtime) leaves the
    /// reload path byte-identical to a daemon built before the feature.
    pub fn set_provider_reload_sink(&mut self, sink: Arc<dyn ProviderReloadSink>) {
        self.provider_reload = Some(sink);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::testsupport::TempDir;

    /// A sink that records each delivery: the provider ids (sorted) and the turn deadline.
    struct RecordingSink {
        calls: Mutex<Vec<(Vec<String>, u64)>>,
    }

    impl RecordingSink {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
            })
        }

        fn calls(&self) -> Vec<(Vec<String>, u64)> {
            self.calls.lock().expect("recording sink").clone()
        }
    }

    impl ProviderReloadSink for RecordingSink {
        fn provider_reload(
            &self,
            providers: &BTreeMap<String, ProviderDefinition>,
            turn_deadline_ms: u64,
        ) {
            self.calls
                .lock()
                .expect("recording sink")
                .push((providers.keys().cloned().collect(), turn_deadline_ms));
        }
    }

    /// A minimal valid workflow carrying a `providers:` block. The tracker points at a dead loopback
    /// address and all paths stay inside the temp dir, so the test is hermetic.
    fn workflow(providers: &str) -> String {
        format!(
            "---\ntracker:\n  kind: linear\n  api_key: tok\n  project_slug: proj\nagent:\n  backend: claude\nclaude:\n  command: claude\n{providers}---\nDo {{ issue.identifier }}.\n"
        )
    }

    fn write(body: &str) -> (String, TempDir) {
        let dir = TempDir::new();
        let path = dir.child("WORKFLOW.md");
        std::fs::write(&path, body).expect("write workflow");
        (path, dir)
    }

    const ONE: &str = "providers:\n  fireworks:\n    protocol: openai-compatible\n    base_url: https://api.example/v1\n    credential:\n      source: keychain\n";

    // The reload path is the ONE place the provider set can change. A successful (re)load must hand
    // the resolved map to the sink — otherwise the daemon's status cache keeps the boot-time provider
    // map forever (B1). An implementation that skips the sink is the mutation this reds.
    #[test]
    fn a_reload_delivers_the_provider_set_to_the_sink() {
        let (path, _dir) = write(&workflow(ONE));
        let mut o = Orchestrator::new(path);
        let sink = RecordingSink::new();
        o.set_provider_reload_sink(sink.clone());
        o.reload_from_disk().expect("reload");
        assert_eq!(
            sink.calls(),
            vec![(vec!["fireworks".to_string()], 3_600_000)],
            "the reload must deliver the resolved provider set and the effective turn deadline"
        );
    }

    // A HOT reload that adds a provider must deliver the NEW set, so a daemon whose provider map was
    // applied at boot picks up the added provider without a restart.
    #[test]
    fn a_hot_reload_delivers_the_changed_provider_set() {
        let (path, _dir) = write(&workflow(ONE));
        let mut o = Orchestrator::new(path.clone());
        let sink = RecordingSink::new();
        o.set_provider_reload_sink(sink.clone());
        o.reload_from_disk().expect("reload");
        assert_eq!(sink.calls().len(), 1);

        let two = format!(
            "{ONE}  second:\n    protocol: openai-compatible\n    base_url: https://api.example/v2\n    credential:\n      source: keychain\n"
        );
        std::fs::write(&path, workflow(&two)).expect("rewrite workflow");
        o.on_reload();

        let calls = sink.calls();
        assert_eq!(calls.len(), 2, "the hot reload must reach the sink");
        assert_eq!(
            calls[1].0,
            vec!["fireworks".to_string(), "second".to_string()],
            "the sink must receive the reloaded set, not the boot-time one"
        );
    }

    // A reload that FAILS keeps the last-good config, so it must deliver nothing: a malformed edit
    // may not invalidate live provider status.
    #[test]
    fn a_failed_reload_delivers_nothing() {
        let (path, _dir) = write(&workflow(ONE));
        let mut o = Orchestrator::new(path.clone());
        let sink = RecordingSink::new();
        o.set_provider_reload_sink(sink.clone());
        o.reload_from_disk().expect("reload");
        assert_eq!(sink.calls().len(), 1);

        // Missing project_slug ⇒ validation fails ⇒ last-good config is kept.
        std::fs::write(
            &path,
            "---\ntracker:\n  kind: linear\n  api_key: tok\n---\nbody\n",
        )
        .expect("write invalid workflow");
        o.on_reload();
        assert_eq!(
            sink.calls().len(),
            1,
            "a failed reload must not deliver anything to the provider sink"
        );
    }

    // A daemon that never installs a sink keeps the reload path byte-identical: no panic, no call.
    #[test]
    fn a_reload_without_a_sink_is_a_noop() {
        let (path, _dir) = write(&workflow(ONE));
        let mut o = Orchestrator::new(path);
        o.reload_from_disk().expect("reload");
    }
}
