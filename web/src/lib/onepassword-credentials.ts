// Desktop-only 1Password service-account credential bridge (STUDIO-1584). The same rules as
// `provider-credentials.ts`: every wrapper guards on the Tauri bridge so the shared component works
// in a plain browser (the daemon's dashboard, `vite dev`, a unit test) by degrading to a safe no-op —
// the browser dashboard deliberately has NO credential mutation surface. The token lives only in the
// single `invoke` argument; nothing here stores or echoes it.

import { invoke } from "@tauri-apps/api/core";
import { hasBridge } from "./bindings";

/** The mutations the desktop app exposes for a 1Password credential. */
export type OnePasswordOperation = "connect" | "replace" | "remove";

/** One 1Password credential's non-secret status. Mirrors the Rust `OnePasswordStatusDto`. */
export interface OnePasswordStatus {
  id: string;
  /** `configured` | `unconfigured` | `unavailable`. */
  status: string;
  recovery: string | null;
  can_connect: boolean;
  can_replace: boolean;
  can_remove: boolean;
}

/** A committed mutation's result. `mutated` is false for an already-absent Remove. */
export interface OnePasswordMutation {
  id: string;
  operation: OnePasswordOperation;
  mutated: boolean;
  status: string;
}

/** True when this host can perform 1Password credential mutations (the desktop app, not a browser). */
export function hasOnePasswordBridge(): boolean {
  return hasBridge();
}

/** The distinct, non-empty `onepassword:` ids the config binds, in first-seen order. */
export function boundOnePasswordIds(projects: { onepassword?: string }[] | undefined): string[] {
  const seen = new Set<string>();
  const ids: string[] = [];
  for (const project of projects ?? []) {
    const id = project.onepassword;
    if (typeof id === "string" && id !== "" && !seen.has(id)) {
      seen.add(id);
      ids.push(id);
    }
  }
  return ids;
}

export async function onePasswordStatus(id: string): Promise<OnePasswordStatus> {
  return invoke<OnePasswordStatus>("onepassword_status", { id });
}

// The token is passed exactly once. The key lives only in this call's argument; nothing here retains it.
export async function onePasswordConnect(
  id: string,
  token: string,
): Promise<OnePasswordMutation> {
  return invoke<OnePasswordMutation>("onepassword_connect", { id, token });
}

export async function onePasswordReplace(
  id: string,
  token: string,
): Promise<OnePasswordMutation> {
  return invoke<OnePasswordMutation>("onepassword_replace", { id, token });
}

export async function onePasswordRemove(id: string): Promise<OnePasswordMutation> {
  return invoke<OnePasswordMutation>("onepassword_remove", { id });
}
