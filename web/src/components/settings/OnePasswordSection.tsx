// OnePasswordSection — the desktop-only 1Password service-account credential surface (STUDIO-1584),
// rendered inside Settings → Providers beside the provider credential cards.
//
// A project binds a 1Password credential by a plain ID in WORKFLOW.md (`onepassword: <id>`); the
// token for that id is stored here, through the desktop app, into the system credential store. The
// browser dashboard has no bridge, so it renders the shared "desktop app required" message rather
// than pretending to offer a mutation.
//
// The entered token is held in React state only long enough to submit it, then the field is cleared
// BEFORE the async command resolves; it is never written to localStorage, history, or a log. The
// honest guarantee is exactly that (an immutable JS string cannot be proven zeroized) — Rust owns
// validation, zeroization, and everything after the call.

import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { Button, Key, SectionCard, TextInput } from "@/components/ui";
import { fetchTypedConfig } from "@/lib/api";
import { DESKTOP_ONLY_MESSAGE } from "@/lib/providers-model";
import { failureMessage } from "@/lib/provider-credentials";
import {
  boundOnePasswordIds,
  hasOnePasswordBridge,
  onePasswordConnect,
  onePasswordRemove,
  onePasswordReplace,
  onePasswordStatus,
  type OnePasswordStatus,
} from "@/lib/onepassword-credentials";

function statusTone(status: OnePasswordStatus | undefined): string {
  if (!status) return "var(--tx-3)";
  switch (status.status) {
    case "configured":
      return "var(--green, var(--tx-1))";
    case "unavailable":
      return "var(--red)";
    default:
      return "var(--tx-3)";
  }
}

function statusLabel(status: OnePasswordStatus | undefined): string {
  if (!status) return "checking…";
  switch (status.status) {
    case "configured":
      return "configured";
    case "unavailable":
      return "unavailable";
    default:
      return "not connected";
  }
}

export function OnePasswordSection() {
  const bridged = hasOnePasswordBridge();
  const config = useQuery({
    queryKey: ["typed-config-onepassword"],
    queryFn: fetchTypedConfig,
    enabled: bridged,
  });
  const ids = boundOnePasswordIds(config.data?.projects);

  const statuses = useQuery({
    queryKey: ["onepassword-statuses", ids],
    queryFn: async () => {
      const entries = await Promise.all(
        ids.map(async (id) => [id, await onePasswordStatus(id)] as const),
      );
      return Object.fromEntries(entries) as Record<string, OnePasswordStatus>;
    },
    enabled: bridged && ids.length > 0,
  });

  return (
    <SectionCard
      title="1Password service accounts"
      icon={Key}
      desc="Each project binds a 1Password service-account credential by id in WORKFLOW.md. Store the token for that id here; it never enters WORKFLOW.md, the database, logs, or a prompt. Agents with no binding get no token and never prompt for Touch ID."
    >
      {!bridged && <p style={{ fontSize: 12, color: "var(--tx-3)" }}>{DESKTOP_ONLY_MESSAGE}</p>}
      {bridged && ids.length === 0 && (
        <p style={{ fontSize: 13, color: "var(--tx-3)" }}>
          No project binds a 1Password credential. Add <code>onepassword: &lt;id&gt;</code> to a
          project in WORKFLOW.md, then store the token here.
        </p>
      )}
      {bridged && ids.length > 0 && (
        <div style={{ display: "flex", flexDirection: "column", gap: 12 }}>
          {ids.map((id) => (
            <OnePasswordRow
              key={id}
              id={id}
              status={statuses.data?.[id]}
              onChanged={() => void statuses.refetch()}
            />
          ))}
        </div>
      )}
    </SectionCard>
  );
}

interface OnePasswordRowProps {
  id: string;
  status: OnePasswordStatus | undefined;
  onChanged: () => void;
}

function OnePasswordRow({ id, status, onChanged }: OnePasswordRowProps) {
  const [editing, setEditing] = useState<"none" | "connect" | "replace">("none");
  const [token, setToken] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const submit = async (operation: "connect" | "replace") => {
    const secret = token;
    setToken("");
    setError(null);
    if (!secret) {
      setError("Enter a service-account token first.");
      return;
    }
    setBusy(true);
    try {
      if (operation === "connect") {
        await onePasswordConnect(id, secret);
      } else {
        await onePasswordReplace(id, secret);
      }
      setEditing("none");
      onChanged();
    } catch (e) {
      setError(failureMessage(e));
    } finally {
      setBusy(false);
    }
  };

  const remove = async () => {
    setError(null);
    setBusy(true);
    try {
      await onePasswordRemove(id);
      onChanged();
    } catch (e) {
      setError(failureMessage(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div
      style={{
        border: "1px solid var(--line)",
        borderRadius: "var(--r-ctrl)",
        padding: 12,
        display: "flex",
        flexDirection: "column",
        gap: 8,
      }}
    >
      <div style={{ display: "flex", alignItems: "center", justifyContent: "space-between", gap: 12 }}>
        <div style={{ display: "flex", flexDirection: "column", gap: 2 }}>
          <code style={{ fontSize: 13 }}>{id}</code>
          <span style={{ fontSize: 12, color: statusTone(status) }}>{statusLabel(status)}</span>
        </div>
        <div style={{ display: "flex", gap: 8 }}>
          {status?.can_connect !== false && (
            <Button variant="subtle" size="sm" onClick={() => setEditing("connect")} disabled={busy}>
              Connect
            </Button>
          )}
          {status?.can_replace && (
            <Button variant="subtle" size="sm" onClick={() => setEditing("replace")} disabled={busy}>
              Replace
            </Button>
          )}
          {status?.can_remove && (
            <Button variant="subtle" size="sm" onClick={() => void remove()} disabled={busy}>
              Remove
            </Button>
          )}
        </div>
      </div>
      {editing !== "none" && (
        <div style={{ display: "flex", gap: 8 }}>
          <TextInput
            mono
            type="password"
            placeholder="ops_…"
            value={token}
            onChange={(e) => setToken(e.target.value)}
            autoComplete="off"
            spellCheck={false}
          />
          <Button variant="primary" size="sm" onClick={() => void submit(editing)} disabled={busy}>
            Save
          </Button>
          <Button
            variant="subtle"
            size="sm"
            onClick={() => {
              setToken("");
              setEditing("none");
            }}
            disabled={busy}
          >
            Cancel
          </Button>
        </div>
      )}
      {error && <p style={{ fontSize: 12, color: "var(--red)" }}>{error}</p>}
    </div>
  );
}
