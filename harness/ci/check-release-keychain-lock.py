#!/usr/bin/env python3
"""The release workflow must serialize its keychain search-list rewrite on the shared lock
(STUDIO-1280).

The user keychain search list is ONE host-global, per-user setting, and the same Mac that runs
this repo's release signs Tally's iOS releases too. `codesign` resolves the signing PRIVATE KEY
by walking that list, first match wins — `--keychain` / `OTHER_CODE_SIGN_FLAGS` only scope where
the CERTIFICATE is found — so a process that rewrites the list while another is mid-archive can
evict or reorder the signing keychain out from under it, and the victim fails with
`errSecInternalComponent`. Tally's `fastlane/Fastfile` (STUDIO-878) serializes its rewrite on
`~/.rhapsody/keychain-search-list.lock`; a lock only one writer honours is not a lock, so this
workflow must take the SAME lock, across the whole window the list order matters: the rewrite
through `make dmg`.

The old release step did `security list-keychains -d user -s "$SIGNING_KEYCHAIN" login.keychain-db`,
REPLACING the whole list with no lock — evicting a concurrent Tally run's keychain (STUDIO-878
review finding alice:B1). This check pins the fix:

  * the list is rewritten by PREPENDING the signing keychain and keeping every existing entry,
    never replaced by the signing keychain + a hardcoded login;
  * that rewrite, the `make dmg` that depends on it, and the restore all run while an exclusive
    `flock(2)` on the shared lock file is held (via perl — macOS has no `flock(1)`, and
    `lockf(1)` is `fcntl(2)`, a different lock namespace that does not exclude Tally's
    `File#flock`);
  * the restore removes ONLY the signing keychain from whatever the list is by then — it never
    restores a snapshot captured earlier (that clobbers a concurrent holder's own change) and
    never writes an empty list.

Prose has no compiler, so this rides the existing `lint` gate like `check-install-action-home.py`.
A guard nobody has watched fire is not a guard, so `check-release-keychain-lock_test.py`
reintroduces the defect (drops the exclusive acquisition, the prepend, the surgical restore) and
asserts this scan reds on it.

The scan matches COMMANDS, not prose (STUDIO-1283): comment lines are ignored and the `LOCK_FILE=`
assignment must pin the shared path exactly, because the shared path is named several times in
release.yml's own comments — a whole-file substring search passed even when only the executable
assignment drifted to a different lock.

Usage: check-release-keychain-lock.py [--workflow PATH]
Exits 0 when the workflow satisfies every rule; 1 when one is violated; 2 on a setup error.
"""
import argparse
import re
import sys
from pathlib import Path

DEFAULT_WORKFLOW = (
    Path(__file__).resolve().parents[2] / ".github" / "workflows" / "release.yml"
)

# A full-line comment: `#` after optional whitespace. A structural check must not match prose.
COMMENT_RE = re.compile(r"^[ \t]*#")
# The exclusive acquisition: an flock(2) taken through perl, held for the guarded child.
FLOCK_MODULE_RE = re.compile(r"perl\s+-MFcntl=:flock")
LOCK_EX_RE = re.compile(r"flock\(\$\w+,\s*LOCK_EX\)")
# The lock file the wrapper must take, pinned at the ASSIGNMENT and at the CALL (STUDIO-1283).
# The rule these replace — a substring search for `keychain-search-list.lock` anywhere in the
# file — matched release.yml's own comments, which name the shared path several times, so a
# drift that changed only the executable LOCK_FILE= assignment still passed.
LOCK_FILE_ASSIGN_RE = re.compile(
    r'\bLOCK_FILE="\$HOME/\.rhapsody/keychain-search-list\.lock"'
)
FLOCK_LOCKFILE_RE = re.compile(r'"\$LOCK_FILE"[ \t]+bash[ \t]+-c')
# The window the lock must be held across: a heredoc handed to `bash -c` inside the locked
# perl invocation. The marker is captured so the guarded body can be sliced out.
WINDOW_OPEN_RE = re.compile(r"bash\s+-c\s+\"\$\(cat <<'([A-Za-z_][A-Za-z0-9_]*)'")
# The defect this replaces: the signing keychain + a hardcoded login, replacing the whole list.
REPLACE_RE = re.compile(
    r'security list-keychains -d user -s "\$SIGNING_KEYCHAIN" login\.keychain-db'
)
# The prepend: seed a list with our keychain, append the snapshot entries, set that list.
PREPEND_SEED_RE = re.compile(r'new_list=\("\$SIGNING_KEYCHAIN"\)')
PREPEND_APPEND_RE = re.compile(r'new_list\+=\("\$kc"\)')
SET_RE = re.compile(r"security list-keychains -d user -s \$\{new_list\[@\]")
# The surgical restore: a function that removes only our keychain, wired to an EXIT trap so it
# runs on failure too, and that never mentions the snapshot it must not restore.
RESTORE_FN_RE = re.compile(r"restore_search_list\(\)")
RESTORE_TRAP_RE = re.compile(r"trap\s+restore_search_list\s+EXIT")
RESTORE_REMOVES_OURS_RE = re.compile(r'if \[ "\$kc" != "\$SIGNING_KEYCHAIN" \]')
# The empty-list guard: `-s` with no keychains would set an EMPTY list, so the restore must
# leave the shared setting alone when removing our keychain would empty it.
RESTORE_EMPTY_GUARD_RE = re.compile(r'\[\s*"\$\{#remaining\[@\]\}"\s*-eq\s*0\s*\]')
# The signing work whose outcome depends on the list order.
MAKE_RE = re.compile(r"make dmg VERSION=")
SNAPSHOT_RE = re.compile(r"security list-keychains -d user \|")


def code_only(text):
    """`text` with comment lines (a `#` after optional whitespace) removed.

    release.yml documents the shared lock by name in its comments, so matching the raw file
    lets a drift that changes only the executable LOCK_FILE= assignment pass on a comment's
    mention of the path (STUDIO-1283). Comments never run; they are not evidence.
    """
    return "\n".join(line for line in text.splitlines() if not COMMENT_RE.match(line))


def window_body(text, marker):
    """The text of the heredoc named `marker`, or None when its terminator is missing.

    The heredoc lives inside the workflow's `run: |` block, so its terminator carries the
    block's YAML indentation; it is matched at the start of a line after optional whitespace.
    """
    open_at = text.find("<<'" + marker + "'")
    if open_at == -1:
        return None
    after = text.find("\n", open_at)
    if after == -1:
        return None
    terminator = re.search(
        r"^[ \t]*" + re.escape(marker) + r"[ \t]*$", text[after:], re.MULTILINE
    )
    if terminator is None:
        return None
    return text[after : after + terminator.start()]


def check_concurrent_safety(text):
    """Return the list of STUDIO-1280 rules the workflow at `text` violates."""
    problems = []
    # Match the executable workflow only: a comment naming a construct is not that construct,
    # and release.yml's comments name the shared lock repeatedly (STUDIO-1283).
    code = code_only(text)

    # 1. The shared list must be rewritten by PREPENDING our keychain, preserving the rest —
    #    never replaced by the signing keychain + a hardcoded login (the B1 defect).
    if REPLACE_RE.search(code):
        problems.append(
            "the shared keychain search list is REPLACED (signing keychain + login only), "
            "evicting a concurrent job's keychain"
        )
    if not PREPEND_SEED_RE.search(code) or not PREPEND_APPEND_RE.search(code):
        problems.append(
            "nothing PREPENDS the signing keychain to the existing search list "
            '(want: new_list=("$SIGNING_KEYCHAIN") then new_list+=("$kc"))'
        )
    if not SET_RE.search(code):
        problems.append(
            "the preserved list is never written back to the shared search list "
            "(want: security list-keychains -d user -s ${new_list[@]})"
        )

    # 2. An exclusive flock(2) on the shared lock, held across the whole rewrite -> make dmg
    #    window. Removing the acquisition must fail here.
    if not FLOCK_MODULE_RE.search(code) or not LOCK_EX_RE.search(code):
        problems.append(
            "no exclusive flock(2) is taken on the shared keychain search-list lock "
            "(want: perl -MFcntl=:flock ... flock($fh, LOCK_EX))"
        )
    # The lock must be the shared one Tally takes, pinned at the ASSIGNMENT and at the CALL.
    # A drift to a second lock would not serialize against Tally's (two different locks would
    # be no better than none), and the comments still naming the shared path must not mask it.
    if not LOCK_FILE_ASSIGN_RE.search(code):
        problems.append(
            "the lock is not the shared ~/.rhapsody/keychain-search-list.lock Tally takes "
            '(want: LOCK_FILE="$HOME/.rhapsody/keychain-search-list.lock") '
            "(two different locks would be no better than none)"
        )
    if not FLOCK_LOCKFILE_RE.search(code):
        problems.append(
            "the perl flock(2) wrapper is not invoked with the shared lock "
            '(want: perl ... "$LOCK_FILE" bash -c ...)'
        )

    window = None
    open_match = WINDOW_OPEN_RE.search(code)
    if open_match is None:
        problems.append(
            "the search-list rewrite is not run inside the locked window "
            "(want: bash -c \"$(cat <<'MARKER' ... MARKER)\" under the flock)"
        )
    else:
        window = window_body(code, open_match.group(1))
        if window is None:
            problems.append(
                f"the locked window heredoc {open_match.group(1)!r} has no terminator"
            )

    if window is not None:
        # The rewrite, the signing work, and the restore must all be inside the lock.
        if not SNAPSHOT_RE.search(window):
            problems.append("the locked window never reads the shared search list")
        make_in_window = MAKE_RE.search(window)
        if make_in_window is None:
            problems.append(
                "make dmg runs OUTSIDE the locked window, so a rewrite can land mid-archive"
            )
        if not RESTORE_TRAP_RE.search(window):
            problems.append(
                "the locked window never wires the restore to an EXIT trap, so a failed "
                "make dmg leaves the signing keychain in the shared list"
            )
        elif make_in_window is not None:
            # The restore must be armed before the archive, so it runs however make dmg ends.
            trap_at = RESTORE_TRAP_RE.search(window).start()
            if make_in_window.start() < trap_at:
                problems.append("the restore trap is armed only after make dmg")
    elif MAKE_RE.search(code):
        problems.append(
            "make dmg runs outside the exclusive lock, so a rewrite can land mid-archive"
        )

    # 3. The restore must remove only our keychain, under the lock — never restore a snapshot
    #    (that clobbers a concurrent holder's own change) and never write an empty list.
    if not RESTORE_FN_RE.search(code):
        problems.append("there is no restore_search_list helper")
    else:
        fn_at = RESTORE_FN_RE.search(code).start()
        body = code[fn_at:]
        end = re.search(r"\n[ \t]*\}[ \t]*\n", body)
        body = body if end is None else body[: end.start()]
        if not RESTORE_REMOVES_OURS_RE.search(body):
            problems.append(
                "restore_search_list does not remove only the signing keychain from the list"
            )
        if "original_list" in body:
            problems.append(
                "restore_search_list restores a snapshot of the shared list, which clobbers a "
                "concurrent job's own change"
            )
        if not RESTORE_EMPTY_GUARD_RE.search(body):
            problems.append(
                "restore_search_list does not guard against writing an empty list"
            )

    return problems


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--workflow", type=Path, default=DEFAULT_WORKFLOW)
    args = parser.parse_args()
    if not args.workflow.is_file():
        print(f"::error::release keychain-lock check: no workflow at {args.workflow}")
        return 2
    problems = check_concurrent_safety(args.workflow.read_text())
    for problem in problems:
        print(f"::error::{problem} (STUDIO-1280)")
    if problems:
        return 1
    print("release keychain-lock check: PASS (release.yml serializes the shared search list)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
