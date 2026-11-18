#!/usr/bin/env python3
"""Deterministic stdlib-only stateless MCP 2026-07-28 HTTP fixture for the `notes` branch."""

from __future__ import annotations

import argparse
import json
import socketserver
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PROTOCOL_VERSION = "2026-07-28"

TOOLS = [
    {
        "name": "customer_notes",
        "description": "Return account notes recorded for a shop customer id.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "customer_id": {
                    "type": "integer",
                    "minimum": 1,
                }
            },
            "required": ["customer_id"],
            "additionalProperties": False,
        },
    }
]

RESOURCES = [
    {
        "uri": "note://handbook/onboarding",
        "name": "Onboarding Notes",
        "mimeType": "application/json",
    }
]

NOTES_BY_CUSTOMER: dict[int, list[dict[str, object]]] = {
    1: [
        {
            "note_id": "n-1",
            "customer_id": 1,
            "summary": "Prefers consolidated monthly invoice",
        }
    ],
    2: [
        {
            "note_id": "n-2",
            "customer_id": 2,
            "summary": "Requested signature on delivery",
        }
    ],
}

RESOURCE_BODIES: dict[str, str] = {
    "note://handbook/onboarding": json.dumps(
        {"topic": "onboarding", "owner": "support-team", "version": 1},
        separators=(",", ":"),
    )
}


class McpNotesHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt: str, *args: object) -> None:
        return

    def _send_json(self, status: int, payload: object) -> None:
        body = json.dumps(payload, separators=(",", ":")).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _rpc_result(self, req_id: object, result: dict[str, object]) -> None:
        out = dict(result)
        out.setdefault("resultType", "complete")
        self._send_json(200, {"jsonrpc": "2.0", "id": req_id, "result": out})

    def _rpc_error(self, status: int, req_id: object, code: int, message: str) -> None:
        self._send_json(
            status,
            {
                "jsonrpc": "2.0",
                "id": req_id,
                "error": {"code": code, "message": message},
            },
        )

    def do_POST(self) -> None:  # noqa: N802
        if self.path != "/mcp":
            self._send_json(404, {"error": "not_found"})
            return

        length_raw = self.headers.get("Content-Length", "0")
        try:
            length = int(length_raw)
        except ValueError:
            self._rpc_error(400, None, -32700, "invalid Content-Length")
            return
        if length < 0 or length > 1024 * 1024:
            self._rpc_error(413, None, -32600, "frame exceeds 1 MiB limit")
            return

        raw_body = self.rfile.read(length)
        try:
            frame = json.loads(raw_body.decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError):
            self._rpc_error(400, None, -32700, "invalid JSON-RPC payload")
            return

        if not isinstance(frame, dict) or frame.get("jsonrpc") != "2.0":
            self._rpc_error(400, None, -32600, "expected JSON-RPC 2.0 object")
            return

        req_id = frame.get("id")
        method = frame.get("method")
        params = frame.get("params") or {}
        if not isinstance(method, str) or not isinstance(params, dict):
            self._rpc_error(400, req_id, -32600, "invalid method or params")
            return

        if "id" not in frame:
            self.send_response(202)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return

        mcp_method = self.headers.get("Mcp-Method")
        if mcp_method != method:
            self._rpc_error(400, req_id, -32022, "missing or mismatched Mcp-Method header")
            return

        meta = params.get("_meta")
        if not isinstance(meta, dict) or meta.get("io.modelcontextprotocol/protocolVersion") != PROTOCOL_VERSION:
            self._rpc_error(200, req_id, -32602, "missing _meta protocolVersion 2026-07-28")
            return

        expected_name: str | None = None
        if method == "tools/call":
            expected_name = params.get("name") if isinstance(params.get("name"), str) else None
        elif method == "resources/read":
            expected_name = params.get("uri") if isinstance(params.get("uri"), str) else None
        if expected_name is not None and self.headers.get("Mcp-Name") != expected_name:
            self._rpc_error(400, req_id, -32022, "missing or mismatched Mcp-Name header")
            return

        if method == "server/discover":
            self._rpc_result(
                req_id,
                {
                    "supportedVersions": [PROTOCOL_VERSION],
                    "capabilities": {"tools": {}, "resources": {}},
                    "serverInfo": {"name": "notes-mcp-fixture", "version": "0.1.0"},
                    "ttlMs": 60000,
                    "cacheScope": "public",
                },
            )
            return

        if method == "tools/list":
            self._rpc_result(
                req_id,
                {
                    "tools": TOOLS,
                    "ttlMs": 60000,
                    "cacheScope": "public",
                },
            )
            return

        if method == "resources/list":
            self._rpc_result(
                req_id,
                {
                    "resources": RESOURCES,
                    "ttlMs": 60000,
                    "cacheScope": "public",
                },
            )
            return

        if method == "resources/templates/list":
            self._rpc_result(
                req_id,
                {
                    "resourceTemplates": [],
                    "ttlMs": 60000,
                    "cacheScope": "public",
                },
            )
            return

        if method == "resources/read":
            uri = params.get("uri")
            if not isinstance(uri, str) or uri not in RESOURCE_BODIES:
                self._rpc_error(200, req_id, -32002, "resource not found")
                return
            self._rpc_result(
                req_id,
                {
                    "contents": [
                        {
                            "uri": uri,
                            "mimeType": "application/json",
                            "text": RESOURCE_BODIES[uri],
                        }
                    ],
                    "ttlMs": 0,
                    "cacheScope": "private",
                },
            )
            return

        if method == "tools/call":
            tool_name = params.get("name")
            arguments = params.get("arguments") or {}
            if tool_name != "customer_notes":
                self._rpc_error(200, req_id, -32602, f"unknown tool: {tool_name}")
                return
            customer_id = arguments.get("customer_id")
            if not isinstance(customer_id, int) or isinstance(customer_id, bool) or customer_id < 1:
                self._rpc_error(200, req_id, -32602, "customer_id must be an integer >= 1")
                return
            notes = NOTES_BY_CUSTOMER.get(customer_id, [])
            structured = {"customer_id": customer_id, "notes": notes}
            self._rpc_result(
                req_id,
                {
                    "content": [
                        {
                            "type": "text",
                            "text": json.dumps(structured, separators=(",", ":")),
                        }
                    ],
                    "structuredContent": structured,
                    "isError": False,
                },
            )
            return

        self._rpc_error(200, req_id, -32601, "method not found")


class LocalhostHTTPServer(ThreadingHTTPServer):
    allow_reuse_address = True

    def server_bind(self) -> None:
        # Bypass socket.getfqdn() to avoid intermittent mDNS/rDNS stalls on loopback.
        socketserver.TCPServer.server_bind(self)
        self.server_name, self.server_port = str(self.server_address[0]), self.server_address[1]


def main() -> int:
    parser = argparse.ArgumentParser(description="Run the deterministic GAP notes MCP HTTP fixture.")
    parser.add_argument(
        "--port",
        type=int,
        default=9102,
        help="Loopback TCP port to bind on 127.0.0.1 (0 picks an ephemeral port; default: 9102).",
    )
    args = parser.parse_args()

    server = LocalhostHTTPServer(("127.0.0.1", args.port), McpNotesHandler)
    bound_port = server.server_address[1]
    print(
        f"mcp_fixture listening on http://127.0.0.1:{bound_port}/mcp (port={bound_port})",
        flush=True,
    )
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
