//! liveness — orchestrator-internal port of Go `internal/liveness`.
//!
//! Reports whether an agent's process group is doing work, used (from O5 onward) to tell a
//! quietly-working run apart from a wedged one. Go's package has no dedicated Rust crate, so it
//! lives here — the orchestrator is its sole consumer.
//!
//! O1 ports the [`Sampler`] trait and the platform selector [`new_sampler`]; the effective builder
//! constructs the sampler and probes it once at build time. On the macOS target (and any non-Linux
//! OS) `new_sampler` returns the no-op stub — the frozen v0.4.0 behavior under Go build tag
//! `!linux` (`sampler_stub.go`). The `/proc`-reading Linux sampler (`sampler_linux.go`) is a later
//! concern (O5 stall detection), gated behind the same trait; the CI/target platform is macOS, so
//! the stub is the parity-correct sampler here.

use std::sync::Arc;

/// Reports cumulative process-group CPU usage. Mirrors Go `liveness.Sampler`.
pub trait Sampler: Send + Sync {
    /// Returns the summed user+system CPU time, in clock ticks, of every process whose
    /// process-group id equals `pgid`. `None` when the value cannot be read (e.g. no readable
    /// `/proc` on this OS, or `/proc` is unreadable), so the orchestrator degrades to "assume
    /// alive". A readable group with no live members returns `Some(0)`. Mirrors Go
    /// `GroupCPU(pgid int) (ticks uint64, ok bool)` — the `(u64, bool)` tuple collapses to
    /// `Option<u64>`.
    fn group_cpu(&self, pgid: i32) -> Option<u64>;
}

/// The non-Linux sampler: [`Sampler::group_cpu`] always reports `None`. Mirrors Go `stubSampler`.
struct StubSampler;

impl Sampler for StubSampler {
    fn group_cpu(&self, _pgid: i32) -> Option<u64> {
        None
    }
}

/// Returns the platform sampler. On the macOS target (and any non-Linux OS) this is the no-op
/// stub; mirrors Go `liveness.NewSampler` selecting `stubSampler` under `!linux`.
pub fn new_sampler() -> Arc<dyn Sampler> {
    Arc::new(StubSampler)
}

#[cfg(test)]
mod tests {
    use super::*;

    // The stub sampler always reports unreadable (Go `stubSampler.GroupCPU` → `(0, false)`), so
    // stall detection degrades to "assume alive" on the macOS target.
    #[test]
    fn stub_sampler_reports_unreadable() {
        let s = new_sampler();
        assert_eq!(s.group_cpu(std::process::id() as i32), None);
        assert_eq!(s.group_cpu(1), None);
    }
}
