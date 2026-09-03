//! P3 phase gate — the cross-language integration check that closes P3.
//!
//! Launches the in-workspace R3 `linear-stub` (`harness/stubs/linear-stub`) with its `basic.json`
//! scenario, points the real `rhapsody-tracker` Linear adapter at it, and drives the FULL read
//! surface plus a `MoveIssueState` write, asserting the normalized [`Issue`] set and the post-move
//! state against the literal values the scenario encodes (the same values the Go adapter's unit
//! tests assert). The stub builds in-workspace, so this runs in CI with no `$REF` and no network —
//! the P3 plan's "assert against expected values derived from the scenario file" gate.
//!
//! The stub is a separate binary; `cargo test --workspace` (CI's `test` job, per the Makefile)
//! compiles every workspace member before running any test, so the binary sits beside this test in
//! the same profile dir. A `cargo test -p rhapsody-tracker` run (which doesn't build the harness
//! member) triggers the one-off build fallback in [`stub_binary`].

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

use rhapsody_tracker::Tracker;
use rhapsody_tracker::linear::{Config, new};

/// Owns the spawned `linear-stub` child and kills it on drop, so a panicking assertion never leaks
/// the process (or leaves its ephemeral port bound).
struct StubProcess {
    child: Child,
}

impl Drop for StubProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The workspace root (two levels up from `crates/tracker`).
fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The `linear-stub` executable path. It lives in the same profile dir as this test binary (cargo
/// builds all workspace members before running any test); derive it from our own executable so a
/// custom `CARGO_TARGET_DIR`/profile is handled automatically. Falls back to an explicit
/// `cargo build -p linear-stub` for package-scoped test runs that don't build the harness member.
fn stub_binary() -> PathBuf {
    let mut dir = std::env::current_exe().expect("locate the test executable");
    dir.pop(); // the test binary file itself
    if dir.file_name().is_some_and(|n| n == "deps") {
        dir.pop(); // deps/ -> the profile dir (debug/release)
    }
    let bin = dir.join(format!("linear-stub{}", std::env::consts::EXE_SUFFIX));
    if !bin.exists() {
        let status = Command::new(env!("CARGO"))
            .args(["build", "-p", "linear-stub"])
            .current_dir(workspace_root())
            .status()
            .expect("run cargo build -p linear-stub");
        assert!(status.success(), "cargo build -p linear-stub failed");
    }
    assert!(
        bin.exists(),
        "linear-stub binary not found at {} (run `cargo test --workspace`)",
        bin.display()
    );
    bin
}

/// The committed basic scenario the stub serves.
fn scenario_path() -> PathBuf {
    workspace_root().join("harness/stubs/linear-stub/testdata/basic.json")
}

/// Spawn the stub on an ephemeral port and read the `LISTENING <port>` line it prints once bound.
fn spawn_stub() -> (StubProcess, u16) {
    let mut child = Command::new(stub_binary())
        .arg("--scenario")
        .arg(scenario_path())
        .args(["--port", "0"])
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn linear-stub");
    let stdout = child.stdout.take().expect("capture stub stdout");
    let mut line = String::new();
    BufReader::new(stdout)
        .read_line(&mut line)
        .expect("read the stub's LISTENING line");
    let port: u16 = line
        .trim()
        .strip_prefix("LISTENING ")
        .unwrap_or_else(|| panic!("unexpected stub greeting: {line:?}"))
        .parse()
        .expect("parse the announced port");
    (StubProcess { child }, port)
}

#[test]
fn stub_gate_drives_full_read_surface_and_a_move() {
    let (_stub, port) = spawn_stub();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");

    rt.block_on(async move {
        let c = new(Config {
            endpoint: format!("http://127.0.0.1:{port}/graphql"),
            api_key: "stub-key".into(),
            project_slug: "558008ab185c".into(),
            active_states: vec!["Todo".into(), "In Progress".into()],
            ..Config::default()
        });

        // ── identity ─────────────────────────────────────────────────────────────────────────
        let viewer = c.resolve_viewer().await.expect("resolve_viewer");
        assert_eq!(viewer.id, "usr_stub");
        assert_eq!(viewer.name, "symphony-stub");

        // ── candidates: the one Todo issue, fully normalized ─────────────────────────────────
        let candidates = c.fetch_candidate_issues().await.expect("fetch_candidate_issues");
        assert_eq!(candidates.len(), 1, "one candidate in the basic scenario");
        let iss = &candidates[0];
        assert_eq!(iss.id, "iss_1");
        assert_eq!(iss.identifier, "RHA-1");
        assert_eq!(iss.title, "Smoke issue");
        assert_eq!(iss.description.as_deref(), Some("Do nothing, successfully."));
        assert_eq!(iss.state, "Todo");
        assert_eq!(iss.team_id, "team_stub");
        assert!(iss.labels.is_none(), "no labels -> None");
        assert!(iss.blocked_by.is_none(), "no blockers -> None");

        // ── by-states / by-ids: minimal normalized issues ───────────────────────────────────
        let by_states = c
            .fetch_issues_by_states(&["Todo".into()])
            .await
            .expect("fetch_issues_by_states");
        assert_eq!(by_states.len(), 1);
        assert_eq!(by_states[0].identifier, "RHA-1");
        assert_eq!(by_states[0].state, "Todo");
        // A non-matching state returns nothing.
        assert!(
            c.fetch_issues_by_states(&["Done".into()])
                .await
                .expect("fetch_issues_by_states(Done)")
                .is_empty()
        );

        let by_ids = c
            .fetch_issue_states_by_ids(&["iss_1".into()])
            .await
            .expect("fetch_issue_states_by_ids");
        assert_eq!(by_ids.len(), 1);
        assert_eq!(by_ids[0].id, "iss_1");
        assert_eq!(by_ids[0].state, "Todo");

        // The labels read answers for the SAME ids, in any state, with labels instead of state.
        let labels = c
            .fetch_issue_labels_by_ids(&["iss_1".into()])
            .await
            .expect("fetch_issue_labels_by_ids");
        assert_eq!(labels.len(), 1);
        assert_eq!(labels[0].id, "iss_1");
        assert_eq!(labels[0].identifier, "RHA-1");
        assert!(labels[0].state.is_empty(), "the labels read carries no state");

        // ── blocked-backlog + branch hint: advisory, empty in this scenario ──────────────────
        assert!(
            c.fetch_blocked_backlog_issues()
                .await
                .expect("fetch_blocked_backlog_issues")
                .is_empty(),
            "no Backlog-state issue in the basic scenario"
        );
        let (branch, pr) = c
            .fetch_issue_branch_by_id("iss_1")
            .await
            .expect("fetch_issue_branch_by_id");
        assert_eq!((branch.as_str(), pr), ("", 0), "no branch/PR -> advisory empty");

        // ── projects: the single scenario project ────────────────────────────────────────────
        let projects = c.list_projects().await.expect("list_projects");
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].id, "proj_stub");
        assert_eq!(projects[0].name, "Rhapsody");
        assert_eq!(projects[0].slug, "558008ab185c");

        // ── the WRITE: move RHA-1 (Todo) to In Progress, then read the post-move state back ──
        c.move_issue_state("iss_1", "team_stub", "In Progress")
            .await
            .expect("move_issue_state");
        let after = c
            .fetch_issue_states_by_ids(&["iss_1".into()])
            .await
            .expect("fetch_issue_states_by_ids after move");
        assert_eq!(after.len(), 1);
        assert_eq!(
            after[0].state, "In Progress",
            "the stub reflects the moved state, proving MoveIssueState round-trips name -> UUID -> mutate"
        );
    });
}
