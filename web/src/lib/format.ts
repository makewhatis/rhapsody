// Pure formatting helpers. Deterministic given (now, value).

export function formatDuration(seconds: number): string {
  if (!Number.isFinite(seconds) || seconds < 0) seconds = 0;
  const s = Math.floor(seconds);
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const sec = s % 60;
  if (h > 0) return `${h}h ${m}m ${sec}s`;
  if (m > 0) return `${m}m ${sec}s`;
  return `${sec}s`;
}

// elapsedSeconds returns whole seconds between an RFC3339 timestamp and now (ms epoch).
export function elapsedSeconds(sinceISO: string, nowMs: number): number {
  const start = Date.parse(sinceISO);
  if (Number.isNaN(start)) return 0;
  return Math.max(0, Math.floor((nowMs - start) / 1000));
}

// countdownSeconds returns whole seconds until an RFC3339 due time (>= 0).
export function countdownSeconds(dueISO: string, nowMs: number): number {
  const due = Date.parse(dueISO);
  if (Number.isNaN(due)) return 0;
  return Math.max(0, Math.ceil((due - nowMs) / 1000));
}

export function formatTokens(n: number): string {
  if (!Number.isFinite(n)) return "0";
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}k`;
  return `${n}`;
}

const DASH = "—";

// runDuration returns a humanized run duration between two RFC3339 timestamps. While a
// run is still going (ended_at empty/invalid) it returns DASH so the table doesn't show a
// fabricated 0s. A negative/zero span (clock skew) clamps to "0s".
export function runDuration(startedISO: string, endedISO: string): string {
  const start = Date.parse(startedISO);
  const end = Date.parse(endedISO);
  if (Number.isNaN(start) || Number.isNaN(end)) return DASH;
  return formatDuration(Math.max(0, Math.floor((end - start) / 1000)));
}

// localTimeZoneLabel returns a short timezone label for `d` in the host's local
// zone (e.g. "EDT", "PST", "GMT+2"), respecting DST for that specific instant.
// When Intl can't supply a name it falls back to the numeric "UTC±HH:MM" offset
// so the label is never empty.
export function localTimeZoneLabel(d: Date): string {
  try {
    const tz = new Intl.DateTimeFormat(undefined, { timeZoneName: "short" })
      .formatToParts(d)
      .find((p) => p.type === "timeZoneName")?.value;
    if (tz) return tz;
  } catch {
    // Intl missing/unsupported — fall through to the numeric offset.
  }
  // getTimezoneOffset is minutes *behind* UTC, so negate to get minutes east of UTC.
  const offsetMin = -d.getTimezoneOffset();
  const sign = offsetMin >= 0 ? "+" : "-";
  const abs = Math.abs(offsetMin);
  const pad = (n: number) => String(n).padStart(2, "0");
  return `UTC${sign}${pad(Math.floor(abs / 60))}:${pad(abs % 60)}`;
}

// formatDateTime renders an RFC3339 timestamp as a compact local "MM/DD HH:MM TZ"
// label (e.g. "06/26 10:06 EDT"), in the viewer's local timezone with an explicit
// timezone suffix. Falls back to DASH for an empty/invalid value.
export function formatDateTime(iso: string): string {
  const ms = Date.parse(iso);
  if (Number.isNaN(ms)) return DASH;
  const d = new Date(ms);
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${pad(d.getMonth() + 1)}/${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())} ${localTimeZoneLabel(d)}`;
}

// outcomeVariant maps a run outcome to a badge color (taxonomy v2): emerald for `completed`,
// destructive for `failed`, secondary for in-flight `running`, muted otherwise
// (stopped | continued | interrupted | unknown).
export function outcomeVariant(
  outcome: string,
): "default" | "muted" | "destructive" | "secondary" {
  switch (outcome) {
    case "completed":
      return "default";
    case "failed":
      return "destructive";
    case "running":
      return "secondary";
    default: // stopped | continued | interrupted | unknown
      return "muted";
  }
}
