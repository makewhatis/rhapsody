import * as React from "react";
import { hasBridge, subscribeLogStream } from "@/lib/bindings";

// LogLine mirrors the daemon's telemetry.LogEntry on the wire (GET /api/v1/logs[/stream]), plus a
// client-assigned `uid`. `seq` resets to 0 each daemon process, so after a restart old and new lines
// share seq values — `uid` is monotonic on the client and unique across restarts, giving React a
// stable, collision-free key.
export interface LogLine {
  seq: number;
  time: string; // RFC3339
  level: string; // "DEBUG" | "INFO" | "WARN" | "ERROR"
  msg: string;
  attrs?: Record<string, string>;
  uid?: number; // client-assigned unique key (set by useLogStream on every emitted line)
}

export type LogStreamStatus = "connecting" | "open" | "closed";

// MAX_LINES bounds the on-screen buffer so a long-lived tab can't grow without limit. It
// matches the daemon ring's order of magnitude; older lines scroll out of memory.
const MAX_LINES = 2000;

// useLogStream tails the daemon process log and keeps a bounded buffer of recent lines, de-duplicated
// by `seq` so the backlog replayed on (re)connect never double-prints, reporting the connection status.
//
// Two transports, one ingestion path: in a plain browser / the daemon-origin dashboard it tails the SSE
// stream directly (GET /api/v1/logs/stream) with EventSource, which reconnects natively. In the packaged
// Tauri app the same-origin custom-protocol proxy can't forward an infinite stream, so the host bridges
// the tail over a Tauri IPC channel (TRA-252) — subscribeLogStream — which delivers the identical epoch +
// log-line frames. Both feed the same de-dup/epoch-reset logic, so a daemon restart re-populates the tail
// automatically either way. clear() blanks the visible buffer without resetting the seq watermark, so a
// post-clear reconnect won't replay what was just cleared.
export function useLogStream(): { lines: LogLine[]; status: LogStreamStatus; clear: () => void } {
  const [lines, setLines] = React.useState<LogLine[]>([]);
  const [status, setStatus] = React.useState<LogStreamStatus>("connecting");
  const lastSeq = React.useRef(0);
  // Monotonic client-side id, stamped on every emitted line for a collision-free React key (seq
  // alone repeats across a daemon restart). Never reset.
  const nextUid = React.useRef(0);
  // The daemon's stream epoch. seq resets to 0 each daemon process, so without this a restarted
  // daemon's entries (low seqs) would all fall at/under the stale watermark and be dropped — the
  // tab would freeze. A changed epoch means a new daemon stream → reset the watermark. A same-daemon
  // reconnect re-announces the SAME epoch, so the seq de-dup still suppresses the replayed backlog.
  const lastEpoch = React.useRef<string | null>(null);

  const clear = React.useCallback(() => setLines([]), []);

  React.useEffect(() => {
    // Transport-agnostic ingestion — shared by the EventSource (browser) and Tauri-channel (desktop)
    // paths so seq de-dup, epoch-reset, and uid stamping behave identically on both.
    const applyEpoch = (epoch: string) => {
      if (lastEpoch.current !== null && epoch !== lastEpoch.current) {
        lastSeq.current = 0; // daemon restarted (seq reset) → accept the new stream's entries
      }
      lastEpoch.current = epoch;
    };
    const applyLine = (raw: string) => {
      let line: LogLine;
      try {
        line = JSON.parse(raw) as LogLine;
      } catch {
        return;
      }
      if (typeof line.seq === "number") {
        if (line.seq <= lastSeq.current) return; // de-dup backlog vs live across reconnect
        lastSeq.current = line.seq;
      }
      line.uid = nextUid.current++;
      setLines((prev) => {
        const next = prev.length >= MAX_LINES ? prev.slice(prev.length - MAX_LINES + 1) : prev.slice();
        next.push(line);
        return next;
      });
    };

    // Packaged Tauri app: the custom-protocol proxy can't stream SSE, so the host forwards the daemon's
    // log tail over an IPC channel (TRA-252). `open`/`reconnecting` drive the status dot; the epoch + line
    // frames feed the shared ingestion. Returns the host-side unsubscribe as the effect cleanup.
    if (hasBridge()) {
      return subscribeLogStream((msg) => {
        switch (msg.kind) {
          case "open":
            setStatus("open");
            break;
          case "reconnecting":
            setStatus("connecting");
            break;
          case "epoch":
            applyEpoch(msg.epoch);
            break;
          case "line":
            applyLine(msg.data);
            break;
        }
      });
    }

    // Browser / daemon-origin: tail the SSE stream directly (same origin, so streaming works).
    if (typeof EventSource === "undefined") {
      setStatus("closed");
      return;
    }
    const es = new EventSource("/api/v1/logs/stream");
    es.onopen = () => setStatus("open");
    es.addEventListener("epoch", (ev: MessageEvent<string>) => applyEpoch(ev.data));
    es.onmessage = (ev: MessageEvent<string>) => applyLine(ev.data);
    es.onerror = () => {
      // EventSource retries on its own; reflect the transient drop without tearing down.
      setStatus("connecting");
    };
    return () => es.close();
  }, []);

  return { lines, status, clear };
}
