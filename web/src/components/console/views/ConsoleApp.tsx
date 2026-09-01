import { useMemo } from "react";
import {
  AppShell,
  JobsIcon,
  MemoryIcon,
  SettingsIcon,
  TeamsIcon,
  type NavItemSpec,
} from "@/components/console";
import { useConsoleRoute } from "@/hooks/useConsoleRoute";
import { useIssueRuns } from "@/hooks/useHistory";
import { useStateQuery } from "@/hooks/useStateQuery";
import { useVersionQuery } from "@/hooks/useTeams";
import { consoleNavFor, type ConsoleRoute, type ConsoleRouteName } from "@/lib/console-routing";
import { JobDetailView } from "./JobDetailView";
import { JobsView } from "./JobsView";
import { SettingsView } from "./SettingsView";

// The Rhapsody Console shell — STUDIO-681 §2, built by STUDIO-683. The persistent rail on every
// view, the capability gate that decides what it contains, and the router that decides what the
// main column renders.
//
// The gate is ONE field on `GET /api/v1/version`: `teams_enabled`. With Teams off the rail is
// Jobs and Settings and NOTHING ELSE — Teams and Memory are absent from the DOM rather than
// greyed out, because a disabled row still advertises a feature the operator cannot reach
// (§2.2). `useConsoleRoute` applies the same gate to the route itself.
export function ConsoleApp() {
  const version = useVersionQuery();
  // Tri-state on purpose: `undefined` until /api/v1/version answers. A daemon too old to carry
  // the field answers with it absent, which settles to `false` — off — rather than staying
  // unknown forever. See `useConsoleRoute` for why the difference matters.
  const teamsEnabled = version.data === undefined ? undefined : version.data.teams_enabled === true;
  const [route, navigate] = useConsoleRoute(teamsEnabled);
  const openJobs = useOpenJobCount();

  const items = useMemo<NavItemSpec[]>(
    () => [
      { id: "jobs", label: "Jobs", icon: <JobsIcon />, count: openJobs },
      // Unknown is treated as off HERE, deliberately: the rail must not advertise a surface the
      // daemon has not confirmed it has. Unlike the route gate this costs nothing if it is
      // briefly wrong — an item appears a moment later; it does not rewrite anyone's URL.
      { id: "teams", label: "Teams", icon: <TeamsIcon />, enabled: teamsEnabled === true },
      { id: "memory", label: "Memory", icon: <MemoryIcon />, enabled: teamsEnabled === true },
      { id: "settings", label: "Settings", icon: <SettingsIcon />, separatorBefore: true },
    ],
    [teamsEnabled, openJobs],
  );

  const go = (name: ConsoleRouteName, key = "") => navigate({ name, key } as ConsoleRoute);

  return (
    <AppShell
      items={items}
      active={consoleNavFor(route)}
      onNavigate={(id) => go(id as ConsoleRouteName)}
      foot={<RailFoot version={version.data?.version ?? ""} teamsEnabled={teamsEnabled === true} />}
    >
      <ConsoleBody route={route} teamsEnabled={teamsEnabled} go={go} />
    </AppShell>
  );
}

function ConsoleBody({
  route,
  teamsEnabled,
  go,
}: {
  route: ConsoleRoute;
  teamsEnabled: boolean | undefined;
  go: (name: ConsoleRouteName, key?: string) => void;
}) {
  // A teams-only route reached before the capability is known renders nothing rather than
  // guessing: one frame of blank beats a placeholder for a view that may be about to redirect.
  if (
    teamsEnabled === undefined &&
    (route.name === "teams" || route.name === "memory" || route.name === "manage")
  ) {
    return null;
  }

  switch (route.name) {
    case "job":
      return <JobDetailView issue={route.key} onNavigate={(to) => go(to)} />;
    case "settings":
      return <SettingsView teamsEnabled={teamsEnabled === true} onManageTeam={() => go("manage")} />;
    // teams / memory / manage are only ever reached with Teams ON — `useConsoleRoute` sends
    // them to Jobs otherwise (§2.4) — and their views are sub-tickets 3–5 of STUDIO-681.
    case "teams":
      return <Pending title="Teams" ticket="STUDIO-684" section="§5" />;
    case "memory":
      return <Pending title="Memory" ticket="STUDIO-685" section="§6" />;
    case "manage":
      return <Pending title="Manage team" ticket="STUDIO-686" section="§7" />;
    default:
      return <JobsView onOpenJob={(issue) => go("job", issue)} />;
  }
}

/** A view this slice does not build, named with the sub-ticket that does. */
function Pending({ title, ticket, section }: { title: string; ticket: string; section: string }) {
  return (
    <section>
      <div className="head">
        <h1>{title}</h1>
      </div>
      <p className="lead">
        {section} of the dashboard redesign is built by {ticket}. This slice (STUDIO-683) builds
        the shell, Jobs, Job detail and Settings.
      </p>
    </section>
  );
}

/** The rail's foot: the daemon's live state, its build, and the capability flags (§2.1). */
function RailFoot({ version, teamsEnabled }: { version: string; teamsEnabled: boolean }) {
  const state = useStateQuery();
  const port = typeof window === "undefined" ? "" : window.location.port;
  const live = state.data?.status === "ok";
  return (
    <>
      <span className={live ? "live" : undefined}>{live ? "● live" : "○ offline"}</span>
      {port === "" ? "" : ` · port ${port}`}
      <br />
      {version === "" ? "version unknown" : version}
      <br />
      {teamsEnabled ? "teams on" : "solo"}
    </>
  );
}

/** The Jobs nav count — tickets the daemon currently has work for. */
function useOpenJobCount(): number {
  const state = useStateQuery();
  const issues = useIssueRuns();
  const keys = new Set<string>();
  for (const r of state.data?.running ?? []) keys.add(r.issue_identifier);
  for (const r of state.data?.retrying ?? []) keys.add(r.issue_identifier);
  for (const b of state.data?.blocked ?? []) keys.add(b.issue_identifier);
  for (const i of issues.data?.issues ?? []) keys.add(i.issue_identifier);
  keys.delete("");
  return keys.size;
}
