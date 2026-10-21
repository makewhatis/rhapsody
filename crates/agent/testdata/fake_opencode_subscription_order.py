#!/usr/bin/env python3
"""Deterministic probe for the fake OpenCode server's `/event` subscription handshake (STUDIO-1439).

Executes `harness/stubs/fake-opencode-server.py`'s real `/event` handler against a recording
response and asserts the fixture recorded the subscription BEFORE it wrote its first response byte.

The adapter's contract is "subscribe before creating a session", and it takes the arrival of
`server.connected` as proof that it has. A fixture that records the subscription only after that
event is written therefore lets a correct adapter reach the session guard first, and the turn fails
with `opencode_control_failed: HTTP request failed` — the flake this probe pins. Running the real
handler here reproduces that exact order deterministically: no sleep, no load, no timing.
"""
import os
import pathlib
import sys
import tempfile

fixture, script = sys.argv[1], sys.argv[2]

import http.server


# The fixture constructs and serves a real server at import time; neutralise that so its handler can
# be driven directly. `exec` (not the import machinery) also keeps this probe from writing a
# `__pycache__` into the stubs directory.
class _NoServer:
    def __init__(self, *a, **k):
        pass

    def serve_forever(self):
        pass


http.server.ThreadingHTTPServer = _NoServer
sys.argv = [fixture, script, "serve", "--port", "1"]
os.environ.setdefault("OPENCODE_SERVER_PASSWORD", "probe-password")
os.environ.setdefault("XDG_DATA_HOME", tempfile.mkdtemp())

module = {"__name__": "fake_opencode_server", "__file__": fixture}
source = pathlib.Path(fixture).read_text()
exec(compile(source, fixture, "exec"), module)

# Every response byte of the /event handshake, in order, each tagged with whether the subscription
# had been recorded when it was written.
written = []


class Response:
    def write(self, data):
        written.append(("event stream", module["subscribed"].is_set()))

    def flush(self):
        pass


class Probe(module["Handler"]):
    """The real handler, with the socket replaced by a recorder so its code runs unmodified."""

    path = "/event"
    wfile = Response()

    def __init__(self):
        pass

    def authorized(self):
        return True

    def send_response(self, *a, **k):
        written.append(("status line", module["subscribed"].is_set()))

    def send_header(self, *a, **k):
        written.append(("header", module["subscribed"].is_set()))

    def end_headers(self, *a, **k):
        written.append(("header terminator", module["subscribed"].is_set()))


module["events"].put(None)  # let the /event loop end right after its first frame
Probe().do_GET()

if not written:
    print("the fixture wrote no /event response at all", file=sys.stderr)
    sys.exit(1)
first, recorded = written[0]
if not recorded:
    print(
        f"the fixture wrote its first /event response byte ({first}) before recording the "
        "subscription: a client that acts on server.connected can race the session guard",
        file=sys.stderr,
    )
    sys.exit(1)
print(f"subscription recorded before the first /event byte ({first})")
