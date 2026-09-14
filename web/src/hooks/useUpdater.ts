import * as React from "react";
import {
  activeRunCount,
  checkForUpdate,
  downloadUpdate,
  installUpdate,
  onUpdateAvailable,
  onUpdateDownloadProgress,
  type DrainOutcome,
  type UpdateDownloadProgress,
  type UpdateInfo,
} from "@/lib/bindings";
import { updatePending, type UpdaterPhase } from "@/lib/updater-model";

// The live update model the UI renders: the phase-machine state plus the actions the Settings
// "Updates" surface and the toolbar gear dot drive. One instance is owned by the shell (so the gear
// dot and the panel share a single source of truth) and passed down.
export interface Updater {
  phase: UpdaterPhase;
  /** The checked/announced update metadata, when known. */
  info: UpdateInfo | null;
  /** The latest download-progress tick, when downloading. */
  progress: UpdateDownloadProgress | null;
  /** The last error message, when `phase === "error"`. */
  error: string | null;
  /** The active-run count when an install is awaiting confirmation (drives the warn dialog); else null. */
  activeRunsPrompt: number | null;
  /**
   * What the last drain-then-install actually did (STUDIO-880), or null when none was asked for.
   *
   * It is the difference between the two ways an install can end up deferred: the agents were still
   * playing and we never waited, or we waited the whole budget and they did not finish. The
   * "deferred" copy says which, because an expired budget ALSO leaves the daemon drained — taking
   * no new work — and an operator told only "scheduled for next quit" would never learn that.
   */
  drainOutcome: DrainOutcome | null;
  /** True while an update is waiting on the user — lights the gear + "Updates" rail dot. */
  pending: boolean;
  /** Run a manual check ("Check for updates"). */
  check: () => void;
  /** Download the available update (progress arrives via the download-progress event). */
  download: () => void;
  /** "Restart to finish": guard on active runs, then install directly or open the warn dialog. */
  requestInstall: () => void;
  /** Warn dialog → stop the active agents and install + relaunch now. */
  confirmInstallNow: () => void;
  /**
   * Warn dialog → drain first: stop the daemon taking NEW work, let the agents finish the turn they
   * are on, and install only then. Interrupts nothing, and can take many minutes.
   */
  drainThenInstall: () => void;
  /** Warn dialog → install on the next graceful quit instead (leaves the agents running). */
  deferToQuit: () => void;
  /** Warn dialog → cancel, leaving the update ready. */
  dismissPrompt: () => void;
}

function errMessage(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

// Phases mid-flow that the passive launch-check event must not overwrite: once the user has started
// downloading/installing (or deferred), a re-fired `update:available` should not bounce them back to
// the bare "available" affordance.
function inFlight(phase: UpdaterPhase): boolean {
  return (
    phase === "downloading" ||
    phase === "ready" ||
    phase === "draining" ||
    phase === "installing" ||
    phase === "deferred"
  );
}

// useUpdater wires U1's Tauri update commands + events (TRA-260) into the phase machine the U3 UI
// renders. Every binding degrades to a no-op / null without the Tauri bridge (a plain browser or a
// unit test), so the hook is inert-but-safe there: the phase stays "idle" and the actions resolve to
// nothing. Mount ONE instance high in the tree (the shell) and share it, so a manual check in the
// Updates panel and the gear dot never disagree.
export function useUpdater(): Updater {
  const [phase, setPhase] = React.useState<UpdaterPhase>("idle");
  const [info, setInfo] = React.useState<UpdateInfo | null>(null);
  const [progress, setProgress] = React.useState<UpdateDownloadProgress | null>(null);
  const [error, setError] = React.useState<string | null>(null);
  const [activeRunsPrompt, setActiveRunsPrompt] = React.useState<number | null>(null);
  const [drainOutcome, setDrainOutcome] = React.useState<DrainOutcome | null>(null);

  // Guard against state updates after unmount: the actions resolve asynchronously and the install
  // path may outlive the panel (the user navigates away while it downloads).
  const mounted = React.useRef(true);
  React.useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);

  // Quiet on-launch check: badge the affordance when the host announces an update, unless the user
  // has already moved past "available" in this session.
  React.useEffect(
    () =>
      onUpdateAvailable((next) => {
        if (!mounted.current) return;
        setInfo(next);
        setPhase((p) => (inFlight(p) ? p : "available"));
      }),
    [],
  );

  // Per-chunk download progress for the bar.
  React.useEffect(
    () =>
      onUpdateDownloadProgress((p) => {
        if (mounted.current) setProgress(p);
      }),
    [],
  );

  const check = React.useCallback(() => {
    setError(null);
    setPhase("checking");
    void checkForUpdate()
      .then((next) => {
        if (!mounted.current) return;
        if (!next) {
          // No bridge (plain browser): nothing to check against.
          setPhase("idle");
          return;
        }
        setInfo(next);
        setPhase(next.available ? "available" : "up-to-date");
      })
      .catch((e: unknown) => {
        if (!mounted.current) return;
        setError(errMessage(e));
        setPhase("error");
      });
  }, []);

  const download = React.useCallback(() => {
    setError(null);
    setProgress({ downloaded: 0, total: null });
    setPhase("downloading");
    void downloadUpdate()
      .then(() => {
        if (mounted.current) setPhase("ready");
      })
      .catch((e: unknown) => {
        if (!mounted.current) return;
        setError(errMessage(e));
        setPhase("error");
      });
  }, []);

  // Install the update, letting U1's guard defer (unless `force`) when runs are active. On the
  // allowed path the host relaunches and this never resolves; a resolved report means either a
  // deferral (blocked_active_runs > 0) or no bridge (null → nothing happened).
  //
  // `drain` (STUDIO-880) asks the host to settle the daemon FIRST — new dispatch stops, the running
  // agents finish the turn they are on, and only then does the guard read the count. It interrupts
  // nothing, so it is the one path that can install over live work without throwing a turn away,
  // and it is the slow one: the host's budget is 30 minutes. It is meaningless with `force`, which
  // skips the guard entirely, so the two are never both set.
  const doInstall = React.useCallback((force: boolean, drain = false) => {
    setError(null);
    setActiveRunsPrompt(null);
    setDrainOutcome(null);
    setPhase(drain ? "draining" : "installing");
    void installUpdate(force, drain)
      .then((report) => {
        if (!mounted.current) return;
        if (!report) {
          setPhase("ready"); // no bridge — restore the ready affordance
          return;
        }
        setDrainOutcome(report.drain ?? null);
        if (report.blocked_active_runs > 0) setPhase("deferred");
        // else: installed; the host relaunch is imminent, leave the in-flight phase showing.
      })
      .catch((e: unknown) => {
        if (!mounted.current) return;
        setError(errMessage(e));
        setPhase("error");
      });
  }, []);

  // "Restart to finish": probe the live run count first so we can WARN before stopping any agents.
  // Zero runs → install straight away; otherwise open the confirm dialog. A failed probe falls back
  // to a guarded install (force=false), which U1 re-checks and defers on its own if needed.
  const requestInstall = React.useCallback(() => {
    void activeRunCount()
      .then((n) => {
        if (!mounted.current) return;
        if (n > 0) setActiveRunsPrompt(n);
        else doInstall(false);
      })
      .catch(() => {
        if (mounted.current) doInstall(false);
      });
  }, [doInstall]);

  const confirmInstallNow = React.useCallback(() => doInstall(true), [doInstall]);
  const drainThenInstall = React.useCallback(() => doInstall(false, true), [doInstall]);
  const deferToQuit = React.useCallback(() => doInstall(false), [doInstall]);
  const dismissPrompt = React.useCallback(() => setActiveRunsPrompt(null), []);

  return {
    phase,
    info,
    progress,
    error,
    activeRunsPrompt,
    drainOutcome,
    pending: updatePending(phase),
    check,
    download,
    requestInstall,
    confirmInstallNow,
    drainThenInstall,
    deferToQuit,
    dismissPrompt,
  };
}
