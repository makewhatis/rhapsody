// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, expect, it, vi } from "vitest";
import { LeadHarnessCard } from "./LeadHarnessCard";
import type { LeadHarnessesView, LeadHarnessSaveResult } from "@/lib/api";

vi.mock("@/lib/api", async (original) => ({ ...await original<typeof import("@/lib/api")>(), fetchProviderCatalog: vi.fn().mockResolvedValue({ models: [{ id: "gpt-suggested" }] }) }));
afterEach(cleanup);
const live: LeadHarnessesView = { harnesses: [
  { harness: "opencode", model: "openai/gpt-test", effort: "xhigh", state: "passed", tested_at: "2026-10-10T10:00:00Z" },
  { harness: "claude", model: "claude-test", effort: "high", state: "failed: isolation trap fired", tested_at: "2026-10-10T10:00:00Z" },
], last_used: { harness: "opencode", model: "openai/gpt-test", effort: "xhigh" } };

it("renders the live list with each self-test state and last used entry", async () => {
  render(<LeadHarnessCard live={live} onSave={vi.fn()} />);
  expect(screen.getByRole("combobox", { name: "Model 1" })).toHaveProperty("value", "openai/gpt-test");
  expect(screen.getByText("passed")).toBeTruthy();
  expect(screen.getByText("failed: isolation trap fired")).toBeTruthy();
  expect(screen.getByText(/Most recent run: opencode openai\/gpt-test xhigh/)).toBeTruthy();
  await waitFor(() => expect(document.querySelector('option[value="openai/gpt-suggested"]')).toBeTruthy());
});
it("Save shows testing and then each entry's result", async () => {
  let finish!: (result: LeadHarnessSaveResult) => void;
  const save = vi.fn(() => new Promise<LeadHarnessSaveResult>((resolve) => { finish = resolve; }));
  render(<LeadHarnessCard live={live} onSave={save} />);
  fireEvent.click(screen.getByRole("button", { name: "Save" }));
  expect(screen.getByRole("button", { name: "Testing…" })).toHaveProperty("disabled", true);
  finish({ ok: true, active: { ...live, harnesses: live.harnesses.map((entry) => ({ ...entry, state: "passed" })) } });
  await waitFor(() => expect(screen.getAllByText("passed")).toHaveLength(2));
  expect(screen.getByRole("button", { name: "Save" })).toHaveProperty("disabled", false);
});
it("a 409 shows reasons and restores the old active list", async () => {
  render(<LeadHarnessCard live={live} onSave={vi.fn().mockResolvedValue({ ok: false, active: live, tested: [{ ...live.harnesses[0], model: "openai/new", state: "failed: unsafe built-in" }], reason: "No entry passed; the old list is still active" })} />);
  fireEvent.change(screen.getByRole("combobox", { name: "Model 1" }), { target: { value: "openai/new" } });
  fireEvent.click(screen.getByRole("button", { name: "Save" }));
  await waitFor(() => expect(screen.getByRole("alert").textContent).toContain("old list is still active"));
  expect(screen.getByText(/failed: unsafe built-in/)).toBeTruthy();
  expect(screen.getByRole("combobox", { name: "Model 1" })).toHaveProperty("value", "openai/gpt-test");
});
it("a one-entry list shows the quiet no-fallback warning", () => {
  render(<LeadHarnessCard live={{ ...live, harnesses: [live.harnesses[0]] }} onSave={vi.fn()} />);
  expect(screen.getByText("No fallback: if this model is unavailable, the lead waits")).toBeTruthy();
});
it("reordering changes the PUT order and does not send self-test metadata", async () => {
  const save = vi.fn().mockResolvedValue({ ok: true, active: live });
  render(<LeadHarnessCard live={live} onSave={save} />);
  fireEvent.click(screen.getByRole("button", { name: "Move entry 2 up" }));
  fireEvent.click(screen.getByRole("button", { name: "Save" }));
  await waitFor(() => expect(save).toHaveBeenCalledWith([
    { harness: "claude", model: "claude-test", effort: "high" },
    { harness: "opencode", model: "openai/gpt-test", effort: "xhigh" },
  ]));
});
