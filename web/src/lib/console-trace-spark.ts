import { phaseGlyph } from "@/lib/console-trace-view";
import type { PhaseKind, TracePhase } from "@/lib/trace-model";

// console-trace-spark — the Jobs worklist's row trace-sparkline (design record
// `~/.rhapsody/docs/console-run-detail-design.md` §6; slice 6 of its §9 plan).
//
// §6's requirement is that "glance-view and deep-view share one vocabulary": the strip on a
// worklist row is drawn with the SAME phase glyphs the run-detail spine uses, over the SAME
// slice-1 phases. So this module derives from `trace-model` and borrows `phaseGlyph` rather than
// keeping a second table that could drift from the spine's.
//
// WHY A PRESENCE STRIP AND NOT THE PHASE SEQUENCE. The obvious sparkline — one glyph per phase, in
// order — does not fit a table cell and does not say anything either. Measured over the 400 most
// recent recorded runs (`GET /api/v1/history` + each run's transcript, then `buildTrace`), a run
// has a MEDIAN of 27 phases and up to 149, and collapsing consecutive repeats barely helps
// (median 27) because real work alternates: `oriented>implemented>oriented>implemented>…`.
// Ordering the DISTINCT kinds by first appearance is no better — it disagreed with the spine's own
// order in 357 of the 400 runs, so it would print a chronology ("verified before implemented")
// that the run did not have. What survives both is which KINDS of work the run did: 300 of the 400
// runs touch 5 or 6 distinct kinds, which is a 5–6 glyph strip, and the count behind each glyph
// carries the weight. The order is then the model's own, fixed, so the glyphs line up down the
// column and a row is comparable with the row above it.
//
// WHY NO ERROR TINT. The prototype tints a failing step red, and `TracePhase.failed` is the
// obvious input — but it fires on 318 of those same 400 runs (274 of them on an `oriented` phase:
// a grep that matched nothing, a Read of a path that does not exist). A tint on 80% of rows is not
// a signal. The run's OUTCOME is the honest one and the adjacent Status pill already carries it,
// so the strip stays about shape alone.

/**
 * The playhead. Not a phase — the run has not finished the step it is on.
 *
 * The prototype draws it as a filled disc, which is a hair away from `other`'s bullet at the 11px
 * this cell renders at; the difference would then be carried by the teal tint alone, which is a
 * colour-only distinction. A play head says the same thing and is unmistakable, and the test below
 * holds it apart from every phase glyph.
 */
export const LIVE_GLYPH = "▶";

/**
 * The phase kinds a strip may show, in the spine's declaration order (`trace-model`'s
 * `PhaseKind`): the design record's five named phases, then `other` for work the model could not
 * name. A kind absent from this list would be dropped from every sparkline silently, so the test
 * asserts it against the kinds `buildTrace` actually produces.
 */
export const SPARK_KINDS: readonly PhaseKind[] = [
  "oriented",
  "implemented",
  "verified",
  "coordinated",
  "handoff",
  "other",
];

/** One glyph of the strip. */
export interface SparkStep {
  /** The phase kind it stands for, or `live` for the playhead. */
  kind: PhaseKind | "live";
  glyph: string;
  /** The phase's own title ("Oriented"), or "Running now" — what the tooltip and a11y text say. */
  label: string;
  /** How many phases of this kind the run has; 0 for the playhead, which counts nothing. */
  count: number;
}

/**
 * The strip for one run: its distinct phase kinds, plus the playhead when it is still in flight.
 *
 * `[]` for a run with no phases — a transcript that has not arrived, or a run that never logged a
 * tool call. The view renders that as a dash: an empty strip is the honest answer, and inventing a
 * shape for a run whose transcript nobody has read is exactly what this module must not do.
 */
export function traceSpark(phases: readonly TracePhase[], live: boolean): SparkStep[] {
  const steps: SparkStep[] = [];
  for (const kind of SPARK_KINDS) {
    const of = phases.filter((phase) => phase.kind === kind);
    if (of.length === 0) continue;
    // The label is the phase's OWN title rather than a copy of the model's title table, for the
    // same reason the glyph is `phaseGlyph`: one vocabulary, one place it is written down.
    steps.push({ kind, glyph: phaseGlyph(kind), label: of[0].title, count: of.length });
  }
  if (live) steps.push({ kind: "live", glyph: LIVE_GLYPH, label: "Running now", count: 0 });
  return steps;
}

/** The strip as one line of text — the cell's tooltip and its accessible name. */
export function sparkSummary(steps: readonly SparkStep[]): string {
  if (steps.length === 0) return "No trace";
  return steps
    .map((step) => (step.count === 0 ? step.label : `${step.label} ×${step.count}`))
    .join(" · ");
}
