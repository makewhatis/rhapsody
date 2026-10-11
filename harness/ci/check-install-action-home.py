#!/usr/bin/env python3
"""Every `taiki-e/install-action` step must run under a job-private HOME (STUDIO-1174).

The action downloads, checksums and extracts in a FIXED, user-global scratch dir — its
`main.sh` sets `install_action_dir="${HOME}/.install-action"`, `tmp_dir=.../tmp`, does
`curl -o tmp`, `sha256sum -c ... *tmp`, then `rm -rf "${tmp_dir}"`, with no locking
anywhere. Every self-hosted Mac runner is a separate runner instance on ONE host, all
running as the same user, so two jobs that use the action at the same instant (on a push
to main, `test` and `desktop`) share that one directory: one job's download or its
trailing `rm -rf` clobbers the other's in-flight `tmp`, and the checksum verify dies
seconds in (`sha256sum: *tmp: No such file or directory`). Setting a job-private `HOME`
on the step (referencing `${{ runner.temp }}`) moves the scratch dir under the job's own
temp — that is the fix this check pins.

Prose has no compiler, so, exactly like `check-plugin.sh` and `check-tmp-leaks.sh`, this
rides the existing `lint` gate. A guard nobody has watched fire is not a guard, so
`check-install-action-home_test.py` reintroduces the defect (drops the `env.HOME`, or
points it at a shared path) and asserts this scan reds on it.

Usage: check-install-action-home.py [--workflow PATH]
Exits 0 when every action step is job-private; 1 when one is not; 2 on a setup error.
"""
import argparse
import re
import sys
from pathlib import Path

DEFAULT_WORKFLOW = Path(__file__).resolve().parents[2] / ".github" / "workflows" / "ci.yml"
ACTION = "taiki-e/install-action"
# A job-private HOME must derive from a per-job/per-runner context, never a literal path
# or the runner's shared `$HOME`. `runner.temp` is the job's private temp dir and
# `runner.workspace` that runner's checkout root; both are unique to the job/runner.
JOB_PRIVATE = ("runner.temp", "runner.workspace")
RUNNER_TEMP = "${{ runner.temp }}"

STEP_RE = re.compile(r"^(\s*)- ")
USES_RE = re.compile(r"^\s*- uses:\s*(\S+)")
ENV_RE = re.compile(r"^(\s*)env:\s*$")
HOME_RE = re.compile(r"^(\s+)HOME:\s*(.+?)\s*$")
JOB_RE = re.compile(r"^  ([A-Za-z0-9_-]+):\s*$")


def step_blocks(lines):
    """Yield (start, end) for each step, grouping nested list items into their parent.

    A step is a `- ` list item; its block runs to the next `- ` at the same or a
    shallower indent, so a nested list inside a step stays part of that step.
    """
    starts = [(i, len(m.group(1))) for i, line in enumerate(lines) if (m := STEP_RE.match(line))]
    for idx, (start, indent) in enumerate(starts):
        end = len(lines)
        for later, later_indent in starts[idx + 1:]:
            if later_indent <= indent:
                end = later
                break
        yield start, end


def job_of(lines, upto):
    """The job key (an indent-2 mapping key after `jobs:`) most recently seen before `upto`."""
    jobs_at = next((i for i, line in enumerate(lines) if line.rstrip() == "jobs:"), None)
    if jobs_at is None:
        return None
    name = None
    for line in lines[jobs_at + 1:upto]:
        match = JOB_RE.match(line)
        if match:
            name = match.group(1)
    return name


def find_home(block):
    """The step's `env.HOME` value, or None when the step sets no such key.

    Only a `HOME:` nested under an `env:` mapping counts — a `HOME:` under `with:` or a
    sibling key is not the environment the action runs under.
    """
    env_indent = None
    for line in block:
        if not line.strip():
            continue
        indent = len(line) - len(line.lstrip())
        if env_indent is not None and indent <= env_indent:
            env_indent = None
        match = ENV_RE.match(line)
        if match:
            env_indent = indent
            continue
        if env_indent is not None:
            match = HOME_RE.match(line)
            if match:
                return match.group(2)
    return None


def check(path):
    """Return (action_step_count, failure_messages) for the workflow at `path`."""
    lines = path.read_text().splitlines()
    seen = 0
    failures = []
    for start, end in step_blocks(lines):
        uses = USES_RE.match(lines[start])
        if not uses or not uses.group(1).startswith(ACTION):
            continue
        seen += 1
        job = job_of(lines, start)
        where = f"job '{job}'" if job else f"line {start + 1}"
        home = find_home(lines[start:end])
        if home is None:
            failures.append(
                f"{where}: `uses: {uses.group(1)}` must set a job-private `env.HOME` "
                f"(referencing {RUNNER_TEMP}) so the action's fixed "
                f"${{HOME}}/.install-action/tmp scratch is per-job; it sets none"
            )
        elif not any(key in home for key in JOB_PRIVATE):
            failures.append(
                f"{where}: `uses: {uses.group(1)}` sets env.HOME={home!r}, which is not "
                f"job-private; it must reference {RUNNER_TEMP}"
            )
    return seen, failures


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--workflow", type=Path, default=DEFAULT_WORKFLOW)
    args = parser.parse_args()
    if not args.workflow.is_file():
        print(f"::error::install-action HOME check: no workflow at {args.workflow}")
        return 2
    seen, failures = check(args.workflow)
    if seen == 0:
        print(f"::error::install-action HOME check: no `{ACTION}` step found in "
              f"{args.workflow} — the check would pass vacuously")
        return 2
    for failure in failures:
        print(f"::error::{failure} (STUDIO-1174)")
    if failures:
        return 1
    print(f"install-action HOME check: PASS ({seen} `{ACTION}` step(s) job-private)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
