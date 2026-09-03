import { ROUTE_EVENT_KIND, UNROUTED_EVENT_KIND, type EventHit } from "@/lib/api";

// run-identity — who a RUN was dispatched as, from the durable record the daemon already keeps
// (design record `~/.rhapsody/docs/console-run-detail-design.md` §3/§6; slice 5 of its §9 plan).
//
// The design record predicted this would arrive as an `identity` field on `RunSummary` (§5, "the
// one field it lacks"). What STUDIO-735 actually shipped is narrower: `assignee` decorates
// `GET /api/v1/history/issues`, which answers one row per ISSUE — that issue's LATEST run — so
// it cannot attribute an older attempt, and the run detail does not read that endpoint at all.
//
// What it CAN read is the record STUDIO-735 itself resolves from: the `teams.route` events row
// every routed dispatch writes (`crates/orchestrator/src/teams.rs`), which is per-RUN, durable, and
// already served by the existing cross-run event search — one row per run, so one bounded request
// covers every attempt in the selector. No daemon change; no invented field.
//
// The daemon's reading is a TRI-STATE (`crates/orchestrator/src/lifecycle.rs`) and so is this one:
// a `teams.route` row NAMES a teammate; a `teams.unrouted` row answers "nobody" definitively —
// the run itself said so, for a solo, unmatched or Teams-off dispatch; and a run with no row is
// not an answer, which is what lets the caller fall back to the live roster instead of publishing
// a "—" it has no grounds for.

/**
 * The identity out of a `teams.route` event's text, or "" when it carries none.
 *
 * Mirrors `route_event_identity` in `crates/orchestrator/src/triage.rs` field for field, including
 * the reason it reads the FIRST whitespace-separated field only rather than searching the text: the
 * `reason` behind it is free prose that can and does quote model output, and a reason containing
 * `identity=` must never be able to name who a run was.
 */
export function routeEventIdentity(text: string): string {
  const first = text.trim().split(/\s+/, 1)[0] ?? "";
  return first.startsWith("identity=") ? first.slice("identity=".length) : "";
}

/**
 * Run id → the teammate that run was dispatched as, over one ticket's routing rows.
 *
 * A key is PRESENT with an empty value when the run recorded that it routed to nobody, and ABSENT
 * when the ticket has no routing row for it at all — a legacy run, a pruned ledger, or a store
 * written before Teams. Those are different answers and the caller acts on them differently, so
 * they are kept apart here rather than both collapsing to "".
 *
 * A route row carrying no parseable identity is dropped rather than recorded as a nobody, which is
 * how the daemon reads it too: `lifecycle.rs` falls through such a row to the unrouted probe.
 * Rows arrive newest-run-first (`ORDER BY e.run_id DESC, e.seq DESC`), so the LOWEST seq wins —
 * a run has exactly one routing row today, and the earliest is the dispatch decision if that ever
 * stops being true.
 */
export function runIdentities(hits: readonly EventHit[]): ReadonlyMap<number, string> {
  const seqs = new Map<number, number>();
  const byRun = new Map<number, string>();
  const take = (hit: EventHit, name: string) => {
    const seen = seqs.get(hit.run_id);
    if (seen !== undefined && seen <= hit.seq) return;
    seqs.set(hit.run_id, hit.seq);
    byRun.set(hit.run_id, name);
  };
  for (const hit of hits) {
    if (hit.kind === ROUTE_EVENT_KIND) {
      const name = routeEventIdentity(hit.text);
      if (name !== "") take(hit, name);
    } else if (hit.kind === UNROUTED_EVENT_KIND) {
      take(hit, "");
    }
  }
  return byRun;
}
