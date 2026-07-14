// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, render, screen, fireEvent, waitFor } from "@testing-library/react";
import type { LinearProject } from "@/lib/api";
import type { ToolResult } from "@/lib/bindings";

const h = {
  credentialStatus: vi.fn(),
  setLinearToken: vi.fn(),
  writeInitialConfig: vi.fn(),
  listLinearProjects: vi.fn(),
  clearLinearToken: vi.fn(),
  probeTools: vi.fn(),
  openExternal: vi.fn(),
};
vi.mock("@/lib/bindings", () => ({
  credentialStatus: () => h.credentialStatus(),
  setLinearToken: (t: string) => h.setLinearToken(t),
  writeInitialConfig: (s: string) => h.writeInitialConfig(s),
  listLinearProjects: () => h.listLinearProjects(),
  clearLinearToken: () => h.clearLinearToken(),
  probeTools: () => h.probeTools(),
  openExternal: (url: string) => h.openExternal(url),
}));
import { Onboarding } from "@/components/onboarding/Onboarding";

const PROJECTS: LinearProject[] = [
  { id: "1", name: "Rhapsody", slug: "872639248532", team: "FND", color: "#10b981" },
  { id: "2", name: "Chamber Docs", slug: "example-docs-aabbccdd", team: "DOCS", color: "#f5b544" },
];

const TOOLS: ToolResult[] = [
  { name: "claude", path: "/opt/homebrew/bin/claude", found: true, healthy: true, version: "2.1.4", detail: "" },
  { name: "git", path: "/usr/bin/git", found: true, healthy: true, version: "2.44.0", detail: "" },
  { name: "gh", path: "/opt/homebrew/bin/gh", found: true, healthy: true, version: "2.62.0", detail: "" },
];

beforeEach(() => {
  h.listLinearProjects.mockResolvedValue(PROJECTS);
  h.clearLinearToken.mockResolvedValue(undefined);
  h.probeTools.mockResolvedValue(TOOLS);
});

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

// Drive the wizard from the (token-present) project step to the sound-check step by picking the
// first project and clicking Continue.
async function reachSoundCheck() {
  fireEvent.click(await screen.findByRole("radio", { name: "Rhapsody" }));
  fireEvent.click(screen.getByRole("button", { name: "Continue" }));
  await screen.findByText(/STEP 3 OF 3/);
}

describe("Onboarding wizard", () => {
  describe("step 1 — Connect Linear", () => {
    it("routes the pasted token to the Keychain binding and advances to the project picker", async () => {
      h.credentialStatus.mockResolvedValueOnce({ has_token: false }).mockResolvedValue({ has_token: true });
      h.setLinearToken.mockResolvedValue(undefined);
      render(<Onboarding onConfigured={vi.fn()} />);

      expect(await screen.findByText(/STEP 1 OF 3 — CONNECT LINEAR/)).toBeTruthy();
      const input = await screen.findByLabelText("Linear API token");
      fireEvent.change(input, { target: { value: "lin_api_abcdefghij" } });
      fireEvent.click(screen.getByRole("button", { name: "Continue" }));

      await waitFor(() => expect(h.setLinearToken).toHaveBeenCalledWith("lin_api_abcdefghij"));
      // Advances to step 2, which fetches real projects and shows the radio list.
      expect(await screen.findByText(/STEP 2 OF 3 — CHOOSE WHAT TO WATCH/)).toBeTruthy();
      await screen.findByRole("radio", { name: "Rhapsody" });
      expect(h.listLinearProjects).toHaveBeenCalled();
    });

    it("keeps Continue disabled until the token looks valid", async () => {
      h.credentialStatus.mockResolvedValue({ has_token: false });
      render(<Onboarding onConfigured={vi.fn()} />);
      await screen.findByLabelText("Linear API token");

      const btn = screen.getByRole("button", { name: "Continue" }) as HTMLButtonElement;
      expect(btn.disabled).toBe(true);
      fireEvent.change(screen.getByLabelText("Linear API token"), { target: { value: "short" } });
      expect(btn.disabled).toBe(true);
      fireEvent.change(screen.getByLabelText("Linear API token"), { target: { value: "lin_api_ok" } });
      expect(btn.disabled).toBe(false);
    });

    it("opens the Linear API-key page from the create-token link", async () => {
      h.credentialStatus.mockResolvedValue({ has_token: false });
      render(<Onboarding onConfigured={vi.fn()} />);
      fireEvent.click(await screen.findByRole("button", { name: /Create a token in Linear/ }));
      expect(h.openExternal).toHaveBeenCalledWith("https://linear.app/settings/account/security");
    });

    it("shows the step-1 progress marker (rust bar on step 1 of 3)", async () => {
      h.credentialStatus.mockResolvedValue({ has_token: false });
      render(<Onboarding onConfigured={vi.fn()} />);
      const bar = await screen.findByRole("progressbar");
      expect(bar.getAttribute("aria-valuenow")).toBe("1");
      expect(bar.getAttribute("aria-valuemax")).toBe("3");
    });
  });

  describe("step 2 — Choose what to watch", () => {
    beforeEach(() => h.credentialStatus.mockResolvedValue({ has_token: true }));

    it("disables Continue until a project radio is picked", async () => {
      render(<Onboarding onConfigured={vi.fn()} />);
      await screen.findByRole("radio", { name: "Rhapsody" });
      const btn = screen.getByRole("button", { name: "Continue" }) as HTMLButtonElement;
      expect(btn.disabled).toBe(true);
      fireEvent.click(screen.getByRole("radio", { name: "Rhapsody" }));
      expect((screen.getByRole("button", { name: "Continue" }) as HTMLButtonElement).disabled).toBe(false);
    });

    it("shows the model select defaulting to the compact opus-4-8 label", async () => {
      render(<Onboarding onConfigured={vi.fn()} />);
      await screen.findByRole("radio", { name: "Rhapsody" });
      expect(screen.getByText("opus-4-8")).toBeTruthy();
    });

    it("← Back clears the stored token and returns to step 1", async () => {
      h.credentialStatus.mockReset();
      h.credentialStatus.mockResolvedValueOnce({ has_token: true }).mockResolvedValue({ has_token: false });
      render(<Onboarding onConfigured={vi.fn()} />);
      await screen.findByRole("radio", { name: "Rhapsody" });

      fireEvent.click(screen.getByRole("button", { name: /Back/ }));
      await waitFor(() => expect(h.clearLinearToken).toHaveBeenCalled());
      await screen.findByLabelText("Linear API token");
      expect(screen.getByText(/STEP 1 OF 3/)).toBeTruthy();
    });

    it("surfaces a Linear error with Retry, and Retry re-fetches", async () => {
      h.listLinearProjects.mockReset();
      h.listLinearProjects.mockRejectedValueOnce(new Error("Authentication required")).mockResolvedValue(PROJECTS);
      render(<Onboarding onConfigured={vi.fn()} />);

      expect(await screen.findByText(/Authentication required/)).toBeTruthy();
      fireEvent.click(screen.getByRole("button", { name: "Retry" }));
      await screen.findByRole("radio", { name: "Rhapsody" });
    });

    it("drops a stale project fetch that resolves after Back to token (no cross-token leak)", async () => {
      // First fetch is controlled (kept in flight); a later fetch returns a DIFFERENT (fresh) list.
      let resolveFirst: (v: LinearProject[]) => void = () => {};
      const first = new Promise<LinearProject[]>((res) => {
        resolveFirst = res;
      });
      h.listLinearProjects.mockReset();
      h.listLinearProjects.mockReturnValueOnce(first).mockResolvedValue([PROJECTS[1]]); // fresh = Chamber Docs
      h.credentialStatus.mockReset();
      h.credentialStatus
        .mockResolvedValueOnce({ has_token: true }) // mount → project step (load #1, pending)
        .mockResolvedValueOnce({ has_token: false }) // back-to-token → token step
        .mockResolvedValue({ has_token: true }); // re-save → project step (load #2, fresh)
      h.setLinearToken.mockResolvedValue(undefined);
      render(<Onboarding onConfigured={vi.fn()} />);

      await screen.findByText(/Loading your Linear projects/);
      fireEvent.click(screen.getByRole("button", { name: /Back/ }));
      await screen.findByLabelText("Linear API token");

      // The in-flight first fetch now resolves with the OLD token's list — it must be DROPPED.
      await act(async () => {
        resolveFirst(PROJECTS); // contains "Rhapsody"
      });

      // Re-enter a token → project step re-fetches fresh (Chamber Docs), never the leaked stale one.
      fireEvent.change(screen.getByLabelText("Linear API token"), { target: { value: "lin_api_newtoken" } });
      fireEvent.click(screen.getByRole("button", { name: "Continue" }));
      await screen.findByRole("radio", { name: "Chamber Docs" });
      expect(screen.queryByRole("radio", { name: "Rhapsody" })).toBeNull();
    });
  });

  describe("step 3 — Sound check", () => {
    beforeEach(() => h.credentialStatus.mockResolvedValue({ has_token: true }));

    it("writes the BARE slugId of the picked project on Start playing, and signals completion", async () => {
      h.writeInitialConfig.mockResolvedValue(undefined);
      const onConfigured = vi.fn();
      render(<Onboarding onConfigured={onConfigured} />);

      await reachSoundCheck();
      fireEvent.click(screen.getByRole("button", { name: "Start playing" }));

      await waitFor(() => expect(h.writeInitialConfig).toHaveBeenCalledWith("872639248532"));
      await waitFor(() => expect(onConfigured).toHaveBeenCalled());
    });

    it("renders the preflight checklist from the tool probe (Linear + CLIs + workspace)", async () => {
      render(<Onboarding onConfigured={vi.fn()} />);
      await reachSoundCheck();
      expect(screen.getByText("Linear API")).toBeTruthy();
      expect(screen.getByText("claude")).toBeTruthy();
      expect(screen.getByText("workspace")).toBeTruthy();
      expect(screen.getByText(/2\.1\.4/)).toBeTruthy();
      const bar = screen.getByRole("progressbar");
      expect(bar.getAttribute("aria-valuenow")).toBe("3");
    });

    it("← Back returns to the project picker without clearing the token", async () => {
      render(<Onboarding onConfigured={vi.fn()} />);
      await reachSoundCheck();
      fireEvent.click(screen.getByRole("button", { name: /Back/ }));
      await screen.findByRole("radio", { name: "Rhapsody" });
      expect(h.clearLinearToken).not.toHaveBeenCalled();
      // The previously-picked project is still selected.
      expect(screen.getByRole("radio", { name: "Rhapsody" }).getAttribute("aria-checked")).toBe("true");
    });

    it("keeps the wizard mounted on a write error and does NOT signal completion", async () => {
      h.writeInitialConfig.mockRejectedValue(new Error("config saved, but the daemon could not start"));
      const onConfigured = vi.fn();
      render(<Onboarding onConfigured={onConfigured} />);

      await reachSoundCheck();
      fireEvent.click(screen.getByRole("button", { name: "Start playing" }));

      expect(await screen.findByText(/config saved, but the daemon could not start/)).toBeTruthy();
      expect(onConfigured).not.toHaveBeenCalled();
    });

    it("lifts a write failure to onError so it can outlive the shell's poll-driven unmount", async () => {
      h.writeInitialConfig.mockRejectedValue(new Error("config saved, but the daemon could not start"));
      const onError = vi.fn();
      render(<Onboarding onConfigured={vi.fn()} onError={onError} />);

      await reachSoundCheck();
      fireEvent.click(screen.getByRole("button", { name: "Start playing" }));

      await waitFor(() =>
        expect(onError).toHaveBeenLastCalledWith("config saved, but the daemon could not start"),
      );
    });
  });

  describe("manual project fallback", () => {
    beforeEach(() => h.credentialStatus.mockResolvedValue({ has_token: true }));

    it("normalizes a full Linear URL to the project slug before writing", async () => {
      h.writeInitialConfig.mockResolvedValue(undefined);
      render(<Onboarding onConfigured={vi.fn()} />);
      await screen.findByRole("radio", { name: "Rhapsody" });

      fireEvent.click(screen.getByRole("button", { name: "Enter it manually" }));
      fireEvent.change(await screen.findByLabelText("Project slug"), {
        target: { value: "https://linear.app/acme/project/rhapsody-app-872639248532/overview" },
      });
      // Continue normalizes + advances to the sound check.
      fireEvent.click(screen.getByRole("button", { name: "Continue" }));
      await screen.findByText(/STEP 3 OF 3/);
      fireEvent.click(screen.getByRole("button", { name: "Start playing" }));

      await waitFor(() => expect(h.writeInitialConfig).toHaveBeenCalledWith("rhapsody-app-872639248532"));
    });

    it("passes a bare slug through and writes it (e.g. a plain-word slugId)", async () => {
      h.writeInitialConfig.mockResolvedValue(undefined);
      render(<Onboarding onConfigured={vi.fn()} />);
      await screen.findByRole("radio", { name: "Rhapsody" });

      fireEvent.click(screen.getByRole("button", { name: "Enter it manually" }));
      fireEvent.change(await screen.findByLabelText("Project slug"), { target: { value: "example-infra" } });
      fireEvent.click(screen.getByRole("button", { name: "Continue" }));
      await screen.findByText(/STEP 3 OF 3/);
      fireEvent.click(screen.getByRole("button", { name: "Start playing" }));

      await waitFor(() => expect(h.writeInitialConfig).toHaveBeenCalledWith("example-infra"));
    });

    it("shows an inline error and does not advance for un-normalizable input", async () => {
      render(<Onboarding onConfigured={vi.fn()} />);
      await screen.findByRole("radio", { name: "Rhapsody" });

      fireEvent.click(screen.getByRole("button", { name: "Enter it manually" }));
      fireEvent.change(await screen.findByLabelText("Project slug"), {
        target: { value: "https://linear.app/acme/team/FOO" },
      });
      fireEvent.click(screen.getByRole("button", { name: "Continue" }));

      expect(await screen.findByText(/Couldn't find a Linear project slug/)).toBeTruthy();
      expect(screen.queryByText(/STEP 3 OF 3/)).toBeNull();
      expect(h.writeInitialConfig).not.toHaveBeenCalled();
    });
  });
});
