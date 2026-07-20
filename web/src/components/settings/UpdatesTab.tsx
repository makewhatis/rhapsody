import * as React from "react";
import {
  Button,
  CheckCircle,
  Collapsible,
  Download,
  Refresh,
  ScrollText,
  StatusDot,
} from "@/components/ui";
import type { Updater } from "@/hooks/useUpdater";
import { downloadPercent, formatBytes } from "@/lib/updater-model";

export interface UpdatesTabProps {
  /** The single shell-owned update model (shared with the toolbar gear dot). */
  updater: Updater;
}

// UpdatesTab — the Settings "Updates" surface (P11-U3). A titled card whose body reflects the
// updater phase machine: idle/up-to-date, an "Update available — vX.Y.Z" row with "What's new" +
// Download, a download progress readout, and a "Restart to finish" affordance that WARNS (via the
// active-runs dialog) before it stops any playing agents. A manual "Check for updates" lives in the
// header. All motion is CSS-keyframe based, so the global `prefers-reduced-motion` guard neutralizes
// it (index.css). Every action degrades to a no-op without the Tauri bridge (plain browser / tests).
export function UpdatesTab({ updater }: UpdatesTabProps) {
  const { phase, info, error } = updater;
  // "Check for updates" is inert while a check/download/install is already running.
  const busy = phase === "checking" || phase === "downloading" || phase === "installing";
  const notes = info?.notes?.trim() ?? "";
  // Show release notes wherever there is a pending update carrying them.
  const showNotes =
    notes.length > 0 &&
    (phase === "available" || phase === "downloading" || phase === "ready" || phase === "deferred");

  return (
    <>
      <div
        style={{
          background: "var(--bg-card)",
          border: "1px solid var(--line)",
          borderRadius: "var(--r-card)",
          boxShadow: "var(--shadow-card)",
        }}
      >
        <div
          style={{
            display: "flex",
            alignItems: "flex-start",
            justifyContent: "space-between",
            gap: 16,
            padding: "18px 22px 16px",
            borderBottom: "1px solid var(--line-2)",
          }}
        >
          <div style={{ display: "flex", gap: 12, alignItems: "flex-start" }}>
            <div
              style={{
                width: 30,
                height: 30,
                borderRadius: 8,
                display: "grid",
                placeItems: "center",
                background: "rgba(255,255,255,.035)",
                border: "1px solid var(--line)",
                color: "var(--tx-2)",
                marginTop: 1,
              }}
            >
              <Download size={15} />
            </div>
            <div>
              <div style={{ fontSize: 14.5, fontWeight: 600, color: "var(--tx)", letterSpacing: "-0.01em" }}>
                Software updates
              </div>
              <div style={{ fontSize: 12.5, color: "var(--tx-3)", marginTop: 3, lineHeight: 1.5, maxWidth: 560 }}>
                Rhapsody installs updates in place; each one is signature-verified before it runs.
              </div>
            </div>
          </div>
          <Button variant="subtle" size="sm" icon={Refresh} disabled={busy} onClick={updater.check}>
            {phase === "checking" ? "Checking…" : "Check for updates"}
          </Button>
        </div>
        <div style={{ padding: 22, display: "flex", flexDirection: "column", gap: 16 }}>
          <StatusBlock updater={updater} />
          {showNotes ? (
            <Collapsible label="What's new" icon={ScrollText}>
              <div style={{ fontSize: 12.5, color: "var(--tx-2)", lineHeight: 1.6, whiteSpace: "pre-wrap" }}>
                {notes}
              </div>
            </Collapsible>
          ) : null}
          {error ? (
            <div style={{ fontSize: 12, color: "var(--red)", lineHeight: 1.5 }}>{error}</div>
          ) : null}
        </div>
      </div>
      <ActiveRunsDialog updater={updater} />
    </>
  );
}

// StatusBlock — the phase-specific status row: a leading dot/icon, a headline + subline, and (for
// actionable phases) the primary button. Kept in one place so the whole state machine reads top to
// bottom.
function StatusBlock({ updater }: { updater: Updater }) {
  const { phase, info } = updater;
  const version = info?.version || "";
  const current = info?.current_version || "";

  switch (phase) {
    case "checking":
      return <Line dot="var(--rust-text)" pulse title="Checking for updates…" />;
    case "up-to-date":
      return (
        <Line
          icon={<CheckCircle size={16} style={{ color: "var(--sage)" }} />}
          title="You're on the latest version"
          sub={current ? `Rhapsody ${current}` : undefined}
        />
      );
    case "available":
      return (
        <Line
          dot="var(--rust-text)"
          title={
            <span>
              Update available{version ? " — " : ""}
              {version ? <VersionTag version={version} /> : null}
            </span>
          }
          sub={current ? `You're on ${current}.` : undefined}
          action={
            <Button variant="primary" size="sm" icon={Download} onClick={updater.download}>
              Download
            </Button>
          }
        />
      );
    case "downloading":
      return <DownloadingRow updater={updater} />;
    case "installing":
      return <Line dot="var(--rust-text)" pulse title="Installing…" />;
    case "ready":
      return (
        <Line
          dot="var(--rust-text)"
          title={<span>Update ready{version ? <> — <VersionTag version={version} /></> : null}</span>}
          sub="Restart Rhapsody to finish installing."
          action={
            <Button variant="primary" size="sm" onClick={updater.requestInstall}>
              Restart to finish
            </Button>
          }
        />
      );
    case "deferred":
      return (
        <Line
          dot="var(--amber)"
          title="Update scheduled"
          sub="Rhapsody will install it on your next quit — the agents keep playing until then."
          action={
            <Button variant="primary" size="sm" onClick={updater.confirmInstallNow}>
              Restart &amp; update now
            </Button>
          }
        />
      );
    case "error":
      return (
        <Line
          dot="var(--red)"
          title="Update check failed"
          sub="Rhapsody couldn't reach the update server. Check your connection and try again."
        />
      );
    case "idle":
    default:
      return (
        <Line
          dot="var(--neutral)"
          title="Keep Rhapsody up to date"
          sub="Check for a newer version, or Rhapsody will notice one on its next launch."
        />
      );
  }
}

// Line — the shared status-row layout: dot/icon · (headline + optional subline) · optional action.
function Line({
  dot,
  pulse,
  icon,
  title,
  sub,
  action,
}: {
  dot?: string;
  pulse?: boolean;
  icon?: React.ReactNode;
  title: React.ReactNode;
  sub?: string;
  action?: React.ReactNode;
}) {
  return (
    <div style={{ display: "flex", alignItems: "center", gap: 12 }}>
      <span style={{ display: "inline-flex", width: 16, justifyContent: "center" }}>
        {icon ?? <StatusDot color={dot} pulse={pulse} size={8} />}
      </span>
      <div style={{ flex: 1, minWidth: 0 }}>
        <div style={{ fontSize: 13.5, fontWeight: 500, color: "var(--tx)" }}>{title}</div>
        {sub ? <div style={{ fontSize: 12, color: "var(--tx-3)", marginTop: 2, lineHeight: 1.45 }}>{sub}</div> : null}
      </div>
      {action}
    </div>
  );
}

// VersionTag — the announced version rendered as a mono rust chip ("v1.4.0").
function VersionTag({ version }: { version: string }) {
  return (
    <span
      className="mono"
      style={{
        fontSize: 12,
        fontWeight: 600,
        color: "var(--rust-text)",
        background: "var(--tint-rust)",
        padding: "1px 7px",
        borderRadius: "var(--r-keycap)",
      }}
    >
      v{version}
    </span>
  );
}

// DownloadingRow — a labelled progress bar. Determinate (a reported total) shows a filled rust bar
// with the byte readout; indeterminate (no Content-Length) shows a pulsing bar + running byte count.
function DownloadingRow({ updater }: { updater: Updater }) {
  const { progress } = updater;
  const pct = downloadPercent(progress);
  const downloaded = progress?.downloaded ?? 0;
  const total = progress?.total ?? null;
  return (
    <div style={{ display: "flex", flexDirection: "column", gap: 8 }}>
      <div style={{ display: "flex", justifyContent: "space-between", alignItems: "baseline" }}>
        <span style={{ fontSize: 13.5, fontWeight: 500, color: "var(--tx)" }}>Downloading update…</span>
        <span className="mono" style={{ fontSize: 11.5, color: "var(--tx-3)" }}>
          {pct != null && total != null
            ? `${formatBytes(downloaded)} of ${formatBytes(total)}`
            : formatBytes(downloaded)}
        </span>
      </div>
      <div
        role="progressbar"
        aria-label="Downloading update"
        aria-valuemin={0}
        aria-valuemax={100}
        aria-valuenow={pct ?? undefined}
        style={{ height: 6, borderRadius: 999, background: "rgba(255,255,255,.06)", overflow: "hidden" }}
      >
        <div
          style={{
            height: "100%",
            width: pct != null ? `${pct}%` : "55%",
            background: "var(--rust)",
            borderRadius: 999,
            transition: "width .2s ease-out",
            // Indeterminate: a gentle opacity breath (neutralized under prefers-reduced-motion).
            animation: pct == null ? "pulse 2.4s ease-in-out infinite" : "none",
          }}
        />
      </div>
    </div>
  );
}

// ActiveRunsDialog — the safety confirm before an install stops running agents (spec: "never
// silently restart with active runs"). Rendered only when an install is awaiting confirmation. Three
// choices: update now (stops the agents), install on next quit (leaves them playing), or cancel.
// Escape / overlay-click cancels. Mirrors ConfirmDialog's surface but needs a second action, so it
// is local rather than the shared single-confirm primitive.
function ActiveRunsDialog({ updater }: { updater: Updater }) {
  const n = updater.activeRunsPrompt;
  const { dismissPrompt } = updater;
  React.useEffect(() => {
    if (n == null) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") dismissPrompt();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [n, dismissPrompt]);
  if (n == null) return null;
  const label = `${n} ${n === 1 ? "agent is" : "agents are"} playing`;
  return (
    <div
      role="presentation"
      onClick={dismissPrompt}
      style={{
        position: "fixed",
        inset: 0,
        zIndex: 500,
        background: "rgba(0,0,0,0.5)",
        display: "flex",
        alignItems: "center",
        justifyContent: "center",
      }}
    >
      <div
        role="dialog"
        aria-modal="true"
        aria-label={label}
        onClick={(e) => e.stopPropagation()}
        style={{
          width: 440,
          maxWidth: "90vw",
          background: "var(--bg-card)",
          border: "1px solid var(--line-strong)",
          borderRadius: "var(--r-card)",
          padding: 22,
          display: "flex",
          flexDirection: "column",
          gap: 14,
        }}
      >
        <div style={{ fontSize: 16, fontWeight: 600, color: "var(--tx)" }}>{label}</div>
        <div style={{ fontSize: 13, color: "var(--tx-3)", lineHeight: 1.5 }}>
          Updating now will stop them and restart Rhapsody. You can install immediately, or wait and let the
          update apply the next time you quit.
        </div>
        <div style={{ display: "flex", justifyContent: "flex-end", gap: 8, marginTop: 4, flexWrap: "wrap" }}>
          <Button type="button" variant="ghost" onClick={dismissPrompt}>
            Cancel
          </Button>
          <Button type="button" variant="subtle" onClick={updater.deferToQuit}>
            Install on next quit
          </Button>
          <Button type="button" variant="primary" onClick={updater.confirmInstallNow}>
            Update now
          </Button>
        </div>
      </div>
    </div>
  );
}
