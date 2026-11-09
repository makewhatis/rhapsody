//! STUDIO-1896's simulated-hour Linear request replay (test-only; no Go counterpart).
//!
//! The per-caller request numbers in the PR come from this module, not from arithmetic. It points
//! fourteen REAL `rhapsody_tracker::linear` clients, all on one API key, at a loopback GraphQL mock.
//! It then drives one simulated hour of the daemon's two steady Linear consumers, in a fixed order:
//!
//! * `on_tick`, at the configured poll interval, or the paced one while the budget is tight;
//! * the Teams triage cycle, at [`crate::triage::TRIAGE_INTERVAL`].
//!
//! It runs this twice: once with triage fetching for itself (the code before this change), and once
//! with triage reading the poll's published set.
//!
//! Two independent counts are compared. The first is the ledger's per-caller breakdown, which is
//! what the daemon logs when the budget tightens. The second is the mock's own count of requests it
//! actually served. They must agree exactly, with nothing `untagged`. That is what makes the
//! breakdown a measurement of the transport rather than a restatement of the code.
//!
//! **What it does not cover.** It does not cover completion revalidation, PR links or state moves.
//! Those are driven by agent runs, and nothing here runs an agent. Nor does it cover the per-retry
//! candidate fan-out. That one has its own call-count test:
//! `retry::tests::a_tick_reads_each_projects_candidates_once_across_retries_and_the_poll`.
//!
//! Run it with `--nocapture` to print both breakdowns:
//! `cargo test -p rhapsody-orchestrator linear_budget_replay -- --nocapture`.

use crate::dispatch::DispatchStates;
use crate::testsupport::{empty_resolved_project, orch_for_retry_multi, set_of};
use crate::triage::{
    CycleOutcome, TRIAGE_INTERVAL, TriageArbiter, TriageDecision, TriageDeps, TriageHandle,
    TriageRequest, TriageTarget, triage_cycle,
};
use async_trait::async_trait;
use chrono::{DateTime, TimeDelta, Utc};
use rhapsody_config::teams::{Identity, Manager, ManagerMode, Teams};
use rhapsody_tracker::Tracker;
use rhapsody_tracker::budget::{UNTAGGED, format_breakdown};
use rhapsody_tracker::linear;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The workload from the ticket: fourteen projects on one key.
const PROJECTS: usize = 14;
/// The candidate tickets on each project's board.
const TICKETS_PER_PROJECT: usize = 3;
/// The poll interval the ticket's daemon ran at (`polling.interval_ms: 60000`).
const POLL_INTERVAL: Duration = Duration::from_secs(60);
/// One simulated hour.
const HOUR: Duration = Duration::from_secs(3600);
/// The identity every ticket is already labelled with, so triage sweeps the board and asks no
/// model: the steady state the ticket measured.
const IDENTITY: &str = "alice";

/// A loopback Linear GraphQL double, served from a plain thread so this crate needs no new tokio
/// feature. It answers the read operations a tick and a triage cycle send and COUNTS every request
/// it serves by GraphQL operation name. That count is the transport-side check on the ledger.
struct MockLinear {
    url: String,
    served: Arc<Mutex<BTreeMap<String, u32>>>,
    stop: Arc<AtomicBool>,
    addr: std::net::SocketAddr,
}

impl Drop for MockLinear {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the blocking accept so the thread sees the flag and exits.
        let _ = TcpStream::connect(self.addr);
    }
}

impl MockLinear {
    /// `remaining`, when set, is the `X-RateLimit-Requests-Remaining` every reply reports (out of
    /// Linear's 2,500), so a replay can run with Linear saying the key is nearly spent.
    fn start(boards: HashMap<String, Vec<Value>>, remaining: Option<u32>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        let served: Arc<Mutex<BTreeMap<String, u32>>> = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));
        let by_id: HashMap<String, Value> = boards
            .values()
            .flatten()
            .map(|n| (n["id"].as_str().unwrap_or_default().to_string(), n.clone()))
            .collect();
        let (served_t, stop_t) = (Arc::clone(&served), Arc::clone(&stop));
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                if stop_t.load(Ordering::SeqCst) {
                    return;
                }
                let Ok(mut stream) = stream else { continue };
                let Some(req) = read_request(&mut stream) else {
                    continue;
                };
                let op = operation_name(&req);
                *served_t.lock().unwrap().entry(op.clone()).or_default() += 1;
                let body = answer(&op, &req, &boards, &by_id).to_string();
                let quota = remaining
                    .map(|r| {
                        format!(
                            "X-RateLimit-Requests-Limit: 2500\r\nX-RateLimit-Requests-Remaining: {r}\r\n"
                        )
                    })
                    .unwrap_or_default();
                let reply = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{quota}Connection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(reply.as_bytes());
            }
        });
        Self {
            url: format!("http://{addr}"),
            served,
            stop,
            addr,
        }
    }

    fn served(&self) -> BTreeMap<String, u32> {
        self.served.lock().unwrap().clone()
    }
}

/// Reads one HTTP/1.1 request and decodes its JSON body.
fn read_request(stream: &mut TcpStream) -> Option<Value> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let header_end = loop {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).to_ascii_lowercase();
    let len: usize = head
        .lines()
        .find_map(|l| l.strip_prefix("content-length:"))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    while buf.len() < header_end + len {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    serde_json::from_slice(&buf[header_end..]).ok()
}

/// `query Candidates($…) { … }` → `Candidates`.
fn operation_name(req: &Value) -> String {
    let q = req["query"].as_str().unwrap_or_default().trim_start();
    q.split_whitespace()
        .nth(1)
        .map(|w| w.split(['(', '{']).next().unwrap_or(w).to_string())
        .unwrap_or_default()
}

fn page(nodes: Vec<Value>) -> Value {
    json!({"data": {"issues": {"nodes": nodes, "pageInfo": {"hasNextPage": false, "endCursor": null}}}})
}

fn answer(
    op: &str,
    req: &Value,
    boards: &HashMap<String, Vec<Value>>,
    by_id: &HashMap<String, Value>,
) -> Value {
    let vars = &req["variables"];
    match op {
        "Viewer" => {
            json!({"data": {"viewer": {"id": "viewer-1", "displayName": "Replay", "email": "replay@example.invalid"}}})
        }
        "Candidates" => {
            let slug = vars["projectSlug"].as_str().unwrap_or_default();
            page(boards.get(slug).cloned().unwrap_or_default())
        }
        "ByIDs" | "IssueLabelsByIDs" | "BranchByID" => {
            let nodes = vars["ids"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|id| by_id.get(id.as_str().unwrap_or_default()).cloned())
                .collect();
            page(nodes)
        }
        // Everything else the replay might reach answers an empty page: an empty backlog, no open
        // issues by label, no tickets in a queried state.
        _ => page(Vec::new()),
    }
}

/// A full candidate node, in the shape the candidate query selects.
fn ticket(project: usize, n: usize) -> Value {
    json!({
        // A UUID, as Linear's are: the by-ids reads drop anything else before sending.
        "id": format!("00000000-0000-4000-8000-{:012}", project * 100 + n),
        "identifier": format!("P{project}-{n}"),
        "title": format!("ticket {n} of project {project}"),
        "description": null,
        "priority": 2.0,
        "url": null,
        "branchName": null,
        "createdAt": "2026-11-09T12:00:00Z",
        "updatedAt": "2026-11-09T12:00:00Z",
        "state": {"name": "Todo"},
        "team": {"id": "team-1"},
        "assignee": {"id": "viewer-1", "displayName": "Replay"},
        "projectMilestone": null,
        "labels": {"nodes": [{"name": format!("rhapsody:@{IDENTITY}")}]},
        "inverseRelations": {"nodes": []},
        "attachments": {"nodes": []},
        "comments": {"nodes": []},
    })
}

/// No ticket on the replay board needs a decision, so a model turn would be a replay bug.
#[derive(Default)]
struct NoTurns(AtomicUsize);

#[async_trait]
impl TriageArbiter for NoTurns {
    async fn arbitrate(&self, _req: &TriageRequest) -> Result<TriageDecision, String> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err("the replay board needs no model turn".to_string())
    }
}

/// Which code the replay's triage runs: the old self-fetching cycle, or the reusing one.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Triage {
    FetchesItsOwn,
    ReusesThePoll,
}

struct HourReport {
    per_op: BTreeMap<&'static str, u32>,
    served: BTreeMap<String, u32>,
    ticks: u32,
    cycles: u32,
}

/// Replays one simulated hour and returns both counts. `linear_remaining` is what the mock reports
/// Linear has left on the key (`None`: no quota headers at all).
async fn replay_hour(triage: Triage, linear_remaining: Option<u32>) -> HourReport {
    let boards: HashMap<String, Vec<Value>> = (0..PROJECTS)
        .map(|p| {
            (
                format!("proj{p}"),
                (0..TICKETS_PER_PROJECT).map(|n| ticket(p, n)).collect(),
            )
        })
        .collect();
    let mock = MockLinear::start(boards, linear_remaining);
    // One key per replay, so each hour has its own ledger; the ledger is process-global by key.
    let key = format!(
        "lin_replay_{triage:?}_{linear_remaining:?}_{}",
        std::process::id()
    );
    let clients: Vec<Arc<linear::Client>> = (0..PROJECTS)
        .map(|p| {
            Arc::new(linear::new(linear::Config {
                endpoint: mock.url.clone(),
                api_key: key.clone(),
                project_slug: format!("proj{p}"),
                active_states: vec!["Todo".to_string(), "In Progress".to_string()],
                ..Default::default()
            }))
        })
        .collect();
    let trackers: Vec<Arc<dyn Tracker>> = clients
        .iter()
        .map(|c| Arc::clone(c) as Arc<dyn Tracker>)
        .collect();
    let slugs: Vec<String> = (0..PROJECTS).map(|p| format!("proj{p}")).collect();

    let projects = trackers
        .iter()
        .zip(&slugs)
        .map(|(t, slug)| {
            let mut p = empty_resolved_project(slug, Arc::clone(t));
            p.active_states = set_of(&["todo", "in progress"]);
            p.terminal_states = set_of(&["done"]);
            p.max_concurrent = 10;
            p
        })
        .collect();
    // Four slots, so some tickets run (and are reconciled every tick) while the rest wait.
    let (mut o, _dispatched) = orch_for_retry_multi(projects, 4);
    o.eff.as_mut().expect("effective").poll_interval = POLL_INTERVAL;
    let start: DateTime<Utc> = Utc::now();
    let elapsed_ms = Arc::new(AtomicI64::new(0));
    let clock = Arc::clone(&elapsed_ms);
    o.now = Box::new(move || start + TimeDelta::milliseconds(clock.load(Ordering::SeqCst)));

    let arbiter = Arc::new(NoTurns::default());
    let published = o.control();
    let sim_clock = Arc::clone(&elapsed_ms);
    let target_trackers = trackers.clone();
    let target_slugs = slugs.clone();
    let deps = TriageDeps {
        teams: Arc::new(Teams {
            enabled: true,
            manager: Manager {
                mode: ManagerMode::LabelsModel,
                ..Manager::default()
            },
            roster: vec![Identity {
                name: IDENTITY.to_string(),
                profile: "swe".to_string(),
                ..Default::default()
            }],
            ..Teams::disabled()
        }),
        target: move || {
            Some(TriageTarget {
                trackers: target_trackers.clone(),
                states: DispatchStates {
                    active: set_of(&["todo", "in progress"]),
                    terminal: set_of(&["done"]),
                    ..DispatchStates::default()
                },
                slugs: target_slugs.clone(),
                candidates: match triage {
                    Triage::FetchesItsOwn => None,
                    // Triage judges the set's age by the WALL clock, which the simulated one runs
                    // ahead of. Shifting the poll's simulated stamp by that lead keeps the age
                    // triage sees equal to the simulated age, so a stale set still falls back.
                    Triage::ReusesThePoll => published.reads_candidates().map(|mut c| {
                        let sim_now =
                            start + TimeDelta::milliseconds(sim_clock.load(Ordering::SeqCst));
                        c.fetched_at -= sim_now - Utc::now();
                        c
                    }),
                },
                ..TriageTarget::default()
            })
        },
        arbiter: Arc::clone(&arbiter) as Arc<dyn TriageArbiter>,
        agent_command: "claude".to_string(),
        billing_guard: false,
        tracker_api_key: String::new(),
        interval: TRIAGE_INTERVAL,
        max_backoff_ms: 20,
        handle: Arc::new(TriageHandle::new()),
        room: None,
        history: None,
        reconcile_marker: None,
        ears: None,
        knowledge: None,
    };

    // The hour, as a merge of two fixed schedules. A tick and a cycle due at the same instant run
    // tick-first, the order the daemon's poll publishes and triage then reads.
    let ctx = crate::control_loop::CancelWait::default();
    let (mut next_tick, mut next_cycle) = (Duration::ZERO, Duration::ZERO);
    let (mut ticks, mut cycles) = (0u32, 0u32);
    while next_tick < HOUR || next_cycle < HOUR {
        if next_tick <= next_cycle {
            elapsed_ms.store(next_tick.as_millis() as i64, Ordering::SeqCst);
            o.on_tick().await;
            ticks += 1;
            next_tick += o.paced_poll_interval();
        } else {
            elapsed_ms.store(next_cycle.as_millis() as i64, Ordering::SeqCst);
            let outcome = triage_cycle(&ctx, &deps, true).await;
            assert_eq!(
                outcome,
                CycleOutcome::Idle,
                "every ticket is already decided"
            );
            cycles += 1;
            next_cycle += TRIAGE_INTERVAL;
        }
    }
    assert_eq!(
        arbiter.0.load(Ordering::SeqCst),
        0,
        "the replay asks no model turn"
    );
    HourReport {
        per_op: clients[0].request_ledger().per_op(Instant::now()),
        served: mock.served(),
        ticks,
        cycles,
    }
}

/// The ledger's per-caller breakdown, summed.
fn total(per_op: &BTreeMap<&'static str, u32>) -> u32 {
    per_op.values().sum()
}

/// STUDIO-1896 review B2: one simulated hour of the ticket's 14-project workload, measured three
/// ways. The first is the old triage, which fetches for itself. The second is this change, with the
/// budget healthy. The third is this change while Linear reports the key 84% spent.
///
/// MUTATION GUARDS:
/// * Make triage ignore the published set (`published_candidates` always `None`). Triage then falls
///   back to its own fetch, which it skips while the budget is tight, so the tight hour's cycles
///   report a tracker failure instead of idling.
/// * Return `poll` unconditionally from `paced_poll_interval`. The tight hour then ticks 60 times,
///   not 30.
#[tokio::test(flavor = "multi_thread")]
async fn a_simulated_hour_of_fourteen_projects_counts_every_linear_request_by_caller() {
    let before = replay_hour(Triage::FetchesItsOwn, None).await;
    let after = replay_hour(Triage::ReusesThePoll, None).await;
    // 400 of 2,500 left is 16%, under the 20% line at which the budget counts as tight.
    let tight = replay_hour(Triage::ReusesThePoll, Some(400)).await;
    for (name, hour) in [
        ("before", &before),
        ("after", &after),
        ("after-tight", &tight),
    ] {
        let served: u32 = hour.served.values().sum();
        println!(
            "{name}: ticks={} triage_cycles={} {}",
            hour.ticks,
            hour.cycles,
            format_breakdown(&hour.per_op)
        );
        println!(
            "{name}: served by GraphQL operation: total={served} {:?}",
            hour.served
        );
        assert_eq!(
            total(&hour.per_op),
            served,
            "{name}: the per-caller counts sum to the requests the transport served"
        );
        assert!(
            !hour.per_op.contains_key(UNTAGGED),
            "{name}: every request is attributed to a caller"
        );
        assert_eq!(hour.cycles, 60, "{name}: triage cycles every minute");
    }
    assert_eq!(before.ticks, 60, "a 60s poll ticks 60 times an hour");
    assert_eq!(after.ticks, 60, "a healthy budget leaves the poll alone");
    assert_eq!(tight.ticks, 30, "a tight budget doubles the poll interval");

    let p = PROJECTS as u32;
    let candidate_queries = |h: &HourReport| h.served.get("Candidates").copied().unwrap_or(0);
    assert_eq!(
        candidate_queries(&before),
        p * before.ticks + p * before.cycles,
        "before: the poll AND every triage cycle read every project"
    );
    assert_eq!(
        candidate_queries(&after),
        p * after.ticks,
        "after: only the poll reads the board; triage reuses its set"
    );
    assert_eq!(
        candidate_queries(&tight),
        p * tight.ticks,
        "after, tight: the slower poll is still the only reader"
    );
}
