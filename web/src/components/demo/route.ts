import { useSyncExternalStore } from "react";

// The primitive gallery is a verification-only route, deliberately kept OUT of the app nav.
// Reach it at `#/demo` (hash routing works on any static host — daemon embed or Wails — with
// no server rewrite) or at a `/demo` path when the host serves the SPA there.
export function isDemoRoute(loc: Pick<Location, "pathname" | "hash"> = window.location): boolean {
  // Normalize trailing slashes so `/demo/` and `/demo//` match the same as `/demo`.
  const p = loc.pathname.replace(/\/+$/, "");
  return loc.hash === "#/demo" || p.endsWith("/demo");
}

function subscribeToLocation(onChange: () => void): () => void {
  window.addEventListener("hashchange", onChange);
  window.addEventListener("popstate", onChange);
  return () => {
    window.removeEventListener("hashchange", onChange);
    window.removeEventListener("popstate", onChange);
  };
}

// useIsDemoRoute re-evaluates isDemoRoute() whenever the URL changes (hashchange / popstate),
// so navigating to or from `#/demo` without a full reload swaps the rendered tree instead of
// leaving a stale one on screen. Returns false where there's no window (SSR/non-browser).
export function useIsDemoRoute(): boolean {
  return useSyncExternalStore(
    subscribeToLocation,
    () => isDemoRoute(),
    () => false,
  );
}
