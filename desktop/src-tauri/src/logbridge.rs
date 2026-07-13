//! logbridge — forwards the supervised `rhapsodyd`'s process-log tail to the packaged webview over a
//! Tauri IPC [`Channel`] instead of Server-Sent Events (TRA-252).
//!
//! Why this exists: the daemon exposes the Logs tab's live tail as an infinite `text/event-stream`
//! (`GET /api/v1/logs/stream`). In a plain browser (or the daemon-origin dashboard) the web hook tails
//! it with `EventSource`. But the packaged app serves `web/` over wry's custom protocol
//! ([`crate::windowserver`]), whose responder is **fully buffered** — `RequestAsyncResponder::respond`
//! takes a complete body, so an infinite stream can never be forwarded through the same-origin proxy
//! (reading it to completion would hang the request). Go's Wails shell did not hit this: its
//! `httputil.ReverseProxy` streams the SSE straight through. To restore the live tail on Tauri we run a
//! host-side task that connects to the daemon's SSE endpoint with a streaming reader ([`reqwest`]'s
//! chunked `Response::chunk`) and re-emits each frame — the epoch announcement and every log line — over
//! a [`Channel`] the Logs view subscribes to. `start`/`stop` bracket the view's lifecycle.
//!
//! The SSE-decoding + forwarding core ([`SseDecoder`], [`stream_once`], [`run_bridge`]) is generic over a
//! message sink so it is unit-testable against a fake SSE backend without a real webview, exactly like
//! [`crate::apiproxy`]/[`crate::windowserver`] are generic over their `next`/`base_url`.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use serde::Serialize;
use tauri::async_runtime::JoinHandle;
use tauri::ipc::Channel;

/// The daemon SSE endpoint the bridge tails — the same path the browser hook opens with `EventSource`.
const LOGS_STREAM_PATH: &str = "/api/v1/logs/stream";

/// How long to wait before re-attempting a dropped/absent stream. Mirrors the browser `EventSource`'s
/// ~3s default reconnect cadence so the desktop tail recovers from a daemon restart on the same timescale.
pub const RECONNECT_DELAY: Duration = Duration::from_secs(3);

/// A message forwarded to the Logs view over the IPC channel. The `epoch` + `line` variants carry the
/// same two SSE frame kinds the browser hook handles (`event: epoch` and `data:` log lines), so the web
/// hook's seq de-dup / epoch-reset logic is shared across both transports; `open`/`reconnecting` drive the
/// connection-status dot (the desktop analogue of `EventSource`'s `onopen`/`onerror`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LogMsg {
    /// The upstream stream is connected (HTTP 200) — status → "live".
    Open,
    /// The daemon's stream epoch (`event: epoch`); a change means the daemon restarted (seq reset).
    Epoch { epoch: String },
    /// One log line, the raw JSON payload of a `data:` frame — parsed by the web hook exactly as the SSE
    /// `onmessage` data is (so the wire shape stays identical across transports).
    Line { data: String },
    /// The stream dropped or the daemon is not up yet; the bridge is retrying — status → "connecting".
    Reconnecting,
}

/// A decoded SSE frame from the daemon's log stream: the epoch announcement, or one log line's raw JSON.
/// Heartbeat comments (`: ping`) and unknown fields are dropped by the decoder and never surface here.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SseFrame {
    Epoch(String),
    Line(String),
}

/// An incremental decoder for the daemon's SSE log stream. Fed arbitrary byte chunks (as they arrive off
/// the socket), it buffers a partial trailing line and yields complete frames. It implements just the
/// slice of the SSE grammar the daemon emits (`$REF`/`handlers_logs.rs`): `event:`/`data:` fields, a blank
/// line as the frame terminator, and `:`-prefixed comment heartbeats — enough to be byte-faithful to the
/// daemon without a full EventSource parser. Buffering raw bytes (not decoded text) keeps a multi-byte
/// UTF-8 character that straddles a chunk boundary intact until the rest of its line arrives.
struct SseDecoder {
    /// Bytes received but not yet terminated by a newline (a partial line).
    buf: Vec<u8>,
    /// The current frame's `event:` value ("" for a default `message` frame).
    event: String,
    /// The current frame's accumulated `data:` value (multiple `data:` lines join with `\n`, per SSE).
    data: String,
    /// Whether any `data:` field has been seen for the current frame (an empty `data:` is still data).
    have_data: bool,
}

impl SseDecoder {
    fn new() -> Self {
        SseDecoder {
            buf: Vec::new(),
            event: String::new(),
            data: String::new(),
            have_data: false,
        }
    }

    /// Feeds a chunk of stream bytes, returning every frame that completed within it.
    fn push(&mut self, chunk: &[u8]) -> Vec<SseFrame> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(nl) = self.buf.iter().position(|&b| b == b'\n') {
            let line_bytes: Vec<u8> = self.buf.drain(..=nl).collect();
            // Decode only the completed line (without its trailing '\n'); a partial line stays buffered.
            let line = String::from_utf8_lossy(&line_bytes[..line_bytes.len() - 1]);
            self.handle_line(line.strip_suffix('\r').unwrap_or(&line), &mut out);
        }
        out
    }

    /// Processes one complete SSE line: dispatch on a blank line, ignore comments, else accumulate the
    /// `event`/`data` field.
    fn handle_line(&mut self, line: &str, out: &mut Vec<SseFrame>) {
        if line.is_empty() {
            self.dispatch(out);
            return;
        }
        if line.starts_with(':') {
            return; // comment / heartbeat (`: ping`)
        }
        let (field, value) = match line.split_once(':') {
            // Per the SSE spec, a single leading space after the colon is part of the delimiter, not data.
            Some((field, rest)) => (field, rest.strip_prefix(' ').unwrap_or(rest)),
            None => (line, ""), // a field name with no colon has an empty value
        };
        match field {
            "event" => self.event = value.to_string(),
            "data" => {
                if self.have_data {
                    self.data.push('\n');
                }
                self.data.push_str(value);
                self.have_data = true;
            }
            _ => {} // id/retry/unknown fields are irrelevant to the log tail
        }
    }

    /// Emits the current frame (if it carried data) at a frame boundary and resets for the next one.
    fn dispatch(&mut self, out: &mut Vec<SseFrame>) {
        if self.have_data {
            let data = std::mem::take(&mut self.data);
            if self.event == "epoch" {
                out.push(SseFrame::Epoch(data));
            } else {
                out.push(SseFrame::Line(data));
            }
        }
        self.event.clear();
        self.data.clear();
        self.have_data = false;
    }
}

/// The result of one connection attempt in [`run_bridge`].
#[derive(Debug, PartialEq, Eq)]
enum StreamOutcome {
    /// The stream never connected, or dropped/ended — retry after a backoff.
    Disconnected,
    /// The message sink is gone (the webview closed / the view unsubscribed) — end the bridge.
    SinkClosed,
}

/// Connects once to `<base><LOGS_STREAM_PATH>`, announces [`LogMsg::Open`], then decodes the SSE body
/// incrementally and forwards each frame through `send` until the stream ends or the sink closes. Reading
/// with [`reqwest::Response::chunk`] (rather than buffering the whole body) is what makes the infinite
/// stream forwardable — the exact thing the buffered custom-protocol proxy cannot do.
async fn stream_once<F>(client: &reqwest::Client, base: &str, send: &F) -> StreamOutcome
where
    F: Fn(LogMsg) -> bool,
{
    let url = format!("{}{}", base.trim_end_matches('/'), LOGS_STREAM_PATH);
    let mut resp = match client.get(&url).send().await {
        Ok(resp) if resp.status().is_success() => resp,
        // A transport error or a non-2xx (e.g. the daemon is mid-restart) is a transient disconnect.
        _ => return StreamOutcome::Disconnected,
    };
    if !send(LogMsg::Open) {
        return StreamOutcome::SinkClosed;
    }
    let mut decoder = SseDecoder::new();
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                for frame in decoder.push(&chunk) {
                    let msg = match frame {
                        SseFrame::Epoch(epoch) => LogMsg::Epoch { epoch },
                        SseFrame::Line(data) => LogMsg::Line { data },
                    };
                    if !send(msg) {
                        return StreamOutcome::SinkClosed;
                    }
                }
            }
            // End of body (daemon closed the stream) or a read error — both are transient disconnects.
            Ok(None) | Err(_) => return StreamOutcome::Disconnected,
        }
    }
}

/// The bridge task: (re)resolve the live daemon target, stream from it, and — on any drop or while the
/// daemon is not yet up — announce [`LogMsg::Reconnecting`] and back off before retrying, until the sink
/// closes. Resolving `base_url` fresh on every attempt (never caching) is essential: a daemon restart
/// rebinds a new loopback port, and the supervisor hands back the new URL — mirroring the per-request
/// resolution in [`crate::apiproxy`]. On reconnect the daemon replays its backlog and re-announces the
/// epoch; the web hook's seq de-dup suppresses the replay and an epoch change resets its watermark.
async fn run_bridge<B, F>(client: reqwest::Client, base_url: B, send: F, retry: Duration)
where
    B: Fn() -> Option<String>,
    F: Fn(LogMsg) -> bool,
{
    loop {
        if let Some(base) = base_url()
            && stream_once(&client, &base, &send).await == StreamOutcome::SinkClosed
        {
            return;
        }
        if !send(LogMsg::Reconnecting) {
            return;
        }
        tokio::time::sleep(retry).await;
    }
}

/// Owns the in-flight log-bridge tasks, keyed by the subscribing [`Channel`]'s id. Managed as Tauri
/// state; the `start_log_stream` / `stop_log_stream` commands drive it. Holds a no-request-timeout
/// [`reqwest::Client`] (an infinite stream must never be cut short by a client deadline).
///
/// Keying by channel id (rather than a single "current" slot) makes start/stop race-proof: a Logs view
/// that unmounts and immediately remounts (React StrictMode, or a rapid tab switch) issues start(A),
/// stop(A), start(B) — stopping stream A must not touch stream B. There is normally exactly one entry.
#[derive(Default)]
pub struct LogBridge {
    client: reqwest::Client,
    streams: Mutex<HashMap<u32, JoinHandle<()>>>,
}

impl LogBridge {
    /// Starts a bridge for `channel`: spawns the streaming task, forwarding each message over it, and
    /// registers the task under the channel's id (aborting any prior task under the same id — defensive,
    /// since ids are unique). `base_url` is resolved per connection attempt (see [`run_bridge`]).
    pub fn start<B>(&self, channel: Channel<LogMsg>, base_url: B)
    where
        B: Fn() -> Option<String> + Send + 'static,
    {
        let id = channel.id();
        let client = self.client.clone();
        // `Channel::send` errors when the receiving webview is gone; map that to "sink closed" so the
        // bridge task ends instead of leaking an upstream socket.
        let send = move |msg: LogMsg| channel.send(msg).is_ok();
        let join = tauri::async_runtime::spawn(run_bridge(client, base_url, send, RECONNECT_DELAY));
        if let Some(previous) = lock(&self.streams).insert(id, join) {
            previous.abort();
        }
    }

    /// Stops the bridge for the channel with `id` (its Logs view unmounted): aborts that task and drops
    /// its upstream connection. A no-op if it already ended. Mirrors closing one `EventSource`.
    pub fn stop(&self, id: u32) {
        if let Some(previous) = lock(&self.streams).remove(&id) {
            previous.abort();
        }
    }
}

/// Recovers a poisoned lock rather than propagating the panic — the guarded section is a tiny
/// insert/remove on the task map, never held across an `.await` (same policy as [`crate::app`]).
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    // ---- SseDecoder ---------------------------------------------------------------------------

    fn decode_all(chunks: &[&str]) -> Vec<SseFrame> {
        let mut d = SseDecoder::new();
        let mut out = Vec::new();
        for c in chunks {
            out.extend(d.push(c.as_bytes()));
        }
        out
    }

    // The daemon's epoch frame (`handlers_logs.rs`: `event: epoch\ndata: {n}\n\n`) decodes to one Epoch.
    #[test]
    fn decodes_epoch_frame() {
        assert_eq!(
            decode_all(&["event: epoch\ndata: 7\n\n"]),
            vec![SseFrame::Epoch("7".into())]
        );
    }

    // A plain `data:` frame (the daemon's `data: {json}\n\n`) decodes to one Line carrying the raw JSON.
    #[test]
    fn decodes_line_frame() {
        assert_eq!(
            decode_all(&["data: {\"seq\":1,\"msg\":\"hi\"}\n\n"]),
            vec![SseFrame::Line("{\"seq\":1,\"msg\":\"hi\"}".into())]
        );
    }

    // Heartbeat comments (`: ping\n\n`) carry no data and must produce no frames.
    #[test]
    fn drops_heartbeat_comments() {
        assert_eq!(decode_all(&[": ping\n\n"]), vec![]);
    }

    // The full daemon opening sequence — epoch, a heartbeat, then a line — in a single chunk.
    #[test]
    fn decodes_epoch_then_line_in_one_chunk() {
        assert_eq!(
            decode_all(&["event: epoch\ndata: 3\n\n: ping\n\ndata: {\"seq\":9}\n\n"]),
            vec![
                SseFrame::Epoch("3".into()),
                SseFrame::Line("{\"seq\":9}".into()),
            ]
        );
    }

    // A frame split across two socket reads (mid-field, mid-value) must still decode once complete.
    #[test]
    fn reassembles_a_frame_split_across_chunks() {
        assert_eq!(
            decode_all(&["event: epo", "ch\ndata: 1", "2\n\n"]),
            vec![SseFrame::Epoch("12".into())]
        );
        // A partial trailing frame yields nothing until its terminator arrives.
        assert_eq!(decode_all(&["data: {\"seq\":1}\n"]), vec![]);
    }

    // CRLF line endings (a stray proxy rewrite) are tolerated: the trailing '\r' is stripped.
    #[test]
    fn tolerates_crlf_line_endings() {
        assert_eq!(
            decode_all(&["event: epoch\r\ndata: 5\r\n\r\n"]),
            vec![SseFrame::Epoch("5".into())]
        );
    }

    // A `data:` value with no leading space keeps its exact bytes (only ONE delimiter space is stripped).
    #[test]
    fn strips_only_a_single_leading_space() {
        assert_eq!(
            decode_all(&["data:  x\n\n"]),
            vec![SseFrame::Line(" x".into())]
        );
    }

    // ---- stream_once / run_bridge -------------------------------------------------------------

    /// A recording sink: collects forwarded messages, and optionally reports "closed" after `close_after`
    /// deliveries so tests can drive the SinkClosed path. Returns `false` (closed) once the cap is hit.
    #[derive(Clone)]
    struct RecordingSink {
        seen: Arc<Mutex<Vec<LogMsg>>>,
        close_after: Option<usize>,
    }

    impl RecordingSink {
        fn open() -> Self {
            RecordingSink {
                seen: Arc::new(Mutex::new(Vec::new())),
                close_after: None,
            }
        }
        fn closing_after(n: usize) -> Self {
            RecordingSink {
                seen: Arc::new(Mutex::new(Vec::new())),
                close_after: Some(n),
            }
        }
        fn messages(&self) -> Vec<LogMsg> {
            self.seen.lock().expect("lock").clone()
        }
        fn send(&self, msg: LogMsg) -> bool {
            let mut seen = self.seen.lock().expect("lock");
            seen.push(msg);
            self.close_after.is_none_or(|cap| seen.len() < cap)
        }
    }

    /// A raw-TCP fake SSE backend: serves ONE connection with the given status + body bytes, then closes
    /// the socket (EOF-delimited body). Enough to drive `stream_once` without pulling in an HTTP server.
    async fn serve_once(status_line: &'static str, body: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        tokio::spawn(async move {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            // Drain the request head (we don't inspect it) enough to let the client finish sending.
            let mut scratch = [0u8; 1024];
            let _ = stream.read(&mut scratch).await;
            let response = format!(
                "{status_line}\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{body}"
            );
            let _ = stream.write_all(response.as_bytes()).await;
            // Drop the stream → EOF, so the client's chunk reader sees the body end (Disconnected).
        });
        url
    }

    // A connected stream forwards Open, then each decoded frame, then reports Disconnected on EOF.
    #[tokio::test]
    async fn stream_once_forwards_open_epoch_and_lines() {
        let url = serve_once(
            "HTTP/1.1 200 OK",
            "event: epoch\ndata: 7\n\ndata: {\"seq\":1,\"msg\":\"a\"}\n\ndata: {\"seq\":2,\"msg\":\"b\"}\n\n",
        )
        .await;
        let sink = RecordingSink::open();
        let outcome = stream_once(&reqwest::Client::new(), &url, &|m| sink.send(m)).await;
        assert_eq!(outcome, StreamOutcome::Disconnected);
        assert_eq!(
            sink.messages(),
            vec![
                LogMsg::Open,
                LogMsg::Epoch { epoch: "7".into() },
                LogMsg::Line {
                    data: "{\"seq\":1,\"msg\":\"a\"}".into()
                },
                LogMsg::Line {
                    data: "{\"seq\":2,\"msg\":\"b\"}".into()
                },
            ]
        );
    }

    // A non-2xx upstream (daemon mid-restart) is a transient disconnect — no Open, no frames.
    #[tokio::test]
    async fn stream_once_treats_non_2xx_as_disconnect() {
        let url = serve_once("HTTP/1.1 503 Service Unavailable", "").await;
        let sink = RecordingSink::open();
        let outcome = stream_once(&reqwest::Client::new(), &url, &|m| sink.send(m)).await;
        assert_eq!(outcome, StreamOutcome::Disconnected);
        assert_eq!(sink.messages(), vec![]);
    }

    // When the sink closes mid-stream (the webview went away), stream_once stops with SinkClosed.
    #[tokio::test]
    async fn stream_once_stops_when_sink_closes() {
        let url = serve_once("HTTP/1.1 200 OK", "event: epoch\ndata: 1\n\n").await;
        // Close right after the first delivery (Open), so the next send (Epoch) reports closed.
        let sink = RecordingSink::closing_after(1);
        let outcome = stream_once(&reqwest::Client::new(), &url, &|m| sink.send(m)).await;
        assert_eq!(outcome, StreamOutcome::SinkClosed);
        assert_eq!(sink.messages(), vec![LogMsg::Open]);
    }

    // With no daemon target, run_bridge announces Reconnecting and retries — until the sink closes.
    #[tokio::test]
    async fn run_bridge_reconnects_while_daemon_absent_then_stops_on_sink_close() {
        let sink = RecordingSink::closing_after(1); // stop after the first Reconnecting
        run_bridge(
            reqwest::Client::new(),
            || None, // daemon not up
            |m| sink.send(m),
            Duration::from_millis(1),
        )
        .await;
        assert_eq!(sink.messages(), vec![LogMsg::Reconnecting]);
    }

    // After a stream drops, run_bridge emits Reconnecting before backing off (the "connecting" status).
    #[tokio::test]
    async fn run_bridge_emits_reconnecting_after_a_dropped_stream() {
        let url = serve_once("HTTP/1.1 200 OK", "event: epoch\ndata: 4\n\n").await;
        // Deliver Open + Epoch from the one connection, then stop on the following Reconnecting.
        let sink = RecordingSink::closing_after(3);
        run_bridge(
            reqwest::Client::new(),
            move || Some(url.clone()),
            |m| sink.send(m),
            Duration::from_millis(1),
        )
        .await;
        assert_eq!(
            sink.messages(),
            vec![
                LogMsg::Open,
                LogMsg::Epoch { epoch: "4".into() },
                LogMsg::Reconnecting,
            ]
        );
    }

    // LogMsg serializes with the `kind`-tagged shape the web hook switches on.
    #[test]
    fn log_msg_serializes_kind_tagged() {
        let json = |m: &LogMsg| serde_json::to_string(m).expect("serialize");
        assert_eq!(json(&LogMsg::Open), r#"{"kind":"open"}"#);
        assert_eq!(json(&LogMsg::Reconnecting), r#"{"kind":"reconnecting"}"#);
        assert_eq!(
            json(&LogMsg::Epoch { epoch: "7".into() }),
            r#"{"kind":"epoch","epoch":"7"}"#
        );
        assert_eq!(
            json(&LogMsg::Line {
                data: "{\"seq\":1}".into()
            }),
            r#"{"kind":"line","data":"{\"seq\":1}"}"#
        );
    }
}
