import * as React from "react";
import {
  NowStats,
  Note,
  Seg,
  Select,
  Stat,
  TeammateAvatar,
  TicketChip,
  Timestamp,
} from "@/components/console";
import { SearchIcon } from "@/components/console/teams/icons";
import { CrossGlyph } from "@/components/console/views/glyphs";
import { useMemoryBanks } from "@/hooks/useMemoryBanks";
import { useInvalidateFact, useTeamsOverview } from "@/hooks/useTeams";
import { formatDateTime } from "@/lib/format";
import {
  ANY,
  STATE_INVALIDATED,
  STATE_VALID,
  bankFacts,
  bankStats,
  factKey,
  filterFacts,
  isInvalidated,
  sortFacts,
  teammateOptions,
  ticketOptions,
  withState,
  type MemoryBank,
  type MemoryFilter,
  type MemorySort,
  type MemoryStateFilter,
} from "@/lib/memory-model";
import { errText } from "@/lib/teams-model";
import { teammateColor } from "@/theme/teammates";
import { cn } from "@/lib/utils";
import type { TeamsFact } from "@/lib/api";
import "@/theme/memory.css";

// The Memory page — STUDIO-681 §6, the fourth slice of the dashboard redesign.
//
// Reachable only when the daemon reports `teams_enabled` (§2.2), so every read below is safe to
// fire. It uses three routes that already exist and invents none (§11):
//
//   GET  /api/v1/teams              roster (bank names, color order) + the memory backend
//   GET  /api/v1/teams/recall       one identity's bank; an EMPTY query browses it
//   POST /api/v1/teams/invalidate   the §5.3 correction, with its reason
//
// TWO reads §6 asks for have NO endpoint on the current daemon, confirmed against
// `crates/httpapi/src/server.rs` and `crates/config/src/memory.rs`:
//
//   1. Recall serves VALID records only (`memory.rs`: `if fact.state != STATE_VALID { continue }`),
//      so a bank's invalidated records cannot be listed at all. This page therefore shows the
//      records invalidated in THIS SESSION — enough for the correction to be visible and undone
//      where it was made, and honest about the rest (the note below says so on screen).
//   2. There is no reinstate route. `LocalBank::revalidate` exists and is documented as "the
//      reversal §5.3 requires", but it is on neither the `MemoryBackend` trait nor any HTTP or MCP
//      surface. `onReinstate` is the seam it plugs into; unwired, the button reports the gap
//      rather than faking a restore the bank on disk would contradict.
//
// Both are dependency tickets, not things to invent here.

/** How the reinstate control explains itself when the daemon has no route behind it. */
const NO_REINSTATE =
  "This daemon has no reinstate endpoint yet, so the record is still invalidated in the bank. " +
  "Reversing it needs a daemon route over the bank's existing revalidate (STUDIO-681 §6 dependency).";

const STATE_OPTIONS: readonly { value: MemoryStateFilter; label: string }[] = [
  { value: ANY, label: "All" },
  { value: STATE_VALID, label: "Valid" },
  { value: STATE_INVALIDATED, label: "Invalidated" },
];

const SORT_OPTIONS: readonly { value: MemorySort; label: string }[] = [
  { value: "newest", label: "Newest" },
  { value: "oldest", label: "Oldest" },
];

/** How much of a commit SHA the chip shows — git's own abbreviation. */
const SHORT_SHA = 7;

export interface MemoryViewProps {
  /** Route to a fact's ticket — the card's "View run" (§2.3 has no run route of its own). */
  onNavigate: (route: "job", key: string) => void;
  /**
   * Put a record back into recall (box 4.6).
   *
   * Optional because the daemon has no route for it yet (see the module note): unwired, Reinstate
   * still renders and still explains itself, and the fact stays invalidated rather than appearing
   * to come back. Supplying this is a one-line change once the endpoint lands.
   */
  onReinstate?: (fact: TeamsFact) => Promise<void>;
}

export function MemoryView({ onNavigate, onReinstate }: MemoryViewProps) {
  const [search, setSearch] = React.useState("");
  const [who, setWho] = React.useState<string>(ANY);
  const [ticket, setTicket] = React.useState<string>(ANY);
  const [state, setState] = React.useState<MemoryStateFilter>(ANY);
  const [sort, setSort] = React.useState<MemorySort>("newest");

  const overview = useTeamsOverview(true);
  const roster = React.useMemo(() => overview.data?.roster ?? [], [overview.data]);
  const names = React.useMemo(() => roster.map((r) => r.name), [roster]);
  const read = useMemoryBanks(names);

  // What the operator changed in this session, keyed by bank+record. It is an OVERLAY rather than
  // an edit of the query cache because a refetch would drop it: recall cannot return an
  // invalidated record, so the correction would erase the very card the operator is looking at.
  const [session, setSession] = React.useState<Record<string, TeamsFact>>({});
  const banks = React.useMemo<MemoryBank[]>(
    () =>
      read.banks.map((b) => ({
        ...b,
        facts: b.facts.map((f) => session[factKey(f)] ?? f),
      })),
    [read.banks, session],
  );

  const facts = React.useMemo(() => bankFacts(banks), [banks]);
  const stats = React.useMemo(() => bankStats(banks), [banks]);
  const filter: MemoryFilter = { search, who, ticket, state };
  const shown = React.useMemo(
    () => sortFacts(filterFacts(facts, filter), sort),
    // `filter` is rebuilt every render; its four fields are the real inputs.
    [facts, search, who, ticket, state, sort],
  );

  const bankOf = React.useCallback(
    (identity: string) => roster.find((r) => r.name === identity)?.bank ?? identity,
    [roster],
  );
  const record = (fact: TeamsFact) => setSession((s) => ({ ...s, [factKey(fact)]: fact }));

  const readError = overview.isError ? overview.error : read.error;

  return (
    // `.rh-console` is normally inherited from AppShell; repeated so the view is also correct
    // rendered on its own (a test, a gallery route) — the same rule the Teams console follows.
    <section className="rh-console">
      <div className="head">
        <h1>Memory</h1>
        <span className="sub">
          agent banks · <code>{overview.data?.backend ?? "…"}</code>
        </span>
      </div>
      <p className="lead">
        What each teammate carries between runs. Host-stamped on write, recalled at turn 1 bounded
        by <code>recall_top_k</code>. A fact that was never true is one reasoned click from gone —
        and invalidation is reversible.
      </p>

      <NowStats className="memstats">
        <Stat value={stats.facts} label="facts" />
        <Stat value={stats.valid} label="valid" />
        <Stat value={stats.invalidated} label="invalidated" tone="bad" />
        <Stat value={stats.banks} label="banks" />
      </NowStats>

      {/*
        Said on screen rather than only in a comment: a page that quietly listed three of a bank's
        five records would read as the whole bank. Both halves are real limits of the daemon this
        page is talking to, and both disappear when the dependency lands.
      */}
      <Note className="memnote">
        A browse is bounded by <code>recall_top_k</code>, and recall serves valid records only — so
        an invalidated record is listed here while the correction is fresh, and is not read back on
        a reload.
      </Note>

      {/*
        A read failure is a line, not a Note: `Note` is the §1.3 inline callout and announces
        itself as one, and the Teams console already reports a failed roster read this way.
      */}
      {readError === undefined || readError === null ? null : (
        <div className="memerr" role="alert">
          Could not read every bank: {errText(readError)}
        </div>
      )}

      <div className="bar">
        <label className="srch">
          <SearchIcon width={14} height={14} />
          <input
            type="text"
            aria-label="Search facts"
            placeholder="Search facts — text, ticket, SHA…"
            value={search}
            onChange={(e) => setSearch(e.target.value)}
          />
        </label>
        {/* Selects, not a chip per teammate or ticket (§6): both axes scale to N. */}
        <Select
          aria-label="Filter memory by teammate"
          value={who}
          onChange={(e) => setWho(e.target.value)}
          options={[
            { value: ANY, label: "All teammates" },
            ...teammateOptions(banks).map((n) => ({ value: n })),
          ]}
        />
        <Select
          aria-label="Filter memory by ticket"
          value={ticket}
          onChange={(e) => setTicket(e.target.value)}
          options={[
            { value: ANY, label: "All tickets" },
            ...ticketOptions(facts).map((t) => ({ value: t })),
          ]}
        />
        <Seg
          aria-label="Filter memory by state"
          options={STATE_OPTIONS}
          value={state}
          onChange={(v) => setState(v as MemoryStateFilter)}
        />
        <Select
          aria-label="Sort memory"
          value={sort}
          onChange={(e) => setSort(e.target.value as MemorySort)}
          options={SORT_OPTIONS}
        />
      </div>

      <div className="facts">
        {shown.length === 0 ? (
          <div className="empty">
            {read.isPending || overview.isPending
              ? "Reading the banks…"
              : facts.length === 0
                ? "No facts retained yet."
                : "No facts match these filters."}
          </div>
        ) : (
          shown.map((fact) => (
            <FactCard
              key={factKey(fact)}
              fact={fact}
              bank={bankOf(fact.identity)}
              backend={overview.data?.backend ?? ""}
              color={teammateColor(names, fact.identity)}
              onViewRun={() => onNavigate("job", fact.ticket)}
              onInvalidated={record}
              onReinstate={onReinstate}
              onReinstated={record}
            />
          ))
        )}
      </div>
    </section>
  );
}

interface FactCardProps {
  fact: TeamsFact;
  /** The bank directory the record lives in — the provenance line's middle term. */
  bank: string;
  backend: string;
  color: string;
  onViewRun: () => void;
  onInvalidated: (fact: TeamsFact) => void;
  onReinstate: ((fact: TeamsFact) => Promise<void>) | undefined;
  onReinstated: (fact: TeamsFact) => void;
}

function FactCard({
  fact,
  bank,
  backend,
  color,
  onViewRun,
  onInvalidated,
  onReinstate,
  onReinstated,
}: FactCardProps) {
  const [armed, setArmed] = React.useState(false);
  const [reason, setReason] = React.useState("");
  const [failed, setFailed] = React.useState("");
  const invalidate = useInvalidateFact();
  const dead = isInvalidated(fact);
  const canSubmit = reason.trim() !== "" && !invalidate.isPending;

  const disarm = () => {
    setArmed(false);
    setReason("");
    setFailed("");
  };

  const confirm = () => {
    if (!canSubmit) return;
    const why = reason.trim();
    setFailed("");
    invalidate.mutate(
      { identity: fact.identity, factID: fact.id, reason: why },
      {
        onSuccess: () => {
          onInvalidated(withState(fact, STATE_INVALIDATED, why));
          disarm();
        },
        // The record is untouched on disk when the daemon refuses, so the card must stay valid:
        // dimming it here would tell the operator a correction landed that did not.
        onError: (e) => setFailed(errText(e)),
      },
    );
  };

  const reinstate = () => {
    setFailed("");
    if (onReinstate === undefined) {
      setFailed(NO_REINSTATE);
      return;
    }
    void onReinstate(fact).then(
      () => onReinstated(withState(fact, STATE_VALID, "")),
      (e: unknown) => setFailed(errText(e)),
    );
  };

  return (
    <article className={cn("fact", dead && "dead")} data-fact={factKey(fact)}>
      {dead ? (
        <div className="deadbanner" role="status">
          <CrossGlyph width={13} height={13} />
          <span className="rs">
            Invalidated{fact.reason === "" ? "." : ` — “${fact.reason}”`}
          </span>
          <button type="button" className="rein" onClick={reinstate}>
            Reinstate
          </button>
        </div>
      ) : null}

      <div className="top">
        <span className="who" style={{ color }}>
          <TeammateAvatar color={color} />
          {fact.identity}
        </span>
        {fact.ticket === "" ? null : <TicketChip>{fact.ticket}</TicketChip>}
        {fact.run_id === "" ? null : <TicketChip variant="sha">run {fact.run_id}</TicketChip>}
        {fact.commit_sha === "" ? null : (
          <TicketChip variant="sha">{fact.commit_sha.slice(0, SHORT_SHA)}</TicketChip>
        )}
        {fact.pr === "" ? null : <TicketChip variant="pr">{fact.pr}</TicketChip>}
        <span className="rt">
          {backend === "" ? null : (
            <span className={cn("badge", backend === "local" && "local")}>{backend}</span>
          )}
          <Timestamp>{formatDateTime(fact.at)}</Timestamp>
        </span>
      </div>

      {/* Untrusted content, same as a room post: rendered as text, never as markup. */}
      <div className="body">{fact.content}</div>

      <div className="foot">
        <span className="prov">
          host-stamped · {bank} · {dead ? STATE_INVALIDATED : STATE_VALID}
        </span>
        <div className="acts">
          {fact.ticket === "" ? null : (
            <button type="button" className="ghost" onClick={onViewRun}>
              View run {fact.run_id}
            </button>
          )}
          {dead || armed ? null : (
            <button type="button" className="ghost danger" onClick={() => setArmed(true)}>
              Invalidate
            </button>
          )}
        </div>
      </div>

      {armed ? (
        <div className="invrow">
          <input
            type="text"
            aria-label="Why is this wrong?"
            placeholder="Why is this wrong? (kept as the reason)"
            value={reason}
            onChange={(e) => setReason(e.target.value)}
          />
          <button type="button" className="ghost" onClick={disarm}>
            Cancel
          </button>
          {/*
            Disabled without a reason, because `POST /api/v1/teams/invalidate` rejects a reasonless
            one — the correction is only useful if whoever finds it later can read why.
          */}
          <button
            type="button"
            className="ghost danger"
            disabled={!canSubmit}
            onClick={confirm}
            aria-label="Confirm invalidate"
          >
            {invalidate.isPending ? "Invalidating…" : "Invalidate"}
          </button>
        </div>
      ) : null}

      {failed === "" ? null : (
        <div className="facterr" role="alert">
          {failed}
        </div>
      )}
    </article>
  );
}
