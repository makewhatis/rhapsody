import { Note } from "@/components/console/Note";
import { useStateQuery } from "@/hooks/useStateQuery";

/**
 * The console's view of a pull request whose board state and activity disagree (STUDIO-898).
 *
 * Six separate defects between 2026-09-12 and 2026-09-14 all presented as an idle board, and each
 * cost hours only because nobody could SEE it — eleven on STUDIO-875, six on STUDIO-893. The daemon
 * logged enough to diagnose the first of those for eleven hours and it was missed anyway, which is
 * why the log alone is not the deliverable: this is the console half of the same advisory the daemon
 * puts on every project's `/api/v1/projects` status and logs at WARN.
 *
 * **It reports and offers no control.** Unlike {@link DrainBanner}, whose condition the operator
 * themself armed and can cancel, a divergence has no known cause and therefore no button that could
 * honestly resolve it — re-dispatching on a rule nobody has watched fire is how a stall becomes a
 * loop. So the banner's whole job is to name the pull request and how long it has been stuck, and let
 * a human decide.
 *
 * It renders nothing at all on a healthy board: the daemon omits the `review_divergence` key entirely
 * unless its sweep reported something, so an ordinary console has no banner to suppress. That
 * silence is load-bearing — a permanent warning nobody reads is precisely the failure this exists to
 * prevent.
 *
 * A row the review watcher is HOLDING for want of a global slot carries a `capacity_held`
 * annotation (STUDIO-950). That wait is deliberate and the daemon knows why, so the row says so and
 * names the budget — the banner never implies a held round is an unexplained stall. The heading
 * claims only what is always true of every row ("board state and activity disagree"); the old
 * "neither progressing nor reported blocked" would have been false the moment a hold was known.
 */
export function DivergenceBanner() {
  const state = useStateQuery();
  const rows = state.data?.review_divergence ?? [];
  if (rows.length === 0) return null;
  return (
    <div role="status" className="setuperr">
      <Note variant="warn">
        {rows.length === 1
          ? "1 pull request's board state and activity disagree:"
          : `${rows.length} pull requests' board state and activity disagree:`}{" "}
        {rows.map((d) => (
          <span key={`${d.pr}:${d.reviewer}`} style={{ display: "block" }}>
            {d.pr}
            {d.ticket ? ` (${d.ticket})` : ""} — {d.detail}, {humanStale(d.stale_secs)}.
            {d.capacity_held
              ? ` It is held for capacity: ${d.capacity_held.holders} run(s) hold the ${d.capacity_held.budget} budget, so no reviewer run can start yet.`
              : ""}
          </span>
        ))}
        Nothing has been changed on your behalf — this is a report.
      </Note>
    </div>
  );
}

/**
 * How long the obligation has been outstanding, in the coarsest honest unit.
 *
 * Coarse on purpose: the daemon only reports past a ninety-minute threshold, so a second-accurate
 * rendering would imply a precision the measurement does not have, and the decision an operator
 * makes from it ("is this minutes or is this all morning?") never needs more.
 *
 * That same threshold means the minutes branch cannot be reached by anything the daemon sends today —
 * it is the floor for a future lower threshold, and for the arithmetic never to render a real
 * divergence as "0 hours". Keep it whichever way the threshold moves.
 */
function humanStale(secs: number): string {
  const hours = Math.floor(secs / 3600);
  if (hours >= 24) {
    const days = Math.floor(hours / 24);
    return `for ${days} ${days === 1 ? "day" : "days"}`;
  }
  if (hours >= 1) return `for ${hours} ${hours === 1 ? "hour" : "hours"}`;
  return `for ${Math.max(1, Math.floor(secs / 60))} minutes`;
}
