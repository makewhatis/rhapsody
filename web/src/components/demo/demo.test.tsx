// @vitest-environment jsdom
import { afterEach, describe, expect, it } from "vitest";
import { act, cleanup, render, renderHook, screen, fireEvent } from "@testing-library/react";
import { isDemoRoute, useIsDemoRoute } from "@/components/demo/route";
import PrimitiveGallery from "@/components/demo/PrimitiveGallery";

afterEach(() => {
  cleanup();
  window.location.hash = "";
});

describe("isDemoRoute", () => {
  it("matches the #/demo hash and a /demo path, nothing else", () => {
    expect(isDemoRoute({ pathname: "/", hash: "#/demo" })).toBe(true);
    expect(isDemoRoute({ pathname: "/demo", hash: "" })).toBe(true);
    expect(isDemoRoute({ pathname: "/demo/", hash: "" })).toBe(true);
    expect(isDemoRoute({ pathname: "/demo//", hash: "" })).toBe(true);
    expect(isDemoRoute({ pathname: "/app/demo", hash: "" })).toBe(true);
    expect(isDemoRoute({ pathname: "/", hash: "" })).toBe(false);
    expect(isDemoRoute({ pathname: "/runs", hash: "#/settings" })).toBe(false);
    expect(isDemoRoute({ pathname: "/predemo", hash: "" })).toBe(false);
    expect(isDemoRoute({ pathname: "/demonstration", hash: "" })).toBe(false);
  });
});

describe("useIsDemoRoute", () => {
  it("re-evaluates when the hash changes (no full reload needed)", () => {
    window.location.hash = "";
    const { result } = renderHook(() => useIsDemoRoute());
    expect(result.current).toBe(false);
    act(() => {
      window.location.hash = "#/demo";
      window.dispatchEvent(new Event("hashchange"));
    });
    expect(result.current).toBe(true);
    act(() => {
      window.location.hash = "";
      window.dispatchEvent(new Event("hashchange"));
    });
    expect(result.current).toBe(false);
  });
});

describe("PrimitiveGallery", () => {
  const SECTIONS = [
    "Buttons",
    "Status",
    "Pills",
    "Cards",
    "Fields & inputs",
    "Stepper",
    "Select",
    "Toggle & Checkbox",
    "Chips",
    "Collapsible",
    "Skeletons",
    "Icons",
  ];

  it("renders a section heading for every primitive group", () => {
    render(<PrimitiveGallery />);
    for (const s of SECTIONS) {
      expect(screen.getByRole("heading", { name: s })).toBeTruthy();
    }
  });

  it("renders the full status set (every STATUS_META label)", () => {
    render(<PrimitiveGallery />);
    expect(screen.getAllByText("in review").length).toBeGreaterThan(0);
    expect(screen.getAllByText("completed").length).toBeGreaterThan(0);
    expect(screen.getAllByText("stopped").length).toBeGreaterThan(0);
  });

  it("has working interactive primitives (toggle flips state)", () => {
    render(<PrimitiveGallery />);
    const firstSwitch = screen.getAllByRole("switch")[0];
    const before = firstSwitch.getAttribute("aria-checked");
    fireEvent.click(firstSwitch);
    expect(firstSwitch.getAttribute("aria-checked")).not.toBe(before);
  });
});
