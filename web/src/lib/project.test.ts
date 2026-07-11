import { describe, expect, it } from "vitest";
import { projectLabel, repoShortName } from "@/lib/project";
import type { RunningSession, RunSummary } from "@/lib/api";

// Sample rows mirroring the obs wire shape (the "render check" sample data).
const runningSample: RunningSession = {
  issue_id: "id-1",
  issue_identifier: "MT-1",
  title: "Add login",
  state: "In Progress",
  project: "tally-symphony-e3b6fdf879c1",
  repo: "git@github.com:makewhatis/tally.git",
  run_id: 1,
  turn_count: 3,
  last_codex_event: "turn_completed",
  started_at: "2026-05-31T10:00:00Z",
  last_event_at: "2026-05-31T10:05:00Z",
  input_tokens: 100,
  output_tokens: 200,
  total_tokens: 300,
};

const runSample: RunSummary = {
  id: 1,
  issue_id: "id-1",
  issue_identifier: "MT-1",
  title: "Add login",
  attempt: 1,
  session_uuid: "u",
  branch: "symphony/MT-1",
  project_slug: "tally-bugs-aaaa1111",
  repo: "git@github.com:makewhatis/tally.git",
  started_at: "2026-05-31T10:00:00Z",
  ended_at: "2026-05-31T10:10:00Z",
  outcome: "completed",
  turns: 4,
  input_tokens: 1,
  output_tokens: 2,
  total_tokens: 3,
  usage_estimated: false,
  error: "",
  transcript_path: "",
};

describe("projectLabel", () => {
  it("returns the project slug for a running session", () => {
    expect(projectLabel(runningSample.project)).toBe("tally-symphony-e3b6fdf879c1");
  });
  it("returns the project_slug for a run summary", () => {
    expect(projectLabel(runSample.project_slug)).toBe("tally-bugs-aaaa1111");
  });
  it("renders a dash for an empty slug (single-project mode)", () => {
    expect(projectLabel("")).toBe("—");
    expect(projectLabel(undefined)).toBe("—");
  });
});

describe("repoShortName", () => {
  it("extracts owner/name from an ssh remote", () => {
    expect(repoShortName("git@github.com:makewhatis/tally.git")).toBe("makewhatis/tally");
  });
  it("extracts owner/name from an https remote", () => {
    expect(repoShortName("https://github.com/makewhatis/ynab-ai-workflow.git")).toBe(
      "makewhatis/ynab-ai-workflow",
    );
  });
  it("returns a dash for an empty repo", () => {
    expect(repoShortName("")).toBe("—");
    expect(repoShortName(undefined)).toBe("—");
  });
  it("falls back to the raw string when it does not look like a git URL", () => {
    expect(repoShortName("weird-value")).toBe("weird-value");
  });
});
