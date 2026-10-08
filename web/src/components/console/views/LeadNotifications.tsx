import { useEffect, useId, useRef, useState } from "react";
import { Card, Chip, Note } from "@/components/console";
import { useLeadDecisions } from "@/hooks/useLeadDecisions";
import { leadNeedsOperator, leadNotifications, leadSummary } from "@/lib/lead";

/** A compact notification centre shared by the dashboard and desktop window. */
export function LeadNotifications() {
  const query = useLeadDecisions();
  const [open, setOpen] = useState(false);
  const id = useId();
  const root = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!open) return;
    const dismiss = (event: MouseEvent) => {
      if (root.current && !root.current.contains(event.target as Node)) setOpen(false);
    };
    const escape = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        setOpen(false);
        root.current?.querySelector<HTMLButtonElement>("button")?.focus();
      }
    };
    document.addEventListener("mousedown", dismiss);
    document.addEventListener("keydown", escape);
    return () => {
      document.removeEventListener("mousedown", dismiss);
      document.removeEventListener("keydown", escape);
    };
  }, [open]);
  const entries = leadNotifications(query.data?.decisions ?? []);
  if (!query.enabled || entries.length === 0) return null;
  const needsYou = entries.filter(leadNeedsOperator).length;
  return <div className="lead-notifications" ref={root}>
    <Chip aria-expanded={open} aria-controls={id} onClick={() => setOpen(!open)}>
      Lead notifications · {entries.length} · {needsYou} {needsYou === 1 ? "needs" : "need"} you
    </Chip>
    {open ? <Card id={id} title="Lead notifications" className="lead-centre" right={<Chip onClick={() => setOpen(false)}>Close</Chip>}>
      <ul aria-label="Lead notifications">
        {[...entries].reverse().map((decision) => {
          const escalation = leadNeedsOperator(decision);
          const verb = escalation ? "escalated" : decision.decision.startsWith("proposed:") ? "proposed" : "decided";
          return <li key={decision.id}>
            <Note variant={escalation ? "operator" : "info"}>
              <span className="lead-summary">Lead {verb} {decision.subject}: {leadSummary(decision)}</span>
              <a href="#lead" onClick={() => setOpen(false)}>Reasoning and Overrule</a>
            </Note>
          </li>;
        })}
      </ul>
    </Card> : null}
  </div>;
}
