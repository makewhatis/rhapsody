// Typed wrappers over the Tauri command bridge (`invoke`) and the app's runtime events. This is the
// canonical `web/` app served as the top-level Tauri frontend (TRA-251): the app-side capabilities
// (daemon lifecycle, tool-doctor, keychain credentials, onboarding) are reached through Tauri
// `invoke(...)`, and the daemon's HTTP API is reached same-origin via the shell's apiproxy.
//
// Migrated off the dead Wails bridge (`window.go.main.App`): the Go bound methods are now
// `#[tauri::command]`s (desktop/src-tauri/src/main.rs) reached via `invoke`, and the Wails runtime
// events become Tauri `listen(...)` subscriptions. Every wrapper degrades to a safe no-op / null /
// empty value when the Tauri bridge is absent (a plain browser: the daemon's served dashboard, the
// vite dev server, or a unit test), so `tsc`, vitest, and a browser stay runnable standalone.

import { Channel, invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { LinearProject } from "@/lib/api";

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

// VersionDTO is the desktop app's build stamp (compiled in via build.rs env vars), shown in the footer.
export interface VersionDTO {
  version: string; // "dev" or a release version like "1.2.0"
  commit: string; // short git SHA (+ "-dirty"), or "none" unstamped
  build_time: string; // RFC3339 UTC, or "unknown"
}

// ToolResult mirrors the Rust toolcheck::ToolResult (Go toolcheck.Result): one external CLI's preflight status.
export interface ToolResult {
  name: string;
  path: string;
  found: boolean;
  healthy: boolean;
  version: string;
  detail: string;
}

// CredentialStatus mirrors the Rust CredentialStatusDto (Go CredentialStatusDTO): whether a Linear
// token is stored, in which backend, and whether the deferred OAuth path is available.
export interface CredentialStatus {
  has_token: boolean;
  backend: string;
  oauth_available: boolean;
}

// tauriAvailable reports whether the Tauri IPC bridge is present. It is absent when the bundle is
// loaded in a plain browser (the daemon's served dashboard, `vite dev` without the app, or a unit
// test), so callers degrade gracefully instead of throwing. Mirrors the reference's
// `window.go?.main?.App` guard, adapted to Tauri's injected globals.
function tauriAvailable(): boolean {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}

/** True when running inside the Tauri host (the IPC bridge is present). */
export function hasBridge(): boolean {
  return tauriAvailable();
}

export async function getStatus(): Promise<StatusDTO | null> {
  if (!tauriAvailable()) return null;
  return invoke<StatusDTO>("status");
}

// appVersion returns the compiled-in build stamp for the footer, or null in a plain browser (no
// bridge) — callers render nothing in that case.
export async function appVersion(): Promise<VersionDTO | null> {
  if (!tauriAvailable()) return null;
  return invoke<VersionDTO>("app_version");
}

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

export async function probeTools(): Promise<ToolResult[]> {
  if (!tauriAvailable()) return [];
  return invoke<ToolResult[]>("probe_tools");
}

export async function setToolOverride(name: string, path: string): Promise<void> {
  if (!tauriAvailable()) return;
  await invoke("set_tool_override", { name, path });
}

export async function credentialStatus(): Promise<CredentialStatus | null> {
  if (!tauriAvailable()) return null;
  return invoke<CredentialStatus>("credential_status");
}

export async function setLinearToken(token: string): Promise<void> {
  if (!tauriAvailable()) return;
  await invoke("set_linear_token", { token });
}

export async function clearLinearToken(): Promise<void> {
  if (!tauriAvailable()) return;
  await invoke("clear_linear_token");
}

// startLinearOAuth triggers the deferred "Connect Linear" flow; in v1 it rejects with a clear
// message (no client_id configured) which the UI surfaces.
export async function startLinearOAuth(): Promise<void> {
  if (!tauriAvailable()) return;
  await invoke("start_linear_oauth");
}

// writeInitialConfig is the onboarding wizard's final step: seed WORKFLOW.md for the chosen
// Linear project and start the daemon.
export async function writeInitialConfig(projectSlug: string): Promise<void> {
  if (!tauriAvailable()) return;
  await invoke("write_initial_config", { projectSlug });
}

// listLinearProjects lists the workspace's Linear projects for the onboarding picker, using the
// token the wizard just saved. Returns [] when the bridge is absent (plain browser / tests); a
// Linear/token failure REJECTS so the caller can surface an error with retry + "back to token".
export async function listLinearProjects(): Promise<LinearProject[]> {
  if (!tauriAvailable()) return [];
  return (await invoke<LinearProject[]>("list_linear_projects")) ?? [];
}

// pickDirectory opens a native folder chooser for a path field, returning the chosen absolute path.
// The Tauri shell does not expose a native picker command (parity with the reference, where the
// picker was an OPTIONAL Go binding absent in the current build), so this degrades to "" and the
// field keeps its manually-typed value — callers apply the result only when non-empty.
export async function pickDirectory(_title: string): Promise<string> {
  return "";
}

// pickFile opens a native file chooser for a tool's executable-path override, returning the chosen
// absolute file path. Degrades to "" for the same reason as pickDirectory (no native-picker command);
// callers apply the result only when non-empty, so the manual-entry field stays usable.
export async function pickFile(_title: string): Promise<string> {
  return "";
}

// installTool asks the supervisor to install/update a required CLI (Tools tab action). The shell has
// no installer command (parity with the reference, where it was OPTIONAL and absent in v1), so it is
// a no-op; the caller re-probes afterwards either way.
export async function installTool(_name: string): Promise<void> {
  return;
}

// openExternal opens a URL in the user's default browser (the embedded webview must not navigate
// away). Under the Tauri host it uses the `open_external` command (macOS `open`); in a plain browser
// it falls back to window.open.
export function openExternal(url: string): void {
  if (tauriAvailable()) void invoke("open_external", { url });
  else window.open(url, "_blank", "noopener");
}

// onShuttingDown subscribes to the app:shutting-down event the shell emits when the user quits, so
// the app can show a "Shutting down…" screen while the daemon stops off the main thread. Returns an
// unsubscribe; a no-op when the Tauri bridge is absent (plain browser / tests).
export function onShuttingDown(cb: () => void): () => void {
  if (!tauriAvailable()) return () => {};
  const pending = listen("app:shutting-down", () => cb());
  return () => void pending.then((un) => un());
}

// onNavigate subscribes to the tray's navigate event ("dashboard" | "settings"); returns an
// unsubscribe function. A no-op when the Tauri bridge is not present (e.g. plain browser).
export function onNavigate(cb: (view: string) => void): () => void {
  if (!tauriAvailable()) return () => {};
  const pending = listen<string>("tray:navigate", (e) => cb(e.payload));
  return () => void pending.then((un) => un());
}

// ---- P11-U1 in-app auto-update -------------------------------------------------------------------

// UpdateInfo mirrors the Rust update::UpdateInfo: the result of a check. When `available` is false the
// app is up to date and `version`/`notes` are empty. Signature verification is the host's built-in
// minisign check (tauri.conf.json pubkey) — a bad artifact fails download/install, never reaching here.
export interface UpdateInfo {
  available: boolean;
  version: string; // the announced version (empty when none)
  current_version: string; // the running app version
  notes: string; // release notes / changelog body (empty when none)
}

// UpdateDownloadProgress mirrors the Rust update::DownloadProgress event payload: cumulative bytes
// downloaded and (when the server reported a Content-Length) the total, so the UI can show a bar.
export interface UpdateDownloadProgress {
  downloaded: number;
  total: number | null; // null → indeterminate (no Content-Length)
}

// InstallReport mirrors the Rust update::InstallReport: the outcome of installUpdate. When the install
// proceeds the app relaunches, so `installed` is rarely observed true; the meaningful signal is
// `blocked_active_runs > 0` — the install was refused because runs are active and was deferred to the
// next graceful quit (a pending flag was persisted).
export interface InstallReport {
  installed: boolean;
  blocked_active_runs: number;
}

// checkForUpdate asks the host to check the release feed for a newer version, returning its metadata (or
// null in a plain browser with no bridge). Rejects on a network / manifest error so the caller can retry.
export async function checkForUpdate(): Promise<UpdateInfo | null> {
  if (!tauriAvailable()) return null;
  return invoke<UpdateInfo>("update_check");
}

// downloadUpdate downloads the checked update (emitting progress via onUpdateDownloadProgress) and caches
// the verified bytes for installUpdate. Requires a prior checkForUpdate (or the quiet launch check).
// A no-op in a plain browser; rejects on a download / signature error.
export async function downloadUpdate(): Promise<void> {
  if (!tauriAvailable()) return;
  await invoke("update_download");
}

// installUpdate installs the update and relaunches into it — UNLESS runs are active and `force` is false,
// in which case it refuses, persists a pending flag (install on next graceful quit), and returns the
// blocking run count in `blocked_active_runs`. On the allowed path the app relaunches, so this usually
// does not resolve. Returns null in a plain browser (no bridge).
export async function installUpdate(force = false): Promise<InstallReport | null> {
  if (!tauriAvailable()) return null;
  return invoke<InstallReport>("update_install", { force });
}

// activeRunCount returns how many runs the daemon is actively executing right now — the count the install
// guard consults, so the UI can warn "N runs active — installing will restart the app" before confirming.
// Returns 0 when the bridge is absent (plain browser / tests).
export async function activeRunCount(): Promise<number> {
  if (!tauriAvailable()) return 0;
  return (await invoke<number>("active_run_count")) ?? 0;
}

// onUpdateAvailable subscribes to the quiet on-launch check's `update:available` event so the UI can
// badge the update affordance without the user asking. Returns an unsubscribe; a no-op when no bridge.
export function onUpdateAvailable(cb: (info: UpdateInfo) => void): () => void {
  if (!tauriAvailable()) return () => {};
  const pending = listen<UpdateInfo>("update:available", (e) => cb(e.payload));
  return () => void pending.then((un) => un());
}

// onUpdateDownloadProgress subscribes to the `update:download-progress` event emitted per chunk during
// downloadUpdate, so the UI can render a progress bar. Returns an unsubscribe; a no-op when no bridge.
export function onUpdateDownloadProgress(cb: (p: UpdateDownloadProgress) => void): () => void {
  if (!tauriAvailable()) return () => {};
  const pending = listen<UpdateDownloadProgress>("update:download-progress", (e) => cb(e.payload));
  return () => void pending.then((un) => un());
}

// LogStreamMsg is one frame the desktop host forwards over the log-stream Channel (Rust
// logbridge::LogMsg, `kind`-tagged): `open`/`reconnecting` mirror EventSource's onopen/onerror for the
// status dot, while `epoch`/`line` carry the same two SSE frame kinds the browser path handles (the
// daemon's `event: epoch` and `data:` log-line JSON), so the hook's de-dup logic is shared across both.
export type LogStreamMsg =
  | { kind: "open" }
  | { kind: "reconnecting" }
  | { kind: "epoch"; epoch: string }
  | { kind: "line"; data: string };

// subscribeLogStream starts the packaged app's live log tail: the host connects to the daemon's SSE log
// stream and re-emits each frame over a Tauri IPC Channel (TRA-252) — the buffered custom-protocol proxy
// can't forward an infinite `text/event-stream`, so the Logs view uses this instead of EventSource under
// Tauri. Returns an unsubscribe that stops the host-side stream. A no-op returning a no-op when the
// bridge is absent (plain browser / tests), so callers fall back to EventSource.
export function subscribeLogStream(onMessage: (msg: LogStreamMsg) => void): () => void {
  if (!tauriAvailable()) return () => {};
  const channel = new Channel<LogStreamMsg>();
  channel.onmessage = onMessage;
  const started = invoke("start_log_stream", { channel });
  // Chain the stop on start's completion so the host can't process it before the stream is registered
  // (StrictMode mount→unmount→mount / IPC reordering), and target this exact stream by its channel id so
  // a rapid re-subscribe never aborts the wrong one. `catch` so a failed start still runs the (no-op) stop.
  return () =>
    void started
      .catch(() => undefined)
      .then(() => invoke("stop_log_stream", { streamId: channel.id }));
}
