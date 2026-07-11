import * as React from "react";
import type { IconComponent } from "./icons";

export interface SectionCardProps {
  title: React.ReactNode;
  icon?: IconComponent;
  desc?: React.ReactNode;
  action?: React.ReactNode;
  children?: React.ReactNode;
  style?: React.CSSProperties;
}

// SectionCard — a titled card with an optional icon chip, description, and header action,
// and a gutter-spaced body. Ported from `ui.jsx`.
export function SectionCard({ title, icon: Icon, desc, action, children, style }: SectionCardProps) {
  return (
    <div
      style={{
        background: "var(--bg-card)",
        border: "1px solid var(--line)",
        borderRadius: "var(--r-card)",
        boxShadow: "var(--shadow-card)",
        ...style,
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
          {Icon ? (
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
              <Icon size={15} />
            </div>
          ) : null}
          <div>
            <div style={{ fontSize: 14.5, fontWeight: 600, color: "var(--tx)", letterSpacing: "-0.01em" }}>
              {title}
            </div>
            {desc ? (
              <div
                style={{
                  fontSize: 12.5,
                  color: "var(--tx-3)",
                  marginTop: 3,
                  lineHeight: 1.5,
                  maxWidth: 560,
                }}
              >
                {desc}
              </div>
            ) : null}
          </div>
        </div>
        {action}
      </div>
      <div style={{ padding: 22, display: "flex", flexDirection: "column", gap: "var(--gut)" }}>{children}</div>
    </div>
  );
}
