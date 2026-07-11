import * as React from "react";
import { AlertTriangle } from "./icons";

export interface FieldErrorProps {
  children?: React.ReactNode;
}

// FieldError — inline validation message with a warning glyph, ported from `ui.jsx`.
export function FieldError({ children }: FieldErrorProps) {
  return (
    <div
      style={{
        display: "flex",
        alignItems: "center",
        gap: 6,
        marginTop: 7,
        fontSize: 12,
        color: "var(--red)",
        fontWeight: 500,
      }}
    >
      <AlertTriangle size={13} />
      {children}
    </div>
  );
}

export interface FieldProps {
  label: React.ReactNode;
  hint?: React.ReactNode;
  error?: React.ReactNode;
  optional?: boolean;
  htmlFor?: string;
  children?: React.ReactNode;
  /** Two-column label-left / control-right layout (matches the Settings rows). */
  inline?: boolean;
  /** Optional control rendered at the right of the label row. */
  action?: React.ReactNode;
}

// Field — labelled form-row wrapper supporting an optional badge, hint, header action,
// inline (two-column) or stacked layout, and validation. Ported from `ui.jsx`.
export function Field({ label, hint, error, optional, htmlFor, children, inline, action }: FieldProps) {
  return (
    <div
      style={
        inline
          ? { display: "grid", gridTemplateColumns: "minmax(0,1fr) 320px", gap: 24, alignItems: "center" }
          : { display: "flex", flexDirection: "column", gap: 7 }
      }
    >
      <div>
        <div style={{ display: "flex", alignItems: "center", justifyContent: "space-between", gap: 8 }}>
          <label
            htmlFor={htmlFor}
            style={{
              fontSize: 13,
              fontWeight: 500,
              color: "var(--tx)",
              letterSpacing: "-0.005em",
              display: "inline-flex",
              alignItems: "center",
              gap: 7,
            }}
          >
            {label}
            {optional ? (
              <span style={{ fontSize: 11, color: "var(--tx-faint)", fontWeight: 400 }}>optional</span>
            ) : null}
          </label>
          {action}
        </div>
        {hint ? (
          <div style={{ fontSize: 12, color: "var(--tx-3)", marginTop: 3, lineHeight: 1.45 }}>{hint}</div>
        ) : null}
        {inline && error ? <FieldError>{error}</FieldError> : null}
      </div>
      <div>
        {children}
        {!inline && error ? <FieldError>{error}</FieldError> : null}
      </div>
    </div>
  );
}
