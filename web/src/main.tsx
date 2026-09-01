import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import "./index.css";
// The console redesign's foundation (STUDIO-682). Both files are scoped under
// `.rh-console`, so loading them alongside the Podium layer changes nothing until
// a component puts that class on the tree.
import "./theme/tokens.css";
import "./theme/console.css";
import App from "./App.tsx";

const queryClient = new QueryClient({
  defaultOptions: { queries: { retry: 1 } },
});

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <QueryClientProvider client={queryClient}>
      <App />
    </QueryClientProvider>
  </StrictMode>,
);
