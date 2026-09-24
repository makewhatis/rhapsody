//! doctor — the `rhapsodyd doctor` diagnostic subcommand (STUDIO-994, provider-auth P13).
//!
//! **Rhapsody-only** (no Go counterpart): the frozen reference has no provider registry to diagnose.
//! Dispatched at the very top of [`crate::run::run`] beside `mcp` and `teams`, so the daemon's
//! run-lock and flag parsing are untouched and `rhapsodyd <workflow>` behaves identically.
//!
//! The doctor answers the migration question `provider-auth-design.md` §7 raises — "is this install
//! still on a legacy auth spelling, and what is the new form?" — by running the SAME
//! `load → decode → resolve → validate` pipeline the daemon runs, then printing:
//!
//! * whether the workflow loaded (or defaults were used), and whether it validates;
//! * the resolved backend and provider/model selection;
//! * every configured provider with its canonical, non-secret credential binding;
//! * the compiled-in managed-OpenCode compatibility table;
//! * the [`rhapsody_config::legacy_warnings`] migration notes.
//!
//! It is **read-only**: §7 requires that config warnings "suggest the new form without rewriting the
//! operator's workflow automatically", so this command never writes the workflow or any sidecar. It
//! also never reads a credential — live per-provider credential *status* is served by a running
//! daemon (`GET /api/v1/providers`, desktop-owner mediated); the doctor reports the configuration
//! that status is derived from. Exit codes match `teams`: 0 on a diagnostic report, 1 on a load or
//! validation failure (or an argument error), because a mistyped command that exited 0 would look
//! like it had diagnosed something.

use std::fmt::Write as _;
use std::io::Write;
use std::path::Path;

use rhapsody_agent::harness::SUPPORTED_OPENCODE_VERSIONS;
use rhapsody_config::{Config, legacy_warnings, validate, workflow};

/// The usage line every argument error quotes.
const USAGE: &str = "usage: rhapsodyd doctor [WORKFLOW.md]";

/// Runs `rhapsodyd doctor [WORKFLOW.md]`, writing the report to `stdout` and any failure to `stderr`
/// behind the `symphony doctor:` marker (the dispatch-marker convention `run_mcp`/`run_teams`
/// established). Returns the process exit code.
pub fn run_doctor<O, E>(args: &[String], stdout: O, stderr: E) -> i32
where
    O: Write,
    E: Write,
{
    run_doctor_with(
        args,
        &|k| std::env::var(k).unwrap_or_default(),
        stdout,
        stderr,
    )
}

/// [`run_doctor`] with the environment injected, so the tests exercise the real stream/exit-code
/// contract against a hermetic temp workflow instead of mutating the process environment.
fn run_doctor_with<O, E>(
    args: &[String],
    getenv: &dyn Fn(&str) -> String,
    mut stdout: O,
    mut stderr: E,
) -> i32
where
    O: Write,
    E: Write,
{
    match doctor_command(args, getenv) {
        Ok((report, code)) => {
            let _ = write!(stdout, "{report}");
            code
        }
        Err(e) => {
            let _ = writeln!(stderr, "symphony doctor: {e}");
            1
        }
    }
}

/// Resolve the workflow and render the report, or return the actionable load failure. Factored out
/// of [`run_doctor`] so the report text is unit-testable without hijacking stdout. The second tuple
/// element is the exit code: 0 for a valid config, 1 for one the daemon's preflight would refuse —
/// the refusal reason is part of the report, not a silent failure.
fn doctor_command(
    args: &[String],
    getenv: &dyn Fn(&str) -> String,
) -> Result<(String, i32), String> {
    let path = workflow_path(args, getenv)?;
    let (mut config, source) = load_config(&path)?;
    // The daemon's preflight: refuses an unusable config with a byte-pinned reason. The doctor
    // reports that reason rather than guessing at a fix. `validate` trims project slugs in place —
    // the same in-memory normalization the daemon performs, never a file rewrite.
    let invalid = validate(&mut config).err().map(|e| e.to_string());
    let report = render(&config, &path, source, invalid.as_deref());
    Ok((report, i32::from(invalid.is_some())))
}

/// The workflow path: an explicit positional argument wins, then `SYMPHONY_WORKFLOW`, then
/// `WORKFLOW.md` in the current directory. A flag-looking argument is an error, not a path.
fn workflow_path(args: &[String], getenv: &dyn Fn(&str) -> String) -> Result<String, String> {
    let positional = args.iter().find(|a| !a.starts_with('-'));
    if args.iter().any(|a| a.starts_with('-')) {
        return Err(format!("unknown flag; {USAGE}"));
    }
    if let Some(p) = positional {
        return Ok(p.clone());
    }
    let env = getenv("SYMPHONY_WORKFLOW");
    Ok(if env.is_empty() {
        "WORKFLOW.md".to_string()
    } else {
        env
    })
}

/// Load + decode + resolve the workflow. A missing file falls back to a BLANK front matter run
/// through the same pipeline (exactly as `teams` and `run::resolve_boot_logdir` do), so an operator
/// can diagnose an install with no `WORKFLOW.md` in the current directory; any other load, decode,
/// or resolve failure is returned, never guessed around.
fn load_config(path: &str) -> Result<(Config, LoadSource), String> {
    let path = Path::new(path);
    let (def, source) = match workflow::load(path) {
        Ok(def) => (def, LoadSource::Loaded),
        Err(_) if !path.exists() => (
            workflow::Definition {
                config: workflow::YamlMap::new(),
                prompt_template: String::new(),
            },
            LoadSource::Defaults,
        ),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let config = rhapsody_config::decode(&def).map_err(|e| e.to_string())?;
    let resolved = rhapsody_config::resolve(config, &crate::bootcfg::workflow_dir(path))
        .map_err(|e| e.to_string())?;
    Ok((resolved, source))
}

/// Whether the report's config came from a file or from defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoadSource {
    Loaded,
    Defaults,
}

impl LoadSource {
    fn label(self) -> &'static str {
        match self {
            LoadSource::Loaded => "loaded",
            LoadSource::Defaults => "not found; using defaults",
        }
    }
}

/// Render the diagnostic report. Deterministic: providers iterate the canonical (sorted) map, so two
/// runs over one config print byte-identical text.
fn render(config: &Config, path: &str, source: LoadSource, invalid: Option<&str>) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "workflow: {} ({})", path, source.label());
    match invalid {
        None => {
            let _ = writeln!(out, "config: valid");
        }
        Some(reason) => {
            let _ = writeln!(out, "config: INVALID ({reason})");
        }
    }
    let _ = writeln!(out, "backend: {}", config.agent.backend);
    if config.agent.provider.is_empty() {
        let _ = writeln!(out, "selection: legacy (no agent.provider)");
    } else {
        let _ = writeln!(
            out,
            "selection: provider={} model={}",
            config.agent.provider, config.agent.model
        );
    }
    let _ = writeln!(out, "opencode: command={:?}", config.opencode.command);

    if config.providers.is_empty() {
        let _ = writeln!(out, "providers: none configured");
    } else {
        let _ = writeln!(out, "providers: {} configured", config.providers.len());
        for (id, def) in &config.providers {
            // A definition whose canonical binding cannot be derived is reported as such rather
            // than hidden; validation would normally have refused it before a dispatch.
            let binding = match def.credential_binding() {
                Ok(b) => format!("endpoint={} binding=ok", b.base_url),
                Err(e) => format!("binding=invalid ({e})"),
            };
            let _ = writeln!(
                out,
                "  - {id}: protocol={} credential={} insecure_http={} {binding}",
                def.protocol, def.credential.source, def.allow_insecure_http
            );
        }
    }

    let _ = writeln!(
        out,
        "harness compatibility: managed opencode versions = {}",
        supported_versions()
    );
    let _ = writeln!(
        out,
        "credential status: configuration only — live per-provider credential state is served by a \
         running daemon at GET /api/v1/providers"
    );

    let warnings = legacy_warnings(config);
    if warnings.is_empty() {
        let _ = writeln!(out, "warnings: none");
    } else {
        let _ = writeln!(out, "warnings: {}", warnings.len());
        for warning in warnings {
            let _ = writeln!(
                out,
                "  [{}] {}: {}",
                warning.code, warning.field, warning.message
            );
            let _ = writeln!(out, "      suggestion: {}", warning.suggestion);
        }
    }
    out
}

/// The compiled-in managed-OpenCode compatibility rows, rendered `version (adapter@version)`.
fn supported_versions() -> String {
    SUPPORTED_OPENCODE_VERSIONS
        .iter()
        .map(|row| {
            format!(
                "{} ({}@{})",
                row.opencode_version, row.adapter_package, row.adapter_version
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No workflow on disk and no `SYMPHONY_WORKFLOW`: the report falls back to defaults, says why
    /// the config is refused, and exits 1 — `doctor` works in an empty directory but does not pretend
    /// a blank config is dispatchable.
    #[test]
    fn a_missing_workflow_falls_back_to_defaults() {
        let (report, code) = doctor_command(&[], &|_| String::new()).expect("defaults load");
        assert!(report.contains("not found; using defaults"), "{report}");
        assert!(
            report.contains("config: INVALID"),
            "a blank config is refused by the daemon's preflight: {report}"
        );
        assert_eq!(code, 1);
    }

    /// A valid legacy OpenCode workflow reports the migration warnings and exits 0. The run is
    /// read-only, so the workflow bytes on disk are unchanged.
    #[test]
    fn a_legacy_workflow_reports_warnings_without_rewriting_it() {
        let dir = crate::testutil::TempDir::new();
        let path = dir.child("WORKFLOW.md");
        let body = "---\ntracker:\n  kind: file\n  source: /tmp/issues.json\nagent:\n  backend: \
                    opencode\nopencode:\n  auth_source: /tmp/auth.json\n---\nbody\n";
        std::fs::write(&path, body).expect("write workflow");
        let before = std::fs::read(&path).expect("read workflow");

        let (report, code) =
            doctor_command(&[path.to_string_lossy().into_owned()], &|_| String::new())
                .expect("legacy workflow loads");

        assert!(report.contains("config: valid"), "{report}");
        assert_eq!(code, 0);
        assert!(report.contains("legacy_opencode_auth_source"), "{report}");
        assert!(report.contains("legacy_opencode_backend"), "{report}");
        assert!(
            report.contains("GET /api/v1/providers"),
            "the report must point at the live credential-status surface: {report}"
        );
        assert_eq!(
            std::fs::read(&path).expect("re-read workflow"),
            before,
            "doctor must never rewrite the operator's workflow (design §7)"
        );
    }

    /// An explicit positional path wins over the environment, and an unknown flag is refused rather
    /// than silently treated as a path.
    #[test]
    fn the_path_argument_wins_and_a_flag_is_refused() {
        assert_eq!(
            workflow_path(&["x.md".to_string()], &|_| String::new()).unwrap(),
            "x.md"
        );
        assert_eq!(
            workflow_path(&[], &|_| "env.md".to_string()).unwrap(),
            "env.md",
            "SYMPHONY_WORKFLOW is the fallback"
        );
        assert!(workflow_path(&["--wat".to_string()], &|_| String::new()).is_err());
    }
}
