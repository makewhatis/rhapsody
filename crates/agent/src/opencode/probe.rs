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
//! * it passes only `--version`, which does not consult project or global config, and sets
//!   `stdin` to null;
//! * it bounds stdout and enforces a process-tree timeout;
//! * unknown, unparseable, or unreachable versions refuse with
//!   [`UNSUPPORTED_HARNESS_VERSION`] / [`PROBE_FAILED`] rather than being treated as compatible.
//!
//! Wiring this probe into preparation is a later slice (PB5); PB0 only defines and tests it.

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
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
fn run_bounded(command: &str, timeout: Duration) -> Result<String, ProbeError> {
    let mut child = Command::new(command)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env_clear()
        .envs(probe_env())
        // Its own process group, so the timeout's tree kill catches anything the probe spawned —
        // the same shape the agent runners use before arming `proctree::kill_tree`.
        .process_group(0)
        .spawn()
        .map_err(|e| ProbeError::Spawn {
            message: e.to_string(),
        })?;

    let pid = child.id();
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() >= deadline => {
                crate::proctree::kill_tree(pid);
                let _ = child.wait();
                return Err(ProbeError::TimedOut);
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(e) => {
                return Err(ProbeError::Spawn {
                    message: e.to_string(),
                });
            }
        }
    }

    let mut buf = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        // `take` bounds the read; a `--version` that printed more is already unparseable.
        let _ = stdout.take(MAX_PROBE_OUTPUT as u64).read_to_end(&mut buf);
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let err = probe_with_timeout(&command, Duration::from_millis(150)).unwrap_err();
        assert_eq!(err.reason(), PROBE_FAILED);
        assert_eq!(err, ProbeError::TimedOut);
    }
}
