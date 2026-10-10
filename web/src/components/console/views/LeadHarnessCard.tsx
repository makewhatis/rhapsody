import { useEffect, useId, useRef, useState } from "react";
import { Button, Card, Note, Select } from "@/components/console";
import { fetchProviderCatalog, type LeadHarnessesView, type LeadHarnessEntry, type LeadHarnessState, type LeadHarnessSaveResult } from "@/lib/api";

export function LeadHarnessCard({ live, onSave }: { live: LeadHarnessesView; onSave: (entries: LeadHarnessEntry[]) => Promise<LeadHarnessSaveResult> }) {
  const [active, setActive] = useState(live);
  const [rows, setRows] = useState(live.harnesses);
  const editing = useRef(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [rejected, setRejected] = useState<LeadHarnessState[]>([]);
  useEffect(() => {
    setActive(live);
    if (!editing.current) setRows(live.harnesses);
  }, [live]);
  const edit = (next: LeadHarnessState[]) => { setRows(next); editing.current = true; setRejected([]); setError(""); };
  const move = (index: number, offset: number) => {
    const next = [...rows];
    [next[index], next[index + offset]] = [next[index + offset], next[index]];
    edit(next);
  };
  const save = async () => {
    setBusy(true); setError(""); setRejected([]);
    editing.current = true;
    try {
      const result = await onSave(rows.map(({ harness, model, effort }) => ({ harness, model, effort })));
      setActive(result.active); setRows(result.active.harnesses);
      editing.current = false;
      if (!result.ok) { setError(result.reason); setRejected(result.tested); }
    } catch (err) { setError(err instanceof Error ? err.message : "Lead harnesses could not be saved."); }
    finally { setBusy(false); }
  };
  return <Card title="Runs on" sub="The lead and review manager share this ordered list. Every new entry is isolation-tested before use.">
    <form className="lead-harness-form" onSubmit={(event) => { event.preventDefault(); void save(); }}>
      {rows.map((entry, index) => <div key={index} className="lead-harness-row">
        <HarnessRow entry={entry} index={index} disabled={busy} onChange={(next) => edit(rows.map((row, i) => i === index ? { ...next, state: "Unsaved", tested_at: null } : row))} />
        <div className="lead-harness-actions">
          <Button type="button" aria-label={`Move entry ${index + 1} up`} disabled={busy || index === 0} onClick={() => move(index, -1)}>↑</Button>
          <Button type="button" aria-label={`Move entry ${index + 1} down`} disabled={busy || index === rows.length - 1} onClick={() => move(index, 1)}>↓</Button>
          <Button type="button" aria-label={`Remove entry ${index + 1}`} disabled={busy || rows.length === 1} onClick={() => edit(rows.filter((_, i) => i !== index))}>Remove</Button>
        </div>
        <div className="sub lead-harness-status"><span>{busy ? "Testing…" : entry.state}</span>{entry.tested_at && !busy ? <> · Tested {new Date(entry.tested_at).toLocaleString()}</> : null}</div>
      </div>)}
      {rows.length === 1 ? <p className="sub">No fallback: if this model is unavailable, the lead waits</p> : null}
      <Button type="button" disabled={busy || rows.length >= 4} onClick={() => edit([...rows, { harness: "claude", model: "", effort: "high", state: "Unsaved", tested_at: null }])}>Add entry</Button>{" "}
      <Button type="submit" disabled={busy || rows.some((entry) => !entry.model.trim())}>{busy ? "Testing…" : "Save"}</Button>
      {error ? <div role="alert"><Note variant="warn">{error}</Note>{rejected.map((entry, index) => <p key={index}>{entry.harness} {entry.model} {entry.effort}: {entry.state}</p>)}</div> : null}
      {active.last_used ? <p className="sub">Most recent run: {active.last_used.harness} {active.last_used.model} {active.last_used.effort}</p> : null}
    </form>
  </Card>;
}

function HarnessRow({ entry, index, disabled, onChange }: { entry: LeadHarnessState; index: number; disabled: boolean; onChange: (entry: LeadHarnessState) => void }) {
  const id = useId();
  const [models, setModels] = useState<string[]>([]);
  const provider = entry.harness === "claude" ? "anthropic" : entry.model.includes("/") ? entry.model.split("/")[0] : "openai";
  useEffect(() => {
    let current = true;
    setModels([]);
    void fetchProviderCatalog(provider).then((catalog) => {
      if (current) setModels(catalog.models.map((model) => entry.harness === "opencode" && !model.id.startsWith(`${provider}/`) ? `${provider}/${model.id}` : model.id));
    }).catch(() => { /* Catalogues are suggestions; free text stays available. */ });
    return () => { current = false; };
  }, [provider, entry.harness]);
  const efforts = entry.harness === "claude" ? ["", "low", "medium", "high", "max"] : ["", "none", "minimal", "low", "medium", "high", "xhigh"];
  return <>
    <label>Harness {index + 1}<Select aria-label={`Harness ${index + 1}`} value={entry.harness} options={["claude", "opencode"]} disabled={disabled} onChange={(event) => onChange({ ...entry, harness: event.target.value, effort: "high" })} /></label>
    <label>Model {index + 1}<input type="text" aria-label={`Model ${index + 1}`} list={id} value={entry.model} maxLength={256} disabled={disabled} onChange={(event) => onChange({ ...entry, model: event.target.value })} /><datalist id={id}>{models.map((model) => <option key={model} value={model} />)}</datalist></label>
    <label>Effort {index + 1}<Select aria-label={`Effort ${index + 1}`} value={entry.effort} options={efforts.map((value) => ({ value, label: value || "Default" }))} disabled={disabled} onChange={(event) => onChange({ ...entry, effort: event.target.value })} /></label>
  </>;
}
