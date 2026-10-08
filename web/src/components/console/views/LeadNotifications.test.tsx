// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { LeadDecision } from "@/lib/api";

const h = vi.hoisted(() => ({ fetchLeadDecisions: vi.fn(), fetchVersion: vi.fn() }));
vi.mock("@/lib/api", async (orig) => ({
  ...await orig<typeof import("@/lib/api")>(),
  fetchLeadDecisions: h.fetchLeadDecisions,
  fetchVersion: h.fetchVersion,
}));
const { LeadNotifications } = await import("./LeadNotifications");
const row: LeadDecision = {
  id: 2, subject: "STUDIO-598", trigger: "blocked_handoff", at: "2026-10-08T10:00:00Z",
  decision: "done: requeue", reasoning: "A full paragraph of reasoning that belongs on the Lead page only.",
  evidence: "missing record", actions: "[]", harness: "opencode", model: "openai/test",
};
function mount() {
  return render(<QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}><LeadNotifications /></QueryClientProvider>);
}
afterEach(() => { cleanup(); vi.resetAllMocks(); });
beforeEach(() => { h.fetchVersion.mockResolvedValue({ teams_enabled: true, lead_enabled: true }); });
describe("Lead notifications", () => {
  it("renders a decision as a compact informational notification with a Lead link", async () => {
    h.fetchLeadDecisions.mockResolvedValue({ decisions: [row], queued: [] });
    const { container } = mount();
    fireEvent.click(await screen.findByRole("button", { name: /Lead notifications/ }));
    const entry = screen.getByRole("listitem");
    expect(entry.textContent).toContain("Lead decided STUDIO-598: requeue");
    expect(entry.querySelector(".note.info")).toBeTruthy();
    expect(screen.getByRole("link", { name: "Reasoning and Overrule" }).getAttribute("href")).toBe("#lead");
    expect(container.textContent).not.toContain(row.reasoning);
    expect(container.textContent).not.toContain("pull requests need attention");
    expect(container.textContent).not.toContain("#0");
    expect(screen.getByRole("button", { name: /Lead notifications/ }).textContent).toContain("0 need you");
    fireEvent.keyDown(document, { key: "Escape" });
    expect(screen.queryByRole("list")).toBeNull();
    expect(document.activeElement).toBe(screen.getByRole("button", { name: /Lead notifications/ }));
  });
  it("uses the operator tone for escalations and counts only non-overruled escalations", async () => {
    h.fetchLeadDecisions.mockResolvedValue({ decisions: [row,
      { ...row, id: 3, subject: "makewhatis/rhapsody#290", decision: "escalate: needs Linear access\nto confirm the design doc" },
      { ...row, id: 4, decision: "escalate: already overruled", overruled_at: row.at },
      { ...row, id: 5, decision: "applying" },
    ], queued: [] });
    mount();
    const button = await screen.findByRole("button", { name: /Lead notifications/ });
    expect(button.textContent).toContain("1 needs you");
    fireEvent.click(button);
    const entry = screen.getByText(/Lead escalated/).closest("li");
    expect(entry?.querySelector(".note.operator")).toBeTruthy();
    expect(entry?.textContent).toContain("needs Linear access to confirm the design doc");
    expect(screen.getAllByRole("listitem")).toHaveLength(2);
  });
  it("does not fetch lead history when the capability is unavailable", async () => {
    h.fetchVersion.mockResolvedValue({ teams_enabled: true, lead_enabled: false });
    const view = mount();
    await waitFor(() => expect(h.fetchVersion).toHaveBeenCalled());
    expect(view.container.innerHTML).toBe("");
    expect(h.fetchLeadDecisions).not.toHaveBeenCalled();
  });
});
