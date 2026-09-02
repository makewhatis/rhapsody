// Client-side routing for the Rhapsody Console — STUDIO-681 §2.3, built by STUDIO-683.
//
// Hash routing, exactly as the committed prototype does it
// (`~/.rhapsody/docs/STUDIO-681-prototype.html`): the route is `location.hash` minus its
// leading `#`, so back/forward and deep links work on any static host — the daemon's
// embedded dashboard and the desktop webview alike — with no server rewrite.
//
// Everything here is pure. The hook that binds it to `window.location` lives in
// `hooks/useConsoleRoute.ts`, which keeps the redirect and highlight rules testable
// without a DOM.

/** Every route the console serves (§2.3). `job` is the only one that carries a key. */
export const CONSOLE_ROUTES = [
  "jobs",
  "job",
  "teams",
  "memory",
  "manage",
  "settings",
  "workflow",
  "tools",
  "logs",
  "updates",
] as const;

export type ConsoleRouteName = (typeof CONSOLE_ROUTES)[number];

export interface ConsoleRoute {
  name: ConsoleRouteName;
  /** The ticket key for `job/:key`; "" for every other route. */
  key: string;
}

/**
 * Jobs is home (§2.1). It is the landing route, the fallback for anything unparseable, and
 * the target every capability redirect points at — the app never auto-lands on Teams.
 */
export const DEFAULT_CONSOLE_ROUTE: ConsoleRoute = { name: "jobs", key: "" };

/**
 * The routes that only exist when the daemon has Teams on (§2.2). With `teams_enabled:false`
 * these are unreachable *and* unrendered: the rail omits their nav items and `gateConsoleRoute`
 * sends the routes themselves back to Jobs, so a stale deep link cannot strand the operator on
 * a surface the daemon cannot serve.
 */
export const TEAMS_ONLY_ROUTES = ["teams", "memory", "manage"] as const satisfies readonly ConsoleRouteName[];

/**
 * Child route → the nav item that owns it (§2.3): `job` sits under Jobs, `manage` under Teams,
 * and `workflow` — the WORKFLOW.md editor the Settings "Workflow" row opens (STUDIO-690) — under
 * Settings. It is a route of its own rather than local state so the editor is deep-linkable and
 * the browser Back button returns to Settings, exactly as `manage` does for Teams.
 *
 * `tools`, `logs` and `updates` (STUDIO-691) are three more of the same shape: the Settings rows
 * that open the tool doctor, the live log tail and the desktop updater (§8.1). None is teams-only
 * — all three exist on a solo daemon — so `gateConsoleRoute` leaves them alone.
 */
const NAV_PARENT: Partial<Record<ConsoleRouteName, ConsoleRouteName>> = {
  job: "jobs",
  manage: "teams",
  workflow: "settings",
  tools: "settings",
  logs: "settings",
  updates: "settings",
};

function isRouteName(v: string): v is ConsoleRouteName {
  return (CONSOLE_ROUTES as readonly string[]).includes(v);
}

/**
 * Parses a `location.hash` (with or without its `#`, with or without a leading `/`) into a
 * route. Anything unrecognised — an empty hash, a typo, a link from an older build, a `job`
 * with no key — resolves to Jobs rather than throwing or rendering nothing.
 */
export function parseConsoleRoute(hash: string): ConsoleRoute {
  const path = hash.replace(/^#/, "").replace(/^\/+/, "").replace(/\/+$/, "");
  if (path === "") return DEFAULT_CONSOLE_ROUTE;

  const [head, ...rest] = path.split("/");
  if (!isRouteName(head)) return DEFAULT_CONSOLE_ROUTE;
  if (head !== "job") return { name: head, key: "" };

  // `job` without a key is not a view — there is no ticket to render — so it lands on Jobs.
  const raw = rest.join("/");
  if (raw === "") return DEFAULT_CONSOLE_ROUTE;
  return { name: "job", key: safeDecode(raw) };
}

// A hand-typed key can carry a stray `%`, which decodeURIComponent throws on. The key is
// display+lookup data, so a malformed escape degrades to its literal text rather than
// white-screening the view.
function safeDecode(raw: string): string {
  try {
    return decodeURIComponent(raw);
  } catch {
    return raw;
  }
}

/** The hash a route navigates to — the inverse of `parseConsoleRoute`. */
export function consoleRouteHash(route: ConsoleRoute): string {
  if (route.name === "job" && route.key !== "") {
    return `#job/${encodeURIComponent(route.key)}`;
  }
  return `#${route.name}`;
}

/**
 * Applies the §2.2 capability gate: with Teams off, every teams-only route falls back to Jobs.
 * A `job/:key` route is NOT gated — a job is a daemon surface, not a Teams one, and a solo
 * daemon's issue history stays readable.
 */
export function gateConsoleRoute(route: ConsoleRoute, teamsEnabled: boolean): ConsoleRoute {
  if (teamsEnabled) return route;
  return (TEAMS_ONLY_ROUTES as readonly ConsoleRouteName[]).includes(route.name)
    ? DEFAULT_CONSOLE_ROUTE
    : route;
}

/** The nav item a route highlights — its parent for a child route, itself otherwise (§2.3). */
export function consoleNavFor(route: ConsoleRoute): ConsoleRouteName {
  return NAV_PARENT[route.name] ?? route.name;
}

export function sameConsoleRoute(a: ConsoleRoute, b: ConsoleRoute): boolean {
  return a.name === b.name && a.key === b.key;
}
