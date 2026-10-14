#!/usr/bin/env python3
"""Pin check-release-keychain-lock.py by MAKING IT FIRE (STUDIO-1280).

A check nobody has watched fire is not a guard: it converts "nobody checked" into "we checked
and it is clean". This mutation test runs the real check against throwaway copies of
`.github/workflows/release.yml` with the STUDIO-878 defect reintroduced one rule at a time — the
exclusive acquisition dropped, the list replaced instead of prepended, the restore made a
snapshot restore — and asserts the check reds on each and names the rule. The green baseline
matters just as much: a red below would prove nothing if the shipped workflow did not pass first.

No dependencies beyond python3. Run from anywhere:
`python3 harness/ci/check-release-keychain-lock_test.py`.
"""
import subprocess
import sys
import tempfile
from pathlib import Path

CHECK = Path(__file__).with_name("check-release-keychain-lock.py").resolve()
REAL_WORKFLOW = CHECK.parents[2] / ".github" / "workflows" / "release.yml"

fail = 0


def run(workflow):
    """Run the real check against `workflow`; return (returncode, combined output)."""
    result = subprocess.run(
        [sys.executable, str(CHECK), "--workflow", str(workflow)],
        capture_output=True, text=True, check=False,
    )
    return result.returncode, result.stdout + result.stderr


def expect(name, text, work, want_rc, needle):
    global fail
    path = work / "mutated-release.yml"
    path.write_text(text)
    rc, out = run(path)
    if rc != want_rc:
        print(f"FAIL - {name}: exit {rc} (want {want_rc})\n{out}")
        fail = 1
    elif needle and needle not in out:
        print(f"FAIL - {name}: exited {rc} but did not report {needle!r}\n{out}")
        fail = 1
    else:
        print(f"ok   - {name}")


with tempfile.TemporaryDirectory(prefix="release-keychain-lock-") as work:
    work = Path(work)
    real = REAL_WORKFLOW.read_text()

    # The exact constructs the fix relies on, so a mutation that fails to match the shipped
    # text is caught here rather than silently testing nothing.
    for snippet in (
        "flock($fh, LOCK_EX)",
        "bash -c \"$(cat <<'SIGNING_WINDOW'",
        'new_list=("$SIGNING_KEYCHAIN")',
        'new_list+=("$kc")',
        "security list-keychains -d user -s ${new_list[@]+",
        "restore_search_list() {",
        "trap restore_search_list EXIT",
        'if [ "$kc" != "$SIGNING_KEYCHAIN" ]; then remaining+=("$kc"); fi',
        'if [ "${#remaining[@]}" -eq 0 ] ||',
        "keychain-search-list.lock",
        "make dmg VERSION=",
    ):
        assert snippet in real, f"shipped release.yml lost {snippet!r}"

    # Baseline: the shipped workflow must pass BEFORE anything is mutated.
    expect("baseline: the shipped release.yml passes", real, work, 0, "PASS")

    # 1. The exclusive acquisition is removed — the STUDIO-878 review mutation. This is the
    #    regression the ticket asks for: the check must fail when it is gone.
    expect(
        "reds when the exclusive flock(2) acquisition is removed",
        real.replace('flock($fh, LOCK_EX) or die "keychain-search-list lock: flock $path: $!\\n";\n', ""),
        work, 1, "no exclusive flock(2) is taken",
    )
    expect(
        "reds when the lock is a shared (LOCK_SH) lock",
        real.replace("flock($fh, LOCK_EX)", "flock($fh, LOCK_SH)"),
        work, 1, "no exclusive flock(2) is taken",
    )

    # 2. The rewrite is not inside the locked window.
    expect(
        "reds when the rewrite is not run inside the locked window",
        real.replace("bash -c \"$(cat <<'SIGNING_WINDOW'", "bash -c true"),
        work, 1, "not run inside the locked window",
    )
    expect(
        "reds when make dmg runs outside the locked window",
        real.replace('make dmg VERSION="$version"', "true").rstrip()
        + '\n          make dmg VERSION="$version"\n',
        work, 1, "make dmg runs OUTSIDE the locked window",
    )
    expect(
        "reds when the restore trap is armed only after make dmg",
        real.replace("trap restore_search_list EXIT\n", "").replace(
            "\n          SIGNING_WINDOW",
            "\n            trap restore_search_list EXIT\n          SIGNING_WINDOW",
        ),
        work, 1, "armed only after make dmg",
    )

    # 3. The list is replaced instead of prepended.
    expect(
        "reds when the shared list is replaced (the B1 defect)",
        real.replace(
            'new_list=("$SIGNING_KEYCHAIN")',
            'security list-keychains -d user -s "$SIGNING_KEYCHAIN" login.keychain-db\n            new_list=()',
        ),
        work, 1, "REPLACED",
    )
    expect(
        "reds when nothing prepends the signing keychain",
        real.replace('new_list=("$SIGNING_KEYCHAIN")', "new_list=()"),
        work, 1, "nothing PREPENDS the signing keychain",
    )
    expect(
        "reds when the preserved list is never written back",
        real.replace(
            'security list-keychains -d user -s ${new_list[@]+"${new_list[@]}"}', ""
        ),
        work, 1, "never written back",
    )

    # 4. The restore clobbers instead of removing only our keychain.
    expect(
        "reds when the restore restores a snapshot",
        real.replace(
            'security list-keychains -d user -s ${remaining[@]+"${remaining[@]}"}',
            "security list-keychains -d user -s $original_list",
        ),
        work, 1, "restores a snapshot",
    )
    expect(
        "reds when the restore does not remove only the signing keychain",
        real.replace(
            'if [ "$kc" != "$SIGNING_KEYCHAIN" ]; then remaining+=("$kc"); fi',
            'remaining+=("$kc")',
        ),
        work, 1, "does not remove only the signing keychain",
    )
    expect(
        "reds when the restore can write an empty list",
        real.replace(
            'if [ "${#remaining[@]}" -eq 0 ] || [ "${#remaining[@]}" -eq "${#current[@]}" ]; then\n                return 0\n              fi\n',
            "",
        ),
        work, 1, "guard against writing an empty list",
    )
    expect(
        "reds when the restore is not wired to an EXIT trap",
        real.replace("trap restore_search_list EXIT\n", ""),
        work, 1, "never wires the restore to an EXIT trap",
    )

    # 5. A second lock path would not serialize against Tally's.
    expect(
        "reds when the lock is not the shared one",
        real.replace("keychain-search-list.lock", "rhapsody-only.lock"),
        work, 1, "not the shared",
    )

    # A vacuous pass is not a pass: a workflow with none of this machinery must not report green.
    expect(
        "reds when the keychain-lock machinery is absent entirely",
        "name: release\njobs:\n  build:\n    steps:\n      - run: make dmg\n",
        work, 1, "no exclusive flock(2) is taken",
    )

if fail:
    print("check-release-keychain-lock_test: FAILURES")
    sys.exit(1)
print("check-release-keychain-lock_test: all cases passed")
