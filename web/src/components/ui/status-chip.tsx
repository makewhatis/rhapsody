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

// The full run/agent status palette, re-tokened for Podium (P10-D1). Job-level statuses
// (running/completed/stopped/failed) + queued/idle, the per-project status enum
// (paused/review — rendered by the Settings agents list, NOT run outcomes), and the
// detail-only segment chips (continued/interrupted).
//
// DISPLAY-LABEL MAPPING: the run enum value stays "running" everywhere (API, state,
// `/api/v1`); only the human-facing chip reads "playing" (the light orchestral flavor of
// the reskin). Enum keys here are the contract; `label` is display-only.
//
// Color assignment follows the spec token table:
//   rust  → playing (live)          slate → waiting / in review / continued
//   amber → stopped/paused/interrupted (warnings)   sage → completed (success)
//   red   → failed                  neutral → idle / queued
export const STATUS_META: Record<StatusKey, StatusMetaEntry> = {
  running: { color: "var(--rust-text)", bg: "var(--tint-rust)", label: "playing", pulse: true },
  idle: { color: "var(--neutral)", bg: "var(--tint-neutral)", label: "idle" },
  // interrupted: a segment whose worker died mid-flight (e.g. the daemon restarted). Boot
  // recovery re-dispatches a NEW run for the issue if it's still active; this row is a frozen
  // tombstone. The amber warn palette — an operator-attention state, not idle.
  interrupted: { color: "var(--amber)", bg: "var(--tint-amber)", label: "interrupted" },
  // continued: a continuation segment that cleanly handed the thread to the next segment.
  // Benign slate (held/handed, by design) — it never pins a finished job to "playing".
  continued: { color: "var(--slate)", bg: "var(--tint-slate)", label: "continued" },
  // paused/review are the per-project status enum (/api/v1/projects), not run outcomes; the
  // Settings agents list renders them, so they MUST stay in this palette.
  paused: { color: "var(--amber)", bg: "var(--tint-amber)", label: "paused" },
  review: { color: "var(--slate)", bg: "var(--tint-slate)", label: "in review" },
  completed: { color: "var(--sage)", bg: "var(--tint-sage)", label: "completed" },
  // stopped is an operator-attention state (Stop, ticket cancelled, external wind-down): the
  // amber WARN palette — not an error (red), not nothing (neutral).
  stopped: { color: "var(--amber)", bg: "var(--tint-amber)", label: "stopped" },
  failed: { color: "var(--red)", bg: "var(--tint-red)", label: "failed" },
  queued: { color: "var(--neutral)", bg: "var(--tint-neutral)", label: "queued" },
  // waiting: a ticket held by an uncleared blockedBy predecessor under graphite/dag
  // orchestration (INF-318/INF-320). A benign "held, by design" state — the slate palette
  // (like review), NOT an error red and NOT a pulsing/live treatment.
  waiting: { color: "var(--slate)", bg: "var(--tint-slate)", label: "waiting" },
};

export interface StatusChipProps {
  status: StatusKey | string;
  /** When set, renders "<count> <label>" (e.g. "3 playing"). */
  count?: number;
  /** Override the displayed label. */
  label?: string;
}

// StatusChip — pill-shaped status badge with a (pulsing) 5px dot (Podium: 11px, 10–12% tint,
// hairline border tinted from the status color).
export function StatusChip({ status, count, label }: StatusChipProps) {
  const m = STATUS_META[status as StatusKey] ?? STATUS_META.idle;
  return (
    <span
      style={{
        display: "inline-flex",
        alignItems: "center",
        gap: 6,
        height: 22,
        padding: "0 9px 0 8px",
        borderRadius: "var(--r-pill)",
        background: m.bg,
        color: m.color,
        fontSize: 11,
        fontWeight: 600,
        whiteSpace: "nowrap",
        border: `1px solid color-mix(in srgb, ${m.color} 22%, transparent)`,
      }}
    >
      <StatusDot color={m.color} pulse={m.pulse} size={5} />
      {count != null ? `${count} ${m.label}` : label || m.label}
    </span>
  );
}
