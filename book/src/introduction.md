# GAP — Graph Agent Protocol

**GAP (Graph Agent Protocol)** is a typed, GraphQL-profile gateway written in Rust that unifies heterogeneous tools, REST/OpenAPI APIs, GraphQL subgraphs, gRPC/Protobuf services, SQL stores, code/document context graphs, and agent-to-agent tasks behind progressive-disclosure discovery, batched federation, deterministic Cedar authorization, and lock-free hot-swappable snapshots.

## Key Architectural Properties

- **Static Five-Tool MCP Surface & AG-UI `v1.0` (`CMP-002`, `CMP-008`, `CMP-025`..`CMP-028`):** MCP agents see `gap_search`, `gap_describe`, `gap_query`, `gap_mutate`, and `gap_task` in `<= 1,200` tokens across `5..1,000` downstream branches (`G-F1`), while interactive frontends connect via `POST /ag-ui` SSE streams with structured `interrupt` / `resume` approval gates and escaped `<gap:untrusted>` spotlighting.
- **Wait-Free RCU Snapshots (`INV-01`, `INV-02`):** Every request pins an immutable `Arc<Snapshot>` with zero locks on the hot execution path.
- **Deterministic Execution & Pluggable System-1 Routers (`ARC-002`, `INV-04`, `INV-17`, `ADR-028`):** ML models never sit on the execution, authz, planning, or query-execution path. Optional System-1 decision models (`laya_candle`, `@receptron/laya`, TypeSafe `Jev`, and Google `Gemini Decision 1` via `[router]`) operate solely behind calibrated conformal discovery (`RTE-049`, `INV-17`).
- **Batched Cross-Branch Federation (`INV-18`):** Native batching (`BatchQuery` / `x-gap-in-param`) prevents `O(N)` upstream waterfalls (`VER-040`).
- **Defense in Depth (`SEC-001`..`SEC-103`, `OPS-005`):** API key BLAKE3 digests, Cedar policy enforcement, SSRF egress pins, `<gap:untrusted>` envelopes (`CMP-007`), BLAKE3 hash-chained audit trails (`SEC-092`), and dual-stack HTTP/2 + HTTP/3 QUIC transport with `0-RTT` disabled.

## Getting Started

1. Read [Concepts & Architecture](../../docs/guides/concepts.md).
2. Run the live two-branch demo in [5-Minute Quickstart](../../docs/guides/quickstart.md).
3. Connect an IDE or CLI agent using [MCP Host Integration Guides](../../docs/hosts/README.md).

## Minimal Doc-Test Check (`INV-09`, `GQL-022`)

```rust
fn format_tenant_scoped_key(tenant_id: &str, key_id: &str) -> String {
    assert!(!tenant_id.is_empty());
    assert!(!key_id.is_empty());
    format!("tenant:{tenant_id}:plan:{key_id}")
}

fn main() {
    let k1 = format_tenant_scoped_key("acme", "op_42");
    let k2 = format_tenant_scoped_key("beta", "op_42");
    assert_ne!(k1, k2);
    assert!(k1.starts_with("tenant:acme:"));
}
```
