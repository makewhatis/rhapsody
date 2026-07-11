import * as React from "react";
import { Check, type IconComponent, Refresh } from "@/components/ui";

export type OverrideMode = "quiet" | "chip";

type PillTone = "neutral" | "emerald";

const PILL_TONES: Record<PillTone, { bg: string; c: string; b: string; hb: string }> = {
  neutral: { bg: "rgba(255,255,255,.05)", c: "var(--tx-2)", b: "var(--line-strong)", hb: "var(--bg-hover)" },
  emerald: { bg: "var(--em-soft)", c: "var(--em-bright)", b: "rgba(16,185,129,.3)", hb: "rgba(16,185,129,.16)" },
};

// PillButton — the small Inherited/Overridden toggle pill used by the chip-mode OverrideField.
function PillButton({
  tone,
  onClick,
  children,
  icon: Icon,
}: {
  tone: PillTone;
  onClick: () => void;
  children: React.ReactNode;
  icon?: IconComponent;
}) {
  const t = PILL_TONES[tone];
  const [hover, setHover] = React.useState(false);
  return (
    <button
      type="button"
      onClick={onClick}
      onMouseEnter={() => setHover(true)}
      onMouseLeave={() => setHover(false)}
      style={{
        display: "inline-flex",
        alignItems: "center",
        gap: 5,
        height: 24,
        padding: "0 9px",
        borderRadius: "var(--r-pill)",
        background: hover ? t.hb : t.bg,
        color: t.c,
        border: `1px solid ${t.b}`,
        fontSize: 11.5,
        fontWeight: 600,
        cursor: "pointer",
        transition: "all .12s",
        whiteSpace: "nowrap",
      }}
    >
      {Icon ? <Icon size={12} /> : null}
      {children}
    </button>
  );
}

export interface OverrideFieldProps {
  label: string;
  hint?: string;
  /** The global default shown when the field is inherited (already humanized for display). */
  globalLabel: string;
  overridden: boolean;
  onOverride: () => void;
  onReset: () => void;
  /** The editable control (a Select) rendered when the field is overridden. */
  control: React.ReactNode;
  mode: OverrideMode;
}

// OverrideField — the inherit-vs-override row (the design's centerpiece). Two treatments: the
// "quiet" default ("Using global default `X` · Override", emerald left-bar + "Reset to global
// default" when overridden) and the "chip" mode (an Inherited/Overridden pill + a dashed
// read-only box). Presence of the override drives which side renders; the parent owns the sparse
// `overrides` map (Override seeds the global value, Reset deletes the key).
export function OverrideField({ label, hint, globalLabel, overridden, onOverride, onReset, control, mode }: OverrideFieldProps) {
  const left = (
    <div>
      <div style={{ display: "flex", alignItems: "center", gap: 9 }}>
        <label style={{ fontSize: 13, fontWeight: 500, color: "var(--tx)" }}>{label}</label>
        {mode === "chip" ? (
          overridden ? (
            <PillButton tone="emerald" onClick={onReset} icon={Check}>
              Overridden
            </PillButton>
          ) : (
            <PillButton tone="neutral" onClick={onOverride}>
              Inherited
            </PillButton>
          )
        ) : null}
      </div>
      {hint ? <div style={{ fontSize: 12, color: "var(--tx-3)", marginTop: 3, lineHeight: 1.45 }}>{hint}</div> : null}
    </div>
  );

  let right: React.ReactNode;
  if (!overridden) {
    if (mode === "chip") {
      right = (
        <div
          className="mono"
          style={{
            display: "flex",
            alignItems: "center",
            height: 40,
            padding: "0 13px",
            borderRadius: "var(--r-ctrl)",
            background: "rgba(255,255,255,.02)",
            border: "1px dashed var(--line-strong)",
            color: "var(--tx-3)",
            fontSize: 13,
          }}
        >
          {globalLabel}
        </div>
      );
    } else {
      right = (
        <div style={{ display: "flex", alignItems: "center", gap: 8, flexWrap: "wrap", minHeight: 40, fontSize: 12.5, color: "var(--tx-3)" }}>
          Using global default
          <span
            className="mono"
            style={{
              fontSize: 12,
              color: "var(--tx-2)",
              background: "var(--bg-input)",
              border: "1px solid var(--line)",
              padding: "3px 8px",
              borderRadius: 6,
            }}
          >
            {globalLabel}
          </span>
          <span style={{ color: "var(--tx-faint)" }}>·</span>
          <button
            type="button"
            onClick={onOverride}
            style={{ background: "transparent", border: "none", color: "var(--em-bright)", fontSize: 12.5, fontWeight: 600, cursor: "pointer", padding: 0 }}
          >
            Override
          </button>
        </div>
      );
    }
  } else {
    right = (
      <div style={{ display: "flex", flexDirection: "column", gap: 8 }}>
        <div style={{ position: "relative", paddingLeft: mode === "chip" ? 0 : 11 }}>
          {mode !== "chip" ? (
            <span
              aria-hidden
              style={{ position: "absolute", left: 0, top: 2, bottom: 2, width: 2, borderRadius: 2, background: "var(--em-bright)" }}
            />
          ) : null}
          {control}
        </div>
        <button
          type="button"
          onClick={onReset}
          style={{
            alignSelf: "flex-start",
            background: "transparent",
            border: "none",
            color: "var(--tx-3)",
            fontSize: 12,
            fontWeight: 500,
            cursor: "pointer",
            padding: 0,
            display: "inline-flex",
            alignItems: "center",
            gap: 5,
          }}
        >
          <Refresh size={12} />
          Reset to global default
        </button>
      </div>
    );
  }

  return (
    <div style={{ display: "grid", gridTemplateColumns: "minmax(0,1fr) 320px", gap: 24, alignItems: "start" }}>
      {left}
      {right}
    </div>
  );
}
