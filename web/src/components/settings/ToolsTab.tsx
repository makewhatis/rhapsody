import * as React from "react";
import { useQuery } from "@tanstack/react-query";
import {
  AlertTriangle,
  Button,
  Check,
  Download,
  Folder,
  type IconComponent,
  Linear,
  Refresh,
  SectionCard,
  Shield,
  SkeletonCard,
  Terminal,
  TextInput,
  X,
} from "@/components/ui";
import { installTool, pickFile, probeTools, setToolOverride, type ToolResult } from "@/lib/bindings";
import { useLinearIdentity } from "@/hooks/useConfig";

type ToolStatus = "ok" | "warn" | "not-found";

const LIGHT: Record<ToolStatus, { color: string; bg: string; ring: string; label: string; icon: IconComponent }> = {
  ok: { color: "var(--em-bright)", bg: "var(--em-soft)", ring: "rgba(16,185,129,.3)", label: "Ready", icon: Check },
  warn: { color: "var(--amber)", bg: "var(--amber-soft)", ring: "rgba(245,181,68,.3)", label: "Warning", icon: AlertTriangle },
  "not-found": { color: "var(--red)", bg: "var(--red-soft)", ring: "rgba(239,83,80,.3)", label: "Not found", icon: X },
};

// Static sub-labels the design shows beside the command (the daemon's ToolResult has no sub).
const SUB: Record<string, string> = { gt: "Graphite", git: "system" };

const ROW_GRID = "26px minmax(150px,1fr) minmax(180px,1.3fr) auto";

function toolStatus(t: ToolResult): ToolStatus {
  if (!t.found) return "not-found";
  return t.healthy ? "ok" : "warn";
}

// Missing CLI → Install; present-but-unhealthy (e.g. an update available) → Update; healthy → none.
function toolAction(t: ToolResult): "Install" | "Update" | null {
  if (!t.found) return "Install";
  if (!t.healthy) return "Update";
  return null;
}

function ToolRow({
  t,
  last,
  onPickPath,
  onSetPath,
  onAction,
}: {
  t: ToolResult;
  last: boolean;
  onPickPath: () => void;
  onSetPath: (path: string) => void;
  onAction: () => void;
}) {
  const status = toolStatus(t);
  const m = LIGHT[status];
  const StatusIcon = m.icon;
  const action = toolAction(t);
  const sub = SUB[t.name];
  // The path field is editable so an override can be typed/pasted (persisted via the existing
  // setToolOverride binding) even where the native file picker isn't wired; keep it in sync when a
  // re-probe changes the detected/overridden path.
  const [path, setPath] = React.useState(t.path);
  const [focused, setFocused] = React.useState(false);
  // Sync the field from probe results only when the user ISN'T editing it — otherwise a refetch
  // (triggered by another row's override/install) would clobber an in-progress edit.
  React.useEffect(() => {
    if (!focused) setPath(t.path);
  }, [t.path, focused]);
  const commitPath = () => {
    // Trim before persisting — a path with stray whitespace would be rejected by the Go binding as
    // not an executable. Normalize the field to the trimmed value too.
    const trimmed = path.trim();
    if (trimmed !== path) setPath(trimmed);
    if (trimmed !== t.path) onSetPath(trimmed);
  };
  return (
    <div
      style={{
        display: "grid",
        gridTemplateColumns: ROW_GRID,
        gap: 18,
        alignItems: "center",
        padding: "16px 22px",
        borderBottom: last ? "none" : "1px solid var(--line-2)",
      }}
    >
      {/* status light */}
      <div
        style={{
          width: 26,
          height: 26,
          borderRadius: "50%",
          display: "grid",
          placeItems: "center",
          background: m.bg,
          border: `1px solid ${m.ring}`,
          color: m.color,
        }}
      >
        <StatusIcon size={status === "warn" ? 13 : 14} style={status === "warn" ? undefined : { strokeWidth: 2.4 }} />
      </div>
      {/* name + cmd + version + detail */}
      <div style={{ minWidth: 0 }}>
        <div style={{ display: "flex", alignItems: "baseline", gap: 9 }}>
          <span className="mono" style={{ fontSize: 14, fontWeight: 600, color: "var(--tx)" }}>
            {t.name}
          </span>
          {sub ? <span style={{ fontSize: 12, color: "var(--tx-3)" }}>{sub}</span> : null}
        </div>
        <div style={{ fontSize: 12, color: "var(--tx-3)", marginTop: 3, display: "flex", alignItems: "center", gap: 7 }}>
          {t.version ? (
            <span className="mono" style={{ color: "var(--tx-2)" }}>
              v{t.version}
            </span>
          ) : (
            <span style={{ color: m.color }}>—</span>
          )}
          <span style={{ color: "var(--tx-faint)" }}>·</span>
          <span style={{ color: status === "ok" ? "var(--tx-3)" : m.color }}>{t.detail}</span>
        </div>
      </div>
      {/* path override */}
      <div style={{ display: "flex", gap: 8 }}>
        <TextInput
          value={path}
          mono
          placeholder="Auto-detect from PATH"
          aria-label={`${t.name} path override`}
          onChange={(e) => setPath(e.target.value)}
          onFocus={() => setFocused(true)}
          onBlur={() => {
            setFocused(false);
            commitPath();
          }}
          onKeyDown={(e) => {
            if (e.key === "Enter") (e.target as HTMLInputElement).blur();
          }}
          style={{ height: 36, fontSize: 12, flex: 1 }}
        />
        <Button
          variant="subtle"
          size="sm"
          icon={Folder}
          aria-label={`Choose path for ${t.name}`}
          onClick={onPickPath}
          style={{ paddingLeft: 11, paddingRight: 11 }}
        />
      </div>
      {/* remediation */}
      <div style={{ display: "flex", justifyContent: "flex-end" }}>
        {action ? (
          <Button variant={status === "not-found" ? "primary" : "subtle"} size="sm" icon={action === "Install" ? Download : Refresh} onClick={onAction}>
            {action}
          </Button>
        ) : (
          <span style={{ fontSize: 12, color: "var(--tx-faint)", display: "inline-flex", alignItems: "center", gap: 6 }}>
            <Check size={13} style={{ color: "var(--em-bright)" }} />
            {m.label}
          </span>
        )}
      </div>
    </div>
  );
}

// ToolsTab — the app-side preflight panel (Go bindings, NOT the daemon). A summary banner over a
// read-only Linear mirror and the required-CLI rows (claude/gh/gt/git): each row shows a status
// light, version + detail, a path-override field with a folder picker, and an Install/Update
// action wired to the supervisor. Re-run preflight re-probes via the `toolcheck` binding. Ported
// from the design `tools.jsx`.
export function ToolsTab() {
  const tools = useQuery({ queryKey: ["tools"], queryFn: probeTools, refetchOnWindowFocus: false });
  const identity = useLinearIdentity();
  const list = tools.data ?? [];
  const okCount = list.filter((t) => toolStatus(t) === "ok").length;
  const issues = list.length - okCount;
  const account = identity.data;
  // No tool data (after loading) means the toolcheck binding isn't reachable — e.g. the
  // daemon-served browser UI with no Wails bridge — or the probe failed. That is NOT "all systems
  // ready"; surface it distinctly so an empty list never reads as a clean bill of health.
  const unavailable = !tools.isLoading && (tools.isError || list.length === 0);
  // When unavailable (probe error / no bridge) don't render the cached rows — they'd be stale and
  // contradict the "preflight unavailable" banner; show the empty-state note instead.
  const displayList = unavailable ? [] : list;

  const [actionError, setActionError] = React.useState<string | null>(null);

  const applyOverride = (name: string, path: string) => {
    setActionError(null);
    void setToolOverride(name, path)
      .then(() => tools.refetch())
      .catch((e: unknown) => {
        // The Go binding rejects a path that isn't an executable. Surface it and re-probe so the
        // field snaps back to the actual detected path rather than implying the override stuck.
        setActionError(e instanceof Error ? e.message : `Couldn't set the ${name} path.`);
        void tools.refetch();
      });
  };
  const pickPath = (name: string) => {
    // A CLI override is a path to the executable FILE, so use the file chooser (not a folder one).
    void pickFile(`Choose ${name} executable`).then((path) => {
      if (path) applyOverride(name, path);
    });
  };
  const runAction = (name: string) => {
    setActionError(null);
    void installTool(name)
      .then(() => tools.refetch())
      .catch((e: unknown) => setActionError(e instanceof Error ? e.message : `Couldn't install ${name}.`));
  };
  const reRun = () => {
    setActionError(null);
    void tools.refetch();
  };

  if (tools.isLoading && !tools.data) {
    return (
      <div style={{ display: "flex", flexDirection: "column", gap: 18 }}>
        <SkeletonCard />
        <SkeletonCard />
      </div>
    );
  }

  return (
    <div style={{ display: "flex", flexDirection: "column", gap: 18 }}>
      {/* summary banner */}
      <div
        style={{
          background: "var(--bg-card)",
          border: "1px solid var(--line)",
          borderRadius: "var(--r-card)",
          padding: "18px 22px",
          display: "flex",
          alignItems: "center",
          justifyContent: "space-between",
          gap: 16,
        }}
      >
        <div style={{ display: "flex", alignItems: "center", gap: 14 }}>
          <div
            style={{
              width: 38,
              height: 38,
              borderRadius: 11,
              display: "grid",
              placeItems: "center",
              background: issues || unavailable ? "var(--amber-soft)" : "var(--em-soft)",
              border: `1px solid ${issues || unavailable ? "rgba(245,181,68,.3)" : "rgba(16,185,129,.3)"}`,
              color: issues || unavailable ? "var(--amber)" : "var(--em-bright)",
            }}
          >
            <Shield size={19} />
          </div>
          <div>
            <div style={{ fontSize: 14.5, fontWeight: 600 }}>
              {unavailable
                ? "Tool preflight unavailable"
                : issues
                  ? `${issues} issue${issues > 1 ? "s" : ""} need attention`
                  : "All systems ready"}
            </div>
            <div style={{ fontSize: 12.5, color: "var(--tx-3)", marginTop: 2 }}>
              {unavailable
                ? "Required-CLI checks run inside the Symphony desktop app."
                : `${okCount} of ${list.length} required CLIs detected · re-checked on launch`}
            </div>
          </div>
        </div>
        <Button variant="subtle" icon={Refresh} onClick={reRun}>
          Re-run preflight
        </Button>
      </div>

      {actionError ? (
        <div
          role="alert"
          style={{
            fontSize: 12.5,
            color: "var(--red)",
            background: "var(--red-soft)",
            border: "1px solid rgba(239,83,80,.3)",
            borderRadius: "var(--r-ctrl)",
            padding: "10px 14px",
          }}
        >
          {actionError}
        </div>
      ) : null}

      {/* Linear connection mirror */}
      <SectionCard title="Linear connection" icon={Linear} desc="Mirrors the account configured on the General tab.">
        <div style={{ display: "grid", gridTemplateColumns: ROW_GRID, gap: 18, alignItems: "center" }}>
          <div
            style={{
              width: 26,
              height: 26,
              borderRadius: "50%",
              display: "grid",
              placeItems: "center",
              background: account?.connected ? "var(--em-soft)" : "var(--bg-raised)",
              border: account?.connected ? "1px solid rgba(16,185,129,.3)" : "1px solid var(--line)",
              color: account?.connected ? "var(--em-bright)" : "var(--tx-3)",
            }}
          >
            {account?.connected ? <Check size={14} style={{ strokeWidth: 2.4 }} /> : <X size={14} />}
          </div>
          <div>
            <div style={{ fontSize: 14, fontWeight: 600 }}>Linear API</div>
            <div style={{ fontSize: 12, color: "var(--tx-3)", marginTop: 3 }}>
              {account?.connected ? (
                <>
                  Connected as <span style={{ color: "var(--tx-2)" }}>{account.name}</span>
                </>
              ) : (
                "Not connected"
              )}
            </div>
          </div>
          <div className="mono" style={{ fontSize: 12, color: "var(--tx-3)" }}>
            {account?.token}
          </div>
          <span style={{ justifySelf: "end", fontSize: 12, color: "var(--tx-faint)", display: "inline-flex", alignItems: "center", gap: 6 }}>
            <Check size={13} style={{ color: account?.connected ? "var(--em-bright)" : "var(--tx-faint)" }} />
            {account?.connected ? "Authenticated" : "Not authenticated"}
          </span>
        </div>
      </SectionCard>

      {/* Required CLIs */}
      <SectionCard title="Required CLIs" icon={Terminal} desc="Symphony shells out to these tools. Override a path if a binary isn't on your PATH.">
        <div style={{ background: "var(--bg-card-2)", border: "1px solid var(--line)", borderRadius: "var(--r-card)" }}>
          {displayList.length === 0 ? (
            <div style={{ padding: "28px 22px", textAlign: "center", color: "var(--tx-3)", fontSize: 13 }}>
              No CLIs detected. Launch the Symphony desktop app to run the preflight checks.
            </div>
          ) : (
            displayList.map((t, i) => (
              <ToolRow
                key={t.name}
                t={t}
                last={i === displayList.length - 1}
                onPickPath={() => pickPath(t.name)}
                onSetPath={(p) => applyOverride(t.name, p)}
                onAction={() => runAction(t.name)}
              />
            ))
          )}
        </div>
      </SectionCard>
    </div>
  );
}
