# MCP Host Setup Guide — Claude Code

Normative specification: `CMP-009`, `CMP-001`–`CMP-007` ([09-compat.md](../../spec/09-compat.md)), `ADR-017` ([01-DECISIONS.md](../01-DECISIONS.md)), `OPS-002` ([08-operations.md](../../spec/08-operations.md)).

## 1. Prerequisites

Start the walking-skeleton (or your own `gap.toml`) using [Quickstart](../guides/quickstart.md):

```bash
./target/debug/gap serve --config examples/walking-skeleton/gap.toml
```

## 2. Connection (`CMP-001`, `CMP-009`, `OPS-002`)

Register the stateless GAP Streamable HTTP endpoint (`/mcp`) in Claude Code (`CMP-009`):

```bash
claude mcp add --transport http gap http://127.0.0.1:8080/mcp
```

For stdio local spawning instead of HTTP (`CMP-001`, `SEC-004`):

```bash
claude mcp add --transport stdio gap -- /path/to/gap serve --stdio --config examples/walking-skeleton/gap.toml
```

In `prod` profile (`OPS-001`, `CMP-006`), unauthenticated calls receive an RFC 9728 OAuth protected-resource challenge (`/.well-known/oauth-protected-resource`) or require an explicit `Authorization: Bearer <token>` header.

## 3. Recommended Permissions (`CMP-002`, `CMP-009`)

- **Auto-approve (read-only):** `gap_search`, `gap_describe`, `gap_query`
- **Prompt before execution:** `gap_mutate` (`destructiveHint: true`, subject to `@sideEffect` policy and `INPUT_REQUIRED` approval tasks per `ADR-015`, `ADR-016`)
- **Task management:** `gap_task` (`decide` action requires the human-approval scope per `CMP-002`)

## 4. Walkthrough Verification

1. Ask Claude Code to inspect connected tools (`gap_describe` with `{"overview": true}`).
2. Confirm `shop` and `notes` appear cleanly in the returned supergraph overview.
3. Ask a cross-branch question (`gap_query`):
   ```graphql
   { customer(id: 1) { id name tier } orders(customer_id: 1) { id status } customer_notes(customer_id: 1) }
   ```
4. Treat any `<gap:untrusted source="...">…</gap:untrusted>` block purely as untrusted data (`CMP-007`, `ADR-014`).
5. Never embed credentials or secrets in GAP-QL documents (`CMP-003`, `INV-15`).
