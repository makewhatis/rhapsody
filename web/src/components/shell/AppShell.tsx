import * as React from "react";
import { SetupToolbar, Toolbar } from "@/components/Toolbar";
import { type SettingsTabId, type TopTabId, TOP_PANEL_ID } from "./placeholders";
import { Settings } from "@/components/settings/Settings";
import { RunsView } from "@/components/runs/RunsView";
import { Onboarding } from "@/components/onboarding/Onboarding";
import { ToastProvider, useToast } from "./Toast";
import { ShutdownOverlay } from "./ShutdownOverlay";
import { useDaemonStatus } from "@/hooks/useDaemonStatus";
import { useStateQuery } from "@/hooks/useStateQuery";
import { useUpdater } from "@/hooks/useUpdater";
import { useTeamsEnabled, useTeamsOverview, useVersionQuery } from "@/hooks/useTeams";
import { TeamsPanel } from "@/components/teams/TeamsPanel";
import { teamsChip } from "@/lib/teams-model";
import { conductorStatus, viewForStatus } from "@/lib/daemon-status";
import { appVersion, hasBridge, onNavigate, onShuttingDown, openExternal, type VersionDTO } from "@/lib/bindings";
import { stamp } from "@/lib/version-stamp";

// The centred, padded content container used by Settings + the onboarding wizard. The Jobs view
// (P10-D3) opts OUT of it to render its instrument strip + footer as full-bleed bands.
const CONTENT_PAD: React.CSSProperties = { maxWidth: 1180, margin: "0 auto", padding: "26px 40px 60px" };
// A lighter padded wrapper for the (rare) lifted onboarding-error banner shown above the full-bleed
// Jobs view, so it isn't flush against the window edge.
const CONTENT_PAD_TOP: React.CSSProperties = { maxWidth: 1180, margin: "0 auto", padding: "20px 40px 0" };
// The first-run wizard's narrow, centered column (mock 2e ~620px setup window, 30/34 content pad).
// It hosts both the lifted onboarding-error banner and the wizard so they share one centered column.
const WIZARD_WRAP: React.CSSProperties = { maxWidth: 560, margin: "0 auto", padding: "34px 34px 60px" };

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
  // The single in-app update model (P11-U3): the toolbar gear dot and the Settings "Updates" surface
  // share this one instance so a check/download in the panel and the gear badge never disagree.
  const updater = useUpdater();
  // Rhapsody Teams (STUDIO-652). THE gate is `teams_enabled` on GET /api/v1/version — the one
  // request the shell already makes at mount for the build stamp. While it is false the overview
  // query below is disabled, so a Teams-off app issues ZERO requests against /api/v1/teams*, shows
  // no chip, and is byte-for-byte the app it was before this ticket.
  const teamsEnabled = useTeamsEnabled();
  const teams = useTeamsOverview(teamsEnabled, data?.poll_interval_ms);
  const teamsChipModel = teamsChip(teams.data);
  // Run-detail selection is lifted out of RunsView so the Teams panel can open a teammate's live
  // run in the SAME detail view the Jobs list uses — one run detail, reached two ways.
  const [openRunId, setOpenRunId] = React.useState<number | null>(null);

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
      {notConfigured ? (
        // First run: no daemon yet, so the toolbar drops the conductor status + transport + gear and
        // shows only the wordmark and a "SETUP" marker (mock 2e).
        <SetupToolbar />
      ) : (
        <Toolbar
          conductor={conductor}
          running={daemonRunning}
          connecting={conductor.phase === "connecting"}
          busy={daemon.busy}
          settingsActive={topTab === "settings"}
          updateAvailable={updater.pending}
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
          teams={teamsChipModel}
          teamsActive={topTab === "teams"}
          onOpenTeams={() => setTopTab(topTab === "teams" ? "runs" : "teams")}
        />
      )}
      {/* Always reserve the vertical scrollbar's width so the centered content (max-width 1180,
          margin auto) never shifts left by a scrollbar width when a tab/run is tall enough to
          overflow — the size-dependent jitter between tabs (and Runs → Run detail). overflowY:
          "scroll" forces the track to always render (reliable for classic, space-taking scrollbars
          even where scrollbar-gutter is unsupported); scrollbarGutter: "stable" is the modern
          complement where the engine honors it. */}
      <div style={{ flex: 1, overflowY: "scroll", overflowX: "hidden", scrollbarGutter: "stable" }}>
        {notConfigured ? (
          // First run: no config yet. Show the wizard instead of the daemon-dependent header/nav
          // (a narrow, centred column). onConfigured re-reads status so the shell swaps to the
          // dashboard once the daemon starts. onError lifts a partial-write failure here so it
          // outlives the poll-driven unmount.
          <div style={WIZARD_WRAP}>
            {onboardErr ? <OnboardErrorBanner message={onboardErr} onDismiss={() => setOnboardErr("")} /> : null}
            <Onboarding onConfigured={() => void daemon.refresh()} onError={setOnboardErr} />
          </div>
        ) : (
          // Runs is the whole main area; the titlebar gear toggles Settings in over it. The panel
          // keeps role="tabpanel" + a label for the a11y tree even though the tab strip is gone. The
          // Jobs view renders full-bleed (its instrument strip + footer are edge-to-edge bands, mock
          // 1a); Settings stays in the centred, padded container.
          <div
            id={TOP_PANEL_ID}
            role="tabpanel"
            aria-label={topTab === "runs" ? "Runs" : topTab === "teams" ? "Teams" : "Settings"}
            tabIndex={0}
            style={topTab === "runs" ? { outline: "none" } : { outline: "none", ...CONTENT_PAD }}
          >
            {onboardErr ? (
              topTab === "runs" ? (
                <div style={CONTENT_PAD_TOP}>
                  <OnboardErrorBanner message={onboardErr} onDismiss={() => setOnboardErr("")} />
                </div>
              ) : (
                <OnboardErrorBanner message={onboardErr} onDismiss={() => setOnboardErr("")} />
              )
            ) : null}
            {topTab === "runs" ? (
              <RunsView openRunId={openRunId} onOpenRun={setOpenRunId} />
            ) : topTab === "teams" ? (
              <TeamsPanel
                pollMs={data?.poll_interval_ms}
                onOpenRun={(runID) => {
                  setOpenRunId(runID);
                  setTopTab("runs");
                }}
                onOpenSettings={() => {
                  setTopTab("settings");
                  setSettingsTab("teams");
                }}
              />
            ) : (
              <Settings tab={settingsTab} onTab={setSettingsTab} onBack={() => setTopTab("runs")} updater={updater} />
            )}
          </div>
        )}
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

// VersionFooter — a dim build stamp pinned to the bottom of the window so it's always clear which
// build is running (release version + git SHA + build time on hover).
//
// It reports TWO builds, because they are separate binaries that drift apart: the desktop shell
// (appVersion(), via the Tauri bridge) and the rhapsodyd daemon (GET /api/v1/version, STUDIO-380).
// The daemon's is the one that matters — it decides how runs are classified — and it was previously
// unreportable, which let a month-stale daemon keep answering `status: ok` indistinguishably from a
// current one. The daemon stamp is fetched over the same loopback API as everything else, so unlike
// the shell stamp it also renders in a plain browser.
function VersionFooter() {
  const [app, setApp] = React.useState<VersionDTO | null>(null);
  React.useEffect(() => {
    void appVersion().then(setApp);
  }, []);
  // The daemon stamp comes from the SHARED version query (STUDIO-652) rather than a second fetch of
  // its own, so the footer and the Teams gate cost exactly one request between them. A daemon that
  // is down or too old to serve the route simply leaves the stamp off; the footer is diagnostic
  // furniture and must never surface an error or white-screen the shell.
  const daemon = useVersionQuery().data ?? null;
  if (!app && !daemon) return null;

  const appLabel = app ? stamp(app.version, app.commit) : "";
  const daemonLabel = daemon ? stamp(daemon.version, daemon.commit) : "";
  // Collapse to one line in the common case where the shell and the daemon ship from the same build;
  // show both only when they actually disagree, which IS the drift worth noticing.
  const same = appLabel !== "" && appLabel === daemonLabel;
  // Both build times on hover. The shell's was the previous tooltip and stays available; the
  // daemon's is the one that dates a stale binary, which is the whole point of the stamp.
  const times = [
    app?.build_time && app.build_time !== "unknown" ? `app built ${app.build_time}` : "",
    daemon?.built_at && daemon.built_at !== "unknown" ? `daemon built ${daemon.built_at}` : "",
  ].filter(Boolean);
  const builtAt = times.length > 0 ? times.join(" · ") : undefined;

  return (
    <div
      className="mono"
      title={builtAt}
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
      {appLabel !== "" && <>Rhapsody {appLabel}</>}
      {!same && daemonLabel !== "" && (
        <>
          {appLabel !== "" && " · "}
          daemon {daemonLabel}
        </>
      )}
    </div>
  );
}
