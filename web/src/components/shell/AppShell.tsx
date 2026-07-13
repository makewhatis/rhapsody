import * as React from "react";
import { Toolbar } from "@/components/Toolbar";
import { type SettingsTabId, type TopTabId, TOP_PANEL_ID } from "./placeholders";
import { Settings } from "@/components/settings/Settings";
import { RunsView } from "@/components/runs/RunsView";
import { Onboarding } from "@/components/onboarding/Onboarding";
import { ToastProvider, useToast } from "./Toast";
import { useDaemonStatus } from "@/hooks/useDaemonStatus";
import { useStateQuery } from "@/hooks/useStateQuery";
import { conductorStatus, viewForStatus } from "@/lib/daemon-status";
import { appVersion, hasBridge, onNavigate, onShuttingDown, openExternal, type VersionDTO } from "@/lib/bindings";
import { StatusDot } from "@/components/ui";

// AppShell — the macOS window shell that hosts the whole UI: the single 46px "Podium" toolbar
// (wordmark, conductor status, Linear/Tools shortcuts, daemon transport, Settings gear) as the first
// row, the Runs dashboard as the main area (Settings toggles in over it via the gear), and toasts.
export function AppShell() {
  return (
    <div
      style={{
        height: "100vh",
        display: "flex",
        flexDirection: "column",
        background: "var(--bg-app)",
        color: "var(--tx)",
        position: "relative",
        overflow: "hidden",
      }}
    >
      <ToastProvider>
        <ShellBody />
      </ToastProvider>
    </div>
  );
}

function ShellBody() {
  // Open on Runs — the dashboard you want at a glance; Settings is one click away.
  const [topTab, setTopTab] = React.useState<TopTabId>("runs");
  const [settingsTab, setSettingsTab] = React.useState<SettingsTabId>("general");
  // Onboarding partial-write failure ("config saved, but the daemon could not start"), lifted out
  // of the wizard. The wizard keeps the daemon from being told to start on failure (onConfigured is
  // success-only), but WriteInitialConfig may already have written WORKFLOW.md — so the ~2s
  // useDaemonStatus poll below can see configured: true and unmount the wizard, discarding its
  // inline alert. Persisting the message here keeps it visible (in the wizard or the dashboard)
  // until the user dismisses it. Mirrors the desktop shell's `onboardErr`.
  const [onboardErr, setOnboardErr] = React.useState("");
  // True once the user quits: the Go side stops the daemon off the main thread and emits
  // app:shutting-down so we can show a "Shutting down…" screen instead of a frozen window.
  const [shuttingDown, setShuttingDown] = React.useState(false);
  // The relative HTTP /api poll reaches the daemon in both hosts: a plain browser is served by the
  // daemon's own origin, and the Wails app reverse-proxies /api to the sidecar (desktop
  // apiProxyMiddleware). Health still comes from the Go bridge when present — it tracks the
  // supervisor lifecycle (Start/Stop/Restart) more directly than the HTTP snapshot.
  const bridge = hasBridge();
  const daemon = useDaemonStatus();
  const { toast } = useToast();
  const { data, isLoading, isError } = useStateQuery();

  // The macOS tray's "Open Dashboard" / "Settings…" items emit tray:navigate; switch the
  // active route to match (a no-op in a plain browser, where the Wails runtime is absent).
  React.useEffect(
    () =>
      onNavigate((view) => {
        if (view === "settings") {
          setTopTab("settings");
          setSettingsTab("general");
        } else {
          setTopTab("runs");
        }
      }),
    [],
  );

  // Show the shutdown screen when the app begins quitting (the daemon stops off the main thread).
  React.useEffect(() => onShuttingDown(() => setShuttingDown(true)), []);

  // A fresh install has no WORKFLOW.md → the daemon can't start and the Settings page (which
  // hydrates from /api) is unusable. Route into the onboarding wizard, which seeds the config via
  // the WriteInitialConfig binding (no daemon needed). Only ever true under the Wails bridge: a
  // plain browser's null status maps to "loading", not "not-configured".
  const view = viewForStatus(daemon.status);
  const notConfigured = view === "not-configured";

  // Conductor status: derive from the Wails bridge status when hosted natively, else from the HTTP
  // /api/v1/state poll (the daemon's own origin / the desktop reverse-proxy). Both feed the same
  // normalized signals so the toolbar renders one honest "what's the ensemble doing" cluster.
  const conductor = bridge
    ? conductorStatus({
        connecting: view === "loading" || view === "starting",
        reachable: true, // the local supervisor is always reachable — a stopped daemon reads as Paused
        running: view === "running",
        degraded: false, // the bridge status carries only a healthy flag (no degraded phase)
        agentCount: daemon.status?.agent_count ?? 0,
        pollMs: data?.poll_interval_ms,
      })
    : conductorStatus({
        connecting: isLoading || !data,
        reachable: !isError,
        running: !isError && !!data,
        degraded: data?.status === "degraded",
        agentCount: data?.running.length ?? 0,
        pollMs: data?.poll_interval_ms,
      });
  const daemonRunning =
    conductor.phase === "playing" || conductor.phase === "idle" || conductor.phase === "degraded";

  // Run a lifecycle action, then toast on success with an action-appropriate subtitle (the
  // titlebar status reflects any failure).
  const lifecycle = (label: string, detail: string, fn: () => Promise<void>) => {
    void fn()
      .then(() => toast(label, detail))
      .catch(() => {
        /* surfaced by the titlebar status label */
      });
  };

  return (
    <>
      <Toolbar
        conductor={conductor}
        running={daemonRunning}
        connecting={conductor.phase === "connecting"}
        busy={daemon.busy}
        settingsActive={topTab === "settings"}
        onStart={() => lifecycle("Daemon started", "The supervisor is running.", daemon.start)}
        onStop={() => lifecycle("Daemon stopped", "The supervisor is stopped.", daemon.stop)}
        onRestart={() => lifecycle("Daemon restarted", "Daemon reloaded configuration ✓", daemon.restart)}
        // The gear toggles Settings ↔ Runs (Runs is the whole main area; there is no tab strip).
        onToggleSettings={() => setTopTab((t) => (t === "settings" ? "runs" : "settings"))}
        // Open Linear in the browser; jump straight to the Tools settings tab.
        onOpenLinear={() => openExternal("https://linear.app")}
        onOpenTools={() => {
          setTopTab("settings");
          setSettingsTab("tools");
        }}
      />
      {/* Always reserve the vertical scrollbar's width so the centered content (max-width 1180,
          margin auto) never shifts left by a scrollbar width when a tab/run is tall enough to
          overflow — the size-dependent jitter between tabs (and Runs → Run detail). overflowY:
          "scroll" forces the track to always render (reliable for classic, space-taking scrollbars
          even where scrollbar-gutter is unsupported); scrollbarGutter: "stable" is the modern
          complement where the engine honors it. */}
      <div style={{ flex: 1, overflowY: "scroll", overflowX: "hidden", scrollbarGutter: "stable" }}>
        <div style={{ maxWidth: 1180, margin: "0 auto", padding: "26px 40px 60px" }}>
          {onboardErr ? <OnboardErrorBanner message={onboardErr} onDismiss={() => setOnboardErr("")} /> : null}
          {notConfigured ? (
            // First run: no config yet. Show the wizard instead of the daemon-dependent header/nav;
            // onConfigured re-reads status so the shell swaps to the dashboard once the daemon starts.
            // onError lifts a partial-write failure here so it outlives the poll-driven unmount.
            <Onboarding onConfigured={() => void daemon.refresh()} onError={setOnboardErr} />
          ) : (
            // Runs is the whole main area; the titlebar gear toggles Settings in over it. The panel
            // keeps role="tabpanel" + a label for the a11y tree even though the tab strip is gone.
            <div
              id={TOP_PANEL_ID}
              role="tabpanel"
              aria-label={topTab === "runs" ? "Runs" : "Settings"}
              tabIndex={0}
              style={{ outline: "none" }}
            >
              {topTab === "runs" ? (
                <RunsView />
              ) : (
                <Settings tab={settingsTab} onTab={setSettingsTab} onBack={() => setTopTab("runs")} />
              )}
            </div>
          )}
        </div>
      </div>
      <VersionFooter />
      {shuttingDown ? <ShutdownOverlay /> : null}
    </>
  );
}

// OnboardErrorBanner — a dismissible alert for the lifted onboarding partial-write failure. It
// renders above the wizard/dashboard so the message survives the wizard's poll-driven unmount.
// Styled with the same red tokens as the wizard's inline alert (the design-system idiom).
function OnboardErrorBanner({ message, onDismiss }: { message: string; onDismiss: () => void }) {
  return (
    <div
      role="alert"
      style={{
        display: "flex",
        alignItems: "flex-start",
        gap: 12,
        fontSize: 12.5,
        color: "var(--red)",
        background: "var(--red-soft)",
        border: "1px solid rgba(239,83,80,.3)",
        borderRadius: "var(--r-ctrl)",
        padding: "9px 12px",
        marginBottom: 18,
      }}
    >
      <span style={{ flex: 1, lineHeight: 1.5 }}>{message}</span>
      <button
        type="button"
        aria-label="Dismiss"
        onClick={onDismiss}
        style={{
          flexShrink: 0,
          background: "transparent",
          border: "none",
          color: "var(--red)",
          cursor: "pointer",
          fontSize: 14,
          lineHeight: 1,
          padding: 0,
        }}
      >
        ✕
      </button>
    </div>
  );
}

// ShutdownOverlay — a full-window "Shutting down…" screen shown while the daemon stops on quit, so
// the app reads as deliberately closing rather than frozen (the stop runs off the main thread).
function ShutdownOverlay() {
  return (
    <div
      style={{
        position: "fixed",
        inset: 0,
        zIndex: 1000,
        background: "var(--bg-app)",
        display: "flex",
        flexDirection: "column",
        alignItems: "center",
        justifyContent: "center",
        gap: 12,
      }}
    >
      <StatusDot color="var(--amber)" size={9} pulse />
      <div style={{ fontSize: 15, fontWeight: 600, color: "var(--tx)" }}>Shutting down…</div>
      <div style={{ fontSize: 12.5, color: "var(--tx-3)" }}>Stopping the daemon and finishing in-flight work.</div>
    </div>
  );
}

// VersionFooter — a dim build stamp pinned to the bottom of the window so it's always clear which
// build is running (release version + git SHA + build time on hover). Renders nothing in a plain
// browser, where the Wails bridge (and thus the stamp) is absent.
function VersionFooter() {
  const [v, setV] = React.useState<VersionDTO | null>(null);
  React.useEffect(() => {
    void appVersion().then(setV);
  }, []);
  if (!v) return null;
  const label = !v.version || v.version === "dev" ? "dev" : `v${v.version}`;
  const commit = v.commit && v.commit !== "none" ? ` · ${v.commit}` : "";
  return (
    <div
      className="mono"
      title={v.build_time && v.build_time !== "unknown" ? `built ${v.build_time}` : undefined}
      style={{
        flexShrink: 0,
        padding: "5px 16px",
        fontSize: 11,
        textAlign: "right",
        color: "var(--tx-faint)",
        borderTop: "1px solid var(--line-2)",
        background: "var(--bg-app)",
      }}
    >
      Rhapsody {label}
      {commit}
    </div>
  );
}
