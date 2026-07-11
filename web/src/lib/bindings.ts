// Typed wrappers over the Wails-injected bridge (window.go.main.App.*) and runtime events.
// Moved into the canonical `web/` app (INF-225) from the desktop shell: the Wails app now
// hosts this whole React UI, so the app-side capabilities (daemon lifecycle, tool-doctor,
// keychain credentials) are reached here through the bridge.
//
// Calling through the injected globals (rather than importing the generated wailsjs/
// bindings, which only exist after a `wails build`) keeps `tsc`, vitest, and a plain
// browser (dev server / demo route) runnable standalone — every wrapper degrades to a safe
// no-op / null when `window.go` is absent.

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

// VersionDTO is the desktop app's build stamp (compiled in via -ldflags), shown in the footer.
export interface VersionDTO {
  version: string; // "dev" or a release version like "1.2.0"
  commit: string; // short git SHA (+ "-dirty"), or "none" unstamped
  build_time: string; // RFC3339 UTC, or "unknown"
}

// ToolResult mirrors the Go toolcheck.Result: one external CLI's preflight status.
export interface ToolResult {
  name: string;
  path: string;
  found: boolean;
  healthy: boolean;
  version: string;
  detail: string;
}

// CredentialStatus mirrors the Go CredentialStatusDTO: whether a Linear token is stored, in
// which backend, and whether the deferred OAuth path is available.
export interface CredentialStatus {
  has_token: boolean;
  backend: string;
  oauth_available: boolean;
}

interface AppBridge {
  Status(): Promise<StatusDTO>;
  AppVersion(): Promise<VersionDTO>;
  StartDaemon(): Promise<void>;
  StopDaemon(): Promise<void>;
  RestartDaemon(): Promise<void>;
  DashboardURL(): Promise<string>;
  ProbeTools(): Promise<ToolResult[]>;
  SetToolOverride(name: string, path: string): Promise<void>;
  CredentialStatus(): Promise<CredentialStatus>;
  SetLinearToken(token: string): Promise<void>;
  ClearLinearToken(): Promise<void>;
  StartLinearOAuth(): Promise<void>;
  WriteInitialConfig(projectSlug: string): Promise<void>;
  // Lists the workspace's Linear projects for the onboarding picker, using the just-saved token.
  // Calls Linear directly (no running daemon yet, INF-277). Rejects on a Linear/token error so the
  // wizard can show a retry + "back to token" affordance.
  ListLinearProjects(): Promise<LinearProject[]>;
  // Optional: native folder chooser for the workspace-root / logs-path directory fields. May be
  // absent in the current Go build, in which case the picker degrades to a no-op (see pickDirectory).
  PickDirectory?(title: string): Promise<string>;
  // Optional: native file chooser for a tool's executable path override (Tools tab). A CLI override
  // is a path to a binary FILE, not a directory — see pickFile.
  PickFile?(title: string): Promise<string>;
  // Optional: ask the supervisor to install/update a required CLI (Tools tab Install/Update
  // action). Absent in builds without an installer, where the action degrades to a no-op + re-probe.
  InstallTool?(name: string): Promise<void>;
}

interface WailsRuntime {
  EventsOn(event: string, cb: (...data: unknown[]) => void): () => void;
  // Window controls (Wails-injected). Optional so tsc / vitest / a plain browser tolerate their
  // absence.
  WindowToggleMaximise?(): void;
  // Open a URL in the user's default browser (not the embedded webview).
  BrowserOpenURL?(url: string): void;
}

declare global {
  interface Window {
    go?: { main?: { App?: AppBridge } };
    runtime?: WailsRuntime;
  }
}

function app(): AppBridge | undefined {
  return window.go?.main?.App;
}

/** True when running inside the Wails host (the Go bridge is present). */
export function hasBridge(): boolean {
  return !!app();
}

export async function getStatus(): Promise<StatusDTO | null> {
  const a = app();
  if (!a) return null;
  return a.Status();
}

// appVersion returns the compiled-in build stamp for the footer, or null in a plain browser (no
// bridge) — callers render nothing in that case.
export async function appVersion(): Promise<VersionDTO | null> {
  const a = app();
  if (!a) return null;
  return a.AppVersion();
}

export async function startDaemon(): Promise<void> {
  await app()?.StartDaemon();
}

export async function stopDaemon(): Promise<void> {
  await app()?.StopDaemon();
}

export async function restartDaemon(): Promise<void> {
  await app()?.RestartDaemon();
}

export async function dashboardURL(): Promise<string> {
  return (await app()?.DashboardURL()) ?? "";
}

export async function probeTools(): Promise<ToolResult[]> {
  return (await app()?.ProbeTools()) ?? [];
}

export async function setToolOverride(name: string, path: string): Promise<void> {
  await app()?.SetToolOverride(name, path);
}

export async function credentialStatus(): Promise<CredentialStatus | null> {
  const a = app();
  if (!a) return null;
  return a.CredentialStatus();
}

export async function setLinearToken(token: string): Promise<void> {
  await app()?.SetLinearToken(token);
}

export async function clearLinearToken(): Promise<void> {
  await app()?.ClearLinearToken();
}

// startLinearOAuth triggers the deferred "Connect Linear" flow; in v1 it rejects with a clear
// message (no client_id configured) which the UI surfaces.
export async function startLinearOAuth(): Promise<void> {
  await app()?.StartLinearOAuth();
}

// writeInitialConfig is the onboarding wizard's final step: seed WORKFLOW.md for the chosen
// Linear project and start the daemon.
export async function writeInitialConfig(projectSlug: string): Promise<void> {
  await app()?.WriteInitialConfig(projectSlug);
}

// listLinearProjects lists the workspace's Linear projects for the onboarding picker, using the
// token the wizard just saved. Returns [] when the bridge is absent (plain browser / tests); a
// Linear/token failure REJECTS so the caller can surface an error with retry + "back to token".
export async function listLinearProjects(): Promise<LinearProject[]> {
  const a = app();
  if (!a) return [];
  return (await a.ListLinearProjects()) ?? [];
}

// pickDirectory opens the native folder chooser (Go binding) for a path field, returning the
// chosen absolute path. Returns "" when the user cancels, when the bridge is absent (plain
// browser / tests), or when this build's Go side does not expose the picker — so callers apply
// the result only when non-empty and the field is otherwise unchanged.
export async function pickDirectory(title: string): Promise<string> {
  const a = app();
  if (!a?.PickDirectory) return "";
  try {
    return (await a.PickDirectory(title)) ?? "";
  } catch {
    return "";
  }
}

// pickFile opens the native file chooser (Go binding) for a tool's executable-path override,
// returning the chosen absolute file path. Returns "" on cancel / when the bridge is absent / when
// this build's Go side does not expose the picker (callers apply the result only when non-empty).
// A CLI path override must point at the binary itself, so this uses a FILE chooser, not a folder one.
export async function pickFile(title: string): Promise<string> {
  const a = app();
  if (!a?.PickFile) return "";
  try {
    return (await a.PickFile(title)) ?? "";
  } catch {
    return "";
  }
}

// installTool asks the supervisor to install/update a required CLI (Tools tab action). Resolves
// to a no-op when the bridge or installer is absent (the caller re-probes afterwards either way).
export async function installTool(name: string): Promise<void> {
  await app()?.InstallTool?.(name);
}

// openExternal opens a URL in the user's default browser. Under the Wails host it uses the runtime
// (the embedded webview must not navigate away); in a plain browser it falls back to window.open.
export function openExternal(url: string): void {
  if (window.runtime?.BrowserOpenURL) window.runtime.BrowserOpenURL(url);
  else window.open(url, "_blank", "noopener");
}

// toggleMaximiseWindow zooms / unzooms the window. Wired to a double-click on the titlebar so the
// app matches the standard macOS "double-click the title bar to zoom" behaviour (the custom drag
// region otherwise swallows it). A no-op when the Wails runtime is absent (plain browser / tests).
export function toggleMaximiseWindow(): void {
  window.runtime?.WindowToggleMaximise?.();
}

// onShuttingDown subscribes to the app:shutting-down event the Go side emits when the user quits,
// so the shell can show a "Shutting down…" screen while the daemon stops off the main thread.
// Returns an unsubscribe; a no-op when the Wails runtime is absent (plain browser / tests).
export function onShuttingDown(cb: () => void): () => void {
  const rt = window.runtime;
  if (!rt) return () => {};
  return rt.EventsOn("app:shutting-down", () => cb());
}

// onNavigate subscribes to the tray's navigate event ("dashboard" | "settings"); returns an
// unsubscribe function. A no-op when the Wails runtime is not present (e.g. plain browser).
export function onNavigate(cb: (view: string) => void): () => void {
  const rt = window.runtime;
  if (!rt) return () => {};
  return rt.EventsOn("tray:navigate", (...data: unknown[]) => cb(String(data[0] ?? "")));
}
