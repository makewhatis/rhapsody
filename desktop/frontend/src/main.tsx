import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import App from "./App";

// Mounts the shell. Ported from $REF/desktop/frontend/src/main.tsx.
const container = document.getElementById("root");
if (!container) {
  throw new Error("root element not found");
}
createRoot(container).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
