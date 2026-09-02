import { StatusDot } from "@/components/ui";

// The full-window "quitting" screen, shown while the desktop host stops the daemon off the main
// thread (the `app:shutting-down` event).
//
// It lives in its own module because BOTH shells own that subscription: the Podium `AppShell` and —
// since STUDIO-687's §2.2.1 flip made it the root — the console's `ConsoleApp`. Importing it from
// `AppShell.tsx` would have pulled that module's whole tree (RunsView, TeamsPanel, the Podium
// Settings) back into the console's bundle to reach twenty lines of markup.
export function ShutdownOverlay() {
  return (
    <div
      style={{
        position: "fixed",
        inset: 0,
        zIndex: 1000,
        background: "var(--bg-app)",
        display: "flex",
        flexDirection: "column",
        alignItems: "center",
        justifyContent: "center",
        gap: 12,
      }}
    >
      <StatusDot color="var(--amber)" size={9} pulse />
      <div style={{ fontSize: 15, fontWeight: 600, color: "var(--tx)" }}>Shutting down…</div>
      <div style={{ fontSize: 12.5, color: "var(--tx-3)" }}>Stopping the daemon and finishing in-flight work.</div>
    </div>
  );
}
