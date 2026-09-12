#!/usr/bin/env python3
"""Drive an ACP agent over stdio the way a daemon adapter would: spawn in its own
process group, speak JSON-RPC on stdin/stdout, log every frame in both directions
verbatim, close stdin when the turn ends, reap.

The CLI harnesses of STUDIO-869 are driven by `drive.py`, which streams one-way
stdout. ACP is bidirectional and request/response, so it needs its own driver —
this one. Same outputs: `<prefix>.jsonl` is the agent's stdout byte-for-byte,
`<prefix>.client.jsonl` is what the client wrote, `<prefix>.timing` lines up both
sides on one clock, `<prefix>.stderr` is stderr.

Usage:
  acp_drive.py <out_prefix> <cwd> <prompt_file> [options] -- <agent argv...>

Options:
  --mcp NAME=CMD[,ARG...]  attach a stdio MCP server to the session (repeatable)
  --client-fs              advertise fs/read_text_file + fs/write_text_file
  --client-terminal        advertise the terminal capability
  --load SESSION_ID        session/load that id instead of session/new (resume)
  --kill-after SECONDS     census the descendant tree, then kill(-pgid, SIGKILL),
                           then report survivors — killtest.py's method, over ACP
"""
import json, os, signal, subprocess, sys, threading, time

PERMISSION_PREFERENCE = ["allow_always", "allow_once", "allow", "always_allow"]


class Driver:
    def __init__(self, prefix, cwd, argv, caps):
        self.prefix, self.caps = prefix, caps
        self.t0 = time.time()
        self.next_id = 0
        self.agent_log = open(prefix + ".jsonl", "wb")
        self.client_log = open(prefix + ".client.jsonl", "wb")
        self.timing = open(prefix + ".timing", "w")
        self.tool_names = []
        self.proc = subprocess.Popen(
            argv, cwd=cwd, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            # == Rust cmd.process_group(0); killtest.py spawns identically.
            preexec_fn=lambda: os.setpgid(0, 0))
        self.pgid = os.getpgid(self.proc.pid)
        self.log(f"PID={self.proc.pid} PGID={self.pgid} argv={' '.join(argv)}")
        self.err_buf = []
        threading.Thread(target=lambda: self.err_buf.append(self.proc.stderr.read()),
                         daemon=True).start()
        self.responses = {}
        self.cond = threading.Condition()
        self.eof = threading.Event()
        self.wlock = threading.Lock()
        threading.Thread(target=self.pump, daemon=True).start()

    # --- plumbing -------------------------------------------------------
    def log(self, msg):
        self.timing.write(f"{time.time() - self.t0:8.3f}  # {msg}\n")
        self.timing.flush()
        print(f"[{time.time() - self.t0:7.3f}] {msg}", file=sys.stderr, flush=True)

    def send(self, obj):
        raw = (json.dumps(obj) + "\n").encode()
        with self.wlock:
            self._send_locked(obj, raw)

    def _send_locked(self, obj, raw):
        self.client_log.write(raw)
        self.client_log.flush()
        self.timing.write(f"{time.time() - self.t0:8.3f}  -> {json.dumps(obj)[:160]}\n")
        self.timing.flush()
        self.proc.stdin.write(raw)
        self.proc.stdin.flush()

    def request(self, method, params):
        with self.wlock:
            self.next_id += 1
            rid = self.next_id
        self.send({"jsonrpc": "2.0", "id": rid, "method": method, "params": params})
        return rid

    def reply(self, rid, result):
        self.send({"jsonrpc": "2.0", "id": rid, "result": result})

    def error(self, rid, code, message):
        self.send({"jsonrpc": "2.0", "id": rid, "error": {"code": code, "message": message}})

    def read_frame(self):
        """Return the next frame, or None at EOF. The agent's bytes are logged
        exactly as received before anything looks at them."""
        raw = self.proc.stdout.readline()
        if not raw:
            return None
        self.agent_log.write(raw)
        self.agent_log.flush()
        self.timing.write(f"{time.time() - self.t0:8.3f}  <- {raw[:160].decode('utf-8', 'replace').rstrip()}\n")
        self.timing.flush()
        try:
            return json.loads(raw)
        except ValueError:
            self.log(f"NON-JSON LINE ON STDOUT ({len(raw)} bytes): {raw[:120]!r}")
            return {"__nonjson__": raw.decode("utf-8", "replace")}

    # --- agent-initiated traffic ---------------------------------------
    def handle(self, f):
        """Dispatch one inbound frame. Returns a result for a pending id, else None."""
        method = f.get("method")
        if method is None:
            return f                                  # a response to one of ours
        rid = f.get("id")
        p = f.get("params") or {}
        if method == "session/update":
            u = p.get("update") or {}
            kind = u.get("sessionUpdate")
            if kind in ("tool_call", "tool_call_update"):
                name = u.get("title") or u.get("rawInput", {}).get("name")
                key = (u.get("kind"), u.get("toolCallId"), name)
                if kind == "tool_call":
                    self.tool_names.append(key)
                    self.log(f"TOOL_CALL kind={u.get('kind')} id={u.get('toolCallId')} title={name!r}")
            return None
        if method == "fs/read_text_file":
            if not self.caps["fs"]:
                self.error(rid, -32601, "client does not implement fs/read_text_file")
                return None
            path = p["path"]
            self.log(f"CLIENT fs/read_text_file {path}")
            try:
                with open(path, encoding="utf-8") as fh:
                    self.reply(rid, {"content": fh.read()})
            except OSError as e:
                self.error(rid, -32603, str(e))
            return None
        if method == "fs/write_text_file":
            if not self.caps["fs"]:
                self.error(rid, -32601, "client does not implement fs/write_text_file")
                return None
            self.log(f"CLIENT fs/write_text_file {p['path']}")
            try:
                with open(p["path"], "w", encoding="utf-8") as fh:
                    fh.write(p.get("content", ""))
                self.reply(rid, None)
            except OSError as e:
                self.error(rid, -32603, str(e))
            return None
        if method == "session/request_permission":
            opts = p.get("options") or []
            pick = None
            for want in PERMISSION_PREFERENCE:
                for o in opts:
                    if o.get("kind") == want or o.get("optionId") == want:
                        pick = o
                        break
                if pick:
                    break
            pick = pick or (opts[0] if opts else None)
            tool = (p.get("toolCall") or {}).get("title")
            self.log(f"PERMISSION tool={tool!r} options={[o.get('optionId') for o in opts]} -> {pick and pick.get('optionId')}")
            if pick is None:
                self.error(rid, -32603, "no permission options offered")
            else:
                self.reply(rid, {"outcome": {"outcome": "selected", "optionId": pick["optionId"]}})
            return None
        if method.startswith("terminal/"):
            self.log(f"CLIENT {method} (unimplemented) params={json.dumps(p)[:200]}")
            self.error(rid, -32601, f"client does not implement {method}")
            return None
        self.log(f"UNHANDLED AGENT REQUEST {method} params={json.dumps(p)[:200]}")
        if rid is not None:
            self.error(rid, -32601, f"client does not implement {method}")
        return None

    def pump(self):
        """Read and dispatch frames until the agent's stdout closes. Responses to
        our own requests land in self.responses; everything else is answered here,
        so the agent never blocks on a client request we have not read yet."""
        while True:
            f = self.read_frame()
            if f is None:
                self.eof.set()
                with self.cond:
                    self.cond.notify_all()
                return
            res = self.handle(f)
            if res is not None and res.get("id") is not None:
                with self.cond:
                    self.responses[res["id"]] = res
                    self.cond.notify_all()

    def await_id(self, rid, timeout=900):
        deadline = time.time() + timeout
        with self.cond:
            while rid not in self.responses:
                if self.eof.is_set():
                    self.log(f"EOF on agent stdout while waiting for id={rid}")
                    return None
                if not self.cond.wait(timeout=max(0.0, deadline - time.time())):
                    self.log(f"TIMEOUT waiting for id={rid} after {timeout}s")
                    return None
            return self.responses.pop(rid)

    # --- kill path ------------------------------------------------------
    def descendants(self):
        ps = subprocess.run(["ps", "-eo", "pid=,ppid=,pgid=,comm="],
                            capture_output=True, text=True).stdout
        rows = []
        for ln in ps.splitlines():
            fs = ln.split(None, 3)
            if len(fs) == 4:
                rows.append((int(fs[0]), int(fs[1]), int(fs[2]), fs[3]))
        kids, frontier = [], {self.proc.pid}
        for _ in range(6):
            new = [r for r in rows if r[1] in frontier and r[0] not in frontier]
            if not new:
                break
            kids += new
            frontier |= {r[0] for r in new}
        return kids

    def kill_and_census(self):
        kids = self.descendants()
        print(f"[goose-acp] pid={self.proc.pid} pgid={self.pgid} descendants={len(kids)}")
        for k in kids:
            tag = "IN-GROUP" if k[2] == self.pgid else "ESCAPED"
            print(f"    pid={k[0]} ppid={k[1]} pgid={k[2]} {tag} {k[3]}")
        os.kill(-self.pgid, signal.SIGKILL)   # == libc::kill(-(pid), SIGKILL)
        try:
            print(f"[goose-acp] leader rc={self.proc.wait(timeout=10)}")
        except subprocess.TimeoutExpired:
            print("[goose-acp] LEADER SURVIVED")
        time.sleep(1.5)
        alive = [k for k in kids
                 if subprocess.run(["kill", "-0", str(k[0])], capture_output=True).returncode == 0]
        print(f"[goose-acp] SURVIVORS AFTER kill(-{self.pgid}, SIGKILL): {len(alive)}")
        for k in alive:
            print(f"    ORPHAN pid={k[0]} pgid={k[2]} {k[3]}")
        return alive

    def finish(self, rc_timeout=20):
        if self.proc.stdin and not self.proc.stdin.closed:
            self.proc.stdin.close()
        try:
            rc = self.proc.wait(timeout=rc_timeout)
        except subprocess.TimeoutExpired:
            self.log("agent did not exit on stdin close; SIGKILLing the group")
            os.kill(-self.pgid, signal.SIGKILL)
            rc = self.proc.wait(timeout=10)
        err = self.err_buf[0] if self.err_buf else b""
        open(self.prefix + ".stderr", "wb").write(err)
        self.log(f"EXIT={rc} STDERR_BYTES={len(err)} TOTAL={time.time() - self.t0:.3f}s")
        for f in (self.agent_log, self.client_log, self.timing):
            f.close()
        print(f"EXIT={rc} STDERR_BYTES={len(err)}", file=sys.stderr)
        return rc


def main():
    a = sys.argv[1:]
    prefix, cwd, prompt_file = a[0], a[1], a[2]
    rest = a[3:]
    mcps, caps, load_id, kill_after = [], {"fs": False, "terminal": False}, None, None
    argv = ["goose", "acp"]
    i = 0
    while i < len(rest):
        t = rest[i]
        if t == "--mcp":
            spec = rest[i + 1]
            name, _, cmd = spec.partition("=")
            parts = cmd.split(",")
            mcps.append({"name": name, "command": parts[0], "args": parts[1:], "env": []})
            i += 2
        elif t == "--client-fs":
            caps["fs"] = True
            i += 1
        elif t == "--client-terminal":
            caps["terminal"] = True
            i += 1
        elif t == "--load":
            load_id = rest[i + 1]
            i += 2
        elif t == "--kill-after":
            kill_after = float(rest[i + 1])
            i += 2
        elif t == "--":
            argv = rest[i + 1:]
            break
        else:
            print(f"acp_drive.py: unknown option {t!r}", file=sys.stderr)
            return 2

    d = Driver(prefix, cwd, argv, caps)
    client_caps = {"fs": {"readTextFile": caps["fs"], "writeTextFile": caps["fs"]},
                   "terminal": caps["terminal"]}
    init = d.await_id(d.request("initialize", {"protocolVersion": 1,
                                               "clientCapabilities": client_caps}), timeout=60)
    if init is None or "result" not in init:
        d.log("initialize failed")
        return d.finish()

    if load_id:
        sid = load_id
        r = d.await_id(d.request("session/load", {"sessionId": sid, "cwd": cwd,
                                                  "mcpServers": mcps}), timeout=300)
        d.log(f"session/load -> {json.dumps(r)[:300] if r else None}")
        if r is None or "result" not in r:
            return d.finish()
    else:
        r = d.await_id(d.request("session/new", {"cwd": cwd, "mcpServers": mcps}), timeout=300)
        if r is None or "result" not in r:
            d.log(f"session/new failed: {json.dumps(r)[:400] if r else 'no response'}")
            return d.finish()
        sid = r["result"]["sessionId"]
    d.log(f"SESSION_ID={sid}")
    print(f"SESSION_ID={sid}", file=sys.stderr, flush=True)

    prompt = open(prompt_file).read()
    pid_req = d.request("session/prompt", {"sessionId": sid,
                                           "prompt": [{"type": "text", "text": prompt}]})

    if kill_after is not None:
        # Let the model get as far as launching its shell child — the pump thread
        # keeps answering it meanwhile — then kill exactly the way the daemon does
        # and report what lived through it.
        time.sleep(kill_after)
        d.kill_and_census()
        for f in (d.agent_log, d.client_log, d.timing):
            f.close()
        open(prefix + ".stderr", "wb").write(d.err_buf[0] if d.err_buf else b"")
        return 0

    res = d.await_id(pid_req, timeout=900)
    d.log(f"session/prompt -> {json.dumps(res)[:400] if res else 'no response'}")
    print(f"TOOL_CALLS={len(d.tool_names)}", file=sys.stderr)
    for k in d.tool_names:
        print(f"    kind={k[0]} id={k[1]} title={k[2]!r}", file=sys.stderr)
    return d.finish()


sys.exit(main())
