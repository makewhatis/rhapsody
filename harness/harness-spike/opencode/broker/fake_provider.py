#!/usr/bin/env python3
"""PB0 fake OpenAI-compatible provider for the managed OpenCode broker fixtures.

This is the deterministic, credential-free half of STUDIO-995. It is a loopback
`POST /v1/chat/completions` SSE server that records every request OpenCode makes to it and
answers per a named scenario, so a real pinned OpenCode binary can be exercised without a real
provider or a paid key. `capture.sh` drives it; the resulting request snapshots live in
`requests/` and are pinned by `crates/agent/tests/opencode_broker_fixture.rs`.

It never records the raw request body: prompts are large, machine-specific, and carry absolute
paths. A snapshot keeps only the shape the broker contract is defined over (`method`, `path`,
`headers`, the closed top-level `body` keys, `model`, `max_tokens`, `stream`, `stream_options`,
`tool_choice`, the tool names, and the message roles). The capability value in the
`authorization` header is redacted to `<CAPABILITY>` before it is ever written.

Scenarios:
  happy       one successful streaming turn (the title-disabled main request)
  retry       first request answers 500, the next answers 200 (OpenCode retries)
  auth        every request answers 401 with a non-retryable body
  compaction  one successful tool-less turn (`--agent compaction`)
  subagent    first request returns a `task` tool call; the subagent's request then succeeds

Usage:
  fake_provider.py --scenario <name> --record <path.json> [--port N] [--capability VALUE]
On start it prints `LISTENING <port>` and flushes, so the caller never assumes a fixed port.
"""

import argparse
import http.server
import json
import sys
import threading

CAPABILITY_PLACEHOLDER = "<CAPABILITY>"

# Headers that are part of the pinned request shape. Everything else (host, content-length,
# date, server) is either machine-specific or connection noise and is deliberately dropped.
RECORDED_HEADERS = ("authorization", "content-type", "accept", "user-agent")


def _snapshot(path, headers, body, status, capability):
    """Reduce one raw request to the committed, machine-independent shape."""
    auth = headers.get("authorization", "")
    if capability and auth == "Bearer " + capability:
        auth = "Bearer " + CAPABILITY_PLACEHOLDER
    elif auth.startswith("Bearer "):
        # The managed capability did not reach the provider (a project/ambient key won). Record it
        # as unexpected so `capture.sh` refuses to commit a fixture that proves the wrong thing.
        auth = "Bearer <UNEXPECTED>"
    recorded = {"authorization": auth}
    for name in RECORDED_HEADERS:
        if name == "authorization":
            continue
        if name in headers:
            recorded[name] = headers[name]

    body = body if isinstance(body, dict) else {}
    tools = body.get("tools")
    tool_names = []
    if isinstance(tools, list):
        for tool in tools:
            fn = tool.get("function") if isinstance(tool, dict) else None
            if isinstance(fn, dict) and isinstance(fn.get("name"), str):
                tool_names.append(fn["name"])
    messages = body.get("messages")
    roles = []
    if isinstance(messages, list):
        for msg in messages:
            if isinstance(msg, dict) and isinstance(msg.get("role"), str):
                roles.append(msg["role"])

    return {
        "method": "POST",
        "path": path,
        "response_status": status,
        "headers": recorded,
        "body": {
            "keys": sorted(body.keys()),
            "model": body.get("model"),
            "max_tokens": body.get("max_tokens"),
            "stream": body.get("stream"),
            "stream_options": body.get("stream_options"),
            "tool_choice": body.get("tool_choice"),
            "tool_names": tool_names,
            "message_roles": roles,
        },
    }


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    scenario = "happy"
    record_path = None
    capability = ""
    records = []
    lock = threading.Lock()

    def log_message(self, *args):  # silence BaseHTTPRequestHandler's stderr chatter
        pass

    def _read_body(self):
        length = int(self.headers.get("Content-Length", "0") or 0)
        raw = self.rfile.read(length) if length else b""
        try:
            return json.loads(raw) if raw else None
        except ValueError:
            return None

    def _json(self, status, payload):
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Connection", "close")
        self.close_connection = True
        body = json.dumps(payload).encode()
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _sse(self, events):
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Connection", "close")
        self.close_connection = True
        self.end_headers()
        for event in events:
            self.wfile.write(("data: " + json.dumps(event) + "\n\n").encode())
            self.wfile.flush()
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()

    def do_GET(self):
        if self.path.endswith("/models"):
            self._json(200, {"object": "list", "data": [{"id": "probe-model", "object": "model"}]})
            return
        self._json(404, {"error": {"message": "not found"}})

    def do_POST(self):
        body = self._read_body()
        headers = {k.lower(): v for k, v in self.headers.items()}

        with Handler.lock:
            index = len(Handler.records)
            Handler.records.append(None)  # reserve the slot so `index` is stable

        status, events = self._plan(index)
        snapshot = _snapshot(self.path, headers, body, status, Handler.capability)
        with Handler.lock:
            Handler.records[index] = snapshot
            self._flush_locked()

        if isinstance(events, int):
            self._json(events, {"error": {"message": self._message(events)}})
            return
        self._sse(events)

    def _flush_locked(self):
        if not Handler.record_path:
            return
        with open(Handler.record_path, "w") as fh:
            json.dump(Handler.records, fh, indent=2, sort_keys=True)
            fh.write("\n")

    @staticmethod
    def _message(status):
        return "invalid api key" if status == 401 else "temporarily unavailable"

    def _plan(self, index):
        scenario = Handler.scenario
        if scenario == "auth":
            return 401, 401
        if scenario == "retry" and index == 0:
            return 500, 500
        if scenario == "subagent" and index == 0:
            return 200, _task_tool_call_events()
        return 200, _text_events()


def _base():
    return {
        "id": "chatcmpl-pb0-fake",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": "probe-model",
    }


def _text_events():
    base = _base()
    return [
        {**base, "choices": [{"index": 0, "delta": {"role": "assistant", "content": "P0_OK"}, "finish_reason": None}]},
        {**base, "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]},
        {**base, "choices": [], "usage": {"prompt_tokens": 10, "completion_tokens": 3, "total_tokens": 13}},
    ]


def _task_tool_call_events():
    base = _base()
    args = json.dumps(
        {
            "description": "probe the subagent path",
            "prompt": "Reply with P0_OK.",
            "subagent_type": "general",
        }
    )
    return [
        {
            **base,
            "choices": [
                {
                    "index": 0,
                    "delta": {
                        "role": "assistant",
                        "tool_calls": [
                            {
                                "index": 0,
                                "id": "call_pb0_subagent",
                                "type": "function",
                                "function": {"name": "task", "arguments": ""},
                            }
                        ],
                    },
                    "finish_reason": None,
                }
            ],
        },
        {
            **base,
            "choices": [
                {
                    "index": 0,
                    "delta": {
                        "tool_calls": [{"index": 0, "function": {"arguments": args}}]
                    },
                    "finish_reason": None,
                }
            ],
        },
        {**base, "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]},
        {**base, "choices": [], "usage": {"prompt_tokens": 20, "completion_tokens": 5, "total_tokens": 25}},
    ]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--scenario", required=True)
    parser.add_argument("--record", required=True)
    parser.add_argument("--port", type=int, default=0)
    parser.add_argument("--capability", default="")
    args = parser.parse_args()

    Handler.scenario = args.scenario
    Handler.record_path = args.record
    Handler.capability = args.capability
    Handler.records = []

    server = http.server.ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
    print("LISTENING", server.server_address[1], flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        sys.exit(0)


if __name__ == "__main__":
    main()
