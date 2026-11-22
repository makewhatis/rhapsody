//! dockerreap — the daemon's Docker-leftover reaper scheduler (STUDIO-2316). Rhapsody-only; no Go
//! counterpart.
//!
//! Runs the orchestrator's reaper (`ControlHandle::reap_docker`) an hour after startup and then
//! daily until `ctx` is cancelled. The first pass waits rather than firing at boot: a restart is
//! when the running set is emptiest and restart recovery is still re-adopting runs, so an immediate
//! pass would judge containers against the least-informed view the daemon ever has. The reaper's
//! workspace-cleanup half already removes a run's containers promptly; this is the backstop.

use std::future::Future;
use std::time::Duration;

use rhapsody_orchestrator::CancelWait;
use rhapsody_orchestrator::dockercleanup::{DOCKER_REAP_INTERVAL, ReapReport};

/// How long after startup the first pass waits.
const FIRST_REAP_DELAY: Duration = Duration::from_secs(3600);

/// Runs `reap` an hour after startup, then every [`DOCKER_REAP_INTERVAL`], until `ctx` is cancelled.
pub async fn run_docker_reap_schedule<R, Fut>(ctx: CancelWait, reap: R)
where
    R: Fn() -> Fut,
    Fut: Future<Output = ReapReport>,
{
    run_with(ctx, FIRST_REAP_DELAY, DOCKER_REAP_INTERVAL, reap).await;
}

async fn run_with<R, Fut>(mut ctx: CancelWait, first: Duration, every: Duration, reap: R)
where
    R: Fn() -> Fut,
    Fut: Future<Output = ReapReport>,
{
    let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + first, every);
    loop {
        tokio::select! {
            _ = ctx.cancelled() => return,
            _ = ticker.tick() => {}
        }
        let report = tokio::select! {
            biased;
            _ = ctx.cancelled() => return,
            report = reap() => report,
        };
        if !report.removed.is_empty() {
            tracing::info!(
                removed = report.removed.len(),
                dry_run = report.dry_run,
                "docker reaper finished a pass"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI32, Ordering};

    use rhapsody_orchestrator::CancelSignal;

    fn counting(calls: &Arc<AtomicI32>) -> impl Fn() -> std::future::Ready<ReapReport> + use<> {
        let calls = Arc::clone(calls);
        move || {
            calls.fetch_add(1, Ordering::SeqCst);
            std::future::ready(ReapReport::default())
        }
    }

    /// Nothing is reaped at boot; the first pass waits for its delay, and cancel stops the task.
    #[tokio::test]
    async fn the_first_pass_waits_and_the_scheduler_stops_on_cancel() {
        let calls = Arc::new(AtomicI32::new(0));
        let signal = CancelSignal::new();
        let task = tokio::spawn(run_with(
            signal.wait(),
            Duration::from_millis(100),
            Duration::from_secs(3600),
            counting(&calls),
        ));
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 0, "no pass at boot");
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1, "one pass after the delay");
        signal.cancel();
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("the docker reaper must stop on cancel")
            .expect("task join");
    }
}
