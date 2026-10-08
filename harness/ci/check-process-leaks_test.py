#!/usr/bin/env python3
"""Exercise the real process scan: a new leak fails; an older child/other checkout does not."""
from pathlib import Path
import subprocess
import sys
import tempfile

SCRIPT = Path(__file__).with_name("check-process-leaks.py").resolve()
ROOT = SCRIPT.parents[2]


with tempfile.TemporaryDirectory(prefix="process-scan-", dir=ROOT / "target") as work:
    work = Path(work)
    root = work / "checkout"
    other = work / "other"
    children = []
    try:
        for checkout in (root, other):
            bin_dir = checkout / "desktop/target/debug"
            bin_dir.mkdir(parents=True)
            (bin_dir / "fakedaemon").symlink_to("/bin/sleep")
        def spawn(checkout):
            child = subprocess.Popen([str(checkout / "desktop/target/debug/fakedaemon"), "60"])
            children.append(child)
            return child
        def scan(mode, *extra):
            return subprocess.run(
                [sys.executable, str(SCRIPT), mode, str(work / "baseline.json"),
                 "--root", str(root), *extra], capture_output=True, text=True, check=False,
            )
        old = spawn(root)
        assert scan("snapshot").returncode == 0
        neighbor = spawn(other)
        assert scan("check").returncode == 0, "older and other-checkout children must be preserved"
        leaked = spawn(root)
        result = scan("check", "--cleanup")
        assert result.returncode == 1 and f"fakedaemon pid {leaked.pid}" in result.stdout, result
        leaked.wait(timeout=5)
        assert old.poll() is None and neighbor.poll() is None, "cleanup killed a protected child"
        assert scan("check").returncode == 0, "cleanup must remove the newly leaked child"
        print("process leak scan mutation checks: PASS (leak named/killed; older and neighbor preserved)")
    finally:
        for child in children:
            if child.poll() is None:
                child.kill()
            child.wait()
