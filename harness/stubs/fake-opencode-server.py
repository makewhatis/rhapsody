#!/usr/bin/env python3
"""Owned-server test double around the existing deterministic CLI scenario scripts.

The real adapter launches this as `serve`. Session/prompt/permission requests and
scoped SSE are real HTTP; the scenario child still checks the old knob precedence,
state collisions, auth retry and escaped tool cleanup. This is not a production
fallback. Pinned-binary canaries independently qualify OpenCode's real protocol.
"""
import base64
import http.server
import json
import os
import pathlib
import queue
import re
import subprocess
import sys
import threading

script, *argv = sys.argv[1:]
port = int(argv[argv.index("--port") + 1])
body = pathlib.Path(script).read_text()
found = re.search(r'"sessionID"\s*:\s*"(ses_[A-Za-z0-9_]+)"', body)
sid = found.group(1) if found else "ses_" + re.sub(r"[^A-Za-z0-9]", "", pathlib.Path(os.environ["XDG_DATA_HOME"]).name)
events = queue.Queue()
subscribed = threading.Event()
auth = "Basic " + base64.b64encode(("opencode:" + os.environ["OPENCODE_SERVER_PASSWORD"]).encode()).decode()
directory = str(pathlib.Path.cwd())


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def authorized(self):
        if self.headers.get("Authorization") == auth and self.headers.get("x-opencode-directory") == directory:
            return True
        self.reply(401, {})
        return False

    def reply(self, status, doc):
        data = json.dumps(doc).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        if not self.authorized():
            return
        if self.path == "/event":
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Connection", "close")
            self.end_headers()
            self.send_event({"type": "server.connected", "properties": {}})
            subscribed.set()
            while True:
                event = events.get()
                if event is None:
                    return
                self.send_event(event)
        elif self.path == "/agent":
            self.reply(200, [{"name": n, "mode": "primary"} for n in ["build", "plan", "compaction"]])
        elif self.path == "/global/health":
            self.reply(200, {"healthy": True, "version": "1.18.30"})
        elif self.path == "/config":
            self.reply(200, json.loads(os.environ.get("OPENCODE_CONFIG_CONTENT", "{}")))
        elif self.path.startswith("/session/"):
            self.reply(200, {"id": self.path.split("/")[2], "directory": directory})
        elif self.path == "/session":
            self.reply(200, [{"id": sid, "directory": directory}])
        else:
            self.reply(404, {})

    def send_event(self, event):
        self.wfile.write(("data: " + json.dumps(event) + "\n\n").encode())
        self.wfile.flush()

    def do_POST(self):
        if not self.authorized():
            return
        request = json.loads(self.rfile.read(int(self.headers.get("Content-Length", "0"))) or "{}")
        if self.path == "/session":
            assert subscribed.is_set(), "session creation must follow subscription readiness"
            self.reply(200, {"id": sid, "directory": directory})
            return
        if self.path.endswith("/fork"):
            self.reply(200, {"id": sid, "directory": directory})
            return
        if self.path.startswith("/permission/") or self.path.endswith("/share"):
            self.reply(200, {})
            return
        assert self.path.endswith("/message") or self.path.endswith("/command"), self.path
        assert subscribed.is_set(), "prompt before event readiness"
        owned = self.path.split("/")[2]
        config = json.loads(os.environ.get("OPENCODE_CONFIG_CONTENT", "{}"))
        managed = "enabled_providers" in config
        manager = "tools" in config
        args = ["run", "--format", "json"]
        if managed:
            args += ["--pure"]
        if not manager:
            args += ["--auto"]
        args += ["--dir", directory]
        model = request.get("model", {})
        model = (model.get("providerID", "") + "/" + model.get("modelID", "")) if isinstance(model, dict) else model
        if manager or managed:
            args += ["--agent", request.get("agent", "build")]
        if model:
            args += ["-m", model]
        if request.get("variant"):
            args += ["--variant", request["variant"]]
        if request.get("agent") and not (manager or managed):
            args += ["--agent", request["agent"]]
        record = pathlib.Path(os.environ["XDG_DATA_HOME"]) / "fake-server-session"
        if record.exists() or not self.path.startswith("/session/" + sid + "/"):
            args += ["-s", owned]
        record.write_text(owned)
        if "--log-level" in argv:
            i = argv.index("--log-level")
            args += argv[i:i+2]
        args += [request.get("arguments", "\n".join(p["text"] for p in request.get("parts", []) if p["type"] == "text"))]
        interpreter = "python3" if body.startswith("#!/usr/bin/env python3") else "bash"
        child = subprocess.Popen([interpreter, script, *args], stdin=subprocess.DEVNULL, stdout=subprocess.PIPE)
        count = 0
        for raw in child.stdout:
            try:
                line = json.loads(raw)
            except (ValueError, UnicodeError):
                # Model scanner bounds/protocol corruption rather than manufacturing success.
                self.send_bad(raw)
                continue
            kind = line.get("type")
            if kind == "error":
                events.put({"type": "session.error", "properties": {"sessionID": owned, "error": line.get("error", {})}})
                continue
            if kind not in ("text", "tool_use", "step_start", "step_finish"):
                continue
            count += 1
            part = line.get("part", {}).copy()
            part["id"] = part.get("id", "prt_" + str(count))
            part["sessionID"] = owned
            part["messageID"] = "msg_fake"
            part["type"] = {"tool_use": "tool", "step_start": "step-start", "step_finish": "step-finish"}.get(kind, kind)
            if kind == "text":
                part["time"] = {"end": 1}
            events.put({"type": "message.part.updated", "properties": {"sessionID": owned, "part": part}})
        child.wait()
        events.put({"type": "session.status", "properties": {"sessionID": owned, "status": {"type": "idle"}}})
        self.reply(200, {})

    def send_bad(self, raw):
        if len(raw) > 10 * 1024 * 1024:
            events.put({"type": "message.part.updated", "properties": {"sessionID": sid, "part": {"id": "prt_big", "sessionID": sid, "type": "text", "time": {"end": 1}, "text": "x" * len(raw)}}})


http.server.ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()
