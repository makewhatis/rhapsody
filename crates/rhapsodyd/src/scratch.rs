//! scratch — the daemon's scratch-directory janitor scheduler (STUDIO-1993). Rhapsody-only; no Go
//! counterpart.
//!
//! Runs the scratch janitor once at startup and then hourly until `ctx` is cancelled. The sweep
//! itself runs OFF the control loop: each cycle round-trips a race-free plan (the scratch policy +
//! the live run set) through the control channel, exactly as the prune scheduler's worktree GC does.
//! Both roots are swept — the daemon-owned scratch root and the historical `$TMPDIR/opencode` — and
//! every removal is logged with its size by the orchestrator's sweep driver.

use std::future::Future;
use std::time::Duration;

use rhapsody_orchestrator::CancelWait;
use rhapsody_orchestrator::scratch::SweepReport;

/// Runs the janitor once at startup, then hourly until `ctx` is cancelled. `sweep` performs one
/// cycle (round-tripping the plan and removing the stale directories) and returns what it removed.
pub async fn run_scratch_schedule<S, Fut>(mut ctx: CancelWait, sweep: S)
where
    S: Fn() -> Fut,
    Fut: Future<Output = SweepReport>,
{
    let hour = Duration::from_secs(3600);
    // Fire once at startup (the explicit cycle below), then hourly — matching the prune scheduler's
    // shape so the first sweep does not wait an hour.
    let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + hour, hour);
    loop {
        if !ctx.is_cancelled() {
            let report = tokio::select! {
                biased;
                _ = ctx.cancelled() => return,
                report = sweep() => report,
            };
            if report.total() > 0 {
                tracing::info!(
                    removed = report.total(),
                    "scratch janitor removed stale scratch directories"
                );
            }
        }
        tokio::select! {
            _ = ctx.cancelled() => return,
            _ = ticker.tick() => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI32, Ordering};

    use rhapsody_orchestrator::CancelSignal;

    /// One startup cycle runs immediately, and the scheduler stops promptly on cancel.
    #[tokio::test]
    async fn the_scheduler_sweeps_once_at_startup_and_stops_on_cancel() {
        let calls = Arc::new(AtomicI32::new(0));
        let signal = CancelSignal::new();
        let task = tokio::spawn({
            let ctx = signal.wait();
            let calls = Arc::clone(&calls);
            async move {
                run_scratch_schedule(ctx, move || {
                    let calls = Arc::clone(&calls);
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        SweepReport::default()
                    }
                })
                .await;
            }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        signal.cancel();
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("the scratch scheduler must stop on cancel")
            .expect("task join");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "the startup cycle must sweep exactly once"
        );
    }
}
