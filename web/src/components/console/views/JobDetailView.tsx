import { useMemo, useState } from "react";
import {
  Button,
  Card,
  Chip,
  Grid,
  GridSide,
  Markdown,
  Mono,
  Pill,
  Seg,
  TeammateAvatar,
  TicketChip,
  Timestamp,
} from "@/components/console";
import { teammateColor } from "@/theme/teammates";
import { useIssueHistory, useTranscript } from "@/hooks/useRunDetail";
import { useLinearIdentity } from "@/hooks/useConfig";
import { useResumeRun, useStopRun } from "@/hooks/useRunActions";
import { useTeamsEnabled, useTeamsOverview, useTeamsRoom } from "@/hooks/useTeams";
import { useTicketFacts } from "@/hooks/useTicketFacts";
import { ticketAssignees } from "@/lib/console-jobs";
import {
  checksSummary,
  clockTime,
  mergeNote,
  runOutcomePill,
  runsNewestFirst,
  type PullRequestView,
} from "@/lib/console-job-detail";
import {
  TRACE_FILTERS,
  TRACE_FILTER_LABELS,
  cardLead,
  filterPhases,
  leadParagraph,
  phaseGlyph,
  prSearchUrl,
  resultEyebrow,
  runVitals,
  ticketUrl,
  type RunVitals,
  type TraceFilter,
} from "@/lib/console-trace-view";
import {
  baseToolName,
  buildResult,
  buildTrace,
  type DidCard,
  type ResultCard,
  type SaidBlock,
  type TracePhase,
} from "@/lib/trace-model";
import type { LogEntry, RunSummary, TeamsFact, TeamsRoomMessage } from "@/lib/api";
import { CrossGlyph, PendingGlyph, TickGlyph } from "./glyphs";
import "@/theme/console-trace.css";

// Job detail — the "Trace" run detail (STUDIO-742), the three zones of the design record
// `~/.rhapsody/docs/console-run-detail-design.md` §3, rebuilt over STUDIO-683's summary strip and
// flat runs list:
//
//   (A) a sticky header — key, title, assignee, outcome, attempt selector, mono vitals, actions;
//   (B) a Result card  — the run's outcome promoted to the top, its hand-off body rendered as
//       sanitized markdown (STUDIO-739) in the slice-1 model's labelled sub-blocks;
//   (C) The Split      — a phase spine on the left, an inspector on the right that shows the
//       selected phase's DID call-cards first and its SAID prose muted and collapsed.
//
// Plus the escape hatch §4 calls mandatory: a "Raw transcript" toggle that drops to the flat
// oldest→newest `LogEntry` list. The folding is a documented heuristic — a debugger is never
// trapped inside it.
//
// The model behind all three zones is `lib/trace-model` (slice 1) and `lib/console-trace-view`;
// nothing here re-derives it. What no endpoint serves is still not invented: there is no PR
// number (§5), so "View PR" resolves through a head-branch search and "Merge" names the daemon
// endpoint it is waiting on. The §4 side cards below the zones — the PR dependency card, the room
// slice, this ticket's memory — are unchanged, and move into the watch-tabs rail in slice 4.

export function JobDetailView({
  issue,
  onNavigate,
}: {
  issue: string;
  onNavigate: (route: "jobs" | "memory") => void;
}) {
  const history = useIssueHistory(issue);
  const teamsEnabled = useTeamsEnabled();
  const overview = useTeamsOverview(teamsEnabled);

  const runs = useMemo(() => runsNewestFirst(history.data?.runs ?? []), [history.data]);
  // The attempt the zones render. `null` follows the newest run, so a ticket that gains a run
  // while the page is open moves with it; picking an attempt pins the choice.
  const [pinned, setPinned] = useState<number | null>(null);
  const run = runs.find((r) => r.id === pinned) ?? runs[0];
  // Live-only for now: a stored run row carries no identity. STUDIO-735's per-run `identity` is
  // wired into this header — and into the spine's attribution — by slice 5.
  const assignee = ticketAssignees(overview.data).get(issue) ?? "";
  const roster = (overview.data?.roster ?? []).map((m) => m.name);

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

      {run === undefined ? (
        <>
          <h1 className="trtitle">{issue}</h1>
          <div className="empty">
            {history.isPending ? "Loading runs…" : "This ticket has no recorded runs."}
          </div>
        </>
      ) : (
        <RunTrace
          run={run}
          runs={runs}
          assignee={assignee}
          roster={roster}
          onBack={() => onNavigate("jobs")}
          onSelectRun={setPinned}
        />
      )}

      <Grid>
        <PullRequestCard pr={null} />
        <GridSide>
          {teamsEnabled ? <RoomSliceCard issue={issue} roster={roster} /> : null}
          {teamsEnabled ? (
            <TicketMemoryCard issue={issue} roster={roster} onOpenMemory={() => onNavigate("memory")} />
          ) : null}
        </GridSide>
      </Grid>
    </section>
  );
}

/** One attempt, rendered as the three zones. Keyed by run id so a switch resets every selection. */
function RunTrace({
  run,
  runs,
  assignee,
  roster,
  onBack,
  onSelectRun,
}: {
  run: RunSummary;
  runs: readonly RunSummary[];
  assignee: string;
  roster: readonly string[];
  onBack: () => void;
  onSelectRun: (id: number) => void;
}) {
  const inFlight = run.outcome === "running";
  const transcript = useTranscript(run.id, inFlight);
  const entries = useMemo(() => transcript.data?.entries ?? [], [transcript.data]);
  const trace = useMemo(() => buildTrace(entries), [entries]);
  const result = useMemo(() => buildResult(entries, run), [entries, run]);
  const vitals = runVitals(run, trace.phases);
  const [raw, setRaw] = useState(false);

  return (
    <>
      <TraceHeader
        run={run}
        runs={runs}
        assignee={assignee}
        roster={roster}
        vitals={vitals}
        onBack={onBack}
        onSelectRun={onSelectRun}
      />

      <div className="trmode">
        <div className="rt">
          <Seg
            aria-label="Transcript rendering"
            options={[
              { value: "trace", label: "Trace" },
              { value: "raw", label: "Raw transcript" },
            ]}
            value={raw ? "raw" : "trace"}
            onChange={(v) => setRaw(v === "raw")}
          />
        </div>
      </div>

      {raw ? (
        <RawTranscript entries={entries} pending={transcript.isPending} />
      ) : (
        <>
          <ResultCardZone run={run} result={result} vitals={vitals} />
          <TraceSplit
            key={run.id}
            phases={trace.phases}
            assignee={assignee}
            pending={transcript.isPending}
          />
        </>
      )}
    </>
  );
}

// --- (A) the sticky header -----------------------------------------------------------------

function TraceHeader({
  run,
  runs,
  assignee,
  roster,
  vitals,
  onBack,
  onSelectRun,
}: {
  run: RunSummary;
  runs: readonly RunSummary[];
  assignee: string;
  roster: readonly string[];
  vitals: RunVitals;
  onBack: () => void;
  onSelectRun: (id: number) => void;
}) {
  const workspaceURLKey = useLinearIdentity().data?.workspace_url_key ?? "";
  return (
    <div className="trhd">
      <button type="button" className="back" aria-label="Back to Jobs" onClick={onBack}>
        ‹
      </button>
      <div className="idw">
        <div className="k">{run.issue_identifier}</div>
        <h1>{run.title === "" ? run.issue_identifier : run.title}</h1>
      </div>
      {assignee === "" ? null : (
        <span className="who2">
          <TeammateAvatar color={teammateColor(roster, assignee)} size={7} />
          {assignee}
        </span>
      )}
      <Pill variant={runOutcomePill(run.outcome)}>
        {run.outcome === "" ? "unknown" : run.outcome}
      </Pill>
      {/* The attempt selector. Slice 3 turns this into the implement→review relay, with the
          hand-off baton and each attempt's own teammate; here it only picks which run to read. */}
      <Seg
        className="trattempts"
        aria-label="Attempt"
        options={runs.map((r) => ({ value: String(r.id), label: `attempt ${r.attempt}` }))}
        value={String(run.id)}
        onChange={(v) => onSelectRun(Number(v))}
      />
      <div className="trvitals">
        <span>
          <b>{vitals.duration}</b>
        </span>
        <span>
          <b>{vitals.turns}</b>
        </span>
        <span>
          <b>{vitals.tokens}</b> tokens
        </span>
        <Mono>{vitals.branch}</Mono>
      </div>
      <HeaderActions run={run} ticketHref={ticketUrl(workspaceURLKey, run.issue_identifier)} />
    </div>
  );
}

function HeaderActions({ run, ticketHref }: { run: RunSummary; ticketHref: string }) {
  const stop = useStopRun(run.id);
  const resume = useResumeRun(run.id);
  const prHref = prSearchUrl(run);
  return (
    <div className="acts">
      {run.outcome === "running" ? (
        <Button variant="sec" onClick={() => stop.mutate()} disabled={stop.isPending}>
          Stop
        </Button>
      ) : null}
      {run.outcome === "stopped" ? (
        <Button onClick={() => resume.mutate()} disabled={resume.isPending}>
          Resume
        </Button>
      ) : null}
      {/* The Messages composer that this button opens is slice 4 of the design record's §9 plan
          (`POST /api/v1/runs/{id}/message` exists; the surface to type into does not yet). */}
      <DepButton title="Waiting on the run-message composer, slice 4 of the Trace plan.">
        Message
      </DepButton>
      {ticketHref === "" ? (
        <DepButton title="No Linear workspace is connected, so the ticket has no deep link.">
          Open ticket
        </DepButton>
      ) : (
        <a className="btn sec" href={ticketHref} target="_blank" rel="noreferrer noopener">
          Open ticket
        </a>
      )}
      {/* No endpoint serves a PR number (design record §5), so this is a head-branch search on
          the run's own remote — it finds the branch's PR without the console asserting one. */}
      {prHref === "" ? (
        <DepButton title="No daemon pull-request endpoint, and this run's remote is not a GitHub one to search.">
          View PR
        </DepButton>
      ) : (
        <a className="btn sec" href={prHref} target="_blank" rel="noreferrer noopener">
          View PR
        </a>
      )}
      <DepButton title="Merging needs the run-branch diff endpoint, deferred in the Trace plan.">
        Merge
      </DepButton>
    </div>
  );
}

/**
 * An action whose surface does not exist yet. It names the dependency in its own tooltip rather
 * than being a dead control or, worse, one that pretends to act (design record §5/§6).
 */
function DepButton({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <button type="button" className="btn sec" disabled title={title}>
      {children}
      <span className="dep">dep</span>
    </button>
  );
}

// --- (B) the Result card -------------------------------------------------------------------

function ResultCardZone({
  run,
  result,
  vitals,
}: {
  run: RunSummary;
  result: ResultCard;
  vitals: RunVitals;
}) {
  const eyebrow = resultEyebrow(run, result.source);
  const lead = cardLead(result);
  return (
    <div className={eyebrow.tone === "done" ? "trrc" : `trrc ${eyebrow.tone}`}>
      <div className="bar" />
      <div className="in">
        <div className="body">
          <div className="eyebrow">{eyebrow.text}</div>
          <h2>{result.headline}</h2>
          {lead === "" ? null : <Markdown source={lead} />}
          {result.sections.map((section, i) => (
            <div className="sect" key={`${i}:${section.heading}`}>
              <div className="lab">{section.label}</div>
              {/* The author's OWN heading beside the model's label: the label is this console's
                  reading of it, and the operator can see what was actually written. */}
              <div className="head">{section.heading}</div>
              <Markdown source={section.body} />
            </div>
          ))}
        </div>
        <div className="trreceipt">
          <div className="rv">
            <b>duration</b>
            {vitals.duration}
          </div>
          <div className="rv">
            <b>turns</b>
            {vitals.turns}
          </div>
          <div className="rv">
            <b>tokens</b>
            {vitals.tokens}
          </div>
          <div className="rv">
            <b>tools</b>
            {vitals.tools}
          </div>
        </div>
      </div>
    </div>
  );
}

// --- (C) The Split -------------------------------------------------------------------------

function TraceSplit({
  phases,
  assignee,
  pending,
}: {
  phases: readonly TracePhase[];
  assignee: string;
  pending: boolean;
}) {
  const [filter, setFilter] = useState<TraceFilter>("all");
  const [query, setQuery] = useState("");
  const [picked, setPicked] = useState<string | null>(null);

  const visible = useMemo(() => filterPhases(phases, filter, query), [phases, filter, query]);
  // The pick is a PREFERENCE, not the selection: a filter that hides the picked phase moves the
  // inspector onto the first phase still visible rather than emptying it, and restoring the
  // filter brings the pick back.
  const selected = visible.find((phase) => phase.id === picked) ?? visible[0];

  return (
    <div className="trsplit">
      <div className="trspine">
        <div className="trfilter">
          {TRACE_FILTERS.map((id) => (
            <Chip key={id} pressed={filter === id} onClick={() => setFilter(id)}>
              {TRACE_FILTER_LABELS[id]}
            </Chip>
          ))}
        </div>
        <input
          type="search"
          className="grep"
          aria-label="Filter steps"
          placeholder="Filter steps…"
          value={query}
          onChange={(e) => setQuery(e.target.value)}
        />
        {visible.map((phase) => (
          <SpineStep
            key={phase.id}
            phase={phase}
            selected={phase.id === selected?.id}
            onSelect={() => setPicked(phase.id)}
          />
        ))}
        {phases.length === 0 ? (
          <div className="empty">
            {pending ? "Loading transcript…" : "No transcript recorded for this run."}
          </div>
        ) : null}
        {phases.length > 0 && visible.length === 0 ? (
          <div className="empty">No step matches.</div>
        ) : null}
      </div>
      <div className="trinsp">
        {selected === undefined ? null : <Inspector phase={selected} assignee={assignee} />}
      </div>
    </div>
  );
}

function SpineStep({
  phase,
  selected,
  onSelect,
}: {
  phase: TracePhase;
  selected: boolean;
  onSelect: () => void;
}) {
  return (
    <button
      type="button"
      className={phase.failed ? "trstep err" : "trstep"}
      aria-pressed={selected}
      onClick={onSelect}
    >
      <span className="g" aria-hidden="true">
        {phaseGlyph(phase.kind)}
      </span>
      <span className="txt">
        <span className="stt">{phase.title}</span>
        {phase.subtitle === "" ? null : <span className="ssub">{phase.subtitle}</span>}
        {phase.effects.length === 0 ? null : (
          <span className="fx">
            {phase.effects.map((effect) => (
              <span className={`fxchip ${effect.kind}`} key={effect.kind}>
                {effect.label}
              </span>
            ))}
          </span>
        )}
      </span>
    </button>
  );
}

/** The selected phase's frame: what the agent DID first, then — muted — what it SAID (§2). */
function Inspector({ phase, assignee }: { phase: TracePhase; assignee: string }) {
  const who = assignee === "" ? "the agent" : assignee;
  return (
    <>
      <h4>
        {phase.title} — what {who} did
      </h4>
      {phase.did.map((card) => (
        <CallCard key={card.seq} card={card} />
      ))}
      {phase.did.length === 0 ? <div className="empty">No tool calls in this step.</div> : null}
      {/* A result with no call to fold onto — a truncated transcript. Surfaced, never dropped. */}
      {phase.orphanResults.map((text, i) => (
        <div className="trcard" key={`orphan:${i}`}>
          <div className="out">
            <pre tabIndex={0}>{text}</pre>
          </div>
        </div>
      ))}
      {phase.said.length === 0 ? null : <Said said={phase.said} who={who} />}
    </>
  );
}

/**
 * One DID: a collapsed one-liner that expands to the tool's own folded result.
 *
 * A failing call starts OPEN and tinted, because the whole point of the spine is that the
 * operator should not have to hunt for the step that broke (design record §3C).
 */
function CallCard({ card }: { card: DidCard }) {
  const [override, setOverride] = useState<boolean | null>(null);
  const open = override ?? card.failed;
  const hasResult = card.result !== "";
  return (
    <div className={`trcard${open ? " open" : ""}${card.failed ? " err" : ""}`}>
      <button
        type="button"
        className="top"
        aria-expanded={open}
        onClick={() => setOverride(!open)}
      >
        <span className="cg" aria-hidden="true">
          {phaseGlyph(card.kind)}
        </span>
        <span className="tool">{baseToolName(card.tool)}</span>
        <span className="tgt">{card.target}</span>
        {/* The daemon serves no exit code, so the badge says only what the folded result proves:
            that it failed, that it came back, or that nothing came back at all. */}
        <span className={card.failed ? "res bad" : hasResult ? "res ok" : "res"}>
          {card.failed ? "error" : hasResult ? "ok" : "—"}
        </span>
        <span className="caret" aria-hidden="true">
          ▸
        </span>
      </button>
      {open ? (
        <div className="out">
          {/* `tabIndex` is what makes the scroll reachable without a mouse: a scrollable region
              that cannot take focus cannot be scrolled from the keyboard at all. */}
          <pre tabIndex={0}>{hasResult ? card.result : "No result recorded for this call."}</pre>
        </div>
      ) : null}
    </div>
  );
}

/** SAID: markdown, muted, collapsed to its lead; thinking dimmed behind a `reasoning` toggle. */
function Said({ said, who }: { said: readonly SaidBlock[]; who: string }) {
  const [expanded, setExpanded] = useState(false);
  const [reasoning, setReasoning] = useState(false);
  const prose = said
    .filter((block) => block.kind === "text")
    .map((block) => block.text)
    .join("\n\n");
  const thinking = said
    .filter((block) => block.kind === "thinking")
    .map((block) => block.text)
    .join("\n\n");
  const lead = leadParagraph(prose);
  return (
    <div className="trsaid">
      <div className="hdr">
        <span className="sg" aria-hidden="true">
          ◔
        </span>
        <span className="lab">what {who} said</span>
      </div>
      {prose === "" ? null : (
        <>
          <Markdown className="prose" source={expanded ? prose : lead} />
          {lead === prose ? null : (
            <button type="button" className="more" onClick={() => setExpanded(!expanded)}>
              {expanded ? "Show less" : "Show more"}
            </button>
          )}
        </>
      )}
      {thinking === "" ? null : (
        <div className="think">
          <button
            type="button"
            aria-expanded={reasoning}
            onClick={() => setReasoning(!reasoning)}
          >
            reasoning ▸
          </button>
          {reasoning ? <Markdown source={thinking} /> : null}
        </div>
      )}
    </div>
  );
}

// --- the raw-transcript escape hatch (§4) --------------------------------------------------

/**
 * Today's flat oldest→newest `LogEntry` list. The folding above is a documented heuristic over a
 * transcript that carries no structured tool metadata, so the design record makes this hatch
 * mandatory: the text is printed VERBATIM here, markdown and all, because this is the view whose
 * job is to show what the daemon actually served.
 */
function RawTranscript({ entries, pending }: { entries: readonly LogEntry[]; pending: boolean }) {
  return (
    <div className="trraw">
      {entries.map((entry) => (
        <div className="rawline" key={entry.seq} tabIndex={0}>
          <span className="rk">{entry.kind}</span>
          {entry.tool === "" ? "" : ` ${entry.tool}`} {entry.text}
        </div>
      ))}
      {entries.length === 0 ? (
        <div className="empty">
          {pending ? "Loading transcript…" : "No transcript recorded for this run."}
        </div>
      ) : null}
    </div>
  );
}

// --- the §4 side cards, unchanged — they move into slice 4's watch-tabs rail --------------

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
            <Markdown source={post.body} />
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
            <Markdown source={fact.content} />
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
