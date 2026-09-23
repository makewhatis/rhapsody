// ProvidersTab — the shared provider registry + model-selection Settings surface (STUDIO-992, §P11).
//
// This is ONE component rendered by both shells (the embedded/desktop Podium Settings and the
// console Settings hub), so it must behave correctly as a plain browser page and inside the Tauri
// webview. The two hard boundaries the ticket names:
//
//   * Browser-only mode may DISPLAY status and edit the (non-secret) global provider/model
//     selection, but it must never render a credential-mutating control — the shared copy says so.
//   * Credential status is read from the daemon's status feed, never derived from the config
//     definition, so an endpoint/protocol edit is visible as `unknown_refreshing` → `binding_mismatch`
//     rather than a stale "Connected".
//
// The global selection is edited through the same draft/autosave path as every other Settings field
// (`onChange` → useConfigDraft). The registry's `providers:` definitions are authored in WORKFLOW.md
// and are read-only here; this surface selects among them.

import * as React from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Button, Cpu, Field, Key, SectionCard, Select, TextInput } from "@/components/ui";
import {
  fetchProviderCatalog,
  fetchProviderStatuses,
  refreshProviderCatalog,
  type ProviderCatalogDTO,
} from "@/lib/api";
import {
  hasProviderBridge,
  providerStatuses as desktopProviderStatuses,
  type ProviderStatus,
} from "@/lib/provider-credentials";
import {
  ACTIVE_SESSION_NOTICE,
  DESKTOP_ONLY_MESSAGE,
  cacheAgeLabel,
  catalogFailed,
  catalogOptions,
  credentialStatusLabel,
  credentialTone,
  isCredentialBlocked,
  manualEntryAllowed,
  providerViews,
  recoveryHint,
  selectionCompatibility,
  type ProviderStatusInput,
  type ProviderView,
} from "@/lib/providers-model";
import type { UiGlobal } from "@/lib/settings-model";
import { ProviderCredentialForm } from "./ProviderCredentialForm";

export interface ProvidersTabProps {
  value: UiGlobal;
  /** Apply a global-defaults edit (the parent marks the form dirty + autosaves). */
  onChange: (next: UiGlobal) => void;
}

const STATUS_QUERY_KEY = "provider-statuses";
const catalogKey = (id: string) => ["provider-catalog", id] as const;

// The status feed in desktop mode is the Tauri bridge's richer DTO (it carries the canonical
// endpoint + the operation flags); in a plain browser it is the daemon's cache-only status view.
function statusQueryFn(bridged: boolean): () => Promise<ProviderStatusInput[]> {
  return bridged ? () => desktopProviderStatuses() : () => fetchProviderStatuses();
}

export function ProvidersTab({ value, onChange }: ProvidersTabProps) {
  const bridged = hasProviderBridge();
  const qc = useQueryClient();

  const statuses = useQuery({
    queryKey: [STATUS_QUERY_KEY, bridged],
    queryFn: statusQueryFn(bridged),
    retry: false,
    // The daemon's status GET is cache-only (it never opens Keychain or performs IPC), so polling it
    // in the browser dashboard is safe and lets a definition reload's `unknown_refreshing` →
    // `binding_mismatch` convergence become visible without a manual reload. The desktop bridge read
    // is the owner's live read, so it is NOT polled — that would re-open Keychain on a timer.
    refetchInterval: bridged ? false : 15000,
  });
  const views = providerViews(value.providers, statuses.data ?? []);

  const selected = value.provider;
  const catalog = useQuery({
    queryKey: catalogKey(selected),
    queryFn: () => fetchProviderCatalog(selected),
    enabled: selected !== "",
    retry: false,
  });
  const snapshot: ProviderCatalogDTO | undefined = selected !== "" ? catalog.data : undefined;

  const [refreshing, setRefreshing] = React.useState(false);
  const [refreshError, setRefreshError] = React.useState<string | null>(null);
  const refreshModels = async () => {
    if (selected === "") return;
    setRefreshing(true);
    setRefreshError(null);
    try {
      const next = await refreshProviderCatalog(selected);
      qc.setQueryData(catalogKey(selected), next);
    } catch (e) {
      setRefreshError(e instanceof Error ? e.message : "the catalog refresh failed");
    } finally {
      setRefreshing(false);
    }
  };

  const providerOptions = [
    {
      value: "",
      label: "None — legacy / native login",
      note: "No provider; the backend's own login and default model",
    },
    ...value.providers.map((p) => ({
      value: p.id,
      label: p.display_name.trim() || p.id,
      note: p.protocol,
    })),
  ];

  const feedback = selectionCompatibility({
    backend: value.agentBackend,
    providers: value.providers,
    provider: value.provider,
    model: value.agentModel,
  });

  return (
    <div style={{ display: "flex", flexDirection: "column", gap: 18 }}>
      <SectionCard
        title="Global model selection"
        icon={Cpu}
        desc="The provider-first default every agent inherits. A provider requires the opencode backend; Claude agents keep their native login and claude.model."
      >
        <Field
          label="Provider"
          inline
          hint="A provider defined in WORKFLOW.md's providers: block. Only opencode can consume one in v1."
        >
          <Select
            value={value.provider}
            options={providerOptions}
            invalid={!feedback.ok}
            onChange={(v) => onChange({ ...value, provider: v })}
          />
        </Field>
        <Field
          label="Model"
          inline
          hint="Exact model id. Suggestions come from the provider's catalog; typing any model id is always allowed."
          error={feedback.ok ? undefined : feedback.reason}
        >
          <TextInput
            mono
            invalid={!feedback.ok}
            aria-label="Global model"
            value={value.agentModel}
            placeholder="accounts/…/model-id"
            onChange={(e) => onChange({ ...value, agentModel: e.target.value })}
          />
        </Field>

        {selected !== "" && (
          <div style={{ display: "flex", flexDirection: "column", gap: 8 }}>
            <div style={{ display: "flex", alignItems: "center", gap: 10, flexWrap: "wrap" }}>
              <Button variant="subtle" size="sm" onClick={() => void refreshModels()} disabled={refreshing}>
                {refreshing ? "Refreshing models…" : "Refresh models"}
              </Button>
              <span style={{ fontSize: 12, color: "var(--tx-3)" }}>
                Catalog {cacheAgeLabel(snapshot?.cache_age_ms)}
                {snapshot?.truncated ? " · truncated" : ""}
              </span>
            </div>
            {/* Suggestions are additive; manual entry in the field above is always available. */}
            {catalogOptions(snapshot).map((opt) => (
              <button
                key={opt.value}
                type="button"
                onClick={() => onChange({ ...value, agentModel: opt.value })}
                style={{
                  alignSelf: "flex-start",
                  fontFamily: "var(--font-mono)",
                  fontSize: 12,
                  color: "var(--tx-2)",
                  background: "transparent",
                  border: "1px solid var(--line)",
                  borderRadius: "var(--r-keycap)",
                  padding: "2px 8px",
                  cursor: "pointer",
                }}
              >
                {opt.value}
              </button>
            ))}
            {catalogFailed(snapshot) && (
              <p style={{ fontSize: 12, color: "var(--amber)" }}>
                The model catalog could not be refreshed
                {snapshot?.error_message ? `: ${snapshot.error_message}` : ""}. Manual entry remains
                available — suggestions are not an allow-list.
              </p>
            )}
            {refreshError && <p style={{ fontSize: 12, color: "var(--red)" }}>{refreshError}</p>}
            {!manualEntryAllowed(snapshot) && (
              // Defensive: the daemon pins manual_entry_allowed true, so this should be unreachable.
              <p style={{ fontSize: 12, color: "var(--red)" }}>Manual model entry is unavailable.</p>
            )}
          </div>
        )}
      </SectionCard>

      <SectionCard
        title="Providers"
        icon={Key}
        desc="Credential status is read from the daemon's non-secret cache. No key, stored binding, or credential revision is ever shown."
      >
        {views.length === 0 ? (
          <p style={{ fontSize: 13, color: "var(--tx-3)" }}>
            No providers are configured. Add a <code className="mono">providers:</code> block to
            WORKFLOW.md to register one.
          </p>
        ) : (
          <div style={{ display: "flex", flexDirection: "column", gap: 12 }}>
            {views.map((view) => (
              <ProviderCard
                key={view.provider_id}
                view={view}
                bridged={bridged}
                status={statuses.data?.find((s) => s.provider_id === view.provider_id)}
                onChanged={() => void statuses.refetch()}
              />
            ))}
          </div>
        )}
        {!bridged && <p style={{ fontSize: 12, color: "var(--tx-3)" }}>{DESKTOP_ONLY_MESSAGE}</p>}
        {bridged && <p style={{ fontSize: 12, color: "var(--tx-3)" }}>{ACTIVE_SESSION_NOTICE}</p>}
      </SectionCard>
    </div>
  );
}

const TONE_COLOR: Record<ReturnType<typeof credentialTone>, string> = {
  ok: "var(--sage)",
  warn: "var(--amber)",
  blocked: "var(--red)",
  unknown: "var(--tx-3)",
};

function ProviderCard({
  view,
  bridged,
  status,
  onChanged,
}: {
  view: ProviderView;
  bridged: boolean;
  status?: ProviderStatusInput;
  onChanged: () => void;
}) {
  const hint = recoveryHint(view.recovery);
  // Reconstruct the desktop DTO shape the credential form consumes. The form only reads the fields
  // present on the view; the adapter is carried through for completeness but never rendered.
  const desktopStatus: ProviderStatus = {
    provider_id: view.provider_id,
    display_name: view.display_name,
    endpoint: view.endpoint,
    adapter: status?.adapter ?? "",
    insecure_http: view.insecure_http,
    status: view.status,
    recovery: view.recovery,
    can_connect: view.can_connect,
    can_replace: view.can_replace,
    can_rebind: view.can_rebind,
    can_remove: view.can_remove,
  };

  return (
    <div
      data-testid={`provider-card-${view.provider_id}`}
      style={{ border: "1px solid var(--line)", borderRadius: "var(--r-card)", padding: "14px 16px" }}
    >
      <div style={{ display: "flex", alignItems: "baseline", justifyContent: "space-between", gap: 12 }}>
        <div>
          <div style={{ fontSize: 13.5, fontWeight: 600 }}>{view.display_name}</div>
          <div className="mono" style={{ fontSize: 12, color: "var(--tx-3)" }}>
            {view.endpoint || "(endpoint unavailable)"}
          </div>
        </div>
        <div style={{ textAlign: "right" }}>
          <div style={{ fontSize: 12.5, color: TONE_COLOR[credentialTone(view.status)] }}>
            {credentialStatusLabel(view.status)}
          </div>
          <div style={{ fontSize: 11, color: "var(--tx-faint)" }}>
            {view.refreshing ? "refreshing · " : ""}
            {cacheAgeLabel(view.cache_age_ms)}
          </div>
        </div>
      </div>

      <div style={{ fontSize: 11.5, color: "var(--tx-faint)", marginTop: 6 }}>
        {view.protocol || "openai-compatible"}
        {view.credential_source ? ` · credential: ${view.credential_source}` : ""}
        {!view.broker_available ? " · broker unavailable — credentialed dispatch is refused" : ""}
      </div>

      {view.insecure_http && (
        <p style={{ fontSize: 12, color: "var(--amber)", marginTop: 8 }}>
          This endpoint uses plaintext HTTP. The credential is sent without transport encryption.
        </p>
      )}

      {hint && (
        <p style={{ fontSize: 12, color: isCredentialBlocked(view.status) ? "var(--red)" : "var(--tx-3)", marginTop: 8 }}>
          {hint}
        </p>
      )}

      {bridged ? (
        <div style={{ marginTop: 10 }}>
          <ProviderCredentialForm provider={desktopStatus} onChanged={onChanged} />
        </div>
      ) : null}
    </div>
  );
}
