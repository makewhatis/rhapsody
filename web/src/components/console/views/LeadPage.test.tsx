// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { LeadPage, type LeadDecision } from "./LeadPage";

afterEach(cleanup);
const row: LeadDecision = { id: 1, subject: "TEST-1", trigger: "blocked_handoff", at: "2026-10-08T10:00:00Z", decision: "done: requeue", reasoning: "Zero verdicts means retry, not adjudication.", evidence: "Three 401s in the run ledger.", actions: "requeue", harness: "opencode", model: "openai/gpt-test" };
describe("LeadPage", () => {
  it("shows the ticket as held with its latest need and escalation repeat count", () => {
    render(<LeadPage decisions={[]} held={[{ id: 1, subject: "TEST-1", kind: "escalation", need: "Supply the corrected acceptance input", at: row.at, repeat_count: 2, decision: 7 }]} onOverrule={vi.fn()} />);
    expect(screen.getByText("Held")).toBeTruthy();
    expect(screen.getByText("Supply the corrected acceptance input")).toBeTruthy();
    expect(screen.getByText("2 escalations")).toBeTruthy();
  });
  it("resolves an escalation with an operator note and reports resolved, not overruled", async () => {
    const resolve = vi.fn().mockResolvedValue(undefined);
    render(<LeadPage decisions={[{ ...row, decision: "escalate: evidence needed", escalation_episode: 4 }]} onOverrule={vi.fn()} onResolve={resolve} />);
    fireEvent.click(screen.getByRole("button", { name: "Resolve" }));
    fireEvent.change(screen.getByRole("textbox", { name: "Resolution note" }), { target: { value: "Acceptance corrected" } });
    fireEvent.click(screen.getByRole("button", { name: "Resolve escalation" }));
    await waitFor(() => expect(resolve).toHaveBeenCalledWith(1, "Acceptance corrected"));
    expect(screen.getByText("Resolved")).toBeTruthy();
    expect(screen.queryByText("Overruled")).toBeNull();
  });
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
