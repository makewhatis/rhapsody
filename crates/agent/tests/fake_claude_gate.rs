//! P4 phase gate — the Rust claude runner driving the COMMITTED `harness/stubs/fake-claude`
//! (success / error / hang) produces the same normalized event streams the Go daemon recorded in
//! `harness/fixtures/runs/*.jsonl`. This is the parity proof the P4 plan's completion check names
//! ("Runner behavior vs recorded Go lifecycles"), and it mirrors the reference's non-live
//! `fake_claude_test.go` (the same three outcomes through the REAL runner, plus the held-open-stdin
//! mailbox coexistence check). No `$REF` is touched at test time — fake-claude is committed.
//!
//! How the comparison works: `runs/*.jsonl` is the Go daemon's `/api/v1/runs/{id}/events` output —
//! its per-event `{at, kind, seq, text, tool}` shape is the raw agent stream-json HUMANIZED (Go
//! `humanize.go`, ported here as [`rhapsody_agent::humanize_stream_line`]) and persisted. So the gate
//! captures the runner's raw stdout via a [`Transcript`], humanizes each line the same way, stamps
//! the store's 1-based `seq`, and asserts the result equals the fixture's FIRST TURN after
//! `harness_fixtures::normalize` (the `success` fixture records 20 identical turns; the runner runs
//! one, so it matches the leading turn). The `stalled` fixture ends at the agent's last emitted line
//! before the hang — exactly what the transcript captures before the turn deadline kills the group.

use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rhapsody_agent::claude::{Config, Runner};
use rhapsody_agent::{
    AgentError, Event, Runner as _, TURN_FAILED, TURN_SUCCEEDED, TURN_TIMED_OUT, Transcript,
    TurnResult, humanize_stream_line,
};
use rhapsody_core::Issue;

/// Absolute path to a committed stub under `harness/stubs/` (the runner cd's into the workspace, so
/// a relative path would not resolve; an absolute path also exercises the stub's "any CWD" contract).
fn stub_path(name: &str) -> String {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(format!("../../harness/stubs/{name}"));
    std::fs::canonicalize(&p)
        .unwrap_or_else(|e| panic!("stub {} not found: {e}", p.display()))
        .to_string_lossy()
        .into_owned()
}

/// RAII temp dir, unique per pid+counter, auto-removed.
struct TempDir {
    dir: std::path::PathBuf,
}

impl TempDir {
    fn new() -> TempDir {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("rhapsody-gate-{}-{seq}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        TempDir { dir }
    }
    fn path(&self) -> String {
        self.dir.to_string_lossy().into_owned()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A shared-buffer `std::io::Write` sink so the test can read the captured transcript after the
/// (owned) sink is moved into the session.
#[derive(Clone)]
struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl SharedBuf {
    fn new() -> SharedBuf {
        SharedBuf(Arc::new(Mutex::new(Vec::new())))
    }
    fn bytes(&self) -> Vec<u8> {
        self.0.lock().unwrap().clone()
    }
}

impl std::io::Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn gate_issue() -> Issue {
    Issue {
        id: "gate".to_string(),
        identifier: "GATE-1".to_string(),
        ..Default::default()
    }
}

/// Runs one turn against `command`, capturing the raw stdout transcript.
async fn run_stub(
    command: String,
    turn_timeout: Duration,
    messages: Option<&mut tokio::sync::mpsc::Receiver<String>>,
) -> (TurnResult, Option<AgentError>, Vec<u8>) {
    let root = TempDir::new();
    let ws = format!("{}/WS", root.path());
    std::fs::create_dir_all(&ws).expect("mkdir ws");
    let raw = SharedBuf::new();
    let transcript = Transcript {
        stdout: Some(Box::new(raw.clone())),
        stderr: None,
    };
    let r = Runner::new(Config {
        command,
        workspace_root: root.path(),
        turn_timeout,
        ..Default::default()
    });
    let sess = r
        .start_session(&ws, gate_issue(), Some(transcript))
        .await
        .expect("start session");
    let on_event = |_e: Event| {};
    let (res, err) = sess
        .run_turn("do the work", None, messages, &on_event)
        .await;
    (res, err, raw.bytes())
}

/// Humanizes a captured raw stdout transcript into the fixture's `/api/v1/runs/{id}/events` shape:
/// one `{at, kind, seq, text, tool}` object per surfaced humanized entry, with the store's 1-based
/// `seq`. `at` is a fixed real timestamp so `normalize` rewrites it to `<TIMESTAMP>` exactly as the
/// capture did.
fn humanized_events(raw: &[u8]) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    let mut seq = 1;
    for line in raw.split(|&b| b == b'\n') {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        for entry in humanize_stream_line(line) {
            out.push(serde_json::json!({
                "at": "2026-07-11T00:00:00Z",
                "kind": entry.kind,
                "seq": seq,
                "text": entry.text,
                "tool": entry.tool,
            }));
            seq += 1;
        }
    }
    out
}

/// Asserts the runner's one-turn humanized events equal the leading (first-turn) events of the
/// committed fixture, after applying the shared `normalize` rules to both.
fn assert_matches_fixture(produced: &[serde_json::Value], fixture_rel: &str) {
    let doc = serde_json::json!({ "events": produced, "run_id": 1 });
    let norm = harness_fixtures::normalize(&serde_json::to_string(&doc).expect("encode produced"));
    let produced_norm: serde_json::Value = serde_json::from_str(&norm).expect("produced is JSON");
    let produced_events = produced_norm["events"].as_array().expect("produced events");
    let fixture = harness_fixtures::load_json(fixture_rel);
    let fixture_events = fixture["events"].as_array().expect("fixture events");

    assert!(!produced_events.is_empty(), "runner produced no events");
    assert!(
        fixture_events.len() >= produced_events.len(),
        "fixture {fixture_rel} has fewer events ({}) than the runner produced ({})",
        fixture_events.len(),
        produced_events.len()
    );
    for (i, (p, f)) in produced_events.iter().zip(fixture_events).enumerate() {
        assert_eq!(
            p, f,
            "event {i} disagrees with {fixture_rel}\n produced: {p}\n fixture:  {f}"
        );
    }
}

// Mirrors Go `claude.TestFakeClaudeIsExecutable`: the committed stubs ship executable.
#[test]
fn fake_claude_stubs_are_executable() {
    for name in ["fake-claude", "fake-claude-error", "fake-claude-hang"] {
        let p = stub_path(name);
        let mode = std::fs::metadata(&p)
            .expect("stat stub")
            .permissions()
            .mode();
        assert!(
            mode & 0o111 != 0,
            "{name} must be committed executable; mode = {mode:o}"
        );
    }
}

// Gate scenario 1 (success) — mirrors Go `claude.TestFakeClaudeSuccessThroughRealRunner` AND asserts
// the normalized event stream equals `runs/success.jsonl`'s first turn.
#[tokio::test]
async fn fake_claude_success_matches_success_fixture() {
    let (res, err, raw) = run_stub(stub_path("fake-claude"), Duration::from_secs(20), None).await;
    assert!(err.is_none(), "RunTurn err = {err:?}");
    assert_eq!(res.status, TURN_SUCCEEDED, "status");
    assert_matches_fixture(&humanized_events(&raw), "runs/success.jsonl");
}

// Gate scenario 2 (error) — mirrors Go `claude.TestFakeClaudeErrorOutcomeThroughRealRunner` AND
// asserts the normalized event stream equals `runs/error.jsonl`.
#[tokio::test]
async fn fake_claude_error_matches_error_fixture() {
    let (res, err, raw) = run_stub(
        stub_path("fake-claude-error"),
        Duration::from_secs(20),
        None,
    )
    .await;
    assert!(err.is_some(), "expected an error for the error outcome");
    assert_eq!(res.status, TURN_FAILED, "status");
    assert_matches_fixture(&humanized_events(&raw), "runs/error.jsonl");
}

// Gate scenario 3 (hang/stalled) — mirrors Go `claude.TestFakeClaudeHangTimesOut` with a short turn
// deadline (the runner-level analogue of the orchestrator stall timeout) AND asserts the events
// captured before the kill equal `runs/stalled.jsonl`.
#[tokio::test]
async fn fake_claude_hang_matches_stalled_fixture() {
    let start = std::time::Instant::now();
    // 3s deadline: generous enough that the stub's process spawn + its three immediate lines are
    // always captured before the kill (even on a loaded CI runner), short enough to stay fast.
    let (res, err, raw) =
        run_stub(stub_path("fake-claude-hang"), Duration::from_secs(3), None).await;
    assert!(
        matches!(err, Some(AgentError::TurnTimeout)),
        "got {err:?}, want TurnTimeout"
    );
    assert_eq!(res.status, TURN_TIMED_OUT, "status");
    assert!(
        start.elapsed() < Duration::from_secs(10),
        "hang kill too slow: {:?}",
        start.elapsed()
    );
    assert_matches_fixture(&humanized_events(&raw), "runs/stalled.jsonl");
}

// Mirrors Go `claude.TestFakeClaudeCoexistsWithHeldOpenStdin` (INF-250): with a message queued on
// the mailbox, the stub (which background-drains stdin) lets the run complete without wedging.
#[tokio::test]
async fn fake_claude_coexists_with_held_open_stdin() {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(4);
    tx.try_send("btw watch the branch".to_string())
        .expect("queue message");
    let (res, err, _raw) = tokio::time::timeout(
        Duration::from_secs(25),
        run_stub(
            stub_path("fake-claude"),
            Duration::from_secs(20),
            Some(&mut rx),
        ),
    )
    .await
    .expect("RunTurn wedged with a held-open stdin mailbox");
    assert!(err.is_none(), "RunTurn err = {err:?}");
    assert_eq!(res.status, TURN_SUCCEEDED, "status");
}
