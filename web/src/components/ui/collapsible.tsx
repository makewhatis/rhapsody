import * as React from "react";
import { ChevronRight } from "./icons";
import type { IconComponent } from "./icons";

export interface CollapsibleProps {
  label: React.ReactNode;
  icon?: IconComponent;
  defaultOpen?: boolean;
  children?: React.ReactNode;
  badge?: React.ReactNode;
}

// Collapsible — disclosure row with a rotating chevron and optional leading icon + badge.
// Ported from `ui.jsx`; `aria-expanded` reflects the open state.
export function Collapsible({ label, icon: Icon, defaultOpen = false, children, badge }: CollapsibleProps) {
  const [open, setOpen] = React.useState(defaultOpen);
  return (
    <div
      style={{
        border: "1px solid var(--line)",
        borderRadius: "var(--r-ctrl)",
        overflow: "hidden",
        background: "var(--bg-card-2)",
      }}
    >
      <button
        type="button"
        aria-expanded={open}
        onClick={() => setOpen(!open)}
        style={{
          width: "100%",
          display: "flex",
          alignItems: "center",
          gap: 9,
          padding: "13px 15px",
          background: "transparent",
          border: "none",
          cursor: "pointer",
          color: "var(--tx)",
          textAlign: "left",
        }}
      >
        <ChevronRight
          size={15}
          style={{ color: "var(--tx-3)", transition: "transform .15s", transform: open ? "rotate(90deg)" : "none" }}
        />
        {Icon ? <Icon size={14} style={{ color: "var(--tx-3)" }} /> : null}
        <span style={{ fontSize: 13, fontWeight: 500, flex: 1 }}>{label}</span>
        {badge}
      </button>
      {open ? (
        <div style={{ padding: "4px 15px 16px 38px", display: "flex", flexDirection: "column", gap: "var(--gut)" }}>
          {children}
        </div>
      ) : null}
    </div>
  );
}
