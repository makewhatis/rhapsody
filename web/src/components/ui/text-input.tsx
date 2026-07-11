import * as React from "react";
import type { IconComponent } from "./icons";

export interface TextInputProps extends React.InputHTMLAttributes<HTMLInputElement> {
  mono?: boolean;
  invalid?: boolean;
  prefixIcon?: IconComponent;
  suffix?: React.ReactNode;
}

// TextInput — text field with the emerald focus ring, invalid state, optional prefix icon,
// suffix adornment, and mono variant. Ported from `ui.jsx`.
export const TextInput = React.forwardRef<HTMLInputElement, TextInputProps>(
  ({ mono, invalid, prefixIcon: PrefixIcon, suffix, style, onFocus, onBlur, ...rest }, ref) => {
    const [focus, setFocus] = React.useState(false);
    return (
      <div style={{ position: "relative", display: "flex", alignItems: "center" }}>
        {PrefixIcon ? (
          <span style={{ position: "absolute", left: 12, color: "var(--tx-3)", display: "flex" }}>
            <PrefixIcon size={15} />
          </span>
        ) : null}
        <input
          ref={ref}
          onFocus={(e) => {
            setFocus(true);
            onFocus?.(e);
          }}
          onBlur={(e) => {
            setFocus(false);
            onBlur?.(e);
          }}
          style={{
            width: "100%",
            height: 40,
            background: "var(--bg-input)",
            border: `1px solid ${invalid ? "rgba(239,83,80,.55)" : focus ? "var(--focus)" : "var(--line)"}`,
            borderRadius: "var(--r-ctrl)",
            color: "var(--tx)",
            fontSize: 13.5,
            padding: PrefixIcon ? "0 12px 0 34px" : "0 13px",
            paddingRight: suffix ? 70 : PrefixIcon ? 12 : 13,
            fontFamily: mono ? "var(--font-mono)" : "inherit",
            transition: "border-color .15s, box-shadow .15s",
            boxShadow: focus ? "0 0 0 3px var(--em-soft)" : "none",
            ...style,
          }}
          {...rest}
        />
        {suffix ? (
          <span style={{ position: "absolute", right: 12, color: "var(--tx-3)", fontSize: 12 }}>{suffix}</span>
        ) : null}
      </div>
    );
  },
);
TextInput.displayName = "TextInput";
