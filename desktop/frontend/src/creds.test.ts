import { describe, expect, it } from "vitest";
import { credentialSummary, tokenLooksValid } from "./creds";

describe("tokenLooksValid", () => {
  it("rejects empty / whitespace", () => {
    expect(tokenLooksValid("")).toBe(false);
    expect(tokenLooksValid("   ")).toBe(false);
  });
  it("accepts a lin_ prefixed key", () => {
    expect(tokenLooksValid("lin_api_abc123")).toBe(true);
  });
  it("accepts a sufficiently long opaque token", () => {
    expect(tokenLooksValid("x".repeat(40))).toBe(true);
  });
  it("rejects a short non-prefixed string", () => {
    expect(tokenLooksValid("nope")).toBe(false);
  });
});

describe("credentialSummary", () => {
  it("checking when no status", () => expect(credentialSummary(null)).toBe("Checking…"));
  it("none when no token", () =>
    expect(credentialSummary({ has_token: false, backend: "keychain", oauth_available: false })).toBe(
      "No Linear token set",
    ));
  it("names the backend when stored", () =>
    expect(credentialSummary({ has_token: true, backend: "keychain", oauth_available: false })).toBe(
      "Linear token stored (keychain)",
    ));
});
