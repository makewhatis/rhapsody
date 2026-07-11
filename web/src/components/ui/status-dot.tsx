import * as React from "react";

export interface StatusDotProps {
  /** Any CSS color (typically a `var(--…)` token). */
  color?: string;
  /** Animate the emerald `pulseDot` halo (used for live/running states). */
  pulse?: boolean;
  size?: number;
  className?: string;
}

// StatusDot — a small colored dot with the package's `pulseDot` halo animation.
// Ported from `ui.jsx`: the pulse halo colour derives from the dot colour (emerald
// glow for the accent dot, a neutral grey otherwise).
export function StatusDot({ color = "var(--em-bright)", pulse = false, size = 8, className }: StatusDotProps) {
  return (
    <span
      className={className}
      data-pulse={pulse ? "true" : undefined}
      style={
        {
          width: size,
          height: size,
          borderRadius: "50%",
          background: color,
          flexShrink: 0,
          "--pulse-color": color === "var(--em-bright)" ? "var(--em-glow)" : "rgba(120,120,120,.3)",
          animation: pulse ? "pulseDot 1.8s ease-out infinite" : "none",
          boxShadow: pulse ? "none" : "0 0 0 0 transparent",
        } as React.CSSProperties
      }
    />
  );
}
