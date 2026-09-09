import { describe, expect, it } from "vitest";

import { mergeStateNote } from "@/lib/console-merge";

describe("mergeStateNote — what an armed auto-merge is waiting on (STUDIO-784)", () => {
  // The two states the whole feature turns on: DIRTY is the one that needs a human, and BLOCKED is
  // the ordinary one an armed merge sits in while its required checks run. They must read
  // differently, because one needs a human and the other needs only patience.
  it("separates a merge waiting on checks from one waiting on a human", () => {
    expect(mergeStateNote("BLOCKED")).toMatch(/required checks/i);
    expect(mergeStateNote("BEHIND")).toMatch(/behind its base/i);
    expect(mergeStateNote("DIRTY")).toMatch(/conflicts/i);
    expect(mergeStateNote("BEHIND")).not.toEqual(mergeStateNote("BLOCKED"));
  });

  // A BEHIND receipt cannot mean "push the branch", and saying so was a real falsehood in the
  // header: `runmerge::resolve_pull_request` REFUSES a behind branch whose repository will not
  // update it (STUDIO-784 gap 1), so every BEHIND that survives to a receipt is one GitHub brings
  // up to date itself. The note reports the wait, not a chore the operator does not have.
  it("does not send the operator to push a branch GitHub updates itself", () => {
    expect(mergeStateNote("BEHIND")).toMatch(/github will bring it up to date/i);
    expect(mergeStateNote("BEHIND", true)).toBe(mergeStateNote("BEHIND"));
    for (const armed of [false, true]) {
      expect(mergeStateNote("BEHIND", armed)).not.toMatch(/cannot land|someone pushes/i);
    }
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
    for (const state of ["BLOCKED", "DIRTY", "SOMETHING_NEW"]) {
      expect(mergeStateNote(state, true)).toBe(mergeStateNote(state));
      expect(mergeStateNote(state, true)).not.toBe("");
    }
  });
});
