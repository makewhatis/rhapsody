import { describe, expect, it } from "vitest";
import { onboardingStep, slugValid } from "./wizard";

describe("onboardingStep", () => {
  it("asks for the token first", () => expect(onboardingStep(false)).toBe("token"));
  it("then the project once a token exists", () => expect(onboardingStep(true)).toBe("project"));
});

describe("slugValid", () => {
  it("rejects empty/whitespace", () => {
    expect(slugValid("")).toBe(false);
    expect(slugValid("   ")).toBe(false);
  });
  it("accepts a non-empty slug", () => expect(slugValid("symphony")).toBe(true));
});
