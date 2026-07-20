// Pure, framework-free helpers for the P11-U3 in-app update UI. The stateful wiring (commands +
// events) lives in `useUpdater`; the display math lives here so it is unit-testable without React.
import type { UpdateDownloadProgress } from "@/lib/bindings";

// The update UI's state machine (drives the Settings "Updates" surface + the gear/rail dot):
//   idle       — the app hasn't checked yet (or a check was reset); offer "Check for updates".
//   checking   — a manual "Check for updates" is in flight.
//   up-to-date — a check found no newer version; show the running version.
//   available  — a newer version exists (not yet downloading); offer "What's new" + "Download".
//   downloading— the update is downloading; show a progress bar.
//   ready      — the update downloaded; offer "Restart to finish".
//   installing — an install is in flight (may relaunch, or defer when runs are active).
//   deferred   — the install was deferred to the next quit (runs were active); offer restart-now.
//   error      — the last check/download/install failed; surface the message + a retry.
export type UpdaterPhase =
  | "idle"
  | "checking"
  | "up-to-date"
  | "available"
  | "downloading"
  | "ready"
  | "installing"
  | "deferred"
  | "error";

// downloadPercent maps a progress tick to a 0–100 bar value, or null when there is nothing
// determinate to show — no progress yet, or the server sent no Content-Length (total null/0), in
// which case the UI renders an indeterminate bar instead. Clamped so a slight overshoot never
// exceeds a full bar.
export function downloadPercent(progress: UpdateDownloadProgress | null): number | null {
  if (!progress || progress.total == null || progress.total <= 0) return null;
  const pct = (progress.downloaded / progress.total) * 100;
  return Math.min(100, Math.round(pct));
}

// formatBytes renders a byte count as a short human string (B / KB / MB, one decimal at KB+), used
// for the "X of Y" download readout. Release artifacts are tens of MB, so MB is the top unit.
export function formatBytes(n: number): string {
  const KB = 1024;
  const MB = KB * 1024;
  if (n < KB) return `${n} B`;
  if (n < MB) return `${(n / KB).toFixed(1)} KB`;
  return `${(n / MB).toFixed(1)} MB`;
}

// updatePending reports whether an update is waiting on the user — the signal that lights the
// Settings gear dot (and the "Updates" rail dot). True from the moment one is found through
// download/install/defer; false when idle, checking, up to date, or errored.
export function updatePending(phase: UpdaterPhase): boolean {
  return (
    phase === "available" ||
    phase === "downloading" ||
    phase === "ready" ||
    phase === "installing" ||
    phase === "deferred"
  );
}
