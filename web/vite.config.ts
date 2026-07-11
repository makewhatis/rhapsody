import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
import path from "node:path";
import fs from "node:fs";

const embedDir = path.resolve(__dirname, "../crates/httpapi/web-dist");

// Re-create the .gitkeep anchor that `emptyOutDir` wipes at the start of each build, so the
// committed placeholder — which lets rust-embed's `#[folder = "web-dist/"]` compile on a clean /
// Node-less checkout — survives a local `npm run build` (mirrors the Go Makefile's build-web anchor
// re-touch). Without this, a developer who builds then `git add -A` would stage the anchor's
// deletion and break the clean-checkout compile.
function keepEmbedAnchor() {
  return {
    name: "keep-embed-anchor",
    closeBundle() {
      fs.mkdirSync(embedDir, { recursive: true });
      fs.writeFileSync(path.join(embedDir, ".gitkeep"), "");
    },
  };
}

// https://vite.dev/config/
export default defineConfig({
  plugins: [react(), tailwindcss(), keepEmbedAnchor()],
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
    outDir: embedDir,
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
