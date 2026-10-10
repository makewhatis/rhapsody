import { useEffect, useState, type ReactNode } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Button, Card, Note, Pill } from "@/components/console";
import { fetchLeadHarnesses, saveLeadHarnesses, overruleLeadDecision, resolveLeadEscalation, type LeadDecision, type LeadDecisionsResponse } from "@/lib/api";
import { LeadHarnessCard } from "./LeadHarnessCard";
import { LEAD_DECISIONS_QUERY_KEY, useLeadDecisions } from "@/hooks/useLeadDecisions";
export type { LeadDecision } from "@/lib/api";

export function LeadPage({ decisions, work = [], held = [], onOverrule, onResolve, runsOn }: {
  decisions: LeadDecision[]; work?: LeadDecisionsResponse["queued"]; held?: LeadDecisionsResponse["held"]; onOverrule: (id: number, note: string) => Promise<void>; onResolve?: (id: number, note: string) => Promise<void>; runsOn?: ReactNode;
}) {
  return <section><h1>Lead</h1><p className="sub">Decisions, reasoning and evidence. Overrule records your preference and asks the lead to undo or redo.</p>
    {runsOn}
    {held.length > 0 ? <Card title="Held subjects" sub="Waiting for a material change or operator action.">
      {held.map((item) => <div key={item.id}><b>{item.subject}</b>{" "}<Pill variant="operator">Held</Pill><p>{item.need}</p>{item.kind === "escalation" ? <span className="sub">{item.repeat_count} escalations</span> : null}</div>)}
    </Card> : null}
    {work.length > 0 ? <Card title="Lead work" sub="Non-escalation work waits until tomorrow when the daily cap is reached.">
      {work.map((item) => <p key={item.id} className="lead-work"><b>{item.subject}</b>{" "}<Pill variant={item.state === "running" ? "run" : "queued"}>{item.state === "running" ? "Running" : item.state === "parked" ? "Parked" : "Queued"}</Pill></p>)}
    </Card> : null}
    <Card title="Decision history" sub={`${decisions.length} decisions`}>
      {decisions.length === 0 ? <div className="empty">No lead decisions yet.</div> : [...decisions].reverse().map((decision) => <Decision key={decision.id} decision={decision} onOverrule={onOverrule} onResolve={onResolve} />)}
    </Card>
  </section>;
}

function Decision({ decision, onOverrule, onResolve }: { decision: LeadDecision; onOverrule: (id: number, note: string) => Promise<void>; onResolve?: (id: number, note: string) => Promise<void> }) {
  const [editing, setEditing] = useState(false);
  const [note, setNote] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [submitted, setSubmitted] = useState(false);
  const [resolving, setResolving] = useState(false);
  const overruled = decision.overruled_at != null || (submitted && !resolving);
  const finished = overruled || submitted;
  const proposal = decision.decision.startsWith("proposed:");
  const escalation = decision.decision.startsWith("escalate:");
  const submit = async () => {
    setBusy(true); setError("");
    try { if (resolving && onResolve) await onResolve(decision.id, note.trim()); else await onOverrule(decision.id, note.trim()); setSubmitted(true); setEditing(false); }
    catch (err) { setError(err instanceof Error ? err.message : "Overrule could not be recorded."); }
    finally { setBusy(false); }
  };
  return <article id={`decision-${decision.id}`} aria-label={`Decision ${decision.id}`} style={{ padding: 18, borderBottom: "1px solid var(--line)" }}>
    <div style={{ display: "flex", gap: 12, alignItems: "center", flexWrap: "wrap" }}>
      <b>{decision.subject}</b><Pill variant={finished ? "queued" : escalation ? "operator" : "done"}>{submitted && resolving ? "Resolved" : overruled ? "Overruled" : escalation ? (decision.escalation_episode != null ? "Needs you" : "Closed escalation") : proposal ? "Proposal" : "Decided"}</Pill>
      <span className="sub">#{decision.id} · {new Date(decision.at).toLocaleString()} · {decision.trigger.replaceAll("_", " ")}</span>
      {!finished && decision.decision !== "applying" ? <Button onClick={() => { setResolving(false); setEditing(true); }}>Overrule</Button> : null}
      {!finished && decision.escalation_episode != null && onResolve ? <Button onClick={() => { setResolving(true); setEditing(true); }}>Resolve</Button> : null}
    </div>
    <p>{decision.decision}</p>
    <h3>Reasoning</h3><p style={{ whiteSpace: "pre-wrap", overflowWrap: "anywhere" }}>{decision.reasoning}</p>
    <h3>Evidence and memories used</h3><pre style={{ whiteSpace: "pre-wrap", overflowWrap: "anywhere" }}>{decision.evidence}</pre>
    <details><summary>Actions · {decision.harness} / {decision.model}</summary><pre style={{ whiteSpace: "pre-wrap", overflowWrap: "anywhere" }}>{decision.actions}</pre></details>
    {overruled ? <Note variant="warn">{decision.overrule_note ?? note} <span className="sub">Undo/redo work queued for the lead.</span></Note> : null}
    {submitted && resolving ? <Note>{note} <span className="sub">Escalation resolved; the subject park is lifted.</span></Note> : null}
    {editing ? <form onSubmit={(event) => { event.preventDefault(); void submit(); }}>
      <label htmlFor={`overrule-${decision.id}`}>{resolving ? "Resolution note" : "Operator preference"}</label>
      <textarea id={`overrule-${decision.id}`} value={note} onChange={(event) => setNote(event.target.value)} maxLength={3000} disabled={busy} style={{ display: "block", width: "100%", margin: "10px 0" }} />
      <Button type="submit" disabled={busy || !note.trim()}>{busy ? "Submitting…" : resolving ? "Resolve escalation" : "Submit overrule"}</Button>{" "}<Button type="button" disabled={busy} onClick={() => { setEditing(false); setResolving(false); }}>Cancel</Button>
      {error ? <div role="alert">{error}</div> : null}
    </form> : null}
  </article>;
}

export function LeadRoute({ entry = "" }: { entry?: string }) {
  const client = useQueryClient();
  const [warning, setWarning] = useState("");
  const query = useLeadDecisions();
  const harnesses = useQuery({ queryKey: ["lead-harnesses"], queryFn: fetchLeadHarnesses, refetchInterval: 30_000 });
  useEffect(() => { if (entry) document.getElementById(entry)?.scrollIntoView?.({ block: "start" }); }, [entry, query.data]);
  if (query.isPending) return <div className="empty">Loading lead decisions…</div>;
  if (query.isError) return <div role="alert">{query.error.message}</div>;
  return <>{warning ? <Note variant="warn">{warning}</Note> : null}
    <LeadPage decisions={query.data.decisions} work={query.data.queued} held={query.data.held} runsOn={harnesses.data ? <LeadHarnessCard live={harnesses.data} onSave={async (entries) => {
      const result = await saveLeadHarnesses(entries);
      client.setQueryData(["lead-harnesses"], result.active);
      return result;
    }} /> : harnesses.isError ? <Note variant="warn">{harnesses.error.message}</Note> : <div className="sub">Loading lead harnesses…</div>} onResolve={async (id, note) => {
    await resolveLeadEscalation(id, note);
    await client.invalidateQueries({ queryKey: LEAD_DECISIONS_QUERY_KEY });
  }} onOverrule={async (id, note) => {
    const result = await overruleLeadDecision(id, note);
    setWarning(result.memory_retained ? "" : "Overrule recorded and work queued, but preference memory is unavailable. The note remains on the decision.");
    await client.invalidateQueries({ queryKey: LEAD_DECISIONS_QUERY_KEY });
  }} /></>;
}
