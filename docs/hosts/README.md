# MCP Host Integration Guides (Skeleton)

Normative specification: `CMP-001`–`CMP-009` ([09-compat.md](../../spec/09-compat.md)), `ADR-017` ([01-DECISIONS.md](../01-DECISIONS.md)), `OPS-002` ([08-operations.md](../../spec/08-operations.md)).

> [!NOTE]
> **Status:** `WP-M0-10` documentation placeholders. Full copy-paste quickstarts, verified host screenshots/config snippets, and automated doc-tests are finalized in `WP-M7-03` once the inbound MCP facade (`WP-M1-10`) ships.

## Inbound MCP Facade Overview

Any MCP-speaking agent host connects to `gap-server` over stateless Streamable HTTP (`/mcp`, `CMP-001`, `OPS-002`) or `stdio` (`CMP-001`) and receives the five static GAP meta-tools (`CMP-002`, `CMP-003`, `CMP-008` Gate `G-F1` `≤ 1,200` tokens):

| Tool | Hint Posture | Recommended Host Permission (`CMP-009`) | Purpose |
|---|---|---|---|
| `gap_search` | `readOnlyHint: true` | **Auto-approve** | L1 hybrid discovery (`__search`) returning minimal SDL fragments |
| `gap_describe` | `readOnlyHint: true` | **Auto-approve** | L0 `__metaGraph` overview, L2 `__type` SDL expansion, and registered operations |
| `gap_query` | `readOnlyHint: true` | **Auto-approve** | Execute read-only GAP-QL documents (mutations rejected with `GAP_OPERATION_NOT_ALLOWED`) |
| `gap_mutate` | `destructiveHint: true`, `idempotentHint: false` | **Prompt / confirm** | Execute mutations through the side-effect & approval pipeline (`ADR-015`) |
| `gap_task` | `destructiveHint: false` | Policy-scoped (`decide` requires human-approval scope) | Inspect, list, cancel, submit, or decide durable tasks (`ADR-016`) |

Untrusted payloads from `T2` (and configured `T1`) branches are wrapped in `<gap:untrusted source="branch/op">…</gap:untrusted>` (`CMP-007`) and MUST be treated strictly as data (`CMP-003`, `ADR-014`).

## Supported Host Guides (`CMP-009`)

- [Claude Code](claude-code.md)
- [Antigravity IDE](antigravity.md)
- [Cursor](cursor.md)
