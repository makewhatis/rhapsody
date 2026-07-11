import * as React from "react";
import { Button, ChevronDown, Git, Info, Pill, Plus, Search, StatusDot, TextInput, X } from "@/components/ui";
import type { LinearProject } from "@/lib/api";
import { NEW_AGENT_CAP, type UiGlobal } from "@/lib/settings-model";

function ProjectPicker({
  value,
  onChange,
  projects,
  usedSlugs,
}: {
  value: string;
  onChange: (slug: string) => void;
  projects: LinearProject[];
  /** Slugs already watched by another agent — excluded so a duplicate can't be picked. */
  usedSlugs: string[];
}) {
  const [q, setQ] = React.useState("");
  const [open, setOpen] = React.useState(false);
  const sel = projects.find((p) => p.slug === value);
  // Only offer projects not already configured (the daemon requires globally-unique slugs).
  const available = projects.filter((p) => !usedSlugs.includes(p.slug));
  const results = available.filter((p) => `${p.name} ${p.slug} ${p.team}`.toLowerCase().includes(q.toLowerCase()));

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
          <span style={{ display: "flex", alignItems: "center", gap: 10 }}>
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
            maxHeight: 280,
            overflowY: "auto",
            animation: "fadeUp .14s ease-out",
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

function InheritItem({ label, value, last }: { label: string; value: React.ReactNode; last?: boolean }) {
  return (
    <div
      style={{
        display: "flex",
        alignItems: "center",
        justifyContent: "space-between",
        padding: last ? "9px 0 0" : "9px 0",
        borderBottom: last ? "none" : "1px solid var(--line-2)",
      }}
    >
      <span style={{ fontSize: 12.5, color: "var(--tx-3)" }}>{label}</span>
      <span className="mono" style={{ fontSize: 12.5, color: "var(--tx-2)" }}>
        {value}
      </span>
    </div>
  );
}

export interface AddAgentSheetProps {
  open: boolean;
  onClose: () => void;
  /** Create the agent: the chosen Linear project + the entered repo URL. */
  onCreate: (project: LinearProject, repo: string) => void;
  projects: LinearProject[];
  /** Slugs already configured on other agents — excluded from the picker (unique-slug rule). */
  usedSlugs: string[];
  /** A pre-existing config validation error (e.g. review-promote) that blocks creating — the parent
   *  would no-op the create otherwise. When set, Create is disabled and the reason is shown. */
  blockedReason?: string | null;
  global: UiGlobal;
}

// AddAgentSheet — the guided slide-in for adding an agent: a searchable Linear-project picker, a
// repo field, an inherits-from-global preview, and a Create button gated on (project set AND
// repo > 4 chars). Ported from the design `sheet.jsx`; closes on Esc + overlay click and resets
// its fields each time it opens.
export function AddAgentSheet({ open, onClose, onCreate, projects, usedSlugs, blockedReason, global }: AddAgentSheetProps) {
  const [proj, setProj] = React.useState("");
  const [repo, setRepo] = React.useState("");

  React.useEffect(() => {
    if (open) {
      setProj("");
      setRepo("");
    }
  }, [open]);

  React.useEffect(() => {
    const h = (e: KeyboardEvent) => {
      if (e.key === "Escape" && open) onClose();
    };
    window.addEventListener("keydown", h);
    return () => window.removeEventListener("keydown", h);
  }, [open, onClose]);

  if (!open) return null;
  const sel = projects.find((p) => p.slug === proj);
  // Gate on the RESOLVED project (not just a slug string): if the picked slug is no longer in the
  // list, Create stays disabled rather than enabled-but-inert.
  const canCreate = !!sel && repo.trim().length > 4 && !blockedReason;

  return (
    <div style={{ position: "fixed", inset: 0, zIndex: 200, display: "flex", justifyContent: "flex-end" }}>
      <div
        data-testid="sheet-overlay"
        onClick={onClose}
        style={{ position: "absolute", inset: 0, background: "rgba(0,0,0,.55)", backdropFilter: "blur(2px)", animation: "overlayIn .2s ease-out" }}
      />
      <div
        role="dialog"
        aria-label="Add agent"
        style={{
          position: "relative",
          width: 500,
          maxWidth: "92%",
          height: "100%",
          background: "var(--bg-app)",
          borderLeft: "1px solid var(--line-strong)",
          boxShadow: "var(--shadow-sheet)",
          display: "flex",
          flexDirection: "column",
          animation: "sheetIn .26s cubic-bezier(.2,.7,.2,1)",
        }}
      >
        {/* header */}
        <div
          style={{
            display: "flex",
            alignItems: "flex-start",
            justifyContent: "space-between",
            gap: 12,
            padding: "22px 26px 18px",
            borderBottom: "1px solid var(--line-2)",
          }}
        >
          <div>
            <div style={{ display: "flex", alignItems: "center", gap: 9 }}>
              <div
                style={{
                  width: 28,
                  height: 28,
                  borderRadius: 8,
                  display: "grid",
                  placeItems: "center",
                  background: "var(--em-soft)",
                  color: "var(--em-bright)",
                  border: "1px solid rgba(16,185,129,.25)",
                }}
              >
                <Plus size={16} />
              </div>
              <h2 style={{ fontSize: 17, fontWeight: 600, letterSpacing: "-0.02em" }}>Add agent</h2>
            </div>
            <p style={{ fontSize: 12.5, color: "var(--tx-3)", marginTop: 7, lineHeight: 1.5 }}>
              Point an agent at a Linear project and a repo. Everything else inherits your global defaults.
            </p>
          </div>
          <button
            type="button"
            aria-label="Close"
            onClick={onClose}
            style={{ background: "transparent", border: "none", color: "var(--tx-3)", cursor: "pointer", display: "flex", padding: 4, borderRadius: 6 }}
          >
            <X size={18} />
          </button>
        </div>

        {/* body */}
        <div style={{ flex: 1, overflowY: "auto", padding: "24px 26px", display: "flex", flexDirection: "column", gap: 24 }}>
          <div style={{ display: "flex", flexDirection: "column", gap: 8 }}>
            <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
              <span
                style={{
                  width: 18,
                  height: 18,
                  borderRadius: "50%",
                  background: "var(--em-soft)",
                  color: "var(--em-bright)",
                  fontSize: 11,
                  fontWeight: 700,
                  display: "grid",
                  placeItems: "center",
                }}
              >
                1
              </span>
              <label style={{ fontSize: 13.5, fontWeight: 600 }}>Linear project</label>
            </div>
            <div style={{ fontSize: 12, color: "var(--tx-3)", marginBottom: 2 }}>The project whose tickets this agent works.</div>
            <ProjectPicker value={proj} onChange={setProj} projects={projects} usedSlugs={usedSlugs} />
          </div>

          <div style={{ display: "flex", flexDirection: "column", gap: 8 }}>
            <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
              <span
                style={{
                  width: 18,
                  height: 18,
                  borderRadius: "50%",
                  background: proj ? "var(--em-soft)" : "rgba(255,255,255,.05)",
                  color: proj ? "var(--em-bright)" : "var(--tx-faint)",
                  fontSize: 11,
                  fontWeight: 700,
                  display: "grid",
                  placeItems: "center",
                  transition: "all .2s",
                }}
              >
                2
              </span>
              <label style={{ fontSize: 13.5, fontWeight: 600, color: proj ? "var(--tx)" : "var(--tx-3)" }}>Repository</label>
            </div>
            <div style={{ fontSize: 12, color: "var(--tx-3)", marginBottom: 2 }}>Git URL cloned fresh for every run.</div>
            <TextInput
              value={repo}
              mono
              prefixIcon={Git}
              placeholder="git@github.com:org/repo.git"
              onChange={(e) => setRepo(e.target.value)}
              style={{ height: 44 }}
            />
          </div>

          {/* inherits-from-global preview */}
          <div style={{ background: "var(--bg-card)", border: "1px solid var(--line)", borderRadius: "var(--r-ctrl)", padding: "14px 16px" }}>
            <div style={{ display: "flex", alignItems: "center", gap: 7, marginBottom: 6 }}>
              <Info size={14} style={{ color: "var(--tx-3)" }} />
              <span style={{ fontSize: 12.5, fontWeight: 600, color: "var(--tx-2)" }}>Inherits from global defaults</span>
              <Pill tone="neutral" style={{ marginLeft: "auto" }}>
                Customize later
              </Pill>
            </div>
            <InheritItem label="Model" value={global.model} />
            <InheritItem label="Effort" value={global.effort} />
            <InheritItem label="Permission mode" value={global.permission} />
            <InheritItem label="Per-agent cap" value={NEW_AGENT_CAP} last />
          </div>
        </div>

        {/* footer */}
        <div
          style={{
            display: "flex",
            alignItems: "center",
            justifyContent: "flex-end",
            gap: 10,
            padding: "16px 26px",
            borderTop: "1px solid var(--line-2)",
            background: "var(--bg-titlebar)",
          }}
        >
          {blockedReason ? (
            <span style={{ fontSize: 12, color: "var(--red)", marginRight: "auto", maxWidth: 280 }}>{blockedReason}</span>
          ) : null}
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button variant="primary" icon={Plus} disabled={!canCreate} onClick={() => sel && onCreate(sel, repo.trim())}>
            Create agent
          </Button>
        </div>
      </div>
    </div>
  );
}
