import { describe, expect, it } from "vitest";
import type { ReviewJob } from "@/lib/api";
import {
  REVIEW_STATUSES,
  isLive,
  prLabel,
  retiredCount,
  reviewRow,
  reviewRows,
  reviewStats,
  shortSha,
} from "@/lib/reviews-model";

// The pure half of the console's Reviews surface (STUDIO-722, slice 8). Everything here is asserted
// against the daemon's REAL watch-set shape — the `REVIEW_STATUS_*` constants in
// crates/store/src/types.rs and the `load_live_review_watch` predicate in crates/store/src/lib.rs —
// because those are what the rendering rests on and a value added there is exactly the drift these
// tests exist to catch.

const HEAD_A = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const HEAD_B = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

function job(over: Partial<ReviewJob> = {}): ReviewJob {
  return {
    owner: "makewhatis",
    repo: "rhapsody",
    number: 12,
    reviewer: "bob",
    author: "alice",
    introduced_by: "handoff:STUDIO-720",
    requested_sha: HEAD_A,
    last_reviewed_sha: HEAD_A,
    status: "reviewed",
    open: true,
    ...over,
  };
}

describe("prLabel / shortSha", () => {
  it("spells a pull request the way the daemon does", () => {
    expect(prLabel(job())).toBe("makewhatis/rhapsody#12");
  });

  it("abbreviates a SHA to seven characters and leaves an absent one empty", () => {
    expect(shortSha(HEAD_A)).toBe("aaaaaaa");
    expect(shortSha("")).toBe("");
  });
});

describe("isLive", () => {
  // The daemon's own predicate: `open = 1 AND status != 'dropped'`. Both halves are load-bearing,
  // and the second is the one a `open`-only check would get wrong — a retirement is a SOFT delete.
  it("mirrors the daemon's live-watch predicate", () => {
    expect(isLive(job({ open: true, status: "reviewed" }))).toBe(true);
    expect(isLive(job({ open: true, status: "dropped" }))).toBe(false);
    expect(isLive(job({ open: false, status: "reviewed" }))).toBe(false);
    expect(isLive(job({ open: false, status: "dropped" }))).toBe(false);
  });
});

describe("reviewRow", () => {
  it("keys a row per (PR, reviewer), which is the watch set's own granularity", () => {
    expect(reviewRow(job({ reviewer: "bob" })).key).toBe("makewhatis/rhapsody#12@bob");
    expect(reviewRow(job({ reviewer: "carol" })).key).toBe("makewhatis/rhapsody#12@carol");
  });

  it("links to the pull request on GitHub", () => {
    expect(reviewRow(job()).url).toBe("https://github.com/makewhatis/rhapsody/pull/12");
  });

  it("gives every status the daemon can store a pill and a label", () => {
    for (const status of REVIEW_STATUSES) {
      const row = reviewRow(job({ status }));
      expect(row.label, status).not.toBe("");
      expect(row.label, status).not.toBe(status);
    }
  });

  // A round that ran out of turns read the head only PARTLY, which is why the daemon deliberately
  // does not advance `last_reviewed_sha` for it. Dressing it as a finished review is how a review
  // that never happened ships as if it had.
  it("does not present a truncated round as a finished one", () => {
    const truncated = reviewRow(job({ status: "truncated", last_reviewed_sha: "" }));
    const reviewed = reviewRow(job({ status: "reviewed" }));
    expect(truncated.variant).not.toBe(reviewed.variant);
    expect(truncated.reviewedShort).toBe("");
  });

  it("shows a status this build has never heard of rather than dropping the row", () => {
    const row = reviewRow(job({ status: "quarantined" }));
    expect(row.label).toBe("quarantined");
    expect(row.live).toBe(true);
  });

  it("falls back to a label when the daemon sends an empty status", () => {
    expect(reviewRow(job({ status: "" })).label).toBe("unknown");
  });
});

describe("reviewRows", () => {
  const jobs = [
    job({ reviewer: "bob", status: "dropped", open: false }),
    job({ reviewer: "carol", status: "in_flight", last_reviewed_sha: "" }),
    job({ number: 13, reviewer: "dave", status: "requested" }),
  ];

  it("hides retired rows by default", () => {
    expect(reviewRows(jobs, "active").map((r) => r.job.reviewer)).toEqual(["carol", "dave"]);
  });

  it("reveals them on demand, always below the live ones", () => {
    expect(reviewRows(jobs, "all").map((r) => r.job.reviewer)).toEqual(["carol", "dave", "bob"]);
  });

  // Stability matters: the daemon orders by (owner, repo, number, reviewer), so two reviewers of
  // one pull request arrive adjacent and must stay that way.
  it("keeps the daemon's order within the live half", () => {
    const two = [
      job({ reviewer: "bob", status: "requested" }),
      job({ reviewer: "carol", status: "in_flight" }),
      job({ number: 13, reviewer: "bob", status: "requested" }),
    ];
    expect(reviewRows(two, "active").map((r) => `${r.pr}@${r.job.reviewer}`)).toEqual([
      "makewhatis/rhapsody#12@bob",
      "makewhatis/rhapsody#12@carol",
      "makewhatis/rhapsody#13@bob",
    ]);
  });

  it("is empty for an empty watch set", () => {
    expect(reviewRows([], "all")).toEqual([]);
  });
});

describe("retiredCount", () => {
  it("counts what the reveal would add, so a set with none can retire the toggle", () => {
    expect(retiredCount([job(), job({ reviewer: "carol" })])).toBe(0);
    expect(retiredCount([job(), job({ reviewer: "carol", status: "dropped", open: false })])).toBe(
      1,
    );
  });
});

describe("reviewStats", () => {
  it("counts distinct pull requests, not rows", () => {
    const stats = reviewStats([
      job({ reviewer: "bob", status: "in_flight" }),
      job({ reviewer: "carol", status: "in_flight" }),
      job({ number: 13, reviewer: "bob", status: "requested" }),
    ]);
    expect(stats.pullRequests).toBe(2);
    expect(stats.inFlight).toBe(2);
    expect(stats.awaiting).toBe(1);
  });

  it("counts a truncated round as still awaiting one — because it is", () => {
    expect(reviewStats([job({ status: "truncated", last_reviewed_sha: "" })]).awaiting).toBe(1);
  });

  it("ignores retired rows entirely", () => {
    const stats = reviewStats([
      job({ status: "dropped", open: false }),
      job({ number: 13, reviewer: "carol", status: "reviewed", last_reviewed_sha: HEAD_B }),
    ]);
    expect(stats).toEqual({ pullRequests: 1, inFlight: 0, awaiting: 0 });
  });
});
