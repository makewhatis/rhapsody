// @vitest-environment jsdom
// STUDIO-681 §10 box 1.5 — "Stepper steps by 1, and by 1000 when value >= 1000."
//
// The arithmetic is the prototype's, verbatim:
//     +  ->  n + (n >= 1000 ? 1000 : 1)
//     -  ->  Math.max(min, n - (n > 1000 ? 1000 : 1))
// Note the deliberate asymmetry at exactly 1000 (`>=` up, `>` down): stepping UP from
// 1000 jumps to 2000, stepping DOWN from 1000 lands on 999 rather than on 0. Spec §1.3's
// one-line summary does not draw that boundary; §0 makes the prototype's markup
// authoritative where the doc leaves a detail open, so these tests pin the prototype.
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { Stepper, stepperDecrement, stepperIncrement } from "./Stepper";

afterEach(cleanup);

describe("step size", () => {
  it("steps by 1 below 1000", () => {
    expect(stepperIncrement(8)).toBe(9);
    expect(stepperDecrement(8)).toBe(7);
    expect(stepperIncrement(999)).toBe(1000);
  });

  it("steps by 1000 at and above 1000 going up", () => {
    expect(stepperIncrement(1000)).toBe(2000);
    expect(stepperIncrement(60000)).toBe(61000);
  });

  it("steps by 1000 above 1000 going down, and by 1 at the 1000 boundary", () => {
    expect(stepperDecrement(60000)).toBe(59000);
    expect(stepperDecrement(1001)).toBe(1);
    // The prototype's `n > 1000` guard: 1000 is the floor of the coarse range.
    expect(stepperDecrement(1000)).toBe(999);
  });

  it("never steps below the minimum", () => {
    expect(stepperDecrement(0)).toBe(0);
    expect(stepperDecrement(1)).toBe(0);
    expect(stepperDecrement(3, 2)).toBe(2);
  });

  it("treats a non-finite value as the minimum rather than producing NaN", () => {
    expect(stepperIncrement(Number.NaN)).toBe(1);
    expect(stepperDecrement(Number.NaN)).toBe(0);
  });
});

describe("the rendered control", () => {
  it("shows the value and reports each step through onChange", () => {
    const onChange = vi.fn();
    render(<Stepper value={8} onChange={onChange} label="Recall top-k" />);

    expect(screen.getByRole("spinbutton", { name: "Recall top-k" })).toHaveProperty("value", "8");

    fireEvent.click(screen.getByRole("button", { name: "Increase Recall top-k" }));
    expect(onChange).toHaveBeenLastCalledWith(9);

    fireEvent.click(screen.getByRole("button", { name: "Decrease Recall top-k" }));
    expect(onChange).toHaveBeenLastCalledWith(7);
  });

  it("steps a four-digit value by 1000 from the buttons", () => {
    const onChange = vi.fn();
    render(<Stepper value={60000} onChange={onChange} label="Turn timeout" unit="ms" />);
    fireEvent.click(screen.getByRole("button", { name: "Increase Turn timeout" }));
    expect(onChange).toHaveBeenLastCalledWith(61000);
    fireEvent.click(screen.getByRole("button", { name: "Decrease Turn timeout" }));
    expect(onChange).toHaveBeenLastCalledWith(59000);
  });

  it("renders the unit beside the control", () => {
    render(<Stepper value={60000} onChange={vi.fn()} label="Turn timeout" unit="ms" />);
    expect(screen.getByText("ms")).toBeTruthy();
  });

  it("accepts typed digits and falls back to the minimum on unparseable input", () => {
    const onChange = vi.fn();
    render(<Stepper value={8} onChange={onChange} label="Recall top-k" />);
    const input = screen.getByRole("spinbutton", { name: "Recall top-k" });

    fireEvent.change(input, { target: { value: "42" } });
    expect(onChange).toHaveBeenLastCalledWith(42);

    fireEvent.change(input, { target: { value: "" } });
    expect(onChange).toHaveBeenLastCalledWith(0);
  });

  it("clamps a typed value below the minimum instead of storing it", () => {
    const onChange = vi.fn();
    render(<Stepper value={5} onChange={onChange} label="Reviewers" min={1} />);
    fireEvent.change(screen.getByRole("spinbutton", { name: "Reviewers" }), { target: { value: "0" } });
    expect(onChange).toHaveBeenLastCalledWith(1);
  });
});
