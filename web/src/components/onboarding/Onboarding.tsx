import * as React from "react";
import { Button, Check, ChevronDown, Linear, Search, StatusDot, TextInput } from "@/components/ui";
import type { LinearProject } from "@/lib/api";
import {
  clearLinearToken,
  credentialStatus,
  listLinearProjects,
  setLinearToken,
  writeInitialConfig,
} from "@/lib/bindings";
import { normalizeProjectSlug, onboardingStep, tokenLooksValid } from "@/lib/onboarding-model";

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

// ProjectPicker — searchable Linear-project picker for the onboarding project step. Adapted from
// settings/AddAgentSheet's ProjectPicker (name + slugId subtext + team), minus the usedSlugs filter
// (onboarding configures the first project, so nothing is taken yet). onChange receives the bare
// slugId — the value the daemon's dispatch query filters on.
function ProjectPicker({
  value,
  onChange,
  projects,
}: {
  value: string;
  onChange: (slug: string) => void;
  projects: LinearProject[];
}) {
  const [q, setQ] = React.useState("");
  // Open by default: onboarding has no prior selection, so showing the project list immediately
  // (rather than behind a focus) is the expected first-run affordance. Picking one closes it to the
  // selected-project summary, which reopens on click.
  const [open, setOpen] = React.useState(true);
  const sel = projects.find((p) => p.slug === value);
  const results = projects.filter((p) => `${p.name} ${p.slug} ${p.team}`.toLowerCase().includes(q.toLowerCase()));

  return (
    <div style={{ position: "relative" }}>
      {sel && !open ? (
        <button
          type="button"
          onClick={() => {
            setOpen(true);
            setQ("");
          }}
          style={{
            width: "100%",
            height: 44,
            display: "flex",
            alignItems: "center",
            justifyContent: "space-between",
            gap: 10,
            background: "var(--bg-input)",
            border: "1px solid var(--line-strong)",
            borderRadius: "var(--r-ctrl)",
            padding: "0 12px",
            cursor: "pointer",
          }}
        >
          <span style={{ display: "flex", alignItems: "center", gap: 10, minWidth: 0 }}>
            <StatusDot color={sel.color} size={9} />
            <span style={{ fontSize: 13.5, fontWeight: 500, color: "var(--tx)" }}>{sel.name}</span>
            <span className="mono" style={{ fontSize: 11.5, color: "var(--tx-3)" }}>
              {sel.team}
            </span>
          </span>
          <ChevronDown size={15} style={{ color: "var(--tx-3)" }} />
        </button>
      ) : (
        <TextInput
          autoFocus
          prefixIcon={Search}
          placeholder="Search your Linear projects…"
          aria-label="Search your Linear projects"
          value={q}
          onChange={(e) => {
            setQ(e.target.value);
            setOpen(true);
          }}
          onFocus={() => setOpen(true)}
          style={{ height: 44 }}
        />
      )}
      {open ? (
        <div
          role="listbox"
          style={{
            marginTop: 8,
            background: "var(--bg-card-2)",
            border: "1px solid var(--line)",
            borderRadius: "var(--r-ctrl)",
            overflow: "hidden",
            maxHeight: 240,
            overflowY: "auto",
          }}
        >
          {results.length === 0 ? (
            <div style={{ padding: "20px", textAlign: "center", color: "var(--tx-3)", fontSize: 13 }}>
              No projects match.
            </div>
          ) : (
            results.map((p) => (
              <button
                key={p.slug}
                type="button"
                role="option"
                aria-selected={p.slug === value}
                onClick={() => {
                  onChange(p.slug);
                  setOpen(false);
                }}
                style={{
                  width: "100%",
                  display: "flex",
                  alignItems: "center",
                  gap: 11,
                  padding: "11px 14px",
                  background: p.slug === value ? "var(--em-soft)" : "transparent",
                  border: "none",
                  borderBottom: "1px solid var(--line-2)",
                  cursor: "pointer",
                  textAlign: "left",
                }}
              >
                <StatusDot color={p.color} size={9} />
                <div style={{ flex: 1, minWidth: 0 }}>
                  <div style={{ fontSize: 13.5, fontWeight: 500, color: "var(--tx)" }}>{p.name}</div>
                  <div
                    className="mono"
                    style={{ fontSize: 11.5, color: "var(--tx-3)", overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}
                  >
                    {p.slug}
                  </div>
                </div>
                <span className="mono" style={{ fontSize: 11, fontWeight: 600, color: "var(--tx-3)" }}>
                  {p.team}
                </span>
              </button>
            ))
          )}
        </div>
      ) : null}
    </div>
  );
}

// linkButton — a small inline text affordance ("Enter it manually" / "Back to token" / "Retry").
function linkButton(label: string, onClick: () => void): React.ReactElement {
  return (
    <button
      type="button"
      onClick={onClick}
      style={{
        background: "none",
        border: "none",
        padding: 0,
        color: "var(--tx-3)",
        fontSize: 12,
        cursor: "pointer",
        textDecoration: "underline",
      }}
    >
      {label}
    </button>
  );
}

// Onboarding — the first-run wizard AppShell shows when the daemon has no WORKFLOW.md yet
// (StatusDTO.configured === false). It breaks the chicken-and-egg of the Settings page — which
// hydrates from the daemon's /api and is therefore unusable before a config exists — by writing
// the initial config through the WriteInitialConfig Go binding, which needs no running daemon.
// Two steps: paste a Linear token (→ Keychain), then PICK a project from the real Linear list (via
// the ListLinearProjects binding, which calls Linear directly pre-daemon — INF-277) and seed the
// config + start the daemon. A manual fallback normalizes a pasted slug/URL to the bare slugId.
// Every binding degrades to a no-op in a plain browser / tests, where this never renders (getStatus
// is null → "loading", not "not-configured").
export function Onboarding({ onConfigured, onError }: OnboardingProps) {
  const [hasToken, setHasToken] = React.useState(false);
  const [token, setToken] = React.useState("");
  const [slug, setSlug] = React.useState("");
  const [busy, setBusy] = React.useState(false);
  const [error, setError] = React.useState<string | null>(null);

  // Project step: real projects fetched via the binding, plus a manual fallback.
  const [projects, setProjects] = React.useState<LinearProject[] | null>(null);
  const [projLoading, setProjLoading] = React.useState(false);
  const [projError, setProjError] = React.useState<string | null>(null);
  const [manualOpen, setManualOpen] = React.useState(false);
  const [manualInput, setManualInput] = React.useState("");
  const [manualError, setManualError] = React.useState<string | null>(null);

  const refreshToken = React.useCallback(async () => {
    const s = await credentialStatus();
    setHasToken(Boolean(s?.has_token));
  }, []);

  React.useEffect(() => {
    void refreshToken();
  }, [refreshToken]);

  const step = onboardingStep(hasToken);

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

  // Fetch projects once we reach the project step (and after a retry/back-to-token reset clears
  // them). Skipped while loading or showing an error so Retry stays the explicit re-fetch trigger.
  React.useEffect(() => {
    if (step === "project" && projects === null && !projLoading && projError === null) {
      void loadProjects();
    }
  }, [step, projects, projLoading, projError, loadProjects]);

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

  // backToToken returns to the token step to fix a bad/revoked key: it clears the stored token and
  // the loaded-project state so re-entering a key re-fetches fresh.
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

  // submitManual normalizes the pasted value (bare slugId / URL slug / full URL) to the bare slugId
  // before writing; un-normalizable input shows an inline error and never writes.
  const submitManual = () => {
    const res = normalizeProjectSlug(manualInput);
    if (!res.ok) {
      setManualError(res.error);
      return;
    }
    setManualError(null);
    void createConfigWith(res.slug);
  };

  return (
    <div style={{ display: "flex", justifyContent: "center", paddingTop: 40 }}>
      <div style={{ width: "100%", maxWidth: 460, display: "flex", flexDirection: "column", gap: 22 }}>
        {/* header */}
        <div style={{ display: "flex", flexDirection: "column", alignItems: "center", gap: 12, textAlign: "center" }}>
          <div
            style={{
              width: 46,
              height: 46,
              borderRadius: 13,
              display: "grid",
              placeItems: "center",
              background: "var(--em-soft)",
              border: "1px solid rgba(16,185,129,.3)",
              color: "var(--em-bright)",
            }}
          >
            <Linear size={22} />
          </div>
          <div>
            <h1 style={{ fontSize: 20, fontWeight: 600, letterSpacing: "-0.02em" }}>Welcome to Symphony</h1>
            <p style={{ fontSize: 13, color: "var(--tx-3)", marginTop: 6, lineHeight: 1.5 }}>
              {step === "token"
                ? "Connect your Linear account to get started."
                : "Pick the Linear project Symphony should watch."}
            </p>
          </div>
        </div>

        {/* step card */}
        <div
          style={{
            background: "var(--bg-card)",
            border: "1px solid var(--line)",
            borderRadius: "var(--r-card)",
            padding: 22,
            display: "flex",
            flexDirection: "column",
            gap: 14,
          }}
        >
          {step === "token" ? (
            <>
              <label style={{ fontSize: 12.5, fontWeight: 600, color: "var(--tx-2)" }}>Linear API token</label>
              <TextInput
                mono
                type="password"
                value={token}
                placeholder="lin_api_…"
                aria-label="Linear API token"
                onChange={(e) => setToken(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === "Enter") void saveToken();
                }}
              />
              <p style={{ fontSize: 12, color: "var(--tx-3)", lineHeight: 1.5 }}>
                Stored in your macOS Keychain — never written to the config file. Create one in Linear
                → Settings → Security &amp; access → Personal API keys.
              </p>
              <Button variant="primary" disabled={!tokenLooksValid(token) || busy} onClick={() => void saveToken()}>
                {busy ? "Saving…" : "Save & continue"}
              </Button>
            </>
          ) : (
            <>
              <div style={{ display: "inline-flex", alignItems: "center", gap: 7, fontSize: 12.5, color: "var(--em-bright)" }}>
                <Check size={14} style={{ strokeWidth: 2.4 }} /> Linear connected
              </div>

              {projLoading ? (
                <>
                  <p style={{ fontSize: 13, color: "var(--tx-3)", padding: "8px 0" }}>Loading your Linear projects…</p>
                  {/* An escape during a slow/hung load: a stale in-flight fetch is dropped (see loadSeq). */}
                  <div>{linkButton("Back to token", () => void backToToken())}</div>
                </>
              ) : projError ? (
                <>
                  <div
                    role="alert"
                    style={{
                      fontSize: 12.5,
                      color: "var(--red)",
                      background: "var(--red-soft)",
                      border: "1px solid rgba(239,83,80,.3)",
                      borderRadius: "var(--r-ctrl)",
                      padding: "9px 12px",
                    }}
                  >
                    Couldn't load your Linear projects: {projError}
                  </div>
                  <div style={{ display: "flex", gap: 14, alignItems: "center" }}>
                    <Button variant="primary" disabled={busy} onClick={() => void loadProjects()}>
                      Retry
                    </Button>
                    {linkButton("Back to token", () => void backToToken())}
                  </div>
                </>
              ) : manualOpen ? (
                <>
                  <label style={{ fontSize: 12.5, fontWeight: 600, color: "var(--tx-2)" }}>Project slug or URL</label>
                  <TextInput
                    mono
                    value={manualInput}
                    placeholder="symphony-app-872639248532 or a Linear project URL"
                    aria-label="Project slug"
                    onChange={(e) => {
                      setManualInput(e.target.value);
                      setManualError(null);
                    }}
                    onKeyDown={(e) => {
                      if (e.key === "Enter") submitManual();
                    }}
                  />
                  <p style={{ fontSize: 12, color: "var(--tx-3)", lineHeight: 1.5 }}>
                    Paste the project's URL or its slug (e.g. <code>my-project-9c29e9ade060</code>).
                  </p>
                  {manualError ? (
                    <div role="alert" style={{ fontSize: 12, color: "var(--red)" }}>
                      {manualError}
                    </div>
                  ) : null}
                  <Button variant="primary" disabled={busy} onClick={() => submitManual()}>
                    {busy ? "Creating…" : "Create config & start"}
                  </Button>
                  <div>{linkButton("Back to project list", () => setManualOpen(false))}</div>
                </>
              ) : (
                <>
                  <label style={{ fontSize: 12.5, fontWeight: 600, color: "var(--tx-2)" }}>Linear project</label>
                  <ProjectPicker value={slug} onChange={setSlug} projects={projects ?? []} />
                  {projects && projects.length === 0 ? (
                    <p style={{ fontSize: 12, color: "var(--tx-3)", lineHeight: 1.5 }}>
                      No projects found for this token — enter the slug manually below.
                    </p>
                  ) : null}
                  <Button variant="primary" disabled={!slug || busy} onClick={() => void createConfigWith(slug)}>
                    {busy ? "Creating…" : "Create config & start"}
                  </Button>
                  <div style={{ display: "flex", gap: 14, alignItems: "center" }}>
                    {linkButton("Enter it manually", () => setManualOpen(true))}
                    {linkButton("Back to token", () => void backToToken())}
                  </div>
                </>
              )}
            </>
          )}

          {error ? (
            <div
              role="alert"
              style={{
                fontSize: 12.5,
                color: "var(--red)",
                background: "var(--red-soft)",
                border: "1px solid rgba(239,83,80,.3)",
                borderRadius: "var(--r-ctrl)",
                padding: "9px 12px",
              }}
            >
              {error}
            </div>
          ) : null}
        </div>
      </div>
    </div>
  );
}
