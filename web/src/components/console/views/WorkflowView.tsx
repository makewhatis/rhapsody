import * as React from "react";
import { Note, Seg } from "@/components/console";
import { AddAgentSheet } from "@/components/settings/AddAgentSheet";
import { GeneralTab } from "@/components/settings/GeneralTab";
import { ProjectsTab } from "@/components/settings/ProjectsTab";
import { useConfigDraft } from "@/hooks/useConfigDraft";
import { autosaveView } from "@/lib/settings-model";
import type { LinearProject } from "@/lib/api";
import "@/theme/console-workflow.css";

// Workflow — the console's WORKFLOW.md editor (STUDIO-681 §8, built by STUDIO-690). It is what the
// Settings "Workflow" row opens, and it is the last Settings-parity gate before the §2.2.1 flip:
// the console must not cost the operator the config editor the Podium Settings already ships.
//
// So it does not rebuild that editor. `GeneralTab` and `ProjectsTab` (INF-224/INF-226) are the
// shipped WORKFLOW.md editor — the global defaults, the per-agent projects, the prompt source, the
// Linear connection — and they are embedded here as they are, over the same draft model
// (`hooks/useConfigDraft`) and therefore the same `GET`/`POST /api/v1/config` data path. Parity is
// then a property of the code rather than a claim: there is one editor, rendered in two shells.
// What this view contributes is the console chrome of §8 — the breadcrumb back to Settings, the
// section switcher, and the save state — from §1 components.

const SECTIONS = [
  { value: "general", label: "General" },
  { value: "projects", label: "Projects" },
] as const;

type Section = (typeof SECTIONS)[number]["value"];

export interface WorkflowViewProps {
  /** Route away — the breadcrumb returns to the Settings hub the "Workflow" row lives on (§8). */
  onNavigate: (route: "settings") => void;
}

export function WorkflowView({ onNavigate }: WorkflowViewProps) {
  const cfg = useConfigDraft();
  const [section, setSection] = React.useState<Section>("general");
  const [sheetOpen, setSheetOpen] = React.useState(false);

  // A daemon that cannot serve its own config gets said out loud, with BOTH causes named: the
  // query failed (the daemon is unreachable), or it answered without a typed view (WORKFLOW.md is
  // on disk but does not parse). An empty form would invite edits that have nothing to save onto.
  if (cfg.unavailable) {
    return (
      <Page onNavigate={onNavigate}>
        <div role="alert">
          <Note variant="warn">
            Could not read <code>WORKFLOW.md</code> — the daemon did not serve a config. It may be
            down, or the file on disk may not parse.
          </Note>
        </div>
      </Page>
    );
  }
  if (!cfg.draft || !cfg.uiGlobal) {
    return (
      <Page onNavigate={onNavigate}>
        <div className="empty">Reading WORKFLOW.md…</div>
      </Page>
    );
  }

  const uiGlobal = cfg.uiGlobal;
  const onCreate = (project: LinearProject, repo: string) => {
    cfg.onCreateAgent(project, repo);
    setSheetOpen(false);
  };

  return (
    <Page onNavigate={onNavigate}>
      <div className="wfbar">
        <Seg
          options={SECTIONS.map((s) => ({ value: s.value, label: s.label }))}
          value={section}
          onChange={(v) => setSection(v as Section)}
          aria-label="Workflow section"
        />
        <SaveState dirty={cfg.dirty} saving={cfg.saving} blocked={cfg.blocked} error={cfg.error} />
      </div>

      <div className="wfembed">
        {section === "general" ? (
          <GeneralTab
            value={uiGlobal}
            onChange={cfg.onGlobalChange}
            account={cfg.account}
            token={cfg.token}
            onTokenChange={cfg.onTokenChange}
            onDisconnect={cfg.onDisconnect}
          />
        ) : (
          <ProjectsTab
            agents={cfg.agents}
            global={uiGlobal}
            linearProjects={cfg.linearProjects}
            mode="quiet"
            listStyle="rows"
            onToggle={cfg.onToggleAgent}
            onAgentChange={cfg.onAgentChange}
            onRemove={cfg.onRemoveAgent}
            openSheet={() => setSheetOpen(true)}
          />
        )}
      </div>

      {/* What a save actually does, stated in terms the daemon guarantees. It is deliberately NOT
          the "restart to apply" of the teams form (§7): teams.yaml is boot-loaded, whereas
          WORKFLOW.md is watched and re-read on change (crates/orchestrator/src/reload.rs), so a
          restart claim here would be false. Nor is it a live-apply promise about work already in
          flight — only the file write and the daemon's re-read are contractual. */}
      <Note variant="info">
        Edits are saved to <code>WORKFLOW.md</code> on their own — there is no Save button. The
        daemon watches that file and re-reads it when it changes; a change it would reject is
        refused with the reason and the file on disk is left untouched.
      </Note>

      <AddAgentSheet
        open={sheetOpen}
        onClose={() => setSheetOpen(false)}
        onCreate={onCreate}
        projects={cfg.linearProjects}
        usedSlugs={cfg.draft.projects.flatMap((p) => p.slugs)}
        blockedReason={cfg.blocked}
        global={uiGlobal}
      />
    </Page>
  );
}

function Page({
  onNavigate,
  children,
}: {
  onNavigate: (route: "settings") => void;
  children: React.ReactNode;
}) {
  return (
    <section>
      <div className="crumbs">
        {/* A button, not a link: it performs an action (routing), not a document jump. */}
        <button type="button" className="link" onClick={() => onNavigate("settings")}>
          Settings
        </button>{" "}
        · Workflow
      </div>
      <div className="head">
        <h1>Workflow</h1>
      </div>
      <p className="lead">
        Everything <code>WORKFLOW.md</code> holds, as a form — the defaults every agent inherits,
        and the projects they watch.
      </p>
      {children}
    </section>
  );
}

/**
 * The save state, derived by the same `autosaveView` the Podium header uses: a validation block
 * or a persist failure (the daemon's own message, verbatim), the pending/in-flight "Saving…", or
 * the settled "All changes saved".
 */
function SaveState(input: {
  dirty: boolean;
  saving: boolean;
  blocked: string | null;
  error: string | null;
}) {
  const view = autosaveView(input);
  if (view.kind === "error") {
    return (
      <span className="save err" role="status">
        {view.message}
      </span>
    );
  }
  return (
    <span className={view.kind === "saving" ? "save" : "save ok"} role="status">
      {view.kind === "saving" ? "Saving…" : "All changes saved"}
    </span>
  );
}
