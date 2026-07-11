import * as React from "react";
import { ChevronDown, Check } from "./icons";

export interface SelectOption {
  value: string;
  label: string;
  note?: string;
  /** Force-disable the mono font for this option's value (default: auto-detect). */
  mono?: boolean;
}

export interface SelectProps {
  value: string;
  options: SelectOption[];
  onChange: (value: string) => void;
  width?: number | string;
  placeholder?: string;
  invalid?: boolean;
}

// Values that look like identifiers/paths/slugs render in the mono font.
const looksMono = (v: string) => /[-_.]/.test(v);

// Select — custom popover select (not the native control). Ported from `ui.jsx`: emerald
// focus ring on open, rotating chevron, selected + hover option states, optional per-option
// note, mono detection, and outside-click / mousedown / focusout / Escape-to-close.
export function Select({ value, options, onChange, width = 320, placeholder = "Select…", invalid }: SelectProps) {
  const [open, setOpen] = React.useState(false);
  const ref = React.useRef<HTMLDivElement>(null);
  const triggerRef = React.useRef<HTMLButtonElement>(null);

  React.useEffect(() => {
    if (!open) return;
    const root = ref.current;
    const handler = (e: MouseEvent) => {
      if (root && !root.contains(e.target as Node)) setOpen(false);
    };
    // Tab/keyboard exit: the options are real buttons, so focus can walk out of the
    // control while it stays open. Close when focus lands outside the root. (relatedTarget
    // is null on some blur cases — e.g. focus leaving the window — so only close when we
    // have a target that is demonstrably outside; otherwise leave it open conservatively.)
    const onFocusOut = (e: FocusEvent) => {
      const next = e.relatedTarget as Node | null;
      if (next && root && !root.contains(next)) setOpen(false);
    };
    // Escape closes and returns focus to the trigger, matching native select behavior.
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        setOpen(false);
        triggerRef.current?.focus();
      }
    };
    document.addEventListener("mousedown", handler);
    root?.addEventListener("focusout", onFocusOut);
    document.addEventListener("keydown", onKeyDown);
    return () => {
      document.removeEventListener("mousedown", handler);
      root?.removeEventListener("focusout", onFocusOut);
      document.removeEventListener("keydown", onKeyDown);
    };
  }, [open]);

  const cur = options.find((o) => o.value === value);
  return (
    <div ref={ref} style={{ position: "relative", width }}>
      <button
        ref={triggerRef}
        type="button"
        aria-haspopup="listbox"
        aria-expanded={open}
        onClick={() => setOpen(!open)}
        style={{
          width: "100%",
          height: 40,
          display: "flex",
          alignItems: "center",
          justifyContent: "space-between",
          gap: 10,
          background: "var(--bg-input)",
          border: `1px solid ${invalid ? "rgba(239,83,80,.55)" : open ? "var(--focus)" : "var(--line)"}`,
          borderRadius: "var(--r-ctrl)",
          color: cur ? "var(--tx)" : "var(--tx-3)",
          fontSize: 13.5,
          padding: "0 12px 0 13px",
          cursor: "pointer",
          boxShadow: open ? "0 0 0 3px var(--em-soft)" : "none",
          transition: "border-color .15s, box-shadow .15s",
          fontFamily: cur && cur.mono !== false && looksMono(String(cur.value)) ? "var(--font-mono)" : "inherit",
          textAlign: "left",
        }}
      >
        <span style={{ overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}>
          {cur ? cur.label : placeholder}
        </span>
        <ChevronDown
          size={15}
          style={{ color: "var(--tx-3)", transition: "transform .15s", transform: open ? "rotate(180deg)" : "none" }}
        />
      </button>
      {open ? (
        <div
          role="listbox"
          style={{
            position: "absolute",
            top: "calc(100% + 6px)",
            left: 0,
            right: 0,
            zIndex: 50,
            background: "var(--bg-raised)",
            border: "1px solid var(--line-strong)",
            borderRadius: "var(--r-ctrl)",
            boxShadow: "var(--shadow-pop)",
            padding: 5,
            animation: "fadeUp .14s ease-out",
            maxHeight: 280,
            overflowY: "auto",
          }}
        >
          {options.map((o) => {
            const sel = o.value === value;
            return (
              <button
                key={o.value}
                type="button"
                role="option"
                aria-selected={sel}
                onClick={() => {
                  onChange(o.value);
                  setOpen(false);
                }}
                onMouseEnter={(e) => {
                  if (!sel) e.currentTarget.style.background = "var(--bg-hover)";
                }}
                onMouseLeave={(e) => {
                  if (!sel) e.currentTarget.style.background = "transparent";
                }}
                style={{
                  width: "100%",
                  display: "flex",
                  alignItems: "center",
                  justifyContent: "space-between",
                  gap: 10,
                  padding: "9px 10px",
                  background: sel ? "var(--em-soft)" : "transparent",
                  border: "none",
                  borderRadius: 7,
                  cursor: "pointer",
                  textAlign: "left",
                  color: "var(--tx)",
                  transition: "background .1s",
                }}
              >
                <span style={{ display: "flex", flexDirection: "column", gap: 2, minWidth: 0 }}>
                  <span
                    style={{
                      fontSize: 13,
                      fontWeight: sel ? 600 : 500,
                      color: sel ? "var(--em-bright)" : "var(--tx)",
                      // honor the per-option mono opt-out, same as the trigger above
                      fontFamily: o.mono !== false && looksMono(String(o.value)) ? "var(--font-mono)" : "inherit",
                      overflow: "hidden",
                      textOverflow: "ellipsis",
                      whiteSpace: "nowrap",
                    }}
                  >
                    {o.label}
                  </span>
                  {o.note ? <span style={{ fontSize: 11.5, color: "var(--tx-3)" }}>{o.note}</span> : null}
                </span>
                {sel ? <Check size={15} style={{ color: "var(--em-bright)", flexShrink: 0 }} /> : null}
              </button>
            );
          })}
        </div>
      ) : null}
    </div>
  );
}
