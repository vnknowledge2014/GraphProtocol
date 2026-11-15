# Connector Authoring Guide (NGC)

Normative specification: [03 — Native Graph Connector (NGC)](../../spec/03-ngc.md) (`NGC-001`..`NGC-063`), `proto/gap/ngc/v1/connector.proto`, `ADR-013`, and `INV-18`.

## 1. Connector Contract (`gap.ngc.v1.NativeConnector`)

A Native Graph Connector (NGC) bridges an external data source, API, or internal service into GAP over gRPC (`tonic`) or in-process Rust trait wiring (`gap-ngc-sdk`).

Every connector implements four core RPCs (`spec/03-ngc.md`):

1. `Describe` — Returns branch SDL snippet, capability descriptor (supported batch keys, `max_batch_keys`, `supports_idempotency_key`), and declared `@sideEffect` / `@pii` metadata.
2. `BatchQuery` — Receives an array of `KeyedQueryRequest` items sharing one root field and returns key-indexed `KeyedQueryResult` frames (`INV-18`: one round-trip per join batch, zero N+1 fan-out).
3. `InvokeMutation` — Executes state-changing operations honoring `idempotency_key` and `dry_run` probes (`ADR-015`).
4. `Health` — Reports connector readiness and protocol compatibility.

## 2. Native vs Emulated Batching (`INV-18`)

- **OpenAPI / REST (`x-gap-in-param`):** Declare the upstream list/batch query parameter (for example `?ids=1,2,3` or `?id=1&id=2`) via `x-gap-in-param` in your OpenAPI operation so `gap-ngc-openapi` folds `K` join keys into `ceil(K / chunk_size)` HTTP requests.
- **SQL (`gap-ngc-sql`):** Translates key batches into bounded `ANY($1)` / `IN (...)` parameterized statements.
- **Custom Rust NGC SDK (`gap-ngc-sdk`):** Implement the async `ConnectorHandler` trait (illustrative signature, `ignore` doctest since workspace crates are not linked into `mdbook test`):

```rust,ignore
use gap_ngc_sdk::server::{BatchQueryInput, BatchQueryOutput, ConnectorHandler};
use gap_ngc_sdk::error::NgcError;

struct InventoryConnector;

impl ConnectorHandler for InventoryConnector {
    async fn batch_query(&self, req: BatchQueryInput) -> Result<BatchQueryOutput, NgcError> {
        let _keys = &req.keys;
        todo!("fetch all keys in one batched backend round-trip")
    }
}
```

## 3. Bounded Chunking Helper

Connectors MUST respect `max_batch_keys` (`INV-18`) when splitting large key sets into upstream batches:

```rust
fn count_batches(unique_keys: usize, max_batch_keys: usize) -> usize {
    assert!(max_batch_keys > 0);
    if unique_keys == 0 {
        0
    } else {
        unique_keys.div_ceil(max_batch_keys)
    }
}

fn main() {
    assert_eq!(count_batches(0, 64), 0);
    assert_eq!(count_batches(64, 64), 1);
    assert_eq!(count_batches(65, 64), 2);
}
```

## 4. Untrusted Error Mapping (`NGC-060`..`NGC-063`, `INV-15`)

Connectors MUST classify failures using `gap.ngc.v1.ErrorCode` (`PERMISSION_DENIED`, `RESOURCE_EXHAUSTED`, `DEADLINE_EXCEEDED`, `INVALID_ARGUMENT`, `UPSTREAM_ERROR`, etc.). At the gateway boundary (`gap-exec`), connector `details` maps are dropped, control characters and stack traces are stripped, and untrusted messages are clamped to `<= 300` Unicode scalar values (`GQL-072`, `NGC-061`). Never pass raw secrets, connection strings, or stack traces back across `BatchQuery` or `InvokeMutation`.

## 5. Verifying Your Connector

Validate any connector against the language-independent conformance kit (`VER-010`, `VER-011`):

```bash
cargo run -p gap-conformance -- --connector-kit --target http://127.0.0.1:50051
```
