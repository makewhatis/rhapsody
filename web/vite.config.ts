import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
import path from "node:path";

// https://vite.dev/config/
export default defineConfig({
  plugins: [react(), tailwindcss()],
  resolve: {
    alias: {
      "@": path.resolve(__dirname, "./src"),
    },
  },
  // Embedded under the symphonyd binary at "/", so assets must be root-relative.
  base: "/",
  build: {
    // Build into the httpapi crate's embed dir so rust-embed's `#[folder = "web-dist/"]`
    // (crates/httpapi/src/web.rs) finds it. The bundle is NOT committed — only web-dist/.gitkeep
    // is (see the repo .gitignore); CI's `web` job runs `npm run build` to populate it.
    outDir: path.resolve(__dirname, "../crates/httpapi/web-dist"),
    emptyOutDir: true,
  },
  server: {
    port: 5173,
    proxy: {
      "/api": {
        // Defaults to the daemon's local port (Makefile `run` => PORT ?= 8799);
        // override with SYMPHONY_API_URL. Dev-server only — not in the built bundle.
        target: process.env.SYMPHONY_API_URL ?? "http://localhost:8799",
        changeOrigin: true,
      },
    },
  },
});
