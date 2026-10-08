//! Disposable lead investigation shell (STUDIO-1135; tech-lead-design §3.4).
//! No Go counterpart. Docker execution and cache warm-up run off the control task.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};

pub const IMAGE: &str = "rhapsody-investigate:rust-1.97.0-node-22.16.0-v1";
pub const PATH: &str = "/opt/rust/bin:/usr/local/bin:/usr/bin:/bin";
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(600);
pub const SESSION_TIMEOUT: Duration = Duration::from_secs(1800);
pub const OUTPUT_LIMIT: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InvestigateError {
    #[error("Docker is unavailable")]
    DockerMissing,
    #[error("investigate unavailable: {0}")]
    Unavailable(String),
    #[error("investigation container disappeared or was OOM-killed")]
    SessionLost,
    #[error("investigation command timed out")]
    CommandTimeout,
    #[error("investigation session ended")]
    SessionEnded,
    #[error("invalid investigation reference")]
    InvalidRef,
}

impl InvestigateError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::DockerMissing => "docker_unavailable",
            Self::Unavailable(_) => "investigate_unavailable",
            Self::SessionLost => "investigate_session_lost",
            Self::CommandTimeout => "investigate_timeout",
            Self::SessionEnded => "investigate_session_ended",
            Self::InvalidRef => "invalid_revision",
        }
    }
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct CommandOutput {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
}

#[async_trait::async_trait]
trait Executor: Send + Sync {
    async fn run(
        &self,
        args: Vec<String>,
        timeout: Duration,
    ) -> Result<CommandOutput, InvestigateError>;
}

fn container_args(name: &str, repo: &str, cache: &str, image: &str) -> Vec<String> {
    strings(&[
        "run",
        "--rm",
        "--detach",
        "--name",
        name,
        "--pull",
        "never",
        "--log-driver",
        "none",
        "--network",
        "none",
        "--read-only",
        "--cap-drop",
        "ALL",
        "--security-opt",
        "no-new-privileges",
        "--user",
        "1000:1000",
        "--pids-limit",
        "512",
        "--cpus",
        "2",
        "--memory",
        "4g",
        "--tmpfs",
        "/scratch:rw,nosuid,nodev,exec,size=1g,mode=1777",
        "--mount",
        &format!("type=bind,src={repo},dst=/repo,readonly"),
        "--mount",
        &format!("type=volume,src={cache},dst=/cache,readonly"),
        "--workdir",
        "/repo",
        "--env",
        &format!("PATH={PATH}"),
        "--env",
        "HOME=/scratch",
        image,
        "env",
        "-i",
        &format!("PATH={PATH}"),
        "HOME=/scratch",
        "sleep",
        "1800",
    ])
}

// /inputs carries only dependency metadata and empty Cargo targets, never sources or config.
const WARMUP: &str = r#"set -eu
cp -R /inputs/. /scratch/
export CARGO_HOME=/cache/cargo
export npm_config_cache=/cache/npm-cache
: > /scratch/.npm-user-empty
: > /scratch/.npm-global-empty
export npm_config_userconfig=/scratch/.npm-user-empty
export npm_config_globalconfig=/scratch/.npm-global-empty
find /scratch -name Cargo.lock -execdir sh -c '
    set -eu
    trap "touch /scratch/.warmup-failed" EXIT
    cargo fetch --locked
    trap - EXIT
' \;
find /scratch -name package-lock.json -not -path '*/node_modules/*' -execdir sh -c '
    set -eu
    trap "touch /scratch/.warmup-failed" EXIT
    npm ci --ignore-scripts --no-audit --no-fund
    relative=${PWD#/scratch}
    mkdir -p "/cache/npm$relative"
    if [ -d node_modules ]; then cp -R node_modules "/cache/npm$relative/"; fi
    trap - EXIT
' \;
test ! -e /scratch/.warmup-failed
"#;

fn warmup_args(name: &str, inputs: &str, cache: &str, image: &str) -> Vec<String> {
    let mut args = container_args(name, inputs, cache, image);
    // Retain fast completed warm-ups until wait has observed their exit code.
    args.retain(|arg| arg != "--rm");
    if let Some(index) = args.iter().position(|arg| arg == "--network") {
        args[index + 1] = "bridge".into();
    }
    for arg in &mut args {
        if arg == &format!("type=bind,src={inputs},dst=/repo,readonly") {
            *arg = format!("type=bind,src={inputs},dst=/inputs,readonly");
        }
        if arg == &format!("type=volume,src={cache},dst=/cache,readonly") {
            *arg = format!("type=volume,src={cache},dst=/cache");
        }
        if arg == "/repo" {
            *arg = "/scratch".into();
        }
    }
    // Record the fixed command in argv; the warm-up has the same bounded session lifetime.
    args.truncate(args.len() - 2);
    args.extend(strings(&[
        "timeout",
        "--signal=KILL",
        "600",
        "sh",
        "-c",
        WARMUP,
    ]));
    args
}

const REFUSAL_PROBES: &[(&str, &str)] = &[
    ("runtime home", "ls \"$HOME/.rhapsody\""),
    ("SSH home", "ls \"$HOME/.ssh\""),
    ("OpenCode data", "ls \"$HOME/.local/share/opencode\""),
    // A default route already grants reach, even when the external test endpoint is down.
    (
        "network route",
        "rg -q '^eth[0-9]+[[:space:]]+00000000' /proc/net/route # network",
    ),
    (
        "network connection",
        "curl --noproxy '*' --connect-timeout 2 --max-time 3 -sf http://1.1.1.1 # network",
    ),
    ("repository write", "touch /repo/.investigate-write-probe"),
    (
        "root filesystem write",
        "touch /var/tmp/.investigate-write-probe",
    ),
    // Examine the original Docker exec environment before env -i. Never print a value.
    (
        "credential environment",
        "env | cut -d= -f1 | rg -q '(_TOKEN|_KEY)$'",
    ),
];

async fn selftest(exec: &dyn Executor, args: Vec<String>) -> Result<(), InvestigateError> {
    let mounts: Vec<_> = args
        .windows(2)
        .filter(|a| a[0] == "--mount")
        .map(|a| &a[1])
        .collect();
    // An extra host-home mount grants reach even if mounted away from HOME. Refuse it regardless
    // of what the model does with it; the two permitted mounts are supplied by the host builder.
    if mounts.len() != 2
        || !mounts.iter().any(|m| m.ends_with("dst=/repo,readonly"))
        || !mounts.iter().any(|m| m.ends_with("dst=/cache,readonly"))
    {
        return Err(InvestigateError::Unavailable(
            "self-test failed: unexpected mount".into(),
        ));
    }
    let name = args
        .windows(2)
        .find(|a| a[0] == "--name")
        .map(|a| a[1].clone())
        .ok_or_else(|| InvestigateError::Unavailable("self-test has no container name".into()))?;
    let created = exec.run(args, Duration::from_secs(30)).await?;
    if created.exit_code != 0 {
        return Err(InvestigateError::Unavailable(
            "self-test container could not start".into(),
        ));
    }
    for (label, cmd) in REFUSAL_PROBES {
        let output = exec
            .run(
                strings(&["exec", &name, "sh", "-c", cmd]),
                Duration::from_secs(10),
            )
            .await?;
        if output.exit_code == 0 || output.exit_code >= 125 {
            return Err(InvestigateError::Unavailable(format!(
                "self-test failed: {label}"
            )));
        }
    }
    for cmd in ["cargo --version", "rg --version", "touch /scratch/.probe"] {
        let output = exec
            .run(exec_args(&name, cmd), Duration::from_secs(10))
            .await?;
        if output.exit_code != 0 {
            return Err(InvestigateError::Unavailable(format!(
                "self-test positive probe failed: {cmd}"
            )));
        }
    }
    Ok(())
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|s| (*s).into()).collect()
}

fn exec_args(name: &str, cmd: &str) -> Vec<String> {
    strings(&[
        "exec",
        "--workdir",
        "/repo",
        name,
        "env",
        "-i",
        &format!("PATH={PATH}"),
        "HOME=/scratch",
        "timeout",
        "--signal=KILL",
        "600",
        "sh",
        "-c",
        cmd,
    ])
}

struct Session {
    exec: Arc<dyn Executor>,
    name: String,
    started: tokio::time::Instant,
    removed: bool,
}

impl Session {
    fn guard(exec: Arc<dyn Executor>, name: String) -> Self {
        Self {
            exec,
            name,
            started: tokio::time::Instant::now(),
            removed: false,
        }
    }
    async fn create(
        exec: Arc<dyn Executor>,
        name: String,
        repo: &str,
        cache: &str,
        image: &str,
    ) -> Result<Self, InvestigateError> {
        // Arm removal before Docker runs: a timeout may have created the named container.
        let session = Self::guard(exec, name);
        let output = session
            .exec
            .run(
                container_args(&session.name, repo, cache, image),
                Duration::from_secs(30),
            )
            .await?;
        if output.exit_code != 0 {
            return Err(InvestigateError::Unavailable(
                "container could not start".into(),
            ));
        }
        let setup = session
            .exec
            .run(
                exec_args(
                    &session.name,
                    "if [ -d /cache/cargo ]; then cp -R /cache/cargo /scratch/.cargo; fi",
                ),
                COMMAND_TIMEOUT,
            )
            .await?;
        if setup.exit_code != 0 {
            return Err(InvestigateError::Unavailable(
                "scratch cache setup failed".into(),
            ));
        }
        Ok(session)
    }

    async fn command(&self, cmd: &str) -> Result<CommandOutput, InvestigateError> {
        let remaining = SESSION_TIMEOUT.saturating_sub(self.started.elapsed());
        if remaining.is_zero() {
            return Err(InvestigateError::SessionEnded);
        }
        let output = self
            .exec
            .run(exec_args(&self.name, cmd), remaining.min(COMMAND_TIMEOUT))
            .await
            .map_err(|e| match e {
                InvestigateError::DockerMissing => InvestigateError::SessionLost,
                other => other,
            })?;
        let state = self
            .exec
            .run(
                strings(&[
                    "inspect",
                    "--format",
                    "{{.State.Running}} {{.State.OOMKilled}}",
                    &self.name,
                ]),
                Duration::from_secs(10),
            )
            .await
            .map_err(|_| InvestigateError::SessionLost)?;
        if state.exit_code != 0 || state.stdout.trim() != "true false" {
            return Err(InvestigateError::SessionLost);
        }
        // The executor's wall-clock deadline is authoritative. Shells can legitimately return
        // GNU timeout's reserved status values too; do not invent a cause from an exit code.
        Ok(output)
    }

    async fn destroy(mut self) {
        if self
            .exec
            .run(
                strings(&["rm", "--force", &self.name]),
                Duration::from_secs(15),
            )
            .await
            .is_ok_and(|o| o.exit_code == 0)
        {
            self.removed = true;
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if self.removed {
            return;
        }
        let exec = Arc::clone(&self.exec);
        let name = self.name.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                if !exec
                    .run(strings(&["rm", "--force", &name]), Duration::from_secs(15))
                    .await
                    .is_ok_and(|o| o.exit_code == 0)
                {
                    tracing::warn!(container = %name, "investigate: container cleanup failed");
                }
            });
        } else {
            tracing::warn!(container = %name, "investigate: no runtime for container cleanup");
        }
    }
}

struct Docker {
    config: PathBuf,
    host: String,
}
#[async_trait::async_trait]
impl Executor for Docker {
    async fn run(
        &self,
        args: Vec<String>,
        timeout: Duration,
    ) -> Result<CommandOutput, InvestigateError> {
        let mut cmd = tokio::process::Command::new("docker");
        cmd.env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.config)
            .args([
                "--config",
                &self.config.to_string_lossy(),
                "--host",
                &self.host,
            ])
            .args(args);
        run_process(cmd, timeout).await
    }
}

async fn bounded_read(mut pipe: impl AsyncRead + Unpin) -> std::io::Result<(Vec<u8>, bool)> {
    let mut output = Vec::new();
    let mut truncated = false;
    let mut buf = [0; 4096];
    loop {
        let n = pipe.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        let keep = (OUTPUT_LIMIT - output.len()).min(n);
        output.extend_from_slice(&buf[..keep]);
        truncated |= keep < n;
    }
    Ok((output, truncated))
}

fn bounded_text(bytes: &[u8], limit: usize) -> String {
    let mut text = String::from_utf8_lossy(bytes).into_owned();
    let mut end = text.len().min(limit);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text
}

async fn run_process(
    mut cmd: tokio::process::Command,
    timeout: Duration,
) -> Result<CommandOutput, InvestigateError> {
    #[cfg(unix)]
    cmd.process_group(0);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd.spawn().map_err(|_| InvestigateError::DockerMissing)?;
    let mut kill = rhapsody_agent::proctree::KillTreeOnDrop::new(child.id().unwrap_or(0));
    let stdout = child.stdout.take().ok_or(InvestigateError::SessionLost)?;
    let stderr = child.stderr.take().ok_or(InvestigateError::SessionLost)?;
    let read = async {
        let (status, out, err) =
            tokio::try_join!(child.wait(), bounded_read(stdout), bounded_read(stderr))
                .map_err(|_| InvestigateError::SessionLost)?;
        let stdout = bounded_text(&out.0, OUTPUT_LIMIT);
        let stderr = bounded_text(&err.0, OUTPUT_LIMIT - stdout.len());
        Ok(CommandOutput {
            exit_code: status.code().unwrap_or(-1),
            truncated: out.1 || err.1 || stdout.len() + stderr.len() < out.0.len() + err.0.len(),
            stdout,
            stderr,
        })
    };
    let result = tokio::time::timeout(timeout, read)
        .await
        .map_err(|_| InvestigateError::CommandTimeout)?;
    if result.is_ok() {
        kill.disarm();
    }
    result
}

struct OwnedDir(PathBuf);
impl OwnedDir {
    fn create(root: &Path, purpose: &str) -> Result<Self, InvestigateError> {
        let path = root.join(format!("{purpose}-{}-{}", std::process::id(), unique()));
        std::fs::create_dir_all(&path).map_err(|_| {
            InvestigateError::Unavailable("could not create investigation directory".into())
        })?;
        Ok(Self(path))
    }
}
impl Drop for OwnedDir {
    fn drop(&mut self) {
        if std::fs::remove_dir_all(&self.0).is_err() {
            tracing::warn!("investigate: owned directory cleanup failed");
        }
    }
}
fn unique() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

struct CheckoutGuard {
    mgr: Arc<rhapsody_workspace::Manager>,
    repo: String,
    run_id: i64,
}
impl Drop for CheckoutGuard {
    fn drop(&mut self) {
        let mgr = self.mgr.clone();
        let repo = self.repo.clone();
        let id = self.run_id;
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                if mgr.remove_investigate_export(&repo, id).await.is_err() {
                    tracing::warn!(run = id, "investigate: PR export cleanup failed");
                }
            });
        }
    }
}

struct InvestigationSession {
    container: Session,
    head: String,
    _checkout: CheckoutGuard,
}
impl InvestigationSession {
    async fn destroy(self) {
        self.container.destroy().await;
    }
}
struct RunSlot {
    active: AtomicBool,
    stop: tokio::sync::Notify,
    session: tokio::sync::Mutex<Option<InvestigationSession>>,
}
impl Default for RunSlot {
    fn default() -> Self {
        Self {
            active: AtomicBool::new(true),
            stop: tokio::sync::Notify::new(),
            session: tokio::sync::Mutex::new(None),
        }
    }
}

/// Shared off-loop runtime. Control only binds/releases ids; Docker, git and async locks belong
/// to request tasks. A released slot cannot create another session.
pub struct Investigations {
    exec: Arc<dyn Executor>,
    image: Result<String, InvestigateError>,
    root: PathBuf,
    runs: Mutex<HashMap<i64, Arc<RunSlot>>>,
    warmups: tokio::sync::Mutex<HashMap<String, String>>,
}

impl Investigations {
    pub async fn boot(root: PathBuf) -> Arc<Self> {
        let root = root.join("investigate");
        let exec: Arc<dyn Executor> = Arc::new(Docker {
            config: root.join("docker-config"),
            // Never load registry logins or credential helpers. Desktop/Linux expose this local
            // socket; an explicit Unix DOCKER_HOST also supports OrbStack. Remote hosts are refused.
            host: std::env::var("DOCKER_HOST")
                .ok()
                .filter(|s| s.starts_with("unix://"))
                .unwrap_or_else(|| "unix:///var/run/docker.sock".into()),
        });
        Self::boot_with_executor(root, exec).await
    }

    async fn boot_with_executor(root: PathBuf, exec: Arc<dyn Executor>) -> Arc<Self> {
        let image = Self::boot_test(exec.clone(), &root).await;
        if let Err(reason) = &image {
            tracing::warn!(code = reason.code(), detail = %reason, "investigate unavailable; manager continues without the tool");
        }
        Arc::new(Self {
            exec,
            image,
            root,
            runs: Mutex::new(HashMap::new()),
            warmups: tokio::sync::Mutex::new(HashMap::new()),
        })
    }

    async fn boot_test(exec: Arc<dyn Executor>, root: &Path) -> Result<String, InvestigateError> {
        std::fs::create_dir_all(root.join("docker-config")).map_err(|_| {
            InvestigateError::Unavailable("could not create private Docker config".into())
        })?;
        let version = exec
            .run(
                strings(&["version", "--format", "{{.Server.Version}}"]),
                Duration::from_secs(15),
            )
            .await?;
        if version.exit_code != 0 {
            return Err(InvestigateError::DockerMissing);
        }
        let info = exec
            .run(
                strings(&["image", "inspect", "--format", "{{.Id}}", IMAGE]),
                Duration::from_secs(15),
            )
            .await?;
        if info.exit_code != 0 || !info.stdout.trim().starts_with("sha256:") {
            return Err(InvestigateError::Unavailable(format!(
                "build {IMAGE} from docker/investigate/Dockerfile"
            )));
        }
        let image = info.stdout.trim().to_string();
        let repo = OwnedDir::create(root, "selftest")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&repo.0, std::fs::Permissions::from_mode(0o777)).map_err(
                |_| InvestigateError::Unavailable("self-test directory permissions".into()),
            )?;
        }
        let name = format!("rhapsody-investigate-selftest-{}", unique());
        let cache = format!("{name}-cache");
        let _volume = VolumeGuard {
            exec: exec.clone(),
            name: cache.clone(),
            container: Some(name.clone()),
        };
        let volume = exec
            .run(
                strings(&["volume", "create", &cache]),
                Duration::from_secs(15),
            )
            .await?;
        if volume.exit_code != 0 {
            return Err(InvestigateError::Unavailable(
                "self-test cache creation failed".into(),
            ));
        }
        let guard = Session::guard(exec.clone(), name.clone());
        let result = selftest(
            exec.as_ref(),
            container_args(&name, &repo.0.to_string_lossy(), &cache, &image),
        )
        .await;
        guard.destroy().await;
        result?;
        Ok(image)
    }

    pub fn bind_run(&self, id: i64) {
        if id > 0 {
            self.runs
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(id, Arc::new(RunSlot::default()));
        }
    }
    pub fn release_run(&self, id: i64) {
        if let Some(slot) = self
            .runs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&id)
        {
            slot.active.store(false, Ordering::Release);
            slot.stop.notify_one();
            tokio::spawn(async move {
                if let Some(session) = slot.session.lock().await.take() {
                    session.destroy().await;
                }
            });
        }
    }

    async fn investigate(
        &self,
        id: i64,
        mgr: Arc<rhapsody_workspace::Manager>,
        repo: &str,
        head: &str,
        cmd: &str,
    ) -> Result<CommandOutput, InvestigateError> {
        let image = self.image.clone()?;
        let slot = self
            .runs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&id)
            .cloned()
            .ok_or(InvestigateError::SessionEnded)?;
        let work = async {
            let mut session = slot.session.lock().await;
            if !slot.active.load(Ordering::Acquire) {
                return Err(InvestigateError::SessionEnded);
            }
            if session.is_none() {
                // A dropped request must not abandon an in-progress PR export. The bounded
                // task owns its result guard; an unconsumed result drops and removes the tree.
                let checkout_mgr = mgr.clone();
                let checkout_repo = repo.to_string();
                let checkout_head = head.to_string();
                let provisioning = tokio::spawn(async move {
                    let ws = checkout_mgr
                        .ensure_investigate_export(&checkout_repo, id, &checkout_head)
                        .await
                        .map_err(|_| {
                            InvestigateError::Unavailable(
                                "PR head is not available in the cached mirror".into(),
                            )
                        })?;
                    let guard = CheckoutGuard {
                        mgr: checkout_mgr,
                        repo: checkout_repo,
                        run_id: id,
                    };
                    Ok::<_, InvestigateError>((ws, guard))
                });
                let (ws, checkout) = provisioning
                    .await
                    .map_err(|_| InvestigateError::Unavailable("PR export task failed".into()))??;
                let cache = self.warm_cache(&ws.path, &image).await?;
                let name = format!("rhapsody-investigate-{}-{id}", unique());
                let container =
                    Session::create(self.exec.clone(), name, &ws.path, &cache, &image).await?;
                let deadline = container.started + SESSION_TIMEOUT;
                *session = Some(InvestigationSession {
                    container,
                    head: head.into(),
                    _checkout: checkout,
                });
                let deadline_slot = slot.clone();
                tokio::spawn(async move {
                    tokio::time::sleep_until(deadline).await;
                    deadline_slot.active.store(false, Ordering::Release);
                    deadline_slot.stop.notify_one();
                    if let Some(session) = deadline_slot.session.lock().await.take() {
                        session.destroy().await;
                    }
                });
            }
            let current = session.as_ref().ok_or(InvestigateError::SessionEnded)?;
            if current.head != head {
                return Err(InvestigateError::InvalidRef);
            }
            let result = current.container.command(cmd).await;
            if result.is_err() {
                slot.active.store(false, Ordering::Release);
                session.take();
            }
            result
        };
        tokio::select! { result = work => result, _ = slot.stop.notified() => Err(InvestigateError::SessionEnded) }
    }

    async fn warm_cache(&self, checkout: &str, image: &str) -> Result<String, InvestigateError> {
        let inputs = OwnedDir::create(&self.root, "inputs")?;
        let fingerprint = metadata_inputs(Path::new(checkout), &inputs.0)?;
        let mut warmed = self.warmups.lock().await;
        if let Some(cache) = warmed.get(&fingerprint) {
            return Ok(cache.clone());
        }
        // A unique volume per attempt avoids admitting partial data from a failed prior warm-up.
        let cache = format!("rhapsody-investigate-cache-{fingerprint}-{}", unique());
        let created = self
            .exec
            .run(
                strings(&["volume", "create", &cache]),
                Duration::from_secs(15),
            )
            .await?;
        if created.exit_code != 0 {
            return Err(InvestigateError::Unavailable(
                "cache volume creation failed".into(),
            ));
        }
        let name = format!("rhapsody-investigate-warm-{}", unique());
        let guard = Session::guard(self.exec.clone(), name.clone());
        let mut failure_volume = VolumeGuard {
            exec: self.exec.clone(),
            name: cache.clone(),
            container: Some(name.clone()),
        };
        let started = self
            .exec
            .run(
                warmup_args(&name, &inputs.0.to_string_lossy(), &cache, image),
                Duration::from_secs(30),
            )
            .await?;
        if started.exit_code != 0 {
            return Err(InvestigateError::Unavailable(
                "cache warm-up could not start".into(),
            ));
        }
        let waited = self
            .exec
            .run(strings(&["wait", &name]), COMMAND_TIMEOUT)
            .await?;
        let removed = self
            .exec
            .run(strings(&["rm", "--force", &name]), Duration::from_secs(15))
            .await?;
        let mut guard = guard;
        guard.removed = removed.exit_code == 0;
        drop(guard);
        if waited.exit_code != 0 || waited.stdout.trim() != "0" || removed.exit_code != 0 {
            return Err(InvestigateError::Unavailable(
                "credential-free cache warm-up failed; commission network-dependent work".into(),
            ));
        }
        failure_volume.name.clear();
        warmed.insert(fingerprint, cache.clone());
        Ok(cache)
    }

    pub async fn shutdown(&self) {
        let slots: Vec<_> = self
            .runs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .drain()
            .map(|(_, slot)| slot)
            .collect();
        for slot in &slots {
            slot.active.store(false, Ordering::Release);
            slot.stop.notify_one();
        }
        for slot in slots {
            if let Some(session) = slot.session.lock().await.take() {
                session.destroy().await;
            }
        }
        let mut warmed = self.warmups.lock().await;
        for (_, cache) in warmed.drain() {
            if !self
                .exec
                .run(strings(&["volume", "rm", &cache]), Duration::from_secs(15))
                .await
                .is_ok_and(|o| o.exit_code == 0)
            {
                tracing::warn!("investigate: shutdown cache cleanup failed");
            }
        }
    }
}

struct VolumeGuard {
    exec: Arc<dyn Executor>,
    name: String,
    container: Option<String>,
}
impl Drop for VolumeGuard {
    fn drop(&mut self) {
        if self.name.is_empty() {
            return;
        }
        let exec = self.exec.clone();
        let name = self.name.clone();
        let container = self.container.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                // Cancellation drops guards in reverse order. Remove the owned warm-up first,
                // so volume removal cannot race a still-mounted cache.
                if let Some(container) = container {
                    let _ = exec
                        .run(
                            strings(&["rm", "--force", &container]),
                            Duration::from_secs(15),
                        )
                        .await;
                }
                if !exec
                    .run(strings(&["volume", "rm", &name]), Duration::from_secs(15))
                    .await
                    .is_ok_and(|o| o.exit_code == 0)
                {
                    tracing::warn!("investigate: cache cleanup failed");
                }
            });
        }
    }
}

fn metadata_inputs(checkout: &Path, destination: &Path) -> Result<String, InvestigateError> {
    use sha2::{Digest, Sha256};
    let mut pending = vec![PathBuf::new()];
    let mut files = Vec::new();
    let mut visited = 0;
    while let Some(relative) = pending.pop() {
        for entry in std::fs::read_dir(checkout.join(&relative))
            .map_err(|_| InvestigateError::Unavailable("checkout metadata unreadable".into()))?
        {
            visited += 1;
            if visited > 20000 {
                return Err(InvestigateError::Unavailable(
                    "checkout metadata exceeds bound".into(),
                ));
            }
            let entry = entry
                .map_err(|_| InvestigateError::Unavailable("checkout entry unreadable".into()))?;
            let kind = entry
                .file_type()
                .map_err(|_| InvestigateError::Unavailable("checkout entry stat failed".into()))?;
            let name = entry.file_name();
            let path = relative.join(&name);
            if kind.is_dir() && name != ".git" {
                pending.push(path);
            } else if kind.is_file()
                && matches!(
                    name.to_str(),
                    Some("Cargo.toml" | "Cargo.lock" | "package.json" | "package-lock.json")
                )
            {
                files.push(path);
            }
        }
    }
    files.sort();
    let mut digest = Sha256::new();
    let mut size = 0;
    for path in files {
        let full = checkout.join(&path);
        let len = std::fs::metadata(&full)
            .map_err(|_| InvestigateError::Unavailable("metadata stat failed".into()))?
            .len();
        size += len;
        if len > 2 * 1024 * 1024 || size > 8 * 1024 * 1024 {
            return Err(InvestigateError::Unavailable(
                "dependency metadata exceeds bound".into(),
            ));
        }
        let bytes = std::fs::read(&full)
            .map_err(|_| InvestigateError::Unavailable("metadata read failed".into()))?;
        digest.update(path.to_string_lossy().as_bytes());
        digest.update([0]);
        digest.update(&bytes);
        digest.update([0]);
        let target = destination.join(&path);
        let parent = target.parent().ok_or(InvestigateError::InvalidRef)?;
        std::fs::create_dir_all(parent)
            .and_then(|_| std::fs::write(&target, &bytes))
            .map_err(|_| InvestigateError::Unavailable("metadata copy failed".into()))?;
        if path.file_name().is_some_and(|n| n == "Cargo.toml") {
            cargo_target_stubs(&bytes, parent)?;
        }
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn cargo_target_stubs(bytes: &[u8], parent: &Path) -> Result<(), InvestigateError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| InvestigateError::Unavailable("Cargo manifest is not UTF-8".into()))?;
    let manifest: toml::Value = toml::from_str(text)
        .map_err(|_| InvestigateError::Unavailable("Cargo manifest is invalid".into()))?;
    if manifest.get("package").is_none() {
        return Ok(());
    }
    let mut paths = vec!["src/lib.rs", "src/main.rs"];
    if let Some(path) = manifest
        .get("lib")
        .and_then(|l| l.get("path"))
        .and_then(|p| p.as_str())
    {
        paths.push(path);
    }
    for kind in ["bin", "example", "test", "bench"] {
        if let Some(targets) = manifest.get(kind).and_then(|t| t.as_array()) {
            for target in targets {
                if let Some(path) = target.get("path").and_then(|p| p.as_str()) {
                    paths.push(path);
                }
            }
        }
    }
    for path in paths {
        if Path::new(path)
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return Err(InvestigateError::Unavailable(
                "Cargo target escapes package".into(),
            ));
        }
        let target = parent.join(path);
        let dir = target.parent().ok_or(InvestigateError::InvalidRef)?;
        std::fs::create_dir_all(dir)
            .and_then(|_| std::fs::write(target, b""))
            .map_err(|_| InvestigateError::Unavailable("Cargo target stub failed".into()))?;
    }
    Ok(())
}

impl crate::ControlHandle {
    pub async fn investigate(
        &self,
        run_id: i64,
        head: &str,
        cmd: &str,
    ) -> Result<CommandOutput, InvestigateError> {
        let runtime = self.investigate.as_ref().ok_or_else(|| {
            InvestigateError::Unavailable("investigate is disabled on this daemon".into())
        })?;
        runtime.image.clone()?;
        if !matches!(head.len(), 40 | 64) || !head.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(InvestigateError::InvalidRef);
        }
        let coord = self
            .manager_coordinate_for(run_id)
            .await
            .map_err(|_| InvestigateError::SessionEnded)?;
        let pr = self
            .manager_pr(run_id)
            .await
            .map_err(|_| InvestigateError::Unavailable("PR head could not be verified".into()))?;
        if pr.get("head").and_then(|s| s.as_str()) != Some(head) {
            return Err(InvestigateError::InvalidRef);
        }
        let mgr = self
            .manager_workspace()
            .await
            .map_err(|_| InvestigateError::Unavailable("workspace is unavailable".into()))?;
        runtime
            .investigate(run_id, mgr, &coord.repo_url, head, cmd)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Fake {
        calls: Mutex<Vec<Vec<String>>>,
        mutation: Mutex<String>,
        lost: Mutex<bool>,
        state: Mutex<String>,
    }
    #[async_trait::async_trait]
    impl Executor for Fake {
        async fn run(
            &self,
            args: Vec<String>,
            _: Duration,
        ) -> Result<CommandOutput, InvestigateError> {
            self.calls.lock().unwrap().push(args.clone());
            if *self.lost.lock().unwrap() {
                return Err(InvestigateError::DockerMissing);
            }
            let cmd = args.last().cloned().unwrap_or_default();
            let mutation = self.mutation.lock().unwrap().clone();
            let escaped = (mutation == "network" && cmd.contains("network"))
                || (mutation == "home" && cmd.contains("rhapsody"))
                || (mutation == "rootfs" && cmd.contains("var/tmp"))
                || (mutation == "env" && cmd.contains("TOKEN"));
            let is_refusal = cmd.contains("rhapsody")
                || cmd.contains("network")
                || cmd.contains("TOKEN")
                || cmd.contains("touch /repo")
                || cmd.contains("var/tmp")
                || cmd.contains(".ssh")
                || cmd.contains("opencode");
            Ok(CommandOutput {
                exit_code: cmd
                    .strip_prefix("exit ")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or_else(|| i32::from(is_refusal && !escaped)),
                stdout: if args[0] == "inspect" {
                    let state = self.state.lock().unwrap();
                    if state.is_empty() {
                        "true false".into()
                    } else {
                        state.clone()
                    }
                } else {
                    String::new()
                },
                ..Default::default()
            })
        }
    }

    #[tokio::test]
    async fn investigate_accepts_the_normalized_manager_pr_head() {
        use rhapsody_store::{RunStart, Sqlite, Store, StorePath};

        let root = crate::testsupport::TempDir::new();
        let head = "a".repeat(40);
        let gh_head = head.clone();
        let gh = crate::ghsummons::GH::new(
            "@symphony",
            Some(Box::new(move |args| {
                assert_eq!(&args[..6], &["pr", "view", "1", "--repo", "o/r", "--json"]);
                Ok(serde_json::to_vec(&serde_json::json!({ "headRefOid": gh_head })).unwrap())
            })),
        );
        let store = Arc::new(Sqlite::open(StorePath::InMemory).unwrap());
        let id = store
            .start_run(RunStart {
                issue_identifier: "pr:o/r#1@manager".into(),
                repo: "git@github.com:o/r.git".into(),
                ..Default::default()
            })
            .unwrap();
        let mgr = Arc::new(
            rhapsody_workspace::Manager::new(rhapsody_workspace::Config {
                root: root.path.clone(),
                hooks: Default::default(),
                hook_timeout: Duration::from_secs(1),
            })
            .unwrap(),
        );
        let exec = Arc::new(Fake::default());
        let runtime = Arc::new(Investigations {
            exec: exec.clone(),
            image: Ok("pinned".into()),
            root: PathBuf::from(&root.path),
            runs: Mutex::new(HashMap::new()),
            warmups: tokio::sync::Mutex::new(HashMap::new()),
        });
        runtime.bind_run(id);
        // An existing session lets the production tool path execute without Docker or a checkout.
        let slot = runtime.runs.lock().unwrap().get(&id).unwrap().clone();
        *slot.session.lock().await = Some(InvestigationSession {
            container: Session::guard(exec.clone(), "verified-head".into()),
            head: head.clone(),
            _checkout: CheckoutGuard {
                mgr: mgr.clone(),
                repo: String::new(),
                run_id: id,
            },
        });
        let mut orchestrator = crate::Orchestrator::new("unused");
        orchestrator.set_store(store);
        let mut handle = orchestrator.control();
        handle.manager_gh = Some(Arc::new(gh));
        handle.investigate = Some(runtime.clone());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        handle.events = tx;
        tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                if let crate::Event::WorkspaceGc { reply } = event {
                    let _ = reply.send(crate::workspace_gc::WorkspaceGcPlan {
                        mgr: Some(mgr.clone()),
                        keep: Default::default(),
                    });
                }
            }
        });

        let pr = handle.manager_pr(id).await.unwrap();
        assert_eq!(pr["head"], head);
        assert!(pr.get("headRefOid").is_none());
        let result = handle.investigate(id, &head, "exit 42").await;
        assert!(result.is_ok(), "verified PR head rejected: {result:?}");
        assert_eq!(result.unwrap().exit_code, 42);
        assert!(
            exec.calls
                .lock()
                .unwrap()
                .contains(&exec_args("verified-head", "exit 42"))
        );

        exec.calls.lock().unwrap().clear();
        assert_eq!(
            handle
                .investigate(id, &"b".repeat(40), "exit 43")
                .await
                .unwrap_err(),
            InvestigateError::InvalidRef
        );
        assert!(
            exec.calls.lock().unwrap().is_empty(),
            "unverified head executed a command"
        );
        runtime.release_run(id);
        tokio::task::yield_now().await;
    }

    #[test]
    fn container_flags_exact() {
        let args = container_args("session", "/owned/tree", "cache", "sha256:pinned");
        assert_eq!(
            args,
            [
                "run",
                "--rm",
                "--detach",
                "--name",
                "session",
                "--pull",
                "never",
                "--log-driver",
                "none",
                "--network",
                "none",
                "--read-only",
                "--cap-drop",
                "ALL",
                "--security-opt",
                "no-new-privileges",
                "--user",
                "1000:1000",
                "--pids-limit",
                "512",
                "--cpus",
                "2",
                "--memory",
                "4g",
                "--tmpfs",
                "/scratch:rw,nosuid,nodev,exec,size=1g,mode=1777",
                "--mount",
                "type=bind,src=/owned/tree,dst=/repo,readonly",
                "--mount",
                "type=volume,src=cache,dst=/cache,readonly",
                "--workdir",
                "/repo",
                "--env",
                "PATH=/opt/rust/bin:/usr/local/bin:/usr/bin:/bin",
                "--env",
                "HOME=/scratch",
                "sha256:pinned",
                "env",
                "-i",
                "PATH=/opt/rust/bin:/usr/local/bin:/usr/bin:/bin",
                "HOME=/scratch",
                "sleep",
                "1800"
            ]
        );
    }

    #[test]
    fn no_host_paths_mounted() {
        let args = container_args("s", "/owned/tree", "cache", "image");
        let mounts: Vec<_> = args
            .windows(2)
            .filter(|a| a[0] == "--mount")
            .map(|a| &a[1])
            .collect();
        assert_eq!(mounts.len(), 2);
        assert!(mounts.iter().all(|m| m.ends_with(",readonly")));
        assert!(mounts.iter().all(|m| !m.contains(".ssh")
            && !m.contains(".rhapsody")
            && !m.contains("/home")
            && !m.contains("opencode")));
    }

    #[test]
    fn env_has_no_tokens() {
        let args = container_args("s", "/owned/tree", "cache", "image");
        let env: Vec<_> = args
            .windows(2)
            .filter(|a| a[0] == "--env")
            .map(|a| a[1].as_str())
            .collect();
        assert_eq!(env, [format!("PATH={PATH}"), "HOME=/scratch".into()]);
    }

    #[test]
    fn warmup_has_network_but_no_credentials() {
        let args = warmup_args("warm", "/owned/inputs", "cache", "image");
        assert!(args.windows(2).any(|a| a == ["--network", "bridge"]));
        assert!(args.iter().any(|a| a.contains("cargo fetch --locked")));
        assert!(args.iter().any(|a| a.contains("npm ci --ignore-scripts")));
        assert!(
            !args
                .iter()
                .any(|a| a.contains("dst=/repo") || a.contains("TOKEN=") || a.contains("KEY="))
        );
        assert!(
            args.iter()
                .any(|a| a == "type=bind,src=/owned/inputs,dst=/inputs,readonly")
        );
    }

    #[tokio::test]
    async fn limits_enforced() {
        let exec = Arc::new(Fake::default());
        let mut s = Session::create(exec.clone(), "s".into(), "/owned/tree", "c", "i")
            .await
            .unwrap();
        s.started = tokio::time::Instant::now() - SESSION_TIMEOUT;
        assert_eq!(
            s.command("true").await.unwrap_err(),
            InvestigateError::SessionEnded
        );
        assert_eq!(COMMAND_TIMEOUT, Duration::from_secs(600));
        assert_eq!(OUTPUT_LIMIT, 65536);
        let mut cmd = tokio::process::Command::new("/bin/sh");
        cmd.args(["-c", "exec sleep 1"]);
        assert_eq!(
            run_process(cmd, Duration::from_millis(20))
                .await
                .unwrap_err(),
            InvestigateError::CommandTimeout
        );
        let mut cmd = tokio::process::Command::new("/bin/sh");
        // Fixed-size output, not a load loop. Both streams must share the 64 KB response ceiling.
        cmd.args([
            "-c",
            "dd if=/dev/zero bs=70000 count=1 2>/dev/null; printf stderr >&2",
        ]);
        let out = run_process(cmd, Duration::from_secs(3)).await.unwrap();
        assert!(out.truncated);
        assert_eq!(out.stdout.len() + out.stderr.len(), 65536);
    }

    #[tokio::test]
    async fn ordinary_exit_codes_are_values_not_deadlines() {
        let exec = Arc::new(Fake::default());
        let session = Session::create(exec, "codes".into(), "/owned/tree", "cache", "image")
            .await
            .unwrap();
        for code in [124, 137] {
            assert_eq!(
                session
                    .command(&format!("exit {code}"))
                    .await
                    .unwrap()
                    .exit_code,
                code
            );
        }
    }

    #[tokio::test]
    async fn session_destroyed_at_run_end() {
        let exec = Arc::new(Fake::default());
        let s = Session::create(exec.clone(), "s".into(), "/owned/tree", "c", "i")
            .await
            .unwrap();
        drop(s);
        tokio::task::yield_now().await;
        assert!(
            exec.calls
                .lock()
                .unwrap()
                .iter()
                .any(|a| a == &["rm", "--force", "s"])
        );
        // Exercise the actual dispatch/exit seam too, without a Docker dependency in CI.
        let root = crate::testsupport::TempDir::new();
        let runtime = Arc::new(Investigations {
            exec: exec.clone(),
            image: Ok("pinned".into()),
            root: PathBuf::from(&root.path),
            runs: Mutex::new(HashMap::new()),
            warmups: tokio::sync::Mutex::new(HashMap::new()),
        });
        let mut orchestrator = crate::Orchestrator::new("unused");
        orchestrator.investigate = Some(runtime.clone());
        let mut entry = crate::testsupport::running_entry(
            rhapsody_core::Issue {
                identifier: "pr:o/r#1@manager".into(),
                ..Default::default()
            },
            "",
            "",
        );
        entry.run_id = 42;
        orchestrator.bind_teams_run(&entry);
        let slot = runtime.runs.lock().unwrap().get(&42).unwrap().clone();
        let mgr = Arc::new(
            rhapsody_workspace::Manager::new(rhapsody_workspace::Config {
                root: root.path.clone(),
                hooks: Default::default(),
                hook_timeout: Duration::from_secs(1),
            })
            .unwrap(),
        );
        *slot.session.lock().await = Some(InvestigationSession {
            container: Session::guard(exec.clone(), "run-42".into()),
            head: "a".repeat(40),
            _checkout: CheckoutGuard {
                mgr,
                repo: String::new(),
                run_id: 42,
            },
        });
        orchestrator.release_teams_run(&entry);
        tokio::task::yield_now().await;
        assert!(!slot.active.load(Ordering::Acquire));
        assert!(!runtime.runs.lock().unwrap().contains_key(&42));
        assert!(
            exec.calls
                .lock()
                .unwrap()
                .iter()
                .any(|a| a == &["rm", "--force", "run-42"])
        );
    }

    #[tokio::test]
    async fn docker_missing_is_typed_and_lead_continues() {
        let _serial = crate::testsupport::TRACING_TEST_LOCK.lock().await;
        let (events, subscriber) = crate::testsupport::recording_subscriber();
        let _guard = tracing::subscriber::set_default(subscriber);
        tracing::callsite::rebuild_interest_cache();
        let root = crate::testsupport::TempDir::new();
        let exec = Arc::new(Fake::default());
        *exec.lost.lock().unwrap() = true;
        let runtime =
            Investigations::boot_with_executor(PathBuf::from(&root.path), exec.clone()).await;
        assert_eq!(
            runtime.image.as_ref().unwrap_err(),
            &InvestigateError::DockerMissing
        );
        assert!(
            events
                .lock()
                .unwrap()
                .iter()
                .any(|e| e.level == "WARN"
                    && e.message.contains("manager continues without the tool"))
        );
        assert_eq!(
            Session::create(exec, "s".into(), "/owned/tree", "c", "i")
                .await
                .err(),
            Some(InvestigateError::DockerMissing)
        );
        // The tool's failure must not enter the manager CLI gate or disable manager authority.
        assert!(!crate::managerrun::manager_live_prompt("").is_empty());
    }

    #[tokio::test]
    async fn docker_vanishes_mid_session_is_typed() {
        let exec = Arc::new(Fake::default());
        let s = Session::create(exec.clone(), "s".into(), "/owned/tree", "c", "i")
            .await
            .unwrap();
        *exec.lost.lock().unwrap() = true;
        assert_eq!(
            s.command("true").await.unwrap_err(),
            InvestigateError::SessionLost
        );
        *exec.lost.lock().unwrap() = false;
        *exec.state.lock().unwrap() = "false true".into();
        assert_eq!(
            s.command("true").await.unwrap_err(),
            InvestigateError::SessionLost,
            "OOM must be a typed tool failure"
        );
    }

    #[tokio::test]
    async fn selftest_sandbox_refusals() {
        let exec = Fake::default();
        selftest(&exec, container_args("s", "/owned/tree", "cache", "image"))
            .await
            .unwrap();
        let calls = exec.calls.lock().unwrap();
        for probe in [
            ".rhapsody",
            ".ssh",
            "opencode",
            "network",
            "/repo",
            "/var/tmp",
            "TOKEN",
            "cargo --version",
            "rg --version",
        ] {
            assert!(
                calls
                    .iter()
                    .any(|a| a.last().is_some_and(|s| s.contains(probe))),
                "unexercised {probe}"
            );
        }
    }

    async fn mutant_red(kind: &str) {
        let exec = Fake::default();
        *exec.mutation.lock().unwrap() = kind.into();
        assert!(
            selftest(&exec, container_args("s", "/owned/tree", "cache", "image"))
                .await
                .is_err(),
            "self-test admitted {kind} escape"
        );
    }
    #[tokio::test]
    async fn mutation_remove_network_none() {
        mutant_red("network").await;
    }
    #[tokio::test]
    async fn mutation_add_home_mount() {
        mutant_red("home").await;
    }
    #[tokio::test]
    async fn mutation_drop_read_only() {
        mutant_red("rootfs").await;
    }
    #[tokio::test]
    async fn mutation_leak_token() {
        mutant_red("env").await;
    }

    #[test]
    fn warmup_metadata_excludes_sources_configs_and_symlinks() {
        let root = crate::testsupport::TempDir::new();
        let tree = Path::new(&root.path).join("tree");
        let inputs = Path::new(&root.path).join("inputs");
        std::fs::create_dir_all(tree.join("src")).unwrap();
        std::fs::create_dir_all(&inputs).unwrap();
        std::fs::write(
            tree.join("Cargo.toml"),
            "[package]\nname='sandbox-test'\nversion='0.1.0'\n[lib]\npath='custom/lib.rs'\n",
        )
        .unwrap();
        std::fs::write(tree.join("src/main.rs"), "source secret").unwrap();
        std::fs::write(tree.join(".npmrc"), "synthetic credential").unwrap();
        std::os::unix::fs::symlink("/nonexistent/operator-home", tree.join("home")).unwrap();
        std::os::unix::fs::symlink(".npmrc", tree.join("package.json")).unwrap();
        let key = metadata_inputs(&tree, &inputs).unwrap();
        assert_eq!(key.len(), 64);
        assert!(inputs.join("Cargo.toml").exists());
        assert_eq!(std::fs::read(inputs.join("custom/lib.rs")).unwrap(), b"");
        assert_eq!(std::fs::read(inputs.join("src/main.rs")).unwrap(), b"");
        for excluded in [".npmrc", "home", "package.json"] {
            assert!(!inputs.join(excluded).exists());
        }
        std::fs::write(
            tree.join("Cargo.toml"),
            "[package]\nname='bad'\nversion='0.1.0'\n[lib]\npath='../outside'\n",
        )
        .unwrap();
        assert!(metadata_inputs(&tree, &inputs).is_err());
    }

    #[tokio::test]
    #[ignore = "requires the pinned investigation image and local Docker; no operator credentials"]
    async fn real_docker_sandbox_refusals_and_mutations() {
        let root = crate::testsupport::TempDir::new();
        let exec: Arc<dyn Executor> = Arc::new(Docker {
            config: Path::new(&root.path).join("docker-config"),
            host: std::env::var("DOCKER_HOST")
                .unwrap_or_else(|_| "unix:///var/run/docker.sock".into()),
        });
        let image = Investigations::boot_test(exec.clone(), Path::new(&root.path))
            .await
            .unwrap();
        let repo = OwnedDir::create(Path::new(&root.path), "repo").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&repo.0, std::fs::Permissions::from_mode(0o777)).unwrap();
        let cache = format!("rhapsody-investigate-mutations-{}", unique());
        assert_eq!(
            exec.run(
                strings(&["volume", "create", &cache]),
                Duration::from_secs(15)
            )
            .await
            .unwrap()
            .exit_code,
            0
        );
        let _cache = VolumeGuard {
            exec: exec.clone(),
            name: cache.clone(),
            container: None,
        };
        let synthetic_home = OwnedDir::create(Path::new(&root.path), "synthetic-home").unwrap();
        // This is a synthetic directory in the test root, never the operator's real home.
        std::fs::create_dir(synthetic_home.0.join(".rhapsody")).unwrap();
        for mutation in ["network", "home", "rootfs", "env"] {
            let name = format!("rhapsody-investigate-mutation-{mutation}-{}", unique());
            let mut args = container_args(&name, &repo.0.to_string_lossy(), &cache, &image);
            match mutation {
                "network" => {
                    let index = args.iter().position(|s| s == "--network").unwrap();
                    args.drain(index..index + 2);
                }
                "home" => {
                    args.splice(
                        1..1,
                        strings(&[
                            "--mount",
                            &format!(
                                "type=bind,src={},dst=/host-home,readonly",
                                synthetic_home.0.display()
                            ),
                        ]),
                    );
                }
                "rootfs" => args.retain(|s| s != "--read-only"),
                "env" => {
                    args.splice(
                        1..1,
                        strings(&["--env", "CANARY_TOKEN=synthetic-nonsecret"]),
                    );
                }
                _ => unreachable!(),
            }
            let guard = Session::guard(exec.clone(), name.clone());
            let verdict = selftest(exec.as_ref(), args).await;
            assert!(verdict.is_err(), "{mutation} escape was admitted");
            println!("mutation {mutation}: {}", verdict.unwrap_err());
            // A refused extra mount never starts a container; rm's not-found is then expected.
            let _ = exec
                .run(strings(&["rm", "--force", &name]), Duration::from_secs(15))
                .await;
            drop(guard);
        }
        println!("real Docker self-test passed, immutable image {image}");
    }

    #[tokio::test]
    #[ignore = "requires pinned Docker image and public registries; credential-free metadata only"]
    async fn real_docker_warmup_and_offline_build() {
        let root = crate::testsupport::TempDir::new();
        // Fixture-only diagnostics: attach to the warm-up's public-registry output. Production
        // suppresses that output because arbitrary dependency metadata may contain a secret URL.
        struct Diagnostics(Arc<dyn Executor>);
        #[async_trait::async_trait]
        impl Executor for Diagnostics {
            async fn run(
                &self,
                mut args: Vec<String>,
                timeout: Duration,
            ) -> Result<CommandOutput, InvestigateError> {
                let warm = args.last().is_some_and(|s| s == WARMUP);
                if warm {
                    args.retain(|s| s != "--detach");
                }
                let out = self.0.run(args, timeout).await?;
                if warm {
                    println!("fixture warm-up output: {} {}", out.stdout, out.stderr);
                }
                Ok(out)
            }
        }
        let exec: Arc<dyn Executor> = Arc::new(Diagnostics(Arc::new(Docker {
            config: Path::new(&root.path).join("docker-config"),
            host: std::env::var("DOCKER_HOST")
                .unwrap_or_else(|_| "unix:///var/run/docker.sock".into()),
        })));
        let image = Investigations::boot_test(exec.clone(), Path::new(&root.path))
            .await
            .unwrap();
        let repo = OwnedDir::create(Path::new(&root.path), "repo").unwrap();
        std::fs::create_dir(repo.0.join("src")).unwrap();
        std::fs::write(repo.0.join("Cargo.toml"), "[package]\nname='investigate-canary'\nversion='0.1.0'\nedition='2021'\n[dependencies]\nitoa='=1.0.18'\n").unwrap();
        std::fs::write(repo.0.join("src/main.rs"), "fn main() { let mut b = itoa::Buffer::new(); assert_eq!(b.format(1135), \"1135\"); }\n").unwrap();
        let lock: toml::Value = toml::from_str(include_str!("../../../Cargo.lock")).unwrap();
        let itoa = lock["package"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"].as_str() == Some("itoa"))
            .unwrap();
        let checksum = itoa["checksum"].as_str().unwrap();
        std::fs::write(repo.0.join("Cargo.lock"), format!("version = 4\n[[package]]\nname='investigate-canary'\nversion='0.1.0'\ndependencies=['itoa']\n[[package]]\nname='itoa'\nversion='1.0.18'\nsource='registry+https://github.com/rust-lang/crates.io-index'\nchecksum='{checksum}'\n")).unwrap();
        std::fs::write(repo.0.join("package.json"), r#"{"name":"investigate-canary","version":"1.0.0","scripts":{"postinstall":"exit 42"}}"#).unwrap();
        std::fs::write(repo.0.join("package-lock.json"), r#"{"name":"investigate-canary","version":"1.0.0","lockfileVersion":3,"packages":{"":{"name":"investigate-canary","version":"1.0.0"}}}"#).unwrap();
        // These synthetic traps must never enter /inputs.
        std::fs::write(repo.0.join(".npmrc"), "registry=http://127.0.0.1:9\n").unwrap();
        std::fs::write(repo.0.join(".env"), "CANARY_TOKEN=synthetic-nonsecret\n").unwrap();
        let runtime = Investigations {
            exec: exec.clone(),
            image: Ok(image.clone()),
            root: PathBuf::from(&root.path),
            runs: Mutex::new(HashMap::new()),
            warmups: tokio::sync::Mutex::new(HashMap::new()),
        };
        let start = std::time::Instant::now();
        let cache = runtime
            .warm_cache(&repo.0.to_string_lossy(), &image)
            .await
            .unwrap();
        println!(
            "credential-free cargo fetch/npm ci --ignore-scripts warm-up: {:.2}s",
            start.elapsed().as_secs_f64()
        );
        let _cache = VolumeGuard {
            exec: exec.clone(),
            name: cache.clone(),
            container: None,
        };
        let name = format!("rhapsody-investigate-build-{}", unique());
        let session = Session::create(
            exec.clone(),
            name.clone(),
            &repo.0.to_string_lossy(),
            &cache,
            &image,
        )
        .await
        .unwrap();
        let out = session.command("cp -R /repo /scratch/project && cd /scratch/project && TMPDIR=/scratch cargo run --locked --offline && rg investigate Cargo.toml && ! touch /cache/write-probe && ! env | cut -d= -f1 | rg '(_TOKEN|_KEY)$'").await.unwrap();
        assert_eq!(out.exit_code, 0, "{} {}", out.stdout, out.stderr);
        for code in [124, 137] {
            assert_eq!(
                session
                    .command(&format!("exit {code}"))
                    .await
                    .unwrap()
                    .exit_code,
                code
            );
        }
        println!(
            "offline build/grep/read-only cache proof: {} {}",
            out.stdout, out.stderr
        );
        assert_eq!(
            exec.run(strings(&["rm", "--force", &name]), Duration::from_secs(15))
                .await
                .unwrap()
                .exit_code,
            0
        );
        drop(session);
    }
}
