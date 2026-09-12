#!/usr/bin/env python3
"""Mimic the daemon's turn loop: spawn in its own process group, stream stdout,
close stdin on the terminal line, reap. Records timings and exit code."""
import json, os, signal, subprocess, sys, time

def main():
    out_prefix = sys.argv[1]
    cwd = sys.argv[2]
    mode = sys.argv[3]          # 'stdin-json' | 'arg' | 'stdin-raw'
    prompt_file = sys.argv[4]
    terminal = sys.argv[5]      # substring marking the terminal line ('' = none)
    argv = sys.argv[6:]
    prompt = open(prompt_file).read()
    if mode == 'arg':
        argv = argv + [prompt]

    t0 = time.time()
    p = subprocess.Popen(argv, cwd=cwd, stdin=subprocess.PIPE,
                         stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                         start_new_session=True)
    print(f"PGID={os.getpgid(p.pid)} PID={p.pid}", file=sys.stderr)
    if mode == 'stdin-json':
        p.stdin.write((json.dumps({"type":"user","message":{"role":"user","content":prompt}})+"\n").encode())
        p.stdin.flush()
    elif mode == 'stdin-raw':
        p.stdin.write(prompt.encode()); p.stdin.flush(); p.stdin.close(); p.stdin = None
    else:
        p.stdin.close(); p.stdin = None

    so = open(out_prefix + ".stdout", "wb")
    events = []
    first = None
    for raw in p.stdout:
        if first is None: first = time.time() - t0
        so.write(raw); so.flush()
        events.append((round(time.time()-t0, 3), raw))
        if terminal and terminal in raw.decode("utf-8", "replace"):
            if p.stdin:
                p.stdin.close(); p.stdin = None
            term_at = time.time() - t0
            print(f"TERMINAL_AT={term_at:.3f}s", file=sys.stderr)
    if p.stdin:
        p.stdin.close()
    err = p.stderr.read()
    rc = p.wait()
    so.close()
    open(out_prefix + ".stderr", "wb").write(err)
    with open(out_prefix + ".timing", "w") as f:
        for t, raw in events:
            f.write(f"{t:8.3f}  {len(raw):6d}  {raw[:110].decode('utf-8','replace').rstrip()}\n")
    print(f"EXIT={rc} TTFB={first} TOTAL={time.time()-t0:.3f}s STDERR_BYTES={len(err)}", file=sys.stderr)
    return 0

sys.exit(main())
