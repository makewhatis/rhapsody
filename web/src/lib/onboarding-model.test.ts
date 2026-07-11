import { describe, expect, it } from "vitest";
import { normalizeProjectSlug, onboardingStep, slugValid, tokenLooksValid } from "@/lib/onboarding-model";

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
        normalizeProjectSlug("https://linear.app/trackai/project/my-project-9c29e9ade060"),
      ).toEqual({ ok: true, slug: "my-project-9c29e9ade060" });
    });

    it("extracts the full slug from a URL with a trailing view segment + query/hash", () => {
      expect(
        normalizeProjectSlug(
          "https://linear.app/trackai/project/symphony-app-872639248532/overview?tab=1#x",
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
      expect(normalizeProjectSlug("https://linear.app/trackai/team/FOO")).toMatchObject({
        ok: false,
      });
      expect(normalizeProjectSlug("https://linear.app/trackai/project/")).toMatchObject({
        ok: false,
      });
    });
  });
});
