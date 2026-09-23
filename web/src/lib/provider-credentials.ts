// Desktop-only provider credential bridge (STUDIO-991, design §P10). Every wrapper guards on the
// Tauri bridge exactly as `bindings.ts` does, so the shared component works in a plain browser (the
// daemon's served dashboard, `vite dev`, a unit test) by degrading to a safe no-op — the browser
// dashboard deliberately has NO credential mutation surface, and must say so rather than pretend.
//
// The webview originates ONE immutable JS string per Connect/Replace. This module never stores it,
// echoes it, or puts it anywhere but the single `invoke` argument; the form clears its reference the
// instant the call is made (see `ProviderCredentialForm`). Rust owns everything after that.

import { invoke } from "@tauri-apps/api/core";
import { hasBridge } from "./bindings";

/** The four §2.5 mutations the desktop app exposes. */
export type ProviderOperation = "connect" | "replace" | "rebind" | "remove";

/** One provider's non-secret credential status. Mirrors the Rust `ProviderStatusDto`. */
export interface ProviderStatus {
  provider_id: string;
  display_name: string;
  endpoint: string;
  adapter: string;
  insecure_http: boolean;
  status: string;
  recovery: string | null;
  can_connect: boolean;
  can_replace: boolean;
  can_rebind: boolean;
  can_remove: boolean;
}

/** The non-secret result of a `prepare`: the normalized endpoint plus the opaque one-use nonce. */
export interface PreparedCommand {
  provider_id: string;
  operation: ProviderOperation;
  endpoint: string;
  insecure_http: boolean;
  nonce: string;
  expires_in_ms: number;
}

/** A committed mutation's result. `mutated` is false for `already_absent`. */
export interface MutationResult {
  provider_id: string;
  operation: ProviderOperation;
  mutated: boolean;
  status: string;
  sync: string;
}

/** A bounded connection-test verdict. Never carries a provider body or a credential. */
export interface TestConnectionResult {
  ok: boolean;
  code: string;
  message: string;
}

/** The typed failure shape the Rust commands return. */
export interface CommandFailure {
  code: string;
  message: string;
}

/** Type guard for a rejected command: Tauri rejects with the serialized `CommandFailure` object. */
export function isCommandFailure(error: unknown): error is CommandFailure {
  return (
    typeof error === "object" &&
    error !== null &&
    typeof (error as CommandFailure).code === "string" &&
    typeof (error as CommandFailure).message === "string"
  );
}

/** The closed failure code, or "unknown" for anything that is not a `CommandFailure`. */
export function failureCode(error: unknown): string {
  return isCommandFailure(error) ? error.code : "unknown";
}

/** The operator-facing message for a failure; never contains a credential. */
export function failureMessage(error: unknown): string {
  if (isCommandFailure(error)) return error.message;
  return "the credential action could not be completed";
}

/** True when this host can perform credential mutations (the desktop app, not a plain browser). */
export function hasProviderBridge(): boolean {
  return hasBridge();
}

export async function providerStatuses(): Promise<ProviderStatus[]> {
  if (!hasBridge()) return [];
  return (await invoke<ProviderStatus[]>("provider_statuses")) ?? [];
}

export async function providerStatus(providerId: string): Promise<ProviderStatus | null> {
  if (!hasBridge()) return null;
  return invoke<ProviderStatus>("provider_status", { providerId });
}

// prepare mints the one-use confirmation for `operation`. The webview never supplies a binding or a
// revision; Rust derives both from the current validated provider definition.
export async function providerPrepare(
  providerId: string,
  operation: ProviderOperation,
): Promise<PreparedCommand> {
  return invoke<PreparedCommand>("provider_prepare", { providerId, operation });
}

// commitConnect/commitReplace pass the entered key exactly once. The key lives only in this call's
// argument; nothing here retains it.
export async function providerConnect(
  providerId: string,
  nonce: string,
  secret: string,
): Promise<MutationResult> {
  return invoke<MutationResult>("provider_connect", { providerId, nonce, secret });
}

export async function providerReplace(
  providerId: string,
  nonce: string,
  secret: string,
): Promise<MutationResult> {
  return invoke<MutationResult>("provider_replace", { providerId, nonce, secret });
}

export async function providerRebind(providerId: string, nonce: string): Promise<MutationResult> {
  return invoke<MutationResult>("provider_rebind", { providerId, nonce });
}

export async function providerRemove(providerId: string, nonce: string): Promise<MutationResult> {
  return invoke<MutationResult>("provider_remove", { providerId, nonce });
}

export async function providerTestConnection(providerId: string): Promise<TestConnectionResult> {
  return invoke<TestConnectionResult>("provider_test_connection", { providerId });
}

// ------------------------------------------------------------------------------------------------
// Presentation helpers (pure, unit-tested without a DOM).
// ------------------------------------------------------------------------------------------------

/** The label displayed for a credential status. */
export function statusLabel(status: string): string {
  switch (status) {
    case "configured":
      return "Connected";
    case "absent":
      return "Not connected";
    case "denied_or_locked":
      return "Keychain locked";
    case "malformed":
      return "Unreadable credential";
    case "binding_mismatch":
      return "Endpoint changed";
    case "owner_unavailable":
      return "Desktop credential owner unavailable";
    case "owner_unauthorized":
      return "Desktop credential owner rejected the daemon";
    default:
      return status;
  }
}

/** Whether the status represents a usable credential. */
export function isConfigured(status: string): boolean {
  return status === "configured";
}

// The exact literal the browser dashboard shows where the desktop app would offer credential
// actions: the shared UI must never imply a browser could change a Keychain item.
export const DESKTOP_ONLY_MESSAGE =
  "Credential changes require the Rhapsody desktop app; this browser dashboard can only show status.";
