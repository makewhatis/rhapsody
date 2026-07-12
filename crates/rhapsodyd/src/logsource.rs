//! logsource — bridges the telemetry in-memory log ring ([`rhapsody_telemetry::LogBuffer`]) to the
//! httpapi [`rhapsody_httpapi::LogSource`] the `/api/v1/logs` + `/logs/stream` endpoints read.
//!
//! Go passes `tel.Logs` (a `*telemetry.LogBuffer`) straight to `httpapi.New` because Go's
//! `httpapi.LogSource` interface is satisfied by that concrete type. The Rust crates each own their
//! own `LogEntry` wire type (telemetry's carries a `DateTime<Utc>`; httpapi's a pre-formatted RFC3339
//! `String`) and neither depends on the other, so this daemon-assembly adapter (F1) converts between
//! them: a snapshot maps each entry, and `subscribe` forwards the telemetry broadcast onto an httpapi
//! broadcast through a background task (a `broadcast::Receiver<A>` cannot be retyped to `<B>`).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;

use tokio::sync::broadcast;

use rhapsody_httpapi::{LogEntry as ApiLogEntry, LogSource};
use rhapsody_telemetry::{LogBuffer, LogEntry as TelLogEntry};

/// The forwarded broadcast's capacity: the ring-buffered SSE fan-out for `/logs/stream`. A slow
/// subscriber that falls this far behind sees a `Lagged` gap (the httpapi handler tolerates it — the
/// per-entry `seq` + stream `epoch` let the client detect + recover), exactly as Go's buffered
/// `Subscribe` channel drops under backpressure.
const FORWARD_CAP: usize = 512;

/// How long the forwarder blocks per `recv` before re-checking the stop flag — bounds how long the
/// thread lingers past a [`LogBufferSource`] drop when the ring has gone quiet.
const FORWARD_POLL: Duration = Duration::from_millis(250);

/// Adapts a [`LogBuffer`] to httpapi's [`LogSource`]. Holds a clone of the buffer (so the snapshot +
/// epoch stay live) and the sender end of the converted broadcast the background forwarder feeds.
pub struct LogBufferSource {
    buf: LogBuffer,
    tx: broadcast::Sender<ApiLogEntry>,
    /// Set on drop to stop the forwarder thread — the ring's `subscribe` retains this source's sender
    /// (via the `cancel` closure the thread holds), so a blocking `recv` alone would never return
    /// `Err` and the thread could never self-terminate; the flag + a bounded `recv_timeout` fix that.
    stop: Arc<AtomicBool>,
}

impl LogBufferSource {
    /// Wraps a [`LogBuffer`] and spawns the background forwarder that converts each new telemetry
    /// entry onto the httpapi broadcast. The telemetry ring's `subscribe` hands back a *blocking* std
    /// `mpsc::Receiver` (its fan-out sends are non-blocking + drop under backpressure), so the
    /// forwarder is a dedicated OS thread doing a bounded blocking `recv`, not a tokio task.
    ///
    /// The thread ends on either signal: the ring's sender drops (`Disconnected`), or this source is
    /// dropped ([`Drop`] sets `stop`, observed within [`FORWARD_POLL`] on the next `recv_timeout`).
    /// The second is load-bearing — the ring retains this subscription's sender inside `ring.subs`
    /// (only released by the `cancel` closure the thread itself holds), so without the stop flag a
    /// quiet ring would block the thread forever, orphaning it past shutdown.
    pub fn new(buf: LogBuffer) -> Self {
        let (tx, _rx) = broadcast::channel(FORWARD_CAP);
        let stop = Arc::new(AtomicBool::new(false));
        let (src_rx, cancel) = buf.subscribe();
        let fwd = tx.clone();
        let thread_stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            loop {
                match src_rx.recv_timeout(FORWARD_POLL) {
                    // Forward the converted entry; a send error just means no live subscriber right
                    // now (the next `subscribe()` gets a fresh receiver), so ignore it.
                    Ok(entry) => {
                        let _ = fwd.send(convert(entry));
                    }
                    // No entry this window — exit if the source was dropped, else keep waiting.
                    Err(RecvTimeoutError::Timeout) => {
                        if thread_stop.load(Ordering::Relaxed) {
                            break;
                        }
                    }
                    // The ring's sender side is gone (full shutdown) — nothing more will arrive.
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
            cancel(); // release this subscription's slot from the ring
        });
        Self { buf, tx, stop }
    }
}

impl Drop for LogBufferSource {
    fn drop(&mut self) {
        // Signal the forwarder thread to exit (observed on its next `recv_timeout`), so it never
        // outlives the source that owns it.
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Converts a telemetry log entry to the httpapi wire entry: the `DateTime<Utc>` becomes a pre-
/// formatted RFC3339 string (Go serializes `time.Time` as RFC3339), the rest carry over 1:1.
fn convert(e: TelLogEntry) -> ApiLogEntry {
    ApiLogEntry {
        seq: e.seq,
        time: e.time.to_rfc3339(),
        level: e.level,
        msg: e.msg,
        attrs: e.attrs,
    }
}

impl LogSource for LogBufferSource {
    fn snapshot(&self) -> Vec<ApiLogEntry> {
        self.buf.snapshot().into_iter().map(convert).collect()
    }

    fn subscribe(&self) -> broadcast::Receiver<ApiLogEntry> {
        self.tx.subscribe()
    }

    fn epoch(&self) -> u64 {
        self.buf.epoch()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    // The telemetry→httpapi entry conversion carries every field and formats the timestamp as RFC3339
    // (the shape the `/api/v1/logs` golden asserts, modulo the normalized `<TIMESTAMP>`).
    #[test]
    fn convert_maps_all_fields() {
        let mut attrs = BTreeMap::new();
        attrs.insert("k".to_string(), "v".to_string());
        let t = TelLogEntry {
            seq: 7,
            time: chrono::DateTime::parse_from_rfc3339("2026-07-11T12:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
            level: "INFO".to_string(),
            msg: "hello".to_string(),
            attrs: attrs.clone(),
        };
        let a = convert(t);
        assert_eq!(a.seq, 7);
        assert_eq!(a.level, "INFO");
        assert_eq!(a.msg, "hello");
        assert_eq!(a.attrs, attrs);
        // A valid RFC3339 instant (normalize collapses it to <TIMESTAMP> in the golden).
        assert!(a.time.starts_with("2026-07-11T12:00:00"), "time={}", a.time);
    }

    // A fresh buffer's snapshot is empty and its epoch is exposed unchanged; `subscribe` yields a live
    // receiver (smoke: the adapter wires the three trait methods).
    #[tokio::test]
    async fn empty_buffer_snapshot_and_epoch() {
        let buf = LogBuffer::new(64, tracing::Level::INFO);
        let epoch = buf.epoch();
        let src = LogBufferSource::new(buf);
        assert!(
            src.snapshot().is_empty(),
            "fresh buffer snapshot must be empty"
        );
        assert_eq!(src.epoch(), epoch);
        let _rx = src.subscribe(); // a receiver can be taken
    }
}
