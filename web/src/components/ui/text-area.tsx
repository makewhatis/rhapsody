import * as React from "react";

export interface TextAreaProps extends React.TextareaHTMLAttributes<HTMLTextAreaElement> {
  mono?: boolean;
}

// TextArea — multiline field with the emerald focus ring and an optional mono variant.
// Ported from `ui.jsx`.
export const TextArea = React.forwardRef<HTMLTextAreaElement, TextAreaProps>(
  ({ mono, style, onFocus, onBlur, ...rest }, ref) => {
    const [focus, setFocus] = React.useState(false);
    return (
      <textarea
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
          minHeight: 150,
          resize: "vertical",
          background: "var(--bg-input)",
          border: `1px solid ${focus ? "var(--focus)" : "var(--line)"}`,
          borderRadius: "var(--r-ctrl)",
          color: "var(--tx)",
          fontSize: 13,
          lineHeight: 1.6,
          padding: "12px 14px",
          fontFamily: mono ? "var(--font-mono)" : "inherit",
          boxShadow: focus ? "0 0 0 3px var(--em-soft)" : "none",
          transition: "border-color .15s, box-shadow .15s",
          ...style,
        }}
        {...rest}
      />
    );
  },
);
TextArea.displayName = "TextArea";
