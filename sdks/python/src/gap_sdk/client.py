"""Thin stdlib HTTP/SSE/MCP client for Graph Agent Protocol (GAP)."""

from __future__ import annotations

import hashlib
import json
import time
from typing import Any, Iterator
import urllib.error
import urllib.request

from .types import (
    GAP_MCP_PROTOCOL_VERSION,
    GapBudget,
    GapSdkError,
    GraphqlResponse,
    PersistedOperationEntry,
    SseEventFrame,
)

TERMINAL_TASK_STATES = frozenset({"completed", "failed", "canceled", "rejected"})


def compute_persisted_sha256(document: str) -> str:
    return hashlib.sha256(document.encode("utf-8")).hexdigest()


def parse_sse_block(raw_block: str) -> Iterator[SseEventFrame]:
    trimmed = raw_block.strip()
    if not trimmed:
        return
    event_name = "message"
    data_lines: list[str] = []
    for line in trimmed.splitlines():
        if line.startswith(":"):
            continue
        if ":" in line:
            field, value = line.split(":", 1)
            if value.startswith(" "):
                value = value[1:]
        else:
            field, value = line, ""
        if field == "event":
            event_name = value
        elif field == "data":
            data_lines.append(value)
    raw_data = "\n".join(data_lines)
    parsed: dict[str, Any] | None = None
    if raw_data:
        try:
            decoded = json.loads(raw_data)
            if isinstance(decoded, dict):
                parsed = decoded
        except json.JSONDecodeError:
            parsed = None
    yield SseEventFrame(event=event_name, data=parsed, raw_data=raw_data)


def _extract_task_state(record: dict[str, Any]) -> str | None:
    if isinstance(record.get("state"), str):
        return record["state"]
    data = record.get("data")
    if isinstance(data, dict):
        if isinstance(data.get("state"), str):
            return data["state"]
        task = data.get("task")
        if isinstance(task, dict) and isinstance(task.get("state"), str):
            return task["state"]
    return None


def _extract_candidate_type_names(search_payload: dict[str, Any]) -> list[str]:
    data = search_payload.get("data", search_payload)
    if not isinstance(data, dict):
        return []
    hits = data.get("hits", data.get("results", []))
    if not isinstance(hits, list):
        return []
    seen: list[str] = []
    for hit in hits:
        if not isinstance(hit, dict):
            continue
        for key in ("typeName", "type"):
            val = hit.get(key)
            if isinstance(val, str) and val not in ("Query", "Mutation") and val not in seen:
                seen.append(val)
    return seen


class GapClient:
    """Zero-dependency synchronous GAP client using stdlib urllib.request."""

    def __init__(
        self,
        base_url: str,
        *,
        token: str | None = None,
        protocol_version: str = GAP_MCP_PROTOCOL_VERSION,
        default_headers: dict[str, str] | None = None,
        timeout_seconds: float = 30.0,
    ) -> None:
        self.base_url = base_url.rstrip("/")
        self._token = token
        self._protocol_version = protocol_version
        self._default_headers = dict(default_headers or {})
        self._timeout = timeout_seconds
        self._persisted_by_hash: dict[str, PersistedOperationEntry] = {}
        self._persisted_by_name: dict[str, PersistedOperationEntry] = {}
        self._rpc_counter = 0

    def _headers(self, extra: dict[str, str] | None = None) -> dict[str, str]:
        headers = dict(self._default_headers)
        if extra:
            headers.update(extra)
        if self._token and "Authorization" not in headers and "authorization" not in headers:
            headers["Authorization"] = f"Bearer {self._token}"
        return headers

    def _request_json(
        self,
        path: str,
        *,
        method: str = "GET",
        body: dict[str, Any] | None = None,
        headers: dict[str, str] | None = None,
        allow_error_status: bool = False,
    ) -> tuple[int, dict[str, Any]]:
        data = json.dumps(body).encode("utf-8") if body is not None else None
        req = urllib.request.Request(
            f"{self.base_url}{path}",
            data=data,
            headers=self._headers(headers),
            method=method,
        )
        try:
            with urllib.request.urlopen(req, timeout=self._timeout) as resp:
                raw = resp.read().decode("utf-8")
                return resp.status, json.loads(raw)
        except urllib.error.HTTPError as err:
            raw = err.read().decode("utf-8", errors="replace")
            try:
                parsed = json.loads(raw)
            except json.JSONDecodeError as exc:
                raise GapSdkError(
                    f"HTTP {err.code} non-JSON response from {path}: {raw}",
                    status=err.code,
                ) from exc
            if allow_error_status and isinstance(parsed, dict):
                return err.code, parsed
            raise GapSdkError(
                f"HTTP {err.code} from {path}",
                status=err.code,
                details=parsed,
            ) from err

    def healthz(self) -> dict[str, Any]:
        _, payload = self._request_json("/healthz")
        return payload

    def readyz(self) -> dict[str, Any]:
        _, payload = self._request_json("/readyz")
        return payload

    def _build_gap_extensions(
        self,
        *,
        deadline_ms: int | None = None,
        budget: GapBudget | None = None,
        include_tokens_estimate: bool | None = None,
    ) -> dict[str, Any] | None:
        gap: dict[str, Any] = {}
        if deadline_ms is not None:
            gap["deadlineMs"] = deadline_ms
        if budget is not None:
            gap["budget"] = budget.to_dict()
        if include_tokens_estimate is not None:
            gap["includeTokensEstimate"] = include_tokens_estimate
        return {"gap": gap} if gap else None

    def execute(
        self,
        query: str,
        *,
        variables: dict[str, Any] | None = None,
        operation_name: str | None = None,
        deadline_ms: int | None = None,
        budget: GapBudget | None = None,
        include_tokens_estimate: bool | None = None,
    ) -> GraphqlResponse:
        payload: dict[str, Any] = {"query": query}
        if variables is not None:
            payload["variables"] = variables
        if operation_name is not None:
            payload["operationName"] = operation_name
        ext = self._build_gap_extensions(
            deadline_ms=deadline_ms,
            budget=budget,
            include_tokens_estimate=include_tokens_estimate,
        )
        if ext is not None:
            payload["extensions"] = ext
        status, parsed = self._request_json(
            "/graphql",
            method="POST",
            body=payload,
            headers={
                "Content-Type": "application/json",
                "Accept": "application/graphql-response+json, application/json",
            },
            allow_error_status=True,
        )
        if status >= 400 and not parsed.get("errors"):
            raise GapSdkError(f"GraphQL failed with HTTP {status}", status=status, details=parsed)
        return GraphqlResponse(
            data=parsed.get("data"),
            errors=list(parsed.get("errors") or []),
            extensions=dict(parsed.get("extensions") or {}),
        )

    def register_persisted_operation(
        self, document: str, name: str | None = None
    ) -> PersistedOperationEntry:
        digest = compute_persisted_sha256(document)
        entry = PersistedOperationEntry(sha256_hash=digest, document=document, name=name)
        self._persisted_by_hash[digest] = entry
        if name:
            self._persisted_by_name[name] = entry
        return entry

    def get_persisted_operation(self, name_or_hash: str) -> PersistedOperationEntry | None:
        return self._persisted_by_hash.get(name_or_hash) or self._persisted_by_name.get(
            name_or_hash
        )

    def execute_persisted(
        self,
        name_or_sha256: str,
        *,
        variables: dict[str, Any] | None = None,
        operation_name: str | None = None,
        include_query_on_miss: bool = True,
    ) -> GraphqlResponse:
        local = self.get_persisted_operation(name_or_sha256)
        sha256_hash = local.sha256_hash if local else name_or_sha256
        ext = {"persistedQuery": {"version": 1, "sha256Hash": sha256_hash}}
        payload: dict[str, Any] = {"extensions": ext}
        if variables is not None:
            payload["variables"] = variables
        if operation_name is not None:
            payload["operationName"] = operation_name

        _, parsed = self._request_json(
            "/graphql",
            method="POST",
            body=payload,
            headers={
                "Content-Type": "application/json",
                "Accept": "application/graphql-response+json, application/json",
            },
            allow_error_status=True,
        )
        if parsed.get("errors") and local and include_query_on_miss:
            payload["query"] = local.document
            _, parsed = self._request_json(
                "/graphql",
                method="POST",
                body=payload,
                headers={
                    "Content-Type": "application/json",
                    "Accept": "application/graphql-response+json, application/json",
                },
                allow_error_status=True,
            )
        return GraphqlResponse(
            data=parsed.get("data"),
            errors=list(parsed.get("errors") or []),
            extensions=dict(parsed.get("extensions") or {}),
        )

    def stream_operation(
        self,
        query: str,
        *,
        variables: dict[str, Any] | None = None,
        operation_name: str | None = None,
    ) -> Iterator[SseEventFrame]:
        payload: dict[str, Any] = {"query": query}
        if variables is not None:
            payload["variables"] = variables
        if operation_name is not None:
            payload["operationName"] = operation_name
        req = urllib.request.Request(
            f"{self.base_url}/graphql/stream",
            data=json.dumps(payload).encode("utf-8"),
            headers=self._headers(
                {
                    "Content-Type": "application/json",
                    "Accept": "text/event-stream",
                }
            ),
            method="POST",
        )
        with urllib.request.urlopen(req, timeout=self._timeout) as resp:
            if resp.status >= 400:
                raise GapSdkError(f"stream_operation HTTP {resp.status}", status=resp.status)
            buffer = ""
            while True:
                line_bytes = resp.readline()
                if not line_bytes:
                    break
                line = line_bytes.decode("utf-8").replace("\r\n", "\n")
                buffer += line
                if buffer.endswith("\n\n"):
                    yield from parse_sse_block(buffer)
                    buffer = ""
            if buffer.strip():
                yield from parse_sse_block(buffer)

    def _post_mcp(
        self,
        method: str,
        params: dict[str, Any],
        tool_name: str | None = None,
    ) -> dict[str, Any]:
        self._rpc_counter += 1
        headers = {
            "Content-Type": "application/json",
            "Accept": "application/json, text/event-stream",
            "MCP-Protocol-Version": self._protocol_version,
            "Mcp-Method": method,
        }
        if tool_name:
            headers["Mcp-Name"] = tool_name
        status, envelope = self._request_json(
            "/mcp",
            method="POST",
            body={
                "jsonrpc": "2.0",
                "id": self._rpc_counter,
                "method": method,
                "params": params,
            },
            headers=headers,
            allow_error_status=True,
        )
        if status >= 400 or "error" in envelope:
            err = envelope.get("error") or {}
            raise GapSdkError(
                str(err.get("message") or f"MCP {method} HTTP {status}"),
                status=status,
                code=str(err["code"]) if "code" in err else None,
                details=err or envelope,
            )
        result = envelope.get("result")
        if not isinstance(result, dict):
            raise GapSdkError(f"MCP response missing result for {method}", status=status)
        return result

    def list_mcp_tools(self) -> list[dict[str, Any]]:
        res = self._post_mcp(
            "tools/list",
            {
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": self._protocol_version,
                    "io.modelcontextprotocol/clientCapabilities": {},
                }
            },
        )
        return list(res.get("tools") or [])

    def call_mcp_tool(self, name: str, arguments: dict[str, Any]) -> dict[str, Any]:
        return self._post_mcp(
            "tools/call",
            {
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": self._protocol_version,
                    "io.modelcontextprotocol/clientCapabilities": {},
                },
                "name": name,
                "arguments": arguments,
            },
            tool_name=name,
        )

    def overview(self) -> dict[str, Any]:
        res = self.call_mcp_tool("gap_describe", {"overview": True})
        return dict(res.get("structuredContent") or res)

    def describe_types(self, types: list[str], *, depth: int = 1) -> dict[str, Any]:
        res = self.call_mcp_tool("gap_describe", {"types": types, "depth": depth})
        return dict(res.get("structuredContent") or res)

    def list_operations(self) -> dict[str, Any]:
        res = self.call_mcp_tool("gap_describe", {"operations": True})
        return dict(res.get("structuredContent") or res)

    def search(
        self,
        query: str,
        *,
        kinds: list[str] | None = None,
        limit: int | None = None,
        budget_tokens: int | None = None,
    ) -> dict[str, Any]:
        args: dict[str, Any] = {"query": query}
        if kinds is not None:
            args["kinds"] = kinds
        if limit is not None:
            args["limit"] = limit
        if budget_tokens is not None:
            args["budget_tokens"] = budget_tokens
        res = self.call_mcp_tool("gap_search", args)
        return dict(res.get("structuredContent") or res)

    def resolve(
        self, intent: str, *, limit: int = 5, depth: int = 1
    ) -> dict[str, Any]:
        search_res = self.search(intent, limit=limit)
        hits = _extract_candidate_type_names(search_res)
        if not hits:
            return {"search": search_res}
        described = self.describe_types(hits[:limit], depth=depth)
        return {"search": search_res, "described": described}

    def task(
        self,
        action: str,
        *,
        task_id: str | None = None,
        decision: str | None = None,
        note: str | None = None,
        input_data: dict[str, Any] | None = None,
    ) -> dict[str, Any]:
        args: dict[str, Any] = {"action": action}
        if task_id is not None:
            args["id"] = task_id
        if decision is not None:
            args["decision"] = decision
        if note is not None:
            args["note"] = note
        if input_data is not None:
            args["input"] = input_data
        res = self.call_mcp_tool("gap_task", args)
        return dict(res.get("structuredContent") or res)

    def list_tasks(self) -> dict[str, Any]:
        return self.task("list")

    def get_task(self, task_id: str) -> dict[str, Any]:
        return self.task("get", task_id=task_id)

    def cancel_task(self, task_id: str) -> dict[str, Any]:
        return self.task("cancel", task_id=task_id)

    def decide_task(
        self, task_id: str, decision: str, *, note: str | None = None
    ) -> dict[str, Any]:
        return self.task("decide", task_id=task_id, decision=decision, note=note)

    def poll_task(
        self,
        task_id: str,
        *,
        interval_seconds: float = 0.2,
        timeout_seconds: float = 30.0,
    ) -> dict[str, Any]:
        deadline = time.monotonic() + timeout_seconds
        while True:
            current = self.get_task(task_id)
            state = _extract_task_state(current)
            if state and state.lower() in TERMINAL_TASK_STATES:
                return current
            if time.monotonic() >= deadline:
                raise GapSdkError(
                    f"Timed out polling task {task_id} after {timeout_seconds}s",
                    code="GAP_TASK_POLL_TIMEOUT",
                    details=current,
                )
            time.sleep(interval_seconds)
