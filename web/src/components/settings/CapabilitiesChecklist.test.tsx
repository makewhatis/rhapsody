// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { CapabilitiesChecklist } from "@/components/settings/CapabilitiesChecklist";

const registry = [
  { name: "code-review", label: "Code Review", description: "Self-review the diff" },
  { name: "simplify", label: "Simplify", description: "Look for complexity" },
];

function stubRegistry(body: unknown, status = 200) {
  vi.stubGlobal(
    "fetch",
    vi.fn(
      async () =>
        new Response(JSON.stringify(body), {
          status,
          headers: { "Content-Type": "application/json" },
        }),
    ),
  );
}

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe("CapabilitiesChecklist", () => {
  it("renders a checkbox per registry entry, checking the selected ones", async () => {
    stubRegistry(registry);
    render(<CapabilitiesChecklist selected={["simplify"]} onChange={() => {}} inheritedDefault={[]} />);
    const boxes = (await screen.findAllByRole("checkbox")) as HTMLInputElement[];
    expect(boxes).toHaveLength(2);
    expect(boxes[0].checked).toBe(false); // code-review not selected
    expect(boxes[1].checked).toBe(true); // simplify selected
    expect(screen.getByText("Code Review")).toBeTruthy();
    expect(screen.getByText("Look for complexity")).toBeTruthy();
  });

  it("toggling an unselected capability adds it; toggling a selected one removes it", async () => {
    stubRegistry(registry);
    const onChange = vi.fn();
    render(<CapabilitiesChecklist selected={["simplify"]} onChange={onChange} inheritedDefault={[]} />);
    const boxes = await screen.findAllByRole("checkbox");
    fireEvent.click(boxes[0]); // add code-review (appended after existing selection)
    expect(onChange).toHaveBeenLastCalledWith(["simplify", "code-review"]);
    fireEvent.click(boxes[1]); // remove simplify
    expect(onChange).toHaveBeenLastCalledWith([]);
  });

  it("shows the inherited-default hint only when nothing is selected", async () => {
    stubRegistry(registry);
    const { rerender } = render(
      <CapabilitiesChecklist selected={[]} onChange={() => {}} inheritedDefault={["code-review"]} />,
    );
    expect(await screen.findByText(/Inheriting global default: code-review/)).toBeTruthy();
    rerender(
      <CapabilitiesChecklist selected={["simplify"]} onChange={() => {}} inheritedDefault={["code-review"]} />,
    );
    expect(screen.queryByText(/Inheriting global default/)).toBeNull();
  });

  it("surfaces a load error instead of crashing", async () => {
    stubRegistry({ error: { code: "boom", message: "nope" } }, 500);
    render(<CapabilitiesChecklist selected={[]} onChange={() => {}} inheritedDefault={[]} />);
    expect(await screen.findByText(/Could not load capabilities/)).toBeTruthy();
  });
});
