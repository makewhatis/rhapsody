import { describe, expect, it } from "vitest";

import { mergeStateNote, ungatedMergeStateNote } from "@/lib/console-merge";

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

describe("ungatedMergeStateNote — a mergeStateStatus no daemon gate has filtered (STUDIO-749)", () => {
  // THE ONE ARM THAT CANNOT SURVIVE LOSING THE GATE. `mergeStateNote`'s BEHIND reading promises
  // GitHub will update the branch itself, and that is only true because
  // `runmerge::resolve_pull_request` has already refused every BEHIND branch on a repository that
  // will not (STUDIO-784 gap 1). A raw value has passed no such refusal, so on this repository —
  // `main` requires up-to-date branches and does not update them — the promise is of something
  // that never happens, while Merge on the same run says "push or update the branch".
  it("does not promise GitHub will update a branch nothing has gated", () => {
    expect(ungatedMergeStateNote("BEHIND")).toMatch(/behind its base/i);
    expect(ungatedMergeStateNote("BEHIND")).not.toMatch(/bring it up to date|github will/i);
    expect(ungatedMergeStateNote("BEHIND")).not.toBe(mergeStateNote("BEHIND"));
  });

  // It says nothing the gate is needed for, and nothing MORE either: it must not become a second
  // vocabulary that drifts from the first. BEHIND is the whole of the difference.
  it("reads every other state exactly as the gated note does", () => {
    for (const state of [
      "",
      "UNKNOWN",
      "CLEAN",
      "BLOCKED",
      "UNSTABLE",
      "DIRTY",
      "DRAFT",
      "HAS_HOOKS",
      "SOMETHING_NEW",
      undefined,
    ]) {
      expect(ungatedMergeStateNote(state)).toBe(mergeStateNote(state));
    }
  });

  // Same normalisation and the same refusal to guess — it is the same switch, not a copy of it.
  it("keeps the shared reading of casing, whitespace and an absent value", () => {
    expect(ungatedMergeStateNote(" behind ")).toBe(ungatedMergeStateNote("BEHIND"));
    expect(ungatedMergeStateNote(undefined)).toBe("");
    expect(ungatedMergeStateNote("SOMETHING_NEW")).toContain("SOMETHING_NEW");
  });
});
