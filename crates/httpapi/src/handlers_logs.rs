//! handlers_logs — the daemon process-log surface for the Logs settings tab. Parity port of Go
//! `$REF/internal/httpapi/handlers_logs.go` (`handleLogs` one-shot snapshot + `handleLogStream` SSE) +
//! the `logsResponse` DTO.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::State;
use axum::http::{Method, StatusCode, header};
use axum::response::Response;
use serde_json::json;
use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::ReceiverStream;

use crate::handlers::require_get;
use crate::logs::{LogEntry, LogSource};
use crate::responses::write_json;

/// The idle keep-alive cadence for the SSE stream: a comment frame every interval keeps intermediaries
/// (and the Wails reverse proxy) from idling out a quiet connection. Mirrors Go `logStreamHeartbeat`.
const LOG_STREAM_HEARTBEAT: Duration = Duration::from_secs(25);

/// `GET /api/v1/logs`: a one-shot snapshot of the retained daemon log ring (oldest first) — the
/// non-streaming fallback + the shape the UI hydrates from before opening the stream. A `None` source
/// returns `entries:[]` (never null / 500). Method-agnostic route, so it guards GET/HEAD here. Mirrors
/// Go `handleLogs`.
pub(crate) async fn handle_logs(
    method: Method,
    State(logs): State<Option<Arc<dyn LogSource>>>,
) -> Response {
    if let Some(resp) = require_get(&method) {
        return resp;
    }
    let entries = logs.map(|l| l.snapshot()).unwrap_or_default();
    write_json(StatusCode::OK, &json!({ "entries": entries }))
}

/// `GET /api/v1/logs/stream` as Server-Sent Events: replays the current ring as backlog, then streams
/// each new entry as a `data: <json>\n\n` frame, with `: ping\n\n` heartbeats. Runs until the client
/// disconnects (the response body — hence the mpsc receiver — is dropped, ending the feeder task). A
/// `None` source still holds the connection open (heartbeats only) rather than 500ing. Mirrors Go
/// `handleLogStream`.
pub(crate) async fn handle_log_stream(
    method: Method,
    State(logs): State<Option<Arc<dyn LogSource>>>,
) -> Response {
    if let Some(resp) = require_get(&method) {
        return resp;
    }
    // HEAD: the SSE headers + 200, no stream (Go returns right after writing the header for HEAD).
    if method == Method::HEAD {
        return sse_response(Body::empty());
    }
    // GET: a background task formats frames into an mpsc the response body drains. When the client
    // disconnects, the body (and its receiver) drop, so the next `tx.send` fails and the task exits.
    let (tx, rx) = mpsc::channel::<Result<String, Infallible>>(64);
    tokio::spawn(stream_logs(logs, tx));
    sse_response(Body::from_stream(ReceiverStream::new(rx)))
}

/// The SSE feeder: announce the epoch, subscribe BEFORE snapshotting (so entries logged during the
/// backlog replay are captured on the channel rather than lost in the gap — any overlap is de-duped by
/// the client on `seq`), replay the backlog, then tail live entries + heartbeats. Mirrors the body of
/// Go `handleLogStream`.
async fn stream_logs(
    logs: Option<Arc<dyn LogSource>>,
    tx: mpsc::Sender<Result<String, Infallible>>,
) {
    let mut sub: Option<broadcast::Receiver<LogEntry>> = None;
    if let Some(src) = &logs {
        if send_frame(&tx, format!("event: epoch\ndata: {}\n\n", src.epoch()))
            .await
            .is_err()
        {
            return;
        }
        sub = Some(src.subscribe());
        for e in src.snapshot() {
            if !send_entry(&tx, &e).await {
                return;
            }
        }
    }

    let mut ticker = tokio::time::interval(LOG_STREAM_HEARTBEAT);
    ticker.tick().await; // consume the immediate first tick so heartbeats are spaced by the interval
    loop {
        // `tx.closed()` completes when the response body (hence the receiver) is dropped on client
        // disconnect — the Rust analog of Go's `<-ctx.Done()`, so a hung-up stream is torn down
        // promptly rather than lingering until the next heartbeat send fails.
        match sub.as_mut() {
            Some(rx) => {
                tokio::select! {
                    _ = tx.closed() => return,
                    recv = rx.recv() => match recv {
                        Ok(e) => {
                            if !send_entry(&tx, &e).await {
                                return;
                            }
                        }
                        Err(broadcast::error::RecvError::Closed) => return,
                        // A lagged subscriber skips the dropped span and keeps tailing (the client
                        // re-syncs on seq); never tear the stream down over back-pressure.
                        Err(broadcast::error::RecvError::Lagged(_)) => {}
                    },
                    _ = ticker.tick() => {
                        if send_frame(&tx, ": ping\n\n".to_string()).await.is_err() {
                            return;
                        }
                    }
                }
            }
            None => {
                tokio::select! {
                    _ = tx.closed() => return,
                    _ = ticker.tick() => {
                        if send_frame(&tx, ": ping\n\n".to_string()).await.is_err() {
                            return;
                        }
                    }
                }
            }
        }
    }
}

/// Serialize `e` and send it as a `data:` frame. Returns `false` only when the send fails (client
/// gone → stop); a serialize failure skips the entry but keeps the stream alive (Go's `writeEntry`).
async fn send_entry(tx: &mpsc::Sender<Result<String, Infallible>>, e: &LogEntry) -> bool {
    let json = match serde_json::to_string(e) {
        Ok(json) => json,
        Err(_) => return true,
    };
    send_frame(tx, format!("data: {json}\n\n")).await.is_ok()
}

/// Push a raw SSE frame onto the channel; `Err` means the receiver (client) is gone.
async fn send_frame(
    tx: &mpsc::Sender<Result<String, Infallible>>,
    frame: String,
) -> Result<(), mpsc::error::SendError<Result<String, Infallible>>> {
    tx.send(Ok(frame)).await
}

/// Build the SSE response with the exact header set Go writes (text/event-stream, no-cache,
/// keep-alive, X-Accel-Buffering: no). The headers are static + valid, so `body(...)` cannot fail;
/// the fallback is a bare 500 (errors are values — never a panic).
fn sse_response(body: Body) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .header("X-Accel-Buffering", "no")
        .body(body)
        .unwrap_or_else(|_| {
            let mut resp = Response::new(Body::empty());
            *resp.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
            resp
        })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use tokio::sync::broadcast;

    use super::*;
    use crate::logs::LogEntry;
    use crate::testutil::{FakeProvider, empty_snapshot};
    use crate::{StateProvider, new_handler};

    /// A test [`LogSource`] behaving like `telemetry.LogBuffer`: a retained backlog, a live broadcast,
    /// and a fixed epoch. `log()` appends to the backlog AND publishes to subscribers (both, like the
    /// real buffer, so an entry logged after subscribe streams live).
    struct FakeLogSource {
        entries: Mutex<Vec<LogEntry>>,
        tx: broadcast::Sender<LogEntry>,
        epoch: u64,
    }

    impl FakeLogSource {
        fn new(epoch: u64) -> Self {
            let (tx, _) = broadcast::channel(64);
            Self {
                entries: Mutex::new(Vec::new()),
                tx,
                epoch,
            }
        }

        fn log(&self, e: LogEntry) {
            self.entries.lock().expect("lock").push(e.clone());
            let _ = self.tx.send(e); // Err (no subscribers) is fine — it still lands in the backlog.
        }
    }

    impl LogSource for FakeLogSource {
        fn snapshot(&self) -> Vec<LogEntry> {
            self.entries.lock().expect("lock").clone()
        }
        fn subscribe(&self) -> broadcast::Receiver<LogEntry> {
            self.tx.subscribe()
        }
        fn epoch(&self) -> u64 {
            self.epoch
        }
    }

    fn entry(seq: u64, level: &str, msg: &str, attrs: &[(&str, &str)]) -> LogEntry {
        LogEntry {
            seq,
            time: "2026-05-28T12:00:00Z".into(),
            level: level.into(),
            msg: msg.into(),
            attrs: attrs
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect::<BTreeMap<_, _>>(),
        }
    }

    async fn spawn_with_logs(logs: Option<Arc<dyn LogSource>>) -> String {
        spawn_provider_logs(FakeProvider::ok(empty_snapshot()), logs).await
    }

    async fn spawn_provider_logs(
        provider: impl StateProvider + 'static,
        logs: Option<Arc<dyn LogSource>>,
    ) -> String {
        crate::testutil::spawn_router(new_handler(Arc::new(provider), logs)).await
    }

    // Mirrors Go `TestHandleLogsSnapshot`.
    #[tokio::test]
    async fn handle_logs_snapshot() {
        let buf = Arc::new(FakeLogSource::new(1));
        buf.log(entry(1, "INFO", "first", &[("run", "1")]));
        buf.log(entry(2, "WARN", "second", &[]));
        let base = spawn_with_logs(Some(buf)).await;
        let resp = reqwest::get(format!("{base}/api/v1/logs"))
            .await
            .expect("GET");
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value =
            serde_json::from_str(&resp.text().await.expect("body")).expect("json");
        let entries = body["entries"].as_array().expect("entries");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["msg"], "first");
        assert_eq!(entries[0]["attrs"]["run"], "1");
        assert_eq!(entries[1]["level"], "WARN");
    }

    // Mirrors Go `TestHandleLogsNilSourceReturnsEmptyArray`.
    #[tokio::test]
    async fn handle_logs_nil_source_returns_empty_array() {
        let base = spawn_with_logs(None).await;
        let resp = reqwest::get(format!("{base}/api/v1/logs"))
            .await
            .expect("GET");
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value =
            serde_json::from_str(&resp.text().await.expect("body")).expect("json");
        assert_eq!(body["entries"], serde_json::json!([]));
    }

    // Mirrors Go `TestHandleLogsRejectsPost`.
    #[tokio::test]
    async fn handle_logs_rejects_post() {
        let base = spawn_with_logs(Some(Arc::new(FakeLogSource::new(1)))).await;
        let status = reqwest::Client::new()
            .post(format!("{base}/api/v1/logs"))
            .send()
            .await
            .expect("POST")
            .status();
        assert_eq!(status, 405);
    }

    /// Read the SSE stream until `needle` appears in a `data:` line, returning that payload. Skips the
    /// leading `event: epoch` block + heartbeat comments; fails on timeout/EOF. The Rust analog of Go's
    /// `readSSEData`.
    async fn read_sse_data(resp: &mut reqwest::Response, needle: &str) -> String {
        let deadline = Duration::from_secs(3);
        let mut buf = String::new();
        loop {
            let chunk = tokio::time::timeout(deadline, resp.chunk())
                .await
                .expect("sse read timed out")
                .expect("sse chunk");
            match chunk {
                Some(bytes) => buf.push_str(&String::from_utf8_lossy(&bytes)),
                None => panic!("sse stream ended before {needle:?} (so far: {buf:?})"),
            }
            for line in buf.lines() {
                if let Some(data) = line.strip_prefix("data: ")
                    && data.contains(needle)
                {
                    return data.to_string();
                }
            }
        }
    }

    // Mirrors Go `TestHandleLogStreamBacklogThenLive`.
    #[tokio::test]
    async fn handle_log_stream_backlog_then_live() {
        let buf = Arc::new(FakeLogSource::new(1));
        buf.log(entry(1, "INFO", "backlog-line", &[]));
        let base = spawn_with_logs(Some(buf.clone())).await;

        let mut resp = reqwest::get(format!("{base}/api/v1/logs/stream"))
            .await
            .expect("GET stream");
        assert_eq!(
            resp.headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("text/event-stream")
        );

        let first = read_sse_data(&mut resp, "backlog-line").await;
        assert!(first.contains("backlog-line"), "first frame: {first}");

        // A live entry written after the connection is open must stream through.
        buf.log(entry(2, "ERROR", "live-line", &[]));
        let second = read_sse_data(&mut resp, "live-line").await;
        assert!(second.contains("live-line"), "second frame: {second}");
    }

    // Mirrors Go `TestHandleLogStreamEmitsEpochFirst`: the first SSE block is the epoch announcement.
    #[tokio::test]
    async fn handle_log_stream_emits_epoch_first() {
        let buf = Arc::new(FakeLogSource::new(7));
        buf.log(entry(1, "INFO", "a-line", &[]));
        let base = spawn_with_logs(Some(buf)).await;
        let mut resp = reqwest::get(format!("{base}/api/v1/logs/stream"))
            .await
            .expect("GET stream");

        let deadline = Duration::from_secs(3);
        let mut buf_s = String::new();
        // Read until we have the first two SSE lines.
        while buf_s.lines().count() < 2 {
            let chunk = tokio::time::timeout(deadline, resp.chunk())
                .await
                .expect("timeout")
                .expect("chunk")
                .expect("stream ended early");
            buf_s.push_str(&String::from_utf8_lossy(&chunk));
        }
        let mut lines = buf_s.lines();
        assert_eq!(lines.next(), Some("event: epoch"), "first line");
        assert_eq!(lines.next(), Some("data: 7"), "epoch value");
    }
}
