#!/usr/bin/env python3
"""Run local GA verification drills (double-build reproducibility, live HTTP cold-start/RSS/VER-032 load,
SIGKILL crash recovery, and loopback conformance tests) and emit bench/results/ga_local/report.json."""

from __future__ import annotations

import concurrent.futures
import hashlib
import json
import os
import platform
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
GAP_KEY = "gap_dev_skeleton_key_01"
GAP_KEY_HASH = "8a7d1134387be2f4aeb799122161c9868d94bdbec857230c917947a746ac8ba3"


def free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return int(s.getsockname()[1])


def wait_http_ok(url: str, timeout_s: float = 15.0) -> tuple[float, int]:
    t0 = time.perf_counter()
    deadline = t0 + timeout_s
    last_err = ""
    while time.perf_counter() < deadline:
        try:
            req = urllib.request.Request(url, method="GET")
            with urllib.request.urlopen(req, timeout=1.0) as resp:
                elapsed_ms = (time.perf_counter() - t0) * 1000.0
                return elapsed_ms, int(resp.status)
        except Exception as exc:
            last_err = str(exc)
            time.sleep(0.005)
    raise RuntimeError(f"Timed out waiting for {url}: {last_err}")


def measure_rss_mib(pid: int) -> float:
    out = subprocess.check_output(["ps", "-o", "rss=", "-p", str(pid)], text=True).strip()
    kb = int(out)
    return round(kb / 1024.0, 2)


def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()


def percentile(sorted_vals: list[float], pct: float) -> float:
    if not sorted_vals:
        return 0.0
    idx = min(len(sorted_vals) - 1, max(0, int(round((pct / 100.0) * (len(sorted_vals) - 1)))))
    return round(sorted_vals[idx], 3)


def verify_reproducible_double_build() -> dict:
    dist1 = ROOT / "target" / "dist-repro-1"
    dist2 = ROOT / "target" / "dist-repro-2"
    shutil.rmtree(dist1, ignore_errors=True)
    shutil.rmtree(dist2, ignore_errors=True)

    env1 = os.environ.copy()
    env1["OUT_DIR"] = str(dist1)
    subprocess.run([str(ROOT / "dist" / "build-reproducible.sh"), "--offline"], env=env1, check=True)
    h1 = sha256_file(dist1 / "gap")
    size1 = (dist1 / "gap").stat().st_size

    subprocess.run(["cargo", "clean", "-p", "gap-cli", "--offline"], cwd=ROOT, check=True)

    env2 = os.environ.copy()
    env2["OUT_DIR"] = str(dist2)
    subprocess.run([str(ROOT / "dist" / "build-reproducible.sh"), "--offline"], env=env2, check=True)
    h2 = sha256_file(dist2 / "gap")
    size2 = (dist2 / "gap").stat().st_size

    # Keep canonical target/dist/gap updated, remove temporary repro binaries.
    target_dist = ROOT / "target" / "dist"
    target_dist.mkdir(parents=True, exist_ok=True)
    shutil.copy2(dist2 / "gap", target_dist / "gap")
    shutil.copy2(dist2 / "gap.sha256", target_dist / "gap.sha256")
    shutil.rmtree(dist1, ignore_errors=True)
    shutil.rmtree(dist2, ignore_errors=True)

    return {
        "build_1_sha256": h1,
        "build_2_sha256": h2,
        "binary_bytes": size1,
        "size_match": size1 == size2,
        "bit_identical": h1 == h2,
    }


def run_mixed_request(idx: int, base_url: str) -> tuple[bool, float, int]:
    slot = idx % 20
    if slot < 12:  # 60%: single-branch read
        path = "/graphql"
        body = json.dumps({"query": "{ customer(id: 1) { id name tier } }"}).encode()
        headers = {
            "Content-Type": "application/json",
            "Accept": "application/graphql-response+json",
            "Authorization": f"Bearer {GAP_KEY}",
        }
        method = "POST"
    elif slot < 17:  # 25%: cross-branch join
        path = "/graphql"
        body = json.dumps(
            {
                "query": "{ customer(id: 1) { id name tier } orders(customer_id: 1) { id total_cents status } customer_notes(customer_id: 1) }"
            }
        ).encode()
        headers = {
            "Content-Type": "application/json",
            "Accept": "application/graphql-response+json",
            "Authorization": f"Bearer {GAP_KEY}",
        }
        method = "POST"
    elif slot < 19:  # 10%: MCP progressive discovery
        path = "/mcp"
        body = json.dumps(
            {
                "jsonrpc": "2.0",
                "id": idx + 1,
                "method": "tools/call",
                "params": {
                    "_meta": {
                        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                        "io.modelcontextprotocol/clientCapabilities": {},
                    },
                    "name": "gap_describe",
                    "arguments": {"overview": True},
                },
            }
        ).encode()
        headers = {
            "Content-Type": "application/json",
            "Accept": "application/json, text/event-stream",
            "MCP-Protocol-Version": "2026-07-28",
            "Mcp-Method": "tools/call",
            "Mcp-Name": "gap_describe",
            "Authorization": f"Bearer {GAP_KEY}",
        }
        method = "POST"
    else:  # 5%: readiness / liveness
        path = "/readyz" if (idx % 2 == 0) else "/healthz"
        body = None
        headers = {}
        method = "GET"

    t0 = time.perf_counter()
    req = urllib.request.Request(f"{base_url}{path}", data=body, headers=headers, method=method)
    try:
        with urllib.request.urlopen(req, timeout=5.0) as resp:
            raw = resp.read()
            elapsed_ms = (time.perf_counter() - t0) * 1000.0
            if resp.status != 200:
                return False, elapsed_ms, resp.status
            if path == "/graphql":
                parsed = json.loads(raw.decode())
                ok = "data" in parsed and not parsed.get("errors")
                return ok, elapsed_ms, resp.status
            return True, elapsed_ms, resp.status
    except urllib.error.HTTPError as err:
        return False, (time.perf_counter() - t0) * 1000.0, int(err.code)
    except Exception:
        return False, (time.perf_counter() - t0) * 1000.0, 0


def run_live_runtime_drills(gap_bin: Path) -> dict:
    with tempfile.TemporaryDirectory(prefix="gap_ga_local_") as tmp:
        tmp_dir = Path(tmp)
        rest_port = free_port()
        mcp_port = free_port()
        gap_port = free_port()

        shutil.copy2(ROOT / "examples" / "walking-skeleton" / "shop.openapi.json", tmp_dir / "shop.openapi.json")
        cfg_path = tmp_dir / "gap.toml"
        cfg_path.write_text(
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
service_name = "gap-ga-local-drill"

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
        gap_proc: subprocess.Popen[bytes] | None = None
        recovered_proc: subprocess.Popen[bytes] | None = None
        try:
            time.sleep(0.25)
            base_url = f"http://127.0.0.1:{gap_port}"
            gap_proc = subprocess.Popen(
                [str(gap_bin), "serve", "--config", str(cfg_path)],
                cwd=tmp_dir,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.PIPE,
            )
            cold_start_ms, ready_status = wait_http_ok(f"{base_url}/readyz", timeout_s=15.0)
            if ready_status != 200:
                raise RuntimeError(f"Expected 200 /readyz, got {ready_status}")
            idle_rss_mib = measure_rss_mib(gap_proc.pid)

            total_requests = 800
            concurrency = 8
            t_load_0 = time.perf_counter()
            latencies: list[float] = []
            errors = 0
            with concurrent.futures.ThreadPoolExecutor(max_workers=concurrency) as pool:
                results = list(pool.map(lambda i: run_mixed_request(i, base_url), range(total_requests)))
            load_elapsed_s = time.perf_counter() - t_load_0
            for ok, lat_ms, _status in results:
                latencies.append(lat_ms)
                if not ok:
                    errors += 1
            latencies.sort()
            post_load_rss_mib = measure_rss_mib(gap_proc.pid)

            # OS SIGKILL (`kill -9`) crash recovery drill (`VER-051`, `G-12`).
            os.kill(gap_proc.pid, signal.SIGKILL)
            kill_exit_code = gap_proc.wait(timeout=5.0)
            gap_proc = None

            recovered_proc = subprocess.Popen(
                [str(gap_bin), "serve", "--config", str(cfg_path)],
                cwd=tmp_dir,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.PIPE,
            )
            recovery_ready_ms, recovery_status = wait_http_ok(f"{base_url}/readyz", timeout_s=15.0)
            post_ok, _, post_http_status = run_mixed_request(0, base_url)
            if not post_ok or post_http_status != 200:
                raise RuntimeError("Recovered server failed post-recovery GraphQL check")

            recovered_proc.send_signal(signal.SIGTERM)
            clean_shutdown_exit = recovered_proc.wait(timeout=10.0)
            recovered_proc = None

            return {
                "cold_start_ms": round(cold_start_ms, 2),
                "cold_start_gate_le_2000ms": cold_start_ms <= 2000.0,
                "idle_rss_mib": idle_rss_mib,
                "post_load_rss_mib": post_load_rss_mib,
                "rss_gate_le_600mib": max(idle_rss_mib, post_load_rss_mib) <= 600.0,
                "mixed_load_requests": total_requests,
                "concurrency": concurrency,
                "elapsed_s": round(load_elapsed_s, 3),
                "measured_rps": round(total_requests / load_elapsed_s, 1),
                "errors": errors,
                "p50_ms": percentile(latencies, 50.0),
                "p95_ms": percentile(latencies, 95.0),
                "p99_ms": percentile(latencies, 99.0),
                "sigkill_exit_code": kill_exit_code,
                "sigkill_recovery_ready_ms": round(recovery_ready_ms, 2),
                "recovery_gate_le_60000ms": recovery_ready_ms <= 60000.0 and recovery_status == 200 and post_ok,
                "clean_sigterm_exit_code": clean_shutdown_exit,
            }
        finally:
            for proc in (gap_proc, recovered_proc, rest_proc, mcp_proc):
                if proc is not None and proc.poll() is None:
                    proc.kill()
                    proc.wait(timeout=5.0)


def run_http3_live_drill() -> dict:
    res = subprocess.run(
        ["cargo", "run", "--release", "-p", "gap-server", "--example", "http3_live_drill", "--offline", "--locked"],
        cwd=ROOT,
        capture_output=True,
        text=True,
        check=True,
    )
    for line in reversed(res.stdout.splitlines()):
        line = line.strip()
        if line.startswith("{"):
            return json.loads(line)
    raise RuntimeError(f"http3_live_drill produced no JSON line:\n{res.stdout}\n{res.stderr}")


def run_loopback_conformance() -> dict:
    tests = [
        ("ngc_kit_openapi", ["cargo", "test", "-p", "gap-conformance", "--test", "ngc_kit_openapi", "--offline", "--", "--ignored"]),
        ("ngc_kit_mcp", ["cargo", "test", "-p", "gap-conformance", "--test", "ngc_kit_mcp", "--offline", "--", "--ignored"]),
    ]
    out = {}
    for name, cmd in tests:
        res = subprocess.run(cmd, cwd=ROOT, capture_output=True, text=True, check=True)
        out[name] = {
            "passed": res.returncode == 0,
            "summary": [line for line in res.stdout.splitlines() if "test result:" in line],
        }
    return out


def main() -> int:
    repro = verify_reproducible_double_build()
    gap_bin = ROOT / "target" / "dist" / "gap"
    runtime = run_live_runtime_drills(gap_bin)
    http3 = run_http3_live_drill()
    loopback = run_loopback_conformance()
    git_head = subprocess.check_output(["git", "rev-parse", "--short", "HEAD"], cwd=ROOT, text=True).strip()

    report = {
        "schema_version": 2,
        "generated_at_utc": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "git_head_base": git_head,
        "platform": f"{platform.system().lower()}/{platform.machine().lower()}",
        "reproducible_build": repro,
        "live_runtime_drills": runtime,
        "http3_live_drills": http3,
        "loopback_conformance": loopback,
    }
    out_path = ROOT / "bench" / "results" / "ga_local" / "report.json"
    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(report, indent=2))
    assert repro["bit_identical"], "Double-build SHA-256 mismatch!"
    assert runtime["cold_start_gate_le_2000ms"], "Cold start exceeded 2000 ms!"
    assert runtime["rss_gate_le_600mib"], "RSS exceeded 600 MiB!"
    assert runtime["errors"] == 0, f"Live mixed load had {runtime['errors']} errors!"
    assert runtime["recovery_gate_le_60000ms"], "SIGKILL recovery failed!"
    assert http3["errors"] == 0, f"HTTP/3 live mixed load had {http3['errors']} errors!"
    assert http3["zero_rtt_disabled"] is True, "HTTP/3 0-RTT must be disabled!"
    return 0


if __name__ == "__main__":
    sys.exit(main())
