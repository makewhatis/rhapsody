import type { LandingView } from "@/lib/api";
import { Pill, type PillVariant } from "./Pill";

const PHASE_TONES: Record<string, PillVariant> = {
  WaitingForDeploy: "queued", Watching: "run", AwaitingAcceptance: "review",
  Done: "done", Held: "operator", Reverting: "blocked", Reopened: "review",
};

export function landingForTicket(landings: readonly LandingView[], ticket: string) {
  // The API is newest-first, but a revert child verifies recovery, not ticket completion.
  return landings.find((landing) => landing.ticket === ticket && landing.revert_of == null);
}

export function LandingBadge({ landing }: { landing?: LandingView }) {
  if (!landing) return null;
  const paused = landing.pausing_auto_merge.length > 0;
  const details = [
    landing.phase_age_s === null ? "phase age unknown" : `${landing.phase_age_s}s in this phase`,
    landing.hold_reason,
    paused ? `Pausing auto-merge: ${landing.pausing_auto_merge.join(", ")}` : null,
  ].filter(Boolean).join(" · ");
  return <Pill variant={PHASE_TONES[landing.phase] ?? "operator"} title={details}>
    Landing: {landing.phase}{landing.first_failing_check ? ` · ${landing.first_failing_check}` : ""}{paused ? " · auto-merge paused" : ""}
  </Pill>;
}
