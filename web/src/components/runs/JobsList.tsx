import * as React from "react";
import { StatusChip } from "@/components/ui/status-chip";
import { StatusDot } from "@/components/ui/status-dot";
import { TextInput } from "@/components/ui/text-input";
import { Search } from "@/components/ui/icons";
import { JOB_FILTERS, matchFilter, searchJobs, type JobFilterId, type JobRow } from "@/lib/runs-model";
import { Panel } from "./Panel";

// Grid template shared by the column header and every row, ported from `runs.jsx`.
const COLS = "minmax(220px,2.1fr) 150px 116px minmax(120px,1fr) 86px 86px 100px";

export interface JobsListProps {
  rows: JobRow[];
  /** Live poll cadence in ms, shown in the footer (from state.poll_interval_ms). */
  pollMs?: number;
  /** Whether the data is actively polling; when false (e.g. under the Wails host where the HTTP
   * poll is disabled) the footer hides the "live" indicator instead of falsely implying it. */
  polling?: boolean;
  onSelect: (runId: number) => void;
}

// JobsList — the unified Live + History jobs list: segmented filter, search, columns, and the
// live polling footer. Replaces the legacy SessionsTable + HistoryTable. Ported from the
// `runs.jsx` RunsView jobs section.
export function JobsList({ rows, pollMs, polling = true, onSelect }: JobsListProps) {
  const [filter, setFilter] = React.useState<JobFilterId>("all");
  const [q, setQ] = React.useState("");

  const visible = searchJobs(
    rows.filter((r) => matchFilter(r, filter)),
    q,
  );
  const pollSecs = pollMs ? Math.round(pollMs / 1000) : null;

  return (
    <Panel style={{ padding: 0 }}>
      {/* header: title + segmented filter + search */}
      <div
        style={{
          display: "flex",
          alignItems: "center",
          justifyContent: "space-between",
          gap: 16,
          padding: "16px 20px",
          borderBottom: "1px solid var(--line-2)",
        }}
      >
        <div style={{ display: "flex", alignItems: "center", gap: 12 }}>
          <span style={{ fontSize: 14.5, fontWeight: 600, letterSpacing: "-0.01em" }}>Jobs</span>
          <div
            style={{
              display: "flex",
              gap: 4,
              background: "var(--bg-input)",
              borderRadius: 9,
              padding: 3,
              border: "1px solid var(--line)",
            }}
          >
            {JOB_FILTERS.map((f) => (
              <button
                key={f.id}
                type="button"
                onClick={() => setFilter(f.id)}
                style={{
                  height: 28,
                  padding: "0 11px",
                  borderRadius: 7,
                  border: "none",
                  cursor: "pointer",
                  fontSize: 12.5,
                  fontWeight: 500,
                  background: filter === f.id ? "var(--bg-active)" : "transparent",
                  color: filter === f.id ? "var(--tx)" : "var(--tx-3)",
                  transition: "all .12s",
                }}
              >
                {f.id === "running" ? (
                  <span style={{ display: "inline-flex", alignItems: "center", gap: 6 }}>
                    <StatusDot color="var(--em-bright)" pulse={polling} size={6} />
                    {f.label}
                  </span>
                ) : (
                  f.label
                )}
              </button>
            ))}
          </div>
        </div>
        <div style={{ width: 240 }}>
          <TextInput
            placeholder="Search jobs…"
            value={q}
            prefixIcon={Search}
            onChange={(e) => setQ(e.target.value)}
            style={{ height: 34 }}
          />
        </div>
      </div>

      {/* column header */}
      <div
        style={{
          display: "grid",
          gridTemplateColumns: COLS,
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
        {["Issue", "Agent", "Status", "Project", "Turn", "Tokens", "Duration"].map((h, i) => (
          <div key={h} style={{ textAlign: i >= 4 ? "right" : "left" }}>
            {h}
          </div>
        ))}
      </div>

      {/* rows */}
      <div>
        {visible.length === 0 ? (
          <div style={{ padding: "48px 0", textAlign: "center", color: "var(--tx-3)", fontSize: 13 }}>
            No jobs match these filters.
          </div>
        ) : (
          visible.map((r) => <RunRow key={r.key} r={r} onSelect={onSelect} />)
        )}
      </div>

      {/* footer */}
      <div
        style={{
          padding: "12px 20px",
          borderTop: "1px solid var(--line-2)",
          display: "flex",
          justifyContent: "space-between",
          alignItems: "center",
        }}
      >
        <span style={{ fontSize: 12, color: "var(--tx-3)" }}>
          {visible.length} of {rows.length} jobs
        </span>
        {polling ? (
          <span
            style={{
              fontSize: 11.5,
              color: "var(--tx-faint)",
              display: "inline-flex",
              alignItems: "center",
              gap: 6,
            }}
          >
            <StatusDot color="var(--em-bright)" pulse size={6} />
            {pollSecs ? `live · polling every ${pollSecs}s` : "live"}
          </span>
        ) : (
          <span style={{ fontSize: 11.5, color: "var(--tx-faint)" }}>live updates paused</span>
        )}
      </div>
    </Panel>
  );
}

interface RunRowProps {
  r: JobRow;
  onSelect: (runId: number) => void;
}

function RunRow({ r, onSelect }: RunRowProps) {
  const [hover, setHover] = React.useState(false);
  const clickable = r.runId > 0;
  return (
    <div
      data-live={r.live ? "true" : undefined}
      onClick={clickable ? () => onSelect(r.runId) : undefined}
      onMouseEnter={() => setHover(true)}
      onMouseLeave={() => setHover(false)}
      style={{
        display: "grid",
        gridTemplateColumns: COLS,
        gap: 16,
        padding: "13px 20px",
        alignItems: "center",
        borderBottom: "1px solid var(--line-2)",
        cursor: clickable ? "pointer" : "default",
        background: hover && clickable ? "var(--bg-hover)" : "transparent",
        transition: "background .1s",
        position: "relative",
      }}
    >
      {r.live ? (
        <span
          data-accent-bar="true"
          style={{
            position: "absolute",
            left: 0,
            top: 8,
            bottom: 8,
            width: 2,
            borderRadius: 2,
            background: "var(--em-bright)",
          }}
        />
      ) : null}
      <div style={{ minWidth: 0 }}>
        <div
          className="mono"
          style={{ fontSize: 13, fontWeight: 600, color: "var(--tx)", letterSpacing: "-0.01em" }}
        >
          {r.issue}
        </div>
        <div
          style={{
            fontSize: 12.5,
            color: "var(--tx-3)",
            marginTop: 2,
            overflow: "hidden",
            textOverflow: "ellipsis",
            whiteSpace: "nowrap",
          }}
        >
          {r.title}
        </div>
      </div>
      <div style={{ display: "flex", alignItems: "center", gap: 8, minWidth: 0 }}>
        <StatusDot color={r.agentColor} size={8} />
        <span
          style={{
            fontSize: 12.5,
            color: "var(--tx-2)",
            overflow: "hidden",
            textOverflow: "ellipsis",
            whiteSpace: "nowrap",
          }}
        >
          {r.agent}
        </span>
      </div>
      <div style={{ display: "flex", flexDirection: "column", alignItems: "flex-start", gap: 3, minWidth: 0 }}>
        <StatusChip status={r.status} />
        {/* Failed jobs surface the reason inline (e.g. "turn timeout", "stalled") so a failure is
            identifiable without opening the run. */}
        {r.subLabel ? (
          <span
            style={{
              fontSize: 11,
              color: "var(--tx-3)",
              maxWidth: "100%",
              overflow: "hidden",
              textOverflow: "ellipsis",
              whiteSpace: "nowrap",
            }}
          >
            {r.subLabel}
          </span>
        ) : null}
      </div>
      <div
        className="mono"
        style={{
          fontSize: 12,
          color: "var(--tx-3)",
          overflow: "hidden",
          textOverflow: "ellipsis",
          whiteSpace: "nowrap",
        }}
      >
        {r.projectShort}
      </div>
      <div className="mono" style={{ fontSize: 12.5, color: "var(--tx-2)", textAlign: "right" }}>
        {r.turn}
      </div>
      <div className="mono" style={{ fontSize: 12.5, color: "var(--tx-2)", textAlign: "right" }}>
        {r.tokens}
      </div>
      <div
        className="mono"
        style={{
          fontSize: 12.5,
          color: r.durationAccent ? "var(--em-bright)" : "var(--tx-2)",
          textAlign: "right",
        }}
      >
        {r.duration}
      </div>
    </div>
  );
}
