//! STUDIO-2169's simulated recall replay for the Teams memory prefetch (test-only; no Go
//! counterpart).
//!
//! The before/after recalls-per-hour numbers in the PR come from this module, not from arithmetic.
//! It drives the REAL [`run_prefetch_schedule`] (its own sleeps, back-off and cycles) under tokio's
//! paused clock, with the entry clock tied to that paused clock, against a counting in-memory
//! bank. Each scenario runs two simulated hours and reports the SECOND, so a cold start's first
//! fill is not counted as steady-state load.
//!
//! Scenarios, all with ten routed candidates split across two teammates and a shared team bank:
//!
//! * `steady`: nothing changes and the bank answers every recall;
//! * `churn`: as `steady`, but one ticket's title changes every ten minutes;
//! * `outage`: every recall fails (the bank is down, or slower than the client's deadline);
//! * `flaky`: nine recalls in ten fail, the shape of the 2026-11-18 incident.
//!
//! Run it with `--nocapture` to print the table:
//! `cargo test -p rhapsody-orchestrator teamsprefetch_replay -- --nocapture`.

use crate::teamsprefetch::{
    MAX_PREFETCH_BACKOFF_MS, PREFETCH_INTERVAL, PrefetchCache, PrefetchDeps, PrefetchTarget,
    run_prefetch_schedule,
};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rhapsody_config::memory::{MemoryBackend, MemoryError, Query, Recalled, Record};
use rhapsody_config::teams::{
    Identity, Manager, ManagerMode, Memory, MemoryBackend as BackendKind, Teams,
};
use rhapsody_core::Issue;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const CANDIDATES: usize = 10;
const HOUR: Duration = Duration::from_secs(3600);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Steady,
    Churn,
    Outage,
    Flaky,
}

/// Counts every recall by bank, failing per [`Mode`].
struct CountingBank {
    mode: Mode,
    calls: Mutex<u64>,
    by_bank: Mutex<BTreeMap<String, u64>>,
}

impl CountingBank {
    fn new(mode: Mode) -> Self {
        Self {
            mode,
            calls: Mutex::new(0),
            by_bank: Mutex::new(BTreeMap::new()),
        }
    }

    fn answer(&self, bank: String) -> Result<Recalled, MemoryError> {
        *self.by_bank.lock().unwrap().entry(bank).or_default() += 1;
        let n = {
            let mut c = self.calls.lock().unwrap();
            *c += 1;
            *c
        };
        let fail = match self.mode {
            Mode::Steady | Mode::Churn => false,
            Mode::Outage => true,
            Mode::Flaky => n % 10 != 0,
        };
        if fail {
            return Err(MemoryError::Io("timed out".to_string()));
        }
        Ok(Recalled::default())
    }

    fn snapshot(&self) -> BTreeMap<String, u64> {
        self.by_bank.lock().unwrap().clone()
    }
}

#[async_trait]
impl MemoryBackend for CountingBank {
    async fn retain(&self, _rec: &Record) -> Result<String, MemoryError> {
        Ok(String::new())
    }
    async fn recall(&self, identity: &str, _q: &Query) -> Result<Recalled, MemoryError> {
        self.answer(format!("agent-{identity}"))
    }
    async fn recall_shared(&self, bank: &str, _q: &Query) -> Result<Recalled, MemoryError> {
        self.answer(bank.to_string())
    }
    async fn invalidate(
        &self,
        _identity: &str,
        _fact_id: &str,
        _reason: &str,
    ) -> Result<bool, MemoryError> {
        Ok(false)
    }
    async fn revalidate(&self, _identity: &str, _fact_id: &str) -> Result<bool, MemoryError> {
        Ok(false)
    }
}

fn teams() -> Teams {
    let ident = |name: &str, label: &str| Identity {
        name: name.to_string(),
        labels: vec![label.to_string()],
        ..Identity::default()
    };
    Teams {
        enabled: true,
        manager: Manager {
            mode: ManagerMode::Labels,
            ..Manager::default()
        },
        memory: Memory {
            backend: BackendKind::Hindsight,
            team_bank: "team".to_string(),
            ..Memory::default()
        },
        roster: vec![ident("alice", "rust"), ident("bob", "web")],
        ..Teams::disabled()
    }
}

fn candidates(elapsed: Duration, mode: Mode) -> Vec<Issue> {
    (0..CANDIDATES)
        .map(|i| {
            let mut title = format!("work on MT-{i}");
            if mode == Mode::Churn && i == 0 {
                title = format!("{title} (edit {})", elapsed.as_secs() / 600);
            }
            Issue {
                id: i.to_string(),
                identifier: format!("MT-{i}"),
                title,
                state: "Todo".to_string(),
                labels: Some(vec![if i % 2 == 0 { "rust" } else { "web" }.to_string()]),
                team_id: "team-1".to_string(),
                ..Issue::default()
            }
        })
        .collect()
}

/// Runs the real schedule for two simulated hours and returns each hour's recalls by bank: the
/// first (cold start, and the first back-off steps) and the second (steady state).
async fn replay(mode: Mode) -> (BTreeMap<String, u64>, BTreeMap<String, u64>) {
    let base: DateTime<Utc> = DateTime::from_timestamp(1_763_500_000, 0).unwrap();
    let start = tokio::time::Instant::now();
    let clock =
        move || base + chrono::Duration::from_std(tokio::time::Instant::now() - start).unwrap();
    let bank = Arc::new(CountingBank::new(mode));
    let deps = PrefetchDeps {
        teams: Arc::new(teams()),
        target: move || {
            Some(PrefetchTarget {
                issues: candidates(tokio::time::Instant::now() - start, mode),
                fetched_at: clock(),
                owner_slugs: Vec::new(),
            })
        },
        backend: Arc::clone(&bank) as Arc<dyn MemoryBackend>,
        cache: Arc::new(PrefetchCache::new()),
        interval: PREFETCH_INTERVAL,
        max_backoff_ms: MAX_PREFETCH_BACKOFF_MS,
        now: Box::new(clock),
    };
    let signal = crate::CancelSignal::new();
    let ctx = signal.wait();
    let mut first = BTreeMap::new();
    let mut second = BTreeMap::new();
    let driver = async {
        // Off the cycle boundaries, so no sample races a cycle landing at the same instant.
        tokio::time::sleep(HOUR + Duration::from_secs(30)).await;
        first = bank.snapshot();
        tokio::time::sleep(HOUR).await;
        second = bank.snapshot();
        signal.cancel();
    };
    tokio::join!(run_prefetch_schedule(ctx, deps), driver);
    let later = second
        .into_iter()
        .map(|(k, v)| {
            let before = first.get(&k).copied().unwrap_or(0);
            (k, v - before)
        })
        .collect();
    (first, later)
}

fn total(m: &BTreeMap<String, u64>) -> u64 {
    m.values().sum()
}

#[tokio::test(start_paused = true)]
async fn teamsprefetch_replay_recalls_per_hour() {
    let mut rows = Vec::new();
    for mode in [Mode::Steady, Mode::Churn, Mode::Outage, Mode::Flaky] {
        let (first, got) = replay(mode).await;
        println!(
            "{mode:?}: first hour {} {first:?}; second hour {} {got:?}",
            total(&first),
            total(&got),
        );
        rows.push((mode, first, got));
    }
    let row = |m: Mode| rows.iter().find(|(r, _, _)| *r == m).unwrap();
    let of = |m: Mode| total(&row(m).2);
    // Ten unchanged candidates on two banks each, refreshed every 30 minutes: 40 an hour. Before
    // STUDIO-2169 the 5-minute refresh made this 240.
    assert_eq!(of(Mode::Steady), 40, "steady state");
    // Plus the edited ticket, re-recalled on both banks after each of its six edits — fewer when
    // an edit lands on the ticket's own 30-minute refresh. Before, edits cost nothing extra only
    // because everything was re-recalled every five minutes anyway.
    assert!(
        of(Mode::Churn) > of(Mode::Steady) && of(Mode::Churn) <= 40 + 6 * 2,
        "one ticket edited every ten minutes: {}",
        of(Mode::Churn)
    );
    // A down bank settles at the 15-minute ceiling: one failing call per 15 minutes.
    assert_eq!(of(Mode::Outage), 4, "a bank that never answers");
    // And it gets there without retrying on the next cycle: failing calls at 1, 3, 7, 15, 30, 45
    // and 60 minutes. The old schedule's first three steps sat under the cadence (10 in hour one).
    assert_eq!(total(&row(Mode::Outage).1), 7, "a down bank's first hour");
    assert!(
        of(Mode::Flaky) <= 12,
        "nine in ten failing: {}",
        of(Mode::Flaky)
    );
}
