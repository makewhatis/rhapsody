import type { LimitJobReport } from "@/lib/api";
import { fetchLimitHandoffs, type EventHit } from "@/lib/api";
import { useQuery } from "@tanstack/react-query";
import { Pill, type PillVariant } from "@/components/console";
import { LIVE_POLL_MS, useStateQuery } from "@/hooks/useStateQuery";

export function LimitChips({ reports }: { reports: LimitJobReport[] }) {
  if (reports.length === 0) return null;
  return <div aria-label="Usage limit status" style={{ display: "flex", gap: 8, flexWrap: "wrap", margin: "12px 0" }}>
    {reports.map((report, index) => {
      let text: string;
      let variant: PillVariant = "queued";
      switch (report.state) {
        case "parked": text = `parked: ${report.account} limit, resumes ${report.resume_at_s ? new Date(report.resume_at_s * 1000).toLocaleString() : "unknown"}`; variant = "parked"; break;
        case "switched": text = `switched to ${report.model || "fallback engine"} (limit)`; variant = "done"; break;
        case "handed_off": text = `handed to ${report.identity || "another teammate"}`; variant = "done"; break;
        default: text = `waiting: ${report.account} limit`;
      }
      return <Pill key={`${report.state}-${index}`} variant={variant} title={report.note ? `Handoff note: ${report.note}` : undefined}>{text}</Pill>;
    })}
  </div>;
}

export function jobLimitReports(live: LimitJobReport[], handoffs: EventHit[], latestRunId: number, identity: string): LimitJobReport[] {
  const hit = handoffs[0];
  let previous: LimitJobReport | undefined;
  if (hit) {
    try { previous = JSON.parse(hit.text) as LimitJobReport; } catch { /* An unreadable historical report is not a current claim. */ }
  }
  if (previous && identity && previous.identity && identity !== previous.identity && latestRunId > (hit?.run_id ?? 0)) {
    return [...live, { ...previous, state: "handed_off", identity }];
  }
  if (live.length > 0) return live;
  if (previous && previous.state === "switched") return [previous];
  return [];
}

export function JobLimitChips({ ticket, latestRunId = 0, identity = "" }: { ticket: string; latestRunId?: number; identity?: string }) {
  const query = useStateQuery();
  const history = useQuery({ queryKey: ["limit-handoffs", ticket], queryFn: () => fetchLimitHandoffs(ticket), enabled: ticket !== "", refetchInterval: LIVE_POLL_MS, refetchOnWindowFocus: false });
  return <LimitChips reports={jobLimitReports((query.data?.limit_jobs ?? []).filter((r) => r.ticket === ticket), history.data ?? [], latestRunId, identity)} />;
}
