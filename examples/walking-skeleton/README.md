# GAP Walking Skeleton (`WP-M1-12`)

End-to-end local gateway walkthrough combining two branches behind one composed supergraph, one `POST /graphql` endpoint, and one `POST /mcp` (`2026-07-28`) / `--stdio` facade:

- **`shop` (`openapi`, `T1`)**: OpenAPI 3.0 specification (`shop.openapi.json`) over `fixtures/rest_fixture.py` (`http://127.0.0.1:9101`) with `customers`, `customer(id)` (`x-gap-in-param` native batching), `orders`, and `order(id)`.
- **`notes` (`mcp`, `T1`)**: Stateless MCP `2026-07-28` server (`fixtures/mcp_fixture.py`, `http://127.0.0.1:9102/mcp`) exposing one tool (`customer_notes`) and one resource (`note://handbook/onboarding`).
- **API key (`dev-agent`)**: Raw development bearer key `gap_dev_skeleton_key_01` (stored in `gap.toml` only as its `blake3-256` digest `8a7d1134387be2f4aeb799122161c9868d94bdbec857230c917947a746ac8ba3`).

---

## 1. Validate Configuration & Run Diagnostics

From the repository root:

```bash
cargo run -p gap-cli -- config check examples/walking-skeleton/gap.toml
```

Expected output (exit code `0`):

```text
ok: examples/walking-skeleton/gap.toml (profile=dev, branches=2, api_keys=1)
```

Run `gap doctor`:

```bash
cargo run -p gap-cli -- doctor --config examples/walking-skeleton/gap.toml
```

Expected output (exit code `0`, zero `fail` lines):

```text
[ok] config.parse: parsed examples/walking-skeleton/gap.toml (profile=dev)
[ok] config.files: all referenced branch files exist and are readable
[ok] server.listen: valid socket address 127.0.0.1:8080
[ok] auth.api_keys: 1 API key(s) configured
[ok] branches.trust: no T2 (untrusted) branches configured
[ok] branches.mcp_stdio: no stdio MCP branches configured
[skipped] runtime.toolchain: skipped (not available in M1)
[skipped] security.sandbox: skipped (not available in M1)
[skipped] models.s1: skipped (not available in M1)
```

---

## 2. Start Upstream Fixtures & `gap serve`

Open three terminals (or background the two Python 3 stdlib fixtures):

```bash
# Terminal 1 — OpenAPI REST fixture on 127.0.0.1:9101
python3 examples/walking-skeleton/fixtures/rest_fixture.py --port 9101
# Expected:
# rest_fixture listening on http://127.0.0.1:9101 (port=9101)

# Terminal 2 — Stateless MCP 2026-07-28 fixture on 127.0.0.1:9102
python3 examples/walking-skeleton/fixtures/mcp_fixture.py --port 9102
# Expected:
# mcp_fixture listening on http://127.0.0.1:9102/mcp (port=9102)

# Terminal 3 — GAP gateway on 127.0.0.1:8080
cargo run -p gap-cli -- serve --config examples/walking-skeleton/gap.toml
```

---

## 3. Probe Health & Readiness

```bash
curl -sS -i http://127.0.0.1:8080/healthz
curl -sS -i http://127.0.0.1:8080/readyz
```

Expected status on both endpoints: `HTTP/1.1 200 OK` (`{"status":"ok"}` / `{"status":"ready","generation":1}`).

---

## 4. Query Both Branches Together over `POST /graphql`

```bash
curl -sS http://127.0.0.1:8080/graphql \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/graphql-response+json' \
  -H 'Authorization: Bearer gap_dev_skeleton_key_01' \
  -d '{
    "query": "{ customer(id: 1) { id name tier } orders(customer_id: 1) { id total_cents status } customer_notes(customer_id: 1) }"
  }'
```

Expected JSON shape (`HTTP 200`):

```json
{
  "data": {
    "customer": {
      "id": 1,
      "name": "Ada Lovelace",
      "tier": "vip"
    },
    "orders": [
      { "id": 101, "total_cents": 4200, "status": "paid" },
      { "id": 103, "total_cents": 8500, "status": "pending" }
    ],
    "customer_notes": {
      "customer_id": 1,
      "notes": [
        {
          "note_id": "n-1",
          "customer_id": 1,
          "summary": "Prefers consolidated monthly invoice"
        }
      ]
    }
  },
  "extensions": {
    "gap": {
      "generation": 1,
      "partial": false
    }
  }
}
```

---

## 5. Drive the Inbound MCP `2026-07-28` Facade (`POST /mcp`)

### 5.1 `server/discover`

```bash
curl -sS http://127.0.0.1:8080/mcp \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -H 'MCP-Protocol-Version: 2026-07-28' \
  -H 'Mcp-Method: server/discover' \
  -H 'Authorization: Bearer gap_dev_skeleton_key_01' \
  -d '{
    "jsonrpc": "2.0",
    "id": 1,
    "method": "server/discover",
    "params": {
      "_meta": {
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientCapabilities": {}
      }
    }
  }'
```

Expected output (`HTTP 200`): JSON-RPC `result` with `"supportedVersions": ["2026-07-28", "2025-11-25"]` and `"resultType": "complete"`.

### 5.2 `gap_describe` (`overview: true`) and `gap_query`

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
    "id": 2,
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

Expected output (`HTTP 200`): `structuredContent.data.overview.branches` lists both `notes` and `shop`.

```bash
curl -sS http://127.0.0.1:8080/mcp \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -H 'MCP-Protocol-Version: 2026-07-28' \
  -H 'Mcp-Method: tools/call' \
  -H 'Mcp-Name: gap_query' \
  -H 'Authorization: Bearer gap_dev_skeleton_key_01' \
  -d '{
    "jsonrpc": "2.0",
    "id": 3,
    "method": "tools/call",
    "params": {
      "_meta": {
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientCapabilities": {}
      },
      "name": "gap_query",
      "arguments": {
        "query": "{ customer(id: 2) { id name } customer_notes(customer_id: 2) }"
      }
    }
  }'
```

Expected output (`HTTP 200`): `isError: false`, `structuredContent.data.customer.name == "Alan Turing"`, and `structuredContent.extensions.gap.generation == 1`.

---

## 6. Drive `gap serve --stdio`

With the two Python fixtures still running on `127.0.0.1:9101` and `127.0.0.1:9102`:

```bash
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}},"name":"gap_query","arguments":{"query":"{ customers(limit: 2) { id name } }"}}}' \
  | cargo run -q -p gap-cli -- serve --config examples/walking-skeleton/gap.toml --stdio
```

Expected output on `stdout`: exactly two newline-delimited JSON-RPC `2.0` response frames (structured logs go exclusively to `stderr`).

---

## 7. Graceful Shutdown (`RUN-090`)

- **First `SIGTERM` / `SIGINT` (`Ctrl-C`)**: flips `/readyz` immediately to `503 Service Unavailable`, stops accepting new connections, drains in-flight requests up to `server.shutdown_grace_ms` (`5000 ms`), and exits with code `0`.
- **Second `SIGTERM` / `SIGINT` during drain**: exits immediately with a non-zero exit code (`130`).
