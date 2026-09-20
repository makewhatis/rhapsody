import { Note } from "@/components/console/Note";
import { useStateQuery } from "@/hooks/useStateQuery";

/**
 * The console's view of a pull request whose review loop has stopped moving — either neither
 * progressing nor reported blocked, or stopped with a stated cause and remedy (STUDIO-898,
 * STUDIO-956).
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
 */
export function DivergenceBanner() {
  const state = useStateQuery();
  const rows = state.data?.review_divergence ?? [];
  if (rows.length === 0) return null;
  return (
    <div role="status" className="setuperr">
      <Note variant="warn">
        {rows.length === 1
          ? "1 pull request needs attention:"
          : `${rows.length} pull requests need attention:`}{" "}
        {rows.map((d) => (
          <span key={`${d.pr}:${d.reviewer}`} style={{ display: "block" }}>
            {d.pr}
            {d.ticket ? ` (${d.ticket})` : ""} — {d.detail}, {humanStale(d.stale_secs)}.
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
 * Coarse on purpose: the staleness rules only report past a ninety-minute threshold, so a
 * second-accurate rendering would imply a precision the measurement does not have, and the decision
 * an operator makes from it ("is this minutes or is this all morning?") never needs more.
 *
 * The minutes branch is reachable: the kinds with NO staleness threshold are reported the moment
 * they happen, so a fresh one arrives as seconds since the newest run — often `0`. Those are
 * `review_escalated`, `review_shipped` and `round_budget_exhausted`: the adjudication arm reports the
 * two decider kinds regardless of age, and the legacy cap reports the budget kind as soon as it is
 * spent. (`round_budget_exhausted` still fires in an adjudicating install whenever the threshold is
 * ABOVE `REVIEW_ROUNDS_PER_PR_CAP` — `review.adjudicate_after_rounds` deliberately has no upper
 * clamp, so a threshold of 9 never fires and the cap stops the loop first.) The staleness-rule kinds
 * floor at ninety minutes and therefore bottom out at "1 hour"; the arithmetic still clamps to a
 * minimum of one unit so a real divergence is never rendered as "0 minutes". Keep that whichever way
 * the threshold moves.
 */
function humanStale(secs: number): string {
  const hours = Math.floor(secs / 3600);
  if (hours >= 24) {
    const days = Math.floor(hours / 24);
    return `for ${days} ${days === 1 ? "day" : "days"}`;
  }
  if (hours >= 1) return `for ${hours} ${hours === 1 ? "hour" : "hours"}`;
  const minutes = Math.max(1, Math.floor(secs / 60));
  return `for ${minutes} ${minutes === 1 ? "minute" : "minutes"}`;
}
