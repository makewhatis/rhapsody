#!/usr/bin/env python3
"""Detect new test children in THIS checkout, preserving older/other-checkout processes.

Snapshot before tests; check on every job exit (including failure/cancellation). PID + start time
identifies a process, so PID reuse does not hide a new leak. Never print full argv/credentials.
--cleanup kills only newly leaked checkout binaries and their supervisor-created process groups.
"""
import argparse
import json
import os
from pathlib import Path
import re
import signal
import subprocess


def processes(root):
    roots = {str(root.absolute()), str(root.resolve())}
    # Include test binaries as well as stand-ins/packaged sidecars. Match the executable position,
    # not a path mentioned in an agent's shell command or in another checkout's arguments.
    prefixes = [f"{base}/{suffix}" for base in roots for suffix in (
        "target/debug/", "target/release/", "desktop/target/debug/", "desktop/target/release/",
    )]
    output = subprocess.check_output(["ps", "-axo", "pid=,lstart=,command="], text=True)
    found = {}
    for line in output.splitlines():
        match = re.match(r"\s*(\d+)\s+(.{24})\s+(.+)", line)
        if not match:
            continue
        pid, started, command = match.groups()
        prefix = next((p for p in prefixes if command.startswith(p)), None)
        if prefix is None:
            continue
        executable = command[len(prefix):].split()[0]
        # Ignore compilers/build tools, but include cargo's hashed test executables under deps/.
        if not (executable.startswith("deps/") or Path(executable).name in (
            "fakedaemon", "rhapsodyd", "linear-stub",
        )):
            continue
        found[f"{pid}:{started}"] = {"pid": int(pid), "name": Path(executable).name}
    return found


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=("snapshot", "check"))
    parser.add_argument("baseline", type=Path)
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[2])
    parser.add_argument("--cleanup", action="store_true")
    args = parser.parse_args()
    current = processes(args.root)
    if args.mode == "snapshot":
        args.baseline.write_text(json.dumps(current))
        return 0
    baseline = json.loads(args.baseline.read_text())
    leaked = {key: value for key, value in current.items() if key not in baseline}
    if not leaked:
        print("process leak check: PASS (no new checkout test processes)")
        return 0
    for value in leaked.values():
        print(f"::error::test process leaked: {value['name']} pid {value['pid']}")
    if args.cleanup:
        # Revalidate the identity before signaling; never kill an unrelated recycled pid.
        live = processes(args.root)
        for key, value in leaked.items():
            if key not in live:
                continue
            try:
                pid = value["pid"]
                if os.getpgid(pid) == pid:
                    os.killpg(pid, signal.SIGKILL)
                else:
                    os.kill(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
