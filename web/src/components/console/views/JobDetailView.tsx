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
  ExternalLink,
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
  useRunIdentityEvents,
  useRunMessages,
  useTranscript,
} from "@/hooks/useRunDetail";
import { useLinearIdentity } from "@/hooks/useConfig";
import {
  useMergeRun,
  useResumeRun,
  useRunMergeability,
  useSendRunMessage,
  useStopRun,
} from "@/hooks/useRunActions";
import { useReviews } from "@/hooks/useReviews";
import { useRunDiff } from "@/hooks/useRunDiff";
import { usePostToRoom, useTeamsEnabled, useTeamsOverview, useTeamsRoom } from "@/hooks/useTeams";
import { useTicketFacts } from "@/hooks/useTicketFacts";
import { ticketAssignees } from "@/lib/console-jobs";
import { clockTime, runOutcomeLabel, runOutcomePill, runsNewestFirst } from "@/lib/console-job-detail";
import { checksSummary, diffFiles, diffStat } from "@/lib/console-diff";
import { mergeStateNote, ungatedMergeStateNote } from "@/lib/console-merge";
import { formatDateTime } from "@/lib/format";
import { isAtBottom } from "@/lib/follow-scroll";
import {
  OUTCOME_RUNNING,
  TRACE_FILTERS,
  TRACE_FILTER_LABELS,
  attemptBucket,
  attemptOptions,
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
  type AttemptOption,
  type Baton,
  type FailingStep,
  type RelayBatons,
  type RunVitals,
  type TraceFilter,
} from "@/lib/console-trace-view";
import {
  ASK_READING_NOTE,
  askNote,
  managerReply,
  type AskedQuestion,
  type AskOutcome,
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
import { runIdentities } from "@/lib/run-identity";
import {
  baseToolName,
  buildResult,
  buildTrace,
  type DidCard,
  type PhaseKind,
  type ResultCard,
  type SaidBlock,
  type TracePhase,
} from "@/lib/trace-model";
import type {
  LogEntry,
  MergeReceipt,
  RunSummary,
  TeamsFact,
  TeamsRoomMessage,
} from "@/lib/api";
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
// Slice 4 (STUDIO-745) adds §3C's watch-tabs rail — Diff / Review / Room / Memory / Messages,
// promoted by STUDIO-766 out of the inspector's column into its own full-width zone below the
// Split — which is where the §4 side cards that used to sit in their own row below the trace now
// live, joined by the run's operator-message timeline and an "Ask about this run" dock that posts
// to the room refed to the run (§6).
//
// Slice 5 (STUDIO-746) wires the DURABLE per-run identity through all of it: the header keeps a
// finished run's assignee once its teammate has left the live roster, the spine signs the steps
// that changed something the team can see, and the baton names each attempt's own teammate so a
// multi-agent ticket reads as a real relay (§3A/§3C/§6). Its source is `lib/run-identity`.
//
// Slice 5's sibling (STUDIO-767) makes the header's Merge a REAL action: the operator clicks it,
// the daemon resolves the run's own pull request and merges it. The console never names a pull
// request — see `handlers_runmerge` for why that absence is the guardrail — so all this sends is
// the run id and, on the second leg of the confirm handshake, the head SHA the daemon just showed
// it (design record `~/.rhapsody/docs/STUDIO-767-console-merge-action.md` §3/G1, §3/G3).
//
// The model behind all three zones is `lib/trace-model` (slice 1) and `lib/console-trace-view`;
// the rail's own is `lib/console-watch`. Nothing here re-derives them. What no endpoint serves is
// still not invented: there is no PR number (§5), so "View PR" resolves through a head-branch
// search, and the Diff tab is a dependency card with a deep link rather than a diff nobody served.

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

  // The ticket's DURABLE routing rows — who each attempt was dispatched as (STUDIO-746). Fetched
  // beside the attempt list because it answers the same question that list does, and because it is
  // the whole ticket's answer: the baton needs the NEIGHBOURING attempt's teammate, not only the
  // one being read.
  //
  // Deliberately NOT gated on `teamsEnabled`, unlike every other Teams read on this page. That
  // flag is the daemon's CURRENT one, and these rows were written when the runs happened: a
  // daemon that ran a team and has since been switched to Teams-off still holds every one of
  // them, so gating would blank the attribution on exactly the history this slice exists to keep.
  // The cost of being wrong the other way is two sub-millisecond searches answering "no rows".
  const identityEvents = useRunIdentityEvents(issue);
  const identities = useMemo(() => runIdentities(identityEvents.data ?? []), [identityEvents.data]);
  // The search's own load state travels WITH its map, the way `rosterRead` travels with the roster
  // — because an empty map is not one answer but three. "Nothing fetched yet", "the fetch failed"
  // and "this ticket has no routing row" are indistinguishable in the map alone, and only the last
  // of them is the documented live-roster fallback.
  const identityRead = { isPending: identityEvents.isPending, isError: identityEvents.isError };

  const runs = useMemo(() => runsNewestFirst(history.data?.runs ?? []), [history.data]);
  // The attempt the zones render. `null` follows the newest run, so a ticket that gains a run
  // while the page is open moves with it; picking an attempt pins the choice.
  const [pinned, setPinned] = useState<number | null>(null);
  const run = runs.find((r) => r.id === pinned) ?? runs[0];
  // The live roster, which only ever names a RUNNING ticket — the gap-filler behind the durable
  // record for a run whose ledger has no routing row at all (`runTeammate`).
  //
  // Withheld until the durable read has SUCCEEDED, because "no routing row" is the ONLY state it
  // answers for. Handed an in-flight or failed search it answers for those too, and then a
  // completed, unrouted run whose ticket has since been re-dispatched is captioned with whoever
  // holds it right now — avatar, ramp colour and all — for a run its own ledger says had nobody.
  // That is the same fabrication `fetchRunIdentityEvents` refuses `allSettled` to avoid, one layer
  // up: no answer YET is not "no record", and a search that failed never became one either.
  const identityKnown = !identityRead.isPending && !identityRead.isError;
  const assignee = identityKnown ? (ticketAssignees(overview.data).get(issue) ?? "") : "";
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
          identities={identities}
          identityRead={identityRead}
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
  identities,
  identityRead,
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
  /** Run id → the teammate that run was dispatched as; see `lib/run-identity` for the tri-state. */
  identities: ReadonlyMap<number, string>;
  /** How that map's own fetch is going — an empty map means nothing without it. */
  identityRead: QueryState;
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
  const batons = useMemo(
    () => relayBatons(runs, run, identities, assignee),
    [runs, run, identities, assignee],
  );
  // The selector's labels, resolved from the same three inputs the batons are — so an option, the
  // baton beside it and the header assignee can never name different teammates for one attempt.
  const attempts = useMemo(
    () => attemptOptions(runs, identities, assignee),
    [runs, identities, assignee],
  );
  // Resolved ONCE, so the header's avatar, the spine's signed steps and the inspector's
  // "what <who> did" can never disagree about whose run this is — they did while only the header
  // knew about a review key.
  const who = runTeammate(run, identities, assignee);
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
        attempts={attempts}
        who={who}
        resolvingWho={identityRead.isPending}
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
            roster={roster}
            pending={transcript.isPending}
            live={inFlight}
            batons={batons}
            jump={jump}
          />
          {/* Zone D (STUDIO-766): the rail is not step-scoped — the same five surfaces whichever
              step the spine has selected — so it is a full-width sibling of the Split, on the
              Result card's border, radius and rhythm, rather than a strip welded under the
              STEP-scoped inspector inside the Split's right column. There it read as though it
              should change per step. (What the five ARE scoped to is stated by the rail's own
              eyebrow, and is not one single scope; see `WatchTabsRail`.)
              Keyed per attempt, as it was implicitly while it rode inside the Split's key: the
              mounted panel keeps state of its own (the Messages composer's send error), and a
              failure reported against the attempt being LEFT must not follow the operator to the
              one they switched to. The key is its own — two siblings sharing one is a collision
              React resolves by dropping a sibling, which is why AskDock's is prefixed too. */}
          <WatchTabsRail key={`watch:${run.id}`} tab={tab} onSelect={setTab}>
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
  attempts,
  who,
  resolvingWho,
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
  /** Every attempt the ticket has, newest first, already labelled — see `attemptOptions`. */
  attempts: readonly AttemptOption[];
  /** The teammate this attempt is attributed to; "" when none resolves. */
  who: string;
  /** Whether the durable routing search may still name one — see the assignee slot below. */
  resolvingWho: boolean;
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
    // `data-attempts` is for the stylesheet, not for anyone reading the DOM: the width one header
    // row costs grows with the attempt selector, so `console-trace.css` sets the single-row
    // breakpoint per bucket rather than once for every ticket. See `attemptBucket`.
    <div className="trhd" data-attempts={attemptBucket(attempts.length)} ref={ref}>
      <button type="button" className="back" aria-label="Back to Jobs" onClick={onBack}>
        ‹
      </button>
      <div className="idw">
        <div className="k">{run.issue_identifier}</div>
        <h1>{run.title === "" ? run.issue_identifier : run.title}</h1>
      </div>
      {/* The persistent assignee (§3A). A run nothing can name keeps the slot and reads "—":
          the header's job is to say who ran this, and an omitted element says nothing at all —
          which is indistinguishable from a layout that forgot to render it.

          While the durable search is still in flight the slot is kept but says NOTHING, for the
          same reason the Result card's headline waits: "—" is an assertion that this run had
          nobody, and it would be a wrong one for every routed run. Only a search that has
          answered gets to fill the slot. A ticketless review run is named by its own key and
          never reaches this. */}
      {who !== "" ? (
        <span className="who2">
          {/* The prototype's `.who` — a 20px avatar carrying the initial, then the name (§3A). */}
          <TeammateAvatar color={teammateColor(roster, who)} size={20} name={who} />
          {who}
        </span>
      ) : resolvingWho ? (
        <span className="whoskel" role="status">
          <span className="vh">Resolving who ran this…</span>
        </span>
      ) : (
        <span className="who2 none">—</span>
      )}
      {/* The outcome pill, in the prototype's own vocabulary rather than the daemon's column
          value — "done", not "completed" (§3A). `runOutcomeLabel` translates only the three
          states the prototype names and passes anything else through.

          It also carries the run's id and start time, which are the two facts NOTHING else on this
          page renders: the route is `#job/<TICKET-KEY>`, the receipt carries neither, and the id
          is the daemon's own handle — what `/api/v1/runs/{id}`, `symphony_run_status` and the logs
          are keyed by. They used to live only in the attempt selector's tooltip, and the
          single-row header sheds that selector on a one-attempt ticket at the desktop default
          width (see `console-trace.css`), which would have taken both with it.

          The pill is where they belong rather than merely where they fit: it is the one header
          member that describes this RUN and not the ticket, so "which run, and when did it start"
          is the same question its state answers. It is never shed and `flex: none` in the single
          row, so the hover target exists at every width. Unconditional, too — the width the
          selector goes at lives in the stylesheet, and a copy of that breakpoint in TSX would be a
          second source of truth for it, silently stale the next time the row is re-measured. */}
      <Pill
        variant={runOutcomePill(run.outcome)}
        title={`run ${run.id} · started ${formatDateTime(run.started_at)}`}
      >
        {runOutcomeLabel(run.outcome)}
      </Pill>
      {/* The live pulse (§3A). Decorative beside the outcome pill, which already says "running"
          in words — a screen reader that announced a second "live" would only repeat it. */}
      {inFlight ? <span className="trpulse" aria-hidden="true" /> : null}
      {/* The attempt selector — the implement→revise relay. Switching swaps the Result card, the
          spine and the header's assignee to that run, and the spine draws the handoff baton
          either side of it (`relayBatons`), naming each attempt's own teammate.

          Labelled "attempt N · <teammate>", as the prototype's `.hd` labels it (STUDIO-763), from
          the durable per-run identity STUDIO-746 wired. `attemptOptions` owns both halves — see it
          for why the ordinal is the ticket's own run order rather than `runs.attempt`, and for the
          two degradations when nothing can name an attempt. The run id is the daemon's own handle
          and the ordinal is not, so it rides along in the tooltip with the start time. */}
      <Seg
        className="trattempts"
        aria-label="Attempt"
        options={attempts.map((a) => ({
          value: String(a.id),
          label: (
            // The label itself rides in the tooltip too: on a narrow wide-view window a long
            // attempt list is clipped, and the teammate is exactly what must stay reachable. A
            // fallback label already IS the run id, so the tooltip does not repeat it.
            <span
              title={
                a.named
                  ? `${a.label} · run ${a.id} · started ${formatDateTime(a.startedAt)}`
                  : `run ${a.id} · started ${formatDateTime(a.startedAt)}`
              }
            >
              {a.label}
            </span>
          ),
        }))}
        value={String(run.id)}
        onChange={(v) => onSelectRun(Number(v))}
      />
      <div className="trvitals">
        {/* Duration, turns and tokens, grouped because they leave together: all three are repeated
            verbatim in the Result card's receipt ~8px below (§3B), so the single-row header sheds
            them rather than squeezing the branch, which the receipt does NOT carry. The wrapper is
            `display: contents` until it is shed, so grouping them costs the row no layout. */}
        <span className="trdup">
          <span>
            <b>{vitals.duration}</b>
          </span>
          <span>
            <b>{vitals.turns}</b>
          </span>
          <span>
            <b>{vitals.tokens}</b> tokens
          </span>
        </span>
        {/* The one vital the Result card's receipt does NOT repeat, so it is the member the row
            keeps and floors — and it carries itself in a tooltip for a branch long enough to
            ellipsize anyway. */}
        <Mono title={vitals.branch}>{vitals.branch}</Mono>
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
  const merge = useMergeRun(run.id);
  const teamsEnabled = useTeamsEnabled();
  // What the daemon would say if Merge were clicked right now (STUDIO-790). Asked only where a
  // merge path exists at all, because with Teams off the daemon serves `teams_disabled` and the
  // control below is dependency-named without a round trip.
  const verdict = useRunMergeability(run.id, teamsEnabled);
  const prHref = prSearchUrl(run);
  // The receipt the daemon is asking the operator to confirm (STUDIO-767). Held here rather than
  // read off `merge.data`, because confirming re-runs the mutation and the modal must keep showing
  // the SAME pull request while that is in flight.
  const [confirming, setConfirming] = useState<MergeReceipt | null>(null);
  const merged = merge.data?.status === "merged" ? merge.data.receipt : null;
  // The pull request the daemon says a click would act on, or null when it refused, failed, or has
  // not answered yet. It names the merge in the button's tooltip and NOTHING else: rendering the
  // header's merge-state note from it too would put a second, independent sentence beside the
  // control, and the two can disagree — a DIRTY receipt is not refused by the daemon, so the note
  // would read "it cannot land" next to a live primary. The pre-click channel is the daemon's own
  // verdict, on the control itself; GitHub's view of a pull request the operator has NOT acted on
  // yet belongs in the confirm modal, which already carries it (STUDIO-790).
  const resolved = verdict.data?.mergeable === true ? verdict.data.receipt : null;
  // What GitHub says the pull request is waiting on, once one has been armed (STUDIO-784). "" when
  // GitHub stated no merge state, which is a real answer and not a reason to guess at one.
  const mergedNote = merged === null ? "" : mergeStateNote(merged.merge_state, true);
  // The console has no toast surface, so a lifecycle action reports here or nowhere. Both halves
  // matter: the request can fail, and it can succeed while the ticket MOVE fails — a run killed
  // whose ticket stayed put is something the operator has to finish by hand. A refused merge lands
  // here too: `merge_refused` carries the daemon's own reason, and it is the whole of what the
  // operator needs to read.
  const problem =
    stop.error?.message ??
    resume.error?.message ??
    merge.error?.message ??
    stop.data?.move_error ??
    resume.data?.move_error ??
    "";
  // Both legs of the handshake go through here, so the modal opens on a `confirm` answer whichever
  // leg produced it — which is what makes a STALE confirmation (the author pushed in between) show
  // the operator the new head instead of merging code they never saw.
  const askMerge = (confirm: string) =>
    merge.mutate(confirm, {
      onSuccess: (res) => setConfirming(res.status === "confirm" ? res.receipt : null),
    });
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
        <ExternalLink className="btn sec" href={ticketHref}>
          Open ticket
        </ExternalLink>
      )}
      {/* No endpoint serves a PR number (design record §5), so this is a head-branch search on
          the run's own remote — it finds the branch's PR without the console asserting one. The
          branch it searches for comes from `runBranch`, since the daemon writes none. */}
      {prHref === "" ? (
        <DepButton title="No daemon pull-request endpoint, and this run's remote is not on github.com, so there is nothing to search.">
          View PR
        </DepButton>
      ) : (
        <ExternalLink className="btn sec" href={prHref}>
          View PR
        </ExternalLink>
      )}
      {/* The real green primary (design §5), rendered from the daemon's own verdict rather than
          from its own in-flight state (STUDIO-790). Every refusal — no open pull request on the
          branch, a live Rhapsody review round, one that asked for changes, an already-merged or
          closed pull request, a branch behind its base, a ticket routed back out of review — is
          still the DAEMON's to make; the console now READS it before the click instead of learning
          it afterwards, and shows it the way every other unavailable action on this header does.
          The console derives none of them: `reason` is the daemon's sentence, verbatim. */}
      {!teamsEnabled ? (
        <DepButton title="Rhapsody Teams is not enabled on this daemon, so it has no merge path.">
          Merge
        </DepButton>
      ) : verdict.isPending ? (
        // Not a live primary yet: offering one before the answer arrives is the bug this fixes,
        // one render earlier.
        <DepButton title="Asking the daemon whether this run's pull request can be merged…">
          Merge
        </DepButton>
      ) : verdict.data && !verdict.data.mergeable ? (
        <DepButton title={`Rhapsody will not merge this run's pull request: ${verdict.data.reason}.`}>
          Merge
        </DepButton>
      ) : (
        <Button
          variant="pri"
          // Two states share this arm, and the title separates them. A resolved verdict names the
          // pull request the click would act on. A verdict that could not be READ — a `gh` that
          // would not answer — leaves the control live on purpose: nobody could ask is not the
          // daemon saying no, and the click still refuses server-side if the answer is no.
          title={
            resolved
              ? `Merge ${resolved.pr} — ${resolved.method}, with GitHub's own auto-merge.`
              : `The daemon could not be asked whether this can be merged (${verdict.error?.message ?? "no answer"}). Clicking asks again and merges nothing before you confirm.`
          }
          onClick={() => askMerge("")}
          disabled={merge.isPending}
        >
          Merge
        </Button>
      )}
      {merged ? (
        <span className="actok" role="status">
          {/* gh's own words when it gave any — under `--auto` they say "will be automatically
              merged", which is the honest thing. The fallback must not overstate it either: an
              applied `--auto` merge ARMED auto-merge, and the pull request lands only if its
              required checks pass. Same distinction the audit row draws (`attempt_line`). */}
          {merged.said === undefined || merged.said === ""
            ? merged.auto
              ? `queued ${merged.pr} for merge`
              : `merged ${merged.pr}`
            : merged.said}
        </span>
      ) : null}
      {/* What the armed merge is WAITING on (STUDIO-784). Its own element rather than a suffix on
          the line above, because the two say different things: that one reports what the daemon
          did, this one reports where GitHub says the pull request stands — and an armed `--auto`
          merge is otherwise one line followed by silence. Absent when GitHub stated no merge
          state, which is a real answer and not a reason to guess. */}
      {mergedNote === "" ? null : (
        <span className="actnote" role="status">
          {mergedNote}
        </span>
      )}
      {confirming === null ? null : (
        <MergeConfirm
          receipt={confirming}
          busy={merge.isPending}
          // The confirmed leg is the ONLY one the merge itself runs on, so this is where a
          // conflict, a branch behind `main` or a review round that started between the legs
          // arrives. `problem` renders it too, but in `.acts` — underneath this modal's veil.
          error={merge.error?.message ?? ""}
          onConfirm={() => askMerge(confirming.head_sha)}
          onClose={() => setConfirming(null)}
        />
      )}
    </div>
  );
}

/**
 * The merge confirmation (design §3/G3). It renders the receipt the DAEMON resolved — the pull
 * request, its head commit, and how it will be merged — and confirming re-POSTs that head SHA.
 *
 * The confirmation is server-enforced, so this modal is not the guard: a forged POST simply skips
 * it, and the daemon still refuses unless the SHA matches the one it re-resolves. What the modal
 * is for is the other failure mode — an operator merging the wrong thing by clicking.
 */
function MergeConfirm({
  receipt,
  busy,
  error,
  onConfirm,
  onClose,
}: {
  receipt: MergeReceipt;
  busy: boolean;
  error: string;
  onConfirm: () => void;
  onClose: () => void;
}) {
  // `aria-modal` is a CLAIM — it tells a screen reader everything outside this dialog is inert —
  // and it is only true if focus is actually in here. Without this, a keyboard operator's focus is
  // still on the Merge button behind the veil, so Enter re-fires the action the dialog exists to
  // interrupt. Focus moves in on open, is trapped in the cycle, and returns whence it came.
  const box = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const from = document.activeElement as HTMLElement | null;
    box.current?.focus();
    return () => from?.focus?.();
  }, []);
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        onClose();
        return;
      }
      if (e.key !== "Tab") return;
      const stops = box.current?.querySelectorAll<HTMLElement>("a[href], button:not([disabled])");
      if (stops === undefined || stops.length === 0) return;
      const first = stops[0];
      const last = stops[stops.length - 1];
      // Wrap at both ends, and also when focus is somewhere outside the dialog entirely — which
      // is where it sits on the very first Tab, since the dialog box itself holds it to start.
      const outside = !box.current?.contains(document.activeElement);
      const to = e.shiftKey
        ? document.activeElement === first || outside
          ? last
          : null
        : document.activeElement === last || outside
          ? first
          : null;
      if (to !== null) {
        e.preventDefault();
        to.focus();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);
  return (
    <div className="mgveil" role="presentation" onClick={onClose}>
      <div
        className="mgconf"
        role="dialog"
        aria-modal="true"
        aria-label={`Merge ${receipt.pr}`}
        ref={box}
        tabIndex={-1}
        onClick={(e) => e.stopPropagation()}
      >
        <div className="ttl">Merge {receipt.pr}?</div>
        <div className="fx">
          <ExternalLink href={receipt.url}>{receipt.url}</ExternalLink>
          <Mono>
            {receipt.head_sha.slice(0, 12)} · {receipt.method}
            {receipt.auto ? ", auto" : ""}
          </Mono>
        </div>
        <p className="sub">
          {receipt.auto
            ? "GitHub's own auto-merge is armed: the pull request lands only once its required checks pass, and never before."
            : "The pull request is merged immediately."}{" "}
          Confirming merges the commit above — if it has been pushed to since, you will be asked
          again.
        </p>
        {/* Where GitHub says the pull request stands right now, so the operator confirms against
            its real state rather than against the hope of one (STUDIO-784). */}
        {mergeStateNote(receipt.merge_state) === "" ? null : (
          <p className="sub">{mergeStateNote(receipt.merge_state)}</p>
        )}
        {error === "" ? null : (
          <p className="err" role="alert">
            {error}
          </p>
        )}
        <div className="row">
          <Button variant="sec" onClick={onClose}>
            Cancel
          </Button>
          <Button variant="pri" onClick={onConfirm} disabled={busy}>
            {busy ? "Merging…" : "Merge"}
          </Button>
        </div>
      </div>
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
      <div className="trbar" />
      <div className="in">
        <div className="body">
          <div className="eyebrow">
            {/* The prototype's status dot. It is an ELEMENT drawn in CSS rather than a `●` typed
                into the string, so the eyebrow's text stays exactly the outcome label: a glyph in
                there would be read out as part of the accessible name and would break the
                assertion that pins that label. */}
            <span className="trdot" aria-hidden="true" />
            {eyebrow.text}
          </div>
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
                <div className="trsect" key={`${i}:${section.heading}`}>
                  <div className="lab">{section.label}</div>
                  {/* The author's OWN heading beside the model's label: the label is this
                      console's reading of it, and the operator can see what was written. */}
                  <div className="trshd">{section.heading}</div>
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
  roster,
  pending,
  live,
  batons,
  jump,
}: {
  phases: readonly TracePhase[];
  /** The teammate this attempt is attributed to; "" when none resolves. */
  who: string;
  roster: readonly string[];
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
  // The step LIST is the scroller, not the page and not `.trspine` — the filter chips and the grep
  // field are the spine's other children and stay pinned above it (STUDIO-821).
  const stepsRef = useRef<HTMLDivElement | null>(null);
  const { atBottom, jumpToBottom } = useFollowScroll(following, stepsRef);
  const behind = live && (!following || !atBottom);

  // The selection can move without the operator having reached for it — the Result card's "jump to
  // failing step", the playhead carrying a live run's pick forward — and the step it lands on can
  // be anywhere in a list that now scrolls inside its own box. Before the cap the page scrolled and
  // the whole spine was on it; now a step outside the box is simply not on screen.
  //
  // `block: "nearest"` is the whole point: a step already visible is left exactly where it is, so
  // this neither fights the follow pin (which has just put the head at the bottom) nor yanks the
  // list when the operator clicks a step they can already see.
  const selectedId = selected?.id;
  useEffect(() => {
    const list = stepsRef.current;
    if (list === null) return;
    const step = list.querySelector('.trstep[aria-pressed="true"]');
    // jsdom implements no layout and leaves `scrollIntoView` undefined; a missing scroll is not a
    // failure anyone can act on, so it is skipped rather than thrown.
    if (step instanceof HTMLElement && typeof step.scrollIntoView === "function") {
      step.scrollIntoView({ block: "nearest" });
    }
  }, [selectedId]);

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
        <div className="trsteps" ref={stepsRef}>
          {/* The baton this attempt was handed (§3C). It leads the spine because that is when it
              happened: the previous run ended and this one picked the work up. */}
          {batons.incoming === null ? null : <BatonRow baton={batons.incoming} direction="in" />}
          {visible.map((phase) => (
            <SpineStep
              key={phase.id}
              phase={phase}
              who={who}
              roster={roster}
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
      </div>
      {/* The right column holds the inspector alone — everything in the Split is scoped to the
          step the spine has selected. The watch-tabs, which follow no step, are zone D below
          the Split (STUDIO-766). */}
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

function scrollToBottom(el: HTMLElement | null) {
  // Assigning `scrollTop` rather than calling `scrollTo`: it is the one form every engine the
  // console runs in — a browser, the Tauri webview, and jsdom under test — implements alike.
  if (el !== null) el.scrollTop = el.scrollHeight;
}

/** What the follow rule gives the view: where the list is, and the way back to the bottom. */
interface FollowScroll {
  /** Whether the step list is still pinned to the bottom, auto-following what the stream appends. */
  atBottom: boolean;
  /** Take the list back to the bottom and resume following it (the "jump to latest" chip). */
  jumpToBottom: () => void;
}

/**
 * Follow-mode for a live run: reports whether the step list is still pinned to the bottom, and
 * keeps it there as the stream appends.
 *
 * The geometry is `lib/follow-scroll`'s, shared verbatim with the logs follow — one definition of
 * "at the bottom", threshold and all, rather than a second one that drifts. What changed in
 * STUDIO-821 is only WHICH element is measured: the scroller is passed in as `ref` — the spine's
 * capped step list — where it used to be the document itself. `lib/follow-scroll` was already
 * element-agnostic (it takes three numbers), so the logs follow is untouched by this.
 *
 * Where the operator has scrolled to is tracked whether following is on or OFF, because `active`
 * can turn back on: a grep that hides a live run's head stops the follow, and the next poll to
 * bring in a phase the grep MATCHES turns it on again while growing the list in the same commit.
 * Observing the position only while active meant that commit had no reading of its own to go on —
 * the last one was from before the operator scrolled — and the list dragged them to the bottom.
 */
function useFollowScroll(active: boolean, ref: RefObject<HTMLElement | null>): FollowScroll {
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
    const el = ref.current;
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
    // Nothing is read here: the position at mount is whatever the list happens to open at, not a
    // choice the operator made about this run, and `lib/follow-scroll`'s other consumer opens a
    // live view at its tail for exactly that reason. The first scroll event is the first thing
    // that speaks for the operator.
    //
    // The listener is on the ELEMENT, not on `window`: a box that scrolls inside the page fires
    // its scroll events at itself and they do not bubble to the window, so the document-level
    // listener this replaced would have gone permanently silent and follow-mode would have been
    // stuck on whatever it read last.
    el.addEventListener("scroll", read, { passive: true });
    return () => el.removeEventListener("scroll", read);
  }, [ref]);

  // Checked every render, acted on only when the page actually GREW: a poll that appends a step
  // must not push the newest line off the screen, and every other re-render — a keystroke in the
  // grep field, a filter chip — must not yank the page around. An operator who has scrolled up is
  // never dragged back; that is what the jump chip is for.
  const height = useRef(0);
  useEffect(() => {
    const el = ref.current;
    if (el === null) return;
    // No reading is written back here: this branch only runs when the list was ALREADY pinned,
    // and it leaves it pinned.
    if (active && pinned.current && el.scrollHeight > height.current) scrollToBottom(el);
    height.current = el.scrollHeight;
  });

  // The way back, and the one thing that RE-takes the pin: an operator who scrolled up has said
  // they are not following, and only they can say they are again. Assigning the position is not
  // enough on its own — a browser announces a programmatic scroll with a scroll event and jsdom
  // never does, so the reading is set here rather than waited for.
  const jumpToBottom = () => {
    scrollToBottom(ref.current);
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

/**
 * The phases a step is SIGNED with its teammate on (§6, "persistent assignee on the header and
 * every post/handoff step").
 *
 * These two and no others, because these are the steps whose effect leaves the run: a room post and
 * a retained fact are read back into a teammate's later prompts, and a hand-off moves the ticket.
 * Whose they were is part of what they mean. Reading and editing are the agent's own work inside
 * its own worktree — signing every step would just repeat the header on every row.
 */
const SIGNED_PHASES: ReadonlySet<PhaseKind> = new Set<PhaseKind>(["coordinated", "handoff"]);

function SpineStep({
  phase,
  who,
  roster,
  selected,
  playing,
  onSelect,
}: {
  phase: TracePhase;
  /** The teammate this attempt is attributed to; "" when none resolves. */
  who: string;
  roster: readonly string[];
  selected: boolean;
  /** The playhead sits here — the newest phase of a run that is still streaming. */
  playing: boolean;
  onSelect: () => void;
}) {
  // Unattributed rather than guessed: a step signed with a name nothing recorded would put words
  // in a teammate's mouth about a post the room can be read back for.
  const signed = who !== "" && SIGNED_PHASES.has(phase.kind);
  return (
    <button
      type="button"
      // `ph`, not `now`: `.rh-console .now` (console.css) is the Jobs-home banner CARD, equal
      // specificity to `.rh-console .trstep`, so a step marked `now` inherited that card's border,
      // gradient and 16px bottom margin on top of the playhead treatment (STUDIO-763 addendum, the
      // twin of STUDIO-771's jobs-list fix).
      className={`trstep${phase.failed ? " err" : ""}${playing ? " ph" : ""}`}
      aria-pressed={selected}
      onClick={onSelect}
    >
      <span className="g" aria-hidden="true">
        {phaseGlyph(phase.kind)}
      </span>
      <span className="txt">
        <span className="sthd">
          <span className="stt">{phase.title}</span>
          {signed ? (
            <span className="stwho">
              <TeammateAvatar color={teammateColor(roster, who)} size={6} />
              <span className="n">{who}</span>
            </span>
          ) : null}
        </span>
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
 * Zone D: five tabs below the Split, and the one panel they switch between.
 *
 * The five do NOT share one scope: only Messages is scoped to the run (`runId={run.id}`), while
 * Diff, Review, Room and Memory are all scoped to the TICKET, built to the last one on
 * `run.issue_identifier`. What they do have in common is the negative — none of them follows the
 * spine, unlike the inspector above, which is scoped to the step the spine has selected. So the
 * eyebrow states that negation, "Not this step", rather than a positive scope that would be false
 * about part of the rail; the JSX comment on it carries the per-tab audit (STUDIO-766).
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
      {/* What the five tabs are scoped to, STATED rather than left to be inferred from position —
          the whole point of pulling this zone out from under the step-scoped inspector.
          It is a negation because every positive label is a lie about part of the rail: only
          Messages is scoped to the run (`runId={run.id}`). Diff, Review, Room and Memory are all
          scoped to the TICKET — `runBranch`/`prSearchUrl`, `reviewsForRun`, `roomPostsFor` and
          `useTicketFacts` are built on `run.issue_identifier` to the last one — so switching
          attempt leaves those four byte-identical. "This whole run" would therefore promise a
          scope change four of the five tabs never make, which is the same defect this ticket
          exists to remove, moved one zone up. "Not this step" is true of all five, and it draws
          the contrast with zone C that this label is here for. */}
      <div className="eyebrow">Not this step</div>
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
 * The Diff tab — the change a run produced on its branch (STUDIO-749; design record §5, §9 slice
 * 7).
 *
 * §5 deferred this as "the one real new endpoint", and until it existed the panel named its
 * dependency and deep-linked to the pull request rather than reconstructing a diff from a
 * transcript. `GET /api/v1/runs/{id}/diff` now serves one, so the panel renders it.
 *
 * **Three states, and they are deliberately three rather than two.** A diff the daemon SERVED, a
 * daemon that says there is nothing to show (an unpushed branch, a merged-and-closed pull request,
 * a non-GitHub remote — the ordinary life of a ticket, so it keeps the dependency card's calm
 * treatment and adds the deep link, exactly as before), and a daemon that could not be ASKED. The
 * third must not read as the second: "there is no diff" is a statement about the run, and "GitHub
 * would not answer" is not.
 *
 * The console derives none of the reasons. `reason` is the daemon's own sentence, verbatim — the
 * discipline the header's Merge already follows (STUDIO-790).
 *
 * **On the merge-state note, and why it is the UNGATED one.** `console-merge.ts` states a
 * precondition its wording depends on: every value reaching `mergeStateNote` came off a
 * `MergeReceipt`, so it passed every refusal `runmerge::resolve_pull_request` makes. `rundiff`
 * refuses nothing — that is the point of the module — so this panel is the first caller holding a
 * value that passed none of them, and one arm of that helper is only true because of the gate.
 * `BEHIND` promises GitHub updates the branch itself, which `resolve_pull_request` guarantees by
 * REFUSING a behind branch on a repository that will not; ungated, that promise would send the
 * operator to wait for something that never happens, while Merge on the same run tells them to
 * push. So this reads through [`ungatedMergeStateNote`], which says only what is observable
 * without the policy read. The other arms are facts of GitHub's own and are shared, not copied:
 * one sentence per GitHub state, still, rather than a second vocabulary that can drift.
 *
 * Separately — and answering a different objection — `mergeStateNote`'s doc also warns against
 * putting this sentence beside the header's Merge, where it was a second reading of a question
 * the daemon had already answered on that control. That warning is satisfied here because there
 * is no Merge control in this zone to contradict; it is not what the precondition above is about.
 */
function DiffPanel({ run }: { run: RunSummary }) {
  const read = useRunDiff(run.id);
  const answer = read.data;
  const files = useMemo(
    () => (answer?.available ? diffFiles(answer.diff.patch) : []),
    [answer],
  );

  if (read.isPending) {
    return <div className="empty">Reading this run's diff…</div>;
  }
  // A question nobody could answer. Kept apart from "nothing to show" and given the daemon's own
  // complaint, because only the latter is the daemon reporting on this run.
  if (read.isError || !answer) {
    return (
      <div className="trdep">
        <b>The diff could not be read.</b> The daemon could not be asked what this run changed
        {read.error ? `: ${read.error.message}` : ""}. That is not the same as this run having
        changed nothing — reopening this tab asks again.
      </div>
    );
  }
  if (!answer.available) {
    return <NoDiff run={run} reason={answer.reason} />;
  }

  const { diff } = answer;
  const stat = diffStat(files);
  const checks = checksSummary(diff.checks);
  const state = ungatedMergeStateNote(diff.merge_state);
  return (
    <>
      {/* What this diff is OF, before the diff itself. A patch with no coordinate cannot be told
          apart from the same files read a push later, which is what `head_sha` is here for. */}
      <div className="trdiffhead">
        <ExternalLink className="pr" href={diff.url}>
          {diff.pr} ↗
        </ExternalLink>
        <Mono>{diff.head_sha.slice(0, 7)}</Mono>
        <span className="stat">
          {stat.files} {stat.files === 1 ? "file" : "files"}
          {" · "}
          <span className="add">+{stat.added}</span> <span className="del">−{stat.removed}</span>
        </span>
        {checks === "" ? null : <span className="checks">{checks}</span>}
        {state === "" ? null : <span className="state">{state}</span>}
      </div>
      {/* Its OWN scroll container (§5's "colorized, scrolls in its own box"), so a thousand-line
          diff does not push the rail, the spine and the result card off the screen — the defect
          STUDIO-821 fixed one zone up, not reintroduced here. */}
      <div className="trdiff">
        {files.map((file, i) => (
          <div className="file" key={`${file.path}-${i}`}>
            {file.path === "" ? null : (
              <div className="path">
                <Mono>{file.path}</Mono>
              </div>
            )}
            <pre className="hunks">
              {file.lines.map((line, j) => (
                // `\u00a0` on an otherwise-empty line: a `pre` collapses a zero-height row, which
                // would make an unchanged blank line vanish from a diff that contains one.
                <span className={`l ${line.kind}`} key={j}>
                  {line.text === "" ? "\u00a0" : line.text}
                  {"\n"}
                </span>
              ))}
            </pre>
          </div>
        ))}
      </div>
      {diff.truncated ? (
        <div className="trdep">
          <b>This diff was cut short.</b> The daemon serves at most 512 KiB of a patch, and this one
          is longer. What is above is the head of it, whole lines only — the rest is on the pull
          request.
        </div>
      ) : null}
    </>
  );
}

/**
 * The daemon saying there is no diff to show — which is an ANSWER, not a fault.
 *
 * Keeps the dependency card's calm treatment and its deep link, because that is exactly what this
 * state was before the endpoint existed and it is still the useful thing to offer. What changed is
 * that the sentence is now the DAEMON's, naming the actual reason, rather than the console naming
 * a missing endpoint.
 */
function NoDiff({ run, reason }: { run: RunSummary; reason: string }) {
  const prHref = prSearchUrl(run);
  const branch = runBranch(run);
  return (
    <div className="trdep">
      <b>There is no diff to show.</b>{" "}
      {reason === "" ? "The daemon resolved no pull request for this run's branch." : `${reason}.`}
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
          <ExternalLink href={prHref}>Open this branch's pull request ↗</ExternalLink>
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
            <ExternalLink className="pr" href={row.url}>
              {row.pr} ↗
            </ExternalLink>
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
        <AskExchange key={asked.id} asked={asked} roster={roster} onOpenRoom={onOpenRoom} />
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

/** How far a shared query has got, seen by a component that has just mounted into it. */
interface ReadProgress {
  data: unknown;
  dataUpdatedAt: number;
  isFetching: boolean;
  isFetchedAfterMount: boolean;
}

/**
 * Whether a room read that COULD have seen this exchange's question has come back.
 *
 * This is the gate [`managerReply`] needs before it may read a question's absence from the window
 * as "the read has moved past it" rather than "the read has not caught up". The card is keyed on
 * the question, so mounting IS the moment the question landed, and the gate is a statement about
 * reads relative to that mount.
 *
 * `isFetchedAfterMount` alone is not that statement. It says a read SETTLED since the mount, which
 * is the same thing only when the reads outstanding at the mount were thrown away. On a WARM room
 * query — the ordinary case, the Room tab holding this key open — they are: `usePostToRoom`
 * invalidates the key on success, and what that does to a read already in flight is drop it and
 * dispatch another. That behaviour belongs to a different module, so it is pinned by its own test
 * (`useTeams.test.tsx`, "discards a room read already in flight"): stop invalidating there, or
 * replace the invalidate with an optimistic update, and the warm door reopens with every test of
 * THIS surface still green.
 *
 * A COLD query is the case that cannot cover, and it is why this hook exists. React-query cancels
 * a fetch only on a query that already holds data, so on the FIRST read of the key — the one the
 * Room tab dispatches on mount — the post's invalidate cancels nothing and dedupes into it. That
 * read is a snapshot of the room from BEFORE the question was appended, yet it settles after,
 * flipping `isFetchedAfterMount` and licensing the dock to announce that its read no longer
 * reaches a question asked a second ago. So a read already outstanding at the mount with nothing
 * cached under it is DISCOUNTED: the gate opens on the settle AFTER it, which — one query fetching
 * serially — can only come from a fetch dispatched once that read had finished, and so after the
 * question.
 *
 * The discount is deliberately narrow, and it is applied only where the cancel provably could not
 * run. It costs one poll of "reading it back…", which claims nothing; a read outstanding on a
 * query that DOES hold data was replaced by the post, and its successor is exactly the read this
 * gate is looking for.
 *
 * A read that FAILS settles too, and either answer here costs nothing: a failed read is reported
 * as a failed read by [`emptyNote`], before any note this gate could have chosen.
 */
function useReadPostdatingMount(room: ReadProgress): boolean {
  // Read on the first render only — the mount, which is when the question landed.
  const uncancellableAtMount = useRef(room.data === undefined && room.isFetching);
  // What that discounted read delivered, kept so the settle after it can be told apart. Both
  // halves matter: the data reference changes whenever the room's content did, and the timestamp
  // changes on every success — including one whose content react-query structurally shared.
  const discounted = useRef<{ data: unknown; at: number } | null>(null);
  if (!room.isFetchedAfterMount) return false;
  if (!uncancellableAtMount.current) return true;
  if (discounted.current === null) {
    discounted.current = { data: room.data, at: room.dataUpdatedAt };
    return false;
  }
  return room.data !== discounted.current.data || room.dataUpdatedAt !== discounted.current.at;
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
 *
 * It is KEYED on that question's id, which is load-bearing rather than a list-rendering habit: one
 * exchange is one question, so mounting is exactly the moment the question landed. Both pieces of
 * state below are scoped to it by that alone — the read gate measures "settled since this mounted",
 * and the answer it holds is discarded when the operator asks something else.
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
  // This exchange's own read gate: whether a read that COULD have seen the question has come back
  // (see [`useReadPostdatingMount`]). Until one has, the newest data on the key can only be a
  // window from before the question, and its silence about it means nothing — see
  // [`managerReply`], which is what turns that difference into what the dock is allowed to say.
  // The card is keyed on the question, so this hook's "since the mount" is "since the question".
  const settledSinceAsking = useReadPostdatingMount(room);
  const outcome = useMemo(
    () => managerReply(messages, asked, settledSinceAsking),
    [messages, asked, settledSinceAsking],
  );
  // The answer, once any read has shown it. A room log is append-only, so a reply that existed
  // cannot stop existing: an `answered` outcome is a fact about the ROOM, while `past-window` is
  // only ever a fact about the READ. Holding the former means the 50-post whole-room window
  // scrolling past the question cannot replace an answer already on screen with "it cannot tell
  // whether @manager replied" — a sentence that would be false at the moment it was shown, and the
  // same disappearance the card is keyed and kept mounted to prevent. `waiting` is deliberately NOT
  // held: it is a claim about the read too, and once the read has moved on it stops being true.
  const [answer, setAnswer] = useState<TeamsRoomMessage | null>(null);
  useEffect(() => {
    if (outcome.kind !== "answered") return;
    const { reply } = outcome;
    setAnswer((held) => (held?.id === reply.id ? held : reply));
  }, [outcome]);
  const shown: AskOutcome = answer === null ? outcome : { kind: "answered", reply: answer };
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
        {shown.kind === "answered" ? (
          <div className="mcard">
            <div className="top">
              {/* The room's own colour for this identity — `@manager` is not on the roster
                  (`RESERVED_IDENTITIES` keeps it off), so this resolves to the unknown-teammate
                  colour. That is the point: the Room tab resolves it exactly the same way, and one
                  identity must not wear two colours across two views of one post. */}
              <span className="who2" style={{ color: teammateColor(roster, shown.reply.from) }}>
                {shown.reply.from}
              </span>
              <Timestamp>{clockTime(shown.reply.at)}</Timestamp>
            </div>
            {/* The room post itself, through the room's own renderer — the manager's prose arrives
                quoted line by line and its records under `From my own records —`, and that layout
                is what tells the operator which half the daemon vouches for. Reshaping it here
                would make this a second answer wearing the first one's name. A body the room read
                had to cut carries the `…` `truncate_bytes` leaves on it, so this surface is
                exactly as honest about its own bound as the room is. */}
            <Markdown source={shown.reply.body} />
          </div>
        ) : (
          <div className="pending">
            {/* Never "answering" or "still thinking": nothing tells this page the manager has even
                read the question, and `past-window` cannot tell whether it replied at all. */}
            {emptyNote(room, ASK_READING_NOTE, askNote(shown))}{" "}
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
