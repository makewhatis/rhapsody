import * as React from "react";
import { Button, SectionCard, Select, TextInput, Toggle } from "@/components/ui";
import { useSaveTeamsConfig, useTeamsConfigQuery } from "@/hooks/useTeams";
import {
  draftErrors,
  MANAGER_MODES,
  MEMORY_BACKENDS,
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
  // A daemon with no on-disk runtime home has nowhere to keep a teams.yaml. Say so, and offer the
  // file's text anyway — the operator can still write it by hand once the daemon has a store.
  if (cfg.isError) {
    return (
      <SectionCard title="Teams" desc="Named teammates with their own profiles, memory and a shared room.">
        <Note tone="red">Could not read teams.yaml: {String(cfg.error)}</Note>
      </SectionCard>
    );
  }

  if (editing && draft) {
    return (
      <TeamsEditor
        draft={draft}
        onChange={setDraft}
        path={view?.path ?? ""}
        restartRequired={view?.restart_required ?? true}
        saving={save.isPending}
        error={save.isError ? String(save.error) : ""}
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

// TeamsEditor — the minimal editor: the toggle, the roster rows, the manager mode and the memory
// backend. Everything else in teams.yaml (manager.model, the timeouts, bank_prefix, recall_top_k,
// the prompt budget) is left to the daemon's schema defaults and preserved verbatim when editing an
// existing file, so this form can never silently drop a hand-tuned key it does not show.
function TeamsEditor({
  draft,
  onChange,
  path,
  restartRequired,
  saving,
  error,
  onCancel,
  onSave,
}: {
  draft: TeamsDraft;
  onChange: (d: TeamsDraft) => void;
  path: string;
  restartRequired: boolean;
  saving: boolean;
  error: string;
  onCancel: () => void;
  onSave: () => void;
}) {
  const errors = draftErrors(draft);
  const setRow = (i: number, row: Partial<RosterDraft>) =>
    onChange({ ...draft, roster: draft.roster.map((r, n) => (n === i ? { ...r, ...row } : r)) });

  return (
    <div style={{ display: "flex", flexDirection: "column", gap: 18 }}>
      <SectionCard
        title="Edit teams.yaml"
        desc="Nothing is written until you press Save. A rejected config is never written at all — the daemon validates first and leaves the file exactly as it was."
      >
        <Row label="Enable Teams">
          <Toggle checked={draft.enabled} onChange={(enabled) => onChange({ ...draft, enabled })} />
        </Row>
        <Row label="Assignment">
          <Select
            value={draft.managerMode}
            width={220}
            options={MANAGER_MODES.map((m) => ({ value: m, label: m }))}
            onChange={(managerMode) => onChange({ ...draft, managerMode })}
          />
        </Row>
        <Row label="Memory">
          <Select
            value={draft.backend}
            width={220}
            options={MEMORY_BACKENDS.map((m) => ({ value: m, label: m }))}
            onChange={(backend) => onChange({ ...draft, backend })}
          />
        </Row>
      </SectionCard>

      <SectionCard
        title="Roster"
        desc="A name becomes a `rhapsody:@<name>` Linear label and an `agent-<name>` memory bank, so it must match ^[a-z][a-z0-9-]*$. Labels are what the router matches against a ticket."
        action={
          <Button
            type="button"
            variant="secondary"
            size="sm"
            onClick={() => onChange({ ...draft, roster: [...draft.roster, { name: "", profile: "swe", labels: "" }] })}
          >
            Add teammate
          </Button>
        }
      >
        <div style={{ display: "flex", flexDirection: "column", gap: 10 }}>
          {draft.roster.map((r, i) => (
            <div key={i} style={{ display: "flex", gap: 8, alignItems: "center", flexWrap: "wrap" }}>
              <TextInput
                value={r.name}
                aria-label={`Teammate ${i + 1} name`}
                placeholder="alice"
                onChange={(e) => setRow(i, { name: e.target.value })}
                style={{ width: 150 }}
              />
              <TextInput
                value={r.profile}
                aria-label={`Teammate ${i + 1} profile`}
                placeholder="swe"
                onChange={(e) => setRow(i, { profile: e.target.value })}
                style={{ width: 130 }}
              />
              <TextInput
                value={r.labels}
                aria-label={`Teammate ${i + 1} labels`}
                placeholder="rust, config"
                onChange={(e) => setRow(i, { labels: e.target.value })}
                style={{ flex: 1, minWidth: 180 }}
              />
              <Button
                type="button"
                variant="ghost"
                size="sm"
                aria-label={`Remove teammate ${i + 1}`}
                onClick={() => onChange({ ...draft, roster: draft.roster.filter((_, n) => n !== i) })}
              >
                Remove
              </Button>
            </div>
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

      <SectionCard title="What Save will write" desc={path ? path : "~/.rhapsody/teams.yaml"}>
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
