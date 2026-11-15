# A2A v1.0 Facade Conformance Suite (`conformance/a2a/`)

Contract and offline GAP-QL conformance fixtures for the GAP `Facade-A2A` profile (`CMP-020`, `CMP-021`, `CMP-022`, `CMP-023`, `INV-15`, `INV-20`, `VER-010`).

## Offline Execution

Run the GraphQL offline driver against the A2A conformance directory:

```bash
cargo run -p gap-conformance --offline -- --class Facade-A2A --offline conformance/a2a
```

Full wire-level HTTP+JSON / JSON-RPC 2.0 / SSE / JWS `AgentCard` / `INPUT_REQUIRED` continuation / two-GAP-node mutual delegation loop tests (`INV-20`, `CMP-020`..`CMP-023`) execute in:

- `adapters/gap-a2a/src/tests.rs`
- `adapters/gap-a2a/tests/loop_and_wire.rs`

```bash
cargo nextest run --no-pager -p gap-a2a --offline
```
