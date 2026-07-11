import * as React from "react";
import type { LinearProject } from "@/lib/api";
import type { UiAgent, UiGlobal } from "@/lib/settings-model";
import { AgentList, EmptyState } from "./AgentList";
import { AgentDetail } from "./AgentDetail";
import type { OverrideMode } from "./OverrideField";

export interface ProjectsTabProps {
  agents: UiAgent[];
  global: UiGlobal;
  linearProjects: LinearProject[];
  mode: OverrideMode;
  listStyle?: "rows" | "cards";
  /** Persist an enable/pause toggle for the agent at `index` (immediate config write). */
  onToggle: (index: number, enabled: boolean) => void;
  /** Sync a detail edit for the agent at `index` into the draft (Save bar persists). */
  onAgentChange: (index: number, agent: UiAgent) => void;
  /** Remove the agent at `index` (immediate config write). */
  onRemove: (index: number) => void;
  openSheet: () => void;
}

// ProjectsTab — switches between the agent list/empty-state and the per-agent detail editor.
// Selection is tracked by array INDEX so editing an agent's Linear project (which changes its
// slug-derived id) does not break the open editor.
export function ProjectsTab({
  agents,
  global,
  linearProjects,
  mode,
  listStyle,
  onToggle,
  onAgentChange,
  onRemove,
  openSheet,
}: ProjectsTabProps) {
  const [selected, setSelected] = React.useState<number | null>(null);
  const selectedAgent = selected != null ? agents[selected] : undefined;

  // If the selected index falls out of range (e.g. a server resync drops the agent list), clear it
  // so a later create doesn't silently reopen the detail editor for a stale index 0.
  React.useEffect(() => {
    if (selected != null && selected >= agents.length) setSelected(null);
  }, [selected, agents.length]);

  if (selected != null && selectedAgent) {
    return (
      <AgentDetail
        key={selected}
        agent={selectedAgent}
        global={global}
        linearProjects={linearProjects}
        mode={mode}
        onChange={(ui) => onAgentChange(selected, ui)}
        onBack={() => setSelected(null)}
        onRemove={() => {
          onRemove(selected);
          setSelected(null);
        }}
      />
    );
  }

  if (agents.length === 0) return <EmptyState openSheet={openSheet} />;

  return (
    <AgentList agents={agents} global={global} listStyle={listStyle} onSelect={setSelected} onToggle={onToggle} openSheet={openSheet} />
  );
}
