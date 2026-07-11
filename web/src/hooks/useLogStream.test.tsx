// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { act, renderHook } from "@testing-library/react";
import { useLogStream } from "@/hooks/useLogStream";

// MockEventSource stands in for the browser EventSource so a test can drive the SSE callbacks
// (the "epoch" event + log "message" events) deterministically.
class MockEventSource {
  static instances: MockEventSource[] = [];
  onopen: (() => void) | null = null;
  onmessage: ((ev: { data: string }) => void) | null = null;
  onerror: (() => void) | null = null;
  private listeners: Record<string, ((ev: { data: string }) => void)[]> = {};
  closed = false;
  constructor(public url: string) {
    MockEventSource.instances.push(this);
  }
  addEventListener(type: string, cb: (ev: { data: string }) => void) {
    (this.listeners[type] ??= []).push(cb);
  }
  close() {
    this.closed = true;
  }
  // --- test drivers ---
  epoch(v: string) {
    (this.listeners.epoch ?? []).forEach((cb) => cb({ data: v }));
  }
  push(seq: number, msg: string) {
    this.onmessage?.({ data: JSON.stringify({ seq, time: "2026-06-08T03:00:00Z", level: "INFO", msg }) });
  }
}

afterEach(() => {
  MockEventSource.instances = [];
  vi.unstubAllGlobals();
});

function msgs(lines: { msg: string }[]): string[] {
  return lines.map((l) => l.msg);
}

describe("useLogStream", () => {
  it("accepts a restarted daemon's logs — an epoch change resets the seq watermark", () => {
    vi.stubGlobal("EventSource", MockEventSource as unknown as typeof EventSource);
    const { result } = renderHook(() => useLogStream());
    const es = MockEventSource.instances[0];

    act(() => {
      es.epoch("1000");
      es.push(1, "old-1");
      es.push(2, "old-2");
    });
    expect(msgs(result.current.lines)).toEqual(["old-1", "old-2"]);

    // Daemon restarts: NEW epoch, seq resets to 1. Without the epoch reset these would all be
    // dropped (seq 1,2 ≤ the old watermark of 2) and the tab would freeze.
    act(() => {
      es.epoch("2000");
      es.push(1, "new-1");
      es.push(2, "new-2");
    });
    expect(msgs(result.current.lines)).toEqual(["old-1", "old-2", "new-1", "new-2"]);
    // seq repeats across the restart (1,2,1,2) — uids must stay unique for stable React keys.
    const uids = result.current.lines.map((l) => l.uid);
    expect(new Set(uids).size).toBe(uids.length);
  });

  it("de-dupes a replayed backlog within the same epoch (reconnect)", () => {
    vi.stubGlobal("EventSource", MockEventSource as unknown as typeof EventSource);
    const { result } = renderHook(() => useLogStream());
    const es = MockEventSource.instances[0];

    act(() => {
      es.epoch("1000");
      es.push(1, "a");
      es.push(2, "b");
      // A same-daemon reconnect re-announces the SAME epoch and replays the backlog; the seq
      // de-dup must suppress the repeats and only the genuinely-new entry (seq 3) is appended.
      es.epoch("1000");
      es.push(1, "a");
      es.push(2, "b");
      es.push(3, "c");
    });
    expect(msgs(result.current.lines)).toEqual(["a", "b", "c"]);
  });
});
