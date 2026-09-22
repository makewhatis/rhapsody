//! End-to-end proof of STUDIO-981/P0c's authenticated desktop-to-daemon credential channel against
//! the REAL, separately-built `rhapsodyd` binary — not a fake/stub, and not an in-process test
//! double for either side. Gated behind `RHAPSODY_CREDENTIAL_BOOTSTRAP_E2E=1` for the same reason
//! `real_rhapsodyd_smoke.rs` gates on `RHAPSODY_SMOKE_RHAPSODYD`: a second `cargo build` from the
//! required `desktop` CI job would contend with the root `test` job's shared target dir.
//!
//! Three scenarios, each launching the real daemon binary with `--credential-probe-account`/
//! `--credential-probe-binding` so it runs an actual authenticate-then-`read_bound` round trip (not
//! merely a `connect`, which the server never acknowledges either way — see
//! `credential_bootstrap::serve_one`'s no-oracle rejection, and jimmy's review of an earlier
//! revision of this test at rhapsody#213, which is exactly why these flags exist):
//!   - `legitimate_launch_authenticates_and_reads_a_real_credential`: exactly what the real desktop
//!     supervisor must do — bind a `BootstrapListener`, spawn the real `rhapsodyd` with
//!     `--credential-bootstrap` and a piped stdin, write the one bootstrap frame, close the pipe.
//!     The real daemon binary connects over a REAL Unix socket, authenticates, and round-trips a
//!     `read_bound` against a real `ProviderCredentialOwner` — proving the full chain the unit tests
//!     each proved in isolation actually composes. Asserts the daemon logs `state=Present`.
//!   - `wrong_token_launch_reports_owner_unauthorized`: the SAME real binary and the SAME real
//!     socket/owner, but the bootstrap frame carries a token nobody issued. The daemon must report
//!     `OwnerUnauthorized`, never `Present`, and never claim it authenticated.
//!   - `direct_launch_with_no_bootstrap_gets_no_credential_owner`: the confused-deputy shape — the
//!     SAME real binary, launched directly with `--credential-bootstrap` and closed stdin (exactly
//!     what a coding-harness child capable of executing an arbitrary binary on disk would do,
//!     bypassing the real supervisor entirely). It must still boot and serve `/healthz` normally,
//!     with nowhere to obtain a credential from — there is no socket path it was ever told about,
//!     and (per `no_direct_keychain_dependency.rs`, in the `rhapsodyd` crate) no Keychain read API
//!     called anywhere under `crates/` for it to fall back to even if it wanted to.
//!
//! Run: `RHAPSODY_CREDENTIAL_BOOTSTRAP_E2E=1 cargo test --test credential_bootstrap_e2e -- --nocapture`.

use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use rhapsody_credential_ipc::domain::{Binding, CredentialRef, Revision};
use rhapsody_desktop::credential_bootstrap::BootstrapListener;
use rhapsody_desktop::provider_credential::ProviderCredentialOwner;

/// Removes the disposable test item on drop (including on a panicking assertion), so this gated,
/// occasionally-run e2e test never leaves a Keychain item behind — it uses the REAL macOS Keychain
/// (not a mock double, which is `#[cfg(test)]`-only and unreachable from an external integration
/// test binary), under the same derived, disposable provider id every other test in this ticket
/// uses, never the Linear item.
struct CleanupGuard {
    owner: Arc<ProviderCredentialOwner>,
    revision: std::sync::Mutex<Revision>,
}

impl Drop for CleanupGuard {
    fn drop(&mut self) {
        let rev = *self.revision.lock().unwrap();
        let _ = self.owner.remove(rev);
    }
}

fn skip_unless_enabled() -> bool {
    if std::env::var_os("RHAPSODY_CREDENTIAL_BOOTSTRAP_E2E").is_none() {
        eprintln!(
            "skip: set RHAPSODY_CREDENTIAL_BOOTSTRAP_E2E=1 to run the real-rhapsodyd credential \
             bootstrap e2e (builds the release rhapsodyd)"
        );
        return true;
    }
    false
}

fn build_release_rhapsodyd() -> std::path::PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("resolve repo root");
    let status = Command::new(env!("CARGO"))
        .args(["build", "--release", "-p", "rhapsodyd", "--manifest-path"])
        .arg(root.join("Cargo.toml"))
        .status()
        .expect("run cargo build");
    assert!(
        status.success(),
        "cargo build --release -p rhapsodyd failed"
    );
    let bin = root.join("target/release/rhapsodyd");
    assert!(
        bin.exists(),
        "release rhapsodyd missing at {}",
        bin.display()
    );
    bin
}

fn pick_free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    listener.local_addr().expect("local addr").port()
}

async fn wait_healthy(port: u16) -> bool {
    let client = reqwest::Client::new();
    for _ in 0..40 {
        if let Ok(resp) = client
            .get(format!("http://127.0.0.1:{port}/healthz"))
            .send()
            .await
            && resp.status().is_success()
        {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    false
}

/// A minimal but VALID WORKFLOW.md — the same shape `run.rs`'s own hermetic tests use
/// (`write_wf`): a dead-loopback tracker endpoint (so nothing ever leaves the machine) and
/// workspace/logging roots inside this test's own temp dir, so a real daemon boots cleanly and
/// reaches its control loop instead of exiting on config validation before this test's actual
/// subject (the credential bootstrap) is ever exercised.
fn temp_workflow_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("rd-cbe2e-{name}-{}", std::process::id()));
    let ws = dir.join("ws");
    let logs = dir.join("logs");
    std::fs::create_dir_all(&dir).unwrap();
    let body = format!(
        "---\ntracker:\n  kind: linear\n  endpoint: http://127.0.0.1:9\n  api_key: tok\n  project_slug: proj\npolling:\n  interval_ms: 50\nagent:\n  backend: claude\nworkspace:\n  root: {ws}\nlogging:\n  dir: {logs}\nstorage:\n  path: \"off\"\n---\nWork the issue.\n",
        ws = ws.display(),
        logs = logs.display(),
    );
    std::fs::write(dir.join("WORKFLOW.md"), body).unwrap();
    dir
}

#[tokio::test]
async fn legitimate_launch_authenticates_and_reads_a_real_credential() {
    if skip_unless_enabled() {
        return;
    }
    let bin = build_release_rhapsodyd();
    let dir = temp_workflow_dir("legit");

    // The REAL OS Keychain, under the same disposable derived test account every P0c test uses —
    // never the Linear item, and cleaned up unconditionally by `CleanupGuard` below.
    let credential_ref = CredentialRef::for_provider("spike-test-provider").expect("valid id");
    let owner = Arc::new(ProviderCredentialOwner::new(&credential_ref));
    let binding = Binding {
        provider_id: "spike-test-provider".into(),
        adapter: "openai-chat-completions-bearer-v1".into(),
        base_url: "https://api.example/v1".into(),
    };
    let connected_revision = match owner
        .connect(
            Revision::INITIAL,
            binding,
            "sk-e2e-real-binary-secret".into(),
        )
        .expect("seed a real Keychain credential")
    {
        rhapsody_desktop::provider_credential::MutationOutcome::Advanced(r) => r,
        other => panic!("{other:?}"),
    };
    let _cleanup = CleanupGuard {
        owner: owner.clone(),
        revision: std::sync::Mutex::new(connected_revision),
    };

    let socket_dir = std::env::temp_dir().join(format!("rd-cbe2e-sock-{}", std::process::id()));
    let listener = BootstrapListener::bind(&socket_dir).expect("bind real unix socket");
    let bootstrap_msg = listener.bootstrap_message();
    let (serve_fut, shutdown) = listener.accept_and_serve(owner);
    let serve = tokio::spawn(serve_fut);

    let port = pick_free_port();
    let mut child = Command::new(&bin)
        .args([
            "--credential-bootstrap",
            "--credential-probe-account",
            "v1:spike-test-provider",
            "--credential-probe-binding",
            &serde_json::to_string(&Binding {
                provider_id: "spike-test-provider".into(),
                adapter: "openai-chat-completions-bearer-v1".into(),
                base_url: "https://api.example/v1".into(),
            })
            .unwrap(),
            "--port",
            &port.to_string(),
        ])
        .arg(dir.join("WORKFLOW.md"))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn real rhapsodyd");

    // Exactly what the supervisor must do: write the ONE bootstrap frame, then close the pipe.
    {
        use std::io::Write;
        let bytes = serde_json::to_vec(&bootstrap_msg).unwrap();
        let mut stdin = child.stdin.take().expect("piped stdin");
        stdin
            .write_all(&(bytes.len() as u32).to_be_bytes())
            .expect("write length prefix");
        stdin.write_all(&bytes).expect("write bootstrap frame");
        // Dropping `stdin` here closes the write end, exactly as the real supervisor must.
    }

    assert!(
        wait_healthy(port).await,
        "real rhapsodyd must answer /healthz"
    );
    // Give the credential-bootstrap task (spawned independently of the HTTP server) a moment to
    // complete its connect + authenticate + `read_bound` round trip.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let _ = child.kill();
    let output = child.wait_with_output().expect("wait for daemon");
    let stderr = String::from_utf8_lossy(&output.stderr);
    // The daemon must have actually READ the credential, not merely connected — `state=Present`
    // only appears once a real `read_bound` round trip against the real owner succeeds.
    assert!(
        stderr.contains("provider-credential owner bootstrap resolved")
            && stderr.contains("Present"),
        "daemon stderr must report a resolved Present read, not merely a connect; stderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("panic"),
        "daemon must not panic while bootstrapping the credential channel; stderr:\n{stderr}"
    );

    shutdown.shutdown();
    let _ = serve.await;
    std::fs::remove_dir_all(&dir).ok();
    std::fs::remove_dir_all(&socket_dir).ok();
}

#[tokio::test]
async fn wrong_token_launch_reports_owner_unauthorized() {
    if skip_unless_enabled() {
        return;
    }
    let bin = build_release_rhapsodyd();
    let dir = temp_workflow_dir("wrongtoken");

    let credential_ref = CredentialRef::for_provider("spike-test-provider").expect("valid id");
    let owner = Arc::new(ProviderCredentialOwner::new(&credential_ref));
    let binding = Binding {
        provider_id: "spike-test-provider".into(),
        adapter: "openai-chat-completions-bearer-v1".into(),
        base_url: "https://api.example/v1".into(),
    };
    let connected_revision = match owner
        .connect(
            Revision::INITIAL,
            binding.clone(),
            "sk-e2e-wrong-token-secret".into(),
        )
        .expect("seed a real Keychain credential")
    {
        rhapsody_desktop::provider_credential::MutationOutcome::Advanced(r) => r,
        other => panic!("{other:?}"),
    };
    let _cleanup = CleanupGuard {
        owner: owner.clone(),
        revision: std::sync::Mutex::new(connected_revision),
    };

    let socket_dir = std::env::temp_dir().join(format!("rd-cbe2e-sock-wt-{}", std::process::id()));
    let listener = BootstrapListener::bind(&socket_dir).expect("bind real unix socket");
    let mut bootstrap_msg = listener.bootstrap_message();
    // The token nobody issued: the real owner never generated this value, so the daemon's Hello
    // must be rejected exactly as it would be for an unrelated same-user process.
    bootstrap_msg.token = "a-token-nobody-issued".to_string();
    let (serve_fut, shutdown) = listener.accept_and_serve(owner);
    let serve = tokio::spawn(serve_fut);

    let port = pick_free_port();
    let mut child = Command::new(&bin)
        .args([
            "--credential-bootstrap",
            "--credential-probe-account",
            "v1:spike-test-provider",
            "--credential-probe-binding",
            &serde_json::to_string(&binding).unwrap(),
            "--port",
            &port.to_string(),
        ])
        .arg(dir.join("WORKFLOW.md"))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn real rhapsodyd");

    {
        use std::io::Write;
        let bytes = serde_json::to_vec(&bootstrap_msg).unwrap();
        let mut stdin = child.stdin.take().expect("piped stdin");
        stdin
            .write_all(&(bytes.len() as u32).to_be_bytes())
            .expect("write length prefix");
        stdin.write_all(&bytes).expect("write bootstrap frame");
    }

    assert!(
        wait_healthy(port).await,
        "real rhapsodyd must answer /healthz even with a rejected credential handshake"
    );
    tokio::time::sleep(Duration::from_millis(500)).await;

    let _ = child.kill();
    let output = child.wait_with_output().expect("wait for daemon");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("provider-credential owner bootstrap resolved")
            && stderr.contains("OwnerUnauthorized"),
        "a wrong-token launch must report OwnerUnauthorized; stderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("Present"),
        "a wrong-token launch must never report a Present read; stderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("panic"),
        "daemon must not panic on a rejected credential handshake; stderr:\n{stderr}"
    );

    shutdown.shutdown();
    let _ = serve.await;
    std::fs::remove_dir_all(&dir).ok();
    std::fs::remove_dir_all(&socket_dir).ok();
}

#[tokio::test]
async fn direct_launch_with_no_bootstrap_gets_no_credential_owner() {
    if skip_unless_enabled() {
        return;
    }
    let bin = build_release_rhapsodyd();
    let dir = temp_workflow_dir("deputy");

    // The confused-deputy shape: launch the exact same signed/real binary directly, passing the
    // SAME flag a legitimate supervisor would, but with stdin closed — no bootstrap frame ever
    // arrives, because there is no real desktop parent on the other end of a pipe.
    let port = pick_free_port();
    let mut child = Command::new(&bin)
        .args(["--credential-bootstrap", "--port", &port.to_string()])
        .arg(dir.join("WORKFLOW.md"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn real rhapsodyd directly");

    // It must still boot and stay healthy — a confused-deputy launch is not a crash, it is simply
    // an ordinary daemon with no credential owner.
    assert!(
        wait_healthy(port).await,
        "a direct launch with no bootstrap must still boot and answer /healthz"
    );
    tokio::time::sleep(Duration::from_millis(500)).await;

    let _ = child.kill();
    let output = child.wait_with_output().expect("wait for daemon");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no provider-credential bootstrap frame received"),
        "daemon stderr must report it has no credential owner; stderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("provider-credential owner connected"),
        "a confused-deputy direct launch must never connect to a credential owner; stderr:\n{stderr}"
    );

    std::fs::remove_dir_all(&dir).ok();
}
