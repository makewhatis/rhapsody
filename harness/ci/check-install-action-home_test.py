#!/usr/bin/env python3
"""Pin check-install-action-home.py by MAKING IT FIRE (STUDIO-1174).

A check nobody has watched fire is not a guard: it converts "nobody checked" into "we
checked and it is clean". This mutation test runs the real check against throwaway copies
of `.github/workflows/ci.yml` with the job-private `HOME` reintroduced as the bug — the
exact mutation the fix is meant to survive (drop the `env.HOME`, or point it at a shared
path) — and asserts the check reds on it and names the step. The green cases matter just
as much: a step that is NOT `taiki-e/install-action` must stay ignored, or the guard
would be weakened by the first person an over-broad match inconvenienced.

No dependencies beyond python3. Run from anywhere:
`python3 harness/ci/check-install-action-home_test.py`.
"""
import subprocess
import sys
import tempfile
from pathlib import Path

CHECK = Path(__file__).with_name("check-install-action-home.py").resolve()
REAL_WORKFLOW = CHECK.parents[2] / ".github" / "workflows" / "ci.yml"

# The exact block the fix adds to each `taiki-e/install-action` step in ci.yml.
ENV_BLOCK = "        env:\n          HOME: ${{ runner.temp }}/install-action-home\n"
SHARED_HOME = "          HOME: /Users/Shared/ci/install-action-home\n"

fail = 0


def run(workflow):
    """Run the real check against `workflow`; return (returncode, combined output)."""
    result = subprocess.run(
        [sys.executable, str(CHECK), "--workflow", str(workflow)],
        capture_output=True, text=True, check=False,
    )
    return result.returncode, result.stdout + result.stderr


def write(text, work):
    path = work / "mutated-ci.yml"
    path.write_text(text)
    return path


def expect(name, workflow, want_rc, needle):
    global fail
    rc, out = run(workflow)
    if rc != want_rc:
        print(f"FAIL - {name}: exit {rc} (want {want_rc})\n{out}")
        fail = 1
    elif needle and needle not in out:
        print(f"FAIL - {name}: exited {rc} but did not report {needle!r}\n{out}")
        fail = 1
    else:
        print(f"ok   - {name}")


with tempfile.TemporaryDirectory(prefix="install-action-home-") as work:
    work = Path(work)
    real = REAL_WORKFLOW.read_text()

    # Baseline: the shipped workflow must pass BEFORE anything is mutated, or a red below
    # would prove nothing (it could be red for the wrong reason).
    expect("baseline: the shipped ci.yml passes", REAL_WORKFLOW, 0, "PASS")

    assert real.count(ENV_BLOCK) == 3, \
        f"expected three job-private HOME blocks in ci.yml, found {real.count(ENV_BLOCK)}"

    # Drop each override separately: every job's action must still be protected.
    blocks = real.split(ENV_BLOCK)
    for index, job in enumerate(("test", "test-macos", "desktop")):
        mutated = ENV_BLOCK.join(blocks[:index + 1]) + ENV_BLOCK.join(blocks[index + 1:])
        expect(f"reds when {job}'s install-action step loses its HOME",
               write(mutated, work), 1, f"job '{job}'")

    # Drop all overrides — the exact reviewer mutation: reverting the whole HOME fix.
    expect("reds when all install-action steps lose their HOME",
           write(real.replace(ENV_BLOCK, ""), work), 1, "must set a job-private")

    # A HOME that is set but NOT job-private (a fixed shared path) is still the bug.
    expect("reds when HOME is set but shared",
           write(real.replace(ENV_BLOCK, ENV_BLOCK.replace(
               "          HOME: ${{ runner.temp }}/install-action-home\n", SHARED_HOME)), work),
           1, "not job-private")

    # No over-match: a step that is not install-action (and sets no HOME) is ignored.
    neutral = (
        "jobs:\n"
        "  test:\n"
        "    steps:\n"
        "      - uses: actions/checkout@v4\n"
        "      - uses: taiki-e/install-action@v2\n"
        "        with:\n"
        "          tool: nextest@0.9.100\n"
        "        env:\n"
        "          HOME: ${{ runner.temp }}/install-action-home\n"
    )
    expect("green: a non-install-action step with no HOME is ignored",
           write(neutral, work), 0, "PASS")

    # A vacuous pass is not a pass: a workflow with no action step must not report green.
    expect("reds (setup) when no install-action step exists",
           write("jobs:\n  test:\n    steps:\n      - uses: actions/checkout@v4\n", work),
           2, "vacuously")

if fail:
    print("check-install-action-home_test: FAILURES")
    sys.exit(1)
print("check-install-action-home_test: all cases passed")
