import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Tauri embeds the built bundle (src-tauri/tauri.conf.json `frontendDist` → ../frontend/dist) and
// serves it at "/", so asset URLs must be relative. Test config lives in vitest.config.ts (kept
// separate to avoid the vite/vitest bundled-vite type clash). Mirrors $REF/desktop/frontend/vite.config.ts
// (Wails), adapted for Tauri: a fixed dev-server port the `tauri dev` command attaches to.
export default defineConfig({
  plugins: [react()],
  base: "./",
  // `cargo tauri dev` (P7-D5) launches this dev server and expects a stable port.
  server: { port: 1420, strictPort: true },
  build: { outDir: "dist", emptyOutDir: true },
});
