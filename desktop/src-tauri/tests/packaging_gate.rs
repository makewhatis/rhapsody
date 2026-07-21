//! Packaging gate — parity port of `$REF/desktop/internal/packaging/gate_test.go`.
//!
//! Pins the gating contract for the macOS packaging/signing helpers (`desktop/scripts`): the
//! Developer ID code-signing and notarization steps must be a clean no-op (exit 0, announcing they
//! skipped) when no Apple credentials are present, so an autonomous/unsigned build stays green — and
//! the hardened-runtime entitlements file must be a valid plist carrying the required key. It also
//! runs the sourceable-lib arg tests (`notarize_args_test.sh`). No signing ever happens here: the
//! gate variables are scrubbed from the child env, so an operator who exports them can't make these
//! tests attempt real signing. Runs in the `desktop` CI job's `cargo test`.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The `desktop/` directory (holds `scripts/` and `build/`). `CARGO_MANIFEST_DIR` is
/// `desktop/src-tauri`, so its parent is `desktop/`. Mirrors `gate_test.go`'s `symphonyDir` (which
/// walks up to the tree holding the scripts), specialized to this fixed layout.
fn desktop_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .canonicalize()
        .expect("resolve desktop dir")
}

/// The current environment minus the signing/notarization gate variables, so a developer who happens
/// to export `APPLE_SIGNING_IDENTITY` / `NOTARY_PROFILE` / `ASC_*` can't make these tests attempt real
/// signing. Mirrors `gate_test.go`'s `scrubbedEnv`, extended to the App Store Connect API-key vars
/// `notarize.sh` also reads.
fn scrubbed_env() -> Vec<(String, String)> {
    const GATE: [&str; 6] = [
        "APPLE_SIGNING_IDENTITY",
        "NOTARY_PROFILE",
        "ASC_KEY_ID",
        "ASC_ISSUER_ID",
        "ASC_API_KEY_P8",
        "ASC_API_KEY_P8_BASE64",
    ];
    std::env::vars()
        .filter(|(k, _)| !GATE.contains(&k.as_str()))
        .collect()
}

/// Runs a `desktop/scripts/<name>` helper via bash with the gate variables scrubbed, returning its
/// combined stdout+stderr and whether it exited 0. Mirrors `gate_test.go`'s `runScript`.
fn run_script(name: &str, args: &[&str]) -> (String, bool) {
    let script = desktop_dir().join("scripts").join(name);
    assert!(script.exists(), "script not found: {}", script.display());
    let out = Command::new("bash")
        .arg(&script)
        .args(args)
        .env_clear()
        .envs(scrubbed_env())
        .output()
        .expect("run bash script");
    let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
    combined.push_str(&String::from_utf8_lossy(&out.stderr));
    (combined, out.status.success())
}

// The core gate guarantee: with APPLE_SIGNING_IDENTITY unset, sign.sh exits 0 and announces it
// skipped — without touching the (here, absent) app. Mirrors TestSignScriptNoOpWithoutIdentity.
#[test]
fn sign_script_no_op_without_identity() {
    let (out, ok) = run_script(
        "sign.sh",
        &[
            "/nonexistent/Rhapsody.app",
            "/nonexistent/entitlements.plist",
        ],
    );
    assert!(
        ok,
        "sign.sh must exit 0 when APPLE_SIGNING_IDENTITY is unset; output:\n{out}"
    );
    assert!(
        out.to_lowercase().contains("skip"),
        "sign.sh should announce it skipped signing; output:\n{out}"
    );
}

// The same for notarize.sh + the notary credentials. Mirrors TestNotarizeScriptNoOpWithoutProfile.
#[test]
fn notarize_script_no_op_without_credentials() {
    let (out, ok) = run_script("notarize.sh", &["/nonexistent/Rhapsody.dmg"]);
    assert!(
        ok,
        "notarize.sh must exit 0 when no notary credentials are set; output:\n{out}"
    );
    assert!(
        out.to_lowercase().contains("skip"),
        "notarize.sh should announce it skipped notarization; output:\n{out}"
    );
}

// TRA-258: notarize.sh now also notarizes + staples the `.app` bundle (`_notarize_app`). The no-op
// gate must hold for that new arg kind too — no credentials means exit 0, before the bundle path
// ever touches the filesystem / ditto / xcrun — so `make dmg`'s _notarize_app stays green unsigned.
#[test]
fn notarize_script_no_op_without_credentials_app_bundle() {
    let (out, ok) = run_script("notarize.sh", &["/nonexistent/Rhapsody.app"]);
    assert!(
        ok,
        "notarize.sh must exit 0 for a .app when no notary credentials are set; output:\n{out}"
    );
    assert!(
        out.to_lowercase().contains("skip"),
        "notarize.sh should announce it skipped notarization for the .app; output:\n{out}"
    );
}

// The usage guards fire, so a misuse fails loudly rather than silently signing/skipping the wrong
// target. Mirrors TestSignScriptRequiresArgs.
#[test]
fn sign_script_requires_args() {
    let (out, ok) = run_script("sign.sh", &[]);
    assert!(
        !ok,
        "sign.sh with no args should fail (usage guard); output:\n{out}"
    );
}

// Mirrors TestNotarizeScriptRequiresArgs.
#[test]
fn notarize_script_requires_args() {
    let (out, ok) = run_script("notarize.sh", &[]);
    assert!(
        !ok,
        "notarize.sh with no args should fail (usage guard); output:\n{out}"
    );
}

// The hardened-runtime entitlements file exists, is a valid plist, and carries the required
// disable-library-validation key (so the signed app loads its separately-signed sidecar). Mirrors
// TestEntitlementsPlist.
#[test]
fn entitlements_plist_valid_and_has_key() {
    let path = desktop_dir().join("build/darwin/entitlements.plist");
    let data = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("entitlements.plist must exist at {}: {e}", path.display()));
    const KEY: &str = "com.apple.security.cs.disable-library-validation";
    assert!(
        data.contains(KEY),
        "entitlements.plist must contain {KEY:?} (so the hardened-runtime app loads the sidecar)"
    );
    // Authoritative validity check on macOS; skip if plutil is unavailable (the substring check
    // above still ran). Mirrors the Go test's `exec.LookPath("plutil")` guard.
    match Command::new("plutil").arg("-lint").arg(&path).output() {
        Ok(out) => assert!(
            out.status.success(),
            "plutil -lint rejected entitlements.plist:\n{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        ),
        Err(_) => eprintln!("plutil unavailable; skipped the authoritative plist lint"),
    }
}

// TRA-269: the ACL half of the window-drag path. `data-tauri-drag-region` (asserted present on the
// toolbar by web/src/components/Toolbar.test.tsx) invokes Tauri v2's internal `startDragging`
// command, which is gated by the `core:window:allow-start-dragging` permission. `core:default` pulls
// in `core:window:default`, whose 28 perms are all read/query ops — it does NOT include
// `allow-start-dragging` — so without an explicit grant the drag is silently denied by the ACL and
// the packaged window can't be moved by its title bar (maximize still works: it's a native overlay
// action outside the permission system). Pin the grant so a future reskin can't drop it again.
// (Tauri-desktop-only; no Go analogue.) We deliberately do NOT require `allow-start-resize-dragging`:
// under `titleBarStyle: "Overlay"` the window keeps its native decorations, so the OS handles resize.
#[test]
fn capabilities_grant_window_start_dragging() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("capabilities/default.json");
    let data = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "capabilities/default.json must exist at {}: {e}",
            path.display()
        )
    });
    let manifest: serde_json::Value =
        serde_json::from_str(&data).expect("capabilities/default.json must be valid JSON");
    let perms = manifest["permissions"]
        .as_array()
        .expect("capabilities/default.json must have a `permissions` array");
    const GRANT: &str = "core:window:allow-start-dragging";
    assert!(
        perms.iter().any(|p| p.as_str() == Some(GRANT)),
        "capabilities/default.json must grant {GRANT:?} so `data-tauri-drag-region` can drag the \
         window (Tauri v2 gates it behind this permission; core:default does not include it); \
         permissions = {perms:?}"
    );
}

// The notarize.sh sourceable-lib arg construction (dual credential modes, precedence, the loud
// partial-config error, base64 key decoding). Runs the ported shell test, which scrubs its own
// notary env and never touches xcrun/the network. Additive over the Go package (which unit-tests the
// same lib via its own shell harness).
#[test]
fn notarize_args_lib_contract() {
    let script = desktop_dir().join("scripts/notarize_args_test.sh");
    let out = Command::new("bash")
        .arg(&script)
        .env_clear()
        .envs(scrubbed_env())
        .output()
        .expect("run notarize_args_test.sh");
    assert!(
        out.status.success(),
        "notarize_args_test.sh failed:\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}

// The Homebrew cask renderer (`render-cask.sh`, TRA-241): the single-stable-channel cask text (version
// + sha256 substitution, the literal `#{version}` interpolation brew evaluates at install time, the
// zap stanza, and the deliberate simplifications vs the Go reference — no verified:/dist host/@channels)
// stays pinned by the ported pure-shell test. No network / Ruby / brew is touched. `render-cask.sh`
// authors the committed cask AND feeds release.yml's release-time auto-bump job.
#[test]
fn render_cask_lib_contract() {
    let script = desktop_dir().join("scripts/render_cask_test.sh");
    let out = Command::new("bash")
        .arg(&script)
        .env_clear()
        .envs(scrubbed_env())
        .output()
        .expect("run render_cask_test.sh");
    assert!(
        out.status.success(),
        "render_cask_test.sh failed:\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}

// The Tauri updater manifest renderer (`render-latest-json.sh`, TRA-261/P11-U2): the latest.json body
// tauri-plugin-updater consumes (version / notes / pub_date / platforms."darwin-aarch64".{signature,
// url}), input flow-through, and the fail-loud shape validation, pinned by the ported pure-shell test.
// No network / signing key is touched. `render-latest-json.sh` is release.yml's single source of truth
// for the manifest it uploads next to the notarized Rhapsody.app.tar.gz.
#[test]
fn render_latest_json_lib_contract() {
    let script = desktop_dir().join("scripts/render_latest_json_test.sh");
    let out = Command::new("bash")
        .arg(&script)
        .env_clear()
        .envs(scrubbed_env())
        .output()
        .expect("run render_latest_json_test.sh");
    assert!(
        out.status.success(),
        "render_latest_json_test.sh failed:\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}
