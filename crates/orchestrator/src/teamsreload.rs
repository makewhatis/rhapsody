//! teamsreload — the live `teams.yaml` ROUTING reload (STUDIO-1976). Rhapsody-only; no Go
//! counterpart.
//!
//! `teams.yaml` is otherwise boot-loaded, and a routing change therefore used to need a restart —
//! which on a busy daemon means draining first, exactly the wait the ticket exists to remove. This
//! module watches the file (mtime polling, mirroring [`crate::reload`]'s WORKFLOW.md watcher — no
//! new dependency) and, on every change, re-loads and VALIDATES it the same way as at boot, then
//! folds ONLY its routing fields into the live config through
//! [`Teams::apply_routing_overlay`](rhapsody_config::teams::Teams::apply_routing_overlay):
//!
//! * `review.model` / `review.provider` / `review.effort` (per harness);
//! * `manager.harness` / `manager.provider` / `manager.model` / `manager.effort`;
//! * each roster identity's own `harness` / `provider` / `model` / `effort`.
//!
//! Roster membership, labels, quorum, review mode and memory stay boot-loaded — a change to those
//! still needs a restart, which is the documented boundary.
//!
//! An invalid file is REFUSED and the last-good routing is kept (logged at `warn!`; the Settings
//! `/api/v1/teams/config` read also reports the reason), so a bad edit can never degrade Teams to
//! off. The reload runs ON the control task, so it never races a dispatch.
//!
//! The control task's `teams` field is not the only consumer of routing: the off-loop triage task
//! holds a boot-captured `Arc<Teams>` and reads `manager.model` for its one-shot turn. A
//! [`TeamsRoutingSink`] is the seam the composition root CAN install to hand that consumer the same
//! routing the control task just applied — the `ProviderReloadSink` pattern, for the same reason.
//! The control-task consumers (review dispatch and the isolated manager run) read the live config
//! directly and need no seam.

use std::sync::Arc;
use std::time::Duration;

use rhapsody_config::teams::Teams;

use crate::control_loop::{CancelWait, Event};
use crate::orchestrator::Orchestrator;

/// The mtime-poll cadence, matching [`crate::reload`]'s `WATCH_POLL_INTERVAL` (Go's fsnotify is
/// instant; a poll is the no-new-dependency tradeoff the WORKFLOW.md watcher already records).
const WATCH_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// The daemon's off-loop observer of a teams ROUTING reload (STUDIO-1976). Synchronous and cheap:
/// it must do NO I/O and must not await, because it is called on the control task. The daemon's
/// implementation writes an in-memory routing cell the off-loop triage task reads.
pub trait TeamsRoutingSink: Send + Sync {
    /// The live config just had its routing fields replaced. `teams` is the full config (the sink
    /// reads only the routing fields it needs).
    fn teams_routing_reload(&self, teams: &Teams);
}

/// The production sink for the manager arbiter (STUDIO-1976 review B1): hands a reloaded
/// `teams.yaml`'s manager tuple to the arbiter so its NEXT turn resolves the live tuple. The triage
/// task's own manager model is published through [`TriageHandle`](crate::triage::TriageHandle)
/// directly by `on_teams_reload` (the control task can write that handle); this covers the arbiter,
/// which the control task cannot otherwise reach.
pub struct ManagerArbiterRoutingSink {
    arbiter: Arc<crate::managerprep::ManagerArbiter>,
}

impl ManagerArbiterRoutingSink {
    pub fn new(arbiter: Arc<crate::managerprep::ManagerArbiter>) -> Self {
        Self { arbiter }
    }
}

impl TeamsRoutingSink for ManagerArbiterRoutingSink {
    fn teams_routing_reload(&self, teams: &Teams) {
        self.arbiter
            .set_manager_fields(&crate::selection::FieldSelection {
                harness: teams.manager.harness.clone(),
                provider: teams.manager.provider.clone(),
                model: teams.manager.model.clone(),
            });
    }
}

impl Orchestrator {
    /// Installs the teams routing reload sink, before `Run` moves the orchestrator into the control
    /// task. `None` (the default for tests and any embedding with no off-loop Teams consumer) leaves
    /// the reload path byte-identical to a daemon built before the feature.
    pub fn set_teams_routing_sink(&mut self, sink: Arc<dyn TeamsRoutingSink>) {
        self.teams_routing_sink = Some(sink);
    }

    /// Watches `teams.yaml` and posts [`Event::TeamsReload`] on change, until `ctx` is cancelled.
    /// A daemon with no on-disk runtime home (or Teams off) spawns nothing. A stat failure is
    /// non-fatal — the poll keeps trying.
    pub(crate) fn start_watch_teams(&self, mut ctx: CancelWait) -> tokio::task::JoinHandle<()> {
        let path = self.teams_config_path.clone();
        let events = self.events.clone();
        tokio::spawn(async move {
            let Some(path) = path else {
                // Nothing to watch: park until cancelled so the caller's `abort()` contract holds.
                ctx.cancelled().await;
                return;
            };
            let mtime = || {
                std::fs::metadata(&path)
                    .ok()
                    .and_then(|m| m.modified().ok())
            };
            let mut last = mtime();
            let mut interval = tokio::time::interval(WATCH_POLL_INTERVAL);
            loop {
                tokio::select! {
                    _ = ctx.cancelled() => return,
                    _ = interval.tick() => {
                        let cur = mtime();
                        if cur != last {
                            last = cur;
                            if events.send(Event::TeamsReload).is_err() {
                                return; // the loop is gone
                            }
                        }
                    }
                }
            }
        })
    }

    /// Re-loads and validates `teams.yaml`, then folds its routing fields into the live config. A
    /// missing path or a Teams-off daemon is a no-op. A file that fails to load/validate keeps the
    /// last-good routing and logs the reason (never degrades Teams to off).
    pub(crate) fn on_teams_reload(&mut self) {
        let Some(path) = self.teams_config_path.clone() else {
            return;
        };
        if self.teams.is_none() {
            return;
        }
        let reloaded = match Teams::try_load(&path) {
            Ok(teams) => teams,
            Err(err) => {
                tracing::warn!(
                    err = %err,
                    path = %path.display(),
                    "teams routing reload failed; keeping last-good routing"
                );
                return;
            }
        };
        // Validate the reloaded routing against the SAME oracle the Settings write uses, so a
        // canonical-but-undefined provider or a provider on `claude` is refused here too rather than
        // replacing the last-good routing (STUDIO-1976 review B2). A config that never loaded has no
        // oracle to validate against; leave the reload to the boot path in that case.
        if let Some(cfg) = self.eff.as_ref().map(|e| &e.cfg) {
            let profiles_dir = self.teams_profiles_dir.as_deref();
            if let Err(reason) =
                crate::modelsview::validate_teams_routing(cfg, &reloaded, profiles_dir)
            {
                tracing::warn!(
                    reason = %reason,
                    path = %path.display(),
                    "teams routing reload refused; keeping last-good routing"
                );
                return;
            }
        }
        let sink = self.teams_routing_sink.clone();
        let Some(teams) = self.teams.as_mut() else {
            return;
        };
        teams.apply_routing_overlay(&reloaded);
        // Publish the manager's live tuple to the off-loop triage task (STUDIO-1976 review B1): its
        // one-shot manager turns hold no `Orchestrator`, so the control task is the only writer.
        if let Some(handle) = self.teams_triage.as_ref() {
            handle.set_manager_routing(&crate::triage::ManagerRouting {
                harness: teams.manager.harness.clone(),
                provider: teams.manager.provider.clone(),
                model: teams.manager.model.clone(),
            });
        }
        if let Some(sink) = sink {
            sink.teams_routing_reload(teams);
        }
        tracing::info!(path = %path.display(), "teams routing reloaded");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use rhapsody_config::teams::{ManagerMode, ReviewMode};
    use std::sync::Mutex;

    use crate::testsupport::TempDir;

    /// A sink that records each delivery's review model for `opencode`.
    struct RecordingSink {
        calls: Mutex<Vec<Option<String>>>,
    }

    impl RecordingSink {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
            })
        }

        fn calls(&self) -> Vec<Option<String>> {
            self.calls.lock().expect("recording sink").clone()
        }
    }

    impl TeamsRoutingSink for RecordingSink {
        fn teams_routing_reload(&self, teams: &Teams) {
            self.calls.lock().expect("recording sink").push(
                teams
                    .review
                    .model
                    .for_harness("opencode", "claude")
                    .map(str::to_string),
            );
        }
    }

    fn teams_yaml(model: &str) -> String {
        format!(
            "enabled: true\nreview:\n  mode: ticketless\n  model:\n    opencode: {model}\nmanager:\n  mode: labels\n  model: claude-opus-5\nroster:\n  - name: alice\n    profile: swe\n"
        )
    }

    fn orchestrator_with(path: std::path::PathBuf) -> Orchestrator {
        let mut o = Orchestrator::new("/tmp/workflow.md");
        o.teams = Some(Teams::try_load(&path).expect("boot teams"));
        o.teams_config_path = Some(path);
        o
    }

    #[test]
    fn a_reload_folds_only_routing_and_notifies_the_sink() {
        let dir = TempDir::new();
        let path = std::path::PathBuf::from(dir.child("teams.yaml"));
        std::fs::write(&path, teams_yaml("deepseek-v4p1-flash")).expect("write");
        let mut o = orchestrator_with(path.clone());
        // Boot's routing.
        assert_eq!(
            o.teams.as_ref().unwrap().review.mode,
            ReviewMode::Ticketless
        );
        let sink = RecordingSink::new();
        o.set_teams_routing_sink(sink.clone());

        // A new routing value lands on disk, then the watcher's event is handled.
        std::fs::write(&path, teams_yaml("openai/gpt-6.1-sol")).expect("write");
        o.on_teams_reload();

        let teams = o.teams.as_ref().expect("teams");
        assert_eq!(
            teams.review.model.for_harness("opencode", "claude"),
            Some("openai/gpt-6.1-sol")
        );
        assert_eq!(sink.calls(), vec![Some("openai/gpt-6.1-sol".to_string())]);
    }

    #[test]
    fn a_boot_loaded_field_change_is_not_applied() {
        let dir = TempDir::new();
        let path = std::path::PathBuf::from(dir.child("teams.yaml"));
        std::fs::write(&path, teams_yaml("deepseek-v4p1-flash")).expect("write");
        let mut o = orchestrator_with(path.clone());
        // Boot config says `labels`; the reloaded file flips it to `off` AND changes the model.
        std::fs::write(
            &path,
            "enabled: true\nreview:\n  mode: ticketless\n  model:\n    opencode: openai/gpt-6.1-sol\nmanager:\n  mode: off\nroster:\n  - name: alice\n    profile: swe\n",
        )
        .expect("write");
        o.on_teams_reload();
        let teams = o.teams.as_ref().expect("teams");
        // Routing moved...
        assert_eq!(
            teams.review.model.for_harness("opencode", "claude"),
            Some("openai/gpt-6.1-sol")
        );
        // ...but the boot-loaded `manager.mode` did not.
        assert_eq!(teams.manager.mode, ManagerMode::Labels);
    }

    #[test]
    fn an_invalid_reload_keeps_the_last_good_routing() {
        let dir = TempDir::new();
        let path = std::path::PathBuf::from(dir.child("teams.yaml"));
        std::fs::write(&path, teams_yaml("deepseek-v4p1-flash")).expect("write");
        let mut o = orchestrator_with(path.clone());
        // A file that parses but fails validation (a non-canonical provider).
        std::fs::write(
            &path,
            "enabled: true\nreview:\n  mode: ticketless\n  provider:\n    opencode: not a provider!\nmanager:\n  mode: labels\n  model: claude-opus-5\nroster:\n  - name: alice\n    profile: swe\n",
        )
        .expect("write");
        o.on_teams_reload();
        assert_eq!(
            o.teams
                .as_ref()
                .unwrap()
                .review
                .model
                .for_harness("opencode", "claude"),
            Some("deepseek-v4p1-flash")
        );
    }

    /// A CANONICAL but UNDEFINED provider is not syntactically invalid, so `Teams::validate` accepts
    /// it — the reload must still refuse it against the workflow's provider registry and keep the
    /// last-good routing (STUDIO-1976 review B2).
    #[test]
    fn a_reload_with_a_canonical_undefined_provider_keeps_the_last_good_routing() {
        let dir = TempDir::new();
        let path = std::path::PathBuf::from(dir.child("teams.yaml"));
        std::fs::write(&path, teams_yaml("deepseek-v4p1-flash")).expect("write");
        let mut o = orchestrator_with(path.clone());
        // The workflow defines `testprovider`, never `not-defined`.
        o.eff = Some(crate::testsupport::empty_effective(std::sync::Arc::new(
            rhapsody_tracker::fake::Fake::default(),
        )));
        std::fs::write(
            &path,
            "enabled: true\nreview:\n  mode: ticketless\n  provider:\n    opencode: not-defined\n  model:\n    opencode: new-model\nmanager:\n  mode: labels\n  model: claude-opus-5\nroster:\n  - name: alice\n    profile: swe\n",
        )
        .expect("write");
        o.on_teams_reload();
        assert_eq!(
            o.teams
                .as_ref()
                .unwrap()
                .review
                .model
                .for_harness("opencode", "claude"),
            Some("deepseek-v4p1-flash"),
            "a canonical-but-undefined provider must not replace the last-good routing"
        );
    }

    /// The production watcher posts `Event::TeamsReload` when the file's mtime moves, and
    /// `on_teams_reload` (the loop's handler) folds the change in — the wiring a direct call to
    /// `on_teams_reload` alone does not exercise (STUDIO-1976 review B5).
    #[tokio::test]
    async fn the_watcher_posts_a_teams_reload_event_and_the_handler_applies_it() {
        let dir = TempDir::new();
        let path = std::path::PathBuf::from(dir.child("teams.yaml"));
        std::fs::write(&path, teams_yaml("deepseek-v4p1-flash")).expect("write");
        let mut o = orchestrator_with(path.clone());
        let mut rx = o.take_events_rx().expect("events rx");
        let signal = crate::control_loop::CancelSignal::new();
        let watch = o.start_watch_teams(signal.wait());

        // Let the watcher take its baseline, then move the mtime.
        tokio::time::sleep(Duration::from_millis(1100)).await;
        std::fs::write(&path, teams_yaml("openai/gpt-6.1-sol")).expect("write");

        let ev = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("the watcher must post a reload event")
            .expect("the channel stays open");
        assert!(matches!(ev, Event::TeamsReload));
        o.on_teams_reload();
        assert_eq!(
            o.teams
                .as_ref()
                .unwrap()
                .review
                .model
                .for_harness("opencode", "claude"),
            Some("openai/gpt-6.1-sol")
        );
        watch.abort();
        signal.cancel();
    }
}
