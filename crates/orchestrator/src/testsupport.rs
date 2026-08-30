//! Shared test scaffolding for the O2 selection/claim modules — the Rust analogue of the Go
//! orchestrator package's cross-`_test.go` helpers (`baseIssue`, `activeSet`, `blocker`, `seedRun`,
//! the tracker fake, the slog buffer), gathered here because Rust test helpers cross module
//! boundaries only through a shared `#[cfg(test)]` module rather than Go's package-shared test files.
//!
//! Two Rust-specific pieces stand in for Go idioms: [`empty_effective`] hand-builds a baseline
//! [`Effective`] (the Rust type holds `Arc<dyn Tracker>`/`Arc<Manager>`/… and so cannot be a partial
//! zero-value struct literal the way Go's `&effective{…}` is — tests override its `pub` scheduling
//! fields), and [`capture_events`] records the `tracing` events a closure emits (the analogue of Go
//! capturing `slog` output to a `bytes.Buffer`).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use rhapsody_agent as agent;
use rhapsody_config::workflow::{Definition, YamlMap};
use rhapsody_config::{Config, decode, resolve};
use rhapsody_core::{BlockerRef, Issue, normalize_state};
use rhapsody_store::{OUTCOME_COMPLETED, RunEnd, RunStart, Sqlite, Store, StorePath};
use rhapsody_tracker::Tracker;
use rhapsody_workspace::{self as workspace, HookScripts, Manager};

use rhapsody_tracker::fake::Fake;

use crate::dispatch::EligibilityGate;
use crate::effective::{DEFAULT_CLAIM_SETTLE_DELAY, DEFAULT_CLAIM_TTL, Effective, ResolvedProject};
use crate::liveness::{self, Sampler};
use crate::obslog::Store as TranscriptStore;
use crate::orchestrator::{Orchestrator, RetryEntry, RunningEntry};

// --- issue / blocker / set builders (Go dispatch_test / select_test helpers) -----------------

/// Builds a minimal candidate issue (title `"t"`). Mirrors Go `select_multi_test.go`'s `iss`.
pub(crate) fn issue(id: &str, ident: &str, state: &str) -> Issue {
    Issue {
        id: id.to_string(),
        identifier: ident.to_string(),
        title: "t".to_string(),
        state: state.to_string(),
        ..Default::default()
    }
}

/// The In-Progress base candidate the eligibility tests start from. Mirrors Go `baseIssue`.
pub(crate) fn base_issue() -> Issue {
    issue("1", "MT-1", "In Progress")
}

/// Builds a blocker reference with the given (optional) identifier + state. Mirrors the
/// `core.BlockerRef{Identifier: …, State: …}` literals in `dispatch_test.go`.
pub(crate) fn blocker(ident: Option<&str>, state: Option<&str>) -> BlockerRef {
    BlockerRef {
        id: None,
        identifier: ident.map(str::to_string),
        state: state.map(str::to_string),
    }
}

/// Builds a state-only blocker (empty ⇒ `None` state). Mirrors `dispatch_depmode_test.go`'s
/// `blocker(state)`.
pub(crate) fn blocker_state(state: &str) -> BlockerRef {
    blocker(None, if state.is_empty() { None } else { Some(state) })
}

/// A normalized string set from raw labels/states.
pub(crate) fn set_of(items: &[&str]) -> HashSet<String> {
    items.iter().map(|s| s.to_string()).collect()
}

/// The common active set `{todo, in progress}`. Mirrors Go `activeSet`.
pub(crate) fn active_set() -> HashSet<String> {
    set_of(&["todo", "in progress"])
}

/// The common terminal set `{done, closed}`. Mirrors Go `terminalSet`.
pub(crate) fn terminal_set() -> HashSet<String> {
    set_of(&["done", "closed"])
}

/// A normalized required-label set (Go `labelSet`, mirroring config-side `normalizeSet`).
pub(crate) fn label_set(labels: &[&str]) -> HashSet<String> {
    labels.iter().map(|l| normalize_state(l)).collect()
}

/// Dependency-mode review/terminal/canceled sets (`dispatch_depmode_test.go`). "Cancelled" is BOTH
/// terminal and cancel-type (mirrors config: a cancel-type state must also be terminal).
pub(crate) fn dep_review() -> HashSet<String> {
    set_of(&["in review"])
}
pub(crate) fn dep_terminal() -> HashSet<String> {
    set_of(&["done", "cancelled"])
}
pub(crate) fn dep_canceled() -> HashSet<String> {
    set_of(&["cancelled"])
}

/// An id set from raw ids (the Go `map[string]bool{"1": true}` reservation sets).
pub(crate) fn id_set(ids: &[&str]) -> HashSet<String> {
    set_of(ids)
}

/// The empty reservation set (Go `nil` running/claimed).
pub(crate) fn no_ids() -> HashSet<String> {
    HashSet::new()
}

/// The identifiers of a pick slice, in order. Mirrors Go `ids`.
pub(crate) fn ids(issues: &[Issue]) -> Vec<String> {
    issues.iter().map(|i| i.identifier.clone()).collect()
}

/// Owns the six eligibility-gate config sets so a test can build a borrowing [`EligibilityGate`]
/// without binding six locals. `standard()`/`dep()` are the two common bases; `with_labels`/
/// `with_mode` layer the per-test overrides.
pub(crate) struct GateData {
    pub active: HashSet<String>,
    pub terminal: HashSet<String>,
    pub labels: HashSet<String>,
    pub mode: String,
    pub review: HashSet<String>,
    pub canceled: HashSet<String>,
}

impl GateData {
    /// active `{todo, in progress}`, terminal `{done, closed}`, disabled mode, no labels/review.
    pub fn standard() -> Self {
        GateData {
            active: active_set(),
            terminal: terminal_set(),
            labels: HashSet::new(),
            mode: String::new(),
            review: HashSet::new(),
            canceled: HashSet::new(),
        }
    }

    /// active `{todo}`, dependency-mode review/terminal/canceled sets. Mirrors the `depmode` tables.
    pub fn dep() -> Self {
        GateData {
            active: set_of(&["todo"]),
            terminal: dep_terminal(),
            labels: HashSet::new(),
            mode: String::new(),
            review: dep_review(),
            canceled: dep_canceled(),
        }
    }

    pub fn with_labels(mut self, labels: &[&str]) -> Self {
        self.labels = label_set(labels);
        self
    }

    pub fn with_mode(mut self, mode: &str) -> Self {
        self.mode = mode.to_string();
        self
    }

    pub fn gate(&self) -> EligibilityGate<'_> {
        EligibilityGate {
            active: &self.active,
            terminal: &self.terminal,
            required_labels: &self.labels,
            mode: &self.mode,
            review: &self.review,
            canceled: &self.canceled,
        }
    }
}

// --- effective + orchestrator builders --------------------------------------------------------

/// A minimal resolved [`Config`] for the `cfg` field of a test [`Effective`] (never read by the O2
/// selection/claim tests). Decoded from a tiny WORKFLOW front matter, mirroring `effective_test.go`.
fn minimal_config() -> Config {
    let front: YamlMap = serde_yaml_ng::from_str(
        "tracker:\n  kind: linear\n  api_key: tok\n  project_slug: proj\n  active_states: [Todo]\nagent:\n  backend: claude\nclaude:\n  command: claude\n",
    )
    .expect("front matter parses");
    let def = Definition {
        config: front,
        prompt_template: String::new(),
    };
    resolve(decode(&def).expect("decode"), "/tmp/wf").expect("resolve")
}

/// A baseline [`Effective`] bound to `tracker`, with every scheduling field zeroed and every
/// infrastructure handle a cheap test double. Tests override the `pub` scheduling fields they care
/// about — the Rust analogue of Go's partial `&effective{…}` literal.
pub(crate) fn empty_effective(tracker: Arc<dyn Tracker>) -> Effective {
    let workspace = Arc::new(
        Manager::new(workspace::Config {
            root: "/tmp".to_string(),
            hooks: HookScripts::default(),
            hook_timeout: Duration::ZERO,
        })
        .expect("workspace manager"),
    );
    Effective {
        cfg: minimal_config(),
        tracker,
        workspace,
        agent: Arc::new(agent::fake::Fake::new()),
        prompt_tmpl: String::new(),
        active_states: HashSet::new(),
        terminal_states: HashSet::new(),
        canceled_states: HashSet::new(),
        review_states: HashSet::new(),
        summon_token: String::new(),
        review_promote_state: String::new(),
        labels: HashSet::new(),
        capabilities: Vec::new(),
        per_state_limits: HashMap::new(),
        max_concurrent: 0,
        prompt_file: String::new(),
        git_flow: String::new(),
        workspace_mode: String::new(),
        pr_label: String::new(),
        dependency_mode: String::new(),
        dep_mode_prompt_file: String::new(),
        claim_mode: String::new(),
        claim_ttl: DEFAULT_CLAIM_TTL,
        claim_settle_delay: DEFAULT_CLAIM_SETTLE_DELAY,
        max_turns: 0,
        max_retry_backoff_ms: 0,
        poll_interval: Duration::ZERO,
        stall_timeout: Duration::ZERO,
        cpu_sampler: liveness::new_sampler(),
        log_dir: String::new(),
        transcripts: Arc::new(TranscriptStore::new(String::new())),
        projects: Vec::new(),
    }
}

/// A baseline [`ResolvedProject`] for `slug` (group defaults to slug) bound to `tracker`, with every
/// scheduling field zeroed and every infrastructure handle a cheap test double. Tests override the
/// `pub` scheduling fields — the analogue of Go's partial `resolvedProject{…}` literal.
pub(crate) fn empty_resolved_project(slug: &str, tracker: Arc<dyn Tracker>) -> ResolvedProject {
    let workspace = Arc::new(
        Manager::new(workspace::Config {
            root: "/tmp".to_string(),
            hooks: HookScripts::default(),
            hook_timeout: Duration::ZERO,
        })
        .expect("workspace manager"),
    );
    ResolvedProject {
        slug: slug.to_string(),
        group: slug.to_string(),
        repo: String::new(),
        name: String::new(),
        disabled: false,
        mcfg: minimal_config(),
        tracker,
        active_states: HashSet::new(),
        terminal_states: HashSet::new(),
        labels: HashSet::new(),
        capabilities: Vec::new(),
        canceled_states: HashSet::new(),
        review_states: HashSet::new(),
        per_state_limits: HashMap::new(),
        max_concurrent: 0,
        prompt_tmpl: String::new(),
        prompt_file: String::new(),
        stall_timeout: Duration::ZERO,
        git_flow: String::new(),
        workspace_mode: String::new(),
        dependency_mode: String::new(),
        dep_mode_prompt_file: String::new(),
        claim_mode: String::new(),
        model: String::new(),
        github_summons: false,
        gh_owner: String::new(),
        gh_repo: String::new(),
        agent: Arc::new(agent::fake::Fake::new()),
        workspace,
    }
}

/// A running entry carrying only the fields the selection pass reads (its issue + owning
/// project slug/group); every other field is a zero value. Mirrors the `runningEntry{issue: …,
/// projectSlug: …, projectGroup: …}` literals in `select_multi_test.go`.
pub(crate) fn running_entry(issue: Issue, project_slug: &str, project_group: &str) -> RunningEntry {
    let epoch = DateTime::from_timestamp(0, 0).expect("epoch");
    RunningEntry {
        issue,
        started_at: epoch,
        retry_attempt: 0,
        cancel: crate::control_loop::CancelSignal::default(),
        project_slug: project_slug.to_string(),
        project_group: project_group.to_string(),
        project_repo: String::new(),
        model: String::new(),
        stack_context: String::new(),
        capabilities_section: String::new(),
        identity: String::new(),
        teammate_section: String::new(),
        last_delivered_summon_at: epoch,
        thread_id: String::new(),
        session_id: String::new(),
        turn_count: 0,
        last_event: String::new(),
        last_message: String::new(),
        last_event_at: epoch,
        input_tokens: 0,
        output_tokens: 0,
        total_tokens: 0,
        cur_input_tokens: 0,
        cur_output_tokens: 0,
        cur_total_tokens: 0,
        run_id: 0,
        event_seq: 0,
        transcript_path: String::new(),
        pgid: 0,
        last_cpu_ticks: 0,
        cpu_sampled: false,
        last_cpu_active_at: epoch,
        recent_events: Vec::new(),
    }
}

/// An orchestrator wired to a fresh in-memory store, returning the shared store handle so a test can
/// [`seed_run`] history the orchestrator then reads back. Mirrors Go `orchWithStore` (minus the eff
/// setup the pr-suppression tests never read).
pub(crate) fn orch_with_store() -> (Orchestrator, Arc<dyn Store + Send + Sync>) {
    let store: Arc<dyn Store + Send + Sync> =
        Arc::new(Sqlite::open(StorePath::InMemory).expect("open in-memory store"));
    let mut o = Orchestrator::new("WORKFLOW.md");
    o.set_store(Arc::clone(&store));
    (o, store)
}

/// Seeds one completed run whose `started_at` is `ended_at - 1m` (Go `seedRun`), the summons-
/// suppression watermark the dispatch tests compare against.
pub(crate) fn seed_run(
    store: &dyn Store,
    issue_id: &str,
    identifier: &str,
    ended_at: DateTime<Utc>,
) {
    let started = ended_at - chrono::Duration::minutes(1);
    let id = store
        .start_run(RunStart {
            issue_id: issue_id.to_string(),
            issue_identifier: identifier.to_string(),
            title: "t".to_string(),
            started_at: started.to_rfc3339_opts(SecondsFormat::Secs, true),
            ..Default::default()
        })
        .expect("start_run");
    store
        .end_run(
            id,
            RunEnd {
                outcome: OUTCOME_COMPLETED.to_string(),
                ended_at: ended_at.to_rfc3339_opts(SecondsFormat::Secs, true),
                ..Default::default()
            },
        )
        .expect("end_run");
}

// --- O5 retry / reconcile / recovery builders (Go orchForRetry / orchForReconcile / recoveryOrch /
//     orchForRetryMulti helpers) ---------------------------------------------------------------------

/// A spawn recorder capturing the dispatched issue ids (Go `orchForRetry`'s `dispatched []string`).
pub(crate) type DispatchedIds = Arc<Mutex<Vec<String>>>;
/// A spawn recorder capturing the dispatched running entries (Go `orchForRetryMulti`'s
/// `dispatched []*runningEntry`).
pub(crate) type DispatchedEntries = Arc<Mutex<Vec<RunningEntry>>>;

/// A spawn seam that records each dispatched issue id into `sink`.
fn record_ids(sink: &DispatchedIds) -> crate::orchestrator::SpawnFn {
    let sink = Arc::clone(sink);
    Box::new(move |iss, _attempt, _re| sink.lock().expect("dispatched lock").push(iss.id.clone()))
}

/// A spawn seam that records each dispatched running entry into `sink`.
pub(crate) fn record_entries(sink: &DispatchedEntries) -> crate::orchestrator::SpawnFn {
    let sink = Arc::clone(sink);
    Box::new(move |_iss, _attempt, re| sink.lock().expect("dispatched lock").push(re.clone()))
}

/// Builds an orchestrator with a no-op spawn (recording dispatched ids) and a fake tracker, so state
/// handlers can be tested synchronously. Mirrors Go `orchForRetry`.
pub(crate) fn orch_for_retry(tr: Arc<Fake>, max_concurrent: i64) -> (Orchestrator, DispatchedIds) {
    let mut eff = empty_effective(tr);
    eff.active_states = set_of(&["todo", "in progress"]);
    eff.terminal_states = set_of(&["done"]);
    eff.max_concurrent = max_concurrent;
    eff.max_retry_backoff_ms = 300_000;
    eff.poll_interval = Duration::from_secs(3600);
    let mut o = Orchestrator::new("WORKFLOW.md");
    o.eff = Some(eff);
    let dispatched: DispatchedIds = Arc::new(Mutex::new(Vec::new()));
    o.spawn = Some(record_ids(&dispatched));
    (o, dispatched)
}

/// Builds a multi-project orchestrator with a spawn recording dispatched running entries and no
/// top-level tracker of consequence (routing is per-project). Mirrors Go `orchForRetryMulti`.
pub(crate) fn orch_for_retry_multi(
    projects: Vec<ResolvedProject>,
    global_max: i64,
) -> (Orchestrator, DispatchedEntries) {
    let mut eff = empty_effective(Arc::new(Fake::new()));
    eff.max_concurrent = global_max;
    eff.max_retry_backoff_ms = 300_000;
    eff.poll_interval = Duration::from_secs(3600);
    eff.projects = projects;
    let mut o = Orchestrator::new("WORKFLOW.md");
    o.eff = Some(eff);
    let dispatched: DispatchedEntries = Arc::new(Mutex::new(Vec::new()));
    o.spawn = Some(record_entries(&dispatched));
    (o, dispatched)
}

/// A single-slug resolved project (group == slug) bound to `tr`, active `{todo, in progress}`,
/// terminal `{done}`, cap 10. Mirrors Go `projWithTracker`.
pub(crate) fn proj_with_tracker(slug: &str, tr: Arc<Fake>, prompt: &str) -> ResolvedProject {
    let mut p = empty_resolved_project(slug, tr);
    p.active_states = set_of(&["todo", "in progress"]);
    p.terminal_states = set_of(&["done"]);
    p.max_concurrent = 10;
    p.prompt_tmpl = prompt.to_string();
    p
}

/// Builds a workspace [`Manager`] rooted at `root` (real filesystem, so create/remove actually touch
/// disk). Mirrors Go `workspace.NewManager` / `mkWS`.
pub(crate) fn mk_workspace(root: &str) -> Arc<Manager> {
    Arc::new(
        Manager::new(workspace::Config {
            root: root.to_string(),
            hooks: HookScripts::default(),
            hook_timeout: Duration::from_secs(1),
        })
        .expect("workspace manager"),
    )
}

/// Builds an orchestrator with a real workspace manager (rooted in the returned [`TempDir`]) and a
/// no-op spawn, for reconcile/liveness tests. Mirrors Go `orchForReconcile`. The [`TempDir`] is
/// returned so the caller keeps it alive for the test's duration.
pub(crate) fn orch_for_reconcile(tr: Arc<Fake>, stall: Duration) -> (Orchestrator, TempDir) {
    let dir = TempDir::new();
    let workspace = mk_workspace(&dir.path);
    let mut eff = empty_effective(tr);
    eff.workspace = workspace;
    eff.active_states = set_of(&["todo", "in progress"]);
    eff.terminal_states = set_of(&["done"]);
    eff.max_concurrent = 10;
    eff.max_retry_backoff_ms = 300_000;
    eff.stall_timeout = stall;
    let mut o = Orchestrator::new("WORKFLOW.md");
    o.eff = Some(eff);
    o.spawn = Some(Box::new(|_iss, _attempt, _re| {})); // no real worker
    (o, dir)
}

/// Inserts a running entry (issue + started_at, every other field zero) and claims it. Mirrors Go
/// `addRunning` (minus the worker-cancel handle, which is O7's).
pub(crate) fn add_running(
    o: &mut Orchestrator,
    id: &str,
    ident: &str,
    state: &str,
    started_at: DateTime<Utc>,
) {
    let mut re = running_entry(issue(id, ident, state), "", "");
    re.started_at = started_at;
    o.running.insert(id.to_string(), re);
    o.claimed.insert(id.to_string());
}

/// Builds an orchestrator on a caller-provided store with a recording no-op spawn and a fixed clock;
/// eff has no resolved projects (legacy path). Mirrors Go `recoveryOrch`.
pub(crate) fn recovery_orch(
    st: Arc<dyn Store + Send + Sync>,
    tr: Arc<Fake>,
    now: DateTime<Utc>,
) -> (Orchestrator, DispatchedIds) {
    let mut eff = empty_effective(tr);
    eff.active_states = set_of(&["todo", "in progress"]);
    eff.terminal_states = set_of(&["done"]);
    eff.max_concurrent = 10;
    eff.max_retry_backoff_ms = 300_000;
    eff.poll_interval = Duration::from_secs(3600);
    eff.stall_timeout = Duration::from_secs(60);
    let mut o = Orchestrator::new("WORKFLOW.md");
    o.set_store(st);
    o.now = Box::new(move || now);
    o.eff = Some(eff);
    let dispatched: DispatchedIds = Arc::new(Mutex::new(Vec::new()));
    o.spawn = Some(record_ids(&dispatched));
    (o, dispatched)
}

/// A UTC datetime, the Rust analogue of Go's `time.Date(y, mo, d, h, mi, s, 0, time.UTC)`.
pub(crate) fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> DateTime<Utc> {
    use chrono::TimeZone;
    Utc.with_ymd_and_hms(y, mo, d, h, mi, s)
        .single()
        .expect("valid utc datetime")
}

/// A pending retry entry with the given key fields, every other field at its zero value (Go's
/// `&retryEntry{issueID: …, identifier: …, attempt: …}` partial literals). Tests set `recovered` /
/// `issue` / `project_slug` on the returned value as needed.
pub(crate) fn retry_entry(issue_id: &str, identifier: &str, attempt: i64) -> RetryEntry {
    RetryEntry {
        issue_id: issue_id.to_string(),
        identifier: identifier.to_string(),
        attempt,
        due_at: DateTime::from_timestamp(0, 0).expect("epoch"),
        err: String::new(),
        project_slug: String::new(),
        project_repo: String::new(),
        issue: Issue::default(),
        due_at_ms: 0,
        recovered: false,
    }
}

/// A scripted CPU sampler returning per-pgid tick values (Go `fakeSampler`). `ok == false` models an
/// unreadable `/proc` (→ `None`); a pgid with no scripted sequence returns `Some(0)`; a sequence is
/// walked and then clamped to its last value.
pub(crate) struct FakeSampler {
    ok: bool,
    seq: HashMap<i32, Vec<u64>>,
    idx: Mutex<HashMap<i32, usize>>,
}

impl FakeSampler {
    pub(crate) fn new(ok: bool, seq: HashMap<i32, Vec<u64>>) -> FakeSampler {
        FakeSampler {
            ok,
            seq,
            idx: Mutex::new(HashMap::new()),
        }
    }
}

impl Sampler for FakeSampler {
    fn group_cpu(&self, pgid: i32) -> Option<u64> {
        if !self.ok {
            return None;
        }
        let Some(s) = self.seq.get(&pgid) else {
            return Some(0);
        };
        if s.is_empty() {
            return Some(0);
        }
        let mut idx = self.idx.lock().expect("sampler idx lock");
        let entry = idx.entry(pgid).or_insert(0);
        let read = (*entry).min(s.len() - 1);
        *entry += 1;
        Some(s[read])
    }
}

// --- tracing capture (Go's slog buffer analogue) ----------------------------------------------

use tracing::field::{Field, Visit};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;

/// One captured `tracing` event: its message and its string-rendered fields.
#[derive(Debug, Clone, Default)]
pub(crate) struct CapturedEvent {
    pub message: String,
    pub fields: HashMap<String, String>,
}

#[derive(Default)]
struct EventVisitor {
    message: String,
    fields: HashMap<String, String>,
}

impl EventVisitor {
    fn put(&mut self, name: &str, value: String) {
        if name == "message" {
            self.message = value;
        } else {
            self.fields.insert(name.to_string(), value);
        }
    }
}

impl Visit for EventVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.put(field.name(), value.to_string());
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        // `%`-formatted (Display) values and the message arrive here as `DisplayValue`/`Arguments`,
        // whose Debug rendering IS the Display text (no surrounding quotes) — the field values the
        // tests assert on.
        self.put(field.name(), format!("{value:?}"));
    }
}

struct RecordingLayer {
    events: Arc<Mutex<Vec<CapturedEvent>>>,
}

impl<S: tracing::Subscriber + for<'a> LookupSpan<'a>> Layer<S> for RecordingLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let mut v = EventVisitor::default();
        event.record(&mut v);
        self.events
            .lock()
            .expect("event buffer lock")
            .push(CapturedEvent {
                message: v.message,
                fields: v.fields,
            });
    }
}

/// Serializes every test that installs a `tracing` subscriber and asserts on captured events
/// (TRA-243). `tracing` caches per-callsite Interest GLOBALLY, so two such tests running on parallel
/// harness threads can poison each other's cache and intermittently drop events. Async tests hold
/// this across their whole body (`lock().await`); the sync [`capture_events`] holds it via
/// `blocking_lock()`. A tokio mutex (already in the tree) is used so the async holders don't trip
/// clippy's `await_holding_lock`. Every holder calls [`tracing::callsite::rebuild_interest_cache`]
/// under the lock so its callsites re-evaluate Interest against ITS subscriber, not a stale entry.
pub(crate) static TRACING_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Runs `f` with a recording subscriber installed as the thread-local default, returning both `f`'s
/// result and every event it emitted, in order — the analogue of Go capturing `slog` output to a
/// `bytes.Buffer` while keeping the call's return value.
pub(crate) fn capture_events<R, F: FnOnce() -> R>(f: F) -> (R, Vec<CapturedEvent>) {
    let _serial = TRACING_TEST_LOCK.blocking_lock(); // TRA-243: no concurrent subscriber test
    let events = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry().with(RecordingLayer {
        events: Arc::clone(&events),
    });
    let result = tracing::subscriber::with_default(subscriber, || {
        tracing::callsite::rebuild_interest_cache();
        f()
    });
    let captured = events.lock().expect("event buffer lock").clone();
    (result, captured)
}

/// The recording subscriber plus the shared buffer it fills, for capturing events across an ASYNC
/// call: install it with [`tracing::subscriber::set_default`] (whose guard holds the default across
/// `.await` on a current-thread runtime), drive the future, drop the guard, then read the buffer. The
/// sync [`capture_events`] cannot wrap a future, so the async worker/loop tests use this instead —
/// mirroring the `set_default` pattern the loop's span test already uses.
pub(crate) fn recording_subscriber() -> (
    Arc<Mutex<Vec<CapturedEvent>>>,
    impl tracing::Subscriber + Send + Sync,
) {
    let events = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry().with(RecordingLayer {
        events: Arc::clone(&events),
    });
    (events, subscriber)
}

/// Convenience: how many captured events carry the given message.
pub(crate) fn count_messages(events: &[CapturedEvent], message: &str) -> usize {
    events.iter().filter(|e| e.message == message).count()
}

// --- temp directories (Go's t.TempDir() analogue) --------------------------------------------

static TEST_DIR_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// RAII temp directory mirroring Go's `t.TempDir()` (unique per pid+counter, auto-removed on drop).
/// The O3 worker / obslog / workspace-GC tests provision real filesystem roots the way the Go tests
/// do; the sibling crates roll the same tiny helper rather than take a `tempfile` dependency, so this
/// matches the workspace crate's `testutil::TempDir`.
pub(crate) struct TempDir {
    pub path: String,
}

impl TempDir {
    pub(crate) fn new() -> TempDir {
        let n = TEST_DIR_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "rhapsody-orchestrator-{}-{}",
            std::process::id(),
            n
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        TempDir {
            path: path.to_string_lossy().into_owned(),
        }
    }

    /// Joins `name` under this directory, returning the path string.
    pub(crate) fn child(&self, name: &str) -> String {
        std::path::Path::new(&self.path)
            .join(name)
            .to_string_lossy()
            .into_owned()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
