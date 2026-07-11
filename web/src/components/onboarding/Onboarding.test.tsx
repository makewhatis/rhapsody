// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, render, screen, fireEvent, waitFor } from "@testing-library/react";
import type { LinearProject } from "@/lib/api";

const h = {
  credentialStatus: vi.fn(),
  setLinearToken: vi.fn(),
  writeInitialConfig: vi.fn(),
  listLinearProjects: vi.fn(),
  clearLinearToken: vi.fn(),
};
vi.mock("@/lib/bindings", () => ({
  credentialStatus: () => h.credentialStatus(),
  setLinearToken: (t: string) => h.setLinearToken(t),
  writeInitialConfig: (s: string) => h.writeInitialConfig(s),
  listLinearProjects: () => h.listLinearProjects(),
  clearLinearToken: () => h.clearLinearToken(),
}));
import { Onboarding } from "@/components/onboarding/Onboarding";

const PROJECTS: LinearProject[] = [
  { id: "1", name: "Symphony App", slug: "872639248532", team: "FND", color: "#10b981" },
  { id: "2", name: "Docs", slug: "symphony-docs-aabbccdd", team: "DOCS", color: "#f5b544" },
];

beforeEach(() => {
  h.listLinearProjects.mockResolvedValue(PROJECTS);
  h.clearLinearToken.mockResolvedValue(undefined);
});

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe("Onboarding", () => {
  it("routes the pasted token to the Keychain binding and advances to the project picker", async () => {
    h.credentialStatus.mockResolvedValueOnce({ has_token: false }).mockResolvedValue({ has_token: true });
    h.setLinearToken.mockResolvedValue(undefined);
    render(<Onboarding onConfigured={vi.fn()} />);

    const input = await screen.findByLabelText("Linear API token");
    fireEvent.change(input, { target: { value: "lin_api_abcdefghij" } });
    fireEvent.click(screen.getByRole("button", { name: /Save & continue/ }));

    await waitFor(() => expect(h.setLinearToken).toHaveBeenCalledWith("lin_api_abcdefghij"));
    // Advances to the project step, which fetches real projects and shows the searchable picker.
    await screen.findByLabelText("Search your Linear projects");
    expect(h.listLinearProjects).toHaveBeenCalled();
  });

  it("keeps the continue button disabled until the token looks valid", async () => {
    h.credentialStatus.mockResolvedValue({ has_token: false });
    render(<Onboarding onConfigured={vi.fn()} />);
    await screen.findByLabelText("Linear API token");

    const btn = screen.getByRole("button", { name: /Save & continue/ }) as HTMLButtonElement;
    expect(btn.disabled).toBe(true);
    fireEvent.change(screen.getByLabelText("Linear API token"), { target: { value: "short" } });
    expect(btn.disabled).toBe(true);
    fireEvent.change(screen.getByLabelText("Linear API token"), { target: { value: "lin_api_ok" } });
    expect(btn.disabled).toBe(false);
  });

  it("writes the BARE slugId when a real project is picked, and signals completion", async () => {
    h.credentialStatus.mockResolvedValue({ has_token: true });
    h.writeInitialConfig.mockResolvedValue(undefined);
    const onConfigured = vi.fn();
    render(<Onboarding onConfigured={onConfigured} />);

    const opt = await screen.findByRole("option", { name: /Symphony App/ });
    fireEvent.click(opt);
    fireEvent.click(screen.getByRole("button", { name: /Create config & start/ }));

    await waitFor(() => expect(h.writeInitialConfig).toHaveBeenCalledWith("872639248532"));
    await waitFor(() => expect(onConfigured).toHaveBeenCalled());
  });

  it("disables create until a project is picked", async () => {
    h.credentialStatus.mockResolvedValue({ has_token: true });
    render(<Onboarding onConfigured={vi.fn()} />);
    await screen.findByRole("option", { name: /Symphony App/ });
    const btn = screen.getByRole("button", { name: /Create config & start/ }) as HTMLButtonElement;
    expect(btn.disabled).toBe(true);
    fireEvent.click(screen.getByRole("option", { name: /Symphony App/ }));
    expect((screen.getByRole("button", { name: /Create config & start/ }) as HTMLButtonElement).disabled).toBe(false);
  });

  it("surfaces a Linear error with Retry + back-to-token, and Retry re-fetches", async () => {
    h.credentialStatus.mockResolvedValue({ has_token: true });
    h.listLinearProjects.mockReset();
    h.listLinearProjects.mockRejectedValueOnce(new Error("Authentication required")).mockResolvedValue(PROJECTS);
    render(<Onboarding onConfigured={vi.fn()} />);

    expect(await screen.findByText(/Authentication required/)).toBeTruthy();
    expect(screen.getByRole("button", { name: "Back to token" })).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Retry" }));
    // Second fetch succeeds → picker renders.
    await screen.findByRole("option", { name: /Symphony App/ });
  });

  it("drops a stale project fetch that resolves after Back to token (no cross-token leak)", async () => {
    // First fetch is controlled (kept in flight); a later fetch returns a DIFFERENT (fresh) list.
    let resolveFirst: (v: LinearProject[]) => void = () => {};
    const first = new Promise<LinearProject[]>((res) => {
      resolveFirst = res;
    });
    h.listLinearProjects.mockReset();
    h.listLinearProjects.mockReturnValueOnce(first).mockResolvedValue([PROJECTS[1]]); // fresh = Docs
    h.credentialStatus
      .mockResolvedValueOnce({ has_token: true }) // mount → project step (load #1, pending)
      .mockResolvedValueOnce({ has_token: false }) // back-to-token → token step
      .mockResolvedValue({ has_token: true }); // re-save → project step (load #2, fresh)
    h.setLinearToken.mockResolvedValue(undefined);
    h.clearLinearToken.mockResolvedValue(undefined);
    render(<Onboarding onConfigured={vi.fn()} />);

    await screen.findByText(/Loading your Linear projects/);
    fireEvent.click(screen.getByRole("button", { name: "Back to token" }));
    await screen.findByLabelText("Linear API token");

    // The in-flight first fetch now resolves with the OLD token's list — it must be DROPPED.
    await act(async () => {
      resolveFirst(PROJECTS); // contains "Symphony App"
    });

    // Re-enter a token → project step re-fetches fresh (Docs), never the leaked stale "Symphony App".
    fireEvent.change(screen.getByLabelText("Linear API token"), { target: { value: "lin_api_newtoken" } });
    fireEvent.click(screen.getByRole("button", { name: /Save & continue/ }));
    await screen.findByRole("option", { name: /Docs/ });
    expect(screen.queryByRole("option", { name: /Symphony App/ })).toBeNull();
  });

  it("back-to-token clears the stored token and returns to the token step", async () => {
    h.credentialStatus.mockResolvedValueOnce({ has_token: true }).mockResolvedValue({ has_token: false });
    render(<Onboarding onConfigured={vi.fn()} />);
    await screen.findByRole("option", { name: /Symphony App/ });

    fireEvent.click(screen.getByRole("button", { name: "Back to token" }));
    await waitFor(() => expect(h.clearLinearToken).toHaveBeenCalled());
    await screen.findByLabelText("Linear API token");
  });

  describe("manual fallback", () => {
    it("normalizes a full Linear URL to the project slug (full segment) before writing", async () => {
      h.credentialStatus.mockResolvedValue({ has_token: true });
      h.writeInitialConfig.mockResolvedValue(undefined);
      render(<Onboarding onConfigured={vi.fn()} />);
      await screen.findByRole("option", { name: /Symphony App/ });

      fireEvent.click(screen.getByRole("button", { name: "Enter it manually" }));
      const input = await screen.findByLabelText("Project slug");
      fireEvent.change(input, {
        target: { value: "https://linear.app/trackai/project/symphony-app-872639248532/overview" },
      });
      fireEvent.click(screen.getByRole("button", { name: /Create config & start/ }));

      // The slug is the full path segment, not a bare hex id — it must equal the configured slugId.
      await waitFor(() => expect(h.writeInitialConfig).toHaveBeenCalledWith("symphony-app-872639248532"));
    });

    it("passes a bare slug through and writes it (e.g. a plain-word slugId)", async () => {
      h.credentialStatus.mockResolvedValue({ has_token: true });
      h.writeInitialConfig.mockResolvedValue(undefined);
      render(<Onboarding onConfigured={vi.fn()} />);
      await screen.findByRole("option", { name: /Symphony App/ });

      fireEvent.click(screen.getByRole("button", { name: "Enter it manually" }));
      fireEvent.change(await screen.findByLabelText("Project slug"), { target: { value: "example-infra" } });
      fireEvent.click(screen.getByRole("button", { name: /Create config & start/ }));

      await waitFor(() => expect(h.writeInitialConfig).toHaveBeenCalledWith("example-infra"));
    });

    it("shows an inline error and never writes for un-normalizable input (URL with no project segment)", async () => {
      h.credentialStatus.mockResolvedValue({ has_token: true });
      render(<Onboarding onConfigured={vi.fn()} />);
      await screen.findByRole("option", { name: /Symphony App/ });

      fireEvent.click(screen.getByRole("button", { name: "Enter it manually" }));
      fireEvent.change(await screen.findByLabelText("Project slug"), {
        target: { value: "https://linear.app/trackai/team/FOO" },
      });
      fireEvent.click(screen.getByRole("button", { name: /Create config & start/ }));

      expect(await screen.findByText(/Couldn't find a Linear project slug/)).toBeTruthy();
      expect(h.writeInitialConfig).not.toHaveBeenCalled();
    });
  });

  it("keeps the wizard mounted on a write error and does NOT signal completion", async () => {
    h.credentialStatus.mockResolvedValue({ has_token: true });
    h.writeInitialConfig.mockRejectedValue(new Error("config saved, but the daemon could not start"));
    const onConfigured = vi.fn();
    render(<Onboarding onConfigured={onConfigured} />);

    fireEvent.click(await screen.findByRole("option", { name: /Symphony App/ }));
    fireEvent.click(screen.getByRole("button", { name: /Create config & start/ }));

    expect(await screen.findByText(/config saved, but the daemon could not start/)).toBeTruthy();
    expect(onConfigured).not.toHaveBeenCalled();
  });

  it("lifts a write failure to onError so it can outlive the shell's poll-driven unmount", async () => {
    h.credentialStatus.mockResolvedValue({ has_token: true });
    h.writeInitialConfig.mockRejectedValue(new Error("config saved, but the daemon could not start"));
    const onError = vi.fn();
    render(<Onboarding onConfigured={vi.fn()} onError={onError} />);

    fireEvent.click(await screen.findByRole("option", { name: /Symphony App/ }));
    fireEvent.click(screen.getByRole("button", { name: /Create config & start/ }));

    await waitFor(() =>
      expect(onError).toHaveBeenLastCalledWith("config saved, but the daemon could not start"),
    );
  });
});
