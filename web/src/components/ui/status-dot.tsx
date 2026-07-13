export interface StatusDotProps {
  /** Any CSS color (typically a `var(--…)` token). */
  color?: string;
  /** Animate the Podium `pulse` (opacity 1→.35→1) used for live/running states. */
  pulse?: boolean;
  size?: number;
  className?: string;
}

// StatusDot — a small colored dot. Live states get the Podium `pulse` (a gentle opacity
// breath at 2.4s, honored down to nothing under prefers-reduced-motion via the global guard
// in index.css). Rust is the canonical live color; any tone may be passed.
export function StatusDot({ color = "var(--rust-text)", pulse = false, size = 7, className }: StatusDotProps) {
  return (
    <span
      className={className}
      data-pulse={pulse ? "true" : undefined}
      style={{
        width: size,
        height: size,
        borderRadius: "50%",
        background: color,
        flexShrink: 0,
        animation: pulse ? "pulse 2.4s ease-in-out infinite" : "none",
      }}
    />
  );
}
