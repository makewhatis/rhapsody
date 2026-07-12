//! goldens — the H2 acceptance gate: every read handler's served body, normalized, byte-matches the
//! committed `harness/fixtures/api/*.json` + `runs/*.jsonl` golden captured from the Go daemon.
//!
//! The success-scenario goldens (history/run-detail/events/metrics/run-events) are driven from the
//! COMMITTED `db/go-daemon.db` — the exact SQLite file the Go daemon wrote in the capture that produced
//! those fixtures — so the port is proven against real daemon data, not a hand-rebuilt approximation.
//! The error/stalled run goldens (whose capture DBs are not committed), the projects/logs goldens
//! (snapshot / process-log derived), and the transcript golden (humanized entries) reconstruct their
//! scenario in-process, exactly as H1's `state.json` golden reconstructs its snapshot.
//!
//! This is a golden-parity gate, NOT a place to launder drift: every `assert_golden` runs the SAME
//! `harness_fixtures::normalize` the capture pipeline uses and compares to the UNEDITED committed
//! fixture. A mismatch means the port is wrong (or the fixture must be re-captured via `make fixtures`),
//! never that the assertion should be loosened.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use rhapsody_agent::LogEntry as TranscriptEntry;
use rhapsody_orchestrator::ProjectStatus;
use rhapsody_store::{
    EventRow, OUTCOME_FAILED, RunEnd, RunFilter, RunStart, Sqlite, Store, StorePath,
};
use serde_json::Value;
use tokio::sync::broadcast;

use crate::testutil::{FakeProvider, empty_snapshot, spawn_router};
use crate::{LogEntry, LogSource, new_handler};

// -------- shared golden scaffolding --------

/// Recursively sort object keys, mirroring the capture pipeline's `jq -S .` (which stabilizes key order
/// before a fixture is committed). Same helper H1's state golden + the config golden use.
fn sort_keys(v: Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(
            m.into_iter()
                .map(|(k, v)| (k, sort_keys(v)))
                .collect::<BTreeMap<_, _>>()
                .into_iter()
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.into_iter().map(sort_keys).collect()),
        other => other,
    }
}

/// Assert the served `body`, canonicalized (sorted keys, pretty, trailing newline) and normalized with
/// `home`, is byte-identical to the committed `fixture`. `home` is the capture-home prefix to rewrite to
/// `<HOME>` (`""` when the fixture carries no home path).
fn assert_golden(body: Value, fixture: &str, home: &str) {
    let pretty = format!(
        "{}\n",
        serde_json::to_string_pretty(&sort_keys(body)).expect("serialize")
    );
    let got = harness_fixtures::normalize_with_home(&pretty, home);
    let want = harness_fixtures::normalize_with_home(&harness_fixtures::load(fixture), home);
    assert_eq!(got, want, "served body drifts from {fixture}");
}

/// A unique scratch directory under the OS temp dir (mirrors the store crate's `scratch_dir`).
fn scratch_dir() -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "rhapsody-httpapi-golden-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

/// Open the committed Go-daemon DB on a writable COPY (`Sqlite::open` sets `journal_mode=WAL`, which
/// rewrites the header + spawns `-wal`/`-shm` sidecars — opening the fixture in place would dirty the
/// tree). Returns the store + the capture-home prefix recovered from the run's `transcript_path` (the
/// SAME substitution `normalize.sh` applied when it wrote the committed fixtures), so `history.json`
/// (which carries a `<HOME>/.symphony/logs/…` transcript path) compares byte-for-byte.
fn open_go_daemon_store() -> (Arc<Sqlite>, String) {
    let src = harness_fixtures::fixtures_dir().join("db/go-daemon.db");
    let db = scratch_dir().join("go-daemon.db");
    std::fs::copy(&src, &db).expect("copy fixture db");
    let store = Arc::new(Sqlite::open(StorePath::Disk(db)).expect("open go-daemon.db"));
    let runs = store.list_runs(RunFilter::default()).expect("list_runs");
    let home = runs
        .first()
        .and_then(|r| r.transcript_path.split("/.symphony/logs/").next())
        .unwrap_or("")
        .to_string();
    (store, home)
}

async fn spawn(provider: FakeProvider) -> String {
    spawn_router(new_handler(Arc::new(provider), None)).await
}

async fn spawn_with_logs(provider: FakeProvider, logs: Arc<dyn LogSource>) -> String {
    spawn_router(new_handler(Arc::new(provider), Some(logs))).await
}

async fn get_json(url: &str) -> (reqwest::StatusCode, Value) {
    let resp = reqwest::get(url).await.expect("GET");
    let status = resp.status();
    let body: Value = serde_json::from_str(&resp.text().await.expect("body")).expect("json");
    (status, body)
}

/// Synthetic capture `$HOME` / stub port the placeholders substitute in — the SAME values the config
/// crate's render golden uses, so the served config normalizes to the committed `<HOME>`/`<PORT>`.
const CONFIG_CAPTURE_HOME: &str = "/capture-home";
const CONFIG_STUB_PORT: &str = "51234";

/// Materialize a committed capture workflow (with the three placeholders substituted) as a real
/// WORKFLOW.md under a scratch dir, returning its path — the served config handler loads it. Mirrors
/// the config crate golden's `load_substituted`, but keeps the file on disk for the HTTP GET.
fn materialize_capture_workflow(name: &str) -> PathBuf {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(format!("../../harness/capture/workflows/{name}.md"));
    let raw = std::fs::read_to_string(&src)
        .unwrap_or_else(|e| panic!("read capture workflow {name}: {e}"));
    let substituted = raw
        .replace("__STUB_PORT__", CONFIG_STUB_PORT)
        .replace(
            "__CLAUDE_CMD__",
            &format!("{CONFIG_CAPTURE_HOME}/bin/fake-claude"),
        )
        .replace(
            "__STORE_PATH__",
            &format!("{CONFIG_CAPTURE_HOME}/symphony.db"),
        );
    let path = scratch_dir().join("WORKFLOW.md");
    std::fs::write(&path, substituted).expect("write workflow");
    path
}

/// The H3 config gate: the served `GET /api/v1/config` body, normalized, is byte-identical to the
/// committed `api/config.json` — the same `minimal.md` capture the config crate's render golden
/// asserts, proven here end-to-end over the HTTP server (the analog of H1's state golden). The config
/// handler REUSES `effective_json::render`, so this closes the loop that the served view matches too.
#[tokio::test]
async fn config_endpoint_matches_config_golden() {
    let path = materialize_capture_workflow("minimal");
    let provider = FakeProvider::ok(empty_snapshot()).with_workflow_path(path.to_string_lossy());
    let base = spawn(provider).await;
    let (status, body) = get_json(&format!("{base}/api/v1/config")).await;
    assert_eq!(status, 200);
    assert_golden(body, "api/config.json", CONFIG_CAPTURE_HOME);
}

fn event_row(seq: i64, kind: &str, text: &str) -> EventRow {
    EventRow {
        seq,
        at: "2026-05-28T12:00:00Z".into(),
        kind: kind.into(),
        tool: String::new(),
        text: text.into(),
    }
}

/// Seed a fresh in-memory store with one FAILED run (RHA-1, run id 1) matching an error/stalled capture:
/// its `error`, token tallies, and coarse events reproduce the committed run-detail + run-events goldens.
fn seed_failed_run(
    title: &str,
    error: &str,
    tokens: (i64, i64, i64),
    events: &[(&str, &str)],
) -> Arc<Sqlite> {
    let store = Arc::new(Sqlite::open(StorePath::InMemory).expect("open in-memory store"));
    let id = store
        .start_run(RunStart {
            issue_id: "iss_1".into(),
            issue_identifier: "RHA-1".into(),
            title: title.into(),
            started_at: "2026-05-28T12:00:00Z".into(),
            project_slug: "558008ab185c".into(),
            ..Default::default()
        })
        .expect("start run");
    let rows: Vec<EventRow> = events
        .iter()
        .enumerate()
        .map(|(i, (kind, text))| event_row(i as i64 + 1, kind, text))
        .collect();
    store.append_events(id, &rows).expect("append events");
    store
        .end_run(
            id,
            RunEnd {
                outcome: OUTCOME_FAILED.into(),
                ended_at: "2026-05-28T12:00:05Z".into(),
                turns: 1,
                input_tokens: tokens.0,
                output_tokens: tokens.1,
                total_tokens: tokens.2,
                error: error.into(),
                ..Default::default()
            },
        )
        .expect("end run");
    store
}

// -------- success-scenario goldens (driven from the committed go-daemon.db) --------

#[tokio::test]
async fn history_matches_golden() {
    let (store, home) = open_go_daemon_store();
    let base = spawn(FakeProvider::ok(empty_snapshot()).with_history(store)).await;
    let (status, body) = get_json(&format!("{base}/api/v1/history")).await;
    assert_eq!(status, 200);
    assert_golden(body, "api/history.json", &home);
}

#[tokio::test]
async fn run_detail_matches_golden() {
    let (store, home) = open_go_daemon_store();
    let base = spawn(FakeProvider::ok(empty_snapshot()).with_history(store)).await;
    let (status, body) = get_json(&format!("{base}/api/v1/runs/1")).await;
    assert_eq!(status, 200);
    assert_golden(body, "api/run_detail.json", &home);
}

#[tokio::test]
async fn event_search_matches_golden() {
    let (store, home) = open_go_daemon_store();
    let base = spawn(FakeProvider::ok(empty_snapshot()).with_history(store)).await;
    let (status, body) = get_json(&format!("{base}/api/v1/events")).await;
    assert_eq!(status, 200);
    assert_golden(body, "api/events.json", &home);
}

#[tokio::test]
async fn metrics_matches_golden() {
    // days=0 (all-time) is time-STABLE against the frozen db and yields the identical single-day rollup
    // the capture's default (days=30, relative to capture wall-clock) produced — the db holds one run.
    let (store, home) = open_go_daemon_store();
    let base = spawn(FakeProvider::ok(empty_snapshot()).with_history(store)).await;
    let (status, body) = get_json(&format!("{base}/api/v1/metrics?days=0")).await;
    assert_eq!(status, 200);
    assert_golden(body, "api/metrics.json", &home);
}

#[tokio::test]
async fn run_events_success_matches_golden() {
    let (store, home) = open_go_daemon_store();
    let base = spawn(FakeProvider::ok(empty_snapshot()).with_history(store)).await;
    let (status, body) = get_json(&format!("{base}/api/v1/runs/1/events")).await;
    assert_eq!(status, 200);
    assert_golden(body, "runs/success.jsonl", &home);
}

// -------- error / stalled run goldens (reconstructed; their capture DBs are not committed) --------

const ERROR_EVENTS: [(&str, &str); 4] = [
    ("event", "session started"),
    ("text", "fake-claude: starting smoke run"),
    ("text", "fake-claude: finishing smoke run"),
    ("event", "turn failed: error_during_execution"),
];

const STALLED_EVENTS: [(&str, &str); 3] = [
    ("event", "session started"),
    ("text", "fake-claude: starting smoke run"),
    ("text", "fake-claude: hanging (FAKE_CLAUDE_HANG=1)"),
];

#[tokio::test]
async fn run_detail_error_matches_golden() {
    let store = seed_failed_run(
        "Error smoke issue",
        "turn_failed: result reported error",
        (1, 1, 2),
        &ERROR_EVENTS,
    );
    let base = spawn(FakeProvider::ok(empty_snapshot()).with_history(store)).await;
    let (status, body) = get_json(&format!("{base}/api/v1/runs/1")).await;
    assert_eq!(status, 200);
    assert_golden(body, "api/run_detail_error.json", "");
}

#[tokio::test]
async fn run_events_error_matches_golden() {
    let store = seed_failed_run(
        "Error smoke issue",
        "turn_failed: result reported error",
        (1, 1, 2),
        &ERROR_EVENTS,
    );
    let base = spawn(FakeProvider::ok(empty_snapshot()).with_history(store)).await;
    let (status, body) = get_json(&format!("{base}/api/v1/runs/1/events")).await;
    assert_eq!(status, 200);
    assert_golden(body, "runs/error.jsonl", "");
}

#[tokio::test]
async fn run_detail_stalled_matches_golden() {
    let store = seed_failed_run(
        "Hang smoke issue",
        "turn_timeout: turn exceeded 3s",
        (0, 0, 0),
        &STALLED_EVENTS,
    );
    let base = spawn(FakeProvider::ok(empty_snapshot()).with_history(store)).await;
    let (status, body) = get_json(&format!("{base}/api/v1/runs/1")).await;
    assert_eq!(status, 200);
    assert_golden(body, "api/run_detail_stalled.json", "");
}

#[tokio::test]
async fn run_events_stalled_matches_golden() {
    let store = seed_failed_run(
        "Hang smoke issue",
        "turn_timeout: turn exceeded 3s",
        (0, 0, 0),
        &STALLED_EVENTS,
    );
    let base = spawn(FakeProvider::ok(empty_snapshot()).with_history(store)).await;
    let (status, body) = get_json(&format!("{base}/api/v1/runs/1/events")).await;
    assert_eq!(status, 200);
    assert_golden(body, "runs/stalled.jsonl", "");
}

// -------- projects golden (snapshot-derived) --------

#[tokio::test]
async fn projects_matches_golden() {
    let mut snap = empty_snapshot();
    snap.projects = vec![ProjectStatus {
        slug: "558008ab185c".into(),
        name: "558008ab185c".into(),
        status: "idle".into(),
        running: 0,
        warnings: Vec::new(),
    }];
    let base = spawn(FakeProvider::ok(snap)).await;
    let (status, body) = get_json(&format!("{base}/api/v1/projects")).await;
    assert_eq!(status, 200);
    assert_golden(body, "api/projects.json", "");
}

// -------- transcript golden (humanized entries) --------

#[tokio::test]
async fn run_transcript_matches_golden() {
    // The success run's transcript is the fake-claude turn cycle × 20 (session started / starting /
    // finishing / turn completed) — the humanized entries the daemon captured. The handler assigns the
    // 1-based seq; the humanize step itself is proven by the agent crate. (`tool` is always "" here.)
    let cycle = [
        ("event", "session started"),
        ("text", "fake-claude: starting smoke run"),
        ("text", "fake-claude: finishing smoke run"),
        ("event", "turn completed"),
    ];
    let entries: Vec<TranscriptEntry> = (0..20)
        .flat_map(|_| {
            cycle.iter().map(|(kind, text)| TranscriptEntry {
                kind: (*kind).into(),
                tool: String::new(),
                text: (*text).into(),
            })
        })
        .collect();
    let base = spawn(FakeProvider::ok(empty_snapshot()).with_transcript(Some(entries))).await;
    let (status, body) = get_json(&format!("{base}/api/v1/runs/1/transcript")).await;
    assert_eq!(status, 200);
    assert_golden(body, "runs/success_transcript.jsonl", "");
}

// -------- logs golden (process-log ring) --------

/// A minimal [`LogSource`] serving a fixed backlog (only `snapshot` is exercised by `GET /api/v1/logs`).
struct GoldenLogSource(Vec<LogEntry>);

impl LogSource for GoldenLogSource {
    fn snapshot(&self) -> Vec<LogEntry> {
        self.0.clone()
    }
    fn subscribe(&self) -> broadcast::Receiver<LogEntry> {
        broadcast::channel(1).1
    }
    fn epoch(&self) -> u64 {
        0
    }
}

fn log_entry(seq: u64, level: &str, msg: &str, attrs: &[(&str, &str)]) -> LogEntry {
    LogEntry {
        seq,
        time: "2026-05-28T12:00:00Z".into(),
        level: level.into(),
        msg: msg.into(),
        attrs: attrs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect(),
    }
}

#[tokio::test]
async fn logs_matches_golden() {
    // Reconstruct the daemon's captured boot-log sequence (27 entries). `home` + a loopback host:port
    // normalize to `<HOME>` / `<PORT>`; the fixed `time` to `<TIMESTAMP>`. This proves the LogEntry wire
    // shape (seq/time/level/msg/attrs, attrs omitempty) — the handler itself just serializes the ring.
    let home = "/cap/home";
    let mut entries = vec![
        log_entry(
            1,
            "INFO",
            "observability server listening",
            &[("addr", "127.0.0.1:8080")],
        ),
        log_entry(
            2,
            "INFO",
            "linear: candidate filter bound to key owner",
            &[
                ("assignee", "symphony-stub"),
                ("email", ""),
                ("id", "usr_stub"),
            ],
        ),
        log_entry(3, "INFO", "pruned old history", &[("retention_days", "30")]),
        log_entry(
            4,
            "WARN",
            "CPU-based liveness unavailable (no readable /proc); stall detection will not fire",
            &[("stall_timeout", "5m0s")],
        ),
        log_entry(
            5,
            "INFO",
            "durable history store open",
            &[("path", &format!("{home}/symphony.db"))],
        ),
        log_entry(
            6,
            "INFO",
            "dispatching issue",
            &[
                ("attempt", "0"),
                ("issue_id", "iss_1"),
                ("issue_identifier", "RHA-1"),
                ("project_slug", "558008ab185c"),
                ("run_id", "1"),
            ],
        ),
    ];
    // seq 7..26: the 20 "agent turn start" lines (turn 1 is resume=false; 2..20 resume=true).
    for turn in 1..=20u64 {
        let resume = if turn == 1 { "false" } else { "true" };
        let turn_s = turn.to_string();
        entries.push(log_entry(
            turn + 6,
            "INFO",
            "agent turn start",
            &[
                ("attempt", "-1"),
                ("issue", "RHA-1"),
                ("resume", resume),
                ("turn", &turn_s),
            ],
        ));
    }
    entries.push(log_entry(
        27,
        "INFO",
        "worker completed",
        &[
            ("issue_id", "iss_1"),
            ("issue_identifier", "RHA-1"),
            ("last_state", "Todo"),
            ("run_id", "1"),
            ("session_id", "fake-claude-session-20"),
            ("snapshot_state", "Todo"),
        ],
    ));

    let source: Arc<dyn LogSource> = Arc::new(GoldenLogSource(entries));
    let base = spawn_with_logs(FakeProvider::ok(empty_snapshot()), source).await;
    let (status, body) = get_json(&format!("{base}/api/v1/logs")).await;
    assert_eq!(status, 200);
    assert_golden(body, "api/logs.json", home);
}
