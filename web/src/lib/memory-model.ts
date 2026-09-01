import type { TeamsFact } from "@/lib/api";

// The Memory page's derivation — STUDIO-681 §6, built by STUDIO-685.
//
// Everything here is pure: banks in, counts and orderings out. The view renders it; the daemon
// routes it reads are `GET /api/v1/teams` (roster + backend) and `GET /api/v1/teams/recall`
// (one identity's bank, browsed), both of which already exist — §11 forbids inventing a surface,
// and `MemoryView` documents the two reads §6 wants that the daemon does not yet serve.

/** A record recall will return. Mirrors `rhapsody-config`'s `STATE_VALID`. */
export const STATE_VALID = "valid";
/** A record §5.3 marked wrong, with its reason. Mirrors `rhapsody-config`'s `STATE_INVALIDATED`. */
export const STATE_INVALIDATED = "invalidated";

/** The wildcard every filter Select and Seg uses for "do not narrow on this axis". */
export const ANY = "all";

/** One teammate's bank as the page read it, in roster order. */
export interface MemoryBank {
  identity: string;
  facts: readonly TeamsFact[];
  /** Record files the daemon could not parse — reported rather than hidden, as recall reports them. */
  skipped?: readonly string[];
}

/** The four counts the header strip shows (§6, box 4.1). */
export interface MemoryStats {
  facts: number;
  valid: number;
  invalidated: number;
  banks: number;
}

export type MemoryStateFilter = typeof ANY | typeof STATE_VALID | typeof STATE_INVALIDATED;
export type MemorySort = "newest" | "oldest";

/** The filter bar's whole state (§6): free text plus three narrowing axes. */
export interface MemoryFilter {
  search: string;
  /** A teammate name, or [`ANY`]. */
  who: string;
  /** A ticket key, or [`ANY`]. */
  ticket: string;
  state: MemoryStateFilter;
}

/**
 * Whether a record has been marked wrong.
 *
 * Only the explicit marker counts. A record carrying no `state` at all reads as VALID here, which
 * is the correct reading of the wire: recall serves valid records only, so anything it returned
 * without a state is one — and treating an unknown state as "invalidated" would dim a fact the
 * daemon still recalls into every turn-1 prompt.
 */
export function isInvalidated(fact: TeamsFact): boolean {
  return fact.state === STATE_INVALIDATED;
}

/** Every bank's records, flattened in roster order. */
export function bankFacts(banks: readonly MemoryBank[]): TeamsFact[] {
  return banks.flatMap((b) => [...b.facts]);
}

/** The header counts (box 4.1). `banks` counts what ANSWERED, so an empty bank still counts. */
export function bankStats(banks: readonly MemoryBank[]): MemoryStats {
  const facts = bankFacts(banks);
  const invalidated = facts.filter(isInvalidated).length;
  return {
    facts: facts.length,
    valid: facts.length - invalidated,
    invalidated,
    banks: banks.length,
  };
}

/**
 * Does `search` match this record (box 4.2 — "text, ticket, SHA")?
 *
 * Substring, not prefix, and case-folded: the card shows a SHA abbreviated to seven characters
 * while the bank stamps the full one, so an operator copying what they can see must still find the
 * record. A blank query matches everything rather than nothing — an empty filter bar is not a
 * filter.
 */
export function factMatches(fact: TeamsFact, search: string): boolean {
  const q = search.trim().toLowerCase();
  if (q === "") return true;
  return (
    fact.content.toLowerCase().includes(q) ||
    fact.ticket.toLowerCase().includes(q) ||
    fact.commit_sha.toLowerCase().includes(q)
  );
}

/** Apply the whole filter bar. Each axis narrows what the previous one left (box 4.3). */
export function filterFacts(facts: readonly TeamsFact[], filter: MemoryFilter): TeamsFact[] {
  return facts.filter((f) => {
    if (filter.who !== ANY && f.identity !== filter.who) return false;
    if (filter.ticket !== ANY && f.ticket !== filter.ticket) return false;
    if (filter.state === STATE_VALID && isInvalidated(f)) return false;
    if (filter.state === STATE_INVALIDATED && !isInvalidated(f)) return false;
    return factMatches(f, filter.search);
  });
}

/**
 * When a record was stamped, as a sortable number.
 *
 * A record the host could not stamp sorts as the OLDEST thing in the bank in both directions,
 * rather than landing in an arbitrary slot — the same rule the Teams console's memory preview
 * uses, so the two surfaces agree about what "recent" means.
 */
function stampedAt(fact: TeamsFact): number {
  const ms = Date.parse(fact.at);
  return Number.isNaN(ms) ? -Infinity : ms;
}

/** Order the list (box 4.3). Returns a new array; ties break on id, for a total order. */
export function sortFacts(facts: readonly TeamsFact[], sort: MemorySort): TeamsFact[] {
  const dir = sort === "oldest" ? -1 : 1;
  return [...facts].sort((a, b) => {
    const at = stampedAt(a);
    const bt = stampedAt(b);
    // Compared before subtracting, so two undated records (both -Infinity) fall through to the
    // id rather than producing the `NaN` an `Infinity - Infinity` would — a comparator that
    // returns NaN orders the array arbitrarily.
    if (at !== bt) return (bt - at) * dir;
    return a.id.localeCompare(b.id);
  });
}

/** The teammate Select's options — roster order, because §1.5 assigns color by that position. */
export function teammateOptions(banks: readonly MemoryBank[]): string[] {
  return banks.map((b) => b.identity);
}

/** The ticket Select's options — each ticket once, sorted, skipping records stamped with none. */
export function ticketOptions(facts: readonly TeamsFact[]): string[] {
  const seen = new Set<string>();
  for (const f of facts) {
    if (f.ticket !== "") seen.add(f.ticket);
  }
  return [...seen].sort();
}

/**
 * One record's identity across banks. A record's `id` is its filename stem inside ONE bank
 * directory, so two teammates can hold `notes` — the bank has to be part of the key or the page
 * would dim the wrong card.
 */
export function factKey(fact: TeamsFact): string {
  return `${fact.identity}/${fact.id}`;
}

/**
 * The state transition boxes 4.5 and 4.6 render, as a value.
 *
 * Nothing is deleted and no provenance is touched: the record keeps its body, run, SHA and
 * timestamp, and only `state` and `reason` move. That is what makes the correction reversible
 * (§5.3) — reinstating is this same function pointed the other way.
 */
export function withState(fact: TeamsFact, state: string, reason: string): TeamsFact {
  return { ...fact, state, reason };
}
