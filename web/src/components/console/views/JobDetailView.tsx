import {
  useEffect,
  useMemo,
  useRef,
  useState,
  type CSSProperties,
  type ReactNode,
  type RefObject,
} from "react";
import {
  Button,
  Card,
  Chip,
  Markdown,
  Mono,
  Pill,
  Seg,
  TeammateAvatar,
  TicketChip,
  Timestamp,
} from "@/components/console";
import { teammateColor } from "@/theme/teammates";
import { useIssueHistory, useRunDetail, useTranscript } from "@/hooks/useRunDetail";
import { useLinearIdentity } from "@/hooks/useConfig";
import { useResumeRun, useSendRunMessage, useStopRun } from "@/hooks/useRunActions";
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
import { formatDateTime } from "@/lib/format";
import { isAtBottom } from "@/lib/follow-scroll";
import {
  OUTCOME_RUNNING,
  TRACE_FILTERS,
  TRACE_FILTER_LABELS,
  cardLead,
  failingStep,
  filterPhases,
  leadParagraph,
  liveRunRow,
  phaseGlyph,
  playheadPhase,
  prSearchUrl,
  relayBatons,
  resultBanner,
  resultEyebrow,
  runTeammate,
  runVitals,
  ticketUrl,
  type Baton,
  type FailingStep,
  type RelayBatons,
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
// Slice 3 (STUDIO-744) adds the states that hero does not cover, on top of the same model: a
// LIVE run turns the spine into a playhead that follows the newest phase and streams into the
// inspector; a FAILED one gains a "jump to failing step" out of its banner; and a ticket whose
// work relayed across attempts gains a handoff baton either side of the attempt being read.
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

      {/* The §4 context cards the three zones did not replace. They become the inspector's
          watch-tabs rail in slice 4; until then they keep their own row under the trace. */}
      <div className="trctx">
        <PullRequestCard pr={null} />
        {teamsEnabled ? <RoomSliceCard issue={issue} roster={roster} /> : null}
        {teamsEnabled ? (
          <TicketMemoryCard issue={issue} roster={roster} onOpenMemory={() => onNavigate("memory")} />
        ) : null}
      </div>
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
  // The two polls slice 3 rides on, both already the daemon's own cadence: the run detail at 2s
  // and the transcript at 1.5s. `/issues/{id}/history` is fetched once and cached for 10s, which
  // is right for the attempt LIST and wrong for the attempt being watched — so the row supplies
  // identity and the poll supplies telemetry, including the terminal outcome that ends the stream.
  const detail = useRunDetail(run.id, run.outcome === OUTCOME_RUNNING);
  const live = liveRunRow(run, detail.data);
  const inFlight = live.outcome === OUTCOME_RUNNING;
  const transcript = useTranscript(run.id, inFlight);
  const entries = useMemo(() => transcript.data?.entries ?? [], [transcript.data]);
  const trace = useMemo(() => buildTrace(entries), [entries]);
  const result = useMemo(() => buildResult(entries, live), [entries, live]);
  const vitals = runVitals(live, trace.phases);
  const batons = useMemo(() => relayBatons(runs, run, assignee), [runs, run, assignee]);
  // Resolved ONCE, so the header's avatar and the inspector's "what <who> did" can never disagree
  // about whose run this is — they did while only the header knew about a review key.
  const who = runTeammate(run, assignee);
  const [raw, setRaw] = useState(false);
  const [composing, setComposing] = useState(false);
  // A jump request rather than a selection: the Result card asks, the Split acts. It carries a
  // nonce so asking TWICE re-opens a card the operator folded away between the two clicks — the
  // jump is an instruction, and a bare "which card" would compare equal and do nothing.
  const [jump, setJump] = useState<{ step: FailingStep; nonce: number } | null>(null);
  const failing = useMemo(() => failingStep(trace.phases), [trace.phases]);
  const headerRef = useRef<HTMLDivElement | null>(null);
  const headerHeight = useStickyHeaderHeight(headerRef);

  // Switching attempt drops a jump aimed at the attempt being left, so the incoming Split — which
  // remounts on the same switch — cannot open a card belonging to the trace it just replaced.
  const selectRun = (id: number) => {
    setJump(null);
    setComposing(false);
    onSelectRun(id);
  };

  return (
    // The spine sticks BELOW the header, whose height is not a constant: the action cluster and
    // the attempt selector wrap onto a second row on a narrow window, and a spine pinned to a
    // hardcoded offset then slides underneath it. The measurement is published as a custom
    // property so the offset stays in the stylesheet, and the CSS keeps a fallback for the render
    // before the first measurement — and for any environment without a `ResizeObserver`.
    <div
      className="trrun"
      style={headerHeight === 0 ? undefined : ({ "--trhd-h": `${headerHeight}px` } as CSSProperties)}
    >
      <TraceHeader
        ref={headerRef}
        run={live}
        runs={runs}
        who={who}
        roster={roster}
        vitals={vitals}
        inFlight={inFlight}
        composing={composing}
        onBack={onBack}
        onSelectRun={selectRun}
        onCompose={() => setComposing(!composing)}
      />

      {/* The operator's own line into a running agent (`POST /api/v1/runs/{id}/message`). Only a
          LIVE run can be reached — a finished one has no agent left, so the header's action names
          that instead of offering a send that cannot land — but a composer already open when the
          run ends stays open, rather than taking a half-written instruction down with it. The
          MESSAGE LIST, with its sent→delivered chips, is slice 4's watch-tab. */}
      {composing ? (
        <MessageComposer runId={run.id} live={inFlight} onClose={() => setComposing(false)} />
      ) : null}

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
          <ResultCardZone
            run={live}
            result={result}
            vitals={vitals}
            pending={transcript.isPending}
            onJumpToFailure={
              failing === null
                ? null
                : () => setJump((prev) => ({ step: failing, nonce: (prev?.nonce ?? 0) + 1 }))
            }
          />
          <TraceSplit
            key={run.id}
            phases={trace.phases}
            who={who}
            pending={transcript.isPending}
            live={inFlight}
            batons={batons}
            jump={jump}
          />
        </>
      )}
    </div>
  );
}

/**
 * The rendered height of the sticky header, or 0 before it can be measured.
 *
 * `ResizeObserver` is the only thing that sees the header REFLOW — its cluster wraps on a window
 * resize, on a font swap, and when a lifecycle action's error text appears — and it is absent in
 * jsdom and in older engines, so its absence degrades to the stylesheet's own fallback offset
 * rather than throwing.
 */
function useStickyHeaderHeight(ref: RefObject<HTMLDivElement | null>): number {
  const [height, setHeight] = useState(0);
  useEffect(() => {
    const el = ref.current;
    if (el === null || typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(() => setHeight(el.offsetHeight));
    observer.observe(el);
    return () => observer.disconnect();
  }, [ref]);
  return height;
}

// --- (A) the sticky header -----------------------------------------------------------------

function TraceHeader({
  ref,
  run,
  runs,
  who,
  roster,
  vitals,
  inFlight,
  composing,
  onBack,
  onSelectRun,
  onCompose,
}: {
  ref: RefObject<HTMLDivElement | null>;
  run: RunSummary;
  runs: readonly RunSummary[];
  /** The teammate this attempt is attributed to; "" when none resolves. */
  who: string;
  roster: readonly string[];
  vitals: RunVitals;
  inFlight: boolean;
  composing: boolean;
  onBack: () => void;
  onSelectRun: (id: number) => void;
  onCompose: () => void;
}) {
  const workspaceURLKey = useLinearIdentity().data?.workspace_url_key ?? "";
  return (
    <div className="trhd" ref={ref}>
      <button type="button" className="back" aria-label="Back to Jobs" onClick={onBack}>
        ‹
      </button>
      <div className="idw">
        <div className="k">{run.issue_identifier}</div>
        <h1>{run.title === "" ? run.issue_identifier : run.title}</h1>
      </div>
      {who === "" ? null : (
        <span className="who2">
          <TeammateAvatar color={teammateColor(roster, who)} size={7} />
          {who}
        </span>
      )}
      <Pill variant={runOutcomePill(run.outcome)}>
        {run.outcome === "" ? "unknown" : run.outcome}
      </Pill>
      {/* The live pulse (§3A). Decorative beside the outcome pill, which already says "running"
          in words — a screen reader that announced a second "live" would only repeat it. */}
      {inFlight ? <span className="trpulse" aria-hidden="true" /> : null}
      {/* The attempt selector — the implement→revise relay. Switching swaps the Result card and
          the spine to that run, and the spine draws the handoff baton either side of it
          (`relayBatons`); each attempt's OWN teammate is slice 5, which is why the baton names
          the runs when one ticket identity covers both.

          Labelled by RUN ID, not by `attempt`: the daemon only increments `attempt` on the retry
          path, so a ticket re-summoned or re-dispatched records every one of its runs as attempt
          0 — 432 of the 441 rows the store has ever written — and an "attempt 0" label repeated
          five times names none of them. The run id is the daemon's own handle on a run and is
          always distinct. The attempt and the start time are real data too, so they ride along
          in the tooltip rather than being dropped. */}
      <Seg
        className="trattempts"
        aria-label="Attempt"
        options={runs.map((r) => ({
          value: String(r.id),
          label: (
            <span title={`attempt ${r.attempt} · started ${formatDateTime(r.started_at)}`}>
              run {r.id}
            </span>
          ),
        }))}
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
      <HeaderActions
        run={run}
        ticketHref={ticketUrl(workspaceURLKey, run.issue_identifier)}
        inFlight={inFlight}
        composing={composing}
        onCompose={onCompose}
      />
    </div>
  );
}

function HeaderActions({
  run,
  ticketHref,
  inFlight,
  composing,
  onCompose,
}: {
  run: RunSummary;
  ticketHref: string;
  inFlight: boolean;
  composing: boolean;
  onCompose: () => void;
}) {
  const stop = useStopRun(run.id);
  const resume = useResumeRun(run.id);
  const prHref = prSearchUrl(run);
  // The console has no toast surface, so a lifecycle action reports here or nowhere. Both halves
  // matter: the request can fail, and it can succeed while the ticket MOVE fails — a run killed
  // whose ticket stayed put is something the operator has to finish by hand.
  const problem =
    stop.error?.message ??
    resume.error?.message ??
    stop.data?.move_error ??
    resume.data?.move_error ??
    "";
  return (
    <div className="acts">
      {problem === "" ? null : (
        <span className="acterr" role="status">
          {problem}
        </span>
      )}
      {inFlight ? (
        <Button variant="sec" onClick={() => stop.mutate()} disabled={stop.isPending}>
          Stop
        </Button>
      ) : null}
      {run.outcome === "stopped" ? (
        <Button onClick={() => resume.mutate()} disabled={resume.isPending}>
          Resume
        </Button>
      ) : null}
      {/* Real while the run is live — `POST /api/v1/runs/{id}/message` is an endpoint the daemon
          already serves. On a finished run it is dependency-named for a different reason than the
          rest of this cluster: there is no missing endpoint, there is no agent left to read it. */}
      {inFlight ? (
        <Button
          variant="sec"
          aria-expanded={composing}
          // Named only while the composer is actually mounted: `aria-controls` pointing at an id
          // that is not in the document is a dangling reference, not a hint.
          aria-controls={composing ? MESSAGE_COMPOSER_ID : undefined}
          onClick={onCompose}
        >
          Message
        </Button>
      ) : (
        <DepButton title="This run has ended — there is no agent left to deliver a message to.">
          Message
        </DepButton>
      )}
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
          the run's own remote — it finds the branch's PR without the console asserting one. The
          branch it searches for comes from `runBranch`, since the daemon writes none. */}
      {prHref === "" ? (
        <DepButton title="No daemon pull-request endpoint, and this run's remote is not on github.com, so there is nothing to search.">
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
function DepButton({ title, children }: { title: string; children: ReactNode }) {
  return (
    <button
      type="button"
      className="btn sec dependency"
      // NOT the `disabled` attribute: a disabled button fires no mouse events, so the tooltip
      // that names the dependency would never open and the control would be merely dead.
      aria-disabled="true"
      title={title}
      onClick={(e) => e.preventDefault()}
    >
      {children}
      <span className="dep">dep</span>
    </button>
  );
}

/** The composer's element id, so the header's Message action can name what it expands. */
const MESSAGE_COMPOSER_ID = "trmsg";

/**
 * The operator's line into a running agent (`POST /api/v1/runs/{id}/message`, INF-250).
 *
 * Deliberately just the composer. The message LIST — each row's sent→delivered chip, polled at 2s
 * — is the Messages watch-tab of slice 4, and building half of it here would be the thing the
 * slice plan splits these tickets to avoid. What this owns is the send and its outcome: the
 * console has no toast surface, so a refusal (the daemon caps pending messages per run) reports
 * here or nowhere.
 *
 * It stays mounted when the run ends underneath it (`live` goes false) instead of unmounting with
 * whatever was typed in it: the send is impossible then and says so, but silently discarding an
 * operator's half-written instruction is not this component's call to make.
 */
function MessageComposer({
  runId,
  live,
  onClose,
}: {
  runId: number;
  live: boolean;
  onClose: () => void;
}) {
  const send = useSendRunMessage(runId);
  const [text, setText] = useState("");
  const [problem, setProblem] = useState("");
  const submit = () => {
    const body = text.trim();
    // An empty send is not an error to report — it is nothing to say. The daemon would reject it
    // anyway, and spending a request to be told so is worse than not making one. Nor is there
    // anything to send to once the run has ended.
    if (!live || body === "" || send.isPending) return;
    setProblem("");
    send.mutate(body, {
      onSuccess: () => setText(""),
      onError: (err) => setProblem(err.message),
    });
  };
  return (
    <div className="trmsg" id={MESSAGE_COMPOSER_ID}>
      <textarea
        aria-label="Message the running agent"
        placeholder="The agent picks this up at its next step…"
        maxLength={4000}
        rows={2}
        value={text}
        disabled={send.isPending}
        onChange={(e) => setText(e.target.value)}
        onKeyDown={(e) => {
          // Enter sends, Shift+Enter breaks the line — and a composition (CJK and friends) is
          // being CONFIRMED by that Enter, never sent by it.
          if (e.key === "Enter" && !e.shiftKey && !e.nativeEvent.isComposing) {
            e.preventDefault();
            submit();
          }
        }}
      />
      <div className="row">
        {live ? null : (
          <span className="acterr" role="status">
            This run has ended — there is no agent left to deliver this to.
          </span>
        )}
        {problem === "" ? null : (
          <span className="acterr" role="status">
            {problem}
          </span>
        )}
        <Button onClick={submit} disabled={!live || send.isPending}>
          Send
        </Button>
        <Button variant="sec" onClick={onClose}>
          Close
        </Button>
      </div>
    </div>
  );
}

// --- (B) the Result card -------------------------------------------------------------------

function ResultCardZone({
  run,
  result,
  vitals,
  pending,
  onJumpToFailure,
}: {
  run: RunSummary;
  result: ResultCard;
  vitals: RunVitals;
  pending: boolean;
  /** null when the trace holds no failing step for the banner to point at. */
  onJumpToFailure: (() => void) | null;
}) {
  const eyebrow = resultEyebrow(run, result.source);
  const banner = resultBanner(run);
  const lead = cardLead(result);
  return (
    <div className={eyebrow.tone === "done" ? "trrc" : `trrc ${eyebrow.tone}`}>
      <div className="bar" />
      <div className="in">
        <div className="body">
          <div className="eyebrow">{eyebrow.text}</div>
          {/* §3B's failed/stopped banner. It comes off the RUN ROW, not the transcript, so it is
              the one thing this card can state before the transcript arrives — and the one thing
              it must not drop when a run hands off and only then dies. */}
          {banner === null ? null : (
            <div className={`trbanner ${banner.tone}`}>
              <b>{banner.label}</b>
              <span>{banner.text}</span>
              {/* §3B's "jump to failing step", which that section assigns to the FAILED banner
                  alone ("Stopped -> amber reason + Resume"). Two conditions, for two different
                  reasons: an operator stop is not a failure to jump into even when the transcript
                  holds a red step, and a run that died before it ran anything has an error with
                  nothing to jump to — a control that selects nothing is worse than one that is
                  not there. */}
              {banner.tone !== "fail" || onJumpToFailure === null ? null : (
                <button type="button" className="jump" onClick={onJumpToFailure}>
                  jump to failing step →
                </button>
              )}
            </div>
          )}
          {/* The headline is read out of the transcript, so until it loads this card has no answer
              — and its fallback is phrased as an assertion ("Completed without a written
              hand-off"), which would be a plainly WRONG one for most runs. It waits instead. */}
          {pending ? (
            <div className="trskel" role="status">
              <span className="vh">Loading transcript…</span>
            </div>
          ) : (
            <>
              <h2>{result.headline}</h2>
              {lead === "" ? null : <Markdown source={lead} />}
              {result.sections.map((section, i) => (
                <div className="sect" key={`${i}:${section.heading}`}>
                  <div className="lab">{section.label}</div>
                  {/* The author's OWN heading beside the model's label: the label is this
                      console's reading of it, and the operator can see what was written. */}
                  <div className="head">{section.heading}</div>
                  <Markdown source={section.body} />
                </div>
              ))}
            </>
          )}
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
            {/* Unlike the three above it, this one is counted from the transcript — a bare 0
                while that is still loading would read as "this run called no tools". */}
            {pending ? "—" : vitals.tools}
          </div>
        </div>
      </div>
    </div>
  );
}

// --- (C) The Split -------------------------------------------------------------------------

function TraceSplit({
  phases,
  who,
  pending,
  live,
  batons,
  jump,
}: {
  phases: readonly TracePhase[];
  /** The teammate this attempt is attributed to; "" when none resolves. */
  who: string;
  pending: boolean;
  /** Whether this attempt is still streaming — what turns the spine into a playhead (§3C). */
  live: boolean;
  batons: RelayBatons;
  jump: { step: FailingStep; nonce: number } | null;
}) {
  const [filter, setFilter] = useState<TraceFilter>("all");
  const [query, setQuery] = useState("");
  const [picked, setPicked] = useState<string | null>(null);

  const visible = useMemo(() => filterPhases(phases, filter, query), [phases, filter, query]);
  // The playhead is a claim about where the RUN is, so it is computed over every phase and NOT
  // over the filtered spine: a grep that hides the newest step would otherwise move the `now`
  // marker back onto a step the run has already left, and the badge would be reporting the
  // filter rather than the run.
  const playhead = playheadPhase(phases);
  // What the spine actually MARKS. When the run's newest phase is filtered out, nothing is
  // marked — an honest silence, rather than `now` on the newest phase that happens to be left.
  const playing = live && visible.some((phase) => phase.id === playhead?.id) ? playhead : undefined;
  // The pick is a PREFERENCE, not the selection: a filter that hides the picked phase moves the
  // inspector onto the first phase still visible rather than emptying it, and restoring the
  // filter brings the pick back.
  //
  // What the fallback IS, though, depends on the run. A finished trace is read forwards, from its
  // first step. A live one is read at its head: with nothing picked the inspector sits on the
  // newest VISIBLE phase, so the next poll that appends one carries the selection with it — that
  // is the playhead, and it costs no timer of its own.
  const selected =
    visible.find((phase) => phase.id === picked) ??
    (live ? playheadPhase(visible) : visible[0]);

  // A jump aims at a phase and, when it has one, a call to open. `picked` is SET rather than the
  // selection forced, so the operator can move on from where the jump landed.
  const target = jump?.step ?? null;
  const nonce = jump?.nonce ?? 0;
  // The jump is an INSTRUCTION, and a chip or grep the operator left active is free to ignore a
  // preference: `selected` above discards a `picked` phase the filter hides, so the failing card
  // would never render, the auto-expand would have nothing to fire on, and the banner's only
  // control would be a silent no-op. So the jump CLEARS the filter on its way in, which is the
  // one thing that guarantees its target is on the spine when it lands.
  useEffect(() => {
    if (jump === null) return;
    setPicked(jump.step.phaseId);
    setFilter("all");
    setQuery("");
  }, [jump]);

  // Following means the inspector is tracking the run's head — so a run whose newest phase the
  // filter has HIDDEN is not following it, however little is picked. A run with no phases yet is
  // still following: there is nothing to track, and nothing to have fallen behind either.
  const headHidden = playhead !== undefined && playing === undefined;
  const following = live && !headHidden && (picked === null || picked === playhead?.id);
  const { atBottom, jumpToBottom } = useFollowScroll(following);
  const behind = live && (!following || !atBottom);

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
        {/* The baton this attempt was handed (§3C). It leads the spine because that is when it
            happened: the previous run ended and this one picked the work up. */}
        {batons.incoming === null ? null : <BatonRow baton={batons.incoming} direction="in" />}
        {visible.map((phase) => (
          <SpineStep
            key={phase.id}
            phase={phase}
            selected={phase.id === selected?.id}
            playing={phase.id === playing?.id}
            onSelect={() => setPicked(phase.id)}
          />
        ))}
        {batons.outgoing === null ? null : <BatonRow baton={batons.outgoing} direction="out" />}
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
        {selected === undefined ? null : (
          <Inspector
            phase={selected}
            who={who}
            openSeq={target?.phaseId === selected.id ? target.cardSeq : null}
            openNonce={nonce}
          />
        )}
      </div>
      {/* Only while the run is LIVE: on a finished trace there is no "latest" to fall behind. */}
      {behind ? (
        <button
          type="button"
          className="trlatest"
          onClick={() => {
            setPicked(null);
            // Same reason as the jump above, and ONLY when it applies: a playhead the filter
            // hides cannot be returned to, so a chip offered because the head is off the spine
            // clears the filter to put it back. A chip offered because the operator merely
            // scrolled up has nothing to fix — the head is right there — and throwing away the
            // grep they typed to get the page back to its bottom would be a loss for free.
            if (headHidden) {
              setFilter("all");
              setQuery("");
            }
            jumpToBottom();
          }}
        >
          Jump to latest ↓
        </button>
      ) : null}
    </div>
  );
}

/** The page's own scroller — the run detail scrolls the document, not a box inside it. */
function pageScroller(): HTMLElement | null {
  if (typeof document === "undefined") return null;
  return (document.scrollingElement as HTMLElement | null) ?? document.documentElement;
}

function scrollToBottom() {
  const el = pageScroller();
  // Assigning `scrollTop` rather than calling `scrollTo`: it is the one form every engine the
  // console runs in — a browser, the Tauri webview, and jsdom under test — implements alike.
  if (el !== null) el.scrollTop = el.scrollHeight;
}

/** What the follow rule gives the view: where the page is, and the way back to the bottom. */
interface FollowScroll {
  /** Whether the page is still pinned to the bottom, auto-following what the stream appends. */
  atBottom: boolean;
  /** Take the page back to the bottom and resume following it (the "jump to latest" chip). */
  jumpToBottom: () => void;
}

/**
 * Follow-mode for a live run: reports whether the page is still pinned to the bottom, and keeps it
 * there as the stream appends.
 *
 * The geometry is `lib/follow-scroll`'s, shared verbatim with the logs follow — one definition of
 * "at the bottom", threshold and all, rather than a second one that drifts.
 *
 * Where the operator has scrolled to is tracked whether following is on or OFF, because `active`
 * can turn back on: a grep that hides a live run's head stops the follow, and the next poll to
 * bring in a phase the grep MATCHES turns it on again while growing the page in the same commit.
 * Observing the position only while active meant that commit had no reading of its own to go on —
 * the last one was from before the operator scrolled — and the page dragged them to the bottom.
 */
function useFollowScroll(active: boolean): FollowScroll {
  const [atBottom, setAtBottom] = useState(true);
  // The same reading as `atBottom`, mirrored into a ref: it is what the growth effect below reads,
  // so a commit that carries BOTH a scroll reading and the growth acts on the reading rather than
  // on the value its own render closed over. `hooks/useFollowScroll` mirrors its `following` the
  // same way and for the same reason. Re-reading the page inside that effect would not do —
  // after the growth an operator who never moved reads as "not at the bottom", and the legitimate
  // follow would break.
  const pinned = useRef(true);
  useEffect(() => {
    const el = pageScroller();
    if (el === null) return;
    const read = () => {
      const at = isAtBottom({
        scrollTop: el.scrollTop,
        scrollHeight: el.scrollHeight,
        clientHeight: el.clientHeight,
      });
      pinned.current = at;
      setAtBottom(at);
    };
    // Nothing is read here: the scroll position at mount is the one the PREVIOUS view left in the
    // document, not a choice the operator made about this run, and `lib/follow-scroll`'s other
    // consumer opens a live view at its tail for exactly that reason. The first scroll event is
    // the first thing that speaks for the operator.
    window.addEventListener("scroll", read, { passive: true });
    return () => window.removeEventListener("scroll", read);
  }, []);

  // Checked every render, acted on only when the page actually GREW: a poll that appends a step
  // must not push the newest line off the screen, and every other re-render — a keystroke in the
  // grep field, a filter chip — must not yank the page around. An operator who has scrolled up is
  // never dragged back; that is what the jump chip is for.
  const height = useRef(0);
  useEffect(() => {
    const el = pageScroller();
    if (el === null) return;
    // No reading is written back here: this branch only runs when the page was ALREADY pinned,
    // and it leaves it pinned.
    if (active && pinned.current && el.scrollHeight > height.current) scrollToBottom();
    height.current = el.scrollHeight;
  });

  // The way back, and the one thing that RE-takes the pin: an operator who scrolled up has said
  // they are not following, and only they can say they are again. Assigning the position is not
  // enough on its own — a browser announces a programmatic scroll with a scroll event and jsdom
  // never does, so the reading is set here rather than waited for.
  const jumpToBottom = () => {
    scrollToBottom();
    pinned.current = true;
    setAtBottom(true);
  };
  return { atBottom, jumpToBottom };
}

/**
 * One relay marker — the handoff baton between two of a ticket's runs (§3C/§6, and the design
 * record's "a handoff renders as a baton so a multi-agent ticket reads as a relay").
 *
 * Not a step: it is what happened BETWEEN two attempts, so it has no phase to inspect and does
 * not take the selection.
 */
function BatonRow({ baton, direction }: { baton: Baton; direction: "in" | "out" }) {
  return (
    <div className={`trbaton ${direction}`}>
      <span className="g" aria-hidden="true">
        ⇄
      </span>
      <span className="bt">
        <b>{direction === "in" ? "picked up" : "handed off"}</b> {baton.text}
      </span>
    </div>
  );
}

function SpineStep({
  phase,
  selected,
  playing,
  onSelect,
}: {
  phase: TracePhase;
  selected: boolean;
  /** The playhead sits here — the newest phase of a run that is still streaming. */
  playing: boolean;
  onSelect: () => void;
}) {
  return (
    <button
      type="button"
      className={`trstep${phase.failed ? " err" : ""}${playing ? " now" : ""}`}
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
function Inspector({
  phase,
  who,
  openSeq,
  openNonce,
}: {
  phase: TracePhase;
  who: string;
  /** The call a jump asked to open, when it landed on THIS phase; null otherwise. */
  openSeq: number | null;
  openNonce: number;
}) {
  const name = who === "" ? "the agent" : who;
  return (
    <>
      <h4>
        {phase.title} — what {name} did
      </h4>
      {phase.did.map((card) => (
        <CallCard key={card.seq} card={card} jump={card.seq === openSeq ? openNonce : 0} />
      ))}
      {phase.did.length === 0 ? <div className="empty">No tool calls in this step.</div> : null}
      {/* A result with no call to fold onto — a truncated transcript. Surfaced, never dropped. */}
      {phase.orphanResults.map((text, i) => (
        <div className="trcard orphan" key={`orphan:${i}`}>
          <div className="reslab">result with no matching call</div>
          <div className="out">
            <pre tabIndex={0}>{text}</pre>
          </div>
        </div>
      ))}
      {phase.said.length === 0 ? null : <Said said={phase.said} who={name} />}
    </>
  );
}

/**
 * One DID: a collapsed one-liner that expands to the tool's own folded result.
 *
 * A failing call starts OPEN and tinted, because the whole point of the spine is that the
 * operator should not have to hunt for the step that broke (design record §3C).
 *
 * `jump` is a NONCE, not a boolean: the Result card's "jump to failing step" has to re-open a card
 * the operator folded away since the last jump, and a boolean that is already `true` would change
 * nothing. Zero means no jump has ever asked for this card.
 */
function CallCard({ card, jump }: { card: DidCard; jump: number }) {
  const [override, setOverride] = useState<boolean | null>(null);
  const open = override ?? card.failed;
  useEffect(() => {
    if (jump > 0) setOverride(true);
  }, [jump]);
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
  // Trimmed, because the lead below is: comparing the two decides whether there is more to show,
  // and a block with trailing whitespace would otherwise offer a "Show more" that reveals none.
  const prose = said
    .filter((block) => block.kind === "text")
    .map((block) => block.text)
    .join("\n\n")
    .trim();
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
