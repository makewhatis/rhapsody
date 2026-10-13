// @vitest-environment jsdom
import { afterEach, expect, it } from "vitest";
import { cleanup, render } from "@testing-library/react";
import { LandingBadge, landingForTicket } from "./LandingBadge";
import { PILL_COLORS } from "./Pill";

afterEach(cleanup);

for (const phase of ["WaitingForDeploy", "Watching", "AwaitingAcceptance", "Done", "Held", "Reverting", "Reopened"]) {
  it(`renders landing ${phase} using an existing pill tone`, () => {
    const { container } = render(<LandingBadge landing={{ id: 1, project_slug: "app", ticket: "APP-1", pr: "o/r#1", phase, phase_since_s: 1, phase_age_s: 42, first_failing_check: "health", hold_reason: null, pausing_auto_merge: ["o/r#2"] }} />);
    expect(container.textContent).toContain(phase);
    expect(container.textContent).toContain("health");
    expect(container.textContent).toContain("auto-merge paused");
    const pill = container.querySelector(".pill");
    expect(Object.keys(PILL_COLORS).some((tone) => pill?.classList.contains(tone))).toBe(true);
    expect(pill?.getAttribute("title")).toContain("o/r#2");
    expect(pill?.getAttribute("title")).toContain("42s");
  });
}

it("omits absent landing and does not invent paused merges", () => {
  expect(render(<LandingBadge />).container.textContent).toBe("");
});

it("shows the original ticket outcome rather than its newer revert child's Done", () => {
  const parent = { id: 1, project_slug: "app", ticket: "APP-1", pr: "o/r#1", phase: "Reopened", phase_since_s: 1, phase_age_s: 42, first_failing_check: "health", hold_reason: null, pausing_auto_merge: [], revert_of: null };
  const child = { ...parent, id: 2, pr: "o/r#2", phase: "Done", revert_of: 1 };
  expect(landingForTicket([child, parent], "APP-1")).toBe(parent);
});
