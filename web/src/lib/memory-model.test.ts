import { describe, expect, it } from "vitest";
import type { TeamsFact } from "@/lib/api";
import {
  STATE_INVALIDATED,
  STATE_VALID,
  bankFacts,
  bankStats,
  factKey,
  factMatches,
  filterFacts,
  isInvalidated,
  sortFacts,
  teammateOptions,
  ticketOptions,
  withState,
  type MemoryBank,
} from "./memory-model";

// STUDIO-681 §6 / §10 sub-ticket 4 — the Memory page's logic, tested without a DOM.
// The view's own boxes (4.4, 4.5, 4.6) live in `MemoryView.test.tsx`; what is here is the
// derivation those boxes render: the stats (4.1), the search (4.2) and the filters + sort (4.3).

function fact(over: Partial<TeamsFact> & Pick<TeamsFact, "id" | "identity">): TeamsFact {
  return {
    document_id: `run-${over.run_id ?? "1"}`,
    ticket: "STUDIO-654",
    commit_sha: "",
    pr: "",
    run_id: "1",
    at: "2026-08-31T19:11:00Z",
    state: STATE_VALID,
    reason: "",
    content: "a fact",
    ...over,
  };
}

const ALICE: MemoryBank = {
  identity: "alice",
  facts: [
    fact({ id: "a1", identity: "alice", run_id: "547", commit_sha: "44d8675cafe", at: "2026-08-31T19:11:00Z" }),
    fact({ id: "a2", identity: "alice", run_id: "545", ticket: "STUDIO-654", at: "2026-08-31T16:54:00Z" }),
  ],
};

const JIMMY: MemoryBank = {
  identity: "jimmy",
  facts: [
    fact({ id: "j1", identity: "jimmy", ticket: "STUDIO-676", run_id: "546", at: "2026-08-31T21:15:00Z" }),
    fact({
      id: "j2",
      identity: "jimmy",
      ticket: "STUDIO-673",
      run_id: "549",
      at: "2026-08-31T13:46:00Z",
      state: STATE_INVALIDATED,
      reason: "fixed in rc.9; default is now 60000.",
    }),
  ],
};

const BANKS = [ALICE, JIMMY];

const ALL_FILTER = { search: "", who: "all", ticket: "all", state: "all" } as const;

describe("bankStats — box 4.1", () => {
  it("counts facts, valid, invalidated and banks from the bank data", () => {
    expect(bankStats(BANKS)).toEqual({ facts: 4, valid: 3, invalidated: 1, banks: 2 });
  });

  it("counts a bank that answered with nothing — an empty bank is still a bank", () => {
    expect(bankStats([{ identity: "sam", facts: [] }])).toEqual({
      facts: 0,
      valid: 0,
      invalidated: 0,
      banks: 1,
    });
  });

  it("reads a record with no state as valid, the way recall does", () => {
    const stats = bankStats([{ identity: "sam", facts: [fact({ id: "s1", identity: "sam", state: "" })] }]);
    expect(stats).toEqual({ facts: 1, valid: 1, invalidated: 0, banks: 1 });
  });
});

describe("factMatches — box 4.2", () => {
  const f = fact({
    id: "a1",
    identity: "alice",
    ticket: "STUDIO-654",
    commit_sha: "44d8675cafebabe",
    content: "The config.go rebase hazard matters more than the fix.",
  });

  it("matches the record body", () => {
    expect(factMatches(f, "rebase hazard")).toBe(true);
  });

  it("matches the ticket key, case-insensitively", () => {
    expect(factMatches(f, "studio-654")).toBe(true);
  });

  it("matches a SHA by prefix, because the card only shows the short form", () => {
    expect(factMatches(f, "44d8675")).toBe(true);
  });

  it("does not match unrelated text", () => {
    expect(factMatches(f, "expo prebuild")).toBe(false);
  });

  it("treats an empty or blank query as matching everything", () => {
    expect(factMatches(f, "")).toBe(true);
    expect(factMatches(f, "   ")).toBe(true);
  });
});

describe("filterFacts — boxes 4.2 and 4.3", () => {
  const facts = bankFacts(BANKS);

  it("search narrows the list to the matching facts", () => {
    const hit = filterFacts(facts, { ...ALL_FILTER, search: "STUDIO-676" });
    expect(hit.map((f) => f.id)).toEqual(["j1"]);
  });

  it("the teammate filter scopes the list to one bank", () => {
    expect(filterFacts(facts, { ...ALL_FILTER, who: "alice" }).map((f) => f.id)).toEqual(["a1", "a2"]);
  });

  it("the ticket filter scopes the list to one ticket", () => {
    expect(filterFacts(facts, { ...ALL_FILTER, ticket: "STUDIO-654" }).map((f) => f.id)).toEqual([
      "a1",
      "a2",
    ]);
  });

  it("the state filter separates valid from invalidated", () => {
    expect(filterFacts(facts, { ...ALL_FILTER, state: "valid" }).map((f) => f.id)).toEqual([
      "a1",
      "a2",
      "j1",
    ]);
    expect(filterFacts(facts, { ...ALL_FILTER, state: "invalidated" }).map((f) => f.id)).toEqual(["j2"]);
  });

  it("combines filters — each one narrows what the previous left", () => {
    const out = filterFacts(facts, { ...ALL_FILTER, who: "jimmy", state: "valid" });
    expect(out.map((f) => f.id)).toEqual(["j1"]);
  });

  it("`all` on every axis narrows nothing", () => {
    expect(filterFacts(facts, ALL_FILTER)).toHaveLength(4);
  });
});

describe("sortFacts — box 4.3", () => {
  const facts = bankFacts(BANKS);

  it("newest puts the most recently stamped fact first", () => {
    expect(sortFacts(facts, "newest").map((f) => f.id)).toEqual(["j1", "a1", "a2", "j2"]);
  });

  it("oldest is the exact reverse ordering", () => {
    expect(sortFacts(facts, "oldest").map((f) => f.id)).toEqual(["j2", "a2", "a1", "j1"]);
  });

  it("does not mutate the input", () => {
    const input = [...facts];
    sortFacts(input, "oldest");
    expect(input.map((f) => f.id)).toEqual(facts.map((f) => f.id));
  });

  it("sorts a record the host could not stamp as the oldest, rather than into an arbitrary slot", () => {
    const undated = fact({ id: "z1", identity: "alice", at: "" });
    expect(sortFacts([...facts, undated], "newest").at(-1)?.id).toBe("z1");
  });

  it("breaks a tie on id, so the same bank renders in the same order twice", () => {
    const same = "2026-08-31T12:00:00Z";
    const tied = [
      fact({ id: "b", identity: "alice", at: same }),
      fact({ id: "a", identity: "alice", at: same }),
    ];
    expect(sortFacts(tied, "newest").map((f) => f.id)).toEqual(["a", "b"]);
    expect(sortFacts(tied, "oldest").map((f) => f.id)).toEqual(["a", "b"]);
  });
});

describe("filter options", () => {
  it("lists teammates in roster order, not alphabetically", () => {
    expect(teammateOptions([JIMMY, ALICE])).toEqual(["jimmy", "alice"]);
  });

  it("lists each ticket once, sorted, ignoring records stamped with none", () => {
    const facts = [...bankFacts(BANKS), fact({ id: "x", identity: "alice", ticket: "" })];
    expect(ticketOptions(facts)).toEqual(["STUDIO-654", "STUDIO-673", "STUDIO-676"]);
  });
});

describe("withState — the transitions boxes 4.5 and 4.6 render", () => {
  const f = fact({ id: "a1", identity: "alice" });

  it("invalidating carries the reason", () => {
    const out = withState(f, STATE_INVALIDATED, "the default changed in rc.9");
    expect(out.state).toBe(STATE_INVALIDATED);
    expect(out.reason).toBe("the default changed in rc.9");
    expect(isInvalidated(out)).toBe(true);
  });

  it("reinstating clears the reason, because the record is true again", () => {
    const dead = withState(f, STATE_INVALIDATED, "wrong");
    const back = withState(dead, STATE_VALID, "");
    expect(back.state).toBe(STATE_VALID);
    expect(back.reason).toBe("");
    expect(isInvalidated(back)).toBe(false);
  });

  it("leaves the record body and its provenance untouched — nothing is deleted", () => {
    const out = withState(f, STATE_INVALIDATED, "wrong");
    expect(out.content).toBe(f.content);
    expect(out.run_id).toBe(f.run_id);
    expect(out.commit_sha).toBe(f.commit_sha);
    expect(out.at).toBe(f.at);
  });

  it("does not mutate the fact it was given", () => {
    withState(f, STATE_INVALIDATED, "wrong");
    expect(f.state).toBe(STATE_VALID);
    expect(f.reason).toBe("");
  });
});

describe("factKey", () => {
  it("qualifies the record id with its bank, because ids are only unique within one", () => {
    const a = fact({ id: "notes", identity: "alice" });
    const j = fact({ id: "notes", identity: "jimmy" });
    expect(factKey(a)).not.toBe(factKey(j));
  });
});
