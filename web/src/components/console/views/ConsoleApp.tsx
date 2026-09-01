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
import { useReinstateFact, useVersionQuery } from "@/hooks/useTeams";
import { consoleNavFor, type ConsoleRoute, type ConsoleRouteName } from "@/lib/console-routing";
import { JobDetailView } from "./JobDetailView";
import { JobsView } from "./JobsView";
import { ManageTeamView } from "./ManageTeamView";
import { MemoryView } from "./MemoryView";
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

  const go = (name: ConsoleRouteName, key = "") => navigate({ name, key });

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

/**
 * The Memory page plus its one write seam.
 *
 * `MemoryView` takes reinstate as a prop rather than calling the mutation itself, so the view stays
 * a pure render of what it is handed — the shape it shipped with. What changed with STUDIO-689 is
 * that there is now something to hand it: `POST /api/v1/teams/reinstate`. A wrapper component,
 * because `ConsoleBody`'s route switch may not call a hook.
 */
function MemoryPage({ go }: { go: (name: ConsoleRouteName, key?: string) => void }) {
  const reinstate = useReinstateFact();
  return (
    <MemoryView
      onNavigate={(to, key) => go(to, key)}
      onReinstate={async (fact) => {
        await reinstate.mutateAsync({ identity: fact.identity, factID: fact.id });
      }}
    />
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
    // them to Jobs otherwise (§2.4). §6's memory page (STUDIO-685) and §7's manage form
    // (STUDIO-686) are built; the room (§5) is sub-ticket 3 of STUDIO-681.
    case "teams":
      return <Pending title="Teams" section="§5" subTicket={3} />;
    case "memory":
      // "View run" has no run route of its own (§2.3) — a fact's run lives on its ticket's Job
      // detail, which is where the runs list already is.
      return <MemoryPage go={go} />;
    case "manage":
      return <ManageTeamView onNavigate={(to) => go(to)} />;
    default:
      return <JobsView onOpenJob={(issue) => go("job", issue)} />;
  }
}

/**
 * A view this slice does not build, named by the SPEC section and sub-ticket that do. It cites
 * the epic rather than a specific issue id on purpose: the sub-ticket numbering is in the spec
 * and verifiable, whereas guessing at the issue key for a ticket this run cannot read would put
 * an unverified reference into shipped UI text.
 */
function Pending({ title, section, subTicket }: { title: string; section: string; subTicket: number }) {
  return (
    <section>
      <div className="head">
        <h1>{title}</h1>
      </div>
      <p className="lead">
        {section} of the dashboard redesign is sub-ticket {subTicket} of STUDIO-681, and is not
        built yet. This slice (STUDIO-683) builds the shell, Jobs, Job detail and Settings.
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
