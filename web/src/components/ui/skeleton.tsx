import * as React from "react";

export interface SkeletonProps {
  w?: number | string;
  h?: number | string;
  r?: number | string;
  style?: React.CSSProperties;
  className?: string;
}

// Skeleton — shimmering placeholder bar (package `shimmer` keyframe), ported from
// `ui.jsx` (exported there as `Skel`).
export function Skeleton({ w = "100%", h = 14, r = 6, style, className }: SkeletonProps) {
  return (
    <div
      className={className}
      data-testid="skeleton"
      style={{
        width: w,
        height: h,
        borderRadius: r,
        background:
          "linear-gradient(90deg, rgba(255,255,255,.04) 25%, rgba(255,255,255,.09) 50%, rgba(255,255,255,.04) 75%)",
        backgroundSize: "680px 100%",
        animation: "shimmer 1.4s linear infinite",
        ...style,
      }}
    />
  );
}

// Backwards-compatible alias matching the package's export name.
export { Skeleton as Skel };

// SkeletonCard — a card-shaped loading placeholder, ported from `ui.jsx`.
export function SkeletonCard() {
  return (
    <div
      style={{
        background: "var(--bg-card)",
        border: "1px solid var(--line)",
        borderRadius: "var(--r-card)",
        boxShadow: "var(--shadow-card)",
      }}
    >
      <div
        style={{
          padding: "18px 22px 16px",
          borderBottom: "1px solid var(--line-2)",
          display: "flex",
          gap: 12,
          alignItems: "center",
        }}
      >
        <Skeleton w={30} h={30} r={8} />
        <div style={{ display: "flex", flexDirection: "column", gap: 7 }}>
          <Skeleton w={140} h={13} />
          <Skeleton w={230} h={10} />
        </div>
      </div>
      <div style={{ padding: 22, display: "flex", flexDirection: "column", gap: 20 }}>
        {[0, 1, 2].map((i) => (
          <div key={i} style={{ display: "flex", flexDirection: "column", gap: 8 }}>
            <Skeleton w={100} h={11} />
            <Skeleton w="100%" h={40} r={9} />
          </div>
        ))}
      </div>
    </div>
  );
}
