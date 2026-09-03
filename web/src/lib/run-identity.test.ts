import { describe, expect, it } from "vitest";
import { ROUTE_EVENT_KIND, UNROUTED_EVENT_KIND, type EventHit } from "@/lib/api";
import { routeEventIdentity, runIdentities } from "@/lib/run-identity";

function hit(over: Partial<EventHit> & Pick<EventHit, "run_id">): EventHit {
  return {
    issue_identifier: "STUDIO-746",
    seq: 1,
    at: "2026-09-03T19:49:01Z",
    kind: ROUTE_EVENT_KIND,
    tool: "",
    text: "identity=alice reason=label",
    ...over,
  };
}

// The parser mirrors `route_event_identity` in `crates/orchestrator/src/triage.rs`, which reads the
// FIRST whitespace field only. The `reason` behind it is free prose that can quote model output, so
// a reason containing `identity=` must never be able to name who a run was.
describe("routeEventIdentity — the name out of a `teams.route` event's text", () => {
  it("reads the identity the daemon writes first", () => {
    expect(routeEventIdentity("identity=alice reason=label")).toBe("alice");
    expect(routeEventIdentity("identity=jimmy reason=label_overlap")).toBe("jimmy");
  });

  it("reads the FIRST field only, so a reason quoting `identity=` cannot name the run", () => {
    expect(routeEventIdentity("identity=alice reason=identity=mallory")).toBe("alice");
    expect(routeEventIdentity("reason=no_match identity=mallory")).toBe("");
  });

  it("names nobody for an empty name, an absent field or an empty text", () => {
    expect(routeEventIdentity("identity= reason=label")).toBe("");
    expect(routeEventIdentity("reason=solo")).toBe("");
    expect(routeEventIdentity("")).toBe("");
    expect(routeEventIdentity("   ")).toBe("");
  });

  it("tolerates the leading and repeated whitespace `split_whitespace` skips", () => {
    expect(routeEventIdentity("  identity=alice   reason=label")).toBe("alice");
    expect(routeEventIdentity("\tidentity=alice\nreason=label")).toBe("alice");
  });
});

// The tri-state the daemon records per run (STUDIO-735): a `teams.route` row NAMES a teammate,
// a `teams.unrouted` row answers "nobody" definitively — the run itself said so — and a run with
// no row at all is not an answer, so the caller falls back to the live roster.
describe("runIdentities — the ticket's per-run dispatch record, keyed by run", () => {
  it("names each run from its own route event", () => {
    const map = runIdentities([
      hit({ run_id: 547, text: "identity=jimmy reason=label" }),
      hit({ run_id: 522, text: "identity=alice reason=label" }),
    ]);
    expect(map.get(547)).toBe("jimmy");
    expect(map.get(522)).toBe("alice");
  });

  it("records an unrouted run as a definite nobody, distinct from no record at all", () => {
    const map = runIdentities([
      hit({ run_id: 522, kind: UNROUTED_EVENT_KIND, text: "reason=solo" }),
    ]);
    expect(map.has(522)).toBe(true);
    expect(map.get(522)).toBe("");
    expect(map.has(999)).toBe(false);
  });

  // The daemon probes `teams.route` FIRST and returns it unconditionally; its unrouted probe only
  // runs for a run that recorded no route (`run_identity`, crates/orchestrator/src/lifecycle.rs).
  // Letting the two kinds compete on `seq` instead would resolve a run carrying both by whichever
  // came first — and answer a DEFINITE nobody for a run the daemon reads as routed.
  it("prefers a route row over an unrouted one, whatever their seqs", () => {
    const both = (routeSeq: number, unroutedSeq: number) =>
      runIdentities([
        hit({ run_id: 547, seq: routeSeq, text: "identity=alice reason=label" }),
        hit({ run_id: 547, seq: unroutedSeq, kind: UNROUTED_EVENT_KIND, text: "reason=solo" }),
      ]).get(547);
    expect(both(3, 1)).toBe("alice");
    expect(both(1, 3)).toBe("alice");
  });

  // `search_events` orders `e.seq DESC` and the daemon's probe is `LIMIT 1`, so the HIGHEST seq is
  // the row it reads. Not reachable today — every routing row in the real store sits at seq 1 and
  // no run carries two — but the fold claims to mirror the daemon, so it has to mirror it here too.
  it("keeps the highest-seq row when a run somehow carries more than one, as the daemon does", () => {
    const map = runIdentities([
      hit({ run_id: 547, seq: 4, text: "identity=jimmy reason=label" }),
      hit({ run_id: 547, seq: 1, text: "identity=alice reason=label" }),
    ]);
    expect(map.get(547)).toBe("jimmy");
  });

  // The daemon reads ONE route row and falls through to the unrouted probe when it carries no
  // identity — it does not go looking for an older route row that parses, and neither does this.
  it("falls through an unparseable top route row to the unrouted probe, not to an older route", () => {
    const rows = [
      hit({ run_id: 547, seq: 4, text: "reason=label" }),
      hit({ run_id: 547, seq: 1, text: "identity=alice reason=label" }),
    ];
    expect(runIdentities(rows).has(547)).toBe(false);
    const withUnrouted = runIdentities([
      ...rows,
      hit({ run_id: 547, seq: 2, kind: UNROUTED_EVENT_KIND, text: "reason=solo" }),
    ]);
    expect(withUnrouted.get(547)).toBe("");
  });

  it("ignores a row of any other kind, and a route row carrying no identity at all", () => {
    const map = runIdentities([
      hit({ run_id: 547, kind: "text", text: "identity=mallory reason=label" }),
      hit({ run_id: 522, text: "reason=label" }),
    ]);
    expect(map.has(547)).toBe(false);
    // A `teams.route` row whose text carries no `identity=` is no route at all — the daemon's own
    // reading (`crates/orchestrator/src/lifecycle.rs`) falls through it to the unrouted probe.
    expect(map.has(522)).toBe(false);
  });

  it("answers an empty map for an empty search", () => {
    expect(runIdentities([]).size).toBe(0);
  });
});
