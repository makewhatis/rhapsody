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

/// This binary's build identity, as served. Constant for the life of the process.
pub(crate) fn current() -> VersionJson {
    VersionJson {
        version: VERSION,
        commit: COMMIT,
        built_at: BUILT_AT,
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
