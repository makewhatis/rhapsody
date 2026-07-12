import { useCallback, useEffect, useState } from "react";
import { credentialStatus, setLinearToken, writeInitialConfig } from "./bindings";
import { onboardingStep, slugValid } from "./wizard";
import { tokenLooksValid } from "./creds";

// Onboarding is the first-launch wizard (design §6): connect Linear (paste a token) → pick a
// project (seed WORKFLOW.md) → start the daemon. The Tool-doctor is reachable from the header
// "Tools" button. Once the config is written the app reports configured and this view yields to
// the dashboard. Ported from $REF/desktop/frontend/src/Onboarding.tsx.
export function Onboarding({ onConfigured, onError }: { onConfigured: () => void; onError?: (msg: string) => void }) {
  const [hasToken, setHasToken] = useState(false);
  const [token, setToken] = useState("");
  const [slug, setSlug] = useState("");
  const [busy, setBusy] = useState(false);
  const [msg, setMsg] = useState("");

  const refresh = useCallback(async () => {
    const s = await credentialStatus();
    setHasToken(Boolean(s?.has_token));
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  const step = onboardingStep(hasToken);

  const saveToken = async () => {
    setBusy(true);
    setMsg("");
    try {
      await setLinearToken(token.trim());
      setToken("");
    } catch (e) {
      // Includes the partial-success case (token persisted, daemon couldn't restart).
      setMsg((e as Error).message);
    } finally {
      // Always re-read status so the step advances when the token IS persisted, even if the
      // daemon-restart leg returned a partial-success error.
      await refresh();
      setBusy(false);
    }
  };

  const create = async () => {
    setBusy(true);
    setMsg("");
    onError?.("");
    try {
      await writeInitialConfig(slug.trim());
    } catch (e) {
      // writeInitialConfig wrote WORKFLOW.md but couldn't (re)start the daemon. Lift the message
      // to the shell too: once configured flips true the wizard unmounts, so a local-only message
      // would flash and vanish before the user reads it.
      setMsg((e as Error).message);
      onError?.((e as Error).message);
    } finally {
      // Always re-read app status — writeInitialConfig may have written WORKFLOW.md even when it
      // returned an error, so `configured` can now be true. Refreshing promptly unmounts the wizard
      // (surfacing onboardErr) instead of leaving it on the create step where a second click would
      // hit the backend "already configured" guard.
      onConfigured();
      setBusy(false);
    }
  };

  return (
    <div className="placeholder onboarding">
      <h2>Welcome to Rhapsody</h2>
      <ol className="steps">
        <li className={step === "token" ? "active" : "done"}>1. Connect Linear</li>
        <li className={step === "project" ? "active" : ""}>2. Pick a project</li>
        <li>3. Start</li>
      </ol>

      {step === "token" ? (
        <div className="onboard-step">
          <label htmlFor="ob-token">Paste a Linear API token</label>
          <input
            id="ob-token"
            type="password"
            placeholder="lin_api_…"
            value={token}
            onChange={(e) => setToken(e.target.value)}
            autoComplete="off"
          />
          <button onClick={() => void saveToken()} disabled={busy || !tokenLooksValid(token)}>
            Save token
          </button>
        </div>
      ) : (
        <div className="onboard-step">
          <label htmlFor="ob-slug">Linear project slug</label>
          <input
            id="ob-slug"
            placeholder="my-project"
            value={slug}
            onChange={(e) => setSlug(e.target.value)}
          />
          <button onClick={() => void create()} disabled={busy || !slugValid(slug)}>
            Create config &amp; start
          </button>
        </div>
      )}

      {msg && (
        <div role="alert" className="cred-msg">
          {msg}
        </div>
      )}
    </div>
  );
}
