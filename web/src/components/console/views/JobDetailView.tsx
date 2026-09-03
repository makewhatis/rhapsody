import {
  useCallback,
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
  Chip,
  Markdown,
  Mono,
  Pill,
  Seg,
  TeammateAvatar,
  TicketChip,
  Timestamp,
} from "@/components/console";
import { handleTablistKeyDown } from "@/components/shell/tabs";
import { teammateColor } from "@/theme/teammates";
import {
  useIssueHistory,
  useRunDetail,
  useRunMessages,
  useTranscript,
} from "@/hooks/useRunDetail";
import { useLinearIdentity } from "@/hooks/useConfig";
import { useResumeRun, useSendRunMessage, useStopRun } from "@/hooks/useRunActions";
import { useReviews } from "@/hooks/useReviews";
import { usePostToRoom, useTeamsEnabled, useTeamsOverview, useTeamsRoom } from "@/hooks/useTeams";
import { useTicketFacts } from "@/hooks/useTicketFacts";
import { ticketAssignees } from "@/lib/console-jobs";
import { clockTime, runOutcomePill, runsNewestFirst } from "@/lib/console-job-detail";
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
  runBranch,
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
  ASK_PAST_WINDOW_NOTE,
  ASK_WAITING_NOTE,
  managerReply,
  type AskedQuestion,
} from "@/lib/console-ask";
import {
  DEFAULT_WATCH_TAB,
  MEMORY_EMPTY_NOTE,
  ROOM_WATCH_WINDOW,
  WATCH_TABS,
  askRefs,
  messageChip,
  reviewsForRun,
  roomEmptyNote,
  roomPostsFor,
  type WatchTabId,
} from "@/lib/console-watch";
import { reviewRow } from "@/lib/reviews-model";
import {
  baseToolName,
  buildResult,
  buildTrace,
  type DidCard,
  type ResultCard,
  type SaidBlock,
  type TracePhase,
} from "@/lib/trace-model";
import type { LogEntry, RunSummary, TeamsFact } from "@/lib/api";
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
// Slice 4 (STUDIO-745) adds §3C's watch-tabs rail under the inspector — Diff / Review / Room /
// Memory / Messages — which is where the §4 side cards that used to sit in their own row below the
// trace now live, joined by the run's operator-message timeline and an "Ask about this run" dock
// that posts to the room refed to the run (§6).
//
// The model behind all three zones is `lib/trace-model` (slice 1) and `lib/console-trace-view`;
// the rail's own is `lib/console-watch`. Nothing here re-derives them. What no endpoint serves is
// still not invented: there is no PR number (§5), so "View PR" resolves through a head-branch
// search, "Merge" names the daemon endpoint it is waiting on, and the Diff tab is a dependency
// card with a deep link rather than a diff nobody served.

export function JobDetailView({
  issue,
  onNavigate,
}: {
  issue: string;
  onNavigate: (route: "jobs" | "memory" | "teams") => void;
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
  // The roster is a PREREQUISITE read, not just a list: a memory bank is per identity, so with no
  // roster there is nobody to recall from and `useTicketFacts` fires nothing at all — settling as
  // an honest, successful, empty answer. That is indistinguishable from "the overview is still in
  // flight" and from "the overview 500'd", and the Memory tab would state "no facts were retained"
  // about banks it never learned the names of. So its own load state travels with it.
  const rosterRead = { isPending: overview.isPending, isError: overview.isError };

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
          rosterRead={rosterRead}
          teamsEnabled={teamsEnabled}
          onBack={() => onNavigate("jobs")}
          onSelectRun={setPinned}
          onOpenMemory={() => onNavigate("memory")}
          onOpenRoom={() => onNavigate("teams")}
        />
      )}
    </section>
  );
}

/** One attempt, rendered as the three zones. Keyed by run id so a switch resets every selection. */
function RunTrace({
  run,
  runs,
  assignee,
  roster,
  rosterRead,
  teamsEnabled,
  onBack,
  onSelectRun,
  onOpenMemory,
  onOpenRoom,
}: {
  run: RunSummary;
  runs: readonly RunSummary[];
  assignee: string;
  roster: readonly string[];
  /** How the roster's own fetch is going — the Memory tab's read depends on it. */
  rosterRead: QueryState;
  teamsEnabled: boolean;
  onBack: () => void;
  onSelectRun: (id: number) => void;
  onOpenMemory: () => void;
  onOpenRoom: () => void;
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
  // The rail's selection, and the draft in its composer. Both live HERE rather than in the panel
  // so that reading the room, then coming back, does not silently discard a half-written
  // instruction — only the panel that is showing is mounted, and its own state dies with it.
  const [tab, setTab] = useState<WatchTabId>(DEFAULT_WATCH_TAB);
  const [draft, setDraft] = useState("");
  // A focus request the composer CONSUMES. It has to survive the tab change that mounts the
  // composer, so it cannot live in the panel — and it has to be cleared once taken, or the panel's
  // mount effect would re-steal focus every later time the operator merely CLICKS the Messages
  // tab, ejecting a keyboard user from the tablist they just used.
  const [focusMessage, setFocusMessage] = useState(false);
  const takeMessageFocus = useCallback(() => setFocusMessage(false), []);
  // A jump request rather than a selection: the Result card asks, the Split acts. It carries a
  // nonce so asking TWICE re-opens a card the operator folded away between the two clicks — the
  // jump is an instruction, and a bare "which card" would compare equal and do nothing.
  const [jump, setJump] = useState<{ step: FailingStep; nonce: number } | null>(null);
  const failing = useMemo(() => failingStep(trace.phases), [trace.phases]);
  const headerRef = useRef<HTMLDivElement | null>(null);
  const headerHeight = useStickyHeaderHeight(headerRef);

  // Switching attempt drops a jump aimed at the attempt being left, so the incoming Split — which
  // remounts on the same switch — cannot open a card belonging to the trace it just replaced.
  //
  // The draft goes with it. An instruction written for run 522 is not an instruction for run 547,
  // and silently retargeting it at whichever attempt the operator switched to would send their
  // words somewhere they never chose. The ask dock is keyed by run id for the same reason: its
  // question is REFED to the attempt, so an unsent one must not follow the operator to another.
  const selectRun = (id: number) => {
    setJump(null);
    setDraft("");
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
        composerId={!raw && tab === "messages" ? MESSAGE_COMPOSER_ID : undefined}
        onBack={onBack}
        onSelectRun={selectRun}
        // The operator's own line into a running agent (`POST /api/v1/runs/{id}/message`) is the
        // rail's Messages tab now, so the header's action takes them there and puts the cursor in
        // it rather than mounting a second composer beside the one in the rail. It also leaves the
        // raw hatch, which does not carry the rail.
        onCompose={() => {
          setRaw(false);
          setTab("messages");
          setFocusMessage(true);
        }}
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
            watch={
              <WatchTabsRail tab={tab} onSelect={setTab}>
                <WatchPanel
                  tab={tab}
                  run={live}
                  inFlight={inFlight}
                  roster={roster}
                  rosterRead={rosterRead}
                  teamsEnabled={teamsEnabled}
                  draft={draft}
                  onDraft={setDraft}
                  focusComposer={focusMessage}
                  onComposerFocused={takeMessageFocus}
                  onOpenMemory={onOpenMemory}
                  onOpenRoom={onOpenRoom}
                />
              </WatchTabsRail>
            }
          />
          {/* §6's "Ask about this run": a room post refed to the run TODAY, which upgrades to the
              answering-manager Answer path when one is served. It needs a room to post into, so
              a daemon with Teams off gets no dock rather than a control that cannot act. */}
          {/* Keyed per attempt, like the Split above it, so switching resets the question — but
              NOT with the bare run id, which is the Split's own key among these same siblings. Two
              siblings sharing a key is a collision React resolves by dropping one of them, which
              left the previous attempt's Split mounted after a switch. */}
          {teamsEnabled ? (
            <AskDock
              key={`ask:${run.id}`}
              run={live}
              who={who}
              roster={roster}
              onOpenRoom={onOpenRoom}
            />
          ) : null}
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
  composerId,
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
  /** The rail composer's element id while it is actually on screen; undefined otherwise. */
  composerId: string | undefined;
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
        composerId={composerId}
        onCompose={onCompose}
      />
    </div>
  );
}

function HeaderActions({
  run,
  ticketHref,
  inFlight,
  composerId,
  onCompose,
}: {
  run: RunSummary;
  ticketHref: string;
  inFlight: boolean;
  composerId: string | undefined;
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
          rest of this cluster: there is no missing endpoint, there is no agent left to read it.
          (The Messages TAB still opens on a finished run — the timeline of what was sent, and what
          expired undelivered, is history worth reading; only the send is impossible.) */}
      {inFlight ? (
        <Button
          variant="sec"
          // Named only while the rail's composer is actually on screen: `aria-controls` pointing at
          // an id that is not in the document is a dangling reference, not a hint.
          aria-controls={composerId}
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
  watch,
}: {
  phases: readonly TracePhase[];
  /** The teammate this attempt is attributed to; "" when none resolves. */
  who: string;
  pending: boolean;
  /** Whether this attempt is still streaming — what turns the spine into a playhead (§3C). */
  live: boolean;
  batons: RelayBatons;
  jump: { step: FailingStep; nonce: number } | null;
  /** §3C's watch-tabs rail, which sits under the inspector in the same column. */
  watch: ReactNode;
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
      {/* The inspector and the rail share the right column: the rail is what the SELECTED frame
          is watched against, so it sits under it rather than beside the spine. */}
      <div className="trright">
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
        {watch}
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
  //
  // Be warned before simplifying this away on a green suite: NO test holds the ref in place. Swap
  // it for the `atBottom` state in the growth effect and every test still passes, because React
  // happens to flush the listener's `setAtBottom` before the growth render in each sequence they
  // cover. The ref is defence against an ordering this file does not currently produce, not
  // against one it does — which is exactly why the next reader will think it is redundant.
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


// --- (C, continued) the watch-tabs rail (§3C, slice 4) --------------------------------------

/** The panel's element id, so every tab in the rail can name what it controls. */
const WATCH_PANEL_ID = "trwatch-panel";

/**
 * The rail: five tabs under the inspector, and the one panel they switch between.
 *
 * Only the SELECTED panel is mounted, which is what keeps the rail's cost honest — a run detail
 * that polled the room, the reviews and the message list all at once, for four surfaces nobody was
 * looking at, would be four background requests per operator per tick. The state a panel must not
 * lose across a switch (the composer's draft) is held by `RunTrace` above it for exactly that
 * reason.
 */
function WatchTabsRail({
  tab,
  onSelect,
  children,
}: {
  tab: WatchTabId;
  onSelect: (tab: WatchTabId) => void;
  children: ReactNode;
}) {
  return (
    <div className="trwatch">
      {/* The ARIA roles below are a promise about the keyboard as much as about the screen
          reader, and `shell/tabs` is the repo's own answer to it — the same wire-up the Settings
          rail uses, so the two tablists behave alike. */}
      <div
        className="tabs"
        role="tablist"
        aria-label="Watch"
        onKeyDown={(e) =>
          handleTablistKeyDown(
            e,
            WATCH_TABS.map((t) => t.id),
            tab,
            onSelect,
            "horizontal",
          )
        }
      >
        {WATCH_TABS.map((t) => (
          <button
            key={t.id}
            type="button"
            role="tab"
            id={`trtab-${t.id}`}
            className={t.id === tab ? "tab on" : "tab"}
            aria-selected={t.id === tab}
            aria-controls={WATCH_PANEL_ID}
            onClick={() => onSelect(t.id)}
          >
            {t.label}
            {/* On the TAB, not only in the panel: §5's deferred surfaces should be legible without
                opening them, exactly as the header's dependency actions are. */}
            {t.dependency ? <span className="dep">dep</span> : null}
          </button>
        ))}
      </div>
      <div className="tabbody" role="tabpanel" id={WATCH_PANEL_ID} aria-labelledby={`trtab-${tab}`}>
        {children}
      </div>
    </div>
  );
}

function WatchPanel({
  tab,
  run,
  inFlight,
  roster,
  rosterRead,
  teamsEnabled,
  draft,
  onDraft,
  focusComposer,
  onComposerFocused,
  onOpenMemory,
  onOpenRoom,
}: {
  tab: WatchTabId;
  run: RunSummary;
  inFlight: boolean;
  roster: readonly string[];
  rosterRead: QueryState;
  teamsEnabled: boolean;
  draft: string;
  onDraft: (text: string) => void;
  focusComposer: boolean;
  onComposerFocused: () => void;
  onOpenMemory: () => void;
  onOpenRoom: () => void;
}) {
  switch (tab) {
    case "diff":
      return <DiffPanel run={run} />;
    case "review":
      return <ReviewPanel run={run} roster={roster} teamsEnabled={teamsEnabled} />;
    case "memory":
      return (
        <TeamsPanel
          teamsEnabled={teamsEnabled}
          what="this ticket's runs retained no memory to show"
        >
          <MemoryPanel
            issue={run.issue_identifier}
            roster={roster}
            rosterRead={rosterRead}
            onOpenMemory={onOpenMemory}
          />
        </TeamsPanel>
      );
    case "messages":
      return (
        <MessagesPanel
          runId={run.id}
          live={inFlight}
          draft={draft}
          onDraft={onDraft}
          focus={focusComposer}
          onFocused={onComposerFocused}
        />
      );
    default:
      return (
        <TeamsPanel teamsEnabled={teamsEnabled} what="there is no room for anyone to post in">
          <RoomPanel issue={run.issue_identifier} roster={roster} onOpenRoom={onOpenRoom} />
        </TeamsPanel>
      );
  }
}

/**
 * What a panel with no rows should say.
 *
 * `isPending` alone is not the question. A settled react-query ERROR is not pending, so branching
 * on it renders the empty copy as a statement of fact about a read that never landed — and the
 * Messages tab's version of that ("No message has been sent to this run's agent") is one an
 * operator answers by sending the same message twice. A failure says so, and says it is a failure
 * to READ rather than an absence.
 */
function emptyNote(query: QueryState, loading: string, none: string) {
  if (query.isError) return "This could not be read from the daemon — the request failed.";
  return query.isPending ? loading : none;
}

/** How far a read has got. The two flags react-query settles on, and the shape [`emptyNote`] asks
 *  for — so a read composed of SEVERAL requests can report itself as one. */
interface QueryState {
  isPending: boolean;
  isError: boolean;
}

/** Two reads as one: still loading if either is, failed if either did. */
function bothReads(a: QueryState, b: QueryState): QueryState {
  return { isPending: a.isPending || b.isPending, isError: a.isError || b.isError };
}

/**
 * A panel whose whole content comes from `/api/v1/teams*`.
 *
 * With Teams off the app makes no Teams request at all (`useTeamsEnabled` is THE gate), so the tab
 * says which feature would fill it rather than sitting empty or, worse, fetching anyway. The
 * children are not mounted, so no gated hook runs.
 */
function TeamsPanel({
  teamsEnabled,
  what,
  children,
}: {
  teamsEnabled: boolean;
  /** A whole clause, not a noun to be joined onto one — see the test that reads the sentence. */
  what: string;
  children: ReactNode;
}) {
  if (teamsEnabled) return <>{children}</>;
  return <div className="trdep">Teams is off on this daemon, so {what}.</div>;
}

/**
 * The Diff tab — a dependency, and deliberately nothing else (design record §5, §9 slice 7).
 *
 * No endpoint serves a run-branch unified diff, so there is no diff to show and none is invented:
 * the panel names the dependency and deep-links to where the change actually is. The link is the
 * same head-branch SEARCH the header's "View PR" uses, so it can never point at a pull request the
 * console guessed at.
 */
function DiffPanel({ run }: { run: RunSummary }) {
  const prHref = prSearchUrl(run);
  const branch = runBranch(run);
  return (
    <div className="trdep">
      <b>The diff is a dependency.</b> A run-branch unified diff needs a daemon endpoint, deferred
      to slice 7 of the Trace plan. Until it exists this panel shows no diff rather than a
      reconstructed one.
      {branch === "" ? null : (
        <div className="row">
          <Mono>{branch}</Mono>
        </div>
      )}
      <div className="row">
        {prHref === "" ? (
          <span className="note">
            This run's remote is not on github.com, so there is no pull request to link to either.
          </span>
        ) : (
          <a href={prHref} target="_blank" rel="noreferrer noopener">
            Open this branch's pull request ↗
          </a>
        )}
      </div>
    </div>
  );
}

/**
 * The Review tab — who is reviewing this run's work, and how far they have got.
 *
 * What IS served is the ticketless watch set (`GET /api/v1/reviews`): one row per (pull request,
 * reviewer) with a status. What is NOT served anywhere is a structured VERDICT — the findings are
 * posted on the pull request by the reviewing agent, and no endpoint carries them back. So the
 * reviewer and the status are real, and the verdict is dependency-named with a link to where it
 * was actually written, per §5's "never fake".
 */
function ReviewPanel({
  run,
  roster,
  teamsEnabled,
}: {
  run: RunSummary;
  roster: readonly string[];
  teamsEnabled: boolean;
}) {
  // Gated on Teams like every other `/api/v1/teams*`-adjacent surface, and — because this panel is
  // mounted only while its tab is showing — it polls the watch set only while someone is reading.
  const reviews = useReviews(teamsEnabled);
  const jobs = reviews.data?.reviews ?? [];
  const rows = useMemo(() => reviewsForRun(jobs, run).map(reviewRow), [jobs, run]);

  if (!teamsEnabled) {
    return <div className="trdep">Teams is off on this daemon, so no reviewer is assigned.</div>;
  }
  // `enabled: false` is the daemon's own answer, not an error: Teams is off, or the review mode is
  // not `ticketless`. Either way nothing is watching this run's pull request, and saying so is
  // more use than an empty list that reads as "no reviewer yet".
  if (reviews.data?.enabled === false) {
    return (
      <div className="trdep">
        Ticketless review is not enabled on this daemon, so no reviewer is watching this run's pull
        request.
      </div>
    );
  }
  if (rows.length === 0) {
    return (
      <div className="empty">
        {emptyNote(
          reviews,
          "Loading reviews…",
          "No review has been requested for this run's work yet.",
        )}
      </div>
    );
  }
  return (
    <>
      <div className="trrev">
        {rows.map((row) => (
          <div className="rev" key={row.key}>
            <span className="who2" style={{ color: teammateColor(roster, row.job.reviewer) }}>
              <TeammateAvatar color={teammateColor(roster, row.job.reviewer)} size={7} />
              {row.job.reviewer}
            </span>
            <Pill variant={row.variant}>{row.label}</Pill>
            <a className="pr" href={row.url} target="_blank" rel="noreferrer noopener">
              {row.pr} ↗
            </a>
            {row.reviewedShort === "" ? null : <Mono>read {row.reviewedShort}</Mono>}
          </div>
        ))}
      </div>
      {/* The one part of a review nothing serves back. Said once, under the rows, rather than
          dressed up as a verdict the console does not have. */}
      <div className="trdep">
        <b>The verdict itself is a dependency.</b> A reviewer posts its findings on the pull
        request; no endpoint serves them back, so this panel reports who is reviewing and how far
        they have got, and never a verdict it did not read.
      </div>
    </>
  );
}

/**
 * "Room · this ticket" (§3C) — the room posts that reference this run's ticket.
 *
 * The read asks for the daemon's widest window ([`ROOM_WATCH_WINDOW`]) and is a window even so, so
 * the empty copy comes from [`roomEmptyNote`], which states what was READ rather than what the
 * room contains, and the panel offers the room itself as the way past its own bound. A by-ticket
 * room read is a DAEMON change (STUDIO-759) and deliberately not attempted here.
 */
function RoomPanel({
  issue,
  roster,
  onOpenRoom,
}: {
  issue: string;
  roster: readonly string[];
  onOpenRoom: () => void;
}) {
  const room = useTeamsRoom(true, ROOM_WATCH_WINDOW);
  const messages = useMemo(() => room.data?.messages ?? [], [room.data]);
  const posts = useMemo(() => roomPostsFor(messages, issue), [messages, issue]);
  return (
    <>
      <div className="memprev">
        {posts.map((post) => (
          <div className="mcard" key={post.id}>
            <div className="top">
              <span
                className="who2"
                style={{
                  color:
                    post.from === "operator" ? "var(--operator)" : teammateColor(roster, post.from),
                }}
              >
                {post.from}
              </span>
              <Timestamp>{clockTime(post.at)}</Timestamp>
            </div>
            <Markdown source={post.body} />
          </div>
        ))}
        {posts.length === 0 ? (
          <div className="empty">
            {emptyNote(room, "Loading room…", roomEmptyNote(messages.length))}
          </div>
        ) : null}
      </div>
      <div className="trwatchfoot">
        <a
          className="link"
          href="#teams"
          onClick={(e) => {
            e.preventDefault();
            onOpenRoom();
          }}
        >
          Open the room →
        </a>
      </div>
    </>
  );
}

/** "Memory from this ticket" (§3C) — the facts this ticket's runs retained. */
function MemoryPanel({
  issue,
  roster,
  rosterRead,
  onOpenMemory,
}: {
  issue: string;
  roster: readonly string[];
  /** The roster fetch this recall depends on: with no roster there is no bank to read. */
  rosterRead: QueryState;
  onOpenMemory: () => void;
}) {
  const facts = useTicketFacts(roster, issue);
  // The recall and the roster it was derived FROM, reported as the single read they are. Without
  // this an unresolved or failed roster reads as a settled, empty, successful recall — which is
  // the panel stating "no facts were retained" about banks it never learned the names of.
  const read = bothReads(facts, rosterRead);
  return (
    <>
      <div className="memprev">
        {facts.data.map((fact: TeamsFact) => (
          <div className="mcard" key={`${fact.identity}:${fact.id}`}>
            <div className="top">
              <TicketChip variant="sha">
                {fact.run_id === "" ? fact.id : `run ${fact.run_id}`}
              </TicketChip>
              <Timestamp>{fact.identity}</Timestamp>
            </div>
            <Markdown source={fact.content} />
          </div>
        ))}
        {facts.data.length === 0 ? (
          <div className="empty">
            {emptyNote(read, "Loading memory…", MEMORY_EMPTY_NOTE)}
          </div>
        ) : null}
      </div>
      <div className="trwatchfoot">
        <a
          className="link"
          href="#memory"
          onClick={(e) => {
            e.preventDefault();
            onOpenMemory();
          }}
        >
          Open Memory →
        </a>
      </div>
    </>
  );
}

/** The composer's element id, so the header's Message action can name where it sent the cursor. */
const MESSAGE_COMPOSER_ID = "trmsg";

/**
 * The Messages tab — the operator's line into a run's agent, both halves (INF-250).
 *
 * The LIST (`GET /api/v1/runs/{id}/messages`) is history and is shown for every run: what was
 * sent, what the agent actually picked up and on which turn, and what expired because the run
 * ended first. It rides the same 2s in-flight cadence the rest of the view does, so a
 * sent→delivered flip shows up without a reload, and freezes when the run does.
 *
 * The COMPOSER (`POST /api/v1/runs/{id}/message`) can only reach a LIVE run — a finished one has
 * no agent left to read it — so on a finished run it stays visible and refuses, rather than
 * vanishing and taking a half-written instruction with it. The console has no toast surface, so a
 * refusal (the daemon caps pending messages per run) reports here or nowhere.
 */
function MessagesPanel({
  runId,
  live,
  draft,
  onDraft,
  focus,
  onFocused,
}: {
  runId: number;
  live: boolean;
  draft: string;
  onDraft: (text: string) => void;
  /** Whether the header's Message action is waiting for the cursor to land in the composer. */
  focus: boolean;
  /** Consumes that request, so a later plain tab click does not re-steal the focus. */
  onFocused: () => void;
}) {
  const messages = useRunMessages(runId, live);
  const send = useSendRunMessage(runId);
  const [problem, setProblem] = useState("");
  const box = useRef<HTMLTextAreaElement | null>(null);
  useEffect(() => {
    if (!focus) return;
    box.current?.focus();
    onFocused();
  }, [focus, onFocused]);

  const submit = () => {
    const body = draft.trim();
    // An empty send is not an error to report — it is nothing to say. The daemon would reject it
    // anyway, and spending a request to be told so is worse than not making one. Nor is there
    // anything to send to once the run has ended.
    if (!live || body === "" || send.isPending) return;
    setProblem("");
    send.mutate(body, {
      onSuccess: () => onDraft(""),
      onError: (err) => setProblem(err.message),
    });
  };

  const rows = messages.data ?? [];
  return (
    <>
      {/* Oldest first — the served order (`ORDER BY id`), and the one a conversation reads in,
          with the composer that continues it at the bottom. */}
      <div className="trmsgs">
        {rows.map((message) => {
          const chip = messageChip(message);
          return (
            <div className="msg" key={message.id}>
              <div className="body">{message.body}</div>
              <span className={`chip ${chip.tone}`}>{chip.label}</span>
            </div>
          );
        })}
        {rows.length === 0 ? (
          <div className="empty">
            {emptyNote(
              messages,
              "Loading messages…",
              "No message has been sent to this run's agent.",
            )}
          </div>
        ) : null}
      </div>
      <div className="trmsg" id={MESSAGE_COMPOSER_ID}>
        <textarea
          ref={box}
          aria-label="Message the running agent"
          placeholder="The agent picks this up at its next step…"
          maxLength={4000}
          rows={2}
          value={draft}
          disabled={send.isPending}
          onChange={(e) => onDraft(e.target.value)}
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
        </div>
      </div>
    </>
  );
}

/**
 * "Ask about this run" (design record §6, §8) — a room post, refed to the run, and the manager's
 * reply to it read back inline (STUDIO-733, `~/.rhapsody/docs/answering-manager-design.md` §9.5
 * slice 5).
 *
 * The design record for the dock said "ship as a room post refed to the run now; upgrade to the
 * answering-manager Answer path when it lands". It has landed (STUDIO-729→732), and this is that
 * upgrade — but NOT in the shape the phrase suggests. There is still no answer route on `/api/v1`
 * and this slice adds none: the manager answers ONCE, in the room, and the console reads that one
 * post back. Everything the operator sees here is the room's own record, so the room stays the
 * single log and the console is a window onto it rather than a second answer engine.
 *
 * `refs` carries the ticket AND the run (`askRefs`), which is what makes it a question about this
 * attempt rather than about the ticket in general — and the id the daemon echoes back for the post
 * is what [`managerReply`] then matches the manager's reply on.
 */
function AskDock({
  run,
  who,
  roster,
  onOpenRoom,
}: {
  run: RunSummary;
  who: string;
  roster: readonly string[];
  onOpenRoom: () => void;
}) {
  const post = usePostToRoom();
  const [question, setQuestion] = useState("");
  const [problem, setProblem] = useState("");
  // The question that LANDED, as the daemon echoed it back — never the text in the box. It is what
  // the exchange below names and what its reply is matched on, so the two can never disagree.
  const [asked, setAsked] = useState<AskedQuestion | null>(null);
  const submit = () => {
    const body = question.trim();
    if (body === "" || post.isPending) return;
    setProblem("");
    post.mutate(
      { body, refs: askRefs(run) },
      {
        onSuccess: (echo) => {
          setQuestion("");
          setAsked({ id: echo.id, body });
        },
        onError: (err) => setProblem(err.message),
      },
    );
  };
  return (
    <div className="askwrap">
      {/* Not gated on `problem`, unlike the receipt it replaces. A refusal here belongs to the
          question being sent NOW; the card is about one that already landed, and dropping a real
          answer off the screen because a later attempt was refused would lose the operator
          something true in order to report something else. The two say different things in
          different places, and the error sits with the box that produced it. */}
      {asked === null ? null : (
        <AskExchange asked={asked} roster={roster} onOpenRoom={onOpenRoom} />
      )}
      <div className="askdock">
        <span className="g" aria-hidden="true">
          ✦
        </span>
        <input
          className="q"
          aria-label="Ask about this run"
          placeholder={
            who === ""
              ? "Ask the team about this run — it posts to the room…"
              : `Ask the team about ${who}'s run — it posts to the room…`
          }
          value={question}
          disabled={post.isPending}
          onChange={(e) => setQuestion(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && !e.nativeEvent.isComposing) {
              e.preventDefault();
              submit();
            }
          }}
        />
        {problem === "" ? null : (
          <span className="acterr" role="status">
            {problem}
          </span>
        )}
        <Button onClick={submit} disabled={post.isPending}>
          Ask
        </Button>
      </div>
    </div>
  );
}

/**
 * One question the dock posted, and what the room says about it — "you asked X / @manager answered
 * Y", against the one room post the manager wrote.
 *
 * It survives the operator typing the next question, which the bare "Posted to the room" receipt it
 * replaces deliberately did not. That receipt named no subject, so a lingering one read as a claim
 * about the text now in the box and had to be cleared on the first keystroke. This card quotes the
 * question that LANDED, so it cannot be misread that way — and it must not vanish, because an
 * answer that disappeared the moment the operator started writing a follow-up would be an answer
 * they had to go to the room to read after all, which is the whole thing this slice removes.
 *
 * The read is the Room tab's own: same endpoint, same window, one react-query key, so the two
 * cannot show different rooms and an open Room tab costs no second request. It is gated on a
 * question having landed — with nothing asked there is nothing to look up, and a dock that polled
 * the room regardless would make every run detail a room reader.
 */
function AskExchange({
  asked,
  roster,
  onOpenRoom,
}: {
  asked: AskedQuestion;
  roster: readonly string[];
  onOpenRoom: () => void;
}) {
  const room = useTeamsRoom(true, ROOM_WATCH_WINDOW);
  const messages = useMemo(() => room.data?.messages ?? [], [room.data]);
  const outcome = useMemo(() => managerReply(messages, asked), [messages, asked]);
  return (
    <div className="askex">
      <div className="qq">
        <span className="lbl">You asked</span>
        {/* The operator's own words, as text: this half is a receipt for what was sent, and
            rendering it as markdown would show something other than what went into the room. */}
        <span className="qb">{asked.body}</span>
      </div>
      {/* The live region wraps BOTH branches rather than sitting on the pending note, because the
          announcement that matters is the ANSWER arriving. A `role="status"` on the note alone is
          announced when the wait starts and then goes silent at the one moment it should speak:
          the note is REPLACED by the card, and a region that has unmounted announces nothing. */}
      <div className="askans" role="status">
        {outcome.kind === "answered" ? (
          <div className="mcard">
            <div className="top">
              {/* The room's own colour for this identity — `@manager` is not on the roster
                  (`RESERVED_IDENTITIES` keeps it off), so this resolves to the unknown-teammate
                  colour. That is the point: the Room tab resolves it exactly the same way, and one
                  identity must not wear two colours across two views of one post. */}
              <span className="who2" style={{ color: teammateColor(roster, outcome.reply.from) }}>
                {outcome.reply.from}
              </span>
              <Timestamp>{clockTime(outcome.reply.at)}</Timestamp>
            </div>
            {/* The room post itself, through the room's own renderer — the manager's prose arrives
                quoted line by line and its records under `From my own records —`, and that layout
                is what tells the operator which half the daemon vouches for. Reshaping it here
                would make this a second answer wearing the first one's name. A body the room read
                had to cut carries the `…` `truncate_bytes` leaves on it, so this surface is
                exactly as honest about its own bound as the room is. */}
            <Markdown source={outcome.reply.body} />
          </div>
        ) : (
          <div className="pending">
            {/* Never "answering" or "still thinking": nothing tells this page the manager has even
                read the question, and `past-window` cannot tell whether it replied at all. */}
            {emptyNote(
              room,
              "Posted to the room — reading it back…",
              outcome.kind === "waiting" ? ASK_WAITING_NOTE : ASK_PAST_WINDOW_NOTE,
            )}{" "}
            <a
              className="link"
              href="#teams"
              onClick={(e) => {
                e.preventDefault();
                onOpenRoom();
              }}
            >
              Open the room →
            </a>
          </div>
        )}
      </div>
    </div>
  );
}
