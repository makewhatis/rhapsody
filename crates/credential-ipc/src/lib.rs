//! rhapsody-credential-ipc — the authenticated desktop-to-daemon credential channel selected by
//! STUDIO-981 (P0c). No Go reference: Symphony never had a provider-credential feature, so this
//! whole crate is a Rhapsody-only addition, not a parity port.
//!
//! ## Why this exists
//!
//! P0c measured, against real Developer-ID-signed binaries and a disposable Keychain item, that a
//! trusted-application ACL can silently exclude `/usr/bin/security` and an unsigned same-user
//! helper — but it cannot exclude a confused-deputy process that simply `exec`s the trusted signed
//! binary itself, because macOS's ACL keys on the CALLING PROCESS'S code identity at the moment of
//! the Keychain call, not on who launched it or with what authority. Since any coding-harness child
//! can already execute an arbitrary binary on disk as the same OS user (see
//! `provider-auth-p0-findings.md` §8), granting `rhapsodyd`'s own signed binary identity Keychain
//! trust would hand that same access to a rogue invocation of `rhapsodyd` a harness child launches
//! directly. Closing that gap needs an authenticated-launch primitive rhapsodyd does not have today
//! — and building that primitive is *also* sufficient, on its own, to hand the daemon credentials
//! without ever granting it direct Keychain access at all. So: the desktop app remains the sole
//! Keychain owner for provider credentials, and `rhapsodyd` receives bound leases only over this
//! authenticated channel.
//!
//! ## Shape
//!
//! - [`domain`] — the non-secret and secret-bearing types both sides share (`CredentialRef`,
//!   `Binding`, `Revision`, `CredentialState`, `BoundCredentialLease`).
//! - [`owner`] — the shared [`CredentialOwner`](owner::CredentialOwner) abstraction (P1): the one
//!   testable seam the desktop owner implements and the daemon consumes, carrying the typed
//!   mutation outcomes and configured/unconfigured status.
//! - [`bounds`] — the broker's size/syntax bounds, re-used from PB1 so the owner refuses an
//!   out-of-bounds credential before storage instead of duplicating the rule.
//! - [`wire`] — length-prefixed JSON framing and the request/response/push message shapes.
//! - [`session`] — the authentication + strictly-increasing-sequence state machine each connection
//!   is driven through; transport-free so its unauthorized/replayed/out-of-order/oversized
//!   guarantees are provable without any packaging (the packaged/signing evidence for the ownership
//!   *decision* itself was gathered separately, against real signed binaries — see the ticket's PR
//!   description and `provider-auth-p0c-findings.md`).
//! - [`token`] — the per-launch bootstrap token generator.
//!
//! This crate lives at `crates/credential-ipc` (a root-workspace member) precisely so it can be a
//! path dependency of BOTH `crates/rhapsodyd` (normal workspace membership) and
//! `desktop/src-tauri` (a path dependency reaching across into the excluded root workspace) — the
//! "shared crate/process boundary" P0c's acceptance contract requires naming. It carries no Tauri
//! dependency, so pulling it into `desktop/` does not reintroduce the heavy-dependency coupling the
//! root `Cargo.toml`'s workspace split exists to avoid.

pub mod bounds;
pub mod domain;
pub mod owner;
pub mod session;
pub mod token;
pub mod wire;

#[cfg(test)]
mod compile_guards {
    use static_assertions::assert_not_impl_any;

    use crate::domain::{BoundCredentialLease, CredentialState};

    // P1 mutation discipline: "Derive Debug/Serialize or add a String getter on the lease;
    // compile-time/API-shape and canary tests must fail." These negative-impl assertions fail to
    // compile if a future change adds `Clone`, `Serialize`, or `Display` to the lease or the state
    // enum. `Debug` stays allowed (it redacts), and is pinned by `domain`'s canary test.
    assert_not_impl_any!(BoundCredentialLease: Clone, serde::Serialize, serde::de::DeserializeOwned);
    assert_not_impl_any!(CredentialState: Clone, serde::Serialize, serde::de::DeserializeOwned, Copy);

    #[test]
    fn package_builds() {}
}
