import { describe, expect, it } from "vitest";
import type { ToolResult } from "@/lib/bindings";
import {
  buildSoundCheck,
  DEFAULT_MODEL,
  MODEL_OPTIONS,
  normalizeProjectSlug,
  onboardingStep,
  slugValid,
  stepCapsLabel,
  stripModel,
  tokenLooksValid,
  TOTAL_STEPS,
  WORKSPACE_ROOT_DEFAULT,
} from "@/lib/onboarding-model";

describe("onboarding-model", () => {
  it("picks the token step without a token, the project step with one", () => {
    expect(onboardingStep(false)).toBe("token");
    expect(onboardingStep(true)).toBe("project");
  });

  it("validates tokens: non-empty and lin_-prefixed or >=40 chars", () => {
    expect(tokenLooksValid("")).toBe(false);
    expect(tokenLooksValid("   ")).toBe(false);
    expect(tokenLooksValid("short")).toBe(false);
    expect(tokenLooksValid("lin_api_x")).toBe(true);
    expect(tokenLooksValid("x".repeat(40))).toBe(true);
    expect(tokenLooksValid("  lin_api_trimmed  ")).toBe(true);
  });

  it("accepts any non-empty slug", () => {
    expect(slugValid("")).toBe(false);
    expect(slugValid("  ")).toBe(false);
    expect(slugValid("my-project")).toBe(true);
  });

  describe("wizard step labels", () => {
    it("has three steps", () => {
      expect(TOTAL_STEPS).toBe(3);
    });

    it("renders the caps step marker for each step", () => {
      expect(stepCapsLabel(1)).toBe("STEP 1 OF 3 — CONNECT LINEAR");
      expect(stepCapsLabel(2)).toBe("STEP 2 OF 3 — CHOOSE WHAT TO WATCH");
      expect(stepCapsLabel(3)).toBe("STEP 3 OF 3 — SOUND CHECK");
    });
  });

  describe("model select", () => {
    it("defaults to the model the daemon seeds into a fresh config", () => {
      expect(DEFAULT_MODEL).toBe("claude-opus-5");
      expect(MODEL_OPTIONS.some((o) => o.value === DEFAULT_MODEL)).toBe(true);
    });

    it("strips the claude- prefix for the compact display", () => {
      expect(stripModel("claude-opus-4-8")).toBe("opus-4-8");
      expect(stripModel("claude-sonnet-4-6")).toBe("sonnet-4-6");
      // A value without the prefix is passed through unchanged.
      expect(stripModel("opus-4-8")).toBe("opus-4-8");
      // Only a LEADING prefix is stripped.
      expect(stripModel("my-claude-model")).toBe("my-claude-model");
    });
  });

  describe("buildSoundCheck", () => {
    const tool = (over: Partial<ToolResult>): ToolResult => ({
      name: "claude",
      path: "/opt/homebrew/bin/claude",
      found: true,
      healthy: true,
      version: "2.1.4",
      detail: "",
      ...over,
    });

    it("derives Linear + the required CLIs + workspace, in order", () => {
      const rows = buildSoundCheck(
        [
          tool({ name: "claude", version: "2.1.4", path: "/opt/homebrew/bin/claude" }),
          tool({ name: "git", version: "2.44.0", path: "/usr/bin/git" }),
          tool({ name: "gh", version: "2.62.0", path: "/opt/homebrew/bin/gh" }),
        ],
        { linearConnected: true },
      );
      expect(rows.map((r) => r.name)).toEqual(["Linear API", "claude", "git", "gh", "workspace"]);
      expect(rows.every((r) => r.ok)).toBe(true);
      // Healthy CLI detail is "version · path".
      expect(rows[1].detail).toBe("2.1.4 · /opt/homebrew/bin/claude");
      // The workspace row shows the seeded runtime home default.
      expect(rows[4].detail).toBe(WORKSPACE_ROOT_DEFAULT);
    });

    it("uses the connected account name for the Linear row when known, else Authenticated", () => {
      expect(buildSoundCheck([], { linearConnected: true, account: "David Johansen" })[0].detail).toBe(
        "David Johansen",
      );
      expect(buildSoundCheck([], { linearConnected: true })[0].detail).toBe("Authenticated");
      const notConnected = buildSoundCheck([], { linearConnected: false })[0];
      expect(notConnected.detail).toBe("Not connected");
      expect(notConnected.ok).toBe(false);
    });

    it("flags a missing-from-PATH binary amber with the consequence-free 'Not found on PATH'", () => {
      const rows = buildSoundCheck([tool({ name: "gh", found: false, healthy: false, version: "", path: "" })], {
        linearConnected: true,
      });
      const gh = rows.find((r) => r.name === "gh")!;
      expect(gh.ok).toBe(false);
      expect(gh.detail).toBe("Not found on PATH");
    });

    it("marks a CLI absent from the probe result (e.g. no bridge → []) as not detected", () => {
      const rows = buildSoundCheck([], { linearConnected: true });
      const claude = rows.find((r) => r.name === "claude")!;
      expect(claude.ok).toBe(false);
      expect(claude.detail).toBe("Not detected");
    });
  });

  describe("normalizeProjectSlug", () => {
    // The slugId is the FULL trailing URL segment (e.g. "my-project-9c29e9ade060"), not a bare hex
    // id — it must equal exactly what ListProjects returns / the picker writes. See GETTING_STARTED.md.
    it("passes a bare slug through unchanged (full hyphenated form)", () => {
      expect(normalizeProjectSlug("my-project-9c29e9ade060")).toEqual({
        ok: true,
        slug: "my-project-9c29e9ade060",
      });
      expect(normalizeProjectSlug("  my-project-9c29e9ade060  ")).toEqual({
        ok: true,
        slug: "my-project-9c29e9ade060",
      });
    });

    it("accepts a bare non-hex slug (Linear slugIds are often plain words)", () => {
      // e.g. the daemon's own fixtures: slugId "example-infra" / "core-proj".
      expect(normalizeProjectSlug("example-infra")).toEqual({ ok: true, slug: "example-infra" });
      expect(normalizeProjectSlug("872639248532")).toEqual({ ok: true, slug: "872639248532" });
    });

    it("extracts the full slug from a Linear project URL", () => {
      expect(
        normalizeProjectSlug("https://linear.app/acme/project/my-project-9c29e9ade060"),
      ).toEqual({ ok: true, slug: "my-project-9c29e9ade060" });
    });

    it("extracts the full slug from a URL with a trailing view segment + query/hash", () => {
      expect(
        normalizeProjectSlug(
          "https://linear.app/acme/project/symphony-app-872639248532/overview?tab=1#x",
        ),
      ).toEqual({ ok: true, slug: "symphony-app-872639248532" });
    });

    it("lowercases an uppercased paste to the canonical slug", () => {
      expect(normalizeProjectSlug("My-Project-9C29E9ADE060")).toEqual({
        ok: true,
        slug: "my-project-9c29e9ade060",
      });
    });

    it("returns an error for empty input or a URL with no project segment", () => {
      expect(normalizeProjectSlug("")).toMatchObject({ ok: false });
      expect(normalizeProjectSlug("   ")).toMatchObject({ ok: false });
      expect(normalizeProjectSlug("https://linear.app/acme/team/FOO")).toMatchObject({
        ok: false,
      });
      expect(normalizeProjectSlug("https://linear.app/acme/project/")).toMatchObject({
        ok: false,
      });
    });
  });
});
