import * as React from "react";
import {
  Button,
  CheckCircle,
  Linear,
  RotateCcw,
  SectionCard,
  SkeletonCard,
  StatusDot,
  Terminal,
  TextInput,
} from "@/components/ui";
import { pickFile, setToolOverride, type ToolResult } from "@/lib/bindings";
import { useLinearIdentity } from "@/hooks/useConfig";
import { useToolDoctor } from "@/hooks/useToolDoctor";
import { useNow } from "@/hooks/useNow";
import { preflightAgeLabel, toolRowState, type ToolRowState } from "@/lib/settings-model";

// The tail the "Not found on PATH" copy shares with the summon flow — the warning message on a
// missing-binary row (mock 2c). A CLI Rhapsody shells out to that isn't on PATH breaks the PR checks
// and the @rhapsody summon re-engage, so the row spells out the consequence rather than a bare error.
const NOT_FOUND_MESSAGE = "Not found on PATH — PR checks and summons will fail";

// Static sub-labels the design shows beside a binary name (the daemon's ToolResult has no `sub`).
const SUB: Record<string, string> = { gt: "Graphite", git: "system" };

// A row's amber border, derived from the amber token so the warning "Set path…" affordance stays in
// the D1 palette (no hardcoded rgba) — mirrors TextInput's color-mix invalid border.
const AMBER_BORDER = "color-mix(in srgb, var(--amber) 35%, transparent)";

// ToolRow — one required-CLI preflight row (mock 2c): a status dot + binary name, a version/detail
// line (or the amber "not found" warning), an editable path-override field, and an Override…/Set
// path… action. Healthy rows read sage; a missing OR unhealthy binary reads amber (row tint + dot +
// message). The path field is editable so an override can be typed/pasted even where the native file
// picker isn't wired (the daemon-served browser has no PickFile binding), and re-syncs from a fresh
// probe only while the user isn't editing it.
function ToolRow({
  t,
  last,
  onPickPath,
  onSetPath,
}: {
  t: ToolResult;
  last: boolean;
  onPickPath: () => void;
  onSetPath: (path: string) => void;
}) {
  const state: ToolRowState = toolRowState(t);
  const warn = state === "warn";
  const sub = SUB[t.name];
  const [path, setPath] = React.useState(t.path);
  const [focused, setFocused] = React.useState(false);
  // Sync the field from a probe result only when the user ISN'T editing it — otherwise a refetch
  // (triggered by another row's override) would clobber an in-progress edit.
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
        gridTemplateColumns: "minmax(150px,1.1fr) minmax(190px,1.6fr) auto",
        gap: 16,
        alignItems: "center",
        padding: "13px 16px",
        background: warn ? "var(--tint-warn-row)" : "transparent",
        borderBottom: last ? "none" : "1px solid var(--hair-section)",
      }}
    >
      {/* status dot + name + version/message line */}
      <div style={{ display: "flex", alignItems: "flex-start", gap: 10, minWidth: 0 }}>
        <span style={{ display: "inline-flex", marginTop: 4 }}>
          <StatusDot color={warn ? "var(--amber)" : "var(--sage)"} size={7} />
        </span>
        <div style={{ minWidth: 0 }}>
          <div style={{ display: "flex", alignItems: "baseline", gap: 8 }}>
            <span className="mono" style={{ fontSize: 12, fontWeight: 600, color: "var(--ink)" }}>
              {t.name}
            </span>
            {sub ? <span style={{ fontSize: 11, color: "var(--faint)" }}>{sub}</span> : null}
          </div>
          {warn ? (
            <div style={{ fontSize: 11.5, color: "var(--amber)", marginTop: 3 }}>
              {t.found ? t.detail || "Needs attention" : NOT_FOUND_MESSAGE}
            </div>
          ) : (
            <div
              className="mono"
              style={{ fontSize: 11.5, color: "var(--faint)", marginTop: 3, display: "flex", gap: 7 }}
            >
              {t.version ? <span style={{ color: "var(--text-muted)" }}>v{t.version}</span> : null}
              {t.version && t.detail ? <span>·</span> : null}
              {t.detail ? <span>{t.detail}</span> : null}
            </div>
          )}
        </div>
      </div>
      {/* editable path override */}
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
        style={{ height: 32, fontSize: 11.5 }}
      />
      {/* override action — a plain path override is the mock's universal remediation (2c) */}
      <div style={{ display: "flex", justifyContent: "flex-end" }}>
        <Button
          variant={warn ? "subtle" : "ghost"}
          size="sm"
          aria-label={`${warn ? "Set" : "Override"} ${t.name} path`}
          onClick={onPickPath}
          style={warn ? { borderColor: AMBER_BORDER, color: "var(--amber)" } : undefined}
        >
          {warn ? "Set path…" : "Override…"}
        </Button>
      </div>
    </div>
  );
}

// ToolsTab — the app-side preflight/doctor panel (mock 2c). It runs the Go `probeTools` binding (NOT
// the daemon) via the shared useToolDoctor query, so the Settings rail's amber warning dot and this
// tab derive off the same result. It shows the last-probe age + a "Re-run preflight" action, a
// read-only mirror of the Linear connection configured on General, and the required-CLI rows with the
// amber warning state for a binary missing from PATH.
export function ToolsTab() {
  const doctor = useToolDoctor();
  const identity = useLinearIdentity();
  const now = useNow(30_000);
  const list = doctor.data ?? [];
  const account = identity.data;

  const [actionError, setActionError] = React.useState<string | null>(null);

  const applyOverride = (name: string, path: string) => {
    setActionError(null);
    void setToolOverride(name, path)
      .then(() => doctor.refetch())
      .catch((e: unknown) => {
        // The Go binding rejects a path that isn't an executable. Surface it and re-probe so the
        // field snaps back to the actual detected path rather than implying the override stuck.
        setActionError(e instanceof Error ? e.message : `Couldn't set the ${name} path.`);
        void doctor.refetch();
      });
  };
  const pickPath = (name: string) => {
    // A CLI override is a path to the executable FILE, so use the file chooser (not a folder one).
    void pickFile(`Choose ${name} executable`).then((path) => {
      if (path) applyOverride(name, path);
    });
  };
  const reRun = () => {
    setActionError(null);
    void doctor.refetch();
  };

  // "preflight ran Xm ago" (mock 2c). dataUpdatedAt is 0 until the first successful probe; ageLabel is
  // null until then, so the header reads "running preflight…" during the initial launch probe.
  const ageLabel = preflightAgeLabel(doctor.dataUpdatedAt, now);
  const headerText =
    doctor.isError && !doctor.data ? "preflight failed" : ageLabel ? `preflight ran ${ageLabel}` : "running preflight…";

  if (doctor.isLoading && !doctor.data) {
    return (
      <div style={{ display: "flex", flexDirection: "column", gap: 18 }}>
        <SkeletonCard />
        <SkeletonCard />
      </div>
    );
  }

  return (
    <div style={{ display: "flex", flexDirection: "column", gap: 18 }}>
      {/* preflight header: last-run age + Re-run */}
      <div style={{ display: "flex", alignItems: "center", justifyContent: "flex-end", gap: 12 }}>
        <span className="mono" style={{ fontSize: 10.5, color: "var(--faint)" }}>
          {headerText}
        </span>
        <Button variant="subtle" size="sm" icon={RotateCcw} onClick={reRun}>
          Re-run preflight
        </Button>
      </div>

      {actionError ? (
        <div
          role="alert"
          style={{
            fontSize: 12.5,
            color: "var(--red)",
            background: "var(--tint-red)",
            border: "1px solid var(--border-danger)",
            borderRadius: "var(--r-ctrl)",
            padding: "10px 14px",
          }}
        >
          {actionError}
        </div>
      ) : null}

      {/* Linear connection mirror — read-only reflection of the General-tab account */}
      <SectionCard title="Linear connection" icon={Linear} desc="Mirrors the account configured on the General tab.">
        <div style={{ display: "flex", alignItems: "center", gap: 14 }}>
          <div
            style={{
              width: 26,
              height: 26,
              borderRadius: "50%",
              display: "grid",
              placeItems: "center",
              background: account?.connected ? "var(--tint-sage)" : "rgba(255,255,255,.03)",
              border: account?.connected ? "1px solid color-mix(in srgb, var(--sage) 30%, transparent)" : "1px solid var(--hair-card)",
              color: account?.connected ? "var(--sage)" : "var(--faint)",
            }}
          >
            <CheckCircle size={14} />
          </div>
          <div style={{ flex: 1, minWidth: 0 }}>
            <div style={{ fontSize: 13, fontWeight: 600 }}>Linear API</div>
            <div style={{ fontSize: 12, color: "var(--text-muted)", marginTop: 2 }}>
              {account?.connected ? (
                <>
                  Connected as <span style={{ color: "var(--text-2)" }}>{account.name}</span>
                </>
              ) : (
                "Not connected"
              )}
            </div>
          </div>
          {account?.token ? (
            <span className="mono" style={{ fontSize: 12, color: "var(--faint)" }}>
              {account.token}
            </span>
          ) : null}
          <span
            style={{
              justifySelf: "end",
              fontSize: 11.5,
              color: account?.connected ? "var(--sage)" : "var(--faint)",
              display: "inline-flex",
              alignItems: "center",
              gap: 5,
            }}
          >
            <CheckCircle size={13} />
            {account?.connected ? "Authenticated" : "Not authenticated"}
          </span>
        </div>
      </SectionCard>

      {/* Required CLIs */}
      <SectionCard
        title="Required CLIs"
        icon={Terminal}
        desc="Rhapsody shells out to these tools. Override a path if a binary isn't on your PATH."
      >
        <div style={{ border: "1px solid var(--hair-card)", borderRadius: "var(--r-card)", overflow: "hidden" }}>
          {list.length === 0 ? (
            <div style={{ padding: "26px 16px", textAlign: "center", color: "var(--faint)", fontSize: 12.5 }}>
              No required CLIs detected.
            </div>
          ) : (
            list.map((t, i) => (
              <ToolRow
                key={t.name}
                t={t}
                last={i === list.length - 1}
                onPickPath={() => pickPath(t.name)}
                onSetPath={(p) => applyOverride(t.name, p)}
              />
            ))
          )}
        </div>
      </SectionCard>
    </div>
  );
}
