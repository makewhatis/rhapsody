//! prune — the daemon's history + worktree prune scheduler (parity port of `runPruneSchedule` in
//! `$REF/cmd/symphony/run.go`).
//!
//! Prunes history AND stale worktrees once at startup and then daily until `ctx` is cancelled. The
//! store handle + retention are read lazily each cycle so a hot-reloaded `retention_days` applies and
//! the store is the real one once opened. The STARTUP worktree GC is gated on `retention_loaded`: the
//! first cycle can fire before the orchestrator's startup reload stores the configured retention,
//! when `retention_fn()` would still return the `New()` default (30) — pruning worktrees against that
//! default could delete idle worktrees sooner than a larger configured retention, so the startup
//! cycle SKIPS worktree GC until retention is loaded (history prune still runs — a Noop store's Prune
//! is a harmless no-op). Daily ticks always run both prunes.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use rhapsody_orchestrator::CancelWait;
use rhapsody_store::Store;

/// Runs the prune cycle once at startup then daily until `ctx` is cancelled. `store_fn` is read
/// lazily each cycle (so it picks up the real store after the orchestrator opens it, and a Noop
/// store's `Prune` is a harmless no-op); `retention_fn` is read each cycle so a hot-reloaded
/// `retention_days` applies; `prune_workspaces(days)` removes per-issue worktrees idle beyond the
/// window (returning the count removed); `retention_loaded` gates the STARTUP worktree GC. Mirrors Go
/// `runPruneSchedule`.
pub async fn run_prune_schedule<SF, RF, PW, Fut, RL>(
    mut ctx: CancelWait,
    store_fn: SF,
    retention_fn: RF,
    prune_workspaces: PW,
    retention_loaded: RL,
) where
    SF: Fn() -> Arc<dyn Store + Send + Sync>,
    RF: Fn() -> i64,
    PW: Fn(i64) -> Fut,
    Fut: Future<Output = usize>,
    RL: Fn() -> bool,
{
    let day = Duration::from_secs(24 * 3600);
    // Go's `time.NewTicker(24h)` fires first after 24h; `interval_at(now + day, day)` matches (the
    // startup cycle below runs explicitly before the first tick).
    let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + day, day);

    let mut startup = true;
    loop {
        // One prune cycle (Go's `prune(startup)`).
        if !ctx.is_cancelled() {
            let days = retention_fn();
            if days <= 0 {
                // retention_days <= 0 means "keep forever" (Prune(days) is a no-op); skip so we don't
                // log a misleading "pruned old history" every cycle.
                tracing::debug!(retention_days = days, "retention disabled; skipping prune");
            } else {
                match store_fn().prune(days) {
                    Ok(()) => tracing::info!(retention_days = days, "pruned old history"),
                    Err(e) => {
                        tracing::error!(retention_days = days, err = %e, "prune failed")
                    }
                }
                // Worktree GC shares the retention window. Skip it on the startup cycle if the
                // configured retention hasn't loaded yet (days would be the New() default), so we
                // never prune worktrees against a default shorter than the configured window.
                if startup && !retention_loaded() {
                    tracing::debug!(
                        "startup prune: retention not loaded yet; skipping worktree GC this cycle"
                    );
                } else {
                    prune_workspaces(days).await;
                }
            }
        }
        startup = false;

        tokio::select! {
            _ = ctx.cancelled() => return,
            _ = ticker.tick() => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicI32, Ordering};
    use std::time::Duration as StdDuration;

    use rhapsody_orchestrator::CancelSignal;
    use rhapsody_store::{Sqlite, StorePath};

    fn mem_store() -> Arc<dyn Store + Send + Sync> {
        Arc::new(Sqlite::open(StorePath::InMemory).expect("open in-memory store"))
    }

    // Mirrors Go `TestPruneScheduleRunsOnceOnFreshDB`: the scheduler's startup prune runs without
    // error against a fresh store, and it stops promptly on ctx cancel.
    #[tokio::test]
    async fn prune_schedule_runs_once_on_fresh_db() {
        let st = mem_store();
        let signal = CancelSignal::new();
        let task = tokio::spawn({
            let ctx = signal.wait();
            let st = Arc::clone(&st);
            async move {
                run_prune_schedule(
                    ctx,
                    move || Arc::clone(&st),
                    || 30,
                    |_| async { 0 },
                    || true,
                )
                .await;
            }
        });
        // Give the startup prune a moment, then cancel.
        tokio::time::sleep(StdDuration::from_millis(50)).await;
        signal.cancel();
        tokio::time::timeout(StdDuration::from_secs(2), task)
            .await
            .expect("prune scheduler did not stop on ctx cancel")
            .expect("prune task join");
    }

    // Mirrors Go `TestPruneScheduleSkipsStartupWorktreeGCUntilRetentionLoaded`: when retention has NOT
    // loaded, the FIRST (startup) cycle prunes history but must NOT run the worktree GC; once
    // retention is loaded, the startup cycle runs it once.
    #[tokio::test]
    async fn prune_schedule_skips_startup_worktree_gc_until_retention_loaded() {
        let st = mem_store();
        let ws_calls = Arc::new(AtomicI32::new(0));

        // retention NOT loaded on the startup cycle → worktree GC skipped.
        let signal = CancelSignal::new();
        let task = tokio::spawn({
            let ctx = signal.wait();
            let st = Arc::clone(&st);
            let calls = Arc::clone(&ws_calls);
            async move {
                run_prune_schedule(
                    ctx,
                    move || Arc::clone(&st),
                    || 30,
                    move |_| {
                        let c = Arc::clone(&calls);
                        async move {
                            c.fetch_add(1, Ordering::SeqCst);
                            0
                        }
                    },
                    || false,
                )
                .await;
            }
        });
        tokio::time::sleep(StdDuration::from_millis(50)).await;
        signal.cancel();
        let _ = task.await;
        assert_eq!(
            ws_calls.load(Ordering::SeqCst),
            0,
            "startup worktree GC must be skipped until retention loads"
        );

        // retention loaded → the startup cycle runs the worktree GC once.
        ws_calls.store(0, Ordering::SeqCst);
        let signal2 = CancelSignal::new();
        let task2 = tokio::spawn({
            let ctx = signal2.wait();
            let st = Arc::clone(&st);
            let calls = Arc::clone(&ws_calls);
            async move {
                run_prune_schedule(
                    ctx,
                    move || Arc::clone(&st),
                    || 30,
                    move |_| {
                        let c = Arc::clone(&calls);
                        async move {
                            c.fetch_add(1, Ordering::SeqCst);
                            0
                        }
                    },
                    || true,
                )
                .await;
            }
        });
        tokio::time::sleep(StdDuration::from_millis(50)).await;
        signal2.cancel();
        let _ = task2.await;
        assert_eq!(
            ws_calls.load(Ordering::SeqCst),
            1,
            "startup worktree GC must run once when retention is loaded"
        );
    }
}
