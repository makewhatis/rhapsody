import { defineConfig } from "@playwright/test";

// The browser layout acceptance for the run-detail header (STUDIO-1023). jsdom does no layout, so
// this drives the real `TraceHeader` mounted by `layout.html` through a headless Chromium at the
// four widths the ticket names. `npm run test:layout`; browsers via `npx playwright install
// chromium`.
export default defineConfig({
  testDir: "./e2e",
  timeout: 30_000,
  fullyParallel: true,
  reporter: "list",
  use: {
    baseURL: "http://localhost:5173",
  },
  webServer: {
    command: "npm run dev -- --port 5173 --strictPort",
    url: "http://localhost:5173/layout.html",
    reuseExistingServer: !process.env.CI,
    timeout: 60_000,
  },
});
