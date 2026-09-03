import { phaseGlyph } from "@/lib/console-trace-view";
import { phaseTitle, type PhaseKind, type TracePhase } from "@/lib/trace-model";

// console-trace-spark — the Jobs worklist's row trace-sparkline (design record
// `~/.rhapsody/docs/console-run-detail-design.md` §6; slice 6 of its §9 plan).
//
// §6's requirement is that "glance-view and deep-view share one vocabulary": the strip on a
// worklist row is drawn with the SAME phase glyphs the run-detail spine uses, over the SAME
// slice-1 phases. So this module derives from `trace-model` and borrows `phaseGlyph` rather than
// keeping a second table that could drift from the spine's.
//
// WHY A CHECKLIST OF KINDS AND NOT THE PHASE SEQUENCE. The obvious sparkline — one glyph per
// phase, in order — does not fit a table cell and does not say anything either. Measured over the
// 453 recorded runs (`GET /api/v1/history` + each run's transcript, then the real `buildTrace`), a
// run has a MEDIAN of 27 phases and up to 149, and collapsing consecutive repeats barely helps
// (median 27) because real work alternates: `oriented>implemented>oriented>implemented>…`. What
// survives is which KINDS of work the run did, and how much of each.
//
// So the strip is a CHECKLIST, not a timeline: one RESERVED slot per kind, always all six, always
// in the model's own declaration order, an absent kind drawn as an empty slot rather than dropped.
// That is a deliberate choice between two honest options, and the tests pin it:
//
//   - Ordering the slots by FIRST APPEARANCE would make the strip the chronology it visually looks
//     like — but it loses the row-to-row comparability, and the order it would print is not stable:
//     over those 453 runs the declaration order disagrees with first appearance in 403 (89%).
//   - Keeping the fixed order but COLLAPSING the gaps (the earlier shape of this module) delivers
//     neither. It reads as a left-to-right chronology it does not have, and the columns still do not
//     line up: `handoff` landed in 4 different columns and `other` in 5, so only `oriented` was
//     column-stable.
//
// Reserving the slot is what makes the fixed order pay for itself — column 3 is "Verified" on every
// row, an empty slot is a visible "this run never tested", and a row IS comparable with the row
// above it. The strip then reads as what it is (which kinds), not as what it is not (when).
//
// WHY THE COUNT IS IN THE CELL AND NOT ONLY THE TOOLTIP. Presence alone is a coarse signal: over
// those same 453 runs the six slots take only 26 distinct values and the modal one covers 186 rows
// (41%), so two of every five rows look identical while their tooltips differ. Tiering each slot's
// COUNT (see [`sparkWeight`]) adds the second dimension in the visible cell — 195 distinct cells,
// modal 16 rows (3.5%) — and the exact numbers stay in the accessible name for anyone who needs
// them, so the tier is a supplement and never the only carrier.
//
// WHY NO ERROR TINT. The prototype tints a failing step red, and `TracePhase.failed` is the
// obvious input — but it fires on 353 of those same 453 runs (305 of them on an `oriented` phase:
// a grep that matched nothing, a Read of a path that does not exist). A tint on 78% of rows is not
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
 * The strip's slots, in the spine's declaration order (`trace-model`'s `PhaseKind`): the design
 * record's five named phases, then `other` for work the model could not name. EVERY strip has
 * every one of these, which is what makes a column mean the same thing on every row.
 *
 * A kind absent from this list would be dropped from every sparkline silently, so the test asserts
 * it against the kinds `buildTrace` actually produces.
 */
export const SPARK_KINDS: readonly PhaseKind[] = [
  "oriented",
  "implemented",
  "verified",
  "coordinated",
  "handoff",
  "other",
];

/**
 * How heavily the run leaned on one kind — the visible cell's second dimension, from the count.
 *
 * `none` is the empty slot. The two boundaries between the three live tiers are the corpus's own
 * lower quartile and median over the 453 recorded runs' per-kind counts (p25 = 1, median = 4), so
 * they fall where the real distribution splits rather than on round numbers: one phase is `light`,
 * a handful is `mid`, and five or more — the top ~45% of lit slots — is `heavy`.
 */
export type SparkWeight = "none" | "light" | "mid" | "heavy";

export function sparkWeight(count: number): SparkWeight {
  if (count <= 0) return "none";
  if (count === 1) return "light";
  return count <= 4 ? "mid" : "heavy";
}

/** One slot of the strip. */
export interface SparkStep {
  /** The phase kind it stands for, or `live` for the playhead. */
  kind: PhaseKind | "live";
  glyph: string;
  /** The phase kind's own title ("Oriented"), or "Running now" — the tooltip and a11y text. */
  label: string;
  /** How many phases of this kind the run has; 0 for an empty slot and for the playhead. */
  count: number;
  /**
   * Whether the run actually reached this kind. `false` is a RESERVED, empty slot — the column
   * keeps its meaning and the row stays comparable. The playhead is always `true`: it is not a
   * kind the run could have skipped.
   */
  present: boolean;
  weight: SparkWeight;
}

/**
 * The strip for one run: one slot per phase kind, plus the playhead when it is still in flight.
 *
 * `[]` for a run with NO phases — a transcript that has not arrived, or a run that never logged a
 * tool call. That is deliberately not six empty slots: six empty slots say "this run did nothing",
 * and a run whose transcript nobody has read has not said that. The view renders `[]` as a dash.
 */
export function traceSpark(phases: readonly TracePhase[], live: boolean): SparkStep[] {
  const steps: SparkStep[] = [];
  if (phases.length > 0) {
    for (const kind of SPARK_KINDS) {
      const count = phases.filter((phase) => phase.kind === kind).length;
      steps.push({
        kind,
        glyph: phaseGlyph(kind),
        // The label comes from the kind, not from a phase instance: an empty slot has no phase to
        // read a title from, and it still has to say what it stands for.
        label: phaseTitle(kind),
        count,
        present: count > 0,
        weight: sparkWeight(count),
      });
    }
  }
  if (live) {
    steps.push({
      kind: "live",
      glyph: LIVE_GLYPH,
      label: "Running now",
      count: 0,
      present: true,
      weight: "none",
    });
  }
  return steps;
}

/**
 * The strip as one line of text — the cell's tooltip and its accessible name.
 *
 * It names what the run DID with exact counts, then the kinds it has not reached — which the empty
 * slots only imply, and "it never tested" is worth saying out loud. They are listed as a `none: …`
 * clause rather than "no Verified" so the phrasing survives every title in the model's own table
 * (`other` is titled "Worked", and "no Worked" is not a sentence).
 *
 * A LIVE run gets `not yet:` instead. Nothing is settled while the run is still going, and the
 * strip is explicitly a snapshot taken when the row was armed — telling a reader a run "reached
 * none" of a kind it may be about to reach would be the one thing this module must not do.
 */
export function sparkSummary(steps: readonly SparkStep[]): string {
  if (steps.length === 0) return "No trace";
  const live = steps.some((step) => step.kind === "live");
  const did = steps.filter((step) => step.kind !== "live" && step.present);
  const absent = steps.filter((step) => step.kind !== "live" && !step.present);
  const parts = did.map((step) => `${step.label} ×${step.count}`);
  if (live) parts.push("Running now");
  const head = parts.join(" · ");
  if (absent.length === 0) return head;
  const tail = `${live ? "not yet" : "none"}: ${absent.map((step) => step.label).join(", ")}`;
  return head === "" ? tail : `${head} — ${tail}`;
}
