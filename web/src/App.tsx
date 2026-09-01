import * as React from "react";
import { ConsoleApp } from "@/components/console/views/ConsoleApp";
import { useIsDemoRoute } from "@/components/demo/route";

// The primitive gallery is a verification-only route (#/demo), code-split so it never ships in
// the main bundle. See components/demo/PrimitiveGallery.
const PrimitiveGallery = React.lazy(() => import("@/components/demo/PrimitiveGallery"));

// The Rhapsody Console (STUDIO-681) is the app. STUDIO-682 built its design system and
// STUDIO-683 wires the shell in: the capability-gated rail, Jobs (home), Job detail and
// Settings. Teams, Memory and Manage-team are sub-tickets 3-5 and render a named placeholder
// until then.
//
// The previous Podium shell (`components/shell/AppShell`) is NOT deleted: its Settings depth —
// the WORKFLOW.md editor, projects, tools, logs and the updater — has no console equivalent
// yet, and those components are what sub-tickets 3-5 and the §10 box 6.2 audit build against.
// See this PR's body: whether to hold the root swap until the epic completes is the reviewer's
// call, and it is one line here.
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
