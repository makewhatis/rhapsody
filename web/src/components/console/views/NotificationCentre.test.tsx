// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { afterEach, describe, expect, it, vi } from "vitest";

const h = vi.hoisted(() => ({ fetchNotifications: vi.fn(), readNotification: vi.fn() }));
vi.mock("@/lib/api", async (orig) => ({
  ...await orig<typeof import("@/lib/api")>(),
  fetchNotifications: h.fetchNotifications, readNotification: h.readNotification,
}));
const { NotificationProvider, NotificationCentre } = await import("./NotificationCentre");
const row = (id: number, group = "decisions", read_at: string | null = null) => ({
  id, group, kind: group === "needs_you" ? "lead_escalation" : "lead_decision",
  subject: `TEST-${id}`, summary: group === "needs_you" ? "Lead escalated: needs login" : "Lead decided: requeue",
  href: `#lead/decision-${id}`, at: "2026-10-08T10:00:00Z", read_at,
});
function mount() {
  return render(<QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
    <NotificationProvider><NotificationCentre /></NotificationProvider>
  </QueryClientProvider>);
}
afterEach(() => { cleanup(); vi.resetAllMocks(); });
describe("operator notification centre (STUDIO-1145 option C)", () => {
  it("groups one-line notices with age and links; only unread Needs you sets the escalation tone", async () => {
    h.fetchNotifications.mockResolvedValue({ notifications: [row(1), row(2, "needs_you"), row(3, "system"), row(4, "activity"), row(5, "needs_you", "2026-10-08T11:00:00Z")] });
    const view = mount();
    const button = await screen.findByRole("button", { name: "Notifications, 4 unread" });
    expect(button.className).toContain("operator");
    fireEvent.click(button);
    const panel = screen.getByRole("dialog", { name: "Notifications" });
    expect(within(panel).getAllByRole("heading", { level: 3 }).map((n) => n.textContent)).toEqual(["Needs you · 1", "Decisions", "System", "Activity"]);
    expect(within(panel).getAllByRole("listitem")).toHaveLength(5);
    expect(within(panel).getByText("TEST-1").closest(".note")?.className).toContain("info");
    expect(within(panel).getByText("TEST-2").closest(".note")?.className).toContain("operator");
    expect(within(panel).getByRole("link", { name: /TEST-2/ }).getAttribute("href")).toBe("#lead/decision-2");
    expect(panel.querySelectorAll("time")).toHaveLength(5);
    expect(view.container.textContent).not.toMatch(/pull requests? needs? attention|#0|Nothing has been changed/);
    fireEvent.keyDown(document, { key: "Escape" });
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(document.activeElement).toBe(button);
  });
  it("shows an expandable strip for unread Needs you only; dismiss persists read and keeps the centre entry", async () => {
    let notices = [row(1, "needs_you"), row(2)];
    h.fetchNotifications.mockImplementation(async () => ({ notifications: notices }));
    h.readNotification.mockImplementation(async (id: number) => {
      notices = notices.map((n) => n.id === id ? { ...n, read_at: "2026-10-08T11:00:00Z" } : n);
    });
    mount();
    fireEvent.click(await screen.findByRole("button", { name: "1 thing needs you" }));
    const strip = screen.getByRole("region", { name: "Needs you" });
    fireEvent.click(within(strip).getByRole("button", { name: "Dismiss TEST-1" }));
    await waitFor(() => expect(screen.queryByRole("region", { name: "Needs you" })).toBeNull());
    expect(h.readNotification).toHaveBeenCalledWith(1);
    const button = screen.getByRole("button", { name: "Notifications, 1 unread" });
    expect(button.className).not.toContain("operator");
    fireEvent.click(button);
    const panel = screen.getByRole("dialog");
    expect(within(panel).getByText("TEST-1").closest("li")?.className).toContain("read");
    expect(within(panel).getAllByRole("listitem")).toHaveLength(2);
  });
  it.each([[], [row(1)], [row(1, "system")], [row(1, "activity")], [row(1, "needs_you", "2026-10-08T11:00:00Z")]].map((notifications) => ({ notifications })))("has no strip without an unread Needs you notice: $notifications", async ({ notifications }) => {
    h.fetchNotifications.mockResolvedValue({ notifications });
    mount();
    await screen.findByRole("button", { name: `Notifications, ${notifications.filter((n) => n.read_at === null).length} unread` });
    expect(screen.queryByRole("region", { name: "Needs you" })).toBeNull();
  });
  it("reports a failed read without dismissing the actionable item", async () => {
    h.fetchNotifications.mockResolvedValue({ notifications: [row(1, "needs_you")] });
    h.readNotification.mockRejectedValue(new Error("Daemon write failed"));
    mount();
    fireEvent.click(await screen.findByRole("button", { name: "1 thing needs you" }));
    fireEvent.click(screen.getByRole("button", { name: "Dismiss TEST-1" }));
    expect(await screen.findByRole("alert")).toHaveProperty("textContent", "Daemon write failed");
    expect(screen.getByRole("region", { name: "Needs you" })).toBeTruthy();
  });
});
