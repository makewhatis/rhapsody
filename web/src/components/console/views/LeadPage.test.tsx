// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { LeadPage, type LeadDecision } from "./LeadPage";

afterEach(cleanup);
const row: LeadDecision = { id: 1, subject: "TEST-1", trigger: "blocked_handoff", at: "2026-10-08T10:00:00Z", decision: "done: requeue", reasoning: "Zero verdicts means retry, not adjudication.", evidence: "Three 401s in the run ledger.", actions: "requeue", harness: "opencode", model: "openai/gpt-test" };
describe("LeadPage", () => {
  it("shows active lead work by its real subject, never the synthetic #0 coordinate", () => {
    render(<LeadPage decisions={[]} work={[{ id: 2, subject: "TEST-598", state: "running", trigger: "blocked_handoff" }]} onOverrule={vi.fn()} />);
    expect(screen.getByText("TEST-598")).toBeTruthy();
    expect(screen.getByText("Running")).toBeTruthy();
    expect(document.body.textContent).not.toContain("#0");
  });
  it("lists decisions with reasoning, evidence and a working Overrule button", async () => {
    const overrule = vi.fn().mockResolvedValue(undefined);
    render(<LeadPage decisions={[row]} onOverrule={overrule} />);
    expect(screen.getByText(row.subject)).toBeTruthy();
    expect(screen.getByText(row.reasoning)).toBeTruthy();
    expect(screen.getByText(row.evidence)).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Overrule" }));
    fireEvent.change(screen.getByRole("textbox", { name: "Operator preference" }), { target: { value: "Diagnose first." } });
    fireEvent.click(screen.getByRole("button", { name: "Submit overrule" }));
    await waitFor(() => expect(overrule).toHaveBeenCalledWith(1, "Diagnose first."));
  });
  it("shows empty and overruled states", () => {
    const view = render(<LeadPage decisions={[]} onOverrule={vi.fn()} />);
    expect(screen.getByText(/No lead decisions yet/)).toBeTruthy();
    view.rerender(<LeadPage decisions={[{ ...row, overruled_at: row.at, overrule_note: "Diagnose first." }]} onOverrule={vi.fn()} />);
    expect(screen.getByText("Overruled")).toBeTruthy();
    expect(screen.getByText("Diagnose first.")).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Overrule" })).toBeNull();
  });
});
