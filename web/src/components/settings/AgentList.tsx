import * as React from "react";
import {
  Boxes,
  Button,
  ChevronRight,
  Info,
  Linear,
  Plus,
  StatusChip,
  StatusDot,
  Toggle,
} from "@/components/ui";
import { agentSeats, effectiveModel, type UiAgent, type UiGlobal } from "@/lib/settings-model";

// The agent-list row/header share one grid template (mock 2b): repo · Linear project · model · on ·
// status · ›. The row leads with the repo (mono) — the agent's identifying handle in the table.
const GRID = "minmax(0,280px) minmax(0,1fr) 120px 64px 130px 24px";

// stripModel drops the "claude-" prefix for the compact model cell (matches the design).
const stripModel = (m: string) => m.replace("claude-", "");

function StatusChipFor({ agent }: { agent: UiAgent }) {
  return agent.status === "running" ? (
    <StatusChip status="running" count={agent.running} />
  ) : (
    <StatusChip status={agent.status} />
  );
}

function AgentRow({
  agent,
  global,
  onClick,
  onToggle,
}: {
  agent: UiAgent;
  global: UiGlobal;
  onClick: () => void;
  onToggle: (enabled: boolean) => void;
}) {
  const [hover, setHover] = React.useState(false);
  const overridden = agent.overrides.model !== undefined;
  return (
    <div
      onClick={onClick}
      onMouseEnter={() => setHover(true)}
      onMouseLeave={() => setHover(false)}
      style={{
        display: "grid",
        gridTemplateColumns: GRID,
        gap: 16,
        alignItems: "center",
        padding: "15px 20px",
        borderBottom: "1px solid var(--line-2)",
        cursor: "pointer",
        background: hover ? "var(--bg-hover)" : "transparent",
        transition: "background .1s",
      }}
    >
      <div
        className="mono"
        style={{
          fontSize: 12.5,
          color: agent.enabled ? "var(--tx)" : "var(--tx-2)",
          minWidth: 0,
          overflow: "hidden",
          textOverflow: "ellipsis",
          whiteSpace: "nowrap",
        }}
      >
        {agent.repoShort}
      </div>
      <div
        style={{
          fontSize: 12.5,
          color: "var(--tx-2)",
          overflow: "hidden",
          textOverflow: "ellipsis",
          whiteSpace: "nowrap",
        }}
      >
        {agent.projectName}
      </div>
      <div style={{ display: "flex", alignItems: "center", gap: 6 }}>
        <span
          className="mono"
          style={{ fontSize: 12, color: "var(--tx-2)", overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}
        >
          {stripModel(effectiveModel(agent, global))}
        </span>
        {overridden ? (
          <span
            title="Overridden"
            style={{ width: 5, height: 5, borderRadius: "50%", background: "var(--em-bright)", flexShrink: 0 }}
          />
        ) : null}
      </div>
      <div onClick={(e) => e.stopPropagation()}>
        <Toggle checked={agent.enabled} onChange={onToggle} size="sm" aria-label={`Enable ${agent.name}`} />
      </div>
      <div>
        <StatusChipFor agent={agent} />
      </div>
      <ChevronRight size={16} style={{ color: "var(--tx-faint)", justifySelf: "end" }} />
    </div>
  );
}

function MetaRow({ k, v, mono, override }: { k: string; v: string; mono?: boolean; override?: boolean }) {
  return (
    <div style={{ display: "flex", alignItems: "center", justifyContent: "space-between", gap: 12 }}>
      <span style={{ fontSize: 12, color: "var(--tx-3)" }}>{k}</span>
      <span
        className={mono ? "mono" : undefined}
        style={{
          fontSize: 12.5,
          color: "var(--tx-2)",
          display: "inline-flex",
          alignItems: "center",
          gap: 6,
          overflow: "hidden",
          textOverflow: "ellipsis",
          whiteSpace: "nowrap",
          maxWidth: 160,
        }}
      >
        {v}
        {override ? <span style={{ width: 5, height: 5, borderRadius: "50%", background: "var(--em-bright)" }} /> : null}
      </span>
    </div>
  );
}

function AgentCard({
  agent,
  global,
  onClick,
  onToggle,
}: {
  agent: UiAgent;
  global: UiGlobal;
  onClick: () => void;
  onToggle: (enabled: boolean) => void;
}) {
  const [hover, setHover] = React.useState(false);
  return (
    <div
      onClick={onClick}
      onMouseEnter={() => setHover(true)}
      onMouseLeave={() => setHover(false)}
      style={{
        background: "var(--bg-card)",
        border: `1px solid ${hover ? "var(--line-strong)" : "var(--line)"}`,
        borderRadius: "var(--r-card)",
        padding: 18,
        cursor: "pointer",
        transition: "border-color .14s, transform .14s",
        transform: hover ? "translateY(-2px)" : "none",
        display: "flex",
        flexDirection: "column",
        gap: 14,
        position: "relative",
        overflow: "hidden",
      }}
    >
      <span
        aria-hidden
        style={{ position: "absolute", left: 0, top: 0, bottom: 0, width: 3, background: agent.color, opacity: agent.enabled ? 1 : 0.35 }}
      />
      <div style={{ display: "flex", alignItems: "flex-start", justifyContent: "space-between", gap: 10 }}>
        <div style={{ display: "flex", alignItems: "center", gap: 10 }}>
          <StatusDot color={agent.color} size={10} pulse={agent.status === "running"} />
          <span style={{ fontSize: 14.5, fontWeight: 600, color: agent.enabled ? "var(--tx)" : "var(--tx-2)" }}>
            {agent.name}
          </span>
        </div>
        <StatusChipFor agent={agent} />
      </div>
      <div style={{ display: "flex", flexDirection: "column", gap: 8 }}>
        <MetaRow k="Project" v={agent.projectName} />
        <MetaRow k="Repo" v={agent.repoShort} mono />
        <MetaRow k="Model" v={stripModel(effectiveModel(agent, global))} mono override={agent.overrides.model !== undefined} />
      </div>
      <div
        style={{
          display: "flex",
          alignItems: "center",
          justifyContent: "space-between",
          paddingTop: 12,
          borderTop: "1px solid var(--line-2)",
        }}
      >
        <span style={{ fontSize: 12, color: "var(--tx-3)" }}>{agent.enabled ? "Enabled" : "Disabled"}</span>
        <div onClick={(e) => e.stopPropagation()}>
          <Toggle checked={agent.enabled} onChange={onToggle} size="sm" aria-label={`Enable ${agent.name}`} />
        </div>
      </div>
    </div>
  );
}

export interface AgentListProps {
  agents: UiAgent[];
  global: UiGlobal;
  listStyle?: "rows" | "cards";
  /** Select an agent by its index (stable across in-detail slug edits). */
  onSelect: (index: number) => void;
  onToggle: (index: number, enabled: boolean) => void;
  openSheet: () => void;
}

// AgentList — the agents header (counts + Add-agent) over either the canonical table-row layout
// or the card-grid alternate, ported from the design `projects.jsx`.
export function AgentList({ agents, global, listStyle = "rows", onSelect, onToggle, openSheet }: AgentListProps) {
  // Seat accounting (mock 2b): "N configured · M enabled · P of Q seats playing" + the open-seats
  // affordance count (Q − M, hidden at 0). Q is the global max-concurrent capacity.
  const seats = agentSeats(agents, global.maxConcurrent);
  return (
    <div style={{ display: "flex", flexDirection: "column", gap: 18 }}>
      <div style={{ display: "flex", alignItems: "center", justifyContent: "space-between" }}>
        <div style={{ display: "flex", alignItems: "baseline", gap: 10 }}>
          <h2 style={{ fontSize: 16, fontWeight: 600, letterSpacing: "-0.02em" }}>Agents</h2>
          <span style={{ fontSize: 13, color: "var(--tx-3)" }}>
            {seats.configured} configured · {seats.enabled} enabled ·{" "}
            <span style={{ color: "var(--rust-text)" }}>
              {seats.playing} of {seats.seats} seats playing
            </span>
          </span>
        </div>
        <Button variant="primary" icon={Plus} onClick={openSheet}>
          Add agent
        </Button>
      </div>
      {listStyle === "cards" ? (
        <div style={{ display: "grid", gridTemplateColumns: "repeat(2,1fr)", gap: 14 }}>
          {/* Key by index, not a.id: toUiAgent derives id from the project slug, which two agents can
              transiently share mid-edit (Save is blocked by duplicateSlugs but the list still renders).
              Rows never reorder — they're a 1:1 map of the agents array — so the index is stable. */}
          {agents.map((a, i) => (
            <AgentCard key={i} agent={a} global={global} onClick={() => onSelect(i)} onToggle={(v) => onToggle(i, v)} />
          ))}
        </div>
      ) : (
        <div style={{ background: "var(--bg-card)", border: "1px solid var(--line)", borderRadius: "var(--r-card)" }}>
          <div
            style={{
              display: "grid",
              gridTemplateColumns: GRID,
              gap: 16,
              padding: "11px 20px",
              borderBottom: "1px solid var(--line-2)",
              fontSize: 10.5,
              fontWeight: 600,
              letterSpacing: ".07em",
              textTransform: "uppercase",
              color: "var(--tx-faint)",
            }}
          >
            {["Repo", "Linear project", "Model", "On", "Status", ""].map((h, i) => (
              <div key={i}>{h}</div>
            ))}
          </div>
          {/* See the cards branch: key by index (not the slug-derived a.id) — duplicate slugs are
              transiently possible mid-edit and rows never reorder, so the index is the stable key. */}
          {agents.map((a, i) => (
            <AgentRow key={i} agent={a} global={global} onClick={() => onSelect(i)} onToggle={(v) => onToggle(i, v)} />
          ))}
        </div>
      )}
      {seats.open > 0 ? <OpenSeats open={seats.open} onAdd={openSheet} /> : null}
    </div>
  );
}

// OpenSeats — the dashed "N seats open" affordance under the agents table (mock 2b). Rendered only
// when there is unfilled capacity (open = maxConcurrent − enabled > 0). The whole row is the
// add-an-agent action (a button, for keyboard reach); the action half reads in rust.
function OpenSeats({ open, onAdd }: { open: number; onAdd: () => void }) {
  const [hover, setHover] = React.useState(false);
  return (
    <button
      type="button"
      onClick={onAdd}
      onMouseEnter={() => setHover(true)}
      onMouseLeave={() => setHover(false)}
      style={{
        width: "100%",
        display: "flex",
        alignItems: "center",
        justifyContent: "center",
        gap: 5,
        padding: "18px 22px",
        border: "1px dashed var(--hair-dashed)",
        borderRadius: "var(--r-card)",
        background: hover ? "rgba(255,255,255,.02)" : "transparent",
        cursor: "pointer",
        fontSize: 12,
        color: "var(--tx-3)",
        transition: "background .12s",
      }}
    >
      {open} {open === 1 ? "seat" : "seats"} open —{" "}
      <span style={{ color: "var(--rust-text)" }}>add an agent to fill them</span>
    </button>
  );
}

// EmptyState — the centered "no agents yet" card with the primary Add CTA + an inherit hint.
export function EmptyState({ openSheet }: { openSheet: () => void }) {
  return (
    <div style={{ background: "var(--bg-card)", border: "1px solid var(--line)", borderRadius: "var(--r-card)" }}>
      <div
        style={{
          display: "flex",
          flexDirection: "column",
          alignItems: "center",
          justifyContent: "center",
          textAlign: "center",
          padding: "84px 32px",
        }}
      >
        <div
          style={{
            width: 64,
            height: 64,
            borderRadius: 18,
            display: "grid",
            placeItems: "center",
            marginBottom: 22,
            background: "radial-gradient(circle at 50% 30%, var(--em-soft), transparent 70%)",
            border: "1px solid var(--line)",
            color: "var(--em-bright)",
          }}
        >
          <Boxes size={28} />
        </div>
        <h2 style={{ fontSize: 18, fontWeight: 600, letterSpacing: "-0.02em" }}>No agents yet</h2>
        <p style={{ fontSize: 13.5, color: "var(--tx-3)", maxWidth: 420, marginTop: 9, lineHeight: 1.6 }}>
          An agent watches one Linear project and runs autonomous coding agents on its tickets. Add your first to start
          scheduling work.
        </p>
        <div style={{ marginTop: 24, display: "flex", gap: 10 }}>
          <Button variant="primary" icon={Plus} onClick={openSheet}>
            Add your first agent
          </Button>
          <Button variant="ghost" icon={Linear} disabled comingSoon>
            Browse Linear projects
          </Button>
        </div>
        <div style={{ marginTop: 30, fontSize: 12, color: "var(--tx-faint)", display: "flex", alignItems: "center", gap: 7 }}>
          <Info size={13} />
          New agents inherit every global default from the General tab.
        </div>
      </div>
    </div>
  );
}
