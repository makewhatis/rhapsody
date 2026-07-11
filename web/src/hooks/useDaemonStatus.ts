import { useCallback, useEffect, useRef, useState } from "react";
import { getStatus, startDaemon, stopDaemon, restartDaemon, type StatusDTO } from "@/lib/bindings";

export interface DaemonControls {
  status: StatusDTO | null;
  /** A lifecycle action (start/stop/restart) is in flight. */
  busy: boolean;
  start: () => Promise<void>;
  stop: () => Promise<void>;
  restart: () => Promise<void>;
  refresh: () => Promise<void>;
}

// useDaemonStatus polls the Wails supervisor status so the shell reflects start / health /
// stop transitions live, and exposes the lifecycle actions. Browser-safe: when the Wails
// bridge is absent (dev server, demo route, tests) getStatus() resolves null and the actions
// are no-ops, so the shell renders without a daemon.
export function useDaemonStatus(pollMs = 2000): DaemonControls {
  const [status, setStatus] = useState<StatusDTO | null>(null);
  const [busy, setBusy] = useState(false);
  const activeRef = useRef(true);

  const refresh = useCallback(async () => {
    try {
      const s = await getStatus();
      if (activeRef.current) setStatus(s);
    } catch {
      /* transient bridge error; keep the last status */
    }
  }, []);

  useEffect(() => {
    activeRef.current = true;
    void refresh();
    const id = window.setInterval(() => void refresh(), pollMs);
    return () => {
      activeRef.current = false;
      window.clearInterval(id);
    };
  }, [refresh, pollMs]);

  const run = useCallback(
    async (fn: () => Promise<void>) => {
      setBusy(true);
      try {
        await fn();
      } finally {
        if (activeRef.current) setBusy(false);
        await refresh();
      }
    },
    [refresh],
  );

  return {
    status,
    busy,
    start: () => run(startDaemon),
    stop: () => run(stopDaemon),
    restart: () => run(restartDaemon),
    refresh,
  };
}
