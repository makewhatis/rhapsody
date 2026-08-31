//! run — the daemon boot (parity port of `$REF/cmd/symphony/{main,run}.go`).
//!
//! [`run`] starts the Rhapsody daemon (and, when configured, the observability HTTP server) for the
//! workflow selected by `args`, running until `ctx` is cancelled. The boot order mirrors Go's `run`:
//! `symphony mcp` dispatch → flag parsing → single-instance run-lock → telemetry init + global
//! subscriber → orchestrator + store-open (injected before `Run`, since the Rust orchestrator defers
//! disk store-open to the daemon) → observability server bind + `runtime.json` publish → startup
//! banner → prune scheduler → the control loop → graceful shutdown/drain.
//!
//! Deviations from Go, all behavior-preserving:
//!   * Go threads a `*slog.Logger`; the Rust crates log through `tracing`'s global subscriber, so the
//!     boot installs `tel.subscriber()` as the process default (best-effort: a repeat install in a
//!     test process is ignored) instead of passing a logger to `orchestrator.New` / `httpapi.New`.
//!   * Go type-asserts the stderr writer to `*os.File` for the banner TTY check; the Rust boot
//!     receives `is_terminal` explicitly (a generic `MakeWriter` cannot be introspected as a TTY).
//!   * The observability server + control loop + prune scheduler run as tokio tasks (Go goroutines);
//!     `run` awaits the control loop and drives the drain on `ctx` cancel.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tracing_subscriber::fmt::MakeWriter;

use rhapsody_config::{Config, Otel, decode, resolve, workflow};
use rhapsody_core::runtimeport;
use rhapsody_orchestrator::{CancelSignal, CancelWait, Orchestrator, TriageHandle};
use rhapsody_telemetry as telemetry;
use rhapsody_tracker as tracker;

use crate::bootcfg::{
    assignee_label, banner_color_enabled, open_store, resolve_banks_dir, resolve_banner_data,
    resolve_capabilities_path, resolve_profiles_dir, resolve_room_dir, resolve_server_port,
    resolve_teams_path, resolve_teams_reconcile_path, workflow_dir,
};
use crate::logsource::LogBufferSource;
use crate::otel::resolve_otel_config;
use crate::state::DaemonState;

/// Bounds the observability server drain at daemon shutdown (Go's `srv.Shutdown` 5s ctx).
const SHUTDOWN_DRAIN: Duration = Duration::from_secs(5);

/// Starts the Rhapsody daemon for the workflow selected by `args`, running until `ctx` is cancelled.
/// Returns the process exit code (Go's `run` `int`): `0` on a clean shutdown, `1` on a fatal boot /
/// run error, `2` on a flag-parse error. `stderr` is any `MakeWriter` (the binary passes
/// `std::io::stderr`; tests pass a capture buffer); `is_terminal` reports whether that stream is a
/// TTY (for the banner's ANSI-color decision). `install_probe` installs the production dispatch
/// credential-liveness probe (BO-59) — `true` for the real binary, `false` for the hermetic daemon
/// tests, which must not shell out to a real `claude`. Mirrors Go `run`.
pub async fn run<W>(
    ctx: CancelWait,
    args: &[String],
    stderr: W,
    is_terminal: bool,
    install_probe: bool,
) -> i32
where
    W: for<'a> MakeWriter<'a> + Clone + Send + Sync + 'static,
{
    // `rhapsodyd mcp [WORKFLOW.md]` runs the local MCP facade over stdio instead of the daemon
    // (INF-473). Dispatched at the very top so the daemon's run-lock / flag parsing is untouched and
    // `rhapsodyd <workflow>` behaves identically. Mirrors Go `run`'s `args[0] == "mcp"` branch.
    if args.first().map(String::as_str) == Some("mcp") {
        return crate::mcp::run_mcp(ctx, &args[1..], stderr).await;
    }

    // `rhapsodyd teams <show|fork> <name>` inspects and forks Rhapsody Teams profiles instead of
    // running the daemon (STUDIO-642; design record `~/.rhapsody/docs/STUDIO-572-rhapsody-teams.md`,
    // §4). Dispatched here for the same reason `mcp` is: the daemon's run-lock and flag parsing stay
    // untouched, and `rhapsodyd <workflow>` behaves identically. Rhapsody-only — Go v0.4.0 has no
    // Teams feature and therefore no counterpart verb.
    if args.first().map(String::as_str) == Some("teams") {
        return crate::teams::run_teams(&args[1..], std::io::stdout(), stderr.make_writer());
    }

    let flags = match parse_flags(args) {
        Ok(f) => f,
        Err(msg) => {
            let _ = writeln!(stderr.make_writer(), "symphony: {msg}");
            return 2;
        }
    };

    // Single-instance guard: refuse a second daemon for the SAME config (an exclusive advisory flock
    // keyed on the workflow path). Held for the process lifetime; the OS releases it on exit.
    let _lock = match crate::runlock::acquire_single_instance_lock(&flags.path) {
        Ok(l) => l,
        Err(e) => {
            let _ = writeln!(stderr.make_writer(), "symphony: {e}");
            return 1;
        }
    };

    // Resolve telemetry config (best-effort load; a bad load → env-only telemetry, the daemon's own
    // load reports config errors). The synthetic default applies `OTEL_*` env even when the workflow
    // fails to load OR decode, so env-only telemetry configuration still takes effect on a config error.
    let otel_cfg = resolve_boot_otel(&flags.path);
    // Resolve the process-log dir (TRA-267): the daemon writes rotating file logs into `logging.dir`
    // (default `~/.rhapsody/logs`), independent of OTLP export. Best-effort, like `resolve_boot_otel`.
    let log_dir = resolve_boot_logdir(&flags.path);
    let tel = telemetry::init(&otel_cfg, Some(&log_dir), stderr.clone());
    // Install the composed subscriber as the process default so the orchestrator + server tasks log
    // through it (Go passes `tel.Logger` explicitly; the Rust crates use the global `tracing`
    // subscriber). Best-effort: a repeat install (a test process running `run` more than once) is a
    // no-op — the boot never depends on which invocation's subscriber wins, since the banner + fatal
    // lines are written to `stderr` directly, not through `tracing`.
    let _ = tracing::dispatcher::set_global_default(tel.subscriber());

    // The fleet hub is HTTP-only; grpc paths 404. Warn once when grpc is selected + export is on.
    if otel_cfg.enabled && otel_cfg.protocol == "grpc" {
        tracing::warn!(
            endpoint = %otel_cfg.endpoint,
            "otel protocol is grpc, but the internal fleet hub is HTTP-only (gRPC paths 404); exports will fail unless this points at a gRPC collector"
        );
    }

    let mut o = Orchestrator::new(flags.path.to_string_lossy().into_owned());
    // Open the durable store from the resolved config + --db / --no-store, and inject it before Run
    // (the Rust orchestrator defers disk store-open to the daemon). A best-effort load failure leaves
    // the config `None`, so open_store falls back to Noop and Run's own reload reports the error.
    let resolved = load_resolved(&flags.path);
    // `durable_store` is false for every fallback — storage off, --no-store, :memory:, AND a
    // failed open. Only the one reader that would ACT on an absence uses it (the Teams
    // identity-label reconcile, STUDIO-672); everything else is guard-free by design.
    let (store, durable_store) = open_store(resolved.as_ref(), &flags.db, flags.no_store);
    o.set_store(store);
    // Load the agent-capabilities registry (~/.rhapsody/capabilities.yaml, colocated with the durable
    // store), seeding defaults on first run, and inject it before Run (BO-12). Best-effort: a load
    // failure — or no on-disk store home (--no-store / off / :memory:) — leaves the registry `None`, so
    // capability rendering is a no-op, never a startup failure. Reuses the SAME resolved store path the
    // store-open above uses for `rhapsody.db`, which keeps the run tests (storage off) hermetic.
    if let Some(caps_path) = resolve_capabilities_path(resolved.as_ref(), &flags.db, flags.no_store)
    {
        match rhapsody_config::capabilities::load_or_seed(&caps_path) {
            Ok(reg) => o.capabilities_registry = Some(reg),
            Err(e) => tracing::warn!(
                err = %e,
                path = %caps_path.display(),
                "capabilities registry load failed; capabilities disabled"
            ),
        }
    }
    // Load the Rhapsody Teams config (~/.rhapsody/teams.yaml, colocated with the durable store) and
    // inject it before Run (STUDIO-639; design record ~/.rhapsody/docs/STUDIO-572-rhapsody-teams.md).
    // Best-effort, and NEVER seeded: an absent file is the off state and the shipped state (§2.1), so
    // unlike the capabilities registry above this never creates the file it reads. A malformed or
    // invalid file yields `Teams::disabled()` plus ONE loud log line — never a startup failure. No
    // on-disk store home (--no-store / off / :memory:) leaves the field `None`. STUDIO-643 makes
    // `dispatch_issue` its first consumer (routing); the profiles directory is resolved alongside so
    // a routed identity's profile can be rendered into the turn-1 prompt.
    let mut teams_cfg = rhapsody_config::teams::Teams::disabled();
    // The off-loop memory prefetch wiring (STUDIO-660, T8): `Some` only for
    // `memory.backend: hindsight` with a usable endpoint. Carried out of the Teams block so the
    // task can be spawned beside triage and the quorum, where every other Teams task lives.
    let mut teams_prefetch: Option<PrefetchWiring> = None;
    if let Some(teams_path) = resolve_teams_path(resolved.as_ref(), &flags.db, flags.no_store) {
        teams_cfg = match rhapsody_config::teams::Teams::try_load(&teams_path) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(
                    err = %e,
                    path = %teams_path.display(),
                    "teams config load failed; teams disabled"
                );
                rhapsody_config::teams::Teams::disabled()
            }
        };
        o.teams = Some(teams_cfg.clone());
        o.teams_profiles_dir = resolve_profiles_dir(resolved.as_ref(), &flags.db, flags.no_store);
        report_profile_issues(o.teams.as_ref(), &teams_path);
        report_inert_manager(o.teams.as_ref());
        // Rhapsody Teams memory (STUDIO-645, T4). Two handles are installed, deliberately DIFFERENT
        // types, and the difference is the design:
        //
        // * `teams_bank` is the CONCRETE `LocalBank` the dispatch path recalls from. It is `Some`
        //   only for `backend: local`, so a remote backend cannot end up on the control task —
        //   `dispatch_issue` is `fn` and could not await one anyway (§5.2's hard rule).
        // * `teams_memory` is the `dyn MemoryBackend` the OFF-LOOP `/api/v1/teams/*` handlers drive.
        //
        // Neither creates anything: the banks directory appears on the first `retain` (§2.1).
        //
        // Rhapsody Teams room (STUDIO-650, T5) is installed alongside on the same terms and with
        // the same split: `teams_room` + `teams_cursors` are the CONCRETE local types the dispatch
        // path catches up from, and the shared `TeamsMemory` gets a `dyn RoomLog` for the off-loop
        // `GET /api/v1/teams/room`. Nothing is created: the room directory appears on the first
        // append (which in this slice means the first triage post) and a cursor file only after a
        // catch-up that actually rendered messages.
        // STUDIO-660, T8 adds a THIRD handle for `backend: hindsight` only, and it is the same
        // split one more time: `teams_prefetch` is a bounded map of plain-data `Fact`s that the
        // dispatch path reads non-blockingly, and the remote client that fills it lives on the
        // off-loop prefetch task spawned below. `teams_bank` stays `None` for hindsight, so the
        // control task still cannot reach a network backend by any path.
        teams_prefetch = install_teams_memory(&mut o, &teams_cfg, resolved.as_ref(), &flags);
    }
    // Install the production dispatch credential-liveness probe (BO-59): before each dispatch the
    // control loop probes `claude -p 'reply with exactly: OK'` through the SAME scrubbed environment the
    // dispatched children get, so a dead agent credential (e.g. an expired Claude OAuth login) skips
    // dispatch WITHOUT claiming — an infrastructure fault fails fast instead of claim→dispatch→die every
    // ~5 min. Gated off for the hermetic daemon tests, which must not shell out to a real `claude`.
    if install_probe {
        o.set_credential_probe(std::sync::Arc::new(
            rhapsody_orchestrator::ClaudeCredentialProbe,
        ));
    }

    // Install the lifetime ctx BEFORE snapshotting the off-loop handle, so the handle's stop/resume/
    // message reply-waits are bounded by the real ctx (not the never-cancelling default). `Run`
    // re-sets the same ctx below.
    o.set_ctx(ctx.clone());

    // --- Rhapsody Teams review quorum (STUDIO-659, slice T7; design record §0.6, §0.12) ---
    //
    // The channel a handoff hands its fan-out to. Created BEFORE `o.control()` so the sender is
    // snapshotted onto the handle that actually performs the handoff, and created ONLY when the
    // quorum is on: with `quorum.enabled: false` (the default) `o.quorum_tx` stays `None`, so a
    // handoff cannot even represent a fan-out. §0.12's cost control, enforced by construction.
    let quorum_rx = spawn_quorum(&teams_cfg).then(|| o.open_quorum_channel());

    // --- the Teams work-assignment seam (STUDIO-669; design record
    // ~/.rhapsody/docs/STUDIO-668-multi-team.md §A) ---
    //
    // Built HERE, before `o.control()` moves the orchestrator into the control task, because BOTH
    // ends need the same `Arc`: the control task kicks a triage cycle the moment its selection gate
    // holds an unassigned candidate, and reads the pending-assignment map when a label write has
    // failed. The triage task below takes the other end.
    //
    // It is created on exactly the condition that spawns the triage task, and that is the safety
    // property, not a tidiness one: `o.teams_triage` is `None` on every boot that has no manager,
    // and the gate holds nothing without it. A daemon can never hold work for a triage task that
    // was not started.
    let triage_seam =
        spawn_triage(install_probe, &teams_cfg).then(|| Arc::new(TriageHandle::new()));
    o.teams_triage = triage_seam.clone();

    // The off-loop HTTP surface, snapshotted BEFORE the orchestrator moves into the control-loop task.
    let handle = o.control();

    // The observability server + prune scheduler run until `run` decides to exit. Their shutdown is a
    // SEPARATE signal `run` cancels AFTER the control loop returns — mirroring Go's `pruneCancel` +
    // `defer srv.Shutdown`, so they stop on a normal ctx-cancel AND on a fast-fail exit (a bad config
    // where `o.run` returns before the top-level ctx is ever cancelled), never hanging the drain.
    let shutdown = CancelSignal::new();

    // --- observability server (optional, upstream §13.7) ---
    let mut dashboard_url = String::new();
    let mut server_task = None;
    let mut runtime_port_published = false;
    if let (eff_port, true) = resolve_server_port(flags.port, &flags.path) {
        // The enable flow (STUDIO-652) reads and writes the SAME `teams.yaml` the boot load above
        // resolved, so `GET/POST /api/v1/teams/config` and the daemon can never disagree about
        // which file matters. `None` (no on-disk runtime home) leaves it empty, and the endpoint
        // reports that rather than guessing a path.
        let teams_config_path = resolve_teams_path(resolved.as_ref(), &flags.db, flags.no_store)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let provider =
            Arc::new(DaemonState::new(handle.clone()).with_teams_config_path(teams_config_path));
        let logs = Arc::new(LogBufferSource::new(tel.logs.clone()));
        match rhapsody_httpapi::Server::bind(provider, Some(logs), &format!("127.0.0.1:{eff_port}"))
            .await
        {
            Ok(server) => {
                let addr = match server.local_addr() {
                    Ok(a) => a,
                    Err(e) => {
                        let _ =
                            writeln!(stderr.make_writer(), "symphony: observability server: {e}");
                        return 1;
                    }
                };
                tracing::info!(%addr, "observability server listening");
                dashboard_url = format!("http://{addr}");
                // Publish the ACTUAL bound loopback port so `rhapsodyd mcp` reaches a daemon launched
                // on a dynamic/ephemeral --port (best-effort; removed on clean shutdown).
                match server.publish_runtime_port() {
                    Ok(()) => runtime_port_published = true,
                    Err(e) => {
                        tracing::warn!(err = %e, "could not write runtime port file (rhapsodyd mcp will fall back to config server.port)")
                    }
                }
                // Serve until the shutdown signal fires, then drain in-flight requests (graceful).
                let mut serve_ctx = shutdown.wait();
                server_task = Some(tokio::spawn(async move {
                    let drain = async move { serve_ctx.cancelled().await };
                    if let Err(e) = server.serve_with_shutdown(drain).await {
                        tracing::error!(err = %e, "observability server error");
                    }
                }));
            }
            Err(e) => {
                let _ = writeln!(stderr.make_writer(), "symphony: observability server: {e}");
                return 1;
            }
        }
    }

    // --- startup banner (purely additive) ---
    let color = banner_color_enabled(is_terminal, flags.no_color, |k| {
        std::env::var(k).unwrap_or_default()
    });
    if let Some(mut data) =
        resolve_banner_data(&flags.path, &dashboard_url, &flags.db, flags.no_store)
    {
        data.assignee = best_effort_assignee_label(&flags.path).await;
        let _ = crate::banner::render(&mut stderr.make_writer(), &data, color);
    }

    // --- prune scheduler (history + stale worktrees; daily, plus one startup cycle) ---
    let prune_ctx = shutdown.wait();
    let prune_handle = handle.clone();
    let prune_task = tokio::spawn(async move {
        let sf = {
            let h = prune_handle.clone();
            move || h.store()
        };
        let rf = {
            let h = prune_handle.clone();
            move || h.current_retention_days()
        };
        let rl = {
            let h = prune_handle.clone();
            move || h.retention_loaded()
        };
        let pw = {
            let h = prune_handle.clone();
            move |days: i64| {
                let h = h.clone();
                async move { h.prune_stale_workspaces(days).await }
            }
        };
        crate::prune::run_prune_schedule(prune_ctx, sf, rf, pw, rl).await;
    });

    // --- Rhapsody Teams triage (STUDIO-644, slice T3b; design record
    // ~/.rhapsody/docs/STUDIO-572-rhapsody-teams.md, §0.11.2) ---
    //
    // The one model turn the Teams design accepts runs HERE — an off-loop background task beside the
    // prune scheduler, on its own cadence, never on the control task. §0.11.2 moved it here after the
    // adversarial review found a model call inside `dispatch_issue` to be the STUDIO-551 head-of-line
    // class: up to `manager.timeout_ms` of stall per unrouted pick, per tick, with no breaker.
    //
    // `spawn_triage` is the whole gate: `manager.mode: labels`, `mode: off`, an empty roster or Teams
    // disabled spawn NOTHING, so those configurations have no task that could have a behaviour delta.
    // `install_probe` additionally holds it back in the hermetic daemon tests, which must never shell
    // out to a real `claude` — the same reason it gates the BO-59 credential probe above.
    // One room handle, shared by both off-loop Teams tasks (triage's decisions, STUDIO-650; the
    // quorum's fan-out post, STUDIO-659). `LocalRoom` is a path plus a lock, so cloning the `Arc`
    // is what keeps the room's single-writer discipline across the two tasks.
    let teams_room = resolve_room_dir(resolved.as_ref(), &flags.db, flags.no_store)
        .map(|dir| Arc::new(rhapsody_config::room::LocalRoom::new(dir)));
    let triage_room = teams_room.clone();
    let quorum_room = teams_room;
    let triage_task = if let Some(seam) = triage_seam {
        let triage_ctx = shutdown.wait();
        let triage_handle = handle.clone();
        let (command, billing_guard, tracker_api_key) = triage_agent_env(resolved.as_ref());
        let deps = rhapsody_orchestrator::TriageDeps {
            teams: Arc::new(teams_cfg.clone()),
            // Read lazily each cycle, exactly as the prune scheduler reads its store handle: the
            // handle is built before the first reload, so the trackers arrive later.
            //
            // These are the PER-PROJECT trackers, not the account-level one (STUDIO-671). The
            // account-level client is bound to the top-level `tracker.project_slug`, which the
            // `projects:` config form leaves empty by design, so triaging through it filtered
            // `project.slugId == ""` — a query Linear answers with zero rows and no error. The task
            // ran every cycle and found nothing to do, silently, while the selection gate held the
            // very tickets it was meant to release.
            //
            // The state sets ride along from the SAME reload (STUDIO-672): triage considers
            // exactly the tickets the selection gate would hold, which means filtering by the
            // gate's own dispatchable-state predicate rather than by labels alone. Without them
            // triage assigned identities to In-Review tickets — parked design records and work
            // already under human review — that the gate could never have held.
            target: move || {
                let (trackers, states) = triage_handle.reads_triage_target()?;
                Some(rhapsody_orchestrator::TriageTarget { trackers, states })
            },
            arbiter: Arc::new(rhapsody_orchestrator::ClaudeTriageArbiter),
            agent_command: command,
            billing_guard,
            tracker_api_key,
            interval: rhapsody_orchestrator::TRIAGE_INTERVAL,
            max_backoff_ms: rhapsody_orchestrator::MAX_TRIAGE_BACKOFF_MS,
            // The other end of the seam built above `o.control()`.
            handle: seam,
            // The room triage posts its decisions to (STUDIO-650, T5). Resolved here rather than
            // taken from `o` because the orchestrator has already moved into the control task by
            // this point; both resolve the same directory through `resolve_room_dir`.
            room: triage_room.map(|r| r as Arc<dyn rhapsody_config::room::RoomLog>),
            // The evidence the one-time review-label reconcile judges by (STUDIO-672): the
            // `teams.route` events row every routed dispatch already writes.
            //
            // Armed ONLY for the durable on-disk store. A `Noop` answers every read `Ok(empty)` and
            // never `Err` — so a store that failed to open would tell this sweep that nobody has
            // ever worn any identity, and it would strip every roster label in the workspace,
            // including the earned ones. Teams needs an on-disk runtime home to exist at all, so a
            // non-durable store here means the open FAILED, and a failed open must disable the
            // sweep rather than answer its question with silence.
            history: durable_store.then(|| {
                Arc::new(rhapsody_orchestrator::StoreIdentityHistory::new(
                    handle.store(),
                    // `resolve()` defaults this to 30; the fallback is unreachable here because a
                    // config that failed to load leaves `resolve_teams_path` `None` and Teams off.
                    resolved
                        .as_ref()
                        .and_then(|c| c.storage.retention_days)
                        .unwrap_or(30),
                )) as Arc<dyn rhapsody_orchestrator::IdentityHistory>
            }),
            // Where a completed sweep is recorded, so "one-time" outlives this process.
            reconcile_marker: resolve_teams_reconcile_path(
                resolved.as_ref(),
                &flags.db,
                flags.no_store,
            ),
        };
        Some(tokio::spawn(async move {
            rhapsody_orchestrator::run_triage_schedule(triage_ctx, deps).await;
        }))
    } else {
        None
    };

    // The quorum's own off-loop task, spawned beside triage and for the same reason: every tracker
    // write it performs (two issue creates and a label) must happen off the control task, and a
    // Linear that never answers must park this task and nothing else. It holds no `Orchestrator`,
    // takes no lock the control task takes, and is cancelled by the same lifetime signal.
    let quorum_task = quorum_rx.map(|rx| {
        let quorum_ctx = shutdown.wait();
        let quorum_handle = handle.clone();
        let deps = rhapsody_orchestrator::QuorumDeps {
            // Cloned rather than moved: the prefetch task below is the third reader of the same
            // boot-loaded config (Teams config is not hot-reloaded in this slice).
            teams: Arc::new(teams_cfg.clone()),
            // Read lazily per request, exactly as triage reads its tracker per cycle: the handle is
            // built before the first reload, so the tracker arrives later.
            target: move || {
                quorum_handle
                    .reads_tracker()
                    .map(|tracker| rhapsody_orchestrator::QuorumTarget { tracker })
            },
            // The same room triage posts to, resolved the same way and for the same reason.
            room: quorum_room.map(|r| r as Arc<dyn rhapsody_config::room::RoomLog>),
            max_backoff_ms: rhapsody_orchestrator::MAX_QUORUM_BACKOFF_MS,
        };
        tokio::spawn(async move {
            rhapsody_orchestrator::run_quorum_task(quorum_ctx, deps, rx).await;
        })
    });

    // --- Rhapsody Teams memory prefetch (STUDIO-660, slice T8; design record §5, §5.4) ---
    //
    // The third off-loop Teams task, beside triage and the quorum, and spawned for exactly the same
    // reason: `memory.backend: hindsight` puts the bank on the far side of a tailnet, and
    // `dispatch_issue` runs inline on the single control task. So the recall happens HERE, ahead of
    // time, and dispatch does nothing but a non-blocking read of what this task already fetched.
    //
    // `install_teams_memory` returning `Some` IS the gate: any other backend, an empty or malformed
    // endpoint, Teams off, or an empty roster all yield `None`, and then no task exists to have a
    // behaviour delta.
    let prefetch_task = teams_prefetch.map(|w| {
        let prefetch_ctx = shutdown.wait();
        let prefetch_handle = handle.clone();
        let deps = rhapsody_orchestrator::PrefetchDeps {
            teams: Arc::new(teams_cfg),
            // Read lazily each cycle, exactly as triage reads its tracker: the handle is built
            // before the first reload, so the tracker arrives later.
            target: move || {
                prefetch_handle
                    .reads_tracker()
                    .map(|tracker| rhapsody_orchestrator::PrefetchTarget { tracker })
            },
            backend: w.backend,
            cache: w.cache,
            interval: rhapsody_orchestrator::PREFETCH_INTERVAL,
            max_backoff_ms: rhapsody_orchestrator::MAX_PREFETCH_BACKOFF_MS,
            now: Box::new(chrono::Utc::now),
        };
        tokio::spawn(async move {
            rhapsody_orchestrator::run_prefetch_schedule(prefetch_ctx, deps).await;
        })
    });

    // --- run the control loop until ctx is cancelled ---
    let run_err = o.run(ctx.clone()).await;

    // The control loop has returned (ctx cancel OR a fatal reload error) — now stop the server + prune
    // regardless of why (Go's `pruneCancel` + `defer srv.Shutdown`).
    shutdown.cancel();
    // Stop + join the prune task BEFORE writing to stderr so its logging cannot race run's output.
    let _ = prune_task.await;
    // The triage task is cancelled by the same signal, and checks it between model turns as well as
    // between cycles. The wait is still BOUNDED: a turn already in flight can take up to
    // `manager.timeout_ms`, and a shutdown must never be held open by one — `kill_on_drop` reaps the
    // child when the runtime tears the task down.
    if let Some(t) = triage_task {
        let _ = tokio::time::timeout(SHUTDOWN_DRAIN, t).await;
    }
    // The quorum task is cancelled by the same signal and checks it on both sides of its receive,
    // so the wait is bounded by whatever tracker write is already in flight.
    if let Some(t) = quorum_task {
        let _ = tokio::time::timeout(SHUTDOWN_DRAIN, t).await;
    }
    // The prefetch task is cancelled by the same signal and checks it on both sides of its sleep as
    // well as between candidates, so the wait is bounded by one remote recall — itself capped by
    // `rhapsody_config::hindsight::REQUEST_TIMEOUT`.
    if let Some(t) = prefetch_task {
        let _ = tokio::time::timeout(SHUTDOWN_DRAIN, t).await;
    }
    // Drain the observability server (bounded, mirroring Go's 5s Shutdown ctx).
    if let Some(t) = server_task {
        let _ = tokio::time::timeout(SHUTDOWN_DRAIN, t).await;
    }
    // Remove the runtime port file we published (only ours; runtimeport guards the PID).
    if runtime_port_published {
        let _ = runtimeport::remove();
    }
    // Flush + stop telemetry exporters (bounded internally so an unreachable collector can't stall).
    tel.shutdown();

    match run_err {
        Ok(()) => 0,
        Err(e) => {
            let _ = writeln!(stderr.make_writer(), "symphony: {e}");
            1
        }
    }
}

/// The parsed daemon flags + positional workflow path. Mirrors the `flag.FlagSet` in Go `run`.
struct Flags {
    /// `--port`: observability HTTP server port (`-1`: use `server.port`; `0`: ephemeral).
    port: i64,
    /// `--db`: history store path override (`""` uses `storage.path`).
    db: String,
    /// `--no-store`: force the Noop store.
    no_store: bool,
    /// `--no-color`: disable the banner's ANSI color.
    no_color: bool,
    /// The positional WORKFLOW.md path (default `WORKFLOW.md`).
    path: PathBuf,
}

/// Parses the daemon flags, mirroring Go's `flag.FlagSet` (`--port` / `--db` take a value, either
/// `--flag value` or `--flag=value`; `--no-store` / `--no-color` are booleans; both single- and
/// double-dash forms are accepted). Flag parsing stops at the first non-flag arg (Go's `flag.Parse`
/// semantics), which becomes the workflow path. An unknown flag or a bad numeric value is an error
/// (Go returns exit 2). Mirrors the flag section of Go `run`.
fn parse_flags(args: &[String]) -> Result<Flags, String> {
    let mut f = Flags {
        port: -1,
        db: String::new(),
        no_store: false,
        no_color: false,
        path: PathBuf::from("WORKFLOW.md"),
    };
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        // `--` ends flag parsing; the next arg (if any) is the workflow path.
        if a == "--" {
            if let Some(p) = args.get(i + 1).filter(|p| !p.is_empty()) {
                f.path = PathBuf::from(p);
            }
            break;
        }
        // A non-flag (or bare "-") is the first positional → the workflow path; stop parsing flags.
        let Some(flag) = a.strip_prefix("--").or_else(|| a.strip_prefix('-')) else {
            if !a.is_empty() {
                f.path = PathBuf::from(a);
            }
            break;
        };
        if flag.is_empty() {
            // A bare "-" is a positional, not a flag.
            f.path = PathBuf::from(a);
            break;
        }
        let (name, inline) = match flag.split_once('=') {
            Some((n, v)) => (n, Some(v.to_string())),
            None => (flag, None),
        };
        match name {
            "no-store" => f.no_store = parse_bool_flag(inline, "no-store")?,
            "no-color" => f.no_color = parse_bool_flag(inline, "no-color")?,
            "port" => {
                let v = take_value(inline, args, &mut i, "port")?;
                f.port = v
                    .parse()
                    .map_err(|_| format!("invalid value {v:?} for flag -port"))?;
            }
            "db" => f.db = take_value(inline, args, &mut i, "db")?,
            other => return Err(format!("flag provided but not defined: -{other}")),
        }
        i += 1;
    }
    Ok(f)
}

/// Parses a boolean flag's value, mirroring Go's `strconv.ParseBool` (which `flag` uses for bool
/// flags): a bare `--flag` (no inline value) is `true`; an inline `=1/t/T/TRUE/True/true` is `true`
/// and `=0/f/F/FALSE/False/false` is `false`; anything else is an error (Go exits 2). This is why a
/// bare `--no-store` differs from `--no-store=0`: the latter means "store ON", not off — the naive
/// `!= "false"` reading silently inverted it.
fn parse_bool_flag(inline: Option<String>, name: &str) -> Result<bool, String> {
    match inline {
        None => Ok(true),
        Some(v) => match v.as_str() {
            "1" | "t" | "T" | "TRUE" | "True" | "true" => Ok(true),
            "0" | "f" | "F" | "FALSE" | "False" | "false" => Ok(false),
            _ => Err(format!("invalid boolean value {v:?} for flag -{name}")),
        },
    }
}

/// Resolves a value-flag's argument: the inline `=value`, or the next arg. Advances `i` past a
/// consumed separate value. Mirrors `flag`'s "needs an argument" error.
fn take_value(
    inline: Option<String>,
    args: &[String],
    i: &mut usize,
    name: &str,
) -> Result<String, String> {
    match inline {
        Some(v) => Ok(v),
        None => {
            *i += 1;
            args.get(*i)
                .cloned()
                .ok_or_else(|| format!("flag needs an argument: -{name}"))
        }
    }
}

/// Resolves telemetry config for the boot, best-effort: starts from the synthetic default
/// (`http` protocol, `symphony` service — so `OTEL_*` env applies even on a config error) and, when
/// the workflow loads + decodes, resolves from its `otel:` block. Mirrors Go `run`'s telemetry-config
/// resolution (lines 88-95).
fn resolve_boot_otel(path: &Path) -> telemetry::Config {
    let base = Otel {
        protocol: "http".to_string(),
        service_name: "symphony".to_string(),
        ..Default::default()
    };
    let getenv = |k: &str| std::env::var(k).unwrap_or_default();
    if let Ok(def) = workflow::load(path)
        && let Ok(cfg) = decode(&def)
    {
        return resolve_otel_config(&cfg.otel, getenv);
    }
    resolve_otel_config(&base, getenv)
}

/// Loads + decodes + resolves the workflow config for the store-open path (`None` on any failure, so
/// the store falls back to Noop and Run's reload reports the error).
/// Reports what an operator needs to know about the roster's profiles at boot, in ONE warning line
/// per category (STUDIO-642; design record `~/.rhapsody/docs/STUDIO-572-rhapsody-teams.md`, §4):
///
///   * a roster entry naming a profile that does not resolve — the "broken agent discovered at
///     dispatch time" §4 exists to prevent, reported here per T1's disable-loudly semantics;
///   * a PINNED profile whose built-in has moved on. §4 is explicit that drift is **reported, never
///     merged**: nothing here rewrites a user's file, and nothing silently upgrades a pin.
///
/// Read-only and never seeding: the profiles directory sits beside `teams.yaml`, and an absent one
/// simply means every profile resolves to its built-in.
fn report_profile_issues(teams: Option<&rhapsody_config::teams::Teams>, teams_path: &Path) {
    let Some(teams) = teams.filter(|t| !t.roster.is_empty()) else {
        return;
    };
    let Some(dir) = teams_path
        .parent()
        .map(|d| d.join("teams").join("profiles"))
    else {
        return;
    };
    let (mut broken, mut drifted) = (Vec::new(), Vec::new());
    for issue in rhapsody_config::profiles::check_roster(teams, &dir) {
        match issue {
            i @ rhapsody_config::profiles::RosterIssue::Unresolvable { .. } => {
                broken.push(i.to_string())
            }
            i @ rhapsody_config::profiles::RosterIssue::Drift { .. } => drifted.push(i.to_string()),
        }
    }
    if !broken.is_empty() {
        tracing::warn!(
            profiles = %broken.join("; "),
            dir = %dir.display(),
            "teams roster names profiles that do not resolve; those identities have no prompt"
        );
    }
    if !drifted.is_empty() {
        tracing::warn!(
            profiles = %drifted.join("; "),
            "teams profiles are pinned behind their built-in; run `rhapsodyd teams show <name>` to see the resolved prompt (reported, never merged)"
        );
    }
}

/// Whether this boot spawns the Teams triage task (STUDIO-644) — the composition-root gate, named
/// so it is testable at exactly the predicate `run` calls.
///
/// Since STUDIO-669 it decides one more thing: this predicate also gates the
/// [`TriageHandle`](rhapsody_orchestrator::TriageHandle) installed on the orchestrator, so a boot
/// that spawns no triage task also has a selection gate that holds nothing. The two cannot drift
/// apart, because they are the same `if`.
///
/// Two conditions, and both are "spawn NOTHING", not "spawn something inert":
///
/// * [`triage_enabled`](rhapsody_orchestrator::triage_enabled) — the design's own gate: Teams
///   enabled, a `manager.mode` that routes, and a non-empty roster. `mode: labels` now qualifies
///   (STUDIO-669, §A.3.3: the task's deterministic assignment needs no model), while `mode: off`
///   and Teams-off still have zero behaviour delta by construction — there is no task to have one,
///   and nothing is ever held waiting for one.
/// * `install_probe` — false in the hermetic daemon tests, which must never shell out to a real
///   `claude`. The BO-59 credential probe is held back by the same flag for the same reason, and
///   holding the handle back with it is what keeps those tests' dispatch byte-identical.
fn spawn_triage(install_probe: bool, teams: &rhapsody_config::teams::Teams) -> bool {
    install_probe && rhapsody_orchestrator::triage_enabled(teams)
}

/// Whether the Rhapsody Teams review-quorum task should exist at all (STUDIO-659, T7; design
/// record §0.12): Teams enabled, `quorum.enabled` true, and a roster to draw reviewers from.
///
/// **A roster of ONE still spawns the task, deliberately.** The obvious optimisation — require two
/// teammates, since one cannot review its own work — would silently delete a behaviour §0.12 asks
/// for by name: "zero ⇒ skip with a **loud room post**, never an error". That post is how an
/// operator who enabled the quorum on a one-person team finds out nothing will ever be reviewed,
/// and it lives in the task, so gating the task on `roster.len() > 1` would make it unreachable.
/// An EMPTY roster is excluded (as it is for [`spawn_triage`]) because no run can wear an identity
/// there, so no handoff could ever build a request to post about.
///
/// Unlike [`spawn_triage`] this is NOT gated on `install_probe`: the quorum shells out to nothing,
/// so a hermetic daemon test can carry the task safely. It is gated on the config alone, which is
/// what makes "`quorum.enabled: false` spawns no task" a property rather than a promise.
fn spawn_quorum(teams: &rhapsody_config::teams::Teams) -> bool {
    teams.enabled && teams.quorum.enabled && !teams.roster.is_empty()
}

/// The claude command, effective billing guard and tracker credential the Teams triage turn runs
/// under (STUDIO-644). Read from the boot-resolved config, alongside `teams.yaml` itself: this
/// slice does not hot-reload Teams config, so its model-turn inputs are boot-scoped too. A daemon
/// with no readable workflow falls back to the same defaults the runner would apply — an empty
/// command means the runner's own `claude` default, and an absent `billing_guard` is on.
fn triage_agent_env(cfg: Option<&Config>) -> (String, bool, String) {
    match cfg {
        Some(c) => (
            c.claude.command.clone(),
            rhapsody_agent::claude::billing_guard_enabled(c.claude.billing_guard),
            c.tracker.api_key.clone(),
        ),
        None => (
            String::new(),
            rhapsody_agent::claude::billing_guard_enabled(None),
            String::new(),
        ),
    }
}

/// §3.5's named startup warning: `enabled: true` with `manager.mode: off` and NO
/// `default_identity` is "behaviour identical to `enabled: false`" — nothing routes and nothing is
/// prepended. That is a real combination to reach by half-editing a file, and the design says it is
/// "worth a startup warning rather than a silent no-op" (STUDIO-643; design record
/// `~/.rhapsody/docs/STUDIO-572-rhapsody-teams.md`, §3.5).
///
/// `mode: off` WITH a `default_identity` is the opposite — single-identity Teams, "probably the right
/// first thing to try" — so it is deliberately not warned about.
/// Builds and installs the Teams memory handles (STUDIO-645, T4). A no-op when Teams is off — no
/// backend is constructed, no path is resolved, nothing is created (§2.4 row 8: "there is no code
/// path").
/// What the off-loop memory prefetch task needs, built by [`install_teams_memory`] and spawned by
/// [`run`] beside triage and the quorum (STUDIO-660, T8).
///
/// It exists so the ONE remote client the daemon builds is shared by both consumers — the off-loop
/// `/api/v1/teams/*` handlers (through `TeamsMemory`) and the prefetch task — rather than each
/// dialling its own connection pool at the same bank.
struct PrefetchWiring {
    backend: Arc<dyn rhapsody_config::memory::MemoryBackend>,
    cache: Arc<rhapsody_orchestrator::PrefetchCache>,
}

fn install_teams_memory(
    o: &mut rhapsody_orchestrator::Orchestrator,
    teams_cfg: &rhapsody_config::teams::Teams,
    resolved: Option<&rhapsody_config::Config>,
    flags: &Flags,
) -> Option<PrefetchWiring> {
    use rhapsody_config::hindsight::HindsightBackend;
    use rhapsody_config::memory::{LocalBank, MemoryBackend, NoneBackend};
    use rhapsody_config::teams::MemoryBackend as BackendKind;

    if !teams_cfg.enabled {
        return None;
    }
    // `backend: hindsight` — the shared cloud bank (§5.4). Built here and handed to BOTH the
    // off-loop `TeamsMemory` surface and the prefetch task; `teams_bank` is deliberately left
    // `None` below, so nothing on the control task can reach it.
    //
    // An empty endpoint is the one configuration that still runs memoryless, because there is
    // nothing to dial. Anything else that fails to build (a malformed URL) does the same, loudly:
    // a broken memory endpoint must not be a startup failure.
    let remote = match teams_cfg.memory.backend {
        BackendKind::Hindsight if teams_cfg.memory.endpoint.trim().is_empty() => {
            tracing::warn!(
                "teams memory: backend is `hindsight` but `memory.endpoint` is empty, so there is \
                 no bank to dial; running with no memory. Set `memory.endpoint` to the service \
                 base URL (and `memory.api_key`, which the deployed service requires), or \
                 `memory.backend: local` for on-disk banks."
            );
            None
        }
        BackendKind::Hindsight => {
            match HindsightBackend::new(
                &teams_cfg.memory.endpoint,
                teams_cfg.memory.bank_prefix.clone(),
                &teams_cfg.memory.api_key,
            ) {
                Ok(h) => Some(Arc::new(
                    h.with_bank_overrides(
                        teams_cfg
                            .roster
                            .iter()
                            .map(|i| (i.name.clone(), i.bank.clone())),
                    ),
                )),
                Err(e) => {
                    tracing::warn!(
                        err = %e,
                        "teams memory: backend `hindsight` could not be built from \
                         `memory.endpoint`; running with no memory"
                    );
                    None
                }
            }
        }
        BackendKind::Local | BackendKind::None => None,
    };
    let banks = resolve_banks_dir(resolved, &teams_cfg.memory.path, &flags.db, flags.no_store);
    let bank = match (teams_cfg.memory.backend, banks) {
        (BackendKind::Local, Some(dir)) => Some(Arc::new(
            LocalBank::new(dir, teams_cfg.memory.bank_prefix.clone()).with_bank_overrides(
                teams_cfg
                    .roster
                    .iter()
                    .map(|i| (i.name.clone(), i.bank.clone())),
            ),
        )),
        (BackendKind::Local, None) => {
            tracing::warn!(
                "teams memory: backend is `local` but there is no on-disk runtime home to anchor \
                 banks to (storage off / in-memory); memory is disabled for this daemon"
            );
            None
        }
        // `hindsight` never takes a LOCAL bank, whether or not the remote one was built: its
        // records live in the cloud. `resolve_banks_dir` is still consulted below for the room's
        // cursors, which are the room's state rather than memory's.
        (BackendKind::Hindsight, _) => None,
        (BackendKind::None, _) => None,
    };
    // The dispatch path only ever sees a LOCAL bank; `none` and `hindsight` leave it `None`, and
    // the turn-1 prompt is then byte-identical to T3a's. For `hindsight` the facts arrive instead
    // through `teams_prefetch`, filled off-loop and read non-blockingly (STUDIO-660).
    o.teams_bank = bank.as_ref().map(Arc::clone);
    let backend: Arc<dyn MemoryBackend> = match (&bank, &remote) {
        (Some(b), _) => Arc::clone(b) as Arc<dyn MemoryBackend>,
        (None, Some(h)) => Arc::clone(h) as Arc<dyn MemoryBackend>,
        (None, None) => Arc::new(NoneBackend),
    };
    // The prefetch cache, installed on the orchestrator ONLY when there is a remote bank to fill it
    // and a roster to route to — the same gate the task's spawn uses, so the field and the task can
    // never disagree about whether prefetching is on.
    let wiring = remote
        .filter(|_| rhapsody_orchestrator::prefetch_enabled(teams_cfg))
        .map(|h| {
            let cache = Arc::new(rhapsody_orchestrator::PrefetchCache::new());
            o.teams_prefetch = Some(Arc::clone(&cache));
            PrefetchWiring {
                backend: h as Arc<dyn MemoryBackend>,
                cache,
            }
        });
    // The room (STUDIO-650, T5). Independent of `memory.backend`: a roster running with
    // `backend: none` still has a room, because the room is the team's shared log rather than any
    // one identity's memory. Only the absence of an on-disk runtime home turns it off.
    let room = resolve_room_dir(resolved, &flags.db, flags.no_store)
        .map(|dir| Arc::new(rhapsody_config::room::LocalRoom::new(dir)));
    // A backstop rather than a live branch: this function is only reached from inside
    // `resolve_teams_path(..).is_some()`, which already required the same runtime home, so today
    // `room` is always `Some` here. It stays because the two resolutions are independent and a
    // future call site need not carry that guarantee.
    if room.is_none() {
        tracing::warn!(
            "teams room: there is no on-disk runtime home to anchor ~/.rhapsody/teams/room/ to \
             (storage off / in-memory); the room is disabled for this daemon"
        );
    }
    // Cursors live in the identity's own state (§0.11.4), resolved by the SAME rule the banks use
    // so a teammate's watermark lands beside its records — including the roster's `bank:`
    // overrides. Resolved even when `memory.backend` is not `local`, because a cursor is the room's
    // state, not memory's.
    //
    // That includes `hindsight` (STUDIO-660): a remote memory bank does NOT move the room or its
    // cursors to the cloud, and it does not make the local banks directory dead. `resolve_banks_dir`
    // still anchors cursors under ~/.rhapsody/teams/banks/ regardless of backend, and any local bank
    // contents a `local` deployment left behind stay exactly where they are — migrating them is its
    // own (unwritten) ticket, never a side effect of flipping one config line.
    let cursors = resolve_banks_dir(resolved, &teams_cfg.memory.path, &flags.db, flags.no_store)
        .map(|dir| {
            Arc::new(
                rhapsody_config::room::Cursors::new(dir, teams_cfg.memory.bank_prefix.clone())
                    .with_bank_overrides(
                        teams_cfg
                            .roster
                            .iter()
                            .map(|i| (i.name.clone(), i.bank.clone())),
                    ),
            )
        });
    // Both handles or neither: a room with no cursor home would re-read the same bounded window on
    // every run forever, which reads to an operator as the room being broken rather than as the
    // watermark being unwritable.
    if let (Some(room), Some(cursors)) = (room.as_ref(), cursors.as_ref()) {
        o.teams_room = Some(Arc::clone(room));
        o.teams_cursors = Some(Arc::clone(cursors));
    }
    let mut mem =
        rhapsody_orchestrator::teamsmemory::TeamsMemory::new(Arc::new(teams_cfg.clone()), backend);
    if let Some(room) = room {
        mem = mem.with_room(room as Arc<dyn rhapsody_config::room::RoomLog>);
    }
    o.teams_memory = Some(Arc::new(mem));
    wiring
}

fn report_inert_manager(teams: Option<&rhapsody_config::teams::Teams>) {
    let Some(teams) = teams else { return };
    if teams.enabled
        && teams.manager.mode == rhapsody_config::teams::ManagerMode::Off
        && teams.manager.default_identity.is_empty()
    {
        tracing::warn!(
            "teams is enabled but manager.mode is `off` with no manager.default_identity: nothing \
             will route and no teammate section will be prepended, which is exactly the behaviour \
             of `enabled: false`. Set manager.default_identity to run every ticket as one teammate."
        );
    }
}

fn load_resolved(path: &Path) -> Option<Config> {
    let def = workflow::load(path).ok()?;
    let cfg = decode(&def).ok()?;
    resolve(cfg, &workflow_dir(path)).ok()
}

/// Resolves the daemon's process-log dir for the boot (TRA-267), best-effort: the resolved
/// `logging.dir` when the workflow loads + decodes + resolves, else the resolved default
/// `~/.rhapsody/logs` — obtained by resolving a blank config, so it reuses the exact tilde-expand /
/// absolutize / default logic in `rhapsody_config::resolve` (mirrors how `resolve_boot_otel` falls
/// back to its synthetic base). Passed to `telemetry::init` as the rolling-file log target.
fn resolve_boot_logdir(path: &Path) -> PathBuf {
    let dir = load_resolved(path)
        .or_else(|| {
            // Fallback: resolve a defaulted-but-unresolved config (empty WORKFLOW.md front matter run
            // through `decode`) so the default inherits the exact `logging.dir` default +
            // normalization from `rhapsody_config`, mirroring `resolve_boot_otel`'s synthetic base.
            let blank = decode(&workflow::Definition {
                config: workflow::YamlMap::new(),
                prompt_template: String::new(),
            })
            .ok()?;
            resolve(blank, &workflow_dir(path)).ok()
        })
        .map(|cfg| cfg.logging.dir)
        .unwrap_or_default();
    PathBuf::from(dir)
}

/// Resolves the Linear key owner (the user whose assigned issues Rhapsody processes) for the startup
/// banner. Best-effort: any load / decode / network failure returns `""` (the banner then shows a
/// generic key-owner fallback) and never blocks startup, since the candidate filter binds to the key
/// owner at poll time regardless. Bounded to 5s. Mirrors Go `bestEffortAssigneeLabel`.
async fn best_effort_assignee_label(path: &Path) -> String {
    let Ok(def) = workflow::load(path) else {
        return String::new();
    };
    let Ok(cfg) = decode(&def) else {
        return String::new();
    };
    let tr = tracker::new(tracker::Spec {
        kind: cfg.tracker.kind,
        endpoint: cfg.tracker.endpoint,
        api_key: cfg.tracker.api_key,
        project_slug: cfg.tracker.project_slug,
        source: cfg.tracker.source,
        active_states: Vec::new(),
        review_states: Vec::new(),
        summon_token: String::new(),
        milestone: String::new(),
        claim_mode: String::new(),
    });
    match tokio::time::timeout(Duration::from_secs(5), tr.resolve_viewer()).await {
        Ok(Ok(v)) => {
            tracing::info!(assignee = %v.display_name, email = %v.email, id = %v.id, "linear: candidate filter bound to key owner");
            assignee_label(&v)
        }
        _ => {
            tracing::warn!(
                "could not resolve Linear key owner for banner (candidate filter still binds to the key owner at poll time)"
            );
            String::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{SharedBuf, TempDir};
    use rhapsody_orchestrator::CancelSignal;

    // A minimal valid workflow, HERMETIC: the tracker points at a dead loopback address (fetches fail
    // fast, non-fatal), and workspace.root + logging.dir + storage stay inside the temp dir so a test
    // NEVER writes into the real `~/.rhapsody` (which on the self-hosted CI runner may hold a live
    // daemon's DB / runtime.json). `storage_block` + `extra_block` are top-level YAML blocks. Literal
    // api_key (not `$VAR`) avoids a process-global env mutation racing parallel tests — see bootcfg.
    fn write_wf(dir: &TempDir, storage_block: &str, extra_block: &str) -> PathBuf {
        let ws = dir.child("ws");
        let logs = dir.child("logs");
        let body = format!(
            "---\ntracker:\n  kind: linear\n  endpoint: http://127.0.0.1:9\n  api_key: tok\n  project_slug: proj\npolling:\n  interval_ms: 50\nagent:\n  backend: claude\nworkspace:\n  root: {ws}\nlogging:\n  dir: {logs}\n{storage_block}{extra_block}---\nWork the issue.\n",
            ws = ws.display(),
            logs = logs.display(),
        );
        let p = dir.child("WORKFLOW.md");
        std::fs::write(&p, body).expect("write WORKFLOW.md");
        p
    }

    const OFF_STORAGE: &str = "storage:\n  path: \"off\"\n";

    // TRA-267: `resolve_boot_logdir` returns the configured `logging.dir` when the workflow resolves,
    // and falls back to the resolved `~/.rhapsody/logs` default when the config path is bad/missing.
    // Hermetic without an `unsafe { set_var }`: the expected default is derived from the process's own
    // `$HOME` (the workspace's established env-test idiom — see the config crate's resolve tests).
    #[test]
    fn resolve_boot_logdir_configured_and_default() {
        // Configured: a valid workflow's resolved `logging.dir` (the temp `logs` dir) is returned.
        let dir = TempDir::new();
        let wf = write_wf(&dir, "", "");
        assert_eq!(resolve_boot_logdir(&wf), dir.child("logs"));

        // Bad/missing config path: falls back to the resolved `~/.rhapsody/logs` default.
        let home = std::env::var("HOME").expect("HOME set in test env");
        let bad = dir.child("does-not-exist").join("WORKFLOW.md");
        assert_eq!(
            resolve_boot_logdir(&bad),
            PathBuf::from(format!("{home}/.rhapsody/logs")),
        );
    }

    /// Runs the daemon until a short deadline, then cancels ctx and awaits a clean exit, returning the
    /// code (mirrors Go's `context.WithTimeout` daemon tests). `buf` captures stderr.
    async fn run_briefly(args: &[&str], buf: &SharedBuf) -> i32 {
        let signal = CancelSignal::new();
        let ctx = signal.wait();
        let argv: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let buf = buf.clone();
        let handle = tokio::spawn(async move { run(ctx, &argv, buf, false, false).await });
        tokio::time::sleep(Duration::from_millis(250)).await;
        signal.cancel();
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("daemon did not exit within 5s of cancel")
            .expect("run task join")
    }

    /// Runs the daemon to completion with a never-cancelled ctx (for the fast-exit error paths).
    async fn run_now(args: &[&str], buf: &SharedBuf) -> i32 {
        let signal = CancelSignal::new();
        let argv: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        run(signal.wait(), &argv, buf.clone(), false, false).await
    }

    // Mirrors Go `TestRunStartsDaemonAndStopsCleanly` (storage forced off to stay hermetic — the
    // clean start/stop behavior is store-independent).
    #[tokio::test(flavor = "multi_thread")]
    async fn run_starts_daemon_and_stops_cleanly() {
        let dir = TempDir::new();
        let wf = write_wf(&dir, OFF_STORAGE, "");
        let buf = SharedBuf::new();
        let code = run_briefly(&[&wf.to_string_lossy()], &buf).await;
        assert_eq!(
            code,
            0,
            "daemon should exit 0 on cancel; stderr={}",
            buf.contents()
        );
    }

    // Mirrors Go `TestRunMissingFileExitsNonZero`.
    #[tokio::test(flavor = "multi_thread")]
    async fn run_missing_file_exits_nonzero() {
        let dir = TempDir::new();
        let absent = dir.child("absent.md");
        let buf = SharedBuf::new();
        let code = run_now(&[&absent.to_string_lossy()], &buf).await;
        assert_ne!(code, 0, "missing workflow file must exit non-zero");
        assert!(
            !buf.contents().is_empty(),
            "expected an operator-visible error on stderr"
        );
    }

    // Mirrors Go `TestRunValidationFailureExitsNonZero`: missing api_key → startup validation fails.
    // storage.path is forced off (hermetic): the store is opened before the loop's validation, so a
    // default path would create a real ~/.rhapsody DB before the daemon rejects the config.
    #[tokio::test(flavor = "multi_thread")]
    async fn run_validation_failure_exits_nonzero() {
        let dir = TempDir::new();
        let ws = dir.child("ws");
        let logs = dir.child("logs");
        let wf = dir.child("WORKFLOW.md");
        std::fs::write(
            &wf,
            format!(
                "---\ntracker:\n  kind: linear\n  api_key: \"\"\n  project_slug: proj\nworkspace:\n  root: {}\nlogging:\n  dir: {}\nstorage:\n  path: \"off\"\n---\nbody\n",
                ws.display(),
                logs.display(),
            ),
        )
        .expect("write");
        let buf = SharedBuf::new();
        assert_ne!(
            run_now(&[&wf.to_string_lossy()], &buf).await,
            0,
            "validation failure must exit non-zero"
        );
    }

    // Mirrors Go `TestRunStartsWithOtelEnabled`: otel.enabled + a dead endpoint → still starts + stops.
    #[tokio::test(flavor = "multi_thread")]
    async fn run_starts_with_otel_enabled() {
        let dir = TempDir::new();
        let wf = write_wf(
            &dir,
            OFF_STORAGE,
            "otel:\n  enabled: true\n  endpoint: 127.0.0.1:4317\n  protocol: grpc\n",
        );
        let buf = SharedBuf::new();
        let code = run_briefly(&[&wf.to_string_lossy()], &buf).await;
        assert_eq!(
            code,
            0,
            "daemon with otel enabled should exit 0; stderr={}",
            buf.contents()
        );
    }

    // Mirrors Go `TestRunRendersBannerToStderr`: the banner (tagline + projects table) is emitted to
    // stderr at startup with no ANSI (a non-TTY capture buffer).
    #[tokio::test(flavor = "multi_thread")]
    async fn run_renders_banner_to_stderr() {
        let dir = TempDir::new();
        let wf = write_wf(&dir, OFF_STORAGE, "");
        let buf = SharedBuf::new();
        let code = run_briefly(&[&wf.to_string_lossy()], &buf).await;
        assert_eq!(code, 0, "daemon should exit 0; stderr={}", buf.contents());
        let s = buf.contents();
        assert!(
            s.contains("coding-agent orchestrator"),
            "expected banner tagline on stderr\n{s}"
        );
        assert!(
            s.contains("PROJECTS") && s.contains("proj"),
            "expected projects table on stderr\n{s}"
        );
        assert!(
            !s.contains('\u{1b}'),
            "banner to a non-TTY buffer must have no ANSI escapes\n{s:?}"
        );
    }

    // Mirrors Go `TestRunDispatchesToMCP`: `mcp <bad path>` dispatches to runMCP (the `symphony mcp:`
    // marker + non-zero exit), NOT the daemon path.
    #[tokio::test(flavor = "multi_thread")]
    async fn run_dispatches_to_mcp() {
        let buf = SharedBuf::new();
        let code = run_now(&["mcp", "/no/such/WORKFLOW.md"], &buf).await;
        assert_ne!(code, 0, "expected non-zero exit for a bad workflow path");
        assert!(
            buf.contents().contains("symphony mcp:"),
            "stderr = {:?}, want the runMCP dispatch marker",
            buf.contents()
        );
    }

    // Mirrors Go `TestRunNoStoreCreatesNoDBFile`: --no-store creates no on-disk DB.
    #[tokio::test(flavor = "multi_thread")]
    async fn run_no_store_creates_no_db_file() {
        let dir = TempDir::new();
        let wf = write_wf(&dir, "", ""); // default storage; --no-store forces Noop before any open
        let buf = SharedBuf::new();
        assert_eq!(
            run_briefly(&["--no-store", &wf.to_string_lossy()], &buf).await,
            0
        );
        assert!(
            !dir.child("ws").join("symphony.db").exists(),
            "--no-store must not create a symphony.db file"
        );
    }

    // Mirrors Go `TestRunDBOffCreatesNoDBFile`: storage.path: off creates no on-disk DB.
    #[tokio::test(flavor = "multi_thread")]
    async fn run_db_off_creates_no_db_file() {
        let dir = TempDir::new();
        let wf = write_wf(&dir, OFF_STORAGE, "");
        let buf = SharedBuf::new();
        assert_eq!(run_briefly(&[&wf.to_string_lossy()], &buf).await, 0);
        assert!(
            !dir.child("ws").join("symphony.db").exists(),
            "storage.path: off must not create a symphony.db file"
        );
    }

    // STUDIO-639 (Teams T1), design §2.1: a `teams.yaml` that is NOT there stays not there. This is
    // the deliberate divergence from the `capabilities.yaml` precedent — `load_or_seed` writes its
    // file on first read, and a disabled feature must not. The test boots the real daemon against an
    // on-disk store so BOTH sidecar paths resolve into the same temp dir, then asserts the asymmetry
    // directly: capabilities.yaml appears (proving the path resolution really reached this dir and the
    // test is not vacuous), teams.yaml does not.
    #[tokio::test(flavor = "multi_thread")]
    async fn run_seeds_capabilities_but_never_seeds_teams_yaml() {
        let dir = TempDir::new();
        let wf = write_wf(&dir, "", "");
        let db = dir.child("rhapsody.db");
        let buf = SharedBuf::new();
        assert_eq!(
            run_briefly(
                &["--db", &db.to_string_lossy(), &wf.to_string_lossy()],
                &buf
            )
            .await,
            0,
            "daemon should exit 0 on cancel; stderr={}",
            buf.contents()
        );
        assert!(
            dir.child("capabilities.yaml").exists(),
            "the capabilities registry IS seeded beside the store — if this fails the sidecar path \
             never resolved here and the teams.yaml assertion below proves nothing"
        );
        assert!(
            !dir.child("teams.yaml").exists(),
            "teams.yaml must NEVER be seeded: an absent file is Teams' off state and shipped state"
        );
        // STUDIO-645 (T4): and no bank directory either. The banks dir appears on the first
        // `retain` and at no other time, so a Teams-off daemon leaves the same filesystem behind it
        // found — the same claim the teams.yaml assertion above makes, for memory.
        assert!(
            !std::path::Path::new(&dir.child("teams")).exists(),
            "a Teams-off boot must create no teams/ directory (banks or profiles)"
        );
    }

    // STUDIO-660 (Teams T8): a `memory.backend: hindsight` teams.yaml boots cleanly and dials
    // NOTHING while it does. The endpoint here is a closed loopback port, so any request at all
    // during boot would be a connection refused the run would have to survive — and the prefetch
    // task deliberately waits out its first interval before a cycle, so there is none to survive.
    #[tokio::test(flavor = "multi_thread")]
    async fn run_starts_cleanly_with_a_hindsight_teams_yaml_without_dialling() {
        let dir = TempDir::new();
        let wf = write_wf(&dir, "", "");
        let db = dir.child("rhapsody.db");
        std::fs::write(
            dir.child("teams.yaml"),
            "enabled: true\nmemory:\n  backend: hindsight\n  endpoint: http://127.0.0.1:9/mcp/\n               api_key: $RHAPSODY_NO_SUCH_VAR\nroster:\n  - name: alice\n    labels: [rust]\n",
        )
        .expect("write teams.yaml");
        let buf = SharedBuf::new();
        assert_eq!(
            run_briefly(
                &["--db", &db.to_string_lossy(), &wf.to_string_lossy()],
                &buf
            )
            .await,
            0,
            "daemon should exit 0 on cancel; stderr={}",
            buf.contents()
        );
        // And no local bank was created on the way: `hindsight` records live in the cloud, and the
        // banks directory still appears only on a `local` retain.
        assert!(
            !std::path::Path::new(&dir.child("teams"))
                .join("banks")
                .exists(),
            "a hindsight boot must create no local banks directory"
        );
    }

    // The other half of the endpoint contract: `backend: hindsight` with NO endpoint keeps the
    // pre-T8 warn-and-memoryless behaviour, and the warning says which key is missing — the whole
    // point of updating that message, since the old one blamed an unimplemented slice.
    //
    // A SCOPED subscriber (`with_default`), not the daemon's stderr: `run` installs the process's
    // one global subscriber, so a log-content assertion made through `run_briefly` would depend on
    // which test in the binary booted first. This calls the composition-root function directly.
    #[test]
    fn a_hindsight_backend_with_no_endpoint_warns_and_runs_memoryless() {
        use rhapsody_config::teams::{Identity, MemoryBackend as BackendKind, Teams};
        use std::sync::{Arc as StdArc, Mutex};
        use tracing_subscriber::layer::SubscriberExt;

        #[derive(Clone, Default)]
        struct Recorder(StdArc<Mutex<Vec<String>>>);
        impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Recorder {
            fn on_event(
                &self,
                event: &tracing::Event<'_>,
                _ctx: tracing_subscriber::layer::Context<'_, S>,
            ) {
                struct V<'a>(&'a mut Vec<String>);
                impl tracing::field::Visit for V<'_> {
                    fn record_debug(
                        &mut self,
                        field: &tracing::field::Field,
                        value: &dyn std::fmt::Debug,
                    ) {
                        self.0.push(format!("{}={value:?}", field.name()));
                    }
                }
                if let Ok(mut got) = self.0.lock() {
                    event.record(&mut V(&mut got));
                }
            }
        }

        let dir = TempDir::new();
        let flags = Flags {
            port: -1,
            db: dir.child("rhapsody.db").to_string_lossy().into_owned(),
            no_store: false,
            no_color: false,
            path: PathBuf::from("WORKFLOW.md"),
        };
        let cfg = load_resolved(std::path::Path::new(&write_wf(&dir, "", "")));
        let mut teams = Teams {
            enabled: true,
            roster: vec![Identity {
                name: "alice".to_string(),
                ..Identity::default()
            }],
            ..Teams::disabled()
        };
        teams.memory.backend = BackendKind::Hindsight;

        let rec = Recorder::default();
        let subscriber = tracing_subscriber::registry().with(rec.clone());
        let mut o = rhapsody_orchestrator::Orchestrator::new(String::new());
        let wiring = tracing::subscriber::with_default(subscriber, || {
            install_teams_memory(&mut o, &teams, cfg.as_ref(), &flags)
        });

        assert!(
            wiring.is_none(),
            "there is no bank to dial, so no task to spawn"
        );
        assert!(o.teams_prefetch.is_none());
        assert!(o.teams_bank.is_none());
        assert!(
            o.teams_memory.is_some(),
            "the off-loop tools still answer — as no-ops"
        );
        let logged = rec.0.lock().expect("logged").join(" | ");
        assert!(
            logged.contains("memory.endpoint") && logged.contains("no memory"),
            "the warning must name the missing key and say what it cost: {logged}"
        );
    }

    // STUDIO-645 (Teams T4), §5.4 + §2.4 row 8: which memory handles a boot installs is decided by
    // the toggle and `memory.backend` alone. This is the exact function `run` calls, so "off costs
    // nothing" is checked at the composition root rather than inferred from it.
    //
    // `teams_bank` is the CONCRETE local bank the DISPATCH path recalls from; it must be `Some` for
    // `local` and `None` for everything else, because that is what makes "no network on the dispatch
    // path" true by construction rather than by care.
    #[test]
    fn teams_memory_handles_are_installed_only_for_an_enabled_local_backend() {
        use rhapsody_config::teams::{MemoryBackend as BackendKind, Teams};

        let with = |enabled: bool, backend: BackendKind| {
            let mut t = Teams {
                enabled,
                ..Teams::disabled()
            };
            t.memory.backend = backend;
            t
        };
        let dir = TempDir::new();
        let flags = Flags {
            port: -1,
            db: dir.child("rhapsody.db").to_string_lossy().into_owned(),
            no_store: false,
            no_color: false,
            path: PathBuf::from("WORKFLOW.md"),
        };
        let cfg = load_resolved(std::path::Path::new(&write_wf(&dir, "", "")));

        // (dispatch-path local bank, off-loop runtime, dispatch-path prefetch cache).
        let install = |teams: &Teams| {
            let mut o = rhapsody_orchestrator::Orchestrator::new(String::new());
            let wiring = install_teams_memory(&mut o, teams, cfg.as_ref(), &flags);
            assert_eq!(
                wiring.is_some(),
                o.teams_prefetch.is_some(),
                "the returned wiring and the installed cache must agree: one gates the task's \
                 spawn and the other gates the dispatch-path read, and a disagreement would mean \
                 a cache nobody fills or a task nobody reads"
            );
            (
                o.teams_bank.is_some(),
                o.teams_memory.is_some(),
                o.teams_prefetch.is_some(),
            )
        };

        assert_eq!(
            install(&with(false, BackendKind::Local)),
            (false, false, false),
            "Teams OFF installs nothing at all — not even the off-loop runtime"
        );
        assert_eq!(
            install(&with(true, BackendKind::Local)),
            (true, true, false),
            "`local` installs both the dispatch-path bank and the off-loop runtime, and no cache"
        );
        assert_eq!(
            install(&with(true, BackendKind::None)),
            (false, true, false),
            "`none` still serves the off-loop tools (as no-ops) but puts NO bank on the dispatch path"
        );

        // STUDIO-660 (T8). `hindsight` NEVER takes the dispatch-path bank slot, whatever else it
        // does — that is what makes "no network on the dispatch path" true by construction. What it
        // installs instead depends on whether there is a bank to dial at all.
        assert_eq!(
            install(&with(true, BackendKind::Hindsight)),
            (false, true, false),
            "`hindsight` with no endpoint runs memoryless, exactly as it did before T8"
        );
        let mut reachable = with(true, BackendKind::Hindsight);
        reachable.memory.endpoint = "https://hindsight.example.ts.net".to_string();
        reachable.roster = vec![rhapsody_config::teams::Identity {
            name: "alice".to_string(),
            ..rhapsody_config::teams::Identity::default()
        }];
        assert_eq!(
            install(&reachable),
            (false, true, true),
            "`hindsight` with an endpoint drives the off-loop tools remotely and feeds dispatch \
             through the prefetch cache — still with NO bank on the dispatch path"
        );
        let mut no_roster = reachable.clone();
        no_roster.roster.clear();
        assert_eq!(
            install(&no_roster),
            (false, true, false),
            "an empty roster routes nothing, so there is nothing to prefetch for"
        );
        let mut malformed = reachable.clone();
        malformed.memory.endpoint = "not a url".to_string();
        assert_eq!(
            install(&malformed),
            (false, true, false),
            "a malformed endpoint degrades to memoryless; it is never a startup failure"
        );

        // Installing handles creates nothing: the banks dir waits for the first retain.
        assert!(
            !std::path::Path::new(&dir.child("teams")).exists(),
            "installing the memory handles must not create the banks directory"
        );
    }

    // STUDIO-659 (Teams T7), design §0.12 and the slice's first acceptance criterion: the review
    // quorum is opt-in per installation, so the fan-out task exists ONLY for an enabled Teams with
    // `quorum.enabled` and a roster of more than one. Every other configuration — which includes
    // every installation shipping today — has no task at all rather than a task that returns early,
    // which is what makes "zero behaviour change" a property rather than a promise.
    #[test]
    fn spawn_quorum_only_for_an_enabled_quorum_with_somebody_to_ask() {
        use rhapsody_config::teams::{Identity, Quorum, Teams};

        let team = |enabled: bool, quorum: bool, names: &[&str]| Teams {
            enabled,
            quorum: Quorum {
                enabled: quorum,
                reviewers: 2,
            },
            roster: names
                .iter()
                .map(|n| Identity {
                    name: (*n).to_string(),
                    ..Default::default()
                })
                .collect(),
            ..Teams::disabled()
        };

        assert!(spawn_quorum(&team(true, true, &["alice", "bob"])));
        assert!(
            !spawn_quorum(&team(true, false, &["alice", "bob"])),
            "the quorum defaults off and must spawn nothing until it is asked for"
        );
        assert!(
            !spawn_quorum(&team(false, true, &["alice", "bob"])),
            "Teams off must spawn no quorum task"
        );
        assert!(
            spawn_quorum(&team(true, true, &["alice"])),
            "a roster of ONE must still spawn the task: §0.12's loud \"nobody to ask\" room post \
             lives there, and it is how an operator learns nothing will ever be reviewed"
        );
        assert!(
            !spawn_quorum(&team(true, true, &[])),
            "an EMPTY roster spawns nothing: no run can wear an identity, so no handoff could \
             build a request to post about"
        );
        assert!(
            !spawn_quorum(&Teams::disabled()),
            "the shipped state must spawn no quorum task"
        );
    }

    // STUDIO-644 (Teams T3b), design §0.11.2 and the slice's first acceptance criterion: the triage
    // task — the ONLY thing in the daemon that can call a model outside a dispatched run — spawns
    // for `labels+model` and for nothing else. This is the exact predicate `run` gates the spawn on,
    // so `mode: off` / Teams-off provably have no task at all, rather than a task that returns
    // early — and, since STUDIO-669, provably no selection gate either.
    #[test]
    fn spawn_triage_for_every_routing_mode_in_production() {
        use rhapsody_config::teams::{Identity, ManagerMode, Teams};

        let with_mode = |mode: ManagerMode, enabled: bool| Teams {
            enabled,
            manager: rhapsody_config::teams::Manager {
                mode,
                ..Default::default()
            },
            roster: vec![Identity {
                name: "alice".to_string(),
                ..Default::default()
            }],
            ..Teams::disabled()
        };

        assert!(spawn_triage(
            true,
            &with_mode(ManagerMode::LabelsModel, true)
        ));
        // STUDIO-669, §A.3.3: `mode: labels` now spawns the task. It runs no model turn there, but
        // it does the deterministic assignment that lets the selection gate hold work safely — and
        // because this same predicate gates the `TriageHandle`, a mode that spawned nothing would
        // be a mode whose gate held nothing either, leaving `labels` with the identity-less
        // dispatch §A exists to end.
        assert!(
            spawn_triage(true, &with_mode(ManagerMode::Labels, true)),
            "`mode: labels` assigns deterministically, so it spawns the task"
        );
        assert!(
            !spawn_triage(true, &with_mode(ManagerMode::Off, true)),
            "`mode: off` must spawn no triage task"
        );
        assert!(
            !spawn_triage(true, &with_mode(ManagerMode::LabelsModel, false)),
            "Teams off must spawn no triage task"
        );
        assert!(
            !spawn_triage(true, &Teams::disabled()),
            "the shipped state must spawn no triage task"
        );
        assert!(
            !spawn_triage(false, &with_mode(ManagerMode::LabelsModel, true)),
            "the hermetic daemon tests must never spawn a task that shells out to claude"
        );
    }

    // STUDIO-644: a `labels+model` teams.yaml boots and stops cleanly. The triage wiring sits beside
    // the prune scheduler in the boot path, so a mistake there would show up as a hang or a non-zero
    // exit rather than as a failing unit test.
    #[tokio::test(flavor = "multi_thread")]
    async fn run_starts_cleanly_with_a_labels_plus_model_teams_yaml() {
        let dir = TempDir::new();
        let wf = write_wf(&dir, "", "");
        let db = dir.child("rhapsody.db");
        std::fs::write(
            dir.child("teams.yaml"),
            "enabled: true\nmanager:\n  mode: labels+model\nroster:\n  - name: alice\n    labels: [rust]\n",
        )
        .expect("write teams.yaml");
        let buf = SharedBuf::new();
        assert_eq!(
            run_briefly(
                &["--db", &db.to_string_lossy(), &wf.to_string_lossy()],
                &buf
            )
            .await,
            0,
            "daemon should exit 0 on cancel; stderr={}",
            buf.contents()
        );
    }

    // STUDIO-642 (Teams T2), design §4: `rhapsodyd teams …` is dispatched at the very top of `run`,
    // beside `mcp`, so it never reaches flag parsing or the run-lock. An unknown verb is the cheapest
    // proof the branch was taken: it exits non-zero with the `symphony teams:` marker rather than
    // treating "teams" as a workflow path.
    #[tokio::test(flavor = "multi_thread")]
    async fn run_dispatches_to_teams() {
        let buf = SharedBuf::new();
        let code = run_now(&["teams", "no-such-verb"], &buf).await;
        assert_ne!(code, 0, "expected non-zero exit for an unknown teams verb");
        assert!(
            buf.contents().contains("symphony teams:"),
            "stderr = {:?}, want the teams dispatch marker",
            buf.contents()
        );
    }

    // STUDIO-642, design §4's never-create-on-read rule: booting the daemon with a roster that names
    // profiles resolves them (which is what produces the drift / unknown-profile warnings) and must
    // still leave `teams/profiles/` absent. Only `rhapsodyd teams fork` ever creates it.
    #[tokio::test(flavor = "multi_thread")]
    async fn run_never_creates_the_profiles_dir() {
        let dir = TempDir::new();
        let wf = write_wf(&dir, "", "");
        let db = dir.child("rhapsody.db");
        std::fs::write(
            dir.child("teams.yaml"),
            "enabled: true\nroster:\n  - name: alice\n    profile: swe\n  - name: bob\n    profile: nosuch\n",
        )
        .expect("write teams.yaml");
        let buf = SharedBuf::new();
        assert_eq!(
            run_briefly(
                &["--db", &db.to_string_lossy(), &wf.to_string_lossy()],
                &buf
            )
            .await,
            0,
            "a roster naming an unknown profile must not fail the boot; stderr={}",
            buf.contents()
        );
        assert!(
            !dir.child("teams").exists(),
            "resolving the roster's profiles must never create {}",
            dir.child("teams").display()
        );
    }

    // STUDIO-639 (Teams T1), design §2.1: a malformed teams.yaml disables Teams LOUDLY and the daemon
    // still starts and stops cleanly — a broken optional config file is never a startup failure.
    #[tokio::test(flavor = "multi_thread")]
    async fn run_starts_cleanly_with_a_malformed_teams_yaml() {
        let dir = TempDir::new();
        let wf = write_wf(&dir, "", "");
        let db = dir.child("rhapsody.db");
        // `roster` as a scalar where a sequence belongs: YAML that cannot become a `Teams`.
        std::fs::write(dir.child("teams.yaml"), "enabled: true\nroster: \"nope\"\n")
            .expect("write teams.yaml");
        let buf = SharedBuf::new();
        assert_eq!(
            run_briefly(
                &["--db", &db.to_string_lossy(), &wf.to_string_lossy()],
                &buf
            )
            .await,
            0,
            "a malformed teams.yaml must not fail the boot; stderr={}",
            buf.contents()
        );
        // And the daemon did not "repair" the file by overwriting it — it is left exactly as written,
        // for the operator to fix.
        assert_eq!(
            std::fs::read_to_string(dir.child("teams.yaml")).expect("read back"),
            "enabled: true\nroster: \"nope\"\n"
        );
    }

    // Mirrors Go `TestRunDBMemoryCreatesNoFile`: --db :memory: creates no on-disk file.
    #[tokio::test(flavor = "multi_thread")]
    async fn run_db_memory_creates_no_file() {
        let dir = TempDir::new();
        let wf = write_wf(&dir, "", "");
        let buf = SharedBuf::new();
        assert_eq!(
            run_briefly(&["--db", ":memory:", &wf.to_string_lossy()], &buf).await,
            0
        );
        assert!(
            !dir.child("ws").join("symphony.db").exists(),
            "--db :memory: must not create an on-disk file"
        );
    }

    // The flag parser: defaults, value flags (separate + inline), booleans, the positional path, and
    // the error paths (unknown flag, missing value). Covers Go's `flag.FlagSet` semantics + the
    // `TestRunDefaultsToCwdWorkflow` default-path behavior (the full cwd integration is the boot e2e).
    #[test]
    fn parse_flags_semantics() {
        // Default: no args → WORKFLOW.md, port -1 (disabled), store on.
        let f = parse_flags(&[]).expect("defaults");
        assert_eq!(f.path, PathBuf::from("WORKFLOW.md"));
        assert_eq!(f.port, -1);
        assert!(!f.no_store && !f.no_color && f.db.is_empty());

        // Positional workflow path.
        let f = parse_flags(&["a.md".to_string()]).expect("positional");
        assert_eq!(f.path, PathBuf::from("a.md"));

        // --port separate + inline; --db; --no-store; then the positional.
        let f = parse_flags(&["--port".into(), "0".into(), "wf".into()]).expect("port sep");
        assert_eq!((f.port, f.path.clone()), (0, PathBuf::from("wf")));
        let f = parse_flags(&["--port=8123".into(), "wf".into()]).expect("port inline");
        assert_eq!(f.port, 8123);
        let f = parse_flags(&["--db".into(), ":memory:".into(), "wf".into()]).expect("db");
        assert_eq!(f.db, ":memory:");
        let f = parse_flags(&["--no-store".into(), "wf".into()]).expect("no-store");
        assert!(f.no_store);
        let f = parse_flags(&["-no-color".into(), "wf".into()]).expect("single-dash bool");
        assert!(f.no_color);

        // Go `strconv.ParseBool` for bool flags: `=0`/`=false`/`=f`/`=FALSE` mean OFF (do NOT disable
        // the store — the naive `!= "false"` reading silently inverted this); `=1`/`=true` mean ON;
        // anything else errors (Go exits 2).
        for off in [
            "--no-store=0",
            "--no-store=false",
            "--no-store=f",
            "--no-store=FALSE",
        ] {
            assert!(
                !parse_flags(&[off.to_string()])
                    .unwrap_or_else(|e| panic!("{off}: {e}"))
                    .no_store,
                "{off} must NOT disable the store"
            );
        }
        for on in ["--no-store=1", "--no-store=true", "--no-store=T"] {
            assert!(
                parse_flags(&[on.to_string()]).expect(on).no_store,
                "{on} must disable the store"
            );
        }
        assert!(
            parse_flags(&["--no-store=garbage".into()]).is_err(),
            "a non-boolean --no-store value must error (Go exits 2)"
        );

        // Error paths: unknown flag, and a value flag with no argument.
        assert!(
            parse_flags(&["--nope".into()]).is_err(),
            "unknown flag must error"
        );
        assert!(
            parse_flags(&["--port".into()]).is_err(),
            "port needs an argument"
        );
        assert!(
            parse_flags(&["--port".into(), "abc".into()]).is_err(),
            "non-numeric port must error"
        );
    }
}
