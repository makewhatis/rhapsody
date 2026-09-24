// ProviderEditor — the Add/Edit provider sheet for Settings → Providers (STUDIO-1048).
//
// Non-secret configuration only: adding, editing and removing a provider DEFINITION works in both
// the desktop app and the browser dashboard, behind the daemon's operator-write guard. Key actions
// (Connect/Replace/Rebind/Remove-credential) stay in ProviderCredentialForm, desktop-only.
//
// The form is a thin shell over the pure `providers-presets` helpers and the daemon: it never
// re-implements a validator the daemon owns. A local pre-check only refuses what is obviously wrong
// (a non-canonical id, an endpoint that cannot join cleanly) so the operator gets fast feedback; the
// daemon's own error text is what is shown for everything else.

import * as React from "react";
import { Button, Field, FieldError, Select, TextInput, X } from "@/components/ui";
import { saveProviderConfig, type ProviderConfigDTO } from "@/lib/api";
import {
  PROVIDER_LIMIT_FIELDS,
  PROVIDER_PRESETS,
  PROTOCOL_OPENAI_COMPATIBLE,
  chatCompletionsUrl,
  endpointChanged,
  endpointJoinsCleanly,
  normalizeProviderBaseUrl,
  providerLimitsOf,
  type ProviderPreset,
} from "@/lib/providers-presets";

export interface ProviderEditorProps {
  open: boolean;
  /** The provider being edited, or null for Add. */
  editing: ProviderConfigDTO | null;
  /** Provider ids already defined (Add refuses a duplicate locally). */
  usedIds: string[];
  /** Whether the edited provider has a stored credential (drives the Rebind warning). */
  hasStoredKey: boolean;
  onClose: () => void;
  onSaved: () => void;
}

const ID_RE = /^[a-z][a-z0-9_-]{0,63}$/;

/** Whether a base URL is plaintext HTTP (needing the visible opt-in). */
function isInsecureUrl(baseUrl: string): boolean {
  return baseUrl.trim().toLowerCase().startsWith("http://");
}

/** The editor's working state for the broker limits: every visible field as its raw input string, so
 *  an empty/invalid entry is refused locally without coercing it to 0. */
interface LimitsDraft {
  fields: Record<string, string>;
  dailyCap: string;
  /** The capability lifetime as it was prefilled; an unchanged value is not sent (it is derived). */
  initialCapability: string;
}

function limitsDraftOf(editing: ProviderConfigDTO | null): LimitsDraft {
  const limits = providerLimitsOf(editing);
  const fields: Record<string, string> = {};
  for (const { key } of PROVIDER_LIMIT_FIELDS) {
    fields[key] = String(limits[key]);
  }
  return {
    fields,
    dailyCap:
      limits.max_reserved_token_units_per_utc_day == null
        ? ""
        : String(limits.max_reserved_token_units_per_utc_day),
    initialCapability: String(limits.capability_lifetime_ms),
  };
}

/** A positive integer, or null when the string is not one. */
function parsePositive(raw: string): number | null {
  if (!/^\d+$/.test(raw.trim())) return null;
  const n = Number(raw.trim());
  return Number.isSafeInteger(n) && n > 0 ? n : null;
}

/** The local limits gate: every visible field must be a positive whole number, and the daily cap is
 *  either blank (no cap) or positive. The daemon still validates the full block (ceilings, ordering). */
function limitsDraftError(limits: LimitsDraft): string | null {
  for (const { key } of PROVIDER_LIMIT_FIELDS) {
    if (parsePositive(limits.fields[key] ?? "") == null) {
      return `${key} must be a positive whole number.`;
    }
  }
  if (limits.dailyCap.trim() !== "" && parsePositive(limits.dailyCap) == null) {
    return "The daily cap must be blank (no cap) or a positive whole number.";
  }
  return null;
}

export function ProviderEditor({
  open,
  editing,
  usedIds,
  hasStoredKey,
  onClose,
  onSaved,
}: ProviderEditorProps) {
  const [id, setId] = React.useState("");
  const [label, setLabel] = React.useState("");
  const [baseUrl, setBaseUrl] = React.useState("");
  const [protocol, setProtocol] = React.useState(PROTOCOL_OPENAI_COMPATIBLE);
  const [allowInsecure, setAllowInsecure] = React.useState(false);
  const [presetId, setPresetId] = React.useState("");
  const [limits, setLimits] = React.useState<LimitsDraft>(() => limitsDraftOf(null));
  const [error, setError] = React.useState<string | null>(null);
  const [busy, setBusy] = React.useState(false);

  React.useEffect(() => {
    if (!open) return;
    setError(null);
    setBusy(false);
    setLimits(limitsDraftOf(editing));
    if (editing) {
      setId(editing.id);
      setLabel(editing.display_name ?? "");
      setBaseUrl(editing.base_url);
      setProtocol(editing.protocol || PROTOCOL_OPENAI_COMPATIBLE);
      setAllowInsecure(Boolean(editing.allow_insecure_http));
      setPresetId("");
    } else {
      const first = PROVIDER_PRESETS[0];
      setId(first?.id ?? "provider");
      setLabel(first?.label ?? "");
      setBaseUrl(first?.base_url ?? "");
      setProtocol(first?.protocol ?? PROTOCOL_OPENAI_COMPATIBLE);
      setAllowInsecure(false);
      setPresetId(first?.base_url ?? "__custom__");
    }
  }, [open, editing]);

  React.useEffect(() => {
    const h = (e: KeyboardEvent) => {
      if (e.key === "Escape" && open) onClose();
    };
    window.addEventListener("keydown", h);
    return () => window.removeEventListener("keydown", h);
  }, [open, onClose]);

  if (!open) return null;

  /** Apply a preset's protocol + base URL (and its suggested id/label when still pristine). */
  const choosePreset = (value: string) => {
    setPresetId(value);
    if (value === "__custom__") return;
    const preset = PROVIDER_PRESETS.find((p) => p.base_url === value);
    if (!preset) return;
    setProtocol(preset.protocol);
    setBaseUrl(preset.base_url);
    setId(preset.id);
    setLabel(preset.label);
  };

  const idError =
    id.trim() === ""
      ? "A provider id is required."
      : !ID_RE.test(id)
        ? "Use lowercase letters, digits, '_' and '-', starting with a letter (max 64)."
        : !editing && usedIds.includes(id)
          ? `A provider with id "${id}" already exists.`
          : null;
  const urlError =
    baseUrl.trim() === ""
      ? "A base URL is required."
      : !endpointJoinsCleanly(baseUrl)
        ? "Enter an absolute http(s) endpoint that joins cleanly to one chat/completions URL."
        : null;
  const rebindWarning =
    editing != null &&
    hasStoredKey &&
    endpointChanged(editing, { base_url: baseUrl, protocol });
  const insecureWarning = isInsecureUrl(baseUrl);
  const limitsError = limitsDraftError(limits);
  const canSave = idError == null && urlError == null && limitsError == null && !busy;

  /** The wire `limits` block: the visible fields (all but capability, always sent — the writer drops
   *  a value equal to its default) plus the daily cap (blank ⇒ 0, the wire's "no daily cap"). The
   *  capability lifetime is sent only when the operator changed it, so an edit does not pin the
   *  deadline-derived default. */
  const limitsPayload = (): Record<string, number> => {
    const out: Record<string, number> = {};
    for (const { key } of PROVIDER_LIMIT_FIELDS) {
      const raw = limits.fields[key] ?? "";
      if (key === "capability_lifetime_ms" && raw === limits.initialCapability) continue;
      out[key] = Number(raw);
    }
    out.max_reserved_token_units_per_utc_day = limits.dailyCap.trim() === "" ? 0 : Number(limits.dailyCap);
    return out;
  };

  const save = async () => {
    setError(null);
    setBusy(true);
    try {
      if (editing) {
        await saveProviderConfig({
          op: "edit",
          provider_id: editing.id,
          definition: {
            protocol,
            display_name: label.trim(),
            base_url: baseUrl.trim(),
            allow_insecure_http: allowInsecure,
            limits: limitsPayload(),
          },
        });
      } else {
        await saveProviderConfig({
          op: "add",
          provider_id: id,
          definition: {
            protocol,
            display_name: label.trim(),
            base_url: baseUrl.trim(),
            allow_insecure_http: allowInsecure,
            limits: limitsPayload(),
          },
        });
      }
      onSaved();
      onClose();
    } catch (e) {
      // The daemon's own error text — never a paraphrase.
      setError(e instanceof Error ? e.message : "the provider could not be saved");
    } finally {
      setBusy(false);
    }
  };

  const presetOptions = [
    ...PROVIDER_PRESETS.map((p: ProviderPreset) => ({
      value: p.base_url,
      label: p.label,
      note: p.base_url,
    })),
    { value: "__custom__", label: "Custom endpoint", note: "Any OpenAI-compatible base URL" },
  ];

  const canonical = normalizeProviderBaseUrl(baseUrl);
  const completions = chatCompletionsUrl(baseUrl);

  return (
    <div style={{ position: "fixed", inset: 0, zIndex: 200, display: "flex", justifyContent: "flex-end" }}>
      <div
        data-testid="provider-editor-overlay"
        onClick={onClose}
        style={{ position: "absolute", inset: 0, background: "rgba(0,0,0,.55)", backdropFilter: "blur(2px)" }}
      />
      <div
        role="dialog"
        aria-label={editing ? "Edit provider" : "Add provider"}
        style={{
          position: "relative",
          width: 520,
          maxWidth: "92%",
          height: "100%",
          background: "var(--bg-app)",
          borderLeft: "1px solid var(--line-strong)",
          boxShadow: "var(--shadow-sheet)",
          display: "flex",
          flexDirection: "column",
          overflowY: "auto",
        }}
      >
        <div
          style={{
            display: "flex",
            alignItems: "flex-start",
            justifyContent: "space-between",
            gap: 12,
            padding: "22px 26px 18px",
            borderBottom: "1px solid var(--line-2)",
          }}
        >
          <div>
            <h2 style={{ fontSize: 17, fontWeight: 600, letterSpacing: "-0.02em" }}>
              {editing ? "Edit provider" : "Add provider"}
            </h2>
            <p style={{ fontSize: 12.5, color: "var(--tx-3)", marginTop: 7, lineHeight: 1.5 }}>
              v1 supports the OpenAI-compatible protocol only. The credential is not entered here —
              connect it from the provider card afterwards.
            </p>
          </div>
          <Button variant="ghost" size="sm" aria-label="Close" onClick={onClose}>
            <X size={16} />
          </Button>
        </div>

        <div style={{ display: "flex", flexDirection: "column", gap: 18, padding: "20px 26px" }}>
          {!editing ? (
            <Field label="Preset" hint="Fills the protocol and base URL; a custom endpoint is always allowed.">
              <Select value={presetId} options={presetOptions} onChange={choosePreset} width="100%" />
            </Field>
          ) : null}

          <Field
            label="Provider id"
            hint="A stable, label-safe id — the WORKFLOW.md key. Fixed once created."
            error={editing == null ? (idError ?? undefined) : undefined}
          >
            <TextInput
              aria-label="Provider id"
              mono
              value={id}
              disabled={editing != null}
              invalid={editing == null && idError != null}
              onChange={(e) => setId(e.target.value.trim())}
            />
          </Field>

          <Field label="Display name" hint="Shown on the provider card; free-form.">
            <TextInput
              aria-label="Display name"
              value={label}
              onChange={(e) => setLabel(e.target.value)}
            />
          </Field>

          <Field
            label="Base URL"
            hint="The protocol base immediately above chat/completions (commonly ending in /v1)."
            error={urlError ?? undefined}
          >
            <TextInput
              aria-label="Base URL"
              mono
              value={baseUrl}
              invalid={urlError != null}
              placeholder="https://api.example.com/v1"
              onChange={(e) => setBaseUrl(e.target.value)}
            />
          </Field>

          {!urlError && completions ? (
            <p style={{ fontSize: 12, color: "var(--tx-3)", marginTop: -8 }}>
              Requests go to <code className="mono">{completions}</code>
              {canonical && canonical !== baseUrl.trim() ? ` (stored as ${canonical})` : ""}.
            </p>
          ) : null}

          <Field label="Protocol" hint="v1's broker speaks only the OpenAI-compatible Chat Completions protocol.">
            <TextInput aria-label="Protocol" mono value={protocol} disabled onChange={() => {}} />
          </Field>

          <Field
            label="Allow insecure HTTP"
            hint="Required to use a plain http:// endpoint. The credential is then sent without transport encryption."
          >
            <label style={{ display: "flex", alignItems: "center", gap: 8, fontSize: 13 }}>
              <input
                type="checkbox"
                aria-label="Allow insecure HTTP"
                checked={allowInsecure}
                onChange={(e) => setAllowInsecure(e.target.checked)}
              />
              I accept sending this provider's credential over plaintext HTTP
            </label>
          </Field>

          {insecureWarning ? (
            <p role="alert" style={{ fontSize: 12.5, color: "var(--amber)" }}>
              This endpoint uses plaintext HTTP. Enable the opt-in above, or the daemon will refuse it.
            </p>
          ) : null}

          <Field
            label="Broker limits"
            hint="The daemon's finite per-turn and per-session guards, prefilled with the V1 defaults. Leave the daily cap blank for no Rhapsody daily cap."
            error={limitsError ?? undefined}
          >
            <div
              style={{
                display: "grid",
                gridTemplateColumns: "1fr 1fr",
                gap: "10px 12px",
              }}
            >
              {PROVIDER_LIMIT_FIELDS.map(({ key, label: fieldLabel }) => (
                <label
                  key={key}
                  style={{
                    display: "flex",
                    flexDirection: "column",
                    gap: 4,
                    fontSize: 11.5,
                    color: "var(--tx-3)",
                  }}
                >
                  <span>{fieldLabel}</span>
                  <TextInput
                    mono
                    inputMode="numeric"
                    aria-label={key}
                    value={limits.fields[key] ?? ""}
                    invalid={limitsError != null}
                    onChange={(e) =>
                      setLimits((l) => ({
                        ...l,
                        fields: { ...l.fields, [key]: e.target.value },
                      }))
                    }
                  />
                </label>
              ))}
              <label
                style={{
                  display: "flex",
                  flexDirection: "column",
                  gap: 4,
                  fontSize: 11.5,
                  color: "var(--tx-3)",
                }}
              >
                <span>Daily token-unit cap (blank = none)</span>
                <TextInput
                  mono
                  inputMode="numeric"
                  aria-label="max_reserved_token_units_per_utc_day"
                  value={limits.dailyCap}
                  invalid={limitsError != null}
                  onChange={(e) => setLimits((l) => ({ ...l, dailyCap: e.target.value }))}
                />
              </label>
            </div>
          </Field>

          {rebindWarning ? (
            <p role="alert" style={{ fontSize: 12.5, color: "var(--amber)" }}>
              This provider has a stored key. Saving a changed base URL or protocol invalidates it:
              the old key does not follow the new endpoint, and the status will move to “rebind
              required”. Only the desktop app can rebind it.
            </p>
          ) : null}

          {error ? <FieldError>{error}</FieldError> : null}
        </div>

        <div
          style={{
            display: "flex",
            justifyContent: "flex-end",
            gap: 8,
            padding: "16px 26px 24px",
            borderTop: "1px solid var(--line-2)",
            marginTop: "auto",
          }}
        >
          <Button variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button variant="primary" disabled={!canSave} onClick={() => void save()}>
            {busy ? "Saving…" : editing ? "Save changes" : "Add provider"}
          </Button>
        </div>
      </div>
    </div>
  );
}
