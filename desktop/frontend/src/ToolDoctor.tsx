import { useCallback, useEffect, useState } from "react";
import { probeTools, setToolOverride, type ToolResult } from "./bindings";
import { remediationHint, statusBadge, toolSummary } from "./tools";

// ToolDoctor is the preflight panel (spec §6): it detects claude/gh/gt/git, shows
// presence/version/health + a remediation hint, and lets the user set a per-tool override
// path. Overrides reach the daemon's agent-launch PATH on the next daemon restart. Ported from
// $REF/desktop/frontend/src/ToolDoctor.tsx.
export function ToolDoctor({ onClose }: { onClose: () => void }) {
  const [results, setResults] = useState<ToolResult[]>([]);
  const [loading, setLoading] = useState(true);

  const recheck = useCallback(async () => {
    setLoading(true);
    try {
      setResults(await probeTools());
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void recheck();
  }, [recheck]);

  const summary = toolSummary(results);

  return (
    <div className="tooldoctor">
      <div className="bar">
        <strong>Tool-doctor</strong>
        <span className="label">
          {loading ? "Checking…" : `${summary.healthy}/${summary.total} healthy`}
        </span>
        <div className="actions">
          <button onClick={() => void recheck()} disabled={loading}>
            Re-check
          </button>
          <button onClick={onClose}>Close</button>
        </div>
      </div>
      <div className="tools">
        {results.map((r) => (
          // Key includes the resolved path so a Re-check / successful override remounts the row
          // and its input reflects the fresh probe result (rather than the stale mount value).
          <ToolRow key={`${r.name}:${r.path}`} result={r} onChanged={() => void recheck()} />
        ))}
      </div>
    </div>
  );
}

// overrideError normalizes a rejected setToolOverride into a user-facing message. A backend
// rejection (non-executable path, prefs write failure) must surface inline rather than being
// silently swallowed by the fire-and-forget save.
export function overrideError(e: unknown): string {
  const msg = e instanceof Error ? e.message : String(e);
  return msg.trim() || "Failed to save override.";
}

// runOverrideSave centralizes the save contract so it can be pinned without a DOM: on backend
// rejection it reports the inline error and does NOT call onChanged; on success it clears the
// error and calls onChanged. setSaving brackets the attempt so the button can disable.
export async function runOverrideSave(opts: {
  name: string;
  path: string;
  persist: (name: string, path: string) => Promise<void>;
  onChanged: () => void;
  setSaving: (v: boolean) => void;
  setError: (v: string | null) => void;
}): Promise<void> {
  opts.setSaving(true);
  opts.setError(null);
  try {
    await opts.persist(opts.name, opts.path.trim());
  } catch (e) {
    // Keep the row editable and surface the failure; do not signal success.
    opts.setError(overrideError(e));
    return;
  } finally {
    opts.setSaving(false);
  }
  opts.onChanged();
}

function ToolRow({ result, onChanged }: { result: ToolResult; onChanged: () => void }) {
  const [path, setPath] = useState(result.path);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const badge = statusBadge(result);
  const save = () =>
    runOverrideSave({ name: result.name, path, persist: setToolOverride, onChanged, setSaving, setError });
  return (
    <div className="tool">
      <div className="tool-head">
        <span className={`dot ${badge}`} />
        <strong>{result.name}</strong>
        <span className="ver">{result.version || (result.found ? "(no version)" : "not found")}</span>
      </div>
      <div className="tool-hint">{remediationHint(result)}</div>
      <div className="tool-override">
        <input
          aria-label={`${result.name} path`}
          placeholder={`/path/to/${result.name}`}
          value={path}
          onChange={(e) => setPath(e.target.value)}
        />
        <button onClick={() => void save()} disabled={saving}>
          Override
        </button>
      </div>
      {error && (
        <div className="tool-error" role="alert">
          {error}
        </div>
      )}
    </div>
  );
}
