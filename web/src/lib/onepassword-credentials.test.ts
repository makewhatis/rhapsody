// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { invoke } from "@tauri-apps/api/core";
import {
  boundOnePasswordIds,
  hasOnePasswordBridge,
  onePasswordConnect,
  onePasswordRemove,
  onePasswordReplace,
  onePasswordStatus,
} from "@/lib/onepassword-credentials";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));

const invokeMock = vi.mocked(invoke);

function setBridge(present: boolean) {
  if (present) (window as unknown as { __TAURI_INTERNALS__: unknown }).__TAURI_INTERNALS__ = {};
  else delete (window as { __TAURI_INTERNALS__?: unknown }).__TAURI_INTERNALS__;
}

beforeEach(() => {
  invokeMock.mockReset();
});

afterEach(() => {
  setBridge(false);
});

describe("boundOnePasswordIds", () => {
  it("collects distinct non-empty ids in first-seen order", () => {
    expect(
      boundOnePasswordIds([
        { onepassword: "studio-infra-readonly" },
        { onepassword: "k8s-studio-prod" },
        { onepassword: "studio-infra-readonly" },
        { onepassword: "" },
        {},
      ]),
    ).toEqual(["studio-infra-readonly", "k8s-studio-prod"]);
  });

  it("returns [] for an absent project list", () => {
    expect(boundOnePasswordIds(undefined)).toEqual([]);
  });
});

describe("onePassword bridge", () => {
  it("degrades to no bridge without the Tauri internals", () => {
    setBridge(false);
    expect(hasOnePasswordBridge()).toBe(false);
  });

  it("reports the bridge when the Tauri internals are present", () => {
    setBridge(true);
    expect(hasOnePasswordBridge()).toBe(true);
  });

  it("passes the id and the token exactly once to the connect command", async () => {
    setBridge(true);
    invokeMock.mockResolvedValue({ id: "studio-infra-readonly", operation: "connect", mutated: true, status: "configured" });
    await onePasswordConnect("studio-infra-readonly", "ops_sentinel");
    expect(invokeMock).toHaveBeenCalledWith("onepassword_connect", {
      id: "studio-infra-readonly",
      token: "ops_sentinel",
    });
  });

  it("invokes the replace, remove and status commands by id", async () => {
    setBridge(true);
    invokeMock.mockResolvedValue({});
    await onePasswordReplace("studio-infra-readonly", "ops_second");
    expect(invokeMock).toHaveBeenCalledWith("onepassword_replace", {
      id: "studio-infra-readonly",
      token: "ops_second",
    });
    await onePasswordRemove("studio-infra-readonly");
    expect(invokeMock).toHaveBeenCalledWith("onepassword_remove", { id: "studio-infra-readonly" });
    await onePasswordStatus("studio-infra-readonly");
    expect(invokeMock).toHaveBeenCalledWith("onepassword_status", { id: "studio-infra-readonly" });
  });
});
