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
  // Embedded under the Go binary at "/", so assets must be root-relative.
  base: "/",
  build: {
    // Build into the Go package dir so `//go:embed all:web/dist` in
    // internal/httpapi/web.go (embed paths are relative to the .go file) finds it.
    outDir: path.resolve(__dirname, "../internal/httpapi/web/dist"),
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
