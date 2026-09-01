//! bootcfg — the daemon's boot-time config resolution helpers (parity port of the config-derived
//! helpers in `$REF/cmd/symphony/run.go`): the observability-server port, the durable store open
//! (the disk store-open the Rust orchestrator defers to the daemon), and the startup banner's
//! presentation data (server port + storage + otel + resolved projects). Kept as pure functions over
//! the workflow path / resolved config so the boot and the tests drive them the same way.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Duration;
use rhapsody_config::memory::DEFAULT_BANKS_SUBDIR;
use rhapsody_config::room::DEFAULT_ROOM_SUBDIR;
use rhapsody_config::{Config, decode, go_duration_string, resolve, resolve_projects, workflow};
use rhapsody_core::Viewer;
use rhapsody_store::{Noop, Sqlite, Store, StorePath, parse_store_path};

use crate::banner;
use crate::otel::resolve_otel_config;

/// `filepath.Dir` for the workflow path: the parent directory, or `"."` for a bare filename. Mirrors
/// Go `dirOf`.
pub(crate) fn workflow_dir(path: &Path) -> String {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| ".".to_string())
}

/// Reads the real process environment (Go's `os.Getenv`): the production `getenv` the boot passes to
/// [`resolve_otel_config`]. Tests inject their own closure instead.
fn os_getenv(k: &str) -> String {
    std::env::var(k).unwrap_or_default()
}

/// Decides the observability HTTP server port: the `--port` flag (`>= 0`) wins; otherwise
/// `server.port` from the workflow enables it; otherwise disabled. Load/decode errors here are
/// non-fatal (the daemon's own load reports them) and simply leave the server disabled. Mirrors Go
/// `resolveServerPort`.
pub fn resolve_server_port(flag_port: i64, workflow_path: &Path) -> (i64, bool) {
    if flag_port >= 0 {
        return (flag_port, true);
    }
    let Ok(def) = workflow::load(workflow_path) else {
        return (0, false);
    };
    let Ok(cfg) = decode(&def) else {
        return (0, false);
    };
    match cfg.server.port {
        Some(p) => (p, true),
        None => (0, false),
    }
}

/// Builds the durable store for a loaded config, honoring the daemon's `--db` / `--no-store`
/// overrides. NEVER fails: on disabled (`off` / `--no-store`) or a failed open it falls back to
/// [`Noop`] so the orchestrator stays guard-free. The Rust orchestrator's `Run` defers this disk
/// store-open to the daemon (P6), which injects the result via `set_store` before `Run`. `cfg` is
/// `None` when the daemon's own best-effort load failed (the orchestrator's `Run` then reports the
/// config error and exits). Mirrors Go `orchestrator.openStore`.
///
/// The second return value is **durability**: `true` only when the handle really is the on-disk
/// SQLite store. Every fallback above answers `false`, including the one an operator never asked
/// for — a `Sqlite::open` that FAILED still yields a working [`Noop`] handle, and a [`Noop`] answers
/// every read with an empty result rather than an error (STUDIO-672). A caller that would act on an
/// absence must be able to tell "this store has nothing to say" from "nothing happened", and from
/// the handle alone it cannot.
pub fn open_store(
    cfg: Option<&Config>,
    db_override: &str,
    disabled: bool,
) -> (Arc<dyn Store + Send + Sync>, bool) {
    if disabled {
        tracing::info!("storage disabled (--no-store); history + recovery off");
        return (Arc::new(Noop), false);
    }
    // The flag override wins over config when non-empty.
    let mut path = db_override.to_string();
    if path.is_empty()
        && let Some(c) = cfg
    {
        path = c.storage.path.clone();
    }
    let sp = parse_store_path(&path);
    if path.is_empty() || sp == StorePath::Off {
        tracing::info!("storage disabled (storage.path: off); history + recovery off");
        return (Arc::new(Noop), false);
    }
    let on_disk = matches!(sp, StorePath::Disk(_));
    match Sqlite::open(sp) {
        Ok(st) => {
            tracing::info!(path = %path, "durable history store open");
            (Arc::new(st), on_disk)
        }
        Err(e) => {
            tracing::error!(path = %path, err = %e, "open store failed; continuing with persistence disabled");
            (Arc::new(Noop), false)
        }
    }
}

/// Resolves the path of the agent-capabilities registry file (`capabilities.yaml`, BO-12), colocated
/// with the durable store in Rhapsody's runtime home (`~/.rhapsody/capabilities.yaml` for the default
/// on-disk store). Reuses the SAME resolved `storage.path` the store-open path uses for `rhapsody.db`
/// (`--db` override wins, via [`parse_store_path`]), so a custom store location keeps the registry
/// alongside it and tests stay hermetic. Returns `None` when there is no on-disk store directory to
/// anchor the file to — a disabled (`--no-store` / `off`) or in-memory (`:memory:`) store, or a failed
/// config load — in which case the daemon runs without a registry and capability rendering is a no-op
/// rather than seeding a file with no natural home. Mirrors [`open_store`]'s path decision.
pub fn resolve_capabilities_path(
    cfg: Option<&Config>,
    db_override: &str,
    no_store: bool,
) -> Option<PathBuf> {
    resolve_runtime_home_file(cfg, db_override, no_store, "capabilities.yaml")
}

/// Resolves the path of the Rhapsody Teams config file (`teams.yaml`, STUDIO-639), colocated with the
/// durable store by exactly the same rule as [`resolve_capabilities_path`] — Teams is user-editable,
/// non-parity data in its own file, following the BO-11 `capabilities.yaml` precedent (design record
/// `~/.rhapsody/docs/STUDIO-572-rhapsody-teams.md`, §2.1). `None` (a disabled / in-memory store, or a
/// failed config load) means the daemon runs with no Teams file to anchor, i.e. Teams stays off.
///
/// Unlike the capabilities registry, an absent `teams.yaml` is NEVER created: absence is the off
/// state and the shipped state (§2.1), so this only ever names a path that may or may not exist.
pub fn resolve_teams_path(
    cfg: Option<&Config>,
    db_override: &str,
    no_store: bool,
) -> Option<PathBuf> {
    resolve_runtime_home_file(cfg, db_override, no_store, "teams.yaml")
}

/// Resolves the Rhapsody Teams PROFILES directory (`teams/profiles/`, STUDIO-642), colocated with
/// the durable store beside `teams.yaml` (design record
/// `~/.rhapsody/docs/STUDIO-572-rhapsody-teams.md`, §1's storage table). `None` (a disabled /
/// in-memory store, or a failed config load) means there is nothing to anchor to.
///
/// Naming the directory does not create it: loading, resolving and `teams show` are strictly
/// read-only, and a profiles directory that does not exist simply means every profile resolves to
/// its built-in (§4). Only `rhapsodyd teams fork` ever creates it.
pub fn resolve_profiles_dir(
    cfg: Option<&Config>,
    db_override: &str,
    no_store: bool,
) -> Option<PathBuf> {
    resolve_runtime_home_file(cfg, db_override, no_store, "teams").map(|d| d.join("profiles"))
}

/// Resolves the marker file the one-time Teams identity-label reconcile records its retirement in
/// (`teams/reconcile.json`, STUDIO-672), colocated with `teams.yaml` and `teams/profiles/` by the
/// same rule every other Teams sidecar follows.
///
/// The sweep removes labels, so "have I already done this?" must survive a restart — a daemon
/// redeploys far more often than an operator relabels a review ticket, and a process-local flag
/// would re-strip a deliberate human label at every boot. `None` (a disabled / in-memory store, or a
/// failed config load) means there is nowhere to record the retirement, which disables the sweep
/// rather than making it a per-boot duty.
///
/// Naming the file does not create it; the triage task writes it once, after a completed sweep.
pub fn resolve_teams_reconcile_path(
    cfg: Option<&Config>,
    db_override: &str,
    no_store: bool,
) -> Option<PathBuf> {
    resolve_runtime_home_file(cfg, db_override, no_store, "teams").map(|d| d.join("reconcile.json"))
}

/// Resolves the Rhapsody Teams memory BANKS directory (STUDIO-645, T4; design record §5.4).
///
/// `memory.path` wins when set — an operator who points memory somewhere else means it. Otherwise
/// the banks sit beside `teams.yaml` and `teams/profiles/` in the runtime home
/// (`~/.rhapsody/teams/banks/`), by the same rule every other sidecar follows. `None` (a disabled /
/// in-memory store, or a failed config load) means there is no durable home to anchor banks to, so
/// memory stays off rather than writing somewhere arbitrary.
///
/// Naming the directory does not create it: the banks directory appears on the first `retain` and at
/// no other time (§2.1's rule, carried into memory).
pub fn resolve_banks_dir(
    cfg: Option<&Config>,
    memory_path: &str,
    db_override: &str,
    no_store: bool,
) -> Option<PathBuf> {
    if !memory_path.is_empty() {
        return Some(PathBuf::from(memory_path));
    }
    resolve_runtime_home_file(cfg, db_override, no_store, DEFAULT_BANKS_SUBDIR)
}

/// Resolves the Rhapsody Teams ROOM directory (STUDIO-650, T5; design record §0.11.4:
/// `~/.rhapsody/teams/room/`).
///
/// Deliberately **not** parameterised on `memory.path`, unlike [`resolve_banks_dir`]: banks are
/// per-identity memory and an operator may legitimately point them at another disk, but the room is
/// one shared log for the whole roster and `teams.yaml` has no key for it. It therefore always sits
/// in the runtime home beside `teams.yaml`, `teams/profiles/` and `teams/banks/`. `None` (a
/// disabled / in-memory store, or a failed config load) means there is no durable home to anchor a
/// room to, so the room stays off rather than writing somewhere arbitrary.
///
/// Naming the directory does not create it: the room appears on the first append and at no other
/// time (§2.1's rule, carried into the room).
pub fn resolve_room_dir(
    cfg: Option<&Config>,
    db_override: &str,
    no_store: bool,
) -> Option<PathBuf> {
    resolve_runtime_home_file(cfg, db_override, no_store, DEFAULT_ROOM_SUBDIR)
}

/// The shared path decision behind [`resolve_capabilities_path`] and [`resolve_teams_path`]: `file`
/// sits in the directory of the resolved on-disk store (`--db` override wins over `storage.path`),
/// so a custom store location keeps its sidecar files alongside it and tests stay hermetic. `None`
/// when there is no on-disk store directory to anchor to. Mirrors [`open_store`]'s path decision.
fn resolve_runtime_home_file(
    cfg: Option<&Config>,
    db_override: &str,
    no_store: bool,
    file: &str,
) -> Option<PathBuf> {
    if no_store {
        return None;
    }
    let mut path = db_override.to_string();
    if path.is_empty() {
        path = cfg?.storage.path.clone();
    }
    match parse_store_path(&path) {
        StorePath::Disk(p) => p.parent().map(|d| d.join(file)),
        StorePath::Off | StorePath::InMemory => None,
    }
}

/// Formats a resolved viewer for the banner: `"Display Name <email>"`, falling back to the display
/// name, then the email, then the id. Mirrors Go `assigneeLabel`.
pub fn assignee_label(v: &Viewer) -> String {
    match (v.display_name.is_empty(), v.email.is_empty()) {
        (false, false) => format!("{} <{}>", v.display_name, v.email),
        (false, true) => v.display_name.clone(),
        (true, false) => v.email.clone(),
        (true, true) => v.id.clone(),
    }
}

/// Decides whether the startup banner uses ANSI color: the output stream must be a terminal,
/// `--no-color` must be unset, and the `NO_COLOR` env convention must not be set. Go type-asserts the
/// writer to `*os.File` + checks `os.ModeCharDevice`; the Rust boot passes
/// `std::io::stderr().is_terminal()` (a non-terminal writer, e.g. a test buffer, is never a TTY).
/// `getenv` is injected for testing. Mirrors Go `bannerColorEnabled`.
pub fn banner_color_enabled(
    is_terminal: bool,
    no_color_flag: bool,
    getenv: impl Fn(&str) -> String,
) -> bool {
    if no_color_flag {
        return false;
    }
    if !getenv("NO_COLOR").is_empty() {
        return false;
    }
    is_terminal
}

/// The banner's Storage row (path, retention): `--no-store` / `off` → `"disabled"`; `:memory:` →
/// `"in-memory"`; otherwise the resolved on-disk path (`--db` override wins) + retention_days. The
/// on-disk path is BEST-EFFORT (the daemon's openStore may fall back to Noop if the path is
/// unwritable, so the banner can optimistically show a path the daemon could not open). Mirrors Go
/// `resolveBannerStorage`.
pub fn resolve_banner_storage(cfg: &Config, db_override: &str, no_store: bool) -> (String, i32) {
    if no_store {
        return ("disabled".to_string(), 0);
    }
    let mut p = db_override.to_string();
    if p.is_empty() {
        p = cfg.storage.path.clone();
    }
    let ret = cfg.storage.retention_days.unwrap_or(30) as i32;
    let pt = p.trim();
    if p.is_empty() || pt.eq_ignore_ascii_case("off") {
        ("disabled".to_string(), 0)
    } else if pt == ":memory:" {
        ("in-memory".to_string(), ret)
    } else {
        (p, ret)
    }
}

/// Loads + decodes + resolves the workflow config and assembles the banner's presentation-ready
/// [`banner::Data`], mirroring the daemon's own resolution (server port, storage, otel,
/// resolve_projects). Returns `None` on a load or decode error so the caller skips the banner (the
/// daemon's own load reports the error). `db_override` / `no_store` mirror the `--db` / `--no-store`
/// flags so the Storage row reflects the effective store. The `assignee` field is left empty for the
/// caller to fill (a best-effort viewer resolve). Mirrors Go `resolveBannerData`.
pub fn resolve_banner_data(
    workflow_path: &Path,
    dashboard_url: &str,
    db_override: &str,
    no_store: bool,
) -> Option<banner::Data> {
    let def = workflow::load(workflow_path).ok()?;
    let cfg = decode(&def).ok()?;
    // Best-effort resolve (Go `_ = config.Resolve(...)`); fall back to the decoded config on error so
    // the banner still renders.
    let resolved = resolve(cfg.clone(), &workflow_dir(workflow_path)).unwrap_or(cfg);

    let otel_endpoint = {
        let oc = resolve_otel_config(&resolved.otel, os_getenv);
        if oc.enabled {
            oc.endpoint
        } else {
            String::new()
        }
    };
    let (storage_path, retention_days) = resolve_banner_storage(&resolved, db_override, no_store);

    let mut d = banner::Data {
        dashboard_url: dashboard_url.to_string(),
        backend: resolved.agent.backend.clone(),
        max_concurrent: resolved.agent.max_concurrent_agents as i32,
        max_turns: resolved.agent.max_turns as i32,
        // Go: `(time.Duration(cfg.Polling.IntervalMS) * time.Millisecond).String()`.
        poll_interval: go_duration_string(Duration::milliseconds(resolved.polling.interval_ms)),
        active_states: resolved.tracker.active_states.clone(),
        storage_path,
        retention_days,
        otel_endpoint,
        ..Default::default()
    };

    for rp in resolve_projects(&resolved) {
        // BillingGuard defaults to enabled (Go: `guard := true; if ... != nil { guard = *... }`).
        let billing_guard = rp.eff.claude.billing_guard.unwrap_or(true);
        d.projects.push(banner::Project {
            slug: rp.slug,
            repo: rp.repo,
            model: rp.eff.claude.model,
            effort: rp.eff.claude.effort,
            permission_mode: rp.eff.claude.permission_mode,
            billing_guard,
        });
    }
    Some(d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;
    use std::path::PathBuf;

    fn noenv(_: &str) -> String {
        String::new()
    }

    /// A minimal valid workflow with a LITERAL api_key (Go's tests use `$TEST_LINEAR_KEY` +
    /// `t.Setenv`; the Rust workspace avoids process-global env in parallel tests, and the daemon's
    /// boot behavior is identical for any non-empty key — see resolve's `$VAR`→"" semantics). `{}` is
    /// the workspace root.
    const VALID_WF: &str = "---\ntracker:\n  kind: linear\n  endpoint: http://127.0.0.1:9\n  api_key: tok\n  project_slug: proj\npolling:\n  interval_ms: 50\nagent:\n  backend: claude\nworkspace:\n  root: {}\n---\nDo {{ issue.identifier }}.\n";

    fn write_wf(dir: &TempDir, content: &str) -> PathBuf {
        let p = dir.child("WORKFLOW.md");
        std::fs::write(&p, content).expect("write WORKFLOW.md");
        p
    }

    fn valid_wf(ws: &Path) -> String {
        VALID_WF.replace("{}", &ws.to_string_lossy())
    }

    // Mirrors Go `TestAssigneeLabel`.
    #[test]
    fn assignee_label_forms() {
        let cases = [
            (
                Viewer {
                    display_name: "David Johansen".to_string(),
                    email: "d@x.com".to_string(),
                    ..Default::default()
                },
                "David Johansen <d@x.com>",
            ),
            (
                Viewer {
                    display_name: "David Johansen".to_string(),
                    ..Default::default()
                },
                "David Johansen",
            ),
            (
                Viewer {
                    email: "d@x.com".to_string(),
                    ..Default::default()
                },
                "d@x.com",
            ),
            (
                Viewer {
                    id: "u-1".to_string(),
                    ..Default::default()
                },
                "u-1",
            ),
        ];
        for (v, want) in cases {
            assert_eq!(assignee_label(&v), want, "assignee_label({v:?})");
        }
    }

    // Mirrors Go `TestBannerColorEnabled` (a non-TTY buffer / --no-color / NO_COLOR all disable),
    // extended with explicit-TTY cases so the gates are actually exercised.
    #[test]
    fn banner_color_enabled_gates() {
        // A non-terminal writer (Go's `bytes.Buffer`) is never colored.
        assert!(!banner_color_enabled(false, false, noenv));
        // --no-color forces color off even on a TTY.
        assert!(!banner_color_enabled(true, true, noenv));
        // NO_COLOR set forces color off even on a TTY.
        let with_no_color = |k: &str| {
            if k == "NO_COLOR" {
                "1".to_string()
            } else {
                String::new()
            }
        };
        assert!(!banner_color_enabled(true, false, with_no_color));
        // A clean TTY with neither gate → color on.
        assert!(banner_color_enabled(true, false, noenv));
    }

    // Mirrors Go `TestResolveBannerDataSingleProject`.
    #[test]
    fn resolve_banner_data_single_project() {
        let dir = TempDir::new();
        let ws = dir.child("ws");
        let wf = write_wf(&dir, &valid_wf(&ws));

        let d = resolve_banner_data(&wf, "http://127.0.0.1:8080", "", false)
            .expect("resolve_banner_data should succeed on a valid workflow");
        assert_eq!(d.dashboard_url, "http://127.0.0.1:8080");
        assert_eq!(d.backend, "claude");
        assert_eq!(d.poll_interval, "50ms");
        assert_eq!(d.projects.len(), 1);
        assert_eq!(d.projects[0].slug, "proj");
        // Default storage path ends in rhapsody.db with retention 30 (TRA-238: the default DB is
        // ~/.rhapsody/rhapsody.db, diverging from Go v0.4.0's ~/.symphony/symphony.db).
        assert!(
            d.storage_path.ends_with("rhapsody.db"),
            "storage={}",
            d.storage_path
        );
        assert_eq!(d.retention_days, 30);
        // BillingGuard defaults to enabled.
        assert!(
            d.projects[0].billing_guard,
            "billing guard defaults to enabled"
        );
    }

    // BO-12: the capabilities registry colocates with the on-disk store; disabled / in-memory / a
    // failed load yield None so the daemon runs without a registry (capability rendering a no-op).
    #[test]
    fn resolve_capabilities_path_variants() {
        // Default (unset) storage resolves to ~/.rhapsody/rhapsody.db → capabilities.yaml sits beside it.
        let dir = TempDir::new();
        let ws = dir.child("ws");
        let wf = write_wf(&dir, &valid_wf(&ws));
        let cfg = resolve(
            decode(&workflow::load(&wf).expect("load")).expect("decode"),
            &workflow_dir(&wf),
        )
        .expect("resolve");
        let got = resolve_capabilities_path(Some(&cfg), "", false).expect("on-disk default → Some");
        assert!(
            got.ends_with(".rhapsody/capabilities.yaml"),
            "default path should be ~/.rhapsody/capabilities.yaml, got {}",
            got.display()
        );
        // --db override wins and colocates the registry with the overridden store dir.
        let over =
            resolve_capabilities_path(None, "/tmp/somewhere/store.db", false).expect("--db → Some");
        assert_eq!(over, PathBuf::from("/tmp/somewhere/capabilities.yaml"));
        // Disabled / in-memory / no config → None (registry off, capabilities a no-op).
        assert_eq!(resolve_capabilities_path(Some(&cfg), "", true), None); // --no-store
        assert_eq!(resolve_capabilities_path(None, "off", false), None);
        assert_eq!(resolve_capabilities_path(None, ":memory:", false), None);
        assert_eq!(resolve_capabilities_path(None, "", false), None); // failed load (cfg None)
    }

    // STUDIO-672: the reconcile marker follows the same runtime-home rule as every other Teams
    // sidecar. `None` for a store with no on-disk home is load-bearing, not incidental — the triage
    // task reads it as "there is nowhere to record a completed sweep", and disables the sweep.
    #[test]
    fn resolve_teams_reconcile_path_variants() {
        let dir = TempDir::new();
        let ws = dir.child("ws");
        let wf = write_wf(&dir, &valid_wf(&ws));
        let cfg = resolve(
            decode(&workflow::load(&wf).expect("load")).expect("decode"),
            &workflow_dir(&wf),
        )
        .expect("resolve");
        let got =
            resolve_teams_reconcile_path(Some(&cfg), "", false).expect("on-disk default → Some");
        assert!(
            got.ends_with(".rhapsody/teams/reconcile.json"),
            "default path should be ~/.rhapsody/teams/reconcile.json, got {}",
            got.display()
        );
        assert_eq!(
            resolve_teams_reconcile_path(None, "/tmp/somewhere/store.db", false)
                .expect("--db → Some"),
            PathBuf::from("/tmp/somewhere/teams/reconcile.json")
        );
        assert_eq!(resolve_teams_reconcile_path(Some(&cfg), "", true), None); // --no-store
        assert_eq!(resolve_teams_reconcile_path(None, "off", false), None);
        assert_eq!(resolve_teams_reconcile_path(None, ":memory:", false), None);
        assert_eq!(resolve_teams_reconcile_path(None, "", false), None); // failed load
        // Resolving only NAMES a path; it never creates the file. Asserted against a store inside
        // this test's own TempDir — NEVER `got`, which names the real `~/.rhapsody` (STUDIO-688):
        // once a live daemon completed its own sweep and wrote that marker, stat-ing it measured the
        // operator's disk rather than this function, and the assertion failed on every machine
        // sharing that home, CI runner included. That nothing creates the file is proven at the real
        // boundary by `run::tests::run_seeds_capabilities_but_never_seeds_teams_yaml`, which boots
        // the daemon and asserts no `teams/` directory appears beside its store at all.
        let hermetic =
            resolve_teams_reconcile_path(None, &dir.child("rhapsody.db").to_string_lossy(), false)
                .expect("temp-dir --db → Some");
        assert_eq!(hermetic, dir.child("teams").join("reconcile.json"));
        assert!(
            !hermetic.exists(),
            "resolving a path never creates the file; only a completed sweep writes it"
        );
    }

    // STUDIO-639 (Teams T1): teams.yaml colocates with the on-disk store by exactly the same rule as
    // capabilities.yaml (design §2.1); disabled / in-memory / a failed load yield None so the daemon
    // runs with no Teams file to anchor and Teams stays off. Resolving a path never touches the disk —
    // teams.yaml is never seeded (§2.1), unlike the capabilities registry.
    #[test]
    fn resolve_teams_path_variants() {
        let dir = TempDir::new();
        let ws = dir.child("ws");
        let wf = write_wf(&dir, &valid_wf(&ws));
        let cfg = resolve(
            decode(&workflow::load(&wf).expect("load")).expect("decode"),
            &workflow_dir(&wf),
        )
        .expect("resolve");
        let got = resolve_teams_path(Some(&cfg), "", false).expect("on-disk default → Some");
        assert!(
            got.ends_with(".rhapsody/teams.yaml"),
            "default path should be ~/.rhapsody/teams.yaml, got {}",
            got.display()
        );
        // It lands in the SAME directory the capabilities registry does — one runtime home.
        let caps = resolve_capabilities_path(Some(&cfg), "", false).expect("caps → Some");
        assert_eq!(got.parent(), caps.parent());
        // --db override wins and colocates teams.yaml with the overridden store dir.
        let over = resolve_teams_path(None, "/tmp/somewhere/store.db", false).expect("--db → Some");
        assert_eq!(over, PathBuf::from("/tmp/somewhere/teams.yaml"));
        // Disabled / in-memory / no config → None (no home ⇒ Teams off).
        assert_eq!(resolve_teams_path(Some(&cfg), "", true), None); // --no-store
        assert_eq!(resolve_teams_path(None, "off", false), None);
        assert_eq!(resolve_teams_path(None, ":memory:", false), None);
        assert_eq!(resolve_teams_path(None, "", false), None); // failed load (cfg None)
        // Resolving only NAMES a path; that nothing ever creates it is proven at the real boundary by
        // `run::tests::run_seeds_capabilities_but_never_seeds_teams_yaml`, which boots the daemon.
    }

    /// The memory BANKS directory sits beside `teams.yaml` and `teams/profiles/` in the one runtime
    /// home (STUDIO-645; design record §5.4), `memory.path` overrides it outright, and resolving it
    /// only NAMES a path — the directory appears on the first `retain` and at no other time.
    #[test]
    fn resolve_banks_dir_defaults_beside_teams_yaml_and_honours_memory_path() {
        let dir = TempDir::new();
        let cfg = resolve(
            decode(&workflow::Definition {
                config: workflow::YamlMap::new(),
                prompt_template: String::new(),
            })
            .expect("decode"),
            "/tmp",
        )
        .expect("resolve");

        let got = resolve_banks_dir(Some(&cfg), "", "", false).expect("on-disk default → Some");
        assert!(
            got.ends_with(".rhapsody/teams/banks"),
            "default should be ~/.rhapsody/teams/banks, got {}",
            got.display()
        );
        // One runtime home: banks sit under the same directory teams.yaml does.
        let teams = resolve_teams_path(Some(&cfg), "", false).expect("teams.yaml → Some");
        assert_eq!(got.parent().and_then(|p| p.parent()), teams.parent());

        // An explicit `memory.path` wins outright — an operator who points memory elsewhere means it.
        assert_eq!(
            resolve_banks_dir(Some(&cfg), "/somewhere/banks", "", false),
            Some(PathBuf::from("/somewhere/banks"))
        );
        // …even with no store home at all, since it needs no home to anchor to.
        assert_eq!(
            resolve_banks_dir(None, "/somewhere/banks", "", true),
            Some(PathBuf::from("/somewhere/banks"))
        );

        // --db override colocates banks with the overridden store dir.
        assert_eq!(
            resolve_banks_dir(None, "", "/tmp/somewhere/store.db", false),
            Some(PathBuf::from("/tmp/somewhere/teams/banks"))
        );
        // Disabled / in-memory / no config → None: no durable home ⇒ memory stays off rather than
        // writing banks somewhere arbitrary.
        assert_eq!(resolve_banks_dir(Some(&cfg), "", "", true), None);
        assert_eq!(resolve_banks_dir(None, "", "off", false), None);
        assert_eq!(resolve_banks_dir(None, "", ":memory:", false), None);
        assert_eq!(resolve_banks_dir(None, "", "", false), None);

        // Resolving must never create a banks directory — asserted on paths this test owns, never on
        // `got` (the real `~/.rhapsody/teams/banks`). Stat-ing the real home made this assertion
        // VACUOUS the moment a live daemon's first `teams_retain` created that directory: `!got
        // .exists()` went permanently false, and the `||` meant the whole claim rode on
        // `/somewhere/banks`, which nothing here could have created anyway (STUDIO-688).
        let anchored =
            resolve_banks_dir(None, "", &dir.child("rhapsody.db").to_string_lossy(), false)
                .expect("temp-dir --db → Some");
        assert_eq!(anchored, dir.child("teams").join("banks"));
        let overridden = dir.child("elsewhere-banks");
        assert_eq!(
            resolve_banks_dir(None, &overridden.to_string_lossy(), "", false),
            Some(overridden.clone())
        );
        assert!(
            !anchored.exists() && !overridden.exists(),
            "resolving must never create a banks directory ({} / {})",
            anchored.display(),
            overridden.display()
        );
    }

    /// The profiles directory sits BESIDE `teams.yaml` in the one runtime home
    /// (STUDIO-642; design record §1's storage table), and resolving it only
    /// names a path — nothing here creates it.
    #[test]
    fn resolve_profiles_dir_sits_beside_teams_yaml() {
        let dir = TempDir::new();
        let ws = dir.child("ws");
        let wf = write_wf(&dir, &valid_wf(&ws));
        let cfg = resolve(
            decode(&workflow::load(&wf).expect("load")).expect("decode"),
            &workflow_dir(&wf),
        )
        .expect("resolve");
        let got = resolve_profiles_dir(Some(&cfg), "", false).expect("on-disk default → Some");
        assert!(
            got.ends_with(".rhapsody/teams/profiles"),
            "default should be ~/.rhapsody/teams/profiles, got {}",
            got.display()
        );
        let teams = resolve_teams_path(Some(&cfg), "", false).expect("teams.yaml → Some");
        assert_eq!(
            got.parent().and_then(|p| p.parent()),
            teams.parent(),
            "the profiles dir must live beside teams.yaml"
        );
        // --db override wins, and the same off/in-memory/no-config rules apply.
        assert_eq!(
            resolve_profiles_dir(None, "/tmp/somewhere/store.db", false).expect("--db → Some"),
            PathBuf::from("/tmp/somewhere/teams/profiles")
        );
        // Resolving must not create the directory — asserted on a store inside this test's own
        // TempDir, never on `got` (the real `~/.rhapsody/teams/profiles`). Stat-ing the real home is
        // what broke the sibling reconcile test, and this one fails the same way the first time an
        // operator runs `rhapsodyd teams fork`, which is the one thing that creates it (STUDIO-688).
        let anchored =
            resolve_profiles_dir(None, &dir.child("rhapsody.db").to_string_lossy(), false)
                .expect("temp-dir --db → Some");
        assert_eq!(anchored, dir.child("teams").join("profiles"));
        assert!(
            !anchored.exists(),
            "resolving must not create {}",
            anchored.display()
        );
        assert_eq!(resolve_profiles_dir(Some(&cfg), "", true), None); // --no-store
        assert_eq!(resolve_profiles_dir(None, "off", false), None);
        assert_eq!(resolve_profiles_dir(None, ":memory:", false), None);
        assert_eq!(resolve_profiles_dir(None, "", false), None);
    }

    // Mirrors Go `TestResolveBannerStorageVariants`.
    #[test]
    fn resolve_banner_storage_variants() {
        let dir = TempDir::new();
        let ws = dir.child("ws");
        let wf = write_wf(&dir, &valid_wf(&ws));

        let d = resolve_banner_data(&wf, "", "", true).expect("resolve_banner_data ok"); // --no-store
        assert_eq!(d.storage_path, "disabled");
        let d2 = resolve_banner_data(&wf, "", ":memory:", false).expect("resolve_banner_data ok");
        assert_eq!(d2.storage_path, "in-memory");
    }

    // Mirrors Go `TestResolveServerPort`.
    #[test]
    fn resolve_server_port_flag_config_disabled() {
        let dir = TempDir::new();
        let ws = dir.child("ws");
        // Workflow WITHOUT server.port.
        let wf_no_port = write_wf(&dir, &valid_wf(&ws));

        // --port flag set → enabled, flag value wins.
        let (port, enabled) = resolve_server_port(8123, &wf_no_port);
        assert!(
            enabled && port == 8123,
            "flag should enable+win: port={port} enabled={enabled}"
        );
        // No flag (-1) and no server.port → disabled.
        let (_, enabled) = resolve_server_port(-1, &wf_no_port);
        assert!(!enabled, "no flag + no server.port → disabled");
        // No flag but server.port in the workflow → enabled with that port.
        let dir2 = TempDir::new();
        let wf_port = write_wf(
            &dir2,
            "---\ntracker:\n  kind: linear\n  api_key: tok\n  project_slug: p\nserver:\n  port: 9099\n---\nhi\n",
        );
        let (port, enabled) = resolve_server_port(-1, &wf_port);
        assert!(
            enabled && port == 9099,
            "server.port should enable: port={port} enabled={enabled}"
        );
    }
}
