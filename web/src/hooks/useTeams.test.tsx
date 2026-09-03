// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, renderHook, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { ReactNode } from "react";

const h = vi.hoisted(() => ({ fetchTeamsRoom: vi.fn(), postTeamsRoom: vi.fn() }));

vi.mock("@/lib/api", async (orig) => {
  const actual = await orig<typeof import("@/lib/api")>();
  return { ...actual, fetchTeamsRoom: h.fetchTeamsRoom, postTeamsRoom: h.postTeamsRoom };
});

const { usePostToRoom, useTeamsRoom } = await import("@/hooks/useTeams");

/** The window the console's watch tabs read the room at, and the key they share with the dock. */
const WINDOW = 50;
const ROOM_KEY = ["teams", "room", WINDOW];

/** One room post, shaped like the daemon's. */
function roomMessage(id: string, body: string) {
  return { id, from: "operator", to: "*", at: "2026-09-03T10:00:00Z", body, refs: [] };
}

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
  h.fetchTeamsRoom.mockReset();
  h.postTeamsRoom.mockReset();
});

describe("usePostToRoom", () => {
  // The console's ask dock rests on this in a way no test of the dock itself can see. What the dock
  // may say about a question it just posted is decided by whether the read that came back could
  // have SEEN that question, and on a WARM room query that holds only because the post drops the
  // read already in flight and dispatches another — so the window a stale read was carrying never
  // lands as the newest one. Stop invalidating here, or replace the invalidate with an optimistic
  // update, and the dock is handed a snapshot from before the question while every test of IT stays
  // green: that is how it would come to report a question asked a second ago as one its read has
  // moved past. The pin is therefore on the observable outcome rather than on how react-query
  // reaches it (at the version pinned today, flipping the invalidate's own `cancelRefetch` does not
  // change it). `useReadPostdatingMount` in the console's run detail closes the same door on a COLD
  // query, which this behaviour cannot reach: a fetch is only ever cancelled on a query that
  // already holds data.
  it("discards a room read already in flight rather than letting it land as the window", async () => {
    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    const wrapper = ({ children }: { children: ReactNode }) => (
      <QueryClientProvider client={qc}>{children}</QueryClientProvider>
    );
    // Read from the cache rather than a render snapshot: what the post acts on is the query.
    const room = () => qc.getQueryCache().find({ queryKey: ROOM_KEY });

    // The first read settles, so the query is WARM — the state the ask dock ordinarily joins.
    h.fetchTeamsRoom.mockResolvedValue({ messages: [], skipped: [] });
    const { result } = renderHook(() => ({ room: useTeamsRoom(true, WINDOW), post: usePostToRoom() }), {
      wrapper,
    });
    await waitFor(() => expect(room()?.state.data).toBeDefined());
    const settled = room()?.state.dataUpdateCount ?? 0;

    // A read is in flight when the question lands — the 5s poll, or the Room tab's own refetch.
    const reads: Array<(v: unknown) => void> = [];
    h.fetchTeamsRoom.mockImplementation(() => new Promise((resolve) => reads.push(resolve)));
    await act(async () => {
      void result.current.room.refetch();
    });
    await waitFor(() => expect(reads.length).toBe(1));

    h.postTeamsRoom.mockResolvedValue({
      id: "f:9", from: "operator", to: "*", at: "2026-09-03T10:00:00Z", refs: [], delivered: 0,
    });
    await act(async () => {
      await result.current.post.mutateAsync({ body: "Why did this stop?", refs: ["run 547"] });
    });

    // The stale read answers. Its window predates the question, and it must not become the query's
    // data: the post replaced it, so what it carries is dropped on arrival.
    await act(async () => {
      reads[0]({ messages: [roomMessage("f:1", "posted before the question")], skipped: [] });
    });
    expect(room()?.state.dataUpdateCount).toBe(settled);
    expect(room()?.state.data).toEqual({ messages: [], skipped: [] });

    // And the read the post dispatched in its place is the one that does land.
    await waitFor(() => expect(reads.length).toBeGreaterThan(1));
    await act(async () => {
      reads[reads.length - 1]({
        messages: [roomMessage("f:9", "Why did this stop?")],
        skipped: [],
      });
    });
    await waitFor(() => expect(room()?.state.dataUpdateCount).toBeGreaterThan(settled));
    expect(room()?.state.data).toMatchObject({ messages: [{ id: "f:9" }] });
  });
});
