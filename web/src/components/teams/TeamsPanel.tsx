import * as React from "react";
import { Button, Pill, SectionCard, StatusDot } from "@/components/ui";
import { ConfirmDialog } from "@/components/ui/confirm-dialog";
import { formatDateTime } from "@/lib/format";
import { errText, roomAuthorLine } from "@/lib/teams-model";
import {
  useInvalidateFact,
  useTeamsOverview,
  useTeamsRecall,
  useTeamsRoom,
} from "@/hooks/useTeams";
import { useStateQuery } from "@/hooks/useStateQuery";
import type { TeamsFact, TeamsRosterRow } from "@/lib/api";

// TeamsPanel — the operator's view of the team (STUDIO-652): who is on the roster, what each of
// them is doing right now, what the room recorded, and what each teammate remembers.
//
// It is mounted ONLY when the daemon reports Teams enabled (the `teams_enabled` gate on
// /api/v1/version), so every query below is safe to fire: none of them can reach a daemon that
// would answer `teams_disabled`. On a Teams-off daemon this component never mounts and the app
// makes no request against /api/v1/teams* at all.
//
// Nothing here writes except the invalidate button. Teammates post through `teams_post` (T6), but
// this panel gets no compose box: a post from the UI has no run identity, so the host could only
// stamp it as a *human* post, and that provenance question (design §0.11.4) is still open.
export interface TeamsPanelProps {
  /** Poll cadence, matched to the daemon's own poll interval when known. */
  pollMs?: number;
  /** Open a run's existing detail view — how a live teammate links to what it is doing. */
  onOpenRun?: (runID: number) => void;
  /** Jump to Settings → Teams (the roster is edited in teams.yaml, not here). */
  onOpenSettings?: () => void;
}

export function TeamsPanel({ pollMs, onOpenRun, onOpenSettings }: TeamsPanelProps) {
  const overview = useTeamsOverview(true, pollMs);
  const room = useTeamsRoom(true, 30, pollMs);
  const roster = overview.data?.roster ?? [];
  // Which teammate's memory is open. Defaults to the first identity once the roster arrives, so the
  // panel shows a real bank rather than an empty prompt to pick one.
  const [selected, setSelected] = React.useState("");
  const identity = selected || roster[0]?.name || "";

  return (
    <div style={{ display: "flex", flexDirection: "column", gap: 18 }}>
      <SectionCard
        title="Roster"
        desc={rosterDesc(overview.data?.manager_mode, overview.data?.backend)}
        action={
          onOpenSettings ? (
            <Button type="button" variant="ghost" size="sm" onClick={onOpenSettings}>
              Edit teams.yaml…
            </Button>
          ) : null
        }
      >
        {overview.isError ? (
          <Note>Could not read the roster: {errText(overview.error)}</Note>
        ) : roster.length === 0 ? (
          <Note>
            {overview.isLoading ? "Loading the roster…" : "Teams is on, but the roster is empty."}
          </Note>
        ) : (
          <RosterTable rows={roster} selected={identity} onSelect={setSelected} onOpenRun={onOpenRun} />
        )}
      </SectionCard>

      <SectionCard
        title="The room"
        desc="What the team recorded, newest last. Read-only here — teammates post with `teams_post`, but a post from this panel would have no run identity."
      >
        {room.isError ? (
          <Note>Could not read the room: {errText(room.error)}</Note>
        ) : (room.data?.messages.length ?? 0) === 0 ? (
          <Note>{room.isLoading ? "Loading the room…" : "Nothing has been posted yet — teammates post to the room with `teams_post`."}</Note>
        ) : (
          <div style={{ display: "flex", flexDirection: "column", gap: 10 }}>
            {room.data?.messages.map((m) => (
              <RoomPost key={m.id} authorLine={roomAuthorLine(m, formatDateTime)} body={m.body} refs={m.refs} />
            ))}
          </div>
        )}
        {(room.data?.skipped.length ?? 0) > 0 ? (
          <Note>{room.data?.skipped.length} log line(s) could not be parsed and were skipped.</Note>
        ) : null}
      </SectionCard>

      <MemoryCard identity={identity} />
    </div>
  );
}

function rosterDesc(mode: string | undefined, backend: string | undefined): string {
  if (!mode && !backend) return "Who is on the team, and what each of them is working right now.";
  const assignment =
    mode === "off"
      ? "Assignment is off — every ticket runs as the default identity"
      : mode === "labels+model"
        ? "Assigned by labels, with a triage model turn on a miss"
        : "Assigned by labels";
  const memory = backend === "none" ? "memory off" : `memory: ${backend}`;
  return `${assignment} · ${memory}.`;
}

// RosterTable — one row per identity with the status the daemon derives from its live runs. A
// teammate with a live run links to that run's existing detail view; an idle one has nothing to
// link to and says so rather than offering a dead control.
function RosterTable({
  rows,
  selected,
  onSelect,
  onOpenRun,
}: {
  rows: TeamsRosterRow[];
  selected: string;
  onSelect: (name: string) => void;
  onOpenRun?: (runID: number) => void;
}) {
  return (
    <table style={{ width: "100%", borderCollapse: "collapse", fontSize: 12.5 }}>
      <thead>
        <tr style={{ textAlign: "left", color: "var(--tx-3)" }}>
          {["Teammate", "Profile", "Labels", "Bank", "Now"].map((h) => (
            <th key={h} style={{ padding: "6px 8px", fontWeight: 500, borderBottom: "1px solid var(--line-2)" }}>
              {h}
            </th>
          ))}
        </tr>
      </thead>
      <tbody>
        {rows.map((r) => (
          <tr key={r.name} style={{ borderBottom: "1px solid var(--line-2)" }}>
            <td style={{ padding: "8px" }}>
              <button
                type="button"
                aria-pressed={selected === r.name}
                aria-label={`Show ${r.name}'s memory`}
                onClick={() => onSelect(r.name)}
                title={`Show ${r.name}'s memory`}
                style={{
                  display: "inline-flex",
                  alignItems: "center",
                  gap: 7,
                  background: "transparent",
                  border: "none",
                  padding: 0,
                  cursor: "pointer",
                  fontSize: 12.5,
                  fontWeight: selected === r.name ? 600 : 400,
                  color: selected === r.name ? "var(--rust-text)" : "var(--tx)",
                }}
              >
                <StatusDot color={r.live_runs > 0 ? "var(--emerald)" : "var(--tx-faint)"} size={6} pulse={r.live_runs > 0} />
                {r.name}
              </button>
            </td>
            <td style={{ padding: "8px", color: "var(--tx-3)" }}>{r.profile || "—"}</td>
            <td style={{ padding: "8px" }}>
              {r.labels.length === 0 ? (
                <span style={{ color: "var(--tx-faint)" }}>—</span>
              ) : (
                <span style={{ display: "inline-flex", gap: 5, flexWrap: "wrap" }}>
                  {r.labels.map((l) => (
                    <Pill key={l} tone="slate">
                      {l}
                    </Pill>
                  ))}
                </span>
              )}
            </td>
            <td className="mono" style={{ padding: "8px", color: "var(--tx-faint)", fontSize: 11 }}>
              {r.bank || "—"}
            </td>
            <td style={{ padding: "8px" }}>
              <LiveCell row={r} onOpenRun={onOpenRun} />
            </td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}

// LiveCell renders the derived status. The daemon reports the tickets a teammate's live runs are
// working, not their run ids, so the link is offered only when the host can resolve one — which is
// why `onOpenRun` is looked up against the ticket the operator clicked, in the shell.
function LiveCell({ row, onOpenRun }: { row: TeamsRosterRow; onOpenRun?: (runID: number) => void }) {
  if (row.live_runs === 0) return <span style={{ color: "var(--tx-faint)" }}>idle</span>;
  return (
    <span style={{ display: "inline-flex", alignItems: "center", gap: 8, flexWrap: "wrap" }}>
      <span style={{ color: "var(--tx-3)" }}>
        {row.live_runs} live
      </span>
      {row.tickets.map((t) => (
        <TicketLink key={t} ticket={t} onOpenRun={onOpenRun} />
      ))}
    </span>
  );
}

// TicketLink opens the run detail for a ticket a teammate is working. The roster reports the
// TICKET, so the shell resolves it to the live run id from the state snapshot it already polls —
// there is no second fetch, and a ticket the snapshot cannot place renders as plain text rather
// than a link that would go nowhere.
function TicketLink({ ticket, onOpenRun }: { ticket: string; onOpenRun?: (runID: number) => void }) {
  const runID = useRunIDForTicket(ticket);
  if (!onOpenRun || runID === 0) {
    return (
      <Pill tone="sky" title="No live run detail available for this ticket">
        {ticket}
      </Pill>
    );
  }
  return (
    <button
      type="button"
      onClick={() => onOpenRun(runID)}
      aria-label={`Open the run for ${ticket}`}
      style={{ background: "transparent", border: "none", padding: 0, cursor: "pointer" }}
    >
      <Pill tone="rust">{ticket} ↗</Pill>
    </button>
  );
}

// A room post, rendered as QUOTED, attributed data (design §0.11.5). Room content is untrusted —
// it reaches every teammate's turn-1 prompt — so it never renders as bare prose that could read as
// the app instructing the operator.
function RoomPost({ authorLine, body, refs }: { authorLine: string; body: string; refs: string[] }) {
  return (
    <blockquote
      style={{
        margin: 0,
        borderLeft: "2px solid var(--line-strong)",
        padding: "2px 0 2px 12px",
        display: "flex",
        flexDirection: "column",
        gap: 4,
      }}
    >
      <div style={{ fontSize: 11.5, color: "var(--tx-faint)" }}>{authorLine}</div>
      <div style={{ fontSize: 12.5, color: "var(--tx)", whiteSpace: "pre-wrap", lineHeight: 1.5 }}>{body}</div>
      {refs.length > 0 ? (
        <div style={{ display: "flex", gap: 5, flexWrap: "wrap" }}>
          {refs.map((r) => (
            <Pill key={r} tone="slate">
              {r}
            </Pill>
          ))}
        </div>
      ) : null}
    </blockquote>
  );
}

// MemoryCard — what one teammate remembers, with the invalidate button design §5.2.3 named and
// deferred: "reachable at the moment someone notices", now literally a button.
//
// The listing is an EMPTY recall query, which the daemon reads as "everything, bounded by
// recall_top_k" — a wrong fact has to be visible before it can be corrected.
function MemoryCard({ identity }: { identity: string }) {
  const recall = useTeamsRecall(identity, "", identity !== "");
  const facts = recall.data?.facts ?? [];
  return (
    <SectionCard
      title={identity ? `What ${identity} remembers` : "Memory"}
      desc="Everything in this teammate's bank, newest first and bounded by recall_top_k. A fact that was never true is one reasoned click from gone."
    >
      {identity === "" ? (
        <Note>Pick a teammate in the roster to see their memory.</Note>
      ) : recall.isError ? (
        <Note>Could not read the bank: {errText(recall.error)}</Note>
      ) : facts.length === 0 ? (
        <Note>{recall.isLoading ? "Loading…" : `${identity} has not recorded anything yet.`}</Note>
      ) : (
        <div style={{ display: "flex", flexDirection: "column", gap: 12 }}>
          {facts.map((f) => (
            <FactRow key={f.id} fact={f} />
          ))}
        </div>
      )}
      {(recall.data?.skipped.length ?? 0) > 0 ? (
        <Note>{recall.data?.skipped.length} bank record(s) could not be read and were skipped.</Note>
      ) : null}
    </SectionCard>
  );
}

// FactRow — one recalled record with its provenance and the invalidate control. The reason is
// REQUIRED (the daemon rejects a reasonless invalidate, and a correction nobody can read is worse
// than none), and the confirm step is what makes it a deliberate act rather than a stray click.
function FactRow({ fact }: { fact: TeamsFact }) {
  const [reason, setReason] = React.useState("");
  const [confirming, setConfirming] = React.useState(false);
  const invalidate = useInvalidateFact();
  const trimmed = reason.trim();

  return (
    <div
      style={{
        border: "1px solid var(--line-2)",
        borderRadius: "var(--r-card)",
        padding: 12,
        display: "flex",
        flexDirection: "column",
        gap: 8,
      }}
    >
      <div style={{ display: "flex", gap: 6, flexWrap: "wrap", alignItems: "center" }}>
        {fact.ticket ? <Pill tone="sky">{fact.ticket}</Pill> : null}
        {fact.run_id ? <Pill tone="slate">run {fact.run_id}</Pill> : null}
        {fact.commit_sha ? (
          <Pill tone="slate" title={fact.commit_sha}>
            {fact.commit_sha.slice(0, 7)}
          </Pill>
        ) : null}
        <span style={{ fontSize: 11.5, color: "var(--tx-faint)" }}>{formatDateTime(fact.at)}</span>
      </div>
      {/* Recalled content is untrusted, exactly like a room post, so it is quoted rather than
          rendered as prose the app appears to be asserting. */}
      <blockquote
        style={{
          margin: 0,
          borderLeft: "2px solid var(--line-strong)",
          padding: "2px 0 2px 12px",
          fontSize: 12.5,
          color: "var(--tx)",
          whiteSpace: "pre-wrap",
          lineHeight: 1.5,
        }}
      >
        {fact.content}
      </blockquote>
      <div style={{ display: "flex", gap: 8, alignItems: "center", flexWrap: "wrap" }}>
        <input
          type="text"
          value={reason}
          onChange={(e) => setReason(e.target.value)}
          placeholder="Why is this wrong?"
          aria-label={`Reason for invalidating ${fact.id}`}
          style={{
            flex: 1,
            minWidth: 200,
            fontSize: 12,
            padding: "6px 9px",
            borderRadius: "var(--r-ctrl)",
            border: "1px solid var(--hair-control)",
            background: "var(--bg-input, rgba(255,255,255,.03))",
            color: "var(--tx)",
          }}
        />
        <Button
          type="button"
          variant="danger"
          size="sm"
          // Disabled without a reason: the daemon refuses one anyway, and the disabled state says
          // WHY up front rather than after a round-trip.
          disabled={trimmed === "" || invalidate.isPending}
          title={trimmed === "" ? "A reason is required" : "Invalidate this memory"}
          onClick={() => setConfirming(true)}
        >
          Invalidate
        </Button>
      </div>
      {invalidate.isError ? <Note tone="red">{errText(invalidate.error)}</Note> : null}
      <ConfirmDialog
        open={confirming}
        title="Invalidate this memory?"
        body={`${fact.identity} will stop recalling it. Nothing is deleted — the record and your reason stay on disk, and it can be restored. Reason: "${trimmed}"`}
        confirmLabel="Invalidate"
        danger
        busy={invalidate.isPending}
        onClose={() => setConfirming(false)}
        onConfirm={() => {
          invalidate.mutate(
            { identity: fact.identity, factID: fact.id, reason: trimmed },
            { onSuccess: () => setConfirming(false) },
          );
        }}
      />
    </div>
  );
}

function Note({ children, tone }: { children: React.ReactNode; tone?: "red" }) {
  return (
    <div style={{ fontSize: 12.5, color: tone === "red" ? "var(--red)" : "var(--tx-3)", padding: "6px 0" }}>
      {children}
    </div>
  );
}

// useRunIDForTicket resolves a ticket identifier to its live run id from the state snapshot the
// shell already polls — no second request, and no new endpoint. Zero when the snapshot does not
// name it (the ticket finished, or persistence is off and there is no run row to open).
function useRunIDForTicket(ticket: string): number {
  const { data } = useStateQuery();
  return React.useMemo(
    () => data?.running.find((r) => r.issue_identifier === ticket)?.run_id ?? 0,
    [data, ticket],
  );
}
