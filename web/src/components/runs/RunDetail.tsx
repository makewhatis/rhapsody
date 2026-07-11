import * as React from "react";
import { StatusChip } from "@/components/ui/status-chip";
import { StatusDot } from "@/components/ui/status-dot";
import { Button } from "@/components/ui/button";
import { ArrowLeft, Link, Pause, RotateCcw } from "@/components/ui/icons";
import { ConfirmDialog } from "@/components/ui/confirm-dialog";
import { useStopRun, useResumeRun, useSendRunMessage } from "@/hooks/useRunActions";
import { useToast } from "@/components/shell/Toast";
import type { LinearProject, LogEntry, RunMessage, RunSummary } from "@/lib/api";
import { elapsedSeconds, formatDateTime, formatDuration, formatTokens, runDuration } from "@/lib/format";
import { repoShortName } from "@/lib/project";
import { isMcpTool, outcomeToStatus, resolveAgent, resolveProject, transcriptEntryType } from "@/lib/runs-model";
import { useNow } from "@/hooks/useNow";
import { useIssueHistory, useRunDetail, useRunMessages, useTranscript } from "@/hooks/useRunDetail";
import { useLinearIdentity } from "@/hooks/useConfig";
import { openExternal } from "@/lib/bindings";
import { Panel } from "./Panel";

export interface RunDetailProps {
  runId: number;
  /** Fetched Linear projects, for agent name + dot colour resolution. */
  projects: LinearProject[];
  /** When false (e.g. under the Wails host, where the daemon HTTP API is unreachable from the
   * asset-server origin) the detail queries stay idle instead of polling a dead origin. */
  enabled?: boolean;
  onBack: () => void;
  onSelectRun: (runId: number) => void;
}

// RunDetail — the drill-in for one run, re-skinned to match `runs.jsx` RunDetail: header +
// 9-field meta grid + transcript + activity + run history. Self-fetches the unified run detail,
// transcript (streaming while running), and per-attempt history. Polling is driven by the
// outcome (poll while running, freeze when terminal); the live snapshot and the finished store
// render identically since the payload is the daemon's unification.
export function RunDetail({ runId, projects, enabled = true, onBack, onSelectRun }: RunDetailProps) {
  const { data, isLoading, isError, error } = useRunDetail(runId, enabled);
  const nowMs = useNow(1000);
  const inFlight = data?.outcome === "running";
  // `streaming` gates the live chrome (pulse, "streaming" note, blinking cursor, live duration
  // accent). It requires both a running outcome AND active polling: when polling is disabled/paused
  // (`enabled` false) the detail is a frozen cached snapshot, so the stream is not live. The
  // run-state badge still shows "running" regardless — only the live-stream affordances are gated.
  const streaming = !!inFlight && enabled;
  const transcriptQ = useTranscript(runId, !!inFlight, enabled);
  const messagesQ = useRunMessages(runId, !!inFlight, enabled);
  const issueHistory = useIssueHistory(data?.issue_identifier ?? "", enabled);
  // The ticket — not any single PR — is the stable target: one issue can spawn a whole Graphite
  // stack of PRs. Build the Linear deep link from the connected workspace's slug + the identifier.
  const workspaceURLKey = useLinearIdentity().data?.workspace_url_key ?? "";

  // Run actions: a danger Stop (confirmed) while running, a Resume on a stopped run. Both toast
  // their result; the success toast distinguishes a clean Backlog/Todo move from a killed-but-not-
  // moved run (so the operator can move the ticket by hand).
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
  // Keep the transcript pinned to the latest line while the run streams.
  React.useEffect(() => {
    if (streaming && scrollRef.current) {
      scrollRef.current.scrollTop = scrollRef.current.scrollHeight;
    }
  }, [entryCount, streaming]);


  if (isLoading || !data) {
    return (
      <BackShell onBack={onBack}>
        <Panel style={{ padding: "40px 22px", textAlign: "center", color: "var(--tx-3)", fontSize: 13 }}>
          {isError ? `Failed to load run: ${(error as Error)?.message ?? "unknown error"}` : "Loading run…"}
        </Panel>
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

  const meta: { k: string; v: React.ReactNode; mono?: boolean; accent?: boolean }[] = [
    { k: "State", v: <StatusChip status={status} /> },
    { k: "Project", v: resolveProject(data.project, projects), mono: true },
    { k: "Repo", v: repoShortName(data.repo), mono: true },
    // attempt is the 0-indexed retry counter: 0 = fresh dispatch that ran first-try (no
    // retries). Showing a bare "attempt 0" on every healthy run reads like an off-by-one bug,
    // so surface it only when a prior attempt actually happened (>= 1), and display it 1-indexed
    // (attempt + 1) so the human-facing ordinal matches intuition — the second dispatch reads
    // "Attempt 2", not the confusing 0-indexed "Attempt 1". See INF-294.
    ...(data.attempt >= 1 ? [{ k: "Attempt", v: String(data.attempt + 1), mono: true }] : []),
    { k: "Turn", v: String(data.turn_count), mono: true },
    {
      k: "Tokens",
      // Match the mock's meta cell: total + est. badge only (the in/out breakdown lives in the
      // "Tokens today" stat tile, not here).
      v: (
        <span style={{ display: "inline-flex", alignItems: "center", gap: 7 }}>
          <span className="mono" style={{ fontSize: 13.5, fontWeight: 500, color: "var(--tx)" }}>
            {formatTokens(data.total_tokens)}
          </span>
          {estimated ? (
            <span
              title="Estimated — token totals are a floored estimate (the run ended without a clean result event)."
              style={{
                fontSize: 10,
                fontWeight: 600,
                color: "var(--tx-3)",
                background: "rgba(255,255,255,.05)",
                border: "1px solid var(--line)",
                padding: "1px 6px",
                borderRadius: 5,
              }}
            >
              est.
            </span>
          ) : null}
        </span>
      ),
    },
    { k: "Started", v: formatDateTime(data.started_at), mono: true },
    { k: "Duration", v: durationText, mono: true, accent: streaming },
  ];

  return (
    <div style={{ display: "flex", flexDirection: "column", gap: 20 }}>
      {/* header */}
      <div style={{ display: "flex", alignItems: "flex-start", justifyContent: "space-between", gap: 16 }}>
        <div style={{ display: "flex", gap: 16, minWidth: 0 }}>
          <button
            type="button"
            onClick={onBack}
            style={{
              display: "inline-flex",
              alignItems: "center",
              gap: 6,
              background: "var(--bg-raised)",
              border: "1px solid var(--line)",
              borderRadius: "var(--r-ctrl)",
              color: "var(--tx-2)",
              height: 34,
              padding: "0 12px",
              cursor: "pointer",
              fontSize: 13,
              flexShrink: 0,
            }}
          >
            <ArrowLeft size={15} />
            Jobs
          </button>
          <div style={{ minWidth: 0 }}>
            <div style={{ display: "flex", alignItems: "center", gap: 11 }}>
              <h1
                className="mono"
                style={{ fontSize: 21, fontWeight: 700, letterSpacing: "-0.02em", whiteSpace: "nowrap" }}
              >
                {data.issue_identifier}
              </h1>
              <StatusChip status={status} />
              <span
                style={{
                  display: "inline-flex",
                  alignItems: "center",
                  gap: 6,
                  fontSize: 12.5,
                  color: "var(--tx-3)",
                }}
              >
                <StatusDot color={agent.color} size={7} />
                {agent.name}
              </span>
            </div>
            <p style={{ fontSize: 14, color: "var(--tx-2)", marginTop: 5, maxWidth: 640 }}>{data.title}</p>
          </div>
        </div>
        {/* Run-level actions. Stop (running) kills the agent now and moves the ticket to Backlog so
            it isn't re-dispatched; Resume (stopped) moves it back to Todo. "Open ticket" is always
            present. */}
        <div style={{ display: "flex", gap: 8, flexShrink: 0 }}>
          {inFlight ? (
            <Button
              type="button"
              variant="danger"
              icon={Pause}
              disabled={stop.isPending}
              onClick={() => setConfirmStop(true)}
            >
              Stop
            </Button>
          ) : null}
          {stopped ? (
            <Button
              type="button"
              variant="subtle"
              icon={RotateCcw}
              disabled={resume.isPending}
              onClick={doResume}
            >
              Resume
            </Button>
          ) : null}
          <Button
            type="button"
            variant="ghost"
            icon={Link}
            disabled={!workspaceURLKey || !data.issue_identifier}
            onClick={() => openExternal(`https://linear.app/${workspaceURLKey}/issue/${data.issue_identifier}`)}
          >
            Open ticket
          </Button>
        </div>
      </div>

      {/* meta grid */}
      <Panel style={{ padding: 20 }}>
        <div style={{ display: "grid", gridTemplateColumns: "repeat(5, 1fr)", gap: "20px 16px" }}>
          {meta.map((m) => (
            <div key={m.k} style={{ display: "flex", flexDirection: "column", gap: 6, minWidth: 0 }}>
              <span
                style={{
                  fontSize: 10.5,
                  fontWeight: 600,
                  letterSpacing: ".07em",
                  textTransform: "uppercase",
                  color: "var(--tx-faint)",
                }}
              >
                {m.k}
              </span>
              {typeof m.v === "string" ? (
                <span
                  className={m.mono ? "mono" : undefined}
                  style={{
                    fontSize: 13.5,
                    fontWeight: 500,
                    color: m.accent ? "var(--em-bright)" : "var(--tx)",
                    overflow: "hidden",
                    textOverflow: "ellipsis",
                    whiteSpace: "nowrap",
                  }}
                >
                  {m.v}
                </span>
              ) : (
                <div>{m.v}</div>
              )}
            </div>
          ))}
        </div>
      </Panel>

      {/* Outcome-aware reason panel (taxonomy v2). `failed` → red "Failure" panel with the real
          worker error (git/SSH/clone, claude startup/turn failure, stall). `stopped` → amber
          "Stopped" panel with action-oriented copy per reason (an operator-attention state, NOT an
          error — a stopped run must never show a red Failure box). Every other outcome: no panel,
          even if `error` is non-empty. */}
      {(() => {
        if (data.outcome === "failed" && data.error) {
          return (
            <Panel style={{ padding: "14px 18px", borderColor: "rgba(239,83,80,.35)", background: "var(--red-soft)" }}>
              <div style={{ display: "flex", flexDirection: "column", gap: 6 }}>
                <span
                  style={{
                    fontSize: 10.5,
                    fontWeight: 600,
                    letterSpacing: ".07em",
                    textTransform: "uppercase",
                    color: "var(--red)",
                  }}
                >
                  Failure
                </span>
                <span
                  className="mono"
                  style={{ fontSize: 12.5, color: "var(--tx)", whiteSpace: "pre-wrap", wordBreak: "break-word" }}
                >
                  {data.error}
                </span>
              </div>
            </Panel>
          );
        }
        if (data.outcome === "stopped") {
          const copy: Record<string, string> = {
            "stopped by user":
              "This run was stopped. Resume dispatches a fresh agent that picks up from the existing branch state.",
            "ticket cancelled": "The ticket was cancelled, so the run was stopped.",
            "ticket moved externally":
              "The ticket left the active states while the agent worked; the run wound down without the agent's hand-off.",
          };
          const body = copy[data.error] ?? data.error;
          return (
            <Panel style={{ padding: "14px 18px", borderColor: "rgba(245,158,11,.35)", background: "var(--amber-soft)" }}>
              <div style={{ display: "flex", flexDirection: "column", gap: 6 }}>
                <span
                  style={{
                    fontSize: 10.5,
                    fontWeight: 600,
                    letterSpacing: ".07em",
                    textTransform: "uppercase",
                    color: "var(--amber)",
                  }}
                >
                  Stopped
                </span>
                <span style={{ fontSize: 12.5, color: "var(--tx)", whiteSpace: "pre-wrap", wordBreak: "break-word" }}>
                  {body}
                </span>
              </div>
            </Panel>
          );
        }
        return null;
      })()}

      {/* the single, complete agent-output transcript (tools + full prose, oldest → newest) */}
      <Panel style={{ padding: 0 }}>
        <PanelHeader title="Agent output" note={streaming ? "streaming" : "final"} live={streaming} />
        <div
          ref={scrollRef}
          style={{
            maxHeight: 620,
            overflowY: "auto",
            padding: "16px 18px",
            display: "flex",
            flexDirection: "column",
            gap: 12,
          }}
        >
          {entries.length === 0 && !inFlight && !transcriptQ.isLoading ? (
            <div style={{ color: "var(--tx-3)", fontSize: 13 }}>No transcript for this run.</div>
          ) : (
            entries.map((e, i) => <TranscriptEntry key={e.seq ?? i} e={e} />)
          )}
          {streaming ? <LiveCursor /> : null}
        </div>
      </Panel>

      {/* operator messages — a composer (while in-flight) + the delivery timeline (INF-250) */}
      <OperatorMessages
        runId={runId}
        inFlight={!!inFlight && enabled}
        messages={messagesQ.data ?? []}
      />

      {/* run history */}
      <Panel style={{ padding: 0 }}>
        <PanelHeader title="Run history" note="newest first" />
        <div style={{ padding: "8px 0" }}>
          {attempts.length === 0 && !issueHistory.isLoading ? (
            <div style={{ padding: "8px 18px", color: "var(--tx-3)", fontSize: 12.5 }}>No prior runs.</div>
          ) : (
            attempts.map((r) => (
              <AttemptRow key={r.id} r={r} current={r.id === runId} onClick={() => onSelectRun(r.id)} />
            ))
          )}
        </div>
      </Panel>

      <ConfirmDialog
        open={confirmStop}
        title={`Stop ${data.issue_identifier}?`}
        body="This kills the running agent now and moves the ticket to Backlog so it isn't picked back up. You can Resume it later."
        confirmLabel="Stop agent"
        danger
        busy={stop.isPending}
        onConfirm={doStop}
        onClose={() => setConfirmStop(false)}
      />
    </div>
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
          <div style={{ color: "var(--tx-3)", fontSize: 12.5 }}>
            No messages yet. Send a “btw …” below and the agent picks it up at its next step.
          </div>
        ) : (
          messages.map((m) => {
            const chip = operatorMessageChip(m);
            return (
              <div
                key={m.id}
                style={{ display: "flex", alignItems: "flex-start", gap: 10, padding: "2px 0" }}
              >
                <div style={{ flexShrink: 0, marginTop: 1 }}>
                  <StatusChip status={chip.status} label={chip.label} />
                </div>
                <span style={{ fontSize: 13, color: "var(--tx)", whiteSpace: "pre-wrap", overflowWrap: "anywhere" }}>
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
                  background: "var(--bg-raised)",
                  border: "1px solid var(--line)",
                  borderRadius: "var(--r-ctrl)",
                  color: "var(--tx)",
                  fontSize: 13,
                  fontFamily: "inherit",
                }}
              />
              <Button
                type="button"
                variant="subtle"
                disabled={trimmed === "" || send.isPending}
                onClick={submit}
              >
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

// BackShell wraps the loading/error state with the same back button as the loaded view.
function BackShell({ onBack, children }: { onBack: () => void; children: React.ReactNode }) {
  return (
    <div style={{ display: "flex", flexDirection: "column", gap: 20 }}>
      <button
        type="button"
        onClick={onBack}
        style={{
          display: "inline-flex",
          alignItems: "center",
          gap: 6,
          alignSelf: "flex-start",
          background: "var(--bg-raised)",
          border: "1px solid var(--line)",
          borderRadius: "var(--r-ctrl)",
          color: "var(--tx-2)",
          height: 34,
          padding: "0 12px",
          cursor: "pointer",
          fontSize: 13,
        }}
      >
        <ArrowLeft size={15} />
        Jobs
      </button>
      {children}
    </div>
  );
}

function PanelHeader({ title, note, live }: { title: string; note?: string; live?: boolean }) {
  return (
    <div
      style={{
        display: "flex",
        alignItems: "center",
        gap: 8,
        padding: "14px 18px",
        borderBottom: "1px solid var(--line-2)",
      }}
    >
      {live ? <StatusDot color="var(--em-bright)" pulse size={7} /> : null}
      <span style={{ fontSize: 13, fontWeight: 600, letterSpacing: "-0.01em" }}>{title}</span>
      {note ? <span style={{ fontSize: 12, color: "var(--tx-faint)" }}>{note}</span> : null}
    </div>
  );
}

// mdBold renders **bold** spans inside a plain-text transcript/activity line.
function mdBold(text: string): React.ReactNode[] {
  return text.split(/(\*\*[^*]+\*\*)/g).map((part, i) =>
    part.startsWith("**") && part.endsWith("**") ? (
      <strong key={i} style={{ color: "var(--tx)", fontWeight: 600 }}>
        {part.slice(2, -2)}
      </strong>
    ) : (
      <React.Fragment key={i}>{part}</React.Fragment>
    ),
  );
}

const MONO = "var(--font-mono)";

function TranscriptEntry({ e }: { e: LogEntry }) {
  const type = transcriptEntryType(e.kind);

  if (type === "divider") {
    return (
      <div style={{ display: "flex", alignItems: "center", gap: 12, padding: "2px 0 6px" }}>
        <div style={{ flex: 1, height: 1, background: "var(--line-2)" }} />
        <span
          style={{
            fontSize: 10.5,
            fontWeight: 600,
            letterSpacing: ".12em",
            color: "var(--tx-faint)",
            fontFamily: MONO,
          }}
        >
          {e.text}
        </span>
        <div style={{ flex: 1, height: 1, background: "var(--line-2)" }} />
      </div>
    );
  }

  if (type === "tool") {
    return (
      <div style={{ display: "flex", flexWrap: "wrap", alignItems: "baseline", gap: 9, padding: "1px 0" }}>
        <span
          data-mcp={isMcpTool(e.tool) ? "true" : undefined}
          style={{
            display: "inline-flex",
            alignItems: "center",
            gap: 5,
            background: "var(--em-soft)",
            color: "var(--em-bright)",
            border: "1px solid rgba(16,185,129,.22)",
            borderRadius: 6,
            padding: "3px 8px",
            fontSize: 12,
            fontWeight: 600,
            fontFamily: MONO,
            whiteSpace: "nowrap",
          }}
        >
          → {e.tool}
        </span>
        {e.text ? (
          <span style={{ fontSize: 12, color: "var(--tx-3)", fontFamily: MONO, wordBreak: "break-word" }}>
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
          fontSize: 12,
          fontFamily: MONO,
          color: "var(--tx-3)",
          lineHeight: 1.55,
        }}
      >
        <span style={{ color: "var(--tx-faint)", flexShrink: 0 }}>←</span>
        <span style={{ whiteSpace: "pre-wrap", wordBreak: "break-word", color: json ? "var(--tx-faint)" : "var(--tx-3)" }}>{e.text}</span>
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
    <div style={{ maxWidth: "82ch" }}>
      <p
        style={{
          margin: 0,
          fontSize: 13,
          lineHeight: 1.6,
          color: muted ? "var(--tx-3)" : "var(--tx)",
          fontStyle: muted ? "italic" : "normal",
          whiteSpace: "pre-wrap",
          overflowWrap: "anywhere",
          maxHeight: clamp ? PROSE_COLLAPSE_PX : undefined,
          overflow: clamp ? "hidden" : undefined,
          WebkitMaskImage: clamp ? "linear-gradient(to bottom, #000 72%, transparent)" : undefined,
          maskImage: clamp ? "linear-gradient(to bottom, #000 72%, transparent)" : undefined,
        }}
      >
        {mdBold(text)}
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
            color: "var(--em-bright)",
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

function LiveCursor() {
  return (
    <div
      style={{ display: "flex", alignItems: "center", gap: 8, fontSize: 12, color: "var(--em-bright)", fontFamily: MONO }}
    >
      <span
        style={{ width: 7, height: 14, background: "var(--em-bright)", borderRadius: 1, animation: "blink 1s steps(2) infinite" }}
      />
      <span style={{ color: "var(--tx-3)" }}>running…</span>
    </div>
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
        borderBottom: "1px solid var(--line-2)",
        cursor: "pointer",
        background: hover ? "var(--bg-hover)" : "transparent",
        borderLeft: current ? "2px solid var(--em-bright)" : "2px solid transparent",
      }}
    >
      <StatusChip status={outcomeToStatus(r.outcome)} />
      {/* attempt 0 = clean first-try dispatch; only label a row when a prior attempt happened, and
          show it 1-indexed (attempt + 1) so "attempt 2" reads as the genuine second dispatch. INF-294 */}
      {r.attempt >= 1 ? (
        <span style={{ fontSize: 12.5, color: "var(--tx-2)", fontFamily: MONO }}>attempt {r.attempt + 1}</span>
      ) : null}
      <span style={{ fontSize: 12.5, color: "var(--tx-3)", fontFamily: MONO }}>{formatDateTime(r.started_at)}</span>
      <span style={{ fontSize: 12.5, color: "var(--tx-3)", fontFamily: MONO }}>{runDuration(r.started_at, r.ended_at)}</span>
      <div style={{ flex: 1 }} />
      <span style={{ fontSize: 12.5, color: "var(--tx-3)", fontFamily: MONO }}>{formatTokens(r.total_tokens)} tok</span>
      {current ? <span style={{ fontSize: 11.5, color: "var(--em-bright)", fontWeight: 600 }}>· current</span> : null}
    </div>
  );
}
