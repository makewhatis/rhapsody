// @vitest-environment jsdom
import { readFileSync } from "node:fs";
import path from "node:path";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import type { LinearProject } from "@/lib/api";
import type { ToolResult } from "@/lib/bindings";

// The console's first-run screen (STUDIO-692; STUDIO-681 §8.1, STUDIO-687 audit gap G2).
//
// The acceptance is REUSE: the console must reach the wizard the Podium shell already ships, on
// the data path it already has, so the two hosts cannot drift into two first-run flows. These
// tests therefore assert that the SHIPPED wizard is what rendered and that it is the shipped
// bindings it talks to — not that some wizard-shaped markup appeared.

const h = vi.hoisted(() => ({
  credentialStatus: vi.fn(),
  setLinearToken: vi.fn(),
  writeInitialConfig: vi.fn(),
  listLinearProjects: vi.fn(),
  clearLinearToken: vi.fn(),
  probeTools: vi.fn(),
  openExternal: vi.fn(),
}));

vi.mock("@/lib/bindings", async (orig) => {
  const actual = await orig<typeof import("@/lib/bindings")>();
  return { ...actual, ...h };
});

const { FirstRunView } = await import("./FirstRunView");

const PROJECTS: LinearProject[] = [
  { id: "1", name: "Rhapsody", slug: "872639248532", team: "FND", color: "#10b981" },
];

const TOOLS: ToolResult[] = [
  { name: "claude", path: "/opt/homebrew/bin/claude", found: true, healthy: true, version: "2.1.4", detail: "" },
];

function mount(props: Partial<Parameters<typeof FirstRunView>[0]> = {}) {
  return render(
    <FirstRunView
      onConfigured={props.onConfigured ?? vi.fn()}
      onError={props.onError ?? vi.fn()}
      error={props.error ?? ""}
      onDismissError={props.onDismissError ?? vi.fn()}
      overlayTitlebar={props.overlayTitlebar}
    />,
  );
}

beforeEach(() => {
  h.credentialStatus.mockResolvedValue({ has_token: true });
  h.listLinearProjects.mockResolvedValue(PROJECTS);
  h.probeTools.mockResolvedValue(TOOLS);
  h.clearLinearToken.mockResolvedValue(undefined);
});

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe("it reuses the shipped Onboarding wizard (§8.1)", () => {
  it("renders the wizard's own steps and progress, not a console rebuild", async () => {
    mount();
    // The shipped wizard's furniture: its 3-step progress marker and its step caps label.
    const bar = await screen.findByRole("progressbar", { name: "Onboarding progress" });
    expect(bar.getAttribute("aria-valuemax")).toBe("3");
    expect(await screen.findByText(/STEP 2 OF 3/)).toBeTruthy();
  });

  it("talks to the shipped first-run bindings — no endpoint of its own", async () => {
    mount();
    // The wizard's own data path: the Keychain probe, then the project list it drives step 2 from.
    await waitFor(() => expect(h.credentialStatus).toHaveBeenCalled());
    await waitFor(() => expect(h.listLinearProjects).toHaveBeenCalled());
    expect(await screen.findByRole("radio", { name: "Rhapsody" })).toBeTruthy();
  });

  it("seeds the config through writeInitialConfig and reports success to the shell", async () => {
    h.writeInitialConfig.mockResolvedValue(undefined);
    const onConfigured = vi.fn();
    mount({ onConfigured });

    fireEvent.click(await screen.findByRole("radio", { name: "Rhapsody" }));
    fireEvent.click(screen.getByRole("button", { name: "Continue" }));
    await screen.findByText(/STEP 3 OF 3/);
    fireEvent.click(screen.getByRole("button", { name: "Start playing" }));

    await waitFor(() => expect(h.writeInitialConfig).toHaveBeenCalledWith("872639248532"));
    await waitFor(() => expect(onConfigured).toHaveBeenCalled());
  });

  it("lifts a partial-write failure to the shell rather than keeping it inline", async () => {
    h.writeInitialConfig.mockRejectedValue(new Error("config saved, but the daemon could not start"));
    const onError = vi.fn();
    mount({ onError });

    fireEvent.click(await screen.findByRole("radio", { name: "Rhapsody" }));
    fireEvent.click(screen.getByRole("button", { name: "Continue" }));
    await screen.findByText(/STEP 3 OF 3/);
    fireEvent.click(screen.getByRole("button", { name: "Start playing" }));

    await waitFor(() =>
      expect(onError).toHaveBeenLastCalledWith("config saved, but the daemon could not start"),
    );
  });
});

describe("the setup chrome", () => {
  it("shows the console's identity and a SETUP marker, and no nav", async () => {
    mount();
    await screen.findByRole("progressbar", { name: "Onboarding progress" });
    expect(screen.getByText("rhapsodyd")).toBeTruthy();
    expect(screen.getByText("Setup")).toBeTruthy();
    // There is nothing to navigate to before a config exists.
    expect(document.querySelector("nav")).toBeNull();
  });

  // STUDIO-701 — on the desktop this bar is the window's title bar as well as the brand lockup:
  // macOS `titleBarStyle: "Overlay"` leaves no system bar to drag, and floats the traffic lights
  // over its left end. It takes the reserve the way Podium's horizontal toolbar did.
  it("becomes the window's title bar under an overlay title bar, and stays put without one", async () => {
    const { unmount } = mount({ overlayTitlebar: true });
    await screen.findByRole("progressbar", { name: "Onboarding progress" });
    expect(document.querySelector(".rh-console.setup.overlay-titlebar")).not.toBeNull();
    const head = document.querySelector("header.setuphead");
    expect(head?.hasAttribute("data-tauri-drag-region")).toBe(true);
    // The lockup is a CHILD of the drag region, and Tauri drags on the element the pointer is
    // over — so the bar's own background drags while its contents keep their own hit-testing.
    expect(head?.querySelector(".logo")).not.toBeNull();
    unmount();

    mount();
    await screen.findByRole("progressbar", { name: "Onboarding progress" });
    expect(document.querySelector(".overlay-titlebar")).toBeNull();
  });

  it("renders the lifted failure as a dismissable alert", async () => {
    const onDismissError = vi.fn();
    mount({ error: "config saved, but the daemon could not start", onDismissError });
    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toContain("config saved, but the daemon could not start");
    fireEvent.click(screen.getByRole("button", { name: "Dismiss" }));
    expect(onDismissError).toHaveBeenCalled();
  });

  it("renders no alert when there is no failure to report", async () => {
    mount();
    await screen.findByRole("progressbar", { name: "Onboarding progress" });
    expect(screen.queryByRole("alert")).toBeNull();
  });
});

// Source contracts — the rules this slice could break silently, checked against the source rather
// than the DOM (the `WorkflowView.test.tsx` precedent).
describe("source contracts", () => {
  const src = (rel: string) => readFileSync(path.resolve(__dirname, rel), "utf8");

  // NOTE: the §2.2.1 "land-dark" guard that used to sit here — asserting App.tsx still rendered the
  // Podium <AppShell /> — was retired by STUDIO-687's box-6.4 flip. The root is now pinned once, in
  // ConsoleApp.test.tsx ("the flip — App.tsx renders the console").

  // §9 — "no invented endpoints". The wizard owns the whole first-run data path; this view adds
  // no request of its own, so it reaches neither the API layer nor `fetch`.
  it("adds no data path of its own", () => {
    const view = src("./FirstRunView.tsx");
    expect(view).not.toMatch(/@\/lib\/api/);
    expect(view).not.toMatch(/\bfetch\(/);
    expect(view).toContain('from "@/components/onboarding/Onboarding"');
  });

  // The embedded Podium wizard renders inside `.rh-console`, where `--accent` means the brand
  // amber rather than Podium's hover background — the same trap `.wfembed` documents.
  it("restores the Podium meaning of --accent inside the embed scope", () => {
    expect(src("../../../theme/console-firstrun.css")).toMatch(
      /\.obembed\s*{[^}]*--accent:\s*var\(--bg-hover\)/,
    );
  });
});
