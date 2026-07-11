import { useEffect, useState } from "react";
import { Button } from "@/components/ui/button";
import { useConfigQuery, useSaveConfig } from "@/hooks/useConfig";
import { configWithForm, formFromConfig, type SettingsForm } from "@/lib/config";

// SettingsView edits the daemon's WORKFLOW.md (the core config fields + the advanced prompt
// body) via GET/POST /api/v1/config — replacing hand-editing the file (spec §5). On save the
// daemon validates and hot-reloads; validation errors are surfaced inline.
export function SettingsView() {
  const { data, isLoading, isError } = useConfigQuery();
  const save = useSaveConfig();

  const [form, setForm] = useState<SettingsForm | null>(null);
  const [promptBody, setPromptBody] = useState("");
  // dirty tracks edits since the last load/save, so the "Saved" banner does not linger after the
  // user changes fields again (the banner reflects the CURRENT form, not just the last mutation).
  const [dirty, setDirty] = useState(false);

  // Seed the form once config loads (and whenever a save echoes fresh config back).
  useEffect(() => {
    if (data) {
      setForm(formFromConfig(data.config));
      setPromptBody(data.prompt_body);
      setDirty(false);
    }
  }, [data]);

  // Check the error state BEFORE the loading state: on a failed fetch isLoading is false and
  // data/form are absent, so a loading-first guard would otherwise show "Loading…" forever.
  if (isError) {
    return <div className="text-sm text-[var(--destructive)]">Could not load config.</div>;
  }
  if (isLoading || !form || !data) {
    return <div className="text-sm text-[var(--muted-foreground)]">Loading settings…</div>;
  }

  const set = <K extends keyof SettingsForm>(key: K, value: SettingsForm[K]) => {
    setDirty(true);
    setForm((f) => (f ? { ...f, [key]: value } : f));
  };

  const onSave = () => {
    save.mutate(
      { config: configWithForm(data.config, form), prompt_body: promptBody },
      { onSuccess: () => setDirty(false) },
    );
  };

  return (
    <div className="flex flex-col gap-6">
      <div className="flex items-center justify-between">
        <h2 className="text-lg font-semibold">Settings</h2>
        <div className="flex items-center gap-3">
          {save.isSuccess && !dirty && (
            <span className="text-xs text-[var(--muted-foreground)]">Saved — hot-reloaded.</span>
          )}
          {dirty && <span className="text-xs text-[var(--muted-foreground)]">Unsaved changes</span>}
          <Button size="sm" onClick={onSave} disabled={save.isPending}>
            {save.isPending ? "Saving…" : "Save"}
          </Button>
        </div>
      </div>

      {save.isError && (
        <div role="alert" className="rounded-md border border-[var(--destructive)] px-3 py-2 text-sm text-[var(--destructive)]">
          {(save.error as Error).message}
        </div>
      )}

      <Section title="Linear tracker">
        <Field id="project_slug" label="Project slug" value={form.projectSlug} onChange={(v) => set("projectSlug", v)} />
        <Field id="active_states" label="Active states (comma-separated)" value={form.activeStates} onChange={(v) => set("activeStates", v)} />
        <Field id="terminal_states" label="Terminal states (comma-separated)" value={form.terminalStates} onChange={(v) => set("terminalStates", v)} />
        <Field id="review_states" label="Review states (comma-separated)" value={form.reviewStates} onChange={(v) => set("reviewStates", v)} />
        <Field id="review_promote_state" label="Review promote state" value={form.reviewPromoteState} onChange={(v) => set("reviewPromoteState", v)} />
        <Field id="milestone" label="Milestone" value={form.milestone} onChange={(v) => set("milestone", v)} />
      </Section>

      <Section title="Agent">
        <Field id="max_concurrent_agents" label="Max concurrent agents" type="number" value={form.maxConcurrentAgents} onChange={(v) => set("maxConcurrentAgents", v)} />
        <Field id="max_turns" label="Max turns" type="number" value={form.maxTurns} onChange={(v) => set("maxTurns", v)} />
      </Section>

      <Section title="Claude">
        <Field id="model" label="Model" value={form.model} onChange={(v) => set("model", v)} />
        <Field id="permission_mode" label="Permission mode" value={form.permissionMode} onChange={(v) => set("permissionMode", v)} />
        <label className="flex items-center gap-2 text-sm">
          <input
            type="checkbox"
            checked={form.billingGuard}
            onChange={(e) => set("billingGuard", e.target.checked)}
          />
          Billing guard (require a Claude subscription, not an API key)
        </label>
      </Section>

      <Section title="Runtime">
        <Field id="workspace_root" label="Workspace root" value={form.workspaceRoot} onChange={(v) => set("workspaceRoot", v)} />
        <Field id="server_port" label="Server port" type="number" value={form.serverPort} onChange={(v) => set("serverPort", v)} />
      </Section>

      <Section title="Prompt body (advanced)">
        <label htmlFor="prompt_body" className="text-xs text-[var(--muted-foreground)]">
          The Liquid-templated prompt the agent receives. Defaults to the proven WORKFLOW.example.md body.
        </label>
        <textarea
          id="prompt_body"
          aria-label="Prompt body"
          className="min-h-48 w-full rounded-md border border-[var(--border)] bg-transparent p-2 font-mono text-xs"
          value={promptBody}
          onChange={(e) => {
            setDirty(true);
            setPromptBody(e.target.value);
          }}
        />
      </Section>
    </div>
  );
}

function Section({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <section className="rounded-lg border border-[var(--border)] p-4">
      <h3 className="mb-3 text-sm font-semibold text-[var(--muted-foreground)]">{title}</h3>
      <div className="flex flex-col gap-3">{children}</div>
    </section>
  );
}

function Field({
  id,
  label,
  value,
  onChange,
  type = "text",
}: {
  id: string;
  label: string;
  value: string;
  onChange: (v: string) => void;
  type?: string;
}) {
  return (
    <div className="flex flex-col gap-1">
      <label htmlFor={id} className="text-xs text-[var(--muted-foreground)]">
        {label}
      </label>
      <input
        id={id}
        type={type}
        className="w-full rounded-md border border-[var(--border)] bg-transparent px-2 py-1 text-sm"
        value={value}
        onChange={(e) => onChange(e.target.value)}
      />
    </div>
  );
}
