import * as React from "react";
import { Button, Check, Key, Play, Select, StatusDot, TextInput } from "@/components/ui";
import type { LinearProject } from "@/lib/api";
import {
  clearLinearToken,
  credentialStatus,
  listLinearProjects,
  openExternal,
  probeTools,
  setLinearToken,
  writeInitialConfig,
  type ToolResult,
} from "@/lib/bindings";
import {
  buildSoundCheck,
  DEFAULT_MODEL,
  MODEL_OPTIONS,
  normalizeProjectSlug,
  onboardingStep,
  stepCapsLabel,
  stripModel,
  tokenLooksValid,
  TOTAL_STEPS,
  type WizardStep,
} from "@/lib/onboarding-model";

export interface OnboardingProps {
  // Called only on a SUCCESSFUL write (config seeded and the daemon told to start); the shell
  // re-reads status, sees `configured: true`, and swaps the wizard for the dashboard. Never called
  // on failure, so a partial-write error (config saved but daemon didn't start) stays visible in
  // the wizard rather than being unmounted away.
  onConfigured: () => void;
  // Lift a partial-write failure ("config saved, but the daemon could not start") into the shell.
  // We keep the wizard mounted on failure (onConfigured is success-only), but the shell's own
  // ~2s useDaemonStatus poll can still see configured: true — WriteInitialConfig wrote WORKFLOW.md
  // before the daemon-start leg failed — and unmount the wizard out from under the inline alert.
  // The shell persists this message in a banner that survives that unmount. Cleared with "" when a
  // fresh attempt starts. Mirrors the desktop shell's lifted `onboardErr`.
  onError?: (msg: string) => void;
}

// The compact URL a user creates a Linear personal API key from (Settings → Security & access →
// Personal API keys). Opened in the default browser (never the embedded webview).
const CREATE_TOKEN_URL = "https://linear.app/settings/account/security";

// The model choices reuse the canonical Settings list but display the compact "opus-4-8" form the
// mock shows (the full "claude-…" value is still what is stored/compared).
const WIZARD_MODEL_OPTIONS = MODEL_OPTIONS.map((o) => ({ ...o, label: stripModel(o.value), mono: true }));

// ProgressIndicator — the shared footer progress marker (mock 2e): the active step is a 16×5 rust
// bar, completed steps are brighter dots, upcoming steps are faint dots. Exposed as a progressbar
// for the a11y tree (and step-nav tests).
function ProgressIndicator({ step }: { step: WizardStep }) {
  return (
    <div
      role="progressbar"
      aria-label="Onboarding progress"
      aria-valuemin={1}
      aria-valuemax={TOTAL_STEPS}
      aria-valuenow={step}
      style={{ display: "flex", alignItems: "center", gap: 6 }}
    >
      {Array.from({ length: TOTAL_STEPS }, (_, i) => {
        const n = i + 1;
        const active = n === step;
        const done = n < step;
        return (
          <span
            key={n}
            aria-hidden
            style={{
              width: active ? 16 : 5,
              height: 5,
              borderRadius: active ? 3 : "50%",
              background: active ? "var(--rust)" : done ? "rgba(255,255,255,.25)" : "rgba(255,255,255,.14)",
              transition: "width .2s, background .2s",
            }}
          />
        );
      })}
    </div>
  );
}

// ProjectRadio — one selectable row of the step-2 Linear-project list (mock 2e): a rust radio, the
// project name, and the team code (right, mono). The selected row carries a faint rust tint.
function ProjectRadio({
  project,
  selected,
  onSelect,
}: {
  project: LinearProject;
  selected: boolean;
  onSelect: () => void;
}) {
  return (
    <button
      type="button"
      role="radio"
      aria-checked={selected}
      aria-label={project.name}
      onClick={onSelect}
      style={{
        display: "flex",
        alignItems: "center",
        gap: 11,
        width: "100%",
        padding: "12px 14px",
        border: "none",
        borderBottom: "1px solid var(--hair-section)",
        background: selected ? "color-mix(in srgb, var(--rust-text) 8%, transparent)" : "transparent",
        cursor: "pointer",
        textAlign: "left",
      }}
    >
      <span
        aria-hidden
        style={{
          width: 16,
          height: 16,
          flexShrink: 0,
          borderRadius: "50%",
          border: `1.5px solid ${selected ? "var(--rust)" : "var(--hair-strong)"}`,
          display: "grid",
          placeItems: "center",
        }}
      >
        {selected ? <span style={{ width: 7, height: 7, borderRadius: "50%", background: "var(--rust)" }} /> : null}
      </span>
      <span style={{ flex: 1, minWidth: 0, fontSize: 13.5, fontWeight: 500, color: "var(--ink)" }}>{project.name}</span>
      <span className="mono" style={{ fontSize: 11, fontWeight: 600, color: selected ? "var(--rust-text)" : "var(--faint)" }}>
        {project.team}
      </span>
    </button>
  );
}

// linkButton — a small inline text affordance ("Enter it manually" / "Back to project list").
function linkButton(label: string, onClick: () => void): React.ReactElement {
  return (
    <button
      type="button"
      onClick={onClick}
      style={{
        background: "none",
        border: "none",
        padding: 0,
        color: "var(--faint)",
        fontSize: 12,
        cursor: "pointer",
        textDecoration: "underline",
      }}
    >
      {label}
    </button>
  );
}

function Alert({ children }: { children: React.ReactNode }) {
  return (
    <div
      role="alert"
      style={{
        fontSize: 12.5,
        color: "var(--red)",
        background: "var(--tint-red)",
        border: "1px solid var(--border-danger)",
        borderRadius: "var(--r-ctrl)",
        padding: "9px 12px",
        lineHeight: 1.5,
      }}
    >
      {children}
    </div>
  );
}

// Onboarding — the first-run wizard AppShell shows when the daemon has no WORKFLOW.md yet
// (StatusDTO.configured === false). It breaks the chicken-and-egg of the Settings page — which
// hydrates from the daemon's /api and is therefore unusable before a config exists — by writing the
// initial config through the WriteInitialConfig Go binding, which needs no running daemon.
//
// Three steps (mock 2e), keeping the original data flow: (1) Connect Linear — paste a token to the
// Keychain; (2) Choose what to watch — PICK a project from the real Linear list (via the
// ListLinearProjects binding, which calls Linear directly pre-daemon — INF-277), with a manual
// slug/URL fallback and a starting model; (3) Sound check — a preflight checklist (the app-side
// `probeTools` doctor + the Linear connection), then "Start playing" seeds the config + starts the
// daemon. The 1↔2 transition is credential-driven (a stored token skips step 1); 2↔3 is local wizard
// state. Every binding degrades to a no-op / [] in a plain browser / tests, where this never renders
// (getStatus is null → "loading", not "not-configured").
export function Onboarding({ onConfigured, onError }: OnboardingProps) {
  const [hasToken, setHasToken] = React.useState(false);
  const [token, setToken] = React.useState("");
  const [slug, setSlug] = React.useState("");
  const [model, setModel] = React.useState<string>(DEFAULT_MODEL);
  const [busy, setBusy] = React.useState(false);
  const [error, setError] = React.useState<string | null>(null);
  // advanced gates step 2 → step 3: both are "token present", so a local flag (not a daemon-visible
  // state) distinguishes the project picker from the sound check.
  const [advanced, setAdvanced] = React.useState(false);

  // Project step: real projects fetched via the binding, plus a manual fallback.
  const [projects, setProjects] = React.useState<LinearProject[] | null>(null);
  const [projLoading, setProjLoading] = React.useState(false);
  const [projError, setProjError] = React.useState<string | null>(null);
  const [manualOpen, setManualOpen] = React.useState(false);
  const [manualInput, setManualInput] = React.useState("");
  const [manualError, setManualError] = React.useState<string | null>(null);

  // Sound-check step: the app-side tool probe (null until fetched on reaching step 3).
  const [tools, setTools] = React.useState<ToolResult[] | null>(null);

  const refreshToken = React.useCallback(async () => {
    const s = await credentialStatus();
    setHasToken(Boolean(s?.has_token));
  }, []);

  React.useEffect(() => {
    void refreshToken();
  }, [refreshToken]);

  // Effective step: no token → 1 (Connect Linear); token present → 3 once advanced, else 2.
  const step: WizardStep = onboardingStep(hasToken) === "token" ? 1 : advanced ? 3 : 2;

  // loadSeq guards against a stale in-flight fetch applying its result after the user navigated
  // away (Back to token) or kicked off a newer fetch (Retry). Each load captures the current seq;
  // backToToken and every new load bump it, so a superseded fetch drops its result instead of
  // repopulating cache with the previous token's projects.
  const loadSeq = React.useRef(0);

  const loadProjects = React.useCallback(async () => {
    const seq = ++loadSeq.current;
    setProjLoading(true);
    setProjError(null);
    try {
      const ps = await listLinearProjects();
      if (seq !== loadSeq.current) return; // superseded → drop the stale result
      setProjects(ps);
    } catch (e) {
      if (seq !== loadSeq.current) return;
      // A bad/revoked key (or any Linear failure) surfaces here — show it with Retry + back-to-token
      // rather than writing a config that would silently never dispatch.
      setProjError(e instanceof Error ? e.message : "Couldn't load your Linear projects.");
      setProjects(null);
    } finally {
      if (seq === loadSeq.current) setProjLoading(false);
    }
  }, []);

  // Fetch projects once a token is stored (steps 2 and 3 both need them), and after a retry/
  // back-to-token reset clears them. Skipped while loading or showing an error so Retry stays the
  // explicit re-fetch trigger.
  React.useEffect(() => {
    if (hasToken && projects === null && !projLoading && projError === null) {
      void loadProjects();
    }
  }, [hasToken, projects, projLoading, projError, loadProjects]);

  // Fetch the tool-doctor probe when the sound-check step is reached (degrades to [] with no bridge).
  React.useEffect(() => {
    if (step === 3 && tools === null) {
      void probeTools().then(setTools);
    }
  }, [step, tools]);

  const saveToken = async () => {
    if (!tokenLooksValid(token) || busy) return;
    setBusy(true);
    setError(null);
    try {
      await setLinearToken(token.trim());
      setToken("");
    } catch (e) {
      // setLinearToken can throw on a PARTIAL success (token persisted, but the daemon couldn't be
      // (re)started). We always re-read status in finally, so the step still advances when the
      // token actually landed; surface the message either way.
      setError(e instanceof Error ? e.message : "Couldn't save the token.");
    } finally {
      await refreshToken();
      setBusy(false);
    }
  };

  // backToToken returns to step 1 to fix a bad/revoked key: it clears the stored token and the
  // loaded-project state so re-entering a key re-fetches fresh (this is the "← Back" from step 2).
  const backToToken = async () => {
    // Invalidate any in-flight project fetch so its late result can't repopulate cache after we
    // clear it (a new token must re-fetch fresh, not show the previous token's projects).
    loadSeq.current++;
    try {
      await clearLinearToken();
    } catch {
      // Best-effort: even if the clear fails, drop our cached state and re-read status below.
    }
    setSlug("");
    setAdvanced(false);
    setProjects(null);
    setProjError(null);
    setProjLoading(false);
    setManualOpen(false);
    setManualError(null);
    setError(null);
    await refreshToken();
  };

  const createConfigWith = async (value: string) => {
    if (busy) return;
    setBusy(true);
    setError(null);
    onError?.("");
    try {
      await writeInitialConfig(value);
      // Success only: the config is written AND the daemon was told to start. Signal the shell to
      // re-read status so it swaps the wizard for the dashboard. We must NOT do this on failure —
      // the WriteInitialConfig binding can fail *after* writing WORKFLOW.md (e.g. it couldn't stop a
      // stale daemon to hand off the loopback port). If we called onConfigured() there, the shell
      // would see configured: true and unmount this wizard, discarding the error alert we just set —
      // exactly the "config saved, but the daemon could not start" message the user needs. Keeping
      // the wizard mounted on failure preserves that message; its text directs the next step (quit &
      // relaunch, or Restart), and a relaunch finds configured: true so onboarding never re-shows.
      onConfigured();
    } catch (e) {
      const msg = e instanceof Error ? e.message : "Couldn't create the configuration.";
      setError(msg);
      // Lift the message to the shell too: WriteInitialConfig may have written WORKFLOW.md before
      // failing, so the shell's poll can flip configured: true and unmount this wizard (with its
      // inline alert) before the user reads it. The shell's banner survives that unmount.
      onError?.(msg);
    } finally {
      setBusy(false);
    }
  };

  // advanceToSoundCheck moves step 2 → step 3. In manual mode it normalizes the pasted value (bare
  // slugId / URL slug / full URL) to the bare slugId first; un-normalizable input shows an inline
  // error and does not advance.
  const advanceToSoundCheck = () => {
    if (manualOpen) {
      const res = normalizeProjectSlug(manualInput);
      if (!res.ok) {
        setManualError(res.error);
        return;
      }
      setManualError(null);
      setSlug(res.slug);
    } else if (!slug) {
      return;
    }
    setAdvanced(true);
  };

  // The footer's primary "Continue" (steps 1–2 only; step 3's primary is the full-width "Start
  // playing"). Its enabled/label state is per-step.
  const primaryAction = step === 1 ? () => void saveToken() : advanceToSoundCheck;
  const continueDisabled =
    busy ||
    (step === 1 && !tokenLooksValid(token)) ||
    (step === 2 && (projLoading || projError !== null || (manualOpen ? manualInput.trim() === "" : slug === "")));

  const heading = step === 1 ? "One token drives the whole ensemble." : step === 2 ? "Point an agent at a project." : "Everything's in tune.";

  return (
    <div style={{ display: "flex", flexDirection: "column", gap: 22 }}>
      {/* header: caps step marker + heading (+ body on step 1) */}
      <div style={{ display: "flex", flexDirection: "column", gap: 10 }}>
        <span style={{ fontSize: 10, fontWeight: 600, letterSpacing: ".14em", color: "var(--rust-text)" }}>
          {stepCapsLabel(step)}
        </span>
        <h1 style={{ fontSize: 17, fontWeight: 600, letterSpacing: "-0.01em", color: "var(--ink)" }}>{heading}</h1>
        {step === 1 ? (
          <p style={{ fontSize: 12.5, color: "var(--text-muted)", lineHeight: 1.55, maxWidth: 460 }}>
            Rhapsody reads tickets from Linear and dispatches a coding agent per ticket. Paste a personal API key — it
            lives in the macOS keychain, never on disk.
          </p>
        ) : null}
      </div>

      {/* step body */}
      {step === 1 ? (
        <div style={{ display: "flex", flexDirection: "column", gap: 12 }}>
          <TextInput
            mono
            type="password"
            prefixIcon={Key}
            value={token}
            placeholder="lin_api_…"
            aria-label="Linear API token"
            onChange={(e) => setToken(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") void saveToken();
            }}
            // Blinking rust caret (mock 2e); compact 34px field.
            style={{ height: 34, fontSize: 13, caretColor: "var(--rust-text)" }}
          />
          <button
            type="button"
            onClick={() => openExternal(CREATE_TOKEN_URL)}
            style={{
              alignSelf: "flex-start",
              background: "none",
              border: "none",
              padding: 0,
              color: "var(--rust-text)",
              fontSize: 12.5,
              cursor: "pointer",
            }}
          >
            Create a token in Linear <span style={{ color: "var(--rust-text)" }}>↗</span>
          </button>
        </div>
      ) : step === 2 ? (
        <div style={{ display: "flex", flexDirection: "column", gap: 14 }}>
          {projLoading ? (
            <p style={{ fontSize: 13, color: "var(--text-muted)", padding: "8px 0" }}>Loading your Linear projects…</p>
          ) : projError ? (
            <>
              <Alert>Couldn't load your Linear projects: {projError}</Alert>
              <Button variant="primary" disabled={busy} onClick={() => void loadProjects()}>
                Retry
              </Button>
            </>
          ) : manualOpen ? (
            <>
              <label style={{ fontSize: 12.5, fontWeight: 600, color: "var(--text-muted)" }}>Project slug or URL</label>
              <TextInput
                mono
                value={manualInput}
                placeholder="rhapsody-app-872639248532 or a Linear project URL"
                aria-label="Project slug"
                onChange={(e) => {
                  setManualInput(e.target.value);
                  setManualError(null);
                }}
                onKeyDown={(e) => {
                  if (e.key === "Enter") advanceToSoundCheck();
                }}
              />
              <p style={{ fontSize: 12, color: "var(--faint)", lineHeight: 1.5 }}>
                Paste the project's URL or its slug (e.g. <code>my-project-9c29e9ade060</code>).
              </p>
              {manualError ? <Alert>{manualError}</Alert> : null}
              <div>{linkButton("Back to project list", () => setManualOpen(false))}</div>
            </>
          ) : (
            <>
              <div
                role="radiogroup"
                aria-label="Linear project"
                style={{
                  border: "1px solid var(--hair-card)",
                  borderRadius: "var(--r-card)",
                  overflow: "hidden",
                  background: "var(--card)",
                }}
              >
                {(projects ?? []).length === 0 ? (
                  <p style={{ padding: "18px 14px", fontSize: 12.5, color: "var(--faint)" }}>
                    No projects found for this token — enter the slug manually below.
                  </p>
                ) : (
                  (projects ?? []).map((p) => (
                    <ProjectRadio key={p.slug} project={p} selected={p.slug === slug} onSelect={() => setSlug(p.slug)} />
                  ))
                )}
              </div>

              {/* detected-repo + model row: the repo is auto-detected by the daemon from the chosen
                  project's Linear settings (no pre-daemon binding), so this reassures + sets the
                  starting model. */}
              <div style={{ display: "flex", alignItems: "center", gap: 12 }}>
                <span style={{ display: "inline-flex", color: "var(--sage)" }}>
                  <Check size={15} style={{ strokeWidth: 2.4 }} />
                </span>
                <span style={{ flex: 1, minWidth: 0, fontSize: 12, color: "var(--text-muted)" }}>
                  Repo detected from the project's Linear settings
                </span>
                <Select value={model} options={WIZARD_MODEL_OPTIONS} onChange={setModel} width={150} />
              </div>

              <div>{linkButton("Enter it manually", () => setManualOpen(true))}</div>
            </>
          )}
        </div>
      ) : (
        <div style={{ display: "flex", flexDirection: "column", gap: 16 }}>
          {/* sound-check checklist */}
          <div
            style={{
              border: "1px solid var(--hair-card)",
              borderRadius: "var(--r-card)",
              overflow: "hidden",
              background: "var(--card)",
            }}
          >
            {buildSoundCheck(tools ?? [], { linearConnected: hasToken }).map((item, i, arr) => (
              <div
                key={item.key}
                style={{
                  display: "flex",
                  alignItems: "center",
                  gap: 12,
                  padding: "11px 14px",
                  borderBottom: i === arr.length - 1 ? "none" : "1px solid var(--hair-section)",
                }}
              >
                {item.ok ? (
                  <span style={{ display: "inline-flex", color: "var(--sage)" }}>
                    <Check size={14} style={{ strokeWidth: 2.4 }} />
                  </span>
                ) : (
                  <StatusDot color="var(--amber)" size={7} />
                )}
                <span className="mono" style={{ width: 110, flexShrink: 0, fontSize: 12.5, color: "var(--ink)" }}>
                  {item.name}
                </span>
                <span
                  className="mono"
                  style={{
                    flex: 1,
                    minWidth: 0,
                    fontSize: 11,
                    color: item.ok ? "var(--faint)" : "var(--amber)",
                    overflow: "hidden",
                    textOverflow: "ellipsis",
                    whiteSpace: "nowrap",
                  }}
                >
                  {item.detail}
                </span>
              </div>
            ))}
          </div>

          <Button variant="primary" icon={Play} disabled={busy} onClick={() => void createConfigWith(slug)} style={{ width: "100%" }}>
            {busy ? "Starting…" : "Start playing"}
          </Button>
          <p style={{ fontSize: 11, color: "var(--faint)", textAlign: "center" }}>
            You can stop or restart anytime from the toolbar.
          </p>
        </div>
      )}

      {error ? <Alert>{error}</Alert> : null}

      {/* shared footer: Back (from step 2) · progress · Continue (steps 1–2) */}
      <div style={{ display: "flex", alignItems: "center", gap: 16, paddingTop: 2 }}>
        {step > 1 ? (
          <Button
            variant="ghost"
            size="sm"
            onClick={step === 2 ? () => void backToToken() : () => setAdvanced(false)}
          >
            ← Back
          </Button>
        ) : null}
        <ProgressIndicator step={step} />
        <div style={{ flex: 1 }} />
        {step < 3 ? (
          <Button variant="primary" disabled={continueDisabled} onClick={primaryAction}>
            {busy && step === 1 ? "Saving…" : "Continue"}
          </Button>
        ) : null}
      </div>
    </div>
  );
}
