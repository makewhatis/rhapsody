import * as React from "react";
import { StatusDot } from "./status-dot";
import { X } from "./icons";

export type ChipsTone = "neutral" | "emerald" | "sky" | "amber";

export interface ChipsProps {
  items: string[];
  onAdd: (value: string) => void;
  onRemove: (value: string) => void;
  tone?: ChipsTone;
  placeholder?: string;
  /** Returns true for an item that should render in the red "invalid" state. */
  invalidItem?: (item: string) => boolean;
}

const TONE_COLOR: Record<ChipsTone, string> = {
  neutral: "var(--tx-2)",
  emerald: "var(--em-bright)",
  sky: "var(--sky)",
  amber: "var(--amber)",
};

// Chips — tag input. Enter or comma commits the typed value (no duplicates); Backspace on
// an empty field removes the last chip; blur commits. `invalidItem` flags chips red.
// Ported from `ui.jsx`.
export function Chips({ items, onAdd, onRemove, tone = "neutral", placeholder = "Add…", invalidItem }: ChipsProps) {
  const [val, setVal] = React.useState("");
  const toneC = TONE_COLOR[tone];
  const add = () => {
    const v = val.trim();
    if (v && !items.includes(v)) onAdd(v);
    setVal("");
  };
  return (
    <div
      style={{
        display: "flex",
        flexWrap: "wrap",
        gap: 7,
        alignItems: "center",
        minHeight: 40,
        background: "var(--bg-input)",
        border: "1px solid var(--line)",
        borderRadius: "var(--r-ctrl)",
        padding: "6px 8px",
      }}
    >
      {items.map((it) => {
        const bad = invalidItem ? invalidItem(it) : false;
        return (
          <span
            key={it}
            style={{
              display: "inline-flex",
              alignItems: "center",
              gap: 6,
              height: 26,
              padding: "0 6px 0 10px",
              borderRadius: 7,
              fontSize: 12.5,
              fontWeight: 500,
              background: bad ? "var(--red-soft)" : "rgba(255,255,255,.05)",
              color: bad ? "var(--red)" : "var(--tx)",
              border: `1px solid ${bad ? "rgba(239,83,80,.4)" : "var(--line)"}`,
            }}
          >
            <StatusDot color={bad ? "var(--red)" : toneC} size={6} />
            {it}
            <button
              type="button"
              aria-label={`Remove ${it}`}
              // Prevent the mousedown from blurring the input first: otherwise onBlur would
              // commit a half-typed draft as a stray chip before this remove click runs.
              onMouseDown={(e) => e.preventDefault()}
              onClick={() => onRemove(it)}
              style={{
                background: "transparent",
                border: "none",
                color: "var(--tx-3)",
                cursor: "pointer",
                display: "flex",
                padding: 2,
                borderRadius: 4,
              }}
            >
              <X size={12} />
            </button>
          </span>
        );
      })}
      <input
        value={val}
        placeholder={placeholder}
        onChange={(e) => setVal(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter" || e.key === ",") {
            e.preventDefault();
            add();
          } else if (e.key === "Backspace" && !val && items.length) {
            onRemove(items[items.length - 1]);
          }
        }}
        onBlur={add}
        style={{
          flex: 1,
          minWidth: 90,
          height: 26,
          background: "transparent",
          border: "none",
          color: "var(--tx)",
          fontSize: 13,
          padding: "0 4px",
        }}
      />
    </div>
  );
}
