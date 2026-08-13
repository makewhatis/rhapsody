//! build — captures this binary's build identity so a running daemon can report which commit it was
//! built from (STUDIO-380). Nothing in the Go reference corresponds to this: it exists because the
//! daemon reported `status: ok` regardless of age, so "Rhapsody is running" and "Rhapsody is current"
//! were indistinguishable from the outside and a month of drift went unnoticed.
//!
//! Emits three `rustc-env` values consumed by `src/build_info.rs`. Every probe is BEST-EFFORT: a
//! build outside a git checkout (a release tarball, a vendored source drop) must still compile, so a
//! failed probe yields the `unknown` sentinel rather than failing the build.
//!
//! Each value can be overridden by an environment variable of the same name, which is how a
//! reproducible or source-tarball build supplies an identity git cannot infer.

use std::process::Command;

/// The value reported when a probe cannot determine the real one. Deliberately not an empty string:
/// the dashboard renders this verbatim, and "unknown" is an honest answer where "" reads as a bug.
const UNKNOWN: &str = "unknown";

fn main() {
    // Re-run whenever the checkout moves to a different commit. `HEAD` covers a commit on the current
    // branch; `logs/HEAD` (the reflog) also ticks on checkout/rebase/reset. Both are resolved via
    // `git rev-parse --git-path`, which returns the correct location inside a linked WORKTREE — where
    // `.git` is a file, not a directory, so a hardcoded `../../.git/HEAD` would silently never fire.
    for path in ["HEAD", "logs/HEAD"] {
        if let Some(resolved) = git(&["rev-parse", "--git-path", path]) {
            println!("cargo:rerun-if-changed={resolved}");
        }
    }

    emit("RHAPSODY_BUILD_COMMIT", git(&["rev-parse", "HEAD"]));
    // `--tags` names the nearest release tag, `--always` degrades to a bare short SHA when no tag is
    // reachable (a shallow CI clone fetched without tags), and `--dirty` marks an uncommitted tree.
    // On a released commit this is `v0.3.1`; eight commits past it, `v0.3.1-8-g581e281` — which states
    // the drift this ticket is about directly, without the reader diffing two SHAs.
    emit(
        "RHAPSODY_BUILD_VERSION",
        git(&["describe", "--tags", "--always", "--dirty"]),
    );
    emit("RHAPSODY_BUILD_TIME", Some(build_timestamp()));
}

/// Publish `name` to the compiled crate, preferring an explicit environment override and falling back
/// to [`UNKNOWN`] when neither the override nor the probe produced a value.
fn emit(name: &str, probed: Option<String>) {
    println!("cargo:rerun-if-env-changed={name}");
    let value = std::env::var(name)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or(probed)
        .unwrap_or_else(|| UNKNOWN.to_string());
    println!("cargo:rustc-env={name}={value}");
}

/// Run `git` with `args` and return its trimmed stdout, or `None` if git is absent, this is not a
/// checkout, or the subcommand failed. Errors are values here — a build script that panicked on a
/// missing git would make the crate unbuildable outside a repository.
fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let value = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!value.is_empty()).then_some(value)
}

/// The build instant as RFC3339 UTC (`2026-08-13T16:30:00Z`), matching the timestamp format every
/// other field on the API uses. `SOURCE_DATE_EPOCH` is honored so a reproducible build can pin it.
fn build_timestamp() -> String {
    let epoch = std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .map(|secs| chrono::DateTime::from_timestamp(secs, 0))
        .unwrap_or_else(|| Some(chrono::Utc::now()));
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
    epoch
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_else(|| UNKNOWN.to_string())
}
