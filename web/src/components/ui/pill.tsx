import * as React from "react";
import { StatusDot } from "./status-dot";

// Podium pill tones. The legacy names (`emerald`/`sky`) are kept so the not-yet-restructured
// screens keep compiling, but their VISUALS are re-pointed onto the warm palette: `emerald`
// (the old success/accent tone) → sage, `sky` (the old info tone) → slate. `rust`/`sage`/`slate`
// are the canonical Podium names.
export type PillTone = "neutral" | "emerald" | "amber" | "sky" | "red" | "rust" | "sage" | "slate";

interface ToneStyle {
  bg: string;
  c: string;
}

const TONES: Record<PillTone, ToneStyle> = {
  neutral: { bg: "var(--tint-neutral)", c: "var(--neutral)" },
  emerald: { bg: "var(--tint-sage)", c: "var(--sage)" },
  sage: { bg: "var(--tint-sage)", c: "var(--sage)" },
  amber: { bg: "var(--tint-amber)", c: "var(--amber)" },
  sky: { bg: "var(--tint-slate)", c: "var(--slate)" },
  slate: { bg: "var(--tint-slate)", c: "var(--slate)" },
  red: { bg: "var(--tint-red)", c: "var(--red)" },
  rust: { bg: "var(--tint-rust)", c: "var(--rust-text)" },
};

export interface PillProps extends React.HTMLAttributes<HTMLSpanElement> {
  tone?: PillTone;
  /** Prepend a 5px status dot in the tone color. */
  dot?: boolean;
}

// Pill — small generic badge with a 10–12% tonal tint fill (Podium spec: 11px, radius 999,
// optional 5px dot, hairline border tinted from the tone color).
export function Pill({ tone = "neutral", dot, children, style, ...rest }: PillProps) {
  const t = TONES[tone];
  return (
    <span
      style={{
        display: "inline-flex",
        alignItems: "center",
        gap: 5,
        height: 21,
        padding: "0 9px",
        borderRadius: "var(--r-pill)",
        background: t.bg,
        color: t.c,
        border: `1px solid color-mix(in srgb, ${t.c} 22%, transparent)`,
        fontSize: 11,
        fontWeight: 600,
        whiteSpace: "nowrap",
        ...style,
      }}
      {...rest}
    >
      {dot ? <StatusDot color={t.c} size={5} /> : null}
      {children}
    </span>
  );
}
