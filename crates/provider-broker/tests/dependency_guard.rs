//! Dependency-direction guard (design §3.3: the broker depends on no agent, orchestrator, HTTP API,
//! config, store or desktop crate).
//!
//! PB1 carried no HTTP stack at all (the crate was protocol-neutral custody). PB2 owns the one
//! private loopback listener, so the HTTP crates (`hyper`/`hyper-util`/`axum`/`tokio`/`reqwest`) are
//! now expected dependencies and are deliberately absent from this list; the ownership boundary that
//! still matters — no `rhapsody-*` sibling and no desktop crate — is asserted both here and below.

const MANIFEST: &str = include_str!("../Cargo.toml");
/// PB2 source scanned to keep the forwarding entry point crate-private (design §5/§6, ticket
/// acceptance: "no generic forwarding primitive is public").
const UPSTREAM_SRC: &str = include_str!("../src/upstream.rs");
const LISTENER_SRC: &str = include_str!("../src/listener.rs");

#[test]
fn no_public_generic_forwarding_primitive() {
    assert!(
        UPSTREAM_SRC.contains("pub(crate) async fn forward_chat_completions"),
        "the forwarding entry point must exist and stay crate-private"
    );
    for needle in [
        "pub async fn forward_chat_completions",
        "pub fn forward(",
        "pub async fn forward(",
        "pub fn forward_request",
        "pub fn proxy",
    ] {
        assert!(
            !UPSTREAM_SRC.contains(needle) && !LISTENER_SRC.contains(needle),
            "a public generic forwarding primitive (`{needle}`) must not be added"
        );
    }
}

#[test]
fn forbidden_crate_dependencies_are_absent() {
    let forbidden = [
        "rhapsody-agent",
        "rhapsody-orchestrator",
        "rhapsody-httpapi",
        "rhapsodyd",
        "rhapsody-config",
        "rhapsody-store",
        "rhapsody-mcp",
        "rhapsody-workspace",
        "desktop",
        "tauri",
    ];
    for name in forbidden {
        assert!(
            !MANIFEST.contains(name),
            "the provider-broker manifest must not depend on `{name}`"
        );
    }
}

#[test]
fn the_crate_is_standalone_protocol_neutral() {
    // No dependency ENTRY may name a `rhapsody-*` crate: the security boundary must not be able to
    // reach the agent or the orchestrator through this crate. (The package's own `name =` line is
    // not a dependency entry.)
    let offenders: Vec<&str> = MANIFEST
        .lines()
        .filter(|line| line.trim_start().starts_with("rhapsody-"))
        .collect();
    assert!(
        offenders.is_empty(),
        "no rhapsody crate may be a dependency of the provider broker: {offenders:?}"
    );
}
