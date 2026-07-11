import * as React from "react";
import { Check } from "@/components/ui/icons";

interface ToastState {
  title: string;
  desc?: string;
}

interface ToastContextValue {
  /** Show a success toast; auto-dismisses after the provider's duration. */
  toast: (title: string, desc?: string) => void;
}

const ToastContext = React.createContext<ToastContextValue | null>(null);

export function useToast(): ToastContextValue {
  const ctx = React.useContext(ToastContext);
  if (!ctx) throw new Error("useToast must be used within a ToastProvider");
  return ctx;
}

export interface ToastProviderProps {
  children: React.ReactNode;
  /** Auto-dismiss delay in ms (package default ≈ 3.4s). */
  duration?: number;
}

// ToastProvider — a minimal success-toast system, ported from `app.jsx` (the `toastIn`
// animation, an emerald check, and ~3.4s auto-dismiss). A single toast at a time replaces
// any in-flight one.
export function ToastProvider({ children, duration = 3400 }: ToastProviderProps) {
  const [state, setState] = React.useState<ToastState | null>(null);
  const toast = React.useCallback((title: string, desc?: string) => setState({ title, desc }), []);

  React.useEffect(() => {
    if (!state) return;
    const id = window.setTimeout(() => setState(null), duration);
    return () => window.clearTimeout(id);
  }, [state, duration]);

  return (
    <ToastContext.Provider value={{ toast }}>
      {children}
      {state ? <ToastView title={state.title} desc={state.desc} /> : null}
    </ToastContext.Provider>
  );
}

function ToastView({ title, desc }: ToastState) {
  return (
    // Outer node owns the centering transform; the inner card owns the toastIn animation so
    // the keyframes' transform doesn't clobber the horizontal centering.
    <div style={{ position: "absolute", bottom: 28, left: "50%", transform: "translateX(-50%)", zIndex: 300 }}>
      <div
        role="status"
        style={{
          display: "flex",
          alignItems: "center",
          gap: 11,
          padding: "13px 18px",
          borderRadius: 12,
          background: "var(--bg-raised)",
          border: "1px solid var(--line-strong)",
          boxShadow: "var(--shadow-pop)",
          animation: "toastIn .26s cubic-bezier(.2,.7,.2,1)",
        }}
      >
        <span
          style={{
            width: 24,
            height: 24,
            borderRadius: "50%",
            background: "var(--em-soft)",
            color: "var(--em-bright)",
            display: "grid",
            placeItems: "center",
          }}
        >
          <Check size={15} style={{ strokeWidth: 2.5 }} />
        </span>
        <div>
          <div style={{ fontSize: 13.5, fontWeight: 600 }}>{title}</div>
          {desc ? <div style={{ fontSize: 12, color: "var(--tx-3)", marginTop: 1 }}>{desc}</div> : null}
        </div>
      </div>
    </div>
  );
}
