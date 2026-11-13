import * as React from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Button, Field, SectionCard, Select, TextInput } from "@/components/ui";
import {
  applyTeamsModels,
  fetchProviderCatalog,
  fetchTeamsModels,
  type ModelsRoutingEntry,
  type ModelsRoutingView,
  type TeamsModelsRequest,
  type TeamsModelsView,
} from "@/lib/api";

// ModelsTab — Settings → Models (STUDIO-1976). Change each teammate's, the reviews' and the
// manager's harness/provider/model LIVE: a teammate's change rewrites that profile's front matter,
// a review/manager change rewrites the teams.yaml field in place, and both apply at the next
// dispatch / review round / manager turn with no restart. There is no autosave — a change is an
// explicit Save, exactly like the Teams tab, because the writes touch operator files.

const MODELS_QUERY_KEY = ["teams-models"] as const;

const EMPTY_ROUTING: ModelsRoutingView = { harness: "", provider: "", model: "", effort: "" };

function errText(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

function Note({ tone, children }: { tone?: "red" | "amber"; children: React.ReactNode }) {
  const color = tone === "red" ? "var(--red)" : tone === "amber" ? "var(--amber, #b45309)" : "var(--tx-3)";
  return <div style={{ fontSize: 12.5, color, lineHeight: 1.5 }}>{children}</div>;
}

/** The resolved tuple, with each field's origin. */
function TupleLine({ tuple }: { tuple: TeamsModelsView["manager"]["resolved"] }) {
  if (tuple.refused) return <Note tone="red">REFUSED: {tuple.refused}</Note>;
  const field = (label: string, value: string, origin: string) => (
    <span style={{ marginRight: 14 }}>
      <span style={{ color: "var(--tx-3)" }}>{label} </span>
      <code>{value || "—"}</code>
      {origin ? <span style={{ color: "var(--tx-3)" }}> [{origin}]</span> : null}
    </span>
  );
  return (
    <div style={{ fontFamily: "var(--font-mono, monospace)", fontSize: 12 }}>
      {field("harness", tuple.harness, tuple.harness_origin)}
      {field("provider", tuple.provider, tuple.provider_origin)}
      {field("model", tuple.model, tuple.model_origin)}
    </div>
  );
}

/** A model text field with the chosen provider's catalog suggested, best-effort. */
function ModelField({
  value,
  provider,
  onChange,
}: {
  value: string;
  provider: string;
  onChange: (v: string) => void;
}) {
  const [models, setModels] = React.useState<string[]>([]);
  React.useEffect(() => {
    if (!provider) {
      setModels([]);
      return;
    }
    let live = true;
    void fetchProviderCatalog(provider)
      .then((c) => {
        if (live) setModels(c.models.map((m) => m.id));
      })
      .catch(() => {
        if (live) setModels([]);
      });
    return () => {
      live = false;
    };
  }, [provider]);
  return (
    <>
      <TextInput
        value={value}
        onChange={(e) => onChange(e.target.value)}
        placeholder="model id (blank inherits)"
        list={`models-${provider}`}
        aria-label="model"
      />
      <datalist id={`models-${provider}`}>
        {models.map((m) => (
          <option key={m} value={m} />
        ))}
      </datalist>
    </>
  );
}

function RoutingEditor({
  value,
  providers,
  harnesses,
  onChange,
}: {
  value: ModelsRoutingView;
  providers: string[];
  harnesses: string[];
  onChange: (v: ModelsRoutingView) => void;
}) {
  const harnessOptions = [
    { value: "", label: "inherit" },
    ...harnesses.map((h) => ({ value: h, label: h })),
  ];
  const providerOptions = [
    { value: "", label: "inherit / none" },
    ...providers.map((p) => ({ value: p, label: p })),
  ];
  const set = (patch: Partial<ModelsRoutingView>) => onChange({ ...value, ...patch });
  return (
    <div style={{ display: "grid", gap: 10 }}>
      <Field label="harness" inline>
        <Select
          value={value.harness}
          options={harnessOptions}
          onChange={(v) => set({ harness: v })}
          width="100%"
        />
      </Field>
      <Field label="provider" inline>
        <Select
          value={value.provider}
          options={providerOptions}
          onChange={(v) => set({ provider: v })}
          width="100%"
        />
      </Field>
      <Field label="model" inline>
        <ModelField
          value={value.model}
          provider={value.provider || value.harness}
          onChange={(v) => set({ model: v })}
        />
      </Field>
      <Field label="effort" inline>
        <TextInput
          value={value.effort}
          onChange={(e) => set({ effort: e.target.value })}
          placeholder="effort (blank inherits)"
          aria-label="effort"
        />
      </Field>
    </div>
  );
}

export function ModelsTab() {
  const qc = useQueryClient();
  const query = useQuery({ queryKey: MODELS_QUERY_KEY, queryFn: fetchTeamsModels, retry: false });
  const [error, setError] = React.useState("");
  const [undo, setUndo] = React.useState<ModelsRoutingEntry[] | null>(null);

  const apply = useMutation({
    mutationFn: (req: TeamsModelsRequest) => applyTeamsModels(req),
    onSuccess: (res, req) => {
      qc.setQueryData(MODELS_QUERY_KEY, res.view);
      setError("");
      if (req.target === "move_all") setUndo(res.previous ?? []);
      if (req.target === "undo") setUndo(null);
    },
    onError: (e: unknown) => setError(errText(e)),
  });

  if (query.isLoading) {
    return (
      <SectionCard title="Models" desc="Live harness, provider and model for every teammate.">
        <Note>Loading…</Note>
      </SectionCard>
    );
  }
  if (query.isError) {
    const msg = errText(query.error);
    // The daemon answers 409 teams_disabled when Teams is off / has no runtime home.
    if (msg.includes("teams_disabled")) {
      return (
        <SectionCard title="Models" desc="Live harness, provider and model for every teammate.">
          <Note>Teams is off. Enable it on the Teams tab to manage models here.</Note>
        </SectionCard>
      );
    }
    return (
      <SectionCard title="Models" desc="Live harness, provider and model for every teammate.">
        <Note tone="red">Could not read models: {msg}</Note>
      </SectionCard>
    );
  }

  const view = query.data!;
  const save = (req: TeamsModelsRequest) => apply.mutate(req);

  return (
    <div style={{ display: "flex", flexDirection: "column", gap: 18 }}>
      {error ? (
        <SectionCard title="Change refused">
          <Note tone="red">{error}</Note>
        </SectionCard>
      ) : null}

      <SectionCard
        title="Move all workers to…"
        desc="Apply one tuple to every non-reviewer teammate, for a burn-window switch. Undo restores each one's previous tuple."
        action={
          undo && undo.length > 0 ? (
            <Button variant="subtle" size="sm" onClick={() => save({ target: "undo", previous: undo })}>
              Undo move-all ({undo.length})
            </Button>
          ) : null
        }
      >
        <MoveAllForm
          providers={view.providers}
          harnesses={view.harnesses}
          disabled={apply.isPending}
          onApply={(routing) => save({ target: "move_all", routing })}
        />
      </SectionCard>

      <SectionCard title="Teammates" desc="Each row shows the tuple a run resolves, and edits that teammate's profile.">
        <div style={{ display: "flex", flexDirection: "column", gap: 16 }}>
          {view.identities.map((row) => (
            <IdentityRow
              key={row.name}
              row={row}
              providers={view.providers}
              harnesses={view.harnesses}
              disabled={apply.isPending}
              onSave={(routing) => save({ target: "identity", name: row.name, routing })}
            />
          ))}
        </div>
      </SectionCard>

      <SectionCard title="Reviews" desc="The model/provider/effort a review run uses, per harness. In-flight reviews keep the model they started with.">
        <div style={{ display: "flex", flexDirection: "column", gap: 16 }}>
          {view.review.map((row) => (
            <ReviewRow
              key={row.harness}
              row={row}
              providers={view.providers}
              harnesses={view.harnesses}
              disabled={apply.isPending}
              onSave={(next) => save({ target: "review", harness: row.harness, ...next })}
            />
          ))}
        </div>
      </SectionCard>

      <SectionCard title="Manager" desc="The manager's own tuple, independent of every teammate.">
        <ManagerRow
          row={view.manager}
          providers={view.providers}
          harnesses={view.harnesses}
          disabled={apply.isPending}
          onSave={(routing) => save({ target: "manager", routing })}
        />
      </SectionCard>
    </div>
  );
}

function MoveAllForm({
  providers,
  harnesses,
  disabled,
  onApply,
}: {
  providers: string[];
  harnesses: string[];
  disabled: boolean;
  onApply: (routing: ModelsRoutingView) => void;
}) {
  const [routing, setRouting] = React.useState<ModelsRoutingView>(EMPTY_ROUTING);
  return (
    <div style={{ display: "flex", flexDirection: "column", gap: 12 }}>
      <RoutingEditor value={routing} providers={providers} harnesses={harnesses} onChange={setRouting} />
      <div>
        <Button
          variant="primary"
          size="sm"
          disabled={disabled || (!routing.harness && !routing.model)}
          onClick={() => onApply(routing)}
        >
          Move all workers
        </Button>
      </div>
    </div>
  );
}

function IdentityRow({
  row,
  providers,
  harnesses,
  disabled,
  onSave,
}: {
  row: TeamsModelsView["identities"][number];
  providers: string[];
  harnesses: string[];
  disabled: boolean;
  onSave: (routing: ModelsRoutingView) => void;
}) {
  const [draft, setDraft] = React.useState<ModelsRoutingView>(row.configured);
  React.useEffect(() => setDraft(row.configured), [row.configured]);
  return (
    <div style={{ borderTop: "1px solid var(--line)", paddingTop: 14 }}>
      <div style={{ display: "flex", alignItems: "baseline", gap: 8, marginBottom: 6 }}>
        <strong>{row.name}</strong>
        <span style={{ color: "var(--tx-3)", fontSize: 12 }}>profile {row.profile}</span>
        {row.code_reviewer ? <span style={{ fontSize: 11, color: "var(--tx-3)" }}>· reviewer</span> : null}
      </div>
      <TupleLine tuple={row.resolved} />
      {row.profile_error ? <Note tone="red">{row.profile_error}</Note> : null}
      <div style={{ marginTop: 8 }}>
        <RoutingEditor value={draft} providers={providers} harnesses={harnesses} onChange={setDraft} />
        <div style={{ marginTop: 8 }}>
          <Button variant="primary" size="sm" disabled={disabled} onClick={() => onSave(draft)}>
            Save {row.name}
          </Button>
        </div>
      </div>
    </div>
  );
}

function ReviewRow({
  row,
  providers,
  harnesses,
  disabled,
  onSave,
}: {
  row: TeamsModelsView["review"][number];
  providers: string[];
  harnesses: string[];
  disabled: boolean;
  onSave: (next: { model: string; provider: string; effort: string }) => void;
}) {
  const [draft, setDraft] = React.useState<ModelsRoutingView>({
    harness: row.harness,
    provider: row.provider,
    model: row.model,
    effort: row.effort,
  });
  React.useEffect(
    () => setDraft({ harness: row.harness, provider: row.provider, model: row.model, effort: row.effort }),
    [row],
  );
  return (
    <div style={{ borderTop: "1px solid var(--line)", paddingTop: 14 }}>
      <strong>{row.harness}</strong>
      <div style={{ marginTop: 8 }}>
        <RoutingEditor value={draft} providers={providers} harnesses={harnesses} onChange={setDraft} />
        <div style={{ marginTop: 8 }}>
          <Button
            variant="primary"
            size="sm"
            disabled={disabled}
            onClick={() => onSave({ model: draft.model, provider: draft.provider, effort: draft.effort })}
          >
            Save review · {row.harness}
          </Button>
        </div>
      </div>
    </div>
  );
}

function ManagerRow({
  row,
  providers,
  harnesses,
  disabled,
  onSave,
}: {
  row: TeamsModelsView["manager"];
  providers: string[];
  harnesses: string[];
  disabled: boolean;
  onSave: (routing: ModelsRoutingView) => void;
}) {
  const [draft, setDraft] = React.useState<ModelsRoutingView>(row.configured);
  React.useEffect(() => setDraft(row.configured), [row.configured]);
  return (
    <div>
      <TupleLine tuple={row.resolved} />
      <div style={{ marginTop: 8 }}>
        <RoutingEditor value={draft} providers={providers} harnesses={harnesses} onChange={setDraft} />
        <div style={{ marginTop: 8 }}>
          <Button variant="primary" size="sm" disabled={disabled} onClick={() => onSave(draft)}>
            Save manager
          </Button>
        </div>
      </div>
    </div>
  );
}
