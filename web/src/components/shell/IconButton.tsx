import * as React from "react";
import { type IconComponent } from "@/components/ui/icons";

export interface IconButtonProps {
  icon: IconComponent;
  /** Accessible name AND tooltip text (e.g. "Start", "Restart", "Settings"). */
  label: string;
  onClick?: () => void;
  /** Dimmed + non-interactive. */
  disabled?: boolean;
  /** Hover turns the icon red (used for Stop). */
  danger?: boolean;
  /** Persistent highlight (used for the Settings gear while Settings is open). */
  active?: boolean;
}

// IconButton — a compact 28px square titlebar control with a custom hover/focus tooltip. Replaces
// the text WinButtons in the consolidated top bar. The button carries an aria-label so it keeps an
// accessible name (and remains queryable by name in tests); the tooltip bubble is decorative
// (aria-hidden). A native `title` is intentionally omitted to avoid a second, lagging OS tooltip.
export function IconButton({ icon: Icon, label, onClick, disabled, danger, active }: IconButtonProps) {
  const [hover, setHover] = React.useState(false);
  const [focus, setFocus] = React.useState(false);
  const show = (hover || focus) && !disabled;
  const lit = (hover && !disabled) || active;

  return (
    <span style={{ position: "relative", display: "inline-flex" }}>
      <button
        type="button"
        aria-label={label}
        aria-pressed={active}
        disabled={disabled}
        onClick={disabled ? undefined : onClick}
        onMouseEnter={() => setHover(true)}
        onMouseLeave={() => setHover(false)}
        onFocus={() => setFocus(true)}
        onBlur={() => setFocus(false)}
        style={{
          display: "inline-flex",
          alignItems: "center",
          justifyContent: "center",
          height: 28,
          width: 28,
          borderRadius: 7,
          cursor: disabled ? "default" : "pointer",
          background: lit ? "var(--bg-hover)" : "transparent",
          color: disabled
            ? "var(--tx-faint)"
            : danger && hover
              ? "var(--red)"
              : active
                ? "var(--tx)"
                : "var(--tx-2)",
          border: "1px solid",
          borderColor: lit ? "var(--line-strong)" : "transparent",
          transition: "all .12s",
        }}
      >
        <Icon size={16} />
      </button>
      {show ? (
        <span
          role="tooltip"
          aria-hidden
          style={{
            position: "absolute",
            top: "100%",
            left: "50%",
            transform: "translateX(-50%)",
            marginTop: 7,
            padding: "3px 8px",
            borderRadius: 6,
            background: "var(--bg-elev, var(--bg-card))",
            color: "var(--tx)",
            border: "1px solid var(--line-strong)",
            boxShadow: "0 4px 14px rgba(0,0,0,0.35)",
            fontSize: 11.5,
            fontWeight: 500,
            lineHeight: 1.2,
            whiteSpace: "nowrap",
            pointerEvents: "none",
            zIndex: 200,
          }}
        >
          {label}
        </span>
      ) : null}
    </span>
  );
}
