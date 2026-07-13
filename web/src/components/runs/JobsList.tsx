import * as React from "react";
import { StatusChip } from "@/components/ui/status-chip";
import { StatusDot } from "@/components/ui/status-dot";
import { TextInput } from "@/components/ui/text-input";
import { Search } from "@/components/ui/icons";
import {
  filterCounts,
  JOB_FILTERS,
  matchFilter,
  searchJobs,
  type JobFilterId,
  type JobRow,
} from "@/lib/runs-model";

// Grid template shared by the column header and every data row (mock 1a):
// Issue · Agent · Status · Project · Turn · Tokens · Duration.
const COLS = "minmax(0,1fr) 104px 112px 92px 56px 84px 92px";
const HEADERS = ["Issue", "Agent", "Status", "Project", "Turn", "Tokens", "Duration"];

export interface JobsListProps {
  rows: JobRow[];
  /** Live poll cadence in ms, shown in the footer (from state.poll_interval_ms). */
  pollMs?: number;
  /** Max turns per run (global config), rendering the "N/max" turn cell. Omitted → bare "N". */
  maxTurns?: number;
  /** Whether the data is actively polling; when false (e.g. under the Wails host where the HTTP
   * poll is disabled) the footer shows "live updates paused" instead of falsely implying live. */
  polling?: boolean;
  onSelect: (runId: number) => void;
}

// JobsList — the Podium jobs view (mock 1a): a control row (title + segmented filter with live
// counts + ⌘K search), the dense 42px table with the 2px rust rule + tint on playing rows, and the
// sage "live · every 2s" footer. Client-side filter + search over the merged Live+History rows.
export function JobsList({ rows, pollMs, maxTurns, polling = true, onSelect }: JobsListProps) {
  const [filter, setFilter] = React.useState<JobFilterId>("all");
  const [q, setQ] = React.useState("");
  const searchRef = React.useRef<HTMLInputElement>(null);

  // ⌘K (⌃K on non-mac) focuses the search field from anywhere in the view.
  React.useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "k") {
        e.preventDefault();
        searchRef.current?.focus();
      }
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, []);

  const counts = filterCounts(rows);
  const playing = counts.running;
  const visible = searchJobs(
    rows.filter((r) => matchFilter(r, filter)),
    q,
  );
  const pollSecs = pollMs ? Math.round(pollMs / 1000) : null;

  return (
    <div style={{ minWidth: 0 }}>
      {/* control row: title + segmented filter + search */}
      <div
        style={{
          height: 46,
          padding: "0 20px",
          display: "flex",
          alignItems: "center",
          justifyContent: "space-between",
          gap: 16,
          borderBottom: "1px solid var(--hair-section)",
        }}
      >
        <div style={{ display: "flex", alignItems: "center", gap: 14, minWidth: 0 }}>
          <span style={{ fontSize: 13, fontWeight: 600 }}>Jobs</span>
          <div
            style={{
              display: "inline-flex",
              gap: 2,
              padding: 2,
              borderRadius: "var(--r-ctrl)",
              background: "rgba(255,255,255,.04)",
              border: "1px solid var(--hair-section)",
            }}
          >
            {JOB_FILTERS.map((f) => {
              const active = filter === f.id;
              // The Playing count is always rust; other counts follow the chip's active/inactive ink.
              const countColor =
                f.id === "running" ? "var(--rust-text)" : active ? "var(--ink)" : "var(--faint)";
              return (
                <button
                  key={f.id}
                  type="button"
                  onClick={() => setFilter(f.id)}
                  style={{
                    padding: "3px 9px",
                    borderRadius: "var(--r-chip)",
                    border: "none",
                    cursor: "pointer",
                    fontSize: 11.5,
                    fontWeight: 500,
                    background: active ? "rgba(255,255,255,.09)" : "transparent",
                    color: active ? "var(--ink)" : "var(--muted)",
                    transition: "background .12s, color .12s",
                  }}
                >
                  {f.label}{" "}
                  <span className="mono" data-filter-count={f.id} style={{ fontSize: 10.5, color: countColor }}>
                    {counts[f.id]}
                  </span>
                </button>
              );
            })}
          </div>
        </div>
        <div style={{ width: 230, flexShrink: 0 }}>
          <TextInput
            ref={searchRef}
            placeholder="Search jobs…"
            value={q}
            prefixIcon={Search}
            onChange={(e) => setQ(e.target.value)}
            suffix={
              <span
                className="mono"
                style={{
                  fontSize: 9.5,
                  padding: "1px 4px",
                  borderRadius: "var(--r-keycap)",
                  border: "1px solid var(--hair-strong)",
                  color: "var(--faint)",
                }}
              >
                ⌘K
              </span>
            }
            style={{ height: 29 }}
          />
        </div>
      </div>

      {/* column header */}
      <div
        style={{
          display: "grid",
          gridTemplateColumns: COLS,
          gap: 14,
          padding: "9px 20px",
          borderBottom: "1px solid var(--hair-row)",
          fontSize: 10,
          fontWeight: 600,
          letterSpacing: ".12em",
          textTransform: "uppercase",
          color: "var(--faint)",
        }}
      >
        {HEADERS.map((h, i) => (
          <div key={h} style={{ textAlign: i >= 4 ? "right" : "left" }}>
            {h}
          </div>
        ))}
      </div>

      {/* rows */}
      <div>
        {visible.length === 0 ? (
          <div style={{ padding: "48px 0", textAlign: "center", color: "var(--faint)", fontSize: 13 }}>
            No jobs match these filters.
          </div>
        ) : (
          visible.map((r) => <RunRow key={r.key} r={r} maxTurns={maxTurns} onSelect={onSelect} />)
        )}
      </div>

      {/* footer */}
      <div
        style={{
          height: 34,
          padding: "0 20px",
          borderTop: "1px solid var(--hair-card)",
          background: "var(--list-footer)",
          display: "flex",
          justifyContent: "space-between",
          alignItems: "center",
        }}
      >
        <span style={{ fontSize: 11, color: "var(--faint)" }}>
          {rows.length} job{rows.length === 1 ? "" : "s"} — {playing} playing
        </span>
        {polling ? (
          <span
            data-live-indicator
            style={{ fontSize: 11, color: "var(--faint)", display: "inline-flex", alignItems: "center", gap: 6 }}
          >
            <StatusDot color="var(--sage)" pulse size={5} />
            <span className="mono">{pollSecs ? `live · every ${pollSecs}s` : "live"}</span>
          </span>
        ) : (
          <span data-live-indicator className="mono" style={{ fontSize: 11, color: "var(--faint)" }}>
            live updates paused
          </span>
        )}
      </div>
    </div>
  );
}

interface RunRowProps {
  r: JobRow;
  maxTurns?: number;
  onSelect: (runId: number) => void;
}

// EM_DASH — empty/"—" cell value (faint), for a never-run held row's turn/tokens/duration.
const EM_DASH = "—";

function RunRow({ r, maxTurns, onSelect }: RunRowProps) {
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
        gap: 14,
        height: 42,
        alignItems: "center",
        // 2px rust rule + tint on a playing row; a transparent 2px rule on the rest keeps the text
        // baseline aligned (18px pad + 2px border == the 20px gutter either way).
        paddingLeft: 18,
        paddingRight: 20,
        borderLeftWidth: 2,
        borderLeftStyle: "solid",
        borderLeftColor: r.live ? "var(--rust)" : "transparent",
        borderBottom: "1px solid var(--hair-row)",
        backgroundColor: r.live
          ? "var(--tint-playing-row)"
          : hover && clickable
            ? "rgba(255,255,255,.03)"
            : "transparent",
        cursor: clickable ? "pointer" : "default",
        transition: "background-color .12s",
      }}
    >
      {/* Issue: key + title inline on one baseline row. */}
      <div style={{ display: "flex", alignItems: "baseline", gap: 9, minWidth: 0 }}>
        <span className="mono" style={{ fontSize: 12, fontWeight: 600, color: "var(--ink)", flexShrink: 0 }}>
          {r.issue}
        </span>
        <span
          style={{
            fontSize: 12,
            color: "var(--muted)",
            overflow: "hidden",
            textOverflow: "ellipsis",
            whiteSpace: "nowrap",
            minWidth: 0,
          }}
        >
          {r.title}
        </span>
      </div>
      {/* Agent */}
      <div
        style={{
          fontSize: 12,
          color: "var(--muted)",
          overflow: "hidden",
          textOverflow: "ellipsis",
          whiteSpace: "nowrap",
        }}
      >
        {r.agent}
      </div>
      {/* Status pill (+ failure reason / waiting-on sub-label) */}
      <div style={{ display: "flex", flexDirection: "column", alignItems: "flex-start", gap: 3, minWidth: 0 }}>
        <StatusChip status={r.status} />
        {r.subLabel ? (
          <span
            style={{
              fontSize: 11,
              color: "var(--faint)",
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
      {/* Project */}
      <div
        style={{
          fontSize: 12,
          color: "var(--muted)",
          overflow: "hidden",
          textOverflow: "ellipsis",
          whiteSpace: "nowrap",
        }}
      >
        {r.projectShort}
      </div>
      {/* Turn: "N" ink + "/max" faint (em-dash when the job never ran). */}
      <div className="mono" style={{ fontSize: 11.5, textAlign: "right" }}>
        {r.turn > 0 ? (
          <>
            <span style={{ color: "var(--ink)" }}>{r.turn}</span>
            {maxTurns ? <span style={{ color: "var(--faint)" }}>/{maxTurns}</span> : null}
          </>
        ) : (
          <span style={{ color: "var(--faint)" }}>{EM_DASH}</span>
        )}
      </div>
      {/* Tokens */}
      <div
        className="mono"
        style={{ fontSize: 11.5, color: r.tokens && r.tokens !== "0" ? "var(--muted)" : "var(--faint)", textAlign: "right" }}
      >
        {r.tokens && r.tokens !== "0" ? r.tokens : EM_DASH}
      </div>
      {/* Duration (rust while live) */}
      <div
        className="mono"
        style={{
          fontSize: 11.5,
          color: r.duration ? (r.durationAccent ? "var(--rust-text)" : "var(--muted)") : "var(--faint)",
          textAlign: "right",
        }}
      >
        {r.duration || EM_DASH}
      </div>
    </div>
  );
}
