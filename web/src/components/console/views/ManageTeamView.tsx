import * as React from "react";
import { Button, Note, Seg, Select, Stepper, TagInput, Toggle } from "@/components/console";
import { useSaveTeamsConfig, useTeamsConfigQuery } from "@/hooks/useTeams";
import {
  defaultIdentityOptions,
  joinRowLabels,
  managerModelDisabled,
  profileOptions,
  rowLabels,
  showsHindsightFields,
  starvedTimeoutMs,
} from "@/lib/console-manage";
import {
  MANAGER_MODES,
  MASKED_API_KEY,
  MEMORY_BACKENDS,
  MIN_MODEL_TIMEOUT_MS,
  MIN_QUORUM_REVIEWERS,
  draftErrors,
  emptyRow,
  errText,
  isStoredSecret,
  quorumNote,
  teamsYamlSnippet,
  toConfig,
  toDraft,
  type RosterDraft,
  type TeamsDraft,
} from "@/lib/teams-model";
import { teammateColorAt } from "@/theme/teammates";
import type { TeamsConfig } from "@/lib/api";
import "@/theme/console-manage.css";

// Manage team — STUDIO-681 §7, the fifth slice of the dashboard redesign: `teams.yaml` as a
// form, so no one has to hand-edit YAML.
//
// It reads and writes exactly one route, `GET`/`POST /api/v1/teams/config`, through the hooks
// the Settings enable flow already uses — no endpoint here is new, which §11 makes a hard rule.
//
// It also adds no MODEL. `lib/teams-model.ts` already loads a draft out of teams.yaml,
// validates it the way `Teams::validate` does, renders its YAML and turns it back into a config
// for the POST, because the Podium Settings editor (STUDIO-652/667) was built on it first. Two
// editors of one file must not disagree about what the file says, so this view composes §1
// components over those same functions and contributes only the §7 reveal rules
// (`lib/console-manage.ts`). The practical consequence is that saving from either surface
// writes byte-identical YAML, including the carry-forward of keys neither editor models.

export interface ManageTeamViewProps {
  /** Route away — the breadcrumb and Cancel both return to the Teams console (§7). */
  onNavigate: (route: "teams") => void;
}

export function ManageTeamView({ onNavigate }: ManageTeamViewProps) {
  const cfg = useTeamsConfigQuery();
  const save = useSaveTeamsConfig();
  const [draft, setDraft] = React.useState<TeamsDraft | null>(null);
  const [showYaml, setShowYaml] = React.useState(false);

  const loaded = cfg.data;
  // Re-hydrate whenever the fetched view changes identity — a first load, a refetch, or the
  // saved copy `useSaveTeamsConfig` writes back into the cache. Nothing else changes it (the
  // query does not poll and does not refetch on focus), so an in-progress edit is not at risk
  // of being overwritten mid-keystroke, and after a save the form correctly shows what is now
  // on disk rather than what was typed.
  React.useEffect(() => {
    if (loaded) setDraft(toDraft(loaded.config, loaded.present));
  }, [loaded]);

  // A daemon with no on-disk runtime home (`--no-store`, `storage.path` off) answers 409
  // teams_config_unavailable. Report its reason verbatim rather than rendering a form whose
  // Save could not possibly land.
  if (cfg.isError) {
    return (
      <Page onNavigate={onNavigate}>
        <div role="alert">
          <Note variant="warn">Could not read teams.yaml: {errText(cfg.error)}</Note>
        </div>
      </Page>
    );
  }
  if (!draft) {
    return (
      <Page onNavigate={onNavigate}>
        <div className="empty">Reading teams.yaml…</div>
      </Page>
    );
  }

  const restartRequired = loaded?.restart_required ?? true;
  const errors = draftErrors(draft);
  // Any edit retires the outcome of the previous save: a "Saved" line sitting above changed
  // fields would claim something about the form that is no longer true.
  const change = (next: TeamsDraft) => {
    if (save.isSuccess || save.isError) save.reset();
    setDraft(next);
  };
  const set = <K extends keyof TeamsDraft>(key: K, value: TeamsDraft[K]) => change({ ...draft, [key]: value });
  const setRow = (i: number, patch: Partial<RosterDraft>) =>
    change({ ...draft, roster: draft.roster.map((r, n) => (n === i ? { ...r, ...patch } : r)) });

  const onSave = () => {
    if (errors.length > 0) return;
    save.mutate(toConfig(draft, loaded?.config) as TeamsConfig);
  };

  return (
    <Page onNavigate={onNavigate}>
      <p className="lead">
        Everything <code>teams.yaml</code> holds, as a form. No one should have to hand-edit YAML.
        {restartRequired ? " Changes apply on the next daemon restart." : ""}
      </p>

      <Roster draft={draft} onRow={setRow} onChange={change} />
      <Manager draft={draft} set={set} />
      <MemoryAndQuorum draft={draft} storedKey={isStoredSecret(loaded?.config?.memory?.api_key)} onChange={change} set={set} />

      <Section title="Advanced">
        <Field
          label="Prompt budget"
          hint="Total bytes the Teams preamble may spend on a run's first turn — identity header, profile, room catch-up and recall together. On overflow the oldest room items go first, then recall; the identity header is never dropped."
        >
          <Stepper
            value={draft.promptBudgetBytes}
            onChange={(v) => set("promptBudgetBytes", v)}
            label="Prompt budget"
            unit="bytes"
          />
        </Field>
      </Section>

      {errors.length > 0 ? (
        <Note variant="warn">
          The daemon would reject this: {errors.join("; ")}.
        </Note>
      ) : null}
      {save.isError ? (
        <div role="alert">
          <Note variant="warn">teams.yaml was not written: {errText(save.error)}</Note>
        </div>
      ) : null}

      {showYaml ? <pre className="yaml">{teamsYamlSnippet(draft)}</pre> : null}

      <div className="barbot">
        <Button variant="link" aria-expanded={showYaml} onClick={() => setShowYaml(!showYaml)}>
          ⟨⟩ View as YAML
        </Button>
        <div className="sp" />
        {save.isSuccess ? (
          <span className="lft" role="status">
            {restartRequired ? "Saved. Changes take effect when the daemon restarts." : "Saved."}
          </span>
        ) : (
          restartRequired && <span className="lft">restart to apply</span>
        )}
        <Button variant="sec" onClick={() => onNavigate("teams")}>
          Cancel
        </Button>
        <Button variant="pri" disabled={errors.length > 0 || save.isPending} onClick={onSave}>
          {save.isPending ? "Saving…" : "Save changes"}
        </Button>
      </div>
    </Page>
  );
}

/** The page chrome every state shares — breadcrumb, title, and the main column. */
function Page({ onNavigate, children }: { onNavigate: (r: "teams") => void; children: React.ReactNode }) {
  return (
    <section>
      <div className="crumbs">
        {/* A button, not a link: it performs an action (routing), not a document jump. */}
        <button type="button" className="link" onClick={() => onNavigate("teams")}>
          Teams
        </button>{" "}
        · Manage team
      </div>
      <div className="head">
        <h1>Manage team</h1>
      </div>
      {children}
    </section>
  );
}

function Section({ title, desc, children }: { title: string; desc?: React.ReactNode; children: React.ReactNode }) {
  return (
    <div className="sect">
      <div className="sh">
        <h2>{title}</h2>
        {desc ? <p>{desc}</p> : null}
      </div>
      <div className="bd">{children}</div>
    </div>
  );
}

/**
 * One labelled row. `htmlFor` is passed only for the plain inputs and selects that can carry an
 * id; the composite controls (Seg, Stepper, Toggle, TagInput) name themselves through the
 * `label` prop §1.3 requires, so their visible label here stays purely visual.
 */
function Field({
  label,
  hint,
  htmlFor,
  children,
}: {
  label: string;
  hint?: React.ReactNode;
  htmlFor?: string;
  children: React.ReactNode;
}) {
  return (
    <div className="field">
      <div className="lb">
        <label htmlFor={htmlFor}>{label}</label>
        {hint ? <div className="h">{hint}</div> : null}
      </div>
      <div className="ct">{children}</div>
    </div>
  );
}

function Roster({
  draft,
  onRow,
  onChange,
}: {
  draft: TeamsDraft;
  onRow: (i: number, patch: Partial<RosterDraft>) => void;
  onChange: (next: TeamsDraft) => void;
}) {
  return (
    <Section
      title="Roster"
      desc={
        <>
          Each teammate is an identity work routes to by the <code>rhapsody:@name</code> label.
        </>
      }
    >
      <table className="rform">
        <thead>
          <tr>
            <th>Name</th>
            <th>Profile</th>
            <th>Extra labels</th>
            <th>Max concurrent</th>
            <th />
          </tr>
        </thead>
        <tbody>
          {draft.roster.map((row, i) => (
            <RosterRow
              // Index-keyed on purpose: a row's identity here IS its position — the name is the
              // field being edited, so keying on it would remount the input on every keystroke.
              key={i}
              n={i + 1}
              row={row}
              color={teammateColorAt(i)}
              onChange={(patch) => onRow(i, patch)}
              onRemove={() => onChange({ ...draft, roster: draft.roster.filter((_, n) => n !== i) })}
            />
          ))}
        </tbody>
      </table>
      <button type="button" className="addrow" onClick={() => onChange({ ...draft, roster: [...draft.roster, emptyRow()] })}>
        + Add teammate
      </button>
      <Note>
        <b>Max concurrent 0</b> means unlimited — the teammate takes as much work as the daemon's own
        dispatch limit allows. Any other number caps their live runs.
      </Note>
    </Section>
  );
}

function RosterRow({
  n,
  row,
  color,
  onChange,
  onRemove,
}: {
  n: number;
  row: RosterDraft;
  color: string;
  onChange: (patch: Partial<RosterDraft>) => void;
  onRemove: () => void;
}) {
  return (
    <tr>
      <td>
        <span className="nm">
          <span className="av" style={{ background: color }} />
          <input
            type="text"
            aria-label={`Teammate ${n} name`}
            placeholder="alice"
            value={row.name}
            onChange={(e) => onChange({ name: e.target.value })}
          />
        </span>
      </td>
      <td>
        <Select
          aria-label={`Teammate ${n} profile`}
          value={row.profile}
          options={profileOptions(row.profile)}
          onChange={(e) => onChange({ profile: e.target.value })}
        />
      </td>
      <td>
        <TagInput
          label={`Teammate ${n} labels`}
          placeholder="add a label…"
          tags={rowLabels(row)}
          onChange={(tags) => onChange({ labels: joinRowLabels(tags) })}
        />
      </td>
      <td>
        <Stepper
          value={row.maxConcurrent}
          onChange={(v) => onChange({ maxConcurrent: v })}
          label={`Teammate ${n} max concurrent`}
        />
      </td>
      <td>
        <button type="button" className="rm" aria-label={`Remove teammate ${n}`} onClick={onRemove}>
          ×
        </button>
      </td>
    </tr>
  );
}

/** The manager mode Seg's labels — the schema's values, spaced the way the prototype prints them. */
const MODE_OPTIONS = MANAGER_MODES.map((m) => ({ value: m, label: m === "labels+model" ? "labels + model" : m }));

function Manager({
  draft,
  set,
}: {
  draft: TeamsDraft;
  set: <K extends keyof TeamsDraft>(key: K, value: TeamsDraft[K]) => void;
}) {
  const starved = starvedTimeoutMs(draft);
  return (
    <Section title="Manager (triage)" desc="How unlabelled work gets routed.">
      <Field label="Mode" hint="Deterministic, or a model turn that falls back to deterministic on a miss.">
        <Seg
          accent
          aria-label="Mode"
          options={MODE_OPTIONS}
          value={draft.managerMode}
          onChange={(v) => set("managerMode", v)}
        />
      </Field>
      <Field label="Model" htmlFor="mgr-model">
        <input
          id="mgr-model"
          type="text"
          className="mono"
          style={{ maxWidth: 280 }}
          // `off` is single-identity Teams: no routing runs, so no model is ever consulted and
          // the field that names one is inert rather than merely unused (box 5.2).
          disabled={managerModelDisabled(draft.managerMode)}
          value={draft.managerModel}
          placeholder="claude-opus-5"
          onChange={(e) => set("managerModel", e.target.value)}
        />
      </Field>
      <Field label="Turn timeout" hint="Exceeded ⇒ the deterministic answer stands. Never blocks dispatch.">
        <Stepper
          value={draft.managerTimeoutMs}
          onChange={(v) => set("managerTimeoutMs", v)}
          label="Turn timeout"
          unit="ms"
        />
        {starved === null ? null : (
          <Note variant="warn" className="mt">
            At <b>{starved} ms</b> the model turn always times out — every ticket falls to the
            deterministic router. The floor is <b>{MIN_MODEL_TIMEOUT_MS} ms</b>; the shipped default is
            60000.
          </Note>
        )}
      </Field>
      <Field label="Default identity" hint="Fallback picks this teammate; otherwise least-loaded." htmlFor="mgr-identity">
        <Select
          id="mgr-identity"
          aria-label="Default identity"
          value={draft.defaultIdentity}
          options={defaultIdentityOptions(draft)}
          onChange={(e) => set("defaultIdentity", e.target.value)}
        />
      </Field>
    </Section>
  );
}

function MemoryAndQuorum({
  draft,
  storedKey,
  onChange,
  set,
}: {
  draft: TeamsDraft;
  storedKey: boolean;
  onChange: (next: TeamsDraft) => void;
  set: <K extends keyof TeamsDraft>(key: K, value: TeamsDraft[K]) => void;
}) {
  return (
    <Section title="Memory & quorum">
      <Field label="Memory backend">
        <Seg
          accent
          aria-label="Memory backend"
          options={[...MEMORY_BACKENDS]}
          value={draft.backend}
          onChange={(v) => set("backend", v)}
        />
      </Field>
      {showsHindsightFields(draft.backend) ? (
        <>
          <Field label="Hindsight endpoint" htmlFor="mem-endpoint">
            <input
              id="mem-endpoint"
              type="text"
              className="mono in-wide"
              placeholder="https://hindsight.example.ts.net/mcp/"
              value={draft.memoryEndpoint}
              onChange={(e) => set("memoryEndpoint", e.target.value)}
            />
          </Field>
          <ApiKeyField draft={draft} storedKey={storedKey} onChange={onChange} />
        </>
      ) : null}
      <Field label="Recall top-k" hint="How many facts a recall returns.">
        <Stepper value={draft.recallTopK} onChange={(v) => set("recallTopK", v)} label="Recall top-k" />
      </Field>
      <Field label="Review quorum" hint="Fan a hand-off's review out to other teammates.">
        <div className="togline">
          <Toggle
            label="Review quorum"
            pressed={draft.quorumEnabled}
            onChange={(v) => set("quorumEnabled", v)}
          />
          <span className="tx">{draft.quorumEnabled ? "Enabled ·" : "Disabled ·"}</span>
          <Stepper
            value={draft.quorumReviewers}
            onChange={(v) => set("quorumReviewers", v)}
            min={MIN_QUORUM_REVIEWERS}
            label="Reviewers"
            unit="reviewers"
          />
        </div>
        {/* The quorum is the most expensive switch in teams.yaml — each hand-off buys `reviewers`
            extra agent runs — so the price is stated before the toggle is flipped. */}
        {draft.quorumEnabled ? <Note className="mt">{quorumNote(draft)}</Note> : null}
      </Field>
    </Section>
  );
}

/**
 * The `memory.api_key` field. A `$NAME` is a POINTER, not a secret — the daemon resolves it from
 * its own environment, so it stays visible and editable. A LITERAL is masked: `toDraft` refuses
 * to load one into the draft, so there is no state this component could render back, and
 * "Replace" is the only way to change it.
 *
 * Replace is destructive and one click away, so it is reversible: `storedKey` comes from the
 * FETCHED config rather than from the draft precisely so the way back outlives the click.
 */
function ApiKeyField({
  draft,
  storedKey,
  onChange,
}: {
  draft: TeamsDraft;
  storedKey: boolean;
  onChange: (next: TeamsDraft) => void;
}) {
  if (draft.apiKeyStored) {
    return (
      <Field label="API key" hint="A key is stored in teams.yaml and is never shown.">
        <div className="togline">
          <input type="text" className="mono" readOnly aria-label="API key" value={MASKED_API_KEY} style={{ maxWidth: 220 }} />
          <Button variant="link" onClick={() => onChange({ ...draft, apiKey: "", apiKeyStored: false })}>
            Replace
          </Button>
        </div>
      </Field>
    );
  }
  return (
    <Field
      label="API key"
      hint={
        storedKey
          ? "Replacing the key stored in teams.yaml. Saving this blank removes the stored key, which leaves the backend unauthenticated."
          : "Name an environment variable — $HINDSIGHT_API_KEY is read from the daemon's environment, so the secret stays out of the file."
      }
      htmlFor="mem-key"
    >
      <div className="togline">
        <input
          id="mem-key"
          type="text"
          className="mono"
          aria-label="API key"
          placeholder="$HINDSIGHT_API_KEY"
          value={draft.apiKey}
          style={{ maxWidth: 220 }}
          onChange={(e) => onChange({ ...draft, apiKey: e.target.value })}
        />
        {storedKey ? (
          <Button variant="link" onClick={() => onChange({ ...draft, apiKey: "", apiKeyStored: true })}>
            Keep existing
          </Button>
        ) : null}
      </div>
    </Field>
  );
}
