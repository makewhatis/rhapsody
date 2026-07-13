export interface ToggleProps {
  checked: boolean;
  onChange: (checked: boolean) => void;
  size?: "sm" | "md";
  /** Accessible name for the switch (recommended when not wrapped in a labelled Field). */
  "aria-label"?: string;
  disabled?: boolean;
}

// Toggle — rust switch with a sliding knob; `role="switch"` + `aria-checked` make the on/off
// state assistive-tech and test friendly.
export function Toggle({ checked, onChange, size = "md", disabled, ...rest }: ToggleProps) {
  const w = size === "sm" ? 36 : 44;
  const h = size === "sm" ? 21 : 25;
  const k = h - 6;
  return (
    <button
      type="button"
      role="switch"
      aria-checked={checked}
      disabled={disabled}
      onClick={() => onChange(!checked)}
      style={{
        width: w,
        height: h,
        borderRadius: 999,
        border: "1px solid",
        borderColor: checked ? "var(--rust-hover)" : "var(--line-strong)",
        background: checked ? "var(--rust)" : "rgba(255,255,255,.06)",
        position: "relative",
        cursor: disabled ? "default" : "pointer",
        transition: "background .18s, border-color .18s",
        flexShrink: 0,
        padding: 0,
        opacity: disabled ? 0.5 : 1,
      }}
      {...rest}
    >
      <span
        style={{
          position: "absolute",
          top: 2,
          left: checked ? w - k - 3 : 2,
          width: k,
          height: k,
          borderRadius: "50%",
          background: checked ? "#fff" : "#cfd6d2",
          transition: "left .18s cubic-bezier(.2,.7,.2,1)",
          boxShadow: "0 1px 2px rgba(0,0,0,.4)",
        }}
      />
    </button>
  );
}
