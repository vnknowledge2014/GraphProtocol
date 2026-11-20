#!/usr/bin/env python3
"""Local interactive bilingual (EN / VI) GAP Learning Server (HTTP on 127.0.0.1)."""

from __future__ import annotations

import argparse
import atexit
import json
import mimetypes
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
WEB_DIR = Path(__file__).resolve().parent

ALLOWED_CLI_SUBS = {
    "serve",
    "init",
    "doctor",
    "config",
    "models",
    "proposals",
    "snapshot",
    "policy",
    "audit",
    "calibrate",
    "bench",
    "ingest",
    "tasks",
    "purge",
    "completions",
}

GAP_KEY = "gap_dev_skeleton_key_01"
GAP_KEY_HASH = "8a7d1134387be2f4aeb799122161c9868d94bdbec857230c917947a746ac8ba3"


def find_gap_binary() -> Path | None:
    for candidate in (
        ROOT / "target" / "dist" / "gap",
        ROOT / "target" / "release" / "gap",
        ROOT / "target" / "debug" / "gap",
    ):
        if candidate.is_file():
            return candidate
    return None


def free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return int(s.getsockname()[1])


class DemoManager:
    def __init__(self) -> None:
        self._lock = threading.Lock()
        self.procs: list[subprocess.Popen] = []
        self.tmpdir: tempfile.TemporaryDirectory | None = None
        self.gap_port: int | None = None

    def is_running(self) -> bool:
        with self._lock:
            return bool(self.procs) and all(p.poll() is None for p in self.procs)

    def stop(self) -> None:
        with self._lock:
            for p in reversed(self.procs):
                if p.poll() is None:
                    p.terminate()
                    try:
                        p.wait(timeout=3.0)
                    except subprocess.TimeoutExpired:
                        p.kill()
                        p.wait(timeout=2.0)
            self.procs.clear()
            if self.tmpdir is not None:
                self.tmpdir.cleanup()
                self.tmpdir = None
            self.gap_port = None

    def start(self) -> int:
        gap_bin = find_gap_binary()
        if gap_bin is None:
            raise RuntimeError("Missing compiled binary `target/dist/gap`; run `./dist/build-reproducible.sh --offline` first")
        self.stop()
        with self._lock:
            tmp = tempfile.TemporaryDirectory(prefix="gap_learn_web_")
            tmp_dir = Path(tmp.name)
            rest_port = free_port()
            mcp_port = free_port()
            gap_port = free_port()

            shutil.copy2(ROOT / "examples" / "walking-skeleton" / "shop.openapi.json", tmp_dir / "shop.openapi.json")
            cfg = tmp_dir / "gap.toml"
            cfg.write_text(
                f"""profile = "dev"

[server]
listen = "127.0.0.1:{gap_port}"
shutdown_grace_ms = 5000

[gateway]
default_tenant = "default"
facade_spotlight = true

[auth]
allow_anonymous = false

[telemetry]
log_level = "warn"
log_json = false
service_name = "gap-web-playground"

[[branches]]
id = "shop"
protocol = "openapi"
trust = "T1"

[branches.openapi]
spec = "shop.openapi.json"
base_url = "http://127.0.0.1:{rest_port}"

[[branches]]
id = "notes"
protocol = "mcp"
trust = "T1"

[branches.mcp]
transport = "http"
url = "http://127.0.0.1:{mcp_port}/mcp"

[[api_keys]]
id = "dev-agent"
key_hash = "{GAP_KEY_HASH}"
tenant = "default"
scopes = ["read"]
""",
                encoding="utf-8",
            )

            rest_proc = subprocess.Popen(
                [sys.executable, str(ROOT / "examples/walking-skeleton/fixtures/rest_fixture.py"), "--port", str(rest_port)],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.PIPE,
            )
            mcp_proc = subprocess.Popen(
                [sys.executable, str(ROOT / "examples/walking-skeleton/fixtures/mcp_fixture.py"), "--port", str(mcp_port)],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.PIPE,
            )
            time.sleep(0.25)
            gap_proc = subprocess.Popen(
                [str(gap_bin), "serve", "--config", str(cfg)],
                cwd=tmp_dir,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.PIPE,
            )
            self.tmpdir = tmp
            self.procs = [rest_proc, mcp_proc, gap_proc]
            self.gap_port = gap_port

        deadline = time.perf_counter() + 10.0
        while time.perf_counter() < deadline:
            try:
                with urllib.request.urlopen(f"http://127.0.0.1:{gap_port}/readyz", timeout=1.0) as r:
                    if r.status == 200:
                        return gap_port
            except Exception:
                time.sleep(0.05)
        self.stop()
        raise RuntimeError("Timed out waiting for local gap serve /readyz")


MANAGER = DemoManager()
atexit.register(MANAGER.stop)


PLAYGROUND_TARGETS = {
    "graphql-join": (
        "/graphql",
        {
            "Content-Type": "application/json",
            "Accept": "application/graphql-response+json",
            "Authorization": f"Bearer {GAP_KEY}",
        },
        {"query": "{ customer(id: 1) { id name tier } orders(customer_id: 1) { id total_cents status } customer_notes(customer_id: 1) }"},
    ),
    "mcp-l0-describe": (
        "/mcp",
        {
            "Content-Type": "application/json",
            "Accept": "application/json, text/event-stream",
            "MCP-Protocol-Version": "2026-07-28",
            "Mcp-Method": "tools/call",
            "Mcp-Name": "gap_describe",
            "Authorization": f"Bearer {GAP_KEY}",
        },
        {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientCapabilities": {},
                },
                "name": "gap_describe",
                "arguments": {"overview": True},
            },
        },
    ),
    "mcp-l1-search": (
        "/mcp",
        {
            "Content-Type": "application/json",
            "Accept": "application/json, text/event-stream",
            "MCP-Protocol-Version": "2026-07-28",
            "Mcp-Method": "tools/call",
            "Mcp-Name": "gap_search",
            "Authorization": f"Bearer {GAP_KEY}",
        },
        {
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientCapabilities": {},
                },
                "name": "gap_search",
                "arguments": {"query": "customer order notes", "limit": 5},
            },
        },
    ),
    "mcp-l2-type": (
        "/mcp",
        {
            "Content-Type": "application/json",
            "Accept": "application/json, text/event-stream",
            "MCP-Protocol-Version": "2026-07-28",
            "Mcp-Method": "tools/call",
            "Mcp-Name": "gap_describe",
            "Authorization": f"Bearer {GAP_KEY}",
        },
        {
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientCapabilities": {},
                },
                "name": "gap_describe",
                "arguments": {"names": ["Customer"]},
            },
        },
    ),
}


class Handler(BaseHTTPRequestHandler):
    def log_message(self, format: str, *args) -> None:  # noqa: A002
        return

    def send_json(self, code: int, obj: dict) -> None:
        raw = json.dumps(obj, ensure_ascii=False).encode("utf-8")
        self.send_response(code)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def do_GET(self) -> None:  # noqa: N802
        parsed = urllib.parse.urlparse(self.path)
        if parsed.path == "/api/status":
            bin_path = find_gap_binary()
            self.send_json(
                200,
                {
                    "binary_found": bin_path is not None,
                    "binary_path": str(bin_path) if bin_path else None,
                    "demo_running": MANAGER.is_running(),
                    "gap_port": MANAGER.gap_port,
                },
            )
            return

        if parsed.path == "/api/cli-help":
            qs = urllib.parse.parse_qs(parsed.query)
            sub = (qs.get("sub") or [""])[0]
            if sub not in ALLOWED_CLI_SUBS:
                self.send_json(400, {"error": f"Unsupported subcommand: {sub}"})
                return
            bin_path = find_gap_binary()
            if bin_path is None:
                self.send_json(503, {"error": "gap binary not built yet"})
                return
            res = subprocess.run([str(bin_path), sub, "--help"], capture_output=True, text=True, timeout=5.0)
            self.send_json(200, {"output": res.stdout or res.stderr})
            return

        rel = "index.html" if parsed.path in ("/", "") else parsed.path.lstrip("/")
        if rel == "CITATION.cff":
            root_ref = (ROOT / "CITATION.cff").resolve()
            if not root_ref.is_file():
                self.send_error(404, "Not found")
                return
            candidate = root_ref
        elif rel == "paper" or rel.startswith("paper/"):
            paper_root = (ROOT / "paper").resolve()
            candidate = (paper_root / "gap-paper.html").resolve() if rel in ("paper", "paper/") else (ROOT / rel).resolve()
            if not candidate.is_relative_to(paper_root):
                self.send_error(404, "Not found")
                return
            if candidate.is_dir():
                candidate = (paper_root / "gap-paper.html").resolve()
            if not candidate.is_relative_to(paper_root) or not candidate.is_file():
                self.send_error(404, "Not found")
                return
        else:
            web_root = WEB_DIR.resolve()
            candidate = (WEB_DIR / rel).resolve()
            if not candidate.is_relative_to(web_root):
                self.send_error(404, "Not found")
                return
            if candidate.is_dir():
                candidate = (candidate / "index.html").resolve()
            if not candidate.is_relative_to(web_root) or not candidate.is_file():
                self.send_error(404, "Not found")
                return
        mime, _ = mimetypes.guess_type(str(candidate))
        ctype = f"{mime or 'application/octet-stream'}; charset=utf-8" if (mime or "").startswith("text/") or mime in ("application/javascript", "application/json") else (mime or "application/octet-stream")
        data = candidate.read_bytes()
        total = len(data)
        range_hdr = self.headers.get("Range")
        if range_hdr and range_hdr.startswith("bytes=") and total > 0:
            spec = range_hdr[6:].split(",", 1)[0].strip()
            if "-" in spec:
                start_s, end_s = spec.split("-", 1)
                try:
                    if start_s == "":
                        suffix = int(end_s)
                        start = max(0, total - suffix)
                        end = total - 1
                    else:
                        start = int(start_s)
                        end = total - 1 if end_s == "" else min(int(end_s), total - 1)
                    if 0 <= start <= end < total:
                        chunk = data[start : end + 1]
                        self.send_response(206)
                        self.send_header("Content-Type", ctype)
                        self.send_header("Accept-Ranges", "bytes")
                        self.send_header("Content-Range", f"bytes {start}-{end}/{total}")
                        self.send_header("Content-Length", str(len(chunk)))
                        self.end_headers()
                        try:
                            self.wfile.write(chunk)
                        except (BrokenPipeError, ConnectionResetError):
                            pass
                        return
                except ValueError:
                    pass
                self.send_response(416)
                self.send_header("Content-Range", f"bytes */{total}")
                self.send_header("Content-Length", "0")
                self.end_headers()
                return
        self.send_response(200)
        self.send_header("Content-Type", ctype)
        self.send_header("Accept-Ranges", "bytes")
        self.send_header("Content-Length", str(total))
        self.end_headers()
        try:
            self.wfile.write(data)
        except (BrokenPipeError, ConnectionResetError):
            pass

    def do_POST(self) -> None:  # noqa: N802
        parsed = urllib.parse.urlparse(self.path)
        if parsed.path == "/api/start-demo":
            try:
                port = MANAGER.start()
                self.send_json(200, {"demo_running": True, "gap_port": port})
            except Exception as exc:
                self.send_json(500, {"error": str(exc)})
            return

        if parsed.path == "/api/stop-demo":
            MANAGER.stop()
            self.send_json(200, {"demo_running": False})
            return

        if parsed.path == "/api/playground":
            length = int(self.headers.get("Content-Length", "0"))
            raw = self.rfile.read(min(length, 16384))
            try:
                req_data = json.loads(raw.decode("utf-8"))
                sc_id = req_data.get("scenario_id")
            except Exception:
                self.send_json(400, {"error": "Invalid JSON"})
                return
            if sc_id not in PLAYGROUND_TARGETS:
                self.send_json(400, {"error": f"Unknown scenario_id: {sc_id}"})
                return
            if not MANAGER.is_running() or MANAGER.gap_port is None:
                self.send_json(503, {"error": "Demo gateway is not running"})
                return
            path, headers, payload = PLAYGROUND_TARGETS[sc_id]
            url = f"http://127.0.0.1:{MANAGER.gap_port}{path}"
            t0 = time.perf_counter()
            req = urllib.request.Request(url, data=json.dumps(payload).encode("utf-8"), headers=headers, method="POST")
            try:
                with urllib.request.urlopen(req, timeout=5.0) as resp:
                    body_txt = resp.read().decode("utf-8")
                    lat_ms = round((time.perf_counter() - t0) * 1000.0, 2)
                    try:
                        body_obj = json.loads(body_txt)
                    except Exception:
                        body_obj = body_txt
                    self.send_json(200, {"http_status": resp.status, "latency_ms": lat_ms, "body": body_obj})
            except urllib.error.HTTPError as err:
                lat_ms = round((time.perf_counter() - t0) * 1000.0, 2)
                self.send_json(200, {"http_status": err.code, "latency_ms": lat_ms, "body": err.read().decode("utf-8", errors="replace")})
            return

        self.send_error(404, "Not found")


def main() -> int:
    ap = argparse.ArgumentParser(description="Run local bilingual (EN/VI) GAP learning portal server")
    ap.add_argument("--port", type=int, default=8787, help="Loopback TCP port (default 8787)")
    args = ap.parse_args()
    server = ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
    print(f"GAP Bilingual Learning Portal (EN/VI) listening at http://127.0.0.1:{args.port}", flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        MANAGER.stop()
        server.server_close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
