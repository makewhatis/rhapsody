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
// the run itself said so, for a solo or unmatched dispatch; and a run with no row is not an
// answer, which is what lets the caller fall back to the live roster instead of publishing a "—"
// it has no grounds for.
//
// Teams-OFF belongs to the third case, not the second: `route_teams` returns `None` for
// `RouteReason::Off` and writes no event at all (`crates/orchestrator/src/teams.rs`), precisely so
// that turning Teams off carries no behavioural delta. A Teams-off run has NO record, and falls
// through to the live roster like any other unrecorded one.

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
 * The fold mirrors `run_identity` (`crates/orchestrator/src/lifecycle.rs`) rather than inventing a
 * second reading of the same rows, because the two are answering one question and a reader who
 * checks the Rust should not find them disagreeing. That means, exactly as the daemon's two probes
 * do:
 *
 * - A `teams.route` row wins UNCONDITIONALLY over a `teams.unrouted` one. The daemon probes route
 *   first and returns it; its unrouted probe only runs for a run that recorded no route. Letting
 *   the kinds compete on `seq` instead would resolve a run carrying both by whichever came first,
 *   and could answer a DEFINITE nobody — which stops the caller, refusing even the live-roster
 *   fallback — for a run the daemon reads as routed.
 * - Within a kind the HIGHEST `seq` wins. Each probe is `LIMIT 1` against a search ordered
 *   `ORDER BY e.run_id DESC, e.seq DESC` (`crates/store/src/sqlite.rs`), so the newest row is the
 *   one the daemon reads. No run in the real store carries two routing rows today; this is what
 *   the mirror says should happen if one ever does.
 * - A route row carrying no parseable identity is no route at all and falls through to the
 *   unrouted probe — and only to the unrouted probe. The daemon reads ONE route row and does not
 *   go looking for an older one that parses, so neither does this.
 */
export function runIdentities(hits: readonly EventHit[]): ReadonlyMap<number, string> {
  // Each kind folded separately, to the row its own probe would have read.
  const routes = new Map<number, EventHit>();
  const unrouted = new Set<number>();
  for (const hit of hits) {
    if (hit.kind === ROUTE_EVENT_KIND) {
      const seen = routes.get(hit.run_id);
      if (seen === undefined || hit.seq > seen.seq) routes.set(hit.run_id, hit);
    } else if (hit.kind === UNROUTED_EVENT_KIND) {
      // No seq needed on this side: the daemon's unrouted probe only asks WHETHER a row exists
      // (`Ok(Some(_))`), and every such row answers the same "nobody".
      unrouted.add(hit.run_id);
    }
  }
  const byRun = new Map<number, string>();
  for (const [runId, hit] of routes) {
    const name = routeEventIdentity(hit.text);
    if (name !== "") byRun.set(runId, name);
  }
  // Only the runs the route probe did not answer for, which is the daemon's own ordering.
  for (const runId of unrouted) {
    if (!byRun.has(runId)) byRun.set(runId, "");
  }
  return byRun;
}
