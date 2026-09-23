// The desktop-only Connect / Replace / Rebind / Remove / Test Connection form (STUDIO-991, §P10).
//
// This is the ONLY place the webview originates a provider credential. It holds the entered string
// in React state just long enough to submit it, then clears the field BEFORE the async command even
// resolves, and never writes it to state, history, localStorage, sessionStorage, or a log. The
// honest guarantee is exactly that: an immutable JS string cannot be proven zeroized, so the form
// clears every reference it owns and relies on the restricted bundled origin plus no persistence —
// it does not claim heap erasure. Rust owns validation, zeroization, and everything after the call.
//
// In a plain browser (the daemon's served dashboard, `vite dev`) there is no bridge, so the form
// renders the shared message that credential changes require the desktop app rather than pretending.

import { useState } from "react";
import {
  DESKTOP_ONLY_MESSAGE,
  failureMessage,
  hasProviderBridge,
  providerConnect,
  providerPrepare,
  providerRebind,
  providerRemove,
  providerReplace,
  providerTestConnection,
  statusLabel,
  type ProviderStatus,
} from "@/lib/provider-credentials";

export interface ProviderCredentialFormProps {
  provider: ProviderStatus;
  /** Called after any committed mutation or a fresh status, so the parent can refetch statuses. */
  onChanged?: (message: string) => void;
}

type Editing = "none" | "connect" | "replace";

export function ProviderCredentialForm({ provider, onChanged }: ProviderCredentialFormProps) {
  const bridged = hasProviderBridge();
  const [editing, setEditing] = useState<Editing>("none");
  const [key, setKey] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  const clearSensitive = () => {
    setKey("");
  };

  const report = (message: string) => {
    setNotice(message);
    onChanged?.(message);
  };

  // submitKey performs the prepare → commit pair for Connect/Replace. The entered string is read
  // into `secret`, the field is cleared IMMEDIATELY, and `secret` is passed straight to the one
  // commit call — it is never held in component state again.
  const submitKey = async (operation: "connect" | "replace") => {
    const secret = key;
    clearSensitive();
    setError(null);
    setNotice(null);
    if (!secret) {
      setError("Enter a credential first.");
      return;
    }
    setBusy(true);
    try {
      const prepared = await providerPrepare(provider.provider_id, operation);
      const result =
        operation === "connect"
          ? await providerConnect(provider.provider_id, prepared.nonce, secret)
          : await providerReplace(provider.provider_id, prepared.nonce, secret);
      setEditing("none");
      report(syncNotice(result.mutated, result.sync));
    } catch (e) {
      setError(failureMessage(e));
    } finally {
      setBusy(false);
    }
  };

  const mutate = async (operation: "rebind" | "remove") => {
    setError(null);
    setNotice(null);
    setBusy(true);
    try {
      const prepared = await providerPrepare(provider.provider_id, operation);
      const result =
        operation === "rebind"
          ? await providerRebind(provider.provider_id, prepared.nonce)
          : await providerRemove(provider.provider_id, prepared.nonce);
      report(syncNotice(result.mutated, result.sync));
    } catch (e) {
      setError(failureMessage(e));
    } finally {
      setBusy(false);
    }
  };

  const test = async () => {
    setError(null);
    setNotice(null);
    setBusy(true);
    try {
      const verdict = await providerTestConnection(provider.provider_id);
      report(verdict.ok ? "Connection succeeded." : `Connection failed: ${verdict.message}`);
    } catch (e) {
      setError(failureMessage(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="rounded-md border border-neutral-800 p-4" data-testid="provider-credential-form">
      <div className="flex items-baseline justify-between gap-4">
        <div>
          <div className="text-sm font-medium text-neutral-100">
            {provider.display_name || provider.provider_id}
          </div>
          <div className="font-mono text-xs text-neutral-400">{provider.endpoint}</div>
        </div>
        <div className="text-xs text-neutral-300">{statusLabel(provider.status)}</div>
      </div>

      {provider.insecure_http && (
        <p className="mt-2 text-xs text-amber-400">
          This endpoint uses plaintext HTTP. The credential will be sent without transport
          encryption.
        </p>
      )}

      {!bridged && <p className="mt-3 text-xs text-neutral-400">{DESKTOP_ONLY_MESSAGE}</p>}

      {bridged && (
        <div className="mt-3 flex flex-wrap items-center gap-2">
          {provider.can_connect && (
            <button type="button" disabled={busy} onClick={() => setEditing("connect")}>
              Connect
            </button>
          )}
          {provider.can_replace && (
            <button type="button" disabled={busy} onClick={() => setEditing("replace")}>
              Replace key
            </button>
          )}
          {provider.can_rebind && (
            <button type="button" disabled={busy} onClick={() => void mutate("rebind")}>
              Rebind to this endpoint
            </button>
          )}
          {provider.can_remove && (
            <button type="button" disabled={busy} onClick={() => void mutate("remove")}>
              Remove
            </button>
          )}
          <button type="button" disabled={busy} onClick={() => void test()}>
            Test connection
          </button>
        </div>
      )}

      {bridged && editing !== "none" && (
        <form
          className="mt-3 flex items-center gap-2"
          onSubmit={(e) => {
            e.preventDefault();
            void submitKey(editing);
          }}
        >
          <input
            type="password"
            aria-label="Provider credential"
            autoComplete="off"
            value={key}
            onChange={(e) => setKey(e.target.value)}
          />
          <button type="submit" disabled={busy}>
            {editing === "connect" ? "Store credential" : "Replace credential"}
          </button>
          <button
            type="button"
            disabled={busy}
            onClick={() => {
              clearSensitive();
              setEditing("none");
            }}
          >
            Cancel
          </button>
        </form>
      )}

      {notice && <p className="mt-2 text-xs text-emerald-400">{notice}</p>}
      {error && <p className="mt-2 text-xs text-red-400">{error}</p>}
    </div>
  );
}

function syncNotice(mutated: boolean, sync: string): string {
  if (!mutated) return "No stored credential — nothing to change.";
  switch (sync) {
    case "synchronized":
      return "Stored and synchronized with the running daemon.";
    case "stored_unsynchronized":
      return "Stored; the running daemon has not observed it yet.";
    case "stored_offline":
      return "Stored; no daemon is running, so it will be observed on next start.";
    default:
      return "Stored.";
  }
}
