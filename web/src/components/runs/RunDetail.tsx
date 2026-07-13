import * as React from "react";
import { StatusChip } from "@/components/ui/status-chip";
import { StatusDot } from "@/components/ui/status-dot";
import { Button } from "@/components/ui/button";
import { ArrowLeft, RotateCcw, Square } from "@/components/ui/icons";
import { useStopRun, useResumeRun, useSendRunMessage } from "@/hooks/useRunActions";
import { useToast } from "@/components/shell/Toast";
import type { LinearProject, LogEntry, RunMessage, RunSummary } from "@/lib/api";
import { elapsedSeconds, formatDateTime, formatDuration, formatTokens, runDuration } from "@/lib/format";
import { repoShortName } from "@/lib/project";
import { isMcpTool, outcomeToStatus, resolveAgent, transcriptEntryType } from "@/lib/runs-model";
import { useNow } from "@/hooks/useNow";
import { useFollowScroll } from "@/hooks/useFollowScroll";
import { useIssueHistory, useRunDetail, useRunMessages, useTranscript } from "@/hooks/useRunDetail";
import { useLinearIdentity } from "@/hooks/useConfig";
import { openExternal } from "@/lib/bindings";
import { Panel } from "./Panel";

export interface RunDetailProps {
  runId: number;
  /** Fetched Linear projects, for agent name resolution (header "{agent} · attempt N"). */
  projects: LinearProject[];
  /** Max turns per run (global config) → the "N/max" turn cell + transcript footer. Omitted → bare N. */
  maxTurns?: number;
  /** When false (e.g. under the Wails host, where the daemon HTTP API is unreachable from the
   * asset-server origin) the detail queries stay idle instead of polling a dead origin. */
  enabled?: boolean;
  onBack: () => void;
  onSelectRun: (runId: number) => void;
}

const MONO = "var(--font-mono)";

// RunDetail — the Podium Run detail view (mock 1d): a full-bleed header (← Jobs, run key, status
// pill, agent · attempt, Stop-run danger w/ inline confirm, Open ticket), the six-cell meta strip,
// and the "Agent output" transcript card (turn dividers, prose, → Tool chips, ← tool returns, a
// blinking streaming cursor + follow mode), with a stopped/failed banner above the transcript. The
// operator-message composer/timeline and the per-attempt run history are preserved below. Self-fetches
// the unified run detail, transcript (streaming while running), messages, and per-attempt history;
// polling is outcome-driven (poll while running, freeze when terminal).
export function RunDetail({ runId, projects, maxTurns, enabled = true, onBack, onSelectRun }: RunDetailProps) {
  const { data, isLoading, isError, error } = useRunDetail(runId, enabled);
  const nowMs = useNow(1000);
  const inFlight = data?.outcome === "running";
  // `streaming` gates the live chrome (pulse dot, "streaming" chip, blinking cursor, follow mode,
  // live duration accent). It requires both a running outcome AND active polling: when polling is
  // disabled/paused (`enabled` false) the detail is a frozen cached snapshot, so the stream is not
  // live. The run-state pill still reads "playing" regardless — only the live affordances are gated.
  const streaming = !!inFlight && enabled;
  const transcriptQ = useTranscript(runId, !!inFlight, enabled);
  const messagesQ = useRunMessages(runId, !!inFlight, enabled);
  const issueHistory = useIssueHistory(data?.issue_identifier ?? "", enabled);
  // The ticket — not any single PR — is the stable target: one issue can spawn a whole Graphite
  // stack of PRs. Build the Linear deep link from the connected workspace's slug + the identifier.
  const workspaceURLKey = useLinearIdentity().data?.workspace_url_key ?? "";

  // Run actions: a danger Stop (inline-confirmed) while running, a primary Resume on a stopped run.
  // Both toast their result; the success toast distinguishes a clean Backlog/Todo move from a
  // killed-but-not-moved run (so the operator can move the ticket by hand).
  const { toast } = useToast();
  const stop = useStopRun(runId);
  const resume = useResumeRun(runId);
  const [confirmStop, setConfirmStop] = React.useState(false);
  const stopped = data?.outcome === "stopped";

  const doStop = () => {
    setConfirmStop(false);
    stop.mutate(undefined, {
      onSuccess: (r) =>
        toast(
          "Agent stopped",
          r.move_error
            ? `Killed, but couldn't move the ticket: ${r.move_error}`
            : `Moved ${r.identifier} to ${r.moved_to ?? "Backlog"}.`,
        ),
      onError: (e) => toast("Stop failed", e.message),
    });
  };
  const doResume = () =>
    resume.mutate(undefined, {
      onSuccess: (r) =>
        r.move_error
          ? toast("Resume failed", `Couldn't move ${r.identifier} to Todo: ${r.move_error}`)
          : toast("Resumed", `Moved ${r.identifier} to ${r.moved_to ?? "Todo"} — the agent will pick it back up.`),
      onError: (e) => toast("Resume failed", e.message),
    });

  const scrollRef = React.useRef<HTMLDivElement>(null);
  const entries = transcriptQ.data?.entries ?? [];
  const entryCount = entries.length;
  // Follow mode: stick to the bottom while streaming; an upward scroll pauses following (and shows
  // "jump to latest ↓"), reaching the bottom (or clicking it) resumes. Only auto-pins while streaming,
  // so a finished transcript opens at its natural top. Shared shape with D6's logs follow.
  const follow = useFollowScroll(scrollRef, entryCount, streaming);

  if (isLoading || !data) {
    return (
      <BackShell onBack={onBack}>
        <div style={{ padding: "40px 22px", textAlign: "center", color: "var(--faint)", fontSize: 13 }}>
          {isError ? `Failed to load run: ${(error as Error)?.message ?? "unknown error"}` : "Loading run…"}
        </div>
      </BackShell>
    );
  }

  const status = outcomeToStatus(data.outcome);
  const agent = resolveAgent(data.project, data.repo, projects);
  const attempts = sortAttempts(issueHistory.data?.runs ?? []);
  const durationText = inFlight
    ? formatDuration(elapsedSeconds(data.started_at, nowMs))
    : runDuration(data.started_at, data.ended_at);
  const estimated = data.usage_estimated || !!inFlight;
  const turnText = maxTurns ? `${data.turn_count}/${maxTurns}` : String(data.turn_count);
  // Branch/PR isn't on the RunDetail payload; the current run's history row carries the branch, so
  // the meta cell surfaces it when the per-issue history has resolved, else an em dash.
  const branch = attempts.find((r) => r.id === runId)?.branch?.trim();
  const linearHref = workspaceURLKey && data.issue_identifier
    ? `https://linear.app/${workspaceURLKey}/issue/${data.issue_identifier}`
    : "";

  const tokensValue = (
    <span style={{ display: "inline-flex", alignItems: "center", gap: 7, minWidth: 0 }}>
      <span className="mono" style={{ fontSize: 12, color: "var(--text-2)" }}>{formatTokens(data.total_tokens)}</span>
      {estimated ? <EstBadge /> : null}
    </span>
  );

  const metaCells: { label: string; value: React.ReactNode; accent?: boolean }[] = [
    { label: "Repo", value: repoShortName(data.repo) },
    { label: "Turn", value: turnText },
    { label: "Tokens", value: tokensValue },
    { label: "Started", value: formatDateTime(data.started_at) },
    { label: "Duration", value: durationText, accent: streaming },
    { label: "Branch", value: branch || "—" },
  ];

  return (
    <div style={{ minWidth: 0 }}>
      {/* header (mock 1d): ← Jobs · run key · status pill · agent · attempt / title, with run actions */}
      <div style={{ padding: "14px 20px 0" }}>
        <div style={{ display: "flex", alignItems: "flex-start", justifyContent: "space-between", gap: 16 }}>
          <div style={{ minWidth: 0 }}>
            <div style={{ display: "flex", alignItems: "center", gap: 11, flexWrap: "wrap" }}>
              <Button type="button" variant="subtle" size="sm" icon={ArrowLeft} onClick={onBack}>
                Jobs
              </Button>
              <h1
                className="mono"
                style={{ fontSize: 15, fontWeight: 600, letterSpacing: "-0.01em", whiteSpace: "nowrap" }}
              >
                {data.issue_identifier}
              </h1>
              <StatusChip status={status} />
              <span style={{ fontSize: 12, color: "var(--faint)", whiteSpace: "nowrap" }}>
                {agent.name} · attempt {data.attempt + 1}
              </span>
            </div>
            {data.title ? (
              <p style={{ fontSize: 13, color: "var(--text-muted)", marginTop: 6, maxWidth: 640 }}>{data.title}</p>
            ) : null}
          </div>
          {/* Run-level actions. Stop (running) inline-confirms then kills the agent + moves the ticket
              to Backlog; Resume (stopped) becomes the primary action and moves it back to Todo. "Open
              ticket" is always present. */}
          <div style={{ display: "flex", gap: 8, flexShrink: 0 }}>
            {inFlight ? (
              <Button
                type="button"
                variant="danger"
                icon={Square}
                disabled={stop.isPending}
                onClick={() => (confirmStop ? doStop() : setConfirmStop(true))}
                // Disarm the inline confirm when the button loses focus (click/tab away), so an armed
                // "Stop …?" is never a one-way trap — the deliberate second click keeps focus and fires.
                onBlur={() => setConfirmStop(false)}
              >
                {confirmStop ? `Stop ${data.issue_identifier}?` : "Stop run"}
              </Button>
            ) : null}
            {stopped ? (
              <Button type="button" variant="primary" icon={RotateCcw} disabled={resume.isPending} onClick={doResume}>
                Resume
              </Button>
            ) : null}
            <Button
              type="button"
              variant="subtle"
              disabled={!linearHref}
              onClick={() => openExternal(linearHref)}
            >
              Open ticket <span style={{ color: "var(--faint)" }}>↗</span>
            </Button>
          </div>
        </div>
      </div>

      {/* six-cell meta strip: Repo · Turn · Tokens · Started · Duration · Branch */}
      <div
        style={{
          background: "var(--meta-strip)",
          borderTop: "1px solid var(--hair-section)",
          borderBottom: "1px solid var(--hair-section)",
          padding: "14px 22px",
          marginTop: 14,
        }}
      >
        <div style={{ display: "grid", gridTemplateColumns: "repeat(6, minmax(0, 1fr))", gap: 36 }}>
          {metaCells.map((c) => (
            <div key={c.label} style={{ display: "flex", flexDirection: "column", gap: 6, minWidth: 0 }}>
              <span
                style={{
                  fontSize: 9.5,
                  fontWeight: 600,
                  letterSpacing: ".08em",
                  textTransform: "uppercase",
                  color: "var(--faint)",
                }}
              >
                {c.label}
              </span>
              {typeof c.value === "string" ? (
                <span
                  className="mono"
                  style={{
                    fontSize: 12,
                    color: c.accent ? "var(--rust-text)" : "var(--text-2)",
                    overflow: "hidden",
                    textOverflow: "ellipsis",
                    whiteSpace: "nowrap",
                  }}
                >
                  {c.value}
                </span>
              ) : (
                <div style={{ minWidth: 0 }}>{c.value}</div>
              )}
            </div>
          ))}
        </div>
      </div>

      {/* Outcome banner above the transcript (mock 1d stopped variant + the preserved failure box).
          `stopped` → amber "STOPPED" banner with action-oriented copy per reason (an operator-attention
          state, NOT an error — never a red box). `failed` → red "FAILED" banner with the worker error.
          Every other outcome renders no banner. */}
      {stopped ? <StateBanner tone="amber" caps="STOPPED" body={stoppedCopy(data.error)} /> : null}
      {data.outcome === "failed" && data.error ? (
        <StateBanner tone="red" caps="FAILED" body={data.error} mono />
      ) : null}

      {/* transcript card (the "Agent output" well) */}
      <div
        style={{
          margin: "16px 20px 20px",
          borderRadius: "var(--r-card)",
          border: "1px solid var(--hair-card)",
          background: "var(--well)",
          overflow: "hidden",
        }}
      >
        {/* header */}
        <div
          style={{
            height: 40,
            display: "flex",
            alignItems: "center",
            gap: 8,
            padding: "0 16px",
            background: "var(--well-header)",
            borderBottom: "1px solid var(--hair-section)",
          }}
        >
          {streaming ? <StatusDot color="var(--rust-text)" pulse size={6} /> : null}
          <span style={{ fontSize: 12.5, fontWeight: 600 }}>Agent output</span>
          {streaming ? (
            <span
              className="mono"
              style={{
                fontSize: 10.5,
                color: "var(--rust-text)",
                background: "var(--tint-rust)",
                borderRadius: "var(--r-keycap)",
                padding: "1px 6px",
              }}
            >
              streaming
            </span>
          ) : null}
          <div style={{ flex: 1 }} />
          {streaming ? (
            <span className="mono" data-follow-state style={{ fontSize: 11, color: "var(--faint)" }}>
              {follow.following ? "following ↓" : "paused"}
            </span>
          ) : null}
        </div>
        {/* body */}
        <div
          ref={scrollRef}
          data-transcript-scroll
          onScroll={streaming ? follow.onScroll : undefined}
          style={{
            maxHeight: 560,
            overflowY: "auto",
            padding: "16px 18px",
            display: "flex",
            flexDirection: "column",
            gap: 13,
          }}
        >
          {entries.length === 0 && !inFlight && !transcriptQ.isLoading ? (
            <div style={{ color: "var(--faint)", fontSize: 13 }}>No transcript for this run.</div>
          ) : (
            entries.map((e, i) => <TranscriptEntry key={e.seq ?? i} e={e} />)
          )}
          {streaming ? <LiveCursor /> : null}
        </div>
        {/* footer */}
        <div
          style={{
            height: 32,
            display: "flex",
            alignItems: "center",
            justifyContent: "space-between",
            gap: 12,
            padding: "0 16px",
            background: "var(--well-footer)",
            borderTop: "1px solid var(--hair-section)",
          }}
        >
          <span className="mono" style={{ fontSize: 10.5, color: "var(--faint)" }}>
            turn {turnText} · {formatTokens(data.total_tokens)} tokens · {streaming ? "streaming" : "final"}
          </span>
          {streaming && !follow.following ? (
            <button
              type="button"
              onClick={follow.jumpToLatest}
              className="mono"
              style={{
                background: "none",
                border: "none",
                cursor: "pointer",
                fontSize: 10.5,
                color: "var(--rust-text)",
                padding: 0,
              }}
            >
              jump to latest ↓
            </button>
          ) : null}
        </div>
      </div>

      {/* operator messages + run history — preserved below the transcript (padded on the full-bleed view) */}
      <div style={{ padding: "0 20px 24px", display: "flex", flexDirection: "column", gap: 16 }}>
        <OperatorMessages runId={runId} inFlight={!!inFlight && enabled} messages={messagesQ.data ?? []} />
        <Panel style={{ padding: 0 }}>
          <PanelHeader title="Run history" note="newest first" />
          <div style={{ padding: "8px 0" }}>
            {attempts.length === 0 && !issueHistory.isLoading ? (
              <div style={{ padding: "8px 18px", color: "var(--faint)", fontSize: 12.5 }}>No prior runs.</div>
            ) : (
              attempts.map((r) => (
                <AttemptRow key={r.id} r={r} current={r.id === runId} onClick={() => onSelectRun(r.id)} />
              ))
            )}
          </div>
        </Panel>
      </div>
    </div>
  );
}

// stoppedCopy maps a stopped run's reason onto action-oriented banner copy (an operator-attention
// state — Resume re-dispatches). An unrecognized reason surfaces verbatim.
function stoppedCopy(reason: string): string {
  const copy: Record<string, string> = {
    "stopped by user":
      "This run was stopped. Resume dispatches a fresh agent that picks up from the existing branch state.",
    "ticket cancelled": "The ticket was cancelled, so the run was stopped.",
    "ticket moved externally":
      "The ticket left the active states while the agent worked; the run wound down without the agent's hand-off.",
  };
  return copy[reason] ?? reason;
}

// StateBanner — a full-width tinted row above the transcript (mock 1d): a caps status word over an
// explanation. Amber for a stopped run (attention, not error), red for a failure.
function StateBanner({ tone, caps, body, mono }: { tone: "amber" | "red"; caps: string; body: string; mono?: boolean }) {
  const color = tone === "amber" ? "var(--amber)" : "var(--red)";
  const bg = tone === "amber" ? "var(--tint-amber)" : "var(--tint-red)";
  const border = tone === "amber" ? "color-mix(in srgb, var(--amber) 35%, transparent)" : "var(--border-danger)";
  return (
    <div
      style={{
        margin: "16px 20px 0",
        borderRadius: "var(--r-card)",
        border: `1px solid ${border}`,
        background: bg,
        padding: "12px 16px",
        display: "flex",
        flexDirection: "column",
        gap: 6,
      }}
    >
      <span style={{ fontSize: 10, fontWeight: 600, letterSpacing: ".1em", textTransform: "uppercase", color }}>
        {caps}
      </span>
      <span
        className={mono ? "mono" : undefined}
        style={{ fontSize: 12.5, color: "var(--text-2)", whiteSpace: "pre-wrap", wordBreak: "break-word" }}
      >
        {body}
      </span>
    </div>
  );
}

// EstBadge — the "est." keycap chip on the Tokens meta cell (live or floored-estimate totals).
function EstBadge() {
  return (
    <span
      title="Estimated — token totals are a floored estimate (the run ended without a clean result event, or is still in flight)."
      style={{
        fontSize: 10,
        fontWeight: 600,
        color: "var(--faint)",
        background: "rgba(255,255,255,.05)",
        border: "1px solid var(--hair-card)",
        padding: "1px 6px",
        borderRadius: "var(--r-keycap)",
      }}
    >
      est.
    </span>
  );
}

// operatorMessageChip maps a message's delivery status to a StatusChip color + label (INF-250):
// sent → neutral "sent", delivered → green "delivered · turn N", expired → muted "expired".
function operatorMessageChip(m: RunMessage): { status: string; label: string } {
  switch (m.status) {
    case "delivered":
      return { status: "completed", label: m.delivered_turn != null ? `delivered · turn ${m.delivered_turn}` : "delivered" };
    case "expired":
      return { status: "idle", label: "expired" };
    default:
      return { status: "queued", label: "sent" };
  }
}

// OperatorMessages renders the run's operator-message timeline plus, while the run is in flight, a
// compact composer to send a mid-run "btw" to the agent (INF-250). The whole panel is hidden for a
// finished run that never received a message, so it doesn't clutter ordinary run detail.
function OperatorMessages({
  runId,
  inFlight,
  messages,
}: {
  runId: number;
  inFlight: boolean;
  messages: RunMessage[];
}) {
  const send = useSendRunMessage(runId);
  const [text, setText] = React.useState("");
  const [err, setErr] = React.useState("");

  if (!inFlight && messages.length === 0) {
    return null;
  }

  const trimmed = text.trim();
  const submit = () => {
    if (trimmed === "" || send.isPending) {
      return;
    }
    setErr("");
    send.mutate(trimmed, {
      onSuccess: () => setText(""),
      onError: (e) => setErr(e.message),
    });
  };

  return (
    <Panel style={{ padding: 0 }}>
      <PanelHeader title="Operator messages" note={inFlight ? "delivered mid-run" : "history"} />
      <div style={{ padding: "12px 18px", display: "flex", flexDirection: "column", gap: 10 }}>
        {messages.length === 0 ? (
          <div style={{ color: "var(--faint)", fontSize: 12.5 }}>
            No messages yet. Send a “btw …” below and the agent picks it up at its next step.
          </div>
        ) : (
          messages.map((m) => {
            const chip = operatorMessageChip(m);
            return (
              <div key={m.id} style={{ display: "flex", alignItems: "flex-start", gap: 10, padding: "2px 0" }}>
                <div style={{ flexShrink: 0, marginTop: 1 }}>
                  <StatusChip status={chip.status} label={chip.label} />
                </div>
                <span style={{ fontSize: 13, color: "var(--ink)", whiteSpace: "pre-wrap", overflowWrap: "anywhere" }}>
                  {m.body}
                </span>
              </div>
            );
          })
        )}

        {inFlight ? (
          <div style={{ display: "flex", flexDirection: "column", gap: 6, marginTop: 4 }}>
            <div style={{ display: "flex", gap: 8 }}>
              <input
                type="text"
                value={text}
                placeholder="Send a message to the running agent…"
                maxLength={4000}
                disabled={send.isPending}
                onChange={(e) => setText(e.target.value)}
                onKeyDown={(e) => {
                  // Ignore Enter while an IME composition is active (CJK etc.), so confirming a
                  // composition doesn't prematurely send.
                  if (e.key === "Enter" && !e.nativeEvent.isComposing) {
                    e.preventDefault();
                    submit();
                  }
                }}
                style={{
                  flex: 1,
                  height: 34,
                  padding: "0 12px",
                  background: "var(--well)",
                  border: "1px solid var(--hair-control)",
                  borderRadius: "var(--r-ctrl)",
                  color: "var(--ink)",
                  fontSize: 13,
                  fontFamily: "inherit",
                }}
              />
              <Button type="button" variant="subtle" disabled={trimmed === "" || send.isPending} onClick={submit}>
                Send
              </Button>
            </div>
            {err ? <span style={{ fontSize: 12, color: "var(--red)" }}>{err}</span> : null}
          </div>
        ) : null}
      </div>
    </Panel>
  );
}

// BackShell wraps the loading/error state with the same "← Jobs" button as the loaded header.
function BackShell({ onBack, children }: { onBack: () => void; children: React.ReactNode }) {
  return (
    <div style={{ minWidth: 0 }}>
      <div style={{ padding: "14px 20px 0" }}>
        <Button type="button" variant="subtle" size="sm" icon={ArrowLeft} onClick={onBack}>
          Jobs
        </Button>
      </div>
      {children}
    </div>
  );
}

// PanelHeader — the section header for the preserved operator-messages + run-history panels.
function PanelHeader({ title, note }: { title: string; note?: string }) {
  return (
    <div
      style={{
        display: "flex",
        alignItems: "center",
        gap: 8,
        padding: "14px 18px",
        borderBottom: "1px solid var(--hair-section)",
      }}
    >
      <span style={{ fontSize: 13, fontWeight: 600, letterSpacing: "-0.01em" }}>{title}</span>
      {note ? <span style={{ fontSize: 12, color: "var(--faint)" }}>{note}</span> : null}
    </div>
  );
}

// renderInline renders **bold** and `code` spans inside a plain-text transcript line (mock 1d prose:
// inline code as a mono keycap; bold as a brighter run).
function renderInline(text: string): React.ReactNode[] {
  return text.split(/(\*\*[^*]+\*\*|`[^`]+`)/g).map((part, i) => {
    if (part.startsWith("**") && part.endsWith("**") && part.length > 4) {
      return (
        <strong key={i} style={{ color: "var(--ink)", fontWeight: 600 }}>
          {part.slice(2, -2)}
        </strong>
      );
    }
    if (part.startsWith("`") && part.endsWith("`") && part.length > 2) {
      return (
        <code
          key={i}
          style={{
            fontFamily: MONO,
            fontSize: 12,
            background: "rgba(255,255,255,.06)",
            borderRadius: "var(--r-keycap)",
            padding: "0 4px",
          }}
        >
          {part.slice(1, -1)}
        </code>
      );
    }
    return <React.Fragment key={i}>{part}</React.Fragment>;
  });
}

function TranscriptEntry({ e }: { e: LogEntry }) {
  const type = transcriptEntryType(e.kind);

  if (type === "divider") {
    return (
      <div style={{ display: "flex", alignItems: "center", gap: 12, padding: "2px 0 4px" }}>
        <div style={{ flex: 1, height: 1, background: "var(--hair-section)" }} />
        <span style={{ fontSize: 10, letterSpacing: ".1em", color: "var(--ghost)", fontFamily: MONO }}>{e.text}</span>
        <div style={{ flex: 1, height: 1, background: "var(--hair-section)" }} />
      </div>
    );
  }

  if (type === "tool") {
    return (
      <div style={{ display: "flex", alignItems: "baseline", gap: 9, minWidth: 0, padding: "1px 0" }}>
        <span
          data-mcp={isMcpTool(e.tool) ? "true" : undefined}
          style={{
            display: "inline-flex",
            alignItems: "center",
            gap: 5,
            flexShrink: 0,
            background: "var(--tint-rust)",
            color: "var(--rust-text)",
            border: "1px solid color-mix(in srgb, var(--rust-text) 35%, transparent)",
            borderRadius: "var(--r-chip)",
            padding: "2px 8px",
            fontSize: 10.5,
            fontWeight: 600,
            fontFamily: MONO,
            whiteSpace: "nowrap",
          }}
        >
          → {e.tool}
        </span>
        {e.text ? (
          <span
            style={{
              fontSize: 11.5,
              color: "var(--neutral)",
              fontFamily: MONO,
              overflow: "hidden",
              textOverflow: "ellipsis",
              whiteSpace: "nowrap",
              minWidth: 0,
            }}
          >
            {e.text}
          </span>
        ) : null}
      </div>
    );
  }

  if (type === "out") {
    const json = e.text.startsWith("{");
    return (
      <div
        style={{
          display: "flex",
          gap: 8,
          borderLeft: "2px solid var(--hair-card)",
          paddingLeft: 12,
          fontSize: 11.5,
          fontFamily: MONO,
          color: "var(--faint)",
          lineHeight: 1.55,
        }}
      >
        <span style={{ color: "var(--faint)", flexShrink: 0 }}>←</span>
        <span style={{ whiteSpace: "pre-wrap", wordBreak: "break-word", color: json ? "var(--ghost)" : "var(--faint)" }}>
          {e.text}
        </span>
      </div>
    );
  }

  // text / thinking
  const muted = e.kind === "thinking";
  return <ProseEntry text={e.text} muted={muted} />;
}

// ProseEntry renders agent prose with newlines preserved (pre-wrap) and collapses long entries
// (skill docs, WORKFLOW.md, multi-paragraph reasoning) behind a "Show more" toggle so the merged
// transcript stays skimmable. Collapsibility is derived from the text (deterministic — works in
// tests); the visual clamp is a max-height with a soft bottom fade.
const PROSE_COLLAPSE_PX = 230;
function ProseEntry({ text, muted }: { text: string; muted: boolean }) {
  const [expanded, setExpanded] = React.useState(false);
  const long = text.length > 700 || text.split("\n").length > 12;
  const clamp = long && !expanded;
  return (
    <div style={{ maxWidth: "88ch" }}>
      <p
        style={{
          margin: 0,
          fontSize: 13,
          lineHeight: 1.6,
          color: muted ? "var(--faint)" : "var(--text-2)",
          fontStyle: muted ? "italic" : "normal",
          whiteSpace: "pre-wrap",
          overflowWrap: "anywhere",
          maxHeight: clamp ? PROSE_COLLAPSE_PX : undefined,
          overflow: clamp ? "hidden" : undefined,
          WebkitMaskImage: clamp ? "linear-gradient(to bottom, #000 72%, transparent)" : undefined,
          maskImage: clamp ? "linear-gradient(to bottom, #000 72%, transparent)" : undefined,
        }}
      >
        {renderInline(text)}
      </p>
      {long ? (
        <button
          type="button"
          onClick={() => setExpanded((v) => !v)}
          style={{
            marginTop: 6,
            padding: 0,
            background: "none",
            border: "none",
            color: "var(--rust-text)",
            fontSize: 12,
            fontWeight: 600,
            fontFamily: "inherit",
            cursor: "pointer",
          }}
        >
          {expanded ? "Show less" : "Show more…"}
        </button>
      ) : null}
    </div>
  );
}

// LiveCursor — the blinking rust caret at the tail of a streaming transcript (mock 1d: 7×14px,
// 1.1s step-end). Neutralized under prefers-reduced-motion by the global guard in index.css.
function LiveCursor() {
  return (
    <span
      data-live-cursor
      aria-hidden
      style={{
        display: "inline-block",
        width: 7,
        height: 14,
        borderRadius: 1,
        background: "var(--rust-text)",
        animation: "blink 1.1s step-end infinite",
      }}
    />
  );
}

// sortAttempts orders the per-attempt history newest-first by started_at.
function sortAttempts(runs: RunSummary[]): RunSummary[] {
  return [...runs].sort((a, b) => (Date.parse(b.started_at) || 0) - (Date.parse(a.started_at) || 0));
}

function AttemptRow({ r, current, onClick }: { r: RunSummary; current: boolean; onClick: () => void }) {
  const [hover, setHover] = React.useState(false);
  return (
    <div
      onClick={onClick}
      onMouseEnter={() => setHover(true)}
      onMouseLeave={() => setHover(false)}
      style={{
        display: "flex",
        alignItems: "center",
        gap: 14,
        padding: "13px 18px",
        borderBottom: "1px solid var(--hair-section)",
        cursor: "pointer",
        background: hover ? "rgba(255,255,255,.03)" : "transparent",
        borderLeft: current ? "2px solid var(--rust)" : "2px solid transparent",
      }}
    >
      <StatusChip status={outcomeToStatus(r.outcome)} />
      {/* attempt 0 = clean first-try dispatch; only label a row when a prior attempt happened, and
          show it 1-indexed (attempt + 1) so "attempt 2" reads as the genuine second dispatch. INF-294 */}
      {r.attempt >= 1 ? (
        <span style={{ fontSize: 12.5, color: "var(--text-muted)", fontFamily: MONO }}>attempt {r.attempt + 1}</span>
      ) : null}
      <span style={{ fontSize: 12.5, color: "var(--faint)", fontFamily: MONO }}>{formatDateTime(r.started_at)}</span>
      <span style={{ fontSize: 12.5, color: "var(--faint)", fontFamily: MONO }}>{runDuration(r.started_at, r.ended_at)}</span>
      <div style={{ flex: 1 }} />
      <span style={{ fontSize: 12.5, color: "var(--faint)", fontFamily: MONO }}>{formatTokens(r.total_tokens)} tok</span>
      {current ? <span style={{ fontSize: 11.5, color: "var(--rust-text)", fontWeight: 600 }}>· current</span> : null}
    </div>
  );
}
