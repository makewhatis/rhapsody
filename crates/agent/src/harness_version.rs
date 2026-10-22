//! Harness CLI version gate (STUDIO-1468). Rhapsody-only; no Go counterpart.
//!
//! Two hard runtime requirements had no version check, so an upgrade could make EVERY run fail at
//! zero tokens and retry forever:
//!
//! * `crates/agent/src/claude/args.rs` adds `--include-partial-messages` to every turn (STUDIO-1285).
//!   A `claude` CLI older than the release that introduced the flag exits immediately on the
//!   unknown flag, so every turn dies before any work; the credential preflight still passes, so
//!   dispatch retries instead of holding.
//! * every OpenCode run drives `opencode serve --pure` against the pinned 1.18.30 HTTP API. Only the
//!   brokered path probed the version; any other OpenCode fails every turn with
//!   `opencode_control_failed` and loops on retries.
//!
//! [`check`] answers "can the installed CLI run this build's argv?" for one harness. The worker
//! calls it near the top of an attempt — after the pure capability validation, before any worktree
//! or child is created — and turns a refusal into the same terminal, no-retry hold a capability
//! refusal gets ([`CapabilityRefusal`]).
//!
//! The gate refuses only a version it actually READ and found unsupported. A CLI whose version
//! cannot be established is UNKNOWN, and — exactly like the credential preflight's `Unknown`
//! verdict — unknown permits dispatch: the defect being closed is an OLD CLI that exits on the
//! unknown flag, and an old CLI still answers `--version`. Failing closed on an unreadable version
//! would refuse every wrapper and test double that does not forward `--version`, which is not
//! evidence of an old CLI.

use std::time::Duration;

use crate::harness::{CapabilityRefusal, HarnessId};

/// The minimum `claude` CLI version that accepts `--include-partial-messages`.
///
/// Determined from the Claude Code changelog: `## 1.0.109` reads "SDK: Added partial message
/// streaming support via `--include-partial-messages` CLI flag" — the release that introduced the
/// flag [`crate::claude::build_args`] adds to every turn. `claude --version` prints the version
/// followed by a suffix (e.g. `2.1.307 (Claude Code)`), so [`parse_claude_version`] takes the
/// leading `MAJOR.MINOR.PATCH`.
pub const MIN_CLAUDE_VERSION: &str = "1.0.109";

/// How an operator upgrades a `claude` CLI below [`MIN_CLAUDE_VERSION`].
pub const CLAUDE_UPGRADE: &str =
    "upgrade Claude Code (`claude update`, or reinstall @anthropic-ai/claude-code)";

/// How an operator upgrades an unsupported `opencode` CLI. `SUPPORTED` is an EXACT pin, so a bare
/// `opencode upgrade` (which installs the latest release) can be refused again — the hint names the
/// supported version to install. [`tests::the_opencode_upgrade_hint_names_the_required_version`]
/// keeps it in step with the compatibility table.
pub const OPENCODE_UPGRADE: &str =
    "install the supported OpenCode version (`opencode upgrade 1.18.30`)";

/// The process-tree timeout for one version probe. `--version` answers in milliseconds; this is a
/// bound on a hung wrapper, not a production value anyone waits out.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// The supported OpenCode versions, rendered for the refusal message. Single-sourced from the PB0
/// compatibility table so the gate and the probe cannot disagree.
fn opencode_required() -> String {
    crate::opencode::probe::SUPPORTED
        .iter()
        .map(|row| row.opencode_version)
        .collect::<Vec<_>>()
        .join(", ")
}

/// The leading `MAJOR.MINOR.PATCH` of a `claude --version` line, if the first non-empty line starts
/// with one. Anything else (a pre-release, a partial version, no output) is `None`, which the caller
/// treats as an unknown version rather than a refusal.
pub fn parse_claude_version(output: &str) -> Option<String> {
    let line = output.lines().map(str::trim).find(|l| !l.is_empty())?;
    let token = line.split_whitespace().next()?;
    is_exact_version(token).then(|| token.to_string())
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

/// Whether `found` is at least `required`, comparing `MAJOR.MINOR.PATCH` numerically. A version that
/// does not parse is never "at least" anything.
pub fn version_at_least(found: &str, required: &str) -> bool {
    match (parse_components(found), parse_components(required)) {
        (Some(f), Some(r)) => f >= r,
        _ => false,
    }
}

fn parse_components(v: &str) -> Option<(u64, u64, u64)> {
    if !is_exact_version(v) {
        return None;
    }
    let mut parts = v.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    Some((major, minor, patch))
}

/// The typed refusal for an unsupported CLI version. Kept as a constructor so the variant's fields
/// stay in one place.
fn refusal(
    harness: HarnessId,
    found: String,
    required: String,
    upgrade: &str,
) -> CapabilityRefusal {
    CapabilityRefusal::CliVersionUnsupported {
        harness,
        found,
        required,
        upgrade: upgrade.to_string(),
    }
}

/// Runs `<command> --version` (the command split exactly as the runner splits it) bounded by a
/// process-tree timeout, returning its stdout. Inherits the daemon environment so a configured
/// command (`claude`, `opencode`, `npx opencode`) resolves on the real `PATH`.
fn version_output(command: &str) -> Result<String, String> {
    let (name, args) =
        crate::claude::split_command(command).map_err(|e| format!("invalid command: {e}"))?;
    crate::opencode::probe::run_version(&name, &args, PROBE_TIMEOUT).map_err(|e| e.message())
}

/// Whether the installed `claude` CLI accepts this build's per-turn argv. An unreadable version is
/// unknown and permits dispatch (see the module doc).
fn check_claude(command: &str) -> Result<(), CapabilityRefusal> {
    let unsupported = |found: String| {
        refusal(
            HarnessId::Claude,
            found,
            MIN_CLAUDE_VERSION.to_string(),
            CLAUDE_UPGRADE,
        )
    };
    let Ok(output) = version_output(command) else {
        return Ok(());
    };
    let Some(found) = parse_claude_version(&output) else {
        return Ok(());
    };
    if version_at_least(&found, MIN_CLAUDE_VERSION) {
        Ok(())
    } else {
        Err(unsupported(found))
    }
}

/// Whether the installed `opencode` CLI is inside the supported compatibility range. An unreadable
/// version is unknown and permits dispatch (see the module doc); the brokered path keeps its own
/// stricter probe.
fn check_opencode(command: &str) -> Result<(), CapabilityRefusal> {
    let unsupported = |found: String| {
        refusal(
            HarnessId::Opencode,
            found,
            opencode_required(),
            OPENCODE_UPGRADE,
        )
    };
    let Ok(output) = version_output(command) else {
        return Ok(());
    };
    let Ok(version) = crate::opencode::probe::parse_probe_output(&output) else {
        return Ok(());
    };
    match crate::opencode::probe::resolve_row(&version) {
        Ok(_) => Ok(()),
        Err(_) => Err(unsupported(version)),
    }
}

/// Refuses when the installed CLI for `harness` cannot run this build's argv. `Ok(())` means the
/// harness may dispatch.
pub fn check(harness: HarnessId, command: &str) -> Result<(), CapabilityRefusal> {
    match harness {
        HarnessId::Claude => check_claude(command),
        HarnessId::Opencode => check_opencode(command),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    /// A fake CLI script. Published executable, but every test runs it through `sh` so the probe
    /// never execs the script directly — dodging the Linux `ETXTBSY` window a sibling test's `fork`
    /// opens, which a fail-open gate could not otherwise tell apart from an unreadable version.
    fn script(body: &str) -> (crate::opencode::testdir::TempDir, String) {
        let dir = crate::opencode::testdir::TempDir::new();
        let path = dir.path().join("fake-cli.sh");
        let tmp = dir.path().join("fake-cli.sh.tmp");
        {
            let mut file = std::fs::File::create(&tmp).expect("create script");
            writeln!(file, "#!/bin/sh").expect("write shebang");
            write!(file, "{body}").expect("write body");
            file.sync_all().expect("flush script");
        }
        std::fs::rename(&tmp, &path).expect("publish script");
        let mut perms = std::fs::metadata(&path).expect("stat script").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).expect("mark script executable");
        let rendered = path.to_string_lossy().into_owned();
        (dir, format!("sh {rendered}"))
    }

    #[test]
    fn min_claude_version_is_the_release_that_added_the_flag() {
        assert_eq!(MIN_CLAUDE_VERSION, "1.0.109");
    }

    /// The upgrade hint must name the version it tells the operator to install: a bare
    /// `opencode upgrade` installs the latest release, which the exact pin refuses again. A change
    /// to the compatibility table that forgets this hint reds here.
    #[test]
    fn the_opencode_upgrade_hint_names_the_required_version() {
        let required = opencode_required();
        assert!(
            OPENCODE_UPGRADE.contains(&required),
            "the upgrade hint {OPENCODE_UPGRADE:?} must name the required version {required:?}"
        );
    }

    #[test]
    fn parse_reads_the_leading_semver_and_refuses_the_rest() {
        assert_eq!(
            parse_claude_version("2.1.307 (Claude Code)\n").as_deref(),
            Some("2.1.307")
        );
        assert_eq!(parse_claude_version("1.0.109").as_deref(), Some("1.0.109"));
        for bad in [
            "",
            "not a version",
            "1.2",
            "1.2.3.4",
            "v1.2.3",
            "1.2.3-beta.1",
        ] {
            assert_eq!(parse_claude_version(bad), None, "should refuse {bad:?}");
        }
    }

    #[test]
    fn version_at_least_compares_numerically_not_lexically() {
        assert!(version_at_least("1.0.109", "1.0.109"));
        assert!(version_at_least("2.1.307", "1.0.109"));
        assert!(version_at_least("1.0.110", "1.0.109"));
        assert!(!version_at_least("1.0.108", "1.0.109"));
        // Lexically "1.0.99" > "1.0.109" would be wrong; numeric comparison keeps it below.
        assert!(!version_at_least("1.0.99", "1.0.109"));
        assert!(!version_at_least("unknown", "1.0.109"));
    }

    /// STUDIO-1468 acceptance: an old claude CLI is refused, a supported one passes, and the refusal
    /// names the found version, the required version and how to upgrade. MUTATION: drop the compare
    /// in `check_claude` and the old-version case returns `Ok`, reding this.
    #[test]
    fn claude_check_refuses_an_old_cli_and_accepts_a_supported_one() {
        let (_old_dir, old) = script("echo '1.0.108 (Claude Code)'\n");
        let err = check(HarnessId::Claude, &old).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("1.0.108"), "{message}");
        assert!(message.contains(MIN_CLAUDE_VERSION), "{message}");
        assert!(message.contains("claude"), "{message}");
        assert!(
            message.contains("upgrade") || message.contains("update"),
            "{message}"
        );

        let (_ok_dir, ok) = script("echo '2.1.307 (Claude Code)'\n");
        assert!(check(HarnessId::Claude, &ok).is_ok());

        // The exact boundary release is supported.
        let (_min_dir, min) = script("echo '1.0.109 (Claude Code)'\n");
        assert!(check(HarnessId::Claude, &min).is_ok());
    }

    /// A `claude` CLI whose version cannot be read is UNKNOWN, and unknown permits dispatch — the
    /// same policy the credential preflight applies. Only a version we READ and find too old refuses.
    #[test]
    fn claude_check_allows_an_unreadable_version() {
        let (_dir, bad) = script("echo 'not a version'\n");
        assert!(check(HarnessId::Claude, &bad).is_ok());
    }

    /// STUDIO-1468 acceptance for the second harness: an unsupported opencode is refused, the pinned
    /// version passes, and the refusal names both versions. MUTATION: drop the `resolve_row` check
    /// and the old-version case returns `Ok`.
    #[test]
    fn opencode_check_refuses_an_old_cli_and_accepts_the_pinned_one() {
        let (_old_dir, old) = script("echo 1.17.0\n");
        let err = check(HarnessId::Opencode, &old).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("1.17.0"), "{message}");
        assert!(message.contains("1.18.30"), "{message}");
        assert!(message.contains("opencode"), "{message}");

        let (_ok_dir, ok) = script("echo 1.18.30\n");
        assert!(check(HarnessId::Opencode, &ok).is_ok());
    }

    /// A configured command that carries base arguments (`npx opencode`) is split exactly as the
    /// runner splits it, so the version gate probes the same executable the run would.
    #[test]
    fn opencode_check_probes_a_command_that_carries_base_arguments() {
        let (_dir, command) = script("echo 1.18.30\n");
        assert!(check(HarnessId::Opencode, &command).is_ok());
    }
}
