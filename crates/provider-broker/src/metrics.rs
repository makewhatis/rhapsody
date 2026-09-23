//! Bounded broker metrics (design §13).
//!
//! Every counter carries no label at all, so no random session/run/token identifier can ever become
//! a metric dimension; cardinality is therefore fixed by construction. The counters mirror the
//! useful set in the design: admitted, denied, forwarded, upstream status class, unknown usage,
//! bytes, tokens, revocations, and active requests. A later slice surfaces them in provider status;
//! this crate only records them.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

/// The broker's bounded counters. Shared behind an `Arc` so a request-lifetime guard can decrement
/// the active count on drop.
#[derive(Debug, Default)]
pub struct BrokerMetrics {
    admitted_requests: AtomicU64,
    denied_requests: AtomicU64,
    forwarded_requests: AtomicU64,
    reported_requests: AtomicU64,
    unknown_usage_requests: AtomicU64,
    inconsistent_usage_requests: AtomicU64,
    request_bytes: AtomicU64,
    response_bytes: AtomicU64,
    reserved_tokens: AtomicU64,
    provider_reported_tokens: AtomicU64,
    revocations: AtomicU64,
    active_requests: AtomicI64,
    upstream_2xx: AtomicU64,
    upstream_3xx: AtomicU64,
    upstream_4xx: AtomicU64,
    upstream_5xx: AtomicU64,
    upstream_other: AtomicU64,
}

/// A point-in-time, non-secret copy of the counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BrokerMetricsSnapshot {
    /// Authenticated requests admitted to the outbound path.
    pub admitted_requests: u64,
    /// Authenticated requests refused locally (schema, budget, or protocol).
    pub denied_requests: u64,
    /// Upstream requests constructed.
    pub forwarded_requests: u64,
    /// Forwarded requests that settled with a valid provider report.
    pub reported_requests: u64,
    /// Forwarded requests that never produced usable usage and stayed conservatively charged.
    pub unknown_usage_requests: u64,
    /// Valid reports whose total and components disagreed (a bounded diagnostic).
    pub inconsistent_usage_requests: u64,
    /// Admitted request bytes.
    pub request_bytes: u64,
    /// Forwarded response bytes.
    pub response_bytes: u64,
    /// Token units reserved against the turn/session/optional day caps.
    pub reserved_tokens: u64,
    /// Conservative provider-reported token total.
    pub provider_reported_tokens: u64,
    /// Turn/denial-threshold revocations.
    pub revocations: u64,
    /// Requests currently admitted and in flight.
    pub active_requests: i64,
    /// Upstream responses by status class.
    pub upstream_2xx: u64,
    /// Upstream responses by status class.
    pub upstream_3xx: u64,
    /// Upstream responses by status class.
    pub upstream_4xx: u64,
    /// Upstream responses by status class.
    pub upstream_5xx: u64,
    /// Upstream responses with a status class outside 2xx-5xx.
    pub upstream_other: u64,
}

impl BrokerMetrics {
    /// A fresh, zeroed counter set.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Record one authenticated request admitted to the outbound path.
    pub fn record_admitted(&self, request_bytes: u64) {
        self.admitted_requests.fetch_add(1, Ordering::Relaxed);
        self.request_bytes
            .fetch_add(request_bytes, Ordering::Relaxed);
    }

    /// Record one locally denied authenticated request.
    pub fn record_denied(&self) {
        self.denied_requests.fetch_add(1, Ordering::Relaxed);
    }

    /// Record one upstream request construction and its token reservation.
    pub fn record_forwarded(&self, reserved_tokens: u64) {
        self.forwarded_requests.fetch_add(1, Ordering::Relaxed);
        self.reserved_tokens
            .fetch_add(reserved_tokens, Ordering::Relaxed);
    }

    /// Record one settled provider report (measurement only).
    pub fn record_reported(&self, tokens: u64) {
        self.reported_requests.fetch_add(1, Ordering::Relaxed);
        self.provider_reported_tokens
            .fetch_add(tokens, Ordering::Relaxed);
    }

    /// Record one internally inconsistent provider report (a bounded diagnostic).
    pub fn record_inconsistent_usage(&self) {
        self.inconsistent_usage_requests
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Record one forwarded request that settled without usable usage.
    pub fn record_unknown_usage(&self) {
        self.unknown_usage_requests.fetch_add(1, Ordering::Relaxed);
    }

    /// Record forwarded response bytes.
    pub fn record_response_bytes(&self, bytes: u64) {
        self.response_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Record one revocation (explicit revoke, expiry, or the denial-threshold abuse revocation).
    pub fn record_revocation(&self) {
        self.revocations.fetch_add(1, Ordering::Relaxed);
    }

    /// Record an upstream response by status class.
    pub fn record_upstream_status(&self, status: u16) {
        let counter = match status / 100 {
            2 => &self.upstream_2xx,
            3 => &self.upstream_3xx,
            4 => &self.upstream_4xx,
            5 => &self.upstream_5xx,
            _ => &self.upstream_other,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// Enter an in-flight request; the returned guard decrements the active count on drop.
    pub fn enter_request(self: &Arc<Self>) -> ActiveRequestGuard {
        self.active_requests.fetch_add(1, Ordering::Relaxed);
        ActiveRequestGuard {
            metrics: Arc::clone(self),
        }
    }

    /// The current counters.
    pub fn snapshot(&self) -> BrokerMetricsSnapshot {
        BrokerMetricsSnapshot {
            admitted_requests: self.admitted_requests.load(Ordering::Relaxed),
            denied_requests: self.denied_requests.load(Ordering::Relaxed),
            forwarded_requests: self.forwarded_requests.load(Ordering::Relaxed),
            reported_requests: self.reported_requests.load(Ordering::Relaxed),
            unknown_usage_requests: self.unknown_usage_requests.load(Ordering::Relaxed),
            inconsistent_usage_requests: self.inconsistent_usage_requests.load(Ordering::Relaxed),
            request_bytes: self.request_bytes.load(Ordering::Relaxed),
            response_bytes: self.response_bytes.load(Ordering::Relaxed),
            reserved_tokens: self.reserved_tokens.load(Ordering::Relaxed),
            provider_reported_tokens: self.provider_reported_tokens.load(Ordering::Relaxed),
            revocations: self.revocations.load(Ordering::Relaxed),
            active_requests: self.active_requests.load(Ordering::Relaxed),
            upstream_2xx: self.upstream_2xx.load(Ordering::Relaxed),
            upstream_3xx: self.upstream_3xx.load(Ordering::Relaxed),
            upstream_4xx: self.upstream_4xx.load(Ordering::Relaxed),
            upstream_5xx: self.upstream_5xx.load(Ordering::Relaxed),
            upstream_other: self.upstream_other.load(Ordering::Relaxed),
        }
    }
}

/// Keeps the active-request count truthful for the request's whole lifetime.
#[derive(Debug)]
pub struct ActiveRequestGuard {
    metrics: Arc<BrokerMetrics>,
}

impl Drop for ActiveRequestGuard {
    fn drop(&mut self) {
        self.metrics.active_requests.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_accumulate_and_the_active_guard_is_balanced() {
        let metrics = BrokerMetrics::new();
        metrics.record_admitted(120);
        metrics.record_forwarded(50);
        metrics.record_reported(10);
        metrics.record_unknown_usage();
        metrics.record_denied();
        metrics.record_revocation();
        metrics.record_upstream_status(200);
        metrics.record_upstream_status(429);
        metrics.record_upstream_status(503);
        metrics.record_upstream_status(101);

        let guard = metrics.enter_request();
        assert_eq!(metrics.snapshot().active_requests, 1);
        drop(guard);

        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.active_requests, 0);
        assert_eq!(snapshot.admitted_requests, 1);
        assert_eq!(snapshot.request_bytes, 120);
        assert_eq!(snapshot.forwarded_requests, 1);
        assert_eq!(snapshot.reserved_tokens, 50);
        assert_eq!(snapshot.reported_requests, 1);
        assert_eq!(snapshot.provider_reported_tokens, 10);
        assert_eq!(snapshot.unknown_usage_requests, 1);
        assert_eq!(snapshot.denied_requests, 1);
        assert_eq!(snapshot.revocations, 1);
        assert_eq!(snapshot.upstream_2xx, 1);
        assert_eq!(snapshot.upstream_4xx, 1);
        assert_eq!(snapshot.upstream_5xx, 1);
        assert_eq!(snapshot.upstream_other, 1);
    }

    #[test]
    fn the_snapshot_carries_no_identifier_dimension() {
        // The snapshot is a fixed struct of counters: there is no label map, so no random
        // session/run/token identifier can ever be a metric dimension.
        let snapshot = BrokerMetrics::new().snapshot();
        assert_eq!(snapshot, BrokerMetricsSnapshot::default());
    }
}
