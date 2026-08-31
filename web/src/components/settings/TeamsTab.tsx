import * as React from "react";
import { Button, Collapsible, Field, SectionCard, Select, Sliders, Stepper, TextInput, Toggle } from "@/components/ui";
import { useSaveTeamsConfig, useTeamsConfigQuery } from "@/hooks/useTeams";
import {
  draftErrors,
  emptyRow,
  errText,
  isStoredSecret,
  MANAGER_MODES,
  MASKED_API_KEY,
  MEMORY_BACKENDS,
  MIN_QUORUM_REVIEWERS,
  quorumNote,
  teamsYamlSnippet,
  toConfig,
  toDraft,
  type RosterDraft,
  type TeamsDraft,
} from "@/lib/teams-model";
import type { TeamsConfig } from "@/lib/api";

// TeamsTab — the Settings surface for Rhapsody Teams (STUDIO-652), and specifically the ENABLE
// FLOW: the one deliberate act that creates `~/.rhapsody/teams.yaml`.
//
// The never-seed rule is the whole shape of this tab. An absent teams.yaml means Teams is off, and
// nothing in the daemon ever creates it implicitly — not booting, not reading, not opening this
// page. So the off state renders as "Teams is off" plus an explicit "Create teams.yaml…" that opens
// the editor, and the file appears only when someone presses Save. Reading this tab writes nothing.
//
// Unlike the rest of Settings there is NO autosave here, deliberately: autosaving a file whose
// absence is a feature would create it the moment the tab was opened.
export function TeamsTab() {
  const cfg = useTeamsConfigQuery();
  const save = useSaveTeamsConfig();
  const [editing, setEditing] = React.useState(false);
  const [draft, setDraft] = React.useState<TeamsDraft | null>(null);

  const view = cfg.data;
  const present = view?.present ?? false;
  const onDisk = view?.config;

  const open = () => {
    setDraft(toDraft(onDisk, present));
    setEditing(true);
  };

  if (cfg.isLoading) return <Note>Loading…</Note>;
  // A daemon with no on-disk runtime home (--no-store, storage.path off/:memory:) has nowhere to
  // keep a teams.yaml and answers 409 teams_config_unavailable. Report its reason verbatim rather
  // than rendering an editor whose Save could not possibly land.
  if (cfg.isError) {
    return (
      <SectionCard title="Teams" desc="Named teammates with their own profiles, memory and a shared room.">
        <Note tone="red">Could not read teams.yaml: {errText(cfg.error)}</Note>
      </SectionCard>
    );
  }

  if (editing && draft) {
    return (
      <TeamsEditor
        draft={draft}
        onChange={setDraft}
        // Whether a literal is on disk is a fact about the FETCHED config, not about the draft:
        // `draft.apiKeyStored` flips to false the moment Replace is pressed, and deriving the
        // "there is something to keep" affordance from the draft would make the way back vanish
        // with the click that created the need for it.
        hasStoredKey={isStoredSecret(onDisk?.memory?.api_key)}
        path={view?.path ?? ""}
        restartRequired={view?.restart_required ?? true}
        saving={save.isPending}
        error={save.isError ? errText(save.error) : ""}
        onCancel={() => setEditing(false)}
        onSave={() => {
          save.mutate(toConfig(draft, onDisk) as TeamsConfig, {
            onSuccess: () => setEditing(false),
          });
        }}
      />
    );
  }

  return (
    <SectionCard
      title="Teams"
      desc="Named teammates with their own profiles, memory and a shared room. Assignment is a Linear label; the roster lives in teams.yaml."
      action={
        <Button type="button" variant="secondary" size="sm" onClick={open}>
          {present ? "Edit teams.yaml…" : "Create teams.yaml…"}
        </Button>
      }
    >
      {!present ? (
        <>
          <Status label="Teams is off" detail="No teams.yaml — which is the shipped state. Nothing creates one until you do." />
          <Path path={view?.path ?? ""} />
        </>
      ) : view?.error ? (
        <>
          <Status
            tone="red"
            label="Teams is off — teams.yaml was rejected"
            detail={view.error}
          />
          <Path path={view.path} />
        </>
      ) : (
        <>
          <Status
            tone={onDisk?.enabled ? "on" : undefined}
            label={onDisk?.enabled ? "Teams is on" : "teams.yaml exists, but enabled is false"}
            detail={`${onDisk?.roster.length ?? 0} teammate(s) · assignment: ${onDisk?.manager.mode} · memory: ${onDisk?.memory.backend}`}
          />
          <Path path={view?.path ?? ""} />
        </>
      )}
    </SectionCard>
  );
}

// TeamsEditor — every field in the `Teams` schema (crates/config/src/teams.rs), grouped so the
// common path stays trivial (STUDIO-667). Two rules shape the layout:
//
//   1. The v1 fields — the toggle, assignment mode, memory backend, and the name/profile/labels
//      roster row — keep the prominence they had. A fresh operator sees exactly what they saw.
//   2. Everything a fresh operator should not have to face sits behind an "Advanced" disclosure,
//      collapsed by default. The editor's job is to make the common path trivial, not to render
//      the schema.
//
// Validation stays SERVER-SIDE and verbatim: `draftErrors` mirrors the three rules `Teams::validate`
// enforces so Save can be disabled rather than inviting a doomed round-trip, and everything else —
// the reserved-name rule, the label-safe rule as the daemon words it — arrives as the daemon's own
// sentence from the POST.
function TeamsEditor({
  draft,
  onChange,
  hasStoredKey,
  path,
  restartRequired,
  saving,
  error,
  onCancel,
  onSave,
}: {
  draft: TeamsDraft;
  onChange: (d: TeamsDraft) => void;
  hasStoredKey: boolean;
  path: string;
  restartRequired: boolean;
  saving: boolean;
  error: string;
  onCancel: () => void;
  onSave: () => void;
}) {
  const errors = draftErrors(draft);
  const set = <K extends keyof TeamsDraft>(key: K, value: TeamsDraft[K]) => onChange({ ...draft, [key]: value });
  const setRow = (i: number, row: Partial<RosterDraft>) =>
    onChange({ ...draft, roster: draft.roster.map((r, n) => (n === i ? { ...r, ...row } : r)) });
  const rosterNames = draft.roster.map((r) => r.name.trim()).filter((n) => n !== "");

  return (
    <div style={{ display: "flex", flexDirection: "column", gap: 18 }}>
      <SectionCard
        title="Edit teams.yaml"
        desc="Nothing is written until you press Save. A rejected config is never written at all — the daemon validates first and leaves the file exactly as it was."
      >
        <Row label="Enable Teams">
          <Toggle checked={draft.enabled} onChange={(enabled) => set("enabled", enabled)} aria-label="Enable Teams" />
        </Row>
        <Row label="Assignment">
          <Select
            value={draft.managerMode}
            width={220}
            options={MANAGER_MODES.map((m) => ({ value: m, label: m }))}
            onChange={(managerMode) => set("managerMode", managerMode)}
          />
        </Row>
        <Row label="Memory">
          <Select
            value={draft.backend}
            width={220}
            options={MEMORY_BACKENDS.map((m) => ({ value: m, label: m }))}
            onChange={(backend) => set("backend", backend)}
          />
        </Row>
        <div style={{ paddingTop: 10 }}>
          <Collapsible label="Advanced" icon={Sliders}>
            <Field
              label="Prompt budget"
              inline
              hint="Total bytes the Teams preamble may spend on a run's first turn — the identity header, profile prose, room catch-up and memory recall together. On overflow the oldest room items go first, then recall; the identity header is never dropped. 0 ⇒ the daemon's default (16000)."
            >
              <Stepper
                value={draft.promptBudgetBytes}
                onChange={(v) => set("promptBudgetBytes", v)}
                min={0}
                max={1000000}
                suffix="bytes"
              />
            </Field>
          </Collapsible>
        </div>
      </SectionCard>

      <SectionCard
        title="Review quorum"
        desc="When a teammate hands off a ticket with an open PR, Rhapsody files review tickets for other teammates so at least one more pair of eyes reads the work independently. Off by default."
      >
        <Field
          label="Fan out reviews on handoff"
          inline
          hint={draft.quorumEnabled ? quorumNote(draft) : "Each handoff would cost one extra agent run per reviewer — the most expensive switch in this file, which is why it is opt-in."}
        >
          <div style={{ display: "flex", justifyContent: "flex-end" }}>
            <Toggle
              checked={draft.quorumEnabled}
              onChange={(v) => set("quorumEnabled", v)}
              aria-label="Fan out reviews on handoff"
            />
          </div>
        </Field>
        {draft.quorumEnabled ? (
          <Field
            label="Reviewers per handoff"
            inline
            hint="How many teammates review one handoff. Chosen least-loaded first, and never the author."
          >
            <Stepper
              value={draft.quorumReviewers}
              onChange={(v) => set("quorumReviewers", v)}
              min={MIN_QUORUM_REVIEWERS}
              max={20}
            />
          </Field>
        ) : null}
      </SectionCard>

      <SectionCard
        title="Manager"
        desc="How a ticket finds a teammate: its `rhapsody:@<name>` label first, then roster labels against ticket labels."
      >
        <Field
          label="Default teammate"
          inline
          hint="Who takes a ticket no label matched. “None” runs it without an identity, exactly as a Teams-off daemon would."
        >
          <Select
            value={draft.defaultIdentity}
            width={320}
            // `placeholder="none"` rather than the generic "Select…": a value orphaned by renaming
            // its teammate matches no option, and `toConfig` clears it on save. Showing "none" makes
            // the control agree with the preview and with what is actually written.
            placeholder="none"
            options={[
              { value: "", label: "none", mono: false },
              ...rosterNames.map((n) => ({ value: n, label: n })),
            ]}
            onChange={(v) => set("defaultIdentity", v)}
          />
        </Field>
        <Collapsible label="Advanced" icon={Sliders}>
          <Field
            label="Triage model"
            inline
            hint="Consulted ONLY in `labels+model`, and only when no label matched. Blank ⇒ the daemon's usual model."
          >
            <TextInput
              mono
              value={draft.managerModel}
              aria-label="Triage model"
              placeholder="claude-opus-5"
              onChange={(e) => set("managerModel", e.target.value)}
            />
          </Field>
          <Field label="Triage max tokens" inline hint="Hard cap on that arbitration turn.">
            <Stepper value={draft.managerMaxTokens} onChange={(v) => set("managerMaxTokens", v)} min={1} max={200000} />
          </Field>
          <Field
            label="Triage timeout"
            inline
            hint="Exceeded ⇒ the deterministic answer stands. Triage never blocks dispatch. A turn spawns a subprocess and waits on a model, so below 15000ms the daemon warns the manager is starved."
          >
            <Stepper
              value={draft.managerTimeoutMs}
              onChange={(v) => set("managerTimeoutMs", v)}
              min={1}
              max={600000}
              suffix="ms"
            />
          </Field>
        </Collapsible>
      </SectionCard>

      {draft.backend !== "none" ? (
        <SectionCard
          title="Memory"
          desc={
            draft.backend === "hindsight"
              ? "Teammates remember across runs in shared Hindsight banks, so a machine is not where the memory lives."
              : "Teammates remember across runs in on-disk banks under the daemon's runtime home."
          }
        >
          {draft.backend === "hindsight" ? (
            <>
              <Field
                label="Endpoint"
                inline
                hint="The Hindsight service base URL. Blank ⇒ the daemon warns and runs memoryless."
              >
                <TextInput
                  mono
                  value={draft.memoryEndpoint}
                  aria-label="Endpoint"
                  placeholder="https://hindsight.example.com"
                  onChange={(e) => set("memoryEndpoint", e.target.value)}
                />
              </Field>
              <ApiKeyField draft={draft} onChange={onChange} hasStoredKey={hasStoredKey} />
            </>
          ) : null}
          <Field label="Bank prefix" inline hint="A teammate's bank id is `<prefix><name>` unless their row overrides it.">
            <TextInput
              mono
              value={draft.bankPrefix}
              aria-label="Bank prefix"
              placeholder="agent-"
              onChange={(e) => set("bankPrefix", e.target.value)}
            />
          </Field>
          <Field label="Recall size" inline hint="How many remembered facts a recall returns. 0 ⇒ the daemon's default (8).">
            <Stepper value={draft.recallTopK} onChange={(v) => set("recallTopK", v)} min={0} max={200} />
          </Field>
          {draft.backend === "local" ? (
            <Collapsible label="Advanced" icon={Sliders}>
              <Field label="Bank directory" inline hint="Blank ⇒ `~/.rhapsody/teams/banks/`.">
                <TextInput
                  mono
                  value={draft.memoryPath}
                  aria-label="Bank directory"
                  placeholder="~/.rhapsody/teams/banks"
                  onChange={(e) => set("memoryPath", e.target.value)}
                />
              </Field>
            </Collapsible>
          ) : null}
        </SectionCard>
      ) : null}

      <SectionCard
        title="Roster"
        desc="A name becomes a `rhapsody:@<name>` Linear label and an `agent-<name>` memory bank, so it must match ^[a-z][a-z0-9-]*$. Labels are what the router matches against a ticket."
        action={
          <Button
            type="button"
            variant="secondary"
            size="sm"
            onClick={() => onChange({ ...draft, roster: [...draft.roster, emptyRow()] })}
          >
            Add teammate
          </Button>
        }
      >
        <div style={{ display: "flex", flexDirection: "column", gap: 10 }}>
          {draft.roster.map((r, i) => (
            <RosterRow
              key={i}
              index={i}
              row={r}
              onChange={(patch) => setRow(i, patch)}
              onRemove={() => onChange({ ...draft, roster: draft.roster.filter((_, n) => n !== i) })}
            />
          ))}
        </div>
        {errors.length > 0 ? (
          <div role="alert" style={{ marginTop: 10 }}>
            {errors.map((e) => (
              <Note key={e} tone="red">
                {e}
              </Note>
            ))}
          </div>
        ) : null}
      </SectionCard>

      <SectionCard
        title="What Save will configure"
        desc={`${path || "~/.rhapsody/teams.yaml"} — the daemon writes this as a full file, rebuilt from its own schema with every default made explicit. Comments, key order, and any key the daemon does not model are not preserved.`}
      >
        <pre
          className="mono"
          style={{
            margin: 0,
            fontSize: 11.5,
            lineHeight: 1.6,
            color: "var(--tx-3)",
            whiteSpace: "pre-wrap",
          }}
        >
          {teamsYamlSnippet(draft)}
        </pre>
      </SectionCard>

      {restartRequired ? (
        <Note>
          Teams config is read once at daemon start — there is no watcher on teams.yaml, unlike
          WORKFLOW.md. Restart the daemon for this to take effect.
        </Note>
      ) : null}
      {error ? <Note tone="red">{error}</Note> : null}
      <div style={{ display: "flex", gap: 8, justifyContent: "flex-end" }}>
        <Button type="button" variant="ghost" onClick={onCancel}>
          Cancel
        </Button>
        <Button type="button" variant="primary" disabled={errors.length > 0 || saving} onClick={onSave}>
          {saving ? "Saving…" : "Save teams.yaml"}
        </Button>
      </div>
    </div>
  );
}

// ApiKeyField — `memory.api_key` for the hindsight backend, and the one field in this editor that
// must never render what is already stored.
//
// A `$NAME` is a POINTER, not a secret: it is shown, edited and encouraged, because the daemon
// resolves it from its own environment and the credential then never sits in teams.yaml at all. A
// literal is accepted (the schema takes one) but is masked on read — `toDraft` refuses to load it
// into the draft, so "Replace" is the only way to change it and there is no state this component
// could accidentally render.
//
// Replace is DESTRUCTIVE and one click away, which is why it is reversible here. Pressing it clears
// the carry-forward flag, so saving from that state writes `api_key: ""` and de-authenticates the
// backend — hindsight answers every `/v1/**` with 401, and the daemon then warns and runs
// memoryless. An operator who pressed it to look, or by accident, must be able to get back without
// abandoning every other edit in the form, and must be told what a blank save does. `hasStoredKey`
// comes from the FETCHED config rather than the draft precisely so the way back outlives the click.
function ApiKeyField({
  draft,
  onChange,
  hasStoredKey,
}: {
  draft: TeamsDraft;
  onChange: (d: TeamsDraft) => void;
  hasStoredKey: boolean;
}) {
  const hint =
    "May name an environment variable — `$HINDSIGHT_API_KEY` is read from the daemon's environment, so the secret stays out of the file. A literal works too, but then it lives in teams.yaml.";
  if (draft.apiKeyStored) {
    return (
      <Field
        label="API key"
        inline
        hint={`A key is stored in teams.yaml and is not shown. ${hint}`}
        action={
          <Button
            type="button"
            variant="ghost"
            size="sm"
            onClick={() => onChange({ ...draft, apiKey: "", apiKeyStored: false })}
          >
            Replace
          </Button>
        }
      >
        <TextInput mono readOnly aria-label="API key" value={MASKED_API_KEY} />
      </Field>
    );
  }
  return (
    <Field
      label="API key"
      inline
      hint={
        hasStoredKey
          ? `Replacing the key stored in teams.yaml. Save with this blank and the stored key is removed, which leaves the backend unauthenticated. ${hint}`
          : hint
      }
      action={
        hasStoredKey ? (
          <Button
            type="button"
            variant="ghost"
            size="sm"
            onClick={() => onChange({ ...draft, apiKey: "", apiKeyStored: true })}
          >
            Keep existing
          </Button>
        ) : undefined
      }
    >
      <TextInput
        mono
        value={draft.apiKey}
        aria-label="API key"
        placeholder="$HINDSIGHT_API_KEY"
        onChange={(e) => onChange({ ...draft, apiKey: e.target.value })}
      />
    </Field>
  );
}

// RosterRow — the clean three-field row STUDIO-652 shipped, plus a per-row disclosure for the two
// overrides most teams never set. The disclosure is a bare button rather than the `Collapsible`
// primitive on purpose: `Collapsible` is a bordered card, and one per roster row would turn a
// compact list into a stack of boxes. It still carries `aria-expanded`, so the affordance is the
// same to assistive tech.
function RosterRow({
  index,
  row,
  onChange,
  onRemove,
}: {
  index: number;
  row: RosterDraft;
  onChange: (patch: Partial<RosterDraft>) => void;
  onRemove: () => void;
}) {
  const [open, setOpen] = React.useState(false);
  const n = index + 1;
  return (
    <div style={{ display: "flex", flexDirection: "column", gap: 8 }}>
      <div style={{ display: "flex", gap: 8, alignItems: "center", flexWrap: "wrap" }}>
        <TextInput
          value={row.name}
          aria-label={`Teammate ${n} name`}
          placeholder="alice"
          onChange={(e) => onChange({ name: e.target.value })}
          style={{ width: 150 }}
        />
        <TextInput
          value={row.profile}
          aria-label={`Teammate ${n} profile`}
          placeholder="swe"
          onChange={(e) => onChange({ profile: e.target.value })}
          style={{ width: 130 }}
        />
        <TextInput
          value={row.labels}
          aria-label={`Teammate ${n} labels`}
          placeholder="rust, config"
          onChange={(e) => onChange({ labels: e.target.value })}
          style={{ flex: 1, minWidth: 180 }}
        />
        <Button
          type="button"
          variant="ghost"
          size="sm"
          aria-label={`Teammate ${n} advanced`}
          aria-expanded={open}
          onClick={() => setOpen(!open)}
        >
          {open ? "Less" : "More"}
        </Button>
        <Button type="button" variant="ghost" size="sm" aria-label={`Remove teammate ${n}`} onClick={onRemove}>
          Remove
        </Button>
      </div>
      {open ? (
        <div style={{ display: "flex", gap: 8, alignItems: "center", flexWrap: "wrap", paddingLeft: 12 }}>
          <span style={{ fontSize: 12, color: "var(--tx-3)" }}>Bank</span>
          <TextInput
            mono
            value={row.bank}
            aria-label={`Teammate ${n} bank`}
            placeholder="agent-<name>"
            onChange={(e) => onChange({ bank: e.target.value })}
            style={{ width: 190 }}
          />
          <span style={{ fontSize: 12, color: "var(--tx-3)" }}>Max concurrent</span>
          <Stepper
            value={row.maxConcurrent}
            onChange={(v) => onChange({ maxConcurrent: v })}
            min={0}
            max={99}
            style={{ width: 130 }}
          />
          <span style={{ fontSize: 12, color: "var(--tx-faint)" }}>0 ⇒ unlimited</span>
        </div>
      ) : null}
    </div>
  );
}

function Row({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div style={{ display: "flex", alignItems: "center", gap: 14, padding: "7px 0" }}>
      <span style={{ width: 130, fontSize: 12.5, color: "var(--tx-3)" }}>{label}</span>
      {children}
    </div>
  );
}

function Status({ label, detail, tone }: { label: string; detail: string; tone?: "red" | "on" }) {
  const color = tone === "red" ? "var(--red)" : tone === "on" ? "var(--sage)" : "var(--tx)";
  return (
    <div style={{ display: "flex", flexDirection: "column", gap: 4, padding: "4px 0" }}>
      <span style={{ fontSize: 13, fontWeight: 600, color }}>{label}</span>
      <span style={{ fontSize: 12.5, color: "var(--tx-3)", lineHeight: 1.5 }}>{detail}</span>
    </div>
  );
}

function Path({ path }: { path: string }) {
  if (!path) return null;
  return (
    <div className="mono" style={{ fontSize: 11, color: "var(--tx-faint)", paddingTop: 6 }}>
      {path}
    </div>
  );
}

function Note({ children, tone }: { children: React.ReactNode; tone?: "red" }) {
  return (
    <div style={{ fontSize: 12.5, color: tone === "red" ? "var(--red)" : "var(--tx-3)", padding: "4px 0", lineHeight: 1.5 }}>
      {children}
    </div>
  );
}
