// Before/after acceptance on an isolated local daemon; no real tracker or model calls.
// npm run build (web), cargo build -p rhapsodyd (root), then:
// node web/e2e/lead-attention-preview.mjs before|after
import { chromium } from "@playwright/test";
import { mkdtemp, mkdir, writeFile, readFile, rm } from "node:fs/promises";
import { spawn, execFileSync } from "node:child_process";
import { once } from "node:events";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const mode = process.argv[2];
if (!["before", "after"].includes(mode)) throw new Error("provide before or after");
const root = resolve(dirname(fileURLToPath(import.meta.url)), "../..");
const home = await mkdtemp(resolve(root, "target/lead-attention-"));
let daemon;
let browser;
try {
  await mkdir(resolve(home, ".rhapsody"));
  const db = resolve(home, ".rhapsody/rhapsody.db");
  const config = resolve(home, "preview.md");
  await writeFile(config, `---
tracker:
  kind: linear
  endpoint: http://127.0.0.1:9/graphql
  api_key: preview-not-a-credential
  project_slug: preview
repo: https://github.com/makewhatis/rhapsody.git
polling: {interval_ms: 1000}
workspace: {root: "${home}/workspaces"}
logging: {dir: "${home}/logs"}
storage: {path: "${db}"}
server: {port: 0}
claude: {command: /usr/bin/false}
otel: {enabled: false}
---
Preview only.
`);
  await writeFile(resolve(home, ".rhapsody/teams.yaml"), `enabled: true
manager:
  mode: off
  lead:
    enabled: true
    max_lead_runs_per_day: 0
    investigate: {enabled: false}
`);
  daemon = spawn(resolve(root, "target/debug/rhapsodyd"), [config, "--port", "0"], {
    cwd: root, env: { PATH: process.env.PATH, HOME: home, TMPDIR: home }, stdio: ["ignore", "ignore", "pipe"],
  });
  let diagnostics = "";
  daemon.stderr.on("data", (data) => { diagnostics = (diagnostics + data).slice(-8000); });
  let api;
  for (let attempt = 0; attempt < 100; attempt++) {
    try {
      const runtime = JSON.parse(await readFile(resolve(home, ".rhapsody/runtime.json"), "utf8"));
      const candidate = `http://127.0.0.1:${runtime.port}`;
      if ((await fetch(`${candidate}/healthz`)).ok) { api = candidate; break; }
    } catch { /* wait for the private listener */ }
    if (daemon.exitCode != null) throw new Error(`preview daemon exited: ${diagnostics}`);
    await new Promise((done) => setTimeout(done, 100));
  }
  if (!api) throw new Error(`preview listener unavailable: ${diagnostics}`);
  execFileSync("sqlite3", [db, `
INSERT INTO rhapsody_lead_items (trigger, subject, question, detail, created_at, state)
VALUES ('blocked_handoff', 'STUDIO-598', 'design access', 'needs Linear access', '2026-10-08T07:00:00Z', 'done'),
('blocked_handoff', 'STUDIO-1142', 'diagnosis', 'diagnose the missing design record', '2026-10-08T07:15:00Z', 'done');
INSERT INTO rhapsody_lead_decisions (item, at, decision, reasoning, evidence, actions, harness, model)
VALUES (1, '2026-10-08T07:00:00Z', 'escalate: needs Linear access to confirm the design doc', 'The design document cannot be confirmed from the local record. Linear access is required to verify which contract is authoritative before implementing the plugin host. The operator must confirm the design document and make it available to the next run.', 'Isolated preview: missing local design record; no credentials.', '[]', 'opencode', 'openai/gpt-test'),
(2, '2026-10-08T07:15:00Z', 'proposed: commission diagnosis', 'Commission a diagnosis-only ticket to locate the design document and reconcile the trait surface. This is a routine proposal for the lead to consider, rather than a hold requiring the operator to unblock a pull request.', 'Isolated preview: diagnosis proposal.', '[]', 'opencode', 'openai/gpt-test');
INSERT INTO runs (issue_id, issue_identifier, title, started_at, ended_at, outcome, project_slug, repo)
VALUES ('lead:makewhatis/rhapsody#0:1@manager', 'lead:makewhatis/rhapsody#0:1@manager', 'Manager run for makewhatis/rhapsody#0', '2026-10-08T07:00:00Z', '2026-10-08T07:10:00Z', 'completed', 'preview', 'https://github.com/makewhatis/rhapsody.git'),
('lead:makewhatis/rhapsody#0:2@manager', 'lead:makewhatis/rhapsody#0:2@manager', 'Manager run for makewhatis/rhapsody#0', '2026-10-08T07:15:00Z', '2026-10-08T07:20:00Z', 'completed', 'preview', 'https://github.com/makewhatis/rhapsody.git');
`]);
  browser = await chromium.launch({ headless: true });
  const page = await browser.newPage({ viewport: { width: 1440, height: 1000 } });
  await page.addInitScript(() => localStorage.setItem("rhapsody.console.jobsView", "board"));
  await page.goto(`${api}/#jobs`);
  await page.getByRole("heading", { name: "Jobs", exact: true }).waitFor();
  if (mode === "before") await page.getByText(/2 pull requests need attention/).waitFor();
  else {
    await page.getByRole("button", { name: /Lead notifications/ }).click();
    await page.getByText(/Lead escalated/).waitFor();
    if (await page.getByText(/pull requests need attention/).count()) throw new Error("lead still in PR banner");
    if ((await page.locator("body").innerText()).includes("#0")) throw new Error("placeholder subject leaked");
    const counts = await (await fetch(`${api}/api/v1/history/issues/counts`)).json();
    if (counts.issues !== 0) throw new Error("lead runs still counted as tickets");
  }
  await page.screenshot({ path: resolve(root, `web/e2e/lead-attention-${mode}.png`), fullPage: true });
  console.log(`lead-attention-preview: PASS (${mode}) — isolated daemon, shared console screenshot`);
} finally {
  await browser?.close();
  if (daemon && daemon.exitCode == null) { const stopped = once(daemon, "exit"); daemon.kill("SIGTERM"); await stopped; }
  await rm(home, { recursive: true, force: true });
}
