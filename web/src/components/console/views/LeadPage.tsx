import { useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Button, Card, Note, Pill } from "@/components/console";
import { fetchLeadDecisions, overruleLeadDecision, type LeadDecision } from "@/lib/api";
export type { LeadDecision } from "@/lib/api";

export function LeadPage({ decisions, onOverrule }: {
  decisions: LeadDecision[]; onOverrule: (id: number, note: string) => Promise<void>;
}) {
  return <section><h1>Lead</h1><p className="sub">Decisions, reasoning and evidence. Overrule records your preference and asks the lead to undo or redo.</p>
    <Card title="Decision history" sub={`${decisions.length} decisions`}>
      {decisions.length === 0 ? <div className="empty">No lead decisions yet.</div> : [...decisions].reverse().map((decision) => <Decision key={decision.id} decision={decision} onOverrule={onOverrule} />)}
    </Card>
  </section>;
}

function Decision({ decision, onOverrule }: { decision: LeadDecision; onOverrule: (id: number, note: string) => Promise<void> }) {
  const [editing, setEditing] = useState(false);
  const [note, setNote] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [submitted, setSubmitted] = useState(false);
  const overruled = decision.overruled_at != null || submitted;
  const proposal = decision.decision.startsWith("proposed:");
  const escalation = decision.decision.startsWith("escalate:");
  const submit = async () => {
    setBusy(true); setError("");
    try { await onOverrule(decision.id, note.trim()); setSubmitted(true); setEditing(false); }
    catch (err) { setError(err instanceof Error ? err.message : "Overrule could not be recorded."); }
    finally { setBusy(false); }
  };
  return <article aria-label={`Decision ${decision.id}`} style={{ padding: 18, borderBottom: "1px solid var(--line)" }}>
    <div style={{ display: "flex", gap: 12, alignItems: "center", flexWrap: "wrap" }}>
      <b>{decision.subject}</b><Pill variant={overruled ? "blocked" : escalation ? "blocked" : proposal ? "review" : "done"}>{overruled ? "Overruled" : escalation ? "Needs you" : proposal ? "Proposal" : "Decided"}</Pill>
      <span className="sub">#{decision.id} · {new Date(decision.at).toLocaleString()} · {decision.trigger.replaceAll("_", " ")}</span>
      {!overruled && decision.decision !== "applying" ? <Button onClick={() => setEditing(true)}>Overrule</Button> : null}
    </div>
    <p>{decision.decision}</p>
    <h3>Reasoning</h3><p style={{ whiteSpace: "pre-wrap", overflowWrap: "anywhere" }}>{decision.reasoning}</p>
    <h3>Evidence and memories used</h3><pre style={{ whiteSpace: "pre-wrap", overflowWrap: "anywhere" }}>{decision.evidence}</pre>
    <details><summary>Actions · {decision.harness} / {decision.model}</summary><pre style={{ whiteSpace: "pre-wrap", overflowWrap: "anywhere" }}>{decision.actions}</pre></details>
    {overruled ? <Note variant="warn">{decision.overrule_note ?? note} <span className="sub">Undo/redo work queued for the lead.</span></Note> : null}
    {editing ? <form onSubmit={(event) => { event.preventDefault(); void submit(); }}>
      <label htmlFor={`overrule-${decision.id}`}>Operator preference</label>
      <textarea id={`overrule-${decision.id}`} value={note} onChange={(event) => setNote(event.target.value)} maxLength={3000} disabled={busy} style={{ display: "block", width: "100%", margin: "10px 0" }} />
      <Button type="submit" disabled={busy || !note.trim()}>{busy ? "Submitting…" : "Submit overrule"}</Button>{" "}<Button type="button" disabled={busy} onClick={() => setEditing(false)}>Cancel</Button>
      {error ? <div role="alert">{error}</div> : null}
    </form> : null}
  </article>;
}

export function LeadRoute() {
  const client = useQueryClient();
  const [warning, setWarning] = useState("");
  const query = useQuery({ queryKey: ["lead-decisions"], queryFn: () => fetchLeadDecisions(), refetchInterval: 5000, refetchOnWindowFocus: false });
  if (query.isPending) return <div className="empty">Loading lead decisions…</div>;
  if (query.isError) return <div role="alert">{query.error.message}</div>;
  return <>{warning ? <Note variant="warn">{warning}</Note> : null}<LeadPage decisions={query.data.decisions} onOverrule={async (id, note) => {
    const result = await overruleLeadDecision(id, note);
    setWarning(result.memory_retained ? "" : "Overrule recorded and work queued, but preference memory is unavailable. The note remains on the decision.");
    await client.invalidateQueries({ queryKey: ["lead-decisions"] });
  }} />{query.data.queued.length > 0 ? <Card title="Queued lead work" sub="Non-escalation work waits until tomorrow when the daily cap is reached.">{query.data.queued.map((item) => <p key={item.id} style={{ padding: "0 18px" }}>{item.subject} · {item.state}</p>)}</Card> : null}</>;
}
