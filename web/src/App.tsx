import * as React from "react";
import { AppShell } from "@/components/shell/AppShell";
import { useIsDemoRoute } from "@/components/demo/route";

// The primitive gallery is a verification-only route (#/demo), code-split so it never ships in
// the main bundle. See components/demo/PrimitiveGallery.
const PrimitiveGallery = React.lazy(() => import("@/components/demo/PrimitiveGallery"));

// The Symphony app shell (INF-225) replaces the legacy Live/History/Settings dashboard. The
// re-skinned Runs view (INF-227) and Settings tab bodies (INF-226) mount into the shell's
// placeholder routes in follow-on tickets; the legacy dashboard components remain in the repo
// for the Runs re-skin to build on. AppShell owns the ToastProvider internally.
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
  return <AppShell />;
}
