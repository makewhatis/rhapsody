#!/usr/bin/env python3
"""Credential-free bounded MCP lifecycle canary; never contacts the daemon."""
import json
import sys
import time

for line in sys.stdin:
    request = json.loads(line)
    if "id" not in request:
        continue
    method = request.get("method")
    if method == "initialize":
        result = {"protocolVersion": "2024-11-05", "capabilities": {"tools": {}}, "serverInfo": {"name": "canary", "version": "1"}}
    elif method == "tools/list":
        result = {"tools": [{"name": "wait", "description": "bounded silent wait", "inputSchema": {"type": "object", "properties": {"fail": {"type": "boolean"}}}}]}
    elif method == "tools/call":
        time.sleep(1)
        print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "error": {"code": -32000, "message": "canary tool error"}}), flush=True)
        continue
    else:
        result = {}
    print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": result}), flush=True)
