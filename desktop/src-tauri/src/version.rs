//! Build stamp compiled into the desktop app (shown in the app footer). Mirrors Go
//! `$REF/desktop/internal/version` + the Makefile `-ldflags` pattern: the three values are
//! overridden at build time via the `RHAPSODY_VERSION` / `RHAPSODY_COMMIT` / `RHAPSODY_BUILD_TIME`
//! env vars the Makefile `app` target (P7-D5) sets, which the compile-time `option_env!`s below
//! read (build.rs marks them rerun-if-env-changed). The defaults identify an un-stamped / plain
//! `cargo build`, matching Go's plain-`go build` defaults ("dev" / "none" / "unknown").

use serde::Serialize;

// Raw compile-time stamp values (None unless the build set the env var). Kept separate from the
// defaulting below so the fallback logic is unit-testable without a stamped build.
const RAW_VERSION: Option<&str> = option_env!("RHAPSODY_VERSION");
const RAW_COMMIT: Option<&str> = option_env!("RHAPSODY_COMMIT");
const RAW_BUILD_TIME: Option<&str> = option_env!("RHAPSODY_BUILD_TIME");

/// The compiled-in build stamp shown in the app footer. Mirrors the Go `VersionDTO`
/// (`$REF/desktop/app.go`): the serde field names match its json tags (`version` / `commit` /
/// `build_time`) so the webview footer sees the identical shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VersionDto {
    pub version: String,
    pub commit: String,
    pub build_time: String,
}

/// The release version (e.g. "1.2.0"); "dev" for an unstamped build. Mirrors `version.Version`.
pub fn version() -> &'static str {
    or_default(RAW_VERSION, "dev")
}

/// The short git SHA the build was cut from (with a "-dirty" suffix when stamped from a dirty tree);
/// "none" when unstamped. Mirrors `version.Commit`.
pub fn commit() -> &'static str {
    or_default(RAW_COMMIT, "none")
}

/// The UTC build timestamp (RFC3339); "unknown" when unstamped. Mirrors `version.BuildTime`.
pub fn build_time() -> &'static str {
    or_default(RAW_BUILD_TIME, "unknown")
}

/// Returns the build stamp for the footer. Mirrors Go `App.AppVersion` (`$REF/desktop/app.go`).
pub fn dto() -> VersionDto {
    VersionDto {
        version: version().to_string(),
        commit: commit().to_string(),
        build_time: build_time().to_string(),
    }
}

/// Returns `v` when it is set and non-empty, else `default` — so an unset OR empty stamp env var
/// yields the documented default (matching Go's plain-build defaults).
fn or_default(v: Option<&'static str>, default: &'static str) -> &'static str {
    match v {
        Some(s) if !s.is_empty() => s,
        _ => default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn or_default_falls_back_when_unset() {
        assert_eq!(or_default(None, "dev"), "dev");
    }

    #[test]
    fn or_default_falls_back_when_empty() {
        assert_eq!(or_default(Some(""), "dev"), "dev");
    }

    #[test]
    fn or_default_keeps_a_set_value() {
        assert_eq!(or_default(Some("1.2.0"), "dev"), "1.2.0");
    }

    #[test]
    fn dto_mirrors_the_accessors() {
        let d = dto();
        assert_eq!(d.version, version());
        assert_eq!(d.commit, commit());
        assert_eq!(d.build_time, build_time());
    }

    #[test]
    fn accessors_are_never_empty() {
        assert!(!version().is_empty());
        assert!(!commit().is_empty());
        assert!(!build_time().is_empty());
    }
}
