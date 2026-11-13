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
//! [`TeamsRoutingSink`] is the seam the composition root installs to hand that consumer the same
//! routing the control task just applied — the `ProviderReloadSink` pattern, for the same reason.

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
        let sink = self.teams_routing_sink.clone();
        let Some(teams) = self.teams.as_mut() else {
            return;
        };
        teams.apply_routing_overlay(&reloaded);
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
}
