//! metrics — Rhapsody's OpenTelemetry instruments. Parity port of Go `telemetry.Metrics`.
//!
//! Bounded metric label keys (the cardinality contract): metric attributes are restricted to these
//! low-cardinality dimensions ONLY. NEVER put issue/run/session identifiers on a metric — those are
//! unbounded and belong on spans and logs. Mirrors `$REF/internal/telemetry/metrics.go`.

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Counter, Gauge, Histogram, Meter, MeterProvider};
use opentelemetry_sdk::metrics::SdkMeterProvider;

/// Metric attribute key: the owning project's slug. Mirrors Go `AttrProject`.
pub const ATTR_PROJECT: &str = "project";
/// Metric attribute key: the claude model. Mirrors Go `AttrModel`.
pub const ATTR_MODEL: &str = "model";
/// Metric attribute key: the run/turn terminal outcome. Mirrors Go `AttrOutcome`.
pub const ATTR_OUTCOME: &str = "outcome";
/// Metric attribute key: the bounded failure reason (`error` | `stalled`). Mirrors Go `AttrReason`.
pub const ATTR_REASON: &str = "reason";

/// Rhapsody's instruments. Build with [`Metrics::new`]; record via the semantic methods. All methods
/// are safe no-ops when built from a no-op meter. Mirrors Go `telemetry.Metrics`.
pub struct Metrics {
    dispatched: Counter<u64>,
    completed: Counter<u64>,
    failed: Counter<u64>,
    retried: Counter<u64>,
    stalled: Counter<u64>,
    running: Gauge<i64>,
    retry_depth: Gauge<i64>,
    run_dur: Histogram<f64>,
    turn_dur: Histogram<f64>,
    in_tok: Counter<u64>,
    out_tok: Counter<u64>,
    tot_tok: Counter<u64>,
}

impl Metrics {
    /// Returns `Metrics` whose methods do nothing (a provider with no readers records nowhere).
    /// Mirrors Go `NoopMetrics`.
    pub fn noop() -> Metrics {
        let mp = SdkMeterProvider::builder().build();
        Metrics::new(&mp.meter("symphony"))
    }

    /// Builds `Metrics` on an arbitrary meter (test helper / F1 wiring). Mirrors Go
    /// `NewMetricsForTest`.
    pub fn new_for_test(meter: &Meter) -> Metrics {
        Metrics::new(meter)
    }

    /// Creates all instruments on the given meter. Mirrors Go `newMetrics`.
    pub fn new(meter: &Meter) -> Metrics {
        let ctr = |name: &'static str, desc: &'static str| {
            meter.u64_counter(name).with_description(desc).build()
        };
        let gauge = |name: &'static str, desc: &'static str| {
            meter.i64_gauge(name).with_description(desc).build()
        };
        let hist = |name: &'static str, desc: &'static str| {
            meter
                .f64_histogram(name)
                .with_unit("s")
                .with_description(desc)
                .build()
        };
        Metrics {
            dispatched: ctr(
                "symphony.issues.dispatched",
                "issues dispatched to workers (attr: project)",
            ),
            completed: ctr(
                "symphony.issues.completed",
                "worker runs that exited cleanly (attr: project)",
            ),
            failed: ctr(
                "symphony.issues.failed",
                "all worker-run failures (attrs: project, reason — reason=error|stalled)",
            ),
            retried: ctr(
                "symphony.issues.retried",
                "retries scheduled (attr: project)",
            ),
            stalled: ctr(
                "symphony.issues.stalled",
                "runs terminated by stall detection (attr: project)",
            ),
            running: gauge(
                "symphony.sessions.running",
                "currently running agent sessions",
            ),
            retry_depth: gauge("symphony.retry_queue.depth", "pending retries"),
            run_dur: hist("symphony.run.duration", "worker run duration (seconds)"),
            turn_dur: hist("symphony.turn.duration", "agent turn duration (seconds)"),
            in_tok: ctr("symphony.tokens.input", "input tokens"),
            out_tok: ctr("symphony.tokens.output", "output tokens"),
            tot_tok: ctr("symphony.tokens.total", "total tokens"),
        }
    }

    /// Labels the project-only counters. Mirrors Go `Metrics.Dispatched`.
    pub fn dispatched(&self, attrs: &[KeyValue]) {
        self.dispatched.add(1, attrs);
    }
    /// Mirrors Go `Metrics.Completed`.
    pub fn completed(&self, attrs: &[KeyValue]) {
        self.completed.add(1, attrs);
    }
    /// Mirrors Go `Metrics.Failed` (attrs: project, reason).
    pub fn failed(&self, attrs: &[KeyValue]) {
        self.failed.add(1, attrs);
    }
    /// Mirrors Go `Metrics.Retried`.
    pub fn retried(&self, attrs: &[KeyValue]) {
        self.retried.add(1, attrs);
    }
    /// Mirrors Go `Metrics.Stalled`.
    pub fn stalled(&self, attrs: &[KeyValue]) {
        self.stalled.add(1, attrs);
    }

    /// Gauges stay label-less (the orchestrator tracks single aggregate counts). Mirrors Go
    /// `Metrics.SetRunning`.
    pub fn set_running(&self, n: i64) {
        self.running.record(n, &[]);
    }
    /// Mirrors Go `Metrics.SetRetryDepth`.
    pub fn set_retry_depth(&self, n: i64) {
        self.retry_depth.record(n, &[]);
    }

    /// Mirrors Go `Metrics.RunDuration` (attrs: project, model, outcome).
    pub fn run_duration(&self, secs: f64, attrs: &[KeyValue]) {
        self.run_dur.record(secs, attrs);
    }
    /// Mirrors Go `Metrics.TurnDuration`.
    pub fn turn_duration(&self, secs: f64, attrs: &[KeyValue]) {
        self.turn_dur.record(secs, attrs);
    }

    /// Records the input/output/total token counters together (attrs: project, model). Mirrors Go
    /// `Metrics.Tokens`.
    pub fn tokens(&self, input: i64, output: i64, total: i64, attrs: &[KeyValue]) {
        self.in_tok.add(input as u64, attrs);
        self.out_tok.add(output as u64, attrs);
        self.tot_tok.add(total as u64, attrs);
    }
}

#[cfg(test)]
mod tests {
    use opentelemetry::metrics::MeterProvider;
    use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData, ResourceMetrics};
    use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};

    use super::*;

    /// Builds a provider whose metrics land in an in-memory exporter — the Rust analogue of Go's
    /// `metric.NewManualReader()` + `reader.Collect` (`force_flush` collects; `get_finished_metrics`
    /// reads back what a `ManualReader.Collect` would return).
    fn collectable() -> (Metrics, InMemoryMetricExporter, SdkMeterProvider) {
        let exporter = InMemoryMetricExporter::default();
        let reader = PeriodicReader::builder(exporter.clone()).build();
        let mp = SdkMeterProvider::builder().with_reader(reader).build();
        let m = Metrics::new(&mp.meter("symphony"));
        (m, exporter, mp)
    }

    /// Forces a collect+export and returns the latest resource metrics batch.
    fn collect(mp: &SdkMeterProvider, exporter: &InMemoryMetricExporter) -> ResourceMetrics {
        mp.force_flush().expect("force_flush");
        exporter
            .get_finished_metrics()
            .expect("finished metrics")
            .pop()
            .expect("at least one exported batch")
    }

    fn metric_names(rm: &ResourceMetrics) -> Vec<String> {
        rm.scope_metrics()
            .flat_map(|sm| sm.metrics().map(|m| m.name().to_string()))
            .collect()
    }

    // Mirrors Go `TestMetricsRecord`: the named instruments record and collect.
    #[test]
    fn metrics_record() {
        let (m, exporter, mp) = collectable();
        m.dispatched(&[]);
        m.dispatched(&[]);
        m.completed(&[]);
        m.set_running(3);
        m.tokens(100, 40, 140, &[]);
        m.run_duration(1.5, &[]);

        let rm = collect(&mp, &exporter);
        let names = metric_names(&rm);
        for want in [
            "symphony.issues.dispatched",
            "symphony.issues.completed",
            "symphony.sessions.running",
            "symphony.tokens.total",
            "symphony.run.duration",
        ] {
            assert!(
                names.iter().any(|n| n == want),
                "metric {want:?} not recorded; got {names:?}"
            );
        }
    }

    // Mirrors Go `TestNoopMetricsSafe`: no method panics, including with attributes.
    #[test]
    fn noop_metrics_safe() {
        let m = Metrics::noop();
        m.dispatched(&[]);
        m.set_running(5);
        m.tokens(1, 2, 3, &[KeyValue::new(ATTR_PROJECT, "alpha")]);
        m.turn_duration(0.1, &[KeyValue::new(ATTR_MODEL, "claude-opus-4-8")]);
        m.failed(&[
            KeyValue::new(ATTR_PROJECT, "alpha"),
            KeyValue::new(ATTR_REASON, "error"),
        ]);
    }

    /// Fails unless metric `name` has at least one datapoint whose attribute set is a superset of
    /// `want`. Walks Sum (counter) and Histogram datapoints. Mirrors Go `requireDatapointAttrs`.
    fn require_datapoint_attrs(rm: &ResourceMetrics, name: &str, want: &[(&str, &str)]) {
        let matches = |attrs: &[KeyValue]| {
            want.iter().all(|(k, v)| {
                attrs
                    .iter()
                    .any(|kv| kv.key.as_str() == *k && kv.value.as_str() == *v)
            })
        };
        for sm in rm.scope_metrics() {
            for metric in sm.metrics() {
                if metric.name() != name {
                    continue;
                }
                let found = match metric.data() {
                    AggregatedMetrics::U64(MetricData::Sum(sum)) => sum
                        .data_points()
                        .any(|dp| matches(&dp.attributes().cloned().collect::<Vec<_>>())),
                    AggregatedMetrics::F64(MetricData::Histogram(h)) => h
                        .data_points()
                        .any(|dp| matches(&dp.attributes().cloned().collect::<Vec<_>>())),
                    _ => false,
                };
                if found {
                    return;
                }
            }
        }
        panic!("metric {name:?}: no datapoint with attributes {want:?}");
    }

    // Mirrors Go `TestMetrics_AttributesRecorded`: the bounded label schema lands on the datapoints.
    #[test]
    fn attributes_recorded() {
        let (m, exporter, mp) = collectable();
        m.failed(&[
            KeyValue::new(ATTR_PROJECT, "alpha"),
            KeyValue::new(ATTR_REASON, "stalled"),
        ]);
        m.tokens(
            10,
            5,
            15,
            &[
                KeyValue::new(ATTR_PROJECT, "alpha"),
                KeyValue::new(ATTR_MODEL, "claude-opus-4-8"),
            ],
        );
        m.run_duration(
            2.5,
            &[
                KeyValue::new(ATTR_PROJECT, "alpha"),
                KeyValue::new(ATTR_MODEL, "claude-opus-4-8"),
                KeyValue::new(ATTR_OUTCOME, "completed"),
            ],
        );

        let rm = collect(&mp, &exporter);
        require_datapoint_attrs(
            &rm,
            "symphony.issues.failed",
            &[("project", "alpha"), ("reason", "stalled")],
        );
        require_datapoint_attrs(
            &rm,
            "symphony.tokens.total",
            &[("project", "alpha"), ("model", "claude-opus-4-8")],
        );
        require_datapoint_attrs(
            &rm,
            "symphony.tokens.input",
            &[("project", "alpha"), ("model", "claude-opus-4-8")],
        );
        require_datapoint_attrs(
            &rm,
            "symphony.run.duration",
            &[
                ("project", "alpha"),
                ("model", "claude-opus-4-8"),
                ("outcome", "completed"),
            ],
        );
    }
}
