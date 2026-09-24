//! STUDIO-1003 (PB8) — the operator-documentation half of the "no overclaiming" gate.
//!
//! PB8 requires operator documentation to state the broker's EXACT guarantee and its residual
//! same-user risk, and forbids the shipped surfaces from overclaiming (the capability is not hidden;
//! there is no exact dollar guarantee for an arbitrary provider; generic usage is unverified). The
//! design's §1.2/§9.4/§17 hold the authoritative wording.
//!
//! This test pins the README statement POSITIVELY: each load-bearing claim must be present. Deleting
//! the section, or weakening a claim until its phrase disappears, reddens it. It deliberately does
//! NOT blocklist "hidden"/"dollar" substrings — the honest section NEGATES both ("there is no exact
//! dollar guarantee", "is NOT hidden"), so a substring scan would false-positive on the correct text.
//! The API half rides in `rhapsody-httpapi`'s provider-response scan and the UI half in the web
//! `console-trace-view` / `ProvidersTab` tests.

use std::path::Path;

#[test]
fn readme_states_the_broker_guarantee_and_its_residual_risk() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("resolve repo root");
    let readme = std::fs::read_to_string(root.join("README.md")).expect("read README.md");
    // Collapse whitespace first: the README hard-wraps prose, so a required phrase can straddle a
    // line break. Normalizing makes the phrase check about the words, not the wrapping.
    let flat = readme.split_whitespace().collect::<Vec<_>>().join(" ");

    // Each needle is a claim the design requires the operator docs to make. Keep them specific to a
    // claim, not generic English, so an accidental match cannot stand in for a deleted section.
    for needle in [
        // The guarantee: the reusable key is leased to the broker, never handed to the harness.
        "The reusable upstream provider key",
        "capability",
        // The capability is spendable within finite limits — never described as hidden.
        "spendable within finite limits",
        // Not a sandbox; same-user ambient access remains.
        "not a process sandbox",
        "same-user ambient access",
        // Transformed exfiltration defeats exact-secret redaction.
        "transformed exfiltration",
        // No exact dollar guarantee for arbitrary providers.
        "no exact dollar guarantee for arbitrary providers",
        // Generic usage is labelled unverified.
        "provider_reported_unverified",
        // Status exposes only closed, non-secret diagnostics.
        "closed, non-secret diagnostics",
    ] {
        assert!(
            flat.contains(needle),
            "README.md must state the broker guarantee/residual risk; missing {needle:?}. The \
             operator docs (design §1.2/§17) must name the exact guarantee and the same-user \
             residual risk without overstating either."
        );
    }
}
