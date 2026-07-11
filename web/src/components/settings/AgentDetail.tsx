import {
  AlertTriangle,
  ArrowLeft,
  Boxes,
  Button,
  Chips,
  Code,
  Collapsible,
  Cpu,
  Field,
  Git,
  Linear,
  List,
  Pill,
  SectionCard,
  Select,
  Sliders,
  StatusChip,
  StatusDot,
  Stepper,
  Terminal,
  TextInput,
  Toggle,
  Trash,
} from "@/components/ui";
import type { LinearProject } from "@/lib/api";
import {
  CLAIM_MODE_HINT,
  CLAIM_MODES,
  DEPENDENCY_MODE_HINT,
  DEPENDENCY_MODES,
  EFFORT_LABEL,
  EFFORTS,
  GIT_WORKFLOWS,
  MODELS,
  PERMISSIONS,
  WORKSPACE_MODE_HINT,
  WORKSPACE_MODE_RECOMMEND_RATIONALE,
  WORKSPACE_MODES,
} from "@/lib/settings-data";
import {
  projectSelectOptions,
  reviewPromoteValid,
  type UiAgent,
  type UiGlobal,
  type UiOverrides,
} from "@/lib/settings-model";
import { OverrideField, type OverrideMode } from "./OverrideField";
import { PromptSource } from "./PromptSource";

// The inherited prompt template shown when an agent leaves its prompt blank (the proven workflow
// body it falls back to). Mirrors the desktop onboarding seed (`defaultPromptBody`), including the
// `HANDOFF: in-review` declaration the runner needs to record a clean run as completed (INF-272).
const INHERITED_PROMPT =
  'You are an autonomous engineer working ticket {{ issue.identifier }}: "{{ issue.title }}".\n\n' +
  "{{ issue.description }}\n\n" +
  "Implement the change test-first, keep the build and tests green, open a draft PR, move the issue to review when done, " +
  "and end your final message with a line: `HANDOFF: in-review` — Symphony records the run as completed only when this declaration is present. Do not merge.";

type OverrideKey = keyof UiOverrides;
// StringOverrideKey is the subset of override keys backed by a Select (string-valued); the
// generic `ov` helper renders only these, so its globalValue narrows to string. Boolean knobs
// (ultracode) render their own OverrideField with a Toggle.
type StringOverrideKey = Extract<
  { [K in OverrideKey]: NonNullable<UiOverrides[K]> extends string ? K : never }[OverrideKey],
  string
>;

export interface AgentDetailProps {
  agent: UiAgent;
  global: UiGlobal;
  linearProjects: LinearProject[];
  mode: OverrideMode;
  /** Apply an edit to this agent. The parent (Settings) is the single source of truth: it folds the
   *  change into the draft and re-derives `agent`, so this editor is fully controlled — no local
   *  copy to drift out of sync, and no stale-snapshot merges across rapid edits. */
  onChange: (agent: UiAgent) => void;
  onBack: () => void;
  onRemove: () => void;
}

// AgentDetail — the per-agent editor. It is CONTROLLED by the Settings draft (the `agent` prop is
// re-derived from the draft each render); every edit calls `onChange` with the next agent and the
// parent persists it via the Save bar (enable/pause included — it batches with the other detail
// edits, unlike the list row's standalone immediate toggle). The Claude overrides are the
// centerpiece: a sparse `overrides` map where Override seeds the global value and Reset deletes it.
export function AgentDetail({ agent: a, global, linearProjects, mode, onChange, onBack, onRemove }: AgentDetailProps) {
  const set = <K extends keyof UiAgent>(k: K, v: UiAgent[K]) => onChange({ ...a, [k]: v });
  // When the saved slug matches no Linear project (e.g. a pre-INF-277 free-text value), show the
  // raw slug + a "not found in Linear" hint instead of the bare placeholder.
  const { options: projectOptions, unmatched: projectUnmatched } = projectSelectOptions(linearProjects, a.projectSlug);
  const setOv = (k: OverrideKey, v: string) => onChange({ ...a, overrides: { ...a.overrides, [k]: v } });
  const setOvBool = (k: OverrideKey, v: boolean) => onChange({ ...a, overrides: { ...a.overrides, [k]: v } });
  const setOvNum = (k: OverrideKey, v: number) => onChange({ ...a, overrides: { ...a.overrides, [k]: v } });
  const clearOv = (k: OverrideKey) => {
    const overrides = { ...a.overrides };
    delete overrides[k];
    onChange({ ...a, overrides });
  };

  const promoteValid = reviewPromoteValid(a);
  const promoteOptions = [...new Set([...a.activeStates, a.reviewPromote])]
    .filter(Boolean)
    .map((s) => ({ value: s, label: s }));

  // ov renders one inherit/override row backed by the sparse `overrides` map.
  const ov = (key: StringOverrideKey, label: string, hint: string | undefined, options: typeof MODELS, fmt?: (v: string) => string) => {
    const globalValue = global[key];
    return (
      <OverrideField
        label={label}
        hint={hint}
        mode={mode}
        globalLabel={fmt ? fmt(globalValue) : globalValue}
        overridden={a.overrides[key] !== undefined}
        onOverride={() => setOv(key, globalValue)}
        onReset={() => clearOv(key)}
        control={<Select value={a.overrides[key] ?? globalValue} options={options} onChange={(v) => setOv(key, v)} />}
      />
    );
  };

  return (
    <div style={{ display: "flex", flexDirection: "column", gap: 18 }}>
      {/* header */}
      <div style={{ display: "flex", alignItems: "center", justifyContent: "space-between", gap: 16 }}>
        <div style={{ display: "flex", alignItems: "center", gap: 14, minWidth: 0 }}>
          <button
            type="button"
            onClick={onBack}
            style={{
              display: "inline-flex",
              alignItems: "center",
              gap: 6,
              background: "var(--bg-raised)",
              border: "1px solid var(--line)",
              borderRadius: "var(--r-ctrl)",
              color: "var(--tx-2)",
              height: 34,
              padding: "0 12px",
              cursor: "pointer",
              fontSize: 13,
            }}
          >
            <ArrowLeft size={15} />
            Agents
          </button>
          <div style={{ display: "flex", alignItems: "center", gap: 11, minWidth: 0 }}>
            <StatusDot color={a.color} size={11} pulse={a.status === "running"} />
            <input
              aria-label="Agent name"
              value={a.name}
              onChange={(e) => set("name", e.target.value)}
              style={{
                background: "transparent",
                border: "1px solid transparent",
                borderRadius: 8,
                color: "var(--tx)",
                fontSize: 19,
                fontWeight: 600,
                letterSpacing: "-0.02em",
                padding: "4px 8px",
                width: Math.max(120, a.name.length * 12 + 28),
                maxWidth: 320,
              }}
              onFocus={(e) => {
                e.target.style.background = "var(--bg-input)";
                e.target.style.borderColor = "var(--line)";
              }}
              onBlur={(e) => {
                e.target.style.background = "transparent";
                e.target.style.borderColor = "transparent";
              }}
            />
            {a.status === "running" ? <StatusChip status="running" count={a.running} /> : <StatusChip status={a.status} />}
          </div>
        </div>
        <div style={{ display: "flex", alignItems: "center", gap: 12 }}>
          <span style={{ fontSize: 12.5, color: "var(--tx-3)" }}>{a.enabled ? "Enabled" : "Disabled"}</span>
          <Toggle checked={a.enabled} onChange={(v) => set("enabled", v)} aria-label="Enable agent" />
        </div>
      </div>

      {/* Linear */}
      <SectionCard title="Linear" icon={Linear} desc="Which project this agent watches.">
        <Field label="Linear project" inline>
          <div style={{ display: "flex", flexDirection: "column", gap: 5, alignItems: "flex-end" }}>
            <Select
              value={a.projectSlug}
              options={projectOptions}
              invalid={projectUnmatched}
              onChange={(v) => set("projectSlug", v)}
            />
            {projectUnmatched ? (
              <span style={{ fontSize: 11.5, color: "var(--red)" }}>
                Not found in Linear — this slug matches no project, so its agent never dispatches.
              </span>
            ) : null}
          </div>
        </Field>
        <Field label="Milestone" inline optional hint="Restrict to issues in a single milestone.">
          <TextInput value={a.milestone} placeholder="Any milestone" onChange={(e) => set("milestone", e.target.value)} />
        </Field>
        <Field label="Required labels" optional hint="Only pick up issues carrying at least one of these labels. Empty inherits the global default.">
          <div style={{ display: "flex", flexDirection: "column", gap: 6 }}>
            <Chips
              items={a.labels}
              tone="amber"
              placeholder="Add label…"
              onAdd={(v) => set("labels", [...a.labels, v])}
              onRemove={(v) => set("labels", a.labels.filter((x) => x !== v))}
            />
            {a.labels.length === 0 ? (
              <span style={{ fontSize: 11.5, color: "var(--tx-3)" }}>
                {global.labels.length > 0
                  ? `Inheriting global default: ${global.labels.join(", ")}`
                  : "No label filter — picks up matching issues regardless of label."}
              </span>
            ) : null}
          </div>
        </Field>
      </SectionCard>

      {/* Repository */}
      <SectionCard title="Repository" icon={Git}>
        <Field label="Git URL" inline hint="Cloned fresh into the workspace root for each run.">
          <TextInput value={a.repo} mono prefixIcon={Git} onChange={(e) => set("repo", e.target.value)} />
        </Field>
      </SectionCard>

      {/* Issue states */}
      <SectionCard
        title="Issue states"
        icon={List}
        desc="Map your Linear workflow states. The promote state must be one of the active states."
      >
        <Field label="Active states" hint="Issues in these states are picked up and worked.">
          <Chips
            items={a.activeStates}
            tone="emerald"
            placeholder="Add state…"
            onAdd={(v) => set("activeStates", [...a.activeStates, v])}
            onRemove={(v) => set("activeStates", a.activeStates.filter((x) => x !== v))}
          />
        </Field>
        <Field label="Terminal states" hint="Work stops when an issue reaches one of these.">
          <Chips
            items={a.terminalStates}
            tone="neutral"
            placeholder="Add state…"
            onAdd={(v) => set("terminalStates", [...a.terminalStates, v])}
            onRemove={(v) => set("terminalStates", a.terminalStates.filter((x) => x !== v))}
          />
        </Field>
        <Field label="Review states" optional hint="Where finished work waits for a human.">
          <Chips
            items={a.reviewStates}
            tone="sky"
            placeholder="Add state…"
            onAdd={(v) => set("reviewStates", [...a.reviewStates, v])}
            onRemove={(v) => set("reviewStates", a.reviewStates.filter((x) => x !== v))}
          />
        </Field>
        <Field
          label="Review-promote state"
          inline
          hint="On re-open, issues return to this state."
          error={
            !promoteValid
              ? `“${a.reviewPromote}” must be one of the active states (${a.activeStates.join(", ")}).`
              : undefined
          }
        >
          <Select
            value={a.reviewPromote}
            invalid={!promoteValid}
            options={promoteOptions}
            onChange={(v) => set("reviewPromote", v)}
          />
        </Field>
      </SectionCard>

      {/* Concurrency */}
      <SectionCard title="Concurrency" icon={Sliders}>
        <Field
          label="Per-agent cap"
          inline
          hint={`Runs this agent may execute in parallel. Bounded by the global max (${global.maxConcurrent}).`}
        >
          <Stepper value={a.cap} onChange={(v) => set("cap", v)} min={1} max={global.maxConcurrent} />
        </Field>
      </SectionCard>

      {/* Claude overrides — the centerpiece */}
      <SectionCard
        title="Claude overrides"
        icon={Cpu}
        desc="Every field inherits your global default. Override only what this project needs — the rest stays in sync automatically."
      >
        {ov("model", "Model", undefined, MODELS)}
        {ov("effort", "Effort", "Reasoning budget per turn.", EFFORTS, (v) => EFFORT_LABEL[v] ?? v)}
        {ov("permission", "Permission mode", undefined, PERMISSIONS)}
        {ov("gitFlow", "Git workflow", "Graphite enforces gt via a worktree guard hook.", GIT_WORKFLOWS)}
        <OverrideField
          label="Ultracode"
          hint="Enable Claude Code's ultracode setting."
          mode={mode}
          globalLabel={global.ultracode ? "On" : "Off"}
          overridden={a.overrides.ultracode !== undefined}
          onOverride={() => setOvBool("ultracode", global.ultracode)}
          onReset={() => clearOv("ultracode")}
          control={
            <Toggle
              checked={a.overrides.ultracode ?? global.ultracode}
              onChange={(v) => setOvBool("ultracode", v)}
              aria-label="Ultracode"
            />
          }
        />
        <OverrideField
          label="Turn timeout"
          hint="Hard wall-clock cap on a single agent turn before it's killed and retried."
          mode={mode}
          globalLabel={`${global.requestTimeoutMin} min`}
          overridden={a.overrides.turnTimeoutMin !== undefined}
          onOverride={() => setOvNum("turnTimeoutMin", global.requestTimeoutMin)}
          onReset={() => clearOv("turnTimeoutMin")}
          control={
            <Stepper
              value={a.overrides.turnTimeoutMin ?? global.requestTimeoutMin}
              onChange={(v) => setOvNum("turnTimeoutMin", v)}
              min={1}
              max={360}
              suffix="min"
            />
          }
        />
        <OverrideField
          label="Stall timeout"
          hint="Idle-output cap before a stalled turn is killed and retried."
          mode={mode}
          globalLabel={`${global.stallTimeoutMin} min`}
          overridden={a.overrides.stallTimeoutMin !== undefined}
          onOverride={() => setOvNum("stallTimeoutMin", global.stallTimeoutMin)}
          onReset={() => clearOv("stallTimeoutMin")}
          control={
            <Stepper
              value={a.overrides.stallTimeoutMin ?? global.stallTimeoutMin}
              onChange={(v) => setOvNum("stallTimeoutMin", v)}
              min={0}
              max={120}
              suffix="min"
            />
          }
        />
        <OverrideField
          label="Billing guard"
          hint="Forces the agent to bill your logged-in Claude subscription and aborts the run if it detects metered-API billing. Turn OFF only to deliberately allow metered Anthropic-API usage for this project."
          mode={mode}
          globalLabel={global.billingGuard ? "On" : "Off"}
          overridden={a.overrides.billingGuard !== undefined}
          onOverride={() => setOvBool("billingGuard", global.billingGuard)}
          onReset={() => clearOv("billingGuard")}
          control={
            <Toggle
              checked={a.overrides.billingGuard ?? global.billingGuard}
              onChange={(v) => setOvBool("billingGuard", v)}
              aria-label="Billing guard"
            />
          }
        />
        <OverrideField
          label="Command"
          hint="The CLI binary this agent launches."
          mode={mode}
          globalLabel={global.command}
          overridden={a.overrides.command !== undefined}
          onOverride={() => setOv("command", global.command)}
          onReset={() => clearOv("command")}
          control={
            <TextInput
              mono
              value={a.overrides.command ?? global.command}
              placeholder="claude"
              onChange={(e) => setOv("command", e.target.value)}
            />
          }
        />
      </SectionCard>

      {/* Dependencies — how this project's dependent tickets are sequenced (INF-318/INF-320). A
          per-agent override of the global dependency_mode, mirroring the git_flow inherit/override
          plumbing; the help hint documents all three modes + thresholds. */}
      <SectionCard
        title="Dependencies"
        icon={Boxes}
        desc="How the daemon sequences dependent tickets from Linear blockedBy edges. Inherits the global default."
      >
        {ov("dependencyMode", "Dependency mode", DEPENDENCY_MODE_HINT, DEPENDENCY_MODES)}
        {ov("workspaceMode", "Workspace mode", WORKSPACE_MODE_HINT, WORKSPACE_MODES)}
        {a.workspaceModeRecommended && a.overrides.workspaceMode === undefined && (
          <div
            style={{
              border: "1px solid rgba(16,185,129,.28)",
              borderRadius: "var(--r-card)",
              background: "rgba(16,185,129,.05)",
              padding: 14,
              display: "flex",
              alignItems: "center",
              justifyContent: "space-between",
              gap: 14,
            }}
          >
            <div>
              <Pill tone="emerald">Recommended: Clone</Pill>
              <div style={{ fontSize: 12.5, color: "var(--tx-3)", marginTop: 6, maxWidth: 520, lineHeight: 1.5 }}>
                {WORKSPACE_MODE_RECOMMEND_RATIONALE}
              </div>
            </div>
            <Button variant="secondary" onClick={() => setOv("workspaceMode", "clone")}>
              Use clone
            </Button>
          </div>
        )}
      </SectionCard>

      {/* Claiming — how this project acquires tickets (INF-477). A per-agent override of the global
          claim_mode, mirroring the dependency_mode inherit/override plumbing. */}
      <SectionCard
        title="Claiming"
        icon={Linear}
        desc="How the daemon acquires this project's tickets. Inherits the global default."
      >
        {ov("claimMode", "Claim mode", CLAIM_MODE_HINT, CLAIM_MODES)}
      </SectionCard>

      {/* Prompt */}
      <SectionCard
        title="Prompt"
        icon={Terminal}
        desc="Liquid-templated body the agent receives, inline or from a file. Inherits the global default when left blank."
      >
        <Collapsible
          label="Prompt source"
          icon={Code}
          defaultOpen={!!a.prompt || !!a.promptFile}
          badge={a.prompt || a.promptFile ? <Pill tone="emerald">Custom</Pill> : <Pill tone="neutral">Inherited</Pill>}
        >
          {/* The inherited template is a PLACEHOLDER, not the value: an empty prompt/path stays empty
              (inherits the global) until the user actually types, so a stray keystroke can't silently
              persist the whole default template — or a stray path — as a custom override. */}
          <PromptSource
            prompt={a.prompt}
            onPromptChange={(v) => set("prompt", v)}
            promptFile={a.promptFile}
            onPromptFileChange={(v) => set("promptFile", v)}
            promptPlaceholder={INHERITED_PROMPT}
            // When this agent doesn't override the path, a global prompt_file is inherited and wins
            // at run time — surface it so the editor doesn't imply inline edits take effect.
            inheritedFile={global.promptFile}
          />
        </Collapsible>
      </SectionCard>

      {/* Danger zone */}
      <div
        style={{
          border: "1px solid rgba(239,83,80,.28)",
          borderRadius: "var(--r-card)",
          background: "rgba(239,83,80,.04)",
          padding: 20,
          display: "flex",
          alignItems: "center",
          justifyContent: "space-between",
          gap: 16,
        }}
      >
        <div>
          <div style={{ fontSize: 13.5, fontWeight: 600, color: "var(--tx)", display: "flex", alignItems: "center", gap: 8 }}>
            <AlertTriangle size={15} style={{ color: "var(--red)" }} />
            Remove agent
          </div>
          <div style={{ fontSize: 12.5, color: "var(--tx-3)", marginTop: 4, maxWidth: 520, lineHeight: 1.5 }}>
            Stops watching this project and deletes its configuration. Run history is retained. This cannot be undone.
          </div>
        </div>
        <Button variant="danger" icon={Trash} onClick={onRemove}>
          Remove agent
        </Button>
      </div>
    </div>
  );
}
