import { Button, Note, Pill } from "@/components/console";
import { useTeamsConfigQuery } from "@/hooks/useTeams";
import {
  StorageRowGlyph,
  TeamsRowGlyph,
  TelemetryRowGlyph,
  WorkflowRowGlyph,
} from "./glyphs";
import type { ReactNode } from "react";

// Settings — the daemon configuration hub (STUDIO-681 §8), built by STUDIO-683.
//
// The Teams row is the load-bearing one: with Teams OFF it is the ONLY discovery path to the
// feature, because §2.2 removes Teams and Memory from the rail entirely (§10 box 2.5).
export function SettingsView({
  teamsEnabled,
  onManageTeam,
}: {
  teamsEnabled: boolean;
  onManageTeam: () => void;
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
        <WorkflowRow />
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
function WorkflowRow() {
  return (
    <SettingRow
      icon={<WorkflowRowGlyph />}
      title="Workflow"
      detail={<code>~/.rhapsody/WORKFLOW.md</code>}
      action={null}
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
