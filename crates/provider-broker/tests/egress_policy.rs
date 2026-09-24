//! STUDIO-1003 (PB8) — the egress-policy pin for the one outbound client.
//!
//! Design §6.2 / §14.2: the fixed upstream client must not follow redirects, must ignore ambient
//! proxy environment variables, must be HTTP/1-only, must not transparently decompress, and must
//! refuse an invalid TLS certificate. Named mutation 3 ("weaken redirect/proxy/schema/… controls")
//! names proxy and TLS explicitly, so this file guards them.
//!
//! **Why a source pin and not a behavioral fixture.** The two controls mutation 3 names are both
//! disabled-by-convention rather than observable in this build:
//!
//! * `.no_proxy()` is load-bearing only if reqwest is compiled with its `system-proxy` feature.
//!   `crates/provider-broker/Cargo.toml` builds reqwest with `default-features = false, features =
//!   ["default-tls"]`, and every other reqwest 0.12 entry in the workspace does the same, so
//!   `system-proxy` is OFF today and no behavioral test can distinguish `.no_proxy()` present from
//!   absent. The explicit call is defense in depth against a future feature-unification that turns
//!   `system-proxy` on — exactly the hazard its own source comment names.
//! * `.danger_accept_invalid_certs(false)` restates reqwest's default. Observing it behaviorally
//!   needs a TLS fake upstream with an invalid certificate, which would add a TLS-server dependency
//!   to a crate whose test surface deliberately has none.
//!
//! So this is the repo's established source-scan shape (`tests/dependency_guard.rs`,
//! `crates/orchestrator/tests/custody_surface.rs`): it asserts the production builder still carries
//! each control, and it scans only the portion of `upstream.rs` BEFORE `#[cfg(test)]` so a unit test
//! naming the control cannot satisfy the pin for it.
//!
//! MUTATION GUARD: delete `.no_proxy()` or flip `danger_accept_invalid_certs` to accept invalid
//! certificates, and the corresponding needle below is absent → red.

/// `upstream.rs` up to its first `#[cfg(test)]`, i.e. only the production builder. Truncating here
/// is deliberate: an `include_str!` over the whole file would also pull in the file's own unit-test
/// module, and a test that happened to name a control would keep the pin green after the production
/// call was removed (the trap the review called out).
fn production_source() -> String {
    let source = include_str!("../src/upstream.rs");
    match source.split_once("#[cfg(test)]") {
        Some((production, _tests)) => production.to_string(),
        None => source.to_string(),
    }
}

/// Every control the fixed client's construction must keep. Each is the literal the production
/// builder uses; a rename must update both sides in one commit, which is the point.
const REQUIRED_CONTROLS: &[&str] = &[
    // Redirects are never followed; a redirect must not carry the credential elsewhere.
    ".redirect(reqwest::redirect::Policy::none())",
    // Ambient HTTP_PROXY/HTTPS_PROXY/system proxies are never inherited.
    ".no_proxy()",
    // HTTP/1 only for v1.
    ".http1_only()",
    // Decompression is explicitly off in every direction.
    ".no_gzip()",
    ".no_brotli()",
    ".no_deflate()",
    ".no_zstd()",
    // Platform trust roots with hostname validation; invalid certificates are refused.
    ".tls_built_in_root_certs(true)",
    ".danger_accept_invalid_certs(false)",
    // Bounded connect and response-progress timeouts (design §6.2); removing either lets a stalled
    // upstream hold the broker's forward future open past its deadline.
    ".connect_timeout(UPSTREAM_CONNECT_TIMEOUT)",
    ".read_timeout(UPSTREAM_READ_TIMEOUT)",
];

#[test]
fn the_outbound_client_keeps_every_fixed_egress_control() {
    let production = production_source();
    for control in REQUIRED_CONTROLS {
        assert!(
            production.contains(control),
            "the fixed outbound client lost an egress control (`{control}`); design §6.2/§14.2 \
             requires redirects, proxy, HTTP/1, decompression and invalid TLS to stay fixed"
        );
    }
}

/// The redirect policy in particular must be `none` (never `limited`/`default`), and the two
/// controls mutation 3 names must be present together — a bare presence check above could be
/// satisfied by one of the pair while the other regressed.
#[test]
fn redirect_and_tls_controls_are_the_refusing_forms() {
    let production = production_source();
    assert!(
        production.contains("redirect(reqwest::redirect::Policy::none())"),
        "redirects must be refused outright"
    );
    assert!(
        production.contains("danger_accept_invalid_certs(false)"),
        "invalid certificates must be refused"
    );
    assert!(
        production.contains("no_proxy()"),
        "ambient proxies must be ignored"
    );
}
