import { Button, Note, Pill } from "@/components/console";
import { useTeamsConfigQuery } from "@/hooks/useTeams";
import { useToolDoctor } from "@/hooks/useToolDoctor";
import type { Updater } from "@/hooks/useUpdater";
import { doctorHasWarnings } from "@/lib/settings-model";
import {
  LogsRowGlyph,
  StorageRowGlyph,
  TeamsRowGlyph,
  TelemetryRowGlyph,
  ToolsRowGlyph,
  UpdatesRowGlyph,
  WorkflowRowGlyph,
} from "./glyphs";
import type { ReactNode } from "react";

// Settings — the daemon configuration hub (STUDIO-681 §8), built by STUDIO-683.
//
// The Teams row is the load-bearing one: with Teams OFF it is the ONLY discovery path to the
// feature, because §2.2 removes Teams and Memory from the rail entirely (§10 box 2.5).
//
// The Tools, Logs and Updates rows are the §8.1 parity amendment (STUDIO-691): the shipped Podium
// Settings nav is `general · projects · teams · tools · logs · updates`, and the §2.2.1 flip may
// not drop three of those on the floor. Each opens the SHIPPED tab in console chrome
// (`SettingsTabView`), the way the Workflow row opens the shipped config editor.
export function SettingsView({
  teamsEnabled,
  updater,
  onManageTeam,
  onEditWorkflow,
  onOpen,
}: {
  teamsEnabled: boolean;
  /** The shell-owned update model — lights the Updates row while an update is waiting (P11 U3). */
  updater: Updater;
  onManageTeam: () => void;
  onEditWorkflow: () => void;
  /** Open one of the Settings child routes that embeds a shipped Podium tab (§8.1). */
  onOpen: (route: "tools" | "logs" | "updates") => void;
}) {
  return (
    <section>
      <div className="head">
        <h1>Settings</h1>
      </div>
      <p className="lead">
        Daemon configuration. The Teams config has its own form — everything else follows{" "}
        <code>WORKFLOW.md</code>.
      </p>
      <div className="setgrp">
        {teamsEnabled ? <TeamsOnRow onManageTeam={onManageTeam} /> : <TeamsOffRow />}
        <WorkflowRow onEdit={onEditWorkflow} />
        <ToolsRow onOpen={() => onOpen("tools")} />
        <LogsRow onOpen={() => onOpen("logs")} />
        <UpdatesRow pending={updater.pending} onOpen={() => onOpen("updates")} />
        <StorageRow />
        <TelemetryRow />
      </div>
    </section>
  );
}

function SettingRow({
  icon,
  title,
  detail,
  action,
}: {
  icon: ReactNode;
  title: string;
  detail: ReactNode;
  action: ReactNode;
}) {
  return (
    <div className="setrow">
      <span className="ic2">{icon}</span>
      <div className="tx">
        <b>{title}</b>
        <p>{detail}</p>
      </div>
      <div className="rt">{action}</div>
    </div>
  );
}

function TeamsOnRow({ onManageTeam }: { onManageTeam: () => void }) {
  const config = useTeamsConfigQuery();
  const size = config.data?.config.roster.length ?? 0;
  return (
    <SettingRow
      icon={<TeamsRowGlyph />}
      title="Teams"
      detail={`Roster, manager, memory, quorum — enabled${size === 0 ? "" : `, ${size} ${size === 1 ? "teammate" : "teammates"}`}.`}
      action={
        <Button variant="sec" onClick={onManageTeam}>
          Manage team →
        </Button>
      }
    />
  );
}

/**
 * The "Enable teams" card (§8, §10 box 2.5). It does NOT flip the daemon: `teams.enabled` is
 * boot-loaded (§2.2), the write surface is the manage-team form of §7 (sub-ticket 5), and the
 * one thing this slice must not do is claim a live toggle. So it explains the feature, states
 * the restart, and points at the file — a promise the console can keep today.
 */
function TeamsOffRow() {
  const config = useTeamsConfigQuery();
  return (
    <div className="setrow">
      <span className="ic2">
        <TeamsRowGlyph />
      </span>
      <div className="tx">
        <b>Teams</b>
        <p>
          Off — the daemon runs solo, one agent per issue. Enable to route work to named teammates
          with a shared room, memory, and review quorum.
        </p>
        <Note variant="info">
          Teams is read once at start-up, so enabling it means setting <code>enabled: true</code> in{" "}
          <code>{config.data?.path ?? "~/.rhapsody/teams.yaml"}</code> and restarting the daemon.
          Changes apply on restart.
        </Note>
      </div>
      <div className="rt">
        <Pill variant="queued">off</Pill>
      </div>
    </div>
  );
}

// GET /api/v1/config returns the WORKFLOW.md front matter and prompt body, but not the file's
// path, so the row names the documented default rather than claiming to have read a location.
//
// "Edit →" opens the console's WORKFLOW.md editor (STUDIO-690). Until it existed this row was
// inert, which is exactly the parity gap §2.2.1 refuses to flip over: the shipped Podium Settings
// can edit WORKFLOW.md, so the console has to be able to before it becomes the dashboard.
function WorkflowRow({ onEdit }: { onEdit: () => void }) {
  return (
    <SettingRow
      icon={<WorkflowRowGlyph />}
      title="Workflow"
      detail={<code>~/.rhapsody/WORKFLOW.md</code>}
      action={
        <Button variant="sec" onClick={onEdit}>
          Edit →
        </Button>
      }
    />
  );
}

/**
 * Tools — the tool-doctor row (audit G4). It carries the amber warning Pill the Podium rail's Tools
 * item carries, off the SAME shared `useToolDoctor` cache entry, so a missing binary is visible from
 * the Settings hub without opening the tab. Mounting the query here also means the probe runs as
 * soon as Settings opens, exactly as it does in the Podium shell.
 */
function ToolsRow({ onOpen }: { onOpen: () => void }) {
  const doctor = useToolDoctor();
  const warn = doctorHasWarnings(doctor.data ?? []);
  return (
    <SettingRow
      icon={<ToolsRowGlyph />}
      title="Tools"
      detail="Required CLIs and connection health, re-checked on launch."
      action={
        <>
          {warn ? <Pill variant="review">needs attention</Pill> : null}
          <Button variant="sec" aria-label="Open Tools" onClick={onOpen}>
            Open →
          </Button>
        </>
      }
    />
  );
}

/** Logs — the live daemon log tail (audit G5). */
function LogsRow({ onOpen }: { onOpen: () => void }) {
  return (
    <SettingRow
      icon={<LogsRowGlyph />}
      title="Logs"
      detail="Live daemon process log — polling, dispatch, restarts and errors."
      action={
        <Button variant="sec" aria-label="Open Logs" onClick={onOpen}>
          Open →
        </Button>
      }
    />
  );
}

/**
 * Updates — the desktop auto-update row (audit G3, P11 U3). The Pill is the console's echo of the
 * Podium rail's rust "available" dot: it lights off the shell-owned updater, so a pending update is
 * discoverable from the hub. Outside the desktop app the updater stays idle and never lights.
 */
function UpdatesRow({ pending, onOpen }: { pending: boolean; onOpen: () => void }) {
  return (
    <SettingRow
      icon={<UpdatesRowGlyph />}
      title="Updates"
      detail="Check for, download and install new versions of the desktop app."
      action={
        <>
          {pending ? <Pill variant="review">update available</Pill> : null}
          <Button variant="sec" aria-label="Open Updates" onClick={onOpen}>
            Open →
          </Button>
        </>
      }
    />
  );
}

function StorageRow() {
  return (
    <SettingRow
      icon={<StorageRowGlyph />}
      title="Storage"
      detail="Durable run history — SQLite, WAL."
      action={null}
    />
  );
}

function TelemetryRow() {
  return (
    <SettingRow
      icon={<TelemetryRowGlyph />}
      title="Telemetry"
      detail="OpenTelemetry export — off by default."
      action={null}
    />
  );
}
