//! STUDIO-994 (provider-auth P13) — the cross-lane release matrix manifest.
//!
//! The design calls P13 "the cross-lane release gate: it consumes PB8's broker-specific evidence
//! rather than opening a second implementation of the same hostile fixtures." The behaviour matrix
//! (credential states, version/knob refusals, broker lifecycle/budget, collision, redaction,
//! cancellation, binding/CSRF guards, canary scans, cleanup) was built and tested by its owning
//! slices (P0a..P12, PB0..PB8); duplicating those hostile fixtures here would be the exact mistake
//! the design names. This manifest instead PINS the named negative fixtures across every lane: a
//! deleted guard (or a guard stripped of its `#[test]`/`#[tokio::test]` attribute) turns this red,
//! which is the release-gate half of the mutation discipline — the owning fixture is what goes red
//! when its control is actually removed.
//!
//! It is intentionally a coverage manifest, not another behaviour test: the per-item assertions and
//! their mutation guards live in the files below. Each entry names one required negative control and
//! the test function that exercises it.

use std::fs;
use std::path::{Path, PathBuf};

/// One pinned release-gate fixture: `(lane, relative file, test function name)`.
///
/// The lane is the acceptance category it satisfies, quoted in the failure message so a reviewer can
/// map a red manifest entry back to the design matrix.
const MATRIX: &[(&str, &str, &str)] = &[
    // --- Canary cleanliness: a reusable key / capability must never cross an owned boundary. ---
    (
        "canary: broker redaction surfaces",
        "crates/provider-broker/tests/redaction_canary.rs",
        "no_redacting_surface_carries_the_key_or_capability",
    ),
    (
        "canary: assembled daemon chain",
        "crates/rhapsodyd/tests/brokered_daemon_e2e.rs",
        "the_brokered_daemon_chain_is_canary_clean_at_every_boundary",
    ),
    // --- Budget: under-reported/unsettled usage must not release reservation or day budget. ---
    (
        "budget: under-report keeps the reservation",
        "crates/provider-broker/tests/loopback.rs",
        "an_under_report_does_not_release_the_reservation_or_allow_extra_requests",
    ),
    (
        "budget: concurrent durable-day enforcement",
        "crates/provider-broker/tests/budget.rs",
        "concurrent_runs_cannot_oversubscribe_a_shared_day_authority",
    ),
    (
        "budget: concurrent day charges at the store",
        "crates/store/src/sqlite.rs",
        "concurrent_day_charges_cannot_oversubscribe",
    ),
    (
        "budget: exhausted day cap refuses the turn",
        "crates/provider-broker/tests/budget.rs",
        "arm_turn_refuses_when_the_day_budget_is_already_exhausted",
    ),
    // --- Compatibility: unsupported OpenCode version / knob / request shape must refuse. ---
    (
        "compat: version table is fail-closed",
        "crates/agent/src/opencode/probe.rs",
        "resolve_refuses_unknown_and_near_versions",
    ),
    (
        "compat: unsupported binary refuses at preparation",
        "crates/agent/tests/opencode_brokered.rs",
        "an_unsupported_binary_refuses_at_preparation",
    ),
    (
        "compat: unsupported brokered knobs refuse",
        "crates/agent/tests/opencode_brokered.rs",
        "unsupported_brokered_knobs_refuse_before_probe_or_state",
    ),
    (
        "compat: hostile project config cannot retarget the provider",
        "crates/agent/tests/opencode_brokered.rs",
        "a_hostile_project_config_cannot_retarget_the_generated_provider",
    ),
    (
        "compat: unknown generation control is refused",
        "crates/provider-broker/tests/loopback.rs",
        "an_unknown_generation_control_is_refused_before_upstream_contact",
    ),
    // --- CSRF / binding / collision / redirect controls. ---
    (
        "csrf: model-refresh needs the operator guard",
        "crates/httpapi/src/handlers_providers.rs",
        "refresh_requires_operator_guard_and_a_closed_empty_body",
    ),
    (
        "binding: mismatch refuses registration",
        "crates/provider-broker/tests/lifecycle.rs",
        "a_binding_mismatch_refuses_registration",
    ),
    (
        "binding: stale owner revision is discarded",
        "crates/provider-status/src/status.rs",
        "an_older_owner_revision_is_discarded",
    ),
    (
        "collision: session id cannot alias an existing session",
        "crates/provider-broker/tests/lifecycle.rs",
        "a_session_id_collision_fails_closed_without_aliasing_the_existing_session",
    ),
    (
        "redirect: upstream redirects are not followed",
        "crates/provider-broker/tests/loopback.rs",
        "redirects_are_not_followed",
    ),
    (
        "base-path: a base already ending in chat/completions is refused",
        "crates/provider-broker/src/upstream.rs",
        "a_base_already_ending_in_chat_completions_is_refused",
    ),
    (
        "hostile HTTP: host/origin/method/query/path variants are refused",
        "crates/provider-broker/tests/loopback.rs",
        "host_origin_method_query_and_path_variants_are_refused",
    ),
    // --- Redaction / cancellation races. ---
    (
        "redaction: every chunk-split boundary is covered",
        "crates/provider-broker/tests/loopback.rs",
        "redaction_spans_every_chunk_split_boundary",
    ),
    (
        "cancellation: revocation cancels a live stream",
        "crates/provider-broker/tests/loopback.rs",
        "revoking_the_session_cancels_a_live_stream",
    ),
    (
        "revocation: live turns are invalidated and custody released",
        "crates/provider-broker/tests/lifecycle.rs",
        "session_revoke_invalidates_live_turns_refuses_new_turns_and_releases_custody",
    ),
    // --- Preparation races and refusals. ---
    (
        "prepare: stale completion is dropped",
        "crates/orchestrator/src/prepare.rs",
        "a_stale_token_completion_is_dropped",
    ),
    (
        "prepare: refusal re-arms only on change",
        "crates/orchestrator/src/prepare.rs",
        "refusal_gate_suppresses_identical_and_rearms_on_change",
    ),
    (
        "prepare: a stale credential revision is dropped",
        "crates/orchestrator/src/prepare.rs",
        "a_completion_with_a_stale_credential_revision_is_dropped",
    ),
    (
        "credential: a missing credential refuses before anything is created",
        "crates/agent/src/opencode/state.rs",
        "a_missing_credential_is_refused_before_anything_is_created",
    ),
    // --- State-root escape and cleanup. ---
    (
        "state-root: inside the workspace root is refused",
        "crates/agent/src/opencode/state.rs",
        "a_state_root_inside_the_workspace_root_is_refused",
    ),
    (
        "cleanup: private state removal is idempotent",
        "crates/agent/src/opencode/state.rs",
        "cleanup_is_idempotent_and_drop_also_removes",
    ),
];

fn repo_root() -> PathBuf {
    // `CARGO_MANIFEST_DIR` is `crates/rhapsodyd` for an integration test.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("canonicalize repo root")
}

/// Assert `name` is defined as a test function in `file`.
///
/// The check is presence + a test attribute immediately above it, so a renamed/removed fixture or
/// one downgraded from `#[test]`/`#[tokio::test]` to a plain helper all turn this red.
fn assert_pinned(repo: &Path, lane: &str, file: &str, name: &str) {
    let path = repo.join(file);
    let src =
        fs::read_to_string(&path).unwrap_or_else(|e| panic!("[{lane}] {file}: unreadable ({e})"));
    let needle = format!("fn {name}(");
    let idx = src
        .find(&needle)
        .unwrap_or_else(|| panic!("[{lane}] {file}: missing negative fixture {name}"));
    let window = &src[idx.saturating_sub(400)..idx];
    assert!(
        window.contains("#[test]") || window.contains("#[tokio::test"),
        "[{lane}] {file}: {name} is no longer a #[test]/#[tokio::test] fixture"
    );
}

#[test]
fn the_release_matrixs_named_negative_fixtures_are_present() {
    let repo = repo_root();
    // A canary assertion so a mis-resolved repo root fails loudly rather than scanning nothing.
    assert!(
        repo.join("Cargo.toml").is_file(),
        "repo root did not resolve to a cargo workspace: {}",
        repo.display()
    );
    for (lane, file, name) in MATRIX {
        assert_pinned(&repo, lane, file, name);
    }
    assert!(
        MATRIX.len() >= 25,
        "the release matrix must pin the full set of lanes, got {}",
        MATRIX.len()
    );
}
