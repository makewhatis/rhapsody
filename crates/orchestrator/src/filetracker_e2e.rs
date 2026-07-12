//! filetracker_e2e — the INF-303 no-Linear end-to-end acceptance suite (O8, closes the P5 gate).
//!
//! Drives the REAL orchestrator control pass against a temp FILE tracker (`tracker.kind: file`) + the
//! committed `harness/stubs/fake-claude` agent stub through the real claude runner, a real
//! mkdir-backed workspace manager, and an in-memory store — zero network, zero spend. It exercises the
//! four control surfaces the no-Linear smoke validates, end-to-end:
//!   (1) seed Todo               → dispatch + stored run (recorded `continued`, then re-dispatched)
//!   (2) long-run + flip to Done → reconcile tears the live run down (recorded `completed`), no re-dispatch
//!   (3) park In Review + summon → MoveIssueState promote (persisted to the file) + re-engage
//!
//! Like Go `internal/orchestrator/filetracker_e2e_test.go`, the daemon's background poll loop is NEVER
//! started (`poll_interval` is an hour): the test drives `on_tick` / `on_retry` directly and pumps the
//! control channel itself, so every orchestrator-state mutation happens on this one task — the only
//! other tasks are the worker (runs the stub, sends events) and the timers (send events) — making the
//! drive deterministic and race-free.
//!
//! Deviations from the Go source, all behavior-preserving:
//!   * The fake-claude env knob (`FAKE_CLAUDE_SLEEP_S` / `FAKE_CLAUDE_HANG`) is injected via an
//!     `env VAR=val <stub>` command prefix rather than a process-global `t.Setenv`, so the suite's
//!     `#[tokio::test]`s (which cargo runs in PARALLEL, unlike Go's serial package tests) can never
//!     race each other's environment.
//!   * The terminal-teardown scenario (2) pins a long-but-BOUNDED stub sleep where Go uses
//!     `FAKE_CLAUDE_HANG=1` (sleep forever). Reconcile still tears the LIVE run down within a few
//!     seconds — the assertions are identical — but because the Rust worker-cancel drops the run future
//!     rather than SIGKILLing the process group (reconcile's `terminate` fires `re.cancel`; the actual
//!     kill-propagation into the runner is a noted O3/O5 follow-up), a forever-hang would ORPHAN the
//!     stub on the CI runner. A bounded sleep self-terminates on SIGPIPE once the run future is dropped.

use std::sync::Arc;
use std::time::Duration;

use rhapsody_agent::claude;
use rhapsody_store::{OUTCOME_COMPLETED, OUTCOME_CONTINUED, RunFilter, Sqlite, Store, StorePath};
use rhapsody_tracker::{Tracker, file};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::Instant;

use crate::control_loop::{CancelSignal, Event};
use crate::obslog::Store as TranscriptStore;
use crate::orchestrator::Orchestrator;
use crate::retry::EvRetry;
use crate::testsupport::{TempDir, empty_effective, mk_workspace, set_of};

/// The minimal issue shape seeded into the tracker file (Go `fIssue`). `latest_summon_at` is an
/// RFC3339 string; `""` omits it.
struct FIssue {
    id: &'static str,
    identifier: &'static str,
    title: &'static str,
    state: &'static str,
    team_id: &'static str,
    latest_summon_at: &'static str,
}

/// Resolves the committed fake-claude stub to an absolute path (Go `fakeClaudeBin`, `scripts/fake-claude`;
/// here `harness/stubs/fake-claude` under the workspace root, relative to this crate's manifest dir).
fn fake_claude_bin() -> String {
    let p =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../harness/stubs/fake-claude");
    std::fs::canonicalize(&p)
        .unwrap_or_else(|e| {
            panic!(
                "committed fake-claude stub not found at {}: {e}",
                p.display()
            )
        })
        .to_string_lossy()
        .into_owned()
}

/// The claude runner `command` for the committed stub with `env_kv` injected via `env` (see the module
/// docs): `env FAKE_CLAUDE_SLEEP_S=0 /abs/harness/stubs/fake-claude`.
fn fake_claude_cmd(env_kv: &str) -> String {
    let bin = fake_claude_bin();
    if env_kv.is_empty() {
        bin
    } else {
        format!("env {env_kv} {bin}")
    }
}

/// (Re)writes the tracker source JSON atomically (temp + rename), matching how CI or a human would
/// mutate it out-of-band between polls. Mirrors Go `writeTrackerFile`.
fn write_tracker_file(path: &str, issues: &[FIssue]) {
    let arr: Vec<serde_json::Value> = issues
        .iter()
        .map(|i| {
            let mut m = serde_json::Map::new();
            m.insert("id".into(), i.id.into());
            m.insert("identifier".into(), i.identifier.into());
            m.insert("title".into(), i.title.into());
            m.insert("state".into(), i.state.into());
            if !i.team_id.is_empty() {
                m.insert("team_id".into(), i.team_id.into());
            }
            if !i.latest_summon_at.is_empty() {
                m.insert("latest_summon_at".into(), i.latest_summon_at.into());
            }
            serde_json::Value::Object(m)
        })
        .collect();
    let doc = serde_json::json!({
        "state_types": { "backlog": "Backlog", "unstarted": "Todo" },
        "issues": arr,
    });
    let bytes = serde_json::to_vec_pretty(&doc).expect("marshal tracker doc");
    let tmp = format!("{path}.tmp");
    std::fs::write(&tmp, bytes).expect("write tracker tmp");
    std::fs::rename(&tmp, path).expect("rename tracker file");
}

/// Reads the tracker file and returns the state of the issue with the given id. Mirrors Go
/// `readIssueState`.
fn read_issue_state(path: &str, id: &str) -> String {
    let bytes = std::fs::read(path).expect("read tracker file");
    let doc: serde_json::Value = serde_json::from_slice(&bytes).expect("parse tracker file");
    for iss in doc["issues"].as_array().expect("issues array") {
        if iss["id"].as_str() == Some(id) {
            return iss["state"].as_str().unwrap_or_default().to_string();
        }
    }
    panic!("issue {id:?} not found in {path}");
}

/// The number of stored runs (Go `countRuns`).
fn count_runs(o: &Orchestrator) -> usize {
    o.store()
        .list_runs(RunFilter::default())
        .expect("list runs")
        .len()
}

/// The assembled file-tracker orchestrator plus the handles a test drives it through. The temp dirs are
/// held for the test's duration (dropped → removed), mirroring Go `t.TempDir()`'s lifetime.
struct Ft {
    o: Orchestrator,
    rx: UnboundedReceiver<Event>,
    store: Arc<dyn Store + Send + Sync>,
    signal: CancelSignal,
    _root: TempDir,
    _transcripts: TempDir,
    _log_dir: TempDir,
}

/// Wires an orchestrator to a real file tracker, the real claude runner pointed at the committed stub
/// (`claude_env` injected via `env`), a real mkdir-backed workspace manager, and an in-memory store.
/// Mirrors Go `buildFileTrackerOrch`. The control loop is never started; the test drives it directly.
fn build_file_tracker_orch(src: &str, claude_env: &str) -> Ft {
    let store: Arc<dyn Store + Send + Sync> =
        Arc::new(Sqlite::open(StorePath::InMemory).expect("open in-memory store"));
    let root = TempDir::new();
    let transcripts = TempDir::new();
    let log_dir = TempDir::new();

    let workspace = mk_workspace(&root.path);
    let runner = Arc::new(claude::Runner::new(claude::Config {
        command: fake_claude_cmd(claude_env),
        workspace_root: root.path.clone(),
        // Long enough that the long-run case stays running until reconcile tears it down.
        turn_timeout: Duration::from_secs(30),
        ..Default::default()
    }));
    let tracker: Arc<dyn Tracker> = Arc::new(file::new(file::Config {
        source: src.to_string(),
        active_states: vec!["Todo".to_string(), "In Progress".to_string()],
        review_states: vec!["In Review".to_string()],
        ..Default::default()
    }));

    let mut eff = empty_effective(tracker);
    eff.workspace = workspace;
    eff.agent = runner; // Arc<claude::Runner> → Arc<dyn Runner>
    eff.prompt_tmpl = "do the smoke work".to_string(); // plain text; the stub ignores the prompt
    eff.active_states = set_of(&["todo", "in progress"]);
    eff.terminal_states = set_of(&["done", "cancelled"]);
    eff.canceled_states = set_of(&["cancelled"]);
    eff.review_states = set_of(&["in review"]);
    eff.summon_token = "@symphony".to_string();
    eff.review_promote_state = "In Progress".to_string();
    eff.max_concurrent = 10;
    eff.max_retry_backoff_ms = 1000;
    eff.max_turns = 2;
    eff.poll_interval = Duration::from_secs(3600); // no background ticks; the test drives on_tick itself
    eff.stall_timeout = Duration::from_secs(60);
    eff.transcripts = Arc::new(TranscriptStore::new(transcripts.path.clone()));
    eff.log_dir = log_dir.path.clone();

    let mut o = Orchestrator::new("WORKFLOW.md");
    o.set_store(Arc::clone(&store));
    let signal = CancelSignal::new();
    o.ctx = Some(signal.wait()); // set so the continuation/retry timers arm (they select on this ctx)
    o.eff = Some(eff);
    let rx = o.take_events_rx().expect("control-event receiver");
    Ft {
        o,
        rx,
        store,
        signal,
        _root: root,
        _transcripts: transcripts,
        _log_dir: log_dir,
    }
}

/// Processes control events exactly as the control loop would ([`Orchestrator::drive_event`]) until
/// `cond` holds or `timeout` elapses, returning `cond`'s final value. `cond` is re-checked on every
/// event and on a short ticker so a pending timer (a continuation retry) is observed even between
/// events. Mirrors Go `pump`.
async fn pump(
    o: &mut Orchestrator,
    rx: &mut UnboundedReceiver<Event>,
    timeout: Duration,
    cond: impl Fn(&Orchestrator) -> bool,
) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if cond(o) {
            return true;
        }
        let now = Instant::now();
        if now >= deadline {
            return cond(o);
        }
        let step = (deadline - now).min(Duration::from_millis(10));
        tokio::select! {
            ev = rx.recv() => match ev {
                Some(ev) => o.drive_event(ev).await,
                None => return cond(o),
            },
            _ = tokio::time::sleep(step) => {}
        }
    }
}

/// Cancels the lifetime ctx + any live worker (as `shutdown` does), then drains events until the
/// workers unwind (bounded), so no late worker-exit send blocks and the temp dirs can be removed.
/// Mirrors Go `buildFileTrackerOrch`'s `t.Cleanup`.
async fn teardown(o: &mut Orchestrator, rx: &mut UnboundedReceiver<Event>, signal: &CancelSignal) {
    signal.cancel(); // stop the tick/retry timers (they select on o.ctx)
    for re in o.running.values() {
        re.cancel.cancel(); // cancel any still-live worker, exactly as `shutdown` does
    }
    let wg = o.wg.clone();
    let wait = wg.wait();
    let deadline = tokio::time::sleep(Duration::from_secs(5));
    tokio::pin!(wait, deadline);
    loop {
        tokio::select! {
            _ = &mut wait => break,
            _ = rx.recv() => {}
            _ = &mut deadline => break,
        }
    }
}

// Mirrors Go `TestFileTrackerE2E_DispatchContinuedRedispatch`.
#[tokio::test(flavor = "multi_thread")]
async fn file_tracker_e2e_dispatch_continued_redispatch() {
    let dir = TempDir::new();
    let src = dir.child("issues.json");
    write_tracker_file(
        &src,
        &[FIssue {
            id: "i1",
            identifier: "SMK-1",
            title: "Smoke",
            state: "Todo",
            team_id: "team-1",
            latest_summon_at: "",
        }],
    );
    let mut ft = build_file_tracker_orch(&src, "FAKE_CLAUDE_SLEEP_S=0"); // finish instantly

    // (1) seed Todo → poll dispatches and stores a run.
    ft.o.on_tick().await;
    assert!(
        pump(&mut ft.o, &mut ft.rx, Duration::from_secs(10), |o| o
            .running
            .is_empty()
            && count_runs(o) >= 1)
        .await,
        "no stored run after dispatching the Todo ticket"
    );
    let runs = ft.store.list_runs(RunFilter::default()).expect("list runs");
    assert_eq!(
        runs.len(),
        1,
        "want exactly 1 run after first dispatch, got {runs:?}"
    );
    // (2) the ticket stayed active (the stub can't move it), so the segment is `continued`.
    assert_eq!(
        runs[0].outcome, OUTCOME_CONTINUED,
        "first run outcome = {:?}, want continued",
        runs[0].outcome
    );
    assert_eq!(
        runs[0].issue_identifier, "SMK-1",
        "run row malformed: {:?}",
        runs[0]
    );
    assert!(
        !runs[0].ended_at.is_empty(),
        "run row malformed: {:?}",
        runs[0]
    );
    // ...and a continuation is scheduled, which re-dispatches a second run.
    assert!(
        pump(
            &mut ft.o,
            &mut ft.rx,
            Duration::from_secs(10),
            |o| count_runs(o) >= 2
        )
        .await,
        "continuation did not re-dispatch; runs={}",
        count_runs(&ft.o)
    );

    teardown(&mut ft.o, &mut ft.rx, &ft.signal).await;
}

// Mirrors Go `TestFileTrackerE2E_TerminalTeardown`.
#[tokio::test(flavor = "multi_thread")]
async fn file_tracker_e2e_terminal_teardown() {
    let dir = TempDir::new();
    let src = dir.child("issues.json");
    write_tracker_file(
        &src,
        &[FIssue {
            id: "i1",
            identifier: "SMK-1",
            title: "Smoke",
            state: "Todo",
            team_id: "team-1",
            latest_summon_at: "",
        }],
    );
    // A long-but-BOUNDED stub sleep stands in for Go's FAKE_CLAUDE_HANG=1 (see the module docs): the
    // run stays alive so reconcile (not the stub's own exit) tears it down, without orphaning a
    // forever-hung stub on the CI runner.
    let mut ft = build_file_tracker_orch(&src, "FAKE_CLAUDE_SLEEP_S=30");

    // Dispatch the long-running worker and let it get going (drain its init/assistant events).
    ft.o.on_tick().await;
    pump(&mut ft.o, &mut ft.rx, Duration::from_secs(3), |_| false).await;
    assert_eq!(
        ft.o.running.len(),
        1,
        "long-running worker not running: running={}",
        ft.o.running.len()
    );

    // (3) flip the ticket to Done out-of-band; the next poll's reconcile sees the terminal state,
    // terminates the worker, cleans the workspace, and records the run `completed`.
    write_tracker_file(
        &src,
        &[FIssue {
            id: "i1",
            identifier: "SMK-1",
            title: "Smoke",
            state: "Done",
            team_id: "team-1",
            latest_summon_at: "",
        }],
    );
    ft.o.on_tick().await;

    assert_eq!(
        ft.o.running.len(),
        0,
        "run not torn down after the ticket went terminal: running={}",
        ft.o.running.len()
    );
    let runs = ft.store.list_runs(RunFilter::default()).expect("list runs");
    assert_eq!(
        runs.len(),
        1,
        "want 1 completed run after teardown, got {runs:?}"
    );
    assert_eq!(
        runs[0].outcome, OUTCOME_COMPLETED,
        "want 1 completed run after teardown, got {runs:?}"
    );
    // No re-dispatch follows a terminal teardown.
    assert!(
        !pump(
            &mut ft.o,
            &mut ft.rx,
            Duration::from_millis(500),
            |o| count_runs(o) > 1
        )
        .await,
        "unexpected re-dispatch after teardown; runs={}",
        count_runs(&ft.o)
    );

    teardown(&mut ft.o, &mut ft.rx, &ft.signal).await;
}

// Mirrors Go `TestFileTrackerE2E_ReviewReopenPromotesAndReengages`.
#[tokio::test(flavor = "multi_thread")]
async fn file_tracker_e2e_review_reopen_promotes_and_reengages() {
    let dir = TempDir::new();
    let src = dir.child("issues.json");
    // Start active so a prior run exists (review-reopen re-engages only a ticket Symphony has worked
    // before, by comparing the summon time to the last run's end time).
    write_tracker_file(
        &src,
        &[FIssue {
            id: "i1",
            identifier: "SMK-1",
            title: "Smoke",
            state: "In Progress",
            team_id: "team-1",
            latest_summon_at: "",
        }],
    );
    let mut ft = build_file_tracker_orch(&src, "FAKE_CLAUDE_SLEEP_S=0");

    ft.o.on_tick().await;
    assert!(
        pump(&mut ft.o, &mut ft.rx, Duration::from_secs(10), |o| o
            .running
            .is_empty()
            && count_runs(o) >= 1)
        .await,
        "prior run did not complete"
    );
    assert!(
        ft.o.claimed.contains("i1"),
        "expected the claim to be held after a continued exit"
    );

    // Park the ticket In Review with a fresh (far-future) summon.
    write_tracker_file(
        &src,
        &[FIssue {
            id: "i1",
            identifier: "SMK-1",
            title: "Smoke",
            state: "In Review",
            team_id: "team-1",
            latest_summon_at: "2099-01-01T00:00:00Z",
        }],
    );

    // Fire the pending continuation retry: the ticket is no longer active, so on_retry releases the
    // claim (the real release path), leaving it unclaimed and eligible for review-reopen.
    ft.o.on_retry(EvRetry {
        issue_id: "i1".to_string(),
    })
    .await;
    assert!(
        !ft.o.claimed.contains("i1"),
        "claim should be released once the ticket left the active states"
    );
    let runs_before = count_runs(&ft.o);

    // (4) the next poll finds a fresh summon on a review-state ticket → promote via MoveIssueState
    // (persisted back to the file) → dispatch.
    ft.o.on_tick().await;
    assert!(
        pump(
            &mut ft.o,
            &mut ft.rx,
            Duration::from_secs(10),
            |o| count_runs(o) > runs_before
        )
        .await,
        "review-summoned ticket was not re-engaged"
    );
    assert_eq!(
        read_issue_state(&src, "i1"),
        "In Progress",
        "MoveIssueState promote not persisted to the file"
    );

    teardown(&mut ft.o, &mut ft.rx, &ft.signal).await;
}
