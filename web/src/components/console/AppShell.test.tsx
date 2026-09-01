// @vitest-environment jsdom
// STUDIO-681 §1.3 / §10 box 1.3 — AppShell and NavItem render from props with the states
// the prototype shows: active, an optional count, a separator, and the capability gate
// that §2.2 depends on (a disabled item is ABSENT, not greyed).
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { AppShell, type NavItemSpec } from "./AppShell";
import { NavItem } from "./NavItem";
import { JobsIcon, MemoryIcon, SettingsIcon, TeamsIcon } from "./icons";

afterEach(cleanup);

function rail(teamsEnabled: boolean): NavItemSpec[] {
  return [
    { id: "jobs", label: "Jobs", icon: <JobsIcon />, count: 6 },
    { id: "teams", label: "Teams", icon: <TeamsIcon />, enabled: teamsEnabled },
    { id: "memory", label: "Memory", icon: <MemoryIcon />, enabled: teamsEnabled },
    { id: "settings", label: "Settings", icon: <SettingsIcon />, separatorBefore: true },
  ];
}

describe("AppShell", () => {
  it("carries the theme scope so the tokens resolve for everything inside it", () => {
    const { container } = render(<AppShell items={rail(true)} active="jobs" />);
    const root = container.firstElementChild;
    expect(root?.classList.contains("rh-console")).toBe(true);
    expect(root?.classList.contains("app")).toBe(true);
  });

  it("renders the rail, the wordmark and the main column with its children", () => {
    const { container } = render(
      <AppShell items={rail(true)} active="jobs">
        <p>worklist</p>
      </AppShell>,
    );
    expect(container.querySelector(".rail")).not.toBeNull();
    expect(screen.getByText("rhapsodyd")).toBeTruthy();
    expect(container.querySelector("main.main")?.textContent).toBe("worklist");
  });

  it("renders every enabled nav item, in order, with its count", () => {
    render(<AppShell items={rail(true)} active="jobs" />);
    const links = screen.getAllByRole("link").map((a) => a.getAttribute("data-nav"));
    expect(links).toEqual(["jobs", "teams", "memory", "settings"]);
    expect(screen.getByText("6")).toBeTruthy();
  });

  it("omits a capability-disabled item from the DOM entirely, rather than greying it", () => {
    // §2.2: with teams off the rail is Jobs + Settings, and Teams/Memory must not be
    // present at all — a hidden-but-rendered row still advertises an unreachable feature.
    const { container } = render(<AppShell items={rail(false)} active="jobs" />);
    expect(screen.getAllByRole("link").map((a) => a.getAttribute("data-nav"))).toEqual(["jobs", "settings"]);
    expect(container.querySelector('[data-nav="teams"]')).toBeNull();
    expect(container.querySelector('[data-nav="memory"]')).toBeNull();
    expect(container.textContent).not.toContain("Memory");
  });

  it("marks exactly the active item, and marks it for assistive tech too", () => {
    const { container } = render(<AppShell items={rail(true)} active="teams" />);
    expect(container.querySelectorAll("a.active").length).toBe(1);
    const active = container.querySelector("a.active");
    expect(active?.getAttribute("data-nav")).toBe("teams");
    expect(active?.getAttribute("aria-current")).toBe("page");
  });

  it("highlights a parent when a child route is active", () => {
    // §2.3: `manage` highlights Teams, `job/:key` highlights Jobs. The shell is told which
    // parent to light up; it does not try to derive it.
    const { container } = render(<AppShell items={rail(true)} active="jobs" />);
    expect(container.querySelector("a.active")?.getAttribute("data-nav")).toBe("jobs");
  });

  it("draws the separator above the item that asks for one", () => {
    const { container } = render(<AppShell items={rail(true)} active="jobs" />);
    const sep = container.querySelector(".nav .sep");
    expect(sep).not.toBeNull();
    expect(sep?.nextElementSibling?.getAttribute("data-nav")).toBe("settings");
  });

  it("reports navigation by id", () => {
    const onNavigate = vi.fn();
    render(<AppShell items={rail(true)} active="jobs" onNavigate={onNavigate} />);
    fireEvent.click(screen.getByText("Teams"));
    expect(onNavigate).toHaveBeenCalledWith("teams");
  });

  it("renders the rail foot only when given one", () => {
    const { container, rerender } = render(<AppShell items={rail(true)} active="jobs" />);
    expect(container.querySelector(".rail .foot")).toBeNull();
    rerender(<AppShell items={rail(true)} active="jobs" foot={<span className="live">● live</span>} />);
    expect(container.querySelector(".rail .foot")?.textContent).toContain("live");
  });
});

describe("NavItem", () => {
  it("is a real link targeting its route hash, so the rail works without JS routing", () => {
    render(<NavItem id="teams" label="Teams" icon={<TeamsIcon />} />);
    expect(screen.getByRole("link", { name: /Teams/ }).getAttribute("href")).toBe("#teams");
  });

  it("honours an explicit href over the default hash", () => {
    render(<NavItem id="teams" label="Teams" icon={<TeamsIcon />} href="/teams" />);
    expect(screen.getByRole("link", { name: /Teams/ }).getAttribute("href")).toBe("/teams");
  });

  it("omits the count badge when there is no count", () => {
    const { container } = render(<NavItem id="teams" label="Teams" icon={<TeamsIcon />} />);
    expect(container.querySelector(".ct")).toBeNull();
  });

  it("renders a zero count rather than swallowing it as falsy", () => {
    const { container } = render(<NavItem id="jobs" label="Jobs" icon={<JobsIcon />} count={0} />);
    expect(container.querySelector(".ct")?.textContent).toBe("0");
  });

  it("is inactive by default and carries no aria-current when inactive", () => {
    const { container } = render(<NavItem id="jobs" label="Jobs" icon={<JobsIcon />} />);
    expect(container.querySelector("a")?.classList.contains("active")).toBe(false);
    expect(container.querySelector("a")?.getAttribute("aria-current")).toBeNull();
  });
});
