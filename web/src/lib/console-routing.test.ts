import { describe, expect, it } from "vitest";
import {
  CONSOLE_ROUTES,
  DEFAULT_CONSOLE_ROUTE,
  TEAMS_ONLY_ROUTES,
  consoleNavFor,
  consoleRouteHash,
  gateConsoleRoute,
  parseConsoleRoute,
  sameConsoleRoute,
} from "./console-routing";

describe("parseConsoleRoute", () => {
  // §10 box 2.3 — the default landing is Jobs, from every spelling of "no route".
  it.each(["", "#", "#/", "#jobs", "#/jobs", "#jobs/"])("%o lands on jobs", (hash) => {
    expect(parseConsoleRoute(hash)).toEqual({ name: "jobs", key: "" });
  });

  it("never resolves an unknown route to teams", () => {
    // The one route the app must never auto-land on (§2.1), including from a typo or a
    // deep link left over from an older build.
    for (const hash of ["#nope", "#job", "#teams-console", "#/demo"]) {
      expect(parseConsoleRoute(hash).name).not.toBe("teams");
    }
  });

  it("reads the ticket key out of job/:key", () => {
    expect(parseConsoleRoute("#job/STUDIO-654")).toEqual({ name: "job", key: "STUDIO-654" });
    expect(parseConsoleRoute("#/job/STUDIO-654")).toEqual({ name: "job", key: "STUDIO-654" });
  });

  it("percent-decodes a key and falls back to jobs when it is missing", () => {
    expect(parseConsoleRoute("#job/STUDIO%2D654").key).toBe("STUDIO-654");
    expect(parseConsoleRoute("#job/")).toEqual(DEFAULT_CONSOLE_ROUTE);
    expect(parseConsoleRoute("#job")).toEqual(DEFAULT_CONSOLE_ROUTE);
  });

  it("resolves each named route", () => {
    for (const name of CONSOLE_ROUTES) {
      if (name === "job") continue;
      expect(parseConsoleRoute(`#${name}`)).toEqual({ name, key: "" });
    }
  });
});

describe("consoleRouteHash", () => {
  it("round-trips every route through parse", () => {
    const routes = [
      { name: "jobs", key: "" },
      { name: "job", key: "STUDIO-654" },
      { name: "teams", key: "" },
      { name: "memory", key: "" },
      { name: "manage", key: "" },
      { name: "reviews", key: "" },
      { name: "settings", key: "" },
    ] as const;
    for (const r of routes) {
      expect(parseConsoleRoute(consoleRouteHash(r))).toEqual(r);
    }
  });

  it("escapes a key that would otherwise split the hash", () => {
    const hash = consoleRouteHash({ name: "job", key: "A/B" });
    expect(parseConsoleRoute(hash)).toEqual({ name: "job", key: "A/B" });
  });
});

describe("gateConsoleRoute", () => {
  // §10 box 2.4 — with teams off, every teams-only surface falls back to Jobs.
  it.each([...TEAMS_ONLY_ROUTES])("redirects %s to jobs when teams is off", (name) => {
    expect(gateConsoleRoute({ name, key: "" }, false)).toEqual(DEFAULT_CONSOLE_ROUTE);
  });

  it("leaves teams-only routes alone when teams is on", () => {
    for (const name of TEAMS_ONLY_ROUTES) {
      expect(gateConsoleRoute({ name, key: "" }, true)).toEqual({ name, key: "" });
    }
  });

  it("keeps jobs, job/:key and settings reachable with teams off", () => {
    // A job is not a teams surface: with teams off the daemon still runs one agent per
    // issue, so its history stays readable. Only the rail's teams items disappear (§2.2).
    expect(gateConsoleRoute({ name: "job", key: "STUDIO-654" }, false)).toEqual({
      name: "job",
      key: "STUDIO-654",
    });
    expect(gateConsoleRoute({ name: "settings", key: "" }, false).name).toBe("settings");
    expect(gateConsoleRoute({ name: "jobs", key: "" }, false).name).toBe("jobs");
  });
});

describe("consoleNavFor", () => {
  // §10 box 2.12 — a child route highlights its parent nav item.
  it("highlights the parent of a child route", () => {
    expect(consoleNavFor({ name: "job", key: "STUDIO-654" })).toBe("jobs");
    expect(consoleNavFor({ name: "manage", key: "" })).toBe("teams");
    // The Reviews surface (STUDIO-722) is a Teams child like Manage team, so it lights the Teams
    // rail item rather than adding one of its own.
    expect(consoleNavFor({ name: "reviews", key: "" })).toBe("teams");
  });

  it("highlights a top-level route itself", () => {
    expect(consoleNavFor({ name: "jobs", key: "" })).toBe("jobs");
    expect(consoleNavFor({ name: "teams", key: "" })).toBe("teams");
    expect(consoleNavFor({ name: "memory", key: "" })).toBe("memory");
    expect(consoleNavFor({ name: "settings", key: "" })).toBe("settings");
  });
});

describe("sameConsoleRoute", () => {
  it("compares name and key", () => {
    expect(sameConsoleRoute({ name: "job", key: "A" }, { name: "job", key: "A" })).toBe(true);
    expect(sameConsoleRoute({ name: "job", key: "A" }, { name: "job", key: "B" })).toBe(false);
    expect(sameConsoleRoute({ name: "jobs", key: "" }, { name: "teams", key: "" })).toBe(false);
  });
});

// STUDIO-690 — the WORKFLOW.md editor is a child of Settings (§8): its own route so it is
// deep-linkable and Back-able, but it highlights Settings in the rail, and it is NOT a teams
// surface (WORKFLOW.md is the solo daemon's config too, so the gate must leave it alone).
describe("the workflow route (STUDIO-690)", () => {
  it("parses and round-trips", () => {
    expect(parseConsoleRoute("#workflow")).toEqual({ name: "workflow", key: "" });
    expect(consoleRouteHash({ name: "workflow", key: "" })).toBe("#workflow");
  });

  it("highlights Settings", () => {
    expect(consoleNavFor({ name: "workflow", key: "" })).toBe("settings");
  });

  it("stays reachable with teams off", () => {
    expect(gateConsoleRoute({ name: "workflow", key: "" }, false)).toEqual({
      name: "workflow",
      key: "",
    });
  });
});

// STUDIO-691 — Tools, Logs and Updates are Settings children, exactly like `workflow` (§8.1). They
// are the three surfaces the shipped Podium Settings has that the console did not, and the go-live
// flip (§10 box 6.4) is blocked until they are reachable. None is a teams surface: the tool doctor,
// the daemon log tail and the desktop updater all exist on a solo daemon.
describe("the Settings tab routes (STUDIO-691)", () => {
  const routes = ["tools", "logs", "updates"] as const;

  it("parse and round-trip", () => {
    for (const name of routes) {
      expect(parseConsoleRoute(`#${name}`)).toEqual({ name, key: "" });
      expect(consoleRouteHash({ name, key: "" })).toBe(`#${name}`);
    }
  });

  it("highlight Settings", () => {
    for (const name of routes) {
      expect(consoleNavFor({ name, key: "" })).toBe("settings");
    }
  });

  it("stay reachable with teams off", () => {
    for (const name of routes) {
      expect(gateConsoleRoute({ name, key: "" }, false)).toEqual({ name, key: "" });
    }
  });
});
