import { useQuery, type UseQueryResult } from "@tanstack/react-query";
import { probeTools, type ToolResult } from "@/lib/bindings";

// TOOL_DOCTOR_QUERY_KEY is the shared cache key for the preflight/doctor probe (mock 2c). The Tools
// tab and the Settings rail both read it via useToolDoctor, so TanStack Query dedupes them onto one
// cache entry: a Re-run from the Tools tab updates the same data the rail derives its amber
// warning-dot from, and both re-render together.
export const TOOL_DOCTOR_QUERY_KEY = ["tools"] as const;

// useToolDoctor runs the app-side preflight (the Go `probeTools` binding) and exposes the shared
// TanStack query. It is called from BOTH the Tools tab (rows + "Re-run preflight" + "preflight ran
// Xm ago") and the Settings shell (the rail's Tools warning dot). Mounting it in the shell means the
// probe runs as soon as Settings opens, so the rail dot reflects a warning even before the Tools tab
// is visited ("re-checked on launch"). refetchOnWindowFocus is off — the probe is manual (Re-run)
// plus the on-mount launch; a plain browser without the desktop bridge resolves to [] (no warning).
export function useToolDoctor(): UseQueryResult<ToolResult[]> {
  return useQuery({ queryKey: TOOL_DOCTOR_QUERY_KEY, queryFn: probeTools, refetchOnWindowFocus: false });
}
