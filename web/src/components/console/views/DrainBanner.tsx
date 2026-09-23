import { useMutation, useQueryClient } from "@tanstack/react-query";
import { Button } from "@/components/ui";
import { Note } from "@/components/console/Note";
import { STATE_QUERY_KEY, useStateQuery } from "@/hooks/useStateQuery";
import { setDrain } from "@/lib/api";
import { errText } from "@/lib/teams-model";

/**
 * The console's view of an armed drain, and the way out of one (STUDIO-880).
 *
 * A drain stops the daemon taking NEW work so its in-flight runs can reach a turn boundary before a
 * restart. From the outside that looks exactly like a wedged daemon — tickets sit in Todo and
 * nothing is dispatched — so **silence is this feature's failure mode**, and saying so is the point
 * of this banner rather than a nicety. It is the console half of the same advisory the daemon puts
 * on every project's `/api/v1/projects` status and logs at WARN.
 *
 * **Cancel is the other half, and it is not optional.** A drain outlives the thing that asked for
 * one: a desktop wait whose 30-minute budget expires deliberately leaves the drain ARMED (nothing is
 * interrupted, nothing is restarted), so from then on the daemon takes no work at all. Without a
 * control here the only ways out were a restart — throwing away the very turn the drain was
 * protecting — or a hand-rolled `curl` against `/api/v1/drain`. It goes over HTTP rather than the
 * desktop bridge so it works in both hosts; see [`setDrain`].
 *
 * It renders nothing at all unless a drain is armed: the daemon omits the `drain` key entirely while
 * dispatching normally, so a console pointed at an ordinary daemon has no banner to hide.
 */
export function DrainBanner() {
  const state = useStateQuery();
  const qc = useQueryClient();
  const cancel = useMutation({
    mutationFn: () => setDrain(false),
    // Settled, not success: a refused cancel must re-read rather than leave the banner asserting a
    // state the daemon may not be in.
    onSettled: () => void qc.invalidateQueries({ queryKey: STATE_QUERY_KEY }),
  });
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
          : "Nothing is in flight — restarting now interrupts nothing."}{" "}
        <Button
          type="button"
          variant="subtle"
          size="sm"
          disabled={cancel.isPending}
          onClick={() => cancel.mutate()}
        >
          {cancel.isPending ? "Cancelling…" : "Cancel drain"}
        </Button>
        {cancel.isError ? ` The daemon refused the cancel: ${errText(cancel.error)}` : null}
      </Note>
    </div>
  );
}
