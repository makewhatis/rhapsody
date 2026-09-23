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
  type PreparedCommand,
  type ProviderStatus,
} from "@/lib/provider-credentials";

export interface ProviderCredentialFormProps {
  provider: ProviderStatus;
  /** Called after any committed mutation or a fresh status, so the parent can refetch statuses. */
  onChanged?: (message: string) => void;
}

type Editing = "none" | "connect" | "replace" | "rebind";

export function ProviderCredentialForm({ provider, onChanged }: ProviderCredentialFormProps) {
  const bridged = hasProviderBridge();
  const [editing, setEditing] = useState<Editing>("none");
  const [key, setKey] = useState("");
  const [prepared, setPrepared] = useState<PreparedCommand | null>(null);
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
      const preparedCommand = await providerPrepare(provider.provider_id, operation);
      const result =
        operation === "connect"
          ? await providerConnect(provider.provider_id, preparedCommand.nonce, secret)
          : await providerReplace(provider.provider_id, preparedCommand.nonce, secret);
      setEditing("none");
      report(syncNotice(result.mutated, result.sync));
    } catch (e) {
      setError(failureMessage(e));
    } finally {
      setBusy(false);
    }
  };

  // beginRebind mints the one-use confirmation WITHOUT committing. The returned non-secret endpoint
  // is the current canonical destination the confirmation must display before the operator agrees.
  const beginRebind = async () => {
    setError(null);
    setNotice(null);
    setBusy(true);
    try {
      const preparedCommand = await providerPrepare(provider.provider_id, "rebind");
      setPrepared(preparedCommand);
      setEditing("rebind");
    } catch (e) {
      setError(failureMessage(e));
    } finally {
      setBusy(false);
    }
  };

  const confirmRebind = async () => {
    if (!prepared) return;
    // The nonce is one-use: drop the local reference before the call so a second click cannot reuse it.
    const nonce = prepared.nonce;
    setPrepared(null);
    setEditing("none");
    setError(null);
    setNotice(null);
    setBusy(true);
    try {
      const result = await providerRebind(provider.provider_id, nonce);
      report(syncNotice(result.mutated, result.sync));
    } catch (e) {
      setError(failureMessage(e));
    } finally {
      setBusy(false);
    }
  };

  const remove = async () => {
    setError(null);
    setNotice(null);
    setBusy(true);
    try {
      const preparedCommand = await providerPrepare(provider.provider_id, "remove");
      const result = await providerRemove(provider.provider_id, preparedCommand.nonce);
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
            <button type="button" disabled={busy} onClick={() => void beginRebind()}>
              Rebind to this endpoint
            </button>
          )}
          {provider.can_remove && (
            <button type="button" disabled={busy} onClick={() => void remove()}>
              Remove
            </button>
          )}
          <button type="button" disabled={busy} onClick={() => void test()}>
            Test connection
          </button>
        </div>
      )}

      {bridged && (editing === "connect" || editing === "replace") && (
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

      {bridged && editing === "rebind" && prepared && (
        // The explicit Rebind confirmation: the operator must see the EXACT canonical destination the
        // stored credential will be moved to, and an extra plaintext-HTTP warning when the new
        // endpoint is not TLS. Rebind changes only the binding; it never replaces the stored key.
        <div
          data-testid="rebind-confirmation"
          className="mt-3 rounded-md border border-amber-700/60 p-3"
        >
          <p className="text-xs text-neutral-200">
            Rebind this credential to the new canonical destination? The stored key is kept; only the
            endpoint binding changes.
          </p>
          <p className="mono mt-1 break-all text-xs text-neutral-300">{prepared.endpoint}</p>
          {prepared.insecure_http && (
            <p className="mt-1 text-xs text-amber-400">
              This destination uses plaintext HTTP — the credential will be sent without transport
              encryption.
            </p>
          )}
          <div className="mt-2 flex items-center gap-2">
            <button type="button" disabled={busy} onClick={() => void confirmRebind()}>
              Confirm rebind
            </button>
            <button
              type="button"
              disabled={busy}
              onClick={() => {
                setPrepared(null);
                setEditing("none");
              }}
            >
              Cancel
            </button>
          </div>
        </div>
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
