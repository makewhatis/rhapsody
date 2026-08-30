// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type * as React from "react";
import type {
  TeamsOverview,
  TeamsRecallResponse,
  TeamsRoomResponse,
  StateResponse,
} from "@/lib/api";

const h = vi.hoisted(() => ({
  fetchTeamsOverview: vi.fn(),
  fetchTeamsRoom: vi.fn(),
  fetchTeamsRecall: vi.fn(),
  postTeamsInvalidate: vi.fn(),
  fetchState: vi.fn(),
}));

vi.mock("@/lib/api", async (orig) => {
  const actual = await orig<typeof import("@/lib/api")>();
  return {
    ...actual,
    fetchTeamsOverview: h.fetchTeamsOverview,
    fetchTeamsRoom: h.fetchTeamsRoom,
    fetchTeamsRecall: h.fetchTeamsRecall,
    postTeamsInvalidate: h.postTeamsInvalidate,
    fetchState: h.fetchState,
  };
});

import { TeamsPanel } from "@/components/teams/TeamsPanel";

const overview: TeamsOverview = {
  enabled: true,
  manager_mode: "labels",
  default_identity: "",
  backend: "local",
  roster: [
    { name: "alice", profile: "swe", labels: ["rust"], bank: "agent-alice", max_concurrent: 0, live_runs: 1, tickets: ["MT-9"] },
    { name: "bob", profile: "reviewer", labels: [], bank: "agent-bob", max_concurrent: 0, live_runs: 0, tickets: [] },
  ],
};

const room: TeamsRoomResponse = {
  messages: [
    {
      id: "2026-08-30:0",
      from: "@manager",
      to: "*",
      at: "2026-08-30T10:00:00Z",
      body: "assigned MT-9 to alice",
      refs: ["MT-9"],
    },
  ],
  skipped: [],
};

function recall(facts: TeamsRecallResponse["facts"]): TeamsRecallResponse {
  return { identity: "alice", facts, skipped: [] };
}

const fact = {
  id: "20260830T100000Z-run-412",
  identity: "alice",
  document_id: "run-412",
  ticket: "MT-9",
  commit_sha: "abc1234def",
  pr: "",
  run_id: "412",
  at: "2026-08-30T10:00:00Z",
  state: "valid",
  reason: "",
  content: "the mirror lock is per-repo",
};

const liveState = {
  status: "ok",
  poll_interval_ms: 2000,
  running: [{ issue_identifier: "MT-9", run_id: 412 }],
  retrying: [],
  codex_totals: { input_tokens: 0, output_tokens: 0, total_tokens: 0, seconds_running: 0 },
  rate_limits: [],
  blocked: [],
} as unknown as StateResponse;

function renderPanel(props: Partial<React.ComponentProps<typeof TeamsPanel>> = {}) {
  h.fetchTeamsOverview.mockResolvedValue(overview);
  h.fetchTeamsRoom.mockResolvedValue(room);
  h.fetchTeamsRecall.mockResolvedValue(recall([fact]));
  h.fetchState.mockResolvedValue(liveState);
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <TeamsPanel {...props} />
    </QueryClientProvider>,
  );
}

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe("TeamsPanel roster", () => {
  it("renders each identity with its profile, labels, bank and derived status", async () => {
    renderPanel();
    expect(await screen.findByRole("button", { name: "Show alice's memory" })).toBeTruthy();
    expect(screen.getByText("agent-alice")).toBeTruthy();
    expect(screen.getByText("reviewer")).toBeTruthy();
    expect(screen.getByText("1 live")).toBeTruthy();
    // An idle teammate says so rather than offering a control that goes nowhere.
    expect(screen.getByText("idle")).toBeTruthy();
  });

  it("says how tickets are assigned and whether anything is remembered", async () => {
    renderPanel();
    expect(await screen.findByText(/Assigned by labels · memory: local\./)).toBeTruthy();
  });

  // A teammate with a live run links to that run's EXISTING detail view — no new endpoint: the run
  // id is resolved from the state snapshot the shell already polls.
  it("opens the run detail for a ticket a teammate is working", async () => {
    const onOpenRun = vi.fn();
    renderPanel({ onOpenRun });
    fireEvent.click(await screen.findByRole("button", { name: "Open the run for MT-9" }));
    expect(onOpenRun).toHaveBeenCalledWith(412);
  });
});

describe("TeamsPanel room", () => {
  // Design §0.11.5: room content is untrusted and must render as QUOTED, attributed data.
  it("renders each post quoted, with its author and time", async () => {
    renderPanel();
    const body = await screen.findByText("assigned MT-9 to alice");
    expect(body.closest("blockquote")).toBeTruthy();
    expect(screen.getByText(/@manager wrote on /)).toBeTruthy();
  });

  it("offers no way to post — writes are the room's other half", async () => {
    renderPanel();
    await screen.findByText("assigned MT-9 to alice");
    expect(screen.queryByRole("button", { name: /post/i })).toBeNull();
  });
});

describe("TeamsPanel memory", () => {
  it("lists what the selected teammate remembers, quoted with its provenance", async () => {
    renderPanel();
    const content = await screen.findByText("the mirror lock is per-repo");
    expect(content.closest("blockquote")).toBeTruthy();
    expect(screen.getByText("run 412")).toBeTruthy();
    expect(screen.getByText("abc1234")).toBeTruthy();
    // An empty query IS the browse — "everything, bounded" — not a search for "".
    expect(h.fetchTeamsRecall).toHaveBeenCalledWith("alice", "");
  });

  it("switches the memory listing to another teammate", async () => {
    renderPanel();
    fireEvent.click(await screen.findByRole("button", { name: "Show bob's memory" }));
    await waitFor(() => expect(h.fetchTeamsRecall).toHaveBeenCalledWith("bob", ""));
  });

  // §5.3: a reasonless correction is unreadable to whoever finds it later, and the daemon refuses
  // one anyway — the button says so up front rather than after a round-trip.
  it("requires a reason before the invalidate button is live", async () => {
    renderPanel();
    const button = (await screen.findAllByRole("button", { name: "Invalidate" }))[0] as HTMLButtonElement;
    expect(button.disabled).toBe(true);
    fireEvent.change(screen.getByLabelText(`Reason for invalidating ${fact.id}`), {
      target: { value: "STUDIO-408 was Done on 2026-08-19" },
    });
    expect(button.disabled).toBe(false);
  });

  // The round-trip design §5.2.3 asked this button to close: reason → confirm → the fact leaves the
  // listing, with no reload.
  it("confirms, invalidates with the reason, and drops the fact from the listing", async () => {
    renderPanel();
    h.postTeamsInvalidate.mockResolvedValue({
      identity: "alice",
      fact_id: fact.id,
      invalidated: true,
      reason: "measured otherwise",
    });
    fireEvent.change(await screen.findByLabelText(`Reason for invalidating ${fact.id}`), {
      target: { value: "measured otherwise" },
    });
    fireEvent.click(screen.getAllByRole("button", { name: "Invalidate" })[0]);

    // A confirmation step stands between the click and the write.
    const dialog = await screen.findByRole("dialog", { name: "Invalidate this memory?" });
    expect(dialog.textContent).toContain("Nothing is deleted");
    // The refetch after a successful invalidate sees the bank without it.
    h.fetchTeamsRecall.mockResolvedValue(recall([]));
    fireEvent.click(screen.getAllByRole("button", { name: "Invalidate" }).at(-1)!);

    await waitFor(() =>
      expect(h.postTeamsInvalidate).toHaveBeenCalledWith("alice", fact.id, "measured otherwise"),
    );
    await waitFor(() => expect(screen.queryByText("the mirror lock is per-repo")).toBeNull());
  });

  it("surfaces the daemon's own complaint when an invalidate fails", async () => {
    renderPanel();
    h.postTeamsInvalidate.mockRejectedValue(new Error("no such record in alice's bank"));
    fireEvent.change(await screen.findByLabelText(`Reason for invalidating ${fact.id}`), {
      target: { value: "measured otherwise" },
    });
    fireEvent.click(screen.getAllByRole("button", { name: "Invalidate" })[0]);
    fireEvent.click(screen.getAllByRole("button", { name: "Invalidate" }).at(-1)!);
    expect(await screen.findByText(/no such record in alice's bank/)).toBeTruthy();
  });
});
