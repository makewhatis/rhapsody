import { defineConfig } from "vitest/config";
import path from "node:path";

export default defineConfig({
  resolve: {
    alias: {
      "@": path.resolve(__dirname, "./src"),
    },
  },
  test: {
    // Pure-logic tests default to node; component tests opt into jsdom per-file via a
    // `// @vitest-environment jsdom` pragma at the top of the .test.tsx file.
    environment: "node",
    include: ["src/**/*.test.{ts,tsx}"],
  },
});
