#!/usr/bin/env python3
"""Deterministic stdlib-only REST fixture for the GAP walking-skeleton `shop` branch."""

from __future__ import annotations

import argparse
import json
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlsplit

CUSTOMERS = [
    {"id": 1, "name": "Ada Lovelace", "tier": "vip"},
    {"id": 2, "name": "Alan Turing", "tier": "standard"},
    {"id": 3, "name": "Grace Hopper", "tier": "vip"},
]

ORDERS = [
    {"id": 101, "customer_id": 1, "total_cents": 4200, "status": "paid"},
    {"id": 102, "customer_id": 2, "total_cents": 1900, "status": "shipped"},
    {"id": 103, "customer_id": 1, "total_cents": 8500, "status": "pending"},
]


def _parse_int(values: list[str] | None, *, minimum: int) -> int | None:
    if not values:
        return None
    raw = values[0].strip()
    if not raw:
        return None
    value = int(raw)
    if value < minimum:
        raise ValueError(f"expected integer >= {minimum}")
    return value


def _slice_page(items: list[dict], query: dict[str, list[str]]) -> list[dict]:
    offset = _parse_int(query.get("offset"), minimum=0) or 0
    limit = _parse_int(query.get("limit"), minimum=1)
    sliced = items[offset:]
    if limit is not None:
        sliced = sliced[:limit]
    return sliced


class ShopHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt: str, *args: object) -> None:
        return

    def _send_json(self, status: int, payload: object) -> None:
        body = json.dumps(payload, separators=(",", ":")).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("X-RateLimit-Limit", "1000")
        self.send_header("X-RateLimit-Remaining", "999")
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self) -> None:  # noqa: N802
        parsed = urlsplit(self.path)
        path = parsed.path
        query = parse_qs(parsed.query, keep_blank_values=False)

        try:
            if path == "/customers":
                rows = list(CUSTOMERS)
                if "ids" in query and query["ids"]:
                    wanted = {
                        int(part.strip())
                        for raw in query["ids"]
                        for part in raw.split(",")
                        if part.strip()
                    }
                    rows = [row for row in rows if row["id"] in wanted]
                self._send_json(200, _slice_page(rows, query))
                return

            if path.startswith("/customers/"):
                tail = path.removeprefix("/customers/")
                if "/" in tail or not tail:
                    self._send_json(404, {"error": "not_found"})
                    return
                cid = int(tail)
                if cid < 1:
                    raise ValueError("id must be >= 1")
                for row in CUSTOMERS:
                    if row["id"] == cid:
                        self._send_json(200, row)
                        return
                self._send_json(404, {"error": "customer_not_found"})
                return

            if path == "/orders":
                rows = list(ORDERS)
                customer_id = _parse_int(query.get("customer_id"), minimum=1)
                if customer_id is not None:
                    rows = [row for row in rows if row["customer_id"] == customer_id]
                self._send_json(200, _slice_page(rows, query))
                return

            if path.startswith("/orders/"):
                tail = path.removeprefix("/orders/")
                if "/" in tail or not tail:
                    self._send_json(404, {"error": "not_found"})
                    return
                oid = int(tail)
                if oid < 1:
                    raise ValueError("id must be >= 1")
                for row in ORDERS:
                    if row["id"] == oid:
                        self._send_json(200, row)
                        return
                self._send_json(404, {"error": "order_not_found"})
                return

            self._send_json(404, {"error": "not_found"})
        except ValueError as exc:
            self._send_json(400, {"error": str(exc)})


def main() -> int:
    parser = argparse.ArgumentParser(description="Run the deterministic GAP shop REST fixture.")
    parser.add_argument(
        "--port",
        type=int,
        default=9101,
        help="Loopback TCP port to bind on 127.0.0.1 (0 picks an ephemeral port; default: 9101).",
    )
    args = parser.parse_args()

    server = ThreadingHTTPServer(("127.0.0.1", args.port), ShopHandler)
    bound_port = server.server_address[1]
    print(f"rest_fixture listening on http://127.0.0.1:{bound_port} (port={bound_port})", flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
