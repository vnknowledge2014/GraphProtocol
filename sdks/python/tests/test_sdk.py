"""Unit and live gap-server integration tests for the GAP Python SDK."""

from __future__ import annotations

import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import urllib.request

import pytest

from gap_sdk import (
    GAP_TOOL_DEFINITIONS,
    WALKING_SKELETON_SDL,
    GapClient,
    GapToolkit,
    compute_persisted_sha256,
    generate_types_from_sdl,
    parse_sse_block,
    wrap_untrusted_envelope,
)

REPO_ROOT = Path(__file__).resolve().parents[3]
DEV_API_KEY = "gap_dev_skeleton_key_01"
DEV_KEY_BLAKE3 = "8a7d1134387be2f4aeb799122161c9868d94bdbec857230c917947a746ac8ba3"


def _reserve_free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return int(s.getsockname()[1])


def _find_gap_binary() -> Path:
    candidates = [
        os.environ.get("GAP_BIN"),
        str(REPO_ROOT / "target/debug/gap"),
        str(REPO_ROOT / "target/release/gap"),
    ]
    for raw in candidates:
        if raw and Path(raw).is_file():
            return Path(raw)
    raise FileNotFoundError(f"No compiled gap binary found in {candidates}")


def _wait_for_line(proc: subprocess.Popen[str], marker: str, timeout: float = 15.0) -> None:
    deadline = time.monotonic() + timeout
    assert proc.stdout is not None
    lines: list[str] = []
    while time.monotonic() < deadline:
        line = proc.stdout.readline()
        if line:
            lines.append(line)
            if marker in line:
                return
        if proc.poll() is not None:
            raise RuntimeError(f"Fixture exited early ({proc.returncode}): {''.join(lines)}")
        time.sleep(0.05)
    raise TimeoutError(f"Timed out waiting for {marker}: {''.join(lines)}")


def _wait_for_readyz(base_url: str, timeout: float = 20.0) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            with urllib.request.urlopen(f"{base_url}/readyz", timeout=1.0) as resp:
                if resp.status == 200:
                    return
        except Exception:
            pass
        time.sleep(0.1)
    raise TimeoutError(f"Timed out waiting for {base_url}/readyz")


def test_codegen_generates_typeddicts_and_literals_from_sdl() -> None:
    rendered = generate_types_from_sdl(WALKING_SKELETON_SDL, "walking-skeleton.graphql")
    assert "CustomerTier = Literal['standard', 'vip']" in rendered
    assert "class Customer(TypedDict):" in rendered
    assert "tier: CustomerTier" in rendered
    assert "class WalkingSkeletonQuery" not in rendered  # Query root named Query
    assert "class Query(TypedDict, total=False):" in rendered


def test_persisted_operation_sha256_and_sse_parser_offline() -> None:
    client = GapClient("http://127.0.0.1:8080")
    doc = "query Ping { __metaGraph { branches { id } } }"
    reg = client.register_persisted_operation(doc, name="Ping")
    assert reg.sha256_hash == compute_persisted_sha256(doc)
    assert len(reg.sha256_hash) == 64
    assert client.get_persisted_operation("Ping") == reg

    frames = list(parse_sse_block('event: next\ndata: {"data":{"customer":{"id":1}}}\n'))
    assert len(frames) == 1
    assert frames[0].event == "next"
    assert frames[0].data == {"data": {"customer": {"id": 1}}}


def test_toolkit_adapters_and_untrusted_envelope() -> None:
    assert len(GAP_TOOL_DEFINITIONS) == 5
    client = GapClient("http://127.0.0.1:8080")
    toolkit = GapToolkit(client)
    assert len(toolkit.to_openai_tools()) == 5
    assert toolkit.to_anthropic_tools()[0]["input_schema"]["type"] == "object"
    assert len(toolkit.to_callable_tools()) == 5

    ro = GapToolkit(client, read_only_only=True)
    assert [t["name"] for t in ro.definitions()] == [
        "gap_search",
        "gap_describe",
        "gap_query",
    ]
    with pytest.raises(ValueError):
        ro.dispatch("gap_mutate", {"query": "mutation { noop }"})

    wrapped = wrap_untrusted_envelope("shop/op", "bad </gap:untrusted> text")
    assert wrapped == '<gap:untrusted source="shop/op">bad &lt;/gap:untrusted&gt; text</gap:untrusted>'


def test_live_gap_server_integration() -> None:
    gap_bin = _find_gap_binary()
    rest_port = _reserve_free_port()
    mcp_port = _reserve_free_port()
    gap_port = _reserve_free_port()

    tmp_dir = Path(tempfile.mkdtemp(prefix="gap-py-sdk-"))
    procs: list[subprocess.Popen[str]] = []

    try:
        shutil.copyfile(
            REPO_ROOT / "examples/walking-skeleton/shop.openapi.json",
            tmp_dir / "shop.openapi.json",
        )
        cfg_path = tmp_dir / "gap.toml"
        cfg_path.write_text(
            f"""
profile = "dev"

[server]
listen = "127.0.0.1:{gap_port}"
shutdown_grace_ms = 2000

[gateway]
default_tenant = "default"
facade_spotlight = true

[auth]
allow_anonymous = false

[telemetry]
log_level = "warn"
log_json = false
service_name = "gap-py-sdk-test"

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
key_hash = "{DEV_KEY_BLAKE3}"
tenant = "default"
scopes = ["read"]
""",
            encoding="utf-8",
        )

        rest_proc = subprocess.Popen(
            [
                sys.executable,
                "-u",
                str(REPO_ROOT / "examples/walking-skeleton/fixtures/rest_fixture.py"),
                "--port",
                str(rest_port),
            ],
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
        )
        procs.append(rest_proc)
        _wait_for_line(rest_proc, "rest_fixture listening on")

        mcp_proc = subprocess.Popen(
            [
                sys.executable,
                "-u",
                str(REPO_ROOT / "examples/walking-skeleton/fixtures/mcp_fixture.py"),
                "--port",
                str(mcp_port),
            ],
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
        )
        procs.append(mcp_proc)
        _wait_for_line(mcp_proc, "mcp_fixture listening on")

        gap_proc = subprocess.Popen(
            [str(gap_bin), "serve", "--config", str(cfg_path)],
            cwd=str(tmp_dir),
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
        )
        procs.append(gap_proc)

        base_url = f"http://127.0.0.1:{gap_port}"
        _wait_for_readyz(base_url)

        client = GapClient(base_url, token=DEV_API_KEY)
        assert client.healthz()["status"] == "ok"

        query_doc = (
            "{ customer(id: 1) { id name tier } "
            "orders(customer_id: 1) { id total_cents status } "
            "customer_notes(customer_id: 1) }"
        )
        res = client.execute(query_doc, include_tokens_estimate=True)
        assert not res.errors
        assert res.data is not None
        assert res.data["customer"]["name"] == "Ada Lovelace"
        assert res.data["customer"]["tier"] == "vip"
        assert len(res.data["orders"]) >= 1

        entry = client.register_persisted_operation(query_doc, name="CrossBranchCustomer")
        pq = client.execute_persisted("CrossBranchCustomer")
        assert pq.data is not None
        assert pq.data["customer"]["id"] == 1
        assert len(entry.sha256_hash) == 64

        frames = list(client.stream_operation("{ customer(id: 2) { id name } }"))
        assert any(
            f.event == "next" and ((f.data or {}).get("data") or {}).get("customer", {}).get("id") == 2
            for f in frames
        )
        assert any(f.event == "complete" for f in frames)

        tools = client.list_mcp_tools()
        assert [t["name"] for t in tools] == [
            "gap_search",
            "gap_describe",
            "gap_query",
            "gap_mutate",
            "gap_task",
        ]

        ov = client.overview()
        assert "shop" in json.dumps(ov) and "notes" in json.dumps(ov)

        search_out = client.search("customer", limit=3)
        assert "customer" in json.dumps(search_out).lower()

        described = client.describe_types(["Customer"], depth=1)
        assert "Customer" in json.dumps(described)

        resolved = client.resolve("customer", limit=3)
        assert "search" in resolved

        tasks_list = client.list_tasks()
        assert isinstance(tasks_list, dict)

        toolkit = GapToolkit(client, read_only_only=True)
        callable_tools = {t.name: t for t in toolkit.to_callable_tools()}
        adapter_out = callable_tools["gap_query"].invoke(
            {"query": "{ customer(id: 1) { id name } }"}
        )
        assert "Ada Lovelace" in json.dumps(adapter_out)
    finally:
        for p in reversed(procs):
            if p.poll() is None:
                p.terminate()
                try:
                    p.wait(timeout=3.0)
                except subprocess.TimeoutExpired:
                    p.kill()
                    p.wait(timeout=2.0)
        shutil.rmtree(tmp_dir, ignore_errors=True)
