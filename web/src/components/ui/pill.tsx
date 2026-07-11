import * as React from "react";

export type PillTone = "neutral" | "emerald" | "amber" | "sky" | "red";

interface ToneStyle {
  bg: string;
  c: string;
  b: string;
}

const TONES: Record<PillTone, ToneStyle> = {
  neutral: { bg: "rgba(255,255,255,.05)", c: "var(--tx-2)", b: "var(--line)" },
  emerald: { bg: "var(--em-soft)", c: "var(--em-bright)", b: "rgba(16,185,129,.25)" },
  amber: { bg: "var(--amber-soft)", c: "var(--amber)", b: "rgba(245,181,68,.25)" },
  sky: { bg: "var(--sky-soft)", c: "var(--sky)", b: "rgba(56,189,248,.25)" },
  red: { bg: "var(--red-soft)", c: "var(--red)", b: "rgba(239,83,80,.25)" },
};

export interface PillProps extends React.HTMLAttributes<HTMLSpanElement> {
  tone?: PillTone;
}

// Pill — small generic badge with tonal fills, ported from `ui.jsx`.
export function Pill({ tone = "neutral", children, style, ...rest }: PillProps) {
  const t = TONES[tone];
  return (
    <span
      style={{
        display: "inline-flex",
        alignItems: "center",
        gap: 5,
        height: 22,
        padding: "0 9px",
        borderRadius: "var(--r-pill)",
        background: t.bg,
        color: t.c,
        border: `1px solid ${t.b}`,
        fontSize: 11.5,
        fontWeight: 600,
        ...style,
      }}
      {...rest}
    >
      {children}
    </span>
  );
}
