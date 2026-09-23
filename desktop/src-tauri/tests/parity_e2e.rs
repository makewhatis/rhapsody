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

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::{HeaderMap, Method, StatusCode};
use rhapsody_credential_ipc::domain::{
    Binding, BoundCredentialLease, CredentialRead, CredentialRef, CredentialState, Revision,
};
use rhapsody_credential_ipc::owner::{
    CredentialOwner, CredentialStatus, MutationError, MutationOutcome,
};
use rhapsody_desktop::apiproxy::{self, ProxyRequest};
use rhapsody_desktop::credential_bootstrap::{ChannelObservations, CredentialOwnerLookup};
use rhapsody_desktop::provider_commands::{
    DaemonObservation, HttpConnectionTester, OwnerFactory, ProviderCommandService,
    ProviderOperation, SystemClock,
};
use rhapsody_desktop::supervisor::{
    CredentialBootstrap, Options, State, Supervisor, resolve_binary, resources_dir_for,
};

mod support;

use support::SupervisorGuard;

const GATE: &str = "RHAPSODY_PARITY_E2E";

/// Total budget for `/api/v1/state` to answer 200 through the apiproxy once the daemon is healthy.
const STATE_POLL_TIMEOUT: Duration = Duration::from_secs(10);
/// Cadence of that poll — the same order as the supervisor's own 250ms `/healthz` readiness poll,
/// short enough that the usual case (the snapshot lands almost immediately) costs one extra tick.
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
    // STUDIO-1038: the daemon runs in its own process group, so any assertion panic below would
    // orphan it; the guard kills the group on every exit path (a no-op once `stop()` has run).
    let _sup_guard = SupervisorGuard::new(&sup);

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
/// `snapshot_unavailable` (its own `SNAPSHOT_TIMEOUT` elapsing). A single unretried GET straight
/// after `sup.start` catches exactly that window and fails a run that is not actually broken. So
/// this polls the same way `sup.start` already polls `/healthz`. (A transient 502 from a failed
/// forward is a separate matter — the daemon's server is demonstrably up by then — but retrying
/// absorbs that too.)
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

/// A unique temp dir for one test run, created here and removed on drop (STUDIO-1031), so an
/// e2e run leaves no `rhapsody-d5-e2e-*` entry in `$TMPDIR`. `Deref`s to `Path`.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(prefix: &str) -> TempDir {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!(
            "{prefix}-{}-{}-{nonce}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        TempDir { path }
    }
}

impl std::ops::Deref for TempDir {
    type Target = Path;
    fn deref(&self) -> &Self::Target {
        &self.path
    }
}

impl AsRef<Path> for TempDir {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        if std::env::var_os("RHAPSODY_KEEP_TEST_DIRS").is_none() {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

fn unique_tmp(prefix: &str) -> TempDir {
    TempDir::new(prefix)
}

/// A scratch dir directly under `/tmp` (removed on drop), for the credential-listener socket: a Unix
/// `sun_path` is capped at ~104 bytes, and the per-session `$TMPDIR` is long enough on its own to
/// blow that budget. Not `rhapsody-`-prefixed for that reason; the drop guard is what keeps it tidy.
struct ShortDir {
    path: PathBuf,
}

impl ShortDir {
    fn new(prefix: &str) -> ShortDir {
        let path = PathBuf::from("/tmp").join(format!("{prefix}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create short dir");
        ShortDir { path }
    }
}

impl std::ops::Deref for ShortDir {
    type Target = Path;
    fn deref(&self) -> &Self::Target {
        &self.path
    }
}

impl Drop for ShortDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
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

// ---- STUDIO-1035: packaged desktop-to-sidecar credential bootstrap -------------------------------

/// A read/write in-memory `CredentialOwner` — the injected stand-in for the production Keychain
/// owner. The [`CredentialOwner`] trait IS the P0c crate/process boundary, so serving this over the
/// authenticated channel exercises the same supervisor wiring the real Keychain owner uses, without
/// requiring the CI runner's login keychain to be unlocked. The production owner's own CAS semantics
/// (and their mutation-discipline tests) live in `provider_credential`.
struct InMemoryOwner {
    revision: Mutex<Revision>,
    state: Mutex<Option<(Binding, String)>>,
}

impl InMemoryOwner {
    fn new() -> Arc<InMemoryOwner> {
        Arc::new(InMemoryOwner {
            revision: Mutex::new(Revision::INITIAL),
            state: Mutex::new(None),
        })
    }
}

impl CredentialOwner for InMemoryOwner {
    fn read_bound(&self, expected_binding: &Binding) -> CredentialRead {
        let revision = *self.revision.lock().expect("revision");
        let state = match self.state.lock().expect("state").as_ref() {
            None => CredentialState::Absent,
            Some((binding, value)) if binding == expected_binding => {
                CredentialState::Present(BoundCredentialLease::new(binding.clone(), value.clone()))
            }
            Some(_) => CredentialState::BindingMismatch,
        };
        CredentialRead { revision, state }
    }
    fn connect(
        &self,
        expected_revision: Revision,
        binding: Binding,
        value: String,
    ) -> Result<MutationOutcome, MutationError> {
        if expected_revision != *self.revision.lock().expect("revision") {
            return Err(MutationError::StaleRevision(
                *self.revision.lock().expect("revision"),
            ));
        }
        let mut state = self.state.lock().expect("state");
        if state.is_some() {
            return Err(MutationError::PreconditionFailed);
        }
        *state = Some((binding, value));
        drop(state);
        Ok(MutationOutcome::Advanced(self.advance()))
    }
    fn replace(
        &self,
        expected_revision: Revision,
        current_binding: &Binding,
        new_value: String,
    ) -> Result<MutationOutcome, MutationError> {
        if expected_revision != *self.revision.lock().expect("revision") {
            return Err(MutationError::StaleRevision(
                *self.revision.lock().expect("revision"),
            ));
        }
        let mut state = self.state.lock().expect("state");
        match state.as_ref() {
            Some((binding, _)) if binding == current_binding => {}
            _ => return Err(MutationError::PreconditionFailed),
        }
        let existing = state.take().ok_or(MutationError::PreconditionFailed)?;
        *state = Some((existing.0, new_value));
        drop(state);
        Ok(MutationOutcome::Advanced(self.advance()))
    }
    fn rebind(
        &self,
        _expected_revision: Revision,
        _new_binding: Binding,
    ) -> Result<MutationOutcome, MutationError> {
        Err(MutationError::PreconditionFailed)
    }
    fn remove(&self, expected_revision: Revision) -> Result<MutationOutcome, MutationError> {
        if expected_revision != *self.revision.lock().expect("revision") {
            return Err(MutationError::StaleRevision(
                *self.revision.lock().expect("revision"),
            ));
        }
        let mut state = self.state.lock().expect("state");
        if state.is_none() {
            return Ok(MutationOutcome::AlreadyAbsent(expected_revision));
        }
        *state = None;
        drop(state);
        Ok(MutationOutcome::Advanced(self.advance()))
    }
    fn status(&self) -> CredentialStatus {
        if self.state.lock().expect("state").is_some() {
            CredentialStatus::Configured
        } else {
            CredentialStatus::Unconfigured
        }
    }
}

impl InMemoryOwner {
    fn advance(&self) -> Revision {
        let mut revision = self.revision.lock().expect("revision");
        *revision = revision.next();
        *revision
    }
}

/// A shared provider-id → owner registry, used BOTH as the command service's owner factory and as
/// the credential listener's lookup — so the daemon observes the exact instance the command surface
/// mutates (one revision counter per provider).
#[derive(Clone)]
struct SharedOwners {
    owners: Arc<Mutex<BTreeMap<String, Arc<dyn CredentialOwner>>>>,
}

impl SharedOwners {
    fn factory(&self) -> OwnerFactory {
        let owners = self.owners.clone();
        Arc::new(move |provider_id: &str| owners.lock().ok()?.get(provider_id).cloned())
    }
}

impl CredentialOwnerLookup for SharedOwners {
    fn owner_for(&self, account: &str) -> Option<Arc<dyn CredentialOwner>> {
        let provider_id = account.strip_prefix("v1:")?;
        CredentialRef::for_provider(provider_id).ok()?;
        self.owners.lock().ok()?.get(provider_id).cloned()
    }
}

/// One provider definition in the packaged e2e's WORKFLOW.md, bound to a dead loopback endpoint
/// (the boot status refresh only reads the credential — it never contacts the provider).
fn workflow_with_providers(ws: &Path, logs: &Path, provider_ids: &[&str]) -> String {
    let mut providers = String::new();
    for id in provider_ids {
        providers.push_str(&format!(
            "  {id}:\n    protocol: openai-compatible\n    display_name: {id}\n    base_url: https://127.0.0.1:9/v1\n    credential:\n      source: keychain\n"
        ));
    }
    format!(
        "---\ntracker:\n  kind: linear\n  endpoint: http://127.0.0.1:9\n  api_key: tok\n  project_slug: proj\nproviders:\n{providers}workspace:\n  root: {ws}\nlogging:\n  dir: {logs}\nstorage:\n  path: \"off\"\n---\nWork.\n",
        ws = ws.display(),
        logs = logs.display(),
    )
}

/// Polls the shared observations map until the daemon has been served `account`, bounded.
async fn wait_observed(
    observations: &Arc<ChannelObservations>,
    account: &str,
    timeout: Duration,
) -> Revision {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(revision) = observations.observed(account) {
            return revision;
        }
        assert!(
            Instant::now() < deadline,
            "the packaged daemon never read {account} over the credential channel within {timeout:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The packaged P0c ownership model, end to end (STUDIO-1035): the app's OWN supervisor spawns the
/// REAL packaged `rhapsodyd` from the `make app` bundle with `--credential-bootstrap`, serves the
/// authenticated channel from the injected desktop-owned `CredentialOwner`, and the daemon's boot
/// provider status refresh reads the credential — the revision lands in the shared observations map
/// that the provider commands read. A credential stored while the daemon was offline is observed on
/// the next startup, and the sync verdict is `stored_unsynchronized` until the daemon has observed
/// the mutation's revision (never before).
///
/// Gated with the P7-D5 parity e2e (`RHAPSODY_PARITY_E2E=1`, `make app` first).
#[tokio::test]
async fn packaged_supervisor_bootstraps_a_stored_credential_and_reports_the_sync_verdict() {
    if std::env::var_os(GATE).is_none() {
        eprintln!(
            "skip: set {GATE}=1 to run the packaged credential-bootstrap e2e (`make app` first)"
        );
        return;
    }
    let root = repo_root();
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
        .expect("resolve the packaged rhapsodyd sidecar");

    let work = unique_tmp("rhapsody-1035-e2e");
    let ws = work.join("ws");
    let logs = work.join("logs");
    std::fs::create_dir_all(&ws).expect("mkdir ws");
    std::fs::create_dir_all(&logs).expect("mkdir logs");
    let workflow = work.join("WORKFLOW.md");
    std::fs::write(
        &workflow,
        workflow_with_providers(&ws, &logs, &["packaged-e2e", "packaged-e2e-absent"]),
    )
    .expect("write WORKFLOW.md");

    // Two desktop-owned credentials: one stored while the daemon is OFFLINE (the acceptance's
    // store-offline case), and one deliberately absent to exercise `already_absent` synchronization.
    let present = InMemoryOwner::new();
    let present_binding = Binding {
        provider_id: "packaged-e2e".into(),
        adapter: "openai-chat-completions-bearer-v1".into(),
        base_url: "https://127.0.0.1:9/v1".into(),
    };
    let stored_revision = match present
        .connect(
            Revision::INITIAL,
            present_binding.clone(),
            "sk-packaged-e2e-fake".into(),
        )
        .expect("seed the offline credential")
    {
        MutationOutcome::Advanced(r) => r,
        other => panic!("unexpected connect outcome: {other:?}"),
    };
    let absent = InMemoryOwner::new();

    let shared = SharedOwners {
        owners: Arc::new(Mutex::new(BTreeMap::from([
            (
                "packaged-e2e".to_string(),
                present.clone() as Arc<dyn CredentialOwner>,
            ),
            (
                "packaged-e2e-absent".to_string(),
                absent.clone() as Arc<dyn CredentialOwner>,
            ),
        ]))),
    };
    let observations = Arc::new(ChannelObservations::new());

    // The command service and the listener share ONE owner registry (see `SharedOwners`).
    let service = ProviderCommandService::with_dependencies(
        Some(workflow.clone()),
        shared.factory(),
        Arc::new(HttpConnectionTester::new()),
        Arc::new(SystemClock),
        Arc::new(rhapsody_credential_ipc::token::generate),
        std::time::Duration::from_secs(120),
    );

    let socket_dir = ShortDir::new("rd-1035-sock");

    let path = std::env::var("PATH").unwrap_or_default();
    let sup = Supervisor::new(Options {
        binary_path: sidecar,
        workflow_path: Some(workflow.clone()),
        base_env: Some(vec![
            format!("HOME={}", work.join("home").display()),
            format!("PATH={path}"),
        ]),
        linear_api_key: "stub-key".to_string(),
        startup_timeout: Duration::from_secs(20),
        max_restarts: 1,
        credential_bootstrap: Some(CredentialBootstrap {
            socket_dir: socket_dir.to_path_buf(),
            lookup: Arc::new(shared.clone()),
            observations: observations.clone(),
        }),
        ..Default::default()
    });
    // STUDIO-1038: as above — a failed assertion must not leave this daemon (or its temp dir)
    // behind. Declared after `work` so it drops (kills the daemon) before the temp dir is removed.
    let _sup_guard = SupervisorGuard::new(&sup);

    sup.start(tokio::time::sleep(Duration::from_secs(30)))
        .await
        .expect("supervisor start: the packaged rhapsodyd must become healthy");
    assert_eq!(sup.status().state, State::Running);

    // Bullet 3: the credential stored while the daemon was offline is observed on this startup.
    let observed = wait_observed(&observations, "v1:packaged-e2e", Duration::from_secs(10)).await;
    assert_eq!(
        observed, stored_revision,
        "the daemon must observe the revision stored while it was offline"
    );
    let absent_observed = wait_observed(
        &observations,
        "v1:packaged-e2e-absent",
        Duration::from_secs(10),
    )
    .await;
    assert_eq!(absent_observed, Revision::INITIAL);

    // Bullet 2: a committed mutation reports `stored_unsynchronized` — never `synchronized` — while
    // the daemon's observed revision is older than the mutation's, then `synchronized` is reachable
    // once the daemon has observed the revision it is acknowledged against.
    let prepared = service
        .prepare("packaged-e2e", ProviderOperation::Replace)
        .expect("prepare replace");
    let stale = service
        .commit(
            "packaged-e2e",
            ProviderOperation::Replace,
            &prepared.nonce,
            Some("sk-packaged-e2e-replaced".into()),
            DaemonObservation {
                running: true,
                observed_revision: Some(observed),
            },
        )
        .expect("commit replace");
    assert!(stale.mutated);
    assert_eq!(
        stale.sync, "stored_unsynchronized",
        "a mutation the daemon has not observed must never claim synchronization"
    );

    // `already_absent` on the other credential is acknowledged against the UNCHANGED revision it was
    // observed at, without a mutation — and IS synchronized because the daemon has observed it.
    let prepared = service
        .prepare("packaged-e2e-absent", ProviderOperation::Remove)
        .expect("prepare remove");
    let acknowledged = service
        .commit(
            "packaged-e2e-absent",
            ProviderOperation::Remove,
            &prepared.nonce,
            None,
            DaemonObservation {
                running: true,
                observed_revision: Some(absent_observed),
            },
        )
        .expect("commit remove");
    assert!(!acknowledged.mutated);
    assert_eq!(acknowledged.sync, "synchronized");

    // With the daemon stopped, the same class of mutation is `stored_offline`, never synchronized.
    sup.stop().await;
    assert_eq!(sup.status().state, State::Stopped);
    let prepared = service
        .prepare("packaged-e2e", ProviderOperation::Replace)
        .expect("prepare replace offline");
    let offline = service
        .commit(
            "packaged-e2e",
            ProviderOperation::Replace,
            &prepared.nonce,
            Some("sk-packaged-e2e-offline".into()),
            DaemonObservation {
                running: false,
                observed_revision: Some(stored_revision),
            },
        )
        .expect("commit offline");
    assert_eq!(offline.sync, "stored_offline");

    std::fs::remove_dir_all(&work).ok();
    eprintln!(
        "STUDIO-1035 packaged e2e OK: supervisor bootstrapped the credential owner; offline store \
         observed on startup; sync verdicts ordered correctly"
    );
}
