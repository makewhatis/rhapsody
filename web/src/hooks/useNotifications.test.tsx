// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { renderHook, waitFor } from "@testing-library/react";
import type { NotificationEntry, StateResponse } from "@/lib/api";
import { NOTIFICATIONS_SEEN_KEY } from "@/hooks/useNotifications";

// Drive useNotifications by mocking the live query (a mutable holder a test sets between renders)
// and the bindings (hasBridge reports the host, notifyNative records calls). The hook reads only
// `data.notifications`, so the snapshot is a minimal cast.
const h = vi.hoisted(() => ({
  bridge: { value: true },
  notifyNative: vi.fn<(title: string, body: string) => Promise<void>>(),
  data: { current: undefined as StateResponse | undefined },
}));

vi.mock("@/hooks/useStateQuery", () => ({
  useStateQuery: () => ({ data: h.data.current }),
}));

vi.mock("@/lib/bindings", () => ({
  hasBridge: () => h.bridge.value,
  notifyNative: h.notifyNative,
}));

import { useNotifications } from "@/hooks/useNotifications";

// This jsdom build has no `localStorage` global at all (see JobsView.test.tsx), so install a plain
// in-memory one — the same shape the hook writes to in the desktop webview, and the thing that must
// survive a reload for the de-dupe to hold.
function memoryStorage(): Storage {
  const m = new Map<string, string>();
  return {
    get length() {
      return m.size;
    },
    clear: () => m.clear(),
    getItem: (k: string) => m.get(k) ?? null,
    key: (i: number) => [...m.keys()][i] ?? null,
    removeItem: (k: string) => void m.delete(k),
    setItem: (k: string, v: string) => void m.set(k, v),
  } as Storage;
}

function n(over: Partial<NotificationEntry> = {}): NotificationEntry {
  return {
    id: 1,
    at: "2026-09-22T12:00:00Z",
    title: "Review loop held: STUDIO-988",
    body: "rounds=5 attempts=14 spend=anthropic 259M",
    ticket: "STUDIO-988",
    pr: "https://github.com/makewhatis/rhapsody/pull/218",
    ...over,
  };
}

function snapshot(notifications?: NotificationEntry[]): StateResponse {
  return { status: "ok", poll_interval_ms: 2000, notifications } as unknown as StateResponse;
}

beforeEach(() => {
  vi.stubGlobal("localStorage", memoryStorage());
  h.bridge.value = true;
  h.notifyNative.mockReset().mockResolvedValue(undefined);
  h.data.current = undefined;
});

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("useNotifications", () => {
  it("shows each pending notification once", async () => {
    h.data.current = snapshot([n()]);
    const { rerender } = renderHook(() => useNotifications());
    await waitFor(() => expect(h.notifyNative).toHaveBeenCalledTimes(1));
    expect(h.notifyNative).toHaveBeenCalledWith(
      "Review loop held: STUDIO-988",
      "rounds=5 attempts=14 spend=anthropic 259M",
    );

    // The 2s poll serves the SAME pending queue again — no second notification.
    rerender();
    rerender();
    await waitFor(() => expect(h.notifyNative).toHaveBeenCalledTimes(1));
  });

  it("shows a newly-crossed notification and still suppresses the ones already shown", async () => {
    h.data.current = snapshot([n()]);
    const { rerender } = renderHook(() => useNotifications());
    await waitFor(() => expect(h.notifyNative).toHaveBeenCalledTimes(1));

    // Round ten after five: a second crossing for the same ticket, plus the first still queued.
    h.data.current = snapshot([n(), n({ id: 2, at: "2026-09-23T09:00:00Z", body: "rounds=10" })]);
    rerender();
    await waitFor(() => expect(h.notifyNative).toHaveBeenCalledTimes(2));
    expect(h.notifyNative).toHaveBeenLastCalledWith("Review loop held: STUDIO-988", "rounds=10");
  });

  it("does not re-show a notification after a desktop reload", async () => {
    h.data.current = snapshot([n()]);
    const first = renderHook(() => useNotifications());
    await waitFor(() => expect(h.notifyNative).toHaveBeenCalledTimes(1));
    first.unmount();

    // A reload remounts the hook against the same daemon, whose queue is unchanged.
    renderHook(() => useNotifications());
    await waitFor(() => expect(h.notifyNative).toHaveBeenCalledTimes(1));
  });

  it("is inert without the Tauri bridge", async () => {
    h.bridge.value = false;
    h.data.current = snapshot([n()]);
    const { rerender } = renderHook(() => useNotifications());
    rerender();
    expect(h.notifyNative).not.toHaveBeenCalled();
    expect(localStorage.getItem(NOTIFICATIONS_SEEN_KEY)).toBeNull();
  });

  it("does nothing when the daemon serves no notifications key", async () => {
    h.data.current = snapshot(undefined);
    const { rerender } = renderHook(() => useNotifications());
    rerender();
    expect(h.notifyNative).not.toHaveBeenCalled();
  });
});
