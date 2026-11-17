# Migration Guide

Normative specification: `ADR-017` ([01-DECISIONS.md](../01-DECISIONS.md)), `CMP-008`, `ING-010`, `OPS-044`.

## 1. When to Adopt GAP (`ADR-017` Criterion)

GAP is not meant to replace a single small MCP server inside a simple local script. Our published `GAP-Bench` measurements (`docs/benchmarks/REPORT.md`) show when Direct-MCP is still cheaper versus when GAP yields an order-of-magnitude reduction:

- **Keep Direct-MCP** when an agent connects to a small static catalog (`<= 10` tools) performing single-hop lookups, or `<= 50` single-hop tools with warm provider prompt-caching and no security/policy separation requirements.
- **Migrate to GAP** when any of the following apply:
  1. Total exposed operations grow (`N >= 50` uncached or `N >= 200` cached), where progressive disclosure keeps `tools/list <= 1,200` tokens (`G-F1`).
  2. Queries combine multiple backends (`cross_branch_join` / batched REST + MCP + SQL), eliminating multi-turn LLM orchestration loops (`INV-18`).
  3. Agents inspect codebases or architecture context graphs (`code_graph`, Gate `G-C1` `< 15%` tokens vs file-scan loops).
  4. Governance requires Cedar authz, dry-run/human approvals (`ADR-015`), hash-chained audit logs (`SEC-092`), or `<gap:untrusted>` `T2` isolation (`CMP-007`).

## 2. Migrating by Backend Type

### OpenAPI / REST Services

1. Point `[[branches]]` (`protocol = "openapi"`) at your `openapi.json` / `openapi.yaml` and `base_url`.
2. Annotate batchable lookup endpoints with `x-gap-in-param` to enable native batched joins (`INV-18`).
3. Validate with `gap serve --check-only --config gap.toml`.

### Existing GraphQL Subgraphs

1. Configure `protocol = "graphql"` with endpoint URL and pinned SDL file or introspection policy (`T1`).
2. Add `@requiresScope` / `@sideEffect` / `@pii` directives cleanly in overlay policies without breaking downstream resolvers.

### gRPC / Protobuf & SQL Services

1. Add `protocol = "grpc"` referencing your compiled protobuf descriptor set (`ING-012`), or `protocol = "sql"` referencing read-only/audited SQL queries.
2. Wrap custom internal services with `gap-ngc-sdk` when bespoke `BatchQuery` batching is needed.

### Existing Multi-Server MCP Setups

Instead of registering 10–100 separate MCP servers in Claude Code, Antigravity, or Cursor:

1. Move upstream MCP HTTP/stdio servers into `[[branches]]` (`protocol = "mcp"`) inside `gap.toml`.
2. Explicitly allow-list any `--stdio` binaries (`SEC-072`).
3. Point your IDE/agent host at the single GAP endpoint `http://127.0.0.1:8080/mcp` (see [Host Integration Guides](../hosts/README.md)).

### Interactive Agent Frontends (`AG-UI v1.0`, `CMP-025`..`CMP-028`)

Web and UI agent frameworks (such as CopilotKit or any AG-UI `HttpAgent`) can point directly at `http://127.0.0.1:8080/ag-ui` (over HTTP/2 or HTTP/3 `Alt-Svc`):

1. Discover runtime features via `GET /ag-ui/capabilities` (`protocol = "ag-ui"`, `version = "1.0"`, `interrupts = true`).
2. Stream typed runs over `POST /ag-ui`; `@sideEffect(level: DESTRUCTIVE)` or Cedar-gated `GAP_APPROVAL_REQUIRED` mutations pause cleanly via `RUN_FINISHED(outcome = "interrupt")` and resume with `resume: [{ interruptId, status: "approve" | "deny" }]` (`CMP-027`).

## 3. Gradual Cutover Checklist

```bash
gap init ./my-gateway
gap config check ./my-gateway/gap.toml
gap doctor --config ./my-gateway/gap.toml
gap ingest once --config ./my-gateway/gap.toml
gap proposals list
```

Verify GraphQL queries return identical fields to legacy REST/MCP calls:

```graphql
query VerifyCutover {
  __metaGraph {
    branches {
      id
      operationCount
    }
  }
}
```

Ensure shadow/rollback safety check using snapshot generations:

```rust
fn can_rollback(active_gen: u64, target_gen: u64) -> bool {
    target_gen > 0 && target_gen < active_gen
}

fn main() {
    assert!(can_rollback(4, 3));
    assert!(!can_rollback(1, 0));
    assert!(!can_rollback(3, 3));
}
```
