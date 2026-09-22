//! Dependency-direction guard (PB1 acceptance: "crate has no dependency on
//! agent/orchestrator/httpapi/desktop"; explicit non-goal: "No HTTP/Axum").
//!
//! This asserts against the crate's own manifest so a future edit that reaches for an agent,
//! orchestrator, HTTP API, or desktop type reddens immediately, before the ownership boundary is
//! quietly broken.

const MANIFEST: &str = include_str!("../Cargo.toml");

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
        "axum",
        "hyper",
        "reqwest",
        "tokio",
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
