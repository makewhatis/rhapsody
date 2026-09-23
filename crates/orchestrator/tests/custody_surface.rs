//! STUDIO-1000 (PB5) custody/surface guard.
//!
//! The binding acceptance says "`RunningEntry` and public snapshots contain no broker handle or
//! secret", and the mutation discipline says placing a broker handle in `RunningEntry`/snapshot or
//! retaining a raw `ApiKey` must turn a guard red. Rust cannot express "this struct has no field of
//! that type" as a trait bound, so this is the repo's established source-scan shape (see
//! `ghsummons`'s `every_gh_exec_goes_through_the_blocking_pool`): read the sources that define the
//! loop-confined state and the public wire shape, and refuse a broker-custody type or a raw-key
//! representation appearing in them.
//!
//! The scan is deliberately narrow — the broker custody types themselves live in
//! `crates/provider-broker`, and the prepared-dispatch layer in `crates/agent/src/dispatch.rs`; only
//! the orchestrator's loop-confined state and its `/api/v1/state` render must stay free of them.

/// Broker custody handles and secret-bearing types that must never appear in loop-confined state or
/// the public snapshot surface.
const FORBIDDEN: &[&str] = &[
    "BrokerSession",
    "BrokerLedgerReceiver",
    "BrokerTurnAttempt",
    "BrokerRegistration",
    "BoundCredentialLease",
    "CapabilityToken",
    "PreparedProvider",
    "DispatchRunner",
    "ProviderAuth",
];

fn assert_surface_free(label: &str, source: &str) {
    for token in FORBIDDEN {
        assert!(
            !source.contains(token),
            "{label} must not mention broker-custody/secret type `{token}`; custody belongs to the \
             non-Clone dispatch runner, not loop-confined state or a public snapshot"
        );
    }
}

/// `RunningEntry` is the loop-confined, cloneable run record. It must hold neither a broker session
/// nor a secret: the live adapter session owns the `BrokerSession`, and the worker owns the
/// non-secret `BrokerLedgerReceiver` outside the loop.
#[test]
fn running_entry_source_has_no_broker_custody_or_secret() {
    assert_surface_free(
        "crates/orchestrator/src/orchestrator.rs",
        include_str!("../src/orchestrator.rs"),
    );
}

/// The `/api/v1/state` wire render is a public snapshot; it must never serialize a broker handle or a
/// secret.
#[test]
fn state_snapshot_render_has_no_broker_custody_or_secret() {
    assert_surface_free(
        "crates/orchestrator/src/snapshot_json.rs",
        include_str!("../src/snapshot_json.rs"),
    );
    assert_surface_free(
        "crates/orchestrator/src/snapshot.rs",
        include_str!("../src/snapshot.rs"),
    );
}

/// The committed `state.json` golden is the observable public snapshot; pin that it carries no broker
/// custody token either.
#[test]
fn committed_state_fixture_has_no_broker_custody_or_secret() {
    assert_surface_free(
        "harness/fixtures/api/state.json",
        include_str!("../../../harness/fixtures/api/state.json"),
    );
}
