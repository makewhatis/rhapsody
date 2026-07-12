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
import "./styles.css";

// The desktop window shell. Ported from $REF/desktop/frontend/src/App.tsx, reduced to the P7-D3
// surface: the two-layer UI is the status header + the daemon dashboard once healthy, with clear
// not-configured / starting / stopped / error placeholders otherwise, plus the daemon-control buttons
// (Start/Stop/Restart) and the quit "Shutting down…" overlay. The Linear/Tools/Onboarding panels land
// with settings (D4).
export default function App() {
  const [status, setStatus] = useState<StatusDTO | null>(null);
  const [ver, setVer] = useState<VersionDTO | null>(null);
  const [busy, setBusy] = useState(false);
  const [shuttingDown, setShuttingDown] = useState(false);

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

  // A tray "Open"/"Settings" click refreshes the status so a tray-driven start/stop shows at once (the
  // Settings view itself lands in D4). Mirrors $REF App.tsx's onNavigate.
  useEffect(() => onNavigate(() => void getStatus().then(setStatus)), []);

  // Quit shows a "Shutting down…" overlay while the daemon drains off the main thread ($REF app.go
  // emits "app:shutting-down"); once shown it stays until the app exits.
  useEffect(() => onShuttingDown(() => setShuttingDown(true)), []);

  const view = viewForStatus(status);

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
        {view === "dashboard" && status ? (
          <iframe className="dashboard" src={status.url} title="Rhapsody dashboard" />
        ) : (
          <Placeholder view={view} status={status} />
        )}
      </main>

      <footer className="foot">
        {ver ? `Rhapsody ${ver.version} (${ver.commit})` : ""}
      </footer>

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

function Placeholder({ view, status }: { view: string; status: StatusDTO | null }) {
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
    </div>
  );
}
