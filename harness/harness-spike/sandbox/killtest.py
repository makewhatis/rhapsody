#!/usr/bin/env python3
"""Exactly the daemon's spawn: cmd.process_group(0) == setpgid(0,0) in the child,
then libc kill(-pid, SIGKILL). No setsid."""
import os, signal, subprocess, sys, time
label, cwd, wait_s = sys.argv[1], sys.argv[2], float(sys.argv[3])
argv = sys.argv[4:]
p = subprocess.Popen(argv, cwd=cwd, stdin=subprocess.DEVNULL,
                     stdout=open(os.environ.get("KILLTEST_OUT", f"/tmp/killtest-{label}.out"), "wb"),
                     stderr=subprocess.DEVNULL,
                     preexec_fn=lambda: os.setpgid(0, 0))   # == Rust cmd.process_group(0)
pgid = os.getpgid(p.pid)
print(f"[{label}] pid={p.pid} pgid={pgid} daemon_pgid={os.getpgid(0)}")
time.sleep(wait_s)
ps = subprocess.run(["ps","-eo","pid=,ppid=,pgid=,comm="],capture_output=True,text=True).stdout
rows=[]
for ln in ps.splitlines():
    f=ln.split(None,3)
    if len(f)==4: rows.append((int(f[0]),int(f[1]),int(f[2]),f[3]))
kids,frontier=[],{p.pid}
for _ in range(6):
    new=[r for r in rows if r[1] in frontier and r[0] not in frontier]
    if not new: break
    kids+=new; frontier|={r[0] for r in new}
print(f"[{label}] descendants={len(kids)}")
for k in kids: print(f"    pid={k[0]} ppid={k[1]} pgid={k[2]} {'IN-GROUP' if k[2]==pgid else 'ESCAPED'} {k[3]}")
os.kill(-pgid, signal.SIGKILL)     # == libc::kill(-(pid), SIGKILL)
try: print(f"[{label}] leader rc={p.wait(timeout=10)}")
except subprocess.TimeoutExpired: print(f"[{label}] LEADER SURVIVED")
time.sleep(1.5)
alive=[k for k in kids if subprocess.run(["kill","-0",str(k[0])],capture_output=True).returncode==0]
print(f"[{label}] SURVIVORS AFTER kill(-{pgid}, SIGKILL): {len(alive)}")
for k in alive: print(f"    ORPHAN pid={k[0]} pgid={k[2]} {k[3]}")
