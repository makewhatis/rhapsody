import * as React from "react";
import { ConsoleApp } from "@/components/console/views/ConsoleApp";
import { useIsDemoRoute } from "@/components/demo/route";

// The primitive gallery is a verification-only route (#/demo), code-split so it never ships in
// the main bundle. See components/demo/PrimitiveGallery.
const PrimitiveGallery = React.lazy(() => import("@/components/demo/PrimitiveGallery"));

// The Rhapsody Console (STUDIO-681) is the dashboard. This is the single §2.2.1 flip: slices 1–5
// each landed DARK — shipping mountable, tested views while this file kept rendering the Podium
// `AppShell` — and STUDIO-687's completeness audit found every gate clean (all 44 acceptance boxes
// green, Settings at parity, the teams-off app coherent), so the root swaps here, once.
//
// The Podium components are NOT dead: the console embeds the shipped Settings tabs (General,
// Projects, Tools, Logs, Updates) and the Onboarding wizard rather than re-implementing them, so
// this swap changes the shell and the information architecture, not the editors underneath it.
// ConsoleApp owns the capability gate, the first-run wizard and the desktop tray/shutdown
// subscriptions the Podium shell used to own.
export default function App() {
  if (useIsDemoRoute()) {
    return (
      <React.Suspense
        fallback={
          <div
            style={{
              minHeight: "100vh",
              background: "var(--bg-app)",
              color: "var(--tx-3)",
              display: "grid",
              placeItems: "center",
              fontSize: 13,
            }}
          >
            Loading primitives…
          </div>
        }
      >
        <PrimitiveGallery />
      </React.Suspense>
    );
  }
  return <ConsoleApp />;
}
