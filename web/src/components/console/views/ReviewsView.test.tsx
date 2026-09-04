// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { ReviewJob, ReviewsResponse } from "@/lib/api";

// STUDIO-722, slice 8 — the console Reviews surface, driven through the real view against the
// three routes it has: `GET /api/v1/reviews` and the two `POST /api/v1/reviews/*` controls.
//
// The two controls are the ones worth exercising through the DOM rather than the model: they are
// the design's §15-e operator lever, the TRUSTED replacement for room-based control that §14.1's
// F-SEC finding forced, so what matters is that a click sends the pull request's own coordinate to
// the daemon's own route — and that a row nobody may act on offers no button to click.

const h = vi.hoisted(() => ({
  fetchReviews: vi.fn(),
  postReviewRerun: vi.fn(),
  postReviewDismiss: vi.fn(),
  openExternal: vi.fn(),
}));

vi.mock("@/lib/api", async (orig) => {
  const actual = await orig<typeof import("@/lib/api")>();
  return {
    ...actual,
    fetchReviews: h.fetchReviews,
    postReviewRerun: h.postReviewRerun,
    postReviewDismiss: h.postReviewDismiss,
  };
});

// STUDIO-765 — the row's pull-request link leaves the app through the `openExternal` seam.
vi.mock("@/lib/bindings", async (orig) => {
  const actual = await orig<typeof import("@/lib/bindings")>();
  return { ...actual, openExternal: h.openExternal };
});

const { ReviewsView } = await import("./ReviewsView");

const HEAD_A = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

function job(over: Partial<ReviewJob> = {}): ReviewJob {
  return {
    owner: "makewhatis",
    repo: "rhapsody",
    number: 12,
    reviewer: "bob",
    author: "alice",
    introduced_by: "handoff:STUDIO-720",
    requested_sha: HEAD_A,
    last_reviewed_sha: HEAD_A,
    status: "reviewed",
    open: true,
    ...over,
  };
}

function mount(view: ReviewsResponse) {
  h.fetchReviews.mockResolvedValue(view);
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  return render(
    <QueryClientProvider client={client}>
      {/* Polling off: every assertion here is about one served answer, and a 5s refetch would make
          the "the click refreshed the list" assertion pass for the wrong reason. */}
      <ReviewsView onNavigate={() => {}} pollMs={0} />
    </QueryClientProvider>,
  );
}

/** The `<tr>` holding `text`, so an assertion is scoped to one review rather than the table. */
function rowFor(text: string): HTMLElement {
  const cell = screen.getByText(text);
  const tr = cell.closest("tr");
  if (tr === null) throw new Error(`no row holds ${text}`);
  return tr;
}

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe("the Reviews surface", () => {
  /** **Acceptance 1.** A row per watched (PR, reviewer) with its reviewer, status and read SHA. */
  it("lists each watched pull request with its reviewer, status and reviewed SHA", async () => {
    mount({
      enabled: true,
      reviews: [
        job({ reviewer: "bob", status: "reviewed" }),
        job({ reviewer: "carol", status: "in_flight", last_reviewed_sha: "" }),
      ],
    });

    await screen.findByText("Reviewed");
    const bob = rowFor("bob");
    expect(within(bob).getByText("makewhatis/rhapsody#12")).toBeTruthy();
    expect(within(bob).getByText("Reviewed")).toBeTruthy();
    expect(within(bob).getByText("aaaaaaa")).toBeTruthy();

    const carol = rowFor("carol");
    expect(within(carol).getByText("Reviewing")).toBeTruthy();
    // No round has completed, so there is no SHA to claim was read.
    expect(within(carol).queryByText("aaaaaaa")).toBeNull();
  });

  it("links each row to the pull request on GitHub", async () => {
    mount({ enabled: true, reviews: [job()] });
    const link = await screen.findByRole("link");
    expect(link.getAttribute("href")).toBe("https://github.com/makewhatis/rhapsody/pull/12");
  });

  // STUDIO-765 — in the packaged app the href alone opened nothing; the click has to reach the OS.
  it("opens that pull request in the browser rather than dropping the click", async () => {
    mount({ enabled: true, reviews: [job()] });
    const ev = new MouseEvent("click", { bubbles: true, cancelable: true });
    fireEvent(await screen.findByRole("link"), ev);
    expect(h.openExternal).toHaveBeenCalledWith("https://github.com/makewhatis/rhapsody/pull/12");
    expect(ev.defaultPrevented).toBe(true);
  });

  /** **Acceptance 2.** Re-run POSTs the pull request's own coordinate to the trusted route. */
  it("re-runs a review through the daemon's own control, with the row's coordinate", async () => {
    h.postReviewRerun.mockResolvedValue({ pr: "makewhatis/rhapsody#12", rows: 1 });
    mount({ enabled: true, reviews: [job({ status: "approved" })] });

    fireEvent.click(await screen.findByLabelText("Re-run the review of makewhatis/rhapsody#12"));

    await waitFor(() => expect(h.postReviewRerun).toHaveBeenCalledTimes(1));
    expect(h.postReviewRerun.mock.calls[0][0]).toMatchObject({
      owner: "makewhatis",
      repo: "rhapsody",
      number: 12,
    });
    // The write invalidates the watch-set query, so the row moves under the operator's hand rather
    // than on the next poll tick.
    await waitFor(() => expect(h.fetchReviews).toHaveBeenCalledTimes(2));
  });

  /** **Acceptance 3.** Dismiss POSTs to the drop route with the same coordinate. */
  it("dismisses a pull request through the daemon's own control", async () => {
    h.postReviewDismiss.mockResolvedValue({ pr: "makewhatis/rhapsody#12", rows: 2 });
    mount({ enabled: true, reviews: [job()] });

    fireEvent.click(await screen.findByLabelText("Dismiss makewhatis/rhapsody#12 from the watch set"));
    fireEvent.click(screen.getByLabelText("Confirm dismissing makewhatis/rhapsody#12"));

    await waitFor(() => expect(h.postReviewDismiss).toHaveBeenCalledTimes(1));
    expect(h.postReviewDismiss.mock.calls[0][0]).toMatchObject({
      owner: "makewhatis",
      repo: "rhapsody",
      number: 12,
    });
    expect(h.postReviewRerun).not.toHaveBeenCalled();
  });

  // A dismissal is the one control here nothing on this console can undo: `drop_review_watch`
  // clears `open`, and BOTH controls then exclude the row — re-run reads only the live watch set —
  // so the pull request comes back solely through a fresh author hand-off. One unguarded click is
  // the wrong shape for that.
  it("does not dismiss on the first click, and says what cannot be undone", async () => {
    mount({ enabled: true, reviews: [job()] });

    fireEvent.click(await screen.findByLabelText("Dismiss makewhatis/rhapsody#12 from the watch set"));

    expect(h.postReviewDismiss).not.toHaveBeenCalled();
    const warning = screen.getByRole("group", { name: "Dismiss makewhatis/rhapsody#12?" });
    expect(warning.textContent).toMatch(/only a new hand-off/i);
    expect(warning.textContent).toMatch(/re-introduces/i);
  });

  it("abandons a dismissal on cancel, leaving the row alone", async () => {
    mount({ enabled: true, reviews: [job()] });

    fireEvent.click(await screen.findByLabelText("Dismiss makewhatis/rhapsody#12 from the watch set"));
    fireEvent.click(screen.getByLabelText("Cancel dismissing makewhatis/rhapsody#12"));

    expect(h.postReviewDismiss).not.toHaveBeenCalled();
    expect(screen.queryByRole("group")).toBeNull();
    // The row is steerable again, not stuck mid-confirmation.
    expect(screen.getByLabelText("Re-run the review of makewhatis/rhapsody#12")).toBeTruthy();
  });

  /**
   * `rows` is the ONLY thing separating "re-armed" from "accepted and changed nothing": the daemon
   * deliberately leaves an `in_flight` row alone, so a re-run of a pull request whose reviews are
   * all running answers `200 {rows: 0}` and the table looks identical afterwards. Rendering the
   * count is what stops that reading as a broken button.
   */
  it("says a re-run was a no-op rather than answering it with silence", async () => {
    h.postReviewRerun.mockResolvedValue({ pr: "makewhatis/rhapsody#12", rows: 0 });
    mount({ enabled: true, reviews: [job({ status: "in_flight" })] });

    fireEvent.click(await screen.findByLabelText("Re-run the review of makewhatis/rhapsody#12"));

    const status = await screen.findByRole("status");
    expect(status.textContent).toContain("already running");
    expect(status.textContent).toContain("nothing to re-arm");
  });

  it("says how many reviewers a re-run re-armed", async () => {
    h.postReviewRerun.mockResolvedValue({ pr: "makewhatis/rhapsody#12", rows: 2 });
    mount({ enabled: true, reviews: [job({ status: "approved" })] });

    fireEvent.click(await screen.findByLabelText("Re-run the review of makewhatis/rhapsody#12"));

    const status = await screen.findByRole("status");
    expect(status.textContent).toContain("re-armed");
    expect(status.textContent).toContain("2 reviewers");
  });

  /**
   * A dismissal leaves a RUNNING review to finish — stopping a run is `/api/v1/runs/{id}/stop`'s
   * job. So an agent is still checked out on that head under `bypassPermissions` and will still
   * post its findings, while the row reads `Dropped`. Saying nothing is how an operator concludes
   * they cancelled a review they did not.
   */
  it("says a dismissal did not stop the review that was already running", async () => {
    h.postReviewDismiss.mockResolvedValue({ pr: "makewhatis/rhapsody#12", rows: 1 });
    mount({ enabled: true, reviews: [job({ status: "in_flight" })] });

    fireEvent.click(await screen.findByLabelText("Dismiss makewhatis/rhapsody#12 from the watch set"));
    // The confirmation warns BEFORE the click, which is where it stops a misclick.
    expect(
      screen.getByRole("group", { name: "Dismiss makewhatis/rhapsody#12?" }).textContent,
    ).toMatch(/still running/i);
    fireEvent.click(screen.getByLabelText("Confirm dismissing makewhatis/rhapsody#12"));

    const status = await screen.findByRole("status");
    expect(status.textContent).toMatch(/still running/i);
  });

  /**
   * The default `active` filter hides a row the moment it is dropped, so a dismissal used to be
   * answered by the row simply vanishing. Showing the retired rows afterwards puts the `Dropped`
   * pill in front of the operator, which is the evidence the click landed.
   */
  it("reveals the retired rows after a dismissal, so the dropped row is visible", async () => {
    h.postReviewDismiss.mockResolvedValue({ pr: "makewhatis/rhapsody#12", rows: 1 });
    mount({ enabled: true, reviews: [job()] });

    fireEvent.click(await screen.findByLabelText("Dismiss makewhatis/rhapsody#12 from the watch set"));
    // What the daemon will serve on the refetch the write invalidates: the row, now dropped.
    h.fetchReviews.mockResolvedValue({
      enabled: true,
      reviews: [job({ status: "dropped", open: false })],
    });
    fireEvent.click(screen.getByLabelText("Confirm dismissing makewhatis/rhapsody#12"));

    await screen.findByRole("status");
    await waitFor(() => expect(screen.getByText("Dropped")).toBeTruthy());
  });

  it("confirms a dismissal landed, and how many rows it dropped", async () => {
    h.postReviewDismiss.mockResolvedValue({ pr: "makewhatis/rhapsody#12", rows: 2 });
    mount({ enabled: true, reviews: [job()] });

    fireEvent.click(await screen.findByLabelText("Dismiss makewhatis/rhapsody#12 from the watch set"));
    fireEvent.click(screen.getByLabelText("Confirm dismissing makewhatis/rhapsody#12"));

    const status = await screen.findByRole("status");
    expect(status.textContent).toContain("out of the watch set");
    expect(status.textContent).toContain("2 rows");
  });

  // A retired row is history. Re-running it would put a merged, closed or already-dismissed pull
  // request back into the dispatch path — which the daemon refuses anyway, so a button here could
  // only ever fail.
  it("offers no control on a retired row", async () => {
    mount({
      enabled: true,
      reviews: [job({ reviewer: "bob", status: "dropped", open: false })],
    });

    fireEvent.click(await screen.findByRole("button", { name: "All" }));
    const bob = rowFor("bob");
    expect(within(bob).getByText("retired")).toBeTruthy();
    expect(within(bob).queryByRole("button")).toBeNull();
  });

  it("hides retired rows until asked, and says how many there are", async () => {
    mount({
      enabled: true,
      reviews: [
        job({ reviewer: "bob", status: "dropped", open: false }),
        job({ reviewer: "carol", status: "in_flight", last_reviewed_sha: "" }),
      ],
    });

    await screen.findByText("Reviewing");
    expect(screen.queryByText("Dropped")).toBeNull();
    expect(screen.getByText("1 retired")).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "All" }));
    expect(screen.getByText("Dropped")).toBeTruthy();
  });

  /**
   * **Acceptance 4, the console half (§16).** Teams is on (the route would not be reachable
   * otherwise) but the review mode is not `ticketless`, so the daemon reports the subsystem
   * dormant. Say so, rather than rendering an empty table that reads as "nothing to review".
   */
  it("renders no surface at all when the daemon reports the subsystem dormant", async () => {
    mount({ enabled: false, reviews: [] });

    await screen.findByText(/Ticketless review is off on this daemon/);
    expect(screen.queryByRole("table")).toBeNull();
    expect(screen.queryByRole("button", { name: /Re-run/ })).toBeNull();
  });

  it("says the watch set is empty rather than nothing at all", async () => {
    mount({ enabled: true, reviews: [] });
    await screen.findByText("No pull requests are being watched.");
  });

  // The daemon's refusal is the one that decides what happens — an off-allowlist re-run, a pull
  // request nobody introduced — so it reaches the operator verbatim rather than as a paraphrase.
  it("reports the daemon's own refusal when a control is rejected", async () => {
    h.postReviewRerun.mockRejectedValue(new Error("no configured project owns the PR's repo"));
    mount({ enabled: true, reviews: [job()] });

    fireEvent.click(await screen.findByLabelText("Re-run the review of makewhatis/rhapsody#12"));

    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toContain("no configured project owns the PR's repo");
  });

  /**
   * A refusal must not outlive the click it belongs to. React Query holds a mutation's `error`
   * until that SAME mutation next runs, so reading the banner straight off `rerun.error` left a
   * refused re-run's warning standing over a later, successful dismissal — the operator is told the
   * daemon refused something it in fact did.
   */
  it("clears a refusal when the next control succeeds", async () => {
    h.postReviewRerun.mockRejectedValue(new Error("no configured project owns the PR's repo"));
    h.postReviewDismiss.mockResolvedValue({ pr: "makewhatis/rhapsody#12", rows: 1 });
    mount({ enabled: true, reviews: [job()] });

    fireEvent.click(await screen.findByLabelText("Re-run the review of makewhatis/rhapsody#12"));
    await screen.findByRole("alert");

    fireEvent.click(screen.getByLabelText("Dismiss makewhatis/rhapsody#12 from the watch set"));
    fireEvent.click(screen.getByLabelText("Confirm dismissing makewhatis/rhapsody#12"));

    await screen.findByRole("status");
    expect(screen.queryByRole("alert")).toBeNull();
  });
});
