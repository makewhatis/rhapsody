//! Pins the structural half of the STUDIO-981/P0c confused-deputy defense: no source file under
//! `crates/` (which is what actually builds `rhapsodyd`) calls a Keychain read API. `desktop/
//! src-tauri` depends on `keyring` and calls it (for the Linear token and, on the desktop side,
//! `provider_credential::ProviderCredentialOwner`); nothing under `crates/` may.
//!
//! `rhapsodyd_has_no_keychain_crate_dependency` alone is NOT sufficient evidence of that: absence
//! of the `keyring` crate from `cargo tree -p rhapsodyd` does not mean the binary is *incapable* of
//! a direct Keychain read — `security-framework` (whose macOS `passwords` and
//! `os::macos::{keychain,passwords}` modules export Keychain read/write functions un-gated) is
//! already transitively present via `native-tls`'s TLS backend for `reqwest`. A future direct-
//! Keychain read through any of those modules would leave that test green.
//! `no_crates_source_calls_a_keychain_read_api` pins the property that actually matters: no source
//! file under `crates/` references `security_framework` (any path into that crate, including an
//! aliased `use`) or calls a `SecItem*`/`SecKeychain*`/`keyring::` API, checked directly against the
//! text of every `.rs` file, so a future call site fails this test even before its crate happens to
//! show up in `cargo tree`.

use std::path::Path;
use std::process::Command;

#[test]
fn rhapsodyd_has_no_keychain_crate_dependency() {
    let root = repo_root();

    let out = Command::new(env!("CARGO"))
        .args(["tree", "-p", "rhapsodyd", "--manifest-path"])
        .arg(root.join("Cargo.toml"))
        .output()
        .expect("run cargo tree");
    assert!(
        out.status.success(),
        "cargo tree -p rhapsodyd failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let tree = String::from_utf8_lossy(&out.stdout);
    assert!(
        !tree.to_lowercase().contains("keyring"),
        "rhapsodyd must never depend on the `keyring` crate directly (P0c's direct-Keychain design \
         was rejected in favor of authenticated desktop-owned IPC — see \
         rhapsody_credential_ipc's crate doc for why); dependency tree:\n{tree}"
    );
}

/// The real property this ticket relies on: source-level absence of any Keychain read API call
/// under `crates/`, independent of what the dependency graph happens to contain.
#[test]
fn no_crates_source_calls_a_keychain_read_api() {
    let root = repo_root();
    let crates_dir = root.join("crates");
    // Broad on purpose: the bare crate name `security_framework` (no trailing `::`) catches a
    // `use security_framework as kc;` alias too, not just a fully-qualified path — jimmy's review
    // of rhapsody#213 (N4 nit) found `security_framework::` alone misses that form.
    // `security_framework`'s Keychain-capable modules are `passwords` and
    // `os::macos::{keychain,passwords}` (an earlier needle naming only the top-level `passwords`
    // module missed the `os::macos` ones). `SecItem`/`SecKeychain` catch the C API family by prefix
    // rather than naming one function (the prior needle matched only `SecItemCopyMatching`, missing
    // e.g. `SecItemAdd`/`SecKeychainFindGenericPassword`). `keyring::` stays colon-qualified,
    // unlike `security_framework`: the bare word "keyring" already appears in unrelated prose
    // comments elsewhere under `crates/` (e.g. `orchestrator/src/prconditional.rs`, describing a
    // `gh auth` credential rotation), so broadening it the same way would false-positive on those.
    const NEEDLES: &[&str] = &["security_framework", "SecItem", "SecKeychain", "keyring::"];

    let mut offenders = Vec::new();
    let mut stack = vec![crates_dir];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("read_dir {dir:?}: {e}"));
        for entry in entries {
            let entry = entry.expect("dir entry");
            let path = entry.path();
            let file_type = entry.file_type().expect("file type");
            if file_type.is_dir() {
                // Skip build output; every crate's own `target/` would otherwise be walked too.
                if path.file_name().is_some_and(|n| n == "target") {
                    continue;
                }
                stack.push(path);
                continue;
            }
            // Skip this file itself: its own `NEEDLES` list necessarily contains the literal
            // strings it searches for, which would otherwise always self-match.
            if path.extension().is_some_and(|e| e == "rs")
                && path
                    .file_name()
                    .is_some_and(|n| n != "no_direct_keychain_dependency.rs")
            {
                let contents =
                    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
                for needle in NEEDLES {
                    if contents.contains(needle) {
                        offenders.push(format!("{}: contains {needle:?}", path.display()));
                    }
                }
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "a crates/ source file calls a Keychain read API, defeating the P0c confused-deputy \
         defense (rhapsodyd must reach a provider credential only through \
         rhapsody_credential_ipc's authenticated channel):\n{}",
        offenders.join("\n")
    );
}

fn repo_root() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("resolve repo root")
}
