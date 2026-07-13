// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { act, renderHook } from "@testing-library/react";
import type { LogStreamMsg } from "@/lib/bindings";

// Drive the Tauri-channel transport of useLogStream (the packaged app, TRA-252) by mocking the bindings:
// hasBridge() reports the host is present, and subscribeLogStream captures the message callback + returns
// an unsubscribe spy so a test can push channel frames and assert the ingestion behaves exactly like SSE.
const h = vi.hoisted(() => ({
  onMessage: null as null | ((m: LogStreamMsg) => void),
  unsub: vi.fn(),
  present: { value: true },
}));

vi.mock("@/lib/bindings", () => ({
  hasBridge: () => h.present.value,
  subscribeLogStream: (cb: (m: LogStreamMsg) => void) => {
    h.onMessage = cb;
    return h.unsub;
  },
}));

import { useLogStream } from "@/hooks/useLogStream";

afterEach(() => {
  h.onMessage = null;
  h.unsub.mockClear();
  h.present.value = true;
});

function send(m: LogStreamMsg) {
  act(() => h.onMessage?.(m));
}
function line(seq: number, msg: string, level = "INFO"): LogStreamMsg {
  return { kind: "line", data: JSON.stringify({ seq, time: "2026-06-08T03:00:00Z", level, msg }) };
}
function msgs(lines: { msg: string }[]): string[] {
  return lines.map((l) => l.msg);
}

describe("useLogStream (Tauri channel transport)", () => {
  it("appends log lines forwarded over the channel and marks the stream live on open", () => {
    const { result } = renderHook(() => useLogStream());
    expect(result.current.status).toBe("connecting"); // initial, before any frame

    send({ kind: "open" });
    expect(result.current.status).toBe("open");

    send({ kind: "epoch", epoch: "1000" });
    send(line(1, "poll tick"));
    send(line(2, "dispatch failed", "ERROR"));
    expect(msgs(result.current.lines)).toEqual(["poll tick", "dispatch failed"]);
    // uid is stamped on every emitted line (stable React key), just like the SSE path
    expect(result.current.lines.every((l) => typeof l.uid === "number")).toBe(true);
  });

  it("accepts a restarted daemon's logs — an epoch change resets the seq watermark", () => {
    const { result } = renderHook(() => useLogStream());
    send({ kind: "epoch", epoch: "1000" });
    send(line(1, "old-1"));
    send(line(2, "old-2"));
    expect(msgs(result.current.lines)).toEqual(["old-1", "old-2"]);

    // Daemon restart: new epoch, seq resets to 1. Without the reset these would drop under the stale
    // watermark of 2 and the tab would freeze — identical semantics to the SSE transport.
    send({ kind: "epoch", epoch: "2000" });
    send(line(1, "new-1"));
    send(line(2, "new-2"));
    expect(msgs(result.current.lines)).toEqual(["old-1", "old-2", "new-1", "new-2"]);
    const uids = result.current.lines.map((l) => l.uid);
    expect(new Set(uids).size).toBe(uids.length); // seq repeats across restart; uids stay unique
  });

  it("de-dupes a replayed backlog within the same epoch (reconnect)", () => {
    const { result } = renderHook(() => useLogStream());
    send({ kind: "epoch", epoch: "1000" });
    send(line(1, "a"));
    send(line(2, "b"));
    // The host reconnected to the same daemon: it re-announces the SAME epoch and replays the backlog.
    // seq de-dup must suppress the repeats; only the genuinely-new line (seq 3) is appended.
    send({ kind: "epoch", epoch: "1000" });
    send(line(1, "a"));
    send(line(2, "b"));
    send(line(3, "c"));
    expect(msgs(result.current.lines)).toEqual(["a", "b", "c"]);
  });

  it("reflects a reconnecting frame as the connecting status", () => {
    const { result } = renderHook(() => useLogStream());
    send({ kind: "open" });
    expect(result.current.status).toBe("open");
    send({ kind: "reconnecting" });
    expect(result.current.status).toBe("connecting");
  });

  it("ignores an unparseable line payload without throwing", () => {
    const { result } = renderHook(() => useLogStream());
    send({ kind: "open" });
    send({ kind: "line", data: "not json" });
    send(line(1, "good"));
    expect(msgs(result.current.lines)).toEqual(["good"]);
  });

  it("stops the host-side stream on unmount", () => {
    const { unmount } = renderHook(() => useLogStream());
    expect(h.unsub).not.toHaveBeenCalled();
    unmount();
    expect(h.unsub).toHaveBeenCalledOnce();
  });

  it("falls back to EventSource (no channel subscription) when the Tauri bridge is absent", () => {
    h.present.value = false;
    const { result } = renderHook(() => useLogStream());
    expect(h.onMessage).toBeNull(); // subscribeLogStream was not used
    // jsdom has no EventSource → the browser path reports the stream unavailable
    expect(result.current.status).toBe("closed");
  });
});
