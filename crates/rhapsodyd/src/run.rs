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
use rhapsody_orchestrator::{CancelSignal, CancelWait, Orchestrator};
use rhapsody_telemetry as telemetry;
use rhapsody_tracker as tracker;

use crate::bootcfg::{
    assignee_label, banner_color_enabled, open_store, resolve_banner_data, resolve_server_port,
    workflow_dir,
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
/// TTY (for the banner's ANSI-color decision). Mirrors Go `run`.
pub async fn run<W>(ctx: CancelWait, args: &[String], stderr: W, is_terminal: bool) -> i32
where
    W: for<'a> MakeWriter<'a> + Clone + Send + Sync + 'static,
{
    // `rhapsodyd mcp [WORKFLOW.md]` runs the local MCP facade over stdio instead of the daemon
    // (INF-473). Dispatched at the very top so the daemon's run-lock / flag parsing is untouched and
    // `rhapsodyd <workflow>` behaves identically. Mirrors Go `run`'s `args[0] == "mcp"` branch.
    if args.first().map(String::as_str) == Some("mcp") {
        return crate::mcp::run_mcp(ctx, &args[1..], stderr).await;
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
    let store = open_store(
        load_resolved(&flags.path).as_ref(),
        &flags.db,
        flags.no_store,
    );
    o.set_store(store);
    // Install the lifetime ctx BEFORE snapshotting the off-loop handle, so the handle's stop/resume/
    // message reply-waits are bounded by the real ctx (not the never-cancelling default). `Run`
    // re-sets the same ctx below.
    o.set_ctx(ctx.clone());

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
        let provider = Arc::new(DaemonState::new(handle.clone()));
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

    // --- run the control loop until ctx is cancelled ---
    let run_err = o.run(ctx.clone()).await;

    // The control loop has returned (ctx cancel OR a fatal reload error) — now stop the server + prune
    // regardless of why (Go's `pruneCancel` + `defer srv.Shutdown`).
    shutdown.cancel();
    // Stop + join the prune task BEFORE writing to stderr so its logging cannot race run's output.
    let _ = prune_task.await;
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
        let handle = tokio::spawn(async move { run(ctx, &argv, buf, false).await });
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
        run(signal.wait(), &argv, buf.clone(), false).await
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
