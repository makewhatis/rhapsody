import { defineConfig } from "vitest/config";

// The shell's logic tests are pure (no DOM, no React, no Tauri bridge), so a plugin-free node
// environment suffices — and keeping plugins out avoids the vite/vitest bundled-vite type clash.
// Mirrors $REF/desktop/frontend/vitest.config.ts.
export default defineConfig({
  test: {
    environment: "node",
    include: ["src/**/*.test.ts"],
  },
});
