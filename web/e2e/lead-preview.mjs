// Screenshot acceptance against an isolated, locally built rhapsodyd. No real tracker/model I/O.
// Run after `npm run build` in web and `cargo build -p rhapsodyd` at the root.
import { chromium } from "@playwright/test";
import { mkdtemp, mkdir, writeFile, rm } from "node:fs/promises";
import { spawn, execFileSync } from "node:child_process";
import { once } from "node:events";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "../..");
const home = await mkdtemp(resolve(root, "target/lead-preview-"));
let daemon;
let browser;
try {
  await mkdir(resolve(home, ".rhapsody"));
  const config = resolve(home, "preview.md");
  const db = resolve(home, ".rhapsody/rhapsody.db");
  await writeFile(config, `---
tracker:
  kind: linear
  endpoint: http://127.0.0.1:9/graphql
  api_key: preview-not-a-credential
  project_slug: preview
repo: https://github.com/makewhatis/rhapsody.git
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
      const { readFile } = await import("node:fs/promises");
      const runtime = JSON.parse(await readFile(resolve(home, ".rhapsody/runtime.json"), "utf8"));
      api = `http://127.0.0.1:${runtime.port}`;
      if ((await fetch(`${api}/healthz`)).ok) break;
    } catch { /* wait for the private listener */ }
    if (daemon.exitCode != null) throw new Error(`preview daemon exited: ${diagnostics}`);
    await new Promise((done) => setTimeout(done, 100));
  }
  if (!api) throw new Error(`preview listener unavailable: ${diagnostics}`);
  execFileSync("sqlite3", [db, `
INSERT INTO rhapsody_lead_items (trigger, subject, question, detail, created_at, state)
VALUES ('impossible_state', 'PREVIEW-290', 'zero_verdict_escalation', 'zero_verdict_escalation', '2026-10-07T18:00:00Z', 'done'),
('blocked_handoff', 'PREVIEW-87', 'console key', 'needs a B2 console key', '2026-10-08T07:15:00Z', 'done');
INSERT INTO rhapsody_lead_decisions (item, at, decision, reasoning, evidence, actions, harness, model)
VALUES (1, '2026-10-07T18:00:00Z', 'done: requeue', 'No reviewer completed a verdict. Requeue the infrastructure failure instead of adjudicating unread code.', 'Preview replay #290: three 401s, zero verdicts. Run ledger and current ticket state checked. No memories used.', '[{"action":"requeue","ticket":"PREVIEW-290"}]', 'opencode', 'openai/gpt-test'),
(2, '2026-10-08T07:15:00Z', 'escalate: needs a B2 console key: scope X, file Y', 'Key minting needs the operator console. The lead cannot do it within its standing rules.', 'Preview replay flux #87. Ticket specifies console access; no credential is in the evidence. No memories used.', '[{"action":"escalate","need":"needs a B2 console key: scope X, file Y"}]', 'opencode', 'openai/gpt-test');
`]);
  browser = await chromium.launch({ headless: true });
  const page = await browser.newPage({ viewport: { width: 1440, height: 1100 } });
  await page.goto(`${api}/#lead`);
  await page.getByText("PREVIEW-290", { exact: true }).waitFor();
  await page.getByText("PREVIEW-87", { exact: true }).waitFor();
  const buttons = page.getByRole("button", { name: "Overrule", exact: true });
  if (await buttons.count() !== 2) throw new Error("both decisions must offer Overrule");
  await page.screenshot({ path: resolve(root, "web/e2e/lead-page.png"), fullPage: true });
  await buttons.last().click();
  await page.getByRole("textbox", { name: "Operator preference" }).fill("Prefer a diagnosis before another retry.");
  await page.getByRole("button", { name: "Submit overrule" }).click();
  await page.getByText("Overruled", { exact: true }).waitFor();
  const response = await (await fetch(`${api}/api/v1/lead/decisions`)).json();
  if (!response.decisions.some((d) => d.overrule_note === "Prefer a diagnosis before another retry.")) throw new Error("overrule was not durable");
  console.log("lead-preview: PASS — real daemon, decision/evidence display, screenshot, durable Overrule");
} finally {
  await browser?.close();
  if (daemon && daemon.exitCode == null) { const stopped = once(daemon, "exit"); daemon.kill("SIGTERM"); await stopped; }
  await rm(home, { recursive: true, force: true });
}
