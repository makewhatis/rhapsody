import * as React from "react";
import { Button, ScrollText, StatusDot, Trash } from "@/components/ui";
import { useLogStream, type LogLine, type LogStreamStatus } from "@/hooks/useLogStream";

// Level → colour + display label for the gutter chip. Unknown levels fall back to INFO.
const LEVELS: Record<string, { color: string; label: string }> = {
  ERROR: { color: "var(--red)", label: "ERR" },
  WARN: { color: "var(--amber)", label: "WRN" },
  INFO: { color: "var(--em-bright)", label: "INF" },
  DEBUG: { color: "var(--tx-faint)", label: "DBG" },
};

function levelMeta(level: string) {
  return LEVELS[level.toUpperCase()] ?? LEVELS.INFO;
}

type LevelFilter = "all" | "info" | "warn" | "error";

const FILTERS: { id: LevelFilter; label: string }[] = [
  { id: "all", label: "All" },
  { id: "info", label: "Info+" },
  { id: "warn", label: "Warn+" },
  { id: "error", label: "Error" },
];

// rank orders levels so a "warn+" filter keeps WARN and ERROR, etc. Unknown → INFO rank.
function rank(level: string): number {
  switch (level.toUpperCase()) {
    case "DEBUG":
      return 0;
    case "WARN":
      return 2;
    case "ERROR":
      return 3;
    default:
      return 1; // INFO
  }
}

const MIN_RANK: Record<LevelFilter, number> = { all: -1, info: 1, warn: 2, error: 3 };

function passes(line: LogLine, f: LevelFilter): boolean {
  return rank(line.level) >= MIN_RANK[f];
}

// fmtTime renders the record time as HH:MM:SS (local), or "" if unparseable.
function fmtTime(rfc: string): string {
  const d = new Date(rfc);
  if (Number.isNaN(d.getTime())) return "";
  return d.toTimeString().slice(0, 8);
}

const STATUS_META: Record<LogStreamStatus, { color: string; label: string }> = {
  open: { color: "var(--em-bright)", label: "Live" },
  connecting: { color: "var(--amber)", label: "Connecting…" },
  closed: { color: "var(--red)", label: "Unavailable" },
};

function LogRow({ line }: { line: LogLine }) {
  const m = levelMeta(line.level);
  const time = fmtTime(line.time);
  const attrs = line.attrs ? Object.entries(line.attrs) : [];
  return (
    <div
      style={{
        display: "flex",
        gap: 12,
        padding: "3px 16px",
        alignItems: "baseline",
        lineHeight: 1.55,
        borderBottom: "1px solid rgba(255,255,255,.025)",
      }}
    >
      <span style={{ color: "var(--tx-faint)", fontSize: 11.5, flexShrink: 0, width: 60 }}>{time}</span>
      <span
        style={{ color: m.color, fontSize: 10.5, fontWeight: 700, flexShrink: 0, width: 30, letterSpacing: ".04em" }}
      >
        {m.label}
      </span>
      <span style={{ color: "var(--tx)", whiteSpace: "pre-wrap", wordBreak: "break-word", minWidth: 0, flex: 1 }}>
        {line.msg}
        {attrs.length > 0 ? (
          <span style={{ color: "var(--tx-3)" }}>
            {attrs.map(([k, v]) => (
              <span key={k} style={{ marginLeft: 10 }}>
                <span style={{ color: "var(--tx-faint)" }}>{k}=</span>
                {v}
              </span>
            ))}
          </span>
        ) : null}
      </span>
    </div>
  );
}

// LogsTab — a live console for the daemon's own process log (the slog stream), tailing
// GET /api/v1/logs/stream over SSE. It auto-scrolls while pinned to the bottom, pauses
// auto-scroll when the user scrolls up to read history, and offers a level filter + clear.
// shouldStickToBottom decides whether the console should follow the tail when content changes:
// true on the first render (prevHeight 0) or when the viewport was within STICK_THRESHOLD of the
// bottom BEFORE the new lines appended — measured against the PREVIOUS content height so the
// just-appended line doesn't itself read as "scrolled up", and WITHOUT depending on the async
// scroll event (which lags a fast stream and would otherwise yank a reader who scrolled up).
export function shouldStickToBottom(scrollTop: number, clientHeight: number, prevHeight: number): boolean {
  const STICK_THRESHOLD = 32;
  return prevHeight === 0 || scrollTop + clientHeight >= prevHeight - STICK_THRESHOLD;
}

// Read-only: unlike ToolsTab it needs no Wails bridge — the relative SSE URL reaches the
// daemon in both the desktop app (via the proxy) and a plain browser.
export function LogsTab() {
  const { lines, status, clear } = useLogStream();
  const [filter, setFilter] = React.useState<LevelFilter>("all");
  const scrollRef = React.useRef<HTMLDivElement>(null);
  // Console content height at the last commit, so the auto-scroll decision is "was the user at the
  // bottom BEFORE these lines appended" (see shouldStickToBottom). 0 = first render → start pinned.
  const prevHeightRef = React.useRef(0);

  const visible = React.useMemo(() => lines.filter((l) => passes(l, filter)), [lines, filter]);

  // Follow the tail only while the user is at (or near) the bottom; once they scroll up to read
  // earlier output, leave their position alone. No dependency array: this runs on every commit
  // (streaming lines, filter changes, status updates) and reads the live DOM, so a line arriving
  // mid-scroll can't yank the reader down via a stale ref.
  React.useLayoutEffect(() => {
    const el = scrollRef.current;
    if (!el) return;
    if (shouldStickToBottom(el.scrollTop, el.clientHeight, prevHeightRef.current)) {
      el.scrollTop = el.scrollHeight;
    }
    prevHeightRef.current = el.scrollHeight;
  });

  const s = STATUS_META[status];

  return (
    <div style={{ display: "flex", flexDirection: "column", gap: 14 }}>
      {/* toolbar: status + level filter + clear */}
      <div style={{ display: "flex", alignItems: "center", justifyContent: "space-between", gap: 12 }}>
        <span style={{ display: "inline-flex", alignItems: "center", gap: 8, fontSize: 12.5, color: "var(--tx-2)" }}>
          <StatusDot color={s.color} size={7} pulse={status === "open"} />
          {s.label}
          <span style={{ color: "var(--tx-faint)" }}>·</span>
          <span className="mono" style={{ color: "var(--tx-3)" }}>
            {visible.length} line{visible.length === 1 ? "" : "s"}
          </span>
        </span>
        <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
          <div
            role="tablist"
            aria-label="Log level filter"
            style={{ display: "flex", gap: 2, background: "var(--bg-raised)", borderRadius: 8, padding: 3 }}
          >
            {FILTERS.map((f) => (
              <button
                key={f.id}
                type="button"
                role="tab"
                aria-selected={filter === f.id}
                onClick={() => setFilter(f.id)}
                style={{
                  border: "none",
                  cursor: "pointer",
                  borderRadius: 6,
                  padding: "4px 10px",
                  fontSize: 12,
                  fontWeight: 500,
                  background: filter === f.id ? "var(--em-soft)" : "transparent",
                  color: filter === f.id ? "var(--em-bright)" : "var(--tx-3)",
                }}
              >
                {f.label}
              </button>
            ))}
          </div>
          <Button variant="subtle" size="sm" icon={Trash} onClick={clear}>
            Clear
          </Button>
        </div>
      </div>

      {/* console */}
      <div
        ref={scrollRef}
        data-testid="log-console"
        className="mono"
        style={{
          height: "min(560px, 60vh)",
          overflowY: "auto",
          background: "var(--bg-card-2)",
          border: "1px solid var(--line)",
          borderRadius: "var(--r-card)",
          fontSize: 12.5,
          padding: "8px 0",
        }}
      >
        {visible.length === 0 ? (
          <div
            style={{
              display: "flex",
              flexDirection: "column",
              alignItems: "center",
              gap: 8,
              padding: "56px 22px",
              color: "var(--tx-3)",
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
    </div>
  );
}
