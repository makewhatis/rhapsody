//! P7-D5 parity e2e — the closing build gate: the desktop app's supervisor drives the REAL,
//! release-built, PACKAGED `rhapsodyd` sidecar through **start → healthy → dashboard → stop**, using
//! the app's OWN resolve + supervisor + apiproxy code paths against the R3 harness (linear-stub +
//! fake-claude + minimal.md) — exactly the recipe `harness/e2e/boot.sh` boots the daemon with, but
//! driven through the desktop supervision layer instead of launching rhapsodyd directly.
//!
//! Gated behind `RHAPSODY_PARITY_E2E=1` so a plain `desktop` `cargo test` needs neither the harness
//! nor a built bundle. It REQUIRES `make app` to have produced `Rhapsody.app` first (the release
//! rhapsodyd with its embedded dashboard, copied to `Contents/Resources/rhapsodyd`), then resolves
//! that sidecar exactly as the running app does. Run:
//!
//!   make app && RHAPSODY_PARITY_E2E=1 cargo test -p rhapsody-desktop --test parity_e2e -- --nocapture

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::{HeaderMap, Method, StatusCode};
use rhapsody_desktop::apiproxy::{self, ProxyRequest};
use rhapsody_desktop::supervisor::{Options, State, Supervisor, resolve_binary, resources_dir_for};

const GATE: &str = "RHAPSODY_PARITY_E2E";

/// Total budget for `/api/v1/state` to answer 200 through the apiproxy once the daemon is healthy.
const STATE_POLL_TIMEOUT: Duration = Duration::from_secs(10);
/// Cadence of that poll, mirroring the supervisor's own `/healthz` readiness cadence (250ms).
const STATE_POLL_INTERVAL: Duration = Duration::from_millis(200);

#[tokio::test]
async fn app_supervises_real_rhapsodyd_start_healthy_dashboard_stop() {
    if std::env::var_os(GATE).is_none() {
        eprintln!(
            "skip: set {GATE}=1 to run the P7-D5 parity e2e (run `make app` first — it builds the \
             release rhapsodyd + embedded dashboard and packages Rhapsody.app)"
        );
        return;
    }
    let root = repo_root();

    // 1. Resolve the PACKAGED sidecar exactly as the app does at runtime: from the built bundle's
    //    Contents/MacOS/<exe> -> Contents/Resources/rhapsodyd. Proves `make app`'s copy landed where
    //    supervisor/resolve.rs looks.
    let bundle = root.join("desktop/target/release/bundle/macos/Rhapsody.app");
    assert!(
        bundle.is_dir(),
        "Rhapsody.app not found at {} — run `make app` first",
        bundle.display()
    );
    let app_exe = bundle.join("Contents/MacOS/rhapsody-desktop");
    let resources = resources_dir_for(app_exe.to_str().expect("utf-8 path"))
        .expect("bundle must have a Contents/Resources layout");
    let sidecar = resolve_binary("", resources.to_str().expect("utf-8 path"))
        .expect("resolve the packaged rhapsodyd sidecar from the bundle Resources");

    // 2. Build + launch linear-stub (the scripted Linear GraphQL double), same as boot.sh.
    let stub_bin = build_linear_stub(&root);
    let work = unique_tmp("rhapsody-d5-e2e");
    std::fs::create_dir_all(&work).expect("mkdir work");
    let stub_log = work.join("stub.log");
    let stub = Command::new(&stub_bin)
        .arg("--scenario")
        .arg(root.join("harness/capture/scenarios/success.json"))
        .args(["--port", "0"])
        .stdout(Stdio::from(
            std::fs::File::create(&stub_log).expect("create stub log"),
        ))
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn linear-stub");
    let _stub_guard = ChildGuard(stub); // kills the stub even if an assertion panics
    let stub_port = wait_for_listening(&stub_log);

    // 3. Assemble a WORKFLOW.md from minimal.md under a private $HOME (fake-claude copied in), exactly
    //    as boot.sh does — the daemon needs a valid, reachable tracker config to boot healthy.
    let home = work.join("home");
    std::fs::create_dir_all(home.join("bin")).expect("mkdir home/bin");
    let fake_claude = home.join("bin/fake-claude");
    std::fs::copy(root.join("harness/stubs/fake-claude"), &fake_claude).expect("copy fake-claude");
    set_executable(&fake_claude);
    let store = home.join("symphony.db");
    let workflow = home.join("WORKFLOW.md");
    let template = std::fs::read_to_string(root.join("harness/capture/workflows/minimal.md"))
        .expect("read minimal.md");
    let rendered = template
        .replace("__STUB_PORT__", &stub_port.to_string())
        .replace("__CLAUDE_CMD__", fake_claude.to_str().expect("utf-8 path"))
        .replace("__STORE_PATH__", store.to_str().expect("utf-8 path"));
    std::fs::write(&workflow, rendered).expect("write WORKFLOW.md");

    // 4. Supervise the packaged rhapsodyd through the app's OWN supervisor. It picks a free `--port`
    //    (overriding the workflow's `server.port: 0`) and polls it; a private HOME + the real PATH so
    //    the daemon isolates its DB/runtime.json yet can still exec fake-claude.
    let path = std::env::var("PATH").unwrap_or_default();
    let sup = Supervisor::new(Options {
        binary_path: sidecar,
        workflow_path: Some(workflow),
        base_env: Some(vec![
            format!("HOME={}", home.display()),
            format!("PATH={path}"),
            "FAKE_CLAUDE_SLEEP_S=0".to_string(),
        ]),
        linear_api_key: "stub-key".to_string(),
        startup_timeout: Duration::from_secs(20),
        max_restarts: 1,
        ..Default::default()
    });

    // --- start -> healthy ---
    sup.start(tokio::time::sleep(Duration::from_secs(30)))
        .await
        .expect("supervisor start: the packaged rhapsodyd must become healthy");
    assert_eq!(
        sup.status().state,
        State::Running,
        "want Running once healthy"
    );
    assert!(
        sup.healthy().await,
        "packaged rhapsodyd must answer /healthz"
    );

    // --- dashboard ---
    // The daemon serves its embedded React dashboard at `/` (what the app's window shows once
    // healthy), and the app's same-origin apiproxy reaches the live daemon's API.
    let base = sup.url();
    let client = reqwest::Client::new();

    let root_resp = client.get(&base).send().await.expect("GET dashboard root");
    assert!(
        root_resp.status().is_success(),
        "dashboard root status: {}",
        root_resp.status()
    );
    let body = root_resp.text().await.expect("read dashboard body");
    let low = body.to_lowercase();
    assert!(
        low.contains("<!doctype html")
            || low.contains("<html")
            || low.contains("id=\"root\"")
            || low.contains("<title"),
        "dashboard root is not the embedded HTML app; got: {}",
        body.chars().take(200).collect::<String>()
    );

    // Drive the app's apiproxy (same-origin `/api/*` forwarding) against the LIVE daemon, polled to
    // a bound rather than asked once — see [`poll_proxy_state`] for why a single GET races the
    // daemon's snapshot-ready window.
    let resp = poll_proxy_state(&sup, &client).await;
    assert!(
        !resp.body.is_empty(),
        "apiproxy returned an empty /api/v1/state body"
    );

    // --- stop ---
    sup.stop().await;
    assert_eq!(sup.status().state, State::Stopped, "clean stop");
    assert!(
        !sup.healthy().await,
        "daemon still healthy after stop; SIGTERM did not terminate it"
    );

    std::fs::remove_dir_all(&work).ok();
    eprintln!(
        "parity e2e OK: app supervised the packaged rhapsodyd start -> healthy -> dashboard -> stop"
    );
}

/// Drives the app's apiproxy at `GET /api/v1/state` until it forwards a 200 from the live daemon,
/// bounded by [`STATE_POLL_TIMEOUT`], and returns that response.
///
/// `/healthz` answers as soon as the daemon's HTTP server is up, which can be BEFORE the
/// orchestrator has published its first snapshot — `handle_state` then returns a transient 503
/// `snapshot_unavailable` (its own `SNAPSHOT_TIMEOUT` elapsing), and a forward attempted in that
/// same window can hiccup into a 502. A single unretried GET straight after `sup.start` catches
/// exactly that window and fails a run that is not actually broken. So this polls the same way
/// `sup.start` already polls `/healthz`.
///
/// Retrying does NOT mask a real forward break: a `/state` that never reaches 200 within the bound
/// still fails the test, reporting the last status and body it saw.
///
/// The daemon target is re-resolved from the supervisor on EVERY attempt, exactly as the proxy does
/// in the running app (see `apiproxy::handle`) — the bound port is reassigned across a restart, so a
/// target captured once before the loop could go stale mid-poll.
async fn poll_proxy_state(sup: &Supervisor, client: &reqwest::Client) -> apiproxy::ProxyResponse {
    let deadline = Instant::now() + STATE_POLL_TIMEOUT;
    loop {
        let state = sup.status().state;
        let proxy_url = sup.url();
        let resp = apiproxy::handle(
            ProxyRequest {
                method: Method::GET,
                path: "/api/v1/state".to_string(),
                query: None,
                headers: HeaderMap::new(),
                body: Bytes::new(),
            },
            client,
            |_| panic!("an /api/* request must not fall through to the asset handler"),
            || apiproxy::usable_base_url(state, &proxy_url),
        )
        .await;
        if resp.status == StatusCode::OK {
            return resp;
        }
        assert!(
            Instant::now() < deadline,
            "apiproxy did not forward /api/v1/state to the live daemon within \
             {STATE_POLL_TIMEOUT:?}; last response: {} {}",
            resp.status,
            String::from_utf8_lossy(&resp.body)
                .chars()
                .take(200)
                .collect::<String>()
        );
        tokio::time::sleep(STATE_POLL_INTERVAL).await;
    }
}

/// The repo-root workspace: `CARGO_MANIFEST_DIR` is `desktop/src-tauri`, so it is two levels up.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("resolve repo root")
}

/// Builds the `linear-stub` binary from the root workspace (a distinct target dir from desktop's) and
/// returns its path. Mirrors the smoke test's in-test build of the real binary it drives.
fn build_linear_stub(root: &Path) -> PathBuf {
    let status = Command::new(env!("CARGO"))
        .args(["build", "--release", "-p", "linear-stub", "--manifest-path"])
        .arg(root.join("Cargo.toml"))
        .status()
        .expect("run cargo build linear-stub");
    assert!(
        status.success(),
        "cargo build --release -p linear-stub failed"
    );
    let bin = root.join("target/release/linear-stub");
    assert!(bin.exists(), "linear-stub missing at {}", bin.display());
    bin
}

/// Polls `log` until linear-stub announces `LISTENING <port>` on stdout, returning the port.
fn wait_for_listening(log: &Path) -> u16 {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(mut f) = std::fs::File::open(log) {
            let mut s = String::new();
            let _ = f.read_to_string(&mut s);
            for line in s.lines() {
                if let Some(rest) = line.strip_prefix("LISTENING ")
                    && let Ok(p) = rest.trim().parse::<u16>()
                {
                    return p;
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "linear-stub did not announce LISTENING within the deadline"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A unique temp dir path (not yet created) for one test run.
fn unique_tmp(prefix: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "{prefix}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ))
}

/// Marks `p` executable (0755) — the daemon execs the copied fake-claude by absolute path.
fn set_executable(p: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mut perm = std::fs::metadata(p).expect("stat").permissions();
    perm.set_mode(0o755);
    std::fs::set_permissions(p, perm).expect("chmod fake-claude");
}

/// Kills the wrapped child on drop so a panicking assertion never leaks the linear-stub process.
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
