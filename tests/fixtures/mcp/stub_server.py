#!/usr/bin/env python3
"""A tiny stdio MCP server (stateless 2026-07-28 era) used by the `#[ignore = "needs child processes"]` test of WP-M1-09.

It answers server/discover, tools/list and tools/call (an `echo` tool); it never prints anything but protocol lines on stdout.
"""
import json
import sys

TOOLS = [{"name": "echo", "description": "echoes its arguments",
          "inputSchema": {"type": "object", "properties": {"id": {"type": "integer"}}, "required": ["id"]}},
         {"name": "env", "description": "lists the names of the environment variables it received",
          "inputSchema": {"type": "object"}}]
import os


def reply(msg_id, result=None, error=None):
    out = {"jsonrpc": "2.0", "id": msg_id}
    out["error" if error else "result"] = error or result
    sys.stdout.write(json.dumps(out) + "\n")
    sys.stdout.flush()


for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        msg = json.loads(line)
    except ValueError:
        continue
    method, msg_id, params = msg.get("method"), msg.get("id"), msg.get("params") or {}
    if msg_id is None:
        continue  # notification (initialized, cancelled, ...)
    if method == "server/discover":
        reply(msg_id, {"resultType": "complete", "supportedVersions": ["2026-07-28"],
                       "capabilities": {"tools": {}}, "ttlMs": 0, "cacheScope": "private"})
    elif method == "tools/list":
        reply(msg_id, {"resultType": "complete", "tools": TOOLS, "ttlMs": 0, "cacheScope": "private"})
    elif method == "tools/call" and params.get("name") == "env":
        reply(msg_id, {"resultType": "complete", "structuredContent": {"names": sorted(os.environ.keys())}})
    elif method == "tools/call":
        reply(msg_id, {"resultType": "complete", "structuredContent": {"echo": params.get("arguments")}})
    else:
        reply(msg_id, error={"code": -32601, "message": "method not found"})
