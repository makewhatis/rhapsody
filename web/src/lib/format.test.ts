import { afterEach, describe, expect, it, vi } from "vitest";
import {
  countdownSeconds,
  elapsedSeconds,
  formatDateTime,
  formatDuration,
  formatTokens,
  localTimeZoneLabel,
  outcomeVariant,
  runDuration,
} from "@/lib/format";
import { historyQuery } from "@/lib/api";

// Fixed reference instant used as the injected "now" for all time math.
const NOW = Date.parse("2026-05-29T17:04:10Z"); // ms epoch

describe("formatDuration", () => {
  it("formats seconds, minutes, hours", () => {
    expect(formatDuration(0)).toBe("0s");
    expect(formatDuration(45)).toBe("45s");
    expect(formatDuration(90)).toBe("1m 30s");
    expect(formatDuration(3661)).toBe("1h 1m 1s");
  });
  it("clamps negative / non-finite to 0s", () => {
    expect(formatDuration(-5)).toBe("0s");
    expect(formatDuration(Number.NaN)).toBe("0s");
  });
});

describe("elapsedSeconds", () => {
  it("returns whole seconds since an RFC3339 start, floored", () => {
    // started 70.5s before NOW -> floor = 70
    expect(elapsedSeconds("2026-05-29T17:02:59.500Z", NOW)).toBe(70);
  });
  it("never goes negative for a future start", () => {
    expect(elapsedSeconds("2026-05-29T17:05:00Z", NOW)).toBe(0);
  });
  it("returns 0 for an unparseable timestamp", () => {
    expect(elapsedSeconds("not-a-date", NOW)).toBe(0);
  });
});

describe("countdownSeconds", () => {
  it("returns whole seconds until an RFC3339 due time, ceiled", () => {
    // due 49.2s after NOW -> ceil = 50
    expect(countdownSeconds("2026-05-29T17:04:59.200Z", NOW)).toBe(50);
  });
  it("returns 0 once due time has passed", () => {
    expect(countdownSeconds("2026-05-29T17:00:00Z", NOW)).toBe(0);
  });
  it("returns 0 for an unparseable timestamp", () => {
    expect(countdownSeconds("nope", NOW)).toBe(0);
  });
});

describe("formatTokens", () => {
  it("formats raw, thousands, and millions", () => {
    expect(formatTokens(0)).toBe("0");
    expect(formatTokens(950)).toBe("950");
    expect(formatTokens(2000)).toBe("2.0k");
    expect(formatTokens(1_500_000)).toBe("1.5M");
  });
  it("returns 0 for non-finite input", () => {
    expect(formatTokens(Number.NaN)).toBe("0");
  });
});

describe("runDuration", () => {
  it("formats the span between started_at and ended_at", () => {
    expect(runDuration("2026-05-29T17:00:00Z", "2026-05-29T17:01:30Z")).toBe("1m 30s");
  });
  it("returns DASH while a run is still going (no ended_at)", () => {
    expect(runDuration("2026-05-29T17:00:00Z", "")).toBe("—");
  });
  it("clamps a negative span (clock skew) to 0s", () => {
    expect(runDuration("2026-05-29T17:01:00Z", "2026-05-29T17:00:00Z")).toBe("0s");
  });
});

describe("formatDateTime", () => {
  afterEach(() => {
    vi.restoreAllMocks();
  });

  it("returns DASH for an empty or unparseable timestamp", () => {
    expect(formatDateTime("")).toBe("—");
    expect(formatDateTime("not-a-date")).toBe("—");
  });

  it("renders the local MM/DD HH:MM and appends a timezone label", () => {
    const iso = "2026-06-26T14:06:00Z";
    const d = new Date(Date.parse(iso));
    const pad = (n: number) => String(n).padStart(2, "0");
    // Compare against the host's *local* wall-clock parts: this fails if the
    // implementation ever switches to UTC getters.
    const expectedTime = `${pad(d.getMonth() + 1)}/${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
    const out = formatDateTime(iso);
    expect(out.startsWith(`${expectedTime} `)).toBe(true);
    // Suffix is the timezone label and is non-empty.
    expect(out.slice(expectedTime.length + 1)).toBe(localTimeZoneLabel(d));
    expect(localTimeZoneLabel(d).length).toBeGreaterThan(0);
  });

  it("uses the Intl short timezone name when one is available", () => {
    // Use a regular function (not an arrow) so the impl's `new Intl.DateTimeFormat()`
    // returns this object instead of throwing.
    vi.spyOn(Intl, "DateTimeFormat").mockImplementation(function () {
      return {
        formatToParts: () => [{ type: "timeZoneName", value: "EDT" }],
      };
    } as unknown as typeof Intl.DateTimeFormat);
    expect(localTimeZoneLabel(new Date(0))).toBe("EDT");
  });

  it("falls back to a UTC±HH:MM offset when Intl cannot supply a name", () => {
    vi.spyOn(Intl, "DateTimeFormat").mockImplementation((() => {
      throw new Error("Intl unavailable");
    }) as unknown as typeof Intl.DateTimeFormat);
    // -330 min from UTC == 5h30m east of UTC (e.g. IST).
    vi.spyOn(Date.prototype, "getTimezoneOffset").mockReturnValue(-330);
    expect(localTimeZoneLabel(new Date(0))).toBe("UTC+05:30");

    vi.spyOn(Date.prototype, "getTimezoneOffset").mockReturnValue(240);
    expect(localTimeZoneLabel(new Date(0))).toBe("UTC-04:00");
  });
});

describe("outcomeVariant", () => {
  it("maps taxonomy-v2 outcomes to badge colors", () => {
    expect(outcomeVariant("completed")).toBe("default");
    expect(outcomeVariant("failed")).toBe("destructive");
    expect(outcomeVariant("running")).toBe("secondary");
    expect(outcomeVariant("stopped")).toBe("muted");
    expect(outcomeVariant("continued")).toBe("muted");
    expect(outcomeVariant("interrupted")).toBe("muted");
  });
});

describe("historyQuery", () => {
  it("omits empty fields and a zero offset", () => {
    expect(historyQuery({})).toBe("");
    expect(historyQuery({ offset: 0 })).toBe("");
    expect(historyQuery({ issue: "ABC-1" })).toBe("?issue=ABC-1");
  });
  it("includes set filters and pages", () => {
    expect(historyQuery({ issue: "ABC-1", outcome: "completed", limit: 25, offset: 25 })).toBe(
      "?issue=ABC-1&outcome=completed&limit=25&offset=25",
    );
  });
  it("includes the project filter when set and omits it when empty", () => {
    expect(historyQuery({ project: "tally-symphony-e3b6fdf879c1" })).toBe(
      "?project=tally-symphony-e3b6fdf879c1",
    );
    expect(historyQuery({ project: "" })).toBe("");
  });
  it("orders project after outcome and before since", () => {
    expect(
      historyQuery({ issue: "ABC-1", outcome: "failed", project: "p1", since: "2026-05-01T00:00:00Z" }),
    ).toBe("?issue=ABC-1&outcome=failed&project=p1&since=2026-05-01T00%3A00%3A00Z");
  });
});
