import { Check } from "./icons";

export interface CheckboxProps {
  checked: boolean;
  onChange: (checked: boolean) => void;
  "aria-label"?: string;
  disabled?: boolean;
}

// Checkbox — small rust check box; `role="checkbox"` + `aria-checked` expose the state.
export function Checkbox({ checked, onChange, disabled, ...rest }: CheckboxProps) {
  return (
    <button
      type="button"
      role="checkbox"
      aria-checked={checked}
      disabled={disabled}
      onClick={() => onChange(!checked)}
      style={{
        width: 19,
        height: 19,
        borderRadius: 6,
        border: `1.5px solid ${checked ? "var(--rust)" : "var(--line-strong)"}`,
        background: checked ? "var(--rust)" : "transparent",
        display: "grid",
        placeItems: "center",
        cursor: disabled ? "default" : "pointer",
        flexShrink: 0,
        transition: "all .14s",
        padding: 0,
        opacity: disabled ? 0.5 : 1,
      }}
      {...rest}
    >
      {checked ? <Check size={12} style={{ color: "var(--on-rust)", strokeWidth: 3 }} /> : null}
    </button>
  );
}
