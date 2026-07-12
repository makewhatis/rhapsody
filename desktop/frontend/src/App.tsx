import { useEffect, useState } from "react";
import { appVersion, getStatus, type StatusDTO, type VersionDTO } from "./bindings";
import { statusLabel, viewForStatus } from "./status";
import "./styles.css";

// The desktop window shell. Ported from $REF/desktop/frontend/src/App.tsx, reduced to the P7-D1
// surface: the two-layer UI is the status header + the daemon dashboard once healthy, with clear
// not-configured / starting / stopped / error placeholders otherwise. The daemon-control buttons
// (Start/Stop/Restart) arrive with the app lifecycle (D3); the Linear/Tools/Onboarding panels with
// settings (D4). Until the supervisor is wired (D2), getStatus reports state "stopped".
export default function App() {
  const [status, setStatus] = useState<StatusDTO | null>(null);
  const [ver, setVer] = useState<VersionDTO | null>(null);

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

  const view = viewForStatus(status);

  return (
    <div className="app">
      <header className="bar">
        <div className="brand">
          <span className={`dot ${view}`} />
          <strong>Rhapsody</strong>
          <span className="label">{statusLabel(status)}</span>
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
    starting: { title: "Starting symphonyd…", detail: "Waiting for the daemon to become healthy." },
    stopped: { title: "Daemon stopped", detail: "Press Start to launch symphonyd." },
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
