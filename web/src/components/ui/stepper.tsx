import * as React from "react";

export interface StepperProps {
  value: number;
  onChange: (n: number) => void;
  min?: number;
  max?: number;
  suffix?: React.ReactNode;
  style?: React.CSSProperties;
}

// Stepper — numeric input with ▲▼ spinners, clamped to [min, max]. Ported from `ui.jsx`
// (mono input font, hairline-split spinner column). Spinner buttons carry aria-labels for
// assistive tech / testing.
export function Stepper({ value, onChange, min = 0, max = 999, suffix, style }: StepperProps) {
  const v = Number(value);
  const set = (n: number) => onChange(Math.max(min, Math.min(max, n)));
  return (
    <div
      style={{
        display: "flex",
        alignItems: "center",
        height: 40,
        width: 320,
        background: "var(--bg-input)",
        border: "1px solid var(--line)",
        borderRadius: "var(--r-ctrl)",
        overflow: "hidden",
        ...style,
      }}
    >
      <input
        type="text"
        value={value}
        onChange={(e) => {
          const n = parseInt(e.target.value.replace(/[^0-9]/g, ""), 10);
          set(isNaN(n) ? min : n);
        }}
        style={{
          flex: 1,
          height: "100%",
          background: "transparent",
          border: "none",
          color: "var(--tx)",
          fontSize: 13.5,
          padding: "0 13px",
          fontFamily: "var(--font-mono)",
        }}
      />
      {suffix ? <span style={{ fontSize: 12, color: "var(--tx-3)", paddingRight: 10 }}>{suffix}</span> : null}
      <div style={{ display: "flex", flexDirection: "column", borderLeft: "1px solid var(--line)", height: "100%" }}>
        {(["▲", "▼"] as const).map((g, i) => (
          <button
            key={i}
            type="button"
            aria-label={i === 0 ? "Increment" : "Decrement"}
            onClick={() => set(i === 0 ? v + 1 : v - 1)}
            style={{
              flex: 1,
              width: 30,
              background: "transparent",
              border: "none",
              color: "var(--tx-3)",
              cursor: "pointer",
              fontSize: 7,
              lineHeight: 1,
              display: "grid",
              placeItems: "center",
              borderBottom: i === 0 ? "1px solid var(--line)" : "none",
            }}
          >
            {g}
          </button>
        ))}
      </div>
    </div>
  );
}
