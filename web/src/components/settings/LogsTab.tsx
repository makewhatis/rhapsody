import * as React from "react";
import { Button, ScrollText, StatusDot, Trash } from "@/components/ui";
import { useLogStream, type LogLine, type LogStreamStatus } from "@/hooks/useLogStream";
import { useFollowScroll } from "@/hooks/useFollowScroll";
import { LOG_LEVEL_FILTERS, logLinePasses, type LogLevelFilter } from "@/lib/settings-model";

// Level → gutter color + display label (mock 2d: INF #6E675E / WRN #CDA35A / ERR #E0574C). Unknown
// levels fall back to INFO. These are presentational; the level-ordering that drives the filter lives
// in settings-model (logLinePasses) so it is unit-tested there.
const LEVELS: Record<string, { color: string; label: string }> = {
  ERROR: { color: "var(--red)", label: "ERR" },
  WARN: { color: "var(--amber)", label: "WRN" },
  INFO: { color: "var(--faint)", label: "INF" },
  DEBUG: { color: "var(--ghost)", label: "DBG" },
};

function levelMeta(level: string) {
  return LEVELS[level.trim().toUpperCase()] ?? LEVELS.INFO;
}

// Stream connection → the status line's dot color + label (mock 2d: "sage pulse dot + live · N lines").
const STATUS_META: Record<LogStreamStatus, { color: string; label: string; live: boolean }> = {
  open: { color: "var(--sage)", label: "live", live: true },
  connecting: { color: "var(--amber)", label: "connecting…", live: false },
  closed: { color: "var(--red)", label: "unavailable", live: false },
};

// fmtTime renders the record time as HH:MM:SS (local), or "" if unparseable.
function fmtTime(rfc: string): string {
  const d = new Date(rfc);
  if (Number.isNaN(d.getTime())) return "";
  return d.toTimeString().slice(0, 8);
}

// LogRow — one console line (mock 2d): a `66px 34px 1fr` grid of time · level · message, with the
// attrs rendered as dim key=value pairs after the verb. A WARN row carries the amber row tint.
function LogRow({ line }: { line: LogLine }) {
  const m = levelMeta(line.level);
  const time = fmtTime(line.time);
  const attrs = line.attrs ? Object.entries(line.attrs) : [];
  const warn = line.level.trim().toUpperCase() === "WARN";
  return (
    <div
      style={{
        display: "grid",
        gridTemplateColumns: "66px 34px 1fr",
        gap: 12,
        padding: "4px 16px",
        lineHeight: 1.7,
        alignItems: "baseline",
        background: warn ? "var(--tint-warn-row)" : "transparent",
      }}
    >
      <span style={{ color: "var(--ghost)" }}>{time}</span>
      <span style={{ color: m.color, fontWeight: 600, letterSpacing: ".04em" }}>{m.label}</span>
      <span style={{ color: "var(--text-2)", whiteSpace: "nowrap", overflow: "hidden", textOverflow: "ellipsis", minWidth: 0 }}>
        {line.msg}
        {attrs.length > 0 ? (
          <span style={{ color: "var(--ghost)" }}>
            {attrs.map(([k, v]) => (
              <span key={k} style={{ marginLeft: 10 }}>
                {k}={v}
              </span>
            ))}
          </span>
        ) : null}
      </span>
    </div>
  );
}

// LogsTab — a live console for the daemon's own process log (the slog stream, mock 2d). A segmented
// level filter (All/Info+/Warn+/Error) + Clear sit beside a status line (live · N lines); the console
// follows the tail while pinned to the bottom (shared follow hook), pausing on an upward scroll or the
// explicit "pause follow ⏸", and resuming via "jump to latest ↓". Read-only: the tail is sourced by
// useLogStream, which tails SSE directly in a browser and over a Tauri IPC channel in the packaged app
// (the buffered custom-protocol proxy can't forward an infinite SSE stream — TRA-252).
export function LogsTab() {
  const { lines, status, clear } = useLogStream();
  const [filter, setFilter] = React.useState<LogLevelFilter>("all");
  const scrollRef = React.useRef<HTMLDivElement>(null);

  const visible = React.useMemo(() => lines.filter((l) => logLinePasses(l.level, filter)), [lines, filter]);

  // Follow mode (shared with the D4 transcript): auto-pin to the tail as lines stream in; an upward
  // scroll or the "pause follow" button releases it (revealing "jump to latest ↓"), and scrolling
  // back to the bottom or clicking jump resumes. Re-pins whenever the visible line count changes.
  const follow = useFollowScroll(scrollRef, visible.length);

  const s = STATUS_META[status];

  return (
    <div style={{ display: "flex", flexDirection: "column", gap: 14 }}>
      {/* control row: status line + level filter + clear */}
      <div style={{ display: "flex", alignItems: "center", justifyContent: "space-between", gap: 12 }}>
        <span style={{ display: "inline-flex", alignItems: "center", gap: 8, fontSize: 12.5, color: "var(--text-2)" }}>
          <StatusDot color={s.color} size={7} pulse={s.live} />
          {s.label}
          <span style={{ color: "var(--faint)" }}>·</span>
          <span className="mono" style={{ color: "var(--faint)" }}>
            {visible.length} line{visible.length === 1 ? "" : "s"}
          </span>
        </span>
        <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
          <div
            role="tablist"
            aria-label="Log level filter"
            style={{
              display: "inline-flex",
              gap: 2,
              padding: 2,
              borderRadius: "var(--r-ctrl)",
              background: "rgba(255,255,255,.04)",
              border: "1px solid var(--hair-section)",
            }}
          >
            {LOG_LEVEL_FILTERS.map((f) => {
              const active = filter === f.id;
              return (
                <button
                  key={f.id}
                  type="button"
                  role="tab"
                  aria-selected={active}
                  onClick={() => setFilter(f.id)}
                  style={{
                    padding: "3px 9px",
                    borderRadius: "var(--r-chip)",
                    border: "none",
                    cursor: "pointer",
                    fontSize: 11.5,
                    fontWeight: 500,
                    background: active ? "rgba(255,255,255,.09)" : "transparent",
                    color: active ? "var(--ink)" : "var(--tx-2)",
                    transition: "background .12s, color .12s",
                  }}
                >
                  {f.label}
                </button>
              );
            })}
          </div>
          <Button variant="subtle" size="sm" icon={Trash} onClick={clear}>
            Clear
          </Button>
        </div>
      </div>

      {/* console panel */}
      <div
        style={{
          border: "1px solid var(--hair-card)",
          borderRadius: "var(--r-card)",
          background: "var(--well)",
          overflow: "hidden",
        }}
      >
        <div
          ref={scrollRef}
          data-testid="log-console"
          className="mono"
          onScroll={follow.onScroll}
          style={{ height: "min(520px, 58vh)", overflowY: "auto", fontSize: 11, padding: "8px 0" }}
        >
          {visible.length === 0 ? (
            <div
              style={{
                display: "flex",
                flexDirection: "column",
                alignItems: "center",
                gap: 8,
                padding: "56px 22px",
                color: "var(--faint)",
                textAlign: "center",
              }}
            >
              <ScrollText size={22} style={{ opacity: 0.6 }} />
              <span style={{ fontSize: 13 }}>
                {status === "closed"
                  ? "The daemon log stream isn't available. Is the daemon running?"
                  : lines.length === 0
                    ? status === "connecting"
                      ? "Connecting to the daemon…"
                      : "Waiting for the daemon to log…"
                    : "No lines match this level filter."}
              </span>
            </div>
          ) : (
            // Key on the client-assigned uid, not seq: seq resets each daemon process, so after a
            // restart old and new lines would collide on key={l.seq}. uid is unique across restarts.
            visible.map((l) => <LogRow key={l.uid ?? l.seq} line={l} />)
          )}
        </div>
        {/* follow footer */}
        <div
          style={{
            height: 32,
            display: "flex",
            alignItems: "center",
            justifyContent: "space-between",
            gap: 12,
            padding: "0 16px",
            background: "var(--well-footer)",
            borderTop: "1px solid var(--hair-section)",
          }}
        >
          <span className="mono" style={{ fontSize: 10.5, color: "var(--faint)" }}>
            {follow.following ? "following — newest at bottom" : "paused — scrolled up"}
          </span>
          <button
            type="button"
            onClick={follow.following ? follow.pause : follow.jumpToLatest}
            className="mono"
            style={{ background: "none", border: "none", cursor: "pointer", fontSize: 10.5, color: "var(--rust-text)", padding: 0 }}
          >
            {follow.following ? "pause follow ⏸" : "jump to latest ↓"}
          </button>
        </div>
      </div>
    </div>
  );
}
