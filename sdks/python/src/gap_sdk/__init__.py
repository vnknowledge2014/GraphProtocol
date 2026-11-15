"""Thin Python SDK for Graph Agent Protocol (GAP)."""

from .adapters import (
    GAP_TOOL_DEFINITIONS,
    GapToolkit,
    wrap_untrusted_envelope,
)
from .client import GapClient, compute_persisted_sha256, parse_sse_block
from .codegen import generate_types_from_sdl, parse_sdl_to_model
from .generated import (
    WALKING_SKELETON_SDL,
    Customer,
    CustomerNote,
    CustomerNotesPayload,
    CustomerTier,
    Order,
    OrderStatus,
    WalkingSkeletonQuery,
)
from .types import (
    GAP_MCP_PROTOCOL_VERSION,
    GapBudget,
    GapSdkError,
    GraphqlErrorEntry,
    GraphqlResponse,
    PersistedOperationEntry,
    SseEventFrame,
    TaskRecord,
)

__all__ = [
    "GAP_MCP_PROTOCOL_VERSION",
    "GAP_TOOL_DEFINITIONS",
    "WALKING_SKELETON_SDL",
    "Customer",
    "CustomerNote",
    "CustomerNotesPayload",
    "CustomerTier",
    "GapBudget",
    "GapClient",
    "GapSdkError",
    "GapToolkit",
    "GraphqlErrorEntry",
    "GraphqlResponse",
    "Order",
    "OrderStatus",
    "PersistedOperationEntry",
    "SseEventFrame",
    "TaskRecord",
    "WalkingSkeletonQuery",
    "compute_persisted_sha256",
    "generate_types_from_sdl",
    "parse_sdl_to_model",
    "parse_sse_block",
    "wrap_untrusted_envelope",
]
