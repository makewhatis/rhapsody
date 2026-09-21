// The Jobs worklist model — STUDIO-681 §3, built by STUDIO-683.
//
// The console's Jobs view is the operator's queue: one row per ticket the daemon is working.
// The row-per-issue merge itself is NOT re-implemented here — `runs-model.mergeJobs` already
// folds the live snapshot (`/api/v1/state`), the pending retries and the issue-level history
// (`/api/v1/history/issues`) into one row per ticket, and it is the tested source of a job's
// status. This module is the console's PRESENTATION layer over that: it renames the daemon's
// run-centric statuses into the ones the spec's Pill speaks — its five, plus STUDIO-780's
// `reviewing` — and derives the Now-strip counts, the two filters and the project list from them.
//
// DEPENDENCY (§9/§11): the spec maps this view to `GET /api/v1/issues`, which the daemon does
// not serve. There is therefore no assignee and no PR link per ticket. What is used instead,
// and what it costs:
//   - Status  — the TICKET's lifecycle when the daemon resolved one (STUDIO-702), else the
//               daemon's own job status, narrowed to `reviewing` when a live run is on a REVIEW
//               ticket (STUDIO-780) or IS a review run (STUDIO-826). A review run is also the one
//               row whose own outcome is terminal, having no ticket to be handed to a reviewer.
//               See `consoleJobStatus`.
//   - Assignee— the DURABLE assignee the daemon resolves per history row (STUDIO-735), falling
//               back to the Teams roster's LIVE tickets (`GET /api/v1/teams`) only for a row that
//               has none — a run that started before its routing record landed. See
//               `durableAssignees`.
//   - PR      — no endpoint carries one; the column renders "—" until one does.
import type {
  BlockedEntry,
  IssueCountsResponse,
  IssueLifecycle,
  IssueRun,
  IssueStatusBucket,
  RunSummary,
  TeamsOverview,
  TicketCostRow,
  TypedConfigResponse,
} from "@/lib/api";
import type { JobRow } from "@/lib/runs-model";
// The run detail's own vocabulary, imported rather than restated: `statusNote` exists to make the
// worklist and the run detail AGREE about a run, and two copies of "completed reads done" is the
// disagreement it is fixing. `console-job-detail` imports only TYPES back from here, so this is not
// a runtime cycle.
import { runOutcomeLabel } from "@/lib/console-job-detail";
// `console-board` imports ONLY types back from here (`import type`), so this is not a runtime cycle
// — the same arrangement `console-job-detail` uses above.
import { parsePullRequest, pullRequestLabel } from "@/lib/console-board";
import { formatDuration } from "@/lib/format";

/**
 * The states the console's Pill paints (§1.3), plus `reviewing` (STUDIO-780).
 *
 * `reviewing` and `review` are the two things "in review" used to stand for at once, and they are
 * different claims about different subjects: `reviewing` says an agent is DOING a review right now
 * — the ticket's own job is to review a teammate's pull request, and a run is live on it — while
 * `review` says this ticket's work is finished and is AWAITING somebody's review. A worklist that
 * spells both "in review" is not ambiguous by accident; it is asserting they are the same state.
 *
 * `parked` (STUDIO-966) is the answer for a ticket sitting OUTSIDE its project's `active_states` —
 * Backlog, Triage, any state the dispatcher will not pick up. It has its own word because `queued`
 * claims an agent is coming, and for a parked ticket nothing is: a person has to move it. See
 * [`consoleJobStatus`] for why it is derived from the configured state SETS rather than Linear's
 * own state `type`, and [`consoleStatusMatches`] for why the Queued filter still reaches it.
 */
export type ConsoleJobStatus =
  | "run"
  | "reviewing"
  | "review"
  | "queued"
  | "done"
  | "blocked"
  | "parked";

/**
 * The Seg's ids. `reviewing` is deliberately NOT one of them: it is a kind of RUNNING — an agent
 * has the ticket and is working — so "Running" covers it (see [`matchConsoleFilter`]). A sibling
 * button would split the live set in two and leave the strip's four buckets no longer partitioning
 * the worklist, which is the one property the Seg has.
 */
export type ConsoleJobFilterId = "all" | Exclude<ConsoleJobStatus, "reviewing">;

/** The status Seg of §3, in the prototype's order. */
export const CONSOLE_JOB_FILTERS: readonly { id: ConsoleJobFilterId; label: string }[] = [
  { id: "all", label: "All" },
  { id: "review", label: "In review" },
  { id: "run", label: "Running" },
  { id: "queued", label: "Queued" },
  { id: "done", label: "Done" },
];

/** The Pill's text per status — the prototype's wording. */
export const CONSOLE_STATUS_LABELS: Record<ConsoleJobStatus, string> = {
  run: "running",
  reviewing: "reviewing",
  review: "in review",
  queued: "queued",
  done: "done",
  blocked: "blocked",
  parked: "parked",
};

/**
 * Maps a run OUTCOME onto the console's vocabulary — the answer used when the daemon could not
 * resolve the ticket's real state.
 *
 * `completed → review` is the pipeline's own rule: a run that finishes cleanly hands its ticket
 * to the configured review state, so a just-completed run means "waiting on a reviewer".
 * `failed` and `waiting` both mean a human has to act, which is what `blocked` says; `stopped`
 * leaves the ticket idle awaiting its next dispatch, which is `queued`.
 *
 * Takes a plain string rather than `JobStatusKey`: `JobRow.status` is typed as the wider
 * `StatusKey`, and narrowing it with a cast would hide exactly the case the default arm is
 * here to survive.
 */
function fromRunOutcome(status: string): ConsoleJobStatus {
  switch (status) {
    case "running":
      return "run";
    case "completed":
      return "review";
    case "failed":
    case "waiting":
      // STUDIO-967: a run stopped at its per-run token ceiling needs a person, exactly as a failure
      // does — the daemon halted the ticket deliberately and the reconciliation sweep reports it
      // rather than resuming it. Folding it into the default `queued` would hide the one thing the
      // operator has to act on.
    case "token_ceiling":
      return "blocked";
    default:
      return "queued";
  }
}

/**
 * The row's status: the TICKET's lifecycle when the daemon resolved one, else the run outcome —
 * and, when a live run is on a REVIEW ticket, `reviewing` rather than `run`.
 *
 * The ticket's state is the truer signal and outranks the outcome, because an outcome never
 * expires. Every completed run used to read "in review" for as long as the store kept it, so the
 * count grew monotonically with history and `done` was unreachable — STUDIO-702.
 *
 * Two rules are not simply "lifecycle wins", and both are deliberate:
 *   - A LIVE run outranks the ticket. A mid-run handoff parks the ticket in a review state while
 *     the agent is still working, and the worklist must keep saying "running".
 *   - An `open` ticket keeps a `failed`/`waiting` outcome's `blocked`. Those describe the RUN, and
 *     a human still has to act on them; what `open` does override is `completed → review`, since a
 *     ticket that went back to open work is not awaiting a reviewer.
 *
 * An absent or unrecognized `lifecycle` falls back to the outcome mapping unchanged, so a console
 * talking to a daemon that predates the field behaves exactly as it did before.
 *
 * The third rule is `reviewTicket`, and it narrows the LIVE arm only (STUDIO-780). A review ticket
 * carrying a live run is an agent doing a review, which is what `reviewing` says; the same ticket
 * parked in the tracker's review state is not — its run is over and a person owes it a read, which
 * is exactly what `review` already says and what it says for every other ticket in that state. So
 * the flag changes the WORD for live work and nothing else: it never promotes a queued, blocked or
 * terminal row, and it cannot make a row claim an agent is working when none is.
 *
 * It defaults to `false` on the same terms as `lifecycle` defaults to absent — a daemon that does
 * not serve `review_ticket`, a ticket minted before the marker label existed, and a tracker that
 * could not be asked are all the same answer, and all three read exactly as they did before.
 *
 * The fourth rule is `reviewRun`, and it is the one place a row's own OUTCOME is terminal
 * (STUDIO-826). A ticketless review job — `review.mode: ticketless`, a run dispatched against a
 * `pr:owner/repo#n@reviewer` key — has no tracker ticket, so `lifecycle` is not merely unresolved
 * for it, it is unresolvable: there is nothing to resolve. Both halves of the fallback are then
 * wrong for it. Live, `reviewTicket` cannot reach it, because that marker is a TICKET label and
 * there is no ticket to carry one. Finished, `completed → review` claims the work now awaits
 * somebody's review, when the review IS the work and it is over — and that claim went on to bill
 * the strip's "Needs you" for a job that needed nobody.
 *
 * So a review run reads its own run: `reviewing` while live, `done` when it completed, and `blocked`
 * or `queued` exactly as before otherwise. A FAILED review is deliberately still `blocked` and still
 * the operator's move; it is the one review outcome that genuinely does need a person.
 *
 * It changes nothing about a ticket-based review row, which keeps STUDIO-780's behaviour entirely:
 * the two flags mark different subjects, the daemon sets them on different rows, and only the live
 * arm is shared between them.
 *
 * The fifth rule is `heldNeverRan` (STUDIO-949), and it names the word deliberately. A
 * `rhapsody:human` ticket that has NEVER RUN reaches this function as a synthetic `waiting` row,
 * whose outcome maps to `blocked` — the BLOCKER's word. Nothing is blocked and nothing is wrong:
 * the dispatcher has deliberately refused it and no agent will ever run it, so painting it
 * "blocked" puts a deliberate hold one pill away from a real fault, which is the confusion the
 * board's own chip styling exists to prevent. It reads `queued` — waiting for a person rather than
 * mysteriously idle — because the hold, not the run that never happened, is the whole fact.
 *
 * The rule is scoped to a hold with NOTHING RAN (STUDIO-949 rounds 5-7). A hold can OUTLIVE a run —
 * a ticket parked in review, then labelled — and `mergeJobs` says in as many words that "the real
 * run still decides the lane": the card belongs in Review, wearing its `held for a human`
 * sub-label, because the daemon is deferring the review rounds that ticket is owed. Returning
 * `queued` there moved the card out of Review and contradicted the watcher, which is at that same
 * moment holding the review. So the hold's own word only applies where there is no run to name the
 * lane.
 *
 * "Nothing ran" is NOT "no lifecycle resolved" (round 7). `lifecycleByIssue` drops every row the
 * daemon could not answer, and it answers off a TTL cache refreshed a bounded number of ids per
 * lookup — a daemon that just restarted serves most of its rows with no `lifecycle`. An unresolved
 * lifecycle means "the daemon could not ask", not "this ticket never ran", and a row in that gap
 * can carry a real FAILED run: painting it `queued` erases the operator's cue that an agent
 * flailed, and splits the lane from the strip that scores the same ticket through a bucket. The
 * caller passes `heldNeverRan` only for a row that is a current hold AND has no real run at all
 * (`JobRow.runId === 0`), so an unresolved lifecycle never reaches this arm.
 *
 * The sixth rule is `parked` (STUDIO-966), and it splits the `open` arm in two. `open` is the
 * daemon's bucket for "in an active state, OR in none of the configured sets (Backlog, Triage)" —
 * one bucket, deliberately, because the four buckets are derived from the configured state SETS so
 * this classification and the selection gate cannot disagree about what terminal means. That is
 * right about terminal and silent about dispatchable: a `Todo` ticket a finished run left behind is
 * genuinely back in the pool awaiting another agent (`queued`), while a `Backlog` one is not, and no
 * agent will ever start it — the dispatcher's `active_states` set excludes it.
 *
 * The distinction is therefore `tracker_state` against the PROJECT's configured `active_states`, the
 * same set the gate filters by, resolved per project because `projects[].active_states` overlays the
 * global default. Nothing here reads Linear's state `type`: the sets are the daemon's own scheduling
 * vocabulary, which is exactly what makes this agree with the gate. The caller passes `parked`
 * already resolved (see [`isParkedState`]/[`activeStatesFor`]); a blank `tracker_state`, an unknown
 * project and a config that has not loaded all resolve to `false` — say what was said before the
 * rule existed rather than guess.
 *
 * It applies ONLY where the lifecycle is `open`. A ticket in a review or terminal state already
 * reaches a truer word, and a `failed`/`waiting` outcome still keeps `blocked`: that is the RUN's
 * fact and a human has to act on it wherever the ticket is parked. A live run still outranks
 * everything — the earlier arms have already returned.
 *
 * The seventh rule is `budgetHeldNeverRan` (STUDIO-957/970), the fifth rule's sibling for a ticket
 * the dispatcher refused because a provider's daily token budget is spent. It reads `queued` for
 * exactly the reason the human hold does — the refusal, not the run that never happened, is the
 * whole fact, and it is a deliberate wait rather than a fault — but it is a SEPARATE fact from
 * `heldForHuman`: a budget hold clears at local midnight on its own and needs nobody, so the console
 * must not tell an operator a person is required. It is scoped to a hold with nothing ran on the
 * same terms, so a budget hold that outlives a prior run keeps that run's lane.
 */
export function consoleJobStatus(
  status: string,
  lifecycle?: string,
  reviewTicket = false,
  reviewRun = false,
  heldNeverRan = false,
  parked = false,
  budgetHeldNeverRan = false,
): ConsoleJobStatus {
  const fromRun = fromRunOutcome(status);
  // A hold on a ticket that HAS run keeps the run's lane and wears the hold as its sub-label; only a
  // hold on a ticket that never ran speaks for the lane itself. See the doc above.
  if (heldNeverRan || budgetHeldNeverRan) return "queued";
  if (fromRun === "run") return reviewTicket || reviewRun ? "reviewing" : "run";
  // No ticket exists behind this row, so there is no lifecycle for one to outrank and the run's own
  // outcome is the whole truth. `completed` here means the review finished, not that one is owed.
  if (reviewRun) return fromRun === "review" ? "done" : fromRun;
  switch (lifecycle) {
    case "done":
    case "canceled":
      return "done";
    case "in_review":
      return "review";
    case "open":
      // A parked ticket is not waiting for an agent, whatever the run that left it there did — but a
      // FAILED/waiting run is still the operator's move, so `blocked` survives the split.
      if (parked) return fromRun === "blocked" ? "blocked" : "parked";
      return fromRun === "review" ? "queued" : fromRun;
    default:
      return fromRun;
  }
}

/**
 * What the row's own RUN did, when that is a different fact from the status beside it (STUDIO-780).
 *
 * The bug this closes is a reading, not a wrong value: both halves were already true and the
 * worklist stated only one of them. A row painted from the TICKET's lifecycle says "in review",
 * the run detail behind it says "done", and with no cue which subject either word belongs to the
 * list reads as stale — the operator's report was *"they are stuck in 'in review' in the dashboard,
 * and when I click in, they are all done"*. So the row says both: `in review · run done`. Opening
 * it then confirms the row instead of contradicting it.
 *
 * Three conditions, and each one is what keeps the note from being noise:
 *
 *   - `lifecycleResolved` — the status really IS the ticket's. When the daemon could not resolve a
 *     lifecycle the status was inferred FROM the run outcome, so the two are one fact and a note
 *     would restate the pill in different words.
 *   - the run has ENDED. A live run is the row's status, and "running · run running" says nothing.
 *   - the two words DIFFER. A `done` ticket whose run completed reads "done" either way, and the
 *     note is for the rows where the subjects diverge — a merged ticket whose run failed, a parked
 *     ticket whose run is over.
 *
 * The word comes from [`runOutcomeLabel`], which is the one the run detail's header prints, so the
 * two surfaces cannot drift into naming the same outcome differently.
 *
 * The caller adds a fourth condition it can see and this cannot: a row that already carries a
 * `subLabel` gets no note. `subLabel` is the held/failed detail, and on a failed row it is the
 * error itself — "blocked · run failed · <error>" spends a third of the pill restating what the
 * next clause says better. See [`buildConsoleJobs`].
 */
export function statusNote(
  status: ConsoleJobStatus,
  runStatus: string,
  lifecycleResolved: boolean,
): string | undefined {
  if (!lifecycleResolved) return undefined;
  if (runStatus === "running" || runStatus === "waiting") return undefined;
  const ran = runOutcomeLabel(runStatus);
  // "unknown" is what an EMPTY outcome renders as — the daemon said nothing about how this run
  // ended, which is not a fact worth putting beside the ticket's state.
  if (ran === "unknown" || ran === CONSOLE_STATUS_LABELS[status]) return undefined;
  return `run ${ran}`;
}

/**
 * Whether this ticket is waiting on the OPERATOR — the "Needs you" count the design record's §6
 * adds to the Now strip. Derived from state the worklist already holds; no new endpoint.
 *
 * WHAT IT CLAIMS, EXACTLY. A ticket parked in review awaits a person's verdict or merge, and a
 * failed run awaits a person's decision about what happens next. That is the whole claim, and it
 * is deliberately coarse: what is NOT claimed is any sharper discrimination among them, because
 * the facts that would allow one are named at the bottom of this comment and none of them is
 * served to this view.
 *
 * That coarseness is why the strip now carries this and nothing beside it. On a healthy tracker
 * the set is very nearly the in-review set — the failed runs are the whole difference — and when
 * BOTH were painted, the honest answer looked like a bug: the same number, twice, two pills apart
 * in different colours (16/16 on the live default page, 27/27 over all 358 rows). The convergence
 * itself is not the defect, and narrowing it to manufacture a difference was tried and withdrawn
 * twice; if the tracker says twelve tickets are parked for review, twelve tickets really are
 * waiting on a human, and the console should say that ONCE. So per David's 2026-09-03 decision
 * the in-review pill is gone and this is the home's single human-attention flag — it is the
 * in-review-that-needs-you, widened by the failures that need one without reading in-review.
 *
 * `blocked` qualifies only when it is a FAILED run. The other thing that reads blocked is a held
 * dependent (`runs-model`'s synthetic `waiting` row), and that one waits on its predecessor rather
 * than on the operator. If the predecessor needs a human it is counted on its OWN row; counting
 * the dependent too would bill one decision twice.
 *
 * WHY IT DOES NOT SPLIT ON A LIFECYCLE'S PRESENCE, which is the mistake worth keeping written
 * down. An earlier shape counted a review row only when `lifecycle === "in_review"` came back,
 * reading an ABSENT lifecycle as "inferred from a stale outcome, so nobody is waiting". But
 * `StateProvider::issue_lifecycles` (crates/httpapi/src/server.rs) answers off a TTL cache and the
 * reads cell's tracker AT REQUEST TIME, and a missing tracker, a failed round-trip and an unknown
 * id are all *no answer*. Absence is therefore a LIVENESS condition of the daemon, not a property
 * of the ticket — the same ticket answers on a warm cache and does not answer on a cold one. The
 * rows that split were simply uncached, and once they warmed the split became a silent no-op.
 *
 * That liveness question is real, but it belongs to the PAYLOAD rather than to a row, and
 * [`consoleJobCounts`] is where it is answered.
 *
 * WHAT WOULD MAKE IT SHARPER, flagged rather than guessed at (§9/§11). "Needs you" ought to mean
 * "your merge is the next move", and the two facts that would say so are not served here: no
 * endpoint carries a ticket's PR or its checks (the PR column renders "—" for the same reason),
 * and no per-ticket record says whether a REVIEWER — human or agent — already holds it. A
 * threshold guessed from a timestamp would look like a narrowing without being one, so until a
 * supporting endpoint lands this stays the coarse claim it can actually defend.
 *
 * Takes the run status as a plain string for the reason `fromRunOutcome` does: `JobRow.status` is
 * the wider `StatusKey`, and narrowing it with a cast would hide the case this has to survive.
 */
export function needsOperator(status: ConsoleJobStatus, runStatus: string): boolean {
  if (status === "review") return true;
  return status === "blocked" && runStatus !== "waiting";
}

/** One row of the §3 worklist, fully derived so the table stays presentational. */
export interface ConsoleJobRow {
  /** Stable React key. */
  key: string;
  /** Ticket key — also the `job/:key` route target (§10 box 2.8). */
  issue: string;
  /** The run the row's trace-sparkline previews; 0 when persistence is off (there is none). */
  runId: number;
  /** True while this ticket's newest run is genuinely in flight — the sparkline's playhead. */
  live: boolean;
  title: string;
  /** Project display name, or "—" when the daemon runs single-project. */
  project: string;
  /** Raw project slug — the project Select's value. */
  projectSlug: string;
  status: ConsoleJobStatus;
  statusLabel: string;
  /**
   * This row's own RUN status, before any ticket-lifecycle mapping — `JobRow.status` verbatim.
   *
   * `status`/`statusLabel` are the TICKET's when the daemon resolved a lifecycle, so they cannot
   * answer "how did this run end" — a failed review run maps to `blocked`, and the board's reviewer
   * chip would then read "blocked" where it promises the run's own outcome (STUDIO-925). Carried
   * raw so [`runOutcomeLabel`] is applied at the one surface that prints a run's word.
   */
  runOutcome: string;
  /** The tracker's own workflow-state name behind `status`, or "" when the daemon had no answer. */
  trackerState: string;
  /** Teammate name, or "" when solo/unassigned (the table renders "—"). */
  assignee: string;
  /**
   * True when this row's own RUN is a review run (STUDIO-826), carried through from the listing so
   * the board can fold it onto the ticket it reviews instead of rendering it as its own card
   * (STUDIO-925). The row's `reviewOf` is where it attaches.
   */
  reviewRun: boolean;
  /**
   * PR reference, or "" when none is known. Parsed from a review row's `pr:owner/repo#n@reviewer`
   * issue key (STUDIO-925) — the number has always been in the identifier while this column
   * rendered "—". The display form is the prototype's `#n`.
   */
  pr: string;
  /** The PR's URL, or "" when none — what makes the chip a link rather than a label. */
  prUrl: string;
  /**
   * The provider this row's run actually billed (STUDIO-909), or "" when the run recorded none
   * (a legacy row) — the compact scanning badge, never a per-row drill-down.
   */
  provider: string;
  /**
   * The ticket's token cost split by provider (STUDIO-926) — the implementation run plus every
   * review of it, summed. `[]` when the daemon has no run to cost yet (a queued ticket with no
   * history row). See [`ticketCostsByIssue`].
   */
  costs: TicketCost[];
  /**
   * How long the ticket's live run has been going, formatted ("6m"; seconds only under a minute), or "" when [`live`] is
   * false. This plus the current transcript step is the honest "is it moving?" signal the design
   * record's progress-bar ban asks for in its place (STUDIO-926) — a card that has shown the same
   * step for a long while is the real stall tell, and neither half needs a denominator the daemon
   * does not have.
   */
  elapsed: string;
  /**
   * For a review row, the TICKET it is reviewing (STUDIO-834) — what the table leads with in place
   * of the `pr:owner/repo#n@reviewer` key, which names no work. "" for every other row, and for a
   * review row whose origin the daemon could not resolve to a ticket; the table then leads with
   * [`issue`] exactly as it did before this existed.
   *
   * Never the route target. [`issue`] stays the key the row OPENS, because the row shows a review
   * run and the ticket named here is somebody else's job.
   */
  reviewOf: string;
  /** Relative "6m ago", or "—" when the ticket has never run. */
  updated: string;
  /** Sort key: ms since epoch of the newest activity, 0 when unknown. */
  updatedAtMs: number;
  /** Held/failed detail, e.g. "waiting on STUDIO-1 · In Progress". */
  subLabel?: string;
  /**
   * The provider whose spent daily token budget holds this ticket (STUDIO-957/970), or undefined
   * when it is not budget-held. Distinct from the `subLabel`'s human-hold wording on purpose: the
   * two holds clear differently, and the card must not say a person is needed for a clock.
   */
  budgetHeld?: string;
  /**
   * What this row's own RUN did, when the status beside it is the TICKET's and the two are
   * different facts — "run done" on a ticket parked in review (STUDIO-780). See [`statusNote`].
   * Absent when the row's status already says everything there is to say.
   */
  statusNote?: string;
  /** Whether the ticket's next move is the OPERATOR's — the Now strip's "Needs you" (§6). */
  needsYou: boolean;
  /**
   * Whether the daemon actually answered a tracker lifecycle for this ticket on THIS request.
   *
   * Not a property of the ticket — `issue_lifecycles` resolves per request off a TTL cache, so
   * this says only "the tracker spoke for this row just now". It exists so [`consoleJobCounts`]
   * can tell a healthy payload from the stripped one a cold cache serves; see [`needsOperator`].
   */
  lifecycleResolved: boolean;
}

/**
 * Ticket key → teammate name, from the roster's LIVE tickets. Only a RUNNING ticket resolves: the
 * roster lists what each teammate is working on now, so a ticket drops out of it the moment its run
 * ends. That is exactly why it is the fallback rather than the source — see `durableAssignees`.
 */
export function ticketAssignees(overview: TeamsOverview | undefined): Map<string, string> {
  const byTicket = new Map<string, string>();
  for (const mate of overview?.roster ?? []) {
    for (const ticket of mate.tickets ?? []) {
      if (ticket !== "" && !byTicket.has(ticket)) byTicket.set(ticket, mate.name);
    }
  }
  return byTicket;
}

/** One ticket's resolved state: the normalized bucket plus the tracker's own name for it. */
export interface TicketLifecycle {
  lifecycle: IssueLifecycle;
  trackerState: string;
}

/**
 * Ticket key -> resolved lifecycle, from the issue-level listing's per-row fields (STUDIO-702).
 *
 * A row the daemon could not resolve carries neither field and is SKIPPED rather than mapped to a
 * default — an absent key is what makes `consoleJobStatus` fall back to the run outcome, so a
 * placeholder here would silently defeat the fallback. The listing is one row per issue, but the
 * first answer wins if that ever stops being true.
 */
export function lifecycleByIssue(rows: readonly IssueRun[]): Map<string, TicketLifecycle> {
  const byIssue = new Map<string, TicketLifecycle>();
  for (const r of rows) {
    if (r.issue_identifier === "" || r.lifecycle === undefined || byIssue.has(r.issue_identifier)) {
      continue;
    }
    byIssue.set(r.issue_identifier, {
      lifecycle: r.lifecycle,
      trackerState: r.tracker_state ?? "",
    });
  }
  return byIssue;
}

/**
 * The `active_states` sets the console classifies "dispatchable vs parked" against (STUDIO-966),
 * resolved per project exactly as the daemon's selection gate resolves them.
 *
 * `global` is `tracker.active_states` — the default a project inherits; `bySlug` carries only the
 * projects that OVERRIDE it (`projects[].active_states`, or the daemon's own resolved `effective` of
 * that when it is served). A row whose project has no entry falls back to `global`, which is what a
 * single-project install has and what an unknown project must mean. Nothing here hardcodes a state
 * NAME: these are the configured sets verbatim, which is the only way this classification and the
 * gate can agree.
 */
export interface DispatchableStates {
  global: readonly string[];
  bySlug: ReadonlyMap<string, readonly string[]>;
}

/** The empty answer — nothing known, so nothing is parked. The fallback until the config loads. */
export const NO_DISPATCHABLE_STATES: DispatchableStates = { global: [], bySlug: new Map() };

/**
 * Resolve the per-project dispatchable sets from `GET /api/v1/config` (STUDIO-966).
 *
 * A project's own `active_states` overrides the global default; when the daemon serves its resolved
 * `effective`, that is preferred, because it is the daemon's OWN answer for the same question and
 * cannot drift from what the gate uses. A `slug` is what a history row carries (`project_slug`), and
 * `projects[].slugs` is the list a project answers to — first project wins if two ever claimed one.
 */
export function projectActiveStates(cfg: TypedConfigResponse | undefined): DispatchableStates {
  const global = cfg?.global?.active_states ?? [];
  const bySlug = new Map<string, readonly string[]>();
  for (const project of cfg?.projects ?? []) {
    const states = project.effective?.active_states ?? project.active_states ?? global;
    for (const slug of project.slugs ?? []) {
      if (slug !== "" && !bySlug.has(slug)) bySlug.set(slug, states);
    }
  }
  return { global, bySlug };
}

/** The active-state set for one project slug — its own override, else the global default. */
export function activeStatesFor(states: DispatchableStates, projectSlug: string): readonly string[] {
  return states.bySlug.get(projectSlug) ?? states.global;
}

/**
 * Whether a tracker state sits OUTSIDE the dispatchable set — i.e. the gate would never pick this
 * ticket up. Comparison is trimmed and case-folded, mirroring the daemon's `normalize_state`, so
 * `" backlog "` and `"Backlog"` answer alike.
 *
 * A blank state, or no configured set at all, answers `false`: the console cannot tell, and "not
 * parked" is the pre-existing reading. That is the same absence-is-not-a-verdict rule
 * [`lifecycleByIssue`] follows — a cold config load must not repaint every row as parked.
 */
export function isParkedState(trackerState: string, activeStates: readonly string[]): boolean {
  const state = trackerState.trim().toLowerCase();
  if (state === "" || activeStates.length === 0) return false;
  return !activeStates.some((active) => active.trim().toLowerCase() === state);
}

/**
 * The tickets the daemon says are REVIEW TICKETS, from the issue-level listing's `review_ticket`
 * field (STUDIO-780) — tickets whose own job is to review a teammate's pull request.
 *
 * The daemon serializes only the POSITIVE, and this mirrors that exactly: an ordinary ticket, a
 * ticket the tracker could not be asked about, and a review ticket minted before the marker label
 * existed are all simply absent, because all three mean the same thing to this view — say about
 * this row what was said before the field existed.
 *
 * Nothing here reads the title. `"Review: "` is a convention the daemon happens to follow when it
 * MINTS one, not a fact about a ticket, and a hand-written ticket that opens with the word would be
 * mislabelled silently and forever.
 */
export function reviewTicketIssues(rows: readonly IssueRun[]): Set<string> {
  const out = new Set<string>();
  for (const r of rows) {
    if (r.issue_identifier !== "" && r.review_ticket === true) out.add(r.issue_identifier);
  }
  return out;
}

/**
 * The rows whose own RUN is a review, from the issue-level listing's `review_run` field
 * (STUDIO-826) — a run dispatched against a `pr:owner/repo#n@reviewer` key instead of a ticket,
 * which is what `review.mode: ticketless` produces.
 *
 * The sibling of [`reviewTicketIssues`] and not a substitute for it: that one reads a marker on a
 * TICKET, and these rows have no ticket for a marker to live on. It is why they never reached
 * `reviewing`, and why their completion has to stop meaning "awaiting a reviewer" — see
 * [`consoleJobStatus`].
 *
 * Positive-only, mirroring the daemon exactly, for the reason [`reviewTicketIssues`] is.
 *
 * Nothing here parses the key, and nothing reads the title. The daemon resolves this from the run's
 * own issue id and serializes the answer; re-deriving it from the `pr:` prefix would turn a wire
 * format into a UI contract, and `"Review owner/repo#n at <sha>"` is a string the daemon happens to
 * mint rather than a fact about a run.
 */
export function reviewRunIssues(rows: readonly IssueRun[]): Set<string> {
  const out = new Set<string>();
  for (const r of rows) {
    if (r.issue_identifier !== "" && r.review_run === true) out.add(r.issue_identifier);
  }
  return out;
}

/**
 * Review-run key → the TICKET that review is of, from the issue-level listing's `review_of` field
 * (STUDIO-834).
 *
 * The one thing that connects a review row to the work it is about. A `pr:owner/repo#n@reviewer`
 * key names the repository, the number and the reviewer and no ticket at all, so this cannot be
 * derived on this side — the link lives on the daemon's watch row, as the origin it recorded when
 * the pull request entered the watch set, and the daemon parses it with the same reader it moves
 * tickets by. `handoff:` and `adopt:` origins are indistinguishable here by design; both arrive as
 * a bare ticket key.
 *
 * A row carrying no `review_of` — an operator-introduced pull request names no ticket, and a key
 * with no surviving watch row resolves to none — is SKIPPED rather than mapped to "", exactly as
 * [`durableAssignees`] skips a row with no assignee. An absent key is what makes the row fall back
 * to the `pr:` key it already showed.
 */
export function reviewOfTickets(rows: readonly IssueRun[]): Map<string, string> {
  const byIssue = new Map<string, string>();
  for (const r of rows) {
    if (r.issue_identifier === "" || !r.review_of || byIssue.has(r.issue_identifier)) continue;
    byIssue.set(r.issue_identifier, r.review_of);
  }
  return byIssue;
}

/**
 * Ticket key → teammate name, from the issue-level listing's own `assignee` field (STUDIO-735).
 *
 * This is the historical record — who the run was dispatched under — so unlike `ticketAssignees` it
 * still answers once the job has moved to in review or done, which is the whole point. A row
 * carrying no `assignee` is SKIPPED rather than mapped to "", so an absent key is what makes
 * `buildConsoleJobs` consult the live roster; a placeholder here would defeat that fallback exactly
 * as it would for `lifecycleByIssue`. The listing is one row per issue, but the first answer wins if
 * that ever stops being true.
 */
export function durableAssignees(rows: readonly IssueRun[]): Map<string, string> {
  const byIssue = new Map<string, string>();
  for (const r of rows) {
    if (r.issue_identifier === "" || !r.assignee || byIssue.has(r.issue_identifier)) continue;
    byIssue.set(r.issue_identifier, r.assignee);
  }
  return byIssue;
}

/**
 * Ticket key → the provider its newest run actually billed, from the issue-level listing's own
 * `provider` field (STUDIO-909). The compact badge the worklist scans by: "which of these four runs
 * is on Fireworks" is a scanning question. A row with no recorded provider is SKIPPED rather than
 * mapped to "", so an absent key is what leaves the badge off a legacy row — exactly the omission
 * the daemon's field uses to mean "unknown".
 */
export function providerByIssue(rows: readonly IssueRun[]): Map<string, string> {
  const byIssue = new Map<string, string>();
  for (const r of rows) {
    if (r.issue_identifier === "" || !r.provider || byIssue.has(r.issue_identifier)) continue;
    byIssue.set(r.issue_identifier, r.provider);
  }
  return byIssue;
}

/**
 * One provider's token total on a ticket's card (STUDIO-926). `provider` is "" for tokens whose
 * run recorded none (a legacy row) — folded in rather than dropped, because a cost cannot be
 * skipped the way the single-row badge above can. `estimated` is true when ANY run folded into
 * this bucket ended without a clean `result` event: a sum with one floored input is itself a
 * floor, never an authoritative total.
 */
export interface TicketCost {
  provider: string;
  totalTokens: number;
  estimated: boolean;
}

/**
 * Elapsed time for a live row's activity line. Whole minutes once past one, because the clock that
 * drives this ticks every 30s: a seconds figure would sit frozen on screen and then jump, claiming
 * a precision it does not have.
 */
function liveElapsed(ms: number): string {
  const s = Math.max(0, Math.floor(ms / 1000));
  return formatDuration(s < 60 ? s : Math.floor(s / 60) * 60).replace(/ 0s$/, "");
}

/**
 * Ticket key → its token cost, split by provider, from the daemon's whole-store ledger
 * (`GET /api/v1/history/costs`, STUDIO-926).
 *
 * This is the number the ticket's card exists to answer: "one ticket's two reviews cost 9.4M
 * tokens" is EVERY implementation run plus every review round, not the newest of each. The ledger
 * arrives already summed per (ticket, provider) with review runs credited to the ticket they
 * reviewed, so this only groups and orders — it deliberately does NOT sum `/history/issues` rows,
 * which keep one run per key and would drop every earlier round (and show a running ticket as 0).
 *
 * Split BY PROVIDER rather than blended, because that is the one thing this figure must not do:
 * a ticket whose implementation ran on Fireworks and whose review ran on Anthropic spent two
 * different currencies, and a single blended number would hide exactly the split the ticket exists
 * to show. Buckets are ordered largest first (ties broken by provider name) so the biggest cost is
 * always what a reader sees first. A ticket that has spent nothing has no entry, so a queued row
 * carries no cost line at all rather than "0".
 */
export function ticketCostsByIssue(rows: readonly TicketCostRow[]): Map<string, TicketCost[]> {
  const out = new Map<string, TicketCost[]>();
  for (const r of rows) {
    if (!r.ticket || r.total_tokens <= 0) continue;
    const bucket = { provider: r.provider ?? "", totalTokens: r.total_tokens, estimated: r.usage_estimated };
    const list = out.get(r.ticket);
    if (list) list.push(bucket);
    else out.set(r.ticket, [bucket]);
  }
  for (const list of out.values()) {
    list.sort((a, b) => b.totalTokens - a.totalTokens || a.provider.localeCompare(b.provider));
  }
  return out;
}

/**
 * Newest activity per ticket, from the issue-level history rows: a run's end when it has one,
 * else its start. `mergeJobs` surfaces only the start, and the column says "Updated".
 */
export function lastActivityByIssue(rows: readonly RunSummary[]): Map<string, number> {
  const byIssue = new Map<string, number>();
  for (const r of rows) {
    if (r.issue_identifier === "") continue;
    const at = parseMs(r.ended_at) || parseMs(r.started_at);
    const seen = byIssue.get(r.issue_identifier) ?? 0;
    if (at > seen) byIssue.set(r.issue_identifier, at);
  }
  return byIssue;
}

function parseMs(iso: string): number {
  if (!iso) return 0;
  const ms = Date.parse(iso);
  return Number.isNaN(ms) ? 0 : ms;
}

/**
 * A compact "6m ago" for the Updated column. Returns "—" for an unknown or future instant —
 * a clock skew must read as "no information", never as a negative age.
 */
export function relativeSince(atMs: number, nowMs: number): string {
  if (atMs <= 0) return "—";
  const secs = Math.floor((nowMs - atMs) / 1000);
  if (secs < 0) return "—";
  if (secs < 60) return "just now";
  const mins = Math.floor(secs / 60);
  if (mins < 60) return `${mins}m ago`;
  const hours = Math.floor(mins / 60);
  if (hours < 24) return `${hours}h ago`;
  return `${Math.floor(hours / 24)}d ago`;
}

/** Projects the Select offers — every project present in the rows, de-duplicated, sorted. */
export function consoleJobProjects(
  rows: readonly ConsoleJobRow[],
): { value: string; label: string }[] {
  const bySlug = new Map<string, string>();
  for (const row of rows) {
    if (row.projectSlug !== "" && !bySlug.has(row.projectSlug)) {
      bySlug.set(row.projectSlug, row.project);
    }
  }
  return [...bySlug.entries()]
    .sort((a, b) => a[1].localeCompare(b[1]))
    .map(([value, label]) => ({ value, label }));
}

/** The §3 worklist rows, newest activity first with running tickets pinned to the top. */
export function buildConsoleJobs(
  jobs: readonly JobRow[],
  issueRows: readonly IssueRun[],
  overview: TeamsOverview | undefined,
  nowMs: number,
  costRows: readonly TicketCostRow[] = [],
  dispatchable: DispatchableStates = NO_DISPATCHABLE_STATES,
): ConsoleJobRow[] {
  const durable = durableAssignees(issueRows);
  const live = ticketAssignees(overview);
  const activity = lastActivityByIssue(issueRows);
  const lifecycles = lifecycleByIssue(issueRows);
  const reviewTickets = reviewTicketIssues(issueRows);
  const reviewRuns = reviewRunIssues(issueRows);
  const reviewOf = reviewOfTickets(issueRows);
  const providers = providerByIssue(issueRows);
  const costs = ticketCostsByIssue(costRows);

  const out = jobs.map((job): ConsoleJobRow => {
    const ticket = lifecycles.get(job.issue);
    const reviewTicket = reviewTickets.has(job.issue);
    const reviewRun = reviewRuns.has(job.issue);
    // "Held for a human" speaks for the LANE only when there is no run to name it (STUDIO-949):
    // `mergeJobs` sets `runId` to the newest REAL segment's id and 0 for a synthetic hold row, so
    // 0 is the honest "nothing ran" — never "the daemon could not resolve a lifecycle", which a cold
    // cache serves for most rows. See `consoleJobStatus`.
    const heldNeverRan = (job.heldForHuman ?? false) && job.runId === 0;
    // The same "speaks for the lane only when nothing ran" scope for a budget hold (STUDIO-970): a
    // ticket the dispatcher refused has usually never run, but one with a prior stopped run keeps
    // that run's lane and wears the budget as its sub-label.
    const budgetHeldNeverRan = (job.budgetHeld ?? "") !== "" && job.runId === 0;
    // Dispatchable vs parked (STUDIO-966), resolved against THIS row's project — `projects[].*`
    // overlays differ, so a state parked in one project can be live work in another.
    const parked =
      ticket !== undefined &&
      isParkedState(ticket.trackerState, activeStatesFor(dispatchable, job.project));
    const status = consoleJobStatus(
      job.status,
      ticket?.lifecycle,
      reviewTicket,
      reviewRun,
      heldNeverRan,
      parked,
      budgetHeldNeverRan,
    );
    const updatedAtMs = activity.get(job.issue) ?? job.startedAtMs;
    // The PR the row has always carried in its issue key, surfaced (STUDIO-925). Only a review row
    // has one; a plain ticket key never matches the parser.
    const pr = parsePullRequest(job.issue);
    return {
      key: job.key,
      issue: job.issue,
      runId: job.runId,
      live: job.live,
      title: job.title,
      project: job.projectShort,
      projectSlug: job.project,
      status,
      statusLabel: CONSOLE_STATUS_LABELS[status],
      runOutcome: job.status,
      trackerState: ticket?.trackerState ?? "",
      // The durable record first: it is the only one that survives the run. The live roster is the
      // fallback for the gap at the other end — a run dispatched moments ago, whose history row the
      // daemon has not yet decorated.
      assignee: durable.get(job.issue) ?? live.get(job.issue) ?? "",
      reviewRun,
      pr: pr === undefined ? "" : pullRequestLabel(pr),
      prUrl: pr?.url ?? "",
      provider: providers.get(job.issue) ?? "",
      costs: costs.get(job.issue) ?? [],
      // `updatedAtMs` IS this run's start while it is live (no `ended_at` has landed yet to
      // outrank it), so it doubles as the elapsed clock's zero without a second lookup.
      elapsed: job.live ? liveElapsed(nowMs - updatedAtMs) : "",
      reviewOf: reviewOf.get(job.issue) ?? "",
      updated: relativeSince(updatedAtMs, nowMs),
      updatedAtMs,
      subLabel: job.subLabel,
      budgetHeld: job.budgetHeld,
      // Not when the row already has a `subLabel`: that is the held/failed detail, and on a failed
      // row it IS the error, which says more than "run failed" does. See [`statusNote`].
      statusNote:
        job.subLabel === undefined
          ? statusNote(status, job.status, ticket !== undefined)
          : undefined,
      needsYou: needsOperator(status, job.status),
      lifecycleResolved: ticket !== undefined,
    };
  });

  out.sort((a, b) => {
    // `reviewing` pins beside `run` because it IS a live run — a review ticket with an agent on it.
    // Sorting it down among the parked rows would hide the one thing the pin exists to surface.
    const ar = isLive(a.status) ? 0 : 1;
    const br = isLive(b.status) ? 0 : 1;
    if (ar !== br) return ar - br;
    return b.updatedAtMs - a.updatedAtMs;
  });
  return out;
}

/**
 * §10 box 2.7 — the status Seg.
 *
 * "Running" covers `reviewing` as well as `run` (STUDIO-780): both mean an agent has the ticket and
 * is working it, and the Seg's buckets partition the worklist — a live row that answered to no
 * button would simply vanish from every filter but "All". "In review" deliberately does NOT cover
 * it: that button's whole job is "what is parked and waiting on a person", and a ticket an agent is
 * actively reviewing is the opposite of parked.
 */
export function matchConsoleFilter(row: ConsoleJobRow, filter: ConsoleJobFilterId): boolean {
  return consoleStatusMatches(row.status, filter);
}

/**
 * The same rule over a bare STATUS rather than a row (STUDIO-925): the board's cards are not
 * `ConsoleJobRow`s, but the status Seg above them must still filter them, and a second copy of this
 * predicate is exactly how the Seg and the thing it filters drift apart.
 */
export function consoleStatusMatches(status: ConsoleJobStatus, filter: ConsoleJobFilterId): boolean {
  if (filter === "all") return true;
  if (filter === "run") return isLive(status);
  // `parked` files under Queued (STUDIO-966): the two share a lane and a count, because the
  // daemon's whole-store tally groups by lifecycle alone and cannot split them (see the note on
  // [`ConsoleJobCounts`]). The PILL still says "parked" — this only decides which button reaches
  // the row, and a parked row that answered to no button but "All" would vanish from the Seg.
  if (filter === "queued") return status === "queued" || status === "parked";
  return status === filter;
}

/** Whether a status means an agent is working the ticket right now — `run` or `reviewing`. */
function isLive(status: ConsoleJobStatus): boolean {
  return status === "run" || status === "reviewing";
}

/** §10 box 2.7 — the status Seg and the project Select, applied together. */
export function filterConsoleJobs(
  rows: readonly ConsoleJobRow[],
  filter: ConsoleJobFilterId,
  projectSlug: string,
): ConsoleJobRow[] {
  return rows.filter(
    (row) =>
      matchConsoleFilter(row, filter) && (projectSlug === "" || row.projectSlug === projectSlug),
  );
}

/**
 * The per-status tally behind the Now strip, plus the "Needs you" the design record's §6 adds.
 *
 * `needsYou` deliberately CUTS ACROSS the other four rather than partitioning with them — see
 * [`needsOperator`] — so the five numbers do not sum to the row count and are not meant to.
 *
 * WHAT THE STRIP ACTUALLY PAINTS IS FOUR OF THESE FIVE. §3 gave the strip a "running / in review /
 * queued / blocked" row of pills and §6 added "Needs you" beside them, which put the operator's
 * question on the strip TWICE: measured over the live listing the two agreed exactly — 16 and 16
 * on the default page, 27 and 27 over all 358 rows, with neither difference set holding a single
 * ticket. David's 2026-09-03 decision is that the strip asks it ONCE, so `JobsView` no longer
 * renders an in-review pill and "Needs you" is the home's single human-attention flag.
 *
 * `review` survives HERE because this is the model rather than the strip: it is the honest count
 * of rows reading in-review, it is what STUDIO-702's regression pins (terminal tickets stopped
 * being billed as awaiting a reviewer), and the Seg still filters the table to exactly that set.
 * What was dropped is the second pill, not the number.
 */
export interface ConsoleJobCounts {
  /** Rows an agent is working right now — `run` AND `reviewing` (STUDIO-780). */
  running: number;
  /** Rows reading in-review. Still counted, no longer painted — see the note above. */
  review: number;
  /**
   * Rows waiting to run: `queued` AND `parked` (STUDIO-966). The two share this number because the
   * daemon's tally groups by lifecycle and cannot tell them apart — a parked ticket reaches the
   * client as an `open` bucket exactly as a Todo one does — so splitting them here would put the
   * strip and the table on two different rules, which is the disagreement STUDIO-828 exists to
   * prevent. What the number can say honestly is "not yet running"; the row's PILL says which.
   */
  queued: number;
  blocked: number;
  /**
   * How many tickets are waiting on the operator, or `null` for "the console cannot tell" — which
   * the Now strip renders as "—" rather than as a number.
   *
   * WHY THIS ONE STAT IS NULLABLE AND THE OTHER FOUR ARE NOT. The four above are counts of rows
   * the daemon definitely served. This one is a claim about the OUTSIDE world — what a human still
   * owes — and it is only answerable while the tracker is answering. When `issue_lifecycles`
   * resolves nothing for the payload (a cold cache, a missing tracker, a Linear round-trip that
   * failed), `consoleJobStatus` falls back to inferring "in review" from every `completed` outcome,
   * so `review` INFLATES at the exact moment the console knows least — measured on the live
   * listing, 27 rows became 353. A count taken over those inferred rows would be a number the
   * console invented, and the earlier shape that discounted them instead announced "0 need you",
   * which is the one thing it could not know just then. A number either way is a claim; "—" is the
   * truth. This matters more now that it is the strip's ONLY human-attention flag: there is no
   * neighbouring pill left whose obvious inflation would hint that the tracker had gone quiet.
   *
   * The gate is all-or-nothing over the payload on purpose: the daemon resolves lifecycles for a
   * request as a batch, so a page where NOT ONE row got an answer is the outage shape, while a
   * page where some did is a healthy tracker that merely does not know every ticket. An empty
   * worklist counts as knowable — nothing is waiting because there is nothing.
   */
  needsYou: number | null;
}

/** What the tally needs to know about one ticket — the three derived facts, and nothing else. */
interface CountedJob {
  status: ConsoleJobStatus;
  needsYou: boolean;
  lifecycleResolved: boolean;
}

/**
 * The tally itself, over WEIGHTED entries: `[job, howMany]`.
 *
 * The weight is what lets the two callers below share one accumulator instead of one each. The
 * table has a row per ticket and weighs everything 1; the daemon's whole-store tally arrives
 * pre-grouped, with one entry standing for however many issues carry that exact combination of
 * status inputs. Reconstructing a row per issue just to count them again would be the same
 * arithmetic with a list in the middle.
 */
function tally(entries: Iterable<readonly [CountedJob, number]>): ConsoleJobCounts {
  const counts = { running: 0, review: 0, queued: 0, blocked: 0 };
  let needsYou = 0;
  let heard = false;
  let total = 0;
  for (const [job, weight] of entries) {
    total += weight;
    if (isLive(job.status)) counts.running += weight;
    else if (job.status === "review") counts.review += weight;
    // `parked` is folded into queued (STUDIO-966), deliberately: the daemon's tally groups by
    // lifecycle, so a parked ticket reaches this side as an `open` bucket and there is no way for
    // the store-side count to put it anywhere else. Counting it apart here would split the strip
    // from the table — the exact class of disagreement STUDIO-828 exists to prevent. The row's own
    // pill still distinguishes them.
    else if (job.status === "queued" || job.status === "parked") counts.queued += weight;
    else if (job.status === "blocked") counts.blocked += weight;
    if (job.needsYou) needsYou += weight;
    if (job.lifecycleResolved) heard = true;
  }
  return { ...counts, needsYou: heard || total === 0 ? needsYou : null };
}

/**
 * The tally over the rows a client is HOLDING — the table's own count of what it renders.
 *
 * This is no longer what the Now strip paints. It used to be, and that was defect A of STUDIO-828:
 * a fold over the fetched window grows when the operator clicks "Load more" and cannot report more
 * than the window holds, so it was a count of the client's paging rather than of anything. The
 * strip now reads [`consoleStoreCounts`] off a figure the daemon computes over the store.
 *
 * It survives as the DEFINITION of those five numbers over a set of rows, and it earns its keep as
 * the reference the daemon's grouping is pinned against: a test folds a whole store both ways —
 * through the row pipeline here, and through the daemon's buckets there — and asserts the two
 * agree. That agreement is the acceptance criterion the strip and the table share, and pinning it
 * needs a row-side answer to compare with.
 */
export function consoleJobCounts(rows: readonly ConsoleJobRow[]): ConsoleJobCounts {
  return tally(rows.map((row) => [row, 1] as const));
}

/**
 * The Now strip's five numbers, over EVERY issue in the store — STUDIO-828 defect A.
 *
 * `undefined` in, `undefined` out: before the first response there is no answer, and the strip
 * renders "—" rather than four zeroes it would have to take back. A zero is a claim that the store
 * is empty, which is exactly the sort of number this ticket exists to stop the console inventing.
 *
 * The daemon sends the same per-row facts the issue listing sends, grouped by their distinct
 * combinations, and the rule that turns those facts into a pill is applied HERE — by the same
 * [`consoleJobStatus`] and [`needsOperator`] the table's rows go through. That is deliberate and it
 * is the whole reason the endpoint counts inputs rather than statuses: a count derived by a second
 * implementation of the rule can disagree with the row beside it, and a strip that contradicts its
 * own table is worse than one that is merely stale.
 *
 * A bucket the daemon could not resolve a lifecycle for carries no `lifecycle`, exactly as such a
 * row does, so the "—" gate on `needsYou` reads the payload the same way it read the page: some
 * bucket answered ⇒ the tracker is answering. See [`ConsoleJobCounts.needsYou`].
 *
 * `held` is the ONE thing the daemon's tally cannot supply and the client must add: the live
 * snapshot's held dependents (`state.blocked`, INF-318/INF-320), which the worklist renders as rows
 * with no run behind them at all. Adding it does not reopen defect A, and the reason is worth being
 * exact about: the live snapshot is not a page — it is the complete set of what the daemon is doing
 * right now, at every window width — so folding it in is paging-invariant in a way folding the
 * listing never was. Nor can it double-count: a ticket only reads "waiting" when its whole group is
 * synthetic (`runs-model.jobStatus`), i.e. when it has never run, and a ticket that has never run
 * has no stored row for the daemon's tally to have counted.
 *
 * It is inert on a Rhapsody daemon today — the Rust `Snapshot` carries no held-dependent set, so
 * `/api/v1/state` never sends one — and it is here so that the strip and the table cannot disagree
 * about a row the table already knows how to draw, rather than as a feature.
 *
 * `held_for_human` (STUDIO-949) is NOT a client-side set like `held`, and it is the one figure the
 * client cannot compute: it is a COUNT the daemon serves beside the buckets, and it counts ONLY the
 * holds with no stored row — the never-ran ticket the dispatcher refused, for which the console
 * synthesizes a Queued card. The daemon joins the store rows (which control the ROW) with the
 * snapshot's hold set (which controls WHICH ticket) and reclassifies only that shape, so adding this
 * count to `queued` counts each hold exactly once. A hold that HAS run keeps its stored row's bucket
 * — its card stays in the run's lane with the hold as a sub-label (the STUDIO-939 shape) — and is
 * deliberately NOT in this count; a held ticket the daemon is mid-run on keeps its running bucket
 * and is not in it either, matching the console's own exceptions. Reading
 * `state.held_for_human.length` here instead would reopen the double-count, and treating every hold
 * as queued would move a hold that has run out of its lane.
 *
 * `budget_held` (STUDIO-970) is its sibling for the per-provider budget refusal, and is added to
 * `queued` for exactly the same reasons with one difference worth stating: it is a hold that clears
 * at local midnight and needs nobody, so it must NOT reach `needsYou`. Adding a bare COUNT keeps
 * that true by construction — the count carries no row to score through [`needsOperator`] — which is
 * also why the daemon's join is unavoidable here. A review budget hold is excluded by the daemon
 * (it names a pull request coordinate, not a ticket) before the count is served.
 */
export function consoleStoreCounts(
  payload: IssueCountsResponse | undefined,
  held: readonly BlockedEntry[] = [],
): ConsoleJobCounts | undefined {
  if (payload === undefined) return undefined;
  // A held dependent's status inputs are exactly a `waiting` outcome and nothing else, so it goes
  // through the SAME derivation below rather than being scored separately.
  const buckets: readonly IssueStatusBucket[] =
    held.length === 0
      ? payload.buckets
      : [...payload.buckets, { outcome: "waiting", count: held.length }];
  const counts = tally(
    buckets.map((b) => {
      // `reviewTicket` is always false here: the daemon does not resolve that marker for the tally
      // because it cannot move any of these five numbers — a live review TICKET reads `reviewing`
      // and an ordinary live run reads `run`, and `running` counts both. See `IssueStatusBucket`.
      const status = consoleJobStatus(b.outcome, b.lifecycle, false, b.review_run ?? false);
      return [
        {
          status,
          needsYou: needsOperator(status, b.outcome),
          lifecycleResolved: b.lifecycle !== undefined,
        },
        b.count,
      ] as const;
    }),
  );
  const heldForHuman = payload.held_for_human ?? 0;
  const budgetHeld = payload.budget_held ?? 0;
  if (heldForHuman === 0 && budgetHeld === 0) return counts;
  return { ...counts, queued: counts.queued + heldForHuman + budgetHeld };
}

/** One teammate's live state in the Now strip (§3). */
export interface MateState {
  name: string;
  /** The ticket they are on, or "idle". */
  task: string;
  running: boolean;
}

export function mateStates(overview: TeamsOverview | undefined): MateState[] {
  return (overview?.roster ?? []).map((mate) => {
    const tickets = mate.tickets ?? [];
    return {
      name: mate.name,
      task: tickets.length > 0 ? tickets.join(", ") : "idle",
      running: mate.live_runs > 0,
    };
  });
}

/**
 * How many issues one "Load more" step adds to the worklist's request (STUDIO-792).
 *
 * Deliberately the store's own `DEFAULT_RUN_LIMIT` (crates/store/src/sqlite.rs): the daemon's
 * `next_offset` is derived from the page size it ACTUALLY applied, so stepping by anything else
 * would put the console's idea of a page and the daemon's out of step at every boundary.
 */
export const JOBS_PAGE_SIZE = 50;

/** What the worklist currently holds, for [`consoleJobsPageNote`]. */
export interface ConsoleJobsPage {
  /** Rows the table is built from, before the Seg/project filter — the live overlay included. */
  loaded: number;
  /** Rows the filter leaves on screen. */
  visible: number;
  /** The daemon offered a further page (`next_offset` is not null). */
  hasMore: boolean;
  /** A status or project filter is narrowing the loaded set. */
  filtered: boolean;
}

/**
 * The line under the worklist saying how much of the history it is actually showing — STUDIO-792.
 *
 * The daemon has always served paging and the console has never used it, so the list simply ended
 * at the store's 50 newest and said nothing; on the operator's own daemon that hid 336 of 386
 * tickets. A list that quietly stops is the same class of defect as a status that quietly lies, so
 * this sentence is rendered whenever there are rows — including when nothing is truncated, because
 * "all 386" is what tells the operator the end of the list is the end of the history.
 *
 * `loaded` is the TABLE's row count rather than the page size asked for, so it agrees with what is
 * on screen by construction. The two differ whenever a live ticket is absent from the page being
 * held, which is exactly the "52 for a moment, then back to 50" this ticket came from — see the
 * `mergeJobs over a truncated issue page` reproduction in `runs-model.test.ts`. Counting the rows
 * means the strip and the sentence can never disagree about the same list.
 *
 * The filtered wording is not decoration. A filter runs over the LOADED rows only, so "Done · 3"
 * on a truncated list answers a question the operator did not ask — "3 of the newest 50", not "3
 * in all" — and that has to be said out loud while the tail is missing.
 *
 * `hasMore` restates the daemon's own claim and inherits its one imprecision: a full page means
 * "there may be more", so a history of exactly 50 offers a next page that turns out to be empty.
 * The wording is therefore "not loaded yet" rather than an assertion that older jobs exist — one
 * click settles it and the sentence becomes "Showing all 50 jobs."
 */
export function consoleJobsPageNote(page: ConsoleJobsPage): string {
  if (page.loaded <= 0) return "";
  const noun = page.loaded === 1 ? "job" : "jobs";
  if (page.hasMore) {
    const held = `the ${page.loaded} most recent ${noun}`;
    const shown = page.filtered ? `Showing ${page.visible} of ${held}` : `Showing ${held}`;
    const tail = page.filtered
      ? "Older jobs are not loaded yet, so this filter has not been applied to them."
      : "Older jobs are not loaded yet.";
    return `${shown}. ${tail}`;
  }
  const held = `all ${page.loaded} ${noun}`;
  return page.filtered ? `Showing ${page.visible} of ${held}.` : `Showing ${held}.`;
}
