import { Note } from "@/components/console";
import { Onboarding } from "@/components/onboarding/Onboarding";
import { cn } from "@/lib/utils";
import "@/theme/console-firstrun.css";

// FirstRun — the console's first-run screen (STUDIO-681 §8.1, built by STUDIO-692). It is what
// the shell shows INSTEAD of the rail and the router when the supervisor reports `configured:
// false`: a fresh install has no WORKFLOW.md, so every destination the rail offers is backed by a
// config that does not exist yet. The Podium shell answers that state by dropping its own chrome
// for the wizard (`components/shell/AppShell.tsx`), and this is the same answer in console dress —
// the STUDIO-687 audit blocked the go-live flip (gap G2) precisely because the console did not
// have one.
//
// It does NOT rebuild the wizard. `Onboarding` (the shipped first-run flow, mock 2e) is embedded
// as it is, on its own bindings data path — `credentialStatus` / `setLinearToken` /
// `listLinearProjects` / `probeTools` / `writeInitialConfig` — so there is one wizard reached from
// two shells, exactly as STUDIO-690 gave the WORKFLOW.md editor one implementation and two
// shells. What this view contributes is the console's setup chrome: the branded bar, the narrow
// centred column, and the lifted failure banner.

export interface FirstRunViewProps {
  /** Called on a SUCCESSFUL seed — the shell re-reads status and swaps in the console. */
  onConfigured: () => void;
  /** Lift a partial-write failure to the shell, which outlives this view's unmount (see below). */
  onError: (msg: string) => void;
  /** The lifted failure, rendered here while the wizard is still the whole screen. */
  error: string;
  onDismissError: () => void;
  /**
   * The window has the macOS "Overlay" title bar (STUDIO-701). The setup bar is the console's
   * whole chrome on a fresh install, so on the desktop it is also the window's title bar: it
   * reserves the traffic lights' corner and carries the drag region. See `AppShell`.
   */
  overlayTitlebar?: boolean;
}

export function FirstRunView({
  onConfigured,
  onError,
  error,
  onDismissError,
  overlayTitlebar = false,
}: FirstRunViewProps) {
  return (
    <div className={cn("rh-console", "setup", overlayTitlebar && "overlay-titlebar")}>
      {/* The rail's identity without the rail: there is nothing to navigate to yet, so the bar
          carries the rail's own `.logo` lockup and a SETUP marker, and nothing else (Podium's
          `SetupToolbar` makes the same trade). */}
      {/* Unlike the rail, this bar IS horizontal, so it takes the traffic lights the way Podium's
          toolbar did: the drag region is the bar itself, with a left reserve for the lights
          (console-firstrun.css). The attribute is unconditional where the rail's strip is not,
          and the difference is deliberate — the rail's is an ELEMENT, which would take layout
          space in a browser that does not need it, while this is an attribute on a bar that
          exists either way, inert without the Tauri host exactly as Podium's `Toolbar` leaves it.
          The reserve is still gated, so a browser sees the bar it always saw. */}
      <header className="setuphead" data-tauri-drag-region="">
        <span className="logo">
          <span className="mk" aria-hidden="true">
            R
          </span>
          <b>rhapsodyd</b>
        </span>
        <span className="caps">Setup</span>
      </header>
      <main className="setupwrap">
        <OnboardErrorBanner message={error} onDismiss={onDismissError} />
        {/* The embedded Podium wizard. Same scope restoration as the WORKFLOW.md editor's
            `.wfembed`: inside `.rh-console` the token names both palettes spell the same way
            carry the CONSOLE's meanings, and these are Podium components — so `--accent` (a hover
            BACKGROUND to Podium, the brand amber to the console) and `--line` are handed back. */}
        <div className="obembed">
          <Onboarding onConfigured={onConfigured} onError={onError} />
        </div>
      </main>
    </div>
  );
}

/**
 * The lifted first-run failure. Rendered here while the wizard is the whole screen, and by
 * `ConsoleApp` inside the shell afterwards — one banner, because the message's whole purpose is
 * to outlive the moment the wizard is swapped out from under it.
 *
 * Empty message renders nothing, so both call sites can render it unconditionally.
 */
export function OnboardErrorBanner({ message, onDismiss }: { message: string; onDismiss: () => void }) {
  if (message === "") return null;
  return (
    <div role="alert" className="setuperr">
      <Note variant="warn">
        {message}{" "}
        <button type="button" className="link" onClick={onDismiss}>
          Dismiss
        </button>
      </Note>
    </div>
  );
}
