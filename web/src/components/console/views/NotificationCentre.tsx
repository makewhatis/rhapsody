import { createContext, useContext, useEffect, useId, useRef, useState, type ReactNode } from "react";
import { Card, Chip, Note } from "@/components/console";
import { useNoticeFeed, useReadNotice } from "@/hooks/useNoticeFeed";
import { useNow } from "@/hooks/useNow";
import type { DaemonNotice, NoticeGroup } from "@/lib/api";

type Filter = "all" | "needs_you";
interface CentreState {
  notices: DaemonNotice[];
  needsYou: number | null;
  unread: number | null;
  open: boolean;
  filter: Filter;
  show: (filter: Filter) => void;
  close: () => void;
  error: string;
}
const Centre = createContext<CentreState>({ notices: [], needsYou: null, unread: null, open: false,
  filter: "all", show: () => {}, close: () => {}, error: "" });
export const useNotificationCentre = () => useContext(Centre);

export function NotificationProvider({ children }: { children: ReactNode }) {
  const query = useNoticeFeed();
  const [open, setOpen] = useState(false);
  const [filter, setFilter] = useState<Filter>("all");
  const notices = query.data?.notifications ?? [];
  const unread = notices.filter((n) => n.read_at === null);
  return <Centre.Provider value={{ notices,
    needsYou: query.data === undefined ? null : unread.filter((n) => n.group === "needs_you").length,
    unread: query.data === undefined ? null : unread.length,
    open, filter, show: (next) => { setFilter(next); setOpen(true); }, close: () => setOpen(false),
    error: query.isError ? "Notifications could not be refreshed." : "",
  }}>{children}</Centre.Provider>;
}

const groups: [NoticeGroup, string][] = [["needs_you", "Needs you"], ["decisions", "Decisions"], ["system", "System"], ["activity", "Activity"]];
function age(at: string, now: number): string {
  const elapsed = Math.max(0, now - Date.parse(at));
  if (!Number.isFinite(elapsed)) return "age unknown";
  if (elapsed < 60_000) return "just now";
  if (elapsed < 3_600_000) return `${Math.floor(elapsed / 60_000)}m ago`;
  if (elapsed < 86_400_000) return `${Math.floor(elapsed / 3_600_000)}h ago`;
  return `${Math.floor(elapsed / 86_400_000)}d ago`;
}

/** Top-right centre plus the slim, unread-only operator strip. Shared by every route/host. */
export function NotificationCentre() {
  const centre = useNotificationCentre();
  const read = useReadNotice();
  const [expanded, setExpanded] = useState(false);
  const id = useId();
  const stripId = useId();
  const root = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!centre.open) return;
    const outside = (event: MouseEvent) => {
      if (root.current && !root.current.contains(event.target as Node)) centre.close();
    };
    const escape = (event: KeyboardEvent) => {
      if (event.key === "Escape") { centre.close(); root.current?.querySelector<HTMLButtonElement>(".notice-trigger")?.focus(); }
    };
    document.addEventListener("mousedown", outside);
    document.addEventListener("keydown", escape);
    return () => { document.removeEventListener("mousedown", outside); document.removeEventListener("keydown", escape); };
  }, [centre.open, centre.close]);
  const needs = centre.notices.filter((n) => n.group === "needs_you" && n.read_at === null);
  const error = read.isError ? (read.error instanceof Error ? read.error.message : "Read failed") : centre.error;
  return <>
    <div className="notification-bar" ref={root}>
      <Chip className={`notice-trigger${needs.length > 0 ? " operator" : ""}`}
        aria-label={centre.unread === null ? "Notifications loading" : `Notifications, ${centre.unread} unread`}
        aria-expanded={centre.open} aria-controls={id} onClick={() => centre.open ? centre.close() : centre.show("all")}>
        <span aria-hidden="true">!</span> {centre.unread ?? "—"}
      </Chip>
      {centre.open ? <div role="dialog" aria-label="Notifications" id={id} className="notice-panel">
        <Card title="Notifications" right={<Chip onClick={centre.close}>Close</Chip>}>
          <div className="notice-body">
          <div className="notice-filters"><Chip onClick={() => centre.show("all")} pressed={centre.filter === "all"}>All notices</Chip>
            <Chip onClick={() => centre.show("needs_you")} pressed={centre.filter === "needs_you"}>Needs you · {centre.needsYou ?? "—"}</Chip></div>
          {groups.filter(([group]) => centre.filter === "all" || group === centre.filter).map(([group, label]) => {
            const entries = centre.notices.filter((n) => n.group === group);
            return <section key={group} aria-label={label}>
              <h3>{label}{group === "needs_you" ? ` · ${centre.needsYou ?? "—"}` : ""}</h3>
              {entries.length === 0 ? <p className="sub">No notices.</p> : <NoticeList notices={entries} busy={read.isPending} onRead={(id) => read.mutate(id)} onNavigate={centre.close} />}
            </section>;
          })}
          </div>
        </Card>
      </div> : null}
    </div>
    {needs.length > 0 ? <div className="needs-strip" role="region" aria-label="Needs you">
      <Chip className="operator" aria-label={`${needs.length} ${needs.length === 1 ? "thing needs" : "things need"} you`}
        aria-expanded={expanded} aria-controls={stripId} onClick={() => setExpanded(!expanded)}>
        {needs.length} {needs.length === 1 ? "thing needs" : "things need"} you <span aria-hidden="true">{expanded ? "▴" : "▾"}</span>
      </Chip>
      {expanded ? <div id={stripId}><NoticeList notices={needs} busy={read.isPending} onRead={(id) => read.mutate(id)} strip /></div> : null}
    </div> : null}
    {error ? <p className="notice-error" role="alert">{error}</p> : null}
  </>;
}

function NoticeList({ notices, busy, onRead, onNavigate, strip = false }: {
  notices: DaemonNotice[]; busy: boolean; onRead: (id: number) => void; onNavigate?: () => void; strip?: boolean;
}) {
  const now = useNow(30_000);
  return <ul className="notice-list">{[...notices].sort((a, b) => Number(a.read_at !== null) - Number(b.read_at !== null)).map((n) => <li key={n.id} className={n.read_at !== null ? "read" : undefined}>
    <Note variant={n.group === "needs_you" ? "operator" : n.group === "system" ? "warn" : "info"}>
      <div className="notice-line">
        <a className="notice-subject" href={n.href} onClick={onNavigate}>{n.subject}</a>
        <span className="notice-summary">{n.summary}</span>
        <time dateTime={n.at} title={new Date(n.at).toLocaleString()}>{age(n.at, now)}</time>
        {n.read_at === null ? <Chip disabled={busy} aria-label={`${strip ? "Dismiss" : "Mark read"} ${n.subject}`} onClick={() => onRead(n.id)}>{strip ? "Dismiss" : "Read"}</Chip> : <span className="sub">Read</span>}
      </div>
    </Note>
  </li>)}</ul>;
}
