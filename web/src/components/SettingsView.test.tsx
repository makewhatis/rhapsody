// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { ConfigRequest, ConfigResponse } from "@/lib/api";

const fetchConfig = vi.fn<() => Promise<ConfigResponse>>();
const saveConfig = vi.fn<(req: ConfigRequest) => Promise<ConfigResponse>>();
vi.mock("@/lib/api", async () => {
  const actual = await vi.importActual<typeof import("@/lib/api")>("@/lib/api");
  return {
    ...actual,
    fetchConfig: () => fetchConfig(),
    saveConfig: (r: ConfigRequest) => saveConfig(r),
  };
});

import { SettingsView } from "@/components/SettingsView";

function sampleConfig(): ConfigResponse {
  return {
    config: {
      tracker: {
        kind: "linear",
        api_key: "$LINEAR_API_KEY",
        project_slug: "symphony",
        active_states: ["Todo", "In Progress"],
      },
      agent: { backend: "claude", max_concurrent_agents: 2 },
      claude: { model: "claude-opus-4-8", billing_guard: true },
    },
    prompt_body: "Do the work.",
  };
}

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

function renderView() {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  return render(
    <QueryClientProvider client={client}>
      <SettingsView />
    </QueryClientProvider>,
  );
}

describe("SettingsView", () => {
  it("loads config into the form and saves edits, preserving api_key + prompt body", async () => {
    fetchConfig.mockResolvedValue(sampleConfig());
    saveConfig.mockImplementation(async (r) => ({ ...r }));
    renderView();

    const slug = (await screen.findByLabelText(/project slug/i)) as HTMLInputElement;
    expect(slug.value).toBe("symphony");

    fireEvent.change(slug, { target: { value: "changed-slug" } });
    fireEvent.click(screen.getByRole("button", { name: /save/i }));

    await waitFor(() => expect(saveConfig).toHaveBeenCalledTimes(1));
    const req = saveConfig.mock.calls[0][0];
    const tracker = req.config.tracker as Record<string, unknown>;
    expect(tracker.project_slug).toBe("changed-slug");
    // The form never touches the credential indirection.
    expect(tracker.api_key).toBe("$LINEAR_API_KEY");
    // The advanced prompt body round-trips unchanged when not edited.
    expect(req.prompt_body).toBe("Do the work.");
  });

  it("surfaces the daemon's validation error when a save is rejected", async () => {
    fetchConfig.mockResolvedValue(sampleConfig());
    saveConfig.mockRejectedValue(new Error("review_promote_state must be one of active_states"));
    renderView();

    await screen.findByLabelText(/project slug/i);
    fireEvent.click(screen.getByRole("button", { name: /save/i }));

    await waitFor(() => expect(screen.getByText(/review_promote_state/i)).toBeTruthy());
  });

  it("shows a load error (not an endless 'Loading…') when the config fetch fails", async () => {
    fetchConfig.mockRejectedValue(new Error("http_503: config_unavailable"));
    renderView();
    await waitFor(() => expect(screen.getByText(/could not load config/i)).toBeTruthy());
    expect(screen.queryByText(/loading settings/i)).toBeNull();
  });

  it("clears the 'Saved' banner once the user edits again (no stale success)", async () => {
    fetchConfig.mockResolvedValue(sampleConfig());
    saveConfig.mockImplementation(async (r) => ({ ...r }));
    renderView();

    const slug = (await screen.findByLabelText(/project slug/i)) as HTMLInputElement;
    fireEvent.click(screen.getByRole("button", { name: /save/i }));
    await waitFor(() => expect(screen.getByText(/hot-reloaded/i)).toBeTruthy());

    // A further edit must drop the "Saved" banner and surface "Unsaved changes".
    fireEvent.change(slug, { target: { value: "edited-again" } });
    await waitFor(() => expect(screen.queryByText(/hot-reloaded/i)).toBeNull());
    expect(screen.getByText(/unsaved changes/i)).toBeTruthy();
  });
});
