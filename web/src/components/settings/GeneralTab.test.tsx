// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, fireEvent } from "@testing-library/react";
import type { GlobalConfigDTO, LinearIdentity } from "@/lib/api";
import { toUiGlobal } from "@/lib/settings-model";
import { GeneralTab, type GeneralTabProps } from "@/components/settings/GeneralTab";

vi.mock("@/lib/bindings", () => ({
  pickDirectory: vi.fn(async () => "/picked/path"),
}));
import { pickDirectory } from "@/lib/bindings";

function g(): GlobalConfigDTO {
  return {
    tracker: { kind: "linear", endpoint: "e", api_key_set: true },
    polling: { interval_ms: 30000 },
    agent: { backend: "claude", max_concurrent_agents: 8, max_turns: 20, max_retry_backoff_ms: 300000 },
    claude: { command: "claude", model: "claude-sonnet-4-6", effort: "high", permission_mode: "acceptEdits", billing_guard: true, ultracode: false, turn_timeout_ms: 120000, read_timeout_ms: 0, stall_timeout_ms: 0, mcp_config: "", extra_args: [] },
    workspace: { root: "/ws" },
    storage: { path: "/db", retention_days: 30 },
    otel: { enabled: false, endpoint: "", protocol: "grpc", service_name: "s", insecure: false },
    mcp: { enabled: true, allow_send_message: true, allow_stop: false, allow_resume: false },
    server: { port: 4317 },
    logging: { dir: "/logs" },
    repo: "r",
    active_states: ["Todo"],
    terminal_states: ["Done"],
    canceled_states: ["Cancelled"],
    review_states: [],
    review_promote_state: "Todo",
    summon_token: "@symphony",
    github_summons: false,
    milestone: "",
    labels: [],
    capabilities: [],
    prompt: "p",
    prompt_file: "",
    git_flow: "any",
    workspace_mode: "worktree",
    dependency_mode: "disabled",
    claim_mode: "assignee",
  };
}

const account: LinearIdentity = {
  connected: true,
  name: "David Johansen",
  display_name: "David",
  email: "david@example.com",
  token: "lin_api_••••3f2a",
  workspace_url_key: "acme",
};

function renderTab(over: Partial<GeneralTabProps> = {}) {
  const onChange = vi.fn();
  const onTokenChange = vi.fn();
  const onDisconnect = vi.fn();
  render(
    <GeneralTab
      value={toUiGlobal(g())}
      onChange={onChange}
      account={account}
      token=""
      onTokenChange={onTokenChange}
      onDisconnect={onDisconnect}
      {...over}
    />,
  );
  return { onChange, onTokenChange, onDisconnect };
}

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe("GeneralTab", () => {
  it("renders the connected-as identity and a disabled, coming-soon Connect Linear button", () => {
    renderTab();
    expect(screen.getByText(/Connected as David Johansen/)).toBeTruthy();
    expect(screen.getByText("david@example.com")).toBeTruthy();
    const connect = screen.getByRole("button", { name: /Connect Linear/ }) as HTMLButtonElement;
    expect(connect.disabled).toBe(true);
  });

  it("routes the pasted token to onTokenChange (keychain path), never to onChange (config)", () => {
    const { onTokenChange, onChange } = renderTab();
    const input = screen.getByPlaceholderText("Paste lin_api_…");
    fireEvent.change(input, { target: { value: "lin_api_newsecret" } });
    expect(onTokenChange).toHaveBeenCalledWith("lin_api_newsecret");
    expect(onChange).not.toHaveBeenCalled();
  });

  it("mutates global state through onChange when a limit changes", () => {
    const { onChange } = renderTab();
    // The first Stepper is "Max concurrent agents" (8 -> 9).
    fireEvent.click(screen.getAllByLabelText("Increment")[0]);
    expect(onChange).toHaveBeenCalled();
    const next = onChange.mock.calls[0][0];
    expect(next.maxConcurrent).toBe(9);
  });

  it("toggles ultracode through onChange", () => {
    const { onChange } = renderTab();
    fireEvent.click(screen.getByRole("switch", { name: "Ultracode" }));
    const next = onChange.mock.calls.at(-1)?.[0];
    expect(next.ultracode).toBe(true);
  });

  it("renders the four Agent MCP toggles reflecting the DTO defaults", () => {
    renderTab();
    // Fixture defaults: enabled/send-message on, stop/resume off.
    expect(screen.getByRole("switch", { name: "Agent MCP inject into agents" }).getAttribute("aria-checked")).toBe("true");
    expect(screen.getByRole("switch", { name: "Agent MCP allow send message" }).getAttribute("aria-checked")).toBe("true");
    expect(screen.getByRole("switch", { name: "Agent MCP allow stop" }).getAttribute("aria-checked")).toBe("false");
    expect(screen.getByRole("switch", { name: "Agent MCP allow resume" }).getAttribute("aria-checked")).toBe("false");
  });

  it("toggles mcp inject-into-agents through onChange (on -> off)", () => {
    const { onChange } = renderTab();
    fireEvent.click(screen.getByRole("switch", { name: "Agent MCP inject into agents" }));
    expect(onChange.mock.calls.at(-1)?.[0].mcpEnabled).toBe(false);
  });

  it("toggles mcp allow send message through onChange (on -> off)", () => {
    const { onChange } = renderTab();
    fireEvent.click(screen.getByRole("switch", { name: "Agent MCP allow send message" }));
    expect(onChange.mock.calls.at(-1)?.[0].mcpAllowSendMessage).toBe(false);
  });

  it("toggles mcp allow stop through onChange (off -> on)", () => {
    const { onChange } = renderTab();
    fireEvent.click(screen.getByRole("switch", { name: "Agent MCP allow stop" }));
    expect(onChange.mock.calls.at(-1)?.[0].mcpAllowStop).toBe(true);
  });

  it("toggles mcp allow resume through onChange (off -> on)", () => {
    const { onChange } = renderTab();
    fireEvent.click(screen.getByRole("switch", { name: "Agent MCP allow resume" }));
    expect(onChange.mock.calls.at(-1)?.[0].mcpAllowResume).toBe(true);
  });

  it("toggles GitHub summons through onChange (off -> on)", () => {
    const { onChange } = renderTab();
    const toggle = screen.getByRole("switch", { name: "GitHub summons" });
    expect(toggle.getAttribute("aria-checked")).toBe("false");
    fireEvent.click(toggle);
    const next = onChange.mock.calls.at(-1)?.[0];
    expect(next.githubSummons).toBe(true);
  });

  it("renders the GitHub summons toggle checked when github_summons is true", () => {
    render(
      <GeneralTab
        value={toUiGlobal({ ...g(), github_summons: true })}
        onChange={vi.fn()}
        account={account}
        token=""
        onTokenChange={vi.fn()}
        onDisconnect={vi.fn()}
      />,
    );
    expect(screen.getByRole("switch", { name: "GitHub summons" }).getAttribute("aria-checked")).toBe("true");
  });

  it("changes the git workflow through onChange (Any -> Graphite)", () => {
    const { onChange } = renderTab();
    // The git-workflow Select trigger uniquely shows the current "Any" label; open it, pick Graphite.
    fireEvent.click(screen.getByText("Any"));
    fireEvent.click(screen.getByText("Graphite"));
    const next = onChange.mock.calls.at(-1)?.[0];
    expect(next.gitFlow).toBe("graphite");
  });

  it("changes the workspace mode through onChange (Worktree -> Clone) and documents the trade-off (INF-418)", () => {
    const { onChange } = renderTab();
    // The workspace-mode hint states the clone⇄worktree trade-off.
    const hint = screen.getByText(/no cross-ticket checkout lock/);
    expect(hint.textContent).toContain("Clone");
    // The Select trigger uniquely shows the current "Worktree" label; open it and pick Clone.
    fireEvent.click(screen.getByText("Worktree"));
    fireEvent.click(screen.getByText("Clone"));
    const next = onChange.mock.calls.at(-1)?.[0];
    expect(next.workspaceMode).toBe("clone");
  });

  it("changes the dependency mode through onChange (Disabled -> DAG) and documents it (INF-320)", () => {
    const { onChange } = renderTab();
    // The dependency-mode Select trigger uniquely shows "Disabled" (the unconfigured daemon default);
    // open it and pick DAG. The hint anchors on a phrase unique to DEPENDENCY_MODE_HINT.
    const hint = screen.getByText(/How dependent tickets are sequenced/);
    expect(hint.textContent).toContain("Disabled (default)");
    fireEvent.click(screen.getByText("Disabled"));
    fireEvent.click(screen.getByText("DAG"));
    const next = onChange.mock.calls.at(-1)?.[0];
    expect(next.dependencyMode).toBe("dag");
  });

  it("disconnects through the keychain handler", () => {
    const { onDisconnect } = renderTab();
    fireEvent.click(screen.getByRole("button", { name: "Disconnect" }));
    expect(onDisconnect).toHaveBeenCalledOnce();
  });

  it("applies a folder picked through the Go binding to the workspace root", async () => {
    const { onChange } = renderTab();
    fireEvent.click(screen.getByRole("button", { name: /Choose workspace folder/ }));
    await Promise.resolve();
    await Promise.resolve();
    expect(pickDirectory).toHaveBeenCalled();
    const next = onChange.mock.calls.at(-1)?.[0];
    expect(next.workspaceRoot).toBe("/picked/path");
  });

  it("relabels the global turn timeout and edits it in minutes (INF-239)", () => {
    const { onChange } = renderTab();
    // The control lives in the collapsed "Advanced" section — expand it first. Match the section
    // toggle EXACTLY so it doesn't also catch PromptSource's "Advanced: custom path" disclosure.
    fireEvent.click(screen.getByRole("button", { name: "Advanced" }));
    // Relabelled from "Request timeout" -> "Turn timeout".
    expect(screen.getByText("Turn timeout")).toBeTruthy();
    expect(screen.queryByText("Request timeout")).toBeNull();
    // g() has turn_timeout_ms: 120000 => 2 minutes shown in the stepper (unique display value).
    const input = screen.getByDisplayValue("2");
    fireEvent.change(input, { target: { value: "5" } });
    const next = onChange.mock.calls.at(-1)?.[0];
    expect(next.requestTimeoutMin).toBe(5);
  });
});
