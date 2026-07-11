//! logbuffer — the in-memory process-log ring backing the desktop app's Logs tab. Parity port of
//! Go `telemetry.LogBuffer` (an `slog.Handler`) as a `tracing_subscriber::Layer` (the Rust logging
//! substrate). It is always present, independent of OTLP export, and fans each record to live
//! subscribers as well as retaining a bounded backlog. Mirrors `$REF/internal/telemetry/logbuffer.go`.

use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, PoisonError};

use chrono::{DateTime, Utc};
use serde::Serialize;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;

/// How many of the most recent daemon log records the ring retains for the in-app Logs view. Sized
/// to give a useful backlog on first open / reconnect without holding meaningful memory. Mirrors Go
/// `logBufferCap`.
pub const LOG_BUFFER_CAP: usize = 2000;

/// Per-subscriber channel depth. A subscriber that falls behind drops entries (the snapshot backlog
/// recovers recent context on reconnect) rather than stalling the daemon's logging. Mirrors the Go
/// buffer's 256-deep subscriber channel.
const SUBSCRIBER_CHAN_CAP: usize = 256;

/// One rendered daemon process-log record: a single `tracing` event flattened to scalar fields for
/// the UI Logs tab. `attrs` holds the flattened key→value pairs (span-qualified with a dotted
/// prefix); `seq` is a monotonically increasing per-buffer sequence so the UI can de-duplicate the
/// snapshot backlog against the live stream across a reconnect. Mirrors Go `telemetry.LogEntry`.
#[derive(Debug, Clone, Serialize)]
pub struct LogEntry {
    /// Monotonic per-buffer sequence (never reset by eviction).
    pub seq: u64,
    /// The record's timestamp.
    pub time: DateTime<Utc>,
    /// The level string (`"INFO"`, `"WARN"`, …).
    pub level: String,
    /// The record message.
    pub msg: String,
    /// Flattened, span-qualified attributes.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub attrs: BTreeMap<String, String>,
}

/// The shared, concurrency-safe state behind a [`LogBuffer`]: the bounded ring of recent entries
/// plus the set of live subscribers. Mirrors Go `logRing`.
struct LogRing {
    cap: usize,
    entries: VecDeque<LogEntry>,
    seq: u64,
    subs: BTreeMap<usize, SyncSender<LogEntry>>,
    next_sub: usize,
    /// Identifies this process's log stream. `seq` resets to 0 each process, so a changed epoch tells
    /// the client "new stream — reset the watermark". Set once at construction to the process start
    /// time in millis (monotonic across restarts, JS-safe < 2^53). Mirrors Go `logRing.epoch`.
    epoch: u64,
}

impl LogRing {
    fn append(&mut self, mut e: LogEntry) {
        self.seq += 1;
        e.seq = self.seq;
        if self.cap > 0 {
            if self.entries.len() >= self.cap {
                self.entries.pop_front(); // drop the oldest; the ring stays at cap
            }
            self.entries.push_back(e.clone());
        }
        // Non-blocking fan-out under the lock: a full or disconnected subscriber channel drops the
        // entry rather than stalling logging, and a concurrent cancel (which also takes the lock)
        // can never race a send.
        self.subs
            .retain(|_, tx| !matches!(tx.try_send(e.clone()), Err(TrySendError::Disconnected(_))));
    }
}

/// An `slog.Handler`-equivalent `tracing` layer that retains the most recent entries in a bounded
/// ring and fans each new record to live subscribers, backing the daemon's in-app Logs view (GET
/// `/api/v1/logs` and `/api/v1/logs/stream`). Cloning shares the one ring (the Layer installed in
/// the subscriber and the handle held by the API are clones). Mirrors Go `telemetry.LogBuffer`.
#[derive(Clone)]
pub struct LogBuffer {
    ring: Arc<Mutex<LogRing>>,
    /// Minimum severity admitted (records below are dropped). Numeric so it does not depend on
    /// `tracing::Level`'s ordering direction.
    min_severity: u8,
}

/// Numeric severity (higher = more severe) so admission does not depend on `Level`'s `Ord` direction.
fn severity(level: &Level) -> u8 {
    match *level {
        Level::ERROR => 4,
        Level::WARN => 3,
        Level::INFO => 2,
        Level::DEBUG => 1,
        Level::TRACE => 0,
    }
}

impl LogBuffer {
    /// Returns a `LogBuffer` retaining up to `capacity` entries (`0` keeps no backlog but still
    /// streams to live subscribers) and admitting records at or above `min_level`. Mirrors Go
    /// `NewLogBuffer`.
    pub fn new(capacity: usize, min_level: Level) -> LogBuffer {
        LogBuffer {
            ring: Arc::new(Mutex::new(LogRing {
                cap: capacity,
                entries: VecDeque::new(),
                seq: 0,
                subs: BTreeMap::new(),
                next_sub: 0,
                epoch: Utc::now().timestamp_millis().max(0) as u64,
            })),
            min_severity: severity(&min_level),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, LogRing> {
        // Poison-tolerant: a panic elsewhere must not wedge the Logs tab (no unwrap/expect).
        self.ring.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Appends a fully-formed entry to the ring (assigning its seq) and fans it out. The layer's
    /// event path builds the entry then calls this; the ring tests inject entries the same way.
    fn append(&self, e: LogEntry) {
        self.lock().append(e);
    }

    /// Identifies this process's log stream so the client can detect a daemon restart (the seq
    /// counter resets per process) and reset its de-dup watermark. Stable for the buffer's lifetime.
    /// Mirrors Go `LogBuffer.Epoch`.
    pub fn epoch(&self) -> u64 {
        self.lock().epoch
    }

    /// Returns a copy of the retained entries, oldest first. Mirrors Go `LogBuffer.Snapshot`.
    pub fn snapshot(&self) -> Vec<LogEntry> {
        self.lock().entries.iter().cloned().collect()
    }

    /// Registers a live subscriber, returning a receiver of subsequent entries and a cancel function
    /// that unsubscribes (dropping the sender closes the receiver). Sends are non-blocking: a
    /// subscriber that falls behind drops entries rather than stalling the daemon's logging. `cancel`
    /// is idempotent. Mirrors Go `LogBuffer.Subscribe`.
    pub fn subscribe(&self) -> (Receiver<LogEntry>, impl Fn() + Send + Sync + use<>) {
        let (tx, rx) = std::sync::mpsc::sync_channel(SUBSCRIBER_CHAN_CAP);
        let id = {
            let mut ring = self.lock();
            let id = ring.next_sub;
            ring.next_sub += 1;
            ring.subs.insert(id, tx);
            id
        };
        let ring = Arc::clone(&self.ring);
        let cancel = move || {
            // Idempotent: removing an already-removed subscriber is a no-op.
            ring.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .subs
                .remove(&id);
        };
        (rx, cancel)
    }
}

impl<S> Layer<S> for LogBuffer
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        // This span's dotted path = parent path + this span's name (the group prefix for everything
        // logged within it, mirroring slog's WithGroup).
        let parent_path = span
            .parent()
            .and_then(|p| p.extensions().get::<SpanFields>().map(|c| c.path.clone()))
            .unwrap_or_default();
        let path = if parent_path.is_empty() {
            span.name().to_string()
        } else {
            format!("{parent_path}.{}", span.name())
        };
        // Capture the span's own fields, group-qualified by its path.
        let mut fields = BTreeMap::new();
        let mut visitor = FieldVisitor {
            prefix: &path,
            attrs: &mut fields,
            msg: &mut None,
        };
        attrs.record(&mut visitor);
        span.extensions_mut().insert(SpanFields { path, fields });
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        if severity(event.metadata().level()) < self.min_severity {
            return;
        }
        let mut attrs = BTreeMap::new();
        // Fold in each enclosing span's captured fields (root → leaf); the deepest span's path
        // prefixes the event's own fields.
        let mut leaf_path = String::new();
        if let Some(scope) = ctx.event_scope(event) {
            for span in scope.from_root() {
                if let Some(sf) = span.extensions().get::<SpanFields>() {
                    for (k, v) in &sf.fields {
                        attrs.insert(k.clone(), v.clone());
                    }
                    leaf_path.clone_from(&sf.path);
                }
            }
        }
        let mut msg = None;
        let mut visitor = FieldVisitor {
            prefix: &leaf_path,
            attrs: &mut attrs,
            msg: &mut msg,
        };
        event.record(&mut visitor);
        self.append(LogEntry {
            seq: 0,
            time: Utc::now(),
            level: event.metadata().level().to_string(),
            msg: msg.unwrap_or_default(),
            attrs,
        });
    }
}

/// A span's captured fields and its dotted group path (`parent.child`). Stored in span extensions so
/// `on_event` can fold ancestor fields into the record — the tracing analogue of slog's accumulated
/// `pre` attrs + open groups.
struct SpanFields {
    path: String,
    fields: BTreeMap<String, String>,
}

/// Flattens `tracing` field values to `key → string` pairs, prefixing keys with `prefix` (the open
/// span group). The special `message` field becomes the record message, not an attr — mirroring how
/// Go's handler renders the slog message vs. its attrs.
struct FieldVisitor<'a> {
    prefix: &'a str,
    attrs: &'a mut BTreeMap<String, String>,
    msg: &'a mut Option<String>,
}

impl FieldVisitor<'_> {
    fn put(&mut self, name: &str, value: String) {
        if name == "message" {
            *self.msg = Some(value);
            return;
        }
        let key = if self.prefix.is_empty() {
            name.to_string()
        } else {
            format!("{}.{name}", self.prefix)
        };
        self.attrs.insert(key, value);
    }
}

impl Visit for FieldVisitor<'_> {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.put(field.name(), value.to_string());
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.put(field.name(), value.to_string());
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.put(field.name(), value.to_string());
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.put(field.name(), value.to_string());
    }
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.put(field.name(), value.to_string());
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.put(field.name(), format!("{value:?}"));
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tracing_subscriber::layer::SubscriberExt;

    use super::*;

    fn entry(msg: &str) -> LogEntry {
        LogEntry {
            seq: 0,
            time: Utc::now(),
            level: "INFO".to_string(),
            msg: msg.to_string(),
            attrs: BTreeMap::new(),
        }
    }

    // Mirrors Go `TestLogBufferRingEvictsOldest`: caps at N, evicts oldest, seq monotonic + never
    // reset by eviction.
    #[test]
    fn ring_evicts_oldest() {
        let b = LogBuffer::new(3, Level::INFO);
        for c in ['a', 'b', 'c', 'd', 'e'] {
            b.append(entry(&c.to_string()));
        }
        let got = b.snapshot();
        assert_eq!(got.len(), 3, "ring caps at 3");
        assert_eq!(
            (
                got[0].msg.as_str(),
                got[1].msg.as_str(),
                got[2].msg.as_str()
            ),
            ("c", "d", "e"),
            "oldest two evicted, newest three oldest-first"
        );
        assert_eq!(
            (got[0].seq, got[2].seq),
            (3, 5),
            "seq monotonic, not reset by eviction"
        );
    }

    // Mirrors Go `TestLogBufferSnapshotIsCopy`.
    #[test]
    fn snapshot_is_copy() {
        let b = LogBuffer::new(4, Level::INFO);
        b.append(entry("one"));
        let mut snap = b.snapshot();
        snap[0].msg = "mutated".to_string();
        assert_eq!(b.snapshot()[0].msg, "one", "snapshot must be a copy");
    }

    // Mirrors Go `TestLogBufferSubscribeReceivesLiveThenCancel`: receives live, cancel closes the
    // channel, cancel is idempotent.
    #[test]
    fn subscribe_receives_live_then_cancel() {
        let b = LogBuffer::new(8, Level::INFO);
        let (rx, cancel) = b.subscribe();

        b.append(entry("live"));
        let e = rx
            .recv_timeout(Duration::from_secs(1))
            .expect("receive the live entry");
        assert_eq!(e.msg, "live");

        cancel();
        // After cancel the sender is dropped, so the channel is closed (recv errors).
        assert!(rx.recv().is_err(), "channel closed after cancel");
        cancel(); // idempotent: must not panic
    }

    // Mirrors Go `TestLogBufferSlowSubscriberDoesNotBlock`: a never-drained subscriber never stalls
    // append (far past the channel depth).
    #[test]
    fn slow_subscriber_does_not_block() {
        let b = LogBuffer::new(0, Level::INFO); // stream-only
        let (_rx, cancel) = b.subscribe(); // never drained
        for _ in 0..1000 {
            // far exceeds SUBSCRIBER_CHAN_CAP
            b.append(entry("x"));
        }
        cancel();
    }

    // Mirrors Go `TestLogBufferAsSlogHandler`: as a tracing layer, level-filters and flattens fields.
    #[test]
    fn as_tracing_layer_filters_and_flattens() {
        let b = LogBuffer::new(8, Level::INFO);
        let subscriber = tracing_subscriber::registry().with(b.clone());
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(run = 42, issue = "INF-226", "hello");
            tracing::debug!("filtered"); // below INFO → not retained
        });

        let got = b.snapshot();
        assert_eq!(got.len(), 1, "debug filtered");
        let e = &got[0];
        assert_eq!(e.msg, "hello");
        assert_eq!(e.level, "INFO");
        assert_eq!(e.attrs.get("run").map(String::as_str), Some("42"));
        assert_eq!(e.attrs.get("issue").map(String::as_str), Some("INF-226"));
    }

    // Mirrors Go `TestLogBufferWithAttrsAndGroupShareRing`: a span acts as slog's WithGroup — its
    // own fields and events within it are group-qualified with a dotted prefix, and every record
    // lands in the ONE shared ring (whether inside a span or not).
    #[test]
    fn span_grouping_shares_ring() {
        let b = LogBuffer::new(8, Level::INFO);
        let subscriber = tracing_subscriber::registry().with(b.clone());
        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("poll", component = "orchestrator");
            span.in_scope(|| tracing::info!(eligible = 3, "tick"));
            tracing::info!(top = 1, "outside"); // no span → unqualified
        });

        let got = b.snapshot();
        assert_eq!(got.len(), 2, "both records share the one ring");
        let tick = got.iter().find(|e| e.msg == "tick").expect("tick entry");
        assert_eq!(
            tick.attrs.get("poll.component").map(String::as_str),
            Some("orchestrator"),
            "span field is group-qualified"
        );
        assert_eq!(
            tick.attrs.get("poll.eligible").map(String::as_str),
            Some("3"),
            "event field inside the span is group-qualified"
        );
        let outside = got
            .iter()
            .find(|e| e.msg == "outside")
            .expect("outside entry");
        assert_eq!(
            outside.attrs.get("top").map(String::as_str),
            Some("1"),
            "no span → unqualified"
        );
    }
}
