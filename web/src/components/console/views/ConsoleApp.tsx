import { useMemo, useState } from "react";
import {
  AppShell,
  JobsIcon,
  MemoryIcon,
  SettingsIcon,
  TeamsIcon,
  type NavItemSpec,
} from "@/components/console";
import { TeamsConsole } from "@/components/console/teams/TeamsConsole";
import { useConsoleRoute } from "@/hooks/useConsoleRoute";
import { useDaemonStatus } from "@/hooks/useDaemonStatus";
import { useIssueRuns } from "@/hooks/useHistory";
import { useStateQuery } from "@/hooks/useStateQuery";
import { useReinstateFact, useVersionQuery } from "@/hooks/useTeams";
import { useUpdater, type Updater } from "@/hooks/useUpdater";
import { consoleNavFor, type ConsoleRoute, type ConsoleRouteName } from "@/lib/console-routing";
import { viewForStatus } from "@/lib/daemon-status";
import { FirstRunView, OnboardErrorBanner } from "./FirstRunView";
import { JobDetailView } from "./JobDetailView";
import { JobsView } from "./JobsView";
import { ManageTeamView } from "./ManageTeamView";
import { MemoryView } from "./MemoryView";
import { SettingsView } from "./SettingsView";
import { LogsView, ToolsView, UpdatesView } from "./SettingsTabView";
import { WorkflowView } from "./WorkflowView";

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
  const daemon = useDaemonStatus();
  // A partial first-run write — WORKFLOW.md landed but the daemon would not start — lifted out of
  // the wizard, because the ~2s status poll can see `configured: true` from that same half-written
  // config and unmount the wizard (and its inline alert) before it has been read. Held here so it
  // survives into the console. Mirrors the Podium shell's `onboardErr`.
  const [onboardErr, setOnboardErr] = useState("");
  // Tri-state on purpose: `undefined` until /api/v1/version answers. A daemon too old to carry
  // the field answers with it absent, which settles to `false` — off — rather than staying
  // unknown forever. See `useConsoleRoute` for why the difference matters.
  const teamsEnabled = version.data === undefined ? undefined : version.data.teams_enabled === true;
  const [route, navigate] = useConsoleRoute(teamsEnabled);
  const openJobs = useOpenJobCount();
  // ONE updater instance, owned by the shell and shared — the hook's own contract (P11 U3), and the
  // reason the Podium shell mounts it too. Here it feeds both the Settings "Updates" row's pending
  // badge and the Updates view itself, so the two can never disagree. Without the Tauri bridge every
  // binding it calls is a no-op, so the daemon-served dashboard mounts it inert.
  const updater = useUpdater();

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

  // First run (§8.1). No WORKFLOW.md means no config behind any rail destination, so the wizard
  // REPLACES the shell rather than sitting on a route inside it — the same trade the Podium shell
  // makes, and the reason this pre-empts a deep link. Only ever true under the supervisor bridge:
  // a plain browser has no `getStatus`, and a null snapshot reads as "loading", not
  // "not-configured", so the daemon-served dashboard is untouched by this.
  if (viewForStatus(daemon.status) === "not-configured") {
    return (
      <FirstRunView
        onConfigured={() => void daemon.refresh()}
        onError={setOnboardErr}
        error={onboardErr}
        onDismissError={() => setOnboardErr("")}
      />
    );
  }

  return (
    <AppShell
      items={items}
      active={consoleNavFor(route)}
      onNavigate={(id) => go(id as ConsoleRouteName)}
      foot={<RailFoot version={version.data?.version ?? ""} teamsEnabled={teamsEnabled === true} />}
    >
      <OnboardErrorBanner message={onboardErr} onDismiss={() => setOnboardErr("")} />
      <ConsoleBody route={route} teamsEnabled={teamsEnabled} go={go} updater={updater} />
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
  updater,
}: {
  route: ConsoleRoute;
  teamsEnabled: boolean | undefined;
  go: (name: ConsoleRouteName, key?: string) => void;
  updater: Updater;
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
      return (
        <SettingsView
          teamsEnabled={teamsEnabled === true}
          updater={updater}
          onManageTeam={() => go("manage")}
          onEditWorkflow={() => go("workflow")}
          onOpen={(to) => go(to)}
        />
      );
    // The WORKFLOW.md editor the Settings "Workflow" row opens (§8, STUDIO-690). It is NOT
    // teams-gated: WORKFLOW.md is the solo daemon's config too.
    case "workflow":
      return <WorkflowView onNavigate={(to) => go(to)} />;
    // The three Settings children of §8.1 (STUDIO-691), each embedding the shipped Podium tab the
    // §2.2.1 flip would otherwise strand. Like `workflow`, none is teams-gated — the tool doctor,
    // the log tail and the desktop updater all exist on a solo daemon.
    case "tools":
      return <ToolsView onNavigate={(to) => go(to)} />;
    case "logs":
      return <LogsView onNavigate={(to) => go(to)} />;
    case "updates":
      return <UpdatesView onNavigate={(to) => go(to)} updater={updater} />;
    // teams / memory / manage are only ever reached with Teams ON — `useConsoleRoute` sends
    // them to Jobs otherwise (§2.4). All three views are built: §5's room (STUDIO-684), §6's
    // memory page (STUDIO-685) and §7's manage form (STUDIO-686).
    case "teams":
      return <TeamsConsole onNavigate={(to) => go(to)} />;
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
