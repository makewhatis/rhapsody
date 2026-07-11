import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  fetchConfig,
  fetchLinearIdentity,
  fetchLinearProjects,
  fetchProjectStatuses,
  fetchTypedConfig,
  saveConfig,
  saveTypedConfig,
  type ConfigRequest,
  type ConfigResponse,
  type GlobalConfigDTO,
  type ProjectConfigDTO,
  type TypedConfigResponse,
} from "@/lib/api";
import { STATE_QUERY_KEY } from "@/hooks/useStateQuery";

export const CONFIG_QUERY_KEY = ["config"] as const;
export const TYPED_CONFIG_QUERY_KEY = ["config", "typed"] as const;
export const LINEAR_IDENTITY_QUERY_KEY = ["linear", "identity"] as const;
export const LINEAR_PROJECTS_QUERY_KEY = ["linear", "projects"] as const;
export const PROJECT_STATUS_QUERY_KEY = ["projects", "status"] as const;

// useConfigQuery loads the current WORKFLOW.md (front matter + prompt body) for the Settings
// form. It is fetched on demand (no polling): config changes only when the user saves.
export function useConfigQuery() {
  return useQuery<ConfigResponse>({
    queryKey: CONFIG_QUERY_KEY,
    queryFn: fetchConfig,
    refetchOnWindowFocus: false,
  });
}

// useSaveConfig POSTs the edited config; on success it refreshes the config (echoed from disk)
// and the live state (the daemon hot-reloads, so counts/states may shift).
export function useSaveConfig() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (req: ConfigRequest) => saveConfig(req),
    onSuccess: (saved) => {
      qc.setQueryData(CONFIG_QUERY_KEY, saved);
      void qc.invalidateQueries({ queryKey: STATE_QUERY_KEY });
    },
  });
}

// --- Typed multi-agent config (Settings UI / INF-226) ---

// useTypedConfigQuery loads the typed multi-agent view (global + projects) for the Settings
// tabs. Fetched on demand (no polling): config only changes when the user saves.
export function useTypedConfigQuery() {
  return useQuery<TypedConfigResponse>({
    queryKey: TYPED_CONFIG_QUERY_KEY,
    queryFn: fetchTypedConfig,
    refetchOnWindowFocus: false,
  });
}

// useSaveTypedConfig POSTs the typed global + projects; on success it echoes the daemon's
// re-read config back into the query cache and invalidates the live state + per-project status
// (the daemon hot-reloads, so counts/states may shift).
export function useSaveTypedConfig() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: (vars: { global: GlobalConfigDTO; projects: ProjectConfigDTO[] }) =>
      saveTypedConfig(vars.global, vars.projects),
    onSuccess: (saved) => {
      qc.setQueryData(TYPED_CONFIG_QUERY_KEY, saved);
      void qc.invalidateQueries({ queryKey: STATE_QUERY_KEY });
      void qc.invalidateQueries({ queryKey: PROJECT_STATUS_QUERY_KEY });
    },
  });
}

// useLinearIdentity loads the connected-as Linear account (General tab + Tools mirror). The
// masked token comes straight from the daemon; the raw key is never exposed.
export function useLinearIdentity() {
  return useQuery({
    queryKey: LINEAR_IDENTITY_QUERY_KEY,
    queryFn: fetchLinearIdentity,
    refetchOnWindowFocus: false,
  });
}

// useLinearProjects loads the workspace's Linear projects (Add-agent picker + per-agent colour).
export function useLinearProjects() {
  return useQuery({
    queryKey: LINEAR_PROJECTS_QUERY_KEY,
    queryFn: fetchLinearProjects,
    refetchOnWindowFocus: false,
  });
}

// useProjectStatuses polls per-project live run status (status + in-flight count) for the agent
// list rows. Polls on the same cadence as the live state query.
export function useProjectStatuses(opts?: { enabled?: boolean }) {
  return useQuery({
    queryKey: PROJECT_STATUS_QUERY_KEY,
    queryFn: fetchProjectStatuses,
    refetchInterval: 2000,
    refetchOnWindowFocus: false,
    enabled: opts?.enabled ?? true,
  });
}
