import { describe, expect, it } from "vitest";

import { mergeStateNote } from "@/lib/console-merge";

describe("mergeStateNote — what an armed auto-merge is waiting on (STUDIO-784)", () => {
  // The two states the whole feature turns on: BEHIND is the one the daemon refuses outright, and
  // BLOCKED is the ordinary one an armed merge sits in while its required checks run. They must
  // read differently, because one needs a human and the other needs only patience.
  it("separates a merge waiting on checks from one waiting on a human", () => {
    expect(mergeStateNote("BLOCKED")).toMatch(/required checks/i);
    expect(mergeStateNote("BEHIND")).toMatch(/behind its base/i);
    expect(mergeStateNote("DIRTY")).toMatch(/conflicts/i);
    expect(mergeStateNote("BEHIND")).not.toEqual(mergeStateNote("BLOCKED"));
  });

  // GitHub computes mergeability lazily, so "no answer yet" is a real answer. Saying nothing is
  // right; inventing "ready" or "blocked" from an absent field would be a guess shown as a fact.
  it("says nothing when GitHub has not judged the pull request", () => {
    for (const state of ["", "   ", "UNKNOWN", "unknown"]) {
      expect(mergeStateNote(state)).toBe("");
    }
  });

  it("reads GitHub's own value however it is cased", () => {
    expect(mergeStateNote("clean")).toBe(mergeStateNote("CLEAN"));
    expect(mergeStateNote(" behind ")).toBe(mergeStateNote("BEHIND"));
  });

  // GitHub's vocabulary is its own and has grown before. A state this console has never heard of
  // is exactly the one worth showing, so it is passed through rather than swallowed into "".
  it("passes an unrecognised state through rather than hiding it", () => {
    expect(mergeStateNote("SOMETHING_NEW")).toContain("SOMETHING_NEW");
  });

  // `--auto` merges a CLEAN pull request outright, so after the click there is nothing left for it
  // to be waiting on. "GitHub reports it ready to merge" printed beside "queued … for merge" reads
  // as though it had NOT landed, which is the opposite of the truth.
  it("says nothing about a CLEAN pull request once the merge is armed", () => {
    expect(mergeStateNote("CLEAN")).toMatch(/ready to merge/i);
    expect(mergeStateNote("CLEAN", true)).toBe("");
  });

  // The states that still hold a merge up are exactly the ones the armed receipt exists to show,
  // so arming must not silence them too.
  it("still says what an armed merge is stuck on", () => {
    for (const state of ["BLOCKED", "BEHIND", "DIRTY", "SOMETHING_NEW"]) {
      expect(mergeStateNote(state, true)).toBe(mergeStateNote(state));
      expect(mergeStateNote(state, true)).not.toBe("");
    }
  });
});
