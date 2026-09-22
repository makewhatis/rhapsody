//! Pins the structural half of the STUDIO-981/P0c confused-deputy defense: the `rhapsodyd` binary
//! must not even be *capable* of a direct Keychain read, because it has no dependency that could
//! perform one. `desktop/src-tauri` depends on `keyring` (for the Linear token and, on the desktop
//! side, `provider_credential::ProviderCredentialOwner`); `rhapsodyd` and `rhapsody-credential-ipc`
//! must not. This is a stronger, cheaper, always-current complement to a runtime confused-deputy
//! test: a code review or a future PR cannot silently reintroduce a direct-Keychain code path
//! without this failing, even before anyone writes the call site.

use std::process::Command;

#[test]
fn rhapsodyd_has_no_keychain_capable_dependency() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("resolve repo root");

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
        "rhapsodyd must never depend on a Keychain-capable crate (P0c's direct-Keychain design \
         was rejected in favor of authenticated desktop-owned IPC — see \
         rhapsody_credential_ipc's crate doc for why); dependency tree:\n{tree}"
    );
}
