#!/usr/bin/env python3
"""Zero-cost probe: N concurrent `goose acp` processes each do initialize +
session/new (no prompt, so no tokens are spent) and print the id they were given."""
import json, subprocess, sys, threading
N = int(sys.argv[1]); cwd = sys.argv[2]
out = [None]*N
def one(i):
    p = subprocess.Popen(["goose","acp"], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                         stderr=subprocess.DEVNULL, text=True, bufsize=1)
    p.stdin.write(json.dumps({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{"fs":{"readTextFile":False,"writeTextFile":False},"terminal":False}}})+"\n"); p.stdin.flush()
    p.stdout.readline()
    p.stdin.write(json.dumps({"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":cwd,"mcpServers":[]}})+"\n"); p.stdin.flush()
    while True:
        ln = p.stdout.readline()
        if not ln: out[i]="EOF"; break
        f=json.loads(ln)
        if f.get("id")==2:
            out[i]=f.get("result",{}).get("sessionId") or json.dumps(f.get("error"))
            break
    p.stdin.close(); p.wait(timeout=15)
ts=[threading.Thread(target=one,args=(i,)) for i in range(N)]
[t.start() for t in ts]; [t.join() for t in ts]
print("ids:", out)
print("distinct:", len(set(out)), "of", N)
