import { StatusDot } from "@/components/ui/status-dot";
import type { DaySummary, RunSummary, StateResponse } from "@/lib/api";
import { deriveStatTiles, rhythmBars, type StatTile } from "@/lib/runs-model";

export interface RunsStatTilesProps {
  state: StateResponse | undefined;
  /** Daemon-computed totals for today, over the WHOLE store — never a fold over a page (TRA-320). */
  summary: DaySummary | undefined;
  /** The issue-level rows, consulted only for the Playing cell's store-running fallback. */
  rows: RunSummary[];
  /** Max concurrent agents (the seat capacity) → the Playing cell's "of N seats" annotation. */
  maxConcurrent: number;
  /** Whether the data is live (polling). When false (e.g. under the Wails host) the Playing pulse
   *  dot stops breathing so the strip doesn't imply live updates while the queries are idle. */
  live?: boolean;
}

// RunsStatTiles — the "instrument strip": a full-width band of four hairline-separated cells
// (Playing · Completed · Tokens today + rhythm sparkline · Runtime today), derived from the live
// snapshot + the daemon's day summary. Replaces the former 4-up card row. (P10-D3 / mock 1a)
export function RunsStatTiles({ state, summary, rows, maxConcurrent, live = true }: RunsStatTilesProps) {
  const cells = deriveStatTiles(state, summary, rows, maxConcurrent);
  const bars = rhythmBars(summary);
  return (
    <div
      style={{
        display: "grid",
        gridTemplateColumns: "1fr 1fr 1.2fr 1.2fr",
        padding: "16px 0",
        borderBottom: "1px solid var(--hair-section)",
      }}
    >
      {cells.map((cell, i) => (
        <StripCell
          key={cell.key}
          cell={cell}
          first={i === 0}
          live={live}
          bars={cell.key === "tokens" ? bars : undefined}
        />
      ))}
    </div>
  );
}

// StripCell — one instrument cell: a caps label over a value row (big tabular numeral + a faint
// annotation). The live "Playing" cell leads with a pulsing rust dot and colours its numeral rust;
// the Tokens cell right-aligns the rhythm sparkline.
function StripCell({
  cell,
  first,
  live,
  bars,
}: {
  cell: StatTile;
  first: boolean;
  live: boolean;
  bars?: number[];
}) {
  return (
    <div
      style={{
        padding: "0 20px",
        borderLeft: first ? undefined : "1px solid var(--hair-section)",
        minWidth: 0,
      }}
    >
      <div
        style={{
          fontSize: 10,
          fontWeight: 600,
          letterSpacing: ".12em",
          textTransform: "uppercase",
          color: "var(--faint)",
          marginBottom: 7,
        }}
      >
        {cell.label}
      </div>
      <div style={{ display: "flex", alignItems: "center", gap: 8, minWidth: 0 }}>
        {cell.accent ? <StatusDot color="var(--rust-text)" pulse={live} size={7} /> : null}
        <span
          className="mono"
          style={{
            fontSize: 22,
            fontWeight: 600,
            lineHeight: 1,
            letterSpacing: "-0.02em",
            color: cell.accent ?? "var(--ink)",
            flexShrink: 0,
          }}
        >
          {cell.value}
        </span>
        {cell.sub ? (
          <span
            style={{
              fontSize: 11,
              color: "var(--faint)",
              overflow: "hidden",
              textOverflow: "ellipsis",
              whiteSpace: "nowrap",
              minWidth: 0,
            }}
          >
            {cell.sub}
          </span>
        ) : null}
        {bars && bars.length > 0 ? <RhythmSparkline bars={bars} /> : null}
      </div>
    </div>
  );
}

// RhythmSparkline — the token-rhythm bars in the Tokens-today cell: 2px-wide bars, 2px gap, rust
// on an opacity ramp (.3 oldest → .85 newest) with the most-recent bar full rust-text. Purely
// decorative (aria-hidden); the numbers already carry the meaning. (mock 1a)
function RhythmSparkline({ bars }: { bars: number[] }) {
  const H = 22;
  const n = bars.length;
  return (
    <span
      data-rhythm="true"
      aria-hidden
      style={{ display: "inline-flex", alignItems: "flex-end", gap: 2, height: H, marginLeft: "auto", flexShrink: 0 }}
    >
      {bars.map((b, i) => {
        const last = i === n - 1;
        const t = n <= 1 ? 1 : i / (n - 1);
        return (
          <span
            key={i}
            style={{
              width: 2,
              height: Math.max(2, Math.round(b * H)),
              borderRadius: 1,
              background: last ? "var(--rust-text)" : "var(--rust)",
              opacity: last ? 1 : 0.3 + 0.55 * t,
            }}
          />
        );
      })}
    </span>
  );
}
