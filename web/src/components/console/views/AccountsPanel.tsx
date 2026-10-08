import type { AccountView } from "@/lib/api";
import { Card, Chip, Pill, PILL_COLORS, type PillVariant } from "@/components/console";
import { useAccounts } from "@/hooks/useAccounts";
import { useEffect, useState } from "react";

const variants: Record<string, PillVariant> = { ok: "run", warn: "review", "stop-new": "review", handoff: "blocked", wall: "blocked", stale: "queued" };

function countdown(reset: number, now: number): string {
  if (reset <= 0) return "reset unknown";
  if (reset <= now) return "reset reached";
  const minutes = Math.ceil((reset - now) / 60);
  return minutes < 60 ? `resets in ${minutes}m` : `resets in ${Math.floor(minutes / 60)}h ${minutes % 60}m`;
}

export function AccountsPanel({ accounts, nowS }: { accounts: AccountView[]; nowS: number }) {
  return <Card title="Accounts" sub="usage limits · today’s costs">
    {accounts.length === 0 ? <div className="empty">No account observations yet. Limits appear when work reports usage.</div> : accounts.map((account) => {
      const level = account.level ?? "unknown";
      const label = account.stale ? "stale" : level;
      const variant = variants[label] ?? "queued";
      const kind = account.cost_kind === "api_equivalent" ? "API-equivalent" : "USD";
      return <section key={account.account} aria-label={account.account} style={{ padding: 18, borderBottom: "1px solid var(--line)" }}>
        <div style={{ display: "flex", gap: 12, alignItems: "center", flexWrap: "wrap" }}>
          <b>{account.account}</b>
          {account.source === "budget" ? <Chip disabled>Rhapsody budget</Chip> : null}
          <Pill variant={variant}>{label}</Pill>
          <Chip disabled>{account.detection.replaceAll("_", "-")}</Chip>
          {account.using_credits ? <Pill variant="blocked">credits in use</Pill> : <span className="sub">credits off</span>}
          <span className="mono">{account.today_usd == null ? `${kind} today unknown` : `$${account.today_usd.toFixed(2)} ${kind} today`}</span>
        </div>
        {account.stale ? <p className="sub">last known {level} · last observed {new Date(account.last_seen_s * 1000).toLocaleString()}</p> : null}
        {account.stale_reason ? <p className="sub">Probe unavailable: {account.stale_reason.replaceAll("_", " ")}</p> : null}
        {account.windows.length === 0 ? <p className="sub">Utilization and reset not yet observed.</p> : account.windows.map((window) => {
          const percent = Math.round(Math.max(0, Math.min(1, window.utilization)) * 100);
          return <div key={window.window} style={{ marginTop: 14 }}>
            <div style={{ display: "flex", justifyContent: "space-between", gap: 12 }}>
              <span>{window.window.replaceAll("_", " ")} · {percent}%</span>
              <span className="sub" title={window.resets_at_s > 0 ? new Date(window.resets_at_s * 1000).toLocaleString() : undefined}>{countdown(window.resets_at_s, nowS)}</span>
            </div>
            <div role="progressbar" aria-label={`${account.account} ${window.window}`} aria-valuemin={0} aria-valuemax={100} aria-valuenow={percent} style={{ height: 6, background: "var(--line)", borderRadius: 3, overflow: "hidden", marginTop: 6 }}>
              <div style={{ width: `${percent}%`, height: "100%", background: PILL_COLORS[variant] }} />
            </div>
          </div>;
        })}
      </section>;
    })}
  </Card>;
}

export function AccountsPage() {
  const query = useAccounts();
  const [nowS, setNowS] = useState(() => Math.floor(Date.now() / 1000));
  useEffect(() => { const timer = window.setInterval(() => setNowS(Math.floor(Date.now() / 1000)), 1000); return () => window.clearInterval(timer); }, []);
  return <section>
    <h1>Accounts</h1>
    {query.isPending ? <div className="empty">Loading accounts…</div> : query.isError ? <div role="alert">Account usage could not be read.</div> : <AccountsPanel accounts={query.data ?? []} nowS={nowS} />}
  </section>;
}
