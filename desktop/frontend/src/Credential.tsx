import { useCallback, useEffect, useState } from "react";
import {
  clearLinearToken,
  credentialStatus,
  setLinearToken,
  startLinearOAuth,
  type CredentialStatus,
} from "./bindings";
import { credentialSummary, tokenLooksValid } from "./creds";

// Credential is the Linear credential panel (spec §7): paste a token (stored in the Keychain,
// fed to the daemon) — the working v1 path — plus a "Connect Linear" OAuth button whose flow is
// deferred (it surfaces a clear message until a client_id is configured). Ported from
// $REF/desktop/frontend/src/Credential.tsx.
export function Credential({ onClose }: { onClose: () => void }) {
  const [status, setStatus] = useState<CredentialStatus | null>(null);
  const [token, setToken] = useState("");
  const [msg, setMsg] = useState("");
  const [busy, setBusy] = useState(false);

  const refresh = useCallback(async () => {
    setStatus(await credentialStatus());
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  const save = async () => {
    setBusy(true);
    setMsg("");
    try {
      await setLinearToken(token.trim());
      setToken("");
      setMsg("Saved — the daemon will use it.");
    } catch (e) {
      // Includes the partial-success case (token persisted, but the daemon couldn't restart).
      setMsg((e as Error).message);
    } finally {
      // Always re-read status so the header reflects the PERSISTED state — otherwise a partial
      // success would leave it showing "No Linear token set" while the message says it was saved.
      await refresh();
      setBusy(false);
    }
  };

  const clear = async () => {
    setBusy(true);
    setMsg("");
    try {
      await clearLinearToken();
    } catch (e) {
      setMsg((e as Error).message);
    } finally {
      await refresh();
      setBusy(false);
    }
  };

  const connect = async () => {
    setMsg("");
    try {
      await startLinearOAuth();
    } catch (e) {
      setMsg((e as Error).message);
    }
  };

  return (
    <div className="cred">
      <div className="bar">
        <strong>Linear credential</strong>
        <span className="label">{credentialSummary(status)}</span>
        <div className="actions">
          <button onClick={onClose}>Close</button>
        </div>
      </div>
      <div className="cred-body">
        <label htmlFor="linear-token">Paste a Linear API token</label>
        <input
          id="linear-token"
          type="password"
          placeholder="lin_api_…"
          value={token}
          onChange={(e) => setToken(e.target.value)}
          autoComplete="off"
        />
        <div className="actions">
          <button onClick={() => void save()} disabled={busy || !tokenLooksValid(token)}>
            Save token
          </button>
          {status?.has_token && status.backend !== "env" && (
            // No Remove for a dev-only $LINEAR_API_KEY ("env" backend): clearing the Keychain/file
            // store would be a no-op against the environment variable.
            <button onClick={() => void clear()} disabled={busy}>
              Remove
            </button>
          )}
        </div>
        <hr />
        <button
          className="oauth"
          onClick={() => void connect()}
          title={status?.oauth_available ? "Start the Linear OAuth flow" : "Deferred until a Linear OAuth app exists"}
        >
          Connect Linear (OAuth)
        </button>
        {!status?.oauth_available && (
          <p className="hint">OAuth is coming later — for now, paste a token above.</p>
        )}
        {msg && (
          <div role="alert" className="cred-msg">
            {msg}
          </div>
        )}
      </div>
    </div>
  );
}
