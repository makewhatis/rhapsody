import { Note } from "@/components/console/Note";
import { useStateQuery } from "@/hooks/useStateQuery";

/**
 * The console's view of an armed drain (STUDIO-880).
 *
 * A drain stops the daemon taking NEW work so its in-flight runs can reach a turn boundary before a
 * restart. From the outside that looks exactly like a wedged daemon — tickets sit in Todo and
 * nothing is dispatched — so **silence is this feature's failure mode**, and saying so is the point
 * of this banner rather than a nicety. It is the console half of the same advisory the daemon puts
 * on every project's `/api/v1/projects` status and logs at WARN.
 *
 * It renders nothing at all unless a drain is armed: the daemon omits the `drain` key entirely while
 * dispatching normally, so a console pointed at an ordinary daemon has no banner to hide.
 */
export function DrainBanner() {
  const state = useStateQuery();
  const drain = state.data?.drain;
  if (!drain?.active) return null;
  const running = state.data?.running.length ?? 0;
  return (
    <div role="status" className="setuperr">
      <Note variant="warn">
        {drain.reason === "update"
          ? "Draining for an update — no new work is being started."
          : "Draining — no new work is being started."}{" "}
        {running > 0
          ? `${running} ${running === 1 ? "run is" : "runs are"} finishing the current turn; nothing has been interrupted.`
          : "Nothing is in flight — restarting now interrupts nothing."}
      </Note>
    </div>
  );
}
