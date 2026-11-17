# 5-Minute Quickstart (Walking Skeleton)

Normative specification: `OPS-001`, `OPS-005`, `OPS-044`, `CMP-001`, `CMP-009`, `CMP-025` ([09-compat.md](../../spec/09-compat.md)).

Run a complete local GAP supergraph combining two downstream branches (`shop` over OpenAPI REST and `notes` over MCP HTTP) behind one GraphQL endpoint (`POST /graphql`), one static 5-tool MCP `2026-07-28` facade (`POST /mcp`), and one AG-UI `v1.0` SSE stream (`POST /ag-ui`).

## 1. Build and Preflight Check

From the repository root:

```bash
cargo build -p gap-cli --offline
./target/debug/gap config check examples/walking-skeleton/gap.toml
./target/debug/gap doctor --config examples/walking-skeleton/gap.toml
```

Expected exit code: `0` (dev fixture key for `dev-agent` in `examples/walking-skeleton/gap.toml` is `gap_dev_skeleton_key_01`, stored as its BLAKE3 digest).

## 2. Start Upstream Fixtures and `gap serve`

Open three terminals:

```bash
# Terminal 1: OpenAPI REST fixture (port 9101)
python3 examples/walking-skeleton/fixtures/rest_fixture.py --port 9101

# Terminal 2: MCP 2026-07-28 fixture (port 9102)
python3 examples/walking-skeleton/fixtures/mcp_fixture.py --port 9102

# Terminal 3: GAP gateway (port 8080)
./target/debug/gap serve --config examples/walking-skeleton/gap.toml
```

Verify liveness and readiness:

```bash
curl -sS -i http://127.0.0.1:8080/healthz
curl -sS -i http://127.0.0.1:8080/readyz
```

## 3. Execute a Cross-Branch GraphQL Query

Query both the REST `shop` branch (`customer`, `orders`) and the MCP `notes` branch (`customer_notes`) in one typed GraphQL operation:

```graphql
query QuickstartJoin {
  customer(id: 1) {
    id
    name
    tier
  }
  orders(customer_id: 1) {
    id
    total_cents
    status
  }
  customer_notes(customer_id: 1)
}
```

```bash
curl -sS http://127.0.0.1:8080/graphql \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/graphql-response+json' \
  -H 'Authorization: Bearer gap_dev_skeleton_key_01' \
  -d '{
    "query": "{ customer(id: 1) { id name tier } orders(customer_id: 1) { id total_cents status } customer_notes(customer_id: 1) }"
  }'
```

## 4. Connect an MCP Agent Host

Point Claude Code, Antigravity IDE, or Cursor at `http://127.0.0.1:8080/mcp` (see [Host Integration Guides](../hosts/README.md)), or inspect the `gap_describe` overview directly:

```bash
curl -sS http://127.0.0.1:8080/mcp \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -H 'MCP-Protocol-Version: 2026-07-28' \
  -H 'Mcp-Method: tools/call' \
  -H 'Mcp-Name: gap_describe' \
  -H 'Authorization: Bearer gap_dev_skeleton_key_01' \
  -d '{
    "jsonrpc": "2.0",
    "id": 1,
    "method": "tools/call",
    "params": {
      "_meta": {
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientCapabilities": {}
      },
      "name": "gap_describe",
      "arguments": { "overview": true }
    }
  }'
```

Or run `gap` over stdio MCP framing (`SEC-004`) when an agent host spawns the gateway directly:

```bash
./target/debug/gap serve --stdio --config examples/walking-skeleton/gap.toml
```

### 4.1 Stream an AG-UI `v1.0` Run (`/ag-ui`, `CMP-025`)

Frontend agent frameworks (such as CopilotKit / AG-UI `HttpAgent`) can negotiate capabilities and stream typed `RUN_STARTED` -> `STATE_SNAPSHOT` -> `RUN_FINISHED` events directly over SSE:

```bash
curl -sS http://127.0.0.1:8080/ag-ui/capabilities \
  -H 'Authorization: Bearer gap_dev_skeleton_key_01'

curl -sS -N http://127.0.0.1:8080/ag-ui \
  -H 'Content-Type: application/json' \
  -H 'Accept: text/event-stream' \
  -H 'Authorization: Bearer gap_dev_skeleton_key_01' \
  -d '{
    "threadId": "demo-thread",
    "runId": "demo-run-1",
    "messages": [],
    "forwardedProps": {
      "graphql": {
        "query": "{ customer(id: 1) { id name tier } customer_notes(customer_id: 1) }"
      }
    }
  }'
```

## 5. Inspect Telemetry Labels (`OPS-020`)

Every GAP metric uses fixed bounded cardinality labels (for example hashing any tenant slug deterministically into `tenant_bucket` in `0..15`):

```rust
fn tenant_bucket(tenant: &str) -> u8 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in tenant.as_bytes() {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    (hash % 16) as u8
}

fn main() {
    let b = tenant_bucket("default");
    assert!(b < 16);
    assert_eq!(b, tenant_bucket("default"));
}
```
