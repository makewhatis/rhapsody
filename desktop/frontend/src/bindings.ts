// Typed wrappers over the Tauri command bridge (`invoke`) and the shell's DTOs. Ported from
// $REF/desktop/frontend/src/bindings.ts (which wrapped the Wails-injected `window.go.main.App.*`),
// adapted to Tauri v2: the Go bound methods become #[tauri::command]s reached via `invoke`.
//
// D1 (P7) exposes only the read surface the window shell needs — status + the build stamp.
// Later chain tasks add the lifecycle commands (start/stop/restart — D3), credential/tool/onboarding
// bridges (D4), etc., mirroring the corresponding bound methods as they are ported.
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

// StatusDTO mirrors the Go StatusDTO ($REF/desktop/app.go): the supervisor state string, live
// dashboard URL, health, running-agent count, and whether a WORKFLOW.md exists to run.
export interface StatusDTO {
  state: string;
  pid: number;
  restarts: number;
  last_err: string;
  url: string;
  healthy: boolean;
  agent_count: number;
  configured: boolean;
}

// VersionDTO mirrors the Go VersionDTO ($REF/desktop/app.go): the compiled-in build stamp shown
// in the app footer.
export interface VersionDTO {
  version: string;
  commit: string;
  build_time: string;
}

// tauriAvailable reports whether the Tauri IPC bridge is present. It is absent when the bundle is
// loaded in a plain browser (e.g. `vite dev` without the app, or a unit test), so callers degrade
// gracefully instead of throwing. Mirrors the reference's `window.go?.main?.App` guard.
function tauriAvailable(): boolean {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}

// getStatus returns the current daemon status snapshot, or null when the Tauri bridge is absent
// (matching the reference, whose bridge accessor returned undefined outside the app).
export async function getStatus(): Promise<StatusDTO | null> {
  if (!tauriAvailable()) return null;
  return invoke<StatusDTO>("status");
}

// appVersion returns the compiled-in build stamp for the footer, or null when the bridge is absent.
export async function appVersion(): Promise<VersionDTO | null> {
  if (!tauriAvailable()) return null;
  return invoke<VersionDTO>("app_version");
}

// startDaemon/stopDaemon/restartDaemon drive the daemon lifecycle (shell buttons + tray). They map to
// the App's bound methods ($REF/desktop/app.go StartDaemon/StopDaemon/RestartDaemon); no-ops when the
// bridge is absent. start rejects when there is no WORKFLOW.md (the daemon would only fail startup).
export async function startDaemon(): Promise<void> {
  if (!tauriAvailable()) return;
  await invoke("start_daemon");
}

export async function stopDaemon(): Promise<void> {
  if (!tauriAvailable()) return;
  await invoke("stop_daemon");
}

export async function restartDaemon(): Promise<void> {
  if (!tauriAvailable()) return;
  await invoke("restart_daemon");
}

// onNavigate subscribes to the tray's navigate event ("dashboard" | "settings"); returns an
// unsubscribe. Mirrors the reference's `EventsOn("tray:navigate", …)`, adapted to Tauri's `listen`.
// Acting on "settings" (the Settings view) lands in P7-D4; D3 uses it to refresh on a tray action.
export function onNavigate(cb: (view: string) => void): () => void {
  if (!tauriAvailable()) return () => {};
  const pending = listen<string>("tray:navigate", (e) => cb(e.payload));
  return () => void pending.then((un) => un());
}

// onShuttingDown subscribes to the quit-drain event the app emits ($REF/desktop/app.go
// "app:shutting-down") so the shell can render the "Shutting down…" overlay while the daemon drains
// off the main thread. Returns an unsubscribe.
export function onShuttingDown(cb: () => void): () => void {
  if (!tauriAvailable()) return () => {};
  const pending = listen("app:shutting-down", () => cb());
  return () => void pending.then((un) => un());
}
