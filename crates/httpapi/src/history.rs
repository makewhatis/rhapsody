//! history — the read-only store surface the history API depends on. Parity port of Go
//! `$REF/internal/httpapi/history.go` (the `HistoryStore` interface).
//!
//! Go narrows the orchestrator's full `store.Store` to this read-only subset so the handlers can
//! never write; the orchestrator's `Store()` returns the full store and `api.History()` narrows it.
//! The Rust port models the same narrowing with a trait + a blanket impl over every
//! [`rhapsody_store::Store`], so a real [`rhapsody_store::Sqlite`]/[`rhapsody_store::Noop`] is usable
//! as an `Arc<dyn HistoryStore>` without any adapter — the analog of Go's compile-time
//! `var _ HistoryStore = (store.Store)(nil)` assertion.
//!
//! Narrowed to the H2 read slice plus H3's `list_run_messages` (Go's `HistoryStore.ListRunMessages`,
//! INF-250) — consumed only by the run-messages handler in this write lane, so it lands with that
//! handler rather than earlier.

use rhapsody_store::{
    DayRollup, DayTotals, EventHit, EventQuery, EventRow, RunFilter, RunMessage, RunSummary,
    StoreError,
};

/// The read-only subset of [`rhapsody_store::Store`] the history endpoints query. Never writes; the
/// handlers only list recent runs, an issue's run history, a run's events, a cross-run event search,
/// and per-day metrics. A [`rhapsody_store::Noop`] satisfies it (returning empty lists), so a daemon
/// started with persistence disabled serves empty history rather than erroring. Mirrors Go
/// `HistoryStore`.
pub trait HistoryStore: Send + Sync {
    /// Paged/filterable recent runs (`GET /api/v1/history`). Mirrors Go `ListRuns`.
    fn list_runs(&self, f: RunFilter) -> Result<Vec<RunSummary>, StoreError>;
    /// One row per issue — each issue's latest matching run, paged by ISSUE
    /// (`GET /api/v1/history/issues`, TRA-320). Rhapsody-only; Go has no issue-level listing.
    fn list_issue_runs(&self, f: RunFilter) -> Result<Vec<RunSummary>, StoreError>;
    /// Whole-store run/token/runtime totals over a window (`GET /api/v1/history/summary`,
    /// TRA-320). Rhapsody-only; Go has no day-summary endpoint.
    fn day_totals(&self, since: &str, now: &str) -> Result<DayTotals, StoreError>;
    /// A single issue's run history, most-recent first (`GET /api/v1/issues/{id}/history`). Mirrors
    /// Go `IssueHistory`.
    fn issue_history(
        &self,
        identifier: &str,
        project: &str,
        limit: i64,
    ) -> Result<Vec<RunSummary>, StoreError>;
    /// A single run row by id — `Ok(None)` (not an error) when no such run exists, so the unified
    /// run-detail endpoint can answer 404 without treating "missing" as an error. Mirrors Go `GetRun`.
    fn get_run(&self, run_id: i64) -> Result<Option<RunSummary>, StoreError>;
    /// A run's captured events, ordered by seq (`GET /api/v1/runs/{id}/events`). Mirrors Go `RunEvents`.
    fn run_events(&self, run_id: i64) -> Result<Vec<EventRow>, StoreError>;
    /// A cross-run substring search over event text (`GET /api/v1/events`). Mirrors Go `SearchEvents`.
    fn search_events(&self, q: EventQuery) -> Result<Vec<EventHit>, StoreError>;
    /// Per-day run/success/token rollups over the last N days (`GET /api/v1/metrics`). Mirrors Go
    /// `Metrics`.
    fn metrics(&self, since_days: i64, project: &str) -> Result<Vec<DayRollup>, StoreError>;
    /// A run's operator messages with their delivery status, oldest first
    /// (`GET /api/v1/runs/{id}/messages`, INF-250). Mirrors Go `ListRunMessages`.
    fn list_run_messages(&self, run_id: i64) -> Result<Vec<RunMessage>, StoreError>;
}

/// Every thread-safe [`rhapsody_store::Store`] is a [`HistoryStore`] — the Rust analog of Go's
/// compile-time `var _ HistoryStore = (store.Store)(nil)`. Each method forwards to the full store's
/// read side, so the orchestrator's `Sqlite`/`Noop` are handed to the API unchanged. `Send + Sync`
/// is required because the handlers hold the store as an `Arc<dyn HistoryStore>` shared across async
/// tasks (both `Sqlite` — a `Mutex<Connection>` — and `Noop` satisfy it); `?Sized` covers both a
/// concrete `Sqlite`/`Noop` and a `dyn Store`.
impl<S: rhapsody_store::Store + Send + Sync + ?Sized> HistoryStore for S {
    fn list_runs(&self, f: RunFilter) -> Result<Vec<RunSummary>, StoreError> {
        rhapsody_store::Store::list_runs(self, f)
    }
    fn list_issue_runs(&self, f: RunFilter) -> Result<Vec<RunSummary>, StoreError> {
        rhapsody_store::Store::list_issue_runs(self, f)
    }
    fn day_totals(&self, since: &str, now: &str) -> Result<DayTotals, StoreError> {
        rhapsody_store::Store::day_totals(self, since, now)
    }
    fn issue_history(
        &self,
        identifier: &str,
        project: &str,
        limit: i64,
    ) -> Result<Vec<RunSummary>, StoreError> {
        rhapsody_store::Store::issue_history(self, identifier, project, limit)
    }
    fn get_run(&self, run_id: i64) -> Result<Option<RunSummary>, StoreError> {
        rhapsody_store::Store::get_run(self, run_id)
    }
    fn run_events(&self, run_id: i64) -> Result<Vec<EventRow>, StoreError> {
        rhapsody_store::Store::run_events(self, run_id)
    }
    fn search_events(&self, q: EventQuery) -> Result<Vec<EventHit>, StoreError> {
        rhapsody_store::Store::search_events(self, q)
    }
    fn metrics(&self, since_days: i64, project: &str) -> Result<Vec<DayRollup>, StoreError> {
        rhapsody_store::Store::metrics(self, since_days, project)
    }
    fn list_run_messages(&self, run_id: i64) -> Result<Vec<RunMessage>, StoreError> {
        rhapsody_store::Store::list_run_messages(self, run_id)
    }
}
