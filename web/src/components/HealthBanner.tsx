import { RefreshCw } from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";

interface Props {
  status: "ok" | "degraded" | "loading" | "error";
  pollIntervalMs?: number;
  onRefresh: () => void;
  refreshing: boolean;
}

export function HealthBanner({ status, pollIntervalMs, onRefresh, refreshing }: Props) {
  const variant =
    status === "ok" ? "default" : status === "degraded" ? "muted" : status === "error" ? "destructive" : "secondary";
  const label =
    status === "ok"
      ? "Healthy"
      : status === "degraded"
        ? "Degraded"
        : status === "error"
          ? "Unreachable"
          : "Loading";
  return (
    <header className="flex items-center justify-between border-b border-[var(--border)] pb-4">
      <div className="flex items-center gap-3">
        <h1 className="text-xl font-semibold tracking-tight">Symphony</h1>
        <Badge variant={variant}>{label}</Badge>
        {pollIntervalMs != null && (
          <span className="text-xs text-[var(--muted-foreground)]">
            poll {Math.round(pollIntervalMs / 1000)}s
          </span>
        )}
      </div>
      <Button variant="outline" size="sm" onClick={onRefresh} disabled={refreshing}>
        <RefreshCw className={cn("h-4 w-4", refreshing && "animate-spin")} />
        Refresh
      </Button>
    </header>
  );
}
