//! STUDIO-991: the provider-secret command surface must reject any invocation that does not come
//! from the bundled `main` window at the `rhapsody://localhost` origin. This is the black-box half
//! of the check (the pure policy is also unit-tested inside `provider_commands`): it drives the SAME
//! public `authorize_invocation` function the Tauri command layer calls, so a weakened gate reds
//! here rather than only in an in-crate test.
//!
//! This is a plain integration test (no Tauri runtime): it exercises the invocation policy and the
//! capability/CSP shape, both of which are what actually keep the webview honest.

use rhapsody_desktop::provider_commands::authorize_invocation;

fn denied(label: &str, url: Option<&str>) {
    assert!(
        authorize_invocation(label, url).is_err(),
        "({label:?}, {url:?}) must be denied the provider-secret commands"
    );
}

#[test]
fn provider_commands_deny_a_non_main_window() {
    denied("settings", Some("rhapsody://localhost/"));
    denied("other", Some("rhapsody://localhost/"));
}

#[test]
fn provider_commands_deny_a_non_bundled_origin() {
    // A remote document, an internal Tauri scheme, a file URL, and a look-alike host are all denied.
    denied("main", Some("https://localhost/"));
    denied("main", Some("http://localhost/"));
    denied("main", Some("tauri://localhost/"));
    denied("main", Some("file:///etc/passwd"));
    denied("main", Some("rhapsody://localhost.evil.example/"));
    denied("main", Some("rhapsody://evil.example/"));
    denied("main", None);
}

#[test]
fn provider_commands_allow_only_the_bundled_main_origin() {
    assert!(authorize_invocation("main", Some("rhapsody://localhost/")).is_ok());
    assert!(authorize_invocation("main", Some("rhapsody://localhost")).is_ok());
}
