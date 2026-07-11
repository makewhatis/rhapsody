import { StatusDot } from "./status-dot";

export type StatusKey =
  | "running"
  | "idle"
  | "interrupted"
  | "paused"
  | "review"
  | "completed"
  | "stopped"
  | "continued"
  | "failed"
  | "queued"
  | "waiting";

export interface StatusMetaEntry {
  color: string;
  bg: string;
  label: string;
  pulse?: boolean;
}

// The full run/agent status palette. Job-level statuses (running/completed/stopped/failed) +
// queued/idle, the per-project status enum (paused/review — rendered by the Settings agents list,
// NOT run outcomes), and the detail-only segment chips (continued/interrupted). (taxonomy v2, INF-272)
export const STATUS_META: Record<StatusKey, StatusMetaEntry> = {
  running: { color: "var(--em-bright)", bg: "var(--em-soft)", label: "running", pulse: true },
  idle: { color: "var(--tx-2)", bg: "rgba(255,255,255,.05)", label: "idle" },
  // interrupted: a segment whose worker died mid-flight (e.g. the daemon restarted). Boot recovery
  // re-dispatches a NEW run for the issue if it's still active; this row is a frozen tombstone.
  // Neutral palette like idle, but labeled honestly so it doesn't read as "waiting/idle".
  interrupted: { color: "var(--tx-2)", bg: "rgba(255,255,255,.05)", label: "interrupted" },
  // continued: a continuation segment that cleanly handed the thread to the next segment. Neutral,
  // detail-only — it never pins a finished job to "running".
  continued: { color: "var(--tx-2)", bg: "rgba(255,255,255,.05)", label: "continued" },
  // paused/review are the per-project status enum (/api/v1/projects), not run outcomes; the
  // Settings agents list renders them, so they MUST stay in this palette.
  paused: { color: "var(--amber)", bg: "var(--amber-soft)", label: "paused" },
  review: { color: "var(--sky)", bg: "var(--sky-soft)", label: "in review" },
  completed: { color: "var(--em-bright)", bg: "var(--em-soft)", label: "completed" },
  // stopped is an operator-attention state (Stop, ticket cancelled, external wind-down): the amber
  // WARN palette — not an error (red), not nothing (neutral).
  stopped: { color: "var(--amber)", bg: "var(--amber-soft)", label: "stopped" },
  failed: { color: "var(--red)", bg: "var(--red-soft)", label: "failed" },
  queued: { color: "var(--tx-2)", bg: "rgba(255,255,255,.05)", label: "queued" },
  // waiting: a ticket held by an uncleared blockedBy predecessor under graphite/dag orchestration
  // (INF-318/INF-320). A benign "held, by design" state — the neutral sky palette (like review), NOT
  // an error red and NOT a pulsing/live treatment.
  waiting: { color: "var(--sky)", bg: "var(--sky-soft)", label: "waiting" },
};

export interface StatusChipProps {
  status: StatusKey | string;
  /** When set, renders "<count> <label>" (e.g. "3 running"). */
  count?: number;
  /** Override the displayed label. */
  label?: string;
}

// StatusChip — pill-shaped status badge with a (pulsing) dot. The package source tinted
// the border with `${color}22`, which is invalid CSS for a `var()` colour and was dropped
// by the browser; we honour the intent with a faint 13% color-mix tint instead.
export function StatusChip({ status, count, label }: StatusChipProps) {
  const m = STATUS_META[status as StatusKey] ?? STATUS_META.idle;
  return (
    <span
      style={{
        display: "inline-flex",
        alignItems: "center",
        gap: 6,
        height: 24,
        padding: "0 9px 0 8px",
        borderRadius: "var(--r-pill)",
        background: m.bg,
        color: m.color,
        fontSize: 11.5,
        fontWeight: 600,
        letterSpacing: "-0.005em",
        whiteSpace: "nowrap",
        border: `1px solid color-mix(in srgb, ${m.color} 13%, transparent)`,
      }}
    >
      <StatusDot color={m.color} pulse={m.pulse} size={6} />
      {count != null ? `${count} ${m.label}` : label || m.label}
    </span>
  );
}
