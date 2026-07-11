import {
  Button,
  Collapsible,
  Divider,
  Field,
  SectionCard,
  Select,
  Stepper,
  TextInput,
  Toggle,
  Activity,
  CheckCircle,
  Cpu,
  Folder,
  HardDrive,
  Key,
  Linear,
  Link,
  Settings,
  Sliders,
  Terminal,
} from "@/components/ui";
import type { LinearIdentity } from "@/lib/api";
import { pickDirectory } from "@/lib/bindings";
import {
  BACKOFFS,
  CLAIM_MODE_HINT,
  CLAIM_MODES,
  DEPENDENCY_MODE_HINT,
  DEPENDENCY_MODES,
  EFFORTS,
  GIT_WORKFLOWS,
  MODELS,
  PERMISSIONS,
  WORKSPACE_MODE_HINT,
  WORKSPACE_MODES,
} from "@/lib/settings-data";
import type { UiGlobal } from "@/lib/settings-model";
import { PromptSource } from "./PromptSource";

export interface GeneralTabProps {
  value: UiGlobal;
  /** Apply a global-defaults edit (the parent marks the form dirty). */
  onChange: (next: UiGlobal) => void;
  /** The connected-as Linear account (null while loading / unauthenticated). */
  account: LinearIdentity | null;
  /** The pending API token. Kept out of `value`/config — it is written to the keychain on save. */
  token: string;
  onTokenChange: (token: string) => void;
  onDisconnect: () => void;
}

// initials derives a 2-letter avatar label from a display name ("David Johansen" -> "DJ").
function initials(name: string): string {
  const parts = name.trim().split(/\s+/).filter(Boolean);
  if (parts.length === 0) return "—";
  if (parts.length === 1) return parts[0].slice(0, 2).toUpperCase();
  return (parts[0][0] + parts[parts.length - 1][0]).toUpperCase();
}

// ConnectedRow — the connected-as identity chip (emerald-tinted) with avatar initials, name +
// check, masked email, and a Disconnect ghost button.
function ConnectedRow({ account, onDisconnect }: { account: LinearIdentity | null; onDisconnect: () => void }) {
  const connected = !!account?.connected;
  const name = account?.name || account?.display_name || "Not connected";
  return (
    <div
      style={{
        display: "flex",
        alignItems: "center",
        justifyContent: "space-between",
        gap: 14,
        padding: "13px 15px",
        borderRadius: "var(--r-ctrl)",
        background: connected ? "var(--em-soft)" : "rgba(255,255,255,.03)",
        border: connected ? "1px solid rgba(16,185,129,.22)" : "1px solid var(--line)",
      }}
    >
      <div style={{ display: "flex", alignItems: "center", gap: 12 }}>
        <div
          style={{
            width: 32,
            height: 32,
            borderRadius: "50%",
            background: connected ? "var(--em-bright)" : "var(--bg-raised)",
            color: connected ? "var(--on-em)" : "var(--tx-3)",
            display: "grid",
            placeItems: "center",
            fontSize: 13,
            fontWeight: 700,
          }}
        >
          {connected ? initials(name) : "—"}
        </div>
        <div>
          <div style={{ fontSize: 13.5, fontWeight: 600, display: "flex", alignItems: "center", gap: 8 }}>
            {connected ? `Connected as ${name}` : "Not connected"}
            {connected ? <CheckCircle size={14} style={{ color: "var(--em-bright)" }} /> : null}
          </div>
          <div className="mono" style={{ fontSize: 12, color: "var(--tx-2)", marginTop: 1 }}>
            {account?.email || "Paste a personal API token below to connect."}
          </div>
        </div>
      </div>
      {connected ? (
        <Button variant="ghost" size="sm" onClick={onDisconnect}>
          Disconnect
        </Button>
      ) : null}
    </div>
  );
}

// GeneralTab — global defaults every agent inherits. Ported from the Claude Design `general.jsx`
// onto the foundation primitives + tokens. Each edit flows through `onChange` so the shell's Save
// bar reflects the dirty state; the API token is the lone exception — it routes to `onTokenChange`
// and is written to the macOS keychain on save, never serialized into config.
export function GeneralTab({ value, onChange, account, token, onTokenChange, onDisconnect }: GeneralTabProps) {
  const set = <K extends keyof UiGlobal>(k: K, v: UiGlobal[K]) => onChange({ ...value, [k]: v });

  const pickInto = async (k: "workspaceRoot" | "logsPath", title: string) => {
    const path = await pickDirectory(title);
    if (path) set(k, path);
  };

  return (
    <div style={{ display: "flex", flexDirection: "column", gap: 18 }}>
      <SectionCard
        title="Linear connection"
        icon={Linear}
        desc="One Linear account drives every agent. Agents each watch a project inside it."
        action={
          <Button variant="subtle" size="sm" icon={Link} disabled comingSoon>
            Connect Linear
          </Button>
        }
      >
        <ConnectedRow account={account} onDisconnect={onDisconnect} />
        <Field
          label="API token"
          inline
          hint="Personal API key. Paste a new value to replace; stored in the macOS keychain."
        >
          <TextInput
            value={token}
            mono
            prefixIcon={Key}
            onChange={(e) => onTokenChange(e.target.value)}
            placeholder="Paste lin_api_…"
          />
        </Field>
        <Divider />
        <Field
          label="GitHub summons"
          inline
          hint="Re-engage an In-Review ticket when an @symphony comment lands on its unmerged linked GitHub PR."
        >
          <div style={{ display: "flex", justifyContent: "flex-end" }}>
            <Toggle
              checked={value.githubSummons}
              onChange={(v) => set("githubSummons", v)}
              aria-label="GitHub summons"
            />
          </div>
        </Field>
      </SectionCard>

      <SectionCard
        title="Agent limits"
        icon={Sliders}
        desc="Defaults that bound every agent unless a project overrides them."
      >
        <Field
          label="Max concurrent agents"
          inline
          hint="Total runs Symphony will execute in parallel across all projects."
        >
          <Stepper value={value.maxConcurrent} onChange={(v) => set("maxConcurrent", v)} min={1} max={12} />
        </Field>
        <Field label="Max turns per run" inline hint="Hard ceiling before a run is handed off for review.">
          <Stepper value={value.maxTurns} onChange={(v) => set("maxTurns", v)} min={1} max={500} />
        </Field>
        <Field
          label="Retry backoff"
          inline
          hint="How re-queued runs wait after a rate limit or transient error."
        >
          <Select value={value.backoff} options={BACKOFFS} onChange={(v) => set("backoff", v)} />
        </Field>
      </SectionCard>

      <SectionCard
        title="Claude defaults"
        icon={Cpu}
        desc="The model configuration agents inherit. Override per project on the Projects tab."
      >
        <Field label="Model" inline>
          <Select value={value.model} options={MODELS} onChange={(v) => set("model", v)} />
        </Field>
        <Field label="Effort" inline hint="Reasoning budget per turn.">
          <Select value={value.effort} options={EFFORTS} onChange={(v) => set("effort", v)} />
        </Field>
        <Field label="Permission mode" inline>
          <Select value={value.permission} options={PERMISSIONS} onChange={(v) => set("permission", v)} />
        </Field>
        <Field label="Git workflow" inline hint="Graphite enforces gt via a guard hook in each agent's worktree.">
          <Select value={value.gitFlow} options={GIT_WORKFLOWS} onChange={(v) => set("gitFlow", v)} />
        </Field>
        <Field label="Workspace mode" inline hint={WORKSPACE_MODE_HINT}>
          <Select value={value.workspaceMode} options={WORKSPACE_MODES} onChange={(v) => set("workspaceMode", v)} />
        </Field>
        <Field label="Dependency mode" inline hint={DEPENDENCY_MODE_HINT}>
          <Select value={value.dependencyMode} options={DEPENDENCY_MODES} onChange={(v) => set("dependencyMode", v)} />
        </Field>
        <Field label="Claim mode" inline hint={CLAIM_MODE_HINT}>
          <Select value={value.claimMode} options={CLAIM_MODES} onChange={(v) => set("claimMode", v)} />
        </Field>
        <Divider />
        <Field
          label="Billing guard"
          inline
          hint="Refuse to start unless a Claude subscription is active — uses your subscription, not a metered key."
        >
          <div style={{ display: "flex", justifyContent: "flex-end" }}>
            <Toggle checked={value.billingGuard} onChange={(v) => set("billingGuard", v)} aria-label="Billing guard" />
          </div>
        </Field>
        <Field
          label="Ultracode"
          inline
          hint="Enable Claude Code's ultracode setting for every agent (passed as --settings)."
        >
          <div style={{ display: "flex", justifyContent: "flex-end" }}>
            <Toggle checked={value.ultracode} onChange={(v) => set("ultracode", v)} aria-label="Ultracode" />
          </div>
        </Field>
        <Collapsible label="Advanced" icon={Terminal}>
          <Field
            label="Turn timeout"
            inline
            hint="Hard wall-clock cap on a single agent turn before it's killed and retried."
          >
            <Stepper
              value={value.requestTimeoutMin}
              onChange={(v) => set("requestTimeoutMin", v)}
              min={1}
              max={360}
              suffix="min"
            />
          </Field>
          <Field label="Extra CLI args" inline hint="Appended verbatim to every claude invocation.">
            <TextInput
              mono
              value={value.extraArgs}
              placeholder="--mcp-config ./mcp.json"
              onChange={(e) => set("extraArgs", e.target.value)}
            />
          </Field>
        </Collapsible>
      </SectionCard>

      <SectionCard
        title="Prompt"
        icon={Terminal}
        desc="The Liquid-templated body every agent inherits. Provide it inline, or read it from a file per run."
      >
        <PromptSource
          prompt={value.prompt}
          onPromptChange={(v) => set("prompt", v)}
          promptFile={value.promptFile}
          onPromptFileChange={(v) => set("promptFile", v)}
        />
      </SectionCard>

      <SectionCard title="Workspace & storage" icon={HardDrive}>
        <Field label="Workspace root" inline hint="Each run gets an isolated checkout under this directory.">
          <div style={{ display: "flex", gap: 8 }}>
            <TextInput
              mono
              value={value.workspaceRoot}
              onChange={(e) => set("workspaceRoot", e.target.value)}
              style={{ flex: 1 }}
            />
            <Button
              variant="subtle"
              size="md"
              icon={Folder}
              aria-label="Choose workspace folder"
              onClick={() => void pickInto("workspaceRoot", "Choose workspace root")}
              style={{ paddingLeft: 12, paddingRight: 12 }}
            />
          </div>
        </Field>
        <Field
          label="History retention"
          inline
          hint="Older run records and event logs are pruned after this many days."
        >
          <Stepper
            value={value.historyRetentionDays}
            onChange={(v) => set("historyRetentionDays", v)}
            min={1}
            max={365}
            suffix="days"
          />
        </Field>
        <Field
          label="Persist run artifacts"
          inline
          hint="Keep diffs, transcripts and tool output on disk for completed runs."
        >
          <div style={{ display: "flex", justifyContent: "flex-end" }}>
            <Toggle
              checked={value.persistArtifacts}
              onChange={(v) => set("persistArtifacts", v)}
              aria-label="Persist run artifacts"
            />
          </div>
        </Field>
      </SectionCard>

      <SectionCard title="App" icon={Settings}>
        <Field label="Dashboard port" inline hint="Local port the Symphony dashboard binds to.">
          <Stepper value={value.dashboardPort} onChange={(v) => set("dashboardPort", v)} min={1024} max={65535} />
        </Field>
        <Field label="Poll interval" inline hint="How often Symphony checks Linear for new tickets.">
          <Stepper value={value.pollIntervalSec} onChange={(v) => set("pollIntervalSec", v)} min={1} max={120} suffix="sec" />
        </Field>
        <Field label="Logs path" inline>
          <div style={{ display: "flex", gap: 8 }}>
            <TextInput
              mono
              value={value.logsPath}
              onChange={(e) => set("logsPath", e.target.value)}
              style={{ flex: 1 }}
            />
            <Button
              variant="subtle"
              size="md"
              icon={Folder}
              aria-label="Choose logs folder"
              onClick={() => void pickInto("logsPath", "Choose logs directory")}
              style={{ paddingLeft: 12, paddingRight: 12 }}
            />
          </div>
        </Field>
      </SectionCard>

      <SectionCard
        title="Agent MCP"
        icon={Link}
        desc="Expose Symphony's run/daemon state to agents over a local MCP facade (`symphony mcp`). Read tools are always on; these toggles gate injection into dispatched workers and the opt-in write tools."
      >
        <Field
          label="Inject into agents"
          inline
          hint="Add the `symphony` MCP server to every dispatched agent's config so it can query run status instead of reading logs. On by default; turn off to opt out (read tools still work for an operator's own session)."
        >
          <div style={{ display: "flex", justifyContent: "flex-end" }}>
            <Toggle checked={value.mcpEnabled} onChange={(v) => set("mcpEnabled", v)} aria-label="Agent MCP inject into agents" />
          </div>
        </Field>
        <Field
          label="Allow send message"
          inline
          hint="Register the symphony_send_message write tool (deliver a mid-run message to a live run). On by default."
        >
          <div style={{ display: "flex", justifyContent: "flex-end" }}>
            <Toggle checked={value.mcpAllowSendMessage} onChange={(v) => set("mcpAllowSendMessage", v)} aria-label="Agent MCP allow send message" />
          </div>
        </Field>
        <Field
          label="Allow stop"
          inline
          hint="Register the symphony_stop write tool (kill a running agent). Off by default — opt-in."
        >
          <div style={{ display: "flex", justifyContent: "flex-end" }}>
            <Toggle checked={value.mcpAllowStop} onChange={(v) => set("mcpAllowStop", v)} aria-label="Agent MCP allow stop" />
          </div>
        </Field>
        <Field
          label="Allow resume"
          inline
          hint="Register the symphony_resume write tool (resume a canceled run). Off by default — opt-in."
        >
          <div style={{ display: "flex", justifyContent: "flex-end" }}>
            <Toggle checked={value.mcpAllowResume} onChange={(v) => set("mcpAllowResume", v)} aria-label="Agent MCP allow resume" />
          </div>
        </Field>
      </SectionCard>

      <SectionCard
        title="Observability"
        icon={Activity}
        desc="Export run metrics + spans (timings, token counts, outcomes — never prompt or issue text) to your fleet-observability collector. On by default; turn it off to stop reporting."
      >
        <Field
          label="Export telemetry"
          inline
          hint="Reports to the internal OTel hub. Turn off to opt out — the endpoint below is kept for re-enabling."
        >
          <div style={{ display: "flex", justifyContent: "flex-end" }}>
            <Toggle
              checked={value.telemetryEnabled}
              onChange={(v) => set("telemetryEnabled", v)}
              aria-label="Export telemetry"
            />
          </div>
        </Field>
        <Field
          label="Telemetry endpoint"
          inline
          hint="OTLP collector for run metrics + spans. Defaults to the configured OTLP collector."
        >
          <TextInput
            mono
            value={value.telemetryEndpoint}
            placeholder="https://otel-symphony.ops-oma-prod.makewhat.is"
            onChange={(e) => set("telemetryEndpoint", e.target.value)}
          />
        </Field>
      </SectionCard>
    </div>
  );
}
