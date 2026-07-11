import { StatusDot } from "@/components/ui/status-dot";
import type { RunSummary, StateResponse } from "@/lib/api";
import { deriveStatTiles } from "@/lib/runs-model";
import { Panel } from "./Panel";

export interface StatTileProps {
  label: string;
  value: string;
  sub?: string;
  /** Accent colour for the value + a leading dot (e.g. the live "Running" tile). */
  accent?: string;
  /** Pulse the accent dot (live count). */
  pulse?: boolean;
}

// StatTile — one summary tile (label, big mono value, sub). Ported from `runs.jsx` StatTile.
export function StatTile({ label, value, sub, accent, pulse }: StatTileProps) {
  return (
    <Panel style={{ padding: "16px 18px", display: "flex", flexDirection: "column", gap: 6 }}>
      <div style={{ display: "flex", alignItems: "center", gap: 7 }}>
        {accent ? <StatusDot color={accent} pulse={pulse} size={7} /> : null}
        <span
          style={{
            fontSize: 11,
            fontWeight: 600,
            letterSpacing: ".07em",
            textTransform: "uppercase",
            color: "var(--tx-3)",
          }}
        >
          {label}
        </span>
      </div>
      <div
        className="mono"
        style={{
          fontSize: 27,
          fontWeight: 600,
          letterSpacing: "-0.02em",
          color: accent ?? "var(--tx)",
          lineHeight: 1,
        }}
      >
        {value}
      </div>
      {sub ? <div style={{ fontSize: 11.5, color: "var(--tx-3)" }}>{sub}</div> : null}
    </Panel>
  );
}

export interface RunsStatTilesProps {
  state: StateResponse | undefined;
  history: RunSummary[];
  nowMs: number;
  /** Whether the data is live (polling). When false (e.g. under the Wails host) the accent dot
   * stops pulsing so the tiles don't imply live updates while the queries are idle. */
  live?: boolean;
}

// RunsStatTiles — the 4-up tile row (Running / In review / Tokens today / Runtime today),
// derived from the live state snapshot + history rollup. Replaces the legacy TotalsCards.
export function RunsStatTiles({ state, history, nowMs, live = true }: RunsStatTilesProps) {
  const tiles = deriveStatTiles(state, history, nowMs);
  return (
    <div style={{ display: "grid", gridTemplateColumns: "repeat(4, 1fr)", gap: 14 }}>
      {tiles.map((t) => (
        <StatTile key={t.key} label={t.label} value={t.value} sub={t.sub} accent={t.accent} pulse={t.pulse && live} />
      ))}
    </div>
  );
}
