"""Core protocol dataclasses and error types for the GAP Python SDK."""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any

GAP_MCP_PROTOCOL_VERSION = "2026-07-28"


@dataclass(frozen=True)
class GapBudget:
    tokens: int | None = None
    ms: int | None = None

    def to_dict(self) -> dict[str, int]:
        out: dict[str, int] = {}
        if self.tokens is not None:
            out["tokens"] = self.tokens
        if self.ms is not None:
            out["ms"] = self.ms
        return out


@dataclass(frozen=True)
class GraphqlErrorEntry:
    message: str
    locations: list[dict[str, int]] = field(default_factory=list)
    path: list[str | int] = field(default_factory=list)
    extensions: dict[str, Any] = field(default_factory=dict)


@dataclass(frozen=True)
class GraphqlResponse:
    data: dict[str, Any] | None = None
    errors: list[dict[str, Any]] = field(default_factory=list)
    extensions: dict[str, Any] = field(default_factory=dict)


@dataclass(frozen=True)
class PersistedOperationEntry:
    sha256_hash: str
    document: str
    name: str | None = None


@dataclass(frozen=True)
class SseEventFrame:
    event: str
    data: dict[str, Any] | None = None
    raw_data: str = ""


@dataclass(frozen=True)
class TaskRecord:
    id: str
    state: str
    raw: dict[str, Any] = field(default_factory=dict)


class GapSdkError(RuntimeError):
    """Typed error raised by GapClient when transport or protocol calls fail."""

    def __init__(
        self,
        message: str,
        *,
        status: int | None = None,
        code: str | None = None,
        details: Any = None,
    ) -> None:
        super().__init__(message)
        self.status = status
        self.code = code
        self.details = details
