//! Managed-OpenCode compatibility probe (STUDIO-995). Rhapsody-only; no Go counterpart.
//!
//! Brokered OpenCode mode is version-gated because its security controls and request schema are
//! pinned behavior rather than a stable OpenCode API (`provider-broker-design.md` §9.1). V1
//! supports exactly the compatibility row recorded in [`SUPPORTED`]; the pinned request fixtures
//! under `harness/harness-spike/opencode/broker/` are the evidence for it, and
//! `tests/opencode_broker_fixture.rs` pins that evidence.
//!
//! The probe is deliberately tiny and hostile to ambient state:
//!
//! * it runs the resolved executable with `env_clear()` plus the small allow-list in
//!   [`probe_env`] — no credential, no auth/config content, no inherited `OPENCODE_*`;
//! * it passes only `--version`, which does not consult project or global config, sets `stdin` to
//!   null, and runs outside the target worktree with project/plugin discovery disabled;
//! * it drains stdout while it waits and enforces a process-tree timeout, killing the whole tree on
//!   every exit path so a leftover descendant can neither hold the pipe nor outlive the probe;
//! * unknown, unparseable, or unreachable versions refuse with
//!   [`UNSUPPORTED_HARNESS_VERSION`] / [`PROBE_FAILED`] rather than being treated as compatible.
//!
//! Wiring this probe into preparation is a later slice (PB5); PB0 only defines and tests it.

use std::io::{ErrorKind, Read};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

/// The error reason a caller records for an unknown or unparseable version.
pub const UNSUPPORTED_HARNESS_VERSION: &str = "unsupported_harness_version";
/// The error reason a caller records when the probe could not run at all.
pub const PROBE_FAILED: &str = "harness_probe_failed";

/// The default process-tree timeout for one probe.
pub const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Cap on how much of the probe's stdout is read. `--version` emits a few bytes; anything larger is
/// already unparseable, so the bound only prevents an unbounded read.
const MAX_PROBE_OUTPUT: usize = 4096;

/// The window the final drain gets after the child is reaped. The pipe is at EOF by then, so this
/// only bounds a descendant that escaped the sweep and keeps streaming; a fresh window (rather than
/// the probe deadline, which the tree kill may just have passed) is what lets the drain still read
/// the bytes the child wrote before it exited.
const FINAL_DRAIN_WINDOW: Duration = Duration::from_millis(250);

/// One measured OpenCode compatibility row. `adapter_version` is the `@ai-sdk/openai-compatible`
/// build bundled into `opencode_version`; both are pinned by the PB0 fixtures, and a row is only
/// accepted when the executable reports exactly `opencode_version`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompatibilityRow {
    pub opencode_version: &'static str,
    pub adapter_package: &'static str,
    pub adapter_version: &'static str,
}

/// Every OpenCode version the managed broker accepts. Exactly one row in v1; adding a version
/// requires rerunning PB0 and every managed-control fixture (`provider-broker-design.md` §9.1).
pub const SUPPORTED: &[CompatibilityRow] = &[CompatibilityRow {
    opencode_version: "1.18.30",
    adapter_package: "@ai-sdk/openai-compatible",
    adapter_version: "2.0.41",
}];

/// A typed refusal from the probe. Errors are values: the caller records [`ProbeError::reason`]
/// and its message, and never falls back to another version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeError {
    /// The executable ran and reported a version outside [`SUPPORTED`].
    UnsupportedVersion { found: String },
    /// The executable produced output this version of the probe cannot read as one exact version.
    UnparseableOutput,
    /// The executable could not be spawned.
    Spawn { message: String },
    /// The probe exceeded its process-tree timeout.
    TimedOut,
}

impl ProbeError {
    /// The bounded, actionable reason a caller records. Unknown/unparseable versions are
    /// [`UNSUPPORTED_HARNESS_VERSION`]; a probe that could not run is [`PROBE_FAILED`].
    pub fn reason(&self) -> &'static str {
        match self {
            ProbeError::UnsupportedVersion { .. } | ProbeError::UnparseableOutput => {
                UNSUPPORTED_HARNESS_VERSION
            }
            ProbeError::Spawn { .. } | ProbeError::TimedOut => PROBE_FAILED,
        }
    }

    /// A human-readable message. It never includes environment values or credentials.
    pub fn message(&self) -> String {
        match self {
            ProbeError::UnsupportedVersion { found } => {
                format!("unsupported opencode version {found:?}")
            }
            ProbeError::UnparseableOutput => "unparseable opencode version output".to_string(),
            ProbeError::Spawn { message } => format!("could not run the opencode probe: {message}"),
            ProbeError::TimedOut => "opencode version probe timed out".to_string(),
        }
    }
}

/// The minimal allow-listed environment one probe runs with. It deliberately excludes `HOME`, every
/// credential (`LINEAR_API_KEY`, `OPENCODE_AUTH_CONTENT`, `OPENCODE_CONFIG_CONTENT`, …), and the
/// discovery controls a probe must not consult.
pub fn probe_env() -> Vec<(&'static str, &'static str)> {
    vec![
        ("PATH", "/usr/bin:/bin"),
        ("OPENCODE_DISABLE_PROJECT_CONFIG", "1"),
        ("OPENCODE_DISABLE_EXTERNAL_SKILLS", "1"),
        ("OPENCODE_DISABLE_MODELS_FETCH", "1"),
        ("OPENCODE_DISABLE_AUTOUPDATE", "1"),
        ("OPENCODE_DISABLE_SHARE", "1"),
        // §9.1 disables plugin discovery too. Verified against the pinned binary's `RuntimeFlags`,
        // which maps `OPENCODE_DISABLE_DEFAULT_PLUGINS` onto `disableDefaultPlugins` (the default
        // plugin set); configured external plugins are suppressed by `--pure`, which `--version`
        // never reaches because it returns before plugin loading.
        ("OPENCODE_DISABLE_DEFAULT_PLUGINS", "1"),
    ]
}

/// Parses exactly one version line. Accepts a bare `1.18.30` (what `opencode --version` prints) or
/// the `opencode version: 1.18.30` form (`opencode debug info`); anything else — extra lines, a
/// range, a pre-release, a partial version — is [`ProbeError::UnparseableOutput`]. A wrapper that
/// cannot report one exact version is unsupported, not "probably fine".
pub fn parse_probe_output(output: &str) -> Result<String, ProbeError> {
    let mut found: Option<String> = None;
    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let candidate = line
            .strip_prefix("opencode version:")
            .map(str::trim)
            .unwrap_or(line);
        if !is_exact_version(candidate) || found.is_some() {
            return Err(ProbeError::UnparseableOutput);
        }
        found = Some(candidate.to_string());
    }
    found.ok_or(ProbeError::UnparseableOutput)
}
/// True only for `MAJOR.MINOR.PATCH` with numeric, non-empty components and no suffix.
fn is_exact_version(candidate: &str) -> bool {
    let mut parts = candidate.split('.');
    let mut count = 0;
    for part in parts.by_ref() {
        count += 1;
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
    }
    count == 3
}

/// Matches an exactly-equal OpenCode version against [`SUPPORTED`]. A near miss is not compatibility
/// evidence.
pub fn resolve_row(opencode_version: &str) -> Result<&'static CompatibilityRow, ProbeError> {
    SUPPORTED
        .iter()
        .find(|row| row.opencode_version == opencode_version)
        .ok_or_else(|| ProbeError::UnsupportedVersion {
            found: opencode_version.to_string(),
        })
}

/// Probes `command` with the default timeout and returns the matched compatibility row.
pub fn probe(command: &str) -> Result<&'static CompatibilityRow, ProbeError> {
    probe_with_timeout(command, DEFAULT_PROBE_TIMEOUT)
}

/// Probes `command` with an explicit timeout, so tests can exercise the bound without waiting for
/// the production value.
pub fn probe_with_timeout(
    command: &str,
    timeout: Duration,
) -> Result<&'static CompatibilityRow, ProbeError> {
    let output = run_bounded(command, timeout)?;
    let version = parse_probe_output(&output)?;
    resolve_row(&version)
}

/// Runs `command --version` under the allow-listed environment, bounded in bytes and by a
/// process-tree timeout. The child's stdin is null; stderr is discarded.
///
/// The bound covers the WHOLE probe, not just `try_wait` on the direct child. stdout is drained
/// while the child is waited on, so a `--version` larger than the pipe buffer cannot block the
/// child into a false timeout, and a background descendant that keeps stdout open after the child
/// exits cannot outlive the deadline: every exit path reaps the process tree, which closes the pipe
/// the descendant was holding. (A descendant that escaped the tree before the sweep is the one gap
/// [`crate::proctree`] documents; the drain is non-blocking, so even then the probe returns on
/// time rather than blocking on the read.)
fn run_bounded(command: &str, timeout: Duration) -> Result<String, ProbeError> {
    let mut child = Command::new(command)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env_clear()
        .envs(probe_env())
        // The probe runs OUTSIDE the target worktree (§9.1); a neutral cwd keeps a configured
        // wrapper from discovering a project even though `--version` never reads one itself.
        .current_dir("/")
        // Its own process group, so the tree kill catches anything the probe spawned — the same
        // shape the agent runners use before arming `proctree::kill_tree`.
        .process_group(0)
        .spawn()
        .map_err(|e| ProbeError::Spawn {
            message: e.to_string(),
        })?;

    let pid = child.id();
    let mut stdout = child.stdout.take().ok_or_else(|| ProbeError::Spawn {
        message: "the probe child has no piped stdout".to_string(),
    })?;
    set_nonblocking(&stdout)?;

    let deadline = Instant::now() + timeout;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut timed_out = false;
    loop {
        if drain(&mut stdout, &mut buf, &mut chunk, deadline) {
            timed_out = true;
            break;
        }
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() >= deadline => {
                timed_out = true;
                break;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(e) => {
                return Err(ProbeError::Spawn {
                    message: e.to_string(),
                });
            }
        }
    }

    // Reap the whole tree on EVERY exit path, success included: a descendant left holding stdout
    // would otherwise keep the pipe open (blocking the final drain) and go on running.
    crate::proctree::kill_tree(pid);
    let _ = child.wait();
    // The child is dead and the tree reaped, so the pipe reaches EOF once its buffered bytes are
    // read; the read is non-blocking, so an unreachable descendant cannot hang it.
    drain(
        &mut stdout,
        &mut buf,
        &mut chunk,
        Instant::now() + FINAL_DRAIN_WINDOW,
    );

    if timed_out {
        return Err(ProbeError::TimedOut);
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Puts the child's stdout pipe in non-blocking mode so a read never blocks past the deadline.
fn set_nonblocking(stdout: &ChildStdout) -> Result<(), ProbeError> {
    let fd = stdout.as_raw_fd();
    // SAFETY: `fd` is an open pipe read end; both `fcntl` forms take an int and touch no memory.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(ProbeError::Spawn {
            message: "fcntl(F_GETFL) on the probe pipe failed".to_string(),
        });
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(ProbeError::Spawn {
            message: "fcntl(F_SETFL, O_NONBLOCK) on the probe pipe failed".to_string(),
        });
    }
    Ok(())
}

/// Reads everything currently available into `buf`, keeping at most [`MAX_PROBE_OUTPUT`] bytes but
/// discarding the rest so a chatty `--version` cannot fill the pipe and block the child. Returns
/// `true` once `deadline` has passed, which is how an unending output is bounded.
fn drain(
    stdout: &mut ChildStdout,
    buf: &mut Vec<u8>,
    chunk: &mut [u8; 8192],
    deadline: Instant,
) -> bool {
    loop {
        if Instant::now() >= deadline {
            return true;
        }
        match stdout.read(chunk) {
            Ok(0) => return false, // EOF: every write end of the pipe is closed
            Ok(n) => {
                let room = MAX_PROBE_OUTPUT.saturating_sub(buf.len());
                if room > 0 {
                    buf.extend_from_slice(&chunk[..n.min(room)]);
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => return false,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Slack over the configured timeout for the timing assertions. It must swallow the ~200ms
    /// process-tree sweep plus scheduler noise on a loaded runner, yet stay well under the `sleep`
    /// a broken implementation would wait for (the fixtures use 5–30s), so the guard still reds.
    const TIMEOUT_SLACK: Duration = Duration::from_secs(2);

    /// Whether `pid` is still running. SIGKILL cannot be caught; signal 0 checks for existence only.
    fn process_alive(pid: i32) -> bool {
        // SAFETY: signal 0 delivers nothing, it only performs `kill(2)`'s error checking.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    fn script(body: &str) -> (crate::opencode::testdir::TempDir, String) {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        let dir = crate::opencode::testdir::TempDir::new();
        let path = dir.path().join("fake-opencode.sh");
        let mut file = std::fs::File::create(&path).expect("create script");
        writeln!(file, "#!/bin/sh").expect("write shebang");
        write!(file, "{body}").expect("write body");
        drop(file);
        let mut perms = std::fs::metadata(&path).expect("stat script").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).expect("mark script executable");
        let rendered = path.to_string_lossy().into_owned();
        (dir, rendered)
    }

    #[test]
    fn supported_table_pins_the_adapter_identity() {
        assert_eq!(SUPPORTED.len(), 1);
        let row = &SUPPORTED[0];
        assert_eq!(row.opencode_version, "1.18.30");
        assert_eq!(row.adapter_package, "@ai-sdk/openai-compatible");
        assert_eq!(row.adapter_version, "2.0.41");
    }

    #[test]
    fn parse_accepts_bare_and_prefixed_versions() {
        assert_eq!(parse_probe_output("1.18.30\n").unwrap(), "1.18.30");
        assert_eq!(
            parse_probe_output("opencode version: 1.18.30\n").unwrap(),
            "1.18.30"
        );
    }

    #[test]
    fn parse_refuses_unparseable_output() {
        for bad in [
            "",
            "not a version\n",
            "1.18\n",
            "1.18.30.1\n",
            "1.18.30-beta.1\n",
            "v1.18.30\n",
            "1.18.30\n2.0.0\n",
        ] {
            assert_eq!(
                parse_probe_output(bad),
                Err(ProbeError::UnparseableOutput),
                "should refuse {bad:?}"
            );
        }
    }

    #[test]
    fn resolve_refuses_unknown_and_near_versions() {
        assert_eq!(resolve_row("1.18.30").unwrap().adapter_version, "2.0.41");
        for unknown in ["1.18.31", "1.18.3", "1.19.0", "2.0.0", "1.18.30.0"] {
            let err = resolve_row(unknown).unwrap_err();
            assert_eq!(err.reason(), UNSUPPORTED_HARNESS_VERSION);
            assert!(matches!(err, ProbeError::UnsupportedVersion { .. }));
        }
    }

    #[test]
    fn probe_env_carries_no_credential_or_config_content() {
        let env = probe_env();
        let names: Vec<&str> = env.iter().map(|(k, _)| *k).collect();
        for forbidden in [
            "HOME",
            "LINEAR_API_KEY",
            "OPENCODE_AUTH_CONTENT",
            "OPENCODE_CONFIG_CONTENT",
            "OPENCODE_CONFIG",
            "OPENCODE_CONFIG_DIR",
            "XDG_DATA_HOME",
            "ANTHROPIC_API_KEY",
        ] {
            assert!(
                !names.contains(&forbidden),
                "probe env must not carry {forbidden}: {names:?}"
            );
        }
    }

    #[test]
    fn probe_accepts_the_pinned_binary() {
        let (_dir, command) = script("echo 1.18.30\n");
        let row = probe(&command).expect("pinned binary accepted");
        assert_eq!(row.opencode_version, "1.18.30");
        assert_eq!(row.adapter_version, "2.0.41");
    }

    #[test]
    fn probe_refuses_an_unknown_version() {
        let (_dir, command) = script("echo 9.9.9\n");
        let err = probe(&command).unwrap_err();
        assert_eq!(err.reason(), UNSUPPORTED_HARNESS_VERSION);
    }

    #[test]
    fn probe_refuses_unparseable_output() {
        let (_dir, command) = script("echo 'version: unknown'\n");
        let err = probe(&command).unwrap_err();
        assert_eq!(err.reason(), UNSUPPORTED_HARNESS_VERSION);
    }

    #[test]
    fn probe_refuses_a_missing_binary() {
        let err = probe("/nonexistent/pb0/opencode").unwrap_err();
        assert_eq!(err.reason(), PROBE_FAILED);
        assert!(matches!(err, ProbeError::Spawn { .. }));
    }

    #[test]
    fn probe_enforces_a_process_tree_timeout() {
        let (_dir, command) = script("sleep 5\n");
        let start = Instant::now();
        let err = probe_with_timeout(&command, Duration::from_millis(150)).unwrap_err();
        let elapsed = start.elapsed();
        assert_eq!(err.reason(), PROBE_FAILED);
        assert_eq!(err, ProbeError::TimedOut);
        // The assertion is on wall-clock time, not the variant: without the tree kill the probe
        // would simply `wait()` out the child's `sleep 5`.
        assert!(
            elapsed < Duration::from_millis(150) + TIMEOUT_SLACK,
            "the probe outran its deadline: {elapsed:?}"
        );
    }

    /// The timeout path must kill the process TREE, not wait the leader out. A descendant that the
    /// script left behind has to be gone, and the probe must still return on its deadline.
    #[test]
    fn probe_timeout_kills_a_descendant_instead_of_waiting_for_it() {
        let dir = crate::opencode::testdir::TempDir::new();
        let pidfile = dir.path().join("descendant.pid");
        let body = format!(
            "sleep 30 &\necho $! > {pidfile}\nsleep 30\n",
            pidfile = pidfile.display()
        );
        let (_script_dir, command) = script(&body);

        let start = Instant::now();
        let err = probe_with_timeout(&command, Duration::from_secs(3)).unwrap_err();
        let elapsed = start.elapsed();
        assert_eq!(err, ProbeError::TimedOut);
        // Without the tree kill, `sleep 30` outlives the 3s deadline by a wide margin.
        assert!(
            elapsed < Duration::from_secs(3) + TIMEOUT_SLACK,
            "the probe waited for the descendant: {elapsed:?}"
        );

        let pid: i32 = std::fs::read_to_string(&pidfile)
            .expect("the script recorded its descendant's pid")
            .trim()
            .parse()
            .expect("descendant pid");
        let deadline = Instant::now() + Duration::from_secs(3);
        while process_alive(pid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            !process_alive(pid),
            "descendant {pid} survived the probe's process-tree kill"
        );
    }

    /// B1 regression: a wrapper that prints one version line and exits while a background descendant
    /// keeps stdout open. The probe must read the version and stay bounded, and it must not leak the
    /// descendant — killing the tree closes the pipe the descendant held.
    #[test]
    fn probe_bounds_a_wrapper_whose_descendant_holds_stdout() {
        let (_dir, command) = script("sleep 30 &\necho 1.18.30\n");
        let start = Instant::now();
        let row = probe_with_timeout(&command, Duration::from_secs(3))
            .expect("the version line is still read from the bounded pipe");
        let elapsed = start.elapsed();
        assert_eq!(row.opencode_version, "1.18.30");
        // Without the tree kill the read blocks on the descendant's `sleep 30`, far past the bound.
        assert!(
            elapsed < Duration::from_secs(3) + TIMEOUT_SLACK,
            "a descendant holding stdout outran the deadline: {elapsed:?}"
        );
    }

    /// Output larger than one pipe buffer must be drained, not deadlocked: a `--version` that floods
    /// stdout is unparseable, but it has to be read (and bounded) rather than reported as a timeout.
    #[test]
    fn probe_drains_output_larger_than_the_pipe_buffer() {
        let (_dir, command) = script("yes 1.18.30 | head -c 200000\n");
        let start = Instant::now();
        let err = probe_with_timeout(&command, Duration::from_secs(5)).unwrap_err();
        let elapsed = start.elapsed();
        assert_eq!(err, ProbeError::UnparseableOutput);
        assert!(
            elapsed < Duration::from_secs(5),
            "the drain deadlocked: {elapsed:?}"
        );
    }

    /// `env_clear()` is the guard, not the allow-list: an ambient credential must not reach the
    /// probed child even though the test process holds it.
    #[tokio::test]
    async fn probe_child_cannot_see_an_ambient_credential() {
        let _env = crate::ENV_GUARD.write().await; // exclusive: mutates the process environment
        // SAFETY: the write lock excludes every reader, and the var is removed before the lock is
        // released.
        unsafe {
            std::env::set_var("PB0_AMBIENT_CREDENTIAL", "rhp-fake-ambient");
        }
        let (_dir, command) = script(
            "if [ -n \"${PB0_AMBIENT_CREDENTIAL:-}\" ]; then echo leaked; else echo 1.18.30; fi\n",
        );
        let result = probe(&command);
        unsafe {
            std::env::remove_var("PB0_AMBIENT_CREDENTIAL");
        }
        // A child that saw the credential prints `leaked`, which is not a version, so the probe
        // refuses and this unwrap fails.
        let row = result.expect("the probe child must not inherit ambient credentials");
        assert_eq!(row.opencode_version, "1.18.30");
    }
}
