import { useCallback, useEffect, useMemo, useSyncExternalStore } from "react";
import {
  consoleRouteHash,
  gateConsoleRoute,
  parseConsoleRoute,
  type ConsoleRoute,
} from "@/lib/console-routing";

function subscribe(onChange: () => void): () => void {
  window.addEventListener("hashchange", onChange);
  window.addEventListener("popstate", onChange);
  return () => {
    window.removeEventListener("hashchange", onChange);
    window.removeEventListener("popstate", onChange);
  };
}

// The snapshot is the raw hash STRING, not a parsed route: useSyncExternalStore compares
// snapshots by identity, and a fresh object every read would re-render forever.
function readHash(): string {
  return typeof window === "undefined" ? "" : window.location.hash;
}

/**
 * Binds the console's hash routing (STUDIO-681 §2.3) to the window, applying the §2.2
 * capability gate on the way out: with Teams off, a teams-only route resolves to Jobs and the
 * address bar is corrected to match, so a bookmark or a rail link left over from a Teams-on
 * daemon cannot strand the operator.
 *
 * `teamsEnabled` is deliberately TRI-STATE. `undefined` means the capability is not known yet —
 * `GET /api/v1/version` has not answered — and while that is true the gate does not run at all.
 * Collapsing unknown into `false` would redirect a `#teams` deep link to Jobs in the moment
 * before the version request settles, and because the redirect rewrites the address bar it
 * would stick: a bookmark to the Teams console would bounce to Jobs on a perfectly healthy
 * Teams-ON daemon, every single load.
 *
 * The correction uses `replaceState`, not an assignment to `location.hash`: a redirect the
 * operator did not ask for must not become a history entry they have to press Back through.
 */
export function useConsoleRoute(
  teamsEnabled: boolean | undefined,
): [ConsoleRoute, (to: ConsoleRoute) => void] {
  const hash = useSyncExternalStore(subscribe, readHash, () => "");
  const route = useMemo(() => {
    const parsed = parseConsoleRoute(hash);
    return teamsEnabled === undefined ? parsed : gateConsoleRoute(parsed, teamsEnabled);
  }, [hash, teamsEnabled]);

  const known = teamsEnabled !== undefined;
  const wanted = consoleRouteHash(route);
  useEffect(() => {
    if (typeof window === "undefined" || !known) return;
    if (window.location.hash !== wanted) {
      window.history.replaceState(null, "", wanted);
    }
  }, [wanted, known]);

  const navigate = useCallback((to: ConsoleRoute) => {
    const next = consoleRouteHash(to);
    if (window.location.hash === next) return;
    window.location.hash = next;
  }, []);

  return [route, navigate];
}
