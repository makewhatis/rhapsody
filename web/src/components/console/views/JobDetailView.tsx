import { useMemo, useState } from "react";
import {
  Button,
  Card,
  Grid,
  GridSide,
  Mono,
  Pill,
  TeammateAvatar,
  TicketChip,
  Timestamp,
} from "@/components/console";
import { teammateColor } from "@/theme/teammates";
import { useIssueHistory, useTranscript } from "@/hooks/useRunDetail";
import { useIssueRuns } from "@/hooks/useHistory";
import { useNow } from "@/hooks/useNow";
import { useStateQuery } from "@/hooks/useStateQuery";
import { useTeamsEnabled, useTeamsOverview, useTeamsRoom } from "@/hooks/useTeams";
import { useTicketFacts } from "@/hooks/useTicketFacts";
import { lifecycleByIssue, ticketAssignees } from "@/lib/console-jobs";
import {
  buildJobSummary,
  checksSummary,
  clockTime,
  mergeNote,
  runDescription,
  runMeta,
  runOutcomePill,
  runsNewestFirst,
  transcriptTimeline,
  type PullRequestView,
  type TimelineKind,
} from "@/lib/console-job-detail";
import type { RunSummary, TeamsFact, TeamsRoomMessage } from "@/lib/api";
import {
  ClockGlyph,
  CrossGlyph,
  NoteGlyph,
  PendingGlyph,
  PostGlyph,
  RetainGlyph,
  TickGlyph,
  ToolGlyph,
} from "./glyphs";

// Job detail — one ticket's full history (STUDIO-681 §4), built by STUDIO-683: the breadcrumb,
// the summary strip, the runs list with per-run transcripts, and the side column's PR card,
// room slice and memory.
//
// `lib/console-job-detail.ts` records the two things §4 asks for that no endpoint serves — a
// run's KIND and the pull request. Neither is fabricated here.
export function JobDetailView({
  issue,
  onNavigate,
}: {
  issue: string;
  onNavigate: (route: "jobs" | "memory") => void;
}) {
  const nowMs = useNow(30_000);
  const history = useIssueHistory(issue);
  const state = useStateQuery();
  const teamsEnabled = useTeamsEnabled();
  const overview = useTeamsOverview(teamsEnabled);

  // The ticket's own lifecycle (STUDIO-702). `/api/v1/issues/<KEY>/history` serves RUNS and
  // carries no tracker state, so the header reads it from the issue-level listing the Jobs
  // worklist already reads — the default filter, so this shares JobsView's query cache rather
  // than adding a fetch when the operator arrives from the worklist. A deep link fetches it
  // once. The listing is paged: a ticket that falls off the page resolves to `undefined`, and
  // `buildJobSummary` then falls back to the run outcome exactly as it did before (STUDIO-706).
  const issueRuns = useIssueRuns();
  const lifecycle = useMemo(
    () => lifecycleByIssue(issueRuns.data?.issues ?? []).get(issue)?.lifecycle,
    [issueRuns.data, issue],
  );

  const runs = useMemo(() => runsNewestFirst(history.data?.runs ?? []), [history.data]);
  const live = (state.data?.running ?? []).some((r) => r.issue_identifier === issue);
  const assignee = ticketAssignees(overview.data).get(issue) ?? "";
  const roster = (overview.data?.roster ?? []).map((m) => m.name);
  const summary = buildJobSummary(runs, { live, assignee, lifecycle, nowMs });

  return (
    <section>
      <div className="crumbs">
        <a
          href="#jobs"
          onClick={(e) => {
            e.preventDefault();
            onNavigate("jobs");
          }}
        >
          Jobs
        </a>{" "}
        · {issue}
      </div>

      <div className="jobhd">
        <div className="tt">
          <div className="k">
            {issue}
            {summary.project === "" ? "" : ` · ${summary.project}`}
          </div>
          <h1>{summary.title === "" ? issue : summary.title}</h1>
        </div>
        <div className="acts">
          {/* Summon and the Linear deep link are §4 actions that need surfaces this slice does
              not own (a tracker URL, POST /runs/<id>/message on a run that may not exist), so
              they are not wired to something they would only pretend to do. */}
          <Button variant="sec" disabled title="Not wired in this slice">
            Summon
          </Button>
        </div>
      </div>

      <div className="jsummary">
        <Kv label="Status">
          <Pill variant={summary.status}>{summary.statusLabel}</Pill>
        </Kv>
        <Kv label="Assignee">
          {summary.assignee === "" ? (
            "—"
          ) : (
            <>
              <TeammateAvatar color={teammateColor(roster, summary.assignee)} size={7} />
              {summary.assignee}
            </>
          )}
        </Kv>
        <Kv label="Pull request">{summary.pullRequest === "" ? "—" : summary.pullRequest}</Kv>
        <Kv label="Branch">
          <Mono style={{ fontSize: "11.5px" }}>{summary.branch}</Mono>
        </Kv>
        <Kv label="Runs">{summary.runs}</Kv>
        <Kv label="Updated">{summary.updated}</Kv>
      </div>

      <Grid>
        <Card title="Runs" sub={`newest first · ${runs.length} ${runs.length === 1 ? "dispatch" : "dispatches"}`}>
          <div className="runs">
            {runs.map((run, i) => (
              <RunRow key={run.id} run={run} identity={assignee} defaultOpen={i === 0} />
            ))}
          </div>
          {runs.length === 0 ? (
            <div className="empty">{history.isPending ? "Loading runs…" : "This ticket has no recorded runs."}</div>
          ) : null}
        </Card>

        <GridSide>
          <PullRequestCard pr={null} />
          {teamsEnabled ? <RoomSliceCard issue={issue} roster={roster} /> : null}
          {teamsEnabled ? (
            <TicketMemoryCard issue={issue} roster={roster} onOpenMemory={() => onNavigate("memory")} />
          ) : null}
        </GridSide>
      </Grid>
    </section>
  );
}

function Kv({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="kv">
      <div className="l">{label}</div>
      <div className="v">{children}</div>
    </div>
  );
}

// One run row. The expansion mounts only while the row is open, so a ticket with twenty runs
// fetches one transcript — the one the operator asked to see — rather than twenty.
function RunRow({
  run,
  identity,
  defaultOpen,
}: {
  run: RunSummary;
  identity: string;
  defaultOpen: boolean;
}) {
  const [open, setOpen] = useState(defaultOpen);
  return (
    <details className="run" open={open} onToggle={(e) => setOpen(e.currentTarget.open)}>
      <summary>
        <span className="rid">run {run.id}</span>
        <span className="lab">attempt {run.attempt}</span>
        <span className="desc">{runDescription(run)}</span>
        <span className="rt">
          <Pill variant={runOutcomePill(run.outcome)}>
            {run.outcome === "" ? "unknown" : run.outcome}
          </Pill>
          <span className="car" aria-hidden="true">
            ▸
          </span>
        </span>
      </summary>
      {open ? <RunExpansion run={run} identity={identity} /> : null}
    </details>
  );
}

function RunExpansion({ run, identity }: { run: RunSummary; identity: string }) {
  const inFlight = run.outcome === "running";
  const transcript = useTranscript(run.id, inFlight);
  const meta = runMeta(run, identity);
  const timeline = transcriptTimeline(transcript.data?.entries ?? []);

  return (
    <div className="exp">
      <div className="rmeta">
        {meta.identity === "" ? null : <span>{meta.identity}</span>}
        <span>
          {meta.window} · {meta.duration}
        </span>
        <span>{meta.turns}</span>
        <span>{meta.tokens}</span>
      </div>
      <div className="trace">
        {timeline.map((line) => (
          <div key={line.seq} className={line.kind === "done" ? "tline done" : "tline"}>
            <TimelineGlyph kind={line.kind} />
            <span>
              {line.tool === "" ? null : <code>{line.tool}</code>}
              {line.tool === "" ? "" : " "}
              {line.text}
              {line.result === "" ? "" : ` — ${line.result}`}
            </span>
          </div>
        ))}
        {timeline.length === 0 ? (
          <div className="tline">
            <NoteGlyph className="tic" />
            <span>{transcript.isPending ? "Loading transcript…" : "No transcript recorded for this run."}</span>
          </div>
        ) : null}
      </div>
    </div>
  );
}

function TimelineGlyph({ kind }: { kind: TimelineKind }) {
  switch (kind) {
    case "tool":
      return <ToolGlyph className="tic" />;
    case "post":
      return <PostGlyph className="tic" />;
    case "retain":
      return <RetainGlyph className="tic" />;
    case "note":
      return <NoteGlyph className="tic" />;
    case "done":
      return <TickGlyph className="tic" />;
    default:
      return <ClockGlyph className="tic" />;
  }
}

/**
 * The §4 pull-request card. `pr === null` is the SHIPPED state: no daemon endpoint carries a
 * PR, its checks or its mergeability (§9/§11), so the card names the dependency instead of
 * inventing one. Exported so §10 box 2.11 can exercise the populated card directly.
 */
export function PullRequestCard({ pr }: { pr: PullRequestView | null }) {
  if (pr === null) {
    return (
      <Card title="Pull request">
        <div className="empty">
          No pull-request data — the daemon serves no PR endpoint yet. Tracked as a dependency of
          STUDIO-681 §9.
        </div>
      </Card>
    );
  }
  const note = mergeNote(pr);
  const { failed } = checksSummary(pr.checks);
  return (
    <Card title="Pull request" right={<TicketChip variant="pr">{pr.number}</TicketChip>}>
      <div className="checks">
        {pr.checks.map((check) => (
          <div key={check.name} className={`chk ${checkClass(check.state)}`}>
            <CheckGlyph state={check.state} />
            <span className="nm2">{check.name}</span>
            <span className="rt">{check.detail}</span>
          </div>
        ))}
      </div>
      <div className="prnote">
        {note.blocked ? <b>{failed > 0 ? "Blocked: " : "Waiting: "}</b> : null}
        {note.text}
      </div>
    </Card>
  );
}

function checkClass(state: PullRequestView["checks"][number]["state"]): string {
  return state === "pass" ? "ok" : state === "fail" ? "bad" : "pending";
}

function CheckGlyph({ state }: { state: PullRequestView["checks"][number]["state"] }) {
  if (state === "pass") return <TickGlyph className="st" />;
  if (state === "fail") return <CrossGlyph className="st" />;
  return <PendingGlyph className="st" />;
}

/** "Room · this ticket" (§4) — the room posts that reference this key. */
function RoomSliceCard({ issue, roster }: { issue: string; roster: readonly string[] }) {
  const room = useTeamsRoom(true);
  const posts = useMemo(
    () => roomPostsFor(room.data?.messages ?? [], issue),
    [room.data, issue],
  );
  return (
    <Card title="Room · this ticket">
      <div className="memprev">
        {posts.map((post) => (
          <div className="mcard" key={post.id}>
            <div className="top">
              <span
                className="who2"
                style={{ color: post.from === "operator" ? "var(--operator)" : teammateColor(roster, post.from) }}
              >
                {post.from}
              </span>
              <Timestamp>{clockTime(post.at)}</Timestamp>
            </div>
            <p>{post.body}</p>
          </div>
        ))}
        {posts.length === 0 ? (
          <div className="empty">{room.isPending ? "Loading room…" : "No room posts reference this ticket."}</div>
        ) : null}
      </div>
    </Card>
  );
}

/**
 * A post belongs to a ticket when it REFERENCES the key — in `refs`, which is what proves it,
 * or in the body, which is how a teammate writes it in prose. Newest first, matching the room.
 */
export function roomPostsFor(
  messages: readonly TeamsRoomMessage[],
  issue: string,
): TeamsRoomMessage[] {
  if (issue === "") return [];
  return messages
    .filter((m) => (m.refs ?? []).includes(issue) || m.body.includes(issue))
    .slice()
    .reverse();
}

/** "Memory from this ticket" (§4) — the facts this ticket's runs retained. */
function TicketMemoryCard({
  issue,
  roster,
  onOpenMemory,
}: {
  issue: string;
  roster: readonly string[];
  onOpenMemory: () => void;
}) {
  const facts = useTicketFacts(roster, issue);
  return (
    <Card
      title="Memory from this ticket"
      right={
        <a
          className="link"
          href="#memory"
          onClick={(e) => {
            e.preventDefault();
            onOpenMemory();
          }}
        >
          Open →
        </a>
      }
    >
      <div className="memprev">
        {facts.data.map((fact: TeamsFact) => (
          <div className="mcard" key={`${fact.identity}:${fact.id}`}>
            <div className="top">
              <TicketChip variant="sha">{fact.run_id === "" ? fact.id : `run ${fact.run_id}`}</TicketChip>
              <Timestamp>{fact.identity}</Timestamp>
            </div>
            <p>{fact.content}</p>
          </div>
        ))}
        {facts.data.length === 0 ? (
          <div className="empty">
            {facts.isPending ? "Loading memory…" : "No facts were retained from this ticket."}
          </div>
        ) : null}
      </div>
    </Card>
  );
}
