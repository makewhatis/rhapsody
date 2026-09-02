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

  // §2.2.1 — every slice lands DARK. This ticket is a PRECONDITION of the §10 box 6.4 flip
  // (STUDIO-687), not the flip.
  it("leaves App.tsx on the Podium dashboard (land-dark, §2.2.1)", () => {
    const app = src("../../../App.tsx");
    expect(app).not.toContain("ConsoleApp");
    expect(app).toContain("<AppShell />");
  });

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
