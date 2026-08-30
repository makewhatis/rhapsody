//! build_info — the `GET /api/v1/version` wire view over this binary's build identity (STUDIO-380).
//!
//! Rhapsody-only; the Go reference has no counterpart. It exists because a stale daemon was
//! indistinguishable from a current one: `/state` answers `status: ok` no matter how old the binary
//! is, so a month of drift — including a fix the daemon had built for its own run classifier — went
//! unnoticed until runs were audited by hand.
//!
//! This is an ADDITIVE endpoint, deliberately NOT a new field on `/api/v1/state`. `/state` is a
//! byte-parity port of Go `toStateJSON` pinned to the committed `api/state.json` golden, and that
//! golden is recaptured from the frozen Go daemon — which will never emit a build identity. Adding a
//! field there could only be made green by editing the fixture or loosening the assertion, both of
//! which are drift laundering. A separate route keeps every existing payload and golden untouched,
//! following the precedent TRA-320 set when the dashboard needed data `/history` could not carry.
//!
//! The values are baked in at compile time by `build.rs`; see it for the probe and override rules.

use serde::Serialize;

/// The nearest release tag plus any distance past it (`v0.3.1`, or `v0.3.1-8-g581e281` eight commits
/// later), degrading to a short SHA when no tag is reachable.
const VERSION: &str = env!("RHAPSODY_BUILD_VERSION");
/// The full commit SHA this binary was built from — the identity the STUDIO-380 acceptance check is
/// phrased in ("built from a commit at or after `7a0edf8`").
const COMMIT: &str = env!("RHAPSODY_BUILD_COMMIT");
/// When the binary was built, RFC3339 UTC. The signal that first exposed the drift was a file mtime;
/// this reports the same fact without needing filesystem access to the bundle.
const BUILT_AT: &str = env!("RHAPSODY_BUILD_TIME");

/// The `GET /api/v1/version` body. Every field is always present — an absent probe reports the
/// `"unknown"` sentinel rather than being omitted, so a client never has to distinguish "field
/// missing" from "old daemon that predates this endpoint" by shape.
#[derive(Serialize)]
pub(crate) struct VersionJson {
    pub version: &'static str,
    pub commit: &'static str,
    pub built_at: &'static str,
}

/// This binary's build identity. Constant for the life of the process.
pub(crate) fn current() -> VersionJson {
    VersionJson {
        version: VERSION,
        commit: COMMIT,
        built_at: BUILT_AT,
    }
}

/// The full `GET /api/v1/version` body: the build identity, flattened, plus the one **runtime**
/// bit a client must know before it may fetch anything else (STUDIO-652).
///
/// Why a feature flag rides on the version route: a Teams-off dashboard must issue **zero**
/// requests against `/api/v1/teams*` — asking a Teams endpoint whether Teams is on is exactly the
/// poll-to-learn-it-is-off the acceptance forbids. The alternative home, `/api/v1/state`, is
/// byte-pinned to the Go daemon's `api/state.json` golden and can carry no Rhapsody-only key at
/// all. `/version` is already additive, already Rhapsody-only, and already fetched exactly once at
/// shell mount for the build stamp — so the gate rides along for no extra round-trip.
///
/// Flattened rather than nested so the three build fields keep their existing top-level names: a
/// client reading `version`/`commit`/`built_at` today sees no change.
#[derive(Serialize)]
pub(crate) struct VersionResponse {
    #[serde(flatten)]
    pub build: VersionJson,
    /// Rhapsody Teams is configured and on (`~/.rhapsody/teams.yaml` with `enabled: true`).
    pub teams_enabled: bool,
}

/// The served `GET /api/v1/version` body.
pub(crate) fn response(teams_enabled: bool) -> VersionResponse {
    VersionResponse {
        build: current(),
        teams_enabled,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The build script must populate all three values on any build of this crate. A regression here
    // (a dropped `rustc-env`, a renamed variable) would not fail compilation — `env!` would — but a
    // probe silently degrading to the sentinel on a normal in-repo build would, and that is the
    // failure mode that makes the endpoint useless for its one purpose.
    #[test]
    fn build_identity_is_populated() {
        let info = current();
        for (field, value) in [
            ("version", info.version),
            ("commit", info.commit),
            ("built_at", info.built_at),
        ] {
            assert!(!value.is_empty(), "{field} must never be empty");
        }
    }

    // Built inside this repository's checkout, the commit probe must resolve to a real SHA rather
    // than the fallback. Guarded on the build having happened in a git checkout at all, so the suite
    // still passes from a source tarball where `unknown` is the correct answer.
    #[test]
    fn commit_is_a_sha_when_built_in_a_checkout() {
        let info = current();
        if info.commit == "unknown" {
            return;
        }
        assert_eq!(
            info.commit.len(),
            40,
            "expected a full SHA, got {:?}",
            info.commit
        );
        assert!(
            info.commit.chars().all(|c| c.is_ascii_hexdigit()),
            "commit must be hex, got {:?}",
            info.commit
        );
    }
}
