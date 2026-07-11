import * as React from "react";
import { Button } from "@/components/ui/button";

export interface ConfirmDialogProps {
  open: boolean;
  title: string;
  body?: string;
  confirmLabel: string;
  danger?: boolean;
  busy?: boolean;
  onConfirm: () => void;
  onClose: () => void;
}

// ConfirmDialog — a minimal centered modal (no shared primitive existed). Escape / overlay click
// cancels; the confirm button is danger-tinted when `danger`. Rendered only when `open`. Styled
// with the shell's inline CSS-var idiom (mirrors AddAgentSheet's overlay + card surface vars).
export function ConfirmDialog({ open, title, body, confirmLabel, danger, busy, onConfirm, onClose }: ConfirmDialogProps) {
  React.useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [open, onClose]);
  if (!open) return null;
  return (
    <div
      role="presentation"
      onClick={onClose}
      style={{ position: "fixed", inset: 0, zIndex: 500, background: "rgba(0,0,0,0.5)", display: "flex", alignItems: "center", justifyContent: "center" }}
    >
      <div
        role="dialog"
        aria-modal="true"
        aria-label={title}
        onClick={(e) => e.stopPropagation()}
        style={{
          width: 420,
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
        <div style={{ fontSize: 16, fontWeight: 600, color: "var(--tx)" }}>{title}</div>
        {body ? <div style={{ fontSize: 13, color: "var(--tx-3)", lineHeight: 1.5 }}>{body}</div> : null}
        <div style={{ display: "flex", justifyContent: "flex-end", gap: 8, marginTop: 4 }}>
          <Button type="button" variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button type="button" variant={danger ? "danger" : "primary"} disabled={busy} onClick={onConfirm}>
            {busy ? "Working…" : confirmLabel}
          </Button>
        </div>
      </div>
    </div>
  );
}
