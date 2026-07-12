import { useEffect, useState } from "react";
import {
  appVersion,
  getStatus,
  onNavigate,
  onShuttingDown,
  restartDaemon,
  startDaemon,
  stopDaemon,
  type StatusDTO,
  type VersionDTO,
} from "./bindings";
import { statusLabel, viewForStatus } from "./status";
import { Credential } from "./Credential";
import { Onboarding } from "./Onboarding";
import { ToolDoctor } from "./ToolDoctor";
import "./styles.css";

// The desktop window shell. Ported from $REF/desktop/frontend/src/App.tsx: the two-layer UI is the
// status header + the daemon dashboard once healthy, the first-launch Onboarding wizard in the
// not-configured state, and the Linear-credential + Tool-doctor settings panels reachable from the
// header (P7-D4). Keeps the D3 quit "Shutting down…" overlay + the build-stamp footer.
export default function App() {
  const [status, setStatus] = useState<StatusDTO | null>(null);
  const [ver, setVer] = useState<VersionDTO | null>(null);
  const [busy, setBusy] = useState(false);
  const [shuttingDown, setShuttingDown] = useState(false);
  const [showTools, setShowTools] = useState(false);
  const [showCred, setShowCred] = useState(false);
  // onboardErr persists an onboarding "config written but daemon couldn't start" message: once the
  // config is written the wizard unmounts (configured flips true), so the message is lifted here to
  // survive into the stopped/error placeholder. Cleared once the daemon is up.
  const [onboardErr, setOnboardErr] = useState("");

  // Poll the daemon status so the shell reflects start/health/stop transitions live (2s, per $REF).
  useEffect(() => {
    let active = true;
    const tick = async () => {
      try {
        const s = await getStatus();
        if (active) setStatus(s);
      } catch {
        /* transient bridge error; keep last status */
      }
    };
    void tick();
    const id = window.setInterval(tick, 2000);
    return () => {
      active = false;
      window.clearInterval(id);
    };
  }, []);

  // The build stamp is compiled in and static; fetch it once for the footer.
  useEffect(() => {
    void appVersion().then(setVer);
  }, []);

  // A tray "Open"/"Settings" click refreshes the status so a tray-driven start/stop shows at once.
  // Mirrors $REF App.tsx's onNavigate (which likewise just refreshes; the settings panels open from
  // the header buttons).
  useEffect(() => onNavigate(() => void getStatus().then(setStatus)), []);

  // Quit shows a "Shutting down…" overlay while the daemon drains off the main thread ($REF app.go
  // emits "app:shutting-down"); once shown it stays until the app exits.
  useEffect(() => onShuttingDown(() => setShuttingDown(true)), []);

  const view = viewForStatus(status);

  // Once the daemon is healthy, a prior onboarding start-failure message is stale — drop it.
  useEffect(() => {
    if (view === "dashboard") setOnboardErr("");
  }, [view]);

  // Run a daemon action, then refresh the status so the buttons/label reflect the new state.
  const action = async (fn: () => Promise<void>) => {
    setBusy(true);
    try {
      await fn();
    } finally {
      setBusy(false);
      setStatus(await getStatus());
    }
  };

  return (
    <div className="app">
      <header className="bar">
        <div className="brand">
          <span className={`dot ${view}`} />
          <strong>Rhapsody</strong>
          <span className="label">{statusLabel(status)}</span>
        </div>
        <div className="actions">
          <button
            onClick={() => {
              setShowCred((v) => !v);
              setShowTools(false);
            }}
          >
            {showCred ? "Hide Linear" : "Linear"}
          </button>
          <button
            onClick={() => {
              setShowTools((v) => !v);
              setShowCred(false);
            }}
          >
            {showTools ? "Hide tools" : "Tools"}
          </button>
          <button
            disabled={busy || (view !== "stopped" && view !== "error")}
            onClick={() => void action(startDaemon)}
          >
            Start
          </button>
          <button
            disabled={busy || view === "stopped" || view === "not-configured"}
            onClick={() => void action(stopDaemon)}
          >
            Stop
          </button>
          <button disabled={busy || view === "not-configured"} onClick={() => void action(restartDaemon)}>
            Restart
          </button>
        </div>
      </header>

      <main className="content">
        {showCred ? (
          <Credential onClose={() => setShowCred(false)} />
        ) : showTools ? (
          <ToolDoctor onClose={() => setShowTools(false)} />
        ) : view === "dashboard" && status ? (
          <iframe className="dashboard" src={status.url} title="Rhapsody dashboard" />
        ) : view === "not-configured" ? (
          <Onboarding onConfigured={() => void getStatus().then(setStatus)} onError={setOnboardErr} />
        ) : (
          <Placeholder view={view} status={status} onboardErr={onboardErr} />
        )}
      </main>

      <footer className="foot">{ver ? `Rhapsody ${ver.version} (${ver.commit})` : ""}</footer>

      {shuttingDown && (
        <div className="overlay" role="alert">
          <div className="overlay-card">
            <h2>Shutting down…</h2>
            <p>Stopping rhapsodyd and any running agents.</p>
          </div>
        </div>
      )}
    </div>
  );
}

function Placeholder({
  view,
  status,
  onboardErr,
}: {
  view: string;
  status: StatusDTO | null;
  onboardErr?: string;
}) {
  const body: Record<string, { title: string; detail: string }> = {
    loading: { title: "Loading…", detail: "Connecting to the supervisor." },
    "not-configured": {
      title: "Not configured yet",
      detail:
        "Connect Linear and choose a project to create your WORKFLOW.md. Until then the daemon will not start.",
    },
    starting: { title: "Starting rhapsodyd…", detail: "Waiting for the daemon to become healthy." },
    stopped: { title: "Daemon stopped", detail: "Press Start to launch rhapsodyd." },
    error: {
      title: "Daemon stopped after an error",
      detail: status?.last_err || "The daemon exited unexpectedly. Check the logs and try Start.",
    },
  };
  const b = body[view] ?? body.loading;
  return (
    <div className="placeholder">
      <h2>{b.title}</h2>
      <p>{b.detail}</p>
      {/* Persisted onboarding start-failure: config was written but the daemon couldn't start, so the
          wizard unmounted before its message could be read — surface it here too. */}
      {onboardErr && (
        <p role="alert" className="cred-msg">
          {onboardErr}
        </p>
      )}
    </div>
  );
}
